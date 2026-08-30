//! Typed Task Cockpit query/result contracts.
//!
//! These documents carry no raw secrets, credentials, command lines, or
//! client-authoritative absolute paths. Host resolves workspace identity from
//! the selected Task admission.

use serde::{Deserialize, Serialize};

use crate::domain::agent_resource::AgentResourceBinding;
use crate::domain::id::{
    AgentSessionId, ConfiguredServiceId, ResourceId, SubscriptionId, TaskId, TerminalId,
};
use crate::domain::task::{WorkspaceBindingKind, WorkspaceRef};
use crate::providers::ProviderKind;
use crate::terminal::protocol::TerminalSessionId;
use crate::terminal::session::TerminalScreenSnapshot;

pub const MAX_COCKPIT_FILE_LIST: u16 = 64;
pub const MAX_COCKPIT_READ_BYTES: u32 = 64 * 1024;
pub const MAX_COCKPIT_RELATIVE_PATH_BYTES: usize = 1024;
/// Bound on Task Cockpit repository catalog entries (Workspace + configured).
pub const MAX_TASK_REPOSITORIES: usize = 32;
/// Bound on opaque folder config ids carried by [`TaskRepositorySelector`].
pub const MAX_FOLDER_CONFIG_ID_BYTES: usize = 128;
/// Bound on redacted repository labels exposed on the wire.
pub const MAX_REPOSITORY_LABEL_BYTES: usize = 96;

/// Safe Task Cockpit projection of one exact stock-provider resource claim.
///
/// This is intentionally a separate projection from terminal bytes and
/// process identities.  A host may expose it only after the kernel has
/// validated the exact task/agent/resource tuple for the current generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct TaskAgentResourceProjection {
    pub task_id: TaskId,
    pub agent_session_id: AgentSessionId,
    pub resource_id: ResourceId,
    pub provider_kind: ProviderKind,
    pub runtime_generation: u64,
}

impl From<AgentResourceBinding> for TaskAgentResourceProjection {
    fn from(binding: AgentResourceBinding) -> Self {
        Self {
            task_id: binding.task_id,
            agent_session_id: binding.agent_session_id,
            resource_id: binding.resource_id,
            provider_kind: binding.provider_kind,
            runtime_generation: binding.runtime_generation,
        }
    }
}

