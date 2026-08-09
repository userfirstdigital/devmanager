//! Transport-neutral, typed Connect v1 envelope.
//!
//! The envelope owns connection identity, sequence identity, negotiated
//! bounds, and transport metadata. Payload meaning lives in `schema.rs`; the
//! envelope never exposes a serialized payload buffer as its semantic API.

use std::fmt;
use std::num::NonZeroU16;

use serde::de::{self, value::MapAccessDeserializer, Deserializer, MapAccess, Visitor};
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};
use uuid::{Uuid, Variant};

use crate::domain::id::{OperationId, RequestId, TransferId};
use crate::domain::snapshot::{MAX_SNAPSHOT_PAGE_ENCODED_BYTES, MAX_SNAPSHOT_PAGE_ITEMS};
use crate::protocol::{
    ChunkContext as ProtocolChunkContext, ChunkError, ChunkLimitField as ProtocolChunkLimitField,
    ChunkLimits, ChunkLimitsError, FrameLimits, MessagePackCodec, MessagePackError,
    ProtocolVersion, MAX_PHYSICAL_FRAME_BYTES, MAX_REASSEMBLED_MESSAGE_BYTES, PROTOCOL_MAJOR,
    PROTOCOL_MINOR,
};

use super::schema::{ConnectPayload, ConnectPayloadWire, KnownPayloadKind, PayloadError};

pub const CONNECT_PROTOCOL_MAJOR: u16 = PROTOCOL_MAJOR;
pub const CONNECT_PROTOCOL_MINOR: u16 = PROTOCOL_MINOR;
pub const MAX_CONNECT_PHYSICAL_FRAME_BYTES: u32 = MAX_PHYSICAL_FRAME_BYTES;
pub const MAX_CONNECT_REASSEMBLED_MESSAGE_BYTES: u32 = MAX_REASSEMBLED_MESSAGE_BYTES;
pub const MAX_CONNECT_PAGE_ITEMS: u32 = MAX_SNAPSHOT_PAGE_ITEMS;
pub const MAX_CONNECT_PAGE_ENCODED_BYTES: u32 = MAX_SNAPSHOT_PAGE_ENCODED_BYTES;
pub const MAX_CONNECT_CHUNK_BYTES: u32 = 256 * 1024;
pub const MAX_CONNECT_CUMULATIVE_BYTES: u64 = MAX_CONNECT_REASSEMBLED_MESSAGE_BYTES as u64;
pub const MAX_CONNECT_CURSOR_BYTES: u32 = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectIdError {
    InvalidVersion,
    InvalidVariant,
}

impl fmt::Display for ConnectIdError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidVersion => "Connect identifiers must be UUIDv7",
            Self::InvalidVariant => "Connect identifiers must use the RFC 4122/9562 variant",
        })
    }
}

impl std::error::Error for ConnectIdError {}

