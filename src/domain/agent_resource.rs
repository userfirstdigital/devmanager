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
    /// The resource is a plain shell terminal. A provider binding may never
    /// point at one: a shell has no agent session, and a task's shells are
    /// ordinarily registered at the same runtime generation as its agent, so
    /// admitting them here makes every "the one bound resource" search
    /// ambiguous the moment a user opens a terminal tab.
    ResourceMustNotBePlainShell,
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
            Self::ResourceMustNotBePlainShell => {
                write!(formatter, "provider resource must not be a plain shell")
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
        // A plain shell is a Terminal resource owned by the same task, usually
        // at the same runtime generation, so without this every caller that
        // searches for "the resource this agent binds to" finds the shells too.
        if resource.recipe.is_plain_shell() {
            return Err(AgentResourceBindingError::ResourceMustNotBePlainShell);
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

/// More than one Active provider terminal resource matched one agent.
///
/// This is a refusal, not a count: `AgentResourceBinding` exists precisely so a
/// second terminal at the same generation cannot silently become the provider
/// runtime, and picking either one here would be that silent choice. The count
/// travels with it so the refusal is attributable in a log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderResourceAmbiguity {
    pub candidates: usize,
}

impl std::fmt::Display for ProviderResourceAmbiguity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{} active provider terminal resources match this agent generation",
            self.candidates
        )
    }
}

impl std::error::Error for ProviderResourceAmbiguity {}

