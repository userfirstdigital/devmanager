//! Metadata-only discovery protocols for Claude / Codex / Cursor.
//!
//! Reuses attested interactive sessions. Never creates chat sessions, never
//! enables project hooks/MCP, and never goes through Codex conversation
//! launch (forbidden app-server guard stays on the launcher path).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::{json, Value as JsonValue};

use crate::providers::adapter::{
    materialize_provider_environment, ProviderInteractiveProbeError, ProviderInteractiveSession,
    WindowsProviderProbeRunner, MAX_PROVIDER_PROBE_OUTPUT_BYTES, MAX_PROVIDER_PROBE_TIMEOUT,
};
use crate::providers::capabilities::ProviderExecutableHandle;
use crate::providers::capabilities::ProviderExecutablePolicy;
use crate::providers::registry::{ProviderDiscoveryConfig, ProviderRegistry};
use crate::providers::settings::health::unix_now_ms;
use crate::providers::settings::launch_policy::{
    resolve_launch_config, ResolvedProviderLaunchConfig,
};
use crate::providers::settings::metadata_cache::{
    effective_scope_fingerprint, is_stale, usage_state_for_cache, ProviderMetadataCache,
};
use crate::providers::settings::metadata_parse::{
    codex_account_id_from_account_read, fingerprint_account_material,
    parse_claude_initialize_models, parse_codex_model_list, parse_codex_models_cache_file,
    parse_codex_rate_limits, parse_cursor_list_available_models,
};
use crate::providers::settings::metadata_types::{
    CachedModelCatalog, CachedUsageSnapshot, DiscoveredModel, ProviderMetadataSource,
    ProviderModelCatalogWire, ProviderModelEntryWire, ProviderUsageStateWire, ProviderUsageWire,
    MAX_METADATA_MODELS,
};
use crate::providers::settings::model::{
    normalize_model_slug, ProviderDriverKind, ProviderInstanceConfig,
};
use crate::providers::settings::store::ProviderSettingsStore;
use crate::providers::settings::usage_http::{
    codex_home_for_usage, effective_env_has_api_key, query_claude_usage, query_cursor_usage,
    read_codex_models_cache_file, resolve_claude_account_fingerprint,
    resolve_claude_credential_context, resolve_codex_account_fingerprint,
    resolve_cursor_account_fingerprint, resolve_cursor_credential_context, UsageHttpError,
};
use std::sync::Arc;

pub const METADATA_PROBE_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Clone)]
pub enum MetadataProbeError {
    Cancelled,
    TimedOut,
    Unsupported(String),
    Failed(String),
}

impl std::fmt::Display for MetadataProbeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => write!(f, "metadata probe cancelled"),
            Self::TimedOut => write!(f, "metadata probe timed out"),
            Self::Unsupported(msg) => write!(f, "metadata unsupported: {msg}"),
            Self::Failed(msg) => write!(f, "metadata failed: {msg}"),
        }
    }
}

impl std::error::Error for MetadataProbeError {}

impl From<ProviderInteractiveProbeError> for MetadataProbeError {
    fn from(value: ProviderInteractiveProbeError) -> Self {
        match value {
            ProviderInteractiveProbeError::TimedOut => Self::TimedOut,
            ProviderInteractiveProbeError::Cancelled => Self::Cancelled,
            other => Self::Failed(other.to_string()),
        }
    }
}

