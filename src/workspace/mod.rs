use crate::models::{
    DefaultTerminal, DependencyStatus, MacTerminalProfile, RootScanEntry, ScanResult,
};
use crate::theme;
use crate::updater::{UpdaterSnapshot, UpdaterStage};
use gpui::{
    div, px, rgb, AnyElement, App, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    ParentElement, SharedString, Styled, Window,
};
use std::collections::{BTreeSet, HashMap};

const EDITOR_TITLE_HEIGHT_PX: f32 = 46.0;

#[derive(Debug, Clone)]
pub enum EditorPanel {
    Settings(SettingsDraft),
    Project(ProjectDraft),
    Folder(FolderDraft),
    Command(CommandDraft),
    Ssh(SshDraft),
}

impl EditorPanel {
    pub fn title(&self) -> &'static str {
        match self {
            Self::Settings(_) => "Settings",
            Self::Project(draft) => {
                if draft.existing_id.is_some() {
                    "Edit Project"
                } else {
                    "Add Project"
                }
            }
            Self::Folder(draft) => {
                if draft.existing_id.is_some() {
                    "Edit Folder"
                } else {
                    "Add Folder"
                }
            }
            Self::Command(draft) => {
                if draft.existing_id.is_some() {
                    "Edit Command"
                } else {
                    "Add Command"
                }
            }
            Self::Ssh(draft) => {
                if draft.existing_id.is_some() {
                    "Edit SSH Connection"
                } else {
                    "Add SSH Connection"
                }
            }
        }
    }

    pub fn subtitle(&self) -> &'static str {
        match self {
            Self::Settings(_) => "Click a field to type. Settings persist as you change them.",
            Self::Project(_) => "Project metadata and notes are persisted to config.json.",
            Self::Folder(_) => "Folders own command definitions and env helpers.",
            Self::Command(_) => "Args use space-separated tokens. Env uses KEY=VALUE;KEY2=VALUE2.",
            Self::Ssh(_) => "Saved SSH entries can now open native terminal sessions.",
        }
    }

    pub fn save_label(&self) -> &'static str {
        match self {
            Self::Settings(_) => "Close",
            Self::Project(draft) => {
                if draft.existing_id.is_some() {
                    "Save Project"
                } else {
                    "Create Project"
                }
            }
            Self::Folder(draft) => {
                if draft.existing_id.is_some() {
                    "Save Folder"
                } else {
                    "Create Folder"
                }
            }
            Self::Command(draft) => {
                if draft.existing_id.is_some() {
                    "Save Command"
                } else {
                    "Create Command"
                }
            }
            Self::Ssh(draft) => {
                if draft.existing_id.is_some() {
                    "Save SSH"
                } else {
                    "Create SSH"
                }
            }
        }
    }

    pub fn show_delete(&self) -> bool {
        match self {
            Self::Settings(_) => false,
            Self::Project(draft) => draft.existing_id.is_some(),
            Self::Folder(draft) => draft.existing_id.is_some(),
            Self::Command(draft) => draft.existing_id.is_some(),
            Self::Ssh(draft) => draft.existing_id.is_some(),
        }
    }

    pub fn text_value(&self, field: EditorField) -> Option<&String> {
        match (self, field) {
            (Self::Settings(draft), EditorField::Settings(SettingsField::Theme)) => {
                Some(&draft.theme)
            }
            (Self::Settings(draft), EditorField::Settings(SettingsField::LogBufferSize)) => {
                Some(&draft.log_buffer_size)
            }
            (Self::Settings(draft), EditorField::Settings(SettingsField::ClaudeCommand)) => {
                Some(&draft.claude_command)
            }
            (Self::Settings(draft), EditorField::Settings(SettingsField::CodexCommand)) => {
                Some(&draft.codex_command)
            }
            (Self::Settings(draft), EditorField::Settings(SettingsField::TerminalFontSize)) => {
                Some(&draft.terminal_font_size)
            }
            (Self::Project(draft), EditorField::Project(ProjectField::Name)) => Some(&draft.name),
            (Self::Project(draft), EditorField::Project(ProjectField::RootPath)) => {
                Some(&draft.root_path)
            }
            (Self::Project(draft), EditorField::Project(ProjectField::Color)) => Some(&draft.color),
            (Self::Project(draft), EditorField::Project(ProjectField::Notes)) => Some(&draft.notes),
            (Self::Folder(draft), EditorField::Folder(FolderField::Name)) => Some(&draft.name),
            (Self::Folder(draft), EditorField::Folder(FolderField::FolderPath)) => {
                Some(&draft.folder_path)
            }
            (Self::Folder(draft), EditorField::Folder(FolderField::EnvFilePath)) => {
                Some(&draft.env_file_path)
            }
            (Self::Folder(draft), EditorField::Folder(FolderField::PortVariable)) => {
                Some(&draft.port_variable)
            }
            (Self::Folder(draft), EditorField::Folder(FolderField::EnvContents)) => {
                Some(&draft.env_file_contents)
            }
            (Self::Command(draft), EditorField::Command(CommandField::Label)) => Some(&draft.label),
            (Self::Command(draft), EditorField::Command(CommandField::Command)) => {
                Some(&draft.command)
            }
            (Self::Command(draft), EditorField::Command(CommandField::Args)) => {
                Some(&draft.args_text)
            }
            (Self::Command(draft), EditorField::Command(CommandField::Env)) => {
                Some(&draft.env_text)
            }
            (Self::Command(draft), EditorField::Command(CommandField::Port)) => {
                Some(&draft.port_text)
            }
            (Self::Ssh(draft), EditorField::Ssh(SshField::Label)) => Some(&draft.label),
            (Self::Ssh(draft), EditorField::Ssh(SshField::Host)) => Some(&draft.host),
            (Self::Ssh(draft), EditorField::Ssh(SshField::Port)) => Some(&draft.port_text),
            (Self::Ssh(draft), EditorField::Ssh(SshField::Username)) => Some(&draft.username),
            (Self::Ssh(draft), EditorField::Ssh(SshField::Password)) => Some(&draft.password),
            _ => None,
        }
    }

    pub fn text_value_mut(&mut self, field: EditorField) -> Option<&mut String> {
        match (self, field) {
            (Self::Settings(draft), EditorField::Settings(SettingsField::Theme)) => {
                Some(&mut draft.theme)
            }
            (Self::Settings(draft), EditorField::Settings(SettingsField::LogBufferSize)) => {
                Some(&mut draft.log_buffer_size)
            }
            (Self::Settings(draft), EditorField::Settings(SettingsField::ClaudeCommand)) => {
                Some(&mut draft.claude_command)
            }
            (Self::Settings(draft), EditorField::Settings(SettingsField::CodexCommand)) => {
                Some(&mut draft.codex_command)
            }
            (Self::Settings(draft), EditorField::Settings(SettingsField::TerminalFontSize)) => {
                Some(&mut draft.terminal_font_size)
            }
            (Self::Project(draft), EditorField::Project(ProjectField::Name)) => {
                Some(&mut draft.name)
            }
            (Self::Project(draft), EditorField::Project(ProjectField::RootPath)) => {
                Some(&mut draft.root_path)
            }
            (Self::Project(draft), EditorField::Project(ProjectField::Color)) => {
                Some(&mut draft.color)
            }
            (Self::Project(draft), EditorField::Project(ProjectField::Notes)) => {
                Some(&mut draft.notes)
            }
            (Self::Folder(draft), EditorField::Folder(FolderField::Name)) => Some(&mut draft.name),
            (Self::Folder(draft), EditorField::Folder(FolderField::FolderPath)) => {
                Some(&mut draft.folder_path)
            }
            (Self::Folder(draft), EditorField::Folder(FolderField::EnvFilePath)) => {
                Some(&mut draft.env_file_path)
            }
            (Self::Folder(draft), EditorField::Folder(FolderField::PortVariable)) => {
                Some(&mut draft.port_variable)
            }
            (Self::Folder(draft), EditorField::Folder(FolderField::EnvContents)) => {
                Some(&mut draft.env_file_contents)
            }
            (Self::Command(draft), EditorField::Command(CommandField::Label)) => {
                Some(&mut draft.label)
            }
            (Self::Command(draft), EditorField::Command(CommandField::Command)) => {
                Some(&mut draft.command)
            }
            (Self::Command(draft), EditorField::Command(CommandField::Args)) => {
                Some(&mut draft.args_text)
            }
            (Self::Command(draft), EditorField::Command(CommandField::Env)) => {
                Some(&mut draft.env_text)
            }
            (Self::Command(draft), EditorField::Command(CommandField::Port)) => {
                Some(&mut draft.port_text)
            }
            (Self::Ssh(draft), EditorField::Ssh(SshField::Label)) => Some(&mut draft.label),
            (Self::Ssh(draft), EditorField::Ssh(SshField::Host)) => Some(&mut draft.host),
            (Self::Ssh(draft), EditorField::Ssh(SshField::Port)) => Some(&mut draft.port_text),
            (Self::Ssh(draft), EditorField::Ssh(SshField::Username)) => Some(&mut draft.username),
            (Self::Ssh(draft), EditorField::Ssh(SshField::Password)) => Some(&mut draft.password),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SettingsDraft {
    pub default_terminal: DefaultTerminal,
    pub mac_terminal_profile: MacTerminalProfile,
    pub theme: String,
    pub log_buffer_size: String,
    pub claude_command: String,
    pub codex_command: String,
    pub notification_sound: String,
    pub confirm_on_close: bool,
    pub minimize_to_tray: bool,
    pub restore_session_on_start: bool,
    pub terminal_font_size: String,
}

#[derive(Debug, Clone)]
pub struct ProjectDraft {
    pub existing_id: Option<String>,
    pub name: String,
    pub root_path: String,
    pub color: String,
    pub pinned: bool,
    pub save_log_files: bool,
    pub notes: String,
    pub scan_entries: Vec<RootScanEntry>,
    pub selected_folder_paths: BTreeSet<String>,
    pub selected_scripts: HashMap<String, BTreeSet<String>>,
    pub selected_port_variables: HashMap<String, Option<String>>,
    pub scan_message: Option<String>,
    pub is_scanning: bool,
}

#[derive(Debug, Clone)]
pub struct FolderDraft {
    pub project_id: String,
    pub existing_id: Option<String>,
    pub name: String,
    pub folder_path: String,
    pub env_file_path: String,
    pub env_file_contents: String,
    pub env_file_loaded: bool,
    pub port_variable: String,
    pub hidden: bool,
    pub git_branch: Option<String>,
    pub dependency_status: Option<DependencyStatus>,
    pub scan_result: Option<ScanResult>,
    pub selected_scanned_scripts: BTreeSet<String>,
    pub selected_scanned_port_variable: Option<String>,
    pub scan_message: Option<String>,
    pub is_scanning: bool,
}

#[derive(Debug, Clone)]
pub struct CommandDraft {
    pub project_id: String,
    pub folder_id: String,
    pub existing_id: Option<String>,
    pub label: String,
    pub command: String,
    pub args_text: String,
    pub env_text: String,
    pub port_text: String,
    pub auto_restart: bool,
    pub clear_logs_on_restart: bool,
}

#[derive(Debug, Clone)]
pub struct SshDraft {
    pub existing_id: Option<String>,
    pub label: String,
    pub host: String,
    pub port_text: String,
    pub username: String,
    pub password: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorField {
    Settings(SettingsField),
    Project(ProjectField),
    Folder(FolderField),
    Command(CommandField),
    Ssh(SshField),
}

impl EditorField {
    pub fn accepts_text(self) -> bool {
        true
    }

    pub fn allows_newlines(self) -> bool {
        matches!(
            self,
            Self::Project(ProjectField::Notes) | Self::Folder(FolderField::EnvContents)
        )
    }

    pub fn is_numeric(self) -> bool {
        matches!(
            self,
            Self::Settings(SettingsField::LogBufferSize | SettingsField::TerminalFontSize)
                | Self::Command(CommandField::Port)
                | Self::Ssh(SshField::Port)
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsField {
    Theme,
    LogBufferSize,
    ClaudeCommand,
    CodexCommand,
    TerminalFontSize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectField {
    Name,
    RootPath,
    Color,
    SaveLogFiles,
    Notes,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FolderField {
    Name,
    FolderPath,
    EnvFilePath,
    EnvContents,
    PortVariable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandField {
    Label,
    Command,
    Args,
    Env,
    Port,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SshField {
    Label,
    Host,
    Port,
    Username,
    Password,
}

#[derive(Debug, Clone)]
pub struct EditorPaneModel {
    pub panel: EditorPanel,
    pub active_field: Option<EditorField>,
    pub cursor: usize,
    pub notice: Option<String>,
    pub updater: UpdaterSnapshot,
}

#[derive(Debug, Clone)]
pub enum EditorAction {
    FocusField(EditorField),
    Save,
    Delete,
    Close,
    PickProjectRoot,
    ScanProjectRoot,
    ToggleProjectScanFolder(String),
    ToggleProjectScanScript {
        folder_path: String,
        script_name: String,
    },
    SelectProjectPortVariable {
        folder_path: String,
        variable: Option<String>,
    },
    PickFolderPath,
    ScanFolderPath,
    ToggleFolderScanScript(String),
    SelectFolderPortVariable(Option<String>),
    LoadFolderEnvFile,
    OpenFolderExternalTerminal,
    ExportConfig,
    ImportConfigMerge,
    ImportConfigReplace,
    CheckForUpdates,
    DownloadUpdate,
    InstallUpdate,
    CycleDefaultTerminal,
    CycleMacTerminalProfile,
    CycleNotificationSound,
    ToggleConfirmOnClose,
    ToggleMinimizeToTray,
    ToggleRestoreSession,
    ToggleProjectPinned,
    ToggleProjectSaveLogs,
    ToggleFolderHidden,
    ToggleCommandAutoRestart,
    ToggleCommandClearLogs,
}

pub struct EditorActions<'a> {
    pub on_action: &'a dyn Fn(EditorAction) -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
}

pub fn render_editor_surface(
    model: &EditorPaneModel,
    actions: EditorActions<'_>,
) -> impl IntoElement {
    let title = model.panel.title();
    let subtitle = model.panel.subtitle();
    let save_label = model.panel.save_label();
    let notice = model.notice.as_ref().map(|notice| {
        div()
            .px_3()
            .py_2()
            .rounded_md()
            .bg(rgb(theme::AGENT_ROW_BG))
            .border_1()
            .border_color(rgb(theme::BORDER_PRIMARY))
            .text_sm()
            .text_color(rgb(theme::TEXT_MUTED))
            .child(SharedString::from(notice.clone()))
    });

    let body: AnyElement = match &model.panel {
        EditorPanel::Settings(draft) => {
            render_settings_panel(draft, model, &actions).into_any_element()
        }
        EditorPanel::Project(draft) => {
            render_project_panel(draft, model, &actions).into_any_element()
        }
        EditorPanel::Folder(draft) => {
            render_folder_panel(draft, model, &actions).into_any_element()
        }
        EditorPanel::Command(draft) => {
            render_command_panel(draft, model, &actions).into_any_element()
        }
        EditorPanel::Ssh(draft) => render_ssh_panel(draft, model, &actions).into_any_element(),
    };

    let on_close = (actions.on_action)(EditorAction::Close);
    let on_save = (actions.on_action)(EditorAction::Save);
    let on_delete = model
        .panel
        .show_delete()
        .then(|| (actions.on_action)(EditorAction::Delete));

    div()
        .flex_1()
        .h_full()
        .flex()
        .items_center()
        .justify_center()
        .bg(rgb(theme::APP_BG))
        .child(
            div().w(px(820.0)).h_full().px_4().py_4().child(
                div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .gap(px(12.0))
                    .rounded_md()
                    .bg(rgb(theme::PANEL_BG))
                    .border_1()
                    .border_color(rgb(theme::BORDER_PRIMARY))
                    .overflow_hidden()
                    .child(
                        div()
                            .h(px(EDITOR_TITLE_HEIGHT_PX))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_between()
                            .px_3()
                            .bg(rgb(theme::TOPBAR_BG))
                            .border_b_1()
                            .border_color(rgb(theme::BORDER_PRIMARY))
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap(px(2.0))
                                    .child(
                                        div()
                                            .text_xs()
                                            .font_weight(gpui::FontWeight::SEMIBOLD)
                                            .child(title),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(rgb(theme::TEXT_SUBTLE))
                                            .child(subtitle),
                                    ),
                            )
                            .child(
                                div()
                                    .px_2()
                                    .py_1()
                                    .rounded_sm()
                                    .bg(rgb(theme::PANEL_HEADER_BG))
                                    .text_xs()
                                    .text_color(rgb(theme::TEXT_MUTED))
                                    .child("close")
                                    .on_mouse_down(MouseButton::Left, on_close),
                            ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .p_3()
                            .flex()
                            .flex_col()
                            .gap(px(10.0))
                            .children(notice)
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(theme::TEXT_DIM))
                                    .child("Click a field to type. Ctrl+S saves. Esc closes."),
                            )
                            .child(body)
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(8.0))
                                    .child(
                                        div()
                                            .px_3()
                                            .py(px(10.0))
                                            .rounded_sm()
                                            .bg(rgb(theme::PANEL_HEADER_BG))
                                            .border_1()
                                            .border_color(rgb(theme::BORDER_SECONDARY))
                                            .text_xs()
                                            .text_color(rgb(theme::TEXT_PRIMARY))
                                            .child(save_label)
                                            .on_mouse_down(MouseButton::Left, on_save),
                                    )
                                    .children(on_delete.map(|on_delete| {
                                        div()
                                            .px_3()
                                            .py(px(10.0))
                                            .rounded_sm()
                                            .bg(rgb(theme::PROJECT_ROW_BG))
                                            .border_1()
                                            .border_color(rgb(theme::DANGER_TEXT))
                                            .text_xs()
                                            .text_color(rgb(theme::DANGER_TEXT))
                                            .child("delete")
                                            .on_mouse_down(MouseButton::Left, on_delete)
                                    })),
                            ),
                    ),
            ),
        )
}

pub fn next_default_terminal(current: DefaultTerminal) -> DefaultTerminal {
    match current {
        DefaultTerminal::Bash => DefaultTerminal::Powershell,
        DefaultTerminal::Powershell => DefaultTerminal::Cmd,
        DefaultTerminal::Cmd => DefaultTerminal::Bash,
    }
}

pub fn default_terminal_label(value: DefaultTerminal) -> &'static str {
    match value {
        DefaultTerminal::Bash => "bash",
        DefaultTerminal::Powershell => "powershell",
        DefaultTerminal::Cmd => "cmd",
    }
}

pub fn next_mac_terminal_profile(current: MacTerminalProfile) -> MacTerminalProfile {
    match current {
        MacTerminalProfile::System => MacTerminalProfile::Zsh,
        MacTerminalProfile::Zsh => MacTerminalProfile::Bash,
        MacTerminalProfile::Bash => MacTerminalProfile::System,
    }
}

pub fn mac_terminal_profile_label(value: MacTerminalProfile) -> &'static str {
    match value {
        MacTerminalProfile::System => "system",
        MacTerminalProfile::Zsh => "zsh",
        MacTerminalProfile::Bash => "bash",
    }
}

pub fn notification_sound_options() -> &'static [&'static str] {
    &[
        "glass", "chord", "glisten", "polite", "calm", "sharp", "jinja", "cloud", "none",
    ]
}

pub fn next_notification_sound(current: &str) -> String {
    let options = notification_sound_options();
    let index = options
        .iter()
        .position(|option| *option == current)
        .unwrap_or(0);
    options[(index + 1) % options.len()].to_string()
}

fn render_settings_panel(
    draft: &SettingsDraft,
    model: &EditorPaneModel,
    actions: &EditorActions<'_>,
) -> impl IntoElement {
    let on_export = (actions.on_action)(EditorAction::ExportConfig);
    let on_import_merge = (actions.on_action)(EditorAction::ImportConfigMerge);
    let on_import_replace = (actions.on_action)(EditorAction::ImportConfigReplace);
    let on_check_updates = (actions.on_action)(EditorAction::CheckForUpdates);
    let on_cycle_terminal = (actions.on_action)(EditorAction::CycleDefaultTerminal);
    let on_cycle_mac_profile = (actions.on_action)(EditorAction::CycleMacTerminalProfile);
    let on_cycle_sound = (actions.on_action)(EditorAction::CycleNotificationSound);
    let on_download_update = matches!(model.updater.stage, UpdaterStage::UpdateAvailable)
        .then(|| (actions.on_action)(EditorAction::DownloadUpdate));
    let on_install_update = matches!(model.updater.stage, UpdaterStage::ReadyToInstall)
        .then(|| (actions.on_action)(EditorAction::InstallUpdate));
    let on_toggle_confirm = (actions.on_action)(EditorAction::ToggleConfirmOnClose);
    let on_toggle_tray = (actions.on_action)(EditorAction::ToggleMinimizeToTray);
    let on_toggle_restore = (actions.on_action)(EditorAction::ToggleRestoreSession);

    div()
        .flex()
        .flex_col()
        .gap_3()
        .child(render_choice_row(
            "Default terminal",
            default_terminal_label(draft.default_terminal.clone()),
            Some("Cycles through bash, powershell, and cmd"),
            on_cycle_terminal,
        ))
        .child(render_choice_row(
            "macOS terminal profile",
            mac_terminal_profile_label(draft.mac_terminal_profile.clone()),
            Some("Cycles through system, zsh, and bash for macOS shell launch"),
            on_cycle_mac_profile,
        ))
        .child(render_text_field(
            "Theme",
            "Persisted config theme id. The native shell currently uses the dark palette.",
            draft.theme.as_str(),
            EditorField::Settings(SettingsField::Theme),
            model,
            actions,
        ))
        .child(render_text_field(
            "Log buffer size",
            "Numeric terminal scrollback history size",
            draft.log_buffer_size.as_str(),
            EditorField::Settings(SettingsField::LogBufferSize),
            model,
            actions,
        ))
        .child(render_text_field(
            "Claude command",
            "Optional override for Claude shell startup",
            draft.claude_command.as_str(),
            EditorField::Settings(SettingsField::ClaudeCommand),
            model,
            actions,
        ))
        .child(render_text_field(
            "Codex command",
            "Optional override for Codex shell startup",
            draft.codex_command.as_str(),
            EditorField::Settings(SettingsField::CodexCommand),
            model,
            actions,
        ))
        .child(render_choice_row(
            "Notification sound",
            draft.notification_sound.as_str(),
            Some("Cycles through the archived sound ids"),
            on_cycle_sound,
        ))
        .child(render_toggle_row(
            "Confirm before closing live tabs",
            draft.confirm_on_close,
            on_toggle_confirm,
        ))
        .child(render_toggle_row(
            "Minimize to tray",
            draft.minimize_to_tray,
            on_toggle_tray,
        ))
        .child(render_toggle_row(
            "Restore session on start",
            draft.restore_session_on_start,
            on_toggle_restore,
        ))
        .child(render_text_field(
            "Terminal font size",
            "Leave blank to use the default size",
            draft.terminal_font_size.as_str(),
            EditorField::Settings(SettingsField::TerminalFontSize),
            model,
            actions,
        ))
        .child(render_choice_row(
            "Export config",
            "Save a JSON backup",
            Some("Exports projects, notes, settings, and SSH entries"),
            on_export,
        ))
        .child(render_choice_row(
            "Import config (merge)",
            "Add non-duplicate projects",
            Some("Keeps current settings and SSH entries"),
            on_import_merge,
        ))
        .child(render_choice_row(
            "Import config (replace)",
            "Replace current config",
            Some("Overwrites projects, settings, and SSH entries from the selected file"),
            on_import_replace,
        ))
        .child(render_updater_panel(
            &model.updater,
            on_check_updates,
            on_download_update,
            on_install_update,
        ))
}

fn render_project_panel(
    draft: &ProjectDraft,
    model: &EditorPaneModel,
    actions: &EditorActions<'_>,
) -> impl IntoElement {
    let on_toggle_pinned = (actions.on_action)(EditorAction::ToggleProjectPinned);
    let on_toggle_save_logs = (actions.on_action)(EditorAction::ToggleProjectSaveLogs);
    let on_pick_root = (actions.on_action)(EditorAction::PickProjectRoot);
    let on_scan_root = (actions.on_action)(EditorAction::ScanProjectRoot);

    div()
        .flex()
        .flex_col()
        .gap_3()
        .child(render_text_field(
            "Name",
            "Display name in the sidebar",
            draft.name.as_str(),
            EditorField::Project(ProjectField::Name),
            model,
            actions,
        ))
        .child(render_text_field(
            "Root path",
            "Absolute project root path",
            draft.root_path.as_str(),
            EditorField::Project(ProjectField::RootPath),
            model,
            actions,
        ))
        .children(draft.existing_id.is_none().then(|| {
            render_choice_row(
                "Pick root folder",
                if draft.root_path.is_empty() {
                    "Choose directory"
                } else {
                    draft.root_path.as_str()
                },
                Some("Opens the native folder picker for scanner-driven onboarding"),
                on_pick_root,
            )
            .into_any_element()
        }))
        .children(draft.existing_id.is_none().then(|| {
            render_choice_row(
                "Scan root folder",
                if draft.is_scanning {
                    "Scanning..."
                } else {
                    "Discover folders, scripts, and env ports"
                },
                Some(
                    "Scans up to three levels deep, skipping node_modules, target, dist, and .git",
                ),
                on_scan_root,
            )
            .into_any_element()
        }))
        .children(
            draft
                .scan_message
                .as_ref()
                .map(|message| render_notice_row(message.as_str()).into_any_element()),
        )
        .children(
            (draft.existing_id.is_none() && !draft.scan_entries.is_empty())
                .then(|| render_project_scan_panel(draft, actions).into_any_element()),
        )
        .child(render_text_field(
            "Color",
            "Optional hex accent like #6366f1",
            draft.color.as_str(),
            EditorField::Project(ProjectField::Color),
            model,
            actions,
        ))
        .child(render_multiline_field(
            "Notes",
            "Project notes stored in config.json. Enter inserts a new line.",
            draft.notes.as_str(),
            EditorField::Project(ProjectField::Notes),
            model,
            actions,
        ))
        .child(render_toggle_row(
            "Save log files",
            draft.save_log_files,
            on_toggle_save_logs,
        ))
        .child(render_toggle_row("Pinned", draft.pinned, on_toggle_pinned))
}

fn render_folder_panel(
    draft: &FolderDraft,
    model: &EditorPaneModel,
    actions: &EditorActions<'_>,
) -> impl IntoElement {
    let on_toggle_hidden = (actions.on_action)(EditorAction::ToggleFolderHidden);
    let on_pick_folder = (actions.on_action)(EditorAction::PickFolderPath);
    let on_scan_folder = (actions.on_action)(EditorAction::ScanFolderPath);
    let on_load_env = (actions.on_action)(EditorAction::LoadFolderEnvFile);
    let on_open_terminal = (actions.on_action)(EditorAction::OpenFolderExternalTerminal);

    div()
        .flex()
        .flex_col()
        .gap_3()
        .child(render_text_field(
            "Name",
            "Folder label shown in the project workspace",
            draft.name.as_str(),
            EditorField::Folder(FolderField::Name),
            model,
            actions,
        ))
        .child(render_text_field(
            "Folder path",
            "Absolute path to the folder",
            draft.folder_path.as_str(),
            EditorField::Folder(FolderField::FolderPath),
            model,
            actions,
        ))
        .child(render_choice_row(
            "Pick folder",
            if draft.folder_path.is_empty() {
                "Choose directory"
            } else {
                draft.folder_path.as_str()
            },
            Some("Opens the native folder picker and updates the folder path"),
            on_pick_folder,
        ))
        .child(render_choice_row(
            "Scan folder",
            if draft.is_scanning {
                "Scanning..."
            } else {
                "Discover scripts and env ports"
            },
            Some("Imports package.json/Cargo.toml commands and .env port variables"),
            on_scan_folder,
        ))
        .child(render_choice_row(
            "Open external terminal",
            if draft.folder_path.is_empty() {
                "Pick a folder first"
            } else {
                "Open terminal"
            },
            Some("Matches the archived app's helper for opening the current folder in a system terminal."),
            on_open_terminal,
        ))
        .children(
            draft
                .git_branch
                .as_ref()
                .map(|branch| render_info_row("Git branch", branch.as_str(), Some("Read directly from .git/HEAD"))),
        )
        .children(draft.dependency_status.as_ref().map(|status| {
            render_info_row(
                "Dependencies",
                status.status.as_str(),
                Some(status.message.as_str()),
            )
        }))
        .children(
            draft
                .scan_message
                .as_ref()
                .map(|message| render_notice_row(message.as_str()).into_any_element()),
        )
        .children(draft.scan_result.as_ref().map(|scan_result| {
            render_folder_scan_panel(draft, scan_result, actions).into_any_element()
        }))
        .child(render_text_field(
            "Env file path",
            "Optional relative .env path",
            draft.env_file_path.as_str(),
            EditorField::Folder(FolderField::EnvFilePath),
            model,
            actions,
        ))
        .child(render_choice_row(
            "Load env file",
            if draft.env_file_loaded {
                "Reload env contents"
            } else {
                "Load env contents"
            },
            Some("Reads the configured env file so you can edit and save it inline."),
            on_load_env,
        ))
        .children((draft.env_file_loaded || !draft.env_file_contents.is_empty()).then(|| {
            render_multiline_field(
                "Env file contents",
                "Raw .env editor. Comments and blank lines are preserved on save.",
                draft.env_file_contents.as_str(),
                EditorField::Folder(FolderField::EnvContents),
                model,
                actions,
            )
            .into_any_element()
        }))
        .child(render_text_field(
            "Port variable",
            "Optional env var used to derive a server port",
            draft.port_variable.as_str(),
            EditorField::Folder(FolderField::PortVariable),
            model,
            actions,
        ))
        .child(render_toggle_row("Hidden", draft.hidden, on_toggle_hidden))
}

fn render_command_panel(
    draft: &CommandDraft,
    model: &EditorPaneModel,
    actions: &EditorActions<'_>,
) -> impl IntoElement {
    let on_toggle_restart = (actions.on_action)(EditorAction::ToggleCommandAutoRestart);
    let on_toggle_clear_logs = (actions.on_action)(EditorAction::ToggleCommandClearLogs);

    div()
        .flex()
        .flex_col()
        .gap_3()
        .child(render_text_field(
            "Label",
            "Sidebar display label",
            draft.label.as_str(),
            EditorField::Command(CommandField::Label),
            model,
            actions,
        ))
        .child(render_text_field(
            "Command",
            "Program to execute",
            draft.command.as_str(),
            EditorField::Command(CommandField::Command),
            model,
            actions,
        ))
        .child(render_text_field(
            "Args",
            "Space-separated args",
            draft.args_text.as_str(),
            EditorField::Command(CommandField::Args),
            model,
            actions,
        ))
        .child(render_text_field(
            "Env",
            "KEY=VALUE;KEY2=VALUE2",
            draft.env_text.as_str(),
            EditorField::Command(CommandField::Env),
            model,
            actions,
        ))
        .child(render_text_field(
            "Port",
            "Optional numeric port",
            draft.port_text.as_str(),
            EditorField::Command(CommandField::Port),
            model,
            actions,
        ))
        .child(render_toggle_row(
            "Auto restart",
            draft.auto_restart,
            on_toggle_restart,
        ))
        .child(render_toggle_row(
            "Clear logs on restart",
            draft.clear_logs_on_restart,
            on_toggle_clear_logs,
        ))
}

fn render_ssh_panel(
    draft: &SshDraft,
    model: &EditorPaneModel,
    actions: &EditorActions<'_>,
) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap_3()
        .child(render_text_field(
            "Label",
            "Display name for the SSH target",
            draft.label.as_str(),
            EditorField::Ssh(SshField::Label),
            model,
            actions,
        ))
        .child(render_text_field(
            "Host",
            "Hostname or IP address",
            draft.host.as_str(),
            EditorField::Ssh(SshField::Host),
            model,
            actions,
        ))
        .child(render_text_field(
            "Port",
            "Defaults to 22",
            draft.port_text.as_str(),
            EditorField::Ssh(SshField::Port),
            model,
            actions,
        ))
        .child(render_text_field(
            "Username",
            "Remote user name",
            draft.username.as_str(),
            EditorField::Ssh(SshField::Username),
            model,
            actions,
        ))
        .child(render_text_field(
            "Password",
            "Optional saved password",
            draft.password.as_str(),
            EditorField::Ssh(SshField::Password),
            model,
            actions,
        ))
}

fn render_text_field(
    label: &str,
    hint: &str,
    value: &str,
    field: EditorField,
    model: &EditorPaneModel,
    actions: &EditorActions<'_>,
) -> impl IntoElement {
    let focused = model.active_field == Some(field);
    let display_value = if focused {
        display_text_with_cursor(value, model.cursor)
    } else if value.is_empty() {
        "[empty]".to_string()
    } else {
        value.to_string()
    };

    let on_focus = (actions.on_action)(EditorAction::FocusField(field));

    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_xs()
                .text_color(rgb(theme::TEXT_MUTED))
                .child(SharedString::from(label.to_string())),
        )
        .child(
            div()
                .px_3()
                .py_2()
                .rounded_md()
                .bg(rgb(if focused {
                    theme::PROJECT_ROW_BG
                } else {
                    theme::PANEL_HEADER_BG
                }))
                .border_1()
                .border_color(rgb(if focused {
                    theme::AI_DOT
                } else {
                    theme::BORDER_SECONDARY
                }))
                .text_sm()
                .text_color(rgb(if value.is_empty() && !focused {
                    theme::TEXT_SUBTLE
                } else {
                    theme::TEXT_PRIMARY
                }))
                .child(SharedString::from(display_value))
                .on_mouse_down(MouseButton::Left, on_focus),
        )
        .child(
            div()
                .text_xs()
                .text_color(rgb(theme::TEXT_SUBTLE))
                .child(SharedString::from(hint.to_string())),
        )
}

