use devmanager::browser::{
    BrowserAction, BrowserActionTarget, BrowserCommand, BrowserError, BrowserRecipeAction,
    BrowserRecipeInputKind, BrowserRecordingAction, BrowserRecordingActor, BrowserRecordingCommit,
    BrowserRecordingStatus, BrowserRisk, BrowserRuntimeTarget, BrowserTabSnapshot, BrowserViewport,
    BrowserWorkflowCoordinator, BrowserWorkspaceKey, BrowserWorkspaceMutation,
    BrowserWorkspaceSnapshot,
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
        assert_eq!(
            coordinator
                .record_user_chrome_result(&workspace, &command, &Ok(response))
                .expect("record successful chrome command"),
            BrowserRecordingCommit::Recorded,
        );
    }

    assert_eq!(
        coordinator
            .record_user_chrome_result(
                &workspace,
                &BrowserCommand::Navigate {
                    tab_id: "tab-new".to_string(),
                    url: "https://example.test/failed".to_string(),
                },
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
            .record_user_chrome_result(
                &workspace,
                &BrowserCommand::Act {
                    tab_id: "tab-new".to_string(),
                    actions: vec![BrowserAction::Click {
                        target: BrowserActionTarget::default(),
                    }],
                },
                &Ok(devmanager::browser::BrowserResponse::Action {
                    result: devmanager::browser::BrowserActionResult {
                        completed_actions: 1,
                        revision: devmanager::browser::BrowserRevision(8),
                    },
                }),
            )
            .expect("page action must remain on semantic IPC"),
        BrowserRecordingCommit::Ignored,
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
fn windows_host_routes_page_ipc_and_user_chrome_through_the_shared_coordinator() {
    let windows = include_str!("../src/browser/host/windows.rs");
    assert!(windows.contains("workflow_coordinator: BrowserWorkflowCoordinator"));
    assert!(!windows.contains("workflow_recorder: BrowserWorkflowRecorder"));
    assert!(!windows.contains("recording_instances:"));
    assert!(windows.contains("workflow_coordinator.with_recorder"));
    assert!(windows.contains("record_user_chrome_result"));
}

#[test]
fn windows_host_reserves_inspects_and_completes_agent_capture_at_queue_boundaries() {
    let windows = include_str!("../src/browser/host/windows.rs");
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
    coordinator
        .record_user_chrome_result(
            &workspace_b,
            &BrowserCommand::Navigate {
                tab_id: "tab-b".to_string(),
                url: "https://example.test/workspace-b".to_string(),
            },
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
    coordinator
        .record_user_chrome_result(
            &workspace_a,
            &create,
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
