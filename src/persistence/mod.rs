use crate::models::{AppConfig, SessionState, Settings, CURRENT_CONFIG_VERSION};
use serde::Serialize;
use serde_json::{Map, Value};
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::fs;
use std::path::{Path, PathBuf};

const APP_CONFIG_DIR: &str = "com.userfirst.devmanager";
const APP_PROFILE_ENV: &str = "DEVMANAGER_PROFILE";
const APP_INSTANCE_LABEL_ENV: &str = "DEVMANAGER_INSTANCE_LABEL";
const CONFIG_FILE_NAME: &str = "config.json";
const SESSION_FILE_NAME: &str = "session.json";

#[derive(Debug, Clone)]
pub struct WorkspaceSnapshot {
    pub config: AppConfig,
    pub session: SessionState,
}

impl Default for WorkspaceSnapshot {
    fn default() -> Self {
        Self {
            config: AppConfig::default(),
            session: SessionState::default(),
        }
    }
}

#[derive(Debug)]
pub enum PersistenceError {
    ConfigDirectoryUnavailable,
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    Parse {
        path: PathBuf,
        source: serde_json::Error,
    },
}

impl Display for PersistenceError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ConfigDirectoryUnavailable => {
                write!(f, "could not determine the user config directory")
            }
            Self::Io { path, source } => {
                write!(f, "failed to read or write {}: {}", path.display(), source)
            }
            Self::Parse { path, source } => {
                write!(f, "failed to parse {}: {}", path.display(), source)
            }
        }
    }
}

impl Error for PersistenceError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ConfigDirectoryUnavailable => None,
            Self::Io { source, .. } => Some(source),
            Self::Parse { source, .. } => Some(source),
        }
    }
}

pub type Result<T> = std::result::Result<T, PersistenceError>;

pub fn app_config_dir() -> Result<PathBuf> {
    dirs::config_dir()
        .map(|path| path.join(app_config_dir_name()))
        .ok_or(PersistenceError::ConfigDirectoryUnavailable)
}

pub fn app_display_name() -> String {
    match app_instance_label() {
        Some(label) => format!("DevManager [{label}]"),
        None => "DevManager".to_string(),
    }
}

pub fn app_instance_label() -> Option<String> {
    sanitize_instance_label(std::env::var(APP_INSTANCE_LABEL_ENV).ok())
}

pub fn app_instance_profile() -> Option<String> {
    sanitize_scope_segment(std::env::var(APP_PROFILE_ENV).ok())
}

pub fn runtime_session_scope() -> String {
    app_instance_profile().unwrap_or_else(|| format!("pid-{:x}", std::process::id()))
}

fn app_config_dir_name() -> String {
    match app_instance_profile() {
        Some(profile) => format!("{APP_CONFIG_DIR}-{profile}"),
        None => APP_CONFIG_DIR.to_string(),
    }
}

fn sanitize_scope_segment(raw: Option<String>) -> Option<String> {
    let raw = raw?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }

    let sanitized: String = trimmed
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let sanitized = sanitized.trim_matches('-').to_string();
    if sanitized.is_empty() {
        None
    } else {
        Some(sanitized)
    }
}

fn sanitize_instance_label(raw: Option<String>) -> Option<String> {
    let raw = raw?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }

    let sanitized: String = trimmed
        .chars()
        .filter(|ch| !ch.is_control())
        .take(32)
        .collect();
    let sanitized = sanitized.trim().to_string();
    if sanitized.is_empty() {
        None
    } else {
        Some(sanitized)
    }
}

pub fn config_path() -> Result<PathBuf> {
    Ok(app_config_dir()?.join(CONFIG_FILE_NAME))
}

pub fn session_path() -> Result<PathBuf> {
    Ok(app_config_dir()?.join(SESSION_FILE_NAME))
}

pub fn load_workspace() -> Result<WorkspaceSnapshot> {
    let config_path = config_path()?;
    let session_path = session_path()?;
    load_workspace_from_paths(&config_path, &session_path)
}

pub fn load_config() -> Result<AppConfig> {
    let path = config_path()?;
    load_config_from_path(&path)
}

pub fn load_session() -> Result<SessionState> {
    let path = session_path()?;
    load_session_from_path(&path)
}

