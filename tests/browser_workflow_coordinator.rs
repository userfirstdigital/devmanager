use devmanager::browser::{
    BrowserAction, BrowserActionTarget, BrowserCommand, BrowserError, BrowserRecipeAction,
    BrowserRecipeInputKind, BrowserRecipeLocator, BrowserRecordingAction, BrowserRecordingActor,
    BrowserRecordingCommit, BrowserRecordingStatus, BrowserRisk, BrowserRuntimeTarget,
    BrowserTabSnapshot, BrowserViewport, BrowserWorkflowCoordinator, BrowserWorkspaceKey,
    BrowserWorkspaceMutation, BrowserWorkspaceSnapshot, MAX_BROWSER_RECORDING_INPUTS,
};
use std::path::PathBuf;

fn workspace() -> BrowserWorkspaceKey {
    BrowserWorkspaceKey {
        project_id: "project-a".to_string(),
        ai_tab_id: "conversation-a".to_string(),
    }
}

fn navigation_url(action: &BrowserRecipeAction) -> &str {
    let BrowserRecipeAction::Navigate { url } = action else {
        panic!("expected navigation action");
    };
    let devmanager::browser::BrowserRecipeValue::Literal { value } = url else {
        panic!("expected literal navigation URL");
    };
    value
}

fn workspace_response(url: &str, selected_tab_id: &str) -> devmanager::browser::BrowserResponse {
    devmanager::browser::BrowserResponse::Workspace {
        mutation: BrowserWorkspaceMutation {
            revision: devmanager::browser::BrowserRevision(7),
            snapshot: BrowserWorkspaceSnapshot {
                revision: devmanager::browser::BrowserRevision(7),
                tabs: vec![BrowserTabSnapshot {
                    id: selected_tab_id.to_string(),
                    title: "Example".to_string(),
                    url: url.to_string(),
                    viewport: BrowserViewport::default(),
                }],
                selected_tab_id: Some(selected_tab_id.to_string()),
                ..BrowserWorkspaceSnapshot::default()
            },
        },
    }
}

fn upload_command(tab_id: &str, test_id: &str) -> BrowserCommand {
    BrowserCommand::Upload {
        tab_id: tab_id.to_string(),
        target: BrowserActionTarget {
            locator: devmanager::browser::BrowserLocator {
                test_id: Some(test_id.to_string()),
                ..devmanager::browser::BrowserLocator::default()
            },
            ..BrowserActionTarget::default()
        },
        paths: vec![PathBuf::from("C:/private/not-recorded.txt")],
    }
}

fn upload_response() -> devmanager::browser::BrowserResponse {
    devmanager::browser::BrowserResponse::Upload {
        result: devmanager::browser::BrowserUploadResult {
            files: vec![PathBuf::from("C:/private/not-recorded.txt")],
            revision: devmanager::browser::BrowserRevision(8),
        },
    }
}

fn secret_command(tab_id: &str, input_name: &str) -> BrowserCommand {
    BrowserCommand::SecretType {
        tab_id: tab_id.to_string(),
        target: BrowserActionTarget {
            locator: devmanager::browser::BrowserLocator {
                test_id: Some("credential".to_string()),
                ..devmanager::browser::BrowserLocator::default()
            },
            ..BrowserActionTarget::default()
        },
        input_name: input_name.to_string(),
    }
}

fn secret_response() -> devmanager::browser::BrowserResponse {
    devmanager::browser::BrowserResponse::Action {
        result: devmanager::browser::BrowserActionResult {
            completed_actions: 1,
            revision: devmanager::browser::BrowserRevision(9),
        },
    }
}

#[test]
fn shared_coordinator_orders_interleaved_page_chrome_and_agent_results() {
    let coordinator = BrowserWorkflowCoordinator::with_capacity(8);
    let instance = coordinator
        .start(workspace())
        .expect("start the exact workspace recording");
    let agent_side = coordinator.clone();

    assert_eq!(
        agent_side.status(instance.workspace_key()),
        BrowserRecordingStatus::Recording,
        "the host and agent sides must observe one recording authority"
    );

    let page = coordinator
        .reserve_on(
            &instance,
            BrowserRecordingActor::User,
            "tab-a",
            BrowserRisk::Normal,
        )
        .expect("reserve semantic page action first");
    let chrome = coordinator
        .reserve_on(
            &instance,
            BrowserRecordingActor::User,
            "tab-a",
            BrowserRisk::Normal,
        )
        .expect("reserve user chrome action second");
    let agent_success = agent_side
        .reserve_on(
            &instance,
            BrowserRecordingActor::Agent,
            "tab-a",
            BrowserRisk::Normal,
        )
        .expect("reserve queued agent action third");
    let agent_failure = agent_side
        .reserve_on(
            &instance,
            BrowserRecordingActor::Agent,
            "tab-a",
            BrowserRisk::Normal,
        )
        .expect("reserve failed queued agent action fourth");

    assert_eq!(
        agent_side
            .commit(
                agent_success,
                BrowserRecordingAction::navigate("https://example.test/agent")
                    .expect("safe agent action"),
            )
            .expect("complete agent action out of order"),
        BrowserRecordingCommit::Buffered,
    );
    assert_eq!(
        agent_side
            .cancel(agent_failure)
            .expect("cancel failed agent action"),
        BrowserRecordingCommit::Buffered,
    );
    assert_eq!(
        coordinator
            .commit(
                chrome,
                BrowserRecordingAction::navigate("https://example.test/chrome")
                    .expect("safe chrome action"),
            )
            .expect("complete chrome action out of order"),
        BrowserRecordingCommit::Buffered,
    );
    assert_eq!(
        coordinator
            .commit(
                page,
                BrowserRecordingAction::navigate("https://example.test/page")
                    .expect("safe page action"),
            )
            .expect("complete earliest page action"),
        BrowserRecordingCommit::Recorded,
    );

    let review = coordinator.stop(&instance).expect("stop exact instance");
    let urls = review
        .recipe()
        .steps
        .iter()
        .map(|step| navigation_url(&step.action))
        .collect::<Vec<_>>();
    assert_eq!(
        urls,
        vec![
            "https://example.test/page",
            "https://example.test/chrome",
            "https://example.test/agent",
        ],
        "completion timing must not reorder source reservations and failures must not record"
    );
}