/// Merge discovered models with custom/favorites/hidden policy for UI wire.
pub fn project_model_catalog_wire(
    instance: &ProviderInstanceConfig,
    discovered: &[DiscoveredModel],
    checked_at_unix_ms: Option<u64>,
    now_ms: u64,
    source: ProviderMetadataSource,
    config_fingerprint: Option<String>,
    account_fingerprint: Option<String>,
    error: Option<String>,
) -> ProviderModelCatalogWire {
    let hidden: std::collections::BTreeSet<&str> = instance
        .model_policy
        .hidden_builtins
        .iter()
        .map(String::as_str)
        .collect();
    let favorites: std::collections::BTreeSet<&str> = instance
        .model_policy
        .favorite_order
        .iter()
        .map(String::as_str)
        .collect();
    let mut by_slug: BTreeMap<String, ProviderModelEntryWire> = BTreeMap::new();
    for model in discovered {
        let is_hidden = hidden.contains(model.slug.as_str()) || model.hidden;
        by_slug.insert(
            model.slug.clone(),
            ProviderModelEntryWire {
                slug: model.slug.clone(),
                display_name: model.display_name.clone(),
                supports_effort: model.supports_effort,
                supported_efforts: model.supported_efforts.clone(),
                default_effort: model.default_effort.clone(),
                hidden: is_hidden,
                is_custom: false,
                is_favorite: favorites.contains(model.slug.as_str()),
                input_modalities: model.input_modalities.clone(),
            },
        );
    }
    for custom in &instance.custom_models {
        let Ok(slug) = normalize_model_slug(&custom.slug) else {
            continue;
        };
        by_slug
            .entry(slug.clone())
            .or_insert(ProviderModelEntryWire {
                slug: slug.clone(),
                display_name: custom.display_name.clone().unwrap_or_else(|| slug.clone()),
                supports_effort: false,
                supported_efforts: Vec::new(),
                default_effort: None,
                hidden: false,
                is_custom: true,
                is_favorite: favorites.contains(slug.as_str()),
                input_modalities: Vec::new(),
            });
        if let Some(row) = by_slug.get_mut(&slug) {
            row.is_custom = true;
            if let Some(name) = &custom.display_name {
                row.display_name = name.clone();
            }
        }
    }
    let mut ordered = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for slug in &instance.model_policy.favorite_order {
        if let Some(row) = by_slug.get(slug) {
            if !row.hidden && seen.insert(slug.clone()) {
                ordered.push(row.clone());
            }
        }
    }
    for slug in &instance.model_policy.catalog_order {
        if let Some(row) = by_slug.get(slug) {
            if !row.hidden && seen.insert(slug.clone()) {
                ordered.push(row.clone());
            }
        }
    }
    for (slug, row) in &by_slug {
        // Settings must retain hidden rows so they can be made visible again.
        // The composer filters `hidden` when building its picker.
        if seen.insert(slug.clone()) {
            ordered.push(row.clone());
        }
    }
    ProviderModelCatalogWire {
        instance_id: instance.instance_id.to_string(),
        driver: instance.driver.as_str().to_string(),
        models: ordered,
        checked_at_unix_ms,
        stale: is_stale(checked_at_unix_ms, now_ms),
        error,
        source,
        config_fingerprint,
        account_fingerprint,
    }
}

pub fn project_usage_wire(
    instance: &ProviderInstanceConfig,
    usage: &CachedUsageSnapshot,
    backoff_until: Option<u64>,
    now_ms: u64,
    source: ProviderMetadataSource,
    config_fingerprint: Option<String>,
    account_fingerprint: Option<String>,
    error: Option<String>,
) -> ProviderUsageWire {
    let state = usage_state_for_cache(usage, backoff_until, now_ms);
    ProviderUsageWire {
        instance_id: instance.instance_id.to_string(),
        driver: instance.driver.as_str().to_string(),
        state,
        windows: usage.windows.clone(),
        checked_at_unix_ms: usage.checked_at_unix_ms,
        error,
        retry_after_unix_ms: backoff_until,
        source,
        config_fingerprint,
        account_fingerprint,
    }
}

/// Refresh one instance models+usage into the profile metadata cache.
///
/// Async path only resolves the executable handle, then runs all synchronous
/// protocol/file/HTTP work on a cancel-owned OS worker thread. Dropping the
/// future sets cancel and joins the worker (children terminate via session Drop).
pub async fn refresh_instance_metadata(
    registry: &ProviderRegistry,
    cache: &ProviderMetadataCache,
    instance: &ProviderInstanceConfig,
    custody_scope: &[u8],
    cancel: &AtomicBool,
) -> Result<(), MetadataProbeError> {
    if cancel.load(Ordering::Acquire) {
        return Err(MetadataProbeError::Cancelled);
    }
    if instance.driver.is_stub() || !instance.enabled {
        return Ok(());
    }
    let resolved = resolve_launch_config(instance, custody_scope, None)
        .map_err(|e| MetadataProbeError::Failed(e.to_string()))?;
    let handle = registry
        .resolve_executable_handle(resolved.provider_kind, &resolved.discovery)
        .await
        .map_err(|e| MetadataProbeError::Failed(e.to_string()))?;
    if cancel.load(Ordering::Acquire) {
        return Err(MetadataProbeError::Cancelled);
    }

    let instance = instance.clone();
    let cache = cache.clone();
    let worker_cancel = Arc::new(AtomicBool::new(false));
    let worker_cancel_thread = Arc::clone(&worker_cancel);
    let (tx, rx) = std::sync::mpsc::channel::<Result<(), MetadataProbeError>>();
    let join = std::thread::Builder::new()
        .name("devmanager-provider-metadata".into())
        .spawn(move || {
            let result = refresh_instance_metadata_sync(
                &handle,
                &instance,
                &resolved,
                &cache,
                &worker_cancel_thread,
            );
            let _ = tx.send(result);
        })
        .map_err(|e| MetadataProbeError::Failed(e.to_string()))?;

    struct JoinGuard {
        cancel: Arc<AtomicBool>,
        join: Option<std::thread::JoinHandle<()>>,
    }
    impl Drop for JoinGuard {
        fn drop(&mut self) {
            self.cancel.store(true, Ordering::Release);
            if let Some(join) = self.join.take() {
                let _ = join.join();
            }
        }
    }
    let mut guard = JoinGuard {
        cancel: Arc::clone(&worker_cancel),
        join: Some(join),
    };

    loop {
        if cancel.load(Ordering::Acquire) {
            worker_cancel.store(true, Ordering::Release);
        }
        match rx.try_recv() {
            Ok(result) => {
                if let Some(join) = guard.join.take() {
                    let _ = join.join();
                }
                drop(guard);
                return result;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                if let Some(join) = guard.join.take() {
                    let _ = join.join();
                }
                drop(guard);
                return Err(MetadataProbeError::Failed(
                    "metadata worker disconnected".into(),
                ));
            }
        }
    }
}

