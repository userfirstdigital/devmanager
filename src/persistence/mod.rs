use crate::config::model::{decode_legacy_ssh_credential, encode_legacy_ssh_credential};
use crate::config::paths::{resolve_app_paths, AppProfile, BuildKind, ResolvedAppPaths};
use crate::config::{
    AppConfig as StrictAppConfig, ConfigAuthority, ConfigError, ConfigErrorKind, ConfigStore,
    DefaultTerminal as StrictDefaultTerminal, MacTerminalProfile as StrictMacTerminalProfile,
    Nullable, Project as StrictProject, ProjectFolder as StrictProjectFolder,
    RunCommand as StrictRunCommand, SSHConnection as StrictSshConnection,
    Settings as StrictSettings,
};
use crate::config::{SshAuth, SshAuthMode};
use crate::models::{AppConfig, SessionState, Settings, CURRENT_CONFIG_VERSION};
use serde::Serialize;
use serde_json::{Map, Value};
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

#[cfg(test)]
const APP_CONFIG_DIR: &str = "com.userfirst.devmanager";
const APP_PROFILE_ENV: &str = "DEVMANAGER_PROFILE";
const APP_INSTANCE_LABEL_ENV: &str = "DEVMANAGER_INSTANCE_LABEL";
const CONFIG_FILE_NAME: &str = "config.json";
const SESSION_FILE_NAME: &str = "session.json";

/// The durable host binds the already validated/locked CLI profile once,
/// before services resolve storage. Never mutate process environment to select
/// a host's data: child providers and parallel tests have different lifetimes.
#[derive(Debug, Clone, PartialEq, Eq)]
struct BoundHostStorage {
    profile: String,
    root: PathBuf,
}

#[cfg(not(test))]
static BOUND_HOST_STORAGE: OnceLock<BoundHostStorage> = OnceLock::new();

pub fn bind_durable_host_storage(profile: &str, root: &Path) -> Result<()> {
    #[cfg(test)]
    {
        let _ = (profile, root);
        // Unit tests always use the process-unique test root. This production
        // bootstrap seam cannot redirect them to an installed/named profile.
        return Err(PersistenceError::InvalidAppProfile);
    }
    #[cfg(not(test))]
    {
        let binding = validated_host_storage(profile, root)?;
        if let Some(existing) = BOUND_HOST_STORAGE.get() {
            return if existing == &binding {
                Ok(())
            } else {
                Err(PersistenceError::InvalidAppProfile)
            };
        }
        BOUND_HOST_STORAGE
            .set(binding)
            .map_err(|_| PersistenceError::InvalidAppProfile)
    }
}

fn validated_host_storage(profile: &str, root: &Path) -> Result<BoundHostStorage> {
    let AppProfile::Named(profile) =
        AppProfile::named(profile).map_err(|_| PersistenceError::InvalidAppProfile)?
    else {
        return Err(PersistenceError::InvalidAppProfile);
    };
    if !root.is_absolute() || !root.is_dir() {
        return Err(PersistenceError::InvalidAppProfile);
    }
    let root = root.canonicalize().map_err(|source| PersistenceError::Io {
        path: root.to_path_buf(),
        source,
    })?;
    let expected_name = if profile == "production" {
        "com.userfirst.devmanager".to_string()
    } else {
        format!("com.userfirst.devmanager-{profile}")
    };
    if root.file_name().and_then(|name| name.to_str()) != Some(expected_name.as_str()) {
        return Err(PersistenceError::InvalidAppProfile);
    }
    if cfg!(debug_assertions) && profile == "production" {
        return Err(PersistenceError::InvalidAppProfile);
    }
    Ok(BoundHostStorage { profile, root })
}

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
    InvalidAppProfile,
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    Parse {
        path: PathBuf,
        source: serde_json::Error,
    },
    Config(ConfigError),
}

