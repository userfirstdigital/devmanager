use serde::de::{DeserializeSeed, Error as DeError, MapAccess, SeqAccess, Visitor};
use serde::ser::Error as SerError;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use base64::Engine as _;

use crate::domain::ProjectId;

pub const CURRENT_CONFIG_VERSION: u32 = 2;
pub const MAX_CONFIG_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_COLLECTION_ITEMS: usize = 10_000;
pub const MAX_TEXT_BYTES: usize = 1_000_000;
pub const MAX_ID_BYTES: usize = 256;
pub const MAX_JSON_DEPTH: usize = 32;
pub const MAX_JSON_OBJECT_FIELDS: usize = MAX_COLLECTION_ITEMS;

const LEGACY_SSH_CREDENTIAL_PREFIX: &str = "credential:legacy-v1-";
// The opaque reference is still a bounded JSON string.  Its payload is
// larger than an ordinary identifier so normal PEM private keys survive the
// compatibility envelope without ever becoming plaintext config fields.
const MAX_LEGACY_SSH_REFERENCE_BYTES: usize = MAX_TEXT_BYTES;

pub type JsonMap = Map<String, Value>;
pub type ConfigRevision = u64;

/// A JSON field with three states. `Absent` is omitted when serialized, while
/// `Null` is serialized as JSON null. This keeps legacy optional/default
/// distinctions observable without normalizing user data.
#[derive(Clone, PartialEq, Eq)]
pub enum Nullable<T> {
    Absent,
    Null,
    Value(T),
}

impl<T> fmt::Debug for Nullable<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Absent => formatter.write_str("Absent"),
            Self::Null => formatter.write_str("Null"),
            Self::Value(_) => formatter.write_str("Value(<redacted>)"),
        }
    }
}

impl<T> Default for Nullable<T> {
    fn default() -> Self {
        Self::Absent
    }
}

impl<T> Nullable<T> {
    pub const fn is_absent(&self) -> bool {
        matches!(self, Self::Absent)
    }

    pub const fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }

    pub fn as_ref(&self) -> Option<&T> {
        match self {
            Self::Value(value) => Some(value),
            Self::Absent | Self::Null => None,
        }
    }

    pub fn into_option(self) -> Option<T> {
        match self {
            Self::Value(value) => Some(value),
            Self::Absent | Self::Null => None,
        }
    }
}

impl<T: Serialize> Serialize for Nullable<T> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Absent | Self::Null => serializer.serialize_none(),
            Self::Value(value) => value.serialize(serializer),
        }
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Nullable<T> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        match Option::<T>::deserialize(deserializer)? {
            Some(value) => Ok(Self::Value(value)),
            None => Ok(Self::Null),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigErrorKind {
    Parse,
    Validation,
    SecretMaterial,
    Io,
    RevisionConflict,
    ExternalChange,
    AtomicWrite,
    LockTimeout,
    PreviewReplay,
    ProtectedPath,
    PathAlias,
    NotFound,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ConfigError {
    kind: ConfigErrorKind,
    reason: &'static str,
}

impl ConfigError {
    pub const fn new(kind: ConfigErrorKind, reason: &'static str) -> Self {
        Self { kind, reason }
    }

    pub const fn kind(self) -> ConfigErrorKind {
        self.kind
    }
}

impl fmt::Debug for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ConfigError")
            .field("kind", &self.kind)
            .field("reason", &self.reason)
            .finish()
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "configuration {}", self.reason)
    }
}

impl std::error::Error for ConfigError {}

fn default_config_version() -> u32 {
    CURRENT_CONFIG_VERSION
}

#[derive(Clone, PartialEq, Eq)]
pub struct AppConfig {
    pub version: u32,
    pub revision: ConfigRevision,
    pub projects: Vec<Project>,
    settings: Settings,
    pub ssh_connections: Vec<SSHConnection>,
    /// Opt-in Portal connection metadata.  This contains only an opaque
    /// credential-vault reference; bearer material is never part of AppConfig.
    pub portal: PortalConfig,
    pub extra: JsonMap,
    /// Host-issued opaque identities for configured project ids.  These are
    /// persisted as canonical config metadata so reopening the store never
    /// derives a new identity from a filesystem path.
    workspace_project_ids: BTreeMap<String, String>,
    version_present: bool,
    revision_present: bool,
    settings_present: bool,
    source_version: Option<u32>,
}

/// A stable reference into the platform credential store.  The reference is
/// intentionally opaque and is not itself a token or secret.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PortalCredentialReference {
    pub vault_ref: String,
}

impl fmt::Debug for PortalCredentialReference {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PortalCredentialReference")
            .field("vault_ref", &"<redacted>")
            .finish()
    }
}

/// Persisted, typed Portal opt-in.  The default is standalone and cannot
/// start network traffic. Enrollment and a resolved credential are required
/// by the host runtime before a transport is constructed.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct PortalConfig {
    pub enabled: bool,
    pub base_url: Option<String>,
    pub credential_ref: Option<PortalCredentialReference>,
}

impl Default for PortalConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            base_url: None,
            credential_ref: None,
        }
    }
}

impl fmt::Debug for PortalConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PortalConfig")
            .field("enabled", &self.enabled)
            .field("base_url", &self.base_url)
            .field("credential_ref", &self.credential_ref)
            .finish()
    }
}

impl PortalConfig {
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if let Some(base_url) = &self.base_url {
            if base_url.len() > MAX_TEXT_BYTES
                || (!base_url.starts_with("https://")
                    && !base_url.starts_with("http://127.0.0.1")
                    && !base_url.starts_with("http://localhost"))
            {
                return Err(ConfigError::new(
                    ConfigErrorKind::Validation,
                    "Portal base URL must be HTTPS or local development HTTP",
                ));
            }
        }
        if let Some(reference) = &self.credential_ref {
            if reference.vault_ref.is_empty()
                || reference.vault_ref.len() > MAX_ID_BYTES
                || reference.vault_ref.chars().any(char::is_whitespace)
            {
                return Err(ConfigError::new(
                    ConfigErrorKind::Validation,
                    "Portal credential reference is invalid",
                ));
            }
        }
        if self.enabled && (self.base_url.is_none() || self.credential_ref.is_none()) {
            return Err(ConfigError::new(
                ConfigErrorKind::Validation,
                "enabled Portal configuration requires a base URL and credential reference",
            ));
        }
        Ok(())
    }

    /// Configuration alone never authorizes network access. The runtime also
    /// requires an enrolled projection and a resolved bearer credential.
    pub fn is_opted_in(&self) -> bool {
        self.enabled && self.base_url.is_some() && self.credential_ref.is_some()
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            version: CURRENT_CONFIG_VERSION,
            revision: 0,
            projects: Vec::new(),
            settings: Settings::default(),
            ssh_connections: Vec::new(),
            portal: PortalConfig::default(),
            extra: JsonMap::new(),
            workspace_project_ids: BTreeMap::new(),
            version_present: false,
            revision_present: false,
            settings_present: true,
            source_version: None,
        }
    }
}

impl AppConfig {
    pub fn from_json_str(contents: &str) -> Result<Self, ConfigError> {
        if contents.len() > MAX_CONFIG_BYTES {
            return Err(ConfigError::new(
                ConfigErrorKind::Validation,
                "input exceeds the configuration size limit",
            ));
        }

        let value = parse_json_value_rejecting_duplicates(contents)?;
        strict_app_config_from_value(value)
    }

    /// Decode an explicitly selected legacy payload. The runtime store calls
    /// this only inside its locked, one-time migration path; transfer callers
    /// must still fingerprint and consume the import through ConfigStore.
    pub fn from_legacy_json_str(contents: &str) -> Result<Self, ConfigError> {
        if contents.len() > MAX_CONFIG_BYTES {
            return Err(ConfigError::new(
                ConfigErrorKind::Validation,
                "input exceeds the configuration size limit",
            ));
        }

        let mut value = parse_json_value_rejecting_duplicates(contents)?;
        if value
            .as_object()
            .and_then(|object| object.get("version"))
            .and_then(Value::as_u64)
            .is_some_and(|version| version > u64::from(CURRENT_CONFIG_VERSION))
        {
            return Err(ConfigError::new(
                ConfigErrorKind::Validation,
                "legacy configuration version is newer than this application",
            ));
        }
        normalize_legacy_ssh_material(&mut value)?;
        reject_secret_material(&value)?;
        reject_legacy_nul_arguments(&value)?;
        app_config_from_value(value, true)
    }

    pub fn to_json_bytes(&self) -> Result<Vec<u8>, ConfigError> {
        let value = self.to_json_value()?;
        validate_checked_value(&value)?;
        let bytes = serde_json::to_vec_pretty(&value).map_err(|_| {
            ConfigError::new(
                ConfigErrorKind::Parse,
                "configuration could not be serialized",
            )
        })?;
        if bytes.len() > MAX_CONFIG_BYTES {
            return Err(ConfigError::new(
                ConfigErrorKind::Validation,
                "configuration exceeds the size limit",
            ));
        }
        Ok(bytes)
    }

    /// Encode a transfer copy without carrying the compatibility SSH secret
    /// envelope used by the active store.  The active config retains the
    /// envelope until the credential-reference migration lands; exports do
    /// not.
    pub fn to_redacted_json_bytes(&self) -> Result<Vec<u8>, ConfigError> {
        let mut value = self.to_json_value()?;
        redact_legacy_ssh_material(&mut value);
        validate_checked_value(&value)?;
        let bytes = serde_json::to_vec_pretty(&value).map_err(|_| {
            ConfigError::new(
                ConfigErrorKind::Parse,
                "configuration could not be serialized",
            )
        })?;
        if bytes.len() > MAX_CONFIG_BYTES {
            return Err(ConfigError::new(
                ConfigErrorKind::Validation,
                "configuration exceeds the size limit",
            ));
        }
        Ok(bytes)
    }

    pub fn to_json_value(&self) -> Result<Value, ConfigError> {
        let mut value = app_config_wire_value(self)?;
        validate_checked_value(&value)?;
        self.validate()?;
        if self.revision == 0 && !self.revision_present {
            if let Some(object) = value.as_object_mut() {
                object.remove("revision");
            }
        }
        if !self.version_present {
            if let Some(object) = value.as_object_mut() {
                object.remove("version");
            }
        }
        Ok(value)
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    pub(crate) fn source_version(&self) -> Option<u32> {
        self.source_version
    }

    pub fn settings_mut(&mut self) -> &mut Settings {
        self.settings_present = true;
        self.settings
            .present_fields
            .extend(SETTINGS_FIELDS.iter().map(|field| (*field).to_string()));
        &mut self.settings
    }

    pub(crate) fn workspace_project_ids(&self) -> &BTreeMap<String, String> {
        &self.workspace_project_ids
    }

    pub(crate) fn set_workspace_project_ids(&mut self, mapping: BTreeMap<String, String>) {
        self.workspace_project_ids = mapping;
    }

    pub(crate) fn apply_settings_patch(&mut self, patch: &SettingsPatch) {
        self.settings_present = true;
        patch.apply_to(&mut self.settings);
    }

    pub(crate) fn materialize_for_write(&mut self) {
        self.version_present = true;
        self.revision_present = true;
        self.settings_present = true;
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.version == 0 || self.version > CURRENT_CONFIG_VERSION {
            return Err(ConfigError::new(
                ConfigErrorKind::Validation,
                "configuration version is unsupported",
            ));
        }
        if self.projects.len() > MAX_COLLECTION_ITEMS
            || self.ssh_connections.len() > MAX_COLLECTION_ITEMS
        {
            return Err(ConfigError::new(
                ConfigErrorKind::Validation,
                "configuration collection exceeds the item limit",
            ));
        }

        validate_extra_map(&self.extra)?;

        if self.workspace_project_ids.len() > MAX_COLLECTION_ITEMS {
            return Err(ConfigError::new(
                ConfigErrorKind::Validation,
                "workspace identity mapping exceeds the item limit",
            ));
        }
        let configured_project_ids = self
            .projects
            .iter()
            .map(|project| project.id.as_str())
            .collect::<BTreeSet<_>>();
        let mut opaque_ids = BTreeSet::new();
        for (configured_id, opaque_id) in &self.workspace_project_ids {
            validate_id(configured_id)?;
            if !configured_project_ids.contains(configured_id.as_str()) {
                return Err(ConfigError::new(
                    ConfigErrorKind::Validation,
                    "workspace identity mapping contains an unknown project",
                ));
            }
            ProjectId::parse(opaque_id).map_err(|_| {
                ConfigError::new(
                    ConfigErrorKind::Validation,
                    "workspace identity mapping contains an invalid opaque id",
                )
            })?;
            if !opaque_ids.insert(opaque_id.as_str()) {
                return Err(ConfigError::new(
                    ConfigErrorKind::Validation,
                    "workspace identity mapping contains duplicate opaque ids",
                ));
            }
        }

        let mut ids = BTreeMap::new();
        for project in &self.projects {
            validate_id(&project.id)?;
            if ids.insert(project.id.as_str(), ()).is_some() {
                return Err(ConfigError::new(
                    ConfigErrorKind::Validation,
                    "configuration contains duplicate project IDs",
                ));
            }
            validate_project(project)?;
        }

        ids.clear();
        for connection in &self.ssh_connections {
            validate_id(&connection.id)?;
            if ids.insert(connection.id.as_str(), ()).is_some() {
                return Err(ConfigError::new(
                    ConfigErrorKind::Validation,
                    "configuration contains duplicate SSH IDs",
                ));
            }
            validate_ssh_connection(connection)?;
        }

        validate_settings(&self.settings)?;

        validate_checked_value(&app_config_wire_value(self)?)
    }
}

#[derive(Clone, PartialEq, Eq, Default)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub root_path: String,
    pub folders: Vec<ProjectFolder>,
    pub color: Nullable<String>,
    pub pinned: Nullable<bool>,
    pub notes: Nullable<String>,
    pub save_log_files: Nullable<bool>,
    pub created_at: String,
    pub updated_at: String,
    pub archived: Nullable<bool>,
    pub extra: JsonMap,
}

