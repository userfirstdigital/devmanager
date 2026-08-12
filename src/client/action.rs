//! Shared action catalog for CLI and future GPUI clients.
//!
//! This slice exposes `host.actions`, `host.status`, `task.list`, `task.show`,
//! `task.create`, and `task.rename`. It is intentionally not a dynamic
//! plugin framework.

use serde::{Deserialize, Serialize};

use crate::{
    domain::{
        command::{
            Command, CommandEnvelope, CreateTaskIntent, CreateTaskRequestIntent, RenameTaskIntent,
        },
        query::{Query, QueryEnvelope},
        task::{
            ReviewReadiness, TaskActivity, TaskAssignment, TaskAttention, TaskConnectivity,
            TaskFacts, TaskValidationError, WorkspaceRef,
        },
        ClientId, CommandId, EnvironmentId, ProjectId, RequestId, TaskId,
    },
    protocol::Capability,
    services::model::ServiceId,
    workspace::{WorkspaceError, WorkspaceRequest},
};

/// Stable id for listing the shared action catalog.
pub const ACTION_HOST_ACTIONS: &str = "host.actions";
/// Stable id for attaching and reporting host status.
pub const ACTION_HOST_STATUS: &str = "host.status";
/// Stable id for listing Tasks through the paged snapshot query boundary.
pub const ACTION_TASK_LIST: &str = "task.list";
/// Stable id for reading one Task through the host query boundary.
pub const ACTION_TASK_SHOW: &str = "task.show";
/// Frozen V1 codec id for the old durable-workspace create shape.
///
/// This id remains available to decode preserved V1 data, but is intentionally
/// not advertised as an authenticated public action.
pub const ACTION_TASK_CREATE: &str = "task.create";
/// Stable id for request-shaped task creation whose workspace is resolved by the host.
pub const ACTION_TASK_CREATE_V2: &str = "task.create.v2";
/// Stable id for renaming one Task through the host command boundary.
pub const ACTION_TASK_RENAME: &str = "task.rename";
/// Stable id for starting one configured service through the host supervisor.
pub const ACTION_SERVICE_START: &str = "service.start";
/// Stable id for stopping one configured service through the host supervisor.
pub const ACTION_SERVICE_STOP: &str = "service.stop";
/// Stable id for restarting one configured service through the host supervisor.
pub const ACTION_SERVICE_RESTART: &str = "service.restart";
/// Stable id for reading bounded redacted service logs.
pub const ACTION_SERVICE_LOGS: &str = "service.logs";
/// Stable id for reading the last redacted service health snapshot.
pub const ACTION_SERVICE_HEALTH: &str = "service.health";

/// Where an action applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionScope {
    Host,
    Task,
}

/// Risk classification for catalog entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionRisk {
    ReadOnly,
    Mutating,
}

/// Closed argument-schema classification for catalog entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionArgumentSchema {
    None,
    TaskId,
    TaskCreateV1,
    TaskCreateV2,
    TaskRenameV1,
    ServiceControlV1,
}

/// Static metadata for one catalog action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionDescriptor {
    pub id: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub keywords: &'static [&'static str],
    pub scope: ActionScope,
    pub required_capability: Option<Capability>,
    pub risk: ActionRisk,
    pub argument_schema: ActionArgumentSchema,
}

