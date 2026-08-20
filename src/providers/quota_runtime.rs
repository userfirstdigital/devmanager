//! Host-owned quota refresh lifecycle.
//!
//! The quota strip is read-only: it projects the [`ProviderQuotaHost`] cache
//! and never probes a provider from a render or request path.  This module
//! owns the one periodic refresh task for the native host.  It discovers one
//! current executable/version key per registered provider kind, then lets the
//! typed adapter quota source populate the cache.  A missing or unsupported
//! provider surface remains unavailable; no terminal output is interpreted.

use super::adapter::ProviderError;
use super::capabilities::ProviderKind;
use super::quota::{
    AdapterQuotaSource, CanonicalQuotaBar, ProviderQuotaHost, QuotaCacheKey, QuotaClock,
    QuotaConfigError, QuotaObserverConfig,
};
use super::registry::{ProviderDiscoveryConfig, ProviderRegistry};
use super::startup::{stock_provider_registry, STOCK_PROVIDER_REGISTRATION_ORDER};
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// The lifecycle owner never waits indefinitely for a quota worker to settle.
/// Provider probes are independently bounded by `QuotaObserverConfig` and an
/// in-flight cycle is aborted before this join budget is consumed.
pub const QUOTA_RUNTIME_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

const DEFAULT_QUOTA_REFRESH_INTERVAL: Duration = Duration::from_secs(15 * 60);
const DEFAULT_QUOTA_MIN_REFRESH_INTERVAL: Duration = Duration::from_secs(30 * 60);

/// Production settings for the native quota owner.  The observer TTL remains
/// the canonical one-hour display boundary; the shorter refresh period gives a
/// successful source more than one chance to refresh before it is hidden.
#[derive(Debug, Clone, Copy)]
pub struct QuotaRuntimeConfig {
    pub refresh_interval: Duration,
    pub observer: QuotaObserverConfig,
}

impl QuotaRuntimeConfig {
    pub const fn production() -> Self {
        Self {
            refresh_interval: DEFAULT_QUOTA_REFRESH_INTERVAL,
            observer: QuotaObserverConfig {
                timeout: Duration::from_secs(30),
                min_refresh_interval: DEFAULT_QUOTA_MIN_REFRESH_INTERVAL,
                failure_backoff: Duration::from_secs(15 * 60),
                max_jitter: Duration::from_secs(60),
                max_cache_entries: 8,
                max_in_flight_probes: 2,
            },
        }
    }

    fn validate(self) -> Result<(), QuotaRuntimeError> {
        if self.refresh_interval.is_zero() {
            return Err(QuotaRuntimeError::ZeroRefreshInterval);
        }
        self.observer
            .validate()
            .map_err(QuotaRuntimeError::InvalidObserverConfig)
    }
}

impl Default for QuotaRuntimeConfig {
    fn default() -> Self {
        Self::production()
    }
}

#[derive(Debug)]
pub enum QuotaRuntimeError {
    NoAsyncRuntime,
    ZeroRefreshInterval,
    InvalidObserverConfig(QuotaConfigError),
    Provider(ProviderError),
    QuotaHost(QuotaConfigError),
    DuplicateProviderKind(ProviderKind),
}

impl fmt::Display for QuotaRuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoAsyncRuntime => write!(formatter, "quota runtime requires an async host"),
            Self::ZeroRefreshInterval => write!(formatter, "quota refresh interval is zero"),
            Self::InvalidObserverConfig(error) => {
                write!(formatter, "invalid quota observer configuration: {error}")
            }
            Self::Provider(error) => {
                write!(formatter, "stock provider registration failed: {error}")
            }
            Self::QuotaHost(error) => {
                write!(formatter, "quota host initialization failed: {error}")
            }
            Self::DuplicateProviderKind(kind) => {
                write!(formatter, "quota source already registered for {kind:?}")
            }
        }
    }
}

impl std::error::Error for QuotaRuntimeError {}

impl From<ProviderError> for QuotaRuntimeError {
    fn from(error: ProviderError) -> Self {
        Self::Provider(error)
    }
}

/// System epoch clock used by the quota cache and its one-hour freshness
/// boundary.  The worker is still monotonic in scheduling because Tokio owns
/// the interval; epoch values are only for display freshness and provider reset
/// timestamps.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemQuotaClock;

#[async_trait]
impl QuotaClock for SystemQuotaClock {
    fn now_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|duration| u64::try_from(duration.as_millis()).ok())
            .unwrap_or(0)
    }

    async fn sleep_until(&self, deadline_ms: u64) {
        let remaining = deadline_ms.saturating_sub(self.now_ms());
        if remaining > 0 {
            tokio::time::sleep(Duration::from_millis(remaining)).await;
        }
    }
}

/// Native host owner for the canonical quota strip and its periodic refresh.
/// There is one instance per host process and one observer/source per provider
/// kind.  Clones are intentionally not exposed: shutdown must have one owner.
pub struct NativeQuotaHost {
    host: Arc<ProviderQuotaHost>,
    active_keys: Arc<RwLock<BTreeMap<ProviderKind, QuotaCacheKey>>>,
    stop: watch::Sender<bool>,
    task: Option<JoinHandle<()>>,
}