macro_rules! define_connect_id {
    ($name:ident) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(Uuid);

        impl $name {
            pub fn new() -> Self {
                Self::from_uuid(Uuid::now_v7()).expect("Uuid::now_v7 creates UUIDv7")
            }

            pub fn from_uuid(value: Uuid) -> Result<Self, ConnectIdError> {
                if value.get_version_num() != 7 {
                    return Err(ConnectIdError::InvalidVersion);
                }
                if value.get_variant() != Variant::RFC4122 {
                    return Err(ConnectIdError::InvalidVariant);
                }
                Ok(Self(value))
            }

            pub fn from_bytes(value: [u8; 16]) -> Result<Self, ConnectIdError> {
                Self::from_uuid(Uuid::from_bytes(value))
            }

            pub const fn as_uuid(self) -> Uuid {
                self.0
            }

            pub const fn as_bytes(self) -> [u8; 16] {
                self.0.into_bytes()
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl TryFrom<Uuid> for $name {
            type Error = ConnectIdError;

            fn try_from(value: Uuid) -> Result<Self, Self::Error> {
                Self::from_uuid(value)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                self.0.serialize(serializer)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = Uuid::deserialize(deserializer)?;
                Self::from_uuid(value).map_err(de::Error::custom)
            }
        }
    };
}

define_connect_id!(ConnectionId);
define_connect_id!(SessionId);
define_connect_id!(ChannelId);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChannelBinding {
    pub connection_id: ConnectionId,
    pub session_id: SessionId,
    pub channel_id: ChannelId,
}

impl ChannelBinding {
    pub const fn new(
        connection_id: ConnectionId,
        session_id: SessionId,
        channel_id: ChannelId,
    ) -> Self {
        Self {
            connection_id,
            session_id,
            channel_id,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectLimitField {
    PhysicalFrameBytes,
    ReassembledMessageBytes,
    PageItems,
    PageEncodedBytes,
    ChunkBytes,
    CumulativeBytes,
    CursorBytes,
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
            Self::CursorBytes => "max_cursor_bytes",
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
    CumulativeOverflow,
    CumulativeExceeded {
        declared: u64,
        maximum: u64,
    },
    CursorEmpty,
    CursorExceeded {
        declared: u64,
        maximum: u32,
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
            Self::CumulativeOverflow => {
                formatter.write_str("Connect cumulative byte count overflowed")
            }
            Self::CumulativeExceeded { declared, maximum } => write!(
                formatter,
                "Connect cumulative bytes {declared} exceeds {maximum}"
            ),
            Self::CursorEmpty => formatter.write_str("Connect cursors must be nonempty"),
            Self::CursorExceeded { declared, maximum } => write!(
                formatter,
                "Connect cursor bytes {declared} exceeds {maximum}"
            ),
            Self::PayloadExceeded { declared, maximum } => write!(
                formatter,
                "Connect payload bytes {declared} exceeds {maximum}"
            ),
        }
    }
}

impl std::error::Error for ConnectLimitError {}

/// The one negotiated bounds object used by Connect payloads and envelopes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectLimits {
    pub max_physical_frame_bytes: u32,
    pub max_reassembled_message_bytes: u32,
    pub max_page_items: u32,
    pub max_page_encoded_bytes: u32,
    pub max_chunk_bytes: u32,
    pub max_cumulative_bytes: u64,
    pub max_cursor_bytes: u32,
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
            max_cursor_bytes: MAX_CONNECT_CURSOR_BYTES,
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
        max_cursor_bytes: u32,
    ) -> Result<Self, ConnectLimitError> {
        let limits = Self {
            max_physical_frame_bytes,
            max_reassembled_message_bytes,
            max_page_items,
            max_page_encoded_bytes,
            max_chunk_bytes,
            max_cumulative_bytes,
            max_cursor_bytes,
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
            (
                ConnectLimitField::CursorBytes,
                u64::from(self.max_cursor_bytes),
                u64::from(MAX_CONNECT_CURSOR_BYTES),
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
            self.max_cursor_bytes.min(peer.max_cursor_bytes),
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
        self.canonical_chunk_limits()?
            .validate_chunk(cumulative_before, chunk)
            .map_err(map_protocol_chunk_error)
    }

    pub fn validate_cursor_len(self, length: usize) -> Result<(), ConnectLimitError> {
        self.canonical_chunk_limits()?
            .validate_cursor_len(length)
            .map_err(map_protocol_chunk_error)
    }

    /// Creates a chunk receiver from these negotiated Connect limits.
    ///
    /// The protocol context is intentionally wrapped so Connect callers cannot
    /// supply an independent `ChunkLimits` value.
    pub fn chunk_context(
        self,
        transfer_id: TransferId,
        resume_cursor: Option<Vec<u8>>,
    ) -> Result<ChunkContext, ConnectLimitError> {
        self.validate()?;
        let chunk_limits = self.canonical_chunk_limits()?;
        if let Some(cursor) = resume_cursor.as_deref() {
            self.validate_cursor_len(cursor.len())?;
        }
        ProtocolChunkContext::new(transfer_id, chunk_limits, resume_cursor)
            .map(ChunkContext)
            .map_err(map_protocol_chunk_error)
    }

    pub(crate) fn canonical_chunk_limits(self) -> Result<ChunkLimits, ConnectLimitError> {
        ChunkLimits::try_new(
            self.max_chunk_bytes,
            self.max_cumulative_bytes,
            self.max_cursor_bytes,
        )
        .map_err(map_protocol_chunk_limits_error)
    }
}

fn map_protocol_chunk_limits_error(error: ChunkLimitsError) -> ConnectLimitError {
    match error {
        ChunkLimitsError::Zero { field } => ConnectLimitError::Zero {
            field: match field {
                ProtocolChunkLimitField::ChunkBytes => ConnectLimitField::ChunkBytes,
                ProtocolChunkLimitField::CumulativeBytes => ConnectLimitField::CumulativeBytes,
                ProtocolChunkLimitField::CursorBytes => ConnectLimitField::CursorBytes,
            },
        },
        ChunkLimitsError::ExceedsHardMaximum {
            field,
            declared,
            maximum,
        } => ConnectLimitError::ExceedsHardMaximum {
            field: match field {
                ProtocolChunkLimitField::ChunkBytes => ConnectLimitField::ChunkBytes,
                ProtocolChunkLimitField::CumulativeBytes => ConnectLimitField::CumulativeBytes,
                ProtocolChunkLimitField::CursorBytes => ConnectLimitField::CursorBytes,
            },
            declared,
            maximum,
        },
        ChunkLimitsError::ChunkExceedsCumulative { chunk, cumulative } => {
            ConnectLimitError::CumulativeBelowChunk { cumulative, chunk }
        }
    }
}

fn map_protocol_chunk_error(error: ChunkError) -> ConnectLimitError {
    match error {
        ChunkError::Limits(error) => map_protocol_chunk_limits_error(error),
        ChunkError::EmptyPayload => ConnectLimitError::EmptyChunk,
        ChunkError::ChunkTooLarge { declared, maximum } => {
            ConnectLimitError::ChunkExceeded { declared, maximum }
        }
        ChunkError::CumulativeOverflow => ConnectLimitError::CumulativeOverflow,
        ChunkError::CumulativeTooLarge { declared, maximum } => {
            ConnectLimitError::CumulativeExceeded { declared, maximum }
        }
        ChunkError::CursorEmpty => ConnectLimitError::CursorEmpty,
        ChunkError::CursorTooLarge { declared, maximum } => {
            ConnectLimitError::CursorExceeded { declared, maximum }
        }
        ChunkError::TransferIdMismatch
        | ChunkError::IndexMismatch { .. }
        | ChunkError::ResumeCursorMismatch
        | ChunkError::CumulativeHashMismatch
        | ChunkError::AlreadyComplete
        | ChunkError::FinalRequired
        | ChunkError::Poisoned => ConnectLimitError::EmptyChunk,
    }
}

impl Default for ConnectLimits {
    fn default() -> Self {
        Self::v1_default()
    }
}

/// A Connect-owned chunk receiver whose limits are always negotiated from its
/// enclosing `ConnectLimits`.
#[derive(Debug)]
pub struct ChunkContext(ProtocolChunkContext);

impl ChunkContext {
    pub fn accept(&mut self, frame: &crate::protocol::ChunkFrame) -> Result<(), ChunkError> {
        self.0.accept(frame)
    }

    pub const fn is_complete(&self) -> bool {
        self.0.is_complete()
    }

    pub const fn is_poisoned(&self) -> bool {
        self.0.is_poisoned()
    }

    pub const fn next_index(&self) -> u32 {
        self.0.next_index()
    }

    pub const fn cumulative_bytes(&self) -> u64 {
        self.0.cumulative_bytes()
    }

    pub fn require_complete(&mut self) -> Result<(), ChunkError> {
        self.0.require_complete()
    }
}

#[derive(Serialize)]
struct ConnectLimitsWire {
    max_physical_frame_bytes: u32,
    max_reassembled_message_bytes: u32,
    max_page_items: u32,
    max_page_encoded_bytes: u32,
    max_chunk_bytes: u32,
    max_cumulative_bytes: u64,
    max_cursor_bytes: u32,
}

impl Serialize for ConnectLimits {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.validate().map_err(serde::ser::Error::custom)?;
        ConnectLimitsWire {
            max_physical_frame_bytes: self.max_physical_frame_bytes,
            max_reassembled_message_bytes: self.max_reassembled_message_bytes,
            max_page_items: self.max_page_items,
            max_page_encoded_bytes: self.max_page_encoded_bytes,
            max_chunk_bytes: self.max_chunk_bytes,
            max_cumulative_bytes: self.max_cumulative_bytes,
            max_cursor_bytes: self.max_cursor_bytes,
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
            max_cursor_bytes: u32,
        }
        let wire = Wire::deserialize(deserializer)?;
        Self::try_new(
            wire.max_physical_frame_bytes,
            wire.max_reassembled_message_bytes,
            wire.max_page_items,
            wire.max_page_encoded_bytes,
            wire.max_chunk_bytes,
            wire.max_cumulative_bytes,
            wire.max_cursor_bytes,
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

/// Nonzero payload discriminant. Unknown values are retained only by the
/// inert `ConnectPayload::Unknown` variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PayloadKind(NonZeroU16);

impl PayloadKind {
    pub const fn new(value: u16) -> Option<Self> {
        match NonZeroU16::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    pub const fn get(self) -> u16 {
        self.0.get()
    }

    pub fn known(self) -> Option<KnownPayloadKind> {
        super::schema::known_kind_for(self)
    }
}

impl fmt::Display for PayloadKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.get().fmt(formatter)
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

#[derive(Debug)]
pub enum EnvelopeError {
    InvalidVersion { major: u16, minor: u16 },
    InvalidSequence,
    CompressionUnsupported,
    Limits(ConnectLimitError),
    NegotiatedLimitsMismatch,
    Payload(PayloadError),
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
            Self::InvalidSequence => {
                formatter.write_str("Connect channel sequence must be nonzero")
            }
            Self::CompressionUnsupported => {
                formatter.write_str("Connect v1 supports only no compression")
            }
            Self::Limits(error) => error.fmt(formatter),
            Self::NegotiatedLimitsMismatch => {
                formatter.write_str("Connect envelope limits differ from negotiated limits")
            }
            Self::Payload(error) => error.fmt(formatter),
            Self::MessagePack(error) => error.fmt(formatter),
            Self::Encode => formatter.write_str("Connect envelope encoding failed"),
        }
    }
}

impl std::error::Error for EnvelopeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Limits(error) => Some(error),
            Self::Payload(error) => Some(error),
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

impl From<PayloadError> for EnvelopeError {
    fn from(error: PayloadError) -> Self {
        Self::Payload(error)
    }
}

impl From<MessagePackError> for EnvelopeError {
    fn from(error: MessagePackError) -> Self {
        Self::MessagePack(error)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectEnvelope {
    version: ProtocolVersion,
    binding: ChannelBinding,
    sequence: u64,
    request_id: Option<RequestId>,
    operation_id: Option<OperationId>,
    limits: ConnectLimits,
    compression: Compression,
    privacy_class: ConnectPrivacyClass,
    payload: ConnectPayload,
}

impl ConnectEnvelope {
    pub fn new(
        binding: ChannelBinding,
        sequence: u64,
        request_id: Option<RequestId>,
        operation_id: Option<OperationId>,
        limits: ConnectLimits,
        privacy_class: ConnectPrivacyClass,
        payload: ConnectPayload,
    ) -> Result<Self, EnvelopeError> {
        Self::new_with_version(
            ProtocolVersion::current(),
            binding,
            sequence,
            request_id,
            operation_id,
            limits,
            privacy_class,
            payload,
        )
    }

    pub fn new_with_version(
        version: ProtocolVersion,
        binding: ChannelBinding,
        sequence: u64,
        request_id: Option<RequestId>,
        operation_id: Option<OperationId>,
        limits: ConnectLimits,
        privacy_class: ConnectPrivacyClass,
        payload: ConnectPayload,
    ) -> Result<Self, EnvelopeError> {
        let payload = payload.canonicalized_for_wire()?;
        let envelope = Self {
            version,
            binding,
            sequence,
            request_id,
            operation_id,
            limits,
            compression: Compression::None,
            privacy_class,
            payload,
        };
        envelope.validate()?;
        Ok(envelope)
    }

    pub fn validate(&self) -> Result<(), EnvelopeError> {
        let negotiated = ProtocolVersion::current()
            .negotiate(self.version)
            .map_err(|_| EnvelopeError::InvalidVersion {
                major: self.version.major,
                minor: self.version.minor,
            })?;
        if negotiated != self.version {
            return Err(EnvelopeError::InvalidVersion {
                major: self.version.major,
                minor: self.version.minor,
            });
        }
        if self.sequence == 0 {
            return Err(EnvelopeError::InvalidSequence);
        }
        if !matches!(self.compression, Compression::None) {
            return Err(EnvelopeError::CompressionUnsupported);
        }
        self.limits.validate()?;
        self.payload.validate(self.limits)?;
        self.validate_correlations()?;
        Ok(())
    }

    fn validate_correlations(&self) -> Result<(), EnvelopeError> {
        match self.payload.as_request() {
            Some(crate::protocol::ClientRequest::Command(_)) => {
                if self.request_id.is_none() {
                    return Err(EnvelopeError::Payload(PayloadError::Correlation));
                }
            }
            Some(crate::protocol::ClientRequest::Query(query)) => {
                if self.request_id != Some(query.request_id) {
                    return Err(EnvelopeError::Payload(PayloadError::Correlation));
                }
            }
            Some(crate::protocol::ClientRequest::Detach(_)) | None => {}
        }

        match self.payload.as_message() {
            Some(crate::protocol::ServerMessage::QueryReply(reply)) => {
                if self.request_id != Some(reply.request_id) {
                    return Err(EnvelopeError::Payload(PayloadError::Correlation));
                }
            }
            Some(crate::protocol::ServerMessage::CommandReceipt(receipt)) => {
                match receipt.accepted_operation_id() {
                    Some(operation_id) if self.operation_id == Some(operation_id) => {}
                    Some(_) => return Err(EnvelopeError::Payload(PayloadError::Correlation)),
                    None if self.operation_id.is_none() => {}
                    None => return Err(EnvelopeError::Payload(PayloadError::Correlation)),
                }
            }
            _ => {}
        }

        if let Some(settlement) = self.payload.operation_settlement() {
            if self.operation_id != Some(settlement.operation_id()) {
                return Err(EnvelopeError::Payload(PayloadError::Correlation));
            }
        }
        Ok(())
    }

    pub fn encode(&self) -> Result<Vec<u8>, EnvelopeError> {
        let mut canonical = self.clone();
        canonical.payload = canonical.payload.canonicalized_for_wire()?;
        canonical.validate()?;
        let codec = MessagePackCodec::from_limits(canonical.limits.frame_limits())
            .map_err(|_| EnvelopeError::Encode)?;
        let encoded = codec
            .encode(&canonical.wire())
            .map_err(EnvelopeError::MessagePack)?;
        canonical.limits.validate_payload_len(encoded.len())?;
        Ok(encoded)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, EnvelopeError> {
        Self::decode_with_limits(bytes, ConnectLimits::v1_default())
    }

    pub fn decode_with_limits(
        bytes: &[u8],
        negotiated: ConnectLimits,
    ) -> Result<Self, EnvelopeError> {
        negotiated.validate()?;
        negotiated.validate_payload_len(bytes.len())?;
        let codec = MessagePackCodec::from_limits(negotiated.frame_limits())
            .map_err(|_| EnvelopeError::Encode)?;
        let wire = codec
            .decode::<ConnectEnvelopeWire>(bytes)
            .map_err(EnvelopeError::MessagePack)?;
        let envelope = Self::from_wire(wire)?;
        if envelope.limits != negotiated {
            return Err(EnvelopeError::NegotiatedLimitsMismatch);
        }
        Ok(envelope)
    }

    pub const fn binding(&self) -> ChannelBinding {
        self.binding
    }

    pub const fn protocol_version(&self) -> ProtocolVersion {
        self.version
    }

    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    pub const fn request_id(&self) -> Option<RequestId> {
        self.request_id
    }

    pub const fn operation_id(&self) -> Option<OperationId> {
        self.operation_id
    }

    pub const fn limits(&self) -> ConnectLimits {
        self.limits
    }

    pub const fn compression(&self) -> Compression {
        self.compression
    }

    pub const fn privacy_class(&self) -> ConnectPrivacyClass {
        self.privacy_class
    }

    pub fn channel(&self) -> ChannelKind {
        self.payload.channel()
    }

    pub fn payload_kind(&self) -> PayloadKind {
        self.payload.kind()
    }

    pub fn payload_version(&self) -> u16 {
        self.payload.version()
    }

    pub fn payload(&self) -> &ConnectPayload {
        &self.payload
    }

    pub fn known_payload_kind(&self) -> Option<KnownPayloadKind> {
        self.payload_kind().known()
    }

    pub fn is_action_payload(&self) -> bool {
        self.payload.is_action()
    }

    fn wire(&self) -> ConnectEnvelopeWire {
        ConnectEnvelopeWire {
            protocol_major: self.version.major,
            protocol_minor: self.version.minor,
            connection_id: self.binding.connection_id,
            session_id: self.binding.session_id,
            channel_id: self.binding.channel_id,
            channel: self.channel(),
            sequence: self.sequence,
            request_id: self.request_id,
            operation_id: self.operation_id,
            limits: self.limits,
            compression: self.compression,
            privacy_class: self.privacy_class,
            payload_kind: self.payload_kind(),
            payload_version: self.payload_version(),
            payload: ConnectPayloadWire::from(self.payload.clone()),
        }
    }

    fn from_wire(wire: ConnectEnvelopeWire) -> Result<Self, EnvelopeError> {
        if !matches!(wire.compression, Compression::None) {
            return Err(EnvelopeError::CompressionUnsupported);
        }
        let binding = ChannelBinding::new(wire.connection_id, wire.session_id, wire.channel_id);
        let envelope = Self {
            version: ProtocolVersion::new(wire.protocol_major, wire.protocol_minor),
            binding,
            sequence: wire.sequence,
            request_id: wire.request_id,
            operation_id: wire.operation_id,
            limits: wire.limits,
            compression: wire.compression,
            privacy_class: wire.privacy_class,
            payload: ConnectPayload::from(wire.payload),
        };
        if wire.channel != envelope.channel()
            || wire.payload_kind != envelope.payload_kind()
            || wire.payload_version != envelope.payload_version()
        {
            return Err(EnvelopeError::Payload(PayloadError::MetadataMismatch));
        }
        envelope.validate()?;
        Ok(envelope)
    }
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct ConnectEnvelopeWire {
    protocol_major: u16,
    protocol_minor: u16,
    connection_id: ConnectionId,
    session_id: SessionId,
    channel_id: ChannelId,
    channel: ChannelKind,
    sequence: u64,
    request_id: Option<RequestId>,
    operation_id: Option<OperationId>,
    limits: ConnectLimits,
    compression: Compression,
    privacy_class: ConnectPrivacyClass,
    payload_kind: PayloadKind,
    payload_version: u16,
    payload: ConnectPayloadWire,
}

impl<'de> Deserialize<'de> for ConnectEnvelopeWire {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ConnectEnvelopeWireVisitor;

        impl<'de> Visitor<'de> for ConnectEnvelopeWireVisitor {
            type Value = ConnectEnvelopeWire;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a named Connect envelope map")
            }

            fn visit_map<A>(self, map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct NamedConnectEnvelopeWire {
                    protocol_major: u16,
                    protocol_minor: u16,
                    connection_id: ConnectionId,
                    session_id: SessionId,
                    channel_id: ChannelId,
                    channel: ChannelKind,
                    sequence: u64,
                    request_id: Option<RequestId>,
                    operation_id: Option<OperationId>,
                    limits: ConnectLimits,
                    compression: Compression,
                    privacy_class: ConnectPrivacyClass,
                    payload_kind: PayloadKind,
                    payload_version: u16,
                    payload: ConnectPayloadWire,
                }

                let wire = NamedConnectEnvelopeWire::deserialize(MapAccessDeserializer::new(map))?;
                Ok(ConnectEnvelopeWire {
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
                })
            }

            fn visit_seq<A>(self, _sequence: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                Err(de::Error::custom(
                    "Connect envelope must use a named MessagePack map",
                ))
            }
        }

        deserializer.deserialize_map(ConnectEnvelopeWireVisitor)
    }
}

pub type NegotiatedLimits = ConnectLimits;
pub type PrivacyClass = ConnectPrivacyClass;
