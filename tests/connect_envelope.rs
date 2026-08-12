//! Focused Connect envelope privacy fail-closed proofs.

use devmanager::connect::{
    ChannelBinding, ChannelId, ChannelKind, Compression, ConnectEnvelope, ConnectLimits,
    ConnectPrivacyClass, ConnectionId, EnvelopeError, PayloadKind, SessionId,
    CONNECT_PROTOCOL_MAJOR, CONNECT_PROTOCOL_MINOR,
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

#[test]
fn unknown_extension_cannot_be_labeled_raw_content() {
    let connection_id = ConnectionId::from_uuid(uuid(1)).unwrap();
    let session_id = SessionId::from_uuid(uuid(2)).unwrap();
    let channel_id = ChannelId::from_uuid(uuid(3)).unwrap();
    let binding = ChannelBinding::try_from_uuids(
        connection_id.as_uuid(),
        session_id.as_uuid(),
        channel_id.as_uuid(),
    )
    .unwrap();
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
