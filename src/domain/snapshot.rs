use std::collections::BTreeMap;

use crate::domain::agent::AgentSessionFacts;
use crate::domain::artifact::ArtifactFacts;
use crate::domain::id::{AgentSessionId, ArtifactId, ResourceId};
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
