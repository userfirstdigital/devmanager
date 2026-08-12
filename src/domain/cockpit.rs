//! Typed Task Cockpit query/result contracts.
//!
//! These documents carry no raw secrets, credentials, command lines, or
//! client-authoritative absolute paths. Host resolves workspace identity from
//! the selected Task admission.

use serde::{Deserialize, Serialize};

use crate::domain::id::{ConfiguredServiceId, TaskId};
use crate::domain::task::{WorkspaceBindingKind, WorkspaceRef};

pub const MAX_COCKPIT_FILE_LIST: u16 = 64;
pub const MAX_COCKPIT_READ_BYTES: u32 = 64 * 1024;
pub const MAX_COCKPIT_RELATIVE_PATH_BYTES: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskCockpitSurface {
    Workspace,
    Git,
    Files,
    Ssh,
    Services,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskCockpitDeniedReason {
    MissingTask,
    Unauthorized,
    PathTraversal,
    OutsideWorkspace,
    CapabilityDenied,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskCockpitUnavailableReason {
    GitAuthorityNotIssued,
    FileAuthorityNotIssued,
    SshOperationUnsupported,
    ServiceSupervisorUnavailable,
    WriteUnsupported,
    LogsUnsupported,
    HealthUnsupported,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum TaskCockpitQuery {
    WorkspaceStatus,
    GitStatus,
    FilesList {
        relative_directory: Option<String>,
        limit: u16,
    },
    FilesRead {
        relative_path: String,
        max_bytes: u32,
    },
    FilesWrite,
    GitMutate,
    SshStatus,
    SshAction,
    ServiceSnapshots,
    ServiceLogs,
    ServiceHealth,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskWorkspaceKind {
    Main,
    Worktree,
    External,
    Bound,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskWorkspaceProjection {
    pub task_id: TaskId,
    pub kind: TaskWorkspaceKind,
    pub bound: bool,
    pub branch: Option<String>,
    pub has_repository_fingerprint: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskGitProjection {
    pub task_id: TaskId,
    pub branch: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub change_count: u32,
    pub detached: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskFileEntry {
    pub relative_path: String,
    pub is_directory: bool,
    pub secret: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskFilesListProjection {
    pub task_id: TaskId,
    pub entries: Vec<TaskFileEntry>,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskFilesReadProjection {
    pub task_id: TaskId,
    pub relative_path: String,
    pub utf8_prefix: Option<String>,
    pub byte_len: u32,
    pub secret: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskSshEndpoint {
    pub id: String,
    pub label: String,
    pub archived: bool,
    pub has_credential: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskSshProjection {
    pub task_id: TaskId,
    pub endpoints: Vec<TaskSshEndpoint>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskServiceRuntimeState {
    Stopped,
    Starting,
    Healthy,
    Unhealthy,
    External,
    Stopping,
    Failed,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum TaskServiceScope {
    Host,
    Task { task_id: TaskId },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskServiceSnapshot {
    pub service_id: ConfiguredServiceId,
    pub scope: TaskServiceScope,
    pub state: TaskServiceRuntimeState,
    pub generation: u64,
    pub epoch: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskServiceProjection {
    pub task_id: TaskId,
    pub snapshots: Vec<TaskServiceSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum TaskCockpitResult {
    Workspace(TaskWorkspaceProjection),
    Git(TaskGitProjection),
    FilesList(TaskFilesListProjection),
    FilesRead(TaskFilesReadProjection),
    Ssh(TaskSshProjection),
    Services(TaskServiceProjection),
    Denied {
        surface: TaskCockpitSurface,
        reason: TaskCockpitDeniedReason,
    },
    Unavailable {
        surface: TaskCockpitSurface,
        reason: TaskCockpitUnavailableReason,
    },
}

pub fn relative_path_is_safe(path: &str) -> bool {
    if path.is_empty() || path.len() > MAX_COCKPIT_RELATIVE_PATH_BYTES {
        return false;
    }
    if path.contains('\\')
        || path.starts_with('/')
        || path.as_bytes().get(1) == Some(&b':')
        || path.split(['/', '\\']).any(|component| {
            component.is_empty() || component == "." || component == ".."
        })
    {
        return false;
    }
    true
}

pub fn workspace_projection(task_id: TaskId, workspace: &WorkspaceRef) -> TaskWorkspaceProjection {
    let (kind, bound, branch) = workspace_identity(workspace);
    TaskWorkspaceProjection {
        task_id,
        kind,
        bound,
        branch,
        has_repository_fingerprint: workspace.repository_fingerprint().is_some(),
    }
}

pub fn git_projection(task_id: TaskId, workspace: &WorkspaceRef) -> TaskGitProjection {
    let (_, _, branch) = workspace_identity(workspace);
    TaskGitProjection {
        task_id,
        branch,
        ahead: 0,
        behind: 0,
        change_count: 0,
        detached: false,
    }
}

fn workspace_identity(workspace: &WorkspaceRef) -> (TaskWorkspaceKind, bool, Option<String>) {
    match workspace {
        WorkspaceRef::Main | WorkspaceRef::MainWithFingerprint { .. } => {
            (TaskWorkspaceKind::Main, false, None)
        }
        WorkspaceRef::Worktree { branch, .. }
        | WorkspaceRef::WorktreeWithFingerprint { branch, .. } => {
            (TaskWorkspaceKind::Worktree, false, Some(branch.clone()))
        }
        WorkspaceRef::External { .. } => (TaskWorkspaceKind::External, false, None),
        WorkspaceRef::HostBound { binding } => (
            workspace_kind(binding.kind()),
            true,
            binding.branch().map(str::to_owned),
        ),
        WorkspaceRef::ExternalWithFingerprint { binding, .. } => (
            TaskWorkspaceKind::External,
            true,
            binding.branch().map(str::to_owned),
        ),
    }
}

fn workspace_kind(kind: WorkspaceBindingKind) -> TaskWorkspaceKind {
    match kind {
        WorkspaceBindingKind::Main => TaskWorkspaceKind::Main,
        WorkspaceBindingKind::Worktree => TaskWorkspaceKind::Worktree,
        WorkspaceBindingKind::External => TaskWorkspaceKind::External,
    }
}

pub fn cockpit_surface(query: &TaskCockpitQuery) -> TaskCockpitSurface {
    match query {
        TaskCockpitQuery::WorkspaceStatus => TaskCockpitSurface::Workspace,
        TaskCockpitQuery::GitStatus | TaskCockpitQuery::GitMutate => TaskCockpitSurface::Git,
        TaskCockpitQuery::FilesList { .. }
        | TaskCockpitQuery::FilesRead { .. }
        | TaskCockpitQuery::FilesWrite => TaskCockpitSurface::Files,
        TaskCockpitQuery::SshStatus | TaskCockpitQuery::SshAction => TaskCockpitSurface::Ssh,
        TaskCockpitQuery::ServiceSnapshots
        | TaskCockpitQuery::ServiceLogs
        | TaskCockpitQuery::ServiceHealth => TaskCockpitSurface::Services,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::id::ServiceId;
    use crate::domain::query::{Query, QueryResult};

    #[test]
    fn relative_paths_reject_traversal_and_absolute_forms() {
        assert!(relative_path_is_safe("src/lib.rs"));
        assert!(!relative_path_is_safe("../secret"));
        assert!(!relative_path_is_safe("/etc/passwd"));
        assert!(!relative_path_is_safe("C:/Windows/system32"));
        assert!(!relative_path_is_safe("foo\\bar"));
        assert!(!relative_path_is_safe(""));
        assert!(!relative_path_is_safe("foo/./bar"));
    }

    #[test]
    fn configured_service_id_is_not_the_uuid_service_id() {
        let catalog = ConfiguredServiceId::new("api").expect("catalog id");
        let uuid = ServiceId::new();
        assert_eq!(catalog.as_str(), "api");
        assert!(ServiceId::parse("api").is_err());
        assert_ne!(catalog.as_str(), uuid.to_string());
        let encoded = serde_json::to_string(&catalog).expect("encode catalog id");
        assert_eq!(encoded, "\"api\"");
        let uuid_encoded = serde_json::to_string(&uuid).expect("encode uuid");
        assert_ne!(uuid_encoded, encoded);
    }

    #[test]
    fn query_and_result_use_the_task_cockpit_wire_name() {
        let query = Query::TaskCockpit(TaskCockpitQuery::WorkspaceStatus);
        let encoded = serde_json::to_value(&query).expect("encode query");
        assert!(encoded.get("task_cockpit").is_some());
        let decoded: Query = serde_json::from_value(encoded).expect("decode query");
        assert_eq!(decoded, query);

        let task_id = TaskId::new();
        let result = QueryResult::TaskCockpit(TaskCockpitResult::Denied {
            surface: TaskCockpitSurface::Files,
            reason: TaskCockpitDeniedReason::PathTraversal,
        });
        let encoded = serde_json::to_value(&result).expect("encode result");
        assert!(encoded.get("task_cockpit").is_some());
        let decoded: QueryResult = serde_json::from_value(encoded).expect("decode result");
        assert_eq!(decoded, result);
        let _ = task_id;
    }

    #[test]
    fn service_control_intent_wires_configured_string_not_uuid() {
        use crate::domain::command::{Command, ServiceControlAction, ServiceControlIntent};

        let command = Command::ServiceControl(ServiceControlIntent {
            service_id: ConfiguredServiceId::new("api").expect("catalog"),
            resource_generation: 1,
            connection_epoch: 2,
            action_epoch: 3,
            action: ServiceControlAction::Start,
        });
        let encoded = serde_json::to_value(&command).expect("encode");
        let payload = encoded
            .get("service_control")
            .expect("service_control variant");
        assert_eq!(payload.get("service_id").and_then(|value| value.as_str()), Some("api"));
        let text = payload.to_string();
        assert!(!text.contains("019"), "must not look like a UUIDv7 service id: {text}");
        let decoded: Command = serde_json::from_value(encoded).expect("decode");
        let Command::ServiceControl(intent) = decoded else {
            panic!("round trip");
        };
        assert_eq!(intent.service_id.as_str(), "api");
    }

    #[test]
    fn workspace_projection_omits_raw_paths() {
        let task_id = TaskId::new();
        let projection = workspace_projection(task_id, &WorkspaceRef::Main);
        let encoded = serde_json::to_string(&projection).expect("encode");
        assert!(!encoded.contains("C:"));
        assert!(!encoded.contains("/"));
        assert_eq!(projection.kind, TaskWorkspaceKind::Main);
        assert!(!projection.bound);
    }
}
