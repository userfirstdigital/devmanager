//! Host dispatch for typed Task Cockpit queries.
//!
//! Workspace identity is resolved from the selected Task admission. Client
//! paths are never authoritative. Live Logs/Health use the ProcessManager
//! supervisor singleton. Git and files require a revalidated workspace
//! authorization plus an active resource lease.

use uuid::Uuid;

use crate::domain::cockpit::{
    cockpit_surface, relative_path_is_safe, workspace_projection, TaskCockpitDeniedReason,
    TaskCockpitQuery, TaskCockpitResult, TaskCockpitSurface, TaskCockpitUnavailableReason,
    TaskFileEntry, TaskFilesListProjection, TaskFilesReadProjection, TaskGitProjection,
    TaskSshEndpoint, TaskSshProjection, MAX_COCKPIT_FILE_LIST, MAX_COCKPIT_READ_BYTES,
};
use crate::domain::id::{ClientId, CommandId, RequestId, TaskId};
use crate::domain::query::{QueryError, QueryOutcome, QueryResult};
use crate::git::command::{issue_git_host_binding, GitCancellation, GitRepository};
use crate::git::model::StatusKind;
use crate::kernel::CommandBus;
use crate::protocol::CapabilitySet;
use crate::services::model::AdmissionFence;
use crate::services::supervisor::{SupervisorAction, SupervisorError, SupervisorOutcome, SupervisorRefusal};
use crate::services::ProcessManager;
use crate::ssh::{accept_exact_endpoint, ssh_runtime_outcome, SshEndpointDenial, SshRuntimeOutcome};
use crate::workspace::files::{
    ContentKind, EntryKind, FileServiceError, ReadOptions, SecretClassification, MAX_CHUNK_BYTES,
};
use crate::workspace::{
    issue_file_service, WorkspaceProjectRoots, WorkspaceResource, WorkspaceResourceCoordinator,
    WorkspaceService,
};

pub(crate) struct TaskCockpitDispatch<'a> {
    pub capabilities: CapabilitySet,
    pub envelope_task_id: Option<TaskId>,
    pub client_id: ClientId,
    pub connection_id: Uuid,
    pub request_id: RequestId,
    pub query: &'a TaskCockpitQuery,
    pub bus: &'a CommandBus,
    pub service_runtime: Option<&'a ProcessManager>,
    pub ssh_endpoints: Option<&'a [TaskSshEndpoint]>,
    pub workspace_projects: Option<&'a WorkspaceProjectRoots>,
    pub coordinator: Option<&'a WorkspaceResourceCoordinator>,
    pub action_epoch: Option<u64>,
    pub runtime_generation: Option<u64>,
}

