use devmanager::browser::{
    apply_browser_workflow_review_mutation, browser_action_plan,
    browser_workflow_review_editor_for_field, browser_workflow_review_editor_mutation,
    browser_workflow_review_projection, discard_browser_workflow_review,
    preview_browser_workflow_review, save_browser_workflow_review, BrowserPaneAction,
    BrowserPaneContext, BrowserPaneModel, BrowserPaneSurface, BrowserPaneTransient,
    BrowserRecipeAssertion, BrowserRecipeInput, BrowserRecipeInputKind, BrowserRecipeLocator,
    BrowserRecipeValue, BrowserRecipeViewport, BrowserRecipeWait, BrowserRecordingAction,
    BrowserRecordingActor, BrowserRecordingError, BrowserRisk, BrowserWebViewHost,
    BrowserWorkflowCoordinator, BrowserWorkflowReviewAssertionKind, BrowserWorkflowReviewEditor,
    BrowserWorkflowReviewEditorField, BrowserWorkflowReviewMutation,
    BrowserWorkflowReviewProjection, BrowserWorkflowReviewUiState, BrowserWorkspaceKey,
    BrowserWorkspaceSnapshot,
};
use std::time::{SystemTime, UNIX_EPOCH};

fn workspace(project: &str, tab: &str) -> BrowserWorkspaceKey {
    BrowserWorkspaceKey::new(project, tab).expect("valid browser workspace")
}

#[test]
fn workflow_review_mutations_are_exact_typed_and_refresh_the_safe_projection() {
    let coordinator = BrowserWorkflowCoordinator::default();
    let owned = workspace("project-a", "ai-a");
    let other = workspace("project-a", "ai-b");
    let instance = coordinator.start(owned.clone()).expect("start recording");

    for (actor, action) in [
        (
            BrowserRecordingActor::User,
            BrowserRecordingAction::type_text(locator(), "query").expect("text action"),
        ),
        (
            BrowserRecordingActor::Agent,
            BrowserRecordingAction::navigate("https://example.test/results")
                .expect("navigation action"),
        ),
        (
            BrowserRecordingActor::User,
            BrowserRecordingAction::recipe(devmanager::browser::BrowserRecipeAction::Click {
                locator: locator(),
            })
            .expect("click action"),
        ),
    ] {
        let reservation = coordinator
            .reserve_on(&instance, actor, "tab-a", BrowserRisk::Normal)
            .expect("reserve action");
        coordinator
            .commit(reservation, action)
            .expect("commit action");
    }
    coordinator.stop(&instance).expect("stop into review");

    let apply = |mutation| {
        apply_browser_workflow_review_mutation(
            &coordinator,
            Some(&owned),
            &owned,
            BrowserPaneSurface::Claude,
            instance.id(),
            mutation,
        )
        .expect("valid review mutation")
    };

    apply(BrowserWorkflowReviewMutation::SetMetadata {
        id: "search-flow".to_string(),
        name: "Search flow".to_string(),
        description: "Searches safely".to_string(),
        start_url: "https://example.test".to_string(),
        viewport: BrowserRecipeViewport {
            width: 1280,
            height: 720,
            scale_percent: 100,
        },
    });
    apply(BrowserWorkflowReviewMutation::DeleteStep {
        step_id: "step-3".to_string(),
    });
    apply(BrowserWorkflowReviewMutation::MoveStep {
        step_id: "step-2".to_string(),
        new_index: 0,
    });
    apply(BrowserWorkflowReviewMutation::ConvertActionValueToInput {
        step_id: "step-2".to_string(),
        input_name: "destination".to_string(),
        kind: BrowserRecipeInputKind::Url,
    });
    apply(BrowserWorkflowReviewMutation::ConvertActionValueToInput {
        step_id: "step-1".to_string(),
        input_name: "query".to_string(),
        kind: BrowserRecipeInputKind::Text,
    });
    apply(BrowserWorkflowReviewMutation::RenameInput {
        previous_name: "query".to_string(),
        new_name: "search_text".to_string(),
    });
    apply(BrowserWorkflowReviewMutation::SetInputDefault {
        input_name: "search_text".to_string(),
        default_value: Some("updated".to_string()),
    });
    apply(BrowserWorkflowReviewMutation::AddInput {
        input: BrowserRecipeInput {
            name: "optional_text".to_string(),
            kind: BrowserRecipeInputKind::Text,
            default_value: Some("safe".to_string()),
        },
    });
    apply(BrowserWorkflowReviewMutation::RemoveInput {
        input_name: "optional_text".to_string(),
    });
    apply(BrowserWorkflowReviewMutation::SetStepWait {
        step_id: "step-2".to_string(),
        wait: Some(BrowserRecipeWait::Load { timeout_ms: 2_000 }),
    });
    apply(BrowserWorkflowReviewMutation::AddStepAssertion {
        step_id: "step-2".to_string(),
        assertion: BrowserRecipeAssertion::Url {
            value: BrowserRecipeValue::Input {
                name: "destination".to_string(),
            },
            exact: true,
        },
    });
    let refreshed = apply(BrowserWorkflowReviewMutation::RemoveStepAssertion {
        step_id: "step-2".to_string(),
        assertion_index: 0,
    });

    assert_eq!(refreshed.steps.len(), 2);
    let metadata = refreshed
        .metadata
        .as_ref()
        .expect("review metadata controls");
    assert_eq!(metadata.id, "search-flow");
    assert_eq!(metadata.name, "Search flow");
    assert_eq!(metadata.description, "Searches safely");
    assert_eq!(metadata.start_url, "https://example.test");
    assert_eq!(metadata.viewport.width, 1280);
    assert_eq!(refreshed.steps[0].id, "step-2");
    assert_eq!(refreshed.steps[0].index, 0);
    assert_eq!(
        refreshed.steps[0].convertible_kind,
        Some(BrowserRecipeInputKind::Url)
    );
    assert!(refreshed.steps[0].has_wait);
    assert_eq!(refreshed.steps[0].assertion_count, 0);
    assert_eq!(refreshed.steps[1].index, 1);
    assert_eq!(
        refreshed.steps[1].convertible_kind,
        Some(BrowserRecipeInputKind::Text)
    );
    assert_eq!(
        refreshed
            .inputs
            .iter()
            .map(|input| (input.name.as_str(), input.kind))
            .collect::<Vec<_>>(),
        vec![
            ("destination", BrowserRecipeInputKind::Url),
            ("search_text", BrowserRecipeInputKind::Text),
        ]
    );

    assert_eq!(
        apply_browser_workflow_review_mutation(
            &coordinator,
            Some(&other),
            &owned,
            BrowserPaneSurface::Claude,
            instance.id(),
            BrowserWorkflowReviewMutation::DeleteStep {
                step_id: "step-1".to_string(),
            },
        )
        .unwrap_err(),
        BrowserRecordingError::InvalidMutation
    );
    assert_eq!(
        apply_browser_workflow_review_mutation(
            &coordinator,
            Some(&owned),
            &owned,
            BrowserPaneSurface::Server,
            instance.id(),
            BrowserWorkflowReviewMutation::DeleteStep {
                step_id: "step-1".to_string(),
            },
        )
        .unwrap_err(),
        BrowserRecordingError::InvalidMutation
    );
}

