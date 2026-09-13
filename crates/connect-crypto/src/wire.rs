//! Bounded ConnectEnvelope MessagePack boundary for the WASM facade.
//!
//! The JSON strings accepted here are only an ergonomic ABI at the JS/WASM
//! boundary. The websocket never receives JSON: the returned bytes are the
//! same named-field MessagePack envelope used by the native server.

use base64::Engine;
use serde::de::{self, DeserializeSeed, Deserializer, MapAccess, SeqAccess, Visitor};
use serde::ser::{self, SerializeMap, SerializeSeq, Serializer};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Number, Value};
use std::fmt;
use std::marker::PhantomData;
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
const MAX_PAYLOAD_JSON_DEPTH: usize = 128;
/// Decoded JSON tree budget (Values + copied string/key bytes), not a Value count.
const MAX_DECODED_JSON_BYTES: usize = 64 * 1024 * 1024;
const JS_MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;
const JS_MAX_SAFE_INTEGER_U: u64 = 9_007_199_254_740_991;
const CONNECT_BINARY_KEY: &str = "$connectBinary";

fn wire_error() -> JsValue {
    // wasm32 keeps the stable redacted string. Native `--features wasm` tests must
    // not invoke the wasm string import; NULL is a fixed Err sentinel only.
    #[cfg(target_arch = "wasm32")]
    {
        JsValue::from_str("connect envelope rejected")
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        JsValue::NULL
    }
}

/// Private fail-closed marker for payload JSON codec paths. Mapped to
/// [`wire_error`] at the WASM boundary so no diagnostic plaintext escapes.
#[derive(Debug)]
pub struct PayloadJsonError;

impl fmt::Display for PayloadJsonError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("rejected")
    }
}

impl std::error::Error for PayloadJsonError {}

impl ser::Error for PayloadJsonError {
    fn custom<T: fmt::Display>(_msg: T) -> Self {
        Self
    }
}

impl de::Error for PayloadJsonError {
    fn custom<T: fmt::Display>(_msg: T) -> Self {
        Self
    }
}

struct DecodeBudget {
    used: usize,
}

impl DecodeBudget {
    fn new() -> Self {
        Self { used: 0 }
    }

    fn account<E: de::Error>(&mut self, bytes: usize) -> Result<(), E> {
        self.used = self.used.saturating_add(bytes);
        if self.used > MAX_DECODED_JSON_BYTES {
            return Err(reject());
        }
        Ok(())
    }

    fn account_value<E: de::Error>(&mut self) -> Result<(), E> {
        self.account(std::mem::size_of::<Value>())
    }

    fn account_string<E: de::Error>(&mut self, len: usize) -> Result<(), E> {
        self.account(std::mem::size_of::<Value>().saturating_add(len))
    }

    fn account_binary_array<E: de::Error>(&mut self, len: usize) -> Result<(), E> {
        // Array Value plus one Number Value per byte; check before allocation.
        let per_number = std::mem::size_of::<Value>();
        let cost = per_number.saturating_add(len.saturating_mul(per_number));
        self.account(cost)
    }
}

fn reject<E: de::Error>() -> E {
    E::custom("rejected")
}

fn max_container_len_hint() -> usize {
    MAX_DECODED_JSON_BYTES / std::mem::size_of::<Value>().max(1)
}

fn js_safe_i64<E: de::Error>(value: i64) -> Result<i64, E> {
    if !(-JS_MAX_SAFE_INTEGER..=JS_MAX_SAFE_INTEGER).contains(&value) {
        return Err(reject());
    }
    Ok(value)
}

fn js_safe_u64<E: de::Error>(value: u64) -> Result<u64, E> {
    if value > JS_MAX_SAFE_INTEGER_U {
        return Err(reject());
    }
    Ok(value)
}

fn js_safe_f64<E: de::Error>(value: f64) -> Result<f64, E> {
    if !value.is_finite() {
        return Err(reject());
    }
    if value.fract() == 0.0 {
        let max = JS_MAX_SAFE_INTEGER as f64;
        if value > max || value < -max {
            return Err(reject());
        }
    }
    Ok(value)
}

struct ValueSeed<'a> {
    budget: &'a mut DecodeBudget,
}

