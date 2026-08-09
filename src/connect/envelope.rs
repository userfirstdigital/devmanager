//! Transport-neutral, bounded Connect v1 inner-envelope contract.

use std::fmt;
use std::num::NonZeroU16;

use serde::de::{self, Deserializer};
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};
use uuid::{Uuid, Variant};

use crate::domain::id::{OperationId, RequestId};
use crate::domain::snapshot::{MAX_SNAPSHOT_PAGE_ENCODED_BYTES, MAX_SNAPSHOT_PAGE_ITEMS};
use crate::protocol::{
    FrameLimits, MessagePackCodec, MessagePackError, MAX_PHYSICAL_FRAME_BYTES,
    MAX_REASSEMBLED_MESSAGE_BYTES, PROTOCOL_MAJOR, PROTOCOL_MINOR,
};

pub const CONNECT_PROTOCOL_MAJOR: u16 = PROTOCOL_MAJOR;
pub const CONNECT_PROTOCOL_MINOR: u16 = PROTOCOL_MINOR;
pub const MAX_CONNECT_PHYSICAL_FRAME_BYTES: u32 = MAX_PHYSICAL_FRAME_BYTES;
pub const MAX_CONNECT_REASSEMBLED_MESSAGE_BYTES: u32 = MAX_REASSEMBLED_MESSAGE_BYTES;
pub const MAX_CONNECT_PAGE_ITEMS: u32 = MAX_SNAPSHOT_PAGE_ITEMS;
pub const MAX_CONNECT_PAGE_ENCODED_BYTES: u32 = MAX_SNAPSHOT_PAGE_ENCODED_BYTES;
pub const MAX_CONNECT_CHUNK_BYTES: u32 = 256 * 1024;
pub const MAX_CONNECT_CUMULATIVE_BYTES: u64 = MAX_CONNECT_REASSEMBLED_MESSAGE_BYTES as u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectLimitField {
    PhysicalFrameBytes,
    ReassembledMessageBytes,
    PageItems,
    PageEncodedBytes,
    ChunkBytes,
    CumulativeBytes,
}

impl fmt::Display for ConnectLimitField {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::PhysicalFrameBytes => "max_physical_frame_bytes",
            Self::ReassembledMessageBytes => "max_reassembled_message_bytes",
            Self::PageItems => "max_page_items",
            Self::PageEncodedBytes => "max_page_encoded_bytes",
            Self::ChunkBytes => "max_chunk_bytes",
            Self::CumulativeBytes => "max_cumulative_bytes",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectLimitError {
    Zero {
        field: ConnectLimitField,
    },
    ExceedsHardMaximum {
        field: ConnectLimitField,
        declared: u64,
        maximum: u64,
    },
    ChunkExceedsPhysicalFrame {
        chunk: u32,
        physical: u32,
    },
    ChunkExceedsMessage {
        chunk: u32,
        message: u32,
    },
    CumulativeBelowChunk {
        cumulative: u64,
        chunk: u32,
    },
    PageItemsExceeded {
        declared: usize,
        maximum: u32,
    },
    PageBytesExceeded {
        declared: u64,
        maximum: u32,
    },
    EmptyChunk,
    ChunkExceeded {
        declared: u64,
        maximum: u32,
    },
    CumulativeExceeded {
        declared: u64,
        maximum: u64,
    },
    PayloadExceeded {
        declared: u64,
        maximum: u32,
    },
}

impl fmt::Display for ConnectLimitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Zero { field } => write!(formatter, "Connect limit {field} must be nonzero"),
            Self::ExceedsHardMaximum {
                field,
                declared,
                maximum,
            } => write!(
                formatter,
                "Connect limit {field} value {declared} exceeds {maximum}"
            ),
            Self::ChunkExceedsPhysicalFrame { chunk, physical } => write!(
                formatter,
                "Connect chunk limit {chunk} exceeds physical frame limit {physical}"
            ),
            Self::ChunkExceedsMessage { chunk, message } => write!(
                formatter,
                "Connect chunk limit {chunk} exceeds message limit {message}"
            ),
            Self::CumulativeBelowChunk { cumulative, chunk } => write!(
                formatter,
                "Connect cumulative limit {cumulative} is below chunk limit {chunk}"
            ),
            Self::PageItemsExceeded { declared, maximum } => write!(
                formatter,
                "Connect page item count {declared} exceeds {maximum}"
            ),
            Self::PageBytesExceeded { declared, maximum } => {
                write!(formatter, "Connect page bytes {declared} exceeds {maximum}")
            }
            Self::EmptyChunk => formatter.write_str("Connect chunks must be nonempty"),
            Self::ChunkExceeded { declared, maximum } => write!(
                formatter,
                "Connect chunk bytes {declared} exceeds {maximum}"
            ),
            Self::CumulativeExceeded { declared, maximum } => write!(
                formatter,
                "Connect cumulative bytes {declared} exceeds {maximum}"
            ),
            Self::PayloadExceeded { declared, maximum } => write!(
                formatter,
                "Connect payload bytes {declared} exceeds {maximum}"
            ),
        }
    }
}

