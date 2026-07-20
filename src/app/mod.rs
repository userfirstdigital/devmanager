mod chrome;
mod process_monitor;

use crate::assets::AppAssets;
use crate::browser::{
    browser_action_plan, browser_annotation_preview_plan, browser_command_channel,
    browser_event_plan, browser_host_reconcile_plan, browser_pane_open_fallback,
    browser_replay_repair_candidate_from_annotation, browser_response_sync, browser_settings_plan,
    browser_workflow_review_editor_for_field, browser_workflow_review_editor_mutation,
    calculate_browser_split, render_browser_pane, route_browser_request, BrowserActionPlan,
    BrowserAnnotation, BrowserAppExitDisposition, BrowserAttachmentBroker,
    BrowserAttachmentProjection, BrowserBounds, BrowserCommand, BrowserCommandBridge,
    BrowserCommandInbox, BrowserCommandRequest, BrowserError, BrowserGatewayHandle,
    BrowserHostVisibility, BrowserInvocationActor, BrowserInvocationContext, BrowserPaneAction,
    BrowserPaneActions, BrowserPaneContext, BrowserPaneEventPlan, BrowserPaneModel,
    BrowserPaneSurface, BrowserPaneTransient, BrowserReplayInstance, BrowserReplayPaneProjection,
    BrowserReplaySecretError, BrowserReplaySecretPromptEvent, BrowserReplaySecretPromptVault,
    BrowserReplaySecretSubmission, BrowserReplayStatus, BrowserResponse, BrowserRevision,
    BrowserRisk, BrowserSettingsAction, BrowserWebViewHost, BrowserWorkflowReviewEditor,
    BrowserWorkspaceKey, BrowserWorkspaceSnapshot,
};
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
    ai_session_needs_restore, env_service, pid_file, platform_service, ports_service,
    scanner_service, ConfigImportMode, ProcessManager, ProcessOpKind, RemoteSessionEvent,
    SessionManager,
};
use crate::sidebar;
use crate::state::{
    AppState, RuntimeState, SessionDimensions, SessionKind, SessionRuntimeState, SessionStatus,
};
use crate::terminal::{self, view};
use crate::updater::UpdaterService;
use crate::workspace::{
    self, apply_browser_enabled_preference, CommandDraft, EditorAction, EditorField,
    EditorPaneModel, EditorPanel, FolderDraft, FolderField, ProjectDraft, RemotePortForwardDraft,
    RemoteTopTab, SettingsDraft, SshDraft, UiPreviewDraft,
};
use crate::{icons, theme};
use gpui::{
    canvas, div, prelude::*, px, rgb, size, App, AppContext, Application, Bounds, ClipboardEntry,
    ClipboardItem, Context, FocusHandle, IntoElement, KeyDownEvent, Keystroke, Modifiers,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement, Pixels, Point,
    Render, RenderImage, ScrollWheelEvent, Styled, Subscription, TouchPhase, Window, WindowBounds,
    WindowOptions,
};
use rfd::{FileDialog, MessageButtons, MessageDialog, MessageDialogResult, MessageLevel};
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const TERMINAL_TOPBAR_HEIGHT_PX: f32 = 22.0;
const STACK_GAP_PX: f32 = 4.0;
const META_TEXT_HEIGHT_PX: f32 = 0.0;
const NOTICE_HEIGHT_PX: f32 = 26.0;
const PENDING_ANNOTATION_STRIP_HEIGHT_PX: f32 = 28.0;
const PENDING_ANNOTATION_ACTION_NOTICE_DURATION: Duration = Duration::from_secs(8);
const SEARCH_BAR_HEIGHT_PX: f32 = 34.0;
const FOOTER_HEIGHT_PX: f32 = 0.0;
const APP_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const REMOTE_CLIENT_REFRESH_INTERVAL: Duration = Duration::from_millis(16);
const REMOTE_HOST_REQUEST_POLL_INTERVAL: Duration = Duration::from_millis(50);
const REMOTE_HOST_IDLE_POLL_INTERVAL: Duration = Duration::from_millis(250);
const REMOTE_HOST_HOUSEKEEPING_INTERVAL: Duration = Duration::from_millis(100);
const REMOTE_HOST_SNAPSHOT_ACTIVE_INTERVAL: Duration = Duration::from_millis(33);
const REMOTE_HOST_SNAPSHOT_IDLE_INTERVAL: Duration = Duration::from_millis(250);
const REMOTE_RECONNECT_BASE_INTERVAL: Duration = Duration::from_millis(350);
const REMOTE_RECONNECT_MAX_INTERVAL: Duration = Duration::from_secs(5);
const AI_LOCAL_RENDER_GUARD_WINDOW: Duration = Duration::from_secs(30);
const APP_WINDOW_TITLE: &str = "DevManager";
const WINDOW_TITLE_SEPARATOR: &str = " • ";

static EDITOR_ID_COUNTER: AtomicU64 = AtomicU64::new(1);

// Remote-specific background hosting was a legacy Windows taskbar workaround.
// Until we add real tray support, only the global minimize-to-tray setting may
// intercept the window close button.
fn should_minimize_window_on_close(
    minimize_to_tray: bool,
    _legacy_remote_keep_hosting_in_background: bool,
) -> bool {
    minimize_to_tray
}

fn browser_bounds_from_gpui(bounds: Bounds<Pixels>) -> Option<BrowserBounds> {
    let x = f32::from(bounds.origin.x);
    let y = f32::from(bounds.origin.y);
    let width = f32::from(bounds.size.width);
    let height = f32::from(bounds.size.height);
    if !x.is_finite()
        || !y.is_finite()
        || !width.is_finite()
        || !height.is_finite()
        || width <= 0.0
        || height <= 0.0
    {
        return None;
    }
    Some(BrowserBounds {
        x: x.round() as i32,
        y: y.round() as i32,
        width: width.round() as i32,
        height: height.round() as i32,
    })
}

