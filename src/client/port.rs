//! Host-command port used by Connect sessions.
//!
//! This is the HostClient-equivalent surface: mutations, queries, and
//! subscriptions enter as domain envelopes. Presentation DTOs and writer
//! leases are not part of this contract.

use std::fmt;

use crate::domain::command::{CommandEnvelope, CommandReceipt};
use crate::domain::id::{ArtifactId, RequestId, TaskId};
use crate::domain::query::{QueryEnvelope, QueryReply};
use crate::host::IpcError;

use super::host_client::{ArtifactContentBatch, EventReplayBatch};
use super::connection::UnsolicitedServerMessage;

/// Caller-owned provider input. The host remains execution authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderInputCall {
    pub task_id: TaskId,
    pub text: String,
}

/// First-answer-wins approval/question response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalAnswerCall {
    pub task_id: TaskId,
    pub request_id: RequestId,
    pub approved: bool,
}

/// On-demand child transcript fetch. Never part of the initial snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptFetchCall {
    pub task_id: TaskId,
    pub artifact_id: ArtifactId,
}

/// Owner-only personal prompt-library search. Task invitations cannot use this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptQueryCall {
    pub query: String,
    pub resume_cursor: Option<Vec<u8>>,
}

/// Bounded prompt metadata page. Bodies stay on-demand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptMetadataPage {
    pub items: Vec<PromptMetadataItem>,
    pub next_cursor: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptMetadataItem {
    pub prompt_id: String,
    pub title: String,
    pub version: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostPortError {
    Unavailable,
    Unauthorized,
    Unsupported,
    CorrelationMismatch,
    Bounds,
    Ipc(String),
}

impl fmt::Display for HostPortError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable => formatter.write_str("host command port is unavailable"),
            Self::Unauthorized => formatter.write_str("host command port unauthorized"),
            Self::Unsupported => formatter.write_str("host command port unsupported"),
            Self::CorrelationMismatch => formatter.write_str("host command port correlation mismatch"),
            Self::Bounds => formatter.write_str("host command port bounds exceeded"),
            Self::Ipc(detail) => write!(formatter, "host command port ipc: {detail}"),
        }
    }
}

impl std::error::Error for HostPortError {}

impl From<IpcError> for HostPortError {
    fn from(error: IpcError) -> Self {
        match error {
            IpcError::Unavailable => Self::Unavailable,
            IpcError::Unauthorized => Self::Unauthorized,
            IpcError::UnsupportedCapability => Self::Unsupported,
            IpcError::CorrelationMismatch => Self::CorrelationMismatch,
            other => Self::Ipc(other.to_string()),
        }
    }
}

/// Synchronous HostClient-equivalent used by ConnectSession and tests.
pub trait HostCommandPort {
    fn execute_command(
        &mut self,
        envelope: CommandEnvelope,
    ) -> Result<CommandReceipt, HostPortError>;

    fn query(&mut self, envelope: QueryEnvelope) -> Result<QueryReply, HostPortError>;

    fn provider_input(
        &mut self,
        call: ProviderInputCall,
    ) -> Result<CommandReceipt, HostPortError>;

    fn answer_approval(
        &mut self,
        call: ApprovalAnswerCall,
    ) -> Result<CommandReceipt, HostPortError>;

    fn fetch_child_transcript(
        &mut self,
        call: TranscriptFetchCall,
    ) -> Result<ArtifactContentBatch, HostPortError>;

    fn query_personal_prompts(
        &mut self,
        call: PromptQueryCall,
    ) -> Result<PromptMetadataPage, HostPortError>;

    fn drain_unsolicited(&mut self) -> Vec<UnsolicitedServerMessage>;

    fn open_event_replay(
        &mut self,
        after_sequence: u64,
    ) -> Result<EventReplayBatch, HostPortError> {
        let _ = after_sequence;
        Err(HostPortError::Unsupported)
    }
}