#[derive(Clone, PartialEq, Eq, Default)]
pub struct ProjectFolder {
    pub id: String,
    pub name: String,
    pub folder_path: String,
    pub commands: Vec<RunCommand>,
    pub env_file_path: Nullable<String>,
    pub port_variable: Nullable<String>,
    pub hidden: Nullable<bool>,
    pub archived: Nullable<bool>,
    pub extra: JsonMap,
}

#[derive(Clone, PartialEq, Eq, Default)]
pub struct RunCommand {
    pub id: String,
    pub label: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: Nullable<BTreeMap<String, String>>,
    pub port: Nullable<u16>,
    pub auto_restart: Nullable<bool>,
    pub clear_logs_on_restart: Nullable<bool>,
    pub archived: Nullable<bool>,
    pub extra: JsonMap,
}

#[derive(Clone, PartialEq, Eq, Default)]
pub struct SSHConnection {
    pub id: String,
    pub label: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth: Nullable<SshAuth>,
    pub archived: Nullable<bool>,
    pub extra: JsonMap,
}

#[derive(Clone, PartialEq, Eq)]
pub struct SshAuth {
    pub mode: SshAuthMode,
    pub credential_ref: Nullable<String>,
    pub extra: JsonMap,
}

impl Default for SshAuth {
    fn default() -> Self {
        Self {
            mode: SshAuthMode::Default,
            credential_ref: Nullable::Absent,
            extra: JsonMap::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SshAuthMode {
    Default,
    Agent,
    Password,
    PrivateKey,
}

#[derive(Clone, PartialEq, Eq)]
pub struct Settings {
    pub theme: String,
    pub log_buffer_size: u32,
    pub confirm_on_close: bool,
    pub minimize_to_tray: bool,
    pub restore_session_on_start: Nullable<bool>,
    pub default_terminal: DefaultTerminal,
    pub mac_terminal_profile: Nullable<MacTerminalProfile>,
    pub claude_command: Nullable<String>,
    pub codex_command: Nullable<String>,
    pub notification_sound: Nullable<String>,
    pub terminal_font_size: Nullable<u16>,
    pub option_as_meta: bool,
    pub copy_on_select: bool,
    pub keep_selection_on_copy: bool,
    pub show_terminal_scrollbar: bool,
    pub shell_integration_enabled: bool,
    pub terminal_mouse_override: bool,
    pub terminal_read_only: bool,
    pub browser_enabled: bool,
    pub github_token_ref: Nullable<String>,
    pub default_directories: Nullable<DefaultDirectories>,
    pub shell_options: Nullable<ShellOptions>,
    pub editor: Nullable<EditorChoice>,
    pub extra: JsonMap,
    present_fields: BTreeSet<String>,
}

const SETTINGS_FIELDS: &[&str] = &[
    "theme",
    "logBufferSize",
    "confirmOnClose",
    "minimizeToTray",
    "restoreSessionOnStart",
    "defaultTerminal",
    "macTerminalProfile",
    "claudeCommand",
    "codexCommand",
    "notificationSound",
    "terminalFontSize",
    "optionAsMeta",
    "copyOnSelect",
    "keepSelectionOnCopy",
    "showTerminalScrollbar",
    "shellIntegrationEnabled",
    "terminalMouseOverride",
    "terminalReadOnly",
    "browserEnabled",
    "githubTokenRef",
    "defaultDirectories",
    "shellOptions",
    "editor",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct SettingsWire {
    theme: String,
    log_buffer_size: u32,
    confirm_on_close: bool,
    minimize_to_tray: bool,
    #[serde(skip_serializing_if = "Nullable::is_absent")]
    restore_session_on_start: Nullable<bool>,
    default_terminal: DefaultTerminal,
    #[serde(skip_serializing_if = "Nullable::is_absent")]
    mac_terminal_profile: Nullable<MacTerminalProfile>,
    #[serde(skip_serializing_if = "Nullable::is_absent")]
    claude_command: Nullable<String>,
    #[serde(skip_serializing_if = "Nullable::is_absent")]
    codex_command: Nullable<String>,
    #[serde(skip_serializing_if = "Nullable::is_absent")]
    notification_sound: Nullable<String>,
    #[serde(skip_serializing_if = "Nullable::is_absent")]
    terminal_font_size: Nullable<u16>,
    option_as_meta: bool,
    copy_on_select: bool,
    keep_selection_on_copy: bool,
    show_terminal_scrollbar: bool,
    shell_integration_enabled: bool,
    terminal_mouse_override: bool,
    terminal_read_only: bool,
    browser_enabled: bool,
    #[serde(skip_serializing_if = "Nullable::is_absent")]
    github_token_ref: Nullable<String>,
    #[serde(skip_serializing_if = "Nullable::is_absent")]
    default_directories: Nullable<DefaultDirectoriesWire>,
    #[serde(skip_serializing_if = "Nullable::is_absent")]
    shell_options: Nullable<ShellOptionsWire>,
    #[serde(skip_serializing_if = "Nullable::is_absent")]
    editor: Nullable<EditorChoiceWire>,
    #[serde(flatten)]
    extra: JsonMap,
}

impl Default for SettingsWire {
    fn default() -> Self {
        Self {
            theme: "dark".to_string(),
            log_buffer_size: 10_000,
            confirm_on_close: true,
            minimize_to_tray: false,
            restore_session_on_start: Nullable::Absent,
            default_terminal: DefaultTerminal::Bash,
            mac_terminal_profile: Nullable::Absent,
            claude_command: Nullable::Absent,
            codex_command: Nullable::Absent,
            notification_sound: Nullable::Absent,
            terminal_font_size: Nullable::Absent,
            option_as_meta: false,
            copy_on_select: false,
            keep_selection_on_copy: true,
            show_terminal_scrollbar: true,
            shell_integration_enabled: true,
            terminal_mouse_override: false,
            terminal_read_only: false,
            browser_enabled: cfg!(windows),
            github_token_ref: Nullable::Absent,
            default_directories: Nullable::Absent,
            shell_options: Nullable::Absent,
            editor: Nullable::Absent,
            extra: JsonMap::new(),
        }
    }
}

impl Serialize for Settings {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let value = settings_wire_value(self).map_err(SerError::custom)?;
        value.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Settings {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = bounded_value_from_deserializer(deserializer)?;
        settings_from_value(value).map_err(D::Error::custom)
    }
}

impl Settings {
    fn apply_patch(&mut self, patch: &SettingsPatch) {
        for field in &patch.edited_fields {
            match field {
                SettingsField::Theme => self.theme = patch.values.theme.clone(),
                SettingsField::LogBufferSize => self.log_buffer_size = patch.values.log_buffer_size,
                SettingsField::ConfirmOnClose => {
                    self.confirm_on_close = patch.values.confirm_on_close
                }
                SettingsField::MinimizeToTray => {
                    self.minimize_to_tray = patch.values.minimize_to_tray
                }
                SettingsField::RestoreSessionOnStart => {
                    self.restore_session_on_start = patch.values.restore_session_on_start.clone()
                }
                SettingsField::DefaultTerminal => {
                    self.default_terminal = patch.values.default_terminal
                }
                SettingsField::MacTerminalProfile => {
                    self.mac_terminal_profile = patch.values.mac_terminal_profile.clone()
                }
                SettingsField::ClaudeCommand => {
                    self.claude_command = patch.values.claude_command.clone()
                }
                SettingsField::CodexCommand => {
                    self.codex_command = patch.values.codex_command.clone()
                }
                SettingsField::NotificationSound => {
                    self.notification_sound = patch.values.notification_sound.clone()
                }
                SettingsField::TerminalFontSize => {
                    self.terminal_font_size = patch.values.terminal_font_size.clone()
                }
                SettingsField::OptionAsMeta => self.option_as_meta = patch.values.option_as_meta,
                SettingsField::CopyOnSelect => self.copy_on_select = patch.values.copy_on_select,
                SettingsField::KeepSelectionOnCopy => {
                    self.keep_selection_on_copy = patch.values.keep_selection_on_copy
                }
                SettingsField::ShowTerminalScrollbar => {
                    self.show_terminal_scrollbar = patch.values.show_terminal_scrollbar
                }
                SettingsField::ShellIntegrationEnabled => {
                    self.shell_integration_enabled = patch.values.shell_integration_enabled
                }
                SettingsField::TerminalMouseOverride => {
                    self.terminal_mouse_override = patch.values.terminal_mouse_override
                }
                SettingsField::TerminalReadOnly => {
                    self.terminal_read_only = patch.values.terminal_read_only
                }
                SettingsField::BrowserEnabled => {
                    self.browser_enabled = patch.values.browser_enabled
                }
                SettingsField::GithubTokenRef => {
                    self.github_token_ref = patch.values.github_token_ref.clone()
                }
                SettingsField::DefaultDirectories => {
                    self.default_directories = patch.values.default_directories.clone()
                }
                SettingsField::ShellOptions => {
                    self.shell_options = patch.values.shell_options.clone()
                }
                SettingsField::Editor => self.editor = patch.values.editor.clone(),
            }
        }
        for field in &patch.edited_fields {
            self.present_fields.insert(field.json_name().to_string());
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SettingsField {
    Theme,
    LogBufferSize,
    ConfirmOnClose,
    MinimizeToTray,
    RestoreSessionOnStart,
    DefaultTerminal,
    MacTerminalProfile,
    ClaudeCommand,
    CodexCommand,
    NotificationSound,
    TerminalFontSize,
    OptionAsMeta,
    CopyOnSelect,
    KeepSelectionOnCopy,
    ShowTerminalScrollbar,
    ShellIntegrationEnabled,
    TerminalMouseOverride,
    TerminalReadOnly,
    BrowserEnabled,
    GithubTokenRef,
    DefaultDirectories,
    ShellOptions,
    Editor,
}

impl SettingsField {
    fn json_name(self) -> &'static str {
        match self {
            Self::Theme => "theme",
            Self::LogBufferSize => "logBufferSize",
            Self::ConfirmOnClose => "confirmOnClose",
            Self::MinimizeToTray => "minimizeToTray",
            Self::RestoreSessionOnStart => "restoreSessionOnStart",
            Self::DefaultTerminal => "defaultTerminal",
            Self::MacTerminalProfile => "macTerminalProfile",
            Self::ClaudeCommand => "claudeCommand",
            Self::CodexCommand => "codexCommand",
            Self::NotificationSound => "notificationSound",
            Self::TerminalFontSize => "terminalFontSize",
            Self::OptionAsMeta => "optionAsMeta",
            Self::CopyOnSelect => "copyOnSelect",
            Self::KeepSelectionOnCopy => "keepSelectionOnCopy",
            Self::ShowTerminalScrollbar => "showTerminalScrollbar",
            Self::ShellIntegrationEnabled => "shellIntegrationEnabled",
            Self::TerminalMouseOverride => "terminalMouseOverride",
            Self::TerminalReadOnly => "terminalReadOnly",
            Self::BrowserEnabled => "browserEnabled",
            Self::GithubTokenRef => "githubTokenRef",
            Self::DefaultDirectories => "defaultDirectories",
            Self::ShellOptions => "shellOptions",
            Self::Editor => "editor",
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct SettingsPatch {
    values: Settings,
    edited_fields: BTreeSet<SettingsField>,
}

impl Default for SettingsPatch {
    fn default() -> Self {
        Self::new()
    }
}

impl SettingsPatch {
    pub fn new() -> Self {
        Self {
            values: Settings::default(),
            edited_fields: BTreeSet::new(),
        }
    }

    pub fn edited_fields(&self) -> &BTreeSet<SettingsField> {
        &self.edited_fields
    }

    pub fn set_theme(&mut self, value: impl Into<String>) {
        self.values.theme = value.into();
        self.edited_fields.insert(SettingsField::Theme);
    }

    pub fn set_log_buffer_size(&mut self, value: u32) {
        self.values.log_buffer_size = value;
        self.edited_fields.insert(SettingsField::LogBufferSize);
    }

    pub fn set_confirm_on_close(&mut self, value: bool) {
        self.values.confirm_on_close = value;
        self.edited_fields.insert(SettingsField::ConfirmOnClose);
    }

    pub fn set_minimize_to_tray(&mut self, value: bool) {
        self.values.minimize_to_tray = value;
        self.edited_fields.insert(SettingsField::MinimizeToTray);
    }

    pub fn set_restore_session_on_start(&mut self, value: Nullable<bool>) {
        self.values.restore_session_on_start = value;
        self.edited_fields
            .insert(SettingsField::RestoreSessionOnStart);
    }

    pub fn set_default_terminal(&mut self, value: DefaultTerminal) {
        self.values.default_terminal = value;
        self.edited_fields.insert(SettingsField::DefaultTerminal);
    }

    pub fn set_mac_terminal_profile(&mut self, value: Nullable<MacTerminalProfile>) {
        self.values.mac_terminal_profile = value;
        self.edited_fields.insert(SettingsField::MacTerminalProfile);
    }

    pub fn set_claude_command(&mut self, value: Nullable<String>) {
        self.values.claude_command = value;
        self.edited_fields.insert(SettingsField::ClaudeCommand);
    }

    pub fn set_codex_command(&mut self, value: Nullable<String>) {
        self.values.codex_command = value;
        self.edited_fields.insert(SettingsField::CodexCommand);
    }

    pub fn set_notification_sound(&mut self, value: Nullable<String>) {
        self.values.notification_sound = value;
        self.edited_fields.insert(SettingsField::NotificationSound);
    }

    pub fn set_terminal_font_size(&mut self, value: Nullable<u16>) {
        self.values.terminal_font_size = value;
        self.edited_fields.insert(SettingsField::TerminalFontSize);
    }

    pub fn set_option_as_meta(&mut self, value: bool) {
        self.values.option_as_meta = value;
        self.edited_fields.insert(SettingsField::OptionAsMeta);
    }

    pub fn set_copy_on_select(&mut self, value: bool) {
        self.values.copy_on_select = value;
        self.edited_fields.insert(SettingsField::CopyOnSelect);
    }

    pub fn set_keep_selection_on_copy(&mut self, value: bool) {
        self.values.keep_selection_on_copy = value;
        self.edited_fields
            .insert(SettingsField::KeepSelectionOnCopy);
    }

    pub fn set_show_terminal_scrollbar(&mut self, value: bool) {
        self.values.show_terminal_scrollbar = value;
        self.edited_fields
            .insert(SettingsField::ShowTerminalScrollbar);
    }

    pub fn set_shell_integration_enabled(&mut self, value: bool) {
        self.values.shell_integration_enabled = value;
        self.edited_fields
            .insert(SettingsField::ShellIntegrationEnabled);
    }

    pub fn set_terminal_mouse_override(&mut self, value: bool) {
        self.values.terminal_mouse_override = value;
        self.edited_fields
            .insert(SettingsField::TerminalMouseOverride);
    }

    pub fn set_terminal_read_only(&mut self, value: bool) {
        self.values.terminal_read_only = value;
        self.edited_fields.insert(SettingsField::TerminalReadOnly);
    }

    pub fn set_browser_enabled(&mut self, value: bool) {
        self.values.browser_enabled = value;
        self.edited_fields.insert(SettingsField::BrowserEnabled);
    }

    pub fn set_github_token_ref(&mut self, value: Nullable<String>) {
        self.values.github_token_ref = value;
        self.edited_fields.insert(SettingsField::GithubTokenRef);
    }

    pub fn set_default_directories(&mut self, value: Nullable<DefaultDirectories>) {
        self.values.default_directories = value;
        self.edited_fields.insert(SettingsField::DefaultDirectories);
    }

    pub fn set_shell_options(&mut self, value: Nullable<ShellOptions>) {
        self.values.shell_options = value;
        self.edited_fields.insert(SettingsField::ShellOptions);
    }

    pub fn set_editor(&mut self, value: Nullable<EditorChoice>) {
        self.values.editor = value;
        self.edited_fields.insert(SettingsField::Editor);
    }

    pub(crate) fn apply_to(&self, settings: &mut Settings) {
        settings.apply_patch(self);
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: "dark".to_string(),
            log_buffer_size: 10_000,
            confirm_on_close: true,
            minimize_to_tray: false,
            restore_session_on_start: Nullable::Value(true),
            default_terminal: DefaultTerminal::Bash,
            mac_terminal_profile: Nullable::Value(MacTerminalProfile::System),
            claude_command: Nullable::Absent,
            codex_command: Nullable::Absent,
            notification_sound: Nullable::Absent,
            terminal_font_size: Nullable::Absent,
            option_as_meta: false,
            copy_on_select: false,
            keep_selection_on_copy: true,
            show_terminal_scrollbar: true,
            shell_integration_enabled: true,
            terminal_mouse_override: false,
            terminal_read_only: false,
            browser_enabled: cfg!(windows),
            github_token_ref: Nullable::Absent,
            default_directories: Nullable::Absent,
            shell_options: Nullable::Absent,
            editor: Nullable::Absent,
            extra: JsonMap::new(),
            present_fields: SETTINGS_FIELDS
                .iter()
                .map(|field| (*field).to_string())
                .collect(),
        }
    }
}

#[derive(Clone, PartialEq, Eq, Default)]
pub struct DefaultDirectories {
    pub projects: Nullable<String>,
    pub worktrees: Nullable<String>,
    pub exports: Nullable<String>,
    pub extra: JsonMap,
}

#[derive(Clone, PartialEq, Eq, Default)]
pub struct ShellOptions {
    pub program: Nullable<String>,
    pub args: Nullable<Vec<String>>,
    pub inherit_environment: Nullable<bool>,
    pub integration_enabled: Nullable<bool>,
    pub extra: JsonMap,
}

#[derive(Clone, PartialEq, Eq, Default)]
pub struct EditorChoice {
    pub kind: String,
    pub command: Nullable<String>,
    pub args: Nullable<Vec<String>>,
    pub extra: JsonMap,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DefaultTerminal {
    Bash,
    Powershell,
    Pwsh,
    Cmd,
}

impl Default for DefaultTerminal {
    fn default() -> Self {
        Self::Bash
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Clone, PartialEq, Eq)]
pub enum ConfigCommand {
    CreateProject {
        project: Project,
    },
    UpdateProject {
        project: Project,
    },
    ReorderProject {
        project_id: String,
        new_index: usize,
    },
    ArchiveProject {
        project_id: String,
    },
    CreateFolder {
        project_id: String,
        folder: ProjectFolder,
    },
    UpdateFolder {
        project_id: String,
        folder: ProjectFolder,
    },
    ReorderFolder {
        project_id: String,
        folder_id: String,
        new_index: usize,
    },
    ArchiveFolder {
        project_id: String,
        folder_id: String,
    },
    CreateCommand {
        project_id: String,
        folder_id: String,
        command: RunCommand,
    },
    UpdateCommand {
        project_id: String,
        folder_id: String,
        command: RunCommand,
    },
    ReorderCommand {
        project_id: String,
        folder_id: String,
        command_id: String,
        new_index: usize,
    },
    ArchiveCommand {
        project_id: String,
        folder_id: String,
        command_id: String,
    },
    CreateSsh {
        connection: SSHConnection,
    },
    UpdateSsh {
        connection: SSHConnection,
    },
    ReorderSsh {
        connection_id: String,
        new_index: usize,
    },
    ArchiveSsh {
        connection_id: String,
    },
    PatchSettings {
        patch: SettingsPatch,
    },
}

impl ConfigCommand {
    /// Validate all caller-controlled command data without consulting a store.
    pub fn validate(&self) -> Result<(), ConfigError> {
        match self {
            Self::CreateProject { project } | Self::UpdateProject { project } => {
                validate_project(project)
            }
            Self::ReorderProject {
                project_id,
                new_index,
            } => {
                validate_id(project_id)?;
                validate_reorder_index(*new_index)
            }
            Self::ArchiveProject { project_id } => validate_id(project_id),
            Self::CreateFolder { project_id, folder }
            | Self::UpdateFolder { project_id, folder } => {
                validate_id(project_id)?;
                validate_project_folder(folder)
            }
            Self::ReorderFolder {
                project_id,
                folder_id,
                new_index,
            } => {
                validate_id(project_id)?;
                validate_id(folder_id)?;
                validate_reorder_index(*new_index)
            }
            Self::ArchiveFolder {
                project_id,
                folder_id,
            } => {
                validate_id(project_id)?;
                validate_id(folder_id)
            }
            Self::CreateCommand {
                project_id,
                folder_id,
                command,
            }
            | Self::UpdateCommand {
                project_id,
                folder_id,
                command,
            } => {
                validate_id(project_id)?;
                validate_id(folder_id)?;
                validate_run_command(command)
            }
            Self::ReorderCommand {
                project_id,
                folder_id,
                command_id,
                new_index,
            } => {
                validate_id(project_id)?;
                validate_id(folder_id)?;
                validate_id(command_id)?;
                validate_reorder_index(*new_index)
            }
            Self::ArchiveCommand {
                project_id,
                folder_id,
                command_id,
            } => {
                validate_id(project_id)?;
                validate_id(folder_id)?;
                validate_id(command_id)
            }
            Self::CreateSsh { connection } | Self::UpdateSsh { connection } => {
                validate_ssh_connection(connection)
            }
            Self::ReorderSsh {
                connection_id,
                new_index,
            } => {
                validate_id(connection_id)?;
                validate_reorder_index(*new_index)
            }
            Self::ArchiveSsh { connection_id } => validate_id(connection_id),
            Self::PatchSettings { patch } => validate_settings(&patch.values),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct AppConfigWire {
    #[serde(default = "default_config_version")]
    version: u32,
    revision: ConfigRevision,
    projects: Vec<ProjectWire>,
    settings: SettingsWire,
    ssh_connections: Vec<SSHConnectionWire>,
    #[serde(skip_serializing_if = "PortalConfig::is_default")]
    portal: PortalConfig,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    workspace_project_ids: BTreeMap<String, String>,
    #[serde(flatten)]
    extra: JsonMap,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct ProjectWire {
    id: String,
    name: String,
    root_path: String,
    folders: Vec<ProjectFolderWire>,
    #[serde(skip_serializing_if = "Nullable::is_absent")]
    color: Nullable<String>,
    #[serde(skip_serializing_if = "Nullable::is_absent")]
    pinned: Nullable<bool>,
    #[serde(skip_serializing_if = "Nullable::is_absent")]
    notes: Nullable<String>,
    #[serde(skip_serializing_if = "Nullable::is_absent")]
    save_log_files: Nullable<bool>,
    created_at: String,
    updated_at: String,
    #[serde(skip_serializing_if = "Nullable::is_absent")]
    archived: Nullable<bool>,
    #[serde(flatten)]
    extra: JsonMap,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct ProjectFolderWire {
    id: String,
    name: String,
    folder_path: String,
    commands: Vec<RunCommandWire>,
    #[serde(skip_serializing_if = "Nullable::is_absent")]
    env_file_path: Nullable<String>,
    #[serde(skip_serializing_if = "Nullable::is_absent")]
    port_variable: Nullable<String>,
    #[serde(skip_serializing_if = "Nullable::is_absent")]
    hidden: Nullable<bool>,
    #[serde(skip_serializing_if = "Nullable::is_absent")]
    archived: Nullable<bool>,
    #[serde(flatten)]
    extra: JsonMap,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct RunCommandWire {
    id: String,
    label: String,
    command: String,
    args: Vec<String>,
    #[serde(skip_serializing_if = "Nullable::is_absent")]
    env: Nullable<BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "Nullable::is_absent")]
    port: Nullable<u16>,
    #[serde(skip_serializing_if = "Nullable::is_absent")]
    auto_restart: Nullable<bool>,
    #[serde(skip_serializing_if = "Nullable::is_absent")]
    clear_logs_on_restart: Nullable<bool>,
    #[serde(skip_serializing_if = "Nullable::is_absent")]
    archived: Nullable<bool>,
    #[serde(flatten)]
    extra: JsonMap,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct SSHConnectionWire {
    id: String,
    label: String,
    host: String,
    port: u16,
    username: String,
    #[serde(skip_serializing_if = "Nullable::is_absent")]
    auth: Nullable<SshAuthWire>,
    #[serde(skip_serializing_if = "Nullable::is_absent")]
    archived: Nullable<bool>,
    #[serde(flatten)]
    extra: JsonMap,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct SshAuthWire {
    mode: SshAuthMode,
    #[serde(skip_serializing_if = "Nullable::is_absent")]
    credential_ref: Nullable<String>,
    #[serde(flatten)]
    extra: JsonMap,
}

impl Default for SshAuthWire {
    fn default() -> Self {
        Self {
            mode: SshAuthMode::Default,
            credential_ref: Nullable::Absent,
            extra: JsonMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct DefaultDirectoriesWire {
    #[serde(skip_serializing_if = "Nullable::is_absent")]
    projects: Nullable<String>,
    #[serde(skip_serializing_if = "Nullable::is_absent")]
    worktrees: Nullable<String>,
    #[serde(skip_serializing_if = "Nullable::is_absent")]
    exports: Nullable<String>,
    #[serde(flatten)]
    extra: JsonMap,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct ShellOptionsWire {
    #[serde(skip_serializing_if = "Nullable::is_absent")]
    program: Nullable<String>,
    #[serde(skip_serializing_if = "Nullable::is_absent")]
    args: Nullable<Vec<String>>,
    #[serde(skip_serializing_if = "Nullable::is_absent")]
    inherit_environment: Nullable<bool>,
    #[serde(skip_serializing_if = "Nullable::is_absent")]
    integration_enabled: Nullable<bool>,
    #[serde(flatten)]
    extra: JsonMap,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct EditorChoiceWire {
    kind: String,
    #[serde(skip_serializing_if = "Nullable::is_absent")]
    command: Nullable<String>,
    #[serde(skip_serializing_if = "Nullable::is_absent")]
    args: Nullable<Vec<String>>,
    #[serde(flatten)]
    extra: JsonMap,
}

fn map_nullable<T, U>(value: Nullable<T>, map: impl FnOnce(T) -> U) -> Nullable<U> {
    match value {
        Nullable::Absent => Nullable::Absent,
        Nullable::Null => Nullable::Null,
        Nullable::Value(value) => Nullable::Value(map(value)),
    }
}

fn try_map_nullable<T, U>(
    value: Nullable<T>,
    map: impl FnOnce(T) -> Result<U, ConfigError>,
) -> Result<Nullable<U>, ConfigError> {
    match value {
        Nullable::Absent => Ok(Nullable::Absent),
        Nullable::Null => Ok(Nullable::Null),
        Nullable::Value(value) => Ok(Nullable::Value(map(value)?)),
    }
}

impl TryFrom<&Project> for ProjectWire {
    type Error = ConfigError;

    fn try_from(project: &Project) -> Result<Self, Self::Error> {
        validate_id(&project.id)?;
        Ok(Self {
            id: project.id.clone(),
            name: project.name.clone(),
            root_path: project.root_path.clone(),
            folders: project
                .folders
                .iter()
                .map(ProjectFolderWire::try_from)
                .collect::<Result<Vec<_>, _>>()?,
            color: project.color.clone(),
            pinned: project.pinned.clone(),
            notes: project.notes.clone(),
            save_log_files: project.save_log_files.clone(),
            created_at: project.created_at.clone(),
            updated_at: project.updated_at.clone(),
            archived: project.archived.clone(),
            extra: project.extra.clone(),
        })
    }
}

impl TryFrom<ProjectWire> for Project {
    type Error = ConfigError;

    fn try_from(project: ProjectWire) -> Result<Self, Self::Error> {
        validate_id(&project.id)?;
        Ok(Self {
            id: project.id,
            name: project.name,
            root_path: project.root_path,
            folders: project
                .folders
                .into_iter()
                .map(ProjectFolder::try_from)
                .collect::<Result<Vec<_>, _>>()?,
            color: project.color,
            pinned: project.pinned,
            notes: project.notes,
            save_log_files: project.save_log_files,
            created_at: project.created_at,
            updated_at: project.updated_at,
            archived: project.archived,
            extra: project.extra,
        })
    }
}

impl TryFrom<&ProjectFolder> for ProjectFolderWire {
    type Error = ConfigError;

    fn try_from(folder: &ProjectFolder) -> Result<Self, Self::Error> {
        validate_id(&folder.id)?;
        Ok(Self {
            id: folder.id.clone(),
            name: folder.name.clone(),
            folder_path: folder.folder_path.clone(),
            commands: folder
                .commands
                .iter()
                .map(RunCommandWire::try_from)
                .collect::<Result<Vec<_>, _>>()?,
            env_file_path: folder.env_file_path.clone(),
            port_variable: folder.port_variable.clone(),
            hidden: folder.hidden.clone(),
            archived: folder.archived.clone(),
            extra: folder.extra.clone(),
        })
    }
}

impl TryFrom<ProjectFolderWire> for ProjectFolder {
    type Error = ConfigError;

    fn try_from(folder: ProjectFolderWire) -> Result<Self, Self::Error> {
        validate_id(&folder.id)?;
        Ok(Self {
            id: folder.id,
            name: folder.name,
            folder_path: folder.folder_path,
            commands: folder
                .commands
                .into_iter()
                .map(RunCommand::try_from)
                .collect::<Result<Vec<_>, _>>()?,
            env_file_path: folder.env_file_path,
            port_variable: folder.port_variable,
            hidden: folder.hidden,
            archived: folder.archived,
            extra: folder.extra,
        })
    }
}

impl TryFrom<&RunCommand> for RunCommandWire {
    type Error = ConfigError;

    fn try_from(command: &RunCommand) -> Result<Self, Self::Error> {
        validate_id(&command.id)?;
        Ok(Self {
            id: command.id.clone(),
            label: command.label.clone(),
            command: command.command.clone(),
            args: command.args.clone(),
            env: command.env.clone(),
            port: command.port.clone(),
            auto_restart: command.auto_restart.clone(),
            clear_logs_on_restart: command.clear_logs_on_restart.clone(),
            archived: command.archived.clone(),
            extra: command.extra.clone(),
        })
    }
}

impl TryFrom<RunCommandWire> for RunCommand {
    type Error = ConfigError;

    fn try_from(command: RunCommandWire) -> Result<Self, Self::Error> {
        validate_id(&command.id)?;
        Ok(Self {
            id: command.id,
            label: command.label,
            command: command.command,
            args: command.args,
            env: command.env,
            port: command.port,
            auto_restart: command.auto_restart,
            clear_logs_on_restart: command.clear_logs_on_restart,
            archived: command.archived,
            extra: command.extra,
        })
    }
}

impl TryFrom<&SSHConnection> for SSHConnectionWire {
    type Error = ConfigError;

    fn try_from(connection: &SSHConnection) -> Result<Self, Self::Error> {
        validate_id(&connection.id)?;
        Ok(Self {
            id: connection.id.clone(),
            label: connection.label.clone(),
            host: connection.host.clone(),
            port: connection.port,
            username: connection.username.clone(),
            auth: try_map_nullable(connection.auth.clone(), |auth| SshAuthWire::try_from(&auth))?,
            archived: connection.archived.clone(),
            extra: connection.extra.clone(),
        })
    }
}

impl TryFrom<SSHConnectionWire> for SSHConnection {
    type Error = ConfigError;

    fn try_from(connection: SSHConnectionWire) -> Result<Self, Self::Error> {
        validate_id(&connection.id)?;
        Ok(Self {
            id: connection.id,
            label: connection.label,
            host: connection.host,
            port: connection.port,
            username: connection.username,
            auth: try_map_nullable(connection.auth, SshAuth::try_from)?,
            archived: connection.archived,
            extra: connection.extra,
        })
    }
}

impl TryFrom<&SshAuth> for SshAuthWire {
    type Error = ConfigError;

    fn try_from(auth: &SshAuth) -> Result<Self, Self::Error> {
        validate_ssh_auth(auth)?;
        Ok(Self {
            mode: auth.mode,
            credential_ref: auth.credential_ref.clone(),
            extra: auth.extra.clone(),
        })
    }
}

impl TryFrom<SshAuthWire> for SshAuth {
    type Error = ConfigError;

    fn try_from(auth: SshAuthWire) -> Result<Self, Self::Error> {
        let auth = Self {
            mode: auth.mode,
            credential_ref: auth.credential_ref,
            extra: auth.extra,
        };
        validate_ssh_auth(&auth)?;
        Ok(auth)
    }
}

impl From<&DefaultDirectories> for DefaultDirectoriesWire {
    fn from(directories: &DefaultDirectories) -> Self {
        Self {
            projects: directories.projects.clone(),
            worktrees: directories.worktrees.clone(),
            exports: directories.exports.clone(),
            extra: directories.extra.clone(),
        }
    }
}

impl From<DefaultDirectoriesWire> for DefaultDirectories {
    fn from(directories: DefaultDirectoriesWire) -> Self {
        Self {
            projects: directories.projects,
            worktrees: directories.worktrees,
            exports: directories.exports,
            extra: directories.extra,
        }
    }
}

impl From<&ShellOptions> for ShellOptionsWire {
    fn from(options: &ShellOptions) -> Self {
        Self {
            program: options.program.clone(),
            args: options.args.clone(),
            inherit_environment: options.inherit_environment.clone(),
            integration_enabled: options.integration_enabled.clone(),
            extra: options.extra.clone(),
        }
    }
}

impl From<ShellOptionsWire> for ShellOptions {
    fn from(options: ShellOptionsWire) -> Self {
        Self {
            program: options.program,
            args: options.args,
            inherit_environment: options.inherit_environment,
            integration_enabled: options.integration_enabled,
            extra: options.extra,
        }
    }
}

impl From<&EditorChoice> for EditorChoiceWire {
    fn from(editor: &EditorChoice) -> Self {
        Self {
            kind: editor.kind.clone(),
            command: editor.command.clone(),
            args: editor.args.clone(),
            extra: editor.extra.clone(),
        }
    }
}

impl From<EditorChoiceWire> for EditorChoice {
    fn from(editor: EditorChoiceWire) -> Self {
        Self {
            kind: editor.kind,
            command: editor.command,
            args: editor.args,
            extra: editor.extra,
        }
    }
}

impl From<&Settings> for SettingsWire {
    fn from(settings: &Settings) -> Self {
        Self {
            theme: settings.theme.clone(),
            log_buffer_size: settings.log_buffer_size,
            confirm_on_close: settings.confirm_on_close,
            minimize_to_tray: settings.minimize_to_tray,
            restore_session_on_start: settings.restore_session_on_start.clone(),
            default_terminal: settings.default_terminal,
            mac_terminal_profile: settings.mac_terminal_profile.clone(),
            claude_command: settings.claude_command.clone(),
            codex_command: settings.codex_command.clone(),
            notification_sound: settings.notification_sound.clone(),
            terminal_font_size: settings.terminal_font_size.clone(),
            option_as_meta: settings.option_as_meta,
            copy_on_select: settings.copy_on_select,
            keep_selection_on_copy: settings.keep_selection_on_copy,
            show_terminal_scrollbar: settings.show_terminal_scrollbar,
            shell_integration_enabled: settings.shell_integration_enabled,
            terminal_mouse_override: settings.terminal_mouse_override,
            terminal_read_only: settings.terminal_read_only,
            browser_enabled: settings.browser_enabled,
            github_token_ref: settings.github_token_ref.clone(),
            default_directories: map_nullable(settings.default_directories.clone(), |value| {
                DefaultDirectoriesWire::from(&value)
            }),
            shell_options: map_nullable(settings.shell_options.clone(), |value| {
                ShellOptionsWire::from(&value)
            }),
            editor: map_nullable(settings.editor.clone(), |value| {
                EditorChoiceWire::from(&value)
            }),
            extra: settings.extra.clone(),
        }
    }
}

impl From<SettingsWire> for Settings {
    fn from(settings: SettingsWire) -> Self {
        Self {
            theme: settings.theme,
            log_buffer_size: settings.log_buffer_size,
            confirm_on_close: settings.confirm_on_close,
            minimize_to_tray: settings.minimize_to_tray,
            restore_session_on_start: settings.restore_session_on_start,
            default_terminal: settings.default_terminal,
            mac_terminal_profile: settings.mac_terminal_profile,
            claude_command: settings.claude_command,
            codex_command: settings.codex_command,
            notification_sound: settings.notification_sound,
            terminal_font_size: settings.terminal_font_size,
            option_as_meta: settings.option_as_meta,
            copy_on_select: settings.copy_on_select,
            keep_selection_on_copy: settings.keep_selection_on_copy,
            show_terminal_scrollbar: settings.show_terminal_scrollbar,
            shell_integration_enabled: settings.shell_integration_enabled,
            terminal_mouse_override: settings.terminal_mouse_override,
            terminal_read_only: settings.terminal_read_only,
            browser_enabled: settings.browser_enabled,
            github_token_ref: settings.github_token_ref,
            default_directories: map_nullable(settings.default_directories, |value| {
                DefaultDirectories::from(value)
            }),
            shell_options: map_nullable(settings.shell_options, ShellOptions::from),
            editor: map_nullable(settings.editor, EditorChoice::from),
            extra: settings.extra,
            present_fields: BTreeSet::new(),
        }
    }
}

impl TryFrom<&AppConfig> for AppConfigWire {
    type Error = ConfigError;

    fn try_from(config: &AppConfig) -> Result<Self, Self::Error> {
        config.portal.validate()?;
        Ok(Self {
            version: config.version,
            revision: config.revision,
            projects: config
                .projects
                .iter()
                .map(ProjectWire::try_from)
                .collect::<Result<Vec<_>, _>>()?,
            settings: SettingsWire::from(&config.settings),
            ssh_connections: config
                .ssh_connections
                .iter()
                .map(SSHConnectionWire::try_from)
                .collect::<Result<Vec<_>, _>>()?,
            portal: config.portal.clone(),
            workspace_project_ids: config.workspace_project_ids.clone(),
            extra: config.extra.clone(),
        })
    }
}

impl TryFrom<AppConfigWire> for AppConfig {
    type Error = ConfigError;

    fn try_from(config: AppConfigWire) -> Result<Self, Self::Error> {
        let projects = config
            .projects
            .into_iter()
            .map(Project::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        let ssh_connections = config
            .ssh_connections
            .into_iter()
            .map(SSHConnection::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        config.portal.validate()?;
        Ok(Self {
            version: config.version,
            revision: config.revision,
            projects,
            settings: Settings::from(config.settings),
            ssh_connections,
            portal: config.portal,
            workspace_project_ids: config.workspace_project_ids,
            extra: config.extra,
            version_present: false,
            revision_present: false,
            settings_present: false,
            source_version: None,
        })
    }
}

fn checked_serialization_value(value: Value) -> Result<Value, ConfigError> {
    validate_checked_value(&value)?;
    Ok(value)
}

fn validate_checked_value(value: &Value) -> Result<(), ConfigError> {
    reject_secret_material(value)?;
    validate_json_shape(value, 0)?;
    let bytes = serde_json::to_vec(value).map_err(|_| {
        ConfigError::new(
            ConfigErrorKind::Parse,
            "configuration could not be serialized",
        )
    })?;
    if bytes.len() > MAX_CONFIG_BYTES {
        return Err(ConfigError::new(
            ConfigErrorKind::Validation,
            "configuration exceeds the size limit",
        ));
    }
    Ok(())
}

fn app_config_wire_value(config: &AppConfig) -> Result<Value, ConfigError> {
    let mut value = serde_json::to_value(AppConfigWire::try_from(config)?).map_err(|_| {
        ConfigError::new(
            ConfigErrorKind::Parse,
            "configuration could not be serialized",
        )
    })?;
    if let Some(object) = value.as_object_mut() {
        if config.settings_present {
            object.insert(
                "settings".to_string(),
                settings_wire_value_internal(&config.settings, true)?,
            );
        } else {
            object.remove("settings");
        }
    }
    Ok(value)
}

fn settings_wire_value(settings: &Settings) -> Result<Value, ConfigError> {
    settings_wire_value_internal(settings, true)
}

fn settings_wire_value_internal(settings: &Settings, validate: bool) -> Result<Value, ConfigError> {
    if validate {
        validate_settings(settings)?;
    }
    let mut value = serde_json::to_value(SettingsWire::from(settings)).map_err(|_| {
        ConfigError::new(
            ConfigErrorKind::Parse,
            "configuration could not be serialized",
        )
    })?;
    if let Some(object) = value.as_object_mut() {
        for field in SETTINGS_FIELDS {
            if !settings.present_fields.contains(*field) {
                object.remove(*field);
            }
        }
    }
    checked_serialization_value(value)
}

struct StrictJsonValue {
    depth: usize,
}

fn bounded_value_from_deserializer<'de, D>(deserializer: D) -> Result<Value, D::Error>
where
    D: Deserializer<'de>,
{
    StrictJsonValue { depth: 0 }.deserialize(deserializer)
}

impl<'de> DeserializeSeed<'de> for StrictJsonValue {
    type Value = Value;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(StrictJsonValueVisitor { depth: self.depth })
    }
}

struct StrictJsonValueVisitor {
    depth: usize,
}

impl<'de> Visitor<'de> for StrictJsonValueVisitor {
    type Value = Value;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value without duplicate object keys")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E>
    where
        E: DeError,
    {
        Ok(Value::Bool(value))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
    where
        E: DeError,
    {
        Ok(Value::Number(value.into()))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
    where
        E: DeError,
    {
        Ok(Value::Number(value.into()))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: DeError,
    {
        serde_json::Number::from_f64(value)
            .map(Value::Number)
            .ok_or_else(|| E::custom("invalid JSON number"))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: DeError,
    {
        if value.len() > MAX_TEXT_BYTES {
            return Err(E::custom("configuration string exceeds the size limit"));
        }
        Ok(Value::String(value.to_string()))
    }

    fn visit_borrowed_str<E>(self, value: &'de str) -> Result<Self::Value, E>
    where
        E: DeError,
    {
        self.visit_str(value)
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
    where
        E: DeError,
    {
        if value.len() > MAX_TEXT_BYTES {
            return Err(E::custom("configuration string exceeds the size limit"));
        }
        Ok(Value::String(value))
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E>
    where
        E: DeError,
    {
        Ok(Value::Null)
    }

    fn visit_none<E>(self) -> Result<Self::Value, E>
    where
        E: DeError,
    {
        Ok(Value::Null)
    }

    fn visit_seq<A>(self, mut access: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        if self.depth >= MAX_JSON_DEPTH {
            return Err(A::Error::custom(
                "configuration nesting exceeds the depth limit",
            ));
        }
        let mut values = Vec::with_capacity(
            access
                .size_hint()
                .unwrap_or_default()
                .min(MAX_COLLECTION_ITEMS),
        );
        for _ in 0..MAX_COLLECTION_ITEMS {
            let Some(value) = access.next_element_seed(StrictJsonValue {
                depth: self.depth + 1,
            })?
            else {
                return Ok(Value::Array(values));
            };
            values.push(value);
        }
        // Parse one extra element through the same bounded seed before
        // returning the limit error.  `IgnoredAny` would let an attacker
        // drive an unbounded recursive walk after the collection limit was
        // reached.
        if access
            .next_element_seed(StrictJsonValue {
                depth: self.depth + 1,
            })?
            .is_some()
        {
            return Err(A::Error::custom(
                "configuration collection exceeds the item limit",
            ));
        }
        Ok(Value::Array(values))
    }

    fn visit_map<A>(self, mut access: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        if self.depth >= MAX_JSON_DEPTH {
            return Err(A::Error::custom(
                "configuration nesting exceeds the depth limit",
            ));
        }
        let mut object = Map::with_capacity(
            access
                .size_hint()
                .unwrap_or_default()
                .min(MAX_JSON_OBJECT_FIELDS),
        );
        for _ in 0..MAX_JSON_OBJECT_FIELDS {
            let Some(key) = access.next_key_seed(BoundedStringSeed)? else {
                return Ok(Value::Object(object));
            };
            if object.contains_key(&key) {
                return Err(A::Error::custom("duplicate JSON object key"));
            }
            let value = access.next_value_seed(StrictJsonValue {
                depth: self.depth + 1,
            })?;
            object.insert(key, value);
        }
        if access.next_key_seed(BoundedStringSeed)?.is_some() {
            let _ = access.next_value_seed(StrictJsonValue {
                depth: self.depth + 1,
            })?;
            return Err(A::Error::custom(
                "configuration object exceeds the field limit",
            ));
        }
        Ok(Value::Object(object))
    }
}

struct BoundedStringSeed;

impl<'de> DeserializeSeed<'de> for BoundedStringSeed {
    type Value = String;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_string(BoundedStringVisitor)
    }
}

struct BoundedStringVisitor;

impl<'de> Visitor<'de> for BoundedStringVisitor {
    type Value = String;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a bounded JSON object key")
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: DeError,
    {
        if value.len() > MAX_TEXT_BYTES {
            return Err(E::custom("configuration string exceeds the size limit"));
        }
        Ok(value.to_string())
    }

    fn visit_borrowed_str<E>(self, value: &'de str) -> Result<Self::Value, E>
    where
        E: DeError,
    {
        self.visit_str(value)
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
    where
        E: DeError,
    {
        if value.len() > MAX_TEXT_BYTES {
            return Err(E::custom("configuration string exceeds the size limit"));
        }
        Ok(value)
    }
}

fn parse_json_value_rejecting_duplicates(contents: &str) -> Result<Value, ConfigError> {
    let mut deserializer = serde_json::Deserializer::from_str(contents);
    let value = StrictJsonValue { depth: 0 }
        .deserialize(&mut deserializer)
        .map_err(|error| {
            let detail = error.to_string();
            let kind = if detail.contains("size limit")
                || detail.contains("item limit")
                || detail.contains("field limit")
                || detail.contains("depth limit")
            {
                ConfigErrorKind::Validation
            } else {
                ConfigErrorKind::Parse
            };
            ConfigError::new(kind, "JSON could not be parsed")
        })?;
    deserializer
        .end()
        .map_err(|_| ConfigError::new(ConfigErrorKind::Parse, "JSON contains trailing data"))?;
    Ok(value)
}

pub(crate) fn encode_legacy_ssh_credential(
    password: Option<&str>,
    private_key: Option<&str>,
) -> Result<Option<String>, ConfigError> {
    if password.is_none() && private_key.is_none() {
        return Ok(None);
    }
    if password.is_some_and(|material| {
        material.is_empty()
            || material.len() > MAX_TEXT_BYTES
            || material.chars().any(char::is_control)
    }) || private_key.is_some_and(|material| {
        material.is_empty()
            || material.len() > MAX_TEXT_BYTES
            || material
                .chars()
                .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
    }) {
        return Err(ConfigError::new(
            ConfigErrorKind::Validation,
            "legacy SSH credential material is invalid",
        ));
    }
    let payload = serde_json::to_vec(&(password, private_key)).map_err(|_| {
        ConfigError::new(
            ConfigErrorKind::Parse,
            "legacy SSH credential material could not be encoded",
        )
    })?;
    let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload);
    let reference = format!("{LEGACY_SSH_CREDENTIAL_PREFIX}{encoded}");
    if reference.len() > MAX_LEGACY_SSH_REFERENCE_BYTES {
        return Err(ConfigError::new(
            ConfigErrorKind::Validation,
            "legacy SSH credential material exceeds the reference limit",
        ));
    }
    Ok(Some(reference))
}

pub(crate) fn decode_legacy_ssh_credential(
    reference: &str,
) -> Result<Option<(Option<String>, Option<String>)>, ConfigError> {
    let Some(encoded) = reference.strip_prefix(LEGACY_SSH_CREDENTIAL_PREFIX) else {
        return Ok(None);
    };
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| {
            ConfigError::new(
                ConfigErrorKind::Validation,
                "legacy SSH credential reference is malformed",
            )
        })?;
    let (password, private_key): (Option<String>, Option<String>) =
        serde_json::from_slice(&payload).map_err(|_| {
            ConfigError::new(
                ConfigErrorKind::Validation,
                "legacy SSH credential reference is malformed",
            )
        })?;
    encode_legacy_ssh_credential(password.as_deref(), private_key.as_deref())?;
    Ok(Some((password, private_key)))
}

fn normalize_legacy_ssh_material(value: &mut Value) -> Result<(), ConfigError> {
    let Some(connections) = value
        .as_object_mut()
        .and_then(|object| object.get_mut("sshConnections"))
        .and_then(Value::as_array_mut)
    else {
        return Ok(());
    };
    for connection in connections {
        let Some(object) = connection.as_object_mut() else {
            continue;
        };
        let password = match object.get("password") {
            Some(Value::Null) | None => None,
            Some(_) => object
                .remove("password")
                .map(|value| legacy_secret_value(value, "password"))
                .transpose()?,
        };
        let private_key = match object.get("privateKey") {
            Some(Value::Null) | None => None,
            Some(_) => object
                .remove("privateKey")
                .map(|value| legacy_secret_value(value, "privateKey"))
                .transpose()?,
        };
        if password.is_none() && private_key.is_none() {
            continue;
        }
        if object.get("auth").is_some_and(|value| !value.is_null()) {
            return Err(ConfigError::new(
                ConfigErrorKind::SecretMaterial,
                "legacy SSH secret material conflicts with an auth reference",
            ));
        }
        let reference = encode_legacy_ssh_credential(password.as_deref(), private_key.as_deref())?
            .ok_or_else(|| {
                ConfigError::new(
                    ConfigErrorKind::Validation,
                    "legacy SSH credential material is missing",
                )
            })?;
        let mode = if private_key.is_some() {
            "privateKey"
        } else {
            "password"
        };
        object.insert(
            "auth".to_string(),
            serde_json::json!({"mode": mode, "credentialRef": reference}),
        );
    }
    Ok(())
}

fn legacy_secret_value(value: Value, field: &str) -> Result<String, ConfigError> {
    let Some(value) = value.as_str() else {
        return Err(ConfigError::new(
            ConfigErrorKind::SecretMaterial,
            if field == "password" {
                "legacy SSH password must be a string"
            } else {
                "legacy SSH private key must be a string"
            },
        ));
    };
    let invalid_control = if field == "privateKey" {
        value
            .chars()
            .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
    } else {
        value.chars().any(char::is_control)
    };
    if value.is_empty() || value.len() > MAX_TEXT_BYTES || invalid_control {
        return Err(ConfigError::new(
            ConfigErrorKind::Validation,
            "legacy SSH credential material is invalid",
        ));
    }
    Ok(value.to_string())
}

fn redact_legacy_ssh_material(value: &mut Value) {
    let Some(connections) = value
        .as_object_mut()
        .and_then(|object| object.get_mut("sshConnections"))
        .and_then(Value::as_array_mut)
    else {
        return;
    };
    for connection in connections {
        let Some(object) = connection.as_object_mut() else {
            continue;
        };
        let is_legacy = object
            .get("auth")
            .and_then(Value::as_object)
            .and_then(|auth| auth.get("credentialRef"))
            .and_then(Value::as_str)
            .is_some_and(|reference| reference.starts_with(LEGACY_SSH_CREDENTIAL_PREFIX));
        if is_legacy {
            object.remove("auth");
        }
    }
}

fn canonical_object<'a>(
    value: &'a Value,
    allowed: &[&str],
    required: &[&str],
) -> Result<&'a Map<String, Value>, ConfigError> {
    let object = value.as_object().ok_or_else(|| {
        ConfigError::new(
            ConfigErrorKind::Parse,
            "canonical configuration shape is invalid",
        )
    })?;
    if object.keys().any(|key| !allowed.contains(&key.as_str()))
        || required.iter().any(|key| !object.contains_key(*key))
    {
        return Err(ConfigError::new(
            ConfigErrorKind::Parse,
            "canonical configuration shape is invalid",
        ));
    }
    Ok(object)
}

fn canonical_array<'a>(
    object: &'a Map<String, Value>,
    key: &str,
) -> Result<&'a Vec<Value>, ConfigError> {
    object.get(key).and_then(Value::as_array).ok_or_else(|| {
        ConfigError::new(
            ConfigErrorKind::Parse,
            "canonical configuration shape is invalid",
        )
    })
}

fn canonical_optional_object(
    object: &Map<String, Value>,
    key: &str,
    allowed: &[&str],
    required: &[&str],
) -> Result<(), ConfigError> {
    if let Some(value) = object.get(key) {
        if value.is_null() {
            return Ok(());
        }
        canonical_object(value, allowed, required)?;
    }
    Ok(())
}

fn strict_app_config_shape(value: &Value) -> Result<(), ConfigError> {
    let root = canonical_object(
        value,
        &[
            "version",
            "revision",
            "projects",
            "settings",
            "sshConnections",
            "workspaceProjectIds",
            "portal",
        ],
        &[
            "version",
            "revision",
            "projects",
            "settings",
            "sshConnections",
        ],
    )?;
    if root.get("version") != Some(&Value::Number(CURRENT_CONFIG_VERSION.into())) {
        return Err(ConfigError::new(
            ConfigErrorKind::Parse,
            "canonical configuration version is unsupported",
        ));
    }
    if let Some(mapping) = root.get("workspaceProjectIds") {
        let mapping = mapping.as_object().ok_or_else(|| {
            ConfigError::new(
                ConfigErrorKind::Parse,
                "workspace identity mapping must be an object",
            )
        })?;
        if mapping.len() > MAX_COLLECTION_ITEMS {
            return Err(ConfigError::new(
                ConfigErrorKind::Validation,
                "workspace identity mapping exceeds the item limit",
            ));
        }
        let mut opaque_ids = BTreeSet::new();
        for (configured_id, opaque_id) in mapping {
            validate_id(configured_id)?;
            let opaque_id = opaque_id.as_str().ok_or_else(|| {
                ConfigError::new(
                    ConfigErrorKind::Parse,
                    "workspace identity mapping value must be a string",
                )
            })?;
            ProjectId::parse(opaque_id).map_err(|_| {
                ConfigError::new(
                    ConfigErrorKind::Validation,
                    "workspace identity mapping contains an invalid opaque id",
                )
            })?;
            if !opaque_ids.insert(opaque_id) {
                return Err(ConfigError::new(
                    ConfigErrorKind::Validation,
                    "workspace identity mapping contains duplicate opaque ids",
                ));
            }
        }
    }
    for project in canonical_array(root, "projects")? {
        let project = canonical_object(
            project,
            &[
                "id",
                "name",
                "rootPath",
                "folders",
                "color",
                "pinned",
                "notes",
                "saveLogFiles",
                "createdAt",
                "updatedAt",
                "archived",
            ],
            &[
                "id",
                "name",
                "rootPath",
                "folders",
                "createdAt",
                "updatedAt",
            ],
        )?;
        for folder in canonical_array(project, "folders")? {
            let folder = canonical_object(
                folder,
                &[
                    "id",
                    "name",
                    "folderPath",
                    "commands",
                    "envFilePath",
                    "portVariable",
                    "hidden",
                    "archived",
                ],
                &["id", "name", "folderPath", "commands"],
            )?;
            for command in canonical_array(folder, "commands")? {
                let command = canonical_object(
                    command,
                    &[
                        "id",
                        "label",
                        "command",
                        "args",
                        "env",
                        "port",
                        "autoRestart",
                        "clearLogsOnRestart",
                        "archived",
                    ],
                    &["id", "label", "command", "args"],
                )?;
                if let Some(env) = command.get("env") {
                    if !env.is_null() {
                        if !env.is_object() {
                            return Err(ConfigError::new(
                                ConfigErrorKind::Parse,
                                "canonical configuration shape is invalid",
                            ));
                        }
                    }
                }
            }
        }
    }
    let settings = canonical_object(
        root.get("settings").expect("required settings field"),
        SETTINGS_FIELDS,
        &[
            "theme",
            "logBufferSize",
            "confirmOnClose",
            "minimizeToTray",
            "defaultTerminal",
            "optionAsMeta",
            "copyOnSelect",
            "keepSelectionOnCopy",
            "showTerminalScrollbar",
            "shellIntegrationEnabled",
            "terminalMouseOverride",
            "terminalReadOnly",
            "browserEnabled",
        ],
    )?;
    canonical_optional_object(
        settings,
        "defaultDirectories",
        &["projects", "worktrees", "exports"],
        &[],
    )?;
    canonical_optional_object(
        settings,
        "shellOptions",
        &[
            "program",
            "args",
            "inheritEnvironment",
            "integrationEnabled",
        ],
        &[],
    )?;
    canonical_optional_object(settings, "editor", &["kind", "command", "args"], &["kind"])?;
    for connection in canonical_array(root, "sshConnections")? {
        let connection = canonical_object(
            connection,
            &[
                "id", "label", "host", "port", "username", "auth", "archived",
            ],
            &["id", "label", "host", "port", "username"],
        )?;
        canonical_optional_object(connection, "auth", &["mode", "credentialRef"], &["mode"])?;
    }
    Ok(())
}

fn strict_app_config_from_value(value: Value) -> Result<AppConfig, ConfigError> {
    strict_app_config_shape(&value)?;
    app_config_from_value(value, true)
}

fn app_config_from_value(value: Value, strict_validation: bool) -> Result<AppConfig, ConfigError> {
    let (
        version_present,
        revision_present,
        settings_present,
        settings_present_fields,
        source_version,
    ) = {
        let object = value.as_object().ok_or_else(|| {
            ConfigError::new(ConfigErrorKind::Parse, "configuration must be an object")
        })?;
        let version_present = object.contains_key("version");
        let revision_present = object.contains_key("revision");
        let settings_present = object.contains_key("settings");
        let settings_present_fields = match object.get("settings") {
            None => BTreeSet::new(),
            Some(Value::Object(settings)) => settings
                .keys()
                .filter(|key| SETTINGS_FIELDS.contains(&key.as_str()))
                .cloned()
                .collect::<BTreeSet<_>>(),
            Some(_) => {
                return Err(ConfigError::new(
                    ConfigErrorKind::Parse,
                    "settings must be an object",
                ));
            }
        };
        let source_version = Some(
            object
                .get("version")
                .and_then(Value::as_u64)
                .map(|value| value as u32)
                .unwrap_or(1),
        );
        (
            version_present,
            revision_present,
            settings_present,
            settings_present_fields,
            source_version,
        )
    };
    validate_checked_value(&value)?;
    let wire: AppConfigWire = serde_json::from_value(value)
        .map_err(|_| ConfigError::new(ConfigErrorKind::Parse, "configuration shape is invalid"))?;
    let mut config = AppConfig::try_from(wire)?;
    config.version_present = version_present;
    config.revision_present = revision_present;
    config.settings_present = settings_present;
    config.source_version = source_version;
    apply_missing_settings_defaults(&mut config.settings, &settings_present_fields);
    config.settings.present_fields = settings_present_fields;
    if strict_validation {
        config.validate()?;
    }
    Ok(config)
}

fn reject_legacy_nul_arguments(value: &Value) -> Result<(), ConfigError> {
    fn walk(value: &Value) -> Result<(), ConfigError> {
        match value {
            Value::Object(object) => {
                for (key, child) in object {
                    if key == "args" {
                        if let Value::Array(arguments) = child {
                            if arguments.iter().any(|argument| {
                                argument
                                    .as_str()
                                    .is_some_and(|argument| argument.contains('\0'))
                            }) {
                                return Err(ConfigError::new(
                                    ConfigErrorKind::Validation,
                                    "legacy command arguments contain NUL and cannot be migrated",
                                ));
                            }
                        }
                    }
                    walk(child)?;
                }
            }
            Value::Array(values) => {
                for child in values {
                    walk(child)?;
                }
            }
            Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
        }
        Ok(())
    }

    walk(value)
}

fn settings_from_value(value: Value) -> Result<Settings, ConfigError> {
    let present_fields = value
        .as_object()
        .ok_or_else(|| ConfigError::new(ConfigErrorKind::Parse, "settings must be an object"))?
        .keys()
        .filter(|key| SETTINGS_FIELDS.contains(&key.as_str()))
        .cloned()
        .collect();
    validate_checked_value(&value)?;
    let wire: SettingsWire = serde_json::from_value(value)
        .map_err(|_| ConfigError::new(ConfigErrorKind::Parse, "settings shape is invalid"))?;
    let mut settings = Settings::from(wire);
    apply_missing_settings_defaults(&mut settings, &present_fields);
    settings.present_fields = present_fields;
    validate_settings(&settings)?;
    Ok(settings)
}

fn apply_missing_settings_defaults(settings: &mut Settings, present_fields: &BTreeSet<String>) {
    let defaults = Settings::default();
    if !present_fields.contains("restoreSessionOnStart") {
        settings.restore_session_on_start = defaults.restore_session_on_start.clone();
    }
    if !present_fields.contains("macTerminalProfile") {
        settings.mac_terminal_profile = defaults.mac_terminal_profile.clone();
    }
}

fn project_wire_value(project: &Project) -> Result<Value, ConfigError> {
    validate_project(project)?;
    checked_serialization_value(
        serde_json::to_value(ProjectWire::try_from(project)?).map_err(|_| {
            ConfigError::new(
                ConfigErrorKind::Parse,
                "configuration could not be serialized",
            )
        })?,
    )
}

fn project_folder_wire_value(folder: &ProjectFolder) -> Result<Value, ConfigError> {
    validate_project_folder(folder)?;
    checked_serialization_value(
        serde_json::to_value(ProjectFolderWire::try_from(folder)?).map_err(|_| {
            ConfigError::new(
                ConfigErrorKind::Parse,
                "configuration could not be serialized",
            )
        })?,
    )
}

fn run_command_wire_value(command: &RunCommand) -> Result<Value, ConfigError> {
    validate_run_command(command)?;
    checked_serialization_value(
        serde_json::to_value(RunCommandWire::try_from(command)?).map_err(|_| {
            ConfigError::new(
                ConfigErrorKind::Parse,
                "configuration could not be serialized",
            )
        })?,
    )
}

fn ssh_connection_wire_value(connection: &SSHConnection) -> Result<Value, ConfigError> {
    validate_ssh_connection(connection)?;
    checked_serialization_value(
        serde_json::to_value(SSHConnectionWire::try_from(connection)?).map_err(|_| {
            ConfigError::new(
                ConfigErrorKind::Parse,
                "configuration could not be serialized",
            )
        })?,
    )
}

fn ssh_auth_wire_value(auth: &SshAuth) -> Result<Value, ConfigError> {
    validate_ssh_auth(auth)?;
    checked_serialization_value(serde_json::to_value(SshAuthWire::try_from(auth)?).map_err(
        |_| {
            ConfigError::new(
                ConfigErrorKind::Parse,
                "configuration could not be serialized",
            )
        },
    )?)
}

fn default_directories_wire_value(directories: &DefaultDirectories) -> Result<Value, ConfigError> {
    validate_default_directories(directories)?;
    checked_serialization_value(
        serde_json::to_value(DefaultDirectoriesWire::from(directories)).map_err(|_| {
            ConfigError::new(
                ConfigErrorKind::Parse,
                "configuration could not be serialized",
            )
        })?,
    )
}

fn shell_options_wire_value(options: &ShellOptions) -> Result<Value, ConfigError> {
    validate_shell_options(options)?;
    checked_serialization_value(
        serde_json::to_value(ShellOptionsWire::from(options)).map_err(|_| {
            ConfigError::new(
                ConfigErrorKind::Parse,
                "configuration could not be serialized",
            )
        })?,
    )
}

fn editor_choice_wire_value(editor: &EditorChoice) -> Result<Value, ConfigError> {
    validate_editor_choice(editor)?;
    checked_serialization_value(
        serde_json::to_value(EditorChoiceWire::from(editor)).map_err(|_| {
            ConfigError::new(
                ConfigErrorKind::Parse,
                "configuration could not be serialized",
            )
        })?,
    )
}

impl Serialize for AppConfig {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.to_json_value()
            .map_err(SerError::custom)?
            .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for AppConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        app_config_from_value(bounded_value_from_deserializer(deserializer)?, true)
            .map_err(D::Error::custom)
    }
}

impl Serialize for Project {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        project_wire_value(self)
            .map_err(SerError::custom)?
            .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Project {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = bounded_value_from_deserializer(deserializer)?;
        let wire: ProjectWire = serde_json::from_value(value.clone()).map_err(D::Error::custom)?;
        let project = Project::try_from(wire).map_err(D::Error::custom)?;
        validate_project(&project).map_err(D::Error::custom)?;
        validate_checked_value(&value).map_err(D::Error::custom)?;
        Ok(project)
    }
}

impl Serialize for ProjectFolder {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        project_folder_wire_value(self)
            .map_err(SerError::custom)?
            .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ProjectFolder {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = bounded_value_from_deserializer(deserializer)?;
        let wire: ProjectFolderWire =
            serde_json::from_value(value.clone()).map_err(D::Error::custom)?;
        let folder = ProjectFolder::try_from(wire).map_err(D::Error::custom)?;
        validate_project_folder(&folder).map_err(D::Error::custom)?;
        validate_checked_value(&value).map_err(D::Error::custom)?;
        Ok(folder)
    }
}

impl Serialize for RunCommand {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        run_command_wire_value(self)
            .map_err(SerError::custom)?
            .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for RunCommand {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = bounded_value_from_deserializer(deserializer)?;
        let wire: RunCommandWire =
            serde_json::from_value(value.clone()).map_err(D::Error::custom)?;
        let command = RunCommand::try_from(wire).map_err(D::Error::custom)?;
        validate_run_command(&command).map_err(D::Error::custom)?;
        validate_checked_value(&value).map_err(D::Error::custom)?;
        Ok(command)
    }
}

impl Serialize for SSHConnection {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        ssh_connection_wire_value(self)
            .map_err(SerError::custom)?
            .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for SSHConnection {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = bounded_value_from_deserializer(deserializer)?;
        let wire: SSHConnectionWire =
            serde_json::from_value(value.clone()).map_err(D::Error::custom)?;
        let connection = SSHConnection::try_from(wire).map_err(D::Error::custom)?;
        validate_ssh_connection(&connection).map_err(D::Error::custom)?;
        validate_checked_value(&value).map_err(D::Error::custom)?;
        Ok(connection)
    }
}

impl Serialize for SshAuth {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        ssh_auth_wire_value(self)
            .map_err(SerError::custom)?
            .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for SshAuth {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = bounded_value_from_deserializer(deserializer)?;
        let wire: SshAuthWire = serde_json::from_value(value.clone()).map_err(D::Error::custom)?;
        let auth = SshAuth::try_from(wire).map_err(D::Error::custom)?;
        validate_ssh_auth(&auth).map_err(D::Error::custom)?;
        validate_checked_value(&value).map_err(D::Error::custom)?;
        Ok(auth)
    }
}

impl Serialize for DefaultDirectories {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        default_directories_wire_value(self)
            .map_err(SerError::custom)?
            .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for DefaultDirectories {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = bounded_value_from_deserializer(deserializer)?;
        let wire: DefaultDirectoriesWire =
            serde_json::from_value(value.clone()).map_err(D::Error::custom)?;
        let directories = DefaultDirectories::from(wire);
        validate_default_directories(&directories).map_err(D::Error::custom)?;
        validate_checked_value(&value).map_err(D::Error::custom)?;
        Ok(directories)
    }
}

impl Serialize for ShellOptions {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        shell_options_wire_value(self)
            .map_err(SerError::custom)?
            .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ShellOptions {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = bounded_value_from_deserializer(deserializer)?;
        let wire: ShellOptionsWire =
            serde_json::from_value(value.clone()).map_err(D::Error::custom)?;
        let options = ShellOptions::from(wire);
        validate_shell_options(&options).map_err(D::Error::custom)?;
        validate_checked_value(&value).map_err(D::Error::custom)?;
        Ok(options)
    }
}

impl Serialize for EditorChoice {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        editor_choice_wire_value(self)
            .map_err(SerError::custom)?
            .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for EditorChoice {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = bounded_value_from_deserializer(deserializer)?;
        let wire: EditorChoiceWire =
            serde_json::from_value(value.clone()).map_err(D::Error::custom)?;
        let editor = EditorChoice::from(wire);
        validate_editor_choice(&editor).map_err(D::Error::custom)?;
        validate_checked_value(&value).map_err(D::Error::custom)?;
        Ok(editor)
    }
}

struct RedactedText<'a>(&'a str);

impl fmt::Debug for RedactedText<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let _ = self.0;
        formatter.write_str("<redacted>")
    }
}

struct RedactedStrings<'a>(&'a [String]);

impl fmt::Debug for RedactedStrings<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut list = formatter.debug_list();
        for _ in self.0 {
            list.entry(&"<redacted>");
        }
        list.finish()
    }
}

struct RedactedMap<'a>(&'a JsonMap);

impl fmt::Debug for RedactedMap<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut map = formatter.debug_map();
        for _ in self.0.keys() {
            map.entry(&"<redacted>", &"<redacted>");
        }
        map.finish()
    }
}

impl fmt::Debug for AppConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AppConfig")
            .field("version", &self.version)
            .field("revision", &self.revision)
            .field("projects", &self.projects)
            .field("settings", &self.settings)
            .field("ssh_connections", &self.ssh_connections)
            .field("portal", &self.portal)
            .field("extra", &RedactedMap(&self.extra))
            .finish()
    }
}

impl fmt::Debug for Project {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Project")
            .field("id", &RedactedText(&self.id))
            .field("name", &RedactedText(&self.name))
            .field("root_path", &RedactedText(&self.root_path))
            .field("folders", &self.folders)
            .field("color", &self.color)
            .field("pinned", &self.pinned)
            .field("notes", &self.notes)
            .field("save_log_files", &self.save_log_files)
            .field("created_at", &RedactedText(&self.created_at))
            .field("updated_at", &RedactedText(&self.updated_at))
            .field("archived", &self.archived)
            .field("extra", &RedactedMap(&self.extra))
            .finish()
    }
}

impl fmt::Debug for ProjectFolder {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProjectFolder")
            .field("id", &RedactedText(&self.id))
            .field("name", &RedactedText(&self.name))
            .field("folder_path", &RedactedText(&self.folder_path))
            .field("commands", &self.commands)
            .field("env_file_path", &self.env_file_path)
            .field("port_variable", &self.port_variable)
            .field("hidden", &self.hidden)
            .field("archived", &self.archived)
            .field("extra", &RedactedMap(&self.extra))
            .finish()
    }
}

impl fmt::Debug for RunCommand {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RunCommand")
            .field("id", &RedactedText(&self.id))
            .field("label", &RedactedText(&self.label))
            .field("command", &RedactedText(&self.command))
            .field("args", &RedactedStrings(&self.args))
            .field("env", &self.env)
            .field("port", &self.port)
            .field("auto_restart", &self.auto_restart)
            .field("clear_logs_on_restart", &self.clear_logs_on_restart)
            .field("archived", &self.archived)
            .field("extra", &RedactedMap(&self.extra))
            .finish()
    }
}

impl fmt::Debug for SSHConnection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SSHConnection")
            .field("id", &RedactedText(&self.id))
            .field("label", &RedactedText(&self.label))
            .field("host", &RedactedText(&self.host))
            .field("port", &self.port)
            .field("username", &RedactedText(&self.username))
            .field("auth", &self.auth)
            .field("archived", &self.archived)
            .field("extra", &RedactedMap(&self.extra))
            .finish()
    }
}

impl fmt::Debug for SshAuth {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SshAuth")
            .field("mode", &self.mode)
            .field("credential_ref", &self.credential_ref)
            .field("extra", &RedactedMap(&self.extra))
            .finish()
    }
}

impl fmt::Debug for Settings {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Settings")
            .field("theme", &RedactedText(&self.theme))
            .field("log_buffer_size", &self.log_buffer_size)
            .field("confirm_on_close", &self.confirm_on_close)
            .field("minimize_to_tray", &self.minimize_to_tray)
            .field("restore_session_on_start", &self.restore_session_on_start)
            .field("default_terminal", &self.default_terminal)
            .field("mac_terminal_profile", &self.mac_terminal_profile)
            .field("claude_command", &self.claude_command)
            .field("codex_command", &self.codex_command)
            .field("notification_sound", &self.notification_sound)
            .field("terminal_font_size", &self.terminal_font_size)
            .field("option_as_meta", &self.option_as_meta)
            .field("copy_on_select", &self.copy_on_select)
            .field("keep_selection_on_copy", &self.keep_selection_on_copy)
            .field("show_terminal_scrollbar", &self.show_terminal_scrollbar)
            .field("shell_integration_enabled", &self.shell_integration_enabled)
            .field("terminal_mouse_override", &self.terminal_mouse_override)
            .field("terminal_read_only", &self.terminal_read_only)
            .field("browser_enabled", &self.browser_enabled)
            .field("github_token_ref", &self.github_token_ref)
            .field("default_directories", &self.default_directories)
            .field("shell_options", &self.shell_options)
            .field("editor", &self.editor)
            .field("extra", &RedactedMap(&self.extra))
            .finish()
    }
}

impl fmt::Debug for DefaultDirectories {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DefaultDirectories")
            .field("projects", &self.projects)
            .field("worktrees", &self.worktrees)
            .field("exports", &self.exports)
            .field("extra", &RedactedMap(&self.extra))
            .finish()
    }
}

impl fmt::Debug for ShellOptions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ShellOptions")
            .field("program", &self.program)
            .field("args", &self.args)
            .field("inherit_environment", &self.inherit_environment)
            .field("integration_enabled", &self.integration_enabled)
            .field("extra", &RedactedMap(&self.extra))
            .finish()
    }
}

impl fmt::Debug for EditorChoice {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EditorChoice")
            .field("kind", &RedactedText(&self.kind))
            .field("command", &self.command)
            .field("args", &self.args)
            .finish()
    }
}

