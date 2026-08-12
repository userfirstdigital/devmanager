//! Task artifact metadata panel projection.
//!
//! Artifact bodies remain behind the existing paged artifact-content host
//! query.  This panel only displays bounded metadata and emits the existing
//! task refresh action; it never copies an artifact body into a GPUI render.

use crate::client::action::{self, ActionRequest};
use crate::domain::artifact::{ArtifactFacts, ArtifactKind, PrivacyClass};
use crate::domain::id::{ArtifactId, TaskId};
use crate::domain::snapshot::TaskSnapshot;

use super::panel::{task_identity, PanelAction, PanelIdentity, MAX_PANEL_ROWS};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactPanelRow {
    pub id: ArtifactId,
    pub label: String,
    pub kind: ArtifactKind,
    pub privacy_class: PrivacyClass,
    pub created_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactsPanelProjection {
    pub identity: PanelIdentity,
    pub rows: Vec<ArtifactPanelRow>,
    pub truncated: bool,
    pub refresh: PanelAction,
}

impl ArtifactsPanelProjection {
    pub fn from_snapshot(snapshot: Option<&TaskSnapshot>, task_id: TaskId) -> Self {
        let Some(snapshot) = snapshot.filter(|snapshot| snapshot.task.id == task_id) else {
            let identity = task_identity(task_id, None);
            return Self {
                identity,
                rows: Vec::new(),
                truncated: false,
                refresh: PanelAction::disabled(
                    identity,
                    ActionRequest::TaskShow { task_id },
                    super::panel::PanelDisabledReason::NoTaskSelected,
                ),
            };
        };
        let identity = task_identity(task_id, Some(snapshot.task.revision));
        let mut artifacts = snapshot.artifacts.values().collect::<Vec<_>>();
        artifacts.sort_by_key(|artifact| (artifact.created_at_ms, artifact.id));
        let truncated = artifacts.len() > MAX_PANEL_ROWS;
        let rows = artifacts
            .into_iter()
            .take(MAX_PANEL_ROWS)
            .map(artifact_row)
            .collect();
        Self {
            identity,
            rows,
            truncated,
            refresh: PanelAction::enabled(identity, ActionRequest::TaskShow { task_id }),
        }
    }

    pub fn summary(&self) -> String {
        format!(
            "{} artifact(s){}",
            self.rows.len(),
            if self.truncated { " · truncated" } else { "" }
        )
    }
}

fn artifact_row(artifact: &ArtifactFacts) -> ArtifactPanelRow {
    ArtifactPanelRow {
        id: artifact.id,
        label: artifact.label.clone(),
        kind: artifact.kind,
        privacy_class: artifact.privacy_class,
        created_at_ms: artifact.created_at_ms,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::artifact::{ArtifactContentRef, ArtifactFacts};
    use crate::domain::id::{EnvironmentId, ProjectId};
    use crate::domain::snapshot::TaskSnapshot;
    use crate::domain::task::WorkspaceRef;
    use crate::domain::task::{
        ReviewReadiness, TaskActivity, TaskAssignment, TaskAttention, TaskConnectivity, TaskFacts,
    };
    use std::collections::BTreeMap;

    fn snapshot(task_id: TaskId) -> TaskSnapshot {
        let mut task = TaskFacts::new(
            EnvironmentId::new(),
            "phase 6",
            None,
            ProjectId::new(),
            WorkspaceRef::Main,
            TaskAssignment::LocalOwner,
            1,
        )
        .expect("task");
        task.id = task_id;
        task.revision = 7;
        TaskSnapshot {
            task,
            connectivity: TaskConnectivity::Connected,
            attention: TaskAttention::None,
            activity: TaskActivity::Idle,
            review_readiness: ReviewReadiness::NotReady,
            agents: BTreeMap::new(),
            primary_agent_id: None,
            artifacts: BTreeMap::new(),
            resources: BTreeMap::new(),
            provider_sessions: BTreeMap::new(),
            browser: Default::default(),
        }
    }

    #[test]
    fn artifacts_panel_is_metadata_only_and_fenced_to_snapshot_revision() {
        let task_id = TaskId::new();
        let mut snapshot = snapshot(task_id);
        let artifact = ArtifactFacts::new(
            task_id,
            ArtifactKind::Evidence,
            "build log",
            ArtifactContentRef::inline_utf8("body").expect("content"),
            [0; 32],
            PrivacyClass::LocalOnly,
            2,
        )
        .expect("artifact");
        snapshot.artifacts.insert(artifact.id, artifact);
        let panel = ArtifactsPanelProjection::from_snapshot(Some(&snapshot), task_id);
        assert_eq!(panel.rows.len(), 1);
        assert_eq!(panel.identity.revision, Some(7));
        assert_eq!(panel.refresh.action_id, action::ACTION_TASK_SHOW);
        assert!(
            matches!(panel.refresh.request, ActionRequest::TaskShow { task_id: id } if id == task_id)
        );
    }
}
