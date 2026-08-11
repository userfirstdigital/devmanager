use std::collections::BTreeMap;
use std::fmt;

use serde::de::{self, Deserializer, MapAccess, SeqAccess, Visitor};
use serde::ser::{self, SerializeMap, Serializer};
use serde::{Deserialize, Serialize};

use crate::domain::agent::AgentSessionFacts;
use crate::domain::artifact::PrivacyClass;
use crate::domain::artifact::{ArtifactFacts, ArtifactSummary};
use crate::domain::event::DomainEvent;
use crate::domain::id::{
    AgentSessionId, ArtifactId, EventId, OperationId, ResourceId, SnapshotId, TaskId,
};
use crate::domain::operation::OperationFacts;
use crate::domain::provider_input::ProviderSessionProjection;
use crate::domain::resource::ResourceFacts;
use crate::domain::task::{
    ReviewReadiness, TaskActivity, TaskAttention, TaskConnectivity, TaskFacts, VisibleTaskStatus,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskSnapshot {
    pub task: TaskFacts,
    pub connectivity: TaskConnectivity,
    pub attention: TaskAttention,
    pub activity: TaskActivity,
    pub review_readiness: ReviewReadiness,
    pub agents: BTreeMap<AgentSessionId, AgentSessionFacts>,
    pub primary_agent_id: Option<AgentSessionId>,
    pub artifacts: BTreeMap<ArtifactId, ArtifactFacts>,
    pub resources: BTreeMap<ResourceId, ResourceFacts>,
    pub provider_sessions: BTreeMap<AgentSessionId, ProviderSessionProjection>,
}

impl TaskSnapshot {
    pub fn visible_status(&self) -> VisibleTaskStatus {
        VisibleTaskStatus::derive(
            self.connectivity,
            self.attention,
            self.activity,
            self.review_readiness,
        )
    }
}

/// Row-granular durable snapshot sections. Large terminal grids, screenshots,
/// and artifact bodies are intentionally outside this snapshot model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotSection {
    Tasks,
    AgentSessions,
    Artifacts,
    Resources,
    Operations,
}

pub const MAX_SNAPSHOT_PAGE_ITEMS: u32 = 1_000;
pub const MAX_SNAPSHOT_PAGE_ENCODED_BYTES: u32 = 512 * 1024;

/// Provider-neutral, bounded semantic payload retained by the journal. Raw
/// provider envelopes and terminal bytes are deliberately not represented.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SemanticJournalPayload {
    UserMessage {
        text: String,
    },
    AssistantText {
        text: String,
    },
    ReasoningSummary {
        text: String,
    },
    ToolCall {
        tool_name: String,
        call_id: String,
    },
    ToolResult {
        call_id: String,
        status: String,
    },
    ApprovalRequest {
        request_id: String,
        summary: String,
    },
    ApprovalResult {
        request_id: String,
        decision: String,
    },
    Question {
        question_id: String,
        prompt: String,
        options: Vec<String>,
    },
    PlanStep {
        step_id: String,
        title: String,
        status: String,
    },
    UsageObservation {
        remaining_percent: Option<u8>,
    },
    Error {
        code: String,
        message: String,
    },
    TurnState {
        state: String,
    },
    SessionState {
        state: String,
    },
    ArtifactReference {
        label: String,
    },
    Unknown {
        provider: String,
        source_type: String,
        schema_version: u32,
        diagnostic_ref: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageLimitsError {
    ZeroItems,
    TooManyItems,
    ZeroEncodedBytes,
    TooManyEncodedBytes,
}

impl std::fmt::Display for PageLimitsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ZeroItems => write!(f, "snapshot page item limit must be nonzero"),
            Self::TooManyItems => write!(
                f,
                "snapshot page item limit exceeds {MAX_SNAPSHOT_PAGE_ITEMS}"
            ),
            Self::ZeroEncodedBytes => {
                write!(f, "snapshot page encoded-byte limit must be nonzero")
            }
            Self::TooManyEncodedBytes => write!(
                f,
                "snapshot page encoded-byte limit exceeds {MAX_SNAPSHOT_PAGE_ENCODED_BYTES}"
            ),
        }
    }
}

impl std::error::Error for PageLimitsError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageLimits {
    pub max_items: u32,
    pub max_encoded_bytes: u32,
}

impl Serialize for PageLimits {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.validate().map_err(ser::Error::custom)?;
        #[derive(Serialize)]
        struct PageLimitsWire {
            max_items: u32,
            max_encoded_bytes: u32,
        }
        PageLimitsWire {
            max_items: self.max_items,
            max_encoded_bytes: self.max_encoded_bytes,
        }
        .serialize(serializer)
    }
}

