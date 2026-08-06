//! Stable protocol compatibility and safety contracts.

use std::io::{Cursor, Error, ErrorKind, Read, Write};

use devmanager::domain::{
    CancellationReason, ClientId, Command, CommandEnvelope, CommandId, CommandReceipt,
    CreateTaskIntent, DomainEvent, EnvironmentId, Event, EventId, EventPage, OperationErrorCode,
    OperationId, OperationState, OperationUncertaintyCode, ProjectId, Query, QueryEnvelope,
    QueryError, QueryOutcome, QueryReply, QueryResult, RejectionCode, RequestId, ResourceId,
    ReviewReadiness, SubscriptionId, TaskActivity, TaskAssignment, TaskAttention, TaskConnectivity,
    TaskId, WorkspaceRef,
};
use devmanager::protocol::{
    Capability, CapabilitySet, ClientBuildError, ClientHello, ClientHelloError, ClientRequest,
    FrameLimitField, FrameLimits, FrameLimitsError, MessagePackCodec, MessagePackError,
    MessagePackLengthKind, PhysicalFrameCodec, PhysicalFrameError, ProfileFingerprint,
    ProtocolVersion, ServerMessage, StreamFrame, StreamKey, StreamPayloadKind,
    VersionNegotiationError, MAX_CLIENT_BUILD_BYTES, MAX_MESSAGEPACK_COLLECTION_ITEMS,
    MAX_MESSAGEPACK_DEPTH, MAX_MESSAGEPACK_VALUES, PROTOCOL_MAJOR, PROTOCOL_MINOR,
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

#[test]
fn protocol_physical_frame_writer_uses_big_endian_length_prefix() {
    let codec = PhysicalFrameCodec::from_limits(FrameLimits::v1_default()).expect("codec");
    let mut wire = Vec::new();
    codec
        .write(&mut wire, &[0xAA, 0xBB, 0xCC])
        .expect("write frame");
    assert_eq!(wire, vec![0x00, 0x00, 0x00, 0x03, 0xAA, 0xBB, 0xCC]);

    let mut empty_writer = Vec::new();
    assert_eq!(
        codec.write(&mut empty_writer, &[]),
        Err(PhysicalFrameError::Empty)
    );
    assert!(empty_writer.is_empty());
}

struct FragmentedReader {
    inner: Cursor<Vec<u8>>,
    max_read: usize,
    bytes_read: usize,
}

impl FragmentedReader {
    fn new(bytes: Vec<u8>, max_read: usize) -> Self {
        Self {
            inner: Cursor::new(bytes),
            max_read,
            bytes_read: 0,
        }
    }
}

impl Read for FragmentedReader {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let allowed = buffer.len().min(self.max_read);
        let read = self.inner.read(&mut buffer[..allowed])?;
        self.bytes_read += read;
        Ok(read)
    }
}

#[test]
fn protocol_physical_frame_reader_handles_fragmented_and_coalesced_frames() {
    let limits = FrameLimits::v1_default();
    let codec = PhysicalFrameCodec::from_limits(limits).expect("codec");
    let mut wire = Vec::new();
    codec.write(&mut wire, b"first").expect("first frame");
    codec.write(&mut wire, b"second").expect("second frame");

    let mut reader = FragmentedReader::new(wire, 1);
    assert_eq!(codec.read(&mut reader).unwrap(), b"first");
    assert_eq!(codec.read(&mut reader).unwrap(), b"second");
    assert_eq!(
        codec.read(&mut reader),
        Err(PhysicalFrameError::ReadHeader {
            kind: ErrorKind::UnexpectedEof,
        })
    );
}

#[test]
fn protocol_physical_frame_rejects_header_before_payload_read() {
    let small_limit = FrameLimits {
        max_physical_frame_bytes: 8,
        ..FrameLimits::v1_default()
    };
    let cases = [
        (0_u32, FrameLimits::v1_default(), PhysicalFrameError::Empty),
        (
            9_u32,
            small_limit,
            PhysicalFrameError::Oversized {
                declared: 9,
                maximum: 8,
            },
        ),
        (
            1024 * 1024 + 1,
            FrameLimits::v1_default(),
            PhysicalFrameError::Oversized {
                declared: 1024 * 1024 + 1,
                maximum: 1024 * 1024,
            },
        ),
    ];

    for (announced, limits, expected) in cases {
        let mut bytes = announced.to_be_bytes().to_vec();
        bytes.extend_from_slice(b"payload-must-not-be-read");
        let mut reader = FragmentedReader::new(bytes, usize::MAX);
        let codec = PhysicalFrameCodec::from_limits(limits).expect("codec");
        assert_eq!(codec.read(&mut reader), Err(expected));
        assert_eq!(
            reader.bytes_read, 4,
            "invalid header must be rejected before payload I/O"
        );
    }
}

struct FailingWriter {
    bytes_before_failure: usize,
}

impl Write for FailingWriter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        if self.bytes_before_failure == 0 {
            return Err(Error::new(ErrorKind::BrokenPipe, "closed fixture"));
        }
        let written = buffer.len().min(self.bytes_before_failure);
        self.bytes_before_failure -= written;
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn protocol_physical_frame_partial_io_fails_closed() {
    let limits = FrameLimits::v1_default();
    let codec = PhysicalFrameCodec::from_limits(limits).expect("codec");
    for bytes in [
        vec![0x00, 0x00, 0x00],
        vec![0x00, 0x00, 0x00, 0x03, 0xAA, 0xBB],
    ] {
        let expected = if bytes.len() < 4 {
            PhysicalFrameError::ReadHeader {
                kind: ErrorKind::UnexpectedEof,
            }
        } else {
            PhysicalFrameError::ReadPayload {
                declared: 3,
                kind: ErrorKind::UnexpectedEof,
            }
        };
        assert_eq!(codec.read(&mut Cursor::new(bytes)), Err(expected));
    }

    let mut header_writer = FailingWriter {
        bytes_before_failure: 0,
    };
    assert_eq!(
        codec.write(&mut header_writer, b"payload"),
        Err(PhysicalFrameError::WriteHeader {
            kind: ErrorKind::BrokenPipe,
        })
    );

    let mut writer = FailingWriter {
        bytes_before_failure: 5,
    };
    assert_eq!(
        codec.write(&mut writer, b"payload"),
        Err(PhysicalFrameError::WritePayload {
            declared: 7,
            kind: ErrorKind::BrokenPipe,
        })
    );

    let mut untouched = Vec::new();
    let small_codec = PhysicalFrameCodec::from_limits(FrameLimits {
        max_physical_frame_bytes: 8,
        ..limits
    })
    .expect("small codec");
    assert_eq!(
        small_codec.write(&mut untouched, &[0xAA; 9]),
        Err(PhysicalFrameError::Oversized {
            declared: 9,
            maximum: 8,
        })
    );
    assert!(untouched.is_empty());

    let hard_capped = PhysicalFrameCodec::from_limits(FrameLimits {
        max_physical_frame_bytes: u32::MAX,
        ..limits
    })
    .expect("hard-capped codec");
    assert_eq!(hard_capped.max_payload_bytes(), 1024 * 1024);
}

#[derive(Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct MessagePackFixture {
    name: String,
    values: Vec<u16>,
}

#[test]
fn protocol_messagepack_codec_round_trips_one_named_bounded_document() {
    let codec = MessagePackCodec::from_limits(FrameLimits {
        max_physical_frame_bytes: 1_024,
        ..FrameLimits::v1_default()
    })
    .expect("codec");
    let expected = MessagePackFixture {
        name: "bounded".to_string(),
        values: vec![1, 2, 3],
    };

    let encoded = codec.encode(&expected).expect("encode");
    assert_eq!(encoded[0], 0x82, "named structs use a stable map envelope");
    assert_eq!(
        codec
            .decode::<MessagePackFixture>(&encoded)
            .expect("decode"),
        expected
    );
    assert_eq!(codec.decode::<u64>(&encoded), Err(MessagePackError::Decode));
}

#[test]
fn protocol_messagepack_codec_rejects_document_boundary_violations() {
    let codec = MessagePackCodec::from_limits(FrameLimits {
        max_physical_frame_bytes: 8,
        ..FrameLimits::v1_default()
    })
    .expect("codec");

    assert_eq!(codec.decode::<()>(&[]), Err(MessagePackError::Empty));
    assert_eq!(
        codec.decode::<()>(&[0xc0; 9]),
        Err(MessagePackError::Oversized {
            declared: 9,
            maximum: 8,
        })
    );
    assert_eq!(
        codec.decode::<()>(&[0xc1]),
        Err(MessagePackError::ReservedMarker { offset: 0 })
    );
    assert_eq!(
        codec.decode::<String>(&[0xd9, 0x01]),
        Err(MessagePackError::Truncated { offset: 2 })
    );
    assert_eq!(
        codec.decode::<()>(&[0xc0, 0xc0]),
        Err(MessagePackError::TrailingBytes { offset: 1 })
    );
}

#[test]
fn protocol_messagepack_codec_rejects_huge_declarations_before_serde() {
    let codec = MessagePackCodec::from_limits(FrameLimits::v1_default()).expect("codec");
    let cases = [
        (
            vec![0xdd, 0xff, 0xff, 0xff, 0xff],
            MessagePackLengthKind::Array,
            MAX_MESSAGEPACK_COLLECTION_ITEMS,
        ),
        (
            vec![0xdf, 0xff, 0xff, 0xff, 0xff],
            MessagePackLengthKind::Map,
            MAX_MESSAGEPACK_COLLECTION_ITEMS,
        ),
        (
            vec![0xdb, 0xff, 0xff, 0xff, 0xff],
            MessagePackLengthKind::String,
            1024 * 1024,
        ),
        (
            vec![0xc6, 0xff, 0xff, 0xff, 0xff],
            MessagePackLengthKind::Binary,
            1024 * 1024,
        ),
    ];

    for (payload, kind, maximum) in cases {
        assert_eq!(
            codec.decode::<()>(&payload),
            Err(MessagePackError::DeclaredLengthExceeded {
                kind,
                declared: u32::MAX,
                maximum,
            })
        );
    }

    for payload in [vec![0xd4], vec![0xc9, 0xff, 0xff, 0xff, 0xff]] {
        assert_eq!(
            codec.decode::<()>(&payload),
            Err(MessagePackError::UnsupportedExtension { offset: 0 })
        );
    }
}

#[test]
fn protocol_messagepack_codec_bounds_depth_and_total_values() {
    let codec = MessagePackCodec::from_limits(FrameLimits::v1_default()).expect("codec");

    let mut maximum_depth = vec![0x91; usize::from(MAX_MESSAGEPACK_DEPTH)];
    maximum_depth.push(0xc0);
    codec
        .decode::<serde::de::IgnoredAny>(&maximum_depth)
        .expect("the advertised maximum depth remains decodable");

    let mut too_deep = vec![0x91; usize::from(MAX_MESSAGEPACK_DEPTH) + 1];
    too_deep.push(0xc0);
    assert_eq!(
        codec.decode::<()>(&too_deep),
        Err(MessagePackError::DepthExceeded {
            maximum: MAX_MESSAGEPACK_DEPTH,
        })
    );

    let mut too_deep_even_when_empty = vec![0x91; usize::from(MAX_MESSAGEPACK_DEPTH)];
    too_deep_even_when_empty.push(0x90);
    assert_eq!(
        codec.decode::<serde::de::IgnoredAny>(&too_deep_even_when_empty),
        Err(MessagePackError::DepthExceeded {
            maximum: MAX_MESSAGEPACK_DEPTH,
        })
    );

    let mut too_many = vec![0xdc];
    too_many.extend_from_slice(
        &u16::try_from(MAX_MESSAGEPACK_COLLECTION_ITEMS)
            .expect("collection bound fits u16")
            .to_be_bytes(),
    );
    for _ in 0..MAX_MESSAGEPACK_COLLECTION_ITEMS {
        too_many.push(0xdc);
        too_many.extend_from_slice(&66_u16.to_be_bytes());
        too_many.extend_from_slice(&[0xc0; 66]);
    }
    assert!(too_many.len() < 1024 * 1024);
    assert_eq!(
        codec.decode::<()>(&too_many),
        Err(MessagePackError::ValueCountExceeded {
            maximum: MAX_MESSAGEPACK_VALUES,
        })
    );
}