#[test]
fn successful_user_chrome_commands_record_without_duplicating_page_actions() {
    let coordinator = BrowserWorkflowCoordinator::default();
    let workspace = workspace();
    let instance = coordinator
        .start(workspace.clone())
        .expect("start user chrome recording");

    let cases = [
        (
            BrowserCommand::CreateTab {
                url: Some("https://example.test/new".to_string()),
            },
            workspace_response("https://example.test/new", "tab-new"),
        ),
        (
            BrowserCommand::SelectTab {
                tab_id: "tab-new".to_string(),
            },
            workspace_response("https://example.test/new", "tab-new"),
        ),
        (
            BrowserCommand::Navigate {
                tab_id: "tab-new".to_string(),
                url: "https://example.test/account?token=must-not-survive&view=one".to_string(),
            },
            workspace_response("https://example.test/account?view=one", "tab-new"),
        ),
        (
            BrowserCommand::Reload {
                tab_id: "tab-new".to_string(),
            },
            workspace_response("https://example.test/account?view=one", "tab-new"),
        ),
        (
            BrowserCommand::UpdateViewport {
                tab_id: "tab-new".to_string(),
                viewport: BrowserViewport {
                    width: 1440,
                    height: 900,
                    scale_percent: 125,
                },
            },
            workspace_response("https://example.test/account?view=one", "tab-new"),
        ),
        (
            BrowserCommand::CloseTab {
                tab_id: "tab-new".to_string(),
            },
            workspace_response("about:blank", "tab-fallback"),
        ),
    ];

    for (command, response) in cases {
        let capture = coordinator
            .begin_user_chrome_capture(&workspace, &command)
            .expect("preflight successful chrome command")
            .expect("supported chrome command reserves before mutation");
        assert_eq!(
            coordinator
                .complete_user_chrome_capture(capture, &Ok(response))
                .expect("commit successful chrome command"),
            BrowserRecordingCommit::Recorded,
        );
    }

    let failed_command = BrowserCommand::Navigate {
        tab_id: "tab-new".to_string(),
        url: "https://example.test/failed".to_string(),
    };
    let failed_capture = coordinator
        .begin_user_chrome_capture(&workspace, &failed_command)
        .expect("preflight failed browser mutation")
        .expect("supported failed mutation still reserves before execution");
    assert_eq!(
        coordinator
            .complete_user_chrome_capture(
                failed_capture,
                &Err(BrowserError::NavigationFailure {
                    url: "https://example.test/failed".to_string(),
                    message: "failed".to_string(),
                }),
            )
            .expect("failed chrome command is ignored"),
        BrowserRecordingCommit::Ignored,
    );

    assert_eq!(
        coordinator
            .begin_user_chrome_capture(
                &workspace,
                &BrowserCommand::Act {
                    tab_id: "tab-new".to_string(),
                    actions: vec![BrowserAction::Click {
                        target: BrowserActionTarget::default(),
                    }],
                },
            )
            .expect("page action must remain on semantic IPC")
            .is_none(),
        true,
        "user page clicks and typing must not be captured again from host commands"
    );

    let review = coordinator.stop(&instance).expect("stop chrome recording");
    assert_eq!(review.recipe().steps.len(), 6);
    assert!(matches!(
        &review.recipe().steps[0].action,
        BrowserRecipeAction::CreateTab {
            tab,
            url: Some(devmanager::browser::BrowserRecipeValue::Literal { value }),
        } if tab == "tab-1" && value == "https://example.test/new"
    ));
    assert!(matches!(
        &review.recipe().steps[1].action,
        BrowserRecipeAction::SelectTab { tab } if tab == "tab-1"
    ));
    assert_eq!(
        navigation_url(&review.recipe().steps[2].action),
        "https://example.test/account?view=one"
    );
    assert!(matches!(
        review.recipe().steps[3].action,
        BrowserRecipeAction::Reload
    ));
    assert!(matches!(
        &review.recipe().steps[4].action,
        BrowserRecipeAction::SetViewport { viewport }
            if viewport.width == 1440
                && viewport.height == 900
                && viewport.scale_percent == 125
    ));
    assert!(matches!(
        &review.recipe().steps[5].action,
        BrowserRecipeAction::CloseTab { tab } if tab == "tab-1"
    ));
    let json = serde_json::to_string(review.recipe()).expect("serialize review");
    assert!(!json.contains("must-not-survive"));
    assert!(!json.contains("failed"));
}

#[test]
fn recording_start_seeds_selected_tab_as_tab_one() {
    let coordinator = BrowserWorkflowCoordinator::default();
    let workspace = workspace();
    let instance = coordinator
        .start_with_selected_tab(workspace.clone(), "runtime-initial")
        .expect("start recording with the selected runtime tab");

    let select = BrowserCommand::SelectTab {
        tab_id: "runtime-initial".to_string(),
    };
    let capture = coordinator
        .begin_user_chrome_capture(&workspace, &select)
        .expect("reserve initial-tab selection")
        .expect("selection is recordable");
    coordinator
        .complete_user_chrome_capture(
            capture,
            &Ok(workspace_response(
                "https://example.test/initial",
                "runtime-initial",
            )),
        )
        .expect("record initial-tab selection");

    let create = BrowserCommand::CreateTab { url: None };
    let capture = coordinator
        .begin_user_chrome_capture(&workspace, &create)
        .expect("reserve created tab")
        .expect("create is recordable");
    coordinator
        .complete_user_chrome_capture(
            capture,
            &Ok(workspace_response("about:blank", "runtime-created")),
        )
        .expect("record created tab");

    let review = coordinator.stop(&instance).expect("stop recording");
    assert!(matches!(
        &review.recipe().steps[0].action,
        BrowserRecipeAction::SelectTab { tab } if tab == "tab-1"
    ));
    assert!(matches!(
        &review.recipe().steps[1].action,
        BrowserRecipeAction::CreateTab { tab, .. } if tab == "tab-2"
    ));
}