fn render_multiline_field(
    label: &str,
    hint: &str,
    value: &str,
    field: EditorField,
    model: &EditorPaneModel,
    actions: &EditorActions<'_>,
) -> impl IntoElement {
    let focused = model.active_field == Some(field);
    let display_value = if focused {
        display_text_with_cursor(value, model.cursor)
    } else if value.is_empty() {
        "[empty]".to_string()
    } else {
        value.to_string()
    };

    let on_focus = (actions.on_action)(EditorAction::FocusField(field));

    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_xs()
                .text_color(rgb(theme::TEXT_MUTED))
                .child(SharedString::from(label.to_string())),
        )
        .child(
            div()
                .h(px(140.0))
                .px_3()
                .py_3()
                .rounded_md()
                .bg(rgb(if focused {
                    theme::PROJECT_ROW_BG
                } else {
                    theme::PANEL_HEADER_BG
                }))
                .border_1()
                .border_color(rgb(if focused {
                    theme::AI_DOT
                } else {
                    theme::BORDER_SECONDARY
                }))
                .text_sm()
                .text_color(rgb(if value.is_empty() && !focused {
                    theme::TEXT_SUBTLE
                } else {
                    theme::TEXT_PRIMARY
                }))
                .child(SharedString::from(display_value))
                .on_mouse_down(MouseButton::Left, on_focus),
        )
        .child(
            div()
                .text_xs()
                .text_color(rgb(theme::TEXT_SUBTLE))
                .child(SharedString::from(hint.to_string())),
        )
}

