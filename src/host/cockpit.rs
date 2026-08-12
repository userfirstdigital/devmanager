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
    TaskFileEntry, TaskFilesListProjection, TaskFilesReadProjection, TaskGitMutateIntent,
    TaskGitProjection, TaskSshEndpoint, TaskSshProjection, MAX_COCKPIT_FILE_LIST,
    MAX_COCKPIT_READ_BYTES,
};
use crate::domain::id::{ClientId, CommandId, RequestId, TaskId};
use crate::domain::query::{QueryError, QueryOutcome, QueryResult};
use crate::git::command::{
    issue_git_host_binding, GitCancellation, GitConfirmation, GitError, GitRepository,
};
use crate::git::model::{MutationPlan, RepoPath, StatusKind};
use crate::kernel::CommandBus;
use crate::protocol::CapabilitySet;
use crate::services::model::AdmissionFence;
use crate::services::supervisor::{
    SupervisorAction, SupervisorError, SupervisorOutcome, SupervisorRefusal,
};
use crate::services::ProcessManager;
use crate::ssh::{
    accept_exact_endpoint, ssh_runtime_outcome, SshEndpointDenial, SshRuntimeOutcome,
};
use crate::workspace::files::{
    ContentKind, EntryKind, ExpectedRevision, FileServiceError, ReadOptions, SecretClassification,
    MAX_CHUNK_BYTES,
};
use crate::workspace::worktree::{revalidate_cockpit_workspace_action, WorkspaceActionContext};
use crate::workspace::{
    issue_file_service, WorkspaceAuthorization, WorkspaceProjectRoots, WorkspaceResource,
    WorkspaceResourceCoordinator, WorkspaceResourceLease, WorkspaceService,
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
        TaskCockpitQuery::GitMutate { intent, confirm } => {
            serve_git_mutate(&dispatch, task_id, &snapshot.task, intent, *confirm)
        }
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
        TaskCockpitQuery::FilesWrite {
            relative_path,
            utf8_contents,
            expected_sha256_hex,
            confirm,
        } => {
            if !relative_path_is_safe(relative_path) {
                return denied(surface, TaskCockpitDeniedReason::PathTraversal);
            }
            if utf8_contents.len() > MAX_COCKPIT_READ_BYTES as usize {
                return QueryOutcome::Err(QueryError::InvalidRequest);
            }
            serve_files_write(
                &dispatch,
                task_id,
                &snapshot.task,
                relative_path,
                utf8_contents,
                expected_sha256_hex.as_deref(),
                *confirm,
            )
        }
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
                    Ok(services) => QueryOutcome::Ok(QueryResult::TaskCockpit(
                        TaskCockpitResult::Services(services),
                    )),
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
        Err(_) => {
            return denied(
                TaskCockpitSurface::Services,
                TaskCockpitDeniedReason::UnknownService,
            )
        }
    };
    let fence = AdmissionFence::new(resource_generation, connection_epoch, action_epoch);
    match manager.configured_service_observe_for_task(action, &supervisor_id, fence, task_id) {
        Ok(SupervisorOutcome::Logs(log)) => {
            match crate::services::cockpit::to_wire_logs(task_id, service_id.clone(), log) {
                Ok(logs) => QueryOutcome::Ok(QueryResult::TaskCockpit(
                    TaskCockpitResult::ServiceLogs(logs),
                )),
                Err(_) => unavailable(
                    TaskCockpitSurface::Services,
                    TaskCockpitUnavailableReason::ServiceSupervisorUnavailable,
                ),
            }
        }
        Ok(SupervisorOutcome::Health(snapshot)) => {
            match crate::services::cockpit::to_wire_health(task_id, snapshot) {
                Ok(health) => QueryOutcome::Ok(QueryResult::TaskCockpit(
                    TaskCockpitResult::ServiceHealth(health),
                )),
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
        SupervisorError::UnknownService(_) => denied(
            TaskCockpitSurface::Services,
            TaskCockpitDeniedReason::UnknownService,
        ),
        SupervisorError::Refused(SupervisorRefusal::StaleFence) => denied(
            TaskCockpitSurface::Services,
            TaskCockpitDeniedReason::StaleFence,
        ),
        SupervisorError::Refused(SupervisorRefusal::Ownership) => denied(
            TaskCockpitSurface::Services,
            TaskCockpitDeniedReason::ForeignScope,
        ),
        SupervisorError::StaleGeneration { .. } => denied(
            TaskCockpitSurface::Services,
            TaskCockpitDeniedReason::StaleFence,
        ),
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
    match open_git_repository(dispatch, task_id, task) {
        Ok(repository) => git_status_outcome(task_id, &repository),
        Err(outcome) => outcome,
    }
}

fn serve_git_mutate(
    dispatch: &TaskCockpitDispatch<'_>,
    task_id: TaskId,
    task: &crate::domain::task::TaskFacts,
    intent: &TaskGitMutateIntent,
    confirm: bool,
) -> QueryOutcome {
    if dispatch.envelope_task_id != Some(task_id) {
        return denied(
            TaskCockpitSurface::Git,
            TaskCockpitDeniedReason::Unauthorized,
        );
    }
    let paths = match intent {
        TaskGitMutateIntent::Stage { relative_paths }
        | TaskGitMutateIntent::Unstage { relative_paths } => {
            match cockpit_repo_paths(relative_paths) {
                Ok(paths) => Some(paths),
                Err(outcome) => return outcome,
            }
        }
        TaskGitMutateIntent::Commit { message } => {
            if message.trim().is_empty() || message.len() > MAX_COCKPIT_READ_BYTES as usize {
                return QueryOutcome::Err(QueryError::InvalidRequest);
            }
            None
        }
    };
    let repository = match open_git_repository(dispatch, task_id, task) {
        Ok(repository) => repository,
        Err(outcome) => return outcome,
    };
    if let Err(outcome) = revalidate_git_fence(dispatch, task_id, task) {
        return outcome;
    }
    let planned = match intent {
        TaskGitMutateIntent::Stage { .. } => {
            let paths = paths.expect("stage paths");
            match repository.plan_stage(&paths) {
                Ok(plan) => GitPlannedMutation::Stage(plan),
                Err(error) => return map_git_error(error),
            }
        }
        TaskGitMutateIntent::Unstage { .. } => {
            let paths = paths.expect("unstage paths");
            match repository.plan_unstage(&paths) {
                Ok(plan) => GitPlannedMutation::Unstage(plan),
                Err(error) => return map_git_error(error),
            }
        }
        TaskGitMutateIntent::Commit { message } => match repository.plan_commit(message) {
            Ok(plan) => GitPlannedMutation::Commit(plan),
            Err(error) => return map_git_error(error),
        },
    };
    if !confirm {
        return git_status_outcome(task_id, &repository);
    }
    if let Err(outcome) = revalidate_git_fence(dispatch, task_id, task) {
        return outcome;
    }
    let executed = match &planned {
        GitPlannedMutation::Stage(plan) => confirm_git_mutation(&repository, plan)
            .and_then(|confirmation| repository.stage(plan, &confirmation)),
        GitPlannedMutation::Unstage(plan) => confirm_git_mutation(&repository, plan)
            .and_then(|confirmation| repository.unstage(plan, &confirmation)),
        GitPlannedMutation::Commit(plan) => confirm_git_mutation(&repository, plan)
            .and_then(|confirmation| repository.commit(plan, &confirmation)),
    };
    match executed {
        Ok(()) => git_status_outcome(task_id, &repository),
        Err(error) => map_git_error(error),
    }
}

enum GitPlannedMutation {
    Stage(crate::git::model::StagePlan),
    Unstage(crate::git::model::UnstagePlan),
    Commit(crate::git::model::CommitPlan),
}

fn cockpit_repo_paths(paths: &[String]) -> Result<Vec<RepoPath>, QueryOutcome> {
    if paths.is_empty() || paths.len() > usize::from(MAX_COCKPIT_FILE_LIST) {
        return Err(QueryOutcome::Err(QueryError::InvalidRequest));
    }
    let mut converted = Vec::with_capacity(paths.len());
    for path in paths {
        if !relative_path_is_safe(path) {
            return Err(denied(
                TaskCockpitSurface::Git,
                TaskCockpitDeniedReason::PathTraversal,
            ));
        }
        if crate::workspace::files::WorkspaceFileService::classify_secret_path(path)
            == SecretClassification::SecretLike
        {
            return Err(denied(
                TaskCockpitSurface::Git,
                TaskCockpitDeniedReason::CapabilityDenied,
            ));
        }
        converted.push(RepoPath::from_bytes(path.as_bytes().to_vec()));
    }
    Ok(converted)
}

fn confirm_git_mutation<P: MutationPlan>(
    repository: &GitRepository,
    plan: &P,
) -> Result<GitConfirmation, GitError> {
    repository.host_confirm(plan)
}

fn map_git_error(error: GitError) -> QueryOutcome {
    match error {
        GitError::InvalidPath { .. } => denied(
            TaskCockpitSurface::Git,
            TaskCockpitDeniedReason::PathTraversal,
        ),
        GitError::InvalidRequest { .. } => QueryOutcome::Err(QueryError::InvalidRequest),
        GitError::CapabilityDenied { .. } => denied(
            TaskCockpitSurface::Git,
            TaskCockpitDeniedReason::CapabilityDenied,
        ),
        GitError::FingerprintMismatch { .. } | GitError::ConfirmationMismatch { .. } => denied(
            TaskCockpitSurface::Git,
            TaskCockpitDeniedReason::RevisionConflict,
        ),
        GitError::WorkspaceMismatch { .. } => denied(
            TaskCockpitSurface::Git,
            TaskCockpitDeniedReason::Unauthorized,
        ),
        GitError::AuthorityUnavailable => unavailable(
            TaskCockpitSurface::Git,
            TaskCockpitUnavailableReason::GitAuthorityNotIssued,
        ),
        _ => unavailable(
            TaskCockpitSurface::Git,
            TaskCockpitUnavailableReason::GitAuthorityNotIssued,
        ),
    }
}

fn git_status_outcome(task_id: TaskId, repository: &GitRepository) -> QueryOutcome {
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

fn open_git_repository(
    dispatch: &TaskCockpitDispatch<'_>,
    task_id: TaskId,
    task: &crate::domain::task::TaskFacts,
) -> Result<GitRepository, QueryOutcome> {
    let (service, authorization, command_id, action_epoch, runtime_generation) =
        live_mutation_authority(dispatch, task_id, task, TaskCockpitSurface::Git)?;
    let lease = service
        .acquire_task_resource(
            task_id,
            WorkspaceResource::Git,
            dispatch.client_id,
            dispatch.connection_id,
            dispatch.request_id,
            command_id,
            action_epoch,
            runtime_generation,
        )
        .map_err(|_| {
            unavailable(
                TaskCockpitSurface::Git,
                TaskCockpitUnavailableReason::GitAuthorityNotIssued,
            )
        })?;
    revalidate_issued_fence(
        dispatch,
        task_id,
        task,
        &authorization,
        &lease,
        command_id,
        action_epoch,
        runtime_generation,
        WorkspaceResource::Git,
        TaskCockpitSurface::Git,
    )?;
    let binding = issue_git_host_binding(
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
            TaskCockpitSurface::Git,
            TaskCockpitUnavailableReason::GitAuthorityNotIssued,
        )
    })?;
    GitRepository::from_host_binding(binding, GitCancellation::new()).map_err(|_| {
        unavailable(
            TaskCockpitSurface::Git,
            TaskCockpitUnavailableReason::GitAuthorityNotIssued,
        )
    })
}

fn revalidate_git_fence(
    dispatch: &TaskCockpitDispatch<'_>,
    task_id: TaskId,
    task: &crate::domain::task::TaskFacts,
) -> Result<(), QueryOutcome> {
    remint_and_check_authorization(dispatch, task_id, task, TaskCockpitSurface::Git)
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

fn serve_files_write(
    dispatch: &TaskCockpitDispatch<'_>,
    task_id: TaskId,
    task: &crate::domain::task::TaskFacts,
    relative_path: &str,
    utf8_contents: &str,
    expected_sha256_hex: Option<&str>,
    confirm: bool,
) -> QueryOutcome {
    if dispatch.envelope_task_id != Some(task_id) {
        return denied(
            TaskCockpitSurface::Files,
            TaskCockpitDeniedReason::Unauthorized,
        );
    }
    if crate::workspace::files::WorkspaceFileService::classify_secret_path(relative_path)
        == SecretClassification::SecretLike
    {
        return denied(
            TaskCockpitSurface::Files,
            TaskCockpitDeniedReason::CapabilityDenied,
        );
    }
    if !confirm {
        return unavailable(
            TaskCockpitSurface::Files,
            TaskCockpitUnavailableReason::WriteUnsupported,
        );
    }
    let files = match live_file_service(dispatch, task_id, task) {
        Ok(files) => files,
        Err(outcome) => return outcome,
    };
    if let Err(outcome) =
        remint_and_check_authorization(dispatch, task_id, task, TaskCockpitSurface::Files)
    {
        return outcome;
    }
    let expected = match expected_revision(relative_path, expected_sha256_hex, &files) {
        Ok(expected) => expected,
        Err(outcome) => return outcome,
    };
    if let Err(outcome) =
        remint_and_check_authorization(dispatch, task_id, task, TaskCockpitSurface::Files)
    {
        return outcome;
    }
    let plan = match files.plan_write(relative_path, utf8_contents.as_bytes().to_vec(), expected) {
        Ok(plan) => plan,
        Err(error) => return map_file_error(error),
    };
    if let Err(outcome) =
        remint_and_check_authorization(dispatch, task_id, task, TaskCockpitSurface::Files)
    {
        return outcome;
    }
    match files.execute_write(plan) {
        Ok(written) => {
            if !relative_path_is_safe(written.path.as_str()) {
                return denied(
                    TaskCockpitSurface::Files,
                    TaskCockpitDeniedReason::PathTraversal,
                );
            }
            QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::FilesRead(
                TaskFilesReadProjection {
                    task_id,
                    relative_path: written.path.as_str().to_owned(),
                    utf8_prefix: Some(crate::domain::cockpit::truncate_to_max_bytes(
                        utf8_contents,
                        MAX_COCKPIT_READ_BYTES as usize,
                    )),
                    byte_len: written.bytes_written.min(u32::MAX as usize) as u32,
                    secret: false,
                },
            )))
        }
        Err(error) => map_file_error(error),
    }
}

