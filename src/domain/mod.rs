pub mod agent;
pub mod artifact;
pub(crate) mod canonical;
pub mod command;
pub mod event;
pub mod id;
pub mod operation;
pub mod query;
pub mod resource;
pub mod snapshot;
pub mod task;

pub use agent::{AgentRole, AgentSessionFacts, AgentSessionLifecycle, AgentValidationError};
pub use artifact::{
    ArtifactContentRef, ArtifactFacts, ArtifactKind, ArtifactValidationError, PrivacyClass,
};
pub use command::{
    decide, Command, CommandEnvelope, CommandReceipt, CreateTaskIntent, RejectionCode,
    RenameTaskIntent, SetTaskAttentionIntent,
};
pub use event::{
    apply, ApplyError, DomainEvent, Event, EventSerdeError, OperationAcceptedFact,
    OperationCancelledFact, OperationFailedFact, OperationSettledFact, OperationUncertainFact,
    EVENT_SCHEMA_VERSION,
};
pub use id::{
    AgentSessionId, ArtifactId, BrowserContextId, ClientId, CommandId, EnvironmentId, EventId,
    IdError, OperationId, ProjectId, RequestId, ResourceId, ServiceId, SubscriptionId, TaskId,
    TerminalId, TransferId,
};
pub use operation::{
    validate_outcome_fence, CancellationReason, OperationErrorCode, OperationFacts, OperationState,
    OperationUncertaintyCode, OutcomeFenceError,
};
pub use query::{Query, QueryEnvelope, QueryError, QueryOutcome, QueryReply, QueryResult};
pub use resource::{
    OwnerKind, ResourceFacts, ResourceKind, ResourceLifecycle, ResourceRecipe,
    ResourceValidationError,
};
pub use snapshot::TaskSnapshot;
pub use task::{
    ReviewReadiness, TaskActivity, TaskAssignment, TaskAttention, TaskConnectivity, TaskFacts,
    TaskLifecycle, TaskValidationError, VisibleTaskStatus, WorkspaceRef,
};
