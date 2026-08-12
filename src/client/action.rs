//! Shared action catalog for CLI and future GPUI clients.
//!
//! This slice exposes `host.actions`, `host.status`, `task.list`, `task.show`,
//! `task.create`, and `task.rename`. It is intentionally not a dynamic
//! plugin framework.

use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

use crate::domain::command::{Command, CommandEnvelope, CreateTaskIntent, RenameTaskIntent};
use crate::domain::query::{Query, QueryEnvelope};
use crate::domain::task::{
    ReviewReadiness, TaskActivity, TaskAssignment, TaskAttention, TaskConnectivity, TaskFacts,
    TaskValidationError, WorkspaceRef,
};
use crate::domain::{ClientId, CommandId, EnvironmentId, ProjectId, RequestId, TaskId};
use crate::prompts::projection::{
    OwnerDeviceCapability, PromptCursor, PromptLibraryRequest, PromptNamespace,
    PromptProjectionError,
};
use crate::protocol::{Capability, CapabilitySet};

/// Stable id for listing the shared action catalog.
pub const ACTION_HOST_ACTIONS: &str = "host.actions";
/// Stable id for attaching and reporting host status.
pub const ACTION_HOST_STATUS: &str = "host.status";
/// Stable id for listing Tasks through the paged snapshot query boundary.
pub const ACTION_TASK_LIST: &str = "task.list";
/// Stable id for reading one Task through the host query boundary.
pub const ACTION_TASK_SHOW: &str = "task.show";
/// Stable id for creating one Task through the host command boundary.
pub const ACTION_TASK_CREATE: &str = "task.create";
/// Stable id for renaming one Task through the host command boundary.
pub const ACTION_TASK_RENAME: &str = "task.rename";
/// Read-only host query for a personal prompt metadata page.
pub const ACTION_PROMPT_METADATA_PAGE: &str = "prompt.library.metadata_page";
/// Read-only host query for an exact immutable prompt version page.
pub const ACTION_PROMPT_VERSION_PAGE: &str = "prompt.library.version_page";
/// Read-only host query for a bounded prompt version diff page.
pub const ACTION_PROMPT_DIFF: &str = "prompt.library.diff";
/// Read-only host query for a bounded personal prompt search page.
pub const ACTION_PROMPT_SEARCH_PAGE: &str = "prompt.library.search_page";
/// Read-only host query for a linear prompt chain page.
pub const ACTION_PROMPT_CHAIN_PAGE: &str = "prompt.library.chain_page";
/// Read-only host query for a personal prompt history page.
pub const ACTION_PROMPT_HISTORY_PAGE: &str = "prompt.library.history_page";

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
    TaskRenameV1,
    PromptMetadataPageV1,
    PromptVersionPageV1,
    PromptDiffV1,
    PromptChainPageV1,
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
        id: ACTION_TASK_CREATE,
        title: "Create task",
        description: "Create one Task through the host command boundary.",
        keywords: &["task", "create", "new", "add"],
        scope: ActionScope::Host,
        required_capability: None,
        risk: ActionRisk::Mutating,
        argument_schema: ActionArgumentSchema::TaskCreateV1,
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
];

/// Caller-owned `task.create` arguments validated before transport.
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

/// Caller-owned `task.rename` arguments validated before transport.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskRenameArguments {
    pub task_id: TaskId,
    pub title: String,
}

/// Return the single host action registry for this slice.
pub fn catalog() -> &'static [ActionDescriptor] {
    static CATALOG: OnceLock<Vec<ActionDescriptor>> = OnceLock::new();
    CATALOG
        .get_or_init(|| {
            let mut entries = Vec::with_capacity(ACTIONS.len() + PROMPT_LIBRARY_EXTENSION.len());
            entries.extend_from_slice(ACTIONS);
            entries.extend_from_slice(PROMPT_LIBRARY_EXTENSION);
            entries
        })
        .as_slice()
}

const PROMPT_LIBRARY_EXTENSION: &[ActionDescriptor] = &[
    ActionDescriptor {
        id: ACTION_PROMPT_METADATA_PAGE,
        title: "Prompt metadata page",
        description: "Page personal or organization prompt metadata without bodies.",
        keywords: &["prompt", "library", "metadata", "page"],
        scope: ActionScope::Host,
        required_capability: Some(Capability::PromptProjection),
        risk: ActionRisk::ReadOnly,
        argument_schema: ActionArgumentSchema::PromptMetadataPageV1,
    },
    ActionDescriptor {
        id: ACTION_PROMPT_VERSION_PAGE,
        title: "Prompt version page",
        description: "Read one exact immutable prompt version through bounded chunks.",
        keywords: &["prompt", "version", "chunk"],
        scope: ActionScope::Host,
        required_capability: Some(Capability::PromptProjection),
        risk: ActionRisk::ReadOnly,
        argument_schema: ActionArgumentSchema::PromptVersionPageV1,
    },
    ActionDescriptor {
        id: ACTION_PROMPT_DIFF,
        title: "Prompt version diff",
        description: "Read a bounded public diff of two immutable prompt versions.",
        keywords: &["prompt", "diff", "version"],
        scope: ActionScope::Host,
        required_capability: Some(Capability::PromptProjection),
        risk: ActionRisk::ReadOnly,
        argument_schema: ActionArgumentSchema::PromptDiffV1,
    },
    ActionDescriptor {
        id: ACTION_PROMPT_CHAIN_PAGE,
        title: "Prompt chain page",
        description: "Page a linear exact-version prompt chain.",
        keywords: &["prompt", "chain", "library"],
        scope: ActionScope::Host,
        required_capability: Some(Capability::PromptProjection),
        risk: ActionRisk::ReadOnly,
        argument_schema: ActionArgumentSchema::PromptChainPageV1,
    },
];