impl std::error::Error for ConnectLimitError {}

/// Limits carried by and negotiated for a Connect inner channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectLimits {
    pub max_physical_frame_bytes: u32,
    pub max_reassembled_message_bytes: u32,
    pub max_page_items: u32,
    pub max_page_encoded_bytes: u32,
    pub max_chunk_bytes: u32,
    pub max_cumulative_bytes: u64,
}

impl ConnectLimits {
    pub const fn v1_default() -> Self {
        Self {
            max_physical_frame_bytes: MAX_CONNECT_PHYSICAL_FRAME_BYTES,
            max_reassembled_message_bytes: MAX_CONNECT_REASSEMBLED_MESSAGE_BYTES,
            max_page_items: MAX_CONNECT_PAGE_ITEMS,
            max_page_encoded_bytes: MAX_CONNECT_PAGE_ENCODED_BYTES,
            max_chunk_bytes: MAX_CONNECT_CHUNK_BYTES,
            max_cumulative_bytes: MAX_CONNECT_CUMULATIVE_BYTES,
        }
    }

    pub const fn default_v1() -> Self {
        Self::v1_default()
    }

    pub fn try_new(
        max_physical_frame_bytes: u32,
        max_reassembled_message_bytes: u32,
        max_page_items: u32,
        max_page_encoded_bytes: u32,
        max_chunk_bytes: u32,
        max_cumulative_bytes: u64,
    ) -> Result<Self, ConnectLimitError> {
        let limits = Self {
            max_physical_frame_bytes,
            max_reassembled_message_bytes,
            max_page_items,
            max_page_encoded_bytes,
            max_chunk_bytes,
            max_cumulative_bytes,
        };
        limits.validate()?;
        Ok(limits)
    }

    pub fn validate(self) -> Result<(), ConnectLimitError> {
        let bounded = [
            (
                ConnectLimitField::PhysicalFrameBytes,
                u64::from(self.max_physical_frame_bytes),
                u64::from(MAX_CONNECT_PHYSICAL_FRAME_BYTES),
            ),
            (
                ConnectLimitField::ReassembledMessageBytes,
                u64::from(self.max_reassembled_message_bytes),
                u64::from(MAX_CONNECT_REASSEMBLED_MESSAGE_BYTES),
            ),
            (
                ConnectLimitField::PageItems,
                u64::from(self.max_page_items),
                u64::from(MAX_CONNECT_PAGE_ITEMS),
            ),
            (
                ConnectLimitField::PageEncodedBytes,
                u64::from(self.max_page_encoded_bytes),
                u64::from(MAX_CONNECT_PAGE_ENCODED_BYTES),
            ),
            (
                ConnectLimitField::ChunkBytes,
                u64::from(self.max_chunk_bytes),
                u64::from(MAX_CONNECT_CHUNK_BYTES),
            ),
            (
                ConnectLimitField::CumulativeBytes,
                self.max_cumulative_bytes,
                MAX_CONNECT_CUMULATIVE_BYTES,
            ),
        ];
        for (field, value, maximum) in bounded {
            if value == 0 {
                return Err(ConnectLimitError::Zero { field });
            }
            if value > maximum {
                return Err(ConnectLimitError::ExceedsHardMaximum {
                    field,
                    declared: value,
                    maximum,
                });
            }
        }
        if self.max_chunk_bytes > self.max_physical_frame_bytes {
            return Err(ConnectLimitError::ChunkExceedsPhysicalFrame {
                chunk: self.max_chunk_bytes,
                physical: self.max_physical_frame_bytes,
            });
        }
        if self.max_chunk_bytes > self.max_reassembled_message_bytes {
            return Err(ConnectLimitError::ChunkExceedsMessage {
                chunk: self.max_chunk_bytes,
                message: self.max_reassembled_message_bytes,
            });
        }
        if self.max_cumulative_bytes < u64::from(self.max_chunk_bytes) {
            return Err(ConnectLimitError::CumulativeBelowChunk {
                cumulative: self.max_cumulative_bytes,
                chunk: self.max_chunk_bytes,
            });
        }
        if self.max_cumulative_bytes > u64::from(self.max_reassembled_message_bytes) {
            return Err(ConnectLimitError::ExceedsHardMaximum {
                field: ConnectLimitField::CumulativeBytes,
                declared: self.max_cumulative_bytes,
                maximum: u64::from(self.max_reassembled_message_bytes),
            });
        }
        Ok(())
    }

