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
    use connect_crypto::wire::{
        decode_connect_envelope_json, decode_connect_payload_json, encode_connect_envelope_json,
        encode_connect_payload_msgpack,
    };
    use serde::de::{self, Deserializer, SeqAccess, Visitor};
    use serde::ser::Serializer;
    use serde::{Deserialize, Serialize};
    use serde_json::{json, Value};
    use std::fmt;
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

    fn decode_payload(bytes: &[u8]) -> Result<Value, ()> {
        let json = decode_connect_payload_json(bytes).map_err(|_| ())?;
        serde_json::from_str(&json).map_err(|_| ())
    }

    fn bytes_as_json_array(bytes: &[u8]) -> Value {
        Value::Array(
            bytes
                .iter()
                .copied()
                .map(|byte| Value::Number(byte.into()))
                .collect(),
        )
    }

    fn serialize_payload_bytes<S>(value: &[u8], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_bytes(value)
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

    #[test]
    fn host_output_lanes_19_20_21_match_native_catalog() {
        assert!(encode(&envelope(19, "durable", "local_only")).is_ok());
        assert!(encode(&envelope(19, "critical", "local_only")).is_err());
        assert!(encode(&envelope(19, "durable", "raw_content")).is_err());
        assert!(encode(&envelope(19, "durable", "managed_metadata")).is_err());

        assert!(encode(&envelope(20, "critical", "local_only")).is_ok());
        assert!(encode(&envelope(20, "durable", "local_only")).is_err());
        assert!(encode(&envelope(20, "critical", "raw_content")).is_err());
        assert!(encode(&envelope(20, "critical", "managed_metadata")).is_err());

        assert!(encode(&envelope(21, "ephemeral", "local_only")).is_ok());
        assert!(encode(&envelope(21, "ephemeral", "raw_content")).is_ok());
        assert!(encode(&envelope(21, "durable", "local_only")).is_err());
        assert!(encode(&envelope(21, "ephemeral", "managed_metadata")).is_err());
    }

    /// Build MessagePack that matches the native envelope shape but skips the
    /// JSON encoder's validate path, so decode must still reject ManagedMetadata.
    fn bypass_encoder_msgpack(payload_kind: u16, channel: &str, privacy_class: &str) -> Vec<u8> {
        use serde::ser::Serializer;
        use serde::Serialize;

        #[derive(Serialize)]
        struct Limits {
            max_physical_frame_bytes: u32,
            max_reassembled_message_bytes: u32,
            max_page_items: u32,
            max_page_encoded_bytes: u32,
            max_chunk_bytes: u32,
            max_cumulative_bytes: u64,
        }

        #[derive(Serialize)]
        struct BypassEnvelope<'a> {
            protocol_major: u16,
            protocol_minor: u16,
            connection_id: Uuid,
            session_id: Uuid,
            channel_id: Uuid,
            channel: &'a str,
            sequence: u64,
            request_id: Option<Uuid>,
            operation_id: Option<Uuid>,
            limits: Limits,
            compression: &'a str,
            privacy_class: &'a str,
            payload_kind: u16,
            payload_version: u16,
            #[serde(serialize_with = "serialize_payload_bytes")]
            payload: &'a [u8],
        }

        fn serialize_payload_bytes<S>(value: &&[u8], serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            serializer.serialize_bytes(*value)
        }

        rmp_serde::to_vec_named(&BypassEnvelope {
            protocol_major: 1,
            protocol_minor: 0,
            connection_id: Uuid::now_v7(),
            session_id: Uuid::now_v7(),
            channel_id: Uuid::now_v7(),
            channel,
            sequence: 1,
            request_id: None,
            operation_id: None,
            limits: Limits {
                max_physical_frame_bytes: 1024 * 1024,
                max_reassembled_message_bytes: 16 * 1024 * 1024,
                max_page_items: 1000,
                max_page_encoded_bytes: 512 * 1024,
                max_chunk_bytes: 256 * 1024,
                max_cumulative_bytes: 16 * 1024 * 1024,
            },
            compression: "none",
            privacy_class,
            payload_kind,
            payload_version: 1,
            payload: &[0x01, 0x02],
        })
        .expect("bypass MessagePack")
    }

    #[test]
    fn host_output_managed_metadata_rejected_on_encode_and_decode_bypass() {
        for (kind, channel) in [(19_u16, "durable"), (20, "critical"), (21, "ephemeral")] {
            assert!(
                encode(&envelope(kind, channel, "managed_metadata")).is_err(),
                "encode must reject ManagedMetadata for kind {kind}"
            );

            let tampered = bypass_encoder_msgpack(kind, channel, "managed_metadata");
            assert!(
                decode_connect_envelope_json(&tampered).is_err(),
                "decode must reject ManagedMetadata bypass bytes for kind {kind}"
            );

            // LocalOnly still round-trips through decode for the same lanes.
            let allowed = bypass_encoder_msgpack(kind, channel, "local_only");
            assert!(
                decode_connect_envelope_json(&allowed).is_ok(),
                "decode must still accept LocalOnly for kind {kind}"
            );
        }

        let stream_raw = bypass_encoder_msgpack(21, "ephemeral", "raw_content");
        assert!(decode_connect_envelope_json(&stream_raw).is_ok());
    }

    #[test]
    fn payload_json_decodes_uuid_bin_and_preserves_opaque_bytes() {
        // Native DomainId/Uuid serde emits MessagePack bin16 for identity fields.
        let reproduced: [u8; 22] = [
            0x81, 0xa2, 0x69, 0x64, 0xc4, 16, 1, 35, 69, 103, 137, 171, 112, 0, 128, 0, 0, 0, 0, 0,
            0, 85,
        ];
        let decoded = decode_payload(&reproduced).expect("UUID bin must decode");
        let expected_id = [
            1_u8, 35, 69, 103, 137, 171, 112, 0, 128, 0, 0, 0, 0, 0, 0, 85,
        ];
        assert_eq!(decoded["id"], bytes_as_json_array(&expected_id));
        assert!(
            decoded["id"].as_str().is_none(),
            "must not invent UUID strings from 16-byte blobs"
        );

        #[derive(Serialize)]
        struct WithUuid {
            id: Uuid,
            label: &'static str,
            count: u32,
        }

        let id = Uuid::from_bytes(expected_id);
        let named = rmp_serde::to_vec_named(&WithUuid {
            id,
            label: "task",
            count: 7,
        })
        .expect("named struct");
        let decoded = decode_payload(&named).expect("named uuid struct");
        assert_eq!(decoded["id"], bytes_as_json_array(id.as_bytes()));
        assert_eq!(decoded["label"], "task");
        assert_eq!(decoded["count"], 7);
        assert!(decoded["id"].as_str().is_none());

        #[derive(Serialize)]
        struct Nested {
            outer: WithUuid,
        }
        let nested = rmp_serde::to_vec_named(&Nested {
            outer: WithUuid {
                id,
                label: "nested",
                count: 1,
            },
        })
        .expect("nested");
        let decoded = decode_payload(&nested).expect("nested uuid map");
        assert_eq!(decoded["outer"]["id"], bytes_as_json_array(id.as_bytes()));
        assert_eq!(decoded["outer"]["label"], "nested");

        #[derive(Serialize)]
        struct Opaque {
            #[serde(serialize_with = "serialize_payload_bytes")]
            payload: Vec<u8>,
            tags: Vec<u16>,
        }

        let empty = rmp_serde::to_vec_named(&Opaque {
            payload: Vec::new(),
            tags: vec![1, 2, 3],
        })
        .expect("empty binary");
        let decoded = decode_payload(&empty).expect("empty binary");
        assert_eq!(decoded["payload"], json!([]));
        assert_eq!(decoded["tags"], json!([1, 2, 3]));

        let nonempty = rmp_serde::to_vec_named(&Opaque {
            payload: vec![0xde, 0xad, 0xbe, 0xef],
            tags: Vec::new(),
        })
        .expect("nonempty binary");
        let decoded = decode_payload(&nonempty).expect("nonempty binary");
        assert_eq!(
            decoded["payload"],
            bytes_as_json_array(&[0xde, 0xad, 0xbe, 0xef])
        );
        assert_eq!(decoded["tags"], json!([]));
    }

    #[test]
    fn payload_json_rejects_duplicate_nonstring_trailing_and_deep_nesting() {
        // Duplicate string keys.
        let duplicate = [
            0x82_u8, // map 2
            0xa1, b'a', 0x01, // "a": 1
            0xa1, b'a', 0x02, // "a": 2
        ];
        assert!(decode_payload(&duplicate).is_err());
        let allowed_map = [0x81_u8, 0xa1, b'a', 0x01];
        assert_eq!(
            decode_payload(&allowed_map).expect("unique key"),
            json!({"a": 1})
        );

        // Non-string map key (integer).
        let nonstring_key = [0x81_u8, 0x01, 0xa3, b'o', b'k', b'a'];
        assert!(decode_payload(&nonstring_key).is_err());
        let allowed_string_key = [0x81_u8, 0xa1, b'k', 0xc0];
        assert_eq!(
            decode_payload(&allowed_string_key).expect("string key"),
            json!({"k": null})
        );

        // Trailing garbage after a complete value.
        let trailing = [0xc0_u8, 0x00];
        assert!(decode_payload(&trailing).is_err());
        assert_eq!(decode_payload(&[0xc0]).expect("nil"), Value::Null);

        // Binary map key is non-string.
        let binary_key = [0x81_u8, 0xc4, 1, 0x61, 0x01];
        assert!(decode_payload(&binary_key).is_err());

        // Depth: shallow nest allowed; at/beyond max depth rejected.
        fn nest_arrays(depth: usize) -> Vec<u8> {
            let mut bytes = vec![0xc0_u8];
            for _ in 0..depth {
                let mut next = Vec::with_capacity(bytes.len() + 1);
                next.push(0x91);
                next.extend_from_slice(&bytes);
                bytes = next;
            }
            bytes
        }
        assert!(decode_payload(&nest_arrays(32)).is_ok());
        assert!(decode_payload(&nest_arrays(128)).is_err());
        assert!(decode_payload(&nest_arrays(200)).is_err());
    }

    fn msgpack_u64(value: u64) -> Vec<u8> {
        let mut bytes = vec![0xcf];
        bytes.extend_from_slice(&value.to_be_bytes());
        bytes
    }

    fn msgpack_i64(value: i64) -> Vec<u8> {
        let mut bytes = vec![0xd3];
        bytes.extend_from_slice(&value.to_be_bytes());
        bytes
    }

    fn msgpack_f64(value: f64) -> Vec<u8> {
        let mut bytes = vec![0xcb];
        bytes.extend_from_slice(&value.to_be_bytes());
        bytes
    }

    #[test]
    fn payload_json_rejects_integers_beyond_js_max_safe_integer() {
        const MAX_SAFE: u64 = 9_007_199_254_740_991;
        const MAX_SAFE_I: i64 = 9_007_199_254_740_991;

        assert_eq!(
            decode_payload(&msgpack_u64(MAX_SAFE)).expect("u64 max safe"),
            json!(MAX_SAFE)
        );
        assert!(decode_payload(&msgpack_u64(MAX_SAFE + 1)).is_err());

        assert_eq!(
            decode_payload(&msgpack_i64(MAX_SAFE_I)).expect("i64 max safe"),
            json!(MAX_SAFE_I)
        );
        assert_eq!(
            decode_payload(&msgpack_i64(-MAX_SAFE_I)).expect("i64 min safe"),
            json!(-MAX_SAFE_I)
        );
        assert!(decode_payload(&msgpack_i64(MAX_SAFE_I + 1)).is_err());
        assert!(decode_payload(&msgpack_i64(-MAX_SAFE_I - 1)).is_err());

        let safe_f64 = MAX_SAFE as f64;
        assert_eq!(
            decode_payload(&msgpack_f64(safe_f64)).expect("f64 integral max safe"),
            json!(MAX_SAFE)
        );
        assert!(decode_payload(&msgpack_f64(safe_f64 + 1.0)).is_err());
        assert!(decode_payload(&msgpack_f64(-(safe_f64 + 1.0))).is_err());

        // Non-integral finite floats remain admissible inside the decoder.
        let fractional = decode_payload(&msgpack_f64(1.5)).expect("fractional f64");
        assert_eq!(fractional, json!(1.5));
    }

    #[test]
    fn payload_json_rejects_nonfinite_floats_and_extensions() {
        assert!(decode_payload(&msgpack_f64(f64::NAN)).is_err());
        assert!(decode_payload(&msgpack_f64(f64::INFINITY)).is_err());
        assert!(decode_payload(&msgpack_f64(f64::NEG_INFINITY)).is_err());
        assert_eq!(
            decode_payload(&msgpack_f64(2.25)).expect("finite f64"),
            json!(2.25)
        );

        // fixext1: type 0, one data byte — unsupported Ext.
        let fixext1 = [0xd4_u8, 0x00, 0x7f];
        assert!(decode_payload(&fixext1).is_err());
        assert_eq!(
            decode_payload(&[0xc0]).expect("nil counterpart"),
            Value::Null
        );
    }

    fn decode_require_bin(bytes: &[u8]) -> Result<Vec<u8>, ()> {
        struct BinOnly;

        impl<'de> Visitor<'de> for BinOnly {
            type Value = Vec<u8>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("MessagePack BIN")
            }

            fn visit_bytes<E: de::Error>(self, value: &[u8]) -> Result<Self::Value, E> {
                Ok(value.to_vec())
            }

            fn visit_byte_buf<E: de::Error>(self, value: Vec<u8>) -> Result<Self::Value, E> {
                Ok(value)
            }

            fn visit_seq<A>(self, _seq: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                Err(de::Error::invalid_type(de::Unexpected::Seq, &self))
            }
        }

        let mut deserializer = rmp_serde::Deserializer::new(std::io::Cursor::new(bytes));
        deserializer.deserialize_bytes(BinOnly).map_err(|_| ())
    }

    mod require_bin_field {
        use super::*;

        pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
        where
            D: Deserializer<'de>,
        {
            struct BinOnly;

            impl<'de> Visitor<'de> for BinOnly {
                type Value = Vec<u8>;

                fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                    formatter.write_str("MessagePack BIN")
                }

                fn visit_bytes<E: de::Error>(self, value: &[u8]) -> Result<Self::Value, E> {
                    Ok(value.to_vec())
                }

                fn visit_byte_buf<E: de::Error>(self, value: Vec<u8>) -> Result<Self::Value, E> {
                    Ok(value)
                }

                fn visit_seq<A>(self, _seq: A) -> Result<Self::Value, A::Error>
                where
                    A: SeqAccess<'de>,
                {
                    Err(de::Error::invalid_type(de::Unexpected::Seq, &self))
                }
            }

            deserializer.deserialize_bytes(BinOnly)
        }
    }

    #[test]
    fn payload_json_encode_connect_binary_marker_emits_msgpack_bin() {
        let empty = encode_connect_payload_msgpack(r#"{"$connectBinary":""}"#)
            .expect("empty binary marker");
        assert_eq!(
            decode_require_bin(&empty).expect("empty BIN"),
            Vec::<u8>::new()
        );

        let cursor = [0xaa_u8, 0xbb, 0xcc];
        let encoded = base64::engine::general_purpose::STANDARD.encode(cursor);
        let nested = format!(
            r#"{{"section":"tasks","snapshot_id":"01234567-89ab-7cde-8f01-23456789abcd","resume_cursor":{{"$connectBinary":"{encoded}"}}}}"#
        );
        let bytes = encode_connect_payload_msgpack(&nested).expect("snapshot resume_cursor");

        #[derive(Deserialize)]
        struct SnapshotPageQuery {
            section: String,
            snapshot_id: String,
            #[serde(deserialize_with = "require_bin_field::deserialize")]
            resume_cursor: Vec<u8>,
        }
        let query: SnapshotPageQuery =
            rmp_serde::from_slice(&bytes).expect("native-shaped query with BIN cursor");
        assert_eq!(query.section, "tasks");
        assert_eq!(query.snapshot_id, "01234567-89ab-7cde-8f01-23456789abcd");
        assert_eq!(query.resume_cursor, cursor);

        let plain_array = encode_connect_payload_msgpack(
            r#"{"id":[1,35,69,103,137,171,112,0,128,0,0,0,0,0,0,85]}"#,
        )
        .expect("plain 16-byte array");
        #[derive(Deserialize)]
        struct WithBinId {
            #[serde(deserialize_with = "require_bin_field::deserialize")]
            id: Vec<u8>,
        }
        assert!(
            rmp_serde::from_slice::<WithBinId>(&plain_array).is_err(),
            "plain arrays must not become MessagePack BIN"
        );
        let decoded = decode_payload(&plain_array).expect("array payload still decodes");
        assert_eq!(
            decoded["id"],
            bytes_as_json_array(&[1, 35, 69, 103, 137, 171, 112, 0, 128, 0, 0, 0, 0, 0, 0, 85])
        );
    }

    #[test]
    fn payload_json_encode_rejects_malformed_marker_and_bounds() {
        assert!(encode_connect_payload_msgpack(r#"{"$connectBinary":"!!!"}"#).is_err());
        assert!(encode_connect_payload_msgpack(r#"{"$connectBinary":"AQID","extra":1}"#).is_err());
        assert!(encode_connect_payload_msgpack(r#"{"$connectBinary":1}"#).is_err());
        // Missing STANDARD padding is noncanonical.
        assert!(encode_connect_payload_msgpack(r#"{"$connectBinary":"AQI"}"#).is_err());
        let padded = encode_connect_payload_msgpack(r#"{"$connectBinary":"AQI="}"#)
            .expect("canonical padded base64");
        assert_eq!(decode_require_bin(&padded).expect("BIN"), vec![0x01, 0x02]);

        assert!(encode_connect_payload_msgpack(r#"{"a":1,"a":2}"#).is_err());
        assert!(encode_connect_payload_msgpack(r#"{"a":1}"#).is_ok());
        assert!(encode_connect_payload_msgpack("{}{}").is_err());
        assert!(encode_connect_payload_msgpack("{}").is_ok());

        assert!(encode_connect_payload_msgpack(&format!(
            r#"{{"n":{}}}"#,
            9_007_199_254_740_992_u64
        ))
        .is_err());
        assert!(encode_connect_payload_msgpack(&format!(
            r#"{{"n":{}}}"#,
            9_007_199_254_740_991_u64
        ))
        .is_ok());

        fn nest_json(depth: usize) -> String {
            let mut body = "null".to_owned();
            for _ in 0..depth {
                body = format!("[{body}]");
            }
            body
        }
        assert!(encode_connect_payload_msgpack(&nest_json(32)).is_ok());
        assert!(encode_connect_payload_msgpack(&nest_json(129)).is_err());

        let oversize = "A".repeat((16_usize * 1024 * 1024).div_ceil(3) * 4 + 4);
        assert!(
            encode_connect_payload_msgpack(&format!(r#"{{"$connectBinary":"{oversize}"}}"#))
                .is_err()
        );
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