impl Display for PersistenceError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ConfigDirectoryUnavailable => {
                write!(f, "could not determine the user config directory")
            }
            Self::InvalidAppProfile => {
                write!(
                    f,
                    "DEVMANAGER_PROFILE is set to an invalid value; use ASCII letters, digits, '-' or '_'"
                )
            }
            Self::Io { path, source } => {
                write!(f, "failed to read or write {}: {}", path.display(), source)
            }
            Self::Parse { path, source } => {
                write!(f, "failed to parse {}: {}", path.display(), source)
            }
            Self::Config(error) => Display::fmt(error, f),
        }
    }
}

impl Error for PersistenceError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ConfigDirectoryUnavailable | Self::InvalidAppProfile => None,
            Self::Io { source, .. } => Some(source),
            Self::Parse { source, .. } => Some(source),
            Self::Config(error) => Some(error),
        }
    }
}

pub type Result<T> = std::result::Result<T, PersistenceError>;

/// Read-only admission result shared by UI and Git integration. A failed
/// ConfigStore open is a write barrier, not a reason to construct a default
/// config and attempt a late save. Keep the diagnostic on the authority so
/// every caller presents the same root cause and no second availability state
/// can drift from the canonical store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ConfigWriteAvailability {
    Ready,
    Unavailable { diagnostic: String },
}

impl ConfigWriteAvailability {
    pub(crate) fn diagnostic(&self) -> Option<&str> {
        match self {
            Self::Ready => None,
            Self::Unavailable { diagnostic } => Some(diagnostic.as_str()),
        }
    }
}

pub(crate) fn active_config_write_availability() -> ConfigWriteAvailability {
    match open_active_config_store() {
        Ok(_) => ConfigWriteAvailability::Ready,
        Err(error) => ConfigWriteAvailability::Unavailable {
            diagnostic: error.to_string(),
        },
    }
}

/// Persist only a credential-reference removal through the canonical store.
/// Raw GitHub token material has no legacy/config representation and therefore
/// fails closed until the credential provider migration is available.
pub(crate) fn persist_github_token_reference(token: Option<&str>) -> Result<()> {
    if token.is_some() {
        return Err(PersistenceError::Config(ConfigError::new(
            ConfigErrorKind::SecretMaterial,
            "raw GitHub token material cannot be persisted in configuration",
        )));
    }
    let mut store = open_active_config_store()?;
    let mut patch = crate::config::SettingsPatch::new();
    patch.set_github_token_ref(crate::config::Nullable::Null);
    store
        .execute(
            store.snapshot().revision,
            crate::config::ConfigCommand::PatchSettings { patch },
        )
        .map(|_| ())
        .map_err(PersistenceError::Config)
}

pub fn app_config_dir() -> Result<PathBuf> {
    #[cfg(test)]
    {
        let profile = configured_storage_profile()?;
        let path = app_config_dir_for(test_config_root(), profile.as_deref());
        ensure_test_config_dir_is_isolated(path)
    }
    #[cfg(not(test))]
    {
        if let Some(binding) = BOUND_HOST_STORAGE.get() {
            return Ok(binding.root.clone());
        }
        let profile = configured_storage_profile()?.or_else(default_debug_profile);
        dirs::config_dir()
            .map(|base| app_config_dir_for(&base, profile.as_deref()))
            .ok_or(PersistenceError::ConfigDirectoryUnavailable)
    }
}

pub fn app_display_name() -> String {
    match app_instance_label().or_else(app_instance_profile) {
        Some(label) => format!("DevManager [{label}]"),
        None => "DevManager".to_string(),
    }
}

pub fn app_instance_label() -> Option<String> {
    sanitize_instance_label(std::env::var(APP_INSTANCE_LABEL_ENV).ok())
}

pub fn app_instance_profile() -> Option<String> {
    #[cfg(not(test))]
    if let Some(binding) = BOUND_HOST_STORAGE.get() {
        return Some(binding.profile.clone());
    }
    configured_profile().or_else(default_debug_profile)
}

pub fn runtime_session_scope() -> String {
    app_instance_profile().unwrap_or_else(|| format!("pid-{:x}", std::process::id()))
}

fn configured_profile() -> Option<String> {
    sanitize_scope_segment(std::env::var(APP_PROFILE_ENV).ok())
}