    pub fn negotiate(self, peer: Self) -> Result<Self, ConnectLimitError> {
        self.validate()?;
        peer.validate()?;
        Self::try_new(
            self.max_physical_frame_bytes
                .min(peer.max_physical_frame_bytes),
            self.max_reassembled_message_bytes
                .min(peer.max_reassembled_message_bytes),
            self.max_page_items.min(peer.max_page_items),
            self.max_page_encoded_bytes.min(peer.max_page_encoded_bytes),
            self.max_chunk_bytes.min(peer.max_chunk_bytes),
            self.max_cumulative_bytes.min(peer.max_cumulative_bytes),
        )
    }

    pub const fn frame_limits(self) -> FrameLimits {
        FrameLimits {
            max_physical_frame_bytes: self.max_physical_frame_bytes,
            max_reassembled_message_bytes: self.max_reassembled_message_bytes,
            max_page_items: self.max_page_items,
            max_page_encoded_bytes: self.max_page_encoded_bytes,
        }
    }

    pub fn validate_payload_len(self, length: usize) -> Result<(), ConnectLimitError> {
        let declared = u64::try_from(length).unwrap_or(u64::MAX);
        if declared > u64::from(self.max_reassembled_message_bytes) {
            return Err(ConnectLimitError::PayloadExceeded {
                declared,
                maximum: self.max_reassembled_message_bytes,
            });
        }
        Ok(())
    }

    pub fn validate_page(self, items: usize, encoded_bytes: u64) -> Result<(), ConnectLimitError> {
        if items > usize::try_from(self.max_page_items).unwrap_or(usize::MAX) {
            return Err(ConnectLimitError::PageItemsExceeded {
                declared: items,
                maximum: self.max_page_items,
            });
        }
        if encoded_bytes > u64::from(self.max_page_encoded_bytes) {
            return Err(ConnectLimitError::PageBytesExceeded {
                declared: encoded_bytes,
                maximum: self.max_page_encoded_bytes,
            });
        }
        Ok(())
    }

    pub fn validate_chunk(
        self,
        cumulative_before: u64,
        chunk: &[u8],
    ) -> Result<u64, ConnectLimitError> {
        if chunk.is_empty() {
            return Err(ConnectLimitError::EmptyChunk);
        }
        let chunk_bytes = u64::try_from(chunk.len()).unwrap_or(u64::MAX);
        if chunk_bytes > u64::from(self.max_chunk_bytes) {
            return Err(ConnectLimitError::ChunkExceeded {
                declared: chunk_bytes,
                maximum: self.max_chunk_bytes,
            });
        }
        let cumulative = cumulative_before.saturating_add(chunk_bytes);
        if cumulative > self.max_cumulative_bytes {
            return Err(ConnectLimitError::CumulativeExceeded {
                declared: cumulative,
                maximum: self.max_cumulative_bytes,
            });
        }
        Ok(cumulative)
    }
}

impl Default for ConnectLimits {
    fn default() -> Self {
        Self::v1_default()
    }
}

