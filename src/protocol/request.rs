//! Strict outer request/response maps carried after Hello.

use serde::de::{self, MapAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::domain::command::{CommandEnvelope, CommandReceipt};
use crate::domain::query::{QueryEnvelope, QueryReply};

/// One client-initiated request on an authenticated connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientRequest {
    Command(CommandEnvelope),
    Query(QueryEnvelope),
}

impl Serialize for ClientRequest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(1))?;
        match self {
            Self::Command(envelope) => map.serialize_entry("command", envelope)?,
            Self::Query(envelope) => map.serialize_entry("query", envelope)?,
        }
        map.end()
    }
}

enum ClientRequestVariant {
    Command,
    Query,
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
                formatter.write_str("command or query")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                match value {
                    "command" => Ok(ClientRequestVariant::Command),
                    "query" => Ok(ClientRequestVariant::Query),
                    _ => Err(de::Error::unknown_variant(value, &["command", "query"])),
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
                    ClientRequestVariant::Query => ClientRequest::Query(map.next_value()?),
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

/// One host response to a [`ClientRequest`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerResponse {
    CommandReceipt(CommandReceipt),
    QueryReply(QueryReply),
}

impl Serialize for ServerResponse {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(1))?;
        match self {
            Self::CommandReceipt(receipt) => map.serialize_entry("command_receipt", receipt)?,
            Self::QueryReply(reply) => map.serialize_entry("query_reply", reply)?,
        }
        map.end()
    }
}

enum ServerResponseVariant {
    CommandReceipt,
    QueryReply,
}

impl<'de> Deserialize<'de> for ServerResponseVariant {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct VariantVisitor;

        impl Visitor<'_> for VariantVisitor {
            type Value = ServerResponseVariant;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("command_receipt or query_reply")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                match value {
                    "command_receipt" => Ok(ServerResponseVariant::CommandReceipt),
                    "query_reply" => Ok(ServerResponseVariant::QueryReply),
                    _ => Err(de::Error::unknown_variant(
                        value,
                        &["command_receipt", "query_reply"],
                    )),
                }
            }
        }

        deserializer.deserialize_identifier(VariantVisitor)
    }
}

impl<'de> Deserialize<'de> for ServerResponse {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ServerResponseVisitor;

        impl<'de> Visitor<'de> for ServerResponseVisitor {
            type Value = ServerResponse;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a one-entry named ServerResponse map")
            }

            fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                let variant = map
                    .next_key()?
                    .ok_or_else(|| de::Error::custom("ServerResponse variant is missing"))?;
                let response = match variant {
                    ServerResponseVariant::CommandReceipt => {
                        ServerResponse::CommandReceipt(map.next_value()?)
                    }
                    ServerResponseVariant::QueryReply => {
                        ServerResponse::QueryReply(map.next_value()?)
                    }
                };
                if map.next_key::<de::IgnoredAny>()?.is_some() {
                    return Err(de::Error::custom(
                        "ServerResponse must contain exactly one variant",
                    ));
                }
                Ok(response)
            }
        }

        deserializer.deserialize_map(ServerResponseVisitor)
    }
}
