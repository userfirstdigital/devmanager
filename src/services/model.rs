use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Serialize};

use super::health::ServiceState;

pub const MAX_SERVICE_COUNT: usize = 64;
pub const MAX_SERVICE_ID_LENGTH: usize = 64;
pub const MAX_TASK_ID_LENGTH: usize = 64;
pub const MAX_PROGRAM_LENGTH: usize = 256;
pub const MAX_ARGUMENT_COUNT: usize = 32;
pub const MAX_ARGUMENT_LENGTH: usize = 256;
pub const MAX_CWD_LENGTH: usize = 260;
pub const MAX_ENV_REFERENCE_COUNT: usize = 32;
pub const MAX_ENV_REFERENCE_LENGTH: usize = 64;
pub const MAX_DEPENDENCY_COUNT: usize = 32;
pub const MAX_HEALTH_PATH_LENGTH: usize = 128;
pub const MAX_STARTUP_DEADLINE_MS: u64 = 3_600_000;
pub const MAX_PROBE_INTERVAL_MS: u64 = 60_000;
pub const MAX_STOP_TIMEOUT_MS: u64 = 120_000;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct ServiceId(String);

impl ServiceId {
    pub fn new(value: impl Into<String>) -> Result<Self, ValidationError> {
        let value = value.into();
        validate_identifier(&value, ValidationField::ServiceId, MAX_SERVICE_ID_LENGTH)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ServiceId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ServiceScope {
    Task { task_id: String },
    Host,
}

impl ServiceScope {
    pub fn task(task_id: impl Into<String>) -> Self {
        Self::Task {
            task_id: task_id.into(),
        }
    }

    pub fn task_id(&self) -> Option<&str> {
        match self {
            Self::Task { task_id } => Some(task_id),
            Self::Host => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct EnvReference {
    pub name: String,
}

impl EnvReference {
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into() }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    pub env: Vec<EnvReference>,
}

impl CommandSpec {
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            cwd: None,
            env: Vec::new(),
        }
    }

    pub fn with_arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn with_args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    pub fn with_cwd(mut self, cwd: impl Into<String>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    pub fn with_env_reference(mut self, name: impl Into<String>) -> Self {
        self.env.push(EnvReference::new(name));
        self
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum PortProtocol {
    Tcp,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExpectedPort {
    pub protocol: PortProtocol,
    pub port: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HealthPolicy {
    pub startup_deadline_ms: u64,
    pub probe_interval_ms: u64,
    pub max_probe_interval_ms: u64,
    pub backoff_multiplier: u8,
    pub success_threshold: u8,
    pub failure_threshold: u8,
    pub stale_after_ms: u64,
}

impl Default for HealthPolicy {
    fn default() -> Self {
        Self {
            startup_deadline_ms: 30_000,
            probe_interval_ms: 1_000,
            max_probe_interval_ms: 10_000,
            backoff_multiplier: 2,
            success_threshold: 2,
            failure_threshold: 3,
            stale_after_ms: 5_000,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum HealthSpec {
    None,
    Tcp {
        port: u16,
        policy: HealthPolicy,
    },
    Http {
        port: u16,
        path: String,
        policy: HealthPolicy,
    },
}

impl HealthSpec {
    pub fn tcp(port: u16, policy: HealthPolicy) -> Self {
        Self::Tcp { port, policy }
    }

    pub fn policy(&self) -> Option<&HealthPolicy> {
        match self {
            Self::None => None,
            Self::Tcp { policy, .. } | Self::Http { policy, .. } => Some(policy),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum StartupTrigger {
    Manual,
    TaskOpen,
    HostStart,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct StartupPolicy {
    pub trigger: StartupTrigger,
    pub restart_limit: u8,
}

impl StartupPolicy {
    pub fn manual() -> Self {
        Self {
            trigger: StartupTrigger::Manual,
            restart_limit: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct StopPolicy {
    pub graceful_timeout_ms: u64,
    pub kill_after_ms: u64,
}

impl Default for StopPolicy {
    fn default() -> Self {
        Self {
            graceful_timeout_ms: 1_000,
            kill_after_ms: 10_000,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ServiceDefinition {
    pub id: ServiceId,
    pub scope: ServiceScope,
    pub command: CommandSpec,
    pub dependencies: Vec<ServiceId>,
    pub health: HealthSpec,
    pub startup: StartupPolicy,
    pub stop: StopPolicy,
    pub expected_port: Option<ExpectedPort>,
}

impl ServiceDefinition {
    pub fn validate(&self) -> Result<(), ValidationError> {
        validate_identifier(
            self.id.as_str(),
            ValidationField::ServiceId,
            MAX_SERVICE_ID_LENGTH,
        )?;
        if let ServiceScope::Task { task_id } = &self.scope {
            validate_identifier(task_id, ValidationField::TaskId, MAX_TASK_ID_LENGTH)?;
        }
        validate_command(&self.command)?;
        if self.dependencies.len() > MAX_DEPENDENCY_COUNT {
            return Err(ValidationError::TooMany {
                field: ValidationField::Dependency,
                limit: MAX_DEPENDENCY_COUNT,
            });
        }
        let mut dependencies = BTreeSet::new();
        for dependency in &self.dependencies {
            validate_identifier(
                dependency.as_str(),
                ValidationField::Dependency,
                MAX_SERVICE_ID_LENGTH,
            )?;
            if dependency == &self.id {
                return Err(ValidationError::SelfDependency {
                    service: self.id.clone(),
                });
            }
            if !dependencies.insert(dependency.clone()) {
                return Err(ValidationError::DuplicateDependency {
                    service: self.id.clone(),
                    dependency: dependency.clone(),
                });
            }
        }
        validate_health(&self.health)?;
        if self.startup.restart_limit > 10 {
            return Err(ValidationError::InvalidPolicy {
                field: ValidationField::StartupPolicy,
            });
        }
        if self.stop.graceful_timeout_ms == 0
            || self.stop.graceful_timeout_ms > MAX_STOP_TIMEOUT_MS
            || self.stop.kill_after_ms < self.stop.graceful_timeout_ms
            || self.stop.kill_after_ms > MAX_STOP_TIMEOUT_MS
        {
            return Err(ValidationError::InvalidPolicy {
                field: ValidationField::StopPolicy,
            });
        }
        if let Some(expected_port) = self.expected_port {
            validate_port(expected_port.port, ValidationField::ExpectedPort)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LaunchIntent {
    pub service_id: ServiceId,
    pub scope: ServiceScope,
    pub command: CommandSpec,
    pub dependencies: Vec<ServiceId>,
    pub expected_port: Option<ExpectedPort>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServiceCatalog {
    services: BTreeMap<ServiceId, ServiceDefinition>,
}

impl ServiceCatalog {
    pub fn new(definitions: Vec<ServiceDefinition>) -> Result<Self, ValidationError> {
        if definitions.len() > MAX_SERVICE_COUNT {
            return Err(ValidationError::TooMany {
                field: ValidationField::Service,
                limit: MAX_SERVICE_COUNT,
            });
        }
        let mut services = BTreeMap::new();
        for definition in definitions {
            definition.validate()?;
            if services
                .insert(definition.id.clone(), definition.clone())
                .is_some()
            {
                return Err(ValidationError::DuplicateServiceId { id: definition.id });
            }
        }
        for definition in services.values() {
            for dependency in &definition.dependencies {
                if !services.contains_key(dependency) {
                    return Err(ValidationError::UnknownDependency {
                        service: definition.id.clone(),
                        dependency: dependency.clone(),
                    });
                }
            }
        }
        validate_graph(&services)?;
        Ok(Self { services })
    }

    pub fn definition(&self, id: &ServiceId) -> Option<&ServiceDefinition> {
        self.services.get(id)
    }

    pub fn launch_intent(&self, id: &ServiceId) -> Result<LaunchIntent, PlanningError> {
        let definition = self
            .services
            .get(id)
            .ok_or_else(|| PlanningError::UnknownService(id.clone()))?;
        Ok(LaunchIntent {
            service_id: definition.id.clone(),
            scope: definition.scope.clone(),
            command: definition.command.clone(),
            dependencies: definition.dependencies.clone(),
            expected_port: definition.expected_port,
        })
    }

    pub fn dependency_plan(&self, target: &ServiceId) -> Result<DependencyPlan, PlanningError> {
        if !self.services.contains_key(target) {
            return Err(PlanningError::UnknownService(target.clone()));
        }
        let mut ordered = Vec::new();
        let mut visited = BTreeSet::new();
        visit_graph(target, &self.services, &mut visited, &mut ordered)
            .map_err(PlanningError::Cycle)?;
        Ok(DependencyPlan { ordered })
    }

    pub fn admit(
        &self,
        request: AdmissionRequest,
        snapshot: &AdmissionSnapshot,
    ) -> AdmissionDecision {
        let Some(definition) = self.services.get(&request.service_id) else {
            return AdmissionDecision::Refused(AdmissionRejection::UnknownService {
                service: request.service_id,
            });
        };
        let Some(runtime) = snapshot.services.get(&request.service_id) else {
            return AdmissionDecision::Refused(AdmissionRejection::EvidenceUnknown {
                service: request.service_id,
            });
        };
        if runtime.fence != request.fence {
            return AdmissionDecision::Refused(AdmissionRejection::StaleFence {
                service: request.service_id,
                expected: runtime.fence,
                received: request.fence,
            });
        }
        if definition
            .scope
            .task_id()
            .is_some_and(|task_id| snapshot.closing_tasks.contains(task_id))
        {
            return AdmissionDecision::Refused(AdmissionRejection::TaskClosing {
                service: request.service_id,
            });
        }

        match request.action {
            ServiceAction::Start => self.admit_start(request.service_id, snapshot, false),
            ServiceAction::Stop => self.admit_stop(request.service_id, snapshot),
            ServiceAction::Restart => self.admit_restart(request.service_id, snapshot),
        }
    }

    pub fn admit_task_close(
        &self,
        task_id: &str,
        epoch: u64,
        snapshot: &AdmissionSnapshot,
    ) -> Result<TaskClosePlan, AdmissionRejection> {
        let current_epoch = snapshot.task_epochs.get(task_id).copied().ok_or_else(|| {
            AdmissionRejection::TaskEpochStale {
                task_id: task_id.to_owned(),
                expected: None,
                received: epoch,
            }
        })?;
        if current_epoch != epoch {
            return Err(AdmissionRejection::TaskEpochStale {
                task_id: task_id.to_owned(),
                expected: Some(current_epoch),
                received: epoch,
            });
        }

        let mut selected = BTreeSet::new();
        for definition in self.services.values() {
            if definition.scope.task_id() != Some(task_id) {
                continue;
            }
            let Some(runtime) = snapshot.services.get(&definition.id) else {
                continue;
            };
            if matches!(
                &runtime.ownership,
                RuntimeOwnership::Task { task_id: owner } if owner == task_id
            ) {
                if matches!(runtime.state, ServiceState::External) {
                    continue;
                }
                if !matches!(runtime.state, ServiceState::Stopped) {
                    selected.insert(definition.id.clone());
                }
            }
        }
        let mut ordered = self.reverse_selected_order(&selected);
        ordered.retain(|id| selected.contains(id));
        Ok(TaskClosePlan {
            task_id: task_id.to_owned(),
            epoch,
            ordered,
        })
    }

    fn admit_start(
        &self,
        root: ServiceId,
        snapshot: &AdmissionSnapshot,
        force_root: bool,
    ) -> AdmissionDecision {
        let runtime = snapshot.services.get(&root).expect("root checked by admit");
        if runtime.ownership == RuntimeOwnership::External
            || matches!(runtime.state, ServiceState::External)
        {
            return AdmissionDecision::Refused(AdmissionRejection::ExternalNotControllable {
                service: root,
            });
        }
        if let Some(operation) = &runtime.operation {
            if operation.action == ServiceAction::Start
                && matches!(runtime.state, ServiceState::Starting)
            {
                return AdmissionDecision::Coalesced {
                    service: root,
                    operation_id: operation.id,
                    action: ServiceAction::Start,
                };
            }
            return AdmissionDecision::Refused(AdmissionRejection::OperationInProgress {
                service: root,
                action: operation.action,
            });
        }
        if !force_root
            && matches!(
                runtime.state,
                ServiceState::Healthy | ServiceState::Unhealthy
            )
        {
            return AdmissionDecision::Refused(AdmissionRejection::AlreadyRunning {
                service: root,
            });
        }
        if matches!(
            runtime.state,
            ServiceState::Unknown | ServiceState::Stopping
        ) {
            return AdmissionDecision::Refused(AdmissionRejection::EvidenceUnknown {
                service: root,
            });
        }

        let dependency_plan = match self.dependency_plan(&root) {
            Ok(plan) => plan,
            Err(_) => {
                return AdmissionDecision::Refused(AdmissionRejection::EvidenceUnknown {
                    service: root,
                })
            }
        };
        let mut ordered = Vec::new();
        for service_id in dependency_plan.ordered {
            let Some(dependency_runtime) = snapshot.services.get(&service_id) else {
                return AdmissionDecision::Refused(AdmissionRejection::EvidenceUnknown {
                    service: service_id,
                });
            };
            let is_root = service_id == root;
            match dependency_runtime.state {
                ServiceState::Healthy if !is_root => {}
                ServiceState::Stopped => match self.launch_intent(&service_id) {
                    Ok(intent) => ordered.push(intent),
                    Err(_) => {
                        return AdmissionDecision::Refused(AdmissionRejection::EvidenceUnknown {
                            service: service_id,
                        })
                    }
                },
                ServiceState::Failed if is_root => match self.launch_intent(&service_id) {
                    Ok(intent) => ordered.push(intent),
                    Err(_) => {
                        return AdmissionDecision::Refused(AdmissionRejection::EvidenceUnknown {
                            service: service_id,
                        })
                    }
                },
                ServiceState::Failed => {
                    return AdmissionDecision::Refused(AdmissionRejection::DependencyNotReady {
                        service: root.clone(),
                        dependency: service_id,
                        state: ServiceState::Failed,
                    });
                }
                ServiceState::External => {
                    return AdmissionDecision::Refused(if is_root {
                        AdmissionRejection::ExternalNotControllable {
                            service: service_id,
                        }
                    } else {
                        AdmissionRejection::DependencyExternal {
                            service: root.clone(),
                            dependency: service_id,
                        }
                    });
                }
                ServiceState::Starting | ServiceState::Stopping => {
                    return AdmissionDecision::Refused(if is_root {
                        AdmissionRejection::OperationInProgress {
                            service: service_id,
                            action: ServiceAction::Start,
                        }
                    } else {
                        AdmissionRejection::DependencyNotReady {
                            service: root.clone(),
                            dependency: service_id,
                            state: dependency_runtime.state,
                        }
                    });
                }
                ServiceState::Unhealthy | ServiceState::Unknown => {
                    return AdmissionDecision::Refused(if is_root {
                        AdmissionRejection::EvidenceUnknown {
                            service: service_id,
                        }
                    } else {
                        AdmissionRejection::DependencyNotReady {
                            service: root.clone(),
                            dependency: service_id,
                            state: dependency_runtime.state,
                        }
                    });
                }
                ServiceState::Healthy => {
                    if is_root && force_root {
                        ordered.push(self.launch_intent(&service_id).expect("validated service"));
                    }
                }
            }
        }
        AdmissionDecision::Start(StartPlan { root, ordered })
    }

    fn admit_stop(&self, root: ServiceId, snapshot: &AdmissionSnapshot) -> AdmissionDecision {
        let runtime = snapshot.services.get(&root).expect("root checked by admit");
        if runtime.ownership == RuntimeOwnership::External
            || matches!(runtime.state, ServiceState::External)
        {
            return AdmissionDecision::Refused(AdmissionRejection::ExternalNotControllable {
                service: root,
            });
        }
        if let Some(operation) = &runtime.operation {
            if operation.action == ServiceAction::Stop
                && matches!(runtime.state, ServiceState::Stopping)
            {
                return AdmissionDecision::Coalesced {
                    service: root,
                    operation_id: operation.id,
                    action: ServiceAction::Stop,
                };
            }
            return AdmissionDecision::Refused(AdmissionRejection::OperationInProgress {
                service: root,
                action: operation.action,
            });
        }
        if matches!(runtime.state, ServiceState::Stopped) {
            return AdmissionDecision::Refused(AdmissionRejection::AlreadyStopped {
                service: root,
            });
        }
        if matches!(runtime.state, ServiceState::Unknown) {
            return AdmissionDecision::Refused(AdmissionRejection::EvidenceUnknown {
                service: root,
            });
        }

        let selected = self.stop_closure(&root);
        let ordered = self.reverse_selected_order(&selected);
        for service_id in &ordered {
            let Some(dependent) = snapshot.services.get(service_id) else {
                return AdmissionDecision::Refused(AdmissionRejection::EvidenceUnknown {
                    service: service_id.clone(),
                });
            };
            if dependent.ownership == RuntimeOwnership::External
                || matches!(dependent.state, ServiceState::External)
            {
                return AdmissionDecision::Refused(AdmissionRejection::ExternalNotControllable {
                    service: service_id.clone(),
                });
            }
            if matches!(dependent.state, ServiceState::Unknown) {
                return AdmissionDecision::Refused(AdmissionRejection::EvidenceUnknown {
                    service: service_id.clone(),
                });
            }
        }
        AdmissionDecision::Stop(StopPlan { root, ordered })
    }

    fn admit_restart(&self, root: ServiceId, snapshot: &AdmissionSnapshot) -> AdmissionDecision {
        let Some(runtime) = snapshot.services.get(&root) else {
            return AdmissionDecision::Refused(AdmissionRejection::EvidenceUnknown {
                service: root,
            });
        };
        if let Some(operation) = &runtime.operation {
            if operation.action == ServiceAction::Restart {
                return AdmissionDecision::Coalesced {
                    service: root,
                    operation_id: operation.id,
                    action: ServiceAction::Restart,
                };
            }
        }
        if matches!(runtime.state, ServiceState::External)
            || runtime.ownership == RuntimeOwnership::External
        {
            return AdmissionDecision::Refused(AdmissionRejection::ExternalNotControllable {
                service: root,
            });
        }
        let stop = match self.admit_stop(root.clone(), snapshot) {
            AdmissionDecision::Stop(plan) => plan,
            AdmissionDecision::Coalesced { .. } => {
                return AdmissionDecision::Refused(AdmissionRejection::OperationInProgress {
                    service: root,
                    action: ServiceAction::Stop,
                })
            }
            AdmissionDecision::Refused(reason) => return AdmissionDecision::Refused(reason),
            _ => unreachable!("stop admission returns only stop/coalesced/refused"),
        };
        match self.admit_start(root.clone(), snapshot, true) {
            AdmissionDecision::Start(start) => {
                AdmissionDecision::Restart(RestartPlan { stop, start })
            }
            AdmissionDecision::Coalesced { .. } | AdmissionDecision::Refused(_) => {
                AdmissionDecision::Refused(AdmissionRejection::DependencyNotReady {
                    service: root.clone(),
                    dependency: root,
                    state: ServiceState::Unknown,
                })
            }
            _ => unreachable!("forced start admission returns start/coalesced/refused"),
        }
    }

    fn stop_closure(&self, root: &ServiceId) -> BTreeSet<ServiceId> {
        let mut selected = BTreeSet::new();
        selected.insert(root.clone());
        for candidate in self.services.keys() {
            if candidate == root {
                continue;
            }
            if self
                .dependency_plan(candidate)
                .map(|plan| plan.ordered.contains(root))
                .unwrap_or(false)
            {
                selected.insert(candidate.clone());
            }
        }
        selected
    }

    fn reverse_selected_order(&self, selected: &BTreeSet<ServiceId>) -> Vec<ServiceId> {
        let mut ordered = Vec::new();
        let mut visited = BTreeSet::new();
        for id in self.services.keys() {
            if selected.contains(id) {
                visit_graph_selected(id, &self.services, selected, &mut visited, &mut ordered);
            }
        }
        ordered.reverse();
        ordered
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DependencyPlan {
    pub ordered: Vec<ServiceId>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PlanningError {
    UnknownService(ServiceId),
    Cycle(Vec<ServiceId>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AdmissionFence {
    pub generation: u64,
    pub epoch: u64,
}

impl AdmissionFence {
    pub const fn new(generation: u64, epoch: u64) -> Self {
        Self { generation, epoch }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdmissionRequest {
    pub action: ServiceAction,
    pub service_id: ServiceId,
    pub fence: AdmissionFence,
}

impl AdmissionRequest {
    pub fn new(action: ServiceAction, service_id: ServiceId, fence: AdmissionFence) -> Self {
        Self {
            action,
            service_id,
            fence,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServiceAction {
    Start,
    Stop,
    Restart,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RuntimeOwnership {
    None,
    Task { task_id: String },
    Host,
    External,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActiveOperation {
    pub id: u64,
    pub action: ServiceAction,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeRecord {
    pub state: ServiceState,
    pub fence: AdmissionFence,
    pub ownership: RuntimeOwnership,
    pub operation: Option<ActiveOperation>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AdmissionSnapshot {
    pub(crate) services: BTreeMap<ServiceId, RuntimeRecord>,
    pub(crate) task_epochs: BTreeMap<String, u64>,
    pub(crate) closing_tasks: BTreeSet<String>,
}

impl AdmissionSnapshot {
    pub fn set_service(&mut self, id: ServiceId, runtime: RuntimeRecord) {
        self.services.insert(id, runtime);
    }

    pub fn set_task_epoch(&mut self, task_id: impl Into<String>, epoch: u64) {
        self.task_epochs.insert(task_id.into(), epoch);
    }

    pub fn mark_task_closing(&mut self, task_id: impl Into<String>) {
        self.closing_tasks.insert(task_id.into());
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AdmissionDecision {
    Start(StartPlan),
    Stop(StopPlan),
    Restart(RestartPlan),
    Coalesced {
        service: ServiceId,
        operation_id: u64,
        action: ServiceAction,
    },
    Refused(AdmissionRejection),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AdmissionRejection {
    UnknownService {
        service: ServiceId,
    },
    StaleFence {
        service: ServiceId,
        expected: AdmissionFence,
        received: AdmissionFence,
    },
    TaskClosing {
        service: ServiceId,
    },
    TaskEpochStale {
        task_id: String,
        expected: Option<u64>,
        received: u64,
    },
    ExternalNotControllable {
        service: ServiceId,
    },
    DependencyExternal {
        service: ServiceId,
        dependency: ServiceId,
    },
    DependencyNotReady {
        service: ServiceId,
        dependency: ServiceId,
        state: ServiceState,
    },
    EvidenceUnknown {
        service: ServiceId,
    },
    OperationInProgress {
        service: ServiceId,
        action: ServiceAction,
    },
    AlreadyRunning {
        service: ServiceId,
    },
    AlreadyStopped {
        service: ServiceId,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StartPlan {
    pub root: ServiceId,
    pub ordered: Vec<LaunchIntent>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StopPlan {
    pub root: ServiceId,
    pub ordered: Vec<ServiceId>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RestartPlan {
    pub stop: StopPlan,
    pub start: StartPlan,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskClosePlan {
    pub task_id: String,
    pub epoch: u64,
    pub ordered: Vec<ServiceId>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ValidationField {
    Service,
    ServiceId,
    TaskId,
    Program,
    Argument,
    Cwd,
    EnvReference,
    Dependency,
    HealthPath,
    ExpectedPort,
    StartupPolicy,
    StopPolicy,
    HealthPolicy,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ValidationError {
    Empty {
        field: ValidationField,
    },
    TooLong {
        field: ValidationField,
        limit: usize,
    },
    TooMany {
        field: ValidationField,
        limit: usize,
    },
    InvalidIdentifier {
        field: ValidationField,
    },
    UnsafePath {
        field: ValidationField,
    },
    RawSecret {
        field: ValidationField,
    },
    InvalidPort {
        field: ValidationField,
        port: u16,
    },
    InvalidPolicy {
        field: ValidationField,
    },
    DuplicateServiceId {
        id: ServiceId,
    },
    DuplicateDependency {
        service: ServiceId,
        dependency: ServiceId,
    },
    SelfDependency {
        service: ServiceId,
    },
    UnknownDependency {
        service: ServiceId,
        dependency: ServiceId,
    },
    DependencyCycle {
        path: Vec<ServiceId>,
    },
}

impl fmt::Display for ValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty { field } => write!(formatter, "empty {field:?}"),
            Self::TooLong { field, limit } => write!(formatter, "{field:?} exceeds {limit} bytes"),
            Self::TooMany { field, limit } => {
                write!(formatter, "too many {field:?} values; limit {limit}")
            }
            Self::InvalidIdentifier { field } => write!(formatter, "invalid {field:?}"),
            Self::UnsafePath { field } => write!(formatter, "unsafe {field:?} path"),
            Self::RawSecret { field } => write!(formatter, "raw secret in {field:?}"),
            Self::InvalidPort { field, port } => write!(formatter, "invalid {field:?} port {port}"),
            Self::InvalidPolicy { field } => write!(formatter, "invalid {field:?}"),
            Self::DuplicateServiceId { id } => write!(formatter, "duplicate service id {id}"),
            Self::DuplicateDependency {
                service,
                dependency,
            } => {
                write!(formatter, "duplicate dependency {dependency} on {service}")
            }
            Self::SelfDependency { service } => {
                write!(formatter, "service {service} depends on itself")
            }
            Self::UnknownDependency {
                service,
                dependency,
            } => {
                write!(
                    formatter,
                    "service {service} depends on unknown {dependency}"
                )
            }
            Self::DependencyCycle { path } => write!(formatter, "dependency cycle: {path:?}"),
        }
    }
}

impl std::error::Error for ValidationError {}

fn validate_command(command: &CommandSpec) -> Result<(), ValidationError> {
    validate_text(
        &command.program,
        ValidationField::Program,
        MAX_PROGRAM_LENGTH,
    )?;
    if command.program.contains('/') || command.program.contains('\\') {
        return Err(ValidationError::UnsafePath {
            field: ValidationField::Program,
        });
    }
    if command.args.len() > MAX_ARGUMENT_COUNT {
        return Err(ValidationError::TooMany {
            field: ValidationField::Argument,
            limit: MAX_ARGUMENT_COUNT,
        });
    }
    for argument in &command.args {
        validate_text(argument, ValidationField::Argument, MAX_ARGUMENT_LENGTH)?;
        if looks_like_raw_secret(argument) {
            return Err(ValidationError::RawSecret {
                field: ValidationField::Argument,
            });
        }
    }
    if let Some(cwd) = &command.cwd {
        validate_text(cwd, ValidationField::Cwd, MAX_CWD_LENGTH)?;
        validate_relative_path(cwd, ValidationField::Cwd)?;
    }
    if command.env.len() > MAX_ENV_REFERENCE_COUNT {
        return Err(ValidationError::TooMany {
            field: ValidationField::EnvReference,
            limit: MAX_ENV_REFERENCE_COUNT,
        });
    }
    for reference in &command.env {
        validate_text(
            &reference.name,
            ValidationField::EnvReference,
            MAX_ENV_REFERENCE_LENGTH,
        )?;
        if reference.name.contains('=') || looks_like_raw_secret(&reference.name) {
            return Err(ValidationError::RawSecret {
                field: ValidationField::EnvReference,
            });
        }
        if !reference
            .name
            .chars()
            .enumerate()
            .all(|(index, character)| {
                character == '_'
                    || character.is_ascii_alphanumeric() && (index > 0 || character != '0')
            })
            || reference
                .name
                .starts_with(|character: char| character.is_ascii_digit())
        {
            return Err(ValidationError::InvalidIdentifier {
                field: ValidationField::EnvReference,
            });
        }
    }
    Ok(())
}

fn validate_health(health: &HealthSpec) -> Result<(), ValidationError> {
    match health {
        HealthSpec::None => Ok(()),
        HealthSpec::Tcp { port, policy } => {
            validate_port(*port, ValidationField::ExpectedPort)?;
            validate_health_policy(policy)
        }
        HealthSpec::Http { port, path, policy } => {
            validate_port(*port, ValidationField::ExpectedPort)?;
            validate_text(path, ValidationField::HealthPath, MAX_HEALTH_PATH_LENGTH)?;
            if !path.starts_with('/') || path.contains("..") {
                return Err(ValidationError::UnsafePath {
                    field: ValidationField::HealthPath,
                });
            }
            validate_health_policy(policy)
        }
    }
}

fn validate_health_policy(policy: &HealthPolicy) -> Result<(), ValidationError> {
    if policy.startup_deadline_ms == 0
        || policy.startup_deadline_ms > MAX_STARTUP_DEADLINE_MS
        || policy.probe_interval_ms == 0
        || policy.probe_interval_ms > policy.startup_deadline_ms
        || policy.max_probe_interval_ms < policy.probe_interval_ms
        || policy.max_probe_interval_ms > MAX_PROBE_INTERVAL_MS
        || !(1..=4).contains(&policy.backoff_multiplier)
        || !(1..=10).contains(&policy.success_threshold)
        || !(1..=10).contains(&policy.failure_threshold)
        || policy.stale_after_ms < policy.probe_interval_ms
        || policy.stale_after_ms > MAX_PROBE_INTERVAL_MS
    {
        return Err(ValidationError::InvalidPolicy {
            field: ValidationField::HealthPolicy,
        });
    }
    Ok(())
}

fn validate_identifier(
    value: &str,
    field: ValidationField,
    limit: usize,
) -> Result<(), ValidationError> {
    validate_text(value, field, limit)?;
    if !value
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.'))
        || value.starts_with('.')
    {
        return Err(ValidationError::InvalidIdentifier { field });
    }
    Ok(())
}

fn validate_text(value: &str, field: ValidationField, limit: usize) -> Result<(), ValidationError> {
    if value.is_empty() {
        return Err(ValidationError::Empty { field });
    }
    if value.len() > limit {
        return Err(ValidationError::TooLong { field, limit });
    }
    if value.chars().any(char::is_control) || value.contains('\0') {
        return Err(ValidationError::InvalidIdentifier { field });
    }
    Ok(())
}

fn validate_relative_path(value: &str, field: ValidationField) -> Result<(), ValidationError> {
    if value.starts_with('/')
        || value.starts_with('\\')
        || value.as_bytes().get(1).is_some_and(|byte| *byte == b':')
        || value.split(['/', '\\']).any(|component| component == "..")
    {
        return Err(ValidationError::UnsafePath { field });
    }
    Ok(())
}

fn validate_port(port: u16, field: ValidationField) -> Result<(), ValidationError> {
    if port == 0 {
        return Err(ValidationError::InvalidPort { field, port });
    }
    Ok(())
}

fn looks_like_raw_secret(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    lower.contains("password=")
        || lower.contains("token=")
        || lower.contains("secret=")
        || lower.contains("api_key=")
        || lower.contains("private_key=")
        || lower.contains("-----begin")
}

fn validate_graph(
    services: &BTreeMap<ServiceId, ServiceDefinition>,
) -> Result<(), ValidationError> {
    let mut visited = BTreeSet::new();
    let mut stack = Vec::new();
    for id in services.keys() {
        visit_graph_checked(id, services, &mut visited, &mut stack)?;
    }
    Ok(())
}

fn visit_graph_checked(
    id: &ServiceId,
    services: &BTreeMap<ServiceId, ServiceDefinition>,
    visited: &mut BTreeSet<ServiceId>,
    stack: &mut Vec<ServiceId>,
) -> Result<(), ValidationError> {
    if visited.contains(id) {
        return Ok(());
    }
    if let Some(position) = stack.iter().position(|item| item == id) {
        let mut path = stack[position..].to_vec();
        path.push(id.clone());
        return Err(ValidationError::DependencyCycle { path });
    }
    stack.push(id.clone());
    let mut dependencies = services[id].dependencies.clone();
    dependencies.sort();
    for dependency in dependencies {
        visit_graph_checked(&dependency, services, visited, stack)?;
    }
    stack.pop();
    visited.insert(id.clone());
    Ok(())
}

fn visit_graph(
    id: &ServiceId,
    services: &BTreeMap<ServiceId, ServiceDefinition>,
    visited: &mut BTreeSet<ServiceId>,
    ordered: &mut Vec<ServiceId>,
) -> Result<(), Vec<ServiceId>> {
    if visited.contains(id) {
        return Ok(());
    }
    visited.insert(id.clone());
    let mut dependencies = services[id].dependencies.clone();
    dependencies.sort();
    for dependency in dependencies {
        visit_graph(&dependency, services, visited, ordered)?;
    }
    ordered.push(id.clone());
    Ok(())
}

fn visit_graph_selected(
    id: &ServiceId,
    services: &BTreeMap<ServiceId, ServiceDefinition>,
    selected: &BTreeSet<ServiceId>,
    visited: &mut BTreeSet<ServiceId>,
    ordered: &mut Vec<ServiceId>,
) {
    if !selected.contains(id) || !visited.insert(id.clone()) {
        return;
    }
    let mut dependencies = services[id].dependencies.clone();
    dependencies.sort();
    for dependency in dependencies {
        visit_graph_selected(&dependency, services, selected, visited, ordered);
    }
    ordered.push(id.clone());
}