const ACTIONS: &[ActionDescriptor] = &[
    ActionDescriptor {
        id: ACTION_HOST_ACTIONS,
        title: "List actions",
        description: "Emit the shared action catalog as versioned JSON.",
        keywords: &["actions", "catalog", "help", "list"],
        scope: ActionScope::Host,
        required_capability: None,
        risk: ActionRisk::ReadOnly,
        argument_schema: ActionArgumentSchema::None,
    },
    ActionDescriptor {
        id: ACTION_HOST_STATUS,
        title: "Host status",
        description: "Attach to a running named-profile host and report ServerHello status fields.",
        keywords: &["status", "host", "hello", "attach"],
        scope: ActionScope::Host,
        required_capability: None,
        risk: ActionRisk::ReadOnly,
        argument_schema: ActionArgumentSchema::None,
    },
    ActionDescriptor {
        id: ACTION_TASK_LIST,
        title: "List tasks",
        description: "List Tasks through the host paged snapshot query boundary.",
        keywords: &["task", "list", "tasks", "snapshot"],
        scope: ActionScope::Host,
        required_capability: Some(Capability::PagedSnapshots),
        risk: ActionRisk::ReadOnly,
        argument_schema: ActionArgumentSchema::None,
    },
    ActionDescriptor {
        id: ACTION_TASK_SHOW,
        title: "Show task",
        description: "Read one Task snapshot through the host query boundary.",
        keywords: &["task", "show", "inspect", "snapshot"],
        scope: ActionScope::Task,
        required_capability: None,
        risk: ActionRisk::ReadOnly,
        argument_schema: ActionArgumentSchema::TaskId,
    },
    ActionDescriptor {
        id: ACTION_TASK_CREATE_V2,
        title: "Create task (workspace request)",
        description:
            "Create one Task after the host resolves a workspace request against a project root.",
        keywords: &["task", "create", "workspace", "worktree", "new"],
        scope: ActionScope::Host,
        required_capability: None,
        risk: ActionRisk::Mutating,
        argument_schema: ActionArgumentSchema::TaskCreateV2,
    },
    ActionDescriptor {
        id: ACTION_TASK_RENAME,
        title: "Rename task",
        description: "Rename one Task through the host command boundary.",
        keywords: &["task", "rename", "title", "edit"],
        scope: ActionScope::Task,
        required_capability: None,
        risk: ActionRisk::Mutating,
        argument_schema: ActionArgumentSchema::TaskRenameV1,
    },
    ActionDescriptor {
        id: ACTION_SERVICE_START,
        title: "Start service",
        description: "Start one configured command through the managed service supervisor.",
        keywords: &["service", "start", "command", "server"],
        scope: ActionScope::Host,
        required_capability: None,
        risk: ActionRisk::Mutating,
        argument_schema: ActionArgumentSchema::ServiceControlV1,
    },
    ActionDescriptor {
        id: ACTION_SERVICE_STOP,
        title: "Stop service",
        description: "Stop one managed configured command through the service supervisor.",
        keywords: &["service", "stop", "command", "server"],
        scope: ActionScope::Host,
        required_capability: None,
        risk: ActionRisk::Mutating,
        argument_schema: ActionArgumentSchema::ServiceControlV1,
    },
    ActionDescriptor {
        id: ACTION_SERVICE_RESTART,
        title: "Restart service",
        description: "Restart one managed configured command through the service supervisor.",
        keywords: &["service", "restart", "command", "server"],
        scope: ActionScope::Host,
        required_capability: None,
        risk: ActionRisk::Mutating,
        argument_schema: ActionArgumentSchema::ServiceControlV1,
    },
    ActionDescriptor {
        id: ACTION_SERVICE_LOGS,
        title: "Service logs",
        description: "Read bounded redacted logs for one configured service.",
        keywords: &["service", "logs", "output"],
        scope: ActionScope::Host,
        required_capability: None,
        risk: ActionRisk::ReadOnly,
        argument_schema: ActionArgumentSchema::ServiceControlV1,
    },
    ActionDescriptor {
        id: ACTION_SERVICE_HEALTH,
        title: "Service health",
        description: "Read the last redacted health snapshot for one configured service.",
        keywords: &["service", "health", "probe"],
        scope: ActionScope::Host,
        required_capability: None,
        risk: ActionRisk::ReadOnly,
        argument_schema: ActionArgumentSchema::ServiceControlV1,
    },
];

/// Frozen V1 `task.create` arguments. The workspace is already durable; the
/// host transport rejects raw untrusted CreateTask intents, while trusted
/// local callers can continue to use this stable action shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskCreateArguments {
    pub task_id: TaskId,
    pub environment_id: EnvironmentId,
    pub title: String,
    pub description: Option<String>,
    pub project_id: ProjectId,
    pub workspace: WorkspaceRef,
}

/// V2 request-shaped task creation. `WorkspaceRequest` is disposable and is
/// normalized to a durable reference only inside the authenticated host. The
/// host selects the project root from its own ProjectId configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskCreateV2Arguments {
    pub task_id: TaskId,
    pub environment_id: EnvironmentId,
    pub title: String,
    pub description: Option<String>,
    pub project_id: ProjectId,
    pub workspace: WorkspaceRequest,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskCreateError {
    Validation(TaskValidationError),
    Workspace(WorkspaceError),
}

impl std::fmt::Display for TaskCreateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Validation(error) => error.fmt(f),
            Self::Workspace(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for TaskCreateError {}

impl From<TaskValidationError> for TaskCreateError {
    fn from(error: TaskValidationError) -> Self {
        Self::Validation(error)
    }
}

/// Caller-owned `task.rename` arguments validated before transport.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskRenameArguments {
    pub task_id: TaskId,
    pub title: String,
}