impl<'de> DeserializeSeed<'de> for ValueSeed<'_> {
    type Value = Value;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(ValueVisitor {
            budget: self.budget,
            _phantom: PhantomData,
        })
    }
}

struct ValueVisitor<'a, 'de> {
    budget: &'a mut DecodeBudget,
    _phantom: PhantomData<&'de ()>,
}

impl<'de> Visitor<'de> for ValueVisitor<'_, 'de> {
    type Value = Value;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("MessagePack value")
    }

    fn visit_bool<E: de::Error>(self, value: bool) -> Result<Self::Value, E> {
        self.budget.account_value()?;
        Ok(Value::Bool(value))
    }

    fn visit_i64<E: de::Error>(self, value: i64) -> Result<Self::Value, E> {
        let value = js_safe_i64(value)?;
        self.budget.account_value()?;
        Ok(Value::Number(Number::from(value)))
    }

    fn visit_u64<E: de::Error>(self, value: u64) -> Result<Self::Value, E> {
        let value = js_safe_u64(value)?;
        self.budget.account_value()?;
        Ok(Value::Number(Number::from(value)))
    }

    fn visit_i128<E: de::Error>(self, _value: i128) -> Result<Self::Value, E> {
        Err(reject())
    }

    fn visit_u128<E: de::Error>(self, _value: u128) -> Result<Self::Value, E> {
        Err(reject())
    }

    fn visit_f64<E: de::Error>(self, value: f64) -> Result<Self::Value, E> {
        let value = js_safe_f64(value)?;
        self.budget.account_value()?;
        // Integral MessagePack floats have the same JSON-number semantics as
        // integers. Keep their exact decimal spelling at the JS-safe boundary.
        if value.fract() == 0.0 {
            return Ok(Value::Number(Number::from(value as i64)));
        }
        Number::from_f64(value)
            .map(Value::Number)
            .ok_or_else(reject)
    }

    fn visit_f32<E: de::Error>(self, value: f32) -> Result<Self::Value, E> {
        self.visit_f64(f64::from(value))
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
        self.budget.account_string(value.len())?;
        Ok(Value::String(value.to_owned()))
    }

    fn visit_string<E: de::Error>(self, value: String) -> Result<Self::Value, E> {
        self.budget.account_string(value.len())?;
        Ok(Value::String(value))
    }

    fn visit_bytes<E: de::Error>(self, value: &[u8]) -> Result<Self::Value, E> {
        // Physical binaries up to the 1 MiB frame limit remain admissible; the
        // decoded JSON array form is charged against the 64 MiB tree budget.
        if value.len() > MAX_PHYSICAL_FRAME_BYTES {
            return Err(reject());
        }
        self.budget.account_binary_array(value.len())?;
        Ok(Value::Array(
            value
                .iter()
                .copied()
                .map(|byte| Value::Number(Number::from(byte)))
                .collect(),
        ))
    }

    fn visit_byte_buf<E: de::Error>(self, value: Vec<u8>) -> Result<Self::Value, E> {
        self.visit_bytes(&value)
    }

    fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
        self.budget.account_value()?;
        Ok(Value::Null)
    }

    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        self.budget.account_value()?;
        Ok(Value::Null)
    }

    fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        ValueSeed {
            budget: self.budget,
        }
        .deserialize(deserializer)
    }

    fn visit_newtype_struct<D>(self, _deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        Err(reject())
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        self.budget.account_value()?;
        let mut items = Vec::new();
        if let Some(hint) = seq.size_hint() {
            if hint > max_container_len_hint() {
                return Err(reject());
            }
            items.reserve(hint.min(1024));
        }
        while let Some(item) = seq.next_element_seed(ValueSeed {
            budget: self.budget,
        })? {
            items.push(item);
        }
        Ok(Value::Array(items))
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        self.budget.account_value()?;
        let mut object = Map::new();
        if let Some(hint) = map.size_hint() {
            if hint > max_container_len_hint() {
                return Err(reject());
            }
        }
        while let Some(key) = map.next_key_seed(StringKeySeed {
            budget: self.budget,
        })? {
            if object.contains_key(&key) {
                return Err(reject());
            }
            let value = map.next_value_seed(ValueSeed {
                budget: self.budget,
            })?;
            object.insert(key, value);
        }
        Ok(Value::Object(object))
    }
}

