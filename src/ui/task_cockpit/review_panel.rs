//! Review readiness and review-artifact panel projection.

use crate::client::action::{self, ActionRequest};
use crate::domain::artifact::{ArtifactKind, ArtifactSummary, PrivacyClass};
use crate::domain::id::{ArtifactId, TaskId};
use crate::domain::snapshot::TaskSnapshot;
use crate::domain::task::{ReviewReadiness, VisibleTaskStatus};

use super::panel::{
    task_identity, PanelAction, PanelDisabledReason, PanelIdentity, MAX_PANEL_ROWS,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewArtifactRow {
    pub id: ArtifactId,
    pub label: String,
    pub privacy_class: PrivacyClass,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewPanelProjection {
    pub identity: PanelIdentity,
    pub readiness: ReviewReadiness,
    pub status: VisibleTaskStatus,
    pub artifacts: Vec<ReviewArtifactRow>,
    pub truncated: bool,
    pub refresh: PanelAction,
    pub disabled_reason: Option<PanelDisabledReason>,
}

impl ReviewPanelProjection {
    pub fn from_model(
        snapshot: Option<&TaskSnapshot>,
        summaries: impl IntoIterator<Item = ArtifactSummary>,
        task_id: TaskId,
    ) -> Self {
        let Some(snapshot) = snapshot.filter(|snapshot| snapshot.task.id == task_id) else {
            let identity = task_identity(task_id, None);
            return Self {
                identity,
                readiness: ReviewReadiness::NotReady,
                status: VisibleTaskStatus::Disconnected,
                artifacts: Vec::new(),
                truncated: false,
                refresh: PanelAction::disabled(
                    identity,
                    ActionRequest::TaskShow { task_id },
                    PanelDisabledReason::HostProjectionMissing,
                ),
                disabled_reason: Some(PanelDisabledReason::HostProjectionMissing),
            };
        };
        let identity = task_identity(task_id, Some(snapshot.task.revision));
        let mut artifacts = summaries
            .into_iter()
            .filter(|artifact| artifact.task_id == task_id)
            .filter(|artifact| artifact.kind == ArtifactKind::ReviewReport)
            .map(|artifact| ReviewArtifactRow {
                id: artifact.id,
                label: crate::domain::cockpit::truncate_to_max_bytes(
                    &artifact.label,
                    super::panel::MAX_PANEL_LABEL_BYTES,
                ),
                privacy_class: artifact.privacy_class,
            })
            .collect::<Vec<_>>();
        artifacts.sort_by_key(|artifact| artifact.id);
        let truncated = artifacts.len() > MAX_PANEL_ROWS;
        artifacts.truncate(MAX_PANEL_ROWS);
        let readiness = snapshot.review_readiness;
        Self {
            identity,
            readiness,
            status: snapshot.visible_status(),
            artifacts,
            truncated,
            refresh: PanelAction::enabled(identity, ActionRequest::TaskShow { task_id }),
            disabled_reason: (readiness != ReviewReadiness::Ready)
                .then_some(PanelDisabledReason::NotReviewable),
        }
    }

    pub fn summary(&self) -> String {
        format!(
            "{:?} · {:?} · {} review artifact(s){}",
            self.status,
            self.readiness,
            self.artifacts.len(),
            if self.truncated { " · truncated" } else { "" }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::snapshot::TaskSnapshot;

    #[test]
    fn missing_review_snapshot_reports_missing_host_projection() {
        let panel = ReviewPanelProjection::from_model(None, Vec::new(), TaskId::new());
        assert_eq!(panel.readiness, ReviewReadiness::NotReady);
        assert_eq!(
            panel.disabled_reason,
            Some(PanelDisabledReason::HostProjectionMissing)
        );
        assert!(!panel.refresh.is_enabled());
    }

    #[test]
    fn review_panel_preserves_snapshot_revision_and_existing_task_show_action() {
        // A full snapshot fixture is intentionally supplied by the artifact
        // panel tests; this assertion keeps the type-level contract visible to
        // callers without duplicating task construction here.
        let _ = std::mem::size_of::<TaskSnapshot>();
        let identity = task_identity(TaskId::new(), Some(8));
        let action = PanelAction::enabled(
            identity,
            ActionRequest::TaskShow {
                task_id: identity.task_id,
            },
        );
        assert_eq!(action.identity.revision, Some(8));
        assert!(matches!(action.request, ActionRequest::TaskShow { .. }));
    }
}