/// The one durable terminal resource that backs `agent`'s provider runtime.
///
/// This is the single definition of "the provider's terminal resource" and
/// every caller that needs it must come here. It is deliberately the exact set
/// of [`AgentResourceBinding::from_facts`] preconditions the snapshot can decide
/// on its own -- task-owned by this agent's task, `Terminal`, Active, and at the
/// agent's runtime generation -- plus the one rule that arrived with plain
/// shells: a shell is a Terminal resource too, and it is never the provider's.
///
/// Omitting that shell exclusion is a live defect rather than a cosmetic one,
/// because a task's plain shells are usually registered at the same runtime
/// generation as its agent, so a caller that collects "the one Active Terminal
/// resource" finds three and fails closed on a perfectly healthy provider.
///
/// `Ok(None)` means the task genuinely has no provider terminal resource at
/// this generation. `Err` means more than one matched, which no caller may
/// resolve by choosing.
pub fn provider_terminal_resource<'a>(
    snapshot: &'a crate::domain::snapshot::TaskSnapshot,
    agent: &AgentSessionFacts,
) -> Result<Option<&'a ResourceFacts>, ProviderResourceAmbiguity> {
    let matching = snapshot
        .resources
        .values()
        .filter(|resource| {
            resource.task_id == Some(agent.task_id)
                && resource.owner_kind == OwnerKind::Task
                && resource.resource_kind == ResourceKind::Terminal
                && !resource.recipe.is_plain_shell()
                && resource.lifecycle == ResourceLifecycle::Active
                && resource.runtime_generation == agent.runtime_generation
        })
        .collect::<Vec<_>>();
    match matching.as_slice() {
        [] => Ok(None),
        [one] => Ok(Some(one)),
        many => Err(ProviderResourceAmbiguity {
            candidates: many.len(),
        }),
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

    fn snapshot_with(
        resources: Vec<ResourceFacts>,
        agent: &AgentSessionFacts,
    ) -> crate::domain::snapshot::TaskSnapshot {
        use crate::domain::task::{
            ReviewReadiness, TaskActivity, TaskAssignment, TaskAttention, TaskConnectivity,
            TaskFacts, TaskLifecycle, WorkspaceRef,
        };
        let mut snapshot = crate::domain::snapshot::TaskSnapshot {
            task: TaskFacts {
                id: agent.task_id,
                environment_id: crate::domain::id::EnvironmentId::new(),
                title: "Provider resource".into(),
                description: None,
                project_id: crate::domain::id::ProjectId::new(),
                workspace: WorkspaceRef::Main,
                assignment: TaskAssignment::LocalOwner,
                lifecycle: TaskLifecycle::Open,
                action_epoch: 1,
                revision: 1,
                created_at_ms: 1,
            },
            connectivity: TaskConnectivity::Connected,
            attention: TaskAttention::None,
            activity: TaskActivity::Idle,
            review_readiness: ReviewReadiness::NotReady,
            agents: std::collections::BTreeMap::new(),
            primary_agent_id: Some(agent.id),
            artifacts: std::collections::BTreeMap::new(),
            resources: std::collections::BTreeMap::new(),
            provider_sessions: std::collections::BTreeMap::new(),
            browser: crate::domain::browser::BrowserBook::new(),
            terminal_facts: Default::default(),
            terminal_strip: Default::default(),
        };
        snapshot.agents.insert(agent.id, agent.clone());
        for resource in resources {
            snapshot.resources.insert(resource.id, resource);
        }
        snapshot
    }

    fn shell_beside(resource: &ResourceFacts, tail: u8) -> ResourceFacts {
        let mut shell = resource.clone();
        shell.id = ResourceId::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, tail,
        ])
        .expect("shell id");
        shell.recipe = ResourceRecipe::Terminal {
            cols: 80,
            rows: 24,
            launch: Some(crate::domain::resource::TerminalLaunch {
                cwd: std::path::PathBuf::from(if cfg!(windows) { "C:/Code" } else { "/code" }),
                program: std::path::PathBuf::from(if cfg!(windows) {
                    "C:/Windows/System32/cmd.exe"
                } else {
                    "/bin/sh"
                }),
                args: Vec::new(),
            }),
            title: None,
        };
        assert!(shell.recipe.is_plain_shell());
        shell
    }

    #[test]
    fn a_binding_never_points_at_a_plain_shell() {
        let (agent, resource) = facts(7);
        // Same task, same owner, same kind, same generation, Active: a plain
        // shell satisfies every other precondition, which is exactly why the
        // recipe has to be checked.
        let shell = shell_beside(&resource, 4);
        assert_eq!(shell.task_id, resource.task_id);
        assert_eq!(shell.runtime_generation, agent.runtime_generation);
        assert_eq!(shell.lifecycle, ResourceLifecycle::Active);
        assert_eq!(
            AgentResourceBinding::from_facts(&agent, &shell),
            Err(AgentResourceBindingError::ResourceMustNotBePlainShell)
        );
        // The provider's own terminal still binds.
        assert!(AgentResourceBinding::from_facts(&agent, &resource).is_ok());
    }

    #[test]
    fn provider_terminal_resource_ignores_plain_shells_at_the_same_generation() {
        let (agent, resource) = facts(7);
        let shell_a = shell_beside(&resource, 4);
        let shell_b = shell_beside(&resource, 5);
        let snapshot = snapshot_with(
            vec![resource.clone(), shell_a.clone(), shell_b.clone()],
            &agent,
        );
        assert_eq!(
            provider_terminal_resource(&snapshot, &agent)
                .expect("unambiguous")
                .map(|found| found.id),
            Some(resource.id),
            "plain shells are Terminal resources at the same generation and must not count"
        );

        // Shells alone are genuine absence, not a shell promoted to provider.
        let shells_only = snapshot_with(vec![shell_a.clone(), shell_b], &agent);
        assert_eq!(
            provider_terminal_resource(&shells_only, &agent).expect("unambiguous"),
            None
        );
    }

    #[test]
    fn provider_terminal_resource_refuses_to_choose_between_two_candidates() {
        let (agent, resource) = facts(7);
        let mut second = resource.clone();
        second.id = ResourceId::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 9,
        ])
        .expect("second id");
        let snapshot = snapshot_with(vec![resource, second], &agent);
        assert_eq!(
            provider_terminal_resource(&snapshot, &agent),
            Err(ProviderResourceAmbiguity { candidates: 2 })
        );
    }

    #[test]
    fn provider_terminal_resource_requires_active_at_the_agent_generation() {
        let (agent, resource) = facts(7);
        let mut stale = resource.clone();
        stale.runtime_generation = 6;
        assert_eq!(
            provider_terminal_resource(&snapshot_with(vec![stale], &agent), &agent)
                .expect("unambiguous"),
            None
        );
        let mut released = resource.clone();
        released.lifecycle = ResourceLifecycle::Released;
        assert_eq!(
            provider_terminal_resource(&snapshot_with(vec![released], &agent), &agent)
                .expect("unambiguous"),
            None
        );
        // Anything the helper does return must be bindable by the canonical rule.
        let snapshot = snapshot_with(vec![resource], &agent);
        let found = provider_terminal_resource(&snapshot, &agent)
            .expect("unambiguous")
            .expect("present");
        assert!(AgentResourceBinding::from_facts(&agent, found).is_ok());
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