impl fmt::Debug for ConfigCommand {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::CreateProject { .. } => "CreateProject",
            Self::UpdateProject { .. } => "UpdateProject",
            Self::ReorderProject { .. } => "ReorderProject",
            Self::ArchiveProject { .. } => "ArchiveProject",
            Self::CreateFolder { .. } => "CreateFolder",
            Self::UpdateFolder { .. } => "UpdateFolder",
            Self::ReorderFolder { .. } => "ReorderFolder",
            Self::ArchiveFolder { .. } => "ArchiveFolder",
            Self::CreateCommand { .. } => "CreateCommand",
            Self::UpdateCommand { .. } => "UpdateCommand",
            Self::ReorderCommand { .. } => "ReorderCommand",
            Self::ArchiveCommand { .. } => "ArchiveCommand",
            Self::CreateSsh { .. } => "CreateSsh",
            Self::UpdateSsh { .. } => "UpdateSsh",
            Self::ReorderSsh { .. } => "ReorderSsh",
            Self::ArchiveSsh { .. } => "ArchiveSsh",
            Self::PatchSettings { .. } => "PatchSettings",
        };
        formatter.debug_tuple(name).field(&"<redacted>").finish()
    }
}

pub(crate) fn validate_id(id: &str) -> Result<(), ConfigError> {
    if id.is_empty() || id.len() > MAX_ID_BYTES || id != id.trim() {
        return Err(ConfigError::new(
            ConfigErrorKind::Validation,
            "configuration ID is empty or too large",
        ));
    }
    Ok(())
}

