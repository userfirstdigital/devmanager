//! Apply one immutable per-instance probe/launch context BEFORE seal.
//!
//! `ProviderDiscoveryConfig.path` is executable-search PATH only. Home dirs go
//! into `CLAUDE_CONFIG_DIR` / `CODEX_HOME` in the scoped child environment.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::PathBuf;

use sha2::{Digest, Sha256};

use crate::providers::registry::ProviderDiscoveryConfig;
use crate::providers::settings::model::{
    builtin_slugs_for_driver, normalize_model_slug, ProviderDriverKind, ProviderInstanceConfig,
    ProviderSettingsError,
};
use crate::providers::settings::secret::{reveal_secret_value, SecretCustodyError};
use crate::providers::ProviderKind;

#[derive(Clone, PartialEq, Eq)]
pub enum LaunchPolicyError {
    Settings(ProviderSettingsError),
    Secret(SecretCustodyError),
    CannotLaunch(String),
    ShadowHomeConflict(String),
}

impl fmt::Display for LaunchPolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Settings(error) => write!(f, "{error}"),
            Self::Secret(error) => write!(f, "{error}"),
            Self::CannotLaunch(id) => write!(f, "provider instance `{id}` cannot be launched"),
            Self::ShadowHomeConflict(msg) => write!(f, "codex shadow home conflict: {msg}"),
        }
    }
}

impl fmt::Debug for LaunchPolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl std::error::Error for LaunchPolicyError {}

impl From<ProviderSettingsError> for LaunchPolicyError {
    fn from(value: ProviderSettingsError) -> Self {
        Self::Settings(value)
    }
}

impl From<SecretCustodyError> for LaunchPolicyError {
    fn from(value: SecretCustodyError) -> Self {
        Self::Secret(value)
    }
}

/// Opaque non-secret scope identity bound through probe receipts and launch.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct ProviderInstanceScope {
    pub instance_id: String,
    /// SHA-256 hex of non-secret launch identity (home/binary/env names/blobs).
    pub fingerprint: String,
}

impl fmt::Debug for ProviderInstanceScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderInstanceScope")
            .field("instance_id", &self.instance_id)
            .field("fingerprint", &self.fingerprint)
            .finish()
    }
}

impl ProviderInstanceScope {
    pub fn from_instance(instance: &ProviderInstanceConfig) -> Self {
        Self {
            instance_id: instance.instance_id.to_string(),
            fingerprint: instance.launch_identity_fingerprint(),
        }
    }

    pub fn as_cache_key(&self) -> String {
        format!("{}:{}", self.instance_id, self.fingerprint)
    }
}

/// One resolved context used for discovery, probes, and physical launch.
#[derive(Clone)]
pub struct ResolvedProviderLaunchConfig {
    pub scope: ProviderInstanceScope,
    pub provider_kind: ProviderKind,
    pub discovery: ProviderDiscoveryConfig,
    pub environment: BTreeMap<OsString, OsString>,
    pub sensitive_env_keys: Vec<OsString>,
    pub extra_launch_args: Vec<String>,
    pub selected_model: Option<String>,
    pub home_path: Option<PathBuf>,
    pub shadow_home_path: Option<PathBuf>,
    pub api_endpoint: Option<String>,
}

impl fmt::Debug for ResolvedProviderLaunchConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResolvedProviderLaunchConfig")
            .field("scope", &self.scope)
            .field("provider_kind", &self.provider_kind)
            .field("discovery", &self.discovery)
            .field(
                "environment",
                &redacted_env_debug(&self.environment, &self.sensitive_env_keys),
            )
            .field("sensitive_env_key_count", &self.sensitive_env_keys.len())
            .field("extra_launch_args", &self.extra_launch_args)
            .field("selected_model", &self.selected_model)
            .field("home_path", &self.home_path)
            .field("shadow_home_path", &self.shadow_home_path)
            .field("api_endpoint", &self.api_endpoint)
            .finish()
    }
}

