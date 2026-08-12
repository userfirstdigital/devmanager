//! Configured command/service supervisor on top of Phase 3 Job ownership.
//!
//! Start/stop/restart execute admitted plans through a managed suspended
//! Job/process authority. Readiness and liveness probes are scheduled only;
//! they never run on the action or projection hot path. Port reservations and
//! proven-external listeners stay on a separate observation axis.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fmt,
};

use crate::{
    domain::{id::ResourceId, operation::ResourceFence, resource::ResourceKind, TaskId},
    process::{identity::ProcessOwner, ports::PortAuthority},
    services::{
        binding::{ConfiguredServiceBinding, ConfiguredServiceOwner, EnvironmentOverlay},
        health::{
            reduce_service, EvidenceProvenance, EvidenceSource, HealthAxis, HealthTracker,
            LifecycleAxis, OwnershipAxis, PortAxis, ProbeOutcome, ProcessAxis,
            RedactedServiceSnapshot, ServiceEvidence, ServiceState,
        },
        launch_authority::HostManagedLaunchAuthority,
        model::{
            ActionEpoch, ActiveOperation, AdmissionDecision, AdmissionFence, AdmissionRejection,
            AdmissionRequest, AdmissionRequester, AdmissionSnapshot, HostId, LaunchIntent,
            RuntimeOwnership, RuntimeRecord, ServiceAction, ServiceCatalog, ServiceFence,
            ServiceId, ServiceScope, StartPlan, StopPlan, TaskClosePlan, ValidationError,
        },
    },
    state::SessionStatus,
};

pub const MAX_SERVICE_LOG_LINES: usize = 64;
pub const MAX_SERVICE_LOG_LINE_BYTES: usize = 256;
pub const MAX_SUPERVISOR_EVENTS: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SupervisorAction {
    Start,
    Stop,
    Restart,
    Logs,
    Health,
}