pub(crate) fn serve_task_cockpit(dispatch: TaskCockpitDispatch<'_>) -> QueryOutcome {
    if !dispatch.capabilities.grants_task_cockpit() {
        return QueryOutcome::Err(QueryError::UnsupportedCapability);
    }
    let surface = cockpit_surface(dispatch.query);
    let Some(task_id) = dispatch.envelope_task_id else {
        return denied(surface, TaskCockpitDeniedReason::MissingTask);
    };
    let snapshot = match dispatch.bus.task_snapshot(task_id) {
        Ok(Some(snapshot)) => snapshot,
        Ok(None) => return denied(surface, TaskCockpitDeniedReason::MissingTask),
        Err(_) => {
            return QueryOutcome::Err(QueryError::Unavailable {
                reason: "task_lookup",
            })
        }
    };
    if snapshot.task.id != task_id {
        return denied(surface, TaskCockpitDeniedReason::Unauthorized);
    }

    match dispatch.query {
        TaskCockpitQuery::WorkspaceStatus => QueryOutcome::Ok(QueryResult::TaskCockpit(
            TaskCockpitResult::Workspace(workspace_projection(task_id, &snapshot.task.workspace)),
        )),
        TaskCockpitQuery::GitStatus => serve_git_status(&dispatch, task_id, &snapshot.task),
        TaskCockpitQuery::GitMutate => unavailable(
            TaskCockpitSurface::Git,
            TaskCockpitUnavailableReason::GitAuthorityNotIssued,
        ),
        TaskCockpitQuery::FilesList {
            relative_directory,
            limit,
        } => {
            if let Some(directory) = relative_directory {
                if !relative_path_is_safe(directory) {
                    return denied(surface, TaskCockpitDeniedReason::PathTraversal);
                }
            }
            if *limit == 0 || *limit > MAX_COCKPIT_FILE_LIST {
                return QueryOutcome::Err(QueryError::InvalidRequest);
            }
            serve_files_list(
                &dispatch,
                task_id,
                &snapshot.task,
                relative_directory.as_deref(),
                *limit,
            )
        }
        TaskCockpitQuery::FilesRead {
            relative_path,
            max_bytes,
        } => {
            if !relative_path_is_safe(relative_path) {
                return denied(surface, TaskCockpitDeniedReason::PathTraversal);
            }
            if *max_bytes == 0 || *max_bytes > MAX_COCKPIT_READ_BYTES {
                return QueryOutcome::Err(QueryError::InvalidRequest);
            }
            serve_files_read(
                &dispatch,
                task_id,
                &snapshot.task,
                relative_path,
                *max_bytes,
            )
        }
        TaskCockpitQuery::FilesWrite => unavailable(
            TaskCockpitSurface::Files,
            TaskCockpitUnavailableReason::WriteUnsupported,
        ),
        TaskCockpitQuery::SshStatus => match dispatch.ssh_endpoints {
            Some(endpoints) => QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Ssh(
                TaskSshProjection {
                    task_id,
                    endpoints: endpoints.to_vec(),
                },
            ))),
            None => unavailable(
                TaskCockpitSurface::Ssh,
                TaskCockpitUnavailableReason::SshOperationUnsupported,
            ),
        },
        TaskCockpitQuery::SshAction { endpoint_id } => {
            serve_ssh_action(&dispatch, task_id, &snapshot.task, endpoint_id)
        }
        TaskCockpitQuery::ServiceSnapshots => match dispatch.service_runtime {
            Some(manager) => match manager.configured_service_snapshots_for_task(task_id) {
                Ok(projection) => match crate::services::cockpit::to_wire_projection(projection) {
                    Ok(services) => {
                        QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Services(
                            services,
                        )))
                    }
                    Err(_) => unavailable(
                        TaskCockpitSurface::Services,
                        TaskCockpitUnavailableReason::ServiceSupervisorUnavailable,
                    ),
                },
                Err(_) => unavailable(
                    TaskCockpitSurface::Services,
                    TaskCockpitUnavailableReason::ServiceSupervisorUnavailable,
                ),
            },
            None => unavailable(
                TaskCockpitSurface::Services,
                TaskCockpitUnavailableReason::ServiceSupervisorUnavailable,
            ),
        },
        TaskCockpitQuery::ServiceLogs {
            service_id,
            resource_generation,
            connection_epoch,
            action_epoch,
        } => serve_service_observe(
            &dispatch,
            task_id,
            service_id,
            *resource_generation,
            *connection_epoch,
            *action_epoch,
            SupervisorAction::Logs,
        ),
        TaskCockpitQuery::ServiceHealth {
            service_id,
            resource_generation,
            connection_epoch,
            action_epoch,
        } => serve_service_observe(
            &dispatch,
            task_id,
            service_id,
            *resource_generation,
            *connection_epoch,
            *action_epoch,
            SupervisorAction::Health,
        ),
    }
}