fn render_choice_row(
    label: &str,
    value: &str,
    hint: Option<&str>,
    on_click: Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_xs()
                .text_color(rgb(theme::TEXT_MUTED))
                .child(SharedString::from(label.to_string())),
        )
        .child(
            div()
                .px_3()
                .py_2()
                .rounded_md()
                .bg(rgb(theme::PANEL_HEADER_BG))
                .border_1()
                .border_color(rgb(theme::BORDER_SECONDARY))
                .text_sm()
                .text_color(rgb(theme::TEXT_PRIMARY))
                .child(SharedString::from(value.to_string()))
                .on_mouse_down(MouseButton::Left, on_click),
        )
        .children(hint.map(|hint| {
            div()
                .text_xs()
                .text_color(rgb(theme::TEXT_SUBTLE))
                .child(SharedString::from(hint.to_string()))
        }))
}

fn render_toggle_row(
    label: &str,
    value: bool,
    on_click: Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
) -> impl IntoElement {
    render_choice_row(
        label,
        if value { "on" } else { "off" },
        Some("Click to toggle"),
        on_click,
    )
}

fn render_notice_row(message: &str) -> impl IntoElement {
    div()
        .px_3()
        .py_2()
        .rounded_md()
        .bg(rgb(theme::AGENT_ROW_BG))
        .border_1()
        .border_color(rgb(theme::BORDER_PRIMARY))
        .text_sm()
        .text_color(rgb(theme::TEXT_MUTED))
        .child(SharedString::from(message.to_string()))
}

