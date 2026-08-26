//! Behavioral regression helpers for native UX production seams.

#[cfg(test)]
mod tests {
    use crate::domain::cockpit::TaskCockpitQuery;
    use crate::domain::id::{ProjectId, RequestId};
    use crate::domain::{TaskGitMutateIntent, TaskGitProjection, TaskRepositorySelector};
    use crate::ui::header_actions::{HeaderCommitPhase, HeaderCommitWorkflow};
    use crate::ui::native_composer::{
        apply_suggestion, detect_trigger, filter_suggestions, ComposerCursor, PromptDocument,
        PromptSegment, TriggerKind, TriggerSuggestion,
    };
    use crate::ui::project_actions::{
        ProjectActionDraft, ProjectActionEditorField, ProjectActionMenuMode, ProjectActionWorkflow,
    };
    use crate::ui::project_scope::ProjectScope;
    use crate::ui::task_cockpit::panel::PanelDisabledReason;

    #[test]
    fn composer_at_and_slash_apply_through_closed_segments() {
        let mut document = PromptDocument::from_plain_text(None, "see @ma");
        let cursor =
            ComposerCursor::from_expanded(&document, document.serialize_provider_text().len());
        let trigger = detect_trigger(&document, cursor).expect("file trigger");
        assert_eq!(trigger.kind, TriggerKind::File);
        let candidates = vec![TriggerSuggestion {
            label: "main.rs".into(),
            insert: PromptSegment::FileRef {
                relative_path: "src/main.rs".into(),
                is_directory: false,
            },
        }];
        let filtered = filter_suggestions(TriggerKind::File, "ma", &candidates);
        assert!(apply_suggestion(&mut document, &trigger, filtered[0]));
        assert_eq!(document.serialize_provider_text(), "see @src/main.rs");

        let mut document = PromptDocument::from_plain_text(None, "/hel");
        let cursor =
            ComposerCursor::from_expanded(&document, document.serialize_provider_text().len());
        let trigger = detect_trigger(&document, cursor).expect("slash");
        assert_eq!(trigger.kind, TriggerKind::Command);
    }

    #[test]
    fn project_action_editor_and_run_menu_are_keyboard_complete() {
        let mut draft = ProjectActionDraft::new_for_project("p", "f");
        draft.push_char('A');
        draft.focus_field(ProjectActionEditorField::Command);
        draft.push_char('c');
        draft.push_char('m');
        draft.backspace();
        assert_eq!(draft.command, "c");
        assert!(draft.validate());
        let mut workflow = ProjectActionWorkflow::default();
        workflow.open_menu(ProjectId::new());
        workflow.begin_add("p".into(), "f".into());
        assert!(matches!(workflow.mode, ProjectActionMenuMode::Editor(_)));
        workflow.cancel_editor();
        assert!(matches!(workflow.mode, ProjectActionMenuMode::Menu { .. }));
    }

    #[test]
    fn commit_keeps_confirming_until_exact_mutate_request_id() {
        let mut workflow = HeaderCommitWorkflow::default();
        let task_id = crate::domain::id::TaskId::new();
        let selector = TaskRepositorySelector::Folder {
            folder_config_id: "sibling-a".into(),
        };
        let status_id = RequestId::new();
        let mutate_id = RequestId::new();
        let other_id = RequestId::new();
        workflow.begin(task_id, selector.clone(), "Sibling A");
        workflow.bind_status_request(status_id);
        workflow.apply_status_from_command(
            &TaskGitProjection {
                task_id,
                selector: Some(selector.clone()),
                label: Some("Sibling A".into()),
                branch: Some("main".into()),
                ahead: 0,
                behind: 0,
                change_count: 1,
                detached: false,
            },
            status_id,
            task_id,
            &TaskCockpitQuery::GitStatusTargeted {
                selector: selector.clone(),
            },
        );
        workflow.push_message_char('x');
        assert!(workflow.request_confirm().is_some());
        workflow.bind_mutate_request(mutate_id);
        workflow.mark_dispatched();
        assert!(matches!(
            workflow.phase,
            HeaderCommitPhase::Confirming { .. }
        ));

        let projection = TaskGitProjection {
            task_id,
            selector: Some(selector.clone()),
            label: Some("Sibling A".into()),
            branch: Some("main".into()),
            ahead: 0,
            behind: 0,
            change_count: 1,
            detached: false,
        };
        // Duplicate same-shape mutate with another request id must not complete.
        assert!(!workflow.complete_from_command(
            &projection,
            other_id,
            task_id,
            &TaskCockpitQuery::GitMutateTargeted {
                selector: selector.clone(),
                intent: TaskGitMutateIntent::Commit {
                    message: "x".into(),
                },
                confirm: true,
            },
        ));
        assert!(!workflow.complete_from_command(
            &projection,
            status_id,
            task_id,
            &TaskCockpitQuery::GitStatusTargeted {
                selector: selector.clone(),
            },
        ));
        assert!(workflow.complete_from_command(
            &projection,
            mutate_id,
            task_id,
            &TaskCockpitQuery::GitMutateTargeted {
                selector,
                intent: TaskGitMutateIntent::Commit {
                    message: "x".into(),
                },
                confirm: true,
            },
        ));
        assert!(matches!(workflow.phase, HeaderCommitPhase::Success { .. }));
    }

    #[test]
    fn commit_loading_rejects_duplicate_status_request_id() {
        let mut workflow = HeaderCommitWorkflow::default();
        let task_id = crate::domain::id::TaskId::new();
        let selector = TaskRepositorySelector::Workspace;
        let status_id = RequestId::new();
        let other_id = RequestId::new();
        workflow.begin(task_id, selector.clone(), "Workspace");
        workflow.bind_status_request(status_id);
        let query = TaskCockpitQuery::GitStatusTargeted {
            selector: selector.clone(),
        };
        workflow.apply_status_from_command(
            &TaskGitProjection {
                task_id,
                selector: Some(selector.clone()),
                label: Some("Workspace".into()),
                branch: Some("main".into()),
                ahead: 0,
                behind: 0,
                change_count: 1,
                detached: false,
            },
            other_id,
            task_id,
            &query,
        );
        assert!(matches!(workflow.phase, HeaderCommitPhase::LoadingStatus));
        assert!(!workflow.fail_from_command(other_id, task_id, &query, "wrong id"));
        assert!(workflow.fail_from_command(status_id, task_id, &query, "status denied"));
        assert_eq!(
            workflow.phase,
            HeaderCommitPhase::Error("status denied".into())
        );
    }

    #[test]
    fn commit_blocks_read_only_repository_with_visible_reason() {
        let mut workflow = HeaderCommitWorkflow::default();
        workflow.begin_blocked(PanelDisabledReason::RepositoryReadOnly);
        assert_eq!(
            workflow.phase,
            HeaderCommitPhase::Error("Repository is read-only".into())
        );
    }

    #[test]
    fn project_scope_project_variant_is_distinct_from_all() {
        let project_id = ProjectId::new();
        assert_ne!(ProjectScope::All, ProjectScope::Project(project_id));
        assert_eq!(
            ProjectScope::Project(project_id).validated(&[project_id]),
            ProjectScope::Project(project_id)
        );
    }
}
