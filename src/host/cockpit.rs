//! Host dispatch for typed Task Cockpit queries.
//!
//! Workspace identity is resolved from the selected Task admission. Client
//! paths are never authoritative. Surfaces without a live safe authority
//! return an explicit typed unavailable or denied result.

use crate::domain::cockpit::{
    cockpit_surface, relative_path_is_safe, workspace_projection,
    TaskCockpitDeniedReason, TaskCockpitQuery, TaskCockpitResult, TaskCockpitSurface,
    TaskCockpitUnavailableReason, TaskSshEndpoint, TaskSshProjection, MAX_COCKPIT_FILE_LIST,
    MAX_COCKPIT_READ_BYTES,
};
use crate::domain::id::TaskId;
use crate::domain::query::{QueryError, QueryOutcome, QueryResult};
use crate::kernel::CommandBus;
use crate::protocol::CapabilitySet;
use crate::services::ProcessManager;

pub(crate) fn serve_task_cockpit(
    capabilities: CapabilitySet,
    task_id: Option<TaskId>,
    query: &TaskCockpitQuery,
    bus: &CommandBus,
    service_runtime: Option<&ProcessManager>,
    ssh_endpoints: Option<&[TaskSshEndpoint]>,
) -> QueryOutcome {
    if !capabilities.grants_task_cockpit() {
        return QueryOutcome::Err(QueryError::UnsupportedCapability);
    }
    let surface = cockpit_surface(query);
    let Some(task_id) = task_id else {
        return denied(surface, TaskCockpitDeniedReason::MissingTask);
    };
    let snapshot = match bus.task_snapshot(task_id) {
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

    match query {
        TaskCockpitQuery::WorkspaceStatus => QueryOutcome::Ok(QueryResult::TaskCockpit(
            TaskCockpitResult::Workspace(workspace_projection(task_id, &snapshot.task.workspace)),
        )),
        TaskCockpitQuery::GitStatus => unavailable(
            TaskCockpitSurface::Git,
            TaskCockpitUnavailableReason::GitAuthorityNotIssued,
        ),
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
            unavailable(
                TaskCockpitSurface::Files,
                TaskCockpitUnavailableReason::FileAuthorityNotIssued,
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
            unavailable(
                TaskCockpitSurface::Files,
                TaskCockpitUnavailableReason::FileAuthorityNotIssued,
            )
        }
        TaskCockpitQuery::FilesWrite => unavailable(
            TaskCockpitSurface::Files,
            TaskCockpitUnavailableReason::WriteUnsupported,
        ),
        TaskCockpitQuery::SshStatus => match ssh_endpoints {
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
        TaskCockpitQuery::SshAction => unavailable(
            TaskCockpitSurface::Ssh,
            TaskCockpitUnavailableReason::SshOperationUnsupported,
        ),
        TaskCockpitQuery::ServiceSnapshots => match service_runtime {
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
        TaskCockpitQuery::ServiceLogs => unavailable(
            TaskCockpitSurface::Services,
            TaskCockpitUnavailableReason::LogsUnsupported,
        ),
        TaskCockpitQuery::ServiceHealth => unavailable(
            TaskCockpitSurface::Services,
            TaskCockpitUnavailableReason::HealthUnsupported,
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