fn locator() -> BrowserRecipeLocator {
    BrowserRecipeLocator {
        test_id: Some("safe-field".to_string()),
        ..BrowserRecipeLocator::default()
    }
}

fn temporary_project(label: &str) -> std::path::PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "devmanager-browser-review-{label}-{}-{nonce}",
        std::process::id()
    ));
    std::fs::create_dir_all(&path).expect("create temporary project");
    path
}

fn stopped_navigation_review(
    coordinator: &BrowserWorkflowCoordinator,
    workspace_key: &BrowserWorkspaceKey,
) -> u64 {
    let instance = coordinator
        .start(workspace_key.clone())
        .expect("start recording");
    let reservation = coordinator
        .reserve_on(
            &instance,
            BrowserRecordingActor::User,
            "tab-a",
            BrowserRisk::Normal,
        )
        .expect("reserve navigation");
    coordinator
        .commit(
            reservation,
            BrowserRecordingAction::navigate("https://example.test/start")
                .expect("safe navigation"),
        )
        .expect("commit navigation");
    coordinator.stop(&instance).expect("stop into review");
    apply_browser_workflow_review_mutation(
        coordinator,
        Some(workspace_key),
        workspace_key,
        BrowserPaneSurface::Claude,
        instance.id(),
        BrowserWorkflowReviewMutation::SetMetadata {
            id: "saved-flow".to_string(),
            name: "Saved flow".to_string(),
            description: "A safe saved workflow".to_string(),
            start_url: "https://example.test/start".to_string(),
            viewport: BrowserRecipeViewport::default(),
        },
    )
    .expect("valid metadata");
    instance.id()
}

#[test]
fn pane_model_and_actions_expose_explicit_record_stop_and_review_controls() {
    let coordinator = BrowserWorkflowCoordinator::default();
    let owned = workspace("project-a", "ai-a");
    let projection =
        browser_workflow_review_projection(&coordinator, &owned, BrowserPaneSurface::Claude)
            .expect("inactive projection");
    let context = BrowserPaneContext {
        browser_enabled: true,
        platform_supported: true,
        active_surface: Some(BrowserPaneSurface::Claude),
        editor_open: false,
        modal_open: false,
    };
    let snapshot = BrowserWorkspaceSnapshot::default();
    let model = BrowserPaneModel::new(
        owned.clone(),
        &context,
        &snapshot,
        BrowserPaneTransient {
            workflow_review: Some(projection),
            ..BrowserPaneTransient::default()
        },
    );
    assert_eq!(
        model
            .workflow_review
            .as_ref()
            .expect("recording controls")
            .state,
        BrowserWorkflowReviewUiState::Inactive
    );

    for action in [
        BrowserPaneAction::StartRecording,
        BrowserPaneAction::StopRecording { instance_id: 7 },
        BrowserPaneAction::PreviewRecordingReview { instance_id: 7 },
        BrowserPaneAction::SaveRecordingReview { instance_id: 7 },
        BrowserPaneAction::DiscardRecordingReview { instance_id: 7 },
    ] {
        let plan = browser_action_plan(Some(&owned), Some(&snapshot), "", action)
            .expect("review actions are exact pane-local plans");
        assert!(plan.commands.is_empty());
        assert!(plan.diagnostic.is_none());
    }

    let source = std::fs::read_to_string("src/browser/pane.rs").expect("pane source");
    assert!(!source.contains("Not available until browser automation is initialized"));
    assert!(!source.contains("ToggleRecording"));
}