fn protocol_uuid_v7(tail: u8) -> [u8; 16] {
    [
        0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        tail,
    ]
}

fn protocol_client_id(tail: u8) -> ClientId {
    ClientId::from_bytes(protocol_uuid_v7(tail)).expect("client id")
}

fn protocol_profile_fingerprint(tail: u8) -> ProfileFingerprint {
    ProfileFingerprint::hash_normalized(&format!("contract-profile-{tail:02x}"))
}

fn protocol_command_id(tail: u8) -> CommandId {
    CommandId::from_bytes(protocol_uuid_v7(tail)).expect("command id")
}

fn protocol_task_id(tail: u8) -> TaskId {
    TaskId::from_bytes(protocol_uuid_v7(tail)).expect("task id")
}

fn protocol_operation_id(tail: u8) -> OperationId {
    OperationId::from_bytes(protocol_uuid_v7(tail)).expect("operation id")
}

fn protocol_event_id(tail: u8) -> EventId {
    EventId::from_bytes(protocol_uuid_v7(tail)).expect("event id")
}

fn protocol_request_id(tail: u8) -> RequestId {
    RequestId::from_bytes(protocol_uuid_v7(tail)).expect("request id")
}

fn protocol_subscription_id(tail: u8) -> SubscriptionId {
    SubscriptionId::from_bytes(protocol_uuid_v7(tail)).expect("subscription id")
}

#[test]
fn protocol_client_hello_round_trips_and_negotiates_without_freezing_server_hello() {
    let unknown_bit = 1_u64 << 63;
    let requested = CapabilitySet::from_bits(
        Capability::PagedSnapshots.bit() | Capability::EventReplay.bit() | unknown_bit,
    );
    let offered_limits = FrameLimits {
        max_physical_frame_bytes: 64 * 1024,
        max_reassembled_message_bytes: 32 * 1024 * 1024,
        max_page_items: 250,
        max_page_encoded_bytes: 1024 * 1024,
    };
    let hello = ClientHello::new(
        "devmanager/0.4.2",
        protocol_client_id(0x41),
        protocol_profile_fingerprint(0x41),
        requested,
        offered_limits,
    )
    .expect("valid hello");
    let codec = MessagePackCodec::from_limits(FrameLimits::v1_default()).expect("codec");

    let encoded = codec.encode(&hello).expect("encode hello");
    assert_eq!(encoded[0], 0x87, "ClientHello is a seven-field named map");
    assert_eq!(
        rmp_serde::to_vec(&hello).expect("direct serialization")[0],
        0x87,
        "ClientHello never exposes a compact tuple wire shape"
    );
    let decoded = codec.decode::<ClientHello>(&encoded).expect("decode hello");
    assert_eq!(decoded, hello);
    assert!(decoded.requested.contains_bit(unknown_bit));

    let negotiated = decoded
        .negotiate(
            CapabilitySet::from_capabilities([
                Capability::PagedSnapshots,
                Capability::OperationSettlement,
            ]),
            FrameLimits::v1_default(),
        )
        .expect("negotiate hello");
    assert_eq!(negotiated.version, ProtocolVersion::current());
    assert_eq!(negotiated.client_id, protocol_client_id(0x41));
    assert_eq!(
        negotiated.capabilities,
        CapabilitySet::from_capabilities([Capability::PagedSnapshots])
    );
    assert_eq!(
        negotiated.limits,
        FrameLimits {
            max_physical_frame_bytes: 64 * 1024,
            max_reassembled_message_bytes: 16 * 1024 * 1024,
            max_page_items: 250,
            max_page_encoded_bytes: 512 * 1024,
        }
    );
}

#[derive(serde::Serialize)]
struct RawClientHello {
    protocol_major: u16,
    protocol_minor: u16,
    client_build: String,
    client_id: Vec<u8>,
    profile_fingerprint: Vec<u8>,
    requested: CapabilitySet,
    limits: RawHelloLimits,
}

#[derive(Clone, Copy, serde::Serialize)]
struct RawHelloLimits {
    max_physical_frame_bytes: u32,
    max_reassembled_message_bytes: u32,
    max_page_items: u32,
    max_page_encoded_bytes: u32,
}

impl From<FrameLimits> for RawHelloLimits {
    fn from(value: FrameLimits) -> Self {
        Self {
            max_physical_frame_bytes: value.max_physical_frame_bytes,
            max_reassembled_message_bytes: value.max_reassembled_message_bytes,
            max_page_items: value.max_page_items,
            max_page_encoded_bytes: value.max_page_encoded_bytes,
        }
    }
}

fn raw_client_hello() -> RawClientHello {
    RawClientHello {
        protocol_major: PROTOCOL_MAJOR,
        protocol_minor: PROTOCOL_MINOR,
        client_build: "devmanager/0.4.2".to_string(),
        client_id: protocol_client_id(0x42).as_bytes().to_vec(),
        profile_fingerprint: protocol_profile_fingerprint(0x42).as_bytes().to_vec(),
        requested: CapabilitySet::empty(),
        limits: FrameLimits::v1_default().into(),
    }
}

#[test]
fn protocol_client_hello_rejects_invalid_fields_and_document_shape() {
    let client_id = protocol_client_id(0x43);
    let fingerprint = protocol_profile_fingerprint(0x43);
    let requested = CapabilitySet::empty();
    let limits = FrameLimits::v1_default();
    assert_eq!(
        ClientHello::new("", client_id, fingerprint, requested, limits),
        Err(ClientHelloError::Build(ClientBuildError::Empty))
    );
    ClientHello::new(
        "x".repeat(usize::try_from(MAX_CLIENT_BUILD_BYTES).unwrap()),
        client_id,
        fingerprint,
        requested,
        limits,
    )
    .expect("the exact build limit is valid");
    assert_eq!(
        ClientHello::new(
            "x".repeat(usize::try_from(MAX_CLIENT_BUILD_BYTES).unwrap() + 1),
            client_id,
            fingerprint,
            requested,
            limits,
        ),
        Err(ClientHelloError::Build(ClientBuildError::TooLong {
            declared: u64::from(MAX_CLIENT_BUILD_BYTES) + 1,
            maximum: MAX_CLIENT_BUILD_BYTES,
        }))
    );

    let codec = MessagePackCodec::from_limits(limits).expect("codec");
    let forged_empty = ClientHello {
        protocol_major: PROTOCOL_MAJOR,
        protocol_minor: PROTOCOL_MINOR,
        client_build: String::new(),
        client_id,
        profile_fingerprint: fingerprint,
        requested,
        limits,
    };
    assert_eq!(
        codec.encode(&forged_empty),
        Err(MessagePackError::Encode),
        "public fields cannot bypass encode validation"
    );

    let mut invalid = raw_client_hello();
    invalid.client_build.clear();
    let bytes = rmp_serde::to_vec_named(&invalid).expect("raw empty build");
    assert_eq!(
        codec.decode::<ClientHello>(&bytes),
        Err(MessagePackError::Decode)
    );

    invalid = raw_client_hello();
    invalid.client_build = "x".repeat(usize::try_from(MAX_CLIENT_BUILD_BYTES).unwrap() + 1);
    let bytes = rmp_serde::to_vec_named(&invalid).expect("raw long build");
    assert_eq!(
        codec.decode::<ClientHello>(&bytes),
        Err(MessagePackError::Decode)
    );

    invalid = raw_client_hello();
    invalid.client_id = vec![0; 16];
    let bytes = rmp_serde::to_vec_named(&invalid).expect("raw malformed uuid");
    assert_eq!(
        codec.decode::<ClientHello>(&bytes),
        Err(MessagePackError::Decode)
    );

    invalid = raw_client_hello();
    invalid.profile_fingerprint = vec![0; 16];
    let bytes = rmp_serde::to_vec_named(&invalid).expect("raw short fingerprint");
    assert_eq!(
        codec.decode::<ClientHello>(&bytes),
        Err(MessagePackError::Decode)
    );

    invalid = raw_client_hello();
    invalid.limits.max_physical_frame_bytes = 0;
    let bytes = rmp_serde::to_vec_named(&invalid).expect("raw zero limits");
    assert_eq!(
        codec.decode::<ClientHello>(&bytes),
        Err(MessagePackError::Decode)
    );

    let valid = ClientHello::new(
        "devmanager/0.4.2",
        client_id,
        fingerprint,
        requested,
        limits,
    )
    .unwrap();
    let compact_tuple = (
        PROTOCOL_MAJOR,
        PROTOCOL_MINOR,
        "devmanager/0.4.2",
        client_id.as_bytes().to_vec(),
        fingerprint.as_bytes().to_vec(),
        requested,
        RawHelloLimits::from(limits),
    );
    let compact_bytes = rmp_serde::to_vec(&compact_tuple).expect("compact tuple fixture");
    assert_eq!(compact_bytes[0], 0x97);
    assert_eq!(
        codec.decode::<ClientHello>(&compact_bytes),
        Err(MessagePackError::Decode),
        "only the named-map wire shape is accepted"
    );

    let mut duplicate = codec.encode(&valid).expect("valid hello");
    assert_eq!(duplicate[0], 0x87);
    duplicate[0] = 0x88;
    duplicate.extend(rmp_serde::to_vec(&"client_build").unwrap());
    duplicate.extend(rmp_serde::to_vec(&"duplicate").unwrap());
    assert_eq!(
        codec.decode::<ClientHello>(&duplicate),
        Err(MessagePackError::Decode)
    );

    let mut unknown = codec.encode(&valid).expect("valid hello");
    unknown[0] = 0x88;
    unknown.extend(rmp_serde::to_vec(&"future_field").unwrap());
    unknown.push(0xc0);
    assert_eq!(
        codec.decode::<ClientHello>(&unknown),
        Err(MessagePackError::Decode)
    );

    let mut trailing = codec.encode(&valid).expect("valid hello");
    trailing.push(0xc0);
    assert_eq!(
        codec.decode::<ClientHello>(&trailing),
        Err(MessagePackError::TrailingBytes {
            offset: u32::try_from(trailing.len() - 1).unwrap(),
        })
    );
}

#[test]
fn protocol_client_hello_decodes_incompatible_major_then_rejects_negotiation() {
    let mut raw = raw_client_hello();
    raw.protocol_major = PROTOCOL_MAJOR + 1;
    raw.protocol_minor = 99;
    let codec = MessagePackCodec::from_limits(FrameLimits::v1_default()).expect("codec");
    let bytes = rmp_serde::to_vec_named(&raw).expect("encode future hello");
    let decoded = codec
        .decode::<ClientHello>(&bytes)
        .expect("compatibility is not a decode concern");

    assert_eq!(decoded.protocol_major, PROTOCOL_MAJOR + 1);
    assert_eq!(
        decoded.negotiate(CapabilitySet::empty(), FrameLimits::v1_default(),),
        Err(ClientHelloError::Version(
            VersionNegotiationError::IncompatibleMajor {
                local: PROTOCOL_MAJOR,
                peer: PROTOCOL_MAJOR + 1,
            }
        ))
    );
}

#[test]
fn protocol_command_envelope_is_one_strict_named_correlation_map() {
    let expected = CommandEnvelope {
        command_id: protocol_command_id(0x51),
        client_id: protocol_client_id(0x52),
        task_id: Some(protocol_task_id(0x53)),
        issued_at_ms: 1_725_000_000_100,
        expected_task_revision: Some(7),
        command: Command::BeginCloseTask,
    };
    let codec = MessagePackCodec::from_limits(FrameLimits::v1_default()).expect("codec");

    let encoded = codec.encode(&expected).expect("encode command envelope");
    assert_eq!(encoded[0], 0x86, "CommandEnvelope is a six-field map");
    assert_eq!(
        rmp_serde::to_vec(&expected).expect("direct command envelope serialization")[0],
        0x86,
        "compact serializers cannot create a positional request shape"
    );
    assert_eq!(
        codec
            .decode::<CommandEnvelope>(&encoded)
            .expect("decode command envelope"),
        expected
    );

    let unscoped = CommandEnvelope {
        command_id: protocol_command_id(0x57),
        client_id: protocol_client_id(0x58),
        task_id: None,
        issued_at_ms: -1,
        expected_task_revision: None,
        command: Command::BeginCloseTask,
    };
    assert_eq!(
        codec
            .decode::<CommandEnvelope>(&codec.encode(&unscoped).expect("encode null optionals"))
            .expect("decode null optionals without applying business rules"),
        unscoped
    );
}

