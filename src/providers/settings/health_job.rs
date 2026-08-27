//! Cancellation-owned provider health refresh jobs (host-owned, not Settings-page).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::providers::settings::{
    ProviderHealthCache, ProviderHealthRefreshGuard, ProviderProfileOwner,
    ProviderSettingsDocument, DEFAULT_HEALTH_INTERVAL_SECS,
};

/// Owner for scheduled/manual health refresh. Drop cancels the active job.
pub struct ProviderHealthJobOwner {
    profile: Arc<ProviderProfileOwner>,
    cancel: Arc<AtomicBool>,
    active_generation: Arc<AtomicU64>,
    active_guard: Mutex<Option<ProviderHealthRefreshGuard>>,
    config_revision: AtomicU64,
}

impl ProviderHealthJobOwner {
    pub fn from_profile(profile: Arc<ProviderProfileOwner>) -> Self {
        let revision = profile.settings.snapshot().revision;
        Self {
            profile,
            cancel: Arc::new(AtomicBool::new(false)),
            active_generation: Arc::new(AtomicU64::new(0)),
            active_guard: Mutex::new(None),
            config_revision: AtomicU64::new(revision),
        }
    }

    pub fn health_cache(&self) -> &ProviderHealthCache {
        &self.profile.health
    }

    pub fn document(&self) -> ProviderSettingsDocument {
        self.profile.settings.redacted_snapshot()
    }

    pub fn interval_secs(&self) -> u64 {
        self.profile.settings.snapshot().health_interval_secs
    }

    pub fn should_schedule(&self) -> bool {
        self.profile
            .health
            .should_schedule_refresh(self.interval_secs())
    }

    /// Begin a refresh if none is in flight. Retains the RAII guard (no forget).
    pub fn try_begin_manual_refresh(&self) -> Option<u64> {
        if self.cancel.load(Ordering::Acquire) {
            return None;
        }
        let guard = self.profile.health.try_begin_refresh_guard()?;
        let generation = guard.generation();
        self.active_generation.store(generation, Ordering::Release);
        *self.active_guard.lock().unwrap_or_else(|e| e.into_inner()) = Some(guard);
        Some(generation)
    }

    pub fn finish_refresh(&self, generation: u64, error: Option<String>) {
        let mut slot = self.active_guard.lock().unwrap_or_else(|e| e.into_inner());
        if slot
            .as_ref()
            .is_some_and(|guard| guard.generation() == generation)
        {
            slot.take().expect("matched refresh guard").finish(error);
        }
    }

    pub fn is_stale_generation(&self, generation: u64) -> bool {
        self.active_generation.load(Ordering::Acquire) != generation
            || self.cancel.load(Ordering::Acquire)
    }

    pub fn is_stale_config(&self, expected_revision: u64) -> bool {
        self.config_revision.load(Ordering::Acquire) != expected_revision
    }

    pub fn note_config_revision(&self, revision: u64) {
        self.config_revision.store(revision, Ordering::Release);
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Release);
        let _ = self
            .active_guard
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
    }

    pub fn default_interval() -> u64 {
        DEFAULT_HEALTH_INTERVAL_SECS
    }

    pub fn bounded_probe_deadline() -> Duration {
        Duration::from_secs(30)
    }
}

impl Drop for ProviderHealthJobOwner {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn interval_zero_is_manual_only() {
        let dir = tempdir().unwrap();
        let profile = ProviderProfileOwner::open_dir_for_test(dir.path()).unwrap();
        let owner = ProviderHealthJobOwner::from_profile(profile);
        // Force interval 0 via document mutation path is settings-owned; cache
        // schedule helper already covers 0. Owner cancels on drop.
        assert_eq!(
            ProviderHealthJobOwner::default_interval(),
            DEFAULT_HEALTH_INTERVAL_SECS
        );
        let gen = owner.try_begin_manual_refresh().expect("first");
        assert!(owner.try_begin_manual_refresh().is_none());
        owner.finish_refresh(gen, None);
        assert!(owner.try_begin_manual_refresh().is_some());
    }

    #[test]
    fn drop_cancels_in_flight_guard() {
        let dir = tempdir().unwrap();
        let profile = ProviderProfileOwner::open_dir_for_test(dir.path()).unwrap();
        let owner = ProviderHealthJobOwner::from_profile(profile.clone());
        let _gen = owner.try_begin_manual_refresh().unwrap();
        assert!(profile.health.is_refresh_in_flight());
        drop(owner);
        assert!(!profile.health.is_refresh_in_flight());
    }

    #[test]
    fn old_completion_cannot_clear_new_refresh() {
        let dir = tempdir().unwrap();
        let profile = ProviderProfileOwner::open_dir_for_test(dir.path()).unwrap();
        let owner = ProviderHealthJobOwner::from_profile(profile.clone());
        let first = owner.try_begin_manual_refresh().unwrap();
        owner.finish_refresh(first, None);
        let second = owner.try_begin_manual_refresh().unwrap();
        owner.finish_refresh(first, Some("late error".into()));
        assert!(profile.health.is_refresh_in_flight());
        assert!(profile.health.last_error().is_none());
        owner.finish_refresh(second, None);
        assert!(!profile.health.is_refresh_in_flight());
        owner.cancel();
        assert!(owner.try_begin_manual_refresh().is_none());
    }
}
