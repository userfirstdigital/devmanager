//! Shared action catalog for CLI and future GPUI clients.
//!
//! This slice exposes `host.actions`, `host.status`, `task.list`, `task.show`,
//! `task.create`, `task.rename`, and provider input/turn controls. It is
//! intentionally not a dynamic plugin framework. Configured service
//! start/stop/restart actions are capability gated and only enabled after the
//! host supervisor is ready.

use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

use crate::browser::BrowserNativeHostCommand;
use crate::domain::cockpit::{TaskCockpitQuery, MAX_COCKPIT_FILE_LIST};
use crate::domain::command::{
    Command, CommandEnvelope, CreateTaskIntent, CreateTaskRequestIntent, ProviderStartMode,
    RenameTaskIntent, ServiceControlAction, ServiceControlIntent, StartProviderSessionIntent,
    SubmitProviderInputIntent,
};
use crate::domain::id::{
    AgentSessionId, ConfiguredServiceId, PromptChainId, PromptVersionId, ResourceId,
};
use crate::domain::provider_input::{
    validate_provider_images, validate_send_now_payload, ProviderImageAttachment,
    ProviderInputAction, ProviderInputIntentError,
};
use crate::domain::query::{Query, QueryEnvelope};
use crate::domain::task::{
    ReviewReadiness, TaskActivity, TaskAssignment, TaskAttention, TaskConnectivity, TaskFacts,
    TaskValidationError, WorkspaceRef,
};
use crate::domain::{
    ApprovalId, ClientId, CommandId, EnvironmentId, ProjectId, QuestionId, RequestId, TaskId,
    TurnId,
};
use crate::prompts::projection::{
    OwnerDeviceCapability, PromptCursor, PromptLibraryQuery, PromptLibraryRequest, PromptNamespace,
    PromptProjectionError,
};
use crate::prompts::ui::composer::{ComposerInsertionMode, PutPromptVersionInComposer};
use crate::protocol::{Capability, CapabilitySet};
use crate::providers::ProviderKind;
use crate::services::model::ServiceId;
use crate::workspace::{WorkspaceError, WorkspaceRequest};

/// Stable id for listing the shared action catalog.
pub const ACTION_HOST_ACTIONS: &str = "host.actions";
/// Stable id for attaching and reporting host status.
pub const ACTION_HOST_STATUS: &str = "host.status";
/// Native Task Cockpit browser surface command. The command already carries
/// the controller-issued identity and lease; this action never accepts raw
/// HWNDs or an unbound Task id from presentation code.
pub const ACTION_BROWSER_NATIVE: &str = "browser.native";
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
/// Stable id for host-owned project creation from a name and folder path.
pub const ACTION_CONFIG_CREATE_PROJECT: &str = "config.create_project";
pub const ACTION_CONFIG_UPSERT_COMMAND: &str = "config.upsert_command";
pub const ACTION_CONFIG_ARCHIVE_COMMAND: &str = "config.archive_command";
pub const ACTION_CONFIG_RUN_COMMAND: &str = "config.run_command";
pub const ACTION_CONFIG_COMMAND_DETAIL: &str = "config.command_detail";
/// Stable id for renaming one Task through the host command boundary.
pub const ACTION_TASK_RENAME: &str = "task.rename";
/// Mark a Task Done without releasing its runtime or provider conversation.
pub const ACTION_TASK_SETTLE: &str = "task.settle";
/// Restore a Done Task to the active list.
pub const ACTION_TASK_REOPEN: &str = "task.reopen";
/// Stable id for archiving one Task through the host close boundary.
pub const ACTION_TASK_ARCHIVE: &str = "task.archive";
/// Permanently hide an already archived Task while retaining its durable tombstone.
pub const ACTION_TASK_DELETE: &str = "task.delete";
/// Canonical composer Send Now. Settles through SubmitProviderInput.
pub const ACTION_TASK_SEND_NOW: &str = "task.send_now";
/// Canonical composer steer. Settles through SubmitProviderInput.
pub const ACTION_TASK_STEER_CURRENT_TURN: &str = "task.steer_current_turn";
/// Canonical composer queue follow-up. Settles through SubmitProviderInput.
pub const ACTION_TASK_QUEUE_FOLLOW_UP: &str = "task.queue_follow_up";
/// Canonical composer answer. Settles through SubmitProviderInput.
pub const ACTION_TASK_ANSWER_QUESTION: &str = "task.answer_question";
/// Canonical composer approval. Settles through SubmitProviderInput.
pub const ACTION_TASK_RESOLVE_APPROVAL: &str = "task.resolve_approval";
/// Canonical composer stop. Settles through SubmitProviderInput.
pub const ACTION_TASK_STOP_TURN: &str = "task.stop_turn";
/// Reserved Phase 4.7 id. Not registered in `ACTIONS` until the host command exists.
pub const ACTION_TASK_SAVE_COMPOSER_DRAFT: &str = "task.save_composer_draft";
/// Reserved Phase 4.7 id. Not registered in `ACTIONS` until the host command exists.
pub const ACTION_TASK_STAGE_COMPOSER_ATTACHMENT: &str = "task.stage_composer_attachment";
/// Reserved Phase 4.7 id. Not registered in `ACTIONS` until the host command exists.
pub const ACTION_TASK_REMOVE_COMPOSER_ATTACHMENT: &str = "task.remove_composer_attachment";
/// Stable id for sending provider input on the current turn.
pub const ACTION_PROVIDER_SEND_NOW: &str = crate::providers::input::ACTION_PROVIDER_SEND_NOW;
/// Stable id for steering the current provider turn.
pub const ACTION_PROVIDER_STEER_CURRENT_TURN: &str =
    crate::providers::input::ACTION_PROVIDER_STEER_CURRENT_TURN;
/// Stable id for queueing a follow-up after the current turn.
pub const ACTION_PROVIDER_QUEUE_FOLLOW_UP: &str =
    crate::providers::input::ACTION_PROVIDER_QUEUE_FOLLOW_UP;
/// Stable id for answering an exact provider question.
pub const ACTION_PROVIDER_ANSWER_QUESTION: &str =
    crate::providers::input::ACTION_PROVIDER_ANSWER_QUESTION;
/// Stable id for resolving an exact provider approval.
pub const ACTION_PROVIDER_RESOLVE_APPROVAL: &str =
    crate::providers::input::ACTION_PROVIDER_RESOLVE_APPROVAL;
/// Stable id for stopping the current provider turn.
pub const ACTION_PROVIDER_STOP_TURN: &str = crate::providers::input::ACTION_PROVIDER_STOP_TURN;
/// Stable id for interactive input to the current provider terminal.
pub const ACTION_PROVIDER_TERMINAL_INPUT: &str =
    crate::providers::input::ACTION_PROVIDER_TERMINAL_INPUT;
/// Stable id for starting a new AgentSession identity. Not Restart.
pub const ACTION_PROVIDER_NEW_CONVERSATION: &str =
    crate::providers::input::ACTION_PROVIDER_NEW_CONVERSATION;
