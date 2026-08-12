//! Bounded ConnectEnvelope MessagePack boundary for the WASM facade.
//!
//! The JSON strings accepted here are only an ergonomic ABI at the JS/WASM
//! boundary. The websocket never receives JSON: the returned bytes are the
//! same named-field MessagePack envelope used by the native server.

use base64::Engine;
use serde::de::{self, Deserializer, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;
use uuid::{Uuid, Variant};
use wasm_bindgen::prelude::*;

// These limits intentionally mirror the native Connect v1 contract in
// `src/connect/envelope.rs`. Keep the checks in `WireEnvelope::validate` so
// both JSON input and decoded MessagePack take exactly the same path.
const MAX_PHYSICAL_FRAME_BYTES: usize = 1024 * 1024;
const MAX_REASSEMBLED_MESSAGE_BYTES: usize = 16 * 1024 * 1024;
const MAX_PAGE_ITEMS: usize = 1_000;
const MAX_PAGE_ENCODED_BYTES: usize = 512 * 1024;
const MAX_CHUNK_BYTES: usize = 256 * 1024;
const MAX_CUMULATIVE_BYTES: u64 = MAX_REASSEMBLED_MESSAGE_BYTES as u64;
const PROTOCOL_MINOR: u16 = 0;

fn wire_error() -> JsValue {
    JsValue::from_str("connect envelope rejected")
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireLimits {
    max_physical_frame_bytes: u32,
    max_reassembled_message_bytes: u32,
    max_page_items: u32,
    max_page_encoded_bytes: u32,
    max_chunk_bytes: u32,
    max_cumulative_bytes: u64,
}

impl WireLimits {
    fn validate(self, payload_len: usize) -> Result<(), JsValue> {
        let bounded = [
            self.max_physical_frame_bytes as usize,
            self.max_reassembled_message_bytes as usize,
            self.max_page_items as usize,
            self.max_page_encoded_bytes as usize,
            self.max_chunk_bytes as usize,
        ];
        if bounded.iter().any(|value| *value == 0)
            || self.max_physical_frame_bytes as usize > MAX_PHYSICAL_FRAME_BYTES
            || self.max_reassembled_message_bytes as usize > MAX_REASSEMBLED_MESSAGE_BYTES
            || self.max_page_items as usize > MAX_PAGE_ITEMS
            || self.max_page_encoded_bytes as usize > MAX_PAGE_ENCODED_BYTES
            || self.max_chunk_bytes as usize > MAX_CHUNK_BYTES
            || self.max_chunk_bytes > self.max_physical_frame_bytes
            || self.max_chunk_bytes > self.max_reassembled_message_bytes
            || self.max_chunk_bytes as u64 > self.max_cumulative_bytes
            || self.max_cumulative_bytes > MAX_CUMULATIVE_BYTES
            || self.max_cumulative_bytes > self.max_reassembled_message_bytes as u64
            || payload_len > self.max_reassembled_message_bytes as usize
        {
            return Err(wire_error());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum WireChannel {
    Critical,
    Durable,
    Ephemeral,
}

impl WireChannel {
    const fn for_payload_kind(payload_kind: u16) -> Option<Self> {
        Some(match payload_kind {
            // Critical: hello, capabilities, query, query reply, command,
            // command receipt, operation settlement, resync, and error.
            1 | 2 | 5 | 18 | 6 | 7 | 8 | 15 | 16 => Self::Critical,
            // Durable: snapshot/event pages, prompt/browser extensions,
            // chunks, and the reserved extension kind.
            3 | 4 | 12 | 13 | 14 | 17 => Self::Durable,
            // Ephemeral: presence, terminal deltas, and browser frames.
            9 | 10 | 11 => Self::Ephemeral,
            // Unknown/future kinds remain inert extension data, just as they
            // do in the native contract, and are not assigned a channel here.
            _ => return None,
        })
    }

    const fn allows_raw_content(payload_kind: u16) -> bool {
        matches!(payload_kind, 10 | 11 | 14)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum WireCompression {
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum WirePrivacy {
    LocalOnly,
    ManagedMetadata,
    RawContent,
}

mod binary_payload {
    use super::*;
    use serde::ser::Serializer;

    pub fn serialize<S>(value: &[u8], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_bytes(value)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct BytesVisitor;

        impl<'de> Visitor<'de> for BytesVisitor {
            type Value = Vec<u8>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("MessagePack binary bytes")
            }

            fn visit_bytes<E>(self, value: &[u8]) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(value.to_vec())
            }

            fn visit_byte_buf<E>(self, value: Vec<u8>) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(value)
            }

            fn visit_seq<A>(self, _seq: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                Err(de::Error::invalid_type(de::Unexpected::Seq, &self))
            }
        }

        deserializer.deserialize_bytes(BytesVisitor)
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireEnvelope {
    protocol_major: u16,
    protocol_minor: u16,
    connection_id: Uuid,
    session_id: Uuid,
    channel_id: Uuid,
    channel: WireChannel,
    sequence: u64,
    request_id: Option<Uuid>,
    operation_id: Option<Uuid>,
    limits: WireLimits,
    compression: WireCompression,
    privacy_class: WirePrivacy,
    payload_kind: u16,
    payload_version: u16,
    #[serde(with = "binary_payload")]
    payload: Vec<u8>,
}

impl WireEnvelope {
    fn validate(&self) -> Result<(), JsValue> {
        if self.protocol_major != crate::PROTOCOL_MAJOR
            || self.protocol_minor > PROTOCOL_MINOR
            || self.sequence == 0
            || self.payload_kind == 0
            || self.payload_version == 0
            || !matches!(self.compression, WireCompression::None)
        {
            return Err(wire_error());
        }
        for identifier in [self.connection_id, self.session_id, self.channel_id] {
            if identifier.get_version_num() != 7 || identifier.get_variant() != Variant::RFC4122 {
                return Err(wire_error());
            }
        }
        for identifier in [self.request_id, self.operation_id].into_iter().flatten() {
            if identifier.get_version_num() != 7 || identifier.get_variant() != Variant::RFC4122 {
                return Err(wire_error());
            }
        }
        self.limits.validate(self.payload.len())?;
        if let Some(expected_channel) = WireChannel::for_payload_kind(self.payload_kind) {
            if expected_channel != self.channel {
                return Err(wire_error());
            }
        }
        if matches!(self.privacy_class, WirePrivacy::RawContent)
            && !WireChannel::allows_raw_content(self.payload_kind)
        {
            return Err(wire_error());
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct EnvelopeJson {
    protocol_major: u16,
    protocol_minor: u16,
    connection_id: String,
    session_id: String,
    channel_id: String,
    channel: WireChannel,
    sequence: u64,
    request_id: Option<String>,
    operation_id: Option<String>,
    limits: WireLimits,
    compression: WireCompression,
    privacy_class: WirePrivacy,
    payload_kind: u16,
    payload_version: u16,
    payload_base64: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct EnvelopeJsonOut {
    protocol_major: u16,
    protocol_minor: u16,
    connection_id: String,
    session_id: String,
    channel_id: String,
    channel: WireChannel,
    sequence: u64,
    request_id: Option<String>,
    operation_id: Option<String>,
    limits: WireLimits,
    compression: WireCompression,
    privacy_class: WirePrivacy,
    payload_kind: u16,
    payload_version: u16,
    payload_base64: String,
}

fn uuid(value: &str) -> Result<Uuid, JsValue> {
    let parsed = Uuid::parse_str(value).map_err(|_| wire_error())?;
    if parsed.get_version_num() != 7 || parsed.get_variant() != Variant::RFC4122 {
        return Err(wire_error());
    }
    Ok(parsed)
}

fn envelope_from_json(input: EnvelopeJson) -> Result<WireEnvelope, JsValue> {
    if input.payload_kind == 0 {
        return Err(wire_error());
    }
    let payload = base64::engine::general_purpose::STANDARD
        .decode(input.payload_base64)
        .map_err(|_| wire_error())?;
    let envelope = WireEnvelope {
        protocol_major: input.protocol_major,
        protocol_minor: input.protocol_minor,
        connection_id: uuid(&input.connection_id)?,
        session_id: uuid(&input.session_id)?,
        channel_id: uuid(&input.channel_id)?,
        channel: input.channel,
        sequence: input.sequence,
        request_id: input.request_id.as_deref().map(uuid).transpose()?,
        operation_id: input.operation_id.as_deref().map(uuid).transpose()?,
        limits: input.limits,
        compression: input.compression,
        privacy_class: input.privacy_class,
        payload_kind: input.payload_kind,
        payload_version: input.payload_version,
        payload,
    };
    envelope.validate()?;
    Ok(envelope)
}

fn envelope_to_json(envelope: WireEnvelope) -> EnvelopeJsonOut {
    EnvelopeJsonOut {
        protocol_major: envelope.protocol_major,
        protocol_minor: envelope.protocol_minor,
        connection_id: envelope.connection_id.to_string(),
        session_id: envelope.session_id.to_string(),
        channel_id: envelope.channel_id.to_string(),
        channel: envelope.channel,
        sequence: envelope.sequence,
        request_id: envelope.request_id.map(|value| value.to_string()),
        operation_id: envelope.operation_id.map(|value| value.to_string()),
        limits: envelope.limits,
        compression: envelope.compression,
        privacy_class: envelope.privacy_class,
        payload_kind: envelope.payload_kind,
        payload_version: envelope.payload_version,
        payload_base64: base64::engine::general_purpose::STANDARD.encode(envelope.payload),
    }
}

/// Encode a ConnectEnvelope from its stable, non-secret JSON ABI into the
/// native named-field MessagePack wire format.
#[wasm_bindgen]
pub fn encode_connect_envelope_json(input: String) -> Result<Vec<u8>, JsValue> {
    let input: EnvelopeJson = serde_json::from_str(&input).map_err(|_| wire_error())?;
    let envelope = envelope_from_json(input)?;
    let bytes = rmp_serde::to_vec_named(&envelope).map_err(|_| wire_error())?;
    if bytes.len() > envelope.limits.max_physical_frame_bytes as usize {
        return Err(wire_error());
    }
    Ok(bytes)
}

/// Decode only the bounded envelope metadata. Payload bytes remain base64 in
/// this diagnostic/dispatch ABI and are never logged or interpolated in an
/// error string.
#[wasm_bindgen]
pub fn decode_connect_envelope_json(input: &[u8]) -> Result<String, JsValue> {
    if input.is_empty() || input.len() > MAX_PHYSICAL_FRAME_BYTES {
        return Err(wire_error());
    }
    let envelope: WireEnvelope = rmp_serde::from_slice(input).map_err(|_| wire_error())?;
    envelope.validate()?;
    if input.len() > envelope.limits.max_physical_frame_bytes as usize {
        return Err(wire_error());
    }
    serde_json::to_string(&envelope_to_json(envelope)).map_err(|_| wire_error())
}

/// Encode a typed Connect payload map using the same named MessagePack
/// serializer as the native envelope. Binary payloads should be represented by
/// the envelope JSON ABI's `payloadBase64` field instead.
#[wasm_bindgen]
pub fn encode_connect_payload_json(input: String) -> Result<Vec<u8>, JsValue> {
    let value: Value = serde_json::from_str(&input).map_err(|_| wire_error())?;
    let bytes = rmp_serde::to_vec_named(&value).map_err(|_| wire_error())?;
    if bytes.is_empty() || bytes.len() > MAX_REASSEMBLED_MESSAGE_BYTES {
        return Err(wire_error());
    }
    Ok(bytes)
}

#[wasm_bindgen]
pub fn decode_connect_payload_json(input: &[u8]) -> Result<String, JsValue> {
    if input.is_empty() || input.len() > MAX_REASSEMBLED_MESSAGE_BYTES {
        return Err(wire_error());
    }
    let value: Value = rmp_serde::from_slice(input).map_err(|_| wire_error())?;
    serde_json::to_string(&value).map_err(|_| wire_error())
}
