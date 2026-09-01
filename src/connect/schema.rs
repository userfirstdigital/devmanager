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
use crate::domain::query::{Query, QueryEnvelope, QueryError, QueryOutcome, QueryReply};
use crate::domain::snapshot::{EventPage, SnapshotPage, SnapshotSection};
use crate::protocol::{
    Capability, CapabilitySet, MessagePackCodec, MessagePackError, ServerMessage, StreamFrame,
    StreamPayloadKind,
};

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
        PayloadKind::QUERY_REPLY,
        "query_reply",
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
    descriptor(
        PayloadKind::HOST_DURABLE_OUTPUT,
        "host_durable_output",
        ChannelKind::Durable,
        false,
        false,
        MAX_CONNECT_REASSEMBLED_MESSAGE_BYTES,
    ),
    descriptor(
        PayloadKind::HOST_CRITICAL_OUTPUT,
        "host_critical_output",
        ChannelKind::Critical,
        false,
        false,
        MAX_CONNECT_REASSEMBLED_MESSAGE_BYTES,
    ),
    descriptor(
        PayloadKind::HOST_STREAM_OUTPUT,
        "host_stream_output",
        ChannelKind::Ephemeral,
        false,
        true,
        MAX_CONNECT_REASSEMBLED_MESSAGE_BYTES,
    ),
    descriptor(
        PayloadKind::HOST_CONVERSATION_OUTPUT,
        "host_conversation_output",
        ChannelKind::Ephemeral,
        false,
        false,
        MAX_CONNECT_DIAGNOSTIC_BYTES,
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
    /// Optional Connect client identity. Omission asks the host to assign one
    /// and return it on the Hello reply. A supplied UUIDv7 is bound as-is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<ClientId>,
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
    /// Envelope/request correlation. Omission is no request identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<RequestId>,
    /// Envelope/operation correlation. Omission is no operation identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<OperationId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GenericExtensionPayload {
    pub type_id: u16,
    pub schema_version: u16,
    #[serde(with = "binary_payload")]
    pub payload: Vec<u8>,
}

/// Lane discriminant for lossless unsolicited host [`ServerMessage`] wrappers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostOutputLane {
    Durable,
    Critical,
    Ephemeral,
}

/// Lossless Connect wrapper for an existing host [`ServerMessage`].
///
/// The wrapper does not authorize a resource: the future host writer must
/// supply an already-authorized subscription and compare
/// [`Self::required_capabilities`] against negotiated capabilities via
/// [`Self::validate_negotiated_capabilities`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostOutputPayload {
    pub required_capabilities: CapabilitySet,
    pub message: ServerMessage,
}

impl HostOutputPayload {
    /// Construct a host-output carrier. Rejects request/reply/receipt/detach
    /// variants; lane-specific validation runs through
    /// [`Self::validate_for_lane`] or [`ConnectPayload::from_host_output`].
    pub fn new(
        required_capabilities: CapabilitySet,
        message: ServerMessage,
    ) -> Result<Self, PayloadDecodeError> {
        match &message {
            ServerMessage::DurableEvent { .. }
            | ServerMessage::ResyncRequired { .. }
            | ServerMessage::Stream(_)
            | ServerMessage::ConversationDirty { .. } => {}
            ServerMessage::QueryReply(_)
            | ServerMessage::CommandReceipt(_)
            | ServerMessage::TerminalInputAck(_)
            | ServerMessage::UpdateHandoff(_)
            | ServerMessage::Detached(_) => {
                return Err(PayloadDecodeError::Ambiguous {
                    reason:
                        "request, reply, receipt, and detach variants are not host-output payloads",
                });
            }
        }
        Ok(Self {
            required_capabilities,
            message,
        })
    }

    /// Writer-facing intersection check against negotiated session capabilities.
    pub fn validate_negotiated_capabilities(
        &self,
        negotiated: CapabilitySet,
    ) -> Result<(), PayloadDecodeError> {
        if self.required_capabilities.intersection(negotiated) != self.required_capabilities {
            return Err(PayloadDecodeError::Ambiguous {
                reason:
                    "host output required_capabilities are not covered by negotiated capabilities",
            });
        }
        Ok(())
    }