struct StringKeySeed<'a> {
    budget: &'a mut DecodeBudget,
}

impl<'de> DeserializeSeed<'de> for StringKeySeed<'_> {
    type Value = String;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(StringKeyVisitor {
            budget: self.budget,
        })
    }
}

struct StringKeyVisitor<'a> {
    budget: &'a mut DecodeBudget,
}

impl<'de> Visitor<'de> for StringKeyVisitor<'_> {
    type Value = String;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("string map key")
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
        self.budget.account(value.len())?;
        Ok(value.to_owned())
    }

    fn visit_string<E: de::Error>(self, value: String) -> Result<Self::Value, E> {
        self.budget.account(value.len())?;
        Ok(value)
    }

    fn visit_bytes<E: de::Error>(self, _value: &[u8]) -> Result<Self::Value, E> {
        Err(reject())
    }

    fn visit_byte_buf<E: de::Error>(self, _value: Vec<u8>) -> Result<Self::Value, E> {
        Err(reject())
    }
}

fn decode_msgpack_to_json(input: &[u8]) -> Result<Value, PayloadJsonError> {
    let mut deserializer = rmp_serde::Deserializer::new(std::io::Cursor::new(input));
    deserializer.set_max_depth(MAX_PAYLOAD_JSON_DEPTH);
    let mut budget = DecodeBudget::new();
    let value = ValueSeed {
        budget: &mut budget,
    }
    .deserialize(&mut deserializer)
    .map_err(|_| PayloadJsonError)?;
    if deserializer.position() as usize != input.len() {
        return Err(PayloadJsonError);
    }
    Ok(value)
}

fn parse_json_to_value(input: &str) -> Result<Value, PayloadJsonError> {
    if input.is_empty() || input.len() > MAX_DECODED_JSON_BYTES {
        return Err(PayloadJsonError);
    }
    let mut deserializer = serde_json::Deserializer::from_str(input);
    let mut budget = DecodeBudget::new();
    let value = ValueSeed {
        budget: &mut budget,
    }
    .deserialize(&mut deserializer)
    .map_err(|_| PayloadJsonError)?;
    deserializer.end().map_err(|_| PayloadJsonError)?;
    Ok(value)
}

fn decode_connect_binary_marker(
    map: &Map<String, Value>,
) -> Result<Option<Vec<u8>>, PayloadJsonError> {
    if !map.contains_key(CONNECT_BINARY_KEY) {
        return Ok(None);
    }
    if map.len() != 1 {
        return Err(PayloadJsonError);
    }
    let Some(Value::String(encoded)) = map.get(CONNECT_BINARY_KEY) else {
        return Err(PayloadJsonError);
    };
    if encoded.len() > MAX_REASSEMBLED_MESSAGE_BYTES.div_ceil(3).saturating_mul(4) {
        return Err(PayloadJsonError);
    }
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded.as_bytes())
        .map_err(|_| PayloadJsonError)?;
    if decoded.len() > MAX_REASSEMBLED_MESSAGE_BYTES {
        return Err(PayloadJsonError);
    }
    let canonical = base64::engine::general_purpose::STANDARD.encode(&decoded);
    if canonical != *encoded {
        return Err(PayloadJsonError);
    }
    Ok(Some(decoded))
}

struct EncodeNode<'a> {
    value: &'a Value,
    depth: usize,
}

