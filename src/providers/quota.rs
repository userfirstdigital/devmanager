//! One cached quota observation per provider executable/version.
//!
//! Refresh runs off the UI/terminal hot path. Concurrent callers collapse onto
//! a single in-flight probe under one process-wide probe limiter. Client
//! metadata/top-bar projections omit the display value when the observation is
//! at least one hour old. This module does not invent provider fields, call
//! model APIs, or write task snapshots.
//!
//! Claude/Codex/Cursor adapter modules are not present in this worktree, so this
//! crate cannot probe a real subscription CLI/local status surface. The durable
//! contract is the observer, `AdapterQuotaOutcome`, and `ProviderQuotaHost`.

use crate::domain::snapshot::omit_stale_quota_display;
use crate::providers::adapter::MAX_PROVIDER_PROBE_TIMEOUT;
use crate::providers::adapter::{ProviderAdapter, ProviderError};
use crate::providers::capabilities::{
    ProviderCapability, ProviderExecutable, ProviderKind, ProviderVersion,
};
use async_trait::async_trait;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{Notify, Semaphore};

pub use crate::domain::snapshot::QUOTA_DISPLAY_TTL_MS;

pub const MAX_QUOTA_CACHE_ENTRIES: usize = 32;
pub const MAX_QUOTA_IN_FLIGHT_PROBES: usize = 3;
pub const MAX_QUOTA_WINDOWS: usize = 8;
pub const MAX_QUOTA_JITTER: Duration = Duration::from_secs(60);
pub const MAX_QUOTA_BACKOFF: Duration = Duration::from_secs(15 * 60);
pub const MAX_QUOTA_MIN_REFRESH_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// Cache occupancy is per observer, and the host keeps exactly one observer per
/// provider kind. The host-wide ceiling is therefore
/// `max_cache_entries × registered kinds`, not a single shared map.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaWindowError {
    RemainingPercentOutOfRange,
}

impl fmt::Display for QuotaWindowError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RemainingPercentOutOfRange => {
                write!(f, "quota remaining percent must be between 0 and 100")
            }
        }
    }
}

impl std::error::Error for QuotaWindowError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaWindow {
    remaining_percent: Option<u8>,
    resets_at: Option<u64>,
}

impl QuotaWindow {
    pub fn new(
        remaining_percent: Option<u8>,
        resets_at: Option<u64>,
    ) -> Result<Self, QuotaWindowError> {
        if remaining_percent.is_some_and(|percent| percent > 100) {
            return Err(QuotaWindowError::RemainingPercentOutOfRange);
        }
        Ok(Self {
            remaining_percent,
            resets_at,
        })
    }

    pub const fn remaining_percent(&self) -> Option<u8> {
        self.remaining_percent
    }