#[derive(serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum RawUnknownCommand {
    FutureCommand,
}

#[derive(serde::Serialize)]
struct RawCommandEnvelope<C> {
    command_id: CommandId,
    client_id: ClientId,
    task_id: Option<TaskId>,
    issued_at_ms: i64,
    expected_task_revision: Option<u64>,
    command: C,
}

fn raw_command_envelope<C>(command: C) -> RawCommandEnvelope<C> {
    RawCommandEnvelope {
        command_id: protocol_command_id(0x54),
        client_id: protocol_client_id(0x55),
        task_id: Some(protocol_task_id(0x56)),
        issued_at_ms: 1_725_000_000_200,
        expected_task_revision: Some(9),
        command,
    }
}

#[test]
fn protocol_command_envelope_rejects_alternate_or_open_shapes() {
    let codec = MessagePackCodec::from_limits(FrameLimits::v1_default()).expect("codec");
    let valid = raw_command_envelope(Command::BeginCloseTask);
    let valid_bytes = rmp_serde::to_vec_named(&valid).expect("valid raw command");

    let positional = (
        valid.command_id,
        valid.client_id,
        valid.task_id,
        valid.issued_at_ms,
        valid.expected_task_revision,
        Command::BeginCloseTask,
    );
    let positional_bytes = rmp_serde::to_vec(&positional).expect("positional fixture");
    assert_eq!(positional_bytes[0], 0x96);
    assert_eq!(
        codec.decode::<CommandEnvelope>(&positional_bytes),
        Err(MessagePackError::Decode)
    );

    let mut duplicate = valid_bytes.clone();
    duplicate[0] = 0x87;
    duplicate.extend(rmp_serde::to_vec(&"command_id").unwrap());
    duplicate.extend(rmp_serde::to_vec(&valid.command_id).unwrap());
    assert_eq!(
        codec.decode::<CommandEnvelope>(&duplicate),
        Err(MessagePackError::Decode)
    );

    let mut unknown = valid_bytes.clone();
    unknown[0] = 0x87;
    unknown.extend(rmp_serde::to_vec(&"future_field").unwrap());
    unknown.push(0xc0);
    assert_eq!(
        codec.decode::<CommandEnvelope>(&unknown),
        Err(MessagePackError::Decode)
    );

    let mut missing = valid_bytes.clone();
    assert_eq!(missing[0], 0x86);
    missing[0] = 0x85;
    let command_key = rmp_serde::to_vec(&"command").unwrap();
    let command_value = rmp_serde::to_vec(&Command::BeginCloseTask).unwrap();
    let suffix = [command_key, command_value].concat();
    assert!(missing.ends_with(&suffix));
    missing.truncate(missing.len() - suffix.len());
    assert_eq!(
        codec.decode::<CommandEnvelope>(&missing),
        Err(MessagePackError::Decode)
    );

    let unknown_command =
        rmp_serde::to_vec_named(&raw_command_envelope(RawUnknownCommand::FutureCommand))
            .expect("unknown command fixture");
    assert_eq!(
        codec.decode::<CommandEnvelope>(&unknown_command),
        Err(MessagePackError::Decode)
    );

    let mut trailing = valid_bytes;
    trailing.push(0xc0);
    assert_eq!(
        codec.decode::<CommandEnvelope>(&trailing),
        Err(MessagePackError::TrailingBytes {
            offset: u32::try_from(trailing.len() - 1).unwrap(),
        })
    );
}

#[test]
fn protocol_command_receipt_preserves_command_and_accepted_operation_correlation() {
    let accepted = CommandReceipt::Accepted {
        command_id: protocol_command_id(0x61),
        operation_id: protocol_operation_id(0x62),
        task_revision: None,
        event_ids: vec![protocol_event_id(0x63), protocol_event_id(0x64)],
    };
    assert_eq!(accepted.command_id(), protocol_command_id(0x61));
    assert_eq!(
        accepted.accepted_operation_id(),
        Some(protocol_operation_id(0x62))
    );

    let rejected = CommandReceipt::Rejected {
        command_id: protocol_command_id(0x65),
        code: RejectionCode::RevisionConflict,
        current_revision: None,
    };
    assert_eq!(rejected.command_id(), protocol_command_id(0x65));
    assert_eq!(rejected.accepted_operation_id(), None);

    let codec = MessagePackCodec::from_limits(FrameLimits::v1_default()).expect("codec");
    for receipt in [accepted, rejected] {
        let encoded = codec.encode(&receipt).expect("encode receipt");
        assert_eq!(encoded[0], 0x81, "receipt has one outer variant entry");
        assert_eq!(
            rmp_serde::to_vec(&receipt).expect("direct receipt serialization")[0],
            0x81
        );
        assert_eq!(
            codec
                .decode::<CommandReceipt>(&encoded)
                .expect("decode receipt"),
            receipt
        );
    }

    for (index, (code, wire_name)) in [
        (RejectionCode::NotFound, "not_found"),
        (RejectionCode::AlreadyExists, "already_exists"),
        (RejectionCode::RevisionConflict, "revision_conflict"),
        (RejectionCode::InvalidTransition, "invalid_transition"),
        (RejectionCode::OwnershipConflict, "ownership_conflict"),
        (
            RejectionCode::UnsupportedCapability,
            "unsupported_capability",
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let receipt = CommandReceipt::Rejected {
            command_id: protocol_command_id(0xA0 + u8::try_from(index).unwrap()),
            code,
            current_revision: Some(3),
        };
        assert_eq!(
            codec
                .decode::<CommandReceipt>(&codec.encode(&receipt).unwrap())
                .unwrap(),
            receipt
        );
        assert_eq!(
            rmp_serde::to_vec(&code).unwrap(),
            rmp_serde::to_vec(&wire_name).unwrap()
        );
    }
}

fn push_messagepack<T: serde::Serialize + ?Sized>(bytes: &mut Vec<u8>, value: &T) {
    bytes.extend(rmp_serde::to_vec(value).expect("encode fixture value"));
}

fn accepted_receipt_bytes(inner_fields: u8) -> Vec<u8> {
    let mut bytes = vec![0x81];
    push_messagepack(&mut bytes, "accepted");
    bytes.push(0x80 | inner_fields);
    for (key, value) in [
        (
            "command_id",
            rmp_serde::to_vec(&protocol_command_id(0x66)).unwrap(),
        ),
        (
            "operation_id",
            rmp_serde::to_vec(&protocol_operation_id(0x67)).unwrap(),
        ),
        (
            "task_revision",
            rmp_serde::to_vec(&Option::<u64>::None).unwrap(),
        ),
        (
            "event_ids",
            rmp_serde::to_vec(&vec![protocol_event_id(0x68)]).unwrap(),
        ),
    ]
    .into_iter()
    .take(usize::from(inner_fields.min(4)))
    {
        push_messagepack(&mut bytes, key);
        bytes.extend(value);
    }
    bytes
}

#[test]
fn protocol_command_receipt_rejects_alternate_or_open_shapes() {
    let codec = MessagePackCodec::from_limits(FrameLimits::v1_default()).expect("codec");

    let positional_payload = (
        protocol_command_id(0x66),
        protocol_operation_id(0x67),
        Option::<u64>::None,
        vec![protocol_event_id(0x68)],
    );
    let mut positional = vec![0x81];
    push_messagepack(&mut positional, "accepted");
    push_messagepack(&mut positional, &positional_payload);
    assert_eq!(
        codec.decode::<CommandReceipt>(&positional),
        Err(MessagePackError::Decode)
    );

    let mut duplicate = accepted_receipt_bytes(4);
    let inner_offset = 1 + rmp_serde::to_vec(&"accepted").unwrap().len();
    assert_eq!(duplicate[inner_offset], 0x84);
    duplicate[inner_offset] = 0x85;
    push_messagepack(&mut duplicate, "command_id");
    push_messagepack(&mut duplicate, &protocol_command_id(0x66));
    assert_eq!(
        codec.decode::<CommandReceipt>(&duplicate),
        Err(MessagePackError::Decode)
    );

    let mut unknown = accepted_receipt_bytes(4);
    unknown[inner_offset] = 0x85;
    push_messagepack(&mut unknown, "future_field");
    unknown.push(0xc0);
    assert_eq!(
        codec.decode::<CommandReceipt>(&unknown),
        Err(MessagePackError::Decode)
    );

    assert_eq!(
        codec.decode::<CommandReceipt>(&accepted_receipt_bytes(3)),
        Err(MessagePackError::Decode)
    );

    let mut unknown_variant = vec![0x81];
    push_messagepack(&mut unknown_variant, "future_receipt");
    unknown_variant.push(0x80);
    assert_eq!(
        codec.decode::<CommandReceipt>(&unknown_variant),
        Err(MessagePackError::Decode)
    );

    let mut multiple_variants = accepted_receipt_bytes(4);
    multiple_variants[0] = 0x82;
    push_messagepack(&mut multiple_variants, "rejected");
    multiple_variants.push(0x83);
    push_messagepack(&mut multiple_variants, "command_id");
    push_messagepack(&mut multiple_variants, &protocol_command_id(0x69));
    push_messagepack(&mut multiple_variants, "code");
    push_messagepack(&mut multiple_variants, &RejectionCode::NotFound);
    push_messagepack(&mut multiple_variants, "current_revision");
    push_messagepack(&mut multiple_variants, &Option::<u64>::None);
    assert_eq!(
        codec.decode::<CommandReceipt>(&multiple_variants),
        Err(MessagePackError::Decode)
    );

    #[derive(serde::Serialize)]
    #[serde(rename_all = "snake_case")]
    enum RawRejectionCode {
        FutureCode,
    }
    let mut unknown_code = vec![0x81];
    push_messagepack(&mut unknown_code, "rejected");
    unknown_code.push(0x83);
    push_messagepack(&mut unknown_code, "command_id");
    push_messagepack(&mut unknown_code, &protocol_command_id(0x6A));
    push_messagepack(&mut unknown_code, "code");
    push_messagepack(&mut unknown_code, &RawRejectionCode::FutureCode);
    push_messagepack(&mut unknown_code, "current_revision");
    push_messagepack(&mut unknown_code, &Option::<u64>::None);
    assert_eq!(
        codec.decode::<CommandReceipt>(&unknown_code),
        Err(MessagePackError::Decode)
    );

    for numeric in 0_u8..=5 {
        let mut numeric_code = vec![0x81];
        push_messagepack(&mut numeric_code, "rejected");
        numeric_code.push(0x83);
        push_messagepack(&mut numeric_code, "command_id");
        push_messagepack(&mut numeric_code, &protocol_command_id(0x6b));
        push_messagepack(&mut numeric_code, "code");
        push_messagepack(&mut numeric_code, &numeric);
        push_messagepack(&mut numeric_code, "current_revision");
        push_messagepack(&mut numeric_code, &Option::<u64>::None);
        assert_eq!(
            codec.decode::<CommandReceipt>(&numeric_code),
            Err(MessagePackError::Decode)
        );
    }

    let mut trailing = accepted_receipt_bytes(4);
    trailing.push(0xc0);
    assert_eq!(
        codec.decode::<CommandReceipt>(&trailing),
        Err(MessagePackError::TrailingBytes {
            offset: u32::try_from(trailing.len() - 1).unwrap(),
        })
    );
}

#[test]
fn protocol_query_envelope_is_one_strict_named_correlation_map() {
    let expected = QueryEnvelope {
        request_id: protocol_request_id(0x71),
        client_id: protocol_client_id(0x72),
        task_id: Some(protocol_task_id(0x73)),
        query: Query::OperationStatus {
            operation_id: protocol_operation_id(0x74),
        },
    };
    let codec = MessagePackCodec::from_limits(FrameLimits::v1_default()).expect("codec");

    let encoded = codec.encode(&expected).expect("encode query envelope");
    assert_eq!(encoded[0], 0x84, "QueryEnvelope is a four-field map");
    assert_eq!(
        rmp_serde::to_vec(&expected).expect("direct query envelope serialization")[0],
        0x84,
        "compact serializers cannot create a positional query shape"
    );
    assert_eq!(
        codec
            .decode::<QueryEnvelope>(&encoded)
            .expect("decode query envelope"),
        expected
    );

    let unscoped = QueryEnvelope {
        request_id: protocol_request_id(0x75),
        client_id: protocol_client_id(0x76),
        task_id: None,
        query: Query::OperationStatus {
            operation_id: protocol_operation_id(0x77),
        },
    };
    assert_eq!(
        codec
            .decode::<QueryEnvelope>(&codec.encode(&unscoped).expect("encode null task scope"))
            .expect("decode explicit null task scope"),
        unscoped
    );
}

#[derive(serde::Serialize)]
struct RawQueryEnvelope<Q> {
    request_id: RequestId,
    client_id: ClientId,
    task_id: Option<TaskId>,
    query: Q,
}

fn raw_query_envelope<Q>(query: Q) -> RawQueryEnvelope<Q> {
    RawQueryEnvelope {
        request_id: protocol_request_id(0x78),
        client_id: protocol_client_id(0x79),
        task_id: Some(protocol_task_id(0x7a)),
        query,
    }
}

#[derive(serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum RawUnknownQuery {
    FutureQuery,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum RawPositionalQuery {
    OperationStatus((OperationId,)),
}

#[derive(serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum RawOpenQuery {
    OperationStatus {
        operation_id: OperationId,
        future_field: bool,
    },
}

