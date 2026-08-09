use std::collections::HashSet;

use serde::Serialize;
use uuid::Uuid;

use devmanager::connect::{
    canonical_snapshot_page_size, payload_catalog, ChannelBinding, ChannelId, ChannelKind,
    ChunkContext, ChunkFrame, ConnectEnvelope, ConnectLimits, ConnectPayload, ConnectPrivacyClass,
    ConnectionId, GenericExtensionPayload, PayloadKind, SessionId, UnknownPayload,
};
use devmanager::domain::command::CommandReceipt;
use devmanager::domain::event::DomainEvent;
use devmanager::domain::id::{
    ClientId, CommandId, EventId, OperationId, RequestId, ResourceId, SnapshotId, SubscriptionId,
    TaskId, TransferId,
};
use devmanager::domain::operation::{OperationOutcome, OperationOutcomeKind, OutcomeSource};
use devmanager::domain::query::{Query, QueryEnvelope, QueryError, QueryOutcome, QueryReply};
use devmanager::domain::snapshot::{EventPage, SnapshotPage, SnapshotSection};
use devmanager::protocol::{
    ClientRequest, ProtocolVersion, ServerMessage, StreamFrame, StreamKey, StreamPayloadKind,
};

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

fn connect_id<T>(tail: u8) -> T
where
    T: TryFrom<Uuid>,
    <T as TryFrom<Uuid>>::Error: std::fmt::Debug,
{
    T::try_from(uuid_v7(tail)).expect("Connect id")
}

fn domain_id<T>(tail: u8) -> T
where
    T: FromBytes,
{
    T::from_bytes(uuid_v7(tail).into_bytes()).expect("domain id")
}

trait FromBytes: Sized {
    type Error: std::fmt::Debug;

    fn from_bytes(bytes: [u8; 16]) -> Result<Self, Self::Error>;
}

macro_rules! impl_from_bytes {
    ($($type:ty),+ $(,)?) => {
        $(
            impl FromBytes for $type {
                type Error = devmanager::domain::id::IdError;

                fn from_bytes(bytes: [u8; 16]) -> Result<Self, Self::Error> {
                    <$type>::from_bytes(bytes)
                }
            }
        )+
    };
}

impl_from_bytes!(
    ClientId,
    CommandId,
    EventId,
    OperationId,
    RequestId,
    ResourceId,
    SnapshotId,
    SubscriptionId,
    TaskId,
    TransferId,
);

fn binding(tail: u8) -> ChannelBinding {
    ChannelBinding::new(
        connect_id::<ConnectionId>(tail),
        connect_id::<SessionId>(tail + 1),
        connect_id::<ChannelId>(tail + 2),
    )
}

fn query_request(tail: u8) -> QueryEnvelope {
    QueryEnvelope {
        request_id: domain_id::<RequestId>(tail),
        client_id: domain_id::<ClientId>(tail + 1),
        task_id: Some(domain_id::<TaskId>(tail + 2)),
        query: Query::TaskSnapshot,
    }
}

fn query_payload(tail: u8) -> ConnectPayload {
    ConnectPayload::Request(ClientRequest::Query(query_request(tail)))
}

#[derive(Serialize)]
struct CatalogFixture {
    entries: Vec<CatalogFixtureEntry>,
}

#[derive(Serialize)]
struct CatalogFixtureEntry {
    kind: u16,
    name: &'static str,
    channel: ChannelKind,
    version: u16,
    action: bool,
    max_payload_bytes: u32,
}

