use devmanager::browser::{
    calculate_browser_split, BrowserPaneAction, BrowserPaneContext, BrowserPaneModel,
    BrowserPaneSurface, BrowserPaneTransient, BrowserReplayLocatorSlot,
    BrowserReplayPaneProjection, BrowserReplayProjection, BrowserReplayRepairPhase,
    BrowserReplayRepairProjection, BrowserReplayStatus, BrowserResourceHandle, BrowserResourceId,
    BrowserResourceKind, BrowserRevision, BrowserWorkflowReviewProjection,
    BrowserWorkflowReviewUiState, BrowserWorkspaceKey, BrowserWorkspaceSnapshot,
};

const PRIVATE_SENTINEL: &str = "DM_REPLAY_PANE_PRIVATE_SELECTOR_7D2C";

fn workspace() -> BrowserWorkspaceKey {
    BrowserWorkspaceKey::new("workflow-pane-project", "workflow-pane-conversation").unwrap()
}

fn replay(status: BrowserReplayStatus) -> BrowserReplayProjection {
    BrowserReplayProjection {
        workspace_key: workspace(),
        instance_id: 41,
        recipe_id: "checkout-smoke".to_string(),
        status,
        current_step_index: 2,
        total_steps: 5,
        current_step_id: Some(PRIVATE_SENTINEL.to_string()),
        unresolved_secret_inputs: if status == BrowserReplayStatus::NeedsUserSecret {
            vec!["account-password".to_string(), "one-time-code".to_string()]
        } else {
            Vec::new()
        },
        failure: None,
    }
}

fn evidence(kind: BrowserResourceKind, suffix: &str) -> BrowserResourceHandle {
    BrowserResourceHandle {
        id: BrowserResourceId(format!("repair-{suffix}")),
        uri: format!("browser-resource://{PRIVATE_SENTINEL}/{suffix}"),
        mime_type: format!("application/{PRIVATE_SENTINEL}"),
        kind,
        byte_size: 123,
        created_at_epoch_ms: 456,
        pinned: true,
    }
}

fn repair() -> BrowserReplayRepairProjection {
    BrowserReplayRepairProjection {
        workspace_key: workspace(),
        replay_instance_id: 41,
        repair_id: 9,
        recipe_id: "checkout-smoke".to_string(),
        step_id: "submit-order".to_string(),
        step_index: 2,
        locator_slot: BrowserReplayLocatorSlot::PrimaryAction,
        tab_id: format!("tab-{PRIVATE_SENTINEL}"),
        revision: BrowserRevision(17),
        snapshot: evidence(BrowserResourceKind::ReplayRepairSnapshot, "snapshot"),
        screenshot: evidence(BrowserResourceKind::ReplayRepairScreenshot, "screenshot"),
        phase: BrowserReplayRepairPhase::AwaitingPreview,
    }
}

fn model(transient: BrowserPaneTransient) -> BrowserPaneModel {
    BrowserPaneModel::new(
        workspace(),
        &BrowserPaneContext {
            browser_enabled: true,
            platform_supported: true,
            active_surface: Some(BrowserPaneSurface::Codex),
            editor_open: false,
            modal_open: false,
        },
        &BrowserWorkspaceSnapshot::default(),
        transient,
    )
}

#[test]
fn replay_projection_surfaces_running_secret_repair_and_terminal_disappearance_value_free() {
    let running = model(BrowserPaneTransient {
        replay: Some(BrowserReplayPaneProjection {
            replay: replay(BrowserReplayStatus::Running),
            repair: None,
            selecting_replacement: false,
            repair_apply_ready: false,
        }),
        ..BrowserPaneTransient::default()
    });
    let running_projection = running.replay.as_ref().unwrap();
    assert_eq!(
        running_projection.replay.status,
        BrowserReplayStatus::Running
    );
    assert_eq!(running_projection.replay.current_step_index, 2);
    assert_eq!(running_projection.replay.total_steps, 5);
    assert!(running_projection.repair.is_none());

    let secret = model(BrowserPaneTransient {
        replay: Some(BrowserReplayPaneProjection {
            replay: replay(BrowserReplayStatus::NeedsUserSecret),
            repair: None,
            selecting_replacement: false,
            repair_apply_ready: false,
        }),
        ..BrowserPaneTransient::default()
    });
    assert_eq!(
        secret
            .replay
            .as_ref()
            .unwrap()
            .replay
            .unresolved_secret_inputs,
        vec!["account-password", "one-time-code"]
    );

    let repairing = model(BrowserPaneTransient {
        replay: Some(BrowserReplayPaneProjection {
            replay: replay(BrowserReplayStatus::PausedLocatorRepair),
            repair: Some(repair()),
            selecting_replacement: true,
            repair_apply_ready: false,
        }),
        ..BrowserPaneTransient::default()
    });
    let repair_projection = repairing.replay.as_ref().unwrap();
    assert_eq!(
        repair_projection.replay.status,
        BrowserReplayStatus::PausedLocatorRepair
    );
    assert_eq!(
        repair_projection.repair.as_ref().unwrap().locator_slot,
        BrowserReplayLocatorSlot::PrimaryAction
    );
    assert!(repair_projection.selecting_replacement);
    assert!(repairing.page_surface_visible());

    let terminal = model(BrowserPaneTransient::default());
    assert!(terminal.replay.is_none());

    for safe_debug in [format!("{repair_projection:?}"), format!("{:?}", repairing)] {
        assert!(!safe_debug.contains(PRIVATE_SENTINEL));
        assert!(!safe_debug.contains("browser-resource://"));
        assert!(!safe_debug.contains("tab-DM_REPLAY"));
    }
}

