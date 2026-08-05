use serde::{Deserialize, Serialize};

use crate::domain::id::{CommandId, OperationId, TaskId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OperationLifecycle {
    Accepted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationFacts {
    pub id: OperationId,
    pub command_id: CommandId,
    pub task_id: Option<TaskId>,
    pub state: OperationLifecycle,
    pub accepted_at_ms: i64,
}

impl OperationFacts {
    pub fn accepted(command_id: CommandId, task_id: Option<TaskId>, accepted_at_ms: i64) -> Self {
        Self {
            id: OperationId::new(),
            command_id,
            task_id,
            state: OperationLifecycle::Accepted,
            accepted_at_ms,
        }
    }
}