#[test]
fn catalog_metadata_golden_covers_every_v1_entry() {
    let entries = payload_catalog()
        .iter()
        .map(|descriptor| CatalogFixtureEntry {
            kind: descriptor.kind.get(),
            name: descriptor.name,
            channel: descriptor.channel,
            version: descriptor.version,
            action: descriptor.action,
            max_payload_bytes: descriptor.max_payload_bytes,
        })
        .collect::<Vec<_>>();
    assert_eq!(entries.len(), 22);

    let kinds = entries
        .iter()
        .map(|entry| entry.kind)
        .collect::<HashSet<_>>();
    let names = entries
        .iter()
        .map(|entry| entry.name)
        .collect::<HashSet<_>>();
    assert_eq!(kinds.len(), entries.len(), "catalog tags must be unique");
    assert_eq!(names.len(), entries.len(), "catalog names must be unique");
    assert!(entries.iter().all(|entry| {
        entry.version == 1
            && entry.max_payload_bytes == devmanager::connect::MAX_CONNECT_REASSEMBLED_MESSAGE_BYTES
    }));

    let encoded = rmp_serde::to_vec_named(&CatalogFixture { entries }).expect("catalog fixture");
    if !encoded.is_empty() {
        println!(
            "CATALOG_GOLDEN_HEX={}",
            encoded
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<Vec<_>>()
                .join("")
        );
    }
    assert_eq!(
        encoded,
        hex_fixture(include_str!("fixtures/connect/v1/catalog.msgpack.hex")),
        "v1 catalog metadata must stay deterministic"
    );
}

fn empty_snapshot_page(tail: u8) -> SnapshotPage {
    SnapshotPage {
        snapshot_id: domain_id::<SnapshotId>(tail),
        through_sequence: 4,
        section: SnapshotSection::Tasks,
        after_item: None,
        items: Vec::new(),
        encoded_bytes: 0,
        next_cursor: None,
    }
}

#[test]
fn canonical_catalog_derives_typed_payload_metadata_and_round_trips_without_binary_wrapping() {
    let limits = ConnectLimits::v1_default();
    let payload = query_payload(10);

    assert_eq!(payload.kind(), PayloadKind::QUERY);
    assert_eq!(payload.channel(), ChannelKind::Critical);
    assert_eq!(payload.version(), 1);
    assert!(payload_catalog().iter().any(|entry| {
        entry.kind == PayloadKind::QUERY
            && entry.channel == ChannelKind::Critical
            && entry.version == 1
    }));

    let encoded = payload.encode(limits).expect("encode typed payload");
    let decoded = ConnectPayload::decode(payload.kind(), payload.version(), &encoded, limits)
        .expect("decode typed payload");
    assert_eq!(decoded, payload);

    // The payload is a named typed map. It is not serialized once into a
    // binary field and then serialized again as an envelope payload.
    assert!(encoded.contains(&0x81), "typed payload map marker missing");
}

#[test]
fn typed_envelope_derives_channel_kind_and_version_and_matches_golden_fixture() {
    let limits = ConnectLimits::v1_default();
    let payload = query_payload(20);
    let envelope = ConnectEnvelope::new(
        binding(30),
        7,
        Some(domain_id::<RequestId>(20)),
        None,
        limits,
        ConnectPrivacyClass::ManagedMetadata,
        payload.clone(),
    )
    .expect("typed envelope");

    assert_eq!(envelope.channel(), ChannelKind::Critical);
    assert_eq!(envelope.payload_kind(), PayloadKind::QUERY);
    assert_eq!(envelope.payload_version(), 1);
    assert_eq!(envelope.payload(), &payload);

    let encoded = envelope.encode().expect("encode envelope");
    let expected = hex_fixture(include_str!(
        "fixtures/connect/v1/query-envelope.msgpack.hex"
    ));
    assert_eq!(
        encoded, expected,
        "v1 envelope bytes must stay deterministic"
    );
    assert_eq!(
        ConnectEnvelope::decode_with_limits(&encoded, limits).expect("decode envelope"),
        envelope
    );
}

#[test]
fn query_and_reply_use_distinct_catalog_discriminants_and_round_trip() {
    let limits = ConnectLimits::v1_default();
    let query = query_payload(24);
    let reply = ConnectPayload::Message(ServerMessage::QueryReply(QueryReply {
        request_id: domain_id::<RequestId>(24),
        outcome: QueryOutcome::Err(QueryError::NotFound),
    }));

    assert_eq!(query.kind(), PayloadKind::QUERY);
    assert_eq!(reply.kind(), PayloadKind::QUERY_REPLY);
    assert_ne!(query.kind(), reply.kind());

    let encoded = reply.encode(limits).expect("query reply payload");
    assert_eq!(
        ConnectPayload::decode(reply.kind(), reply.version(), &encoded, limits)
            .expect("decode query reply"),
        reply
    );
}

