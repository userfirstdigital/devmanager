//! Stable protocol compatibility and safety contracts.

use devmanager::protocol::{
    Capability, CapabilitySet, FrameLimitField, FrameLimits, FrameLimitsError, ProtocolVersion,
    VersionNegotiationError, PROTOCOL_MAJOR, PROTOCOL_MINOR,
};

#[test]
fn protocol_capability_bits_are_stable_and_unknown_bits_are_tolerated() {
    let named = [
        (Capability::PagedSnapshots, 0),
        (Capability::EventReplay, 1),
        (Capability::OperationSettlement, 2),
        (Capability::ChunkResume, 3),
        (Capability::GenericExtensions, 4),
        (Capability::SemanticConversation, 5),
        (Capability::TerminalDeltas, 6),
        (Capability::BrowserProjection, 7),
        (Capability::PromptProjection, 8),
        (Capability::ConnectEncryption, 9),
        (Capability::Guests, 10),
        (Capability::ManagementMetadata, 11),
    ];
    for (capability, bit_index) in named {
        assert_eq!(capability.bit(), 1_u64 << bit_index);
    }

    let unknown_bit = 1_u64 << 63;
    let requested = CapabilitySet::from_bits(
        Capability::PagedSnapshots.bit() | Capability::EventReplay.bit() | unknown_bit,
    );
    let encoded = rmp_serde::to_vec(&requested).expect("encode capability set");
    let decoded: CapabilitySet = rmp_serde::from_slice(&encoded).expect("decode capability set");
    assert_eq!(decoded.bits(), requested.bits());

    let supported = CapabilitySet::from_capabilities([
        Capability::PagedSnapshots,
        Capability::OperationSettlement,
    ]);
    let granted = supported.intersection(decoded);
    assert_eq!(
        granted,
        CapabilitySet::from_capabilities([Capability::PagedSnapshots])
    );
    assert!(!granted.contains_bit(unknown_bit));
}

#[test]
fn protocol_version_negotiates_lower_minor_and_rejects_different_major() {
    assert_eq!(PROTOCOL_MAJOR, 1);
    assert_eq!(PROTOCOL_MINOR, 0);
    assert_eq!(
        ProtocolVersion::new(1, 5)
            .negotiate(ProtocolVersion::new(1, 2))
            .expect("compatible versions"),
        ProtocolVersion::new(1, 2)
    );
    assert_eq!(
        ProtocolVersion::current()
            .negotiate(ProtocolVersion::new(1, 99))
            .expect("unknown newer minor is compatible"),
        ProtocolVersion::current()
    );
    assert_eq!(
        ProtocolVersion::current().negotiate(ProtocolVersion::new(2, 0)),
        Err(VersionNegotiationError::IncompatibleMajor {
            local: PROTOCOL_MAJOR,
            peer: 2,
        })
    );
}

#[test]
fn protocol_frame_limits_default_and_negotiate_per_field_minimum() {
    let defaults = FrameLimits::v1_default();
    assert_eq!(defaults.max_physical_frame_bytes, 1024 * 1024);
    assert_eq!(defaults.max_reassembled_message_bytes, 16 * 1024 * 1024);
    assert_eq!(defaults.max_page_items, 1_000);
    assert_eq!(defaults.max_page_encoded_bytes, 512 * 1024);

    let peer = FrameLimits {
        max_physical_frame_bytes: 64 * 1024,
        max_reassembled_message_bytes: 32 * 1024 * 1024,
        max_page_items: 250,
        max_page_encoded_bytes: 1024 * 1024,
    };
    assert_eq!(
        defaults.negotiate(peer).expect("negotiate peer limits"),
        FrameLimits {
            max_physical_frame_bytes: 64 * 1024,
            max_reassembled_message_bytes: 16 * 1024 * 1024,
            max_page_items: 250,
            max_page_encoded_bytes: 512 * 1024,
        }
    );

    let oversized_offer = FrameLimits {
        max_physical_frame_bytes: u32::MAX,
        max_reassembled_message_bytes: u32::MAX,
        max_page_items: u32::MAX,
        max_page_encoded_bytes: u32::MAX,
    };
    assert_eq!(
        oversized_offer
            .negotiate(oversized_offer)
            .expect("hard ceilings cap both offers"),
        defaults
    );
}

#[test]
fn protocol_frame_limits_reject_every_zero_offer() {
    let cases = [
        (
            FrameLimits {
                max_physical_frame_bytes: 0,
                ..FrameLimits::v1_default()
            },
            FrameLimitField::PhysicalFrameBytes,
        ),
        (
            FrameLimits {
                max_reassembled_message_bytes: 0,
                ..FrameLimits::v1_default()
            },
            FrameLimitField::ReassembledMessageBytes,
        ),
        (
            FrameLimits {
                max_page_items: 0,
                ..FrameLimits::v1_default()
            },
            FrameLimitField::PageItems,
        ),
        (
            FrameLimits {
                max_page_encoded_bytes: 0,
                ..FrameLimits::v1_default()
            },
            FrameLimitField::PageEncodedBytes,
        ),
    ];

    for (offer, field) in cases {
        assert_eq!(
            FrameLimits::v1_default().negotiate(offer),
            Err(FrameLimitsError::Zero { field })
        );
    }

    #[derive(serde::Serialize)]
    struct RawFrameLimits {
        max_physical_frame_bytes: u32,
        max_reassembled_message_bytes: u32,
        max_page_items: u32,
        max_page_encoded_bytes: u32,
    }
    let malformed_wire = rmp_serde::to_vec(&RawFrameLimits {
        max_physical_frame_bytes: 0,
        max_reassembled_message_bytes: 16 * 1024 * 1024,
        max_page_items: 1_000,
        max_page_encoded_bytes: 512 * 1024,
    })
    .expect("encode malformed peer offer");
    assert!(
        rmp_serde::from_slice::<FrameLimits>(&malformed_wire).is_err(),
        "wire decode must enforce the same nonzero contract"
    );
}