#[test]
fn user_chrome_capture_failures_never_leave_a_saveable_incomplete_recording() {
    let workspace = workspace();

    let preflight_sanitizer = BrowserWorkflowCoordinator::default();
    preflight_sanitizer
        .start(workspace.clone())
        .expect("start preflight sanitizer recording");
    assert!(matches!(
        preflight_sanitizer.begin_user_chrome_capture(
            &workspace,
            &BrowserCommand::Navigate {
                tab_id: "tab-a".to_string(),
                url: "https://user:secret@example.test/private".to_string(),
            },
        ),
        Err(devmanager::browser::BrowserRecordingError::InvalidAction)
    ));
    assert_eq!(
        preflight_sanitizer.status(&workspace),
        BrowserRecordingStatus::Inactive
    );

    let capacity = BrowserWorkflowCoordinator::with_capacity(0);
    capacity
        .start(workspace.clone())
        .expect("start capacity recording");
    assert!(matches!(
        capacity.begin_user_chrome_capture(
            &workspace,
            &BrowserCommand::Reload {
                tab_id: "tab-a".to_string(),
            },
        ),
        Err(devmanager::browser::BrowserRecordingError::CapacityExceeded)
    ));
    assert_eq!(
        capacity.status(&workspace),
        BrowserRecordingStatus::Inactive
    );

    let alias = BrowserWorkflowCoordinator::default();
    alias
        .start(workspace.clone())
        .expect("start alias recording");
    for index in 0..64 {
        let tab_id = format!("tab-{index}");
        let alias_capture = alias
            .begin_user_chrome_capture(
                &workspace,
                &BrowserCommand::SelectTab {
                    tab_id: tab_id.clone(),
                },
            )
            .expect("reserve before alias conversion")
            .expect("select tab is captured");
        alias
            .complete_user_chrome_capture(
                alias_capture,
                &Ok(workspace_response("https://example.test/", &tab_id)),
            )
            .expect("fill one bounded logical alias");
    }
    let alias_capture = alias
        .begin_user_chrome_capture(
            &workspace,
            &BrowserCommand::SelectTab {
                tab_id: "tab-overflow".to_string(),
            },
        )
        .expect("reserve before alias conversion")
        .expect("select tab is captured");
    assert_eq!(
        alias.complete_user_chrome_capture(
            alias_capture,
            &Ok(workspace_response("https://example.test/", "tab-overflow",)),
        ),
        Err(devmanager::browser::BrowserRecordingError::CapacityExceeded),
    );
    assert_eq!(alias.status(&workspace), BrowserRecordingStatus::Inactive);

    let sanitizer = BrowserWorkflowCoordinator::default();
    sanitizer
        .start(workspace.clone())
        .expect("start sanitizer recording");
    let sanitizer_capture = sanitizer
        .begin_user_chrome_capture(
            &workspace,
            &BrowserCommand::Navigate {
                tab_id: "tab-a".to_string(),
                url: "https://example.test/safe".to_string(),
            },
        )
        .expect("safe intent passes preflight")
        .expect("navigate is captured");
    assert_eq!(
        sanitizer.complete_user_chrome_capture(
            sanitizer_capture,
            &Ok(workspace_response(
                "https://user:secret@example.test/private",
                "tab-a",
            )),
        ),
        Err(devmanager::browser::BrowserRecordingError::InvalidAction),
    );
    assert_eq!(
        sanitizer.status(&workspace),
        BrowserRecordingStatus::Inactive
    );
}

#[test]
fn stale_user_chrome_completion_cannot_discard_a_restarted_recording() {
    let coordinator = BrowserWorkflowCoordinator::default();
    let workspace = workspace();
    let old = coordinator
        .start(workspace.clone())
        .expect("start old recording");
    let capture = coordinator
        .begin_user_chrome_capture(
            &workspace,
            &BrowserCommand::Reload {
                tab_id: "tab-old".to_string(),
            },
        )
        .expect("reserve old mutation")
        .expect("reload is captured");
    coordinator.stop(&old).expect("stop old exact instance");
    coordinator
        .discard(&old)
        .expect("discard old exact instance");
    let replacement = coordinator
        .start(workspace.clone())
        .expect("start replacement recording");

    assert!(coordinator
        .complete_user_chrome_capture(
            capture,
            &Ok(workspace_response("https://example.test/", "tab-old")),
        )
        .is_err());
    assert_eq!(
        coordinator.active_instance(&workspace),
        Some(replacement),
        "stale completion must not discard the replacement instance"
    );
    assert_eq!(
        coordinator.status(&workspace),
        BrowserRecordingStatus::Recording
    );
}

#[test]
fn windows_host_routes_page_ipc_and_user_chrome_through_the_shared_coordinator() {
    let windows = include_str!("../src/browser/host/windows.rs");
    assert!(windows.contains("workflow_coordinator: BrowserWorkflowCoordinator"));
    assert!(!windows.contains("workflow_recorder: BrowserWorkflowRecorder"));
    assert!(!windows.contains("recording_instances:"));
    assert!(windows.contains("workflow_coordinator.with_recorder"));
    assert!(windows.contains("begin_user_chrome_capture"));
    assert!(windows.contains("complete_user_chrome_capture"));
    assert!(!windows.contains("record_user_chrome_result"));
    let begin = windows
        .find(".begin_user_chrome_capture(")
        .expect("user chrome preflight reservation");
    let mutate = windows
        .find("self.handle_command_inner(window, workspace_key, command)")
        .expect("user chrome browser mutation");
    assert!(
        begin < mutate,
        "capture must reserve before browser mutation"
    );
}

#[test]
fn windows_host_reserves_inspects_and_completes_agent_capture_at_queue_boundaries() {
    let windows = include_str!("../src/browser/host/windows.rs").replace("\r\n", "\n");
    let reserve = windows
        .find(".reserve_agent_command(")
        .expect("agent capture reserves at host ingress");
    let enqueue = windows
        .find(".operation_queue\n            .enqueue")
        .expect("agent operation queue enqueue");
    assert!(reserve < enqueue, "capture must reserve before queueing");

    let inspect = windows
        .find(".inspect_agent_actions(")
        .expect("runtime inspection enters capture authority");
    let continue_actions = windows
        .find("self.continue_actions")
        .expect("inspected actions continue to execution");
    assert!(
        inspect < continue_actions,
        "capture must inspect before execution"
    );

    let complete = windows
        .find(".complete_agent_command(")
        .expect("agent result completes capture reservation");
    let respond = windows
        .find("request.respond(result)")
        .expect("host delivers the final response");
    assert!(
        complete < respond,
        "capture must finalize before response delivery"
    );
}

#[test]
fn windows_host_drains_prior_page_events_before_user_or_agent_host_reservations() {
    let windows = include_str!("../src/browser/host/windows.rs");
    let user_start = windows
        .find("pub fn handle_command(")
        .expect("public user chrome command seam");
    let user_end = windows[user_start..]
        .find("\n    fn handle_command_with_user_capture(")
        .map(|offset| user_start + offset)
        .expect("end public user command seam");
    let user = &windows[user_start..user_end];
    let user_drain = user
        .find("pump_page_recording_ipc")
        .expect("drain older page events before user chrome");
    let user_command = user
        .find("handle_command_with_user_capture")
        .expect("execute user chrome command");
    assert!(user_drain < user_command);

    let agent_start = windows
        .find("pub fn handle_request(")
        .expect("agent request ingress");
    let agent_end = windows[agent_start..]
        .find("\n    pub fn pump_async_completions(")
        .map(|offset| agent_start + offset)
        .expect("end agent request ingress");
    let agent = &windows[agent_start..agent_end];
    let agent_drain = agent
        .find("pump_page_recording_ipc")
        .expect("drain older page events before agent reserve");
    let agent_reserve = agent
        .find(".reserve_agent_command(")
        .expect("reserve agent source order");
    assert!(agent_drain < agent_reserve);
}

