//! Provider settings document model and validation.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

use crate::providers::ProviderKind;

pub const CLAUDE_DEFAULT_INSTANCE_ID: &str = "claude";
pub const CODEX_DEFAULT_INSTANCE_ID: &str = "codex";
pub const CURSOR_DEFAULT_INSTANCE_ID: &str = "cursor";

/// Default scheduled health interval (seconds). `0` means manual-only.
pub const DEFAULT_HEALTH_INTERVAL_SECS: u64 = 300;

const MAX_INSTANCE_ID_LEN: usize = 64;
const MAX_ENV_NAME_LEN: usize = 128;
const MAX_ENV_VALUE_LEN: usize = 16_384;
const MAX_DISPLAY_NAME_LEN: usize = 128;
const MAX_MODEL_SLUG_LEN: usize = 128;
const MAX_CUSTOM_MODELS: usize = 64;
const MAX_ENV_VARS: usize = 64;
const MAX_LAUNCH_ARGS: usize = 64;
const MAX_LAUNCH_ARG_LEN: usize = 1_024;

#[derive(Clone, PartialEq, Eq)]
pub enum ProviderSettingsError {
    InvalidInstanceId(String),
    InvalidEnvName(String),
    EnvValueTooLong,
    TooManyEnvVars,
    DuplicateEnvName(String),
    InvalidModelSlug(String),
    UnknownModel(String),
    TooManyCustomModels,
    DuplicateCustomModel(String),
    BuiltinModelCollision(String),
    StubCannotEnable(String),
    UnknownInstance(String),
    InstanceDisabled(String),
    ReservedLaunchArg(String),
    ReservedEnvKey(String),
    DisplayNameTooLong,
    TooManyLaunchArgs,
    LaunchArgTooLong,
    ImmutableBuiltinDriver,
    DuplicateInstanceId(String),
    StaleRevision { expected: u64, actual: u64 },
    Corrupt(String),
}

impl std::fmt::Display for ProviderSettingsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidInstanceId(id) => write!(f, "invalid provider instance id `{id}`"),
            Self::InvalidEnvName(name) => write!(f, "invalid environment variable name `{name}`"),
            Self::EnvValueTooLong => write!(f, "environment variable value exceeds bound"),
            Self::TooManyEnvVars => write!(f, "too many environment variables"),
            Self::DuplicateEnvName(name) => {
                write!(f, "duplicate environment variable name `{name}`")
            }
            Self::InvalidModelSlug(slug) => write!(f, "invalid model slug `{slug}`"),
            Self::UnknownModel(slug) => {
                write!(
                    f,
                    "selected model `{slug}` is not a known builtin or custom model"
                )
            }
            Self::TooManyCustomModels => write!(f, "too many custom models"),
            Self::DuplicateCustomModel(slug) => write!(f, "duplicate custom model slug `{slug}`"),
            Self::BuiltinModelCollision(slug) => {
                write!(f, "custom model slug collides with builtin `{slug}`")
            }
            Self::StubCannotEnable(name) => {
                write!(f, "stub provider `{name}` cannot be enabled or launched")
            }
            Self::UnknownInstance(id) => write!(f, "unknown provider instance `{id}`"),
            Self::InstanceDisabled(id) => write!(f, "provider instance `{id}` is disabled"),
            Self::ReservedLaunchArg(arg) => {
                write!(f, "launch argument overrides a reserved control: `{arg}`")
            }
            Self::ReservedEnvKey(key) => {
                write!(
                    f,
                    "environment key overrides a reserved identity control: `{key}`"
                )
            }
            Self::DisplayNameTooLong => write!(f, "display name exceeds bound"),
            Self::TooManyLaunchArgs => write!(f, "too many launch arguments"),
            Self::LaunchArgTooLong => write!(f, "launch argument exceeds bound"),
            Self::ImmutableBuiltinDriver => {
                write!(f, "builtin provider instance driver cannot be changed")
            }
            Self::DuplicateInstanceId(id) => {
                write!(f, "provider instance id `{id}` already exists")
            }
            Self::StaleRevision { expected, actual } => {
                write!(
                    f,
                    "stale provider settings revision: expected {expected}, got {actual}"
                )
            }
            Self::Corrupt(msg) => write!(f, "corrupt provider settings: {msg}"),
        }
    }
}

impl std::fmt::Debug for ProviderSettingsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}

impl std::error::Error for ProviderSettingsError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderDriverKind {
    Claude,
    Codex,
    Cursor,
    Grok,
    OpenCode,
}

impl ProviderDriverKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Cursor => "cursor",
            Self::Grok => "grok",
            Self::OpenCode => "opencode",
        }
    }

    pub fn is_stub(self) -> bool {
        matches!(self, Self::Grok | Self::OpenCode)
    }

    pub fn is_builtin_supported(self) -> bool {
        matches!(self, Self::Claude | Self::Codex | Self::Cursor)
    }

    pub fn to_provider_kind(self) -> Option<ProviderKind> {
        match self {
            Self::Claude => Some(ProviderKind::ClaudeCode),
            Self::Codex => Some(ProviderKind::Codex),
            Self::Cursor => Some(ProviderKind::Cursor),
            Self::Grok | Self::OpenCode => None,
        }
    }

    pub fn from_provider_kind(kind: ProviderKind) -> Self {
        match kind {
            ProviderKind::ClaudeCode => Self::Claude,
            ProviderKind::Codex => Self::Codex,
            ProviderKind::Cursor => Self::Cursor,
        }
    }
}

/// Builtin drivers that ship with real adapter support.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltinProviderDriver {
    Claude,
    Codex,
    Cursor,
}

impl From<BuiltinProviderDriver> for ProviderDriverKind {
    fn from(value: BuiltinProviderDriver) -> Self {
        match value {
            BuiltinProviderDriver::Claude => Self::Claude,
            BuiltinProviderDriver::Codex => Self::Codex,
            BuiltinProviderDriver::Cursor => Self::Cursor,
        }
    }
}

