mod chrome;

use crate::models::{AppConfig, Project, ProjectFolder, RunCommand, SSHConnection, TabType};
use crate::services::{pid_file, ConfigImportMode, ProcessManager, SessionManager};
use crate::sidebar;
use crate::state::{AppState, SessionDimensions};
use crate::terminal::view;
use crate::theme;
use crate::updater::UpdaterService;
use crate::workspace::{
    self, CommandDraft, EditorAction, EditorField, EditorPaneModel, EditorPanel, FolderDraft,
    ProjectDraft, SettingsDraft, SshDraft,
};
use gpui::{
    div, font, prelude::*, px, rgb, size, App, AppContext, Application, Bounds, ClipboardItem,
    Context, FocusHandle, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, ParentElement, Pixels, Point, Render, ScrollWheelEvent, Styled, Window,
    WindowBounds, WindowOptions,
};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const CONTENT_PADDING_PX: f32 = 4.0;
const TERMINAL_TOPBAR_HEIGHT_PX: f32 = 28.0;
const TERMINAL_CARD_PADDING_PX: f32 = 4.0;
const TERMINAL_INNER_PADDING_PX: f32 = 8.0;
const STACK_GAP_PX: f32 = 4.0;
const META_TEXT_HEIGHT_PX: f32 = 0.0;
const NOTICE_HEIGHT_PX: f32 = 26.0;
const FOOTER_HEIGHT_PX: f32 = 0.0;

static EDITOR_ID_COUNTER: AtomicU64 = AtomicU64::new(1);

