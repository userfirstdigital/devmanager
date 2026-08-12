use connect_crypto::{
    CredentialPurpose, CryptoPrologue, NoiseHandshakeMessage, SealedFrame, SEALED_FRAME_VERSION,
    SEALED_NONCE_BYTES, SEALED_TAG_BYTES,
};

#[test]
fn handshake_message_fixture_preserves_the_native_header() {
    let encoded = [1_u8, 2, 0xaa, 0xbb, 0xcc];
    let message = NoiseHandshakeMessage::decode(&encoded).expect("fixture decodes");

    assert_eq!(message.step(), 2);
    assert_eq!(message.body(), &[0xaa, 0xbb, 0xcc]);
    assert_eq!(message.encode().expect("fixture encoding"), encoded);
}

#[test]
fn prologue_fixture_is_deterministic_and_domain_separated() {
    let prologue = CryptoPrologue::new(1, CredentialPurpose::OwnerPairing, [0x09; 16], [0x08; 16])
        .expect("fixture prologue");

    let mut expected = b"DevManagerConnect/v1\0".to_vec();
    expected.extend_from_slice(&1_u16.to_be_bytes());
    expected.push(b"owner-pairing".len() as u8);
    expected.extend_from_slice(b"owner-pairing");
    expected.extend_from_slice(&[0x09; 16]);
    expected.extend_from_slice(&[0x08; 16]);
    assert_eq!(prologue.canonical_bytes(), expected);

    let task = CryptoPrologue::new(1, CredentialPurpose::TaskInvitation, [0x09; 16], [0x08; 16])
        .expect("task prologue");
    assert_ne!(prologue.canonical_bytes(), task.canonical_bytes());
}

#[test]
fn sealed_frame_fixture_is_big_endian_and_bounded() {
    let frame = SealedFrame::from_parts(
        SEALED_FRAME_VERSION,
        7,
        [0x11; SEALED_NONCE_BYTES],
        vec![0x22, 0x33],
        [0x44; SEALED_TAG_BYTES],
    )
    .expect("fixture frame");
    let encoded = frame.encode().expect("fixture encoding");

    assert_eq!(&encoded[..1], &[SEALED_FRAME_VERSION]);
    assert_eq!(&encoded[1..9], &7_u64.to_be_bytes());
    assert_eq!(&encoded[9..25], &[0x11; SEALED_NONCE_BYTES]);
    assert_eq!(&encoded[25..27], &[0x22, 0x33]);
    assert_eq!(&encoded[27..], &[0x44; SEALED_TAG_BYTES]);
    assert_eq!(
        SealedFrame::decode(&encoded).expect("fixture decode"),
        frame
    );
}
