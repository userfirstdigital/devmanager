//! Host adapters for bounded update handoff.
//!
//! [`HostUpdateHandoff`] lives in [`crate::updater::handoff`] so [`crate::updater::UpdaterService`]
//! can gate installs without a host↔updater cycle. This module maps the existing
//! Phase 2 [`crate::domain::Query::InspectHostQuit`] seam onto
//! [`crate::updater::handoff::ActiveResourceProbe`].

use uuid::Uuid;

use crate::domain::host::HostQuitInspection;
use crate::domain::resource::ResourceLifecycle;
use crate::kernel::CommandBus;
use crate::updater::handoff::{
    ActiveResourceProbe, ActiveUpdateResource, UpdateResourceInspection,
};

pub use crate::updater::handoff::{HostUpdateAdmission, HostUpdateHandoff};

/// Map a durable host-quit inspection into the updater handoff resource summary.
pub fn update_inspection_from_host_quit(
    inspection: &HostQuitInspection,
    host_boot_id: Uuid,
) -> UpdateResourceInspection {
    let mut active = Vec::new();
    for agent in &inspection.agents {
        active.push(ActiveUpdateResource {
            resource_id: agent.agent_session_id.to_string(),
            kind: format!("agent:{}", agent.provider_kind),
            lifecycle: format!("{:?}", agent.lifecycle),
            task_id: Some(agent.task_id.to_string()),
        });
    }
    for resource in &inspection.resources {
        if !matches!(
            resource.lifecycle,
            ResourceLifecycle::Active | ResourceLifecycle::Releasing
        ) {
            continue;
        }
        active.push(ActiveUpdateResource {
            resource_id: resource.resource_id.to_string(),
            kind: format!("{:?}", resource.resource_kind),
            lifecycle: format!("{:?}", resource.lifecycle),
            task_id: resource.task_id.map(|id| id.to_string()),
        });
    }
    UpdateResourceInspection {
        inspection_id: inspection.inspection_id,
        host_boot_id,
        active,
        confirmable: inspection.confirmable,
    }
}

/// Probe that reads live blockers through the existing CommandBus InspectHostQuit path.
pub struct CommandBusActiveResourceProbe<'a> {
    bus: &'a CommandBus,
    host_boot_id: Uuid,
}

impl<'a> CommandBusActiveResourceProbe<'a> {
    pub fn new(bus: &'a CommandBus, host_boot_id: Uuid) -> Self {
        Self { bus, host_boot_id }
    }
}

impl ActiveResourceProbe for CommandBusActiveResourceProbe<'_> {
    fn inspect_for_update(&mut self) -> Result<UpdateResourceInspection, String> {
        let inspection = self
            .bus
            .inspect_host_quit()
            .map_err(|error| format!("InspectHostQuit failed: {error}"))?;
        Ok(update_inspection_from_host_quit(
            &inspection,
            self.host_boot_id,
        ))
    }
}

/// Trait for host-connection-facing code that can supply a quit inspection.
///
/// Phase 2 did not add PrepareUpdate envelopes; HostConnection speaks requests on
/// the pipe while the bus owns InspectHostQuit. Implementors bridge that seam.
pub trait HostQuitInspectionSource {
    fn inspect_host_quit_for_update(&mut self) -> Result<HostQuitInspection, String>;
}

impl HostQuitInspectionSource for CommandBus {
    fn inspect_host_quit_for_update(&mut self) -> Result<HostQuitInspection, String> {
        self.inspect_host_quit()
            .map_err(|error| format!("InspectHostQuit failed: {error}"))
    }
}

/// Probe over any [`HostQuitInspectionSource`] (CommandBus or test double).
pub struct HostConnectionUpdateProbe<'a, S: HostQuitInspectionSource + ?Sized> {
    source: &'a mut S,
    host_boot_id: Uuid,
}

impl<'a, S: HostQuitInspectionSource + ?Sized> HostConnectionUpdateProbe<'a, S> {
    pub fn new(source: &'a mut S, host_boot_id: Uuid) -> Self {
        Self {
            source,
            host_boot_id,
        }
    }
}

