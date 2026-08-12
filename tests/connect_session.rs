use std::collections::VecDeque;

use serde::Serialize;
use uuid::Uuid;

use devmanager::connect::{
    decode_inner, encode_inner, ActionId, AuthoritativePermissionContext, ChannelKind,
    ConnectEnvelope, ConnectLimits, ConnectPrivacyClass, ConnectRole, ConnectTransport,
    EphemeralPresence, FocusEpoch, LastSenderHint, PermissionDecision, PermissionDenyReason,
    PermissionEvaluator, PermissionRequest, PresenceSink, ProjectionExtensions, ProjectionSource,
    ReplayRequest, ScopedPermissionGrant, SnapshotRequest, TurnEpoch,
    MAX_CONNECT_RESUME_CURSOR_BYTES,
};
use devmanager::domain::id::{ClientId, OperationId, RequestId, SnapshotId, TaskId};
use devmanager::domain::query::{Query, QueryEnvelope, QueryReply};
use devmanager::domain::snapshot::{EventPage, PageLimits, SnapshotPage, SnapshotSection};
use devmanager::protocol::ProtocolVersion;

fn fixed_uuid_v7(tail: u8) -> Uuid {
    let mut bytes = [0_u8; 16];
    bytes[0] = 0x01;
    bytes[1] = 0x23;
    bytes[2] = 0x45;
    bytes[3] = 0x67;
    bytes[4] = 0x89;
    bytes[5] = 0xab;
    bytes[6] = 0x70;
    bytes[7] = 0xcd;
    bytes[8] = 0x80;
    bytes[9] = 0xef;
    bytes[15] = tail;
    Uuid::from_bytes(bytes)
}

fn task_id(tail: u8) -> TaskId {
    TaskId::from_bytes(fixed_uuid_v7(tail).into_bytes()).expect("task id")
}

fn client_id(tail: u8) -> ClientId {
    ClientId::from_bytes(fixed_uuid_v7(tail).into_bytes()).expect("client id")
}

fn request_id(tail: u8) -> RequestId {
    RequestId::from_bytes(fixed_uuid_v7(tail).into_bytes()).expect("request id")
}

fn operation_id(tail: u8) -> OperationId {
    OperationId::from_bytes(fixed_uuid_v7(tail).into_bytes()).expect("operation id")
}

fn snapshot_id(tail: u8) -> SnapshotId {
    SnapshotId::from_bytes(fixed_uuid_v7(tail).into_bytes()).expect("snapshot id")
}

fn fixture_envelope() -> ConnectEnvelope {
    ConnectEnvelope {
        protocol_major: ProtocolVersion::current().major,
        protocol_minor: ProtocolVersion::current().minor,
        connection_id: fixed_uuid_v7(0x11),
        session_id: fixed_uuid_v7(0x12),
        channel_id: fixed_uuid_v7(0x13),
        channel: ChannelKind::Durable,
        sequence: 7,
        request_id: Some(request_id(0x21)),
        operation_id: Some(operation_id(0x22)),
        limits: ConnectLimits::v1_default(),
        compression: devmanager::connect::Compression::None,
        privacy_class: ConnectPrivacyClass::ManagedMetadata,
        payload_kind: PayloadKind::SNAPSHOT_PAGE,
        payload_version: 1,
        payload: vec![0x91, 0x01, 0x92, 0xa4, b't', b'e', b's', b't'],
    }
}

fn hex_fixture(value: &str) -> Vec<u8> {
    value
        .split_whitespace()
        .map(|byte| u8::from_str_radix(byte, 16).expect("hex fixture byte"))
        .collect()
}

#[test]
fn wire_envelope_fixture_is_deterministic_and_round_trips() {
    let envelope = fixture_envelope();
    let encoded = envelope.encode().expect("encode envelope");
    let expected = hex_fixture(include_str!("fixtures/connect/v1/envelope.msgpack.hex"));

    assert_eq!(
        encoded, expected,
        "v1 encoding must stay byte deterministic"
    );
    assert_eq!(
        ConnectEnvelope::decode(&encoded).expect("decode envelope"),
        envelope
    );
    assert_eq!(encode_inner(&envelope).expect("encode inner"), encoded);
    assert_eq!(decode_inner(&encoded).expect("decode inner"), envelope);
}

#[test]
fn wire_unknown_payload_is_preserved_without_becoming_an_action() {
    let mut envelope = fixture_envelope();
    envelope.payload_kind = PayloadKind::new(0x7fff).expect("unknown nonzero kind");

    assert_eq!(envelope.known_payload_kind(), None);
    assert!(!envelope.is_action_payload());

    let decoded = ConnectEnvelope::decode(&envelope.encode().expect("encode unknown")).unwrap();
    assert_eq!(decoded.payload_kind, envelope.payload_kind);
    assert_eq!(decoded.known_payload_kind(), None);
}

