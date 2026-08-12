//! Bind `config.json` run commands to validated service definitions.
//!
//! Environment *values* never enter [`CommandSpec`]. They stay in a redacted
//! overlay that is applied only at managed-launch time. Caller-supplied cwd and
//! env maps are not trusted; cwd and env layering come from project/folder/
//! command config (plus an optional host-resolved folder env-file overlay).

use std::{
    collections::BTreeMap,
    fmt,
    path::{Component, Path, PathBuf},
};

use crate::{
    config::{Nullable, Project, ProjectFolder, RunCommand},
    domain::TaskId,
    services::model::{
        CommandSpec, ExpectedPort, HealthPolicy, HealthSpec, PortProtocol, ServiceDefinition,
        ServiceId, ServiceScope, StartupPolicy, StopPolicy, ValidationError,
    },
};

const MAX_BOUND_SERVICES: usize = crate::services::model::MAX_SERVICE_COUNT;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConfiguredServiceOwner {
    Project {
        project_id: String,
    },
    Workspace {
        project_id: String,
        folder_id: String,
    },
    Task {
        task_id: TaskId,
    },
}

impl ConfiguredServiceOwner {
    pub fn catalog_scope(&self) -> ServiceScope {
        match self {
            Self::Task { task_id } => ServiceScope::task(*task_id),
            Self::Project { .. } | Self::Workspace { .. } => ServiceScope::Host,
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct EnvironmentOverlay {
    values: BTreeMap<String, String>,
}

impl fmt::Debug for EnvironmentOverlay {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EnvironmentOverlay")
            .field("values", &format_args!("<{} redacted>", self.values.len()))
            .finish()
    }
}

impl EnvironmentOverlay {
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.values.keys().map(String::as_str)
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }

    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    pub(crate) fn into_launch_env(self) -> BTreeMap<String, String> {
        self.values
    }

    pub(crate) fn from_names(names: impl IntoIterator<Item = String>) -> Self {
        let mut values = BTreeMap::new();
        for name in names {
            values.insert(name, String::new());
        }
        Self { values }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfiguredServiceBinding {
    pub owner: ConfiguredServiceOwner,
    pub project_id: String,
    pub folder_id: String,
    pub command_id: String,
    /// Absolute project/workspace root used to resolve relative command cwd at
    /// launch. Never the DevManager process working directory.
    pub workspace_root: String,
    pub definition: ServiceDefinition,
    pub environment: EnvironmentOverlay,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BindingError {
    TooManyServices { limit: usize },
    DuplicateCommand { command_id: String },
    ArchivedCommand,
    InvalidWorkspaceRoot,
    InvalidEnvFilePath,
    EnvFileOutsideWorkspace,
    OwnershipMismatch,
    Validation(ValidationError),
}

impl fmt::Display for BindingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooManyServices { limit } => {
                write!(formatter, "configured service count exceeds {limit}")
            }
            Self::DuplicateCommand { .. } => formatter.write_str("duplicate configured command id"),
            Self::ArchivedCommand => formatter.write_str("archived configured command"),
            Self::InvalidWorkspaceRoot => {
                formatter.write_str("configured workspace root must be absolute")
            }
            Self::InvalidEnvFilePath => {
                formatter.write_str("configured env-file path is empty or unsafe")
            }
            Self::EnvFileOutsideWorkspace => {
                formatter.write_str("configured env-file path resolves outside the workspace root")
            }
            Self::OwnershipMismatch => {
                formatter.write_str("task workspace context requires the matching task owner")
            }
            Self::Validation(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for BindingError {}

impl From<ValidationError> for BindingError {
    fn from(error: ValidationError) -> Self {
        Self::Validation(error)
    }
}

/// Authoritative binding inputs. Cwd is derived from project/folder paths;
/// `folder_env_file` may only carry host-resolved values for the folder's
/// configured env-file path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfiguredServiceSource<'a> {
    pub project: &'a Project,
    pub folder: &'a ProjectFolder,
    pub command: &'a RunCommand,
    pub owner: ConfiguredServiceOwner,
    pub folder_env_file: Option<&'a BTreeMap<String, String>>,
}

pub fn bind_configured_command(
    source: ConfiguredServiceSource<'_>,
) -> Result<ConfiguredServiceBinding, BindingError> {
    if matches!(source.command.archived, Nullable::Value(true)) {
        return Err(BindingError::ArchivedCommand);
    }
    let definition_id = ServiceId::new(source.command.id.clone())?;
    let scope = source.owner.catalog_scope();
    let mut command = CommandSpec::new(source.command.command.clone())?;
    command = command.with_args(source.command.args.iter().cloned())?;
    if let Some(cwd) = derive_workspace_cwd(source.project, source.folder)? {
        command = command.with_cwd(cwd)?;
    }

    let mut overlay = BTreeMap::new();
    if let Some(folder_env) = source.folder_env_file {
        layer_env(&mut overlay, folder_env)?;
    }
    if let Nullable::Value(command_env) = &source.command.env {
        layer_env(&mut overlay, command_env)?;
    }
    if let (Nullable::Value(port), Nullable::Value(variable)) =
        (&source.command.port, &source.folder.port_variable)
    {
        overlay.insert(variable.clone(), port.to_string());
    } else if let Nullable::Value(port) = &source.command.port {
        overlay.insert("PORT".to_owned(), port.to_string());
    }

    let mut names: Vec<String> = overlay.keys().cloned().collect();
    names.sort();
    for name in &names {
        command = command.with_env_reference(name.clone())?;
    }

    let expected_port = match &source.command.port {
        Nullable::Value(port) => Some(ExpectedPort {
            protocol: PortProtocol::Tcp,
            port: *port,
        }),
        Nullable::Absent | Nullable::Null => None,
    };
    let health = match expected_port {
        Some(port) => HealthSpec::Tcp {
            port: port.port,
            policy: HealthPolicy::default(),
        },
        None => HealthSpec::None,
    };
    let restart_limit = match source.command.auto_restart {
        Nullable::Value(true) => 1,
        _ => 0,
    };

    let definition = ServiceDefinition {
        id: definition_id,
        scope,
        command,
        dependencies: Vec::new(),
        health,
        startup: StartupPolicy {
            trigger: crate::services::model::StartupTrigger::Manual,
            restart_limit,
        },
        stop: StopPolicy::default(),
        expected_port,
    };
    definition.validate()?;

    Ok(ConfiguredServiceBinding {
        owner: source.owner,
        project_id: source.project.id.clone(),
        folder_id: source.folder.id.clone(),
        command_id: source.command.id.clone(),
        workspace_root: validate_absolute_workspace_root(&source.project.root_path)?,
        definition,
        environment: EnvironmentOverlay { values: overlay },
    })
}

/// Replace the binding workspace root with a task-specific absolute root.
/// Relative or empty roots fail closed; process cwd is never consulted.
pub fn with_task_workspace_root(
    mut binding: ConfiguredServiceBinding,
    absolute_root: impl AsRef<str>,
) -> Result<ConfiguredServiceBinding, BindingError> {
    binding.workspace_root = validate_absolute_workspace_root(absolute_root.as_ref())?;
    Ok(binding)
}

/// Resolve a configured folder env-file path for host overlay loading.
///
/// Absolute env-file paths keep absolute behavior (with traversal rejected).
/// Relative paths resolve under `project_root` + the folder's normalized
/// workspace-relative path and fail closed on traversal or outside-root joins.
/// Process cwd is never consulted.
pub fn resolve_configured_env_file_path(
    project_root: &str,
    folder_path: &str,
    env_file_path: &str,
) -> Result<PathBuf, BindingError> {
    let env_file_path = env_file_path.trim();
    if env_file_path.is_empty() || has_dot_dot_component(env_file_path) {
        return Err(BindingError::InvalidEnvFilePath);
    }
    if is_absolute_path(env_file_path) {
        return Ok(PathBuf::from(env_file_path));
    }

    let root = validate_absolute_workspace_root(project_root)?;
    let folder_relative = folder_relative_under_root(project_root, folder_path)?;
    let mut resolved = PathBuf::from(&root);
    push_relative_components(&mut resolved, &folder_relative)?;
    push_relative_components(&mut resolved, &normalize_relative(env_file_path))?;
    if !lexical_path_is_under_root(&resolved, &root) {
        return Err(BindingError::EnvFileOutsideWorkspace);
    }
    Ok(resolved)
}

/// Host-private context that binds one selected Task workspace root into
/// service projection/action path resolution without exposing env values.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskServicePathContext {
    pub task_id: crate::domain::TaskId,
    workspace_root: String,
}

impl TaskServicePathContext {
    pub fn try_new(
        task_id: crate::domain::TaskId,
        absolute_workspace_root: impl AsRef<str>,
    ) -> Result<Self, BindingError> {
        Ok(Self {
            task_id,
            workspace_root: validate_absolute_workspace_root(absolute_workspace_root.as_ref())?,
        })
    }

    pub fn workspace_root(&self) -> &str {
        &self.workspace_root
    }

    pub fn bind_service(
        &self,
        binding: ConfiguredServiceBinding,
    ) -> Result<ConfiguredServiceBinding, BindingError> {
        match &binding.owner {
            ConfiguredServiceOwner::Task { task_id } if *task_id == self.task_id => {
                with_task_workspace_root(binding, &self.workspace_root)
            }
            ConfiguredServiceOwner::Task { .. }
            | ConfiguredServiceOwner::Project { .. }
            | ConfiguredServiceOwner::Workspace { .. } => Err(BindingError::OwnershipMismatch),
        }
    }

    pub fn resolve_env_file(
        &self,
        folder_path: &str,
        env_file_path: &str,
    ) -> Result<PathBuf, BindingError> {
        resolve_configured_env_file_path(&self.workspace_root, folder_path, env_file_path)
    }
}

pub fn bind_configured_services<'a>(
    sources: impl IntoIterator<Item = ConfiguredServiceSource<'a>>,
) -> Result<Vec<ConfiguredServiceBinding>, BindingError> {
    let mut bindings = Vec::new();
    let mut seen = BTreeMap::new();
    for source in sources {
        if bindings.len() >= MAX_BOUND_SERVICES {
            return Err(BindingError::TooManyServices {
                limit: MAX_BOUND_SERVICES,
            });
        }
        let binding = bind_configured_command(source)?;
        if seen
            .insert(binding.command_id.clone(), binding.project_id.clone())
            .is_some()
        {
            return Err(BindingError::DuplicateCommand {
                command_id: binding.command_id,
            });
        }
        bindings.push(binding);
    }
    Ok(bindings)
}

fn validate_absolute_workspace_root(root: &str) -> Result<String, BindingError> {
    let root = root.trim();
    if root.is_empty() || !is_absolute_path(root) {
        return Err(BindingError::InvalidWorkspaceRoot);
    }
    Ok(root.to_owned())
}

fn derive_workspace_cwd(
    project: &Project,
    folder: &ProjectFolder,
) -> Result<Option<String>, BindingError> {
    let relative = folder_relative_under_root(&project.root_path, &folder.folder_path)?;
    if relative.is_empty() {
        Ok(None)
    } else {
        Ok(Some(relative))
    }
}

fn folder_relative_under_root(project_root: &str, folder_path: &str) -> Result<String, BindingError> {
    let folder_path = folder_path.trim();
    if folder_path.is_empty() {
        return Ok(String::new());
    }
    if has_dot_dot_component(folder_path) {
        return Err(BindingError::EnvFileOutsideWorkspace);
    }
    if !is_absolute_path(folder_path) {
        return Ok(normalize_relative(folder_path));
    }
    let root = project_root.trim();
    if root.is_empty() {
        return Err(BindingError::InvalidWorkspaceRoot);
    }
    let path_norm = folder_path.replace('\\', "/").trim_end_matches('/').to_owned();
    let root_norm = root.replace('\\', "/").trim_end_matches('/').to_owned();
    if path_norm.eq_ignore_ascii_case(&root_norm) {
        // Folder is the project root itself; relative env files resolve under root.
        return Ok(String::new());
    }
    strip_root_prefix(folder_path, root)
        .map(|relative| normalize_relative(&relative))
        .ok_or(BindingError::EnvFileOutsideWorkspace)
}

fn is_absolute_path(path: &str) -> bool {
    if Path::new(path).is_absolute() || path.starts_with('/') || path.starts_with('\\') {
        return true;
    }
    // A drive-relative path such as `C:folder` is not rooted.  It resolves
    // against the caller's current directory on that drive and would defeat
    // the configured-workspace containment boundary.
    let bytes = path.as_bytes();
    bytes.len() >= 3 && bytes[1] == b':' && (bytes[2] == b'/' || bytes[2] == b'\\')
}

fn strip_root_prefix(path: &str, root: &str) -> Option<String> {
    let path_norm = path.replace('\\', "/").trim_end_matches('/').to_owned();
    let root_norm = root.replace('\\', "/").trim_end_matches('/').to_owned();
    let prefix = format!("{root_norm}/");
    path_norm
        .strip_prefix(&prefix)
        .or_else(|| {
            let root_lower = root_norm.to_ascii_lowercase();
            let path_lower = path_norm.to_ascii_lowercase();
            path_lower
                .strip_prefix(&format!("{root_lower}/"))
                .map(|_| &path_norm[root_norm.len() + 1..])
        })
        .map(|relative| relative.to_owned())
        .filter(|relative| !relative.is_empty())
}

fn normalize_relative(path: &str) -> String {
    path.replace('\\', "/").trim_matches('/').to_owned()
}

fn has_dot_dot_component(path: &str) -> bool {
    Path::new(path).components().any(|component| {
        matches!(component, Component::ParentDir)
            || matches!(component, Component::Normal(name) if name == "..")
    })
}

fn push_relative_components(path: &mut PathBuf, relative: &str) -> Result<(), BindingError> {
    if relative.is_empty() {
        return Ok(());
    }
    for component in Path::new(relative).components() {
        match component {
            Component::Normal(part) => path.push(part),
            Component::CurDir => {}
            Component::ParentDir => return Err(BindingError::EnvFileOutsideWorkspace),
            Component::RootDir | Component::Prefix(_) => {
                return Err(BindingError::InvalidEnvFilePath);
            }
        }
    }
    Ok(())
}

fn lexical_path_is_under_root(path: &Path, root: &str) -> bool {
    let path_norm = path.to_string_lossy().replace('\\', "/").to_ascii_lowercase();
    let root_norm = root.replace('\\', "/").trim_end_matches('/').to_ascii_lowercase();
    path_norm == root_norm || path_norm.starts_with(&format!("{root_norm}/"))
}

fn layer_env(
    overlay: &mut BTreeMap<String, String>,
    values: &BTreeMap<String, String>,
) -> Result<(), BindingError> {
    for (name, value) in values {
        crate::services::model::EnvReference::new(name.clone())?;
        overlay.insert(name.clone(), value.clone());
    }
    Ok(())
}