#[test]
fn envelope_round_trip_preserves_the_validated_negotiated_protocol_version() {
    let version = ProtocolVersion::new(
        devmanager::connect::CONNECT_PROTOCOL_MAJOR,
        devmanager::connect::CONNECT_PROTOCOL_MINOR,
    );
    let envelope = ConnectEnvelope::new_with_version(
        version,
        binding(26),
        1,
        Some(domain_id::<RequestId>(26)),
        None,
        ConnectLimits::v1_default(),
        ConnectPrivacyClass::ManagedMetadata,
        query_payload(26),
    )
    .expect("versioned envelope");

    assert_eq!(envelope.protocol_version(), version);
    let decoded = ConnectEnvelope::decode(&envelope.encode().expect("encode envelope"))
        .expect("decode envelope");
    assert_eq!(decoded.protocol_version(), version);
}

#[derive(Serialize)]
struct EnvelopeWire {
    protocol_major: u16,
    protocol_minor: u16,
    connection_id: ConnectionId,
    session_id: SessionId,
    channel_id: ChannelId,
    channel: ChannelKind,
    sequence: u64,
    request_id: Option<RequestId>,
    operation_id: Option<OperationId>,
    limits: ConnectLimits,
    compression: devmanager::connect::Compression,
    privacy_class: ConnectPrivacyClass,
    payload_kind: PayloadKind,
    payload_version: u16,
    payload: ConnectPayload,
}

fn wire_for(envelope: &ConnectEnvelope) -> EnvelopeWire {
    EnvelopeWire {
        protocol_major: devmanager::connect::CONNECT_PROTOCOL_MAJOR,
        protocol_minor: devmanager::connect::CONNECT_PROTOCOL_MINOR,
        connection_id: envelope.binding().connection_id,
        session_id: envelope.binding().session_id,
        channel_id: envelope.binding().channel_id,
        channel: envelope.channel(),
        sequence: envelope.sequence(),
        request_id: envelope.request_id(),
        operation_id: envelope.operation_id(),
        limits: envelope.limits(),
        compression: devmanager::connect::Compression::None,
        privacy_class: envelope.privacy_class(),
        payload_kind: envelope.payload_kind(),
        payload_version: envelope.payload_version(),
        payload: envelope.payload().clone(),
    }
}

#[test]
fn envelope_decode_rejects_unknown_fields_arrays_and_derived_metadata_mismatches() {
    let envelope = ConnectEnvelope::new(
        binding(40),
        1,
        Some(domain_id::<RequestId>(40)),
        None,
        ConnectLimits::v1_default(),
        ConnectPrivacyClass::ManagedMetadata,
        query_payload(40),
    )
    .expect("typed envelope");

    let mut wrong_channel = wire_for(&envelope);
    wrong_channel.channel = ChannelKind::Durable;
    assert!(ConnectEnvelope::decode(&rmp_serde::to_vec_named(&wrong_channel).unwrap()).is_err());

    let mut wrong_kind = wire_for(&envelope);
    wrong_kind.payload_kind = PayloadKind::SNAPSHOT_PAGE;
    assert!(ConnectEnvelope::decode(&rmp_serde::to_vec_named(&wrong_kind).unwrap()).is_err());

    let mut wrong_version = wire_for(&envelope);
    wrong_version.payload_version = 2;
    assert!(ConnectEnvelope::decode(&rmp_serde::to_vec_named(&wrong_version).unwrap()).is_err());

    #[derive(Serialize)]
    struct UnknownField {
        unknown: u8,
        #[serde(flatten)]
        envelope: EnvelopeWire,
    }
    let unknown = UnknownField {
        unknown: 1,
        envelope: wire_for(&envelope),
    };
    assert!(ConnectEnvelope::decode(&rmp_serde::to_vec_named(&unknown).unwrap()).is_err());

    let positional = rmp_serde::to_vec(&(
        devmanager::connect::CONNECT_PROTOCOL_MAJOR,
        devmanager::connect::CONNECT_PROTOCOL_MINOR,
        envelope.binding().connection_id,
        envelope.binding().session_id,
        envelope.binding().channel_id,
        envelope.channel(),
        envelope.sequence(),
        envelope.request_id(),
        envelope.operation_id(),
        envelope.limits(),
        devmanager::connect::Compression::None,
        envelope.privacy_class(),
        envelope.payload_kind(),
        envelope.payload_version(),
        envelope.payload().clone(),
    ))
    .unwrap();
    assert!(ConnectEnvelope::decode(&positional).is_err());
}