pub fn save_config(config: &AppConfig) -> Result<()> {
    let path = config_path()?;
    save_config_to_path(&path, config)
}

pub fn save_session(session: &SessionState) -> Result<()> {
    let path = session_path()?;
    save_session_to_path(&path, session)
}

pub fn load_config_from_path(path: &Path) -> Result<AppConfig> {
    load_json_file(path, AppConfig::default(), load_config_from_str)
}

pub fn load_session_from_path(path: &Path) -> Result<SessionState> {
    if !path.exists() {
        return Ok(SessionState::default());
    }

    let contents = fs::read_to_string(path).map_err(|source| PersistenceError::Io {
        path: path.to_path_buf(),
        source,
    })?;

    match load_session_from_str(&contents) {
        Ok(session) => Ok(session),
        Err(_) => {
            let _ = fs::remove_file(path);
            Ok(SessionState::default())
        }
    }
}

pub fn save_config_to_path(path: &Path, config: &AppConfig) -> Result<()> {
    write_json_file(path, config)
}

pub fn save_session_to_path(path: &Path, session: &SessionState) -> Result<()> {
    write_json_file(path, session)
}

pub fn load_config_from_str(contents: &str) -> std::result::Result<AppConfig, serde_json::Error> {
    let value: Value = serde_json::from_str(contents)?;
    let migrated = migrate_config_value(value);
    let config: AppConfig = serde_json::from_value(migrated)?;

    Ok(config.migrate())
}

pub fn load_session_from_str(
    contents: &str,
) -> std::result::Result<SessionState, serde_json::Error> {
    let value: Value = serde_json::from_str(contents)?;
    let migrated = migrate_session_value(value);
    let session: SessionState = serde_json::from_value(migrated)?;

    Ok(session.normalize())
}

fn load_json_file<T>(
    path: &Path,
    default_value: T,
    parser: fn(&str) -> std::result::Result<T, serde_json::Error>,
) -> Result<T> {
    if !path.exists() {
        return Ok(default_value);
    }

    let contents = fs::read_to_string(path).map_err(|source| PersistenceError::Io {
        path: path.to_path_buf(),
        source,
    })?;

    parser(&contents).map_err(|source| PersistenceError::Parse {
        path: path.to_path_buf(),
        source,
    })
}

fn write_json_file<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| PersistenceError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }

    let json = serde_json::to_string_pretty(value).map_err(|source| PersistenceError::Parse {
        path: path.to_path_buf(),
        source,
    })?;

    let temp_path = path.with_extension("json.tmp");
    fs::write(&temp_path, json).map_err(|source| PersistenceError::Io {
        path: temp_path.clone(),
        source,
    })?;
    fs::rename(&temp_path, path).map_err(|source| PersistenceError::Io {
        path: path.to_path_buf(),
        source,
    })?;

    Ok(())
}

fn load_workspace_from_paths(config_path: &Path, session_path: &Path) -> Result<WorkspaceSnapshot> {
    Ok(WorkspaceSnapshot {
        config: load_config_from_path(config_path)?,
        session: load_session_from_path(session_path)?,
    })
}

fn migrate_config_value(mut value: Value) -> Value {
    let Some(root) = value.as_object_mut() else {
        return value;
    };

    insert_if_missing(root, "version", Value::from(CURRENT_CONFIG_VERSION));
    insert_if_missing(root, "projects", Value::Array(Vec::new()));
    insert_if_missing(root, "sshConnections", Value::Array(Vec::new()));

    if !root.contains_key("settings") {
        root.insert("settings".to_string(), default_settings_value());
    } else if let Some(settings) = root.get_mut("settings").and_then(Value::as_object_mut) {
        merge_defaults(settings, default_settings_object());
    }

    if let Some(projects) = root.get_mut("projects").and_then(Value::as_array_mut) {
        for project in projects {
            migrate_project_value(project);
        }
    }

    value
}

fn migrate_session_value(mut value: Value) -> Value {
    let Some(root) = value.as_object_mut() else {
        return value;
    };

    insert_if_missing(root, "openTabs", Value::Array(Vec::new()));
    insert_if_missing(root, "activeTabId", Value::Null);
    insert_if_missing(root, "sidebarCollapsed", Value::Bool(false));

    if let Some(open_tabs) = root.get_mut("openTabs").and_then(Value::as_array_mut) {
        for tab in open_tabs {
            migrate_session_tab_value(tab);
        }
    }

    value
}