fn serve_service_observe(
    dispatch: &TaskCockpitDispatch<'_>,
    task_id: TaskId,
    service_id: &crate::domain::id::ConfiguredServiceId,
    resource_generation: u64,
    connection_epoch: u64,
    action_epoch: u64,
    action: SupervisorAction,
) -> QueryOutcome {
    let Some(manager) = dispatch.service_runtime else {
        return unavailable(
            TaskCockpitSurface::Services,
            match action {
                SupervisorAction::Logs => TaskCockpitUnavailableReason::LogsUnsupported,
                SupervisorAction::Health => TaskCockpitUnavailableReason::HealthUnsupported,
                SupervisorAction::Start | SupervisorAction::Stop | SupervisorAction::Restart => {
                    TaskCockpitUnavailableReason::ServiceSupervisorUnavailable
                }
            },
        );
    };
    let supervisor_id = match crate::services::cockpit::supervisor_service_id(service_id) {
        Ok(id) => id,
        Err(_) => return denied(TaskCockpitSurface::Services, TaskCockpitDeniedReason::UnknownService),
    };
    let fence = AdmissionFence::new(resource_generation, connection_epoch, action_epoch);
    match manager.configured_service_observe_for_task(action, &supervisor_id, fence, task_id) {
        Ok(SupervisorOutcome::Logs(log)) => {
            match crate::services::cockpit::to_wire_logs(task_id, service_id.clone(), log) {
                Ok(logs) => QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::ServiceLogs(logs))),
                Err(_) => unavailable(
                    TaskCockpitSurface::Services,
                    TaskCockpitUnavailableReason::ServiceSupervisorUnavailable,
                ),
            }
        }
        Ok(SupervisorOutcome::Health(snapshot)) => {
            match crate::services::cockpit::to_wire_health(task_id, snapshot) {
                Ok(health) => {
                    QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::ServiceHealth(
                        health,
                    )))
                }
                Err(_) => unavailable(
                    TaskCockpitSurface::Services,
                    TaskCockpitUnavailableReason::ServiceSupervisorUnavailable,
                ),
            }
        }
        Ok(_) => unavailable(
            TaskCockpitSurface::Services,
            TaskCockpitUnavailableReason::ServiceSupervisorUnavailable,
        ),
        Err(error) => map_supervisor_observe_error(error),
    }
}

fn map_supervisor_observe_error(error: SupervisorError) -> QueryOutcome {
    match error {
        SupervisorError::UnknownService(_) => {
            denied(TaskCockpitSurface::Services, TaskCockpitDeniedReason::UnknownService)
        }
        SupervisorError::Refused(SupervisorRefusal::StaleFence) => {
            denied(TaskCockpitSurface::Services, TaskCockpitDeniedReason::StaleFence)
        }
        SupervisorError::Refused(SupervisorRefusal::Ownership) => {
            denied(TaskCockpitSurface::Services, TaskCockpitDeniedReason::ForeignScope)
        }
        SupervisorError::StaleGeneration { .. } => {
            denied(TaskCockpitSurface::Services, TaskCockpitDeniedReason::StaleFence)
        }
        _ => unavailable(
            TaskCockpitSurface::Services,
            TaskCockpitUnavailableReason::ServiceSupervisorUnavailable,
        ),
    }
}

fn serve_git_status(
    dispatch: &TaskCockpitDispatch<'_>,
    task_id: TaskId,
    task: &crate::domain::task::TaskFacts,
) -> QueryOutcome {
    let Some((service, authorization, command_id, action_epoch, runtime_generation)) =
        live_workspace_authority(dispatch, task_id, task)
    else {
        return unavailable(
            TaskCockpitSurface::Git,
            TaskCockpitUnavailableReason::WorkspaceAuthorityUnavailable,
        );
    };
    let lease = match service.acquire_task_resource(
        task_id,
        WorkspaceResource::Git,
        dispatch.client_id,
        dispatch.connection_id,
        dispatch.request_id,
        command_id,
        action_epoch,
        runtime_generation,
    ) {
        Ok(lease) => lease,
        Err(_) => {
            return unavailable(
                TaskCockpitSurface::Git,
                TaskCockpitUnavailableReason::GitAuthorityNotIssued,
            )
        }
    };
    let binding = match issue_git_host_binding(
        &authorization,
        lease,
        task_id,
        task.project_id,
        dispatch.client_id,
        dispatch.connection_id,
        dispatch.request_id,
        command_id,
        &task.workspace,
        action_epoch,
        runtime_generation,
    ) {
        Ok(binding) => binding,
        Err(_) => {
            return unavailable(
                TaskCockpitSurface::Git,
                TaskCockpitUnavailableReason::GitAuthorityNotIssued,
            )
        }
    };
    let repository = match GitRepository::from_host_binding(binding, GitCancellation::new()) {
        Ok(repository) => repository,
        Err(_) => {
            return unavailable(
                TaskCockpitSurface::Git,
                TaskCockpitUnavailableReason::GitAuthorityNotIssued,
            )
        }
    };
    match repository.status() {
        Ok(status) => QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Git(
            TaskGitProjection {
                task_id,
                branch: status.branch.as_ref().map(|name| name.as_str().to_owned()),
                ahead: status.ahead,
                behind: status.behind,
                change_count: status
                    .entries
                    .iter()
                    .filter(|entry| entry.kind != StatusKind::Unknown)
                    .count() as u32,
                detached: status.is_detached,
            },
        ))),
        Err(_) => unavailable(
            TaskCockpitSurface::Git,
            TaskCockpitUnavailableReason::GitAuthorityNotIssued,
        ),
    }
}

