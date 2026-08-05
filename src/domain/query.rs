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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryResult {
    OperationStatus {
        operation_id: OperationId,
        state: OperationState,
    },
}

struct OperationStatusResultRef<'a> {
    operation_id: &'a OperationId,
    state: &'a OperationState,
}

impl Serialize for OperationStatusResultRef<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(2))?;
        map.serialize_entry("operation_id", self.operation_id)?;
        map.serialize_entry("state", self.state)?;
        map.end()
    }
}

impl Serialize for QueryResult {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(1))?;
        match self {
            Self::OperationStatus {
                operation_id,
                state,
            } => map.serialize_entry(
                "operation_status",
                &OperationStatusResultRef {
                    operation_id,
                    state,
                },
            )?,
        }
        map.end()
    }
}

enum QueryResultVariant {
    OperationStatus,
}

impl<'de> Deserialize<'de> for QueryResultVariant {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct VariantVisitor;

        impl Visitor<'_> for VariantVisitor {
            type Value = QueryResultVariant;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("operation_status")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                match value {
                    "operation_status" => Ok(QueryResultVariant::OperationStatus),
                    _ => Err(de::Error::unknown_variant(value, &["operation_status"])),
                }
            }
        }

        deserializer.deserialize_identifier(VariantVisitor)
    }
}

enum OperationStatusResultField {
    OperationId,
    State,
}

impl<'de> Deserialize<'de> for OperationStatusResultField {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct FieldVisitor;

        impl Visitor<'_> for FieldVisitor {
            type Value = OperationStatusResultField;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("operation_id or state")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                match value {
                    "operation_id" => Ok(OperationStatusResultField::OperationId),
                    "state" => Ok(OperationStatusResultField::State),
                    _ => Err(de::Error::unknown_field(value, &["operation_id", "state"])),
                }
            }
        }

        deserializer.deserialize_identifier(FieldVisitor)
    }
}

struct OperationStatusResultPayload {
    operation_id: OperationId,
    state: OperationState,
}

impl<'de> Deserialize<'de> for OperationStatusResultPayload {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct PayloadVisitor;

        impl<'de> Visitor<'de> for PayloadVisitor {
            type Value = OperationStatusResultPayload;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a named operation_status result payload map")
            }

            fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                let mut operation_id = None;
                let mut state = None;

                while let Some(field) = map.next_key()? {
                    match field {
                        OperationStatusResultField::OperationId => {
                            if operation_id.is_some() {
                                return Err(de::Error::duplicate_field("operation_id"));
                            }
                            operation_id = Some(map.next_value()?);
                        }
                        OperationStatusResultField::State => {
                            if state.is_some() {
                                return Err(de::Error::duplicate_field("state"));
                            }
                            state = Some(map.next_value()?);
                        }
                    }
                }

                Ok(OperationStatusResultPayload {
                    operation_id: operation_id
                        .ok_or_else(|| de::Error::missing_field("operation_id"))?,
                    state: state.ok_or_else(|| de::Error::missing_field("state"))?,
                })
            }
        }

        deserializer.deserialize_map(PayloadVisitor)
    }
}

impl<'de> Deserialize<'de> for QueryResult {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct QueryResultVisitor;

        impl<'de> Visitor<'de> for QueryResultVisitor {
            type Value = QueryResult;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a one-entry named QueryResult map")
            }

            fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                let variant = map
                    .next_key()?
                    .ok_or_else(|| de::Error::custom("QueryResult variant is missing"))?;
                let result = match variant {
                    QueryResultVariant::OperationStatus => {
                        let payload: OperationStatusResultPayload = map.next_value()?;
                        QueryResult::OperationStatus {
                            operation_id: payload.operation_id,
                            state: payload.state,
                        }
                    }
                };
                if map.next_key::<de::IgnoredAny>()?.is_some() {
                    return Err(de::Error::custom(
                        "QueryResult must contain exactly one variant",
                    ));
                }
                Ok(result)
            }
        }

        deserializer.deserialize_map(QueryResultVisitor)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryError {
    NotFound,
    Unauthorized,
    InvalidRequest,
    UnsupportedCapability,
}

impl Serialize for QueryError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(match self {
            Self::NotFound => "not_found",
            Self::Unauthorized => "unauthorized",
            Self::InvalidRequest => "invalid_request",
            Self::UnsupportedCapability => "unsupported_capability",
        })
    }
}

impl<'de> Deserialize<'de> for QueryError {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct QueryErrorVisitor;

