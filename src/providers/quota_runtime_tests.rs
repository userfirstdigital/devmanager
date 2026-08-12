use super::{NativeQuotaHost, QuotaRuntimeConfig, QUOTA_RUNTIME_SHUTDOWN_TIMEOUT};
use crate::providers::capabilities::ProviderKind;
use crate::providers::quota::QuotaState;
use std::time::Duration;

#[tokio::test(flavor = "current_thread")]
async fn stock_runtime_projects_registered_kinds_without_inventing_values() {
    let runtime = NativeQuotaHost::start_stock(QuotaRuntimeConfig {
        refresh_interval: Duration::from_secs(60 * 60),
        ..QuotaRuntimeConfig::production()
    })
    .expect("stock quota runtime");

    let projection = runtime.top_bar();
    let entries = projection.entries();
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[0].provider(), ProviderKind::ClaudeCode);
    assert_eq!(entries[1].provider(), ProviderKind::Codex);
    assert_eq!(entries[2].provider(), ProviderKind::Cursor);
    assert!(entries.iter().all(|entry| entry.observation().is_none()));
    assert!(entries
        .iter()
        .all(|entry| matches!(entry.state(), QuotaState::Unavailable)));

    runtime.shutdown().await.expect("quota runtime shutdown");
}

#[tokio::test(flavor = "current_thread")]
async fn stock_runtime_shutdown_is_bounded_and_releases_task() {
    let runtime = NativeQuotaHost::start_stock(QuotaRuntimeConfig {
        refresh_interval: Duration::from_millis(1),
        ..QuotaRuntimeConfig::production()
    })
    .expect("stock quota runtime");

    runtime
        .shutdown()
        .await
        .expect("quota runtime must settle before bounded join");
    assert!(QUOTA_RUNTIME_SHUTDOWN_TIMEOUT >= Duration::from_secs(1));
}
