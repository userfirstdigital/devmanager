//! Connect adapter for the organization projection. Standalone hosts expose
//! an empty overlay and never advertise organization capability.

pub use crate::org::{
    OperatingMode, OrgDependency, OrgError, OrganizationProjection, StandaloneOrganization,
};

use crate::protocol::CapabilitySet;

pub fn advertised_capabilities(mode: &OperatingMode, base: CapabilitySet) -> CapabilitySet {
    match mode.organization_capability() {
        Some(capability) => CapabilitySet::from_bits(base.bits() | capability.bit()),
        None => base,
    }
}
