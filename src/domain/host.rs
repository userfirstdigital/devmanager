//! Host-level inspection projections and durable cleanup branch journal types.
//! Side-effect-free wire types only.

use std::num::NonZeroU64;

use serde::{Deserialize, Serialize};

use crate::domain::agent::{AgentRole, AgentSessionLifecycle};
use crate::domain::id::{AgentSessionId, ResourceId, TaskId};
use crate::domain::resource::{OwnerKind, ResourceKind, ResourceLifecycle};
use crate::providers::ProviderKind;

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
    pub provider_kind: ProviderKind,
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

/// Fixed durable host-cleanup journal branches, in advancement order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostCleanupBranch {
    AgentSessions,
    Resources,
    OutstandingEffects,
    TaskTeardowns,
}

impl HostCleanupBranch {
    /// Deterministic advancement order. Resume at the first absent branch.
    pub const ORDER: [Self; 4] = [
        Self::AgentSessions,
        Self::Resources,
        Self::OutstandingEffects,
        Self::TaskTeardowns,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::AgentSessions => "agent_sessions",
            Self::Resources => "resources",
            Self::OutstandingEffects => "outstanding_effects",
            Self::TaskTeardowns => "task_teardowns",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "agent_sessions" => Some(Self::AgentSessions),
            "resources" => Some(Self::Resources),
            "outstanding_effects" => Some(Self::OutstandingEffects),
            "task_teardowns" => Some(Self::TaskTeardowns),
            _ => None,
        }
    }
}

/// Terminal durable outcome for one host-cleanup branch.
///
/// Succeeded always means remaining_count = 0; Failed always means remaining_count > 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case", deny_unknown_fields)]
pub enum HostCleanupBranchOutcome {
    Succeeded,
    Failed { remaining_count: NonZeroU64 },
}

impl HostCleanupBranchOutcome {
    pub fn succeeded() -> Self {
        Self::Succeeded
    }

    pub fn failed(remaining_count: u64) -> Option<Self> {
        NonZeroU64::new(remaining_count).map(|remaining_count| Self::Failed { remaining_count })
    }

    pub fn remaining_count(self) -> u64 {
        match self {
            Self::Succeeded => 0,
            Self::Failed { remaining_count } => remaining_count.get(),
        }
    }

    pub fn result_str(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Failed { .. } => "failed",
        }
    }
}
