//! Public-surface quota contract. Clock-controlled and RNG-injection tests live
//! in `src/providers/quota_tests.rs` so FakeClock/FakeRng/set_rng stay `cfg(test)`.

use async_trait::async_trait;
use devmanager::domain::snapshot::omit_stale_quota_display;
use devmanager::providers::adapter::{
    AdapterDeliveryPermit, AdapterIngressUnavailable, JournalNormalizeError, LaunchProviderRequest,
    NormalizedAdapterDelivery, ProviderAdapter, ProviderError, ProviderLaunchSpec,
    ProviderQuotaStatus, ProviderRuntime, QuotaObservation as AdapterQuotaSample, StopStrategy,
};
use devmanager::providers::capabilities::{
    CapabilityEvidence, CapabilitySupport, EvidenceSourceId, EvidenceStatus, ProviderAuthState,
    ProviderCapabilities, ProviderCapability, ProviderExecutableHandle, ProviderKind,
    ProviderVersion,
};
use devmanager::providers::quota::{
    AdapterQuotaSource, ProductionJitter, QuotaObservation, QuotaObserverConfig,
    QuotaObserverSource, QuotaRng, QuotaSourceOutcome, QuotaWindow, QUOTA_DISPLAY_TTL_MS,
};
use std::sync::Arc;
use std::time::Duration;

#[test]
fn snapshot_omits_quota_display_at_age_of_exactly_one_hour() {
    let display = "5-hour remaining";
    assert_eq!(
        omit_stale_quota_display(1_000, 1_000 + QUOTA_DISPLAY_TTL_MS - 1, display),
        Some(display)
    );
    assert_eq!(
        omit_stale_quota_display(1_000, 1_000 + QUOTA_DISPLAY_TTL_MS, display),
        None
    );
}

#[test]
fn omit_stale_hides_future_and_clock_rollback_observations() {
    assert!(omit_stale_quota_display(2_000, 1_000, "future").is_none());
    assert!(omit_stale_quota_display(5_000, 4_999, "rollback").is_none());
    assert!(omit_stale_quota_display(1_000, 1_000, "now").is_some());
}

#[test]
fn config_rejects_unbounded_timeout_backoff_jitter_and_cache_limits() {
    let mut config = fast_config();
    config.timeout = Duration::ZERO;
    assert!(QuotaObserverConfig::validate(&config).is_err());
    config = fast_config();
    config.timeout = Duration::from_secs(31);
    assert!(QuotaObserverConfig::validate(&config).is_err());
    config = fast_config();
    config.max_jitter = Duration::from_secs(61);
    assert!(QuotaObserverConfig::validate(&config).is_err());
    config = fast_config();
    config.max_cache_entries = 0;
    assert!(QuotaObserverConfig::validate(&config).is_err());
    config = fast_config();
    config.max_in_flight_probes = 0;
    assert!(QuotaObserverConfig::validate(&config).is_err());
    config = fast_config();
    config.max_in_flight_probes = 4;
    assert!(QuotaObserverConfig::validate(&config).is_err());
}

#[test]
fn config_rejects_uncapped_and_overflowing_min_refresh_interval() {
    let mut config = fast_config();
    config.min_refresh_interval = Duration::from_secs(3_601);
    assert!(QuotaObserverConfig::validate(&config).is_err());

    config = fast_config();
    config.min_refresh_interval = Duration::from_millis(u64::MAX);
    config.max_jitter = Duration::from_millis(1);
    assert!(QuotaObserverConfig::validate(&config).is_err());
}

#[test]
fn quota_observation_rejects_zero_observed_at_and_too_many_windows() {
    let window = QuotaWindow::new(Some(10), None).unwrap();
    let version = ProviderVersion::new("1").unwrap();
    assert!(QuotaObservation::new(
        ProviderKind::Codex,
        0,
        None,
        vec![window.clone()],
        version.clone()
    )
    .is_err());
    let windows = (0..9)
        .map(|index| QuotaWindow::new(Some(index as u8), None).unwrap())
        .collect::<Vec<_>>();
    assert!(QuotaObservation::new(ProviderKind::Codex, 1, None, windows, version).is_err());
}

#[test]
fn production_jitter_is_bounded_and_zero_when_uncapped_max_is_zero() {
    let jitter = ProductionJitter;
    assert_eq!(jitter.jitter_ms(0).expect("zero jitter"), 0);
    for _ in 0..8 {
        assert!(jitter.jitter_ms(7).expect("entropy") <= 7);
    }
}

#[tokio::test]
async fn adapter_source_maps_documented_sample_without_fabricating_fields() {
    let adapter = Arc::new(SampleAdapter {
        sample: AdapterQuotaSample::new(ProviderQuotaStatus::Available, Some(33), Some(99))
            .unwrap(),
    });
    let source = AdapterQuotaSource::new(adapter);
    let outcome = source
        .observe_quota(
            std::env::current_exe().expect("test executable").as_path(),
            &ProviderVersion::new("1").unwrap(),
        )
        .await
        .unwrap();
    match outcome {
        QuotaSourceOutcome::Supported { reset_at, windows } => {
            assert_eq!(reset_at, Some(99));
            assert_eq!(windows.len(), 1);
            assert_eq!(windows[0].remaining_percent(), Some(33));
            assert_eq!(windows[0].resets_at(), Some(99));
        }
        other => panic!("expected supported sample, got {other:?}"),
    }
}