fn render_project_scan_panel(
    draft: &ProjectDraft,
    actions: &EditorActions<'_>,
) -> impl IntoElement {
    let summary = format!("{} discovered folder(s)", draft.scan_entries.len());

    div()
        .flex()
        .flex_col()
        .gap_2()
        .child(render_info_row(
            "Discovered folders",
            summary.as_str(),
            Some(
                "Toggle folders to include them in the new project. Selected scripts become commands and selected env ports seed folder defaults.",
            ),
        ))
        .children(
            draft
                .scan_entries
                .iter()
                .map(|entry| render_project_scan_entry(entry, draft, actions).into_any_element()),
        )
}

fn render_project_scan_entry(
    entry: &RootScanEntry,
    draft: &ProjectDraft,
    actions: &EditorActions<'_>,
) -> impl IntoElement {
    let selected = draft.selected_folder_paths.contains(&entry.path);
    let selected_scripts = draft.selected_scripts.get(&entry.path);
    let selected_script_count = selected_scripts.map(|scripts| scripts.len()).unwrap_or(0);
    let selected_port_variable = draft
        .selected_port_variables
        .get(&entry.path)
        .cloned()
        .flatten();
    let detail = format!(
        "{} | {} script(s) | {} port var(s){}",
        project_type_label(&entry.project_type),
        entry.scripts.len(),
        entry.ports.len(),
        if entry.has_env { " | env file" } else { "" }
    );
    let on_toggle_folder =
        (actions.on_action)(EditorAction::ToggleProjectScanFolder(entry.path.clone()));

    div()
        .flex()
        .flex_col()
        .gap_2()
        .p_2()
        .rounded_md()
        .bg(rgb(theme::PANEL_HEADER_BG))
        .border_1()
        .border_color(rgb(theme::BORDER_SECONDARY))
        .child(render_selection_row(
            entry.name.clone(),
            Some(detail),
            selected,
            on_toggle_folder,
        ))
        .children((!entry.scripts.is_empty()).then(|| {
            let script_summary = format!("{selected_script_count} selected");
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(theme::TEXT_MUTED))
                        .child(SharedString::from(format!("Scripts ({script_summary})"))),
                )
                .children(entry.scripts.iter().map(|script| {
                    let is_selected = selected_scripts
                        .map(|scripts| scripts.contains(&script.name))
                        .unwrap_or(false);
                    let on_toggle_script =
                        (actions.on_action)(EditorAction::ToggleProjectScanScript {
                            folder_path: entry.path.clone(),
                            script_name: script.name.clone(),
                        });
                    render_selection_row(
                        script.name.clone(),
                        Some(script.command.clone()),
                        is_selected,
                        on_toggle_script,
                    )
                    .into_any_element()
                }))
                .into_any_element()
        }))
        .children((!entry.ports.is_empty()).then(|| {
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(theme::TEXT_MUTED))
                        .child("Port variable"),
                )
                .child(render_selection_row(
                    "None".to_string(),
                    Some("Leave the folder without a default port variable".to_string()),
                    selected_port_variable.is_none(),
                    (actions.on_action)(EditorAction::SelectProjectPortVariable {
                        folder_path: entry.path.clone(),
                        variable: None,
                    }),
                ))
                .children(entry.ports.iter().map(|port| {
                    let on_select_port =
                        (actions.on_action)(EditorAction::SelectProjectPortVariable {
                            folder_path: entry.path.clone(),
                            variable: Some(port.variable.clone()),
                        });
                    render_selection_row(
                        format!("{} = {}", port.variable, port.port),
                        Some(port.source.clone()),
                        selected_port_variable.as_deref() == Some(port.variable.as_str()),
                        on_select_port,
                    )
                    .into_any_element()
                }))
                .into_any_element()
        }))
}