fn redacted_env_debug(
    env: &BTreeMap<OsString, OsString>,
    sensitive: &[OsString],
) -> BTreeMap<String, String> {
    let sensitive: std::collections::BTreeSet<&OsStr> =
        sensitive.iter().map(OsString::as_os_str).collect();
    env.iter()
        .map(|(k, v)| {
            let key = k.to_string_lossy().into_owned();
            let value = if sensitive.iter().any(|candidate| {
                if cfg!(windows) {
                    candidate.to_string_lossy().eq_ignore_ascii_case(&key)
                } else {
                    *candidate == k.as_os_str()
                }
            }) {
                "<redacted>".into()
            } else {
                v.to_string_lossy().into_owned()
            };
            (key, value)
        })
        .collect()
}

/// Discovery uses executable override only. PATH stays the process search path
/// (or an explicit executable-search override unrelated to home).
pub fn apply_instance_to_discovery(
    instance: &ProviderInstanceConfig,
) -> Result<ProviderDiscoveryConfig, LaunchPolicyError> {
    if instance.driver.is_stub() {
        return Err(LaunchPolicyError::CannotLaunch(
            instance.instance_id.to_string(),
        ));
    }
    if !instance.enabled {
        return Err(LaunchPolicyError::Settings(
            ProviderSettingsError::InstanceDisabled(instance.instance_id.to_string()),
        ));
    }
    Ok(ProviderDiscoveryConfig {
        executable_override: instance
            .binary_path
            .as_ref()
            .filter(|p| !p.is_empty())
            .map(PathBuf::from),
        // Never put home_path here — that field is executable-search PATH.
        path: None,
        child_environment: BTreeMap::new(),
        instance_scope: None,
    })
}

pub fn merge_instance_environment(
    instance: &ProviderInstanceConfig,
    custody_scope: &[u8],
    base: BTreeMap<OsString, OsString>,
) -> Result<(BTreeMap<OsString, OsString>, Vec<OsString>), LaunchPolicyError> {
    let mut env = base;
    let mut sensitive = Vec::new();
    match instance.driver {
        ProviderDriverKind::Claude => {
            if let Some(home) = instance.home_path.as_ref().filter(|p| !p.is_empty()) {
                env.insert(
                    OsString::from("CLAUDE_CONFIG_DIR"),
                    OsString::from(home.as_str()),
                );
            }
        }
        ProviderDriverKind::Codex => {
            if let Some(home) = instance.home_path.as_ref().filter(|p| !p.is_empty()) {
                env.insert(OsString::from("CODEX_HOME"), OsString::from(home.as_str()));
            }
        }
        ProviderDriverKind::Cursor => {
            if let Some(endpoint) = instance.api_endpoint.as_ref().filter(|p| !p.is_empty()) {
                env.insert(
                    OsString::from("CURSOR_API_ENDPOINT"),
                    OsString::from(endpoint.as_str()),
                );
            }
        }
        ProviderDriverKind::Grok | ProviderDriverKind::OpenCode => {
            return Err(LaunchPolicyError::CannotLaunch(
                instance.instance_id.to_string(),
            ));
        }
    }
    for var in &instance.environment {
        let key = OsString::from(&var.name);
        let value = if var.sensitive {
            sensitive.push(key.clone());
            match &var.protected_value {
                Some(blob) => OsString::from(reveal_secret_value(blob, custody_scope)?.as_str()),
                None => OsString::from(var.value.as_deref().unwrap_or("")),
            }
        } else {
            OsString::from(var.value.as_deref().unwrap_or(""))
        };
        env.insert(key, value);
    }
    Ok((env, sensitive))
}

/// Resolve the single immutable context for probe + launch.
pub fn resolve_launch_config(
    instance: &ProviderInstanceConfig,
    custody_scope: &[u8],
    selected_model: Option<String>,
) -> Result<ResolvedProviderLaunchConfig, LaunchPolicyError> {
    resolve_launch_config_with_known_models(instance, custody_scope, selected_model, &[])
}