/// Task Cockpit action for starting one exact task-owned stock provider.
pub const ACTION_PROVIDER_START_SESSION: &str = "provider.start_session";
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
/// Client-local composer insertion. Not a host catalog action and never sends.
pub const ACTION_PROMPT_PUT_IN_COMPOSER: &str = "prompt.library.put_in_composer";
/// Stable control ids for the configured-service supervisor. The host only
/// grants their required capability after its one supervisor is initialized.
pub const ACTION_SERVICE_START: &str = "service.start";
/// Stable stop id; capability-gated by the host supervisor.
pub const ACTION_SERVICE_STOP: &str = "service.stop";
/// Stable restart id; capability-gated by the host supervisor.
pub const ACTION_SERVICE_RESTART: &str = "service.restart";
/// Typed Task Cockpit logs query through the configured-service supervisor.
pub const ACTION_SERVICE_LOGS: &str = "service.logs";
/// Typed Task Cockpit health query through the configured-service supervisor.
pub const ACTION_SERVICE_HEALTH: &str = "service.health";
/// Task-scoped semantic conversation projection.
pub const ACTION_CONVERSATION_STATUS: &str = "conversation.status";
/// Task-scoped workspace identity projection.
pub const ACTION_WORKSPACE_STATUS: &str = "workspace.status";
/// Task-scoped durable git identity/status projection.
pub const ACTION_GIT_STATUS: &str = "git.status";
/// Task-scoped bounded repository catalog for multi-folder projects.
pub const ACTION_GIT_REPOSITORIES: &str = "git.repositories";
/// Task-scoped files list query through WorkspaceFileService authority.
pub const ACTION_FILES_LIST: &str = "files.list";
/// Task-scoped files read query through WorkspaceFileService authority.
pub const ACTION_FILES_READ: &str = "files.read";
/// Reserved files write; stays unpublished until a safe write authority exists.
pub const ACTION_FILES_WRITE: &str = "files.write";
/// Task-scoped SSH status query through host SSH catalog authority.
pub const ACTION_SSH_STATUS: &str = "ssh.status";
/// Typed SSH action query. Not advertised until a Task supervisor adapter exists.
pub const ACTION_SSH_ACTION: &str = "ssh.action";
/// Reserved git mutate; stays unpublished until GitHostBinding is issued.
pub const ACTION_GIT_MUTATE: &str = "git.mutate";
/// Local updater background-check arm. Never installs or launches.
pub const ACTION_UPDATER_START_BACKGROUND: &str = "updater.start_background";
/// Local updater freshness check. Never installs or launches.
pub const ACTION_UPDATER_CHECK: &str = "updater.check";
/// Local updater download. Tests must not invoke install.
pub const ACTION_UPDATER_DOWNLOAD: &str = "updater.download";
/// Local updater install. Native tests must not dispatch this action.
pub const ACTION_UPDATER_INSTALL: &str = "updater.install";

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
    ProviderInputV1,
    PromptMetadataPageV1,
    PromptVersionPageV1,
    PromptDiffV1,
    PromptChainPageV1,
    ServiceControlV1,
    TaskCockpitV1,
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
        id: ACTION_BROWSER_NATIVE,
        title: "Browser surface",
        description:
            "Attach, resize, focus, submit, or detach the exact Task-owned browser surface.",
        keywords: &["browser", "webview", "surface", "attach"],
        scope: ActionScope::Task,
        required_capability: Some(Capability::BrowserProjection),
        risk: ActionRisk::Mutating,
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
        id: ACTION_CONFIG_CREATE_PROJECT,
        title: "Add project",
        description: "Add one project folder through the host configuration store.",
        keywords: &["project", "add", "folder", "workspace", "config"],
        scope: ActionScope::Host,
        required_capability: Some(Capability::TaskCockpit),
        risk: ActionRisk::Mutating,
        argument_schema: ActionArgumentSchema::TaskCockpitV1,
    },
    ActionDescriptor {
        id: ACTION_CONFIG_UPSERT_COMMAND,
        title: "Save project action",
        description: "Create or update one project command through the host configuration store.",
        keywords: &["project", "action", "command", "script", "config"],
        scope: ActionScope::Host,
        required_capability: Some(Capability::TaskCockpit),
        risk: ActionRisk::Mutating,
        argument_schema: ActionArgumentSchema::TaskCockpitV1,
    },
    ActionDescriptor {
        id: ACTION_CONFIG_ARCHIVE_COMMAND,
        title: "Archive project action",
        description: "Archive one project command through the host configuration store.",
        keywords: &["project", "action", "command", "archive", "config"],
        scope: ActionScope::Host,
        required_capability: Some(Capability::TaskCockpit),
        risk: ActionRisk::Mutating,
        argument_schema: ActionArgumentSchema::TaskCockpitV1,
    },
    ActionDescriptor {
        id: ACTION_CONFIG_RUN_COMMAND,
        title: "Run project action",
        description: "Start one configured project command through the host service runtime.",
        keywords: &["project", "action", "command", "run", "start"],
        scope: ActionScope::Host,
        required_capability: Some(Capability::TaskCockpit),
        risk: ActionRisk::Mutating,
        argument_schema: ActionArgumentSchema::TaskCockpitV1,
    },
    ActionDescriptor {
        id: ACTION_CONFIG_COMMAND_DETAIL,
        title: "Project action detail",
        description: "Read one project command's label and command text for edit.",
        keywords: &["project", "action", "command", "detail", "edit"],
        scope: ActionScope::Host,
        required_capability: Some(Capability::TaskCockpit),
        risk: ActionRisk::ReadOnly,
        argument_schema: ActionArgumentSchema::TaskCockpitV1,
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
        id: ACTION_TASK_SETTLE,
        title: "Mark task Done",
        description: "Move one Task to Done while preserving its runtime and conversation.",
        keywords: &["task", "done", "settle", "complete"],
        scope: ActionScope::Task,
        required_capability: None,
        risk: ActionRisk::Mutating,
        argument_schema: ActionArgumentSchema::TaskId,
    },
    ActionDescriptor {
        id: ACTION_TASK_REOPEN,
        title: "Restore task",
        description: "Return one Done Task to the active task list.",
        keywords: &["task", "restore", "reopen", "unsettle"],
        scope: ActionScope::Task,
        required_capability: None,
        risk: ActionRisk::Mutating,
        argument_schema: ActionArgumentSchema::TaskId,
    },
    ActionDescriptor {
        id: ACTION_TASK_ARCHIVE,
        title: "Archive task",
        description: "Archive one Task after releasing its task-owned resources.",
        keywords: &["task", "archive", "close", "remove"],
        scope: ActionScope::Task,
        required_capability: None,
        risk: ActionRisk::Mutating,
        argument_schema: ActionArgumentSchema::TaskId,
    },
    ActionDescriptor {
        id: ACTION_TASK_DELETE,
        title: "Delete task",
        description: "Permanently remove one archived Task from the task list.",
        keywords: &["task", "delete", "permanent", "remove"],
        scope: ActionScope::Task,
        required_capability: None,
        risk: ActionRisk::Mutating,
        argument_schema: ActionArgumentSchema::TaskId,
    },
    ActionDescriptor {
        id: ACTION_PROVIDER_SEND_NOW,
        title: "Send now",
        description: "Send provider input on the exact Task, Agent, generation, and turn.",
        keywords: &["provider", "send", "prompt", "input"],
        scope: ActionScope::Task,
        required_capability: Some(Capability::ProviderInput),
        risk: ActionRisk::Mutating,
        argument_schema: ActionArgumentSchema::ProviderInputV1,
    },
    ActionDescriptor {
        id: ACTION_PROVIDER_STEER_CURRENT_TURN,
        title: "Steer current turn",
        description: "Steer the exact current provider turn without starting a new conversation.",
        keywords: &["provider", "steer", "turn"],
        scope: ActionScope::Task,
        required_capability: Some(Capability::ProviderInput),
        risk: ActionRisk::Mutating,
        argument_schema: ActionArgumentSchema::ProviderInputV1,
    },
    ActionDescriptor {
        id: ACTION_PROVIDER_QUEUE_FOLLOW_UP,
        title: "Queue follow-up",
        description: "Queue a follow-up for the exact AgentSession after the current turn.",
        keywords: &["provider", "queue", "follow-up"],
        scope: ActionScope::Task,
        required_capability: Some(Capability::ProviderInput),
        risk: ActionRisk::Mutating,
        argument_schema: ActionArgumentSchema::ProviderInputV1,
    },
    ActionDescriptor {
        id: ACTION_PROVIDER_ANSWER_QUESTION,
        title: "Answer question",
        description: "Answer one exact provider question. First answer wins across devices.",
        keywords: &["provider", "question", "answer"],
        scope: ActionScope::Task,
        required_capability: Some(Capability::ProviderInput),
        risk: ActionRisk::Mutating,
        argument_schema: ActionArgumentSchema::ProviderInputV1,
    },
    ActionDescriptor {
        id: ACTION_PROVIDER_RESOLVE_APPROVAL,
        title: "Resolve approval",
        description: "Resolve one exact provider approval. First decision wins across devices.",
        keywords: &["provider", "approval", "allow", "deny"],
        scope: ActionScope::Task,
        required_capability: Some(Capability::ProviderInput),
        risk: ActionRisk::Mutating,
        argument_schema: ActionArgumentSchema::ProviderInputV1,
    },
    ActionDescriptor {
        id: ACTION_PROVIDER_STOP_TURN,
        title: "Stop turn",
        description: "Stop the exact current provider turn. This is not Restart.",
        keywords: &["provider", "stop", "turn", "interrupt"],
        scope: ActionScope::Task,
        required_capability: Some(Capability::ProviderInput),
        risk: ActionRisk::Mutating,
        argument_schema: ActionArgumentSchema::ProviderInputV1,
    },
    ActionDescriptor {
        id: ACTION_PROVIDER_TERMINAL_INPUT,
        title: "Provider terminal input",
        description:
            "Send one bounded key or paste sequence to the exact running provider terminal.",
        keywords: &["provider", "terminal", "input", "interactive"],
        scope: ActionScope::Task,
        required_capability: Some(Capability::ProviderInput),
        risk: ActionRisk::Mutating,
        argument_schema: ActionArgumentSchema::ProviderInputV1,
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
    #[serde(default)]
    pub primary_provider: Option<ProviderKind>,
    /// Create the durable task/agent/resource shell without starting a CLI.
    /// The native composer starts it with the user's current launch settings
    /// immediately before accepting the first message.
    #[serde(default)]
    pub defer_primary_provider_start: bool,
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

/// A presentational client request backed by one of the closed catalog entries.
///
/// Components may emit this value, but they do not turn it into a transport
/// operation. The client boundary remains responsible for adding request and
/// command identity before sending it to the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionRequest {
    HostActions,
    HostStatus,
    Browser(BrowserActionRequest),
    TaskList,
    TaskShow {
        task_id: TaskId,
    },
    TaskCreate(TaskCreateArguments),
    TaskCreateV2(TaskCreateV2Arguments),
    TaskRename(TaskRenameArguments),
    TaskSettle {
        task_id: TaskId,
    },
    TaskReopen {
        task_id: TaskId,
    },
    TaskArchive {
        task_id: TaskId,
    },
    TaskDelete {
        task_id: TaskId,
    },
    ProviderInput(ProviderInputActionRequest),
    StartProviderSession(ProviderStartArguments),
    ServiceControl {
        action: ServiceControlAction,
        arguments: ServiceControlArguments,
    },
    TaskCockpit {
        task_id: TaskId,
        query: TaskCockpitQuery,
    },
    PromptLibrary {
        query: PromptLibraryQuery,
    },
    Updater(UpdaterAction),
}

impl ActionRequest {
    pub const fn id(&self) -> &'static str {
        match self {
            Self::HostActions => ACTION_HOST_ACTIONS,
            Self::HostStatus => ACTION_HOST_STATUS,
            Self::Browser(_) => ACTION_BROWSER_NATIVE,
            Self::TaskList => ACTION_TASK_LIST,
            Self::TaskShow { .. } => ACTION_TASK_SHOW,
            Self::TaskCreate(_) => ACTION_TASK_CREATE,
            Self::TaskCreateV2(_) => ACTION_TASK_CREATE_V2,
            Self::TaskRename(_) => ACTION_TASK_RENAME,
            Self::TaskSettle { .. } => ACTION_TASK_SETTLE,
            Self::TaskReopen { .. } => ACTION_TASK_REOPEN,
            Self::TaskArchive { .. } => ACTION_TASK_ARCHIVE,
            Self::TaskDelete { .. } => ACTION_TASK_DELETE,
            Self::ProviderInput(arguments) => arguments.action_id,
            Self::StartProviderSession(_) => ACTION_PROVIDER_START_SESSION,
            Self::ServiceControl { action, .. } => match action {
                ServiceControlAction::Start => ACTION_SERVICE_START,
                ServiceControlAction::Stop => ACTION_SERVICE_STOP,
                ServiceControlAction::Restart => ACTION_SERVICE_RESTART,
            },
            Self::TaskCockpit { query, .. } => cockpit_query_action_id(query),
            Self::PromptLibrary { query } => prompt_query_action_id(query),
            Self::Updater(action) => action.id(),
        }
    }

    pub fn descriptor(&self) -> &'static ActionDescriptor {
        descriptor(self.id()).expect("every ActionRequest must have a catalog descriptor")
    }
}