fn configured_storage_profile() -> Result<Option<String>> {
    match std::env::var(APP_PROFILE_ENV) {
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(PersistenceError::InvalidAppProfile),
        Ok(value) => match AppProfile::named(&value) {
            Ok(AppProfile::Named(name)) => Ok(Some(name)),
            _ => Err(PersistenceError::InvalidAppProfile),
        },
    }
}

fn default_debug_profile() -> Option<String> {
    #[cfg(all(debug_assertions, not(test)))]
    {
        Some("dev-debug".to_string())
    }
    #[cfg(any(not(debug_assertions), test))]
    {
        None
    }
}

fn app_config_dir_for(base: &Path, profile: Option<&str>) -> PathBuf {
    #[cfg(test)]
    {
        let app_profile = match profile {
            Some(name) => AppProfile::UnitTest(name.to_string()),
            None => AppProfile::UnitTest(String::new()),
        };
        resolve_app_paths(base, app_profile, BuildKind::Test)
            .expect("unit-test app path resolution")
            .root
    }
    #[cfg(not(test))]
    {
        let (app_profile, build_kind) = match profile {
            Some(name) => {
                let build_kind = if cfg!(debug_assertions) {
                    BuildKind::Debug
                } else {
                    BuildKind::Release
                };
                (AppProfile::Named(name.to_string()), build_kind)
            }
            None => (AppProfile::Production, BuildKind::Release),
        };
        resolve_app_paths(base, app_profile, build_kind)
            .expect("app path resolution")
            .root
    }
}

#[cfg(test)]
fn test_config_root() -> &'static Path {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "devmanager-unit-tests-{}-{nonce}",
            std::process::id()
        ))
    })
    .as_path()
}

#[cfg(test)]
fn ensure_test_config_dir_is_isolated(path: PathBuf) -> Result<PathBuf> {
    if let Some(production) = dirs::config_dir().map(|base| app_config_dir_for(&base, None)) {
        assert!(
            !path.starts_with(&production),
            "unit-test config path must not use installed DevManager storage"
        );
    }
    Ok(path)
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
    Ok(WorkspaceSnapshot {
        config: load_config()?,
        session: load_session()?,
    })
}

pub fn load_config() -> Result<AppConfig> {
    let store = open_active_config_store()?;
    strict_to_legacy(store.snapshot().config.clone()).map_err(PersistenceError::Config)
}

pub(crate) fn load_config_recovery() -> Result<AppConfig> {
    let paths = active_app_paths()?;
    let snapshot = ConfigStore::recover_host_snapshot(&paths).map_err(PersistenceError::Config)?;
    strict_to_legacy(snapshot.config).map_err(PersistenceError::Config)
}

pub fn load_session() -> Result<SessionState> {
    let path = session_path()?;
    load_session_from_path(&path)
}

pub fn save_config(config: &AppConfig) -> Result<()> {
    // Validate and convert the legacy-facing API before resolving the host
    // authority. The active store remains the only production persistence
    // boundary, while strict fields that have no legacy representation stay
    // attached to their existing canonical records.
    let incoming = legacy_to_strict(config).map_err(PersistenceError::Config)?;
    let mut store = open_active_config_store()?;
    let mut strict = incoming;
    preserve_strict_only_fields(&mut strict, &store.snapshot().config);
    store
        .replace_config(strict)
        .map_err(PersistenceError::Config)?;
    Ok(())
}

/// Export the active canonical snapshot without converting through the
/// legacy-facing UI model.  The session manager is the only desktop caller;
/// keeping this seam crate-private prevents arbitrary callers from selecting
/// a raw config path or bypassing the host-issued ConfigAuthority.
pub(crate) fn export_active_config_to_path(path: &Path) -> Result<()> {
    let store = open_active_config_store()?;
    store
        .export_external_to(path)
        .map_err(PersistenceError::Config)
}

pub fn save_session(session: &SessionState) -> Result<()> {
    let path = session_path()?;
    save_session_to_path(&path, session)
}

