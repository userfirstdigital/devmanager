//! Host-owned cancellation-owned provider health refresh futures.

use std::pin::Pin;
use std::sync::Arc;

use futures_util::future::Future;

use crate::providers::adapter::{
    ProviderProbeKind, ProviderProbeRequest, ProviderProbeRunner, ProviderProbeStatus,
    WindowsProviderProbeRunner,
};
use crate::providers::capabilities::ProviderExecutablePolicy;
use crate::providers::settings::{
    apply_cursor_about_to_row, parse_cursor_about_plain_bytes, parse_cursor_about_strict_json,
    resolve_launch_config, unix_now_ms, ProviderDriverKind, ProviderHealthRow,
    ProviderHealthStatus, ProviderSettingsAuthority, DEFAULT_HEALTH_INTERVAL_SECS,
};
use crate::providers::ProviderKind;
use crate::services::ProcessManager;

pub(crate) type ProviderHealthFuture =
    Pin<Box<dyn Future<Output = ProviderHealthJobOutcome> + Send>>;

#[derive(Debug)]
pub(crate) struct ProviderHealthJobOutcome {
    pub generation: u64,
    pub config_revision: u64,
    pub error: Option<String>,
}

/// RAII: dropping the future (executor cancel) finishes the refresh guard.
struct HealthJobDropGuard {
    authority: Arc<ProviderSettingsAuthority>,
    generation: u64,
    finished: bool,
}

impl HealthJobDropGuard {
    fn finish(mut self, error: Option<String>) {
        self.authority
            .health_job()
            .finish_refresh(self.generation, error);
        self.finished = true;
    }
}

impl Drop for HealthJobDropGuard {
    fn drop(&mut self) {
        if !self.finished {
            self.authority
                .health_job()
                .finish_refresh(self.generation, Some("cancelled".into()));
        }
    }
}

/// Schedule a bounded refresh when interval allows or `force` is set.
pub(crate) fn try_begin_health_job(
    authority: &Arc<ProviderSettingsAuthority>,
    manager: Option<ProcessManager>,
    force: bool,
) -> Option<(u64, ProviderHealthFuture)> {
    if !force && !authority.health_job().should_schedule() {
        return None;
    }
    let generation = authority.health_job().try_begin_manual_refresh()?;
    let config_revision = authority.profile().settings.snapshot().revision;
    let authority = Arc::clone(authority);
    // Construct the DropGuard before boxing the future so an unpolled drop
    // still releases the active refresh guard.
    let guard = HealthJobDropGuard {
        authority: Arc::clone(&authority),
        generation,
        finished: false,
    };
    let future: ProviderHealthFuture = Box::pin(async move {
        let error = run_health_probes(&authority, manager.as_ref(), generation, config_revision)
            .await
            .err();
        if authority.health_job().is_stale_generation(generation)
            || authority.health_job().is_stale_config(config_revision)
        {
            guard.finish(None);
        } else {
            guard.finish(error.clone());
        }
        ProviderHealthJobOutcome {
            generation,
            config_revision,
            error,
        }
    });
    Some((generation, future))
}

