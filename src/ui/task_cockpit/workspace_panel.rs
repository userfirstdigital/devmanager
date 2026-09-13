//! Task-owned workspace panel projection.

use crate::client::action::{self, ActionRequest};
use crate::domain::cockpit::{TaskCockpitQuery, TaskWorkspaceKind, TaskWorkspaceProjection};
use crate::domain::id::TaskId;

use super::panel::{task_identity, PanelAction, PanelDisabledReason, PanelIdentity};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspacePanelProjection {
    pub identity: PanelIdentity,
    pub kind: Option<TaskWorkspaceKind>,
    pub bound: bool,
    pub branch: Option<String>,
    pub has_repository_fingerprint: bool,
    pub refresh: PanelAction,
    pub disabled_reason: Option<PanelDisabledReason>,
}

impl WorkspacePanelProjection {
    pub fn from_host(
        projection: Option<&TaskWorkspaceProjection>,
        task_id: TaskId,
        revision: Option<u64>,
    ) -> Self {
        let identity = task_identity(task_id, revision);
        let request = ActionRequest::TaskCockpit {
            task_id,
            query: TaskCockpitQuery::WorkspaceStatus,
        };
        let Some(projection) = projection.filter(|projection| projection.task_id == task_id) else {
            return Self {
                identity,
                kind: None,
                bound: false,
                branch: None,
                has_repository_fingerprint: false,
                refresh: PanelAction::disabled(
                    identity,
                    request,
                    PanelDisabledReason::HostProjectionMissing,
                ),
                disabled_reason: Some(PanelDisabledReason::HostProjectionMissing),
            };
        };
        Self {
            identity,
            kind: Some(projection.kind),
            bound: projection.bound,
            branch: projection.branch.clone(),
            has_repository_fingerprint: projection.has_repository_fingerprint,
            refresh: PanelAction::enabled(identity, request),
            disabled_reason: None,
        }
    }

    pub fn summary(&self) -> String {
        let Some(kind) = self.kind else {
            return self.disabled_reason.map_or_else(
                || "Workspace unavailable".to_owned(),
                |reason| reason.label().to_owned(),
            );
        };
        let kind = match kind {
            TaskWorkspaceKind::Main => "main",
            TaskWorkspaceKind::Worktree => "worktree",
            TaskWorkspaceKind::External => "external",
            TaskWorkspaceKind::Bound => "bound",
        };
        let branch = self.branch.as_deref().unwrap_or("detached");
        format!(
            "{kind} · {branch} · {}",
            if self.bound { "bound" } else { "unbound" }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::cockpit::TaskWorkspaceKind;

    #[test]
    fn missing_workspace_projection_is_disabled_without_fabricating_state() {
        let task_id = TaskId::new();
        let panel = WorkspacePanelProjection::from_host(None, task_id, Some(4));
        assert_eq!(panel.identity.revision, Some(4));
        assert_eq!(panel.kind, None);
        assert!(!panel.refresh.is_enabled());
        assert_eq!(
            panel.refresh.disabled_reason,
            Some(PanelDisabledReason::HostProjectionMissing)
        );
    }

    #[test]
    fn workspace_projection_preserves_task_and_exact_query() {
        let task_id = TaskId::new();
        let source = TaskWorkspaceProjection {
            task_id,
            kind: TaskWorkspaceKind::Worktree,
            bound: true,
            branch: Some("feature/cockpit".into()),
            has_repository_fingerprint: true,
        };
        let panel = WorkspacePanelProjection::from_host(Some(&source), task_id, Some(9));
        assert!(panel.refresh.is_enabled());
        assert_eq!(panel.refresh.identity.task_id, task_id);
        assert_eq!(panel.refresh.identity.revision, Some(9));
        assert_eq!(panel.refresh.action_id, action::ACTION_WORKSPACE_STATUS);
        assert!(matches!(
            panel.refresh.request,
            ActionRequest::TaskCockpit {
                query: TaskCockpitQuery::WorkspaceStatus,
                ..
            }
        ));
        assert_eq!(panel.summary(), "worktree · feature/cockpit · bound");
    }
}
