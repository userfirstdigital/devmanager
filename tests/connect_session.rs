use std::collections::VecDeque;

use uuid::Uuid;

use devmanager::connect::{
    ActionId, ChannelBinding, ChannelId, ConnectEnvelope, ConnectLimits, ConnectPayload,
    ConnectPrivacyClass, ConnectRole, ConnectTransport, ConnectionId, EphemeralPresence,
    LastSenderHint, PermissionDecision, PermissionDenyReason, PermissionEvaluator,
    PermissionRequest, PresenceSink, ProjectionExtensions, ProjectionSource, ReplayRequest,
    SessionId, SnapshotRequest, MAX_CONNECT_RESUME_CURSOR_BYTES,
};
use devmanager::domain::id::{ClientId, RequestId, SnapshotId, TaskId};
use devmanager::domain::query::{Query, QueryEnvelope, QueryReply};
use devmanager::domain::snapshot::{EventPage, PageLimits, SnapshotPage, SnapshotSection};
use devmanager::protocol::ClientRequest;

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

fn snapshot_id(tail: u8) -> SnapshotId {
    SnapshotId::from_bytes(fixed_uuid_v7(tail).into_bytes()).expect("snapshot id")
}

fn fixture_envelope() -> ConnectEnvelope {
    let request_id = request_id(0x21);
    let payload = ConnectPayload::Request(ClientRequest::Query(QueryEnvelope {
        request_id,
        client_id: client_id(0x22),
        task_id: Some(task_id(0x23)),
        query: Query::TaskSnapshot,
    }));
    ConnectEnvelope::new(
        ChannelBinding::new(
            ConnectionId::from_uuid(fixed_uuid_v7(0x11)).expect("connection id"),
            SessionId::from_uuid(fixed_uuid_v7(0x12)).expect("session id"),
            ChannelId::from_uuid(fixed_uuid_v7(0x13)).expect("channel id"),
        ),
        7,
        Some(request_id),
        None,
        ConnectLimits::v1_default(),
        ConnectPrivacyClass::ManagedMetadata,
        payload,
    )
    .expect("typed envelope")
}

#[test]
fn typed_inner_envelope_is_deterministic_and_round_trips() {
    let envelope = fixture_envelope();
    let encoded = envelope.encode().expect("encode inner");
    assert!(!encoded.is_empty());
    assert_eq!(encoded, envelope.encode().expect("encode again"));
    assert_eq!(
        ConnectEnvelope::decode(&encoded).expect("decode inner"),
        envelope
    );
}

#[test]
fn unknown_payload_remains_inert_without_mutable_envelope_discriminants() {
    let kind = devmanager::connect::PayloadKind::new(0x7fff).expect("unknown nonzero kind");
    let payload = ConnectPayload::Unknown(
        devmanager::connect::UnknownPayload::new(kind, 9, vec![1, 2, 3]).expect("unknown payload"),
    );
    let envelope = ConnectEnvelope::new(
        ChannelBinding::new(
            ConnectionId::from_uuid(fixed_uuid_v7(0x31)).expect("connection id"),
            SessionId::from_uuid(fixed_uuid_v7(0x32)).expect("session id"),
            ChannelId::from_uuid(fixed_uuid_v7(0x33)).expect("channel id"),
        ),
        1,
        None,
        None,
        ConnectLimits::v1_default(),
        ConnectPrivacyClass::ManagedMetadata,
        payload,
    )
    .expect("unknown envelope");

    assert_eq!(envelope.known_payload_kind(), None);
    assert!(!envelope.is_action_payload());
    assert_eq!(
        ConnectEnvelope::decode(&envelope.encode().unwrap()).unwrap(),
        envelope
    );
}

#[test]
fn negotiated_limits_bound_pages_chunks_and_cumulative_transfer() {
    let local = ConnectLimits::try_new(
        8 * 1024,
        64 * 1024,
        100,
        32 * 1024,
        8 * 1024,
        48 * 1024,
        1024,
    )
    .expect("local limits");
    let peer = ConnectLimits::try_new(
        16 * 1024,
        32 * 1024,
        200,
        16 * 1024,
        4 * 1024,
        24 * 1024,
        2048,
    )
    .expect("peer limits");
    let negotiated = local.negotiate(peer).expect("negotiate limits");
    assert_eq!(negotiated.max_physical_frame_bytes, 8 * 1024);
    assert_eq!(negotiated.max_reassembled_message_bytes, 32 * 1024);
    assert_eq!(negotiated.max_page_items, 100);
    assert_eq!(negotiated.max_page_encoded_bytes, 16 * 1024);
    assert_eq!(negotiated.max_chunk_bytes, 4 * 1024);
    assert_eq!(negotiated.max_cumulative_bytes, 24 * 1024);
    assert_eq!(negotiated.max_cursor_bytes, 1024);

    assert!(negotiated.validate_page(100, 16 * 1024).is_ok());
    assert!(negotiated.validate_page(101, 1).is_err());
    assert!(negotiated.validate_page(1, 16 * 1024 + 1).is_err());
    assert_eq!(negotiated.validate_chunk(0, &[0; 4096]).unwrap(), 4096);
    assert!(negotiated.validate_chunk(4096, &[0; 4096]).is_ok());
    assert!(negotiated.validate_chunk(21 * 1024, &[0; 4096]).is_err());
    assert!(negotiated.validate_chunk(0, &[0; 4097]).is_err());
}

