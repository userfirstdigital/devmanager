use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::marker::PhantomData;

use serde::de::{Error as _, IgnoredAny, SeqAccess, Visitor};
use serde::ser::Error as _;
use serde::{Deserialize, Serialize, Serializer};
use sha2::{Digest, Sha256};

use crate::domain::TaskId;

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
pub const SERVICE_CATALOG_SCHEMA_VERSION: u16 = 1;
/// Maximum encoded JSON frame accepted by the service-catalog boundary.
///
/// Callers that receive catalog bytes must use [`ServiceCatalog::decode_json`]
/// rather than handing untrusted input directly to `serde_json`. The decoder
/// performs a bounded lexical pass before serde is allowed to allocate any
/// catalog values.
pub const MAX_SERVICE_CATALOG_FRAME_BYTES: usize = 256 * 1024;
pub const MAX_SERVICE_CATALOG_JSON_STRING_BYTES: usize = 4096;
pub const MAX_SERVICE_CATALOG_JSON_FIELD_NAME_BYTES: usize = 128;
pub const MAX_SERVICE_CATALOG_JSON_ARRAY_ITEMS: usize = 128;
pub const MAX_SERVICE_CATALOG_JSON_OBJECT_FIELDS: usize = 32;
pub const MAX_SERVICE_CATALOG_JSON_DEPTH: usize = 16;

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
struct ServiceIdWire(#[serde(deserialize_with = "deserialize_service_id_string")] String);

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
    Task { task_id: TaskId },
    /// Resolve the command in the host workspace and give the host ownership.
    Host,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
enum ServiceScopeWire {
    Task {
        #[serde(deserialize_with = "deserialize_task_id_string")]
        task_id: String,
    },
    Host,
}

impl ServiceScope {
    pub fn task(task_id: TaskId) -> Self {
        Self::Task { task_id }
    }

    pub fn task_id(&self) -> Option<&TaskId> {
        match self {
            Self::Task { task_id } => Some(task_id),
            Self::Host => None,
        }
    }

    fn validate(&self) -> Result<(), ValidationError> {
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
            ServiceScopeWire::Task { task_id } => Self::Task {
                task_id: TaskId::parse(&task_id).map_err(D::Error::custom)?,
            },
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
                task_id: task_id.to_string(),
            },
            Self::Host => ServiceScopeWire::Host,
        };
        wire.serialize(serializer)
    }
}

#[derive(Clone, Eq, Ord, PartialEq, PartialOrd)]
pub struct ValidatedExecutable(String);

impl ValidatedExecutable {
    pub fn new(value: impl Into<String>) -> Result<Self, ValidationError> {
        let value = value.into();
        validate_executable(&value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ValidatedExecutable {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted executable>")
    }
}

impl<'de> Deserialize<'de> for ValidatedExecutable {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = deserialize_bounded_string::<D, MAX_PROGRAM_LENGTH>(deserializer)?;
        Self::new(value).map_err(D::Error::custom)
    }
}

impl Serialize for ValidatedExecutable {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        validate_executable(&self.0).map_err(S::Error::custom)?;
        serializer.serialize_str(&self.0)
    }
}

#[derive(Clone, Eq, Ord, PartialEq, PartialOrd)]
pub struct WorkspacePath(String);

impl WorkspacePath {
    pub fn new(value: impl Into<String>) -> Result<Self, ValidationError> {
        let value = value.into();
        validate_text(&value, ValidationField::Cwd, MAX_CWD_LENGTH)?;
        validate_relative_path(&value, ValidationField::Cwd)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for WorkspacePath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted workspace path>")
    }
}

impl<'de> Deserialize<'de> for WorkspacePath {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = deserialize_bounded_string::<D, MAX_CWD_LENGTH>(deserializer)?;
        Self::new(value).map_err(D::Error::custom)
    }
}

impl Serialize for WorkspacePath {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        Self::new(self.0.clone()).map_err(S::Error::custom)?;
        serializer.serialize_str(&self.0)
    }
}

#[derive(Clone, Eq, Ord, PartialEq, PartialOrd)]
pub struct CommandArgument(String);

impl CommandArgument {
    pub fn new(value: impl Into<String>) -> Result<Self, ValidationError> {
        let value = value.into();
        validate_text(&value, ValidationField::Argument, MAX_ARGUMENT_LENGTH)?;
        if is_secret_argument(&value) {
            return Err(ValidationError::RawSecret {
                field: ValidationField::Argument,
            });
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for CommandArgument {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted argument>")
    }
}

impl<'de> Deserialize<'de> for CommandArgument {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = deserialize_bounded_string::<D, MAX_ARGUMENT_LENGTH>(deserializer)?;
        Self::new(value).map_err(D::Error::custom)
    }
}

impl Serialize for CommandArgument {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        Self::new(self.0.clone()).map_err(S::Error::custom)?;
        serializer.serialize_str(&self.0)
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct EnvReference {
    name: String,
}

impl EnvReference {
    pub fn new(name: impl Into<String>) -> Result<Self, ValidationError> {
        let name = name.into();
        validate_env_name(&name)?;
        Ok(Self { name })
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

impl fmt::Debug for EnvReference {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted environment name>")
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EnvReferenceWire {
    #[serde(deserialize_with = "deserialize_env_name")]
    name: String,
}

impl<'de> Deserialize<'de> for EnvReference {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = EnvReferenceWire::deserialize(deserializer)?;
        Self::new(wire.name).map_err(D::Error::custom)
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
    program: ValidatedExecutable,
    args: Vec<CommandArgument>,
    /// Canonical path relative to the workspace selected by [`ServiceScope`].
    cwd: Option<WorkspacePath>,
    env: Vec<EnvReference>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandSpecWire {
    program: ValidatedExecutable,
    #[serde(deserialize_with = "deserialize_command_arguments")]
    args: Vec<CommandArgument>,
    cwd: Option<WorkspacePath>,
    #[serde(deserialize_with = "deserialize_env_references")]
    env: Vec<EnvReference>,
}

impl fmt::Debug for CommandSpec {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CommandSpec")
            .field("program", &"<redacted>")
            .field("args", &format_args!("<{} redacted>", self.args.len()))
            .field("cwd", &self.cwd.as_ref().map(|_| "<redacted>"))
            .field("env", &format_args!("<{} names redacted>", self.env.len()))
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
    pub fn new(program: impl Into<String>) -> Result<Self, ValidationError> {
        Ok(Self {
            program: ValidatedExecutable::new(program)?,
            args: Vec::new(),
            cwd: None,
            env: Vec::new(),
        })
    }

    pub fn program(&self) -> &ValidatedExecutable {
        &self.program
    }

    pub fn args(&self) -> &[CommandArgument] {
        &self.args
    }

    pub fn cwd(&self) -> Option<&WorkspacePath> {
        self.cwd.as_ref()
    }

    pub fn env(&self) -> &[EnvReference] {
        &self.env
    }

    pub fn with_arg(mut self, arg: impl Into<String>) -> Result<Self, ValidationError> {
        if self.args.len() >= MAX_ARGUMENT_COUNT {
            return Err(ValidationError::TooMany {
                field: ValidationField::Argument,
                limit: MAX_ARGUMENT_COUNT,
            });
        }
        self.args.push(CommandArgument::new(arg)?);
        if let Err(error) = validate_command_arguments(&self.args) {
            self.args.pop();
            return Err(error);
        }
        Ok(self)
    }

    pub fn with_args<I, S>(mut self, args: I) -> Result<Self, ValidationError>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        for arg in args {
            self = self.with_arg(arg)?;
        }
        Ok(self)
    }

    pub fn with_cwd(mut self, cwd: impl Into<String>) -> Result<Self, ValidationError> {
        self.cwd = Some(WorkspacePath::new(cwd)?);
        Ok(self)
    }

    pub fn with_env_reference(mut self, name: impl Into<String>) -> Result<Self, ValidationError> {
        if self.env.len() >= MAX_ENV_REFERENCE_COUNT {
            return Err(ValidationError::TooMany {
                field: ValidationField::EnvReference,
                limit: MAX_ENV_REFERENCE_COUNT,
            });
        }
        self.env.push(EnvReference::new(name)?);
        Ok(self)
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
        #[serde(deserialize_with = "deserialize_health_path")]
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
    #[serde(deserialize_with = "deserialize_dependencies")]
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
            dependencies: canonical_dependencies(wire.dependencies),
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
        let dependencies = canonical_dependencies(self.dependencies.clone());
        ServiceDefinitionWire {
            id: self.id.clone(),
            scope: self.scope.clone(),
            command: self.command.clone(),
            dependencies,
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

#[derive(Clone, Eq, PartialEq)]
pub struct LaunchIntent {
    service_id: ServiceId,
    scope: ServiceScope,
    command: CommandSpec,
    dependencies: Vec<ServiceId>,
    expected_port: Option<ExpectedPort>,
}

impl fmt::Debug for LaunchIntent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LaunchIntent")
            .field("service_id", &self.service_id)
            .field("scope", &self.scope)
            .field("command", &self.command)
            .field("dependencies", &self.dependencies)
            .field("expected_port", &self.expected_port)
            .finish()
    }
}

impl LaunchIntent {
    pub fn validate(&self) -> Result<(), ValidationError> {
        validate_identifier(
            self.service_id.as_str(),
            ValidationField::ServiceId,
            MAX_SERVICE_ID_LENGTH,
        )?;
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

    pub fn service_id(&self) -> &ServiceId {
        &self.service_id
    }

    pub fn scope(&self) -> &ServiceScope {
        &self.scope
    }

    pub fn command(&self) -> &CommandSpec {
        &self.command
    }

    pub fn dependencies(&self) -> &[ServiceId] {
        &self.dependencies
    }

    pub fn expected_port(&self) -> Option<ExpectedPort> {
        self.expected_port
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServiceCatalog {
    services: BTreeMap<ServiceId, ServiceDefinition>,
}

/// Errors returned by the bounded service-catalog JSON boundary.
///
/// The variants intentionally do not carry parser text or input fragments:
/// catalog bytes can contain command arguments and must never be copied into
/// diagnostics while rejecting malformed or hostile input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServiceCatalogDecodeError {
    FrameTooLarge,
    JsonLimitExceeded,
    MalformedJson,
    InvalidCatalog,
}

impl fmt::Display for ServiceCatalogDecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::FrameTooLarge => "service catalog frame exceeds the maximum size",
            Self::JsonLimitExceeded => "service catalog JSON exceeds a bounded limit",
            Self::MalformedJson => "service catalog JSON is malformed",
            Self::InvalidCatalog => "service catalog is invalid",
        })
    }
}

impl std::error::Error for ServiceCatalogDecodeError {}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ServiceCatalogWire {
    schema_version: u16,
    #[serde(deserialize_with = "deserialize_service_definitions")]
    services: Vec<ServiceDefinition>,
}

impl Serialize for ServiceCatalog {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        ServiceCatalogWire {
            schema_version: SERVICE_CATALOG_SCHEMA_VERSION,
            services: self.services.values().cloned().collect(),
        }
        .serialize(serializer)
    }
}

#[allow(dead_code)]
impl ServiceCatalog {
    /// Decode one untrusted JSON service-catalog frame.
    ///
    /// The raw frame and its JSON containers are bounded before serde parses
    /// any strings into owned values. This is important for escaped strings:
    /// serde_json must otherwise materialize the unescaped scratch string
    /// before a field-level validator can reject it.
    pub fn decode_json(bytes: &[u8]) -> Result<Self, ServiceCatalogDecodeError> {
        if bytes.len() > MAX_SERVICE_CATALOG_FRAME_BYTES {
            return Err(ServiceCatalogDecodeError::FrameTooLarge);
        }
        preflight_service_catalog_json(bytes)?;
        let wire: ServiceCatalogWire =
            serde_json::from_slice(bytes).map_err(|_| ServiceCatalogDecodeError::InvalidCatalog)?;
        if wire.schema_version != SERVICE_CATALOG_SCHEMA_VERSION {
            return Err(ServiceCatalogDecodeError::InvalidCatalog);
        }
        Self::new(wire.services).map_err(|_| ServiceCatalogDecodeError::InvalidCatalog)
    }

