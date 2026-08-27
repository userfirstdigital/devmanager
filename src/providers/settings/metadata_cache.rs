//! Persistent last-good provider metadata cache at the profile root.
//!
//! Malformed files fail closed to empty. Never stores tokens/API keys/home
//! paths. Account fingerprint changes invalidate usage even when config is
//! unchanged. Persisted rows are validated with the same bounds as live data.

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use sha2::{Digest, Sha256};

use super::launch_policy::ResolvedProviderLaunchConfig;
use super::metadata_types::{
    CachedModelCatalog, CachedUsageSnapshot, DiscoveredModel, ProviderMetadataCacheDocument,
    ProviderMetadataCacheEntry, ProviderUsageStateWire, ProviderUsageWindowWire,
    MAX_METADATA_EFFORTS, MAX_METADATA_MODELS, MAX_USAGE_WINDOWS, METADATA_CACHE_VERSION,
    METADATA_STALE_AFTER_MS,
};
use super::model::{normalize_model_slug, validate_instance_id, ProviderInstanceConfig};
use super::usage_http::effective_env_has_api_key;

const CACHE_FILE_NAME: &str = "provider_metadata_cache.json";
const MAX_CACHE_BYTES: u64 = 512 * 1024;
const MAX_CACHE_ENTRIES: usize = 32;
const MAX_FINGERPRINT_LEN: usize = 128;
const MAX_LABEL_LEN: usize = 128;

#[derive(Debug, Clone)]
pub enum MetadataCacheError {
    Io(String),
    Path(String),
}

impl std::fmt::Display for MetadataCacheError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(msg) => write!(f, "metadata cache io: {msg}"),
            Self::Path(msg) => write!(f, "metadata cache path: {msg}"),
        }
    }
}

impl std::error::Error for MetadataCacheError {}

#[derive(Clone)]
pub struct ProviderMetadataCache {
    path: PathBuf,
    inner: Arc<Mutex<ProviderMetadataCacheDocument>>,
}

impl ProviderMetadataCache {
    pub fn open_dir(dir: &Path) -> Result<Self, MetadataCacheError> {
        fs::create_dir_all(dir).map_err(|e| MetadataCacheError::Io(e.to_string()))?;
        let path = dir.join(CACHE_FILE_NAME);
        let document = load_or_empty(&path);
        Ok(Self {
            path,
            inner: Arc::new(Mutex::new(document)),
        })
    }