fn refresh_instance_metadata_sync(
    handle: &ProviderExecutableHandle,
    instance: &ProviderInstanceConfig,
    resolved: &ResolvedProviderLaunchConfig,
    cache: &ProviderMetadataCache,
    cancel: &Arc<AtomicBool>,
) -> Result<(), MetadataProbeError> {
    if cancel.load(Ordering::Acquire) {
        return Err(MetadataProbeError::Cancelled);
    }
    let now = unix_now_ms();
    let config_fp = effective_scope_fingerprint(instance, resolved);
    let before_account = resolve_account_for_instance(instance, resolved);
    let before_cred = capture_credential_context(instance, resolved);

    match &before_account {
        Err(UsageHttpError::UnsupportedContext(_)) => {
            let models = discover_models_buffered(handle, instance, resolved, cancel);
            // Re-validate context before any publish.
            if !matches!(
                resolve_account_for_instance(instance, resolved),
                Err(UsageHttpError::UnsupportedContext(_))
            ) {
                let _ = cache.clear_incompatible_account(
                    instance.instance_id.as_str(),
                    &config_fp,
                    None,
                );
                return Err(MetadataProbeError::Failed(
                    "provider context changed during probe".into(),
                ));
            }
            if let Ok(models) = models {
                let _ = cache.upsert_models(
                    instance.instance_id.as_str(),
                    instance.driver.as_str(),
                    &config_fp,
                    None,
                    CachedModelCatalog {
                        models,
                        checked_at_unix_ms: Some(unix_now_ms()),
                    },
                );
            }
            let _ = cache.upsert_usage(
                instance.instance_id.as_str(),
                instance.driver.as_str(),
                &config_fp,
                None,
                CachedUsageSnapshot {
                    state: ProviderUsageStateWire::Unsupported,
                    ..CachedUsageSnapshot::default()
                },
                None,
            );
            return Ok(());
        }
        Err(UsageHttpError::AuthRequired) => {
            let _ =
                cache.clear_incompatible_account(instance.instance_id.as_str(), &config_fp, None);
        }
        Ok(_) | Err(_) => {}
    }

    let account_fp = before_account.as_ref().ok().and_then(|fp| fp.clone());
    let models_buf = discover_models_buffered(handle, instance, resolved, cancel);
    let usage_buf = discover_usage_buffered(
        handle,
        instance,
        resolved,
        cache,
        &config_fp,
        account_fp.clone(),
        now,
        cancel,
    );

    let after_account = resolve_account_for_instance(instance, resolved);
    let after_cred = capture_credential_context(instance, resolved);
    if !account_results_match(&before_account, &after_account) || before_cred != after_cred {
        let _ = cache.clear_incompatible_account(instance.instance_id.as_str(), &config_fp, None);
        return Err(MetadataProbeError::Failed(
            "account/credential context changed during probe".into(),
        ));
    }

    let publish_account = match &after_account {
        Ok(fp) => fp.clone(),
        Err(UsageHttpError::AuthRequired) => None,
        Err(UsageHttpError::UnsupportedContext(_)) => None,
        Err(_) => account_fp,
    };

    let models_err = models_buf.as_ref().err().cloned();
    let usage_err = usage_buf.as_ref().err().cloned();

    // Publish only after the same canonical account identity is confirmed.
    if let Ok(models) = models_buf {
        let _ = cache.upsert_models(
            instance.instance_id.as_str(),
            instance.driver.as_str(),
            &config_fp,
            publish_account.clone(),
            CachedModelCatalog {
                models,
                checked_at_unix_ms: Some(unix_now_ms()),
            },
        );
    }

    if let Ok(Some((usage, backoff))) = usage_buf {
        let _ = cache.upsert_usage(
            instance.instance_id.as_str(),
            instance.driver.as_str(),
            &config_fp,
            publish_account,
            usage,
            backoff,
        );
    }

    match (models_err, usage_err) {
        (None, None) => Ok(()),
        (Some(e), _) | (None, Some(e)) => Err(e),
    }
}