#[test]
fn native_review_surface_reaches_every_required_safe_edit_and_terminal_action() {
    let pane_source = std::fs::read_to_string("src/browser/pane.rs").expect("pane source");
    let review_start = pane_source
        .find("let workflow_review_panel")
        .expect("review surface start");
    let review_end = pane_source[review_start..]
        .find("let page_surface")
        .map(|offset| review_start + offset)
        .expect("review surface end");
    let review = &pane_source[review_start..review_end];

    for required in [
        "BrowserWorkflowReviewEditorField::Name",
        "BrowserWorkflowReviewEditorField::Id",
        "BrowserWorkflowReviewEditorField::Description",
        "BrowserWorkflowReviewEditorField::StartUrl",
        "BrowserWorkflowReviewMutation::SetMetadata",
        "BrowserWorkflowReviewMutation::DeleteStep",
        "BrowserWorkflowReviewMutation::MoveStep",
        "BrowserWorkflowReviewMutation::ConvertActionValueToInput",
        "BrowserWorkflowReviewMutation::AddInput",
        "BrowserRecipeInputKind::Text",
        "BrowserRecipeInputKind::Url",
        "BrowserRecipeInputKind::File",
        "BrowserRecipeInputKind::Secret",
        "BrowserWorkflowReviewEditorField::InputName",
        "BrowserWorkflowReviewEditorField::InputDefault",
        "BrowserWorkflowReviewMutation::RemoveInput",
        "BrowserRecipeWait::Duration",
        "BrowserRecipeWait::Load",
        "BrowserRecipeWait::NetworkIdle",
        "BrowserWorkflowReviewMutation::SetStepWait",
        "BrowserWorkflowReviewEditorField::Assertion",
        "BrowserWorkflowReviewAssertionKind::Url",
        "BrowserWorkflowReviewAssertionKind::Title",
        "BrowserWorkflowReviewAssertionKind::Text",
        "BrowserWorkflowReviewAssertionKind::Element",
        "BrowserWorkflowReviewAssertionKind::Value",
        "BrowserWorkflowReviewMutation::AddStepAssertionDraft",
        "BrowserWorkflowReviewMutation::RemoveStepAssertion",
        "BrowserPaneAction::PreviewRecordingReview",
        "BrowserPaneAction::SaveRecordingReview",
        "BrowserPaneAction::DiscardRecordingReview",
        ".overflow_y_scroll()",
    ] {
        assert!(review.contains(required), "review UI must reach {required}");
    }

    let app_source = std::fs::read_to_string("src/app/mod.rs").expect("App source");
    let editor_start = app_source
        .find("fn handle_browser_workflow_key")
        .expect("workflow editor handler");
    let editor_end = app_source[editor_start..]
        .find("fn apply_browser_settings_action")
        .map(|offset| editor_start + offset)
        .expect("workflow editor handler end");
    let editor = &app_source[editor_start..editor_end];
    for required in [
        "workflow_review_projection",
        "browser_workflow_review_editor_mutation",
    ] {
        assert!(
            editor.contains(required),
            "keyboard editor must commit {required}"
        );
    }
}

#[test]
fn native_host_exposes_the_one_coordinator_review_bridge() {
    let mut host = BrowserWebViewHost::unavailable("test host");
    let owned = workspace("project-a", "ai-a");
    let projection = host
        .workflow_review_projection(&owned, BrowserPaneSurface::Claude)
        .expect("AI projection");
    assert_eq!(projection.state, BrowserWorkflowReviewUiState::Inactive);
    assert!(host.page_recording_instance(&owned).is_none());
    assert!(host
        .apply_workflow_review_mutation(
            Some(&owned),
            &owned,
            BrowserPaneSurface::Claude,
            1,
            BrowserWorkflowReviewMutation::DeleteStep {
                step_id: "step-1".to_string(),
            },
        )
        .is_err());
    assert!(host
        .preview_workflow_review(Some(&owned), &owned, BrowserPaneSurface::Claude, 1,)
        .is_err());
    assert!(host
        .save_workflow_review(
            Some(&owned),
            &owned,
            BrowserPaneSurface::Claude,
            1,
            std::path::Path::new("."),
            true,
        )
        .is_err());
    assert!(host
        .discard_workflow_review(Some(&owned), &owned, BrowserPaneSurface::Claude, 1,)
        .is_err());
}

#[test]
fn native_shell_routes_explicit_review_actions_without_persisting_volatile_state() {
    let source = std::fs::read_to_string("src/app/mod.rs").expect("App source");
    let model_start = source
        .find("fn active_browser_model")
        .expect("model builder");
    let action_start = source
        .find("fn apply_browser_pane_action")
        .expect("pane action handler");
    let action_end = source[action_start..]
        .find("fn handle_browser_address_key")
        .map(|offset| action_start + offset)
        .expect("action handler end");
    let model = &source[model_start..action_start];
    let actions = &source[action_start..action_end];

    assert!(model.contains("workflow_review_projection"));
    assert!(actions.contains("BrowserPaneAction::StartRecording"));
    assert!(actions.contains("start_page_recording"));
    assert!(actions.contains("BrowserPaneAction::StopRecording"));
    assert!(actions.contains("stop_page_recording"));
    assert!(actions.contains("BrowserPaneAction::MutateRecordingReview"));
    assert!(actions.contains("apply_workflow_review_mutation"));
    assert!(actions.contains("BrowserPaneAction::PreviewRecordingReview"));
    assert!(actions.contains("preview_workflow_review"));
    assert!(actions.contains("BrowserPaneAction::SaveRecordingReview"));
    assert!(actions.contains("local_browser_workflow_project_root"));
    assert!(actions.contains("save_workflow_review"));
    assert!(actions.contains("BrowserPaneAction::DiscardRecordingReview"));
    assert!(actions.contains("discard_workflow_review"));
    assert!(actions.contains("self.remote_mode.is_some()"));
    let ui_state = source
        .find("struct BrowserWorkspaceUiState")
        .expect("volatile UI state");
    let ui_state_end = source[ui_state..]
        .find("struct BrowserDividerDrag")
        .map(|offset| ui_state + offset)
        .expect("volatile UI state end");
    assert!(source[ui_state..ui_state_end].contains("workflow_preview"));

    let app_state = std::fs::read_to_string("src/state/app_state.rs").expect("AppState source");
    assert!(!app_state.contains("workflow_preview"));
    assert!(!app_state.contains("BrowserRecordingReview"));
}

