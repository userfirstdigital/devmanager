//! Strict outer request/response maps carried after Hello.

use serde::de::{self, MapAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

use crate::domain::command::{CommandEnvelope, CommandReceipt};
use crate::domain::event::DomainEvent;
use crate::domain::id::{CommandId, SubscriptionId};
use crate::domain::query::{QueryEnvelope, QueryReply};
use crate::protocol::control::{DetachAck, DetachRequest};
use crate::protocol::stream::StreamFrame;
use crate::terminal::protocol::{InputAck, TerminalInputRequest};
use crate::updater::UpdateHandoffToken;

/// One client-initiated request on an authenticated connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientRequest {
    Command(CommandEnvelope),
    TerminalInput(TerminalInputRequest),
    Query(QueryEnvelope),
    Detach(DetachRequest),
}

impl Serialize for ClientRequest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(1))?;
        match self {
            Self::Command(envelope) => map.serialize_entry("command", envelope)?,
            Self::TerminalInput(request) => map.serialize_entry("terminal_input", request)?,
            Self::Query(envelope) => map.serialize_entry("query", envelope)?,
            Self::Detach(request) => map.serialize_entry("detach", request)?,
        }
        map.end()
    }
}

enum ClientRequestVariant {
    Command,
    TerminalInput,
    Query,
    Detach,
}

impl<'de> Deserialize<'de> for ClientRequestVariant {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct VariantVisitor;

        impl Visitor<'_> for VariantVisitor {
            type Value = ClientRequestVariant;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("command, terminal_input, query, or detach")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                match value {
                    "command" => Ok(ClientRequestVariant::Command),
                    "terminal_input" => Ok(ClientRequestVariant::TerminalInput),
                    "query" => Ok(ClientRequestVariant::Query),
                    "detach" => Ok(ClientRequestVariant::Detach),
                    _ => Err(de::Error::unknown_variant(
                        value,
                        &["command", "terminal_input", "query", "detach"],
                    )),
                }
            }
        }

        deserializer.deserialize_identifier(VariantVisitor)
    }
}

impl<'de> Deserialize<'de> for ClientRequest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ClientRequestVisitor;

        impl<'de> Visitor<'de> for ClientRequestVisitor {
            type Value = ClientRequest;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a one-entry named ClientRequest map")
            }

            fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                let variant = map
                    .next_key()?
                    .ok_or_else(|| de::Error::custom("ClientRequest variant is missing"))?;
                let request = match variant {
                    ClientRequestVariant::Command => ClientRequest::Command(map.next_value()?),
                    ClientRequestVariant::TerminalInput => {
                        ClientRequest::TerminalInput(map.next_value()?)
                    }
                    ClientRequestVariant::Query => ClientRequest::Query(map.next_value()?),
                    ClientRequestVariant::Detach => ClientRequest::Detach(map.next_value()?),
                };
                if map.next_key::<de::IgnoredAny>()?.is_some() {
                    return Err(de::Error::custom(
                        "ClientRequest must contain exactly one variant",
                    ));
                }
                Ok(request)
            }
        }

        deserializer.deserialize_map(ClientRequestVisitor)
    }
}

/// Correlated reply to an authenticated PrepareUpdate command.
///
/// The token is the host-owned handoff authority. It is never reconstructed
/// by the client, and its fields are deliberately omitted from `Debug` so a
/// routine protocol diagnostic cannot log the bearer token.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateHandoffReply {
    pub command_id: CommandId,
    pub token: UpdateHandoffToken,
}

impl fmt::Debug for UpdateHandoffReply {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UpdateHandoffReply")
            .field("command_id", &self.command_id)
            .field("token", &"[redacted]")
            .finish()
    }
}

/// One host-originated message on an authenticated connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerMessage {
    CommandReceipt(CommandReceipt),
    TerminalInputAck(InputAck),
    UpdateHandoff(UpdateHandoffReply),
    QueryReply(QueryReply),
    DurableEvent {
        subscription_id: SubscriptionId,
        event: DomainEvent,
    },
    ResyncRequired {
        subscription_id: SubscriptionId,
        last_delivered_sequence: u64,
        newest_sequence: u64,
    },
    Stream(StreamFrame),
    Detached(DetachAck),
}

struct DurableEventPayloadRef<'a> {
    subscription_id: &'a SubscriptionId,
    event: &'a DomainEvent,
}

impl Serialize for DurableEventPayloadRef<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(2))?;
        map.serialize_entry("subscription_id", self.subscription_id)?;
        map.serialize_entry("event", self.event)?;
        map.end()
    }
}

struct ResyncRequiredPayloadRef<'a> {
    subscription_id: &'a SubscriptionId,
    last_delivered_sequence: u64,
    newest_sequence: u64,
}

