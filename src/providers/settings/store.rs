//! Atomic profile-scoped persistence for provider settings.

use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::persistence::app_config_dir;
use crate::providers::settings::model::{
    ProviderEnvVar, ProviderInstanceConfig, ProviderSettingsDocument, ProviderSettingsError,
};
use crate::providers::settings::secret::{
    protect_secret_value, reveal_secret_value, SecretCustodyError,
};
use crate::ui::workspace_layout::write_atomically;

const PROVIDERS_FILE_NAME: &str = "providers.json";
const MAX_SETTINGS_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Clone)]
pub enum ProviderSettingsStoreError {
    Settings(ProviderSettingsError),
    Secret(SecretCustodyError),
    Io(String),
    Json(String),
    Path(String),
    Corrupt(String),
}

impl std::fmt::Display for ProviderSettingsStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Settings(error) => write!(f, "{error}"),
            Self::Secret(error) => write!(f, "{error}"),
            Self::Io(msg) => write!(f, "provider settings io: {msg}"),
            Self::Json(msg) => write!(f, "provider settings json: {msg}"),
            Self::Path(msg) => write!(f, "provider settings path unavailable: {msg}"),
            Self::Corrupt(msg) => write!(f, "corrupt provider settings: {msg}"),
        }
    }
}

impl std::fmt::Debug for ProviderSettingsStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}

impl std::error::Error for ProviderSettingsStoreError {}

impl From<ProviderSettingsError> for ProviderSettingsStoreError {
    fn from(value: ProviderSettingsError) -> Self {
        Self::Settings(value)
    }
}

impl From<SecretCustodyError> for ProviderSettingsStoreError {
    fn from(value: SecretCustodyError) -> Self {
        Self::Secret(value)
    }
}

impl From<io::Error> for ProviderSettingsStoreError {
    fn from(value: io::Error) -> Self {
        Self::Io(value.to_string())
    }
}

impl From<serde_json::Error> for ProviderSettingsStoreError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value.to_string())
    }
}

#[derive(Clone)]
pub struct ProviderSettingsStore {
    path: PathBuf,
    custody_scope: Vec<u8>,
    inner: Arc<Mutex<ProviderSettingsDocument>>,
}

impl ProviderSettingsStore {
    pub fn open_profile_default() -> Result<Self, ProviderSettingsStoreError> {
        let dir = app_config_dir().map_err(|e| {
            ProviderSettingsStoreError::Path(format!("app_config_dir unavailable: {e}"))
        })?;
        Self::open_dir(&dir)
    }