    pub fn snapshot(&self) -> ProviderMetadataCacheDocument {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn entry(
        &self,
        instance_id: &str,
        config_fingerprint: &str,
    ) -> Option<ProviderMetadataCacheEntry> {
        self.snapshot().entries.into_iter().find(|entry| {
            entry.instance_id == instance_id && entry.config_fingerprint == config_fingerprint
        })
    }

    pub fn upsert_models(
        &self,
        instance_id: &str,
        driver: &str,
        config_fingerprint: &str,
        account_fingerprint: Option<String>,
        models: CachedModelCatalog,
    ) -> Result<(), MetadataCacheError> {
        let models = sanitize_model_catalog(models);
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let slot = find_or_insert(
            &mut guard,
            instance_id,
            driver,
            config_fingerprint,
            account_fingerprint.clone(),
        );
        if slot.account_fingerprint != account_fingerprint {
            slot.usage = CachedUsageSnapshot::default();
            slot.usage_backoff_until_unix_ms = None;
            slot.account_fingerprint = account_fingerprint;
        }
        slot.models = models;
        persist(&self.path, &guard)
    }

    pub fn upsert_usage(
        &self,
        instance_id: &str,
        driver: &str,
        config_fingerprint: &str,
        account_fingerprint: Option<String>,
        usage: CachedUsageSnapshot,
        backoff_until_unix_ms: Option<u64>,
    ) -> Result<(), MetadataCacheError> {
        let usage = sanitize_usage_snapshot(usage);
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let slot = find_or_insert(
            &mut guard,
            instance_id,
            driver,
            config_fingerprint,
            account_fingerprint.clone(),
        );
        if slot.account_fingerprint != account_fingerprint {
            // Never retag prior models under a new account identity.
            slot.models = CachedModelCatalog::default();
            slot.usage = CachedUsageSnapshot::default();
            slot.account_fingerprint = account_fingerprint;
        }
        slot.usage = usage;
        slot.usage_backoff_until_unix_ms = backoff_until_unix_ms;
        persist(&self.path, &guard)
    }

    /// Clear models+usage when account proof is missing or mismatched.
    pub fn clear_incompatible_account(
        &self,
        instance_id: &str,
        config_fingerprint: &str,
        expected_account: Option<&str>,
    ) -> Result<(), MetadataCacheError> {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut changed = false;
        for entry in &mut guard.entries {
            if entry.instance_id == instance_id && entry.config_fingerprint == config_fingerprint {
                if entry.account_fingerprint.as_deref() != expected_account {
                    entry.models = CachedModelCatalog::default();
                    entry.usage = CachedUsageSnapshot::default();
                    entry.usage_backoff_until_unix_ms = None;
                    entry.account_fingerprint = expected_account.map(str::to_string);
                    changed = true;
                }
            }
        }
        if changed {
            persist(&self.path, &guard)?;
        }
        Ok(())
    }

    /// Alias used by older call sites / tests when the account scope changes.
    pub fn invalidate_usage_for_account_change(
        &self,
        instance_id: &str,
        config_fingerprint: &str,
        expected_account: Option<&str>,
    ) -> Result<(), MetadataCacheError> {
        self.clear_incompatible_account(instance_id, config_fingerprint, expected_account)
    }

    pub fn prune_to_instances(&self, live: &[(String, String)]) -> Result<(), MetadataCacheError> {
        let allowed: std::collections::BTreeSet<(String, String)> = live.iter().cloned().collect();
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let before = guard.entries.len();
        guard.entries.retain(|entry| {
            allowed.contains(&(entry.instance_id.clone(), entry.config_fingerprint.clone()))
        });
        if guard.entries.len() != before {
            persist(&self.path, &guard)?;
        }
        Ok(())
    }

    pub fn clear(&self) -> Result<(), MetadataCacheError> {
        let empty = ProviderMetadataCacheDocument {
            version: METADATA_CACHE_VERSION,
            entries: Vec::new(),
        };
        *self.inner.lock().unwrap_or_else(|e| e.into_inner()) = empty.clone();
        persist(&self.path, &empty)
    }
}

fn find_or_insert<'a>(
    doc: &'a mut ProviderMetadataCacheDocument,
    instance_id: &str,
    driver: &str,
    config_fingerprint: &str,
    account_fingerprint: Option<String>,
) -> &'a mut ProviderMetadataCacheEntry {
    if let Some(idx) = doc.entries.iter().position(|entry| {
        entry.instance_id == instance_id && entry.config_fingerprint == config_fingerprint
    }) {
        return &mut doc.entries[idx];
    }
    if doc.entries.len() >= MAX_CACHE_ENTRIES {
        doc.entries.remove(0);
    }
    doc.entries.push(ProviderMetadataCacheEntry {
        instance_id: instance_id.to_string(),
        driver: driver.to_string(),
        config_fingerprint: config_fingerprint.to_string(),
        account_fingerprint,
        models: CachedModelCatalog::default(),
        usage: CachedUsageSnapshot::default(),
        usage_backoff_until_unix_ms: None,
    });
    doc.entries.last_mut().expect("just pushed")
}

fn load_or_empty(path: &Path) -> ProviderMetadataCacheDocument {
    match load_document(path) {
        Ok(doc) => doc,
        Err(_) => ProviderMetadataCacheDocument {
            version: METADATA_CACHE_VERSION,
            entries: Vec::new(),
        },
    }
}

fn load_document(path: &Path) -> Result<ProviderMetadataCacheDocument, MetadataCacheError> {
    if !path.exists() {
        return Ok(ProviderMetadataCacheDocument {
            version: METADATA_CACHE_VERSION,
            entries: Vec::new(),
        });
    }
    let meta = fs::metadata(path).map_err(|e| MetadataCacheError::Io(e.to_string()))?;
    if !meta.is_file() || meta.len() > MAX_CACHE_BYTES {
        return Err(MetadataCacheError::Io("cache file invalid".into()));
    }
    let mut file = fs::File::open(path).map_err(|e| MetadataCacheError::Io(e.to_string()))?;
    let mut bytes = Vec::new();
    file.take(MAX_CACHE_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|e| MetadataCacheError::Io(e.to_string()))?;
    if bytes.len() as u64 > MAX_CACHE_BYTES {
        return Err(MetadataCacheError::Io("cache too large".into()));
    }
    let doc: ProviderMetadataCacheDocument = serde_json::from_slice(&bytes)
        .map_err(|e| MetadataCacheError::Io(format!("malformed: {e}")))?;
    if doc.version != METADATA_CACHE_VERSION {
        return Err(MetadataCacheError::Io("unsupported cache version".into()));
    }
    Ok(sanitize_document(doc))
}