pub fn run() {
    Application::new().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1440.0), px(920.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some("DevManager".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |_, cx| cx.new(NativeShell::new),
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
    editor_needs_focus: bool,
    synced_session_id: Option<String>,
    last_dimensions: Option<SessionDimensions>,
    terminal_selection: Option<TerminalSelection>,
    is_selecting_terminal: bool,
    editor_panel: Option<EditorPanel>,
    editor_active_field: Option<EditorField>,
    editor_cursor: usize,
    pending_close_tab_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct TerminalGridPosition {
    row: usize,
    column: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TerminalSelection {
    anchor: TerminalGridPosition,
    head: TerminalGridPosition,
    moved: bool,
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
        process_manager.set_notification_sound(state.config.settings.notification_sound.clone());
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
        } else {
            let recovered = process_manager.reconcile_saved_server_tabs(&mut state);
            let ai_restore =
                process_manager.restore_ai_tabs(&mut state, SessionDimensions::default());
            let ssh_restore = process_manager.restore_ssh_tabs(&mut state);
            let mut restore_notes = Vec::new();
            if recovered > 0 {
                restore_notes.push(format!("recovered {recovered} server tab(s)"));
            }
            if ai_restore.relaunched > 0 {
                restore_notes.push(format!("relaunched {} AI tab(s)", ai_restore.relaunched));
            }
            if ai_restore.reattached > ai_restore.relaunched {
                restore_notes.push(format!(
                    "re-attached {} AI tab(s)",
                    ai_restore.reattached.saturating_sub(ai_restore.relaunched)
                ));
            }
            if ssh_restore.reattached > 0 || ssh_restore.recovered > 0 {
                restore_notes.push(format!(
                    "re-attached {} SSH tab(s)",
                    ssh_restore.reattached + ssh_restore.recovered
                ));
            }
            if ssh_restore.disconnected > 0 {
                restore_notes.push(format!(
                    "left {} SSH tab(s) disconnected",
                    ssh_restore.disconnected
                ));
            }
            if !restore_notes.is_empty() {
                terminal_notice = Some(restore_notes.join(", "));
            }
        }

        let active_spec = state.active_terminal_spec();
        let active_tab_type = state.active_tab().map(|tab| tab.tab_type.clone());

        let synced_session_id = match active_tab_type {
            Some(TabType::Server) => {
                process_manager.set_active_session(active_spec.session_id.clone());
                Some(active_spec.session_id)
            }
            Some(TabType::Claude) | Some(TabType::Codex) => {
                process_manager.set_active_session(active_spec.session_id.clone());
                Some(active_spec.session_id)
            }
            Some(TabType::Ssh) => {
                let runtime = process_manager.runtime_state();
                let live_session = state
                    .active_tab()
                    .and_then(|tab| tab.pty_session_id.as_deref())
                    .and_then(|session_id| runtime.sessions.get(session_id))
                    .map(|session| {
                        session.status.is_live()
                            && matches!(session.session_kind, crate::state::SessionKind::Ssh)
                    })
                    .unwrap_or(false);
                if live_session {
                    process_manager.set_active_session(active_spec.session_id.clone());
                    Some(active_spec.session_id)
                } else {
                    terminal_notice = terminal_notice.or_else(|| {
                        Some("SSH session is disconnected. Connect from the sidebar.".to_string())
                    });
                    None
                }
            }
            _ => {
                if let Err(error) = process_manager.spawn_shell_session(
                    active_spec.session_id.clone(),
                    &active_spec.cwd,
                    SessionDimensions::default(),
                    Some(state.settings().default_terminal.clone()),
                ) {
                    terminal_notice =
                        Some(format!("Failed to start initial shell session: {error}"));
                } else {
                    process_manager.set_active_session(active_spec.session_id.clone());
                }
                Some(active_spec.session_id)
            }
        };

        let _ = session_manager.save_session(&state.session_state());
        if updater.is_configured() {
            let _ = updater.check_for_updates();
        }

        Self {
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
            editor_needs_focus: false,
            synced_session_id,
            last_dimensions: None,
            terminal_selection: None,
            is_selecting_terminal: false,
            editor_panel: None,
            editor_active_field: None,
            editor_cursor: 0,
            pending_close_tab_id: None,
        }
    }

    fn save_session_state(&mut self) {
        if let Err(error) = self
            .session_manager
            .save_session(&self.state.session_state())
        {
            self.terminal_notice = Some(format!("Failed to save session state: {error}"));
        }
    }

    fn save_config_state(&mut self) {
        if let Err(error) = self.session_manager.save_config(&self.state.config) {
            self.editor_notice = Some(format!("Failed to save config: {error}"));
        } else {
            self.process_manager
                .set_notification_sound(self.state.config.settings.notification_sound.clone());
        }
    }

    fn sidebar_width(&self) -> f32 {
        sidebar::sidebar_width_px(self.state.sidebar_collapsed)
    }

    fn terminal_dimensions(&self, window: &Window) -> SessionDimensions {
        let viewport = window.viewport_size();
        let viewport_width: f32 = viewport.width.into();
        let viewport_height: f32 = viewport.height.into();
        let text_system = window.text_system();
        let font = font(".ZedMono");
        let font_id = text_system.resolve_font(&font);
        let cell_width = text_system
            .ch_width(font_id, px(self.terminal_font_size()))
            .map(f32::from)
            .unwrap_or(8.0)
            .max(6.0);
        let available_width = (viewport_width
            - self.sidebar_width()
            - (CONTENT_PADDING_PX * 2.0)
            - (TERMINAL_CARD_PADDING_PX * 2.0)
            - TERMINAL_INNER_PADDING_PX)
            .max(320.0);
        let available_height = (viewport_height
            - chrome::STATUS_BAR_HEIGHT_PX
            - TERMINAL_TOPBAR_HEIGHT_PX
            - (CONTENT_PADDING_PX * 2.0)
            - (TERMINAL_CARD_PADDING_PX * 2.0)
            - TERMINAL_INNER_PADDING_PX)
            .max(160.0);

        SessionDimensions::from_available_space(
            available_width,
            available_height,
            cell_width,
            self.terminal_line_height(),
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
        if self.should_confirm_tab_close(&tab)
            && self.pending_close_tab_id.as_deref() != Some(tab_id)
        {
            self.pending_close_tab_id = Some(tab_id.to_string());
            self.terminal_notice = Some(format!(
                "Press close again to stop {}.",
                self.state.tab_label(&tab)
            ));
            cx.notify();
            return;
        }
        self.pending_close_tab_id = None;

        match tab.tab_type {
            TabType::Server => {
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
                let closed_sessions = self.process_manager.close_all_live_sessions();
                self.save_session_state();
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

    fn terminal_line_height(&self) -> f32 {
        view::terminal_line_height(self.terminal_font_size())
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
        self.editor_panel = None;
        self.editor_active_field = None;
        self.editor_cursor = 0;
        self.editor_notice = None;
        self.editor_needs_focus = false;
        self.did_focus_terminal = false;
        cx.notify();
    }

    fn focus_editor(&mut self, window: &mut Window) {
        window.focus(&self.editor_focus);
        self.editor_needs_focus = false;
    }

    fn open_settings_action(&mut self, cx: &mut Context<Self>) {
        let settings = self.state.settings().clone();
        self.open_editor(
            EditorPanel::Settings(SettingsDraft {
                default_terminal: settings.default_terminal,
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
            }),
            cx,
        );
    }

    fn open_add_project_action(&mut self, cx: &mut Context<Self>) {
        self.open_editor(
            EditorPanel::Project(ProjectDraft {
                existing_id: None,
                name: String::new(),
                root_path: String::new(),
                color: String::new(),
                pinned: false,
                notes: String::new(),
            }),
            cx,
        );
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
        self.open_editor(
            EditorPanel::Folder(FolderDraft {
                project_id: project_id.to_string(),
                existing_id: None,
                name: String::new(),
                folder_path: root_path,
                env_file_path: String::new(),
                port_variable: String::new(),
                hidden: false,
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
            self.open_editor(
                EditorPanel::Folder(FolderDraft {
                    project_id: lookup.project.id.clone(),
                    existing_id: Some(lookup.folder.id.clone()),
                    name: lookup.folder.name.clone(),
                    folder_path: lookup.folder.folder_path.clone(),
                    env_file_path: lookup.folder.env_file_path.clone().unwrap_or_default(),
                    port_variable: lookup.folder.port_variable.clone().unwrap_or_default(),
                    hidden: lookup.folder.hidden.unwrap_or(false),
                }),
                cx,
            );
        } else {
            self.editor_notice = Some(format!("Unknown folder `{folder_id}`"));
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
            Ok(value) => value,
            Err(error) => {
                self.editor_notice = Some(error);
                cx.notify();
                return;
            }
        };

        self.state.update_settings(settings);
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
                let timestamp = current_timestamp_string();
                let project = Project {
                    id: draft
                        .existing_id
                        .clone()
                        .unwrap_or_else(|| next_entity_id("project")),
                    name: draft.name.trim().to_string(),
                    root_path: draft.root_path.trim().to_string(),
                    folders: existing
                        .as_ref()
                        .map(|project| project.folders.clone())
                        .unwrap_or_default(),
                    color: normalize_optional_string(&draft.color),
                    pinned: Some(draft.pinned),
                    notes: normalize_optional_string(&draft.notes),
                    save_log_files: existing.as_ref().and_then(|project| project.save_log_files),
                    created_at: existing
                        .as_ref()
                        .map(|project| project.created_at.clone())
                        .unwrap_or_else(|| timestamp.clone()),
                    updated_at: timestamp,
                };
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
                let folder = ProjectFolder {
                    id: draft
                        .existing_id
                        .clone()
                        .unwrap_or_else(|| next_entity_id("folder")),
                    name: draft.name.trim().to_string(),
                    folder_path: draft.folder_path.trim().to_string(),
                    commands: existing
                        .as_ref()
                        .map(|folder| folder.commands.clone())
                        .unwrap_or_default(),
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
            EditorAction::CycleDefaultTerminal => {
                if let Some(EditorPanel::Settings(draft)) = self.editor_panel.as_mut() {
                    draft.default_terminal =
                        workspace::next_default_terminal(draft.default_terminal.clone());
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
            EditorAction::ToggleConfirmOnClose => {
                if let Some(EditorPanel::Settings(draft)) = self.editor_panel.as_mut() {
                    draft.confirm_on_close = !draft.confirm_on_close;
                    self.apply_settings_draft(cx);
                }
            }
            EditorAction::ToggleMinimizeToTray => {
                if let Some(EditorPanel::Settings(draft)) = self.editor_panel.as_mut() {
                    draft.minimize_to_tray = !draft.minimize_to_tray;
                    self.apply_settings_draft(cx);
                }
            }
            EditorAction::ToggleRestoreSession => {
                if let Some(EditorPanel::Settings(draft)) = self.editor_panel.as_mut() {
                    draft.restore_session_on_start = !draft.restore_session_on_start;
                    self.apply_settings_draft(cx);
                }
            }
            EditorAction::ToggleProjectPinned => {
                if let Some(EditorPanel::Project(draft)) = self.editor_panel.as_mut() {
                    draft.pinned = !draft.pinned;
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
        self.focus_editor(window);
    }

    fn handle_editor_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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

    fn sync_terminal_session(&mut self, window: &mut Window) -> view::TerminalPaneModel {
        let mut active_spec = self.state.active_terminal_spec();
        let active_tab = self.state.active_tab().cloned();
        let active_tab_type = active_tab.as_ref().map(|tab| tab.tab_type.clone());
        let mut active_session = None;

        match active_tab_type {
            Some(TabType::Server) => {
                self.process_manager
                    .set_active_session(active_spec.session_id.clone());
                if self.synced_session_id.as_deref() != Some(active_spec.session_id.as_str()) {
                    self.synced_session_id = Some(active_spec.session_id.clone());
                    self.last_dimensions = None;
                }

                let session_live = self
                    .process_manager
                    .runtime_state()
                    .sessions
                    .get(&active_spec.session_id)
                    .map(|session| session.status.is_live())
                    .unwrap_or(false);
                if session_live {
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
                    active_session = self.process_manager.active_session();
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

        if !self.did_focus_terminal {
            window.focus(&self.terminal_focus);
            self.did_focus_terminal = true;
        }

        let selection = active_session
            .as_ref()
            .and_then(|session| self.selection_snapshot(session.screen.cols));

        view::TerminalPaneModel {
            active_project: self
                .state
                .active_project()
                .map(|project| project.name.clone())
                .unwrap_or_else(|| "No project selected".to_string()),
            session_label: active_spec.display_label,
            active_tab_type,
            session: active_session,
            startup_notice: self
                .startup_notice
                .clone()
                .or_else(|| self.terminal_notice.clone()),
            debug_enabled: self.process_manager.debug_enabled(),
            font_size: self.terminal_font_size(),
            line_height: self.terminal_line_height(),
            selection,
        }
    }

    fn focus_terminal(&mut self, window: &mut Window) {
        window.focus(&self.terminal_focus);
        self.did_focus_terminal = true;
    }

    fn start_server_action(
        &mut self,
        command_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let dimensions = self.terminal_dimensions(window);
        match self
            .process_manager
            .start_server(&mut self.state, command_id, dimensions)
        {
            Ok(()) => {
                self.synced_session_id = Some(command_id.to_string());
                self.terminal_notice = None;
                self.save_session_state();
            }
            Err(error) => {
                self.terminal_notice = Some(format!("Failed to start server: {error}"));
            }
        }
        cx.notify();
    }

    fn stop_server_action(&mut self, command_id: &str, cx: &mut Context<Self>) {
        if let Err(error) = self.process_manager.stop_server(command_id) {
            self.terminal_notice = Some(format!("Failed to stop server: {error}"));
        }
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
            self.save_session_state();
            self.synced_session_id = self.state.active_terminal_spec().session_id.into();
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
        _: &mut Context<Self>,
    ) {
        self.focus_terminal(window);

        let Some(cell) = self.grid_position_for_mouse(event.position, window) else {
            self.terminal_selection = None;
            self.is_selecting_terminal = false;
            return;
        };

        if event.modifiers.shift {
            if let Some(selection) = self.terminal_selection.as_mut() {
                selection.head = cell;
                selection.moved = true;
            } else {
                self.terminal_selection = Some(TerminalSelection {
                    anchor: cell,
                    head: cell,
                    moved: false,
                });
            }
        } else {
            self.terminal_selection = Some(TerminalSelection {
                anchor: cell,
                head: cell,
                moved: false,
            });
        }

        self.is_selecting_terminal = true;
        window.prevent_default();
    }

    fn handle_terminal_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.is_selecting_terminal || !event.dragging() {
            return;
        }

        let Some(cell) = self.grid_position_for_mouse(event.position, window) else {
            return;
        };

        if let Some(selection) = self.terminal_selection.as_mut() {
            if selection.head != cell {
                selection.head = cell;
                selection.moved = true;
                cx.notify();
            }
        }
    }

    fn handle_terminal_mouse_up(
        &mut self,
        _: &MouseUpEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.finish_terminal_selection(window, cx);
    }

    fn handle_terminal_mouse_up_out(
        &mut self,
        _: &MouseUpEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.finish_terminal_selection(window, cx);
    }

    fn finish_terminal_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(selection) = self.terminal_selection {
            if !selection.moved {
                self.terminal_selection = None;
                cx.notify();
            }
        }
        self.is_selecting_terminal = false;
        window.prevent_default();
    }

    fn handle_terminal_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let session_id = self.state.active_terminal_spec().session_id;
        let action = translate_key_event(event);

        match action {
            TerminalKeyAction::Ignore => {}
            TerminalKeyAction::CloseSession => {
                if let Some(tab) = self.state.active_tab().cloned() {
                    self.close_tab_action(&tab.id, cx);
                } else {
                    let _ = self.process_manager.close_session(&session_id);
                }
                window.prevent_default();
            }
            TerminalKeyAction::CopySelection => {
                if let Some(text) = self.selected_text() {
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                    window.prevent_default();
                }
            }
            TerminalKeyAction::Paste => {
                if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                    let _ = self.process_manager.paste_to_session(&session_id, &text);
                }
                window.prevent_default();
            }
            TerminalKeyAction::Write(text) => {
                let _ = self.process_manager.write_to_session(&session_id, &text);
                window.prevent_default();
            }
        }
    }

    fn handle_terminal_scroll(
        &mut self,
        event: &ScrollWheelEvent,
        window: &mut Window,
        _: &mut Context<Self>,
    ) {
        let delta = event.delta.pixel_delta(px(self.terminal_line_height()));
        let delta_lines = {
            let y: f32 = delta.y.into();
            (y / self.terminal_line_height()).round() as i32
        };

        if delta_lines != 0 {
            let session_id = self.state.active_terminal_spec().session_id;
            let _ = self
                .process_manager
                .scroll_session(&session_id, delta_lines);
            window.prevent_default();
        }
    }

    fn grid_position_for_mouse(
        &self,
        position: Point<Pixels>,
        window: &Window,
    ) -> Option<TerminalGridPosition> {
        let session = self.process_manager.active_session()?;
        let bounds = self.terminal_text_bounds(window, &session)?;
        let x: f32 = position.x.into();
        let y: f32 = position.y.into();

        if x < bounds.left
            || y < bounds.top
            || x >= bounds.left + bounds.width
            || y >= bounds.top + bounds.height
        {
            return None;
        }

        let column = (((x - bounds.left) / bounds.cell_width).floor() as usize)
            .min(bounds.cols.saturating_sub(1));
        let row = (((y - bounds.top) / bounds.row_height).floor() as usize)
            .min(bounds.rows.saturating_sub(1));

        Some(TerminalGridPosition { row, column })
    }

    fn terminal_text_bounds(
        &self,
        window: &Window,
        session: &crate::terminal::session::TerminalSessionView,
    ) -> Option<TerminalTextBounds> {
        let mut rows = session.screen.rows.max(1);
        let mut cols = session.screen.cols.max(1);
        let cell_width = f32::from(session.runtime.dimensions.cell_width.max(1));
        let row_height = self.terminal_line_height();
        let mut top = TERMINAL_TOPBAR_HEIGHT_PX + CONTENT_PADDING_PX + TERMINAL_CARD_PADDING_PX;

        if self.startup_notice.is_some() || self.terminal_notice.is_some() {
            top += NOTICE_HEIGHT_PX + STACK_GAP_PX;
        }
        if session.runtime.exit.is_some() {
            top += NOTICE_HEIGHT_PX + STACK_GAP_PX;
        }

        top += TERMINAL_INNER_PADDING_PX;

        let viewport = window.viewport_size();
        let viewport_width: f32 = viewport.width.into();
        let viewport_height: f32 = viewport.height.into();
        let left = self.sidebar_width()
            + CONTENT_PADDING_PX
            + TERMINAL_CARD_PADDING_PX
            + TERMINAL_INNER_PADDING_PX;

        if viewport_width <= left || viewport_height <= top {
            return None;
        }

        let right_padding =
            CONTENT_PADDING_PX + TERMINAL_CARD_PADDING_PX + TERMINAL_INNER_PADDING_PX;
        let bottom_padding = chrome::STATUS_BAR_HEIGHT_PX
            + CONTENT_PADDING_PX
            + TERMINAL_CARD_PADDING_PX
            + TERMINAL_INNER_PADDING_PX
            + FOOTER_HEIGHT_PX
            + if self.process_manager.debug_enabled() {
                META_TEXT_HEIGHT_PX + STACK_GAP_PX
            } else {
                0.0
            };

        let available_width = (viewport_width - left - right_padding).max(cell_width);
        let available_height = (viewport_height - top - bottom_padding).max(row_height);
        cols = cols.min((available_width / cell_width).floor().max(1.0) as usize);
        rows = rows.min((available_height / row_height).floor().max(1.0) as usize);
        let width = cols as f32 * cell_width;
        let height = rows as f32 * row_height;

        Some(TerminalTextBounds {
            left,
            top,
            width,
            height,
            cell_width,
            row_height,
            rows,
            cols,
        })
    }

    fn selection_snapshot(&self, screen_cols: usize) -> Option<view::TerminalSelectionSnapshot> {
        let selection = self.terminal_selection?;
        if !selection.moved {
            return None;
        }

        let (start, end) = ordered_selection(selection.anchor, selection.head);

        Some(view::TerminalSelectionSnapshot {
            start_row: start.row,
            start_column: start.column,
            end_row: end.row,
            end_column: (end.column + 1).min(screen_cols),
        })
    }

    fn selected_text(&self) -> Option<String> {
        let session = self.process_manager.active_session()?;
        let selection = self.selection_snapshot(session.screen.cols)?;
        let mut lines = Vec::new();

        for row in selection.start_row..=selection.end_row {
            let line = session.screen.lines.get(row)?;
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

            let mut segment: String = characters[start..end]
                .iter()
                .map(|character| {
                    if *character == '\u{00a0}' {
                        ' '
                    } else {
                        *character
                    }
                })
                .collect();

            while segment.ends_with(' ') {
                segment.pop();
            }

            lines.push(segment);
        }

        Some(lines.join("\n"))
    }
}

impl Render for NativeShell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let render_started = Instant::now();
        let runtime_snapshot = self.process_manager.runtime_state();
        let updater_snapshot = self.updater.snapshot();
        let editor_model = self.editor_panel.clone().map(|panel| EditorPaneModel {
            panel,
            active_field: self.editor_active_field,
            cursor: self.editor_cursor,
            notice: self.editor_notice.clone(),
            updater: updater_snapshot.clone(),
        });
        let terminal_model = if editor_model.is_none() {
            Some(self.sync_terminal_session(window))
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
                    this.open_edit_project_action(&project_id, cx);
                }))
            };
        let make_project_notes_handler =
            |project_id: String| -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
                Box::new(cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                    this.open_project_notes_action(&project_id, cx);
                }))
            };
        let make_delete_project_handler =
            |project_id: String| -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
                Box::new(cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                    this.delete_project_action(&project_id, cx);
                }))
            };
        let make_add_folder_handler =
            |project_id: String| -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
                Box::new(cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                    this.open_add_folder_action(&project_id, cx);
                }))
            };
        let make_edit_folder_handler =
            |project_id: String,
             folder_id: String|
             -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
                Box::new(cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                    this.open_edit_folder_action(&project_id, &folder_id, cx);
                }))
            };
        let make_delete_folder_handler =
            |project_id: String,
             folder_id: String|
             -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
                Box::new(cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                    this.delete_folder_action(&project_id, &folder_id, cx);
                }))
            };
        let make_add_command_handler =
            |project_id: String,
             folder_id: String|
             -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
                Box::new(cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                    this.open_add_command_action(&project_id, &folder_id, cx);
                }))
            };
        let make_edit_command_handler =
            |command_id: String| -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
                Box::new(cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                    this.open_edit_command_action(&command_id, cx);
                }))
            };
        let make_delete_command_handler =
            |command_id: String| -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
                Box::new(cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                    this.delete_command_action(&command_id, cx);
                }))
            };
        let make_add_ssh_handler = || -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
            Box::new(cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                this.open_add_ssh_action(cx);
            }))
        };
        let make_edit_ssh_handler =
            |connection_id: String| -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
                Box::new(cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                    this.open_edit_ssh_action(&connection_id, cx);
                }))
            };
        let make_delete_ssh_handler =
            |connection_id: String| -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
                Box::new(cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
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
                    this.connect_ssh_action(&connection_id, window, cx);
                }))
            };
        let make_disconnect_ssh_handler =
            |connection_id: String| -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
                Box::new(cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                    this.disconnect_ssh_action(&connection_id, cx);
                }))
            };
        let make_restart_ssh_handler =
            |connection_id: String| -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
                Box::new(cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                    this.restart_ssh_action(&connection_id, window, cx);
                }))
            };
        let make_start_handler =
            |command_id: String| -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
                Box::new(cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                    this.start_server_action(&command_id, window, cx);
                }))
            };
        let make_stop_handler =
            |command_id: String| -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
                Box::new(cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                    this.stop_server_action(&command_id, cx);
                }))
            };
        let make_restart_handler =
            |command_id: String| -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
                Box::new(cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                    this.restart_server_action(&command_id, window, cx);
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
                .map(|session| session.runtime.status.is_live())
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
                },
            ))
            .child(
                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .child(if let Some(model) = editor_model.as_ref() {
                        if self.editor_needs_focus {
                            self.focus_editor(window);
                        }

                        div()
                            .flex_1()
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
                            .on_mouse_move(cx.listener(Self::handle_terminal_mouse_move))
                            .on_mouse_up(
                                MouseButton::Left,
                                cx.listener(Self::handle_terminal_mouse_up),
                            )
                            .on_mouse_up_out(
                                MouseButton::Left,
                                cx.listener(Self::handle_terminal_mouse_up_out),
                            )
                            .on_key_down(cx.listener(Self::handle_terminal_key))
                            .on_scroll_wheel(cx.listener(Self::handle_terminal_scroll))
                            .child(view::render_terminal_surface(model))
                    })
                    .child(chrome::render_status_bar(
                        &runtime_snapshot,
                        &updater_snapshot,
                        chrome::StatusBarActions {
                            on_install_update: &make_install_update_handler,
                        },
                    )),
            )
    }
}

