use serde::de::{self, MapAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::domain::id::{ClientId, OperationId, RequestId, SnapshotId, SubscriptionId, TaskId};
use crate::domain::operation::OperationState;
use crate::domain::snapshot::{EventPage, SnapshotPage, SnapshotSection, TaskSnapshotItem};

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
    OperationStatus {
        operation_id: OperationId,
    },
    /// Task scope is taken from [`QueryEnvelope::task_id`].
    TaskSnapshot,
    /// Open (`snapshot_id` and `resume_cursor` both absent) or resume (both present).
    SnapshotPage {
        section: SnapshotSection,
        snapshot_id: Option<SnapshotId>,
        resume_cursor: Option<Vec<u8>>,
    },
    ReleaseSnapshot {
        snapshot_id: SnapshotId,
    },
    OpenEventReplay {
        after_sequence: u64,
    },
    ContinueEventReplay {
        subscription_id: SubscriptionId,
        resume_cursor: Vec<u8>,
    },
    ReleaseEventReplay {
        subscription_id: SubscriptionId,
    },
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

struct EmptyNamedMap;

impl Serialize for EmptyNamedMap {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_map(Some(0))?.end()
    }
}

struct SnapshotPageQueryRef<'a> {
    section: &'a SnapshotSection,
    snapshot_id: &'a Option<SnapshotId>,
    resume_cursor: &'a Option<Vec<u8>>,
}

impl Serialize for SnapshotPageQueryRef<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(3))?;
        map.serialize_entry("section", self.section)?;
        map.serialize_entry("snapshot_id", self.snapshot_id)?;
        map.serialize_entry("resume_cursor", self.resume_cursor)?;
        map.end()
    }
}

struct ReleaseSnapshotQueryRef<'a> {
    snapshot_id: &'a SnapshotId,
}

impl Serialize for ReleaseSnapshotQueryRef<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(1))?;
        map.serialize_entry("snapshot_id", self.snapshot_id)?;
        map.end()
    }
}

struct OpenEventReplayQueryRef {
    after_sequence: u64,
}

impl Serialize for OpenEventReplayQueryRef {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(1))?;
        map.serialize_entry("after_sequence", &self.after_sequence)?;
        map.end()
    }
}

struct ContinueEventReplayQueryRef<'a> {
    subscription_id: &'a SubscriptionId,
    resume_cursor: &'a [u8],
}

impl Serialize for ContinueEventReplayQueryRef<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(2))?;
        map.serialize_entry("subscription_id", self.subscription_id)?;
        map.serialize_entry("resume_cursor", self.resume_cursor)?;
        map.end()
    }
}

struct ReleaseEventReplayQueryRef<'a> {
    subscription_id: &'a SubscriptionId,
}

impl Serialize for ReleaseEventReplayQueryRef<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(1))?;
        map.serialize_entry("subscription_id", self.subscription_id)?;
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
            Self::TaskSnapshot => map.serialize_entry("task_snapshot", &EmptyNamedMap)?,
            Self::SnapshotPage {
                section,
                snapshot_id,
                resume_cursor,
            } => map.serialize_entry(
                "snapshot_page",
                &SnapshotPageQueryRef {
                    section,
                    snapshot_id,
                    resume_cursor,
                },
            )?,
            Self::ReleaseSnapshot { snapshot_id } => {
                map.serialize_entry("release_snapshot", &ReleaseSnapshotQueryRef { snapshot_id })?
            }
            Self::OpenEventReplay { after_sequence } => map.serialize_entry(
                "open_event_replay",
                &OpenEventReplayQueryRef {
                    after_sequence: *after_sequence,
                },
            )?,
            Self::ContinueEventReplay {
                subscription_id,
                resume_cursor,
            } => map.serialize_entry(
                "continue_event_replay",
                &ContinueEventReplayQueryRef {
                    subscription_id,
                    resume_cursor,
                },
            )?,
            Self::ReleaseEventReplay { subscription_id } => map.serialize_entry(
                "release_event_replay",
                &ReleaseEventReplayQueryRef { subscription_id },
            )?,
        }
        map.end()
    }
}