fn account_results_match(
    before: &Result<Option<String>, UsageHttpError>,
    after: &Result<Option<String>, UsageHttpError>,
) -> bool {
    match (before, after) {
        (Ok(a), Ok(b)) => a == b,
        (Err(UsageHttpError::AuthRequired), Err(UsageHttpError::AuthRequired)) => true,
        (
            Err(UsageHttpError::UnsupportedContext(_)),
            Err(UsageHttpError::UnsupportedContext(_)),
        ) => true,
        _ => false,
    }
}

fn capture_credential_context(
    instance: &ProviderInstanceConfig,
    resolved: &ResolvedProviderLaunchConfig,
) -> Option<String> {
    match instance.driver {
        ProviderDriverKind::Claude => resolve_claude_credential_context(resolved).ok(),
        ProviderDriverKind::Cursor => resolve_cursor_credential_context(resolved).ok(),
        ProviderDriverKind::Codex => {
            // Canonical Codex identity is auth.json account_id; re-hash for compare.
            resolve_codex_account_fingerprint(resolved).ok().flatten()
        }
        ProviderDriverKind::Grok | ProviderDriverKind::OpenCode => None,
    }
}

fn discover_models_buffered(
    handle: &ProviderExecutableHandle,
    instance: &ProviderInstanceConfig,
    resolved: &ResolvedProviderLaunchConfig,
    cancel: &Arc<AtomicBool>,
) -> Result<Vec<DiscoveredModel>, MetadataProbeError> {
    if cancel.load(Ordering::Acquire) {
        return Err(MetadataProbeError::Cancelled);
    }
    match instance.driver {
        ProviderDriverKind::Claude => discover_claude_models(handle, &resolved.discovery, cancel),
        ProviderDriverKind::Cursor => discover_cursor_models(handle, &resolved.discovery, cancel),
        ProviderDriverKind::Codex => {
            match discover_codex_models(handle, &resolved.discovery, cancel) {
                Ok(models) => Ok(models),
                Err(error) => {
                    let home = codex_home_for_usage(resolved).or_else(default_codex_home);
                    home.as_ref()
                        .and_then(|path| read_codex_models_cache_file(path))
                        .and_then(|body| parse_codex_models_cache_file(&body).ok())
                        .ok_or(error)
                }
            }
        }
        ProviderDriverKind::Grok | ProviderDriverKind::OpenCode => {
            Err(MetadataProbeError::Unsupported("stub".into()))
        }
    }
}

/// Discover usage without publishing. Returns `Ok(None)` when backoff fence skips the query.
fn discover_usage_buffered(
    handle: &ProviderExecutableHandle,
    instance: &ProviderInstanceConfig,
    resolved: &ResolvedProviderLaunchConfig,
    cache: &ProviderMetadataCache,
    config_fp: &str,
    account_fp: Option<String>,
    now: u64,
    cancel: &Arc<AtomicBool>,
) -> Result<Option<(CachedUsageSnapshot, Option<u64>)>, MetadataProbeError> {
    if cancel.load(Ordering::Acquire) {
        return Err(MetadataProbeError::Cancelled);
    }
    if let Some(entry) = cache.entry(instance.instance_id.as_str(), config_fp) {
        if entry
            .usage_backoff_until_unix_ms
            .is_some_and(|until| until > now)
        {
            return Ok(None);
        }
    }
    match instance.driver {
        ProviderDriverKind::Claude => match query_claude_usage(resolved, now) {
            Ok(outcome) => {
                if let Some(got) = outcome.account_fingerprint.as_ref() {
                    if account_fp.as_ref() != Some(got) {
                        return Err(MetadataProbeError::Failed(
                            "claude usage account mismatch".into(),
                        ));
                    }
                }
                Ok(Some((outcome.usage, None)))
            }
            Err(UsageHttpError::Backoff { retry_after_secs }) => {
                let until = now.saturating_add(retry_after_secs.saturating_mul(1000));
                let prior = cache
                    .entry(instance.instance_id.as_str(), config_fp)
                    .map(|e| e.usage)
                    .unwrap_or_default();
                let mut usage = prior;
                usage.state = ProviderUsageStateWire::Backoff;
                Ok(Some((usage, Some(until))))
            }
            Err(UsageHttpError::AuthRequired) => Ok(Some((
                CachedUsageSnapshot {
                    state: ProviderUsageStateWire::AuthRequired,
                    ..CachedUsageSnapshot::default()
                },
                None,
            ))),
            Err(UsageHttpError::UnsupportedContext(_)) => Ok(Some((
                CachedUsageSnapshot {
                    state: ProviderUsageStateWire::Unsupported,
                    ..CachedUsageSnapshot::default()
                },
                None,
            ))),
            Err(_) => Ok(Some((
                CachedUsageSnapshot {
                    state: ProviderUsageStateWire::Failed,
                    ..CachedUsageSnapshot::default()
                },
                None,
            ))),
        },
        ProviderDriverKind::Cursor => match query_cursor_usage(resolved, now) {
            Ok(outcome) => {
                if let Some(got) = outcome.account_fingerprint.as_ref() {
                    if account_fp.as_ref() != Some(got) {
                        return Err(MetadataProbeError::Failed(
                            "cursor usage account mismatch".into(),
                        ));
                    }
                }
                Ok(Some((outcome.usage, None)))
            }
            Err(UsageHttpError::Backoff { retry_after_secs }) => {
                let until = now.saturating_add(retry_after_secs.saturating_mul(1000));
                let prior = cache
                    .entry(instance.instance_id.as_str(), config_fp)
                    .map(|e| e.usage)
                    .unwrap_or_default();
                let mut usage = prior;
                usage.state = ProviderUsageStateWire::Backoff;
                Ok(Some((usage, Some(until))))
            }
            Err(UsageHttpError::UnsupportedContext(_)) => Ok(Some((
                CachedUsageSnapshot {
                    state: ProviderUsageStateWire::Unsupported,
                    ..CachedUsageSnapshot::default()
                },
                None,
            ))),
            Err(UsageHttpError::AuthRequired) => Ok(Some((
                CachedUsageSnapshot {
                    state: ProviderUsageStateWire::AuthRequired,
                    ..CachedUsageSnapshot::default()
                },
                None,
            ))),
            Err(_) => Ok(Some((
                CachedUsageSnapshot {
                    state: ProviderUsageStateWire::Failed,
                    ..CachedUsageSnapshot::default()
                },
                None,
            ))),
        },
        ProviderDriverKind::Codex => {
            match discover_codex_usage_and_account(handle, &resolved.discovery, cancel) {
                Ok((usage, probe_account)) => {
                    if let (Some(expected), Some(probe)) =
                        (account_fp.as_ref(), probe_account.as_ref())
                    {
                        if expected != probe {
                            return Err(MetadataProbeError::Failed(
                                "codex account changed during probe".into(),
                            ));
                        }
                    }
                    // Re-read auth.json as the canonical after-check (email-only
                    // account/read must not replace auth.json account_id).
                    let reread = resolve_codex_account_fingerprint(resolved).ok().flatten();
                    if reread != account_fp {
                        return Err(MetadataProbeError::Failed(
                            "codex auth.json account changed during probe".into(),
                        ));
                    }
                    Ok(Some((usage, None)))
                }
                Err(_) => Ok(Some((
                    CachedUsageSnapshot {
                        state: ProviderUsageStateWire::Failed,
                        ..CachedUsageSnapshot::default()
                    },
                    None,
                ))),
            }
        }
        ProviderDriverKind::Grok | ProviderDriverKind::OpenCode => Ok(None),
    }
}