    pub fn open_dir(dir: &Path) -> Result<Self, ProviderSettingsStoreError> {
        fs::create_dir_all(dir)?;
        let path = dir.join(PROVIDERS_FILE_NAME);
        let custody_scope = dir.to_string_lossy().as_bytes().to_vec();
        let document = if path.exists() {
            load_document(&path, &custody_scope)?
        } else {
            let doc = ProviderSettingsDocument::with_builtins();
            save_document(&path, &custody_scope, &doc)?;
            doc
        };
        document.validate()?;
        Ok(Self {
            path,
            custody_scope,
            inner: Arc::new(Mutex::new(document)),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn snapshot(&self) -> ProviderSettingsDocument {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn redacted_snapshot(&self) -> ProviderSettingsDocument {
        self.snapshot().redacted_projection()
    }

    pub fn replace(
        &self,
        next: ProviderSettingsDocument,
    ) -> Result<ProviderSettingsDocument, ProviderSettingsStoreError> {
        self.replace_with_expected_revision(None, next)
    }

    /// Persist `next` only when `expected_revision` matches the in-memory doc
    /// (when provided). Seals secrets before writing and before publishing memory.
    pub fn replace_with_expected_revision(
        &self,
        expected_revision: Option<u64>,
        mut next: ProviderSettingsDocument,
    ) -> Result<ProviderSettingsDocument, ProviderSettingsStoreError> {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(expected) = expected_revision {
            if guard.revision != expected {
                return Err(ProviderSettingsStoreError::Settings(
                    ProviderSettingsError::StaleRevision {
                        expected,
                        actual: guard.revision,
                    },
                ));
            }
        }
        self.merge_blank_secrets_from_doc(&mut next, &guard)?;
        next.revision = guard.revision.saturating_add(1);
        seal_document_in_place(&mut next, &self.custody_scope)?;
        next.validate()?;
        save_document(&self.path, &self.custody_scope, &next)?;
        *guard = next.clone();
        Ok(next)
    }

    pub fn update<F>(
        &self,
        mutator: F,
    ) -> Result<ProviderSettingsDocument, ProviderSettingsStoreError>
    where
        F: FnOnce(&mut ProviderSettingsDocument) -> Result<(), ProviderSettingsError>,
    {
        let current = self.snapshot();
        let expected = current.revision;
        let mut next = current;
        mutator(&mut next)?;
        self.replace_with_expected_revision(Some(expected), next)
    }

    fn merge_blank_secrets_from_doc(
        &self,
        next: &mut ProviderSettingsDocument,
        current: &ProviderSettingsDocument,
    ) -> Result<(), ProviderSettingsStoreError> {
        for instance in &mut next.instances {
            let Some(prev) = current.get(instance.instance_id.as_str()) else {
                continue;
            };
            merge_instance_secrets(instance, prev, &self.custody_scope)?;
        }
        Ok(())
    }

    pub fn custody_scope_for_instance(&self, instance_id: &str) -> Vec<u8> {
        let mut scope = self.custody_scope.clone();
        scope.extend_from_slice(b"/");
        scope.extend_from_slice(instance_id.as_bytes());
        scope
    }

    pub fn resolve_environment_map(
        &self,
        instance: &ProviderInstanceConfig,
    ) -> Result<std::collections::BTreeMap<String, String>, ProviderSettingsStoreError> {
        let scope = self.custody_scope_for_instance(instance.instance_id.as_str());
        let mut out = std::collections::BTreeMap::new();
        for env in &instance.environment {
            let value = if env.sensitive {
                match &env.protected_value {
                    Some(blob) => reveal_secret_value(blob, &scope)?.to_string(),
                    None => env.value.clone().unwrap_or_default(),
                }
            } else {
                env.value.clone().unwrap_or_default()
            };
            if !env.name.is_empty() {
                out.insert(env.name.clone(), value);
            }
        }
        Ok(out)
    }
}

fn merge_instance_secrets(
    next: &mut ProviderInstanceConfig,
    prev: &ProviderInstanceConfig,
    profile_scope: &[u8],
) -> Result<(), ProviderSettingsStoreError> {
    let mut scope = profile_scope.to_vec();
    scope.extend_from_slice(b"/");
    scope.extend_from_slice(next.instance_id.as_str().as_bytes());
    for env in &mut next.environment {
        let prev_env = prev.environment.iter().find(|p| {
            if cfg!(windows) {
                p.name.eq_ignore_ascii_case(&env.name)
            } else {
                p.name == env.name
            }
        });
        if !env.sensitive {
            // A redacted value is not an empty value. Require explicit replacement
            // when declassifying instead of erasing or exposing the stored secret.
            if env.value.as_ref().is_none_or(|value| value.is_empty())
                && prev_env.is_some_and(|previous| previous.protected_value.is_some())
            {
                return Err(ProviderSettingsStoreError::Corrupt(
                    "enter a replacement value before making a stored secret non-sensitive".into(),
                ));
            }
            env.protected_value = None;
            env.value_redacted = false;
            continue;
        }
        let blank_new = env.value.as_ref().map(|v| v.is_empty()).unwrap_or(true);
        if blank_new {
            if let Some(prev_env) = prev_env {
                if let Some(blob) = &prev_env.protected_value {
                    env.protected_value = Some(blob.clone());
                    env.value = None;
                    env.value_redacted = true;
                    continue;
                }
            }
            // No prior secret and blank input — leave unset.
            env.protected_value = None;
            env.value = None;
            env.value_redacted = false;
        } else if let Some(plain) = env.value.take() {
            env.protected_value = Some(protect_secret_value(&plain, &scope)?);
            env.value_redacted = true;
        }
    }
    Ok(())
}

fn load_document(
    path: &Path,
    custody_scope: &[u8],
) -> Result<ProviderSettingsDocument, ProviderSettingsStoreError> {
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(MAX_SETTINGS_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_SETTINGS_BYTES {
        return Err(ProviderSettingsStoreError::Corrupt(
            "providers.json exceeds the size limit".into(),
        ));
    }
    if bytes.is_empty() {
        return Err(ProviderSettingsStoreError::Corrupt(
            "providers.json is empty".into(),
        ));
    }
    let mut doc: ProviderSettingsDocument = serde_json::from_slice(&bytes).map_err(|e| {
        ProviderSettingsStoreError::Corrupt(format!("providers.json parse failed: {e}"))
    })?;
    let contained_plaintext = doc.instances.iter().any(|instance| {
        instance
            .environment
            .iter()
            .any(|env| env.sensitive && env.value.is_some())
    });
    // Ensure sensitive values are never left as plaintext after load.
    for instance in &mut doc.instances {
        let mut scope = custody_scope.to_vec();
        scope.extend_from_slice(b"/");
        scope.extend_from_slice(instance.instance_id.as_str().as_bytes());
        for env in &mut instance.environment {
            seal_env_in_place(env, &scope)?;
        }
    }
    doc.validate()
        .map_err(|e| ProviderSettingsStoreError::Corrupt(e.to_string()))?;
    // Ensure stub/builtin catalog rows exist even for older files.
    ensure_catalog_coverage(&mut doc)?;
    if contained_plaintext {
        save_document(path, custody_scope, &doc)?;
    }
    Ok(doc)
}

fn seal_document_in_place(
    doc: &mut ProviderSettingsDocument,
    custody_scope: &[u8],
) -> Result<(), ProviderSettingsStoreError> {
    for instance in &mut doc.instances {
        let mut scope = custody_scope.to_vec();
        scope.extend_from_slice(b"/");
        scope.extend_from_slice(instance.instance_id.as_str().as_bytes());
        for env in &mut instance.environment {
            seal_env_in_place(env, &scope)?;
        }
    }
    Ok(())
}

fn seal_env_in_place(
    env: &mut ProviderEnvVar,
    scope: &[u8],
) -> Result<(), ProviderSettingsStoreError> {
    if !env.sensitive {
        env.protected_value = None;
        env.value_redacted = false;
        return Ok(());
    }
    if env.protected_value.is_some() {
        env.value = None;
        env.value_redacted = true;
        return Ok(());
    }
    if let Some(plain) = env.value.take() {
        if !plain.is_empty() {
            #[cfg(windows)]
            {
                env.protected_value = Some(protect_secret_value(&plain, scope)?);
                env.value_redacted = true;
            }
            #[cfg(not(windows))]
            {
                // Sensitive secrets remain honestly unsupported off Windows.
                env.value = Some(plain);
                return Err(ProviderSettingsStoreError::Secret(
                    SecretCustodyError::Unsupported,
                ));
            }
        } else {
            env.value_redacted = false;
        }
    }
    Ok(())
}

fn ensure_catalog_coverage(
    doc: &mut ProviderSettingsDocument,
) -> Result<(), ProviderSettingsStoreError> {
    let defaults = ProviderSettingsDocument::with_builtins();
    for default in defaults.instances {
        if doc.get(default.instance_id.as_str()).is_none() {
            doc.instances.push(default);
        }
    }
    doc.validate()?;
    Ok(())
}

fn save_document(
    path: &Path,
    custody_scope: &[u8],
    doc: &ProviderSettingsDocument,
) -> Result<(), ProviderSettingsStoreError> {
    let mut sealed = doc.clone();
    for instance in &mut sealed.instances {
        let mut scope = custody_scope.to_vec();
        scope.extend_from_slice(b"/");
        scope.extend_from_slice(instance.instance_id.as_str().as_bytes());
        for env in &mut instance.environment {
            seal_env_in_place(env, &scope)?;
        }
    }
    sealed.validate()?;
    // Never write plaintext sensitive values.
    for instance in &sealed.instances {
        for env in &instance.environment {
            if env.sensitive && env.value.is_some() {
                return Err(ProviderSettingsStoreError::Corrupt(
                    "refusing to persist plaintext sensitive env".into(),
                ));
            }
        }
    }
    let bytes = serde_json::to_vec_pretty(&sealed)?;
    if bytes.len() as u64 > MAX_SETTINGS_BYTES {
        return Err(ProviderSettingsStoreError::Corrupt(
            "provider settings exceed the size limit".into(),
        ));
    }
    write_atomically(path, &bytes)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::settings::model::{
        BuiltinProviderDriver, ProviderDriverKind, ProviderInstanceId,
    };
    use tempfile::tempdir;

    #[test]
    fn roundtrip_preserves_unknown_and_defaults() {
        let dir = tempdir().unwrap();
        let store = ProviderSettingsStore::open_dir(dir.path()).unwrap();
        let mut doc = store.snapshot();
        doc.unknown
            .insert("extra".into(), serde_json::Value::String("keep-me".into()));
        doc.set_health_interval(0);
        store.replace(doc).unwrap();
        let reopened = ProviderSettingsStore::open_dir(dir.path()).unwrap();
        let snap = reopened.snapshot();
        assert_eq!(snap.health_interval_secs, 0);
        assert_eq!(
            snap.unknown.get("extra").and_then(|v| v.as_str()),
            Some("keep-me")
        );
        assert!(snap.get("grok").unwrap().driver.is_stub());
    }

    #[cfg(windows)]
    #[test]
    fn sensitive_env_persists_protected_and_blank_preserves() {
        let dir = tempdir().unwrap();
        let store = ProviderSettingsStore::open_dir(dir.path()).unwrap();
        store
            .update(|doc| {
                let mut claude = doc.get("claude").unwrap().clone();
                claude.environment.push(ProviderEnvVar {
                    name: "API_TOKEN".into(),
                    value: Some("tok-live".into()),
                    sensitive: true,
                    protected_value: None,
                    value_redacted: false,
                });
                doc.upsert_instance(claude)
            })
            .unwrap();
        let raw = fs::read_to_string(store.path()).unwrap();
        assert!(!raw.contains("tok-live"));
        assert!(raw.contains("protectedValue") || raw.contains("protected_value"));

        // Blank redacted save preserves secret.
        store
            .update(|doc| {
                let mut claude = doc.get("claude").unwrap().clone();
                claude.environment[0].value = None;
                claude.environment[0].value_redacted = true;
                claude.environment[0].protected_value = None;
                doc.upsert_instance(claude)
            })
            .unwrap();
        let claude = store.snapshot().get("claude").unwrap().clone();
        let map = store.resolve_environment_map(&claude).unwrap();
        assert_eq!(map.get("API_TOKEN").map(String::as_str), Some("tok-live"));
    }

    #[test]
    fn invalid_env_rejected() {
        let dir = tempdir().unwrap();
        let store = ProviderSettingsStore::open_dir(dir.path()).unwrap();
        let err = store.update(|doc| {
            let mut claude = doc.get("claude").unwrap().clone();
            claude.environment.push(ProviderEnvVar {
                name: "bad-name!".into(),
                value: Some("x".into()),
                sensitive: false,
                protected_value: None,
                value_redacted: false,
            });
            doc.upsert_instance(claude)
        });
        assert!(err.is_err());
    }

    #[test]
    fn stub_enable_rejected_by_store() {
        let dir = tempdir().unwrap();
        let store = ProviderSettingsStore::open_dir(dir.path()).unwrap();
        let err = store.update(|doc| {
            let mut grok = doc.get("grok").unwrap().clone();
            grok.enabled = true;
            doc.upsert_instance(grok)
        });
        assert!(err.is_err());
    }

    #[test]
    fn custom_instance_add_and_delete() {
        let dir = tempdir().unwrap();
        let store = ProviderSettingsStore::open_dir(dir.path()).unwrap();
        store
            .update(|doc| {
                let mut custom =
                    ProviderInstanceConfig::builtin_default(BuiltinProviderDriver::Codex);
                custom.instance_id = ProviderInstanceId::new("codex_work").unwrap();
                custom.display_name = "Codex Work".into();
                custom.driver = ProviderDriverKind::Codex;
                doc.upsert_instance(custom)
            })
            .unwrap();
        store
            .update(|doc| {
                doc.remove_custom_instance("codex_work")?;
                Ok(())
            })
            .unwrap();
        assert!(store.snapshot().get("codex_work").is_none());
    }

    #[test]
    fn corrupt_json_fails_closed() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join(PROVIDERS_FILE_NAME), b"{not-json").unwrap();
        assert!(ProviderSettingsStore::open_dir(dir.path()).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn redacted_secret_declassification_requires_replacement_and_preserves_state() {
        let dir = tempdir().unwrap();
        let store = ProviderSettingsStore::open_dir(dir.path()).unwrap();
        store
            .update(|doc| {
                let mut instance = doc.get("claude").unwrap().clone();
                instance.environment.push(ProviderEnvVar {
                    name: "API_TOKEN".into(),
                    value: Some("private-token".into()),
                    sensitive: true,
                    protected_value: None,
                    value_redacted: false,
                });
                doc.upsert_instance(instance)
            })
            .unwrap();
        let saved_bytes = fs::read(store.path()).unwrap();
        let mut redacted = store.redacted_snapshot();
        let instance = redacted
            .instances
            .iter_mut()
            .find(|i| i.instance_id.as_str() == "claude")
            .unwrap();
        instance.environment[0].sensitive = false;
        assert!(store.replace(redacted).is_err());
        assert_eq!(fs::read(store.path()).unwrap(), saved_bytes);
        assert_eq!(
            store
                .resolve_environment_map(store.snapshot().get("claude").unwrap())
                .unwrap()["API_TOKEN"],
            "private-token"
        );
    }

    #[test]
    fn failed_persistence_does_not_publish_next_revision() {
        let dir = tempdir().unwrap();
        let store = ProviderSettingsStore::open_dir(dir.path()).unwrap();
        let revision = store.snapshot().revision;
        // Turn this test-owned destination into a directory to force atomic-write failure.
        fs::remove_file(store.path()).unwrap();
        fs::create_dir(store.path()).unwrap();
        assert!(store
            .update(|doc| {
                doc.set_health_interval(0);
                Ok(())
            })
            .is_err());
        assert_eq!(store.snapshot().revision, revision);
        assert_eq!(store.snapshot().health_interval_secs, 300);
    }
}