#[test]
fn replay_actions_carry_exact_instance_and_repair_authority() {
    assert!(matches!(
        BrowserPaneAction::CancelReplay { instance_id: 41 },
        BrowserPaneAction::CancelReplay { instance_id: 41 }
    ));
    assert!(matches!(
        BrowserPaneAction::BeginReplayRepairSelection {
            instance_id: 41,
            repair_id: 9,
        },
        BrowserPaneAction::BeginReplayRepairSelection {
            instance_id: 41,
            repair_id: 9,
        }
    ));
    assert!(matches!(
        BrowserPaneAction::ApplyReplayRepair {
            instance_id: 41,
            repair_id: 9,
            resume: true,
        },
        BrowserPaneAction::ApplyReplayRepair {
            instance_id: 41,
            repair_id: 9,
            resume: true,
        }
    ));
}

#[test]
fn secret_entry_hides_page_but_repair_selection_keeps_the_exact_page_visible() {
    let coordinator = devmanager::browser::BrowserReplayCoordinator::default();
    let recipe = devmanager::browser::BrowserRecipeV1 {
        schema_version: devmanager::browser::BROWSER_RECIPE_SCHEMA_VERSION,
        id: "prompt-visibility".to_string(),
        name: "Prompt visibility".to_string(),
        description: String::new(),
        start_url: "https://example.test".to_string(),
        viewport: devmanager::browser::BrowserRecipeViewport::default(),
        inputs: vec![devmanager::browser::BrowserRecipeInput {
            name: "password".to_string(),
            kind: devmanager::browser::BrowserRecipeInputKind::Secret,
            default_value: None,
        }],
        steps: vec![devmanager::browser::BrowserRecipeStep {
            id: "type-password".to_string(),
            action: devmanager::browser::BrowserRecipeAction::Type {
                locator: devmanager::browser::BrowserRecipeLocator {
                    test_id: Some("password".to_string()),
                    ..devmanager::browser::BrowserRecipeLocator::default()
                },
                value: devmanager::browser::BrowserRecipeValue::Input {
                    name: "password".to_string(),
                },
            },
            wait: None,
            assertions: Vec::new(),
        }],
    };
    let started = coordinator
        .start(
            workspace(),
            devmanager::browser::compile_browser_replay(&recipe, Vec::new()).unwrap(),
        )
        .unwrap();
    let (vault, _) = devmanager::browser::BrowserReplaySecretPromptVault::install(
        started.instance,
        started.projection.unresolved_secret_inputs,
    )
    .unwrap();
    let secret = model(BrowserPaneTransient {
        replay_secret_prompt: Some(vault.projection()),
        ..BrowserPaneTransient::default()
    });
    assert!(!secret.page_surface_visible());

    let selecting = model(BrowserPaneTransient {
        replay: Some(BrowserReplayPaneProjection {
            replay: replay(BrowserReplayStatus::PausedLocatorRepair),
            repair: Some(repair()),
            selecting_replacement: true,
            repair_apply_ready: false,
        }),
        ..BrowserPaneTransient::default()
    });
    assert!(selecting.page_surface_visible());
}

#[test]
fn inactive_and_recording_workflow_projections_keep_repair_selection_page_visible() {
    for state in [
        BrowserWorkflowReviewUiState::Inactive,
        BrowserWorkflowReviewUiState::Recording { instance_id: 73 },
    ] {
        let pane = model(BrowserPaneTransient {
            workflow_review: Some(BrowserWorkflowReviewProjection {
                workspace_key: workspace(),
                state,
                metadata: None,
                steps: Vec::new(),
                inputs: Vec::new(),
            }),
            replay: Some(BrowserReplayPaneProjection {
                replay: replay(BrowserReplayStatus::PausedLocatorRepair),
                repair: Some(repair()),
                selecting_replacement: true,
                repair_apply_ready: false,
            }),
            ..BrowserPaneTransient::default()
        });

        assert!(pane.page_surface_visible());
    }
}