fn migrate_project_value(value: &mut Value) {
    let Some(project) = value.as_object_mut() else {
        return;
    };

    insert_if_missing(project, "folders", Value::Array(Vec::new()));

    if let Some(folders) = project.get_mut("folders").and_then(Value::as_array_mut) {
        for folder in folders {
            migrate_folder_value(folder);
        }
    }
}

fn migrate_folder_value(value: &mut Value) {
    let Some(folder) = value.as_object_mut() else {
        return;
    };

    insert_if_missing(folder, "commands", Value::Array(Vec::new()));
}

fn migrate_session_tab_value(value: &mut Value) {
    let Some(tab) = value.as_object_mut() else {
        return;
    };

    let tab_type = tab.get("type").and_then(Value::as_str).unwrap_or_default();
    if tab_type == "server" && !tab.contains_key("ptySessionId") {
        if let Some(command_id) = tab.get("commandId").cloned() {
            tab.insert("ptySessionId".to_string(), command_id);
        }
    }
}

fn insert_if_missing(map: &mut Map<String, Value>, key: &str, value: Value) {
    if !map.contains_key(key) {
        map.insert(key.to_string(), value);
    }
}

fn merge_defaults(target: &mut Map<String, Value>, defaults: Map<String, Value>) {
    for (key, value) in defaults {
        if !target.contains_key(&key) {
            target.insert(key, value);
        }
    }
}

fn default_settings_value() -> Value {
    Value::Object(default_settings_object())
}

fn default_settings_object() -> Map<String, Value> {
    match serde_json::to_value(Settings::default()) {
        Ok(Value::Object(map)) => map,
        _ => Map::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{AppConfig, Project};
    use crate::remote::test_support::TestProfileEnvGuard;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_test_dir(label: &str) -> PathBuf {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let path = std::env::temp_dir().join(format!(
            "devmanager-persistence-tests-{label}-{millis}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn corrupt_session_file_is_deleted_and_defaults_restored() {
        let temp_dir = temp_test_dir("corrupt-session");
        let session_path = temp_dir.join("session.json");
        fs::write(&session_path, "{ invalid json").unwrap();

        let session = load_session_from_path(&session_path).unwrap();

        assert_eq!(session, SessionState::default());
        assert!(!session_path.exists());
    }

    #[test]
    fn app_config_dir_name_defaults_without_profile() {
        let _profile = TestProfileEnvGuard::without_profile();
        assert_eq!(app_config_dir_name(), "com.userfirst.devmanager");
    }

    #[test]
    fn sanitize_scope_segment_normalizes_profile_values() {
        assert_eq!(
            sanitize_scope_segment(Some(" Dev Watch ".to_string())).as_deref(),
            Some("dev-watch")
        );
        assert_eq!(
            sanitize_scope_segment(Some("___".to_string())).as_deref(),
            Some("___")
        );
        assert_eq!(sanitize_scope_segment(Some("   ".to_string())), None);
    }

    #[test]
    fn sanitize_instance_label_preserves_human_friendly_text() {
        assert_eq!(
            sanitize_instance_label(Some(" Dev Build ".to_string())).as_deref(),
            Some("Dev Build")
        );
    }

    #[test]
    fn load_workspace_keeps_config_when_session_is_corrupt() {
        let temp_dir = temp_test_dir("workspace-fallback");
        let config_path = temp_dir.join("config.json");
        let session_path = temp_dir.join("session.json");

        let mut config = AppConfig::default();
        config.projects.push(Project {
            id: "project-1".to_string(),
            name: "Recovered Project".to_string(),
            root_path: ".".to_string(),
            folders: Vec::new(),
            color: None,
            pinned: Some(false),
            notes: None,
            save_log_files: Some(false),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
        });
        save_config_to_path(&config_path, &config).unwrap();
        fs::write(&session_path, "not valid json").unwrap();

        let snapshot = load_workspace_from_paths(&config_path, &session_path).unwrap();

        assert_eq!(snapshot.config.projects.len(), 1);
        assert_eq!(snapshot.config.projects[0].name, "Recovered Project");
        assert_eq!(snapshot.session, SessionState::default());
        assert!(!session_path.exists());
    }
}