fn render_folder_scan_panel(
    draft: &FolderDraft,
    scan_result: &ScanResult,
    actions: &EditorActions<'_>,
) -> impl IntoElement {
    let script_summary = format!(
        "{} discovered script(s), {} selected",
        scan_result.scripts.len(),
        draft.selected_scanned_scripts.len()
    );

    div()
        .flex()
        .flex_col()
        .gap_2()
        .child(render_info_row(
            "Scan results",
            script_summary.as_str(),
            Some("Selected scripts will be created for new folders and merged into existing folders when they are not already present."),
        ))
        .children((!scan_result.scripts.is_empty()).then(|| {
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(theme::TEXT_MUTED))
                        .child("Scripts"),
                )
                .children(scan_result.scripts.iter().map(|script| {
                    let on_toggle_script =
                        (actions.on_action)(EditorAction::ToggleFolderScanScript(script.name.clone()));
                    render_selection_row(
                        script.name.clone(),
                        Some(script.command.clone()),
                        draft.selected_scanned_scripts.contains(&script.name),
                        on_toggle_script,
                    )
                    .into_any_element()
                }))
                .into_any_element()
        }))
        .children((!scan_result.ports.is_empty()).then(|| {
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(theme::TEXT_MUTED))
                        .child("Port variable"),
                )
                .child(render_selection_row(
                    "None".to_string(),
                    Some("Do not bind a default port variable".to_string()),
                    draft.selected_scanned_port_variable.is_none(),
                    (actions.on_action)(EditorAction::SelectFolderPortVariable(None)),
                ))
                .children(scan_result.ports.iter().map(|port| {
                    let on_select_port =
                        (actions.on_action)(EditorAction::SelectFolderPortVariable(Some(
                            port.variable.clone(),
                        )));
                    render_selection_row(
                        format!("{} = {}", port.variable, port.port),
                        Some(port.source.clone()),
                        draft.selected_scanned_port_variable.as_deref()
                            == Some(port.variable.as_str()),
                        on_select_port,
                    )
                    .into_any_element()
                }))
                .into_any_element()
        }))
}

