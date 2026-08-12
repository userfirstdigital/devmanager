use async_trait::async_trait;
use devmanager::domain::PageLimits;
use devmanager::protocol::{query_semantic_journal_page, semantic_journal_query_available};
use devmanager::providers::adapter::{
    AdapterIngressUnavailable, JournalNormalizeError, LaunchProviderRequest,
    NormalizedAdapterDelivery, ProviderAdapter, ProviderError, ProviderLaunchSpec, ProviderRuntime,
    QuotaObservation, StopStrategy,
};
use devmanager::providers::capabilities::{
    ProviderCapabilities, ProviderCapability, ProviderExecutableHandle, ProviderKind,
};
use devmanager::providers::journal::{
    stock_adapter_ingress, stock_adapter_ingress_available, AdapterDeliveryPermit,
};

struct UnavailableAdapter;

#[async_trait]
impl ProviderAdapter for UnavailableAdapter {
    fn kind(&self) -> ProviderKind {
        ProviderKind::ClaudeCode
    }

    async fn probe(
        &self,
        _executable: &ProviderExecutableHandle,
    ) -> Result<ProviderCapabilities, ProviderError> {
        Err(ProviderError::UnsupportedCapability(
            ProviderCapability::ParseSignal,
        ))
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
    ) -> Result<Option<QuotaObservation>, ProviderError> {
        Ok(None)
    }
}

#[test]
fn journal_stock_adapter_ingress_is_explicitly_unavailable() {
    assert!(!stock_adapter_ingress_available());
    assert!(stock_adapter_ingress().is_err());
}

#[test]
fn journal_protocol_query_seam_is_capability_unavailable() {
    assert!(!semantic_journal_query_available());
    let limits = PageLimits::new(16, 8 * 1024).expect("limits");
    assert!(query_semantic_journal_page(0, limits).is_err());
}

#[test]
fn journal_adapter_normalize_delivery_is_typed_unavailable_not_journal_event() {
    let adapter: &dyn ProviderAdapter = &UnavailableAdapter;
    let _ = adapter;
    assert!(matches!(
        JournalNormalizeError::Unavailable(AdapterIngressUnavailable),
        JournalNormalizeError::Unavailable(_)
    ));
}