    pub fn validate_for_lane(
        &self,
        lane: HostOutputLane,
        limits: ConnectLimits,
    ) -> Result<(), PayloadDecodeError> {
        limits.validate()?;
        match (lane, &self.message) {
            (HostOutputLane::Durable, ServerMessage::DurableEvent { event, .. }) => {
                if !self.required_capabilities.contains(Capability::EventReplay) {
                    return Err(PayloadDecodeError::Ambiguous {
                        reason: "host durable output requires EventReplay in required_capabilities",
                    });
                }
                // Event identity/sequence bytes are preserved; MessagePack length
                // bounds are enforced when the wrapper is encoded.
                let _ = event;
            }
            (
                HostOutputLane::Critical,
                ServerMessage::ResyncRequired {
                    last_delivered_sequence,
                    newest_sequence,
                    ..
                },
            ) => {
                if !self.required_capabilities.contains(Capability::EventReplay) {
                    return Err(PayloadDecodeError::Ambiguous {
                        reason:
                            "host critical output requires EventReplay in required_capabilities",
                    });
                }
                if *newest_sequence < *last_delivered_sequence {
                    return Err(PayloadDecodeError::Ambiguous {
                        reason: "host critical ResyncRequired newest_sequence is before last_delivered_sequence",
                    });
                }
            }
            (HostOutputLane::Ephemeral, ServerMessage::Stream(frame)) => {
                validate_host_stream_output(self.required_capabilities, frame, limits)?;
            }
            (HostOutputLane::Ephemeral, ServerMessage::ConversationDirty { high_water, .. }) => {
                let required = CapabilitySet::from_capabilities([
                    Capability::TaskCockpit,
                    Capability::SemanticConversation,
                ]);
                if *high_water == 0 || self.required_capabilities != required {
                    return Err(PayloadDecodeError::Ambiguous {
                        reason: "host conversation output requires nonzero high_water and exactly TaskCockpit plus SemanticConversation",
                    });
                }
            }
            (HostOutputLane::Durable, _) => {
                return Err(PayloadDecodeError::Ambiguous {
                    reason: "host durable output accepts only ServerMessage::DurableEvent",
                });
            }
            (HostOutputLane::Critical, _) => {
                return Err(PayloadDecodeError::Ambiguous {
                    reason: "host critical output accepts only ServerMessage::ResyncRequired",
                });
            }
            (HostOutputLane::Ephemeral, _) => {
                return Err(PayloadDecodeError::Ambiguous {
                    reason: "host ephemeral output accepts only ServerMessage::Stream or ConversationDirty",
                });
            }
        }
        Ok(())
    }
}