enum QueryVariant {
    OperationStatus,
    TaskSnapshot,
    SnapshotPage,
    ReleaseSnapshot,
    OpenEventReplay,
    ContinueEventReplay,
    ReleaseEventReplay,
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
                formatter.write_str(
                    "operation_status, task_snapshot, snapshot_page, release_snapshot, open_event_replay, continue_event_replay, or release_event_replay",
                )
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                match value {
                    "operation_status" => Ok(QueryVariant::OperationStatus),
                    "task_snapshot" => Ok(QueryVariant::TaskSnapshot),
                    "snapshot_page" => Ok(QueryVariant::SnapshotPage),
                    "release_snapshot" => Ok(QueryVariant::ReleaseSnapshot),
                    "open_event_replay" => Ok(QueryVariant::OpenEventReplay),
                    "continue_event_replay" => Ok(QueryVariant::ContinueEventReplay),
                    "release_event_replay" => Ok(QueryVariant::ReleaseEventReplay),
                    _ => Err(de::Error::unknown_variant(
                        value,
                        &[
                            "operation_status",
                            "task_snapshot",
                            "snapshot_page",
                            "release_snapshot",
                            "open_event_replay",
                            "continue_event_replay",
                            "release_event_replay",
                        ],
                    )),
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

impl<'de> Deserialize<'de> for EmptyNamedMap {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct EmptyMapVisitor;

        impl<'de> Visitor<'de> for EmptyMapVisitor {
            type Value = EmptyNamedMap;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("an empty named map")
            }

            fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                if let Some(key) = map.next_key::<String>()? {
                    return Err(de::Error::unknown_field(&key, &[]));
                }
                Ok(EmptyNamedMap)
            }
        }

        deserializer.deserialize_map(EmptyMapVisitor)
    }
}

struct SnapshotPageQueryPayload {
    section: SnapshotSection,
    snapshot_id: Option<SnapshotId>,
    resume_cursor: Option<Vec<u8>>,
}

enum SnapshotPageQueryField {
    Section,
    SnapshotId,
    ResumeCursor,
}

impl<'de> Deserialize<'de> for SnapshotPageQueryField {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct FieldVisitor;

        impl Visitor<'_> for FieldVisitor {
            type Value = SnapshotPageQueryField;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("section, snapshot_id, or resume_cursor")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                match value {
                    "section" => Ok(SnapshotPageQueryField::Section),
                    "snapshot_id" => Ok(SnapshotPageQueryField::SnapshotId),
                    "resume_cursor" => Ok(SnapshotPageQueryField::ResumeCursor),
                    _ => Err(de::Error::unknown_field(
                        value,
                        &["section", "snapshot_id", "resume_cursor"],
                    )),
                }
            }
        }

        deserializer.deserialize_identifier(FieldVisitor)
    }
}

impl<'de> Deserialize<'de> for SnapshotPageQueryPayload {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct PayloadVisitor;

        impl<'de> Visitor<'de> for PayloadVisitor {
            type Value = SnapshotPageQueryPayload;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a named snapshot_page query payload map")
            }

            fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                let mut section = None;
                let mut snapshot_id: Option<Option<SnapshotId>> = None;
                let mut resume_cursor: Option<Option<Vec<u8>>> = None;

                while let Some(field) = map.next_key()? {
                    match field {
                        SnapshotPageQueryField::Section => {
                            if section.is_some() {
                                return Err(de::Error::duplicate_field("section"));
                            }
                            section = Some(map.next_value()?);
                        }
                        SnapshotPageQueryField::SnapshotId => {
                            if snapshot_id.is_some() {
                                return Err(de::Error::duplicate_field("snapshot_id"));
                            }
                            snapshot_id = Some(map.next_value()?);
                        }
                        SnapshotPageQueryField::ResumeCursor => {
                            if resume_cursor.is_some() {
                                return Err(de::Error::duplicate_field("resume_cursor"));
                            }
                            resume_cursor = Some(map.next_value()?);
                        }
                    }
                }

                Ok(SnapshotPageQueryPayload {
                    section: section.ok_or_else(|| de::Error::missing_field("section"))?,
                    snapshot_id: snapshot_id
                        .ok_or_else(|| de::Error::missing_field("snapshot_id"))?,
                    resume_cursor: resume_cursor
                        .ok_or_else(|| de::Error::missing_field("resume_cursor"))?,
                })
            }
        }

        deserializer.deserialize_map(PayloadVisitor)
    }
}

struct ReleaseSnapshotQueryPayload {
    snapshot_id: SnapshotId,
}

enum ReleaseSnapshotQueryField {
    SnapshotId,
}

