pub mod agent;
pub mod artifact;
pub(crate) mod canonical;
pub mod command;
pub mod event;
pub mod host;
pub mod id;
pub mod operation;
pub mod org;
pub mod query;
pub mod resource;
pub mod snapshot;
pub mod task;

pub use agent::{AgentRole, AgentSessionFacts, AgentSessionLifecycle, AgentValidationError};
pub use artifact::{
    ArtifactContentRef, ArtifactFacts, ArtifactKind, ArtifactSummary, ArtifactValidationError,
    PrivacyClass,
};
pub use command::{
    decide, Command, CommandEnvelope, CommandReceipt, ConfirmHostQuitIntent, CreateTaskIntent,
    CreateTaskRequestIntent, RejectionCode, RenameTaskIntent, SetTaskAttentionIntent,
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
    AgentSessionId, ArtifactId, BrowserContextId, ClientId, CommandId, EnvironmentId, EventId,
    IdError, OperationId, OutboxId, ProjectId, RequestId, ResourceId, ServiceId, SnapshotId,
    SubscriptionId, TaskId, TaskInviteId, TerminalId, TransferId,
};
pub use operation::{
    validate_outcome_fence, validate_source_for_kind, validate_terminal_fact_source,
    CancellationReason, OperationErrorCode, OperationFacts, OperationOutcome, OperationOutcomeKind,
    OperationState, OperationUncertaintyCode, OutcomeFenceError, OutcomeSource, ResourceFence,
    MAX_EXTERNAL_IDENTITY_BYTES,
};
pub use org::{ManagedScope, TaskScope};
pub use query::{Query, QueryEnvelope, QueryError, QueryOutcome, QueryReply, QueryResult};
pub use resource::{
    OwnerKind, ResourceFacts, ResourceKind, ResourceLifecycle, ResourceRecipe,
    ResourceValidationError,
};
pub use snapshot::{
    canonical_artifact_content_page_size, canonical_event_page_size, canonical_snapshot_page_size,
    ArtifactContentPage, CanonicalPageSizeError, EventPage, PageLimits, PageLimitsError,
    SnapshotItem, SnapshotItemKey, SnapshotPage, SnapshotSection, TaskSnapshot, TaskSnapshotItem,
    MAX_SNAPSHOT_PAGE_ENCODED_BYTES, MAX_SNAPSHOT_PAGE_ITEMS,
};
pub use task::{
    RepositoryFingerprint, ReviewReadiness, TaskActivity, TaskAssignment, TaskAttention,
    TaskConnectivity, TaskFacts, TaskLifecycle, TaskValidationError, VisibleTaskStatus,
    WorkspaceChoice, WorkspaceRef,
};