#[test]
fn queued_agent_capture_inspects_before_retaining_values_and_commits_success_only_in_order() {
    const PASSWORD_SENTINEL: &str = "password-value-must-not-survive";
    const PATH_SENTINEL: &str = "C:\\private\\upload-secret.txt";
    const CDP_SENTINEL: &str = "Bearer request-body-must-not-survive";

    let workspace = workspace();
    let target = BrowserActionTarget {
        locator: devmanager::browser::BrowserLocator {
            accessibility_role: Some("textbox".to_string()),
            accessibility_name: Some("Credential".to_string()),
            test_id: Some("credential".to_string()),
            css_selectors: vec!["#credential".to_string()],
        },
        ..BrowserActionTarget::default()
    };
    let password_command = BrowserCommand::Act {
        tab_id: "tab-a".to_string(),
        actions: vec![BrowserAction::Type {
            target: target.clone(),
            text: PASSWORD_SENTINEL.to_string(),
        }],
    };

    let before_inspection = BrowserWorkflowCoordinator::default();
    let before_instance = before_inspection
        .start(workspace.clone())
        .expect("start pre-inspection recording");
    before_inspection
        .reserve_agent_command(
            &workspace,
            "agent-password-before-inspection",
            &password_command,
            BrowserRisk::Normal,
        )
        .expect("reserve without retaining the command value");
    let before_review = before_inspection
        .stop(&before_instance)
        .expect("stop before runtime inspection");
    assert!(before_review.recipe().steps.is_empty());

    let coordinator = BrowserWorkflowCoordinator::default();
    let instance = coordinator
        .start(workspace.clone())
        .expect("start agent recording");
    let navigate = BrowserCommand::Navigate {
        tab_id: "tab-a".to_string(),
        url: "https://example.test/after-agent".to_string(),
    };
    let failed = BrowserCommand::Navigate {
        tab_id: "tab-a".to_string(),
        url: "https://example.test/failed-agent".to_string(),
    };
    let upload = BrowserCommand::Upload {
        tab_id: "tab-a".to_string(),
        target: BrowserActionTarget {
            locator: devmanager::browser::BrowserLocator {
                accessibility_role: Some("button".to_string()),
                accessibility_name: Some("Upload file".to_string()),
                test_id: Some("upload".to_string()),
                css_selectors: vec!["#upload".to_string()],
            },
            ..BrowserActionTarget::default()
        },
        paths: vec![PathBuf::from(PATH_SENTINEL)],
    };
    let cdp = BrowserCommand::Cdp {
        tab_id: "tab-a".to_string(),
        method: "Page.bringToFront".to_string(),
        params: serde_json::json!({"authorization": CDP_SENTINEL}),
    };

    for (operation_id, command) in [
        ("agent-password", &password_command),
        ("agent-navigate", &navigate),
        ("agent-failed", &failed),
        ("agent-upload", &upload),
        ("agent-cdp", &cdp),
    ] {
        coordinator
            .reserve_agent_command(&workspace, operation_id, command, BrowserRisk::Normal)
            .expect("reserve agent command in source order");
    }

    coordinator
        .complete_agent_command(
            &workspace,
            "agent-navigate",
            &navigate,
            &Ok(workspace_response(
                "https://example.test/after-agent",
                "tab-a",
            )),
        )
        .expect("buffer later successful navigation");
    coordinator
        .complete_agent_command(
            &workspace,
            "agent-failed",
            &failed,
            &Err(BrowserError::Interrupted),
        )
        .expect("cancel failed queued command");
    coordinator
        .complete_agent_command(
            &workspace,
            "agent-upload",
            &upload,
            &Ok(devmanager::browser::BrowserResponse::Upload {
                result: devmanager::browser::BrowserUploadResult {
                    files: vec![PathBuf::from(PATH_SENTINEL)],
                    revision: devmanager::browser::BrowserRevision(8),
                },
            }),
        )
        .expect("buffer content-free upload marker");
    coordinator
        .complete_agent_command(
            &workspace,
            "agent-cdp",
            &cdp,
            &Ok(devmanager::browser::BrowserResponse::Cdp {
                inline_result: Some(serde_json::json!({"echo": CDP_SENTINEL})),
                resource: None,
            }),
        )
        .expect("buffer method-only CDP marker");

    coordinator
        .inspect_agent_actions(
            &workspace,
            "agent-password",
            &password_command,
            &[BrowserRuntimeTarget {
                role: Some("textbox".to_string()),
                input_type: Some("password".to_string()),
                autocomplete: Some("current-password".to_string()),
                ..BrowserRuntimeTarget::default()
            }],
            BrowserRisk::AccountSecurity,
        )
        .expect("inspect target before retaining action");
    coordinator
        .complete_agent_command(
            &workspace,
            "agent-password",
            &password_command,
            &Ok(devmanager::browser::BrowserResponse::Action {
                result: devmanager::browser::BrowserActionResult {
                    completed_actions: 1,
                    revision: devmanager::browser::BrowserRevision(9),
                },
            }),
        )
        .expect("commit earliest agent action last");

    let review = coordinator.stop(&instance).expect("stop agent recording");
    assert_eq!(review.recipe().steps.len(), 4);
    assert!(matches!(
        &review.recipe().steps[0].action,
        BrowserRecipeAction::Type {
            value: devmanager::browser::BrowserRecipeValue::Input { name },
            ..
        } if name == "secret"
    ));
    assert!(review.recipe().inputs.iter().any(|input| {
        input.name == "secret"
            && input.kind == BrowserRecipeInputKind::Secret
            && input.default_value.is_none()
    }));
    assert_eq!(
        navigation_url(&review.recipe().steps[1].action),
        "https://example.test/after-agent"
    );
    assert!(matches!(
        review.recipe().steps[2].action,
        BrowserRecipeAction::Upload { .. }
    ));
    assert!(matches!(
        &review.recipe().steps[3].action,
        BrowserRecipeAction::CdpMarker { method } if method == "Page.bringToFront"
    ));
    let json = serde_json::to_string(review.recipe()).expect("serialize safe agent review");
    for forbidden in [
        PASSWORD_SENTINEL,
        PATH_SENTINEL,
        CDP_SENTINEL,
        "failed-agent",
    ] {
        assert!(
            !json.contains(forbidden),
            "retained forbidden agent value: {forbidden}"
        );
    }
}

