//! Transport-control messages that are neither domain queries nor durable commands.

use serde::de::{self, MapAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use uuid::Uuid;

use crate::domain::id::RequestId;
use crate::domain::ClientId;

/// Client-initiated request to detach one authenticated connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetachRequest {
    pub request_id: RequestId,
    pub client_id: ClientId,
    pub connection_id: Uuid,
}

const DETACH_REQUEST_FIELDS: &[&str] = &["request_id", "client_id", "connection_id"];

enum DetachRequestField {
    RequestId,
    ClientId,
    ConnectionId,
}

impl<'de> Deserialize<'de> for DetachRequestField {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct FieldVisitor;

        impl Visitor<'_> for FieldVisitor {
            type Value = DetachRequestField;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("request_id, client_id, or connection_id")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                match value {
                    "request_id" => Ok(DetachRequestField::RequestId),
                    "client_id" => Ok(DetachRequestField::ClientId),
                    "connection_id" => Ok(DetachRequestField::ConnectionId),
                    _ => Err(de::Error::unknown_field(value, DETACH_REQUEST_FIELDS)),
                }
            }
        }

        deserializer.deserialize_identifier(FieldVisitor)
    }
}

impl Serialize for DetachRequest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(3))?;
        map.serialize_entry("request_id", &self.request_id)?;
        map.serialize_entry("client_id", &self.client_id)?;
        map.serialize_entry("connection_id", &self.connection_id)?;
        map.end()
    }
}

impl<'de> Deserialize<'de> for DetachRequest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct DetachRequestVisitor;

        impl<'de> Visitor<'de> for DetachRequestVisitor {
            type Value = DetachRequest;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a three-field named DetachRequest map")
            }

            fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                let mut request_id = None;
                let mut client_id = None;
                let mut connection_id = None;
                while let Some(field) = map.next_key()? {
                    match field {
                        DetachRequestField::RequestId => {
                            if request_id.is_some() {
                                return Err(de::Error::duplicate_field("request_id"));
                            }
                            request_id = Some(map.next_value()?);
                        }
                        DetachRequestField::ClientId => {
                            if client_id.is_some() {
                                return Err(de::Error::duplicate_field("client_id"));
                            }
                            client_id = Some(map.next_value()?);
                        }
                        DetachRequestField::ConnectionId => {
                            if connection_id.is_some() {
                                return Err(de::Error::duplicate_field("connection_id"));
                            }
                            connection_id = Some(map.next_value()?);
                        }
                    }
                }
                Ok(DetachRequest {
                    request_id: request_id.ok_or_else(|| de::Error::missing_field("request_id"))?,
                    client_id: client_id.ok_or_else(|| de::Error::missing_field("client_id"))?,
                    connection_id: connection_id
                        .ok_or_else(|| de::Error::missing_field("connection_id"))?,
                })
            }
        }

        deserializer.deserialize_map(DetachRequestVisitor)
    }
}

/// Host acknowledgment that one connection registration was detached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetachAck {
    pub request_id: RequestId,
    pub connection_id: Uuid,
}

const DETACH_ACK_FIELDS: &[&str] = &["request_id", "connection_id"];

enum DetachAckField {
    RequestId,
    ConnectionId,
}

impl<'de> Deserialize<'de> for DetachAckField {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct FieldVisitor;

        impl Visitor<'_> for FieldVisitor {
            type Value = DetachAckField;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("request_id or connection_id")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                match value {
                    "request_id" => Ok(DetachAckField::RequestId),
                    "connection_id" => Ok(DetachAckField::ConnectionId),
                    _ => Err(de::Error::unknown_field(value, DETACH_ACK_FIELDS)),
                }
            }
        }

        deserializer.deserialize_identifier(FieldVisitor)
    }
}

impl Serialize for DetachAck {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(2))?;
        map.serialize_entry("request_id", &self.request_id)?;
        map.serialize_entry("connection_id", &self.connection_id)?;
        map.end()
    }
}

impl<'de> Deserialize<'de> for DetachAck {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct DetachAckVisitor;

        impl<'de> Visitor<'de> for DetachAckVisitor {
            type Value = DetachAck;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a two-field named DetachAck map")
            }

            fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                let mut request_id = None;
                let mut connection_id = None;
                while let Some(field) = map.next_key()? {
                    match field {
                        DetachAckField::RequestId => {
                            if request_id.is_some() {
                                return Err(de::Error::duplicate_field("request_id"));
                            }
                            request_id = Some(map.next_value()?);
                        }
                        DetachAckField::ConnectionId => {
                            if connection_id.is_some() {
                                return Err(de::Error::duplicate_field("connection_id"));
                            }
                            connection_id = Some(map.next_value()?);
                        }
                    }
                }
                Ok(DetachAck {
                    request_id: request_id.ok_or_else(|| de::Error::missing_field("request_id"))?,
                    connection_id: connection_id
                        .ok_or_else(|| de::Error::missing_field("connection_id"))?,
                })
            }
        }

        deserializer.deserialize_map(DetachAckVisitor)
    }
}