impl PageLimits {
    pub fn new(max_items: u32, max_encoded_bytes: u32) -> Result<Self, PageLimitsError> {
        let limits = Self {
            max_items,
            max_encoded_bytes,
        };
        limits.validate()?;
        Ok(limits)
    }

    pub fn validate(&self) -> Result<(), PageLimitsError> {
        if self.max_items == 0 {
            return Err(PageLimitsError::ZeroItems);
        }
        if self.max_items > MAX_SNAPSHOT_PAGE_ITEMS {
            return Err(PageLimitsError::TooManyItems);
        }
        if self.max_encoded_bytes == 0 {
            return Err(PageLimitsError::ZeroEncodedBytes);
        }
        if self.max_encoded_bytes > MAX_SNAPSHOT_PAGE_ENCODED_BYTES {
            return Err(PageLimitsError::TooManyEncodedBytes);
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for PageLimits {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct PageLimitsWire {
            max_items: u32,
            max_encoded_bytes: u32,
        }

        let wire = PageLimitsWire::deserialize(deserializer)?;
        Self::new(wire.max_items, wire.max_encoded_bytes).map_err(de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotItemKey {
    Task(TaskId),
    AgentSession(AgentSessionId),
    Artifact(ArtifactId),
    Resource(ResourceId),
    Operation(OperationId),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskSnapshotItem {
    pub task: TaskFacts,
    pub connectivity: TaskConnectivity,
    pub attention: TaskAttention,
    pub activity: TaskActivity,
    pub review_readiness: ReviewReadiness,
    pub primary_agent_id: Option<AgentSessionId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotItem {
    Task(TaskSnapshotItem),
    AgentSession(AgentSessionFacts),
    Artifact(ArtifactSummary),
    Resource(ResourceFacts),
    Operation(OperationFacts),
}

/// One page from one immutable snapshot view.
///
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotPage {
    pub snapshot_id: SnapshotId,
    pub through_sequence: u64,
    pub section: SnapshotSection,
    /// Exclusive boundary used to produce this page.
    pub after_item: Option<SnapshotItemKey>,
    pub items: Vec<SnapshotItem>,
    /// Exact canonical MessagePack size of this page body.
    pub encoded_bytes: u32,
    pub next_cursor: Option<Vec<u8>>,
}

/// Bounded semantic-journal projection. This is intentionally not a
/// `SnapshotSection`: the global Task snapshot must never carry raw provider
/// payloads, terminal bytes, or unknown event bodies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticJournalFact {
    pub id: EventId,
    pub sequence: u64,
    pub provider: String,
    pub schema_version: u32,
    pub kind: String,
    pub visibility: String,
    pub privacy_class: PrivacyClass,
    pub redacted: bool,
    pub payload: SemanticJournalPayload,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticJournalPage {
    pub after_sequence: u64,
    pub through_sequence: u64,
    pub high_water: u64,
    pub encoded_bytes: u32,
    pub next_sequence: Option<u64>,
    pub facts: Vec<SemanticJournalFact>,
}

/// One bounded page from a replay session pinned to a durable high-water mark.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventPage {
    /// Exclusive sequence boundary used to produce this page.
    pub after_sequence: u64,
    /// Fixed high-water sequence captured when replay began.
    pub through_sequence: u64,
    pub events: Vec<DomainEvent>,
    pub next_cursor: Option<Vec<u8>>,
}

/// One bounded on-demand artifact content page. `payload` is MessagePack binary
/// bytes (never a byte array). `offset` is the first byte included in this page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactContentPage {
    pub artifact_id: ArtifactId,
    pub offset: u64,
    pub total_bytes: u64,
    pub sha256: [u8; 32],
    pub payload: Vec<u8>,
    pub encoded_bytes: u32,
    pub next_cursor: Option<Vec<u8>>,
}

struct ArtifactContentBinaryRef<'a>(&'a [u8]);

impl Serialize for ArtifactContentBinaryRef<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(self.0)
    }
}

struct OptionalArtifactContentBinaryRef<'a>(Option<&'a [u8]>);

impl Serialize for OptionalArtifactContentBinaryRef<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self.0 {
            Some(bytes) => serializer.serialize_some(&ArtifactContentBinaryRef(bytes)),
            None => serializer.serialize_none(),
        }
    }
}

struct ArtifactContentBinary(Vec<u8>);

