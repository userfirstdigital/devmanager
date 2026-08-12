//! Versioned organization wire names carried as generic Connect extensions.
//!
//! These constants do not add payload-catalog tags. Organization facts ride
//! on the existing `extension` kind so anonymous/local Hello stays unchanged.

use super::Capability;

pub const ORGANIZATION_SCHEMA_VERSION: u16 = 1;
pub const ORGANIZATION_PROMPT_BODY_LIMIT_BYTES: u32 = 256 * 1024;
pub const ORGANIZATION_PROMPT_PAGE_ITEM_LIMIT: u32 = 100;
pub const ORGANIZATION_PROMPT_PAGE_ENCODED_LIMIT_BYTES: u32 = 512 * 1024;

/// Reserved generic-extension type identifiers for organization projections.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u16)]
pub enum OrganizationExtensionKind {
    Membership = 1001,
    ManagedTask = 1002,
    OrganizationPrompt = 1003,
    WatcherView = 1004,
    LocalAction = 1005,
    EvidenceBundle = 1006,
    BoardWorkflow = 1007,
}

impl OrganizationExtensionKind {
    pub const fn type_id(self) -> u16 {
        self as u16
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Membership => "organization_membership",
            Self::ManagedTask => "managed_task_link",
            Self::OrganizationPrompt => "organization_prompt",
            Self::WatcherView => "organization_watcher",
            Self::LocalAction => "local_action",
            Self::EvidenceBundle => "evidence_bundle",
            Self::BoardWorkflow => "board_workflow",
        }
    }
}

pub const fn organization_extension_type(kind: OrganizationExtensionKind) -> u16 {
    kind.type_id()
}

/// Anonymous/local standalone Hello must not advertise organization projection.
pub const fn organization_capability_for_standalone() -> Capability {
    Capability::OrganizationProjection
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::CapabilitySet;

    #[test]
    fn standalone_hello_excludes_organization_capability() {
        let advertised = CapabilitySet::empty();
        assert!(!advertised.contains(organization_capability_for_standalone()));
        assert_eq!(OrganizationExtensionKind::Membership.type_id(), 1001);
        assert_eq!(ORGANIZATION_SCHEMA_VERSION, 1);
        assert_eq!(ORGANIZATION_PROMPT_BODY_LIMIT_BYTES, 256 * 1024);
        assert_eq!(ORGANIZATION_PROMPT_PAGE_ITEM_LIMIT, 100);
    }
}