fn serve_files_list(
    dispatch: &TaskCockpitDispatch<'_>,
    task_id: TaskId,
    task: &crate::domain::task::TaskFacts,
    relative_directory: Option<&str>,
    limit: u16,
) -> QueryOutcome {
    let files = match live_file_service(dispatch, task_id, task) {
        Ok(files) => files,
        Err(outcome) => return outcome,
    };
    match files.list(relative_directory, usize::from(limit)) {
        Ok(entries) => {
            let truncated = entries.len() >= usize::from(limit);
            let entries = entries
                .into_iter()
                .take(usize::from(limit))
                .filter_map(|entry| {
                    if !relative_path_is_safe(entry.path.as_str()) {
                        return None;
                    }
                    Some(TaskFileEntry {
                        relative_path: entry.path.as_str().to_owned(),
                        is_directory: entry.kind == EntryKind::Directory,
                        secret: entry.secret == SecretClassification::SecretLike,
                    })
                })
                .collect();
            QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::FilesList(
                TaskFilesListProjection {
                    task_id,
                    entries,
                    truncated,
                },
            )))
        }
        Err(error) => map_file_error(error),
    }
}

fn serve_files_read(
    dispatch: &TaskCockpitDispatch<'_>,
    task_id: TaskId,
    task: &crate::domain::task::TaskFacts,
    relative_path: &str,
    max_bytes: u32,
) -> QueryOutcome {
    let files = match live_file_service(dispatch, task_id, task) {
        Ok(files) => files,
        Err(outcome) => return outcome,
    };
    let options = ReadOptions {
        chunk_bytes: (max_bytes as usize).min(MAX_CHUNK_BYTES).max(1),
        total_bytes: max_bytes as usize,
    };
    if crate::workspace::files::WorkspaceFileService::classify_secret_path(relative_path)
        == SecretClassification::SecretLike
    {
        return denied(
            TaskCockpitSurface::Files,
            TaskCockpitDeniedReason::CapabilityDenied,
        );
    }
    match files.read(relative_path, options) {
        Ok(result) => {
            if !relative_path_is_safe(result.path.as_str()) {
                return denied(
                    TaskCockpitSurface::Files,
                    TaskCockpitDeniedReason::PathTraversal,
                );
            }
            if crate::workspace::files::WorkspaceFileService::classify_secret_path(
                result.path.as_str(),
            ) == SecretClassification::SecretLike
            {
                return denied(
                    TaskCockpitSurface::Files,
                    TaskCockpitDeniedReason::CapabilityDenied,
                );
            }
            let mut collected = Vec::new();
            for chunk in &result.chunks {
                let remaining = (max_bytes as usize).saturating_sub(collected.len());
                if remaining == 0 {
                    break;
                }
                collected.extend_from_slice(&chunk.bytes[..chunk.bytes.len().min(remaining)]);
            }
            let utf8_prefix = match result.content_kind {
                ContentKind::Text => {
                    crate::workspace::files::WorkspaceFileService::bounded_utf8_prefix(
                        &collected,
                        max_bytes as usize,
                    )
                }
                ContentKind::Binary => None,
            };
            QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::FilesRead(
                TaskFilesReadProjection {
                    task_id,
                    relative_path: result.path.as_str().to_owned(),
                    utf8_prefix,
                    byte_len: result.total_bytes.min(u64::from(u32::MAX)) as u32,
                    secret: false,
                },
            )))
        }
        Err(error) => map_file_error(error),
    }
}