impl<'de> Deserialize<'de> for ArtifactContentBinary {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct BinaryVisitor;

        impl<'de> Visitor<'de> for BinaryVisitor {
            type Value = ArtifactContentBinary;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("MessagePack binary bytes")
            }

            fn visit_bytes<E: de::Error>(self, value: &[u8]) -> Result<Self::Value, E> {
                Ok(ArtifactContentBinary(value.to_vec()))
            }

            fn visit_byte_buf<E: de::Error>(self, value: Vec<u8>) -> Result<Self::Value, E> {
                Ok(ArtifactContentBinary(value))
            }

            fn visit_seq<A: SeqAccess<'de>>(self, _seq: A) -> Result<Self::Value, A::Error> {
                Err(de::Error::invalid_type(de::Unexpected::Seq, &self))
            }
        }

        deserializer.deserialize_bytes(BinaryVisitor)
    }
}

struct OptionalArtifactContentBinary(Option<Vec<u8>>);

impl<'de> Deserialize<'de> for OptionalArtifactContentBinary {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct OptionalBinaryVisitor;

        impl<'de> Visitor<'de> for OptionalBinaryVisitor {
            type Value = OptionalArtifactContentBinary;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("optional MessagePack binary bytes")
            }

            fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(OptionalArtifactContentBinary(None))
            }

            fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(OptionalArtifactContentBinary(None))
            }

            fn visit_some<D: Deserializer<'de>>(
                self,
                deserializer: D,
            ) -> Result<Self::Value, D::Error> {
                let ArtifactContentBinary(bytes) =
                    ArtifactContentBinary::deserialize(deserializer)?;
                Ok(OptionalArtifactContentBinary(Some(bytes)))
            }

            fn visit_bytes<E: de::Error>(self, value: &[u8]) -> Result<Self::Value, E> {
                Ok(OptionalArtifactContentBinary(Some(value.to_vec())))
            }

            fn visit_byte_buf<E: de::Error>(self, value: Vec<u8>) -> Result<Self::Value, E> {
                Ok(OptionalArtifactContentBinary(Some(value)))
            }

            fn visit_seq<A: SeqAccess<'de>>(self, _seq: A) -> Result<Self::Value, A::Error> {
                Err(de::Error::invalid_type(de::Unexpected::Seq, &self))
            }
        }

        deserializer.deserialize_option(OptionalBinaryVisitor)
    }
}

impl Serialize for ArtifactContentPage {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(7))?;
        map.serialize_entry("artifact_id", &self.artifact_id)?;
        map.serialize_entry("offset", &self.offset)?;
        map.serialize_entry("total_bytes", &self.total_bytes)?;
        map.serialize_entry("sha256", &self.sha256)?;
        map.serialize_entry("payload", &ArtifactContentBinaryRef(&self.payload))?;
        map.serialize_entry("encoded_bytes", &self.encoded_bytes)?;
        map.serialize_entry(
            "next_cursor",
            &OptionalArtifactContentBinaryRef(self.next_cursor.as_deref()),
        )?;
        map.end()
    }
}