#[test]
fn permission_evaluator_is_task_scoped_and_watcher_never_mutates() {
    let task = task_id(0x31);
    let other_task = task_id(0x32);
    let evaluator = PermissionEvaluator::default();

    assert_eq!(
        evaluator.evaluate(PermissionRequest {
            role: ConnectRole::PairedOwner,
            task_id: None,
            action: ActionId::APPROVE_DANGEROUS,
            credential: None,
        }),
        PermissionDecision::Denied(PermissionDenyReason::DeviceCredentialRequired)
    );
    assert_eq!(
        evaluator.evaluate(PermissionRequest {
            role: ConnectRole::Watcher { task_id: task },
            task_id: Some(task),
            action: ActionId::READ_TASK,
            credential: None,
        }),
        PermissionDecision::Allow
    );
    assert!(matches!(
        evaluator.evaluate(PermissionRequest {
            role: ConnectRole::Watcher { task_id: task },
            task_id: Some(task),
            action: ActionId::MUTATE_TASK,
            credential: None,
        }),
        PermissionDecision::Denied(PermissionDenyReason::WatcherReadOnly)
    ));
    assert_eq!(
        evaluator.evaluate(PermissionRequest {
            role: ConnectRole::Collaborator { task_id: task },
            task_id: Some(task),
            action: ActionId::MUTATE_TASK,
            credential: None,
        }),
        PermissionDecision::Allow
    );
    assert!(matches!(
        evaluator.evaluate(PermissionRequest {
            role: ConnectRole::Collaborator {
                task_id: other_task
            },
            task_id: Some(task),
            action: ActionId::MUTATE_TASK,
            credential: None,
        }),
        PermissionDecision::Denied(PermissionDenyReason::TaskScopeMismatch)
    ));
    assert!(matches!(
        evaluator.evaluate(PermissionRequest {
            role: ConnectRole::Collaborator { task_id: task },
            task_id: Some(task),
            action: ActionId::APPROVE_DANGEROUS,
            credential: None,
        }),
        PermissionDecision::Denied(PermissionDenyReason::OwnerOnly)
    ));
    assert!(matches!(
        evaluator.evaluate(PermissionRequest {
            role: ConnectRole::Watcher { task_id: task },
            task_id: Some(task),
            action: ActionId::new(0x7fff).expect("unknown action"),
            credential: None,
        }),
        PermissionDecision::Denied(PermissionDenyReason::UnknownAction)
    ));
}

#[test]
fn presence_is_ephemeral_last_sender_metadata_only() {
    let task = task_id(0x41);
    let first = client_id(0x42);
    let second = client_id(0x43);
    let mut presence = EphemeralPresence::new(2);

    presence.record(LastSenderHint::new(task, first, 100));
    assert_eq!(presence.last_sender(task).unwrap().client_id, first);
    presence.record(LastSenderHint::new(task, second, 101));
    assert_eq!(presence.last_sender(task).unwrap().client_id, second);
    assert_eq!(presence.last_sender(task).unwrap().observed_at_ms, 101);
}

#[test]
fn zero_capacity_presence_retains_nothing() {
    let task = task_id(0x44);
    let client = client_id(0x45);
    let mut presence = EphemeralPresence::new(0);

    assert!(!presence.record(LastSenderHint::new(task, client, 100)));
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
            .push_back(envelope.encode().expect("encode inner envelope"));
        Ok(())
    }

    fn receive(&mut self) -> Result<Option<ConnectEnvelope>, Self::Error> {
        Ok(self
            .frames
            .pop_front()
            .map(|frame| ConnectEnvelope::decode(&frame).expect("decode inner")))
    }

    fn close(&mut self) -> Result<(), Self::Error> {
        self.closed = true;
        Ok(())
    }
}

#[test]
fn transport_trait_keeps_inner_semantics_identical() {
    let envelope = fixture_envelope();
    let direct_bytes = envelope.encode().expect("direct inner bytes");
    let relay_bytes = envelope.encode().expect("relay inner bytes");
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
    assert!(ConnectPayload::SnapshotPage(oversized_snapshot)
        .validate(ConnectLimits::v1_default())
        .is_err());

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
    assert!(ConnectPayload::EventPage(oversized_replay)
        .validate(ConnectLimits::v1_default())
        .is_err());

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
