use uuid::Uuid;

use devmanager::connect::{
    connect_prologue, lock_noise_pattern, preferred_connect_route, ChannelBinding, ChannelId,
    ConnectChannelKey, ConnectEnvelope, ConnectLimits, ConnectPayload, ConnectPrivacyClass,
    ConnectionId, EndToEndChannel, SessionId, CONNECT_CRYPTO_PRODUCTION_READY,
    CONNECT_NOISE_FIRST_PAIRING_PATTERN, CONNECT_NOISE_PINNED_DEVICE_PATTERN,
};
use devmanager::domain::id::{ClientId, RequestId, TaskId};
use devmanager::domain::query::{Query, QueryEnvelope};
use devmanager::protocol::ClientRequest;
use devmanager::protocol::{
    instantiate_noise_channel, ChannelRole, CredentialPurpose, CryptoError, CryptoHoldReason,
    ReplayWindow, SealedFrame, SourceLevelSealer, MAX_SEALED_PLAINTEXT_BYTES, MAX_SESSION_AGE_SECS,
    NOISE_FIRST_PAIRING_PATTERN, REPLAY_WINDOW_SIZE, SEALED_NONCE_BYTES,
};

const FIXTURE_SECRET: &str = "SEALED-FIXTURE-SECRET";

fn uuid_v7(tail: u8) -> Uuid {
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

impl FromBytes for TaskId {
    type Error = devmanager::domain::id::IdError;
    fn from_bytes(bytes: [u8; 16]) -> Result<Self, Self::Error> {
        TaskId::from_bytes(bytes)
    }
}

impl FromBytes for ClientId {
    type Error = devmanager::domain::id::IdError;
    fn from_bytes(bytes: [u8; 16]) -> Result<Self, Self::Error> {
        ClientId::from_bytes(bytes)
    }
}

impl FromBytes for RequestId {
    type Error = devmanager::domain::id::IdError;
    fn from_bytes(bytes: [u8; 16]) -> Result<Self, Self::Error> {
        RequestId::from_bytes(bytes)
    }
}

fn fixture_key() -> ConnectChannelKey {
    ConnectChannelKey::from_bytes([0x11; 32])
}

fn fixture_prologue(purpose: CredentialPurpose) -> devmanager::protocol::CryptoPrologue {
    connect_prologue(
        purpose,
        uuid_v7(0x31).into_bytes(),
        uuid_v7(0x32).into_bytes(),
    )
    .expect("prologue")
}

fn fixture_envelope() -> ConnectEnvelope {
    let request_id = domain_id::<RequestId>(0x21);
    let payload = ConnectPayload::Request(ClientRequest::Query(QueryEnvelope {
        request_id,
        client_id: domain_id::<ClientId>(0x22),
        task_id: Some(domain_id::<TaskId>(0x23)),
        query: Query::TaskSnapshot,
    }));
    ConnectEnvelope::new(
        ChannelBinding::new(
            ConnectionId::from_uuid(uuid_v7(0x11)).expect("connection id"),
            SessionId::from_uuid(uuid_v7(0x32)).expect("session id"),
            ChannelId::from_uuid(uuid_v7(0x13)).expect("channel id"),
        ),
        7,
        Some(request_id),
        None,
        ConnectLimits::v1_default(),
        ConnectPrivacyClass::RawContent,
        payload,
    )
    .expect("typed envelope")
}

fn nonce(fill: u8) -> [u8; SEALED_NONCE_BYTES] {
    [fill; SEALED_NONCE_BYTES]
}

#[test]
fn owner_and_invitation_transcripts_cannot_substitute() {
    let owner = fixture_prologue(CredentialPurpose::OwnerPairing);
    let invite = fixture_prologue(CredentialPurpose::TaskInvitation);
    assert_ne!(owner.canonical_bytes(), invite.canonical_bytes());
    assert_ne!(
        owner.purpose().transcript_label(),
        invite.purpose().transcript_label()
    );

    let sealer = SourceLevelSealer::derive(&fixture_key(), owner, ChannelRole::Initiator);
    let frame = sealer
        .seal(1, nonce(0x22), FIXTURE_SECRET.as_bytes())
        .expect("seal");
    let other = SourceLevelSealer::derive(&fixture_key(), invite, ChannelRole::Responder);
    assert_eq!(other.open(&frame), Err(CryptoError::Authenticity));
}

#[test]
fn prologue_binds_protocol_major_route_session_and_purpose() {
    let prologue = fixture_prologue(CredentialPurpose::OwnerPairing);
    assert_eq!(
        prologue.canonical_bytes(),
        hex_fixture(include_str!("fixtures/connect/v1/crypto-prologue.hex"))
    );
    assert_eq!(
        connect_prologue(
            CredentialPurpose::OwnerPairing,
            uuid_v7(0x31).into_bytes(),
            uuid_v7(0x32).into_bytes(),
        )
        .expect("same prologue")
        .canonical_bytes(),
        prologue.canonical_bytes()
    );
    assert_eq!(
        connect_prologue(
            CredentialPurpose::OwnerPairing,
            uuid_v7(0x99).into_bytes(),
            uuid_v7(0x32).into_bytes(),
        )
        .expect("route change")
        .canonical_bytes()
            == prologue.canonical_bytes(),
        false
    );
}

#[test]
fn sealed_frame_matches_committed_vector_and_redacts_ciphertext() {
    let sealer = SourceLevelSealer::derive(
        &fixture_key(),
        fixture_prologue(CredentialPurpose::OwnerPairing),
        ChannelRole::Initiator,
    );
    let frame = sealer
        .seal(1, nonce(0x22), FIXTURE_SECRET.as_bytes())
        .expect("seal");
    let encoded = frame.encode().expect("encode");
    assert_eq!(
        encoded,
        hex_fixture(include_str!("fixtures/connect/v1/crypto-sealed-frame.hex"))
    );
    assert_eq!(SealedFrame::decode(&encoded).expect("decode"), frame);
    let rendered = format!("{frame:?}");
    assert!(!rendered.contains(FIXTURE_SECRET));
    assert!(!rendered.contains("2222222222222222"));
    assert!(rendered.contains("ciphertext_len"));
}

#[test]
fn paired_channel_round_trips_raw_content_and_prefers_direct() {
    assert_eq!(
        preferred_connect_route(true),
        devmanager::connect::ConnectRoute::Direct
    );
    assert_eq!(
        preferred_connect_route(false),
        devmanager::connect::ConnectRoute::Relay
    );

    let (mut initiator, mut responder) = EndToEndChannel::pair_source_level(
        fixture_key(),
        fixture_prologue(CredentialPurpose::OwnerPairing),
        true,
        10,
    )
    .expect("pair");
    assert_eq!(
        initiator.preferred_route(),
        devmanager::connect::ConnectRoute::Direct
    );
    let envelope = fixture_envelope();
    initiator
        .bind_session(envelope.binding())
        .expect("session binding");
    let frame = initiator.seal(&envelope, nonce(0x22), 11).expect("seal");
    let opened = responder.open(&frame, 12).expect("open");
    assert_eq!(opened, envelope);
    assert!(!format!("{frame:?}").contains("TaskSnapshot"));
}

#[test]
fn tamper_replay_reorder_and_wrong_key_fail_closed() {
    let (mut initiator, mut responder) = EndToEndChannel::pair_source_level(
        fixture_key(),
        fixture_prologue(CredentialPurpose::OwnerPairing),
        false,
        20,
    )
    .expect("pair");
    let first = initiator
        .seal_bytes(FIXTURE_SECRET.as_bytes(), nonce(0x22), 21)
        .expect("first");
    let second = initiator
        .seal_bytes(b"second", nonce(0x23), 22)
        .expect("second");

    let mut tampered = first.encode().expect("encode");
    let last = tampered.len() - 1;
    tampered[last] ^= 0x01;
    let tampered = SealedFrame::decode(&tampered).expect("decode tampered");
    assert_eq!(
        responder.open_bytes(&tampered, 23),
        Err(CryptoError::Authenticity)
    );

    let recovered = responder.open_bytes(&first, 24).expect("first accepted");
    assert_eq!(recovered, FIXTURE_SECRET.as_bytes());
    assert_eq!(
        responder.open_bytes(&first, 25),
        Err(CryptoError::Replay { sequence: 1 })
    );

    let mut late = ReplayWindow::new();
    late.accept(REPLAY_WINDOW_SIZE + 2).expect("high");
    assert_eq!(
        late.accept(1),
        Err(CryptoError::ReplayTooOld { sequence: 1 })
    );

    assert_eq!(
        responder.open_bytes(&second, 26).expect("reorder"),
        b"second"
    );

    let mut stranger = EndToEndChannel::open_source_level(
        ConnectChannelKey::from_bytes([0x99; 32]),
        fixture_prologue(CredentialPurpose::OwnerPairing),
        ChannelRole::Responder,
        false,
        27,
        false,
    )
    .expect("wrong key");
    assert_eq!(
        stranger.open_bytes(&first, 28),
        Err(CryptoError::Authenticity)
    );
}

#[test]
fn revoked_key_oversized_frame_session_age_and_sequence_exhaustion_fail_closed() {
    assert_eq!(
        EndToEndChannel::open_source_level(
            fixture_key(),
            fixture_prologue(CredentialPurpose::OwnerPairing),
            ChannelRole::Initiator,
            true,
            1,
            true,
        )
        .err(),
        Some(CryptoError::RevokedKey)
    );

    let sealer = SourceLevelSealer::derive(
        &fixture_key(),
        fixture_prologue(CredentialPurpose::OwnerPairing),
        ChannelRole::Initiator,
    );
    let oversized = vec![0_u8; MAX_SEALED_PLAINTEXT_BYTES as usize + 1];
    assert_eq!(
        sealer.seal(1, nonce(0x22), &oversized),
        Err(CryptoError::PlaintextExceeded {
            declared: u64::from(MAX_SEALED_PLAINTEXT_BYTES) + 1
        })
    );

    let (mut initiator, _) = EndToEndChannel::pair_source_level(
        fixture_key(),
        fixture_prologue(CredentialPurpose::OwnerPairing),
        true,
        1,
    )
    .expect("pair");
    assert_eq!(
        initiator.seal_bytes(b"late", nonce(0x22), 1 + MAX_SESSION_AGE_SECS),
        Err(CryptoError::SessionExpired)
    );

    let mut exhausted = initiator.with_send_cursor(u64::MAX);
    assert_eq!(
        exhausted.seal_bytes(b"one-more", nonce(0x22), 2),
        Err(CryptoError::SequenceExhausted)
    );
}

#[test]
fn production_noise_instantiation_is_explicit_hold() {
    assert!(!CONNECT_CRYPTO_PRODUCTION_READY);
    lock_noise_pattern(NOISE_FIRST_PAIRING_PATTERN, true).expect("locked XX");
    lock_noise_pattern(CONNECT_NOISE_PINNED_DEVICE_PATTERN, false).expect("locked IK");
    assert_eq!(
        lock_noise_pattern("Noise_NN_25519_ChaChaPoly_BLAKE2s", true),
        Err(CryptoError::AlgorithmDowngrade)
    );
    let hold = instantiate_noise_channel(CONNECT_NOISE_FIRST_PAIRING_PATTERN, true)
        .expect_err("noise hold");
    assert_eq!(hold.reason, CryptoHoldReason::ProductionReviewRequired);
    let hold = EndToEndChannel::open_noise(CONNECT_NOISE_FIRST_PAIRING_PATTERN, true)
        .expect_err("channel hold");
    assert_eq!(hold.reason, CryptoHoldReason::ProductionReviewRequired);
    assert!(format!("{hold}").contains("HOLD"));
}
