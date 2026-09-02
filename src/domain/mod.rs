pub mod agent;
pub mod agent_resource;
pub mod artifact;
pub mod browser;
pub(crate) mod canonical;
pub mod cockpit;
pub mod codec;
pub mod command;
pub mod event;
pub mod host;
pub mod id;
pub mod operation;
pub mod org;
pub mod provider_input;
pub mod query;
pub mod resource;
pub mod snapshot;
pub mod task;
pub mod terminal_facts;

pub use crate::providers::ProviderKind;
pub use agent::{
    AgentRole, AgentSessionFacts, AgentSessionLifecycle, AgentValidationError, ProviderSessionId,
    ProviderSessionIdError, SpecialistPermission, MAX_PROVIDER_SESSION_ID_BYTES,
};
pub use agent_resource::{AgentResourceBinding, AgentResourceBindingError};
pub use artifact::{
    ArtifactContentRef, ArtifactFacts, ArtifactKind, ArtifactSummary, ArtifactValidationError,
    PrivacyClass, SpecialistResult, SpecialistStatus, MAX_SPECIALIST_ID_REFS,
    MAX_SPECIALIST_RAW_ARTIFACT_BYTES, MAX_SPECIALIST_TEXT_BYTES,
};
pub use cockpit::{
    cockpit_surface, git_projection, redact_repository_label, relative_path_is_safe,
    task_agent_resource_projection, validate_folder_config_id, workspace_projection,
    AgentConnectionRow, AgentConnectionSnapshot, AgentPresence, BrowserProcessSessionProjection,
    ConfigCommandDetailProjection, ConfigSidebarFolder, ConfigSidebarProject,
    ConfigSidebarProvider, ConfigSidebarProviderKind, ConfigSidebarServer, ConfigSidebarSnapshot,
    ConfigSidebarSsh, TaskAgentResourceProjection, TaskCockpitDeniedReason, TaskCockpitQuery,
    TaskCockpitResult, TaskCockpitSurface, TaskCockpitUnavailableReason, TaskFileEntry,
    TaskFilesListProjection, TaskFilesReadProjection, TaskGitMutateIntent, TaskGitProjection,
    TaskGitRepositoriesProjection, TaskRepositoryCatalogEntry, TaskRepositoryKind,
    TaskRepositorySelector, TaskRepositorySelectorError, TaskServiceProjection,
    TaskServiceRuntimeState, TaskServiceScope, TaskServiceSnapshot, TaskSshEndpoint,
    TaskSshProjection, TaskTerminalProjection, TaskWorkspaceKind, TaskWorkspaceProjection,
    MAX_COCKPIT_FILE_LIST, MAX_COCKPIT_READ_BYTES, MAX_FOLDER_CONFIG_ID_BYTES,
    MAX_REPOSITORY_LABEL_BYTES, MAX_TASK_REPOSITORIES,
};
pub use codec::{
    decode_orchestration_msgpack, encode_orchestration_msgpack, preflight_msgpack,
    MsgPackPreflightError, OrchestrationCodecError, MAX_ORCHESTRATION_MSGPACK_BYTES,
    MAX_ORCHESTRATION_MSGPACK_COLLECTION_ITEMS, MAX_ORCHESTRATION_MSGPACK_DEPTH,
    MAX_ORCHESTRATION_MSGPACK_NODES, MAX_ORCHESTRATION_MSGPACK_STRING_BYTES,
};
pub use command::{
    command_payload_digest, decide, AcceptSpecialistHandoffIntent, ArmUpdateInstallIntent,
    CancelSpecialistIntent, Command, CommandEnvelope, CommandReceipt, ConfirmHostQuitIntent,
    ConfirmUpdateDrainIntent, CreateTaskIntent, CreateTaskRequestIntent, OpenShellTerminalIntent,
    PrepareUpdateIntent, PromotePrimaryIntent, RejectionCode, RenameTaskIntent,
    RequestSpecialistIntent, ServiceControlAction, ServiceControlIntent, SetTaskAttentionIntent,
    SubmitProviderInputIntent, DEFAULT_MAX_TOP_LEVEL_RUNTIMES,
};
pub use event::{
    apply, ApplyError, DomainEvent, Event, EventSerdeError, OperationAcceptedFact,
    OperationCancelledFact, OperationFailedFact, OperationSettledFact, OperationUncertainFact,
    EVENT_SCHEMA_VERSION,
};
pub use host::{
    HostCleanupBranch, HostCleanupBranchOutcome, HostQuitAgentBlocker, HostQuitInspection,
    HostQuitResourceBlocker, HostQuitWorktreeInspection,
};
pub use id::{
    AgentSessionId, ApprovalId, ArtifactId, BrowserContextId, BrowserRequestId, BrowserSessionId,
    BrowserTabId, ClientId, CommandId, ConfiguredServiceId, ConfiguredServiceIdError,
    EnvironmentId, EventId, IdError, OperationId, OutboxId, ProjectId, PromptChainId,
    PromptChainLinkId, PromptHistoryId, PromptId, PromptVersionId, QuestionId, RequestId,
    ResourceId, ServiceId, SnapshotId, SubscriptionId, TaskId, TaskInviteId, TerminalId,
    TransferId, TurnId, MAX_CONFIGURED_SERVICE_ID_BYTES,
};
pub use operation::{
    validate_outcome_fence, validate_source_for_kind, validate_terminal_fact_source,
    CancellationReason, OperationErrorCode, OperationFacts, OperationOutcome, OperationOutcomeKind,
    OperationState, OperationUncertaintyCode, OutcomeFenceError, OutcomeSource, ResourceFence,
    MAX_EXTERNAL_IDENTITY_BYTES,
};
pub use org::{ManagedScope, TaskScope};
pub use provider_input::{
    provider_kind_from_wire, validate_provider_fence, PresentProviderApprovalIntent,
    PresentProviderQuestionIntent, ProviderDeliveryHoldReason, ProviderDeliveryVisibility,
    ProviderFenceContext, ProviderFenceError, ProviderFenceIdentity, ProviderImageAttachment,
    ProviderInputAction, ProviderInputIntentError, ProviderInputSettlement, ProviderIntentPhase,
    ProviderResolutionWinner, ProviderSessionProjection, ProviderWaitFence, ProviderWaitRecord,
    SettleProviderWaitIntent, MAX_PROVIDER_APPROVAL_WINS, MAX_PROVIDER_IMAGE_ATTACHMENTS,
    MAX_PROVIDER_IMAGE_BYTES, MAX_PROVIDER_IMAGE_PATH_BYTES, MAX_PROVIDER_INPUT_TEXT_BYTES,
    MAX_PROVIDER_KIND_BYTES, MAX_PROVIDER_QUESTION_WINS, MAX_PROVIDER_SESSION_STATE_BYTES,
    MAX_PROVIDER_WAITS,
};
pub use query::{Query, QueryEnvelope, QueryError, QueryOutcome, QueryReply, QueryResult};
pub use resource::{
    OwnerKind, ResourceFacts, ResourceKind, ResourceLifecycle, ResourceRecipe,
    ResourceValidationError,
};
pub use snapshot::{
    canonical_artifact_content_page_size, canonical_event_page_size, canonical_snapshot_page_size,
    provider_plan_step_lifecycle, ArtifactContentPage, CanonicalPageSizeError, EventPage,
    PageLimits, PageLimitsError, PlanStepKind, PlanStepStatus, ProviderPlanStepLifecycle,
    SemanticJournalFact, SemanticJournalPage, SemanticJournalPayload, SnapshotItem,
    SnapshotItemKey, SnapshotPage, SnapshotSection, TaskSnapshot, TaskSnapshotItem,
    MAX_SNAPSHOT_PAGE_ENCODED_BYTES, MAX_SNAPSHOT_PAGE_ITEMS,
};
pub use task::{
    RepositoryFingerprint, ReviewReadiness, TaskActivity, TaskAssignment, TaskAttention,
    TaskConnectivity, TaskFacts, TaskLifecycle, TaskValidationError, VisibleTaskStatus,
    WorkspaceChoice, WorkspaceRef,
};
pub use terminal_facts::{
    HostTerminalFact, TaskTerminalStrip, TerminalExit, TerminalFacts, TerminalStripError,
    MAX_PLAIN_SHELLS_PER_TASK, TERMINAL_ACTIVITY_COALESCE_MS, TERMINAL_CWD_DEBOUNCE_MS,
};