async fn run_health_probes(
    authority: &ProviderSettingsAuthority,
    manager: Option<&ProcessManager>,
    generation: u64,
    config_revision: u64,
) -> Result<(), String> {
    if authority.health_job().is_stale_generation(generation)
        || authority.health_job().is_stale_config(config_revision)
    {
        return Ok(());
    }
    let document = authority.profile().settings.snapshot();
    if manager.is_none() {
        return Err("provider runtime unavailable".into());
    }
    // Health has its own adapter attestations. Re-probing must never quarantine
    // a launch surface retained by an active task or an exact-resume handshake.
    let health_registry = crate::providers::startup::stock_provider_registry()
        .map_err(|_| "health registry unavailable".to_string())?;
    for instance in &document.instances {
        if authority.health_job().is_stale_generation(generation)
            || authority.health_job().is_stale_config(config_revision)
        {
            break;
        }
        if instance.driver.is_stub() || !instance.enabled {
            continue;
        }
        let Some(kind) = instance.driver.to_provider_kind() else {
            continue;
        };
        let scope = authority
            .profile()
            .settings
            .custody_scope_for_instance(instance.instance_id.as_str());
        let resolved = match resolve_launch_config(instance, &scope, None) {
            Ok(resolved) => resolved,
            Err(_) => {
                publish_error(
                    authority,
                    instance.instance_id.as_str(),
                    instance.driver,
                    "launch config unavailable".into(),
                );
                continue;
            }
        };
        let outcome = probe_one_instance(
            &health_registry,
            kind,
            instance.instance_id.as_str(),
            instance.driver,
            &resolved.discovery,
        )
        .await;
        if authority.health_job().is_stale_generation(generation)
            || authority.health_job().is_stale_config(config_revision)
        {
            break;
        }
        match outcome {
            Ok(row) => authority.profile().health.upsert_row(row),
            Err(message) => publish_error(
                authority,
                instance.instance_id.as_str(),
                instance.driver,
                message,
            ),
        }
    }
    Ok(())
}

fn publish_error(
    authority: &ProviderSettingsAuthority,
    instance_id: &str,
    driver: ProviderDriverKind,
    message: String,
) {
    // Never carry a prior account/version as current while this check failed.
    let mut row = ProviderHealthRow::unknown(instance_id, driver);
    row.status = ProviderHealthStatus::Unavailable;
    row.error = Some(message);
    row.checked_at_unix_ms = Some(unix_now_ms());
    authority.profile().health.upsert_row(row);
}

async fn probe_one_instance(
    registry: &crate::providers::registry::ProviderRegistry,
    kind: ProviderKind,
    instance_id: &str,
    driver: ProviderDriverKind,
    discovery: &crate::providers::registry::ProviderDiscoveryConfig,
) -> Result<ProviderHealthRow, String> {
    match kind {
        ProviderKind::Cursor => probe_cursor_about(registry, instance_id, driver, discovery).await,
        ProviderKind::ClaudeCode | ProviderKind::Codex => {
            probe_via_registry(registry, kind, instance_id, driver, discovery).await
        }
    }
}

async fn probe_via_registry(
    registry: &crate::providers::registry::ProviderRegistry,
    kind: ProviderKind,
    instance_id: &str,
    driver: ProviderDriverKind,
    discovery: &crate::providers::registry::ProviderDiscoveryConfig,
) -> Result<ProviderHealthRow, String> {
    match crate::host::agent_connection::observe_with_trusted_auth(registry, kind, discovery).await
    {
        Ok(observation) => {
            let version = Some(observation.version().as_str().to_string());
            let auth = observation.capabilities().auth_state;
            let mut row = ProviderHealthRow::unknown(instance_id, driver);
            row.version = version;
            row.checked_at_unix_ms = Some(unix_now_ms());
            row.status = match auth {
                crate::providers::ProviderAuthState::AuthenticatedSubscription => {
                    ProviderHealthStatus::Healthy
                }
                crate::providers::ProviderAuthState::AuthRequired => {
                    row.error = Some("Not signed in".into());
                    ProviderHealthStatus::Degraded
                }
                crate::providers::ProviderAuthState::Unknown => ProviderHealthStatus::Unknown,
            };
            Ok(row)
        }
        Err(_) => Err("health probe failed".into()),
    }
}

fn stderr_indicates_unsupported_format(stderr: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(stderr) else {
        return false;
    };
    let lower = text.to_ascii_lowercase();
    lower.contains("unsupported") && lower.contains("format")
        || lower.contains("unknown option") && lower.contains("format")
        || lower.contains("unrecognized") && lower.contains("format")
}