        impl Visitor<'_> for QueryErrorVisitor {
            type Value = QueryError;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a named QueryError code")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                match value {
                    "not_found" => Ok(QueryError::NotFound),
                    "unauthorized" => Ok(QueryError::Unauthorized),
                    "invalid_request" => Ok(QueryError::InvalidRequest),
                    "unsupported_capability" => Ok(QueryError::UnsupportedCapability),
                    _ => Err(de::Error::unknown_variant(
                        value,
                        &[
                            "not_found",
                            "unauthorized",
                            "invalid_request",
                            "unsupported_capability",
                        ],
                    )),
                }
            }
        }

        deserializer.deserialize_str(QueryErrorVisitor)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryReply {
    pub request_id: RequestId,
    pub outcome: QueryOutcome,
}

impl Serialize for QueryReply {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(2))?;
        map.serialize_entry("request_id", &self.request_id)?;
        map.serialize_entry("outcome", &self.outcome)?;
        map.end()
    }
}

enum QueryReplyField {
    RequestId,
    Outcome,
}

impl<'de> Deserialize<'de> for QueryReplyField {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct FieldVisitor;

        impl Visitor<'_> for FieldVisitor {
            type Value = QueryReplyField;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("request_id or outcome")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                match value {
                    "request_id" => Ok(QueryReplyField::RequestId),
                    "outcome" => Ok(QueryReplyField::Outcome),
                    _ => Err(de::Error::unknown_field(value, &["request_id", "outcome"])),
                }
            }
        }

        deserializer.deserialize_identifier(FieldVisitor)
    }
}

impl<'de> Deserialize<'de> for QueryReply {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct QueryReplyVisitor;

        impl<'de> Visitor<'de> for QueryReplyVisitor {
            type Value = QueryReply;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a named QueryReply map")
            }

            fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                let mut request_id = None;
                let mut outcome = None;

                while let Some(field) = map.next_key()? {
                    match field {
                        QueryReplyField::RequestId => {
                            if request_id.is_some() {
                                return Err(de::Error::duplicate_field("request_id"));
                            }
                            request_id = Some(map.next_value()?);
                        }
                        QueryReplyField::Outcome => {
                            if outcome.is_some() {
                                return Err(de::Error::duplicate_field("outcome"));
                            }
                            outcome = Some(map.next_value()?);
                        }
                    }
                }

                Ok(QueryReply {
                    request_id: request_id.ok_or_else(|| de::Error::missing_field("request_id"))?,
                    outcome: outcome.ok_or_else(|| de::Error::missing_field("outcome"))?,
                })
            }
        }

        deserializer.deserialize_map(QueryReplyVisitor)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryOutcome {
    Ok(QueryResult),
    Err(QueryError),
}

impl Serialize for QueryOutcome {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(1))?;
        match self {
            Self::Ok(result) => map.serialize_entry("ok", result)?,
            Self::Err(error) => map.serialize_entry("err", error)?,
        }
        map.end()
    }
}

enum QueryOutcomeVariant {
    Ok,
    Err,
}

impl<'de> Deserialize<'de> for QueryOutcomeVariant {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct VariantVisitor;

        impl Visitor<'_> for VariantVisitor {
            type Value = QueryOutcomeVariant;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("ok or err")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                match value {
                    "ok" => Ok(QueryOutcomeVariant::Ok),
                    "err" => Ok(QueryOutcomeVariant::Err),
                    _ => Err(de::Error::unknown_variant(value, &["ok", "err"])),
                }
            }
        }

        deserializer.deserialize_identifier(VariantVisitor)
    }
}

impl<'de> Deserialize<'de> for QueryOutcome {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct QueryOutcomeVisitor;

        impl<'de> Visitor<'de> for QueryOutcomeVisitor {
            type Value = QueryOutcome;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a one-entry named QueryOutcome map")
            }

            fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                let variant = map
                    .next_key()?
                    .ok_or_else(|| de::Error::custom("QueryOutcome variant is missing"))?;
                let outcome = match variant {
                    QueryOutcomeVariant::Ok => QueryOutcome::Ok(map.next_value()?),
                    QueryOutcomeVariant::Err => QueryOutcome::Err(map.next_value()?),
                };
                if map.next_key::<de::IgnoredAny>()?.is_some() {
                    return Err(de::Error::custom(
                        "QueryOutcome must contain exactly one variant",
                    ));
                }
                Ok(outcome)
            }
        }

        deserializer.deserialize_map(QueryOutcomeVisitor)
    }
}
