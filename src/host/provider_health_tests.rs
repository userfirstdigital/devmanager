//! Behavioral regressions for host-owned provider settings health (written, not executed).

use std::sync::Arc;

use crate::providers::settings::{
    ProviderProfileOwner, ProviderSettingsAuthority, ProviderSettingsHostRequest,
    ProviderSettingsMutation, ProviderSettingsQuery, ProviderSettingsReply,
    DEFAULT_HEALTH_INTERVAL_SECS,
};
use tempfile::tempdir;

use super::provider_health;

#[test]
fn health_manual_zero_refuses_schedule_without_force() {
    let dir = tempdir().unwrap();
    let profile = ProviderProfileOwner::open_dir_for_test(dir.path()).unwrap();
    let authority = Arc::new(ProviderSettingsAuthority::from_profile(profile));
    let rev = authority.snapshot().revision;
    authority
        .mutate(ProviderSettingsMutation::SetHealthInterval {
            expected_revision: rev,
            interval_secs: 0,
        })
        .unwrap();
    assert!(!authority.health_job().should_schedule());
    assert!(provider_health::try_begin_health_job(&authority, None, false).is_none());
    let started = provider_health::try_begin_health_job(&authority, None, true);
    assert!(started.is_some());
}

#[test]
fn health_reentry_refused_until_finish() {
    let dir = tempdir().unwrap();
    let profile = ProviderProfileOwner::open_dir_for_test(dir.path()).unwrap();
    let authority = Arc::new(ProviderSettingsAuthority::from_profile(profile));
    let first = provider_health::try_begin_health_job(&authority, None, true);
    assert!(first.is_some());
    assert!(provider_health::try_begin_health_job(&authority, None, true).is_none());
}

#[test]
fn unpolled_health_future_drop_releases_active_guard() {
    let dir = tempdir().unwrap();
    let profile = ProviderProfileOwner::open_dir_for_test(dir.path()).unwrap();
    let authority = Arc::new(ProviderSettingsAuthority::from_profile(profile.clone()));
    let started = provider_health::try_begin_health_job(&authority, None, true).expect("started");
    assert!(profile.health.is_refresh_in_flight());
    // Drop before first poll: DropGuard constructed before Box::pin must release.
    drop(started);
    assert!(!profile.health.is_refresh_in_flight());
    assert!(provider_health::try_begin_health_job(&authority, None, true).is_some());
}

#[test]
fn started_health_future_cancel_also_releases_active_guard() {
    let dir = tempdir().unwrap();
    let profile = ProviderProfileOwner::open_dir_for_test(dir.path()).unwrap();
    let authority = Arc::new(ProviderSettingsAuthority::from_profile(profile.clone()));
    let (_generation, future) =
        provider_health::try_begin_health_job(&authority, None, true).expect("started");
    assert!(profile.health.is_refresh_in_flight());
    // Future was constructed (owned) then cancelled without completing probes.
    drop(future);
    assert!(!profile.health.is_refresh_in_flight());
    assert!(provider_health::try_begin_health_job(&authority, None, true).is_some());
}

#[test]
fn agent_connection_cache_query_remains_responsive_while_health_pending() {
    let dir = tempdir().unwrap();
    let profile = ProviderProfileOwner::open_dir_for_test(dir.path()).unwrap();
    let authority = Arc::new(ProviderSettingsAuthority::from_profile(profile.clone()));
    let started = provider_health::try_begin_health_job(&authority, None, true).expect("health");
    assert!(profile.health.is_refresh_in_flight());
    // Legacy AgentConnection path is now a sync cache projection; it must not
    // wait for the pending health future (or any CLI probe) to return.
    let snapshot =
        super::agent_connection::project_agent_connection_from_authority(&authority, Vec::new());
    assert_eq!(snapshot.agents.len(), 2);
    let taskish = authority.query(ProviderSettingsQuery::Snapshot).unwrap();
    assert!(matches!(taskish, ProviderSettingsReply::Snapshot(_)));
    drop(started);
    assert!(!profile.health.is_refresh_in_flight());
}

#[test]
fn cursor_nonzero_exit_never_treated_as_healthy_even_with_positive_email() {
    use crate::providers::adapter::ProviderProbeStatus;
    let status = provider_health::cursor_about_health_from_probe_status(
        ProviderProbeStatus::NonZeroExit,
        b"ok",
    );
    assert!(
        status.is_err(),
        "NonZeroExit without unsupported-format must fail"
    );
    let fallback = provider_health::cursor_about_health_from_probe_status(
        ProviderProbeStatus::NonZeroExit,
        b"Error: unsupported format json",
    )
    .expect("unsupported format may fall back");
    assert_eq!(fallback, provider_health::CursorAboutFallback::UsePlain);
    let completed =
        provider_health::cursor_about_health_from_probe_status(ProviderProbeStatus::Completed, b"")
            .expect("completed");
    assert_eq!(completed, provider_health::CursorAboutFallback::UseJson);
}

#[test]
fn stale_config_revision_ignored_by_job_owner() {
    let dir = tempdir().unwrap();
    let profile = ProviderProfileOwner::open_dir_for_test(dir.path()).unwrap();
    let authority = ProviderSettingsAuthority::from_profile(profile);
    let gen = authority.health_job().try_begin_manual_refresh().unwrap();
    let old_rev = authority.profile().settings.snapshot().revision;
    authority.health_job().note_config_revision(old_rev + 1);
    assert!(authority.health_job().is_stale_config(old_rev));
    authority.health_job().finish_refresh(gen, None);
}

#[test]
fn host_request_snapshot_is_cache_only() {
    let dir = tempdir().unwrap();
    let profile = ProviderProfileOwner::open_dir_for_test(dir.path()).unwrap();
    let authority = ProviderSettingsAuthority::from_profile(profile);
    match authority.query(ProviderSettingsQuery::Snapshot).unwrap() {
        ProviderSettingsReply::Snapshot(snap) => {
            assert_eq!(snap.health_interval_secs, DEFAULT_HEALTH_INTERVAL_SECS);
            assert!(!snap.health_in_flight);
        }
        other => panic!("unexpected {other:?}"),
    }
    match authority
        .query(ProviderSettingsQuery::Refresh { force: true })
        .unwrap()
    {
        ProviderSettingsReply::RefreshStarted { generation } => {
            assert!(generation > 0);
            authority.health_job().finish_refresh(generation, None);
        }
        other => panic!("unexpected {other:?}"),
    }
    let _ = ProviderSettingsHostRequest::Snapshot;
}

#[test]
fn agent_connection_projection_is_sync_from_settings_cache() {
    let dir = tempdir().unwrap();
    let profile = ProviderProfileOwner::open_dir_for_test(dir.path()).unwrap();
    let authority = ProviderSettingsAuthority::from_profile(profile);
    let snapshot =
        super::agent_connection::project_agent_connection_from_authority(&authority, Vec::new());
    assert_eq!(snapshot.agents.len(), 2);
    // Sync projection: never blocks on CLI probes.
    assert!(snapshot.agents.iter().all(|row| matches!(
        row.presence,
        crate::domain::AgentPresence::NotSignedIn
            | crate::domain::AgentPresence::SignedIn
            | crate::domain::AgentPresence::NotFound
            | crate::domain::AgentPresence::CheckFailed
            | crate::domain::AgentPresence::Checking
    )));
}

#[test]
fn default_interval_constant_is_300() {
    assert_eq!(DEFAULT_HEALTH_INTERVAL_SECS, 300);
    assert_eq!(provider_health::default_health_interval_secs(), 300);
}