impl SupervisorAction {
    fn mutating(self) -> Option<ServiceAction> {
        match self {
            Self::Start => Some(ServiceAction::Start),
            Self::Stop => Some(ServiceAction::Stop),
            Self::Restart => Some(ServiceAction::Restart),
            Self::Logs | Self::Health => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProbeKind {
    Readiness,
    Liveness,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DueProbe {
    pub service_id: ServiceId,
    pub generation: u64,
    pub kind: ProbeKind,
    pub due_at_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RedactedLogLine {
    pub observed_at_ms: u64,
    pub generation: u64,
    pub text: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BoundedServiceLog {
    pub service_id: ServiceId,
    pub generation: u64,
    pub lines: Vec<RedactedLogLine>,
    pub truncated: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SupervisorEventKind {
    Started,
    Stopped,
    Failed,
    Crashed,
    Coalesced,
    PortReserved,
    PortExternal,
    ProbeScheduled,
    TaskClosed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RedactedSupervisorEvent {
    pub service_id: Option<ServiceId>,
    pub kind: SupervisorEventKind,
    pub generation: u64,
    pub observed_at_ms: u64,
}

/// Public refusal class. Admission internals stay crate-private.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SupervisorRefusal {
    StaleFence,
    Ownership,
    External,
    Dependency,
    OperationInProgress,
    AlreadyRunning,
    AlreadyStopped,
    EvidenceUnknown,
    TaskClosing,
    Other,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SupervisorError {
    UnknownService(ServiceId),
    Binding(ValidationError),
    Refused(SupervisorRefusal),
    StaleGeneration { expected: u64, received: u64 },
    Launch { stage: ManagedLaunchStage },
    PortBusy { port: u16 },
    ExternalPort { port: u16 },
    ProbeHotPath,
    TeardownFailed,
}

impl fmt::Display for SupervisorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownService(id) => write!(formatter, "unknown service {id}"),
            Self::Binding(error) => error.fmt(formatter),
            Self::Refused(_) => formatter.write_str("service action refused"),
            Self::StaleGeneration { .. } => formatter.write_str("stale service generation"),
            Self::Launch { stage } => write!(formatter, "managed launch failed at {stage:?}"),
            Self::PortBusy { port } => write!(formatter, "port {port} is reserved"),
            Self::ExternalPort { port } => write!(formatter, "port {port} is externally owned"),
            Self::ProbeHotPath => formatter.write_str("probe executed on the hot path"),
            Self::TeardownFailed => formatter.write_str("managed teardown failed"),
        }
    }
}

impl std::error::Error for SupervisorError {}

impl From<crate::services::binding::BindingError> for SupervisorError {
    fn from(error: crate::services::binding::BindingError) -> Self {
        match error {
            crate::services::binding::BindingError::Validation(error) => Self::Binding(error),
            crate::services::binding::BindingError::TooManyServices { limit } => {
                Self::Binding(ValidationError::TooMany {
                    field: crate::services::model::ValidationField::ServiceId,
                    limit,
                })
            }
            crate::services::binding::BindingError::DuplicateCommand { .. }
            | crate::services::binding::BindingError::ArchivedCommand
            | crate::services::binding::BindingError::InvalidWorkspaceRoot => {
                Self::Binding(ValidationError::InvalidIdentifier {
                    field: crate::services::model::ValidationField::Cwd,
                })
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManagedLaunchStage {
    Prepare,
    Register,
    Resume,
    Teardown,
}

#[derive(Clone, Eq, PartialEq)]
pub struct ManagedLaunchSpec {
    pub resource_id: ResourceId,
    pub generation: u64,
    pub owner: ProcessOwner,
    pub kind: ResourceKind,
    pub program: String,
    pub args: Vec<String>,
    /// Absolute launch working directory resolved from the configured
    /// project/workspace root. Never the DevManager process cwd.
    pub cwd: String,
    pub environment: BTreeMap<String, String>,
    pub display_label: String,
}

impl fmt::Debug for ManagedLaunchSpec {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ManagedLaunchSpec")
            .field("resource_id", &self.resource_id)
            .field("generation", &self.generation)
            .field("owner", &self.owner)
            .field("kind", &self.kind)
            .field("program", &"<redacted>")
            .field("args", &format_args!("<{} redacted>", self.args.len()))
            .field("cwd", &"<redacted>")
            .field(
                "environment",
                &format_args!("<{} redacted>", self.environment.len()),
            )
            .field("display_label", &self.display_label)
            .finish()
    }
}

pub trait ManagedLaunchAuthority {
    type Pending;
    type Live;

    fn prepare_suspended(
        &mut self,
        spec: &ManagedLaunchSpec,
    ) -> Result<Self::Pending, SupervisorError>;
    fn register_suspended(
        &mut self,
        pending: Self::Pending,
    ) -> Result<Self::Pending, SupervisorError>;
    fn resume(&mut self, pending: Self::Pending) -> Result<Self::Live, SupervisorError>;
    /// Exact Job teardown. On failure the live handle must be restored into
    /// `live` so the caller can retry; never raw-PID kill.
    fn teardown(
        &mut self,
        live: &mut Option<Self::Live>,
        fence: ResourceFence,
    ) -> Result<(), SupervisorError>;
    /// Drain newly captured PTY output lines for the live generation.
    fn drain_output_lines(&self, live: &Self::Live) -> Vec<String>;
    /// Observe a waiter exit for the live generation. `None` means still live;
    /// `Some(code)` is a terminal exit observation.
    fn take_exit(&self, live: &Self::Live) -> Option<Option<i32>>;
    fn live_generation(live: &Self::Live) -> u64;
    fn live_count(&self) -> usize;
    fn residue_count(&self) -> usize;
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum PortClaim {
    Reserved {
        service_id: ServiceId,
        generation: u64,
    },
    External {
        owner_pid: Option<u32>,
    },
    /// Unknown / ProbeError observations fail closed until a Free or
    /// ProvenExternal proof replaces them.
    Indeterminate,
}

struct ServiceRuntime<A: ManagedLaunchAuthority> {
    binding_owner: ConfiguredServiceOwner,
    workspace_root: String,
    resource_id: ResourceId,
    overlay: EnvironmentOverlay,
    evidence: ServiceEvidence,
    health: Option<HealthTracker>,
    pending: Option<A::Pending>,
    live: Option<A::Live>,
    operation: Option<ActiveOperation>,
    logs: VecDeque<RedactedLogLine>,
    log_truncated: bool,
}

pub struct ServiceSupervisor<A: ManagedLaunchAuthority> {
    catalog: ServiceCatalog,
    snapshot: AdmissionSnapshot,
    host_id: HostId,
    now_ms: u64,
    next_operation_id: u64,
    authority: A,
    runtimes: BTreeMap<ServiceId, ServiceRuntime<A>>,
    ports: BTreeMap<u16, PortClaim>,
    events: VecDeque<RedactedSupervisorEvent>,
    probe_executions: usize,
}

/// Production supervisor bound to the Phase 3 suspended Job/PTY authority.
pub type ConfiguredServiceSupervisor = ServiceSupervisor<HostManagedLaunchAuthority>;

impl<A: ManagedLaunchAuthority> ServiceSupervisor<A> {
    pub fn from_bindings(
        bindings: Vec<ConfiguredServiceBinding>,
        authority: A,
        host_id: HostId,
        now_ms: u64,
    ) -> Result<Self, SupervisorError> {
        let mut overlays = BTreeMap::new();
        let mut owners = BTreeMap::new();
        let mut roots = BTreeMap::new();
        let mut definitions = Vec::new();
        for binding in bindings {
            overlays.insert(binding.definition.id.clone(), binding.environment);
            owners.insert(binding.definition.id.clone(), binding.owner);
            roots.insert(binding.definition.id.clone(), binding.workspace_root);
            definitions.push(binding.definition);
        }
        let catalog = ServiceCatalog::new(definitions).map_err(SupervisorError::Binding)?;
        Self::from_catalog_with_roots(catalog, overlays, owners, roots, authority, host_id, now_ms)
    }

    pub fn from_catalog(
        catalog: ServiceCatalog,
        overlays: BTreeMap<ServiceId, EnvironmentOverlay>,
        owners: BTreeMap<ServiceId, ConfiguredServiceOwner>,
        authority: A,
        host_id: HostId,
        now_ms: u64,
    ) -> Result<Self, SupervisorError> {
        Self::from_catalog_with_roots(
            catalog,
            overlays,
            owners,
            BTreeMap::new(),
            authority,
            host_id,
            now_ms,
        )
    }

    pub fn from_catalog_with_workspace_roots(
        catalog: ServiceCatalog,
        overlays: BTreeMap<ServiceId, EnvironmentOverlay>,
        owners: BTreeMap<ServiceId, ConfiguredServiceOwner>,
        workspace_roots: BTreeMap<ServiceId, String>,
        authority: A,
        host_id: HostId,
        now_ms: u64,
    ) -> Result<Self, SupervisorError> {
        Self::from_catalog_with_roots(
            catalog,
            overlays,
            owners,
            workspace_roots,
            authority,
            host_id,
            now_ms,
        )
    }

    fn from_catalog_with_roots(
        catalog: ServiceCatalog,
        overlays: BTreeMap<ServiceId, EnvironmentOverlay>,
        owners: BTreeMap<ServiceId, ConfiguredServiceOwner>,
        workspace_roots: BTreeMap<ServiceId, String>,
        authority: A,
        host_id: HostId,
        now_ms: u64,
    ) -> Result<Self, SupervisorError> {
        let mut snapshot = AdmissionSnapshot::default();
        let mut runtimes = BTreeMap::new();
        for definition in catalog.definitions() {
            let fence = AdmissionFence::new(1, 1, 1);
            snapshot.set_service(
                definition.id.clone(),
                RuntimeRecord {
                    state: ServiceState::Stopped,
                    fence,
                    ownership: RuntimeOwnership::None,
                    operation: None,
                },
            );
            runtimes.insert(
                definition.id.clone(),
                ServiceRuntime {
                    binding_owner: owners
                        .get(&definition.id)
                        .cloned()
                        .unwrap_or_else(|| owner_from_scope(&definition.scope)),
                    workspace_root: workspace_roots
                        .get(&definition.id)
                        .cloned()
                        .unwrap_or_default(),
                    resource_id: ResourceId::new(),
                    overlay: overlays.get(&definition.id).cloned().unwrap_or_else(|| {
                        EnvironmentOverlay::from_names(
                            definition
                                .command
                                .env()
                                .iter()
                                .map(|reference| reference.name().to_owned()),
                        )
                    }),
                    evidence: stopped_evidence(now_ms, 1, 1),
                    health: definition.health.policy().copied().map(HealthTracker::new),
                    pending: None,
                    live: None,
                    operation: None,
                    logs: VecDeque::new(),
                    log_truncated: false,
                },
            );
        }
        Ok(Self {
            catalog,
            snapshot,
            host_id,
            now_ms,
            next_operation_id: 1,
            authority,
            runtimes,
            ports: BTreeMap::new(),
            events: VecDeque::new(),
            probe_executions: 0,
        })
    }

    pub fn now_ms(&self) -> u64 {
        self.now_ms
    }

    pub fn advance_clock(&mut self, now_ms: u64) {
        self.now_ms = self.now_ms.max(now_ms);
        let ids: Vec<ServiceId> = self.runtimes.keys().cloned().collect();
        for id in ids {
            let Some(runtime) = self.runtimes.get_mut(&id) else {
                continue;
            };
            let generation = runtime.evidence.generation;
            if let Some(tracker) = runtime.health.as_mut() {
                let _ = tracker.advance(self.now_ms, generation);
                runtime.evidence.health = tracker.axis();
            }
            self.project_runtime(&id);
        }
    }

    pub fn handle(
        &mut self,
        action: SupervisorAction,
        service_id: &ServiceId,
        fence: AdmissionFence,
        requester: AdmissionRequester,
    ) -> Result<SupervisorOutcome, SupervisorError> {
        self.pump_io();
        match action {
            SupervisorAction::Logs => self.logs(service_id, fence).map(SupervisorOutcome::Logs),
            SupervisorAction::Health => self
                .health(service_id, fence)
                .map(SupervisorOutcome::Health),
            SupervisorAction::Start | SupervisorAction::Stop | SupervisorAction::Restart => {
                let service_action = action.mutating().expect("mutating action");
                let request = match requester {
                    AdmissionRequester::Task(task_id) => AdmissionRequest::for_task(
                        service_action,
                        service_id.clone(),
                        fence,
                        task_id,
                    ),
                    AdmissionRequester::Host(_) => AdmissionRequest::for_host(
                        service_action,
                        service_id.clone(),
                        fence,
                        self.host_id,
                    ),
                };
                match self.catalog.admit(request, &self.snapshot) {
                    AdmissionDecision::Coalesced {
                        service,
                        operation_id,
                        action,
                    } => {
                        self.push_event(
                            Some(service.clone()),
                            SupervisorEventKind::Coalesced,
                            fence.resource_generation(),
                        );
                        Ok(SupervisorOutcome::Coalesced {
                            service,
                            operation_id,
                            action,
                        })
                    }
                    AdmissionDecision::Refused(rejection) => {
                        Err(SupervisorError::Refused(refusal_from(rejection)))
                    }
                    AdmissionDecision::Start(plan) => {
                        plan.revalidate(&self.catalog, &self.snapshot)
                            .map_err(|rejection| {
                                SupervisorError::Refused(refusal_from(rejection))
                            })?;
                        self.execute_start(plan)
                    }
                    AdmissionDecision::Stop(plan) => {
                        plan.revalidate(&self.catalog, &self.snapshot)
                            .map_err(|rejection| {
                                SupervisorError::Refused(refusal_from(rejection))
                            })?;
                        self.execute_stop(plan, false)
                    }
                    AdmissionDecision::Restart(plan) => {
                        plan.revalidate(&self.catalog, &self.snapshot)
                            .map_err(|rejection| {
                                SupervisorError::Refused(refusal_from(rejection))
                            })?;
                        self.execute_stop(plan.stop().clone(), true)?;
                        self.execute_start(plan.start().clone())
                    }
                }
            }
        }
    }

    pub fn close_task(
        &mut self,
        task_id: TaskId,
        epoch: u64,
    ) -> Result<SupervisorOutcome, SupervisorError> {
        let epoch = ActionEpoch::new(epoch);
        self.snapshot.mark_task_closing(task_id);
        self.snapshot.set_task_epoch(task_id, epoch);
        let plan = self
            .catalog
            .admit_task_close(task_id, epoch, &self.snapshot)
            .map_err(|rejection| SupervisorError::Refused(refusal_from(rejection)))?;
        plan.revalidate(&self.catalog, &self.snapshot)
            .map_err(|rejection| SupervisorError::Refused(refusal_from(rejection)))?;
        self.execute_task_close(plan)
    }

    pub fn observe_port(&mut self, port: u16, authority: PortAuthority, owner_pid: Option<u32>) {
        match authority {
            PortAuthority::ProvenExternal => {
                // An external scan cannot supersede an exact live claim from
                // this supervisor.  The managed listener may be observed in
                // the same refresh as the external scan; preserving the
                // reservation keeps that race from painting a live service
                // blue or dropping its stop authority.
                if self.port_is_live_managed(port) {
                    return;
                }
                self.ports.insert(port, PortClaim::External { owner_pid });
                let ids = self.service_ids_for_port(port);
                for id in ids {
                    if self
                        .runtimes
                        .get(&id)
                        .is_some_and(|runtime| runtime.live.is_some())
                    {
                        continue;
                    }
                    self.mark_external(&id, port, owner_pid);
                }
            }
            PortAuthority::Free => {
                let was_blocking = matches!(
                    self.ports.get(&port),
                    Some(PortClaim::External { .. } | PortClaim::Indeterminate)
                );
                if was_blocking {
                    self.ports.remove(&port);
                    self.clear_external_for_port(port);
                }
            }
            PortAuthority::Managed => {
                // A current managed-resource proof is useful only when it
                // reconciles to this supervisor's exact live service.  If it
                // belongs to another managed resource, retain a blocking
                // indeterminate claim rather than treating it as external or
                // attempting to adopt it.
                if let Some((service_id, generation)) = self
                    .service_ids_for_port(port)
                    .into_iter()
                    .find_map(|service_id| {
                        self.runtimes.get(&service_id).and_then(|runtime| {
                            runtime
                                .live
                                .as_ref()
                                .map(|_| (service_id, runtime.evidence.generation))
                        })
                    })
                {
                    let now_ms = self.now_ms;
                    if let Some(runtime) = self.runtimes.get_mut(&service_id) {
                        runtime.evidence.port = PortAxis::Owned { port };
                        runtime.evidence.ownership = match &runtime.binding_owner {
                            ConfiguredServiceOwner::Task { task_id } => OwnershipAxis::Task {
                                task_id: task_id.to_string(),
                            },
                            ConfiguredServiceOwner::Project { .. }
                            | ConfiguredServiceOwner::Workspace { .. } => OwnershipAxis::Host,
                        };
                        runtime.evidence.observed_at_ms = now_ms;
                        runtime.evidence.provenance = EvidenceProvenance {
                            source: EvidenceSource::ProcessRegistry,
                            observed_at_ms: now_ms,
                            generation: Some(generation),
                            epoch: Some(runtime.evidence.epoch),
                        };
                    }
                    self.ports.insert(
                        port,
                        PortClaim::Reserved {
                            service_id,
                            generation,
                        },
                    );
                    return;
                }
                self.ports.insert(port, PortClaim::Indeterminate);
                for id in self.service_ids_for_port(port) {
                    if self
                        .runtimes
                        .get(&id)
                        .is_some_and(|runtime| runtime.live.is_some())
                    {
                        continue;
                    }
                    self.mark_port_indeterminate(&id, port);
                }
            }
            PortAuthority::Unknown | PortAuthority::ProbeError => {
                self.ports.insert(port, PortClaim::Indeterminate);
                let ids = self.service_ids_for_port(port);
                for id in ids {
                    if self
                        .runtimes
                        .get(&id)
                        .is_some_and(|runtime| runtime.live.is_some())
                    {
                        continue;
                    }
                    self.mark_port_indeterminate(&id, port);
                }
            }
        }
    }

    fn service_ids_for_port(&self, port: u16) -> Vec<ServiceId> {
        self.catalog
            .definitions()
            .filter(|definition| {
                definition
                    .expected_port
                    .is_some_and(|expected| expected.port == port)
            })
            .map(|definition| definition.id.clone())
            .collect()
    }

    fn port_is_live_managed(&self, port: u16) -> bool {
        let Some(PortClaim::Reserved {
            service_id,
            generation,
        }) = self.ports.get(&port)
        else {
            return false;
        };
        self.runtimes.get(service_id).is_some_and(|runtime| {
            runtime.live.is_some() && runtime.evidence.generation == *generation
        })
    }

    pub fn due_probes(&self) -> Vec<DueProbe> {
        let mut due = Vec::new();
        for (id, runtime) in &self.runtimes {
            let Some(tracker) = runtime.health.as_ref() else {
                continue;
            };
            let schedule = tracker.schedule();
            if !schedule.is_due(self.now_ms) {
                continue;
            }
            let kind = if matches!(
                runtime.evidence.lifecycle,
                LifecycleAxis::Starting | LifecycleAxis::Running
            ) && !matches!(tracker.axis(), HealthAxis::Healthy { .. })
            {
                ProbeKind::Readiness
            } else {
                ProbeKind::Liveness
            };
            due.push(DueProbe {
                service_id: id.clone(),
                generation: runtime.evidence.generation,
                kind,
                due_at_ms: schedule.next_probe_at_ms.unwrap_or(self.now_ms),
            });
        }
        due
    }

    pub fn apply_probe(
        &mut self,
        service_id: &ServiceId,
        generation: u64,
        outcome: ProbeOutcome,
    ) -> Result<ServiceState, SupervisorError> {
        self.probe_executions = self.probe_executions.saturating_add(1);
        let runtime = self
            .runtimes
            .get_mut(service_id)
            .ok_or_else(|| SupervisorError::UnknownService(service_id.clone()))?;
        if runtime.evidence.generation != generation {
            return Err(SupervisorError::StaleGeneration {
                expected: runtime.evidence.generation,
                received: generation,
            });
        }
        if let Some(tracker) = runtime.health.as_mut() {
            tracker
                .record_probe(
                    self.now_ms,
                    generation,
                    outcome,
                    EvidenceSource::HealthProbe,
                )
                .map_err(|_| SupervisorError::StaleGeneration {
                    expected: generation,
                    received: generation,
                })?;
            runtime.evidence.health = tracker.axis();
            if matches!(
                tracker.axis(),
                HealthAxis::Healthy { .. } | HealthAxis::Unhealthy { .. }
            ) {
                runtime.evidence.lifecycle = LifecycleAxis::Running;
                runtime.operation = None;
            }
            runtime.evidence.observed_at_ms = self.now_ms;
            runtime.evidence.provenance = EvidenceProvenance {
                source: EvidenceSource::HealthProbe,
                observed_at_ms: self.now_ms,
                generation: Some(generation),
                epoch: Some(runtime.evidence.epoch),
            };
        }
        self.project_runtime(service_id);
        Ok(self.state(service_id))
    }

    pub fn report_exit(
        &mut self,
        service_id: &ServiceId,
        generation: u64,
        exit_code: Option<i32>,
    ) -> Result<ServiceState, SupervisorError> {
        let runtime = self
            .runtimes
            .get_mut(service_id)
            .ok_or_else(|| SupervisorError::UnknownService(service_id.clone()))?;
        if runtime.evidence.generation != generation {
            return Err(SupervisorError::StaleGeneration {
                expected: runtime.evidence.generation,
                received: generation,
            });
        }
        let resource_id = runtime.resource_id;
        let fence = ResourceFence::new(resource_id, generation);
        if runtime.live.is_some() {
            self.authority.teardown(
                &mut self
                    .runtimes
                    .get_mut(service_id)
                    .ok_or_else(|| SupervisorError::UnknownService(service_id.clone()))?
                    .live,
                fence,
            )?;
        }
        let runtime = self
            .runtimes
            .get_mut(service_id)
            .ok_or_else(|| SupervisorError::UnknownService(service_id.clone()))?;
        if let Some(tracker) = runtime.health.as_mut() {
            let _ = tracker.process_exit(self.now_ms, generation);
            runtime.evidence.health = tracker.axis();
        } else {
            runtime.evidence.health = HealthAxis::Crashed;
        }
        runtime.evidence.lifecycle = LifecycleAxis::Failed;
        runtime.evidence.process = ProcessAxis::Crashed { generation };
        // Teardown released the exact service reservation.  Do not leave the
        // prior Owned port/Host ownership in the failure snapshot: that would
        // make a crashed process look like it still owns its listener and
        // could block a safe subsequent start forever.
        runtime.evidence.port = PortAxis::Free;
        runtime.evidence.ownership = OwnershipAxis::None;
        runtime.pending = None;
        runtime.operation = None;
        self.release_port_for(service_id, generation);
        self.push_event(
            Some(service_id.clone()),
            SupervisorEventKind::Crashed,
            generation,
        );
        if let Some(code) = exit_code {
            self.append_log(service_id, generation, format!("exited {code}"));
        }
        self.project_runtime(service_id);
        Ok(self.state(service_id))
    }

    pub fn logs(
        &self,
        service_id: &ServiceId,
        fence: AdmissionFence,
    ) -> Result<BoundedServiceLog, SupervisorError> {
        self.require_fence(service_id, fence)?;
        let runtime = self
            .runtimes
            .get(service_id)
            .ok_or_else(|| SupervisorError::UnknownService(service_id.clone()))?;
        Ok(BoundedServiceLog {
            service_id: service_id.clone(),
            generation: runtime.evidence.generation,
            lines: runtime.logs.iter().cloned().collect(),
            truncated: runtime.log_truncated,
        })
    }

    pub fn health(
        &self,
        service_id: &ServiceId,
        fence: AdmissionFence,
    ) -> Result<RedactedServiceSnapshot, SupervisorError> {
        self.require_fence(service_id, fence)?;
        let definition = self
            .catalog
            .definition(service_id)
            .ok_or_else(|| SupervisorError::UnknownService(service_id.clone()))?;
        let runtime = self
            .runtimes
            .get(service_id)
            .ok_or_else(|| SupervisorError::UnknownService(service_id.clone()))?;
        Ok(RedactedServiceSnapshot::from_evidence(
            service_id.clone(),
            definition.scope.clone(),
            &runtime.evidence,
        ))
    }

    pub fn state(&self, service_id: &ServiceId) -> ServiceState {
        self.runtimes
            .get(service_id)
            .map(|runtime| reduce_service(&runtime.evidence))
            .unwrap_or(ServiceState::Unknown)
    }

    pub fn session_status(&self, service_id: &ServiceId) -> SessionStatus {
        session_status_for_ui(self.state(service_id))
    }

    pub fn snapshot(
        &self,
        service_id: &ServiceId,
    ) -> Result<RedactedServiceSnapshot, SupervisorError> {
        let fence = self.fence(service_id)?;
        self.health(service_id, fence)
    }

    pub fn fence(&self, service_id: &ServiceId) -> Result<AdmissionFence, SupervisorError> {
        self.snapshot
            .service(service_id)
            .map(|runtime| runtime.fence)
            .ok_or_else(|| SupervisorError::UnknownService(service_id.clone()))
    }

    pub fn events(&self) -> impl Iterator<Item = &RedactedSupervisorEvent> {
        self.events.iter()
    }

    pub fn probe_executions(&self) -> usize {
        self.probe_executions
    }

    pub fn live_count(&self) -> usize {
        self.authority.live_count()
    }

    pub fn residue_count(&self) -> usize {
        self.authority.residue_count()
    }

    pub fn catalog_definitions(&self) -> Vec<crate::services::model::ServiceDefinition> {
        self.catalog.definitions().cloned().collect()
    }

    pub fn service_ids(&self) -> Vec<ServiceId> {
        self.catalog
            .definitions()
            .map(|definition| definition.id.clone())
            .collect()
    }

    pub fn dependency_labels(&self) -> Vec<(ServiceId, Vec<ServiceId>)> {
        self.catalog
            .definitions()
            .map(|definition| (definition.id.clone(), definition.dependencies().to_vec()))
            .collect()
    }

    pub fn port_claim(&self, port: u16) -> Option<PortClaimView> {
        match self.ports.get(&port) {
            Some(PortClaim::Reserved {
                service_id,
                generation,
            }) => Some(PortClaimView::Reserved {
                service_id: service_id.clone(),
                generation: *generation,
            }),
            Some(PortClaim::External { owner_pid }) => Some(PortClaimView::External {
                owner_pid: *owner_pid,
            }),
            Some(PortClaim::Indeterminate) => Some(PortClaimView::Indeterminate),
            None => None,
        }
    }

    fn execute_start(&mut self, plan: StartPlan) -> Result<SupervisorOutcome, SupervisorError> {
        let mut started = Vec::new();
        for item in plan.members() {
            if item.intent().is_none() {
                continue;
            }
            match self.start_member(item.service_id(), item.fence(), item.intent().unwrap()) {
                Ok(()) => started.push(item.service_id().clone()),
                Err(error) => {
                    self.fail_member(item.service_id(), item.fence().resource_generation().get());
                    return Err(error);
                }
            }
        }
        Ok(SupervisorOutcome::Changed {
            services: started,
            state: self.state(plan.root()),
        })
    }

    fn execute_stop(
        &mut self,
        plan: StopPlan,
        for_restart: bool,
    ) -> Result<SupervisorOutcome, SupervisorError> {
        let mut stopped = Vec::new();
        for item in plan.members() {
            self.stop_member(item.service_id(), item.fence(), for_restart)?;
            stopped.push(item.service_id().clone());
        }
        Ok(SupervisorOutcome::Changed {
            services: stopped,
            state: self.state(plan.root()),
        })
    }

    fn execute_task_close(
        &mut self,
        plan: TaskClosePlan,
    ) -> Result<SupervisorOutcome, SupervisorError> {
        let mut stopped = Vec::new();
        for item in plan.members() {
            if matches!(self.state(item.service_id()), ServiceState::Stopped) {
                continue;
            }
            self.stop_member(item.service_id(), item.fence(), false)?;
            stopped.push(item.service_id().clone());
        }
        self.push_event(None, SupervisorEventKind::TaskClosed, plan.epoch().get());
        Ok(SupervisorOutcome::Changed {
            services: stopped,
            state: ServiceState::Stopped,
        })
    }

    fn start_member(
        &mut self,
        service_id: &ServiceId,
        fence: &ServiceFence,
        intent: &LaunchIntent,
    ) -> Result<(), SupervisorError> {
        let admitted_generation = fence.resource_generation().get();
        let admitted_epoch = fence.action_epoch().get();
        let runtime = self
            .runtimes
            .get(service_id)
            .ok_or_else(|| SupervisorError::UnknownService(service_id.clone()))?;
        let current_generation = runtime.evidence.generation;
        let current_epoch = runtime.evidence.epoch;
        // Direct start/stop sees an exact fence match. Restart stop advances
        // first, so start is allowed when current is exactly admitted+1.
        let restart_continuation = current_generation == admitted_generation.saturating_add(1)
            && current_epoch == admitted_epoch.saturating_add(1);
        if current_generation != admitted_generation && !restart_continuation {
            return Err(SupervisorError::Refused(SupervisorRefusal::StaleFence));
        }

        if let Some(port) = intent.expected_port().map(|port| port.port) {
            match self.ports.get(&port) {
                Some(PortClaim::External { .. }) => {
                    self.mark_external(service_id, port, None);
                    return Err(SupervisorError::ExternalPort { port });
                }
                Some(PortClaim::Indeterminate) => {
                    self.mark_port_indeterminate(service_id, port);
                    return Err(SupervisorError::Refused(SupervisorRefusal::EvidenceUnknown));
                }
                Some(PortClaim::Reserved {
                    service_id: owner,
                    generation: claimed,
                }) if owner != service_id
                    || (*claimed != current_generation && *claimed != admitted_generation) =>
                {
                    // Refuse without advancing generation, but project the
                    // current fence so callers are not left holding a stale view.
                    self.project_runtime(service_id);
                    return Err(SupervisorError::PortBusy { port });
                }
                Some(PortClaim::Reserved { .. }) | None => {}
            }
        }

        let generation = current_generation
            .checked_add(1)
            .ok_or(SupervisorError::Refused(SupervisorRefusal::Other))?;
        let epoch = current_epoch
            .checked_add(1)
            .ok_or(SupervisorError::Refused(SupervisorRefusal::Other))?;

        let spec = self.launch_spec(service_id, generation, intent)?;
        let pending = match self.authority.prepare_suspended(&spec) {
            Ok(pending) => pending,
            Err(error) => {
                self.fail_member(service_id, generation);
                return Err(error);
            }
        };
        let runtime = self
            .runtimes
            .get_mut(service_id)
            .ok_or_else(|| SupervisorError::UnknownService(service_id.clone()))?;
        runtime.pending = Some(pending);
        let pending = runtime
            .pending
            .take()
            .expect("pending launch was just stored");
        let registered = match self.authority.register_suspended(pending) {
            Ok(registered) => registered,
            Err(error) => {
                self.fail_member(service_id, generation);
                return Err(error);
            }
        };
        let live = match self.authority.resume(registered) {
            Ok(live) => live,
            Err(error) => {
                self.fail_member(service_id, generation);
                return Err(error);
            }
        };

        if let Some(port) = intent.expected_port().map(|port| port.port) {
            self.ports.insert(
                port,
                PortClaim::Reserved {
                    service_id: service_id.clone(),
                    generation,
                },
            );
            self.push_event(
                Some(service_id.clone()),
                SupervisorEventKind::PortReserved,
                generation,
            );
        }

        let has_health = {
            let runtime = self
                .runtimes
                .get_mut(service_id)
                .ok_or_else(|| SupervisorError::UnknownService(service_id.clone()))?;
            runtime.live = Some(live);
            runtime.pending = None;
            runtime.evidence.lifecycle = LifecycleAxis::Starting;
            runtime.evidence.process = ProcessAxis::Running { generation };
            runtime.evidence.ownership = match &runtime.binding_owner {
                ConfiguredServiceOwner::Task { task_id } => OwnershipAxis::Task {
                    task_id: task_id.to_string(),
                },
                ConfiguredServiceOwner::Project { .. }
                | ConfiguredServiceOwner::Workspace { .. } => OwnershipAxis::Host,
            };
            runtime.evidence.generation = generation;
            runtime.evidence.epoch = epoch;
            runtime.evidence.observed_at_ms = self.now_ms;
            runtime.evidence.port = intent
                .expected_port()
                .map(|port| PortAxis::Owned { port: port.port })
                .unwrap_or(PortAxis::Free);
            runtime.evidence.provenance = EvidenceProvenance {
                source: EvidenceSource::ProcessRegistry,
                observed_at_ms: self.now_ms,
                generation: Some(generation),
                epoch: Some(epoch),
            };
            let has_health = runtime.health.is_some();
            if let Some(tracker) = runtime.health.as_mut() {
                tracker.start(self.now_ms, generation).map_err(|_| {
                    SupervisorError::StaleGeneration {
                        expected: generation,
                        received: generation,
                    }
                })?;
                runtime.evidence.health = tracker.axis();
                let operation_id = self.next_operation_id;
                self.next_operation_id = self.next_operation_id.saturating_add(1);
                runtime.operation = Some(ActiveOperation {
                    id: operation_id,
                    action: ServiceAction::Start,
                });
            } else {
                // Disabled health finishes Start immediately so stop/restart are
                // admissible against a projected Healthy fence.
                runtime.evidence.health = HealthAxis::Disabled;
                runtime.evidence.lifecycle = LifecycleAxis::Running;
                runtime.operation = None;
            }
            has_health
        };
        if has_health {
            self.push_event(
                Some(service_id.clone()),
                SupervisorEventKind::ProbeScheduled,
                generation,
            );
        }
        self.append_log(service_id, generation, "started");
        self.push_event(
            Some(service_id.clone()),
            SupervisorEventKind::Started,
            generation,
        );
        self.project_runtime(service_id);
        Ok(())
    }

    fn stop_member(
        &mut self,
        service_id: &ServiceId,
        fence: &ServiceFence,
        for_restart: bool,
    ) -> Result<(), SupervisorError> {
        let admitted_generation = fence.resource_generation().get();
        let admitted_epoch = fence.action_epoch().get();
        let runtime = self
            .runtimes
            .get_mut(service_id)
            .ok_or_else(|| SupervisorError::UnknownService(service_id.clone()))?;
        if runtime.evidence.generation != admitted_generation
            || runtime.evidence.epoch != admitted_epoch
        {
            return Err(SupervisorError::Refused(SupervisorRefusal::StaleFence));
        }
        let live_generation = admitted_generation;
        let live_epoch = admitted_epoch;
        let prior_lifecycle = runtime.evidence.lifecycle.clone();
        runtime.evidence.lifecycle = LifecycleAxis::Stopping;
        runtime.pending = None;
        let resource_id = runtime.resource_id;
        self.project_runtime(service_id);
        let teardown_fence = ResourceFence::new(resource_id, live_generation);
        if let Err(error) = self.authority.teardown(
            &mut self
                .runtimes
                .get_mut(service_id)
                .ok_or_else(|| SupervisorError::UnknownService(service_id.clone()))?
                .live,
            teardown_fence,
        ) {
            if let Some(runtime) = self.runtimes.get_mut(service_id) {
                runtime.evidence.lifecycle = prior_lifecycle;
            }
            self.project_runtime(service_id);
            return Err(error);
        }
        let generation = live_generation
            .checked_add(1)
            .ok_or(SupervisorError::Refused(SupervisorRefusal::Other))?;
        let epoch = live_epoch
            .checked_add(1)
            .ok_or(SupervisorError::Refused(SupervisorRefusal::Other))?;
        self.release_port_for(service_id, live_generation);
        let runtime = self
            .runtimes
            .get_mut(service_id)
            .ok_or_else(|| SupervisorError::UnknownService(service_id.clone()))?;
        if let Some(tracker) = runtime.health.as_mut() {
            let _ = tracker.cancel(self.now_ms, live_generation);
        }
        runtime.evidence.generation = generation;
        runtime.evidence.epoch = epoch;
        runtime.evidence.lifecycle = LifecycleAxis::Stopped;
        runtime.evidence.process = ProcessAxis::Exited { exit_code: Some(0) };
        runtime.evidence.port = PortAxis::Free;
        runtime.evidence.ownership = OwnershipAxis::None;
        runtime.evidence.health = HealthAxis::Cancelled;
        runtime.evidence.observed_at_ms = self.now_ms;
        runtime.evidence.provenance = EvidenceProvenance {
            source: EvidenceSource::ProcessRegistry,
            observed_at_ms: self.now_ms,
            generation: Some(generation),
            epoch: Some(epoch),
        };
        runtime.operation = None;
        if !for_restart {
            self.append_log(service_id, generation, "stopped");
            self.push_event(
                Some(service_id.clone()),
                SupervisorEventKind::Stopped,
                generation,
            );
        }
        self.project_runtime(service_id);
        Ok(())
    }

    fn fail_member(&mut self, service_id: &ServiceId, generation: u64) {
        let resource_id = self
            .runtimes
            .get(service_id)
            .map(|runtime| runtime.resource_id);
        if let Some(resource_id) = resource_id {
            // Prefer the live generation when a handle exists; otherwise the
            // failed launch generation is only projected into evidence.
            let live_generation = generation;
            let fence = ResourceFence::new(resource_id, live_generation);
            let _ = self.authority.teardown(
                &mut self
                    .runtimes
                    .get_mut(service_id)
                    .expect("service runtime present")
                    .live,
                fence,
            );
        }
        if let Some(runtime) = self.runtimes.get_mut(service_id) {
            runtime.pending = None;
            runtime.evidence.generation = generation;
            runtime.evidence.lifecycle = LifecycleAxis::Failed;
            runtime.evidence.process = ProcessAxis::Crashed { generation };
            runtime.evidence.health = HealthAxis::Crashed;
            runtime.evidence.observed_at_ms = self.now_ms;
            runtime.operation = None;
        }
        self.release_port_for(service_id, generation);
        self.push_event(
            Some(service_id.clone()),
            SupervisorEventKind::Failed,
            generation,
        );
        self.project_runtime(service_id);
    }

    fn mark_external(&mut self, service_id: &ServiceId, port: u16, owner_pid: Option<u32>) {
        if let Some(runtime) = self.runtimes.get_mut(service_id) {
            runtime.evidence.lifecycle = LifecycleAxis::Running;
            runtime.evidence.process = ProcessAxis::Unknown;
            runtime.evidence.health = HealthAxis::Unknown;
            runtime.evidence.port = PortAxis::External { port, owner_pid };
            runtime.evidence.ownership = OwnershipAxis::External;
            runtime.evidence.observed_at_ms = self.now_ms;
            runtime.evidence.provenance = EvidenceProvenance {
                source: EvidenceSource::PortSnapshot,
                observed_at_ms: self.now_ms,
                generation: Some(runtime.evidence.generation),
                epoch: Some(runtime.evidence.epoch),
            };
        }
        self.push_event(
            Some(service_id.clone()),
            SupervisorEventKind::PortExternal,
            self.runtimes
                .get(service_id)
                .map(|runtime| runtime.evidence.generation)
                .unwrap_or(0),
        );
        self.project_runtime(service_id);
    }

    fn mark_port_indeterminate(&mut self, service_id: &ServiceId, port: u16) {
        let now_ms = self.now_ms;
        if let Some(runtime) = self.runtimes.get_mut(service_id) {
            runtime.evidence.port = PortAxis::Unknown;
            runtime.evidence.ownership = OwnershipAxis::Unknown;
            runtime.evidence.observed_at_ms = now_ms;
            runtime.evidence.provenance = EvidenceProvenance {
                source: EvidenceSource::PortSnapshot,
                observed_at_ms: now_ms,
                generation: Some(runtime.evidence.generation),
                epoch: Some(runtime.evidence.epoch),
            };
            let _ = port;
        }
        self.project_runtime(service_id);
    }

    fn clear_external_for_port(&mut self, port: u16) {
        let ids: Vec<ServiceId> = self
            .catalog
            .definitions()
            .filter(|definition| {
                definition
                    .expected_port
                    .is_some_and(|expected| expected.port == port)
            })
            .map(|definition| definition.id.clone())
            .collect();
        for id in ids {
            let Some(runtime) = self.runtimes.get_mut(&id) else {
                continue;
            };
            if runtime.live.is_some() {
                continue;
            }
            let unclear = matches!(
                runtime.evidence.ownership,
                OwnershipAxis::External | OwnershipAxis::Unknown
            ) || matches!(runtime.evidence.port, PortAxis::Unknown);
            if !unclear {
                continue;
            }
            let generation = runtime.evidence.generation;
            let epoch = runtime.evidence.epoch;
            runtime.evidence = stopped_evidence(self.now_ms, generation, epoch);
            self.project_runtime(&id);
        }
    }

    fn launch_spec(
        &self,
        service_id: &ServiceId,
        generation: u64,
        intent: &LaunchIntent,
    ) -> Result<ManagedLaunchSpec, SupervisorError> {
        let runtime = self
            .runtimes
            .get(service_id)
            .ok_or_else(|| SupervisorError::UnknownService(service_id.clone()))?;
        let mut environment = runtime.overlay.clone().into_launch_env();
        for reference in intent.command().env() {
            environment
                .entry(reference.name().to_owned())
                .or_insert_with(String::new);
        }
        let cwd = resolve_launch_cwd(
            &runtime.workspace_root,
            intent.command().cwd().map(|path| path.as_str()),
        )?;
        Ok(ManagedLaunchSpec {
            resource_id: runtime.resource_id,
            generation,
            owner: match intent.scope() {
                ServiceScope::Task { task_id } => ProcessOwner::Task(*task_id),
                ServiceScope::Host => ProcessOwner::Host,
            },
            kind: ResourceKind::Service,
            program: intent.command().program().as_str().to_owned(),
            args: intent
                .command()
                .args()
                .iter()
                .map(|argument| argument.as_str().to_owned())
                .collect(),
            cwd,
            environment,
            display_label: service_id.as_str().to_owned(),
        })
    }

    /// Drain PTY reader output into bounded logs and apply waiter exits through
    /// `report_exit` so crashes leave Failed (not Healthy) with port release.
    pub fn pump_io(&mut self) {
        let ids: Vec<ServiceId> = self.runtimes.keys().cloned().collect();
        let mut observed = Vec::new();
        for service_id in ids {
            let Some(runtime) = self.runtimes.get(&service_id) else {
                continue;
            };
            let Some(live) = runtime.live.as_ref() else {
                continue;
            };
            let generation = A::live_generation(live);
            let lines = self.authority.drain_output_lines(live);
            let exit = self.authority.take_exit(live);
            if !lines.is_empty() || exit.is_some() {
                observed.push((service_id, generation, lines, exit));
            }
        }
        for (service_id, generation, lines, exit) in observed {
            for line in lines {
                self.append_log(&service_id, generation, line);
            }
            if let Some(exit_code) = exit {
                let _ = self.report_exit(&service_id, generation, exit_code);
            }
        }
    }

    fn project_runtime(&mut self, service_id: &ServiceId) {
        let Some(runtime) = self.runtimes.get(service_id) else {
            return;
        };
        let state = reduce_service(&runtime.evidence);
        let ownership = match &runtime.evidence.ownership {
            OwnershipAxis::Task { task_id } => TaskId::parse(task_id)
                .ok()
                .map(|task_id| RuntimeOwnership::Task { task_id })
                .unwrap_or(RuntimeOwnership::None),
            OwnershipAxis::Host => RuntimeOwnership::Host {
                host_id: self.host_id,
            },
            OwnershipAxis::External => RuntimeOwnership::External,
            OwnershipAxis::None | OwnershipAxis::Unknown | OwnershipAxis::Inconsistent => {
                RuntimeOwnership::None
            }
        };
        let fence = AdmissionFence::new(
            runtime.evidence.generation,
            runtime.evidence.epoch,
            runtime.evidence.epoch,
        );
        let operation = runtime.operation.clone();
        self.snapshot.set_service(
            service_id.clone(),
            RuntimeRecord {
                state,
                fence,
                ownership,
                operation,
            },
        );
    }

    fn require_fence(
        &self,
        service_id: &ServiceId,
        fence: AdmissionFence,
    ) -> Result<(), SupervisorError> {
        let current = self.fence(service_id)?;
        if current != fence {
            return Err(SupervisorError::Refused(SupervisorRefusal::StaleFence));
        }
        Ok(())
    }

    fn release_port_for(&mut self, service_id: &ServiceId, generation: u64) {
        let port = self
            .catalog
            .definition(service_id)
            .and_then(|definition| definition.expected_port)
            .map(|port| port.port);
        if let Some(port) = port {
            if matches!(
                self.ports.get(&port),
                Some(PortClaim::Reserved {
                    service_id: owner,
                    generation: claimed
                }) if owner == service_id && *claimed == generation
            ) {
                self.ports.remove(&port);
            }
        }
    }

    fn append_log(&mut self, service_id: &ServiceId, generation: u64, text: impl Into<String>) {
        let Some(runtime) = self.runtimes.get_mut(service_id) else {
            return;
        };
        let text = redact_log_text(&text.into(), &runtime.overlay);
        if runtime.logs.len() >= MAX_SERVICE_LOG_LINES {
            runtime.logs.pop_front();
            runtime.log_truncated = true;
        }
        runtime.logs.push_back(RedactedLogLine {
            observed_at_ms: self.now_ms,
            generation,
            text,
        });
    }

    fn push_event(
        &mut self,
        service_id: Option<ServiceId>,
        kind: SupervisorEventKind,
        generation: u64,
    ) {
        if self.events.len() >= MAX_SUPERVISOR_EVENTS {
            self.events.pop_front();
        }
        self.events.push_back(RedactedSupervisorEvent {
            service_id,
            kind,
            generation,
            observed_at_ms: self.now_ms,
        });
    }
}

impl<A: ManagedLaunchAuthority> Drop for ServiceSupervisor<A> {
    fn drop(&mut self) {
        let ids: Vec<ServiceId> = self.runtimes.keys().cloned().collect();
        for id in ids {
            let Some(runtime) = self.runtimes.get(&id) else {
                continue;
            };
            let fence = ResourceFence::new(runtime.resource_id, runtime.evidence.generation);
            if let Some(runtime) = self.runtimes.get_mut(&id) {
                runtime.pending = None;
                let _ = self.authority.teardown(&mut runtime.live, fence);
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SupervisorOutcome {
    Changed {
        services: Vec<ServiceId>,
        state: ServiceState,
    },
    Coalesced {
        service: ServiceId,
        operation_id: u64,
        action: ServiceAction,
    },
    Logs(BoundedServiceLog),
    Health(RedactedServiceSnapshot),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PortClaimView {
    Reserved {
        service_id: ServiceId,
        generation: u64,
    },
    External {
        owner_pid: Option<u32>,
    },
    Indeterminate,
}

pub fn session_status_for_ui(state: ServiceState) -> SessionStatus {
    match state {
        ServiceState::Stopped | ServiceState::Unknown => SessionStatus::Stopped,
        ServiceState::Starting => SessionStatus::Starting,
        ServiceState::Healthy | ServiceState::Unhealthy => SessionStatus::Running,
        ServiceState::External => SessionStatus::Stopped,
        ServiceState::Stopping => SessionStatus::Stopping,
        ServiceState::Failed => SessionStatus::Failed,
    }
}

fn refusal_from(rejection: AdmissionRejection) -> SupervisorRefusal {
    match rejection {
        AdmissionRejection::StaleFence { .. } | AdmissionRejection::PlanStale { .. } => {
            SupervisorRefusal::StaleFence
        }
        AdmissionRejection::OwnershipMismatch { .. }
        | AdmissionRejection::RequesterMismatch { .. } => SupervisorRefusal::Ownership,
        AdmissionRejection::ExternalNotControllable { .. }
        | AdmissionRejection::DependencyExternal { .. } => SupervisorRefusal::External,
        AdmissionRejection::DependencyNotReady { .. } => SupervisorRefusal::Dependency,
        AdmissionRejection::OperationInProgress { .. } => SupervisorRefusal::OperationInProgress,
        AdmissionRejection::AlreadyRunning { .. } => SupervisorRefusal::AlreadyRunning,
        AdmissionRejection::AlreadyStopped { .. } => SupervisorRefusal::AlreadyStopped,
        AdmissionRejection::EvidenceUnknown { .. } | AdmissionRejection::UnknownService { .. } => {
            SupervisorRefusal::EvidenceUnknown
        }
        AdmissionRejection::TaskClosing { .. }
        | AdmissionRejection::TaskCloseNotAdmitted { .. }
        | AdmissionRejection::TaskEpochStale { .. } => SupervisorRefusal::TaskClosing,
        _ => SupervisorRefusal::Other,
    }
}

fn owner_from_scope(scope: &ServiceScope) -> ConfiguredServiceOwner {
    match scope {
        ServiceScope::Task { task_id } => ConfiguredServiceOwner::Task { task_id: *task_id },
        ServiceScope::Host => ConfiguredServiceOwner::Project {
            project_id: "host".to_owned(),
        },
    }
}

fn stopped_evidence(now_ms: u64, generation: u64, epoch: u64) -> ServiceEvidence {
    ServiceEvidence {
        lifecycle: LifecycleAxis::Stopped,
        process: ProcessAxis::Absent,
        health: HealthAxis::Disabled,
        port: PortAxis::Free,
        ownership: OwnershipAxis::None,
        generation,
        epoch,
        observed_at_ms: now_ms,
        provenance: EvidenceProvenance {
            source: EvidenceSource::Admission,
            observed_at_ms: now_ms,
            generation: Some(generation),
            epoch: Some(epoch),
        },
    }
}

fn resolve_launch_cwd(
    workspace_root: &str,
    relative: Option<&str>,
) -> Result<String, SupervisorError> {
    let root = workspace_root.trim();
    if root.is_empty() || !is_absolute_workspace_root(root) {
        return Err(SupervisorError::Binding(ValidationError::UnsafePath {
            field: crate::services::model::ValidationField::Cwd,
        }));
    }
    Ok(match relative {
        Some(relative) => std::path::Path::new(root)
            .join(relative)
            .to_string_lossy()
            .into_owned(),
        None => root.to_owned(),
    })
}

fn is_absolute_workspace_root(path: &str) -> bool {
    if std::path::Path::new(path).is_absolute() || path.starts_with('/') || path.starts_with('\\') {
        return true;
    }
    let bytes = path.as_bytes();
    bytes.len() >= 3 && bytes[1] == b':' && (bytes[2] == b'/' || bytes[2] == b'\\')
}

/// Resolve a configured program through the host PATH authority and pin the
/// canonical executable before LaunchIntent validation. Bare names such as
/// `node`/`npm` are accepted; process cwd is never used as a search root.
pub fn resolve_configured_service_program(program: &str) -> Result<String, SupervisorError> {
    resolve_configured_service_program_with(
        program,
        std::env::var_os("PATH").as_deref(),
        std::env::var("PATHEXT").ok().as_deref(),
    )
}

pub fn resolve_configured_service_program_with(
    program: &str,
    path_var: Option<&std::ffi::OsStr>,
    path_ext: Option<&str>,
) -> Result<String, SupervisorError> {
    use std::path::{Path, PathBuf};

    let supplied = PathBuf::from(program);
    let resolved =
        if Path::new(program).is_absolute() || program.contains('/') || program.contains('\\') {
            std::fs::canonicalize(&supplied).map_err(|_| SupervisorError::Launch {
                stage: ManagedLaunchStage::Prepare,
            })?
        } else {
            let path = path_var.ok_or(SupervisorError::Launch {
                stage: ManagedLaunchStage::Prepare,
            })?;
            let extensions = path_ext.unwrap_or(".COM;.EXE;.BAT;.CMD");
            let names = executable_candidate_names(program, cfg!(windows), extensions);
            let mut found = None;
            for directory in std::env::split_paths(path) {
                for name in &names {
                    let candidate = directory.join(name);
                    if candidate.is_file() {
                        found = Some(candidate.canonicalize().map_err(|_| {
                            SupervisorError::Launch {
                                stage: ManagedLaunchStage::Prepare,
                            }
                        })?);
                        break;
                    }
                }
                if found.is_some() {
                    break;
                }
            }
            found.ok_or(SupervisorError::Launch {
                stage: ManagedLaunchStage::Prepare,
            })?
        };
    Ok(resolved.to_string_lossy().into_owned())
}

fn executable_candidate_names(program: &str, windows: bool, path_ext: &str) -> Vec<String> {
    let mut names = Vec::new();
    if windows && std::path::Path::new(program).extension().is_none() {
        names.extend(
            path_ext
                .split(';')
                .filter(|extension| !extension.is_empty())
                .map(|extension| format!("{program}{}", extension.to_ascii_lowercase())),
        );
        names.extend(
            path_ext
                .split(';')
                .filter(|extension| !extension.is_empty())
                .map(|extension| format!("{program}{}", extension.to_ascii_uppercase())),
        );
    }
    names.push(program.to_string());
    names
}

fn redact_log_text(text: &str, overlay: &EnvironmentOverlay) -> String {
    let mut redacted = text.to_owned();
    for name in overlay.names() {
        if let Some(value) = overlay.get(name) {
            if !value.is_empty() {
                redacted = redacted.replace(value, "[redacted]");
            }
        }
    }
    if redacted.len() > MAX_SERVICE_LOG_LINE_BYTES {
        redacted.truncate(MAX_SERVICE_LOG_LINE_BYTES);
    }
    redacted
}

#[cfg(windows)]
pub fn prepare_managed_service_pty(
    slave: &dyn portable_pty::SlavePty,
    spec: ManagedLaunchSpec,
) -> Result<crate::process::launcher::PendingManagedLaunch, SupervisorError> {
    use std::{ffi::OsString, path::PathBuf};

    if spec.generation == 0 {
        return Err(SupervisorError::Launch {
            stage: ManagedLaunchStage::Prepare,
        });
    }
    let mut environment = BTreeMap::new();
    for (key, value) in spec.environment {
        environment.insert(OsString::from(key), OsString::from(value));
    }
    let intent = crate::process::launcher::LaunchIntent {
        resource_id: spec.resource_id,
        generation: spec.generation,
        owner: spec.owner,
        kind: spec.kind,
        executable: PathBuf::from(spec.program),
        args: spec.args.into_iter().map(OsString::from).collect(),
        cwd: PathBuf::from(spec.cwd),
        environment,
        display_label: spec.display_label,
    };
    crate::process::launcher::prepare_suspended_pty(slave, intent).map_err(|_| {
        SupervisorError::Launch {
            stage: ManagedLaunchStage::Prepare,
        }
    })
}

/// Deterministic managed-launch double used by supervisor contract tests.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FakeFailStage {
    Prepare,
    Register,
    Resume,
}

struct FakeLaunchInner {
    prepared: usize,
    registered: usize,
    resumed: usize,
    aborted: usize,
    torn_down: usize,
    next_token: u64,
    live: BTreeSet<u64>,
    fail_at: Option<FakeFailStage>,
    last_spec: Option<ManagedLaunchSpec>,
    outputs: BTreeMap<u64, VecDeque<String>>,
    exits: BTreeMap<u64, Option<i32>>,
}

pub struct FakePending {
    inner: std::rc::Rc<std::cell::RefCell<FakeLaunchInner>>,
    token: u64,
    generation: u64,
    live: bool,
}

impl Drop for FakePending {
    fn drop(&mut self) {
        if !self.live {
            let mut inner = self.inner.borrow_mut();
            inner.aborted = inner.aborted.saturating_add(1);
            inner.live.remove(&self.token);
        }
    }
}

#[derive(Clone)]
pub struct FakeLiveLaunch {
    inner: std::rc::Rc<std::cell::RefCell<FakeLaunchInner>>,
    token: u64,
    generation: u64,
}

#[derive(Clone)]
pub struct FakeLaunchAuthority {
    inner: std::rc::Rc<std::cell::RefCell<FakeLaunchInner>>,
}

impl FakeLaunchAuthority {
    pub fn new() -> Self {
        Self {
            inner: std::rc::Rc::new(std::cell::RefCell::new(FakeLaunchInner {
                prepared: 0,
                registered: 0,
                resumed: 0,
                aborted: 0,
                torn_down: 0,
                next_token: 1,
                live: BTreeSet::new(),
                fail_at: None,
                last_spec: None,
                outputs: BTreeMap::new(),
                exits: BTreeMap::new(),
            })),
        }
    }

    pub fn fail_at(&self, stage: FakeFailStage) {
        self.inner.borrow_mut().fail_at = Some(stage);
    }

    pub fn prepared(&self) -> usize {
        self.inner.borrow().prepared
    }

    pub fn registered(&self) -> usize {
        self.inner.borrow().registered
    }

    pub fn resumed(&self) -> usize {
        self.inner.borrow().resumed
    }

    pub fn aborted(&self) -> usize {
        self.inner.borrow().aborted
    }

    pub fn torn_down(&self) -> usize {
        self.inner.borrow().torn_down
    }

    pub fn last_env_names(&self) -> Vec<String> {
        self.inner
            .borrow()
            .last_spec
            .as_ref()
            .map(|spec| spec.environment.keys().cloned().collect())
            .unwrap_or_default()
    }

    pub fn last_cwd(&self) -> Option<String> {
        self.inner
            .borrow()
            .last_spec
            .as_ref()
            .map(|spec| spec.cwd.clone())
    }

    pub fn last_program(&self) -> Option<String> {
        self.inner
            .borrow()
            .last_spec
            .as_ref()
            .map(|spec| spec.program.clone())
    }

    pub fn last_spec_debug(&self) -> String {
        self.inner
            .borrow()
            .last_spec
            .as_ref()
            .map(|spec| format!("{spec:?}"))
            .unwrap_or_default()
    }

    pub fn push_output(&self, token: u64, line: impl Into<String>) {
        self.inner
            .borrow_mut()
            .outputs
            .entry(token)
            .or_default()
            .push_back(line.into());
    }

    pub fn push_exit(&self, token: u64, code: Option<i32>) {
        self.inner.borrow_mut().exits.insert(token, code);
    }

    pub fn live_token(&self) -> Option<u64> {
        self.inner.borrow().live.iter().next().copied()
    }
}

impl ManagedLaunchAuthority for FakeLaunchAuthority {
    type Pending = FakePending;
    type Live = FakeLiveLaunch;

    fn prepare_suspended(
        &mut self,
        spec: &ManagedLaunchSpec,
    ) -> Result<Self::Pending, SupervisorError> {
        let mut inner = self.inner.borrow_mut();
        inner.last_spec = Some(spec.clone());
        if inner.fail_at == Some(FakeFailStage::Prepare) {
            return Err(SupervisorError::Launch {
                stage: ManagedLaunchStage::Prepare,
            });
        }
        inner.prepared = inner.prepared.saturating_add(1);
        let token = inner.next_token;
        inner.next_token = inner.next_token.saturating_add(1);
        let generation = spec.generation;
        drop(inner);
        Ok(FakePending {
            inner: self.inner.clone(),
            token,
            generation,
            live: false,
        })
    }

    fn register_suspended(
        &mut self,
        pending: Self::Pending,
    ) -> Result<Self::Pending, SupervisorError> {
        let mut inner = self.inner.borrow_mut();
        if inner.fail_at == Some(FakeFailStage::Register) {
            return Err(SupervisorError::Launch {
                stage: ManagedLaunchStage::Register,
            });
        }
        inner.registered = inner.registered.saturating_add(1);
        drop(inner);
        Ok(pending)
    }

    fn resume(&mut self, mut pending: Self::Pending) -> Result<Self::Live, SupervisorError> {
        let mut inner = self.inner.borrow_mut();
        if inner.fail_at == Some(FakeFailStage::Resume) {
            drop(inner);
            return Err(SupervisorError::Launch {
                stage: ManagedLaunchStage::Resume,
            });
        }
        let token = pending.token;
        let generation = pending.generation;
        pending.live = true;
        inner.resumed = inner.resumed.saturating_add(1);
        inner.live.insert(token);
        drop(inner);
        drop(pending);
        Ok(FakeLiveLaunch {
            inner: self.inner.clone(),
            token,
            generation,
        })
    }

    fn teardown(
        &mut self,
        live: &mut Option<Self::Live>,
        _fence: ResourceFence,
    ) -> Result<(), SupervisorError> {
        let Some(handle) = live.take() else {
            return Ok(());
        };
        let mut inner = self.inner.borrow_mut();
        inner.live.remove(&handle.token);
        inner.outputs.remove(&handle.token);
        inner.exits.remove(&handle.token);
        inner.torn_down = inner.torn_down.saturating_add(1);
        Ok(())
    }

    fn drain_output_lines(&self, live: &Self::Live) -> Vec<String> {
        self.inner
            .borrow_mut()
            .outputs
            .get_mut(&live.token)
            .map(|queue| queue.drain(..).collect())
            .unwrap_or_default()
    }

    fn take_exit(&self, live: &Self::Live) -> Option<Option<i32>> {
        self.inner.borrow_mut().exits.remove(&live.token)
    }

    fn live_generation(live: &Self::Live) -> u64 {
        live.generation
    }

    fn live_count(&self) -> usize {
        self.inner.borrow().live.len()
    }

    fn residue_count(&self) -> usize {
        self.inner.borrow().live.len()
    }
}

impl AdmissionSnapshot {
    pub(crate) fn service(&self, id: &ServiceId) -> Option<&RuntimeRecord> {
        self.services.get(id)
    }
}
