//! Cached provider health projection with bounded background refresh ownership.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::providers::settings::model::{
    ProviderDriverKind, ProviderSettingsDocument, DEFAULT_HEALTH_INTERVAL_SECS,
};

pub use crate::providers::settings::model::DEFAULT_HEALTH_INTERVAL_SECS as HEALTH_INTERVAL_DEFAULT;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderHealthStatus {
    Unknown,
    Checking,
    Healthy,
    Degraded,
    Unavailable,
    StubUnsupported,
}

impl ProviderHealthStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Checking => "checking",
            Self::Healthy => "healthy",
            Self::Degraded => "degraded",
            Self::Unavailable => "unavailable",
            Self::StubUnsupported => "stub_unsupported",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderHealthRow {
    pub instance_id: String,
    pub driver: ProviderDriverKind,
    pub status: ProviderHealthStatus,
    pub version: Option<String>,
    pub account_email_masked: Option<String>,
    pub account_email: Option<String>,
    pub subscription_tier: Option<String>,
    pub checked_at_unix_ms: Option<u64>,
    pub error: Option<String>,
    pub reveal_email: bool,
}

impl ProviderHealthRow {
    pub fn stub(instance_id: &str, driver: ProviderDriverKind) -> Self {
        Self {
            instance_id: instance_id.to_string(),
            driver,
            status: ProviderHealthStatus::StubUnsupported,
            version: None,
            account_email_masked: None,
            account_email: None,
            subscription_tier: None,
            checked_at_unix_ms: None,
            error: Some("Provider is not supported in native DevManager yet".into()),
            reveal_email: false,
        }
    }

    pub fn unknown(instance_id: &str, driver: ProviderDriverKind) -> Self {
        Self {
            instance_id: instance_id.to_string(),
            driver,
            status: ProviderHealthStatus::Unknown,
            version: None,
            account_email_masked: None,
            account_email: None,
            subscription_tier: None,
            checked_at_unix_ms: None,
            error: None,
            reveal_email: false,
        }
    }

    pub fn mask_email(email: &str) -> String {
        let Some((user, domain)) = email.split_once('@') else {
            return "***".into();
        };
        if user.is_empty() {
            return format!("***@{domain}");
        }
        let first = user.chars().next().unwrap_or('*');
        format!("{first}***@{domain}")
    }
}

#[derive(Debug, Clone)]
pub struct ProviderHealthCache {
    inner: Arc<Mutex<HealthCacheState>>,
}

#[derive(Debug, Default)]
struct HealthCacheState {
    rows: BTreeMap<String, ProviderHealthRow>,
    scopes: BTreeMap<String, (String, bool)>,
    last_refresh_started: Option<Instant>,
    last_refresh_finished: Option<Instant>,
    last_error: Option<String>,
    refresh_in_flight: bool,
    generation: u64,
}

/// RAII in-flight guard — dropping clears the in-flight bit for the generation
/// when finish was not already called.
pub struct ProviderHealthRefreshGuard {
    cache: ProviderHealthCache,
    generation: u64,
    finished: bool,
}

impl ProviderHealthRefreshGuard {
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn finish(mut self, error: Option<String>) {
        self.cache.finish_refresh(self.generation, error);
        self.finished = true;
    }
}

impl Drop for ProviderHealthRefreshGuard {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        self.cache
            .finish_refresh(self.generation, Some("cancelled".into()));
    }
}