pub fn load_config_from_path(path: &Path) -> Result<AppConfig> {
    let strict = match crate::config::project_store::read_external_config(path) {
        Ok(strict) => strict,
        Err(error) if error.kind() == ConfigErrorKind::NotFound => return Ok(AppConfig::default()),
        Err(error) => return Err(PersistenceError::Config(error)),
    };
    strict_to_legacy(strict).map_err(PersistenceError::Config)
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
    let strict = legacy_to_strict(config).map_err(PersistenceError::Config)?;
    write_config_json_file(path, &strict)
}

pub fn save_session_to_path(path: &Path, session: &SessionState) -> Result<()> {
    write_json_file(path, session)
}

pub fn load_config_from_str(contents: &str) -> std::result::Result<AppConfig, serde_json::Error> {
    let strict = StrictAppConfig::from_legacy_json_str(contents).map_err(config_error_as_json)?;
    strict_to_legacy(strict).map_err(config_error_as_json)
}

pub fn load_session_from_str(
    contents: &str,
) -> std::result::Result<SessionState, serde_json::Error> {
    let value: Value = serde_json::from_str(contents)?;
    let migrated = migrate_session_value(value);
    let session: SessionState = serde_json::from_value::<SessionState>(migrated)?;

    Ok(session.normalize())
}

fn active_app_paths() -> Result<ResolvedAppPaths> {
    let root = app_config_dir()?;
    Ok(ResolvedAppPaths {
        config: root.join(CONFIG_FILE_NAME),
        remote: root.join("remote.json"),
        database: root.join("kernel.sqlite3"),
        browser_root: root.join("browser"),
        logs: root.join("logs"),
        root,
    })
}

fn open_active_config_store() -> Result<ConfigStore> {
    let paths = active_app_paths()?;
    let authority = ConfigAuthority::from_host_paths(&paths).map_err(PersistenceError::Config)?;
    ConfigStore::open_with_legacy_migration(authority).map_err(PersistenceError::Config)
}

fn config_error_as_json(error: ConfigError) -> serde_json::Error {
    serde_json::Error::io(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        error.to_string(),
    ))
}

fn strict_to_legacy(config: StrictAppConfig) -> std::result::Result<AppConfig, ConfigError> {
    let settings = strict_settings_to_legacy(config.settings().clone());
    let projects = config
        .projects
        .into_iter()
        .map(strict_project_to_legacy)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let ssh_connections = config
        .ssh_connections
        .into_iter()
        .map(strict_ssh_to_legacy)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(AppConfig {
        version: config.version,
        projects,
        settings,
        ssh_connections,
    })
}

/// The durable config and the terminal/UI layer carry two structurally
/// identical copies of this enum (`config::model` and `models::config`). One
/// conversion, here, so a variant added to either copy is a compile error in
/// exactly one place instead of a silent default in whichever call site was
/// forgotten. Both matches stay exhaustive on purpose — no `_` arm.
impl From<StrictDefaultTerminal> for crate::models::DefaultTerminal {
    fn from(terminal: StrictDefaultTerminal) -> Self {
        match terminal {
            StrictDefaultTerminal::Bash => Self::Bash,
            StrictDefaultTerminal::Powershell => Self::Powershell,
            StrictDefaultTerminal::Pwsh => Self::Pwsh,
            StrictDefaultTerminal::Cmd => Self::Cmd,
        }
    }
}

/// See [`From<StrictDefaultTerminal>`](crate::models::DefaultTerminal).
impl From<StrictMacTerminalProfile> for crate::models::MacTerminalProfile {
    fn from(profile: StrictMacTerminalProfile) -> Self {
        match profile {
            StrictMacTerminalProfile::System => Self::System,
            StrictMacTerminalProfile::Zsh => Self::Zsh,
            StrictMacTerminalProfile::Bash => Self::Bash,
        }
    }
}

