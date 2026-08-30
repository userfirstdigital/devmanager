//! Git/changes panel projection backed by the typed Task Cockpit query.

use crate::client::action::{self, ActionRequest};
use crate::domain::cockpit::{
    redact_repository_label, TaskCockpitQuery, TaskGitProjection, TaskGitRepositoriesProjection,
    TaskRepositoryCatalogEntry, TaskRepositoryKind, TaskRepositorySelector, MAX_TASK_REPOSITORIES,
};
use crate::domain::id::TaskId;

use super::panel::{task_identity, PanelAction, PanelDisabledReason, PanelIdentity};

/// One bounded, path-redacted repository row for the Changes panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangesRepositoryRow {
    pub selector: TaskRepositorySelector,
    pub label: String,
    pub kind: TaskRepositoryKind,
    pub scope_label: &'static str,
    pub available: bool,
    pub read_only: bool,
    pub selected: bool,
    pub element_id: String,
    pub state_label: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangesPanelProjection {
    pub identity: PanelIdentity,
    pub selected_selector: Option<TaskRepositorySelector>,
    pub selected_label: Option<String>,
    pub branch: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub change_count: u32,
    pub detached: bool,
    pub repositories: Vec<ChangesRepositoryRow>,
    pub refresh: PanelAction,
    pub disabled_reason: Option<PanelDisabledReason>,
}

impl ChangesPanelProjection {
    pub fn from_host(
        git: Option<&TaskGitProjection>,
        catalog: Option<&TaskGitRepositoriesProjection>,
        selected: Option<&TaskRepositorySelector>,
        task_id: TaskId,
        revision: Option<u64>,
    ) -> Self {
        let identity = task_identity(task_id, revision);
        let catalog = catalog.filter(|catalog| catalog.task_id == task_id);
        // Never trust a caller selector until it reconciles against this Task catalog.
        let selected_selector =
            catalog.and_then(|catalog| reconcile_selected_repository(selected, catalog));
        let repositories = catalog
            .map(|catalog| project_repository_rows(catalog, selected_selector.as_ref()))
            .unwrap_or_default();

        let Some(refresh_selector) = selected_selector.clone() else {
            // Missing/foreign catalog or no status-readable entry: do not invent
            // an enabled Workspace refresh target.
            let disabled_reason = PanelDisabledReason::HostProjectionMissing;
            let placeholder = ActionRequest::TaskCockpit {
                task_id,
                query: TaskCockpitQuery::GitRepositories,
            };
            return Self {
                identity,
                selected_selector: None,
                selected_label: None,
                branch: None,
                ahead: 0,
                behind: 0,
                change_count: 0,
                detached: false,
                repositories,
                refresh: PanelAction::disabled(identity, placeholder, disabled_reason),
                disabled_reason: Some(disabled_reason),
            };
        };

        let request = ActionRequest::TaskCockpit {
            task_id,
            query: TaskCockpitQuery::GitStatusTargeted {
                selector: refresh_selector.clone(),
            },
        };

        let git = git.filter(|projection| {
            projection.task_id == task_id
                && git_projection_matches_selector(projection, &refresh_selector)
        });

        let selected_label = git
            .and_then(|projection| projection.label.as_deref())
            .map(redact_repository_label)
            .or_else(|| {
                repositories
                    .iter()
                    .find(|row| row.selected)
                    .map(|row| row.label.clone())
            });

        let Some(projection) = git else {
            let disabled_reason = PanelDisabledReason::ProjectionLoading;
            return Self {
                identity,
                selected_selector,
                selected_label,
                branch: None,
                ahead: 0,
                behind: 0,
                change_count: 0,
                detached: false,
                repositories,
                refresh: PanelAction::disabled(identity, request, disabled_reason),
                disabled_reason: Some(disabled_reason),
            };
        };

        Self {
            identity,
            selected_selector,
            selected_label,
            branch: projection.branch.clone(),
            ahead: projection.ahead,
            behind: projection.behind,
            change_count: projection.change_count,
            detached: projection.detached,
            repositories,
            refresh: PanelAction::enabled(identity, request),
            disabled_reason: None,
        }
    }

