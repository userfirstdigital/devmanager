//! Profile-scoped singleton owner for settings, bindings, and health cache.
//!
//! Production hosts open one owner for the exact native profile root passed at
//! supervised start. Never resolve installed APPDATA via `app_config_dir`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use crate::providers::settings::binding::ProviderInstanceBindingStore;
use crate::providers::settings::health::ProviderHealthCache;
use crate::providers::settings::store::{ProviderSettingsStore, ProviderSettingsStoreError};

static PROFILE_OWNERS: OnceLock<Mutex<BTreeMap<PathBuf, Arc<ProviderProfileOwner>>>> =
    OnceLock::new();

#[derive(Clone)]
pub struct ProviderProfileOwner {
    pub settings: ProviderSettingsStore,
    pub bindings: ProviderInstanceBindingStore,
    pub health: ProviderHealthCache,
    root: PathBuf,
}

impl ProviderProfileOwner {
    /// Open (or reuse) the singleton owner for one canonical profile directory.
    pub fn open_dir(dir: &Path) -> Result<Arc<Self>, ProviderSettingsStoreError> {
        std::fs::create_dir_all(dir).map_err(|e| {
            ProviderSettingsStoreError::Path(format!("provider profile create failed: {e}"))
        })?;
        let dir = dir.canonicalize().map_err(|e| {
            ProviderSettingsStoreError::Path(format!("provider profile canonicalize failed: {e}"))
        })?;
        let slot = PROFILE_OWNERS.get_or_init(|| Mutex::new(BTreeMap::new()));
        let mut guard = slot.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = guard.get(&dir) {
            return Ok(Arc::clone(existing));
        }
        let settings = ProviderSettingsStore::open_dir(&dir)?;
        let bindings = ProviderInstanceBindingStore::open_dir(&dir)
            .map_err(|e| ProviderSettingsStoreError::Path(format!("bindings unavailable: {e}")))?;
        let health = ProviderHealthCache::new();
        health.seed_from_document(&settings.snapshot());
        let owner = Arc::new(Self {
            settings,
            bindings,
            health,
            root: dir.clone(),
        });
        guard.insert(dir, Arc::clone(&owner));
        Ok(owner)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    #[cfg(test)]
    pub fn open_dir_for_test(dir: &Path) -> Result<Arc<Self>, ProviderSettingsStoreError> {
        // Tests intentionally bypass the process-global map so two roots under
        // the same ambient env stay independent within one process.
        let settings = ProviderSettingsStore::open_dir(dir)?;
        let bindings = ProviderInstanceBindingStore::open_dir(dir)
            .map_err(|e| ProviderSettingsStoreError::Path(format!("bindings unavailable: {e}")))?;
        let health = ProviderHealthCache::new();
        health.seed_from_document(&settings.snapshot());
        Ok(Arc::new(Self {
            settings,
            bindings,
            health,
            root: dir.to_path_buf(),
        }))
    }
}