#[test]
fn review_preview_save_discard_are_atomic_local_and_exact() {
    let owned = workspace("project-a", "ai-a");
    let other = workspace("project-a", "ai-b");
    let project_root = temporary_project("save");
    let coordinator = BrowserWorkflowCoordinator::default();
    let instance_id = stopped_navigation_review(&coordinator, &owned);

    let mut preview = preview_browser_workflow_review(
        &coordinator,
        Some(&owned),
        &owned,
        BrowserPaneSurface::Claude,
        instance_id,
    )
    .expect("validated immutable preview");
    preview.name = "Changed clone".to_string();
    assert_eq!(
        preview_browser_workflow_review(
            &coordinator,
            Some(&owned),
            &owned,
            BrowserPaneSurface::Claude,
            instance_id,
        )
        .expect("fresh preview")
        .name,
        "Saved flow"
    );

    assert!(save_browser_workflow_review(
        &coordinator,
        Some(&other),
        &owned,
        BrowserPaneSurface::Claude,
        instance_id,
        &project_root,
        false,
    )
    .is_err());
    assert!(save_browser_workflow_review(
        &coordinator,
        Some(&owned),
        &owned,
        BrowserPaneSurface::Claude,
        instance_id,
        &project_root,
        true,
    )
    .is_err());
    assert!(!project_root.join(".devmanager").exists());

    let saved = save_browser_workflow_review(
        &coordinator,
        Some(&owned),
        &owned,
        BrowserPaneSurface::Claude,
        instance_id,
        &project_root,
        false,
    )
    .expect("atomic local save");
    assert_eq!(
        saved,
        project_root.join(".devmanager/browser-workflows/saved-flow.json")
    );
    assert!(std::fs::read_to_string(&saved)
        .expect("saved bytes")
        .ends_with('\n'));
    assert_eq!(
        browser_workflow_review_projection(&coordinator, &owned, BrowserPaneSurface::Claude,)
            .expect("inactive after save")
            .state,
        BrowserWorkflowReviewUiState::Inactive
    );

    let failure_root = temporary_project("failure").join("not-a-directory");
    std::fs::write(&failure_root, b"file").expect("hostile root file");
    let failed_coordinator = BrowserWorkflowCoordinator::default();
    let failed_instance = stopped_navigation_review(&failed_coordinator, &owned);
    assert!(save_browser_workflow_review(
        &failed_coordinator,
        Some(&owned),
        &owned,
        BrowserPaneSurface::Codex,
        failed_instance,
        &failure_root,
        false,
    )
    .is_err());
    assert!(matches!(
        browser_workflow_review_projection(&failed_coordinator, &owned, BrowserPaneSurface::Codex,)
            .expect("review retained after failed save")
            .state,
        BrowserWorkflowReviewUiState::Review { .. }
    ));
    discard_browser_workflow_review(
        &failed_coordinator,
        Some(&owned),
        &owned,
        BrowserPaneSurface::Codex,
        failed_instance,
    )
    .expect("discard exact retained review");
    assert!(matches!(
        browser_workflow_review_projection(&failed_coordinator, &owned, BrowserPaneSurface::Codex,)
            .expect("inactive after discard")
            .state,
        BrowserWorkflowReviewUiState::Inactive
    ));

    let _ = std::fs::remove_dir_all(project_root);
    let _ = std::fs::remove_dir_all(
        failure_root
            .parent()
            .expect("temporary failure project parent"),
    );
}

#[test]
fn workflow_review_projection_is_exact_ai_only_bounded_and_value_free() {
    const PRIVATE_TEXT: &str = "ordinary-text-sentinel";
    const PRIVATE_PATH: &str = r"C:\private\file-path-sentinel.txt";

    let coordinator = BrowserWorkflowCoordinator::default();
    let owned = workspace("project-a", "ai-a");
    let other = workspace("project-a", "ai-b");

    let inactive: BrowserWorkflowReviewProjection =
        browser_workflow_review_projection(&coordinator, &owned, BrowserPaneSurface::Claude)
            .expect("Claude workspace receives an inactive projection");
    assert_eq!(inactive.state, BrowserWorkflowReviewUiState::Inactive);
    assert!(inactive.steps.is_empty());
    assert!(inactive.inputs.is_empty());

    assert!(
        browser_workflow_review_projection(&coordinator, &owned, BrowserPaneSurface::Server,)
            .is_none()
    );
    assert!(
        browser_workflow_review_projection(&coordinator, &owned, BrowserPaneSurface::Ssh,)
            .is_none()
    );

    let instance = coordinator.start(owned.clone()).expect("start recording");
    let recording =
        browser_workflow_review_projection(&coordinator, &owned, BrowserPaneSurface::Codex)
            .expect("Codex workspace receives its recording projection");
    assert_eq!(
        recording.state,
        BrowserWorkflowReviewUiState::Recording {
            instance_id: instance.id(),
        }
    );

    let other_projection =
        browser_workflow_review_projection(&coordinator, &other, BrowserPaneSurface::Claude)
            .expect("another AI workspace remains independently inactive");
    assert_eq!(
        other_projection.state,
        BrowserWorkflowReviewUiState::Inactive
    );
    assert!(other_projection.steps.is_empty());
    assert!(other_projection.inputs.is_empty());

    let text = coordinator
        .reserve_on(
            &instance,
            BrowserRecordingActor::User,
            "tab-a",
            BrowserRisk::Normal,
        )
        .expect("reserve user text");
    coordinator
        .commit(
            text,
            BrowserRecordingAction::type_text(locator(), PRIVATE_TEXT)
                .expect("safe literal capture"),
        )
        .expect("commit user text");

    let password = coordinator
        .reserve_on(
            &instance,
            BrowserRecordingActor::Agent,
            "tab-a",
            BrowserRisk::AccountSecurity,
        )
        .expect("reserve agent password");
    coordinator
        .commit(
            password,
            BrowserRecordingAction::type_password(locator()).expect("password marker"),
        )
        .expect("commit agent password");

    coordinator.stop(&instance).expect("stop into review");
    let review =
        browser_workflow_review_projection(&coordinator, &owned, BrowserPaneSurface::Claude)
            .expect("stopped recording projects review state");
    assert_eq!(
        review.state,
        BrowserWorkflowReviewUiState::Review {
            instance_id: instance.id(),
        }
    );
    assert_eq!(review.steps.len(), 2);
    assert_eq!(review.steps[0].actor, BrowserRecordingActor::User);
    assert_eq!(review.steps[0].summary, "Type text");
    assert_eq!(review.steps[1].actor, BrowserRecordingActor::Agent);
    assert_eq!(review.steps[1].summary, "Type secret input");
    assert_eq!(review.inputs.len(), 1);
    assert_eq!(review.inputs[0].kind, BrowserRecipeInputKind::Secret);
    assert!(review.inputs[0].unset);

    let safe_debug = format!("{review:?}");
    assert!(!safe_debug.contains(PRIVATE_TEXT));
    assert!(!safe_debug.contains(PRIVATE_PATH));
    assert!(!safe_debug.contains("value"));
    assert!(!safe_debug.contains("path"));
}

