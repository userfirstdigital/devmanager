use serde::de::{self, Deserializer};
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};

use crate::domain::canonical;
use crate::domain::id::{CommandId, EventId, OperationId, ResourceId, TaskId};

/// Maximum UTF-8 byte length for durable external reconciliation identities.
pub const MAX_EXTERNAL_IDENTITY_BYTES: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationErrorCode {
    SideEffectFailed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CancellationReason {
    Superseded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationUncertaintyCode {
    AmbiguousDispatch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutcomeFenceError {
    PartialResourceFence,
    InvalidSourceForKind,
    EmptyExternalIdentity,
    ExternalIdentityTooLong,
}

impl std::fmt::Display for OutcomeFenceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PartialResourceFence => {
                write!(
                    f,
                    "resource_id and runtime_generation must both be present or both absent"
                )
            }
            Self::InvalidSourceForKind => {
                write!(
                    f,
                    "outcome source is not valid for the requested outcome kind"
                )
            }
            Self::EmptyExternalIdentity => {
                write!(f, "external identity must be non-empty canonical text")
            }
            Self::ExternalIdentityTooLong => {
                write!(
                    f,
                    "external identity exceeds {MAX_EXTERNAL_IDENTITY_BYTES} bytes"
                )
            }
        }
    }
}

impl std::error::Error for OutcomeFenceError {}

pub fn validate_outcome_fence(
    resource_id: Option<ResourceId>,
    runtime_generation: Option<u64>,
) -> Result<(), OutcomeFenceError> {
    match (resource_id, runtime_generation) {
        (None, None) | (Some(_), Some(_)) => Ok(()),
        _ => Err(OutcomeFenceError::PartialResourceFence),
    }
}

fn canonicalize_external_identity(value: impl Into<String>) -> Result<String, OutcomeFenceError> {
    let Some(canonical) = canonical::canonicalize(value.into()) else {
        return Err(OutcomeFenceError::EmptyExternalIdentity);
    };
    if canonical.len() > MAX_EXTERNAL_IDENTITY_BYTES {
        return Err(OutcomeFenceError::ExternalIdentityTooLong);
    }
    Ok(canonical)
}

fn require_canonical_external_identity(value: &str) -> Result<(), OutcomeFenceError> {
    if value.is_empty() || !canonical::is_canonical(value) {
        return Err(OutcomeFenceError::EmptyExternalIdentity);
    }
    if value.len() > MAX_EXTERNAL_IDENTITY_BYTES {
        return Err(OutcomeFenceError::ExternalIdentityTooLong);
    }
    Ok(())
}

/// Paired resource identity and runtime generation fence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceFence {
    pub resource_id: ResourceId,
    pub runtime_generation: u64,
}

impl ResourceFence {
    pub fn new(resource_id: ResourceId, runtime_generation: u64) -> Self {
        Self {
            resource_id,
            runtime_generation,
        }
    }

    pub fn from_parts(
        resource_id: Option<ResourceId>,
        runtime_generation: Option<u64>,
    ) -> Result<Option<Self>, OutcomeFenceError> {
        validate_outcome_fence(resource_id, runtime_generation)?;
        Ok(match (resource_id, runtime_generation) {
            (Some(resource_id), Some(runtime_generation)) => {
                Some(Self::new(resource_id, runtime_generation))
            }
            (None, None) => None,
            _ => unreachable!("validate_outcome_fence rejects partial fences"),
        })
    }

    pub fn into_parts(fence: Option<Self>) -> (Option<ResourceId>, Option<u64>) {
        match fence {
            Some(fence) => (Some(fence.resource_id), Some(fence.runtime_generation)),
            None => (None, None),
        }
    }
}

/// Provenance of an operation outcome. Reconciliation evidence is durable text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutcomeSource {
    Dispatch,
    VerifiedReconciliation {
        effect_index: u32,
        external_identity: String,
    },
}

impl OutcomeSource {
    pub fn verified_reconciliation(
        effect_index: u32,
        external_identity: impl Into<String>,
    ) -> Result<Self, OutcomeFenceError> {
        Ok(Self::VerifiedReconciliation {
            effect_index,
            external_identity: canonicalize_external_identity(external_identity)?,
        })
    }

    pub fn validate(&self) -> Result<(), OutcomeFenceError> {
        match self {
            Self::Dispatch => Ok(()),
            Self::VerifiedReconciliation {
                external_identity, ..
            } => require_canonical_external_identity(external_identity),
        }
    }

    pub fn is_dispatch(&self) -> bool {
        matches!(self, Self::Dispatch)
    }

