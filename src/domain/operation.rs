use serde::{Deserialize, Serialize};

use crate::domain::id::{CommandId, EventId, OperationId, ResourceId, TaskId};

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
