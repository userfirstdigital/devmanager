use devmanager::connect::{InMemoryIdentityPersistence, IsolatedRemoteStore};

// The fake-vault behavioral suite is compiled inside the library's `cfg(test)`
// module so proof constructors and the in-memory store never exist as a
// caller-forgeable integration/debug authority.
#[test]
fn integration_surface_rejects_caller_defined_production_custody() {
    let result = IsolatedRemoteStore::new(InMemoryIdentityPersistence::default());
    assert!(matches!(
        result,
        Err(devmanager::connect::IdentityError::ProductionStoreForbidden)
    ));
}