fn resolve_account_for_instance(
    instance: &ProviderInstanceConfig,
    resolved: &ResolvedProviderLaunchConfig,
) -> Result<Option<String>, UsageHttpError> {
    if effective_env_has_api_key(&resolved.discovery.child_environment)
        || effective_env_has_api_key(&resolved.environment)
    {
        return Err(UsageHttpError::UnsupportedContext("api key context".into()));
    }
    match instance.driver {
        ProviderDriverKind::Claude => resolve_claude_account_fingerprint(resolved),
        ProviderDriverKind::Cursor => resolve_cursor_account_fingerprint(resolved),
        // Prefer auth.json identity at startup; app-server account/read may refine later.
        ProviderDriverKind::Codex => resolve_codex_account_fingerprint(resolved),
        ProviderDriverKind::Grok | ProviderDriverKind::OpenCode => {
            Err(UsageHttpError::UnsupportedContext("stub".into()))
        }
    }
}

fn default_codex_home() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("CODEX_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(home));
    }
    dirs::home_dir().map(|h| h.join(".codex"))
}

fn runner_for_handle(
    handle: &ProviderExecutableHandle,
) -> Result<WindowsProviderProbeRunner, MetadataProbeError> {
    let file_name = handle
        .canonical_path()
        .file_name()
        .ok_or_else(|| MetadataProbeError::Failed("executable name missing".into()))?
        .to_string_lossy()
        .into_owned();
    let policy = ProviderExecutablePolicy::new([file_name])
        .map_err(|e| MetadataProbeError::Failed(e.to_string()))?;
    Ok(WindowsProviderProbeRunner::new(policy))
}

