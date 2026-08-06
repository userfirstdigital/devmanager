//! Host-level inspection projections. Side-effect-free wire types only.

use serde::{Deserialize, Serialize};

use crate::domain::agent::{AgentRole, AgentSessionLifecycle};
use crate::domain::id::{AgentSessionId, ResourceId, TaskId};
use crate::domain::resource::{OwnerKind, ResourceKind, ResourceLifecycle};

/// Durable blockers observed by [`crate::domain::Query::InspectHostQuit`].
///
/// Counts are derived by the UI from the typed lists. This slice never claims
/// worktree cleanliness or confirmability.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostQuitInspection {
    /// Durable event high-water from the same read snapshot as the blockers.
    pub inspection_id: u64,
    pub agents: Vec<HostQuitAgentBlocker>,
    pub resources: Vec<HostQuitResourceBlocker>,
    pub worktrees: HostQuitWorktreeInspection,
    pub confirmable: bool,
}

/// Open or Closing agent session that blocks host quit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostQuitAgentBlocker {
    pub agent_session_id: AgentSessionId,
    pub task_id: TaskId,
    pub task_title: String,
    pub role: AgentRole,
    pub provider_kind: String,
    pub lifecycle: AgentSessionLifecycle,
    pub runtime_generation: u64,
}

/// Active or Releasing resource that blocks host quit.
///
/// Host-owned blockers carry `task_id` / `task_title` as `None`. Recipes, URLs,
/// commands, and other raw payload are never included.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostQuitResourceBlocker {
    pub resource_id: ResourceId,
    pub task_id: Option<TaskId>,
    pub task_title: Option<String>,
    pub owner_kind: OwnerKind,
    pub resource_kind: ResourceKind,
    pub lifecycle: ResourceLifecycle,
    pub runtime_generation: u64,
}

/// Worktree dirtiness is intentionally not inspected in this slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostQuitWorktreeInspection {
    NotInspected,
}