    pub fn is_verified_reconciliation(&self) -> bool {
        matches!(self, Self::VerifiedReconciliation { .. })
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct VerifiedReconciliationWire {
    effect_index: u32,
    external_identity: String,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum OutcomeSourceWire {
    Dispatch,
    VerifiedReconciliation(VerifiedReconciliationWire),
}

impl Serialize for OutcomeSource {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let wire = match self {
            Self::Dispatch => OutcomeSourceWire::Dispatch,
            Self::VerifiedReconciliation {
                effect_index,
                external_identity,
            } => OutcomeSourceWire::VerifiedReconciliation(VerifiedReconciliationWire {
                effect_index: *effect_index,
                external_identity: external_identity.clone(),
            }),
        };
        wire.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for OutcomeSource {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = OutcomeSourceWire::deserialize(deserializer)?;
        match wire {
            OutcomeSourceWire::Dispatch => Ok(Self::Dispatch),
            OutcomeSourceWire::VerifiedReconciliation(inner) => {
                // Strict wire path: reject non-canonical text instead of trimming.
                require_canonical_external_identity(&inner.external_identity)
                    .map_err(de::Error::custom)?;
                Ok(Self::VerifiedReconciliation {
                    effect_index: inner.effect_index,
                    external_identity: inner.external_identity,
                })
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum OperationOutcomeKind {
    Settled { result_event_ids: Vec<EventId> },
    Failed { code: OperationErrorCode },
    Cancelled { reason: CancellationReason },
    Uncertain { code: OperationUncertaintyCode },
}

impl OperationOutcomeKind {
    pub fn allows_verified_reconciliation(&self) -> bool {
        matches!(self, Self::Settled { .. } | Self::Failed { .. })
    }
}

/// Side-effect outcome observation. Does not duplicate command_id; the store derives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationOutcome {
    pub operation_id: OperationId,
    pub occurred_at_ms: i64,
    pub action_epoch: Option<u64>,
    pub resource_fence: Option<ResourceFence>,
    pub source: OutcomeSource,
    pub kind: OperationOutcomeKind,
}

impl OperationOutcome {
    pub fn new(
        operation_id: OperationId,
        occurred_at_ms: i64,
        action_epoch: Option<u64>,
        resource_fence: Option<ResourceFence>,
        source: OutcomeSource,
        kind: OperationOutcomeKind,
    ) -> Result<Self, OutcomeFenceError> {
        source.validate()?;
        validate_source_for_kind(&source, &kind)?;
        Ok(Self {
            operation_id,
            occurred_at_ms,
            action_epoch,
            resource_fence,
            source,
            kind,
        })
    }
}

pub fn validate_source_for_kind(
    source: &OutcomeSource,
    kind: &OperationOutcomeKind,
) -> Result<(), OutcomeFenceError> {
    match (source, kind.allows_verified_reconciliation()) {
        (OutcomeSource::Dispatch, _) => Ok(()),
        (OutcomeSource::VerifiedReconciliation { .. }, true) => Ok(()),
        (OutcomeSource::VerifiedReconciliation { .. }, false) => {
            Err(OutcomeFenceError::InvalidSourceForKind)
        }
    }
}

/// Settled/failed durable facts may carry either dispatch or verified-reconciliation source.
pub fn validate_terminal_fact_source(source: &OutcomeSource) -> Result<(), OutcomeFenceError> {
    source.validate()
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OperationOutcomeWire {
    operation_id: OperationId,
    occurred_at_ms: i64,
    action_epoch: Option<u64>,
    resource_fence: Option<ResourceFence>,
    source: OutcomeSource,
    kind: OperationOutcomeKind,
}

impl Serialize for OperationOutcome {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        OperationOutcomeWire {
            operation_id: self.operation_id,
            occurred_at_ms: self.occurred_at_ms,
            action_epoch: self.action_epoch,
            resource_fence: self.resource_fence,
            source: self.source.clone(),
            kind: self.kind.clone(),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for OperationOutcome {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = OperationOutcomeWire::deserialize(deserializer)?;
        Self::new(
            wire.operation_id,
            wire.occurred_at_ms,
            wire.action_epoch,
            wire.resource_fence,
            wire.source,
            wire.kind,
        )
        .map_err(de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationState {
    Accepted,
    Settled {
        settled_at_ms: i64,
        result_event_ids: Vec<EventId>,
    },
    Failed {
        settled_at_ms: i64,
        code: OperationErrorCode,
    },
    Cancelled {
        settled_at_ms: i64,
        reason: CancellationReason,
    },
    Uncertain {
        observed_at_ms: i64,
        code: OperationUncertaintyCode,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationFacts {
    pub id: OperationId,
    pub command_id: CommandId,
    pub task_id: Option<TaskId>,
    pub state: OperationState,
    pub accepted_at_ms: i64,
}

impl OperationFacts {
    pub fn accepted(command_id: CommandId, task_id: Option<TaskId>, accepted_at_ms: i64) -> Self {
        Self {
            id: OperationId::new(),
            command_id,
            task_id,
            state: OperationState::Accepted,
            accepted_at_ms,
        }
    }
}