#[derive(serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum RawMissingQuery {
    OperationStatus {},
}

#[derive(serde::Serialize)]
struct RawOperationStatusPayload {
    operation_id: OperationId,
}

struct RawMultipleQuery;

impl serde::Serialize for RawMultipleQuery {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;

        let mut map = serializer.serialize_map(Some(2))?;
        map.serialize_entry(
            "operation_status",
            &RawOperationStatusPayload {
                operation_id: protocol_operation_id(0x7b),
            },
        )?;
        map.serialize_entry("future_query", &())?;
        map.end()
    }
}

#[test]
fn protocol_query_envelope_rejects_alternate_or_open_shapes() {
    let codec = MessagePackCodec::from_limits(FrameLimits::v1_default()).expect("codec");
    let valid = raw_query_envelope(Query::OperationStatus {
        operation_id: protocol_operation_id(0x7b),
    });
    let valid_bytes = rmp_serde::to_vec_named(&valid).expect("valid raw query");

    let positional = (
        valid.request_id,
        valid.client_id,
        valid.task_id,
        Query::OperationStatus {
            operation_id: protocol_operation_id(0x7b),
        },
    );
    assert_eq!(
        codec.decode::<QueryEnvelope>(&rmp_serde::to_vec(&positional).unwrap()),
        Err(MessagePackError::Decode)
    );

    let mut duplicate = valid_bytes.clone();
    duplicate[0] = 0x85;
    push_messagepack(&mut duplicate, "request_id");
    push_messagepack(&mut duplicate, &valid.request_id);
    assert_eq!(
        codec.decode::<QueryEnvelope>(&duplicate),
        Err(MessagePackError::Decode)
    );

    let mut unknown = valid_bytes.clone();
    unknown[0] = 0x85;
    push_messagepack(&mut unknown, "future_field");
    unknown.push(0xc0);
    assert_eq!(
        codec.decode::<QueryEnvelope>(&unknown),
        Err(MessagePackError::Decode)
    );

    let mut missing = valid_bytes.clone();
    let query_suffix = [
        rmp_serde::to_vec(&"query").unwrap(),
        rmp_serde::to_vec_named(&valid.query).unwrap(),
    ]
    .concat();
    assert!(missing.ends_with(&query_suffix));
    missing[0] = 0x83;
    missing.truncate(missing.len() - query_suffix.len());
    assert_eq!(
        codec.decode::<QueryEnvelope>(&missing),
        Err(MessagePackError::Decode)
    );

    for malformed_query in [
        rmp_serde::to_vec_named(&raw_query_envelope(RawUnknownQuery::FutureQuery)).unwrap(),
        rmp_serde::to_vec_named(&raw_query_envelope(RawPositionalQuery::OperationStatus((
            protocol_operation_id(0x7b),
        ))))
        .unwrap(),
        rmp_serde::to_vec_named(&raw_query_envelope(RawOpenQuery::OperationStatus {
            operation_id: protocol_operation_id(0x7b),
            future_field: true,
        }))
        .unwrap(),
        rmp_serde::to_vec_named(&raw_query_envelope(RawMissingQuery::OperationStatus {})).unwrap(),
        rmp_serde::to_vec_named(&raw_query_envelope(RawMultipleQuery)).unwrap(),
    ] {
        assert_eq!(
            codec.decode::<QueryEnvelope>(&malformed_query),
            Err(MessagePackError::Decode)
        );
    }

    let mut trailing = valid_bytes;
    trailing.push(0xc0);
    assert_eq!(
        codec.decode::<QueryEnvelope>(&trailing),
        Err(MessagePackError::TrailingBytes {
            offset: u32::try_from(trailing.len() - 1).unwrap(),
        })
    );
}

#[test]
fn protocol_operation_state_preserves_every_closed_named_shape() {
    let codec = MessagePackCodec::from_limits(FrameLimits::v1_default()).expect("codec");
    assert_eq!(
        rmp_serde::to_vec(&OperationErrorCode::SideEffectFailed).unwrap(),
        rmp_serde::to_vec(&"side_effect_failed").unwrap()
    );
    assert_eq!(
        rmp_serde::to_vec(&CancellationReason::Superseded).unwrap(),
        rmp_serde::to_vec(&"superseded").unwrap()
    );
    assert_eq!(
        rmp_serde::to_vec(&OperationUncertaintyCode::AmbiguousDispatch).unwrap(),
        rmp_serde::to_vec(&"ambiguous_dispatch").unwrap()
    );
    let states = [
        OperationState::Accepted,
        OperationState::Settled {
            settled_at_ms: -1,
            result_event_ids: vec![protocol_event_id(0x81), protocol_event_id(0x82)],
        },
        OperationState::Failed {
            settled_at_ms: 1_725_000_000_300,
            code: OperationErrorCode::SideEffectFailed,
        },
        OperationState::Cancelled {
            settled_at_ms: 1_725_000_000_301,
            reason: CancellationReason::Superseded,
        },
        OperationState::Uncertain {
            observed_at_ms: 1_725_000_000_302,
            code: OperationUncertaintyCode::AmbiguousDispatch,
        },
    ];

    for state in states {
        let encoded = codec.encode(&state).expect("encode operation state");
        assert_eq!(
            rmp_serde::to_vec(&state).expect("direct operation-state serialization"),
            encoded,
            "compact serializers must preserve the named state shape"
        );
        assert_eq!(
            codec
                .decode::<OperationState>(&encoded)
                .expect("decode operation state"),
            state
        );
        let json = serde_json::to_vec(&state).expect("encode operation state as JSON");
        assert_eq!(
            serde_json::from_slice::<OperationState>(&json)
                .expect("decode operation state from JSON"),
            state
        );
    }
}

fn operation_state_map<T: serde::Serialize + ?Sized>(variant: &str, payload: &T) -> Vec<u8> {
    let mut bytes = vec![0x81];
    push_messagepack(&mut bytes, variant);
    bytes.extend(rmp_serde::to_vec_named(payload).expect("encode named state payload"));
    bytes
}

#[derive(serde::Serialize)]
struct RawSettledState {
    settled_at_ms: i64,
    result_event_ids: Vec<EventId>,
}

#[derive(serde::Serialize)]
struct RawSettledStateMissingField {
    settled_at_ms: i64,
}

#[derive(serde::Serialize)]
struct RawCodeState<C> {
    settled_at_ms: i64,
    code: C,
}

#[derive(serde::Serialize)]
struct RawReasonState<R> {
    settled_at_ms: i64,
    reason: R,
}

#[derive(serde::Serialize)]
struct RawUncertainState<C> {
    observed_at_ms: i64,
    code: C,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum RawFutureCode {
    FutureCode,
}

#[test]
fn protocol_operation_state_rejects_alternate_or_open_shapes() {
    let codec = MessagePackCodec::from_limits(FrameLimits::v1_default()).expect("codec");
    let settled = RawSettledState {
        settled_at_ms: 1_725_000_000_310,
        result_event_ids: vec![protocol_event_id(0x83)],
    };
    let valid = operation_state_map("settled", &settled);
    let payload_offset = 1 + rmp_serde::to_vec(&"settled").unwrap().len();
    assert_eq!(valid[payload_offset], 0x82);

    let positional = operation_state_map(
        "settled",
        &(settled.settled_at_ms, vec![protocol_event_id(0x83)]),
    );
    assert_eq!(
        codec.decode::<OperationState>(&positional),
        Err(MessagePackError::Decode)
    );

    let mut duplicate = valid.clone();
    duplicate[payload_offset] = 0x83;
    push_messagepack(&mut duplicate, "settled_at_ms");
    push_messagepack(&mut duplicate, &settled.settled_at_ms);
    assert_eq!(
        codec.decode::<OperationState>(&duplicate),
        Err(MessagePackError::Decode)
    );

    let mut unknown = valid.clone();
    unknown[payload_offset] = 0x83;
    push_messagepack(&mut unknown, "future_field");
    unknown.push(0xc0);
    assert_eq!(
        codec.decode::<OperationState>(&unknown),
        Err(MessagePackError::Decode)
    );

    assert_eq!(
        codec.decode::<OperationState>(&operation_state_map(
            "settled",
            &RawSettledStateMissingField {
                settled_at_ms: settled.settled_at_ms,
            },
        )),
        Err(MessagePackError::Decode)
    );

    assert_eq!(
        codec.decode::<OperationState>(&operation_state_map("accepted", &())),
        Err(MessagePackError::Decode)
    );
    assert_eq!(
        codec.decode::<OperationState>(&operation_state_map("future_state", &())),
        Err(MessagePackError::Decode)
    );
    assert_eq!(
        codec.decode::<OperationState>(&rmp_serde::to_vec(&"future_state").unwrap()),
        Err(MessagePackError::Decode)
    );
    assert_eq!(
        codec.decode::<OperationState>(&rmp_serde::to_vec(&("accepted",)).unwrap()),
        Err(MessagePackError::Decode)
    );

    let mut multiple = valid.clone();
    multiple[0] = 0x82;
    push_messagepack(&mut multiple, "failed");
    multiple.extend(
        rmp_serde::to_vec_named(&RawCodeState {
            settled_at_ms: settled.settled_at_ms,
            code: OperationErrorCode::SideEffectFailed,
        })
        .unwrap(),
    );
    assert_eq!(
        codec.decode::<OperationState>(&multiple),
        Err(MessagePackError::Decode)
    );

    for unknown_code in [
        operation_state_map(
            "failed",
            &RawCodeState {
                settled_at_ms: 1,
                code: RawFutureCode::FutureCode,
            },
        ),
        operation_state_map(
            "cancelled",
            &RawReasonState {
                settled_at_ms: 1,
                reason: RawFutureCode::FutureCode,
            },
        ),
        operation_state_map(
            "uncertain",
            &RawUncertainState {
                observed_at_ms: 1,
                code: RawFutureCode::FutureCode,
            },
        ),
    ] {
        assert_eq!(
            codec.decode::<OperationState>(&unknown_code),
            Err(MessagePackError::Decode)
        );
    }

    for numeric_code in [
        operation_state_map(
            "failed",
            &RawCodeState {
                settled_at_ms: 1,
                code: 0_u8,
            },
        ),
        operation_state_map(
            "cancelled",
            &RawReasonState {
                settled_at_ms: 1,
                reason: 0_u8,
            },
        ),
        operation_state_map(
            "uncertain",
            &RawUncertainState {
                observed_at_ms: 1,
                code: 0_u8,
            },
        ),
    ] {
        assert_eq!(
            codec.decode::<OperationState>(&numeric_code),
            Err(MessagePackError::Decode)
        );
    }

    let mut trailing = valid;
    trailing.push(0xc0);
    assert_eq!(
        codec.decode::<OperationState>(&trailing),
        Err(MessagePackError::TrailingBytes {
            offset: u32::try_from(trailing.len() - 1).unwrap(),
        })
    );
}

#[test]
fn protocol_query_reply_preserves_request_and_operation_correlation() {
    let codec = MessagePackCodec::from_limits(FrameLimits::v1_default()).expect("codec");
    let expected_request_id = protocol_request_id(0x91);
    let expected_operation_id = protocol_operation_id(0x92);
    let success = QueryReply {
        request_id: expected_request_id,
        outcome: QueryOutcome::Ok(QueryResult::OperationStatus {
            operation_id: expected_operation_id,
            state: OperationState::Settled {
                settled_at_ms: -1,
                result_event_ids: vec![protocol_event_id(0x93)],
            },
        }),
    };

    let encoded = codec.encode(&success).expect("encode successful reply");
    assert_eq!(encoded[0], 0x82, "QueryReply is a two-field map");
    assert_eq!(
        rmp_serde::to_vec(&success).expect("direct query-reply serialization"),
        encoded,
        "compact serializers must preserve the named reply shape"
    );
    assert_eq!(
        serde_json::from_value::<QueryReply>(serde_json::to_value(&success).unwrap()).unwrap(),
        success
    );
    let decoded = codec
        .decode::<QueryReply>(&encoded)
        .expect("decode successful reply");
    assert_eq!(decoded.request_id, expected_request_id);
    match decoded.outcome {
        QueryOutcome::Ok(QueryResult::OperationStatus {
            operation_id,
            state,
        }) => {
            assert_eq!(operation_id, expected_operation_id);
            assert!(matches!(state, OperationState::Settled { .. }));
        }
        QueryOutcome::Ok(other) => panic!("expected operation status reply, got {other:?}"),
        QueryOutcome::Err(error) => panic!("expected successful reply, got {error:?}"),
    }

    let rejected = QueryReply {
        request_id: protocol_request_id(0x94),
        outcome: QueryOutcome::Err(QueryError::NotFound),
    };
    assert_eq!(
        codec
            .decode::<QueryReply>(&codec.encode(&rejected).expect("encode error reply"))
            .expect("decode error reply"),
        rejected
    );
    assert_eq!(
        serde_json::from_value::<QueryReply>(serde_json::to_value(&rejected).unwrap()).unwrap(),
        rejected
    );
}

#[derive(serde::Serialize)]
struct RawQueryReply<O> {
    request_id: RequestId,
    outcome: O,
}

fn raw_query_reply<O>(outcome: O) -> RawQueryReply<O> {
    RawQueryReply {
        request_id: protocol_request_id(0x95),
        outcome,
    }
}

#[derive(serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum RawOkOutcome<R> {
    Ok(R),
}

