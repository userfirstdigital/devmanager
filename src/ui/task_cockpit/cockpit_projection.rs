//! Typed Task Cockpit host-query projection for the native GPUI dock.
//!
//! These values are assembled only from [`TaskCockpitResult`] and catalog
//! descriptors. They never invent git/files/ssh/service facts or reuse demo
//! fixtures.

use crate::client::action::{
    cockpit_surface_descriptors, CockpitSurfaceDescriptor, CockpitSurfaceKind, ACTION_FILES_LIST,
    ACTION_FILES_READ, ACTION_GIT_STATUS, ACTION_SERVICE_HEALTH, ACTION_SERVICE_LOGS,
    ACTION_SSH_STATUS, ACTION_WORKSPACE_STATUS,
};
use crate::domain::cockpit::{
    TaskCockpitDeniedReason, TaskCockpitResult, TaskCockpitSurface, TaskCockpitUnavailableReason,
    TaskFileEntry, TaskFilesListProjection, TaskFilesReadProjection, TaskGitProjection,
    TaskServiceHealth, TaskServiceLogs, TaskServiceProjection, TaskServiceRuntimeState,
    TaskSshProjection, TaskWorkspaceProjection,
};
use crate::domain::id::TaskId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CockpitSurfaceLoad {
    Empty,
    Loading {
        action_id: &'static str,
    },
    Ready,
    Error {
        message: String,
    },
    Denied {
        reason: TaskCockpitDeniedReason,
    },
    Unavailable {
        reason: TaskCockpitUnavailableReason,
    },
}

impl CockpitSurfaceLoad {
    pub fn label(&self) -> String {
        match self {
            Self::Empty => "empty".into(),
            Self::Loading { action_id } => format!("loading {action_id}"),
            Self::Ready => "ready".into(),
            Self::Error { message } => message.clone(),
            Self::Denied { reason } => format!("denied:{reason:?}"),
            Self::Unavailable { reason } => format!("unavailable:{reason:?}"),
        }
    }

