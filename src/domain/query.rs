use serde::{Deserialize, Serialize};

use crate::domain::id::{ClientId, OperationId, RequestId, TaskId};
use crate::domain::operation::OperationState;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryEnvelope {
    pub request_id: RequestId,
    pub client_id: ClientId,
    pub task_id: Option<TaskId>,
    pub query: Query,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Query {
    OperationStatus { operation_id: OperationId },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryResult {
    OperationStatus {
        operation_id: OperationId,
        state: OperationState,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryError {
    NotFound,
    Unauthorized,
    InvalidRequest,
    UnsupportedCapability,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryReply {
    pub request_id: RequestId,
    pub outcome: QueryOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryOutcome {
    Ok(QueryResult),
    Err(QueryError),
}