#[test]
fn compact_replay_controls_are_rendered_above_page_and_journal_at_minimum_width() {
    let layout = calculate_browser_split(626.0, 50, 300.0, 320.0, 6.0);
    assert_eq!(layout.pane_width, 320.0);

    let pane = include_str!("../src/browser/pane.rs").replace("\r\n", "\n");
    let app = include_str!("../src/app/mod.rs").replace("\r\n", "\n");
    for label in [
        "Cancel replay",
        "Select replacement",
        "Save repair",
        "Save and retry",
    ] {
        assert!(pane.contains(&format!("\"{label}\"")), "missing {label}");
    }
    assert!(pane.contains("current_step_index"));
    assert!(pane.contains("total_steps"));
    assert!(pane.contains("unresolved_secret_inputs"));
    assert!(pane.contains("locator_slot"));
    assert!(pane.contains("snapshot") && pane.contains("screenshot"));
    assert!(pane.contains("repair_apply_ready"));

    let replay_panel = pane.find("let replay_panel").expect("compact replay panel");
    let journal = pane
        .find(".children(journal_rows)")
        .expect("bounded action journal");
    let page = pane
        .find(".children(page_surface)")
        .expect("browser page surface");
    assert!(replay_panel < journal && journal < page);
    assert!(app.contains("300.0,\n                                320.0,"));

    let visibility = pane
        .find("pub fn page_surface_visible")
        .expect("model page visibility boundary");
    let visibility_end = pane[visibility..]
        .find("\n    }")
        .map(|end| visibility + end)
        .unwrap();
    let visibility_body = &pane[visibility..visibility_end];
    assert!(visibility_body.contains("replay_secret_prompt"));
    assert!(!visibility_body.contains("selecting_replacement"));
}

#[test]
fn native_shell_projects_only_the_active_coordinator_replay_and_exact_selection() {
    let app = include_str!("../src/app/mod.rs").replace("\r\n", "\n");
    let model_start = app.find("fn active_browser_model(").unwrap();
    let model_end = app[model_start..]
        .find("fn active_browser_workspace(")
        .map(|end| model_start + end)
        .unwrap();
    let model = &app[model_start..model_end];
    let compact_model: String = model.split_whitespace().collect();
    assert!(compact_model.contains("replay_coordinator.active_state(&workspace_key)"));
    assert!(model.contains("BrowserReplayPaneProjection"));
    assert!(model.contains("selecting_replacement"));
    assert!(model.contains("locator_repair_apply_ready"));

    let pump_start = app.find("fn pump_browser_events(").unwrap();
    let pump_end = app[pump_start..]
        .find("fn with_browser_host_control_barrier")
        .map(|end| pump_start + end)
        .unwrap();
    let pump = &app[pump_start..pump_end];
    assert!(pump.contains("reconcile_browser_replay_state"));
    assert!(
        pump.find("reconcile_browser_replay_state").unwrap()
            < pump.find("events.is_empty()").unwrap()
    );
}

#[test]
fn native_actions_use_exact_replay_authority_and_existing_user_repair_lanes() {
    let app = include_str!("../src/app/mod.rs").replace("\r\n", "\n");
    let action_start = app.find("fn apply_browser_pane_action(").unwrap();
    let action_end = app[action_start..]
        .find("fn handle_browser_replay_secret_key(")
        .map(|end| action_start + end)
        .unwrap();
    let action = &app[action_start..action_end];

    let cancel = action
        .find("BrowserPaneAction::CancelReplay { instance_id }")
        .expect("exact replay cancel action");
    let begin = action
        .find("BrowserPaneAction::BeginReplayRepairSelection")
        .expect("exact repair picker action");
    let apply = action
        .find("BrowserPaneAction::ApplyReplayRepair")
        .expect("exact repair apply action");
    assert!(cancel < begin && begin < apply);

    let cancel_body = &action[cancel..begin];
    assert!(cancel_body.contains("exact_instance(&workspace_key, instance_id)"));
    assert!(cancel_body.contains("coordinator.cancel(&instance)"));

    let begin_body = &action[begin..apply];
    assert!(begin_body.contains("exact_repair("));
    assert!(begin_body.contains("BrowserReplayStatus::PausedLocatorRepair"));
    assert!(begin_body.contains("BrowserCommand::SetAnnotationMode"));
    assert!(begin_body.contains("tab_id: repair.tab_id.clone()"));

    let apply_body = &action[apply..];
    assert!(apply_body.contains("exact_repair("));
    assert!(apply_body.contains("request_replay_repair_apply"));
    assert!(apply_body.contains("true,"));
    assert!(apply_body.contains("resume,"));
    assert!(apply_body.contains("BrowserInvocationContext::user("));
    assert!(apply_body.contains("BrowserRisk::Normal"));
}

#[test]
fn repair_selection_is_revoked_on_route_replay_repair_or_revision_change() {
    let app = include_str!("../src/app/mod.rs").replace("\r\n", "\n");
    let reconcile_start = app
        .find("fn reconcile_browser_replay_repair_selection(")
        .expect("repair selection reconciliation boundary");
    let reconcile_end = app[reconcile_start..]
        .find("fn reconcile_browser_replay_state(")
        .map(|end| reconcile_start + end)
        .unwrap();
    let reconcile = &app[reconcile_start..reconcile_end];
    for exact in [
        "workspace_key",
        "instance_id",
        "repair_id",
        "tab_id",
        "revision",
        "BrowserReplayStatus::PausedLocatorRepair",
    ] {
        assert!(reconcile.contains(exact), "missing exact {exact} fence");
    }
    assert!(reconcile.contains("cancel_browser_replay_repair_selection"));
    assert!(!reconcile.contains("request_replay_repair_preview"));
}