    pub fn is_loading(&self) -> bool {
        matches!(self, Self::Loading { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskCockpitLiveProjection {
    pub task_id: TaskId,
    pub load: CockpitSurfaceLoad,
    pub workspace: Option<TaskWorkspaceProjection>,
    pub git: Option<TaskGitProjection>,
    pub files: Option<TaskFilesListProjection>,
    pub file_read: Option<TaskFilesReadProjection>,
    pub ssh: Option<TaskSshProjection>,
    pub services: Option<TaskServiceProjection>,
    pub service_logs: Option<TaskServiceLogs>,
    pub service_health: Option<TaskServiceHealth>,
}

impl TaskCockpitLiveProjection {
    pub fn empty(task_id: TaskId) -> Self {
        Self {
            task_id,
            load: CockpitSurfaceLoad::Empty,
            workspace: None,
            git: None,
            files: None,
            file_read: None,
            ssh: None,
            services: None,
            service_logs: None,
            service_health: None,
        }
    }

    pub fn begin_query(&mut self, action_id: &'static str) {
        self.load = CockpitSurfaceLoad::Loading { action_id };
    }

    pub fn apply_result(&mut self, result: &TaskCockpitResult) {
        match result {
            TaskCockpitResult::Workspace(value) if value.task_id == self.task_id => {
                self.workspace = Some(value.clone());
                self.load = load_for_ready_or_empty(false);
            }
            TaskCockpitResult::Git(value) if value.task_id == self.task_id => {
                self.git = Some(value.clone());
                self.load =
                    load_for_ready_or_empty(value.change_count == 0 && value.branch.is_none());
            }
            TaskCockpitResult::FilesList(value) if value.task_id == self.task_id => {
                self.files = Some(value.clone());
                self.load = load_for_ready_or_empty(value.entries.is_empty());
            }
            TaskCockpitResult::FilesRead(value) if value.task_id == self.task_id => {
                self.file_read = Some(value.clone());
                self.load = CockpitSurfaceLoad::Ready;
            }
            TaskCockpitResult::Ssh(value) if value.task_id == self.task_id => {
                self.ssh = Some(value.clone());
                self.load = load_for_ready_or_empty(value.endpoints.is_empty());
            }
            TaskCockpitResult::Services(value) if value.task_id == self.task_id => {
                self.services = Some(value.clone());
                self.load = load_for_ready_or_empty(value.snapshots.is_empty());
            }
            TaskCockpitResult::ServiceLogs(value) if value.task_id == self.task_id => {
                self.service_logs = Some(value.clone());
                self.load = load_for_ready_or_empty(value.lines.is_empty());
            }
            TaskCockpitResult::ServiceHealth(value) if value.task_id == self.task_id => {
                self.service_health = Some(value.clone());
                self.load = CockpitSurfaceLoad::Ready;
            }
            TaskCockpitResult::Denied { reason, .. } => {
                self.load = CockpitSurfaceLoad::Denied { reason: *reason };
            }
            TaskCockpitResult::Unavailable { reason, .. } => {
                self.load = CockpitSurfaceLoad::Unavailable { reason: *reason };
            }
            _ => {
                self.load = CockpitSurfaceLoad::Error {
                    message: "task cockpit result does not match the selected task".into(),
                };
            }
        }
    }

    pub fn visible_file_entries(&self) -> &[TaskFileEntry] {
        self.files
            .as_ref()
            .map(|files| files.entries.as_slice())
            .unwrap_or(&[])
    }

    pub fn external_port_in_use(&self) -> bool {
        self.services.as_ref().is_some_and(|services| {
            services
                .snapshots
                .iter()
                .any(|snapshot| snapshot.state == TaskServiceRuntimeState::External)
        })
    }

    pub fn surface_descriptors(&self) -> &'static [CockpitSurfaceDescriptor] {
        cockpit_surface_descriptors()
    }

    pub fn reachable_query_ids(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.surface_descriptors().iter().filter_map(|surface| {
            if surface.available {
                surface.action_id
            } else {
                None
            }
        })
    }
}

fn load_for_ready_or_empty(empty: bool) -> CockpitSurfaceLoad {
    if empty {
        CockpitSurfaceLoad::Empty
    } else {
        CockpitSurfaceLoad::Ready
    }
}

pub fn surface_query_action_id(surface: TaskCockpitSurface) -> Option<&'static str> {
    match surface {
        TaskCockpitSurface::Workspace => Some(ACTION_WORKSPACE_STATUS),
        TaskCockpitSurface::Git => Some(ACTION_GIT_STATUS),
        TaskCockpitSurface::Files => Some(ACTION_FILES_LIST),
        TaskCockpitSurface::Ssh => Some(ACTION_SSH_STATUS),
        TaskCockpitSurface::Services => Some(ACTION_SERVICE_LOGS),
    }
}

pub fn file_read_action_id() -> &'static str {
    ACTION_FILES_READ
}

pub fn service_health_action_id() -> &'static str {
    ACTION_SERVICE_HEALTH
}

