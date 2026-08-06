//! Generic ephemeral resource-stream wire envelope.

use std::fmt;
use std::num::NonZeroU16;

use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::domain::id::{ResourceId, SubscriptionId};

/// One independently coalesced resource stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StreamKey(ResourceId);

impl StreamKey {
    pub const fn from_resource_id(resource_id: ResourceId) -> Self {
        Self(resource_id)
    }

    pub const fn resource_id(self) -> ResourceId {
        self.0
    }
}

impl From<ResourceId> for StreamKey {
    fn from(resource_id: ResourceId) -> Self {
        Self::from_resource_id(resource_id)
    }
}

impl Serialize for StreamKey {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for StreamKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        ResourceId::deserialize(deserializer).map(Self)
    }
}

/// Negotiated stream payload discriminant. Zero is rejected; unknown nonzero
/// values remain transportable for future decoders.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StreamPayloadKind(NonZeroU16);

impl StreamPayloadKind {
    pub const fn new(value: u16) -> Option<Self> {
        match NonZeroU16::new(value) {
            Some(inner) => Some(Self(inner)),
            None => None,
        }
    }

    pub const fn get(self) -> u16 {
        self.0.get()
    }
}

impl Serialize for StreamPayloadKind {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.get().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for StreamPayloadKind {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = u16::deserialize(deserializer)?;
        Self::new(value).ok_or_else(|| de::Error::custom("StreamPayloadKind must be nonzero"))
    }
}

impl fmt::Display for StreamPayloadKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.get().fmt(f)
    }
}

/// Host→client ephemeral resource stream frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamFrame {
    pub subscription_id: SubscriptionId,
    pub stream: StreamKey,
    pub generation: u64,
    pub sequence: u64,
    pub payload_kind: StreamPayloadKind,
    pub schema_version: u16,
    pub payload: Vec<u8>,
}

enum StreamFrameField {
    SubscriptionId,
    Stream,
    Generation,
    Sequence,
    PayloadKind,
    SchemaVersion,
    Payload,
}

impl<'de> Deserialize<'de> for StreamFrameField {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct FieldVisitor;

        impl Visitor<'_> for FieldVisitor {
            type Value = StreamFrameField;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(
                    "subscription_id, stream, generation, sequence, payload_kind, schema_version, or payload",
                )
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                match value {
                    "subscription_id" => Ok(StreamFrameField::SubscriptionId),
                    "stream" => Ok(StreamFrameField::Stream),
                    "generation" => Ok(StreamFrameField::Generation),
                    "sequence" => Ok(StreamFrameField::Sequence),
                    "payload_kind" => Ok(StreamFrameField::PayloadKind),
                    "schema_version" => Ok(StreamFrameField::SchemaVersion),
                    "payload" => Ok(StreamFrameField::Payload),
                    _ => Err(de::Error::unknown_field(
                        value,
                        &[
                            "subscription_id",
                            "stream",
                            "generation",
                            "sequence",
                            "payload_kind",
                            "schema_version",
                            "payload",
                        ],
                    )),
                }
            }
        }

        deserializer.deserialize_identifier(FieldVisitor)
    }
}

/// MessagePack binary payload wrapper — never an array of bytes.
struct BinaryPayloadRef<'a>(&'a [u8]);

impl Serialize for BinaryPayloadRef<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_bytes(self.0)
    }
}

struct BinaryPayload(Vec<u8>);

impl<'de> Deserialize<'de> for BinaryPayload {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct BinaryVisitor;

        impl<'de> Visitor<'de> for BinaryVisitor {
            type Value = BinaryPayload;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("MessagePack binary bytes")
            }

            fn visit_bytes<E>(self, value: &[u8]) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(BinaryPayload(value.to_vec()))
            }

            fn visit_byte_buf<E>(self, value: Vec<u8>) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(BinaryPayload(value))
            }

            fn visit_seq<A>(self, _seq: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                Err(de::Error::invalid_type(de::Unexpected::Seq, &self))
            }
        }

        deserializer.deserialize_bytes(BinaryVisitor)
    }
}

impl Serialize for StreamFrame {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(7))?;
        map.serialize_entry("subscription_id", &self.subscription_id)?;
        map.serialize_entry("stream", &self.stream)?;
        map.serialize_entry("generation", &self.generation)?;
        map.serialize_entry("sequence", &self.sequence)?;
        map.serialize_entry("payload_kind", &self.payload_kind)?;
        map.serialize_entry("schema_version", &self.schema_version)?;
        map.serialize_entry("payload", &BinaryPayloadRef(&self.payload))?;
        map.end()
    }
}

impl<'de> Deserialize<'de> for StreamFrame {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct StreamFrameVisitor;

        impl<'de> Visitor<'de> for StreamFrameVisitor {
            type Value = StreamFrame;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a seven-field named StreamFrame map")
            }

            fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                let mut subscription_id = None;
                let mut stream = None;
                let mut generation = None;
                let mut sequence = None;
                let mut payload_kind = None;
                let mut schema_version = None;
                let mut payload = None;
                while let Some(field) = map.next_key()? {
                    match field {
                        StreamFrameField::SubscriptionId => {
                            if subscription_id.is_some() {
                                return Err(de::Error::duplicate_field("subscription_id"));
                            }
                            subscription_id = Some(map.next_value()?);
                        }
                        StreamFrameField::Stream => {
                            if stream.is_some() {
                                return Err(de::Error::duplicate_field("stream"));
                            }
                            stream = Some(map.next_value()?);
                        }
                        StreamFrameField::Generation => {
                            if generation.is_some() {
                                return Err(de::Error::duplicate_field("generation"));
                            }
                            generation = Some(map.next_value()?);
                        }
                        StreamFrameField::Sequence => {
                            if sequence.is_some() {
                                return Err(de::Error::duplicate_field("sequence"));
                            }
                            sequence = Some(map.next_value()?);
                        }
                        StreamFrameField::PayloadKind => {
                            if payload_kind.is_some() {
                                return Err(de::Error::duplicate_field("payload_kind"));
                            }
                            payload_kind = Some(map.next_value()?);
                        }
                        StreamFrameField::SchemaVersion => {
                            if schema_version.is_some() {
                                return Err(de::Error::duplicate_field("schema_version"));
                            }
                            schema_version = Some(map.next_value()?);
                        }
                        StreamFrameField::Payload => {
                            if payload.is_some() {
                                return Err(de::Error::duplicate_field("payload"));
                            }
                            let BinaryPayload(bytes) = map.next_value()?;
                            payload = Some(bytes);
                        }
                    }
                }
                Ok(StreamFrame {
                    subscription_id: subscription_id
                        .ok_or_else(|| de::Error::missing_field("subscription_id"))?,
                    stream: stream.ok_or_else(|| de::Error::missing_field("stream"))?,
                    generation: generation.ok_or_else(|| de::Error::missing_field("generation"))?,
                    sequence: sequence.ok_or_else(|| de::Error::missing_field("sequence"))?,
                    payload_kind: payload_kind
                        .ok_or_else(|| de::Error::missing_field("payload_kind"))?,
                    schema_version: schema_version
                        .ok_or_else(|| de::Error::missing_field("schema_version"))?,
                    payload: payload.ok_or_else(|| de::Error::missing_field("payload"))?,
                })
            }
        }

        deserializer.deserialize_map(StreamFrameVisitor)
    }
}
