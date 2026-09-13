//! Minimal provider-runtime factory/bridge proofs.

use devmanager::providers::{
    register_stock_adapters, registered_stock_kinds, stock_adapter_ingress,
    stock_adapter_ingress_available, stock_provider_registry, ProviderError, ProviderKind,
    ProviderRegistry, STOCK_PROVIDER_REGISTRATION_ORDER,
};

#[test]
fn provider_runtime_stock_factory_is_deterministic() {
    let registry = stock_provider_registry().expect("stock registry");
    assert_eq!(
        registered_stock_kinds(&registry),
        STOCK_PROVIDER_REGISTRATION_ORDER.to_vec()
    );
    assert!(registry.is_registered(ProviderKind::ClaudeCode));
    assert!(registry.is_registered(ProviderKind::Codex));
    assert!(registry.is_registered(ProviderKind::Cursor));

    let mut empty = ProviderRegistry::new();
    register_stock_adapters(&mut empty).expect("register once");
    assert!(matches!(
        register_stock_adapters(&mut empty),
        Err(ProviderError::DuplicateProviderKind(_))
    ));
}

#[test]
fn provider_runtime_free_stock_journal_ingress_stays_unavailable() {
    assert!(!stock_adapter_ingress_available());
    assert!(stock_adapter_ingress().is_err());
}