fn render_selection_row(
    label: String,
    detail: Option<String>,
    selected: bool,
    on_click: Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
) -> impl IntoElement {
    div()
        .px_3()
        .py_2()
        .rounded_md()
        .bg(rgb(if selected {
            theme::PROJECT_ROW_BG
        } else {
            theme::PANEL_BG
        }))
        .border_1()
        .border_color(rgb(if selected {
            theme::AI_DOT
        } else {
            theme::BORDER_SECONDARY
        }))
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap(px(10.0))
                .child(
                    div()
                        .flex_1()
                        .flex()
                        .flex_col()
                        .gap(px(2.0))
                        .child(
                            div()
                                .text_sm()
                                .text_color(rgb(theme::TEXT_PRIMARY))
                                .child(SharedString::from(label)),
                        )
                        .children(detail.map(|detail| {
                            div()
                                .text_xs()
                                .text_color(rgb(theme::TEXT_SUBTLE))
                                .child(SharedString::from(detail))
                        })),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(if selected {
                            theme::AI_DOT
                        } else {
                            theme::TEXT_MUTED
                        }))
                        .child(if selected { "selected" } else { "available" }),
                ),
        )
        .on_mouse_down(MouseButton::Left, on_click)
}

fn project_type_label(value: &str) -> &'static str {
    match value {
        "rust" => "Rust",
        "both" => "Node + Rust",
        _ => "Node",
    }
}

