//! Git/changes panel projection backed by the typed Task Cockpit query.

use crate::client::action::{self, ActionRequest};
use crate::domain::cockpit::{TaskCockpitQuery, TaskGitProjection};
use crate::domain::id::TaskId;

use super::panel::{task_identity, PanelAction, PanelDisabledReason, PanelIdentity};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangesPanelProjection {
    pub identity: PanelIdentity,
    pub branch: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub change_count: u32,
    pub detached: bool,
    pub refresh: PanelAction,
    pub disabled_reason: Option<PanelDisabledReason>,
}

impl ChangesPanelProjection {
    pub fn from_host(
        projection: Option<&TaskGitProjection>,
        task_id: TaskId,
        revision: Option<u64>,
    ) -> Self {
        let identity = task_identity(task_id, revision);
        let request = ActionRequest::TaskCockpit {
            task_id,
            query: TaskCockpitQuery::GitStatus,
        };
        let Some(projection) = projection.filter(|projection| projection.task_id == task_id) else {
            return Self {
                identity,
                branch: None,
                ahead: 0,
                behind: 0,
                change_count: 0,
                detached: false,
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
            branch: projection.branch.clone(),
            ahead: projection.ahead,
            behind: projection.behind,
            change_count: projection.change_count,
            detached: projection.detached,
            refresh: PanelAction::enabled(identity, request),
            disabled_reason: None,
        }
    }

    pub fn summary(&self) -> String {
        if let Some(reason) = self.disabled_reason {
            return reason.label().to_owned();
        }
        format!(
            "{} · {} change(s) · +{}/-{}",
            self.branch
                .as_deref()
                .unwrap_or(if self.detached { "detached" } else { "unknown" }),
            self.change_count,
            self.ahead,
            self.behind
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changes_panel_keeps_git_query_and_revision_identity() {
        let task_id = TaskId::new();
        let source = TaskGitProjection {
            task_id,
            branch: Some("main".into()),
            ahead: 2,
            behind: 1,
            change_count: 3,
            detached: false,
        };
        let panel = ChangesPanelProjection::from_host(Some(&source), task_id, Some(12));
        assert!(panel.refresh.is_enabled());
        assert_eq!(panel.refresh.action_id, action::ACTION_GIT_STATUS);
        assert_eq!(panel.refresh.identity.revision, Some(12));
        assert!(matches!(
            panel.refresh.request,
            ActionRequest::TaskCockpit {
                task_id: request_task,
                query: TaskCockpitQuery::GitStatus,
            } if request_task == task_id
        ));
        assert_eq!(panel.summary(), "main · 3 change(s) · +2/-1");
    }

    #[test]
    fn changes_panel_does_not_show_zeroes_for_a_missing_host_projection() {
        let panel = ChangesPanelProjection::from_host(None, TaskId::new(), None);
        assert_eq!(
            panel.disabled_reason,
            Some(PanelDisabledReason::HostProjectionMissing)
        );
        assert_eq!(panel.summary(), "Host projection unavailable");
    }
}