#[derive(serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum RawErrOutcome<E> {
    Err(E),
}

struct RawUnknownOutcome;

impl serde::Serialize for RawUnknownOutcome {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;

        let mut map = serializer.serialize_map(Some(1))?;
        map.serialize_entry("future_outcome", &())?;
        map.end()
    }
}

struct RawMultipleOutcome;

impl serde::Serialize for RawMultipleOutcome {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;

        let mut map = serializer.serialize_map(Some(2))?;
        map.serialize_entry(
            "ok",
            &QueryResult::OperationStatus {
                operation_id: protocol_operation_id(0x96),
                state: OperationState::Accepted,
            },
        )?;
        map.serialize_entry("err", &QueryError::NotFound)?;
        map.end()
    }
}

struct RawUnknownQueryResult;

impl serde::Serialize for RawUnknownQueryResult {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;

        let mut map = serializer.serialize_map(Some(1))?;
        map.serialize_entry("future_result", &())?;
        map.end()
    }
}

#[derive(serde::Serialize)]
struct RawOperationStatusResultPayload {
    operation_id: OperationId,
    state: OperationState,
}

struct RawMultipleQueryResult;

impl serde::Serialize for RawMultipleQueryResult {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;

        let payload = RawOperationStatusResultPayload {
            operation_id: protocol_operation_id(0x96),
            state: OperationState::Accepted,
        };
        let mut map = serializer.serialize_map(Some(2))?;
        map.serialize_entry("operation_status", &payload)?;
        map.serialize_entry("future_result", &())?;
        map.end()
    }
}

#[derive(serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum RawPositionalQueryResult {
    OperationStatus((OperationId, OperationState)),
}

#[derive(serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum RawOpenQueryResult {
    OperationStatus {
        operation_id: OperationId,
        state: OperationState,
        future_field: bool,
    },
}

#[derive(serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum RawMissingQueryResult {
    OperationStatus { operation_id: OperationId },
}

#[derive(serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum RawFutureQueryError {
    FutureError,
}

struct RawNumericQueryErrorOutcome;

impl serde::Serialize for RawNumericQueryErrorOutcome {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;

        let mut map = serializer.serialize_map(Some(1))?;
        map.serialize_entry("err", &0_u8)?;
        map.end()
    }
}

#[test]
fn protocol_query_reply_rejects_alternate_or_open_shapes() {
    let codec = MessagePackCodec::from_limits(FrameLimits::v1_default()).expect("codec");
    let valid = raw_query_reply(QueryOutcome::Ok(QueryResult::OperationStatus {
        operation_id: protocol_operation_id(0x96),
        state: OperationState::Accepted,
    }));
    let valid_bytes = rmp_serde::to_vec_named(&valid).expect("valid raw reply");

    assert_eq!(
        codec
            .decode::<QueryReply>(&rmp_serde::to_vec(&(valid.request_id, &valid.outcome)).unwrap()),
        Err(MessagePackError::Decode)
    );

    let mut duplicate = valid_bytes.clone();
    duplicate[0] = 0x83;
    push_messagepack(&mut duplicate, "request_id");
    push_messagepack(&mut duplicate, &valid.request_id);
    assert_eq!(
        codec.decode::<QueryReply>(&duplicate),
        Err(MessagePackError::Decode)
    );

    let mut unknown = valid_bytes.clone();
    unknown[0] = 0x83;
    push_messagepack(&mut unknown, "future_field");
    unknown.push(0xc0);
    assert_eq!(
        codec.decode::<QueryReply>(&unknown),
        Err(MessagePackError::Decode)
    );

    let mut missing = valid_bytes.clone();
    let outcome_suffix = [
        rmp_serde::to_vec(&"outcome").unwrap(),
        rmp_serde::to_vec_named(&valid.outcome).unwrap(),
    ]
    .concat();
    assert!(missing.ends_with(&outcome_suffix));
    missing[0] = 0x81;
    missing.truncate(missing.len() - outcome_suffix.len());
    assert_eq!(
        codec.decode::<QueryReply>(&missing),
        Err(MessagePackError::Decode)
    );

    for malformed in [
        rmp_serde::to_vec_named(&raw_query_reply(RawUnknownOutcome)).unwrap(),
        rmp_serde::to_vec_named(&raw_query_reply(RawMultipleOutcome)).unwrap(),
        rmp_serde::to_vec_named(&raw_query_reply(RawOkOutcome::Ok(RawUnknownQueryResult))).unwrap(),
        rmp_serde::to_vec_named(&raw_query_reply(RawOkOutcome::Ok(RawMultipleQueryResult)))
            .unwrap(),
        rmp_serde::to_vec_named(&raw_query_reply(RawOkOutcome::Ok(
            RawPositionalQueryResult::OperationStatus((
                protocol_operation_id(0x96),
                OperationState::Accepted,
            )),
        )))
        .unwrap(),
        rmp_serde::to_vec_named(&raw_query_reply(RawOkOutcome::Ok(
            RawOpenQueryResult::OperationStatus {
                operation_id: protocol_operation_id(0x96),
                state: OperationState::Accepted,
                future_field: true,
            },
        )))
        .unwrap(),
        rmp_serde::to_vec_named(&raw_query_reply(RawOkOutcome::Ok(
            RawMissingQueryResult::OperationStatus {
                operation_id: protocol_operation_id(0x96),
            },
        )))
        .unwrap(),
        rmp_serde::to_vec_named(&raw_query_reply(RawErrOutcome::Err(
            RawFutureQueryError::FutureError,
        )))
        .unwrap(),
        rmp_serde::to_vec_named(&raw_query_reply(RawNumericQueryErrorOutcome)).unwrap(),
    ] {
        assert_eq!(
            codec.decode::<QueryReply>(&malformed),
            Err(MessagePackError::Decode)
        );
    }

    let mut trailing = valid_bytes;
    trailing.push(0xc0);
    assert_eq!(
        codec.decode::<QueryReply>(&trailing),
        Err(MessagePackError::TrailingBytes {
            offset: u32::try_from(trailing.len() - 1).unwrap(),
        })
    );
}

#[test]
fn protocol_task_snapshot_query_is_strict_empty_named_payload() {
    let codec = MessagePackCodec::from_limits(FrameLimits::v1_default()).expect("codec");
    let expected = QueryEnvelope {
        request_id: protocol_request_id(0xa1),
        client_id: protocol_client_id(0xa2),
        task_id: Some(protocol_task_id(0xa3)),
        query: Query::TaskSnapshot,
    };
    let encoded = codec.encode(&expected).expect("encode task snapshot query");
    assert_eq!(
        codec
            .decode::<QueryEnvelope>(&encoded)
            .expect("decode task snapshot query"),
        expected
    );

    #[derive(serde::Serialize)]
    #[serde(rename_all = "snake_case")]
    enum RawOpenTaskSnapshot {
        TaskSnapshot { future_field: bool },
    }
    assert_eq!(
        codec.decode::<QueryEnvelope>(
            &rmp_serde::to_vec_named(&raw_query_envelope(RawOpenTaskSnapshot::TaskSnapshot {
                future_field: true,
            }))
            .unwrap()
        ),
        Err(MessagePackError::Decode)
    );
}

#[test]
fn protocol_client_request_and_server_response_are_one_strict_named_variant() {
    let codec = MessagePackCodec::from_limits(FrameLimits::v1_default()).expect("codec");
    let command = ClientRequest::Command(CommandEnvelope {
        command_id: protocol_command_id(0xb1),
        client_id: protocol_client_id(0xb2),
        task_id: None,
        issued_at_ms: 1_725_000_000_100,
        expected_task_revision: None,
        command: Command::BeginCloseTask,
    });
    let query = ClientRequest::Query(QueryEnvelope {
        request_id: protocol_request_id(0xb3),
        client_id: protocol_client_id(0xb4),
        task_id: Some(protocol_task_id(0xb5)),
        query: Query::TaskSnapshot,
    });

    for request in [&command, &query] {
        let encoded = codec.encode(request).expect("encode client request");
        assert_eq!(encoded[0], 0x81, "ClientRequest is a one-entry map");
        assert_eq!(
            &codec
                .decode::<ClientRequest>(&encoded)
                .expect("decode client request"),
            request
        );
    }

    let receipt = ServerMessage::CommandReceipt(CommandReceipt::Accepted {
        command_id: protocol_command_id(0xb6),
        operation_id: protocol_operation_id(0xb7),
        task_revision: Some(1),
        event_ids: vec![protocol_event_id(0xb8)],
    });
    let reply = ServerMessage::QueryReply(QueryReply {
        request_id: protocol_request_id(0xb9),
        outcome: QueryOutcome::Err(QueryError::NotFound),
    });
    for response in [&receipt, &reply] {
        let encoded = codec.encode(response).expect("encode server message");
        assert_eq!(encoded[0], 0x81, "ServerMessage is a one-entry map");
        assert_eq!(
            &codec
                .decode::<ServerMessage>(&encoded)
                .expect("decode server message"),
            response
        );
    }

    #[derive(serde::Serialize)]
    #[serde(rename_all = "snake_case")]
    enum RawUnknownRequest {
        FutureRequest,
    }
    assert_eq!(
        codec.decode::<ClientRequest>(
            &rmp_serde::to_vec_named(&RawUnknownRequest::FutureRequest).unwrap()
        ),
        Err(MessagePackError::Decode)
    );
    #[derive(serde::Serialize)]
    #[serde(rename_all = "snake_case")]
    enum RawUnknownResponse {
        FutureResponse,
    }
    assert_eq!(
        codec.decode::<ServerMessage>(
            &rmp_serde::to_vec_named(&RawUnknownResponse::FutureResponse).unwrap()
        ),
        Err(MessagePackError::Decode)
    );
}