pub fn summary_line(projection: &TaskCockpitLiveProjection, kind: CockpitSurfaceKind) -> String {
    match (&projection.load, kind) {
        (CockpitSurfaceLoad::Loading { action_id }, _) => format!("Loading {action_id}"),
        (CockpitSurfaceLoad::Error { message }, _) => message.clone(),
        (CockpitSurfaceLoad::Denied { reason }, _) => format!("Denied ({reason:?})"),
        (CockpitSurfaceLoad::Unavailable { reason }, _) => format!("Unavailable ({reason:?})"),
        (CockpitSurfaceLoad::Empty, CockpitSurfaceKind::Git) => "No git changes".into(),
        (CockpitSurfaceLoad::Empty, CockpitSurfaceKind::Files) => "No files in this folder".into(),
        (CockpitSurfaceLoad::Empty, CockpitSurfaceKind::Ssh) => "No SSH endpoints".into(),
        (CockpitSurfaceLoad::Empty, CockpitSurfaceKind::Services) => {
            "No configured services".into()
        }
        (CockpitSurfaceLoad::Empty, _) => "Empty".into(),
        (_, CockpitSurfaceKind::Git) => projection
            .git
            .as_ref()
            .map(|git| {
                format!(
                    "Git · {} · {} change(s) · +{}/-{}",
                    git.branch.as_deref().unwrap_or("detached"),
                    git.change_count,
                    git.ahead,
                    git.behind
                )
            })
            .unwrap_or_else(|| "Git · host projection".into()),
        (_, CockpitSurfaceKind::Files) => {
            let files = projection.visible_file_entries();
            format!("Files · {} listed", files.len())
        }
        (_, CockpitSurfaceKind::Ssh) => projection
            .ssh
            .as_ref()
            .map(|ssh| format!("SSH · {} endpoint(s)", ssh.endpoints.len()))
            .unwrap_or_else(|| "SSH · host projection".into()),
        (_, CockpitSurfaceKind::Services) => {
            let count = projection
                .services
                .as_ref()
                .map(|services| services.snapshots.len())
                .unwrap_or(0);
            if projection.external_port_in_use() {
                format!("Services · {count} · external port in use")
            } else {
                format!("Services · {count}")
            }
        }
        (_, CockpitSurfaceKind::Workspace) => projection
            .workspace
            .as_ref()
            .map(|workspace| format!("Workspace · {:?}", workspace.kind))
            .unwrap_or_else(|| "Workspace · host projection".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::cockpit::{TaskServiceScope, TaskServiceSnapshot, TaskWorkspaceKind};
    use crate::domain::id::ConfiguredServiceId;

    fn task_id() -> TaskId {
        TaskId::new()
    }

    #[test]
    fn apply_result_projects_git_files_ssh_and_external_service_without_demo_state() {
        let task_id = task_id();
        let mut projection = TaskCockpitLiveProjection::empty(task_id);
        projection.begin_query(ACTION_GIT_STATUS);
        assert!(projection.load.is_loading());

        projection.apply_result(&TaskCockpitResult::Git(TaskGitProjection {
            task_id,
            branch: Some("codex/final-e2e-ui".into()),
            ahead: 1,
            behind: 0,
            change_count: 2,
            detached: false,
        }));
        assert_eq!(projection.load, CockpitSurfaceLoad::Ready);
        assert_eq!(
            summary_line(&projection, CockpitSurfaceKind::Git),
            "Git · codex/final-e2e-ui · 2 change(s) · +1/-0"
        );

        projection.apply_result(&TaskCockpitResult::FilesList(TaskFilesListProjection {
            task_id,
            entries: Vec::new(),
            truncated: false,
        }));
        assert_eq!(projection.load, CockpitSurfaceLoad::Empty);

        projection.apply_result(&TaskCockpitResult::Ssh(TaskSshProjection {
            task_id,
            endpoints: Vec::new(),
            runtime: None,
        }));
        assert_eq!(
            summary_line(&projection, CockpitSurfaceKind::Ssh),
            "No SSH endpoints"
        );

        projection.apply_result(&TaskCockpitResult::Services(TaskServiceProjection {
            task_id,
            snapshots: vec![TaskServiceSnapshot {
                service_id: ConfiguredServiceId::new("api").expect("id"),
                scope: TaskServiceScope::Task { task_id },
                state: TaskServiceRuntimeState::External,
                generation: 3,
                epoch: 4,
            }],
        }));
        assert!(projection.external_port_in_use());
        assert!(summary_line(&projection, CockpitSurfaceKind::Services)
            .contains("external port in use"));
        assert!(projection
            .reachable_query_ids()
            .any(|id| id == ACTION_GIT_STATUS
                || id == ACTION_FILES_LIST
                || id == ACTION_SSH_STATUS));
    }

    #[test]
    fn foreign_or_unavailable_results_fail_closed_instead_of_keeping_stale_ready() {
        let task_id = task_id();
        let mut projection = TaskCockpitLiveProjection::empty(task_id);
        projection.apply_result(&TaskCockpitResult::Workspace(TaskWorkspaceProjection {
            task_id: TaskId::new(),
            kind: TaskWorkspaceKind::Main,
            bound: false,
            branch: None,
            has_repository_fingerprint: false,
        }));
        assert!(matches!(projection.load, CockpitSurfaceLoad::Error { .. }));

        projection.apply_result(&TaskCockpitResult::Unavailable {
            surface: TaskCockpitSurface::Git,
            reason: TaskCockpitUnavailableReason::GitAuthorityNotIssued,
        });
        assert_eq!(
            projection.load,
            CockpitSurfaceLoad::Unavailable {
                reason: TaskCockpitUnavailableReason::GitAuthorityNotIssued,
            }
        );
        assert_eq!(
            surface_query_action_id(TaskCockpitSurface::Git),
            Some(ACTION_GIT_STATUS)
        );
    }
}