#[test]
fn workflow_review_editor_is_volatile_and_redacted_while_actions_are_typed() {
    let editor = BrowserWorkflowReviewEditor {
        instance_id: 7,
        field: BrowserWorkflowReviewEditorField::Name,
        draft: "never-log-editor-sentinel".to_string(),
        cursor: 4,
        focused: true,
    };
    assert!(!format!("{editor:?}").contains("never-log-editor-sentinel"));

    let coordinator = BrowserWorkflowCoordinator::default();
    let owned = workspace("project-a", "ai-a");
    let model = BrowserPaneModel::new(
        owned.clone(),
        &BrowserPaneContext {
            browser_enabled: true,
            platform_supported: true,
            active_surface: Some(BrowserPaneSurface::Claude),
            editor_open: false,
            modal_open: false,
        },
        &BrowserWorkspaceSnapshot::default(),
        BrowserPaneTransient {
            workflow_review: browser_workflow_review_projection(
                &coordinator,
                &owned,
                BrowserPaneSurface::Claude,
            ),
            workflow_editor: Some(editor.clone()),
            ..BrowserPaneTransient::default()
        },
    );
    assert_eq!(model.workflow_editor, Some(editor));

    assert!(matches!(
        BrowserPaneAction::FocusRecordingReviewField {
            instance_id: 7,
            field: BrowserWorkflowReviewEditorField::InputDefault {
                input_name: "query".to_string(),
            },
        },
        BrowserPaneAction::FocusRecordingReviewField { .. }
    ));
    assert!(matches!(
        BrowserPaneAction::CancelRecordingReviewEdit,
        BrowserPaneAction::CancelRecordingReviewEdit
    ));

    let pane_source = std::fs::read_to_string("src/browser/pane.rs").expect("pane source");
    for structure in ["BrowserPaneTransient", "BrowserPaneModel"] {
        let declaration = pane_source
            .find(&format!("pub struct {structure}"))
            .expect("preview carrier declaration");
        let attributes = &pane_source[declaration.saturating_sub(80)..declaration];
        assert!(
            !attributes.contains("derive(Debug"),
            "{structure} must not debug captured preview text"
        );
    }
    let app_source = std::fs::read_to_string("src/app/mod.rs").expect("App source");
    let declaration = app_source
        .find("struct BrowserWorkspaceUiState")
        .expect("volatile App preview carrier");
    let attributes = &app_source[declaration.saturating_sub(80)..declaration];
    assert!(
        !attributes.contains("derive(Debug"),
        "volatile App state must not debug captured preview text"
    );
}

#[test]
fn coordinator_lifecycle_enumerates_recording_and_review_instances_by_project() {
    let coordinator = BrowserWorkflowCoordinator::default();
    let review_workspace = workspace("project-a", "ai-review");
    let recording_workspace = workspace("project-a", "ai-recording");
    let other_workspace = workspace("project-b", "ai-other");
    let review = coordinator
        .start(review_workspace.clone())
        .expect("review instance");
    coordinator.stop(&review).expect("stop into review");
    let recording = coordinator
        .start(recording_workspace.clone())
        .expect("recording instance");
    let other = coordinator
        .start(other_workspace.clone())
        .expect("other project instance");

    assert_eq!(
        coordinator
            .current_project_instances("project-a")
            .iter()
            .map(|instance| instance.workspace_key().ai_tab_id.as_str())
            .collect::<Vec<_>>(),
        vec!["ai-recording", "ai-review"]
    );
    assert_eq!(
        coordinator
            .current_project_instances("project-b")
            .iter()
            .map(|instance| instance.id())
            .collect::<Vec<_>>(),
        vec![other.id()]
    );
    assert_eq!(
        coordinator
            .current_instance(&recording_workspace)
            .expect("recording current")
            .id(),
        recording.id()
    );
}