fn validate_text(value: &str) -> Result<(), ConfigError> {
    if value.is_empty() || value.len() > MAX_TEXT_BYTES || value.chars().any(char::is_control) {
        return Err(ConfigError::new(
            ConfigErrorKind::Validation,
            "configuration text is empty, contains control characters, or exceeds the size limit",
        ));
    }
    Ok(())
}

fn validate_notes(value: &str) -> Result<(), ConfigError> {
    if value.is_empty()
        || value.len() > MAX_TEXT_BYTES
        || value
            .chars()
            .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
    {
        return Err(ConfigError::new(
            ConfigErrorKind::Validation,
            "configuration note is empty, contains unsafe control characters, or exceeds the size limit",
        ));
    }
    Ok(())
}

fn validate_reorder_index(index: usize) -> Result<(), ConfigError> {
    if index > MAX_COLLECTION_ITEMS {
        return Err(ConfigError::new(
            ConfigErrorKind::Validation,
            "reorder target is outside the collection",
        ));
    }
    Ok(())
}

fn validate_env_key(key: &str) -> Result<(), ConfigError> {
    let mut bytes = key.bytes();
    let Some(first) = bytes.next() else {
        return Err(ConfigError::new(
            ConfigErrorKind::Validation,
            "environment key has invalid grammar",
        ));
    };
    if !(first == b'_' || first.is_ascii_alphabetic())
        || !bytes.all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
    {
        return Err(ConfigError::new(
            ConfigErrorKind::Validation,
            "environment key has invalid grammar",
        ));
    }
    Ok(())
}