impl Serialize for ResyncRequiredPayloadRef<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(3))?;
        map.serialize_entry("subscription_id", self.subscription_id)?;
        map.serialize_entry("last_delivered_sequence", &self.last_delivered_sequence)?;
        map.serialize_entry("newest_sequence", &self.newest_sequence)?;
        map.end()
    }
}

impl Serialize for ServerMessage {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(1))?;
        match self {
            Self::CommandReceipt(receipt) => map.serialize_entry("command_receipt", receipt)?,
            Self::TerminalInputAck(ack) => map.serialize_entry("terminal_input_ack", ack)?,
            Self::UpdateHandoff(reply) => map.serialize_entry("update_handoff", reply)?,
            Self::QueryReply(reply) => map.serialize_entry("query_reply", reply)?,
            Self::DurableEvent {
                subscription_id,
                event,
            } => map.serialize_entry(
                "durable_event",
                &DurableEventPayloadRef {
                    subscription_id,
                    event,
                },
            )?,
            Self::ResyncRequired {
                subscription_id,
                last_delivered_sequence,
                newest_sequence,
            } => map.serialize_entry(
                "resync_required",
                &ResyncRequiredPayloadRef {
                    subscription_id,
                    last_delivered_sequence: *last_delivered_sequence,
                    newest_sequence: *newest_sequence,
                },
            )?,
            Self::Stream(frame) => map.serialize_entry("stream", frame)?,
            Self::Detached(ack) => map.serialize_entry("detached", ack)?,
        }
        map.end()
    }
}

enum ServerMessageVariant {
    CommandReceipt,
    TerminalInputAck,
    UpdateHandoff,
    QueryReply,
    DurableEvent,
    ResyncRequired,
    Stream,
    Detached,
}

impl<'de> Deserialize<'de> for ServerMessageVariant {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct VariantVisitor;

        impl Visitor<'_> for VariantVisitor {
            type Value = ServerMessageVariant;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("command_receipt, terminal_input_ack, update_handoff, query_reply, durable_event, resync_required, stream, or detached")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                match value {
                    "command_receipt" => Ok(ServerMessageVariant::CommandReceipt),
                    "terminal_input_ack" => Ok(ServerMessageVariant::TerminalInputAck),
                    "update_handoff" => Ok(ServerMessageVariant::UpdateHandoff),
                    "query_reply" => Ok(ServerMessageVariant::QueryReply),
                    "durable_event" => Ok(ServerMessageVariant::DurableEvent),
                    "resync_required" => Ok(ServerMessageVariant::ResyncRequired),
                    "stream" => Ok(ServerMessageVariant::Stream),
                    "detached" => Ok(ServerMessageVariant::Detached),
                    _ => Err(de::Error::unknown_variant(
                        value,
                        &[
                            "command_receipt",
                            "terminal_input_ack",
                            "update_handoff",
                            "query_reply",
                            "durable_event",
                            "resync_required",
                            "stream",
                            "detached",
                        ],
                    )),
                }
            }
        }

        deserializer.deserialize_identifier(VariantVisitor)
    }
}

enum DurableEventField {
    SubscriptionId,
    Event,
}

impl<'de> Deserialize<'de> for DurableEventField {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct FieldVisitor;

        impl Visitor<'_> for FieldVisitor {
            type Value = DurableEventField;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("subscription_id or event")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                match value {
                    "subscription_id" => Ok(DurableEventField::SubscriptionId),
                    "event" => Ok(DurableEventField::Event),
                    _ => Err(de::Error::unknown_field(
                        value,
                        &["subscription_id", "event"],
                    )),
                }
            }
        }

        deserializer.deserialize_identifier(FieldVisitor)
    }
}

struct DurableEventPayload {
    subscription_id: SubscriptionId,
    event: DomainEvent,
}

impl<'de> Deserialize<'de> for DurableEventPayload {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct PayloadVisitor;

        impl<'de> Visitor<'de> for PayloadVisitor {
            type Value = DurableEventPayload;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a named durable_event payload map")
            }

            fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                let mut subscription_id = None;
                let mut event = None;
                while let Some(field) = map.next_key()? {
                    match field {
                        DurableEventField::SubscriptionId => {
                            if subscription_id.is_some() {
                                return Err(de::Error::duplicate_field("subscription_id"));
                            }
                            subscription_id = Some(map.next_value()?);
                        }
                        DurableEventField::Event => {
                            if event.is_some() {
                                return Err(de::Error::duplicate_field("event"));
                            }
                            event = Some(map.next_value()?);
                        }
                    }
                }
                Ok(DurableEventPayload {
                    subscription_id: subscription_id
                        .ok_or_else(|| de::Error::missing_field("subscription_id"))?,
                    event: event.ok_or_else(|| de::Error::missing_field("event"))?,
                })
            }
        }

        deserializer.deserialize_map(PayloadVisitor)
    }
}

