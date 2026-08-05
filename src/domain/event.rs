use std::collections::BTreeMap;
use std::fmt;

use serde::de::{self, Deserializer};
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};

use crate::domain::agent::AgentSessionFacts;
use crate::domain::artifact::ArtifactFacts;
use crate::domain::id::{AgentSessionId, CommandId, EventId, OperationId, ResourceId, TaskId};
use crate::domain::operation::{
    validate_outcome_fence, CancellationReason, OperationErrorCode, OperationUncertaintyCode,
    OutcomeFenceError,
};
use crate::domain::resource::{ResourceFacts, ResourceLifecycle};
use crate::domain::snapshot::TaskSnapshot;
use crate::domain::task::{
    ReviewReadiness, TaskActivity, TaskAttention, TaskConnectivity, TaskFacts, TaskLifecycle,
};

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
    pub fn new(
        command_id: CommandId,
        operation_id: OperationId,
        settled_at_ms: i64,
        result_event_ids: Vec<EventId>,
        action_epoch: Option<u64>,
        resource_id: Option<ResourceId>,
        runtime_generation: Option<u64>,
    ) -> Result<Self, OutcomeFenceError> {
        validate_outcome_fence(resource_id, runtime_generation)?;
        Ok(Self {
            command_id,
            operation_id,
            settled_at_ms,
            result_event_ids,
            action_epoch,
            resource_id,
            runtime_generation,
        })
    }
}

impl OperationFailedFact {
    pub fn new(
        command_id: CommandId,
        operation_id: OperationId,
        settled_at_ms: i64,
        code: OperationErrorCode,
        action_epoch: Option<u64>,
        resource_id: Option<ResourceId>,
        runtime_generation: Option<u64>,
    ) -> Result<Self, OutcomeFenceError> {
        validate_outcome_fence(resource_id, runtime_generation)?;
        Ok(Self {
            command_id,
            operation_id,
            settled_at_ms,
            code,
            action_epoch,
            resource_id,
            runtime_generation,
        })
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
}

impl Serialize for OperationSettledFact {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        OperationSettledFactWire {
            command_id: self.command_id,
            operation_id: self.operation_id,
            settled_at_ms: self.settled_at_ms,
            result_event_ids: self.result_event_ids.clone(),
            action_epoch: self.action_epoch,
            resource_id: self.resource_id,
            runtime_generation: self.runtime_generation,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for OperationSettledFact {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = OperationSettledFactWire::deserialize(deserializer)?;
        Self::new(
            wire.command_id,
            wire.operation_id,
            wire.settled_at_ms,
            wire.result_event_ids,
            wire.action_epoch,
            wire.resource_id,
            wire.runtime_generation,
        )
        .map_err(de::Error::custom)
    }
}

impl Serialize for OperationFailedFact {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        OperationFailedFactWire {
            command_id: self.command_id,
            operation_id: self.operation_id,
            settled_at_ms: self.settled_at_ms,
            code: self.code,
            action_epoch: self.action_epoch,
            resource_id: self.resource_id,
            runtime_generation: self.runtime_generation,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for OperationFailedFact {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = OperationFailedFactWire::deserialize(deserializer)?;
        Self::new(
            wire.command_id,
            wire.operation_id,
            wire.settled_at_ms,
            wire.code,
            wire.action_epoch,
            wire.resource_id,
            wire.runtime_generation,
        )
        .map_err(de::Error::custom)
    }
}

impl Serialize for OperationCancelledFact {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
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
pub struct PrimaryAgentSetPayload {
    pub agent_session_id: AgentSessionId,
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
    TaskReopened,
    TaskArchived,
    AgentSessionRegistered {
        agent: AgentSessionFacts,
    },
    PrimaryAgentSet {
        agent_session_id: AgentSessionId,
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
    OperationAccepted(OperationAcceptedFact),
    OperationSettled(OperationSettledFact),
    OperationFailed(OperationFailedFact),
    OperationCancelled(OperationCancelledFact),
    OperationUncertain(OperationUncertainFact),
}

impl Event {
    pub fn event_type(&self) -> &'static str {
        match self {
            Self::TaskCreated { .. } => "task.created",
            Self::TaskRenamed { .. } => "task.renamed",
            Self::TaskAttentionSet { .. } => "task.attention_set",
            Self::TaskCloseBegun { .. } => "task.close_begun",
            Self::TaskReopened => "task.reopened",
            Self::TaskArchived => "task.archived",
            Self::AgentSessionRegistered { .. } => "agent_session.registered",
            Self::PrimaryAgentSet { .. } => "primary_agent.set",
            Self::ArtifactRegistered { .. } => "artifact.registered",
            Self::ResourceRegistered { .. } => "resource.registered",
            Self::ResourceReleaseBegun { .. } => "resource.release_begun",
            Self::ResourceReleased { .. } => "resource.released",
            Self::OperationAccepted(_) => "operation.accepted",
            Self::OperationSettled(_) => "operation.settled",
            Self::OperationFailed(_) => "operation.failed",
            Self::OperationCancelled(_) => "operation.cancelled",
            Self::OperationUncertain(_) => "operation.uncertain",
        }
    }

