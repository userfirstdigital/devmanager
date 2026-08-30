//! Typed Task Cockpit host-query projection for the native GPUI dock.
//!
//! These values are assembled only from [`TaskCockpitResult`] and catalog
//! descriptors. They never invent git/files/ssh/service facts or reuse demo
//! fixtures.

use crate::client::action::{
    cockpit_surface_descriptors, CockpitSurfaceDescriptor, CockpitSurfaceKind,
    ACTION_BROWSER_NATIVE, ACTION_FILES_LIST, ACTION_FILES_READ, ACTION_GIT_STATUS,
    ACTION_PROVIDER_TERMINAL_INPUT, ACTION_SERVICE_HEALTH, ACTION_SERVICE_LOGS, ACTION_SSH_STATUS,
    ACTION_WORKSPACE_STATUS,
};
use crate::domain::cockpit::{
    TaskCockpitDeniedReason, TaskCockpitResult, TaskCockpitSurface, TaskCockpitUnavailableReason,
    TaskFileEntry, TaskFilesListProjection, TaskFilesReadProjection, TaskGitCommitDiffProjection,
    TaskGitFileDiffProjection, TaskGitHistoryProjection, TaskGitProjection,
    TaskGitRepositoriesProjection, TaskServiceHealth, TaskServiceLogs, TaskServiceProjection,
    TaskServiceRuntimeState, TaskSshProjection, TaskTerminalProjection, TaskWorkspaceProjection,
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
    pub repositories: Option<TaskGitRepositoriesProjection>,
    pub git: Option<TaskGitProjection>,
    pub git_file_diff: Option<TaskGitFileDiffProjection>,
    pub git_history: Option<TaskGitHistoryProjection>,
    pub git_commit_diff: Option<TaskGitCommitDiffProjection>,
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
            repositories: None,
            git: None,
            git_file_diff: None,
            git_history: None,
            git_commit_diff: None,
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
            TaskCockpitResult::GitRepositories(value) if value.task_id == self.task_id => {
                self.repositories = Some(value.clone());
                self.reconcile_git_with_catalog();
                self.load = load_for_ready_or_empty(value.repositories.is_empty());
            }
            TaskCockpitResult::Git(value) if value.task_id == self.task_id => {
                self.git = Some(value.clone());
                self.reconcile_git_with_catalog();
                self.load =
                    load_for_ready_or_empty(value.change_count == 0 && value.branch.is_none());
            }
            TaskCockpitResult::GitFileDiff(value) if value.task_id == self.task_id => {
                self.git_file_diff = Some(value.clone());
                self.load = CockpitSurfaceLoad::Ready;
            }
            TaskCockpitResult::GitHistory(value) if value.task_id == self.task_id => {
                self.git_history = Some(value.clone());
                self.load = load_for_ready_or_empty(value.entries.is_empty());
            }
            TaskCockpitResult::GitCommitDiff(value) if value.task_id == self.task_id => {
                self.git_commit_diff = Some(value.clone());
                self.load = CockpitSurfaceLoad::Ready;
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
            TaskCockpitResult::Config(_)
            | TaskCockpitResult::AgentConnection(_)
            | TaskCockpitResult::BrowserProcessSession(_)
            | TaskCockpitResult::Conversation(_)
            | TaskCockpitResult::Terminal(_)
            | TaskCockpitResult::ConfigCommandDetail(_) => {}
            TaskCockpitResult::Denied { reason, .. } => {
                self.load = CockpitSurfaceLoad::Denied { reason: *reason };
            }
            TaskCockpitResult::Unavailable { reason, .. } => {
                self.load = CockpitSurfaceLoad::Unavailable { reason: *reason };
            }
            TaskCockpitResult::GitRepositories(_) => {
                self.repositories = None;
                self.git = None;
                self.load = CockpitSurfaceLoad::Error {
                    message: "task cockpit result does not match the selected task".into(),
                };
            }
            TaskCockpitResult::Git(_) => {
                self.git = None;
                self.load = CockpitSurfaceLoad::Error {
                    message: "task cockpit result does not match the selected task".into(),
                };
            }
            TaskCockpitResult::GitFileDiff(_) => {
                self.git_file_diff = None;
                self.load = CockpitSurfaceLoad::Error {
                    message: "task cockpit result does not match the selected task".into(),
                };
            }
            TaskCockpitResult::GitHistory(_) => {
                self.git_history = None;
                self.load = CockpitSurfaceLoad::Error {
                    message: "task cockpit result does not match the selected task".into(),
                };
            }
            TaskCockpitResult::GitCommitDiff(_) => {
                self.git_commit_diff = None;
                self.load = CockpitSurfaceLoad::Error {
                    message: "task cockpit result does not match the selected task".into(),
                };
            }
            _ => {
                self.load = CockpitSurfaceLoad::Error {
                    message: "task cockpit result does not match the selected task".into(),
                };
            }
        }
    }

    /// Drop retained git status when its selector is absent from the Task catalog.
    fn reconcile_git_with_catalog(&mut self) {
        let Some(catalog) = self.repositories.as_ref() else {
            return;
        };
        let Some(git) = self.git.as_ref() else {
            return;
        };
        let Some(selector) = git.selector.as_ref() else {
            return;
        };
        if !catalog
            .repositories
            .iter()
            .any(|entry| &entry.selector == selector)
        {
            self.git = None;
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
        TaskCockpitSurface::Conversation => None,
        TaskCockpitSurface::Terminal => Some(ACTION_PROVIDER_TERMINAL_INPUT),
        TaskCockpitSurface::Workspace => Some(ACTION_WORKSPACE_STATUS),
        TaskCockpitSurface::Git => Some(ACTION_GIT_STATUS),
        TaskCockpitSurface::Files => Some(ACTION_FILES_LIST),
        TaskCockpitSurface::Ssh => Some(ACTION_SSH_STATUS),
        TaskCockpitSurface::Services => Some(ACTION_SERVICE_LOGS),
        TaskCockpitSurface::Browser => Some(ACTION_BROWSER_NATIVE),
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
                let repository = git.label.as_deref().unwrap_or("Repository");
                format!(
                    "Git · {repository} · {} · {} change(s) · +{}/-{}",
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
    use crate::domain::cockpit::{
        TaskRepositoryCatalogEntry, TaskRepositoryKind, TaskRepositorySelector, TaskServiceScope,
        TaskServiceSnapshot, TaskWorkspaceKind,
    };
    use crate::domain::id::ConfiguredServiceId;

    fn task_id() -> TaskId {
        TaskId::new()
    }

    fn git_projection(task_id: TaskId) -> TaskGitProjection {
        TaskGitProjection {
            task_id,
            selector: Some(TaskRepositorySelector::Workspace),
            label: Some("Workspace".into()),
            branch: Some("codex/final-e2e-ui".into()),
            ahead: 1,
            behind: 0,
            change_count: 2,
            detached: false,
            entries: Vec::new(),
        }
    }

    #[test]
    fn apply_result_projects_git_files_ssh_and_external_service_without_demo_state() {
        let task_id = task_id();
        let mut projection = TaskCockpitLiveProjection::empty(task_id);
        projection.begin_query(ACTION_GIT_STATUS);
        assert!(projection.load.is_loading());

        projection.apply_result(&TaskCockpitResult::Git(git_projection(task_id)));
        assert_eq!(projection.load, CockpitSurfaceLoad::Ready);
        assert_eq!(
            summary_line(&projection, CockpitSurfaceKind::Git),
            "Git · Workspace · codex/final-e2e-ui · 2 change(s) · +1/-0"
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
    fn git_repositories_are_retained_and_clear_stale_status_when_selector_leaves_catalog() {
        let task_id = task_id();
        let mut projection = TaskCockpitLiveProjection::empty(task_id);
        projection.apply_result(&TaskCockpitResult::Git(TaskGitProjection {
            task_id,
            selector: Some(TaskRepositorySelector::Folder {
                folder_config_id: "sibling-a".into(),
            }),
            label: Some("Sibling A".into()),
            branch: Some("feature".into()),
            ahead: 0,
            behind: 0,
            change_count: 1,
            detached: false,
            entries: Vec::new(),
        }));
        assert!(projection.git.is_some());

        projection.apply_result(&TaskCockpitResult::GitRepositories(
            TaskGitRepositoriesProjection {
                task_id,
                repositories: vec![
                    TaskRepositoryCatalogEntry {
                        selector: TaskRepositorySelector::Workspace,
                        label: "Workspace".into(),
                        kind: TaskRepositoryKind::Workspace,
                        available: true,
                        read_only: false,
                    },
                    TaskRepositoryCatalogEntry {
                        selector: TaskRepositorySelector::ProjectRoot,
                        label: "Project root".into(),
                        kind: TaskRepositoryKind::ProjectRoot,
                        available: true,
                        read_only: false,
                    },
                ],
            },
        ));
        assert_eq!(
            projection
                .repositories
                .as_ref()
                .map(|catalog| catalog.repositories.len()),
            Some(2)
        );
        assert!(
            projection.git.is_none(),
            "status for a selector absent from the new catalog must clear"
        );
    }

    #[test]
    fn foreign_git_or_repository_catalog_results_fail_closed_and_drop_stale_git() {
        let task_id = task_id();
        let mut projection = TaskCockpitLiveProjection::empty(task_id);
        projection.apply_result(&TaskCockpitResult::Git(git_projection(task_id)));
        assert!(projection.git.is_some());

        projection.apply_result(&TaskCockpitResult::GitRepositories(
            TaskGitRepositoriesProjection {
                task_id: TaskId::new(),
                repositories: vec![TaskRepositoryCatalogEntry {
                    selector: TaskRepositorySelector::Workspace,
                    label: "Workspace".into(),
                    kind: TaskRepositoryKind::Workspace,
                    available: true,
                    read_only: false,
                }],
            },
        ));
        assert!(matches!(projection.load, CockpitSurfaceLoad::Error { .. }));
        assert!(projection.repositories.is_none());
        assert!(projection.git.is_none());

        projection.apply_result(&TaskCockpitResult::Git(git_projection(task_id)));
        projection.apply_result(&TaskCockpitResult::Git(TaskGitProjection {
            task_id: TaskId::new(),
            selector: Some(TaskRepositorySelector::Workspace),
            label: Some("Workspace".into()),
            branch: Some("other".into()),
            ahead: 0,
            behind: 0,
            change_count: 9,
            detached: false,
            entries: Vec::new(),
        }));
        assert!(matches!(projection.load, CockpitSurfaceLoad::Error { .. }));
        assert!(projection.git.is_none());
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

    #[test]
    fn terminal_results_remain_out_of_band_from_live_projection() {
        let task_id = task_id();
        let mut projection = TaskCockpitLiveProjection::empty(task_id);
        projection.begin_query(ACTION_FILES_LIST);
        assert!(projection.load.is_loading());
        projection.apply_result(&TaskCockpitResult::Terminal(TaskTerminalProjection {
            task_id,
            terminal_id: crate::domain::TerminalId::new(),
            session_id: crate::terminal::protocol::TerminalSessionId::new(),
            agent_session_id: crate::domain::AgentSessionId::new(),
            resource_id: crate::domain::ResourceId::new(),
            runtime_generation: 1,
            resource_generation: 1,
            action_epoch: 1,
            accepts_input_without_conversation_id: false,
            sequence: 1,
            title: Some("out-of-band".into()),
            text_lines: vec!["should not land in live files".into()],
            screen: Default::default(),
        }));
        assert!(
            projection.files.is_none(),
            "Terminal stays out-of-band; FilesList projection is untouched"
        );
        assert!(
            projection.load.is_loading(),
            "Terminal must not settle the in-band cockpit surface load"
        );
    }
}