fn expected_revision(
    relative_path: &str,
    expected_sha256_hex: Option<&str>,
    files: &crate::workspace::files::WorkspaceFileService,
) -> Result<ExpectedRevision, QueryOutcome> {
    let Some(hex) = expected_sha256_hex else {
        return Ok(ExpectedRevision::missing());
    };
    let expected_hash =
        parse_sha256_hex(hex).ok_or_else(|| QueryOutcome::Err(QueryError::InvalidRequest))?;
    match files.current_revision(relative_path) {
        Ok(revision) if revision.sha256 == Some(expected_hash) => {
            Ok(ExpectedRevision::exact(revision))
        }
        Ok(_) => Err(denied(
            TaskCockpitSurface::Files,
            TaskCockpitDeniedReason::RevisionConflict,
        )),
        Err(FileServiceError::NotFound { .. }) => Err(denied(
            TaskCockpitSurface::Files,
            TaskCockpitDeniedReason::RevisionConflict,
        )),
        Err(error) => Err(map_file_error(error)),
    }
}

fn parse_sha256_hex(value: &str) -> Option<[u8; 32]> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let mut digest = [0u8; 32];
    for (index, chunk) in value.as_bytes().chunks_exact(2).enumerate() {
        digest[index] = u8::from_str_radix(std::str::from_utf8(chunk).ok()?, 16).ok()?;
    }
    Some(digest)
}