fn spawn_session(
    handle: &ProviderExecutableHandle,
    arguments: &[String],
    discovery: &ProviderDiscoveryConfig,
    cancel: &Arc<AtomicBool>,
    env_overlay: BTreeMap<std::ffi::OsString, std::ffi::OsString>,
) -> Result<ProviderInteractiveSession, MetadataProbeError> {
    if cancel.load(Ordering::Acquire) {
        return Err(MetadataProbeError::Cancelled);
    }
    let runner = runner_for_handle(handle)?;
    let mut env = if discovery.child_environment.is_empty() {
        materialize_provider_environment(BTreeMap::new())
    } else {
        discovery.child_environment.clone()
    };
    for (key, value) in env_overlay {
        env.insert(key, value);
    }
    env.remove(&std::ffi::OsString::from("CLAUDECODE"));
    env.remove(&std::ffi::OsString::from("CLAUDECODE".to_ascii_lowercase()));
    runner
        .spawn_interactive_with_cancel(
            handle.clone(),
            arguments,
            env,
            METADATA_PROBE_TIMEOUT.min(MAX_PROVIDER_PROBE_TIMEOUT),
            MAX_PROVIDER_PROBE_OUTPUT_BYTES,
            Some(Arc::clone(cancel)),
        )
        .map_err(MetadataProbeError::from)
}

fn discover_claude_models(
    handle: &ProviderExecutableHandle,
    discovery: &ProviderDiscoveryConfig,
    cancel: &Arc<AtomicBool>,
) -> Result<Vec<DiscoveredModel>, MetadataProbeError> {
    let args = claude_metadata_args();
    let mut overlay = BTreeMap::new();
    overlay.insert(
        std::ffi::OsString::from("ENABLE_CLAUDEAI_MCP_SERVERS"),
        std::ffi::OsString::from("false"),
    );
    let mut session = spawn_session(handle, &args, discovery, cancel, overlay)?;
    let request_id = format!("metadata-{}", unix_now_ms());
    let payload = json!({
        "type": "control_request",
        "request_id": request_id,
        "request": { "subtype": "initialize" }
    });
    session.write_line(&payload.to_string())?;
    let lines = session.read_until(|line| {
        if let Ok(value) = serde_json::from_str::<JsonValue>(line) {
            value.get("type").and_then(|v| v.as_str()) == Some("control_response")
                || value.pointer("/response/response/models").is_some()
                || value.pointer("/response/models").is_some()
        } else {
            false
        }
    })?;
    let _ = session.terminate();
    let body = lines.last().cloned().unwrap_or_default();
    parse_claude_initialize_models(&body).map_err(MetadataProbeError::Failed)
}

fn claude_metadata_args() -> Vec<String> {
    vec![
        "--print".into(),
        "--input-format".into(),
        "stream-json".into(),
        "--output-format".into(),
        "stream-json".into(),
        "--verbose".into(),
        "--no-session-persistence".into(),
        "--settings".into(),
        r#"{"disableAllHooks":true}"#.into(),
        "--strict-mcp-config".into(),
        "--mcp-config".into(),
        r#"{"mcpServers":{}}"#.into(),
    ]
}

fn discover_codex_models(
    handle: &ProviderExecutableHandle,
    discovery: &ProviderDiscoveryConfig,
    cancel: &Arc<AtomicBool>,
) -> Result<Vec<DiscoveredModel>, MetadataProbeError> {
    let mut session = spawn_session(
        handle,
        &["app-server".into()],
        discovery,
        cancel,
        BTreeMap::new(),
    )?;
    session.write_line(
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "clientInfo": { "name": "devmanager", "version": env!("CARGO_PKG_VERSION") },
                "capabilities": { "experimentalApi": true }
            }
        })
        .to_string(),
    )?;
    let _ = session.read_until(|line| jsonrpc_id_is(line, 1))?;
    session.write_line(
        &json!({
            "jsonrpc": "2.0",
            "method": "initialized",
            "params": {}
        })
        .to_string(),
    )?;
    let mut models = Vec::new();
    let mut cursor: Option<String> = None;
    let mut list_id = 2_u64;
    let mut pages = 0_usize;
    loop {
        if cancel.load(Ordering::Acquire) {
            let _ = session.terminate();
            return Err(MetadataProbeError::Cancelled);
        }
        if pages >= 64 || models.len() >= MAX_METADATA_MODELS {
            break;
        }
        let mut params = json!({ "limit": 100 });
        if let Some(next) = &cursor {
            // Codex ModelListParams uses `cursor` (response returns `nextCursor`).
            params["cursor"] = JsonValue::String(next.clone());
        }
        session.write_line(
            &json!({
                "jsonrpc": "2.0",
                "id": list_id,
                "method": "model/list",
                "params": params
            })
            .to_string(),
        )?;
        let lines = session.read_until(|line| jsonrpc_id_is(line, list_id))?;
        let body = lines.last().cloned().unwrap_or_default();
        let value: JsonValue =
            serde_json::from_str(&body).map_err(|e| MetadataProbeError::Failed(e.to_string()))?;
        let result = value
            .get("result")
            .ok_or_else(|| MetadataProbeError::Failed("codex model/list missing result".into()))?;
        let (page, next) = parse_codex_model_list(result).map_err(MetadataProbeError::Failed)?;
        if page.is_empty() && next.is_some() && next == cursor {
            // Guard against servers ignoring an unrecognized pagination field
            // and repeating the first page forever.
            break;
        }
        let before_len = models.len();
        models.extend(page);
        pages += 1;
        if models.len() == before_len && next.is_some() && next == cursor {
            break;
        }
        cursor = next;
        list_id += 1;
        if cursor.is_none() {
            break;
        }
    }
    let _ = session.terminate();
    Ok(models)
}