fn strict_settings_to_legacy(settings: StrictSettings) -> Settings {
    Settings {
        theme: settings.theme,
        log_buffer_size: settings.log_buffer_size,
        confirm_on_close: settings.confirm_on_close,
        minimize_to_tray: settings.minimize_to_tray,
        restore_session_on_start: nullable_to_option(settings.restore_session_on_start),
        default_terminal: settings.default_terminal.into(),
        mac_terminal_profile: nullable_to_option(settings.mac_terminal_profile)
            .map(crate::models::MacTerminalProfile::from),
        claude_command: nullable_to_option(settings.claude_command),
        codex_command: nullable_to_option(settings.codex_command),
        notification_sound: nullable_to_option(settings.notification_sound),
        terminal_font_size: nullable_to_option(settings.terminal_font_size),
        option_as_meta: settings.option_as_meta,
        copy_on_select: settings.copy_on_select,
        keep_selection_on_copy: settings.keep_selection_on_copy,
        show_terminal_scrollbar: settings.show_terminal_scrollbar,
        shell_integration_enabled: settings.shell_integration_enabled,
        terminal_mouse_override: settings.terminal_mouse_override,
        terminal_read_only: settings.terminal_read_only,
        github_token: None,
        browser_enabled: settings.browser_enabled,
    }
}

fn strict_project_to_legacy(
    project: StrictProject,
) -> std::result::Result<crate::models::Project, ConfigError> {
    Ok(crate::models::Project {
        id: project.id,
        name: project.name,
        root_path: project.root_path,
        folders: project
            .folders
            .into_iter()
            .map(strict_folder_to_legacy)
            .collect::<std::result::Result<Vec<_>, _>>()?,
        color: nullable_to_option(project.color),
        pinned: nullable_to_option(project.pinned),
        notes: nullable_to_option(project.notes),
        save_log_files: nullable_to_option(project.save_log_files),
        created_at: project.created_at,
        updated_at: project.updated_at,
    })
}

fn strict_folder_to_legacy(
    folder: StrictProjectFolder,
) -> std::result::Result<crate::models::ProjectFolder, ConfigError> {
    Ok(crate::models::ProjectFolder {
        id: folder.id,
        name: folder.name,
        folder_path: folder.folder_path,
        commands: folder
            .commands
            .into_iter()
            .map(strict_command_to_legacy)
            .collect::<std::result::Result<Vec<_>, _>>()?,
        env_file_path: nullable_to_option(folder.env_file_path),
        port_variable: nullable_to_option(folder.port_variable),
        hidden: nullable_to_option(folder.hidden),
    })
}

fn strict_command_to_legacy(
    command: StrictRunCommand,
) -> std::result::Result<crate::models::RunCommand, ConfigError> {
    Ok(crate::models::RunCommand {
        id: command.id,
        label: command.label,
        command: command.command,
        args: command.args,
        env: nullable_to_option(command.env).map(|env| env.into_iter().collect()),
        port: nullable_to_option(command.port),
        auto_restart: nullable_to_option(command.auto_restart),
        clear_logs_on_restart: nullable_to_option(command.clear_logs_on_restart),
    })
}

fn strict_ssh_to_legacy(
    connection: StrictSshConnection,
) -> std::result::Result<crate::models::SSHConnection, ConfigError> {
    let (password, private_key) = match connection.auth.as_ref() {
        Some(auth) => match auth.credential_ref.as_ref() {
            Some(reference) => match decode_legacy_ssh_credential(reference)? {
                Some((password, private_key)) => (password, private_key),
                None => (None, None),
            },
            None => (None, None),
        },
        None => (None, None),
    };
    Ok(crate::models::SSHConnection {
        id: connection.id,
        label: connection.label,
        host: connection.host,
        port: connection.port,
        username: connection.username,
        password,
        private_key,
    })
}

fn nullable_to_option<T>(value: Nullable<T>) -> Option<T> {
    value.into_option()
}

fn legacy_to_strict(config: &AppConfig) -> std::result::Result<StrictAppConfig, ConfigError> {
    if config.settings.github_token.is_some() {
        return Err(ConfigError::new(
            ConfigErrorKind::SecretMaterial,
            "raw secret material must be stored through credential references",
        ));
    }

    let mut strict = StrictAppConfig::default();
    strict.version = config.version.max(CURRENT_CONFIG_VERSION);
    strict.projects = config
        .projects
        .iter()
        .map(legacy_project_to_strict)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    strict.ssh_connections = config
        .ssh_connections
        .iter()
        .map(legacy_ssh_to_strict)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    *strict.settings_mut() = legacy_settings_to_strict(&config.settings);
    strict.materialize_for_write();
    strict.validate()?;
    Ok(strict)
}

