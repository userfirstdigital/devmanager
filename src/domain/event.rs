use std::collections::BTreeMap;
use std::fmt;

use serde::de::{self, Deserializer};
use serde::ser::{self, Serializer};
use serde::{Deserialize, Serialize};

use crate::domain::agent::{
    AgentRole, AgentSessionFacts, AgentSessionLifecycle, ProviderSessionId, SpecialistPermission,
};
use crate::domain::agent_resource::AgentResourceBinding;
use crate::domain::artifact::{
    structured_specialist_result, verify_inline_content_digest, ArtifactContentRef, ArtifactFacts,
    ArtifactKind, MAX_SPECIALIST_RAW_ARTIFACT_BYTES,
};
use crate::domain::browser::{BrowserBook, BrowserContractError, BrowserDurableFact};
use crate::domain::canonical;
use crate::domain::host::{HostCleanupBranch, HostCleanupBranchOutcome};
use crate::domain::id::{
    AgentSessionId, ApprovalId, ClientId, CommandId, EventId, OperationId, QuestionId, ResourceId,
    TaskId, TurnId,
};
use crate::domain::operation::{
    validate_outcome_fence, validate_terminal_fact_source, CancellationReason, OperationErrorCode,
    OperationUncertaintyCode, OutcomeFenceError, OutcomeSource,
};
use crate::domain::provider_input::{
    validate_provider_fence, ProviderDeliveryVisibility, ProviderFenceIdentity,
    ProviderInputAction, ProviderInputSettlement, ProviderResolutionWinner, ProviderWaitFence,
    ProviderWaitRecord,
};
use crate::domain::resource::{ResourceFacts, ResourceLifecycle};
use crate::domain::snapshot::TaskSnapshot;
use crate::domain::task::{
    ReviewReadiness, TaskActivity, TaskAttention, TaskConnectivity, TaskFacts, TaskLifecycle,
    WorkspaceRef,
};
use crate::providers::ProviderKind;