impl<S: HostQuitInspectionSource + ?Sized> ActiveResourceProbe
    for HostConnectionUpdateProbe<'_, S>
{
    fn inspect_for_update(&mut self) -> Result<UpdateResourceInspection, String> {
        let inspection = self.source.inspect_host_quit_for_update()?;
        Ok(update_inspection_from_host_quit(
            &inspection,
            self.host_boot_id,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::host::{
        HostQuitAgentBlocker, HostQuitResourceBlocker, HostQuitWorktreeInspection,
    };
    use crate::domain::id::{AgentSessionId, ResourceId, TaskId};
    use crate::domain::resource::{OwnerKind, ResourceKind};
    use crate::domain::{AgentRole, AgentSessionLifecycle};
    use crate::updater::handoff::{FixedActiveResourceProbe, SilentReplacementDecision};
    use std::time::{Duration, SystemTime};

    #[test]
    fn host_quit_inspection_maps_active_and_releasing_resources() {
        let inspection = HostQuitInspection {
            inspection_id: 9,
            agents: vec![HostQuitAgentBlocker {
                agent_session_id: AgentSessionId::new(),
                task_id: TaskId::new(),
                task_title: "t".into(),
                role: AgentRole::Primary,
                provider_kind: "claude".into(),
                lifecycle: AgentSessionLifecycle::Open,
                runtime_generation: 1,
            }],
            resources: vec![HostQuitResourceBlocker {
                resource_id: ResourceId::new(),
                task_id: Some(TaskId::new()),
                task_title: Some("t".into()),
                owner_kind: OwnerKind::Task,
                resource_kind: ResourceKind::Terminal,
                lifecycle: ResourceLifecycle::Active,
                runtime_generation: 1,
            }],
            worktrees: HostQuitWorktreeInspection::NotInspected,
            confirmable: true,
        };
        let mapped = update_inspection_from_host_quit(&inspection, Uuid::nil());
        assert_eq!(mapped.inspection_id, 9);
        assert_eq!(mapped.active.len(), 2);
    }

    #[test]
    fn host_connection_probe_trait_feeds_handoff_gate() {
        struct FixedQuit(HostQuitInspection);
        impl HostQuitInspectionSource for FixedQuit {
            fn inspect_host_quit_for_update(&mut self) -> Result<HostQuitInspection, String> {
                Ok(self.0.clone())
            }
        }

        let mut source = FixedQuit(HostQuitInspection {
            inspection_id: 3,
            agents: Vec::new(),
            resources: Vec::new(),
            worktrees: HostQuitWorktreeInspection::NotInspected,
            confirmable: true,
        });
        let mut probe = HostConnectionUpdateProbe::new(&mut source, Uuid::now_v7());
        let mut handoff = HostUpdateHandoff::default();
        let (inspection, decision) = handoff
            .inspect_with_probe(&mut probe, "devmanager/0.4.2", "devmanager-host/0.4.2")
            .expect("probe");
        assert!(inspection.active.is_empty());
        assert_eq!(decision, SilentReplacementDecision::Allowed);
        assert_eq!(handoff.admission(), HostUpdateAdmission::Ready);
    }

    #[test]
    fn abort_before_irreversible_returns_ready() {
        let boot = Uuid::now_v7();
        let mut handoff = HostUpdateHandoff::default();
        let mut probe = FixedActiveResourceProbe {
            inspection: UpdateResourceInspection {
                inspection_id: 1,
                host_boot_id: boot,
                active: Vec::new(),
                confirmable: true,
            },
        };
        let token = handoff
            .run_pre_install_gate(
                &mut probe,
                "0.4.2",
                "devmanager/0.4.2",
                "devmanager-host/0.4.2",
                SystemTime::UNIX_EPOCH + Duration::from_secs(1),
                false,
            )
            .expect("gate");
        assert!(!handoff.install_irreversible());
        assert_eq!(
            handoff.abort_pre_install().expect("abort"),
            HostUpdateAdmission::Ready
        );
        assert_eq!(token.host_boot_id, boot);
    }
}