/// Native-shell browser action backed by an already-admitted controller
/// command. Keeping the command here lets keyboard/pointer paths share the
/// same exact identity/lease fence as the host, while avoiding a second
/// browser command vocabulary in the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserActionRequest {
    pub command: BrowserNativeHostCommand,
}

impl BrowserActionRequest {
    pub fn task_id(&self) -> crate::domain::id::TaskId {
        self.command.identity().task_id()
    }
}

/// A typed provider action captured by the native shell. The host receives it
/// as the existing durable SubmitProviderInput command; no raw PTY bytes are
/// exposed to the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderInputActionRequest {
    /// The composer-owned durable command identity. The native action adapter
    /// must preserve it so host acceptance settles the exact submitted draft.
    pub command_id: CommandId,
    pub action_id: &'static str,
    pub arguments: ProviderInputArguments,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderStartArguments {
    pub task_id: TaskId,
    pub agent_session_id: AgentSessionId,
    pub resource_id: ResourceId,
    pub provider_kind: ProviderKind,
    pub mode: ProviderStartMode,
    pub launch_options: crate::providers::adapter::ProviderLaunchOptions,
    pub action_epoch: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdaterAction {
    StartBackground,
    Check,
    Download,
    Install,
}

impl UpdaterAction {
    pub const fn id(self) -> &'static str {
        match self {
            Self::StartBackground => ACTION_UPDATER_START_BACKGROUND,
            Self::Check => ACTION_UPDATER_CHECK,
            Self::Download => ACTION_UPDATER_DOWNLOAD,
            Self::Install => ACTION_UPDATER_INSTALL,
        }
    }
}

pub const fn cockpit_query_action_id(query: &TaskCockpitQuery) -> &'static str {
    match query {
        TaskCockpitQuery::ConfigSnapshot => ACTION_WORKSPACE_STATUS,
        TaskCockpitQuery::AgentConnection => ACTION_HOST_STATUS,
        TaskCockpitQuery::ConfigCreateProject { .. } => ACTION_CONFIG_CREATE_PROJECT,
        TaskCockpitQuery::ConfigUpsertCommand { .. } => ACTION_CONFIG_UPSERT_COMMAND,
        TaskCockpitQuery::ConfigArchiveCommand { .. } => ACTION_CONFIG_ARCHIVE_COMMAND,
        TaskCockpitQuery::ConfigRunCommand { .. } => ACTION_CONFIG_RUN_COMMAND,
        TaskCockpitQuery::ConfigCommandDetail { .. } => ACTION_CONFIG_COMMAND_DETAIL,
        TaskCockpitQuery::ProviderSettings(_) | TaskCockpitQuery::RemoteAccess(_) => {
            ACTION_HOST_STATUS
        }
        TaskCockpitQuery::BrowserProcessSession => ACTION_BROWSER_NATIVE,
        TaskCockpitQuery::Conversation { .. }
        | TaskCockpitQuery::OpenConversationSubscription { .. }
        | TaskCockpitQuery::ReleaseConversationSubscription { .. }
        | TaskCockpitQuery::ProviderInputState => ACTION_CONVERSATION_STATUS,
        TaskCockpitQuery::Terminal
        | TaskCockpitQuery::TerminalScroll { .. }
        | TaskCockpitQuery::TerminalResize { .. }
        | TaskCockpitQuery::TerminalReadiness
        | TaskCockpitQuery::TerminalFor { .. }
        | TaskCockpitQuery::TerminalScrollFor { .. }
        | TaskCockpitQuery::TerminalResizeFor { .. }
        | TaskCockpitQuery::TerminalReadinessFor { .. }
        | TaskCockpitQuery::TaskTerminals => ACTION_PROVIDER_TERMINAL_INPUT,
        TaskCockpitQuery::WorkspaceStatus => ACTION_WORKSPACE_STATUS,
        TaskCockpitQuery::GitRepositories => ACTION_GIT_REPOSITORIES,
        TaskCockpitQuery::GitStatus
        | TaskCockpitQuery::GitStatusTargeted { .. }
        | TaskCockpitQuery::GitFileDiffTargeted { .. }
        | TaskCockpitQuery::GitHistoryTargeted { .. }
        | TaskCockpitQuery::GitCommitDiffTargeted { .. }
        | TaskCockpitQuery::GitMutate { .. }
        | TaskCockpitQuery::GitMutateTargeted { .. } => ACTION_GIT_STATUS,
        TaskCockpitQuery::FilesList { .. } => ACTION_FILES_LIST,
        TaskCockpitQuery::FilesRead { .. } => ACTION_FILES_READ,
        TaskCockpitQuery::FilesWrite { .. } => ACTION_FILES_WRITE,
        TaskCockpitQuery::SshStatus => ACTION_SSH_STATUS,
        TaskCockpitQuery::SshAction { .. } => ACTION_SSH_ACTION,
        TaskCockpitQuery::ServiceSnapshots | TaskCockpitQuery::ServiceLogs { .. } => {
            ACTION_SERVICE_LOGS
        }
        TaskCockpitQuery::ServiceHealth { .. } => ACTION_SERVICE_HEALTH,
    }
}

pub const fn prompt_query_action_id(query: &PromptLibraryQuery) -> &'static str {
    match query {
        PromptLibraryQuery::MetadataPage { .. } => ACTION_PROMPT_METADATA_PAGE,
        PromptLibraryQuery::ExactVersion { .. } => ACTION_PROMPT_VERSION_PAGE,
        PromptLibraryQuery::Diff { .. } => ACTION_PROMPT_DIFF,
        PromptLibraryQuery::Search { .. } => ACTION_PROMPT_SEARCH_PAGE,
        PromptLibraryQuery::ChainPage { .. } => ACTION_PROMPT_CHAIN_PAGE,
        PromptLibraryQuery::HistoryPage { .. } => ACTION_PROMPT_HISTORY_PAGE,
    }
}

pub fn task_cockpit_request(task_id: TaskId, action_id: &str) -> Option<ActionRequest> {
    let query = match action_id {
        ACTION_WORKSPACE_STATUS => TaskCockpitQuery::WorkspaceStatus,
        ACTION_GIT_STATUS => TaskCockpitQuery::GitStatus,
        ACTION_GIT_REPOSITORIES => TaskCockpitQuery::GitRepositories,
        ACTION_FILES_LIST => TaskCockpitQuery::FilesList {
            relative_directory: None,
            limit: MAX_COCKPIT_FILE_LIST,
        },
        ACTION_FILES_READ => return None,
        ACTION_SSH_STATUS => TaskCockpitQuery::SshStatus,
        ACTION_SERVICE_LOGS | ACTION_SERVICE_HEALTH => return None,
        _ => return None,
    };
    Some(ActionRequest::TaskCockpit { task_id, query })
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

/// Explicit read/write posture for a cockpit surface descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CockpitSurfaceAccess {
    ReadOnly,
    ReadWrite,
}

/// Cockpit-facing workspace surfaces that must not reuse legacy RemoteAction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CockpitSurfaceKind {
    Workspace,
    Git,
    Files,
    Ssh,
    Services,
}

/// Typed unavailable/available descriptor for one Task Cockpit surface.
///
/// These are projection contracts only. They do not mint RemoteAction values
/// and do not claim a native-shell renderer path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CockpitSurfaceDescriptor {
    pub kind: CockpitSurfaceKind,
    pub action_id: Option<&'static str>,
    pub available: bool,
    pub access: CockpitSurfaceAccess,
    pub redacts_secrets: bool,
    pub disabled_reason: Option<&'static str>,
}

/// Closed cockpit surface catalog for workspace/git/files/ssh/services.
///
/// Service start/stop/restart remain on the capability-gated ServiceControl
/// path. Workspace/git/files list+read, SSH status, and service logs/health
/// are typed TaskCockpit queries. File writes, git mutate, and SSH actions stay
/// explicitly unavailable until their production authority exists.
pub fn cockpit_surface_descriptors() -> &'static [CockpitSurfaceDescriptor] {
    const SURFACES: &[CockpitSurfaceDescriptor] = &[
        CockpitSurfaceDescriptor {
            kind: CockpitSurfaceKind::Services,
            action_id: Some(ACTION_SERVICE_START),
            available: true,
            access: CockpitSurfaceAccess::ReadWrite,
            redacts_secrets: true,
            disabled_reason: None,
        },
        CockpitSurfaceDescriptor {
            kind: CockpitSurfaceKind::Services,
            action_id: Some(ACTION_SERVICE_STOP),
            available: true,
            access: CockpitSurfaceAccess::ReadWrite,
            redacts_secrets: true,
            disabled_reason: None,
        },
        CockpitSurfaceDescriptor {
            kind: CockpitSurfaceKind::Services,
            action_id: Some(ACTION_SERVICE_RESTART),
            available: true,
            access: CockpitSurfaceAccess::ReadWrite,
            redacts_secrets: true,
            disabled_reason: None,
        },
        CockpitSurfaceDescriptor {
            kind: CockpitSurfaceKind::Services,
            action_id: Some(ACTION_SERVICE_LOGS),
            available: true,
            access: CockpitSurfaceAccess::ReadOnly,
            redacts_secrets: true,
            disabled_reason: None,
        },
        CockpitSurfaceDescriptor {
            kind: CockpitSurfaceKind::Services,
            action_id: Some(ACTION_SERVICE_HEALTH),
            available: true,
            access: CockpitSurfaceAccess::ReadOnly,
            redacts_secrets: true,
            disabled_reason: None,
        },
        CockpitSurfaceDescriptor {
            kind: CockpitSurfaceKind::Workspace,
            action_id: Some(ACTION_WORKSPACE_STATUS),
            available: true,
            access: CockpitSurfaceAccess::ReadOnly,
            redacts_secrets: true,
            disabled_reason: None,
        },
        CockpitSurfaceDescriptor {
            kind: CockpitSurfaceKind::Git,
            action_id: Some(ACTION_GIT_STATUS),
            available: true,
            access: CockpitSurfaceAccess::ReadOnly,
            redacts_secrets: true,
            disabled_reason: None,
        },
        CockpitSurfaceDescriptor {
            kind: CockpitSurfaceKind::Files,
            action_id: Some(ACTION_FILES_LIST),
            available: true,
            access: CockpitSurfaceAccess::ReadOnly,
            redacts_secrets: true,
            disabled_reason: None,
        },
        CockpitSurfaceDescriptor {
            kind: CockpitSurfaceKind::Files,
            action_id: Some(ACTION_FILES_READ),
            available: true,
            access: CockpitSurfaceAccess::ReadOnly,
            redacts_secrets: true,
            disabled_reason: None,
        },
        CockpitSurfaceDescriptor {
            kind: CockpitSurfaceKind::Files,
            action_id: Some(ACTION_FILES_WRITE),
            available: false,
            access: CockpitSurfaceAccess::ReadWrite,
            redacts_secrets: true,
            disabled_reason: Some("safe workspace file writes are not issued on this host"),
        },
        CockpitSurfaceDescriptor {
            kind: CockpitSurfaceKind::Ssh,
            action_id: Some(ACTION_SSH_STATUS),
            available: true,
            access: CockpitSurfaceAccess::ReadOnly,
            redacts_secrets: true,
            disabled_reason: None,
        },
        CockpitSurfaceDescriptor {
            kind: CockpitSurfaceKind::Ssh,
            action_id: Some(ACTION_SSH_ACTION),
            available: false,
            access: CockpitSurfaceAccess::ReadOnly,
            redacts_secrets: true,
            disabled_reason: Some(
                "ssh launch/stop is unavailable until a Task supervisor adapter is issued",
            ),
        },
    ];
    SURFACES
}

