//! Canonical Connect v1 payload catalog.
//!
//! Phase 1 domain and protocol types remain the semantic source of truth. This
//! module supplies the Connect discriminant, version, channel, privacy, and
//! bounded payload wrapper that carries those types over direct or relay routes.

use serde::{Deserialize, Serialize};

use crate::domain::command::{CommandEnvelope, CommandReceipt};
use crate::domain::id::{
    ClientId, CommandId, EventId, OperationId, RequestId, SnapshotId, TaskId, TransferId,
};
use crate::domain::operation::OperationState;
use crate::domain::query::{Query, QueryEnvelope};
use crate::domain::snapshot::{EventPage, SnapshotPage, SnapshotSection};
use crate::protocol::{CapabilitySet, MessagePackCodec, MessagePackError};

use super::envelope::{
    binary_payload, ChannelKind, ConnectLimitError, ConnectLimits, ConnectPrivacyClass,
    KnownPayloadKind, PayloadKind, MAX_CONNECT_DIAGNOSTIC_BYTES, MAX_CONNECT_PAGE_ENCODED_BYTES,
    MAX_CONNECT_REASSEMBLED_MESSAGE_BYTES,
};
use super::epoch::{FocusEpoch, TurnEpoch};
use super::permission::HostCapabilityGrant;
use super::presence::LastSenderHint;
use super::transport::{
    validate_advertised_relay_url, BrowserExtensionDescriptor, PromptExtensionDescriptor,
};

pub const CONNECT_PAYLOAD_SCHEMA_VERSION: u16 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PayloadDescriptor {
    pub kind: PayloadKind,
    pub name: &'static str,
    pub channel: ChannelKind,
    pub action: bool,
    pub allows_raw_content: bool,
    pub max_payload_bytes: u32,
}

/// The reviewed v1 catalog. Unknown discriminants are decoded only as the
/// inert `Extension` variant and are never added to this table implicitly.
static PAYLOAD_CATALOG: &[PayloadDescriptor] = &[
    descriptor(
        PayloadKind::HELLO,
        "hello",
        ChannelKind::Critical,
        false,
        false,
        MAX_CONNECT_REASSEMBLED_MESSAGE_BYTES,
    ),
    descriptor(
        PayloadKind::CAPABILITIES,
        "capabilities",
        ChannelKind::Critical,
        false,
        false,
        MAX_CONNECT_REASSEMBLED_MESSAGE_BYTES,
    ),
    descriptor(
        PayloadKind::SNAPSHOT_PAGE,
        "snapshot_page",
        ChannelKind::Durable,
        false,
        false,
        MAX_CONNECT_PAGE_ENCODED_BYTES,
    ),
    descriptor(
        PayloadKind::EVENT_PAGE,
        "event_page",
        ChannelKind::Durable,
        false,
        false,
        MAX_CONNECT_PAGE_ENCODED_BYTES,
    ),
    descriptor(
        PayloadKind::QUERY,
        "query",
        ChannelKind::Critical,
        false,
        false,
        MAX_CONNECT_REASSEMBLED_MESSAGE_BYTES,
    ),
    descriptor(
        PayloadKind::COMMAND,
        "command",
        ChannelKind::Critical,
        true,
        false,
        MAX_CONNECT_REASSEMBLED_MESSAGE_BYTES,
    ),
    descriptor(
        PayloadKind::COMMAND_RECEIPT,
        "command_receipt",
        ChannelKind::Critical,
        false,
        false,
        MAX_CONNECT_REASSEMBLED_MESSAGE_BYTES,
    ),
    descriptor(
        PayloadKind::OPERATION_SETTLEMENT,
        "operation_settlement",
        ChannelKind::Critical,
        false,
        false,
        MAX_CONNECT_REASSEMBLED_MESSAGE_BYTES,
    ),
    descriptor(
        PayloadKind::PRESENCE,
        "presence",
        ChannelKind::Ephemeral,
        false,
        false,
        MAX_CONNECT_DIAGNOSTIC_BYTES,
    ),
    descriptor(
        PayloadKind::TERMINAL_DELTA,
        "terminal_delta",
        ChannelKind::Ephemeral,
        false,
        true,
        MAX_CONNECT_REASSEMBLED_MESSAGE_BYTES,
    ),
    descriptor(
        PayloadKind::BROWSER_FRAME,
        "browser_frame",
        ChannelKind::Ephemeral,
        false,
        true,
        MAX_CONNECT_REASSEMBLED_MESSAGE_BYTES,
    ),
    descriptor(
        PayloadKind::PROMPT_EXTENSION,
        "prompt_extension",
        ChannelKind::Durable,
        false,
        false,
        MAX_CONNECT_REASSEMBLED_MESSAGE_BYTES,
    ),
    descriptor(
        PayloadKind::BROWSER_EXTENSION,
        "browser_extension",
        ChannelKind::Durable,
        false,
        false,
        MAX_CONNECT_REASSEMBLED_MESSAGE_BYTES,
    ),
    descriptor(
        PayloadKind::CHUNK,
        "chunk",
        ChannelKind::Durable,
        false,
        true,
        MAX_CONNECT_REASSEMBLED_MESSAGE_BYTES,
    ),
    descriptor(
        PayloadKind::RESYNC,
        "resync",
        ChannelKind::Critical,
        false,
        false,
        MAX_CONNECT_DIAGNOSTIC_BYTES,
    ),
    descriptor(
        PayloadKind::ERROR,
        "error",
        ChannelKind::Critical,
        false,
        false,
        MAX_CONNECT_DIAGNOSTIC_BYTES,
    ),
    descriptor(
        PayloadKind::EXTENSION,
        "extension",
        ChannelKind::Durable,
        false,
        false,
        MAX_CONNECT_REASSEMBLED_MESSAGE_BYTES,
    ),
];