#[tokio::test]
async fn adapter_source_maps_unavailable_and_unsupported_without_fabricating_windows() {
    let unavailable = AdapterQuotaSource::new(Arc::new(OutcomeAdapter {
        outcome: Outcome::Unavailable,
    }));
    assert_eq!(
        unavailable
            .observe_quota(
                std::env::current_exe().expect("test executable").as_path(),
                &ProviderVersion::new("1").unwrap()
            )
            .await
            .unwrap(),
        QuotaSourceOutcome::Unavailable
    );

    let unsupported = AdapterQuotaSource::new(Arc::new(OutcomeAdapter {
        outcome: Outcome::Unsupported,
    }));
    assert_eq!(
        unsupported
            .observe_quota(
                std::env::current_exe().expect("test executable").as_path(),
                &ProviderVersion::new("1").unwrap()
            )
            .await
            .unwrap(),
        QuotaSourceOutcome::Unsupported
    );
}

fn fast_config() -> QuotaObserverConfig {
    QuotaObserverConfig {
        timeout: Duration::from_millis(50),
        min_refresh_interval: Duration::from_millis(1),
        failure_backoff: Duration::from_millis(1),
        max_jitter: Duration::from_millis(0),
        max_cache_entries: 8,
        max_in_flight_probes: 1,
    }
}

struct SampleAdapter {
    sample: AdapterQuotaSample,
}

#[async_trait]
impl ProviderAdapter for SampleAdapter {
    fn kind(&self) -> ProviderKind {
        ProviderKind::ClaudeCode
    }

    async fn probe(
        &self,
        _executable: &ProviderExecutableHandle,
    ) -> Result<ProviderCapabilities, ProviderError> {
        Ok(ProviderCapabilities {
            kind: ProviderKind::ClaudeCode,
            version: ProviderVersion::new("1").unwrap(),
            auth_state: ProviderAuthState::Unknown,
            exact_resume: CapabilitySupport::Unknown,
            semantic_events: CapabilitySupport::Unknown,
            provider_session_id: CapabilitySupport::Unknown,
            build_launch: CapabilitySupport::Unknown,
            parse_signal: CapabilitySupport::Unknown,
            cooperative_stop: CapabilitySupport::Unknown,
            observe_quota: CapabilitySupport::Supported,
            evidence: Vec::new(),
        })
    }

    fn build_launch(
        &self,
        _request: LaunchProviderRequest,
    ) -> Result<ProviderLaunchSpec, ProviderError> {
        Err(ProviderError::UnsupportedCapability(
            ProviderCapability::BuildLaunch,
        ))
    }

    fn normalize_delivery(
        &self,
        _permit: &AdapterDeliveryPermit,
        _bytes: &[u8],
    ) -> Result<NormalizedAdapterDelivery, JournalNormalizeError> {
        Err(JournalNormalizeError::Unavailable(
            AdapterIngressUnavailable,
        ))
    }

    fn cooperative_stop(&self, _session: &ProviderRuntime) -> StopStrategy {
        StopStrategy::Unsupported
    }

    async fn observe_quota(
        &self,
        _executable: &ProviderExecutableHandle,
    ) -> Result<Option<AdapterQuotaSample>, ProviderError> {
        Ok(Some(self.sample))
    }
}

#[derive(Clone, Copy)]
enum Outcome {
    Unavailable,
    Unsupported,
}

struct OutcomeAdapter {
    outcome: Outcome,
}

#[async_trait]
impl ProviderAdapter for OutcomeAdapter {
    fn kind(&self) -> ProviderKind {
        ProviderKind::ClaudeCode
    }

    async fn probe(
        &self,
        _executable: &ProviderExecutableHandle,
    ) -> Result<ProviderCapabilities, ProviderError> {
        Ok(ProviderCapabilities {
            kind: ProviderKind::ClaudeCode,
            version: ProviderVersion::new("1").unwrap(),
            auth_state: ProviderAuthState::AuthRequired,
            exact_resume: CapabilitySupport::Unknown,
            semantic_events: CapabilitySupport::Unknown,
            provider_session_id: CapabilitySupport::Unknown,
            build_launch: CapabilitySupport::Unknown,
            parse_signal: CapabilitySupport::Unknown,
            cooperative_stop: CapabilitySupport::Unknown,
            observe_quota: CapabilitySupport::Supported,
            evidence: vec![CapabilityEvidence::new(
                EvidenceSourceId::AuthStatusProbe,
                1,
                EvidenceStatus::AuthRequired,
                None,
            )
            .unwrap()],
        })
    }

    fn build_launch(
        &self,
        _request: LaunchProviderRequest,
    ) -> Result<ProviderLaunchSpec, ProviderError> {
        Err(ProviderError::UnsupportedCapability(
            ProviderCapability::BuildLaunch,
        ))
    }

    fn normalize_delivery(
        &self,
        _permit: &AdapterDeliveryPermit,
        _bytes: &[u8],
    ) -> Result<NormalizedAdapterDelivery, JournalNormalizeError> {
        Err(JournalNormalizeError::Unavailable(
            AdapterIngressUnavailable,
        ))
    }

    fn cooperative_stop(&self, _session: &ProviderRuntime) -> StopStrategy {
        StopStrategy::Unsupported
    }

    async fn observe_quota(
        &self,
        _executable: &ProviderExecutableHandle,
    ) -> Result<Option<AdapterQuotaSample>, ProviderError> {
        match self.outcome {
            Outcome::Unavailable => Ok(None),
            Outcome::Unsupported => Err(ProviderError::UnsupportedCapability(
                ProviderCapability::ObserveQuota,
            )),
        }
    }
}