#[test]
fn secret_type_recording_reserves_inspects_and_commits_only_named_unset_secret_reference() {
    let coordinator = BrowserWorkflowCoordinator::default();
    let workspace = workspace();
    let instance = coordinator
        .start(workspace.clone())
        .expect("start recording");
    let target = BrowserActionTarget {
        locator: devmanager::browser::BrowserLocator {
            accessibility_role: Some("textbox".to_string()),
            accessibility_name: Some("Account credential".to_string()),
            test_id: Some("credential".to_string()),
            css_selectors: vec!["#credential".to_string()],
        },
        ..BrowserActionTarget::default()
    };
    let command = BrowserCommand::SecretType {
        tab_id: "tab-a".to_string(),
        target: target.clone(),
        input_name: "account_credential".to_string(),
    };

    coordinator
        .reserve_agent_command(&workspace, "secret-type", &command, BrowserRisk::Normal)
        .expect("reserve value-free marker");
    coordinator
        .inspect_agent_secret_type(
            &workspace,
            "secret-type",
            &command,
            &BrowserRuntimeTarget {
                role: Some("textbox".to_string()),
                input_type: Some("text".to_string()),
                autocomplete: Some("current-password".to_string()),
                ..BrowserRuntimeTarget::default()
            },
            BrowserRisk::AccountSecurity,
        )
        .expect("inspect before preparing named secret marker");
    coordinator
        .complete_agent_command(
            &workspace,
            "secret-type",
            &command,
            &Ok(devmanager::browser::BrowserResponse::Action {
                result: devmanager::browser::BrowserActionResult {
                    completed_actions: 1,
                    revision: devmanager::browser::BrowserRevision(11),
                },
            }),
        )
        .expect("commit safe marker");

    let second = BrowserCommand::SecretType {
        tab_id: "tab-a".to_string(),
        target,
        input_name: "one_time_code".to_string(),
    };
    coordinator
        .reserve_agent_command(
            &workspace,
            "secret-type-second",
            &second,
            BrowserRisk::Normal,
        )
        .expect("reserve second named secret at same locator");
    coordinator
        .inspect_agent_secret_type(
            &workspace,
            "secret-type-second",
            &second,
            &BrowserRuntimeTarget {
                role: Some("textbox".to_string()),
                input_type: Some("text".to_string()),
                ..BrowserRuntimeTarget::default()
            },
            BrowserRisk::AccountSecurity,
        )
        .expect("inspect second named secret");
    coordinator
        .complete_agent_command(
            &workspace,
            "secret-type-second",
            &second,
            &Ok(devmanager::browser::BrowserResponse::Action {
                result: devmanager::browser::BrowserActionResult {
                    completed_actions: 1,
                    revision: devmanager::browser::BrowserRevision(12),
                },
            }),
        )
        .expect("commit distinct named secret");

    let review = coordinator.stop(&instance).expect("stop recording");
    assert_eq!(review.recipe().steps.len(), 2);
    assert!(matches!(
        &review.recipe().steps[0].action,
        BrowserRecipeAction::Type {
            value: devmanager::browser::BrowserRecipeValue::Input { name },
            ..
        } if name == "account_credential"
    ));
    assert!(review.recipe().inputs.iter().any(|input| {
        input.name == "account_credential"
            && input.kind == BrowserRecipeInputKind::Secret
            && input.default_value.is_none()
    }));
    assert!(matches!(
        &review.recipe().steps[1].action,
        BrowserRecipeAction::Type {
            value: devmanager::browser::BrowserRecipeValue::Input { name },
            ..
        } if name == "one_time_code"
    ));
    let json = serde_json::to_string(review.recipe()).expect("serialize safe recording");
    assert!(!json.contains("sentinel"));
}

#[test]
fn secret_recording_input_collision_is_rejected_at_begin_before_exposure() {
    let coordinator = BrowserWorkflowCoordinator::with_capacity(4);
    let workspace = workspace();
    let instance = coordinator
        .start(workspace.clone())
        .expect("start recording");
    let upload = coordinator
        .reserve_on(
            &instance,
            BrowserRecordingActor::User,
            "tab-a",
            BrowserRisk::Normal,
        )
        .expect("reserve upload");
    coordinator
        .commit(
            upload,
            BrowserRecordingAction::upload(BrowserRecipeLocator {
                test_id: Some("file".to_string()),
                ..BrowserRecipeLocator::default()
            })
            .expect("prepare upload"),
        )
        .expect("commit generated file input");

    let command = BrowserCommand::SecretType {
        tab_id: "tab-a".to_string(),
        target: BrowserActionTarget {
            locator: devmanager::browser::BrowserLocator {
                test_id: Some("credential".to_string()),
                ..devmanager::browser::BrowserLocator::default()
            },
            ..BrowserActionTarget::default()
        },
        input_name: "file".to_string(),
    };
    let error = coordinator
        .reserve_agent_command(&workspace, "collision", &command, BrowserRisk::Normal)
        .expect_err("input ownership is validated before target inspection");
    assert_eq!(
        error,
        devmanager::browser::BrowserRecordingError::InvalidAction
    );

    let review = coordinator.stop(&instance).expect("stop recording");
    assert_eq!(review.recipe().steps.len(), 1);
    assert_eq!(review.recipe().inputs[0].kind, BrowserRecipeInputKind::File);
}

#[test]
fn secret_recording_input_capacity_is_rejected_at_begin_before_exposure() {
    let coordinator = BrowserWorkflowCoordinator::with_capacity(MAX_BROWSER_RECORDING_INPUTS + 2);
    let workspace = workspace();
    let instance = coordinator
        .start(workspace.clone())
        .expect("start recording");
    for index in 0..MAX_BROWSER_RECORDING_INPUTS {
        let reservation = coordinator
            .reserve_on(
                &instance,
                BrowserRecordingActor::User,
                "tab-a",
                BrowserRisk::Normal,
            )
            .expect("reserve generated upload input");
        coordinator
            .commit(
                reservation,
                BrowserRecordingAction::upload(BrowserRecipeLocator {
                    test_id: Some(format!("file-{index}")),
                    ..BrowserRecipeLocator::default()
                })
                .expect("prepare generated upload input"),
            )
            .expect("commit generated upload input");
    }

    let command = BrowserCommand::SecretType {
        tab_id: "tab-a".to_string(),
        target: BrowserActionTarget {
            locator: devmanager::browser::BrowserLocator {
                test_id: Some("credential".to_string()),
                ..devmanager::browser::BrowserLocator::default()
            },
            ..BrowserActionTarget::default()
        },
        input_name: "overflow_secret".to_string(),
    };
    let error = coordinator
        .reserve_agent_command(&workspace, "capacity", &command, BrowserRisk::Normal)
        .expect_err("input capacity is validated before target inspection");
    assert_eq!(
        error,
        devmanager::browser::BrowserRecordingError::CapacityExceeded
    );

    let review = coordinator.stop(&instance).expect("stop recording");
    assert_eq!(review.recipe().inputs.len(), MAX_BROWSER_RECORDING_INPUTS);
    assert_eq!(review.recipe().steps.len(), MAX_BROWSER_RECORDING_INPUTS);
}

