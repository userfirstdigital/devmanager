//! Header Open / Commit affordances backed by typed TaskCockpit authority.

use crate::domain::cockpit::TaskCockpitQuery;
use crate::domain::id::RequestId;
use crate::domain::{
    redact_repository_label, TaskGitMutateIntent, TaskGitProjection, TaskId, TaskRepositorySelector,
};
use crate::ui::task_cockpit::changes_panel::git_projection_matches_selector;
use crate::ui::task_cockpit::panel::PanelDisabledReason;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeaderCommitPhase {
    Idle,
    LoadingStatus,
    Preview {
        message: String,
        change_count: u32,
        branch: Option<String>,
        error: Option<String>,
    },
    Confirming {
        message: String,
    },
    Success {
        message: String,
    },
    Error(String),
}

impl Default for HeaderCommitPhase {
    fn default() -> Self {
        Self::Idle
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HeaderCommitWorkflow {
    pub task_id: Option<TaskId>,
    /// Exact repository selector captured when Commit began / status correlated.
    pub selector: Option<TaskRepositorySelector>,
    /// Path-redacted repository label shown in the overlay.
    pub repository_label: Option<String>,
    /// Exact host request id for the LoadingStatus git status probe.
    pub status_request_id: Option<RequestId>,
    /// Exact host request id for the Confirming mutate.
    pub mutate_request_id: Option<RequestId>,
    pub phase: HeaderCommitPhase,
}

impl HeaderCommitWorkflow {
    pub fn begin(
        &mut self,
        task_id: TaskId,
        selector: TaskRepositorySelector,
        repository_label: impl Into<String>,
    ) {
        self.task_id = Some(task_id);
        self.selector = Some(selector);
        self.repository_label = Some(redact_repository_label(&repository_label.into()));
        self.status_request_id = None;
        self.mutate_request_id = None;
        self.phase = HeaderCommitPhase::LoadingStatus;
    }

    pub fn begin_blocked(&mut self, reason: PanelDisabledReason) {
        *self = Self::default();
        self.phase = HeaderCommitPhase::Error(reason.label().to_owned());
    }

    pub fn bind_status_request(&mut self, request_id: RequestId) {
        if matches!(self.phase, HeaderCommitPhase::LoadingStatus) {
            self.status_request_id = Some(request_id);
        }
    }

    pub fn bind_mutate_request(&mut self, request_id: RequestId) {
        if matches!(self.phase, HeaderCommitPhase::Confirming { .. }) {
            self.mutate_request_id = Some(request_id);
        }
    }

    /// True when this is the exact commit status probe (request id + task + selector + kind).
    pub fn correlates_status_command(
        &self,
        request_id: RequestId,
        task_id: TaskId,
        query: &TaskCockpitQuery,
    ) -> bool {
        if !matches!(self.phase, HeaderCommitPhase::LoadingStatus) {
            return false;
        }
        if self.status_request_id != Some(request_id) {
            return false;
        }
        if self.task_id != Some(task_id) {
            return false;
        }
        let Some(captured) = self.selector.as_ref() else {
            return false;
        };
        match query {
            TaskCockpitQuery::GitStatusTargeted { selector } => selector == captured,
            // Legacy shim: only when the capture itself is Workspace.
            TaskCockpitQuery::GitStatus => *captured == TaskRepositorySelector::Workspace,
            _ => false,
        }
    }

    /// True when this is the exact confirmed mutate (request id + task + selector + confirm).
    pub fn correlates_mutation_command(
        &self,
        request_id: RequestId,
        task_id: TaskId,
        query: &TaskCockpitQuery,
    ) -> bool {
        if !matches!(self.phase, HeaderCommitPhase::Confirming { .. }) {
            return false;
        }
        if self.mutate_request_id != Some(request_id) {
            return false;
        }
        if self.task_id != Some(task_id) {
            return false;
        }
        let Some(captured) = self.selector.as_ref() else {
            return false;
        };
        match query {
            TaskCockpitQuery::GitMutateTargeted {
                selector,
                confirm: true,
                ..
            } => selector == captured,
            TaskCockpitQuery::GitMutate { confirm: true, .. } => {
                *captured == TaskRepositorySelector::Workspace
            }
            _ => false,
        }
    }

    pub fn correlates_failure_command(
        &self,
        request_id: RequestId,
        task_id: TaskId,
        query: &TaskCockpitQuery,
    ) -> bool {
        self.correlates_status_command(request_id, task_id, query)
            || self.correlates_mutation_command(request_id, task_id, query)
    }

    pub fn apply_status_from_command(
        &mut self,
        projection: &TaskGitProjection,
        request_id: RequestId,
        command_task_id: TaskId,
        query: &TaskCockpitQuery,
    ) {
        if !self.correlates_status_command(request_id, command_task_id, query) {
            return;
        }
        self.apply_status(projection);
    }

    pub fn apply_status(&mut self, projection: &TaskGitProjection) {
        if self.task_id != Some(projection.task_id) {
            return;
        }
        let Some(expected) = self.selector.as_ref() else {
            return;
        };
        if !git_projection_matches_selector(projection, expected) {
            // Unrelated Git response must not retarget an in-flight commit.
            return;
        }
        if let Some(label) = projection.label.as_deref() {
            self.repository_label = Some(redact_repository_label(label));
        }
        if projection.change_count == 0 {
            self.clear_request_fence();
            self.phase = HeaderCommitPhase::Error("No changes to commit.".to_string());
            return;
        }
        self.status_request_id = None;
        self.phase = HeaderCommitPhase::Preview {
            message: String::new(),
            change_count: projection.change_count,
            branch: projection.branch.clone(),
            error: None,
        };
    }

    pub fn set_message(&mut self, message: String) {
        if let HeaderCommitPhase::Preview {
            message: current,
            error,
            ..
        } = &mut self.phase
        {
            *current = message;
            *error = None;
        }
    }

    pub fn push_message_char(&mut self, ch: char) {
        if let HeaderCommitPhase::Preview { message, error, .. } = &mut self.phase {
            message.push(ch);
            *error = None;
        }
    }

    pub fn backspace_message(&mut self) {
        if let HeaderCommitPhase::Preview { message, error, .. } = &mut self.phase {
            message.pop();
            *error = None;
        }
    }

    pub fn request_confirm(&mut self) -> Option<String> {
        let HeaderCommitPhase::Preview { message, error, .. } = &mut self.phase else {
            return None;
        };
        let trimmed = message.trim();
        if trimmed.is_empty() {
            *error = Some("Commit message is required.".to_string());
            return None;
        }
        let message = trimmed.to_string();
        self.mutate_request_id = None;
        self.phase = HeaderCommitPhase::Confirming {
            message: message.clone(),
        };
        Some(message)
    }

    pub fn confirmed_intent(&self) -> Option<TaskGitMutateIntent> {
        match &self.phase {
            HeaderCommitPhase::Confirming { message } => Some(TaskGitMutateIntent::Commit {
                message: message.clone(),
            }),
            _ => None,
        }
    }

    pub fn confirmed_selector(&self) -> Option<&TaskRepositorySelector> {
        match &self.phase {
            HeaderCommitPhase::Confirming { .. } => self.selector.as_ref(),
            _ => None,
        }
    }

    /// Keep the confirming phase visible until the host correlates success/error.
    pub fn mark_dispatched(&mut self) {
        // Intentionally retain Confirming so the overlay does not hide the outcome.
    }

    /// Complete only for the exact confirmed mutate command plus matching projection.
    pub fn complete_from_command(
        &mut self,
        projection: &TaskGitProjection,
        request_id: RequestId,
        command_task_id: TaskId,
        query: &TaskCockpitQuery,
    ) -> bool {
        if !self.correlates_mutation_command(request_id, command_task_id, query) {
            return false;
        }
        self.complete_if_correlated(projection)
    }

    /// Complete only when the Git response matches the captured task and selector.
    pub fn complete_if_correlated(&mut self, projection: &TaskGitProjection) -> bool {
        if !matches!(self.phase, HeaderCommitPhase::Confirming { .. }) {
            return false;
        }
        if self.task_id != Some(projection.task_id) {
            return false;
        }
        let Some(expected) = self.selector.as_ref() else {
            return false;
        };
        if !git_projection_matches_selector(projection, expected) {
            return false;
        }
        let repository = self
            .repository_label
            .clone()
            .or_else(|| projection.label.clone())
            .unwrap_or_else(|| "Repository".into());
        self.succeed(format!(
            "Committed on {} ({})",
            projection.branch.clone().unwrap_or_else(|| "HEAD".into()),
            repository
        ));
        true
    }

    /// Fail LoadingStatus/Confirming only for the exact commit status/mutate command.
    pub fn fail_from_command(
        &mut self,
        request_id: RequestId,
        command_task_id: TaskId,
        query: &TaskCockpitQuery,
        message: impl Into<String>,
    ) -> bool {
        if !self.correlates_failure_command(request_id, command_task_id, query) {
            return false;
        }
        self.fail(message);
        true
    }

    pub fn succeed(&mut self, message: impl Into<String>) {
        self.clear_request_fence();
        self.phase = HeaderCommitPhase::Success {
            message: message.into(),
        };
    }

    pub fn cancel(&mut self) {
        *self = Self::default();
    }

    pub fn fail(&mut self, message: impl Into<String>) {
        self.clear_request_fence();
        self.phase = HeaderCommitPhase::Error(message.into());
    }

    pub fn overlay_repository_label(&self) -> &str {
        self.repository_label.as_deref().unwrap_or("Repository")
    }

    fn clear_request_fence(&mut self) {
        self.status_request_id = None;
        self.mutate_request_id = None;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeaderOpenTarget {
    FilesRoot,
    DiffPath { relative_path: String },
}

impl HeaderOpenTarget {
    pub fn files_list_directory(&self) -> Option<String> {
        match self {
            Self::FilesRoot => None,
            Self::DiffPath { relative_path } => {
                let parent = relative_path
                    .rsplit_once('/')
                    .map(|(parent, _)| parent.to_string());
                parent.filter(|value| !value.is_empty())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(
        task_id: TaskId,
        selector: TaskRepositorySelector,
        label: &str,
        change_count: u32,
    ) -> TaskGitProjection {
        TaskGitProjection {
            task_id,
            selector: Some(selector),
            label: Some(label.into()),
            branch: Some("main".into()),
            ahead: 0,
            behind: 0,
            change_count,
            detached: false,
            entries: Vec::new(),
        }
    }

    #[test]
    fn commit_never_mutates_without_explicit_confirmation() {
        let mut workflow = HeaderCommitWorkflow::default();
        let task_id = TaskId::new();
        workflow.begin(task_id, TaskRepositorySelector::Workspace, "Workspace");
        workflow.apply_status(&status(
            task_id,
            TaskRepositorySelector::Workspace,
            "Workspace",
            2,
        ));
        assert!(workflow.confirmed_intent().is_none());
        workflow.push_message_char('f');
        workflow.push_message_char('i');
        workflow.backspace_message();
        workflow.push_message_char('x');
        assert!(workflow.confirmed_intent().is_none());
        let message = workflow.request_confirm().expect("confirm");
        assert_eq!(message, "fx");
        workflow.mark_dispatched();
        assert!(matches!(
            workflow.phase,
            HeaderCommitPhase::Confirming { .. }
        ));
        assert_eq!(
            workflow.confirmed_intent(),
            Some(TaskGitMutateIntent::Commit {
                message: "fx".into()
            })
        );
        assert_eq!(
            workflow.confirmed_selector(),
            Some(&TaskRepositorySelector::Workspace)
        );
        workflow.succeed("Committed fx");
        assert!(matches!(workflow.phase, HeaderCommitPhase::Success { .. }));
    }

    #[test]
    fn empty_status_and_empty_message_surface_errors() {
        let mut workflow = HeaderCommitWorkflow::default();
        let task_id = TaskId::new();
        workflow.begin(task_id, TaskRepositorySelector::Workspace, "Workspace");
        workflow.apply_status(&status(
            task_id,
            TaskRepositorySelector::Workspace,
            "Workspace",
            0,
        ));
        assert!(matches!(workflow.phase, HeaderCommitPhase::Error(_)));
        workflow.begin(task_id, TaskRepositorySelector::Workspace, "Workspace");
        workflow.apply_status(&status(
            task_id,
            TaskRepositorySelector::Workspace,
            "Workspace",
            1,
        ));
        assert!(workflow.request_confirm().is_none());
    }

    #[test]
    fn selector_drift_does_not_retarget_or_complete_in_flight_commit() {
        let mut workflow = HeaderCommitWorkflow::default();
        let task_id = TaskId::new();
        let captured = TaskRepositorySelector::Folder {
            folder_config_id: "sibling-a".into(),
        };
        workflow.begin(task_id, captured.clone(), "Sibling A");
        workflow.apply_status(&status(task_id, captured.clone(), "Sibling A", 2));
        workflow.push_message_char('x');
        assert!(workflow.request_confirm().is_some());
        workflow.mark_dispatched();

        let drifted = status(task_id, TaskRepositorySelector::Workspace, "Workspace", 9);
        assert!(!workflow.complete_if_correlated(&drifted));
        assert!(matches!(
            workflow.phase,
            HeaderCommitPhase::Confirming { .. }
        ));
        assert_eq!(workflow.confirmed_selector(), Some(&captured));

        // Status preview for a different selector must not rewrite the capture.
        workflow.apply_status(&drifted);
        assert_eq!(workflow.selector.as_ref(), Some(&captured));
        assert_eq!(workflow.overlay_repository_label(), "Sibling A");

        assert!(workflow.complete_if_correlated(&status(
            task_id,
            captured.clone(),
            "Sibling A",
            2,
        )));
        match &workflow.phase {
            HeaderCommitPhase::Success { message } => assert!(message.contains("Sibling A")),
            other => panic!("expected success, got {other:?}"),
        }
    }

    #[test]
    fn read_only_and_unavailable_block_commit_with_visible_reason() {
        let mut workflow = HeaderCommitWorkflow::default();
        workflow.begin_blocked(PanelDisabledReason::RepositoryReadOnly);
        assert_eq!(
            workflow.phase,
            HeaderCommitPhase::Error("Repository is read-only".into())
        );
        workflow.begin_blocked(PanelDisabledReason::RepositoryUnavailable);
        assert_eq!(
            workflow.phase,
            HeaderCommitPhase::Error("Repository is unavailable".into())
        );
    }

    #[test]
    fn duplicate_same_shape_different_request_id_cannot_apply_or_complete() {
        let mut workflow = HeaderCommitWorkflow::default();
        let task_id = TaskId::new();
        let captured = TaskRepositorySelector::Folder {
            folder_config_id: "sibling-a".into(),
        };
        let status_id = RequestId::new();
        let other_status_id = RequestId::new();
        workflow.begin(task_id, captured.clone(), "Sibling A");
        workflow.bind_status_request(status_id);

        let status_query = TaskCockpitQuery::GitStatusTargeted {
            selector: captured.clone(),
        };
        workflow.apply_status_from_command(
            &status(task_id, captured.clone(), "Sibling A", 2),
            other_status_id,
            task_id,
            &status_query,
        );
        assert!(matches!(workflow.phase, HeaderCommitPhase::LoadingStatus));

        workflow.apply_status_from_command(
            &status(task_id, captured.clone(), "Sibling A", 2),
            status_id,
            task_id,
            &status_query,
        );
        assert!(matches!(workflow.phase, HeaderCommitPhase::Preview { .. }));
        workflow.push_message_char('x');
        assert!(workflow.request_confirm().is_some());

        let mutate_id = RequestId::new();
        let other_mutate_id = RequestId::new();
        workflow.bind_mutate_request(mutate_id);
        let mutate = TaskCockpitQuery::GitMutateTargeted {
            selector: captured.clone(),
            intent: TaskGitMutateIntent::Commit {
                message: "x".into(),
            },
            confirm: true,
        };
        assert!(!workflow.complete_from_command(
            &status(task_id, captured.clone(), "Sibling A", 2),
            other_mutate_id,
            task_id,
            &mutate,
        ));
        assert!(matches!(
            workflow.phase,
            HeaderCommitPhase::Confirming { .. }
        ));
        assert!(!workflow.complete_from_command(
            &status(task_id, captured.clone(), "Sibling A", 2),
            status_id,
            task_id,
            &TaskCockpitQuery::GitStatusTargeted {
                selector: captured.clone(),
            },
        ));
        assert!(workflow.complete_from_command(
            &status(task_id, captured.clone(), "Sibling A", 2),
            mutate_id,
            task_id,
            &mutate,
        ));
        assert!(matches!(workflow.phase, HeaderCommitPhase::Success { .. }));
    }

    #[test]
    fn same_selector_status_refresh_cannot_complete_confirming_but_mutation_can() {
        let mut workflow = HeaderCommitWorkflow::default();
        let task_id = TaskId::new();
        let captured = TaskRepositorySelector::Folder {
            folder_config_id: "sibling-a".into(),
        };
        let status_id = RequestId::new();
        let mutate_id = RequestId::new();
        workflow.begin(task_id, captured.clone(), "Sibling A");
        workflow.bind_status_request(status_id);
        workflow.apply_status_from_command(
            &status(task_id, captured.clone(), "Sibling A", 2),
            status_id,
            task_id,
            &TaskCockpitQuery::GitStatusTargeted {
                selector: captured.clone(),
            },
        );
        workflow.push_message_char('x');
        assert!(workflow.request_confirm().is_some());
        workflow.bind_mutate_request(mutate_id);
        workflow.mark_dispatched();

        let refresh = TaskCockpitQuery::GitStatusTargeted {
            selector: captured.clone(),
        };
        assert!(!workflow.complete_from_command(
            &status(task_id, captured.clone(), "Sibling A", 2),
            status_id,
            task_id,
            &refresh,
        ));
        assert!(matches!(
            workflow.phase,
            HeaderCommitPhase::Confirming { .. }
        ));

        let mutate = TaskCockpitQuery::GitMutateTargeted {
            selector: captured.clone(),
            intent: TaskGitMutateIntent::Commit {
                message: "x".into(),
            },
            confirm: true,
        };
        assert!(workflow.complete_from_command(
            &status(task_id, captured.clone(), "Sibling A", 2),
            mutate_id,
            task_id,
            &mutate,
        ));
        assert!(matches!(workflow.phase, HeaderCommitPhase::Success { .. }));
    }

    #[test]
    fn loading_and_confirming_fail_only_for_exact_request_id_commands() {
        let mut workflow = HeaderCommitWorkflow::default();
        let task_id = TaskId::new();
        let captured = TaskRepositorySelector::Workspace;
        let status_id = RequestId::new();
        let other_id = RequestId::new();
        workflow.begin(task_id, captured.clone(), "Workspace");
        workflow.bind_status_request(status_id);

        assert!(!workflow.fail_from_command(
            other_id,
            task_id,
            &TaskCockpitQuery::GitStatusTargeted {
                selector: captured.clone(),
            },
            "wrong request",
        ));
        assert!(!workflow.fail_from_command(
            status_id,
            task_id,
            &TaskCockpitQuery::GitRepositories,
            "unrelated catalog failure",
        ));
        assert!(matches!(workflow.phase, HeaderCommitPhase::LoadingStatus));

        assert!(workflow.fail_from_command(
            status_id,
            task_id,
            &TaskCockpitQuery::GitStatusTargeted {
                selector: captured.clone(),
            },
            "status unavailable",
        ));
        assert_eq!(
            workflow.phase,
            HeaderCommitPhase::Error("status unavailable".into())
        );

        let mutate_id = RequestId::new();
        workflow.begin(task_id, captured.clone(), "Workspace");
        workflow.bind_status_request(status_id);
        workflow.apply_status_from_command(
            &status(task_id, captured.clone(), "Workspace", 1),
            status_id,
            task_id,
            &TaskCockpitQuery::GitStatusTargeted {
                selector: captured.clone(),
            },
        );
        workflow.push_message_char('m');
        assert!(workflow.request_confirm().is_some());
        workflow.bind_mutate_request(mutate_id);
        assert!(!workflow.fail_from_command(
            status_id,
            task_id,
            &TaskCockpitQuery::GitStatusTargeted {
                selector: captured.clone(),
            },
            "status refresh failure",
        ));
        assert!(matches!(
            workflow.phase,
            HeaderCommitPhase::Confirming { .. }
        ));
        assert!(workflow.fail_from_command(
            mutate_id,
            task_id,
            &TaskCockpitQuery::GitMutateTargeted {
                selector: captured,
                intent: TaskGitMutateIntent::Commit {
                    message: "m".into(),
                },
                confirm: true,
            },
            "mutate denied",
        ));
        assert_eq!(
            workflow.phase,
            HeaderCommitPhase::Error("mutate denied".into())
        );
    }

    #[test]
    fn legacy_workspace_shims_correlate_only_for_workspace_capture_with_request_id() {
        let mut workflow = HeaderCommitWorkflow::default();
        let task_id = TaskId::new();
        let folder = TaskRepositorySelector::Folder {
            folder_config_id: "sibling-a".into(),
        };
        let request_id = RequestId::new();
        workflow.begin(task_id, folder.clone(), "Sibling A");
        workflow.bind_status_request(request_id);
        assert!(!workflow.correlates_status_command(
            request_id,
            task_id,
            &TaskCockpitQuery::GitStatus
        ));
        workflow.apply_status_from_command(
            &status(task_id, folder.clone(), "Sibling A", 1),
            request_id,
            task_id,
            &TaskCockpitQuery::GitStatus,
        );
        assert!(matches!(workflow.phase, HeaderCommitPhase::LoadingStatus));

        workflow.begin(task_id, TaskRepositorySelector::Workspace, "Workspace");
        workflow.bind_status_request(request_id);
        assert!(workflow.correlates_status_command(
            request_id,
            task_id,
            &TaskCockpitQuery::GitStatus
        ));
    }
}