impl<'de> Deserialize<'de> for ArtifactContentPage {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(field_identifier, rename_all = "snake_case")]
        enum Field {
            ArtifactId,
            Offset,
            TotalBytes,
            Sha256,
            Payload,
            EncodedBytes,
            NextCursor,
        }

        struct PageVisitor;

        impl<'de> Visitor<'de> for PageVisitor {
            type Value = ArtifactContentPage;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a named ArtifactContentPage map")
            }

            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
                let mut artifact_id = None;
                let mut offset = None;
                let mut total_bytes = None;
                let mut sha256 = None;
                let mut payload = None;
                let mut encoded_bytes = None;
                let mut next_cursor = None;

                while let Some(field) = map.next_key()? {
                    match field {
                        Field::ArtifactId => {
                            if artifact_id.is_some() {
                                return Err(de::Error::duplicate_field("artifact_id"));
                            }
                            artifact_id = Some(map.next_value()?);
                        }
                        Field::Offset => {
                            if offset.is_some() {
                                return Err(de::Error::duplicate_field("offset"));
                            }
                            offset = Some(map.next_value()?);
                        }
                        Field::TotalBytes => {
                            if total_bytes.is_some() {
                                return Err(de::Error::duplicate_field("total_bytes"));
                            }
                            total_bytes = Some(map.next_value()?);
                        }
                        Field::Sha256 => {
                            if sha256.is_some() {
                                return Err(de::Error::duplicate_field("sha256"));
                            }
                            sha256 = Some(map.next_value()?);
                        }
                        Field::Payload => {
                            if payload.is_some() {
                                return Err(de::Error::duplicate_field("payload"));
                            }
                            let ArtifactContentBinary(bytes) = map.next_value()?;
                            payload = Some(bytes);
                        }
                        Field::EncodedBytes => {
                            if encoded_bytes.is_some() {
                                return Err(de::Error::duplicate_field("encoded_bytes"));
                            }
                            encoded_bytes = Some(map.next_value()?);
                        }
                        Field::NextCursor => {
                            if next_cursor.is_some() {
                                return Err(de::Error::duplicate_field("next_cursor"));
                            }
                            let OptionalArtifactContentBinary(bytes) = map.next_value()?;
                            next_cursor = Some(bytes);
                        }
                    }
                }

                Ok(ArtifactContentPage {
                    artifact_id: artifact_id
                        .ok_or_else(|| de::Error::missing_field("artifact_id"))?,
                    offset: offset.ok_or_else(|| de::Error::missing_field("offset"))?,
                    total_bytes: total_bytes
                        .ok_or_else(|| de::Error::missing_field("total_bytes"))?,
                    sha256: sha256.ok_or_else(|| de::Error::missing_field("sha256"))?,
                    payload: payload.ok_or_else(|| de::Error::missing_field("payload"))?,
                    encoded_bytes: encoded_bytes
                        .ok_or_else(|| de::Error::missing_field("encoded_bytes"))?,
                    next_cursor: next_cursor
                        .ok_or_else(|| de::Error::missing_field("next_cursor"))?,
                })
            }
        }

        const FIELDS: &[&str] = &[
            "artifact_id",
            "offset",
            "total_bytes",
            "sha256",
            "payload",
            "encoded_bytes",
            "next_cursor",
        ];
        deserializer.deserialize_struct("ArtifactContentPage", FIELDS, PageVisitor)
    }
}

/// The confidence of a background-produced process accounting sample.
///
/// A numeric zero is not a substitute for a failed query or an unavailable
/// first-sample baseline. Consumers must inspect this status before treating
/// CPU/resource values as complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ProcessMetricStatus {
    Complete,
    Partial,
    #[default]
    Unknown,
    Failed,
}

impl ProcessMetricStatus {
    pub fn is_complete(self) -> bool {
        matches!(self, Self::Complete)
    }

    pub fn is_failed(self) -> bool {
        matches!(self, Self::Failed)
    }
}

/// Immutable, background-produced accounting for one owned process tree.
///
/// This is intentionally a runtime projection rather than a durable domain
/// fact. It carries enough information for consumers to distinguish a complete
/// tree from a partial observation without making the render/input path query
/// the operating system.
#[derive(Debug, Clone, PartialEq)]
pub struct ProcessAccountingSnapshot {
    pub sampled_at: std::time::Duration,
    pub interval: Option<std::time::Duration>,
    pub logical_processors: u32,
    pub machine_cpu_percent: f64,
    pub core_equivalent_percent: f64,
    pub memory_bytes: u64,
    pub process_count: u32,
    pub metrics_unavailable: bool,
    pub status: ProcessMetricStatus,
    /// A bounded, sanitized diagnostic for a failed/partial observation. Raw
    /// command lines and environment values never enter this projection.
    pub error: Option<String>,
    /// Monotonic sampler generation. A new generation fences PID reuse and
    /// counter baselines even when a PID number is recycled.
    pub generation: u64,
    pub io_read_bytes: Option<u64>,
    pub io_write_bytes: Option<u64>,
    pub members: Vec<ProcessAccountingMemberSnapshot>,
}

/// One unique Job-member observation in an accounting snapshot.
#[derive(Debug, Clone, PartialEq)]
pub struct ProcessAccountingMemberSnapshot {
    pub pid: u32,
    pub creation_time_100ns: Option<u64>,
    pub machine_cpu_percent: Option<f64>,
    pub core_equivalent_percent: Option<f64>,
    /// Platform-private memory bytes. Windows uses `PrivateUsage` (private
    /// committed bytes); Unix uses `/proc/<pid>/smaps_rollup` private
    /// clean+dirty (private resident bytes). This is deliberately not named
    /// working set.
    pub private_memory_bytes: Option<u64>,
    pub io_read_bytes: Option<u64>,
    pub io_write_bytes: Option<u64>,
    pub metrics_unavailable: bool,
    pub status: ProcessMetricStatus,
    pub executable: Option<String>,
    pub generation: u64,
}