/// Single registry consumed by uniqueness and lookup.
pub fn registered_actions() -> impl Iterator<Item = &'static ActionDescriptor> {
    catalog().iter()
}

/// Truthful disabled reason for a catalog id under the negotiated grant set.
///
/// Callers must treat a missing catalog `disabled_reason` key as disabled.
/// Only an explicit JSON `null` together with `enabled: true` may enable.
pub fn disabled_reason(id: &str, granted: CapabilitySet) -> Option<&'static str> {
    let Some(action) = action_by_id(id) else {
        return Some("unknown action");
    };
    if let Some(required) = action.required_capability {
        if !granted.contains(required) {
            return Some(match required {
                Capability::PromptProjection => "personal_prompt_library capability not granted",
                _ => "required capability not granted",
            });
        }
    }
    match id {
        ACTION_PROMPT_METADATA_PAGE
        | ACTION_PROMPT_VERSION_PAGE
        | ACTION_PROMPT_DIFF
        | ACTION_PROMPT_CHAIN_PAGE => {
            Some("owner_device_session unavailable until Phase 9 authenticated pairing")
        }
        _ => None,
    }
}

/// Fail-closed enablement: omission of `disabled_reason` must not enable.
pub fn action_enabled(id: &str, granted: CapabilitySet) -> bool {
    action_by_id(id).is_some() && disabled_reason(id, granted).is_none()
}

pub fn action_by_id(id: &str) -> Option<&'static ActionDescriptor> {
    registered_actions().find(|action| action.id == id)
}

pub fn prompt_metadata_page_request(
    request_id: RequestId,
    client_id: ClientId,
    capability: &OwnerDeviceCapability,
    expected_library_revision: Option<u64>,
    cursor: Option<PromptCursor>,
) -> Result<PromptLibraryRequest, PromptProjectionError> {
    PromptLibraryRequest::metadata_page(
        request_id,
        client_id,
        capability,
        PromptNamespace::Personal,
        expected_library_revision,
        cursor,
    )
}

/// Fail when two descriptors share a stable id.
pub fn require_unique_ids() -> Result<(), String> {
    let mut seen = Vec::new();
    for action in registered_actions() {
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

/// Build the shared `task.create` mutation after local canonicalization.
pub fn task_create_command(
    command_id: CommandId,
    client_id: ClientId,
    issued_at_ms: i64,
    args: TaskCreateArguments,
) -> Result<CommandEnvelope, TaskValidationError> {
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
        ActionArgumentSchema, ActionRisk, ActionScope, TaskCreateArguments, TaskRenameArguments,
        ACTION_HOST_ACTIONS, ACTION_HOST_STATUS, ACTION_PROMPT_CHAIN_PAGE, ACTION_PROMPT_DIFF,
        ACTION_PROMPT_METADATA_PAGE, ACTION_PROMPT_VERSION_PAGE, ACTION_TASK_CREATE,
        ACTION_TASK_LIST, ACTION_TASK_RENAME, ACTION_TASK_SHOW,
    };
    use crate::domain::command::Command;
    use crate::domain::query::Query;
    use crate::domain::task::{
        ReviewReadiness, TaskActivity, TaskAssignment, TaskAttention, TaskConnectivity,
        WorkspaceRef,
    };
    use crate::domain::{ClientId, CommandId, EnvironmentId, ProjectId, RequestId, TaskId};
    use crate::protocol::Capability;

    #[test]
    fn catalog_exposes_unique_read_and_create_actions() {
        let ids: Vec<&str> = catalog().iter().map(|action| action.id).collect();
        assert!(ids.contains(&ACTION_HOST_ACTIONS));
        assert!(ids.contains(&ACTION_HOST_STATUS));
        assert!(ids.contains(&ACTION_TASK_LIST));
        assert!(ids.contains(&ACTION_TASK_SHOW));
        assert!(ids.contains(&ACTION_TASK_CREATE));
        assert!(ids.contains(&ACTION_TASK_RENAME));
        assert_eq!(ids.len(), catalog().len());
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
                    ACTION_TASK_CREATE => (
                        ActionScope::Host,
                        ActionRisk::Mutating,
                        ActionArgumentSchema::TaskCreateV1,
                        None,
                    ),
                    ACTION_TASK_RENAME => (
                        ActionScope::Task,
                        ActionRisk::Mutating,
                        ActionArgumentSchema::TaskRenameV1,
                        None,
                    ),
                    ACTION_PROMPT_METADATA_PAGE => (
                        ActionScope::Host,
                        ActionRisk::ReadOnly,
                        ActionArgumentSchema::PromptMetadataPageV1,
                        Some(Capability::PromptProjection),
                    ),
                    ACTION_PROMPT_VERSION_PAGE => (
                        ActionScope::Host,
                        ActionRisk::ReadOnly,
                        ActionArgumentSchema::PromptVersionPageV1,
                        Some(Capability::PromptProjection),
                    ),
                    ACTION_PROMPT_DIFF => (
                        ActionScope::Host,
                        ActionRisk::ReadOnly,
                        ActionArgumentSchema::PromptDiffV1,
                        Some(Capability::PromptProjection),
                    ),
                    ACTION_PROMPT_CHAIN_PAGE => (
                        ActionScope::Host,
                        ActionRisk::ReadOnly,
                        ActionArgumentSchema::PromptChainPageV1,
                        Some(Capability::PromptProjection),
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