impl ProviderHealthCache {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(HealthCacheState::default())),
        }
    }

    pub fn seed_from_document(&self, doc: &ProviderSettingsDocument) {
        let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        state.rows.retain(|id, _| doc.get(id).is_some());
        state.scopes.retain(|id, _| doc.get(id).is_some());
        for instance in &doc.instances {
            let id = instance.instance_id.as_str().to_string();
            let scope = (instance.launch_identity_fingerprint(), instance.enabled);
            if state.scopes.get(&id) == Some(&scope) && state.rows.contains_key(&id) {
                continue;
            }
            state.scopes.insert(id.clone(), scope);
            let row = if instance.driver.is_stub() {
                ProviderHealthRow::stub(&id, instance.driver)
            } else {
                ProviderHealthRow::unknown(&id, instance.driver)
            };
            state.rows.insert(id, row);
        }
    }

    pub fn snapshot_rows(&self) -> Vec<ProviderHealthRow> {
        let state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        state.rows.values().cloned().collect()
    }

    pub fn last_error(&self) -> Option<String> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .last_error
            .clone()
    }

    pub fn is_refresh_in_flight(&self) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .refresh_in_flight
    }

    /// Returns a generation when a new refresh may start. Concurrent reentry is refused.
    /// Prefer [`Self::try_begin_refresh_guard`] when a drop/cancel owner is available.
    pub fn try_begin_refresh(&self) -> Option<u64> {
        self.try_begin_refresh_guard().map(|guard| {
            let generation = guard.generation();
            // Generation-only callers must finish_refresh; the guard would otherwise
            // clear in-flight on drop. Forget is confined to this API seam so the
            // job owner path can retain a real guard instead.
            std::mem::forget(guard);
            Some(generation)
        })?
    }

    /// Returns an RAII guard when a new refresh may start. Concurrent reentry is refused.
    pub fn try_begin_refresh_guard(&self) -> Option<ProviderHealthRefreshGuard> {
        let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if state.refresh_in_flight {
            return None;
        }
        state.refresh_in_flight = true;
        state.last_refresh_started = Some(Instant::now());
        state.last_error = None;
        state.generation = state.generation.saturating_add(1);
        let generation = state.generation;
        let enabled: Vec<String> = state
            .scopes
            .iter()
            .filter(|(_, (_, enabled))| *enabled)
            .map(|(id, _)| id.clone())
            .collect();
        for id in enabled {
            if let Some(row) = state.rows.get_mut(&id) {
                if row.status == ProviderHealthStatus::StubUnsupported {
                    continue;
                }
                row.status = ProviderHealthStatus::Checking;
            }
        }
        Some(ProviderHealthRefreshGuard {
            cache: self.clone(),
            generation,
            finished: false,
        })
    }

    pub fn upsert_row(&self, row: ProviderHealthRow) {
        let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        state.rows.insert(row.instance_id.clone(), row);
    }

    pub fn set_refresh_error(&self, generation: u64, error: impl Into<String>) {
        let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if state.generation != generation {
            return;
        }
        state.last_error = Some(error.into());
    }

    pub fn finish_refresh(&self, generation: u64, error: Option<String>) {
        let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if state.generation != generation {
            return;
        }
        if let Some(error) = error.as_ref() {
            state.last_error = Some(error.clone());
        }
        for row in state.rows.values_mut() {
            if row.status == ProviderHealthStatus::Checking {
                row.status = ProviderHealthStatus::Unavailable;
                row.error = Some(
                    error
                        .clone()
                        .unwrap_or_else(|| "Health check did not complete".into()),
                );
            }
        }
        state.refresh_in_flight = false;
        state.last_refresh_finished = Some(Instant::now());
    }

    pub fn set_email_reveal(&self, instance_id: &str, reveal: bool) {
        let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(row) = state.rows.get_mut(instance_id) {
            row.reveal_email = reveal;
        }
    }

    /// Whether a scheduled refresh should run given interval_secs (0 = never scheduled).
    pub fn should_schedule_refresh(&self, interval_secs: u64) -> bool {
        if interval_secs == 0 {
            return false;
        }
        let state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if state.refresh_in_flight {
            return false;
        }
        match state.last_refresh_finished.or(state.last_refresh_started) {
            None => true,
            Some(at) => at.elapsed() >= Duration::from_secs(interval_secs),
        }
    }
}

impl Default for ProviderHealthCache {
    fn default() -> Self {
        Self::new()
    }
}