fn cursor_health_executable_policy(
    resolved_file_name: &str,
) -> Result<ProviderExecutablePolicy, String> {
    // Resolved CLI may be `agent` / `cursor-agent` / `agent.exe`; allowlist must
    // include the exact resolved name without duplicate-entrypoint failure.
    let mut names = vec![resolved_file_name.to_string()];
    for candidate in ["cursor-agent", "agent"] {
        let duplicate = names.iter().any(|existing| {
            existing.eq_ignore_ascii_case(candidate)
                || existing
                    .strip_suffix(".exe")
                    .or_else(|| existing.strip_suffix(".EXE"))
                    .is_some_and(|stem| stem.eq_ignore_ascii_case(candidate))
        });
        if !duplicate {
            names.push(candidate.to_string());
        }
    }
    ProviderExecutablePolicy::new(names).map_err(|_| "cursor executable policy unavailable".into())
}

/// Decide whether Cursor about stdout may be treated as authenticated health.
/// NonZeroExit is never healthy, even when stdout claims a positive userEmail.
pub(crate) fn cursor_about_health_from_probe_status(
    status: ProviderProbeStatus,
    stderr: &[u8],
) -> Result<CursorAboutFallback, String> {
    match status {
        ProviderProbeStatus::Completed => Ok(CursorAboutFallback::UseJson),
        ProviderProbeStatus::NonZeroExit if stderr_indicates_unsupported_format(stderr) => {
            Ok(CursorAboutFallback::UsePlain)
        }
        ProviderProbeStatus::NonZeroExit => Err("cursor about exited non-zero".into()),
        _ => Err("cursor about probe did not complete".into()),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CursorAboutFallback {
    UseJson,
    UsePlain,
}

async fn probe_cursor_about(
    registry: &crate::providers::registry::ProviderRegistry,
    instance_id: &str,
    driver: ProviderDriverKind,
    discovery: &crate::providers::registry::ProviderDiscoveryConfig,
) -> Result<ProviderHealthRow, String> {
    let handle = registry
        .resolve_executable_handle(ProviderKind::Cursor, discovery)
        .await
        .map_err(|_| "cursor executable unavailable".to_string())?;
    let file_name = handle
        .canonical_path()
        .file_name()
        .ok_or_else(|| "cursor executable name unavailable".to_string())?
        .to_string_lossy()
        .into_owned();
    let policy = cursor_health_executable_policy(&file_name)?;
    let runner = WindowsProviderProbeRunner::new(policy);
    let json_request =
        ProviderProbeRequest::new(handle.clone(), ProviderProbeKind::CursorAboutJson)
            .map_err(|_| "cursor about request invalid".to_string())?
            .with_child_environment(discovery.child_environment.clone())
            .with_scope_fingerprint(
                discovery
                    .instance_scope
                    .as_ref()
                    .map(|scope| scope.as_cache_key()),
            );
    let json_result = runner
        .run(json_request)
        .await
        .map_err(|_| "cursor about probe failed".to_string())?;
    let fallback =
        cursor_about_health_from_probe_status(json_result.status(), json_result.stderr())?;
    let (result, used_plain) = match fallback {
        CursorAboutFallback::UseJson => (json_result, false),
        CursorAboutFallback::UsePlain => {
            let plain = ProviderProbeRequest::new(handle, ProviderProbeKind::CursorAboutPlain)
                .map_err(|_| "cursor about plain request invalid".to_string())?
                .with_child_environment(discovery.child_environment.clone())
                .with_scope_fingerprint(
                    discovery
                        .instance_scope
                        .as_ref()
                        .map(|scope| scope.as_cache_key()),
                );
            let plain_result = runner
                .run(plain)
                .await
                .map_err(|_| "cursor about plain probe failed".to_string())?;
            if !matches!(plain_result.status(), ProviderProbeStatus::Completed) {
                return Err("cursor about plain probe did not complete".into());
            }
            (plain_result, true)
        }
    };
    let facts = if used_plain {
        parse_cursor_about_plain_bytes(result.stdout())
    } else {
        parse_cursor_about_strict_json(result.stdout())
    };
    let mut row = ProviderHealthRow::unknown(instance_id, driver);
    apply_cursor_about_to_row(&mut row, &facts);
    Ok(row)
}

pub(crate) fn default_health_interval_secs() -> u64 {
    DEFAULT_HEALTH_INTERVAL_SECS
}
