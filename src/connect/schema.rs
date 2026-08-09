//! Canonical Connect v1 typed payload catalog.
//!
//! This is the only place where a Connect payload discriminant is mapped to a
//! semantic Rust type. Existing Phase 1 request, message, page, stream, event,
//! and operation types remain the meaning-bearing types; the wrappers below
//! only add Connect-specific framing where Phase 1 has no equivalent.

use std::fmt;

use serde::de::{self, Deserializer, MapAccess, Visitor};
use serde::ser::{SerializeMap, Serializer};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::domain::id::{OperationId, TransferId};
use crate::domain::operation::{OperationOutcome, OperationOutcomeKind, OutcomeFenceError};
use crate::domain::query::{Query, QueryEnvelope, QueryReply};
use crate::domain::snapshot::{EventPage, SnapshotPage};
use crate::protocol::{
    CapabilitySet, ClientHello, ClientRequest, MessagePackCodec, MessagePackError, ServerHello,
    ServerMessage, StreamFrame,
};

use super::envelope::{ChannelKind, ConnectLimitError, ConnectLimits, PayloadKind};
use super::presence::LastSenderHint;
use super::transport::{BrowserExtensionDescriptor, PromptExtensionDescriptor};

pub const CONNECT_PAYLOAD_SCHEMA_VERSION: u16 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PayloadDescriptor {
    pub kind: PayloadKind,
    pub known: KnownPayloadKind,
    pub name: &'static str,
    pub channel: ChannelKind,
    pub version: u16,
    pub action: bool,
    pub max_payload_bytes: u32,
}

macro_rules! define_payload_catalog {
    ($(($constant:ident, $known:ident, $tag:literal, $name:literal, $channel:ident, $action:literal)),+ $(,)?) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum KnownPayloadKind {
            $($known,)+
        }

        impl PayloadKind {
            $(pub const $constant: Self = Self::new($tag).unwrap();)+
        }

        /// The reviewed v1 catalog is the one source for Connect tags, names,
        /// channel placement, payload versions, action status, and size bounds.
        pub const PAYLOAD_CATALOG: &[PayloadDescriptor] = &[
            $(PayloadDescriptor {
                kind: PayloadKind::$constant,
                known: KnownPayloadKind::$known,
                name: $name,
                channel: ChannelKind::$channel,
                version: CONNECT_PAYLOAD_SCHEMA_VERSION,
                action: $action,
                max_payload_bytes: super::envelope::MAX_CONNECT_REASSEMBLED_MESSAGE_BYTES,
            },)+
        ];
    };
}

define_payload_catalog!(
    (HELLO, Hello, 1, "hello", Critical, false),
    (
        CAPABILITIES,
        Capabilities,
        2,
        "capabilities",
        Critical,
        false
    ),
    (
        SNAPSHOT_PAGE,
        SnapshotPage,
        3,
        "snapshot_page",
        Durable,
        false
    ),
    (EVENT_PAGE, EventPage, 4, "event_page", Durable, false),
    (QUERY, Query, 5, "query", Critical, false),
    (COMMAND, Command, 6, "command", Critical, true),
    (
        COMMAND_RECEIPT,
        CommandReceipt,
        7,
        "command_receipt",
        Critical,
        false
    ),
    (
        OPERATION_SETTLEMENT,
        OperationSettlement,
        8,
        "operation_settlement",
        Critical,
        false
    ),
    (PRESENCE, Presence, 9, "presence", Ephemeral, false),
    (
        TERMINAL_DELTA,
        TerminalDelta,
        10,
        "terminal_delta",
        Ephemeral,
        false
    ),
    (
        BROWSER_FRAME,
        BrowserFrame,
        11,
        "browser_frame",
        Ephemeral,
        false
    ),
    (
        PROMPT_EXTENSION,
        PromptExtension,
        12,
        "prompt_extension",
        Durable,
        false
    ),
    (
        BROWSER_EXTENSION,
        BrowserExtension,
        13,
        "browser_extension",
        Durable,
        false
    ),
    (CHUNK, Chunk, 14, "chunk", Durable, false),
    (RESYNC, Resync, 15, "resync", Critical, false),
    (ERROR, Error, 16, "error", Critical, false),
    (EXTENSION, Extension, 17, "extension", Durable, false),
    (DETACH, Detach, 18, "detach", Critical, false),
    (
        DURABLE_EVENT,
        DurableEvent,
        19,
        "durable_event",
        Durable,
        false
    ),
    (STREAM, Stream, 20, "stream", Ephemeral, false),
    (DETACHED, Detached, 21, "detached", Critical, false),
    (QUERY_REPLY, QueryReply, 22, "query_reply", Critical, false),
);

