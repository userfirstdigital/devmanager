use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub version: u32,
    pub projects: Vec<Project>,
    pub settings: Settings,
    #[serde(rename = "sshConnections")]
    pub ssh_connections: Vec<SSHConnection>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub name: String,
    #[serde(rename = "rootPath")]
    pub root_path: String,
    pub folders: Vec<ProjectFolder>,
    pub color: Option<String>,
    pub pinned: Option<bool>,
    pub notes: Option<String>,
    #[serde(rename = "saveLogFiles")]
    pub save_log_files: Option<bool>,
    #[serde(rename = "createdAt")]
    pub created_at: String,
    #[serde(rename = "updatedAt")]
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectFolder {
    pub id: String,
    pub name: String,
    #[serde(rename = "folderPath")]
    pub folder_path: String,
    pub commands: Vec<RunCommand>,
    #[serde(rename = "envFilePath")]
    pub env_file_path: Option<String>,
    #[serde(rename = "portVariable")]
    pub port_variable: Option<String>,
    pub hidden: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunCommand {
    pub id: String,
    pub label: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: Option<std::collections::HashMap<String, String>>,
    pub port: Option<u16>,
    #[serde(rename = "autoRestart")]
    pub auto_restart: Option<bool>,
    #[serde(rename = "clearLogsOnRestart")]
    pub clear_logs_on_restart: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SSHConnection {
    pub id: String,
    pub label: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    pub theme: String,
    #[serde(rename = "logBufferSize")]
    pub log_buffer_size: u32,
    #[serde(rename = "confirmOnClose")]
    pub confirm_on_close: bool,
    #[serde(rename = "minimizeToTray")]
    pub minimize_to_tray: bool,
    #[serde(rename = "restoreSessionOnStart")]
    pub restore_session_on_start: Option<bool>,
    #[serde(rename = "defaultTerminal")]
    pub default_terminal: String,
    #[serde(rename = "claudeCommand")]
    pub claude_command: Option<String>,
    #[serde(rename = "codexCommand")]
    pub codex_command: Option<String>,
    #[serde(rename = "notificationSound")]
    pub notification_sound: Option<String>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            version: 2,
            projects: Vec::new(),
            settings: Settings::default(),
            ssh_connections: Vec::new(),
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: "dark".to_string(),
            log_buffer_size: 10000,
            confirm_on_close: true,
            minimize_to_tray: false,
            restore_session_on_start: Some(true),
            default_terminal: "bash".to_string(),
            claude_command: None,
            codex_command: None,
            notification_sound: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionState {
    #[serde(rename = "openTabs")]
    pub open_tabs: Vec<SessionTab>,
    #[serde(rename = "activeTabId")]
    pub active_tab_id: Option<String>,
    #[serde(rename = "sidebarCollapsed")]
    pub sidebar_collapsed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionTab {
    pub id: String,
    #[serde(rename = "type")]
    pub tab_type: String,
    #[serde(rename = "projectId")]
    pub project_id: String,
    #[serde(rename = "commandId")]
    pub command_id: Option<String>,
    #[serde(rename = "ptySessionId")]
    pub pty_session_id: Option<String>,
    #[serde(rename = "label")]
    pub label: Option<String>,
    #[serde(rename = "sshConnectionId")]
    pub ssh_connection_id: Option<String>,
}

impl Default for SessionState {
    fn default() -> Self {
        Self {
            open_tabs: Vec::new(),
            active_tab_id: None,
            sidebar_collapsed: false,
        }
    }
}

// Scanner types
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanResult {
    pub scripts: Vec<ScannedScript>,
    pub ports: Vec<ScannedPort>,
    pub has_package_json: bool,
    pub has_cargo_toml: bool,
    pub has_env_file: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScannedScript {
    pub name: String,
    pub command: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScannedPort {
    pub variable: String,
    pub port: u16,
    pub source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DependencyStatus {
    pub status: String, // "missing" | "outdated" | "ok"
    pub message: String,
}

// Root scanner types
#[derive(Debug, Clone, Serialize, Deserialize)]
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

// Resource types
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessTreeInfo {
    pub command_id: String,
    pub processes: Vec<ChildProcessInfo>,
    pub total_memory_mb: f64,
    pub total_cpu_percent: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChildProcessInfo {
    pub pid: u32,
    pub name: String,
    pub memory_mb: f64,
    pub cpu_percent: f32,
}

// Port types
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortConflict {
    pub port: u16,
    pub commands: Vec<PortConflictEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortConflictEntry {
    pub project_name: String,
    pub command_label: String,
    pub command_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortStatus {
    pub port: u16,
    pub in_use: bool,
    pub pid: Option<u32>,
    pub process_name: Option<String>,
}

// Env types
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvEntry {
    #[serde(rename = "type")]
    pub entry_type: String, // "variable" | "comment" | "blank"
    pub key: Option<String>,
    pub value: Option<String>,
    pub raw: String,
}