fn validate_run_command(command: &RunCommand) -> Result<(), ConfigError> {
    validate_id(&command.id)?;
    validate_extra_map(&command.extra)?;
    validate_text(&command.label)?;
    validate_text(&command.command)?;
    if command.args.len() > MAX_COLLECTION_ITEMS {
        return Err(ConfigError::new(
            ConfigErrorKind::Validation,
            "argument collection exceeds the item limit",
        ));
    }
    for argument in &command.args {
        validate_text(argument)?;
    }
    if let Nullable::Value(port) = &command.port {
        if *port == 0 {
            return Err(ConfigError::new(
                ConfigErrorKind::Validation,
                "configured command port must be nonzero",
            ));
        }
    }
    if let Nullable::Value(env) = &command.env {
        if env.len() > MAX_COLLECTION_ITEMS {
            return Err(ConfigError::new(
                ConfigErrorKind::Validation,
                "environment collection exceeds the item limit",
            ));
        }
        for (key, value) in env {
            validate_env_key(key)?;
            validate_text(value)?;
        }
    }
    Ok(())
}

fn validate_project_folder(folder: &ProjectFolder) -> Result<(), ConfigError> {
    validate_id(&folder.id)?;
    validate_extra_map(&folder.extra)?;
    validate_text(&folder.name)?;
    validate_text(&folder.folder_path)?;
    validate_nullable_text(&folder.env_file_path)?;
    validate_nullable_text(&folder.port_variable)?;
    if folder.commands.len() > MAX_COLLECTION_ITEMS {
        return Err(ConfigError::new(
            ConfigErrorKind::Validation,
            "command collection exceeds the item limit",
        ));
    }
    let mut command_ids = BTreeMap::new();
    for command in &folder.commands {
        validate_run_command(command)?;
        if command_ids.insert(command.id.as_str(), ()).is_some() {
            return Err(ConfigError::new(
                ConfigErrorKind::Validation,
                "configuration contains duplicate command IDs",
            ));
        }
    }
    Ok(())
}