/// Map the kernel's exact claim into a bounded Task Cockpit projection.
pub const fn task_agent_resource_projection(
    binding: AgentResourceBinding,
) -> TaskAgentResourceProjection {
    TaskAgentResourceProjection {
        task_id: binding.task_id,
        agent_session_id: binding.agent_session_id,
        resource_id: binding.resource_id,
        provider_kind: binding.provider_kind,
        runtime_generation: binding.runtime_generation,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskCockpitSurface {
    Conversation,
    Terminal,
    Workspace,
    Git,
    Files,
    Ssh,
    Services,
    Browser,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskCockpitDeniedReason {
    MissingTask,
    Unauthorized,
    PathTraversal,
    OutsideWorkspace,
    CapabilityDenied,
    StaleFence,
    UnknownService,
    ForeignScope,
    RevisionConflict,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskCockpitUnavailableReason {
    TerminalUnavailable,
    /// Host-attested: a provider launch/restore is queued or in flight for this task.
    TerminalStartPending,
    /// Host-attested: no pending launch, no live provider binding, and no
    /// persisted launch for the exact agent — safe to StartProviderSession once.
    TerminalNotStarted,
    /// Host-attested: live identityless Codex PTY shows a blocking trust/setup
    /// screen that requires user action on that host before composer input.
    TerminalProviderSetupRequired,
    GitAuthorityNotIssued,
    FileAuthorityNotIssued,
    SshOperationUnsupported,
    SshTaskSupervisorAdapterMissing,
    ServiceSupervisorUnavailable,
    WriteUnsupported,
    LogsUnsupported,
    HealthUnsupported,
    WorkspaceAuthorityUnavailable,
    BrowserProcessSessionUnavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum TaskGitMutateIntent {
    Stage { relative_paths: Vec<String> },
    Unstage { relative_paths: Vec<String> },
    Commit { message: String },
}

/// Opaque client-chosen repository target. Paths never cross this boundary;
/// the host resolves every locator from sealed config / workspace authority.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum TaskRepositorySelector {
    /// The Task's existing bound checkout / worktree.
    Workspace,
    /// The configured Project root for the Task's project.
    ProjectRoot,
    /// One active configured Project folder, by opaque folder config id.
    Folder { folder_config_id: String },
}

impl TaskRepositorySelector {
    /// Reject empty, oversized, control, or path-like folder config ids.
    /// Workspace / ProjectRoot selectors are always well-formed.
    pub fn validate(&self) -> Result<(), TaskRepositorySelectorError> {
        match self {
            Self::Workspace | Self::ProjectRoot => Ok(()),
            Self::Folder { folder_config_id } => validate_folder_config_id(folder_config_id),
        }
    }

    pub const fn kind(&self) -> TaskRepositoryKind {
        match self {
            Self::Workspace => TaskRepositoryKind::Workspace,
            Self::ProjectRoot => TaskRepositoryKind::ProjectRoot,
            Self::Folder { .. } => TaskRepositoryKind::ConfiguredFolder,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskRepositorySelectorError {
    EmptyFolderConfigId,
    OversizedFolderConfigId,
    InvalidFolderConfigId,
    PathLikeFolderConfigId,
}

/// Wire kind / scope for one catalogued repository target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskRepositoryKind {
    Workspace,
    ProjectRoot,
    ConfiguredFolder,
}

/// One bounded, path-redacted repository catalog entry for a Task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskRepositoryCatalogEntry {
    pub selector: TaskRepositorySelector,
    pub label: String,
    pub kind: TaskRepositoryKind,
    pub available: bool,
    pub read_only: bool,
}

/// Bounded catalog of repositories addressable for one Task / project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskGitRepositoriesProjection {
    pub task_id: TaskId,
    pub repositories: Vec<TaskRepositoryCatalogEntry>,
}

pub fn validate_folder_config_id(raw: &str) -> Result<(), TaskRepositorySelectorError> {
    let value = raw.trim();
    if value.is_empty() {
        return Err(TaskRepositorySelectorError::EmptyFolderConfigId);
    }
    if value.len() > MAX_FOLDER_CONFIG_ID_BYTES {
        return Err(TaskRepositorySelectorError::OversizedFolderConfigId);
    }
    if value.contains('\0') || value.chars().any(char::is_control) {
        return Err(TaskRepositorySelectorError::InvalidFolderConfigId);
    }
    if value.contains('/')
        || value.contains('\\')
        || value.contains("..")
        || value.starts_with('.')
        || value.as_bytes().get(1) == Some(&b':')
        || value.contains(':')
    {
        return Err(TaskRepositorySelectorError::PathLikeFolderConfigId);
    }
    Ok(())
}

pub fn redact_repository_label(label: &str) -> String {
    let filtered: String = label
        .chars()
        .filter(|character| !character.is_control())
        .collect();
    let trimmed = filtered.trim();
    if trimmed.is_empty() {
        return "Repository".to_owned();
    }
    let bounded = truncate_to_max_bytes(trimmed, MAX_REPOSITORY_LABEL_BYTES);
    if bounded.trim().is_empty() {
        "Repository".to_owned()
    } else {
        bounded
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum TaskCockpitQuery {
    /// Exact current primary-provider input fences from the owning host.
    ProviderInputState,
    /// Read-only host configuration summary for the native configuration rail.
    /// This carries labels and capability metadata only; roots, commands,
    /// environment values, and credential material remain host-private.
    ConfigSnapshot,
    /// Read-only Claude and Codex CLI authentication observation.
    AgentConnection,
    /// Exact host-owned live provider-process session for the selected Task.
    /// The client may mirror this identity into its local Browser gateway, but
    /// must never synthesize or infer a replacement id.
    BrowserProcessSession,
    /// Host-owned project creation. The host validates the folder, persists
    /// it through ConfigStore, and re-issues workspace authority. Clients
    /// never write `config.json`.
    ConfigCreateProject {
        name: String,
        root_path: String,
    },
    /// Create or update one project RunCommand through ConfigStore.
    ConfigUpsertCommand {
        project_id: String,
        folder_id: String,
        #[serde(default)]
        command_id: Option<String>,
        label: String,
        command: String,
    },
    /// Archive one project RunCommand through ConfigStore.
    ConfigArchiveCommand {
        project_id: String,
        folder_id: String,
        command_id: String,
    },
    /// Start one configured project command through the host service runtime.
    ConfigRunCommand {
        project_id: String,
        folder_id: String,
        command_id: String,
    },
    /// Narrow host-local read of one RunCommand's label + command text for
    /// edit. ConfigSnapshot intentionally redacts commands; this query never
    /// ships roots, env, or credentials.
    ConfigCommandDetail {
        project_id: String,
        folder_id: String,
        command_id: String,
    },
    /// Local-authority provider settings snapshot / refresh / mutate.
    /// Handled before task-id requirement; Connect maps these to deny.
    ProviderSettings(crate::providers::settings::ProviderSettingsHostRequest),
    /// Explicit local-only listener setup. Never authorized over Connect.
    RemoteAccess(crate::host::remote_setup::RemoteSetupRequest),
    /// Read the bounded provider-neutral semantic conversation retained for
    /// the selected Task. The cursor is exclusive; zero requests the current
    /// retained window from its beginning.
    Conversation {
        after_sequence: u64,
    },
    /// Open an ephemeral semantic-conversation subscription for the selected
    /// Task. Returns one initial page plus a subscription id; later dirtiness
    /// arrives as coalesced `ConversationDirty` notices (no per-token events).
    OpenConversationSubscription {
        after_sequence: u64,
    },
    /// Release one conversation subscription owned by this client/output/task.
    ReleaseConversationSubscription {
        subscription_id: SubscriptionId,
    },
    /// One bounded, task-owned terminal screen using the host's exact
    /// task/agent/resource generation fence. This query never launches a PTY.
    /// Missing attachment remains `TerminalUnavailable` for wire compatibility —
    /// it is not authoritative absence.
    Terminal,
    /// Move the exact task terminal's host-owned viewport through scrollback
    /// and return the resulting bounded screen. Positive values move toward
    /// older rows; negative values return toward the live prompt. This never
    /// writes PTY input unless the terminal application owns wheel reporting.
    TerminalScroll {
        delta_lines: i32,
    },
    /// Opt-in readiness classification for first-send. Shares the Terminal
    /// projection when live and chat-ready; otherwise may return
    /// `TerminalStartPending`, `TerminalProviderSetupRequired`, or
    /// `TerminalNotStarted`. Legacy `Terminal` callers never receive the new
    /// reasons. Older hosts reject this variant — clients must fail closed.
    TerminalReadiness,
    WorkspaceStatus,
    /// Bounded path-redacted catalog of repositories for the exact Task/project.
    GitRepositories,
    /// Compatibility shim: identical to `GitStatusTargeted { selector: Workspace }`.
    GitStatus,
    /// Explicitly targeted repository status for one opaque selector.
    GitStatusTargeted {
        selector: TaskRepositorySelector,
    },
    FilesList {
        relative_directory: Option<String>,
        limit: u16,
    },
    FilesRead {
        relative_path: String,
        max_bytes: u32,
    },
    FilesWrite {
        relative_path: String,
        utf8_contents: String,
        #[serde(default)]
        expected_sha256_hex: Option<String>,
        #[serde(default)]
        confirm: bool,
    },
    /// Compatibility shim: identical to `GitMutateTargeted { selector: Workspace, .. }`.
    GitMutate {
        intent: TaskGitMutateIntent,
        #[serde(default)]
        confirm: bool,
    },
    /// Explicitly targeted Git mutation for one opaque selector.
    GitMutateTargeted {
        selector: TaskRepositorySelector,
        intent: TaskGitMutateIntent,
        #[serde(default)]
        confirm: bool,
    },
    SshStatus,
    SshAction {
        endpoint_id: String,
    },
    ServiceSnapshots,
    ServiceLogs {
        service_id: ConfiguredServiceId,
        resource_generation: u64,
        connection_epoch: u64,
        action_epoch: u64,
    },
    ServiceHealth {
        service_id: ConfiguredServiceId,
        resource_generation: u64,
        connection_epoch: u64,
        action_epoch: u64,
    },
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
    /// Present for targeted status; legacy Workspace shims may omit or set Workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selector: Option<TaskRepositorySelector>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskSshLifecycle {
    Starting,
    Running,
    Stopping,
    Stopped,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskSshRuntimeError {
    CredentialUnavailable,
    HostKeyPrompt,
    Launch,
    StaleFence,
    Teardown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskSshRuntimeProjection {
    pub task_id: TaskId,
    pub agent_session_id: AgentSessionId,
    pub resource_id: ResourceId,
    pub runtime_generation: u64,
    pub action_epoch: u64,
    pub endpoint_id: String,
    pub lifecycle: TaskSshLifecycle,
    pub error: Option<TaskSshRuntimeError>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskSshProjection {
    pub task_id: TaskId,
    pub endpoints: Vec<TaskSshEndpoint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<TaskSshRuntimeProjection>,
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
#[serde(deny_unknown_fields)]
pub struct TaskServiceLogLine {
    pub observed_at_ms: u64,
    pub generation: u64,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskServiceLogs {
    pub task_id: TaskId,
    pub service_id: ConfiguredServiceId,
    pub generation: u64,
    pub lines: Vec<TaskServiceLogLine>,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskServiceHealth {
    pub task_id: TaskId,
    pub snapshot: TaskServiceSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskTerminalProjection {
    pub task_id: TaskId,
    pub terminal_id: TerminalId,
    pub session_id: TerminalSessionId,
    pub agent_session_id: AgentSessionId,
    pub resource_id: ResourceId,
    pub runtime_generation: u64,
    pub resource_generation: u64,
    pub action_epoch: u64,
    /// Host-attested exception for a live Codex runtime whose launch explicitly
    /// reports conversation identity unsupported. Never inferred from PTY text.
    #[serde(default)]
    pub accepts_input_without_conversation_id: bool,
    pub sequence: u64,
    pub title: Option<String>,
    /// Bounded plain-text rows used by the native IPC projection. The host
    /// deliberately does not serialize thousands of rich cell structs across
    /// the one-megabyte transport; the client rebuilds default-themed cells.
    #[serde(default)]
    pub text_lines: Vec<String>,
    pub screen: TerminalScreenSnapshot,
}

/// Bounded, redacted configuration projection issued by the host's canonical
/// ConfigStore.  It deliberately contains no absolute paths, command text,
/// environment values, or credential references.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigSidebarSnapshot {
    pub revision: u64,
    pub projects: Vec<ConfigSidebarProject>,
    pub servers: Vec<ConfigSidebarServer>,
    pub ssh_connections: Vec<ConfigSidebarSsh>,
    pub providers: Vec<ConfigSidebarProvider>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigSidebarProject {
    pub config_id: String,
    pub label: String,
    pub root_configured: bool,
    /// Host-issued opaque `ProjectId` used by `task.create.v2`. Empty when the
    /// mapping has not been issued yet. Never a filesystem path.
    #[serde(default)]
    pub workspace_id: String,
    pub folders: Vec<ConfigSidebarFolder>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigSidebarFolder {
    pub config_id: String,
    pub label: String,
    pub server_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigSidebarServer {
    pub project_id: String,
    pub folder_id: String,
    pub command_id: String,
    pub project_label: String,
    pub folder_label: String,
    pub label: String,
    pub port: Option<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigSidebarSsh {
    pub config_id: String,
    pub label: String,
    pub host: String,
    pub port: u16,
    pub username: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigSidebarProviderKind {
    Claude,
    Codex,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigSidebarProvider {
    pub provider: ConfigSidebarProviderKind,
    pub command_configured: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentPresence {
    Checking,
    SignedIn,
    NotSignedIn,
    NotFound,
    CheckFailed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConnectionRow {
    pub provider: ConfigSidebarProviderKind,
    pub presence: AgentPresence,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConnectionSnapshot {
    pub agents: Vec<AgentConnectionRow>,
    /// Tasks whose exact provider restore failed for the current host generation.
    /// Distinct from durable [`crate::domain::task::TaskAttention::Failed`].
    #[serde(default)]
    pub restore_failed_task_ids: Vec<crate::domain::id::TaskId>,
}

impl AgentConnectionSnapshot {
    pub fn connected(&self) -> bool {
        self.agents
            .iter()
            .any(|row| row.presence == AgentPresence::SignedIn)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum TaskCockpitResult {
    ProviderInputState(ProviderInputStateProjection),
    Config(ConfigSidebarSnapshot),
    ConfigCommandDetail(ConfigCommandDetailProjection),
    AgentConnection(AgentConnectionSnapshot),
    ProviderSettings(crate::providers::settings::ProviderSettingsReply),
    RemoteAccess(crate::host::remote_setup::RemoteSetupReply),
    BrowserProcessSession(BrowserProcessSessionProjection),
    Conversation(crate::domain::snapshot::SemanticJournalPage),
    ConversationSubscription {
        subscription_id: SubscriptionId,
        page: crate::domain::snapshot::SemanticJournalPage,
    },
    ConversationSubscriptionReleased {
        subscription_id: SubscriptionId,
    },
    Terminal(TaskTerminalProjection),
    Workspace(TaskWorkspaceProjection),
    GitRepositories(TaskGitRepositoriesProjection),
    Git(TaskGitProjection),
    FilesList(TaskFilesListProjection),
    FilesRead(TaskFilesReadProjection),
    Ssh(TaskSshProjection),
    Services(TaskServiceProjection),
    ServiceLogs(TaskServiceLogs),
    ServiceHealth(TaskServiceHealth),
    Denied {
        surface: TaskCockpitSurface,
        reason: TaskCockpitDeniedReason,
    },
    Unavailable {
        surface: TaskCockpitSurface,
        reason: TaskCockpitUnavailableReason,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrowserProcessSessionProjection {
    pub task_id: crate::domain::id::TaskId,
    pub process_session_id: String,
}

/// Input authority without full projection maps or raw provider output.
pub const MAX_PROVIDER_INPUT_STATE_WAITS: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderInputStateProjection {
    pub task_id: TaskId,
    pub task_revision: u64,
    pub action_epoch: u64,
    pub agent_session_id: Option<AgentSessionId>,
    pub runtime_generation: Option<u64>,
    pub agent_lifecycle: Option<crate::domain::agent::AgentSessionLifecycle>,
    pub provider_kind: Option<ProviderKind>,
    pub provider_session_id: Option<crate::domain::agent::ProviderSessionId>,
    pub current_turn: Option<crate::domain::id::TurnId>,
    pub open_question: Option<crate::domain::id::QuestionId>,
    pub open_approval: Option<crate::domain::id::ApprovalId>,
    pub pending_wait_command_ids: Vec<crate::domain::id::CommandId>,
}

impl ProviderInputStateProjection {
    pub fn from_snapshot(snapshot: &crate::domain::snapshot::TaskSnapshot) -> Self {
        let agent = snapshot
            .primary_agent_id
            .and_then(|id| snapshot.agents.get(&id))
            .filter(|agent| agent.task_id == snapshot.task.id);
        let input = agent.and_then(|agent| snapshot.provider_sessions.get(&agent.id));
        Self {
            task_id: snapshot.task.id,
            task_revision: snapshot.task.revision,
            action_epoch: snapshot.task.action_epoch,
            agent_session_id: agent.map(|a| a.id),
            runtime_generation: agent.map(|a| a.runtime_generation),
            agent_lifecycle: agent.map(|a| a.lifecycle),
            provider_kind: agent.map(|a| a.provider_kind.clone()),
            provider_session_id: agent.and_then(|a| a.provider_session_id.clone()),
            current_turn: input.and_then(|p| p.current_turn),
            open_question: input.and_then(|p| p.open_question),
            open_approval: input.and_then(|p| p.open_approval),
            pending_wait_command_ids: input
                .map(|p| {
                    p.waits
                        .iter()
                        .filter_map(|(id, wait)| wait.pending.then_some(*id))
                        .take(MAX_PROVIDER_INPUT_STATE_WAITS + 1)
                        .collect()
                })
                .unwrap_or_default(),
        }
    }
}

/// Bounded host-local RunCommand detail for edit. Command text is included;
/// roots, env files, and credentials stay host-private.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigCommandDetailProjection {
    pub project_id: String,
    pub folder_id: String,
    pub command_id: String,
    pub label: String,
    pub command: String,
}

/// Truncate `text` to at most `max_bytes` without splitting a UTF-8 character.
pub fn truncate_to_max_bytes(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

pub fn relative_path_is_safe(path: &str) -> bool {
    if path.is_empty() || path.len() > MAX_COCKPIT_RELATIVE_PATH_BYTES {
        return false;
    }
    if path.contains('\\')
        || path.starts_with('/')
        || path.as_bytes().get(1) == Some(&b':')
        || path
            .split(['/', '\\'])
            .any(|component| component.is_empty() || component == "." || component == "..")
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
        selector: Some(TaskRepositorySelector::Workspace),
        label: Some("Workspace".to_owned()),
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
        TaskCockpitQuery::ConfigSnapshot
        | TaskCockpitQuery::AgentConnection
        | TaskCockpitQuery::ConfigCreateProject { .. }
        | TaskCockpitQuery::ConfigUpsertCommand { .. }
        | TaskCockpitQuery::ConfigArchiveCommand { .. }
        | TaskCockpitQuery::ConfigRunCommand { .. }
        | TaskCockpitQuery::ConfigCommandDetail { .. }
        | TaskCockpitQuery::ProviderSettings(_)
        | TaskCockpitQuery::RemoteAccess(_) => TaskCockpitSurface::Workspace,
        TaskCockpitQuery::BrowserProcessSession => TaskCockpitSurface::Browser,
        TaskCockpitQuery::Conversation { .. }
        | TaskCockpitQuery::OpenConversationSubscription { .. }
        | TaskCockpitQuery::ReleaseConversationSubscription { .. }
        | TaskCockpitQuery::ProviderInputState => TaskCockpitSurface::Conversation,
        TaskCockpitQuery::Terminal
        | TaskCockpitQuery::TerminalScroll { .. }
        | TaskCockpitQuery::TerminalReadiness => TaskCockpitSurface::Terminal,
        TaskCockpitQuery::WorkspaceStatus => TaskCockpitSurface::Workspace,
        TaskCockpitQuery::GitRepositories
        | TaskCockpitQuery::GitStatus
        | TaskCockpitQuery::GitStatusTargeted { .. }
        | TaskCockpitQuery::GitMutate { .. }
        | TaskCockpitQuery::GitMutateTargeted { .. } => TaskCockpitSurface::Git,
        TaskCockpitQuery::FilesList { .. }
        | TaskCockpitQuery::FilesRead { .. }
        | TaskCockpitQuery::FilesWrite { .. } => TaskCockpitSurface::Files,
        TaskCockpitQuery::SshStatus | TaskCockpitQuery::SshAction { .. } => TaskCockpitSurface::Ssh,
        TaskCockpitQuery::ServiceSnapshots
        | TaskCockpitQuery::ServiceLogs { .. }
        | TaskCockpitQuery::ServiceHealth { .. } => TaskCockpitSurface::Services,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::id::ServiceId;
    use crate::domain::query::{Query, QueryResult};

    #[test]
    fn truncate_to_max_bytes_stays_on_utf8_boundaries() {
        let text = "aé🎉";
        assert_eq!(truncate_to_max_bytes(text, text.len()), text);
        let truncated = truncate_to_max_bytes(text, 3);
        assert!(truncated.len() <= 3);
        assert!(truncated.is_char_boundary(truncated.len()));
        assert!(!truncated.contains('🎉'));
    }

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

        let create = Query::TaskCockpit(TaskCockpitQuery::ConfigCreateProject {
            name: "demo".into(),
            root_path: "C:/repo".into(),
        });
        let encoded = serde_json::to_value(&create).expect("encode create");
        assert!(encoded
            .get("task_cockpit")
            .and_then(|value| value.get("config_create_project"))
            .is_some());
        let decoded: Query = serde_json::from_value(encoded).expect("decode create");
        assert_eq!(decoded, create);

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
        assert_eq!(
            payload.get("service_id").and_then(|value| value.as_str()),
            Some("api")
        );
        let text = payload.to_string();
        assert!(
            !text.contains("019"),
            "must not look like a UUIDv7 service id: {text}"
        );
        let decoded: Command = serde_json::from_value(encoded).expect("decode");
        let Command::ServiceControl(intent) = decoded else {
            panic!("round trip");
        };
        assert_eq!(intent.service_id.as_str(), "api");
    }

    #[test]
    fn service_logs_query_wires_configured_string_not_uuid() {
        let query = Query::TaskCockpit(TaskCockpitQuery::ServiceLogs {
            service_id: ConfiguredServiceId::new("api").expect("catalog"),
            resource_generation: 1,
            connection_epoch: 2,
            action_epoch: 3,
        });
        let encoded = serde_json::to_value(&query).expect("encode");
        let payload = encoded
            .get("task_cockpit")
            .and_then(|value| value.get("service_logs"))
            .expect("service logs");
        assert_eq!(
            payload.get("service_id").and_then(|value| value.as_str()),
            Some("api")
        );
        let text = payload.to_string();
        assert!(
            !text.contains("019"),
            "must not look like a UUIDv7 service id: {text}"
        );
    }

    #[test]
    fn mutate_queries_require_typed_payloads_not_raw_paths_or_commands() {
        let write = Query::TaskCockpit(TaskCockpitQuery::FilesWrite {
            relative_path: "notes.txt".into(),
            utf8_contents: "hello".into(),
            expected_sha256_hex: None,
            confirm: true,
        });
        let encoded = serde_json::to_value(&write).expect("encode write");
        let payload = encoded
            .get("task_cockpit")
            .and_then(|value| value.get("files_write"))
            .expect("files_write");
        assert_eq!(
            payload
                .get("relative_path")
                .and_then(|value| value.as_str()),
            Some("notes.txt")
        );
        assert!(payload.get("utf8_contents").is_some());
        assert!(payload.get("command").is_none());
        let decoded: Query = serde_json::from_value(encoded).expect("decode write");
        assert_eq!(decoded, write);

        let mutate = Query::TaskCockpit(TaskCockpitQuery::GitMutate {
            intent: TaskGitMutateIntent::Stage {
                relative_paths: vec!["README.md".into()],
            },
            confirm: true,
        });
        let encoded = serde_json::to_value(&mutate).expect("encode mutate");
        let payload = encoded
            .get("task_cockpit")
            .and_then(|value| value.get("git_mutate"))
            .expect("git_mutate");
        assert!(payload.get("intent").is_some());
        assert!(payload.get("command").is_none());
        let decoded: Query = serde_json::from_value(encoded).expect("decode mutate");
        assert_eq!(decoded, mutate);
    }

    fn workspace_projection_omits_raw_paths() {
        let task_id = TaskId::new();
        let projection = workspace_projection(task_id, &WorkspaceRef::Main);
        let encoded = serde_json::to_string(&projection).expect("encode");
        assert!(!encoded.contains("C:"));
        assert!(!encoded.contains("/"));
        assert_eq!(projection.kind, TaskWorkspaceKind::Main);
        assert!(!projection.bound);
    }

    #[test]
    fn agent_connection_snapshot_is_connected_when_one_agent_is_signed_in() {
        let snapshot = AgentConnectionSnapshot {
            agents: vec![
                AgentConnectionRow {
                    provider: ConfigSidebarProviderKind::Claude,
                    presence: AgentPresence::NotFound,
                },
                AgentConnectionRow {
                    provider: ConfigSidebarProviderKind::Codex,
                    presence: AgentPresence::SignedIn,
                },
            ],
            restore_failed_task_ids: Vec::new(),
        };
        assert!(snapshot.connected());
        assert!(!AgentConnectionSnapshot {
            agents: vec![],
            restore_failed_task_ids: Vec::new(),
        }
        .connected());
    }

    #[test]
    fn repository_selector_rejects_path_like_folder_ids() {
        assert!(TaskRepositorySelector::Workspace.validate().is_ok());
        assert!(TaskRepositorySelector::ProjectRoot.validate().is_ok());
        assert!(TaskRepositorySelector::Folder {
            folder_config_id: "apps-api".into(),
        }
        .validate()
        .is_ok());
        assert_eq!(
            TaskRepositorySelector::Folder {
                folder_config_id: "C:/repos/api".into(),
            }
            .validate(),
            Err(TaskRepositorySelectorError::PathLikeFolderConfigId)
        );
        assert_eq!(
            TaskRepositorySelector::Folder {
                folder_config_id: "../secret".into(),
            }
            .validate(),
            Err(TaskRepositorySelectorError::PathLikeFolderConfigId)
        );
        assert_eq!(
            TaskRepositorySelector::Folder {
                folder_config_id: "foo/bar".into(),
            }
            .validate(),
            Err(TaskRepositorySelectorError::PathLikeFolderConfigId)
        );
        assert_eq!(
            TaskRepositorySelector::Folder {
                folder_config_id: String::new(),
            }
            .validate(),
            Err(TaskRepositorySelectorError::EmptyFolderConfigId)
        );
    }

    #[test]
    fn redact_repository_label_is_utf8_byte_bounded_without_splitting_characters() {
        assert_eq!(redact_repository_label("  plain  "), "plain");
        assert_eq!(redact_repository_label("\u{0001}\u{0007}"), "Repository");
        assert_eq!(redact_repository_label("   \t  "), "Repository");
        let multibyte = "á".repeat(80);
        let redacted = redact_repository_label(&multibyte);
        assert!(redacted.len() <= MAX_REPOSITORY_LABEL_BYTES);
        assert!(redacted.is_char_boundary(redacted.len()));
        assert!(!redacted.is_empty());
        assert_eq!(
            redacted.chars().count(),
            MAX_REPOSITORY_LABEL_BYTES / "á".len()
        );
        let mixed = format!("{}🎉", "a".repeat(95));
        let redacted = redact_repository_label(&mixed);
        assert!(redacted.len() <= MAX_REPOSITORY_LABEL_BYTES);
        assert!(redacted.is_char_boundary(redacted.len()));
        assert!(!redacted.contains('🎉'));
    }

    #[test]
    fn git_repositories_and_targeted_status_roundtrip_without_paths() {
        let task_id = TaskId::new();
        let catalog = Query::TaskCockpit(TaskCockpitQuery::GitRepositories);
        let encoded = serde_json::to_value(&catalog).expect("encode catalog query");
        assert_eq!(
            encoded.get("task_cockpit").and_then(|value| value.as_str()),
            Some("git_repositories")
        );
        let decoded: Query = serde_json::from_value(encoded).expect("decode catalog query");
        assert_eq!(decoded, catalog);

        let targeted = Query::TaskCockpit(TaskCockpitQuery::GitStatusTargeted {
            selector: TaskRepositorySelector::Folder {
                folder_config_id: "sibling-b".into(),
            },
        });
        let encoded = serde_json::to_value(&targeted).expect("encode targeted");
        let payload = encoded
            .get("task_cockpit")
            .and_then(|value| value.get("git_status_targeted"))
            .expect("targeted payload");
        let text = payload.to_string();
        assert!(!text.contains("C:"));
        assert!(!text.contains("/repos/"));
        assert!(!text.contains('\\'));
        let decoded: Query = serde_json::from_value(encoded).expect("decode targeted");
        assert_eq!(decoded, targeted);

        let result = QueryResult::TaskCockpit(TaskCockpitResult::GitRepositories(
            TaskGitRepositoriesProjection {
                task_id,
                repositories: vec![
                    TaskRepositoryCatalogEntry {
                        selector: TaskRepositorySelector::Workspace,
                        label: "Workspace".into(),
                        kind: TaskRepositoryKind::Workspace,
                        available: true,
                        read_only: false,
                    },
                    TaskRepositoryCatalogEntry {
                        selector: TaskRepositorySelector::ProjectRoot,
                        label: "Project".into(),
                        kind: TaskRepositoryKind::ProjectRoot,
                        available: true,
                        read_only: false,
                    },
                ],
            },
        ));
        let encoded = serde_json::to_string(&result).expect("encode result");
        assert!(!encoded.contains("C:"));
        assert!(!encoded.contains("/home/"));
        let decoded: QueryResult = serde_json::from_str(&encoded).expect("decode result");
        assert_eq!(decoded, result);

        let status = QueryResult::TaskCockpit(TaskCockpitResult::Git(TaskGitProjection {
            task_id,
            selector: Some(TaskRepositorySelector::Folder {
                folder_config_id: "sibling-b".into(),
            }),
            label: Some("Sibling B".into()),
            branch: Some("main".into()),
            ahead: 1,
            behind: 0,
            change_count: 2,
            detached: false,
        }));
        let encoded = serde_json::to_string(&status).expect("encode status");
        assert!(!encoded.contains("C:"));
        assert!(!encoded.contains("folder_path"));
        let decoded: QueryResult = serde_json::from_str(&encoded).expect("decode status");
        assert_eq!(decoded, status);
    }

    #[test]
    fn legacy_git_status_and_mutate_remain_workspace_shims_on_the_wire() {
        let status = Query::TaskCockpit(TaskCockpitQuery::GitStatus);
        let encoded = serde_json::to_value(&status).expect("encode status");
        assert_eq!(
            encoded.get("task_cockpit").and_then(|value| value.as_str()),
            Some("git_status")
        );
        assert!(encoded
            .get("task_cockpit")
            .and_then(|value| value.get("selector"))
            .is_none());

        let mutate = Query::TaskCockpit(TaskCockpitQuery::GitMutate {
            intent: TaskGitMutateIntent::Commit {
                message: "ship".into(),
            },
            confirm: true,
        });
        let encoded = serde_json::to_value(&mutate).expect("encode mutate");
        let payload = encoded
            .get("task_cockpit")
            .and_then(|value| value.get("git_mutate"))
            .expect("git_mutate");
        assert!(payload.get("selector").is_none());
        assert_eq!(
            cockpit_surface(&TaskCockpitQuery::GitStatus),
            TaskCockpitSurface::Git
        );
        assert_eq!(
            cockpit_surface(&TaskCockpitQuery::GitStatusTargeted {
                selector: TaskRepositorySelector::Workspace,
            }),
            TaskCockpitSurface::Git
        );
        assert_eq!(
            cockpit_surface(&TaskCockpitQuery::GitRepositories),
            TaskCockpitSurface::Git
        );
    }

    #[test]
    fn terminal_query_routes_to_the_terminal_surface() {
        assert_eq!(
            cockpit_surface(&TaskCockpitQuery::Terminal),
            TaskCockpitSurface::Terminal
        );
        let encoded = serde_json::to_value(Query::TaskCockpit(TaskCockpitQuery::Terminal))
            .expect("encode terminal query");
        assert_eq!(
            encoded.get("task_cockpit").and_then(|value| value.as_str()),
            Some("terminal")
        );
    }

    #[test]
    fn terminal_readiness_is_opt_in_wire_variant_on_terminal_surface() {
        assert_eq!(
            cockpit_surface(&TaskCockpitQuery::TerminalReadiness),
            TaskCockpitSurface::Terminal
        );
        let encoded = serde_json::to_value(Query::TaskCockpit(TaskCockpitQuery::TerminalReadiness))
            .expect("encode readiness");
        assert_eq!(
            encoded.get("task_cockpit").and_then(|value| value.as_str()),
            Some("terminal_readiness")
        );
        let unavailable = TaskCockpitResult::Unavailable {
            surface: TaskCockpitSurface::Terminal,
            reason: TaskCockpitUnavailableReason::TerminalNotStarted,
        };
        let round_trip: TaskCockpitResult =
            serde_json::from_value(serde_json::to_value(&unavailable).expect("encode"))
                .expect("decode");
        assert_eq!(round_trip, unavailable);
        let setup_required = TaskCockpitResult::Unavailable {
            surface: TaskCockpitSurface::Terminal,
            reason: TaskCockpitUnavailableReason::TerminalProviderSetupRequired,
        };
        let setup_round_trip: TaskCockpitResult =
            serde_json::from_value(serde_json::to_value(&setup_required).expect("encode"))
                .expect("decode setup reason");
        assert_eq!(setup_round_trip, setup_required);
    }

    #[test]
    fn targeted_mutate_binds_selector_on_the_wire() {
        let mutate = Query::TaskCockpit(TaskCockpitQuery::GitMutateTargeted {
            selector: TaskRepositorySelector::ProjectRoot,
            intent: TaskGitMutateIntent::Stage {
                relative_paths: vec!["README.md".into()],
            },
            confirm: false,
        });
        let encoded = serde_json::to_value(&mutate).expect("encode");
        let payload = encoded
            .get("task_cockpit")
            .and_then(|value| value.get("git_mutate_targeted"))
            .expect("targeted mutate");
        assert_eq!(
            payload.get("selector").and_then(|value| value.as_str()),
            Some("project_root")
        );
        let decoded: Query = serde_json::from_value(encoded).expect("decode");
        assert_eq!(decoded, mutate);
    }
}