fn preserve_strict_only_fields(incoming: &mut StrictAppConfig, current: &StrictAppConfig) {
    incoming.set_workspace_project_ids(current.workspace_project_ids().clone());
    let current_projects = &current.projects;
    for project in &mut incoming.projects {
        let Some(previous) = current_projects.iter().find(|item| item.id == project.id) else {
            continue;
        };
        project.archived = previous.archived.clone();
        project.extra = previous.extra.clone();
        for folder in &mut project.folders {
            let Some(previous_folder) = previous.folders.iter().find(|item| item.id == folder.id)
            else {
                continue;
            };
            folder.archived = previous_folder.archived.clone();
            folder.extra = previous_folder.extra.clone();
            for command in &mut folder.commands {
                let Some(previous_command) = previous_folder
                    .commands
                    .iter()
                    .find(|item| item.id == command.id)
                else {
                    continue;
                };
                command.archived = previous_command.archived.clone();
                command.extra = previous_command.extra.clone();
            }
        }
    }

    for connection in &mut incoming.ssh_connections {
        let Some(previous) = current
            .ssh_connections
            .iter()
            .find(|item| item.id == connection.id)
        else {
            continue;
        };
        if connection.auth.is_absent() {
            connection.auth = previous.auth.clone();
        }
        connection.archived = previous.archived.clone();
        connection.extra = previous.extra.clone();
    }

    let previous_settings = current.settings().clone();
    let settings = incoming.settings_mut();
    settings.github_token_ref = previous_settings.github_token_ref;
    settings.default_directories = previous_settings.default_directories;
    settings.shell_options = previous_settings.shell_options;
    settings.editor = previous_settings.editor;
    settings.extra = previous_settings.extra;
}

fn legacy_settings_to_strict(settings: &Settings) -> StrictSettings {
    let mut strict = StrictSettings::default();
    strict.theme = settings.theme.clone();
    strict.log_buffer_size = settings.log_buffer_size;
    strict.confirm_on_close = settings.confirm_on_close;
    strict.minimize_to_tray = settings.minimize_to_tray;
    strict.restore_session_on_start = option_to_nullable(settings.restore_session_on_start);
    strict.default_terminal = match settings.default_terminal {
        crate::models::DefaultTerminal::Bash => StrictDefaultTerminal::Bash,
        crate::models::DefaultTerminal::Powershell => StrictDefaultTerminal::Powershell,
        crate::models::DefaultTerminal::Pwsh => StrictDefaultTerminal::Pwsh,
        crate::models::DefaultTerminal::Cmd => StrictDefaultTerminal::Cmd,
    };
    strict.mac_terminal_profile = option_to_nullable(settings.mac_terminal_profile.clone().map(
        |profile| match profile {
            crate::models::MacTerminalProfile::System => StrictMacTerminalProfile::System,
            crate::models::MacTerminalProfile::Zsh => StrictMacTerminalProfile::Zsh,
            crate::models::MacTerminalProfile::Bash => StrictMacTerminalProfile::Bash,
        },
    ));
    strict.claude_command = option_to_nullable(settings.claude_command.clone());
    strict.codex_command = option_to_nullable(settings.codex_command.clone());
    strict.notification_sound = option_to_nullable(settings.notification_sound.clone());
    strict.terminal_font_size = option_to_nullable(settings.terminal_font_size);
    strict.option_as_meta = settings.option_as_meta;
    strict.copy_on_select = settings.copy_on_select;
    strict.keep_selection_on_copy = settings.keep_selection_on_copy;
    strict.show_terminal_scrollbar = settings.show_terminal_scrollbar;
    strict.shell_integration_enabled = settings.shell_integration_enabled;
    strict.terminal_mouse_override = settings.terminal_mouse_override;
    strict.terminal_read_only = settings.terminal_read_only;
    strict.browser_enabled = settings.browser_enabled;
    strict
}

