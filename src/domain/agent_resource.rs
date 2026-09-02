//! Exact durable identity for a stock-provider runtime resource.
//!
//! A provider session is backed by one task-owned terminal/resource, but the
//! relationship is intentionally not inferred from a PTY, cwd, timestamps, or
//! a transcript.  Callers must carry the exact `ResourceId` that was admitted
//! for the session and validate the two durable projections together before a
//! provider is launched or a host projection is emitted.

use serde::{Deserialize, Serialize};

use crate::domain::agent::{AgentSessionFacts, AgentSessionLifecycle};
use crate::domain::id::{AgentSessionId, ResourceId, TaskId};
use crate::domain::resource::{OwnerKind, ResourceFacts, ResourceKind, ResourceLifecycle};
use crate::providers::ProviderKind;

/// The exact identity shared by a task-owned provider session and its terminal
/// resource for one runtime generation.
///
/// This value deliberately has no provider-session-id field.  That opaque ID
/// may only enter the durable agent facts through the correlated,
/// current-generation provider hook path.  A launch caller cannot inject or
/// replace it while claiming a resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct AgentResourceBinding {
    pub task_id: TaskId,
    pub agent_session_id: AgentSessionId,
    pub resource_id: ResourceId,
    pub provider_kind: ProviderKind,
    pub runtime_generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentResourceBindingError {
    ResourceTaskMismatch,
    ResourceMustBeTaskOwned,
    ResourceMustBeTerminal,
    AgentMustBeOpen,
    ResourceMustBeActive,
    RuntimeGenerationMismatch,
}

impl std::fmt::Display for AgentResourceBindingError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ResourceTaskMismatch => write!(formatter, "resource belongs to another task"),
            Self::ResourceMustBeTaskOwned => write!(formatter, "resource must be task-owned"),
            Self::ResourceMustBeTerminal => {
                write!(formatter, "provider resource must be a terminal")
            }
            Self::AgentMustBeOpen => write!(formatter, "agent session is not open"),
            Self::ResourceMustBeActive => write!(formatter, "provider resource is not active"),
            Self::RuntimeGenerationMismatch => {
                write!(formatter, "agent and resource runtime generations differ")
            }
        }
    }
}

impl std::error::Error for AgentResourceBindingError {}

impl AgentResourceBinding {
    /// Join the two durable facts without inventing an association.
    ///
    /// The caller supplies the exact resource row; this method never searches
    /// by generation, cwd, timestamps, or terminal identity.  Consequently a
    /// second terminal at the same generation cannot silently become the
    /// provider runtime.
    pub fn from_facts(
        agent: &AgentSessionFacts,
        resource: &ResourceFacts,
    ) -> Result<Self, AgentResourceBindingError> {
        if resource.task_id != Some(agent.task_id) {
            return if resource.task_id.is_some() {
                Err(AgentResourceBindingError::ResourceTaskMismatch)
            } else {
                Err(AgentResourceBindingError::ResourceMustBeTaskOwned)
            };
        }
        if resource.owner_kind != OwnerKind::Task {
            return Err(AgentResourceBindingError::ResourceMustBeTaskOwned);
        }
        if resource.resource_kind != ResourceKind::Terminal {
            return Err(AgentResourceBindingError::ResourceMustBeTerminal);
        }
        if agent.lifecycle != AgentSessionLifecycle::Open {
            return Err(AgentResourceBindingError::AgentMustBeOpen);
        }
        if resource.lifecycle != ResourceLifecycle::Active {
            return Err(AgentResourceBindingError::ResourceMustBeActive);
        }
        if agent.runtime_generation != resource.runtime_generation {
            return Err(AgentResourceBindingError::RuntimeGenerationMismatch);
        }
        Ok(Self {
            task_id: agent.task_id,
            agent_session_id: agent.id,
            resource_id: resource.id,
            provider_kind: agent.provider_kind,
            runtime_generation: agent.runtime_generation,
        })
    }

    /// Return whether this claim names the same immutable identity.
    pub fn matches(self, requested: Self) -> bool {
        self.task_id == requested.task_id
            && self.agent_session_id == requested.agent_session_id
            && self.resource_id == requested.resource_id
            && self.provider_kind == requested.provider_kind
            && self.runtime_generation == requested.runtime_generation
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::agent::{AgentRole, ProviderSessionId};
    use crate::domain::id::{AgentSessionId, ResourceId, TaskId};
    use crate::domain::resource::ResourceRecipe;

    fn ids() -> (TaskId, AgentSessionId, ResourceId) {
        let bytes = |tail| {
            [
                0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, tail,
            ]
        };
        (
            TaskId::from_bytes(bytes(1)).expect("task"),
            AgentSessionId::from_bytes(bytes(2)).expect("agent"),
            ResourceId::from_bytes(bytes(3)).expect("resource"),
        )
    }

    fn facts(generation: u64) -> (AgentSessionFacts, ResourceFacts) {
        let (task_id, agent_session_id, resource_id) = ids();
        (
            AgentSessionFacts {
                id: agent_session_id,
                task_id,
                role: AgentRole::Primary,
                provider_kind: ProviderKind::Codex,
                provider_session_id: Some(ProviderSessionId::new("hook-session").expect("id")),
                lifecycle: AgentSessionLifecycle::Open,
                runtime_generation: generation,
                revision: 0,
            },
            ResourceFacts {
                id: resource_id,
                task_id: Some(task_id),
                owner_kind: OwnerKind::Task,
                resource_kind: ResourceKind::Terminal,
                recipe: ResourceRecipe::terminal(120, 40),
                lifecycle: ResourceLifecycle::Active,
                runtime_generation: generation,
                updated_at_ms: 1,
            },
        )
    }

    #[test]
    fn binds_only_the_explicit_task_terminal_at_the_same_generation() {
        let (agent, resource) = facts(7);
        let binding = AgentResourceBinding::from_facts(&agent, &resource).expect("binding");
        assert_eq!(binding.task_id, agent.task_id);
        assert_eq!(binding.agent_session_id, agent.id);
        assert_eq!(binding.resource_id, resource.id);
        assert_eq!(binding.provider_kind, ProviderKind::Codex);
        assert_eq!(binding.runtime_generation, 7);
    }

    #[test]
    fn rejects_a_replacement_generation_instead_of_reusing_the_old_resource() {
        let (agent, mut resource) = facts(7);
        resource.runtime_generation = 8;
        assert_eq!(
            AgentResourceBinding::from_facts(&agent, &resource),
            Err(AgentResourceBindingError::RuntimeGenerationMismatch)
        );
        agent
            .provider_session_id
            .as_ref()
            .expect("hook id remains data");
    }

    #[test]
    fn rejects_non_terminal_or_non_active_resources() {
        let (agent, mut resource) = facts(7);
        resource.resource_kind = ResourceKind::BrowserContext;
        assert_eq!(
            AgentResourceBinding::from_facts(&agent, &resource),
            Err(AgentResourceBindingError::ResourceMustBeTerminal)
        );
        resource.resource_kind = ResourceKind::Terminal;
        resource.lifecycle = ResourceLifecycle::Releasing;
        assert_eq!(
            AgentResourceBinding::from_facts(&agent, &resource),
            Err(AgentResourceBindingError::ResourceMustBeActive)
        );
    }
}
