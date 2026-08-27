//! Durable task → provider instance binding for exact-resume.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::domain::TaskId;
use crate::persistence::app_config_dir;
use crate::providers::settings::model::{
    ProviderSettingsDocument, ProviderSettingsError, CLAUDE_DEFAULT_INSTANCE_ID,
    CODEX_DEFAULT_INSTANCE_ID, CURSOR_DEFAULT_INSTANCE_ID,
};
use crate::providers::ProviderKind;
use crate::ui::workspace_layout::write_atomically;

const BINDINGS_FILE_NAME: &str = "provider_instance_bindings.json";

#[derive(Clone, PartialEq, Eq)]
pub enum ProviderInstanceBindingError {
    Settings(ProviderSettingsError),
    Io(String),
    Corrupt(String),
    MissingInstance(String, String),
    InstanceChanged(String, String, String),
    StubBinding(String),
    Unbound(String),
}

impl fmt::Display for ProviderInstanceBindingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Settings(error) => write!(f, "{error}"),
            Self::Io(msg) => write!(f, "provider instance binding io: {msg}"),
            Self::Corrupt(msg) => write!(f, "provider instance binding corrupt: {msg}"),
            Self::MissingInstance(task, id) => {
                write!(
                    f,
                    "task `{task}` is bound to missing provider instance `{id}`"
                )
            }
            Self::InstanceChanged(task, id, reason) => {
                write!(
                    f,
                    "task `{task}` is bound to changed provider instance `{id}`: {reason}"
                )
            }
            Self::StubBinding(id) => write!(f, "cannot bind stub provider instance `{id}`"),
            Self::Unbound(task) => write!(f, "task `{task}` has no provider instance binding"),
        }
    }
}

impl fmt::Debug for ProviderInstanceBindingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl std::error::Error for ProviderInstanceBindingError {}