    pub fn summary(&self) -> String {
        if let Some(reason) = self.disabled_reason {
            if self.repositories.is_empty() {
                return reason.label().to_owned();
            }
        }
        let repository = self.selected_label.as_deref().unwrap_or("Repository");
        let branch =
            self.branch
                .as_deref()
                .unwrap_or(if self.detached { "detached" } else { "unknown" });
        format!(
            "{repository} · {branch} · {} change(s) · +{}/-{}",
            self.change_count, self.ahead, self.behind
        )
    }
}

pub fn repository_element_id(selector: &TaskRepositorySelector) -> String {
    match selector {
        TaskRepositorySelector::Workspace => "native-changes-repo-workspace".into(),
        TaskRepositorySelector::ProjectRoot => "native-changes-repo-project-root".into(),
        TaskRepositorySelector::Folder { folder_config_id } => {
            format!("native-changes-repo-folder-{folder_config_id}")
        }
    }
}

pub fn repository_scope_label(kind: TaskRepositoryKind) -> &'static str {
    match kind {
        TaskRepositoryKind::Workspace => "Workspace",
        TaskRepositoryKind::ProjectRoot => "Project root",
        TaskRepositoryKind::ConfiguredFolder => "Folder",
    }
}

pub fn repository_state_label(entry: &TaskRepositoryCatalogEntry) -> &'static str {
    if !entry.available {
        "unavailable"
    } else if entry.read_only {
        "read-only"
    } else {
        "available"
    }
}

pub fn repository_status_readable(entry: &TaskRepositoryCatalogEntry) -> bool {
    entry.available
}

pub fn repository_mutation_allowed(entry: &TaskRepositoryCatalogEntry) -> bool {
    entry.available && !entry.read_only
}

/// Default selection: Workspace when available, otherwise the first available entry.
pub fn default_repository_selector(
    catalog: &TaskGitRepositoriesProjection,
) -> Option<TaskRepositorySelector> {
    catalog
        .repositories
        .iter()
        .find(|entry| {
            entry.selector == TaskRepositorySelector::Workspace && repository_status_readable(entry)
        })
        .or_else(|| {
            catalog
                .repositories
                .iter()
                .find(|entry| repository_status_readable(entry))
        })
        .map(|entry| entry.selector.clone())
}

/// Keep the current selection when it remains status-readable; otherwise default.
pub fn reconcile_selected_repository(
    current: Option<&TaskRepositorySelector>,
    catalog: &TaskGitRepositoriesProjection,
) -> Option<TaskRepositorySelector> {
    if let Some(selector) = current {
        if catalog
            .repositories
            .iter()
            .any(|entry| &entry.selector == selector && repository_status_readable(entry))
        {
            return Some(selector.clone());
        }
    }
    default_repository_selector(catalog)
}

pub fn git_projection_matches_selector(
    projection: &TaskGitProjection,
    expected: &TaskRepositorySelector,
) -> bool {
    match projection.selector.as_ref() {
        Some(selector) => selector == expected,
        // Legacy Workspace shim may omit selector.
        None => *expected == TaskRepositorySelector::Workspace,
    }
}

