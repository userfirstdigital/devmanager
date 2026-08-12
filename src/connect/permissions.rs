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
    ActionId, AuthoritativePermissionContext, ConnectRole, PermissionDecision,
    PermissionDenyReason, PermissionEvaluator, PermissionRequest, ScopedPermissionGrant,
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

    pub const fn watcher(task_id: TaskId, privacy: ConnectPrivacyClass) -> Self {
        Self {
            role: ConnectRole::Watcher { task_id },
            privacy,
        }
    }

    pub const fn collaborator(task_id: TaskId, privacy: ConnectPrivacyClass) -> Self {
        Self {
            role: ConnectRole::Collaborator { task_id },
            privacy,
        }
    }
}

/// Maps a host request onto a local action before the host port is invoked.
pub fn action_for_client_request(request: &ClientRequest) -> Option<(ActionId, Option<TaskId>)> {
    match request {
        ClientRequest::TerminalInput(request) => {
            Some((ActionId::SEND_PROMPT, Some(request.context.task_id)))
        }
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
                Query::PromptLibrary(_) => ActionId::READ_PERSONAL_PROMPTS,
                Query::TaskCockpit(_) => ActionId::READ_TASK,
            };
            Some((action, envelope.task_id))
        }
        ClientRequest::Command(envelope) => {
            let action = match &envelope.command {
                Command::CreateTask(_)
                | Command::CreateTaskV2(_)
                | Command::RenameTask(_)
                | Command::SetTaskAttention(_)
                | Command::BeginCloseTask
                | Command::ReopenTask
                | Command::RegisterAgentSession { .. }
                | Command::SetPrimaryAgent { .. }
                | Command::RegisterArtifact { .. }
                | Command::RegisterResource { .. }
                | Command::ReleaseResource { .. }
                | Command::RequestSpecialist(_)
                | Command::PromotePrimary(_)
                | Command::CancelSpecialist(_)
                | Command::AcceptSpecialistHandoff(_)
                | Command::ServiceControl(_)
                | Command::PrepareUpdate(_)
                | Command::ConfirmUpdateDrain(_)
                | Command::AbortUpdateHandoff
                | Command::ArmUpdateInstall(_)
                | Command::ConfirmHostQuit(_) => ActionId::MUTATE_TASK,
                Command::SubmitProviderInput(_) => ActionId::SEND_PROMPT,
                Command::PromptLibrary(_) | Command::PromptChain(_) => {
                    ActionId::READ_PERSONAL_PROMPTS
                }
                Command::Browser(_) => ActionId::BROWSER_COMMAND,
                // These variants are journal ingress only. Keep them outside
                // the client action map so an authenticated client cannot
                // accidentally turn an internal fact into a host action.
                Command::PresentProviderQuestion(_)
                | Command::PresentProviderApproval(_)
                | Command::SettleProviderWait(_) => return None,
            };
            let task_id = match &envelope.command {
                Command::CreateTask(intent) => Some(intent.id),
                Command::CreateTaskV2(intent) => Some(intent.id),
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
        credential: None,
    }
}

/// Transport authorization for the organization extension. The host runtime
/// applies the enrolled membership/policy decision after this paired-session
/// gate; keeping the transport action here prevents an authenticated frame
/// from bypassing the normal Connect permission evaluator.
pub const fn organization_permission(mutating: bool) -> PermissionRequest {
    PermissionRequest {
        role: ConnectRole::PairedOwner,
        task_id: None,
        action: if mutating {
            ActionId::MUTATE_TASK
        } else {
            ActionId::READ_TASK
        },
        credential: None,
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
        credential: None,
    }
}

