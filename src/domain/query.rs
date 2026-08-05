use serde::de::{self, MapAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::domain::id::{ClientId, OperationId, RequestId, TaskId};
use crate::domain::operation::OperationState;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryEnvelope {
    pub request_id: RequestId,
    pub client_id: ClientId,
    pub task_id: Option<TaskId>,
    pub query: Query,
}

impl Serialize for QueryEnvelope {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(4))?;
        map.serialize_entry("request_id", &self.request_id)?;
        map.serialize_entry("client_id", &self.client_id)?;
        map.serialize_entry("task_id", &self.task_id)?;
        map.serialize_entry("query", &self.query)?;
        map.end()
    }
}

const QUERY_ENVELOPE_FIELDS: &[&str] = &["request_id", "client_id", "task_id", "query"];

enum QueryEnvelopeField {
    RequestId,
    ClientId,
    TaskId,
    Query,
}

impl<'de> Deserialize<'de> for QueryEnvelopeField {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct FieldVisitor;

        impl Visitor<'_> for FieldVisitor {
            type Value = QueryEnvelopeField;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a QueryEnvelope field name")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                match value {
                    "request_id" => Ok(QueryEnvelopeField::RequestId),
                    "client_id" => Ok(QueryEnvelopeField::ClientId),
                    "task_id" => Ok(QueryEnvelopeField::TaskId),
                    "query" => Ok(QueryEnvelopeField::Query),
                    _ => Err(de::Error::unknown_field(value, QUERY_ENVELOPE_FIELDS)),
                }
            }
        }

        deserializer.deserialize_identifier(FieldVisitor)
    }
}

impl<'de> Deserialize<'de> for QueryEnvelope {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct QueryEnvelopeVisitor;

        impl<'de> Visitor<'de> for QueryEnvelopeVisitor {
            type Value = QueryEnvelope;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a named QueryEnvelope map")
            }

            fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                let mut request_id = None;
                let mut client_id = None;
                let mut task_id: Option<Option<TaskId>> = None;
                let mut query = None;

                while let Some(field) = map.next_key()? {
                    match field {
                        QueryEnvelopeField::RequestId => {
                            if request_id.is_some() {
                                return Err(de::Error::duplicate_field("request_id"));
                            }
                            request_id = Some(map.next_value()?);
                        }
                        QueryEnvelopeField::ClientId => {
                            if client_id.is_some() {
                                return Err(de::Error::duplicate_field("client_id"));
                            }
                            client_id = Some(map.next_value()?);
                        }
                        QueryEnvelopeField::TaskId => {
                            if task_id.is_some() {
                                return Err(de::Error::duplicate_field("task_id"));
                            }
                            task_id = Some(map.next_value()?);
                        }
                        QueryEnvelopeField::Query => {
                            if query.is_some() {
                                return Err(de::Error::duplicate_field("query"));
                            }
                            query = Some(map.next_value()?);
                        }
                    }
                }

                Ok(QueryEnvelope {
                    request_id: request_id.ok_or_else(|| de::Error::missing_field("request_id"))?,
                    client_id: client_id.ok_or_else(|| de::Error::missing_field("client_id"))?,
                    task_id: task_id.ok_or_else(|| de::Error::missing_field("task_id"))?,
                    query: query.ok_or_else(|| de::Error::missing_field("query"))?,
                })
            }
        }

        deserializer.deserialize_map(QueryEnvelopeVisitor)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Query {
    OperationStatus { operation_id: OperationId },
}

struct OperationStatusQueryRef<'a> {
    operation_id: &'a OperationId,
}

impl Serialize for OperationStatusQueryRef<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(1))?;
        map.serialize_entry("operation_id", self.operation_id)?;
        map.end()
    }
}

impl Serialize for Query {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(1))?;
        match self {
            Self::OperationStatus { operation_id } => map.serialize_entry(
                "operation_status",
                &OperationStatusQueryRef { operation_id },
            )?,
        }
        map.end()
    }
}

enum QueryVariant {
    OperationStatus,
}

impl<'de> Deserialize<'de> for QueryVariant {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct VariantVisitor;

        impl Visitor<'_> for VariantVisitor {
            type Value = QueryVariant;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("operation_status")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                match value {
                    "operation_status" => Ok(QueryVariant::OperationStatus),
                    _ => Err(de::Error::unknown_variant(value, &["operation_status"])),
                }
            }
        }

        deserializer.deserialize_identifier(VariantVisitor)
    }
}

struct OperationStatusQueryPayload {
    operation_id: OperationId,
}

enum OperationStatusQueryField {
    OperationId,
}

impl<'de> Deserialize<'de> for OperationStatusQueryField {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct FieldVisitor;

        impl Visitor<'_> for FieldVisitor {
            type Value = OperationStatusQueryField;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("operation_id")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                match value {
                    "operation_id" => Ok(OperationStatusQueryField::OperationId),
                    _ => Err(de::Error::unknown_field(value, &["operation_id"])),
                }
            }
        }

        deserializer.deserialize_identifier(FieldVisitor)
    }
}

impl<'de> Deserialize<'de> for OperationStatusQueryPayload {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct PayloadVisitor;

        impl<'de> Visitor<'de> for PayloadVisitor {
            type Value = OperationStatusQueryPayload;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a named operation_status query payload map")
            }

            fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                let mut operation_id = None;
                while let Some(field) = map.next_key()? {
                    match field {
                        OperationStatusQueryField::OperationId => {
                            if operation_id.is_some() {
                                return Err(de::Error::duplicate_field("operation_id"));
                            }
                            operation_id = Some(map.next_value()?);
                        }
                    }
                }
                Ok(OperationStatusQueryPayload {
                    operation_id: operation_id
                        .ok_or_else(|| de::Error::missing_field("operation_id"))?,
                })
            }
        }

        deserializer.deserialize_map(PayloadVisitor)
    }
}

impl<'de> Deserialize<'de> for Query {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct QueryVisitor;

        impl<'de> Visitor<'de> for QueryVisitor {
            type Value = Query;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a one-entry named Query map")
            }

            fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                let variant = map
                    .next_key()?
                    .ok_or_else(|| de::Error::custom("Query variant is missing"))?;
                let query = match variant {
                    QueryVariant::OperationStatus => {
                        let payload: OperationStatusQueryPayload = map.next_value()?;
                        Query::OperationStatus {
                            operation_id: payload.operation_id,
                        }
                    }
                };
                if map.next_key::<de::IgnoredAny>()?.is_some() {
                    return Err(de::Error::custom("Query must contain exactly one variant"));
                }
                Ok(query)
            }
        }

        deserializer.deserialize_map(QueryVisitor)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryResult {
    OperationStatus {
        operation_id: OperationId,
        state: OperationState,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryError {
    NotFound,
    Unauthorized,
    InvalidRequest,
    UnsupportedCapability,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryReply {
    pub request_id: RequestId,
    pub outcome: QueryOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryOutcome {
    Ok(QueryResult),
    Err(QueryError),
}
