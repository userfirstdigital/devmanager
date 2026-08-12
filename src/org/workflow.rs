//! Board comments, handoffs, assignments, and review as metadata events.
//! Full BoardCard rows are not mirrored into local SQLite.

use serde::{Deserialize, Serialize};

use crate::domain::id::{ArtifactId, TaskId};
use crate::org::error::OrgError;
use crate::org::identity::BoardCardId;
use crate::org::ids::HandoffId;
use crate::org::managed::{EnrollmentState, ManagedTaskLink};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssignmentAcceptance {
    PendingLocalAccept,
    Accepted,
    Declined,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewState {
    None,
    Requested,
    Approved,
    ChangesRequested,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum BoardWorkflowEvent {
    Assignment {
        board_card_id: BoardCardId,
        assignee: String,
        acceptance: AssignmentAcceptance,
        auto_launch: bool,
    },
    Comment {
        board_card_id: BoardCardId,
        event_id: String,
        body_redacted: bool,
    },
    Handoff {
        handoff_id: HandoffId,
        artifact_id: ArtifactId,
        summary: String,
        status: String,
        checkpoint: Option<String>,
        git_reference: Option<String>,
    },
    Review {
        board_card_id: BoardCardId,
        state: ReviewState,
    },
    PhaseBlocked {
        board_card_id: BoardCardId,
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedWorkflowProjection {
    pub task_id: TaskId,
    pub board_card_id: BoardCardId,
    pub assignment: Option<AssignmentAcceptance>,
    pub review: ReviewState,
    pub last_comment_event_id: Option<String>,
    pub last_handoff_id: Option<HandoffId>,
}

impl ManagedWorkflowProjection {
    pub fn from_link(link: &ManagedTaskLink) -> Result<Self, OrgError> {
        if link.enrollment_state != EnrollmentState::Enrolled {
            return Err(OrgError::Unlinked);
        }
        Ok(Self {
            task_id: link.local_task_id,
            board_card_id: link.board_card_id.clone(),
            assignment: None,
            review: ReviewState::None,
            last_comment_event_id: None,
            last_handoff_id: None,
        })
    }

    pub fn apply(&mut self, event: BoardWorkflowEvent) -> Result<(), OrgError> {
        match event {
            BoardWorkflowEvent::Assignment {
                board_card_id,
                auto_launch,
                acceptance,
                ..
            } => {
                self.ensure_card(&board_card_id)?;
                if auto_launch {
                    return Err(OrgError::AutoLaunchForbidden);
                }
                self.assignment = Some(acceptance);
            }
            BoardWorkflowEvent::Comment {
                board_card_id,
                event_id,
                body_redacted,
            } => {
                self.ensure_card(&board_card_id)?;
                if !body_redacted {
                    return Err(OrgError::ProhibitedField);
                }
                self.last_comment_event_id = Some(event_id);
            }
            BoardWorkflowEvent::Handoff {
                handoff_id,
                summary,
                ..
            } => {
                if summary.trim().is_empty() {
                    return Err(OrgError::EmptyIdentity);
                }
                self.last_handoff_id = Some(handoff_id);
            }
            BoardWorkflowEvent::Review {
                board_card_id,
                state,
            } => {
                self.ensure_card(&board_card_id)?;
                self.review = state;
            }
            BoardWorkflowEvent::PhaseBlocked { board_card_id, .. } => {
                self.ensure_card(&board_card_id)?;
            }
        }
        Ok(())
    }

    fn ensure_card(&self, board_card_id: &BoardCardId) -> Result<(), OrgError> {
        if board_card_id != &self.board_card_id {
            return Err(OrgError::CrossTenant);
        }
        Ok(())
    }
}