#[test]
fn protocol_server_message_unsolicited_variants_are_strict_named_maps() {
    let codec = MessagePackCodec::from_limits(FrameLimits::v1_default()).expect("codec");
    let event = DomainEvent {
        id: protocol_event_id(0xd1),
        task_id: Some(protocol_task_id(0xd2)),
        sequence: 7,
        task_revision: Some(3),
        occurred_at_ms: 1_725_000_000_700,
        payload: Event::TaskRenamed {
            title: "Duplex event".into(),
        },
    };
    let durable = ServerMessage::DurableEvent {
        subscription_id: protocol_subscription_id(0xd3),
        event: event.clone(),
    };
    let resync = ServerMessage::ResyncRequired {
        subscription_id: protocol_subscription_id(0xd4),
        last_delivered_sequence: 7,
        newest_sequence: 12,
    };

    for message in [&durable, &resync] {
        let encoded = codec.encode(message).expect("encode unsolicited");
        assert_eq!(
            encoded[0], 0x81,
            "ServerMessage unsolicited is a one-entry map"
        );
        assert_eq!(
            &codec
                .decode::<ServerMessage>(&encoded)
                .expect("decode unsolicited"),
            message
        );
    }

    #[derive(serde::Serialize)]
    struct OpenDurablePayload {
        subscription_id: SubscriptionId,
        event: DomainEvent,
        future_field: bool,
    }
    struct OpenDurable {
        event: DomainEvent,
    }
    impl serde::Serialize for OpenDurable {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            use serde::ser::SerializeMap;
            let mut map = serializer.serialize_map(Some(1))?;
            map.serialize_entry(
                "durable_event",
                &OpenDurablePayload {
                    subscription_id: protocol_subscription_id(0xd5),
                    event: self.event.clone(),
                    future_field: true,
                },
            )?;
            map.end()
        }
    }
    assert_eq!(
        codec.decode::<ServerMessage>(
            &rmp_serde::to_vec_named(&OpenDurable {
                event: event.clone()
            })
            .unwrap()
        ),
        Err(MessagePackError::Decode),
        "durable_event must reject unknown payload fields"
    );

    #[derive(serde::Serialize)]
    struct MissingResyncPayload {
        subscription_id: SubscriptionId,
        last_delivered_sequence: u64,
    }
    struct MissingResync;
    impl serde::Serialize for MissingResync {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            use serde::ser::SerializeMap;
            let mut map = serializer.serialize_map(Some(1))?;
            map.serialize_entry(
                "resync_required",
                &MissingResyncPayload {
                    subscription_id: protocol_subscription_id(0xd6),
                    last_delivered_sequence: 1,
                },
            )?;
            map.end()
        }
    }
    assert_eq!(
        codec.decode::<ServerMessage>(&rmp_serde::to_vec_named(&MissingResync).unwrap()),
        Err(MessagePackError::Decode),
        "resync_required must reject missing newest_sequence"
    );

    #[derive(serde::Serialize)]
    struct PositionalDurable(SubscriptionId, DomainEvent);
    struct PositionalDurableMessage {
        event: DomainEvent,
    }
    impl serde::Serialize for PositionalDurableMessage {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            use serde::ser::SerializeMap;
            let mut map = serializer.serialize_map(Some(1))?;
            map.serialize_entry(
                "durable_event",
                &PositionalDurable(protocol_subscription_id(0xd7), self.event.clone()),
            )?;
            map.end()
        }
    }
    assert_eq!(
        codec.decode::<ServerMessage>(
            &rmp_serde::to_vec_named(&PositionalDurableMessage {
                event: event.clone()
            })
            .unwrap()
        ),
        Err(MessagePackError::Decode),
        "durable_event must reject positional payload forms"
    );

    #[derive(serde::Serialize)]
    struct ExactDurablePayload {
        subscription_id: SubscriptionId,
        event: DomainEvent,
    }
    struct MultipleVariants {
        event: DomainEvent,
    }
    impl serde::Serialize for MultipleVariants {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            use serde::ser::SerializeMap;
            let mut map = serializer.serialize_map(Some(2))?;
            map.serialize_entry(
                "command_receipt",
                &CommandReceipt::Rejected {
                    command_id: protocol_command_id(0xd8),
                    code: RejectionCode::NotFound,
                    current_revision: None,
                },
            )?;
            map.serialize_entry(
                "durable_event",
                &ExactDurablePayload {
                    subscription_id: protocol_subscription_id(0xd9),
                    event: self.event.clone(),
                },
            )?;
            map.end()
        }
    }
    assert_eq!(
        codec.decode::<ServerMessage>(
            &rmp_serde::to_vec_named(&MultipleVariants { event }).unwrap()
        ),
        Err(MessagePackError::Decode),
        "ServerMessage must reject multiple top-level variants"
    );
}

fn protocol_resource_id(tail: u8) -> ResourceId {
    ResourceId::from_bytes(protocol_uuid_v7(tail)).expect("resource id")
}

fn protocol_stream_frame(kind: u16) -> StreamFrame {
    StreamFrame {
        subscription_id: protocol_subscription_id(0xe1),
        stream: StreamKey::from(protocol_resource_id(0xe2)),
        generation: 3,
        sequence: 9,
        payload_kind: StreamPayloadKind::new(kind).expect("nonzero kind"),
        schema_version: 1,
        payload: b"ephemeral-state".to_vec(),
    }
}

#[test]
fn stream_frame_and_server_message_stream_round_trip_named_map() {
    // Catches: StreamFrame / ServerMessage::Stream must be a strict seven-field
    // named map under outer wire key exactly "stream".
    let codec = MessagePackCodec::from_limits(FrameLimits::v1_default()).expect("codec");
    let frame = protocol_stream_frame(7);
    let message = ServerMessage::Stream(frame.clone());
    let encoded = codec.encode(&message).expect("encode stream");
    assert_eq!(encoded[0], 0x81, "ServerMessage::Stream is a one-entry map");
    assert_eq!(
        &codec
            .decode::<ServerMessage>(&encoded)
            .expect("decode stream"),
        &message
    );

    struct StreamProbe {
        subscription_id: SubscriptionId,
        stream: StreamKey,
        generation: u64,
        sequence: u64,
        payload_kind: StreamPayloadKind,
        schema_version: u16,
        payload: Vec<u8>,
    }
    impl serde::Serialize for StreamProbe {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            use serde::ser::SerializeMap;
            struct Bin<'a>(&'a [u8]);
            impl serde::Serialize for Bin<'_> {
                fn serialize<Ser>(&self, serializer: Ser) -> Result<Ser::Ok, Ser::Error>
                where
                    Ser: serde::Serializer,
                {
                    serializer.serialize_bytes(self.0)
                }
            }
            let mut map = serializer.serialize_map(Some(7))?;
            map.serialize_entry("subscription_id", &self.subscription_id)?;
            map.serialize_entry("stream", &self.stream)?;
            map.serialize_entry("generation", &self.generation)?;
            map.serialize_entry("sequence", &self.sequence)?;
            map.serialize_entry("payload_kind", &self.payload_kind)?;
            map.serialize_entry("schema_version", &self.schema_version)?;
            map.serialize_entry("payload", &Bin(&self.payload))?;
            map.end()
        }
    }
    struct OuterStream {
        inner: StreamProbe,
    }
    impl serde::Serialize for OuterStream {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            use serde::ser::SerializeMap;
            let mut map = serializer.serialize_map(Some(1))?;
            map.serialize_entry("stream", &self.inner)?;
            map.end()
        }
    }
    let probe = codec
        .encode(&OuterStream {
            inner: StreamProbe {
                subscription_id: frame.subscription_id,
                stream: frame.stream,
                generation: frame.generation,
                sequence: frame.sequence,
                payload_kind: frame.payload_kind,
                schema_version: frame.schema_version,
                payload: frame.payload.clone(),
            },
        })
        .expect("encode probe");
    assert_eq!(
        probe, encoded,
        "stable outer key must be exactly `stream` with seven named inner fields"
    );
}

