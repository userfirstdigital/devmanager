//! Host adapters for bounded update handoff.
//!
//! [`HostUpdateHandoff`] / [`HostUpdateRuntimeGate`] live in [`crate::updater::handoff`]
//! so [`crate::updater::UpdaterService`] and the host executor share one FSM without
//! an updater↔host import cycle. This module maps the existing Phase 2
//! [`crate::domain::Query::InspectHostQuit`] seam onto an **owned**
//! [`crate::updater::handoff::ActiveResourceProbe`] (Send + 'static, no borrowed
//! references, no unsafe Send).

use std::sync::{Arc, Mutex};

use uuid::Uuid;

use crate::domain::host::HostQuitInspection;
use crate::domain::resource::ResourceLifecycle;
use crate::kernel::CommandBus;
use crate::updater::handoff::{
    ActiveResourceProbe, ActiveUpdateResource, UpdateResourceInspection,
};

pub use crate::updater::handoff::HostUpdateRuntimeGate;

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

/// Trait for host-connection-facing code that can supply a quit inspection.
///
/// Implementors need not be Send; owned probes wrap Send sources only.
pub trait HostQuitInspectionSource {
    fn inspect_host_quit_for_update(&mut self) -> Result<HostQuitInspection, String>;
}

impl HostQuitInspectionSource for CommandBus {
    fn inspect_host_quit_for_update(&mut self) -> Result<HostQuitInspection, String> {
        self.inspect_host_quit()
            .map_err(|error| format!("InspectHostQuit failed: {error}"))
    }
}

/// Owned Send+'static probe built from a closure (host IPC / executor / tests).
pub struct OwnedActiveResourceProbe {
    inspect: Box<dyn FnMut() -> Result<UpdateResourceInspection, String> + Send>,
}

impl OwnedActiveResourceProbe {
    pub fn from_fn(
        inspect: impl FnMut() -> Result<UpdateResourceInspection, String> + Send + 'static,
    ) -> Self {
        Self {
            inspect: Box::new(inspect),
        }
    }

    /// Own a Send [`HostQuitInspectionSource`] so the probe is Send+'static.
    pub fn from_send_source<S>(source: S, host_boot_id: Uuid) -> Self
    where
        S: HostQuitInspectionSource + Send + 'static,
    {
        let source = Arc::new(Mutex::new(source));
        Self::from_fn(move || {
            let mut guard = source
                .lock()
                .map_err(|_| "update probe source lock is poisoned".to_string())?;
            let inspection = guard.inspect_host_quit_for_update()?;
            Ok(update_inspection_from_host_quit(&inspection, host_boot_id))
        })
    }
}

impl ActiveResourceProbe for OwnedActiveResourceProbe {
    fn inspect_for_update(&mut self) -> Result<UpdateResourceInspection, String> {
        (self.inspect)()
    }
}

impl std::fmt::Debug for OwnedActiveResourceProbe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("OwnedActiveResourceProbe")
    }
}

/// Executor/IPC-backed owned probe installed by the host or native client.
///
/// Constructed from [`crate::host::HostRequestHandle::owned_update_resource_probe`] or a
/// client IPC callback — never from a borrowed `&CommandBus`.
pub struct HostExecutorActiveResourceProbe {
    inner: OwnedActiveResourceProbe,
}

impl HostExecutorActiveResourceProbe {
    pub fn new(inner: OwnedActiveResourceProbe) -> Self {
        Self { inner }
    }

    pub fn into_owned(self) -> OwnedActiveResourceProbe {
        self.inner
    }
}

impl ActiveResourceProbe for HostExecutorActiveResourceProbe {
    fn inspect_for_update(&mut self) -> Result<UpdateResourceInspection, String> {
        self.inner.inspect_for_update()
    }
}

/// Fixed-source probe for contract tests (owned, Send+'static).
pub fn owned_probe_from_quit_inspection(
    inspection: HostQuitInspection,
    host_boot_id: Uuid,
) -> OwnedActiveResourceProbe {
    struct FixedQuit(HostQuitInspection);
    impl HostQuitInspectionSource for FixedQuit {
        fn inspect_host_quit_for_update(&mut self) -> Result<HostQuitInspection, String> {
            Ok(self.0.clone())
        }
    }
    OwnedActiveResourceProbe::from_send_source(FixedQuit(inspection), host_boot_id)
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
    use crate::updater::handoff::{
        FixedActiveResourceProbe, HostUpdateAdmission, HostUpdateHandoff, HostUpdateRuntimeGate,
        SilentReplacementDecision, UpdateResourceInspection,
    };
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
    fn owned_source_probe_feeds_handoff_gate() {
        let boot = Uuid::now_v7();
        let mut probe = owned_probe_from_quit_inspection(
            HostQuitInspection {
                inspection_id: 3,
                agents: Vec::new(),
                resources: Vec::new(),
                worktrees: HostQuitWorktreeInspection::NotInspected,
                confirmable: true,
            },
            boot,
        );
        let mut handoff = HostUpdateHandoff::default();
        let (inspection, decision) = handoff
            .inspect_with_probe(&mut probe, "devmanager/0.4.2", "devmanager-host/0.4.2")
            .expect("probe");
        assert!(inspection.active.is_empty());
        assert_eq!(decision, SilentReplacementDecision::Allowed);
        assert_eq!(handoff.admission(), HostUpdateAdmission::Ready);
    }

    #[test]
    fn runtime_gate_stops_launches_while_draining() {
        let gate = HostUpdateRuntimeGate::new();
        assert!(!gate.stops_new_launches());
        let boot = Uuid::now_v7();
        let mut probe = FixedActiveResourceProbe {
            inspection: UpdateResourceInspection {
                inspection_id: 1,
                host_boot_id: boot,
                active: Vec::new(),
                confirmable: true,
            },
        };
        let token = gate
            .prepare_update(
                &mut probe,
                "0.4.2",
                "devmanager/0.4.2",
                "devmanager-host/0.4.2",
                SystemTime::UNIX_EPOCH + Duration::from_secs(1),
                false,
            )
            .expect("prepare");
        assert!(gate.stops_new_launches());
        assert_eq!(
            gate.abort_pre_install().expect("abort"),
            HostUpdateAdmission::Ready
        );
        assert!(!gate.stops_new_launches());
        assert_eq!(token.host_boot_id, boot);
    }
}
