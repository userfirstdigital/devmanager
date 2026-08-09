use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::de::Error as _;
use serde::ser::Error as _;
use serde::{Deserialize, Serialize, Serializer};

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

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
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

#[derive(Deserialize)]
#[serde(transparent)]
struct ServiceIdWire(String);

impl<'de> Deserialize<'de> for ServiceId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = ServiceIdWire::deserialize(deserializer)?;
        Self::new(wire.0).map_err(D::Error::custom)
    }
}

impl Serialize for ServiceId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        validate_identifier(&self.0, ValidationField::ServiceId, MAX_SERVICE_ID_LENGTH)
            .map_err(S::Error::custom)?;
        self.0.serialize(serializer)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ServiceScope {
    /// Resolve the command in the task's workspace and give that task ownership.
    Task { task_id: String },
    /// Resolve the command in the host workspace and give the host ownership.
    Host,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
enum ServiceScopeWire {
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

    fn validate(&self) -> Result<(), ValidationError> {
        if let Self::Task { task_id } = self {
            validate_identifier(task_id, ValidationField::TaskId, MAX_TASK_ID_LENGTH)?;
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for ServiceScope {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = ServiceScopeWire::deserialize(deserializer)?;
        let scope = match wire {
            ServiceScopeWire::Task { task_id } => Self::Task { task_id },
            ServiceScopeWire::Host => Self::Host,
        };
        scope.validate().map_err(D::Error::custom)?;
        Ok(scope)
    }
}

impl Serialize for ServiceScope {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.validate().map_err(S::Error::custom)?;
        let wire = match self {
            Self::Task { task_id } => ServiceScopeWire::Task {
                task_id: task_id.clone(),
            },
            Self::Host => ServiceScopeWire::Host,
        };
        wire.serialize(serializer)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EnvReference {
    pub name: String,
}

impl EnvReference {
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into() }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EnvReferenceWire {
    name: String,
}

impl<'de> Deserialize<'de> for EnvReference {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = EnvReferenceWire::deserialize(deserializer)?;
        let reference = Self::new(wire.name);
        validate_env_reference(&reference).map_err(D::Error::custom)?;
        Ok(reference)
    }
}

impl Serialize for EnvReference {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        validate_env_reference(self).map_err(S::Error::custom)?;
        EnvReferenceWire {
            name: self.name.clone(),
        }
        .serialize(serializer)
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
    /// Canonical path relative to the workspace selected by [`ServiceScope`].
    pub cwd: Option<String>,
    pub env: Vec<EnvReference>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandSpecWire {
    program: String,
    args: Vec<String>,
    cwd: Option<String>,
    env: Vec<EnvReference>,
}

impl fmt::Debug for CommandSpec {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let redacted_args = vec!["<redacted>"; self.args.len()];
        formatter
            .debug_struct("CommandSpec")
            .field("program", &self.program)
            .field("args", &redacted_args)
            .field("cwd", &self.cwd)
            .field("env", &self.env)
            .finish()
    }
}

impl<'de> Deserialize<'de> for CommandSpec {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = CommandSpecWire::deserialize(deserializer)?;
        let command = Self {
            program: wire.program,
            args: wire.args,
            cwd: wire.cwd,
            env: wire.env,
        };
        validate_command(&command).map_err(D::Error::custom)?;
        Ok(command)
    }
}

impl Serialize for CommandSpec {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        validate_command(self).map_err(S::Error::custom)?;
        CommandSpecWire {
            program: self.program.clone(),
            args: self.args.clone(),
            cwd: self.cwd.clone(),
            env: self.env.clone(),
        }
        .serialize(serializer)
    }
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExpectedPort {
    pub protocol: PortProtocol,
    pub port: u16,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExpectedPortWire {
    protocol: PortProtocol,
    port: u16,
}

impl<'de> Deserialize<'de> for ExpectedPort {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = ExpectedPortWire::deserialize(deserializer)?;
        validate_port(wire.port, ValidationField::ExpectedPort).map_err(D::Error::custom)?;
        Ok(Self {
            protocol: wire.protocol,
            port: wire.port,
        })
    }
}

impl Serialize for ExpectedPort {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        validate_port(self.port, ValidationField::ExpectedPort).map_err(S::Error::custom)?;
        ExpectedPortWire {
            protocol: self.protocol,
            port: self.port,
        }
        .serialize(serializer)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HealthPolicy {
    pub startup_deadline_ms: u64,
    pub probe_interval_ms: u64,
    pub max_probe_interval_ms: u64,
    pub backoff_multiplier: u8,
    pub success_threshold: u8,
    pub failure_threshold: u8,
    pub stale_after_ms: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HealthPolicyWire {
    startup_deadline_ms: u64,
    probe_interval_ms: u64,
    max_probe_interval_ms: u64,
    backoff_multiplier: u8,
    success_threshold: u8,
    failure_threshold: u8,
    stale_after_ms: u64,
}

impl<'de> Deserialize<'de> for HealthPolicy {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = HealthPolicyWire::deserialize(deserializer)?;
        let policy = Self {
            startup_deadline_ms: wire.startup_deadline_ms,
            probe_interval_ms: wire.probe_interval_ms,
            max_probe_interval_ms: wire.max_probe_interval_ms,
            backoff_multiplier: wire.backoff_multiplier,
            success_threshold: wire.success_threshold,
            failure_threshold: wire.failure_threshold,
            stale_after_ms: wire.stale_after_ms,
        };
        validate_health_policy(&policy).map_err(D::Error::custom)?;
        Ok(policy)
    }
}

impl Serialize for HealthPolicy {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        validate_health_policy(self).map_err(S::Error::custom)?;
        HealthPolicyWire {
            startup_deadline_ms: self.startup_deadline_ms,
            probe_interval_ms: self.probe_interval_ms,
            max_probe_interval_ms: self.max_probe_interval_ms,
            backoff_multiplier: self.backoff_multiplier,
            success_threshold: self.success_threshold,
            failure_threshold: self.failure_threshold,
            stale_after_ms: self.stale_after_ms,
        }
        .serialize(serializer)
    }
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

#[derive(Clone, Debug, Eq, PartialEq)]
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

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
enum HealthSpecWire {
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

impl<'de> Deserialize<'de> for HealthSpec {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = HealthSpecWire::deserialize(deserializer)?;
        let health = match wire {
            HealthSpecWire::None => Self::None,
            HealthSpecWire::Tcp { port, policy } => Self::Tcp { port, policy },
            HealthSpecWire::Http { port, path, policy } => Self::Http { port, path, policy },
        };
        validate_health(&health).map_err(D::Error::custom)?;
        Ok(health)
    }
}

impl Serialize for HealthSpec {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        validate_health(self).map_err(S::Error::custom)?;
        let wire = match self {
            Self::None => HealthSpecWire::None,
            Self::Tcp { port, policy } => HealthSpecWire::Tcp {
                port: *port,
                policy: *policy,
            },
            Self::Http { port, path, policy } => HealthSpecWire::Http {
                port: *port,
                path: path.clone(),
                policy: *policy,
            },
        };
        wire.serialize(serializer)
    }
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StartupPolicy {
    pub trigger: StartupTrigger,
    pub restart_limit: u8,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StartupPolicyWire {
    trigger: StartupTrigger,
    restart_limit: u8,
}

impl<'de> Deserialize<'de> for StartupPolicy {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = StartupPolicyWire::deserialize(deserializer)?;
        let policy = Self {
            trigger: wire.trigger,
            restart_limit: wire.restart_limit,
        };
        validate_startup_policy(&policy).map_err(D::Error::custom)?;
        Ok(policy)
    }
}

impl Serialize for StartupPolicy {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        validate_startup_policy(self).map_err(S::Error::custom)?;
        StartupPolicyWire {
            trigger: self.trigger,
            restart_limit: self.restart_limit,
        }
        .serialize(serializer)
    }
}

impl StartupPolicy {
    pub fn manual() -> Self {
        Self {
            trigger: StartupTrigger::Manual,
            restart_limit: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StopPolicy {
    pub graceful_timeout_ms: u64,
    pub kill_after_ms: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StopPolicyWire {
    graceful_timeout_ms: u64,
    kill_after_ms: u64,
}

impl<'de> Deserialize<'de> for StopPolicy {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = StopPolicyWire::deserialize(deserializer)?;
        let policy = Self {
            graceful_timeout_ms: wire.graceful_timeout_ms,
            kill_after_ms: wire.kill_after_ms,
        };
        validate_stop_policy(&policy).map_err(D::Error::custom)?;
        Ok(policy)
    }
}

impl Serialize for StopPolicy {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        validate_stop_policy(self).map_err(S::Error::custom)?;
        StopPolicyWire {
            graceful_timeout_ms: self.graceful_timeout_ms,
            kill_after_ms: self.kill_after_ms,
        }
        .serialize(serializer)
    }
}

impl Default for StopPolicy {
    fn default() -> Self {
        Self {
            graceful_timeout_ms: 1_000,
            kill_after_ms: 10_000,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
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

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ServiceDefinitionWire {
    id: ServiceId,
    scope: ServiceScope,
    command: CommandSpec,
    dependencies: Vec<ServiceId>,
    health: HealthSpec,
    startup: StartupPolicy,
    stop: StopPolicy,
    expected_port: Option<ExpectedPort>,
}

impl<'de> Deserialize<'de> for ServiceDefinition {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = ServiceDefinitionWire::deserialize(deserializer)?;
        let definition = Self {
            id: wire.id,
            scope: wire.scope,
            command: wire.command,
            dependencies: wire.dependencies,
            health: wire.health,
            startup: wire.startup,
            stop: wire.stop,
            expected_port: wire.expected_port,
        };
        definition.validate().map_err(D::Error::custom)?;
        Ok(definition)
    }
}

impl Serialize for ServiceDefinition {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.validate().map_err(S::Error::custom)?;
        ServiceDefinitionWire {
            id: self.id.clone(),
            scope: self.scope.clone(),
            command: self.command.clone(),
            dependencies: self.dependencies.clone(),
            health: self.health.clone(),
            startup: self.startup,
            stop: self.stop,
            expected_port: self.expected_port,
        }
        .serialize(serializer)
    }
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
        validate_startup_policy(&self.startup)?;
        validate_stop_policy(&self.stop)?;
        if let Some(expected_port) = self.expected_port {
            validate_port(expected_port.port, ValidationField::ExpectedPort)?;
            if let Some(health_port) = health_port(&self.health) {
                if expected_port.port != health_port {
                    return Err(ValidationError::PortMismatch {
                        expected_port: expected_port.port,
                        health_port,
                    });
                }
            }
        }
        Ok(())
    }

    fn effective_expected_port(&self) -> Option<ExpectedPort> {
        self.expected_port.or_else(|| {
            health_port(&self.health).map(|port| ExpectedPort {
                protocol: PortProtocol::Tcp,
                port,
            })
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LaunchIntent {
    pub service_id: ServiceId,
    pub scope: ServiceScope,
    pub command: CommandSpec,
    pub dependencies: Vec<ServiceId>,
    pub expected_port: Option<ExpectedPort>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LaunchIntentWire {
    service_id: ServiceId,
    scope: ServiceScope,
    command: CommandSpec,
    dependencies: Vec<ServiceId>,
    expected_port: Option<ExpectedPort>,
}

impl<'de> Deserialize<'de> for LaunchIntent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = LaunchIntentWire::deserialize(deserializer)?;
        let intent = Self {
            service_id: wire.service_id,
            scope: wire.scope,
            command: wire.command,
            dependencies: wire.dependencies,
            expected_port: wire.expected_port,
        };
        intent.validate().map_err(D::Error::custom)?;
        Ok(intent)
    }
}

impl Serialize for LaunchIntent {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.validate().map_err(S::Error::custom)?;
        LaunchIntentWire {
            service_id: self.service_id.clone(),
            scope: self.scope.clone(),
            command: self.command.clone(),
            dependencies: self.dependencies.clone(),
            expected_port: self.expected_port,
        }
        .serialize(serializer)
    }
}

impl LaunchIntent {
    pub fn validate(&self) -> Result<(), ValidationError> {
        validate_identifier(
            self.service_id.as_str(),
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
            if dependency == &self.service_id {
                return Err(ValidationError::SelfDependency {
                    service: self.service_id.clone(),
                });
            }
            if !dependencies.insert(dependency.clone()) {
                return Err(ValidationError::DuplicateDependency {
                    service: self.service_id.clone(),
                    dependency: dependency.clone(),
                });
            }
        }
        if let Some(expected_port) = self.expected_port {
            validate_port(expected_port.port, ValidationField::ExpectedPort)?;
        }
        Ok(())
    }
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
        let intent = LaunchIntent {
            service_id: definition.id.clone(),
            scope: definition.scope.clone(),
            command: definition.command.clone(),
            dependencies: definition.dependencies.clone(),
            expected_port: definition.effective_expected_port(),
        };
        debug_assert!(intent.validate().is_ok());
        Ok(intent)
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
        if !snapshot.closing_tasks.contains(task_id) {
            return Err(AdmissionRejection::TaskCloseNotAdmitted {
                task_id: task_id.to_owned(),
            });
        }
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
                if let Some(operation) = &runtime.operation {
                    return Err(AdmissionRejection::OperationInProgress {
                        service: definition.id.clone(),
                        action: operation.action,
                    });
                }
                if matches!(
                    runtime.state,
                    ServiceState::Starting | ServiceState::Stopping
                ) {
                    return Err(AdmissionRejection::OperationInProgress {
                        service: definition.id.clone(),
                        action: if matches!(runtime.state, ServiceState::Starting) {
                            ServiceAction::Start
                        } else {
                            ServiceAction::Stop
                        },
                    });
                }
                if !matches!(runtime.state, ServiceState::Stopped) {
                    selected.insert(definition.id.clone());
                }
            }
        }
        let ordered = self
            .reverse_selected_order(&selected)
            .into_iter()
            .filter_map(|service_id| {
                let definition = self.services.get(&service_id)?;
                snapshot
                    .services
                    .get(&service_id)
                    .map(|runtime| ServicePlanItem {
                        service_id: service_id.clone(),
                        fence: ServiceFence::capture(&service_id, runtime),
                        expected_state: runtime.state.clone(),
                        scope: definition.scope.clone(),
                    })
            })
            .collect();
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
        let definition = self
            .services
            .get(&root)
            .expect("root definition checked by admit");
        let runtime = snapshot.services.get(&root).expect("root checked by admit");
        if let Err(rejection) =
            validate_ownership(&root, definition, runtime, ServiceAction::Start, true)
        {
            return AdmissionDecision::Refused(rejection);
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
            if let Some(operation) = &dependency_runtime.operation {
                return AdmissionDecision::Refused(AdmissionRejection::OperationInProgress {
                    service: service_id,
                    action: operation.action,
                });
            }
            let definition = self
                .services
                .get(&service_id)
                .expect("dependency is validated in catalog");
            match dependency_runtime.state {
                ServiceState::Healthy if !is_root => {
                    if let Err(rejection) = validate_ownership(
                        &service_id,
                        definition,
                        dependency_runtime,
                        ServiceAction::Start,
                        false,
                    ) {
                        return AdmissionDecision::Refused(rejection);
                    }
                    ordered.push(StartPlanItem {
                        service_id: service_id.clone(),
                        intent: None,
                        fence: ServiceFence::capture(&service_id, dependency_runtime),
                        expected_state: dependency_runtime.state.clone(),
                        scope: definition.scope.clone(),
                    });
                }
                ServiceState::Stopped => {
                    if let Err(rejection) = validate_ownership(
                        &service_id,
                        definition,
                        dependency_runtime,
                        ServiceAction::Start,
                        true,
                    ) {
                        return AdmissionDecision::Refused(rejection);
                    }
                    let intent = match self.launch_intent(&service_id) {
                        Ok(intent) => intent,
                        Err(_) => {
                            return AdmissionDecision::Refused(
                                AdmissionRejection::EvidenceUnknown {
                                    service: service_id,
                                },
                            )
                        }
                    };
                    ordered.push(StartPlanItem {
                        service_id: service_id.clone(),
                        intent: Some(intent),
                        fence: ServiceFence::capture(&service_id, dependency_runtime),
                        expected_state: dependency_runtime.state.clone(),
                        scope: definition.scope.clone(),
                    });
                }
                ServiceState::Failed if is_root => {
                    if let Err(rejection) = validate_ownership(
                        &service_id,
                        definition,
                        dependency_runtime,
                        ServiceAction::Start,
                        false,
                    ) {
                        return AdmissionDecision::Refused(rejection);
                    }
                    let intent = match self.launch_intent(&service_id) {
                        Ok(intent) => intent,
                        Err(_) => {
                            return AdmissionDecision::Refused(
                                AdmissionRejection::EvidenceUnknown {
                                    service: service_id,
                                },
                            )
                        }
                    };
                    ordered.push(StartPlanItem {
                        service_id: service_id.clone(),
                        intent: Some(intent),
                        fence: ServiceFence::capture(&service_id, dependency_runtime),
                        expected_state: dependency_runtime.state.clone(),
                        scope: definition.scope.clone(),
                    });
                }
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
                    return AdmissionDecision::Refused(AdmissionRejection::OperationInProgress {
                        service: service_id,
                        action: if matches!(dependency_runtime.state, ServiceState::Starting) {
                            ServiceAction::Start
                        } else {
                            ServiceAction::Stop
                        },
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
                            state: dependency_runtime.state.clone(),
                        }
                    });
                }
                ServiceState::Healthy => {
                    if is_root && force_root {
                        if let Err(rejection) = validate_ownership(
                            &service_id,
                            definition,
                            dependency_runtime,
                            ServiceAction::Start,
                            false,
                        ) {
                            return AdmissionDecision::Refused(rejection);
                        }
                        let intent = match self.launch_intent(&service_id) {
                            Ok(intent) => intent,
                            Err(_) => {
                                return AdmissionDecision::Refused(
                                    AdmissionRejection::EvidenceUnknown {
                                        service: service_id,
                                    },
                                )
                            }
                        };
                        ordered.push(StartPlanItem {
                            service_id: service_id.clone(),
                            intent: Some(intent),
                            fence: ServiceFence::capture(&service_id, dependency_runtime),
                            expected_state: dependency_runtime.state.clone(),
                            scope: definition.scope.clone(),
                        });
                    }
                }
            }
        }
        AdmissionDecision::Start(StartPlan { root, ordered })
    }

    fn admit_stop(&self, root: ServiceId, snapshot: &AdmissionSnapshot) -> AdmissionDecision {
        let definition = self
            .services
            .get(&root)
            .expect("root definition checked by admit");
        let runtime = snapshot.services.get(&root).expect("root checked by admit");
        if let Err(rejection) =
            validate_ownership(&root, definition, runtime, ServiceAction::Stop, false)
        {
            return AdmissionDecision::Refused(rejection);
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
        if matches!(
            runtime.state,
            ServiceState::Starting | ServiceState::Stopping
        ) {
            return AdmissionDecision::Refused(AdmissionRejection::OperationInProgress {
                service: root,
                action: if matches!(runtime.state, ServiceState::Starting) {
                    ServiceAction::Start
                } else {
                    ServiceAction::Stop
                },
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
        let mut ordered = Vec::new();
        for service_id in self.reverse_selected_order(&selected) {
            let Some(dependent) = snapshot.services.get(&service_id) else {
                return AdmissionDecision::Refused(AdmissionRejection::EvidenceUnknown {
                    service: service_id,
                });
            };
            if let Some(operation) = &dependent.operation {
                return AdmissionDecision::Refused(AdmissionRejection::OperationInProgress {
                    service: service_id,
                    action: operation.action,
                });
            }
            if matches!(dependent.state, ServiceState::Stopped) {
                continue;
            }
            if matches!(
                dependent.state,
                ServiceState::Starting | ServiceState::Stopping
            ) {
                return AdmissionDecision::Refused(AdmissionRejection::OperationInProgress {
                    service: service_id,
                    action: if matches!(dependent.state, ServiceState::Starting) {
                        ServiceAction::Start
                    } else {
                        ServiceAction::Stop
                    },
                });
            }
            if matches!(dependent.state, ServiceState::Unknown) {
                return AdmissionDecision::Refused(AdmissionRejection::EvidenceUnknown {
                    service: service_id,
                });
            }
            let definition = self
                .services
                .get(&service_id)
                .expect("selected service is validated in catalog");
            if let Err(rejection) = validate_ownership(
                &service_id,
                definition,
                dependent,
                ServiceAction::Stop,
                false,
            ) {
                return AdmissionDecision::Refused(rejection);
            }
            ordered.push(ServicePlanItem {
                service_id: service_id.clone(),
                fence: ServiceFence::capture(&service_id, dependent),
                expected_state: dependent.state.clone(),
                scope: definition.scope.clone(),
            });
        }
        AdmissionDecision::Stop(StopPlan { root, ordered })
    }

    fn admit_restart(&self, root: ServiceId, snapshot: &AdmissionSnapshot) -> AdmissionDecision {
        let Some(runtime) = snapshot.services.get(&root) else {
            return AdmissionDecision::Refused(AdmissionRejection::EvidenceUnknown {
                service: root,
            });
        };
        let definition = self
            .services
            .get(&root)
            .expect("root definition checked by admit");
        if let Err(rejection) =
            validate_ownership(&root, definition, runtime, ServiceAction::Restart, false)
        {
            return AdmissionDecision::Refused(rejection);
        }
        if let Some(operation) = &runtime.operation {
            if operation.action == ServiceAction::Restart {
                return AdmissionDecision::Coalesced {
                    service: root,
                    operation_id: operation.id,
                    action: ServiceAction::Restart,
                };
            }
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

/// Immutable compare-and-swap token captured for one service-plan member.
///
/// The runtime generation/epoch pair is not sufficient by itself: ownership
/// is part of the authority being revalidated and must change the token too.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServiceFence {
    pub service_id: ServiceId,
    pub generation: u64,
    pub epoch: u64,
    pub ownership: RuntimeOwnership,
}

impl ServiceFence {
    fn capture(service_id: &ServiceId, runtime: &RuntimeRecord) -> Self {
        Self {
            service_id: service_id.clone(),
            generation: runtime.fence.generation,
            epoch: runtime.fence.epoch,
            ownership: runtime.ownership.clone(),
        }
    }
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
    TaskCloseNotAdmitted {
        task_id: String,
    },
    TaskEpochStale {
        task_id: String,
        expected: Option<u64>,
        received: u64,
    },
    OwnershipMismatch {
        service: ServiceId,
        expected: RuntimeOwnership,
        received: RuntimeOwnership,
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
    PlanStale {
        service: ServiceId,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StartPlan {
    pub root: ServiceId,
    pub ordered: Vec<StartPlanItem>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StopPlan {
    pub root: ServiceId,
    pub ordered: Vec<StopPlanItem>,
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
    pub ordered: Vec<ClosePlanItem>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StartPlanItem {
    pub service_id: ServiceId,
    pub intent: Option<LaunchIntent>,
    pub fence: ServiceFence,
    pub expected_state: ServiceState,
    pub scope: ServiceScope,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServicePlanItem {
    pub service_id: ServiceId,
    pub fence: ServiceFence,
    pub expected_state: ServiceState,
    pub scope: ServiceScope,
}

pub type StopPlanItem = ServicePlanItem;
pub type ClosePlanItem = ServicePlanItem;

impl StartPlan {
    /// Revalidate every member against one immutable snapshot before effects.
    pub fn revalidate(&self, snapshot: &AdmissionSnapshot) -> Result<(), AdmissionRejection> {
        for item in &self.ordered {
            revalidate_start_item(item, snapshot)?;
        }
        Ok(())
    }
}

impl StopPlan {
    /// Revalidate every member against one immutable snapshot before effects.
    pub fn revalidate(&self, snapshot: &AdmissionSnapshot) -> Result<(), AdmissionRejection> {
        for item in &self.ordered {
            revalidate_service_item(item, snapshot)?;
        }
        Ok(())
    }
}

impl RestartPlan {
    /// Revalidate the complete stop/start decision against one snapshot.
    pub fn revalidate(&self, snapshot: &AdmissionSnapshot) -> Result<(), AdmissionRejection> {
        self.stop.revalidate(snapshot)?;
        self.start.revalidate(snapshot)
    }
}

impl TaskClosePlan {
    /// Revalidate the close barrier, epoch, and every owned member atomically.
    pub fn revalidate(&self, snapshot: &AdmissionSnapshot) -> Result<(), AdmissionRejection> {
        if !snapshot.closing_tasks.contains(&self.task_id) {
            return Err(AdmissionRejection::TaskCloseNotAdmitted {
                task_id: self.task_id.clone(),
            });
        }
        if snapshot.task_epochs.get(&self.task_id).copied() != Some(self.epoch) {
            return Err(AdmissionRejection::TaskEpochStale {
                task_id: self.task_id.clone(),
                expected: snapshot.task_epochs.get(&self.task_id).copied(),
                received: self.epoch,
            });
        }
        for item in &self.ordered {
            if item.scope
                != (ServiceScope::Task {
                    task_id: self.task_id.clone(),
                })
            {
                return Err(AdmissionRejection::PlanStale {
                    service: item.service_id.clone(),
                });
            }
            if item.fence.ownership
                != (RuntimeOwnership::Task {
                    task_id: self.task_id.clone(),
                })
            {
                return Err(AdmissionRejection::PlanStale {
                    service: item.service_id.clone(),
                });
            }
            revalidate_close_item(item, snapshot)?;
        }
        Ok(())
    }
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
    PortMismatch {
        expected_port: u16,
        health_port: u16,
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
            Self::PortMismatch {
                expected_port,
                health_port,
            } => write!(
                formatter,
                "expected port {expected_port} does not match health port {health_port}"
            ),
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
        if is_secret_argument(argument) {
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
        validate_env_reference(reference)?;
    }
    Ok(())
}

fn validate_env_reference(reference: &EnvReference) -> Result<(), ValidationError> {
    validate_text(
        &reference.name,
        ValidationField::EnvReference,
        MAX_ENV_REFERENCE_LENGTH,
    )?;
    if reference.name.contains('=') {
        return Err(ValidationError::RawSecret {
            field: ValidationField::EnvReference,
        });
    }
    if !reference
        .name
        .chars()
        .enumerate()
        .all(|(index, character)| {
            character == '_' || character.is_ascii_alphanumeric() && (index > 0 || character != '0')
        })
        || reference
            .name
            .starts_with(|character: char| character.is_ascii_digit())
    {
        return Err(ValidationError::InvalidIdentifier {
            field: ValidationField::EnvReference,
        });
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

fn validate_startup_policy(policy: &StartupPolicy) -> Result<(), ValidationError> {
    if policy.restart_limit > 10 {
        return Err(ValidationError::InvalidPolicy {
            field: ValidationField::StartupPolicy,
        });
    }
    Ok(())
}

fn validate_stop_policy(policy: &StopPolicy) -> Result<(), ValidationError> {
    if policy.graceful_timeout_ms == 0
        || policy.graceful_timeout_ms > MAX_STOP_TIMEOUT_MS
        || policy.kill_after_ms < policy.graceful_timeout_ms
        || policy.kill_after_ms > MAX_STOP_TIMEOUT_MS
    {
        return Err(ValidationError::InvalidPolicy {
            field: ValidationField::StopPolicy,
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
    let canonical_root = value == ".";
    let has_noncanonical_component = !canonical_root
        && value
            .split('/')
            .any(|component| component.is_empty() || component == ".");
    if value != value.trim()
        || value.contains('\\')
        || has_noncanonical_component
        || value.starts_with('/')
        || value.starts_with('\\')
        || value.as_bytes().get(1).is_some_and(|byte| *byte == b':')
        || value.split('/').any(|component| component == "..")
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

fn health_port(health: &HealthSpec) -> Option<u16> {
    match health {
        HealthSpec::None => None,
        HealthSpec::Tcp { port, .. } | HealthSpec::Http { port, .. } => Some(*port),
    }
}

fn is_secret_argument(value: &str) -> bool {
    let option = value.split_once('=').map_or(value, |(name, _)| name);
    matches!(
        option.to_ascii_lowercase().as_str(),
        "--token"
            | "--api-token"
            | "--access-token"
            | "--api-key"
            | "--api_key"
            | "--secret"
            | "--client-secret"
            | "--password"
            | "--private-key"
            | "--private_key"
    ) || is_secret_assignment(value)
}

fn is_secret_assignment(value: &str) -> bool {
    let Some((name, _)) = value.split_once('=') else {
        return false;
    };
    matches!(
        name.to_ascii_lowercase().as_str(),
        "token"
            | "api-token"
            | "api_token"
            | "access-token"
            | "access_token"
            | "api-key"
            | "api_key"
            | "secret"
            | "client-secret"
            | "client_secret"
            | "password"
            | "private-key"
            | "private_key"
    ) || value.starts_with("-----BEGIN ")
}

fn expected_ownership(scope: &ServiceScope) -> RuntimeOwnership {
    match scope {
        ServiceScope::Task { task_id } => RuntimeOwnership::Task {
            task_id: task_id.clone(),
        },
        ServiceScope::Host => RuntimeOwnership::Host,
    }
}

fn validate_ownership(
    service_id: &ServiceId,
    definition: &ServiceDefinition,
    runtime: &RuntimeRecord,
    action: ServiceAction,
    allow_initial_claim: bool,
) -> Result<(), AdmissionRejection> {
    if runtime.ownership == RuntimeOwnership::External
        || matches!(runtime.state, ServiceState::External)
    {
        return Err(AdmissionRejection::ExternalNotControllable {
            service: service_id.clone(),
        });
    }

    if allow_initial_claim
        && action == ServiceAction::Start
        && matches!(runtime.state, ServiceState::Stopped)
        && runtime.ownership == RuntimeOwnership::None
    {
        return Ok(());
    }

    let expected = expected_ownership(&definition.scope);
    if runtime.ownership == expected {
        Ok(())
    } else {
        Err(AdmissionRejection::OwnershipMismatch {
            service: service_id.clone(),
            expected,
            received: runtime.ownership.clone(),
        })
    }
}

fn revalidate_start_item(
    item: &StartPlanItem,
    snapshot: &AdmissionSnapshot,
) -> Result<(), AdmissionRejection> {
    revalidate_service_item(
        &ServicePlanItem {
            service_id: item.service_id.clone(),
            fence: item.fence.clone(),
            expected_state: item.expected_state,
            scope: item.scope.clone(),
        },
        snapshot,
    )
}

fn revalidate_service_item(
    item: &ServicePlanItem,
    snapshot: &AdmissionSnapshot,
) -> Result<(), AdmissionRejection> {
    revalidate_service_item_inner(item, snapshot, true)
}

fn revalidate_close_item(
    item: &ServicePlanItem,
    snapshot: &AdmissionSnapshot,
) -> Result<(), AdmissionRejection> {
    revalidate_service_item_inner(item, snapshot, false)
}

fn revalidate_service_item_inner(
    item: &ServicePlanItem,
    snapshot: &AdmissionSnapshot,
    reject_closing: bool,
) -> Result<(), AdmissionRejection> {
    if reject_closing {
        if let ServiceScope::Task { task_id } = &item.scope {
            if snapshot.closing_tasks.contains(task_id) {
                return Err(AdmissionRejection::TaskClosing {
                    service: item.service_id.clone(),
                });
            }
        }
    }
    if reject_closing {
        if let RuntimeOwnership::Task { task_id } = &item.fence.ownership {
            if snapshot.closing_tasks.contains(task_id) {
                return Err(AdmissionRejection::TaskClosing {
                    service: item.service_id.clone(),
                });
            }
        }
    }
    let Some(runtime) = snapshot.services.get(&item.service_id) else {
        return Err(AdmissionRejection::EvidenceUnknown {
            service: item.service_id.clone(),
        });
    };
    if runtime.fence.generation != item.fence.generation
        || runtime.fence.epoch != item.fence.epoch
        || runtime.ownership != item.fence.ownership
        || runtime.state != item.expected_state
    {
        return Err(AdmissionRejection::PlanStale {
            service: item.service_id.clone(),
        });
    }
    if let Some(operation) = &runtime.operation {
        return Err(AdmissionRejection::OperationInProgress {
            service: item.service_id.clone(),
            action: operation.action,
        });
    }
    Ok(())
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