fn live_file_service(
    dispatch: &TaskCockpitDispatch<'_>,
    task_id: TaskId,
    task: &crate::domain::task::TaskFacts,
) -> Result<crate::workspace::files::WorkspaceFileService, QueryOutcome> {
    let (service, authorization, command_id, action_epoch, runtime_generation) =
        live_mutation_authority(dispatch, task_id, task, TaskCockpitSurface::Files)?;
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
    revalidate_issued_fence(
        dispatch,
        task_id,
        task,
        &authorization,
        &lease,
        command_id,
        action_epoch,
        runtime_generation,
        WorkspaceResource::File,
        TaskCockpitSurface::Files,
    )?;
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
    live_mutation_authority(dispatch, task_id, task, TaskCockpitSurface::Workspace).ok()
}

fn live_mutation_authority(
    dispatch: &TaskCockpitDispatch<'_>,
    task_id: TaskId,
    task: &crate::domain::task::TaskFacts,
    surface: TaskCockpitSurface,
) -> Result<
    (
        WorkspaceService,
        WorkspaceAuthorization,
        CommandId,
        u64,
        u64,
    ),
    QueryOutcome,
> {
    if dispatch.envelope_task_id != Some(task_id) {
        return Err(denied(surface, TaskCockpitDeniedReason::Unauthorized));
    }
    let Some(projects) = dispatch.workspace_projects else {
        return Err(unavailable(
            surface,
            TaskCockpitUnavailableReason::WorkspaceAuthorityUnavailable,
        ));
    };
    let Some(coordinator) = dispatch.coordinator else {
        return Err(unavailable(
            surface,
            TaskCockpitUnavailableReason::WorkspaceAuthorityUnavailable,
        ));
    };
    let Some(action_epoch) = dispatch.action_epoch else {
        return Err(unavailable(
            surface,
            TaskCockpitUnavailableReason::WorkspaceAuthorityUnavailable,
        ));
    };
    let Some(runtime_generation) = dispatch.runtime_generation else {
        return Err(unavailable(
            surface,
            TaskCockpitUnavailableReason::WorkspaceAuthorityUnavailable,
        ));
    };
    let command_id = CommandId::from_bytes(*dispatch.request_id.as_bytes()).map_err(|_| {
        unavailable(
            surface,
            TaskCockpitUnavailableReason::WorkspaceAuthorityUnavailable,
        )
    })?;
    let service = WorkspaceService::from_durable_with_task_coordinator(
        task.project_id,
        task_id,
        projects,
        &task.workspace,
        coordinator.clone(),
    )
    .map_err(|_| {
        unavailable(
            surface,
            TaskCockpitUnavailableReason::WorkspaceAuthorityUnavailable,
        )
    })?;
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
        .map_err(|_| denied(surface, TaskCockpitDeniedReason::StaleFence))?;
    Ok((
        service,
        authorization,
        command_id,
        action_epoch,
        runtime_generation,
    ))
}