#[test]
fn cross_tab_upload_then_secret_records_without_retry_in_both_completion_orders() {
    for secret_completes_first in [true, false] {
        let coordinator = BrowserWorkflowCoordinator::with_capacity(4);
        let workspace = workspace();
        let instance = coordinator.start(workspace.clone()).unwrap();
        let upload = upload_command("tab-a", "upload-a");
        let secret = secret_command("tab-b", "credential");
        coordinator
            .reserve_agent_command(&workspace, "upload-a", &upload, BrowserRisk::Normal)
            .unwrap();
        coordinator
            .reserve_agent_command(&workspace, "secret-b", &secret, BrowserRisk::Normal)
            .unwrap();
        coordinator
            .inspect_agent_secret_type(
                &workspace,
                "secret-b",
                &secret,
                &BrowserRuntimeTarget::default(),
                BrowserRisk::AccountSecurity,
            )
            .expect("source-order input ownership is independent of callback order");

        if secret_completes_first {
            coordinator
                .complete_agent_command(&workspace, "secret-b", &secret, &Ok(secret_response()))
                .unwrap();
            coordinator
                .complete_agent_command(&workspace, "upload-a", &upload, &Ok(upload_response()))
                .unwrap();
        } else {
            coordinator
                .complete_agent_command(&workspace, "upload-a", &upload, &Ok(upload_response()))
                .unwrap();
            coordinator
                .complete_agent_command(&workspace, "secret-b", &secret, &Ok(secret_response()))
                .unwrap();
        }

        let review = coordinator.stop(&instance).unwrap();
        assert_eq!(review.recipe().steps.len(), 2);
        assert_eq!(review.recipe().inputs.len(), 2);
        assert_eq!(review.recipe().inputs[0].name, "file");
        assert_eq!(review.recipe().inputs[0].kind, BrowserRecipeInputKind::File);
        assert_eq!(review.recipe().inputs[1].name, "credential");
        assert_eq!(
            review.recipe().inputs[1].kind,
            BrowserRecipeInputKind::Secret
        );
        assert!(matches!(
            &review.recipe().steps[0].action,
            BrowserRecipeAction::Upload {
                file: devmanager::browser::BrowserRecipeValue::Input { name },
                ..
            } if name == "file"
        ));
        assert!(matches!(
            &review.recipe().steps[1].action,
            BrowserRecipeAction::Type {
                value: devmanager::browser::BrowserRecipeValue::Input { name },
                ..
            } if name == "credential"
        ));
    }
}

#[test]
fn cross_tab_secret_then_upload_owns_names_in_source_order_for_both_completion_orders() {
    for upload_completes_first in [true, false] {
        let coordinator = BrowserWorkflowCoordinator::with_capacity(4);
        let workspace = workspace();
        let instance = coordinator.start(workspace.clone()).unwrap();
        let secret = secret_command("tab-a", "file");
        let upload = upload_command("tab-b", "upload-b");
        coordinator
            .reserve_agent_command(&workspace, "secret-a", &secret, BrowserRisk::Normal)
            .unwrap();
        coordinator
            .reserve_agent_command(&workspace, "upload-b", &upload, BrowserRisk::Normal)
            .unwrap();
        coordinator
            .inspect_agent_secret_type(
                &workspace,
                "secret-a",
                &secret,
                &BrowserRuntimeTarget::default(),
                BrowserRisk::AccountSecurity,
            )
            .expect("the earlier explicit secret owns file before upload begins");

        if upload_completes_first {
            coordinator
                .complete_agent_command(&workspace, "upload-b", &upload, &Ok(upload_response()))
                .unwrap();
            coordinator
                .complete_agent_command(&workspace, "secret-a", &secret, &Ok(secret_response()))
                .unwrap();
        } else {
            coordinator
                .complete_agent_command(&workspace, "secret-a", &secret, &Ok(secret_response()))
                .unwrap();
            coordinator
                .complete_agent_command(&workspace, "upload-b", &upload, &Ok(upload_response()))
                .unwrap();
        }

        let review = coordinator.stop(&instance).unwrap();
        assert_eq!(review.recipe().inputs.len(), 2);
        assert_eq!(review.recipe().inputs[0].name, "file");
        assert_eq!(
            review.recipe().inputs[0].kind,
            BrowserRecipeInputKind::Secret
        );
        assert_eq!(review.recipe().inputs[1].name, "file_2");
        assert_eq!(review.recipe().inputs[1].kind, BrowserRecipeInputKind::File);
        assert!(matches!(
            &review.recipe().steps[0].action,
            BrowserRecipeAction::Type {
                value: devmanager::browser::BrowserRecipeValue::Input { name },
                ..
            } if name == "file"
        ));
        assert!(matches!(
            &review.recipe().steps[1].action,
            BrowserRecipeAction::Upload {
                file: devmanager::browser::BrowserRecipeValue::Input { name },
                ..
            } if name == "file_2"
        ));
    }
}

#[test]
fn cancelled_earlier_secret_keeps_the_later_uploads_preclaimed_name() {
    let coordinator = BrowserWorkflowCoordinator::with_capacity(4);
    let workspace = workspace();
    let instance = coordinator.start(workspace.clone()).unwrap();
    let secret = secret_command("tab-a", "file");
    let upload = upload_command("tab-b", "upload-b");
    coordinator
        .reserve_agent_command(&workspace, "secret-a", &secret, BrowserRisk::Normal)
        .unwrap();
    coordinator
        .reserve_agent_command(&workspace, "upload-b", &upload, BrowserRisk::Normal)
        .unwrap();
    coordinator
        .inspect_agent_secret_type(
            &workspace,
            "secret-a",
            &secret,
            &BrowserRuntimeTarget::default(),
            BrowserRisk::AccountSecurity,
        )
        .expect("the secret owner was fixed before the later upload began");

    coordinator
        .complete_agent_command(
            &workspace,
            "secret-a",
            &secret,
            &Err(BrowserError::Interrupted),
        )
        .expect("cancel the earlier secret");
    coordinator
        .complete_agent_command(&workspace, "upload-b", &upload, &Ok(upload_response()))
        .expect("commit the later upload");

    let review = coordinator.stop(&instance).unwrap();
    assert_eq!(review.recipe().steps.len(), 1);
    assert_eq!(review.recipe().inputs.len(), 1);
    assert_eq!(review.recipe().inputs[0].name, "file_2");
    assert_eq!(review.recipe().inputs[0].kind, BrowserRecipeInputKind::File);
    assert!(matches!(
        &review.recipe().steps[0].action,
        BrowserRecipeAction::Upload {
            file: devmanager::browser::BrowserRecipeValue::Input { name },
            ..
        } if name == "file_2"
    ));
}

#[test]
fn explicit_upload_secret_collision_fails_at_begin_with_value_free_error() {
    let coordinator = BrowserWorkflowCoordinator::with_capacity(4);
    let workspace = workspace();
    let instance = coordinator.start(workspace.clone()).unwrap();
    let upload = upload_command("tab-a", "upload-a");
    let secret = secret_command("tab-b", "file");
    coordinator
        .reserve_agent_command(&workspace, "upload-a", &upload, BrowserRisk::Normal)
        .unwrap();
    let error = coordinator
        .reserve_agent_command(&workspace, "secret-b", &secret, BrowserRisk::Normal)
        .expect_err("the later explicit secret cannot steal the earlier File owner");
    assert_eq!(
        error,
        devmanager::browser::BrowserRecordingError::InvalidAction
    );
    assert_eq!(format!("{error:?}"), "InvalidAction");
    coordinator
        .complete_agent_command(&workspace, "upload-a", &upload, &Ok(upload_response()))
        .unwrap();
    let review = coordinator.stop(&instance).unwrap();
    assert_eq!(review.recipe().steps.len(), 1);
    assert_eq!(review.recipe().inputs.len(), 1);
    assert_eq!(review.recipe().inputs[0].name, "file");
    assert_eq!(review.recipe().inputs[0].kind, BrowserRecipeInputKind::File);
}

