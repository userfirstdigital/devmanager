use connect_crypto::{
    CredentialPurpose, CryptoPrologue, NoiseHandshakeMessage, SealedFrame,
    NOISE_FIRST_PAIRING_PATTERN, NOISE_PINNED_DEVICE_PATTERN, PROTOCOL_MAJOR, SEALED_FRAME_VERSION,
    SEALED_NONCE_BYTES, SEALED_TAG_BYTES,
};

#[test]
fn browser_runtime_golden_fixture_matches_native_identity() {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/connect/v1/crypto-runtime.json"
    ))
    .expect("valid Connect browser runtime fixture");

    assert_eq!(fixture["schemaVersion"], 1);
    assert_eq!(fixture["protocolMajor"], PROTOCOL_MAJOR);
    assert_eq!(fixture["firstPairingPattern"], NOISE_FIRST_PAIRING_PATTERN);
    assert_eq!(fixture["pinnedDevicePattern"], NOISE_PINNED_DEVICE_PATTERN);
    assert_eq!(fixture["modulePath"], "./wasm/connect_crypto.js");
    assert_eq!(
        fixture["requiredExports"],
        serde_json::json!([
            "WasmConnectHandshake",
            "connect_protocol_major",
            "connect_noise_pattern",
            "encode_connect_envelope_json",
            "decode_connect_envelope_json",
            "encode_connect_payload_json",
            "decode_connect_payload_json"
        ])
    );
}

#[cfg(feature = "wasm")]
mod wire_contract {
    use base64::Engine;
    use connect_crypto::wire::{decode_connect_envelope_json, encode_connect_envelope_json};
    use serde_json::{json, Value};
    use uuid::Uuid;

    fn envelope(payload_kind: u16, channel: &str, privacy_class: &str) -> Value {
        let identifier = || Uuid::now_v7().to_string();
        json!({
            "protocolMajor": 1,
            "protocolMinor": 0,
            "connectionId": identifier(),
            "sessionId": identifier(),
            "channelId": identifier(),
            "channel": channel,
            "sequence": 1,
            "requestId": Value::Null,
            "operationId": Value::Null,
            "limits": {
                "max_physical_frame_bytes": 1024 * 1024,
                "max_reassembled_message_bytes": 16 * 1024 * 1024,
                "max_page_items": 1000,
                "max_page_encoded_bytes": 512 * 1024,
                "max_chunk_bytes": 256 * 1024,
                "max_cumulative_bytes": 16 * 1024 * 1024_u64,
            },
            "compression": "none",
            "privacyClass": privacy_class,
            "payloadKind": payload_kind,
            "payloadVersion": 1,
            "payloadBase64": base64::engine::general_purpose::STANDARD.encode([0x01, 0x02]),
        })
    }

    fn encode(value: &Value) -> Result<Vec<u8>, wasm_bindgen::JsValue> {
        encode_connect_envelope_json(serde_json::to_string(value).expect("JSON fixture"))
    }

    #[test]
    fn valid_native_envelope_round_trips_through_wasm_abi() {
        let input = envelope(3, "durable", "managed_metadata");
        let encoded = encode(&input).expect("valid envelope");
        let decoded = decode_connect_envelope_json(&encoded).expect("valid MessagePack");
        let output: Value = serde_json::from_str(&decoded).expect("JSON output");

        assert_eq!(output["protocolMajor"], 1);
        assert_eq!(output["channel"], "durable");
        assert_eq!(output["payloadKind"], 3);
        assert_eq!(output["payloadBase64"], input["payloadBase64"]);
    }

    #[test]
    fn page_limits_are_bounded_by_the_native_contract() {
        let mut too_many_items = envelope(3, "durable", "managed_metadata");
        too_many_items["limits"]["max_page_items"] = json!(1001);
        assert!(encode(&too_many_items).is_err());

        let mut too_many_bytes = envelope(3, "durable", "managed_metadata");
        too_many_bytes["limits"]["max_page_encoded_bytes"] = json!(512 * 1024 + 1);
        assert!(encode(&too_many_bytes).is_err());
    }

    #[test]
    fn channel_and_raw_content_rules_match_native_payload_catalog() {
        let mismatched_channel = envelope(3, "critical", "managed_metadata");
        assert!(encode(&mismatched_channel).is_err());

        let raw_page = envelope(3, "durable", "raw_content");
        assert!(encode(&raw_page).is_err());

        let raw_terminal = envelope(10, "ephemeral", "raw_content");
        assert!(encode(&raw_terminal).is_ok());

        let raw_unknown = envelope(99, "durable", "raw_content");
        assert!(encode(&raw_unknown).is_err());
    }
}

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
