use devmanager::connect::{
    ObservationDependency, ObservationError, ObservationSchema, OBSERVATION_SCHEMA_REVISION,
};

#[test]
fn public_observation_surface_keeps_publication_as_typed_holds() {
    assert_eq!(
        ObservationError::Unavailable(ObservationDependency::DurableOutbox),
        ObservationError::Unavailable(ObservationDependency::DurableOutbox)
    );
    assert_eq!(
        ObservationError::Unavailable(ObservationDependency::PortalObservationEffect),
        ObservationError::Unavailable(ObservationDependency::PortalObservationEffect)
    );
    assert_ne!(
        ObservationDependency::DurableOutbox,
        ObservationDependency::PortalObservationEffect
    );
    assert_eq!(
        ObservationSchema::current().revision(),
        OBSERVATION_SCHEMA_REVISION
    );
}
