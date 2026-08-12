//! Production managed-launch authority for configured services.
//!
//! Uses the Phase 3 suspended PTY → Job register → resume handoff and exact
//! Job teardown. No raw PID kill path exists here.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

#[cfg(windows)]
use std::{ffi::OsString, path::PathBuf, sync::atomic::AtomicBool};

#[cfg(windows)]
use portable_pty::{native_pty_system, MasterPty, PtySize, SlavePty};

use crate::{
    domain::id::ResourceId,
    process::identity::ProcessOwner,
    process::teardown::TeardownCompletionStore,
    services::supervisor::{
        ManagedLaunchAuthority, ManagedLaunchSpec, ManagedLaunchStage, SupervisorError,
    },
};

#[cfg(windows)]
use crate::{
    domain::{
        operation::{OperationId, ResourceFence},
        resource::ResourceKind,
    },
    process::{
        job::ManagedProcessJob,
        launcher::{prepare_suspended_pty, PendingManagedLaunch, RegisteredPendingManagedLaunch},
        registry::ProcessRegistry,
        teardown::{
            ManagedTerminalActorHandles, ManagedTerminalIo, ManagedTerminalTeardown,
            TeardownOutcome,
        },
    },
};

#[cfg(not(windows))]
use crate::domain::operation::{OperationId, ResourceFence};

const MAX_SERVICE_AUTHORITY_RESOURCES: usize = 256;

#[derive(Debug, Clone, Copy)]
struct IssuedServiceResource {
    owner: ProcessOwner,
    resource_id: ResourceId,
    generation: u64,
}

struct ServiceAuthorityState {
    next_action_epoch: u64,
    resources: BTreeMap<String, IssuedServiceResource>,
    completion_store: Option<TeardownCompletionStore>,
}

/// Host-owned issuer shared with ProcessManager so service launches mint
/// generations through the same completion-store family as terminals.
pub struct ServiceLaunchIssuer {
    state: Mutex<ServiceAuthorityState>,
}

impl ServiceLaunchIssuer {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(ServiceAuthorityState {
                next_action_epoch: 1,
                resources: BTreeMap::new(),
                completion_store: None,
            }),
        }
    }

    fn issue(
        &self,
        session_id: &str,
        owner: ProcessOwner,
    ) -> Result<(u64, TeardownCompletionStore, OperationId), String> {
        if session_id.trim().is_empty() || session_id.len() > 256 {
            return Err("service authority session identity is invalid".to_string());
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| "service authority issuer poisoned".to_string())?;
        if state.resources.len() >= MAX_SERVICE_AUTHORITY_RESOURCES
            && !state.resources.contains_key(session_id)
        {
            return Err("service authority retention is full".to_string());
        }
        let action_epoch = state.next_action_epoch;
        state.next_action_epoch = state
            .next_action_epoch
            .checked_add(1)
            .ok_or_else(|| "service action epoch space is exhausted".to_string())?;
        let issued = match state.resources.get(session_id).copied() {
            Some(current) if current.owner == owner => IssuedServiceResource {
                generation: current
                    .generation
                    .checked_add(1)
                    .ok_or_else(|| "service runtime generation is exhausted".to_string())?,
                ..current
            },
            _ => IssuedServiceResource {
                owner,
                resource_id: ResourceId::new(),
                generation: 1,
            },
        };
        state.resources.insert(session_id.to_string(), issued);
        if state.completion_store.is_none() {
            #[cfg(windows)]
            {
                state.completion_store = Some(TeardownCompletionStore::for_terminal_host()?);
            }
            #[cfg(not(windows))]
            {
                state.completion_store = Some(TeardownCompletionStore::default());
            }
        }
        let completion_store = state
            .completion_store
            .clone()
            .ok_or_else(|| "service teardown completion store missing".to_string())?;
        Ok((action_epoch, completion_store, OperationId::new()))
    }
}