pub fn payload_catalog() -> &'static [PayloadDescriptor] {
    PAYLOAD_CATALOG
}

pub(crate) fn descriptor_for(kind: PayloadKind) -> Option<&'static PayloadDescriptor> {
    PAYLOAD_CATALOG
        .iter()
        .find(|descriptor| descriptor.kind == kind)
}

pub(crate) fn known_kind_for(kind: PayloadKind) -> Option<KnownPayloadKind> {
    descriptor_for(kind).map(|descriptor| descriptor.known)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HelloPayload {
    Client(ClientHello),
    Server(ServerHello),
}

impl Serialize for HelloPayload {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(1))?;
        match self {
            Self::Client(value) => map.serialize_entry("client", value)?,
            Self::Server(value) => map.serialize_entry("server", value)?,
        }
        map.end()
    }
}

enum HelloTag {
    Client,
    Server,
}

impl<'de> Deserialize<'de> for HelloTag {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct TagVisitor;

        impl Visitor<'_> for TagVisitor {
            type Value = HelloTag;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("client or server hello tag")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                match value {
                    "client" => Ok(HelloTag::Client),
                    "server" => Ok(HelloTag::Server),
                    _ => Err(de::Error::unknown_variant(value, &["client", "server"])),
                }
            }
        }

        deserializer.deserialize_identifier(TagVisitor)
    }
}

impl<'de> Deserialize<'de> for HelloPayload {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct HelloVisitor;

        impl<'de> Visitor<'de> for HelloVisitor {
            type Value = HelloPayload;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a one-entry named hello map")
            }

            fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                let tag = map
                    .next_key::<HelloTag>()?
                    .ok_or_else(|| de::Error::custom("hello tag is missing"))?;
                let value = match tag {
                    HelloTag::Client => HelloPayload::Client(map.next_value()?),
                    HelloTag::Server => HelloPayload::Server(map.next_value()?),
                };
                if map.next_key::<de::IgnoredAny>()?.is_some() {
                    return Err(de::Error::custom(
                        "hello map must contain exactly one entry",
                    ));
                }
                Ok(value)
            }
        }

        deserializer.deserialize_map(HelloVisitor)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationSettlementPayload {
    pub operation_id: OperationId,
    pub outcome: OperationOutcome,
}

impl OperationSettlementPayload {
    pub fn new(operation_id: OperationId, outcome: OperationOutcome) -> Result<Self, PayloadError> {
        let payload = Self {
            operation_id,
            outcome,
        };
        payload.validate()?;
        Ok(payload)
    }

    pub fn operation_id(&self) -> OperationId {
        self.operation_id
    }

