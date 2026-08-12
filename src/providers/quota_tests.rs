//! Clock-controlled and adversarial quota tests. Compiled only with the library
//! test harness so FakeClock, FakeRng, and set_rng never appear on the
//! production surface.

use super::test_fakes::{FakeClock, FakeRng};
use super::*;
use crate::domain::snapshot::omit_stale_quota_display;
use crate::providers::adapter::{
    AdapterQuotaOutcome, JournalEvent, LaunchProviderRequest, ProviderAdapter, ProviderError,
    ProviderLaunchSpec, ProviderQuotaStatus, ProviderRuntime, ProviderSignal,
    QuotaObservation as AdapterQuotaSample, StopStrategy,
};
use crate::providers::capabilities::{
    CapabilityEvidence, CapabilitySupport, EvidenceSourceId, EvidenceStatus, ProviderAuthState,
    ProviderCapabilities, ProviderCapability, ProviderExecutable, ProviderKind, ProviderVersion,
};
use async_trait::async_trait;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;

#[tokio::test]
async fn ten_sessions_share_one_observation_per_provider_executable_version() {
    let clock = FakeClock::new(1_700_000_000_000);
    let source = SequenceSource::supported(ProviderKind::ClaudeCode, 80, Some(1_700_000_360_000));
    let observer = observer(&clock, Arc::clone(&source) as Arc<dyn QuotaObserverSource>);
    let key = cache_key(ProviderKind::ClaudeCode, "2.1.0");

    let first = observer.refresh(&key).await;
    assert_eq!(first.state(), QuotaState::Fresh);
    let observation = first.observation().expect("fresh observation");
    assert_eq!(observation.provider(), ProviderKind::ClaudeCode);
    assert_eq!(observation.observed_at(), 1_700_000_000_000);
    assert_eq!(observation.reset_at(), Some(1_700_000_360_000));
    assert_eq!(observation.source_version().as_str(), "2.1.0");
    assert_eq!(observation.windows().len(), 1);
    assert_eq!(observation.windows()[0].remaining_percent(), Some(80));

    let views = (0..10).map(|_| observer.view(&key)).collect::<Vec<_>>();
    for view in &views {
        assert_eq!(view.state(), QuotaState::Fresh);
        assert_eq!(view.observation(), first.observation());
    }
    assert_eq!(source.calls(), 1);
}

#[tokio::test]
async fn supported_unavailable_and_auth_required_are_distinct_states() {
    let clock = FakeClock::new(10);
    let source = SequenceSource::new(
        ProviderKind::Codex,
        [
            QuotaSourceOutcome::Supported {
                reset_at: None,
                windows: vec![QuotaWindow::new(Some(40), None).unwrap()],
            },
            QuotaSourceOutcome::Unavailable,
            QuotaSourceOutcome::Unsupported,
            QuotaSourceOutcome::AuthRequired,
        ],
    );
    let observer = observer_with_config(
        &clock,
        Arc::clone(&source) as Arc<dyn QuotaObserverSource>,
        fast_config(),
    );
    let key = cache_key(ProviderKind::Codex, "1.0.0");

    let supported = observer.refresh(&key).await;
    assert_eq!(supported.state(), QuotaState::Fresh);
    assert!(supported.observation().is_some());

    clock.advance(1);
    let unavailable = observer.refresh(&key).await;
    assert_eq!(unavailable.state(), QuotaState::Unavailable);
    assert!(unavailable.observation().is_none());

    clock.advance(1);
    let unsupported = observer.refresh(&key).await;
    assert_eq!(unsupported.state(), QuotaState::Unsupported);
    assert!(unsupported.observation().is_none());

    clock.advance(1);
    let auth_required = observer.refresh(&key).await;
    assert_eq!(auth_required.state(), QuotaState::AuthRequired);
    assert!(auth_required.observation().is_none());
    assert_eq!(source.calls(), 4);
}

#[tokio::test]
async fn refresh_jitter_delays_the_next_allowed_probe() {
    let clock = FakeClock::new(1);
    let source = SequenceSource::supported(ProviderKind::Cursor, 10, None);
    let observer = observer_with_config(
        &clock,
        Arc::clone(&source) as Arc<dyn QuotaObserverSource>,
        QuotaObserverConfig {
            timeout: Duration::from_millis(50),
            min_refresh_interval: Duration::from_millis(1_000),
            failure_backoff: Duration::from_millis(1_000),
            max_jitter: Duration::from_millis(250),
            max_cache_entries: 8,
            max_in_flight_probes: 1,
        },
    );
    observer.set_rng(Arc::new(FakeRng::new(250)));
    let key = cache_key(ProviderKind::Cursor, "0.4.0");

    assert_eq!(observer.refresh(&key).await.state(), QuotaState::Fresh);
    assert_eq!(source.calls(), 1);

    clock.advance(1_249);
    assert_eq!(observer.refresh(&key).await.state(), QuotaState::Fresh);
    assert_eq!(source.calls(), 1);

    clock.advance(1);
    assert_eq!(observer.refresh(&key).await.state(), QuotaState::Fresh);
    assert_eq!(source.calls(), 2);
}

