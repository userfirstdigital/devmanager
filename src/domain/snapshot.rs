use std::collections::BTreeMap;

use serde::de::{self, Deserializer};
use serde::ser::{self, Serializer};
use serde::{Deserialize, Serialize};

use crate::domain::agent::AgentSessionFacts;
use crate::domain::artifact::ArtifactFacts;
use crate::domain::event::DomainEvent;
use crate::domain::id::{AgentSessionId, ArtifactId, OperationId, ResourceId, SnapshotId, TaskId};
use crate::domain::operation::OperationFacts;
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
    Artifact(ArtifactFacts),
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