/// Stub catalog entries — visible, never enableable/launchable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StubProviderDriver {
    Grok,
    OpenCode,
}

impl From<StubProviderDriver> for ProviderDriverKind {
    fn from(value: StubProviderDriver) -> Self {
        match value {
            StubProviderDriver::Grok => Self::Grok,
            StubProviderDriver::OpenCode => Self::OpenCode,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProviderInstanceId(String);

impl ProviderInstanceId {
    pub fn new(raw: impl Into<String>) -> Result<Self, ProviderSettingsError> {
        let id = raw.into();
        validate_instance_id(&id)?;
        Ok(Self(id))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ProviderInstanceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

pub fn validate_instance_id(id: &str) -> Result<(), ProviderSettingsError> {
    if id.is_empty() || id.len() > MAX_INSTANCE_ID_LEN {
        return Err(ProviderSettingsError::InvalidInstanceId(id.to_string()));
    }
    let mut chars = id.chars();
    let Some(first) = chars.next() else {
        return Err(ProviderSettingsError::InvalidInstanceId(id.to_string()));
    };
    if !first.is_ascii_alphabetic() {
        return Err(ProviderSettingsError::InvalidInstanceId(id.to_string()));
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return Err(ProviderSettingsError::InvalidInstanceId(id.to_string()));
    }
    Ok(())
}

pub fn validate_env_name(name: &str) -> Result<(), ProviderSettingsError> {
    if name.is_empty() || name.len() > MAX_ENV_NAME_LEN {
        return Err(ProviderSettingsError::InvalidEnvName(name.to_string()));
    }
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return Err(ProviderSettingsError::InvalidEnvName(name.to_string()));
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return Err(ProviderSettingsError::InvalidEnvName(name.to_string()));
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(ProviderSettingsError::InvalidEnvName(name.to_string()));
    }
    Ok(())
}

pub fn validate_model_slug(slug: &str) -> Result<(), ProviderSettingsError> {
    let trimmed = slug.trim();
    if trimmed.is_empty() || trimmed.len() > MAX_MODEL_SLUG_LEN {
        return Err(ProviderSettingsError::InvalidModelSlug(slug.to_string()));
    }
    // Provider aliases may include bracket suffixes (e.g. `claude-opus-5[1m]`).
    // Characters stay bounded argv-safe tokens; never shell-interpolated.
    if !trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | ':' | '[' | ']'))
    {
        return Err(ProviderSettingsError::InvalidModelSlug(slug.to_string()));
    }
    if trimmed.contains("[[")
        || trimmed.contains("]]")
        || trimmed.matches('[').count() != trimmed.matches(']').count()
    {
        return Err(ProviderSettingsError::InvalidModelSlug(slug.to_string()));
    }
    Ok(())
}

fn is_immutable_catalog_id(id: &str) -> bool {
    matches!(
        id,
        CLAUDE_DEFAULT_INSTANCE_ID
            | CODEX_DEFAULT_INSTANCE_ID
            | CURSOR_DEFAULT_INSTANCE_ID
            | "grok"
            | "opencode"
    )
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderEnvVar {
    pub name: String,
    /// Plaintext only in memory after reveal; never serialized when sensitive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    pub sensitive: bool,
    /// OS-protected blob when `sensitive`; omitted from UI/log projections.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protected_value: Option<String>,
    /// UI/redacted projection flag — true when a secret exists but value is withheld.
    #[serde(default)]
    pub value_redacted: bool,
}

impl PartialEq for ProviderEnvVar {
    fn eq(&self, other: &Self) -> bool {
        // Reflexive: compare all fields equally. Sensitive sealed rows keep
        // `value = None` and `protected_value = Some(...)`.
        self.name == other.name
            && self.sensitive == other.sensitive
            && self.value_redacted == other.value_redacted
            && self.protected_value == other.protected_value
            && self.value == other.value
    }
}

impl Eq for ProviderEnvVar {}

impl std::fmt::Debug for ProviderEnvVar {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderEnvVar")
            .field("name", &self.name)
            .field("sensitive", &self.sensitive)
            .field("value_redacted", &self.value_redacted)
            .field(
                "value",
                &if self.sensitive {
                    "<redacted>"
                } else {
                    self.value.as_deref().unwrap_or("")
                },
            )
            .field(
                "protected_value",
                &self.protected_value.as_ref().map(|_| "<protected>"),
            )
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomModelEntry {
    pub slug: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ModelVisibilityPolicy {
    /// Builtin slugs the user hid from the picker.
    #[serde(default)]
    pub hidden_builtins: Vec<String>,
    /// Ordered favorite slugs (custom or builtin). Favorites appear first.
    #[serde(default)]
    pub favorite_order: Vec<String>,
    /// Optional full-catalog display order (settings UI). Compatible with older
    /// documents that only persisted `favorite_order`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub catalog_order: Vec<String>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderInstanceConfig {
    pub instance_id: ProviderInstanceId,
    pub driver: ProviderDriverKind,
    pub enabled: bool,
    pub display_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accent_color: Option<String>,
    #[serde(default)]
    pub environment: Vec<ProviderEnvVar>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shadow_home_path: Option<String>,
    #[serde(default)]
    pub launch_args: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_endpoint: Option<String>,
    #[serde(default)]
    pub custom_models: Vec<CustomModelEntry>,
    #[serde(default)]
    pub model_policy: ModelVisibilityPolicy,
    /// Unknown driver-config fields preserved across round-trips.
    #[serde(default, flatten)]
    pub unknown: BTreeMap<String, JsonValue>,
}

impl PartialEq for ProviderInstanceConfig {
    fn eq(&self, other: &Self) -> bool {
        self.instance_id == other.instance_id
            && self.driver == other.driver
            && self.enabled == other.enabled
            && self.display_name == other.display_name
            && self.accent_color == other.accent_color
            && self.environment == other.environment
            && self.binary_path == other.binary_path
            && self.home_path == other.home_path
            && self.shadow_home_path == other.shadow_home_path
            && self.launch_args == other.launch_args
            && self.api_endpoint == other.api_endpoint
            && self.custom_models == other.custom_models
            && self.model_policy == other.model_policy
            && self.unknown == other.unknown
    }
}

impl Eq for ProviderInstanceConfig {}

impl std::fmt::Debug for ProviderInstanceConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderInstanceConfig")
            .field("instance_id", &self.instance_id)
            .field("driver", &self.driver)
            .field("enabled", &self.enabled)
            .field("display_name", &self.display_name)
            .field("accent_color", &self.accent_color)
            .field("environment", &self.environment)
            .field("binary_path", &self.binary_path)
            .field("home_path", &self.home_path)
            .field("shadow_home_path", &self.shadow_home_path)
            .field("launch_args", &self.launch_args)
            .field("api_endpoint", &self.api_endpoint)
            .field("custom_models", &self.custom_models)
            .field("model_policy", &self.model_policy)
            .field("unknown_keys", &self.unknown.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl ProviderInstanceConfig {
    pub fn builtin_default(driver: BuiltinProviderDriver) -> Self {
        let (id, name) = match driver {
            BuiltinProviderDriver::Claude => (CLAUDE_DEFAULT_INSTANCE_ID, "Claude"),
            BuiltinProviderDriver::Codex => (CODEX_DEFAULT_INSTANCE_ID, "Codex"),
            BuiltinProviderDriver::Cursor => (CURSOR_DEFAULT_INSTANCE_ID, "Cursor"),
        };
        Self {
            instance_id: ProviderInstanceId::new(id).expect("builtin id"),
            driver: driver.into(),
            enabled: true,
            display_name: name.to_string(),
            accent_color: None,
            environment: Vec::new(),
            binary_path: None,
            home_path: None,
            shadow_home_path: None,
            launch_args: Vec::new(),
            api_endpoint: None,
            custom_models: Vec::new(),
            model_policy: ModelVisibilityPolicy::default(),
            unknown: BTreeMap::new(),
        }
    }

    pub fn stub_catalog(stub: StubProviderDriver) -> Self {
        let (id, name) = match stub {
            StubProviderDriver::Grok => ("grok", "Grok"),
            StubProviderDriver::OpenCode => ("opencode", "OpenCode"),
        };
        Self {
            instance_id: ProviderInstanceId::new(id).expect("stub id"),
            driver: stub.into(),
            enabled: false,
            display_name: name.to_string(),
            accent_color: None,
            environment: Vec::new(),
            binary_path: None,
            home_path: None,
            shadow_home_path: None,
            launch_args: Vec::new(),
            api_endpoint: None,
            custom_models: Vec::new(),
            model_policy: ModelVisibilityPolicy::default(),
            unknown: BTreeMap::new(),
        }
    }

    pub fn validate(&self) -> Result<(), ProviderSettingsError> {
        validate_instance_id(self.instance_id.as_str())?;
        let catalog_driver = match self.instance_id.as_str() {
            CLAUDE_DEFAULT_INSTANCE_ID => Some(ProviderDriverKind::Claude),
            CODEX_DEFAULT_INSTANCE_ID => Some(ProviderDriverKind::Codex),
            CURSOR_DEFAULT_INSTANCE_ID => Some(ProviderDriverKind::Cursor),
            "grok" => Some(ProviderDriverKind::Grok),
            "opencode" => Some(ProviderDriverKind::OpenCode),
            _ => None,
        };
        if catalog_driver.is_some_and(|driver| driver != self.driver) {
            return Err(ProviderSettingsError::ImmutableBuiltinDriver);
        }
        if self.display_name.len() > MAX_DISPLAY_NAME_LEN {
            return Err(ProviderSettingsError::DisplayNameTooLong);
        }
        if self.driver.is_stub() && self.enabled {
            return Err(ProviderSettingsError::StubCannotEnable(
                self.driver.as_str().to_string(),
            ));
        }
        if self.environment.len() > MAX_ENV_VARS {
            return Err(ProviderSettingsError::TooManyEnvVars);
        }
        let mut env_names = std::collections::BTreeSet::new();
        for env in &self.environment {
            validate_env_name(&env.name)?;
            let key = if cfg!(windows) {
                env.name.to_ascii_uppercase()
            } else {
                env.name.clone()
            };
            if !env_names.insert(key) {
                return Err(ProviderSettingsError::DuplicateEnvName(env.name.clone()));
            }
            reject_reserved_env_key(&env.name)?;
            if let Some(value) = &env.value {
                if value.len() > MAX_ENV_VALUE_LEN {
                    return Err(ProviderSettingsError::EnvValueTooLong);
                }
            }
        }
        if self.launch_args.len() > MAX_LAUNCH_ARGS {
            return Err(ProviderSettingsError::TooManyLaunchArgs);
        }
        for arg in &self.launch_args {
            if arg.len() > MAX_LAUNCH_ARG_LEN {
                return Err(ProviderSettingsError::LaunchArgTooLong);
            }
            reject_reserved_launch_arg(self.driver, arg)?;
        }
        if self.custom_models.len() > MAX_CUSTOM_MODELS {
            return Err(ProviderSettingsError::TooManyCustomModels);
        }
        let builtins: std::collections::BTreeSet<String> =
            builtin_slugs_for_driver(self.driver).into_iter().collect();
        let mut seen = std::collections::BTreeSet::new();
        for model in &self.custom_models {
            let slug = normalize_model_slug(&model.slug)?;
            if builtins.contains(&slug) {
                return Err(ProviderSettingsError::BuiltinModelCollision(slug));
            }
            if !seen.insert(slug.clone()) {
                return Err(ProviderSettingsError::DuplicateCustomModel(slug));
            }
        }
        Ok(())
    }

    pub fn redacted_projection(&self) -> Self {
        let mut clone = self.clone();
        for env in &mut clone.environment {
            if env.sensitive {
                let has_secret = env.protected_value.is_some()
                    || env.value.as_ref().is_some_and(|v| !v.is_empty())
                    || env.value_redacted;
                env.value = None;
                env.protected_value = None;
                env.value_redacted = has_secret;
            }
        }
        clone
    }

    /// Accept the original empty built-in identity without weakening configured
    /// instances. New bindings always use the framed fingerprint.
    pub fn matches_launch_identity_fingerprint(&self, expected: &str) -> bool {
        use sha2::{Digest, Sha256};
        if expected == self.launch_identity_fingerprint() {
            return true;
        }
        let canonical_default = self.driver.to_provider_kind().is_some_and(|kind| {
            self.instance_id.as_str()
                == crate::providers::settings::default_instance_id_for_kind(kind)
        }) && [
            self.binary_path.as_deref(),
            self.home_path.as_deref(),
            self.shadow_home_path.as_deref(),
            self.api_endpoint.as_deref(),
        ]
        .into_iter()
        .all(|value| value.is_none_or(str::is_empty))
            && self.environment.is_empty()
            && self.launch_args.is_empty();
        canonical_default
            && expected
                == format!(
                    "{:x}",
                    Sha256::digest(
                        format!("{}|{}||||", self.instance_id, self.driver.as_str()).as_bytes()
                    )
                )
    }

    /// Non-secret fingerprint of launch identity for binding/resume correlation.
    pub fn launch_identity_fingerprint(&self) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        // Configuration identity, not authentication evidence. Length framing
        // prevents user-entered delimiters from aliasing adjacent fields.
        fn field(hasher: &mut Sha256, value: &str) {
            hasher.update((value.len() as u64).to_be_bytes());
            hasher.update(value.as_bytes());
        }
        field(&mut hasher, "devmanager-provider-config-v2");
        field(&mut hasher, self.instance_id.as_str());
        field(&mut hasher, self.driver.as_str());
        field(&mut hasher, self.binary_path.as_deref().unwrap_or(""));
        field(&mut hasher, self.home_path.as_deref().unwrap_or(""));
        field(&mut hasher, self.shadow_home_path.as_deref().unwrap_or(""));
        field(&mut hasher, self.api_endpoint.as_deref().unwrap_or(""));
        hasher.update((self.launch_args.len() as u64).to_be_bytes());
        for argument in &self.launch_args {
            field(&mut hasher, argument);
        }
        hasher.update((self.environment.len() as u64).to_be_bytes());
        for env in &self.environment {
            field(&mut hasher, &env.name);
            hasher.update([u8::from(env.sensitive)]);
            if env.sensitive {
                field(&mut hasher, env.protected_value.as_deref().unwrap_or(""));
            } else {
                field(&mut hasher, env.value.as_deref().unwrap_or(""));
            }
        }
        format!("{:x}", hasher.finalize())
    }
}

pub fn normalize_model_slug(slug: &str) -> Result<String, ProviderSettingsError> {
    validate_model_slug(slug)?;
    Ok(slug.trim().to_string())
}

pub fn builtin_slugs_for_driver(driver: ProviderDriverKind) -> Vec<String> {
    match driver {
        ProviderDriverKind::Claude => vec!["opus".into(), "sonnet".into(), "haiku".into()],
        ProviderDriverKind::Codex => vec![
            "gpt-5.6-sol".into(),
            "gpt-5.6-terra".into(),
            "gpt-5.6-luna".into(),
        ],
        ProviderDriverKind::Cursor | ProviderDriverKind::Grok | ProviderDriverKind::OpenCode => {
            Vec::new()
        }
    }
}

fn reject_reserved_env_key(name: &str) -> Result<(), ProviderSettingsError> {
    const RESERVED_EXACT: &[&str] = &[
        "CLAUDE_CONFIG_DIR",
        "CODEX_HOME",
        "CURSOR_API_ENDPOINT",
        "DEVMANAGER_PROVIDER_SESSION_ID",
        "DEVMANAGER_PROVIDER_RELAY",
        "DEVMANAGER_HOOK_NONCE",
        "DEVMANAGER_HOOK_RELAY",
    ];
    const RESERVED_PREFIXES: &[&str] = &[
        "DEVMANAGER_HOOK_",
        "DEVMANAGER_RELAY_",
        "DEVMANAGER_PROVIDER_",
        "DEVMANAGER_SESSION_",
        "CLAUDE_CODE_",
    ];
    let upper = name.to_ascii_uppercase();
    if RESERVED_EXACT
        .iter()
        .any(|key| key.eq_ignore_ascii_case(&upper))
    {
        return Err(ProviderSettingsError::ReservedEnvKey(name.to_string()));
    }
    if RESERVED_PREFIXES
        .iter()
        .any(|prefix| upper.starts_with(prefix))
    {
        return Err(ProviderSettingsError::ReservedEnvKey(name.to_string()));
    }
    Ok(())
}

fn reject_reserved_launch_arg(
    driver: ProviderDriverKind,
    arg: &str,
) -> Result<(), ProviderSettingsError> {
    let trimmed = arg.trim();
    if trimmed.is_empty() {
        return Err(ProviderSettingsError::ReservedLaunchArg(arg.to_string()));
    }
    let lower = trimmed.to_ascii_lowercase();
    // Positional subcommands / raw prompts that change protocol identity.
    const POSITIONAL: &[&str] = &[
        "resume",
        "exec",
        "login",
        "logout",
        "auth",
        "mcp",
        "app-server",
        "sandbox",
        "about",
        "status",
        "chat",
    ];
    if !lower.starts_with('-') && POSITIONAL.iter().any(|cmd| lower == *cmd) {
        return Err(ProviderSettingsError::ReservedLaunchArg(arg.to_string()));
    }
    // Raw positional prompt tokens (no leading dash) are driver-owned.
    if !lower.starts_with('-') {
        return Err(ProviderSettingsError::ReservedLaunchArg(arg.to_string()));
    }
    let flag = lower.split_once('=').map(|(k, _)| k).unwrap_or(&lower);
    // Attached short flags: -cfoo / -pfoo / -Cvalue
    let short_attached = if flag.len() > 2 && flag.starts_with('-') && !flag.starts_with("--") {
        Some(&flag[..2])
    } else {
        None
    };
    let mut reserved: Vec<&str> = vec![
        "--resume",
        "-r",
        "--session-id",
        "--session",
        "--cwd",
        "-c",
        "--cd",
        "--permission-mode",
        "--dangerously-skip-permissions",
        "--dangerously-bypass-approvals-and-sandbox",
        "--print",
        "-p",
        "--output-format",
        "--input-format",
        "--format",
        "--hook",
        "--hooks",
        "--model",
        "-m",
        "--settings",
        "--add-dir",
        "--allowedtools",
        "--disallowedtools",
    ];
    match driver {
        ProviderDriverKind::Codex => {
            reserved.extend([
                "-c",
                "--config",
                "-s",
                "--sandbox",
                "-a",
                "--ask-for-approval",
                "--full-auto",
                "--profile",
            ]);
        }
        ProviderDriverKind::Claude => {
            reserved.extend([
                "--append-system-prompt",
                "--system-prompt",
                "--ide",
                "-c", // Claude also uses -C/--cwd; lowercased above
            ]);
        }
        ProviderDriverKind::Cursor => {
            reserved.extend(["--api-key", "--auth"]);
        }
        ProviderDriverKind::Grok | ProviderDriverKind::OpenCode => {}
    }
    if reserved.iter().any(|item| flag == *item) {
        return Err(ProviderSettingsError::ReservedLaunchArg(arg.to_string()));
    }
    if let Some(short) = short_attached {
        if reserved.iter().any(|item| *item == short) {
            return Err(ProviderSettingsError::ReservedLaunchArg(arg.to_string()));
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderSettingsDocument {
    pub revision: u64,
    /// Scheduled health probe interval in seconds. `0` = manual only.
    pub health_interval_secs: u64,
    pub instances: Vec<ProviderInstanceConfig>,
    /// Unknown top-level fields preserved across round-trips.
    #[serde(default, flatten)]
    pub unknown: BTreeMap<String, JsonValue>,
}

impl PartialEq for ProviderSettingsDocument {
    fn eq(&self, other: &Self) -> bool {
        self.revision == other.revision
            && self.health_interval_secs == other.health_interval_secs
            && self.instances == other.instances
            && self.unknown == other.unknown
    }
}

impl Eq for ProviderSettingsDocument {}

impl Default for ProviderSettingsDocument {
    fn default() -> Self {
        Self::with_builtins()
    }
}

impl ProviderSettingsDocument {
    pub fn with_builtins() -> Self {
        Self {
            revision: 1,
            health_interval_secs: DEFAULT_HEALTH_INTERVAL_SECS,
            instances: vec![
                ProviderInstanceConfig::builtin_default(BuiltinProviderDriver::Claude),
                ProviderInstanceConfig::builtin_default(BuiltinProviderDriver::Codex),
                ProviderInstanceConfig::builtin_default(BuiltinProviderDriver::Cursor),
                ProviderInstanceConfig::stub_catalog(StubProviderDriver::Grok),
                ProviderInstanceConfig::stub_catalog(StubProviderDriver::OpenCode),
            ],
            unknown: BTreeMap::new(),
        }
    }

    pub fn validate(&self) -> Result<(), ProviderSettingsError> {
        let mut ids = std::collections::BTreeSet::new();
        for instance in &self.instances {
            instance.validate()?;
            if !ids.insert(instance.instance_id.as_str().to_string()) {
                return Err(ProviderSettingsError::Corrupt(format!(
                    "duplicate instance id {}",
                    instance.instance_id
                )));
            }
        }
        // Reserved catalog ids must exist with the exact driver mapping.
        for (id, driver) in [
            (CLAUDE_DEFAULT_INSTANCE_ID, ProviderDriverKind::Claude),
            (CODEX_DEFAULT_INSTANCE_ID, ProviderDriverKind::Codex),
            (CURSOR_DEFAULT_INSTANCE_ID, ProviderDriverKind::Cursor),
            ("grok", ProviderDriverKind::Grok),
            ("opencode", ProviderDriverKind::OpenCode),
        ] {
            let Some(instance) = self.get(id) else {
                return Err(ProviderSettingsError::Corrupt(format!(
                    "missing reserved catalog instance `{id}`"
                )));
            };
            if instance.driver != driver {
                return Err(ProviderSettingsError::ImmutableBuiltinDriver);
            }
        }
        // Grok/OpenCode stay disabled stubs across load and ReplaceDocument.
        for id in ["grok", "opencode"] {
            let instance = self.get(id).expect("validated present");
            if !instance.driver.is_stub() || instance.enabled {
                return Err(ProviderSettingsError::StubCannotEnable(
                    instance.driver.as_str().to_string(),
                ));
            }
        }
        Ok(())
    }

    pub fn get(&self, id: &str) -> Option<&ProviderInstanceConfig> {
        self.instances
            .iter()
            .find(|instance| instance.instance_id.as_str() == id)
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut ProviderInstanceConfig> {
        self.instances
            .iter_mut()
            .find(|instance| instance.instance_id.as_str() == id)
    }

    pub fn require_enabled_launchable(
        &self,
        id: &str,
    ) -> Result<&ProviderInstanceConfig, ProviderSettingsError> {
        let instance = self
            .get(id)
            .ok_or_else(|| ProviderSettingsError::UnknownInstance(id.to_string()))?;
        if instance.driver.is_stub() {
            return Err(ProviderSettingsError::StubCannotEnable(
                instance.driver.as_str().to_string(),
            ));
        }
        if !instance.enabled {
            return Err(ProviderSettingsError::InstanceDisabled(id.to_string()));
        }
        Ok(instance)
    }

    pub fn redacted_projection(&self) -> Self {
        Self {
            revision: self.revision,
            health_interval_secs: self.health_interval_secs,
            instances: self
                .instances
                .iter()
                .map(ProviderInstanceConfig::redacted_projection)
                .collect(),
            unknown: self.unknown.clone(),
        }
    }

    /// Update an existing instance in place. Rejects unknown ids.
    pub fn update_instance(
        &mut self,
        instance: ProviderInstanceConfig,
    ) -> Result<(), ProviderSettingsError> {
        instance.validate()?;
        let Some(existing) = self.get_mut(instance.instance_id.as_str()) else {
            return Err(ProviderSettingsError::UnknownInstance(
                instance.instance_id.to_string(),
            ));
        };
        if is_immutable_catalog_id(existing.instance_id.as_str())
            && existing.driver != instance.driver
        {
            return Err(ProviderSettingsError::ImmutableBuiltinDriver);
        }
        *existing = instance;
        self.revision = self.revision.saturating_add(1);
        Ok(())
    }

    /// Add a new custom instance. Rejects duplicate ids (never overwrites).
    pub fn add_instance(
        &mut self,
        instance: ProviderInstanceConfig,
    ) -> Result<(), ProviderSettingsError> {
        instance.validate()?;
        if self.get(instance.instance_id.as_str()).is_some() {
            return Err(ProviderSettingsError::DuplicateInstanceId(
                instance.instance_id.to_string(),
            ));
        }
        if is_immutable_catalog_id(instance.instance_id.as_str()) {
            return Err(ProviderSettingsError::DuplicateInstanceId(
                instance.instance_id.to_string(),
            ));
        }
        self.instances.push(instance);
        self.revision = self.revision.saturating_add(1);
        Ok(())
    }

    /// Compatibility helper: update if present, otherwise add.
    pub fn upsert_instance(
        &mut self,
        instance: ProviderInstanceConfig,
    ) -> Result<(), ProviderSettingsError> {
        if self.get(instance.instance_id.as_str()).is_some() {
            self.update_instance(instance)
        } else {
            self.add_instance(instance)
        }
    }

    pub fn remove_custom_instance(
        &mut self,
        id: &str,
    ) -> Result<ProviderInstanceConfig, ProviderSettingsError> {
        let idx = self
            .instances
            .iter()
            .position(|instance| instance.instance_id.as_str() == id)
            .ok_or_else(|| ProviderSettingsError::UnknownInstance(id.to_string()))?;
        let removed = self.instances.remove(idx);
        if is_immutable_catalog_id(removed.instance_id.as_str()) || removed.driver.is_stub() {
            self.instances.insert(idx, removed);
            return Err(ProviderSettingsError::Corrupt(
                "cannot delete builtin or stub catalog entry; reset instead".into(),
            ));
        }
        self.revision = self.revision.saturating_add(1);
        Ok(removed)
    }

    pub fn reset_builtin(&mut self, id: &str) -> Result<(), ProviderSettingsError> {
        let driver = match id {
            CLAUDE_DEFAULT_INSTANCE_ID => BuiltinProviderDriver::Claude,
            CODEX_DEFAULT_INSTANCE_ID => BuiltinProviderDriver::Codex,
            CURSOR_DEFAULT_INSTANCE_ID => BuiltinProviderDriver::Cursor,
            "grok" => {
                if let Some(slot) = self.get_mut(id) {
                    *slot = ProviderInstanceConfig::stub_catalog(StubProviderDriver::Grok);
                    self.revision = self.revision.saturating_add(1);
                    return Ok(());
                }
                return Err(ProviderSettingsError::UnknownInstance(id.to_string()));
            }
            "opencode" => {
                if let Some(slot) = self.get_mut(id) {
                    *slot = ProviderInstanceConfig::stub_catalog(StubProviderDriver::OpenCode);
                    self.revision = self.revision.saturating_add(1);
                    return Ok(());
                }
                return Err(ProviderSettingsError::UnknownInstance(id.to_string()));
            }
            _ => {
                return Err(ProviderSettingsError::Corrupt(
                    "reset applies only to builtin or stub catalog ids".into(),
                ));
            }
        };
        if let Some(slot) = self.get_mut(id) {
            *slot = ProviderInstanceConfig::builtin_default(driver);
            self.revision = self.revision.saturating_add(1);
            Ok(())
        } else {
            Err(ProviderSettingsError::UnknownInstance(id.to_string()))
        }
    }

    pub fn set_health_interval(&mut self, secs: u64) {
        self.health_interval_secs = secs;
        self.revision = self.revision.saturating_add(1);
    }

    /// Picker order: favorites that exist in the visible catalog, then remaining
    /// visible builtins/customs. Hidden builtins and unknown favorites are excluded.
    pub fn ordered_picker_models(
        &self,
        instance_id: &str,
        builtin_slugs: &[String],
    ) -> Result<Vec<String>, ProviderSettingsError> {
        let instance = self
            .get(instance_id)
            .ok_or_else(|| ProviderSettingsError::UnknownInstance(instance_id.to_string()))?;
        let hidden: std::collections::BTreeSet<&str> = instance
            .model_policy
            .hidden_builtins
            .iter()
            .map(String::as_str)
            .collect();
        let mut available = std::collections::BTreeSet::new();
        for slug in builtin_slugs {
            if !hidden.contains(slug.as_str()) {
                available.insert(slug.clone());
            }
        }
        for custom in &instance.custom_models {
            available.insert(custom.slug.clone());
        }
        let mut ordered = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        for slug in &instance.model_policy.favorite_order {
            if available.contains(slug) && seen.insert(slug.clone()) {
                ordered.push(slug.clone());
            }
        }
        for slug in available {
            if seen.insert(slug.clone()) {
                ordered.push(slug);
            }
        }
        Ok(ordered)
    }

    /// Full catalog for show/hide UI (includes hidden builtins).
    pub fn full_model_catalog(
        &self,
        instance_id: &str,
        builtin_slugs: &[String],
    ) -> Result<Vec<String>, ProviderSettingsError> {
        let instance = self
            .get(instance_id)
            .ok_or_else(|| ProviderSettingsError::UnknownInstance(instance_id.to_string()))?;
        let mut catalog = builtin_slugs.to_vec();
        for custom in &instance.custom_models {
            if !catalog.iter().any(|s| s == &custom.slug) {
                catalog.push(custom.slug.clone());
            }
        }
        Ok(catalog)
    }

    pub fn add_custom_model(
        &mut self,
        instance_id: &str,
        slug: &str,
        display_name: Option<String>,
    ) -> Result<(), ProviderSettingsError> {
        validate_model_slug(slug)?;
        let instance = self
            .get_mut(instance_id)
            .ok_or_else(|| ProviderSettingsError::UnknownInstance(instance_id.to_string()))?;
        if instance.custom_models.iter().any(|m| m.slug == slug) {
            return Err(ProviderSettingsError::DuplicateCustomModel(
                slug.to_string(),
            ));
        }
        if instance.custom_models.len() >= MAX_CUSTOM_MODELS {
            return Err(ProviderSettingsError::TooManyCustomModels);
        }
        instance.custom_models.push(CustomModelEntry {
            slug: slug.trim().to_string(),
            display_name,
        });
        self.revision = self.revision.saturating_add(1);
        Ok(())
    }

    pub fn remove_custom_model(
        &mut self,
        instance_id: &str,
        slug: &str,
    ) -> Result<(), ProviderSettingsError> {
        let instance = self
            .get_mut(instance_id)
            .ok_or_else(|| ProviderSettingsError::UnknownInstance(instance_id.to_string()))?;
        let before = instance.custom_models.len();
        instance.custom_models.retain(|m| m.slug != slug);
        if instance.custom_models.len() == before {
            return Err(ProviderSettingsError::InvalidModelSlug(slug.to_string()));
        }
        instance.model_policy.favorite_order.retain(|s| s != slug);
        self.revision = self.revision.saturating_add(1);
        Ok(())
    }

    pub fn set_builtin_hidden(
        &mut self,
        instance_id: &str,
        slug: &str,
        hidden: bool,
    ) -> Result<(), ProviderSettingsError> {
        let instance = self
            .get_mut(instance_id)
            .ok_or_else(|| ProviderSettingsError::UnknownInstance(instance_id.to_string()))?;
        let list = &mut instance.model_policy.hidden_builtins;
        if hidden {
            if !list.iter().any(|s| s == slug) {
                list.push(slug.to_string());
            }
        } else {
            list.retain(|s| s != slug);
        }
        self.revision = self.revision.saturating_add(1);
        Ok(())
    }

    pub fn set_favorite(
        &mut self,
        instance_id: &str,
        slug: &str,
        favorite: bool,
    ) -> Result<(), ProviderSettingsError> {
        let instance = self
            .get_mut(instance_id)
            .ok_or_else(|| ProviderSettingsError::UnknownInstance(instance_id.to_string()))?;
        let order = &mut instance.model_policy.favorite_order;
        if favorite {
            if !order.iter().any(|s| s == slug) {
                order.push(slug.to_string());
            }
        } else {
            order.retain(|s| s != slug);
        }
        self.revision = self.revision.saturating_add(1);
        Ok(())
    }

    pub fn move_favorite(
        &mut self,
        instance_id: &str,
        slug: &str,
        up: bool,
    ) -> Result<(), ProviderSettingsError> {
        let instance = self
            .get_mut(instance_id)
            .ok_or_else(|| ProviderSettingsError::UnknownInstance(instance_id.to_string()))?;
        let order = &mut instance.model_policy.favorite_order;
        let Some(idx) = order.iter().position(|s| s == slug) else {
            return Err(ProviderSettingsError::InvalidModelSlug(slug.to_string()));
        };
        if up {
            if idx == 0 {
                return Ok(());
            }
            order.swap(idx, idx - 1);
        } else if idx + 1 < order.len() {
            order.swap(idx, idx + 1);
        }
        self.revision = self.revision.saturating_add(1);
        Ok(())
    }

    /// Full settings catalog order: explicit `catalog_order`, then remaining
    /// builtins/customs (including hidden). Used by the settings Show/Hide UI.
    pub fn ordered_settings_catalog(
        &self,
        instance_id: &str,
        builtin_slugs: &[String],
    ) -> Result<Vec<String>, ProviderSettingsError> {
        let instance = self
            .get(instance_id)
            .ok_or_else(|| ProviderSettingsError::UnknownInstance(instance_id.to_string()))?;
        let full = self.full_model_catalog(instance_id, builtin_slugs)?;
        let available: std::collections::BTreeSet<&str> = full.iter().map(String::as_str).collect();
        let mut ordered = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        for slug in &instance.model_policy.catalog_order {
            if available.contains(slug.as_str()) && seen.insert(slug.clone()) {
                ordered.push(slug.clone());
            }
        }
        for slug in full {
            if seen.insert(slug.clone()) {
                ordered.push(slug);
            }
        }
        Ok(ordered)
    }

    pub fn move_catalog_model(
        &mut self,
        instance_id: &str,
        slug: &str,
        up: bool,
        builtin_slugs: &[String],
    ) -> Result<(), ProviderSettingsError> {
        let ordered = self.ordered_settings_catalog(instance_id, builtin_slugs)?;
        let Some(idx) = ordered.iter().position(|s| s == slug) else {
            return Err(ProviderSettingsError::InvalidModelSlug(slug.to_string()));
        };
        let mut next = ordered;
        if up {
            if idx == 0 {
                return Ok(());
            }
            next.swap(idx, idx - 1);
        } else if idx + 1 < next.len() {
            next.swap(idx, idx + 1);
        }
        let instance = self
            .get_mut(instance_id)
            .ok_or_else(|| ProviderSettingsError::UnknownInstance(instance_id.to_string()))?;
        instance.model_policy.catalog_order = next;
        self.revision = self.revision.saturating_add(1);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_include_supported_and_stub_catalog() {
        let doc = ProviderSettingsDocument::with_builtins();
        assert_eq!(doc.health_interval_secs, 300);
        assert!(doc.get("claude").unwrap().enabled);
        assert!(doc.get("codex").unwrap().enabled);
        assert!(doc.get("cursor").unwrap().enabled);
        assert!(!doc.get("grok").unwrap().enabled);
        assert!(doc.get("grok").unwrap().driver.is_stub());
        assert!(!doc.get("opencode").unwrap().enabled);
        assert!(doc.validate().is_ok());
    }

    #[test]
    fn invalid_instance_ids_rejected() {
        assert!(validate_instance_id("").is_err());
        assert!(validate_instance_id("1bad").is_err());
        assert!(validate_instance_id("has space").is_err());
        assert!(validate_instance_id(&"a".repeat(65)).is_err());
        assert!(validate_instance_id("Claude_Code-1").is_ok());
    }

    #[test]
    fn stub_cannot_enable() {
        let mut doc = ProviderSettingsDocument::with_builtins();
        let mut grok = doc.get("grok").unwrap().clone();
        grok.enabled = true;
        assert!(matches!(
            grok.validate(),
            Err(ProviderSettingsError::StubCannotEnable(_))
        ));
        assert!(matches!(
            doc.require_enabled_launchable("grok"),
            Err(ProviderSettingsError::StubCannotEnable(_))
        ));
    }

    #[test]
    fn document_replacement_cannot_rebrand_stub_as_a_launchable_driver() {
        for id in ["grok", "opencode"] {
            let mut doc = ProviderSettingsDocument::with_builtins();
            let instance = doc.get_mut(id).unwrap();
            instance.driver = ProviderDriverKind::Claude;
            instance.enabled = true;
            assert_eq!(
                doc.validate(),
                Err(ProviderSettingsError::ImmutableBuiltinDriver)
            );
        }
    }

    #[test]
    fn unknown_fields_preserved_on_roundtrip() {
        let mut doc = ProviderSettingsDocument::with_builtins();
        doc.unknown
            .insert("futureTop".into(), JsonValue::String("keep".into()));
        let mut claude = doc.get("claude").unwrap().clone();
        claude
            .unknown
            .insert("futureDriverField".into(), JsonValue::Bool(true));
        doc.upsert_instance(claude).unwrap();
        let json = serde_json::to_string(&doc).unwrap();
        let back: ProviderSettingsDocument = serde_json::from_str(&json).unwrap();
        assert_eq!(
            back.unknown.get("futureTop").and_then(|v| v.as_str()),
            Some("keep")
        );
        assert_eq!(
            back.get("claude")
                .unwrap()
                .unknown
                .get("futureDriverField")
                .and_then(|v| v.as_bool()),
            Some(true)
        );
    }

    #[test]
    fn model_ordering_hide_favorite_custom() {
        let mut doc = ProviderSettingsDocument::with_builtins();
        doc.add_custom_model("claude", "my/custom", None).unwrap();
        doc.set_favorite("claude", "my/custom", true).unwrap();
        doc.set_favorite("claude", "opus", true).unwrap();
        doc.set_favorite("claude", "unknown-favorite", true)
            .unwrap();
        doc.move_favorite("claude", "opus", true).unwrap();
        doc.set_builtin_hidden("claude", "haiku", true).unwrap();
        let builtins = vec!["opus".into(), "sonnet".into(), "haiku".into()];
        let ordered = doc.ordered_picker_models("claude", &builtins).unwrap();
        assert_eq!(ordered[0], "opus");
        assert_eq!(ordered[1], "my/custom");
        assert!(ordered.contains(&"sonnet".to_string()));
        assert!(!ordered.contains(&"haiku".to_string()));
        assert!(!ordered.contains(&"unknown-favorite".to_string()));
    }

    #[test]
    fn env_var_eq_is_reflexive_for_sensitive() {
        let env = ProviderEnvVar {
            name: "TOKEN".into(),
            value: Some("x".into()),
            sensitive: true,
            protected_value: None,
            value_redacted: false,
        };
        assert_eq!(env, env);
    }

    #[test]
    fn add_instance_rejects_duplicate_id() {
        let mut doc = ProviderSettingsDocument::with_builtins();
        let err = doc.add_instance(ProviderInstanceConfig::builtin_default(
            BuiltinProviderDriver::Claude,
        ));
        assert!(matches!(
            err,
            Err(ProviderSettingsError::DuplicateInstanceId(_))
        ));
    }

    #[test]
    fn reserved_launch_args_rejected() {
        let mut inst = ProviderInstanceConfig::builtin_default(BuiltinProviderDriver::Codex);
        inst.launch_args = vec!["--resume".into()];
        assert!(matches!(
            inst.validate(),
            Err(ProviderSettingsError::ReservedLaunchArg(_))
        ));
    }

    #[test]
    fn redaction_strips_sensitive_values() {
        let mut inst = ProviderInstanceConfig::builtin_default(BuiltinProviderDriver::Claude);
        inst.environment.push(ProviderEnvVar {
            name: "TOKEN".into(),
            value: Some("secret".into()),
            sensitive: true,
            protected_value: Some("blob".into()),
            value_redacted: true,
        });
        let redacted = inst.redacted_projection();
        assert!(redacted.environment[0].value.is_none());
        assert!(redacted.environment[0].protected_value.is_none());
        assert!(redacted.environment[0].value_redacted);
    }
}