    pub fn new(definitions: Vec<ServiceDefinition>) -> Result<Self, ValidationError> {
        if definitions.len() > MAX_SERVICE_COUNT {
            return Err(ValidationError::TooMany {
                field: ValidationField::Service,
                limit: MAX_SERVICE_COUNT,
            });
        }
        let mut services = BTreeMap::new();
        for mut definition in definitions {
            definition.dependencies = canonical_dependencies(definition.dependencies);
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

    pub fn definitions(&self) -> impl Iterator<Item = &ServiceDefinition> {
        self.services.values()
    }

    /// Hash the exact versioned canonical wire representation.
    pub fn fingerprint(&self) -> [u8; 32] {
        let encoded = serde_json::to_vec(self).expect("validated service catalog must serialize");
        Sha256::digest(encoded).into()
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

    pub(crate) fn admit(
        &self,
        request: AdmissionRequest,
        snapshot: &AdmissionSnapshot,
    ) -> AdmissionDecision {
        let Some(definition) = self.services.get(&request.service_id) else {
            return AdmissionDecision::Refused(AdmissionRejection::UnknownService {
                service: request.service_id,
            });
        };
        if !requester_matches_scope(&request.requester, &definition.scope) {
            return AdmissionDecision::Refused(AdmissionRejection::RequesterMismatch {
                service: request.service_id,
            });
        }
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

        let requester = request.requester;
        match request.action {
            ServiceAction::Start => {
                self.admit_start(request.service_id, requester, snapshot, false)
            }
            ServiceAction::Stop => self.admit_stop(request.service_id, requester, snapshot),
            ServiceAction::Restart => self.admit_restart(request.service_id, requester, snapshot),
        }
    }

    pub(crate) fn admit_task_close(
        &self,
        task_id: TaskId,
        epoch: ActionEpoch,
        snapshot: &AdmissionSnapshot,
    ) -> Result<TaskClosePlan, AdmissionRejection> {
        if !snapshot.closing_tasks.contains(&task_id) {
            return Err(AdmissionRejection::TaskCloseNotAdmitted { task_id });
        }
        let current_epoch = snapshot.task_epochs.get(&task_id).copied().ok_or_else(|| {
            AdmissionRejection::TaskEpochStale {
                task_id,
                expected: None,
                received: epoch,
            }
        })?;
        if current_epoch != epoch {
            return Err(AdmissionRejection::TaskEpochStale {
                task_id,
                expected: Some(current_epoch),
                received: epoch,
            });
        }
        validate_task_close_snapshot_resources(self, task_id, snapshot)?;

        let mut selected = BTreeSet::new();
        for definition in self.services.values() {
            if definition.scope.task_id() != Some(&task_id) {
                continue;
            }
            let Some(runtime) = snapshot.services.get(&definition.id) else {
                return Err(AdmissionRejection::EvidenceUnknown {
                    service: definition.id.clone(),
                });
            };
            if matches!(runtime.state, ServiceState::External)
                || matches!(runtime.ownership, RuntimeOwnership::External)
            {
                return Err(AdmissionRejection::ExternalNotControllable {
                    service: definition.id.clone(),
                });
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
            if !matches!(runtime.state, ServiceState::Stopped)
                && runtime.ownership != (RuntimeOwnership::Task { task_id })
            {
                return Err(AdmissionRejection::OwnershipMismatch {
                    service: definition.id.clone(),
                    expected: RuntimeOwnership::Task { task_id },
                    received: runtime.ownership.clone(),
                });
            }
            let stopped_ownership_is_compatible =
                matches!(&runtime.ownership, RuntimeOwnership::None);
            if matches!(runtime.state, ServiceState::Stopped) && !stopped_ownership_is_compatible {
                return Err(AdmissionRejection::OwnershipMismatch {
                    service: definition.id.clone(),
                    expected: RuntimeOwnership::Task { task_id },
                    received: runtime.ownership.clone(),
                });
            }
            selected.insert(definition.id.clone());
        }
        let ordered = self
            .reverse_selected_order(&selected)
            .into_iter()
            .map(|service_id| {
                let definition = self
                    .services
                    .get(&service_id)
                    .expect("selected service is validated in catalog");
                let runtime = snapshot
                    .services
                    .get(&service_id)
                    .expect("task-close evidence was checked above");
                ServicePlanItem {
                    service_id: service_id.clone(),
                    fence: ServiceFence::capture(&service_id, runtime),
                    expected_state: runtime.state,
                    scope: definition.scope.clone(),
                }
            })
            .collect();
        Ok(TaskClosePlan {
            task_id,
            epoch,
            ordered,
        })
    }

    fn admit_start(
        &self,
        root: ServiceId,
        requester: AdmissionRequester,
        snapshot: &AdmissionSnapshot,
        force_root: bool,
    ) -> AdmissionDecision {
        let definition = self
            .services
            .get(&root)
            .expect("root definition checked by admit");
        let runtime = snapshot.services.get(&root).expect("root checked by admit");
        if let Err(rejection) = validate_ownership(
            &root,
            definition,
            runtime,
            ServiceAction::Start,
            &requester,
            true,
        ) {
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
            if !requester_matches_scope(&requester, &definition.scope) {
                return AdmissionDecision::Refused(AdmissionRejection::RequesterMismatch {
                    service: service_id,
                });
            }
            match dependency_runtime.state {
                ServiceState::Healthy if !is_root => {
                    if let Err(rejection) = validate_ownership(
                        &service_id,
                        definition,
                        dependency_runtime,
                        ServiceAction::Start,
                        &requester,
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
                        &requester,
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
                        &requester,
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
                            &requester,
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
        AdmissionDecision::Start(StartPlan {
            root,
            requester,
            ordered,
        })
    }

    fn admit_stop(
        &self,
        root: ServiceId,
        requester: AdmissionRequester,
        snapshot: &AdmissionSnapshot,
    ) -> AdmissionDecision {
        let definition = self
            .services
            .get(&root)
            .expect("root definition checked by admit");
        let runtime = snapshot.services.get(&root).expect("root checked by admit");
        if let Err(rejection) = validate_ownership(
            &root,
            definition,
            runtime,
            ServiceAction::Stop,
            &requester,
            false,
        ) {
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
                &requester,
                false,
            ) {
                return AdmissionDecision::Refused(rejection);
            }
            if !requester_matches_scope(&requester, &definition.scope) {
                return AdmissionDecision::Refused(AdmissionRejection::RequesterMismatch {
                    service: service_id,
                });
            }
            ordered.push(ServicePlanItem {
                service_id: service_id.clone(),
                fence: ServiceFence::capture(&service_id, dependent),
                expected_state: dependent.state.clone(),
                scope: definition.scope.clone(),
            });
        }
        AdmissionDecision::Stop(StopPlan {
            root,
            requester,
            ordered,
        })
    }

    fn admit_restart(
        &self,
        root: ServiceId,
        requester: AdmissionRequester,
        snapshot: &AdmissionSnapshot,
    ) -> AdmissionDecision {
        let Some(runtime) = snapshot.services.get(&root) else {
            return AdmissionDecision::Refused(AdmissionRejection::EvidenceUnknown {
                service: root,
            });
        };
        let definition = self
            .services
            .get(&root)
            .expect("root definition checked by admit");
        if let Err(rejection) = validate_ownership(
            &root,
            definition,
            runtime,
            ServiceAction::Restart,
            &requester,
            false,
        ) {
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
        let stop = match self.admit_stop(root.clone(), requester.clone(), snapshot) {
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
        match self.admit_start(root.clone(), requester, snapshot, true) {
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
    ordered: Vec<ServiceId>,
}

impl DependencyPlan {
    pub fn services(&self) -> &[ServiceId] {
        &self.ordered
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PlanningError {
    UnknownService(ServiceId),
    Cycle(Vec<ServiceId>),
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct ResourceGeneration(u64);

#[allow(dead_code)]
impl ResourceGeneration {
    pub(crate) const fn new(value: u64) -> Self {
        Self(value)
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct ConnectionEpoch(u64);

#[allow(dead_code)]
impl ConnectionEpoch {
    pub(crate) const fn new(value: u64) -> Self {
        Self(value)
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct ActionEpoch(u64);

#[allow(dead_code)]
impl ActionEpoch {
    pub(crate) const fn new(value: u64) -> Self {
        Self(value)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AdmissionFence {
    resource_generation: ResourceGeneration,
    connection_epoch: ConnectionEpoch,
    action_epoch: ActionEpoch,
}

#[allow(dead_code)]
impl AdmissionFence {
    pub(crate) const fn new(
        resource_generation: u64,
        connection_epoch: u64,
        action_epoch: u64,
    ) -> Self {
        Self {
            resource_generation: ResourceGeneration::new(resource_generation),
            connection_epoch: ConnectionEpoch::new(connection_epoch),
            action_epoch: ActionEpoch::new(action_epoch),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct HostId(u64);

#[allow(dead_code)]
impl HostId {
    pub(crate) const fn new(value: u64) -> Self {
        Self(value)
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
#[allow(dead_code)]
struct HostAuthority {
    host_id: HostId,
}

#[allow(dead_code)]
impl HostAuthority {
    pub(crate) const fn new(host_id: HostId) -> Self {
        Self { host_id }
    }
}

impl fmt::Debug for HostAuthority {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("HostAuthority(<opaque>)")
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(dead_code)]
enum AdmissionRequester {
    Task(TaskId),
    Host(HostAuthority),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AdmissionRequest {
    action: ServiceAction,
    service_id: ServiceId,
    fence: AdmissionFence,
    requester: AdmissionRequester,
}

#[allow(dead_code)]
impl AdmissionRequest {
    pub(crate) fn for_task(
        action: ServiceAction,
        service_id: ServiceId,
        fence: AdmissionFence,
        task_id: TaskId,
    ) -> Self {
        Self {
            action,
            service_id,
            fence,
            requester: AdmissionRequester::Task(task_id),
        }
    }

    pub(crate) fn for_host(
        action: ServiceAction,
        service_id: ServiceId,
        fence: AdmissionFence,
        host_id: HostId,
    ) -> Self {
        Self {
            action,
            service_id,
            fence,
            requester: AdmissionRequester::Host(HostAuthority::new(host_id)),
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
#[allow(dead_code)]
pub(crate) enum RuntimeOwnership {
    None,
    Task { task_id: TaskId },
    Host { host_id: HostId },
    External,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ActiveOperation {
    pub(crate) id: u64,
    pub(crate) action: ServiceAction,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RuntimeRecord {
    pub(crate) state: ServiceState,
    pub(crate) fence: AdmissionFence,
    pub(crate) ownership: RuntimeOwnership,
    pub(crate) operation: Option<ActiveOperation>,
}

/// Immutable compare-and-swap token captured for one service-plan member.
///
/// The complete resource/connection/action fence is not sufficient by itself:
/// ownership is part of the authority being revalidated and must change the
/// token too.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ServiceFence {
    service_id: ServiceId,
    resource_generation: ResourceGeneration,
    connection_epoch: ConnectionEpoch,
    action_epoch: ActionEpoch,
    ownership: RuntimeOwnership,
}

#[allow(dead_code)]
impl ServiceFence {
    fn capture(service_id: &ServiceId, runtime: &RuntimeRecord) -> Self {
        Self {
            service_id: service_id.clone(),
            resource_generation: runtime.fence.resource_generation,
            connection_epoch: runtime.fence.connection_epoch,
            action_epoch: runtime.fence.action_epoch,
            ownership: runtime.ownership.clone(),
        }
    }

    pub fn service_id(&self) -> &ServiceId {
        &self.service_id
    }

    pub(crate) fn resource_generation(&self) -> ResourceGeneration {
        self.resource_generation
    }

    pub(crate) fn connection_epoch(&self) -> ConnectionEpoch {
        self.connection_epoch
    }

    pub(crate) fn action_epoch(&self) -> ActionEpoch {
        self.action_epoch
    }

    pub(crate) fn ownership(&self) -> &RuntimeOwnership {
        &self.ownership
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct AdmissionSnapshot {
    services: BTreeMap<ServiceId, RuntimeRecord>,
    task_epochs: BTreeMap<TaskId, ActionEpoch>,
    closing_tasks: BTreeSet<TaskId>,
}

#[allow(dead_code)]
impl AdmissionSnapshot {
    pub(crate) fn set_service(&mut self, id: ServiceId, runtime: RuntimeRecord) {
        self.services.insert(id, runtime);
    }

    pub(crate) fn set_task_epoch(&mut self, task_id: TaskId, epoch: ActionEpoch) {
        self.task_epochs.insert(task_id, epoch);
    }

    pub(crate) fn mark_task_closing(&mut self, task_id: TaskId) {
        self.closing_tasks.insert(task_id);
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(dead_code)]
pub(crate) enum AdmissionDecision {
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
#[allow(dead_code)]
pub(crate) enum AdmissionRejection {
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
    RequesterMismatch {
        service: ServiceId,
    },
    TaskCloseNotAdmitted {
        task_id: TaskId,
    },
    TaskEpochStale {
        task_id: TaskId,
        expected: Option<ActionEpoch>,
        received: ActionEpoch,
    },
    TaskOwnedResourceNotInCatalog {
        service: ServiceId,
        task_id: TaskId,
    },
    TaskOwnedResourceOutsideScope {
        service: ServiceId,
        task_id: TaskId,
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
pub(crate) struct StartPlan {
    root: ServiceId,
    requester: AdmissionRequester,
    ordered: Vec<StartPlanItem>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StopPlan {
    root: ServiceId,
    requester: AdmissionRequester,
    ordered: Vec<StopPlanItem>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RestartPlan {
    stop: StopPlan,
    start: StartPlan,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TaskClosePlan {
    task_id: TaskId,
    epoch: ActionEpoch,
    ordered: Vec<ClosePlanItem>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StartPlanItem {
    service_id: ServiceId,
    intent: Option<LaunchIntent>,
    fence: ServiceFence,
    expected_state: ServiceState,
    scope: ServiceScope,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ServicePlanItem {
    service_id: ServiceId,
    fence: ServiceFence,
    expected_state: ServiceState,
    scope: ServiceScope,
}

pub(crate) type StopPlanItem = ServicePlanItem;
pub(crate) type ClosePlanItem = ServicePlanItem;

#[allow(dead_code)]
impl StartPlanItem {
    pub(crate) fn service_id(&self) -> &ServiceId {
        &self.service_id
    }

    pub(crate) fn intent(&self) -> Option<&LaunchIntent> {
        self.intent.as_ref()
    }

    pub(crate) fn fence(&self) -> &ServiceFence {
        &self.fence
    }

    pub(crate) fn expected_state(&self) -> &ServiceState {
        &self.expected_state
    }

    pub(crate) fn scope(&self) -> &ServiceScope {
        &self.scope
    }
}

#[allow(dead_code)]
impl ServicePlanItem {
    pub(crate) fn service_id(&self) -> &ServiceId {
        &self.service_id
    }

    pub(crate) fn fence(&self) -> &ServiceFence {
        &self.fence
    }

    pub(crate) fn expected_state(&self) -> &ServiceState {
        &self.expected_state
    }

    pub(crate) fn scope(&self) -> &ServiceScope {
        &self.scope
    }
}

#[allow(dead_code)]
impl StartPlan {
    pub(crate) fn root(&self) -> &ServiceId {
        &self.root
    }

    pub(crate) fn members(&self) -> &[StartPlanItem] {
        &self.ordered
    }

    /// Revalidate every member against one immutable snapshot before effects.
    pub(crate) fn revalidate(
        &self,
        catalog: &ServiceCatalog,
        snapshot: &AdmissionSnapshot,
    ) -> Result<(), AdmissionRejection> {
        validate_start_plan(self, catalog)?;
        for item in &self.ordered {
            revalidate_start_item(item, snapshot)?;
        }
        Ok(())
    }
}

#[allow(dead_code)]
impl StopPlan {
    pub(crate) fn root(&self) -> &ServiceId {
        &self.root
    }

    pub(crate) fn members(&self) -> &[StopPlanItem] {
        &self.ordered
    }

    /// Revalidate every member against one immutable snapshot before effects.
    pub(crate) fn revalidate(
        &self,
        catalog: &ServiceCatalog,
        snapshot: &AdmissionSnapshot,
    ) -> Result<(), AdmissionRejection> {
        validate_stop_plan(self, catalog, snapshot)?;
        for item in &self.ordered {
            revalidate_service_item(item, snapshot)?;
        }
        Ok(())
    }
}

#[allow(dead_code)]
impl RestartPlan {
    pub(crate) fn stop(&self) -> &StopPlan {
        &self.stop
    }

    pub(crate) fn start(&self) -> &StartPlan {
        &self.start
    }

    /// Revalidate the complete stop/start decision against one snapshot.
    pub(crate) fn revalidate(
        &self,
        catalog: &ServiceCatalog,
        snapshot: &AdmissionSnapshot,
    ) -> Result<(), AdmissionRejection> {
        self.stop.revalidate(catalog, snapshot)?;
        self.start.revalidate(catalog, snapshot)
    }
}

#[allow(dead_code)]
impl TaskClosePlan {
    pub(crate) fn task_id(&self) -> TaskId {
        self.task_id
    }

    pub(crate) fn epoch(&self) -> ActionEpoch {
        self.epoch
    }

    pub(crate) fn members(&self) -> &[ClosePlanItem] {
        &self.ordered
    }

    /// Revalidate the close barrier, epoch, and every owned member atomically.
    pub(crate) fn revalidate(
        &self,
        catalog: &ServiceCatalog,
        snapshot: &AdmissionSnapshot,
    ) -> Result<(), AdmissionRejection> {
        validate_close_plan(self, catalog)?;
        if !snapshot.closing_tasks.contains(&self.task_id) {
            return Err(AdmissionRejection::TaskCloseNotAdmitted {
                task_id: self.task_id,
            });
        }
        if snapshot.task_epochs.get(&self.task_id).copied() != Some(self.epoch) {
            return Err(AdmissionRejection::TaskEpochStale {
                task_id: self.task_id,
                expected: snapshot.task_epochs.get(&self.task_id).copied(),
                received: self.epoch,
            });
        }
        validate_task_close_snapshot_resources(catalog, self.task_id, snapshot)?;
        for item in &self.ordered {
            if item.scope
                != (ServiceScope::Task {
                    task_id: self.task_id,
                })
            {
                return Err(AdmissionRejection::PlanStale {
                    service: item.service_id.clone(),
                });
            }
            let stopped_ownership_is_compatible =
                matches!(&item.fence.ownership, RuntimeOwnership::None);
            let valid_stopped_ownership = matches!(item.expected_state, ServiceState::Stopped)
                && stopped_ownership_is_compatible;
            if item.fence.ownership
                != (RuntimeOwnership::Task {
                    task_id: self.task_id,
                })
                && !valid_stopped_ownership
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
    validate_executable(command.program.as_str())?;
    if command.args.len() > MAX_ARGUMENT_COUNT {
        return Err(ValidationError::TooMany {
            field: ValidationField::Argument,
            limit: MAX_ARGUMENT_COUNT,
        });
    }
    validate_command_arguments(&command.args)?;
    if let Some(cwd) = &command.cwd {
        WorkspacePath::new(cwd.as_str().to_owned())?;
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

fn validate_command_arguments(arguments: &[CommandArgument]) -> Result<(), ValidationError> {
    for (index, argument) in arguments.iter().enumerate() {
        CommandArgument::new(argument.as_str().to_owned())?;
        let inline_secret_name = argument
            .as_str()
            .split_once('=')
            .filter(|(option, _)| is_secret_option(option))
            .map(|(_, assigned)| assigned)
            .filter(|assigned| is_secret_name(assigned));
        let split_secret_name = argument.as_str().split_once('=').is_none()
            && is_secret_option(argument.as_str())
            && arguments
                .get(index + 1)
                .is_some_and(|next| is_secret_name(next.as_str()))
            && arguments.get(index + 2).is_some();
        if (inline_secret_name.is_some() && arguments.get(index + 1).is_some()) || split_secret_name
        {
            return Err(ValidationError::RawSecret {
                field: ValidationField::Argument,
            });
        }
    }
    Ok(())
}

fn validate_executable(value: &str) -> Result<(), ValidationError> {
    validate_text(value, ValidationField::Program, MAX_PROGRAM_LENGTH)?;
    if value.contains('/')
        || value.contains('\\')
        || value.contains('=')
        || value.chars().any(char::is_whitespace)
    {
        return Err(ValidationError::UnsafePath {
            field: ValidationField::Program,
        });
    }
    Ok(())
}

fn validate_env_name(value: &str) -> Result<(), ValidationError> {
    validate_text(
        value,
        ValidationField::EnvReference,
        MAX_ENV_REFERENCE_LENGTH,
    )?;
    if value.contains('=') || is_secret_assignment(value) {
        return Err(ValidationError::RawSecret {
            field: ValidationField::EnvReference,
        });
    }
    let mut characters = value.chars();
    let Some(first) = characters.next() else {
        return Err(ValidationError::Empty {
            field: ValidationField::EnvReference,
        });
    };
    if !(first == '_' || first.is_ascii_alphabetic())
        || !characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
    {
        return Err(ValidationError::InvalidIdentifier {
            field: ValidationField::EnvReference,
        });
    }
    Ok(())
}

fn preflight_service_catalog_json(bytes: &[u8]) -> Result<(), ServiceCatalogDecodeError> {
    let mut scanner = JsonPreflight { bytes, position: 0 };
    scanner.skip_whitespace();
    scanner.parse_value(0)?;
    scanner.skip_whitespace();
    if scanner.position != bytes.len() {
        return Err(ServiceCatalogDecodeError::MalformedJson);
    }
    Ok(())
}

struct JsonPreflight<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> JsonPreflight<'a> {
    fn parse_value(&mut self, depth: usize) -> Result<(), ServiceCatalogDecodeError> {
        if depth > MAX_SERVICE_CATALOG_JSON_DEPTH {
            return Err(ServiceCatalogDecodeError::JsonLimitExceeded);
        }
        match self.peek() {
            Some(b'"') => self.parse_string(false),
            Some(b'{') => self.parse_object(depth),
            Some(b'[') => self.parse_array(depth),
            Some(b't') => self.parse_literal(b"true"),
            Some(b'f') => self.parse_literal(b"false"),
            Some(b'n') => self.parse_literal(b"null"),
            Some(b'-' | b'0'..=b'9') => self.parse_number(),
            _ => Err(ServiceCatalogDecodeError::MalformedJson),
        }
    }

    fn parse_object(&mut self, depth: usize) -> Result<(), ServiceCatalogDecodeError> {
        self.expect_byte(b'{')?;
        self.skip_whitespace();
        if self.consume_byte(b'}') {
            return Ok(());
        }

        let mut fields = 0;
        loop {
            if fields >= MAX_SERVICE_CATALOG_JSON_OBJECT_FIELDS {
                return Err(ServiceCatalogDecodeError::JsonLimitExceeded);
            }
            self.parse_string(true)?;
            self.skip_whitespace();
            self.expect_byte(b':')?;
            self.skip_whitespace();
            self.parse_value(depth + 1)?;
            fields += 1;
            self.skip_whitespace();
            if self.consume_byte(b'}') {
                return Ok(());
            }
            self.expect_byte(b',')?;
            self.skip_whitespace();
        }
    }

    fn parse_array(&mut self, depth: usize) -> Result<(), ServiceCatalogDecodeError> {
        self.expect_byte(b'[')?;
        self.skip_whitespace();
        if self.consume_byte(b']') {
            return Ok(());
        }

        let mut items = 0;
        loop {
            if items >= MAX_SERVICE_CATALOG_JSON_ARRAY_ITEMS {
                return Err(ServiceCatalogDecodeError::JsonLimitExceeded);
            }
            self.parse_value(depth + 1)?;
            items += 1;
            self.skip_whitespace();
            if self.consume_byte(b']') {
                return Ok(());
            }
            self.expect_byte(b',')?;
            self.skip_whitespace();
        }
    }

    fn parse_string(&mut self, field_name: bool) -> Result<(), ServiceCatalogDecodeError> {
        self.expect_byte(b'"')?;
        let limit = if field_name {
            MAX_SERVICE_CATALOG_JSON_FIELD_NAME_BYTES
        } else {
            MAX_SERVICE_CATALOG_JSON_STRING_BYTES
        };
        let mut raw_length = 0usize;
        loop {
            let Some(byte) = self.next_byte() else {
                return Err(ServiceCatalogDecodeError::MalformedJson);
            };
            match byte {
                b'"' => return Ok(()),
                b'\\' => {
                    raw_length = raw_length.saturating_add(2);
                    let Some(escape) = self.next_byte() else {
                        return Err(ServiceCatalogDecodeError::MalformedJson);
                    };
                    if escape == b'u' {
                        raw_length = raw_length.saturating_add(4);
                        for _ in 0..4 {
                            let Some(hex) = self.next_byte() else {
                                return Err(ServiceCatalogDecodeError::MalformedJson);
                            };
                            if !hex.is_ascii_hexdigit() {
                                return Err(ServiceCatalogDecodeError::MalformedJson);
                            }
                        }
                    } else if !matches!(
                        escape,
                        b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't'
                    ) {
                        return Err(ServiceCatalogDecodeError::MalformedJson);
                    }
                }
                byte if byte < 0x20 => {
                    return Err(ServiceCatalogDecodeError::MalformedJson);
                }
                _ => raw_length = raw_length.saturating_add(1),
            }
            if raw_length > limit {
                return Err(ServiceCatalogDecodeError::JsonLimitExceeded);
            }
        }
    }

    fn parse_literal(&mut self, literal: &[u8]) -> Result<(), ServiceCatalogDecodeError> {
        if self.bytes.get(self.position..self.position + literal.len()) == Some(literal) {
            self.position += literal.len();
            Ok(())
        } else {
            Err(ServiceCatalogDecodeError::MalformedJson)
        }
    }

    fn parse_number(&mut self) -> Result<(), ServiceCatalogDecodeError> {
        let start = self.position;
        while matches!(
            self.peek(),
            Some(b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9')
        ) {
            self.position += 1;
        }
        if self.position == start {
            Err(ServiceCatalogDecodeError::MalformedJson)
        } else {
            Ok(())
        }
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.position += 1;
        }
    }

    fn expect_byte(&mut self, expected: u8) -> Result<(), ServiceCatalogDecodeError> {
        if self.consume_byte(expected) {
            Ok(())
        } else {
            Err(ServiceCatalogDecodeError::MalformedJson)
        }
    }

    fn consume_byte(&mut self, expected: u8) -> bool {
        if self.peek() == Some(expected) {
            self.position += 1;
            true
        } else {
            false
        }
    }

    fn next_byte(&mut self) -> Option<u8> {
        let byte = self.bytes.get(self.position).copied()?;
        self.position += 1;
        Some(byte)
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.position).copied()
    }
}

fn deserialize_bounded_string<'de, D, const MAX: usize>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserializer.deserialize_string(BoundedStringVisitor::<MAX>)
}

struct BoundedStringVisitor<const MAX: usize>;

impl<'de, const MAX: usize> Visitor<'de> for BoundedStringVisitor<MAX> {
    type Value = String;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "a string of at most {MAX} bytes")
    }

    fn visit_borrowed_str<E>(self, value: &'de str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        self.visit_str(value)
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        if value.len() > MAX {
            return Err(E::custom(format!("string exceeds {MAX} bytes")));
        }
        Ok(value.to_owned())
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        if value.len() > MAX {
            return Err(E::custom(format!("string exceeds {MAX} bytes")));
        }
        Ok(value)
    }
}

fn deserialize_bounded_vec<'de, D, T, const MAX: usize>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    deserializer.deserialize_seq(BoundedVecVisitor::<T, MAX>(PhantomData))
}

struct BoundedVecVisitor<T, const MAX: usize>(PhantomData<T>);

impl<'de, T, const MAX: usize> Visitor<'de> for BoundedVecVisitor<T, MAX>
where
    T: Deserialize<'de>,
{
    type Value = Vec<T>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "a sequence with at most {MAX} items")
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while values.len() < MAX {
            let Some(value) = sequence.next_element()? else {
                return Ok(values);
            };
            values.push(value);
        }
        if sequence.next_element::<IgnoredAny>()?.is_some() {
            return Err(A::Error::custom(format!("sequence exceeds {MAX} items")));
        }
        Ok(values)
    }
}

fn deserialize_service_id_string<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_bounded_string::<D, MAX_SERVICE_ID_LENGTH>(deserializer)
}

fn deserialize_task_id_string<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_bounded_string::<D, MAX_TASK_ID_LENGTH>(deserializer)
}

fn deserialize_env_name<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_bounded_string::<D, MAX_ENV_REFERENCE_LENGTH>(deserializer)
}

fn deserialize_health_path<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_bounded_string::<D, MAX_HEALTH_PATH_LENGTH>(deserializer)
}

fn deserialize_command_arguments<'de, D>(deserializer: D) -> Result<Vec<CommandArgument>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_bounded_vec::<D, CommandArgument, MAX_ARGUMENT_COUNT>(deserializer)
}

fn deserialize_env_references<'de, D>(deserializer: D) -> Result<Vec<EnvReference>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_bounded_vec::<D, EnvReference, MAX_ENV_REFERENCE_COUNT>(deserializer)
}

fn deserialize_dependencies<'de, D>(deserializer: D) -> Result<Vec<ServiceId>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_bounded_vec::<D, ServiceId, MAX_DEPENDENCY_COUNT>(deserializer)
}

fn deserialize_service_definitions<'de, D>(
    deserializer: D,
) -> Result<Vec<ServiceDefinition>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_bounded_vec::<D, ServiceDefinition, MAX_SERVICE_COUNT>(deserializer)
}

fn canonical_dependencies(mut dependencies: Vec<ServiceId>) -> Vec<ServiceId> {
    dependencies.sort();
    dependencies
}

fn validate_env_reference(reference: &EnvReference) -> Result<(), ValidationError> {
    validate_env_name(&reference.name)
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
    (option.starts_with("--") && is_secret_name(option))
        || is_secret_assignment(value)
        || value.split_once('=').is_some_and(|(name, assigned)| {
            is_secret_option(name)
                && assigned
                    .split_once('=')
                    .is_some_and(|(name, _)| is_secret_name(name))
        })
}

fn is_secret_assignment(value: &str) -> bool {
    if value.starts_with("-----BEGIN ") {
        return true;
    }
    let Some((name, _)) = value.split_once('=') else {
        return false;
    };
    is_secret_name(name)
}

fn is_secret_name(name: &str) -> bool {
    matches!(
        normalize_secret_name(name).as_str(),
        "token"
            | "api_token"
            | "access_token"
            | "api_key"
            | "access_key"
            | "secret"
            | "client_secret"
            | "password"
            | "private_key"
    )
}

fn is_secret_option(value: &str) -> bool {
    let option = value.split_once('=').map_or(value, |(name, _)| name);
    matches!(
        normalize_secret_name(option).as_str(),
        "env" | "set_env" | "env_var" | "set_env_var"
    )
}

fn normalize_secret_name(value: &str) -> String {
    value
        .trim_start_matches('-')
        .to_ascii_lowercase()
        .replace('-', "_")
}

#[allow(dead_code)]
fn validate_ownership(
    service_id: &ServiceId,
    definition: &ServiceDefinition,
    runtime: &RuntimeRecord,
    action: ServiceAction,
    requester: &AdmissionRequester,
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

    let expected = match (&definition.scope, requester) {
        (ServiceScope::Task { task_id }, AdmissionRequester::Task(requester_task))
            if task_id == requester_task =>
        {
            RuntimeOwnership::Task { task_id: *task_id }
        }
        (ServiceScope::Host, AdmissionRequester::Host(authority)) => RuntimeOwnership::Host {
            host_id: authority.host_id,
        },
        _ => {
            return Err(AdmissionRejection::RequesterMismatch {
                service: service_id.clone(),
            })
        }
    };
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

fn requester_matches_scope(requester: &AdmissionRequester, scope: &ServiceScope) -> bool {
    match (requester, scope) {
        (AdmissionRequester::Task(requester), ServiceScope::Task { task_id }) => {
            requester == task_id
        }
        (AdmissionRequester::Host(_), ServiceScope::Host) => true,
        _ => false,
    }
}

fn validate_start_plan(
    plan: &StartPlan,
    catalog: &ServiceCatalog,
) -> Result<(), AdmissionRejection> {
    let Some(root_definition) = catalog.definition(&plan.root) else {
        return Err(AdmissionRejection::PlanStale {
            service: plan.root.clone(),
        });
    };
    if !requester_matches_scope(&plan.requester, &root_definition.scope) {
        return Err(AdmissionRejection::PlanStale {
            service: plan.root.clone(),
        });
    }
    let expected_order =
        catalog
            .dependency_plan(&plan.root)
            .map_err(|_| AdmissionRejection::PlanStale {
                service: plan.root.clone(),
            })?;
    if plan.ordered.len() != expected_order.services().len()
        || plan
            .ordered
            .iter()
            .map(|item| &item.service_id)
            .ne(expected_order.services().iter())
    {
        return Err(AdmissionRejection::PlanStale {
            service: plan.root.clone(),
        });
    }
    for item in &plan.ordered {
        let Some(definition) = catalog.definition(&item.service_id) else {
            return Err(AdmissionRejection::PlanStale {
                service: item.service_id.clone(),
            });
        };
        if item.fence.service_id != item.service_id
            || item.scope != definition.scope
            || !requester_matches_scope(&plan.requester, &definition.scope)
        {
            return Err(AdmissionRejection::PlanStale {
                service: item.service_id.clone(),
            });
        }
        let expected_intent =
            catalog
                .launch_intent(&item.service_id)
                .map_err(|_| AdmissionRejection::PlanStale {
                    service: item.service_id.clone(),
                })?;
        let requires_intent = item.service_id == plan.root
            || matches!(
                item.expected_state,
                ServiceState::Stopped | ServiceState::Failed
            );
        match (&item.intent, requires_intent) {
            (Some(intent), true) if intent == &expected_intent => {}
            (None, false) => {}
            _ => {
                return Err(AdmissionRejection::PlanStale {
                    service: item.service_id.clone(),
                })
            }
        }
    }
    Ok(())
}

fn validate_stop_plan(
    plan: &StopPlan,
    catalog: &ServiceCatalog,
    snapshot: &AdmissionSnapshot,
) -> Result<(), AdmissionRejection> {
    let Some(root_definition) = catalog.definition(&plan.root) else {
        return Err(AdmissionRejection::PlanStale {
            service: plan.root.clone(),
        });
    };
    if !requester_matches_scope(&plan.requester, &root_definition.scope) {
        return Err(AdmissionRejection::PlanStale {
            service: plan.root.clone(),
        });
    }
    let selected = catalog.stop_closure(&plan.root);
    for service_id in &selected {
        if !snapshot.services.contains_key(service_id) {
            return Err(AdmissionRejection::EvidenceUnknown {
                service: service_id.clone(),
            });
        }
    }
    let expected_order: Vec<_> = catalog
        .reverse_selected_order(&selected)
        .into_iter()
        .filter(|service_id| {
            snapshot
                .services
                .get(service_id)
                .is_some_and(|runtime| !matches!(runtime.state, ServiceState::Stopped))
        })
        .collect();
    if plan.ordered.len() != expected_order.len()
        || plan
            .ordered
            .iter()
            .map(|item| &item.service_id)
            .ne(expected_order.iter())
    {
        return Err(AdmissionRejection::PlanStale {
            service: plan.root.clone(),
        });
    }
    for item in &plan.ordered {
        let Some(definition) = catalog.definition(&item.service_id) else {
            return Err(AdmissionRejection::PlanStale {
                service: item.service_id.clone(),
            });
        };
        if item.fence.service_id != item.service_id
            || item.scope != definition.scope
            || !requester_matches_scope(&plan.requester, &definition.scope)
        {
            return Err(AdmissionRejection::PlanStale {
                service: item.service_id.clone(),
            });
        }
    }
    Ok(())
}

fn validate_close_plan(
    plan: &TaskClosePlan,
    catalog: &ServiceCatalog,
) -> Result<(), AdmissionRejection> {
    let selected: BTreeSet<_> = catalog
        .services
        .values()
        .filter(|definition| definition.scope.task_id() == Some(&plan.task_id))
        .map(|definition| definition.id.clone())
        .collect();
    let expected_order = catalog.reverse_selected_order(&selected);
    if plan.ordered.len() != expected_order.len()
        || plan
            .ordered
            .iter()
            .map(|item| &item.service_id)
            .ne(expected_order.iter())
    {
        let service = plan
            .ordered
            .first()
            .map(|item| item.service_id.clone())
            .or_else(|| expected_order.first().cloned())
            .unwrap_or_else(|| ServiceId::new("plan").expect("static service id"));
        return Err(AdmissionRejection::PlanStale { service });
    }
    for item in &plan.ordered {
        let Some(definition) = catalog.definition(&item.service_id) else {
            return Err(AdmissionRejection::PlanStale {
                service: item.service_id.clone(),
            });
        };
        if item.fence.service_id != item.service_id
            || item.scope != definition.scope
            || definition.scope.task_id() != Some(&plan.task_id)
        {
            return Err(AdmissionRejection::PlanStale {
                service: item.service_id.clone(),
            });
        }
    }
    Ok(())
}

fn validate_task_close_snapshot_resources(
    catalog: &ServiceCatalog,
    task_id: TaskId,
    snapshot: &AdmissionSnapshot,
) -> Result<(), AdmissionRejection> {
    for (service_id, runtime) in &snapshot.services {
        if runtime.ownership != (RuntimeOwnership::Task { task_id }) {
            continue;
        }
        let Some(definition) = catalog.definition(service_id) else {
            return Err(AdmissionRejection::TaskOwnedResourceNotInCatalog {
                service: service_id.clone(),
                task_id,
            });
        };
        if definition.scope.task_id() != Some(&task_id) {
            return Err(AdmissionRejection::TaskOwnedResourceOutsideScope {
                service: service_id.clone(),
                task_id,
            });
        }
    }
    Ok(())
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
    if item.fence.service_id != item.service_id
        || runtime.fence.resource_generation != item.fence.resource_generation
        || runtime.fence.connection_epoch != item.fence.connection_epoch
        || runtime.fence.action_epoch != item.fence.action_epoch
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

#[cfg(test)]
mod tests {
    use super::*;

    fn id(value: &str) -> ServiceId {
        ServiceId::new(value).expect("test service id")
    }

    fn task_a() -> TaskId {
        TaskId::parse("0198b6b0-0000-7000-8000-000000000001").expect("test task id")
    }

    fn task_b() -> TaskId {
        TaskId::parse("0198b6b0-0000-7000-8000-000000000002").expect("test task id")
    }

    fn host_a() -> HostId {
        HostId::new(1)
    }

    fn host_b() -> HostId {
        HostId::new(2)
    }

    fn command() -> CommandSpec {
        CommandSpec::new("node")
            .expect("valid executable")
            .with_arg("server.js")
            .expect("valid argument")
            .with_cwd("apps/api")
            .expect("valid workspace path")
            .with_env_reference("PORT")
            .expect("valid environment reference")
    }

    fn policy() -> HealthPolicy {
        HealthPolicy {
            startup_deadline_ms: 5_000,
            probe_interval_ms: 1_000,
            max_probe_interval_ms: 4_000,
            backoff_multiplier: 2,
            success_threshold: 2,
            failure_threshold: 2,
            stale_after_ms: 2_500,
        }
    }

    fn service(
        name: &str,
        scope: ServiceScope,
        dependencies: Vec<ServiceId>,
        port: u16,
    ) -> ServiceDefinition {
        ServiceDefinition {
            id: id(name),
            scope,
            command: command(),
            dependencies,
            health: HealthSpec::Tcp {
                port,
                policy: policy(),
            },
            startup: StartupPolicy::manual(),
            stop: StopPolicy::default(),
            expected_port: Some(ExpectedPort {
                protocol: PortProtocol::Tcp,
                port,
            }),
        }
    }

    fn record(
        state: ServiceState,
        fence: AdmissionFence,
        ownership: RuntimeOwnership,
        operation: Option<ActiveOperation>,
    ) -> RuntimeRecord {
        RuntimeRecord {
            state,
            fence,
            ownership,
            operation,
        }
    }

    fn request(
        action: ServiceAction,
        service_id: ServiceId,
        fence: AdmissionFence,
        task_id: TaskId,
    ) -> AdmissionRequest {
        AdmissionRequest::for_task(action, service_id, fence, task_id)
    }

    #[test]
    fn admission_requires_exact_requester_scope_and_host_capability() {
        let catalog = ServiceCatalog::new(vec![
            service("task-api", ServiceScope::task(task_a()), vec![], 8080),
            service("host-db", ServiceScope::Host, vec![], 5432),
        ])
        .unwrap();
        let fence = AdmissionFence::new(4, 3, 9);
        let mut snapshot = AdmissionSnapshot::default();
        snapshot.set_service(
            id("task-api"),
            record(ServiceState::Stopped, fence, RuntimeOwnership::None, None),
        );
        snapshot.set_service(
            id("host-db"),
            record(ServiceState::Stopped, fence, RuntimeOwnership::None, None),
        );

        assert!(matches!(
            catalog.admit(request(ServiceAction::Start, id("task-api"), fence, task_b()), &snapshot),
            AdmissionDecision::Refused(AdmissionRejection::RequesterMismatch { service })
                if service == id("task-api")
        ));
        assert!(matches!(
            catalog.admit(request(ServiceAction::Start, id("host-db"), fence, task_a()), &snapshot),
            AdmissionDecision::Refused(AdmissionRejection::RequesterMismatch { service })
                if service == id("host-db")
        ));
        assert!(matches!(
            catalog.admit(
                AdmissionRequest::for_host(ServiceAction::Start, id("host-db"), fence, host_a()),
                &snapshot,
            ),
            AdmissionDecision::Start(_)
        ));
    }

    #[test]
    fn host_capability_must_match_the_live_host_owner() {
        let catalog =
            ServiceCatalog::new(vec![service("host-db", ServiceScope::Host, vec![], 5432)])
                .unwrap();
        let fence = AdmissionFence::new(4, 3, 9);
        let mut snapshot = AdmissionSnapshot::default();
        snapshot.set_service(
            id("host-db"),
            record(
                ServiceState::Healthy,
                fence,
                RuntimeOwnership::Host { host_id: host_a() },
                None,
            ),
        );

        assert!(matches!(
            catalog.admit(
                AdmissionRequest::for_host(
                    ServiceAction::Stop,
                    id("host-db"),
                    fence,
                    host_b(),
                ),
                &snapshot,
            ),
            AdmissionDecision::Refused(AdmissionRejection::OwnershipMismatch {
                service,
                expected: RuntimeOwnership::Host { host_id },
                ..
            }) if service == id("host-db") && host_id == host_b()
        ));
    }

    #[test]
    fn admission_orders_dependencies_coalesces_duplicate_start_and_blocks_failures() {
        let catalog = ServiceCatalog::new(vec![
            service("api", ServiceScope::task(task_a()), vec![id("db")], 8080),
            service("db", ServiceScope::task(task_a()), vec![], 5432),
        ])
        .unwrap();
        let fence = AdmissionFence::new(4, 3, 9);
        let mut snapshot = AdmissionSnapshot::default();
        snapshot.set_service(
            id("api"),
            record(ServiceState::Stopped, fence, RuntimeOwnership::None, None),
        );
        snapshot.set_service(
            id("db"),
            record(ServiceState::Stopped, fence, RuntimeOwnership::None, None),
        );

        let AdmissionDecision::Start(plan) = catalog.admit(
            request(ServiceAction::Start, id("api"), fence, task_a()),
            &snapshot,
        ) else {
            panic!("expected start plan");
        };
        assert_eq!(
            plan.members()
                .iter()
                .map(|item| item.service_id().clone())
                .collect::<Vec<_>>(),
            vec![id("db"), id("api")]
        );
        assert!(plan.revalidate(&catalog, &snapshot).is_ok());

        snapshot.set_service(
            id("api"),
            record(
                ServiceState::Starting,
                fence,
                RuntimeOwnership::Task { task_id: task_a() },
                Some(ActiveOperation {
                    id: 55,
                    action: ServiceAction::Start,
                }),
            ),
        );
        assert!(matches!(
            catalog.admit(
                request(ServiceAction::Start, id("api"), fence, task_a()),
                &snapshot
            ),
            AdmissionDecision::Coalesced {
                operation_id: 55,
                action: ServiceAction::Start,
                ..
            }
        ));

        snapshot.set_service(
            id("db"),
            record(ServiceState::Failed, fence, RuntimeOwnership::None, None),
        );
        snapshot.set_service(
            id("api"),
            record(ServiceState::Stopped, fence, RuntimeOwnership::None, None),
        );
        assert!(matches!(
            catalog.admit(request(ServiceAction::Start, id("api"), fence, task_a()), &snapshot),
            AdmissionDecision::Refused(AdmissionRejection::DependencyNotReady {
                dependency,
                state: ServiceState::Failed,
                ..
            }) if dependency == id("db")
        ));
    }

    #[test]
    fn plan_revalidation_checks_catalog_root_order_scope_intent_and_fence_identity() {
        let catalog = ServiceCatalog::new(vec![
            service("api", ServiceScope::task(task_a()), vec![id("db")], 8080),
            service("db", ServiceScope::task(task_a()), vec![], 5432),
        ])
        .unwrap();
        let fence = AdmissionFence::new(4, 3, 9);
        let mut snapshot = AdmissionSnapshot::default();
        snapshot.set_service(
            id("api"),
            record(ServiceState::Stopped, fence, RuntimeOwnership::None, None),
        );
        snapshot.set_service(
            id("db"),
            record(ServiceState::Stopped, fence, RuntimeOwnership::None, None),
        );
        let AdmissionDecision::Start(plan) = catalog.admit(
            request(ServiceAction::Start, id("api"), fence, task_a()),
            &snapshot,
        ) else {
            panic!("expected start plan");
        };
        assert!(plan.revalidate(&catalog, &snapshot).is_ok());

        let mut wrong_root = plan.clone();
        wrong_root.root = id("db");
        assert!(wrong_root.revalidate(&catalog, &snapshot).is_err());

        let mut wrong_order = plan.clone();
        wrong_order.ordered.reverse();
        assert!(wrong_order.revalidate(&catalog, &snapshot).is_err());

        let mut wrong_scope = plan.clone();
        wrong_scope.ordered[0].scope = ServiceScope::Host;
        assert!(wrong_scope.revalidate(&catalog, &snapshot).is_err());

        let mut wrong_intent = plan.clone();
        wrong_intent.ordered[0].intent = None;
        assert!(wrong_intent.revalidate(&catalog, &snapshot).is_err());

        let mut wrong_fence_identity = plan;
        wrong_fence_identity.ordered[0].fence.service_id = id("api");
        assert!(wrong_fence_identity
            .revalidate(&catalog, &snapshot)
            .is_err());
    }

    #[test]
    fn plan_revalidation_rejects_a_changed_connection_epoch() {
        let catalog = ServiceCatalog::new(vec![service(
            "api",
            ServiceScope::task(task_a()),
            vec![],
            8080,
        )])
        .unwrap();
        let fence = AdmissionFence::new(4, 3, 9);
        let mut snapshot = AdmissionSnapshot::default();
        snapshot.set_service(
            id("api"),
            record(ServiceState::Stopped, fence, RuntimeOwnership::None, None),
        );
        let AdmissionDecision::Start(plan) = catalog.admit(
            request(ServiceAction::Start, id("api"), fence, task_a()),
            &snapshot,
        ) else {
            panic!("expected start plan");
        };

        snapshot.set_service(
            id("api"),
            record(
                ServiceState::Stopped,
                AdmissionFence::new(4, 4, 9),
                RuntimeOwnership::None,
                None,
            ),
        );
        assert!(plan.revalidate(&catalog, &snapshot).is_err());
    }

    #[test]
    fn stop_restart_revalidation_checks_reverse_order_and_snapshot_state() {
        let catalog = ServiceCatalog::new(vec![
            service("api", ServiceScope::task(task_a()), vec![id("db")], 8080),
            service("db", ServiceScope::task(task_a()), vec![], 5432),
        ])
        .unwrap();
        let fence = AdmissionFence::new(4, 3, 9);
        let owned = RuntimeOwnership::Task { task_id: task_a() };
        let mut snapshot = AdmissionSnapshot::default();
        snapshot.set_service(
            id("api"),
            record(ServiceState::Healthy, fence, owned.clone(), None),
        );
        snapshot.set_service(
            id("db"),
            record(ServiceState::Healthy, fence, owned.clone(), None),
        );

        let AdmissionDecision::Stop(stop_plan) = catalog.admit(
            request(ServiceAction::Stop, id("db"), fence, task_a()),
            &snapshot,
        ) else {
            panic!("expected stop plan");
        };
        assert_eq!(
            stop_plan
                .members()
                .iter()
                .map(|item| item.service_id().clone())
                .collect::<Vec<_>>(),
            vec![id("api"), id("db")]
        );
        assert!(stop_plan.revalidate(&catalog, &snapshot).is_ok());
        snapshot.set_service(
            id("api"),
            record(
                ServiceState::Healthy,
                fence,
                owned.clone(),
                Some(ActiveOperation {
                    id: 93,
                    action: ServiceAction::Start,
                }),
            ),
        );
        assert!(stop_plan.revalidate(&catalog, &snapshot).is_err());
        snapshot.set_service(
            id("api"),
            record(ServiceState::Healthy, fence, owned.clone(), None),
        );

        let AdmissionDecision::Restart(restart_plan) = catalog.admit(
            request(ServiceAction::Restart, id("api"), fence, task_a()),
            &snapshot,
        ) else {
            panic!("expected restart plan");
        };
        assert!(restart_plan.revalidate(&catalog, &snapshot).is_ok());
        assert_eq!(
            restart_plan
                .stop()
                .members()
                .iter()
                .map(|item| item.service_id().clone())
                .collect::<Vec<_>>(),
            vec![id("api")]
        );
        assert_eq!(
            restart_plan
                .start()
                .members()
                .iter()
                .map(|item| item.service_id().clone())
                .collect::<Vec<_>>(),
            vec![id("db"), id("api")]
        );
    }

    #[test]
    fn task_close_fails_closed_and_accounts_for_every_task_scoped_definition() {
        let catalog = ServiceCatalog::new(vec![
            service("api", ServiceScope::task(task_a()), vec![], 8080),
            service("worker", ServiceScope::task(task_a()), vec![], 8081),
            service("other", ServiceScope::task(task_b()), vec![], 8082),
            service("host-db", ServiceScope::Host, vec![], 5432),
        ])
        .unwrap();
        let fence = AdmissionFence::new(4, 3, 9);

        let mut missing = AdmissionSnapshot::default();
        missing.set_task_epoch(task_a(), ActionEpoch::new(9));
        missing.mark_task_closing(task_a());
        missing.set_service(
            id("api"),
            record(
                ServiceState::Healthy,
                fence,
                RuntimeOwnership::Task { task_id: task_a() },
                None,
            ),
        );
        assert!(matches!(
            catalog.admit_task_close(task_a(), ActionEpoch::new(9), &missing),
            Err(AdmissionRejection::EvidenceUnknown { service }) if service == id("worker")
        ));

        let mut foreign = missing.clone();
        foreign.set_service(
            id("worker"),
            record(
                ServiceState::Healthy,
                fence,
                RuntimeOwnership::Task { task_id: task_b() },
                None,
            ),
        );
        assert!(matches!(
            catalog.admit_task_close(task_a(), ActionEpoch::new(9), &foreign),
            Err(AdmissionRejection::OwnershipMismatch { service, .. }) if service == id("worker")
        ));

        let mut host_owned = missing.clone();
        host_owned.set_service(
            id("worker"),
            record(
                ServiceState::Healthy,
                fence,
                RuntimeOwnership::Host { host_id: host_a() },
                None,
            ),
        );
        assert!(matches!(
            catalog.admit_task_close(task_a(), ActionEpoch::new(9), &host_owned),
            Err(AdmissionRejection::OwnershipMismatch { service, .. }) if service == id("worker")
        ));

        let mut external = missing.clone();
        external.set_service(
            id("worker"),
            record(
                ServiceState::External,
                fence,
                RuntimeOwnership::External,
                None,
            ),
        );
        assert!(matches!(
            catalog.admit_task_close(task_a(), ActionEpoch::new(9), &external),
            Err(AdmissionRejection::ExternalNotControllable { service }) if service == id("worker")
        ));

        let mut complete = missing;
        complete.set_service(
            id("worker"),
            record(ServiceState::Stopped, fence, RuntimeOwnership::None, None),
        );
        complete.set_service(
            id("other"),
            record(
                ServiceState::Healthy,
                fence,
                RuntimeOwnership::Task { task_id: task_b() },
                None,
            ),
        );
        complete.set_service(
            id("host-db"),
            record(
                ServiceState::Healthy,
                fence,
                RuntimeOwnership::Host { host_id: host_a() },
                None,
            ),
        );
        let close = catalog
            .admit_task_close(task_a(), ActionEpoch::new(9), &complete)
            .unwrap();
        assert_eq!(close.members().len(), 2);
        assert_eq!(
            close
                .members()
                .iter()
                .map(|item| item.service_id().clone())
                .collect::<Vec<_>>(),
            vec![id("worker"), id("api")]
        );
        assert!(close.revalidate(&catalog, &complete).is_ok());
    }

    #[test]
    fn task_close_rejects_stopped_members_that_still_retain_task_ownership() {
        let catalog = ServiceCatalog::new(vec![service(
            "api",
            ServiceScope::task(task_a()),
            vec![],
            8080,
        )])
        .unwrap();
        let fence = AdmissionFence::new(4, 3, 9);
        let mut snapshot = AdmissionSnapshot::default();
        snapshot.set_task_epoch(task_a(), ActionEpoch::new(9));
        snapshot.mark_task_closing(task_a());
        snapshot.set_service(
            id("api"),
            record(
                ServiceState::Stopped,
                fence,
                RuntimeOwnership::Task { task_id: task_a() },
                None,
            ),
        );

        assert!(matches!(
            catalog.admit_task_close(task_a(), ActionEpoch::new(9), &snapshot),
            Err(AdmissionRejection::OwnershipMismatch { service, .. }) if service == id("api")
        ));
    }

    #[test]
    fn task_close_rejects_task_owned_snapshot_resources_missing_from_catalog() {
        let catalog = ServiceCatalog::new(vec![service(
            "api",
            ServiceScope::task(task_a()),
            vec![],
            8080,
        )])
        .unwrap();
        let fence = AdmissionFence::new(4, 3, 9);
        let mut snapshot = AdmissionSnapshot::default();
        snapshot.set_task_epoch(task_a(), ActionEpoch::new(9));
        snapshot.mark_task_closing(task_a());
        snapshot.set_service(
            id("api"),
            record(
                ServiceState::Healthy,
                fence,
                RuntimeOwnership::Task { task_id: task_a() },
                None,
            ),
        );
        snapshot.set_service(
            id("removed-service"),
            record(
                ServiceState::Stopped,
                fence,
                RuntimeOwnership::Task { task_id: task_a() },
                None,
            ),
        );

        assert!(matches!(
            catalog.admit_task_close(task_a(), ActionEpoch::new(9), &snapshot),
            Err(AdmissionRejection::TaskOwnedResourceNotInCatalog { service, task_id })
                if service == id("removed-service") && task_id == task_a()
        ));
    }

    #[test]
    fn task_close_rejects_task_ownership_on_host_scoped_snapshot_resources() {
        let catalog = ServiceCatalog::new(vec![
            service("api", ServiceScope::task(task_a()), vec![], 8080),
            service("host-db", ServiceScope::Host, vec![], 5432),
        ])
        .unwrap();
        let fence = AdmissionFence::new(4, 3, 9);
        let mut snapshot = AdmissionSnapshot::default();
        snapshot.set_task_epoch(task_a(), ActionEpoch::new(9));
        snapshot.mark_task_closing(task_a());
        snapshot.set_service(
            id("api"),
            record(
                ServiceState::Healthy,
                fence,
                RuntimeOwnership::Task { task_id: task_a() },
                None,
            ),
        );
        snapshot.set_service(
            id("host-db"),
            record(
                ServiceState::Healthy,
                fence,
                RuntimeOwnership::Task { task_id: task_a() },
                None,
            ),
        );

        assert!(matches!(
            catalog.admit_task_close(task_a(), ActionEpoch::new(9), &snapshot),
            Err(AdmissionRejection::TaskOwnedResourceOutsideScope { service, task_id })
                if service == id("host-db") && task_id == task_a()
        ));
    }

    #[test]
    fn command_arguments_reject_secret_assignment_options_without_exposing_values() {
        let secret = "raw-secret-value";
        for args in [
            vec![format!("--env=API_TOKEN={secret}")],
            vec!["--env".to_string(), format!("API_TOKEN={secret}")],
            vec![format!("--set-env=TOKEN={secret}")],
            vec!["--set-env".to_string(), format!("TOKEN={secret}")],
        ] {
            let error = CommandSpec::new("node")
                .expect("valid command")
                .with_args(args)
                .expect_err("raw assignment must be rejected");
            assert!(!format!("{error:?}").contains(secret));
            assert!(!error.to_string().contains(secret));
        }
    }
}