#[tokio::test]
async fn concurrent_refresh_requests_collapse_to_one_probe() {
    let clock = FakeClock::new(5);
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let source = Arc::new(GateSource {
        kind: ProviderKind::ClaudeCode,
        calls: AtomicUsize::new(0),
        started: Arc::clone(&started),
        release: Arc::clone(&release),
        outcome: Mutex::new(Ok(QuotaSourceOutcome::Supported {
            reset_at: None,
            windows: vec![QuotaWindow::new(Some(15), None).unwrap()],
        })),
    });
    let observer = Arc::new(observer(
        &clock,
        Arc::clone(&source) as Arc<dyn QuotaObserverSource>,
    ));
    let key = cache_key(ProviderKind::ClaudeCode, "3.0.0");

    let mut joins = Vec::new();
    for _ in 0..8 {
        let observer = Arc::clone(&observer);
        let key = key.clone();
        joins.push(tokio::spawn(async move { observer.refresh(&key).await }));
    }

    loop {
        if source.calls.load(Ordering::SeqCst) >= 1 {
            break;
        }
        tokio::select! {
            _ = started.notified() => {}
            _ = tokio::time::sleep(Duration::from_millis(5)) => {}
        }
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    release.notify_waiters();

    let views = futures_util::future::join_all(joins)
        .await
        .into_iter()
        .map(|joined| joined.expect("refresh task"))
        .collect::<Vec<_>>();
    assert!(views
        .iter()
        .all(|view| view.state() == QuotaState::Fresh && view.observation().is_some()));
    assert_eq!(
        views
            .iter()
            .map(|view| view.observation().unwrap().observed_at())
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        1
    );
    assert_eq!(source.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn last_success_survives_failed_refresh_until_replaced() {
    let clock = FakeClock::new(100);
    let source = SequenceSource::new(
        ProviderKind::Codex,
        [
            Ok(QuotaSourceOutcome::Supported {
                reset_at: Some(9_000),
                windows: vec![QuotaWindow::new(Some(90), Some(9_000)).unwrap()],
            }),
            Err(QuotaSourceError::Failed),
            Ok(QuotaSourceOutcome::Supported {
                reset_at: Some(12_000),
                windows: vec![QuotaWindow::new(Some(20), Some(12_000)).unwrap()],
            }),
        ],
    );
    let observer = observer_with_config(
        &clock,
        Arc::clone(&source) as Arc<dyn QuotaObserverSource>,
        fast_config(),
    );
    let key = cache_key(ProviderKind::Codex, "1.2.3");

    let first = observer.refresh(&key).await;
    assert_eq!(first.state(), QuotaState::Fresh);
    assert_eq!(
        first.observation().unwrap().windows()[0].remaining_percent(),
        Some(90)
    );

    clock.advance(1);
    let failed = observer.refresh(&key).await;
    assert_eq!(failed.state(), QuotaState::Failed);
    assert_eq!(
        failed.observation().unwrap().windows()[0].remaining_percent(),
        Some(90)
    );
    assert!(failed.diagnostic().is_some());

    clock.advance(1);
    let replaced = observer.refresh(&key).await;
    assert_eq!(replaced.state(), QuotaState::Fresh);
    assert_eq!(
        replaced.observation().unwrap().windows()[0].remaining_percent(),
        Some(20)
    );
    assert_eq!(replaced.observation().unwrap().reset_at(), Some(12_000));
}

#[tokio::test]
async fn background_timeout_marks_failed_and_applies_backoff() {
    let clock = FakeClock::new(0);
    let source = HangSource::new();
    let observer = observer_with_config(
        &clock,
        Arc::clone(&source) as Arc<dyn QuotaObserverSource>,
        QuotaObserverConfig {
            timeout: Duration::from_millis(40),
            min_refresh_interval: Duration::from_millis(1),
            failure_backoff: Duration::from_millis(80),
            max_jitter: Duration::from_millis(0),
            max_cache_entries: 8,
            max_in_flight_probes: 1,
        },
    );
    let key = cache_key(ProviderKind::Cursor, "9.9.9");

    let refresh = {
        let observer = observer.clone();
        let key = key.clone();
        tokio::spawn(async move { observer.refresh(&key).await })
    };
    source.wait_until_calls(1).await;
    assert_eq!(observer.view(&key).state(), QuotaState::Refreshing);

    clock.advance(40);
    let timed_out = refresh.await.expect("timeout refresh");
    assert_eq!(timed_out.state(), QuotaState::Failed);
    assert!(timed_out.observation().is_none());
    assert_eq!(source.calls(), 1);

    clock.advance(79);
    assert_eq!(observer.refresh(&key).await.state(), QuotaState::Failed);
    assert_eq!(source.calls(), 1);

    clock.advance(1);
    let retry = {
        let observer = observer.clone();
        let key = key.clone();
        tokio::spawn(async move { observer.refresh(&key).await })
    };
    source.wait_until_calls(2).await;
    clock.advance(40);
    assert_eq!(retry.await.expect("retry").state(), QuotaState::Failed);
    assert_eq!(source.calls(), 2);
}

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

#[tokio::test]
async fn observer_hides_display_value_at_age_of_exactly_one_hour() {
    let clock = FakeClock::new(5_000);
    let source = SequenceSource::supported(ProviderKind::ClaudeCode, 55, None);
    let observer = observer(&clock, Arc::clone(&source) as Arc<dyn QuotaObserverSource>);
    let key = cache_key(ProviderKind::ClaudeCode, "4.0.0");

    assert!(observer.refresh(&key).await.observation().is_some());

    clock.advance(QUOTA_DISPLAY_TTL_MS - 1);
    assert!(observer.view(&key).observation().is_some());
    assert_eq!(observer.view(&key).state(), QuotaState::Fresh);

    clock.advance(1);
    let stale = observer.view(&key);
    assert!(stale.observation().is_none());
    assert_eq!(stale.state(), QuotaState::Fresh);
    assert_eq!(source.calls(), 1);
}

#[tokio::test]
async fn distinct_executable_versions_do_not_share_observations() {
    let clock = FakeClock::new(20);
    let source = SequenceSource::new(
        ProviderKind::ClaudeCode,
        [
            QuotaSourceOutcome::Supported {
                reset_at: None,
                windows: vec![QuotaWindow::new(Some(70), None).unwrap()],
            },
            QuotaSourceOutcome::Supported {
                reset_at: None,
                windows: vec![QuotaWindow::new(Some(10), None).unwrap()],
            },
        ],
    );
    let observer = observer(&clock, Arc::clone(&source) as Arc<dyn QuotaObserverSource>);
    let older = cache_key(ProviderKind::ClaudeCode, "1.0.0");
    let newer = cache_key(ProviderKind::ClaudeCode, "1.0.1");

    let first = observer.refresh(&older).await;
    let second = observer.refresh(&newer).await;
    assert_eq!(
        first.observation().unwrap().windows()[0].remaining_percent(),
        Some(70)
    );
    assert_eq!(
        second.observation().unwrap().windows()[0].remaining_percent(),
        Some(10)
    );
    assert_eq!(source.calls(), 2);
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
            Path::new("C:/bin/claude.exe"),
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
async fn view_never_probes_on_the_hot_path() {
    let clock = FakeClock::new(1);
    let source = SequenceSource::supported(ProviderKind::Codex, 1, None);
    let observer = observer(&clock, Arc::clone(&source) as Arc<dyn QuotaObserverSource>);
    let key = cache_key(ProviderKind::Codex, "0.1.0");

    let untouched = observer.view(&key);
    assert!(untouched.observation().is_none());
    assert_eq!(source.calls(), 0);
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
fn production_jitter_is_bounded_and_zero_when_uncapped_max_is_zero() {
    let jitter = ProductionJitter;
    assert_eq!(jitter.jitter_ms(0).expect("zero jitter"), 0);
    for _ in 0..8 {
        assert!(jitter.jitter_ms(7).expect("entropy") <= 7);
    }
}

#[tokio::test]
async fn bounded_cache_evicts_oldest_idle_key_deterministically() {
    let clock = FakeClock::new(100);
    let source = SequenceSource::supported(ProviderKind::ClaudeCode, 50, None);
    let mut config = observer_config();
    config.max_cache_entries = 2;
    let observer = observer_with_config(
        &clock,
        Arc::clone(&source) as Arc<dyn QuotaObserverSource>,
        config,
    );
    let first = cache_key(ProviderKind::ClaudeCode, "1.0.0");
    let second = cache_key(ProviderKind::ClaudeCode, "1.0.1");
    let third = cache_key(ProviderKind::ClaudeCode, "1.0.2");

    assert_eq!(observer.refresh(&first).await.state(), QuotaState::Fresh);
    clock.advance(1);
    assert_eq!(observer.refresh(&second).await.state(), QuotaState::Fresh);
    clock.advance(1);
    assert_eq!(observer.refresh(&third).await.state(), QuotaState::Fresh);

    assert_eq!(observer.cache_len(), 2);
    assert!(observer.view(&first).observation().is_none());
    assert!(observer.view(&second).observation().is_some());
    assert!(observer.view(&third).observation().is_some());
}

#[tokio::test]
async fn global_probe_limiter_serializes_distinct_keys() {
    let clock = FakeClock::new(1);
    let started_a = Arc::new(Notify::new());
    let release_a = Arc::new(Notify::new());
    let started_b = Arc::new(Notify::new());
    let release_b = Arc::new(Notify::new());
    let source_a = Arc::new(GateSource {
        kind: ProviderKind::ClaudeCode,
        calls: AtomicUsize::new(0),
        started: Arc::clone(&started_a),
        release: Arc::clone(&release_a),
        outcome: Mutex::new(Ok(QuotaSourceOutcome::Supported {
            reset_at: None,
            windows: vec![QuotaWindow::new(Some(1), None).unwrap()],
        })),
    });
    let source_b = Arc::new(GateSource {
        kind: ProviderKind::Codex,
        calls: AtomicUsize::new(0),
        started: Arc::clone(&started_b),
        release: Arc::clone(&release_b),
        outcome: Mutex::new(Ok(QuotaSourceOutcome::Supported {
            reset_at: None,
            windows: vec![QuotaWindow::new(Some(2), None).unwrap()],
        })),
    });
    let limiter = QuotaProbeLimiter::new(1).expect("limiter");
    let observer_a = QuotaObserver::with_limiter(
        Arc::new(clock.clone()),
        Arc::clone(&source_a) as Arc<dyn QuotaObserverSource>,
        observer_config(),
        limiter.clone(),
    )
    .expect("observer a");
    let observer_b = QuotaObserver::with_limiter(
        Arc::new(clock.clone()),
        Arc::clone(&source_b) as Arc<dyn QuotaObserverSource>,
        observer_config(),
        limiter,
    )
    .expect("observer b");
    let key_a = cache_key(ProviderKind::ClaudeCode, "a");
    let key_b = cache_key(ProviderKind::Codex, "b");

    let first = {
        let observer_a = observer_a.clone();
        let key_a = key_a.clone();
        tokio::spawn(async move { observer_a.refresh(&key_a).await })
    };
    loop {
        if source_a.calls.load(Ordering::SeqCst) >= 1 {
            break;
        }
        tokio::select! {
            _ = started_a.notified() => {}
            _ = tokio::time::sleep(Duration::from_millis(5)) => {}
        }
    }
    let second = {
        let observer_b = observer_b.clone();
        let key_b = key_b.clone();
        tokio::spawn(async move { observer_b.refresh(&key_b).await })
    };
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(source_b.calls.load(Ordering::SeqCst), 0);

    release_a.notify_waiters();
    first.await.expect("first");
    loop {
        if source_b.calls.load(Ordering::SeqCst) >= 1 {
            break;
        }
        tokio::select! {
            _ = started_b.notified() => {}
            _ = tokio::time::sleep(Duration::from_millis(5)) => {}
        }
    }
    release_b.notify_waiters();
    assert_eq!(second.await.expect("second").state(), QuotaState::Fresh);
}

#[tokio::test]
async fn cancelled_leader_settles_waiters_and_allows_retry() {
    let clock = FakeClock::new(0);
    let source = HangSource::new();
    let observer = observer_with_config(
        &clock,
        Arc::clone(&source) as Arc<dyn QuotaObserverSource>,
        QuotaObserverConfig {
            timeout: Duration::from_millis(5_000),
            min_refresh_interval: Duration::from_millis(0),
            failure_backoff: Duration::from_millis(0),
            max_jitter: Duration::from_millis(0),
            max_cache_entries: 8,
            max_in_flight_probes: 1,
        },
    );
    let key = cache_key(ProviderKind::Cursor, "9.9.9");
    let leader = {
        let observer = observer.clone();
        let key = key.clone();
        tokio::spawn(async move { observer.refresh(&key).await })
    };
    source.wait_until_calls(1).await;
    let waiter = {
        let observer = observer.clone();
        let key = key.clone();
        tokio::spawn(async move { observer.refresh(&key).await })
    };
    tokio::task::yield_now().await;
    leader.abort();
    let settled = tokio::time::timeout(Duration::from_secs(1), waiter)
        .await
        .expect("waiter settled")
        .expect("waiter task");
    assert_eq!(settled.state(), QuotaState::Failed);
    assert_eq!(settled.diagnostic(), Some(QuotaDiagnostic::Cancelled));

    let retry = {
        let observer = observer.clone();
        let key = key.clone();
        tokio::spawn(async move { observer.refresh(&key).await })
    };
    source.wait_until_calls(2).await;
    retry.abort();
    assert_eq!(source.calls(), 2);
}

#[tokio::test]
async fn source_kind_mismatch_does_not_probe() {
    let clock = FakeClock::new(3);
    let source = SequenceSource::supported(ProviderKind::ClaudeCode, 10, None);
    let observer = observer(&clock, Arc::clone(&source) as Arc<dyn QuotaObserverSource>);
    let key = cache_key(ProviderKind::Codex, "1.0.0");
    let view = observer.refresh(&key).await;
    assert_eq!(view.state(), QuotaState::Failed);
    assert_eq!(view.diagnostic(), Some(QuotaDiagnostic::KindMismatch));
    assert_eq!(source.calls(), 0);
}

#[tokio::test]
async fn adapter_source_maps_auth_required_and_unsupported_without_fabricating_windows() {
    let auth = AdapterQuotaSource::new(Arc::new(OutcomeAdapter {
        outcome: AdapterQuotaOutcome::AuthRequired,
    }));
    assert_eq!(
        auth.observe_quota(
            Path::new("C:/bin/claude.exe"),
            &ProviderVersion::new("1").unwrap()
        )
        .await
        .unwrap(),
        QuotaSourceOutcome::AuthRequired
    );

    let unsupported = AdapterQuotaSource::new(Arc::new(OutcomeAdapter {
        outcome: AdapterQuotaOutcome::Unsupported,
    }));
    assert_eq!(
        unsupported
            .observe_quota(
                Path::new("C:/bin/claude.exe"),
                &ProviderVersion::new("1").unwrap()
            )
            .await
            .unwrap(),
        QuotaSourceOutcome::Unsupported
    );
}

#[tokio::test]
async fn host_schedules_one_observer_per_kind_and_hides_stale_top_bar_metadata() {
    let clock = FakeClock::new(1_000);
    let claude = SequenceSource::supported(ProviderKind::ClaudeCode, 12, None);
    let codex = SequenceSource::supported(ProviderKind::Codex, 44, None);
    let host = ProviderQuotaHost::new(Arc::new(clock.clone()), observer_config()).expect("host");
    host.register(Arc::clone(&claude) as Arc<dyn QuotaObserverSource>)
        .expect("register claude");
    host.register(Arc::clone(&codex) as Arc<dyn QuotaObserverSource>)
        .expect("register codex");
    assert!(host
        .register(SequenceSource::supported(ProviderKind::ClaudeCode, 1, None)
            as Arc<dyn QuotaObserverSource>)
        .is_err());

    let claude_key = cache_key(ProviderKind::ClaudeCode, "2.0.0");
    host.tick(&[claude_key.clone(), claude_key.clone()]).await;
    assert_eq!(claude.calls(), 1);
    assert_eq!(codex.calls(), 0);

    let strip = host.project_top_bar();
    assert_eq!(strip.len(), 2);
    let claude_row = strip
        .iter()
        .find(|row| row.provider() == ProviderKind::ClaudeCode)
        .expect("claude row");
    assert!(claude_row.observation().is_some());
    assert_eq!(
        strip
            .iter()
            .find(|row| row.provider() == ProviderKind::Codex)
            .expect("codex row")
            .observation()
            .map(|observation| observation.windows()[0].remaining_percent()),
        None
    );

    clock.advance(QUOTA_DISPLAY_TTL_MS);
    let hidden = host.project_top_bar();
    assert!(hidden
        .iter()
        .find(|row| row.provider() == ProviderKind::ClaudeCode)
        .expect("claude row")
        .observation()
        .is_none());

    clock.set(1_000);
    let resurrected = host.project_top_bar();
    assert!(
        resurrected
            .iter()
            .find(|row| row.provider() == ProviderKind::ClaudeCode)
            .expect("claude row")
            .observation()
            .is_none(),
        "clock rollback must not resurrect a TTL-sealed observation"
    );
}

#[test]
fn omit_stale_hides_future_and_clock_rollback_observations() {
    assert!(omit_stale_quota_display(2_000, 1_000, "future").is_none());
    assert!(omit_stale_quota_display(5_000, 4_999, "rollback").is_none());
    assert!(omit_stale_quota_display(1_000, 1_000, "now").is_some());
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
fn production_jitter_is_unbiased_and_fails_closed_without_entropy() {
    assert_eq!(
        unbiased_jitter_from_bits(0, 0).unwrap(),
        0,
        "zero cap stays zero"
    );
    assert!(unbiased_jitter_from_bits(u64::MAX, 2).is_none());
    assert_eq!(unbiased_jitter_from_bits(0, 2).unwrap(), 0);
    assert_eq!(unbiased_jitter_from_bits(1, 2).unwrap(), 1);
    assert_eq!(unbiased_jitter_from_bits(2, 2).unwrap(), 2);

    let jitter = ProductionJitter;
    assert_eq!(jitter.jitter_ms(0).expect("zero jitter"), 0);

    let failing = FailingEntropy;
    assert_eq!(
        failing.jitter_ms(7),
        Err(QuotaJitterError::EntropyUnavailable)
    );
}

#[tokio::test]
async fn entropy_failure_is_visible_and_does_not_drop_last_good() {
    let clock = FakeClock::new(10);
    let source = SequenceSource::supported(ProviderKind::Codex, 40, None);
    let observer = observer(&clock, Arc::clone(&source) as Arc<dyn QuotaObserverSource>);
    let key = cache_key(ProviderKind::Codex, "1.0.0");
    assert_eq!(observer.refresh(&key).await.state(), QuotaState::Fresh);

    observer.set_rng(Arc::new(FailingEntropy));
    clock.advance(1);
    let failed = observer.refresh(&key).await;
    assert_eq!(failed.state(), QuotaState::Failed);
    assert_eq!(
        failed.diagnostic(),
        Some(QuotaDiagnostic::EntropyUnavailable)
    );
    assert_eq!(
        failed
            .observation()
            .and_then(|observation| observation.windows()[0].remaining_percent()),
        Some(40)
    );
}

#[tokio::test]
async fn eviction_skips_in_flight_entries() {
    let clock = FakeClock::new(100);
    let source = Arc::new(SelectiveHangSource::new(ProviderKind::ClaudeCode, "hang"));
    let mut config = observer_config();
    config.max_cache_entries = 2;
    config.max_in_flight_probes = 3;
    let observer = observer_with_config(
        &clock,
        Arc::clone(&source) as Arc<dyn QuotaObserverSource>,
        config,
    );
    let in_flight = cache_key(ProviderKind::ClaudeCode, "hang");
    let idle = cache_key(ProviderKind::ClaudeCode, "idle");
    let newer = cache_key(ProviderKind::ClaudeCode, "newer");

    let leader = {
        let observer = observer.clone();
        let in_flight = in_flight.clone();
        tokio::spawn(async move { observer.refresh(&in_flight).await })
    };
    source.wait_until_calls(1).await;
    clock.advance(1);
    assert_eq!(observer.refresh(&idle).await.state(), QuotaState::Fresh);
    clock.advance(1);
    assert_eq!(observer.refresh(&newer).await.state(), QuotaState::Fresh);

    assert_eq!(observer.cache_len(), 2);
    assert_eq!(observer.view(&in_flight).state(), QuotaState::Refreshing);
    assert!(observer.view(&idle).observation().is_none());
    assert!(observer.view(&newer).observation().is_some());
    leader.abort();
}

#[tokio::test]
async fn all_in_flight_cap_returns_typed_cache_full_without_growth() {
    let clock = FakeClock::new(1);
    let source = HangSource::new();
    let mut config = observer_config();
    config.max_cache_entries = 2;
    config.max_in_flight_probes = 2;
    config.timeout = Duration::from_secs(5);
    let observer = observer_with_config(
        &clock,
        Arc::clone(&source) as Arc<dyn QuotaObserverSource>,
        config,
    );
    let first = cache_key(ProviderKind::Cursor, "a");
    let second = cache_key(ProviderKind::Cursor, "b");
    let third = cache_key(ProviderKind::Cursor, "c");

    let leader_a = {
        let observer = observer.clone();
        let first = first.clone();
        tokio::spawn(async move { observer.refresh(&first).await })
    };
    source.wait_until_calls(1).await;
    let leader_b = {
        let observer = observer.clone();
        let second = second.clone();
        tokio::spawn(async move { observer.refresh(&second).await })
    };
    source.wait_until_calls(2).await;

    let full = observer.refresh(&third).await;
    assert_eq!(full.state(), QuotaState::Failed);
    assert_eq!(full.diagnostic(), Some(QuotaDiagnostic::CacheFull));
    assert_eq!(observer.cache_len(), 2);
    assert!(observer.view(&third).observation().is_none());
    assert_eq!(observer.view(&first).state(), QuotaState::Refreshing);
    assert_eq!(observer.view(&second).state(), QuotaState::Refreshing);
    leader_a.abort();
    leader_b.abort();
}

#[tokio::test]
async fn thirty_three_unique_keys_stay_bounded_to_per_observer_max() {
    let clock = FakeClock::new(1);
    let source = SequenceSource::supported(ProviderKind::Codex, 9, None);
    let mut config = observer_config();
    config.max_cache_entries = 32;
    let observer = observer_with_config(
        &clock,
        Arc::clone(&source) as Arc<dyn QuotaObserverSource>,
        config,
    );
    for index in 0..33 {
        let key = cache_key(ProviderKind::Codex, &format!("1.0.{index}"));
        assert_eq!(observer.refresh(&key).await.state(), QuotaState::Fresh);
        clock.advance(1);
    }
    assert_eq!(observer.cache_len(), 32);
    assert!(observer
        .view(&cache_key(ProviderKind::Codex, "1.0.0"))
        .observation()
        .is_none());
    assert!(observer
        .view(&cache_key(ProviderKind::Codex, "1.0.32"))
        .observation()
        .is_some());
}

#[tokio::test]
async fn host_cache_bound_is_per_kind_with_one_observer_per_kind() {
    let clock = FakeClock::new(1);
    let claude = SequenceSource::supported(ProviderKind::ClaudeCode, 1, None);
    let codex = SequenceSource::supported(ProviderKind::Codex, 2, None);
    let mut config = observer_config();
    config.max_cache_entries = 2;
    let host = ProviderQuotaHost::new(Arc::new(clock.clone()), config).expect("host");
    host.register(Arc::clone(&claude) as Arc<dyn QuotaObserverSource>)
        .expect("claude");
    host.register(Arc::clone(&codex) as Arc<dyn QuotaObserverSource>)
        .expect("codex");
    assert!(host
        .register(SequenceSource::supported(ProviderKind::ClaudeCode, 3, None)
            as Arc<dyn QuotaObserverSource>)
        .is_err());

    host.tick(&[
        cache_key(ProviderKind::ClaudeCode, "a"),
        cache_key(ProviderKind::ClaudeCode, "b"),
        cache_key(ProviderKind::ClaudeCode, "c"),
        cache_key(ProviderKind::Codex, "a"),
        cache_key(ProviderKind::Codex, "b"),
    ])
    .await;

    assert_eq!(host.cache_len(ProviderKind::ClaudeCode), 2);
    assert_eq!(host.cache_len(ProviderKind::Codex), 2);
    assert_eq!(host.cache_len_total(), 4);
}

#[tokio::test]
async fn auth_required_and_unsupported_clear_displayed_numbers() {
    let clock = FakeClock::new(20);
    let source = SequenceSource::new(
        ProviderKind::ClaudeCode,
        [
            QuotaSourceOutcome::Supported {
                reset_at: None,
                windows: vec![QuotaWindow::new(Some(77), None).unwrap()],
            },
            QuotaSourceOutcome::AuthRequired,
            QuotaSourceOutcome::Unsupported,
        ],
    );
    let observer = observer(&clock, Arc::clone(&source) as Arc<dyn QuotaObserverSource>);
    let key = cache_key(ProviderKind::ClaudeCode, "3.0.0");

    let fresh = observer.refresh(&key).await;
    assert_eq!(
        fresh
            .observation()
            .and_then(|observation| observation.windows()[0].remaining_percent()),
        Some(77)
    );

    clock.advance(1);
    let auth = observer.refresh(&key).await;
    assert_eq!(auth.state(), QuotaState::AuthRequired);
    assert!(auth.observation().is_none());

    clock.advance(1);
    let unsupported = observer.refresh(&key).await;
    assert_eq!(unsupported.state(), QuotaState::Unsupported);
    assert!(unsupported.observation().is_none());
}

#[tokio::test]
async fn top_bar_winner_uses_successful_observation_identity_not_view_lru() {
    let clock = FakeClock::new(1_000);
    let source = SequenceSource::supported(ProviderKind::Codex, 15, None);
    let host = ProviderQuotaHost::new(Arc::new(clock.clone()), observer_config()).expect("host");
    host.register(Arc::clone(&source) as Arc<dyn QuotaObserverSource>)
        .expect("register");
    let older = cache_key(ProviderKind::Codex, "1.0.0");
    let newer = cache_key(ProviderKind::Codex, "1.0.1");

    host.tick(&[older.clone()]).await;
    clock.advance(50);
    host.tick(&[newer.clone()]).await;
    clock.advance(50);
    assert!(host.view(&older).observation().is_some());

    let strip = host.project_top_bar();
    let winner = strip
        .iter()
        .find(|row| row.provider() == ProviderKind::Codex)
        .and_then(|row| row.observation())
        .expect("codex winner");
    assert_eq!(winner.source_version().as_str(), "1.0.1");
    assert_eq!(winner.observed_at(), 1_050);

    let from_host = canonical_top_bar(&host);
    assert_eq!(from_host.entries(), strip.as_slice());
    let from_entries = canonical_top_bar(strip.clone());
    assert_eq!(from_entries.entries(), strip.as_slice());
}

#[tokio::test]
async fn refresh_rejects_clock_rollback_observations_and_view_hides_future() {
    let clock = FakeClock::new(2_000);
    let source = SequenceSource::new(
        ProviderKind::Cursor,
        [
            QuotaSourceOutcome::Supported {
                reset_at: None,
                windows: vec![QuotaWindow::new(Some(10), None).unwrap()],
            },
            QuotaSourceOutcome::Supported {
                reset_at: None,
                windows: vec![QuotaWindow::new(Some(99), None).unwrap()],
            },
        ],
    );
    let observer = observer(&clock, Arc::clone(&source) as Arc<dyn QuotaObserverSource>);
    let key = cache_key(ProviderKind::Cursor, "4.0.0");

    assert_eq!(observer.refresh(&key).await.state(), QuotaState::Fresh);
    clock.set(1_500);
    assert!(
        observer.view(&key).observation().is_none(),
        "now behind observed_at must hide displayed numbers"
    );

    let rolled = observer.refresh(&key).await;
    assert_eq!(rolled.state(), QuotaState::Failed);
    assert_eq!(
        rolled.diagnostic(),
        Some(QuotaDiagnostic::InvalidObservation)
    );
    assert!(
        rolled.observation().is_none(),
        "now behind observed_at must hide displayed numbers"
    );

    clock.set(2_000);
    assert_eq!(
        observer
            .view(&key)
            .observation()
            .and_then(|observation| observation.windows()[0].remaining_percent()),
        Some(10),
        "Failed may retain last-good; rollback must not replace or seal it"
    );
}

fn observer_config() -> QuotaObserverConfig {
    QuotaObserverConfig {
        timeout: Duration::from_millis(50),
        min_refresh_interval: Duration::from_millis(0),
        failure_backoff: Duration::from_millis(0),
        max_jitter: Duration::from_millis(0),
        max_cache_entries: 8,
        max_in_flight_probes: 1,
    }
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

fn observer(clock: &FakeClock, source: Arc<dyn QuotaObserverSource>) -> QuotaObserver {
    observer_with_config(
        clock,
        source,
        QuotaObserverConfig {
            timeout: Duration::from_millis(50),
            min_refresh_interval: Duration::from_millis(0),
            failure_backoff: Duration::from_millis(0),
            max_jitter: Duration::from_millis(0),
            max_cache_entries: 8,
            max_in_flight_probes: 1,
        },
    )
}

fn observer_with_config(
    clock: &FakeClock,
    source: Arc<dyn QuotaObserverSource>,
    config: QuotaObserverConfig,
) -> QuotaObserver {
    QuotaObserver::new(Arc::new(clock.clone()), source, config).expect("valid quota config")
}

fn cache_key(kind: ProviderKind, version: &str) -> QuotaCacheKey {
    QuotaCacheKey::new(
        kind,
        ProviderExecutable::new(PathBuf::from("C:/bin/provider.exe"), [0x42; 32]).unwrap(),
        ProviderVersion::new(version).unwrap(),
    )
}

struct SequenceSource {
    kind: ProviderKind,
    calls: AtomicUsize,
    outcomes: Mutex<VecDeque<Result<QuotaSourceOutcome, QuotaSourceError>>>,
    last: Mutex<Option<Result<QuotaSourceOutcome, QuotaSourceError>>>,
}

impl SequenceSource {
    fn new<I, T>(kind: ProviderKind, outcomes: I) -> Arc<Self>
    where
        I: IntoIterator<Item = T>,
        T: Into<SequenceOutcome>,
    {
        Arc::new(Self {
            kind,
            calls: AtomicUsize::new(0),
            outcomes: Mutex::new(
                outcomes
                    .into_iter()
                    .map(Into::into)
                    .map(|item| item.0)
                    .collect(),
            ),
            last: Mutex::new(None),
        })
    }

    fn supported(kind: ProviderKind, remaining_percent: u8, reset_at: Option<u64>) -> Arc<Self> {
        Self::new(
            kind,
            [QuotaSourceOutcome::Supported {
                reset_at,
                windows: vec![QuotaWindow::new(Some(remaining_percent), reset_at).unwrap()],
            }],
        )
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

struct SequenceOutcome(Result<QuotaSourceOutcome, QuotaSourceError>);

impl From<QuotaSourceOutcome> for SequenceOutcome {
    fn from(outcome: QuotaSourceOutcome) -> Self {
        Self(Ok(outcome))
    }
}

impl From<Result<QuotaSourceOutcome, QuotaSourceError>> for SequenceOutcome {
    fn from(outcome: Result<QuotaSourceOutcome, QuotaSourceError>) -> Self {
        Self(outcome)
    }
}

#[async_trait]
impl QuotaObserverSource for SequenceSource {
    fn kind(&self) -> ProviderKind {
        self.kind
    }

    async fn observe_quota(
        &self,
        _executable: &Path,
        _version: &ProviderVersion,
    ) -> Result<QuotaSourceOutcome, QuotaSourceError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let next = self.outcomes.lock().unwrap().pop_front();
        if let Some(next) = next {
            *self.last.lock().unwrap() = Some(next.clone());
            next
        } else {
            self.last
                .lock()
                .unwrap()
                .clone()
                .unwrap_or(Ok(QuotaSourceOutcome::Unavailable))
        }
    }
}

struct GateSource {
    kind: ProviderKind,
    calls: AtomicUsize,
    started: Arc<Notify>,
    release: Arc<Notify>,
    outcome: Mutex<Result<QuotaSourceOutcome, QuotaSourceError>>,
}

#[async_trait]
impl QuotaObserverSource for GateSource {
    fn kind(&self) -> ProviderKind {
        self.kind
    }

    async fn observe_quota(
        &self,
        _executable: &Path,
        _version: &ProviderVersion,
    ) -> Result<QuotaSourceOutcome, QuotaSourceError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.started.notify_waiters();
        self.release.notified().await;
        self.outcome.lock().unwrap().clone()
    }
}

struct FailingEntropy;

impl QuotaRng for FailingEntropy {
    fn jitter_ms(&self, _max_inclusive: u64) -> Result<u64, QuotaJitterError> {
        Err(QuotaJitterError::EntropyUnavailable)
    }
}

struct SelectiveHangSource {
    kind: ProviderKind,
    hang_version: String,
    calls: AtomicUsize,
    started: Notify,
}

impl SelectiveHangSource {
    fn new(kind: ProviderKind, hang_version: &str) -> Self {
        Self {
            kind,
            hang_version: hang_version.to_string(),
            calls: AtomicUsize::new(0),
            started: Notify::new(),
        }
    }

    async fn wait_until_calls(&self, expected: usize) {
        loop {
            if self.calls.load(Ordering::SeqCst) >= expected {
                return;
            }
            tokio::select! {
                _ = self.started.notified() => {}
                _ = tokio::time::sleep(Duration::from_millis(5)) => {}
            }
        }
    }
}

#[async_trait]
impl QuotaObserverSource for SelectiveHangSource {
    fn kind(&self) -> ProviderKind {
        self.kind
    }

    async fn observe_quota(
        &self,
        _executable: &Path,
        version: &ProviderVersion,
    ) -> Result<QuotaSourceOutcome, QuotaSourceError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.started.notify_waiters();
        if version.as_str() == self.hang_version {
            std::future::pending().await
        } else {
            Ok(QuotaSourceOutcome::Supported {
                reset_at: None,
                windows: vec![QuotaWindow::new(Some(50), None).unwrap()],
            })
        }
    }
}

struct HangSource {
    calls: AtomicUsize,
    started: Notify,
}

impl HangSource {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            started: Notify::new(),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    async fn wait_until_calls(&self, expected: usize) {
        loop {
            if self.calls() >= expected {
                return;
            }
            tokio::select! {
                _ = self.started.notified() => {}
                _ = tokio::time::sleep(Duration::from_millis(5)) => {}
            }
        }
    }
}

#[async_trait]
impl QuotaObserverSource for HangSource {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Cursor
    }

    async fn observe_quota(
        &self,
        _executable: &Path,
        _version: &ProviderVersion,
    ) -> Result<QuotaSourceOutcome, QuotaSourceError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.started.notify_waiters();
        std::future::pending().await
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

    async fn probe(&self, _executable: &Path) -> Result<ProviderCapabilities, ProviderError> {
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

    fn parse_signal(&self, _signal: ProviderSignal) -> Vec<JournalEvent> {
        Vec::new()
    }

    fn cooperative_stop(&self, _session: &ProviderRuntime) -> StopStrategy {
        StopStrategy::Unsupported
    }

    async fn observe_quota(
        &self,
        _executable: &Path,
    ) -> Result<AdapterQuotaOutcome, ProviderError> {
        Ok(AdapterQuotaOutcome::Observed(self.sample))
    }
}

struct OutcomeAdapter {
    outcome: AdapterQuotaOutcome,
}

#[async_trait]
impl ProviderAdapter for OutcomeAdapter {
    fn kind(&self) -> ProviderKind {
        ProviderKind::ClaudeCode
    }

    async fn probe(&self, _executable: &Path) -> Result<ProviderCapabilities, ProviderError> {
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

    fn parse_signal(&self, _signal: ProviderSignal) -> Vec<JournalEvent> {
        Vec::new()
    }

    fn cooperative_stop(&self, _session: &ProviderRuntime) -> StopStrategy {
        StopStrategy::Unsupported
    }

    async fn observe_quota(
        &self,
        _executable: &Path,
    ) -> Result<AdapterQuotaOutcome, ProviderError> {
        Ok(self.outcome)
    }
}