impl<'de> Deserialize<'de> for ReleaseSnapshotQueryField {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct FieldVisitor;

        impl Visitor<'_> for FieldVisitor {
            type Value = ReleaseSnapshotQueryField;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("snapshot_id")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                match value {
                    "snapshot_id" => Ok(ReleaseSnapshotQueryField::SnapshotId),
                    _ => Err(de::Error::unknown_field(value, &["snapshot_id"])),
                }
            }
        }

        deserializer.deserialize_identifier(FieldVisitor)
    }
}

impl<'de> Deserialize<'de> for ReleaseSnapshotQueryPayload {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct PayloadVisitor;

        impl<'de> Visitor<'de> for PayloadVisitor {
            type Value = ReleaseSnapshotQueryPayload;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a named release_snapshot query payload map")
            }

            fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                let mut snapshot_id = None;
                while let Some(field) = map.next_key()? {
                    match field {
                        ReleaseSnapshotQueryField::SnapshotId => {
                            if snapshot_id.is_some() {
                                return Err(de::Error::duplicate_field("snapshot_id"));
                            }
                            snapshot_id = Some(map.next_value()?);
                        }
                    }
                }
                Ok(ReleaseSnapshotQueryPayload {
                    snapshot_id: snapshot_id
                        .ok_or_else(|| de::Error::missing_field("snapshot_id"))?,
                })
            }
        }

        deserializer.deserialize_map(PayloadVisitor)
    }
}

struct OpenEventReplayQueryPayload {
    after_sequence: u64,
}

enum OpenEventReplayQueryField {
    AfterSequence,
}

impl<'de> Deserialize<'de> for OpenEventReplayQueryField {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct FieldVisitor;

        impl Visitor<'_> for FieldVisitor {
            type Value = OpenEventReplayQueryField;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("after_sequence")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                match value {
                    "after_sequence" => Ok(OpenEventReplayQueryField::AfterSequence),
                    _ => Err(de::Error::unknown_field(value, &["after_sequence"])),
                }
            }
        }

        deserializer.deserialize_identifier(FieldVisitor)
    }
}

impl<'de> Deserialize<'de> for OpenEventReplayQueryPayload {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct PayloadVisitor;

        impl<'de> Visitor<'de> for PayloadVisitor {
            type Value = OpenEventReplayQueryPayload;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a named open_event_replay query payload map")
            }

            fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                let mut after_sequence = None;
                while let Some(field) = map.next_key()? {
                    match field {
                        OpenEventReplayQueryField::AfterSequence => {
                            if after_sequence.is_some() {
                                return Err(de::Error::duplicate_field("after_sequence"));
                            }
                            after_sequence = Some(map.next_value()?);
                        }
                    }
                }
                Ok(OpenEventReplayQueryPayload {
                    after_sequence: after_sequence
                        .ok_or_else(|| de::Error::missing_field("after_sequence"))?,
                })
            }
        }

        deserializer.deserialize_map(PayloadVisitor)
    }
}

struct ContinueEventReplayQueryPayload {
    subscription_id: SubscriptionId,
    resume_cursor: Vec<u8>,
}

enum ContinueEventReplayQueryField {
    SubscriptionId,
    ResumeCursor,
}

impl<'de> Deserialize<'de> for ContinueEventReplayQueryField {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct FieldVisitor;

        impl Visitor<'_> for FieldVisitor {
            type Value = ContinueEventReplayQueryField;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("subscription_id or resume_cursor")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                match value {
                    "subscription_id" => Ok(ContinueEventReplayQueryField::SubscriptionId),
                    "resume_cursor" => Ok(ContinueEventReplayQueryField::ResumeCursor),
                    _ => Err(de::Error::unknown_field(
                        value,
                        &["subscription_id", "resume_cursor"],
                    )),
                }
            }
        }

        deserializer.deserialize_identifier(FieldVisitor)
    }
}

impl<'de> Deserialize<'de> for ContinueEventReplayQueryPayload {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct PayloadVisitor;

        impl<'de> Visitor<'de> for PayloadVisitor {
            type Value = ContinueEventReplayQueryPayload;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a named continue_event_replay query payload map")
            }

            fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                let mut subscription_id = None;
                let mut resume_cursor = None;
                while let Some(field) = map.next_key()? {
                    match field {
                        ContinueEventReplayQueryField::SubscriptionId => {
                            if subscription_id.is_some() {
                                return Err(de::Error::duplicate_field("subscription_id"));
                            }
                            subscription_id = Some(map.next_value()?);
                        }
                        ContinueEventReplayQueryField::ResumeCursor => {
                            if resume_cursor.is_some() {
                                return Err(de::Error::duplicate_field("resume_cursor"));
                            }
                            resume_cursor = Some(map.next_value()?);
                        }
                    }
                }
                Ok(ContinueEventReplayQueryPayload {
                    subscription_id: subscription_id
                        .ok_or_else(|| de::Error::missing_field("subscription_id"))?,
                    resume_cursor: resume_cursor
                        .ok_or_else(|| de::Error::missing_field("resume_cursor"))?,
                })
            }
        }

        deserializer.deserialize_map(PayloadVisitor)
    }
}

struct ReleaseEventReplayQueryPayload {
    subscription_id: SubscriptionId,
}

enum ReleaseEventReplayQueryField {
    SubscriptionId,
}

impl<'de> Deserialize<'de> for ReleaseEventReplayQueryField {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct FieldVisitor;

        impl Visitor<'_> for FieldVisitor {
            type Value = ReleaseEventReplayQueryField;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("subscription_id")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                match value {
                    "subscription_id" => Ok(ReleaseEventReplayQueryField::SubscriptionId),
                    _ => Err(de::Error::unknown_field(value, &["subscription_id"])),
                }
            }
        }

        deserializer.deserialize_identifier(FieldVisitor)
    }
}

impl<'de> Deserialize<'de> for ReleaseEventReplayQueryPayload {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct PayloadVisitor;

        impl<'de> Visitor<'de> for PayloadVisitor {
            type Value = ReleaseEventReplayQueryPayload;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a named release_event_replay query payload map")
            }

            fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                let mut subscription_id = None;
                while let Some(field) = map.next_key()? {
                    match field {
                        ReleaseEventReplayQueryField::SubscriptionId => {
                            if subscription_id.is_some() {
                                return Err(de::Error::duplicate_field("subscription_id"));
                            }
                            subscription_id = Some(map.next_value()?);
                        }
                    }
                }
                Ok(ReleaseEventReplayQueryPayload {
                    subscription_id: subscription_id
                        .ok_or_else(|| de::Error::missing_field("subscription_id"))?,
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
                    QueryVariant::TaskSnapshot => {
                        let _: EmptyNamedMap = map.next_value()?;
                        Query::TaskSnapshot
                    }
                    QueryVariant::SnapshotPage => {
                        let payload: SnapshotPageQueryPayload = map.next_value()?;
                        Query::SnapshotPage {
                            section: payload.section,
                            snapshot_id: payload.snapshot_id,
                            resume_cursor: payload.resume_cursor,
                        }
                    }
                    QueryVariant::ReleaseSnapshot => {
                        let payload: ReleaseSnapshotQueryPayload = map.next_value()?;
                        Query::ReleaseSnapshot {
                            snapshot_id: payload.snapshot_id,
                        }
                    }
                    QueryVariant::OpenEventReplay => {
                        let payload: OpenEventReplayQueryPayload = map.next_value()?;
                        Query::OpenEventReplay {
                            after_sequence: payload.after_sequence,
                        }
                    }
                    QueryVariant::ContinueEventReplay => {
                        let payload: ContinueEventReplayQueryPayload = map.next_value()?;
                        Query::ContinueEventReplay {
                            subscription_id: payload.subscription_id,
                            resume_cursor: payload.resume_cursor,
                        }
                    }
                    QueryVariant::ReleaseEventReplay => {
                        let payload: ReleaseEventReplayQueryPayload = map.next_value()?;
                        Query::ReleaseEventReplay {
                            subscription_id: payload.subscription_id,
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
    TaskSnapshot {
        snapshot: TaskSnapshotItem,
    },
    SnapshotPage {
        page: SnapshotPage,
    },
    SnapshotReleased {
        snapshot_id: SnapshotId,
    },
    EventReplayPage {
        subscription_id: SubscriptionId,
        page: EventPage,
    },
    EventReplayReleased {
        subscription_id: SubscriptionId,
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

struct TaskSnapshotResultRef<'a> {
    snapshot: &'a TaskSnapshotItem,
}

impl Serialize for TaskSnapshotResultRef<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(1))?;
        map.serialize_entry("snapshot", self.snapshot)?;
        map.end()
    }
}

struct SnapshotPageResultRef<'a> {
    page: &'a SnapshotPage,
}

impl Serialize for SnapshotPageResultRef<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(1))?;
        map.serialize_entry("page", self.page)?;
        map.end()
    }
}

