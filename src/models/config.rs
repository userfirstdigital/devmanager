use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub const CURRENT_CONFIG_VERSION: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, rename_all = "camelCase")]
pub struct AppConfig {
    pub version: u32,
    pub projects: Vec<Project>,
    pub settings: Settings,
    pub ssh_connections: Vec<SSHConnection>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            version: CURRENT_CONFIG_VERSION,
            projects: Vec::new(),
            settings: Settings::default(),
            ssh_connections: Vec::new(),
        }
    }
}

impl AppConfig {
    pub fn migrate(mut self) -> Self {
        if self.version < CURRENT_CONFIG_VERSION {
            self.version = CURRENT_CONFIG_VERSION;
        }

        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct Project {
    pub id: String,
    pub name: String,
    pub root_path: String,
    pub folders: Vec<ProjectFolder>,
    pub color: Option<String>,
    pub pinned: Option<bool>,
    pub notes: Option<String>,
    pub save_log_files: Option<bool>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct ProjectFolder {
    pub id: String,
    pub name: String,
    pub folder_path: String,
    pub commands: Vec<RunCommand>,
    pub env_file_path: Option<String>,
    pub port_variable: Option<String>,
    pub hidden: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct RunCommand {
    pub id: String,
    pub label: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: Option<HashMap<String, String>>,
    pub port: Option<u16>,
    pub auto_restart: Option<bool>,
    pub clear_logs_on_restart: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct SSHConnection {
    pub id: String,
    pub label: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DefaultTerminal {
    Bash,
    Powershell,
    Cmd,
}

impl Default for DefaultTerminal {
    fn default() -> Self {
        Self::Bash
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MacTerminalProfile {
    System,
    Zsh,
    Bash,
}

impl Default for MacTerminalProfile {
    fn default() -> Self {
        Self::System
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    pub theme: String,
    pub log_buffer_size: u32,
    pub confirm_on_close: bool,
    pub minimize_to_tray: bool,
    pub restore_session_on_start: Option<bool>,
    pub default_terminal: DefaultTerminal,
    pub mac_terminal_profile: Option<MacTerminalProfile>,
    pub claude_command: Option<String>,
    pub codex_command: Option<String>,
    pub notification_sound: Option<String>,
    pub terminal_font_size: Option<u16>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: "dark".to_string(),
            log_buffer_size: 10_000,
            confirm_on_close: true,
            minimize_to_tray: false,
            restore_session_on_start: Some(true),
            default_terminal: DefaultTerminal::Bash,
            mac_terminal_profile: Some(MacTerminalProfile::System),
            claude_command: None,
            codex_command: None,
            notification_sound: None,
            terminal_font_size: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TabType {
    Server,
    Claude,
    Codex,
    Ssh,
}

impl Default for TabType {
    fn default() -> Self {
        Self::Server
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct SessionState {
    pub open_tabs: Vec<SessionTab>,
    pub active_tab_id: Option<String>,
    pub sidebar_collapsed: bool,
}

impl SessionState {
    pub fn normalize(mut self) -> Self {
        for tab in &mut self.open_tabs {
            if matches!(tab.tab_type, TabType::Server) && tab.pty_session_id.is_none() {
                tab.pty_session_id = tab.command_id.clone();
            }
        }

        if self
            .active_tab_id
            .as_ref()
            .is_none_or(|active| !self.open_tabs.iter().any(|tab| &tab.id == active))
        {
            self.active_tab_id = self.open_tabs.first().map(|tab| tab.id.clone());
        }

        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct SessionTab {
    pub id: String,
    #[serde(rename = "type")]
    pub tab_type: TabType,
    pub project_id: String,
    pub command_id: Option<String>,
    pub pty_session_id: Option<String>,
    pub label: Option<String>,
    pub ssh_connection_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct ScanResult {
    pub scripts: Vec<ScannedScript>,
    pub ports: Vec<ScannedPort>,
    pub has_package_json: bool,
    pub has_cargo_toml: bool,
    pub has_env_file: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct ScannedScript {
    pub name: String,
    pub command: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct ScannedPort {
    pub variable: String,
    pub port: u16,
    pub source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct RootScanEntry {
    pub path: String,
    pub name: String,
    #[serde(rename = "hasEnv")]
    pub has_env: bool,
    #[serde(rename = "projectType")]
    pub project_type: String,
    pub scripts: Vec<ScannedScript>,
    pub ports: Vec<ScannedPort>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct DependencyStatus {
    pub status: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct PortConflict {
    pub port: u16,
    pub commands: Vec<PortConflictEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct PortConflictEntry {
    pub project_name: String,
    pub command_label: String,
    pub command_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct PortStatus {
    pub port: u16,
    pub in_use: bool,
    pub pid: Option<u32>,
    pub process_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum EnvEntryType {
    Variable,
    Comment,
    Blank,
}

impl Default for EnvEntryType {
    fn default() -> Self {
        Self::Blank
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct EnvEntry {
    #[serde(rename = "type")]
    pub entry_type: EnvEntryType,
    pub key: Option<String>,
    pub value: Option<String>,
    pub raw: String,
}