fn render_updater_panel(
    updater: &UpdaterSnapshot,
    on_check: Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
    on_download: Option<Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>>,
    on_install: Option<Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>>,
) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap_3()
        .child(render_info_row(
            "App version",
            updater.current_version.as_str(),
            Some(
                "Packaged builds compiled with updater metadata can check GitHub-hosted releases.",
            ),
        ))
        .child(render_info_row(
            "Updater status",
            updater_stage_label(&updater.stage),
            Some(updater.detail.as_str()),
        ))
        .children(updater.target_version.as_ref().map(|version| {
            render_info_row(
                "Latest available version",
                version.as_str(),
                Some("This version comes from the signed latest.json manifest."),
            )
        }))
        .children(updater.endpoints.first().map(|endpoint| {
            render_info_row(
                "Manifest endpoint",
                endpoint.as_str(),
                Some("The updater checks this URL for a signed release manifest."),
            )
        }))
        .child(render_choice_row(
            "Check for updates",
            "Check now",
            Some("Queries the configured manifest URL in the background."),
            on_check,
        ))
        .children(on_download.map(|on_download| {
            render_choice_row(
                "Download update",
                "Download now",
                Some("Downloads and verifies the signed installer bundle."),
                on_download,
            )
        }))
        .children(on_install.map(|on_install| {
            render_choice_row(
                "Restart to update",
                "Install and close DevManager",
                Some("Launches the installer and closes the current app to finish the update."),
                on_install,
            )
        }))
        .children(updater.release_notes.as_ref().map(|notes| {
            render_info_row(
                "Release notes",
                notes.as_str(),
                Some("Release notes from the signed manifest / GitHub release."),
            )
        }))
}