fn live_file_service(
    dispatch: &TaskCockpitDispatch<'_>,
    task_id: TaskId,
    task: &crate::domain::task::TaskFacts,
) -> Result<crate::workspace::files::WorkspaceFileService, QueryOutcome> {
    let Some((service, authorization, command_id, action_epoch, runtime_generation)) =
        live_workspace_authority(dispatch, task_id, task)
    else {
        return Err(unavailable(
            TaskCockpitSurface::Files,
            TaskCockpitUnavailableReason::WorkspaceAuthorityUnavailable,
        ));
    };
    let lease = service
        .acquire_task_resource(
            task_id,
            WorkspaceResource::File,
            dispatch.client_id,
            dispatch.connection_id,
            dispatch.request_id,
            command_id,
            action_epoch,
            runtime_generation,
        )
        .map_err(|_| {
            unavailable(
                TaskCockpitSurface::Files,
                TaskCockpitUnavailableReason::FileAuthorityNotIssued,
            )
        })?;
    issue_file_service(
        &authorization,
        lease,
        task_id,
        task.project_id,
        dispatch.client_id,
        dispatch.connection_id,
        dispatch.request_id,
        command_id,
        &task.workspace,
        action_epoch,
        runtime_generation,
    )
    .map_err(|_| {
        unavailable(
            TaskCockpitSurface::Files,
            TaskCockpitUnavailableReason::FileAuthorityNotIssued,
        )
    })
}

fn live_workspace_authority(
    dispatch: &TaskCockpitDispatch<'_>,
    task_id: TaskId,
    task: &crate::domain::task::TaskFacts,
) -> Option<(
    WorkspaceService,
    crate::workspace::WorkspaceAuthorization,
    CommandId,
    u64,
    u64,
)> {
    let projects = dispatch.workspace_projects?;
    let coordinator = dispatch.coordinator?;
    let action_epoch = dispatch.action_epoch?;
    let runtime_generation = dispatch.runtime_generation?;
    let command_id = CommandId::from_bytes(*dispatch.request_id.as_bytes()).ok()?;
    let service = WorkspaceService::from_durable_with_task_coordinator(
        task.project_id,
        task_id,
        projects,
        &task.workspace,
        coordinator.clone(),
    )
    .ok()?;
    let authorization = service
        .authorize_current_with_generation(
            &task.workspace,
            task_id,
            dispatch.client_id,
            dispatch.connection_id,
            dispatch.request_id,
            command_id,
            action_epoch,
            runtime_generation,
        )
        .ok()?;
    Some((
        service,
        authorization,
        command_id,
        action_epoch,
        runtime_generation,
    ))
}

fn serve_ssh_action(
    dispatch: &TaskCockpitDispatch<'_>,
    task_id: TaskId,
    task: &crate::domain::task::TaskFacts,
    endpoint_id: &str,
) -> QueryOutcome {
    let Some(endpoints) = dispatch.ssh_endpoints else {
        return unavailable(
            TaskCockpitSurface::Ssh,
            TaskCockpitUnavailableReason::SshOperationUnsupported,
        );
    };
    match accept_exact_endpoint(endpoints, endpoint_id) {
        Ok(_) => {}
        Err(SshEndpointDenial::ForeignInput) | Err(SshEndpointDenial::UnknownEndpoint) => {
            return denied(TaskCockpitSurface::Ssh, TaskCockpitDeniedReason::Unauthorized);
        }
    }
    if dispatch.envelope_task_id != Some(task_id) {
        return denied(TaskCockpitSurface::Ssh, TaskCockpitDeniedReason::Unauthorized);
    }
    if live_workspace_authority(dispatch, task_id, task).is_none() {
        return unavailable(
            TaskCockpitSurface::Ssh,
            TaskCockpitUnavailableReason::WorkspaceAuthorityUnavailable,
        );
    }
    match ssh_runtime_outcome() {
        SshRuntimeOutcome::Unavailable {
            reason: crate::ssh::SshUnavailableReason::TaskSupervisorAdapterMissing,
        } => unavailable(
            TaskCockpitSurface::Ssh,
            TaskCockpitUnavailableReason::SshTaskSupervisorAdapterMissing,
        ),
    }
}

fn map_file_error(error: FileServiceError) -> QueryOutcome {
    match error {
        FileServiceError::InvalidPath { .. }
        | FileServiceError::ReparseRejected { .. }
        | FileServiceError::HardLinkRejected { .. } => {
            denied(TaskCockpitSurface::Files, TaskCockpitDeniedReason::PathTraversal)
        }
        FileServiceError::SecretLikePath => {
            denied(TaskCockpitSurface::Files, TaskCockpitDeniedReason::CapabilityDenied)
        }
        FileServiceError::OutsideWorkspace { .. } => {
            denied(TaskCockpitSurface::Files, TaskCockpitDeniedReason::OutsideWorkspace)
        }
        FileServiceError::AuthorityUnavailable | FileServiceError::RootUnavailable => unavailable(
            TaskCockpitSurface::Files,
            TaskCockpitUnavailableReason::FileAuthorityNotIssued,
        ),
        _ => unavailable(
            TaskCockpitSurface::Files,
            TaskCockpitUnavailableReason::FileAuthorityNotIssued,
        ),
    }
}