#[test]
fn native_lifecycle_discards_volatile_workflow_state_on_route_or_destructive_changes() {
    let app = std::fs::read_to_string("src/app/mod.rs").expect("App source");
    assert!(app.contains("browser_workflow_route: Option<BrowserWorkspaceKey>"));
    let sync = app
        .find("fn sync_browser_host_visibility")
        .expect("visibility sync");
    let sync_end = app[sync..]
        .find("fn active_browser_model")
        .map(|offset| sync + offset)
        .expect("visibility sync end");
    let sync = &app[sync..sync_end];
    assert!(sync.contains("discard_workflow_state"));
    assert!(sync.contains("browser_workflow_route"));

    let windows = std::fs::read_to_string("src/browser/host/windows.rs").expect("host source");
    assert!(windows.contains("pub fn discard_workflow_state"));
    assert!(windows.contains("current_project_instances"));
    assert!(windows.contains("BrowserHostControl::InterruptWorkspace"));
    assert!(windows.contains("BrowserHostControl::InterruptProject"));
    let reset = windows
        .find("BrowserCommand::ResetWorkspace =>")
        .expect("reset");
    let reset_tail = &windows[reset..windows.len().min(reset + 900)];
    assert!(reset_tail.contains("discard_workflow_state"));
}
#[test]
fn invalid_review_metadata_remains_editable_and_can_be_repaired() {
    let coordinator = BrowserWorkflowCoordinator::default();
    let owned = workspace("project-repair", "ai-repair");
    let instance_id = stopped_navigation_review(&coordinator, &owned);
    let apply = |mutation| {
        apply_browser_workflow_review_mutation(
            &coordinator,
            Some(&owned),
            &owned,
            BrowserPaneSurface::Claude,
            instance_id,
            mutation,
        )
        .expect("draft mutation remains available")
    };

    apply(BrowserWorkflowReviewMutation::SetMetadata {
        id: "bad id".to_string(),
        name: String::new(),
        description: "repair me".to_string(),
        start_url: "https://example.test/start".to_string(),
        viewport: BrowserRecipeViewport {
            width: 0,
            height: 720,
            scale_percent: 100,
        },
    });
    assert!(preview_browser_workflow_review(
        &coordinator,
        Some(&owned),
        &owned,
        BrowserPaneSurface::Claude,
        instance_id,
    )
    .is_err());

    let projection =
        browser_workflow_review_projection(&coordinator, &owned, BrowserPaneSurface::Claude)
            .expect("invalid draft still has a safe projection");
    let mut name_editor = browser_workflow_review_editor_for_field(
        &projection,
        instance_id,
        BrowserWorkflowReviewEditorField::Name,
    )
    .expect("blank name remains editable");
    assert!(name_editor.draft.is_empty());
    name_editor.draft = "Repaired workflow".to_string();
    apply(
        browser_workflow_review_editor_mutation(&projection, &name_editor)
            .expect("name repair mutation"),
    );

    let projection =
        browser_workflow_review_projection(&coordinator, &owned, BrowserPaneSurface::Claude)
            .expect("partially repaired projection");
    let mut id_editor = browser_workflow_review_editor_for_field(
        &projection,
        instance_id,
        BrowserWorkflowReviewEditorField::Id,
    )
    .expect("invalid id remains editable");
    assert_eq!(id_editor.draft, "bad id");
    id_editor.draft = "repaired-workflow".to_string();
    apply(
        browser_workflow_review_editor_mutation(&projection, &id_editor)
            .expect("id repair mutation"),
    );

    let projection =
        browser_workflow_review_projection(&coordinator, &owned, BrowserPaneSurface::Claude)
            .expect("invalid viewport projection");
    browser_workflow_review_editor_for_field(
        &projection,
        instance_id,
        BrowserWorkflowReviewEditorField::Description,
    )
    .expect("invalid viewport must not block field focus");
    let metadata = projection.metadata.expect("projected metadata");
    apply(BrowserWorkflowReviewMutation::SetMetadata {
        id: metadata.id,
        name: metadata.name,
        description: metadata.description,
        start_url: metadata.start_url,
        viewport: BrowserRecipeViewport::default(),
    });

    preview_browser_workflow_review(
        &coordinator,
        Some(&owned),
        &owned,
        BrowserPaneSurface::Claude,
        instance_id,
    )
    .expect("repaired review previews");
    let project_root = temporary_project("repair");
    save_browser_workflow_review(
        &coordinator,
        Some(&owned),
        &owned,
        BrowserPaneSurface::Claude,
        instance_id,
        &project_root,
        false,
    )
    .expect("repaired review saves");

    let pane_source = include_str!("../src/browser/pane.rs");
    let app_source = include_str!("../src/app/mod.rs");

    assert!(
        pane_source.contains("browser_workflow_review_editor_for_field"),
        "review fields need to open from the safe projection even while the draft is invalid"
    );
    assert!(
        pane_source.contains("browser_workflow_review_editor_mutation"),
        "editor submission needs to preserve the other current draft fields without requiring preview validation"
    );
    assert!(
        app_source.contains("workflow_review_projection"),
        "focus and Enter handling need to derive repair edits from the current projection"
    );

    let focus_handler = app_source
        .split("BrowserPaneAction::FocusRecordingReviewField")
        .nth(1)
        .expect("focus handler");
    let focus_handler = focus_handler
        .split("BrowserPaneAction::")
        .next()
        .expect("bounded focus handler");
    assert!(
        !focus_handler.contains("preview_workflow_review"),
        "an invalid id, blank name, or invalid viewport must not prevent reopening a field for repair"
    );
}