#[test]
fn stream_frame_rejects_zero_payload_kind_and_malformed_payloads() {
    // Catches: zero payload_kind and missing/duplicate/unknown/positional stream
    // payloads must fail closed at construction/deserialization.
    let codec = MessagePackCodec::from_limits(FrameLimits::v1_default()).expect("codec");
    assert!(
        StreamPayloadKind::new(0).is_none(),
        "zero StreamPayloadKind must be rejected at construction"
    );

    struct BinPayload(Vec<u8>);
    impl serde::Serialize for BinPayload {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            serializer.serialize_bytes(&self.0)
        }
    }

    struct StreamPayloadRaw {
        subscription_id: SubscriptionId,
        stream: StreamKey,
        generation: u64,
        sequence: u64,
        payload_kind: u16,
        schema_version: u16,
        payload: BinPayload,
    }
    impl serde::Serialize for StreamPayloadRaw {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            use serde::ser::SerializeMap;
            let mut map = serializer.serialize_map(Some(7))?;
            map.serialize_entry("subscription_id", &self.subscription_id)?;
            map.serialize_entry("stream", &self.stream)?;
            map.serialize_entry("generation", &self.generation)?;
            map.serialize_entry("sequence", &self.sequence)?;
            map.serialize_entry("payload_kind", &self.payload_kind)?;
            map.serialize_entry("schema_version", &self.schema_version)?;
            map.serialize_entry("payload", &self.payload)?;
            map.end()
        }
    }
    struct StreamMessageRaw {
        inner: StreamPayloadRaw,
    }
    impl serde::Serialize for StreamMessageRaw {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            use serde::ser::SerializeMap;
            let mut map = serializer.serialize_map(Some(1))?;
            map.serialize_entry("stream", &self.inner)?;
            map.end()
        }
    }
    assert_eq!(
        codec.decode::<ServerMessage>(
            &rmp_serde::to_vec_named(&StreamMessageRaw {
                inner: StreamPayloadRaw {
                    subscription_id: protocol_subscription_id(0xe3),
                    stream: StreamKey::from(protocol_resource_id(0xe4)),
                    generation: 1,
                    sequence: 2,
                    payload_kind: 0,
                    schema_version: 1,
                    payload: BinPayload(b"x".to_vec()),
                }
            })
            .unwrap()
        ),
        Err(MessagePackError::Decode),
        "zero payload_kind must be rejected on the wire"
    );

    #[derive(serde::Serialize)]
    struct MissingStreamPayload {
        subscription_id: SubscriptionId,
        stream: StreamKey,
        generation: u64,
        sequence: u64,
        payload_kind: u16,
        schema_version: u16,
    }
    struct MissingStream;
    impl serde::Serialize for MissingStream {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            use serde::ser::SerializeMap;
            let mut map = serializer.serialize_map(Some(1))?;
            map.serialize_entry(
                "stream",
                &MissingStreamPayload {
                    subscription_id: protocol_subscription_id(0xe5),
                    stream: StreamKey::from(protocol_resource_id(0xe6)),
                    generation: 1,
                    sequence: 1,
                    payload_kind: 1,
                    schema_version: 1,
                },
            )?;
            map.end()
        }
    }
    assert_eq!(
        codec.decode::<ServerMessage>(&rmp_serde::to_vec_named(&MissingStream).unwrap()),
        Err(MessagePackError::Decode),
        "stream must reject missing payload field"
    );

    struct OpenStreamPayload {
        subscription_id: SubscriptionId,
        stream: StreamKey,
        generation: u64,
        sequence: u64,
        payload_kind: u16,
        schema_version: u16,
        payload: BinPayload,
        future_field: bool,
    }
    impl serde::Serialize for OpenStreamPayload {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            use serde::ser::SerializeMap;
            let mut map = serializer.serialize_map(Some(8))?;
            map.serialize_entry("subscription_id", &self.subscription_id)?;
            map.serialize_entry("stream", &self.stream)?;
            map.serialize_entry("generation", &self.generation)?;
            map.serialize_entry("sequence", &self.sequence)?;
            map.serialize_entry("payload_kind", &self.payload_kind)?;
            map.serialize_entry("schema_version", &self.schema_version)?;
            map.serialize_entry("payload", &self.payload)?;
            map.serialize_entry("future_field", &self.future_field)?;
            map.end()
        }
    }
    struct OpenStream;
    impl serde::Serialize for OpenStream {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            use serde::ser::SerializeMap;
            let mut map = serializer.serialize_map(Some(1))?;
            map.serialize_entry(
                "stream",
                &OpenStreamPayload {
                    subscription_id: protocol_subscription_id(0xe7),
                    stream: StreamKey::from(protocol_resource_id(0xe8)),
                    generation: 1,
                    sequence: 1,
                    payload_kind: 1,
                    schema_version: 1,
                    payload: BinPayload(b"x".to_vec()),
                    future_field: true,
                },
            )?;
            map.end()
        }
    }
    assert_eq!(
        codec.decode::<ServerMessage>(&rmp_serde::to_vec_named(&OpenStream).unwrap()),
        Err(MessagePackError::Decode),
        "stream must reject unknown payload fields"
    );

    // Explicit array-payload rejection with otherwise-valid fields.
    {
        use serde::ser::SerializeMap;
        struct ArrayPayloadStream;
        impl serde::Serialize for ArrayPayloadStream {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: serde::Serializer,
            {
                let mut outer = serializer.serialize_map(Some(1))?;
                struct Inner;
                impl serde::Serialize for Inner {
                    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
                    where
                        S: serde::Serializer,
                    {
                        let mut map = serializer.serialize_map(Some(7))?;
                        map.serialize_entry("subscription_id", &protocol_subscription_id(0xed))?;
                        map.serialize_entry(
                            "stream",
                            &StreamKey::from(protocol_resource_id(0xee)),
                        )?;
                        map.serialize_entry("generation", &1u64)?;
                        map.serialize_entry("sequence", &1u64)?;
                        map.serialize_entry("payload_kind", &1u16)?;
                        map.serialize_entry("schema_version", &1u16)?;
                        map.serialize_entry("payload", &vec![0x61u8])?;
                        map.end()
                    }
                }
                outer.serialize_entry("stream", &Inner)?;
                outer.end()
            }
        }
        assert_eq!(
            codec.decode::<ServerMessage>(&rmp_serde::to_vec_named(&ArrayPayloadStream).unwrap()),
            Err(MessagePackError::Decode),
            "stream payload must reject MessagePack array bytes"
        );
    }

    let valid_stream = protocol_stream_frame(1);
    let valid_message = ServerMessage::Stream(valid_stream.clone());
    let mut duplicate = codec.encode(&valid_message).expect("valid stream bytes");
    // Outer one-entry map, then "stream" key, then seven-field inner map marker.
    assert_eq!(duplicate[0], 0x81);
    let stream_key = rmp_serde::to_vec("stream").expect("stream key");
    assert_eq!(&duplicate[1..1 + stream_key.len()], stream_key.as_slice());
    let inner_offset = 1 + stream_key.len();
    assert_eq!(
        duplicate[inner_offset], 0x87,
        "StreamFrame inner map must declare seven fields"
    );
    duplicate[inner_offset] = 0x88;
    push_messagepack(&mut duplicate, "generation");
    push_messagepack(&mut duplicate, &99u64);
    assert_eq!(
        codec.decode::<ServerMessage>(&duplicate),
        Err(MessagePackError::Decode),
        "stream must reject duplicate fields"
    );

    #[derive(serde::Serialize)]
    struct PositionalStream(SubscriptionId, StreamKey, u64, u64, u16, u16, Vec<u8>);
    struct PositionalStreamMessage;
    impl serde::Serialize for PositionalStreamMessage {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            use serde::ser::SerializeMap;
            let mut map = serializer.serialize_map(Some(1))?;
            map.serialize_entry(
                "stream",
                &PositionalStream(
                    protocol_subscription_id(0xeb),
                    StreamKey::from(protocol_resource_id(0xec)),
                    1,
                    1,
                    1,
                    1,
                    b"x".to_vec(),
                ),
            )?;
            map.end()
        }
    }
    assert_eq!(
        codec.decode::<ServerMessage>(&rmp_serde::to_vec_named(&PositionalStreamMessage).unwrap()),
        Err(MessagePackError::Decode),
        "stream must reject positional payload forms"
    );
}

#[test]
fn stream_frame_large_binary_payload_round_trips_without_collection_ceiling() {
    // Catches: StreamFrame.payload must be MessagePack binary bytes, not an array,
    // so the codec collection preflight cannot impose a 1000-byte payload limit.
    let codec = MessagePackCodec::from_limits(FrameLimits::v1_default()).expect("codec");
    let payload = vec![0x5a; 4096];
    let frame = StreamFrame {
        subscription_id: protocol_subscription_id(0xf1),
        stream: StreamKey::from(protocol_resource_id(0xf2)),
        generation: 1,
        sequence: 1,
        payload_kind: StreamPayloadKind::new(1).expect("kind"),
        schema_version: 1,
        payload: payload.clone(),
    };
    let message = ServerMessage::Stream(frame.clone());
    let encoded = codec
        .encode(&message)
        .expect("4096-byte stream payload must encode");
    let decoded = codec
        .decode::<ServerMessage>(&encoded)
        .expect("4096-byte stream payload must decode");
    assert_eq!(decoded, message);

    // Locate the payload field value marker after the "payload" key.
    let payload_key = rmp_serde::to_vec("payload").expect("payload key");
    let key_at = encoded
        .windows(payload_key.len())
        .position(|window| window == payload_key.as_slice())
        .expect("encoded stream must contain payload key");
    let marker = encoded[key_at + payload_key.len()];
    assert!(
        matches!(marker, 0xc4 | 0xc5 | 0xc6),
        "payload must be MessagePack binary (bin8/bin16/bin32), got marker 0x{marker:02x}"
    );
    assert!(
        !matches!(marker, 0x90..=0x9f | 0xdc | 0xdd),
        "payload must not be a MessagePack array"
    );
}

fn protocol_environment_id(tail: u8) -> EnvironmentId {
    EnvironmentId::from_bytes(protocol_uuid_v7(tail)).expect("environment id")
}

fn protocol_project_id(tail: u8) -> ProjectId {
    ProjectId::from_bytes(protocol_uuid_v7(tail)).expect("project id")
}

fn protocol_create_task_intent(task_tail: u8) -> CreateTaskIntent {
    CreateTaskIntent {
        id: protocol_task_id(task_tail),
        environment_id: protocol_environment_id(0xc1),
        title: "Strict create".into(),
        description: None,
        project_id: protocol_project_id(0xc2),
        workspace: WorkspaceRef::Main,
        assignment: TaskAssignment::LocalOwner,
        created_at_ms: 1_725_000_000_000,
        connectivity: TaskConnectivity::Connected,
        attention: TaskAttention::None,
        activity: TaskActivity::Idle,
        review_readiness: ReviewReadiness::NotReady,
    }
}

#[test]
fn protocol_client_request_create_task_rejects_unknown_intent_field_and_multiple_variants() {
    let codec = MessagePackCodec::from_limits(FrameLimits::v1_default()).expect("codec");

    #[derive(serde::Serialize)]
    struct RawOpenCreateTaskIntent {
        id: TaskId,
        environment_id: EnvironmentId,
        title: String,
        description: Option<String>,
        project_id: ProjectId,
        workspace: WorkspaceRef,
        assignment: TaskAssignment,
        created_at_ms: i64,
        connectivity: TaskConnectivity,
        attention: TaskAttention,
        activity: TaskActivity,
        review_readiness: ReviewReadiness,
        future_field: bool,
    }

    #[derive(serde::Serialize)]
    #[serde(rename_all = "snake_case")]
    enum RawOpenCreateCommand {
        CreateTask(RawOpenCreateTaskIntent),
    }

    let intent = protocol_create_task_intent(0xc3);
    let open_intent = RawOpenCreateTaskIntent {
        id: intent.id,
        environment_id: intent.environment_id,
        title: intent.title.clone(),
        description: intent.description.clone(),
        project_id: intent.project_id,
        workspace: intent.workspace.clone(),
        assignment: intent.assignment.clone(),
        created_at_ms: intent.created_at_ms,
        connectivity: intent.connectivity,
        attention: intent.attention,
        activity: intent.activity,
        review_readiness: intent.review_readiness,
        future_field: true,
    };
    #[derive(serde::Serialize)]
    struct RawClientCommandRequest<C> {
        command: RawCommandEnvelope<C>,
    }
    assert_eq!(
        codec.decode::<ClientRequest>(
            &rmp_serde::to_vec_named(&RawClientCommandRequest {
                command: raw_command_envelope(RawOpenCreateCommand::CreateTask(open_intent)),
            })
            .unwrap()
        ),
        Err(MessagePackError::Decode),
        "unknown CreateTaskIntent field must be rejected"
    );

    let valid_command = ClientRequest::Command(CommandEnvelope {
        command_id: protocol_command_id(0xc6),
        client_id: protocol_client_id(0xc7),
        task_id: None,
        issued_at_ms: 1_725_000_000_100,
        expected_task_revision: None,
        command: Command::CreateTask(protocol_create_task_intent(0xc8)),
    });
    assert_eq!(
        codec
            .decode::<ClientRequest>(
                &codec
                    .encode(&valid_command)
                    .expect("encode valid create request")
            )
            .expect("decode valid create request"),
        valid_command
    );

    struct RawMultipleClientRequest;
    impl serde::Serialize for RawMultipleClientRequest {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            use serde::ser::SerializeMap;
            let mut map = serializer.serialize_map(Some(2))?;
            map.serialize_entry(
                "command",
                &CommandEnvelope {
                    command_id: protocol_command_id(0xc9),
                    client_id: protocol_client_id(0xca),
                    task_id: None,
                    issued_at_ms: 1_725_000_000_100,
                    expected_task_revision: None,
                    command: Command::BeginCloseTask,
                },
            )?;
            map.serialize_entry(
                "query",
                &QueryEnvelope {
                    request_id: protocol_request_id(0xcb),
                    client_id: protocol_client_id(0xcc),
                    task_id: Some(protocol_task_id(0xcd)),
                    query: Query::TaskSnapshot,
                },
            )?;
            map.end()
        }
    }
    assert_eq!(
        codec.decode::<ClientRequest>(&rmp_serde::to_vec_named(&RawMultipleClientRequest).unwrap()),
        Err(MessagePackError::Decode),
        "ClientRequest must reject multiple top-level variants"
    );
}