/// Return the closed catalog for this slice.
pub fn catalog() -> &'static [ActionDescriptor] {
    static CATALOG: OnceLock<Vec<ActionDescriptor>> = OnceLock::new();
    CATALOG
        .get_or_init(|| {
            let mut entries = Vec::with_capacity(
                ACTIONS.len()
                    + PROMPT_LIBRARY_EXTENSION.len()
                    + SERVICE_CONTROL_EXTENSION.len()
                    + TASK_COCKPIT_EXTENSION.len()
                    + COMPOSER_TURN_EXTENSION.len()
                    + UPDATER_EXTENSION.len(),
            );
            entries.extend_from_slice(ACTIONS);
            entries.extend_from_slice(PROMPT_LIBRARY_EXTENSION);
            entries.extend_from_slice(SERVICE_CONTROL_EXTENSION);
            entries.extend_from_slice(TASK_COCKPIT_EXTENSION);
            entries.extend_from_slice(COMPOSER_TURN_EXTENSION);
            entries.extend_from_slice(UPDATER_EXTENSION);
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

/// Host service controls are catalogued as typed actions. The host grants the
/// `ServiceSupervisor` capability only after its configured supervisor is
/// initialized; clients therefore fail closed until that grant is present.
const SERVICE_CONTROL_EXTENSION: &[ActionDescriptor] = &[
    ActionDescriptor {
        id: ACTION_SERVICE_START,
        title: "Start service",
        description: "Start one configured service through the host supervisor.",
        keywords: &["service", "start", "server"],
        scope: ActionScope::Host,
        required_capability: Some(Capability::ServiceSupervisor),
        risk: ActionRisk::Mutating,
        argument_schema: ActionArgumentSchema::ServiceControlV1,
    },
    ActionDescriptor {
        id: ACTION_SERVICE_STOP,
        title: "Stop service",
        description: "Stop one configured service through the host supervisor.",
        keywords: &["service", "stop", "server"],
        scope: ActionScope::Host,
        required_capability: Some(Capability::ServiceSupervisor),
        risk: ActionRisk::Mutating,
        argument_schema: ActionArgumentSchema::ServiceControlV1,
    },
    ActionDescriptor {
        id: ACTION_SERVICE_RESTART,
        title: "Restart service",
        description: "Restart one configured service through the host supervisor.",
        keywords: &["service", "restart", "server"],
        scope: ActionScope::Host,
        required_capability: Some(Capability::ServiceSupervisor),
        risk: ActionRisk::Mutating,
        argument_schema: ActionArgumentSchema::ServiceControlV1,
    },
];

const TASK_COCKPIT_EXTENSION: &[ActionDescriptor] = &[
    ActionDescriptor {
        id: ACTION_PROVIDER_START_SESSION,
        title: "Start provider session",
        description: "Start one exact task-owned stock provider runtime.",
        keywords: &["provider", "start", "session", "cockpit"],
        scope: ActionScope::Task,
        required_capability: Some(Capability::ProviderInput),
        risk: ActionRisk::Mutating,
        argument_schema: ActionArgumentSchema::TaskCockpitV1,
    },
    ActionDescriptor {
        id: ACTION_CONVERSATION_STATUS,
        title: "Conversation status",
        description: "Read the selected Task's bounded semantic AI conversation.",
        keywords: &["conversation", "messages", "timeline", "cockpit"],
        scope: ActionScope::Task,
        required_capability: Some(Capability::SemanticConversation),
        risk: ActionRisk::ReadOnly,
        argument_schema: ActionArgumentSchema::TaskCockpitV1,
    },
    ActionDescriptor {
        id: ACTION_WORKSPACE_STATUS,
        title: "Workspace status",
        description: "Read the selected Task workspace identity without raw paths or secrets.",
        keywords: &["workspace", "task", "cockpit"],
        scope: ActionScope::Task,
        required_capability: Some(Capability::TaskCockpit),
        risk: ActionRisk::ReadOnly,
        argument_schema: ActionArgumentSchema::TaskCockpitV1,
    },
    ActionDescriptor {
        id: ACTION_GIT_STATUS,
        title: "Git status",
        description: "Read a bounded redacted git identity for the selected Task workspace.",
        keywords: &["git", "status", "cockpit"],
        scope: ActionScope::Task,
        required_capability: Some(Capability::TaskCockpit),
        risk: ActionRisk::ReadOnly,
        argument_schema: ActionArgumentSchema::TaskCockpitV1,
    },
    ActionDescriptor {
        id: ACTION_GIT_REPOSITORIES,
        title: "Git repositories",
        description:
            "List bounded path-redacted repositories available for the selected Task project.",
        keywords: &["git", "repositories", "catalog", "cockpit"],
        scope: ActionScope::Task,
        required_capability: Some(Capability::TaskCockpit),
        risk: ActionRisk::ReadOnly,
        argument_schema: ActionArgumentSchema::TaskCockpitV1,
    },
    ActionDescriptor {
        id: ACTION_FILES_LIST,
        title: "List files",
        description: "List bounded workspace files through host Task file authority.",
        keywords: &["files", "list", "cockpit"],
        scope: ActionScope::Task,
        required_capability: Some(Capability::TaskCockpit),
        risk: ActionRisk::ReadOnly,
        argument_schema: ActionArgumentSchema::TaskCockpitV1,
    },
    ActionDescriptor {
        id: ACTION_FILES_READ,
        title: "Read file",
        description: "Read a bounded workspace file through host Task file authority.",
        keywords: &["files", "read", "cockpit"],
        scope: ActionScope::Task,
        required_capability: Some(Capability::TaskCockpit),
        risk: ActionRisk::ReadOnly,
        argument_schema: ActionArgumentSchema::TaskCockpitV1,
    },
    ActionDescriptor {
        id: ACTION_SSH_STATUS,
        title: "SSH status",
        description: "Read redacted SSH catalog status for the selected Task.",
        keywords: &["ssh", "status", "cockpit"],
        scope: ActionScope::Task,
        required_capability: Some(Capability::TaskCockpit),
        risk: ActionRisk::ReadOnly,
        argument_schema: ActionArgumentSchema::TaskCockpitV1,
    },
    ActionDescriptor {
        id: ACTION_SERVICE_LOGS,
        title: "Service logs",
        description: "Read bounded redacted logs for one configured service in the selected Task.",
        keywords: &["service", "logs", "cockpit"],
        scope: ActionScope::Task,
        required_capability: Some(Capability::TaskCockpit),
        risk: ActionRisk::ReadOnly,
        argument_schema: ActionArgumentSchema::ServiceControlV1,
    },
    ActionDescriptor {
        id: ACTION_SERVICE_HEALTH,
        title: "Service health",
        description: "Read redacted health for one configured service in the selected Task.",
        keywords: &["service", "health", "cockpit"],
        scope: ActionScope::Task,
        required_capability: Some(Capability::TaskCockpit),
        risk: ActionRisk::ReadOnly,
        argument_schema: ActionArgumentSchema::ServiceControlV1,
    },
];

const COMPOSER_TURN_EXTENSION: &[ActionDescriptor] = &[
    ActionDescriptor {
        id: ACTION_TASK_SEND_NOW,
        title: "Send now",
        description: "Send composer input on the current provider turn.",
        keywords: &["send", "composer", "turn"],
        scope: ActionScope::Task,
        required_capability: Some(Capability::ProviderInput),
        risk: ActionRisk::Mutating,
        argument_schema: ActionArgumentSchema::ProviderInputV1,
    },
    ActionDescriptor {
        id: ACTION_TASK_STEER_CURRENT_TURN,
        title: "Steer current turn",
        description: "Steer the current provider turn from the composer.",
        keywords: &["steer", "composer", "turn"],
        scope: ActionScope::Task,
        required_capability: Some(Capability::ProviderInput),
        risk: ActionRisk::Mutating,
        argument_schema: ActionArgumentSchema::ProviderInputV1,
    },
    ActionDescriptor {
        id: ACTION_TASK_QUEUE_FOLLOW_UP,
        title: "Queue follow-up",
        description: "Queue a follow-up after the current provider turn.",
        keywords: &["queue", "composer", "turn"],
        scope: ActionScope::Task,
        required_capability: Some(Capability::ProviderInput),
        risk: ActionRisk::Mutating,
        argument_schema: ActionArgumentSchema::ProviderInputV1,
    },
    ActionDescriptor {
        id: ACTION_TASK_ANSWER_QUESTION,
        title: "Answer question",
        description: "Answer the exact projected provider question.",
        keywords: &["answer", "question", "composer"],
        scope: ActionScope::Task,
        required_capability: Some(Capability::ProviderInput),
        risk: ActionRisk::Mutating,
        argument_schema: ActionArgumentSchema::ProviderInputV1,
    },
    ActionDescriptor {
        id: ACTION_TASK_RESOLVE_APPROVAL,
        title: "Resolve approval",
        description: "Resolve the exact projected provider approval.",
        keywords: &["approval", "composer"],
        scope: ActionScope::Task,
        required_capability: Some(Capability::ProviderInput),
        risk: ActionRisk::Mutating,
        argument_schema: ActionArgumentSchema::ProviderInputV1,
    },
    ActionDescriptor {
        id: ACTION_TASK_STOP_TURN,
        title: "Stop turn",
        description: "Stop the current provider turn.",
        keywords: &["stop", "composer", "turn"],
        scope: ActionScope::Task,
        required_capability: Some(Capability::ProviderInput),
        risk: ActionRisk::Mutating,
        argument_schema: ActionArgumentSchema::ProviderInputV1,
    },
];

const UPDATER_EXTENSION: &[ActionDescriptor] = &[
    ActionDescriptor {
        id: ACTION_UPDATER_START_BACKGROUND,
        title: "Start background update checks",
        description: "Arm the existing updater background-check loop without installing.",
        keywords: &["updater", "background", "check"],
        scope: ActionScope::Host,
        required_capability: None,
        risk: ActionRisk::ReadOnly,
        argument_schema: ActionArgumentSchema::None,
    },
    ActionDescriptor {
        id: ACTION_UPDATER_CHECK,
        title: "Check for updates",
        description: "Run one updater freshness check without downloading or installing.",
        keywords: &["updater", "check"],
        scope: ActionScope::Host,
        required_capability: None,
        risk: ActionRisk::ReadOnly,
        argument_schema: ActionArgumentSchema::None,
    },
    ActionDescriptor {
        id: ACTION_UPDATER_DOWNLOAD,
        title: "Download update",
        description: "Download an admitted update package without launching an installer.",
        keywords: &["updater", "download"],
        scope: ActionScope::Host,
        required_capability: None,
        risk: ActionRisk::Mutating,
        argument_schema: ActionArgumentSchema::None,
    },
    ActionDescriptor {
        id: ACTION_UPDATER_INSTALL,
        title: "Install update",
        description: "Install a downloaded update. Tests must not dispatch this action.",
        keywords: &["updater", "install"],
        scope: ActionScope::Host,
        required_capability: None,
        risk: ActionRisk::Mutating,
        argument_schema: ActionArgumentSchema::None,
    },
];

pub fn registered_actions() -> impl Iterator<Item = &'static ActionDescriptor> {
    catalog().iter()
}

pub fn disabled_reason(id: &str, granted: CapabilitySet) -> Option<&'static str> {
    let Some(action) = action_by_id(id) else {
        return Some("unknown action");
    };
    if let Some(required) = action.required_capability {
        if !granted.contains(required) {
            return Some(match required {
                Capability::PromptProjection => "personal_prompt_library capability not granted",
                Capability::ServiceSupervisor => {
                    "configured service supervisor capability not granted"
                }
                Capability::TaskCockpit => "task_cockpit capability not granted",
                _ => "required capability not granted",
            });
        }
    }
    None
}