/// Caller-owned configured-service action arguments. The host supervisor
/// admits these against the exact generation fence; they are not a durable
/// journal command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceControlArguments {
    pub service_id: ServiceId,
    pub resource_generation: u64,
    pub connection_epoch: u64,
    pub action_epoch: u64,
}

/// Return the closed catalog for this slice.
pub fn catalog() -> &'static [ActionDescriptor] {
    ACTIONS
}

/// Fail when two descriptors share a stable id.
pub fn require_unique_ids() -> Result<(), String> {
    let mut seen = Vec::new();
    for action in catalog() {
        if seen.contains(&action.id) {
            return Err(format!("duplicate action id: {}", action.id));
        }
        seen.push(action.id);
    }
    Ok(())
}

/// Build the shared side-effect-free request for `task.show`.
pub fn task_show_query(
    request_id: RequestId,
    client_id: ClientId,
    task_id: TaskId,
) -> QueryEnvelope {
    QueryEnvelope {
        request_id,
        client_id,
        task_id: Some(task_id),
        query: Query::TaskSnapshot,
    }
}

/// Build the shared `task.create` mutation after content canonicalization and
/// workspace resolution.
pub fn task_create_command(
    command_id: CommandId,
    client_id: ClientId,
    issued_at_ms: i64,
    args: TaskCreateArguments,
) -> Result<CommandEnvelope, TaskCreateError> {
    let title = TaskFacts::canonicalize_title(args.title)?;
    let description = TaskFacts::canonicalize_description(args.description)?;
    args.workspace.validate()?;
    Ok(CommandEnvelope {
        command_id,
        client_id,
        task_id: None,
        issued_at_ms,
        expected_task_revision: None,
        command: Command::CreateTask(CreateTaskIntent {
            id: args.task_id,
            environment_id: args.environment_id,
            title,
            description,
            project_id: args.project_id,
            workspace: args.workspace,
            assignment: TaskAssignment::LocalOwner,
            created_at_ms: issued_at_ms,
            connectivity: TaskConnectivity::Connected,
            attention: TaskAttention::None,
            activity: TaskActivity::Idle,
            review_readiness: ReviewReadiness::NotReady,
        }),
    })
}

/// Build the truthful V2 request. No durable workspace or project path is
/// produced here; the authenticated host resolves the request against its
/// ProjectId configuration.
pub fn task_create_v2_command(
    command_id: CommandId,
    client_id: ClientId,
    issued_at_ms: i64,
    args: TaskCreateV2Arguments,
) -> Result<CommandEnvelope, TaskCreateError> {
    let title = TaskFacts::canonicalize_title(args.title)?;
    let description = TaskFacts::canonicalize_description(args.description)?;
    Ok(CommandEnvelope {
        command_id,
        client_id,
        task_id: None,
        issued_at_ms,
        expected_task_revision: None,
        command: Command::CreateTaskV2(CreateTaskRequestIntent {
            id: args.task_id,
            environment_id: args.environment_id,
            title,
            description,
            project_id: args.project_id,
            workspace: args.workspace,
            assignment: TaskAssignment::LocalOwner,
            created_at_ms: issued_at_ms,
            connectivity: TaskConnectivity::Connected,
            attention: TaskAttention::None,
            activity: TaskActivity::Idle,
            review_readiness: ReviewReadiness::NotReady,
        }),
    })
}

/// Build the shared `task.rename` mutation after local canonicalization.
pub fn task_rename_command(
    command_id: CommandId,
    client_id: ClientId,
    issued_at_ms: i64,
    expected_task_revision: u64,
    args: TaskRenameArguments,
) -> Result<CommandEnvelope, TaskValidationError> {
    let title = TaskFacts::canonicalize_title(args.title)?;
    Ok(CommandEnvelope {
        command_id,
        client_id,
        task_id: Some(args.task_id),
        issued_at_ms,
        expected_task_revision: Some(expected_task_revision),
        command: Command::RenameTask(RenameTaskIntent { title }),
    })
}