impl Default for ServiceLaunchIssuer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(windows)]
enum PendingStage {
    Prepared {
        pending: PendingManagedLaunch,
        operation_id: OperationId,
        action_epoch: u64,
        session_id: String,
        completion_store: TeardownCompletionStore,
        io: Arc<ManagedTerminalIo>,
        slave: Box<dyn SlavePty + Send>,
        master: Box<dyn MasterPty + Send>,
    },
    Registered {
        pending: RegisteredPendingManagedLaunch,
        teardown: Arc<ManagedTerminalTeardown>,
        slave: Box<dyn SlavePty + Send>,
        master: Box<dyn MasterPty + Send>,
    },
}

pub struct HostPendingLaunch {
    #[cfg(windows)]
    stage: PendingStage,
    #[cfg(not(windows))]
    _private: (),
}

pub struct HostLiveLaunch {
    #[cfg(windows)]
    teardown: Arc<ManagedTerminalTeardown>,
    #[cfg(windows)]
    fence: ResourceFence,
    #[cfg(windows)]
    _child: Box<dyn portable_pty::Child + Send + Sync>,
    #[cfg(windows)]
    _slave: Box<dyn SlavePty + Send>,
    #[cfg(windows)]
    _master: Box<dyn MasterPty + Send>,
    #[cfg(not(windows))]
    _private: (),
}

/// Production Phase 3 managed-launch authority for configured services.
pub struct HostManagedLaunchAuthority {
    issuer: Arc<ServiceLaunchIssuer>,
    live: BTreeMap<ResourceId, u64>,
}

impl HostManagedLaunchAuthority {
    pub fn new() -> Self {
        Self {
            issuer: Arc::new(ServiceLaunchIssuer::new()),
            live: BTreeMap::new(),
        }
    }

    pub(crate) fn with_issuer(issuer: Arc<ServiceLaunchIssuer>) -> Self {
        Self {
            issuer,
            live: BTreeMap::new(),
        }
    }

    pub(crate) fn issuer(&self) -> Arc<ServiceLaunchIssuer> {
        Arc::clone(&self.issuer)
    }
}

impl Default for HostManagedLaunchAuthority {
    fn default() -> Self {
        Self::new()
    }
}

impl ManagedLaunchAuthority for HostManagedLaunchAuthority {
    type Pending = HostPendingLaunch;
    type Live = HostLiveLaunch;

