//! Session-level Connect authorization on top of [`PermissionEvaluator`].
//!
//! Network authentication never implies personal-prompt, filesystem, process,
//! or approval authority. Unknown actions deny.

use crate::client::port::{ApprovalAnswerCall, PromptQueryCall, ProviderInputCall};
use crate::domain::command::Command;
use crate::domain::id::TaskId;
use crate::domain::query::Query;
use crate::protocol::ClientRequest;

use super::permission::{
    ActionId, ConnectRole, PermissionDecision, PermissionDenyReason, PermissionEvaluator,
    PermissionRequest,
};
use super::ConnectPrivacyClass;

/// One authorized Connect attempt after the transport has authenticated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionPermissionContext {
    pub role: ConnectRole,
    pub privacy: ConnectPrivacyClass,
}

impl SessionPermissionContext {
    pub const fn paired_owner(privacy: ConnectPrivacyClass) -> Self {
        Self {
            role: ConnectRole::PairedOwner,
            privacy,
        }
    }
}

/// Maps a host request onto a local action before the host port is invoked.
pub fn action_for_client_request(request: &ClientRequest) -> Option<(ActionId, Option<TaskId>)> {
    match request {
        ClientRequest::Query(envelope) => {
            let action = match envelope.query {
                Query::TaskSnapshot
                | Query::SnapshotPage { .. }
                | Query::ReleaseSnapshot { .. }
                | Query::OpenEventReplay { .. }
                | Query::ContinueEventReplay { .. }
                | Query::ReleaseEventReplay { .. }
                | Query::OpenArtifactContent { .. }
                | Query::ContinueArtifactContent { .. }
                | Query::ReleaseArtifactContent { .. } => ActionId::READ_TASK,
                Query::OperationStatus { .. } => ActionId::READ_OPERATION,
                Query::InspectHostQuit => ActionId::READ_OPERATION,
            };
            Some((action, envelope.task_id))
        }
        ClientRequest::Command(envelope) => {
            let action = match envelope.command {
                Command::CreateTask(_)
                | Command::RenameTask(_)
                | Command::SetTaskAttention(_)
                | Command::BeginCloseTask
                | Command::ReopenTask
                | Command::RegisterAgentSession { .. }
                | Command::SetPrimaryAgent { .. }
                | Command::RegisterArtifact { .. }
                | Command::RegisterResource { .. }
                | Command::ReleaseResource { .. } => ActionId::MUTATE_TASK,
                Command::ConfirmHostQuit(_) => ActionId::MUTATE_TASK,
            };
            let task_id = match &envelope.command {
                Command::CreateTask(intent) => Some(intent.id),
                _ => envelope.task_id,
            };
            Some((action, task_id))
        }
        ClientRequest::Detach(_) => None,
    }
}

pub fn action_for_provider_input(call: &ProviderInputCall) -> PermissionRequest {
    PermissionRequest {
        role: ConnectRole::PairedOwner,
        task_id: Some(call.task_id),
        action: ActionId::SEND_PROMPT,
    }
}

pub fn action_for_approval(call: &ApprovalAnswerCall, dangerous: bool) -> PermissionRequest {
    PermissionRequest {
        role: ConnectRole::PairedOwner,
        task_id: Some(call.task_id),
        action: if dangerous {
            ActionId::APPROVE_DANGEROUS
        } else {
            ActionId::ANSWER_REQUEST
        },
    }
}

pub fn action_for_prompt_query(_call: &PromptQueryCall) -> PermissionRequest {
    PermissionRequest {
        role: ConnectRole::PairedOwner,
        task_id: None,
        action: ActionId::READ_PERSONAL_PROMPTS,
    }
}

/// Local gate used by [`super::session::ConnectSession`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionAuthorizer {
    evaluator: PermissionEvaluator,
    context: SessionPermissionContext,
}

impl SessionAuthorizer {
    pub const fn new(evaluator: PermissionEvaluator, context: SessionPermissionContext) -> Self {
        Self { evaluator, context }
    }

    pub const fn paired_owner() -> Self {
        Self::new(
            PermissionEvaluator::owner_only(),
            SessionPermissionContext::paired_owner(ConnectPrivacyClass::RawContent),
        )
    }

    pub const fn context(self) -> SessionPermissionContext {
        self.context
    }

    pub fn authorize_request(&self, request: &ClientRequest) -> PermissionDecision {
        let Some((action, task_id)) = action_for_client_request(request) else {
            return PermissionDecision::Allow;
        };
        self.evaluate(PermissionRequest {
            role: self.context.role,
            task_id,
            action,
        })
    }

    pub fn authorize(&self, request: PermissionRequest) -> PermissionDecision {
        self.evaluate(PermissionRequest {
            role: self.context.role,
            ..request
        })
    }

    pub fn authorize_personal_prompts(&self) -> PermissionDecision {
        if !matches!(self.context.role, ConnectRole::PairedOwner) {
            return PermissionDecision::Denied(PermissionDenyReason::OwnerOnly);
        }
        self.evaluate(PermissionRequest {
            role: self.context.role,
            task_id: None,
            action: ActionId::READ_PERSONAL_PROMPTS,
        })
    }

    fn evaluate(&self, request: PermissionRequest) -> PermissionDecision {
        self.evaluator.evaluate(request)
    }
}