#[test]
fn limits_validate_final_deserialized_values_and_checked_cumulative_bounds() {
    let mut invalid = ConnectLimits::v1_default();
    invalid.max_cursor_bytes = 0;
    let invalid_bytes = rmp_serde::to_vec_named(&serde_json::json!({
        "max_physical_frame_bytes": invalid.max_physical_frame_bytes,
        "max_reassembled_message_bytes": invalid.max_reassembled_message_bytes,
        "max_page_items": invalid.max_page_items,
        "max_page_encoded_bytes": invalid.max_page_encoded_bytes,
        "max_chunk_bytes": invalid.max_chunk_bytes,
        "max_cumulative_bytes": invalid.max_cumulative_bytes,
        "max_cursor_bytes": invalid.max_cursor_bytes,
    }))
    .unwrap();
    let mut deserializer = rmp_serde::Deserializer::new(std::io::Cursor::new(invalid_bytes));
    assert!(<ConnectLimits as serde::Deserialize>::deserialize(&mut deserializer).is_err());

    let limits =
        ConnectLimits::try_new(4096, 8192, 10, 2048, 1024, 4096, 1024).expect("bounded limits");
    assert!(matches!(
        limits.validate_chunk(u64::MAX, &[1]),
        Err(devmanager::connect::ConnectLimitError::CumulativeOverflow)
    ));
    assert!(limits.validate_cursor_len(1025).is_err());
}

#[test]
fn pages_recompute_canonical_size_and_do_not_trust_claimed_encoded_bytes() {
    let limits = ConnectLimits::v1_default();
    let mut page = empty_snapshot_page(50);
    page.encoded_bytes = canonical_snapshot_page_size(&page).expect("canonical size");
    let payload = ConnectPayload::SnapshotPage(page.clone());
    payload.validate(limits).expect("valid page");

    page.encoded_bytes += 1;
    assert!(ConnectPayload::SnapshotPage(page).validate(limits).is_err());

    let events = ConnectPayload::EventPage(EventPage {
        after_sequence: 0,
        through_sequence: 0,
        events: Vec::<DomainEvent>::new(),
        next_cursor: None,
    });
    events.validate(limits).expect("valid empty event page");
}

#[test]
fn chunk_uses_transfer_id_and_validates_index_count_digest_cumulative_and_cursor_context() {
    let limits =
        ConnectLimits::try_new(4096, 8192, 10, 2048, 1024, 4096, 1024).expect("bounded limits");
    let transfer_id = domain_id::<TransferId>(60);
    let cursor = vec![0x71, 0x72];
    let frame =
        ChunkFrame::new(transfer_id, 0, 1, vec![1, 2, 3], Some(cursor.clone())).expect("chunk");
    frame.validate(limits).expect("valid chunk");

    let mut context = ChunkContext::new(transfer_id, 1, Some(cursor)).expect("chunk context");
    context.accept(&frame).expect("context accepts chunk");
    assert!(context.is_complete());

    let mut bad_digest = frame.clone();
    bad_digest.digest[0] ^= 1;
    assert!(bad_digest.validate(limits).is_err());

    let mut bad_index = frame.clone();
    bad_index.index = 1;
    assert!(bad_index.validate(limits).is_err());

    let mut bad_cursor = frame;
    bad_cursor.cursor = Some(vec![0; 1025]);
    assert!(bad_cursor.validate(limits).is_err());
}