    pub fn validate(&self) -> Result<(), PayloadError> {
        if self.operation_id != self.outcome.operation_id {
            return Err(PayloadError::Correlation);
        }
        self.outcome
            .validate()
            .map_err(|_| PayloadError::Settlement)?;
        if matches!(
            &self.outcome.kind,
            OperationOutcomeKind::Settled {
                ref result_event_ids
            } if result_event_ids.is_empty()
        ) {
            return Err(PayloadError::Settlement);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkFrame {
    pub transfer_id: TransferId,
    pub index: u32,
    pub count: u32,
    pub cumulative_bytes: u64,
    pub digest: [u8; 32],
    pub payload: Vec<u8>,
    pub cursor: Option<Vec<u8>>,
}

fn digest(payload: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(payload);
    hasher.finalize().into()
}

impl ChunkFrame {
    pub fn new(
        transfer_id: TransferId,
        index: u32,
        count: u32,
        payload: Vec<u8>,
        cursor: Option<Vec<u8>>,
    ) -> Result<Self, PayloadError> {
        let cumulative_bytes = u64::try_from(payload.len()).map_err(|_| PayloadError::Bounds)?;
        let digest = digest(&payload);
        let frame = Self {
            transfer_id,
            index,
            count,
            cumulative_bytes,
            digest,
            payload,
            cursor,
        };
        frame.validate_shape()?;
        Ok(frame)
    }

    fn validate_shape(&self) -> Result<(), PayloadError> {
        if self.count == 0 || self.index >= self.count || self.payload.is_empty() {
            return Err(PayloadError::Bounds);
        }
        let payload_len = u64::try_from(self.payload.len()).map_err(|_| PayloadError::Bounds)?;
        if self.cumulative_bytes < payload_len
            || self.cumulative_bytes - payload_len > super::envelope::MAX_CONNECT_CUMULATIVE_BYTES
        {
            return Err(PayloadError::Bounds);
        }
        if self.digest != digest(&self.payload) {
            return Err(PayloadError::DigestMismatch);
        }
        Ok(())
    }

    pub fn validate(&self, limits: ConnectLimits) -> Result<(), PayloadError> {
        self.validate_shape()?;
        let payload_len = u64::try_from(self.payload.len()).map_err(|_| PayloadError::Bounds)?;
        let cumulative_before = self
            .cumulative_bytes
            .checked_sub(payload_len)
            .ok_or(PayloadError::Bounds)?;
        limits
            .validate_chunk(cumulative_before, &self.payload)
            .map_err(PayloadError::Limits)?;
        if let Some(cursor) = self.cursor.as_deref() {
            limits
                .validate_cursor_len(cursor.len())
                .map_err(PayloadError::Limits)?;
        }
        Ok(())
    }
}

impl Serialize for ChunkFrame {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(7))?;
        map.serialize_entry("transfer_id", &self.transfer_id)?;
        map.serialize_entry("index", &self.index)?;
        map.serialize_entry("count", &self.count)?;
        map.serialize_entry("cumulative_bytes", &self.cumulative_bytes)?;
        map.serialize_entry("digest", &self.digest)?;
        map.serialize_entry("payload", &BinaryRef(&self.payload))?;
        map.serialize_entry("cursor", &OptionalBinaryRef(self.cursor.as_deref()))?;
        map.end()
    }
}

impl<'de> Deserialize<'de> for ChunkFrame {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            transfer_id: TransferId,
            index: u32,
            count: u32,
            cumulative_bytes: u64,
            digest: [u8; 32],
            #[serde(with = "binary")]
            payload: Vec<u8>,
            #[serde(with = "optional_binary")]
            cursor: Option<Vec<u8>>,
        }
        let wire = Wire::deserialize(deserializer)?;
        let frame = Self {
            transfer_id: wire.transfer_id,
            index: wire.index,
            count: wire.count,
            cumulative_bytes: wire.cumulative_bytes,
            digest: wire.digest,
            payload: wire.payload,
            cursor: wire.cursor,
        };
        frame.validate_shape().map_err(de::Error::custom)?;
        Ok(frame)
    }
}

/// Context needed to validate cross-frame chunk identity and cumulative
/// cursor continuity. It is deliberately not serialized into a frame.
#[derive(Debug)]
pub struct ChunkContext {
    transfer_id: TransferId,
    count: u32,
    next_index: u32,
    cumulative_bytes: u64,
    cursor: Option<Vec<u8>>,
    cumulative_hasher: Sha256,
}

impl ChunkContext {
    pub fn new(
        transfer_id: TransferId,
        count: u32,
        cursor: Option<Vec<u8>>,
    ) -> Result<Self, PayloadError> {
        if count == 0 {
            return Err(PayloadError::Bounds);
        }
        Ok(Self {
            transfer_id,
            count,
            next_index: 0,
            cumulative_bytes: 0,
            cursor,
            cumulative_hasher: Sha256::new(),
        })
    }

    pub fn accept(&mut self, frame: &ChunkFrame) -> Result<(), PayloadError> {
        frame.validate_shape()?;
        if frame.transfer_id != self.transfer_id
            || frame.count != self.count
            || frame.index != self.next_index
            || frame.cursor != self.cursor
        {
            return Err(PayloadError::CursorContext);
        }
        let payload_len = u64::try_from(frame.payload.len()).map_err(|_| PayloadError::Bounds)?;
        let expected_cumulative = self
            .cumulative_bytes
            .checked_add(payload_len)
            .ok_or(PayloadError::Bounds)?;
        if frame.cumulative_bytes != expected_cumulative {
            return Err(PayloadError::CursorContext);
        }
        self.cumulative_hasher.update(&frame.payload);
        self.cumulative_bytes = expected_cumulative;
        self.next_index = self.next_index.checked_add(1).ok_or(PayloadError::Bounds)?;
        Ok(())
    }