fn denied(surface: TaskCockpitSurface, reason: TaskCockpitDeniedReason) -> QueryOutcome {
    QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Denied {
        surface,
        reason,
    }))
}

fn unavailable(surface: TaskCockpitSurface, reason: TaskCockpitUnavailableReason) -> QueryOutcome {
    QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Unavailable {
        surface,
        reason,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::command::{Command, CommandEnvelope, CreateTaskRequestIntent};
    use crate::domain::task::{
        ReviewReadiness, TaskActivity, TaskAssignment, TaskAttention, TaskConnectivity,
    };
    use crate::domain::{CommandId, EnvironmentId, ProjectId};
    use crate::protocol::Capability;
    use crate::workspace::{WorkspaceRequest, WorkspaceResourceCoordinator};
    use std::fs;

    fn create_bound_task() -> (
        tempfile::TempDir,
        CommandBus,
        ClientId,
        TaskId,
        WorkspaceProjectRoots,
    ) {
        let repository = tempfile::tempdir().expect("repo");
        let output = std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(repository.path())
            .output()
            .expect("git init");
        assert!(output.status.success());
        fs::write(repository.path().join("README.md"), "hello-aé\n").expect("readme");
        fs::write(repository.path().join("blob.bin"), [0u8, 1, 2, 255]).expect("binary");
        fs::write(repository.path().join(".env"), "SECRET=1\n").expect("env");
        let database = repository.path().join("cockpit.sqlite");
        let mut bus = CommandBus::open(&database).expect("bus");
        let client_id = ClientId::new();
        let task_id = TaskId::new();
        let project_id = ProjectId::new();
        let roots = WorkspaceProjectRoots::try_from_pairs([(
            project_id,
            repository.path().to_path_buf(),
        )])
        .expect("roots");
        let create = CommandEnvelope {
            command_id: CommandId::new(),
            client_id,
            task_id: None,
            issued_at_ms: 1,
            expected_task_revision: None,
            command: Command::CreateTaskV2(CreateTaskRequestIntent {
                id: task_id,
                environment_id: EnvironmentId::new(),
                title: "Cockpit".into(),
                description: None,
                project_id,
                workspace: WorkspaceRequest::main(),
                assignment: TaskAssignment::LocalOwner,
                created_at_ms: 1,
                connectivity: TaskConnectivity::Connected,
                attention: TaskAttention::None,
                activity: TaskActivity::Idle,
                review_readiness: ReviewReadiness::NotReady,
            }),
        };
        crate::host::connection::dispatch_authenticated_request_with_workspace_projects(
            client_id,
            CapabilitySet::empty(),
            &mut bus,
            &roots,
            crate::protocol::ClientRequest::Command(create),
        )
        .expect("create");
        (repository, bus, client_id, task_id, roots)
    }

    #[test]
    fn ssh_action_requires_task_authority_before_missing_adapter() {
        let (_repository, bus, client_id, task_id, roots) = create_bound_task();
        let endpoints = [TaskSshEndpoint {
            id: "jump".into(),
            label: "Jump".into(),
            archived: false,
            has_credential: true,
        }];
        let granted = CapabilitySet::from_capabilities([Capability::TaskCockpit]);
        let accepted = serve_task_cockpit(TaskCockpitDispatch {
            capabilities: granted,
            envelope_task_id: Some(task_id),
            client_id,
            connection_id: Uuid::now_v7(),
            request_id: RequestId::new(),
            query: &TaskCockpitQuery::SshAction {
                endpoint_id: "jump".into(),
            },
            bus: &bus,
            service_runtime: None,
            ssh_endpoints: Some(&endpoints),
            workspace_projects: Some(&roots),
            coordinator: None,
            action_epoch: None,
            runtime_generation: None,
        });
        assert!(matches!(
            accepted,
            QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Unavailable {
                surface: TaskCockpitSurface::Ssh,
                reason: TaskCockpitUnavailableReason::WorkspaceAuthorityUnavailable,
            }))
        ));
        let coordinator = WorkspaceResourceCoordinator::new();
        let named = serve_task_cockpit(TaskCockpitDispatch {
            capabilities: granted,
            envelope_task_id: Some(task_id),
            client_id,
            connection_id: Uuid::now_v7(),
            request_id: RequestId::new(),
            query: &TaskCockpitQuery::SshAction {
                endpoint_id: "jump".into(),
            },
            bus: &bus,
            service_runtime: None,
            ssh_endpoints: Some(&endpoints),
            workspace_projects: Some(&roots),
            coordinator: Some(&coordinator),
            action_epoch: Some(1),
            runtime_generation: Some(1),
        });
        assert!(matches!(
            named,
            QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Unavailable {
                surface: TaskCockpitSurface::Ssh,
                reason: TaskCockpitUnavailableReason::SshTaskSupervisorAdapterMissing,
            }))
        ));
        let foreign = serve_task_cockpit(TaskCockpitDispatch {
            capabilities: granted,
            envelope_task_id: Some(task_id),
            client_id,
            connection_id: Uuid::now_v7(),
            request_id: RequestId::new(),
            query: &TaskCockpitQuery::SshAction {
                endpoint_id: "deploy@secret.example".into(),
            },
            bus: &bus,
            service_runtime: None,
            ssh_endpoints: Some(&endpoints),
            workspace_projects: Some(&roots),
            coordinator: None,
            action_epoch: None,
            runtime_generation: None,
        });
        assert!(matches!(
            foreign,
            QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Denied {
                surface: TaskCockpitSurface::Ssh,
                reason: TaskCockpitDeniedReason::Unauthorized,
            }))
        ));
    }

    #[test]
    fn files_read_returns_text_omits_binary_and_denies_secret_paths() {
        let (_repository, bus, client_id, task_id, roots) = create_bound_task();
        let granted = CapabilitySet::from_capabilities([Capability::TaskCockpit]);
        let coordinator = WorkspaceResourceCoordinator::new();
        let text = serve_task_cockpit(TaskCockpitDispatch {
            capabilities: granted,
            envelope_task_id: Some(task_id),
            client_id,
            connection_id: Uuid::now_v7(),
            request_id: RequestId::new(),
            query: &TaskCockpitQuery::FilesRead {
                relative_path: "README.md".into(),
                max_bytes: 32,
            },
            bus: &bus,
            service_runtime: None,
            ssh_endpoints: None,
            workspace_projects: Some(&roots),
            coordinator: Some(&coordinator),
            action_epoch: Some(1),
            runtime_generation: Some(1),
        });
        let QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::FilesRead(text))) = text
        else {
            panic!("expected text read, got {text:?}");
        };
        assert_eq!(text.secret, false);
        assert!(text.utf8_prefix.as_deref().unwrap_or("").contains("hello"));
        let binary = serve_task_cockpit(TaskCockpitDispatch {
            capabilities: granted,
            envelope_task_id: Some(task_id),
            client_id,
            connection_id: Uuid::now_v7(),
            request_id: RequestId::new(),
            query: &TaskCockpitQuery::FilesRead {
                relative_path: "blob.bin".into(),
                max_bytes: 32,
            },
            bus: &bus,
            service_runtime: None,
            ssh_endpoints: None,
            workspace_projects: Some(&roots),
            coordinator: Some(&coordinator),
            action_epoch: Some(1),
            runtime_generation: Some(1),
        });
        let QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::FilesRead(binary))) =
            binary
        else {
            panic!("expected binary read, got {binary:?}");
        };
        assert!(binary.utf8_prefix.is_none());
        assert_eq!(binary.secret, false);
        let secret = serve_task_cockpit(TaskCockpitDispatch {
            capabilities: granted,
            envelope_task_id: Some(task_id),
            client_id,
            connection_id: Uuid::now_v7(),
            request_id: RequestId::new(),
            query: &TaskCockpitQuery::FilesRead {
                relative_path: ".env".into(),
                max_bytes: 32,
            },
            bus: &bus,
            service_runtime: None,
            ssh_endpoints: None,
            workspace_projects: Some(&roots),
            coordinator: Some(&coordinator),
            action_epoch: Some(1),
            runtime_generation: Some(1),
        });
        assert!(matches!(
            secret,
            QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Denied {
                surface: TaskCockpitSurface::Files,
                reason: TaskCockpitDeniedReason::CapabilityDenied,
            }))
        ));
    }
}