struct SnapshotReleasedResultRef<'a> {
    snapshot_id: &'a SnapshotId,
}

impl Serialize for SnapshotReleasedResultRef<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(1))?;
        map.serialize_entry("snapshot_id", self.snapshot_id)?;
        map.end()
    }
}

struct EventReplayPageResultRef<'a> {
    subscription_id: &'a SubscriptionId,
    page: &'a EventPage,
}

impl Serialize for EventReplayPageResultRef<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(2))?;
        map.serialize_entry("subscription_id", self.subscription_id)?;
        map.serialize_entry("page", self.page)?;
        map.end()
    }
}

struct EventReplayReleasedResultRef<'a> {
    subscription_id: &'a SubscriptionId,
}

impl Serialize for EventReplayReleasedResultRef<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(1))?;
        map.serialize_entry("subscription_id", self.subscription_id)?;
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
            Self::TaskSnapshot { snapshot } => {
                map.serialize_entry("task_snapshot", &TaskSnapshotResultRef { snapshot })?
            }
            Self::SnapshotPage { page } => {
                map.serialize_entry("snapshot_page", &SnapshotPageResultRef { page })?
            }
            Self::SnapshotReleased { snapshot_id } => map.serialize_entry(
                "snapshot_released",
                &SnapshotReleasedResultRef { snapshot_id },
            )?,
            Self::EventReplayPage {
                subscription_id,
                page,
            } => map.serialize_entry(
                "event_replay_page",
                &EventReplayPageResultRef {
                    subscription_id,
                    page,
                },
            )?,
            Self::EventReplayReleased { subscription_id } => map.serialize_entry(
                "event_replay_released",
                &EventReplayReleasedResultRef { subscription_id },
            )?,
        }
        map.end()
    }
}

enum QueryResultVariant {
    OperationStatus,
    TaskSnapshot,
    SnapshotPage,
    SnapshotReleased,
    EventReplayPage,
    EventReplayReleased,
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
                formatter.write_str(
                    "operation_status, task_snapshot, snapshot_page, snapshot_released, event_replay_page, or event_replay_released",
                )
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                match value {
                    "operation_status" => Ok(QueryResultVariant::OperationStatus),
                    "task_snapshot" => Ok(QueryResultVariant::TaskSnapshot),
                    "snapshot_page" => Ok(QueryResultVariant::SnapshotPage),
                    "snapshot_released" => Ok(QueryResultVariant::SnapshotReleased),
                    "event_replay_page" => Ok(QueryResultVariant::EventReplayPage),
                    "event_replay_released" => Ok(QueryResultVariant::EventReplayReleased),
                    _ => Err(de::Error::unknown_variant(
                        value,
                        &[
                            "operation_status",
                            "task_snapshot",
                            "snapshot_page",
                            "snapshot_released",
                            "event_replay_page",
                            "event_replay_released",
                        ],
                    )),
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

enum TaskSnapshotResultField {
    Snapshot,
}

impl<'de> Deserialize<'de> for TaskSnapshotResultField {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct FieldVisitor;

        impl Visitor<'_> for FieldVisitor {
            type Value = TaskSnapshotResultField;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("snapshot")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                match value {
                    "snapshot" => Ok(TaskSnapshotResultField::Snapshot),
                    _ => Err(de::Error::unknown_field(value, &["snapshot"])),
                }
            }
        }

        deserializer.deserialize_identifier(FieldVisitor)
    }
}

struct TaskSnapshotResultPayload {
    snapshot: TaskSnapshotItem,
}

impl<'de> Deserialize<'de> for TaskSnapshotResultPayload {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct PayloadVisitor;

        impl<'de> Visitor<'de> for PayloadVisitor {
            type Value = TaskSnapshotResultPayload;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a named task_snapshot result payload map")
            }

            fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                let mut snapshot = None;
                while let Some(field) = map.next_key()? {
                    match field {
                        TaskSnapshotResultField::Snapshot => {
                            if snapshot.is_some() {
                                return Err(de::Error::duplicate_field("snapshot"));
                            }
                            snapshot = Some(map.next_value()?);
                        }
                    }
                }
                Ok(TaskSnapshotResultPayload {
                    snapshot: snapshot.ok_or_else(|| de::Error::missing_field("snapshot"))?,
                })
            }
        }

        deserializer.deserialize_map(PayloadVisitor)
    }
}

struct SnapshotPageResultPayload {
    page: SnapshotPage,
}

enum SnapshotPageResultField {
    Page,
}

impl<'de> Deserialize<'de> for SnapshotPageResultField {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct FieldVisitor;

        impl Visitor<'_> for FieldVisitor {
            type Value = SnapshotPageResultField;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("page")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                match value {
                    "page" => Ok(SnapshotPageResultField::Page),
                    _ => Err(de::Error::unknown_field(value, &["page"])),
                }
            }
        }

        deserializer.deserialize_identifier(FieldVisitor)
    }
}

impl<'de> Deserialize<'de> for SnapshotPageResultPayload {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct PayloadVisitor;

        impl<'de> Visitor<'de> for PayloadVisitor {
            type Value = SnapshotPageResultPayload;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a named snapshot_page result payload map")
            }

            fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                let mut page = None;
                while let Some(field) = map.next_key()? {
                    match field {
                        SnapshotPageResultField::Page => {
                            if page.is_some() {
                                return Err(de::Error::duplicate_field("page"));
                            }
                            page = Some(map.next_value()?);
                        }
                    }
                }
                Ok(SnapshotPageResultPayload {
                    page: page.ok_or_else(|| de::Error::missing_field("page"))?,
                })
            }
        }

        deserializer.deserialize_map(PayloadVisitor)
    }
}

struct SnapshotReleasedResultPayload {
    snapshot_id: SnapshotId,
}

enum SnapshotReleasedResultField {
    SnapshotId,
}

impl<'de> Deserialize<'de> for SnapshotReleasedResultField {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct FieldVisitor;

        impl Visitor<'_> for FieldVisitor {
            type Value = SnapshotReleasedResultField;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("snapshot_id")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                match value {
                    "snapshot_id" => Ok(SnapshotReleasedResultField::SnapshotId),
                    _ => Err(de::Error::unknown_field(value, &["snapshot_id"])),
                }
            }
        }

        deserializer.deserialize_identifier(FieldVisitor)
    }
}

impl<'de> Deserialize<'de> for SnapshotReleasedResultPayload {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct PayloadVisitor;

        impl<'de> Visitor<'de> for PayloadVisitor {
            type Value = SnapshotReleasedResultPayload;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a named snapshot_released result payload map")
            }

            fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                let mut snapshot_id = None;
                while let Some(field) = map.next_key()? {
                    match field {
                        SnapshotReleasedResultField::SnapshotId => {
                            if snapshot_id.is_some() {
                                return Err(de::Error::duplicate_field("snapshot_id"));
                            }
                            snapshot_id = Some(map.next_value()?);
                        }
                    }
                }
                Ok(SnapshotReleasedResultPayload {
                    snapshot_id: snapshot_id
                        .ok_or_else(|| de::Error::missing_field("snapshot_id"))?,
                })
            }
        }

        deserializer.deserialize_map(PayloadVisitor)
    }
}

struct EventReplayPageResultPayload {
    subscription_id: SubscriptionId,
    page: EventPage,
}

enum EventReplayPageResultField {
    SubscriptionId,
    Page,
}

impl<'de> Deserialize<'de> for EventReplayPageResultField {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct FieldVisitor;

        impl Visitor<'_> for FieldVisitor {
            type Value = EventReplayPageResultField;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("subscription_id or page")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                match value {
                    "subscription_id" => Ok(EventReplayPageResultField::SubscriptionId),
                    "page" => Ok(EventReplayPageResultField::Page),
                    _ => Err(de::Error::unknown_field(
                        value,
                        &["subscription_id", "page"],
                    )),
                }
            }
        }

        deserializer.deserialize_identifier(FieldVisitor)
    }
}

impl<'de> Deserialize<'de> for EventReplayPageResultPayload {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct PayloadVisitor;

        impl<'de> Visitor<'de> for PayloadVisitor {
            type Value = EventReplayPageResultPayload;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a named event_replay_page result payload map")
            }

            fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                let mut subscription_id = None;
                let mut page = None;
                while let Some(field) = map.next_key()? {
                    match field {
                        EventReplayPageResultField::SubscriptionId => {
                            if subscription_id.is_some() {
                                return Err(de::Error::duplicate_field("subscription_id"));
                            }
                            subscription_id = Some(map.next_value()?);
                        }
                        EventReplayPageResultField::Page => {
                            if page.is_some() {
                                return Err(de::Error::duplicate_field("page"));
                            }
                            page = Some(map.next_value()?);
                        }
                    }
                }
                Ok(EventReplayPageResultPayload {
                    subscription_id: subscription_id
                        .ok_or_else(|| de::Error::missing_field("subscription_id"))?,
                    page: page.ok_or_else(|| de::Error::missing_field("page"))?,
                })
            }
        }

        deserializer.deserialize_map(PayloadVisitor)
    }
}

struct EventReplayReleasedResultPayload {
    subscription_id: SubscriptionId,
}

enum EventReplayReleasedResultField {
    SubscriptionId,
}

impl<'de> Deserialize<'de> for EventReplayReleasedResultField {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct FieldVisitor;

        impl Visitor<'_> for FieldVisitor {
            type Value = EventReplayReleasedResultField;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("subscription_id")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                match value {
                    "subscription_id" => Ok(EventReplayReleasedResultField::SubscriptionId),
                    _ => Err(de::Error::unknown_field(value, &["subscription_id"])),
                }
            }
        }

        deserializer.deserialize_identifier(FieldVisitor)
    }
}

impl<'de> Deserialize<'de> for EventReplayReleasedResultPayload {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct PayloadVisitor;

        impl<'de> Visitor<'de> for PayloadVisitor {
            type Value = EventReplayReleasedResultPayload;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a named event_replay_released result payload map")
            }

            fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                let mut subscription_id = None;
                while let Some(field) = map.next_key()? {
                    match field {
                        EventReplayReleasedResultField::SubscriptionId => {
                            if subscription_id.is_some() {
                                return Err(de::Error::duplicate_field("subscription_id"));
                            }
                            subscription_id = Some(map.next_value()?);
                        }
                    }
                }
                Ok(EventReplayReleasedResultPayload {
                    subscription_id: subscription_id
                        .ok_or_else(|| de::Error::missing_field("subscription_id"))?,
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
                    QueryResultVariant::TaskSnapshot => {
                        let payload: TaskSnapshotResultPayload = map.next_value()?;
                        QueryResult::TaskSnapshot {
                            snapshot: payload.snapshot,
                        }
                    }
                    QueryResultVariant::SnapshotPage => {
                        let payload: SnapshotPageResultPayload = map.next_value()?;
                        QueryResult::SnapshotPage { page: payload.page }
                    }
                    QueryResultVariant::SnapshotReleased => {
                        let payload: SnapshotReleasedResultPayload = map.next_value()?;
                        QueryResult::SnapshotReleased {
                            snapshot_id: payload.snapshot_id,
                        }
                    }
                    QueryResultVariant::EventReplayPage => {
                        let payload: EventReplayPageResultPayload = map.next_value()?;
                        QueryResult::EventReplayPage {
                            subscription_id: payload.subscription_id,
                            page: payload.page,
                        }
                    }
                    QueryResultVariant::EventReplayReleased => {
                        let payload: EventReplayReleasedResultPayload = map.next_value()?;
                        QueryResult::EventReplayReleased {
                            subscription_id: payload.subscription_id,
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
    ReplayUnavailable {
        oldest_sequence: u64,
        newest_sequence: u64,
    },
}

struct ReplayUnavailableErrorRef {
    oldest_sequence: u64,
    newest_sequence: u64,
}

impl Serialize for ReplayUnavailableErrorRef {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(2))?;
        map.serialize_entry("oldest_sequence", &self.oldest_sequence)?;
        map.serialize_entry("newest_sequence", &self.newest_sequence)?;
        map.end()
    }
}

impl Serialize for QueryError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::NotFound => serializer.serialize_str("not_found"),
            Self::Unauthorized => serializer.serialize_str("unauthorized"),
            Self::InvalidRequest => serializer.serialize_str("invalid_request"),
            Self::UnsupportedCapability => serializer.serialize_str("unsupported_capability"),
            Self::ReplayUnavailable {
                oldest_sequence,
                newest_sequence,
            } => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry(
                    "replay_unavailable",
                    &ReplayUnavailableErrorRef {
                        oldest_sequence: *oldest_sequence,
                        newest_sequence: *newest_sequence,
                    },
                )?;
                map.end()
            }
        }
    }
}