fn sanitize_document(mut doc: ProviderMetadataCacheDocument) -> ProviderMetadataCacheDocument {
    doc.entries.truncate(MAX_CACHE_ENTRIES);
    doc.entries = doc
        .entries
        .into_iter()
        .filter_map(|entry| sanitize_entry(entry))
        .collect();
    doc
}

fn sanitize_entry(mut entry: ProviderMetadataCacheEntry) -> Option<ProviderMetadataCacheEntry> {
    validate_instance_id(&entry.instance_id).ok()?;
    if entry.driver.is_empty() || entry.driver.len() > 32 {
        return None;
    }
    if entry.config_fingerprint.is_empty() || entry.config_fingerprint.len() > MAX_FINGERPRINT_LEN {
        return None;
    }
    if entry
        .account_fingerprint
        .as_ref()
        .is_some_and(|fp| fp.is_empty() || fp.len() > MAX_FINGERPRINT_LEN)
    {
        entry.account_fingerprint = None;
    }
    entry.models = sanitize_model_catalog(entry.models);
    entry.usage = sanitize_usage_snapshot(entry.usage);
    Some(entry)
}

fn sanitize_model_catalog(mut catalog: CachedModelCatalog) -> CachedModelCatalog {
    catalog.models.truncate(MAX_METADATA_MODELS);
    catalog.models = catalog
        .models
        .into_iter()
        .filter_map(|model| sanitize_discovered_model(model))
        .collect();
    catalog
}

fn sanitize_usage_snapshot(mut usage: CachedUsageSnapshot) -> CachedUsageSnapshot {
    usage.windows.truncate(MAX_USAGE_WINDOWS);
    usage.windows = usage
        .windows
        .into_iter()
        .filter_map(|window| sanitize_usage_window(window))
        .collect();
    usage
}

fn persist(path: &Path, doc: &ProviderMetadataCacheDocument) -> Result<(), MetadataCacheError> {
    let sanitized = sanitize_document(doc.clone());
    let bytes =
        serde_json::to_vec_pretty(&sanitized).map_err(|e| MetadataCacheError::Io(e.to_string()))?;
    if bytes.len() as u64 > MAX_CACHE_BYTES {
        return Err(MetadataCacheError::Io(
            "refusing to persist oversized metadata cache".into(),
        ));
    }
    write_atomically_local(path, &bytes).map_err(|e| MetadataCacheError::Io(e.to_string()))
}