/// Like [`resolve_launch_config`], but also accepts discovered catalog slugs
/// from the last-good metadata cache (aliases such as `claude-opus-5[1m]`).
pub fn resolve_launch_config_with_known_models(
    instance: &ProviderInstanceConfig,
    custody_scope: &[u8],
    selected_model: Option<String>,
    known_catalog_slugs: &[String],
) -> Result<ResolvedProviderLaunchConfig, LaunchPolicyError> {
    instance.validate()?;
    if instance.driver.is_stub() || !instance.enabled {
        return Err(LaunchPolicyError::CannotLaunch(
            instance.instance_id.to_string(),
        ));
    }
    let provider_kind = instance
        .driver
        .to_provider_kind()
        .ok_or_else(|| LaunchPolicyError::CannotLaunch(instance.instance_id.to_string()))?;
    let selected_model = match selected_model {
        Some(raw) => {
            let slug = normalize_model_slug(&raw)?;
            let builtins = builtin_slugs_for_driver(instance.driver);
            let customs: Vec<_> = instance
                .custom_models
                .iter()
                .map(|m| m.slug.as_str())
                .collect();
            let known = known_catalog_slugs.iter().any(|s| s == &slug)
                || builtins.iter().any(|b| b == &slug)
                || customs.iter().any(|c| *c == slug);
            if !known {
                return Err(LaunchPolicyError::Settings(
                    ProviderSettingsError::UnknownModel(slug),
                ));
            }
            // Hidden builtins remain selectable in management but composer
            // policy uses visibility separately; launch membership accepts known.
            Some(slug)
        }
        None => None,
    };
    let scope = ProviderInstanceScope::from_instance(instance);
    let mut discovery = apply_instance_to_discovery(instance)?;
    let (mut environment, sensitive_env_keys) =
        merge_instance_environment(instance, custody_scope, BTreeMap::new())?;
    // Shadow-only Codex: shared home is for link prep only. Effective CODEX_HOME
    // for probe/commitment/launch is the shadow path before discovery is sealed.
    let mut home_path = instance
        .home_path
        .as_ref()
        .filter(|p| !p.is_empty())
        .map(PathBuf::from);
    let shadow_home_path = instance
        .shadow_home_path
        .as_ref()
        .filter(|p| !p.is_empty())
        .map(PathBuf::from);
    if matches!(instance.driver, ProviderDriverKind::Codex)
        && shadow_home_path.is_some()
        && home_path.is_none()
    {
        home_path = Some(default_codex_shared_home());
    }
    if let Some(shadow) = shadow_home_path.as_ref() {
        if matches!(instance.driver, ProviderDriverKind::Codex) {
            environment.insert(
                OsString::from("CODEX_HOME"),
                OsString::from(shadow.as_os_str()),
            );
        }
    }
    let environment = crate::providers::adapter::materialize_provider_environment(environment);
    discovery.path = environment.get(&OsString::from("PATH")).cloned();
    discovery.child_environment = environment.clone();
    discovery.instance_scope = Some(scope.clone());
    Ok(ResolvedProviderLaunchConfig {
        scope,
        provider_kind,
        discovery,
        environment,
        sensitive_env_keys,
        extra_launch_args: instance.launch_args.clone(),
        selected_model,
        home_path,
        shadow_home_path,
        api_endpoint: instance.api_endpoint.clone(),
    })
}

fn default_codex_shared_home() -> PathBuf {
    if let Some(home) = std::env::var_os("CODEX_HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(home);
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".codex")
}

