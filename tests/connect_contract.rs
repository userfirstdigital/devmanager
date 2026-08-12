use serde::Serialize;
use uuid::Uuid;

use devmanager::connect::{
    canonical_schema_fixtures, encode_canonical_schema, payload_catalog, ActionId, ChannelBinding,
    ChannelId, ChannelKind, ConnectEnvelope, ConnectLimitError, ConnectLimits, ConnectPayload,
    ConnectPrivacyClass, ConnectionId, EnvelopeError, GenericExtensionPayload, HelloPayload,
    PayloadDecodeError, PayloadKind, SessionId, MAX_CONNECT_DIAGNOSTIC_BYTES,
};
use devmanager::domain::id::{ClientId, RequestId, TaskId};
use devmanager::domain::query::{Query, QueryEnvelope};
use devmanager::protocol::CapabilitySet;

fn uuid_v7(tail: u8) -> Uuid {
    let mut bytes = [0_u8; 16];
    bytes[0] = 0x01;
    bytes[1] = 0x23;
    bytes[2] = 0x45;
    bytes[3] = 0x67;
    bytes[4] = 0x89;
    bytes[5] = 0xab;
    bytes[6] = 0x70;
    bytes[8] = 0x80;
    bytes[15] = tail;
    Uuid::from_bytes(bytes)
}

fn uuid_v4() -> Uuid {
    Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").expect("stable UUIDv4")
}

fn binding(tail: u8) -> ChannelBinding {
    ChannelBinding::new(
        ConnectionId::from_uuid(uuid_v7(tail)).unwrap(),
        SessionId::from_uuid(uuid_v7(tail + 1)).unwrap(),
        ChannelId::from_uuid(uuid_v7(tail + 2)).unwrap(),
    )
}

fn query_payload(tail: u8) -> ConnectPayload {
    ConnectPayload::Query(QueryEnvelope {
        request_id: RequestId::from_bytes(uuid_v7(tail).into_bytes()).unwrap(),
        client_id: ClientId::from_bytes(uuid_v7(tail + 1).into_bytes()).unwrap(),
        task_id: Some(TaskId::from_bytes(uuid_v7(tail + 2).into_bytes()).unwrap()),
        query: Query::TaskSnapshot,
    })
}

#[test]
fn opaque_ids_accept_only_rfc4122_uuidv7() {
    assert!(ConnectionId::from_uuid(uuid_v7(1)).is_ok());
    assert!(matches!(
        ConnectionId::from_uuid(uuid_v4()),
        Err(devmanager::connect::ConnectIdError::InvalidVersion)
    ));
    assert_eq!(
        ConnectionId::from_uuid(uuid_v7(1)).unwrap().as_uuid(),
        uuid_v7(1)
    );
}

#[test]
fn canonical_payload_catalog_decodes_typed_values_and_keeps_unknown_extensions_inert() {
    let limits = ConnectLimits::v1_default();
    let payload = query_payload(10);
    let bytes = payload.encode(limits).unwrap();
    let decoded = ConnectPayload::decode(PayloadKind::QUERY, 1, &bytes, limits).unwrap();
    assert!(matches!(decoded, ConnectPayload::Query(_)));
    assert!(!decoded.is_action());
    assert!(decoded.as_command().is_none());

    assert!(matches!(
        ConnectPayload::decode(PayloadKind::QUERY, 2, &bytes, limits),
        Err(PayloadDecodeError::UnsupportedVersion { .. })
    ));

    let extension =
        ConnectPayload::decode(PayloadKind::new(0x7fff).unwrap(), 9, &[0x91, 0x01], limits)
            .unwrap();
    assert!(matches!(
        extension,
        ConnectPayload::Extension(GenericExtensionPayload {
            type_id: 0x7fff,
            schema_version: 9,
            ..
        })
    ));
    assert!(!extension.is_action());
    assert!(extension.as_command().is_none());
    assert_eq!(extension.kind(), PayloadKind::new(0x7fff).unwrap());

    let catalog = payload_catalog();
    assert_eq!(catalog.len(), 17);
    assert!(catalog
        .iter()
        .any(|descriptor| descriptor.kind == PayloadKind::COMMAND && descriptor.action));
    assert!(catalog
        .iter()
        .all(|descriptor| descriptor.max_payload_bytes > 0));
    assert_eq!(
        catalog
            .iter()
            .filter(|descriptor| descriptor.action)
            .count(),
        1
    );
}

#[test]
fn envelope_uses_typed_payload_and_rejects_channel_or_privacy_ambiguity() {
    let limits = ConnectLimits::v1_default();
    let bound = binding(20);
    let envelope = ConnectEnvelope::new(
        bound,
        ChannelKind::Critical,
        1,
        None,
        None,
        limits,
        ConnectPrivacyClass::ManagedMetadata,
        query_payload(30),
    )
    .unwrap();

    let bytes = envelope.encode().unwrap();
    let decoded = ConnectEnvelope::decode_with_limits(&bytes, limits).unwrap();
    assert!(matches!(
        decoded.decode_payload().unwrap(),
        ConnectPayload::Query(_)
    ));
    assert_eq!(decoded.binding().unwrap(), bound);
    assert!(!decoded.is_action_payload());

    assert!(matches!(
        ConnectEnvelope::new(
            bound,
            ChannelKind::Durable,
            1,
            None,
            None,
            limits,
            ConnectPrivacyClass::ManagedMetadata,
            query_payload(31),
        ),
        Err(EnvelopeError::ChannelMismatch)
    ));
    assert!(matches!(
        ConnectEnvelope::new(
            bound,
            ChannelKind::Critical,
            1,
            None,
            None,
            limits,
            ConnectPrivacyClass::RawContent,
            query_payload(32),
        ),
        Err(EnvelopeError::PrivacyViolation)
    ));
}