fn option_to_nullable<T>(value: Option<T>) -> Nullable<T> {
    match value {
        Some(value) => Nullable::Value(value),
        None => Nullable::Absent,
    }
}

fn legacy_project_to_strict(
    project: &crate::models::Project,
) -> std::result::Result<StrictProject, ConfigError> {
    Ok(StrictProject {
        id: project.id.clone(),
        name: project.name.clone(),
        root_path: project.root_path.clone(),
        folders: project
            .folders
            .iter()
            .map(legacy_folder_to_strict)
            .collect::<std::result::Result<Vec<_>, _>>()?,
        color: option_to_nullable(project.color.clone()),
        pinned: option_to_nullable(project.pinned),
        notes: option_to_nullable(project.notes.clone()),
        save_log_files: option_to_nullable(project.save_log_files),
        created_at: project.created_at.clone(),
        updated_at: project.updated_at.clone(),
        archived: Nullable::Absent,
        extra: Default::default(),
    })
}

fn legacy_folder_to_strict(
    folder: &crate::models::ProjectFolder,
) -> std::result::Result<StrictProjectFolder, ConfigError> {
    Ok(StrictProjectFolder {
        id: folder.id.clone(),
        name: folder.name.clone(),
        folder_path: folder.folder_path.clone(),
        commands: folder
            .commands
            .iter()
            .map(legacy_command_to_strict)
            .collect::<std::result::Result<Vec<_>, _>>()?,
        env_file_path: option_to_nullable(folder.env_file_path.clone()),
        port_variable: option_to_nullable(folder.port_variable.clone()),
        hidden: option_to_nullable(folder.hidden),
        archived: Nullable::Absent,
        extra: Default::default(),
    })
}

fn legacy_command_to_strict(
    command: &crate::models::RunCommand,
) -> std::result::Result<StrictRunCommand, ConfigError> {
    Ok(StrictRunCommand {
        id: command.id.clone(),
        label: command.label.clone(),
        command: command.command.clone(),
        args: command.args.clone(),
        env: option_to_nullable(command.env.clone().map(|env| env.into_iter().collect())),
        port: option_to_nullable(command.port),
        auto_restart: option_to_nullable(command.auto_restart),
        clear_logs_on_restart: option_to_nullable(command.clear_logs_on_restart),
        archived: Nullable::Absent,
        extra: Default::default(),
    })
}

fn legacy_ssh_to_strict(
    connection: &crate::models::SSHConnection,
) -> std::result::Result<StrictSshConnection, ConfigError> {
    let auth = match encode_legacy_ssh_credential(
        connection.password.as_deref(),
        connection.private_key.as_deref(),
    )? {
        Some(reference) => {
            let mode = if connection.private_key.is_some() {
                SshAuthMode::PrivateKey
            } else {
                SshAuthMode::Password
            };
            Nullable::Value(SshAuth {
                mode,
                credential_ref: Nullable::Value(reference),
                extra: Map::new(),
            })
        }
        None => Nullable::Absent,
    };
    Ok(StrictSshConnection {
        id: connection.id.clone(),
        label: connection.label.clone(),
        host: connection.host.clone(),
        port: connection.port,
        username: connection.username.clone(),
        auth,
        archived: Nullable::Absent,
        extra: Default::default(),
    })
}