pub fn action_enabled(id: &str, granted: CapabilitySet) -> bool {
    action_by_id(id).is_some() && disabled_reason(id, granted).is_none()
}

/// Return the catalog disabled reason with the host-runtime readiness gate.
///
/// A negotiated capability is not enough: the host must have successfully
/// bound its one configured supervisor before service actions become usable.
pub fn service_action_disabled_reason(
    id: &str,
    granted: CapabilitySet,
    supervisor_initialized: bool,
) -> Option<&'static str> {
    let reason = disabled_reason(id, granted);
    if reason.is_some() {
        return reason;
    }
    if matches!(
        id,
        ACTION_SERVICE_START | ACTION_SERVICE_STOP | ACTION_SERVICE_RESTART
    ) && !supervisor_initialized
    {
        return Some("configured service supervisor is not initialized");
    }
    None
}

pub fn action_enabled_with_service_state(
    id: &str,
    granted: CapabilitySet,
    supervisor_initialized: bool,
) -> bool {
    action_by_id(id).is_some()
        && service_action_disabled_reason(id, granted, supervisor_initialized).is_none()
}

pub fn action_by_id(id: &str) -> Option<&'static ActionDescriptor> {
    registered_actions().find(|action| action.id == id)
}

pub fn put_prompt_version_in_composer(
    task_id: TaskId,
    agent_session_id: AgentSessionId,
    prompt_version_id: PromptVersionId,
    insertion: ComposerInsertionMode,
    chain_link_id: Option<crate::domain::id::PromptChainLinkId>,
) -> PutPromptVersionInComposer {
    crate::client::composer::put_prompt_version_in_composer(
        task_id,
        agent_session_id,
        prompt_version_id,
        insertion,
        chain_link_id,
    )
}

pub fn prompt_search_page_request(
    request_id: RequestId,
    client_id: ClientId,
    capability: &OwnerDeviceCapability,
    query: String,
    cursor: Option<PromptCursor>,
) -> Result<PromptLibraryRequest, PromptProjectionError> {
    PromptLibraryRequest::search(
        request_id,
        client_id,
        capability,
        PromptNamespace::Personal,
        query,
        cursor,
    )
}

pub fn prompt_history_page_request(
    request_id: RequestId,
    client_id: ClientId,
    capability: &OwnerDeviceCapability,
    expected_library_revision: Option<u64>,
    cursor: Option<PromptCursor>,
) -> Result<PromptLibraryRequest, PromptProjectionError> {
    PromptLibraryRequest::history_page(
        request_id,
        client_id,
        capability,
        expected_library_revision,
        cursor,
    )
}

pub fn prompt_diff_request(
    request_id: RequestId,
    client_id: ClientId,
    capability: &OwnerDeviceCapability,
    old_version_id: PromptVersionId,
    new_version_id: PromptVersionId,
    cursor: Option<PromptCursor>,
) -> Result<PromptLibraryRequest, PromptProjectionError> {
    PromptLibraryRequest::diff(
        request_id,
        client_id,
        capability,
        old_version_id,
        new_version_id,
        cursor,
    )
}

pub fn prompt_chain_page_request(
    request_id: RequestId,
    client_id: ClientId,
    capability: &OwnerDeviceCapability,
    chain_id: PromptChainId,
    expected_library_revision: Option<u64>,
    cursor: Option<PromptCursor>,
) -> Result<PromptLibraryRequest, PromptProjectionError> {
    PromptLibraryRequest::chain_page(
        request_id,
        client_id,
        capability,
        Some(chain_id),
        expected_library_revision,
        cursor,
    )
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

/// Resolve one stable action id through the single shared catalog.
pub fn descriptor(id: &str) -> Option<&'static ActionDescriptor> {
    catalog().iter().find(|action| action.id == id)
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

/// Build a task-scoped Task Cockpit query. The host resolves workspace
/// identity from the envelope task; callers must not attach a path.
pub fn task_cockpit_query(
    request_id: RequestId,
    client_id: ClientId,
    task_id: TaskId,
    query: TaskCockpitQuery,
) -> QueryEnvelope {
    QueryEnvelope {
        request_id,
        client_id,
        task_id: Some(task_id),
        query: Query::TaskCockpit(query),
    }
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
            primary_provider: args.primary_provider,
            defer_primary_provider_start: args.defer_primary_provider_start,
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

/// Build the shared `task.archive` mutation after the caller captured the
/// exact durable revision. The host releases leftover resources, then closes.
pub fn task_archive_command(
    command_id: CommandId,
    client_id: ClientId,
    issued_at_ms: i64,
    expected_task_revision: u64,
    task_id: TaskId,
) -> CommandEnvelope {
    CommandEnvelope {
        command_id,
        client_id,
        task_id: Some(task_id),
        issued_at_ms,
        expected_task_revision: Some(expected_task_revision),
        command: Command::BeginCloseTask,
    }
}

/// Caller-owned provider input arguments validated before transport.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderInputArguments {
    pub task_id: TaskId,
    pub agent_session_id: AgentSessionId,
    pub runtime_generation: u64,
    pub action_epoch: u64,
    pub turn_id: TurnId,
    pub question_id: Option<QuestionId>,
    pub approval_id: Option<ApprovalId>,
    #[serde(
        default,
        deserialize_with = "crate::domain::provider_input::deserialize_optional_provider_text"
    )]
    pub text: Option<String>,
    pub wait: Option<bool>,
    pub allow: Option<bool>,
    /// Staged image identities for SendNow only. Empty is omitted on the wire.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "crate::domain::provider_input::deserialize_optional_provider_images"
    )]
    pub images: Vec<ProviderImageAttachment>,
}