fn validate_host_stream_output(
    required_capabilities: CapabilitySet,
    frame: &StreamFrame,
    limits: ConnectLimits,
) -> Result<(), PayloadDecodeError> {
    if required_capabilities.bits() == 0 {
        return Err(PayloadDecodeError::Ambiguous {
            reason: "host stream output requires a nonempty explicit capability set",
        });
    }
    // Deliberate boundary: only production-defined StreamPayloadKind values are
    // admitted. Test fixtures that invent terminal kind IDs (for example 1 or 3)
    // are rejected until a production kind is defined.
    if frame.payload_kind != StreamPayloadKind::BROWSER_FRAME {
        return Err(PayloadDecodeError::Ambiguous {
            reason: "host stream output rejects unknown StreamPayloadKind values; only production-defined BROWSER_FRAME (8) is admitted",
        });
    }
    if !required_capabilities.contains(Capability::BrowserProjection) {
        return Err(PayloadDecodeError::Ambiguous {
            reason: "host stream BROWSER_FRAME requires BrowserProjection in required_capabilities",
        });
    }
    limits.validate_payload_len(frame.payload.len())?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectPayload {
    Hello(HelloPayload),
    Capabilities(CapabilitySet),
    SnapshotPage(SnapshotPage),
    EventPage(EventPage),
    Query(QueryEnvelope),
    QueryReply(QueryReply),
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
    HostDurableOutput(HostOutputPayload),
    HostCriticalOutput(HostOutputPayload),
    HostStreamOutput(HostOutputPayload),
    HostConversationOutput(HostOutputPayload),
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
            Self::QueryReply(_) => PayloadKind::QUERY_REPLY,
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
            Self::HostDurableOutput(_) => PayloadKind::HOST_DURABLE_OUTPUT,
            Self::HostCriticalOutput(_) => PayloadKind::HOST_CRITICAL_OUTPUT,
            Self::HostStreamOutput(_) => PayloadKind::HOST_STREAM_OUTPUT,
            Self::HostConversationOutput(_) => PayloadKind::HOST_CONVERSATION_OUTPUT,
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
            | Self::QueryReply(_)
            | Self::Command(_)
            | Self::CommandReceipt(_)
            | Self::OperationSettlement(_)
            | Self::Resync(_)
            | Self::Error(_)
            | Self::HostCriticalOutput(_) => ChannelKind::Critical,
            Self::SnapshotPage(_)
            | Self::EventPage(_)
            | Self::PromptExtension(_)
            | Self::BrowserExtension(_)
            | Self::Chunk(_)
            | Self::Extension(_)
            | Self::HostDurableOutput(_) => ChannelKind::Durable,
            Self::Presence(_)
            | Self::TerminalDelta(_)
            | Self::BrowserFrame(_)
            | Self::HostStreamOutput(_)
            | Self::HostConversationOutput(_) => ChannelKind::Ephemeral,
        }
    }

    pub const fn is_action(&self) -> bool {
        matches!(self, Self::Command(_))
    }

    pub const fn is_host_output(&self) -> bool {
        matches!(
            self,
            Self::HostDurableOutput(_)
                | Self::HostCriticalOutput(_)
                | Self::HostStreamOutput(_)
                | Self::HostConversationOutput(_)
        )
    }

    pub const fn allows_raw_content(&self) -> bool {
        matches!(
            self,
            Self::TerminalDelta(_)
                | Self::BrowserFrame(_)
                | Self::Chunk(_)
                | Self::HostStreamOutput(_)
        )
    }

    pub const fn as_command(&self) -> Option<&CommandEnvelope> {
        match self {
            Self::Command(command) => Some(command),
            _ => None,
        }
    }

    /// Canonical request-lane conversion from the shared host executor.
    ///
    /// Live durable events, streams, and detach acks are not request replies.
    /// Connect must not pretend an unattached output writer delivered them.
    pub fn from_host_server_message(message: ServerMessage) -> Result<Self, PayloadDecodeError> {
        match message {
            ServerMessage::QueryReply(reply) => Ok(Self::QueryReply(reply)),
            ServerMessage::CommandReceipt(receipt) => Ok(Self::CommandReceipt(receipt)),
            ServerMessage::TerminalInputAck(_) => Err(PayloadDecodeError::Ambiguous {
                reason: "terminal input acknowledgements are not Connect request-lane payloads",
            }),
            ServerMessage::UpdateHandoff(_) => Err(PayloadDecodeError::Ambiguous {
                reason: "update handoff replies stay on the authenticated host control lane",
            }),
            ServerMessage::ResyncRequired {
                last_delivered_sequence,
                newest_sequence,
                ..
            } => Ok(Self::Resync(ResyncPayload {
                channel_sequence: last_delivered_sequence,
                newest_sequence,
                reason: ResyncReason::Gap,
            })),
            ServerMessage::DurableEvent { .. }
            | ServerMessage::Stream(_)
            | ServerMessage::ConversationDirty { .. }
            | ServerMessage::Detached(_) => Err(PayloadDecodeError::Ambiguous {
                reason: "host output is not a Connect request-lane payload",
            }),
        }
    }

    /// Lossless unsolicited host-output conversion. Distinct from the lossy
    /// request-lane [`Self::from_host_server_message`] path.
    pub fn from_host_output(
        message: ServerMessage,
        required_capabilities: CapabilitySet,
    ) -> Result<Self, PayloadDecodeError> {
        let host = HostOutputPayload::new(required_capabilities, message)?;
        let payload = match &host.message {
            ServerMessage::DurableEvent { .. } => {
                host.validate_for_lane(HostOutputLane::Durable, ConnectLimits::v1_default())?;
                Self::HostDurableOutput(host)
            }
            ServerMessage::ResyncRequired { .. } => {
                host.validate_for_lane(HostOutputLane::Critical, ConnectLimits::v1_default())?;
                Self::HostCriticalOutput(host)
            }
            ServerMessage::Stream(_) => {
                host.validate_for_lane(HostOutputLane::Ephemeral, ConnectLimits::v1_default())?;
                Self::HostStreamOutput(host)
            }
            ServerMessage::ConversationDirty { .. } => {
                host.validate_for_lane(HostOutputLane::Ephemeral, ConnectLimits::v1_default())?;
                Self::HostConversationOutput(host)
            }
            ServerMessage::QueryReply(_)
            | ServerMessage::CommandReceipt(_)
            | ServerMessage::TerminalInputAck(_)
            | ServerMessage::UpdateHandoff(_)
            | ServerMessage::Detached(_) => {
                return Err(PayloadDecodeError::Ambiguous {
                    reason:
                        "request, reply, receipt, and detach variants are not host-output payloads",
                });
            }
        };
        Ok(payload)
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
            Self::QueryReply(value) => encode_named(value, limits)?,
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
            Self::HostDurableOutput(value)
            | Self::HostCriticalOutput(value)
            | Self::HostStreamOutput(value)
            | Self::HostConversationOutput(value) => encode_named(value, limits)?,
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
            KnownPayloadKind::QueryReply => Self::QueryReply(codec.decode(bytes)?),
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
            KnownPayloadKind::HostDurableOutput => Self::HostDurableOutput(codec.decode(bytes)?),
            KnownPayloadKind::HostCriticalOutput => Self::HostCriticalOutput(codec.decode(bytes)?),
            KnownPayloadKind::HostStreamOutput => Self::HostStreamOutput(codec.decode(bytes)?),
            KnownPayloadKind::HostConversationOutput => {
                Self::HostConversationOutput(codec.decode(bytes)?)
            }
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
            | Self::QueryReply(_)
            | Self::Command(_)
            | Self::CommandReceipt(_)
            | Self::OperationSettlement(_)
            | Self::Presence(_)
            | Self::PromptExtension(_)
            | Self::BrowserExtension(_) => {}
            Self::HostDurableOutput(host) => {
                host.validate_for_lane(HostOutputLane::Durable, limits)?;
            }
            Self::HostCriticalOutput(host) => {
                host.validate_for_lane(HostOutputLane::Critical, limits)?;
            }
            Self::HostStreamOutput(host) => {
                host.validate_for_lane(HostOutputLane::Ephemeral, limits)?;
            }
            Self::HostConversationOutput(host) => {
                host.validate_for_lane(HostOutputLane::Ephemeral, limits)?;
            }
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

fn fixture_subscription(tail: u8) -> crate::domain::id::SubscriptionId {
    crate::domain::id::SubscriptionId::from_bytes(fixture_uuid(tail))
        .expect("canonical subscription id")
}

fn fixture_resource(tail: u8) -> crate::domain::id::ResourceId {
    crate::domain::id::ResourceId::from_bytes(fixture_uuid(tail)).expect("canonical resource id")
}

fn fixture_host_durable() -> HostOutputPayload {
    HostOutputPayload::new(
        CapabilitySet::from_capabilities([Capability::EventReplay]),
        ServerMessage::DurableEvent {
            subscription_id: fixture_subscription(0x71),
            event: crate::domain::DomainEvent {
                id: fixture_event(0x55),
                task_id: Some(fixture_task(0x43)),
                sequence: 3,
                task_revision: Some(1),
                occurred_at_ms: 1,
                payload: crate::domain::Event::TaskReopened,
            },
        },
    )
    .expect("canonical durable host output")
}

fn fixture_host_critical() -> HostOutputPayload {
    HostOutputPayload::new(
        CapabilitySet::from_capabilities([Capability::EventReplay]),
        ServerMessage::ResyncRequired {
            subscription_id: fixture_subscription(0x72),
            last_delivered_sequence: 2,
            newest_sequence: 5,
        },
    )
    .expect("canonical critical host output")
}

fn fixture_host_stream() -> HostOutputPayload {
    HostOutputPayload::new(
        CapabilitySet::from_capabilities([Capability::BrowserProjection]),
        ServerMessage::Stream(crate::protocol::StreamFrame {
            subscription_id: fixture_subscription(0x73),
            stream: crate::protocol::StreamKey::from_resource_id(fixture_resource(0x74)),
            generation: 1,
            sequence: 2,
            payload_kind: StreamPayloadKind::BROWSER_FRAME,
            schema_version: 1,
            payload: vec![0x62, 0x72],
        }),
    )
    .expect("canonical stream host output")
}

fn fixture_host_conversation() -> HostOutputPayload {
    HostOutputPayload::new(
        CapabilitySet::from_capabilities([
            Capability::TaskCockpit,
            Capability::SemanticConversation,
        ]),
        ServerMessage::ConversationDirty {
            subscription_id: fixture_subscription(0x75),
            task_id: fixture_task(0x43),
            high_water: 4,
        },
    )
    .expect("canonical conversation host output")
}

/// Deterministic named payloads used as the Rust schema source of truth.
pub fn native_browser_contract_fixtures() -> Vec<CanonicalSchemaFixture> {
    use crate::domain::cockpit::{ProviderInputStateProjection, TaskCockpitResult};
    use crate::domain::query::QueryResult;
    use crate::domain::snapshot::{
        SemanticJournalFact, SemanticJournalPage, SemanticJournalPayload,
    };
    let mut fixtures = canonical_schema_fixtures()
        .into_iter()
        .filter(|f| matches!(f.payload.kind().get(), 1 | 18 | 19 | 20 | 21 | 22))
        .collect::<Vec<_>>();
    let state = ProviderInputStateProjection {
        task_id: fixture_task(0x43),
        task_revision: 3,
        action_epoch: 4,
        agent_session_id: Some(
            crate::domain::AgentSessionId::from_bytes(fixture_uuid(0x44)).unwrap(),
        ),
        resource_id: Some(crate::domain::ResourceId::from_bytes(fixture_uuid(0x45)).unwrap()),
        runtime_generation: Some(7),
        agent_lifecycle: Some(crate::domain::agent::AgentSessionLifecycle::Open),
        provider_kind: Some(crate::providers::ProviderKind::ClaudeCode),
        provider_session_id: Some(
            crate::domain::agent::ProviderSessionId::new("native-exact-conversation").unwrap(),
        ),
        current_turn: None,
        open_question: None,
        open_approval: None,
        pending_wait_command_ids: Vec::new(),
    };
    fixtures.push(CanonicalSchemaFixture {
        name: "provider_input_state",
        payload: ConnectPayload::QueryReply(QueryReply {
            request_id: fixture_request(0x41),
            outcome: QueryOutcome::Ok(QueryResult::TaskCockpit(
                TaskCockpitResult::ProviderInputState(state),
            )),
        }),
    });
    for (name, reset, more) in [
        ("conversation_final", false, false),
        ("conversation_page", false, true),
        ("conversation_rollover", true, false),
    ] {
        let mut page = SemanticJournalPage {
            oldest_sequence: 1,
            cursor_rolled_over: reset,
            after_sequence: 0,
            through_sequence: if more { 1 } else { 2 },
            high_water: 2,
            encoded_bytes: 0,
            next_sequence: more.then_some(1),
            facts: vec![SemanticJournalFact {
                id: fixture_event(0x55),
                sequence: 1,
                occurred_at_ms: Some(1),
                provider: "claude_code".into(),
                schema_version: 1,
                kind: "assistant_text".into(),
                visibility: "task".into(),
                privacy_class: crate::domain::PrivacyClass::LocalOnly,
                redacted: false,
                payload: SemanticJournalPayload::AssistantText {
                    text: "Native conversation text".into(),
                },
            }],
        };
        page.encoded_bytes = crate::domain::snapshot::canonical_semantic_page_size(&page).unwrap();
        fixtures.push(CanonicalSchemaFixture {
            name,
            payload: ConnectPayload::QueryReply(QueryReply {
                request_id: fixture_request(0x41),
                outcome: QueryOutcome::Ok(QueryResult::TaskCockpit(
                    TaskCockpitResult::Conversation(page),
                )),
            }),
        });
    }
    fixtures
}

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
                client_id: None,
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
            name: "query_reply",
            payload: ConnectPayload::QueryReply(QueryReply {
                request_id: fixture_request(0x41),
                outcome: QueryOutcome::Err(QueryError::NotFound),
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
                request_id: None,
                operation_id: None,
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
        CanonicalSchemaFixture {
            name: "host_durable_output",
            payload: ConnectPayload::HostDurableOutput(fixture_host_durable()),
        },
        CanonicalSchemaFixture {
            name: "host_critical_output",
            payload: ConnectPayload::HostCriticalOutput(fixture_host_critical()),
        },
        CanonicalSchemaFixture {
            name: "host_stream_output",
            payload: ConnectPayload::HostStreamOutput(fixture_host_stream()),
        },
        CanonicalSchemaFixture {
            name: "host_conversation_output",
            payload: ConnectPayload::HostConversationOutput(fixture_host_conversation()),
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
            client_id: None,
        }
    }

    #[test]
    fn hello_boundary_fields_are_additive_and_fail_closed() {
        let encoded = serde_json::to_string(&hello(None, None)).expect("encode");
        assert!(!encoded.contains("relay_url"));
        assert!(!encoded.contains("capability_grant"));
        assert!(!encoded.contains("client_id"));
        let decoded: HelloPayload = serde_json::from_str(&encoded).expect("decode");
        assert_eq!(decoded.relay_url, None);
        assert_eq!(decoded.capability_grant, None);
        assert_eq!(decoded.client_id, None);
        ConnectPayload::Hello(decoded)
            .validate(ConnectLimits::v1_default())
            .expect("omitted optional authority is valid");

        let mut with_client = hello(None, None);
        let supplied = ClientId::new();
        with_client.client_id = Some(supplied);
        let encoded = serde_json::to_string(&with_client).expect("encode");
        assert!(encoded.contains("client_id"));
        let decoded: HelloPayload = serde_json::from_str(&encoded).expect("decode");
        assert_eq!(decoded.client_id, Some(supplied));
    }

    #[test]
    fn error_correlation_fields_are_additive_and_fail_closed() {
        let encoded = serde_json::to_string(&ErrorPayload {
            code: 400,
            message: "bad".to_owned(),
            request_id: None,
            operation_id: None,
        })
        .expect("encode");
        assert!(!encoded.contains("request_id"));
        assert!(!encoded.contains("operation_id"));
        let decoded: ErrorPayload = serde_json::from_str(&encoded).expect("decode");
        assert_eq!(decoded.request_id, None);
        assert_eq!(decoded.operation_id, None);
    }

    #[test]
    fn query_reply_round_trips_and_converts_from_host_server_message() {
        let reply = QueryReply {
            request_id: fixture_request(0x41),
            outcome: QueryOutcome::Err(QueryError::NotFound),
        };
        let payload = ConnectPayload::QueryReply(reply.clone());
        let limits = ConnectLimits::v1_default();
        let bytes = payload.encode(limits).expect("encode");
        let decoded = ConnectPayload::decode(payload.kind(), payload.version(), &bytes, limits)
            .expect("decode");
        assert_eq!(decoded, payload);
        assert_eq!(
            ConnectPayload::from_host_server_message(crate::protocol::ServerMessage::QueryReply(
                reply
            ))
            .expect("convert"),
            payload
        );
    }

    #[test]
    fn hello_rejects_invalid_relay_advertisement() {
        let payload = ConnectPayload::Hello(hello(
            Some("https://relay.example.test/connect".to_owned()),
            None,
        ));
        assert!(payload.validate(ConnectLimits::v1_default()).is_err());
    }

    #[test]
    fn host_output_preserves_exact_server_message_fields() {
        let limits = ConnectLimits::v1_default();
        let durable = ConnectPayload::from_host_output(
            ServerMessage::DurableEvent {
                subscription_id: fixture_subscription(0x71),
                event: crate::domain::DomainEvent {
                    id: fixture_event(0x55),
                    task_id: Some(fixture_task(0x43)),
                    sequence: 3,
                    task_revision: Some(1),
                    occurred_at_ms: 1,
                    payload: crate::domain::Event::TaskReopened,
                },
            },
            CapabilitySet::from_capabilities([Capability::EventReplay]),
        )
        .expect("durable");
        let critical = ConnectPayload::from_host_output(
            ServerMessage::ResyncRequired {
                subscription_id: fixture_subscription(0x72),
                last_delivered_sequence: 2,
                newest_sequence: 5,
            },
            CapabilitySet::from_capabilities([Capability::EventReplay]),
        )
        .expect("critical");
        let stream = ConnectPayload::from_host_output(
            ServerMessage::Stream(crate::protocol::StreamFrame {
                subscription_id: fixture_subscription(0x73),
                stream: crate::protocol::StreamKey::from_resource_id(fixture_resource(0x74)),
                generation: 1,
                sequence: 2,
                payload_kind: StreamPayloadKind::BROWSER_FRAME,
                schema_version: 1,
                payload: vec![0x62, 0x72],
            }),
            CapabilitySet::from_capabilities([Capability::BrowserProjection]),
        )
        .expect("stream");

        for payload in [&durable, &critical, &stream] {
            let bytes = payload.encode(limits).expect("encode");
            let decoded = ConnectPayload::decode(payload.kind(), payload.version(), &bytes, limits)
                .expect("decode");
            assert_eq!(&decoded, payload);
        }

        let ConnectPayload::HostDurableOutput(host) = &durable else {
            panic!("durable wrapper");
        };
        match &host.message {
            ServerMessage::DurableEvent {
                subscription_id,
                event,
            } => {
                assert_eq!(*subscription_id, fixture_subscription(0x71));
                assert_eq!(event.id, fixture_event(0x55));
                assert_eq!(event.sequence, 3);
                assert_eq!(event.task_id, Some(fixture_task(0x43)));
            }
            other => panic!("expected DurableEvent, got {other:?}"),
        }

        let ConnectPayload::HostStreamOutput(host) = &stream else {
            panic!("stream wrapper");
        };
        match &host.message {
            ServerMessage::Stream(frame) => {
                assert_eq!(frame.subscription_id, fixture_subscription(0x73));
                assert_eq!(
                    frame.stream,
                    crate::protocol::StreamKey::from_resource_id(fixture_resource(0x74))
                );
                assert_eq!(frame.generation, 1);
                assert_eq!(frame.sequence, 2);
                assert_eq!(frame.payload_kind, StreamPayloadKind::BROWSER_FRAME);
                assert_eq!(frame.schema_version, 1);
                assert_eq!(frame.payload, vec![0x62, 0x72]);
            }
            other => panic!("expected Stream, got {other:?}"),
        }

        assert!(matches!(
            ConnectPayload::from_host_server_message(ServerMessage::DurableEvent {
                subscription_id: fixture_subscription(0x71),
                event: crate::domain::DomainEvent {
                    id: fixture_event(0x55),
                    task_id: None,
                    sequence: 1,
                    task_revision: None,
                    occurred_at_ms: 1,
                    payload: crate::domain::Event::TaskReopened,
                },
            }),
            Err(PayloadDecodeError::Ambiguous { .. })
        ));
    }

    #[test]
    fn host_output_rejects_wrong_lane_variant_and_request_messages() {
        let durable_message = ServerMessage::DurableEvent {
            subscription_id: fixture_subscription(0x71),
            event: crate::domain::DomainEvent {
                id: fixture_event(0x55),
                task_id: None,
                sequence: 1,
                task_revision: None,
                occurred_at_ms: 1,
                payload: crate::domain::Event::TaskReopened,
            },
        };
        let caps = CapabilitySet::from_capabilities([Capability::EventReplay]);
        let host = HostOutputPayload::new(caps, durable_message.clone()).expect("construct");
        assert!(host
            .validate_for_lane(HostOutputLane::Critical, ConnectLimits::v1_default())
            .is_err());
        assert!(host
            .validate_for_lane(HostOutputLane::Ephemeral, ConnectLimits::v1_default())
            .is_err());

        let mismatched = ConnectPayload::HostCriticalOutput(host);
        assert!(mismatched.validate(ConnectLimits::v1_default()).is_err());

        assert!(HostOutputPayload::new(
            caps,
            ServerMessage::QueryReply(QueryReply {
                request_id: fixture_request(0x41),
                outcome: QueryOutcome::Err(QueryError::NotFound),
            }),
        )
        .is_err());
    }

    #[test]
    fn host_output_rejects_missing_capability_and_negotiated_gap() {
        let message = ServerMessage::DurableEvent {
            subscription_id: fixture_subscription(0x71),
            event: crate::domain::DomainEvent {
                id: fixture_event(0x55),
                task_id: None,
                sequence: 1,
                task_revision: None,
                occurred_at_ms: 1,
                payload: crate::domain::Event::TaskReopened,
            },
        };
        assert!(ConnectPayload::from_host_output(message, CapabilitySet::empty()).is_err());

        let host = fixture_host_durable();
        assert!(host
            .validate_negotiated_capabilities(CapabilitySet::empty())
            .is_err());
        assert!(host
            .validate_negotiated_capabilities(CapabilitySet::from_capabilities([
                Capability::EventReplay
            ]))
            .is_ok());
    }

    #[test]
    fn host_critical_output_rejects_reversed_resync_sequences() {
        let host = HostOutputPayload::new(
            CapabilitySet::from_capabilities([Capability::EventReplay]),
            ServerMessage::ResyncRequired {
                subscription_id: fixture_subscription(0x72),
                last_delivered_sequence: 9,
                newest_sequence: 3,
            },
        )
        .expect("construct");
        assert!(host
            .validate_for_lane(HostOutputLane::Critical, ConnectLimits::v1_default())
            .is_err());
    }

    #[test]
    fn host_stream_output_rejects_unknown_stream_kind() {
        let invented = StreamPayloadKind::new(3).expect("nonzero invented kind");
        let host = HostOutputPayload::new(
            CapabilitySet::from_capabilities([Capability::BrowserProjection]),
            ServerMessage::Stream(crate::protocol::StreamFrame {
                subscription_id: fixture_subscription(0x73),
                stream: crate::protocol::StreamKey::from_resource_id(fixture_resource(0x74)),
                generation: 1,
                sequence: 1,
                payload_kind: invented,
                schema_version: 1,
                payload: vec![0x01],
            }),
        )
        .expect("construct");
        let error = host
            .validate_for_lane(HostOutputLane::Ephemeral, ConnectLimits::v1_default())
            .expect_err("unknown stream kinds stay rejected");
        assert!(matches!(
            error,
            PayloadDecodeError::Ambiguous { reason }
            if reason.contains("BROWSER_FRAME (8)")
        ));
    }

    #[test]
    fn host_stream_output_rejects_oversize_payload() {
        let mut tight = ConnectLimits::v1_default();
        tight.max_reassembled_message_bytes = 64;
        tight.max_physical_frame_bytes = 64;
        tight.max_chunk_bytes = 32;
        tight.max_cumulative_bytes = 64;
        let host = HostOutputPayload::new(
            CapabilitySet::from_capabilities([Capability::BrowserProjection]),
            ServerMessage::Stream(crate::protocol::StreamFrame {
                subscription_id: fixture_subscription(0x73),
                stream: crate::protocol::StreamKey::from_resource_id(fixture_resource(0x74)),
                generation: 1,
                sequence: 1,
                payload_kind: StreamPayloadKind::BROWSER_FRAME,
                schema_version: 1,
                payload: vec![0xab; 128],
            }),
        )
        .expect("construct");
        assert!(host
            .validate_for_lane(HostOutputLane::Ephemeral, tight)
            .is_err());
    }
}