fn validate_ssh_auth(auth: &SshAuth) -> Result<(), ConfigError> {
    validate_extra_map(&auth.extra)?;
    match auth.mode {
        SshAuthMode::Default | SshAuthMode::Agent => {
            if matches!(auth.credential_ref, Nullable::Value(_)) {
                return Err(ConfigError::new(
                    ConfigErrorKind::Validation,
                    "SSH authentication has an unexpected credential reference",
                ));
            }
        }
        SshAuthMode::Password | SshAuthMode::PrivateKey => {
            let Nullable::Value(reference) = &auth.credential_ref else {
                return Err(ConfigError::new(
                    ConfigErrorKind::Validation,
                    "SSH authentication is incomplete",
                ));
            };
            validate_credential_reference(reference)?;
        }
    }
    Ok(())
}

fn validate_credential_reference(reference: &str) -> Result<(), ConfigError> {
    if !is_opaque_reference(reference) {
        return Err(ConfigError::new(
            ConfigErrorKind::SecretMaterial,
            "secret reference is not an opaque credential reference",
        ));
    }
    if reference.starts_with(LEGACY_SSH_CREDENTIAL_PREFIX) {
        decode_legacy_ssh_credential(reference)?.ok_or_else(|| {
            ConfigError::new(
                ConfigErrorKind::Validation,
                "legacy SSH credential reference is malformed",
            )
        })?;
    }
    Ok(())
}