pub const fn payload_catalog() -> &'static [PayloadDescriptor] {
    PAYLOAD_CATALOG
}

const fn descriptor(
    kind: PayloadKind,
    name: &'static str,
    channel: ChannelKind,
    action: bool,
    allows_raw_content: bool,
    max_payload_bytes: u32,
) -> PayloadDescriptor {
    PayloadDescriptor {
        kind,
        name,
        channel,
        action,
        allows_raw_content,
        max_payload_bytes,
    }
}

pub fn catalog_entry(kind: PayloadKind) -> Option<&'static PayloadDescriptor> {
    payload_catalog()
        .iter()
        .find(|descriptor| descriptor.kind == kind)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HelloPayload {
    pub capabilities: CapabilitySet,
    pub limits: ConnectLimits,
    pub privacy_class: ConnectPrivacyClass,
    /// Optional host route advertisement for relay-capable peers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay_url: Option<String>,
    /// Optional explicit host grant. Omission is no authority.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability_grant: Option<HostCapabilityGrant>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationSettlementPayload {
    pub operation_id: OperationId,
    pub state: OperationState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamDeltaPayload {
    pub task_id: Option<TaskId>,
    pub generation: u64,
    pub sequence: u64,
    #[serde(with = "binary_payload")]
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChunkPayload {
    pub transfer_id: TransferId,
    pub index: u32,
    pub final_chunk: bool,
    pub cumulative_bytes: u64,
    pub cumulative_sha256: [u8; 32],
    #[serde(with = "binary_payload")]
    pub payload: Vec<u8>,
    #[serde(with = "optional_binary")]
    pub resume_cursor: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResyncPayload {
    pub channel_sequence: u64,
    pub newest_sequence: u64,
    pub reason: ResyncReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResyncReason {
    Gap,
    ReplayUnavailable,
    Backpressure,
    ProtocolMismatch,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ErrorPayload {
    pub code: u16,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GenericExtensionPayload {
    pub type_id: u16,
    pub schema_version: u16,
    #[serde(with = "binary_payload")]
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectPayload {
    Hello(HelloPayload),
    Capabilities(CapabilitySet),
    SnapshotPage(SnapshotPage),
    EventPage(EventPage),
    Query(QueryEnvelope),
    Command(CommandEnvelope),
    CommandReceipt(CommandReceipt),
    OperationSettlement(OperationSettlementPayload),
    Presence(LastSenderHint),
    TerminalDelta(StreamDeltaPayload),
    BrowserFrame(StreamDeltaPayload),
    PromptExtension(PromptExtensionDescriptor),
    BrowserExtension(BrowserExtensionDescriptor),
    Chunk(ChunkPayload),
    Resync(ResyncPayload),
    Error(ErrorPayload),
    Extension(GenericExtensionPayload),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalSchemaFixture {
    pub name: &'static str,
    pub payload: ConnectPayload,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PayloadDecodeError {
    UnsupportedVersion { kind: PayloadKind, version: u16 },
    MessagePack(MessagePackError),
    Limits(ConnectLimitError),
    Ambiguous { reason: &'static str },
    InvalidPayload,
}

impl std::fmt::Display for PayloadDecodeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedVersion { kind, version } => write!(
                formatter,
                "unsupported Connect payload version {version} for kind {}",
                kind.get()
            ),
            Self::MessagePack(error) => error.fmt(formatter),
            Self::Limits(error) => error.fmt(formatter),
            Self::Ambiguous { reason } => write!(formatter, "ambiguous Connect payload: {reason}"),
            Self::InvalidPayload => formatter.write_str("invalid Connect payload"),
        }
    }
}

impl std::error::Error for PayloadDecodeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::MessagePack(error) => Some(error),
            Self::Limits(error) => Some(error),
            _ => None,
        }
    }
}

impl From<MessagePackError> for PayloadDecodeError {
    fn from(error: MessagePackError) -> Self {
        Self::MessagePack(error)
    }
}

impl From<ConnectLimitError> for PayloadDecodeError {
    fn from(error: ConnectLimitError) -> Self {
        Self::Limits(error)
    }
}

impl ConnectPayload {
    pub fn kind(&self) -> PayloadKind {
        match self {
            Self::Hello(_) => PayloadKind::HELLO,
            Self::Capabilities(_) => PayloadKind::CAPABILITIES,
            Self::SnapshotPage(_) => PayloadKind::SNAPSHOT_PAGE,
            Self::EventPage(_) => PayloadKind::EVENT_PAGE,
            Self::Query(_) => PayloadKind::QUERY,
            Self::Command(_) => PayloadKind::COMMAND,
            Self::CommandReceipt(_) => PayloadKind::COMMAND_RECEIPT,
            Self::OperationSettlement(_) => PayloadKind::OPERATION_SETTLEMENT,
            Self::Presence(_) => PayloadKind::PRESENCE,
            Self::TerminalDelta(_) => PayloadKind::TERMINAL_DELTA,
            Self::BrowserFrame(_) => PayloadKind::BROWSER_FRAME,
            Self::PromptExtension(_) => PayloadKind::PROMPT_EXTENSION,
            Self::BrowserExtension(_) => PayloadKind::BROWSER_EXTENSION,
            Self::Chunk(_) => PayloadKind::CHUNK,
            Self::Resync(_) => PayloadKind::RESYNC,
            Self::Error(_) => PayloadKind::ERROR,
            Self::Extension(extension) => PayloadKind::new(extension.type_id)
                .filter(|kind| kind.known().is_none() || *kind == PayloadKind::EXTENSION)
                .unwrap_or(PayloadKind::EXTENSION),
        }
    }

    pub const fn version(&self) -> u16 {
        match self {
            Self::Extension(extension) => extension.schema_version,
            _ => CONNECT_PAYLOAD_SCHEMA_VERSION,
        }
    }

    pub const fn channel(&self) -> ChannelKind {
        match self {
            Self::Hello(_)
            | Self::Capabilities(_)
            | Self::Query(_)
            | Self::Command(_)
            | Self::CommandReceipt(_)
            | Self::OperationSettlement(_)
            | Self::Resync(_)
            | Self::Error(_) => ChannelKind::Critical,
            Self::SnapshotPage(_)
            | Self::EventPage(_)
            | Self::PromptExtension(_)
            | Self::BrowserExtension(_)
            | Self::Chunk(_)
            | Self::Extension(_) => ChannelKind::Durable,
            Self::Presence(_) | Self::TerminalDelta(_) | Self::BrowserFrame(_) => {
                ChannelKind::Ephemeral
            }
        }
    }

    pub const fn is_action(&self) -> bool {
        matches!(self, Self::Command(_))
    }

    pub const fn allows_raw_content(&self) -> bool {
        matches!(
            self,
            Self::TerminalDelta(_) | Self::BrowserFrame(_) | Self::Chunk(_)
        )
    }

    pub const fn as_command(&self) -> Option<&CommandEnvelope> {
        match self {
            Self::Command(command) => Some(command),
            _ => None,
        }
    }

    pub fn encode(&self, limits: ConnectLimits) -> Result<Vec<u8>, PayloadDecodeError> {
        self.validate(limits)?;
        let bytes = match self {
            Self::Extension(extension) if self.kind().known().is_none() => {
                if extension.payload.is_empty() {
                    return Err(PayloadDecodeError::InvalidPayload);
                }
                extension.payload.clone()
            }
            Self::Hello(value) => encode_named(value, limits)?,
            Self::Capabilities(value) => encode_named(value, limits)?,
            Self::SnapshotPage(value) => encode_named(value, limits)?,
            Self::EventPage(value) => encode_named(value, limits)?,
            Self::Query(value) => encode_named(value, limits)?,
            Self::Command(value) => encode_named(value, limits)?,
            Self::CommandReceipt(value) => encode_named(value, limits)?,
            Self::OperationSettlement(value) => encode_named(value, limits)?,
            Self::Presence(value) => encode_named(value, limits)?,
            Self::TerminalDelta(value) | Self::BrowserFrame(value) => encode_named(value, limits)?,
            Self::PromptExtension(value) => encode_named(value, limits)?,
            Self::BrowserExtension(value) => encode_named(value, limits)?,
            Self::Chunk(value) => encode_named(value, limits)?,
            Self::Resync(value) => encode_named(value, limits)?,
            Self::Error(value) => encode_named(value, limits)?,
            Self::Extension(value) => encode_named(value, limits)?,
        };
        limits.validate_payload_len(bytes.len())?;
        if let Some(descriptor) = catalog_entry(self.kind()) {
            let declared = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
            if declared > u64::from(descriptor.max_payload_bytes) {
                return Err(ConnectLimitError::PayloadExceeded {
                    declared,
                    maximum: descriptor.max_payload_bytes,
                }
                .into());
            }
        }
        Ok(bytes)
    }

    pub fn decode(
        kind: PayloadKind,
        version: u16,
        bytes: &[u8],
        limits: ConnectLimits,
    ) -> Result<Self, PayloadDecodeError> {
        limits.validate()?;
        if bytes.is_empty() {
            return Err(PayloadDecodeError::InvalidPayload);
        }
        limits.validate_payload_len(bytes.len())?;
        if kind.known().is_none() {
            if version == 0 {
                return Err(PayloadDecodeError::UnsupportedVersion { kind, version });
            }
            return Ok(Self::Extension(GenericExtensionPayload {
                type_id: kind.get(),
                schema_version: version,
                payload: bytes.to_vec(),
            }));
        }
        if version != CONNECT_PAYLOAD_SCHEMA_VERSION {
            return Err(PayloadDecodeError::UnsupportedVersion { kind, version });
        }
        if let Some(descriptor) = catalog_entry(kind) {
            let declared = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
            if declared > u64::from(descriptor.max_payload_bytes) {
                return Err(ConnectLimitError::PayloadExceeded {
                    declared,
                    maximum: descriptor.max_payload_bytes,
                }
                .into());
            }
        }
        let codec = MessagePackCodec::from_limits(limits.frame_limits())
            .map_err(|_| PayloadDecodeError::InvalidPayload)?;
        let payload = match kind.known().ok_or(PayloadDecodeError::InvalidPayload)? {
            KnownPayloadKind::Hello => Self::Hello(codec.decode(bytes)?),
            KnownPayloadKind::Capabilities => Self::Capabilities(codec.decode(bytes)?),
            KnownPayloadKind::SnapshotPage => Self::SnapshotPage(codec.decode(bytes)?),
            KnownPayloadKind::EventPage => Self::EventPage(codec.decode(bytes)?),
            KnownPayloadKind::Query => Self::Query(codec.decode(bytes)?),
            KnownPayloadKind::Command => Self::Command(codec.decode(bytes)?),
            KnownPayloadKind::CommandReceipt => Self::CommandReceipt(codec.decode(bytes)?),
            KnownPayloadKind::OperationSettlement => {
                Self::OperationSettlement(codec.decode(bytes)?)
            }
            KnownPayloadKind::Presence => Self::Presence(codec.decode(bytes)?),
            KnownPayloadKind::TerminalDelta => Self::TerminalDelta(codec.decode(bytes)?),
            KnownPayloadKind::BrowserFrame => Self::BrowserFrame(codec.decode(bytes)?),
            KnownPayloadKind::PromptExtension => Self::PromptExtension(codec.decode(bytes)?),
            KnownPayloadKind::BrowserExtension => Self::BrowserExtension(codec.decode(bytes)?),
            KnownPayloadKind::Chunk => Self::Chunk(codec.decode(bytes)?),
            KnownPayloadKind::Resync => Self::Resync(codec.decode(bytes)?),
            KnownPayloadKind::Error => Self::Error(codec.decode(bytes)?),
            KnownPayloadKind::Extension => Self::Extension(codec.decode(bytes)?),
        };
        payload.validate(limits)?;
        Ok(payload)
    }

    pub fn validate(&self, limits: ConnectLimits) -> Result<(), PayloadDecodeError> {
        limits.validate()?;
        match self {
            Self::Hello(hello) => {
                hello.limits.validate()?;
                if matches!(hello.privacy_class, ConnectPrivacyClass::RawContent) {
                    return Err(PayloadDecodeError::Ambiguous {
                        reason: "hello cannot advertise RawContent as the default privacy class",
                    });
                }
                if let Some(relay_url) = hello.relay_url.as_deref() {
                    validate_advertised_relay_url(relay_url, None).map_err(|_| {
                        PayloadDecodeError::Ambiguous {
                            reason: "hello advertised relay URL is invalid",
                        }
                    })?;
                }
            }
            Self::SnapshotPage(page) => {
                limits.validate_page(page.items.len(), u64::from(page.encoded_bytes))?;
                if let Some(cursor) = page.next_cursor.as_deref() {
                    limits.validate_cursor_len(cursor.len())?;
                }
            }
            Self::EventPage(page) => {
                limits.validate_page(page.events.len(), 0)?;
                if let Some(cursor) = page.next_cursor.as_deref() {
                    limits.validate_cursor_len(cursor.len())?;
                }
            }
            Self::Chunk(chunk) => {
                let cumulative = limits.validate_chunk(
                    chunk
                        .cumulative_bytes
                        .saturating_sub(u64::try_from(chunk.payload.len()).unwrap_or(u64::MAX)),
                    &chunk.payload,
                )?;
                if cumulative != chunk.cumulative_bytes {
                    return Err(PayloadDecodeError::Ambiguous {
                        reason: "chunk cumulative_bytes does not match payload length",
                    });
                }
                if let Some(cursor) = chunk.resume_cursor.as_deref() {
                    limits.validate_cursor_len(cursor.len())?;
                }
            }
            Self::Resync(resync) => {
                if resync.newest_sequence < resync.channel_sequence {
                    return Err(PayloadDecodeError::Ambiguous {
                        reason: "resync newest_sequence is before channel_sequence",
                    });
                }
            }
            Self::Error(error) => {
                limits.validate_diagnostic_len(error.message.len())?;
            }
            Self::TerminalDelta(delta) | Self::BrowserFrame(delta) => {
                limits.validate_payload_len(delta.payload.len())?;
            }
            Self::Extension(extension) => {
                if extension.type_id == 0 || extension.schema_version == 0 {
                    return Err(PayloadDecodeError::Ambiguous {
                        reason: "extension type_id and schema_version must be nonzero",
                    });
                }
                if PayloadKind::new(extension.type_id)
                    .and_then(PayloadKind::known)
                    .is_some_and(|kind| kind.is_action())
                {
                    return Err(PayloadDecodeError::Ambiguous {
                        reason: "generic extensions cannot carry action discriminants",
                    });
                }
                limits.validate_payload_len(extension.payload.len())?;
            }
            Self::Capabilities(_)
            | Self::Query(_)
            | Self::Command(_)
            | Self::CommandReceipt(_)
            | Self::OperationSettlement(_)
            | Self::Presence(_)
            | Self::PromptExtension(_)
            | Self::BrowserExtension(_) => {}
        }
        Ok(())
    }
}

fn encode_named<T: Serialize>(
    value: &T,
    limits: ConnectLimits,
) -> Result<Vec<u8>, PayloadDecodeError> {
    let codec = MessagePackCodec::from_limits(limits.frame_limits())
        .map_err(|_| PayloadDecodeError::InvalidPayload)?;
    codec.encode(value).map_err(PayloadDecodeError::MessagePack)
}

fn fixture_uuid(tail: u8) -> [u8; 16] {
    let mut bytes = [0_u8; 16];
    bytes[0] = 0x01;
    bytes[1] = 0x23;
    bytes[2] = 0x45;
    bytes[3] = 0x67;
    bytes[4] = 0x89;
    bytes[5] = 0xab;
    bytes[6] = 0x70;
    bytes[8] = 0x80;
    bytes[15] = tail;
    bytes
}

fn fixture_task(tail: u8) -> TaskId {
    TaskId::from_bytes(fixture_uuid(tail)).expect("canonical task id")
}

fn fixture_request(tail: u8) -> RequestId {
    RequestId::from_bytes(fixture_uuid(tail)).expect("canonical request id")
}

fn fixture_client(tail: u8) -> ClientId {
    ClientId::from_bytes(fixture_uuid(tail)).expect("canonical client id")
}

fn fixture_command(tail: u8) -> CommandId {
    CommandId::from_bytes(fixture_uuid(tail)).expect("canonical command id")
}

fn fixture_operation(tail: u8) -> OperationId {
    OperationId::from_bytes(fixture_uuid(tail)).expect("canonical operation id")
}

fn fixture_event(tail: u8) -> EventId {
    EventId::from_bytes(fixture_uuid(tail)).expect("canonical event id")
}

fn fixture_snapshot(tail: u8) -> SnapshotId {
    SnapshotId::from_bytes(fixture_uuid(tail)).expect("canonical snapshot id")
}

fn fixture_transfer(tail: u8) -> TransferId {
    TransferId::from_bytes(fixture_uuid(tail)).expect("canonical transfer id")
}

/// Deterministic named payloads used as the Rust schema source of truth.
pub fn canonical_schema_fixtures() -> Vec<CanonicalSchemaFixture> {
    let empty_caps = CapabilitySet::empty();
    vec![
        CanonicalSchemaFixture {
            name: "hello",
            payload: ConnectPayload::Hello(HelloPayload {
                capabilities: empty_caps,
                limits: ConnectLimits::v1_default(),
                privacy_class: ConnectPrivacyClass::LocalOnly,
                relay_url: None,
                capability_grant: None,
            }),
        },
        CanonicalSchemaFixture {
            name: "capabilities",
            payload: ConnectPayload::Capabilities(empty_caps),
        },
        CanonicalSchemaFixture {
            name: "snapshot_page",
            payload: ConnectPayload::SnapshotPage(SnapshotPage {
                snapshot_id: fixture_snapshot(0x31),
                through_sequence: 1,
                section: SnapshotSection::Tasks,
                after_item: None,
                items: Vec::new(),
                encoded_bytes: 0,
                next_cursor: None,
            }),
        },
        CanonicalSchemaFixture {
            name: "event_page",
            payload: ConnectPayload::EventPage(EventPage {
                after_sequence: 0,
                through_sequence: 0,
                events: Vec::new(),
                next_cursor: None,
            }),
        },
        CanonicalSchemaFixture {
            name: "query",
            payload: ConnectPayload::Query(QueryEnvelope {
                request_id: fixture_request(0x41),
                client_id: fixture_client(0x42),
                task_id: Some(fixture_task(0x43)),
                query: Query::TaskSnapshot,
            }),
        },
        CanonicalSchemaFixture {
            name: "command",
            payload: ConnectPayload::Command(CommandEnvelope {
                command_id: fixture_command(0x51),
                client_id: fixture_client(0x52),
                task_id: Some(fixture_task(0x53)),
                issued_at_ms: 1,
                expected_task_revision: Some(1),
                command: crate::domain::command::Command::BeginCloseTask,
            }),
        },
        CanonicalSchemaFixture {
            name: "command_receipt",
            payload: ConnectPayload::CommandReceipt(CommandReceipt::Accepted {
                command_id: fixture_command(0x51),
                operation_id: fixture_operation(0x54),
                task_revision: Some(1),
                event_ids: vec![fixture_event(0x55)],
                prompt_mutation: None,
            }),
        },
        CanonicalSchemaFixture {
            name: "operation_settlement",
            payload: ConnectPayload::OperationSettlement(OperationSettlementPayload {
                operation_id: fixture_operation(0x54),
                state: OperationState::Accepted,
            }),
        },
        CanonicalSchemaFixture {
            name: "presence",
            payload: ConnectPayload::Presence(LastSenderHint::new(
                fixture_task(0x43),
                fixture_client(0x42),
                1,
                TurnEpoch::new(1).expect("canonical turn epoch"),
                FocusEpoch::new(1).expect("canonical focus epoch"),
            )),
        },
        CanonicalSchemaFixture {
            name: "terminal_delta",
            payload: ConnectPayload::TerminalDelta(StreamDeltaPayload {
                task_id: Some(fixture_task(0x43)),
                generation: 1,
                sequence: 1,
                payload: vec![0x61],
            }),
        },
        CanonicalSchemaFixture {
            name: "browser_frame",
            payload: ConnectPayload::BrowserFrame(StreamDeltaPayload {
                task_id: Some(fixture_task(0x43)),
                generation: 1,
                sequence: 1,
                payload: vec![0x62],
            }),
        },
        CanonicalSchemaFixture {
            name: "prompt_extension",
            payload: ConnectPayload::PromptExtension(PromptExtensionDescriptor {
                schema_version: CONNECT_PAYLOAD_SCHEMA_VERSION,
                capabilities: empty_caps,
            }),
        },
        CanonicalSchemaFixture {
            name: "browser_extension",
            payload: ConnectPayload::BrowserExtension(BrowserExtensionDescriptor {
                schema_version: CONNECT_PAYLOAD_SCHEMA_VERSION,
                capabilities: empty_caps,
            }),
        },
        CanonicalSchemaFixture {
            name: "chunk",
            payload: ConnectPayload::Chunk(ChunkPayload {
                transfer_id: fixture_transfer(0x61),
                index: 0,
                final_chunk: true,
                cumulative_bytes: 1,
                cumulative_sha256: [0xab; 32],
                payload: vec![0x63],
                resume_cursor: None,
            }),
        },
        CanonicalSchemaFixture {
            name: "resync",
            payload: ConnectPayload::Resync(ResyncPayload {
                channel_sequence: 2,
                newest_sequence: 4,
                reason: ResyncReason::Gap,
            }),
        },
        CanonicalSchemaFixture {
            name: "error",
            payload: ConnectPayload::Error(ErrorPayload {
                code: 1,
                message: "unavailable".to_owned(),
            }),
        },
        CanonicalSchemaFixture {
            name: "extension",
            payload: ConnectPayload::Extension(GenericExtensionPayload {
                type_id: 0x7fff,
                schema_version: 9,
                payload: vec![0x91, 0x01],
            }),
        },
    ]
}

pub fn encode_canonical_schema(
    limits: ConnectLimits,
) -> Result<Vec<(&'static str, Vec<u8>)>, PayloadDecodeError> {
    canonical_schema_fixtures()
        .into_iter()
        .map(|fixture| {
            fixture
                .payload
                .encode(limits)
                .map(|bytes| (fixture.name, bytes))
        })
        .collect()
}

mod optional_binary {
    use serde::de::Deserializer;
    use serde::ser::Serializer;
    use serde::{Deserialize, Serialize};

    pub fn serialize<S>(value: &Option<Vec<u8>>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match value {
            Some(value) => serializer.serialize_some(&BinaryRef(value)),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<Vec<u8>>, D::Error>
    where
        D: Deserializer<'de>,
    {
        Option::<BinaryValue>::deserialize(deserializer).map(|value| value.map(|value| value.0))
    }

    struct BinaryRef<'a>(&'a [u8]);

    impl Serialize for BinaryRef<'_> {
        fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            serializer.serialize_bytes(self.0)
        }
    }

    struct BinaryValue(Vec<u8>);

    impl<'de> Deserialize<'de> for BinaryValue {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            super::binary_payload::deserialize(deserializer).map(Self)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hello(
        relay_url: Option<String>,
        capability_grant: Option<HostCapabilityGrant>,
    ) -> HelloPayload {
        HelloPayload {
            capabilities: CapabilitySet::empty(),
            limits: ConnectLimits::v1_default(),
            privacy_class: ConnectPrivacyClass::LocalOnly,
            relay_url,
            capability_grant,
        }
    }

    #[test]
    fn hello_boundary_fields_are_additive_and_fail_closed() {
        let encoded = serde_json::to_string(&hello(None, None)).expect("encode");
        assert!(!encoded.contains("relay_url"));
        assert!(!encoded.contains("capability_grant"));
        let decoded: HelloPayload = serde_json::from_str(&encoded).expect("decode");
        assert_eq!(decoded.relay_url, None);
        assert_eq!(decoded.capability_grant, None);
        ConnectPayload::Hello(decoded)
            .validate(ConnectLimits::v1_default())
            .expect("omitted optional authority is valid");
    }

    #[test]
    fn hello_rejects_invalid_relay_advertisement() {
        let payload = ConnectPayload::Hello(hello(
            Some("https://relay.example.test/connect".to_owned()),
            None,
        ));
        assert!(payload.validate(ConnectLimits::v1_default()).is_err());
    }
}