impl NativeQuotaHost {
    /// Start the stock provider owner on the current Tokio runtime.
    pub fn start_stock(config: QuotaRuntimeConfig) -> Result<Self, QuotaRuntimeError> {
        config.validate()?;
        let runtime =
            tokio::runtime::Handle::try_current().map_err(|_| QuotaRuntimeError::NoAsyncRuntime)?;
        let registry = Arc::new(stock_provider_registry()?);
        let host = Arc::new(
            ProviderQuotaHost::new(Arc::new(SystemQuotaClock), config.observer)
                .map_err(QuotaRuntimeError::QuotaHost)?,
        );
        for kind in STOCK_PROVIDER_REGISTRATION_ORDER {
            let Some(adapter) = registry.adapter(kind) else {
                continue;
            };
            host.register(Arc::new(AdapterQuotaSource::new(adapter)))
                .map_err(|error| match error {
                    super::quota::QuotaHostError::DuplicateProviderKind(kind) => {
                        QuotaRuntimeError::DuplicateProviderKind(kind)
                    }
                    super::quota::QuotaHostError::InvalidConfig(error) => {
                        QuotaRuntimeError::QuotaHost(error)
                    }
                })?;
        }

        let active_keys = Arc::new(RwLock::new(BTreeMap::new()));
        let (stop, stop_rx) = watch::channel(false);
        let task = runtime.spawn(refresh_loop(
            Arc::clone(&host),
            Arc::clone(&registry),
            Arc::clone(&active_keys),
            stop_rx,
            config.refresh_interval,
        ));
        Ok(Self {
            host,
            active_keys,
            stop,
            task: Some(task),
        })
    }

    /// Canonical source for the native top bar.  This method only reads the
    /// cache and performs no provider discovery, process launch, or I/O.
    pub fn top_bar(&self) -> CanonicalQuotaBar {
        super::quota::canonical_top_bar(self.host.as_ref())
    }

    pub fn host(&self) -> &ProviderQuotaHost {
        self.host.as_ref()
    }

    /// Returns the currently selected executable/version key per provider kind.
    /// The snapshot is useful for diagnostics and never grants launch authority.
    pub fn active_keys(&self) -> Vec<QuotaCacheKey> {
        self.active_keys
            .read()
            .map(|keys| keys.values().cloned().collect())
            .unwrap_or_default()
    }

    /// Signal cancellation, abort an in-flight discovery/refresh cycle, and
    /// join the owner within a bounded budget.  The host calls this before its
    /// other runtime teardown so quota work cannot outlive the host.
    pub async fn shutdown(mut self) -> Result<(), QuotaRuntimeError> {
        let _ = self.stop.send(true);
        let Some(mut task) = self.task.take() else {
            return Ok(());
        };
        if tokio::time::timeout(QUOTA_RUNTIME_SHUTDOWN_TIMEOUT, &mut task)
            .await
            .is_err()
        {
            task.abort();
            let _ = task.await;
        }
        Ok(())
    }
}

impl Drop for NativeQuotaHost {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

async fn refresh_loop(
    host: Arc<ProviderQuotaHost>,
    registry: Arc<ProviderRegistry>,
    active_keys: Arc<RwLock<BTreeMap<ProviderKind, QuotaCacheKey>>>,
    mut stop_rx: watch::Receiver<bool>,
    refresh_interval: Duration,
) {
    let mut interval = tokio::time::interval(refresh_interval);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Tokio intervals fire immediately. Consuming that tick keeps the first
    // PATH observe off the host's current-thread runtime until Hello can bind
    // and accept; hashing native Claude on that thread starves the pipe.
    interval.tick().await;
    loop {
        tokio::select! {
            changed = stop_rx.changed() => {
                if changed.is_err() || *stop_rx.borrow() {
                    break;
                }
            }
            _ = interval.tick() => {
                tokio::select! {
                    _ = refresh_cycle(
                        Arc::clone(&host),
                        Arc::clone(&registry),
                        Arc::clone(&active_keys),
                    ) => {}
                    changed = stop_rx.changed() => {
                        if changed.is_err() || *stop_rx.borrow() {
                            // Dropping the in-flight future cancels provider
                            // discovery and quota timeout work before the
                            // owner joins.
                            break;
                        }
                    }
                }
            }
        }
    }
}

async fn refresh_cycle(
    host: Arc<ProviderQuotaHost>,
    registry: Arc<ProviderRegistry>,
    active_keys: Arc<RwLock<BTreeMap<ProviderKind, QuotaCacheKey>>>,
) {
    let mut discovered = BTreeMap::new();
    for kind in STOCK_PROVIDER_REGISTRATION_ORDER {
        let observation = registry
            .observe(kind, &ProviderDiscoveryConfig::default())
            .await;
        if let Ok(observation) = observation {
            discovered.insert(
                kind,
                QuotaCacheKey::new(
                    kind,
                    observation.executable().clone(),
                    observation.version().clone(),
                ),
            );
        }
    }
    if discovered.is_empty() {
        return;
    }
    let current = {
        let Ok(mut keys) = active_keys.write() else {
            return;
        };
        *keys = discovered;
        keys.values().cloned().collect::<Vec<_>>()
    };
    host.tick(&current).await;
}

#[cfg(test)]
#[path = "quota_runtime_tests.rs"]
mod tests;