fn validate_default_directories(directories: &DefaultDirectories) -> Result<(), ConfigError> {
    validate_extra_map(&directories.extra)?;
    validate_nullable_text(&directories.projects)?;
    validate_nullable_text(&directories.worktrees)?;
    validate_nullable_text(&directories.exports)
}

fn validate_shell_options(options: &ShellOptions) -> Result<(), ConfigError> {
    validate_extra_map(&options.extra)?;
    validate_nullable_text(&options.program)?;
    if let Nullable::Value(args) = &options.args {
        if args.len() > MAX_COLLECTION_ITEMS {
            return Err(ConfigError::new(
                ConfigErrorKind::Validation,
                "shell argument collection exceeds the item limit",
            ));
        }
        for argument in args {
            validate_text(argument)?;
        }
    }
    Ok(())
}

fn validate_editor_choice(editor: &EditorChoice) -> Result<(), ConfigError> {
    validate_extra_map(&editor.extra)?;
    validate_text(&editor.kind)?;
    match editor.kind.as_str() {
        "command" => {
            let Nullable::Value(command) = &editor.command else {
                return Err(ConfigError::new(
                    ConfigErrorKind::Validation,
                    "command editor requires a command",
                ));
            };
            validate_text(command)?;
        }
        _ if matches!(editor.command, Nullable::Value(_))
            || matches!(editor.args, Nullable::Value(_)) =>
        {
            return Err(ConfigError::new(
                ConfigErrorKind::Validation,
                "editor command and arguments do not match the editor kind",
            ));
        }
        _ => {}
    }
    if let Nullable::Value(args) = &editor.args {
        if args.len() > MAX_COLLECTION_ITEMS {
            return Err(ConfigError::new(
                ConfigErrorKind::Validation,
                "editor argument collection exceeds the item limit",
            ));
        }
        for argument in args {
            validate_text(argument)?;
        }
    }
    Ok(())
}