#[derive(Serialize)]
struct UnknownEnvelopeField {
    unknown: u8,
    protocol_major: u16,
    protocol_minor: u16,
    connection_id: Uuid,
    session_id: Uuid,
    channel_id: Uuid,
    channel: ChannelKind,
    sequence: u64,
    request_id: Option<RequestId>,
    operation_id: Option<OperationId>,
    limits: ConnectLimits,
    compression: devmanager::connect::Compression,
    privacy_class: ConnectPrivacyClass,
    payload_kind: PayloadKind,
    payload_version: u16,
    payload: Vec<u8>,
}

#[test]
fn wire_decode_rejects_unknown_fields_and_incompatible_versions() {
    let envelope = fixture_envelope();
    let unknown = UnknownEnvelopeField {
        unknown: 1,
        protocol_major: envelope.protocol_major,
        protocol_minor: envelope.protocol_minor,
        connection_id: envelope.connection_id,
        session_id: envelope.session_id,
        channel_id: envelope.channel_id,
        channel: envelope.channel,
        sequence: envelope.sequence,
        request_id: envelope.request_id,
        operation_id: envelope.operation_id,
        limits: envelope.limits,
        compression: envelope.compression,
        privacy_class: envelope.privacy_class,
        payload_kind: envelope.payload_kind,
        payload_version: envelope.payload_version,
        payload: envelope.payload.clone(),
    };
    let unknown_bytes = rmp_serde::to_vec_named(&unknown).expect("unknown field bytes");
    assert!(ConnectEnvelope::decode(&unknown_bytes).is_err());

    let mut incompatible = envelope;
    incompatible.protocol_major = 2;
    assert!(incompatible.encode().is_err());
}

#[test]
fn negotiated_limits_bound_message_pages_chunks_and_cumulative_transfer() {
    let local = ConnectLimits {
        max_physical_frame_bytes: 8 * 1024,
        max_reassembled_message_bytes: 64 * 1024,
        max_page_items: 100,
        max_page_encoded_bytes: 32 * 1024,
        max_chunk_bytes: 8 * 1024,
        max_cumulative_bytes: 48 * 1024,
    };
    let peer = ConnectLimits {
        max_physical_frame_bytes: 16 * 1024,
        max_reassembled_message_bytes: 32 * 1024,
        max_page_items: 200,
        max_page_encoded_bytes: 16 * 1024,
        max_chunk_bytes: 4 * 1024,
        max_cumulative_bytes: 24 * 1024,
    };
    let negotiated = local.negotiate(peer).expect("negotiate limits");
    assert_eq!(negotiated.max_physical_frame_bytes, 8 * 1024);
    assert_eq!(negotiated.max_reassembled_message_bytes, 32 * 1024);
    assert_eq!(negotiated.max_page_items, 100);
    assert_eq!(negotiated.max_page_encoded_bytes, 16 * 1024);
    assert_eq!(negotiated.max_chunk_bytes, 4 * 1024);
    assert_eq!(negotiated.max_cumulative_bytes, 24 * 1024);

    assert!(negotiated.validate_page(100, 16 * 1024).is_ok());
    assert!(negotiated.validate_page(101, 1).is_err());
    assert!(negotiated.validate_page(1, 16 * 1024 + 1).is_err());
    assert_eq!(negotiated.validate_chunk(0, &[0; 4096]).unwrap(), 4096);
    assert!(negotiated.validate_chunk(4096, &[0; 4096]).is_ok());
    assert!(negotiated.validate_chunk(21 * 1024, &[0; 4096]).is_err());
    assert!(negotiated.validate_chunk(0, &[0; 4097]).is_err());

    let mut oversized = fixture_envelope();
    oversized.limits = negotiated;
    oversized.payload = vec![0; 32 * 1024 + 1];
    assert!(oversized.encode().is_err());
    assert!(ConnectEnvelope::decode_with_limits(
        &fixture_envelope().encode().expect("fixture bytes"),
        negotiated,
    )
    .is_err());
}