enum TerminalKeyAction {
    Ignore,
    Write(String),
    Paste,
    CopySelection,
    CloseSession,
}

fn ordered_selection(
    anchor: TerminalGridPosition,
    head: TerminalGridPosition,
) -> (TerminalGridPosition, TerminalGridPosition) {
    if anchor <= head {
        (anchor, head)
    } else {
        (head, anchor)
    }
}

fn translate_key_event(event: &KeyDownEvent) -> TerminalKeyAction {
    let key = event.keystroke.key.to_ascii_lowercase();
    let modifiers = event.keystroke.modifiers;

    let secondary = modifiers.control || modifiers.platform;
    if secondary && modifiers.shift && key == "w" {
        return TerminalKeyAction::CloseSession;
    }
    if secondary && modifiers.shift && key == "c" {
        return TerminalKeyAction::CopySelection;
    }
    if secondary && key == "v" {
        return TerminalKeyAction::Paste;
    }

    if modifiers.control && !modifiers.alt && !modifiers.platform && key.len() == 1 {
        if let Some(control_char) = control_character(&key) {
            return TerminalKeyAction::Write(control_char.to_string());
        }
    }

    if let Some(sequence) = special_key_sequence(&key) {
        return TerminalKeyAction::Write(sequence.to_string());
    }

    if key == "space" {
        if modifiers.alt && !secondary {
            return TerminalKeyAction::Write("\u{1b} ".to_string());
        }
        if !secondary && !modifiers.alt {
            return TerminalKeyAction::Write(" ".to_string());
        }
    }

    if let Some(text) = event.keystroke.key_char.clone() {
        if modifiers.alt && !secondary {
            return TerminalKeyAction::Write(format!("\u{1b}{text}"));
        }
        if !secondary || modifiers.shift {
            return TerminalKeyAction::Write(text);
        }
    }

    TerminalKeyAction::Ignore
}

fn control_character(key: &str) -> Option<char> {
    let byte = key.as_bytes().first().copied()?;
    if byte.is_ascii_alphabetic() {
        Some((byte.to_ascii_lowercase() & 0x1f) as char)
    } else {
        None
    }
}

fn special_key_sequence(key: &str) -> Option<&'static str> {
    match key {
        "enter" => Some("\r"),
        "tab" => Some("\t"),
        "backspace" => Some("\u{7f}"),
        "escape" => Some("\u{1b}"),
        "up" => Some("\u{1b}[A"),
        "down" => Some("\u{1b}[B"),
        "right" => Some("\u{1b}[C"),
        "left" => Some("\u{1b}[D"),
        "home" => Some("\u{1b}[H"),
        "end" => Some("\u{1b}[F"),
        "pageup" => Some("\u{1b}[5~"),
        "pagedown" => Some("\u{1b}[6~"),
        "delete" => Some("\u{1b}[3~"),
        _ => None,
    }
}

fn normalize_optional_string(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
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