    fn prepare_suspended(
        &mut self,
        spec: &ManagedLaunchSpec,
    ) -> Result<Self::Pending, SupervisorError> {
        #[cfg(not(windows))]
        {
            let _ = spec;
            Err(SupervisorError::Launch {
                stage: ManagedLaunchStage::Prepare,
            })
        }
        #[cfg(windows)]
        {
            if spec.generation == 0 {
                return Err(SupervisorError::Launch {
                    stage: ManagedLaunchStage::Prepare,
                });
            }
            let session_id = format!("service:{}", spec.display_label);
            let (action_epoch, completion_store, operation_id) = self
                .issuer
                .issue(&session_id, spec.owner)
                .map_err(|_| SupervisorError::Launch {
                    stage: ManagedLaunchStage::Prepare,
                })?;

            let pty_system = native_pty_system();
            let pair = pty_system
                .openpty(PtySize {
                    rows: 24,
                    cols: 80,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .map_err(|_| SupervisorError::Launch {
                    stage: ManagedLaunchStage::Prepare,
                })?;
            let writer = Arc::new(Mutex::new(
                Box::new(std::io::sink()) as Box<dyn std::io::Write + Send>
            ));
            let master_slot = Arc::new(Mutex::new(None));
            let actors = Arc::new(Mutex::new(ManagedTerminalActorHandles::default()));
            let input_admission = Arc::new(AtomicBool::new(false));
            let io = ManagedTerminalIo::new(
                Arc::clone(&writer),
                Arc::clone(&master_slot),
                Arc::clone(&actors),
                Arc::clone(&input_admission),
            );

            let mut environment = BTreeMap::new();
            for (key, value) in &spec.environment {
                environment.insert(OsString::from(key), OsString::from(value));
            }
            let intent = crate::process::launcher::LaunchIntent {
                resource_id: spec.resource_id,
                generation: spec.generation,
                owner: spec.owner,
                kind: ResourceKind::Service,
                executable: PathBuf::from(&spec.program),
                args: spec.args.iter().cloned().map(OsString::from).collect(),
                cwd: PathBuf::from(&spec.cwd),
                environment,
                display_label: spec.display_label.clone(),
            };
            let pending = prepare_suspended_pty(&*pair.slave, intent).map_err(|_| {
                SupervisorError::Launch {
                    stage: ManagedLaunchStage::Prepare,
                }
            })?;
            Ok(HostPendingLaunch {
                stage: PendingStage::Prepared {
                    pending,
                    operation_id,
                    action_epoch,
                    session_id,
                    completion_store,
                    io,
                    slave: pair.slave,
                    master: pair.master,
                },
            })
        }
    }

    fn register_suspended(
        &mut self,
        pending: Self::Pending,
    ) -> Result<Self::Pending, SupervisorError> {
        #[cfg(not(windows))]
        {
            let _ = pending;
            Err(SupervisorError::Launch {
                stage: ManagedLaunchStage::Register,
            })
        }
        #[cfg(windows)]
        {
            let PendingStage::Prepared {
                pending,
                operation_id,
                action_epoch,
                session_id,
                completion_store,
                io,
                slave,
                master,
            } = pending.stage
            else {
                return Err(SupervisorError::Launch {
                    stage: ManagedLaunchStage::Register,
                });
            };
            let mut registry = ProcessRegistry::<ManagedProcessJob>::new();
            let registered =
                pending
                    .register_suspended(&mut registry)
                    .map_err(|_| SupervisorError::Launch {
                        stage: ManagedLaunchStage::Register,
                    })?;
            let fence = registered.fence().clone();
            let teardown = ManagedTerminalTeardown::from_registered_for_service(
                registry,
                fence,
                operation_id,
                action_epoch,
                completion_store,
                session_id,
                Vec::new(),
                io,
            )
            .map_err(|_| SupervisorError::Launch {
                stage: ManagedLaunchStage::Register,
            })?;
            Ok(HostPendingLaunch {
                stage: PendingStage::Registered {
                    pending: registered,
                    teardown,
                    slave,
                    master,
                },
            })
        }
    }

    fn resume(&mut self, pending: Self::Pending) -> Result<Self::Live, SupervisorError> {
        #[cfg(not(windows))]
        {
            let _ = pending;
            Err(SupervisorError::Launch {
                stage: ManagedLaunchStage::Resume,
            })
        }
        #[cfg(windows)]
        {
            let PendingStage::Registered {
                pending,
                teardown,
                slave,
                master,
            } = pending.stage
            else {
                return Err(SupervisorError::Launch {
                    stage: ManagedLaunchStage::Resume,
                });
            };
            teardown.arm_before_resume();
            let child = ManagedTerminalTeardown::resume_registered_for_service(&teardown, pending)
                .map_err(|_| SupervisorError::Launch {
                    stage: ManagedLaunchStage::Resume,
                })?;
            let fence = child.fence().resource();
            self.live
                .insert(fence.resource_id, fence.runtime_generation);
            Ok(HostLiveLaunch {
                teardown,
                fence,
                _child: child.into_child(),
                _slave: slave,
                _master: master,
            })
        }
    }

    fn teardown(
        &mut self,
        live: &mut Option<Self::Live>,
        fence: ResourceFence,
    ) -> Result<(), SupervisorError> {
        #[cfg(not(windows))]
        {
            let _ = (live, fence);
            Err(SupervisorError::TeardownFailed)
        }
        #[cfg(windows)]
        {
            let Some(handle) = live.take() else {
                return Ok(());
            };
            if handle.fence != fence {
                *live = Some(handle);
                return Err(SupervisorError::TeardownFailed);
            }
            match handle.teardown.close() {
                Ok(report) if report.outcome() == TeardownOutcome::Closed => {
                    self.live.remove(&fence.resource_id);
                    Ok(())
                }
                Ok(_) | Err(_) => {
                    *live = Some(handle);
                    Err(SupervisorError::TeardownFailed)
                }
            }
        }
    }

    fn live_count(&self) -> usize {
        self.live.len()
    }

    fn residue_count(&self) -> usize {
        self.live.len()
    }
}