#[cfg(test)]
mod tests {
    use super::{
        catalog, require_unique_ids, task_create_command, task_rename_command, task_show_query,
        ActionArgumentSchema, ActionRisk, ActionScope, TaskCreateArguments, TaskCreateV2Arguments,
        TaskRenameArguments, ACTION_HOST_ACTIONS, ACTION_HOST_STATUS, ACTION_SERVICE_HEALTH,
        ACTION_SERVICE_LOGS, ACTION_SERVICE_RESTART, ACTION_SERVICE_START, ACTION_SERVICE_STOP,
        ACTION_TASK_CREATE, ACTION_TASK_CREATE_V2, ACTION_TASK_LIST, ACTION_TASK_RENAME,
        ACTION_TASK_SHOW,
    };
    use crate::{
        domain::{
            command::Command,
            query::Query,
            task::{
                ReviewReadiness, TaskActivity, TaskAssignment, TaskAttention, TaskConnectivity,
                WorkspaceRef,
            },
            ClientId, CommandId, EnvironmentId, ProjectId, RequestId, TaskId,
        },
        protocol::Capability,
    };

    #[test]
    fn catalog_exposes_unique_read_and_create_actions() {
        let ids: Vec<&str> = catalog().iter().map(|action| action.id).collect();
        assert!(ids.contains(&ACTION_HOST_ACTIONS));
        assert!(ids.contains(&ACTION_HOST_STATUS));
        assert!(ids.contains(&ACTION_TASK_LIST));
        assert!(ids.contains(&ACTION_TASK_SHOW));
        assert!(!ids.contains(&ACTION_TASK_CREATE));
        assert!(ids.contains(&ACTION_TASK_RENAME));
        assert!(ids.contains(&ACTION_SERVICE_START));
        assert!(ids.contains(&ACTION_SERVICE_STOP));
        assert!(ids.contains(&ACTION_SERVICE_RESTART));
        assert!(ids.contains(&ACTION_SERVICE_LOGS));
        assert!(ids.contains(&ACTION_SERVICE_HEALTH));
        assert_eq!(ids.len(), 11);
        require_unique_ids().expect("ids must be unique");
        for action in catalog() {
            let (expected_scope, expected_risk, expected_schema, expected_capability) =
                match action.id {
                    ACTION_TASK_LIST => (
                        ActionScope::Host,
                        ActionRisk::ReadOnly,
                        ActionArgumentSchema::None,
                        Some(Capability::PagedSnapshots),
                    ),
                    ACTION_TASK_SHOW => (
                        ActionScope::Task,
                        ActionRisk::ReadOnly,
                        ActionArgumentSchema::TaskId,
                        None,
                    ),
                    ACTION_TASK_CREATE_V2 => (
                        ActionScope::Host,
                        ActionRisk::Mutating,
                        ActionArgumentSchema::TaskCreateV2,
                        None,
                    ),
                    ACTION_TASK_RENAME => (
                        ActionScope::Task,
                        ActionRisk::Mutating,
                        ActionArgumentSchema::TaskRenameV1,
                        None,
                    ),
                    ACTION_SERVICE_START | ACTION_SERVICE_STOP | ACTION_SERVICE_RESTART => (
                        ActionScope::Host,
                        ActionRisk::Mutating,
                        ActionArgumentSchema::ServiceControlV1,
                        None,
                    ),
                    ACTION_SERVICE_LOGS | ACTION_SERVICE_HEALTH => (
                        ActionScope::Host,
                        ActionRisk::ReadOnly,
                        ActionArgumentSchema::ServiceControlV1,
                        None,
                    ),
                    _ => (
                        ActionScope::Host,
                        ActionRisk::ReadOnly,
                        ActionArgumentSchema::None,
                        None,
                    ),
                };
            assert_eq!(action.scope, expected_scope);
            assert_eq!(action.risk, expected_risk);
            assert_eq!(action.argument_schema, expected_schema);
            assert_eq!(action.required_capability, expected_capability);
        }
    }

    #[test]
    fn task_show_factory_binds_client_request_and_task_scope() {
        let request_id = RequestId::new();
        let client_id = ClientId::new();
        let task_id = TaskId::new();
        let query = task_show_query(request_id, client_id, task_id);
        assert_eq!(query.request_id, request_id);
        assert_eq!(query.client_id, client_id);
        assert_eq!(query.task_id, Some(task_id));
        assert_eq!(query.query, Query::TaskSnapshot);
    }