#[test]
fn user_entered_assertion_uses_the_recorded_steps_real_locator() {
    let coordinator = BrowserWorkflowCoordinator::default();
    let owned = workspace("project-assertion", "ai-assertion");
    let instance = coordinator.start(owned.clone()).expect("start recording");
    for action in [
        BrowserRecordingAction::recipe(devmanager::browser::BrowserRecipeAction::Click {
            locator: locator(),
        })
        .expect("click action"),
        BrowserRecordingAction::navigate("https://example.test/after-click")
            .expect("navigation action"),
    ] {
        let reservation = coordinator
            .reserve_on(
                &instance,
                BrowserRecordingActor::User,
                "tab-a",
                BrowserRisk::Normal,
            )
            .expect("reserve assertion action");
        coordinator
            .commit(reservation, action)
            .expect("commit assertion action");
    }
    coordinator.stop(&instance).expect("stop into review");
    let apply = |mutation| {
        apply_browser_workflow_review_mutation(
            &coordinator,
            Some(&owned),
            &owned,
            BrowserPaneSurface::Claude,
            instance.id(),
            mutation,
        )
    };
    apply(BrowserWorkflowReviewMutation::SetMetadata {
        id: "assertion-flow".to_string(),
        name: "Assertion flow".to_string(),
        description: "Uses entered assertion values".to_string(),
        start_url: "https://example.test/start".to_string(),
        viewport: BrowserRecipeViewport::default(),
    })
    .expect("valid metadata");

    let expected_assertions = [
        (
            BrowserWorkflowReviewAssertionKind::Url,
            "https://assert.example/result",
        ),
        (
            BrowserWorkflowReviewAssertionKind::Title,
            "Quarterly results",
        ),
        (BrowserWorkflowReviewAssertionKind::Text, "Completed safely"),
        (BrowserWorkflowReviewAssertionKind::Value, "42"),
    ];
    for (kind, expected) in expected_assertions {
        let projection =
            browser_workflow_review_projection(&coordinator, &owned, BrowserPaneSurface::Claude)
                .expect("assertion projection");
        let mut editor = browser_workflow_review_editor_for_field(
            &projection,
            instance.id(),
            BrowserWorkflowReviewEditorField::Assertion {
                step_id: "step-1".to_string(),
                kind,
            },
        )
        .expect("assertion editor");
        assert!(editor.draft.is_empty());
        editor.draft = expected.to_string();
        assert!(!format!("{editor:?}").contains(expected));
        let mutation = browser_workflow_review_editor_mutation(&projection, &editor)
            .expect("entered assertion mutation");
        assert!(!format!("{mutation:?}").contains(expected));
        apply(mutation).expect("entered assertion accepted");
    }
    apply(BrowserWorkflowReviewMutation::AddStepAssertionDraft {
        step_id: "step-1".to_string(),
        kind: BrowserWorkflowReviewAssertionKind::Element,
        expected: None,
    })
    .expect("element assertion derives locator");

    let recipe = preview_browser_workflow_review(
        &coordinator,
        Some(&owned),
        &owned,
        BrowserPaneSurface::Claude,
        instance.id(),
    )
    .expect("assertion recipe preview");
    let assertions = &recipe.steps[0].assertions;
    assert!(matches!(
        &assertions[0],
        BrowserRecipeAssertion::Url {
            value: BrowserRecipeValue::Literal { value },
            exact: true,
        } if value == "https://assert.example/result"
    ));
    assert!(matches!(
        &assertions[1],
        BrowserRecipeAssertion::Title {
            value: BrowserRecipeValue::Literal { value },
            exact: false,
        } if value == "Quarterly results"
    ));
    assert!(matches!(
        &assertions[2],
        BrowserRecipeAssertion::Text {
            value: BrowserRecipeValue::Literal { value },
            present: true,
        } if value == "Completed safely"
    ));
    assert!(matches!(
        &assertions[3],
        BrowserRecipeAssertion::Value {
            locator: BrowserRecipeLocator { test_id: Some(test_id), .. },
            value: BrowserRecipeValue::Literal { value },
        } if test_id == "safe-field" && value == "42"
    ));
    assert!(matches!(
        &assertions[4],
        BrowserRecipeAssertion::Element {
            locator: BrowserRecipeLocator { test_id: Some(test_id), .. },
            ..
        } if test_id == "safe-field"
    ));

    let projection =
        browser_workflow_review_projection(&coordinator, &owned, BrowserPaneSurface::Claude)
            .expect("projection after assertions");
    assert!(projection.steps[0].has_assertion_locator);
    assert!(!projection.steps[1].has_assertion_locator);
    let mut blank = browser_workflow_review_editor_for_field(
        &projection,
        instance.id(),
        BrowserWorkflowReviewEditorField::Assertion {
            step_id: "step-1".to_string(),
            kind: BrowserWorkflowReviewAssertionKind::Title,
        },
    )
    .expect("blank assertion editor");
    blank.draft = "   ".to_string();
    assert_eq!(
        browser_workflow_review_editor_mutation(&projection, &blank).unwrap_err(),
        BrowserRecordingError::InvalidMutation
    );
    assert_eq!(
        apply(BrowserWorkflowReviewMutation::AddStepAssertionDraft {
            step_id: "step-2".to_string(),
            kind: BrowserWorkflowReviewAssertionKind::Value,
            expected: Some("unattached".to_string()),
        })
        .unwrap_err(),
        BrowserRecordingError::InvalidMutation
    );
    assert_eq!(
        preview_browser_workflow_review(
            &coordinator,
            Some(&owned),
            &owned,
            BrowserPaneSurface::Claude,
            instance.id(),
        )
        .expect("invalid assertion leaves recipe unchanged")
        .steps[0]
            .assertions
            .len(),
        5
    );

    let pane_source = include_str!("../src/browser/pane.rs");

    assert!(
        pane_source.contains("BrowserWorkflowReviewAssertionKind"),
        "assertion editing needs an explicit typed kind"
    );
    assert!(
        pane_source.contains("AddStepAssertionDraft"),
        "entered expected text must remain volatile until the coordinator resolves the assertion"
    );
    assert!(
        pane_source.contains("primary_locator_for_step"),
        "element and value assertions must derive their locator from the actual recorded step"
    );
    for placeholder in [
        "Expected title",
        "Expected text",
        "Expected value",
        "workflow-review-target",
    ] {
        assert!(
            !pane_source.contains(placeholder),
            "review UI must not manufacture placeholder assertion data: {placeholder}"
        );
    }

    let app_source = include_str!("../src/app/mod.rs");
    let cancel = app_source
        .split("BrowserPaneAction::CancelRecordingReviewEdit =>")
        .nth(1)
        .expect("cancel editor branch")
        .split("BrowserPaneAction::MutateRecordingReview")
        .next()
        .expect("bounded cancel editor branch");
    assert!(cancel.contains("ui.workflow_editor = None"));
    assert!(!cancel.contains("apply_workflow_review_mutation"));
}