#[test]
fn permission_evaluator_is_task_scoped_and_watcher_never_mutates() {
    let task = task_id(0x31);
    let other_task = task_id(0x32);
    let evaluator = PermissionEvaluator::default();
    let context = AuthoritativePermissionContext::live(4, 5, 6).expect("live epochs");

    assert_eq!(
        evaluator.evaluate(PermissionRequest {
            role: ConnectRole::PairedOwner,
            task_id: None,
            action: ActionId::APPROVE_DANGEROUS,
            credential: None,
        }),
        PermissionDecision::Denied(PermissionDenyReason::DeviceCredentialRequired)
    );
    let watcher_read = PermissionRequest {
        role: ConnectRole::Watcher { task_id: task },
        task_id: Some(task),
        action: ActionId::READ_TASK,
        credential: None,
    };
    let watcher_grant = ScopedPermissionGrant::issue(
        ConnectRole::Watcher { task_id: task },
        task,
        ActionId::READ_TASK,
        context,
    )
    .expect("watcher grant");
    assert_eq!(
        evaluator.evaluate_with_scoped_grant(watcher_read, &watcher_grant, context),
        PermissionDecision::Allow
    );
    let watcher_mutate = PermissionRequest {
        role: ConnectRole::Watcher { task_id: task },
        task_id: Some(task),
        action: ActionId::MUTATE_TASK,
        credential: None,
    };
    let watcher_mutate_grant = ScopedPermissionGrant::issue(
        ConnectRole::Watcher { task_id: task },
        task,
        ActionId::MUTATE_TASK,
        context,
    )
    .expect("watcher mutate grant");
    assert!(matches!(
        evaluator.evaluate_with_scoped_grant(watcher_mutate, &watcher_mutate_grant, context),
        PermissionDecision::Denied(PermissionDenyReason::WatcherReadOnly)
    ));
    let collaborator_mutate = PermissionRequest {
        role: ConnectRole::Collaborator { task_id: task },
        task_id: Some(task),
        action: ActionId::MUTATE_TASK,
        credential: None,
    };
    let collaborator_grant = ScopedPermissionGrant::issue(
        ConnectRole::Collaborator { task_id: task },
        task,
        ActionId::MUTATE_TASK,
        context,
    )
    .expect("collaborator grant");
    assert_eq!(
        evaluator.evaluate_with_scoped_grant(collaborator_mutate.clone(), &collaborator_grant, context),
        PermissionDecision::Allow
    );
    let foreign_grant = ScopedPermissionGrant::issue(
        ConnectRole::Collaborator {
            task_id: other_task,
        },
        other_task,
        ActionId::MUTATE_TASK,
        context,
    )
    .expect("foreign grant");
    assert!(matches!(
        evaluator.evaluate_with_scoped_grant(collaborator_mutate, &foreign_grant, context),
        PermissionDecision::Denied(PermissionDenyReason::ScopedGrantRequired)
    ));
    let dangerous = PermissionRequest {
        role: ConnectRole::Collaborator { task_id: task },
        task_id: Some(task),
        action: ActionId::APPROVE_DANGEROUS,
        credential: None,
    };
    let dangerous_grant = ScopedPermissionGrant::issue(
        ConnectRole::Collaborator { task_id: task },
        task,
        ActionId::APPROVE_DANGEROUS,
        context,
    )
    .expect("dangerous grant");
    assert!(matches!(
        evaluator.evaluate_with_scoped_grant(dangerous, &dangerous_grant, context),
        PermissionDecision::Denied(PermissionDenyReason::OwnerOnly)
    ));
    let unknown = PermissionRequest {
        role: ConnectRole::Watcher { task_id: task },
        task_id: Some(task),
        action: ActionId::new(0x7fff).expect("unknown action"),
        credential: None,
    };
    let unknown_grant = ScopedPermissionGrant::issue(
        ConnectRole::Watcher { task_id: task },
        task,
        ActionId::new(0x7fff).expect("unknown action"),
        context,
    )
    .expect("unknown grant");
    assert!(matches!(
        evaluator.evaluate_with_scoped_grant(unknown, &unknown_grant, context),
        PermissionDecision::Denied(PermissionDenyReason::UnknownAction)
    ));
}

#[test]
fn presence_is_ephemeral_last_sender_metadata_only() {
    let task = task_id(0x41);
    let first = client_id(0x42);
    let second = client_id(0x43);
    let mut presence = EphemeralPresence::new(2);

    let turn = TurnEpoch::new(1).expect("turn");
    let focus = FocusEpoch::new(1).expect("focus");
    presence.record(LastSenderHint::new(task, first, 100, turn, focus));
    assert_eq!(presence.last_sender(task).unwrap().client_id, first);
    presence.record(LastSenderHint::new(task, second, 101, turn, focus));
    assert_eq!(presence.last_sender(task).unwrap().client_id, second);
    assert_eq!(presence.last_sender(task).unwrap().observed_at_ms, 101);
    assert_eq!(presence.last_sender(task).unwrap().turn_epoch, turn);
    assert_eq!(presence.last_sender(task).unwrap().focus_epoch, focus);
}

