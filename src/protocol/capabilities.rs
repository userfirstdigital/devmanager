use serde::{Deserialize, Serialize};

pub const PROTOCOL_MAJOR: u16 = 1;
pub const PROTOCOL_MINOR: u16 = 0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProtocolVersion {
    pub major: u16,
    pub minor: u16,
}

impl ProtocolVersion {
    pub const fn new(major: u16, minor: u16) -> Self {
        Self { major, minor }
    }

    pub const fn current() -> Self {
        Self::new(PROTOCOL_MAJOR, PROTOCOL_MINOR)
    }

    pub fn negotiate(self, peer: Self) -> Result<Self, VersionNegotiationError> {
        if self.major != peer.major {
            return Err(VersionNegotiationError::IncompatibleMajor {
                local: self.major,
                peer: peer.major,
            });
        }
        Ok(Self::new(self.major, self.minor.min(peer.minor)))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionNegotiationError {
    IncompatibleMajor { local: u16, peer: u16 },
}

impl std::fmt::Display for VersionNegotiationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::IncompatibleMajor { local, peer } => {
                write!(
                    f,
                    "protocol major {peer} is incompatible with local major {local}"
                )
            }
        }
    }
}

impl std::error::Error for VersionNegotiationError {}

/// Stable capability bit assignments for protocol v1.
///
/// `CapabilitySet` carries the bits on the wire so a newer minor peer's
/// unknown bits can be preserved and safely excluded by intersection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Capability {
    PagedSnapshots = 0,
    EventReplay = 1,
    OperationSettlement = 2,
    ChunkResume = 3,
    GenericExtensions = 4,
    SemanticConversation = 5,
    TerminalDeltas = 6,
    BrowserProjection = 7,
    PromptProjection = 8,
    ConnectEncryption = 9,
    Guests = 10,
    ManagementMetadata = 11,
    ExplicitDetach = 12,
    HostShutdown = 13,
    ProviderInput = 14,
    OrganizationProjection = 15,
    ServiceSupervisor = 16,
    TaskCockpit = 17,
    /// Correlated PrepareUpdate replies carry the exact host-issued handoff
    /// token. This is separate from HostShutdown so older clients never see
    /// the new reply variant on the wire.
    UpdateHandoff = 18,
}

impl Capability {
    pub const fn bit(self) -> u64 {
        1_u64 << (self as u8)
    }

    /// Stable capability name used by Hello/catalog documents.
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::PagedSnapshots => "paged_snapshots",
            Self::EventReplay => "event_replay",
            Self::OperationSettlement => "operation_settlement",
            Self::ChunkResume => "chunk_resume",
            Self::GenericExtensions => "generic_extensions",
            Self::SemanticConversation => "semantic_conversation",
            Self::TerminalDeltas => "terminal_deltas",
            Self::BrowserProjection => "browser_projection",
            Self::PromptProjection => "personal_prompt_library",
            Self::ConnectEncryption => "connect_encryption",
            Self::Guests => "guests",
            Self::ManagementMetadata => "management_metadata",
            Self::ExplicitDetach => "explicit_detach",
            Self::HostShutdown => "host_shutdown",
            Self::ProviderInput => "provider_input",
            Self::OrganizationProjection => "organization_projection",
            Self::ServiceSupervisor => "service_supervisor",
            Self::TaskCockpit => "task_cockpit",
            Self::UpdateHandoff => "update_handoff",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CapabilitySet {
    bits: u64,
}

impl CapabilitySet {
    pub const fn empty() -> Self {
        Self { bits: 0 }
    }

    pub const fn from_bits(bits: u64) -> Self {
        Self { bits }
    }

    pub fn from_capabilities(capabilities: impl IntoIterator<Item = Capability>) -> Self {
        let mut bits = 0;
        for capability in capabilities {
            bits |= capability.bit();
        }
        Self { bits }
    }

    pub const fn bits(self) -> u64 {
        self.bits
    }

    pub const fn contains(self, capability: Capability) -> bool {
        self.contains_bit(capability.bit())
    }

    pub const fn contains_bit(self, bit: u64) -> bool {
        bit != 0 && self.bits & bit == bit
    }

    pub const fn intersection(self, peer: Self) -> Self {
        Self::from_bits(self.bits & peer.bits)
    }

    /// Personal prompt library frames stay on the host / owner-device path.
    pub const fn grants_personal_prompt_library(self) -> bool {
        self.contains(Capability::PromptProjection)
    }

    pub const fn grants_task_cockpit(self) -> bool {
        self.contains(Capability::TaskCockpit)
    }
}

#[cfg(test)]
mod tests {
    use super::{Capability, CapabilitySet};

    #[test]
    fn task_cockpit_wire_name_and_bit_are_stable_and_not_service_supervisor() {
        assert_eq!(Capability::TaskCockpit.wire_name(), "task_cockpit");
        assert_eq!(Capability::TaskCockpit.bit(), 1_u64 << 17);
        assert_eq!(
            Capability::ServiceSupervisor.wire_name(),
            "service_supervisor"
        );
        assert_eq!(Capability::ServiceSupervisor.bit(), 1_u64 << 16);
        let granted = CapabilitySet::from_capabilities([Capability::TaskCockpit]);
        assert!(granted.grants_task_cockpit());
        assert!(!granted.contains(Capability::ServiceSupervisor));
        assert!(!CapabilitySet::empty().grants_task_cockpit());
    }

    #[test]
    fn update_handoff_is_a_distinct_forward_compatible_capability() {
        assert_eq!(Capability::UpdateHandoff.wire_name(), "update_handoff");
        assert_eq!(Capability::UpdateHandoff.bit(), 1_u64 << 18);
        let offered =
            CapabilitySet::from_capabilities([Capability::HostShutdown, Capability::UpdateHandoff]);
        assert!(offered.contains(Capability::UpdateHandoff));
        assert!(offered.contains(Capability::HostShutdown));
        assert_eq!(
            offered.intersection(CapabilitySet::from_bits(1_u64 << 13)),
            CapabilitySet::from_capabilities([Capability::HostShutdown])
        );
    }
}