    pub const fn resets_at(&self) -> Option<u64> {
        self.resets_at
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaObservation {
    provider: ProviderKind,
    observed_at: u64,
    reset_at: Option<u64>,
    windows: Vec<QuotaWindow>,
    source_version: ProviderVersion,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaObservationError {
    ObservedAtZero,
    TooManyWindows,
}

impl QuotaObservation {
    pub fn new(
        provider: ProviderKind,
        observed_at: u64,
        reset_at: Option<u64>,
        windows: Vec<QuotaWindow>,
        source_version: ProviderVersion,
    ) -> Result<Self, QuotaObservationError> {
        if observed_at == 0 {
            return Err(QuotaObservationError::ObservedAtZero);
        }
        if windows.len() > MAX_QUOTA_WINDOWS {
            return Err(QuotaObservationError::TooManyWindows);
        }
        Ok(Self {
            provider,
            observed_at,
            reset_at,
            windows,
            source_version,
        })
    }

    pub const fn provider(&self) -> ProviderKind {
        self.provider
    }

    pub const fn observed_at(&self) -> u64 {
        self.observed_at
    }

    pub const fn reset_at(&self) -> Option<u64> {
        self.reset_at
    }

    pub fn windows(&self) -> &[QuotaWindow] {
        &self.windows
    }

    pub const fn source_version(&self) -> &ProviderVersion {
        &self.source_version
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaState {
    Fresh,
    Refreshing,
    Unavailable,
    Unsupported,
    AuthRequired,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaDiagnostic {
    TimedOut,
    ProbeFailed,
    Cancelled,
    KindMismatch,
    CacheFull,
    EntropyUnavailable,
    InvalidObservation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaView {
    state: QuotaState,
    observation: Option<QuotaObservation>,
    diagnostic: Option<QuotaDiagnostic>,
}

impl QuotaView {
    pub const fn state(&self) -> QuotaState {
        self.state
    }

    pub const fn observation(&self) -> Option<&QuotaObservation> {
        self.observation.as_ref()
    }

    pub const fn diagnostic(&self) -> Option<QuotaDiagnostic> {
        self.diagnostic
    }

    fn failed(diagnostic: QuotaDiagnostic) -> Self {
        Self {
            state: QuotaState::Failed,
            observation: None,
            diagnostic: Some(diagnostic),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct QuotaCacheKey {
    kind: ProviderKind,
    executable: ProviderExecutable,
    version: ProviderVersion,
}

impl QuotaCacheKey {
    pub fn new(
        kind: ProviderKind,
        executable: ProviderExecutable,
        version: ProviderVersion,
    ) -> Self {
        Self {
            kind,
            executable,
            version,
        }
    }

    pub const fn kind(&self) -> ProviderKind {
        self.kind
    }

    pub fn executable(&self) -> &ProviderExecutable {
        &self.executable
    }

    pub const fn version(&self) -> &ProviderVersion {
        &self.version
    }

    fn eviction_rank(&self) -> (ProviderKind, String, String) {
        (
            self.kind,
            self.executable.canonical_path().display().to_string(),
            self.version.as_str().to_owned(),
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuotaSourceOutcome {
    Supported {
        reset_at: Option<u64>,
        windows: Vec<QuotaWindow>,
    },
    Unavailable,
    Unsupported,
    AuthRequired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaSourceError {
    TimedOut,
    Failed,
}

#[async_trait]
pub trait QuotaObserverSource: Send + Sync {
    fn kind(&self) -> ProviderKind;

    async fn observe_quota(
        &self,
        executable: &Path,
        version: &ProviderVersion,
    ) -> Result<QuotaSourceOutcome, QuotaSourceError>;
}

#[async_trait]
pub trait QuotaClock: Send + Sync {
    fn now_ms(&self) -> u64;
    async fn sleep_until(&self, deadline_ms: u64);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaJitterError {
    EntropyUnavailable,
}

pub trait QuotaRng: Send + Sync {
    fn jitter_ms(&self, max_inclusive: u64) -> Result<u64, QuotaJitterError>;
}

/// Maps raw entropy bits onto `0..=max_inclusive` without modulo bias.
/// Returns `None` when `bits` falls in the rejection-sampling remainder.
pub(crate) fn unbiased_jitter_from_bits(bits: u64, max_inclusive: u64) -> Option<u64> {
    if max_inclusive == 0 {
        return Some(0);
    }
    if max_inclusive == u64::MAX {
        return Some(bits);
    }
    let span = u128::from(max_inclusive) + 1;
    let range = u128::from(u64::MAX) + 1;
    let limit = (range / span) * span;
    if u128::from(bits) < limit {
        Some((u128::from(bits) % span) as u64)
    } else {
        None
    }
}

#[cfg(test)]
mod test_fakes {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[derive(Clone)]
    pub struct FakeClock {
        now_ms: Arc<AtomicU64>,
        advanced: Arc<Notify>,
    }

    impl FakeClock {
        pub fn new(now_ms: u64) -> Self {
            Self {
                now_ms: Arc::new(AtomicU64::new(now_ms)),
                advanced: Arc::new(Notify::new()),
            }
        }

        pub fn advance(&self, ms: u64) {
            self.now_ms.fetch_add(ms, Ordering::SeqCst);
            self.advanced.notify_waiters();
        }

        pub fn set(&self, now_ms: u64) {
            self.now_ms.store(now_ms, Ordering::SeqCst);
            self.advanced.notify_waiters();
        }
    }

    #[async_trait]
    impl QuotaClock for FakeClock {
        fn now_ms(&self) -> u64 {
            self.now_ms.load(Ordering::SeqCst)
        }

        async fn sleep_until(&self, deadline_ms: u64) {
            loop {
                if self.now_ms() >= deadline_ms {
                    return;
                }
                let notified = self.advanced.notified();
                if self.now_ms() >= deadline_ms {
                    return;
                }
                notified.await;
            }
        }
    }

    #[derive(Debug, Clone, Copy)]
    pub struct FakeRng {
        jitter_ms: u64,
    }

    impl FakeRng {
        pub fn new(jitter_ms: u64) -> Self {
            Self { jitter_ms }
        }
    }

    impl QuotaRng for FakeRng {
        fn jitter_ms(&self, max_inclusive: u64) -> Result<u64, QuotaJitterError> {
            Ok(self.jitter_ms.min(max_inclusive))
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ProductionJitter;

impl QuotaRng for ProductionJitter {
    fn jitter_ms(&self, max_inclusive: u64) -> Result<u64, QuotaJitterError> {
        if max_inclusive == 0 {
            return Ok(0);
        }
        loop {
            let mut bytes = [0_u8; 8];
            getrandom::fill(&mut bytes).map_err(|_| QuotaJitterError::EntropyUnavailable)?;
            if let Some(value) = unbiased_jitter_from_bits(u64::from_le_bytes(bytes), max_inclusive)
            {
                return Ok(value);
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaConfigError {
    ZeroTimeout,
    TimeoutTooLong,
    BackoffTooLong,
    JitterTooLong,
    RefreshIntervalTooLong,
    RefreshIntervalOverflow,
    ZeroCacheEntries,
    CacheTooLarge,
    ZeroInFlight,
    InFlightTooLarge,
}

impl fmt::Display for QuotaConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroTimeout => write!(f, "quota probe timeout must be non-zero"),
            Self::TimeoutTooLong => write!(f, "quota probe timeout exceeded its bound"),
            Self::BackoffTooLong => write!(f, "quota failure backoff exceeded its bound"),
            Self::JitterTooLong => write!(f, "quota jitter exceeded its bound"),
            Self::RefreshIntervalTooLong => {
                write!(f, "quota min refresh interval exceeded its bound")
            }
            Self::RefreshIntervalOverflow => {
                write!(f, "quota min refresh interval plus jitter overflowed")
            }
            Self::ZeroCacheEntries => write!(f, "quota cache must keep at least one entry"),
            Self::CacheTooLarge => write!(f, "quota cache exceeded its bound"),
            Self::ZeroInFlight => write!(f, "quota probe limiter must allow at least one probe"),
            Self::InFlightTooLarge => write!(f, "quota probe limiter exceeded its bound"),
        }
    }
}

impl std::error::Error for QuotaConfigError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuotaObserverConfig {
    pub timeout: Duration,
    pub min_refresh_interval: Duration,
    pub failure_backoff: Duration,
    pub max_jitter: Duration,
    pub max_cache_entries: usize,
    pub max_in_flight_probes: usize,
}

impl QuotaObserverConfig {
    pub fn validate(&self) -> Result<(), QuotaConfigError> {
        if self.timeout.is_zero() {
            return Err(QuotaConfigError::ZeroTimeout);
        }
        if self.timeout > MAX_PROVIDER_PROBE_TIMEOUT {
            return Err(QuotaConfigError::TimeoutTooLong);
        }
        if self.failure_backoff > MAX_QUOTA_BACKOFF {
            return Err(QuotaConfigError::BackoffTooLong);
        }
        if self.max_jitter > MAX_QUOTA_JITTER {
            return Err(QuotaConfigError::JitterTooLong);
        }
        if self.min_refresh_interval > MAX_QUOTA_MIN_REFRESH_INTERVAL {
            return Err(QuotaConfigError::RefreshIntervalTooLong);
        }
        let min_refresh_ms = u64_millis(self.min_refresh_interval);
        let jitter_ms = u64_millis(self.max_jitter);
        if min_refresh_ms.checked_add(jitter_ms).is_none() {
            return Err(QuotaConfigError::RefreshIntervalOverflow);
        }
        if self.max_cache_entries == 0 {
            return Err(QuotaConfigError::ZeroCacheEntries);
        }
        if self.max_cache_entries > MAX_QUOTA_CACHE_ENTRIES {
            return Err(QuotaConfigError::CacheTooLarge);
        }
        if self.max_in_flight_probes == 0 {
            return Err(QuotaConfigError::ZeroInFlight);
        }
        if self.max_in_flight_probes > MAX_QUOTA_IN_FLIGHT_PROBES {
            return Err(QuotaConfigError::InFlightTooLarge);
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct QuotaProbeLimiter {
    inner: Arc<Semaphore>,
}

impl QuotaProbeLimiter {
    pub fn new(max_in_flight_probes: usize) -> Result<Self, QuotaConfigError> {
        if max_in_flight_probes == 0 {
            return Err(QuotaConfigError::ZeroInFlight);
        }
        if max_in_flight_probes > MAX_QUOTA_IN_FLIGHT_PROBES {
            return Err(QuotaConfigError::InFlightTooLarge);
        }
        Ok(Self {
            inner: Arc::new(Semaphore::new(max_in_flight_probes)),
        })
    }
}

pub struct AdapterQuotaSource {
    adapter: Arc<dyn ProviderAdapter>,
}

impl AdapterQuotaSource {
    pub fn new(adapter: Arc<dyn ProviderAdapter>) -> Self {
        Self { adapter }
    }
}

#[async_trait]
impl QuotaObserverSource for AdapterQuotaSource {
    fn kind(&self) -> ProviderKind {
        self.adapter.kind()
    }

    async fn observe_quota(
        &self,
        executable: &Path,
        _version: &ProviderVersion,
    ) -> Result<QuotaSourceOutcome, QuotaSourceError> {
        let identity =
            ProviderExecutable::from_path(executable).map_err(|_| QuotaSourceError::Failed)?;
        let handle = identity
            .open_for_launch()
            .map_err(|_| QuotaSourceError::Failed)?;
        match self.adapter.observe_quota(&handle).await {
            Ok(Some(sample)) => {
                let window = QuotaWindow::new(sample.remaining_percent(), sample.resets_at_ms())
                    .map_err(|_| QuotaSourceError::Failed)?;
                Ok(QuotaSourceOutcome::Supported {
                    reset_at: sample.resets_at_ms(),
                    windows: vec![window],
                })
            }
            Ok(None) => Ok(QuotaSourceOutcome::Unavailable),
            Err(ProviderError::UnsupportedCapability(ProviderCapability::ObserveQuota)) => {
                Ok(QuotaSourceOutcome::Unsupported)
            }
            Err(_) => Err(QuotaSourceError::Failed),
        }
    }
}

struct RefreshFlight {
    result: Mutex<Option<QuotaView>>,
    completed: Notify,
}

struct SlotState {
    status: QuotaState,
    last_success: Option<QuotaObservation>,
    diagnostic: Option<QuotaDiagnostic>,
    next_allowed_at: u64,
    last_used_at: u64,
    stale_sealed: bool,
}

impl SlotState {
    fn new(now_ms: u64) -> Self {
        Self {
            status: QuotaState::Unavailable,
            last_success: None,
            diagnostic: None,
            next_allowed_at: 0,
            last_used_at: now_ms,
            stale_sealed: false,
        }
    }

    fn project(&mut self, now_ms: u64) -> QuotaView {
        if let Some(observation) = &self.last_success {
            if now_ms >= observation.observed_at()
                && now_ms.saturating_sub(observation.observed_at()) >= QUOTA_DISPLAY_TTL_MS
            {
                self.stale_sealed = true;
            }
        }
        QuotaView {
            state: self.status,
            observation: if self.stale_sealed {
                None
            } else {
                self.last_success.as_ref().and_then(|observation| {
                    omit_stale_quota_display(observation.observed_at(), now_ms, observation.clone())
                })
            },
            diagnostic: self.diagnostic,
        }
    }

    fn replace_last_success(&mut self, observation: QuotaObservation) {
        self.last_success = Some(observation);
        self.stale_sealed = false;
    }
}

struct QuotaSlot {
    state: Mutex<SlotState>,
    in_flight: Mutex<Option<Arc<RefreshFlight>>>,
}

impl QuotaSlot {
    fn new(now_ms: u64) -> Self {
        Self {
            state: Mutex::new(SlotState::new(now_ms)),
            in_flight: Mutex::new(None),
        }
    }

    fn begin_flight(&self) -> (Arc<RefreshFlight>, bool) {
        let mut in_flight = self.in_flight.lock().unwrap();
        if let Some(flight) = in_flight.as_ref() {
            (Arc::clone(flight), false)
        } else {
            let flight = Arc::new(RefreshFlight {
                result: Mutex::new(None),
                completed: Notify::new(),
            });
            *in_flight = Some(Arc::clone(&flight));
            (flight, true)
        }
    }

    fn finish_flight(&self, flight: &Arc<RefreshFlight>, view: QuotaView) {
        *flight.result.lock().unwrap() = Some(view);
        flight.completed.notify_waiters();
        let mut in_flight = self.in_flight.lock().unwrap();
        if in_flight
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, flight))
        {
            *in_flight = None;
        }
    }

    fn abandon_flight(&self, flight: &Arc<RefreshFlight>, now_ms: u64) {
        let view = {
            let mut state = self.state.lock().unwrap();
            if state.status == QuotaState::Refreshing {
                state.status = QuotaState::Failed;
                state.diagnostic = Some(QuotaDiagnostic::Cancelled);
                state.next_allowed_at = 0;
            }
            state.project(now_ms)
        };
        self.finish_flight(flight, view);
    }

    async fn await_flight(&self, flight: Arc<RefreshFlight>) -> QuotaView {
        loop {
            let notified = flight.completed.notified();
            if let Some(view) = flight.result.lock().unwrap().clone() {
                return view;
            }
            notified.await;
        }
    }
}

struct FlightGuard {
    slot: Arc<QuotaSlot>,
    flight: Arc<RefreshFlight>,
    clock: Arc<dyn QuotaClock>,
    finished: bool,
}

impl FlightGuard {
    fn new(slot: Arc<QuotaSlot>, flight: Arc<RefreshFlight>, clock: Arc<dyn QuotaClock>) -> Self {
        Self {
            slot,
            flight,
            clock,
            finished: false,
        }
    }

    fn complete(&mut self, view: QuotaView) {
        self.slot.finish_flight(&self.flight, view);
        self.finished = true;
    }
}

impl Drop for FlightGuard {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        self.slot.abandon_flight(&self.flight, self.clock.now_ms());
    }
}

struct QuotaObserverInner {
    clock: Arc<dyn QuotaClock>,
    rng: Mutex<Arc<dyn QuotaRng>>,
    source: Arc<dyn QuotaObserverSource>,
    config: QuotaObserverConfig,
    limiter: QuotaProbeLimiter,
    slots: Mutex<HashMap<QuotaCacheKey, Arc<QuotaSlot>>>,
}

#[derive(Clone)]
pub struct QuotaObserver {
    inner: Arc<QuotaObserverInner>,
}

impl QuotaObserver {
    pub fn new(
        clock: Arc<dyn QuotaClock>,
        source: Arc<dyn QuotaObserverSource>,
        config: QuotaObserverConfig,
    ) -> Result<Self, QuotaConfigError> {
        let limiter = QuotaProbeLimiter::new(config.max_in_flight_probes)?;
        Self::with_limiter(clock, source, config, limiter)
    }

    pub fn with_limiter(
        clock: Arc<dyn QuotaClock>,
        source: Arc<dyn QuotaObserverSource>,
        config: QuotaObserverConfig,
        limiter: QuotaProbeLimiter,
    ) -> Result<Self, QuotaConfigError> {
        config.validate()?;
        Ok(Self {
            inner: Arc::new(QuotaObserverInner {
                clock,
                rng: Mutex::new(Arc::new(ProductionJitter)),
                source,
                config,
                limiter,
                slots: Mutex::new(HashMap::new()),
            }),
        })
    }

    #[cfg(test)]
    fn set_rng(&self, rng: Arc<dyn QuotaRng>) {
        *self.inner.rng.lock().unwrap() = rng;
    }

    pub fn cache_len(&self) -> usize {
        self.inner.slots.lock().unwrap().len()
    }

    pub fn view(&self, key: &QuotaCacheKey) -> QuotaView {
        let now_ms = self.inner.clock.now_ms();
        match self.inner.slots.lock().unwrap().get(key) {
            Some(slot) => slot.state.lock().unwrap().project(now_ms),
            None => QuotaView {
                state: QuotaState::Unavailable,
                observation: None,
                diagnostic: None,
            },
        }
    }

    pub async fn refresh(&self, key: &QuotaCacheKey) -> QuotaView {
        if self.inner.source.kind() != key.kind() {
            return QuotaView::failed(QuotaDiagnostic::KindMismatch);
        }
        let Some(slot) = self.slot(key) else {
            return QuotaView::failed(QuotaDiagnostic::CacheFull);
        };
        let (flight, leader) = slot.begin_flight();
        if !leader {
            return slot.await_flight(flight).await;
        }
        let mut guard = FlightGuard::new(Arc::clone(&slot), flight, Arc::clone(&self.inner.clock));
        let view = self.run_refresh(key, &slot).await;
        guard.complete(view.clone());
        view
    }

    fn slot(&self, key: &QuotaCacheKey) -> Option<Arc<QuotaSlot>> {
        let now_ms = self.inner.clock.now_ms();
        let mut slots = self.inner.slots.lock().unwrap();
        if let Some(slot) = slots.get(key) {
            slot.state.lock().unwrap().last_used_at = now_ms;
            return Some(Arc::clone(slot));
        }
        if slots.len() >= self.inner.config.max_cache_entries && !evict_oldest_idle(&mut slots) {
            return None;
        }
        let slot = Arc::new(QuotaSlot::new(now_ms));
        slots.insert(key.clone(), Arc::clone(&slot));
        Some(slot)
    }

    fn current_views(&self) -> Vec<(Option<(u64, ProviderVersion)>, QuotaView)> {
        let now_ms = self.inner.clock.now_ms();
        self.inner
            .slots
            .lock()
            .unwrap()
            .values()
            .map(|slot| {
                let mut state = slot.state.lock().unwrap();
                let identity = state.last_success.as_ref().map(|observation| {
                    (
                        observation.observed_at(),
                        observation.source_version().clone(),
                    )
                });
                (identity, state.project(now_ms))
            })
            .collect()
    }

    async fn run_refresh(&self, key: &QuotaCacheKey, slot: &QuotaSlot) -> QuotaView {
        let now_ms = self.inner.clock.now_ms();
        {
            let mut state = slot.state.lock().unwrap();
            if state
                .last_success
                .as_ref()
                .is_some_and(|observation| now_ms < observation.observed_at())
            {
                state.status = QuotaState::Failed;
                state.diagnostic = Some(QuotaDiagnostic::InvalidObservation);
                state.next_allowed_at =
                    now_ms.saturating_add(u64_millis(self.inner.config.failure_backoff));
                state.last_used_at = now_ms;
                return state.project(now_ms);
            }
            if now_ms < state.next_allowed_at {
                return state.project(now_ms);
            }
            state.status = QuotaState::Refreshing;
            state.last_used_at = now_ms;
        }

        let _permit = self
            .inner
            .limiter
            .inner
            .acquire()
            .await
            .expect("quota probe limiter remains open");
        let timeout = self.inner.config.timeout;
        let deadline = self
            .inner
            .clock
            .now_ms()
            .saturating_add(u64_millis(timeout));
        let outcome = tokio::select! {
            result = self.inner.source.observe_quota(
                key.executable().canonical_path(),
                key.version(),
            ) => result,
            _ = self.inner.clock.sleep_until(deadline) => Err(QuotaSourceError::TimedOut),
        };

        let now_ms = self.inner.clock.now_ms();
        let jitter = self
            .inner
            .rng
            .lock()
            .unwrap()
            .jitter_ms(u64_millis(self.inner.config.max_jitter));
        let mut state = slot.state.lock().unwrap();
        let jitter = match jitter {
            Ok(jitter) => jitter,
            Err(QuotaJitterError::EntropyUnavailable) => {
                state.status = QuotaState::Failed;
                state.diagnostic = Some(QuotaDiagnostic::EntropyUnavailable);
                state.next_allowed_at =
                    now_ms.saturating_add(u64_millis(self.inner.config.failure_backoff));
                state.last_used_at = now_ms;
                return state.project(now_ms);
            }
        };
        if let Some(previous) = &state.last_success {
            if now_ms < previous.observed_at() {
                state.status = QuotaState::Failed;
                state.diagnostic = Some(QuotaDiagnostic::InvalidObservation);
                state.next_allowed_at =
                    now_ms.saturating_add(u64_millis(self.inner.config.failure_backoff) + jitter);
                state.last_used_at = now_ms;
                return state.project(now_ms);
            }
        }
        match outcome {
            Ok(QuotaSourceOutcome::Supported { reset_at, windows }) => {
                match QuotaObservation::new(
                    key.kind(),
                    now_ms,
                    reset_at,
                    windows,
                    key.version().clone(),
                ) {
                    Ok(observation) => {
                        state.replace_last_success(observation);
                        state.status = QuotaState::Fresh;
                        state.diagnostic = None;
                        state.next_allowed_at = now_ms.saturating_add(
                            u64_millis(self.inner.config.min_refresh_interval) + jitter,
                        );
                    }
                    Err(_) => {
                        state.status = QuotaState::Failed;
                        state.diagnostic = Some(QuotaDiagnostic::InvalidObservation);
                        state.next_allowed_at = now_ms
                            .saturating_add(u64_millis(self.inner.config.failure_backoff) + jitter);
                    }
                }
            }
            Ok(QuotaSourceOutcome::Unavailable) => {
                state.last_success = None;
                state.status = QuotaState::Unavailable;
                state.diagnostic = None;
                state.next_allowed_at = now_ms
                    .saturating_add(u64_millis(self.inner.config.min_refresh_interval) + jitter);
            }
            Ok(QuotaSourceOutcome::Unsupported) => {
                state.last_success = None;
                state.status = QuotaState::Unsupported;
                state.diagnostic = None;
                state.next_allowed_at = now_ms
                    .saturating_add(u64_millis(self.inner.config.min_refresh_interval) + jitter);
            }
            Ok(QuotaSourceOutcome::AuthRequired) => {
                state.last_success = None;
                state.status = QuotaState::AuthRequired;
                state.diagnostic = None;
                state.next_allowed_at = now_ms
                    .saturating_add(u64_millis(self.inner.config.min_refresh_interval) + jitter);
            }
            Err(error) => {
                state.status = QuotaState::Failed;
                state.diagnostic = Some(match error {
                    QuotaSourceError::TimedOut => QuotaDiagnostic::TimedOut,
                    QuotaSourceError::Failed => QuotaDiagnostic::ProbeFailed,
                });
                state.next_allowed_at =
                    now_ms.saturating_add(u64_millis(self.inner.config.failure_backoff) + jitter);
            }
        }
        state.last_used_at = now_ms;
        state.project(now_ms)
    }
}

fn evict_oldest_idle(slots: &mut HashMap<QuotaCacheKey, Arc<QuotaSlot>>) -> bool {
    let victim = slots
        .iter()
        .filter(|(_, slot)| slot.in_flight.lock().unwrap().is_none())
        .min_by_key(|(key, slot)| (slot.state.lock().unwrap().last_used_at, key.eviction_rank()))
        .map(|(key, _)| key.clone());
    if let Some(key) = victim {
        slots.remove(&key);
        true
    } else {
        false
    }
}

fn u64_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaStripEntry {
    provider: ProviderKind,
    state: QuotaState,
    observation: Option<QuotaObservation>,
}

impl QuotaStripEntry {
    pub const fn provider(&self) -> ProviderKind {
        self.provider
    }

    pub const fn state(&self) -> QuotaState {
        self.state
    }

    pub const fn observation(&self) -> Option<&QuotaObservation> {
        self.observation.as_ref()
    }
}

/// Sealed so the status bar can be built only from `QuotaStripEntry` or the
/// host that produces it. Semantic replay / `Status{usage}` cannot implement
/// this trait from another crate (or this one without editing this module).
mod quota_bar_source {
    use super::{ProviderQuotaHost, QuotaStripEntry};

    pub trait Sealed {
        fn into_strip_entries(self) -> Vec<QuotaStripEntry>;
    }

    impl Sealed for Vec<QuotaStripEntry> {
        fn into_strip_entries(self) -> Vec<QuotaStripEntry> {
            self
        }
    }

    impl Sealed for &ProviderQuotaHost {
        fn into_strip_entries(self) -> Vec<QuotaStripEntry> {
            self.project_top_bar()
        }
    }
}

/// Canonical top-bar payload. Cutover must call [`canonical_top_bar`] and, in
/// the same change, delete legacy scrape so replay cannot remain a second truth.
///
/// # Port manifest (these files only)
/// - `src/providers/quota.rs`
/// - `src/providers/mod.rs` (re-export)
/// - `src/domain/snapshot.rs` (`omit_stale_quota_display`)
/// - `src/domain/mod.rs` (re-export)
/// - `src/providers/registry.rs` (executable/version keys for `tick` only)
///
/// Excluded: adapter foundation hunks, `tests/provider_registry.rs`, `src/app/**`.
///
/// # Deletion-union order (same change as first `canonical_top_bar` call)
/// 1. Replace `ai_quota_statuses()` at the status-bar call site with
///    `canonical_top_bar(&host).entries()`.
/// 2. Delete `spawn_ai_quota_refresh_task` and its `App::new` start.
/// 3. Delete `refresh_ai_quota_states` and `latest_quota_usage_from_replay`.
/// 4. Delete `ai_quota_states`, `AiQuotaState`, `is_ai_quota_stale`,
///    `ai_provider_name`, `app_now_epoch_ms`.
/// 5. Delete `AI_QUOTA_REFRESH_INTERVAL` and `AI_QUOTA_VISIBILITY_TTL`.
///
/// Keep `chrome::QuotaStatus` / `render_ai_quota_status` as view-only mapping
/// from `&[QuotaStripEntry]`. Do not add a replay/`SemanticEvent` constructor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalQuotaBar {
    entries: Vec<QuotaStripEntry>,
}

impl CanonicalQuotaBar {
    pub fn from_source<S: quota_bar_source::Sealed>(source: S) -> Self {
        Self {
            entries: source.into_strip_entries(),
        }
    }

    pub fn entries(&self) -> &[QuotaStripEntry] {
        &self.entries
    }
}

/// Compile/API assertion: the bar accepts only sealed strip sources.
pub fn canonical_top_bar<S: quota_bar_source::Sealed>(source: S) -> CanonicalQuotaBar {
    CanonicalQuotaBar::from_source(source)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaHostError {
    DuplicateProviderKind(ProviderKind),
    InvalidConfig(QuotaConfigError),
}

impl fmt::Display for QuotaHostError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateProviderKind(kind) => {
                write!(f, "quota observer already registered for {kind:?}")
            }
            Self::InvalidConfig(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for QuotaHostError {}

/// Sole scheduled quota strip source. Wiring this into the status bar must
/// delete `App::ai_quota_states` / scrape in the same change; the two must not
/// coexist as dual truth.
pub struct ProviderQuotaHost {
    clock: Arc<dyn QuotaClock>,
    config: QuotaObserverConfig,
    limiter: QuotaProbeLimiter,
    observers: Mutex<BTreeMap<ProviderKind, QuotaObserver>>,
}

impl ProviderQuotaHost {
    pub fn new(
        clock: Arc<dyn QuotaClock>,
        config: QuotaObserverConfig,
    ) -> Result<Self, QuotaConfigError> {
        config.validate()?;
        Ok(Self {
            limiter: QuotaProbeLimiter::new(config.max_in_flight_probes)?,
            clock,
            config,
            observers: Mutex::new(BTreeMap::new()),
        })
    }

    pub fn register(&self, source: Arc<dyn QuotaObserverSource>) -> Result<(), QuotaHostError> {
        let kind = source.kind();
        let mut observers = self.observers.lock().unwrap();
        if observers.contains_key(&kind) {
            return Err(QuotaHostError::DuplicateProviderKind(kind));
        }
        let observer = QuotaObserver::with_limiter(
            Arc::clone(&self.clock),
            source,
            self.config,
            self.limiter.clone(),
        )
        .map_err(QuotaHostError::InvalidConfig)?;
        observers.insert(kind, observer);
        Ok(())
    }

    pub async fn tick(&self, keys: &[QuotaCacheKey]) {
        let mut seen = HashSet::new();
        let unique = keys
            .iter()
            .filter(|key| seen.insert((*key).clone()))
            .cloned()
            .collect::<Vec<_>>();
        for key in unique {
            let observer = self.observers.lock().unwrap().get(&key.kind()).cloned();
            if let Some(observer) = observer {
                let _ = observer.refresh(&key).await;
            }
        }
    }

    pub fn view(&self, key: &QuotaCacheKey) -> QuotaView {
        self.observers
            .lock()
            .unwrap()
            .get(&key.kind())
            .map(|observer| observer.view(key))
            .unwrap_or(QuotaView {
                state: QuotaState::Unavailable,
                observation: None,
                diagnostic: None,
            })
    }

    pub fn cache_len(&self, kind: ProviderKind) -> usize {
        self.observers
            .lock()
            .unwrap()
            .get(&kind)
            .map(QuotaObserver::cache_len)
            .unwrap_or(0)
    }

    pub fn cache_len_total(&self) -> usize {
        self.observers
            .lock()
            .unwrap()
            .values()
            .map(QuotaObserver::cache_len)
            .sum()
    }

    pub fn project_top_bar(&self) -> Vec<QuotaStripEntry> {
        self.observers
            .lock()
            .unwrap()
            .iter()
            .map(|(&kind, observer)| {
                let selected = observer
                    .current_views()
                    .into_iter()
                    .max_by_key(|(identity, _)| {
                        identity.as_ref().map(|(observed_at, version)| {
                            (*observed_at, version.as_str().to_owned())
                        })
                    })
                    .map(|(_, view)| view);
                match selected {
                    Some(view) => QuotaStripEntry {
                        provider: kind,
                        state: view.state(),
                        observation: view.observation().cloned(),
                    },
                    None => QuotaStripEntry {
                        provider: kind,
                        state: QuotaState::Unavailable,
                        observation: None,
                    },
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod lost_wakeup {
    use super::*;

    #[tokio::test]
    async fn follower_sees_result_published_before_subscribe() {
        let slot = QuotaSlot::new(0);
        let (flight, leader) = slot.begin_flight();
        assert!(leader);
        let published = QuotaView::failed(QuotaDiagnostic::Cancelled);
        slot.finish_flight(&flight, published.clone());
        let observed = tokio::time::timeout(Duration::from_millis(50), slot.await_flight(flight))
            .await
            .expect("prepublished follower must not hang");
        assert_eq!(observed, published);
    }

    #[tokio::test]
    async fn follower_subscribe_before_recheck_wins_the_publish_race() {
        let slot = Arc::new(QuotaSlot::new(0));
        let (flight, leader) = slot.begin_flight();
        assert!(leader);
        let waiter_slot = Arc::clone(&slot);
        let waiter_flight = Arc::clone(&flight);
        let started = Arc::new(Notify::new());
        let waiter_started = Arc::clone(&started);
        let waiter = tokio::spawn(async move {
            waiter_started.notify_waiters();
            waiter_slot.await_flight(waiter_flight).await
        });
        started.notified().await;
        tokio::task::yield_now().await;
        let published = QuotaView::failed(QuotaDiagnostic::Cancelled);
        slot.finish_flight(&flight, published.clone());
        let observed = tokio::time::timeout(Duration::from_millis(200), waiter)
            .await
            .expect("racy follower must settle")
            .expect("waiter task");
        assert_eq!(observed, published);
    }
}

#[cfg(test)]
#[path = "quota_tests.rs"]
mod tests;