pub const EVENT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DomainEvent {
    pub id: EventId,
    pub task_id: Option<TaskId>,
    pub sequence: u64,
    pub task_revision: Option<u64>,
    pub occurred_at_ms: i64,
    pub payload: Event,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationAcceptedFact {
    pub command_id: CommandId,
    pub operation_id: OperationId,
    pub accepted_at_ms: i64,
    pub action_epoch: Option<u64>,
    pub resource_id: Option<ResourceId>,
    pub runtime_generation: Option<u64>,
}

impl OperationAcceptedFact {
    pub fn new(
        command_id: CommandId,
        operation_id: OperationId,
        accepted_at_ms: i64,
        action_epoch: Option<u64>,
        resource_id: Option<ResourceId>,
        runtime_generation: Option<u64>,
    ) -> Result<Self, OutcomeFenceError> {
        validate_outcome_fence(resource_id, runtime_generation)?;
        Ok(Self {
            command_id,
            operation_id,
            accepted_at_ms,
            action_epoch,
            resource_id,
            runtime_generation,
        })
    }

    pub fn validate(&self) -> Result<(), OutcomeFenceError> {
        validate_outcome_fence(self.resource_id, self.runtime_generation)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OperationAcceptedFactWire {
    command_id: CommandId,
    operation_id: OperationId,
    accepted_at_ms: i64,
    action_epoch: Option<u64>,
    resource_id: Option<ResourceId>,
    runtime_generation: Option<u64>,
}

impl Serialize for OperationAcceptedFact {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.validate().map_err(ser::Error::custom)?;
        OperationAcceptedFactWire {
            command_id: self.command_id,
            operation_id: self.operation_id,
            accepted_at_ms: self.accepted_at_ms,
            action_epoch: self.action_epoch,
            resource_id: self.resource_id,
            runtime_generation: self.runtime_generation,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for OperationAcceptedFact {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = OperationAcceptedFactWire::deserialize(deserializer)?;
        Self::new(
            wire.command_id,
            wire.operation_id,
            wire.accepted_at_ms,
            wire.action_epoch,
            wire.resource_id,
            wire.runtime_generation,
        )
        .map_err(de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationSettledFact {
    pub command_id: CommandId,
    pub operation_id: OperationId,
    pub settled_at_ms: i64,
    pub result_event_ids: Vec<EventId>,
    pub action_epoch: Option<u64>,
    pub resource_id: Option<ResourceId>,
    pub runtime_generation: Option<u64>,
    pub source: OutcomeSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationFailedFact {
    pub command_id: CommandId,
    pub operation_id: OperationId,
    pub settled_at_ms: i64,
    pub code: OperationErrorCode,
    pub action_epoch: Option<u64>,
    pub resource_id: Option<ResourceId>,
    pub runtime_generation: Option<u64>,
    pub source: OutcomeSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationCancelledFact {
    pub command_id: CommandId,
    pub operation_id: OperationId,
    pub settled_at_ms: i64,
    pub reason: CancellationReason,
    pub action_epoch: Option<u64>,
    pub resource_id: Option<ResourceId>,
    pub runtime_generation: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationUncertainFact {
    pub command_id: CommandId,
    pub operation_id: OperationId,
    pub observed_at_ms: i64,
    pub code: OperationUncertaintyCode,
    pub action_epoch: Option<u64>,
    pub resource_id: Option<ResourceId>,
    pub runtime_generation: Option<u64>,
}

// Manual outcome wire types with fence validation on deserialize.

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OperationSettledFactWire {
    command_id: CommandId,
    operation_id: OperationId,
    settled_at_ms: i64,
    result_event_ids: Vec<EventId>,
    action_epoch: Option<u64>,
    resource_id: Option<ResourceId>,
    runtime_generation: Option<u64>,
    source: OutcomeSource,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OperationFailedFactWire {
    command_id: CommandId,
    operation_id: OperationId,
    settled_at_ms: i64,
    code: OperationErrorCode,
    action_epoch: Option<u64>,
    resource_id: Option<ResourceId>,
    runtime_generation: Option<u64>,
    source: OutcomeSource,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OperationCancelledFactWire {
    command_id: CommandId,
    operation_id: OperationId,
    settled_at_ms: i64,
    reason: CancellationReason,
    action_epoch: Option<u64>,
    resource_id: Option<ResourceId>,
    runtime_generation: Option<u64>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OperationUncertainFactWire {
    command_id: CommandId,
    operation_id: OperationId,
    observed_at_ms: i64,
    code: OperationUncertaintyCode,
    action_epoch: Option<u64>,
    resource_id: Option<ResourceId>,
    runtime_generation: Option<u64>,
}

impl OperationSettledFact {
    /// Dispatch convenience: preserves the historical `new` call shape.
    pub fn new(
        command_id: CommandId,
        operation_id: OperationId,
        settled_at_ms: i64,
        result_event_ids: Vec<EventId>,
        action_epoch: Option<u64>,
        resource_id: Option<ResourceId>,
        runtime_generation: Option<u64>,
    ) -> Result<Self, OutcomeFenceError> {
        Self::with_source(
            command_id,
            operation_id,
            settled_at_ms,
            result_event_ids,
            action_epoch,
            resource_id,
            runtime_generation,
            OutcomeSource::Dispatch,
        )
    }

    pub fn with_source(
        command_id: CommandId,
        operation_id: OperationId,
        settled_at_ms: i64,
        result_event_ids: Vec<EventId>,
        action_epoch: Option<u64>,
        resource_id: Option<ResourceId>,
        runtime_generation: Option<u64>,
        source: OutcomeSource,
    ) -> Result<Self, OutcomeFenceError> {
        validate_outcome_fence(resource_id, runtime_generation)?;
        validate_terminal_fact_source(&source)?;
        Ok(Self {
            command_id,
            operation_id,
            settled_at_ms,
            result_event_ids,
            action_epoch,
            resource_id,
            runtime_generation,
            source,
        })
    }

    pub fn validate(&self) -> Result<(), OutcomeFenceError> {
        validate_outcome_fence(self.resource_id, self.runtime_generation)?;
        validate_terminal_fact_source(&self.source)
    }
}

impl OperationFailedFact {
    /// Dispatch convenience: preserves the historical `new` call shape.
    pub fn new(
        command_id: CommandId,
        operation_id: OperationId,
        settled_at_ms: i64,
        code: OperationErrorCode,
        action_epoch: Option<u64>,
        resource_id: Option<ResourceId>,
        runtime_generation: Option<u64>,
    ) -> Result<Self, OutcomeFenceError> {
        Self::with_source(
            command_id,
            operation_id,
            settled_at_ms,
            code,
            action_epoch,
            resource_id,
            runtime_generation,
            OutcomeSource::Dispatch,
        )
    }

    pub fn with_source(
        command_id: CommandId,
        operation_id: OperationId,
        settled_at_ms: i64,
        code: OperationErrorCode,
        action_epoch: Option<u64>,
        resource_id: Option<ResourceId>,
        runtime_generation: Option<u64>,
        source: OutcomeSource,
    ) -> Result<Self, OutcomeFenceError> {
        validate_outcome_fence(resource_id, runtime_generation)?;
        validate_terminal_fact_source(&source)?;
        Ok(Self {
            command_id,
            operation_id,
            settled_at_ms,
            code,
            action_epoch,
            resource_id,
            runtime_generation,
            source,
        })
    }

    pub fn validate(&self) -> Result<(), OutcomeFenceError> {
        validate_outcome_fence(self.resource_id, self.runtime_generation)?;
        validate_terminal_fact_source(&self.source)
    }
}

impl OperationCancelledFact {
    pub fn new(
        command_id: CommandId,
        operation_id: OperationId,
        settled_at_ms: i64,
        reason: CancellationReason,
        action_epoch: Option<u64>,
        resource_id: Option<ResourceId>,
        runtime_generation: Option<u64>,
    ) -> Result<Self, OutcomeFenceError> {
        validate_outcome_fence(resource_id, runtime_generation)?;
        Ok(Self {
            command_id,
            operation_id,
            settled_at_ms,
            reason,
            action_epoch,
            resource_id,
            runtime_generation,
        })
    }

    pub fn validate(&self) -> Result<(), OutcomeFenceError> {
        validate_outcome_fence(self.resource_id, self.runtime_generation)
    }
}

impl OperationUncertainFact {
    pub fn new(
        command_id: CommandId,
        operation_id: OperationId,
        observed_at_ms: i64,
        code: OperationUncertaintyCode,
        action_epoch: Option<u64>,
        resource_id: Option<ResourceId>,
        runtime_generation: Option<u64>,
    ) -> Result<Self, OutcomeFenceError> {
        validate_outcome_fence(resource_id, runtime_generation)?;
        Ok(Self {
            command_id,
            operation_id,
            observed_at_ms,
            code,
            action_epoch,
            resource_id,
            runtime_generation,
        })
    }

    pub fn validate(&self) -> Result<(), OutcomeFenceError> {
        validate_outcome_fence(self.resource_id, self.runtime_generation)
    }
}

impl Serialize for OperationSettledFact {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.validate().map_err(ser::Error::custom)?;
        OperationSettledFactWire {
            command_id: self.command_id,
            operation_id: self.operation_id,
            settled_at_ms: self.settled_at_ms,
            result_event_ids: self.result_event_ids.clone(),
            action_epoch: self.action_epoch,
            resource_id: self.resource_id,
            runtime_generation: self.runtime_generation,
            source: self.source.clone(),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for OperationSettledFact {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = OperationSettledFactWire::deserialize(deserializer)?;
        Self::with_source(
            wire.command_id,
            wire.operation_id,
            wire.settled_at_ms,
            wire.result_event_ids,
            wire.action_epoch,
            wire.resource_id,
            wire.runtime_generation,
            wire.source,
        )
        .map_err(de::Error::custom)
    }
}

impl Serialize for OperationFailedFact {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.validate().map_err(ser::Error::custom)?;
        OperationFailedFactWire {
            command_id: self.command_id,
            operation_id: self.operation_id,
            settled_at_ms: self.settled_at_ms,
            code: self.code,
            action_epoch: self.action_epoch,
            resource_id: self.resource_id,
            runtime_generation: self.runtime_generation,
            source: self.source.clone(),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for OperationFailedFact {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = OperationFailedFactWire::deserialize(deserializer)?;
        Self::with_source(
            wire.command_id,
            wire.operation_id,
            wire.settled_at_ms,
            wire.code,
            wire.action_epoch,
            wire.resource_id,
            wire.runtime_generation,
            wire.source,
        )
        .map_err(de::Error::custom)
    }
}

impl Serialize for OperationCancelledFact {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.validate().map_err(ser::Error::custom)?;
        OperationCancelledFactWire {
            command_id: self.command_id,
            operation_id: self.operation_id,
            settled_at_ms: self.settled_at_ms,
            reason: self.reason,
            action_epoch: self.action_epoch,
            resource_id: self.resource_id,
            runtime_generation: self.runtime_generation,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for OperationCancelledFact {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = OperationCancelledFactWire::deserialize(deserializer)?;
        Self::new(
            wire.command_id,
            wire.operation_id,
            wire.settled_at_ms,
            wire.reason,
            wire.action_epoch,
            wire.resource_id,
            wire.runtime_generation,
        )
        .map_err(de::Error::custom)
    }
}

impl Serialize for OperationUncertainFact {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.validate().map_err(ser::Error::custom)?;
        OperationUncertainFactWire {
            command_id: self.command_id,
            operation_id: self.operation_id,
            observed_at_ms: self.observed_at_ms,
            code: self.code,
            action_epoch: self.action_epoch,
            resource_id: self.resource_id,
            runtime_generation: self.runtime_generation,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for OperationUncertainFact {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = OperationUncertainFactWire::deserialize(deserializer)?;
        Self::new(
            wire.command_id,
            wire.operation_id,
            wire.observed_at_ms,
            wire.code,
            wire.action_epoch,
            wire.resource_id,
            wire.runtime_generation,
        )
        .map_err(de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskCreatedPayload {
    pub task: TaskFacts,
    pub connectivity: TaskConnectivity,
    pub attention: TaskAttention,
    pub activity: TaskActivity,
    pub review_readiness: ReviewReadiness,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskRenamedPayload {
    pub title: String,
}

impl TaskRenamedPayload {
    fn validated(title: String) -> Result<Self, crate::domain::task::TaskValidationError> {
        Ok(Self {
            title: TaskFacts::canonicalize_title(title)?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskAttentionSetPayload {
    pub attention: TaskAttention,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskCloseBegunPayload {
    pub action_epoch: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskUnitPayload {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSessionRegisteredPayload {
    pub agent: AgentSessionFacts,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentProviderSessionBoundPayload {
    pub agent_session_id: AgentSessionId,
    pub resource_id: ResourceId,
    pub provider_session_id: ProviderSessionId,
    pub runtime_generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrimaryAgentSetPayload {
    pub agent_session_id: AgentSessionId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnstartedPrimaryProviderReboundPayload {
    pub agent_session_id: AgentSessionId,
    pub provider_kind: ProviderKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactRegisteredPayload {
    pub artifact: ArtifactFacts,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceRegisteredPayload {
    pub resource: ResourceFacts,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceReleaseBegunPayload {
    pub resource_id: ResourceId,
    pub runtime_generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceReleasedPayload {
    pub resource_id: ResourceId,
    pub runtime_generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostCloseBegunPayload {
    pub operation_id: OperationId,
    pub action_epoch: u64,
    pub inspection_id: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostCleanupBranchCompletedPayload {
    pub operation_id: OperationId,
    pub action_epoch: u64,
    pub branch: HostCleanupBranch,
    pub outcome: HostCleanupBranchOutcome,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderInputAcceptedPayload {
    pub command_id: CommandId,
    pub client_id: ClientId,
    pub operation_id: OperationId,
    pub agent_session_id: AgentSessionId,
    pub provider_kind: ProviderKind,
    pub provider_session_id: Option<ProviderSessionId>,
    pub runtime_generation: u64,
    pub turn_id: TurnId,
    pub action_epoch: u64,
    pub question_id: Option<QuestionId>,
    pub approval_id: Option<ApprovalId>,
    pub action: ProviderInputAction,
    pub wait: bool,
    pub delivery: ProviderDeliveryVisibility,
}

impl fmt::Debug for ProviderInputAcceptedPayload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderInputAcceptedPayload")
            .field("command_id", &self.command_id)
            .field("client_id", &self.client_id)
            .field("operation_id", &self.operation_id)
            .field("agent_session_id", &self.agent_session_id)
            .field("provider_kind", &self.provider_kind)
            .field("provider_session_id", &self.provider_session_id)
            .field("runtime_generation", &self.runtime_generation)
            .field("turn_id", &self.turn_id)
            .field("action_epoch", &self.action_epoch)
            .field("question_id", &self.question_id)
            .field("approval_id", &self.approval_id)
            .field("action", &self.action)
            .field("wait", &self.wait)
            .field("delivery", &self.delivery)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderQuestionPresentedPayload {
    pub agent_session_id: AgentSessionId,
    pub provider_kind: ProviderKind,
    pub provider_session_id: ProviderSessionId,
    pub runtime_generation: u64,
    pub turn_id: TurnId,
    pub action_epoch: u64,
    pub question_id: QuestionId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderApprovalPresentedPayload {
    pub agent_session_id: AgentSessionId,
    pub provider_kind: ProviderKind,
    pub provider_session_id: ProviderSessionId,
    pub runtime_generation: u64,
    pub turn_id: TurnId,
    pub action_epoch: u64,
    pub approval_id: ApprovalId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderWaitSettledPayload {
    pub fence: ProviderWaitFence,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderInputDeliveredPayload {
    pub command_id: CommandId,
    pub client_id: ClientId,
    pub operation_id: OperationId,
    pub agent_session_id: AgentSessionId,
    pub provider_kind: ProviderKind,
    pub provider_session_id: Option<ProviderSessionId>,
    pub runtime_generation: u64,
    pub turn_id: TurnId,
    pub action_epoch: u64,
    pub question_id: Option<QuestionId>,
    pub approval_id: Option<ApprovalId>,
}

#[derive(Clone, PartialEq, Eq)]
pub struct SpecialistRequestedPayload {
    pub specialist_id: AgentSessionId,
    pub requested_by: AgentSessionId,
    pub purpose: String,
    pub agent: AgentSessionFacts,
    pub permission: SpecialistPermission,
    pub workspace: WorkspaceRef,
    pub action_epoch: u64,
    pub runtime_generation: u64,
    pub resource_id: Option<ResourceId>,
}

impl fmt::Debug for SpecialistRequestedPayload {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SpecialistRequestedPayload")
            .field("specialist_id", &self.specialist_id)
            .field("requested_by", &self.requested_by)
            .field(
                "purpose",
                &format_args!("<redacted {} bytes>", self.purpose.len()),
            )
            .field("agent", &self.agent)
            .field("permission", &self.permission)
            .field("workspace", &"<redacted>")
            .field("action_epoch", &self.action_epoch)
            .field("runtime_generation", &self.runtime_generation)
            .field("resource_id", &self.resource_id)
            .finish()
    }
}

impl SpecialistRequestedPayload {
    pub fn validate(&self) -> Result<(), &'static str> {
        if !canonical::is_bounded_canonical(&self.purpose) {
            return Err("specialist purpose exceeds bound or is not canonical");
        }
        if self.agent.id != self.specialist_id
            || self.requested_by == self.specialist_id
            || !matches!(self.agent.role, AgentRole::Specialist { .. })
            || self.agent.runtime_generation != self.runtime_generation
            || !matches!(self.permission, SpecialistPermission::ReadOnly)
        {
            return Err("specialist request lineage is invalid");
        }
        self.agent
            .validate_for_registration()
            .map_err(|_| "specialist agent registration facts are invalid")?;
        self.workspace
            .validate()
            .map_err(|_| "specialist workspace is invalid")?;
        Ok(())
    }
}

impl Serialize for SpecialistRequestedPayload {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.validate().map_err(ser::Error::custom)?;
        #[derive(Serialize)]
        struct SpecialistRequestedPayloadWire<'a> {
            specialist_id: AgentSessionId,
            requested_by: AgentSessionId,
            purpose: &'a str,
            agent: &'a AgentSessionFacts,
            permission: SpecialistPermission,
            workspace: &'a WorkspaceRef,
            action_epoch: u64,
            runtime_generation: u64,
            resource_id: Option<ResourceId>,
        }
        SpecialistRequestedPayloadWire {
            specialist_id: self.specialist_id,
            requested_by: self.requested_by,
            purpose: &self.purpose,
            agent: &self.agent,
            permission: self.permission,
            workspace: &self.workspace,
            action_epoch: self.action_epoch,
            runtime_generation: self.runtime_generation,
            resource_id: self.resource_id,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for SpecialistRequestedPayload {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct SpecialistRequestedPayloadWire {
            specialist_id: AgentSessionId,
            requested_by: AgentSessionId,
            purpose: String,
            agent: AgentSessionFacts,
            permission: SpecialistPermission,
            workspace: WorkspaceRef,
            action_epoch: u64,
            runtime_generation: u64,
            resource_id: Option<ResourceId>,
        }

        let wire = SpecialistRequestedPayloadWire::deserialize(deserializer)?;
        let payload = Self {
            specialist_id: wire.specialist_id,
            requested_by: wire.requested_by,
            purpose: wire.purpose,
            agent: wire.agent,
            permission: wire.permission,
            workspace: wire.workspace,
            action_epoch: wire.action_epoch,
            runtime_generation: wire.runtime_generation,
            resource_id: wire.resource_id,
        };
        payload.validate().map_err(de::Error::custom)?;
        Ok(payload)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrimaryPromotedPayload {
    pub previous: AgentSessionId,
    pub promoted: AgentSessionId,
    pub action_epoch: u64,
    pub runtime_generation: u64,
}

impl PrimaryPromotedPayload {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.previous == self.promoted {
            return Err("primary promotion requires distinct previous and promoted agents");
        }
        Ok(())
    }
}

impl Serialize for PrimaryPromotedPayload {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.validate().map_err(ser::Error::custom)?;
        #[derive(Serialize)]
        #[serde(deny_unknown_fields)]
        struct PrimaryPromotedPayloadWire {
            previous: AgentSessionId,
            promoted: AgentSessionId,
            action_epoch: u64,
            runtime_generation: u64,
        }
        PrimaryPromotedPayloadWire {
            previous: self.previous,
            promoted: self.promoted,
            action_epoch: self.action_epoch,
            runtime_generation: self.runtime_generation,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for PrimaryPromotedPayload {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct PrimaryPromotedPayloadWire {
            previous: AgentSessionId,
            promoted: AgentSessionId,
            action_epoch: u64,
            runtime_generation: u64,
        }

        let wire = PrimaryPromotedPayloadWire::deserialize(deserializer)?;
        let payload = Self {
            previous: wire.previous,
            promoted: wire.promoted,
            action_epoch: wire.action_epoch,
            runtime_generation: wire.runtime_generation,
        };
        payload.validate().map_err(de::Error::custom)?;
        Ok(payload)
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct SpecialistHandoffRecordedPayload {
    pub specialist_id: AgentSessionId,
    pub artifact: ArtifactFacts,
    pub structured: bool,
    pub action_epoch: u64,
    pub runtime_generation: u64,
}

impl fmt::Debug for SpecialistHandoffRecordedPayload {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SpecialistHandoffRecordedPayload")
            .field("specialist_id", &self.specialist_id)
            .field("artifact", &self.artifact)
            .field("structured", &self.structured)
            .field("action_epoch", &self.action_epoch)
            .field("runtime_generation", &self.runtime_generation)
            .finish()
    }
}

impl SpecialistHandoffRecordedPayload {
    pub fn validate(&self) -> Result<(), &'static str> {
        self.artifact
            .validate()
            .map_err(|_| "handoff artifact is invalid")?;
        if self.artifact.kind != ArtifactKind::ReviewReport
            || verify_inline_content_digest(&self.artifact).is_err()
        {
            return Err("handoff artifact digest or kind is invalid");
        }
        let ArtifactContentRef::InlineUtf8(body) = &self.artifact.content_ref else {
            return Err("handoff artifact must retain bounded inline output");
        };
        if body.len() > MAX_SPECIALIST_RAW_ARTIFACT_BYTES {
            return Err("handoff artifact exceeds raw output bound");
        }
        if self.structured {
            structured_specialist_result(&self.artifact)
                .map_err(|_| "structured handoff body failed specialist result validation")?;
        }
        Ok(())
    }
}

impl Serialize for SpecialistHandoffRecordedPayload {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.validate().map_err(ser::Error::custom)?;
        #[derive(Serialize)]
        struct SpecialistHandoffRecordedPayloadWire<'a> {
            specialist_id: AgentSessionId,
            artifact: &'a ArtifactFacts,
            structured: bool,
            action_epoch: u64,
            runtime_generation: u64,
        }
        SpecialistHandoffRecordedPayloadWire {
            specialist_id: self.specialist_id,
            artifact: &self.artifact,
            structured: self.structured,
            action_epoch: self.action_epoch,
            runtime_generation: self.runtime_generation,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for SpecialistHandoffRecordedPayload {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct SpecialistHandoffRecordedPayloadWire {
            specialist_id: AgentSessionId,
            artifact: ArtifactFacts,
            structured: bool,
            action_epoch: u64,
            runtime_generation: u64,
        }

        let wire = SpecialistHandoffRecordedPayloadWire::deserialize(deserializer)?;
        let payload = Self {
            specialist_id: wire.specialist_id,
            artifact: wire.artifact,
            structured: wire.structured,
            action_epoch: wire.action_epoch,
            runtime_generation: wire.runtime_generation,
        };
        payload.validate().map_err(de::Error::custom)?;
        Ok(payload)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpecialistClosedPayload {
    pub specialist_id: AgentSessionId,
    pub action_epoch: u64,
    pub runtime_generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    TaskCreated {
        task: TaskFacts,
        connectivity: TaskConnectivity,
        attention: TaskAttention,
        activity: TaskActivity,
        review_readiness: ReviewReadiness,
    },
    TaskRenamed {
        title: String,
    },
    TaskAttentionSet {
        attention: TaskAttention,
    },
    TaskCloseBegun {
        action_epoch: u64,
    },
    TaskSettled,
    TaskReopened,
    TaskArchived,
    TaskDeleted,
    AgentSessionRegistered {
        agent: AgentSessionFacts,
    },
    AgentProviderSessionBound {
        agent_session_id: AgentSessionId,
        resource_id: ResourceId,
        provider_session_id: ProviderSessionId,
        runtime_generation: u64,
    },
    PrimaryAgentSet {
        agent_session_id: AgentSessionId,
    },
    UnstartedPrimaryProviderRebound {
        agent_session_id: AgentSessionId,
        provider_kind: ProviderKind,
    },
    SpecialistRequested {
        specialist_id: AgentSessionId,
        requested_by: AgentSessionId,
        purpose: String,
        agent: AgentSessionFacts,
        permission: SpecialistPermission,
        workspace: WorkspaceRef,
        action_epoch: u64,
        runtime_generation: u64,
        resource_id: Option<ResourceId>,
    },
    PrimaryPromoted {
        previous: AgentSessionId,
        promoted: AgentSessionId,
        action_epoch: u64,
        runtime_generation: u64,
    },
    SpecialistHandoffRecorded {
        specialist_id: AgentSessionId,
        artifact: ArtifactFacts,
        structured: bool,
        action_epoch: u64,
        runtime_generation: u64,
    },
    SpecialistClosed {
        specialist_id: AgentSessionId,
        action_epoch: u64,
        runtime_generation: u64,
    },
    ArtifactRegistered {
        artifact: ArtifactFacts,
    },
    ResourceRegistered {
        resource: ResourceFacts,
    },
    ResourceReleaseBegun {
        resource_id: ResourceId,
        runtime_generation: u64,
    },
    ResourceReleased {
        resource_id: ResourceId,
        runtime_generation: u64,
    },
    HostCloseBegun {
        operation_id: OperationId,
        action_epoch: u64,
        inspection_id: u64,
    },
    HostCleanupBranchCompleted {
        operation_id: OperationId,
        action_epoch: u64,
        branch: HostCleanupBranch,
        outcome: HostCleanupBranchOutcome,
    },
    OperationAccepted(OperationAcceptedFact),
    OperationSettled(OperationSettledFact),
    OperationFailed(OperationFailedFact),
    OperationCancelled(OperationCancelledFact),
    OperationUncertain(OperationUncertainFact),
    ProviderInputAccepted {
        command_id: CommandId,
        client_id: ClientId,
        operation_id: OperationId,
        agent_session_id: AgentSessionId,
        provider_kind: ProviderKind,
        provider_session_id: Option<ProviderSessionId>,
        runtime_generation: u64,
        turn_id: TurnId,
        action_epoch: u64,
        question_id: Option<QuestionId>,
        approval_id: Option<ApprovalId>,
        action: ProviderInputAction,
        wait: bool,
        delivery: ProviderDeliveryVisibility,
    },
    ProviderQuestionPresented {
        agent_session_id: AgentSessionId,
        provider_kind: ProviderKind,
        provider_session_id: ProviderSessionId,
        runtime_generation: u64,
        turn_id: TurnId,
        action_epoch: u64,
        question_id: QuestionId,
    },
    ProviderApprovalPresented {
        agent_session_id: AgentSessionId,
        provider_kind: ProviderKind,
        provider_session_id: ProviderSessionId,
        runtime_generation: u64,
        turn_id: TurnId,
        action_epoch: u64,
        approval_id: ApprovalId,
    },
    ProviderWaitSettled {
        fence: ProviderWaitFence,
    },
    ProviderInputDelivered {
        command_id: CommandId,
        client_id: ClientId,
        operation_id: OperationId,
        agent_session_id: AgentSessionId,
        provider_kind: ProviderKind,
        provider_session_id: Option<ProviderSessionId>,
        runtime_generation: u64,
        turn_id: TurnId,
        action_epoch: u64,
        question_id: Option<QuestionId>,
        approval_id: Option<ApprovalId>,
    },
    Browser(BrowserDurableFact),
}

impl Event {
    pub fn event_type(&self) -> &'static str {
        match self {
            Self::TaskCreated { .. } => "task.created",
            Self::TaskRenamed { .. } => "task.renamed",
            Self::TaskAttentionSet { .. } => "task.attention_set",
            Self::TaskCloseBegun { .. } => "task.close_begun",
            Self::TaskSettled => "task.settled",
            Self::TaskReopened => "task.reopened",
            Self::TaskArchived => "task.archived",
            Self::TaskDeleted => "task.deleted",
            Self::AgentSessionRegistered { .. } => "agent_session.registered",
            Self::AgentProviderSessionBound { .. } => "agent_session.provider_bound",
            Self::PrimaryAgentSet { .. } => "primary_agent.set",
            Self::UnstartedPrimaryProviderRebound { .. } => {
                "agent_session.unstarted_provider_rebound"
            }
            Self::SpecialistRequested { .. } => "specialist.requested",
            Self::PrimaryPromoted { .. } => "primary_agent.promoted",
            Self::SpecialistHandoffRecorded { .. } => "specialist.handoff_recorded",
            Self::SpecialistClosed { .. } => "specialist.closed",
            Self::ArtifactRegistered { .. } => "artifact.registered",
            Self::ResourceRegistered { .. } => "resource.registered",
            Self::ResourceReleaseBegun { .. } => "resource.release_begun",
            Self::ResourceReleased { .. } => "resource.released",
            Self::HostCloseBegun { .. } => "host.close_begun",
            Self::HostCleanupBranchCompleted { .. } => "host.cleanup_branch_completed",
            Self::OperationAccepted(_) => "operation.accepted",
            Self::OperationSettled(_) => "operation.settled",
            Self::OperationFailed(_) => "operation.failed",
            Self::OperationCancelled(_) => "operation.cancelled",
            Self::OperationUncertain(_) => "operation.uncertain",
            Self::ProviderInputAccepted { .. } => "provider_input.accepted",
            Self::ProviderQuestionPresented { .. } => "provider_input.question_presented",
            Self::ProviderApprovalPresented { .. } => "provider_input.approval_presented",
            Self::ProviderWaitSettled { .. } => "provider_input.wait_settled",
            Self::ProviderInputDelivered { .. } => "provider_input.delivered",
            Self::Browser(_) => "browser.fact",
        }
    }

    pub fn is_task_mutation(&self) -> bool {
        !matches!(
            self,
            Self::HostCloseBegun { .. }
                | Self::HostCleanupBranchCompleted { .. }
                | Self::OperationAccepted(_)
                | Self::OperationSettled(_)
                | Self::OperationFailed(_)
                | Self::OperationCancelled(_)
                | Self::OperationUncertain(_)
                | Self::ProviderInputDelivered { .. }
        )
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "event_type", content = "payload")]
enum EventBody {
    #[serde(rename = "task.created")]
    TaskCreated(TaskCreatedPayload),
    #[serde(rename = "task.renamed")]
    TaskRenamed(TaskRenamedPayload),
    #[serde(rename = "task.attention_set")]
    TaskAttentionSet(TaskAttentionSetPayload),
    #[serde(rename = "task.close_begun")]
    TaskCloseBegun(TaskCloseBegunPayload),
    #[serde(rename = "task.settled")]
    TaskSettled(TaskUnitPayload),
    #[serde(rename = "task.reopened")]
    TaskReopened(TaskUnitPayload),
    #[serde(rename = "task.archived")]
    TaskArchived(TaskUnitPayload),
    #[serde(rename = "task.deleted")]
    TaskDeleted(TaskUnitPayload),
    #[serde(rename = "agent_session.registered")]
    AgentSessionRegistered(AgentSessionRegisteredPayload),
    #[serde(rename = "agent_session.provider_bound")]
    AgentProviderSessionBound(AgentProviderSessionBoundPayload),
    #[serde(rename = "primary_agent.set")]
    PrimaryAgentSet(PrimaryAgentSetPayload),
    #[serde(rename = "agent_session.unstarted_provider_rebound")]
    UnstartedPrimaryProviderRebound(UnstartedPrimaryProviderReboundPayload),
    #[serde(rename = "specialist.requested")]
    SpecialistRequested(SpecialistRequestedPayload),
    #[serde(rename = "primary_agent.promoted")]
    PrimaryPromoted(PrimaryPromotedPayload),
    #[serde(rename = "specialist.handoff_recorded")]
    SpecialistHandoffRecorded(SpecialistHandoffRecordedPayload),
    #[serde(rename = "specialist.closed")]
    SpecialistClosed(SpecialistClosedPayload),
    #[serde(rename = "artifact.registered")]
    ArtifactRegistered(ArtifactRegisteredPayload),
    #[serde(rename = "resource.registered")]
    ResourceRegistered(ResourceRegisteredPayload),
    #[serde(rename = "resource.release_begun")]
    ResourceReleaseBegun(ResourceReleaseBegunPayload),
    #[serde(rename = "resource.released")]
    ResourceReleased(ResourceReleasedPayload),
    #[serde(rename = "host.close_begun")]
    HostCloseBegun(HostCloseBegunPayload),
    #[serde(rename = "host.cleanup_branch_completed")]
    HostCleanupBranchCompleted(HostCleanupBranchCompletedPayload),
    #[serde(rename = "operation.accepted")]
    OperationAccepted(OperationAcceptedFact),
    #[serde(rename = "operation.settled")]
    OperationSettled(OperationSettledFact),
    #[serde(rename = "operation.failed")]
    OperationFailed(OperationFailedFact),
    #[serde(rename = "operation.cancelled")]
    OperationCancelled(OperationCancelledFact),
    #[serde(rename = "operation.uncertain")]
    OperationUncertain(OperationUncertainFact),
    #[serde(rename = "provider_input.accepted")]
    ProviderInputAccepted(ProviderInputAcceptedPayload),
    #[serde(rename = "provider_input.question_presented")]
    ProviderQuestionPresented(ProviderQuestionPresentedPayload),
    #[serde(rename = "provider_input.approval_presented")]
    ProviderApprovalPresented(ProviderApprovalPresentedPayload),
    #[serde(rename = "provider_input.wait_settled")]
    ProviderWaitSettled(ProviderWaitSettledPayload),
    #[serde(rename = "provider_input.delivered")]
    ProviderInputDelivered(ProviderInputDeliveredPayload),
    #[serde(rename = "browser.fact")]
    Browser(BrowserDurableFact),
}

#[derive(Serialize, Deserialize)]
struct EventDocument {
    schema_version: u32,
    #[serde(flatten)]
    body: EventBody,
}

impl From<&Event> for EventDocument {
    fn from(event: &Event) -> Self {
        let body = match event {
            Event::TaskCreated {
                task,
                connectivity,
                attention,
                activity,
                review_readiness,
            } => EventBody::TaskCreated(TaskCreatedPayload {
                task: task.clone(),
                connectivity: *connectivity,
                attention: *attention,
                activity: *activity,
                review_readiness: *review_readiness,
            }),
            Event::TaskRenamed { title } => EventBody::TaskRenamed(TaskRenamedPayload {
                title: title.clone(),
            }),
            Event::TaskAttentionSet { attention } => {
                EventBody::TaskAttentionSet(TaskAttentionSetPayload {
                    attention: *attention,
                })
            }
            Event::TaskCloseBegun { action_epoch } => {
                EventBody::TaskCloseBegun(TaskCloseBegunPayload {
                    action_epoch: *action_epoch,
                })
            }
            Event::TaskSettled => EventBody::TaskSettled(TaskUnitPayload {}),
            Event::TaskReopened => EventBody::TaskReopened(TaskUnitPayload {}),
            Event::TaskArchived => EventBody::TaskArchived(TaskUnitPayload {}),
            Event::TaskDeleted => EventBody::TaskDeleted(TaskUnitPayload {}),
            Event::AgentSessionRegistered { agent } => {
                EventBody::AgentSessionRegistered(AgentSessionRegisteredPayload {
                    agent: agent.clone(),
                })
            }
            Event::AgentProviderSessionBound {
                agent_session_id,
                resource_id,
                provider_session_id,
                runtime_generation,
            } => EventBody::AgentProviderSessionBound(AgentProviderSessionBoundPayload {
                agent_session_id: *agent_session_id,
                resource_id: *resource_id,
                provider_session_id: provider_session_id.clone(),
                runtime_generation: *runtime_generation,
            }),
            Event::PrimaryAgentSet { agent_session_id } => {
                EventBody::PrimaryAgentSet(PrimaryAgentSetPayload {
                    agent_session_id: *agent_session_id,
                })
            }
            Event::UnstartedPrimaryProviderRebound {
                agent_session_id,
                provider_kind,
            } => {
                EventBody::UnstartedPrimaryProviderRebound(UnstartedPrimaryProviderReboundPayload {
                    agent_session_id: *agent_session_id,
                    provider_kind: *provider_kind,
                })
            }

            Event::SpecialistRequested {
                specialist_id,
                requested_by,
                purpose,
                agent,
                permission,
                workspace,
                action_epoch,
                runtime_generation,
                resource_id,
            } => EventBody::SpecialistRequested(SpecialistRequestedPayload {
                specialist_id: *specialist_id,
                requested_by: *requested_by,
                purpose: purpose.clone(),
                agent: agent.clone(),
                permission: *permission,
                workspace: workspace.clone(),
                action_epoch: *action_epoch,
                runtime_generation: *runtime_generation,
                resource_id: *resource_id,
            }),
            Event::PrimaryPromoted {
                previous,
                promoted,
                action_epoch,
                runtime_generation,
            } => EventBody::PrimaryPromoted(PrimaryPromotedPayload {
                previous: *previous,
                promoted: *promoted,
                action_epoch: *action_epoch,
                runtime_generation: *runtime_generation,
            }),
            Event::SpecialistHandoffRecorded {
                specialist_id,
                artifact,
                structured,
                action_epoch,
                runtime_generation,
            } => EventBody::SpecialistHandoffRecorded(SpecialistHandoffRecordedPayload {
                specialist_id: *specialist_id,
                artifact: artifact.clone(),
                structured: *structured,
                action_epoch: *action_epoch,
                runtime_generation: *runtime_generation,
            }),
            Event::SpecialistClosed {
                specialist_id,
                action_epoch,
                runtime_generation,
            } => EventBody::SpecialistClosed(SpecialistClosedPayload {
                specialist_id: *specialist_id,
                action_epoch: *action_epoch,
                runtime_generation: *runtime_generation,
            }),
            Event::ArtifactRegistered { artifact } => {
                EventBody::ArtifactRegistered(ArtifactRegisteredPayload {
                    artifact: artifact.clone(),
                })
            }
            Event::ResourceRegistered { resource } => {
                EventBody::ResourceRegistered(ResourceRegisteredPayload {
                    resource: resource.clone(),
                })
            }
            Event::ResourceReleaseBegun {
                resource_id,
                runtime_generation,
            } => EventBody::ResourceReleaseBegun(ResourceReleaseBegunPayload {
                resource_id: *resource_id,
                runtime_generation: *runtime_generation,
            }),
            Event::ResourceReleased {
                resource_id,
                runtime_generation,
            } => EventBody::ResourceReleased(ResourceReleasedPayload {
                resource_id: *resource_id,
                runtime_generation: *runtime_generation,
            }),
            Event::HostCloseBegun {
                operation_id,
                action_epoch,
                inspection_id,
            } => EventBody::HostCloseBegun(HostCloseBegunPayload {
                operation_id: *operation_id,
                action_epoch: *action_epoch,
                inspection_id: *inspection_id,
            }),
            Event::HostCleanupBranchCompleted {
                operation_id,
                action_epoch,
                branch,
                outcome,
            } => EventBody::HostCleanupBranchCompleted(HostCleanupBranchCompletedPayload {
                operation_id: *operation_id,
                action_epoch: *action_epoch,
                branch: *branch,
                outcome: *outcome,
            }),
            Event::OperationAccepted(fact) => EventBody::OperationAccepted(fact.clone()),
            Event::OperationSettled(fact) => EventBody::OperationSettled(fact.clone()),
            Event::OperationFailed(fact) => EventBody::OperationFailed(fact.clone()),
            Event::OperationCancelled(fact) => EventBody::OperationCancelled(fact.clone()),
            Event::OperationUncertain(fact) => EventBody::OperationUncertain(fact.clone()),
            Event::ProviderInputAccepted {
                command_id,
                client_id,
                operation_id,
                agent_session_id,
                provider_kind,
                provider_session_id,
                runtime_generation,
                turn_id,
                action_epoch,
                question_id,
                approval_id,
                action,
                wait,
                delivery,
            } => EventBody::ProviderInputAccepted(ProviderInputAcceptedPayload {
                command_id: *command_id,
                client_id: *client_id,
                operation_id: *operation_id,
                agent_session_id: *agent_session_id,
                provider_kind: provider_kind.clone(),
                provider_session_id: provider_session_id.clone(),
                runtime_generation: *runtime_generation,
                turn_id: *turn_id,
                action_epoch: *action_epoch,
                question_id: *question_id,
                approval_id: *approval_id,
                action: action.clone(),
                wait: *wait,
                delivery: *delivery,
            }),
            Event::ProviderQuestionPresented {
                agent_session_id,
                provider_kind,
                provider_session_id,
                runtime_generation,
                turn_id,
                action_epoch,
                question_id,
            } => EventBody::ProviderQuestionPresented(ProviderQuestionPresentedPayload {
                agent_session_id: *agent_session_id,
                provider_kind: provider_kind.clone(),
                provider_session_id: provider_session_id.clone(),
                runtime_generation: *runtime_generation,
                turn_id: *turn_id,
                action_epoch: *action_epoch,
                question_id: *question_id,
            }),
            Event::ProviderApprovalPresented {
                agent_session_id,
                provider_kind,
                provider_session_id,
                runtime_generation,
                turn_id,
                action_epoch,
                approval_id,
            } => EventBody::ProviderApprovalPresented(ProviderApprovalPresentedPayload {
                agent_session_id: *agent_session_id,
                provider_kind: provider_kind.clone(),
                provider_session_id: provider_session_id.clone(),
                runtime_generation: *runtime_generation,
                turn_id: *turn_id,
                action_epoch: *action_epoch,
                approval_id: *approval_id,
            }),
            Event::ProviderWaitSettled { fence } => {
                EventBody::ProviderWaitSettled(ProviderWaitSettledPayload {
                    fence: fence.clone(),
                })
            }
            Event::ProviderInputDelivered {
                command_id,
                client_id,
                operation_id,
                agent_session_id,
                provider_kind,
                provider_session_id,
                runtime_generation,
                turn_id,
                action_epoch,
                question_id,
                approval_id,
            } => EventBody::ProviderInputDelivered(ProviderInputDeliveredPayload {
                command_id: *command_id,
                client_id: *client_id,
                operation_id: *operation_id,
                agent_session_id: *agent_session_id,
                provider_kind: provider_kind.clone(),
                provider_session_id: provider_session_id.clone(),
                runtime_generation: *runtime_generation,
                turn_id: *turn_id,
                action_epoch: *action_epoch,
                question_id: *question_id,
                approval_id: *approval_id,
            }),
            Event::Browser(fact) => EventBody::Browser(fact.clone()),
        };
        Self {
            schema_version: EVENT_SCHEMA_VERSION,
            body,
        }
    }
}

impl TryFrom<EventDocument> for Event {
    type Error = EventSerdeError;

    fn try_from(value: EventDocument) -> Result<Self, Self::Error> {
        if value.schema_version != EVENT_SCHEMA_VERSION {
            return Err(EventSerdeError::UnsupportedSchemaVersion(u64::from(
                value.schema_version,
            )));
        }
        Ok(match value.body {
            EventBody::TaskCreated(p) => {
                p.task
                    .validate_for_create()
                    .map_err(|_| EventSerdeError::Payload)?;
                Event::TaskCreated {
                    task: p.task,
                    connectivity: p.connectivity,
                    attention: p.attention,
                    activity: p.activity,
                    review_readiness: p.review_readiness,
                }
            }
            EventBody::TaskRenamed(p) => {
                let payload =
                    TaskRenamedPayload::validated(p.title).map_err(|_| EventSerdeError::Payload)?;
                Event::TaskRenamed {
                    title: payload.title,
                }
            }
            EventBody::TaskAttentionSet(p) => Event::TaskAttentionSet {
                attention: p.attention,
            },
            EventBody::TaskCloseBegun(p) => Event::TaskCloseBegun {
                action_epoch: p.action_epoch,
            },
            EventBody::TaskSettled(_) => Event::TaskSettled,
            EventBody::TaskReopened(_) => Event::TaskReopened,
            EventBody::TaskArchived(_) => Event::TaskArchived,
            EventBody::TaskDeleted(_) => Event::TaskDeleted,
            EventBody::AgentSessionRegistered(p) => {
                p.agent
                    .validate_for_registration()
                    .map_err(|_| EventSerdeError::Payload)?;
                Event::AgentSessionRegistered { agent: p.agent }
            }
            EventBody::AgentProviderSessionBound(p) => Event::AgentProviderSessionBound {
                agent_session_id: p.agent_session_id,
                resource_id: p.resource_id,
                provider_session_id: p.provider_session_id,
                runtime_generation: p.runtime_generation,
            },
            EventBody::PrimaryAgentSet(p) => Event::PrimaryAgentSet {
                agent_session_id: p.agent_session_id,
            },
            EventBody::UnstartedPrimaryProviderRebound(p) => {
                Event::UnstartedPrimaryProviderRebound {
                    agent_session_id: p.agent_session_id,
                    provider_kind: p.provider_kind,
                }
            }

            EventBody::SpecialistRequested(p) => {
                p.validate().map_err(|_| EventSerdeError::Payload)?;
                Event::SpecialistRequested {
                    specialist_id: p.specialist_id,
                    requested_by: p.requested_by,
                    purpose: p.purpose,
                    agent: p.agent,
                    permission: p.permission,
                    workspace: p.workspace,
                    action_epoch: p.action_epoch,
                    runtime_generation: p.runtime_generation,
                    resource_id: p.resource_id,
                }
            }
            EventBody::PrimaryPromoted(p) => {
                p.validate().map_err(|_| EventSerdeError::Payload)?;
                Event::PrimaryPromoted {
                    previous: p.previous,
                    promoted: p.promoted,
                    action_epoch: p.action_epoch,
                    runtime_generation: p.runtime_generation,
                }
            }
            EventBody::SpecialistHandoffRecorded(p) => {
                p.validate().map_err(|_| EventSerdeError::Payload)?;
                Event::SpecialistHandoffRecorded {
                    specialist_id: p.specialist_id,
                    artifact: p.artifact,
                    structured: p.structured,
                    action_epoch: p.action_epoch,
                    runtime_generation: p.runtime_generation,
                }
            }
            EventBody::SpecialistClosed(p) => Event::SpecialistClosed {
                specialist_id: p.specialist_id,
                action_epoch: p.action_epoch,
                runtime_generation: p.runtime_generation,
            },
            EventBody::ArtifactRegistered(p) => {
                p.artifact
                    .validate()
                    .map_err(|_| EventSerdeError::Payload)?;
                Event::ArtifactRegistered {
                    artifact: p.artifact,
                }
            }
            EventBody::ResourceRegistered(p) => {
                p.resource
                    .validate()
                    .map_err(|_| EventSerdeError::Payload)?;
                if p.resource.owner_kind != crate::domain::resource::OwnerKind::Task
                    || p.resource.lifecycle != crate::domain::resource::ResourceLifecycle::Active
                {
                    return Err(EventSerdeError::Payload);
                }
                Event::ResourceRegistered {
                    resource: p.resource,
                }
            }
            EventBody::ResourceReleaseBegun(p) => Event::ResourceReleaseBegun {
                resource_id: p.resource_id,
                runtime_generation: p.runtime_generation,
            },
            EventBody::ResourceReleased(p) => Event::ResourceReleased {
                resource_id: p.resource_id,
                runtime_generation: p.runtime_generation,
            },
            EventBody::HostCloseBegun(p) => Event::HostCloseBegun {
                operation_id: p.operation_id,
                action_epoch: p.action_epoch,
                inspection_id: p.inspection_id,
            },
            EventBody::HostCleanupBranchCompleted(p) => Event::HostCleanupBranchCompleted {
                operation_id: p.operation_id,
                action_epoch: p.action_epoch,
                branch: p.branch,
                outcome: p.outcome,
            },
            EventBody::OperationAccepted(fact) => Event::OperationAccepted(fact),
            EventBody::OperationSettled(fact) => Event::OperationSettled(fact),
            EventBody::OperationFailed(fact) => Event::OperationFailed(fact),
            EventBody::OperationCancelled(fact) => Event::OperationCancelled(fact),
            EventBody::OperationUncertain(fact) => Event::OperationUncertain(fact),
            EventBody::ProviderInputAccepted(p) => {
                if p.delivery.is_delivered() {
                    return Err(EventSerdeError::Payload);
                }
                let fence = ProviderFenceIdentity::new_with_identity(
                    Some(p.command_id),
                    // Event envelopes carry the task identity; the provider payload
                    // deliberately carries all remaining fence identities.
                    None,
                    p.agent_session_id,
                    p.provider_kind.clone(),
                    p.provider_session_id.clone(),
                    Some(p.operation_id),
                    p.runtime_generation,
                    p.action_epoch,
                    p.turn_id,
                    p.question_id,
                    p.approval_id,
                );
                validate_provider_fence(&fence, Some(&p.action), Some(p.wait), None)
                    .map_err(|_| EventSerdeError::Payload)?;
                Event::ProviderInputAccepted {
                    command_id: p.command_id,
                    client_id: p.client_id,
                    operation_id: p.operation_id,
                    agent_session_id: p.agent_session_id,
                    provider_kind: p.provider_kind,
                    provider_session_id: p.provider_session_id,
                    runtime_generation: p.runtime_generation,
                    turn_id: p.turn_id,
                    action_epoch: p.action_epoch,
                    question_id: p.question_id,
                    approval_id: p.approval_id,
                    action: p.action,
                    wait: p.wait,
                    delivery: p.delivery,
                }
            }
            EventBody::ProviderQuestionPresented(p) => {
                let fence = ProviderFenceIdentity::new_with_identity(
                    None,
                    None,
                    p.agent_session_id,
                    p.provider_kind.clone(),
                    p.provider_session_id.clone(),
                    None,
                    p.runtime_generation,
                    p.action_epoch,
                    p.turn_id,
                    Some(p.question_id),
                    None,
                );
                validate_provider_fence(&fence, None, None, None)
                    .map_err(|_| EventSerdeError::Payload)?;
                Event::ProviderQuestionPresented {
                    agent_session_id: p.agent_session_id,
                    provider_kind: p.provider_kind,
                    provider_session_id: p.provider_session_id,
                    runtime_generation: p.runtime_generation,
                    turn_id: p.turn_id,
                    action_epoch: p.action_epoch,
                    question_id: p.question_id,
                }
            }
            EventBody::ProviderApprovalPresented(p) => {
                let fence = ProviderFenceIdentity::new_with_identity(
                    None,
                    None,
                    p.agent_session_id,
                    p.provider_kind.clone(),
                    p.provider_session_id.clone(),
                    None,
                    p.runtime_generation,
                    p.action_epoch,
                    p.turn_id,
                    None,
                    Some(p.approval_id),
                );
                validate_provider_fence(&fence, None, None, None)
                    .map_err(|_| EventSerdeError::Payload)?;
                Event::ProviderApprovalPresented {
                    agent_session_id: p.agent_session_id,
                    provider_kind: p.provider_kind,
                    provider_session_id: p.provider_session_id,
                    runtime_generation: p.runtime_generation,
                    turn_id: p.turn_id,
                    action_epoch: p.action_epoch,
                    approval_id: p.approval_id,
                }
            }
            EventBody::ProviderWaitSettled(p) => {
                validate_provider_fence(&p.fence.identity(), None, None, None)
                    .map_err(|_| EventSerdeError::Payload)?;
                Event::ProviderWaitSettled { fence: p.fence }
            }
            EventBody::ProviderInputDelivered(p) => {
                let fence = ProviderFenceIdentity::new_with_identity(
                    Some(p.command_id),
                    None,
                    p.agent_session_id,
                    p.provider_kind.clone(),
                    p.provider_session_id.clone(),
                    Some(p.operation_id),
                    p.runtime_generation,
                    p.action_epoch,
                    p.turn_id,
                    p.question_id,
                    p.approval_id,
                );
                validate_provider_fence(&fence, None, None, None)
                    .map_err(|_| EventSerdeError::Payload)?;
                Event::ProviderInputDelivered {
                    command_id: p.command_id,
                    client_id: p.client_id,
                    operation_id: p.operation_id,
                    agent_session_id: p.agent_session_id,
                    provider_kind: p.provider_kind,
                    provider_session_id: p.provider_session_id,
                    runtime_generation: p.runtime_generation,
                    turn_id: p.turn_id,
                    action_epoch: p.action_epoch,
                    question_id: p.question_id,
                    approval_id: p.approval_id,
                }
            }
            EventBody::Browser(fact) => Event::Browser(fact),
        })
    }
}

impl Serialize for Event {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        EventDocument::from(self).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Event {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let document = EventDocument::deserialize(deserializer)?;
        Event::try_from(document).map_err(de::Error::custom)
    }
}

#[cfg(test)]
mod durable_workspace_serde_tests {
    use super::{DomainEvent, Event, EventSerdeError};
    use crate::domain::id::{EnvironmentId, EventId, ProjectId};
    use crate::domain::snapshot::{
        EventPage, SnapshotItem, SnapshotPage, SnapshotSection, TaskSnapshotItem,
    };
    use crate::domain::task::{
        ReviewReadiness, TaskActivity, TaskAssignment, TaskAttention, TaskConnectivity, TaskFacts,
        WorkspaceBindingFact, WorkspaceBindingKind, WorkspacePathFact, WorkspaceRef,
    };

    fn fact(path: &str, identity: &str) -> WorkspacePathFact {
        WorkspacePathFact::new(path.into(), identity.into(), None, None)
            .expect("valid host-only path fact")
    }

    fn task_with_host_binding() -> TaskFacts {
        let binding = WorkspaceBindingFact::issue(
            WorkspaceBindingKind::Main,
            fact(
                r"C:\Users\sentinel\secret-workspace",
                "windows:device-secret",
            ),
            fact(
                r"C:\Users\sentinel\secret-workspace",
                "windows:workspace-secret",
            ),
            Some(fact(
                r"C:\Users\sentinel\secret-workspace",
                "windows:repo-secret",
            )),
            Some(fact(
                r"C:\Users\sentinel\secret-workspace\.git",
                "windows:git-secret",
            )),
            None,
            Some(fact(
                r"C:\Users\sentinel\secret-workspace\.git",
                "windows:marker-secret",
            )),
            None,
            None,
            Some(fact(
                r"C:\Users\sentinel\secret-workspace\.git\HEAD",
                "windows:head-secret",
            )),
            None,
        )
        .expect("valid host binding");
        let mut task = TaskFacts::new(
            EnvironmentId::new(),
            "opaque workspace",
            Some("safe metadata".into()),
            ProjectId::new(),
            WorkspaceRef::HostBound { binding },
            TaskAssignment::LocalOwner,
            1_725_000_000_001,
        )
        .expect("task facts");
        task.revision = 1;
        task
    }

    fn assert_no_host_material(encoded: &str) {
        for sentinel in [
            r"C:\Users\sentinel\secret-workspace",
            "sentinel",
            "device-secret",
            "workspace-secret",
            "TOP_SECRET",
        ] {
            assert!(
                !encoded.contains(sentinel),
                "durable bytes leaked {sentinel}"
            );
        }
    }

    #[test]
    fn task_created_event_snapshot_and_replay_are_pathless() {
        let task = task_with_host_binding();
        let event = Event::TaskCreated {
            task: task.clone(),
            connectivity: TaskConnectivity::Connected,
            attention: TaskAttention::None,
            activity: TaskActivity::Idle,
            review_readiness: ReviewReadiness::NotReady,
        };
        let domain_event = DomainEvent {
            id: EventId::new(),
            task_id: Some(task.id),
            sequence: 7,
            task_revision: Some(1),
            occurred_at_ms: 1_725_000_000_002,
            payload: event.clone(),
        };
        let snapshot = SnapshotPage {
            snapshot_id: crate::domain::id::SnapshotId::new(),
            through_sequence: 7,
            section: SnapshotSection::Tasks,
            after_item: None,
            items: vec![SnapshotItem::Task(TaskSnapshotItem {
                task,
                connectivity: TaskConnectivity::Connected,
                attention: TaskAttention::None,
                activity: TaskActivity::Idle,
                review_readiness: ReviewReadiness::NotReady,
                primary_agent_id: None,
            })],
            encoded_bytes: 1,
            next_cursor: None,
        };
        let replay = EventPage {
            after_sequence: 0,
            through_sequence: 7,
            events: vec![domain_event],
            next_cursor: None,
        };

        for encoded in [
            serde_json::to_string(&event).expect("event json"),
            rmp_serde::to_vec(&event)
                .expect("event msgpack")
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
            serde_json::to_string(&snapshot).expect("snapshot json"),
            rmp_serde::to_vec(&snapshot)
                .expect("snapshot msgpack")
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
            serde_json::to_string(&replay).expect("replay json"),
            rmp_serde::to_vec(&replay)
                .expect("replay msgpack")
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
        ] {
            assert_no_host_material(&encoded);
        }

        let event_json = serde_json::to_string(&event).expect("event json");
        let _: Event = serde_json::from_str(&event_json).expect("event replay");
        let snapshot_json = serde_json::to_string(&snapshot).expect("snapshot json");
        let _: SnapshotPage = serde_json::from_str(&snapshot_json).expect("snapshot restore");
        let replay_json = serde_json::to_string(&replay).expect("replay json");
        let _: EventPage = serde_json::from_str(&replay_json).expect("replay restore");
    }

    #[test]
    fn event_serde_errors_never_echo_attacker_text() {
        let rendered = format!("{}", EventSerdeError::UnknownEventType);
        assert_eq!(rendered, "unknown event type");
        assert!(!rendered.contains("attacker"));
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventSerdeError {
    UnknownEventType,
    UnsupportedSchemaVersion(u64),
    InvalidEnvelope,
    Payload,
}

impl fmt::Display for EventSerdeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownEventType => write!(f, "unknown event type"),
            Self::UnsupportedSchemaVersion(version) => {
                write!(f, "unsupported event schema version: {version}")
            }
            Self::InvalidEnvelope => write!(f, "invalid durable event envelope"),
            Self::Payload => write!(f, "invalid event payload"),
        }
    }
}

impl std::error::Error for EventSerdeError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyError {
    MissingSnapshot,
    SnapshotAlreadyExists,
    TaskMismatch,
    NotFound,
    InvalidTransition,
    RevisionConflict,
    OwnershipConflict,
    AlreadyExists,
}

impl fmt::Display for ApplyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingSnapshot => write!(f, "snapshot required"),
            Self::SnapshotAlreadyExists => write!(f, "snapshot already exists"),
            Self::TaskMismatch => write!(f, "event task does not match snapshot"),
            Self::NotFound => write!(f, "referenced entity not found"),
            Self::InvalidTransition => write!(f, "invalid event transition"),
            Self::RevisionConflict => write!(f, "task revision conflict"),
            Self::OwnershipConflict => write!(f, "ownership conflict"),
            Self::AlreadyExists => write!(f, "entity already exists"),
        }
    }
}

impl std::error::Error for ApplyError {}

pub fn apply(
    snapshot: Option<TaskSnapshot>,
    event: &DomainEvent,
) -> Result<TaskSnapshot, ApplyError> {
    match &event.payload {
        Event::TaskCreated {
            task,
            connectivity,
            attention,
            activity,
            review_readiness,
        } => {
            if snapshot.is_some() {
                return Err(ApplyError::SnapshotAlreadyExists);
            }
            match event.task_id {
                Some(id) if id == task.id => {}
                _ => return Err(ApplyError::TaskMismatch),
            }
            if event.task_revision != Some(1) || task.revision != 1 {
                return Err(ApplyError::RevisionConflict);
            }
            task.validate_for_create()
                .map_err(|_| ApplyError::InvalidTransition)?;
            Ok(TaskSnapshot {
                task: task.clone(),
                connectivity: *connectivity,
                attention: *attention,
                activity: *activity,
                review_readiness: *review_readiness,
                agents: BTreeMap::new(),
                primary_agent_id: None,
                artifacts: BTreeMap::new(),
                resources: BTreeMap::new(),
                provider_sessions: BTreeMap::new(),
                browser: {
                    let mut browser = BrowserBook::new();
                    browser.open_task(task.id).map_err(apply_browser_error)?;
                    browser
                },
                terminal_facts: Default::default(),
                terminal_strip: Default::default(),
            })
        }
        Event::Browser(fact) => {
            let mut snap = snapshot.ok_or(ApplyError::MissingSnapshot)?;
            require_matching_task_id(&snap, event)?;
            if fact.task_id() != snap.task.id {
                return Err(ApplyError::OwnershipConflict);
            }
            snap.browser
                .apply_facts(std::slice::from_ref(fact))
                .map_err(apply_browser_error)?;
            Ok(snap)
        }
        Event::OperationAccepted(_fact) => {
            let snap = snapshot.ok_or(ApplyError::MissingSnapshot)?;
            require_matching_task_id(&snap, event)?;
            Ok(snap)
        }
        Event::HostCloseBegun { .. } | Event::HostCleanupBranchCompleted { .. } => {
            Err(ApplyError::InvalidTransition)
        }
        Event::OperationSettled(_)
        | Event::OperationFailed(_)
        | Event::OperationCancelled(_)
        | Event::OperationUncertain(_) => {
            let snap = snapshot.ok_or(ApplyError::MissingSnapshot)?;
            require_matching_task_id(&snap, event)?;
            Ok(snap)
        }
        Event::ProviderInputDelivered { .. } => {
            let mut snap = snapshot.ok_or(ApplyError::MissingSnapshot)?;
            require_matching_task_id(&snap, event)?;
            // Delivery is a durable provider-session projection change, but it
            // does not consume a task revision. Replay must still apply it so
            // rebuild/status validation agrees with the live projector.
            apply_into(&mut snap, &event.payload, event.occurred_at_ms)?;
            Ok(snap)
        }
        other => {
            let mut snap = snapshot.ok_or(ApplyError::MissingSnapshot)?;
            require_matching_task_id(&snap, event)?;
            let next_revision = require_next_revision(&snap, event)?;
            apply_into(&mut snap, other, event.occurred_at_ms)?;
            snap.task.revision = next_revision;
            Ok(snap)
        }
    }
}

fn require_matching_task_id(snap: &TaskSnapshot, event: &DomainEvent) -> Result<(), ApplyError> {
    match event.task_id {
        Some(id) if id == snap.task.id => Ok(()),
        Some(_) => Err(ApplyError::TaskMismatch),
        None => Err(ApplyError::TaskMismatch),
    }
}

fn require_next_revision(snap: &TaskSnapshot, event: &DomainEvent) -> Result<u64, ApplyError> {
    let Some(observed) = event.task_revision else {
        return Err(ApplyError::RevisionConflict);
    };
    let expected = snap
        .task
        .revision
        .checked_add(1)
        .ok_or(ApplyError::InvalidTransition)?;
    if observed != expected {
        return Err(ApplyError::RevisionConflict);
    }
    Ok(expected)
}

fn validate_snapshot_provider_fence(
    snap: &TaskSnapshot,
    agent_session_id: AgentSessionId,
    fence: &ProviderFenceIdentity,
    action: Option<&ProviderInputAction>,
    wait: Option<bool>,
    current_turn: Option<TurnId>,
    allow_closing: bool,
) -> Result<(), ApplyError> {
    let agent = snap
        .agents
        .get(&agent_session_id)
        .ok_or(ApplyError::NotFound)?;
    let context = crate::domain::provider_input::ProviderFenceContext {
        task_id: snap.task.id,
        agent_session_id,
        agent_task_id: agent.task_id,
        provider_kind: agent.provider_kind,
        provider_session_id: agent.provider_session_id.clone(),
        runtime_generation: agent.runtime_generation,
        action_epoch: snap.task.action_epoch,
        lifecycle: agent.lifecycle,
        current_turn,
        allow_closing,
    };
    validate_provider_fence(fence, action, wait, Some(&context))
        .map_err(|_| ApplyError::InvalidTransition)
}

fn apply_into(
    snap: &mut TaskSnapshot,
    payload: &Event,
    occurred_at_ms: i64,
) -> Result<(), ApplyError> {
    match payload {
        Event::TaskRenamed { title } => {
            let title = TaskFacts::canonicalize_title(title.clone())
                .map_err(|_| ApplyError::InvalidTransition)?;
            snap.task.title = title;
        }
        Event::TaskAttentionSet { attention } => {
            snap.attention = *attention;
        }
        Event::TaskCloseBegun { action_epoch } => {
            if !matches!(
                snap.task.lifecycle,
                TaskLifecycle::Open | TaskLifecycle::Settled
            ) {
                return Err(ApplyError::InvalidTransition);
            }
            let expected = snap
                .task
                .action_epoch
                .checked_add(1)
                .ok_or(ApplyError::InvalidTransition)?;
            if *action_epoch != expected {
                return Err(ApplyError::InvalidTransition);
            }
            snap.task.lifecycle = TaskLifecycle::Closing;
            snap.task.action_epoch = *action_epoch;
            snap.browser
                .close_task(snap.task.id)
                .map_err(apply_browser_error)?;
        }
        Event::TaskSettled => {
            if snap.task.lifecycle != TaskLifecycle::Open {
                return Err(ApplyError::InvalidTransition);
            }
            snap.task.lifecycle = TaskLifecycle::Settled;
        }
        Event::TaskReopened => {
            match snap.task.lifecycle {
                TaskLifecycle::Settled | TaskLifecycle::Closing | TaskLifecycle::Archived => {}
                TaskLifecycle::Open | TaskLifecycle::Deleted => {
                    return Err(ApplyError::InvalidTransition)
                }
            }
            snap.browser
                .open_task(snap.task.id)
                .map_err(apply_browser_error)?;
            snap.task.lifecycle = TaskLifecycle::Open;
        }
        Event::TaskArchived => {
            if snap.task.lifecycle != TaskLifecycle::Closing {
                return Err(ApplyError::InvalidTransition);
            }
            if snap.resources.values().any(|resource| {
                matches!(
                    resource.lifecycle,
                    ResourceLifecycle::Active | ResourceLifecycle::Releasing
                )
            }) {
                return Err(ApplyError::InvalidTransition);
            }
            if snap.agents.values().any(|agent| {
                agent.lifecycle == AgentSessionLifecycle::Open
                    && matches!(agent.role, AgentRole::Specialist { .. })
            }) {
                return Err(ApplyError::InvalidTransition);
            }
            snap.task.lifecycle = TaskLifecycle::Archived;
        }
        Event::TaskDeleted => {
            if snap.task.lifecycle != TaskLifecycle::Archived {
                return Err(ApplyError::InvalidTransition);
            }
            snap.task.lifecycle = TaskLifecycle::Deleted;
        }
        Event::AgentSessionRegistered { agent } => {
            if agent.task_id != snap.task.id {
                return Err(ApplyError::OwnershipConflict);
            }
            agent
                .validate_for_registration()
                .map_err(|_| ApplyError::InvalidTransition)?;
            if snap.agents.contains_key(&agent.id) {
                return Err(ApplyError::AlreadyExists);
            }
            snap.agents.insert(agent.id, agent.clone());
            snap.provider_sessions.entry(agent.id).or_default();
        }
        Event::AgentProviderSessionBound {
            agent_session_id,
            resource_id,
            provider_session_id,
            runtime_generation,
        } => {
            let Some(agent) = snap.agents.get(agent_session_id) else {
                return Err(ApplyError::NotFound);
            };
            if agent.lifecycle != AgentSessionLifecycle::Open
                || agent.runtime_generation != *runtime_generation
            {
                return Err(ApplyError::InvalidTransition);
            }
            let Some(resource) = snap.resources.get(resource_id) else {
                return Err(ApplyError::NotFound);
            };
            AgentResourceBinding::from_facts(agent, resource)
                .map_err(|_| ApplyError::InvalidTransition)?;
            let agent = snap
                .agents
                .get_mut(agent_session_id)
                .ok_or(ApplyError::NotFound)?;
            match agent.provider_session_id.as_ref() {
                Some(bound) if bound == provider_session_id => {}
                Some(_) => return Err(ApplyError::OwnershipConflict),
                None => {
                    agent.provider_session_id = Some(provider_session_id.clone());
                    agent.revision = agent.revision.saturating_add(1);
                }
            }
        }
        Event::PrimaryAgentSet { agent_session_id } => {
            let Some(agent) = snap.agents.get(agent_session_id) else {
                return Err(ApplyError::NotFound);
            };
            if !matches!(agent.role, crate::domain::agent::AgentRole::Primary) {
                return Err(ApplyError::InvalidTransition);
            }
            snap.primary_agent_id = Some(*agent_session_id);
        }
        Event::UnstartedPrimaryProviderRebound {
            agent_session_id,
            provider_kind,
        } => {
            if !snap.is_unstarted_draft() {
                return Err(ApplyError::InvalidTransition);
            }
            if snap.primary_agent_id != Some(*agent_session_id) {
                return Err(ApplyError::OwnershipConflict);
            }
            let Some(agent) = snap.agents.get_mut(agent_session_id) else {
                return Err(ApplyError::NotFound);
            };
            if !matches!(agent.role, crate::domain::agent::AgentRole::Primary)
                || agent.lifecycle != AgentSessionLifecycle::Open
                || agent.provider_session_id.is_some()
            {
                return Err(ApplyError::InvalidTransition);
            }
            agent.provider_kind = *provider_kind;
            agent.revision = agent.revision.saturating_add(1);
        }
        Event::SpecialistRequested {
            specialist_id,
            requested_by,
            agent,
            purpose,
            permission,
            action_epoch,
            runtime_generation,
            resource_id,
            workspace,
            ..
        } => {
            if agent.id != *specialist_id || agent.task_id != snap.task.id {
                return Err(ApplyError::OwnershipConflict);
            }
            if !canonical::is_bounded_canonical(purpose)
                || !matches!(agent.role, AgentRole::Specialist { .. })
                || !matches!(permission, SpecialistPermission::ReadOnly)
                || *action_epoch != snap.task.action_epoch
                || agent.runtime_generation != *runtime_generation
                || agent.id == *requested_by
            {
                return Err(ApplyError::InvalidTransition);
            }
            workspace
                .validate()
                .map_err(|_| ApplyError::InvalidTransition)?;
            let Some(requester) = snap.agents.get(requested_by) else {
                return Err(ApplyError::NotFound);
            };
            if requester.lifecycle != AgentSessionLifecycle::Open
                || !matches!(requester.role, AgentRole::Primary)
                || snap.primary_agent_id != Some(*requested_by)
                || requester.runtime_generation != *runtime_generation
            {
                return Err(ApplyError::InvalidTransition);
            }
            if snap
                .agents
                .values()
                .filter(|existing| {
                    existing.lifecycle == AgentSessionLifecycle::Open
                        && matches!(
                            existing.role,
                            AgentRole::Primary | AgentRole::Specialist { .. }
                        )
                })
                .count()
                >= crate::domain::command::DEFAULT_MAX_TOP_LEVEL_RUNTIMES
            {
                return Err(ApplyError::InvalidTransition);
            }
            if let Some(resource_id) = resource_id {
                let Some(resource) = snap.resources.get(resource_id) else {
                    return Err(ApplyError::NotFound);
                };
                if resource.task_id != Some(snap.task.id)
                    || resource.runtime_generation != *runtime_generation
                {
                    return Err(ApplyError::InvalidTransition);
                }
            }
            agent
                .validate_for_registration()
                .map_err(|_| ApplyError::InvalidTransition)?;
            if snap.agents.contains_key(&agent.id) {
                return Err(ApplyError::AlreadyExists);
            }
            snap.agents.insert(agent.id, agent.clone());
        }
        Event::PrimaryPromoted {
            previous,
            promoted,
            action_epoch,
            runtime_generation,
        } => {
            if previous == promoted || *action_epoch != snap.task.action_epoch {
                return Err(ApplyError::InvalidTransition);
            }
            if snap.primary_agent_id != Some(*previous) {
                return Err(ApplyError::InvalidTransition);
            }
            let Some(previous_agent) = snap.agents.get_mut(previous) else {
                return Err(ApplyError::NotFound);
            };
            if !matches!(previous_agent.role, AgentRole::Primary)
                || previous_agent.lifecycle != AgentSessionLifecycle::Open
                || previous_agent.runtime_generation != *runtime_generation
            {
                return Err(ApplyError::InvalidTransition);
            }
            previous_agent.role =
                AgentRole::specialist("primary").map_err(|_| ApplyError::InvalidTransition)?;
            let Some(promoted_agent) = snap.agents.get_mut(promoted) else {
                return Err(ApplyError::NotFound);
            };
            if !matches!(promoted_agent.role, AgentRole::Specialist { .. })
                || promoted_agent.lifecycle != AgentSessionLifecycle::Open
                || promoted_agent.runtime_generation != *runtime_generation
            {
                return Err(ApplyError::InvalidTransition);
            }
            promoted_agent.role = AgentRole::Primary;
            snap.primary_agent_id = Some(*promoted);
        }
        Event::SpecialistHandoffRecorded {
            specialist_id,
            artifact,
            structured,
            action_epoch,
            runtime_generation,
        } => {
            if artifact.task_id != snap.task.id {
                return Err(ApplyError::OwnershipConflict);
            }
            if artifact.kind != ArtifactKind::ReviewReport
                || *action_epoch != snap.task.action_epoch
            {
                return Err(ApplyError::InvalidTransition);
            }
            let ArtifactContentRef::InlineUtf8(body) = &artifact.content_ref else {
                return Err(ApplyError::InvalidTransition);
            };
            if body.len() > MAX_SPECIALIST_RAW_ARTIFACT_BYTES {
                return Err(ApplyError::InvalidTransition);
            }
            artifact
                .validate()
                .map_err(|_| ApplyError::InvalidTransition)?;
            verify_inline_content_digest(artifact).map_err(|_| ApplyError::InvalidTransition)?;
            if *structured {
                let result = structured_specialist_result(artifact)
                    .map_err(|_| ApplyError::InvalidTransition)?;
                for id in result.evidence.iter().chain(&result.artifacts) {
                    let existing = snap.artifacts.get(id).ok_or(ApplyError::NotFound)?;
                    if existing.task_id != snap.task.id {
                        return Err(ApplyError::OwnershipConflict);
                    }
                }
            }
            if snap.artifacts.contains_key(&artifact.id) {
                return Err(ApplyError::AlreadyExists);
            }
            let Some(specialist) = snap.agents.get_mut(specialist_id) else {
                return Err(ApplyError::NotFound);
            };
            if !matches!(specialist.role, AgentRole::Specialist { .. })
                || specialist.lifecycle != AgentSessionLifecycle::Open
                || specialist.runtime_generation != *runtime_generation
            {
                return Err(ApplyError::InvalidTransition);
            }
            specialist.lifecycle = AgentSessionLifecycle::Closed;
            snap.artifacts.insert(artifact.id, artifact.clone());
        }
        Event::SpecialistClosed {
            specialist_id,
            action_epoch,
            runtime_generation,
        } => {
            let Some(specialist) = snap.agents.get_mut(specialist_id) else {
                return Err(ApplyError::NotFound);
            };
            if !matches!(specialist.role, AgentRole::Specialist { .. })
                || specialist.lifecycle != AgentSessionLifecycle::Open
                || specialist.runtime_generation != *runtime_generation
                || *action_epoch != snap.task.action_epoch
            {
                return Err(ApplyError::InvalidTransition);
            }
            specialist.lifecycle = AgentSessionLifecycle::Closed;
        }
        Event::ArtifactRegistered { artifact } => {
            if artifact.task_id != snap.task.id {
                return Err(ApplyError::OwnershipConflict);
            }
            artifact
                .validate()
                .map_err(|_| ApplyError::InvalidTransition)?;
            if snap.artifacts.contains_key(&artifact.id) {
                return Err(ApplyError::AlreadyExists);
            }
            snap.artifacts.insert(artifact.id, artifact.clone());
        }
        Event::ResourceRegistered { resource } => {
            if resource.owner_kind != crate::domain::resource::OwnerKind::Task {
                return Err(ApplyError::OwnershipConflict);
            }
            match resource.task_id {
                Some(id) if id == snap.task.id => {}
                _ => return Err(ApplyError::OwnershipConflict),
            }
            resource
                .validate()
                .map_err(|_| ApplyError::InvalidTransition)?;
            if resource.lifecycle != ResourceLifecycle::Active {
                return Err(ApplyError::InvalidTransition);
            }
            if snap.resources.contains_key(&resource.id) {
                return Err(ApplyError::AlreadyExists);
            }
            snap.resources.insert(resource.id, resource.clone());
        }
        Event::ResourceReleaseBegun {
            resource_id,
            runtime_generation,
        } => {
            let Some(resource) = snap.resources.get_mut(resource_id) else {
                return Err(ApplyError::NotFound);
            };
            if resource.owner_kind != crate::domain::resource::OwnerKind::Task
                || resource.task_id != Some(snap.task.id)
            {
                return Err(ApplyError::OwnershipConflict);
            }
            if resource.lifecycle != ResourceLifecycle::Active {
                return Err(ApplyError::InvalidTransition);
            }
            if resource.runtime_generation != *runtime_generation {
                return Err(ApplyError::InvalidTransition);
            }
            resource.lifecycle = ResourceLifecycle::Releasing;
            resource.updated_at_ms = occurred_at_ms;
        }
        Event::ResourceReleased {
            resource_id,
            runtime_generation,
        } => {
            let Some(resource) = snap.resources.get_mut(resource_id) else {
                return Err(ApplyError::NotFound);
            };
            if resource.owner_kind != crate::domain::resource::OwnerKind::Task
                || resource.task_id != Some(snap.task.id)
            {
                return Err(ApplyError::OwnershipConflict);
            }
            if resource.lifecycle != ResourceLifecycle::Releasing {
                return Err(ApplyError::InvalidTransition);
            }
            if resource.runtime_generation != *runtime_generation {
                return Err(ApplyError::InvalidTransition);
            }
            resource.lifecycle = ResourceLifecycle::Released;
            resource.updated_at_ms = occurred_at_ms;
        }
        Event::ProviderInputAccepted {
            command_id,
            client_id,
            operation_id,
            agent_session_id,
            provider_kind,
            provider_session_id,
            runtime_generation,
            turn_id,
            action_epoch,
            question_id,
            approval_id,
            action,
            wait,
            delivery,
        } => {
            if delivery.is_delivered() {
                return Err(ApplyError::InvalidTransition);
            }
            let current_turn = snap
                .provider_sessions
                .get(agent_session_id)
                .and_then(|session| {
                    if matches!(action, ProviderInputAction::SendNow { .. })
                        && session.can_begin_send_now_turn(*turn_id)
                    {
                        None
                    } else {
                        session.current_turn
                    }
                });
            let fence = ProviderFenceIdentity::new_with_identity(
                Some(*command_id),
                Some(snap.task.id),
                *agent_session_id,
                provider_kind.clone(),
                provider_session_id.clone(),
                Some(*operation_id),
                *runtime_generation,
                *action_epoch,
                *turn_id,
                *question_id,
                *approval_id,
            );
            validate_snapshot_provider_fence(
                snap,
                *agent_session_id,
                &fence,
                Some(action),
                Some(*wait),
                current_turn,
                false,
            )?;
            if snap.task.lifecycle == TaskLifecycle::Settled
                && matches!(action, ProviderInputAction::SendNow { .. })
            {
                snap.task.lifecycle = TaskLifecycle::Open;
            }
            let session = snap.provider_sessions.entry(*agent_session_id).or_default();
            match action {
                ProviderInputAction::AnswerQuestion { question_id, .. } => {
                    if session.question_winners.contains_key(question_id) {
                        return Err(ApplyError::AlreadyExists);
                    }
                    if session.open_question != Some(*question_id) {
                        return Err(ApplyError::InvalidTransition);
                    }
                }
                ProviderInputAction::ResolveApproval { approval_id, .. } => {
                    if session.approval_winners.contains_key(approval_id) {
                        return Err(ApplyError::AlreadyExists);
                    }
                    if session.open_approval != Some(*approval_id) {
                        return Err(ApplyError::InvalidTransition);
                    }
                }
                _ => {}
            }
            session.current_turn = Some(*turn_id);
            session.last_settlement = Some(ProviderInputSettlement {
                command_id: *command_id,
                operation_id: Some(*operation_id),
                intent: crate::domain::provider_input::ProviderIntentPhase::Accepted,
                delivery: *delivery,
            });
            let winner = ProviderResolutionWinner {
                command_id: *command_id,
                client_id: *client_id,
                accepted_at_ms: occurred_at_ms,
            };
            match action {
                ProviderInputAction::AnswerQuestion {
                    question_id: qid, ..
                } => {
                    session
                        .bounded_insert_question_winner(*qid, winner)
                        .map_err(|_| ApplyError::InvalidTransition)?;
                    session.open_question = None;
                }
                ProviderInputAction::ResolveApproval {
                    approval_id: aid, ..
                } => {
                    session
                        .bounded_insert_approval_winner(*aid, winner)
                        .map_err(|_| ApplyError::InvalidTransition)?;
                    session.open_approval = None;
                }
                _ => {}
            }
            if *wait {
                session
                    .bounded_insert_wait(
                        *command_id,
                        ProviderWaitRecord {
                            fence: ProviderWaitFence::new_with_identity(
                                *command_id,
                                snap.task.id,
                                *operation_id,
                                *action_epoch,
                                *agent_session_id,
                                provider_kind.clone(),
                                provider_session_id.clone(),
                                *runtime_generation,
                                *turn_id,
                                *question_id,
                                *approval_id,
                            ),
                            pending: true,
                        },
                    )
                    .map_err(|_| ApplyError::InvalidTransition)?;
            }
        }
        Event::ProviderQuestionPresented {
            agent_session_id,
            provider_kind,
            provider_session_id,
            runtime_generation,
            turn_id,
            action_epoch,
            question_id,
        } => {
            let current_turn = snap
                .provider_sessions
                .get(agent_session_id)
                .and_then(|session| session.current_turn);
            let fence = ProviderFenceIdentity::new_with_identity(
                None,
                Some(snap.task.id),
                *agent_session_id,
                provider_kind.clone(),
                provider_session_id.clone(),
                None,
                *runtime_generation,
                *action_epoch,
                *turn_id,
                Some(*question_id),
                None,
            );
            validate_snapshot_provider_fence(
                snap,
                *agent_session_id,
                &fence,
                None,
                None,
                current_turn,
                false,
            )?;
            let session = snap.provider_sessions.entry(*agent_session_id).or_default();
            if session.question_winners.contains_key(question_id) {
                return Err(ApplyError::AlreadyExists);
            }
            if session.open_question.is_some() {
                return Err(ApplyError::AlreadyExists);
            }
            session.current_turn = Some(*turn_id);
            session.open_question = Some(*question_id);
        }
        Event::ProviderApprovalPresented {
            agent_session_id,
            provider_kind,
            provider_session_id,
            runtime_generation,
            turn_id,
            action_epoch,
            approval_id,
        } => {
            let current_turn = snap
                .provider_sessions
                .get(agent_session_id)
                .and_then(|session| session.current_turn);
            let fence = ProviderFenceIdentity::new_with_identity(
                None,
                Some(snap.task.id),
                *agent_session_id,
                provider_kind.clone(),
                provider_session_id.clone(),
                None,
                *runtime_generation,
                *action_epoch,
                *turn_id,
                None,
                Some(*approval_id),
            );
            validate_snapshot_provider_fence(
                snap,
                *agent_session_id,
                &fence,
                None,
                None,
                current_turn,
                false,
            )?;
            let session = snap.provider_sessions.entry(*agent_session_id).or_default();
            if session.approval_winners.contains_key(approval_id) {
                return Err(ApplyError::AlreadyExists);
            }
            if session.open_approval.is_some() {
                return Err(ApplyError::AlreadyExists);
            }
            session.current_turn = Some(*turn_id);
            session.open_approval = Some(*approval_id);
        }
        Event::ProviderWaitSettled { fence } => {
            let current_turn = snap
                .provider_sessions
                .get(&fence.agent_session_id())
                .and_then(|session| session.current_turn);
            validate_snapshot_provider_fence(
                snap,
                fence.agent_session_id(),
                &fence.identity(),
                None,
                None,
                current_turn,
                true,
            )?;
            let session = snap
                .provider_sessions
                .get_mut(&fence.agent_session_id())
                .ok_or(ApplyError::NotFound)?;
            let record = session
                .waits
                .get_mut(&fence.command_id())
                .ok_or(ApplyError::NotFound)?;
            if !record.fence.matches(fence) {
                return Err(ApplyError::InvalidTransition);
            }
            if !record.pending {
                return Err(ApplyError::AlreadyExists);
            }
            record.pending = false;
        }
        Event::ProviderInputDelivered {
            command_id,
            operation_id,
            agent_session_id,
            provider_kind,
            provider_session_id,
            runtime_generation,
            turn_id,
            action_epoch,
            question_id,
            approval_id,
            ..
        } => {
            let current_turn = snap
                .provider_sessions
                .get(agent_session_id)
                .and_then(|session| session.current_turn);
            let fence = ProviderFenceIdentity::new_with_identity(
                Some(*command_id),
                Some(snap.task.id),
                *agent_session_id,
                provider_kind.clone(),
                provider_session_id.clone(),
                Some(*operation_id),
                *runtime_generation,
                *action_epoch,
                *turn_id,
                *question_id,
                *approval_id,
            );
            validate_snapshot_provider_fence(
                snap,
                *agent_session_id,
                &fence,
                None,
                None,
                current_turn,
                true,
            )?;
            let session = snap
                .provider_sessions
                .get_mut(agent_session_id)
                .ok_or(ApplyError::NotFound)?;
            let Some(settlement) = session.last_settlement else {
                return Err(ApplyError::InvalidTransition);
            };
            if settlement.command_id != *command_id
                || settlement.operation_id != Some(*operation_id)
            {
                // Provider input is accepted on the connection lane while the
                // destination adapter settles physical writes on its own
                // ordered lane. A later accepted keystroke can therefore be
                // the presentation `last_settlement` when an earlier write is
                // durably delivered. The operation/outbox lineage validates
                // that earlier delivery; keep the newer presentation fact.
                return Ok(());
            }
            if settlement.delivery.is_delivered() {
                return Err(ApplyError::InvalidTransition);
            }
            session.last_settlement = Some(ProviderInputSettlement {
                command_id: *command_id,
                operation_id: Some(*operation_id),
                intent: crate::domain::provider_input::ProviderIntentPhase::Accepted,
                delivery: ProviderDeliveryVisibility::delivered(),
            });
        }
        Event::TaskCreated { .. }
        | Event::HostCloseBegun { .. }
        | Event::HostCleanupBranchCompleted { .. }
        | Event::OperationAccepted(_)
        | Event::OperationSettled(_)
        | Event::OperationFailed(_)
        | Event::OperationCancelled(_)
        | Event::OperationUncertain(_)
        | Event::Browser(_) => unreachable!("handled by apply()"),
    }
    Ok(())
}

fn apply_browser_error(error: BrowserContractError) -> ApplyError {
    match error {
        BrowserContractError::CrossTask => ApplyError::OwnershipConflict,
        BrowserContractError::GenerationMismatch
        | BrowserContractError::ClosedTask
        | BrowserContractError::IdempotencyConflict
        | BrowserContractError::BoundExceeded
        | BrowserContractError::InvalidRequest
        | BrowserContractError::HostEffectUnavailable => ApplyError::InvalidTransition,
    }
}

pub fn apply_all(
    mut snapshot: Option<TaskSnapshot>,
    events: &[Event],
) -> Result<TaskSnapshot, ApplyError> {
    let task_id = snapshot.as_ref().map(|snap| snap.task.id);
    for (index, payload) in events.iter().enumerate() {
        let sequence = u64::try_from(index.saturating_add(1)).unwrap_or(u64::MAX);
        let task_revision = if payload.is_task_mutation() {
            match snapshot.as_ref() {
                Some(snap) => Some(
                    snap.task
                        .revision
                        .checked_add(1)
                        .ok_or(ApplyError::InvalidTransition)?,
                ),
                None => Some(1),
            }
        } else {
            None
        };
        snapshot = Some(apply(
            snapshot,
            &DomainEvent {
                id: EventId::new(),
                task_id,
                sequence,
                task_revision,
                occurred_at_ms: 1,
                payload: payload.clone(),
            },
        )?);
    }
    snapshot.ok_or(ApplyError::MissingSnapshot)
}