    pub const fn is_complete(&self) -> bool {
        self.next_index == self.count
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ErrorPayload {
    pub code: u16,
    pub message: String,
}

impl ErrorPayload {
    pub fn new(code: u16, message: impl Into<String>) -> Result<Self, PayloadError> {
        let payload = Self {
            code,
            message: message.into(),
        };
        payload.validate(ConnectLimits::v1_default())?;
        Ok(payload)
    }

    fn validate(&self, limits: ConnectLimits) -> Result<(), PayloadError> {
        if self.code == 0
            || self.message.is_empty()
            || self.message.len() > usize::try_from(limits.max_reassembled_message_bytes).unwrap()
        {
            return Err(PayloadError::Bounds);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GenericExtensionPayload {
    pub type_id: u16,
    pub schema_version: u16,
    #[serde(with = "binary")]
    pub payload: Vec<u8>,
}

impl GenericExtensionPayload {
    pub fn new(type_id: u16, schema_version: u16, payload: Vec<u8>) -> Result<Self, PayloadError> {
        let extension = Self {
            type_id,
            schema_version,
            payload,
        };
        if extension.type_id == 0 || extension.schema_version == 0 {
            return Err(PayloadError::Bounds);
        }
        Ok(extension)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnknownPayload {
    pub kind: PayloadKind,
    pub version: u16,
    #[serde(with = "binary")]
    pub payload: Vec<u8>,
}

impl UnknownPayload {
    pub fn new(kind: PayloadKind, version: u16, payload: Vec<u8>) -> Result<Self, PayloadError> {
        if kind.known().is_some() || version == 0 {
            return Err(PayloadError::Bounds);
        }
        Ok(Self {
            kind,
            version,
            payload,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectPayload {
    Hello(HelloPayload),
    Capabilities(CapabilitySet),
    SnapshotPage(SnapshotPage),
    EventPage(EventPage),
    Request(ClientRequest),
    Message(ServerMessage),
    OperationSettlement(OperationSettlementPayload),
    Presence(LastSenderHint),
    TerminalDelta(StreamFrame),
    BrowserFrame(StreamFrame),
    PromptExtension(PromptExtensionDescriptor),
    BrowserExtension(BrowserExtensionDescriptor),
    Chunk(ChunkFrame),
    Error(ErrorPayload),
    Extension(GenericExtensionPayload),
    Unknown(UnknownPayload),
}

impl ConnectPayload {
    pub fn kind(&self) -> PayloadKind {
        match self {
            Self::Hello(_) => PayloadKind::HELLO,
            Self::Capabilities(_) => PayloadKind::CAPABILITIES,
            Self::SnapshotPage(_) => PayloadKind::SNAPSHOT_PAGE,
            Self::EventPage(_) => PayloadKind::EVENT_PAGE,
            Self::Request(ClientRequest::Command(_)) => PayloadKind::COMMAND,
            Self::Request(ClientRequest::Query(_)) => PayloadKind::QUERY,
            Self::Request(ClientRequest::Detach(_)) => PayloadKind::DETACH,
            Self::Message(ServerMessage::CommandReceipt(_)) => PayloadKind::COMMAND_RECEIPT,
            Self::Message(ServerMessage::QueryReply(_)) => PayloadKind::QUERY_REPLY,
            Self::Message(ServerMessage::DurableEvent { .. }) => PayloadKind::DURABLE_EVENT,
            Self::Message(ServerMessage::ResyncRequired { .. }) => PayloadKind::RESYNC,
            Self::Message(ServerMessage::Stream(_)) => PayloadKind::STREAM,
            Self::Message(ServerMessage::Detached(_)) => PayloadKind::DETACHED,
            Self::OperationSettlement(_) => PayloadKind::OPERATION_SETTLEMENT,
            Self::Presence(_) => PayloadKind::PRESENCE,
            Self::TerminalDelta(_) => PayloadKind::TERMINAL_DELTA,
            Self::BrowserFrame(_) => PayloadKind::BROWSER_FRAME,
            Self::PromptExtension(_) => PayloadKind::PROMPT_EXTENSION,
            Self::BrowserExtension(_) => PayloadKind::BROWSER_EXTENSION,
            Self::Chunk(_) => PayloadKind::CHUNK,
            Self::Error(_) => PayloadKind::ERROR,
            Self::Extension(_) => PayloadKind::EXTENSION,
            Self::Unknown(value) => value.kind,
        }
    }

    pub fn channel(&self) -> ChannelKind {
        descriptor_for(self.kind())
            .map(|descriptor| descriptor.channel)
            .unwrap_or(ChannelKind::Durable)
    }

    pub fn version(&self) -> u16 {
        match self {
            Self::Unknown(value) => value.version,
            _ => CONNECT_PAYLOAD_SCHEMA_VERSION,
        }
    }

    pub fn is_action(&self) -> bool {
        descriptor_for(self.kind()).is_some_and(|descriptor| descriptor.action)
    }

    pub fn as_request(&self) -> Option<&ClientRequest> {
        match self {
            Self::Request(request) => Some(request),
            _ => None,
        }
    }

    pub fn as_message(&self) -> Option<&ServerMessage> {
        match self {
            Self::Message(message) => Some(message),
            _ => None,
        }
    }

    pub fn operation_settlement(&self) -> Option<&OperationSettlementPayload> {
        match self {
            Self::OperationSettlement(value) => Some(value),
            _ => None,
        }
    }

    pub fn encode(&self, limits: ConnectLimits) -> Result<Vec<u8>, PayloadError> {
        limits.validate()?;
        self.validate(limits)?;
        let codec = MessagePackCodec::from_limits(limits.frame_limits())
            .map_err(|_| PayloadError::Encode)?;
        codec.encode(self).map_err(PayloadError::MessagePack)
    }

    pub fn decode(
        kind: PayloadKind,
        version: u16,
        bytes: &[u8],
        limits: ConnectLimits,
    ) -> Result<Self, PayloadError> {
        limits.validate()?;
        limits.validate_payload_len(bytes.len())?;
        if descriptor_for(kind).is_some() && version != CONNECT_PAYLOAD_SCHEMA_VERSION {
            return Err(PayloadError::UnsupportedVersion { kind, version });
        }
        if descriptor_for(kind).is_none() && version == 0 {
            return Err(PayloadError::UnsupportedVersion { kind, version });
        }
        let codec = MessagePackCodec::from_limits(limits.frame_limits())
            .map_err(|_| PayloadError::Encode)?;
        let payload = codec
            .decode::<Self>(bytes)
            .map_err(PayloadError::MessagePack)?;
        if payload.kind() != kind || payload.version() != version {
            return Err(PayloadError::MetadataMismatch);
        }
        payload.validate(limits)?;
        Ok(payload)
    }

    pub fn validate(&self, limits: ConnectLimits) -> Result<(), PayloadError> {
        limits.validate()?;
        let descriptor = descriptor_for(self.kind());
        if let Some(descriptor) = descriptor {
            if self.version() != descriptor.version || self.channel() != descriptor.channel {
                return Err(PayloadError::MetadataMismatch);
            }
        } else if !matches!(self, Self::Unknown(_)) {
            return Err(PayloadError::MetadataMismatch);
        }

        match self {
            Self::Hello(HelloPayload::Client(value)) => {
                value.validate().map_err(|_| PayloadError::Invalid)?;
            }
            Self::Hello(HelloPayload::Server(value)) => {
                value.validate().map_err(|_| PayloadError::Invalid)?;
            }
            Self::Capabilities(_) => {}
            Self::SnapshotPage(page) => validate_snapshot_page(page, limits)?,
            Self::EventPage(page) => validate_event_page(page, limits)?,
            Self::Request(request) => validate_request(request, limits)?,
            Self::Message(message) => validate_message(message, limits)?,
            Self::OperationSettlement(settlement) => settlement.validate()?,
            Self::Presence(hint) => {
                if hint.observed_at_ms < 0 {
                    return Err(PayloadError::Bounds);
                }
            }
            Self::TerminalDelta(frame) | Self::BrowserFrame(frame) => {
                validate_stream_frame(frame, limits)?;
            }
            Self::PromptExtension(value) => {
                if value.schema_version == 0 {
                    return Err(PayloadError::Bounds);
                }
            }
            Self::BrowserExtension(value) => {
                if value.schema_version == 0 {
                    return Err(PayloadError::Bounds);
                }
            }
            Self::Chunk(frame) => frame.validate(limits)?,
            Self::Error(error) => error.validate(limits)?,
            Self::Extension(extension) => {
                if extension.type_id == 0 || extension.schema_version == 0 {
                    return Err(PayloadError::Bounds);
                }
                limits.validate_payload_len(extension.payload.len())?;
            }
            Self::Unknown(value) => {
                if value.kind.known().is_some() || value.version == 0 {
                    return Err(PayloadError::MetadataMismatch);
                }
                limits.validate_payload_len(value.payload.len())?;
            }
        }
        Ok(())
    }
}

fn validate_request(request: &ClientRequest, limits: ConnectLimits) -> Result<(), PayloadError> {
    match request {
        ClientRequest::Command(_) => Ok(()),
        ClientRequest::Query(query) => validate_query(query, limits),
        ClientRequest::Detach(_) => Ok(()),
    }
}

fn validate_query(query: &QueryEnvelope, limits: ConnectLimits) -> Result<(), PayloadError> {
    let cursor = match &query.query {
        Query::SnapshotPage {
            snapshot_id,
            resume_cursor,
            ..
        } => {
            if snapshot_id.is_some() != resume_cursor.is_some() {
                return Err(PayloadError::Correlation);
            }
            resume_cursor.as_deref()
        }
        Query::ContinueEventReplay { resume_cursor, .. }
        | Query::ContinueArtifactContent { resume_cursor, .. } => Some(resume_cursor.as_slice()),
        _ => None,
    };
    if let Some(cursor) = cursor {
        limits.validate_cursor_len(cursor.len())?;
    }
    Ok(())
}

fn validate_message(message: &ServerMessage, limits: ConnectLimits) -> Result<(), PayloadError> {
    match message {
        ServerMessage::CommandReceipt(_) => Ok(()),
        ServerMessage::QueryReply(QueryReply { outcome, .. }) => {
            if matches!(outcome, crate::domain::query::QueryOutcome::Err(_)) {
                Ok(())
            } else {
                Ok(())
            }
        }
        ServerMessage::DurableEvent { event, .. } => {
            if event.sequence == 0 {
                return Err(PayloadError::Bounds);
            }
            Ok(())
        }
        ServerMessage::ResyncRequired {
            last_delivered_sequence,
            newest_sequence,
            ..
        } => {
            if newest_sequence < last_delivered_sequence {
                return Err(PayloadError::Bounds);
            }
            Ok(())
        }
        ServerMessage::Stream(frame) => validate_stream_frame(frame, limits),
        ServerMessage::Detached(_) => Ok(()),
    }
}

fn validate_stream_frame(frame: &StreamFrame, limits: ConnectLimits) -> Result<(), PayloadError> {
    if frame.generation == 0 || frame.sequence == 0 || frame.schema_version == 0 {
        return Err(PayloadError::Bounds);
    }
    limits.validate_payload_len(frame.payload.len())?;
    Ok(())
}

pub fn canonical_snapshot_page_size(page: &SnapshotPage) -> Result<u32, PayloadError> {
    let mut canonical = page.clone();
    canonical.encoded_bytes = 0;
    let encoded = rmp_serde::to_vec_named(&canonical).map_err(|_| PayloadError::Encode)?;
    u32::try_from(encoded.len()).map_err(|_| PayloadError::Bounds)
}

fn validate_snapshot_page(page: &SnapshotPage, limits: ConnectLimits) -> Result<(), PayloadError> {
    let encoded = canonical_snapshot_page_size(page)?;
    if u64::from(page.encoded_bytes) != u64::from(encoded) {
        return Err(PayloadError::CanonicalSizeMismatch);
    }
    limits.validate_page(page.items.len(), u64::from(encoded))?;
    if let Some(cursor) = page.next_cursor.as_deref() {
        limits.validate_cursor_len(cursor.len())?;
    }
    if page.through_sequence == 0 && !page.items.is_empty() {
        return Err(PayloadError::Bounds);
    }
    Ok(())
}

fn validate_event_page(page: &EventPage, limits: ConnectLimits) -> Result<(), PayloadError> {
    if page.through_sequence < page.after_sequence {
        return Err(PayloadError::Bounds);
    }
    let mut previous = page.after_sequence;
    for event in &page.events {
        if event.sequence <= previous || event.sequence > page.through_sequence {
            return Err(PayloadError::Bounds);
        }
        previous = event.sequence;
    }
    limits.validate_page(
        page.events.len(),
        u64::try_from(
            rmp_serde::to_vec_named(page)
                .map_err(|_| PayloadError::Encode)?
                .len(),
        )
        .map_err(|_| PayloadError::Bounds)?,
    )?;
    if let Some(cursor) = page.next_cursor.as_deref() {
        limits.validate_cursor_len(cursor.len())?;
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PayloadError {
    UnsupportedVersion { kind: PayloadKind, version: u16 },
    MetadataMismatch,
    Correlation,
    Settlement,
    CursorContext,
    CanonicalSizeMismatch,
    DigestMismatch,
    Bounds,
    Invalid,
    Encode,
    Limits(ConnectLimitError),
    MessagePack(MessagePackError),
}

impl fmt::Display for PayloadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedVersion { kind, version } => write!(
                formatter,
                "unsupported Connect payload version {version} for kind {}",
                kind.get()
            ),
            Self::MetadataMismatch => formatter.write_str("Connect payload metadata mismatch"),
            Self::Correlation => formatter.write_str("Connect payload correlation is invalid"),
            Self::Settlement => {
                formatter.write_str("Connect operation settlement is not authoritative")
            }
            Self::CursorContext => formatter.write_str("Connect chunk cursor context is invalid"),
            Self::CanonicalSizeMismatch => {
                formatter.write_str("Connect page encoded byte count is not canonical")
            }
            Self::DigestMismatch => formatter.write_str("Connect chunk digest is invalid"),
            Self::Bounds => formatter.write_str("Connect payload exceeds its typed bounds"),
            Self::Invalid => formatter.write_str("Connect payload is invalid"),
            Self::Encode => formatter.write_str("Connect payload encoding failed"),
            Self::Limits(error) => error.fmt(formatter),
            Self::MessagePack(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for PayloadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Limits(error) => Some(error),
            Self::MessagePack(error) => Some(error),
            _ => None,
        }
    }
}

impl From<ConnectLimitError> for PayloadError {
    fn from(error: ConnectLimitError) -> Self {
        Self::Limits(error)
    }
}

impl From<OutcomeFenceError> for PayloadError {
    fn from(_: OutcomeFenceError) -> Self {
        Self::Settlement
    }
}

enum PayloadTag {
    Hello,
    Capabilities,
    SnapshotPage,
    EventPage,
    Request,
    Message,
    OperationSettlement,
    Presence,
    TerminalDelta,
    BrowserFrame,
    PromptExtension,
    BrowserExtension,
    Chunk,
    Error,
    Extension,
    Unknown,
}

impl<'de> Deserialize<'de> for PayloadTag {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct TagVisitor;

        impl Visitor<'_> for TagVisitor {
            type Value = PayloadTag;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a known Connect payload tag")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                match value {
                    "hello" => Ok(PayloadTag::Hello),
                    "capabilities" => Ok(PayloadTag::Capabilities),
                    "snapshot_page" => Ok(PayloadTag::SnapshotPage),
                    "event_page" => Ok(PayloadTag::EventPage),
                    "request" => Ok(PayloadTag::Request),
                    "message" => Ok(PayloadTag::Message),
                    "operation_settlement" => Ok(PayloadTag::OperationSettlement),
                    "presence" => Ok(PayloadTag::Presence),
                    "terminal_delta" => Ok(PayloadTag::TerminalDelta),
                    "browser_frame" => Ok(PayloadTag::BrowserFrame),
                    "prompt_extension" => Ok(PayloadTag::PromptExtension),
                    "browser_extension" => Ok(PayloadTag::BrowserExtension),
                    "chunk" => Ok(PayloadTag::Chunk),
                    "error" => Ok(PayloadTag::Error),
                    "extension" => Ok(PayloadTag::Extension),
                    "unknown" => Ok(PayloadTag::Unknown),
                    _ => Err(de::Error::unknown_variant(
                        value,
                        &[
                            "hello",
                            "capabilities",
                            "snapshot_page",
                            "event_page",
                            "request",
                            "message",
                            "operation_settlement",
                            "presence",
                            "terminal_delta",
                            "browser_frame",
                            "prompt_extension",
                            "browser_extension",
                            "chunk",
                            "error",
                            "extension",
                            "unknown",
                        ],
                    )),
                }
            }
        }

        deserializer.deserialize_identifier(TagVisitor)
    }
}

impl Serialize for ConnectPayload {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(1))?;
        match self {
            Self::Hello(value) => map.serialize_entry("hello", value)?,
            Self::Capabilities(value) => map.serialize_entry("capabilities", value)?,
            Self::SnapshotPage(value) => map.serialize_entry("snapshot_page", value)?,
            Self::EventPage(value) => map.serialize_entry("event_page", value)?,
            Self::Request(value) => map.serialize_entry("request", value)?,
            Self::Message(value) => map.serialize_entry("message", value)?,
            Self::OperationSettlement(value) => {
                map.serialize_entry("operation_settlement", value)?
            }
            Self::Presence(value) => map.serialize_entry("presence", value)?,
            Self::TerminalDelta(value) => map.serialize_entry("terminal_delta", value)?,
            Self::BrowserFrame(value) => map.serialize_entry("browser_frame", value)?,
            Self::PromptExtension(value) => map.serialize_entry("prompt_extension", value)?,
            Self::BrowserExtension(value) => map.serialize_entry("browser_extension", value)?,
            Self::Chunk(value) => map.serialize_entry("chunk", value)?,
            Self::Error(value) => map.serialize_entry("error", value)?,
            Self::Extension(value) => map.serialize_entry("extension", value)?,
            Self::Unknown(value) => map.serialize_entry("unknown", value)?,
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for ConnectPayload {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct PayloadVisitor;

        impl<'de> Visitor<'de> for PayloadVisitor {
            type Value = ConnectPayload;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a one-entry named Connect payload map")
            }

            fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                let tag = map
                    .next_key::<PayloadTag>()?
                    .ok_or_else(|| de::Error::custom("Connect payload tag is missing"))?;
                let payload = match tag {
                    PayloadTag::Hello => ConnectPayload::Hello(map.next_value()?),
                    PayloadTag::Capabilities => ConnectPayload::Capabilities(map.next_value()?),
                    PayloadTag::SnapshotPage => ConnectPayload::SnapshotPage(map.next_value()?),
                    PayloadTag::EventPage => ConnectPayload::EventPage(map.next_value()?),
                    PayloadTag::Request => ConnectPayload::Request(map.next_value()?),
                    PayloadTag::Message => ConnectPayload::Message(map.next_value()?),
                    PayloadTag::OperationSettlement => {
                        ConnectPayload::OperationSettlement(map.next_value()?)
                    }
                    PayloadTag::Presence => ConnectPayload::Presence(map.next_value()?),
                    PayloadTag::TerminalDelta => ConnectPayload::TerminalDelta(map.next_value()?),
                    PayloadTag::BrowserFrame => ConnectPayload::BrowserFrame(map.next_value()?),
                    PayloadTag::PromptExtension => {
                        ConnectPayload::PromptExtension(map.next_value()?)
                    }
                    PayloadTag::BrowserExtension => {
                        ConnectPayload::BrowserExtension(map.next_value()?)
                    }
                    PayloadTag::Chunk => ConnectPayload::Chunk(map.next_value()?),
                    PayloadTag::Error => ConnectPayload::Error(map.next_value()?),
                    PayloadTag::Extension => ConnectPayload::Extension(map.next_value()?),
                    PayloadTag::Unknown => ConnectPayload::Unknown(map.next_value()?),
                };
                if map.next_key::<de::IgnoredAny>()?.is_some() {
                    return Err(de::Error::custom(
                        "Connect payload must contain exactly one entry",
                    ));
                }
                Ok(payload)
            }
        }

        deserializer.deserialize_map(PayloadVisitor)
    }
}

mod binary {
    use serde::de::{self, Deserializer, Visitor};
    use serde::Serializer;
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
        struct BinaryVisitor;

        impl<'de> Visitor<'de> for BinaryVisitor {
            type Value = Vec<u8>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("MessagePack binary bytes")
            }

            fn visit_bytes<E: de::Error>(self, value: &[u8]) -> Result<Self::Value, E> {
                Ok(value.to_vec())
            }

            fn visit_byte_buf<E: de::Error>(self, value: Vec<u8>) -> Result<Self::Value, E> {
                Ok(value)
            }

            fn visit_seq<A: de::SeqAccess<'de>>(self, _seq: A) -> Result<Self::Value, A::Error> {
                Err(de::Error::invalid_type(de::Unexpected::Seq, &self))
            }
        }

        deserializer.deserialize_bytes(BinaryVisitor)
    }
}

mod optional_binary {
    use serde::de::{self, Deserializer, Visitor};
    use std::fmt;

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<Vec<u8>>, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct OptionalBinaryVisitor;

        impl<'de> Visitor<'de> for OptionalBinaryVisitor {
            type Value = Option<Vec<u8>>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("optional MessagePack binary bytes")
            }

            fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(None)
            }

            fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(None)
            }

            fn visit_some<D: Deserializer<'de>>(
                self,
                deserializer: D,
            ) -> Result<Self::Value, D::Error> {
                super::binary::deserialize(deserializer).map(Some)
            }
        }

        deserializer.deserialize_option(OptionalBinaryVisitor)
    }
}

struct BinaryRef<'a>(&'a [u8]);

impl Serialize for BinaryRef<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_bytes(self.0)
    }
}

struct OptionalBinaryRef<'a>(Option<&'a [u8]>);

impl Serialize for OptionalBinaryRef<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self.0 {
            Some(value) => serializer.serialize_some(&BinaryRef(value)),
            None => serializer.serialize_none(),
        }
    }
}