pub fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn apply_probe_outcome(
    row: &mut ProviderHealthRow,
    version: Option<String>,
    email: Option<String>,
    subscription: Option<String>,
    ok: bool,
    error: Option<String>,
) {
    row.checked_at_unix_ms = Some(unix_now_ms());
    row.version = version;
    if let Some(email) = email {
        row.account_email_masked = Some(ProviderHealthRow::mask_email(&email));
        row.account_email = Some(email);
    } else {
        // Honest unknown — do not invent account metadata.
        row.account_email = None;
        row.account_email_masked = None;
    }
    row.subscription_tier = subscription;
    row.error = error;
    row.status = if ok {
        ProviderHealthStatus::Healthy
    } else if row.error.is_some() {
        ProviderHealthStatus::Unavailable
    } else {
        ProviderHealthStatus::Degraded
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_and_stub_rows_never_enter_checking() {
        let cache = ProviderHealthCache::new();
        let mut doc = ProviderSettingsDocument::with_builtins();
        doc.get_mut("claude").unwrap().enabled = false;
        cache.seed_from_document(&doc);
        let guard = cache.try_begin_refresh_guard().unwrap();
        let rows = cache.snapshot_rows();
        assert_eq!(
            rows.iter()
                .find(|r| r.instance_id == "claude")
                .unwrap()
                .status,
            ProviderHealthStatus::Unknown
        );
        for id in ["grok", "opencode"] {
            assert_eq!(
                rows.iter().find(|r| r.instance_id == id).unwrap().status,
                ProviderHealthStatus::StubUnsupported
            );
        }
        assert_eq!(
            rows.iter()
                .find(|r| r.instance_id == "codex")
                .unwrap()
                .status,
            ProviderHealthStatus::Checking
        );
        drop(guard);
        assert!(cache
            .snapshot_rows()
            .iter()
            .all(|r| r.status != ProviderHealthStatus::Checking));
    }

    #[test]
    fn changed_account_scope_cannot_keep_old_health_metadata() {
        let cache = ProviderHealthCache::new();
        let mut doc = ProviderSettingsDocument::with_builtins();
        cache.seed_from_document(&doc);
        let mut row = ProviderHealthRow::unknown("claude", ProviderDriverKind::Claude);
        apply_probe_outcome(
            &mut row,
            Some("1.0".into()),
            Some("a@example.com".into()),
            None,
            true,
            None,
        );
        cache.upsert_row(row);
        doc.get_mut("claude").unwrap().display_name = "Cosmetic".into();
        cache.seed_from_document(&doc);
        assert_eq!(
            cache
                .snapshot_rows()
                .into_iter()
                .find(|r| r.instance_id == "claude")
                .unwrap()
                .status,
            ProviderHealthStatus::Healthy
        );
        doc.get_mut("claude").unwrap().home_path = Some("another-account".into());
        cache.seed_from_document(&doc);
        let row = cache
            .snapshot_rows()
            .into_iter()
            .find(|r| r.instance_id == "claude")
            .unwrap();
        assert_eq!(row.status, ProviderHealthStatus::Unknown);
        assert!(row.account_email.is_none());
        assert!(row.version.is_none());
    }

    #[test]
    fn interval_zero_disables_scheduled_but_manual_guard_works() {
        let cache = ProviderHealthCache::new();
        assert!(!cache.should_schedule_refresh(0));
        let guard = cache.try_begin_refresh_guard().expect("manual refresh");
        assert!(cache.is_refresh_in_flight());
        assert!(cache.try_begin_refresh_guard().is_none(), "reentry refused");
        drop(guard);
        assert!(!cache.is_refresh_in_flight());
    }

    #[test]
    fn email_masking() {
        assert_eq!(
            ProviderHealthRow::mask_email("alice@example.com"),
            "a***@example.com"
        );
    }

    #[test]
    fn default_interval_constant() {
        assert_eq!(DEFAULT_HEALTH_INTERVAL_SECS, 300);
    }
}