enum QueryErrorMapVariant {
    ReplayUnavailable,
}

impl<'de> Deserialize<'de> for QueryErrorMapVariant {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct VariantVisitor;

        impl Visitor<'_> for VariantVisitor {
            type Value = QueryErrorMapVariant;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("replay_unavailable")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                match value {
                    "replay_unavailable" => Ok(QueryErrorMapVariant::ReplayUnavailable),
                    _ => Err(de::Error::unknown_variant(
                        value,
                        &[
                            "not_found",
                            "unauthorized",
                            "invalid_request",
                            "unsupported_capability",
                            "replay_unavailable",
                        ],
                    )),
                }
            }
        }

        deserializer.deserialize_identifier(VariantVisitor)
    }
}

struct ReplayUnavailableErrorPayload {
    oldest_sequence: u64,
    newest_sequence: u64,
}

enum ReplayUnavailableErrorField {
    OldestSequence,
    NewestSequence,
}

impl<'de> Deserialize<'de> for ReplayUnavailableErrorField {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct FieldVisitor;

        impl Visitor<'_> for FieldVisitor {
            type Value = ReplayUnavailableErrorField;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("oldest_sequence or newest_sequence")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                match value {
                    "oldest_sequence" => Ok(ReplayUnavailableErrorField::OldestSequence),
                    "newest_sequence" => Ok(ReplayUnavailableErrorField::NewestSequence),
                    _ => Err(de::Error::unknown_field(
                        value,
                        &["oldest_sequence", "newest_sequence"],
                    )),
                }
            }
        }

        deserializer.deserialize_identifier(FieldVisitor)
    }
}

impl<'de> Deserialize<'de> for ReplayUnavailableErrorPayload {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct PayloadVisitor;

        impl<'de> Visitor<'de> for PayloadVisitor {
            type Value = ReplayUnavailableErrorPayload;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a named replay_unavailable error payload map")
            }

            fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                let mut oldest_sequence = None;
                let mut newest_sequence = None;
                while let Some(field) = map.next_key()? {
                    match field {
                        ReplayUnavailableErrorField::OldestSequence => {
                            if oldest_sequence.is_some() {
                                return Err(de::Error::duplicate_field("oldest_sequence"));
                            }
                            oldest_sequence = Some(map.next_value()?);
                        }
                        ReplayUnavailableErrorField::NewestSequence => {
                            if newest_sequence.is_some() {
                                return Err(de::Error::duplicate_field("newest_sequence"));
                            }
                            newest_sequence = Some(map.next_value()?);
                        }
                    }
                }
                Ok(ReplayUnavailableErrorPayload {
                    oldest_sequence: oldest_sequence
                        .ok_or_else(|| de::Error::missing_field("oldest_sequence"))?,
                    newest_sequence: newest_sequence
                        .ok_or_else(|| de::Error::missing_field("newest_sequence"))?,
                })
            }
        }

        deserializer.deserialize_map(PayloadVisitor)
    }
}

impl<'de> Deserialize<'de> for QueryError {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct QueryErrorVisitor;

        impl<'de> Visitor<'de> for QueryErrorVisitor {
            type Value = QueryError;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a named QueryError code or one-entry replay_unavailable map")
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
                            "replay_unavailable",
                        ],
                    )),
                }
            }

            fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                let variant = map
                    .next_key()?
                    .ok_or_else(|| de::Error::custom("QueryError map variant is missing"))?;
                let error = match variant {
                    QueryErrorMapVariant::ReplayUnavailable => {
                        let payload: ReplayUnavailableErrorPayload = map.next_value()?;
                        QueryError::ReplayUnavailable {
                            oldest_sequence: payload.oldest_sequence,
                            newest_sequence: payload.newest_sequence,
                        }
                    }
                };
                if map.next_key::<de::IgnoredAny>()?.is_some() {
                    return Err(de::Error::custom(
                        "QueryError must contain exactly one variant",
                    ));
                }
                Ok(error)
            }
        }

        deserializer.deserialize_any(QueryErrorVisitor)
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
