//! Focused Connect envelope privacy fail-closed proofs.

use devmanager::connect::{
    ChannelBinding, ChannelId, ChannelKind, Compression, ConnectEnvelope, ConnectLimits,
    ConnectPayload, ConnectPrivacyClass, ConnectionId, EnvelopeError, HostOutputPayload,
    PayloadKind, SessionId, CONNECT_PROTOCOL_MAJOR, CONNECT_PROTOCOL_MINOR,
};
use devmanager::domain::{DomainEvent, Event, EventId, ResourceId, SubscriptionId, TaskId};
use devmanager::protocol::{
    Capability, CapabilitySet, ServerMessage, StreamFrame, StreamKey, StreamPayloadKind,
};
use uuid::Uuid;

fn uuid(tail: u8) -> Uuid {
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

fn binding() -> ChannelBinding {
    let connection_id = ConnectionId::from_uuid(uuid(1)).unwrap();
    let session_id = SessionId::from_uuid(uuid(2)).unwrap();
    let channel_id = ChannelId::from_uuid(uuid(3)).unwrap();
    ChannelBinding::try_from_uuids(
        connection_id.as_uuid(),
        session_id.as_uuid(),
        channel_id.as_uuid(),
    )
    .unwrap()
}

#[test]
fn actual_wasm_payload_fixtures_match_native_serializer() {
    use base64::Engine;
    let expected: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/connect/v1/native-payloads.json"))
            .expect("native WASM fixtures");
    let actual = devmanager::connect::native_browser_contract_fixtures()
        .into_iter()
        .filter(|fixture| matches!(fixture.payload.kind().get(), 1 | 18 | 19 | 20 | 21 | 22))
        .map(|fixture| {
            serde_json::json!({
                "name": fixture.name,
                "payloadKind": fixture.payload.kind().get(),
                "channel": fixture.payload.channel(),
                "payloadBase64": base64::engine::general_purpose::STANDARD.encode(
                    fixture.payload.encode(ConnectLimits::v1_default()).expect("native payload")
                ),
            })
        })
        .collect::<Vec<_>>();
    assert_eq!(serde_json::json!(actual), expected,
        "regenerate with cargo run --example connect-wire-fixtures and rerun actual-WASM browser tests");
}

#[test]
fn unknown_extension_cannot_be_labeled_raw_content() {
    let binding = binding();
    let mut envelope = ConnectEnvelope {
        protocol_major: CONNECT_PROTOCOL_MAJOR,
        protocol_minor: CONNECT_PROTOCOL_MINOR,
        connection_id: binding.connection_id.as_uuid(),
        session_id: binding.session_id.as_uuid(),
        channel_id: binding.channel_id.as_uuid(),
        channel: ChannelKind::Durable,
        sequence: 1,
        request_id: None,
        operation_id: None,
        limits: ConnectLimits::v1_default(),
        compression: Compression::None,
        privacy_class: ConnectPrivacyClass::RawContent,
        payload_kind: PayloadKind::new(0x0ABC).unwrap(),
        payload_version: 1,
        payload: vec![1, 2, 3, 4],
    };
    assert!(matches!(
        envelope.validate(),
        Err(EnvelopeError::PrivacyViolation)
    ));
    envelope.privacy_class = ConnectPrivacyClass::ManagedMetadata;
    assert!(envelope.validate().is_ok());
}

#[test]
fn host_outputs_reject_managed_metadata_and_allow_stream_raw_content() {
    let durable = ConnectPayload::HostDurableOutput(
        HostOutputPayload::new(
            CapabilitySet::from_capabilities([Capability::EventReplay]),
            ServerMessage::DurableEvent {
                subscription_id: SubscriptionId::from_bytes(uuid(0x71).into_bytes()).unwrap(),
                event: DomainEvent {
                    id: EventId::from_bytes(uuid(0x55).into_bytes()).unwrap(),
                    task_id: Some(TaskId::from_bytes(uuid(0x43).into_bytes()).unwrap()),
                    sequence: 1,
                    task_revision: Some(1),
                    occurred_at_ms: 1,
                    payload: Event::TaskReopened,
                },
            },
        )
        .unwrap(),
    );
    assert!(matches!(
        ConnectEnvelope::new(
            binding(),
            ChannelKind::Durable,
            1,
            None,
            None,
            ConnectLimits::v1_default(),
            ConnectPrivacyClass::ManagedMetadata,
            durable.clone(),
        ),
        Err(EnvelopeError::PrivacyViolation)
    ));
    assert!(ConnectEnvelope::new(
        binding(),
        ChannelKind::Durable,
        1,
        None,
        None,
        ConnectLimits::v1_default(),
        ConnectPrivacyClass::LocalOnly,
        durable,
    )
    .is_ok());

    let stream = ConnectPayload::HostStreamOutput(
        HostOutputPayload::new(
            CapabilitySet::from_capabilities([Capability::BrowserProjection]),
            ServerMessage::Stream(StreamFrame {
                subscription_id: SubscriptionId::from_bytes(uuid(0x73).into_bytes()).unwrap(),
                stream: StreamKey::from_resource_id(
                    ResourceId::from_bytes(uuid(0x74).into_bytes()).unwrap(),
                ),
                generation: 1,
                sequence: 1,
                payload_kind: StreamPayloadKind::BROWSER_FRAME,
                schema_version: 1,
                payload: vec![0x01],
            }),
        )
        .unwrap(),
    );
    assert!(ConnectEnvelope::new(
        binding(),
        ChannelKind::Ephemeral,
        1,
        None,
        None,
        ConnectLimits::v1_default(),
        ConnectPrivacyClass::RawContent,
        stream.clone(),
    )
    .is_ok());
    assert!(matches!(
        ConnectEnvelope::new(
            binding(),
            ChannelKind::Ephemeral,
            1,
            None,
            None,
            ConnectLimits::v1_default(),
            ConnectPrivacyClass::ManagedMetadata,
            stream,
        ),
        Err(EnvelopeError::PrivacyViolation)
    ));

    let dirty = ConnectPayload::HostConversationOutput(
        HostOutputPayload::new(
            CapabilitySet::from_capabilities([
                Capability::TaskCockpit,
                Capability::SemanticConversation,
            ]),
            ServerMessage::ConversationDirty {
                subscription_id: SubscriptionId::from_bytes(uuid(0x75).into_bytes()).unwrap(),
                task_id: TaskId::from_bytes(uuid(0x76).into_bytes()).unwrap(),
                high_water: 4,
            },
        )
        .unwrap(),
    );
    assert!(ConnectEnvelope::new(
        binding(),
        ChannelKind::Ephemeral,
        1,
        None,
        None,
        ConnectLimits::v1_default(),
        ConnectPrivacyClass::LocalOnly,
        dirty.clone(),
    )
    .is_ok());
    assert!(matches!(
        ConnectEnvelope::new(
            binding(),
            ChannelKind::Ephemeral,
            1,
            None,
            None,
            ConnectLimits::v1_default(),
            ConnectPrivacyClass::RawContent,
            dirty,
        ),
        Err(EnvelopeError::PrivacyViolation)
    ));
}