/// Truncate to at most `max_bytes` without panicking on a UTF-8 codepoint boundary.
fn truncate_str_at_boundary(value: &mut String, max_bytes: usize) {
    if value.len() <= max_bytes {
        return;
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
}

fn sanitize_discovered_model(mut model: DiscoveredModel) -> Option<DiscoveredModel> {
    let slug = normalize_model_slug(&model.slug).ok()?;
    model.slug = slug;
    if model.display_name.len() > MAX_LABEL_LEN {
        truncate_str_at_boundary(&mut model.display_name, MAX_LABEL_LEN);
    }
    model.supported_efforts.truncate(MAX_METADATA_EFFORTS);
    model.supported_efforts.retain(|e| {
        !e.is_empty()
            && e.len() <= 32
            && e.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    });
    if let Some(default) = model.default_effort.as_mut() {
        if default.is_empty() || default.len() > 32 {
            model.default_effort = None;
        }
    }
    model.input_modalities.truncate(8);
    Some(model)
}

fn sanitize_usage_window(mut window: ProviderUsageWindowWire) -> Option<ProviderUsageWindowWire> {
    if window.id.is_empty() || window.id.len() > 64 {
        return None;
    }
    if window.label.len() > MAX_LABEL_LEN {
        truncate_str_at_boundary(&mut window.label, MAX_LABEL_LEN);
    }
    // Fail closed on out-of-range percents rather than silently retaining them.
    if window.used_percent.is_some_and(|p| p > 100) {
        return None;
    }
    if window.remaining_percent.is_some_and(|p| p > 100) {
        return None;
    }
    if let Some(scope) = window.scope_label.as_mut() {
        if scope.len() > MAX_LABEL_LEN {
            truncate_str_at_boundary(scope, MAX_LABEL_LEN);
        }
    }
    Some(window)
}

/// Local atomic replace — ports the MoveFileExW REPLACE_EXISTING|WRITE_THROUGH
/// pattern from workspace_layout without a UI dependency. Never deletes the
/// existing destination on replacement failure.
fn write_atomically_local(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(CACHE_FILE_NAME);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let temporary = parent.join(format!(
        "{file_name}.devmanager-meta-tmp-{}-{stamp}",
        std::process::id()
    ));
    {
        let mut handle = fs::File::create(&temporary)?;
        handle.write_all(bytes)?;
        handle.sync_all()?;
    }
    match replace_file(&temporary, path) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = fs::remove_file(&temporary);
            Err(error)
        }
    }
}