/// Prepare Codex shadow home: share state via links, keep auth.json and
/// models_cache private. Fail closed on existing conflicts.
pub fn prepare_codex_shadow_home(
    home: &std::path::Path,
    shadow: &std::path::Path,
) -> Result<(), LaunchPolicyError> {
    use std::fs;
    fs::create_dir_all(home).map_err(|e| {
        LaunchPolicyError::ShadowHomeConflict(format!("cannot create shared home: {e}"))
    })?;
    fs::create_dir_all(shadow).map_err(|e| {
        LaunchPolicyError::ShadowHomeConflict(format!("cannot create shadow home: {e}"))
    })?;
    let home =
        fs::canonicalize(home).map_err(|e| LaunchPolicyError::ShadowHomeConflict(e.to_string()))?;
    let shadow = fs::canonicalize(shadow)
        .map_err(|e| LaunchPolicyError::ShadowHomeConflict(e.to_string()))?;
    if home.starts_with(&shadow) || shadow.starts_with(&home) {
        return Err(LaunchPolicyError::ShadowHomeConflict(
            "shared and shadow homes must be distinct, non-nested directories".into(),
        ));
    }
    // Private files must never be linked or overwritten from home.
    for private in ["auth.json", "models_cache.json", "models_cache"] {
        let shadow_private = shadow.join(private);
        match fs::symlink_metadata(&shadow_private) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(LaunchPolicyError::ShadowHomeConflict(format!(
                    "private `{private}` must not be a link"
                )));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(LaunchPolicyError::ShadowHomeConflict(error.to_string())),
        }
    }
    // Ported from T3 CodexHomeLayout. These must exist before either CLI
    // creates private copies that would split continuation history.
    for name in [
        "sessions",
        "archived_sessions",
        "sqlite",
        "shell_snapshots",
        "worktrees",
        "skills",
        "plugins",
        "cache",
        "logs",
        "mcp-oauth-locks",
    ] {
        fs::create_dir_all(home.join(name))
            .map_err(|e| LaunchPolicyError::ShadowHomeConflict(e.to_string()))?;
    }
    // Link non-private entries from home when absent in shadow.
    let entries = fs::read_dir(&home).map_err(|e| {
        LaunchPolicyError::ShadowHomeConflict(format!("cannot read CODEX_HOME: {e}"))
    })?;
    let mut missing_links = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| {
            LaunchPolicyError::ShadowHomeConflict(format!("cannot read CODEX_HOME entry: {e}"))
        })?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if matches!(
            name_str.as_ref(),
            "auth.json" | "models_cache.json" | "models_cache" | "log" | "memories" | "tmp"
        ) {
            continue;
        }
        let dest = shadow.join(&name);
        let src = entry.path();
        match fs::symlink_metadata(&dest) {
            Ok(meta) if meta.file_type().is_symlink() => {
                let target = fs::canonicalize(&dest)
                    .map_err(|e| LaunchPolicyError::ShadowHomeConflict(e.to_string()))?;
                let expected = fs::canonicalize(&src)
                    .map_err(|e| LaunchPolicyError::ShadowHomeConflict(e.to_string()))?;
                if target != expected {
                    return Err(LaunchPolicyError::ShadowHomeConflict(format!(
                        "{} links to a different shared entry",
                        dest.display()
                    )));
                }
                continue;
            }
            Ok(_) => {
                return Err(LaunchPolicyError::ShadowHomeConflict(format!(
                    "{} already exists and is not a shared link",
                    dest.display()
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(LaunchPolicyError::ShadowHomeConflict(error.to_string())),
        }
        missing_links.push((src, dest));
    }
    for (src, dest) in missing_links {
        link_or_fail(&src, &dest)?;
    }
    Ok(())
}

fn link_or_fail(src: &std::path::Path, dest: &std::path::Path) -> Result<(), LaunchPolicyError> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::{symlink_dir, symlink_file};
        let meta = std::fs::symlink_metadata(src).map_err(|e| {
            LaunchPolicyError::ShadowHomeConflict(format!("cannot stat {}: {e}", src.display()))
        })?;
        let result = if meta.is_dir() {
            symlink_dir(src, dest)
        } else {
            symlink_file(src, dest)
        };
        result.map_err(|e| {
            LaunchPolicyError::ShadowHomeConflict(format!(
                "cannot link {} -> {}: {e}",
                src.display(),
                dest.display()
            ))
        })
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(src, dest).map_err(|e| {
            LaunchPolicyError::ShadowHomeConflict(format!(
                "cannot link {} -> {}: {e}",
                src.display(),
                dest.display()
            ))
        })
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = (src, dest);
        Err(LaunchPolicyError::ShadowHomeConflict(
            "shadow home linking unsupported on this platform".into(),
        ))
    }
}