#[test]
fn protocol_nested_create_task_snapshot_and_command_reject_unknown_fields() {
    use devmanager::domain::{AgentSessionId, TaskFacts, TaskLifecycle, TaskSnapshotItem};

    let codec = MessagePackCodec::from_limits(FrameLimits::v1_default()).expect("codec");
    let intent = protocol_create_task_intent(0xd1);

    #[derive(serde::Serialize)]
    struct RawClientCommandRequest<C> {
        command: RawCommandEnvelope<C>,
    }

    #[derive(serde::Serialize)]
    #[serde(rename_all = "snake_case")]
    enum RawOpenWorkspace {
        Main { future_field: bool },
    }
    #[derive(serde::Serialize)]
    struct RawIntentWithWorkspace {
        id: TaskId,
        environment_id: EnvironmentId,
        title: String,
        description: Option<String>,
        project_id: ProjectId,
        workspace: RawOpenWorkspace,
        assignment: TaskAssignment,
        created_at_ms: i64,
        connectivity: TaskConnectivity,
        attention: TaskAttention,
        activity: TaskActivity,
        review_readiness: ReviewReadiness,
    }
    #[derive(serde::Serialize)]
    #[serde(rename_all = "snake_case")]
    enum RawCreateWithWorkspace {
        CreateTask(RawIntentWithWorkspace),
    }
    assert_eq!(
        codec.decode::<ClientRequest>(
            &rmp_serde::to_vec_named(&RawClientCommandRequest {
                command: raw_command_envelope(RawCreateWithWorkspace::CreateTask(
                    RawIntentWithWorkspace {
                        id: intent.id,
                        environment_id: intent.environment_id,
                        title: intent.title.clone(),
                        description: None,
                        project_id: intent.project_id,
                        workspace: RawOpenWorkspace::Main { future_field: true },
                        assignment: TaskAssignment::LocalOwner,
                        created_at_ms: intent.created_at_ms,
                        connectivity: intent.connectivity,
                        attention: intent.attention,
                        activity: intent.activity,
                        review_readiness: intent.review_readiness,
                    },
                )),
            })
            .unwrap()
        ),
        Err(MessagePackError::Decode),
        "unknown nested workspace field must be rejected"
    );

    #[derive(serde::Serialize)]
    #[serde(rename_all = "snake_case")]
    enum RawOpenAssignment {
        LocalOwner { future_field: bool },
    }
    #[derive(serde::Serialize)]
    struct RawIntentWithAssignment {
        id: TaskId,
        environment_id: EnvironmentId,
        title: String,
        description: Option<String>,
        project_id: ProjectId,
        workspace: WorkspaceRef,
        assignment: RawOpenAssignment,
        created_at_ms: i64,
        connectivity: TaskConnectivity,
        attention: TaskAttention,
        activity: TaskActivity,
        review_readiness: ReviewReadiness,
    }
    #[derive(serde::Serialize)]
    #[serde(rename_all = "snake_case")]
    enum RawCreateWithAssignment {
        CreateTask(RawIntentWithAssignment),
    }
    assert_eq!(
        codec.decode::<ClientRequest>(
            &rmp_serde::to_vec_named(&RawClientCommandRequest {
                command: raw_command_envelope(RawCreateWithAssignment::CreateTask(
                    RawIntentWithAssignment {
                        id: intent.id,
                        environment_id: intent.environment_id,
                        title: intent.title.clone(),
                        description: None,
                        project_id: intent.project_id,
                        workspace: WorkspaceRef::Main,
                        assignment: RawOpenAssignment::LocalOwner { future_field: true },
                        created_at_ms: intent.created_at_ms,
                        connectivity: intent.connectivity,
                        attention: intent.attention,
                        activity: intent.activity,
                        review_readiness: intent.review_readiness,
                    },
                )),
            })
            .unwrap()
        ),
        Err(MessagePackError::Decode),
        "unknown nested assignment field must be rejected"
    );

    let task = TaskFacts {
        id: protocol_task_id(0xd2),
        environment_id: protocol_environment_id(0xd3),
        title: "Snap".into(),
        description: None,
        project_id: protocol_project_id(0xd4),
        workspace: WorkspaceRef::Main,
        assignment: TaskAssignment::LocalOwner,
        lifecycle: TaskLifecycle::Open,
        action_epoch: 0,
        revision: 1,
        created_at_ms: 1_725_000_000_000,
    };
    let _ = TaskSnapshotItem {
        task: task.clone(),
        connectivity: TaskConnectivity::Connected,
        attention: TaskAttention::None,
        activity: TaskActivity::Idle,
        review_readiness: ReviewReadiness::NotReady,
        primary_agent_id: None,
    };
    #[derive(serde::Serialize)]
    struct RawOpenTaskFacts {
        id: TaskId,
        environment_id: EnvironmentId,
        title: String,
        description: Option<String>,
        project_id: ProjectId,
        workspace: WorkspaceRef,
        assignment: TaskAssignment,
        lifecycle: TaskLifecycle,
        action_epoch: u64,
        revision: u64,
        created_at_ms: i64,
        future_field: bool,
    }
    #[derive(serde::Serialize)]
    struct RawOpenSnapshotItem {
        task: RawOpenTaskFacts,
        connectivity: TaskConnectivity,
        attention: TaskAttention,
        activity: TaskActivity,
        review_readiness: ReviewReadiness,
        primary_agent_id: Option<AgentSessionId>,
    }
    #[derive(serde::Serialize)]
    #[serde(rename_all = "snake_case")]
    enum RawOpenQueryResult {
        TaskSnapshot { snapshot: RawOpenSnapshotItem },
    }
    assert_eq!(
        codec.decode::<QueryResult>(
            &rmp_serde::to_vec_named(&RawOpenQueryResult::TaskSnapshot {
                snapshot: RawOpenSnapshotItem {
                    task: RawOpenTaskFacts {
                        id: task.id,
                        environment_id: task.environment_id,
                        title: task.title,
                        description: None,
                        project_id: task.project_id,
                        workspace: WorkspaceRef::Main,
                        assignment: TaskAssignment::LocalOwner,
                        lifecycle: TaskLifecycle::Open,
                        action_epoch: 0,
                        revision: 1,
                        created_at_ms: task.created_at_ms,
                        future_field: true,
                    },
                    connectivity: TaskConnectivity::Connected,
                    attention: TaskAttention::None,
                    activity: TaskActivity::Idle,
                    review_readiness: ReviewReadiness::NotReady,
                    primary_agent_id: None,
                },
            })
            .unwrap()
        ),
        Err(MessagePackError::Decode),
        "unknown TaskFacts field in TaskSnapshot must be rejected"
    );

    let agent = AgentSessionId::from_bytes(protocol_uuid_v7(0xd5)).expect("agent");
    #[derive(serde::Serialize)]
    #[serde(rename_all = "snake_case")]
    enum RawOpenSetPrimaryAgent {
        SetPrimaryAgent {
            agent_session_id: AgentSessionId,
            future_field: bool,
        },
    }
    assert_eq!(
        codec.decode::<ClientRequest>(
            &rmp_serde::to_vec_named(&RawClientCommandRequest {
                command: raw_command_envelope(RawOpenSetPrimaryAgent::SetPrimaryAgent {
                    agent_session_id: agent,
                    future_field: true,
                }),
            })
            .unwrap()
        ),
        Err(MessagePackError::Decode),
        "unknown Command struct-variant field must be rejected"
    );
}

#[test]
fn protocol_event_replay_queries_and_results_round_trip_named_maps() {
    let codec = MessagePackCodec::from_limits(FrameLimits::v1_default()).expect("codec");
    let subscription_id = protocol_subscription_id(0xe1);
    let page = EventPage {
        after_sequence: 0,
        through_sequence: 3,
        events: Vec::new(),
        next_cursor: Some(vec![0x01, 0x02]),
    };

    for query in [
        Query::OpenEventReplay { after_sequence: 0 },
        Query::ContinueEventReplay {
            subscription_id,
            resume_cursor: vec![0xaa, 0xbb],
        },
        Query::ReleaseEventReplay { subscription_id },
    ] {
        let encoded = codec.encode(&query).expect("encode replay query");
        assert_eq!(encoded[0], 0x81, "Query is a one-entry named map");
        assert_eq!(
            codec
                .decode::<Query>(&encoded)
                .expect("decode replay query"),
            query
        );
    }

    for result in [
        QueryResult::EventReplayPage {
            subscription_id,
            page: page.clone(),
        },
        QueryResult::EventReplayReleased { subscription_id },
    ] {
        let encoded = codec.encode(&result).expect("encode replay result");
        assert_eq!(encoded[0], 0x81, "QueryResult is a one-entry named map");
        assert_eq!(
            codec
                .decode::<QueryResult>(&encoded)
                .expect("decode replay result"),
            result
        );
    }

    let envelope = QueryEnvelope {
        request_id: protocol_request_id(0xe2),
        client_id: protocol_client_id(0xe3),
        task_id: None,
        query: Query::OpenEventReplay { after_sequence: 0 },
    };
    assert_eq!(
        codec
            .decode::<QueryEnvelope>(&codec.encode(&envelope).expect("encode replay envelope"))
            .expect("decode replay envelope"),
        envelope
    );

    let reply = QueryReply {
        request_id: protocol_request_id(0xe4),
        outcome: QueryOutcome::Ok(QueryResult::EventReplayPage {
            subscription_id,
            page,
        }),
    };
    assert_eq!(
        codec
            .decode::<QueryReply>(&codec.encode(&reply).expect("encode replay reply"))
            .expect("decode replay reply"),
        reply
    );
}

#[test]
fn protocol_query_error_replay_unavailable_is_strict_named_map() {
    let codec = MessagePackCodec::from_limits(FrameLimits::v1_default()).expect("codec");
    let error = QueryError::ReplayUnavailable {
        oldest_sequence: 4,
        newest_sequence: 12,
    };
    let encoded = codec.encode(&error).expect("encode replay unavailable");
    assert_eq!(
        encoded[0], 0x81,
        "ReplayUnavailable must be a one-entry named map, not a bare string"
    );
    assert_eq!(
        codec
            .decode::<QueryError>(&encoded)
            .expect("decode replay unavailable"),
        error
    );
    assert_eq!(
        serde_json::from_value::<QueryError>(serde_json::to_value(&error).unwrap()).unwrap(),
        error
    );

    let reply = QueryReply {
        request_id: protocol_request_id(0xe5),
        outcome: QueryOutcome::Err(error),
    };
    assert_eq!(
        codec
            .decode::<QueryReply>(&codec.encode(&reply).expect("encode error reply"))
            .expect("decode error reply"),
        reply
    );

    #[derive(serde::Serialize)]
    #[serde(rename_all = "snake_case")]
    enum RawStringReplayUnavailable {
        ReplayUnavailable,
    }
    assert_eq!(
        codec.decode::<QueryError>(
            &rmp_serde::to_vec_named(&RawStringReplayUnavailable::ReplayUnavailable).unwrap()
        ),
        Err(MessagePackError::Decode),
        "bare replay_unavailable string must be rejected"
    );

    #[derive(serde::Serialize)]
    struct RawOpenReplayUnavailable {
        oldest_sequence: u64,
        newest_sequence: u64,
        future_field: bool,
    }
    struct RawOpenReplayUnavailableError;
    impl serde::Serialize for RawOpenReplayUnavailableError {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            use serde::ser::SerializeMap;
            let mut map = serializer.serialize_map(Some(1))?;
            map.serialize_entry(
                "replay_unavailable",
                &RawOpenReplayUnavailable {
                    oldest_sequence: 1,
                    newest_sequence: 2,
                    future_field: true,
                },
            )?;
            map.end()
        }
    }
    assert_eq!(
        codec.decode::<QueryError>(
            &rmp_serde::to_vec_named(&RawOpenReplayUnavailableError).unwrap()
        ),
        Err(MessagePackError::Decode),
        "unknown replay_unavailable field must be rejected"
    );

    #[derive(serde::Serialize)]
    #[serde(rename_all = "snake_case")]
    enum RawOpenEventReplayQuery {
        OpenEventReplay {
            after_sequence: u64,
            future_field: bool,
        },
    }
    assert_eq!(
        codec.decode::<Query>(
            &rmp_serde::to_vec_named(&RawOpenEventReplayQuery::OpenEventReplay {
                after_sequence: 0,
                future_field: true,
            })
            .unwrap()
        ),
        Err(MessagePackError::Decode),
        "unknown open_event_replay field must be rejected"
    );

    #[derive(serde::Serialize)]
    #[serde(rename_all = "snake_case")]
    enum RawMissingContinueEventReplay {
        ContinueEventReplay { subscription_id: SubscriptionId },
    }
    assert_eq!(
        codec.decode::<Query>(
            &rmp_serde::to_vec_named(&RawMissingContinueEventReplay::ContinueEventReplay {
                subscription_id: protocol_subscription_id(0xe6),
            })
            .unwrap()
        ),
        Err(MessagePackError::Decode),
        "continue_event_replay missing resume_cursor must be rejected"
    );
}
