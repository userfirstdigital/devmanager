//! Stable protocol compatibility and safety contracts.

use std::io::{Cursor, Error, ErrorKind, Read, Write};

use devmanager::protocol::{
    Capability, CapabilitySet, FrameLimitField, FrameLimits, FrameLimitsError, MessagePackCodec,
    MessagePackError, MessagePackLengthKind, PhysicalFrameCodec, PhysicalFrameError,
    ProtocolVersion, VersionNegotiationError, MAX_MESSAGEPACK_COLLECTION_ITEMS,
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