/// Build the shared provider-input mutation after bound and identity checks.
pub fn provider_input_command(
    command_id: CommandId,
    client_id: ClientId,
    issued_at_ms: i64,
    expected_task_revision: u64,
    action_id: &str,
    args: ProviderInputArguments,
) -> Result<CommandEnvelope, ProviderInputIntentError> {
    validate_provider_images(&args.images)?;
    let action = match action_id {
        ACTION_PROVIDER_SEND_NOW | ACTION_TASK_SEND_NOW => {
            let text = args.text.unwrap_or_default();
            validate_send_now_payload(&text, &args.images)?;
            ProviderInputAction::SendNow {
                text,
                wait: args.wait.unwrap_or(false),
                images: args.images,
            }
        }
        ACTION_PROVIDER_STEER_CURRENT_TURN | ACTION_TASK_STEER_CURRENT_TURN => {
            if !args.images.is_empty() {
                return Err(ProviderInputIntentError::ImagesUnsupported);
            }
            ProviderInputAction::SteerCurrentTurn {
                text: args.text.ok_or(ProviderInputIntentError::EmptyText)?,
            }
        }
        ACTION_PROVIDER_QUEUE_FOLLOW_UP | ACTION_TASK_QUEUE_FOLLOW_UP => {
            if !args.images.is_empty() {
                return Err(ProviderInputIntentError::ImagesUnsupported);
            }
            ProviderInputAction::QueueFollowUp {
                text: args.text.ok_or(ProviderInputIntentError::EmptyText)?,
            }
        }
        ACTION_PROVIDER_ANSWER_QUESTION | ACTION_TASK_ANSWER_QUESTION => {
            if !args.images.is_empty() {
                return Err(ProviderInputIntentError::ImagesUnsupported);
            }
            ProviderInputAction::AnswerQuestion {
                question_id: args
                    .question_id
                    .ok_or(ProviderInputIntentError::InconsistentNestedIds)?,
                answer: args.text.ok_or(ProviderInputIntentError::EmptyText)?,
            }
        }
        ACTION_PROVIDER_RESOLVE_APPROVAL | ACTION_TASK_RESOLVE_APPROVAL => {
            if !args.images.is_empty() {
                return Err(ProviderInputIntentError::ImagesUnsupported);
            }
            ProviderInputAction::ResolveApproval {
                approval_id: args
                    .approval_id
                    .ok_or(ProviderInputIntentError::InconsistentNestedIds)?,
                allow: args
                    .allow
                    .ok_or(ProviderInputIntentError::InconsistentNestedIds)?,
            }
        }
        ACTION_PROVIDER_STOP_TURN | ACTION_TASK_STOP_TURN => {
            if !args.images.is_empty() {
                return Err(ProviderInputIntentError::ImagesUnsupported);
            }
            ProviderInputAction::StopTurn
        }
        ACTION_PROVIDER_TERMINAL_INPUT => {
            if !args.images.is_empty() {
                return Err(ProviderInputIntentError::ImagesUnsupported);
            }
            ProviderInputAction::TerminalInput {
                text: args.text.ok_or(ProviderInputIntentError::EmptyText)?,
            }
        }
        _ => return Err(ProviderInputIntentError::InconsistentNestedIds),
    };
    let intent = SubmitProviderInputIntent::try_new(
        args.agent_session_id,
        args.runtime_generation,
        args.turn_id,
        args.action_epoch,
        args.question_id,
        args.approval_id,
        action,
    )?;
    Ok(CommandEnvelope {
        command_id,
        client_id,
        task_id: Some(args.task_id),
        issued_at_ms,
        expected_task_revision: Some(expected_task_revision),
        command: Command::SubmitProviderInput(intent),
    })
}

/// Build a stock-provider launch command from the exact Task Cockpit fence.
pub fn provider_start_command(
    command_id: CommandId,
    client_id: ClientId,
    issued_at_ms: i64,
    expected_task_revision: u64,
    args: ProviderStartArguments,
) -> Result<CommandEnvelope, ProviderInputIntentError> {
    // Task action epochs are zero-based: a newly-created open task is
    // legitimately fenced at epoch zero until a lifecycle transition advances
    // it. Only the durable revision uses zero as an invalid sentinel.
    if expected_task_revision == 0 {
        return Err(ProviderInputIntentError::InconsistentNestedIds);
    }
    Ok(CommandEnvelope {
        command_id,
        client_id,
        task_id: Some(args.task_id),
        issued_at_ms,
        expected_task_revision: Some(expected_task_revision),
        command: Command::StartProviderSession(StartProviderSessionIntent {
            task_id: args.task_id,
            agent_session_id: args.agent_session_id,
            resource_id: args.resource_id,
            provider_kind: args.provider_kind,
            mode: args.mode,
            launch_options: args.launch_options,
            expected_task_revision,
            expected_action_epoch: args.action_epoch,
        }),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceControlActionError {
    UnknownAction,
    InvalidFence,
    InvalidServiceId,
}

impl std::fmt::Display for ServiceControlActionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownAction => formatter.write_str("unknown service control action"),
            Self::InvalidFence => formatter.write_str("service control fence must be nonzero"),
            Self::InvalidServiceId => {
                formatter.write_str("service control requires a configured catalog id")
            }
        }
    }
}

impl std::error::Error for ServiceControlActionError {}

/// Build a host-only configured-service command from one catalog action.
pub fn service_control_command(
    command_id: CommandId,
    client_id: ClientId,
    issued_at_ms: i64,
    action_id: &str,
    args: ServiceControlArguments,
) -> Result<CommandEnvelope, ServiceControlActionError> {
    service_control_command_with_task(command_id, client_id, None, issued_at_ms, action_id, args)
}