    #[test]
    fn task_create_factory_builds_the_canonical_default_command() {
        let command_id = CommandId::new();
        let client_id = ClientId::new();
        let task_id = TaskId::new();
        let environment_id = EnvironmentId::new();
        let project_id = ProjectId::new();
        let issued_at_ms = 1_725_000_000_100;
        let envelope = task_create_command(
            command_id,
            client_id,
            issued_at_ms,
            TaskCreateArguments {
                task_id,
                environment_id,
                title: "New Task".into(),
                description: Some("Created through the shared action".into()),
                project_id,
                workspace: WorkspaceRef::Main,
            },
        )
        .expect("valid task.create arguments");

        assert_eq!(envelope.command_id, command_id);
        assert_eq!(envelope.client_id, client_id);
        assert_eq!(envelope.task_id, None);
        assert_eq!(envelope.issued_at_ms, issued_at_ms);
        assert_eq!(envelope.expected_task_revision, None);
        let Command::CreateTask(intent) = envelope.command else {
            panic!("task.create must build CreateTask");
        };
        assert_eq!(intent.id, task_id);
        assert_eq!(intent.environment_id, environment_id);
        assert_eq!(intent.title, "New Task");
        assert_eq!(
            intent.description.as_deref(),
            Some("Created through the shared action")
        );
        assert_eq!(intent.project_id, project_id);
        assert_eq!(intent.workspace, WorkspaceRef::Main);
        assert_eq!(intent.assignment, TaskAssignment::LocalOwner);
        assert_eq!(intent.created_at_ms, issued_at_ms);
        assert_eq!(intent.connectivity, TaskConnectivity::Connected);
        assert_eq!(intent.attention, TaskAttention::None);
        assert_eq!(intent.activity, TaskActivity::Idle);
        assert_eq!(intent.review_readiness, ReviewReadiness::NotReady);
    }

    #[test]
    fn task_create_factory_rejects_invalid_content_before_transport() {
        let result = task_create_command(
            CommandId::new(),
            ClientId::new(),
            1_725_000_000_100,
            TaskCreateArguments {
                task_id: TaskId::new(),
                environment_id: EnvironmentId::new(),
                title: "   ".into(),
                description: None,
                project_id: ProjectId::new(),
                workspace: WorkspaceRef::Main,
            },
        );
        assert!(result.is_err(), "blank titles must fail before transport");
    }

    #[test]
    fn frozen_task_create_v1_arguments_still_accept_the_durable_workspace_shape() {
        let value = serde_json::json!({
            "task_id": TaskId::new(),
            "environment_id": EnvironmentId::new(),
            "title": "Frozen V1 task",
            "description": null,
            "project_id": ProjectId::new(),
            "workspace": "main"
        });

        let result = serde_json::from_value::<TaskCreateArguments>(value);
        assert!(
            result.is_ok(),
            "TaskCreateV1 must remain decodable: {}",
            result.expect_err("expected the current shape to fail")
        );
    }

    #[test]
    fn task_create_v2_arguments_reject_a_client_supplied_project_root() {
        let mut value = serde_json::json!({
            "task_id": TaskId::new(),
            "environment_id": EnvironmentId::new(),
            "title": "Host-owned project root",
            "description": null,
            "project_id": ProjectId::new(),
            "project_root": "C:/client-selected-root",
            "workspace": {
                "choice": "main",
                "path": null,
                "branch": null,
                "external_confirmed": false
            }
        });

        assert!(
            serde_json::from_value::<TaskCreateV2Arguments>(value.clone()).is_err(),
            "V2 must not decode a client-selected project root"
        );

        value
            .as_object_mut()
            .expect("V2 arguments object")
            .remove("project_root");
        serde_json::from_value::<TaskCreateV2Arguments>(value)
            .expect("V2 must decode without a client project root");
    }

    #[test]
    fn task_rename_factory_binds_task_and_expected_revision() {
        let command_id = CommandId::new();
        let client_id = ClientId::new();
        let task_id = TaskId::new();
        let envelope = task_rename_command(
            command_id,
            client_id,
            1_725_000_000_100,
            7,
            TaskRenameArguments {
                task_id,
                title: "Renamed Task".into(),
            },
        )
        .expect("valid task.rename arguments");

        assert_eq!(envelope.command_id, command_id);
        assert_eq!(envelope.client_id, client_id);
        assert_eq!(envelope.task_id, Some(task_id));
        assert_eq!(envelope.expected_task_revision, Some(7));
        let Command::RenameTask(intent) = envelope.command else {
            panic!("task.rename must build RenameTask");
        };
        assert_eq!(intent.title, "Renamed Task");
    }

    #[test]
    fn task_rename_factory_rejects_blank_title_before_transport() {
        let result = task_rename_command(
            CommandId::new(),
            ClientId::new(),
            1_725_000_000_100,
            1,
            TaskRenameArguments {
                task_id: TaskId::new(),
                title: "  ".into(),
            },
        );
        assert!(
            result.is_err(),
            "blank rename titles must fail before transport"
        );
    }
}