#[test]
fn review_reordering_rejects_tab_lifecycle_boundaries_but_allows_adjacent_actions() {
    let coordinator = BrowserWorkflowCoordinator::default();
    let owned = workspace("project-reorder", "ai-reorder");
    let instance = coordinator.start(owned.clone()).expect("start recording");
    for action in [
        devmanager::browser::BrowserRecipeAction::CreateTab {
            tab: "secondary".to_string(),
            url: None,
        },
        devmanager::browser::BrowserRecipeAction::SelectTab {
            tab: "secondary".to_string(),
        },
        devmanager::browser::BrowserRecipeAction::Click { locator: locator() },
        devmanager::browser::BrowserRecipeAction::Hover { locator: locator() },
    ] {
        let reservation = coordinator
            .reserve_on(
                &instance,
                BrowserRecordingActor::User,
                "tab-a",
                BrowserRisk::Normal,
            )
            .expect("reserve reorder action");
        coordinator
            .commit(
                reservation,
                BrowserRecordingAction::recipe(action).expect("recordable reorder action"),
            )
            .expect("commit reorder action");
    }
    coordinator.stop(&instance).expect("stop into review");
    let apply = |mutation| {
        apply_browser_workflow_review_mutation(
            &coordinator,
            Some(&owned),
            &owned,
            BrowserPaneSurface::Claude,
            instance.id(),
            mutation,
        )
    };
    apply(BrowserWorkflowReviewMutation::SetMetadata {
        id: "safe-reorder".to_string(),
        name: "Safe reorder".to_string(),
        description: "Keeps tab lifecycle ordered".to_string(),
        start_url: "https://example.test/start".to_string(),
        viewport: BrowserRecipeViewport::default(),
    })
    .expect("valid reorder metadata");

    let projection =
        browser_workflow_review_projection(&coordinator, &owned, BrowserPaneSurface::Claude)
            .expect("reorder projection");
    assert_eq!(projection.steps.len(), 4);
    assert!(!projection.steps[0].can_move_up);
    assert!(!projection.steps[0].can_move_down);
    assert!(!projection.steps[1].can_move_up);
    assert!(!projection.steps[1].can_move_down);
    assert!(!projection.steps[2].can_move_up);
    assert!(projection.steps[2].can_move_down);
    assert!(projection.steps[3].can_move_up);
    assert!(!projection.steps[3].can_move_down);

    assert_eq!(
        apply(BrowserWorkflowReviewMutation::MoveStep {
            step_id: "step-2".to_string(),
            new_index: 0,
        })
        .unwrap_err(),
        BrowserRecordingError::InvalidMutation
    );
    assert_eq!(
        browser_workflow_review_projection(&coordinator, &owned, BrowserPaneSurface::Claude,)
            .expect("unchanged projection")
            .steps
            .iter()
            .map(|step| step.id.as_str())
            .collect::<Vec<_>>(),
        vec!["step-1", "step-2", "step-3", "step-4"]
    );

    apply(BrowserWorkflowReviewMutation::MoveStep {
        step_id: "step-4".to_string(),
        new_index: 2,
    })
    .expect("adjacent non-tab move remains allowed");
    assert_eq!(
        browser_workflow_review_projection(&coordinator, &owned, BrowserPaneSurface::Claude,)
            .expect("safe reordered projection")
            .steps
            .iter()
            .map(|step| step.id.as_str())
            .collect::<Vec<_>>(),
        vec!["step-1", "step-2", "step-4", "step-3"]
    );
    preview_browser_workflow_review(
        &coordinator,
        Some(&owned),
        &owned,
        BrowserPaneSurface::Claude,
        instance.id(),
    )
    .expect("safe reorder still previews");
    let project_root = temporary_project("safe-reorder");
    save_browser_workflow_review(
        &coordinator,
        Some(&owned),
        &owned,
        BrowserPaneSurface::Claude,
        instance.id(),
        &project_root,
        false,
    )
    .expect("safe reorder still saves");

    let pane_source = include_str!("../src/browser/pane.rs");
    let recording_source = include_str!("../src/browser/recording.rs");

    assert!(
        recording_source.contains("can_move_step"),
        "the recording domain needs one conservative move predicate"
    );
    assert!(
        recording_source.contains("CreateTab")
            && recording_source.contains("SelectTab")
            && recording_source.contains("CloseTab"),
        "the move predicate must recognize every tab lifecycle action"
    );
    assert!(
        pane_source.contains("can_move_up") && pane_source.contains("can_move_down"),
        "the review projection must expose only moves accepted by the domain predicate"
    );
    assert!(
        pane_source.contains("step.can_move_up") && pane_source.contains("step.can_move_down"),
        "the UI must hide moves that cross or involve a tab lifecycle boundary while retaining safe adjacent moves"
    );
}

#[test]
fn narrow_review_pane_uses_wrapped_bounded_control_groups() {
    let pane_source = include_str!("../src/browser/pane.rs");

    assert!(
        pane_source.contains("workflow_control_group"),
        "review controls need one narrow-pane-safe layout primitive"
    );
    assert!(
        pane_source.contains(".flex_wrap()"),
        "multiple step and assertion controls must wrap at the 320px pane minimum"
    );
    assert!(
        pane_source.contains(".overflow_y_scroll()"),
        "the complete review remains vertically reachable"
    );
    assert!(
        !pane_source.contains(
            ".children(assertion_buttons)\n                    .children(remove_assertions)"
        ),
        "assertion add and remove controls must share a wrapping bounded group"
    );
}