pub fn run() {
    Application::new()
        .with_assets(AppAssets::new())
        .run(|cx: &mut App| {
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
                    let window_lifetime = shell.read(cx).browser_host.window_lifetime_fence();
                    let closed_window_lifetime = window_lifetime.clone();
                    cx.on_window_closed(move |cx| {
                        let centralized_exit = closed_window_lifetime.teardown_ready();
                        closed_window_lifetime.assert_drained_after_window_close();
                        if !centralized_exit && cx.windows().is_empty() {
                            execute_app_termination(PendingAppTermination::Quit, cx);
                        }
                    })
                    .detach();
                    let close_handler = shell.clone();
                    let closing_window_lifetime = window_lifetime;
                    window.on_window_should_close(cx, move |window, cx| {
                        closing_window_lifetime.guard_window_close(|| {
                            close_handler.update(cx, |shell, cx| {
                                shell.handle_window_should_close(window, cx)
                            })
                        })
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

#[derive(Debug, Clone)]
enum ActionableNotice {
    PortInUse { command_id: String, message: String },
    ForceQuit { message: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingAppTermination {
    Quit,
    ExitAfterUpdate,
}

impl PendingAppTermination {
    fn coalesce(self, requested: Self) -> Self {
        if matches!(self, Self::ExitAfterUpdate) || matches!(requested, Self::ExitAfterUpdate) {
            Self::ExitAfterUpdate
        } else {
            Self::Quit
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShutdownFailureDisposition {
    IgnoreStale,
    PreservePendingTermination,
    ResumeInteractiveShutdown,
}

fn shutdown_completion_is_current(pending_op_id: Option<u64>, completion_op_id: u64) -> bool {
    pending_op_id == Some(completion_op_id)
}

fn shutdown_failure_disposition(
    pending_op_id: Option<u64>,
    completion_op_id: u64,
    pending_termination: Option<PendingAppTermination>,
) -> ShutdownFailureDisposition {
    if !shutdown_completion_is_current(pending_op_id, completion_op_id) {
        ShutdownFailureDisposition::IgnoreStale
    } else if pending_termination.is_some() {
        ShutdownFailureDisposition::PreservePendingTermination
    } else {
        ShutdownFailureDisposition::ResumeInteractiveShutdown
    }
}

fn retire_pending_shutdown_for_forced_termination(
    pending_op_id: &mut Option<u64>,
    pending_window_close: &mut bool,
) {
    *pending_op_id = None;
    *pending_window_close = false;
}

fn promote_pending_app_termination_for_update(pending: &mut Option<PendingAppTermination>) {
    if let Some(current) = *pending {
        *pending = Some(current.coalesce(PendingAppTermination::ExitAfterUpdate));
    }
}

fn take_ready_app_termination(
    pending: &mut Option<PendingAppTermination>,
    native_window_teardown_ready: bool,
) -> Option<PendingAppTermination> {
    native_window_teardown_ready
        .then(|| pending.take())
        .flatten()
}

fn execute_app_termination(termination: PendingAppTermination, cx: &App) {
    match termination {
        PendingAppTermination::Quit => cx.quit(),
        PendingAppTermination::ExitAfterUpdate => std::process::exit(0),
    }
}

struct NativeShell {
    state: AppState,
    session_manager: SessionManager,
    process_manager: ProcessManager,
    browser_gateway: Option<BrowserGatewayHandle>,
    browser_host: BrowserWebViewHost,
    browser_app_config_dir: Option<std::path::PathBuf>,
    browser_bridge: BrowserCommandBridge,
    browser_inbox: Option<BrowserCommandInbox>,
    browser_tasks_started: bool,
    browser_ui: HashMap<BrowserWorkspaceKey, BrowserWorkspaceUiState>,
    browser_workflow_route: Option<BrowserWorkspaceKey>,
    browser_replay_secret_prompt: Option<BrowserReplaySecretPromptVault>,
    browser_replay_secret_submitter: Option<BrowserReplaySecretSubmitter>,
    browser_replay_repair_selection: Option<BrowserReplayRepairSelection>,
    browser_address_focus: FocusHandle,
    browser_replay_secret_focus: FocusHandle,
    browser_annotation_focus: FocusHandle,
    browser_workflow_focus: FocusHandle,
    browser_split_bounds: Option<BrowserBounds>,
    browser_page_bounds: Option<BrowserBounds>,
    browser_divider_drag: Option<BrowserDividerDrag>,
    updater: UpdaterService,
    startup_notice: Option<String>,
    terminal_notice: Option<String>,
    pending_annotation_action_notice: Option<PendingAnnotationActionNotice>,
    terminal_actionable_notice: Option<ActionableNotice>,
    editor_notice: Option<String>,
    remote_machine_state: RemoteMachineState,
    remote_host_service: RemoteHostService,
    remote_client_pool: RemoteClientPool,
    remote_mode: Option<RemoteModeState>,
    local_state_backup: Option<AppState>,
    last_remote_host_config_revision: u64,
    last_remote_snapshot_sync_at: Option<Instant>,
    last_remote_app_revision: u64,
    last_remote_runtime_revision: u64,
    last_remote_port_hash: u64,
    remote_live_session_generations: HashMap<String, u64>,
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
    process_monitor: Option<process_monitor::ProcessMonitorState>,
    process_monitor_revision: u64,
    last_window_title: Option<String>,
    splash_image: Option<Arc<RenderImage>>,
    splash_fetch_in_flight: bool,
    native_dialog_blockers: Arc<AtomicUsize>,
    remote_connect_request_id: u64,
    remote_status_notice: Option<RemoteStatusNotice>,
    pending_shutdown_op_id: Option<u64>,
    pending_window_close: bool,
    pending_install_update: Option<String>,
    pending_app_termination: Option<PendingAppTermination>,
    window_subscriptions: Vec<Subscription>,
}

type BrowserReplaySecretSubmitter =
    Box<dyn FnOnce(BrowserReplaySecretSubmission) -> Result<(), BrowserReplaySecretError> + Send>;

#[derive(Clone, Debug, PartialEq, Eq)]
struct BrowserReplayRepairSelection {
    workspace_key: BrowserWorkspaceKey,
    instance_id: u64,
    repair_id: u64,
    tab_id: String,
    revision: BrowserRevision,
}

struct NativeDialogPauseGuard {
    blockers: Arc<AtomicUsize>,
}

#[derive(Clone, Default)]
struct BrowserWorkspaceUiState {
    address_draft: Option<String>,
    address_cursor: usize,
    address_focused: bool,
    loading: bool,
    diagnostic: Option<String>,
    action_status: Option<String>,
    annotation_mode: bool,
    annotation_draft: Option<crate::browser::BrowserAnnotationDraft>,
    annotation_comment: String,
    annotation_cursor: usize,
    annotation_focused: bool,
    workflow_preview: Option<String>,
    workflow_editor: Option<BrowserWorkflowReviewEditor>,
}

#[derive(Debug, Clone)]
struct BrowserDividerDrag {
    workspace_key: BrowserWorkspaceKey,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemoteStatusBarAction {
    ConnectPreferred,
    OpenRemoteConnectTab,
    OpenRemoteHostTab,
    RetryReconnect,
    DisconnectRemote,
    TakeRemoteControl,
    TakeHostControl,
}

struct RemoteStatusBarState {
    model: chrome::RemoteStatusBarModel,
    primary_action: Option<RemoteStatusBarAction>,
    secondary_action: Option<RemoteStatusBarAction>,
    tertiary_action: Option<RemoteStatusBarAction>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RemoteStatusBarConnectionSnapshot {
    connected_label: String,
    has_control: bool,
    reconnecting: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemoteStatusBarClientKind {
    Desktop,
    Browser,
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

fn remote_status_bar_client_kind(client_id: &str) -> RemoteStatusBarClientKind {
    if client_id.starts_with("web-") {
        RemoteStatusBarClientKind::Browser
    } else {
        RemoteStatusBarClientKind::Desktop
    }
}

fn remote_host_ready_transports(host_status: &remote::RemoteHostStatus) -> (bool, bool) {
    (
        host_status.enabled && host_status.listening,
        host_status.web_enabled && host_status.web_listener_error.is_none(),
    )
}

fn remote_status_bar_action_button(action: RemoteStatusBarAction) -> chrome::StatusBarQuickAction {
    let (label, tone) = match action {
        RemoteStatusBarAction::ConnectPreferred => {
            ("Quick connect".to_string(), chrome::StatusBarTone::Accent)
        }
        RemoteStatusBarAction::OpenRemoteConnectTab => {
            ("Connect...".to_string(), chrome::StatusBarTone::Accent)
        }
        RemoteStatusBarAction::OpenRemoteHostTab => {
            ("Host".to_string(), chrome::StatusBarTone::Accent)
        }
        RemoteStatusBarAction::RetryReconnect => {
            ("Retry".to_string(), chrome::StatusBarTone::Accent)
        }
        RemoteStatusBarAction::DisconnectRemote => {
            ("Disconnect".to_string(), chrome::StatusBarTone::Danger)
        }
        RemoteStatusBarAction::TakeRemoteControl | RemoteStatusBarAction::TakeHostControl => {
            ("Take control".to_string(), chrome::StatusBarTone::Accent)
        }
    };
    chrome::StatusBarQuickAction { label, tone }
}

fn build_remote_status_bar_transport_toggle(
    icon_path: &'static str,
    enabled: bool,
    ready: bool,
    has_error: bool,
    count: usize,
    active_tone: chrome::StatusBarTone,
) -> chrome::StatusBarTransportToggle {
    chrome::StatusBarTransportToggle {
        icon_path,
        enabled,
        tone: if !enabled {
            chrome::StatusBarTone::Muted
        } else if has_error {
            chrome::StatusBarTone::Danger
        } else if ready {
            active_tone
        } else {
            chrome::StatusBarTone::Warning
        },
        count: (count > 0).then_some(count),
    }
}

fn remote_status_bar_local_control_label(
    host_status: &remote::RemoteHostStatus,
    local_has_control: bool,
) -> Option<&'static str> {
    if local_has_control {
        None
    } else {
        Some(
            match host_status
                .controller_client_id
                .as_deref()
                .map(remote_status_bar_client_kind)
            {
                Some(RemoteStatusBarClientKind::Desktop) => "Desktop controls",
                Some(RemoteStatusBarClientKind::Browser) => "Browser controls",
                None => "Remote controls",
            },
        )
    }
}

fn build_remote_status_bar_state(
    remote_connection: Option<&RemoteStatusBarConnectionSnapshot>,
    host_status: &remote::RemoteHostStatus,
    preferred_host: Option<&remote::KnownRemoteHost>,
    local_has_control: bool,
    remote_notice_is_error: bool,
) -> RemoteStatusBarState {
    let (desktop_ready, browser_ready) = remote_host_ready_transports(host_status);
    let host_issue =
        (host_status.enabled && !desktop_ready) || (host_status.web_enabled && !browser_ready);
    let native_host = build_remote_status_bar_transport_toggle(
        icons::SERVER,
        host_status.enabled,
        desktop_ready,
        host_status.listener_error.is_some(),
        host_status.connected_native_clients,
        chrome::StatusBarTone::Success,
    );
    let web_host = build_remote_status_bar_transport_toggle(
        icons::GLOBE,
        host_status.web_enabled,
        browser_ready,
        host_status.web_listener_error.is_some(),
        host_status.connected_web_clients,
        chrome::StatusBarTone::Accent,
    );

    if let Some(remote_connection) = remote_connection {
        let primary_action = Some(if remote_connection.reconnecting {
            RemoteStatusBarAction::RetryReconnect
        } else {
            if remote_connection.has_control {
                RemoteStatusBarAction::DisconnectRemote
            } else {
                RemoteStatusBarAction::TakeRemoteControl
            }
        });
        return RemoteStatusBarState {
            model: chrome::RemoteStatusBarModel {
                label: format!("Remote • {}", remote_connection.connected_label),
                tone: if remote_connection.reconnecting {
                    chrome::StatusBarTone::Warning
                } else if remote_connection.has_control {
                    chrome::StatusBarTone::Accent
                } else {
                    chrome::StatusBarTone::Warning
                },
                native_host,
                web_host,
                primary_action: primary_action.map(remote_status_bar_action_button),
                secondary_action: None,
                tertiary_action: None,
            },
            primary_action,
            secondary_action: None,
            tertiary_action: None,
        };
    }

    let label = remote_status_bar_local_control_label(host_status, local_has_control)
        .map(|detail| format!("Local • {detail}"))
        .unwrap_or_else(|| "Local".to_string());
    let primary_action = Some(if !local_has_control {
        RemoteStatusBarAction::TakeHostControl
    } else if host_issue {
        RemoteStatusBarAction::OpenRemoteHostTab
    } else if preferred_host.is_some() {
        RemoteStatusBarAction::ConnectPreferred
    } else {
        RemoteStatusBarAction::OpenRemoteConnectTab
    });
    RemoteStatusBarState {
        model: chrome::RemoteStatusBarModel {
            label,
            tone: if !local_has_control {
                chrome::StatusBarTone::Warning
            } else if host_issue || remote_notice_is_error {
                chrome::StatusBarTone::Danger
            } else {
                chrome::StatusBarTone::Muted
            },
            native_host,
            web_host,
            primary_action: primary_action.map(remote_status_bar_action_button),
            secondary_action: None,
            tertiary_action: None,
        },
        primary_action,
        secondary_action: None,
        tertiary_action: None,
    }
}

fn reconcile_restored_browser_attachment_state(
    state: &mut AppState,
    broker: &BrowserAttachmentBroker,
) -> bool {
    let mut changed = false;
    for tab in &mut state.open_tabs {
        if !matches!(tab.tab_type, TabType::Claude | TabType::Codex) {
            continue;
        }
        let Ok(workspace_key) = BrowserWorkspaceKey::new(tab.project_id.clone(), tab.id.clone())
        else {
            continue;
        };
        let Some(snapshot) = tab.browser_workspace.as_mut() else {
            continue;
        };
        broker.observe_workspace(workspace_key.clone(), snapshot);
        changed = broker.overlay_snapshot(&workspace_key, snapshot) || changed;
    }
    if changed {
        state.mark_dirty();
    }
    changed
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BrowserAttachmentProjectionTransaction {
    Applied,
    PersistFailed,
    NewerProjectionRemainsDirty,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingAnnotationActionFailure {
    RemovePersistence,
    MissingAnnotation,
    RemoveFailed,
    InvalidSavedUrl,
    PreviewFailed,
    RemoteRemove,
}

impl PendingAnnotationActionFailure {
    fn notice(self) -> &'static str {
        match self {
            Self::RemovePersistence => "Annotation removal will retry when browser state saves.",
            Self::MissingAnnotation => "Annotation is no longer pending in this conversation.",
            Self::RemoveFailed => "Could not remove the pending annotation.",
            Self::InvalidSavedUrl => "Saved annotation URL cannot be previewed.",
            Self::PreviewFailed => "Could not open the annotation preview.",
            Self::RemoteRemove => "Remove pending browser annotations from the connected host.",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingAnnotationActionNotice {
    workspace_key: BrowserWorkspaceKey,
    failure: PendingAnnotationActionFailure,
    remote_mode: bool,
    expires_at: Instant,
}

impl PendingAnnotationActionNotice {
    fn new(
        workspace_key: BrowserWorkspaceKey,
        failure: PendingAnnotationActionFailure,
        remote_mode: bool,
        now: Instant,
    ) -> Self {
        Self {
            workspace_key,
            failure,
            remote_mode,
            expires_at: now + PENDING_ANNOTATION_ACTION_NOTICE_DURATION,
        }
    }
}

fn clear_pending_annotation_action_notice(
    notice: &mut Option<PendingAnnotationActionNotice>,
    workspace_key: &BrowserWorkspaceKey,
) {
    if notice
        .as_ref()
        .is_some_and(|notice| &notice.workspace_key == workspace_key)
    {
        *notice = None;
    }
}

fn pending_annotation_action_notice_message(
    notice: Option<&PendingAnnotationActionNotice>,
    active_workspace: Option<&BrowserWorkspaceKey>,
    remote_mode: bool,
    now: Instant,
) -> Option<&'static str> {
    notice
        .filter(|notice| {
            notice.remote_mode == remote_mode
                && now < notice.expires_at
                && Some(&notice.workspace_key) == active_workspace
        })
        .map(|notice| notice.failure.notice())
}

fn browser_workspace_key_for_ai_tab(tab: Option<&SessionTab>) -> Option<BrowserWorkspaceKey> {
    let tab = tab.filter(|tab| matches!(tab.tab_type, TabType::Claude | TabType::Codex))?;
    BrowserWorkspaceKey::new(tab.project_id.clone(), tab.id.clone()).ok()
}

fn refresh_terminal_pane_model_notice(
    model: &mut view::TerminalPaneModel,
    startup_notice: Option<&str>,
    transient_terminal_notice: Option<&str>,
    action_notice: &mut Option<PendingAnnotationActionNotice>,
    active_workspace: Option<&BrowserWorkspaceKey>,
    remote_mode: bool,
    now: Instant,
) {
    if action_notice
        .as_ref()
        .is_some_and(|notice| notice.remote_mode != remote_mode || now >= notice.expires_at)
    {
        *action_notice = None;
    }
    model.startup_notice = pending_annotation_action_notice_message(
        action_notice.as_ref(),
        active_workspace,
        remote_mode,
        now,
    )
    .map(str::to_string)
    .or_else(|| startup_notice.map(str::to_string))
    .or_else(|| transient_terminal_notice.map(str::to_string));
}

trait BrowserAttachmentProjectionSink {
    fn acknowledge_host(
        &mut self,
        projection: &BrowserAttachmentProjection,
    ) -> Result<BrowserWorkspaceSnapshot, BrowserError>;

    fn persist_snapshot(
        &mut self,
        workspace_key: &BrowserWorkspaceKey,
        snapshot: BrowserWorkspaceSnapshot,
    ) -> bool;
}

fn reconcile_browser_attachment_projection_transaction(
    broker: &BrowserAttachmentBroker,
    projection: &BrowserAttachmentProjection,
    sink: &mut impl BrowserAttachmentProjectionSink,
) -> Result<BrowserAttachmentProjectionTransaction, BrowserError> {
    let mut snapshot = sink.acknowledge_host(projection)?;
    broker.observe_workspace(projection.workspace_key.clone(), &snapshot);
    broker.overlay_snapshot(&projection.workspace_key, &mut snapshot);
    if !sink.persist_snapshot(&projection.workspace_key, snapshot) {
        return Ok(BrowserAttachmentProjectionTransaction::PersistFailed);
    }
    if broker.acknowledge_dirty_projection(projection) {
        Ok(BrowserAttachmentProjectionTransaction::Applied)
    } else {
        Ok(BrowserAttachmentProjectionTransaction::NewerProjectionRemainsDirty)
    }
}

fn remove_pending_annotation_projection_transaction(
    broker: &BrowserAttachmentBroker,
    active_workspace_key: &BrowserWorkspaceKey,
    action: &view::PendingAnnotationAction,
    sink: &mut impl BrowserAttachmentProjectionSink,
) -> Result<BrowserAttachmentProjectionTransaction, BrowserError> {
    let missing = || BrowserError::MissingAnnotation {
        id: action.annotation_id.clone(),
    };
    if &action.workspace_key != active_workspace_key {
        return Err(missing());
    }
    let current = broker.projection(active_workspace_key);
    if !current
        .pending_annotation_ids
        .iter()
        .any(|pending| pending == &action.annotation_id)
        || !current
            .pending_annotations
            .iter()
            .any(|annotation| annotation.id == action.annotation_id)
    {
        return Err(missing());
    }

    let projection = broker.detach(active_workspace_key, &action.annotation_id);
    reconcile_browser_attachment_projection_transaction(broker, &projection, sink)
}

struct NativeShellBrowserAttachmentProjectionSink<'a, 'b> {
    shell: &'a mut NativeShell,
    window: &'b Window,
}

impl BrowserAttachmentProjectionSink for NativeShellBrowserAttachmentProjectionSink<'_, '_> {
    fn acknowledge_host(
        &mut self,
        projection: &BrowserAttachmentProjection,
    ) -> Result<BrowserWorkspaceSnapshot, BrowserError> {
        self.shell
            .with_browser_host_control_barrier(self.window, |browser_host| {
                browser_host.acknowledge_attachment_projection(projection)
            })
    }

    fn persist_snapshot(
        &mut self,
        workspace_key: &BrowserWorkspaceKey,
        snapshot: BrowserWorkspaceSnapshot,
    ) -> bool {
        if !self
            .shell
            .state
            .update_browser_workspace(&workspace_key.ai_tab_id, move |current| *current = snapshot)
        {
            return false;
        }
        self.shell.save_session_state()
    }
}

impl NativeShell {
    fn new(cx: &mut Context<Self>) -> Self {
        let session_manager = SessionManager::new();
        let (mut browser_host, browser_app_config_dir, browser_config_diagnostic) =
            match crate::persistence::app_config_dir() {
                Ok(app_config_dir) => {
                    let browser_host = BrowserWebViewHost::new(&app_config_dir);
                    let trusted_app_config_dir = browser_host
                        .trusted_app_config_dir()
                        .map(std::path::Path::to_path_buf);
                    (browser_host, trusted_app_config_dir, None)
                }
                Err(error) => {
                    let diagnostic = format!(
                        "Browser configuration is unavailable; browser tools are disabled while AI terminals continue normally: {error}"
                    );
                    (
                        BrowserWebViewHost::unavailable(diagnostic.clone()),
                        None,
                        Some(diagnostic),
                    )
                }
            };
        browser_host.attach_foreground_executor(cx.foreground_executor().clone());
        let (browser_bridge, browser_inbox) = browser_command_channel(64);
        let remote_machine_state = remote::load_remote_machine_state().unwrap_or_default();
        let native_dialog_blockers = Arc::new(AtomicUsize::new(0));
        let (mut state, mut startup_notice) = match session_manager.load_workspace() {
            Ok(snapshot) => (AppState::from_workspace(snapshot), None),
            Err(error) => (
                AppState::default(),
                Some(format!(
                    "Fell back to an empty workspace because legacy state could not be loaded: {error}"
                )),
            ),
        };
        if let Some(diagnostic) = browser_config_diagnostic {
            startup_notice = Some(match startup_notice {
                Some(existing) => format!("{existing}\n{diagnostic}"),
                None => diagnostic,
            });
        }
        pid_file::cleanup_orphaned_processes();

        let process_manager = ProcessManager::new();
        process_manager.set_settings(state.config.settings.clone());
        process_manager.set_notification_sound(state.config.settings.notification_sound.clone());
        process_manager.set_log_buffer_size(state.config.settings.log_buffer_size as usize);
        let browser_gateway = if state.config.settings.browser_enabled
            && browser_host.status().available
        {
            match browser_app_config_dir.as_ref() {
                Some(browser_app_config_dir) => {
                    match BrowserGatewayHandle::start_with_app_config_dir(
                        browser_bridge.clone(),
                        browser_app_config_dir,
                    ) {
                        Ok(gateway) => {
                            process_manager
                                .set_browser_gateway_registrar(Some(gateway.registrar()));
                            Some(gateway)
                        }
                        Err(error) => {
                            let diagnostic = format!(
                                    "Browser tools are unavailable; AI terminals will continue normally: {error}"
                                );
                            startup_notice = Some(match startup_notice {
                                Some(existing) => format!("{existing}\n{diagnostic}"),
                                None => diagnostic,
                            });
                            None
                        }
                    }
                }
                None => None,
            }
        } else {
            None
        };
        let updater = UpdaterService::new();
        let remote_host_service = RemoteHostService::new(remote_machine_state.host.clone());
        let bootstrap_manager = process_manager.clone();
        remote_host_service.set_session_bootstrap_provider(Some(Arc::new(move |session_id| {
            let session_view = bootstrap_manager.session_view(session_id)?;
            let replay_bytes = bootstrap_manager
                .session_replay_bytes(session_id)
                .unwrap_or_default();
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
                        input_manager.write_user_text_to_session(&session_id, &text)
                    }
                    RemoteTerminalInput::Bytes { session_id, bytes } => {
                        input_manager.write_user_bytes_to_session(&session_id, &bytes)
                    }
                    RemoteTerminalInput::Control { session_id, bytes } => {
                        input_manager.write_bytes_to_session(&session_id, &bytes)
                    }
                    RemoteTerminalInput::Paste { session_id, text } => {
                        input_manager.paste_user_text_to_session(&session_id, &text)
                    }
                    RemoteTerminalInput::Image {
                        session_id,
                        attachment,
                        authority,
                    } => crate::remote::web::image_paste::handle_web_image_paste(
                        &input_manager,
                        &session_id,
                        &attachment,
                        || {
                            authority.as_ref().is_none_or(|authority| {
                                input_host_service.web_mutation_authority_is_current(authority)
                            })
                        },
                    ),
                    RemoteTerminalInput::ComposerBatch {
                        session_id,
                        text,
                        attachments,
                        authority,
                    } => crate::remote::web::image_paste::handle_web_composer_batch(
                        &input_manager,
                        &session_id,
                        &attachments,
                        &text,
                        || input_host_service.web_mutation_authority_is_current(&authority),
                    ),
                };
                if result.is_ok() {
                    input_host_service.record_input_write_latency(enqueued_at_epoch_ms);
                }
                result
            },
        )));
        let resize_manager = process_manager.clone();
        remote_host_service.set_terminal_resize_handler(Some(Arc::new(
            move |session_id, dimensions| {
                let _ = resize_manager.resize_session(&session_id, dimensions);
            },
        )));
        let event_host_service = remote_host_service.clone();
        process_manager.set_remote_session_handler(Some(Arc::new(move |event| match event {
            RemoteSessionEvent::Output {
                session_id,
                bytes,
                mode,
            } => {
                event_host_service.push_session_output_with_mode(&session_id, bytes, mode);
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
            RemoteSessionEvent::Semantic { draft } => {
                event_host_service.push_semantic_draft(draft);
            }
            RemoteSessionEvent::ClaudeSemantic { identity, draft } => {
                event_host_service.push_claude_semantic_draft(identity, draft);
            }
            RemoteSessionEvent::ClaudeAdapterRegistered { identity } => {
                event_host_service.push_claude_adapter_registered(identity);
            }
            RemoteSessionEvent::ClaudeAdapterRemoved { identity } => {
                event_host_service.push_claude_adapter_removed(&identity);
            }
            RemoteSessionEvent::CodexSemantic { identity, draft } => {
                event_host_service.push_codex_semantic_draft(identity, draft);
            }
            RemoteSessionEvent::CodexAdapterRegistered { identity } => {
                event_host_service.push_codex_adapter_registered(identity);
            }
            RemoteSessionEvent::CodexAdapterRemoved { identity } => {
                event_host_service.push_codex_adapter_removed(&identity);
            }
            RemoteSessionEvent::AdapterHealth {
                stable_session_key,
                health,
            } => {
                event_host_service.push_semantic_adapter_health(stable_session_key, health);
            }
        })));
        let focus_manager = process_manager.clone();
        remote_host_service.set_focused_session_handler(Some(Arc::new(move |session_id| {
            focus_manager.set_active_session(session_id);
        })));
        let remote_client_pool = RemoteClientPool::default();
        let mut terminal_notice = None;
        let restore_enabled = state
            .config
            .settings
            .restore_session_on_start
            .unwrap_or(true);

        if !restore_enabled {
            state.open_tabs.clear();
            state.active_tab_id = None;
            state.sidebar_collapsed = false;
        } else {
            reconcile_restored_browser_attachment_state(
                &mut state,
                &process_manager.browser_attachment_broker(),
            );
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
            browser_gateway,
            browser_host,
            browser_app_config_dir,
            browser_bridge,
            browser_inbox: Some(browser_inbox),
            browser_tasks_started: false,
            browser_ui: HashMap::new(),
            browser_workflow_route: None,
            browser_replay_secret_prompt: None,
            browser_replay_secret_submitter: None,
            browser_replay_repair_selection: None,
            browser_address_focus: cx.focus_handle(),
            browser_replay_secret_focus: cx.focus_handle(),
            browser_annotation_focus: cx.focus_handle(),
            browser_workflow_focus: cx.focus_handle(),
            browser_split_bounds: None,
            browser_page_bounds: None,
            browser_divider_drag: None,
            updater,
            startup_notice,
            terminal_notice,
            pending_annotation_action_notice: None,
            terminal_actionable_notice: None,
            editor_notice: None,
            remote_machine_state,
            remote_host_service,
            remote_client_pool,
            remote_mode: None,
            local_state_backup: None,
            last_remote_host_config_revision: 0,
            last_remote_snapshot_sync_at: None,
            last_remote_app_revision: 0,
            last_remote_runtime_revision: 0,
            last_remote_port_hash: 0,
            remote_live_session_generations: HashMap::new(),
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
            process_monitor: None,
            process_monitor_revision: 0,
            last_window_title: None,
            splash_image: None,
            splash_fetch_in_flight: false,
            native_dialog_blockers,
            remote_connect_request_id: 0,
            remote_status_notice: None,
            pending_shutdown_op_id: None,
            pending_window_close: false,
            pending_install_update: None,
            pending_app_termination: None,
            window_subscriptions: Vec::new(),
        };

        Self::spawn_splash_image_fetch(shell.native_dialog_blockers.clone(), cx);

        shell
    }

    fn start_browser_tasks(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.browser_tasks_started {
            return;
        }
        let Some(mut inbox) = self.browser_inbox.take() else {
            return;
        };
        self.browser_tasks_started = true;
        let this = cx.weak_entity();
        window
            .spawn(cx, move |cx: &mut gpui::AsyncWindowContext| {
                let mut async_cx = cx.clone();
                async move {
                    while let Some(request) = inbox.recv().await {
                        let updated = this.update_in(&mut async_cx, |shell, window, cx| {
                            shell.handle_browser_request(request, window, cx);
                        });
                        if updated.is_err() {
                            break;
                        }
                    }
                }
            })
            .detach();

        let this = cx.weak_entity();
        window
            .spawn(cx, async move |cx| loop {
                cx.background_executor()
                    .timer(Duration::from_millis(33))
                    .await;
                if this
                    .update_in(&mut *cx, |shell, window, cx| {
                        shell.pump_browser_events(window, cx);
                    })
                    .is_err()
                {
                    break;
                }
            })
            .detach();
    }

    fn reconcile_browser_gateway(&mut self) -> Option<String> {
        let should_run =
            self.state.settings().browser_enabled && self.browser_host.status().available;
        if !should_run {
            self.process_manager.set_browser_gateway_registrar(None);
            self.browser_gateway = None;
            return None;
        }
        if self.browser_gateway.is_some() {
            return None;
        }
        let Some(browser_app_config_dir) = self.browser_app_config_dir.as_ref() else {
            self.process_manager.set_browser_gateway_registrar(None);
            self.browser_gateway = None;
            return Some(
                "Browser configuration is unavailable; browser tools are disabled while AI terminals continue normally"
                    .to_string(),
            );
        };
        match BrowserGatewayHandle::start_with_app_config_dir(
            self.browser_bridge.clone(),
            browser_app_config_dir,
        ) {
            Ok(gateway) => {
                self.process_manager
                    .set_browser_gateway_registrar(Some(gateway.registrar()));
                self.browser_gateway = Some(gateway);
                None
            }
            Err(error) => {
                self.process_manager.set_browser_gateway_registrar(None);
                Some(format!(
                    "Browser tools are unavailable; AI terminals will continue normally: {error}"
                ))
            }
        }
    }

    fn open_browser_workspace_keys(&self) -> Vec<BrowserWorkspaceKey> {
        self.state
            .ai_tabs()
            .filter_map(|tab| BrowserWorkspaceKey::new(tab.project_id.clone(), tab.id.clone()).ok())
            .collect()
    }

    fn active_open_browser_route(&self) -> Option<BrowserWorkspaceKey> {
        (self.remote_mode.is_none()
            && self.state.settings().browser_enabled
            && self.browser_host.status().available)
            .then(|| self.active_browser_workspace())
            .flatten()
            .and_then(|(workspace_key, snapshot)| snapshot.pane_open.then_some(workspace_key))
    }

    fn browser_route_is_open(&self, workspace_key: &BrowserWorkspaceKey) -> bool {
        self.active_open_browser_route().as_ref() == Some(workspace_key)
    }

    fn browser_route_can_be_opened(&self, workspace_key: &BrowserWorkspaceKey) -> bool {
        self.remote_mode.is_none()
            && self.state.settings().browser_enabled
            && self.browser_host.status().available
            && self
                .active_browser_workspace()
                .is_some_and(|(active_key, _)| active_key == *workspace_key)
    }

    fn dispatch_browser_command(
        &mut self,
        workspace_key: &BrowserWorkspaceKey,
        command: BrowserCommand,
        window: &Window,
    ) -> Result<BrowserResponse, BrowserError> {
        let opens_selected_route = matches!(
            command,
            BrowserCommand::Ensure { .. } | BrowserCommand::SetPaneOpen { open: true }
        ) && self.browser_route_can_be_opened(workspace_key);
        if !self.browser_route_is_open(workspace_key) && !opens_selected_route {
            return Err(BrowserError::CrashedView {
                message: "browser command route is not the active visible local AI conversation"
                    .to_string(),
            });
        }
        let open_workspaces = self.open_browser_workspace_keys();
        let active_browser_route = self.active_open_browser_route();
        let browser_bridge = self.browser_bridge.clone();
        let event_bridge = browser_bridge.clone();
        let result = browser_bridge.with_locked_host_work_for_command(
            workspace_key,
            &command,
            |controls, lifecycle_requests, repair_cleanups| {
                self.browser_host
                    .publish_pending_user_input_cutoffs(|event, _state| {
                        event_bridge.observe_host_event_under_host_control_barrier(event);
                    });
                match &command {
                    BrowserCommand::Stop { .. }
                    | BrowserCommand::CloseTab { .. }
                    | BrowserCommand::ResetWorkspace => {
                        self.retire_browser_replay_ui_after_interrupt(workspace_key);
                    }
                    BrowserCommand::ClearProjectProfile => {
                        for key in open_workspaces
                            .iter()
                            .filter(|key| key.project_id == workspace_key.project_id)
                        {
                            self.retire_browser_replay_ui_after_interrupt(key);
                        }
                    }
                    _ => {}
                }
                let browser_host = &mut self.browser_host;
                for control in controls {
                    browser_host.handle_control(control);
                }
                for cleanup in repair_cleanups {
                    browser_host.handle_repair_highlight_cleanup(window, cleanup);
                }
                for request in lifecycle_requests {
                    let _ = route_browser_request_for_active_workspace(
                        active_browser_route.as_ref(),
                        request,
                        |request| {
                            browser_host.handle_request(window, request);
                        },
                    );
                }
                browser_host.handle_command(window, workspace_key, command.clone())
            },
        );
        match &result {
            Ok(response) => self.synchronize_browser_response(workspace_key, &command, response),
            Err(error) => {
                self.browser_ui
                    .entry(workspace_key.clone())
                    .or_default()
                    .diagnostic = Some(error.to_string());
            }
        }
        result
    }

    fn synchronize_browser_response(
        &mut self,
        workspace_key: &BrowserWorkspaceKey,
        command: &BrowserCommand,
        response: &BrowserResponse,
    ) {
        let open_workspaces = self.open_browser_workspace_keys();
        let mut persist = false;
        if let Some(sync) = browser_response_sync(&open_workspaces, workspace_key, response) {
            let mut snapshot = sync.snapshot;
            self.project_local_browser_snapshot(&sync.workspace_key, &mut snapshot);
            persist = self.state.update_browser_workspace(
                &sync.workspace_key.ai_tab_id,
                move |current| {
                    *current = snapshot;
                },
            );
            let ui = self.browser_ui.entry(sync.workspace_key).or_default();
            if !ui.address_focused {
                ui.address_draft = None;
            }
            ui.diagnostic = None;
        } else if matches!(response, BrowserResponse::Acknowledged) {
            match command {
                BrowserCommand::ResetWorkspace => {
                    self.process_manager
                        .browser_attachment_broker()
                        .reset_workspace_state(workspace_key);
                    persist = self
                        .state
                        .update_browser_workspace(&workspace_key.ai_tab_id, |snapshot| {
                            *snapshot = BrowserWorkspaceSnapshot::default()
                        });
                    self.browser_ui.remove(workspace_key);
                }
                BrowserCommand::ClearProjectProfile => {
                    let project_id = workspace_key.project_id.clone();
                    let tab_ids: Vec<_> = self
                        .state
                        .ai_tabs()
                        .filter(|tab| tab.project_id == project_id)
                        .map(|tab| tab.id.clone())
                        .collect();
                    for tab_id in tab_ids {
                        if let Some(key) = self.state.browser_workspace_key(&tab_id) {
                            self.process_manager
                                .browser_attachment_broker()
                                .reset_workspace_state(&key);
                        }
                        persist = self.state.update_browser_workspace(&tab_id, |snapshot| {
                            *snapshot = BrowserWorkspaceSnapshot::default();
                        }) || persist;
                    }
                    self.browser_ui
                        .retain(|key, _| key.project_id != project_id);
                }
                _ => {}
            }
        }
        if let BrowserResponse::Status { status } = response {
            self.browser_ui
                .entry(workspace_key.clone())
                .or_default()
                .diagnostic = status.diagnostic.clone();
        }
        if persist {
            self.save_session_state();
        }
    }

    fn project_local_browser_snapshot(
        &self,
        workspace_key: &BrowserWorkspaceKey,
        snapshot: &mut BrowserWorkspaceSnapshot,
    ) -> bool {
        if self.remote_mode.is_some() {
            return false;
        }
        let broker = self.process_manager.browser_attachment_broker();
        broker.observe_workspace(workspace_key.clone(), snapshot);
        broker.overlay_snapshot(workspace_key, snapshot)
    }

    fn reconcile_browser_attachment_projections(&mut self, window: &Window) -> bool {
        if self.remote_mode.is_some() {
            return false;
        }
        let broker = self.process_manager.browser_attachment_broker();
        let projections = broker.dirty_projections();
        let mut changed = false;
        for projection in projections {
            if !self.browser_route_is_open(&projection.workspace_key) {
                continue;
            }
            let transaction = {
                let mut sink = NativeShellBrowserAttachmentProjectionSink {
                    shell: self,
                    window,
                };
                reconcile_browser_attachment_projection_transaction(&broker, &projection, &mut sink)
            };
            match transaction {
                Ok(BrowserAttachmentProjectionTransaction::Applied)
                | Ok(BrowserAttachmentProjectionTransaction::NewerProjectionRemainsDirty) => {
                    changed = true;
                }
                Ok(BrowserAttachmentProjectionTransaction::PersistFailed) => {}
                Err(error) => {
                    self.browser_ui
                        .entry(projection.workspace_key.clone())
                        .or_default()
                        .diagnostic = Some(error.to_string());
                }
            }
        }
        changed
    }

    fn handle_browser_request(
        &mut self,
        request: BrowserCommandRequest,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let workspace_key = request.workspace_key().clone();
        let active_browser_route = self.active_open_browser_route();
        let route_result = self.with_browser_host_control_barrier(window, |browser_host| {
            route_browser_request_for_active_workspace(
                active_browser_route.as_ref(),
                request,
                |request| browser_host.handle_request(window, request),
            )
        });
        if let Err(error) = route_result {
            self.browser_ui.entry(workspace_key).or_default().diagnostic = Some(error.to_string());
        }
        self.sync_browser_host_visibility(Some(window));
        cx.notify();
    }

    fn pump_browser_events(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let browser_bridge = self.browser_bridge.clone();
        let events = self.with_browser_host_control_barrier(window, |browser_host| {
            let mut events = browser_host.drain_events_with_pre_apply_observer(|event, _state| {
                browser_bridge.observe_host_event_under_host_control_barrier(event);
            });
            browser_host.pump_async_completions(window);
            let completion_events =
                browser_host.drain_events_with_pre_apply_observer(|event, _state| {
                    browser_bridge.observe_host_event_under_host_control_barrier(event);
                });
            events.extend(completion_events);
            events
        });
        if self.try_finish_app_termination(cx) {
            return;
        }
        let projected_attachments = self.reconcile_browser_attachment_projections(window);
        let replay_changed = self.reconcile_browser_replay_state(window, cx);
        if events.is_empty() {
            if projected_attachments || replay_changed {
                self.sync_browser_host_visibility(Some(window));
                cx.notify();
            }
            return;
        }
        let mut events = VecDeque::from(events);
        let open_workspaces = self.open_browser_workspace_keys();
        let mut persist = false;
        while let Some(event) = events.pop_front() {
            let Some(plan) = browser_event_plan(&open_workspaces, &event) else {
                continue;
            };
            match plan {
                BrowserPaneEventPlan::SyncSnapshot {
                    workspace_key,
                    interrupt_agent,
                    loading,
                    ..
                } => {
                    let _ = interrupt_agent;
                    if let Some(loading) = loading {
                        self.browser_ui
                            .entry(workspace_key.clone())
                            .or_default()
                            .loading = loading;
                    }
                    if let Some(mut snapshot) = self
                        .browser_host
                        .workspace_snapshot(&workspace_key)
                        .cloned()
                    {
                        self.project_local_browser_snapshot(&workspace_key, &mut snapshot);
                        persist =
                            self.state.update_browser_workspace(
                                &workspace_key.ai_tab_id,
                                move |current| *current = snapshot,
                            ) || persist;
                    }
                }
                BrowserPaneEventPlan::OpenLogicalTab { workspace_key, url } => {
                    let _ = self.dispatch_browser_command(
                        &workspace_key,
                        BrowserCommand::CreateTab { url: Some(url) },
                        window,
                    );
                }
                BrowserPaneEventPlan::DownloadStatus {
                    workspace_key,
                    message,
                } => {
                    self.browser_ui
                        .entry(workspace_key)
                        .or_default()
                        .action_status = Some(message);
                }
                BrowserPaneEventPlan::Diagnostic {
                    workspace_key,
                    message,
                } => {
                    self.browser_ui.entry(workspace_key).or_default().diagnostic = Some(message);
                }
                BrowserPaneEventPlan::CaptureAnnotation {
                    workspace_key,
                    tab_id,
                    candidate,
                } => {
                    if let Some(selection) =
                        self.browser_replay_repair_selection
                            .clone()
                            .filter(|selection| {
                                selection.workspace_key == workspace_key
                                    && selection.tab_id == tab_id
                            })
                    {
                        let coordinator = self.browser_bridge.replay_coordinator();
                        let active = coordinator.active_state(&workspace_key);
                        let exact = active.as_ref().is_some_and(|active| {
                            active.instance.id() == selection.instance_id
                                && active.projection.status
                                    == BrowserReplayStatus::PausedLocatorRepair
                                && active.repair.as_ref().is_some_and(|repair| {
                                    repair.repair_id == selection.repair_id
                                        && repair.tab_id == selection.tab_id
                                        && repair.revision == selection.revision
                                        && candidate.revision == selection.revision
                                })
                        });
                        let repair = exact
                            .then(|| {
                                coordinator.exact_repair(
                                    &workspace_key,
                                    selection.instance_id,
                                    selection.repair_id,
                                )
                            })
                            .transpose();

                        let cleared =
                            self.with_browser_host_control_barrier(window, |browser_host| {
                                browser_host.cancel_annotation_selection(
                                    &selection.workspace_key,
                                    &selection.tab_id,
                                )
                            });
                        self.browser_replay_repair_selection = None;
                        let ui = self.browser_ui.entry(workspace_key.clone()).or_default();
                        ui.annotation_mode = false;
                        if let Err(error) = cleared {
                            ui.diagnostic = Some(error.to_string());
                            continue;
                        }
                        let repair = match repair {
                            Ok(Some(repair)) => repair,
                            Ok(None) | Err(_) => {
                                ui.diagnostic = Some(
                                    "Replay repair selection is no longer current.".to_string(),
                                );
                                continue;
                            }
                        };
                        let replacement = match browser_replay_repair_candidate_from_annotation(
                            &candidate,
                            selection.revision,
                        ) {
                            Ok(candidate) => candidate,
                            Err(error) => {
                                ui.diagnostic = Some(error.to_string());
                                continue;
                            }
                        };
                        ui.diagnostic = None;
                        ui.action_status = Some("Previewing replacement element...".to_string());

                        let controller = self
                            .browser_bridge
                            .bind(workspace_key.clone(), Duration::from_secs(300));
                        let result_coordinator = coordinator.clone();
                        let result_workspace = workspace_key.clone();
                        let instance_id = selection.instance_id;
                        let repair_id = selection.repair_id;
                        let this = cx.weak_entity();
                        window
                            .spawn(cx, async move |cx| {
                                let result = controller
                                    .request_replay_repair_preview(
                                        &coordinator,
                                        &repair,
                                        replacement,
                                        BrowserInvocationActor::User,
                                    )
                                    .await;
                                let _ = this.update_in(&mut *cx, |shell, _window, cx| {
                                    let still_exact = result_coordinator
                                        .active_state(&result_workspace)
                                        .is_some_and(|active| {
                                            active.instance.id() == instance_id
                                                && active.repair.as_ref().is_some_and(|repair| {
                                                    repair.repair_id == repair_id
                                                })
                                        });
                                    if !still_exact {
                                        return;
                                    }
                                    let ui = shell
                                        .browser_ui
                                        .entry(result_workspace.clone())
                                        .or_default();
                                    match result {
                                        Ok(_) => {
                                            ui.diagnostic = None;
                                            ui.action_status =
                                                Some("Replacement preview ready".to_string());
                                        }
                                        Err(error) => {
                                            ui.diagnostic = Some(error.to_string());
                                        }
                                    }
                                    cx.notify();
                                });
                            })
                            .detach();
                        continue;
                    }
                    let result = self.dispatch_browser_command(
                        &workspace_key,
                        BrowserCommand::CaptureAnnotation { tab_id, candidate },
                        window,
                    );
                    let ui = self.browser_ui.entry(workspace_key).or_default();
                    ui.annotation_mode = false;
                    match result {
                        Ok(_) => {
                            ui.action_status =
                                Some("Capturing annotation screenshot...".to_string());
                            ui.diagnostic = None;
                        }
                        Err(error) => ui.diagnostic = Some(error.to_string()),
                    }
                }
                BrowserPaneEventPlan::ShowAnnotationDraft {
                    workspace_key,
                    draft,
                } => {
                    let ui = self.browser_ui.entry(workspace_key).or_default();
                    ui.annotation_mode = false;
                    ui.annotation_draft = Some(draft);
                    ui.annotation_comment.clear();
                    ui.annotation_cursor = 0;
                    ui.annotation_focused = true;
                    ui.action_status = Some("Annotation screenshot captured".to_string());
                    ui.diagnostic = None;
                    window.focus(&self.browser_annotation_focus);
                }
                BrowserPaneEventPlan::AnnotationModeChanged {
                    workspace_key,
                    enabled,
                } => {
                    let ui = self.browser_ui.entry(workspace_key).or_default();
                    ui.annotation_mode = enabled;
                }
                BrowserPaneEventPlan::ClearAnnotation { workspace_key } => {
                    if self
                        .browser_replay_repair_selection
                        .as_ref()
                        .is_some_and(|selection| selection.workspace_key == workspace_key)
                    {
                        self.cancel_browser_replay_repair_selection(Some(window));
                    }
                    let ui = self.browser_ui.entry(workspace_key).or_default();
                    ui.annotation_mode = false;
                    ui.annotation_draft = None;
                    ui.annotation_comment.clear();
                    ui.annotation_cursor = 0;
                    ui.annotation_focused = false;
                }
                BrowserPaneEventPlan::ConfirmApproval {
                    workspace_key,
                    tab_id,
                    request,
                } => {
                    let pending = self.with_browser_host_control_barrier(window, |browser_host| {
                        browser_host.is_pending_approval(
                            &workspace_key,
                            &tab_id,
                            &request.operation_id,
                        )
                    });
                    if !pending {
                        continue;
                    }
                    let description = format!(
                        "Actor: {:?}\nIntent: {}\nRisk: {:?}\nAction: {}\nOrigin: {}",
                        request.actor,
                        request.intent,
                        request.effective_risk,
                        request.action_summary,
                        request.origin_url
                    );
                    let _dialog_pause = self.pause_for_native_dialog();
                    let approved = MessageDialog::new()
                        .set_level(MessageLevel::Warning)
                        .set_title("Confirm Browser Action")
                        .set_description(description)
                        .set_buttons(MessageButtons::YesNo)
                        .show()
                        == MessageDialogResult::Yes;
                    let browser_bridge = self.browser_bridge.clone();
                    let (post_dialog_events, resolution) =
                        self.with_browser_host_control_barrier(window, |browser_host| {
                            let events = browser_host.drain_events_with_pre_apply_observer(
                                |event, _state| {
                                    browser_bridge
                                        .observe_host_event_under_host_control_barrier(event);
                                },
                            );
                            let resolution = browser_host.resolve_approval(
                                window,
                                &workspace_key,
                                &tab_id,
                                &request.operation_id,
                                approved,
                            );
                            (events, resolution)
                        });
                    events.extend(post_dialog_events);
                    match resolution {
                        Ok(()) => {
                            self.browser_ui
                                .entry(workspace_key)
                                .or_default()
                                .action_status = Some(if approved {
                                "Approved browser action".to_string()
                            } else {
                                "Denied browser action".to_string()
                            });
                        }
                        Err(error) => {
                            self.browser_ui.entry(workspace_key).or_default().diagnostic =
                                Some(error.to_string());
                        }
                    }
                }
            }
        }
        if persist {
            self.save_session_state();
        }
        self.sync_browser_host_visibility(Some(window));
        cx.notify();
    }

    fn with_browser_host_control_barrier<R>(
        &mut self,
        window: &Window,
        enter_host: impl FnOnce(&mut BrowserWebViewHost) -> R,
    ) -> R {
        let active_browser_route = self.active_open_browser_route();
        let browser_host = &mut self.browser_host;
        let browser_bridge = self.browser_bridge.clone();
        browser_bridge.with_locked_host_work_and_repair_cleanups(
            |controls, lifecycle_requests, repair_cleanups| {
                browser_host.publish_pending_user_input_cutoffs(|event, _state| {
                    browser_bridge.observe_host_event_under_host_control_barrier(event);
                });
                for control in controls {
                    browser_host.handle_control(control);
                }
                for cleanup in repair_cleanups {
                    browser_host.handle_repair_highlight_cleanup(window, cleanup);
                }
                for request in lifecycle_requests {
                    let _ = route_browser_request_for_active_workspace(
                        active_browser_route.as_ref(),
                        request,
                        |request| browser_host.handle_request(window, request),
                    );
                }
                enter_host(browser_host)
            },
        )
    }

    fn browser_pane_context(&self) -> BrowserPaneContext {
        BrowserPaneContext {
            browser_enabled: self.state.settings().browser_enabled,
            platform_supported: self.browser_host.status().available,
            active_surface: self.state.active_tab().map(|tab| match tab.tab_type {
                TabType::Server => BrowserPaneSurface::Server,
                TabType::Claude => BrowserPaneSurface::Claude,
                TabType::Codex => BrowserPaneSurface::Codex,
                TabType::Ssh => BrowserPaneSurface::Ssh,
            }),
            editor_open: self.editor_panel.is_some(),
            modal_open: self.process_monitor.is_some() || self.add_project_wizard.is_some(),
        }
    }

    fn reconcile_browser_replay_repair_selection(&mut self, window: &Window) -> bool {
        let Some(selection) = self.browser_replay_repair_selection.clone() else {
            return false;
        };
        let active_workspace = self
            .active_browser_workspace()
            .map(|(workspace_key, _)| workspace_key);
        if active_workspace.as_ref() != Some(&selection.workspace_key) {
            return self.cancel_browser_replay_repair_selection(Some(window));
        }
        let coordinator = self.browser_bridge.replay_coordinator();
        let active = coordinator.active_state(&selection.workspace_key);
        let live_snapshot = self
            .browser_host
            .workspace_snapshot(&selection.workspace_key);
        let exact = active.as_ref().is_some_and(|active| {
            active.instance.id() == selection.instance_id
                && active.projection.status == BrowserReplayStatus::PausedLocatorRepair
                && active.repair.as_ref().is_some_and(|repair| {
                    repair.repair_id == selection.repair_id
                        && repair.tab_id == selection.tab_id
                        && repair.revision == selection.revision
                })
        }) && live_snapshot.is_some_and(|snapshot| {
            snapshot.revision == selection.revision
                && snapshot.selected_tab_id.as_deref() == Some(selection.tab_id.as_str())
        });
        if exact {
            return false;
        }
        self.cancel_browser_replay_repair_selection(Some(window))
    }

    fn cancel_browser_replay_repair_selection(&mut self, window: Option<&Window>) -> bool {
        let Some(selection) = self.browser_replay_repair_selection.take() else {
            return false;
        };
        let result = if let Some(window) = window {
            self.with_browser_host_control_barrier(window, |browser_host| {
                browser_host
                    .cancel_annotation_selection(&selection.workspace_key, &selection.tab_id)
            })
        } else {
            self.browser_host
                .cancel_annotation_selection(&selection.workspace_key, &selection.tab_id)
        };
        let ui = self.browser_ui.entry(selection.workspace_key).or_default();
        ui.annotation_mode = false;
        if let Err(error) = result {
            ui.diagnostic = Some(error.to_string());
        }
        true
    }

    fn reconcile_browser_replay_state(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let mut changed = self.reconcile_browser_replay_repair_selection(window);
        let Some(workspace_key) = self
            .active_browser_workspace()
            .map(|(workspace_key, _)| workspace_key)
        else {
            return self
                .close_browser_replay_secret_prompt_for_route(None)
                .is_some()
                || changed;
        };
        let active = self
            .browser_bridge
            .replay_coordinator()
            .active_state(&workspace_key);
        let Some(active) = active else {
            if self
                .browser_replay_secret_prompt
                .as_ref()
                .is_some_and(|vault| vault.workspace_key() == &workspace_key)
            {
                if let Some(vault) = self.browser_replay_secret_prompt.take() {
                    let instance = vault.instance().clone();
                    self.browser_replay_secret_submitter = None;
                    let _ = vault.replay_replaced(&instance);
                    changed = true;
                }
            } else {
                changed = self
                    .close_browser_replay_secret_prompt_for_route(Some(&workspace_key))
                    .is_some()
                    || changed;
            }
            return changed;
        };

        if active.projection.status != BrowserReplayStatus::NeedsUserSecret {
            if self
                .browser_replay_secret_prompt
                .as_ref()
                .is_some_and(|vault| vault.workspace_key() == &workspace_key)
            {
                if let Some(vault) = self.browser_replay_secret_prompt.take() {
                    let instance = vault.instance().clone();
                    self.browser_replay_secret_submitter = None;
                    let _ = vault.replay_replaced(&instance);
                    changed = true;
                }
            } else {
                changed = self
                    .close_browser_replay_secret_prompt_for_route(Some(&workspace_key))
                    .is_some()
                    || changed;
            }
            return changed;
        }

        if self
            .browser_replay_secret_prompt
            .as_ref()
            .is_some_and(|vault| vault.same_instance(&active.instance))
        {
            return changed;
        }
        if let Some(vault) = self.browser_replay_secret_prompt.take() {
            let instance = vault.instance().clone();
            self.browser_replay_secret_submitter = None;
            let _ = vault.replay_replaced(&instance);
        }
        let prompt_instance = active.instance;
        let input_names = active.projection.unresolved_secret_inputs;
        let coordinator = self.browser_bridge.replay_coordinator();
        let instance = prompt_instance.clone();
        let submitter: BrowserReplaySecretSubmitter = Box::new(move |submission| {
            coordinator
                .submit_secrets(&instance, submission)
                .map(|_| ())
        });
        match self.install_browser_replay_secret_prompt(
            prompt_instance,
            input_names,
            submitter,
            window,
            cx,
        ) {
            Ok(_) => changed = true,
            Err(error) => {
                self.browser_ui.entry(workspace_key).or_default().diagnostic =
                    Some(error.to_string());
                changed = true;
            }
        }
        changed
    }

    fn install_browser_replay_secret_prompt(
        &mut self,
        instance: BrowserReplayInstance,
        input_names: Vec<String>,
        submitter: BrowserReplaySecretSubmitter,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<BrowserReplaySecretPromptEvent, BrowserReplaySecretError> {
        if let Some(previous) = self.browser_replay_secret_prompt.take() {
            let previous_instance = previous.instance().clone();
            let _ = previous.replay_replaced(&previous_instance);
        }
        self.browser_replay_secret_submitter = None;
        let (vault, event) = BrowserReplaySecretPromptVault::install(instance, input_names)?;
        self.browser_replay_secret_prompt = Some(vault);
        self.browser_replay_secret_submitter = Some(submitter);
        window.focus(&self.browser_replay_secret_focus);
        cx.notify();
        Ok(event)
    }

    fn focus_browser_replay_secret_prompt(
        &mut self,
        workspace_key: &BrowserWorkspaceKey,
        instance_id: u64,
        input_name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<BrowserReplaySecretPromptEvent, BrowserReplaySecretError> {
        let vault = self
            .browser_replay_secret_prompt
            .as_mut()
            .filter(|vault| {
                vault.workspace_key() == workspace_key && vault.instance().id() == instance_id
            })
            .ok_or(BrowserReplaySecretError::StaleAuthority)?;
        let instance = vault.instance().clone();
        let event = vault.focus(&instance, input_name)?;
        window.focus(&self.browser_replay_secret_focus);
        cx.notify();
        Ok(event)
    }

    fn edit_browser_replay_secret_prompt(
        &mut self,
        workspace_key: &BrowserWorkspaceKey,
        instance_id: u64,
        input_name: &str,
        text: &str,
        cx: &mut Context<Self>,
    ) -> Result<BrowserReplaySecretPromptEvent, BrowserReplaySecretError> {
        let vault = self
            .browser_replay_secret_prompt
            .as_mut()
            .filter(|vault| {
                vault.workspace_key() == workspace_key && vault.instance().id() == instance_id
            })
            .ok_or(BrowserReplaySecretError::StaleAuthority)?;
        let instance = vault.instance().clone();
        let event = vault.edit(&instance, input_name, text)?;
        cx.notify();
        Ok(event)
    }

    fn backspace_browser_replay_secret_prompt(
        &mut self,
        workspace_key: &BrowserWorkspaceKey,
        instance_id: u64,
        input_name: &str,
        cx: &mut Context<Self>,
    ) -> Result<BrowserReplaySecretPromptEvent, BrowserReplaySecretError> {
        let vault = self
            .browser_replay_secret_prompt
            .as_mut()
            .filter(|vault| {
                vault.workspace_key() == workspace_key && vault.instance().id() == instance_id
            })
            .ok_or(BrowserReplaySecretError::StaleAuthority)?;
        let instance = vault.instance().clone();
        let event = vault.backspace(&instance, input_name)?;
        cx.notify();
        Ok(event)
    }

    fn submit_browser_replay_secret_prompt(
        &mut self,
        workspace_key: &BrowserWorkspaceKey,
        instance_id: u64,
        cx: &mut Context<Self>,
    ) -> Result<BrowserReplaySecretPromptEvent, BrowserReplaySecretError> {
        let exact = self
            .browser_replay_secret_prompt
            .as_ref()
            .filter(|vault| {
                vault.workspace_key() == workspace_key && vault.instance().id() == instance_id
            })
            .ok_or(BrowserReplaySecretError::StaleAuthority)?;
        if exact.projection().is_set.iter().any(|is_set| !is_set) {
            return Err(BrowserReplaySecretError::InvalidSubmission);
        }
        let vault = self
            .browser_replay_secret_prompt
            .take()
            .ok_or(BrowserReplaySecretError::StaleAuthority)?;
        let instance = vault.instance().clone();
        let (submission, event) = vault.submit(&instance)?;
        let submitter = self
            .browser_replay_secret_submitter
            .take()
            .ok_or(BrowserReplaySecretError::StaleAuthority)?;
        submitter(submission)?;
        cx.notify();
        Ok(event)
    }

    fn cancel_browser_replay_secret_prompt(
        &mut self,
        workspace_key: &BrowserWorkspaceKey,
        instance_id: u64,
        cx: &mut Context<Self>,
    ) -> Result<BrowserReplaySecretPromptEvent, BrowserReplaySecretError> {
        let exact = self
            .browser_replay_secret_prompt
            .as_ref()
            .filter(|vault| {
                vault.workspace_key() == workspace_key && vault.instance().id() == instance_id
            })
            .ok_or(BrowserReplaySecretError::StaleAuthority)?;
        let instance = exact.instance().clone();
        self.browser_bridge
            .replay_coordinator()
            .cancel(&instance)
            .map_err(|_| BrowserReplaySecretError::StaleAuthority)?;
        let vault = self
            .browser_replay_secret_prompt
            .take()
            .ok_or(BrowserReplaySecretError::StaleAuthority)?;
        self.browser_replay_secret_submitter = None;
        let event = vault.cancel(&instance)?;
        cx.notify();
        Ok(event)
    }

    fn close_browser_replay_secret_prompt_for_route(
        &mut self,
        route: Option<&BrowserWorkspaceKey>,
    ) -> Option<BrowserReplaySecretPromptEvent> {
        let close = self
            .browser_replay_secret_prompt
            .as_ref()
            .is_some_and(|vault| route != Some(vault.workspace_key()));
        if !close {
            return None;
        }
        let vault = self.browser_replay_secret_prompt.take()?;
        let instance = vault.instance().clone();
        self.browser_replay_secret_submitter = None;
        vault.route_switch(&instance).ok()
    }

    fn retire_browser_replay_ui_after_interrupt(&mut self, workspace_key: &BrowserWorkspaceKey) {
        if self
            .browser_replay_secret_prompt
            .as_ref()
            .is_some_and(|vault| vault.workspace_key() == workspace_key)
        {
            if let Some(vault) = self.browser_replay_secret_prompt.take() {
                let instance = vault.instance().clone();
                self.browser_replay_secret_submitter = None;
                let _ = vault.route_switch(&instance);
            }
        }
        if self
            .browser_replay_repair_selection
            .as_ref()
            .is_some_and(|selection| &selection.workspace_key == workspace_key)
        {
            self.cancel_browser_replay_repair_selection(None);
        }
    }

    fn discard_browser_workflow_state_after_replay_interrupt(
        &mut self,
        workspace_key: &BrowserWorkspaceKey,
    ) {
        self.retire_browser_replay_ui_after_interrupt(workspace_key);
        self.browser_host.discard_workflow_state(workspace_key);
        if let Some(ui) = self.browser_ui.get_mut(workspace_key) {
            ui.workflow_preview = None;
            ui.workflow_editor = None;
        }
        if self.browser_workflow_route.as_ref() == Some(workspace_key) {
            self.browser_workflow_route = None;
        }
    }

    fn interrupt_active_browser_replay_before_route_change(
        &mut self,
        next_workspace: Option<&BrowserWorkspaceKey>,
    ) {
        let Some(previous) = self
            .active_browser_workspace()
            .map(|(workspace_key, _)| workspace_key)
        else {
            return;
        };
        if next_workspace == Some(&previous) {
            return;
        }
        self.browser_bridge.interrupt_workspace(&previous);
        self.discard_browser_workflow_state_after_replay_interrupt(&previous);
    }

    fn interrupt_browser_workspace_before_teardown(&mut self, workspace_key: &BrowserWorkspaceKey) {
        self.browser_bridge.interrupt_workspace(workspace_key);
        self.discard_browser_workflow_state_after_replay_interrupt(workspace_key);
    }

    fn interrupt_browser_project_before_mutation(&mut self, project_id: &str) {
        self.browser_bridge.interrupt_project(project_id);
        let workspaces = self
            .state
            .ai_tabs()
            .filter(|tab| tab.project_id == project_id)
            .filter_map(|tab| browser_workspace_key_for_ai_tab(Some(tab)))
            .collect::<Vec<_>>();
        for workspace_key in workspaces {
            self.discard_browser_workflow_state_after_replay_interrupt(&workspace_key);
        }
    }

    fn interrupt_all_browser_replays_before_shutdown(&mut self) {
        let browser_bridge = self.browser_bridge.clone();
        browser_bridge.interrupt_all_with_host_cleanup(|| {
            self.browser_host.interrupt_all_local_work();
        });
        let workspaces = self.open_browser_workspace_keys();
        for workspace_key in workspaces {
            self.discard_browser_workflow_state_after_replay_interrupt(&workspace_key);
        }
    }

    fn begin_browser_window_teardown(&mut self) -> BrowserAppExitDisposition {
        self.interrupt_all_browser_replays_before_shutdown();
        self.browser_host.begin_native_window_teardown()
    }

    fn resume_browser_window_after_canceled_shutdown(&mut self) {
        if self.pending_app_termination.is_some() {
            return;
        }
        let _ = self
            .browser_host
            .resume_native_window_after_canceled_teardown();
    }

    fn request_app_termination(
        &mut self,
        requested: PendingAppTermination,
        cx: &mut Context<Self>,
    ) {
        self.pending_app_termination = Some(
            self.pending_app_termination
                .map_or(requested, |current| current.coalesce(requested)),
        );
        retire_pending_shutdown_for_forced_termination(
            &mut self.pending_shutdown_op_id,
            &mut self.pending_window_close,
        );
        match self.begin_browser_window_teardown() {
            BrowserAppExitDisposition::ExitNow => {
                let _ = self.try_finish_app_termination(cx);
            }
            BrowserAppExitDisposition::Deferred => {
                self.terminal_notice = Some(
                    "Waiting for browser initialization to stop before exiting...".to_string(),
                );
                cx.notify();
            }
        }
    }

    fn try_finish_app_termination(&mut self, cx: &mut Context<Self>) -> bool {
        let native_window_teardown_ready = self.browser_host.native_window_teardown_ready();
        let Some(termination) = take_ready_app_termination(
            &mut self.pending_app_termination,
            native_window_teardown_ready,
        ) else {
            return false;
        };
        execute_app_termination(termination, cx);
        true
    }

    fn sync_browser_host_visibility(&mut self, window: Option<&Window>) {
        let workflow_route = if self.remote_mode.is_none()
            && self.state.settings().browser_enabled
            && self.browser_host.status().available
        {
            self.state.active_tab().and_then(|tab| {
                matches!(tab.tab_type, TabType::Claude | TabType::Codex)
                    .then(|| BrowserWorkspaceKey::new(tab.project_id.clone(), tab.id.clone()).ok())
                    .flatten()
            })
        } else {
            None
        };
        self.close_browser_replay_secret_prompt_for_route(workflow_route.as_ref());
        if self.browser_workflow_route != workflow_route {
            if let Some(previous) = self.browser_workflow_route.take() {
                self.browser_host.discard_workflow_state(&previous);
                if let Some(ui) = self.browser_ui.get_mut(&previous) {
                    ui.workflow_preview = None;
                    ui.workflow_editor = None;
                }
            }
            self.browser_workflow_route = workflow_route;
        }
        let context = self.browser_pane_context();
        let plan = self.state.active_tab().and_then(|tab| {
            let key = BrowserWorkspaceKey::new(tab.project_id.clone(), tab.id.clone()).ok()?;
            let snapshot = tab.browser_workspace.as_ref()?;
            Some(browser_host_reconcile_plan(
                &context,
                &key,
                snapshot,
                self.browser_divider_drag.is_some(),
                self.browser_host.workspace_snapshot(&key),
            ))
        });
        let mut active_workspace = match plan.as_ref().map(|plan| &plan.visibility) {
            Some(BrowserHostVisibility::Selected { workspace_key, .. }) => {
                Some(workspace_key.clone())
            }
            Some(BrowserHostVisibility::Hidden) | None => None,
        };
        if active_workspace.as_ref().is_some_and(|workspace_key| {
            self.browser_host.page_recording_status(workspace_key)
                == crate::browser::BrowserRecordingStatus::Review
        }) {
            active_workspace = None;
        }
        if self
            .browser_replay_secret_prompt
            .as_ref()
            .is_some_and(|vault| Some(vault.workspace_key()) == active_workspace.as_ref())
        {
            active_workspace = None;
        }
        if let Some(snapshot) = plan.and_then(|plan| plan.ensure_snapshot) {
            let Some(window) = window else {
                let _ = self.browser_host.set_active_workspace(None);
                return;
            };
            if let Some(workspace_key) = active_workspace.as_ref() {
                if self
                    .dispatch_browser_command(
                        workspace_key,
                        BrowserCommand::Ensure { snapshot },
                        window,
                    )
                    .is_err()
                {
                    active_workspace = None;
                }
            }
        }
        if let Err(error) = self
            .browser_host
            .set_active_workspace(active_workspace.clone())
        {
            if let Some(workspace_key) = active_workspace {
                self.browser_ui.entry(workspace_key).or_default().diagnostic =
                    Some(error.to_string());
            }
        }
    }

    fn active_browser_model(&self) -> Option<BrowserPaneModel> {
        let tab = self.state.active_tab()?;
        if !matches!(tab.tab_type, TabType::Claude | TabType::Codex) {
            return None;
        }
        let workspace_key =
            BrowserWorkspaceKey::new(tab.project_id.clone(), tab.id.clone()).ok()?;
        let snapshot = tab.browser_workspace.clone().unwrap_or_default();
        let ui = self
            .browser_ui
            .get(&workspace_key)
            .cloned()
            .unwrap_or_default();
        let diagnostic = ui
            .diagnostic
            .or_else(|| self.process_manager.browser_diagnostic(&tab.id));
        let workflow_review = if self.remote_mode.is_none() {
            self.browser_host.workflow_review_projection(
                &workspace_key,
                match tab.tab_type {
                    TabType::Claude => BrowserPaneSurface::Claude,
                    TabType::Codex => BrowserPaneSurface::Codex,
                    TabType::Server | TabType::Ssh => return None,
                },
            )
        } else {
            None
        };
        let replay_secret_prompt = self
            .browser_replay_secret_prompt
            .as_ref()
            .filter(|vault| vault.workspace_key() == &workspace_key)
            .map(BrowserReplaySecretPromptVault::projection);
        let replay_coordinator = self.browser_bridge.replay_coordinator();
        let replay = replay_coordinator
            .active_state(&workspace_key)
            .map(|active| {
                let repair_apply_ready = active
                    .repair
                    .as_ref()
                    .and_then(|repair| {
                        replay_coordinator
                            .exact_repair(&workspace_key, active.instance.id(), repair.repair_id)
                            .ok()
                    })
                    .and_then(|repair| replay_coordinator.locator_repair_apply_ready(&repair).ok())
                    .unwrap_or(false);
                let selecting_replacement = self
                    .browser_replay_repair_selection
                    .as_ref()
                    .is_some_and(|selection| {
                        selection.workspace_key == workspace_key
                            && selection.instance_id == active.instance.id()
                            && active.repair.as_ref().is_some_and(|repair| {
                                selection.repair_id == repair.repair_id
                                    && selection.tab_id == repair.tab_id
                                    && selection.revision == repair.revision
                            })
                    });
                BrowserReplayPaneProjection {
                    replay: active.projection,
                    repair: active.repair,
                    selecting_replacement,
                    repair_apply_ready,
                }
            });
        Some(BrowserPaneModel::new(
            workspace_key,
            &self.browser_pane_context(),
            &snapshot,
            BrowserPaneTransient {
                address_draft: ui.address_draft,
                address_cursor: ui.address_cursor,
                address_focused: ui.address_focused,
                loading: ui.loading,
                diagnostic,
                action_status: ui.action_status,
                divider_dragging: self.browser_divider_drag.is_some(),
                annotation_mode: ui.annotation_mode,
                annotation_draft: ui.annotation_draft,
                annotation_comment: ui.annotation_comment,
                annotation_cursor: ui.annotation_cursor,
                annotation_focused: ui.annotation_focused,
                workflow_review,
                workflow_preview: ui.workflow_preview,
                workflow_editor: ui.workflow_editor,
                replay_secret_prompt,
                replay,
            },
        ))
    }

    fn active_browser_workspace(&self) -> Option<(BrowserWorkspaceKey, BrowserWorkspaceSnapshot)> {
        let tab = self.state.active_tab()?;
        if !matches!(tab.tab_type, TabType::Claude | TabType::Codex) {
            return None;
        }
        Some((
            BrowserWorkspaceKey::new(tab.project_id.clone(), tab.id.clone()).ok()?,
            tab.browser_workspace.clone().unwrap_or_default(),
        ))
    }

    fn pending_annotation_source_for_tab(
        &self,
        tab: Option<&SessionTab>,
    ) -> Option<(
        BrowserWorkspaceKey,
        BrowserWorkspaceSnapshot,
        Vec<BrowserAnnotation>,
    )> {
        let Some(tab) = tab.filter(|tab| matches!(tab.tab_type, TabType::Claude | TabType::Codex))
        else {
            return None;
        };
        let Ok(workspace_key) = BrowserWorkspaceKey::new(tab.project_id.clone(), tab.id.clone())
        else {
            return None;
        };
        let snapshot = tab.browser_workspace.clone().unwrap_or_default();
        let pending_annotations = if self.remote_mode.is_some() {
            snapshot
                .pending_annotation_ids
                .iter()
                .filter_map(|id| {
                    snapshot
                        .annotations
                        .iter()
                        .find(|annotation| &annotation.id == id)
                        .cloned()
                })
                .collect::<Vec<_>>()
        } else {
            self.process_manager
                .browser_attachment_broker()
                .projection(&workspace_key)
                .pending_annotations
        };
        Some((workspace_key, snapshot, pending_annotations))
    }

    fn pending_annotation_chip_models_for_tab(
        &self,
        tab: Option<&SessionTab>,
    ) -> Vec<view::PendingAnnotationChipModel> {
        let tab_type = tab.map(|tab| &tab.tab_type);
        let Some((workspace_key, snapshot, pending_annotations)) =
            self.pending_annotation_source_for_tab(tab)
        else {
            return Vec::new();
        };
        view::pending_annotation_chip_models(
            tab_type,
            &workspace_key,
            &snapshot,
            &pending_annotations,
        )
    }

    fn show_pending_annotation_action_failure(
        &mut self,
        workspace_key: &BrowserWorkspaceKey,
        failure: PendingAnnotationActionFailure,
        cx: &mut Context<Self>,
    ) {
        let notice = PendingAnnotationActionNotice::new(
            workspace_key.clone(),
            failure,
            self.remote_mode.is_some(),
            Instant::now(),
        );
        let message = failure.notice();
        self.pending_annotation_action_notice = Some(notice.clone());
        self.browser_ui
            .entry(workspace_key.clone())
            .or_default()
            .diagnostic = Some(message.to_string());
        let background_executor = cx.background_executor().clone();
        cx.spawn(
            move |this: gpui::WeakEntity<Self>, cx: &mut gpui::AsyncApp| {
                let mut async_cx = cx.clone();
                async move {
                    background_executor
                        .timer(PENDING_ANNOTATION_ACTION_NOTICE_DURATION)
                        .await;
                    let _ = this.update(&mut async_cx, |this, cx| {
                        if this.pending_annotation_action_notice.as_ref() == Some(&notice) {
                            this.pending_annotation_action_notice = None;
                            cx.notify();
                        }
                    });
                }
            },
        )
        .detach();
    }

    fn remove_pending_annotation_action(
        &mut self,
        action: view::PendingAnnotationAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.remote_mode.is_some() {
            self.show_pending_annotation_action_failure(
                &action.workspace_key,
                PendingAnnotationActionFailure::RemoteRemove,
                cx,
            );
            cx.notify();
            return;
        }
        let Some((active_workspace_key, _, _)) =
            self.pending_annotation_source_for_tab(self.state.active_tab())
        else {
            self.terminal_notice =
                Some("Select the matching Claude or Codex conversation first.".to_string());
            cx.notify();
            return;
        };
        let broker = self.process_manager.browser_attachment_broker();
        let result = {
            let mut sink = NativeShellBrowserAttachmentProjectionSink {
                shell: self,
                window,
            };
            remove_pending_annotation_projection_transaction(
                &broker,
                &active_workspace_key,
                &action,
                &mut sink,
            )
        };
        match result {
            Ok(BrowserAttachmentProjectionTransaction::Applied)
            | Ok(BrowserAttachmentProjectionTransaction::NewerProjectionRemainsDirty) => {
                clear_pending_annotation_action_notice(
                    &mut self.pending_annotation_action_notice,
                    &active_workspace_key,
                );
                let ui = self
                    .browser_ui
                    .entry(active_workspace_key.clone())
                    .or_default();
                ui.action_status = Some("Removed annotation from the next prompt".to_string());
                ui.diagnostic = None;
            }
            Ok(BrowserAttachmentProjectionTransaction::PersistFailed) => {
                self.show_pending_annotation_action_failure(
                    &active_workspace_key,
                    PendingAnnotationActionFailure::RemovePersistence,
                    cx,
                );
            }
            Err(BrowserError::MissingAnnotation { .. }) => {
                self.show_pending_annotation_action_failure(
                    &active_workspace_key,
                    PendingAnnotationActionFailure::MissingAnnotation,
                    cx,
                );
            }
            Err(_) => {
                self.show_pending_annotation_action_failure(
                    &active_workspace_key,
                    PendingAnnotationActionFailure::RemoveFailed,
                    cx,
                );
            }
        }
        self.sync_browser_host_visibility(Some(window));
        cx.notify();
    }

    fn preview_pending_annotation_action(
        &mut self,
        action: view::PendingAnnotationAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((active_workspace_key, snapshot, pending_annotations)) =
            self.pending_annotation_source_for_tab(self.state.active_tab())
        else {
            self.terminal_notice =
                Some("Select the matching Claude or Codex conversation first.".to_string());
            cx.notify();
            return;
        };
        let plan = match browser_annotation_preview_plan(
            Some(&active_workspace_key),
            &action.workspace_key,
            Some(&snapshot),
            &pending_annotations,
            &action.annotation_id,
        ) {
            Ok(plan) => plan,
            Err(BrowserError::MissingAnnotation { .. }) => {
                self.show_pending_annotation_action_failure(
                    &active_workspace_key,
                    PendingAnnotationActionFailure::MissingAnnotation,
                    cx,
                );
                cx.notify();
                return;
            }
            Err(_) => {
                self.show_pending_annotation_action_failure(
                    &active_workspace_key,
                    PendingAnnotationActionFailure::InvalidSavedUrl,
                    cx,
                );
                cx.notify();
                return;
            }
        };

        let mut failed = false;
        for command in plan.commands {
            if self
                .dispatch_browser_command(&plan.workspace_key, command, window)
                .is_err()
            {
                failed = true;
                break;
            }
        }
        if failed {
            self.show_pending_annotation_action_failure(
                &plan.workspace_key,
                PendingAnnotationActionFailure::PreviewFailed,
                cx,
            );
        } else {
            clear_pending_annotation_action_notice(
                &mut self.pending_annotation_action_notice,
                &plan.workspace_key,
            );
            let ui = self
                .browser_ui
                .entry(plan.workspace_key.clone())
                .or_default();
            ui.action_status = Some("Opened annotation preview".to_string());
            ui.diagnostic = None;
        }
        self.sync_browser_host_visibility(Some(window));
        cx.notify();
    }

    fn capture_browser_split_bounds(&mut self, bounds: Bounds<Pixels>, cx: &mut Context<Self>) {
        let Some(bounds) = browser_bounds_from_gpui(bounds) else {
            return;
        };
        if self.browser_split_bounds != Some(bounds) {
            self.browser_split_bounds = Some(bounds);
            cx.notify();
        }
    }

    fn capture_browser_page_bounds(&mut self, bounds: Bounds<Pixels>, cx: &mut Context<Self>) {
        let Some(bounds) = browser_bounds_from_gpui(bounds) else {
            return;
        };
        if self.browser_page_bounds == Some(bounds) {
            return;
        }
        self.browser_page_bounds = Some(bounds);
        if let Err(error) = self.browser_host.set_bounds(bounds) {
            if let Some((workspace_key, _)) = self.active_browser_workspace() {
                self.browser_ui.entry(workspace_key).or_default().diagnostic =
                    Some(error.to_string());
                cx.notify();
            }
        }
    }

    fn apply_browser_pane_action(
        &mut self,
        action: BrowserPaneAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((workspace_key, snapshot)) = self.active_browser_workspace() else {
            self.terminal_notice = Some("Select a Claude or Codex conversation first.".to_string());
            cx.notify();
            return;
        };
        let workflow_surface = match self.state.active_tab().map(|tab| tab.tab_type.clone()) {
            Some(TabType::Claude) => BrowserPaneSurface::Claude,
            Some(TabType::Codex) => BrowserPaneSurface::Codex,
            Some(TabType::Server | TabType::Ssh) | None => {
                self.terminal_notice =
                    Some("Select a Claude or Codex conversation first.".to_string());
                cx.notify();
                return;
            }
        };

        if self.browser_replay_secret_prompt.is_some() && action.is_annotation_editor_action() {
            return;
        }
        if matches!(action, BrowserPaneAction::StartRecording) && self.remote_mode.is_some() {
            self.browser_ui.entry(workspace_key).or_default().diagnostic =
                Some("Browser workflow recording is unavailable from a remote client.".to_string());
            cx.notify();
            return;
        }

        let address_draft = self
            .browser_ui
            .get(&workspace_key)
            .and_then(|ui| ui.address_draft.clone())
            .unwrap_or_default();
        let plan = match validate_browser_pane_action_before_replay_interrupt(
            &workspace_key,
            &snapshot,
            &address_draft,
            &action,
            || {
                self.browser_bridge.interrupt_workspace(&workspace_key);
                self.retire_browser_replay_ui_after_interrupt(&workspace_key);
            },
        ) {
            Ok(plan) => plan,
            Err(error) => {
                self.browser_ui.entry(workspace_key).or_default().diagnostic =
                    Some(error.to_string());
                cx.notify();
                return;
            }
        };

        match action {
            BrowserPaneAction::FocusReplaySecret {
                workspace_key: action_workspace,
                instance_id,
                input_name,
            } => {
                let result = if action_workspace == workspace_key {
                    self.focus_browser_replay_secret_prompt(
                        &action_workspace,
                        instance_id,
                        &input_name,
                        window,
                        cx,
                    )
                } else {
                    Err(BrowserReplaySecretError::StaleAuthority)
                };
                if let Err(error) = result {
                    self.browser_ui.entry(workspace_key).or_default().diagnostic =
                        Some(error.to_string());
                    cx.notify();
                }
                return;
            }
            BrowserPaneAction::SubmitReplaySecrets {
                workspace_key: action_workspace,
                instance_id,
            } => {
                let result = if action_workspace == workspace_key {
                    self.submit_browser_replay_secret_prompt(&action_workspace, instance_id, cx)
                } else {
                    Err(BrowserReplaySecretError::StaleAuthority)
                };
                let ui = self.browser_ui.entry(workspace_key).or_default();
                match result {
                    Ok(_) => {
                        ui.diagnostic = None;
                        ui.action_status = Some("Submitted replay secrets".to_string());
                    }
                    Err(error) => ui.diagnostic = Some(error.to_string()),
                }
                self.sync_browser_host_visibility(Some(window));
                cx.notify();
                return;
            }
            BrowserPaneAction::CancelReplaySecrets {
                workspace_key: action_workspace,
                instance_id,
            } => {
                let result = if action_workspace == workspace_key {
                    self.cancel_browser_replay_secret_prompt(&action_workspace, instance_id, cx)
                } else {
                    Err(BrowserReplaySecretError::StaleAuthority)
                };
                let ui = self.browser_ui.entry(workspace_key).or_default();
                match result {
                    Ok(_) => {
                        ui.diagnostic = None;
                        ui.action_status = Some("Cancelled replay secret prompt".to_string());
                    }
                    Err(error) => ui.diagnostic = Some(error.to_string()),
                }
                self.sync_browser_host_visibility(Some(window));
                cx.notify();
                return;
            }
            BrowserPaneAction::CancelReplay { instance_id } => {
                let coordinator = self.browser_bridge.replay_coordinator();
                let result = coordinator
                    .exact_instance(&workspace_key, instance_id)
                    .and_then(|instance| coordinator.cancel(&instance));
                let cancelled = result.is_ok();
                let ui = self.browser_ui.entry(workspace_key.clone()).or_default();
                match result {
                    Ok(_) => {
                        ui.diagnostic = None;
                        ui.action_status = Some("Cancelled browser replay".to_string());
                    }
                    Err(error) => ui.diagnostic = Some(error.to_string()),
                }
                if cancelled {
                    self.reconcile_browser_replay_state(window, cx);
                }
                self.sync_browser_host_visibility(Some(window));
                cx.notify();
                return;
            }
            BrowserPaneAction::BeginReplayRepairSelection {
                instance_id,
                repair_id,
            } => {
                let coordinator = self.browser_bridge.replay_coordinator();
                let result = (|| {
                    let active = coordinator.active_state(&workspace_key).ok_or_else(|| {
                        BrowserError::InvalidInvocation {
                            field: "replayInstanceId".to_string(),
                        }
                    })?;
                    if active.instance.id() != instance_id
                        || active.projection.status != BrowserReplayStatus::PausedLocatorRepair
                    {
                        return Err(BrowserError::InvalidInvocation {
                            field: "replayInstanceId".to_string(),
                        });
                    }
                    let _repair_instance = coordinator
                        .exact_repair(&workspace_key, instance_id, repair_id)
                        .map_err(|_| BrowserError::InvalidInvocation {
                            field: "repairId".to_string(),
                        })?;
                    let repair = active
                        .repair
                        .ok_or_else(|| BrowserError::InvalidInvocation {
                            field: "repairId".to_string(),
                        })?;
                    if repair.repair_id != repair_id {
                        return Err(BrowserError::InvalidInvocation {
                            field: "repairId".to_string(),
                        });
                    }
                    let live = self
                        .browser_host
                        .workspace_snapshot(&workspace_key)
                        .ok_or_else(|| BrowserError::CrashedView {
                            message: "repair page is unavailable".to_string(),
                        })?;
                    if live.revision != repair.revision
                        || live.selected_tab_id.as_deref() != Some(repair.tab_id.as_str())
                    {
                        return Err(BrowserError::StaleReference {
                            expected: repair.revision,
                            actual: live.revision,
                        });
                    }
                    self.cancel_browser_replay_repair_selection(Some(window));
                    self.dispatch_browser_command(
                        &workspace_key,
                        BrowserCommand::SetAnnotationMode {
                            tab_id: repair.tab_id.clone(),
                            enabled: true,
                        },
                        window,
                    )?;
                    self.browser_replay_repair_selection = Some(BrowserReplayRepairSelection {
                        workspace_key: workspace_key.clone(),
                        instance_id,
                        repair_id,
                        tab_id: repair.tab_id,
                        revision: repair.revision,
                    });
                    Ok(())
                })();
                let ui = self.browser_ui.entry(workspace_key).or_default();
                match result {
                    Ok(()) => {
                        ui.annotation_mode = true;
                        ui.diagnostic = None;
                        ui.action_status = Some("Select a replacement element".to_string());
                    }
                    Err(error) => ui.diagnostic = Some(error.to_string()),
                }
                self.sync_browser_host_visibility(Some(window));
                cx.notify();
                return;
            }
            BrowserPaneAction::ApplyReplayRepair {
                instance_id,
                repair_id,
                resume,
            } => {
                let coordinator = self.browser_bridge.replay_coordinator();
                let repair = match coordinator.exact_repair(&workspace_key, instance_id, repair_id)
                {
                    Ok(repair) => repair,
                    Err(error) => {
                        self.browser_ui.entry(workspace_key).or_default().diagnostic =
                            Some(error.to_string());
                        cx.notify();
                        return;
                    }
                };
                let context = match BrowserInvocationContext::user(
                    if resume {
                        "save replay repair and retry the failed step"
                    } else {
                        "save replay repair"
                    },
                    BrowserRisk::Normal,
                ) {
                    Ok(context) => context,
                    Err(error) => {
                        self.browser_ui.entry(workspace_key).or_default().diagnostic =
                            Some(error.to_string());
                        cx.notify();
                        return;
                    }
                };
                self.cancel_browser_replay_repair_selection(Some(window));
                let controller = self
                    .browser_bridge
                    .bind(workspace_key.clone(), Duration::from_secs(300));
                let this = cx.weak_entity();
                let result_coordinator = coordinator.clone();
                let result_workspace = workspace_key.clone();
                window
                    .spawn(cx, async move |cx| {
                        let result = controller
                            .request_replay_repair_apply(
                                &coordinator,
                                &repair,
                                true,
                                resume,
                                context,
                            )
                            .await;
                        let _ = this.update_in(&mut *cx, |shell, window, cx| {
                            let still_exact = result_coordinator
                                .active_state(&result_workspace)
                                .is_some_and(|active| active.instance.id() == instance_id);
                            if !still_exact {
                                return;
                            }
                            let ui = shell
                                .browser_ui
                                .entry(result_workspace.clone())
                                .or_default();
                            match result {
                                Ok(_) => {
                                    ui.diagnostic = None;
                                    ui.action_status = Some(if resume {
                                        "Saved repair and resumed replay".to_string()
                                    } else {
                                        "Saved replay repair".to_string()
                                    });
                                }
                                Err(error) => ui.diagnostic = Some(error.to_string()),
                            }
                            shell.sync_browser_host_visibility(Some(window));
                            cx.notify();
                        });
                    })
                    .detach();
                self.browser_ui
                    .entry(workspace_key)
                    .or_default()
                    .action_status = Some(
                    if resume {
                        "Saving repair and retrying..."
                    } else {
                        "Saving repair..."
                    }
                    .to_string(),
                );
                cx.notify();
                return;
            }
            BrowserPaneAction::DividerBegin { .. } => {
                self.browser_divider_drag = Some(BrowserDividerDrag { workspace_key });
                self.sync_browser_host_visibility(Some(window));
                cx.notify();
                return;
            }
            BrowserPaneAction::DividerUpdate { pointer_x } => {
                if let (Some(drag), Some(bounds)) = (
                    self.browser_divider_drag.as_ref(),
                    self.browser_split_bounds,
                ) {
                    let right = bounds.x.saturating_add(bounds.width);
                    let pane_width = (right as f32 - pointer_x).clamp(0.0, bounds.width as f32);
                    let percent = if bounds.width > 0 {
                        ((pane_width / bounds.width as f32) * 100.0).round() as u8
                    } else {
                        50
                    };
                    let tab_id = drag.workspace_key.ai_tab_id.clone();
                    self.state.update_browser_workspace(&tab_id, |snapshot| {
                        snapshot.set_split_percent(percent);
                    });
                    cx.notify();
                }
                return;
            }
            BrowserPaneAction::DividerEnd => {
                if self.browser_divider_drag.take().is_some() {
                    self.save_session_state();
                    self.sync_browser_host_visibility(Some(window));
                    cx.notify();
                }
                return;
            }
            BrowserPaneAction::FocusAddress => {
                let selected_url = snapshot
                    .selected_tab_id
                    .as_deref()
                    .and_then(|selected| snapshot.tabs.iter().find(|tab| tab.id == selected))
                    .map(|tab| tab.url.clone())
                    .unwrap_or_default();
                let ui = self.browser_ui.entry(workspace_key).or_default();
                let draft = ui.address_draft.get_or_insert(selected_url);
                ui.address_cursor = draft.chars().count();
                ui.address_focused = true;
                window.focus(&self.browser_address_focus);
                cx.notify();
                return;
            }
            BrowserPaneAction::EditAddress(value) => {
                let ui = self.browser_ui.entry(workspace_key).or_default();
                ui.address_cursor = value.chars().count();
                ui.address_draft = Some(value);
                ui.address_focused = true;
                cx.notify();
                return;
            }
            BrowserPaneAction::FocusAnnotation => {
                let ui = self.browser_ui.entry(workspace_key).or_default();
                if ui.annotation_draft.is_some() {
                    ui.annotation_cursor = ui
                        .annotation_cursor
                        .min(ui.annotation_comment.chars().count());
                    ui.annotation_focused = true;
                    window.focus(&self.browser_annotation_focus);
                    cx.notify();
                }
                return;
            }
            BrowserPaneAction::ToggleAnnotation => {
                if self
                    .browser_replay_repair_selection
                    .as_ref()
                    .is_some_and(|selection| selection.workspace_key == workspace_key)
                {
                    self.cancel_browser_replay_repair_selection(Some(window));
                    self.browser_ui
                        .entry(workspace_key)
                        .or_default()
                        .action_status = Some("Cancelled replacement selection".to_string());
                    cx.notify();
                    return;
                }
                let enabled = !self
                    .browser_ui
                    .get(&workspace_key)
                    .is_some_and(|ui| ui.annotation_mode);
                let Some(tab_id) = snapshot
                    .selected_tab_id
                    .clone()
                    .or_else(|| snapshot.tabs.first().map(|tab| tab.id.clone()))
                else {
                    self.browser_ui.entry(workspace_key).or_default().diagnostic =
                        Some("Browser workspace has no selected tab.".to_string());
                    cx.notify();
                    return;
                };
                match self.dispatch_browser_command(
                    &workspace_key,
                    BrowserCommand::SetAnnotationMode { tab_id, enabled },
                    window,
                ) {
                    Ok(_) => {
                        let ui = self.browser_ui.entry(workspace_key).or_default();
                        ui.annotation_mode = enabled;
                        if !enabled {
                            ui.annotation_draft = None;
                            ui.annotation_comment.clear();
                            ui.annotation_cursor = 0;
                            ui.annotation_focused = false;
                        }
                        ui.diagnostic = None;
                    }
                    Err(error) => {
                        self.browser_ui.entry(workspace_key).or_default().diagnostic =
                            Some(error.to_string());
                    }
                }
                cx.notify();
                return;
            }
            BrowserPaneAction::SaveAnnotation => {
                let Some((draft_id, comment)) =
                    self.browser_ui.get(&workspace_key).and_then(|ui| {
                        ui.annotation_draft
                            .as_ref()
                            .map(|draft| (draft.id.clone(), ui.annotation_comment.clone()))
                    })
                else {
                    return;
                };
                if comment.trim().is_empty() {
                    self.browser_ui.entry(workspace_key).or_default().diagnostic =
                        Some("Annotation comment cannot be blank.".to_string());
                    cx.notify();
                    return;
                }
                match self.dispatch_browser_command(
                    &workspace_key,
                    BrowserCommand::SaveAnnotationDraft { draft_id, comment },
                    window,
                ) {
                    Ok(_) => {
                        let ui = self.browser_ui.entry(workspace_key).or_default();
                        ui.annotation_draft = None;
                        ui.annotation_comment.clear();
                        ui.annotation_cursor = 0;
                        ui.annotation_focused = false;
                        ui.action_status = Some("Saved browser annotation".to_string());
                    }
                    Err(error) => {
                        self.browser_ui.entry(workspace_key).or_default().diagnostic =
                            Some(error.to_string());
                    }
                }
                cx.notify();
                return;
            }
            BrowserPaneAction::CancelAnnotation => {
                let Some(draft_id) = self
                    .browser_ui
                    .get(&workspace_key)
                    .and_then(|ui| ui.annotation_draft.as_ref())
                    .map(|draft| draft.id.clone())
                else {
                    return;
                };
                match self.dispatch_browser_command(
                    &workspace_key,
                    BrowserCommand::CancelAnnotationDraft { draft_id },
                    window,
                ) {
                    Ok(_) => {
                        let ui = self.browser_ui.entry(workspace_key).or_default();
                        ui.annotation_draft = None;
                        ui.annotation_comment.clear();
                        ui.annotation_cursor = 0;
                        ui.annotation_focused = false;
                        ui.action_status = Some("Canceled browser annotation".to_string());
                    }
                    Err(error) => {
                        self.browser_ui.entry(workspace_key).or_default().diagnostic =
                            Some(error.to_string());
                    }
                }
                cx.notify();
                return;
            }
            BrowserPaneAction::StartRecording => {
                if self
                    .browser_host
                    .workspace_snapshot(&workspace_key)
                    .is_none()
                    && self
                        .dispatch_browser_command(
                            &workspace_key,
                            BrowserCommand::Ensure {
                                snapshot: snapshot.clone(),
                            },
                            window,
                        )
                        .is_err()
                {
                    self.browser_ui.entry(workspace_key).or_default().diagnostic =
                        Some("Browser workflow recording could not start.".to_string());
                    cx.notify();
                    return;
                }
                let result = self.with_browser_host_control_barrier(window, |browser_host| {
                    browser_host.start_page_recording(&workspace_key)
                });
                let ui = self.browser_ui.entry(workspace_key).or_default();
                match result {
                    Ok(_) => {
                        ui.diagnostic = None;
                        ui.workflow_preview = None;
                        ui.workflow_editor = None;
                        ui.action_status = Some("Recording browser workflow".to_string());
                    }
                    Err(_) => {
                        ui.diagnostic =
                            Some("Browser workflow recording could not start.".to_string());
                    }
                }
                self.sync_browser_host_visibility(Some(window));
                cx.notify();
                return;
            }
            BrowserPaneAction::StopRecording { instance_id } => {
                let instance = self.browser_host.page_recording_instance(&workspace_key);
                let result = match instance {
                    Some(instance) if instance.id() == instance_id => self
                        .with_browser_host_control_barrier(window, |browser_host| {
                            browser_host.stop_page_recording(&instance)
                        })
                        .map(|_| ()),
                    _ => Err(crate::browser::BrowserPageRecordingIpcError::Untrusted),
                };
                let ui = self.browser_ui.entry(workspace_key).or_default();
                match result {
                    Ok(()) => {
                        ui.diagnostic = None;
                        ui.workflow_preview = None;
                        ui.workflow_editor = None;
                        ui.action_status = Some("Recording stopped - review ready".to_string());
                    }
                    Err(_) => {
                        ui.diagnostic =
                            Some("Browser workflow recording is no longer active.".to_string());
                    }
                }
                self.sync_browser_host_visibility(Some(window));
                cx.notify();
                return;
            }
            BrowserPaneAction::FocusRecordingReviewField { instance_id, field } => {
                let result = self
                    .browser_host
                    .workflow_review_projection(&workspace_key, workflow_surface)
                    .ok_or(crate::browser::BrowserRecordingError::StaleInstance)
                    .and_then(|projection| {
                        browser_workflow_review_editor_for_field(&projection, instance_id, field)
                    });
                match result {
                    Ok(editor) => {
                        let ui = self.browser_ui.entry(workspace_key).or_default();
                        ui.diagnostic = None;
                        ui.workflow_editor = Some(editor);
                        window.focus(&self.browser_workflow_focus);
                    }
                    Err(_) => {
                        self.browser_ui.entry(workspace_key).or_default().diagnostic =
                            Some("Workflow review field cannot be edited.".to_string());
                    }
                }
                cx.notify();
                return;
            }
            BrowserPaneAction::CancelRecordingReviewEdit => {
                if let Some(ui) = self.browser_ui.get_mut(&workspace_key) {
                    ui.workflow_editor = None;
                }
                cx.notify();
                return;
            }
            BrowserPaneAction::MutateRecordingReview {
                instance_id,
                mutation,
            } => {
                let result = self.with_browser_host_control_barrier(window, |browser_host| {
                    browser_host.apply_workflow_review_mutation(
                        Some(&workspace_key),
                        &workspace_key,
                        workflow_surface,
                        instance_id,
                        mutation,
                    )
                });
                let ui = self.browser_ui.entry(workspace_key).or_default();
                match result {
                    Ok(_) => {
                        ui.diagnostic = None;
                        ui.workflow_preview = None;
                        ui.workflow_editor = None;
                        ui.action_status = Some("Updated workflow review".to_string());
                    }
                    Err(_) => {
                        ui.diagnostic = Some("Workflow review change is invalid.".to_string());
                    }
                }
                cx.notify();
                return;
            }
            BrowserPaneAction::PreviewRecordingReview { instance_id } => {
                let result = self.with_browser_host_control_barrier(window, |browser_host| {
                    browser_host.preview_workflow_review(
                        Some(&workspace_key),
                        &workspace_key,
                        workflow_surface,
                        instance_id,
                    )
                });
                let ui = self.browser_ui.entry(workspace_key).or_default();
                match result.and_then(|recipe| {
                    serde_json::to_string_pretty(&recipe)
                        .map(|mut preview| {
                            preview.push('\n');
                            preview
                        })
                        .map_err(|_| BrowserError::InvalidRecipe {
                            message: "validated preview could not be rendered".to_string(),
                        })
                }) {
                    Ok(preview) => {
                        ui.diagnostic = None;
                        ui.workflow_preview = Some(preview);
                        ui.action_status = Some("Validated workflow preview".to_string());
                    }
                    Err(_) => {
                        ui.diagnostic =
                            Some("Workflow review must be valid before preview.".to_string());
                    }
                }
                cx.notify();
                return;
            }
            BrowserPaneAction::SaveRecordingReview { instance_id } => {
                let remote_client = self.remote_mode.is_some();
                let project_root =
                    local_browser_workflow_project_root(&self.state, &workspace_key, remote_client);
                let result = project_root.and_then(|project_root| {
                    self.with_browser_host_control_barrier(window, |browser_host| {
                        browser_host.save_workflow_review(
                            Some(&workspace_key),
                            &workspace_key,
                            workflow_surface,
                            instance_id,
                            &project_root,
                            remote_client,
                        )
                    })
                });
                let ui = self.browser_ui.entry(workspace_key).or_default();
                match result {
                    Ok(_) => {
                        ui.diagnostic = None;
                        ui.workflow_preview = None;
                        ui.workflow_editor = None;
                        ui.action_status = Some("Saved browser workflow".to_string());
                    }
                    Err(_) => {
                        ui.diagnostic = Some(
                            "Browser workflow could not be saved; review was retained.".to_string(),
                        );
                    }
                }
                self.sync_browser_host_visibility(Some(window));
                cx.notify();
                return;
            }
            BrowserPaneAction::DiscardRecordingReview { instance_id } => {
                let result = self.with_browser_host_control_barrier(window, |browser_host| {
                    browser_host.discard_workflow_review(
                        Some(&workspace_key),
                        &workspace_key,
                        workflow_surface,
                        instance_id,
                    )
                });
                let ui = self.browser_ui.entry(workspace_key).or_default();
                match result {
                    Ok(()) => {
                        ui.diagnostic = None;
                        ui.workflow_preview = None;
                        ui.workflow_editor = None;
                        ui.action_status = Some("Discarded workflow review".to_string());
                    }
                    Err(_) => {
                        ui.diagnostic = Some("Workflow review is no longer active.".to_string());
                    }
                }
                self.sync_browser_host_visibility(Some(window));
                cx.notify();
                return;
            }
            _ => {}
        }

        if let Some(diagnostic) = plan.diagnostic {
            self.browser_ui
                .entry(plan.workspace_key)
                .or_default()
                .diagnostic = Some(diagnostic);
            cx.notify();
            return;
        }

        let mut failed = None;
        for command in plan.commands {
            match self.dispatch_browser_command(&plan.workspace_key, command, window) {
                Ok(BrowserResponse::DownloadDirectory { path }) => {
                    if let Err(error) = platform_service::open_path(&path) {
                        failed = Some(error);
                        break;
                    }
                }
                Ok(_) => {}
                Err(error) => {
                    failed = Some(error.to_string());
                    break;
                }
            }
        }
        if let Some(message) = failed {
            self.browser_ui
                .entry(plan.workspace_key.clone())
                .or_default()
                .diagnostic = Some(message);
            if let Some(pane_open) = browser_pane_open_fallback(&action) {
                self.state
                    .update_browser_workspace(&plan.workspace_key.ai_tab_id, |snapshot| {
                        snapshot.pane_open = pane_open;
                    });
                self.save_session_state();
            }
        } else {
            let ui = self.browser_ui.entry(plan.workspace_key).or_default();
            ui.action_status = Some(match action {
                BrowserPaneAction::Stop => "Stopped browser activity".to_string(),
                BrowserPaneAction::OpenDownloads => "Opened downloads".to_string(),
                _ => "Browser updated".to_string(),
            });
            if matches!(action, BrowserPaneAction::SubmitAddress) {
                ui.address_focused = false;
                ui.address_draft = None;
            }
        }
        self.sync_browser_host_visibility(Some(window));
        cx.notify();
    }

    fn handle_browser_replay_secret_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(projection) = self
            .browser_replay_secret_prompt
            .as_ref()
            .map(BrowserReplaySecretPromptVault::projection)
        else {
            return;
        };
        let workspace_key = projection.workspace_key;
        let instance_id = projection.instance_id;
        let Some(input_name) = projection.focused_input else {
            window.prevent_default();
            return;
        };
        let key = event.keystroke.key.to_ascii_lowercase();
        let result = match key.as_str() {
            "escape" => self.cancel_browser_replay_secret_prompt(&workspace_key, instance_id, cx),
            "enter" => self.submit_browser_replay_secret_prompt(&workspace_key, instance_id, cx),
            "backspace" => self.backspace_browser_replay_secret_prompt(
                &workspace_key,
                instance_id,
                &input_name,
                cx,
            ),
            "tab" => {
                let current = projection
                    .input_names
                    .iter()
                    .position(|name| name == &input_name)
                    .unwrap_or_default();
                let next = if event.keystroke.modifiers.shift {
                    current
                        .checked_sub(1)
                        .unwrap_or_else(|| projection.input_names.len().saturating_sub(1))
                } else {
                    (current + 1) % projection.input_names.len()
                };
                self.focus_browser_replay_secret_prompt(
                    &workspace_key,
                    instance_id,
                    &projection.input_names[next],
                    window,
                    cx,
                )
            }
            _ if event.keystroke.modifiers.control
                || event.keystroke.modifiers.platform
                || event.keystroke.modifiers.alt =>
            {
                Ok(BrowserReplaySecretPromptEvent {
                    workspace_key: workspace_key.clone(),
                    instance_id,
                    operation: crate::browser::BrowserReplaySecretPromptOperation::Focused,
                    input_name: Some(input_name.clone()),
                    focused_input: Some(input_name.clone()),
                    is_set: None,
                })
            }
            _ => match event.keystroke.key_char.as_deref() {
                Some(text) if !text.is_empty() => self.edit_browser_replay_secret_prompt(
                    &workspace_key,
                    instance_id,
                    &input_name,
                    text,
                    cx,
                ),
                _ => Ok(BrowserReplaySecretPromptEvent {
                    workspace_key: workspace_key.clone(),
                    instance_id,
                    operation: crate::browser::BrowserReplaySecretPromptOperation::Focused,
                    input_name: Some(input_name.clone()),
                    focused_input: Some(input_name),
                    is_set: None,
                }),
            },
        };
        if let Err(error) = result {
            self.browser_ui.entry(workspace_key).or_default().diagnostic = Some(error.to_string());
            cx.notify();
        }
        window.prevent_default();
    }

    fn handle_browser_address_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((workspace_key, _)) = self.active_browser_workspace() else {
            return;
        };
        let key = event.keystroke.key.to_ascii_lowercase();
        if key == "enter" {
            self.apply_browser_pane_action(BrowserPaneAction::SubmitAddress, window, cx);
            window.prevent_default();
            return;
        }
        if key == "escape" {
            if let Some(ui) = self.browser_ui.get_mut(&workspace_key) {
                ui.address_focused = false;
                ui.address_draft = None;
            }
            cx.notify();
            window.prevent_default();
            return;
        }
        let secondary = event.keystroke.modifiers.control || event.keystroke.modifiers.platform;
        let paste_text = if secondary && key == "v" {
            cx.read_from_clipboard().and_then(|item| item.text())
        } else {
            None
        };
        let ui = self.browser_ui.entry(workspace_key).or_default();
        let draft = ui.address_draft.get_or_insert_default();
        let mut selection = None;
        if apply_text_key_to_string(
            draft,
            &mut ui.address_cursor,
            &mut selection,
            event,
            paste_text.as_deref(),
            false,
            false,
        ) {
            ui.address_focused = true;
            cx.notify();
            window.prevent_default();
        }
    }

    fn handle_browser_annotation_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.browser_replay_secret_prompt.is_some() {
            window.prevent_default();
            return;
        }
        let Some((workspace_key, _)) = self.active_browser_workspace() else {
            return;
        };
        if self
            .browser_ui
            .get(&workspace_key)
            .and_then(|ui| ui.annotation_draft.as_ref())
            .is_none()
        {
            return;
        }
        let key = event.keystroke.key.to_ascii_lowercase();
        if key == "escape" {
            self.apply_browser_pane_action(BrowserPaneAction::CancelAnnotation, window, cx);
            window.prevent_default();
            return;
        }
        let secondary = event.keystroke.modifiers.control || event.keystroke.modifiers.platform;
        if secondary && key == "enter" {
            self.apply_browser_pane_action(BrowserPaneAction::SaveAnnotation, window, cx);
            window.prevent_default();
            return;
        }
        let paste_text = if secondary && key == "v" {
            cx.read_from_clipboard().and_then(|item| item.text())
        } else {
            None
        };
        let ui = self.browser_ui.entry(workspace_key).or_default();
        let mut selection = None;
        if apply_text_key_to_string(
            &mut ui.annotation_comment,
            &mut ui.annotation_cursor,
            &mut selection,
            event,
            paste_text.as_deref(),
            false,
            true,
        ) {
            ui.annotation_focused = true;
            ui.diagnostic = None;
            cx.notify();
            window.prevent_default();
        }
    }

    fn handle_browser_workflow_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((workspace_key, _)) = self.active_browser_workspace() else {
            return;
        };
        let Some(editor) = self
            .browser_ui
            .get(&workspace_key)
            .and_then(|ui| ui.workflow_editor.clone())
        else {
            return;
        };
        let key = event.keystroke.key.to_ascii_lowercase();
        if key == "escape" {
            self.apply_browser_pane_action(
                BrowserPaneAction::CancelRecordingReviewEdit,
                window,
                cx,
            );
            window.prevent_default();
            return;
        }
        if key == "enter" {
            let surface = match self.state.active_tab().map(|tab| tab.tab_type.clone()) {
                Some(TabType::Claude) => BrowserPaneSurface::Claude,
                Some(TabType::Codex) => BrowserPaneSurface::Codex,
                Some(TabType::Server | TabType::Ssh) | None => return,
            };
            let mutation = self
                .browser_host
                .workflow_review_projection(&workspace_key, surface)
                .ok_or(crate::browser::BrowserRecordingError::StaleInstance)
                .and_then(|projection| {
                    browser_workflow_review_editor_mutation(&projection, &editor)
                });
            match mutation {
                Ok(mutation) => self.apply_browser_pane_action(
                    BrowserPaneAction::MutateRecordingReview {
                        instance_id: editor.instance_id,
                        mutation,
                    },
                    window,
                    cx,
                ),
                Err(_) => {
                    self.browser_ui.entry(workspace_key).or_default().diagnostic =
                        Some("Workflow review is no longer active.".to_string());
                    cx.notify();
                }
            }
            window.prevent_default();
            return;
        }

        let secondary = event.keystroke.modifiers.control || event.keystroke.modifiers.platform;
        let paste_text = if secondary && key == "v" {
            cx.read_from_clipboard().and_then(|item| item.text())
        } else {
            None
        };
        let ui = self.browser_ui.entry(workspace_key).or_default();
        let Some(editor) = ui.workflow_editor.as_mut() else {
            return;
        };
        let mut selection = None;
        if apply_text_key_to_string(
            &mut editor.draft,
            &mut editor.cursor,
            &mut selection,
            event,
            paste_text.as_deref(),
            false,
            false,
        ) {
            editor.focused = true;
            ui.diagnostic = None;
            cx.notify();
            window.prevent_default();
        }
    }

    fn apply_browser_settings_action(
        &mut self,
        action: BrowserSettingsAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.state.settings().browser_enabled {
            self.editor_notice = Some(
                "Enable per-conversation Browser before using browser data actions.".to_string(),
            );
            cx.notify();
            return;
        }
        let status = self.browser_host.status();
        if !status.available {
            self.editor_notice = Some(status.diagnostic.unwrap_or_else(|| {
                "WebView2 is unavailable; browser data actions cannot run.".to_string()
            }));
            cx.notify();
            return;
        }
        let Some((active_workspace, _)) = self.active_browser_workspace() else {
            self.editor_notice =
                Some("Select an active Claude or Codex conversation first.".to_string());
            cx.notify();
            return;
        };
        let open_workspaces = self.open_browser_workspace_keys();
        let plan = match browser_settings_plan(
            action.clone(),
            Some(&active_workspace),
            &open_workspaces,
        ) {
            Ok(plan) => plan,
            Err(error) => {
                self.editor_notice = Some(error.to_string());
                cx.notify();
                return;
            }
        };
        match self.dispatch_browser_command(&plan.route_key, plan.command, window) {
            Ok(BrowserResponse::DownloadDirectory { path }) => {
                self.editor_notice = Some(match platform_service::open_path(&path) {
                    Ok(()) => "Opened active project browser downloads.".to_string(),
                    Err(error) => format!("Could not reveal browser downloads: {error}"),
                });
            }
            Ok(_) => {
                self.editor_notice = Some(match action {
                    BrowserSettingsAction::ClearActiveProjectProfile => {
                        "Cleared the active project browser profile. Downloads and captured resources were retained."
                            .to_string()
                    }
                    BrowserSettingsAction::ResetActiveConversation => {
                        "Reset the active conversation browser workspace.".to_string()
                    }
                    BrowserSettingsAction::RevealActiveDownloads => {
                        "Opened active project browser downloads.".to_string()
                    }
                });
            }
            Err(error) => {
                self.editor_notice = Some(format!("Browser operation failed: {error}"));
            }
        }
        self.sync_browser_host_visibility(Some(window));
        cx.notify();
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
                    let mut next_interval = REMOTE_CLIENT_REFRESH_INTERVAL;
                    let mut last_host_housekeeping_at =
                        Instant::now() - REMOTE_HOST_HOUSEKEEPING_INTERVAL;
                    loop {
                        background_executor.timer(next_interval).await;
                        while native_dialog_blockers.load(Ordering::Acquire) > 0 {
                            background_executor.timer(Duration::from_millis(50)).await;
                        }
                        let run_host_housekeeping = last_host_housekeeping_at.elapsed()
                            >= REMOTE_HOST_HOUSEKEEPING_INTERVAL;
                        if this
                            .update(&mut async_cx, |shell, cx: &mut Context<'_, Self>| {
                                let mut next = REMOTE_CLIENT_REFRESH_INTERVAL;
                                let mut ran_host_housekeeping = false;
                                let mut changed = if shell.remote_mode.is_some() {
                                    shell.sync_remote_client_snapshot(cx)
                                } else {
                                    next = if shell
                                        .remote_host_service
                                        .status()
                                        .any_transport_enabled()
                                        || shell.remote_host_service.has_pending_requests()
                                    {
                                        REMOTE_HOST_REQUEST_POLL_INTERVAL
                                    } else {
                                        REMOTE_HOST_IDLE_POLL_INTERVAL
                                    };
                                    let host_changed = shell.pump_remote_host_requests(cx);
                                    if run_host_housekeeping || host_changed {
                                        shell.refresh_remote_host_maintenance(cx);
                                        ran_host_housekeeping = true;
                                    }
                                    host_changed
                                };
                                changed = shell.handle_process_op_completions(cx) || changed;
                                if shell.process_monitor.is_some() {
                                    let revision = shell.process_manager.runtime_revision();
                                    if revision != shell.process_monitor_revision {
                                        shell.process_monitor_revision = revision;
                                        changed = true;
                                    }
                                    next = Duration::from_millis(500);
                                }
                                if changed {
                                    cx.notify();
                                }
                                (next, ran_host_housekeeping)
                            })
                            .map(|(next, ran_host_housekeeping)| {
                                next_interval = next;
                                if ran_host_housekeeping {
                                    last_host_housekeeping_at = Instant::now();
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

    fn save_session_state(&mut self) -> bool {
        if self.remote_mode.is_some() {
            return false;
        }
        if let Err(error) = self
            .session_manager
            .save_session(&persisted_session_state(&self.state))
        {
            self.terminal_notice = Some(format!("Failed to save session state: {error}"));
            false
        } else {
            true
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

    fn persist_known_remote_hosts(&mut self) {
        if let Err(error) = remote::save_remote_known_hosts(&self.remote_machine_state.known_hosts)
        {
            self.editor_notice = Some(format!("Failed to save remote hosts: {error}"));
        }
    }

    fn refresh_remote_host_config_from_service(&mut self) {
        self.remote_machine_state.host = self.remote_host_service.config();
        self.last_remote_host_config_revision = self.remote_host_service.config_revision();
        self.sync_settings_remote_draft();
    }

    fn sync_remote_host_config_from_service(&mut self) {
        let latest_revision = self.remote_host_service.config_revision();
        if latest_revision == self.last_remote_host_config_revision {
            return;
        }
        self.refresh_remote_host_config_from_service();
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

    fn ensure_local_host_mutation_control(&mut self) -> bool {
        if self.local_host_has_control() {
            return true;
        }
        self.remote_host_service.take_local_control();
        self.editor_notice = Some("This machine controls the host again.".to_string());
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

    fn remote_terminal_control_model(&self) -> Option<view::TerminalRemoteControlModel> {
        let remote_mode = self.remote_mode.as_ref()?;
        if remote_mode.reconnect.is_some() {
            return Some(view::TerminalRemoteControlModel {
                label: "Reconnecting".to_string(),
                color: theme::WARNING_TEXT,
                can_take: false,
                can_release: false,
            });
        }
        if remote_mode.snapshot.you_have_control {
            Some(view::TerminalRemoteControlModel {
                label: "You control".to_string(),
                color: theme::SUCCESS_TEXT,
                can_take: false,
                can_release: true,
            })
        } else {
            Some(view::TerminalRemoteControlModel {
                label: "Watching only".to_string(),
                color: theme::WARNING_TEXT,
                can_take: true,
                can_release: false,
            })
        }
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

    fn spawn_remote_request(
        &mut self,
        action: RemoteAction,
        cx: &mut Context<Self>,
        on_complete: impl FnOnce(&mut Self, Result<RemoteActionResult, String>, &mut Context<Self>)
            + Send
            + 'static,
    ) -> bool {
        let Some(remote_mode) = self.remote_mode.as_ref() else {
            return false;
        };
        if remote_mode.reconnect.is_some() {
            on_complete(self, Err("Reconnecting to remote host...".to_string()), cx);
            return true;
        }
        let client = remote_mode.client.clone();
        cx.spawn(
            async move |this: gpui::WeakEntity<Self>, cx: &mut gpui::AsyncApp| {
                let result = cx
                    .background_executor()
                    .spawn(async move { client.request(action) })
                    .await;
                let _ = this.update(cx, |this, cx| {
                    on_complete(this, result, cx);
                });
            },
        )
        .detach();
        true
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
            draft.remote_host_clients = remote_status.connected_native_clients;
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
            draft.remote_web_enabled = self.remote_machine_state.host.web.enabled;
            draft.remote_web_pairing_token =
                self.remote_machine_state.host.web.pairing_token.clone();
            draft.remote_web_listener_url = Some(self.remote_machine_state.host.web.display_url());
            draft.remote_web_listener_error = remote_status.web_listener_error;
            draft.remote_web_paired_clients =
                self.remote_machine_state.host.web.paired_clients.len();
            draft.remote_web_paired_clients_detail =
                self.remote_machine_state.host.web.paired_clients.clone();
            draft.remote_access_activity_log =
                self.remote_machine_state.host.web.activity_log.clone();

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
        if !remote_status.any_transport_enabled() {
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

        let app_revision = self.state.revision();
        let runtime_revision = self.process_manager.runtime_revision();
        let port_hash = local_stable_hash(&self.server_port_snapshot.statuses);
        let forced_sync = self.last_remote_snapshot_sync_at.is_none();
        let app_changed = forced_sync || app_revision != self.last_remote_app_revision;
        let runtime_changed = forced_sync || runtime_revision != self.last_remote_runtime_revision;
        let port_changed = forced_sync || port_hash != self.last_remote_port_hash;
        if !forced_sync
            && !app_changed
            && !runtime_changed
            && !port_changed
            && !has_pending_requests
        {
            self.last_remote_snapshot_sync_at = Some(now);
            return;
        }

        self.remote_host_service.update_snapshot_parts(
            app_changed.then(|| remote_shared_app_state(&self.state)),
            runtime_changed.then(|| runtime_state.clone()),
            port_changed.then(|| self.server_port_snapshot.statuses.clone()),
        );
        self.last_remote_app_revision = app_revision;
        self.last_remote_runtime_revision = runtime_revision;
        self.last_remote_port_hash = port_hash;
        self.last_remote_snapshot_sync_at = Some(now);
    }

    fn refresh_remote_host_maintenance(&mut self, cx: &mut Context<Self>) {
        let runtime_state = self.process_manager.runtime_state();
        self.sync_server_port_snapshot(&runtime_state, cx);
        self.sync_remote_host_live_sessions(&runtime_state);
        self.sync_remote_host_snapshot_if_due(&runtime_state);
    }

    fn sync_remote_host_live_sessions(&mut self, runtime_state: &RuntimeState) {
        if self.remote_mode.is_some() {
            return;
        }

        let remote_status = self.remote_host_service.status();
        if !remote_status.any_transport_enabled() || remote_status.connected_clients == 0 {
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
        let local_server_id = self.remote_machine_state.host.server_id.trim();
        self.remote_machine_state
            .known_hosts
            .iter()
            .filter(|host| {
                local_server_id.is_empty()
                    || host.server_id.trim().is_empty()
                    || host.server_id.trim() != local_server_id
            })
            .max_by_key(|host| host.last_connected_epoch_ms.unwrap_or(0))
            .cloned()
    }

    fn status_bar_remote_tab(&self) -> RemoteTopTab {
        RemoteTopTab::Connect
    }

    fn ensure_remote_settings_open_with_tab(&mut self, tab: RemoteTopTab, cx: &mut Context<Self>) {
        if let Some(EditorPanel::Settings(draft)) = self.editor_panel.as_mut() {
            if draft.remote_focus_only {
                draft.remote_active_tab = tab;
                self.sync_settings_remote_draft();
                cx.notify();
                return;
            }
        }
        self.open_settings_panel(true, cx);
        if let Some(EditorPanel::Settings(draft)) = self.editor_panel.as_mut() {
            draft.remote_active_tab = tab;
        }
        self.sync_settings_remote_draft();
        cx.notify();
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

    fn toggle_local_status_bar_hosting(&mut self, cx: &mut Context<Self>) {
        if !self.ensure_local_host_mutation_control() {
            return;
        }
        let config = self.remote_host_service.config();
        if let Err(error) = self.remote_host_service.update_native_listener_settings(
            !config.enabled,
            config.bind_address,
            config.port,
        ) {
            let message = format!("Could not update desktop hosting: {error}");
            self.editor_notice = Some(message.clone());
            self.set_remote_status_notice(message, true);
        }
        self.refresh_remote_host_config_from_service();
        cx.notify();
    }

    fn toggle_local_status_bar_web_hosting(&mut self, cx: &mut Context<Self>) {
        if !self.ensure_local_host_mutation_control() {
            return;
        }
        let config = self.remote_host_service.config();
        if let Err(error) = self.remote_host_service.update_web_listener_settings(
            !config.web.enabled,
            config.web.bind_address,
            config.web.port,
        ) {
            let message = format!("Could not update browser access: {error}");
            self.editor_notice = Some(message.clone());
            self.set_remote_status_notice(message, true);
        }
        self.refresh_remote_host_config_from_service();
        cx.notify();
    }

    fn apply_native_listener_draft(&mut self, toggle_enabled: bool) -> Result<(), String> {
        let (enabled, bind_address, port) = match self.editor_panel.as_ref() {
            Some(EditorPanel::Settings(draft)) => (
                if toggle_enabled {
                    !draft.remote_host_enabled
                } else {
                    draft.remote_host_enabled
                },
                draft.remote_bind_address.clone(),
                draft.remote_port.clone(),
            ),
            _ => return Err("Remote settings are not open".to_string()),
        };
        let bind_address = normalize_remote_bind_address(&bind_address)?;
        let port = parse_required_remote_port(&port, "Desktop port")?;
        self.remote_host_service
            .update_native_listener_settings(enabled, bind_address, port)?;
        self.refresh_remote_host_config_from_service();
        Ok(())
    }

    fn apply_browser_listener_draft(&mut self, toggle_enabled: bool) -> Result<(), String> {
        let (enabled, bind_address, port) = match self.editor_panel.as_ref() {
            Some(EditorPanel::Settings(draft)) => (
                if toggle_enabled {
                    !draft.remote_web_enabled
                } else {
                    draft.remote_web_enabled
                },
                draft.remote_web_bind_address.clone(),
                draft.remote_web_port.clone(),
            ),
            _ => return Err("Remote settings are not open".to_string()),
        };
        let bind_address = normalize_remote_bind_address(&bind_address)?;
        let port = parse_required_remote_port(&port, "Browser port")?;
        self.remote_host_service
            .update_web_listener_settings(enabled, bind_address, port)?;
        self.refresh_remote_host_config_from_service();
        Ok(())
    }

    fn copy_remote_pairing_token_action(&mut self, cx: &mut Context<Self>) {
        let token = self.remote_host_service.status().pairing_token;
        if token.trim().is_empty() {
            self.editor_notice =
                Some("Generate or enable hosting before copying a desktop pair token.".to_string());
            self.set_remote_status_notice(
                "Generate or enable hosting before copying a desktop pair token.",
                true,
            );
        } else {
            cx.write_to_clipboard(ClipboardItem::new_string(token));
            self.editor_notice = Some("Copied desktop pair token to the clipboard.".to_string());
            self.set_remote_status_notice("Copied desktop pair token to the clipboard.", false);
        }
        self.sync_settings_remote_draft();
        cx.notify();
    }

    fn copy_remote_web_invite_link_action(&mut self, cx: &mut Context<Self>) {
        let status = self.remote_host_service.status();
        let web = self.remote_host_service.config().web;
        let url = web.display_url();
        let token = web.pairing_token.clone();
        if !web.enabled {
            self.editor_notice =
                Some("Enable browser access before copying an invite link.".to_string());
            self.set_remote_status_notice(
                "Enable browser access before copying an invite link.",
                true,
            );
        } else if let Some(error) = status.web_listener_error {
            self.editor_notice = Some(error.clone());
            self.set_remote_status_notice(&error, true);
        } else if token.trim().is_empty() {
            self.editor_notice =
                Some("Generate a browser pair token before copying an invite link.".to_string());
            self.set_remote_status_notice(
                "Generate a browser pair token before copying an invite link.",
                true,
            );
        } else {
            let invite = format!("{url}/pair?t={token}");
            cx.write_to_clipboard(ClipboardItem::new_string(invite));
            self.editor_notice = Some("Copied browser invite link to the clipboard.".to_string());
            self.set_remote_status_notice("Copied browser invite link to the clipboard.", false);
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
        self.remote_status_bar_state().model
    }

    fn remote_status_bar_state(&self) -> RemoteStatusBarState {
        let host_status = self.remote_host_service.status();
        let preferred_host = self.preferred_known_remote_host();
        let remote_connection =
            self.remote_mode
                .as_ref()
                .map(|remote_mode| RemoteStatusBarConnectionSnapshot {
                    connected_label: remote_mode.connected_label.clone(),
                    has_control: remote_mode.snapshot.you_have_control,
                    reconnecting: remote_mode.reconnect.is_some(),
                });
        build_remote_status_bar_state(
            remote_connection.as_ref(),
            &host_status,
            preferred_host.as_ref(),
            self.local_host_has_control(),
            self.remote_status_notice
                .as_ref()
                .is_some_and(|notice| notice.is_error),
        )
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
        self.interrupt_all_browser_replays_before_shutdown();
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
        self.persist_known_remote_hosts();
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
        self.interrupt_all_browser_replays_before_shutdown();
        self.remote_connect_request_id = self.remote_connect_request_id.saturating_add(1);
        if let Some(remote_mode) = self.remote_mode.take() {
            remote_mode.port_forwards.shutdown();
            self.remote_client_pool.remove(&remote_mode.pool_key);
            remote_mode.client.disconnect();
        }
        if let Some(mut local_state) = self.local_state_backup.take() {
            let changed = reconcile_restored_browser_attachment_state(
                &mut local_state,
                &self.process_manager.browser_attachment_broker(),
            );
            self.state = local_state;
            if changed {
                self.save_session_state();
            }
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
        let _ = dimensions;
        // When another remote client controls the host, this desktop becomes
        // a passive viewer. Rebuilding a fresh `TerminalReplica` from the
        // entire replay buffer on every dirty generation is especially costly
        // for Claude/Codex full-screen TUIs and can stall or crash the native
        // window while selecting those tabs. Viewer mode should mirror the
        // host's exact current screen, not synthesize a locally-resized copy.
        self.process_manager.session_view(session_id)
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
        let active_spec = self.state.active_terminal_spec();
        self.process_manager
            .session_view(&active_spec.session_id)
            .or_else(|| self.process_manager.active_session())
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

    fn ai_tab_session_needs_restore(&self, tab: &crate::models::SessionTab) -> bool {
        let Some(session_id) = tab.pty_session_id.as_deref() else {
            return true;
        };
        let runtime = self.process_manager.runtime_state();
        let session_runtime = runtime.sessions.get(session_id).cloned();
        let session_attached = self.process_manager.session_attached(session_id);

        ai_session_needs_restore(session_runtime.as_ref(), session_attached, Instant::now())
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

        let Some(work_permit) = self.remote_host_service.try_acquire_work_permit() else {
            if let Some(response) = response {
                let _ = response.send(RemoteActionResult::error(
                    "Remote host Git work is busy. Retry shortly.",
                ));
            }
            return true;
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
                        work_permit.run(|| match action {
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
                        })
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

    fn handle_process_op_completions(&mut self, cx: &mut Context<Self>) -> bool {
        let completions = self.process_manager.drain_process_op_completions();
        if completions.is_empty() {
            return false;
        }

        let mut did_change = false;
        for completion in completions {
            if let Some(response) = completion.remote_response {
                let mut remote_result = match completion.result {
                    Ok(()) => RemoteActionResult::ok(completion.context.message.clone(), None),
                    Err(ref error) => RemoteActionResult::error(error.clone()),
                };
                if completion.result.is_ok() {
                    if let Some(session_id) = completion.context.session_id.as_deref() {
                        match completion.kind {
                            ProcessOpKind::SpawnAi | ProcessOpKind::RestartAi => {
                                remote_result.payload = remote_ai_tab_payload_for_remote_response(
                                    &self.state,
                                    &self.process_manager,
                                    session_id,
                                    Instant::now(),
                                );
                            }
                            _ => {}
                        }
                    }
                }
                let _ = response.send(remote_result);
            }

            if completion.result.is_ok() {
                match completion.kind {
                    ProcessOpKind::StopServer | ProcessOpKind::StopAll => {
                        if let Some(command_id) = completion.context.session_id.as_deref() {
                            if let Some(state) = self.active_port_state.as_mut() {
                                if state.command_id == command_id {
                                    state.status = None;
                                    state.last_checked_at = None;
                                    state.refresh_in_flight = false;
                                }
                            }
                            self.terminal_notice = Some(format!(
                                "Stopped `{command_id}` and released its processes."
                            ));
                        } else if matches!(completion.kind, ProcessOpKind::StopAll) {
                            self.terminal_notice =
                                Some("Stopped all running server tabs.".to_string());
                        }
                    }
                    ProcessOpKind::StartServer
                    | ProcessOpKind::RestartServer
                    | ProcessOpKind::KillPortAndRestart => {
                        if let Some(command_id) = completion.context.session_id.as_deref() {
                            if completion.context.focus {
                                self.synced_session_id = Some(command_id.to_string());
                            }
                            self.terminal_notice = None;
                            self.terminal_actionable_notice = None;
                            if completion.kind == ProcessOpKind::KillPortAndRestart {
                                if let Some(port) = completion.context.port {
                                    self.record_port_kill_feedback(
                                        command_id,
                                        port,
                                        PortKillFeedback::Killed,
                                    );
                                }
                            }
                        }
                    }
                    ProcessOpKind::StartSsh | ProcessOpKind::RestartSsh => {
                        if let Some(session_id) = completion.context.session_id.as_deref() {
                            self.synced_session_id = Some(session_id.to_string());
                            self.last_dimensions = None;
                            self.terminal_notice = None;
                        }
                    }
                    ProcessOpKind::Shutdown => {
                        if shutdown_completion_is_current(
                            self.pending_shutdown_op_id,
                            completion.op_id,
                        ) {
                            self.pending_window_close = false;
                            self.pending_shutdown_op_id = None;
                            self.terminal_actionable_notice = None;
                            let termination = if self.pending_install_update.take().is_some() {
                                PendingAppTermination::ExitAfterUpdate
                            } else {
                                PendingAppTermination::Quit
                            };
                            self.request_app_termination(termination, cx);
                        }
                    }
                    ProcessOpKind::KillProcess | ProcessOpKind::KillProcessTree => {
                        if let Some(message) = completion.context.message.clone() {
                            self.terminal_notice = Some(message);
                        }
                    }
                    _ => {}
                }
                self.save_session_state();
                did_change = true;
            } else if let Err(error) = completion.result {
                match completion.kind {
                    ProcessOpKind::StartServer
                    | ProcessOpKind::RestartServer
                    | ProcessOpKind::KillPortAndRestart => {
                        self.terminal_notice =
                            Some(format!("Failed to run server action: {error}"));
                        if completion.kind == ProcessOpKind::KillPortAndRestart {
                            if let (Some(command_id), Some(port)) = (
                                completion.context.session_id.as_deref(),
                                completion.context.port,
                            ) {
                                self.record_port_kill_feedback(
                                    command_id,
                                    port,
                                    PortKillFeedback::Error,
                                );
                            }
                        }
                    }
                    ProcessOpKind::StopServer => {
                        self.terminal_notice = Some(format!(
                            "Failed to stop cleanly. The port may still be in use. ({error})"
                        ));
                    }
                    ProcessOpKind::StartSsh | ProcessOpKind::RestartSsh => {
                        self.terminal_notice =
                            Some(format!("Failed to run SSH session action: {error}"));
                    }
                    ProcessOpKind::SpawnAi | ProcessOpKind::RestartAi => {
                        self.terminal_notice =
                            Some(format!("Failed to run AI session action: {error}"));
                    }
                    ProcessOpKind::Shutdown => {
                        match shutdown_failure_disposition(
                            self.pending_shutdown_op_id,
                            completion.op_id,
                            self.pending_app_termination,
                        ) {
                            ShutdownFailureDisposition::IgnoreStale => continue,
                            ShutdownFailureDisposition::PreservePendingTermination => {
                                self.pending_window_close = false;
                                self.pending_shutdown_op_id = None;
                                self.terminal_actionable_notice = None;
                                self.terminal_notice = Some(format!(
                                    "Shutdown did not complete cleanly: {error}. Waiting for browser initialization to stop before exiting..."
                                ));
                            }
                            ShutdownFailureDisposition::ResumeInteractiveShutdown => {
                                let message = format!("Shutdown did not complete cleanly: {error}");
                                self.terminal_notice = Some(message.clone());
                                self.terminal_actionable_notice =
                                    Some(ActionableNotice::ForceQuit { message });
                                self.pending_window_close = false;
                                self.pending_shutdown_op_id = None;
                                self.resume_browser_window_after_canceled_shutdown();
                            }
                        }
                    }
                    _ => {
                        self.terminal_notice = Some(error);
                    }
                }
                did_change = true;
            }
        }

        if did_change {
            self.last_remote_snapshot_sync_at = None;
        }
        did_change
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

            let mut defer_response_send = false;
            let result = match action {
                RemoteAction::StartServer {
                    command_id,
                    focus,
                    dimensions,
                } => {
                    if let Err(error) = self
                        .process_manager
                        .validate_server_launch(&self.state, &command_id)
                    {
                        RemoteActionResult::error(error)
                    } else {
                        if focus {
                            self.interrupt_active_browser_replay_before_route_change(None);
                        }
                        let result = self.process_manager.start_server_with_remote_response(
                            &mut self.state,
                            &command_id,
                            dimensions,
                            focus,
                            response.clone(),
                        );
                        match result {
                            Ok(()) => {
                                did_change = true;
                                self.save_session_state();
                                defer_response_send = response.is_some();
                                RemoteActionResult::ok(None, None)
                            }
                            Err(error) => RemoteActionResult::error(error),
                        }
                    }
                }
                RemoteAction::StopServer { command_id } => {
                    match self.process_manager.enqueue_stop_server_and_wait(
                        &command_id,
                        Duration::ZERO,
                        response.clone(),
                    ) {
                        Ok(()) => {
                            did_change = true;
                            self.save_session_state();
                            defer_response_send = response.is_some();
                            RemoteActionResult::ok(None, None)
                        }
                        Err(error) => RemoteActionResult::error(error),
                    }
                }
                RemoteAction::RestartServer {
                    command_id,
                    dimensions,
                } => {
                    if let Err(error) = self
                        .process_manager
                        .validate_server_launch(&self.state, &command_id)
                    {
                        RemoteActionResult::error(error)
                    } else {
                        self.interrupt_active_browser_replay_before_route_change(None);
                        match self.process_manager.restart_server_with_remote_response(
                            &mut self.state,
                            &command_id,
                            dimensions,
                            "--- Restarting... ---",
                            response.clone(),
                        ) {
                            Ok(()) => {
                                did_change = true;
                                self.save_session_state();
                                defer_response_send = response.is_some();
                                RemoteActionResult::ok(None, None)
                            }
                            Err(error) => RemoteActionResult::error(error),
                        }
                    }
                }
                RemoteAction::LaunchAi {
                    project_id,
                    tab_type,
                    dimensions,
                } => {
                    let start_result = self
                        .process_manager
                        .start_ai_session_activate_with_response(
                            &mut self.state,
                            &project_id,
                            tab_type,
                            dimensions,
                            false,
                            response.clone(),
                        );
                    match start_result {
                        Ok(session_id) => {
                            did_change = true;
                            self.save_session_state();
                            defer_response_send = response.is_some();
                            let payload = remote_ai_tab_payload_for_remote_response(
                                &self.state,
                                &self.process_manager,
                                &session_id,
                                Instant::now(),
                            );
                            RemoteActionResult::ok(Some(format!("Opened {session_id}")), payload)
                        }
                        Err(error) => RemoteActionResult::error(error),
                    }
                }
                RemoteAction::OpenAiTab { tab_id, dimensions } => {
                    match self
                        .process_manager
                        .ensure_ai_session_for_tab_with_response(
                            &mut self.state,
                            &tab_id,
                            dimensions,
                            false,
                            false,
                            response.clone(),
                        ) {
                        Ok(session_id) => {
                            did_change = true;
                            self.save_session_state();
                            defer_response_send = response.is_some();
                            RemoteActionResult::ok(
                                None,
                                remote_ai_tab_payload_for_remote_response(
                                    &self.state,
                                    &self.process_manager,
                                    &session_id,
                                    Instant::now(),
                                ),
                            )
                        }
                        Err(error) => RemoteActionResult::error(error),
                    }
                }
                RemoteAction::RestartAiTab { tab_id, dimensions } => {
                    if let Err(error) = self
                        .process_manager
                        .validate_ai_restart(&self.state, &tab_id)
                    {
                        RemoteActionResult::error(error)
                    } else {
                        let workspace_key =
                            self.state.find_ai_tab(&tab_id).cloned().and_then(|tab| {
                                self.state
                                    .find_project(&tab.project_id)
                                    .and_then(|_| browser_workspace_key_for_ai_tab(Some(&tab)))
                            });
                        if let Some(workspace_key) = workspace_key.as_ref() {
                            self.interrupt_browser_workspace_before_teardown(workspace_key);
                        }
                        match self
                            .process_manager
                            .restart_ai_session_activate_with_response(
                                &mut self.state,
                                &tab_id,
                                dimensions,
                                false,
                                response.clone(),
                            ) {
                            Ok(session_id) => {
                                did_change = true;
                                self.save_session_state();
                                defer_response_send = response.is_some();
                                RemoteActionResult::ok(
                                    None,
                                    remote_ai_tab_payload_for_remote_response(
                                        &self.state,
                                        &self.process_manager,
                                        &session_id,
                                        Instant::now(),
                                    ),
                                )
                            }
                            Err(error) => RemoteActionResult::error(error),
                        }
                    }
                }
                RemoteAction::CloseAiTab { tab_id } => {
                    let workspace_key =
                        browser_workspace_key_for_ai_tab(self.state.find_ai_tab(&tab_id));
                    if let Some(workspace_key) = workspace_key.as_ref() {
                        self.interrupt_browser_workspace_before_teardown(workspace_key);
                    }
                    match self.process_manager.close_ai_session_with_response(
                        &mut self.state,
                        &tab_id,
                        response.clone(),
                    ) {
                        Ok(()) => {
                            did_change = true;
                            self.save_session_state();
                            defer_response_send = response.is_some();
                            RemoteActionResult::ok(None, None)
                        }
                        Err(error) => RemoteActionResult::error(error),
                    }
                }
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
                        self.interrupt_active_browser_replay_before_route_change(None);
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
                } => {
                    if self.state.find_ssh_connection(&connection_id).is_some() {
                        self.interrupt_active_browser_replay_before_route_change(None);
                    }
                    match self.process_manager.start_ssh_session_with_response(
                        &mut self.state,
                        &connection_id,
                        dimensions,
                        response.clone(),
                    ) {
                        Ok(_) => {
                            did_change = true;
                            self.save_session_state();
                            defer_response_send = response.is_some();
                            RemoteActionResult::ok(None, None)
                        }
                        Err(error) => RemoteActionResult::error(error),
                    }
                }
                RemoteAction::RestartSsh {
                    connection_id,
                    dimensions,
                } => {
                    let connection_exists =
                        self.state.find_ssh_connection(&connection_id).is_some();
                    if connection_exists {
                        self.interrupt_active_browser_replay_before_route_change(None);
                    }
                    if let Some(tab_id) = self
                        .state
                        .find_ssh_tab_by_connection(&connection_id)
                        .map(|tab| tab.id.clone())
                    {
                        match self.process_manager.restart_ssh_session_with_response(
                            &mut self.state,
                            &tab_id,
                            dimensions,
                            response.clone(),
                        ) {
                            Ok(_) => {
                                did_change = true;
                                self.save_session_state();
                                defer_response_send = response.is_some();
                                RemoteActionResult::ok(None, None)
                            }
                            Err(error) => RemoteActionResult::error(error),
                        }
                    } else {
                        match self.process_manager.start_ssh_session_with_response(
                            &mut self.state,
                            &connection_id,
                            dimensions,
                            response.clone(),
                        ) {
                            Ok(_) => {
                                did_change = true;
                                self.save_session_state();
                                defer_response_send = response.is_some();
                                RemoteActionResult::ok(None, None)
                            }
                            Err(error) => RemoteActionResult::error(error),
                        }
                    }
                }
                RemoteAction::DisconnectSsh { connection_id } => {
                    if let Some(tab_id) = self
                        .state
                        .find_ssh_tab_by_connection(&connection_id)
                        .map(|tab| tab.id.clone())
                    {
                        match self.process_manager.close_ssh_session_with_response(
                            &mut self.state,
                            &tab_id,
                            response.clone(),
                        ) {
                            Ok(()) => {
                                did_change = true;
                                self.save_session_state();
                                defer_response_send = response.is_some();
                                RemoteActionResult::ok(None, None)
                            }
                            Err(error) => RemoteActionResult::error(error),
                        }
                    } else {
                        RemoteActionResult::ok(None, None)
                    }
                }
                RemoteAction::CloseSession { session_id } => {
                    let workspace_key = self
                        .current_runtime_snapshot()
                        .sessions
                        .get(&session_id)
                        .and_then(|session| session.tab_id.as_deref())
                        .and_then(|tab_id| {
                            browser_workspace_key_for_ai_tab(self.state.find_ai_tab(tab_id))
                        });
                    if let Some(workspace_key) = workspace_key.as_ref() {
                        self.interrupt_browser_workspace_before_teardown(workspace_key);
                    }
                    match self.process_manager.close_session(&session_id) {
                        Ok(()) => {
                            did_change = true;
                            self.save_session_state();
                            RemoteActionResult::ok(None, None)
                        }
                        Err(error) => RemoteActionResult::error(error),
                    }
                }
                RemoteAction::CloseTab { tab_id } => {
                    let workspace_key =
                        browser_workspace_key_for_ai_tab(self.state.find_ai_tab(&tab_id));
                    if let Some(workspace_key) = workspace_key.as_ref() {
                        self.interrupt_browser_workspace_before_teardown(workspace_key);
                    }
                    match self.process_manager.close_tab(&mut self.state, &tab_id) {
                        Ok(()) => {
                            did_change = true;
                            self.synced_session_id = None;
                            self.last_dimensions = None;
                            self.save_session_state();
                            RemoteActionResult::ok(None, None)
                        }
                        Err(error) => RemoteActionResult::error(error),
                    }
                }
                RemoteAction::StopAllServers => {
                    let stopped = self.process_manager.stop_all_servers();
                    if stopped > 0 {
                        did_change = true;
                        self.save_session_state();
                    }
                    let message = if stopped == 0 {
                        "No running servers to stop.".to_string()
                    } else {
                        format!("Stopping {stopped} running server tab(s).")
                    };
                    self.terminal_notice = Some(message.clone());
                    cx.notify();
                    RemoteActionResult::ok(Some(message), None)
                }
                RemoteAction::SaveProject { project } => {
                    self.state.upsert_project(project);
                    did_change = true;
                    self.save_config_state();
                    self.save_session_state();
                    RemoteActionResult::ok(None, None)
                }
                RemoteAction::DeleteProject { project_id } => {
                    if let Err(error) = validate_project_deletion(&self.state, &project_id) {
                        RemoteActionResult::error(error)
                    } else {
                        self.delete_project_action(&project_id, cx);
                        did_change = true;
                        RemoteActionResult::ok(None, None)
                    }
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
                    if connection.private_key.is_none() {
                        ProcessManager::remove_materialized_ssh_key(&connection.id);
                    }
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
                if !defer_response_send {
                    let _ = response.send(result);
                }
            }
        }

        if did_change {
            self.last_remote_snapshot_sync_at = None;
            cx.notify();
        }
        did_change
    }

    fn handle_window_should_close(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.sync_terminal_focus(None);
        self.capture_window_bounds(window);

        if should_minimize_window_on_close(
            self.state.settings().minimize_to_tray,
            self.remote_machine_state.host.keep_hosting_in_background,
        ) {
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

        let _ = self.begin_browser_window_teardown();
        self.save_session_state();
        match self.process_manager.schedule_shutdown(APP_SHUTDOWN_TIMEOUT) {
            Ok(op_id) => {
                self.pending_shutdown_op_id = Some(op_id);
                self.pending_window_close = true;
                self.terminal_notice = Some("Shutting down managed processes...".to_string());
                cx.notify();
                false
            }
            Err(error) => {
                self.resume_browser_window_after_canceled_shutdown();
                self.terminal_notice = Some(format!("Failed to start shutdown: {error}"));
                cx.notify();
                false
            }
        }
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
            self.remote_send_action(RemoteAction::StopAllServers);
            self.terminal_notice = Some("Stopping remote server tab(s)...".to_string());
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
        if let Some(workspace_key) = browser_workspace_key_for_ai_tab(Some(&tab)) {
            self.interrupt_browser_workspace_before_teardown(&workspace_key);
        }
        if self.remote_mode.is_some() {
            self.remote_send_action(RemoteAction::CloseTab {
                tab_id: tab_id.to_string(),
            });
            self.state.remove_tab(tab_id);
            self.synced_session_id = None;
            self.last_dimensions = None;
            self.terminal_notice = None;
            cx.notify();
            return;
        }

        match self.process_manager.close_tab(&mut self.state, tab_id) {
            Ok(()) => {
                self.synced_session_id = None;
                self.last_dimensions = None;
                self.save_session_state();
            }
            Err(error) => {
                self.terminal_notice = Some(format!("Failed to close tab: {error}"));
            }
        }
        cx.notify();
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
        let replace_disables_browser = matches!(mode, ConfigImportMode::Replace)
            && self.state.settings().browser_enabled
            && !config.settings.browser_enabled;
        if replace_disables_browser {
            self.interrupt_all_browser_replays_before_shutdown();
        }
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

        let removed_ai_workspaces = removed_ai_tabs
            .iter()
            .filter_map(|tab_id| {
                browser_workspace_key_for_ai_tab(self.state.find_ai_tab(tab_id.as_str()))
            })
            .collect::<Vec<_>>();
        for workspace_key in &removed_ai_workspaces {
            self.interrupt_browser_workspace_before_teardown(workspace_key);
        }

        self.state.config = config;
        self.state.mark_dirty();
        let browser_gateway_diagnostic = self.reconcile_browser_gateway();
        self.sync_browser_host_visibility(None);

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
        self.sync_browser_host_visibility(None);

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
        self.editor_notice = Some(match browser_gateway_diagnostic {
            Some(diagnostic) => format!("{mode_label}: {}\n{diagnostic}", source_path.display()),
            None => format!("{mode_label}: {}", source_path.display()),
        });
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
        match self.updater.install_update() {
            Ok(version) => {
                promote_pending_app_termination_for_update(&mut self.pending_app_termination);
                let _ = self.begin_browser_window_teardown();
                self.save_session_state();
                match self.process_manager.schedule_shutdown(APP_SHUTDOWN_TIMEOUT) {
                    Ok(op_id) => {
                        self.pending_shutdown_op_id = Some(op_id);
                        self.pending_install_update = Some(version.clone());
                        self.editor_notice = Some(format!(
                            "Installer for {version} launched. Shutting down managed processes..."
                        ));
                    }
                    Err(error) => {
                        self.resume_browser_window_after_canceled_shutdown();
                        self.editor_notice =
                            Some(format!("Failed to start shutdown before update: {error}"));
                    }
                }
                cx.notify();
            }
            Err(error) => {
                self.editor_notice = Some(error);
                cx.notify();
            }
        }
    }

    fn force_quit_action(&mut self, cx: &mut Context<Self>) {
        self.terminal_actionable_notice = None;
        let termination = if self.pending_install_update.take().is_some() {
            PendingAppTermination::ExitAfterUpdate
        } else {
            PendingAppTermination::Quit
        };
        self.request_app_termination(termination, cx);
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
        self.close_browser_replay_secret_prompt_for_route(None);
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
        let browser_status = self.browser_host.status();
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
                remote_active_tab: RemoteTopTab::Connect,
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
                browser_enabled: settings.browser_enabled,
                browser_available: browser_status.available,
                browser_diagnostic: browser_status.diagnostic,
                remote_host_enabled: self.remote_machine_state.host.enabled,
                remote_bind_address: self.remote_machine_state.host.bind_address.clone(),
                remote_port: self.remote_machine_state.host.port.to_string(),
                remote_web_bind_address: self.remote_machine_state.host.web.bind_address.clone(),
                remote_web_port: self.remote_machine_state.host.web.port.to_string(),
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
                remote_host_clients: remote_status.connected_native_clients,
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
                remote_web_enabled: self.remote_machine_state.host.web.enabled,
                remote_web_pairing_token: self.remote_machine_state.host.web.pairing_token.clone(),
                remote_web_listener_url: Some(self.remote_machine_state.host.web.display_url()),
                remote_web_listener_error: remote_status.web_listener_error,
                remote_web_paired_clients: self.remote_machine_state.host.web.paired_clients.len(),
                remote_web_paired_clients_detail: self
                    .remote_machine_state
                    .host
                    .web
                    .paired_clients
                    .clone(),
                remote_access_activity_log: self.remote_machine_state.host.web.activity_log.clone(),
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
        self.close_browser_replay_secret_prompt_for_route(None);
        self.add_project_wizard = Some(workspace::AddProjectWizard::default());
        cx.notify();
    }

    fn open_process_monitor_action(&mut self, cx: &mut Context<Self>) {
        self.close_browser_replay_secret_prompt_for_route(None);
        self.process_monitor = Some(process_monitor::ProcessMonitorState::default());
        self.process_monitor_revision = self.process_manager.runtime_revision();
        cx.notify();
    }

    fn close_process_monitor_action(&mut self, cx: &mut Context<Self>) {
        self.process_monitor = None;
        cx.notify();
    }

    fn handle_process_monitor_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.process_monitor.is_none() {
            return false;
        }
        if event.keystroke.key.to_ascii_lowercase() == "escape" {
            self.close_process_monitor_action(cx);
            window.prevent_default();
            return true;
        }
        true
    }

    fn handle_process_monitor_action(
        &mut self,
        action: process_monitor::ProcessMonitorAction,
        cx: &mut Context<Self>,
    ) {
        match action {
            process_monitor::ProcessMonitorAction::Close => {
                self.close_process_monitor_action(cx);
            }
            process_monitor::ProcessMonitorAction::ToggleSession(session_id) => {
                if let Some(monitor) = self.process_monitor.as_mut() {
                    if !monitor.expanded_sessions.insert(session_id.clone()) {
                        monitor.expanded_sessions.remove(&session_id);
                    }
                    cx.notify();
                }
            }
            process_monitor::ProcessMonitorAction::KillProcess { session_id, pid } => {
                match self
                    .process_manager
                    .enqueue_kill_process(&session_id, pid, None)
                {
                    Ok(()) => {
                        self.terminal_notice =
                            Some(format!("Killing process {pid} in `{session_id}`..."));
                    }
                    Err(error) => {
                        self.terminal_notice = Some(format!("Failed to kill process: {error}"));
                    }
                }
                cx.notify();
            }
            process_monitor::ProcessMonitorAction::KillProcessTree { session_id, pid } => {
                match self
                    .process_manager
                    .enqueue_kill_process_tree(&session_id, pid, None)
                {
                    Ok(()) => {
                        self.terminal_notice =
                            Some(format!("Killing process tree {pid} in `{session_id}`..."));
                    }
                    Err(error) => {
                        self.terminal_notice =
                            Some(format!("Failed to kill process tree: {error}"));
                    }
                }
                cx.notify();
            }
            process_monitor::ProcessMonitorAction::StopSession(session_id) => {
                self.stop_monitor_session(&session_id, cx);
            }
        }
    }

    fn stop_monitor_session(&mut self, session_id: &str, cx: &mut Context<Self>) {
        let runtime = self.process_manager.runtime_state();
        let Some(session) = runtime.sessions.get(session_id).cloned() else {
            self.terminal_notice = Some(format!("Unknown session `{session_id}`."));
            cx.notify();
            return;
        };

        let result = match session.session_kind {
            SessionKind::Server => {
                let command_id = session
                    .command_id
                    .clone()
                    .unwrap_or_else(|| session_id.to_string());
                self.process_manager.enqueue_stop_server_and_wait(
                    &command_id,
                    Duration::from_secs(5),
                    None,
                )
            }
            SessionKind::Claude | SessionKind::Codex => {
                if let Some(tab_id) = session.tab_id.as_deref() {
                    if let Some(workspace_key) =
                        browser_workspace_key_for_ai_tab(self.state.find_ai_tab(tab_id))
                    {
                        self.interrupt_browser_workspace_before_teardown(&workspace_key);
                    }
                    self.process_manager
                        .close_ai_session(&mut self.state, tab_id)
                } else {
                    self.process_manager.close_session(session_id)
                }
            }
            SessionKind::Ssh => {
                if let Some(tab_id) = session.tab_id.as_deref() {
                    self.process_manager
                        .close_ssh_session(&mut self.state, tab_id)
                } else {
                    self.process_manager.close_session(session_id)
                }
            }
            SessionKind::Shell => self.process_manager.close_session(session_id),
        };

        match result {
            Ok(()) => {
                self.terminal_notice = Some(format!("Stopping session `{session_id}`..."));
                self.save_session_state();
            }
            Err(error) => {
                self.terminal_notice = Some(format!("Failed to stop session: {error}"));
            }
        }
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

        let project_name_for_remote = project_name.clone();
        if self.spawn_remote_request(
            RemoteAction::SaveProject {
                project: project.clone(),
            },
            cx,
            move |this, result, cx| {
                match result {
                    Ok(result) if result.ok => {
                        if this.editor_notice.is_none() {
                            this.editor_notice =
                                Some(format!("Created project `{project_name_for_remote}`"));
                        }
                    }
                    Ok(result) => {
                        this.editor_notice = Some(
                            result
                                .message
                                .unwrap_or_else(|| "Could not create remote project.".to_string()),
                        );
                    }
                    Err(error) => {
                        this.editor_notice =
                            Some(format!("Could not create remote project: {error}"));
                    }
                }
                cx.notify();
            },
        ) {
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
        if self.spawn_remote_request(
            RemoteAction::BrowsePath {
                directories_only: true,
                start_path: None,
            },
            cx,
            |this, browse_result, cx| {
                let picked_path = match browse_result {
                    Ok(RemoteActionResult {
                        ok: true,
                        payload: Some(RemoteActionPayload::BrowsePath { path }),
                        ..
                    }) => path,
                    Ok(result) => {
                        this.editor_notice = Some(result.message.unwrap_or_else(|| {
                            "Could not pick a folder on the remote host.".to_string()
                        }));
                        cx.notify();
                        return;
                    }
                    Err(error) => {
                        this.editor_notice =
                            Some(format!("Could not open the remote host picker: {error}"));
                        cx.notify();
                        return;
                    }
                };
                let Some(root_path) = picked_path else {
                    return;
                };
                let default_name = last_path_segment(&root_path);
                let _ = this.spawn_remote_request(
                    RemoteAction::ScanRoot {
                        root_path: root_path.clone(),
                    },
                    cx,
                    move |this, scan_result, cx| {
                        if let Some(wizard) = this.add_project_wizard.as_mut() {
                            wizard.root_path = root_path.clone();
                            if wizard.name.trim().is_empty() {
                                wizard.name = default_name.clone();
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
                                            "Could not scan the selected remote project root."
                                                .to_string()
                                        }),
                                    );
                                }
                                Err(error) => {
                                    clear_root_scan_entries(
                                        wizard,
                                        format!(
                                            "Could not scan the selected remote project root: {error}"
                                        ),
                                    );
                                }
                            }
                        }
                        cx.notify();
                    },
                );
            },
        ) {
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
        let env_file_path = match self.editor_panel.as_ref() {
            Some(EditorPanel::Folder(draft)) => draft.env_file_path.trim().to_string(),
            _ => return,
        };
        if self.spawn_remote_request(
            RemoteAction::BrowsePath {
                directories_only: true,
                start_path: None,
            },
            cx,
            move |this, browse_result, cx| {
                let picked_path = match browse_result {
                    Ok(RemoteActionResult {
                        ok: true,
                        payload: Some(RemoteActionPayload::BrowsePath { path }),
                        ..
                    }) => path,
                    Ok(result) => {
                        this.editor_notice = Some(result.message.unwrap_or_else(|| {
                            "Could not pick a folder on the remote host.".to_string()
                        }));
                        cx.notify();
                        return;
                    }
                    Err(error) => {
                        this.editor_notice =
                            Some(format!("Could not open the remote host picker: {error}"));
                        cx.notify();
                        return;
                    }
                };
                let Some(folder_path) = picked_path else {
                    return;
                };
                let default_name = last_path_segment(&folder_path);
                let folder_path_for_env = folder_path.clone();
                let env_file_path_for_pick = env_file_path.clone();
                let apply_pick = move |this: &mut Self,
                                       env_contents: Option<String>,
                                       cx: &mut Context<Self>| {
                    if let Some(EditorPanel::Folder(draft)) = this.editor_panel.as_mut() {
                        draft.folder_path = folder_path.clone();
                        if draft.name.trim().is_empty() {
                            draft.name = default_name.clone();
                        }
                        draft.git_branch = None;
                        draft.dependency_status = None;
                        draft.scan_result = None;
                        draft.selected_scanned_scripts.clear();
                        draft.selected_scanned_port_variable = None;
                        if !env_file_path_for_pick.is_empty() {
                            draft.env_file_loaded = env_contents.is_some();
                            draft.env_file_contents = env_contents.unwrap_or_default();
                        } else {
                            draft.env_file_contents.clear();
                            draft.env_file_loaded = false;
                        }
                        draft.scan_message = Some(format!(
                            "Picked remote folder `{folder_path}`. Scan the folder to refresh scripts, ports, and repo status."
                        ));
                    }
                    cx.notify();
                };
                if env_file_path.is_empty() {
                    apply_pick(this, None, cx);
                    return;
                }
                let env_path = std::path::Path::new(&folder_path_for_env)
                    .join(&env_file_path)
                    .to_string_lossy()
                    .to_string();
                let _ = this.spawn_remote_request(
                    RemoteAction::ReadTextFile { path: env_path },
                    cx,
                    move |this, read_result, cx| {
                        let env_contents = match read_result {
                            Ok(RemoteActionResult {
                                ok: true,
                                payload: Some(RemoteActionPayload::TextFile { contents, .. }),
                                ..
                            }) => Some(contents),
                            _ => None,
                        };
                        apply_pick(this, env_contents, cx);
                    },
                );
            },
        ) {
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

        let folder_path_for_remote = folder_path.clone();
        let existing_labels_for_remote = existing_labels.clone();
        if self.spawn_remote_request(
            RemoteAction::ScanFolder {
                folder_path: folder_path_for_remote.clone(),
            },
            cx,
            move |this, result, cx| {
                let scan_result = match result {
                    Ok(result) if result.ok => match result.payload {
                        Some(RemoteActionPayload::FolderScan { scan }) => Ok(scan),
                        _ => Err("Remote host did not return a folder scan.".to_string()),
                    },
                    Ok(result) => Err(result
                        .message
                        .unwrap_or_else(|| "Remote folder scan failed.".to_string())),
                    Err(error) => Err(format!("Remote folder scan failed: {error}")),
                };
                if let Some(EditorPanel::Folder(draft)) = this.editor_panel.as_mut() {
                    draft.is_scanning = false;
                    draft.git_branch = None;
                    draft.dependency_status = None;
                    match scan_result {
                        Ok(scan) => {
                            let selected_scripts: std::collections::BTreeSet<String> =
                                scanner_service::auto_selected_script_names(&scan.scripts)
                                    .into_iter()
                                    .filter(|name| !existing_labels_for_remote.contains(name))
                                    .collect();
                            let selected_port_variable =
                                scanner_service::auto_selected_port_variable(&scan.ports);
                            if draft.env_file_path.trim().is_empty() {
                                draft.env_file_path = scanner_service::default_env_file_for_dir(
                                    std::path::Path::new(&folder_path_for_remote),
                                )
                                .unwrap_or_default();
                            }
                            if let Some(variable) = selected_port_variable.clone() {
                                draft.port_variable = variable.clone();
                            }

                            let scan_message = if scan.scripts.is_empty()
                                && !scan.has_package_json
                                && !scan.has_cargo_toml
                            {
                                "No package.json or Cargo.toml was found in this folder."
                                    .to_string()
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
            },
        ) {
            return;
        }

        let scan_result = scanner_service::scan_project(&folder_path);
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

        let full_path = std::path::Path::new(&folder_path)
            .join(&env_file_path)
            .to_string_lossy()
            .to_string();
        if self.spawn_remote_request(
            RemoteAction::ReadTextFile { path: full_path },
            cx,
            move |this, result, cx| {
                let env_contents = match result {
                    Ok(result) if result.ok => match result.payload {
                        Some(RemoteActionPayload::TextFile { contents, .. }) => Some(contents),
                        _ => None,
                    },
                    Ok(result) => {
                        this.editor_notice = Some(
                            result
                                .message
                                .unwrap_or_else(|| "Remote env load failed.".to_string()),
                        );
                        cx.notify();
                        return;
                    }
                    Err(error) => {
                        this.editor_notice = Some(format!("Remote env load failed: {error}"));
                        cx.notify();
                        return;
                    }
                };
                match env_contents {
                    Some(contents) => {
                        if let Some(EditorPanel::Folder(draft)) = this.editor_panel.as_mut() {
                            draft.env_file_contents = contents;
                            draft.env_file_loaded = true;
                            draft.scan_message = Some("Loaded env file contents.".to_string());
                        }
                    }
                    None => {
                        if let Some(EditorPanel::Folder(draft)) = this.editor_panel.as_mut() {
                            draft.env_file_contents.clear();
                            draft.env_file_loaded = true;
                            draft.scan_message = Some(
                                "Env file does not exist yet. Saving the folder will create it."
                                    .to_string(),
                            );
                        }
                    }
                }
                this.editor_active_field = Some(EditorField::Folder(FolderField::EnvContents));
                this.editor_cursor = this
                    .editor_panel
                    .as_ref()
                    .and_then(|panel| {
                        panel.text_value(EditorField::Folder(FolderField::EnvContents))
                    })
                    .map(|value| value.chars().count())
                    .unwrap_or(0);
                this.editor_needs_focus = true;
                cx.notify();
            },
        ) {
            return;
        }

        let env_contents = load_folder_env_contents(&folder_path, &env_file_path);

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
        env_service::write_env_text(
            std::path::Path::new(folder_path)
                .join(env_file_path)
                .as_path(),
            contents,
        )
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
                key_text: String::new(),
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
                    key_text: connection.private_key.unwrap_or_default(),
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
        let next_browser_enabled = draft.browser_enabled;
        if self.state.settings().browser_enabled && !next_browser_enabled {
            self.interrupt_all_browser_replays_before_shutdown();
        }
        apply_browser_enabled_preference(&mut settings, next_browser_enabled);

        self.state.update_settings(settings);
        let browser_gateway_diagnostic = self.reconcile_browser_gateway();
        self.sync_browser_host_visibility(None);
        self.process_manager
            .set_log_buffer_size(self.state.settings().log_buffer_size as usize);
        self.save_config_state();
        self.last_dimensions = None;
        self.editor_notice =
            browser_gateway_diagnostic.or_else(|| Some("Settings saved".to_string()));
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
                if self.spawn_remote_request(
                    RemoteAction::SaveProject {
                        project: project.clone(),
                    },
                    cx,
                    |this, result, cx| match result {
                        Ok(result) if result.ok => this.close_editor(cx),
                        Ok(result) => {
                            this.editor_notice = Some(
                                result
                                    .message
                                    .unwrap_or_else(|| "Could not save project".to_string()),
                            );
                            cx.notify();
                        }
                        Err(error) => {
                            this.editor_notice = Some(format!("Could not save project: {error}"));
                            cx.notify();
                        }
                    },
                ) {
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
                let project_id = draft.project_id.clone();
                let env_file_contents = (draft.env_file_loaded
                    && !draft.env_file_path.trim().is_empty())
                .then_some(draft.env_file_contents.clone());
                let save_remote_folder =
                    move |this: &mut Self, cx: &mut Context<Self>, folder: ProjectFolder| {
                        let project_id = project_id.clone();
                        let env_file_contents = env_file_contents.clone();
                        let _ = this.spawn_remote_request(
                            RemoteAction::SaveFolder {
                                project_id,
                                folder,
                                env_file_contents,
                            },
                            cx,
                            |this, result, cx| match result {
                                Ok(result) if result.ok => this.close_editor(cx),
                                Ok(result) => {
                                    this.editor_notice =
                                        Some(result.message.unwrap_or_else(|| {
                                            "Could not save folder".to_string()
                                        }));
                                    cx.notify();
                                }
                                Err(error) => {
                                    this.editor_notice =
                                        Some(format!("Could not save folder: {error}"));
                                    cx.notify();
                                }
                            },
                        );
                    };
                if self.remote_mode.is_some() {
                    if draft.env_file_loaded && !draft.env_file_path.trim().is_empty() {
                        let folder_path = draft.folder_path.trim().to_string();
                        let env_file_path = draft.env_file_path.trim().to_string();
                        let env_contents = draft.env_file_contents.clone();
                        let full_path = std::path::Path::new(&folder_path)
                            .join(&env_file_path)
                            .to_string_lossy()
                            .to_string();
                        let folder_for_save = folder.clone();
                        let _ = self.spawn_remote_request(
                            RemoteAction::WriteTextFile {
                                path: full_path,
                                contents: env_contents,
                            },
                            cx,
                            move |this, result, cx| match result {
                                Ok(result) if result.ok => {
                                    save_remote_folder(this, cx, folder_for_save);
                                }
                                Ok(result) => {
                                    this.editor_notice =
                                        Some(result.message.unwrap_or_else(|| {
                                            "Could not save env file.".to_string()
                                        }));
                                    cx.notify();
                                }
                                Err(error) => {
                                    this.editor_notice =
                                        Some(format!("Could not save env file: {error}"));
                                    cx.notify();
                                }
                            },
                        );
                    } else {
                        save_remote_folder(self, cx, folder);
                    }
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
                if self.spawn_remote_request(
                    RemoteAction::SaveCommand {
                        project_id: draft.project_id.clone(),
                        folder_id: draft.folder_id.clone(),
                        command: command.clone(),
                    },
                    cx,
                    |this, result, cx| match result {
                        Ok(result) if result.ok => this.close_editor(cx),
                        Ok(result) => {
                            this.editor_notice = Some(
                                result
                                    .message
                                    .unwrap_or_else(|| "Could not save command".to_string()),
                            );
                            cx.notify();
                        }
                        Err(error) => {
                            this.editor_notice = Some(format!("Could not save command: {error}"));
                            cx.notify();
                        }
                    },
                ) {
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
                    private_key: normalize_optional_string(&draft.key_text),
                };
                if !self.ensure_mutation_control(cx) {
                    return;
                }
                if self.spawn_remote_request(
                    RemoteAction::SaveSsh {
                        connection: connection.clone(),
                    },
                    cx,
                    |this, result, cx| match result {
                        Ok(result) if result.ok => this.close_editor(cx),
                        Ok(result) => {
                            this.editor_notice =
                                Some(result.message.unwrap_or_else(|| {
                                    "Could not save SSH connection".to_string()
                                }));
                            cx.notify();
                        }
                        Err(error) => {
                            this.editor_notice =
                                Some(format!("Could not save SSH connection: {error}"));
                            cx.notify();
                        }
                    },
                ) {
                    return;
                }
                if connection.private_key.is_none() {
                    ProcessManager::remove_materialized_ssh_key(&connection.id);
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
                if let Err(error) = validate_project_deletion(&self.state, &project_id) {
                    self.editor_notice = Some(error);
                    cx.notify();
                    return;
                }
                if self.remote_mode.is_some() {
                    if !self.ensure_remote_control(cx) {
                        return;
                    }
                    let _ = self.spawn_remote_request(
                        RemoteAction::DeleteProject {
                            project_id: project_id.clone(),
                        },
                        cx,
                        |this, result, cx| match result {
                            Ok(result) if result.ok => this.close_editor(cx),
                            Ok(result) => {
                                this.editor_notice = Some(
                                    result
                                        .message
                                        .unwrap_or_else(|| "Could not delete project".to_string()),
                                );
                                cx.notify();
                            }
                            Err(error) => {
                                this.editor_notice =
                                    Some(format!("Could not delete project: {error}"));
                                cx.notify();
                            }
                        },
                    );
                    return;
                }
                self.interrupt_browser_project_before_mutation(&project_id);
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
                    let _ = self.spawn_remote_request(
                        RemoteAction::DeleteFolder {
                            project_id: draft.project_id.clone(),
                            folder_id: folder_id.clone(),
                        },
                        cx,
                        |this, result, cx| match result {
                            Ok(result) if result.ok => this.close_editor(cx),
                            Ok(result) => {
                                this.editor_notice = Some(
                                    result
                                        .message
                                        .unwrap_or_else(|| "Could not delete folder".to_string()),
                                );
                                cx.notify();
                            }
                            Err(error) => {
                                this.editor_notice =
                                    Some(format!("Could not delete folder: {error}"));
                                cx.notify();
                            }
                        },
                    );
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
                    let _ = self.spawn_remote_request(
                        RemoteAction::DeleteCommand {
                            project_id: draft.project_id.clone(),
                            folder_id: draft.folder_id.clone(),
                            command_id: command_id.clone(),
                        },
                        cx,
                        |this, result, cx| match result {
                            Ok(result) if result.ok => this.close_editor(cx),
                            Ok(result) => {
                                this.editor_notice = Some(
                                    result
                                        .message
                                        .unwrap_or_else(|| "Could not delete command".to_string()),
                                );
                                cx.notify();
                            }
                            Err(error) => {
                                this.editor_notice =
                                    Some(format!("Could not delete command: {error}"));
                                cx.notify();
                            }
                        },
                    );
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
                    let _ = self.spawn_remote_request(
                        RemoteAction::DeleteSsh {
                            connection_id: connection_id.clone(),
                        },
                        cx,
                        |this, result, cx| match result {
                            Ok(result) if result.ok => this.close_editor(cx),
                            Ok(result) => {
                                this.editor_notice = Some(result.message.unwrap_or_else(|| {
                                    "Could not delete SSH connection".to_string()
                                }));
                                cx.notify();
                            }
                            Err(error) => {
                                this.editor_notice =
                                    Some(format!("Could not delete SSH connection: {error}"));
                                cx.notify();
                            }
                        },
                    );
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
        if let Err(error) = validate_project_deletion(&self.state, project_id) {
            self.terminal_notice = Some(error);
            cx.notify();
            return;
        }
        if !self.ensure_mutation_control(cx) {
            return;
        }
        if self.remote_mode.is_some() {
            let _ = self.spawn_remote_request(
                RemoteAction::DeleteProject {
                    project_id: project_id.to_string(),
                },
                cx,
                |this, result, cx| {
                    match result {
                        Ok(result) if result.ok => {
                            this.terminal_notice = None;
                        }
                        Ok(result) => {
                            this.terminal_notice = Some(
                                result
                                    .message
                                    .unwrap_or_else(|| "Could not delete project.".to_string()),
                            );
                        }
                        Err(error) => {
                            this.terminal_notice =
                                Some(format!("Could not delete project: {error}"));
                        }
                    }
                    cx.notify();
                },
            );
            return;
        }

        self.interrupt_browser_project_before_mutation(project_id);
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
            let _ = self.spawn_remote_request(
                RemoteAction::DeleteFolder {
                    project_id: project_id.to_string(),
                    folder_id: folder_id.to_string(),
                },
                cx,
                |this, result, cx| {
                    match result {
                        Ok(result) if result.ok => {
                            this.terminal_notice = None;
                        }
                        Ok(result) => {
                            this.terminal_notice = Some(
                                result
                                    .message
                                    .unwrap_or_else(|| "Could not delete folder.".to_string()),
                            );
                        }
                        Err(error) => {
                            this.terminal_notice =
                                Some(format!("Could not delete folder: {error}"));
                        }
                    }
                    cx.notify();
                },
            );
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
            let _ = self.spawn_remote_request(
                RemoteAction::DeleteCommand {
                    project_id,
                    folder_id,
                    command_id: command_id.clone(),
                },
                cx,
                |this, result, cx| {
                    match result {
                        Ok(result) if result.ok => {
                            this.terminal_notice = None;
                        }
                        Ok(result) => {
                            this.terminal_notice = Some(
                                result
                                    .message
                                    .unwrap_or_else(|| "Could not delete command.".to_string()),
                            );
                        }
                        Err(error) => {
                            this.terminal_notice =
                                Some(format!("Could not delete command: {error}"));
                        }
                    }
                    cx.notify();
                },
            );
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
            let _ = self.spawn_remote_request(
                RemoteAction::DeleteSsh {
                    connection_id: connection_id.to_string(),
                },
                cx,
                |this, result, cx| {
                    match result {
                        Ok(result) if result.ok => {
                            this.terminal_notice = None;
                        }
                        Ok(result) => {
                            this.terminal_notice =
                                Some(result.message.unwrap_or_else(|| {
                                    "Could not delete SSH connection.".to_string()
                                }));
                        }
                        Err(error) => {
                            this.terminal_notice =
                                Some(format!("Could not delete SSH connection: {error}"));
                        }
                    }
                    cx.notify();
                },
            );
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
        ProcessManager::remove_materialized_ssh_key(connection_id);
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

    fn drag_editor_field_to(&mut self, field: EditorField, cursor: usize, cx: &mut Context<Self>) {
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
            EditorAction::ToggleBrowserEnabled => {
                if let Some(EditorPanel::Settings(draft)) = self.editor_panel.as_mut() {
                    draft.browser_enabled = !draft.browser_enabled;
                    draft.open_picker = None;
                    let enabled = draft.browser_enabled;
                    self.apply_settings_draft(cx);
                    if enabled {
                        let status = self.browser_host.status();
                        if !status.available {
                            self.editor_notice = status.diagnostic.or_else(|| {
                                Some(
                                    "WebView2 is unavailable; the Browser companion pane cannot open."
                                        .to_string(),
                                )
                            });
                            cx.notify();
                        }
                    }
                }
            }
            EditorAction::ClearActiveBrowserProfile => self.apply_browser_settings_action(
                BrowserSettingsAction::ClearActiveProjectProfile,
                window,
                cx,
            ),
            EditorAction::ResetActiveBrowserWorkspace => self.apply_browser_settings_action(
                BrowserSettingsAction::ResetActiveConversation,
                window,
                cx,
            ),
            EditorAction::RevealActiveBrowserDownloads => self.apply_browser_settings_action(
                BrowserSettingsAction::RevealActiveDownloads,
                window,
                cx,
            ),
            EditorAction::SelectRemoteTopTab(tab) => {
                if let Some(EditorPanel::Settings(draft)) = self.editor_panel.as_mut() {
                    draft.remote_active_tab = tab;
                    cx.notify();
                }
            }
            EditorAction::ToggleRemoteHosting => {
                if !self.ensure_mutation_control(cx) {
                    return;
                }
                if let Err(error) = self.apply_native_listener_draft(true) {
                    self.editor_notice = Some(format!("Could not update desktop hosting: {error}"));
                }
                cx.notify();
            }
            EditorAction::RegenerateRemotePairingToken => {
                if !self.ensure_mutation_control(cx) {
                    return;
                }
                match self.remote_host_service.regenerate_native_pairing_token() {
                    Ok(_) => self.refresh_remote_host_config_from_service(),
                    Err(error) => {
                        self.editor_notice =
                            Some(format!("Could not generate desktop pairing token: {error}"));
                    }
                }
                cx.notify();
            }
            EditorAction::CopyRemotePairingToken => {
                self.copy_remote_pairing_token_action(cx);
            }
            EditorAction::ToggleRemoteWebHosting => {
                if !self.ensure_mutation_control(cx) {
                    return;
                }
                if let Err(error) = self.apply_browser_listener_draft(true) {
                    self.editor_notice = Some(format!("Could not update browser access: {error}"));
                }
                cx.notify();
            }
            EditorAction::ApplyRemoteWebNetworkSettings => {
                if !self.ensure_mutation_control(cx) {
                    return;
                }
                match self.apply_browser_listener_draft(false) {
                    Ok(()) => {
                        self.editor_notice = Some("Applied browser network settings.".to_string())
                    }
                    Err(error) => {
                        self.editor_notice =
                            Some(format!("Could not apply browser network settings: {error}"));
                    }
                }
                cx.notify();
            }
            EditorAction::RegenerateRemoteWebPairingToken => {
                if !self.ensure_mutation_control(cx) {
                    return;
                }
                match self.remote_host_service.regenerate_web_pairing_token() {
                    Ok(_) => self.refresh_remote_host_config_from_service(),
                    Err(error) => {
                        self.editor_notice =
                            Some(format!("Could not generate browser pairing token: {error}"));
                    }
                }
                cx.notify();
            }
            EditorAction::ResetRemoteWebAccess => {
                if !self.ensure_mutation_control(cx) {
                    return;
                }
                if self.remote_host_service.reset_browser_access() {
                    self.refresh_remote_host_config_from_service();
                    self.editor_notice =
                        Some("Reset browser access. All browsers must pair again.".to_string());
                } else {
                    self.editor_notice = Some("Could not reset browser access.".to_string());
                }
                cx.notify();
            }
            EditorAction::CopyRemoteWebPairingToken => {
                let token = self.remote_host_service.config().web.pairing_token;
                if token.trim().is_empty() {
                    self.editor_notice = Some(
                        "Enable browser access first to generate a browser pair token.".to_string(),
                    );
                    self.set_remote_status_notice(
                        "Enable browser access first to generate a browser pair token.",
                        true,
                    );
                } else {
                    cx.write_to_clipboard(ClipboardItem::new_string(token));
                    self.editor_notice =
                        Some("Copied browser pair token to the clipboard.".to_string());
                    self.set_remote_status_notice(
                        "Copied browser pair token to the clipboard.",
                        false,
                    );
                }
                self.sync_settings_remote_draft();
                cx.notify();
            }
            EditorAction::CopyRemoteWebInviteLink => {
                self.copy_remote_web_invite_link_action(cx);
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
                self.persist_known_remote_hosts();
                self.sync_settings_remote_draft();
                self.editor_notice = Some("Removed saved remote host.".to_string());
                cx.notify();
            }
            EditorAction::RevokeRemoteClient(client_id) => {
                if self.remote_host_service.revoke_paired_client(&client_id) {
                    self.refresh_remote_host_config_from_service();
                    self.editor_notice = Some("Revoked paired remote client.".to_string());
                } else {
                    self.editor_notice = Some("Could not revoke paired remote client.".to_string());
                }
                cx.notify();
            }
            EditorAction::RevokeRemoteWebClient(client_id) => {
                if self
                    .remote_host_service
                    .revoke_paired_web_client(&client_id)
                {
                    self.refresh_remote_host_config_from_service();
                    self.editor_notice = Some("Revoked paired browser.".to_string());
                } else {
                    self.editor_notice = Some("Could not revoke paired browser.".to_string());
                }
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
        if self.add_project_wizard.is_some() || self.process_monitor.is_some() {
            return;
        }
        self.focus_editor(window);
    }

    fn handle_editor_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.is_selecting_editor = false;
    }

    fn handle_wizard_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.handle_process_monitor_key(event, window, cx) {
            return true;
        }
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
        if self.browser_replay_secret_prompt.is_some() {
            window.prevent_default();
            return;
        }
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
            if let Some(panel) = self.editor_panel.as_ref() {
                if let Some(value) = panel.text_value(field) {
                    if let Some((start, end)) = selection_range(
                        self.editor_cursor,
                        self.editor_selection_anchor,
                        value.chars().count(),
                    ) {
                        let selected: String =
                            value.chars().skip(start).take(end - start).collect();
                        cx.write_to_clipboard(ClipboardItem::new_string(selected));
                    }
                }
            }
            window.prevent_default();
            return;
        }

        // Cut selected text
        if secondary && key == "x" {
            if let Some(panel) = self.editor_panel.as_ref() {
                if let Some(value) = panel.text_value(field) {
                    if let Some((start, end)) = selection_range(
                        self.editor_cursor,
                        self.editor_selection_anchor,
                        value.chars().count(),
                    ) {
                        let selected: String =
                            value.chars().skip(start).take(end - start).collect();
                        cx.write_to_clipboard(ClipboardItem::new_string(selected));
                    }
                }
            }
            if let Some(panel) = self.editor_panel.as_mut() {
                if let Some(value) = panel.text_value_mut(field) {
                    let mut chars: Vec<char> = value.chars().collect();
                    delete_selection(
                        &mut chars,
                        &mut self.editor_cursor,
                        &mut self.editor_selection_anchor,
                    );
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
        runtime_snapshot: &RuntimeState,
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
            } else {
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
            let pending_annotations =
                self.pending_annotation_chip_models_for_tab(active_tab.as_ref());
            let active_workspace_key = browser_workspace_key_for_ai_tab(active_tab.as_ref());
            let mut model = view::TerminalPaneModel {
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
                startup_notice: None,
                blocking_notice,
                actionable_notice: None,
                pending_annotations,
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
            let startup_notice = self.startup_notice.clone();
            let terminal_notice = self.terminal_notice.clone();
            refresh_terminal_pane_model_notice(
                &mut model,
                startup_notice.as_deref(),
                terminal_notice.as_deref(),
                &mut self.pending_annotation_action_notice,
                active_workspace_key.as_ref(),
                true,
                Instant::now(),
            );
            return model;
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

                let server_runtime = runtime_snapshot
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
                    let mut current_view = if local_has_resize_control {
                        self.process_manager
                            .session_view_from_runtime(runtime_snapshot, &active_spec.session_id)
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
                        current_view = self.process_manager.session_view(&active_spec.session_id);
                    }
                    self.terminal_notice = None;
                    active_session = current_view;
                } else if self.terminal_notice.is_none() {
                    self.terminal_notice = Some(
                        "Server session is not running. Start it from the sidebar.".to_string(),
                    );
                }
            }
            Some(TabType::Claude) | Some(TabType::Codex) => {
                let dimensions = self.terminal_dimensions(window);
                if let Some(active_tab) = active_tab.as_ref() {
                    let (session_runtime, session_attached) = active_tab
                        .pty_session_id
                        .as_deref()
                        .map(|session_id| {
                            (
                                runtime_snapshot.sessions.get(session_id).cloned(),
                                self.process_manager.session_attached(session_id),
                            )
                        })
                        .unwrap_or((None, false));
                    let needs_restore = ai_session_needs_restore(
                        session_runtime.as_ref(),
                        session_attached,
                        Instant::now(),
                    );

                    if needs_restore {
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

                if self.synced_session_id.as_deref() != Some(active_spec.session_id.as_str()) {
                    self.synced_session_id = Some(active_spec.session_id.clone());
                    self.last_dimensions = None;
                }

                let session_runtime = runtime_snapshot
                    .sessions
                    .get(&active_spec.session_id)
                    .cloned();
                let session_attached = self
                    .process_manager
                    .session_attached(&active_spec.session_id);
                // Selecting a busy Claude/Codex tab while the web client is
                // also attached can make `session_view()` expensive enough to
                // freeze the native window if we call it just to answer the
                // startup-guard question. Use the cheap runtime metadata here
                // and take a real snapshot only in the branch that will render
                // it.
                let passive_view_available = session_attached && session_runtime.is_some();
                let render_mode = if local_has_resize_control {
                    native_ai_render_mode(
                        session_runtime.as_ref(),
                        session_attached,
                        passive_view_available,
                        Instant::now(),
                    )
                } else {
                    NativeAiRenderMode::PassiveView
                };
                match render_mode {
                    NativeAiRenderMode::Wait => {
                        self.terminal_notice = Some(
                            "AI session is still starting in background. Wait a moment before opening it locally."
                                .to_string(),
                        );
                        active_session = None;
                    }
                    NativeAiRenderMode::PassiveView => {
                        self.terminal_notice = None;
                        active_session =
                            self.local_viewer_session_view(&active_spec.session_id, dimensions);
                    }
                    NativeAiRenderMode::ActiveControl => {
                        // Reuse the runtime snapshot we already have for the
                        // first local paint. Taking another eager
                        // `session_view()` here reintroduced the "open the AI
                        // tab locally while web is attached" hang regression.
                        let mut current_view = self
                            .process_manager
                            .session_view_from_runtime(runtime_snapshot, &active_spec.session_id);
                        if current_view.is_some() {
                            self.process_manager
                                .set_active_session(active_spec.session_id.clone());
                        }
                        if terminal_view_needs_resize(
                            self.last_dimensions,
                            current_view.as_ref(),
                            dimensions,
                        ) && self
                            .process_manager
                            .resize_session(&active_spec.session_id, dimensions)
                            .is_ok()
                        {
                            self.last_dimensions = Some(dimensions);
                            current_view =
                                self.process_manager.session_view(&active_spec.session_id);
                        }
                        self.terminal_notice = None;
                        active_session = current_view;
                    }
                }
            }
            Some(TabType::Ssh) => {
                if let Some(active_tab) = active_tab.as_ref() {
                    let session_live = active_tab
                        .pty_session_id
                        .as_deref()
                        .and_then(|session_id| runtime_snapshot.sessions.get(session_id).cloned())
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
                        let mut current_view = if local_has_resize_control {
                            self.process_manager.session_view_from_runtime(
                                runtime_snapshot,
                                &active_spec.session_id,
                            )
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
                            current_view =
                                self.process_manager.session_view(&active_spec.session_id);
                        }

                        self.terminal_notice = None;
                        active_session = current_view;
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
            _ => {
                // No tab is selected — show splash image regardless of whether
                // any tabs exist. Startup never auto-spawns a shell.
                self.ensure_splash_image(cx);
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
        let actionable_notice =
            self.terminal_actionable_notice
                .as_ref()
                .and_then(|notice| match notice {
                    ActionableNotice::PortInUse {
                        command_id,
                        message,
                        ..
                    } => {
                        if command_id.as_str() == active_spec.session_id.as_str() {
                            Some(view::TerminalActionableNotice {
                                message: message.clone(),
                                action_label: "Kill process & start server",
                                action_color: theme::DANGER_TEXT,
                            })
                        } else {
                            None
                        }
                    }
                    ActionableNotice::ForceQuit { message } => {
                        Some(view::TerminalActionableNotice {
                            message: message.clone(),
                            action_label: "Quit anyway",
                            action_color: theme::DANGER_TEXT,
                        })
                    }
                });
        let pending_annotations = self.pending_annotation_chip_models_for_tab(active_tab.as_ref());
        let active_workspace_key = browser_workspace_key_for_ai_tab(active_tab.as_ref());
        let mut model = view::TerminalPaneModel {
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
            startup_notice: None,
            blocking_notice,
            actionable_notice,
            pending_annotations,
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
        let startup_notice = self.startup_notice.clone();
        let terminal_notice = self.terminal_notice.clone();
        refresh_terminal_pane_model_notice(
            &mut model,
            startup_notice.as_deref(),
            terminal_notice.as_deref(),
            &mut self.pending_annotation_action_notice,
            active_workspace_key.as_ref(),
            false,
            Instant::now(),
        );
        model
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
        scrollbar_model_for_screen(
            &session.screen,
            self.terminal_scrollbar_drag
                .map(|drag| drag.thumb_top_ratio),
            self.state.settings().show_terminal_scrollbar,
        )
    }

    fn terminal_scrollbar_geometry(
        &self,
        window: &Window,
        session: &crate::terminal::session::TerminalSessionView,
    ) -> Option<TerminalScrollbarGeometry> {
        if !self.state.settings().show_terminal_scrollbar {
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
        let remote_control = self.remote_terminal_control_model();

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
                remote_control: remote_control.clone(),
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
                remote_control: remote_control.clone(),
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
            remote_control,
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
        let case_sensitive = self.terminal_search.case_sensitive;
        if self.spawn_remote_request(
            RemoteAction::SearchSession {
                session_id: session_id.clone(),
                query,
                case_sensitive,
            },
            cx,
            |this, result, cx| {
                this.terminal_search.matches = match result {
                    Ok(RemoteActionResult {
                        ok: true,
                        payload: Some(RemoteActionPayload::SearchMatches { matches }),
                        ..
                    }) => matches,
                    Ok(result) => {
                        this.terminal_notice = Some(result.message.unwrap_or_else(|| {
                            "Could not search the remote terminal buffer.".to_string()
                        }));
                        Vec::new()
                    }
                    Err(error) => {
                        this.terminal_notice = Some(format!(
                            "Could not search the remote terminal buffer: {error}"
                        ));
                        Vec::new()
                    }
                };
                this.terminal_search.selected_index =
                    (!this.terminal_search.matches.is_empty()).then_some(0);
                if this.terminal_search.selected_index.is_some() {
                    this.jump_to_selected_search_match();
                }
                cx.notify();
            },
        ) {
            return;
        }
        self.terminal_search.matches = self
            .process_manager
            .search_session(
                &session_id,
                &self.terminal_search.query,
                self.terminal_search.case_sensitive,
                256,
            )
            .unwrap_or_default();
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
        let export = if selection_only {
            RemoteTerminalExport::Selection {
                text: self.selected_text().unwrap_or_default(),
            }
        } else if include_scrollback {
            RemoteTerminalExport::Scrollback
        } else {
            RemoteTerminalExport::Screen
        };
        if self.spawn_remote_request(
            RemoteAction::ExportSessionText {
                session_id: session_id.clone(),
                export,
            },
            cx,
            move |this, result, cx| {
                let text = match result {
                    Ok(RemoteActionResult {
                        ok: true,
                        payload: Some(RemoteActionPayload::ExportText { text }),
                        ..
                    }) => text,
                    Ok(result) => {
                        this.terminal_notice = Some(
                            result
                                .message
                                .unwrap_or_else(|| format!("Failed to export terminal {kind}.")),
                        );
                        cx.notify();
                        return;
                    }
                    Err(error) => {
                        this.terminal_notice =
                            Some(format!("Failed to export terminal {kind}: {error}"));
                        cx.notify();
                        return;
                    }
                };

                if text.is_empty() {
                    this.terminal_notice =
                        Some("Nothing to export from this terminal.".to_string());
                    cx.notify();
                    return;
                }

                match write_terminal_export(kind, &text) {
                    Ok(path) => {
                        this.terminal_notice =
                            Some(format!("Wrote terminal {kind} to {}", path.display()));
                    }
                    Err(error) => {
                        this.terminal_notice =
                            Some(format!("Failed to export terminal {kind}: {error}"));
                    }
                }
                cx.notify();
            },
        ) {
            return;
        }
        let text = if selection_only {
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
        if let Err(error) = self
            .process_manager
            .validate_server_launch(&self.state, command_id)
        {
            self.terminal_notice = Some(format!("Failed to start server: {error}"));
            cx.notify();
            return;
        }
        if !self.ensure_mutation_control(cx) {
            return;
        }
        if self.remote_mode.is_some() {
            let dimensions = self.terminal_dimensions(window);
            self.remote_send_action(RemoteAction::StartServer {
                command_id: command_id.to_string(),
                focus: focus_started_server,
                dimensions,
            });
            if focus_started_server {
                self.select_server_tab_action(command_id, cx);
            }
            self.terminal_notice = Some(format!("Starting remote `{command_id}`..."));
            self.terminal_actionable_notice = None;
            cx.notify();
            return;
        }

        let dimensions = self.terminal_dimensions(window);
        let Some(port) = self
            .state
            .find_command(command_id)
            .and_then(|lookup| lookup.command.port)
        else {
            if focus_started_server {
                self.interrupt_active_browser_replay_before_route_change(None);
            }
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
                    self.terminal_actionable_notice = None;
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
                        if let Err(error) = this
                            .process_manager
                            .validate_server_launch(&this.state, &command_id)
                        {
                            this.terminal_notice = Some(format!("Failed to start server: {error}"));
                            cx.notify();
                            return;
                        }
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
                            let message =
                                format!("Port {port} is already in use by {owner_label}.");
                            this.terminal_notice = Some(message.clone());
                            this.terminal_actionable_notice = Some(ActionableNotice::PortInUse {
                                command_id: command_id.clone(),
                                message,
                            });
                            cx.notify();
                            return;
                        }

                        if focus_started_server {
                            this.interrupt_active_browser_replay_before_route_change(None);
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
                                this.terminal_actionable_notice = None;
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

        match self.process_manager.enqueue_stop_server_and_wait(
            &command_id,
            std::time::Duration::from_secs(5),
            None,
        ) {
            Ok(()) => {}
            Err(error) => {
                self.terminal_notice = Some(error);
            }
        }
        cx.notify();
    }

    fn restart_server_action(
        &mut self,
        command_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Err(error) = self
            .process_manager
            .validate_server_launch(&self.state, command_id)
        {
            self.terminal_notice = Some(format!("Failed to restart server: {error}"));
            cx.notify();
            return;
        }
        if !self.ensure_mutation_control(cx) {
            return;
        }
        if self.remote_mode.is_some() {
            let dimensions = self.terminal_dimensions(window);
            self.remote_send_action(RemoteAction::RestartServer {
                command_id: command_id.to_string(),
                dimensions,
            });
            self.terminal_notice = Some(format!("Restarting remote `{command_id}`..."));
            cx.notify();
            return;
        }

        self.interrupt_active_browser_replay_before_route_change(None);
        let dimensions = self.terminal_dimensions(window);
        let port = self
            .state
            .find_command(command_id)
            .and_then(|lookup| lookup.command.port);
        self.invalidate_server_port_snapshot(port);
        match self
            .process_manager
            .restart_server(&mut self.state, command_id, dimensions)
        {
            Ok(()) => {
                self.terminal_notice = Some(format!("Restarting `{command_id}`..."));
                self.save_session_state();
            }
            Err(error) => {
                self.terminal_notice = Some(format!("Failed to restart server: {error}"));
            }
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
        if let Err(error) = self
            .process_manager
            .validate_server_launch(&self.state, command_id)
        {
            self.terminal_notice = Some(format!(
                "Failed to restart server after freeing port: {error}"
            ));
            cx.notify();
            return;
        }
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

        self.interrupt_active_browser_replay_before_route_change(None);
        let _ = self.process_manager.write_virtual_text(
            command_id,
            &format!("\r\n\x1b[33m--- Resolving port {port} conflict... ---\x1b[0m\r\n"),
        );

        self.record_port_kill_feedback(command_id, port, PortKillFeedback::None);
        self.refresh_port_state(command_id.to_string(), port, cx);
        let dimensions = self.terminal_dimensions(window);
        let banner = format!("--- Starting after freeing port {port}... ---");

        match self.process_manager.schedule_kill_port_and_restart(
            &mut self.state,
            command_id,
            port,
            dimensions,
            &banner,
            None,
        ) {
            Ok(()) => {
                self.synced_session_id = Some(command_id.to_string());
                self.terminal_notice = Some(format!("Resolving port {port} conflict..."));
                self.terminal_actionable_notice = None;
                self.save_session_state();
            }
            Err(error) => {
                self.record_port_kill_feedback(command_id, port, PortKillFeedback::Error);
                self.terminal_notice = Some(format!(
                    "Failed to restart server after freeing port: {error}"
                ));
                let _ = self.process_manager.write_virtual_text(
                    command_id,
                    &format!("\x1b[31mFailed to resolve port {port} conflict: {error}\x1b[0m\r\n"),
                );
            }
        }
        let _ = project_id;
        let _ = command;
        cx.notify();
    }

    fn select_server_tab_action(&mut self, command_id: &str, cx: &mut Context<Self>) {
        let lookup = self.state.find_command(command_id).map(|lookup| {
            (
                lookup.project.id.clone(),
                lookup.command.id.clone(),
                lookup.command.label.clone(),
            )
        });
        let server_tab_exists = existing_server_tab(&self.state, command_id).is_some();
        if lookup.is_none() && !server_tab_exists {
            self.terminal_notice = Some(format!("Unknown command `{command_id}`"));
            cx.notify();
            return;
        }
        self.interrupt_active_browser_replay_before_route_change(None);
        if self.remote_mode.is_some() {
            if let Some((project_id, command_id, label)) = lookup.clone() {
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
        if self.state.find_project(project_id).is_none() {
            self.terminal_notice = Some(format!("Unknown project `{project_id}`"));
            cx.notify();
            return;
        }
        if self.remote_mode.is_some() {
            let dimensions = self.terminal_dimensions(window);
            self.remote_send_action(RemoteAction::LaunchAi {
                project_id: project_id.to_string(),
                tab_type,
                dimensions,
            });
            self.show_terminal_surface();
            self.terminal_notice = Some("Launching remote AI session...".to_string());
            cx.notify();
            return;
        }

        let dimensions = self.terminal_dimensions(window);
        self.interrupt_active_browser_replay_before_route_change(None);
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
                let next_workspace = browser_workspace_key_for_ai_tab(Some(&tab));
                self.interrupt_active_browser_replay_before_route_change(next_workspace.as_ref());
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
            self.remote_send_action(RemoteAction::OpenAiTab {
                tab_id: tab_id.to_string(),
                dimensions,
            });
            self.show_terminal_surface();
            self.terminal_notice = Some("Opening remote AI tab...".to_string());
            cx.notify();
            return;
        }

        let Some(tab) = self.state.find_ai_tab(tab_id).cloned() else {
            cx.notify();
            return;
        };

        let next_workspace = browser_workspace_key_for_ai_tab(Some(&tab));
        self.interrupt_active_browser_replay_before_route_change(next_workspace.as_ref());
        self.state.select_tab(tab_id);
        self.show_terminal_surface();
        self.synced_session_id = tab.pty_session_id.clone();
        self.last_dimensions = None;

        if self.ai_tab_session_needs_restore(&tab) {
            if !self.local_host_has_control() {
                self.terminal_notice = Some(
                    "Another remote client controls this host. Take local control to reopen this AI session."
                        .to_string(),
                );
                self.save_session_state();
                cx.notify();
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
                    self.synced_session_id = Some(session_id);
                    self.terminal_notice = None;
                }
                Err(error) => {
                    self.terminal_notice = Some(format!("Failed to open AI tab: {error}"));
                }
            }
        } else {
            self.terminal_notice = None;
        }

        self.save_session_state();
        cx.notify();
    }

    fn restart_ai_tab_action(&mut self, tab_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if !self.ensure_mutation_control(cx) {
            return;
        }
        let Some(tab) = self.state.find_ai_tab(tab_id).cloned() else {
            self.terminal_notice = Some(format!("Unknown AI tab `{tab_id}`"));
            cx.notify();
            return;
        };
        if self.state.find_project(&tab.project_id).is_none() {
            self.terminal_notice = Some(format!("Unknown project `{}`", tab.project_id));
            cx.notify();
            return;
        }
        if let Err(error) = self
            .process_manager
            .validate_ai_restart(&self.state, tab_id)
        {
            self.terminal_notice = Some(format!("Failed to restart AI tab: {error}"));
            cx.notify();
            return;
        }
        let workspace_key = browser_workspace_key_for_ai_tab(Some(&tab));
        if self.remote_mode.is_some() {
            let dimensions = self.terminal_dimensions(window);
            self.remote_send_action(RemoteAction::RestartAiTab {
                tab_id: tab_id.to_string(),
                dimensions,
            });
            self.show_terminal_surface();
            self.terminal_notice = Some("Restarting remote AI tab...".to_string());
            cx.notify();
            return;
        }

        self.interrupt_active_browser_replay_before_route_change(workspace_key.as_ref());
        if let Some(workspace_key) = workspace_key.as_ref() {
            self.interrupt_browser_workspace_before_teardown(workspace_key);
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
        let workspace_key = browser_workspace_key_for_ai_tab(self.state.find_ai_tab(tab_id));
        if let Some(workspace_key) = workspace_key.as_ref() {
            self.interrupt_browser_workspace_before_teardown(workspace_key);
        }
        if self.remote_mode.is_some() {
            self.remote_send_action(RemoteAction::CloseAiTab {
                tab_id: tab_id.to_string(),
            });
            self.state.remove_tab(tab_id);
            self.synced_session_id = None;
            self.last_dimensions = None;
            self.terminal_notice = Some("Closing remote AI tab...".to_string());
            cx.notify();
            return;
        }

        if let Err(error) = self.process_manager.close_tab(&mut self.state, tab_id) {
            self.terminal_notice = Some(format!("Failed to close AI tab: {error}"));
        } else {
            self.synced_session_id = None;
            self.last_dimensions = None;
            self.save_session_state();
        }
        cx.notify();
    }

    fn open_ssh_tab_action(&mut self, connection_id: &str, cx: &mut Context<Self>) {
        let Some(connection) = self.state.find_ssh_connection(connection_id).cloned() else {
            self.terminal_notice = Some(format!("Unknown SSH connection `{connection_id}`"));
            cx.notify();
            return;
        };
        if self.remote_mode.is_some() {
            if let Some(tab) = self
                .state
                .find_ssh_tab_by_connection(connection_id)
                .cloned()
            {
                self.interrupt_active_browser_replay_before_route_change(None);
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
            self.interrupt_active_browser_replay_before_route_change(None);
            self.remote_send_action(RemoteAction::OpenSshTab {
                connection_id: connection_id.to_string(),
            });
            self.show_terminal_surface();
            self.terminal_notice = Some("Opening remote SSH tab...".to_string());
            cx.notify();
            return;
        }

        if !self.ensure_mutation_control(cx) {
            return;
        }

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
        self.interrupt_active_browser_replay_before_route_change(None);
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
        if self.state.find_ssh_connection(connection_id).is_none() {
            self.terminal_notice = Some(format!("Unknown SSH connection `{connection_id}`"));
            cx.notify();
            return;
        }
        if self.remote_mode.is_some() {
            let dimensions = self.terminal_dimensions(window);
            self.interrupt_active_browser_replay_before_route_change(None);
            self.remote_send_action(RemoteAction::ConnectSsh {
                connection_id: connection_id.to_string(),
                dimensions,
            });
            self.show_terminal_surface();
            self.terminal_notice = Some("Connecting remote SSH session...".to_string());
            cx.notify();
            return;
        }

        let dimensions = self.terminal_dimensions(window);
        self.interrupt_active_browser_replay_before_route_change(None);
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
        if self.state.find_ssh_connection(connection_id).is_none() {
            self.terminal_notice = Some(format!("Unknown SSH connection `{connection_id}`"));
            cx.notify();
            return;
        }
        if self.remote_mode.is_some() {
            let dimensions = self.terminal_dimensions(window);
            self.interrupt_active_browser_replay_before_route_change(None);
            self.remote_send_action(RemoteAction::RestartSsh {
                connection_id: connection_id.to_string(),
                dimensions,
            });
            self.show_terminal_surface();
            self.terminal_notice = Some("Restarting remote SSH session...".to_string());
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

        self.interrupt_active_browser_replay_before_route_change(None);
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
            self.remote_send_action(RemoteAction::DisconnectSsh {
                connection_id: connection_id.to_string(),
            });
            self.synced_session_id = None;
            self.last_dimensions = None;
            self.terminal_notice = Some("Disconnecting remote SSH session...".to_string());
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
        if self.add_project_wizard.is_some() || self.process_monitor.is_some() {
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
                        self.remote_send_terminal_input(RemoteTerminalInput::Control {
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
                            self.remote_send_terminal_input(RemoteTerminalInput::Control {
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
                        self.remote_send_terminal_input(RemoteTerminalInput::Control {
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
                    self.remote_send_terminal_input(RemoteTerminalInput::Control {
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
        if self.browser_replay_secret_prompt.is_some() {
            window.prevent_default();
            return;
        }
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
                    if self.remote_mode.is_some() {
                        self.remote_send_action(RemoteAction::CloseSession {
                            session_id: session_id.clone(),
                        });
                    } else {
                        let _ = self.process_manager.close_session(session_id);
                    }
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
                                let _ = self
                                    .process_manager
                                    .paste_user_text_to_session(&session_id, &text);
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
                                    .write_user_bytes_to_session(&session_id, &bytes);
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
                        let _ = self
                            .process_manager
                            .write_user_text_to_session(&session_id, &text);
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
                                self.remote_send_terminal_input(RemoteTerminalInput::Control {
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
                    self.remote_send_terminal_input(RemoteTerminalInput::Control {
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
        let scrollbar_width = if self.state.settings().show_terminal_scrollbar {
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

        let active_workspace_key = browser_workspace_key_for_ai_tab(self.state.active_tab());
        let pending_action_notice_visible = pending_annotation_action_notice_message(
            self.pending_annotation_action_notice.as_ref(),
            active_workspace_key.as_ref(),
            self.remote_mode.is_some(),
            Instant::now(),
        )
        .is_some();
        if self.startup_notice.is_some()
            || self.terminal_notice.is_some()
            || pending_action_notice_visible
        {
            top += NOTICE_HEIGHT_PX + STACK_GAP_PX;
        }
        if self
            .current_active_session_view()
            .is_some_and(|session| session.runtime.awaiting_external_editor)
        {
            top += NOTICE_HEIGHT_PX + STACK_GAP_PX;
        }
        if self
            .pending_annotation_source_for_tab(self.state.active_tab())
            .is_some_and(|(_, _, pending_annotations)| !pending_annotations.is_empty())
        {
            top += PENDING_ANNOTATION_STRIP_HEIGHT_PX + STACK_GAP_PX;
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
        apply_window_bounds_state(
            &mut self.state,
            crate::models::WindowBoundsState {
                x: f32::from(bounds.origin.x),
                y: f32::from(bounds.origin.y),
                width: f32::from(bounds.size.width),
                height: f32::from(bounds.size.height),
                maximized,
            },
        );
    }
}

fn apply_window_bounds_state(state: &mut AppState, next: crate::models::WindowBoundsState) -> bool {
    if state.window_bounds == Some(next) {
        return false;
    }
    state.window_bounds = Some(next);
    true
}

fn remote_shared_app_state(state: &AppState) -> AppState {
    let mut next = state.clone();
    next.window_bounds = None;
    next
}

impl Render for NativeShell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let render_started = Instant::now();
        self.start_browser_tasks(window, cx);
        self.sync_browser_host_visibility(Some(window));
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

        let local_runtime_snapshot = if self.remote_mode.is_none() {
            Some(self.process_manager.runtime_state())
        } else {
            None
        };

        let runtime_snapshot =
            local_runtime_snapshot.unwrap_or_else(|| self.current_runtime_snapshot());
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
            Some(self.sync_terminal_session(window, &runtime_snapshot, cx))
        } else {
            None
        };
        let browser_model = self.active_browser_model();

        let make_open_settings_handler =
            || -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
                Box::new(cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                    this.open_settings_action(cx);
                }))
            };
        let make_open_remote_handler = || -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
            Box::new(cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                this.ensure_remote_settings_open_with_tab(this.status_bar_remote_tab(), cx);
            }))
        };
        let make_remote_native_toggle_handler =
            || -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
                Box::new(cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                    this.toggle_local_status_bar_hosting(cx);
                }))
            };
        let make_remote_web_toggle_handler =
            || -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
                Box::new(cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                    this.toggle_local_status_bar_web_hosting(cx);
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
                    RemoteStatusBarAction::OpenRemoteConnectTab => {
                        this.ensure_remote_settings_open_with_tab(RemoteTopTab::Connect, cx);
                    }
                    RemoteStatusBarAction::OpenRemoteHostTab => {
                        this.ensure_remote_settings_open_with_tab(RemoteTopTab::Host, cx);
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
                    RemoteStatusBarAction::TakeHostControl => {
                        this.remote_host_service.take_local_control();
                        this.editor_notice =
                            Some("This machine controls the host again.".to_string());
                        this.sync_settings_remote_draft();
                        cx.notify();
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
        let make_take_remote_control_handler =
            || -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
                Box::new(cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                    if let Some(remote_mode) = this.remote_mode.as_ref() {
                        remote_mode.client.take_control();
                    }
                    this.editor_notice =
                        Some("This client now controls the remote host.".to_string());
                    this.sync_settings_remote_draft();
                    cx.notify();
                }))
            };
        let make_release_remote_control_handler =
            || -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
                Box::new(cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                    if let Some(remote_mode) = this.remote_mode.as_ref() {
                        remote_mode.client.release_control();
                    }
                    this.editor_notice =
                        Some("This client released control and is now a viewer.".to_string());
                    this.sync_settings_remote_draft();
                    cx.notify();
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
        let make_force_quit_handler = || -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
            Box::new(cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                this.force_quit_action(cx);
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
        let make_open_process_monitor_handler =
            || -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
                Box::new(cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                    this.open_process_monitor_action(cx);
                }))
            };
        let make_process_monitor_action_handler =
            |action: process_monitor::ProcessMonitorAction| -> Box<
                dyn Fn(&MouseDownEvent, &mut Window, &mut App),
            > {
                Box::new(cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                    this.handle_process_monitor_action(action.clone(), cx);
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
        let make_browser_action_handler = {
            let browser_entity = editor_entity.clone();
            Arc::new(
                move |action: BrowserPaneAction| -> Box<
                    dyn Fn(&MouseDownEvent, &mut Window, &mut App),
                > {
                    let browser_entity = browser_entity.clone();
                    Box::new(move |_, window, app| {
                        app.stop_propagation();
                        let _ = browser_entity.update(app, |this, cx| {
                            this.apply_browser_pane_action(action.clone(), window, cx);
                        });
                    })
                },
            )
        };
        let preview_pending_annotation_handler: view::PendingAnnotationActionHandler = {
            let entity = editor_entity.clone();
            Arc::new(move |action, _, window, app| {
                let _ = entity.update(app, move |this, cx| {
                    this.preview_pending_annotation_action(action, window, cx);
                });
            })
        };
        let remove_pending_annotation_handler: view::PendingAnnotationActionHandler = {
            let entity = editor_entity.clone();
            Arc::new(move |action, _, window, app| {
                let _ = entity.update(app, move |this, cx| {
                    this.remove_pending_annotation_action(action, window, cx);
                });
            })
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
                on_open_browser: browser_model
                    .as_ref()
                    .filter(|model| model.eligible && !model.pane_open)
                    .map(|_| make_browser_action_handler(BrowserPaneAction::Open)),
                on_preview_annotation: Some(preview_pending_annotation_handler.clone()),
                on_remove_annotation: Some(remove_pending_annotation_handler.clone()),
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
                on_actionable_notice_action: self.terminal_actionable_notice.as_ref().map(
                    |notice| match notice {
                        ActionableNotice::PortInUse {
                            command_id: cmd_id, ..
                        } => make_kill_port_handler(cmd_id.clone()),
                        ActionableNotice::ForceQuit { .. } => make_force_quit_handler(),
                    },
                ),
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
                on_take_remote_control: controls
                    .and_then(|controls| controls.remote_control.as_ref())
                    .filter(|control| control.can_take)
                    .map(|_| make_take_remote_control_handler()),
                on_release_remote_control: controls
                    .and_then(|controls| controls.remote_control.as_ref())
                    .filter(|control| control.can_release)
                    .map(|_| make_release_remote_control_handler()),
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
                            .into_any_element()
                    } else {
                        let model = terminal_model.as_ref().expect("terminal model");
                        let terminal_surface = div()
                            .flex_1()
                            .h_full()
                            .overflow_hidden()
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
                            .into_any_element();

                        if let Some(browser) = browser_model
                            .clone()
                            .filter(|model| model.eligible && model.pane_open)
                        {
                            let total_width = self
                                .browser_split_bounds
                                .map(|bounds| bounds.width as f32)
                                .unwrap_or_else(|| {
                                    let width: f32 = window.viewport_size().width.into();
                                    width
                                });
                            let layout = calculate_browser_split(
                                total_width,
                                browser.split_percent,
                                300.0,
                                320.0,
                                6.0,
                            );
                            let split_entity = cx.weak_entity();
                            let page_entity = split_entity.clone();
                            let browser_actions = BrowserPaneActions {
                                on_action: make_browser_action_handler.clone(),
                                on_address_key: Box::new(
                                    cx.listener(Self::handle_browser_address_key),
                                ),
                                on_replay_secret_key: Box::new(
                                    cx.listener(Self::handle_browser_replay_secret_key),
                                ),
                                on_annotation_key: Box::new(
                                    cx.listener(Self::handle_browser_annotation_key),
                                ),
                                on_workflow_key: Box::new(
                                    cx.listener(Self::handle_browser_workflow_key),
                                ),
                                on_page_bounds: Arc::new(move |bounds, _window, app| {
                                    let _ = page_entity.update(app, |this, cx| {
                                        this.capture_browser_page_bounds(bounds, cx);
                                    });
                                }),
                            };

                            div()
                                .flex_1()
                                .h_full()
                                .min_w(px(0.0))
                                .relative()
                                .flex()
                                .flex_row()
                                .overflow_hidden()
                                .on_mouse_move(cx.listener(
                                    |this, event: &MouseMoveEvent, window, cx| {
                                        if this.browser_divider_drag.is_some() {
                                            this.apply_browser_pane_action(
                                                BrowserPaneAction::DividerUpdate {
                                                    pointer_x: f32::from(event.position.x),
                                                },
                                                window,
                                                cx,
                                            );
                                        }
                                    },
                                ))
                                .on_mouse_up(
                                    MouseButton::Left,
                                    cx.listener(|this, _: &MouseUpEvent, window, cx| {
                                        if this.browser_divider_drag.is_some() {
                                            this.apply_browser_pane_action(
                                                BrowserPaneAction::DividerEnd,
                                                window,
                                                cx,
                                            );
                                        }
                                    }),
                                )
                                .on_mouse_up_out(
                                    MouseButton::Left,
                                    cx.listener(|this, _: &MouseUpEvent, window, cx| {
                                        if this.browser_divider_drag.is_some() {
                                            this.apply_browser_pane_action(
                                                BrowserPaneAction::DividerEnd,
                                                window,
                                                cx,
                                            );
                                        }
                                    }),
                                )
                                .child(
                                    canvas(
                                        move |bounds, _window, app| {
                                            let _ = split_entity.update(app, |this, cx| {
                                                this.capture_browser_split_bounds(bounds, cx);
                                            });
                                        },
                                        |_, _, _, _| {},
                                    )
                                    .absolute()
                                    .top(px(0.0))
                                    .left(px(0.0))
                                    .size_full(),
                                )
                                .child(
                                    div()
                                        .h_full()
                                        .w(px(layout.terminal_width))
                                        .flex_none()
                                        .overflow_hidden()
                                        .child(terminal_surface),
                                )
                                .child(
                                    div()
                                        .h_full()
                                        .w(px(layout.divider_width))
                                        .flex_none()
                                        .cursor_col_resize()
                                        .bg(rgb(theme::BORDER_PRIMARY))
                                        .hover(|style| style.bg(rgb(theme::PRIMARY)))
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(
                                                |this, event: &MouseDownEvent, window, cx| {
                                                    this.apply_browser_pane_action(
                                                        BrowserPaneAction::DividerBegin {
                                                            pointer_x: f32::from(event.position.x),
                                                        },
                                                        window,
                                                        cx,
                                                    );
                                                },
                                            ),
                                        ),
                                )
                                .child(
                                    div()
                                        .h_full()
                                        .w(px(layout.pane_width))
                                        .flex_none()
                                        .overflow_hidden()
                                        .child(render_browser_pane(
                                            browser,
                                            self.browser_address_focus.clone(),
                                            self.browser_replay_secret_focus.clone(),
                                            self.browser_annotation_focus.clone(),
                                            self.browser_workflow_focus.clone(),
                                            browser_actions,
                                        )),
                                )
                                .into_any_element()
                        } else {
                            terminal_surface
                        }
                    })
                    .child(chrome::render_status_bar(
                        &runtime_snapshot,
                        &updater_snapshot,
                        Some(&remote_status_bar.model),
                        chrome::StatusBarActions {
                            on_open_process_monitor: &make_open_process_monitor_handler,
                            on_install_update: &make_install_update_handler,
                            on_open_remote: &make_open_remote_handler,
                            on_remote_native_toggle: &make_remote_native_toggle_handler,
                            on_remote_web_toggle: &make_remote_web_toggle_handler,
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
            .children(self.process_monitor.as_ref().map(|monitor| {
                process_monitor::render_process_monitor(
                    monitor,
                    &self.state,
                    &runtime_snapshot,
                    process_monitor::ProcessMonitorActions {
                        on_action: &make_process_monitor_action_handler,
                    },
                )
            }))
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

fn remote_ai_response_should_include_session_view(
    session: Option<&crate::state::SessionRuntimeState>,
    session_attached: bool,
    now: Instant,
) -> bool {
    matches!(
        native_ai_render_mode(session, session_attached, true, now),
        NativeAiRenderMode::ActiveControl
    )
}

fn remote_ai_tab_payload_for_remote_response(
    state: &AppState,
    process_manager: &ProcessManager,
    session_id: &str,
    now: Instant,
) -> Option<RemoteActionPayload> {
    let runtime = process_manager.runtime_state();
    let session_runtime = runtime.sessions.get(session_id).cloned();
    let session_attached = process_manager.session_attached(session_id);
    let session_view = remote_ai_response_should_include_session_view(
        session_runtime.as_ref(),
        session_attached,
        now,
    )
    .then(|| process_manager.session_view(session_id))
    .flatten();
    remote_ai_tab_payload(state, session_id, session_view)
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
    matches!(
        tab.tab_type,
        TabType::Server | TabType::Claude | TabType::Codex | TabType::Ssh
    )
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
    for tab in open_tabs.iter_mut() {
        if matches!(tab.tab_type, TabType::Claude | TabType::Codex) {
            tab.command_id = None;
            tab.pty_session_id = None;
        }
    }
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
    if state
        .active_tab()
        .is_some_and(|tab| matches!(tab.tab_type, TabType::Claude | TabType::Codex))
    {
        state.active_tab_id = state
            .open_tabs
            .iter()
            .find(|tab| matches!(tab.tab_type, TabType::Server | TabType::Ssh))
            .map(|tab| tab.id.clone());
    }
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

fn normalize_remote_bind_address(value: &str) -> Result<String, String> {
    let value = value.trim();
    let value = if value.is_empty() { "127.0.0.1" } else { value };
    value
        .parse::<IpAddr>()
        .map(|address| address.to_string())
        .map_err(|_| format!("`{value}` is not a valid IP bind address"))
}

fn parse_required_remote_port(value: &str, label: &str) -> Result<u16, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err(format!("{label} is required"));
    }
    let port = value
        .parse::<u16>()
        .map_err(|_| format!("{label} must be a number between 1 and 65535"))?;
    if port == 0 {
        return Err(format!("{label} must be between 1 and 65535"));
    }
    Ok(port)
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

fn selection_range(
    cursor: usize,
    anchor: Option<usize>,
    char_len: usize,
) -> Option<(usize, usize)> {
    anchor.and_then(|a| {
        let clamped_cursor = cursor.min(char_len);
        let clamped_anchor = a.min(char_len);
        let (start, end) = if clamped_anchor < clamped_cursor {
            (clamped_anchor, clamped_cursor)
        } else {
            (clamped_cursor, clamped_anchor)
        };
        if start == end {
            None
        } else {
            Some((start, end))
        }
    })
}

fn normalize_selection_bounds(cursor: &mut usize, anchor: &mut Option<usize>, char_len: usize) {
    *cursor = (*cursor).min(char_len);
    if let Some(current_anchor) = anchor.as_mut() {
        *current_anchor = (*current_anchor).min(char_len);
    }
    if matches!(anchor, Some(current_anchor) if *current_anchor == *cursor) {
        *anchor = None;
    }
}

fn delete_selection(chars: &mut Vec<char>, cursor: &mut usize, anchor: &mut Option<usize>) {
    normalize_selection_bounds(cursor, anchor, chars.len());
    if let Some((start, end)) = selection_range(*cursor, *anchor, chars.len()) {
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
    normalize_selection_bounds(cursor, selection_anchor, chars.len());

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
            } else if let Some((start, _)) =
                selection_range(*cursor, *selection_anchor, chars.len())
            {
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
            } else if let Some((_, end)) = selection_range(*cursor, *selection_anchor, chars.len())
            {
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
            if selection_range(*cursor, *selection_anchor, chars.len()).is_some() {
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
            if selection_range(*cursor, *selection_anchor, chars.len()).is_some() {
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

/// Pure scrollbar math shared by render and tests. With no scrollback
/// (alt-screen apps, fresh sessions) this intentionally returns a
/// full-height inert thumb instead of `None`, so the gutter stays visible
/// whenever the setting is on — matching Windows Terminal.
fn scrollbar_model_for_screen(
    screen: &crate::terminal::session::TerminalScreenSnapshot,
    drag_thumb_top_ratio: Option<f32>,
    enabled: bool,
) -> Option<view::TerminalScrollbarModel> {
    if !enabled {
        return None;
    }

    let total_lines = screen.total_lines.max(screen.rows.max(1));
    let visible_lines = screen.rows.max(1);
    let max_offset = screen.history_size.max(1);
    let thumb_height_ratio = visible_lines as f32 / total_lines as f32;
    let thumb_top_ratio = drag_thumb_top_ratio
        .unwrap_or_else(|| scrollbar_thumb_top_ratio(screen.display_offset, max_offset));

    Some(view::TerminalScrollbarModel {
        thumb_top_ratio: thumb_top_ratio.clamp(0.0, 1.0),
        thumb_height_ratio,
    })
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NativeAiRenderMode {
    Wait,
    PassiveView,
    ActiveControl,
}

fn native_ai_render_mode(
    session: Option<&crate::state::SessionRuntimeState>,
    session_attached: bool,
    has_session_view: bool,
    now: Instant,
) -> NativeAiRenderMode {
    let Some(session) = session else {
        return NativeAiRenderMode::ActiveControl;
    };
    if !session_attached {
        return NativeAiRenderMode::ActiveControl;
    }
    let startup_guard_active = session.status == crate::state::SessionStatus::Starting
        || (session.session_kind.is_ai()
            && session.status.is_live()
            && !session.at_prompt
            && session
                .started_at
                .map(|started_at| {
                    now.saturating_duration_since(started_at) <= AI_LOCAL_RENDER_GUARD_WINDOW
                })
                .unwrap_or(false));
    if startup_guard_active {
        return if has_session_view {
            NativeAiRenderMode::PassiveView
        } else {
            NativeAiRenderMode::Wait
        };
    }
    NativeAiRenderMode::ActiveControl
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

fn local_browser_workflow_project_root(
    state: &AppState,
    workspace_key: &BrowserWorkspaceKey,
    remote_client: bool,
) -> Result<std::path::PathBuf, BrowserError> {
    if remote_client {
        return Err(BrowserError::InvalidInvocation {
            field: "localProjectRoot".to_string(),
        });
    }
    let project = state
        .find_project(&workspace_key.project_id)
        .ok_or_else(|| BrowserError::InvalidWorkspaceKey {
            field: "projectId".to_string(),
        })?;
    let root = std::path::PathBuf::from(&project.root_path);
    if !root.is_dir() {
        return Err(BrowserError::InvalidRecipe {
            message: "owning project root is unavailable".to_string(),
        });
    }
    root.canonicalize()
        .map_err(|_| BrowserError::InvalidRecipe {
            message: "owning project root could not be verified".to_string(),
        })
}

fn existing_server_tab<'a>(state: &'a AppState, tab_id: &str) -> Option<&'a SessionTab> {
    state
        .find_tab(tab_id)
        .filter(|tab| tab.tab_type == TabType::Server)
}

fn validate_project_deletion(state: &AppState, project_id: &str) -> Result<(), String> {
    state
        .find_project(project_id)
        .map(|_| ())
        .ok_or_else(|| format!("Unknown project `{project_id}`"))
}

fn validate_browser_pane_action_before_replay_interrupt(
    workspace_key: &BrowserWorkspaceKey,
    snapshot: &BrowserWorkspaceSnapshot,
    address_draft: &str,
    action: &BrowserPaneAction,
    mut interrupt_then_retire: impl FnMut(),
) -> Result<BrowserActionPlan, BrowserError> {
    let plan = browser_action_plan(
        Some(workspace_key),
        Some(snapshot),
        address_draft,
        action.clone(),
    )?;
    let interrupts_active_replay = match action {
        BrowserPaneAction::SelectTab(_) => !plan.commands.is_empty(),
        BrowserPaneAction::Collapse
        | BrowserPaneAction::CreateTab
        | BrowserPaneAction::Back
        | BrowserPaneAction::Forward
        | BrowserPaneAction::Reload
        | BrowserPaneAction::FocusAddress
        | BrowserPaneAction::SubmitAddress
        | BrowserPaneAction::SetViewport(_)
        | BrowserPaneAction::ToggleAnnotation
        | BrowserPaneAction::StartRecording => true,
        _ => false,
    };
    if interrupts_active_replay {
        interrupt_then_retire();
    }
    Ok(plan)
}

fn route_browser_request_for_active_workspace(
    active_workspace: Option<&BrowserWorkspaceKey>,
    request: BrowserCommandRequest,
    dispatch_open: impl FnOnce(BrowserCommandRequest),
) -> Result<(), BrowserError> {
    let route_is_open = active_workspace == Some(request.workspace_key());
    route_browser_request(route_is_open, request, dispatch_open)
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

    #[test]
    fn update_exit_dominates_repeated_normal_quit_requests() {
        assert_eq!(
            PendingAppTermination::Quit.coalesce(PendingAppTermination::Quit),
            PendingAppTermination::Quit
        );
        assert_eq!(
            PendingAppTermination::Quit.coalesce(PendingAppTermination::ExitAfterUpdate),
            PendingAppTermination::ExitAfterUpdate
        );
        assert_eq!(
            PendingAppTermination::ExitAfterUpdate.coalesce(PendingAppTermination::Quit),
            PendingAppTermination::ExitAfterUpdate
        );
    }

    #[test]
    fn pending_app_termination_is_retained_until_native_window_teardown_drains() {
        let mut pending = Some(PendingAppTermination::ExitAfterUpdate);

        assert_eq!(take_ready_app_termination(&mut pending, false), None);
        assert_eq!(pending, Some(PendingAppTermination::ExitAfterUpdate));
        assert_eq!(
            take_ready_app_termination(&mut pending, true),
            Some(PendingAppTermination::ExitAfterUpdate)
        );
        assert_eq!(pending, None);
    }

    #[test]
    fn shutdown_failures_ignore_stale_ops_and_preserve_forced_exit_authority() {
        assert_eq!(
            shutdown_failure_disposition(
                Some(42),
                41,
                Some(PendingAppTermination::ExitAfterUpdate),
            ),
            ShutdownFailureDisposition::IgnoreStale
        );
        assert_eq!(
            shutdown_failure_disposition(None, 42, Some(PendingAppTermination::Quit)),
            ShutdownFailureDisposition::IgnoreStale
        );
        assert_eq!(
            shutdown_failure_disposition(
                Some(42),
                42,
                Some(PendingAppTermination::ExitAfterUpdate),
            ),
            ShutdownFailureDisposition::PreservePendingTermination
        );
        assert_eq!(
            shutdown_failure_disposition(Some(42), 42, Some(PendingAppTermination::Quit)),
            ShutdownFailureDisposition::PreservePendingTermination
        );
        assert_eq!(
            shutdown_failure_disposition(Some(42), 42, None),
            ShutdownFailureDisposition::ResumeInteractiveShutdown
        );
    }

    #[test]
    fn forced_termination_retires_the_shutdown_op_before_late_success_or_failure() {
        let mut pending_shutdown_op_id = Some(42);
        let mut pending_window_close = true;

        retire_pending_shutdown_for_forced_termination(
            &mut pending_shutdown_op_id,
            &mut pending_window_close,
        );

        assert_eq!(pending_shutdown_op_id, None);
        assert!(!pending_window_close);
        assert!(!shutdown_completion_is_current(pending_shutdown_op_id, 42));
        assert_eq!(
            shutdown_failure_disposition(
                pending_shutdown_op_id,
                42,
                Some(PendingAppTermination::ExitAfterUpdate),
            ),
            ShutdownFailureDisposition::IgnoreStale
        );
    }

    #[test]
    fn successful_update_install_promotes_only_an_existing_pending_termination() {
        let mut quit = Some(PendingAppTermination::Quit);
        promote_pending_app_termination_for_update(&mut quit);
        assert_eq!(quit, Some(PendingAppTermination::ExitAfterUpdate));

        let mut update = Some(PendingAppTermination::ExitAfterUpdate);
        promote_pending_app_termination_for_update(&mut update);
        assert_eq!(update, Some(PendingAppTermination::ExitAfterUpdate));

        let mut no_pending_termination = None;
        promote_pending_app_termination_for_update(&mut no_pending_termination);
        assert_eq!(no_pending_termination, None);
    }

    fn sample_ai_tab() -> SessionTab {
        SessionTab {
            id: "tab-1".to_string(),
            tab_type: TabType::Claude,
            project_id: "project-1".to_string(),
            command_id: Some("stale-ai-command".to_string()),
            pty_session_id: Some("session-1".to_string()),
            label: Some("Claude 1".to_string()),
            ssh_connection_id: None,
            browser_workspace: Some(
                serde_json::from_value(serde_json::json!({
                    "paneOpen": true,
                    "pendingAnnotationIds": ["annotation-1"]
                }))
                .expect("browser workspace fixture"),
            ),
        }
    }

    fn attachment_snapshot(ids: &[&str]) -> BrowserWorkspaceSnapshot {
        let annotations = ids
            .iter()
            .map(|id| {
                serde_json::json!({
                    "id": id,
                    "kind": "element",
                    "tabId": "page",
                    "anchorRevision": 1,
                    "comment": format!("Review {id}"),
                    "url": "https://example.test/page",
                    "locator": {},
                    "bounds": { "x": 1, "y": 2, "width": 3, "height": 4 },
                    "viewport": {},
                    "screenshotResource": format!("shot-{id}"),
                    "computedStyles": {},
                    "resolved": false
                })
            })
            .collect::<Vec<_>>();
        serde_json::from_value(serde_json::json!({
            "annotations": annotations,
            "pendingAnnotationRevision": ids.len(),
            "pendingAnnotationIds": ids,
        }))
        .expect("attachment snapshot fixture")
    }

    fn active_ai_terminal_model(
        workspace_key: &BrowserWorkspaceKey,
        snapshot: &BrowserWorkspaceSnapshot,
    ) -> view::TerminalPaneModel {
        view::TerminalPaneModel {
            active_project: "Project".to_string(),
            session_label: "Claude".to_string(),
            active_tab_type: Some(TabType::Claude),
            session: None,
            startup_notice: None,
            blocking_notice: None,
            actionable_notice: None,
            pending_annotations: view::pending_annotation_chip_models(
                Some(&TabType::Claude),
                workspace_key,
                snapshot,
                &snapshot.annotations,
            ),
            debug_enabled: false,
            font_size: view::TERMINAL_FONT_SIZE,
            cell_width: 8.0,
            line_height: view::TERMINAL_LINE_HEIGHT,
            selection: None,
            search: None,
            search_highlight: None,
            scrollbar: None,
            runtime_controls: None,
            splash_image: None,
        }
    }

    struct RecordingAttachmentProjectionSink {
        state: AppState,
        host_snapshot: BrowserWorkspaceSnapshot,
        fail_host: bool,
        fail_persist: bool,
        host_calls: usize,
        persist_calls: usize,
        broker_for_persist_observation: BrowserAttachmentBroker,
        dirty_during_persist: bool,
        concurrent_observation: Option<(BrowserAttachmentBroker, BrowserWorkspaceSnapshot)>,
    }

    impl BrowserAttachmentProjectionSink for RecordingAttachmentProjectionSink {
        fn acknowledge_host(
            &mut self,
            projection: &crate::browser::BrowserAttachmentProjection,
        ) -> Result<BrowserWorkspaceSnapshot, BrowserError> {
            self.host_calls += 1;
            if let Some((broker, snapshot)) = self.concurrent_observation.take() {
                broker.observe_workspace(projection.workspace_key.clone(), &snapshot);
            }
            if self.fail_host {
                return Err(BrowserError::CrashedView {
                    message: "host acknowledgement failed".to_string(),
                });
            }
            Ok(self.host_snapshot.clone())
        }

        fn persist_snapshot(
            &mut self,
            workspace_key: &BrowserWorkspaceKey,
            snapshot: BrowserWorkspaceSnapshot,
        ) -> bool {
            self.persist_calls += 1;
            self.dirty_during_persist = !self
                .broker_for_persist_observation
                .dirty_projections()
                .is_empty();
            let updated = self
                .state
                .update_browser_workspace(&workspace_key.ai_tab_id, move |current| {
                    *current = snapshot
                });
            updated && !self.fail_persist
        }
    }

    fn delivered_projection_fixture() -> (
        BrowserAttachmentBroker,
        crate::browser::BrowserAttachmentProjection,
        BrowserWorkspaceSnapshot,
    ) {
        let broker = BrowserAttachmentBroker::default();
        let key = BrowserWorkspaceKey::new("project-1", "tab-1").unwrap();
        let stale = attachment_snapshot(&["ann-delivered"]);
        broker.observe_workspace(key.clone(), &stale);
        let initial = broker.dirty_projections().pop().unwrap();
        assert!(broker.acknowledge_dirty_projection(&initial));
        broker.bind_session("session-1", key);
        let reservation = broker
            .reserve_for_input(
                "session-1",
                crate::browser::BrowserPromptInput::Text("prompt"),
            )
            .unwrap();
        broker.commit(reservation).unwrap();
        let projection = broker.dirty_projections().pop().unwrap();
        (broker, projection, stale)
    }

    fn projection_sink(
        stale: BrowserWorkspaceSnapshot,
        broker: BrowserAttachmentBroker,
    ) -> RecordingAttachmentProjectionSink {
        let mut state = AppState::default();
        let mut tab = sample_ai_tab();
        tab.browser_workspace = Some(stale.clone());
        state.open_tabs.push(tab);
        let mut host_snapshot = stale;
        host_snapshot.pending_annotation_ids.clear();
        RecordingAttachmentProjectionSink {
            state,
            host_snapshot,
            fail_host: false,
            fail_persist: false,
            host_calls: 0,
            persist_calls: 0,
            broker_for_persist_observation: broker,
            dirty_during_persist: false,
            concurrent_observation: None,
        }
    }

    #[test]
    fn empty_pump_projection_transaction_applies_persists_then_acknowledges() {
        let (broker, projection, stale) = delivered_projection_fixture();
        let mut sink = projection_sink(stale, broker.clone());

        let result =
            reconcile_browser_attachment_projection_transaction(&broker, &projection, &mut sink)
                .unwrap();

        assert_eq!(result, BrowserAttachmentProjectionTransaction::Applied);
        assert_eq!(sink.host_calls, 1);
        assert_eq!(sink.persist_calls, 1);
        assert!(sink.dirty_during_persist);
        assert!(sink
            .state
            .browser_workspace("tab-1")
            .unwrap()
            .pending_annotation_ids
            .is_empty());
        assert!(broker.dirty_projections().is_empty());
    }

    #[test]
    fn projection_transaction_host_or_persist_failure_remains_retryable() {
        for (fail_host, fail_persist, expected_persist_calls) in
            [(true, false, 0), (false, true, 1)]
        {
            let (broker, projection, stale) = delivered_projection_fixture();
            let mut sink = projection_sink(stale, broker.clone());
            sink.fail_host = fail_host;
            sink.fail_persist = fail_persist;

            let result = reconcile_browser_attachment_projection_transaction(
                &broker,
                &projection,
                &mut sink,
            );

            if fail_host {
                assert!(result.is_err());
            } else {
                assert_eq!(
                    result.unwrap(),
                    BrowserAttachmentProjectionTransaction::PersistFailed
                );
            }
            assert_eq!(sink.persist_calls, expected_persist_calls);
            assert_eq!(broker.dirty_projections(), vec![projection]);
        }
    }

    #[test]
    fn projection_transaction_preserves_a_concurrent_newer_generation_as_dirty() {
        let (broker, projection, stale) = delivered_projection_fixture();
        let newer = attachment_snapshot(&["ann-delivered", "ann-new"]);
        let mut sink = projection_sink(stale, broker.clone());
        sink.host_snapshot = newer.clone();
        sink.concurrent_observation = Some((broker.clone(), newer));

        let result =
            reconcile_browser_attachment_projection_transaction(&broker, &projection, &mut sink)
                .unwrap();

        assert_eq!(
            result,
            BrowserAttachmentProjectionTransaction::NewerProjectionRemainsDirty
        );
        assert_eq!(
            sink.state
                .browser_workspace("tab-1")
                .unwrap()
                .pending_annotation_ids,
            vec!["ann-new"]
        );
        let retry = broker.dirty_projections();
        assert_eq!(retry.len(), 1);
        assert_eq!(retry[0].pending_annotation_ids, vec!["ann-new"]);
    }

    #[test]
    fn pending_annotation_remove_detaches_reconciles_and_retains_saved_context() {
        let broker = BrowserAttachmentBroker::default();
        let key = BrowserWorkspaceKey::new("project-1", "tab-1").unwrap();
        let saved = attachment_snapshot(&["ann-remove"]);
        let screenshot = saved
            .annotation("ann-remove")
            .unwrap()
            .screenshot_resource
            .clone();
        broker.observe_workspace(key.clone(), &saved);
        let initial = broker.dirty_projections().pop().unwrap();
        assert!(broker.acknowledge_dirty_projection(&initial));
        let mut sink = projection_sink(saved, broker.clone());
        let action = view::PendingAnnotationAction {
            workspace_key: key.clone(),
            annotation_id: "ann-remove".to_string(),
        };

        let result =
            remove_pending_annotation_projection_transaction(&broker, &key, &action, &mut sink)
                .unwrap();

        assert_eq!(result, BrowserAttachmentProjectionTransaction::Applied);
        assert_eq!(sink.host_calls, 1);
        assert_eq!(sink.persist_calls, 1);
        assert!(broker.projection(&key).pending_annotation_ids.is_empty());
        assert!(broker.dirty_projections().is_empty());
        let persisted = sink.state.browser_workspace("tab-1").unwrap();
        assert!(persisted.pending_annotation_ids.is_empty());
        assert_eq!(
            persisted
                .annotation("ann-remove")
                .unwrap()
                .screenshot_resource,
            screenshot
        );
    }

    #[test]
    fn pending_annotation_remove_rejects_cross_workspace_or_stale_actions_before_detach() {
        let broker = BrowserAttachmentBroker::default();
        let key = BrowserWorkspaceKey::new("project-1", "tab-1").unwrap();
        let saved = attachment_snapshot(&["ann-pending"]);
        broker.observe_workspace(key.clone(), &saved);
        let initial = broker.dirty_projections().pop().unwrap();
        assert!(broker.acknowledge_dirty_projection(&initial));

        for action in [
            view::PendingAnnotationAction {
                workspace_key: BrowserWorkspaceKey::new("project-2", "tab-2").unwrap(),
                annotation_id: "ann-pending".to_string(),
            },
            view::PendingAnnotationAction {
                workspace_key: key.clone(),
                annotation_id: "ann-stale".to_string(),
            },
        ] {
            let mut sink = projection_sink(saved.clone(), broker.clone());
            let error =
                remove_pending_annotation_projection_transaction(&broker, &key, &action, &mut sink)
                    .unwrap_err();
            assert!(matches!(error, BrowserError::MissingAnnotation { .. }));
            assert_eq!(sink.host_calls, 0);
            assert_eq!(sink.persist_calls, 0);
            assert_eq!(
                broker.projection(&key).pending_annotation_ids,
                vec!["ann-pending"]
            );
        }
    }

    #[test]
    fn pending_annotation_remove_persistence_failure_remains_retryable() {
        let broker = BrowserAttachmentBroker::default();
        let key = BrowserWorkspaceKey::new("project-1", "tab-1").unwrap();
        let saved = attachment_snapshot(&["ann-remove"]);
        broker.observe_workspace(key.clone(), &saved);
        let initial = broker.dirty_projections().pop().unwrap();
        assert!(broker.acknowledge_dirty_projection(&initial));
        let mut sink = projection_sink(saved, broker.clone());
        sink.fail_persist = true;
        let action = view::PendingAnnotationAction {
            workspace_key: key.clone(),
            annotation_id: "ann-remove".to_string(),
        };

        let result =
            remove_pending_annotation_projection_transaction(&broker, &key, &action, &mut sink)
                .unwrap();

        assert_eq!(
            result,
            BrowserAttachmentProjectionTransaction::PersistFailed
        );
        assert_eq!(broker.dirty_projections().len(), 1);
    }

    #[test]
    fn pending_annotation_action_failure_notices_are_fixed_concise_and_non_sensitive() {
        let cases = [
            (
                PendingAnnotationActionFailure::RemovePersistence,
                "Annotation removal will retry when browser state saves.",
            ),
            (
                PendingAnnotationActionFailure::MissingAnnotation,
                "Annotation is no longer pending in this conversation.",
            ),
            (
                PendingAnnotationActionFailure::RemoveFailed,
                "Could not remove the pending annotation.",
            ),
            (
                PendingAnnotationActionFailure::InvalidSavedUrl,
                "Saved annotation URL cannot be previewed.",
            ),
            (
                PendingAnnotationActionFailure::PreviewFailed,
                "Could not open the annotation preview.",
            ),
            (
                PendingAnnotationActionFailure::RemoteRemove,
                "Remove pending browser annotations from the connected host.",
            ),
        ];

        for (failure, expected) in cases {
            let notice = failure.notice();
            assert_eq!(notice, expected);
            assert!(notice.len() <= 64);
            assert!(!notice.contains("super-secret"));
        }
    }

    #[test]
    fn chip_action_failure_survives_active_ai_terminal_model_refresh_while_pane_is_collapsed() {
        let now = Instant::now();
        let key = BrowserWorkspaceKey::new("project-1", "tab-1").unwrap();
        let mut snapshot = attachment_snapshot(&["ann-preview"]);
        snapshot.pane_open = false;
        let mut action_notice = Some(PendingAnnotationActionNotice::new(
            key.clone(),
            PendingAnnotationActionFailure::PreviewFailed,
            false,
            now,
        ));
        let mut model = active_ai_terminal_model(&key, &snapshot);

        refresh_terminal_pane_model_notice(
            &mut model,
            Some("Existing startup notice"),
            None,
            &mut action_notice,
            Some(&key),
            false,
            now,
        );
        assert!(!snapshot.pane_open);
        assert_eq!(model.pending_annotations.len(), 1);
        assert_eq!(
            model.startup_notice.as_deref(),
            Some("Could not open the annotation preview.")
        );

        model.startup_notice = None;
        refresh_terminal_pane_model_notice(
            &mut model,
            None,
            None,
            &mut action_notice,
            Some(&key),
            false,
            now + Duration::from_secs(1),
        );
        assert_eq!(
            model.startup_notice.as_deref(),
            Some("Could not open the annotation preview."),
            "ordinary local AI refresh must not erase chip-action feedback"
        );
        let _rendered = view::render_terminal_surface(&model, None);
    }

    #[test]
    fn chip_action_notice_is_workspace_and_mode_scoped_then_clears_on_success_or_expiry() {
        let now = Instant::now();
        let key = BrowserWorkspaceKey::new("project-1", "tab-1").unwrap();
        let other = BrowserWorkspaceKey::new("project-1", "tab-2").unwrap();
        let snapshot = attachment_snapshot(&["ann-preview"]);
        let mut model = active_ai_terminal_model(&key, &snapshot);
        let mut action_notice = Some(PendingAnnotationActionNotice::new(
            key.clone(),
            PendingAnnotationActionFailure::PreviewFailed,
            true,
            now,
        ));

        refresh_terminal_pane_model_notice(
            &mut model,
            None,
            None,
            &mut action_notice,
            Some(&key),
            true,
            now,
        );
        assert_eq!(
            model.startup_notice.as_deref(),
            Some("Could not open the annotation preview."),
            "remote AI refresh must project remote chip-action feedback"
        );
        clear_pending_annotation_action_notice(&mut action_notice, &key);
        model.startup_notice = None;
        refresh_terminal_pane_model_notice(
            &mut model,
            None,
            Some("Existing transient notice"),
            &mut action_notice,
            Some(&key),
            true,
            now,
        );
        assert_eq!(
            model.startup_notice.as_deref(),
            Some("Existing transient notice")
        );

        action_notice = Some(PendingAnnotationActionNotice::new(
            key.clone(),
            PendingAnnotationActionFailure::RemoveFailed,
            false,
            now,
        ));
        model.startup_notice = None;
        refresh_terminal_pane_model_notice(
            &mut model,
            None,
            None,
            &mut action_notice,
            Some(&other),
            false,
            now,
        );
        assert!(model.startup_notice.is_none());
        assert!(
            action_notice.is_some(),
            "another workspace must not consume it"
        );

        refresh_terminal_pane_model_notice(
            &mut model,
            None,
            None,
            &mut action_notice,
            Some(&key),
            true,
            now,
        );
        assert!(model.startup_notice.is_none());
        assert!(
            action_notice.is_none(),
            "a local/remote mode change clears it"
        );

        action_notice = Some(PendingAnnotationActionNotice::new(
            key.clone(),
            PendingAnnotationActionFailure::InvalidSavedUrl,
            false,
            now,
        ));
        refresh_terminal_pane_model_notice(
            &mut model,
            None,
            Some("Existing transient notice"),
            &mut action_notice,
            Some(&key),
            false,
            now + PENDING_ANNOTATION_ACTION_NOTICE_DURATION + Duration::from_millis(1),
        );
        assert!(action_notice.is_none());
        assert_eq!(
            model.startup_notice.as_deref(),
            Some("Existing transient notice"),
            "expiry must restore the existing transient notice lifecycle"
        );
    }

    #[test]
    fn restored_attachment_state_overlays_tombstones_and_unions_new_annotations() {
        let broker = BrowserAttachmentBroker::default();
        let key = BrowserWorkspaceKey::new("project-1", "tab-1").unwrap();
        let original = attachment_snapshot(&["ann-delivered"]);
        broker.observe_workspace(key.clone(), &original);
        let observed = broker.dirty_projections().pop().unwrap();
        assert!(broker.acknowledge_dirty_projection(&observed));
        broker.bind_session("session-1", key);
        let reservation = broker
            .reserve_for_input(
                "session-1",
                crate::browser::BrowserPromptInput::Text("prompt"),
            )
            .unwrap();
        broker.commit(reservation).unwrap();

        let mut state = AppState::default();
        let mut tab = sample_ai_tab();
        tab.browser_workspace = Some(attachment_snapshot(&["ann-delivered", "ann-new"]));
        state.open_tabs.push(tab);

        assert!(reconcile_restored_browser_attachment_state(
            &mut state, &broker
        ));
        let restored = state.browser_workspace("tab-1").unwrap();
        assert_eq!(restored.pending_annotation_ids, vec!["ann-new"]);
        assert_eq!(restored.annotations.len(), 2);
        assert!(restored.pending_annotation_revision.0 >= 3);
    }

    #[test]
    fn remote_disconnect_backup_projection_suppresses_a_stale_delivered_id() {
        let (broker, _projection, stale) = delivered_projection_fixture();
        let mut backup = AppState::default();
        let mut tab = sample_ai_tab();
        tab.browser_workspace = Some(stale);
        backup.open_tabs.push(tab);

        assert!(reconcile_restored_browser_attachment_state(
            &mut backup,
            &broker
        ));

        assert!(backup
            .browser_workspace("tab-1")
            .unwrap()
            .pending_annotation_ids
            .is_empty());
        assert_eq!(broker.dirty_projections().len(), 1);
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
            browser_workspace: None,
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
            browser_workspace: None,
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
            private_key: None,
        }
    }

    fn sample_remote_host_status() -> remote::RemoteHostStatus {
        remote::RemoteHostStatus {
            enabled: false,
            web_enabled: false,
            bind_address: "127.0.0.1".to_string(),
            port: 43871,
            pairing_token: "PAIR1234".to_string(),
            connected_clients: 0,
            connected_native_clients: 0,
            connected_web_clients: 0,
            controller_client_id: None,
            listening: false,
            listener_error: None,
            web_listener_error: None,
            last_connection_note: None,
            last_connection_is_error: false,
            latency: RemoteLatencyStats::default(),
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
    fn editor_text_input_clamps_stale_selection_anchor_before_delete() {
        let mut value = "300".to_string();
        let mut cursor = 3usize;
        let mut selection_anchor = Some(4usize);
        let event = KeyDownEvent {
            keystroke: Keystroke {
                modifiers: Modifiers::default(),
                key: "1".to_string(),
                key_char: Some("1".to_string()),
            },
            is_held: false,
        };

        let changed = apply_text_key_to_string(
            &mut value,
            &mut cursor,
            &mut selection_anchor,
            &event,
            None,
            true,
            false,
        );

        assert!(changed);
        assert_eq!(value, "3001");
        assert_eq!(cursor, 4);
        assert_eq!(selection_anchor, None);
    }

    #[test]
    fn annotation_comment_editor_accepts_multiline_unicode_text() {
        let mut value = "Review 👁".to_string();
        let mut cursor = value.chars().count();
        let mut selection_anchor = None;
        let enter = KeyDownEvent {
            keystroke: Keystroke {
                modifiers: Modifiers::default(),
                key: "enter".to_string(),
                key_char: None,
            },
            is_held: false,
        };
        assert!(apply_text_key_to_string(
            &mut value,
            &mut cursor,
            &mut selection_anchor,
            &enter,
            None,
            false,
            true,
        ));

        let text = KeyDownEvent {
            keystroke: Keystroke {
                modifiers: Modifiers::default(),
                key: "n".to_string(),
                key_char: Some("next".to_string()),
            },
            is_held: false,
        };
        assert!(apply_text_key_to_string(
            &mut value,
            &mut cursor,
            &mut selection_anchor,
            &text,
            None,
            false,
            true,
        ));
        assert_eq!(value, "Review 👁\nnext");
        assert_eq!(cursor, value.chars().count());
    }

    #[test]
    fn remote_status_bar_idle_state_shows_local_with_transport_toggles() {
        let status = sample_remote_host_status();
        let preferred_host = remote::KnownRemoteHost {
            label: "Studio".to_string(),
            ..Default::default()
        };

        let state =
            build_remote_status_bar_state(None, &status, Some(&preferred_host), true, false);

        assert_eq!(state.model.label, "Local");
        assert!(!state.model.native_host.enabled);
        assert_eq!(state.model.native_host.count, None);
        assert_eq!(state.model.native_host.tone, chrome::StatusBarTone::Muted);
        assert!(!state.model.web_host.enabled);
        assert_eq!(state.model.web_host.count, None);
        assert_eq!(state.model.web_host.tone, chrome::StatusBarTone::Muted);
        assert_eq!(
            state.primary_action,
            Some(RemoteStatusBarAction::ConnectPreferred)
        );
        assert_eq!(
            state
                .model
                .primary_action
                .as_ref()
                .map(|action| action.label.as_str()),
            Some("Quick connect")
        );
        assert_eq!(state.secondary_action, None);
    }

    #[test]
    fn empty_remote_bind_normalizes_to_loopback_and_port_is_required() {
        assert_eq!(
            normalize_remote_bind_address("").expect("empty bind should normalize safely"),
            "127.0.0.1"
        );
        assert!(parse_required_remote_port("", "Browser port").is_err());
        assert!(parse_required_remote_port("0", "Browser port").is_err());
        assert!(parse_required_remote_port("not-a-port", "Browser port").is_err());
        assert_eq!(
            parse_required_remote_port("43872", "Browser port").expect("valid browser port"),
            43872
        );
    }

    #[test]
    fn remote_status_bar_local_hosting_state_uses_transport_icons() {
        let mut status = sample_remote_host_status();
        status.enabled = true;
        status.web_enabled = true;
        status.listening = true;

        let state = build_remote_status_bar_state(None, &status, None, true, false);

        assert_eq!(state.model.label, "Local");
        assert!(state.model.native_host.enabled);
        assert_eq!(state.model.native_host.count, None);
        assert_eq!(state.model.native_host.tone, chrome::StatusBarTone::Success);
        assert!(state.model.web_host.enabled);
        assert_eq!(state.model.web_host.count, None);
        assert_eq!(state.model.web_host.tone, chrome::StatusBarTone::Accent);
        assert_eq!(
            state.primary_action,
            Some(RemoteStatusBarAction::OpenRemoteConnectTab)
        );
        assert_eq!(
            state
                .model
                .primary_action
                .as_ref()
                .map(|action| action.label.as_str()),
            Some("Connect...")
        );
    }

    #[test]
    fn remote_status_bar_local_hosting_counts_show_on_transport_icons() {
        let mut status = sample_remote_host_status();
        status.enabled = true;
        status.web_enabled = true;
        status.listening = true;
        status.connected_clients = 3;
        status.connected_native_clients = 2;
        status.connected_web_clients = 1;
        status.controller_client_id = Some("web-1".to_string());

        let state = build_remote_status_bar_state(None, &status, None, false, false);

        assert_eq!(state.model.label, "Local • Browser controls");
        assert_eq!(state.model.native_host.count, Some(2));
        assert_eq!(state.model.web_host.count, Some(1));
        assert_eq!(
            state.primary_action,
            Some(RemoteStatusBarAction::TakeHostControl)
        );
    }

    #[test]
    fn remote_status_bar_remote_connection_prioritizes_remote_label_but_keeps_local_host_icons() {
        let mut status = sample_remote_host_status();
        status.enabled = true;
        status.web_enabled = true;
        status.listening = true;
        status.connected_clients = 1;
        status.connected_native_clients = 0;
        status.connected_web_clients = 1;
        let remote_connection = RemoteStatusBarConnectionSnapshot {
            connected_label: "Studio".to_string(),
            has_control: false,
            reconnecting: false,
        };

        let state =
            build_remote_status_bar_state(Some(&remote_connection), &status, None, true, false);

        assert_eq!(state.model.label, "Remote • Studio");
        assert!(state.model.native_host.enabled);
        assert_eq!(state.model.native_host.count, None);
        assert_eq!(state.model.native_host.tone, chrome::StatusBarTone::Success);
        assert!(state.model.web_host.enabled);
        assert_eq!(state.model.web_host.count, Some(1));
        assert_eq!(
            state.primary_action,
            Some(RemoteStatusBarAction::TakeRemoteControl)
        );
    }

    #[test]
    fn remote_status_bar_remote_reconnect_surfaces_retry_action() {
        let status = sample_remote_host_status();
        let remote_connection = RemoteStatusBarConnectionSnapshot {
            connected_label: "Studio".to_string(),
            has_control: true,
            reconnecting: true,
        };

        let state =
            build_remote_status_bar_state(Some(&remote_connection), &status, None, true, false);

        assert_eq!(state.model.label, "Remote • Studio");
        assert_eq!(
            state.primary_action,
            Some(RemoteStatusBarAction::RetryReconnect)
        );
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
    fn remote_ai_launch_response_skips_session_view_for_fresh_attached_startup_sessions() {
        let now = Instant::now();
        let mut session = SessionRuntimeState::new(
            "claude-session",
            PathBuf::from("."),
            SessionDimensions::default(),
            TerminalBackend::PortablePtyFeedingAlacritty,
        );
        session.session_kind = crate::state::SessionKind::Claude;
        session.status = crate::state::SessionStatus::Starting;
        session.started_at = Some(now);

        assert!(!remote_ai_response_should_include_session_view(
            Some(&session),
            true,
            now,
        ));

        session.status = crate::state::SessionStatus::Running;
        session.at_prompt = true;
        assert!(remote_ai_response_should_include_session_view(
            Some(&session),
            true,
            now + AI_LOCAL_RENDER_GUARD_WINDOW,
        ));
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
    fn scrollbar_model_shows_full_height_thumb_without_history() {
        let mut screen = screen_from_lines(&["one", "two"]);
        screen.total_lines = 2;
        screen.history_size = 0;
        screen.display_offset = 0;

        let model = scrollbar_model_for_screen(&screen, None, true).expect("model");

        assert_eq!(model.thumb_height_ratio, 1.0);
    }

    #[test]
    fn scrollbar_model_hidden_when_setting_disabled() {
        let mut screen = screen_from_lines(&["one", "two"]);
        screen.total_lines = 20;
        screen.history_size = 18;

        assert!(scrollbar_model_for_screen(&screen, None, false).is_none());
    }

    #[test]
    fn scrollbar_model_keeps_proportional_thumb_with_history() {
        let mut screen = screen_from_lines(&["one", "two"]);
        screen.total_lines = 8;
        screen.history_size = 6;
        screen.display_offset = 0;

        let model = scrollbar_model_for_screen(&screen, None, true).expect("model");

        assert_eq!(model.thumb_height_ratio, 0.25);
        assert_eq!(model.thumb_top_ratio, 1.0);
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
    fn native_ai_render_uses_passive_view_for_fresh_attached_ai_startup_sessions() {
        let now = Instant::now();
        let mut session = SessionRuntimeState::new(
            "claude-session",
            PathBuf::from("."),
            SessionDimensions::default(),
            TerminalBackend::PortablePtyFeedingAlacritty,
        );
        session.session_kind = crate::state::SessionKind::Claude;
        session.status = crate::state::SessionStatus::Starting;
        session.started_at = Some(now);

        assert_eq!(
            native_ai_render_mode(Some(&session), true, true, now),
            NativeAiRenderMode::PassiveView
        );
        assert_eq!(
            native_ai_render_mode(Some(&session), true, false, now),
            NativeAiRenderMode::Wait
        );
        assert_eq!(
            native_ai_render_mode(Some(&session), false, true, now),
            NativeAiRenderMode::ActiveControl
        );

        session.status = crate::state::SessionStatus::Running;
        assert_eq!(
            native_ai_render_mode(Some(&session), true, true, now),
            NativeAiRenderMode::PassiveView
        );

        session.at_prompt = true;
        assert_eq!(
            native_ai_render_mode(Some(&session), true, true, now),
            NativeAiRenderMode::ActiveControl
        );

        session.at_prompt = false;
        session.started_at = Some(now - AI_LOCAL_RENDER_GUARD_WINDOW - Duration::from_secs(1));
        assert_eq!(
            native_ai_render_mode(Some(&session), true, true, now),
            NativeAiRenderMode::ActiveControl
        );

        assert_eq!(
            native_ai_render_mode(None, true, false, now),
            NativeAiRenderMode::ActiveControl
        );
    }

    #[test]
    fn apply_window_bounds_state_updates_bounds_without_bumping_revision() {
        let mut state = AppState::default();
        let bounds = crate::models::WindowBoundsState {
            x: 10.0,
            y: 20.0,
            width: 800.0,
            height: 600.0,
            maximized: false,
        };

        let initial_revision = state.revision();
        assert!(apply_window_bounds_state(&mut state, bounds));
        let after_first = state.revision();
        assert_eq!(after_first, initial_revision);
        assert_eq!(state.window_bounds, Some(bounds));

        assert!(!apply_window_bounds_state(&mut state, bounds));
        assert_eq!(state.revision(), after_first);
    }

    #[test]
    fn remote_shared_app_state_omits_window_bounds() {
        let mut state = AppState::default();
        state.window_bounds = Some(crate::models::WindowBoundsState {
            x: 10.0,
            y: 20.0,
            width: 800.0,
            height: 600.0,
            maximized: false,
        });

        let shared = remote_shared_app_state(&state);

        assert_eq!(shared.window_bounds, None);
        assert_eq!(shared.revision(), state.revision());
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
    fn close_button_background_behavior_uses_only_global_minimize_setting() {
        assert!(!should_minimize_window_on_close(false, false));
        assert!(!should_minimize_window_on_close(false, true));
        assert!(should_minimize_window_on_close(true, false));
        assert!(should_minimize_window_on_close(true, true));
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
    fn persisted_session_state_keeps_ai_workspace_but_strips_runtime_identity() {
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
            vec!["tab-1", "server-tab", "ssh-tab"]
        );
        assert_eq!(session.active_tab_id.as_deref(), Some("tab-1"));
        let ai_tab = &session.open_tabs[0];
        assert_eq!(ai_tab.command_id, None);
        assert_eq!(ai_tab.pty_session_id, None);
        assert_eq!(ai_tab.project_id, "project-1");
        let browser_json =
            serde_json::to_value(ai_tab.browser_workspace.as_ref().unwrap()).unwrap();
        assert_eq!(
            browser_json["pendingAnnotationIds"],
            serde_json::json!(["annotation-1"])
        );
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
    fn restore_saved_tabs_keeps_ai_tab_inactive_without_starting_provider() {
        let mut state = AppState::default();
        state.open_tabs.push(sample_ai_tab());
        state.active_tab_id = Some("tab-1".to_string());

        let manager = ProcessManager::new();
        let notice = restore_saved_tabs(&manager, &mut state, SessionDimensions::default());

        assert!(notice.is_none());
        assert!(manager.runtime_state().sessions.is_empty());
        assert_eq!(state.open_tabs.len(), 1);
        assert_eq!(state.open_tabs[0].id, "tab-1");
        assert_eq!(state.open_tabs[0].project_id, "project-1");
        assert_eq!(state.open_tabs[0].command_id, None);
        assert_eq!(state.open_tabs[0].pty_session_id, None);
        let browser_json =
            serde_json::to_value(state.open_tabs[0].browser_workspace.as_ref().unwrap()).unwrap();
        assert_eq!(
            browser_json["pendingAnnotationIds"],
            serde_json::json!(["annotation-1"])
        );
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
        assert_eq!(state.open_tabs.len(), 2);
        assert_eq!(state.open_tabs[0].id, "tab-1");
        assert_eq!(state.open_tabs[1].id, "server-tab");
        assert_eq!(state.open_tabs[0].pty_session_id, None);
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

    #[test]
    fn server_tab_fallback_rejects_an_active_ai_tab_without_state_mutation() {
        let mut state = AppState::default();
        let ai_tab = sample_ai_tab();
        state.active_tab_id = Some(ai_tab.id.clone());
        state.open_tabs.push(ai_tab);
        let tabs_before = state.open_tabs.clone();
        let active_before = state.active_tab_id.clone();

        assert!(existing_server_tab(&state, "tab-1").is_none());
        assert_eq!(state.open_tabs, tabs_before);
        assert_eq!(state.active_tab_id, active_before);

        state.open_tabs.push(SessionTab {
            id: "server-tab".to_string(),
            tab_type: TabType::Server,
            project_id: "project-1".to_string(),
            command_id: Some("server-command".to_string()),
            pty_session_id: Some("server-command".to_string()),
            label: Some("Server".to_string()),
            ssh_connection_id: None,
            browser_workspace: None,
        });
        assert_eq!(
            existing_server_tab(&state, "server-tab").map(|tab| tab.tab_type.clone()),
            Some(TabType::Server)
        );
    }

    #[test]
    fn stale_project_deletion_preflight_is_side_effect_free() {
        let mut state = AppState::default();
        let ai_tab = sample_ai_tab();
        state.active_tab_id = Some(ai_tab.id.clone());
        state.open_tabs.push(ai_tab);
        let tabs_before = state.open_tabs.clone();
        let active_before = state.active_tab_id.clone();
        let projects_before = state.config.projects.clone();

        assert_eq!(
            validate_project_deletion(&state, "project-1"),
            Err("Unknown project `project-1`".to_string())
        );
        assert_eq!(state.open_tabs, tabs_before);
        assert_eq!(state.active_tab_id, active_before);
        assert_eq!(state.config.projects, projects_before);

        state.config.projects.push(Project {
            id: "project-1".to_string(),
            ..Project::default()
        });
        assert_eq!(validate_project_deletion(&state, "project-1"), Ok(()));
    }

    fn populated_secret_replay(
        workspace_key: &BrowserWorkspaceKey,
        recipe_id: &str,
    ) -> (
        BrowserCommandBridge,
        crate::browser::BrowserReplayCoordinator,
        crate::browser::BrowserReplayStart,
        BrowserReplaySecretPromptVault,
    ) {
        use crate::browser::{
            compile_browser_replay, BrowserRecipeAction, BrowserRecipeInput,
            BrowserRecipeInputKind, BrowserRecipeLocator, BrowserRecipeStep, BrowserRecipeV1,
            BrowserRecipeValue, BrowserRecipeViewport, BROWSER_RECIPE_SCHEMA_VERSION,
        };

        let (bridge, _inbox) = browser_command_channel(4);
        let coordinator = bridge.replay_coordinator();
        let started = coordinator
            .start(
                workspace_key.clone(),
                compile_browser_replay(
                    &BrowserRecipeV1 {
                        schema_version: BROWSER_RECIPE_SCHEMA_VERSION,
                        id: recipe_id.to_string(),
                        name: "Prompt boundary fixture".to_string(),
                        description: "Prompt boundary fixture".to_string(),
                        start_url: "https://example.test".to_string(),
                        viewport: BrowserRecipeViewport::default(),
                        inputs: vec![BrowserRecipeInput {
                            name: "credential".to_string(),
                            kind: BrowserRecipeInputKind::Secret,
                            default_value: None,
                        }],
                        steps: vec![BrowserRecipeStep {
                            id: "credential".to_string(),
                            action: BrowserRecipeAction::Type {
                                locator: BrowserRecipeLocator {
                                    test_id: Some("credential".to_string()),
                                    ..BrowserRecipeLocator::default()
                                },
                                value: BrowserRecipeValue::Input {
                                    name: "credential".to_string(),
                                },
                            },
                            wait: None,
                            assertions: Vec::new(),
                        }],
                    },
                    Vec::new(),
                )
                .unwrap(),
            )
            .unwrap();
        let (mut vault, _) = BrowserReplaySecretPromptVault::install(
            started.instance.clone(),
            started.projection.unresolved_secret_inputs.clone(),
        )
        .unwrap();
        vault
            .edit(&started.instance, "credential", "populated-secret")
            .unwrap();
        (bridge, coordinator, started, vault)
    }

    #[test]
    fn invalid_lifecycle_preflights_preserve_control_runtime_queue_and_replay_witnesses() {
        #[derive(Debug, Clone, PartialEq, Eq)]
        struct LifecycleBoundaryWitness {
            control_owner: Option<String>,
            control_generation: u64,
            tabs: Vec<SessionTab>,
            runtime_generation: u64,
            queue_generation: u64,
            replay_status: BrowserReplayStatus,
        }

        let mut state = AppState::default();
        state.config.projects.push(sample_project());
        let ai_tab = sample_ai_tab();
        state.active_tab_id = Some(ai_tab.id.clone());
        state.open_tabs.push(ai_tab);
        let workspace_key = BrowserWorkspaceKey::new("project-1", "tab-1").unwrap();
        let (bridge, coordinator, started, _vault) =
            populated_secret_replay(&workspace_key, "lifecycle-preflight-witness");
        let process_manager = ProcessManager::new();
        let runtime_generation = process_manager.runtime_revision();
        let initial = LifecycleBoundaryWitness {
            control_owner: Some("remote-controller".to_string()),
            control_generation: 41,
            tabs: state.open_tabs.clone(),
            runtime_generation,
            queue_generation: 17,
            replay_status: coordinator.status(&started.instance).unwrap().status,
        };

        for preflight in [
            process_manager.validate_server_launch(&state, "server-cmd"),
            process_manager.validate_server_launch(&state, "missing-command"),
            validate_project_deletion(&state, "missing-project"),
        ] {
            let mut witness = initial.clone();
            let result = preflight.map(|_| {
                witness.control_owner = None;
                witness.control_generation += 1;
                witness.tabs.clear();
                witness.runtime_generation += 1;
                witness.queue_generation += 1;
                bridge.interrupt_workspace(&workspace_key);
                witness.replay_status = coordinator.status(&started.instance).unwrap().status;
            });

            assert!(result.is_err());
            assert_eq!(witness, initial);
            assert_eq!(state.open_tabs, initial.tabs);
            assert_eq!(process_manager.runtime_revision(), runtime_generation);
            assert!(process_manager.drain_process_op_completions().is_empty());
            assert_eq!(
                coordinator.status(&started.instance).unwrap().status,
                initial.replay_status
            );
        }
    }

    #[test]
    fn tab_selection_preflight_preserves_invalid_authority_and_cancels_valid_change_first() {
        use crate::browser::{BrowserTabSnapshot, BrowserViewport};

        let workspace_key = BrowserWorkspaceKey::new("tab-project", "tab-conversation").unwrap();
        let snapshot = BrowserWorkspaceSnapshot {
            tabs: vec![
                BrowserTabSnapshot {
                    id: "tab-a".to_string(),
                    title: "A".to_string(),
                    url: "https://a.test".to_string(),
                    viewport: BrowserViewport::default(),
                },
                BrowserTabSnapshot {
                    id: "tab-b".to_string(),
                    title: "B".to_string(),
                    url: "https://b.test".to_string(),
                    viewport: BrowserViewport::default(),
                },
            ],
            selected_tab_id: Some("tab-a".to_string()),
            ..BrowserWorkspaceSnapshot::default()
        };

        for (recipe_id, action, expect_error) in [
            (
                "unknown-tab-prompt",
                BrowserPaneAction::SelectTab("missing-tab".to_string()),
                true,
            ),
            (
                "selected-tab-prompt",
                BrowserPaneAction::SelectTab("tab-a".to_string()),
                false,
            ),
        ] {
            let (bridge, coordinator, started, vault) =
                populated_secret_replay(&workspace_key, recipe_id);
            let status_before = coordinator.status(&started.instance).unwrap().status;
            let mut prompt = Some(vault);
            let mut boundary_called = false;
            let result = validate_browser_pane_action_before_replay_interrupt(
                &workspace_key,
                &snapshot,
                "",
                &action,
                || {
                    boundary_called = true;
                    bridge.interrupt_workspace(&workspace_key);
                    prompt.take();
                },
            );

            assert_eq!(result.is_err(), expect_error, "{recipe_id}");
            if let Ok(plan) = result {
                assert!(plan.commands.is_empty(), "{recipe_id}");
            }
            assert!(!boundary_called, "{recipe_id}");
            assert_eq!(
                prompt.as_ref().unwrap().projection().is_set,
                vec![true],
                "{recipe_id}"
            );
            assert_eq!(
                coordinator.status(&started.instance).unwrap().status,
                status_before,
                "{recipe_id}"
            );
        }

        let (bridge, coordinator, started, vault) =
            populated_secret_replay(&workspace_key, "different-tab-prompt");
        let mut prompt = Some(vault);
        let mut status_before_retirement = None;
        let plan = validate_browser_pane_action_before_replay_interrupt(
            &workspace_key,
            &snapshot,
            "",
            &BrowserPaneAction::SelectTab("tab-b".to_string()),
            || {
                bridge.interrupt_workspace(&workspace_key);
                status_before_retirement =
                    Some(coordinator.status(&started.instance).unwrap().status);
                prompt.take();
            },
        )
        .unwrap();

        assert_eq!(
            plan.commands,
            vec![BrowserCommand::SelectTab {
                tab_id: "tab-b".to_string()
            }]
        );
        assert_eq!(
            status_before_retirement,
            Some(BrowserReplayStatus::Cancelled)
        );
        assert!(prompt.is_none());
    }

    #[test]
    fn host_barriers_publish_queued_input_before_priority_lifecycle_dispatch() {
        let source = include_str!("mod.rs");
        let dispatch_start = source.find("fn dispatch_browser_command(").unwrap();
        let dispatch_end = source[dispatch_start..]
            .find("fn synchronize_browser_response(")
            .map(|offset| dispatch_start + offset)
            .unwrap();
        let dispatch = &source[dispatch_start..dispatch_end];
        let dispatch_publish = dispatch.find("publish_pending_user_input_cutoffs").unwrap();
        let dispatch_lifecycle = dispatch.find("for request in lifecycle_requests").unwrap();
        assert!(dispatch_publish < dispatch_lifecycle);

        let barrier_start = source
            .find("fn with_browser_host_control_barrier<R>(")
            .unwrap();
        let barrier_end = source[barrier_start..]
            .find("fn browser_pane_context(")
            .map(|offset| barrier_start + offset)
            .unwrap();
        let barrier = &source[barrier_start..barrier_end];
        let barrier_publish = barrier.find("publish_pending_user_input_cutoffs").unwrap();
        let barrier_lifecycle = barrier.find("for request in lifecycle_requests").unwrap();
        let enter_host = barrier.find("enter_host(browser_host)").unwrap();
        assert!(barrier_publish < barrier_lifecycle && barrier_lifecycle < enter_host);
    }

    #[test]
    fn valid_conflicting_pane_controls_cancel_running_replays_even_without_a_secret_prompt() {
        use crate::browser::{
            compile_browser_replay, BrowserRecipeAction, BrowserRecipeStep, BrowserRecipeV1,
            BrowserRecipeViewport, BrowserTabSnapshot, BrowserViewport,
            BROWSER_RECIPE_SCHEMA_VERSION,
        };

        fn running_replay(
            workspace_key: &BrowserWorkspaceKey,
            recipe_id: &str,
        ) -> (
            BrowserCommandBridge,
            crate::browser::BrowserReplayCoordinator,
            crate::browser::BrowserReplayStart,
        ) {
            let (bridge, _inbox) = browser_command_channel(4);
            let coordinator = bridge.replay_coordinator();
            let started = coordinator
                .start(
                    workspace_key.clone(),
                    compile_browser_replay(
                        &BrowserRecipeV1 {
                            schema_version: BROWSER_RECIPE_SCHEMA_VERSION,
                            id: recipe_id.to_string(),
                            name: "Running pane replay".to_string(),
                            description: "Pane interruption fixture".to_string(),
                            start_url: "https://example.test".to_string(),
                            viewport: BrowserRecipeViewport::default(),
                            inputs: Vec::new(),
                            steps: vec![BrowserRecipeStep {
                                id: "reload".to_string(),
                                action: BrowserRecipeAction::Reload,
                                wait: None,
                                assertions: Vec::new(),
                            }],
                        },
                        Vec::new(),
                    )
                    .unwrap(),
                )
                .unwrap();
            coordinator.begin(&started.instance).unwrap();
            (bridge, coordinator, started)
        }

        let workspace_key = BrowserWorkspaceKey::new("pane-project", "pane-conversation").unwrap();
        let snapshot = BrowserWorkspaceSnapshot {
            pane_open: true,
            tabs: vec![
                BrowserTabSnapshot {
                    id: "tab-a".to_string(),
                    title: "A".to_string(),
                    url: "https://a.test".to_string(),
                    viewport: BrowserViewport::default(),
                },
                BrowserTabSnapshot {
                    id: "tab-b".to_string(),
                    title: "B".to_string(),
                    url: "https://b.test".to_string(),
                    viewport: BrowserViewport::default(),
                },
            ],
            selected_tab_id: Some("tab-a".to_string()),
            ..BrowserWorkspaceSnapshot::default()
        };

        for (recipe_id, address, action) in [
            ("running-collapse", "", BrowserPaneAction::Collapse),
            (
                "running-select-tab",
                "",
                BrowserPaneAction::SelectTab("tab-b".to_string()),
            ),
            (
                "running-navigate",
                "https://next.test/path",
                BrowserPaneAction::SubmitAddress,
            ),
            ("running-back", "", BrowserPaneAction::Back),
            (
                "running-viewport",
                "",
                BrowserPaneAction::SetViewport(crate::browser::BrowserViewportPreset::Mobile),
            ),
            (
                "running-annotation",
                "",
                BrowserPaneAction::ToggleAnnotation,
            ),
        ] {
            let (bridge, coordinator, started) = running_replay(&workspace_key, recipe_id);
            let mut boundary_called = false;
            let plan = validate_browser_pane_action_before_replay_interrupt(
                &workspace_key,
                &snapshot,
                address,
                &action,
                || {
                    boundary_called = true;
                    bridge.interrupt_workspace(&workspace_key);
                },
            )
            .unwrap();

            assert!(!plan.commands.is_empty(), "{recipe_id}");
            assert!(boundary_called, "{recipe_id}");
            assert_eq!(
                coordinator.status(&started.instance).unwrap().status,
                BrowserReplayStatus::Cancelled,
                "{recipe_id}"
            );
        }

        for (recipe_id, address, action, expect_error) in [
            (
                "invalid-unknown-tab",
                "",
                BrowserPaneAction::SelectTab("missing-tab".to_string()),
                true,
            ),
            (
                "noop-selected-tab",
                "",
                BrowserPaneAction::SelectTab("tab-a".to_string()),
                false,
            ),
            (
                "invalid-blank-address",
                "   ",
                BrowserPaneAction::SubmitAddress,
                true,
            ),
        ] {
            let (bridge, coordinator, started) = running_replay(&workspace_key, recipe_id);
            let mut boundary_called = false;
            let result = validate_browser_pane_action_before_replay_interrupt(
                &workspace_key,
                &snapshot,
                address,
                &action,
                || {
                    boundary_called = true;
                    bridge.interrupt_workspace(&workspace_key);
                },
            );

            assert_eq!(result.is_err(), expect_error, "{recipe_id}");
            if let Ok(plan) = result {
                assert!(plan.commands.is_empty(), "{recipe_id}");
            }
            assert!(!boundary_called, "{recipe_id}");
            assert_eq!(
                coordinator.status(&started.instance).unwrap().status,
                BrowserReplayStatus::Running,
                "{recipe_id}"
            );
        }
    }

    #[tokio::test]
    async fn route_loss_after_successful_validation_rejects_priority_lifecycle_dispatch() {
        use crate::browser::{
            compile_browser_replay, BrowserRecipeAction, BrowserRecipeStep, BrowserRecipeV1,
            BrowserRecipeViewport, BROWSER_RECIPE_SCHEMA_VERSION,
        };

        let replay_plan = |id: &str| {
            compile_browser_replay(
                &BrowserRecipeV1 {
                    schema_version: BROWSER_RECIPE_SCHEMA_VERSION,
                    id: id.to_string(),
                    name: "Lifecycle route replay".to_string(),
                    description: "Priority route admission fixture".to_string(),
                    start_url: "https://example.test".to_string(),
                    viewport: BrowserRecipeViewport::default(),
                    inputs: Vec::new(),
                    steps: vec![BrowserRecipeStep {
                        id: "reload".to_string(),
                        action: BrowserRecipeAction::Reload,
                        wait: None,
                        assertions: Vec::new(),
                    }],
                },
                Vec::new(),
            )
            .unwrap()
        };
        let workspace_key =
            BrowserWorkspaceKey::new("lifecycle-route-project", "conversation").unwrap();
        let (bridge, mut inbox) = browser_command_channel(4);
        let controller = bridge.bind(workspace_key.clone(), Duration::from_secs(1));
        let coordinator = bridge.replay_coordinator();

        let validating_controller = controller.clone();
        let validation = tokio::spawn(async move {
            validating_controller
                .request_with_context(
                    BrowserCommand::WorkspaceState,
                    BrowserInvocationContext::agent(
                        "validate lifecycle route",
                        BrowserRisk::Normal,
                    )
                    .unwrap(),
                )
                .await
        });
        let validation_request = inbox.recv().await.expect("workspace validation request");
        validation_request.respond(Ok(BrowserResponse::WorkspaceState {
            snapshot: BrowserWorkspaceSnapshot {
                pane_open: true,
                ..BrowserWorkspaceSnapshot::default()
            },
        }));
        assert!(matches!(
            validation.await.unwrap(),
            Ok(BrowserResponse::WorkspaceState { .. })
        ));

        bridge.interrupt_workspace(&workspace_key);
        let replay = coordinator
            .start(workspace_key.clone(), replay_plan("inactive-route"))
            .unwrap();
        coordinator.begin(&replay.instance).unwrap();
        let retained_controller = controller.clone();
        let retained = tokio::spawn(async move {
            retained_controller
                .request(BrowserCommand::Reload {
                    tab_id: "tab-a".to_string(),
                })
                .await
        });
        let retained_request = inbox.recv().await.expect("retained tab request");
        assert!(retained_request.cancellation_is_current());
        let close_controller = controller.clone();
        let close = tokio::spawn(async move {
            close_controller
                .request(BrowserCommand::CloseTab {
                    tab_id: "tab-a".to_string(),
                })
                .await
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            while bridge.pending_work_count() < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("priority lifecycle request must enqueue");
        assert_eq!(
            coordinator.status(&replay.instance).unwrap().status,
            BrowserReplayStatus::Running,
            "an unadmitted lifecycle request cannot cancel a newer inactive-route replay"
        );
        assert!(
            retained_request.cancellation_is_current(),
            "route-rejected lifecycle work cannot advance tab cancellation epochs"
        );
        let (_controls, mut lifecycle_requests) =
            bridge.with_locked_host_work(|controls, requests| (controls, requests));
        assert_eq!(lifecycle_requests.len(), 1);
        let request = lifecycle_requests.pop().unwrap();
        let mut host_mutated = false;
        let error = route_browser_request_for_active_workspace(None, request, |_| {
            host_mutated = true;
        })
        .expect_err("lost route must reject the priority lifecycle request");

        assert!(!host_mutated, "CloseTab must not reach the browser host");
        assert_eq!(close.await.unwrap(), Err(error));
        assert_eq!(
            coordinator.status(&replay.instance).unwrap().status,
            BrowserReplayStatus::Running
        );
        assert!(retained_request.cancellation_is_current());
        retained_request.respond(Ok(BrowserResponse::Acknowledged));
        assert_eq!(retained.await.unwrap(), Ok(BrowserResponse::Acknowledged));

        coordinator.cancel(&replay.instance).unwrap();
        let active_replay = coordinator
            .start(workspace_key.clone(), replay_plan("active-route"))
            .unwrap();
        coordinator.begin(&active_replay.instance).unwrap();
        let active_close_controller = controller.clone();
        let active_close = tokio::spawn(async move {
            active_close_controller
                .request_with_context(
                    BrowserCommand::CloseTab {
                        tab_id: "tab-a".to_string(),
                    },
                    BrowserInvocationContext::agent(
                        "direct agent close outside replay execution",
                        BrowserRisk::Destructive,
                    )
                    .unwrap(),
                )
                .await
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            while bridge.pending_work_count() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("active lifecycle request must enqueue");
        let (_controls, mut active_requests) =
            bridge.with_locked_host_work(|controls, requests| (controls, requests));
        let active_request = active_requests.pop().expect("active lifecycle request");
        let mut active_host_mutated = false;
        route_browser_request_for_active_workspace(
            Some(&workspace_key),
            active_request,
            |request| {
                assert_eq!(
                    coordinator.status(&active_replay.instance).unwrap().status,
                    BrowserReplayStatus::Cancelled,
                    "valid lifecycle cancellation must precede host mutation"
                );
                assert!(request.cancellation_is_current());
                active_host_mutated = true;
                request.respond(Ok(BrowserResponse::Acknowledged));
            },
        )
        .unwrap();
        assert!(active_host_mutated);
        assert_eq!(
            active_close.await.unwrap(),
            Ok(BrowserResponse::Acknowledged)
        );
    }

    #[test]
    fn ordinary_settings_disable_interrupts_all_browser_work_before_config_assignment() {
        let source = include_str!("mod.rs").replace("\r\n", "\n");
        let start = source
            .find("fn apply_settings_draft(")
            .expect("settings application helper");
        let end = source[start..]
            .find("fn save_editor_action(")
            .map(|offset| start + offset)
            .expect("settings helper boundary");
        let apply = &source[start..end];
        let interrupt = apply
            .find("self.interrupt_all_browser_replays_before_shutdown()")
            .expect("ordinary settings disable must interrupt local browser work");
        let preference = apply
            .find("apply_browser_enabled_preference(&mut settings, next_browser_enabled)")
            .expect("browser preference assignment");
        let assignment = apply
            .find("self.state.update_settings(settings)")
            .expect("settings state assignment");

        assert!(interrupt < preference);
        assert!(preference < assignment);
    }

    #[test]
    fn invalid_browser_address_preserves_populated_prompt_and_replay_authority() {
        use crate::browser::{
            compile_browser_replay, BrowserRecipeAction, BrowserRecipeInput,
            BrowserRecipeInputKind, BrowserRecipeLocator, BrowserRecipeStep, BrowserRecipeV1,
            BrowserRecipeValue, BrowserRecipeViewport, BrowserTabSnapshot, BrowserViewport,
            BROWSER_RECIPE_SCHEMA_VERSION,
        };

        let workspace_key =
            BrowserWorkspaceKey::new("prompt-project", "prompt-conversation").unwrap();
        let (bridge, _inbox) = browser_command_channel(4);
        let coordinator = bridge.replay_coordinator();
        let started = coordinator
            .start(
                workspace_key.clone(),
                compile_browser_replay(
                    &BrowserRecipeV1 {
                        schema_version: BROWSER_RECIPE_SCHEMA_VERSION,
                        id: "invalid-address-prompt".to_string(),
                        name: "Invalid address prompt".to_string(),
                        description: "Prompt preservation fixture".to_string(),
                        start_url: "https://example.test".to_string(),
                        viewport: BrowserRecipeViewport::default(),
                        inputs: vec![BrowserRecipeInput {
                            name: "credential".to_string(),
                            kind: BrowserRecipeInputKind::Secret,
                            default_value: None,
                        }],
                        steps: vec![BrowserRecipeStep {
                            id: "credential".to_string(),
                            action: BrowserRecipeAction::Type {
                                locator: BrowserRecipeLocator {
                                    test_id: Some("credential".to_string()),
                                    ..BrowserRecipeLocator::default()
                                },
                                value: BrowserRecipeValue::Input {
                                    name: "credential".to_string(),
                                },
                            },
                            wait: None,
                            assertions: Vec::new(),
                        }],
                    },
                    Vec::new(),
                )
                .unwrap(),
            )
            .unwrap();
        let (mut vault, _) = BrowserReplaySecretPromptVault::install(
            started.instance.clone(),
            started.projection.unresolved_secret_inputs.clone(),
        )
        .unwrap();
        vault
            .edit(&started.instance, "credential", "populated-secret")
            .unwrap();
        let mut prompt = Some(vault);
        let mut boundary_called = false;
        let snapshot = BrowserWorkspaceSnapshot {
            tabs: vec![BrowserTabSnapshot {
                id: "prompt-tab".to_string(),
                title: "Prompt fixture".to_string(),
                url: "https://example.test".to_string(),
                viewport: BrowserViewport::default(),
            }],
            selected_tab_id: Some("prompt-tab".to_string()),
            ..BrowserWorkspaceSnapshot::default()
        };

        let result = validate_browser_pane_action_before_replay_interrupt(
            &workspace_key,
            &snapshot,
            "   ",
            &BrowserPaneAction::SubmitAddress,
            || {
                boundary_called = true;
                bridge.interrupt_workspace(&workspace_key);
                prompt.take();
            },
        );

        assert!(matches!(
            result,
            Err(BrowserError::NavigationFailure { url, message })
                if url.is_empty() && message == "address cannot be blank"
        ));
        assert!(!boundary_called);
        assert_eq!(prompt.as_ref().unwrap().projection().is_set, vec![true]);
        assert_eq!(
            coordinator.status(&started.instance).unwrap().status,
            BrowserReplayStatus::NeedsUserSecret
        );
    }

    #[test]
    fn browser_workflow_save_root_is_exact_real_and_local_only() {
        let root = std::env::temp_dir().join(format!(
            "devmanager-workflow-root-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).expect("create project root");
        let mut state = AppState::default();
        state.config.projects.push(Project {
            id: "project-a".to_string(),
            root_path: root.to_string_lossy().into_owned(),
            ..Project::default()
        });
        let owned = BrowserWorkspaceKey::new("project-a", "ai-a").expect("workspace");
        let other = BrowserWorkspaceKey::new("project-b", "ai-a").expect("other workspace");

        assert_eq!(
            local_browser_workflow_project_root(&state, &owned, false)
                .expect("exact local project root"),
            root.canonicalize().expect("canonical project root")
        );
        assert!(local_browser_workflow_project_root(&state, &owned, true).is_err());
        assert!(local_browser_workflow_project_root(&state, &other, false).is_err());

        std::fs::remove_dir_all(root).expect("remove project root");
    }
}