impl Serialize for ConnectLimits {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.validate().map_err(serde::ser::Error::custom)?;
        #[derive(Serialize)]
        struct Wire {
            max_physical_frame_bytes: u32,
            max_reassembled_message_bytes: u32,
            max_page_items: u32,
            max_page_encoded_bytes: u32,
            max_chunk_bytes: u32,
            max_cumulative_bytes: u64,
        }
        Wire {
            max_physical_frame_bytes: self.max_physical_frame_bytes,
            max_reassembled_message_bytes: self.max_reassembled_message_bytes,
            max_page_items: self.max_page_items,
            max_page_encoded_bytes: self.max_page_encoded_bytes,
            max_chunk_bytes: self.max_chunk_bytes,
            max_cumulative_bytes: self.max_cumulative_bytes,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ConnectLimits {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            max_physical_frame_bytes: u32,
            max_reassembled_message_bytes: u32,
            max_page_items: u32,
            max_page_encoded_bytes: u32,
            max_chunk_bytes: u32,
            max_cumulative_bytes: u64,
        }
        let wire = Wire::deserialize(deserializer)?;
        Self::try_new(
            wire.max_physical_frame_bytes,
            wire.max_reassembled_message_bytes,
            wire.max_page_items,
            wire.max_page_encoded_bytes,
            wire.max_chunk_bytes,
            wire.max_cumulative_bytes,
        )
        .map_err(de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelKind {
    Critical,
    Durable,
    Ephemeral,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Compression {
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectPrivacyClass {
    LocalOnly,
    ManagedMetadata,
    RawContent,
}

/// Nonzero payload discriminant. Unknown values are retained as inert data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PayloadKind(NonZeroU16);

impl PayloadKind {
    pub const HELLO: Self = Self(NonZeroU16::new(1).unwrap());
    pub const CAPABILITIES: Self = Self(NonZeroU16::new(2).unwrap());
    pub const SNAPSHOT_PAGE: Self = Self(NonZeroU16::new(3).unwrap());
    pub const EVENT_PAGE: Self = Self(NonZeroU16::new(4).unwrap());
    pub const QUERY: Self = Self(NonZeroU16::new(5).unwrap());
    pub const COMMAND: Self = Self(NonZeroU16::new(6).unwrap());
    pub const COMMAND_RECEIPT: Self = Self(NonZeroU16::new(7).unwrap());
    pub const OPERATION_SETTLEMENT: Self = Self(NonZeroU16::new(8).unwrap());
    pub const PRESENCE: Self = Self(NonZeroU16::new(9).unwrap());
    pub const TERMINAL_DELTA: Self = Self(NonZeroU16::new(10).unwrap());
    pub const BROWSER_FRAME: Self = Self(NonZeroU16::new(11).unwrap());
    pub const PROMPT_EXTENSION: Self = Self(NonZeroU16::new(12).unwrap());
    pub const BROWSER_EXTENSION: Self = Self(NonZeroU16::new(13).unwrap());
    pub const CHUNK: Self = Self(NonZeroU16::new(14).unwrap());
    pub const RESYNC: Self = Self(NonZeroU16::new(15).unwrap());
    pub const ERROR: Self = Self(NonZeroU16::new(16).unwrap());
    pub const EXTENSION: Self = Self(NonZeroU16::new(17).unwrap());

    pub const fn new(value: u16) -> Option<Self> {
        match NonZeroU16::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    pub const fn get(self) -> u16 {
        self.0.get()
    }

    pub const fn known(self) -> Option<KnownPayloadKind> {
        Some(match self.get() {
            1 => KnownPayloadKind::Hello,
            2 => KnownPayloadKind::Capabilities,
            3 => KnownPayloadKind::SnapshotPage,
            4 => KnownPayloadKind::EventPage,
            5 => KnownPayloadKind::Query,
            6 => KnownPayloadKind::Command,
            7 => KnownPayloadKind::CommandReceipt,
            8 => KnownPayloadKind::OperationSettlement,
            9 => KnownPayloadKind::Presence,
            10 => KnownPayloadKind::TerminalDelta,
            11 => KnownPayloadKind::BrowserFrame,
            12 => KnownPayloadKind::PromptExtension,
            13 => KnownPayloadKind::BrowserExtension,
            14 => KnownPayloadKind::Chunk,
            15 => KnownPayloadKind::Resync,
            16 => KnownPayloadKind::Error,
            17 => KnownPayloadKind::Extension,
            _ => return None,
        })
    }
}

impl Serialize for PayloadKind {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.get().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for PayloadKind {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = u16::deserialize(deserializer)?;
        Self::new(value).ok_or_else(|| de::Error::custom("payload kind must be nonzero"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KnownPayloadKind {
    Hello,
    Capabilities,
    SnapshotPage,
    EventPage,
    Query,
    Command,
    CommandReceipt,
    OperationSettlement,
    Presence,
    TerminalDelta,
    BrowserFrame,
    PromptExtension,
    BrowserExtension,
    Chunk,
    Resync,
    Error,
    Extension,
}

impl KnownPayloadKind {
    pub const fn is_action(self) -> bool {
        matches!(self, Self::Command)
    }
}

#[derive(Debug)]
pub enum EnvelopeError {
    InvalidVersion { major: u16, minor: u16 },
    InvalidUuid { field: &'static str },
    InvalidSequence,
    InvalidPayloadVersion,
    CompressionUnsupported,
    Limits(ConnectLimitError),
    NegotiatedLimitsMismatch,
    MessagePack(MessagePackError),
    Encode,
}

impl fmt::Display for EnvelopeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidVersion { major, minor } => {
                write!(
                    formatter,
                    "unsupported Connect protocol version {major}.{minor}"
                )
            }
            Self::InvalidUuid { field } => write!(formatter, "Connect {field} must be UUIDv7"),
            Self::InvalidSequence => {
                formatter.write_str("Connect channel sequence must be nonzero")
            }
            Self::InvalidPayloadVersion => {
                formatter.write_str("Connect payload version must be nonzero")
            }
            Self::CompressionUnsupported => {
                formatter.write_str("Connect v1 supports only no compression")
            }
            Self::Limits(error) => error.fmt(formatter),
            Self::NegotiatedLimitsMismatch => {
                formatter.write_str("Connect envelope limits differ from negotiated limits")
            }
            Self::MessagePack(error) => error.fmt(formatter),
            Self::Encode => formatter.write_str("Connect envelope encoding failed"),
        }
    }
}

impl std::error::Error for EnvelopeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Limits(error) => Some(error),
            Self::MessagePack(error) => Some(error),
            _ => None,
        }
    }
}

impl From<ConnectLimitError> for EnvelopeError {
    fn from(error: ConnectLimitError) -> Self {
        Self::Limits(error)
    }
}

impl From<MessagePackError> for EnvelopeError {
    fn from(error: MessagePackError) -> Self {
        Self::MessagePack(error)
    }
}

/// The one inner envelope used by direct and relay routes alike.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectEnvelope {
    pub protocol_major: u16,
    pub protocol_minor: u16,
    pub connection_id: Uuid,
    pub session_id: Uuid,
    pub channel_id: Uuid,
    pub channel: ChannelKind,
    pub sequence: u64,
    pub request_id: Option<RequestId>,
    pub operation_id: Option<OperationId>,
    pub limits: ConnectLimits,
    pub compression: Compression,
    pub privacy_class: ConnectPrivacyClass,
    pub payload_kind: PayloadKind,
    pub payload_version: u16,
    pub payload: Vec<u8>,
}

impl ConnectEnvelope {
    pub fn validate(&self) -> Result<(), EnvelopeError> {
        if self.protocol_major != CONNECT_PROTOCOL_MAJOR
            || self.protocol_minor > CONNECT_PROTOCOL_MINOR
        {
            return Err(EnvelopeError::InvalidVersion {
                major: self.protocol_major,
                minor: self.protocol_minor,
            });
        }
        validate_uuid(self.connection_id, "connection_id")?;
        validate_uuid(self.session_id, "session_id")?;
        validate_uuid(self.channel_id, "channel_id")?;
        if self.sequence == 0 {
            return Err(EnvelopeError::InvalidSequence);
        }
        if self.payload_version == 0 {
            return Err(EnvelopeError::InvalidPayloadVersion);
        }
        if !matches!(self.compression, Compression::None) {
            return Err(EnvelopeError::CompressionUnsupported);
        }
        self.limits.validate()?;
        self.limits.validate_payload_len(self.payload.len())?;
        Ok(())
    }

    pub fn encode(&self) -> Result<Vec<u8>, EnvelopeError> {
        self.validate()?;
        let codec = MessagePackCodec::from_limits(self.limits.frame_limits())
            .map_err(|_| EnvelopeError::Encode)?;
        codec.encode(self).map_err(EnvelopeError::MessagePack)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, EnvelopeError> {
        let codec = MessagePackCodec::from_limits(ConnectLimits::v1_default().frame_limits())
            .map_err(|_| EnvelopeError::Encode)?;
        codec.decode(bytes).map_err(EnvelopeError::MessagePack)
    }

    pub fn decode_with_limits(
        bytes: &[u8],
        negotiated: ConnectLimits,
    ) -> Result<Self, EnvelopeError> {
        negotiated.validate()?;
        let codec = MessagePackCodec::from_limits(negotiated.frame_limits())
            .map_err(|_| EnvelopeError::Encode)?;
        let envelope = codec
            .decode::<Self>(bytes)
            .map_err(EnvelopeError::MessagePack)?;
        if envelope.limits != negotiated {
            return Err(EnvelopeError::NegotiatedLimitsMismatch);
        }
        Ok(envelope)
    }

    pub const fn known_payload_kind(&self) -> Option<KnownPayloadKind> {
        self.payload_kind.known()
    }

    pub const fn is_action_payload(&self) -> bool {
        match self.known_payload_kind() {
            Some(kind) => kind.is_action(),
            None => false,
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConnectEnvelopeWire {
    protocol_major: u16,
    protocol_minor: u16,
    connection_id: Uuid,
    session_id: Uuid,
    channel_id: Uuid,
    channel: ChannelKind,
    sequence: u64,
    request_id: Option<RequestId>,
    operation_id: Option<OperationId>,
    limits: ConnectLimits,
    compression: Compression,
    privacy_class: ConnectPrivacyClass,
    payload_kind: PayloadKind,
    payload_version: u16,
    #[serde(with = "binary_payload")]
    payload: Vec<u8>,
}

impl Serialize for ConnectEnvelope {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.validate().map_err(serde::ser::Error::custom)?;
        ConnectEnvelopeWire {
            protocol_major: self.protocol_major,
            protocol_minor: self.protocol_minor,
            connection_id: self.connection_id,
            session_id: self.session_id,
            channel_id: self.channel_id,
            channel: self.channel,
            sequence: self.sequence,
            request_id: self.request_id,
            operation_id: self.operation_id,
            limits: self.limits,
            compression: self.compression,
            privacy_class: self.privacy_class,
            payload_kind: self.payload_kind,
            payload_version: self.payload_version,
            payload: self.payload.clone(),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ConnectEnvelope {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ConnectEnvelopeWire::deserialize(deserializer)?;
        let envelope = Self {
            protocol_major: wire.protocol_major,
            protocol_minor: wire.protocol_minor,
            connection_id: wire.connection_id,
            session_id: wire.session_id,
            channel_id: wire.channel_id,
            channel: wire.channel,
            sequence: wire.sequence,
            request_id: wire.request_id,
            operation_id: wire.operation_id,
            limits: wire.limits,
            compression: wire.compression,
            privacy_class: wire.privacy_class,
            payload_kind: wire.payload_kind,
            payload_version: wire.payload_version,
            payload: wire.payload,
        };
        envelope.validate().map_err(de::Error::custom)?;
        Ok(envelope)
    }
}

fn validate_uuid(value: Uuid, field: &'static str) -> Result<(), EnvelopeError> {
    if value.get_version_num() != 7 || value.get_variant() != Variant::RFC4122 {
        return Err(EnvelopeError::InvalidUuid { field });
    }
    Ok(())
}

mod binary_payload {
    use serde::de::{self, Deserializer, SeqAccess, Visitor};
    use serde::ser::Serializer;
    use std::fmt;

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

pub type ConnectionId = Uuid;
pub type SessionId = Uuid;
pub type ChannelId = Uuid;
pub type NegotiatedLimits = ConnectLimits;
pub type PrivacyClass = ConnectPrivacyClass;