impl Serialize for EncodeNode<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        if self.depth > MAX_PAYLOAD_JSON_DEPTH {
            return Err(ser::Error::custom("rejected"));
        }
        match self.value {
            Value::Null => serializer.serialize_unit(),
            Value::Bool(value) => serializer.serialize_bool(*value),
            Value::Number(number) => {
                if let Some(value) = number.as_i64() {
                    serializer.serialize_i64(value)
                } else if let Some(value) = number.as_u64() {
                    serializer.serialize_u64(value)
                } else if let Some(value) = number.as_f64() {
                    serializer.serialize_f64(value)
                } else {
                    Err(ser::Error::custom("rejected"))
                }
            }
            Value::String(value) => serializer.serialize_str(value),
            Value::Array(items) => {
                let mut seq = serializer.serialize_seq(Some(items.len()))?;
                for item in items {
                    seq.serialize_element(&EncodeNode {
                        value: item,
                        depth: self.depth.saturating_add(1),
                    })?;
                }
                seq.end()
            }
            Value::Object(map) => match decode_connect_binary_marker(map) {
                Ok(Some(bytes)) => serializer.serialize_bytes(&bytes),
                Ok(None) => {
                    let mut object = serializer.serialize_map(Some(map.len()))?;
                    for (key, value) in map {
                        object.serialize_entry(
                            key,
                            &EncodeNode {
                                value,
                                depth: self.depth.saturating_add(1),
                            },
                        )?;
                    }
                    object.end()
                }
                Err(_) => Err(ser::Error::custom("rejected")),
            },
        }
    }
}

/// Encode a Connect payload JSON document into named MessagePack bytes.
///
/// Exact objects `{"$connectBinary":"<STANDARD padded base64>"}` become
/// MessagePack BIN. Ordinary arrays stay arrays; UUID strings stay strings.
/// The reserved marker must be the sole object key and use canonical base64.
pub fn encode_connect_payload_msgpack(input: &str) -> Result<Vec<u8>, PayloadJsonError> {
    let value = parse_json_to_value(input)?;
    let bytes = rmp_serde::to_vec_named(&EncodeNode {
        value: &value,
        depth: 0,
    })
    .map_err(|_| PayloadJsonError)?;
    if bytes.is_empty() || bytes.len() > MAX_REASSEMBLED_MESSAGE_BYTES {
        return Err(PayloadJsonError);
    }
    Ok(bytes)
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
            // command receipt, operation settlement, resync, error, and
            // lossless host critical output.
            1 | 2 | 5 | 18 | 6 | 7 | 8 | 15 | 16 | 20 => Self::Critical,
            // Durable: snapshot/event pages, prompt/browser extensions,
            // chunks, the reserved extension kind, and lossless host durable
            // output.
            3 | 4 | 12 | 13 | 14 | 17 | 19 => Self::Durable,
            // Ephemeral: presence, terminal deltas, browser frames, and
            // lossless host stream and semantic conversation output.
            9 | 10 | 11 | 21 | 22 => Self::Ephemeral,
            // Unknown/future kinds remain inert extension data, just as they
            // do in the native contract, and are not assigned a channel here.
            _ => return None,
        })
    }

    const fn allows_raw_content(payload_kind: u16) -> bool {
        // Mirror native RawContent grants: terminal/browser/chunk/host-stream.
        matches!(payload_kind, 10 | 11 | 14 | 21)
    }

    /// Host-output kinds carry local-session data and reject ManagedMetadata,
    /// matching native `ConnectEnvelope` privacy without opening payload bytes.
    const fn rejects_managed_metadata(payload_kind: u16) -> bool {
        matches!(payload_kind, 19 | 20 | 21 | 22)
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
        if matches!(self.privacy_class, WirePrivacy::ManagedMetadata)
            && WireChannel::rejects_managed_metadata(self.payload_kind)
        {
            return Err(wire_error());
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

/// Encode a typed Connect payload JSON document into the native named-field
/// MessagePack wire format.
///
/// Binary fields (for example native query `resume_cursor`) must use the exact
/// marker object `{"$connectBinary":"<STANDARD padded base64>"}`. That marker
/// serializes as MessagePack BIN. Ordinary JSON arrays remain arrays; UUID and
/// other identity strings remain strings. Envelope `payloadBase64` is a
/// separate envelope-level ABI and is unchanged here.
#[wasm_bindgen]
pub fn encode_connect_payload_json(input: String) -> Result<Vec<u8>, JsValue> {
    encode_connect_payload_msgpack(&input).map_err(|_| wire_error())
}

#[wasm_bindgen]
pub fn decode_connect_payload_json(input: &[u8]) -> Result<String, JsValue> {
    if input.is_empty() || input.len() > MAX_REASSEMBLED_MESSAGE_BYTES {
        return Err(wire_error());
    }
    let value = decode_msgpack_to_json(input).map_err(|_| wire_error())?;
    serde_json::to_string(&value).map_err(|_| wire_error())
}
