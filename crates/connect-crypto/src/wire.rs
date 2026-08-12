//! Bounded ConnectEnvelope MessagePack boundary for the WASM facade.
//!
//! The JSON strings accepted here are only an ergonomic ABI at the JS/WASM
//! boundary. The websocket never receives JSON: the returned bytes are the
//! same named-field MessagePack envelope used by the native server.

use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::{Uuid, Variant};
use wasm_bindgen::prelude::*;

const MAX_PHYSICAL_FRAME_BYTES: usize = 1024 * 1024;
const MAX_REASSEMBLED_MESSAGE_BYTES: usize = 16 * 1024 * 1024;
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
            || self.max_chunk_bytes > self.max_physical_frame_bytes
            || self.max_chunk_bytes as u64 > self.max_cumulative_bytes
            || self.max_cumulative_bytes > self.max_reassembled_message_bytes as u64
            || payload_len > self.max_reassembled_message_bytes as usize
        {
            return Err(wire_error());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum WireChannel {
    Critical,
    Durable,
    Ephemeral,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum WireCompression {
    None,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum WirePrivacy {
    LocalOnly,
    ManagedMetadata,
    RawContent,
}

mod binary_payload {
    use serde::ser::Serializer;

    pub fn serialize<S>(value: &[u8], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_bytes(value)
    }
}

#[derive(Debug, Serialize, Deserialize)]
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
    if input.protocol_major != crate::PROTOCOL_MAJOR
        || input.protocol_minor > PROTOCOL_MINOR
        || input.sequence == 0
        || input.payload_kind == 0
        || input.payload_version == 0
        || !matches!(input.compression, WireCompression::None)
    {
        return Err(wire_error());
    }
    let payload = base64::engine::general_purpose::STANDARD
        .decode(input.payload_base64)
        .map_err(|_| wire_error())?;
    input.limits.validate(payload.len())?;
    Ok(WireEnvelope {
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
    })
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
    if bytes.len() > MAX_PHYSICAL_FRAME_BYTES {
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
    envelope.limits.validate(envelope.payload.len())?;
    if envelope.protocol_major != crate::PROTOCOL_MAJOR
        || envelope.protocol_minor > PROTOCOL_MINOR
        || envelope.sequence == 0
        || envelope.payload_kind == 0
        || envelope.payload_version == 0
        || !matches!(envelope.compression, WireCompression::None)
    {
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