#[cfg(windows)]
fn replace_file(temporary: &Path, destination: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let temporary: Vec<u16> = temporary.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    unsafe {
        MoveFileExW(
            PCWSTR(temporary.as_ptr()),
            PCWSTR(destination.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
        .map_err(io::Error::from)
    }
}

#[cfg(not(windows))]
fn replace_file(temporary: &Path, destination: &Path) -> io::Result<()> {
    fs::rename(temporary, destination)
}

/// Launch-identity fingerprint only (legacy helper).
pub fn config_scope_fingerprint(instance: &ProviderInstanceConfig) -> String {
    instance.launch_identity_fingerprint()
}

/// Effective scope includes resolved home/endpoint *values* (or matching
/// non-secret child-env values when resolved fields are absent) and API-key
/// *presence* only — never token/API-key secret values.
pub fn effective_scope_fingerprint(
    instance: &ProviderInstanceConfig,
    resolved: &ResolvedProviderLaunchConfig,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(instance.launch_identity_fingerprint().as_bytes());
    hasher.update(b"|");
    let home = resolved
        .shadow_home_path
        .as_ref()
        .or(resolved.home_path.as_ref())
        .map(|p| p.to_string_lossy().into_owned())
        .or_else(|| {
            env_value_from_map(
                &resolved.discovery.child_environment,
                &["CLAUDE_CONFIG_DIR", "CODEX_HOME"],
            )
        })
        .unwrap_or_default();
    hasher.update(home.as_bytes());
    hasher.update(b"|");
    let endpoint = resolved
        .api_endpoint
        .clone()
        .filter(|e| !e.trim().is_empty())
        .or_else(|| {
            env_value_from_map(
                &resolved.discovery.child_environment,
                &["CURSOR_API_ENDPOINT", "ANTHROPIC_BASE_URL"],
            )
        })
        .unwrap_or_default();
    hasher.update(endpoint.as_bytes());
    hasher.update(b"|");
    hasher.update([u8::from(
        effective_env_has_api_key(&resolved.discovery.child_environment)
            || effective_env_has_api_key(&resolved.environment),
    )]);
    // Stable set of API-key env *names* present (values never hashed).
    let mut key_names: Vec<String> = resolved
        .discovery
        .child_environment
        .keys()
        .chain(resolved.environment.keys())
        .map(|k| k.to_string_lossy().to_ascii_uppercase())
        .filter(|k| {
            matches!(
                k.as_str(),
                "ANTHROPIC_API_KEY"
                    | "ANTHROPIC_AUTH_TOKEN"
                    | "OPENAI_API_KEY"
                    | "CURSOR_API_KEY"
                    | "CURSOR_AUTH_TOKEN"
            )
        })
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    key_names.sort();
    for name in key_names {
        hasher.update(name.as_bytes());
        hasher.update(b";");
    }
    format!("{:x}", hasher.finalize())
}

fn env_value_from_map(
    env: &std::collections::BTreeMap<std::ffi::OsString, std::ffi::OsString>,
    names: &[&str],
) -> Option<String> {
    for name in names {
        if let Some((_, value)) = env.iter().find(|(k, _)| {
            if cfg!(windows) {
                k.to_string_lossy().eq_ignore_ascii_case(name)
            } else {
                k.as_os_str() == std::ffi::OsStr::new(name)
            }
        }) {
            let text = value.to_string_lossy();
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

pub fn is_stale(checked_at_unix_ms: Option<u64>, now_ms: u64) -> bool {
    match checked_at_unix_ms {
        Some(checked) if now_ms >= checked => {
            now_ms.saturating_sub(checked) >= METADATA_STALE_AFTER_MS
        }
        Some(_) => true,
        None => true,
    }
}

pub fn usage_state_for_cache(
    usage: &CachedUsageSnapshot,
    backoff_until: Option<u64>,
    now_ms: u64,
) -> ProviderUsageStateWire {
    if backoff_until.is_some_and(|until| until > now_ms) {
        return ProviderUsageStateWire::Backoff;
    }
    if usage.windows.is_empty() {
        return match usage.state {
            ProviderUsageStateWire::Unsupported => ProviderUsageStateWire::Unsupported,
            ProviderUsageStateWire::AuthRequired => ProviderUsageStateWire::AuthRequired,
            ProviderUsageStateWire::Failed => ProviderUsageStateWire::Failed,
            ProviderUsageStateWire::Unavailable => ProviderUsageStateWire::Unavailable,
            ProviderUsageStateWire::Backoff => ProviderUsageStateWire::Backoff,
            _ => ProviderUsageStateWire::Unknown,
        };
    }
    if is_stale(usage.checked_at_unix_ms, now_ms) {
        ProviderUsageStateWire::Stale
    } else {
        ProviderUsageStateWire::Fresh
    }
}

pub fn hash_secret_free(parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part.as_bytes());
    }
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod validate_tests {
    use super::*;

    #[test]
    fn rejects_invalid_percent_and_oversize_on_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CACHE_FILE_NAME);
        let bad = r#"{
          "version": 1,
          "entries": [{
            "instanceId": "claude",
            "driver": "claude",
            "configFingerprint": "abc",
            "models": { "models": [{ "slug": "bad slug", "displayName": "x" }] },
            "usage": { "windows": [{ "id": "w", "label": "L", "usedPercent": 250 }] }
          }]
        }"#;
        fs::write(&path, bad).unwrap();
        let cache = ProviderMetadataCache::open_dir(dir.path()).unwrap();
        let entry = cache.entry("claude", "abc").unwrap();
        assert!(entry.models.models.is_empty());
        assert!(entry.usage.windows.is_empty());
    }

    #[test]
    fn write_atomically_replaces_existing_twice() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("meta.json");
        write_atomically_local(&path, br#"{"v":1}"#).unwrap();
        write_atomically_local(&path, br#"{"v":2}"#).unwrap();
        assert_eq!(fs::read(&path).unwrap(), br#"{"v":2}"#);
    }

    #[test]
    fn write_atomically_failure_preserves_existing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("meta.json");
        write_atomically_local(&path, b"KEEP-OLD").unwrap();
        let original = fs::read(&path).unwrap();
        // Destination becomes a directory so MoveFileEx/rename fails; old bytes
        // for a sibling file must remain untouched, and the failing path must
        // not delete an existing destination file via the old remove-then-rename bug.
        let dir_dest = dir.path().join("as-dir.json");
        fs::create_dir(&dir_dest).unwrap();
        assert!(write_atomically_local(&dir_dest, b"NEW").is_err());
        assert!(dir_dest.is_dir());
        assert_eq!(fs::read(&path).unwrap(), original);
    }

    #[test]
    fn truncate_str_at_utf8_boundary_no_panic() {
        // "é" is 2 bytes; max 1 must not panic on mid-codepoint.
        let mut label = "aé".to_string();
        truncate_str_at_boundary(&mut label, 1);
        assert_eq!(label, "a");
        let mut emoji = "hi😀".to_string();
        let before = emoji.len();
        truncate_str_at_boundary(&mut emoji, before - 1);
        assert!(emoji.starts_with("hi"));
        assert!(emoji.is_char_boundary(emoji.len()));
    }
}