/// Build a ServiceControl command bound to one Task scope for fail-closed
/// ownership checks on task-scoped services.
pub fn service_control_command_with_task(
    command_id: CommandId,
    client_id: ClientId,
    task_id: Option<TaskId>,
    issued_at_ms: i64,
    action_id: &str,
    args: ServiceControlArguments,
) -> Result<CommandEnvelope, ServiceControlActionError> {
    let action = match action_id {
        ACTION_SERVICE_START => ServiceControlAction::Start,
        ACTION_SERVICE_STOP => ServiceControlAction::Stop,
        ACTION_SERVICE_RESTART => ServiceControlAction::Restart,
        ACTION_SERVICE_LOGS | ACTION_SERVICE_HEALTH => {
            return Err(ServiceControlActionError::UnknownAction);
        }
        _ => return Err(ServiceControlActionError::UnknownAction),
    };
    if args.resource_generation == 0 || args.connection_epoch == 0 || args.action_epoch == 0 {
        return Err(ServiceControlActionError::InvalidFence);
    }
    let service_id = ConfiguredServiceId::new(args.service_id.as_str())
        .map_err(|_| ServiceControlActionError::InvalidServiceId)?;
    Ok(CommandEnvelope {
        command_id,
        client_id,
        task_id,
        issued_at_ms,
        expected_task_revision: None,
        command: Command::ServiceControl(ServiceControlIntent {
            service_id,
            resource_generation: args.resource_generation,
            connection_epoch: args.connection_epoch,
            action_epoch: args.action_epoch,
            action,
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::{
        catalog, provider_start_command, require_unique_ids, service_control_command,
        task_cockpit_query, task_create_command, task_rename_command, task_show_query,
        ActionArgumentSchema, ActionRisk, ActionScope, ProviderStartArguments,
        ServiceControlArguments, TaskCreateArguments, TaskCreateV2Arguments, TaskRenameArguments,
        ACTION_BROWSER_NATIVE, ACTION_CONFIG_ARCHIVE_COMMAND, ACTION_CONFIG_COMMAND_DETAIL,
        ACTION_CONFIG_CREATE_PROJECT, ACTION_CONFIG_RUN_COMMAND, ACTION_CONFIG_UPSERT_COMMAND,
        ACTION_CONVERSATION_STATUS, ACTION_FILES_LIST, ACTION_FILES_READ, ACTION_GIT_REPOSITORIES,
        ACTION_GIT_STATUS, ACTION_HOST_ACTIONS, ACTION_HOST_STATUS, ACTION_PROMPT_CHAIN_PAGE,
        ACTION_PROMPT_DIFF, ACTION_PROMPT_METADATA_PAGE, ACTION_PROMPT_VERSION_PAGE,
        ACTION_PROVIDER_ANSWER_QUESTION, ACTION_PROVIDER_NEW_CONVERSATION,
        ACTION_PROVIDER_QUEUE_FOLLOW_UP, ACTION_PROVIDER_RESOLVE_APPROVAL,
        ACTION_PROVIDER_SEND_NOW, ACTION_PROVIDER_START_SESSION,
        ACTION_PROVIDER_STEER_CURRENT_TURN, ACTION_PROVIDER_STOP_TURN,
        ACTION_PROVIDER_TERMINAL_INPUT, ACTION_SERVICE_HEALTH, ACTION_SERVICE_LOGS,
        ACTION_SERVICE_RESTART, ACTION_SERVICE_START, ACTION_SERVICE_STOP, ACTION_SSH_ACTION,
        ACTION_SSH_STATUS, ACTION_TASK_ANSWER_QUESTION, ACTION_TASK_ARCHIVE, ACTION_TASK_CREATE,
        ACTION_TASK_CREATE_V2, ACTION_TASK_DELETE, ACTION_TASK_LIST, ACTION_TASK_QUEUE_FOLLOW_UP,
        ACTION_TASK_RENAME, ACTION_TASK_REOPEN, ACTION_TASK_RESOLVE_APPROVAL, ACTION_TASK_SEND_NOW,
        ACTION_TASK_SETTLE, ACTION_TASK_SHOW, ACTION_TASK_STEER_CURRENT_TURN,
        ACTION_TASK_STOP_TURN, ACTION_UPDATER_CHECK, ACTION_UPDATER_DOWNLOAD,
        ACTION_UPDATER_INSTALL, ACTION_UPDATER_START_BACKGROUND, ACTION_WORKSPACE_STATUS,
    };
    use crate::{
        domain::{
            command::Command,
            query::Query,
            task::{
                ReviewReadiness, TaskActivity, TaskAssignment, TaskAttention, TaskConnectivity,
                WorkspaceRef,
            },
            AgentSessionId, ClientId, CommandId, EnvironmentId, ProjectId, RequestId, ResourceId,
            TaskId,
        },
        protocol::Capability,
        providers::ProviderKind,
        services::model::ServiceId,
    };

    #[test]
    fn catalog_exposes_unique_read_and_create_actions() {
        let ids: Vec<&str> = catalog().iter().map(|action| action.id).collect();
        assert!(ids.contains(&ACTION_HOST_ACTIONS));
        assert!(ids.contains(&ACTION_HOST_STATUS));
        assert!(ids.contains(&ACTION_TASK_LIST));
        assert!(ids.contains(&ACTION_TASK_SHOW));
        assert!(!ids.contains(&ACTION_TASK_CREATE));
        assert!(ids.contains(&ACTION_TASK_CREATE_V2));
        assert!(ids.contains(&ACTION_CONFIG_CREATE_PROJECT));
        assert!(ids.contains(&ACTION_CONFIG_UPSERT_COMMAND));
        assert!(ids.contains(&ACTION_CONFIG_ARCHIVE_COMMAND));
        assert!(ids.contains(&ACTION_CONFIG_RUN_COMMAND));
        assert!(ids.contains(&ACTION_CONFIG_COMMAND_DETAIL));
        assert!(ids.contains(&ACTION_TASK_RENAME));
        assert!(ids.contains(&ACTION_TASK_SETTLE));
        assert!(ids.contains(&ACTION_TASK_REOPEN));
        assert!(ids.contains(&ACTION_TASK_ARCHIVE));
        assert!(ids.contains(&ACTION_TASK_DELETE));
        assert!(ids.contains(&ACTION_PROVIDER_SEND_NOW));
        assert!(ids.contains(&ACTION_PROVIDER_STEER_CURRENT_TURN));
        assert!(ids.contains(&ACTION_PROVIDER_QUEUE_FOLLOW_UP));
        assert!(ids.contains(&ACTION_PROVIDER_ANSWER_QUESTION));
        assert!(ids.contains(&ACTION_PROVIDER_RESOLVE_APPROVAL));
        assert!(ids.contains(&ACTION_PROVIDER_STOP_TURN));
        assert!(ids.contains(&ACTION_PROVIDER_TERMINAL_INPUT));
        assert!(!ids.contains(&ACTION_PROVIDER_NEW_CONVERSATION));
        assert!(ids.contains(&ACTION_TASK_SEND_NOW));
        assert!(ids.contains(&ACTION_TASK_STEER_CURRENT_TURN));
        assert!(ids.contains(&ACTION_TASK_QUEUE_FOLLOW_UP));
        assert!(ids.contains(&ACTION_TASK_ANSWER_QUESTION));
        assert!(ids.contains(&ACTION_TASK_RESOLVE_APPROVAL));
        assert!(ids.contains(&ACTION_TASK_STOP_TURN));
        assert!(ids.contains(&ACTION_UPDATER_CHECK));
        assert!(ids.contains(&ACTION_UPDATER_DOWNLOAD));
        assert!(ids.contains(&ACTION_UPDATER_START_BACKGROUND));
        assert!(ids.contains(&ACTION_UPDATER_INSTALL));
        assert!(ids.contains(&ACTION_BROWSER_NATIVE));
        assert!(ids.contains(&ACTION_PROVIDER_START_SESSION));
        assert!(ids.contains(&ACTION_CONVERSATION_STATUS));
        assert_eq!(ids.len(), 50);
        assert!(ids.contains(&ACTION_SERVICE_START));
        assert!(ids.contains(&ACTION_SERVICE_STOP));
        assert!(ids.contains(&ACTION_SERVICE_RESTART));
        assert!(ids.contains(&ACTION_SERVICE_LOGS));
        assert!(ids.contains(&ACTION_SERVICE_HEALTH));
        assert!(!ids.contains(&ACTION_SSH_ACTION));
        assert!(ids.contains(&ACTION_WORKSPACE_STATUS));
        assert!(ids.contains(&ACTION_GIT_STATUS));
        assert!(ids.contains(&ACTION_GIT_REPOSITORIES));
        assert!(ids.contains(&ACTION_FILES_LIST));
        assert!(ids.contains(&ACTION_FILES_READ));
        assert!(ids.contains(&ACTION_SSH_STATUS));
        require_unique_ids().expect("ids must be unique");
        for action in catalog() {
            let (expected_scope, expected_risk, expected_schema, expected_capability) =
                match action.id {
                    ACTION_BROWSER_NATIVE => (
                        ActionScope::Task,
                        ActionRisk::Mutating,
                        ActionArgumentSchema::None,
                        Some(Capability::BrowserProjection),
                    ),
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
                    ACTION_CONFIG_CREATE_PROJECT => (
                        ActionScope::Host,
                        ActionRisk::Mutating,
                        ActionArgumentSchema::TaskCockpitV1,
                        Some(Capability::TaskCockpit),
                    ),
                    ACTION_CONFIG_UPSERT_COMMAND
                    | ACTION_CONFIG_ARCHIVE_COMMAND
                    | ACTION_CONFIG_RUN_COMMAND => (
                        ActionScope::Host,
                        ActionRisk::Mutating,
                        ActionArgumentSchema::TaskCockpitV1,
                        Some(Capability::TaskCockpit),
                    ),
                    ACTION_CONFIG_COMMAND_DETAIL => (
                        ActionScope::Host,
                        ActionRisk::ReadOnly,
                        ActionArgumentSchema::TaskCockpitV1,
                        Some(Capability::TaskCockpit),
                    ),
                    ACTION_TASK_RENAME => (
                        ActionScope::Task,
                        ActionRisk::Mutating,
                        ActionArgumentSchema::TaskRenameV1,
                        None,
                    ),
                    ACTION_TASK_SETTLE | ACTION_TASK_REOPEN | ACTION_TASK_ARCHIVE
                    | ACTION_TASK_DELETE => (
                        ActionScope::Task,
                        ActionRisk::Mutating,
                        ActionArgumentSchema::TaskId,
                        None,
                    ),
                    ACTION_PROVIDER_SEND_NOW
                    | ACTION_PROVIDER_STEER_CURRENT_TURN
                    | ACTION_PROVIDER_QUEUE_FOLLOW_UP
                    | ACTION_PROVIDER_ANSWER_QUESTION
                    | ACTION_PROVIDER_RESOLVE_APPROVAL
                    | ACTION_PROVIDER_TERMINAL_INPUT
                    | ACTION_PROVIDER_STOP_TURN
                    | ACTION_TASK_SEND_NOW
                    | ACTION_TASK_STEER_CURRENT_TURN
                    | ACTION_TASK_QUEUE_FOLLOW_UP
                    | ACTION_TASK_ANSWER_QUESTION
                    | ACTION_TASK_RESOLVE_APPROVAL
                    | ACTION_TASK_STOP_TURN => (
                        ActionScope::Task,
                        ActionRisk::Mutating,
                        ActionArgumentSchema::ProviderInputV1,
                        Some(Capability::ProviderInput),
                    ),
                    ACTION_PROVIDER_START_SESSION => (
                        ActionScope::Task,
                        ActionRisk::Mutating,
                        ActionArgumentSchema::TaskCockpitV1,
                        Some(Capability::ProviderInput),
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
                    ACTION_SERVICE_START | ACTION_SERVICE_STOP | ACTION_SERVICE_RESTART => (
                        ActionScope::Host,
                        ActionRisk::Mutating,
                        ActionArgumentSchema::ServiceControlV1,
                        Some(Capability::ServiceSupervisor),
                    ),
                    ACTION_CONVERSATION_STATUS => (
                        ActionScope::Task,
                        ActionRisk::ReadOnly,
                        ActionArgumentSchema::TaskCockpitV1,
                        Some(Capability::SemanticConversation),
                    ),
                    ACTION_WORKSPACE_STATUS
                    | ACTION_GIT_STATUS
                    | ACTION_GIT_REPOSITORIES
                    | ACTION_FILES_LIST
                    | ACTION_FILES_READ
                    | ACTION_SSH_STATUS => (
                        ActionScope::Task,
                        ActionRisk::ReadOnly,
                        ActionArgumentSchema::TaskCockpitV1,
                        Some(Capability::TaskCockpit),
                    ),
                    ACTION_SERVICE_LOGS | ACTION_SERVICE_HEALTH => (
                        ActionScope::Task,
                        ActionRisk::ReadOnly,
                        ActionArgumentSchema::ServiceControlV1,
                        Some(Capability::TaskCockpit),
                    ),
                    ACTION_UPDATER_START_BACKGROUND | ACTION_UPDATER_CHECK => (
                        ActionScope::Host,
                        ActionRisk::ReadOnly,
                        ActionArgumentSchema::None,
                        None,
                    ),
                    ACTION_UPDATER_DOWNLOAD | ACTION_UPDATER_INSTALL => (
                        ActionScope::Host,
                        ActionRisk::Mutating,
                        ActionArgumentSchema::None,
                        None,
                    ),
                    _ => (
                        ActionScope::Host,
                        ActionRisk::ReadOnly,
                        ActionArgumentSchema::None,
                        None,
                    ),
                };
            assert_eq!(action.scope, expected_scope, "scope for {}", action.id);
            assert_eq!(action.risk, expected_risk, "risk for {}", action.id);
            assert_eq!(
                action.argument_schema, expected_schema,
                "argument schema for {}",
                action.id
            );
            assert_eq!(
                action.required_capability, expected_capability,
                "required capability for {}",
                action.id
            );
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
    fn service_control_factory_requires_a_live_fence_and_keeps_host_scope() {
        let command_id = CommandId::new();
        let client_id = ClientId::new();
        let service_id = ServiceId::new("api").expect("bounded service id");
        let envelope = service_control_command(
            command_id,
            client_id,
            1_725_000_000_100,
            ACTION_SERVICE_START,
            ServiceControlArguments {
                service_id: service_id.clone(),
                resource_generation: 1,
                connection_epoch: 2,
                action_epoch: 3,
            },
        )
        .expect("valid service action");
        assert_eq!(envelope.command_id, command_id);
        assert_eq!(envelope.client_id, client_id);
        assert!(envelope.task_id.is_none());
        let Command::ServiceControl(intent) = envelope.command else {
            panic!("service action must build ServiceControl");
        };
        assert_eq!(intent.service_id.as_str(), service_id.as_str());
        assert_eq!(intent.resource_generation, 1);
        assert_eq!(intent.connection_epoch, 2);
        assert_eq!(intent.action_epoch, 3);
        assert_eq!(intent.action, crate::domain::ServiceControlAction::Start);
        assert!(service_control_command(
            CommandId::new(),
            client_id,
            1,
            ACTION_SERVICE_START,
            ServiceControlArguments {
                service_id,
                resource_generation: 0,
                connection_epoch: 2,
                action_epoch: 3,
            },
        )
        .is_err());
    }

    #[test]
    fn service_control_with_task_binds_envelope_scope_and_rejects_logs_health() {
        use super::{
            cockpit_surface_descriptors, service_control_command_with_task, CockpitSurfaceKind,
            ACTION_FILES_LIST, ACTION_GIT_STATUS, ACTION_SSH_STATUS, ACTION_WORKSPACE_STATUS,
        };

        let command_id = CommandId::new();
        let client_id = ClientId::new();
        let task_id = TaskId::new();
        let service_id = ServiceId::new("api").expect("bounded service id");
        let envelope = service_control_command_with_task(
            command_id,
            client_id,
            Some(task_id),
            1_725_000_000_100,
            ACTION_SERVICE_STOP,
            ServiceControlArguments {
                service_id: service_id.clone(),
                resource_generation: 4,
                connection_epoch: 5,
                action_epoch: 6,
            },
        )
        .expect("task-scoped service action");
        assert_eq!(envelope.task_id, Some(task_id));
        assert!(service_control_command_with_task(
            CommandId::new(),
            client_id,
            Some(task_id),
            1,
            ACTION_SERVICE_LOGS,
            ServiceControlArguments {
                service_id: service_id.clone(),
                resource_generation: 1,
                connection_epoch: 1,
                action_epoch: 1,
            },
        )
        .is_err());
        assert!(service_control_command_with_task(
            CommandId::new(),
            client_id,
            Some(task_id),
            1,
            ACTION_SERVICE_HEALTH,
            ServiceControlArguments {
                service_id,
                resource_generation: 1,
                connection_epoch: 1,
                action_epoch: 1,
            },
        )
        .is_err());

        let surfaces = cockpit_surface_descriptors();
        assert!(surfaces.iter().any(|surface| {
            surface.kind == CockpitSurfaceKind::Git
                && surface.available
                && surface.action_id == Some(ACTION_GIT_STATUS)
                && surface.disabled_reason.is_none()
        }));
        assert!(surfaces.iter().any(|surface| {
            surface.kind == CockpitSurfaceKind::Files
                && surface.available
                && surface.action_id == Some(ACTION_FILES_LIST)
                && surface.disabled_reason.is_none()
        }));
        assert!(surfaces.iter().any(|surface| {
            surface.kind == CockpitSurfaceKind::Ssh
                && surface.available
                && surface.action_id == Some(ACTION_SSH_STATUS)
        }));
        assert!(surfaces.iter().any(|surface| {
            surface.kind == CockpitSurfaceKind::Ssh
                && !surface.available
                && surface.action_id == Some(ACTION_SSH_ACTION)
                && surface.disabled_reason
                    == Some(
                        "ssh launch/stop is unavailable until a Task supervisor adapter is issued",
                    )
        }));
        assert_eq!(
            surfaces.iter().filter(|surface| surface.available).count(),
            10
        );
        assert!(surfaces.iter().any(|surface| {
            surface.kind == CockpitSurfaceKind::Services
                && surface.available
                && surface.action_id == Some(ACTION_SERVICE_LOGS)
        }));
        assert!(surfaces.iter().any(|surface| {
            surface.kind == CockpitSurfaceKind::Workspace
                && surface.available
                && surface.action_id == Some(ACTION_WORKSPACE_STATUS)
        }));
        let ids: Vec<&str> = catalog().iter().map(|action| action.id).collect();
        assert!(ids.contains(&ACTION_WORKSPACE_STATUS));
        assert!(ids.contains(&ACTION_GIT_STATUS));
        assert!(ids.contains(&ACTION_GIT_REPOSITORIES));
        assert!(ids.contains(&ACTION_FILES_LIST));
        assert!(ids.contains(&ACTION_SSH_STATUS));
    }

    #[test]
    fn task_cockpit_actions_fail_closed_without_capability() {
        use crate::protocol::CapabilitySet;
        assert_eq!(
            super::disabled_reason(ACTION_WORKSPACE_STATUS, CapabilitySet::empty()),
            Some("task_cockpit capability not granted")
        );
        assert!(super::action_enabled(
            ACTION_WORKSPACE_STATUS,
            crate::protocol::CapabilitySet::from_capabilities([Capability::TaskCockpit])
        ));
    }

    #[test]
    fn task_cockpit_factory_requires_exact_task_identity() {
        let request_id = RequestId::new();
        let client_id = ClientId::new();
        let task_id = TaskId::new();
        let envelope = task_cockpit_query(
            request_id,
            client_id,
            task_id,
            crate::domain::TaskCockpitQuery::WorkspaceStatus,
        );
        assert_eq!(envelope.request_id, request_id);
        assert_eq!(envelope.client_id, client_id);
        assert_eq!(envelope.task_id, Some(task_id));
        assert_eq!(
            envelope.query,
            Query::TaskCockpit(crate::domain::TaskCockpitQuery::WorkspaceStatus)
        );
        let logs = task_cockpit_query(
            request_id,
            client_id,
            task_id,
            crate::domain::TaskCockpitQuery::ServiceLogs {
                service_id: crate::domain::id::ConfiguredServiceId::new("api").expect("catalog"),
                resource_generation: 1,
                connection_epoch: 2,
                action_epoch: 3,
            },
        );
        assert_eq!(logs.task_id, Some(task_id));
        assert!(matches!(
            logs.query,
            Query::TaskCockpit(crate::domain::TaskCockpitQuery::ServiceLogs { .. })
        ));
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
    fn task_create_v2_carries_optional_primary_provider() {
        let args = TaskCreateV2Arguments {
            task_id: TaskId::new(),
            environment_id: EnvironmentId::new(),
            title: "New Claude task".into(),
            description: None,
            project_id: ProjectId::new(),
            workspace: crate::workspace::WorkspaceRequest::main(),
            primary_provider: Some(crate::providers::ProviderKind::ClaudeCode),
            defer_primary_provider_start: false,
        };
        assert_eq!(
            args.primary_provider,
            Some(crate::providers::ProviderKind::ClaudeCode)
        );
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

    #[test]
    fn provider_start_factory_accepts_the_initial_task_action_epoch() {
        let task_id = TaskId::new();
        let agent_session_id = AgentSessionId::new();
        let resource_id = ResourceId::new();
        let envelope = provider_start_command(
            CommandId::new(),
            ClientId::new(),
            1_725_000_000_100,
            5,
            ProviderStartArguments {
                task_id,
                agent_session_id,
                resource_id,
                provider_kind: ProviderKind::ClaudeCode,
                mode: crate::domain::command::ProviderStartMode::Open,
                launch_options: crate::providers::adapter::ProviderLaunchOptions::default(),
                action_epoch: 0,
            },
        )
        .expect("zero is the valid initial durable task action epoch");

        let Command::StartProviderSession(intent) = envelope.command else {
            panic!("provider start must build StartProviderSession");
        };
        assert_eq!(intent.task_id, task_id);
        assert_eq!(intent.agent_session_id, agent_session_id);
        assert_eq!(intent.resource_id, resource_id);
        assert_eq!(intent.expected_task_revision, 5);
        assert_eq!(intent.expected_action_epoch, 0);
    }

    #[test]
    fn task_cockpit_and_turn_requests_reuse_typed_host_contracts() {
        use super::{
            cockpit_query_action_id, provider_input_command, task_cockpit_request, ActionRequest,
            ProviderInputArguments, UpdaterAction, ACTION_FILES_WRITE, ACTION_PROMPT_METADATA_PAGE,
        };
        use crate::domain::cockpit::MAX_COCKPIT_FILE_LIST;
        use crate::domain::{AgentSessionId, TurnId};
        use crate::prompts::projection::PromptLibraryQuery;
        use crate::prompts::projection::PromptNamespace;

        let task_id = TaskId::new();
        let request = task_cockpit_request(task_id, ACTION_GIT_STATUS).expect("git route");
        assert_eq!(request.id(), ACTION_GIT_STATUS);
        let ActionRequest::TaskCockpit { query, .. } = request else {
            panic!("git status must stay a TaskCockpit query");
        };
        assert_eq!(cockpit_query_action_id(&query), ACTION_GIT_STATUS);
        let files = task_cockpit_request(task_id, ACTION_FILES_LIST).expect("files");
        assert!(matches!(
            files,
            ActionRequest::TaskCockpit {
                query: crate::domain::TaskCockpitQuery::FilesList { limit, .. },
                ..
            } if limit == MAX_COCKPIT_FILE_LIST
        ));
        assert!(task_cockpit_request(task_id, ACTION_FILES_WRITE).is_none());

        let prompt = ActionRequest::PromptLibrary {
            query: PromptLibraryQuery::MetadataPage {
                namespace: PromptNamespace::Personal,
                cursor: None,
                expected_revision: None,
            },
        };
        assert_eq!(prompt.id(), ACTION_PROMPT_METADATA_PAGE);
        assert_eq!(
            ActionRequest::Updater(UpdaterAction::Check).id(),
            ACTION_UPDATER_CHECK
        );
        assert_eq!(
            ActionRequest::Updater(UpdaterAction::Install).id(),
            ACTION_UPDATER_INSTALL
        );

        let envelope = provider_input_command(
            CommandId::new(),
            ClientId::new(),
            1_725_000_000_100,
            2,
            ACTION_TASK_SEND_NOW,
            ProviderInputArguments {
                task_id,
                agent_session_id: AgentSessionId::new(),
                runtime_generation: 3,
                action_epoch: 4,
                turn_id: TurnId::new(),
                question_id: None,
                approval_id: None,
                text: Some("ship it".into()),
                wait: Some(false),
                allow: None,
                images: Vec::new(),
            },
        )
        .expect("task.send_now maps to SubmitProviderInput");
        assert!(matches!(envelope.command, Command::SubmitProviderInput(_)));

        let absolute = if cfg!(windows) {
            r"C:\repo\.devmanager\pasted-images\x.png"
        } else {
            "/repo/.devmanager/pasted-images/x.png"
        };
        let image =
            crate::domain::ProviderImageAttachment::try_new(absolute, [7; 32], 32).expect("image");
        let rejected = provider_input_command(
            CommandId::new(),
            ClientId::new(),
            1_725_000_000_100,
            2,
            ACTION_TASK_STEER_CURRENT_TURN,
            ProviderInputArguments {
                task_id,
                agent_session_id: AgentSessionId::new(),
                runtime_generation: 3,
                action_epoch: 4,
                turn_id: TurnId::new(),
                question_id: None,
                approval_id: None,
                text: Some("steer".into()),
                wait: None,
                allow: None,
                images: vec![image],
            },
        );
        assert!(matches!(
            rejected,
            Err(crate::domain::ProviderInputIntentError::ImagesUnsupported)
        ));
    }
}