impl From<ProviderSettingsError> for ProviderInstanceBindingError {
    fn from(value: ProviderSettingsError) -> Self {
        Self::Settings(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderInstanceBinding {
    pub task_id: String,
    pub instance_id: String,
    pub driver: String,
    /// Non-secret launch identity fingerprint captured at bind time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_identity_fingerprint: Option<String>,
    /// Optional account fingerprint when a trusted probe exposed one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_fingerprint: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct BindingsDocument {
    revision: u64,
    bindings: BTreeMap<String, ProviderInstanceBinding>,
}

#[derive(Clone)]
pub struct ProviderInstanceBindingStore {
    path: PathBuf,
    inner: Arc<Mutex<BindingsDocument>>,
}

impl ProviderInstanceBindingStore {
    pub fn open_profile_default() -> Result<Self, ProviderInstanceBindingError> {
        let dir = app_config_dir().map_err(|e| {
            ProviderInstanceBindingError::Io(format!("app_config_dir unavailable: {e}"))
        })?;
        Self::open_dir(&dir)
    }

    pub fn open_dir(dir: &Path) -> Result<Self, ProviderInstanceBindingError> {
        fs::create_dir_all(dir).map_err(|e| ProviderInstanceBindingError::Io(e.to_string()))?;
        let path = dir.join(BINDINGS_FILE_NAME);
        let document = if path.exists() {
            let bytes =
                fs::read(&path).map_err(|e| ProviderInstanceBindingError::Io(e.to_string()))?;
            let doc: BindingsDocument = serde_json::from_slice(&bytes).map_err(|e| {
                ProviderInstanceBindingError::Corrupt(format!("bindings parse failed: {e}"))
            })?;
            validate_loaded_document(&doc)?;
            doc
        } else {
            BindingsDocument::default()
        };
        Ok(Self {
            path,
            inner: Arc::new(Mutex::new(document)),
        })
    }

    pub fn get(&self, task_id: &TaskId) -> Option<ProviderInstanceBinding> {
        let key = task_id.to_string();
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .bindings
            .get(&key)
            .cloned()
    }

    /// Bind on first launch. Subsequent calls must match the exact binding.
    pub fn bind_on_first_launch(
        &self,
        task_id: &TaskId,
        instance_id: &str,
        driver: &str,
        launch_identity_fingerprint: Option<String>,
        settings: &ProviderSettingsDocument,
    ) -> Result<ProviderInstanceBinding, ProviderInstanceBindingError> {
        let instance = settings.require_enabled_launchable(instance_id)?;
        if instance.driver.is_stub() {
            return Err(ProviderInstanceBindingError::StubBinding(
                instance_id.to_string(),
            ));
        }
        if instance.driver.as_str() != driver {
            return Err(ProviderInstanceBindingError::InstanceChanged(
                task_id.to_string(),
                instance_id.to_string(),
                format!("driver mismatch {} vs {driver}", instance.driver.as_str()),
            ));
        }
        let key = task_id.to_string();
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = guard.bindings.get(&key) {
            validate_existing(
                existing,
                instance_id,
                driver,
                launch_identity_fingerprint.as_deref(),
                instance,
            )?;
            return Ok(existing.clone());
        }
        let mut next = guard.clone();
        let binding = ProviderInstanceBinding {
            task_id: key.clone(),
            instance_id: instance_id.to_string(),
            driver: driver.to_string(),
            launch_identity_fingerprint,
            account_fingerprint: None,
        };
        next.bindings.insert(key, binding.clone());
        next.revision = next.revision.saturating_add(1);
        persist(&self.path, &next)?;
        *guard = next;
        Ok(binding)
    }

    pub fn require_binding_for_resume(
        &self,
        task_id: &TaskId,
        settings: &ProviderSettingsDocument,
    ) -> Result<ProviderInstanceBinding, ProviderInstanceBindingError> {
        let key = task_id.to_string();
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let Some(binding) = guard.bindings.get(&key).cloned() else {
            return Err(ProviderInstanceBindingError::Unbound(key));
        };
        drop(guard);
        let Some(instance) = settings.get(&binding.instance_id) else {
            return Err(ProviderInstanceBindingError::MissingInstance(
                binding.task_id,
                binding.instance_id,
            ));
        };
        if instance.driver.as_str() != binding.driver {
            return Err(ProviderInstanceBindingError::InstanceChanged(
                binding.task_id,
                binding.instance_id,
                format!(
                    "driver changed from {} to {}",
                    binding.driver,
                    instance.driver.as_str()
                ),
            ));
        }
        if !instance.enabled || instance.driver.is_stub() {
            return Err(ProviderInstanceBindingError::InstanceChanged(
                binding.task_id,
                binding.instance_id,
                "instance disabled or stub".into(),
            ));
        }
        if let Some(expected) = binding.launch_identity_fingerprint.as_deref() {
            if !instance.matches_launch_identity_fingerprint(expected) {
                return Err(ProviderInstanceBindingError::InstanceChanged(
                    binding.task_id,
                    binding.instance_id,
                    "launch identity fingerprint changed".into(),
                ));
            }
        }
        Ok(binding)
    }
}

fn validate_loaded_document(doc: &BindingsDocument) -> Result<(), ProviderInstanceBindingError> {
    for (key, binding) in &doc.bindings {
        if key != &binding.task_id {
            return Err(ProviderInstanceBindingError::Corrupt(format!(
                "binding key `{key}` does not match task_id `{}`",
                binding.task_id
            )));
        }
        if binding.instance_id.is_empty() || binding.driver.is_empty() {
            return Err(ProviderInstanceBindingError::Corrupt(
                "binding missing instance_id or driver".into(),
            ));
        }
    }
    Ok(())
}

fn validate_existing(
    existing: &ProviderInstanceBinding,
    instance_id: &str,
    driver: &str,
    launch_identity_fingerprint: Option<&str>,
    instance: &super::model::ProviderInstanceConfig,
) -> Result<(), ProviderInstanceBindingError> {
    if existing.instance_id != instance_id {
        return Err(ProviderInstanceBindingError::InstanceChanged(
            existing.task_id.clone(),
            existing.instance_id.clone(),
            format!("refusing fallback to `{instance_id}`"),
        ));
    }
    if existing.driver != driver {
        return Err(ProviderInstanceBindingError::InstanceChanged(
            existing.task_id.clone(),
            existing.instance_id.clone(),
            format!("driver mismatch {} vs {driver}", existing.driver),
        ));
    }
    if let (Some(expected), Some(actual)) = (
        existing.launch_identity_fingerprint.as_deref(),
        launch_identity_fingerprint,
    ) {
        if expected != actual
            && !(instance.matches_launch_identity_fingerprint(expected)
                && instance.matches_launch_identity_fingerprint(actual))
        {
            return Err(ProviderInstanceBindingError::InstanceChanged(
                existing.task_id.clone(),
                existing.instance_id.clone(),
                "launch identity fingerprint changed".into(),
            ));
        }
    }
    Ok(())
}

fn persist(path: &Path, doc: &BindingsDocument) -> Result<(), ProviderInstanceBindingError> {
    let bytes = serde_json::to_vec_pretty(doc)
        .map_err(|e| ProviderInstanceBindingError::Corrupt(e.to_string()))?;
    write_atomically(path, &bytes)
        .map_err(|e: io::Error| ProviderInstanceBindingError::Io(e.to_string()))?;
    Ok(())
}

pub fn default_instance_id_for_kind(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::ClaudeCode => CLAUDE_DEFAULT_INSTANCE_ID,
        ProviderKind::Codex => CODEX_DEFAULT_INSTANCE_ID,
        ProviderKind::Cursor => CURSOR_DEFAULT_INSTANCE_ID,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::settings::model::ProviderSettingsDocument;
    use tempfile::tempdir;

    #[test]
    fn first_bind_retained_and_mismatch_fails() {
        let dir = tempdir().unwrap();
        let store = ProviderInstanceBindingStore::open_dir(dir.path()).unwrap();
        let settings = ProviderSettingsDocument::with_builtins();
        let task = TaskId::new();
        let binding = store
            .bind_on_first_launch(&task, "claude", "claude", Some("fp".into()), &settings)
            .unwrap();
        assert_eq!(binding.instance_id, "claude");
        let err = store.bind_on_first_launch(&task, "codex", "codex", None, &settings);
        assert!(matches!(
            err,
            Err(ProviderInstanceBindingError::InstanceChanged(_, _, _))
        ));
    }

    #[test]
    fn failed_persist_leaves_no_phantom() {
        let dir = tempdir().unwrap();
        let store = ProviderInstanceBindingStore::open_dir(dir.path()).unwrap();
        // Replace path with a file so persist fails.
        let bogon = dir.path().join("not-a-dir");
        fs::write(&bogon, b"x").unwrap();
        let broken = ProviderInstanceBindingStore {
            path: bogon.join("bindings.json"),
            inner: store.inner.clone(),
        };
        let settings = ProviderSettingsDocument::with_builtins();
        let task = TaskId::new();
        let err = broken.bind_on_first_launch(&task, "claude", "claude", None, &settings);
        assert!(err.is_err());
        assert!(store.get(&task).is_none());
    }

    #[test]
    fn stub_cannot_bind() {
        let dir = tempdir().unwrap();
        let store = ProviderInstanceBindingStore::open_dir(dir.path()).unwrap();
        let settings = ProviderSettingsDocument::with_builtins();
        let err = store.bind_on_first_launch(&TaskId::new(), "grok", "grok", None, &settings);
        assert!(err.is_err());
    }
}