#[test]
fn negotiated_limits_cover_zero_overflow_pages_chunks_cursors_and_diagnostics() {
    assert!(ConnectLimits::try_new(0, 8192, 10, 2048, 1024, 4096).is_err());
    assert!(ConnectLimits::try_new(4096, 8192, 0, 2048, 1024, 4096).is_err());

    let limits = ConnectLimits::try_new(4096, 8192, 10, 2048, 1024, 4096).unwrap();
    assert!(matches!(
        limits.validate_chunk(u64::MAX, &[1]),
        Err(ConnectLimitError::CumulativeOverflow)
    ));
    assert!(matches!(
        limits.validate_chunk(0, &[]),
        Err(ConnectLimitError::EmptyChunk)
    ));
    assert!(limits.validate_page(10, 2048).is_ok());
    assert!(limits.validate_page(11, 1).is_err());
    assert!(limits.validate_cursor_len(1).is_ok());
    assert!(matches!(
        limits.validate_cursor_len(0),
        Err(ConnectLimitError::CursorEmpty)
    ));
    assert!(matches!(
        limits.validate_diagnostic_len(0),
        Err(ConnectLimitError::DiagnosticEmpty)
    ));
    assert!(matches!(
        limits.validate_diagnostic_len(MAX_CONNECT_DIAGNOSTIC_BYTES as usize + 1),
        Err(ConnectLimitError::DiagnosticExceeded { .. })
    ));

    let oversized = ConnectPayload::Error(devmanager::connect::ErrorPayload {
        code: 1,
        message: "x".repeat(limits.max_diagnostic_bytes() as usize + 1),
    });
    assert!(oversized.encode(limits).is_err());
}

#[test]
fn decode_rejects_unknown_fields_empty_payloads_and_zero_kinds() {
    let limits = ConnectLimits::v1_default();
    #[derive(Serialize)]
    struct UnknownHello {
        extra: u8,
        capabilities: CapabilitySet,
        limits: ConnectLimits,
        privacy_class: ConnectPrivacyClass,
    }
    let unknown = rmp_serde::to_vec_named(&UnknownHello {
        extra: 1,
        capabilities: CapabilitySet::empty(),
        limits,
        privacy_class: ConnectPrivacyClass::LocalOnly,
    })
    .unwrap();
    assert!(ConnectPayload::decode(PayloadKind::HELLO, 1, &unknown, limits).is_err());
    assert!(PayloadKind::new(0).is_none());
    assert!(ConnectPayload::decode(PayloadKind::HELLO, 1, &[], limits).is_err());
    assert!(matches!(
        ConnectPayload::Hello(HelloPayload {
            capabilities: CapabilitySet::empty(),
            limits,
            privacy_class: ConnectPrivacyClass::RawContent,
        })
        .encode(limits),
        Err(PayloadDecodeError::Ambiguous { .. })
    ));
}

#[test]
fn canonical_schema_fixtures_are_complete_deterministic_and_local_first() {
    let limits = ConnectLimits::v1_default();
    let fixtures = canonical_schema_fixtures();
    let catalog = payload_catalog();
    assert_eq!(fixtures.len(), catalog.len());
    for descriptor in catalog {
        assert!(
            fixtures
                .iter()
                .any(|fixture| fixture.name == descriptor.name),
            "missing canonical fixture {}",
            descriptor.name
        );
    }

    let first = encode_canonical_schema(limits).unwrap();
    let second = encode_canonical_schema(limits).unwrap();
    assert_eq!(first, second);

    for (name, bytes) in &first {
        let fixture = fixtures
            .iter()
            .find(|fixture| fixture.name == *name)
            .unwrap();
        let decoded = ConnectPayload::decode(
            fixture.payload.kind(),
            fixture.payload.version(),
            bytes,
            limits,
        )
        .unwrap();
        assert_eq!(decoded.kind(), fixture.payload.kind());
        assert_eq!(decoded.channel(), descriptor_channel(name));
        assert_eq!(decoded.is_action(), *name == "command");
        if *name == "hello" {
            let ConnectPayload::Hello(hello) = decoded else {
                panic!("hello fixture");
            };
            assert_eq!(hello.privacy_class, ConnectPrivacyClass::LocalOnly);
        }
        assert!(
            !matches!(decoded, ConnectPayload::Error(ref error) if error.message.contains("secret")
                || error.message.contains("token")
                || error.message.contains("transcript"))
        );
    }
}

fn descriptor_channel(name: &str) -> ChannelKind {
    payload_catalog()
        .iter()
        .find(|descriptor| descriptor.name == name)
        .unwrap()
        .channel
}

#[test]
fn unknown_actions_fail_closed_even_when_the_wire_kind_is_known() {
    assert!(ActionId::new(0x7fff).unwrap().known().is_none());
    let limits = ConnectLimits::v1_default();
    let smuggled = ConnectPayload::Extension(GenericExtensionPayload {
        type_id: PayloadKind::COMMAND.get(),
        schema_version: 1,
        payload: vec![0x91, 0x01],
    });
    assert!(smuggled.encode(limits).is_err());
    assert!(!smuggled.is_action());
}
