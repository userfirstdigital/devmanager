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
                Query::OperationStatus { .. } | Query::CommandReceiptStatus { .. } => {
                    ActionId::READ_OPERATION
                }
                Query::InspectHostQuit => ActionId::READ_OPERATION,
                Query::PromptLibrary(_) => ActionId::READ_PERSONAL_PROMPTS,
                Query::TaskCockpit(
                    crate::domain::cockpit::TaskCockpitQuery::ConfigCreateProject { .. }
                    | crate::domain::cockpit::TaskCockpitQuery::ConfigUpsertCommand { .. }
                    | crate::domain::cockpit::TaskCockpitQuery::ConfigArchiveCommand { .. }
                    | crate::domain::cockpit::TaskCockpitQuery::ConfigRunCommand { .. }
                    | crate::domain::cockpit::TaskCockpitQuery::ConfigCommandDetail { .. }
                    | crate::domain::cockpit::TaskCockpitQuery::ProviderSettings(_)
                    | crate::domain::cockpit::TaskCockpitQuery::RemoteAccess(_)
                    | crate::domain::cockpit::TaskCockpitQuery::OpenShellTerminal { .. },
                ) => {
                    // Config mutations, command-text detail, provider settings,
                    // and opening a shell stay host-local. Connect must not map
                    // them as READ_TASK: opening a shell spawns a process on the
                    // host under host-chosen authority, which is not a read.
                    return None;
                }
                // GitRepositories / targeted Git status+mutate remain Task
                // cockpit reads/mutations over READ_TASK; host fence owns paths.
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
                | Command::SettleTask
                | Command::BeginCloseTask
                | Command::ReopenTask
                | Command::DeleteTask
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
                | Command::StartProviderSession(_)
                | Command::PrepareUpdate(_)
                | Command::ConfirmUpdateDrain(_)
                | Command::AbortUpdateHandoff
                | Command::ArmUpdateInstall(_)
                | Command::ConfirmHostQuit(_)
                | Command::CloseTerminal { .. }
                | Command::RenameTerminal { .. }
                | Command::SetTerminalStrip(_) => ActionId::MUTATE_TASK,
                Command::SubmitProviderInput(_) => ActionId::SEND_PROMPT,
                Command::PromptLibrary(_) | Command::PromptChain(_) => {
                    ActionId::READ_PERSONAL_PROMPTS
                }
                Command::Browser(_) => ActionId::BROWSER_COMMAND,
                // These variants are journal ingress only. Keep them outside
                // the client action map so an authenticated client cannot
                // accidentally turn an internal fact into a host action.
                //
                // `OpenShellTerminal` joins them for the same reason its query
                // form does: opening a shell spawns a process on the host under
                // host-chosen authority, which is not a MUTATE_TASK the way a
                // rename is. The command form names its own program, args and
                // cwd, so a remote principal holding MUTATE_TASK must not be
                // able to send one; the host issues it itself.
                Command::BindProviderSession { .. }
                | Command::RebindUnstartedPrimaryProvider { .. }
                | Command::PresentProviderQuestion(_)
                | Command::PresentProviderApproval(_)
                | Command::OpenShellTerminal(_)
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
    use crate::domain::cockpit::TaskCockpitQuery;
    use crate::domain::command::CommandEnvelope;
    use crate::domain::command::{Command, ServiceControlAction, ServiceControlIntent};
    use crate::domain::id::{ClientId, CommandId, RequestId};
    use crate::domain::query::QueryEnvelope;
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
    fn config_create_project_is_not_a_connect_read() {
        let request = ClientRequest::Query(QueryEnvelope {
            request_id: RequestId::new(),
            client_id: ClientId::new(),
            task_id: None,
            query: Query::TaskCockpit(TaskCockpitQuery::ConfigCreateProject {
                name: "workspace".into(),
                root_path: "C:/workspace".into(),
            }),
        });
        assert_eq!(action_for_client_request(&request), None);
        assert_eq!(
            SessionAuthorizer::paired_owner().authorize_request(&request),
            PermissionDecision::Denied(PermissionDenyReason::UnknownAction)
        );
    }

    #[test]
    fn remote_listener_setup_is_never_a_connect_action() {
        use crate::host::remote_setup::{RemoteListenOptions, RemoteSetupRequest};
        for setup in [
            RemoteSetupRequest::Snapshot,
            RemoteSetupRequest::PairingInfo,
            RemoteSetupRequest::Disable {
                command_id: crate::domain::CommandId::new(),
            },
            RemoteSetupRequest::Retry {
                command_id: crate::domain::CommandId::new(),
            },
            RemoteSetupRequest::Enable {
                command_id: crate::domain::CommandId::new(),
                options: RemoteListenOptions {
                    bind_address: "127.0.0.1".into(),
                    port: 8443,
                    advertised_origin: None,
                    certificate_path: None,
                    private_key_path: None,
                },
            },
        ] {
            let request = ClientRequest::Query(QueryEnvelope {
                request_id: RequestId::new(),
                client_id: ClientId::new(),
                task_id: None,
                query: Query::TaskCockpit(TaskCockpitQuery::RemoteAccess(setup)),
            });
            assert_eq!(action_for_client_request(&request), None);
            assert_eq!(
                SessionAuthorizer::paired_owner().authorize_request(&request),
                PermissionDecision::Denied(PermissionDenyReason::UnknownAction)
            );
        }
    }

    #[test]
    fn provider_settings_is_not_a_connect_read() {
        use crate::providers::settings::ProviderSettingsHostRequest;
        for request in [
            ProviderSettingsHostRequest::Snapshot,
            ProviderSettingsHostRequest::Refresh { force: true },
            ProviderSettingsHostRequest::Refresh { force: false },
            ProviderSettingsHostRequest::Mutate(
                crate::providers::settings::ProviderSettingsMutation::SetHealthInterval {
                    expected_revision: 1,
                    interval_secs: 0,
                },
            ),
        ] {
            let client_request = ClientRequest::Query(QueryEnvelope {
                request_id: RequestId::new(),
                client_id: ClientId::new(),
                task_id: None,
                query: Query::TaskCockpit(TaskCockpitQuery::ProviderSettings(request)),
            });
            assert_eq!(action_for_client_request(&client_request), None);
            assert_eq!(
                SessionAuthorizer::paired_owner().authorize_request(&client_request),
                PermissionDecision::Denied(PermissionDenyReason::UnknownAction)
            );
        }
    }

    /// A remote principal holding MUTATE_TASK must not be able to open a shell.
    ///
    /// The command names its own program, args and cwd, and the kernel decider
    /// has no program allowlist, so mapping it as an ordinary task mutation
    /// would make "rename this task" and "run this binary" the same permission.
    #[test]
    fn open_shell_terminal_is_never_a_connect_mutation() {
        use crate::domain::command::OpenShellTerminalIntent;
        use crate::domain::resource::{
            OwnerKind, ResourceFacts, ResourceKind, ResourceLifecycle, ResourceRecipe,
            TerminalLaunch,
        };

        let task_id = TaskId::new();
        let request = ClientRequest::Command(CommandEnvelope {
            command_id: CommandId::new(),
            client_id: ClientId::new(),
            task_id: Some(task_id),
            issued_at_ms: 1,
            expected_task_revision: Some(1),
            command: Command::OpenShellTerminal(OpenShellTerminalIntent {
                resource: ResourceFacts {
                    id: crate::domain::id::ResourceId::new(),
                    task_id: Some(task_id),
                    owner_kind: OwnerKind::Task,
                    resource_kind: ResourceKind::Terminal,
                    recipe: ResourceRecipe::Terminal {
                        cols: 120,
                        rows: 40,
                        launch: Some(TerminalLaunch {
                            cwd: std::path::PathBuf::from(if cfg!(windows) {
                                "C:/code"
                            } else {
                                "/code"
                            }),
                            program: std::path::PathBuf::from("attacker.exe"),
                            args: Vec::new(),
                        }),
                        title: None,
                    },
                    lifecycle: ResourceLifecycle::Active,
                    runtime_generation: 1,
                    updated_at_ms: 1,
                },
            }),
        });
        assert_eq!(action_for_client_request(&request), None);
        assert_eq!(
            SessionAuthorizer::paired_owner().authorize_request(&request),
            PermissionDecision::Denied(PermissionDenyReason::UnknownAction)
        );

        // The query form the client is supposed to use is denied here too: the
        // host serves it locally and Connect never maps it at all.
        let query = ClientRequest::Query(QueryEnvelope {
            request_id: RequestId::new(),
            client_id: ClientId::new(),
            task_id: Some(task_id),
            query: Query::TaskCockpit(TaskCockpitQuery::OpenShellTerminal {
                cwd: None,
                expected_task_revision: 1,
            }),
        });
        assert_eq!(action_for_client_request(&query), None);
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
