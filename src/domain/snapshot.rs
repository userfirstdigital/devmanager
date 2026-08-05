use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::domain::agent::AgentSessionFacts;
use crate::domain::artifact::ArtifactFacts;
use crate::domain::id::{AgentSessionId, ArtifactId, ResourceId, SnapshotId};
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
/// Resume boundaries, encoded-size accounting, and signed cursors are added by
/// the following pagination slice; this first shape establishes snapshot
/// identity and one global durable event boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotPage {
    pub snapshot_id: SnapshotId,
    pub through_sequence: u64,
    pub section: SnapshotSection,
    pub items: Vec<SnapshotItem>,
}