fn write_config_json_file(path: &Path, config: &StrictAppConfig) -> Result<()> {
    ConfigStore::write_external_config(path, config).map_err(PersistenceError::Config)
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
    fn host_storage_binding_requires_the_exact_named_root() {
        let directory = tempfile::tempdir().expect("tempdir");
        let owner = directory
            .path()
            .join("com.userfirst.devmanager-remote-owner");
        fs::create_dir(&owner).expect("owner root");
        let binding = validated_host_storage("remote-owner", &owner).expect("exact root");
        assert_eq!(binding.root, owner.canonicalize().unwrap());
        assert_eq!(binding.profile, "remote-owner");
        assert!(validated_host_storage("other-owner", &owner).is_err());
        assert!(validated_host_storage("remote-owner", Path::new("relative")).is_err());
        assert!(validated_host_storage("bad/profile", &owner).is_err());
    }

    #[test]
    fn production_host_storage_binding_cannot_redirect_unit_tests() {
        let before = app_config_dir().expect("test root");
        assert!(bind_durable_host_storage("remote-owner", &before).is_err());
        assert_eq!(app_config_dir().expect("same test root"), before);
        assert!(before.starts_with(test_config_root()));
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
    fn unprofiled_unit_tests_never_resolve_the_production_directory() {
        let _profile = TestProfileEnvGuard::without_profile();
        let active = app_config_dir().expect("test config directory");
        let production = dirs::config_dir()
            .expect("production config parent")
            .join(APP_CONFIG_DIR);

        assert!(active.starts_with(std::env::temp_dir()));
        assert!(!active.starts_with(&production));
    }

    #[test]
    fn named_unit_test_profiles_remain_beneath_the_test_root() {
        let _profile = TestProfileEnvGuard::new("pairing-isolation");
        let active = app_config_dir().expect("test config directory");

        assert!(active.starts_with(test_config_root()));
        assert!(active
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.contains("pairing-isolation")));
    }

    #[test]
    fn invalid_explicit_profile_env_cannot_resolve_app_config_dir() {
        let _profile = TestProfileEnvGuard::without_profile();

        std::env::set_var(APP_PROFILE_ENV, "native next");
        let aliased = app_config_dir();
        assert!(
            matches!(aliased, Err(PersistenceError::InvalidAppProfile)),
            "space-containing profile must not alias to a normalized name: {aliased:?}"
        );
        if let Ok(path) = &aliased {
            assert!(
                !path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.contains("native-next")),
                "invalid explicit profile aliased to normalized storage path {}",
                path.display()
            );
        }

        for invalid in ["", "   ", "/", "..", r"a\b", "a/b"] {
            std::env::set_var(APP_PROFILE_ENV, invalid);
            let result = app_config_dir();
            assert!(
                matches!(result, Err(PersistenceError::InvalidAppProfile)),
                "explicit invalid profile {invalid:?} must not resolve storage: {result:?}"
            );
            if let Ok(path) = result {
                assert_ne!(
                    path.file_name().and_then(|name| name.to_str()),
                    Some(APP_CONFIG_DIR),
                    "invalid explicit profile {invalid:?} fell back to unprofiled production spelling"
                );
            }
        }
    }

    #[test]
    fn production_and_debug_directory_names_are_explicit() {
        let base = Path::new("config-root");
        assert_eq!(
            app_config_dir_for(base, None),
            base.join("com.userfirst.devmanager")
        );
        assert_eq!(
            app_config_dir_for(base, Some("dev-debug")),
            base.join("com.userfirst.devmanager-dev-debug")
        );
    }

    #[test]
    fn config_write_availability_preserves_the_canonical_diagnostic() {
        let availability = ConfigWriteAvailability::Unavailable {
            diagnostic: "strict config parse failed".to_string(),
        };

        assert_eq!(
            availability.diagnostic(),
            Some("strict config parse failed")
        );
        assert_eq!(ConfigWriteAvailability::Ready.diagnostic(), None);
    }

    #[test]
    fn app_display_name_falls_back_to_the_active_profile() {
        let _profile = TestProfileEnvGuard::new("display-profile");
        let profile = app_instance_profile().expect("active test profile");

        assert_eq!(app_display_name(), format!("DevManager [{profile}]"));
    }

    #[test]
    #[should_panic(expected = "unit-test config path must not use installed DevManager storage")]
    fn unit_test_path_guard_rejects_the_production_tree() {
        let production = dirs::config_dir()
            .expect("production config parent")
            .join(APP_CONFIG_DIR);

        ensure_test_config_dir_is_isolated(production.join("nested"))
            .expect("unsafe test path must be rejected");
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