    pub fn is_task_mutation(&self) -> bool {
        !matches!(
            self,
            Self::OperationAccepted(_)
                | Self::OperationSettled(_)
                | Self::OperationFailed(_)
                | Self::OperationCancelled(_)
                | Self::OperationUncertain(_)
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
    #[serde(rename = "task.reopened")]
    TaskReopened(TaskUnitPayload),
    #[serde(rename = "task.archived")]
    TaskArchived(TaskUnitPayload),
    #[serde(rename = "agent_session.registered")]
    AgentSessionRegistered(AgentSessionRegisteredPayload),
    #[serde(rename = "primary_agent.set")]
    PrimaryAgentSet(PrimaryAgentSetPayload),
    #[serde(rename = "artifact.registered")]
    ArtifactRegistered(ArtifactRegisteredPayload),
    #[serde(rename = "resource.registered")]
    ResourceRegistered(ResourceRegisteredPayload),
    #[serde(rename = "resource.release_begun")]
    ResourceReleaseBegun(ResourceReleaseBegunPayload),
    #[serde(rename = "resource.released")]
    ResourceReleased(ResourceReleasedPayload),
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
            Event::TaskReopened => EventBody::TaskReopened(TaskUnitPayload {}),
            Event::TaskArchived => EventBody::TaskArchived(TaskUnitPayload {}),
            Event::AgentSessionRegistered { agent } => {
                EventBody::AgentSessionRegistered(AgentSessionRegisteredPayload {
                    agent: agent.clone(),
                })
            }
            Event::PrimaryAgentSet { agent_session_id } => {
                EventBody::PrimaryAgentSet(PrimaryAgentSetPayload {
                    agent_session_id: *agent_session_id,
                })
            }
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
            Event::OperationAccepted(fact) => EventBody::OperationAccepted(fact.clone()),
            Event::OperationSettled(fact) => EventBody::OperationSettled(fact.clone()),
            Event::OperationFailed(fact) => EventBody::OperationFailed(fact.clone()),
            Event::OperationCancelled(fact) => EventBody::OperationCancelled(fact.clone()),
            Event::OperationUncertain(fact) => EventBody::OperationUncertain(fact.clone()),
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
                    .map_err(|err| EventSerdeError::Payload(err.to_string()))?;
                Event::TaskCreated {
                    task: p.task,
                    connectivity: p.connectivity,
                    attention: p.attention,
                    activity: p.activity,
                    review_readiness: p.review_readiness,
                }
            }
            EventBody::TaskRenamed(p) => {
                let payload = TaskRenamedPayload::validated(p.title)
                    .map_err(|err| EventSerdeError::Payload(err.to_string()))?;
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
            EventBody::TaskReopened(_) => Event::TaskReopened,
            EventBody::TaskArchived(_) => Event::TaskArchived,
            EventBody::AgentSessionRegistered(p) => {
                p.agent
                    .validate_for_registration()
                    .map_err(|err| EventSerdeError::Payload(err.to_string()))?;
                Event::AgentSessionRegistered { agent: p.agent }
            }
            EventBody::PrimaryAgentSet(p) => Event::PrimaryAgentSet {
                agent_session_id: p.agent_session_id,
            },
            EventBody::ArtifactRegistered(p) => {
                p.artifact
                    .validate()
                    .map_err(|err| EventSerdeError::Payload(err.to_string()))?;
                Event::ArtifactRegistered {
                    artifact: p.artifact,
                }
            }
            EventBody::ResourceRegistered(p) => {
                p.resource
                    .validate()
                    .map_err(|err| EventSerdeError::Payload(err.to_string()))?;
                if p.resource.owner_kind != crate::domain::resource::OwnerKind::Task
                    || p.resource.lifecycle != crate::domain::resource::ResourceLifecycle::Active
                {
                    return Err(EventSerdeError::Payload(
                        "resource registration requires Active Task-owned facts".into(),
                    ));
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
            EventBody::OperationAccepted(fact) => Event::OperationAccepted(fact),
            EventBody::OperationSettled(fact) => Event::OperationSettled(fact),
            EventBody::OperationFailed(fact) => Event::OperationFailed(fact),
            EventBody::OperationCancelled(fact) => Event::OperationCancelled(fact),
            EventBody::OperationUncertain(fact) => Event::OperationUncertain(fact),
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventSerdeError {
    UnknownEventType(String),
    UnsupportedSchemaVersion(u64),
    InvalidEnvelope,
    Payload(String),
}

impl fmt::Display for EventSerdeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownEventType(name) => write!(f, "unknown event type: {name}"),
            Self::UnsupportedSchemaVersion(version) => {
                write!(f, "unsupported event schema version: {version}")
            }
            Self::InvalidEnvelope => write!(f, "invalid durable event envelope"),
            Self::Payload(message) => write!(f, "invalid event payload: {message}"),
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
            })
        }
        Event::OperationAccepted(_fact) => {
            let snap = snapshot.ok_or(ApplyError::MissingSnapshot)?;
            require_matching_task_id(&snap, event)?;
            Ok(snap)
        }
        Event::OperationSettled(_)
        | Event::OperationFailed(_)
        | Event::OperationCancelled(_)
        | Event::OperationUncertain(_) => {
            let snap = snapshot.ok_or(ApplyError::MissingSnapshot)?;
            require_matching_task_id(&snap, event)?;
            Ok(snap)
        }
        other => {
            let mut snap = snapshot.ok_or(ApplyError::MissingSnapshot)?;
            require_matching_task_id(&snap, event)?;
            let next_revision = require_next_revision(&snap, event)?;
            apply_into(&mut snap, other)?;
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

fn apply_into(snap: &mut TaskSnapshot, payload: &Event) -> Result<(), ApplyError> {
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
            if snap.task.lifecycle != TaskLifecycle::Open {
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
        }
        Event::TaskReopened => {
            match snap.task.lifecycle {
                TaskLifecycle::Closing | TaskLifecycle::Archived => {}
                TaskLifecycle::Open => return Err(ApplyError::InvalidTransition),
            }
            snap.task.lifecycle = TaskLifecycle::Open;
        }
        Event::TaskArchived => {
            if snap.task.lifecycle != TaskLifecycle::Closing {
                return Err(ApplyError::InvalidTransition);
            }
            snap.task.lifecycle = TaskLifecycle::Archived;
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
        }
        Event::TaskCreated { .. }
        | Event::OperationAccepted(_)
        | Event::OperationSettled(_)
        | Event::OperationFailed(_)
        | Event::OperationCancelled(_)
        | Event::OperationUncertain(_) => unreachable!("handled by apply()"),
    }
    Ok(())
}