fn discover_codex_usage_and_account(
    handle: &ProviderExecutableHandle,
    discovery: &ProviderDiscoveryConfig,
    cancel: &Arc<AtomicBool>,
) -> Result<(CachedUsageSnapshot, Option<String>), MetadataProbeError> {
    let mut session = spawn_session(
        handle,
        &["app-server".into()],
        discovery,
        cancel,
        BTreeMap::new(),
    )?;
    session.write_line(
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "clientInfo": { "name": "devmanager", "version": env!("CARGO_PKG_VERSION") },
                "capabilities": { "experimentalApi": true }
            }
        })
        .to_string(),
    )?;
    let _ = session.read_until(|line| jsonrpc_id_is(line, 1))?;
    session.write_line(
        &json!({
            "jsonrpc": "2.0",
            "method": "initialized",
            "params": {}
        })
        .to_string(),
    )?;
    let mut account = None;
    session.write_line(
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "account/read",
            "params": {}
        })
        .to_string(),
    )?;
    if let Ok(lines) = session.read_until(|line| jsonrpc_id_is(line, 2)) {
        if let Some(body) = lines.last() {
            if let Ok(value) = serde_json::from_str::<JsonValue>(body) {
                if let Some(result) = value.get("result") {
                    if let Some(material) = codex_account_id_from_account_read(result) {
                        account = Some(fingerprint_account_material(&material));
                    }
                }
            }
        }
    }
    session.write_line(
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "account/rateLimits/read",
            "params": {}
        })
        .to_string(),
    )?;
    let lines = session.read_until(|line| jsonrpc_id_is(line, 3))?;
    let _ = session.terminate();
    let body = lines.last().cloned().unwrap_or_default();
    let value: JsonValue =
        serde_json::from_str(&body).map_err(|e| MetadataProbeError::Failed(e.to_string()))?;
    let result = value.get("result").unwrap_or(&value);
    let mut usage = parse_codex_rate_limits(result).map_err(MetadataProbeError::Failed)?;
    usage.checked_at_unix_ms = Some(unix_now_ms());
    Ok((usage, account))
}

fn discover_cursor_models(
    handle: &ProviderExecutableHandle,
    discovery: &ProviderDiscoveryConfig,
    cancel: &Arc<AtomicBool>,
) -> Result<Vec<DiscoveredModel>, MetadataProbeError> {
    let mut session = spawn_session(handle, &["acp".into()], discovery, cancel, BTreeMap::new())?;
    session.write_line(
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": 1,
                "clientInfo": { "name": "devmanager", "version": env!("CARGO_PKG_VERSION") },
                "clientCapabilities": {
                    "_meta": { "parameterizedModelPicker": true }
                }
            }
        })
        .to_string(),
    )?;
    let _ = session.read_until(|line| jsonrpc_id_is(line, 1))?;
    session.write_line(
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "cursor/list_available_models",
            "params": {}
        })
        .to_string(),
    )?;
    let lines = session.read_until(|line| jsonrpc_id_is(line, 2))?;
    let _ = session.terminate();
    let body = lines.last().cloned().unwrap_or_default();
    let value: JsonValue =
        serde_json::from_str(&body).map_err(|e| MetadataProbeError::Failed(e.to_string()))?;
    let result = value
        .get("result")
        .ok_or_else(|| MetadataProbeError::Failed("cursor models missing result".into()))?;
    parse_cursor_list_available_models(result).map_err(MetadataProbeError::Failed)
}

fn jsonrpc_id_is(line: &str, id: u64) -> bool {
    serde_json::from_str::<JsonValue>(line)
        .ok()
        .and_then(|v| v.get("id").and_then(|id_v| id_v.as_u64()))
        == Some(id)
}