fn remint_and_check_authorization(
    dispatch: &TaskCockpitDispatch<'_>,
    task_id: TaskId,
    task: &crate::domain::task::TaskFacts,
    surface: TaskCockpitSurface,
) -> Result<(), QueryOutcome> {
    let (_, authorization, command_id, action_epoch, runtime_generation) =
        live_mutation_authority(dispatch, task_id, task, surface)?;
    if authorization
        .validated_binding(
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
        .is_none()
    {
        return Err(denied(surface, TaskCockpitDeniedReason::StaleFence));
    }
    Ok(())
}

fn revalidate_issued_fence(
    dispatch: &TaskCockpitDispatch<'_>,
    task_id: TaskId,
    task: &crate::domain::task::TaskFacts,
    authorization: &WorkspaceAuthorization,
    lease: &WorkspaceResourceLease,
    command_id: CommandId,
    action_epoch: u64,
    runtime_generation: u64,
    resource: WorkspaceResource,
    surface: TaskCockpitSurface,
) -> Result<(), QueryOutcome> {
    let context = WorkspaceActionContext::new(
        task_id,
        task.project_id,
        dispatch.client_id,
        dispatch.connection_id,
        dispatch.request_id,
        command_id,
        task.workspace.clone(),
        action_epoch,
        runtime_generation,
    );
    revalidate_cockpit_workspace_action(authorization, lease, &context, resource).map_err(|error| {
        match error {
            crate::workspace::worktree::WorktreeError::StaleAuthority => {
                denied(surface, TaskCockpitDeniedReason::StaleFence)
            }
            _ => unavailable(
                surface,
                TaskCockpitUnavailableReason::WorkspaceAuthorityUnavailable,
            ),
        }
    })
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
            return denied(
                TaskCockpitSurface::Ssh,
                TaskCockpitDeniedReason::Unauthorized,
            );
        }
    }
    if dispatch.envelope_task_id != Some(task_id) {
        return denied(
            TaskCockpitSurface::Ssh,
            TaskCockpitDeniedReason::Unauthorized,
        );
    }
    if let Err(outcome) =
        remint_and_check_authorization(dispatch, task_id, task, TaskCockpitSurface::Ssh)
    {
        return outcome;
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
        | FileServiceError::HardLinkRejected { .. } => denied(
            TaskCockpitSurface::Files,
            TaskCockpitDeniedReason::PathTraversal,
        ),
        FileServiceError::SecretLikePath => denied(
            TaskCockpitSurface::Files,
            TaskCockpitDeniedReason::CapabilityDenied,
        ),
        FileServiceError::OutsideWorkspace { .. } => denied(
            TaskCockpitSurface::Files,
            TaskCockpitDeniedReason::OutsideWorkspace,
        ),
        FileServiceError::Conflict { .. } | FileServiceError::ChangedDuringRead { .. } => denied(
            TaskCockpitSurface::Files,
            TaskCockpitDeniedReason::RevisionConflict,
        ),
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
        for (key, value) in [
            ("user.name", "Cockpit Test"),
            ("user.email", "cockpit@example.invalid"),
        ] {
            let configured = std::process::Command::new("git")
                .args(["config", key, value])
                .current_dir(repository.path())
                .output()
                .expect("git config");
            assert!(configured.status.success());
        }
        fs::write(repository.path().join("README.md"), "hello-aé\n").expect("readme");
        fs::write(repository.path().join("blob.bin"), [0u8, 1, 2, 255]).expect("binary");
        fs::write(repository.path().join(".env"), "SECRET=1\n").expect("env");
        let database = repository.path().join("cockpit.sqlite");
        let mut bus = CommandBus::open(&database).expect("bus");
        let client_id = ClientId::new();
        let task_id = TaskId::new();
        let project_id = ProjectId::new();
        let roots =
            WorkspaceProjectRoots::try_from_pairs([(project_id, repository.path().to_path_buf())])
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

    fn granted() -> CapabilitySet {
        CapabilitySet::from_capabilities([Capability::TaskCockpit])
    }

    fn dispatch<'a>(
        bus: &'a CommandBus,
        client_id: ClientId,
        task_id: TaskId,
        query: &'a TaskCockpitQuery,
        roots: Option<&'a WorkspaceProjectRoots>,
        coordinator: Option<&'a WorkspaceResourceCoordinator>,
        action_epoch: Option<u64>,
        runtime_generation: Option<u64>,
    ) -> TaskCockpitDispatch<'a> {
        TaskCockpitDispatch {
            capabilities: granted(),
            envelope_task_id: Some(task_id),
            client_id,
            connection_id: Uuid::now_v7(),
            request_id: RequestId::new(),
            query,
            bus,
            service_runtime: None,
            ssh_endpoints: None,
            workspace_projects: roots,
            coordinator,
            action_epoch,
            runtime_generation,
        }
    }

    #[test]
    fn files_write_denies_traversal_secret_and_stale_expected_revision() {
        let (_repository, bus, client_id, task_id, roots) = create_bound_task();
        let coordinator = WorkspaceResourceCoordinator::new();
        let traversal = serve_task_cockpit(dispatch(
            &bus,
            client_id,
            task_id,
            &TaskCockpitQuery::FilesWrite {
                relative_path: "../secret.env".into(),
                utf8_contents: "nope".into(),
                expected_sha256_hex: None,
                confirm: true,
            },
            Some(&roots),
            Some(&coordinator),
            Some(1),
            Some(1),
        ));
        assert!(matches!(
            traversal,
            QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Denied {
                surface: TaskCockpitSurface::Files,
                reason: TaskCockpitDeniedReason::PathTraversal,
            }))
        ));
        let secret = serve_task_cockpit(dispatch(
            &bus,
            client_id,
            task_id,
            &TaskCockpitQuery::FilesWrite {
                relative_path: ".env".into(),
                utf8_contents: "SECRET=2\n".into(),
                expected_sha256_hex: None,
                confirm: true,
            },
            Some(&roots),
            Some(&coordinator),
            Some(1),
            Some(1),
        ));
        assert!(matches!(
            secret,
            QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Denied {
                surface: TaskCockpitSurface::Files,
                reason: TaskCockpitDeniedReason::CapabilityDenied,
            }))
        ));
        let missing = serve_task_cockpit(dispatch(
            &bus,
            client_id,
            task_id,
            &TaskCockpitQuery::FilesWrite {
                relative_path: "notes.txt".into(),
                utf8_contents: "phase6\n".into(),
                expected_sha256_hex: None,
                confirm: true,
            },
            Some(&roots),
            None,
            None,
            None,
        ));
        assert!(matches!(
            missing,
            QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Unavailable {
                surface: TaskCockpitSurface::Files,
                reason: TaskCockpitUnavailableReason::WorkspaceAuthorityUnavailable,
            }))
        ));
        let unconfirmed = serve_task_cockpit(dispatch(
            &bus,
            client_id,
            task_id,
            &TaskCockpitQuery::FilesWrite {
                relative_path: "notes.txt".into(),
                utf8_contents: "phase6\n".into(),
                expected_sha256_hex: None,
                confirm: false,
            },
            Some(&roots),
            Some(&coordinator),
            Some(1),
            Some(1),
        ));
        assert!(matches!(
            unconfirmed,
            QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Unavailable {
                surface: TaskCockpitSurface::Files,
                reason: TaskCockpitUnavailableReason::WriteUnsupported,
            }))
        ));
        let created = serve_task_cockpit(dispatch(
            &bus,
            client_id,
            task_id,
            &TaskCockpitQuery::FilesWrite {
                relative_path: "notes.txt".into(),
                utf8_contents: "phase6\n".into(),
                expected_sha256_hex: None,
                confirm: true,
            },
            Some(&roots),
            Some(&coordinator),
            Some(1),
            Some(1),
        ));
        let QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::FilesRead(created))) =
            created
        else {
            panic!("expected create write, got {created:?}");
        };
        assert_eq!(created.relative_path, "notes.txt");
        assert_eq!(created.utf8_prefix.as_deref(), Some("phase6\n"));
        let conflict = serve_task_cockpit(dispatch(
            &bus,
            client_id,
            task_id,
            &TaskCockpitQuery::FilesWrite {
                relative_path: "notes.txt".into(),
                utf8_contents: "changed\n".into(),
                expected_sha256_hex: Some(
                    "0000000000000000000000000000000000000000000000000000000000000000".into(),
                ),
                confirm: true,
            },
            Some(&roots),
            Some(&coordinator),
            Some(1),
            Some(1),
        ));
        assert!(matches!(
            conflict,
            QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Denied {
                surface: TaskCockpitSurface::Files,
                reason: TaskCockpitDeniedReason::RevisionConflict,
            }))
        ));
    }

    #[test]
    fn git_mutate_denies_traversal_and_stages_when_test_confirm_is_available() {
        let (_repository, bus, client_id, task_id, roots) = create_bound_task();
        let coordinator = WorkspaceResourceCoordinator::new();
        let traversal = serve_task_cockpit(dispatch(
            &bus,
            client_id,
            task_id,
            &TaskCockpitQuery::GitMutate {
                intent: TaskGitMutateIntent::Stage {
                    relative_paths: vec!["../secret".into()],
                },
                confirm: true,
            },
            Some(&roots),
            Some(&coordinator),
            Some(1),
            Some(1),
        ));
        assert!(matches!(
            traversal,
            QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Denied {
                surface: TaskCockpitSurface::Git,
                reason: TaskCockpitDeniedReason::PathTraversal,
            }))
        ));
        let planned = serve_task_cockpit(dispatch(
            &bus,
            client_id,
            task_id,
            &TaskCockpitQuery::GitMutate {
                intent: TaskGitMutateIntent::Stage {
                    relative_paths: vec!["README.md".into()],
                },
                confirm: false,
            },
            Some(&roots),
            Some(&coordinator),
            Some(1),
            Some(1),
        ));
        let QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Git(_))) = planned else {
            panic!("expected plan-only git projection, got {planned:?}");
        };
        let staged = serve_task_cockpit(dispatch(
            &bus,
            client_id,
            task_id,
            &TaskCockpitQuery::GitMutate {
                intent: TaskGitMutateIntent::Stage {
                    relative_paths: vec!["README.md".into()],
                },
                confirm: true,
            },
            Some(&roots),
            Some(&coordinator),
            Some(1),
            Some(1),
        ));
        let QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Git(staged))) = staged
        else {
            panic!("expected confirmed stage, got {staged:?}");
        };
        assert_eq!(staged.task_id, task_id);
    }

    #[test]
    fn git_mutate_host_issuer_unstages_and_commits_only_through_confirmed_plans() {
        let (_repository, bus, client_id, task_id, roots) = create_bound_task();
        let coordinator = WorkspaceResourceCoordinator::new();
        let staged = serve_task_cockpit(dispatch(
            &bus,
            client_id,
            task_id,
            &TaskCockpitQuery::GitMutate {
                intent: TaskGitMutateIntent::Stage {
                    relative_paths: vec!["README.md".into()],
                },
                confirm: true,
            },
            Some(&roots),
            Some(&coordinator),
            Some(1),
            Some(1),
        ));
        let QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Git(_))) = staged else {
            panic!("expected host-confirmed stage, got {staged:?}");
        };
        let unstaged = serve_task_cockpit(dispatch(
            &bus,
            client_id,
            task_id,
            &TaskCockpitQuery::GitMutate {
                intent: TaskGitMutateIntent::Unstage {
                    relative_paths: vec!["README.md".into()],
                },
                confirm: true,
            },
            Some(&roots),
            Some(&coordinator),
            Some(1),
            Some(1),
        ));
        let QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Git(_))) = unstaged else {
            panic!("expected host-confirmed unstage, got {unstaged:?}");
        };
        let restaged = serve_task_cockpit(dispatch(
            &bus,
            client_id,
            task_id,
            &TaskCockpitQuery::GitMutate {
                intent: TaskGitMutateIntent::Stage {
                    relative_paths: vec!["README.md".into()],
                },
                confirm: true,
            },
            Some(&roots),
            Some(&coordinator),
            Some(1),
            Some(1),
        ));
        let QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Git(_))) = restaged else {
            panic!("expected host-confirmed restage, got {restaged:?}");
        };
        let committed = serve_task_cockpit(dispatch(
            &bus,
            client_id,
            task_id,
            &TaskCockpitQuery::GitMutate {
                intent: TaskGitMutateIntent::Commit {
                    message: "cockpit host issuer".into(),
                },
                confirm: true,
            },
            Some(&roots),
            Some(&coordinator),
            Some(1),
            Some(1),
        ));
        let QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::Git(committed))) =
            committed
        else {
            panic!("expected host-confirmed commit, got {committed:?}");
        };
        assert_eq!(committed.task_id, task_id);
        assert_eq!(committed.change_count, 0);
    }

    #[test]
    fn mutation_revalidation_rejects_stale_action_epoch() {
        let (_repository, bus, client_id, task_id, roots) = create_bound_task();
        let coordinator = WorkspaceResourceCoordinator::new();
        let snapshot = bus.task_snapshot(task_id).expect("lookup").expect("task");
        let query = TaskCockpitQuery::WorkspaceStatus;
        let prepared = dispatch(
            &bus,
            client_id,
            task_id,
            &query,
            Some(&roots),
            Some(&coordinator),
            Some(1),
            Some(1),
        );
        let (service, authorization, command_id, action_epoch, runtime_generation) =
            live_mutation_authority(
                &prepared,
                task_id,
                &snapshot.task,
                TaskCockpitSurface::Files,
            )
            .expect("authority");
        let lease = service
            .acquire_task_resource(
                task_id,
                WorkspaceResource::File,
                prepared.client_id,
                prepared.connection_id,
                prepared.request_id,
                command_id,
                action_epoch,
                runtime_generation,
            )
            .expect("file lease");
        let current = WorkspaceActionContext::new(
            task_id,
            snapshot.task.project_id,
            prepared.client_id,
            prepared.connection_id,
            prepared.request_id,
            command_id,
            snapshot.task.workspace.clone(),
            action_epoch,
            runtime_generation,
        );
        revalidate_cockpit_workspace_action(
            &authorization,
            &lease,
            &current,
            WorkspaceResource::File,
        )
        .expect("current fence");
        let stale = WorkspaceActionContext::new(
            task_id,
            snapshot.task.project_id,
            prepared.client_id,
            prepared.connection_id,
            prepared.request_id,
            command_id,
            snapshot.task.workspace.clone(),
            action_epoch.saturating_add(3),
            runtime_generation,
        );
        assert!(matches!(
            revalidate_cockpit_workspace_action(
                &authorization,
                &lease,
                &stale,
                WorkspaceResource::File,
            ),
            Err(crate::workspace::worktree::WorktreeError::StaleAuthority)
        ));
    }
}