pub fn scope_fingerprint_bytes(scope: &ProviderInstanceScope) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(scope.instance_id.as_bytes());
    hasher.update(b"|");
    hasher.update(scope.fingerprint.as_bytes());
    let digest = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::settings::model::BuiltinProviderDriver;
    use tempfile::tempdir;

    #[test]
    fn home_is_env_not_discovery_path() {
        let mut inst = ProviderInstanceConfig::builtin_default(BuiltinProviderDriver::Codex);
        inst.home_path = Some("C:/tmp/codex-home".into());
        inst.binary_path = Some("C:/tools/codex.exe".into());
        let discovery = apply_instance_to_discovery(&inst).unwrap();
        assert!(discovery.path.is_none());
        assert_eq!(
            discovery.executable_override.unwrap(),
            PathBuf::from("C:/tools/codex.exe")
        );
        let (env, _) = merge_instance_environment(&inst, b"scope", BTreeMap::new()).unwrap();
        assert_eq!(
            env.get(OsStr::new("CODEX_HOME"))
                .map(|v| v.to_string_lossy().into_owned()),
            Some("C:/tmp/codex-home".into())
        );
    }

    #[test]
    fn disabled_and_stub_refused() {
        let mut inst = ProviderInstanceConfig::builtin_default(BuiltinProviderDriver::Claude);
        inst.enabled = false;
        assert!(apply_instance_to_discovery(&inst).is_err());
        let stub = ProviderInstanceConfig::stub_catalog(
            crate::providers::settings::model::StubProviderDriver::Grok,
        );
        assert!(apply_instance_to_discovery(&stub).is_err());
    }

    #[test]
    fn resolve_sets_same_env_on_discovery_and_launch() {
        let mut inst = ProviderInstanceConfig::builtin_default(BuiltinProviderDriver::Claude);
        inst.environment
            .push(crate::providers::settings::model::ProviderEnvVar {
                name: "MY_FLAG".into(),
                value: Some("1".into()),
                sensitive: false,
                protected_value: None,
                value_redacted: false,
            });
        let resolved = resolve_launch_config(&inst, b"scope", Some("opus".into())).unwrap();
        assert_eq!(resolved.discovery.child_environment, resolved.environment);
        assert_eq!(
            resolved.scope.instance_id,
            resolved
                .discovery
                .instance_scope
                .as_ref()
                .unwrap()
                .instance_id
        );
        assert_eq!(resolved.selected_model.as_deref(), Some("opus"));
    }

    #[test]
    fn shadow_home_fails_closed_on_existing_unshared_directory() {
        let dir = tempdir().unwrap();
        let home = dir.path().join("home");
        let shadow = dir.path().join("shadow");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&shadow).unwrap();
        std::fs::create_dir_all(shadow.join("sessions")).unwrap();
        let err = prepare_codex_shadow_home(&home, &shadow).unwrap_err();
        assert!(matches!(err, LaunchPolicyError::ShadowHomeConflict(_)));
        assert!(err.to_string().contains("not a shared link"));
    }

    #[test]
    fn resolved_debug_redacts_sensitive_env() {
        let mut inst = ProviderInstanceConfig::builtin_default(BuiltinProviderDriver::Claude);
        inst.environment
            .push(crate::providers::settings::model::ProviderEnvVar {
                name: "TOKEN".into(),
                value: Some("super-secret".into()),
                sensitive: true,
                protected_value: None,
                value_redacted: false,
            });
        // Sensitive without protected blob still resolves plaintext for launch
        // in-memory; Debug must not print it.
        let resolved = resolve_launch_config(&inst, b"scope", None).unwrap();
        let rendered = format!("{resolved:?}");
        assert!(!rendered.contains("super-secret"));
        assert!(rendered.contains("<redacted>"));
    }
}