/// Project model catalogs + usage from the host-owned unredacted settings store.
///
/// Callers must pass [`ProviderSettingsStore`] (not a redacted snapshot) so
/// `custody_scope_for_instance` can decrypt protected env for scope fingerprints.
/// This path is read-only w.r.t. the metadata cache (no prune / clear).
pub fn project_all_from_cache(
    settings: &ProviderSettingsStore,
    cache: &ProviderMetadataCache,
    now_ms: u64,
) -> (Vec<ProviderModelCatalogWire>, Vec<ProviderUsageWire>) {
    let document = settings.snapshot();
    let mut catalogs = Vec::new();
    let mut usages = Vec::new();
    for instance in &document.instances {
        if instance.driver.is_stub() {
            catalogs.push(ProviderModelCatalogWire::empty(
                instance.instance_id.as_str(),
                instance.driver.as_str(),
            ));
            usages.push(ProviderUsageWire::unsupported(
                instance.instance_id.as_str(),
                instance.driver.as_str(),
            ));
            continue;
        }
        let custody = settings.custody_scope_for_instance(instance.instance_id.as_str());
        let Ok(resolved) = resolve_launch_config(instance, &custody, None) else {
            catalogs.push(ProviderModelCatalogWire::empty(
                instance.instance_id.as_str(),
                instance.driver.as_str(),
            ));
            usages.push(ProviderUsageWire::empty(
                instance.instance_id.as_str(),
                instance.driver.as_str(),
            ));
            continue;
        };
        let config_fp = effective_scope_fingerprint(instance, &resolved);
        let entry = cache.entry(instance.instance_id.as_str(), &config_fp);

        match resolve_account_for_instance(instance, &resolved) {
            Err(UsageHttpError::UnsupportedContext(_)) => {
                // Custom/API/endpoint contexts: models usable without OAuth account;
                // quota stays isolated as Unsupported. Always merge custom models.
                let discovered = entry
                    .as_ref()
                    .map(|e| e.models.models.as_slice())
                    .unwrap_or(&[]);
                let catalog = project_model_catalog_wire(
                    instance,
                    discovered,
                    entry.as_ref().and_then(|e| e.models.checked_at_unix_ms),
                    now_ms,
                    if discovered.is_empty() {
                        ProviderMetadataSource::Empty
                    } else {
                        ProviderMetadataSource::LastGood
                    },
                    Some(config_fp.clone()),
                    None,
                    None,
                );
                catalogs.push(catalog);
                usages.push(ProviderUsageWire::unsupported(
                    instance.instance_id.as_str(),
                    instance.driver.as_str(),
                ));
            }
            Ok(None) | Err(UsageHttpError::AuthRequired) => {
                catalogs.push(ProviderModelCatalogWire::empty(
                    instance.instance_id.as_str(),
                    instance.driver.as_str(),
                ));
                let mut usage = ProviderUsageWire::empty(
                    instance.instance_id.as_str(),
                    instance.driver.as_str(),
                );
                usage.state = ProviderUsageStateWire::AuthRequired;
                usages.push(usage);
            }
            Ok(Some(verified_account)) => {
                let (catalog, usage) = match entry {
                    Some(entry)
                        if entry.account_fingerprint.as_deref()
                            == Some(verified_account.as_str()) =>
                    {
                        let catalog = project_model_catalog_wire(
                            instance,
                            &entry.models.models,
                            entry.models.checked_at_unix_ms,
                            now_ms,
                            if entry.models.models.is_empty() {
                                ProviderMetadataSource::Empty
                            } else {
                                ProviderMetadataSource::LastGood
                            },
                            Some(entry.config_fingerprint.clone()),
                            entry.account_fingerprint.clone(),
                            None,
                        );
                        let usage = project_usage_wire(
                            instance,
                            &entry.usage,
                            entry.usage_backoff_until_unix_ms,
                            now_ms,
                            if entry.usage.windows.is_empty() {
                                ProviderMetadataSource::Empty
                            } else {
                                ProviderMetadataSource::LastGood
                            },
                            Some(entry.config_fingerprint.clone()),
                            entry.account_fingerprint.clone(),
                            None,
                        );
                        (catalog, usage)
                    }
                    _ => (
                        ProviderModelCatalogWire::empty(
                            instance.instance_id.as_str(),
                            instance.driver.as_str(),
                        ),
                        ProviderUsageWire::empty(
                            instance.instance_id.as_str(),
                            instance.driver.as_str(),
                        ),
                    ),
                };
                catalogs.push(catalog);
                usages.push(usage);
            }
            Err(_) => {
                catalogs.push(ProviderModelCatalogWire::empty(
                    instance.instance_id.as_str(),
                    instance.driver.as_str(),
                ));
                usages.push(ProviderUsageWire::empty(
                    instance.instance_id.as_str(),
                    instance.driver.as_str(),
                ));
            }
        }
    }
    (catalogs, usages)
}

/// Prune obsolete cache rows for the live settings document. Call from refresh
/// completion or config mutation — never from snapshot/read projection.
pub fn prune_metadata_cache_for_settings(
    settings: &ProviderSettingsStore,
    cache: &ProviderMetadataCache,
) {
    let document = settings.snapshot();
    let mut live_keys = Vec::new();
    for instance in &document.instances {
        if instance.driver.is_stub() {
            continue;
        }
        let custody = settings.custody_scope_for_instance(instance.instance_id.as_str());
        if let Ok(resolved) = resolve_launch_config(instance, &custody, None) {
            live_keys.push((
                instance.instance_id.to_string(),
                effective_scope_fingerprint(instance, &resolved),
            ));
        }
    }
    let _ = cache.prune_to_instances(&live_keys);
}