#[test]
fn failed_begin_rolls_back_reservation_and_input_owner_atomically() {
    let coordinator = BrowserWorkflowCoordinator::with_capacity(5);
    let workspace = workspace();
    let instance = coordinator.start(workspace.clone()).unwrap();
    let upload = upload_command("tab-a", "upload-a");
    coordinator
        .reserve_agent_command(&workspace, "upload-a", &upload, BrowserRisk::Normal)
        .unwrap();

    let collision = secret_command("tab-b", "file");
    assert_eq!(
        coordinator
            .reserve_agent_command(&workspace, "collision", &collision, BrowserRisk::Normal,),
        Err(devmanager::browser::BrowserRecordingError::InvalidAction)
    );

    let replacement = secret_command("tab-b", "credential");
    coordinator
        .reserve_agent_command(&workspace, "replacement", &replacement, BrowserRisk::Normal)
        .expect("failed begin released its reservation and input ownership");
    coordinator
        .inspect_agent_secret_type(
            &workspace,
            "replacement",
            &replacement,
            &BrowserRuntimeTarget::default(),
            BrowserRisk::AccountSecurity,
        )
        .unwrap();
    coordinator
        .complete_agent_command(
            &workspace,
            "replacement",
            &replacement,
            &Ok(secret_response()),
        )
        .unwrap();
    coordinator
        .complete_agent_command(&workspace, "upload-a", &upload, &Ok(upload_response()))
        .unwrap();

    let review = coordinator.stop(&instance).unwrap();
    assert_eq!(review.recipe().steps.len(), 2);
    assert_eq!(review.recipe().inputs[0].name, "file");
    assert_eq!(review.recipe().inputs[0].kind, BrowserRecipeInputKind::File);
    assert_eq!(review.recipe().inputs[1].name, "credential");
    assert_eq!(
        review.recipe().inputs[1].kind,
        BrowserRecipeInputKind::Secret
    );
}

#[test]
fn source_order_input_capacity_fails_at_begin_before_secret_exposure() {
    let coordinator = BrowserWorkflowCoordinator::with_capacity(MAX_BROWSER_RECORDING_INPUTS + 2);
    let workspace = workspace();
    let instance = coordinator.start(workspace.clone()).unwrap();
    let mut uploads = Vec::new();
    for index in 0..MAX_BROWSER_RECORDING_INPUTS {
        let upload = upload_command("tab-a", &format!("upload-{index}"));
        let operation_id = format!("upload-{index}");
        coordinator
            .reserve_agent_command(&workspace, &operation_id, &upload, BrowserRisk::Normal)
            .unwrap();
        uploads.push((operation_id, upload));
    }
    let secret = secret_command("tab-b", "overflow_secret");
    let error = coordinator
        .reserve_agent_command(&workspace, "secret-b", &secret, BrowserRisk::Normal)
        .expect_err("all input owners are claimed before asynchronous inspection");
    assert_eq!(
        error,
        devmanager::browser::BrowserRecordingError::CapacityExceeded
    );
    assert_eq!(format!("{error:?}"), "CapacityExceeded");
    for (operation_id, upload) in &uploads {
        coordinator
            .complete_agent_command(&workspace, operation_id, upload, &Ok(upload_response()))
            .unwrap();
    }
    let review = coordinator.stop(&instance).unwrap();
    assert_eq!(review.recipe().steps.len(), MAX_BROWSER_RECORDING_INPUTS);
    assert_eq!(review.recipe().inputs.len(), MAX_BROWSER_RECORDING_INPUTS);
}

#[test]
fn stop_discard_restart_releases_all_preclaimed_input_owners() {
    let coordinator = BrowserWorkflowCoordinator::with_capacity(4);
    let workspace = workspace();
    let first = coordinator.start(workspace.clone()).unwrap();
    let pending_secret = secret_command("tab-a", "file");
    coordinator
        .reserve_agent_command(
            &workspace,
            "pending-secret",
            &pending_secret,
            BrowserRisk::Normal,
        )
        .unwrap();
    coordinator
        .inspect_agent_secret_type(
            &workspace,
            "pending-secret",
            &pending_secret,
            &BrowserRuntimeTarget::default(),
            BrowserRisk::AccountSecurity,
        )
        .unwrap();

    let first_review = coordinator.stop(&first).unwrap();
    assert!(first_review.recipe().inputs.is_empty());
    assert!(first_review.recipe().steps.is_empty());
    coordinator.discard(&first).unwrap();

    let restarted = coordinator.start(workspace.clone()).unwrap();
    let upload = upload_command("tab-b", "upload-after-restart");
    coordinator
        .reserve_agent_command(
            &workspace,
            "upload-after-restart",
            &upload,
            BrowserRisk::Normal,
        )
        .expect("restart has a fresh input ownership domain");
    coordinator
        .complete_agent_command(
            &workspace,
            "upload-after-restart",
            &upload,
            &Ok(upload_response()),
        )
        .unwrap();
    let restarted_review = coordinator.stop(&restarted).unwrap();
    assert_eq!(restarted_review.recipe().inputs.len(), 1);
    assert_eq!(restarted_review.recipe().inputs[0].name, "file");
    assert_eq!(
        restarted_review.recipe().inputs[0].kind,
        BrowserRecipeInputKind::File
    );
}

#[test]
fn stop_restart_and_workspace_lifecycle_fence_late_agent_completions() {
    let coordinator = BrowserWorkflowCoordinator::default();
    let workspace_a = workspace();
    let workspace_b = BrowserWorkspaceKey {
        project_id: "project-a".to_string(),
        ai_tab_id: "conversation-b".to_string(),
    };
    let first_a = coordinator
        .start(workspace_a.clone())
        .expect("start first workspace A recording");
    let first_b = coordinator
        .start(workspace_b.clone())
        .expect("start independent workspace B recording");
    let old_command = BrowserCommand::Navigate {
        tab_id: "tab-a".to_string(),
        url: "https://example.test/old-instance".to_string(),
    };
    coordinator
        .reserve_agent_command(
            &workspace_a,
            "old-operation",
            &old_command,
            BrowserRisk::Normal,
        )
        .expect("reserve old instance action");
    let workspace_b_command = BrowserCommand::Navigate {
        tab_id: "tab-b".to_string(),
        url: "https://example.test/workspace-b".to_string(),
    };
    let workspace_b_capture = coordinator
        .begin_user_chrome_capture(&workspace_b, &workspace_b_command)
        .expect("preflight independent workspace B action")
        .expect("navigate reserves before mutation");
    coordinator
        .complete_user_chrome_capture(
            workspace_b_capture,
            &Ok(workspace_response(
                "https://example.test/workspace-b",
                "tab-b",
            )),
        )
        .expect("record independent workspace B action");

    let stopped_a = coordinator.stop(&first_a).expect("stop first A instance");
    assert!(stopped_a.recipe().steps.is_empty());
    coordinator
        .complete_agent_command(
            &workspace_a,
            "old-operation",
            &old_command,
            &Ok(workspace_response(
                "https://example.test/old-instance",
                "tab-a",
            )),
        )
        .expect("late old completion is fenced");
    let stopped_b = coordinator.stop(&first_b).expect("stop workspace B");
    assert_eq!(stopped_b.recipe().steps.len(), 1);
    assert_eq!(
        navigation_url(&stopped_b.recipe().steps[0].action),
        "https://example.test/workspace-b"
    );

    coordinator
        .discard(&first_a)
        .expect("discard first A review");
    coordinator.discard(&first_b).expect("discard B review");
    let restarted_a = coordinator
        .start(workspace_a.clone())
        .expect("restart workspace A with a fresh authority");
    let create = BrowserCommand::CreateTab {
        url: Some("https://example.test/restarted".to_string()),
    };
    let restarted_capture = coordinator
        .begin_user_chrome_capture(&workspace_a, &create)
        .expect("preflight restarted authority")
        .expect("create tab reserves before mutation");
    coordinator
        .complete_user_chrome_capture(
            restarted_capture,
            &Ok(workspace_response(
                "https://example.test/restarted",
                "runtime-tab-after-restart",
            )),
        )
        .expect("record only on restarted authority");
    let restarted_review = coordinator
        .stop(&restarted_a)
        .expect("stop restarted A authority");
    assert_eq!(restarted_review.recipe().steps.len(), 1);
    assert!(matches!(
        &restarted_review.recipe().steps[0].action,
        BrowserRecipeAction::CreateTab { tab, .. } if tab == "tab-1"
    ));
}