fn validate_project(project: &Project) -> Result<(), ConfigError> {
    validate_id(&project.id)?;
    validate_extra_map(&project.extra)?;
    validate_text(&project.name)?;
    validate_text(&project.root_path)?;
    validate_text(&project.created_at)?;
    validate_text(&project.updated_at)?;
    validate_nullable_text(&project.color)?;
    if let Nullable::Value(notes) = &project.notes {
        validate_notes(notes)?;
    }
    if project.folders.len() > MAX_COLLECTION_ITEMS {
        return Err(ConfigError::new(
            ConfigErrorKind::Validation,
            "folder collection exceeds the item limit",
        ));
    }

    let mut ids = BTreeMap::new();
    for folder in &project.folders {
        validate_id(&folder.id)?;
        if ids.insert(folder.id.as_str(), ()).is_some() {
            return Err(ConfigError::new(
                ConfigErrorKind::Validation,
                "configuration contains duplicate folder IDs",
            ));
        }
        validate_project_folder(folder)?;
        let mut command_ids = BTreeMap::new();
        for command in &folder.commands {
            validate_id(&command.id)?;
            if command_ids.insert(command.id.as_str(), ()).is_some() {
                return Err(ConfigError::new(
                    ConfigErrorKind::Validation,
                    "configuration contains duplicate command IDs",
                ));
            }
            validate_run_command(command)?;
        }
    }
    Ok(())
}

fn validate_ssh_connection(connection: &SSHConnection) -> Result<(), ConfigError> {
    validate_id(&connection.id)?;
    validate_extra_map(&connection.extra)?;
    validate_text(&connection.label)?;
    if connection.port == 0 {
        return Err(ConfigError::new(
            ConfigErrorKind::Validation,
            "SSH port must be nonzero",
        ));
    }
    validate_text(&connection.host)?;
    validate_text(&connection.username)?;
    if let Nullable::Value(auth) = &connection.auth {
        validate_ssh_auth(auth)?;
    }
    Ok(())
}

fn validate_settings(settings: &Settings) -> Result<(), ConfigError> {
    validate_extra_map(&settings.extra)?;
    validate_text(&settings.theme)?;
    validate_nullable_text(&settings.claude_command)?;
    validate_nullable_text(&settings.codex_command)?;
    validate_nullable_text(&settings.notification_sound)?;
    if let Nullable::Value(reference) = &settings.github_token_ref {
        validate_credential_reference(reference)?;
    }
    if let Nullable::Value(directories) = &settings.default_directories {
        validate_default_directories(directories)?;
    }
    if let Nullable::Value(shell) = &settings.shell_options {
        validate_shell_options(shell)?;
    }
    if let Nullable::Value(editor) = &settings.editor {
        validate_editor_choice(editor)?;
    }
    Ok(())
}

fn validate_nullable_text<T: AsRef<str>>(value: &Nullable<T>) -> Result<(), ConfigError> {
    if let Nullable::Value(value) = value {
        validate_text(value.as_ref())?;
    }
    Ok(())
}

fn validate_extra_map(extra: &JsonMap) -> Result<(), ConfigError> {
    if extra.len() > MAX_JSON_OBJECT_FIELDS {
        return Err(ConfigError::new(
            ConfigErrorKind::Validation,
            "configuration extra map exceeds the field limit",
        ));
    }
    for (key, value) in extra {
        if key.is_empty() || key.len() > MAX_TEXT_BYTES || key.chars().any(char::is_control) {
            return Err(ConfigError::new(
                ConfigErrorKind::Validation,
                "configuration extra map key is invalid",
            ));
        }
        let normalized: String = key
            .chars()
            .filter(|character| character.is_ascii_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect();
        if normalized.ends_with("ref") {
            match value {
                Value::Null => {}
                Value::String(reference) => validate_credential_reference(reference)?,
                _ => {
                    return Err(ConfigError::new(
                        ConfigErrorKind::SecretMaterial,
                        "secret reference is not an opaque credential reference",
                    ));
                }
            }
        } else if (normalized.contains("password")
            || normalized.contains("privatekey")
            || normalized == "secret"
            || normalized.contains("token"))
            && !matches!(value, Value::Null)
        {
            return Err(ConfigError::new(
                ConfigErrorKind::SecretMaterial,
                "raw secret material is not accepted",
            ));
        }
        reject_secret_material(value)?;
        validate_json_shape(value, 0)?;
    }
    Ok(())
}

fn reject_secret_material(value: &Value) -> Result<(), ConfigError> {
    fn walk(value: &Value) -> Result<(), ConfigError> {
        match value {
            Value::Object(object) => {
                for (key, child) in object {
                    let normalized: String = key
                        .chars()
                        .filter(|character| character.is_ascii_alphanumeric())
                        .flat_map(char::to_lowercase)
                        .collect();
                    let is_reference = normalized.ends_with("ref");
                    if is_reference {
                        match child {
                            Value::Null => {}
                            Value::String(reference) => validate_credential_reference(reference)?,
                            _ => {
                                return Err(ConfigError::new(
                                    ConfigErrorKind::SecretMaterial,
                                    "secret reference is not an opaque credential reference",
                                ));
                            }
                        }
                    }
                    if !is_reference
                        && (normalized.contains("password")
                            || normalized.contains("privatekey")
                            || normalized == "secret"
                            || normalized.contains("token"))
                        && !matches!(child, Value::Null)
                    {
                        return Err(ConfigError::new(
                            ConfigErrorKind::SecretMaterial,
                            "raw secret material is not accepted",
                        ));
                    }
                    walk(child)?;
                }
            }
            Value::Array(values) => {
                for child in values {
                    walk(child)?;
                }
            }
            Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
        }
        Ok(())
    }

    walk(value)
}

fn is_opaque_reference(value: &str) -> bool {
    let Some(payload) = value.strip_prefix("credential:") else {
        return false;
    };
    let max_payload = if value.starts_with(LEGACY_SSH_CREDENTIAL_PREFIX) {
        MAX_LEGACY_SSH_REFERENCE_BYTES
    } else {
        MAX_ID_BYTES
    };
    !payload.is_empty()
        && payload.len() <= max_payload
        && payload
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
}

fn validate_json_shape(value: &Value, depth: usize) -> Result<(), ConfigError> {
    if depth > MAX_JSON_DEPTH {
        return Err(ConfigError::new(
            ConfigErrorKind::Validation,
            "configuration nesting exceeds the depth limit",
        ));
    }
    match value {
        Value::Object(object) => {
            if object.len() > MAX_COLLECTION_ITEMS {
                return Err(ConfigError::new(
                    ConfigErrorKind::Validation,
                    "configuration object exceeds the field limit",
                ));
            }
            for child in object.values() {
                validate_json_shape(child, depth + 1)?;
            }
        }
        Value::Array(values) => {
            if values.len() > MAX_COLLECTION_ITEMS {
                return Err(ConfigError::new(
                    ConfigErrorKind::Validation,
                    "configuration array exceeds the item limit",
                ));
            }
            for child in values {
                validate_json_shape(child, depth + 1)?;
            }
        }
        Value::String(string) if string.len() > MAX_TEXT_BYTES => {
            return Err(ConfigError::new(
                ConfigErrorKind::Validation,
                "configuration string exceeds the size limit",
            ));
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
    Ok(())
}