enum ResyncRequiredField {
    SubscriptionId,
    LastDeliveredSequence,
    NewestSequence,
}

impl<'de> Deserialize<'de> for ResyncRequiredField {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct FieldVisitor;

        impl Visitor<'_> for FieldVisitor {
            type Value = ResyncRequiredField;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("subscription_id, last_delivered_sequence, or newest_sequence")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                match value {
                    "subscription_id" => Ok(ResyncRequiredField::SubscriptionId),
                    "last_delivered_sequence" => Ok(ResyncRequiredField::LastDeliveredSequence),
                    "newest_sequence" => Ok(ResyncRequiredField::NewestSequence),
                    _ => Err(de::Error::unknown_field(
                        value,
                        &[
                            "subscription_id",
                            "last_delivered_sequence",
                            "newest_sequence",
                        ],
                    )),
                }
            }
        }

        deserializer.deserialize_identifier(FieldVisitor)
    }
}

struct ResyncRequiredPayload {
    subscription_id: SubscriptionId,
    last_delivered_sequence: u64,
    newest_sequence: u64,
}

impl<'de> Deserialize<'de> for ResyncRequiredPayload {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct PayloadVisitor;

        impl<'de> Visitor<'de> for PayloadVisitor {
            type Value = ResyncRequiredPayload;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a named resync_required payload map")
            }

            fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                let mut subscription_id = None;
                let mut last_delivered_sequence = None;
                let mut newest_sequence = None;
                while let Some(field) = map.next_key()? {
                    match field {
                        ResyncRequiredField::SubscriptionId => {
                            if subscription_id.is_some() {
                                return Err(de::Error::duplicate_field("subscription_id"));
                            }
                            subscription_id = Some(map.next_value()?);
                        }
                        ResyncRequiredField::LastDeliveredSequence => {
                            if last_delivered_sequence.is_some() {
                                return Err(de::Error::duplicate_field("last_delivered_sequence"));
                            }
                            last_delivered_sequence = Some(map.next_value()?);
                        }
                        ResyncRequiredField::NewestSequence => {
                            if newest_sequence.is_some() {
                                return Err(de::Error::duplicate_field("newest_sequence"));
                            }
                            newest_sequence = Some(map.next_value()?);
                        }
                    }
                }
                Ok(ResyncRequiredPayload {
                    subscription_id: subscription_id
                        .ok_or_else(|| de::Error::missing_field("subscription_id"))?,
                    last_delivered_sequence: last_delivered_sequence
                        .ok_or_else(|| de::Error::missing_field("last_delivered_sequence"))?,
                    newest_sequence: newest_sequence
                        .ok_or_else(|| de::Error::missing_field("newest_sequence"))?,
                })
            }
        }

        deserializer.deserialize_map(PayloadVisitor)
    }
}

impl<'de> Deserialize<'de> for ServerMessage {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ServerMessageVisitor;

        impl<'de> Visitor<'de> for ServerMessageVisitor {
            type Value = ServerMessage;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a one-entry named ServerMessage map")
            }

            fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                let variant = map
                    .next_key()?
                    .ok_or_else(|| de::Error::custom("ServerMessage variant is missing"))?;
                let message = match variant {
                    ServerMessageVariant::CommandReceipt => {
                        ServerMessage::CommandReceipt(map.next_value()?)
                    }
                    ServerMessageVariant::TerminalInputAck => {
                        ServerMessage::TerminalInputAck(map.next_value()?)
                    }
                    ServerMessageVariant::UpdateHandoff => {
                        ServerMessage::UpdateHandoff(map.next_value()?)
                    }
                    ServerMessageVariant::QueryReply => {
                        ServerMessage::QueryReply(map.next_value()?)
                    }
                    ServerMessageVariant::DurableEvent => {
                        let payload: DurableEventPayload = map.next_value()?;
                        ServerMessage::DurableEvent {
                            subscription_id: payload.subscription_id,
                            event: payload.event,
                        }
                    }
                    ServerMessageVariant::ResyncRequired => {
                        let payload: ResyncRequiredPayload = map.next_value()?;
                        ServerMessage::ResyncRequired {
                            subscription_id: payload.subscription_id,
                            last_delivered_sequence: payload.last_delivered_sequence,
                            newest_sequence: payload.newest_sequence,
                        }
                    }
                    ServerMessageVariant::Stream => ServerMessage::Stream(map.next_value()?),
                    ServerMessageVariant::Detached => ServerMessage::Detached(map.next_value()?),
                };
                if map.next_key::<de::IgnoredAny>()?.is_some() {
                    return Err(de::Error::custom(
                        "ServerMessage must contain exactly one variant",
                    ));
                }
                Ok(message)
            }
        }

        deserializer.deserialize_map(ServerMessageVisitor)
    }
}