fn project_repository_rows(
    catalog: &TaskGitRepositoriesProjection,
    selected: Option<&TaskRepositorySelector>,
) -> Vec<ChangesRepositoryRow> {
    catalog
        .repositories
        .iter()
        .take(MAX_TASK_REPOSITORIES)
        .map(|entry| {
            let label = redact_repository_label(&entry.label);
            ChangesRepositoryRow {
                selector: entry.selector.clone(),
                label,
                kind: entry.kind,
                scope_label: repository_scope_label(entry.kind),
                available: entry.available,
                read_only: entry.read_only,
                selected: selected == Some(&entry.selector),
                element_id: repository_element_id(&entry.selector),
                state_label: repository_state_label(entry),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog(task_id: TaskId) -> TaskGitRepositoriesProjection {
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
                    selector: TaskRepositorySelector::Folder {
                        folder_config_id: "sibling-b".into(),
                    },
                    label: "Sibling B".into(),
                    kind: TaskRepositoryKind::ConfiguredFolder,
                    available: true,
                    read_only: false,
                },
            ],
        }
    }

    #[test]
    fn changes_panel_projects_two_repos_with_distinct_status_and_targeted_refresh() {
        let task_id = TaskId::new();
        let catalog = catalog(task_id);
        let selected = TaskRepositorySelector::Folder {
            folder_config_id: "sibling-b".into(),
        };
        let git = TaskGitProjection {
            task_id,
            selector: Some(selected.clone()),
            label: Some("Sibling B".into()),
            branch: Some("feature/b".into()),
            ahead: 2,
            behind: 1,
            change_count: 4,
            detached: false,
            entries: Vec::new(),
        };
        let panel = ChangesPanelProjection::from_host(
            Some(&git),
            Some(&catalog),
            Some(&selected),
            task_id,
            Some(12),
        );
        assert!(panel.refresh.is_enabled());
        assert_eq!(panel.refresh.action_id, action::ACTION_GIT_STATUS);
        assert_eq!(panel.refresh.identity.revision, Some(12));
        assert!(matches!(
            &panel.refresh.request,
            ActionRequest::TaskCockpit {
                task_id: request_task,
                query: TaskCockpitQuery::GitStatusTargeted { selector },
            } if *request_task == task_id && *selector == selected
        ));
        assert_eq!(panel.repositories.len(), 2);
        assert_eq!(panel.repositories[0].scope_label, "Workspace");
        assert_eq!(panel.repositories[1].scope_label, "Folder");
        assert!(panel.repositories[1].selected);
        assert_eq!(panel.selected_selector.as_ref(), Some(&selected));
        assert_eq!(
            panel.summary(),
            "Sibling B · feature/b · 4 change(s) · +2/-1"
        );
        assert!(!panel.summary().contains('\\'));
        assert!(!panel.summary().contains("C:"));
        assert_eq!(
            panel.repositories[1].element_id,
            "native-changes-repo-folder-sibling-b"
        );
    }

    #[test]
    fn unavailable_and_read_only_rows_stay_visible_and_honest() {
        let task_id = TaskId::new();
        let catalog = TaskGitRepositoriesProjection {
            task_id,
            repositories: vec![
                TaskRepositoryCatalogEntry {
                    selector: TaskRepositorySelector::Workspace,
                    label: "Workspace".into(),
                    kind: TaskRepositoryKind::Workspace,
                    available: false,
                    read_only: false,
                },
                TaskRepositoryCatalogEntry {
                    selector: TaskRepositorySelector::ProjectRoot,
                    label: "Project root".into(),
                    kind: TaskRepositoryKind::ProjectRoot,
                    available: true,
                    read_only: true,
                },
            ],
        };
        let selected = TaskRepositorySelector::ProjectRoot;
        let panel =
            ChangesPanelProjection::from_host(None, Some(&catalog), Some(&selected), task_id, None);
        assert_eq!(panel.repositories[0].state_label, "unavailable");
        assert!(!panel.repositories[0].available);
        assert_eq!(panel.repositories[1].state_label, "read-only");
        assert!(panel.repositories[1].available);
        assert!(panel.repositories[1].read_only);
        assert!(repository_status_readable(&catalog.repositories[1]));
        assert!(!repository_mutation_allowed(&catalog.repositories[1]));
        assert_eq!(
            default_repository_selector(&catalog),
            Some(TaskRepositorySelector::ProjectRoot)
        );
    }

    #[test]
    fn selector_capture_rejects_foreign_or_missing_host_git_status() {
        let task_id = TaskId::new();
        let catalog = catalog(task_id);
        let selected = TaskRepositorySelector::Workspace;
        let foreign = TaskGitProjection {
            task_id: TaskId::new(),
            selector: Some(TaskRepositorySelector::Workspace),
            label: Some("Workspace".into()),
            branch: Some("main".into()),
            ahead: 0,
            behind: 0,
            change_count: 3,
            detached: false,
            entries: Vec::new(),
        };
        let panel = ChangesPanelProjection::from_host(
            Some(&foreign),
            Some(&catalog),
            Some(&selected),
            task_id,
            None,
        );
        assert_eq!(
            panel.disabled_reason,
            Some(PanelDisabledReason::ProjectionLoading)
        );
        assert_eq!(panel.change_count, 0);

        let missing = ChangesPanelProjection::from_host(None, None, None, TaskId::new(), None);
        assert_eq!(
            missing.disabled_reason,
            Some(PanelDisabledReason::HostProjectionMissing)
        );
        assert!(missing.selected_selector.is_none());
        assert!(!missing.refresh.is_enabled());
        assert_eq!(missing.summary(), "Host projection unavailable");
        assert!(matches!(
            missing.refresh.request,
            ActionRequest::TaskCockpit {
                query: TaskCockpitQuery::GitRepositories,
                ..
            }
        ));
    }

    #[test]
    fn stale_selector_absent_from_catalog_cannot_enable_targeted_refresh() {
        let task_id = TaskId::new();
        let catalog = TaskGitRepositoriesProjection {
            task_id,
            repositories: vec![TaskRepositoryCatalogEntry {
                selector: TaskRepositorySelector::Workspace,
                label: "Workspace".into(),
                kind: TaskRepositoryKind::Workspace,
                available: true,
                read_only: false,
            }],
        };
        let stale = TaskRepositorySelector::Folder {
            folder_config_id: "gone".into(),
        };
        let stale_git = TaskGitProjection {
            task_id,
            selector: Some(stale.clone()),
            label: Some("Gone".into()),
            branch: Some("feature".into()),
            ahead: 0,
            behind: 0,
            change_count: 5,
            detached: false,
            entries: Vec::new(),
        };
        let panel = ChangesPanelProjection::from_host(
            Some(&stale_git),
            Some(&catalog),
            Some(&stale),
            task_id,
            None,
        );
        assert_eq!(
            panel.selected_selector,
            Some(TaskRepositorySelector::Workspace)
        );
        assert!(!panel.refresh.is_enabled());
        assert_eq!(
            panel.disabled_reason,
            Some(PanelDisabledReason::ProjectionLoading)
        );
        assert!(matches!(
            panel.refresh.request,
            ActionRequest::TaskCockpit {
                query: TaskCockpitQuery::GitStatusTargeted {
                    selector: TaskRepositorySelector::Workspace
                },
                ..
            }
        ));
    }

    #[test]
    fn missing_or_foreign_catalog_does_not_invent_workspace_refresh() {
        let task_id = TaskId::new();
        let git = TaskGitProjection {
            task_id,
            selector: Some(TaskRepositorySelector::Workspace),
            label: Some("Workspace".into()),
            branch: Some("main".into()),
            ahead: 0,
            behind: 0,
            change_count: 2,
            detached: false,
            entries: Vec::new(),
        };
        let foreign_catalog = TaskGitRepositoriesProjection {
            task_id: TaskId::new(),
            repositories: vec![TaskRepositoryCatalogEntry {
                selector: TaskRepositorySelector::Workspace,
                label: "Workspace".into(),
                kind: TaskRepositoryKind::Workspace,
                available: true,
                read_only: false,
            }],
        };
        for catalog in [None, Some(&foreign_catalog)] {
            let panel = ChangesPanelProjection::from_host(
                Some(&git),
                catalog,
                Some(&TaskRepositorySelector::Workspace),
                task_id,
                None,
            );
            assert!(panel.selected_selector.is_none());
            assert!(!panel.refresh.is_enabled());
            assert_eq!(
                panel.disabled_reason,
                Some(PanelDisabledReason::HostProjectionMissing)
            );
        }
    }

    #[test]
    fn reconcile_keeps_available_selection_and_defaults_deterministically() {
        let task_id = TaskId::new();
        let catalog = catalog(task_id);
        let folder = TaskRepositorySelector::Folder {
            folder_config_id: "sibling-b".into(),
        };
        assert_eq!(
            reconcile_selected_repository(Some(&folder), &catalog),
            Some(folder)
        );
        assert_eq!(
            reconcile_selected_repository(None, &catalog),
            Some(TaskRepositorySelector::Workspace)
        );
        let unavailable_workspace = TaskGitRepositoriesProjection {
            task_id,
            repositories: vec![
                TaskRepositoryCatalogEntry {
                    selector: TaskRepositorySelector::Workspace,
                    label: "Workspace".into(),
                    kind: TaskRepositoryKind::Workspace,
                    available: false,
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
        };
        assert_eq!(
            reconcile_selected_repository(
                Some(&TaskRepositorySelector::Workspace),
                &unavailable_workspace
            ),
            Some(TaskRepositorySelector::ProjectRoot)
        );
    }
}