pub fn action_for_prompt_query(_call: &PromptQueryCall) -> PermissionRequest {
    PermissionRequest {
        role: ConnectRole::PairedOwner,
        task_id: None,
        action: ActionId::READ_PERSONAL_PROMPTS,
        credential: None,
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

    pub const fn watcher(task_id: TaskId) -> Self {
        Self::new(
            PermissionEvaluator::new(false),
            SessionPermissionContext::watcher(task_id, ConnectPrivacyClass::ManagedMetadata),
        )
    }

    pub const fn collaborator(task_id: TaskId) -> Self {
        Self::new(
            PermissionEvaluator::new(true),
            SessionPermissionContext::collaborator(task_id, ConnectPrivacyClass::ManagedMetadata),
        )
    }

    pub const fn context(self) -> SessionPermissionContext {
        self.context
    }

    pub fn authorize_request(&self, request: &ClientRequest) -> PermissionDecision {
        // Detach is the authenticated connection teardown handshake, not an
        // application action. Every other request must map to a known action;
        // an unmapped/new command is denied rather than treated as harmless.
        if matches!(request, ClientRequest::Detach(_)) {
            return PermissionDecision::Allow;
        }
        let Some((action, task_id)) = action_for_client_request(request) else {
            return PermissionDecision::Denied(PermissionDenyReason::UnknownAction);
        };
        // Network authentication alone never authorizes. Paired owners still
        // need a verified credential path; guests need a live scoped grant.
        self.evaluate(PermissionRequest {
            role: self.context.role,
            task_id,
            action,
            credential: None,
        })
    }

    /// Authorize a mapped client request only when a trusted authority supplies
    /// the live scoped grant for the current connection/session/route epochs.
    pub fn authorize_request_with_grant(
        &self,
        request: &ClientRequest,
        grant: &ScopedPermissionGrant,
        context: AuthoritativePermissionContext,
    ) -> PermissionDecision {
        if matches!(request, ClientRequest::Detach(_)) {
            return PermissionDecision::Allow;
        }
        let Some((action, task_id)) = action_for_client_request(request) else {
            return PermissionDecision::Denied(PermissionDenyReason::UnknownAction);
        };
        self.evaluator.evaluate_with_scoped_grant(
            PermissionRequest {
                role: self.context.role,
                task_id,
                action,
                credential: None,
            },
            grant,
            context,
        )
    }

    pub fn authorize(&self, request: PermissionRequest) -> PermissionDecision {
        self.evaluate(PermissionRequest {
            role: self.context.role,
            ..request
        })
    }

    pub fn authorize_with_grant(
        &self,
        request: PermissionRequest,
        grant: &ScopedPermissionGrant,
        context: AuthoritativePermissionContext,
    ) -> PermissionDecision {
        self.evaluator.evaluate_with_scoped_grant(
            PermissionRequest {
                role: self.context.role,
                ..request
            },
            grant,
            context,
        )
    }

    pub fn authorize_personal_prompts(&self) -> PermissionDecision {
        if !matches!(self.context.role, ConnectRole::PairedOwner) {
            return PermissionDecision::Denied(PermissionDenyReason::OwnerOnly);
        }
        self.evaluate(PermissionRequest {
            role: self.context.role,
            task_id: None,
            action: ActionId::READ_PERSONAL_PROMPTS,
            credential: None,
        })
    }

    fn evaluate(&self, request: PermissionRequest) -> PermissionDecision {
        self.evaluator.evaluate(request)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::command::CommandEnvelope;
    use crate::domain::command::{Command, ServiceControlAction, ServiceControlIntent};
    use crate::domain::id::{ClientId, CommandId};
    use crate::protocol::ClientRequest;

    #[test]
    fn service_control_maps_to_mutate_and_watcher_stays_read_only() {
        let task_id = TaskId::new();
        let authorizer = SessionAuthorizer::watcher(task_id);
        let request = ClientRequest::Command(CommandEnvelope {
            command_id: CommandId::new(),
            client_id: ClientId::new(),
            task_id: Some(task_id),
            issued_at_ms: 1,
            expected_task_revision: None,
            command: Command::ServiceControl(ServiceControlIntent {
                service_id: crate::domain::id::ConfiguredServiceId::new("demo")
                    .expect("valid configured service id"),
                resource_generation: 1,
                connection_epoch: 1,
                action_epoch: 1,
                action: ServiceControlAction::Stop,
            }),
        });
        assert_eq!(
            action_for_client_request(&request),
            Some((ActionId::MUTATE_TASK, Some(task_id)))
        );
        // Without a scoped grant the watcher path remains fail-closed.
        assert_eq!(
            authorizer.authorize_request(&request),
            PermissionDecision::Denied(PermissionDenyReason::ScopedGrantRequired)
        );
        let context = AuthoritativePermissionContext::live(1, 2, 3).unwrap();
        let grant = ScopedPermissionGrant::issue(
            ConnectRole::Watcher { task_id },
            task_id,
            ActionId::MUTATE_TASK,
            context,
        )
        .unwrap();
        assert_eq!(
            authorizer.authorize_request_with_grant(&request, &grant, context),
            PermissionDecision::Denied(PermissionDenyReason::WatcherReadOnly)
        );
    }

    #[test]
    fn unknown_unmapped_actions_deny() {
        let authorizer = SessionAuthorizer::paired_owner();
        assert_eq!(
            authorizer.authorize(PermissionRequest {
                role: ConnectRole::PairedOwner,
                task_id: None,
                action: ActionId::new(99).unwrap(),
                credential: None,
            }),
            PermissionDecision::Denied(PermissionDenyReason::UnknownAction)
        );
    }
}