#[test]
fn windows_user_interrupt_denial_and_callback_failure_share_response_finalization() {
    let windows = include_str!("../src/browser/host/windows.rs");
    let user_input_start = windows
        .find("BrowserHostEvent::UserInput {")
        .expect("user-input lifecycle branch");
    let user_input_end = windows[user_input_start..]
        .find("BrowserHostEvent::DomMutation")
        .map(|offset| user_input_start + offset)
        .expect("end user-input branch");
    assert!(windows[user_input_start..user_input_end].contains("cancel_tab_operations"));

    let denial_start = windows
        .find("if !approved {")
        .expect("approval denial branch");
    let denial_end = windows[denial_start..]
        .find("self.apply_visibility_plan()?")
        .map(|offset| denial_start + offset)
        .expect("end denial branch");
    let denial = &windows[denial_start..denial_end];
    assert!(denial.contains("finish_queued_request"));
    assert!(denial.contains("BrowserError::BlockedPermission"));

    let callback_failure = windows
        .find("WebView2 callback failed")
        .expect("async callback failure branch");
    let finish_before_failure = windows[..callback_failure]
        .rfind("finish_queued_request")
        .expect("callback failure uses final response path");
    assert!(finish_before_failure < callback_failure);
}

#[test]
fn inactive_coordinator_never_changes_agent_command_admission() {
    let coordinator = BrowserWorkflowCoordinator::default();
    coordinator
        .reserve_agent_command(
            &workspace(),
            "",
            &BrowserCommand::Act {
                tab_id: "tab-a".to_string(),
                actions: Vec::new(),
            },
            BrowserRisk::Normal,
        )
        .expect("inactive capture is an unconditional no-op");
}

#[test]
fn sensitive_runtime_keypress_retains_only_fixed_control_keys() {
    let coordinator = BrowserWorkflowCoordinator::default();
    let workspace = workspace();
    let instance = coordinator
        .start(workspace.clone())
        .expect("start sensitive keypress capture");
    let target = BrowserActionTarget {
        locator: devmanager::browser::BrowserLocator {
            accessibility_role: Some("textbox".to_string()),
            accessibility_name: Some("Password".to_string()),
            test_id: Some("password".to_string()),
            css_selectors: vec!["#password".to_string()],
        },
        ..BrowserActionTarget::default()
    };
    let command = BrowserCommand::Act {
        tab_id: "tab-a".to_string(),
        actions: vec![
            BrowserAction::Keypress {
                target: Some(target.clone()),
                key: "q".to_string(),
            },
            BrowserAction::Keypress {
                target: Some(target),
                key: "Enter".to_string(),
            },
        ],
    };
    coordinator
        .reserve_agent_command(&workspace, "sensitive-keys", &command, BrowserRisk::Normal)
        .expect("reserve sensitive keypresses");
    let runtime = BrowserRuntimeTarget {
        role: Some("textbox".to_string()),
        input_type: Some("password".to_string()),
        autocomplete: Some("current-password".to_string()),
        ..BrowserRuntimeTarget::default()
    };
    coordinator
        .inspect_agent_actions(
            &workspace,
            "sensitive-keys",
            &command,
            &[runtime.clone(), runtime],
            BrowserRisk::AccountSecurity,
        )
        .expect("inspect both sensitive keypress targets");
    coordinator
        .complete_agent_command(
            &workspace,
            "sensitive-keys",
            &command,
            &Ok(devmanager::browser::BrowserResponse::Action {
                result: devmanager::browser::BrowserActionResult {
                    completed_actions: 2,
                    revision: devmanager::browser::BrowserRevision(10),
                },
            }),
        )
        .expect("complete sensitive keypress command");
    let review = coordinator.stop(&instance).expect("stop keypress capture");
    assert_eq!(review.recipe().steps.len(), 1);
    assert!(matches!(
        &review.recipe().steps[0].action,
        BrowserRecipeAction::Keypress {
            key: devmanager::browser::BrowserRecipeValue::Literal { value },
            ..
        } if value == "Enter"
    ));
}

#[test]
fn agent_capture_requires_the_exact_success_response_variant() {
    let coordinator = BrowserWorkflowCoordinator::default();
    let workspace = workspace();
    let instance = coordinator
        .start(workspace.clone())
        .expect("start exact-response capture");
    let upload = BrowserCommand::Upload {
        tab_id: "tab-a".to_string(),
        target: BrowserActionTarget {
            locator: devmanager::browser::BrowserLocator {
                accessibility_role: Some("button".to_string()),
                accessibility_name: Some("Upload file".to_string()),
                test_id: Some("upload".to_string()),
                css_selectors: vec!["#upload".to_string()],
            },
            ..BrowserActionTarget::default()
        },
        paths: vec![PathBuf::from("C:\\private\\must-not-enter.txt")],
    };
    coordinator
        .reserve_agent_command(
            &workspace,
            "mismatched-response",
            &upload,
            BrowserRisk::Normal,
        )
        .expect("reserve upload");
    coordinator
        .complete_agent_command(
            &workspace,
            "mismatched-response",
            &upload,
            &Ok(devmanager::browser::BrowserResponse::Acknowledged),
        )
        .expect("mismatched success cancels capture");
    let review = coordinator
        .stop(&instance)
        .expect("stop exact-response capture");
    assert!(review.recipe().steps.is_empty());
}
