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
use crate::terminal::protocol::{FocusEpoch, TerminalSessionId};
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
    /// Resize the exact task-owned PTY to the native terminal grid measured by
    /// the client. The host validates the bounded dimensions and never creates
    /// or retargets a terminal through this query.
    TerminalResize {
        cols: u16,
        rows: u16,
    },
    /// Opt-in readiness classification for first-send. Shares the Terminal
    /// projection when live and chat-ready; otherwise may return
    /// `TerminalStartPending`, `TerminalProviderSetupRequired`, or
    /// `TerminalNotStarted`. Legacy `Terminal` callers never receive the new
    /// reasons. Older hosts reject this variant — clients must fail closed.
    TerminalReadiness,
    /// One bounded terminal screen addressed by its durable resource. This is
    /// the plain-shell form of [`Self::Terminal`]; the legacy unit variants
    /// above keep meaning "the provider terminal" so older clients and hosts
    /// stay wire-compatible. A shell carries no agent session, so the host
    /// fences it on resource lifecycle and generation only.
    TerminalFor {
        resource_id: ResourceId,
    },
    /// [`Self::TerminalScroll`] addressed by durable resource.
    TerminalScrollFor {
        resource_id: ResourceId,
        delta_lines: i32,
    },
    /// [`Self::TerminalResize`] addressed by durable resource.
    TerminalResizeFor {
        resource_id: ResourceId,
        cols: u16,
        rows: u16,
    },
    /// [`Self::TerminalReadiness`] addressed by durable resource.
    TerminalReadinessFor {
        resource_id: ResourceId,
    },
    /// The Task's whole terminal strip: the provider terminal first, then the
    /// durable plain-shell order. This is a chip list only — it carries no
    /// screen bytes and never launches or attaches a PTY.
    TaskTerminals,
    /// Host-authority request for one new plain shell terminal on this Task.
    ///
    /// The client never supplies a program or arguments: the host resolves the
    /// working directory and the shell executable, builds the durable
    /// `ResourceFacts`, and executes `Command::OpenShellTerminal` on the
    /// client's behalf under `expected_task_revision`. It is admitted as a
    /// typed query for the same reason `ConfigRunCommand` is — the effect is
    /// owned by the exclusive host executor, not by a client-built envelope.
    /// Older hosts reject this variant, so clients must fail closed.
    OpenShellTerminal {
        /// Optional absolute working directory. `None` means the Task's own
        /// runtime working directory, which only the host can resolve.
        #[serde(default)]
        cwd: Option<String>,
        expected_task_revision: u64,
    },
    WorkspaceStatus,
    /// Bounded path-redacted catalog of repositories for the exact Task/project.
    GitRepositories,
    /// Compatibility shim: identical to `GitStatusTargeted { selector: Workspace }`.
    GitStatus,
    /// Explicitly targeted repository status for one opaque selector.
    GitStatusTargeted {
        selector: TaskRepositorySelector,
    },
    /// Bounded host-owned diff for one repository-relative changed file.
    GitFileDiffTargeted {
        selector: TaskRepositorySelector,
        relative_path: String,
        staged: bool,
    },
    /// Bounded recent history for one selected repository.
    GitHistoryTargeted {
        selector: TaskRepositorySelector,
        limit: u16,
        skip: u32,
    },
    /// Bounded patch for one exact commit selected from Git history.
    GitCommitDiffTargeted {
        selector: TaskRepositorySelector,
        commit_hash: String,
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
    /// Bounded, workspace-relative status rows for the native Git window.
    /// Paths are repository-relative and remain bound to `selector`; no host
    /// filesystem root crosses the client boundary.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entries: Vec<TaskGitEntryProjection>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskGitEntryStatus {
    Modified,
    Added,
    Deleted,
    Renamed,
    Copied,
    TypeChanged,
    Untracked,
    Conflict,
    Submodule,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskGitEntryProjection {
    pub relative_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_relative_path: Option<String>,
    pub status: TaskGitEntryStatus,
    pub staged: bool,
    pub unstaged: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskGitFileDiffProjection {
    pub task_id: TaskId,
    pub selector: TaskRepositorySelector,
    pub relative_path: String,
    pub staged: bool,
    pub diff: crate::git::git_service::GitDiffResult,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskGitHistoryProjection {
    pub task_id: TaskId,
    pub selector: TaskRepositorySelector,
    pub entries: Vec<crate::git::git_service::GitLogEntry>,
    pub skip: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskGitCommitDiffProjection {
    pub task_id: TaskId,
    pub selector: TaskRepositorySelector,
    pub commit_hash: String,
    pub diff: crate::git::git_service::GitDiffResult,
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

/// Live runtime state of one hosted terminal, on the wire.
///
/// One-to-one with `crate::terminal::service::TerminalRuntimeState`. `Running`
/// is the default so a projection encoded by an older host still decodes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TerminalRuntimeStateWire {
    #[default]
    Running,
    Exited {
        summary: String,
    },
    /// Boot window only: a durable terminal not yet reconciled with a runtime,
    /// or one whose hosted entry has already been retired.
    Unknown,
}

/// One bounded terminal screen.
///
/// `agent_session_id`, `runtime_generation` and `action_epoch` stay required on
/// the wire so every existing client reader keeps compiling. A plain shell has
/// none of them: it sends `AgentSessionId::nil()`, `runtime_generation: 0`, and
/// an `action_epoch` of zero unless the host holds one. Read `is_provider`
/// before trusting any of those three — `is_provider == false` means the fences
/// are sentinels, not identity, and the provider input path must reject them.
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
    /// Exact host-owned focus fence required for raw input to this PTY.
    pub focus_epoch: FocusEpoch,
    /// Last raw input sequence accepted by the host for this terminal generation.
    /// The next request must use this value plus one.
    pub accepted_input_sequence: u64,
    /// Host-attested exception for a live Codex runtime whose launch explicitly
    /// reports conversation identity unsupported. Never inferred from PTY text.
    #[serde(default)]
    pub accepts_input_without_conversation_id: bool,
    pub sequence: u64,
    pub title: Option<String>,
    /// Bounded plain-text rows used by the native IPC projection. The host
    /// avoids serializing thousands of default cells across the one-megabyte
    /// transport; the client rebuilds those rows and overlays only sparse
    /// styled cells retained in `screen.cells`.
    #[serde(default)]
    pub text_lines: Vec<String>,
    pub screen: TerminalScreenSnapshot,
    /// False for a plain shell. Defaults to false only for a projection that
    /// predates plain shells, which older hosts only ever issued for the
    /// provider slot; those hosts also send a real `agent_session_id`, so the
    /// client's provider path stays gated on identity, never on this flag alone.
    #[serde(default)]
    pub is_provider: bool,
    #[serde(default)]
    pub runtime_state: TerminalRuntimeStateWire,
}

/// One chip of a Task's terminal strip: identity, label and liveness, with no
/// screen bytes. The host answers this from the durable facts, so a shell whose
/// hosted entry has already been retired still renders until `ResourceReleased`
/// removes it from the strip.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskTerminalChip {
    pub resource_id: ResourceId,
    pub is_provider: bool,
    /// The user-set terminal title, when one was recorded.
    pub title: Option<String>,
    /// Fallback display text when there is no title: the launch program's file
    /// stem for a shell (`pwsh`, `cmd`), `terminal` for the provider slot.
    pub label: String,
    pub runtime_state: TerminalRuntimeStateWire,
    /// Redacted working directory, never an absolute host path. Inside the
    /// task's workspace root it is the path relative to that root, with `.`
    /// meaning the root itself; anywhere else it is the final path component
    /// alone. Display only — it is not a path a client may open or send back.
    pub live_cwd: Option<String>,
    pub exit: Option<crate::domain::terminal_facts::TerminalExit>,
    pub created_at_ms: i64,
    pub last_activity_at_ms: i64,
}

/// The Task's whole terminal strip.
///
/// `terminals` leads with the provider chip when one exists, then follows
/// `order` exactly. `focused: None` is a valid state and means no chip is
/// selected, not that focus is unknown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskTerminalsProjection {
    pub task_id: TaskId,
    pub terminals: Vec<TaskTerminalChip>,
    /// The durable plain-shell order. The provider chip is never in it.
    pub order: Vec<ResourceId>,
    pub focused: Option<ResourceId>,
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
    TaskTerminals(TaskTerminalsProjection),
    Workspace(TaskWorkspaceProjection),
    GitRepositories(TaskGitRepositoriesProjection),
    Git(TaskGitProjection),
    GitFileDiff(TaskGitFileDiffProjection),
    GitHistory(TaskGitHistoryProjection),
    GitCommitDiff(TaskGitCommitDiffProjection),
    FilesList(TaskFilesListProjection),
    FilesRead(TaskFilesReadProjection),
    Ssh(TaskSshProjection),
    Services(TaskServiceProjection),
    ServiceLogs(TaskServiceLogs),
    ServiceHealth(TaskServiceHealth),
    Denied {
        surface: TaskCockpitSurface,
        reason: TaskCockpitDeniedReason,
        /// One host-written sentence naming the exact resolved value behind
        /// `reason`, when the closed enum cannot carry it.
        ///
        /// `OutsideWorkspace` alone cannot tell "that directory is gone" from
        /// "you gave me a relative path", and the operator reading the client
        /// has no access to the host's stderr. Additive and optional: the wire
        /// codec is `rmp_serde::to_vec_named`, so an older payload without the
        /// field still decodes, and a `None` is not written at all.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    Unavailable {
        surface: TaskCockpitSurface,
        reason: TaskCockpitUnavailableReason,
        /// See [`TaskCockpitResult::Denied::detail`]. `TerminalUnavailable`
        /// covers both "no shell is installed" and "the recipe was rejected".
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
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
    pub resource_id: Option<ResourceId>,
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
        let resource_id = agent.and_then(|agent| {
            snapshot.resources.values().find_map(|resource| {
                AgentResourceBinding::from_facts(agent, resource)
                    .ok()
                    .map(|binding| binding.resource_id)
            })
        });
        Self {
            task_id: snapshot.task.id,
            task_revision: snapshot.task.revision,
            action_epoch: snapshot.task.action_epoch,
            agent_session_id: agent.map(|a| a.id),
            resource_id,
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
        entries: Vec::new(),
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
        | TaskCockpitQuery::TerminalResize { .. }
        | TaskCockpitQuery::TerminalReadiness
        | TaskCockpitQuery::TerminalFor { .. }
        | TaskCockpitQuery::TerminalScrollFor { .. }
        | TaskCockpitQuery::TerminalResizeFor { .. }
        | TaskCockpitQuery::TerminalReadinessFor { .. }
        | TaskCockpitQuery::TaskTerminals
        | TaskCockpitQuery::OpenShellTerminal { .. } => TaskCockpitSurface::Terminal,
        TaskCockpitQuery::WorkspaceStatus => TaskCockpitSurface::Workspace,
        TaskCockpitQuery::GitRepositories
        | TaskCockpitQuery::GitStatus
        | TaskCockpitQuery::GitStatusTargeted { .. }
        | TaskCockpitQuery::GitFileDiffTargeted { .. }
        | TaskCockpitQuery::GitHistoryTargeted { .. }
        | TaskCockpitQuery::GitCommitDiffTargeted { .. }
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
            detail: None,
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
            entries: Vec::new(),
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
    fn terminal_resize_is_a_typed_terminal_surface_query() {
        let query = TaskCockpitQuery::TerminalResize {
            cols: 120,
            rows: 42,
        };
        assert_eq!(cockpit_surface(&query), TaskCockpitSurface::Terminal);
        let encoded = serde_json::to_value(Query::TaskCockpit(query)).expect("encode resize");
        assert_eq!(
            encoded
                .get("task_cockpit")
                .and_then(|value| value.get("terminal_resize"))
                .and_then(|value| value.get("rows"))
                .and_then(|value| value.as_u64()),
            Some(42)
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
            detail: None,
        };
        let round_trip: TaskCockpitResult =
            serde_json::from_value(serde_json::to_value(&unavailable).expect("encode"))
                .expect("decode");
        assert_eq!(round_trip, unavailable);
        let setup_required = TaskCockpitResult::Unavailable {
            surface: TaskCockpitSurface::Terminal,
            reason: TaskCockpitUnavailableReason::TerminalProviderSetupRequired,
            detail: None,
        };
        let setup_round_trip: TaskCockpitResult =
            serde_json::from_value(serde_json::to_value(&setup_required).expect("encode"))
                .expect("decode setup reason");
        assert_eq!(setup_round_trip, setup_required);
    }

    /// `detail` is additive on a live wire shape, so both directions matter:
    /// a payload written before the field existed must still decode, and a
    /// refusal that has no named cause must not start emitting a null.
    ///
    /// Asserted through [`MessagePackCodec`], which is what the transport
    /// actually uses (`rmp_serde::to_vec_named`, a self-describing map) — not
    /// only through `serde_json`, which could agree while the real codec did
    /// not.
    #[test]
    fn cockpit_refusals_carry_an_optional_detail_additively() {
        use crate::protocol::{FrameLimits, MessagePackCodec};

        let codec = MessagePackCodec::from_limits(FrameLimits::v1_default()).expect("codec");

        // 1. The OLD shape — surface + reason, no `detail` — still decodes.
        let legacy_denied: TaskCockpitResult = codec
            .decode(
                &codec
                    .encode(&serde_json::json!({
                        "denied": {
                            "surface": "terminal",
                            "reason": "outside_workspace",
                        }
                    }))
                    .expect("encode legacy denied"),
            )
            .expect("a payload written before `detail` existed must still decode");
        assert_eq!(
            legacy_denied,
            TaskCockpitResult::Denied {
                surface: TaskCockpitSurface::Terminal,
                reason: TaskCockpitDeniedReason::OutsideWorkspace,
                detail: None,
            }
        );
        let legacy_unavailable: TaskCockpitResult = codec
            .decode(
                &codec
                    .encode(&serde_json::json!({
                        "unavailable": {
                            "surface": "terminal",
                            "reason": "terminal_unavailable",
                        }
                    }))
                    .expect("encode legacy unavailable"),
            )
            .expect("a payload written before `detail` existed must still decode");
        assert_eq!(
            legacy_unavailable,
            TaskCockpitResult::Unavailable {
                surface: TaskCockpitSurface::Terminal,
                reason: TaskCockpitUnavailableReason::TerminalUnavailable,
                detail: None,
            }
        );

        // 2. A refusal with no named cause writes exactly the old bytes, so a
        //    reader that has not been updated sees no change at all.
        assert_eq!(
            codec
                .encode(&TaskCockpitResult::Denied {
                    surface: TaskCockpitSurface::Terminal,
                    reason: TaskCockpitDeniedReason::OutsideWorkspace,
                    detail: None,
                })
                .expect("encode denied without detail"),
            codec
                .encode(&serde_json::json!({
                    "denied": { "surface": "terminal", "reason": "outside_workspace" }
                }))
                .expect("encode legacy denied"),
            "a None detail must not be written at all"
        );

        // 3. A named cause survives the round trip verbatim.
        for named in [
            TaskCockpitResult::Denied {
                surface: TaskCockpitSurface::Terminal,
                reason: TaskCockpitDeniedReason::OutsideWorkspace,
                detail: Some("cwd is not a directory: D:/gone".into()),
            },
            TaskCockpitResult::Unavailable {
                surface: TaskCockpitSurface::Terminal,
                reason: TaskCockpitUnavailableReason::TerminalUnavailable,
                detail: Some(
                    "no shell found; tried: pwsh, cmd.exe (powershell.exe excluded)".into(),
                ),
            },
        ] {
            let round_trip: TaskCockpitResult = codec
                .decode(&codec.encode(&named).expect("encode named"))
                .expect("decode named");
            assert_eq!(round_trip, named);
            let json_round_trip: TaskCockpitResult =
                serde_json::from_value(serde_json::to_value(&named).expect("json encode"))
                    .expect("json decode");
            assert_eq!(json_round_trip, named);
        }
    }

    #[test]
    fn resource_addressed_terminal_queries_keep_the_legacy_wire_forms() {
        // The legacy unit variants are the provider terminal and must keep
        // decoding byte-for-byte: an older client sends exactly these.
        for legacy in ["\"terminal\"", "\"terminal_readiness\""] {
            let decoded: TaskCockpitQuery =
                serde_json::from_str(legacy).expect("legacy unit form still decodes");
            assert_eq!(cockpit_surface(&decoded), TaskCockpitSurface::Terminal);
        }
        let resource_id = ResourceId::new();
        for query in [
            TaskCockpitQuery::TerminalFor { resource_id },
            TaskCockpitQuery::TerminalScrollFor {
                resource_id,
                delta_lines: -3,
            },
            TaskCockpitQuery::TerminalResizeFor {
                resource_id,
                cols: 100,
                rows: 30,
            },
            TaskCockpitQuery::TerminalReadinessFor { resource_id },
            TaskCockpitQuery::TaskTerminals,
            TaskCockpitQuery::OpenShellTerminal {
                cwd: None,
                expected_task_revision: 4,
            },
            TaskCockpitQuery::OpenShellTerminal {
                cwd: Some("C:/Code/demo".to_string()),
                expected_task_revision: 9,
            },
        ] {
            assert_eq!(cockpit_surface(&query), TaskCockpitSurface::Terminal);
            let encoded = serde_json::to_value(&query).expect("encode targeted terminal query");
            let decoded: TaskCockpitQuery =
                serde_json::from_value(encoded).expect("decode targeted terminal query");
            assert_eq!(decoded, query);
        }
        let encoded = serde_json::to_value(Query::TaskCockpit(TaskCockpitQuery::TaskTerminals))
            .expect("encode strip query");
        assert_eq!(
            encoded.get("task_cockpit").and_then(|value| value.as_str()),
            Some("task_terminals")
        );
    }

    #[test]
    fn task_terminals_projection_round_trips_and_shells_carry_wire_valid_sentinels() {
        let resource_id = ResourceId::new();
        let strip = TaskCockpitResult::TaskTerminals(TaskTerminalsProjection {
            task_id: TaskId::new(),
            terminals: vec![
                TaskTerminalChip {
                    resource_id: ResourceId::new(),
                    is_provider: true,
                    title: None,
                    label: "terminal".into(),
                    runtime_state: TerminalRuntimeStateWire::Running,
                    live_cwd: None,
                    exit: None,
                    created_at_ms: 0,
                    last_activity_at_ms: 0,
                },
                TaskTerminalChip {
                    resource_id,
                    is_provider: false,
                    title: Some("build".into()),
                    label: "pwsh".into(),
                    runtime_state: TerminalRuntimeStateWire::Exited {
                        summary: "exit 0".into(),
                    },
                    live_cwd: Some("C:/Code".into()),
                    exit: Some(crate::domain::terminal_facts::TerminalExit {
                        code: Some(0),
                        summary: "exit 0".into(),
                        at_ms: 11,
                    }),
                    created_at_ms: 7,
                    last_activity_at_ms: 9,
                },
            ],
            order: vec![resource_id],
            // No chip selected is a valid strip state, not unknown focus.
            focused: None,
        });
        let round_trip: TaskCockpitResult =
            serde_json::from_value(serde_json::to_value(&strip).expect("encode strip"))
                .expect("decode strip");
        assert_eq!(round_trip, strip);

        // A plain shell fills the required provider identity fields with the
        // zero sentinel; the validating id decoder must still accept them.
        let sentinel = serde_json::to_value(AgentSessionId::nil()).expect("encode sentinel");
        let decoded: AgentSessionId =
            serde_json::from_value(sentinel).expect("shell sentinel must survive the wire");
        assert!(decoded.is_nil());
    }

    #[test]
    fn terminal_projection_defaults_the_new_shell_fields() {
        let legacy = serde_json::json!({
            "task_id": TaskId::new(),
            "terminal_id": crate::domain::id::TerminalId::new(),
            "session_id": TerminalSessionId::new(),
            "agent_session_id": AgentSessionId::new(),
            "resource_id": ResourceId::new(),
            "runtime_generation": 1,
            "resource_generation": 1,
            "action_epoch": 1,
            "focus_epoch": FocusEpoch::initial(),
            "accepted_input_sequence": 0,
            "sequence": 1,
            "title": serde_json::Value::Null,
            "screen": TerminalScreenSnapshot::default(),
        });
        let decoded: TaskTerminalProjection =
            serde_json::from_value(legacy).expect("a pre-shell projection still decodes");
        assert!(!decoded.is_provider);
        assert_eq!(decoded.runtime_state, TerminalRuntimeStateWire::Running);
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