#[test]
fn stream_payload_reuses_stream_frame_and_settlement_never_promotes_acceptance_to_completion() {
    let resource_id = domain_id::<ResourceId>(70);
    let stream = StreamFrame {
        subscription_id: domain_id(71),
        stream: StreamKey::from(resource_id),
        generation: 1,
        sequence: 1,
        payload_kind: StreamPayloadKind::new(1).unwrap(),
        schema_version: 1,
        payload: vec![1, 2, 3],
    };
    ConnectPayload::TerminalDelta(stream)
        .validate(ConnectLimits::v1_default())
        .expect("valid stream");

    let command_id = domain_id::<CommandId>(72);
    let operation_id = domain_id::<OperationId>(73);
    let event_id = domain_id::<EventId>(74);
    let receipt = CommandReceipt::Accepted {
        command_id,
        operation_id,
        task_revision: Some(1),
        event_ids: vec![event_id],
    };
    let outcome = OperationOutcome::new(
        operation_id,
        2,
        None,
        None,
        OutcomeSource::Dispatch,
        OperationOutcomeKind::Settled {
            result_event_ids: vec![event_id],
        },
    )
    .expect("outcome");
    let settlement =
        devmanager::connect::OperationSettlementPayload::new(operation_id, outcome.clone())
            .expect("settlement");
    ConnectPayload::OperationSettlement(settlement)
        .validate(ConnectLimits::v1_default())
        .expect("valid settlement");

    let settlement = devmanager::connect::OperationSettlementPayload::new(operation_id, outcome)
        .expect("settlement replay");
    let settlement_bytes = ConnectPayload::OperationSettlement(settlement)
        .encode(ConnectLimits::v1_default())
        .expect("encode settlement");
    assert!(!settlement_bytes
        .windows(b"accepted".len())
        .any(|window| window == b"accepted"));

    let mismatched = devmanager::connect::OperationSettlementPayload::new(
        domain_id::<OperationId>(75),
        OperationOutcome::new(
            operation_id,
            2,
            None,
            None,
            OutcomeSource::Dispatch,
            OperationOutcomeKind::Settled {
                result_event_ids: vec![event_id],
            },
        )
        .expect("outcome"),
    );
    assert!(
        mismatched.is_err(),
        "settlement correlation must be authoritative"
    );

    let accepted = ConnectPayload::Message(ServerMessage::CommandReceipt(receipt));
    assert_eq!(accepted.kind(), PayloadKind::COMMAND_RECEIPT);
    assert!(matches!(
        accepted,
        ConnectPayload::Message(ServerMessage::CommandReceipt(_))
    ));
}

#[test]
fn unknown_payloads_are_bounded_inert_and_generic_extensions_are_typed() {
    let limits = ConnectLimits::v1_default();
    let kind = PayloadKind::new(0x7fff).expect("unknown kind");
    let unknown = ConnectPayload::Unknown(
        UnknownPayload::new(kind, 9, vec![1, 2, 3]).expect("unknown payload"),
    );
    assert!(!unknown.is_action());
    assert_eq!(unknown.kind(), kind);
    assert_eq!(
        ConnectPayload::decode(kind, 9, &unknown.encode(limits).unwrap(), limits).unwrap(),
        unknown
    );

    let extension =
        ConnectPayload::Extension(GenericExtensionPayload::new(0x33, 1, vec![4, 5]).unwrap());
    extension.validate(limits).expect("generic extension");
    assert!(!extension.is_action());
}

fn hex_fixture(value: &str) -> Vec<u8> {
    let compact: String = value
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect();
    compact
        .as_bytes()
        .chunks(2)
        .map(|byte| {
            u8::from_str_radix(std::str::from_utf8(byte).expect("fixture hex"), 16)
                .expect("fixture byte")
        })
        .collect()
}