#[test]
fn zero_capacity_presence_retains_nothing() {
    let task = task_id(0x44);
    let client = client_id(0x45);
    let mut presence = EphemeralPresence::new(0);

    assert!(!presence.record(LastSenderHint::new(
        task,
        client,
        100,
        TurnEpoch::new(1).expect("turn"),
        FocusEpoch::new(1).expect("focus"),
    )));
    assert!(presence.is_empty());
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TransportError;

struct MemoryTransport {
    frames: VecDeque<Vec<u8>>,
    closed: bool,
}

impl ConnectTransport for MemoryTransport {
    type Error = TransportError;

    fn send(&mut self, envelope: ConnectEnvelope) -> Result<(), Self::Error> {
        self.frames
            .push_back(encode_inner(&envelope).expect("encode inner envelope"));
        Ok(())
    }

    fn receive(&mut self) -> Result<Option<ConnectEnvelope>, Self::Error> {
        Ok(self
            .frames
            .pop_front()
            .map(|frame| decode_inner(&frame).expect("decode inner")))
    }

    fn close(&mut self) -> Result<(), Self::Error> {
        self.closed = true;
        Ok(())
    }
}

#[test]
fn transport_trait_keeps_direct_and_relay_inner_semantics_identical() {
    let envelope = fixture_envelope();
    let direct_bytes = encode_inner(&envelope).expect("direct inner bytes");
    let relay_bytes = encode_inner(&envelope).expect("relay inner bytes");
    assert_eq!(direct_bytes, relay_bytes);

    let mut transport = MemoryTransport {
        frames: VecDeque::new(),
        closed: false,
    };
    transport.send(envelope.clone()).expect("send");
    assert_eq!(transport.receive().expect("receive"), Some(envelope));
    transport.close().expect("close");
    assert!(transport.closed);
}

struct FixtureProjectionSource;

impl ProjectionSource for FixtureProjectionSource {
    fn snapshot_page(
        &self,
        request: SnapshotRequest,
    ) -> Result<SnapshotPage, devmanager::connect::ProjectionError> {
        Ok(SnapshotPage {
            snapshot_id: request.snapshot_id.unwrap_or_else(|| snapshot_id(0x51)),
            through_sequence: 4,
            section: request.section,
            after_item: None,
            items: Vec::new(),
            encoded_bytes: 0,
            next_cursor: None,
        })
    }

    fn event_page(
        &self,
        request: ReplayRequest,
    ) -> Result<EventPage, devmanager::connect::ProjectionError> {
        Ok(EventPage {
            after_sequence: request.after_sequence,
            through_sequence: request.after_sequence,
            events: Vec::new(),
            next_cursor: None,
        })
    }

    fn query(
        &self,
        request: QueryEnvelope,
    ) -> Result<QueryReply, devmanager::connect::ProjectionError> {
        Ok(QueryReply {
            request_id: request.request_id,
            outcome: devmanager::domain::query::QueryOutcome::Err(
                devmanager::domain::query::QueryError::UnsupportedCapability,
            ),
        })
    }

    fn extensions(&self) -> ProjectionExtensions {
        ProjectionExtensions::default()
    }
}

#[test]
fn projection_source_reuses_bounded_domain_pages_and_optional_extensions() {
    let source = FixtureProjectionSource;
    let page_limits = PageLimits::new(10, 1024).expect("page limits");
    let snapshot = source
        .snapshot_page(SnapshotRequest {
            task_id: Some(task_id(0x61)),
            section: SnapshotSection::Tasks,
            snapshot_id: None,
            resume_cursor: None,
            limits: page_limits,
        })
        .expect("snapshot page");
    assert_eq!(snapshot.items.len(), 0);
    let mut oversized_snapshot = snapshot.clone();
    oversized_snapshot.next_cursor = Some(vec![0; MAX_CONNECT_RESUME_CURSOR_BYTES + 1]);
    assert!(devmanager::connect::validate_snapshot_page(&oversized_snapshot, page_limits).is_err());
    let replay = source
        .event_page(ReplayRequest {
            task_id: Some(task_id(0x61)),
            after_sequence: 4,
            resume_cursor: None,
            limits: page_limits,
        })
        .expect("event page");
    assert_eq!(replay.after_sequence, 4);
    let mut oversized_replay = replay.clone();
    oversized_replay.next_cursor = Some(vec![0; MAX_CONNECT_RESUME_CURSOR_BYTES + 1]);
    assert!(devmanager::connect::validate_event_page(&oversized_replay, page_limits).is_err());
    let reply = source
        .query(QueryEnvelope {
            request_id: request_id(0x62),
            client_id: client_id(0x63),
            task_id: Some(task_id(0x61)),
            query: Query::TaskSnapshot,
        })
        .expect("query reply");
    assert!(matches!(
        reply.outcome,
        devmanager::domain::query::QueryOutcome::Err(
            devmanager::domain::query::QueryError::UnsupportedCapability
        )
    ));
    assert!(source.extensions().prompt.is_none());
    assert!(source.extensions().browser.is_none());
}