fn render_info_row(label: &str, value: &str, hint: Option<&str>) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_xs()
                .text_color(rgb(theme::TEXT_MUTED))
                .child(SharedString::from(label.to_string())),
        )
        .child(
            div()
                .px_3()
                .py_2()
                .rounded_md()
                .bg(rgb(theme::PANEL_HEADER_BG))
                .border_1()
                .border_color(rgb(theme::BORDER_SECONDARY))
                .text_sm()
                .text_color(rgb(theme::TEXT_PRIMARY))
                .child(SharedString::from(value.to_string())),
        )
        .children(hint.map(|hint| {
            div()
                .text_xs()
                .text_color(rgb(theme::TEXT_SUBTLE))
                .child(SharedString::from(hint.to_string()))
        }))
}

fn updater_stage_label(stage: &UpdaterStage) -> &'static str {
    match stage {
        UpdaterStage::Disabled => "disabled",
        UpdaterStage::Idle => "idle",
        UpdaterStage::Checking => "checking",
        UpdaterStage::UpToDate => "up to date",
        UpdaterStage::UpdateAvailable => "update available",
        UpdaterStage::Downloading => "downloading",
        UpdaterStage::ReadyToInstall => "ready to install",
        UpdaterStage::Installing => "installing",
        UpdaterStage::Error => "error",
    }
}

fn display_text_with_cursor(value: &str, cursor: usize) -> String {
    let chars: Vec<char> = value.chars().collect();
    let cursor = cursor.min(chars.len());
    let mut display = String::new();
    for (index, character) in chars.iter().enumerate() {
        if index == cursor {
            display.push('|');
        }
        display.push(*character);
    }
    if cursor == chars.len() {
        display.push('|');
    }
    if display.is_empty() {
        display.push('|');
    }
    display
}
