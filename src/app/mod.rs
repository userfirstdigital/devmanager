mod chrome;

use crate::assets::AppAssets;
use crate::models::{
    AppConfig, DependencyStatus, MacTerminalProfile, PortStatus, Project, ProjectFolder,
    RunCommand, SSHConnection, SessionState, SessionTab, TabType,
};
use crate::notifications;
use crate::services::{
    env_service, pid_file, platform_service, ports_service, scanner_service, ConfigImportMode,
    ManagedShutdownReport, ProcessManager, SessionManager,
};
use crate::sidebar;
use crate::state::{AppState, SessionDimensions};
use crate::terminal::{self, view};
use crate::theme;
use crate::updater::UpdaterService;
use crate::workspace::{
    self, CommandDraft, EditorAction, EditorField, EditorPaneModel, EditorPanel, FolderDraft,
    ProjectDraft, SettingsDraft, SshDraft, UiPreviewDraft,
};
use gpui::{
    div, prelude::*, px, rgb, size, App, AppContext, Application, Bounds, ClipboardEntry,
    ClipboardItem, Context, FocusHandle, IntoElement, KeyDownEvent, Keystroke, Modifiers,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement, Pixels, Point,
    Render, RenderImage, ScrollWheelEvent, Styled, Subscription, TouchPhase, Window, WindowBounds,
    WindowOptions,
};
use rfd::{FileDialog, MessageButtons, MessageDialog, MessageDialogResult, MessageLevel};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const TERMINAL_TOPBAR_HEIGHT_PX: f32 = 22.0;
const STACK_GAP_PX: f32 = 4.0;
const META_TEXT_HEIGHT_PX: f32 = 0.0;
const NOTICE_HEIGHT_PX: f32 = 26.0;
const FOOTER_HEIGHT_PX: f32 = 0.0;
const APP_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
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
                        title: Some(APP_WINDOW_TITLE.into()),
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
    terminal_focus: FocusHandle,
    editor_focus: FocusHandle,
    did_focus_terminal: bool,
    focused_terminal_session_id: Option<String>,
    active_port_state: Option<ActivePortState>,
    ssh_password_prompt_state: Option<SshPasswordPromptState>,
    editor_needs_focus: bool,
    synced_session_id: Option<String>,
    last_dimensions: Option<SessionDimensions>,
    terminal_selection: Option<TerminalSelection>,
    terminal_scroll_px: Pixels,
    is_selecting_terminal: bool,
    last_terminal_mouse_report: Option<(TerminalGridPosition, Option<MouseButton>)>,
    editor_panel: Option<EditorPanel>,
    editor_active_field: Option<EditorField>,
    editor_cursor: usize,
    sidebar_context_menu: Option<sidebar::SidebarContextMenu>,
    add_project_wizard: Option<workspace::AddProjectWizard>,
    last_window_title: Option<String>,
    splash_image: Option<Arc<RenderImage>>,
    splash_fetch_in_flight: bool,
    window_subscriptions: Vec<Subscription>,
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct SshPasswordPromptState {
    session_id: String,
    fingerprint: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SshPasswordPromptMatch {
    fingerprint: String,
}

impl NativeShell {
    fn new(cx: &mut Context<Self>) -> Self {
        let session_manager = SessionManager::new();
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
            Self::spawn_updater_refresh_task(updater.clone(), cx);
        }

        let shell = Self {
            state,
            session_manager,
            process_manager,
            updater,
            startup_notice,
            terminal_notice,
            editor_notice: None,
            terminal_focus: cx.focus_handle(),
            editor_focus: cx.focus_handle(),
            did_focus_terminal: false,
            focused_terminal_session_id: None,
            active_port_state: None,
            ssh_password_prompt_state: None,
            editor_needs_focus: false,
            synced_session_id,
            last_dimensions: None,
            terminal_selection: None,
            terminal_scroll_px: px(0.0),
            is_selecting_terminal: false,
            last_terminal_mouse_report: None,
            editor_panel: None,
            editor_active_field: None,
            editor_cursor: 0,
            sidebar_context_menu: None,
            add_project_wizard: None,
            last_window_title: None,
            splash_image: None,
            splash_fetch_in_flight: false,
            window_subscriptions: Vec::new(),
        };

        Self::spawn_splash_image_fetch(cx);

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

    fn spawn_splash_image_fetch(cx: &mut Context<Self>) {
        let executor = cx.background_executor().clone();
        cx.spawn(
            move |this: gpui::WeakEntity<Self>, cx: &mut gpui::AsyncApp| {
                let mut async_cx = cx.clone();
                async move {
                    let image = executor.spawn(async move { fetch_splash_image() }).await;
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
            Self::spawn_splash_image_fetch(cx);
        }
    }

    fn spawn_updater_refresh_task(updater: UpdaterService, cx: &mut Context<Self>) {
        cx.spawn(
            move |this: gpui::WeakEntity<Self>, cx: &mut gpui::AsyncApp| {
                let background_executor = cx.background_executor().clone();
                let mut async_cx = cx.clone();
                async move {
                    let mut previous_snapshot = updater.snapshot();
                    loop {
                        background_executor.timer(Duration::from_millis(500)).await;
                        let next_snapshot = updater.snapshot();
                        if next_snapshot != previous_snapshot {
                            previous_snapshot = next_snapshot;
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

    fn save_session_state(&mut self) {
        if let Err(error) = self
            .session_manager
            .save_session(&persisted_session_state(&self.state))
        {
            self.terminal_notice = Some(format!("Failed to save session state: {error}"));
        }
    }

    fn save_config_state(&mut self) {
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

    fn perform_managed_shutdown(&mut self) -> ManagedShutdownReport {
        self.save_session_state();
        self.process_manager
            .shutdown_managed_processes(APP_SHUTDOWN_TIMEOUT)
    }

    fn handle_window_should_close(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.sync_terminal_focus(None);
        self.capture_window_bounds(window);

        if self.state.settings().minimize_to_tray {
            self.save_session_state();
            window.minimize_window();
            self.terminal_notice = Some(
                "DevManager minimized instead of closing. Disable `Minimize to tray` to quit from the window close button."
                    .to_string(),
            );
            cx.notify();
            return false;
        }

        let live_sessions = self.process_manager.live_session_count();
        if self.state.settings().confirm_on_close && live_sessions > 0 {
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

        SessionDimensions::from_available_space(
            layout.available_width,
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
        let runtime = self.process_manager.runtime_state();

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
        if matches!(self.editor_panel, Some(EditorPanel::Settings(_))) {
            self.close_editor(cx);
            return;
        }
        let settings = self.state.settings().clone();
        self.open_editor(
            EditorPanel::Settings(SettingsDraft {
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
                open_picker: None,
            }),
            cx,
        );
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

        self.state.upsert_project(project);
        self.save_config_state();
        self.save_session_state();
        if self.editor_notice.is_none() {
            self.editor_notice = Some(format!("Created project `{project_name}`"));
        }
        cx.notify();
    }

    fn wizard_pick_root_folder(&mut self, cx: &mut Context<Self>) {
        let Some(path) = rfd::FileDialog::new().pick_folder() else {
            return;
        };
        let root_path = path.to_string_lossy().to_string();
        let default_name = path
            .file_name()
            .map(|value| value.to_string_lossy().to_string())
            .unwrap_or_default();

        if let Some(wizard) = self.add_project_wizard.as_mut() {
            wizard.root_path = root_path;
            if wizard.name.trim().is_empty() {
                wizard.name = default_name;
                wizard.cursor = wizard.name.len();
            }
            wizard.selected_scripts.clear();
            wizard.selected_port_variables.clear();

            match scanner_service::scan_root(&wizard.root_path) {
                Ok(scan_entries) => {
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
                Err(error) => {
                    wizard.scan_entries.clear();
                    wizard.selected_folders.clear();
                    wizard.scan_message = Some(error);
                }
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
        let Some(path) = FileDialog::new().pick_folder() else {
            return;
        };
        let folder_path = path.to_string_lossy().to_string();
        let default_name = path
            .file_name()
            .map(|value| value.to_string_lossy().to_string())
            .unwrap_or_default();

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

        match load_folder_env_contents(&folder_path, &env_file_path) {
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
        cx.notify();
    }

    fn open_folder_external_terminal_action(&mut self, cx: &mut Context<Self>) {
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
                if draft.env_file_loaded && !draft.env_file_path.trim().is_empty() {
                    let env_file_path = std::path::Path::new(draft.folder_path.trim())
                        .join(draft.env_file_path.trim());
                    if let Err(error) =
                        env_service::write_env_text(&env_file_path, &draft.env_file_contents)
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
        if let Some(EditorPanel::Settings(draft)) = self.editor_panel.as_mut() {
            draft.open_picker = None;
        }
        let cursor = self
            .editor_panel
            .as_ref()
            .and_then(|panel| panel.text_value(field))
            .map(|value| value.chars().count())
            .unwrap_or(0);
        self.editor_active_field = Some(field);
        self.editor_cursor = cursor;
        self.focus_editor(window);
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
        let mut active_spec = self.state.active_terminal_spec();
        let active_tab = self.state.active_tab().cloned();
        let active_tab_type = active_tab.as_ref().map(|tab| tab.tab_type.clone());
        let mut active_session = None;

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
                    if self.last_dimensions != Some(dimensions)
                        && self
                            .process_manager
                            .resize_session(&active_spec.session_id, dimensions)
                            .is_ok()
                    {
                        self.last_dimensions = Some(dimensions);
                    }
                    self.terminal_notice = None;
                    active_session = self.process_manager.session_view(&active_spec.session_id);
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

                if self.last_dimensions != Some(dimensions)
                    && self
                        .process_manager
                        .resize_session(&active_spec.session_id, dimensions)
                        .is_ok()
                {
                    self.last_dimensions = Some(dimensions);
                }
                active_session = self.process_manager.active_session();
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
                        if self.last_dimensions != Some(dimensions)
                            && self
                                .process_manager
                                .resize_session(&active_spec.session_id, dimensions)
                                .is_ok()
                        {
                            self.last_dimensions = Some(dimensions);
                        }

                        self.terminal_notice = None;
                        active_session = self.process_manager.session_view(&active_spec.session_id);
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
                if self.last_dimensions != Some(dimensions)
                    && self
                        .process_manager
                        .resize_session(&active_spec.session_id, dimensions)
                        .is_ok()
                {
                    self.last_dimensions = Some(dimensions);
                }
                active_session = self.process_manager.active_session();
            }
        }

        if active_tab_type == Some(TabType::Ssh) {
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
            .and_then(|session| self.selection_snapshot(session.screen.cols));
        let runtime_controls = self.runtime_controls_model(
            active_tab_type.clone(),
            &active_spec,
            active_session.as_ref(),
            cx,
        );
        let terminal_metrics = self.terminal_render_metrics(window);
        let blocking_notice = active_session.as_ref().and_then(|session| {
            session
                .runtime
                .awaiting_external_editor
                .then_some("Save and close text editor to continue...".to_string())
        });

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
            runtime_controls,
            splash_image: self.splash_image.clone(),
        }
    }

    fn focus_terminal(&mut self, window: &mut Window) {
        let session_id = self.state.active_terminal_spec().session_id;
        self.sync_terminal_focus(Some(session_id));
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

    fn sync_active_port_state(
        &mut self,
        command_id: &str,
        port: Option<u16>,
        active_session: Option<&crate::terminal::session::TerminalSessionView>,
        cx: &mut Context<Self>,
    ) {
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

            let interval = if active_session.is_some_and(|session| session.runtime.status.is_live())
                && state.status.as_ref().is_some_and(|status| !status.in_use)
            {
                std::time::Duration::from_secs(1)
            } else {
                port_refresh_interval(active_session)
            };
            let should_refresh = state
                .last_checked_at
                .map(|checked_at| checked_at.elapsed() >= interval)
                .unwrap_or(true);
            if should_refresh && !state.refresh_in_flight {
                self.refresh_port_state(command_id.to_string(), port, cx);
            }
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
        state.refresh_in_flight = true;

        let background_executor = cx.background_executor().clone();
        cx.spawn(
            move |this: gpui::WeakEntity<Self>, cx: &mut gpui::AsyncApp| {
                let mut async_cx = cx.clone();
                async move {
                    let status = background_executor
                        .spawn(async move { ports_service::check_port_in_use(port).ok() })
                        .await;
                    let _ = this.update(&mut async_cx, |this, cx: &mut Context<'_, Self>| {
                        if let Some(state) = this.active_port_state.as_mut() {
                            if state.command_id != command_id || state.port != port {
                                return;
                            }
                            state.status = status;
                            state.last_checked_at = Some(Instant::now());
                            state.refresh_in_flight = false;
                            cx.notify();
                        }
                    });
                }
            },
        )
        .detach();
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
        let Some(session) = self.process_manager.session_view(session_id) else {
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
        cx: &mut Context<Self>,
    ) -> Option<view::TerminalRuntimeControlsModel> {
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
            });
        }

        if active_tab_type != Some(TabType::Server) {
            self.active_port_state = None;
            return None;
        }

        let (command_id, port) = {
            let lookup = self.state.find_command(&active_spec.session_id)?;
            (lookup.command.id.clone(), lookup.command.port)
        };
        self.sync_active_port_state(&command_id, port, active_session, cx);

        let status = active_session
            .map(|session| session.runtime.status)
            .unwrap_or(crate::state::SessionStatus::Stopped);
        let port_state = self
            .active_port_state
            .as_ref()
            .filter(|state| state.command_id == command_id);
        let has_port_conflict = port_state
            .and_then(|state| state.status.as_ref())
            .map(|status| !is_managed_port_owner(active_session, status))
            .unwrap_or(false);
        let probe_disagrees_with_live_session = active_session
            .is_some_and(|session| session.runtime.status.is_live())
            && port_state
                .and_then(|state| state.status.as_ref())
                .is_some_and(|status| !status.in_use);
        let port_label = port.map(|port| {
            if let Some(status) = port_state.and_then(|state| state.status.as_ref()) {
                if probe_disagrees_with_live_session {
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
                }
            } else {
                format!("port {port} • checking")
            }
        });
        let port_color = if has_port_conflict {
            theme::WARNING_TEXT
        } else if probe_disagrees_with_live_session {
            theme::TEXT_MUTED
        } else if port_state
            .and_then(|state| state.status.as_ref())
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
            can_start: !status.is_live(),
            can_stop: status.is_live(),
            can_restart: status.is_live(),
            can_clear: active_session.is_some(),
            can_kill_port: port.is_some() && has_port_conflict,
            can_open_url: port.is_some()
                && status == crate::state::SessionStatus::Running
                && !has_port_conflict,
            kill_label,
            kill_color,
            prompt_action_label: None,
            prompt_action_color: theme::PRIMARY,
        })
    }

    fn start_server_action(
        &mut self,
        command_id: &str,
        focus_started_server: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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

        if let Some(state) = self.active_port_state.as_mut() {
            if state.command_id == command_id && state.port == port {
                state.status = None;
                state.last_checked_at = None;
                state.refresh_in_flight = true;
            }
        }

        let command_id = command_id.to_string();
        let background_executor = cx.background_executor().clone();
        cx.spawn(
            move |this: gpui::WeakEntity<Self>, cx: &mut gpui::AsyncApp| {
                let mut async_cx = cx.clone();
                async move {
                    let status = background_executor
                        .spawn(async move { ports_service::check_port_in_use(port).ok() })
                        .await;
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
        cx.spawn(
            move |this: gpui::WeakEntity<Self>, cx: &mut gpui::AsyncApp| {
                let background_executor = cx.background_executor().clone();
                let mut async_cx = cx.clone();
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
        let dimensions = self.terminal_dimensions(window);
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

        let active_session = self.process_manager.active_session();
        let session_mode = active_session.as_ref().map(|session| session.screen.mode);
        let session_id = self.state.active_terminal_spec().session_id;
        if session_mode.is_some_and(|mode| mode.mouse_reporting()) {
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
                    let _ = self
                        .process_manager
                        .write_bytes_to_session(&session_id, &sequence);
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
        let active_session = self.process_manager.active_session();
        let session_mode = active_session.as_ref().map(|session| session.screen.mode);
        if session_mode.is_some_and(|mode| mode.mouse_reporting()) {
            if let Some(cell) = self.grid_position_for_mouse(event.position, window, true) {
                let report_key = (cell, event.pressed_button);
                if self.last_terminal_mouse_report != Some(report_key) {
                    if let Some(sequence) = mouse_move_report(
                        session_mode.unwrap_or_default(),
                        cell,
                        event.pressed_button,
                        event.modifiers,
                    ) {
                        let session_id = self.state.active_terminal_spec().session_id;
                        let _ = self
                            .process_manager
                            .write_bytes_to_session(&session_id, &sequence);
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

    fn handle_terminal_mouse_up(
        &mut self,
        event: &MouseUpEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let active_session = self.process_manager.active_session();
        let session_mode = active_session.as_ref().map(|session| session.screen.mode);
        if session_mode.is_some_and(|mode| mode.mouse_reporting()) {
            if let Some(cell) = self.grid_position_for_mouse(event.position, window, true) {
                if let Some(sequence) = mouse_button_report(
                    session_mode.unwrap_or_default(),
                    cell,
                    event.button,
                    event.modifiers,
                    false,
                ) {
                    let session_id = self.state.active_terminal_spec().session_id;
                    let _ = self
                        .process_manager
                        .write_bytes_to_session(&session_id, &sequence);
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

    fn handle_terminal_mouse_up_out(
        &mut self,
        event: &MouseUpEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let active_session = self.process_manager.active_session();
        let session_mode = active_session.as_ref().map(|session| session.screen.mode);
        if session_mode.is_some_and(|mode| mode.mouse_reporting()) {
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
                let session_id = self.state.active_terminal_spec().session_id;
                let _ = self
                    .process_manager
                    .write_bytes_to_session(&session_id, &sequence);
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
        self.last_terminal_mouse_report = None;
        window.prevent_default();
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
        let session_id = self.state.active_terminal_spec().session_id;
        let active_session = self.process_manager.active_session();
        let mode = active_session
            .as_ref()
            .map(|session| session.screen.mode)
            .unwrap_or_default();
        let binding_context = TerminalBindingContext {
            has_selection: active_session
                .as_ref()
                .and_then(|session| self.selection_snapshot(session.screen.cols))
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
                    let _ = self.process_manager.close_session(&session_id);
                }
                window.prevent_default();
            }
            TerminalKeyAction::CopySelection => {
                if self.copy_terminal_selection_to_clipboard(cx) {
                    window.prevent_default();
                }
            }
            TerminalKeyAction::Paste => {
                if let Some(clipboard) = cx.read_from_clipboard() {
                    match terminal_clipboard_payload(&clipboard) {
                        Some(TerminalClipboardPayload::Text(text)) => {
                            let _ = self.process_manager.paste_to_session(&session_id, &text);
                        }
                        Some(TerminalClipboardPayload::RawBytes(bytes)) => {
                            let _ = self
                                .process_manager
                                .write_bytes_to_session(&session_id, &bytes);
                        }
                        None => {}
                    }
                }
                window.prevent_default();
            }
            TerminalKeyAction::SendInput(input) => {
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
                        self.process_manager.note_server_interrupt(&session_id);
                    }
                    let _ = self.process_manager.write_to_session(&session_id, &text);
                    window.prevent_default();
                }
            }
        }
    }

    fn handle_terminal_scroll(
        &mut self,
        event: &ScrollWheelEvent,
        window: &mut Window,
        _: &mut Context<Self>,
    ) {
        let Some(delta_lines) = self.determine_terminal_scroll_lines(event, window) else {
            return;
        };

        if delta_lines == 0 {
            return;
        }

        let session_id = self.state.active_terminal_spec().session_id;
        if let Some(session) = self.process_manager.active_session() {
            if session.screen.mode.mouse_reporting() {
                if let Some(cell) = self.grid_position_for_mouse(event.position, window, true) {
                    if let Some(sequences) =
                        mouse_scroll_report(session.screen.mode, cell, delta_lines, event)
                    {
                        for sequence in sequences {
                            let _ = self
                                .process_manager
                                .write_bytes_to_session(&session_id, &sequence);
                        }
                        self.last_terminal_mouse_report = Some((cell, None));
                    }
                }
            } else if session.screen.mode.alternate_screen
                && session.screen.mode.alternate_scroll
                && !event.modifiers.shift
            {
                let sequence = alt_scroll_bytes(delta_lines);
                let _ = self
                    .process_manager
                    .write_bytes_to_session(&session_id, &sequence);
            } else {
                let _ = self
                    .process_manager
                    .scroll_session(&session_id, delta_lines);
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
        let session = self.process_manager.active_session()?;
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
        let session = self.process_manager.active_session()?;
        let bounds = self.terminal_text_bounds(window, &session)?;
        terminal_endpoint_for_mouse(position, bounds, clamp_to_terminal)
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
        let available_width = layout.available_width.max(cell_width);
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

    fn selection_snapshot(&self, screen_cols: usize) -> Option<view::TerminalSelectionSnapshot> {
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

        Some(view::TerminalSelectionSnapshot {
            start_row: start.position.row,
            start_column,
            end_row: end.position.row,
            end_column,
        })
    }

    fn selected_text(&self) -> Option<String> {
        let session = self.process_manager.active_session()?;
        let selection = self.selection_snapshot(session.screen.cols)?;
        let mut lines = Vec::new();

        for row in selection.start_row..=selection.end_row {
            let line = session.screen.lines.get(row)?;
            let characters: Vec<char> = line.iter().map(|cell| cell.character).collect();
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

            lines.push(segment);
        }

        Some(lines.join("\n"))
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
        let runtime_snapshot = self.process_manager.runtime_state();
        self.sync_window_title(window, &runtime_snapshot);
        let updater_snapshot = self.updater.snapshot();
        let editor_model = self.editor_panel.clone().map(|panel| EditorPaneModel {
            panel,
            active_field: self.editor_active_field,
            cursor: self.editor_cursor,
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
        let make_editor_action_handler =
            |action: EditorAction| -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
                Box::new(cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                    this.apply_editor_action(action.clone(), window, cx);
                }))
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
        if runtime_snapshot
            .sessions
            .values()
            .any(|s| matches!(s.ai_activity, Some(crate::state::AiActivity::Thinking)))
        {
            window.request_animation_frame();
        }

        let terminal_actions = terminal_model.as_ref().and_then(|model| {
            model.runtime_controls.as_ref().map(|controls| {
                let command_id = self.state.active_terminal_spec().session_id;
                view::TerminalPaneActions {
                    on_start_server: controls
                        .can_start
                        .then(|| make_focused_start_handler(command_id.clone())),
                    on_stop_server: controls
                        .can_stop
                        .then(|| make_stop_handler(command_id.clone())),
                    on_restart_server: controls
                        .can_restart
                        .then(|| make_restart_handler(command_id.clone())),
                    on_clear_output: controls
                        .can_clear
                        .then(|| make_clear_output_handler(command_id.clone())),
                    on_kill_port: controls
                        .can_kill_port
                        .then(|| make_kill_port_handler(command_id.clone())),
                    on_open_local_url: controls
                        .can_open_url
                        .then(|| make_open_server_url_handler(command_id.clone())),
                    on_prompt_action: controls
                        .prompt_action_label
                        .as_ref()
                        .map(|_| make_respond_to_ssh_prompt_handler(command_id)),
                }
            })
        });

        div()
            .size_full()
            .flex()
            .bg(rgb(theme::APP_BG))
            .text_color(rgb(theme::TEXT_PRIMARY))
            .child(sidebar::render_sidebar(
                &self.state,
                &runtime_snapshot,
                sidebar::SidebarActions {
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
                            .child(workspace::render_editor_surface(
                                model,
                                workspace::EditorActions {
                                    on_action: &make_editor_action_handler,
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
                        chrome::StatusBarActions {
                            on_install_update: &make_install_update_handler,
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
    let row = position.row.min(screen.lines.len().saturating_sub(1));
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
            let line = screen.lines.get(row)?;
            let (start, end) = semantic_selection_bounds(line, position.column, screen.cols);
            Some(TerminalSelection {
                anchor: endpoint_at_boundary(row, start, screen.cols),
                head: endpoint_at_boundary(row, end, screen.cols),
                moved: start != end,
                mode,
            })
        }
        TerminalSelectionMode::Lines => Some(TerminalSelection {
            anchor: endpoint_at_boundary(row, 0, screen.cols),
            head: endpoint_at_boundary(row, screen.cols, screen.cols),
            moved: screen.cols > 0,
            mode,
        }),
    }
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

fn port_refresh_interval(
    active_session: Option<&crate::terminal::session::TerminalSessionView>,
) -> std::time::Duration {
    let Some(session) = active_session else {
        return std::time::Duration::from_secs(1);
    };

    if matches!(
        session.runtime.status,
        crate::state::SessionStatus::Starting | crate::state::SessionStatus::Stopping
    ) {
        return std::time::Duration::from_secs(2);
    }

    if session
        .runtime
        .started_at
        .is_some_and(|started_at| started_at.elapsed() < std::time::Duration::from_secs(15))
    {
        return std::time::Duration::from_secs(2);
    }

    std::time::Duration::from_secs(5)
}

fn is_managed_port_owner(
    active_session: Option<&crate::terminal::session::TerminalSessionView>,
    status: &PortStatus,
) -> bool {
    let Some(pid) = status.pid else {
        return false;
    };
    let Some(session) = active_session else {
        return false;
    };

    if session.runtime.pid == Some(pid) {
        return true;
    }

    session.runtime.resources.process_ids.contains(&pid)
}

fn normalize_optional_string(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn current_window_title(state: &AppState, runtime: &crate::state::RuntimeState) -> String {
    let Some(tab) = state.active_tab() else {
        return APP_WINDOW_TITLE.to_string();
    };

    let segments = [
        window_title_project_name(tab, state),
        active_tab_live_title(tab, runtime)
            .or_else(|| Some(window_title_fallback_label(tab, state))),
        Some(APP_WINDOW_TITLE.to_string()),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();

    dedupe_adjacent_segments(segments).join(WINDOW_TITLE_SEPARATOR)
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

fn apply_text_key_to_string(
    value: &mut String,
    cursor: &mut usize,
    event: &KeyDownEvent,
    paste_text: Option<&str>,
    numeric_only: bool,
    allow_newlines: bool,
) -> bool {
    let key = event.keystroke.key.to_ascii_lowercase();
    let modifiers = event.keystroke.modifiers;
    let secondary = modifiers.control || modifiers.platform;
    let mut chars: Vec<char> = value.chars().collect();
    *cursor = (*cursor).min(chars.len());

    match key.as_str() {
        "left" => {
            *cursor = (*cursor).saturating_sub(1);
            return true;
        }
        "right" => {
            *cursor = (*cursor + 1).min(chars.len());
            return true;
        }
        "home" => {
            *cursor = 0;
            return true;
        }
        "end" => {
            *cursor = chars.len();
            return true;
        }
        "enter" => {
            if allow_newlines {
                chars.insert(*cursor, '\n');
                *cursor += 1;
                *value = chars.into_iter().collect();
                return true;
            }
            return false;
        }
        "backspace" => {
            if *cursor > 0 {
                chars.remove(*cursor - 1);
                *cursor -= 1;
                *value = chars.into_iter().collect();
                return true;
            }
            return false;
        }
        "delete" => {
            if *cursor < chars.len() {
                chars.remove(*cursor);
                *value = chars.into_iter().collect();
                return true;
            }
            return false;
        }
        _ => {}
    }

    if secondary {
        if let Some(paste_text) = paste_text {
            let filtered = filter_editor_text_input(paste_text, numeric_only, allow_newlines);
            if filtered.is_empty() {
                return false;
            }
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
    fn port_refresh_interval_is_eager_without_active_session() {
        assert_eq!(
            port_refresh_interval(None),
            std::time::Duration::from_secs(1)
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
