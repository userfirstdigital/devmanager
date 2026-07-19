use devmanager::browser::{
    browser_command_channel, compile_browser_replay, execute_browser_replay, BrowserAction,
    BrowserActionResult, BrowserActionTarget, BrowserCommand, BrowserCommandRequest, BrowserError,
    BrowserInvocationActor, BrowserLocator, BrowserRecipeAction, BrowserRecipeAssertion,
    BrowserRecipeElementState, BrowserRecipeInput, BrowserRecipeInputKind, BrowserRecipeLocator,
    BrowserRecipeStep, BrowserRecipeV1, BrowserRecipeValue, BrowserRecipeViewport,
    BrowserRecipeWait, BrowserReplayCoordinator, BrowserReplayFailureCode, BrowserReplayProjection,
    BrowserReplayPublicInput, BrowserReplaySecretPromptVault, BrowserReplayStatus,
    BrowserResourceHandle, BrowserResourceId, BrowserResourceKind, BrowserResourceLimits,
    BrowserResourceStore, BrowserResponse, BrowserRevision, BrowserRisk, BrowserScreenshotMode,
    BrowserTabSnapshot, BrowserUploadResult, BrowserViewport, BrowserWaitCondition,
    BrowserWaitResult, BrowserWorkspaceKey, BrowserWorkspaceMutation, BrowserWorkspaceSnapshot,
    BROWSER_RECIPE_SCHEMA_VERSION,
};
use std::path::{Path, PathBuf};
#[cfg(target_os = "windows")]
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

static NEXT_TEMP_ROOT: AtomicU64 = AtomicU64::new(0);
static REPLAY_RESOURCE_STORE: OnceLock<BrowserResourceStore> = OnceLock::new();

#[test]
fn repaired_executor_phases_are_resumed_only_by_the_real_authorized_apply_pipeline() {
    let executor = include_str!("../src/browser/replay_executor.rs");
    let phase_start = executor
        .find("async fn batch_c_resume_retries_only_action_wait_step_wait_or_exact_assertion_phase")
        .unwrap();
    let phase_end = executor[phase_start..]
        .find("async fn stale_repair_cannot_resume_the_next_repair_in_the_same_executor")
        .unwrap()
        + phase_start;
    let phases = &executor[phase_start..phase_end];
    assert!(!phases.contains("resume_locator_repair_for_executor_test"));
    assert!(phases.matches("apply_exact_repair(").count() >= 4);
    for exact_cursor in [
        "BrowserReplayRepairResumeCursor::Action",
        "BrowserReplayRepairResumeCursor::ActionWait",
        "BrowserReplayRepairResumeCursor::StepWait",
        "BrowserReplayRepairResumeCursor::Assertion(1)",
    ] {
        assert!(phases.contains(exact_cursor), "missing {exact_cursor}");
    }
    assert!(phases.contains("single mutating action"));
    assert!(phases.contains("step-wait-only retry"));
    assert!(phases.contains("assertion-one-only retry"));

    let second_start = phase_end;
    let second_end = executor[second_start..]
        .find("async fn secret_type_locator_failure_enters_primary_action_repair")
        .unwrap()
        + second_start;
    let second = &executor[second_start..second_end];
    assert!(!second.contains("resume_locator_repair_for_executor_test"));
    assert_eq!(second.matches("apply_exact_repair(").count(), 1);
    assert!(second.contains("assert_ne!(stale_repair.repair_id(), active_repair.repair_id())"));
    assert!(second.contains("ready-first-repair"));
    assert!(second.contains("BrowserReplayStatus::Cancelled"));
    assert!(second.contains("second_snapshot.id"));
    assert!(second.contains("second_screenshot.id"));

    let fixture_start = executor.find("async fn recipe_fixture(").unwrap();
    let fixture_end = executor[fixture_start..]
        .find("async fn failed_click_fixture(")
        .unwrap()
        + fixture_start;
    let fixture = &executor[fixture_start..fixture_end];
    assert!(fixture.contains("save_recipe(&project_root, &recipe)"));
    assert!(fixture.contains("&run_project_root"));

    let applied_start = executor
        .find("async fn applied_without_resume_requires_fresh_exact_preview_before_no_write_executor_retry")
        .unwrap();
    let applied_end = executor[applied_start..]
        .find("async fn stale_repair_cannot_resume_the_next_repair_in_the_same_executor")
        .unwrap()
        + applied_start;
    let applied = &executor[applied_start..applied_end];
    assert!(applied.contains("apply_previewed_repair(&mut fixture, &repair, false, true)"));
    assert!(applied.contains("preview_exact_repair(&mut fixture, \"repaired-submit\", 10)"));
    assert!(applied.contains("apply_previewed_repair(&mut fixture, &repair, true, false)"));
    assert!(applied.contains("permissions.set_readonly(true)"));
    assert!(applied.contains("assert_eq!(resumed.replay.current_step_index, 0)"));
    assert!(applied.contains("command_targets_test_id(retry.command(), \"repaired-submit\")"));
}

fn replay_resource_store() -> &'static BrowserResourceStore {
    REPLAY_RESOURCE_STORE.get_or_init(|| {
        let root = std::env::temp_dir().join(format!(
            "devmanager-replay-executor-resources-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        BrowserResourceStore::open(root, BrowserResourceLimits::default())
            .expect("open replay executor resource store")
    })
}

#[cfg(target_os = "windows")]
fn create_directory_redirect(target: &Path, link: &Path) -> std::io::Result<()> {
    let status = std::process::Command::new("cmd.exe")
        .args(["/c", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    status
        .success()
        .then_some(())
        .ok_or_else(|| std::io::Error::other("mklink /J failed"))
}

#[cfg(not(target_os = "windows"))]
fn create_directory_redirect(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(target_os = "windows")]
fn remove_directory_redirect(link: &Path) {
    let _ = std::fs::remove_dir(link);
}

#[cfg(not(target_os = "windows"))]
fn remove_directory_redirect(link: &Path) {
    let _ = std::fs::remove_file(link);
}

fn workspace(conversation: &str) -> BrowserWorkspaceKey {
    BrowserWorkspaceKey::new("replay-executor", conversation).unwrap()
}

fn setup_recipe() -> BrowserRecipeV1 {
    BrowserRecipeV1 {
        schema_version: BROWSER_RECIPE_SCHEMA_VERSION,
        id: "executor-setup".to_string(),
        name: "Executor setup".to_string(),
        description: "Sequential replay setup fixture".to_string(),
        start_url: "https://example.test/replay-start".to_string(),
        viewport: BrowserRecipeViewport {
            width: 1440,
            height: 900,
            scale_percent: 125,
        },
        inputs: Vec::new(),
        steps: vec![BrowserRecipeStep {
            id: "reload".to_string(),
            action: BrowserRecipeAction::Reload,
            wait: None,
            assertions: Vec::new(),
        }],
    }
}

fn canonical_project_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .canonicalize()
        .expect("canonical project root")
}

fn workspace_response(tab_id: &str, url: &str, viewport: BrowserViewport) -> BrowserResponse {
    workspace_response_with_tabs(tab_id, vec![(tab_id, url, viewport)])
}

fn workspace_response_with_tabs(
    selected_tab_id: &str,
    tabs: Vec<(&str, &str, BrowserViewport)>,
) -> BrowserResponse {
    BrowserResponse::Workspace {
        mutation: BrowserWorkspaceMutation {
            revision: BrowserRevision(1),
            snapshot: BrowserWorkspaceSnapshot {
                revision: BrowserRevision(1),
                tabs: tabs
                    .into_iter()
                    .map(|(tab_id, url, viewport)| BrowserTabSnapshot {
                        id: tab_id.to_string(),
                        title: "Replay".to_string(),
                        url: url.to_string(),
                        viewport,
                    })
                    .collect(),
                selected_tab_id: Some(selected_tab_id.to_string()),
                ..BrowserWorkspaceSnapshot::default()
            },
        },
    }
}

async fn next_request(
    inbox: &mut devmanager::browser::BrowserCommandInbox,
    label: &str,
) -> BrowserCommandRequest {
    for _ in 0..200 {
        let lifecycle = inbox.with_locked_host_work(|_controls, mut requests| {
            assert!(requests.len() <= 1, "replay queued lifecycle commands");
            requests.pop()
        });
        if let Some(request) = lifecycle {
            return request;
        }
        match tokio::time::timeout(Duration::from_millis(10), inbox.recv()).await {
            Ok(Some(request)) => return request,
            Ok(None) => panic!("browser command inbox closed before {label}"),
            Err(_) => {}
        }
    }
    panic!("timed out waiting for {label}")
}

fn recipe_locator(test_id: &str) -> BrowserRecipeLocator {
    BrowserRecipeLocator {
        test_id: Some(test_id.to_string()),
        ..BrowserRecipeLocator::default()
    }
}

fn literal(value: &str) -> BrowserRecipeValue {
    BrowserRecipeValue::Literal {
        value: value.to_string(),
    }
}

fn action_target(test_id: &str) -> BrowserActionTarget {
    BrowserActionTarget {
        locator: BrowserLocator {
            test_id: Some(test_id.to_string()),
            ..BrowserLocator::default()
        },
        ..BrowserActionTarget::default()
    }
}

fn action_recipe(actions: Vec<BrowserRecipeAction>) -> BrowserRecipeV1 {
    BrowserRecipeV1 {
        schema_version: BROWSER_RECIPE_SCHEMA_VERSION,
        id: "executor-actions".to_string(),
        name: "Executor actions".to_string(),
        description: "Every checkpoint eight action".to_string(),
        start_url: "https://example.test/action-start".to_string(),
        viewport: BrowserRecipeViewport::default(),
        inputs: Vec::new(),
        steps: actions
            .into_iter()
            .enumerate()
            .map(|(index, action)| BrowserRecipeStep {
                id: format!("action-{}", index + 1),
                action,
                wait: None,
                assertions: Vec::new(),
            })
            .collect(),
    }
}

fn upload_recipe() -> BrowserRecipeV1 {
    BrowserRecipeV1 {
        schema_version: BROWSER_RECIPE_SCHEMA_VERSION,
        id: "executor-upload".to_string(),
        name: "Executor upload".to_string(),
        description: "Execution-time upload containment".to_string(),
        start_url: "https://example.test/upload".to_string(),
        viewport: BrowserRecipeViewport::default(),
        inputs: vec![BrowserRecipeInput {
            name: "upload-file".to_string(),
            kind: BrowserRecipeInputKind::File,
            default_value: None,
        }],
        steps: vec![BrowserRecipeStep {
            id: "upload".to_string(),
            action: BrowserRecipeAction::Upload {
                locator: recipe_locator("upload"),
                file: BrowserRecipeValue::Input {
                    name: "upload-file".to_string(),
                },
            },
            wait: None,
            assertions: Vec::new(),
        }],
    }
}

fn secret_type_recipe() -> BrowserRecipeV1 {
    BrowserRecipeV1 {
        schema_version: BROWSER_RECIPE_SCHEMA_VERSION,
        id: "executor-secret-type".to_string(),
        name: "Executor secret type".to_string(),
        description: "Ordinary and private Type routing fixture".to_string(),
        start_url: "https://example.test/sign-in".to_string(),
        viewport: BrowserRecipeViewport::default(),
        inputs: vec![
            BrowserRecipeInput {
                name: "display-name".to_string(),
                kind: BrowserRecipeInputKind::Text,
                default_value: None,
            },
            BrowserRecipeInput {
                name: "credential".to_string(),
                kind: BrowserRecipeInputKind::Secret,
                default_value: None,
            },
        ],
        steps: vec![
            BrowserRecipeStep {
                id: "type-display-name".to_string(),
                action: BrowserRecipeAction::Type {
                    locator: recipe_locator("display-name"),
                    value: BrowserRecipeValue::Input {
                        name: "display-name".to_string(),
                    },
                },
                wait: None,
                assertions: Vec::new(),
            },
            BrowserRecipeStep {
                id: "type-credential".to_string(),
                action: BrowserRecipeAction::Type {
                    locator: recipe_locator("credential"),
                    value: BrowserRecipeValue::Input {
                        name: "credential".to_string(),
                    },
                },
                wait: None,
                assertions: Vec::new(),
            },
        ],
    }
}

fn secret_type_action_response(completed_actions: usize) -> BrowserResponse {
    BrowserResponse::Action {
        result: BrowserActionResult {
            completed_actions,
            revision: BrowserRevision(7),
        },
    }
}

fn assertion_recipe() -> BrowserRecipeV1 {
    BrowserRecipeV1 {
        schema_version: BROWSER_RECIPE_SCHEMA_VERSION,
        id: "executor-assertions".to_string(),
        name: "Executor assertions".to_string(),
        description: "Ordered typed assertion fixture".to_string(),
        start_url: "https://example.test/assertions".to_string(),
        viewport: BrowserRecipeViewport::default(),
        inputs: Vec::new(),
        steps: vec![BrowserRecipeStep {
            id: "assert-everything".to_string(),
            action: BrowserRecipeAction::Click {
                locator: recipe_locator("submit"),
            },
            wait: Some(BrowserRecipeWait::Url {
                value: BrowserRecipeValue::Literal {
                    value: "https://example.test/after-click".to_string(),
                },
                exact: true,
                timeout_ms: 2_000,
            }),
            assertions: vec![
                BrowserRecipeAssertion::Url {
                    value: BrowserRecipeValue::Literal {
                        value: "https://example.test/after-click".to_string(),
                    },
                    exact: true,
                },
                BrowserRecipeAssertion::Title {
                    value: BrowserRecipeValue::Literal {
                        value: "Ready".to_string(),
                    },
                    exact: false,
                },
                BrowserRecipeAssertion::Text {
                    value: BrowserRecipeValue::Literal {
                        value: "Saved".to_string(),
                    },
                    present: true,
                },
                BrowserRecipeAssertion::Text {
                    value: BrowserRecipeValue::Literal {
                        value: "Error".to_string(),
                    },
                    present: false,
                },
                BrowserRecipeAssertion::Element {
                    locator: recipe_locator("status"),
                    state: BrowserRecipeElementState::Present,
                },
                BrowserRecipeAssertion::Element {
                    locator: recipe_locator("removed"),
                    state: BrowserRecipeElementState::Absent,
                },
                BrowserRecipeAssertion::Element {
                    locator: recipe_locator("visible"),
                    state: BrowserRecipeElementState::Visible,
                },
                BrowserRecipeAssertion::Element {
                    locator: recipe_locator("hidden"),
                    state: BrowserRecipeElementState::Hidden,
                },
                BrowserRecipeAssertion::Value {
                    locator: recipe_locator("result"),
                    value: BrowserRecipeValue::Literal {
                        value: "42".to_string(),
                    },
                },
            ],
        }],
    }
}

fn every_step_wait_recipe() -> BrowserRecipeV1 {
    let waits = vec![
        BrowserRecipeWait::Duration { duration_ms: 7 },
        BrowserRecipeWait::Url {
            value: literal("https://example.test/exact"),
            exact: true,
            timeout_ms: 101,
        },
        BrowserRecipeWait::Url {
            value: literal("https://example.test/contains"),
            exact: false,
            timeout_ms: 102,
        },
        BrowserRecipeWait::Load { timeout_ms: 103 },
        BrowserRecipeWait::NetworkIdle { timeout_ms: 104 },
        BrowserRecipeWait::ElementPresent {
            locator: recipe_locator("present"),
            timeout_ms: 105,
        },
        BrowserRecipeWait::ElementVisible {
            locator: recipe_locator("visible"),
            timeout_ms: 106,
        },
        BrowserRecipeWait::ElementHidden {
            locator: recipe_locator("hidden"),
            timeout_ms: 107,
        },
        BrowserRecipeWait::TextPresent {
            value: literal("ready"),
            timeout_ms: 108,
        },
        BrowserRecipeWait::TextAbsent {
            value: literal("error"),
            timeout_ms: 109,
        },
    ];
    BrowserRecipeV1 {
        schema_version: BROWSER_RECIPE_SCHEMA_VERSION,
        id: "executor-waits".to_string(),
        name: "Executor waits".to_string(),
        description: "Every portable replay wait variant".to_string(),
        start_url: "https://example.test/waits".to_string(),
        viewport: BrowserRecipeViewport::default(),
        inputs: Vec::new(),
        steps: waits
            .into_iter()
            .enumerate()
            .map(|(index, wait)| BrowserRecipeStep {
                id: format!("wait-{}", index + 1),
                action: BrowserRecipeAction::Reload,
                wait: Some(wait),
                assertions: Vec::new(),
            })
            .collect(),
    }
}

fn temp_directory(label: &str) -> PathBuf {
    let sequence = NEXT_TEMP_ROOT.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "devmanager-replay-executor-{label}-{}-{sequence}",
        std::process::id()
    ));
    std::fs::create_dir_all(&path).expect("create replay temp directory");
    path.canonicalize()
        .expect("canonical replay temp directory")
}

async fn assert_no_request(inbox: &mut devmanager::browser::BrowserCommandInbox) {
    assert!(
        tokio::time::timeout(Duration::from_millis(25), inbox.recv())
            .await
            .is_err(),
        "executor queued another command before the prior response"
    );
}

async fn respond_default_setup(
    inbox: &mut devmanager::browser::BrowserCommandInbox,
    start_url: &str,
) {
    let create = next_request(inbox, "setup create").await;
    assert_eq!(create.command(), &BrowserCommand::CreateTab { url: None });
    create.respond(Ok(workspace_response(
        "runtime-setup",
        "about:blank",
        BrowserViewport::default(),
    )));

    let viewport = next_request(inbox, "setup viewport").await;
    assert!(matches!(
        viewport.command(),
        BrowserCommand::UpdateViewport { tab_id, viewport }
            if tab_id == "runtime-setup" && viewport == &BrowserViewport::default()
    ));
    viewport.respond(Ok(workspace_response(
        "runtime-setup",
        "about:blank",
        BrowserViewport::default(),
    )));

    let navigate = next_request(inbox, "setup navigate").await;
    assert_eq!(
        navigate.command(),
        &BrowserCommand::Navigate {
            tab_id: "runtime-setup".to_string(),
            url: start_url.to_string(),
        }
    );
    navigate.respond(Ok(workspace_response(
        "runtime-setup",
        start_url,
        BrowserViewport::default(),
    )));
}

#[tokio::test]
async fn setup_uses_fresh_tab_and_awaits_each_exact_response() {
    let root = canonical_project_root();
    let plan = compile_browser_replay(&setup_recipe(), Vec::new()).unwrap();
    let coordinator = BrowserReplayCoordinator::default();
    let started = coordinator
        .start(workspace("valid-root"), plan)
        .expect("start replay");
    let (bridge, mut inbox) = browser_command_channel(8);
    let controller = bridge.bind(
        started.instance.workspace_key().clone(),
        Duration::from_secs(1),
    );
    let instance = started.instance.clone();
    let run = tokio::spawn({
        let coordinator = coordinator.clone();
        async move {
            execute_browser_replay(
                &controller,
                &coordinator,
                &instance,
                started.execution,
                BrowserInvocationActor::Agent,
                replay_resource_store(),
                &root,
            )
            .await
        }
    });

    let create = inbox.recv().await.expect("fresh setup-tab request");
    assert_eq!(create.command(), &BrowserCommand::CreateTab { url: None });
    assert_eq!(create.context().actor, BrowserInvocationActor::Agent);
    assert_eq!(
        create.context().declared_risk,
        devmanager::browser::BrowserRisk::Normal
    );
    assert!(!create.context().intent.contains("example.test"));
    let mut operation_ids = vec![create.context().operation_id.clone()];
    assert_no_request(&mut inbox).await;
    create.respond(Ok(workspace_response(
        "runtime-setup",
        "about:blank",
        BrowserViewport::default(),
    )));

    let viewport = inbox.recv().await.expect("setup viewport request");
    assert_eq!(
        viewport.command(),
        &BrowserCommand::UpdateViewport {
            tab_id: "runtime-setup".to_string(),
            viewport: BrowserViewport {
                width: 1440,
                height: 900,
                scale_percent: 125,
            },
        }
    );
    operation_ids.push(viewport.context().operation_id.clone());
    assert_no_request(&mut inbox).await;
    viewport.respond(Ok(workspace_response(
        "runtime-setup",
        "about:blank",
        BrowserViewport {
            width: 1440,
            height: 900,
            scale_percent: 125,
        },
    )));

    let navigate = inbox.recv().await.expect("setup navigation request");
    assert_eq!(
        navigate.command(),
        &BrowserCommand::Navigate {
            tab_id: "runtime-setup".to_string(),
            url: "https://example.test/replay-start".to_string(),
        }
    );
    operation_ids.push(navigate.context().operation_id.clone());
    assert_no_request(&mut inbox).await;
    navigate.respond(Ok(workspace_response(
        "runtime-setup",
        "https://example.test/replay-start",
        BrowserViewport {
            width: 1440,
            height: 900,
            scale_percent: 125,
        },
    )));

    let reload = inbox.recv().await.expect("first recipe step");
    assert_eq!(
        reload.command(),
        &BrowserCommand::Reload {
            tab_id: "runtime-setup".to_string(),
        }
    );
    operation_ids.push(reload.context().operation_id.clone());
    reload.respond(Ok(BrowserResponse::Acknowledged));

    operation_ids.sort();
    operation_ids.dedup();
    assert_eq!(
        operation_ids.len(),
        4,
        "every command needs a unique context"
    );
    assert_eq!(
        run.await
            .expect("executor task")
            .expect("safe replay result"),
        BrowserReplayProjection {
            workspace_key: workspace("valid-root"),
            instance_id: 1,
            recipe_id: "executor-setup".to_string(),
            status: BrowserReplayStatus::Completed,
            current_step_index: 1,
            total_steps: 1,
            current_step_id: None,
            unresolved_secret_inputs: Vec::new(),
            failure: None,
        }
    );

    let invalid_plan = compile_browser_replay(&setup_recipe(), Vec::new()).unwrap();
    let invalid = coordinator
        .start(workspace("invalid-root"), invalid_plan)
        .expect("start invalid-root replay");
    let invalid_instance = invalid.instance.clone();
    let observed_invalid_instance = invalid_instance.clone();
    let invalid_root = canonical_project_root().join("missing-replay-root");
    let invalid_controller = bridge.bind(
        invalid.instance.workspace_key().clone(),
        Duration::from_secs(1),
    );
    let invalid_run = tokio::spawn({
        let coordinator = coordinator.clone();
        async move {
            execute_browser_replay(
                &invalid_controller,
                &coordinator,
                &invalid_instance,
                invalid.execution,
                BrowserInvocationActor::Agent,
                replay_resource_store(),
                &invalid_root,
            )
            .await
        }
    });
    assert_no_request(&mut inbox).await;
    let failed = invalid_run
        .await
        .expect("invalid-root executor task")
        .expect_err("invalid root must fail before execution begins");
    assert_eq!(
        format!("{failed:?}"),
        "InvalidExecutionAuthority".to_string()
    );
    assert_eq!(
        coordinator
            .status(&observed_invalid_instance)
            .unwrap()
            .status,
        BrowserReplayStatus::Pending
    );
}

#[tokio::test]
async fn replay_preflight_rejects_untrusted_authority_without_state_or_browser_side_effects() {
    struct Case {
        label: &'static str,
        actor: BrowserInvocationActor,
        controller_workspace: &'static str,
        root: PathBuf,
    }

    let canonical_root = canonical_project_root();
    let cases = [
        Case {
            label: "workspace-mismatch",
            actor: BrowserInvocationActor::Agent,
            controller_workspace: "different-workspace",
            root: canonical_root.clone(),
        },
        Case {
            label: "user-actor",
            actor: BrowserInvocationActor::User,
            controller_workspace: "user-actor",
            root: canonical_root.clone(),
        },
        Case {
            label: "internal-actor",
            actor: BrowserInvocationActor::Internal,
            controller_workspace: "internal-actor",
            root: canonical_root.clone(),
        },
        Case {
            label: "invalid-root",
            actor: BrowserInvocationActor::Agent,
            controller_workspace: "invalid-root-preflight",
            root: canonical_root.join("missing-replay-root-preflight"),
        },
    ];

    for case in cases {
        let coordinator = BrowserReplayCoordinator::default();
        let started = coordinator
            .start(
                workspace(case.label),
                compile_browser_replay(&setup_recipe(), Vec::new()).unwrap(),
            )
            .expect("start authority replay");
        let instance = started.instance.clone();
        let (bridge, mut inbox) = browser_command_channel(4);
        let controller = bridge.bind(
            workspace(case.controller_workspace),
            Duration::from_millis(5),
        );

        let result = execute_browser_replay(
            &controller,
            &coordinator,
            &instance,
            started.execution,
            case.actor,
            replay_resource_store(),
            &case.root,
        )
        .await;

        assert_eq!(
            result.map_err(|error| format!("{error:?}")),
            Err("InvalidExecutionAuthority".to_string()),
            "{} must fail closed with a value-free authority error",
            case.label
        );
        assert_eq!(
            coordinator.status(&instance).unwrap().status,
            BrowserReplayStatus::Pending,
            "{} mutated replay state before authority validation",
            case.label
        );
        assert_no_request(&mut inbox).await;
    }
}

#[tokio::test]
async fn replay_preflight_rejects_a_foreign_execution_handle_without_side_effects() {
    let coordinator = BrowserReplayCoordinator::default();
    let foreign = coordinator
        .start(
            workspace("foreign-handle"),
            compile_browser_replay(&setup_recipe(), Vec::new()).unwrap(),
        )
        .expect("start foreign replay");
    let expected = coordinator
        .start(
            workspace("expected-handle"),
            compile_browser_replay(&setup_recipe(), Vec::new()).unwrap(),
        )
        .expect("start expected replay");
    let expected_instance = expected.instance.clone();
    let foreign_instance = foreign.instance.clone();
    let (bridge, mut inbox) = browser_command_channel(4);
    let controller = bridge.bind(
        expected_instance.workspace_key().clone(),
        Duration::from_secs(1),
    );

    let error = execute_browser_replay(
        &controller,
        &coordinator,
        &expected_instance,
        foreign.execution,
        BrowserInvocationActor::Agent,
        replay_resource_store(),
        &canonical_project_root(),
    )
    .await
    .expect_err("foreign execution handle must be rejected");

    assert_eq!(format!("{error:?}"), "StaleInstance");
    assert_eq!(
        coordinator.status(&expected_instance).unwrap().status,
        BrowserReplayStatus::Pending
    );
    assert_eq!(
        coordinator.status(&foreign_instance).unwrap().status,
        BrowserReplayStatus::Pending
    );
    assert_no_request(&mut inbox).await;
}

#[tokio::test]
async fn cancellation_while_setup_is_awaiting_discards_the_late_response() {
    let root = canonical_project_root();
    let plan = compile_browser_replay(&setup_recipe(), Vec::new()).unwrap();
    let coordinator = BrowserReplayCoordinator::default();
    let started = coordinator
        .start(workspace("cancel-setup"), plan)
        .expect("start replay");
    let instance = started.instance.clone();
    let lease = started.lease.clone();
    let (bridge, mut inbox) = browser_command_channel(4);
    let controller = bridge.bind(instance.workspace_key().clone(), Duration::from_secs(1));
    let run = tokio::spawn({
        let coordinator = coordinator.clone();
        async move {
            execute_browser_replay(
                &controller,
                &coordinator,
                &instance,
                started.execution,
                BrowserInvocationActor::Agent,
                replay_resource_store(),
                &root,
            )
            .await
        }
    });

    let create = inbox.recv().await.expect("in-flight setup request");
    let cancelled = coordinator
        .interrupt_workspace(&workspace("cancel-setup"))
        .expect("cancel exact workspace replay");
    assert_eq!(cancelled.status, BrowserReplayStatus::Cancelled);
    assert!(lease.is_cancelled());
    create.respond(Ok(workspace_response(
        "late-runtime-tab",
        "about:blank",
        BrowserViewport::default(),
    )));

    assert_no_request(&mut inbox).await;
    let outcome = run
        .await
        .expect("cancelled executor task")
        .expect("cancelled projection remains observable");
    assert_eq!(outcome.status, BrowserReplayStatus::Cancelled);
    assert_eq!(outcome.current_step_index, 0);
}

#[test]
fn cancellation_status_fence_checks_both_sides_of_the_running_projection_read() {
    let source = include_str!("../src/browser/replay_executor.rs").replace("\r\n", "\n");
    let start = source.find("fn terminal_projection(").unwrap();
    let end = source[start..].find("fn resolve_value").unwrap() + start;
    let fence = &source[start..end];
    let status = fence
        .find("coordinator\n        .status(instance)")
        .unwrap();
    let cancellation_checks = fence
        .match_indices("execution.is_cancelled()")
        .map(|(index, _)| index)
        .collect::<Vec<_>>();

    assert_eq!(
        cancellation_checks.len(),
        2,
        "the Running projection must be fenced before and after its status read"
    );
    assert!(
        cancellation_checks[0] < status && status < cancellation_checks[1],
        "a cancellation racing the status/lease split can return stale Running"
    );
}

#[tokio::test]
async fn secret_readiness_waits_on_the_existing_value_free_signal_before_host_work() {
    let root = canonical_project_root();
    let plan = compile_browser_replay(
        &secret_type_recipe(),
        vec![BrowserReplayPublicInput::new(
            "display-name",
            BrowserRecipeInputKind::Text,
            "ordinary user",
        )],
    )
    .unwrap();
    let coordinator = BrowserReplayCoordinator::default();
    let started = coordinator
        .start(workspace("secret-readiness"), plan)
        .unwrap();
    let instance = started.instance.clone();
    let (bridge, mut inbox) = browser_command_channel(8);
    let controller = bridge.bind(instance.workspace_key().clone(), Duration::from_secs(1));
    let run = tokio::spawn({
        let coordinator = coordinator.clone();
        let instance = instance.clone();
        async move {
            execute_browser_replay(
                &controller,
                &coordinator,
                &instance,
                started.execution,
                BrowserInvocationActor::Agent,
                replay_resource_store(),
                &root,
            )
            .await
        }
    });

    tokio::task::yield_now().await;
    assert!(
        !run.is_finished(),
        "executor must await the exact secret-ready signal"
    );
    assert_no_request(&mut inbox).await;

    let (mut prompt, _) = BrowserReplaySecretPromptVault::install(
        instance.clone(),
        coordinator
            .status(&instance)
            .unwrap()
            .unresolved_secret_inputs,
    )
    .unwrap();
    prompt
        .edit(&instance, "credential", "private value")
        .unwrap();
    let (submission, _) = prompt.submit(&instance).unwrap();
    coordinator.submit_secrets(&instance, submission).unwrap();

    respond_default_setup(&mut inbox, "https://example.test/sign-in").await;
    next_request(&mut inbox, "ordinary text Type")
        .await
        .respond(Ok(secret_type_action_response(1)));
    let secret = next_request(&mut inbox, "private secret Type").await;
    assert!(matches!(secret.validate_secret_sidecar(), Ok(Some(_))));
    secret.respond(Ok(secret_type_action_response(1)));

    let outcome = run.await.unwrap().unwrap();
    assert_eq!(outcome.status, BrowserReplayStatus::Completed);
}

#[tokio::test]
async fn secret_type_uses_the_private_sidecar_while_text_type_stays_an_ordinary_action() {
    const SECRET_SENTINEL: &str = "DM_EXECUTOR_SECRET_SENTINEL_74A9";
    let root = canonical_project_root();
    let plan = compile_browser_replay(
        &secret_type_recipe(),
        vec![BrowserReplayPublicInput::new(
            "display-name",
            BrowserRecipeInputKind::Text,
            "ordinary user",
        )],
    )
    .expect("compile secret Type replay");
    let coordinator = BrowserReplayCoordinator::default();
    let started = coordinator
        .start(workspace("secret-routing"), plan)
        .expect("start secret Type replay");
    assert_eq!(
        started.projection.status,
        BrowserReplayStatus::NeedsUserSecret
    );
    let instance = started.instance.clone();
    let (mut prompt, _) = BrowserReplaySecretPromptVault::install(
        instance.clone(),
        started.projection.unresolved_secret_inputs.clone(),
    )
    .expect("install exact secret prompt");
    prompt
        .edit(&instance, "credential", SECRET_SENTINEL)
        .expect("edit exact secret input");
    let (submission, _) = prompt.submit(&instance).expect("consume secret prompt");
    coordinator
        .submit_secrets(&instance, submission)
        .expect("install replay secrets");

    let (bridge, mut inbox) = browser_command_channel(8);
    let controller = bridge.bind(instance.workspace_key().clone(), Duration::from_secs(1));
    let run = tokio::spawn({
        let coordinator = coordinator.clone();
        let instance = instance.clone();
        async move {
            execute_browser_replay(
                &controller,
                &coordinator,
                &instance,
                started.execution,
                BrowserInvocationActor::Agent,
                replay_resource_store(),
                &root,
            )
            .await
        }
    });
    respond_default_setup(&mut inbox, "https://example.test/sign-in").await;

    let ordinary = next_request(&mut inbox, "ordinary text Type").await;
    assert_eq!(
        ordinary.command(),
        &BrowserCommand::Act {
            tab_id: "runtime-setup".to_string(),
            actions: vec![BrowserAction::Type {
                target: action_target("display-name"),
                text: "ordinary user".to_string(),
            }],
        }
    );
    assert!(matches!(ordinary.validate_secret_sidecar(), Ok(None)));
    ordinary.respond(Ok(secret_type_action_response(1)));

    let secret = next_request(&mut inbox, "private secret Type").await;
    assert_eq!(
        secret.command(),
        &BrowserCommand::SecretType {
            tab_id: "runtime-setup".to_string(),
            target: action_target("credential"),
            input_name: "credential".to_string(),
        }
    );
    assert_eq!(secret.context().declared_risk, BrowserRisk::AccountSecurity);
    assert!(matches!(secret.validate_secret_sidecar(), Ok(Some(_))));
    let safe_surfaces = format!(
        "{}\n{:?}\n{:?}\n{:?}",
        serde_json::to_string(secret.command()).unwrap(),
        secret.command(),
        secret.context(),
        coordinator.status(&instance).unwrap(),
    );
    assert!(!safe_surfaces.contains(SECRET_SENTINEL));
    assert!(!safe_surfaces.contains("ordinary user"));
    secret.respond(Ok(secret_type_action_response(1)));

    let outcome = run
        .await
        .expect("secret executor task")
        .expect("secret replay projection");
    assert_eq!(outcome.status, BrowserReplayStatus::Completed);
    assert_eq!(outcome.current_step_index, 2);
    assert_no_request(&mut inbox).await;
}

#[tokio::test]
async fn secret_type_requires_the_standard_exactly_one_action_response() {
    let root = canonical_project_root();
    let plan = compile_browser_replay(
        &secret_type_recipe(),
        vec![BrowserReplayPublicInput::new(
            "display-name",
            BrowserRecipeInputKind::Text,
            "ordinary user",
        )],
    )
    .unwrap();
    let coordinator = BrowserReplayCoordinator::default();
    let started = coordinator
        .start(workspace("secret-response"), plan)
        .unwrap();
    let instance = started.instance.clone();
    let (mut prompt, _) = BrowserReplaySecretPromptVault::install(
        instance.clone(),
        started.projection.unresolved_secret_inputs.clone(),
    )
    .unwrap();
    prompt
        .edit(&instance, "credential", "private value")
        .unwrap();
    let (submission, _) = prompt.submit(&instance).unwrap();
    coordinator.submit_secrets(&instance, submission).unwrap();
    let (bridge, mut inbox) = browser_command_channel(8);
    let controller = bridge.bind(instance.workspace_key().clone(), Duration::from_secs(1));
    let run = tokio::spawn({
        let coordinator = coordinator.clone();
        let instance = instance.clone();
        async move {
            execute_browser_replay(
                &controller,
                &coordinator,
                &instance,
                started.execution,
                BrowserInvocationActor::Agent,
                replay_resource_store(),
                &root,
            )
            .await
        }
    });
    respond_default_setup(&mut inbox, "https://example.test/sign-in").await;
    next_request(&mut inbox, "ordinary text Type")
        .await
        .respond(Ok(secret_type_action_response(1)));
    let secret = next_request(&mut inbox, "private secret Type").await;
    assert!(matches!(secret.validate_secret_sidecar(), Ok(Some(_))));
    secret.respond(Ok(secret_type_action_response(2)));

    let outcome = run.await.unwrap().unwrap();
    assert_eq!(outcome.status, BrowserReplayStatus::Failed);
    assert_eq!(outcome.failure, Some(BrowserReplayFailureCode::StepFailed));
    assert_eq!(outcome.current_step_index, 1);
    assert_no_request(&mut inbox).await;
}

#[tokio::test]
async fn secret_type_cancellation_closes_the_exact_sidecar_and_fences_the_late_response() {
    let root = canonical_project_root();
    let plan = compile_browser_replay(
        &secret_type_recipe(),
        vec![BrowserReplayPublicInput::new(
            "display-name",
            BrowserRecipeInputKind::Text,
            "ordinary user",
        )],
    )
    .unwrap();
    let coordinator = BrowserReplayCoordinator::default();
    let started = coordinator.start(workspace("secret-cancel"), plan).unwrap();
    let instance = started.instance.clone();
    let (mut prompt, _) = BrowserReplaySecretPromptVault::install(
        instance.clone(),
        started.projection.unresolved_secret_inputs.clone(),
    )
    .unwrap();
    prompt
        .edit(&instance, "credential", "private value")
        .unwrap();
    let (submission, _) = prompt.submit(&instance).unwrap();
    coordinator.submit_secrets(&instance, submission).unwrap();
    let (bridge, mut inbox) = browser_command_channel(8);
    let controller = bridge.bind(instance.workspace_key().clone(), Duration::from_secs(1));
    let run = tokio::spawn({
        let coordinator = coordinator.clone();
        let instance = instance.clone();
        async move {
            execute_browser_replay(
                &controller,
                &coordinator,
                &instance,
                started.execution,
                BrowserInvocationActor::Agent,
                replay_resource_store(),
                &root,
            )
            .await
        }
    });
    respond_default_setup(&mut inbox, "https://example.test/sign-in").await;
    next_request(&mut inbox, "ordinary text Type")
        .await
        .respond(Ok(secret_type_action_response(1)));
    let secret = next_request(&mut inbox, "private secret Type").await;
    assert!(matches!(secret.validate_secret_sidecar(), Ok(Some(_))));

    let cancelled = coordinator
        .interrupt_workspace(instance.workspace_key())
        .expect("cancel exact replay during secure host request");
    assert_eq!(cancelled.status, BrowserReplayStatus::Cancelled);
    assert!(matches!(
        secret.validate_secret_sidecar(),
        Err(BrowserError::InvalidInvocation { field }) if field == "secretSidecar"
    ));
    secret.respond(Ok(secret_type_action_response(1)));

    let outcome = run.await.unwrap().unwrap();
    assert_eq!(outcome.status, BrowserReplayStatus::Cancelled);
    assert_eq!(outcome.current_step_index, 1);
    assert_no_request(&mut inbox).await;
}

#[tokio::test]
async fn every_recipe_action_maps_to_one_existing_command() {
    let control = recipe_locator("control");
    let actions = vec![
        BrowserRecipeAction::CreateTab {
            tab: "tab-2".to_string(),
            url: Some(BrowserRecipeValue::Literal {
                value: "https://example.test/created".to_string(),
            }),
        },
        BrowserRecipeAction::SelectTab {
            tab: "tab-1".to_string(),
        },
        BrowserRecipeAction::SelectTab {
            tab: "tab-2".to_string(),
        },
        BrowserRecipeAction::CloseTab {
            tab: "tab-2".to_string(),
        },
        BrowserRecipeAction::Back,
        BrowserRecipeAction::Forward,
        BrowserRecipeAction::Reload,
        BrowserRecipeAction::SetViewport {
            viewport: BrowserRecipeViewport {
                width: 1024,
                height: 768,
                scale_percent: 100,
            },
        },
        BrowserRecipeAction::CdpMarker {
            method: "Runtime.enable".to_string(),
        },
        BrowserRecipeAction::Navigate {
            url: BrowserRecipeValue::Literal {
                value: "https://example.test/navigated".to_string(),
            },
        },
        BrowserRecipeAction::Click {
            locator: control.clone(),
        },
        BrowserRecipeAction::Hover {
            locator: control.clone(),
        },
        BrowserRecipeAction::Focus {
            locator: control.clone(),
        },
        BrowserRecipeAction::Type {
            locator: control.clone(),
            value: BrowserRecipeValue::Literal {
                value: "typed-value-sentinel".to_string(),
            },
        },
        BrowserRecipeAction::Clear {
            locator: control.clone(),
        },
        BrowserRecipeAction::Select {
            locator: control.clone(),
            values: vec![
                BrowserRecipeValue::Literal {
                    value: "first-option".to_string(),
                },
                BrowserRecipeValue::Literal {
                    value: "second-option".to_string(),
                },
            ],
        },
        BrowserRecipeAction::Keypress {
            locator: Some(control.clone()),
            key: BrowserRecipeValue::Literal {
                value: "Enter".to_string(),
            },
        },
        BrowserRecipeAction::Scroll {
            locator: Some(control.clone()),
            delta_x: 5,
            delta_y: 20,
        },
        BrowserRecipeAction::DragDrop {
            source: recipe_locator("source"),
            destination: recipe_locator("destination"),
        },
        BrowserRecipeAction::Download {
            locator: control.clone(),
        },
        BrowserRecipeAction::Wait {
            condition: BrowserRecipeWait::NetworkIdle { timeout_ms: 25 },
        },
        BrowserRecipeAction::Screenshot { full_page: true },
    ];
    let recipe = action_recipe(actions);
    let total_steps = recipe.steps.len();
    let plan = compile_browser_replay(&recipe, Vec::new()).unwrap();
    let coordinator = BrowserReplayCoordinator::default();
    let started = coordinator
        .start(workspace("all-actions"), plan)
        .expect("start action replay");
    let instance = started.instance.clone();
    let root = canonical_project_root();
    let (bridge, mut inbox) = browser_command_channel(32);
    let controller = bridge.bind(instance.workspace_key().clone(), Duration::from_secs(1));
    let run = tokio::spawn({
        let coordinator = coordinator.clone();
        async move {
            execute_browser_replay(
                &controller,
                &coordinator,
                &instance,
                started.execution,
                BrowserInvocationActor::Agent,
                replay_resource_store(),
                &root,
            )
            .await
        }
    });

    let default_viewport = BrowserViewport::default();
    let resized_viewport = BrowserViewport {
        width: 1024,
        height: 768,
        scale_percent: 100,
    };
    let setup_target = action_target("control");
    let action_response = || BrowserResponse::Action {
        result: BrowserActionResult {
            completed_actions: 1,
            revision: BrowserRevision(2),
        },
    };
    let mut expected = vec![
        (
            BrowserCommand::CreateTab { url: None },
            workspace_response("runtime-setup", "about:blank", default_viewport.clone()),
        ),
        (
            BrowserCommand::UpdateViewport {
                tab_id: "runtime-setup".to_string(),
                viewport: default_viewport.clone(),
            },
            workspace_response("runtime-setup", "about:blank", default_viewport.clone()),
        ),
        (
            BrowserCommand::Navigate {
                tab_id: "runtime-setup".to_string(),
                url: "https://example.test/action-start".to_string(),
            },
            workspace_response(
                "runtime-setup",
                "https://example.test/action-start",
                default_viewport.clone(),
            ),
        ),
        (
            BrowserCommand::CreateTab {
                url: Some("https://example.test/created".to_string()),
            },
            workspace_response_with_tabs(
                "runtime-created",
                vec![
                    (
                        "runtime-setup",
                        "https://example.test/action-start",
                        default_viewport.clone(),
                    ),
                    (
                        "runtime-created",
                        "https://example.test/created",
                        default_viewport.clone(),
                    ),
                ],
            ),
        ),
        (
            BrowserCommand::SelectTab {
                tab_id: "runtime-setup".to_string(),
            },
            workspace_response_with_tabs(
                "runtime-setup",
                vec![
                    (
                        "runtime-setup",
                        "https://example.test/action-start",
                        default_viewport.clone(),
                    ),
                    (
                        "runtime-created",
                        "https://example.test/created",
                        default_viewport.clone(),
                    ),
                ],
            ),
        ),
        (
            BrowserCommand::SelectTab {
                tab_id: "runtime-created".to_string(),
            },
            workspace_response_with_tabs(
                "runtime-created",
                vec![
                    (
                        "runtime-setup",
                        "https://example.test/action-start",
                        default_viewport.clone(),
                    ),
                    (
                        "runtime-created",
                        "https://example.test/created",
                        default_viewport.clone(),
                    ),
                ],
            ),
        ),
        (
            BrowserCommand::CloseTab {
                tab_id: "runtime-created".to_string(),
            },
            workspace_response(
                "runtime-setup",
                "https://example.test/action-start",
                default_viewport.clone(),
            ),
        ),
        (
            BrowserCommand::Back {
                tab_id: "runtime-setup".to_string(),
            },
            BrowserResponse::Acknowledged,
        ),
        (
            BrowserCommand::Forward {
                tab_id: "runtime-setup".to_string(),
            },
            BrowserResponse::Acknowledged,
        ),
        (
            BrowserCommand::Reload {
                tab_id: "runtime-setup".to_string(),
            },
            BrowserResponse::Acknowledged,
        ),
        (
            BrowserCommand::UpdateViewport {
                tab_id: "runtime-setup".to_string(),
                viewport: resized_viewport.clone(),
            },
            workspace_response(
                "runtime-setup",
                "https://example.test/action-start",
                resized_viewport.clone(),
            ),
        ),
        (
            BrowserCommand::Cdp {
                tab_id: "runtime-setup".to_string(),
                method: "Runtime.enable".to_string(),
                params: serde_json::json!({}),
            },
            BrowserResponse::Cdp {
                inline_result: None,
                resource: None,
            },
        ),
        (
            BrowserCommand::Navigate {
                tab_id: "runtime-setup".to_string(),
                url: "https://example.test/navigated".to_string(),
            },
            workspace_response(
                "runtime-setup",
                "https://example.test/navigated",
                resized_viewport.clone(),
            ),
        ),
        (
            BrowserCommand::Act {
                tab_id: "runtime-setup".to_string(),
                actions: vec![BrowserAction::Click {
                    target: setup_target.clone(),
                }],
            },
            action_response(),
        ),
        (
            BrowserCommand::Act {
                tab_id: "runtime-setup".to_string(),
                actions: vec![BrowserAction::Hover {
                    target: setup_target.clone(),
                }],
            },
            action_response(),
        ),
        (
            BrowserCommand::Act {
                tab_id: "runtime-setup".to_string(),
                actions: vec![BrowserAction::Focus {
                    target: setup_target.clone(),
                }],
            },
            action_response(),
        ),
        (
            BrowserCommand::Act {
                tab_id: "runtime-setup".to_string(),
                actions: vec![BrowserAction::Type {
                    target: setup_target.clone(),
                    text: "typed-value-sentinel".to_string(),
                }],
            },
            action_response(),
        ),
        (
            BrowserCommand::Act {
                tab_id: "runtime-setup".to_string(),
                actions: vec![BrowserAction::Clear {
                    target: setup_target.clone(),
                }],
            },
            action_response(),
        ),
        (
            BrowserCommand::Act {
                tab_id: "runtime-setup".to_string(),
                actions: vec![BrowserAction::Select {
                    target: setup_target.clone(),
                    values: vec!["first-option".to_string(), "second-option".to_string()],
                }],
            },
            action_response(),
        ),
        (
            BrowserCommand::Act {
                tab_id: "runtime-setup".to_string(),
                actions: vec![BrowserAction::Keypress {
                    target: Some(setup_target.clone()),
                    key: "Enter".to_string(),
                }],
            },
            action_response(),
        ),
        (
            BrowserCommand::Act {
                tab_id: "runtime-setup".to_string(),
                actions: vec![BrowserAction::Scroll {
                    target: Some(setup_target.clone()),
                    delta_x: 5,
                    delta_y: 20,
                }],
            },
            action_response(),
        ),
        (
            BrowserCommand::Act {
                tab_id: "runtime-setup".to_string(),
                actions: vec![BrowserAction::DragDrop {
                    source: action_target("source"),
                    destination: action_target("destination"),
                }],
            },
            action_response(),
        ),
        (
            BrowserCommand::Act {
                tab_id: "runtime-setup".to_string(),
                actions: vec![BrowserAction::Click {
                    target: setup_target,
                }],
            },
            action_response(),
        ),
        (
            BrowserCommand::Wait {
                tab_id: "runtime-setup".to_string(),
                condition: BrowserWaitCondition::NetworkIdle,
                timeout_ms: 25,
            },
            BrowserResponse::Wait {
                result: BrowserWaitResult {
                    matched: true,
                    elapsed_ms: 10,
                    revision: BrowserRevision(3),
                },
            },
        ),
        (
            BrowserCommand::Screenshot {
                tab_id: "runtime-setup".to_string(),
                mode: BrowserScreenshotMode::FullPage,
            },
            BrowserResponse::Screenshot {
                resource: BrowserResourceHandle {
                    id: BrowserResourceId("screenshot-resource".to_string()),
                    uri: "browser-resource://screenshot-resource".to_string(),
                    mime_type: "image/png".to_string(),
                    kind: BrowserResourceKind::Screenshot,
                    byte_size: 10,
                    created_at_epoch_ms: 1,
                    pinned: false,
                },
            },
        ),
    ];

    let mut operation_ids = Vec::new();
    for (index, (expected_command, response)) in expected.drain(..).enumerate() {
        let request = next_request(&mut inbox, &format!("command {index}")).await;
        assert_eq!(request.command(), &expected_command, "command {index}");
        assert_eq!(request.context().actor, BrowserInvocationActor::Agent);
        assert_eq!(
            request.context().declared_risk,
            devmanager::browser::BrowserRisk::Normal
        );
        assert!(!request.context().intent.contains("typed-value-sentinel"));
        assert!(!request.context().intent.contains("example.test"));
        operation_ids.push(request.context().operation_id.clone());
        request.respond(Ok(response));
    }
    operation_ids.sort();
    operation_ids.dedup();
    assert_eq!(operation_ids.len(), total_steps + 3);

    let completed = run
        .await
        .expect("action executor task")
        .expect("safe action replay result");
    assert_eq!(completed.status, BrowserReplayStatus::Completed);
    assert_eq!(completed.current_step_index, total_steps);
}

#[tokio::test]
async fn replay_cdp_declares_shared_conservative_method_risk() {
    let methods = [
        ("Browser.getVersion", BrowserRisk::Normal),
        ("Browser.close", BrowserRisk::Destructive),
        ("Experimental.unknownMutation", BrowserRisk::Destructive),
    ];
    let recipe = action_recipe(
        methods
            .iter()
            .map(|(method, _)| BrowserRecipeAction::CdpMarker {
                method: (*method).to_string(),
            })
            .collect(),
    );
    let coordinator = BrowserReplayCoordinator::default();
    let started = coordinator
        .start(
            workspace("cdp-method-risk"),
            compile_browser_replay(&recipe, Vec::new()).unwrap(),
        )
        .expect("start CDP risk replay");
    let instance = started.instance.clone();
    let root = canonical_project_root();
    let (bridge, mut inbox) = browser_command_channel(8);
    let controller = bridge.bind(instance.workspace_key().clone(), Duration::from_secs(1));
    let run = tokio::spawn({
        let coordinator = coordinator.clone();
        async move {
            execute_browser_replay(
                &controller,
                &coordinator,
                &instance,
                started.execution,
                BrowserInvocationActor::Agent,
                replay_resource_store(),
                &root,
            )
            .await
        }
    });

    respond_default_setup(&mut inbox, "https://example.test/action-start").await;
    for (method, expected_risk) in methods {
        let request = next_request(&mut inbox, "CDP method risk").await;
        assert!(matches!(
            request.command(),
            BrowserCommand::Cdp {
                method: actual_method,
                ..
            } if actual_method == method
        ));
        assert_eq!(
            request.context().declared_risk,
            expected_risk,
            "CDP risk for {method}"
        );
        request.respond(Ok(BrowserResponse::Cdp {
            inline_result: None,
            resource: None,
        }));
    }

    assert_eq!(
        run.await.unwrap().unwrap().status,
        BrowserReplayStatus::Completed
    );
}

#[tokio::test]
async fn every_recipe_step_wait_maps_to_the_typed_host_wait() {
    let recipe = every_step_wait_recipe();
    let total_steps = recipe.steps.len();
    let plan = compile_browser_replay(&recipe, Vec::new()).unwrap();
    let coordinator = BrowserReplayCoordinator::default();
    let started = coordinator
        .start(workspace("all-step-waits"), plan)
        .expect("start wait replay");
    let instance = started.instance.clone();
    let observed_instance = instance.clone();
    let root = canonical_project_root();
    let (bridge, mut inbox) = browser_command_channel(32);
    let controller = bridge.bind(instance.workspace_key().clone(), Duration::from_secs(1));
    let run = tokio::spawn({
        let coordinator = coordinator.clone();
        async move {
            execute_browser_replay(
                &controller,
                &coordinator,
                &instance,
                started.execution,
                BrowserInvocationActor::Agent,
                replay_resource_store(),
                &root,
            )
            .await
        }
    });

    respond_default_setup(&mut inbox, "https://example.test/waits").await;
    let expected = vec![
        (BrowserWaitCondition::Duration { duration_ms: 7 }, 7),
        (
            BrowserWaitCondition::Url {
                value: "https://example.test/exact".to_string(),
                exact: true,
            },
            101,
        ),
        (
            BrowserWaitCondition::Url {
                value: "https://example.test/contains".to_string(),
                exact: false,
            },
            102,
        ),
        (BrowserWaitCondition::Load, 103),
        (BrowserWaitCondition::NetworkIdle, 104),
        (
            BrowserWaitCondition::ElementPresent {
                target: action_target("present"),
            },
            105,
        ),
        (
            BrowserWaitCondition::ElementVisible {
                target: action_target("visible"),
            },
            106,
        ),
        (
            BrowserWaitCondition::ElementHidden {
                target: action_target("hidden"),
            },
            107,
        ),
        (
            BrowserWaitCondition::TextPresent {
                text: "ready".to_string(),
            },
            108,
        ),
        (
            BrowserWaitCondition::TextAbsent {
                text: "error".to_string(),
            },
            109,
        ),
    ];
    assert_eq!(expected.len(), total_steps);

    let mut operation_ids = Vec::new();
    for (index, (condition, timeout_ms)) in expected.into_iter().enumerate() {
        let action = next_request(&mut inbox, &format!("wait action {index}")).await;
        assert_eq!(
            action.command(),
            &BrowserCommand::Reload {
                tab_id: "runtime-setup".to_string(),
            }
        );
        operation_ids.push(action.context().operation_id.clone());
        action.respond(Ok(BrowserResponse::Acknowledged));

        let wait = next_request(&mut inbox, &format!("typed wait {index}")).await;
        assert_eq!(
            wait.command(),
            &BrowserCommand::Wait {
                tab_id: "runtime-setup".to_string(),
                condition,
                timeout_ms,
            }
        );
        assert_eq!(wait.context().actor, BrowserInvocationActor::Agent);
        assert_eq!(wait.context().declared_risk, BrowserRisk::Normal);
        assert_eq!(wait.context().intent, "replay step wait");
        operation_ids.push(wait.context().operation_id.clone());
        assert_eq!(
            coordinator
                .status(&observed_instance)
                .unwrap()
                .current_step_index,
            index,
            "step advanced before wait {index} completed"
        );
        wait.respond(Ok(BrowserResponse::Wait {
            result: BrowserWaitResult {
                matched: true,
                elapsed_ms: 1,
                revision: BrowserRevision(100 + index as u64),
            },
        }));
    }

    operation_ids.sort();
    operation_ids.dedup();
    assert_eq!(operation_ids.len(), total_steps * 2);
    let completed = run
        .await
        .expect("wait executor task")
        .expect("safe wait replay result");
    assert_eq!(completed.status, BrowserReplayStatus::Completed);
    assert_eq!(completed.current_step_index, total_steps);
}

#[tokio::test]
async fn replay_forwards_the_maximum_valid_recipe_wait_without_truncation() {
    let recipe = BrowserRecipeV1 {
        schema_version: BROWSER_RECIPE_SCHEMA_VERSION,
        id: "maximum-replay-wait".to_string(),
        name: "Maximum replay wait".to_string(),
        description: "Exercises the recipe wait ceiling".to_string(),
        start_url: "https://example.test/maximum-wait".to_string(),
        viewport: BrowserRecipeViewport::default(),
        inputs: Vec::new(),
        steps: vec![BrowserRecipeStep {
            id: "wait".to_string(),
            action: BrowserRecipeAction::Reload,
            wait: Some(BrowserRecipeWait::NetworkIdle {
                timeout_ms: 300_000,
            }),
            assertions: Vec::new(),
        }],
    };
    let coordinator = BrowserReplayCoordinator::default();
    let started = coordinator
        .start(
            workspace("maximum-replay-wait"),
            compile_browser_replay(&recipe, Vec::new()).unwrap(),
        )
        .expect("start maximum wait replay");
    let instance = started.instance.clone();
    let root = canonical_project_root();
    let (bridge, mut inbox) = browser_command_channel(8);
    let controller = bridge.bind(instance.workspace_key().clone(), Duration::from_secs(1));
    let run = tokio::spawn({
        let coordinator = coordinator.clone();
        async move {
            execute_browser_replay(
                &controller,
                &coordinator,
                &instance,
                started.execution,
                BrowserInvocationActor::Agent,
                replay_resource_store(),
                &root,
            )
            .await
        }
    });

    respond_default_setup(&mut inbox, "https://example.test/maximum-wait").await;
    next_request(&mut inbox, "maximum wait action")
        .await
        .respond(Ok(BrowserResponse::Acknowledged));
    let wait = next_request(&mut inbox, "maximum recipe wait").await;
    assert!(matches!(
        wait.command(),
        BrowserCommand::Wait {
            condition: BrowserWaitCondition::NetworkIdle,
            timeout_ms: 300_000,
            ..
        }
    ));
    wait.respond(Ok(BrowserResponse::Wait {
        result: BrowserWaitResult {
            matched: true,
            elapsed_ms: 300_000,
            revision: BrowserRevision(1),
        },
    }));
    assert_eq!(
        run.await.unwrap().unwrap().status,
        BrowserReplayStatus::Completed
    );
}

#[tokio::test]
async fn unmatched_step_wait_fails_without_advancing_or_running_later_work() {
    let plan = compile_browser_replay(&every_step_wait_recipe(), Vec::new()).unwrap();
    let coordinator = BrowserReplayCoordinator::default();
    let started = coordinator
        .start(workspace("unmatched-step-wait"), plan)
        .expect("start unmatched wait replay");
    let instance = started.instance.clone();
    let root = canonical_project_root();
    let (bridge, mut inbox) = browser_command_channel(8);
    let controller = bridge.bind(instance.workspace_key().clone(), Duration::from_secs(1));
    let run = tokio::spawn({
        let coordinator = coordinator.clone();
        async move {
            execute_browser_replay(
                &controller,
                &coordinator,
                &instance,
                started.execution,
                BrowserInvocationActor::Agent,
                replay_resource_store(),
                &root,
            )
            .await
        }
    });

    respond_default_setup(&mut inbox, "https://example.test/waits").await;
    let action = next_request(&mut inbox, "first wait action").await;
    action.respond(Ok(BrowserResponse::Acknowledged));
    let wait = next_request(&mut inbox, "first unmatched wait").await;
    assert!(matches!(
        wait.command(),
        BrowserCommand::Wait {
            condition: BrowserWaitCondition::Duration { duration_ms: 7 },
            timeout_ms: 7,
            ..
        }
    ));
    wait.respond(Ok(BrowserResponse::Wait {
        result: BrowserWaitResult {
            matched: false,
            elapsed_ms: 7,
            revision: BrowserRevision(1),
        },
    }));

    assert_no_request(&mut inbox).await;
    let failed = run
        .await
        .expect("unmatched wait executor task")
        .expect("safe unmatched wait projection");
    assert_eq!(failed.status, BrowserReplayStatus::Failed);
    assert_eq!(failed.current_step_index, 0);
    assert_eq!(failed.failure, Some(BrowserReplayFailureCode::StepFailed));
}

#[tokio::test]
async fn page_condition_timeout_is_assertion_failure_but_transport_timeout_is_step_failure() {
    #[derive(Clone, Copy)]
    enum Case {
        OrdinaryPageCondition,
        AssertionPageCondition,
        AssertionTransport,
    }

    for (index, case) in [
        Case::OrdinaryPageCondition,
        Case::AssertionPageCondition,
        Case::AssertionTransport,
    ]
    .into_iter()
    .enumerate()
    {
        let assertion = matches!(
            case,
            Case::AssertionPageCondition | Case::AssertionTransport
        );
        let recipe = BrowserRecipeV1 {
            schema_version: BROWSER_RECIPE_SCHEMA_VERSION,
            id: format!("timeout-case-{index}"),
            name: "Timeout case".to_string(),
            description: "Distinguishes page-condition and transport timeouts".to_string(),
            start_url: "https://example.test/timeout-case".to_string(),
            viewport: BrowserRecipeViewport::default(),
            inputs: Vec::new(),
            steps: vec![BrowserRecipeStep {
                id: "timeout-step".to_string(),
                action: BrowserRecipeAction::Reload,
                wait: (!assertion).then_some(BrowserRecipeWait::Duration { duration_ms: 7 }),
                assertions: assertion
                    .then(|| BrowserRecipeAssertion::Url {
                        value: literal("https://example.test/timeout-case"),
                        exact: true,
                    })
                    .into_iter()
                    .collect(),
            }],
        };
        let coordinator = BrowserReplayCoordinator::default();
        let started = coordinator
            .start(
                workspace(&format!("timeout-case-{index}")),
                compile_browser_replay(&recipe, Vec::new()).unwrap(),
            )
            .expect("start timeout replay");
        let instance = started.instance.clone();
        let root = canonical_project_root();
        let (bridge, mut inbox) = browser_command_channel(8);
        let controller = bridge.bind(instance.workspace_key().clone(), Duration::from_secs(1));
        let run = tokio::spawn({
            let coordinator = coordinator.clone();
            async move {
                execute_browser_replay(
                    &controller,
                    &coordinator,
                    &instance,
                    started.execution,
                    BrowserInvocationActor::Agent,
                    replay_resource_store(),
                    &root,
                )
                .await
            }
        });

        respond_default_setup(&mut inbox, "https://example.test/timeout-case").await;
        next_request(&mut inbox, "timeout case action")
            .await
            .respond(Ok(BrowserResponse::Acknowledged));
        let wait = next_request(&mut inbox, "timeout case wait").await;
        assert!(matches!(wait.command(), BrowserCommand::Wait { .. }));
        wait.respond(Err(BrowserError::Timeout {
            operation: match case {
                Case::OrdinaryPageCondition | Case::AssertionPageCondition => {
                    "pageCondition".to_string()
                }
                Case::AssertionTransport => "wait".to_string(),
            },
        }));

        let failed = run
            .await
            .expect("timeout executor task")
            .expect("timeout has a safe failure projection");
        assert_eq!(failed.status, BrowserReplayStatus::Failed);
        assert_eq!(
            failed.failure,
            Some(match case {
                Case::AssertionPageCondition => BrowserReplayFailureCode::AssertionFailed,
                Case::OrdinaryPageCondition | Case::AssertionTransport => {
                    BrowserReplayFailureCode::StepFailed
                }
            }),
            "timeout case {index} mapped to the wrong replay failure"
        );
    }
}

#[tokio::test]
async fn wrong_response_variant_fails_setup_action_wait_and_assertion() {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Phase {
        SetupCreate,
        SetupViewport,
        SetupNavigate,
        Action,
        Wait,
        Assertion,
    }

    for (case_index, phase) in [
        Phase::SetupCreate,
        Phase::SetupViewport,
        Phase::SetupNavigate,
        Phase::Action,
        Phase::Wait,
        Phase::Assertion,
    ]
    .into_iter()
    .enumerate()
    {
        let plan = compile_browser_replay(&assertion_recipe(), Vec::new()).unwrap();
        let coordinator = BrowserReplayCoordinator::default();
        let workspace_key = workspace(&format!("wrong-response-{case_index}"));
        let started = coordinator
            .start(workspace_key, plan)
            .expect("start wrong-response replay");
        let instance = started.instance.clone();
        let root = canonical_project_root();
        let (bridge, mut inbox) = browser_command_channel(8);
        let controller = bridge.bind(instance.workspace_key().clone(), Duration::from_secs(1));
        let run = tokio::spawn({
            let coordinator = coordinator.clone();
            async move {
                execute_browser_replay(
                    &controller,
                    &coordinator,
                    &instance,
                    started.execution,
                    BrowserInvocationActor::Agent,
                    replay_resource_store(),
                    &root,
                )
                .await
            }
        });

        let create = next_request(&mut inbox, "setup create response-shape case").await;
        if phase == Phase::SetupCreate {
            create.respond(Ok(BrowserResponse::Acknowledged));
        } else {
            create.respond(Ok(workspace_response(
                "runtime-setup",
                "about:blank",
                BrowserViewport::default(),
            )));
            let viewport = next_request(&mut inbox, "setup viewport response-shape case").await;
            if phase == Phase::SetupViewport {
                viewport.respond(Ok(BrowserResponse::Acknowledged));
            } else {
                viewport.respond(Ok(workspace_response(
                    "runtime-setup",
                    "about:blank",
                    BrowserViewport::default(),
                )));
                let navigate = next_request(&mut inbox, "setup navigate response-shape case").await;
                if phase == Phase::SetupNavigate {
                    navigate.respond(Ok(BrowserResponse::Acknowledged));
                } else {
                    navigate.respond(Ok(workspace_response(
                        "runtime-setup",
                        "https://example.test/assertions",
                        BrowserViewport::default(),
                    )));
                    let action = next_request(&mut inbox, "action response-shape case").await;
                    if phase == Phase::Action {
                        action.respond(Ok(BrowserResponse::Acknowledged));
                    } else {
                        action.respond(Ok(BrowserResponse::Action {
                            result: BrowserActionResult {
                                completed_actions: 1,
                                revision: BrowserRevision(1),
                            },
                        }));
                        let wait = next_request(&mut inbox, "wait response-shape case").await;
                        if phase == Phase::Wait {
                            wait.respond(Ok(BrowserResponse::Acknowledged));
                        } else {
                            wait.respond(Ok(BrowserResponse::Wait {
                                result: BrowserWaitResult {
                                    matched: true,
                                    elapsed_ms: 1,
                                    revision: BrowserRevision(2),
                                },
                            }));
                            let assertion =
                                next_request(&mut inbox, "assertion response-shape case").await;
                            assert!(phase == Phase::Assertion);
                            assertion.respond(Ok(BrowserResponse::Acknowledged));
                        }
                    }
                }
            }
        }

        assert_no_request(&mut inbox).await;
        let failed = run
            .await
            .expect("wrong-response executor task")
            .expect("safe wrong-response projection");
        assert_eq!(failed.status, BrowserReplayStatus::Failed);
        assert_eq!(failed.current_step_index, 0);
        assert_eq!(failed.failure, Some(BrowserReplayFailureCode::StepFailed));
    }
}

#[tokio::test]
async fn host_error_details_collapse_to_a_value_free_failure_projection() {
    let plan = compile_browser_replay(&assertion_recipe(), Vec::new()).unwrap();
    let coordinator = BrowserReplayCoordinator::default();
    let started = coordinator
        .start(workspace("host-error-redaction"), plan)
        .expect("start host-error replay");
    let instance = started.instance.clone();
    let root = canonical_project_root();
    let (bridge, mut inbox) = browser_command_channel(8);
    let controller = bridge.bind(instance.workspace_key().clone(), Duration::from_secs(1));
    let run = tokio::spawn({
        let coordinator = coordinator.clone();
        async move {
            execute_browser_replay(
                &controller,
                &coordinator,
                &instance,
                started.execution,
                BrowserInvocationActor::Agent,
                replay_resource_store(),
                &root,
            )
            .await
        }
    });

    respond_default_setup(&mut inbox, "https://example.test/assertions").await;
    let action = next_request(&mut inbox, "host-error action").await;
    action.respond(Err(BrowserError::Io {
        operation: "sentinel-host-operation".to_string(),
        path: PathBuf::from(r"C:\sensitive\sentinel-secret-file.txt"),
        message: "sentinel-secret-value".to_string(),
    }));

    assert_no_request(&mut inbox).await;
    let failed = run
        .await
        .expect("host-error executor task")
        .expect("safe host-error projection");
    assert_eq!(failed.status, BrowserReplayStatus::Failed);
    assert_eq!(failed.failure, Some(BrowserReplayFailureCode::StepFailed));
    for surface in [
        format!("{failed:?}"),
        serde_json::to_string(&failed).unwrap(),
    ] {
        assert!(!surface.contains("sentinel-host-operation"));
        assert!(!surface.contains("sentinel-secret-file"));
        assert!(!surface.contains("sentinel-secret-value"));
        assert!(!surface.contains("sensitive"));
    }
}

#[tokio::test]
async fn cancellation_and_replacement_fence_late_action_wait_and_assertion_responses() {
    #[derive(Clone, Copy)]
    enum Boundary {
        Action,
        Wait,
        Assertion,
    }
    #[derive(Clone, Copy)]
    enum Interruption {
        Cancel,
        Replace,
    }

    for (case_index, (interruption, boundary)) in [
        (Interruption::Cancel, Boundary::Action),
        (Interruption::Cancel, Boundary::Wait),
        (Interruption::Cancel, Boundary::Assertion),
        (Interruption::Replace, Boundary::Action),
        (Interruption::Replace, Boundary::Wait),
        (Interruption::Replace, Boundary::Assertion),
    ]
    .into_iter()
    .enumerate()
    {
        let workspace_key = workspace(&format!("fenced-late-response-{case_index}"));
        let plan = compile_browser_replay(&assertion_recipe(), Vec::new()).unwrap();
        let coordinator = BrowserReplayCoordinator::default();
        let started = coordinator
            .start(workspace_key.clone(), plan)
            .expect("start fenced replay");
        let old_instance = started.instance.clone();
        let observed_old_instance = old_instance.clone();
        let old_lease = started.lease.clone();
        let root = canonical_project_root();
        let (bridge, mut inbox) = browser_command_channel(8);
        let controller = bridge.bind(workspace_key.clone(), Duration::from_secs(1));
        let run = tokio::spawn({
            let coordinator = coordinator.clone();
            async move {
                execute_browser_replay(
                    &controller,
                    &coordinator,
                    &old_instance,
                    started.execution,
                    BrowserInvocationActor::Agent,
                    replay_resource_store(),
                    &root,
                )
                .await
            }
        });

        respond_default_setup(&mut inbox, "https://example.test/assertions").await;
        let in_flight = match boundary {
            Boundary::Action => next_request(&mut inbox, "in-flight action").await,
            Boundary::Wait | Boundary::Assertion => {
                let action = next_request(&mut inbox, "action before interruption").await;
                action.respond(Ok(BrowserResponse::Action {
                    result: BrowserActionResult {
                        completed_actions: 1,
                        revision: BrowserRevision(1),
                    },
                }));
                let wait = next_request(&mut inbox, "wait before interruption").await;
                if matches!(boundary, Boundary::Wait) {
                    wait
                } else {
                    wait.respond(Ok(BrowserResponse::Wait {
                        result: BrowserWaitResult {
                            matched: true,
                            elapsed_ms: 1,
                            revision: BrowserRevision(2),
                        },
                    }));
                    next_request(&mut inbox, "in-flight assertion").await
                }
            }
        };

        let replacement_instance = match interruption {
            Interruption::Cancel => {
                let projection = coordinator
                    .interrupt_workspace(&workspace_key)
                    .expect("cancel in-flight replay");
                assert_eq!(projection.status, BrowserReplayStatus::Cancelled);
                None
            }
            Interruption::Replace => {
                let replacement = coordinator
                    .replace(
                        workspace_key.clone(),
                        compile_browser_replay(&setup_recipe(), Vec::new()).unwrap(),
                    )
                    .expect("replace in-flight replay");
                assert_eq!(replacement.projection.status, BrowserReplayStatus::Pending);
                Some(replacement.instance)
            }
        };
        assert!(old_lease.is_cancelled());

        match boundary {
            Boundary::Action => in_flight.respond(Ok(BrowserResponse::Action {
                result: BrowserActionResult {
                    completed_actions: 1,
                    revision: BrowserRevision(10),
                },
            })),
            Boundary::Wait | Boundary::Assertion => in_flight.respond(Ok(BrowserResponse::Wait {
                result: BrowserWaitResult {
                    matched: true,
                    elapsed_ms: 1,
                    revision: BrowserRevision(11),
                },
            })),
        }

        assert_no_request(&mut inbox).await;
        let cancelled = run
            .await
            .expect("fenced executor task")
            .expect("retained cancellation projection");
        assert_eq!(cancelled.status, BrowserReplayStatus::Cancelled);
        assert_eq!(cancelled.current_step_index, 0);
        assert_eq!(
            coordinator.status(&observed_old_instance).unwrap(),
            cancelled
        );
        if let Some(replacement_instance) = replacement_instance {
            assert_eq!(
                coordinator.status(&replacement_instance).unwrap().status,
                BrowserReplayStatus::Pending
            );
        }
    }
}

#[tokio::test]
async fn interrupted_host_command_terminalizes_the_exact_replay_as_cancelled() {
    let plan = compile_browser_replay(&assertion_recipe(), Vec::new()).unwrap();
    let coordinator = BrowserReplayCoordinator::default();
    let started = coordinator
        .start(workspace("host-interrupted"), plan)
        .expect("start interrupted replay");
    let instance = started.instance.clone();
    let root = canonical_project_root();
    let (bridge, mut inbox) = browser_command_channel(8);
    let controller = bridge.bind(instance.workspace_key().clone(), Duration::from_secs(1));
    let run = tokio::spawn({
        let coordinator = coordinator.clone();
        async move {
            execute_browser_replay(
                &controller,
                &coordinator,
                &instance,
                started.execution,
                BrowserInvocationActor::Agent,
                replay_resource_store(),
                &root,
            )
            .await
        }
    });

    respond_default_setup(&mut inbox, "https://example.test/assertions").await;
    let action = next_request(&mut inbox, "interrupted action").await;
    action.respond(Err(BrowserError::Interrupted));
    assert_no_request(&mut inbox).await;
    let cancelled = run
        .await
        .expect("interrupted executor task")
        .expect("cancelled projection");
    assert_eq!(cancelled.status, BrowserReplayStatus::Cancelled);
    assert_eq!(cancelled.current_step_index, 0);
    assert_eq!(cancelled.failure, None);
}

#[tokio::test]
async fn tab_aliases_advance_only_on_exact_create_select_and_close_snapshots() {
    #[derive(Clone, Copy)]
    enum Case {
        Create,
        Select,
        Close,
    }

    for (case_index, case) in [Case::Create, Case::Select, Case::Close]
        .into_iter()
        .enumerate()
    {
        let mut actions = vec![BrowserRecipeAction::CreateTab {
            tab: "tab-2".to_string(),
            url: Some(literal("https://example.test/created")),
        }];
        match case {
            Case::Create => {}
            Case::Select => actions.push(BrowserRecipeAction::SelectTab {
                tab: "tab-1".to_string(),
            }),
            Case::Close => actions.push(BrowserRecipeAction::CloseTab {
                tab: "tab-2".to_string(),
            }),
        }
        let plan = compile_browser_replay(&action_recipe(actions), Vec::new()).unwrap();
        let coordinator = BrowserReplayCoordinator::default();
        let started = coordinator
            .start(
                workspace(&format!("tab-snapshot-mismatch-{case_index}")),
                plan,
            )
            .expect("start tab snapshot replay");
        let instance = started.instance.clone();
        let root = canonical_project_root();
        let (bridge, mut inbox) = browser_command_channel(8);
        let controller = bridge.bind(instance.workspace_key().clone(), Duration::from_secs(1));
        let run = tokio::spawn({
            let coordinator = coordinator.clone();
            async move {
                execute_browser_replay(
                    &controller,
                    &coordinator,
                    &instance,
                    started.execution,
                    BrowserInvocationActor::Agent,
                    replay_resource_store(),
                    &root,
                )
                .await
            }
        });

        respond_default_setup(&mut inbox, "https://example.test/action-start").await;
        let create = next_request(&mut inbox, "tab create").await;
        assert_eq!(
            create.command(),
            &BrowserCommand::CreateTab {
                url: Some("https://example.test/created".to_string()),
            }
        );
        if matches!(case, Case::Create) {
            create.respond(Ok(workspace_response_with_tabs(
                "runtime-setup",
                vec![
                    (
                        "runtime-setup",
                        "https://example.test/action-start",
                        BrowserViewport::default(),
                    ),
                    (
                        "runtime-created",
                        "https://example.test/created",
                        BrowserViewport::default(),
                    ),
                ],
            )));
        } else {
            create.respond(Ok(workspace_response_with_tabs(
                "runtime-created",
                vec![
                    (
                        "runtime-setup",
                        "https://example.test/action-start",
                        BrowserViewport::default(),
                    ),
                    (
                        "runtime-created",
                        "https://example.test/created",
                        BrowserViewport::default(),
                    ),
                ],
            )));
            let mutation = next_request(&mut inbox, "tab select or close").await;
            match case {
                Case::Create => unreachable!(),
                Case::Select => {
                    assert_eq!(
                        mutation.command(),
                        &BrowserCommand::SelectTab {
                            tab_id: "runtime-setup".to_string(),
                        }
                    );
                    mutation.respond(Ok(workspace_response_with_tabs(
                        "runtime-created",
                        vec![
                            (
                                "runtime-setup",
                                "https://example.test/action-start",
                                BrowserViewport::default(),
                            ),
                            (
                                "runtime-created",
                                "https://example.test/created",
                                BrowserViewport::default(),
                            ),
                        ],
                    )));
                }
                Case::Close => {
                    assert_eq!(
                        mutation.command(),
                        &BrowserCommand::CloseTab {
                            tab_id: "runtime-created".to_string(),
                        }
                    );
                    mutation.respond(Ok(workspace_response_with_tabs(
                        "runtime-setup",
                        vec![
                            (
                                "runtime-setup",
                                "https://example.test/action-start",
                                BrowserViewport::default(),
                            ),
                            (
                                "runtime-created",
                                "https://example.test/created",
                                BrowserViewport::default(),
                            ),
                        ],
                    )));
                }
            }
        }

        assert_no_request(&mut inbox).await;
        let failed = run
            .await
            .expect("tab snapshot executor task")
            .expect("safe tab snapshot projection");
        assert_eq!(failed.status, BrowserReplayStatus::Failed);
        assert_eq!(
            failed.current_step_index,
            if matches!(case, Case::Create) { 0 } else { 1 }
        );
        assert_eq!(failed.failure, Some(BrowserReplayFailureCode::StepFailed));
    }
}

#[tokio::test]
async fn legacy_create_tab_one_binds_the_recipe_alias_to_the_created_runtime_tab() {
    let recipe = action_recipe(vec![
        BrowserRecipeAction::CreateTab {
            tab: "tab-1".to_string(),
            url: Some(literal("https://example.test/legacy-created")),
        },
        BrowserRecipeAction::SelectTab {
            tab: "tab-1".to_string(),
        },
        BrowserRecipeAction::Reload,
    ]);
    let plan = compile_browser_replay(&recipe, Vec::new()).expect("compile legacy tab-1 recipe");
    let coordinator = BrowserReplayCoordinator::default();
    let started = coordinator
        .start(workspace("legacy-tab-one"), plan)
        .expect("start legacy tab-1 replay");
    let instance = started.instance.clone();
    let root = canonical_project_root();
    let (bridge, mut inbox) = browser_command_channel(8);
    let controller = bridge.bind(instance.workspace_key().clone(), Duration::from_secs(1));
    let run = tokio::spawn({
        let coordinator = coordinator.clone();
        async move {
            execute_browser_replay(
                &controller,
                &coordinator,
                &instance,
                started.execution,
                BrowserInvocationActor::Agent,
                replay_resource_store(),
                &root,
            )
            .await
        }
    });

    respond_default_setup(&mut inbox, "https://example.test/action-start").await;
    let created_snapshot = || {
        workspace_response_with_tabs(
            "runtime-created",
            vec![
                (
                    "runtime-setup",
                    "https://example.test/action-start",
                    BrowserViewport::default(),
                ),
                (
                    "runtime-created",
                    "https://example.test/legacy-created",
                    BrowserViewport::default(),
                ),
            ],
        )
    };
    let create = next_request(&mut inbox, "legacy tab-1 create").await;
    assert_eq!(
        create.command(),
        &BrowserCommand::CreateTab {
            url: Some("https://example.test/legacy-created".to_string()),
        }
    );
    create.respond(Ok(created_snapshot()));
    let select = next_request(&mut inbox, "legacy tab-1 select").await;
    assert_eq!(
        select.command(),
        &BrowserCommand::SelectTab {
            tab_id: "runtime-created".to_string(),
        }
    );
    select.respond(Ok(created_snapshot()));
    let reload = next_request(&mut inbox, "legacy tab-1 reload").await;
    assert_eq!(
        reload.command(),
        &BrowserCommand::Reload {
            tab_id: "runtime-created".to_string(),
        }
    );
    reload.respond(Ok(BrowserResponse::Acknowledged));

    let completed = run
        .await
        .expect("legacy tab executor task")
        .expect("safe legacy tab projection");
    assert_eq!(completed.status, BrowserReplayStatus::Completed);
    assert_eq!(completed.current_step_index, 3);
}

#[tokio::test]
async fn upload_resolves_at_execution_and_declares_containment_risk() {
    let root = temp_directory("inside-root");
    let file = root.join("inside.txt");
    std::fs::write(&file, b"inside replay upload").expect("write in-root upload fixture");
    let canonical_file = file.canonicalize().expect("canonical upload fixture");
    let plan = compile_browser_replay(
        &upload_recipe(),
        vec![BrowserReplayPublicInput::new(
            "upload-file",
            BrowserRecipeInputKind::File,
            "inside.txt",
        )],
    )
    .unwrap();
    let coordinator = BrowserReplayCoordinator::default();
    let started = coordinator
        .start(workspace("upload-inside"), plan)
        .expect("start upload replay");
    let instance = started.instance.clone();
    let (bridge, mut inbox) = browser_command_channel(8);
    let controller = bridge.bind(instance.workspace_key().clone(), Duration::from_secs(1));
    let run = tokio::spawn({
        let coordinator = coordinator.clone();
        let root = root.clone();
        async move {
            execute_browser_replay(
                &controller,
                &coordinator,
                &instance,
                started.execution,
                BrowserInvocationActor::Agent,
                replay_resource_store(),
                &root,
            )
            .await
        }
    });

    respond_default_setup(&mut inbox, "https://example.test/upload").await;
    let upload = next_request(&mut inbox, "in-root upload").await;
    assert_eq!(
        upload.command(),
        &BrowserCommand::Upload {
            tab_id: "runtime-setup".to_string(),
            target: action_target("upload"),
            paths: vec![canonical_file.clone()],
        }
    );
    assert_eq!(upload.context().declared_risk, BrowserRisk::Normal);
    assert_eq!(upload.local_project_root(), Some(root.as_path()));
    assert!(!upload.context().intent.contains("inside.txt"));
    assert!(!format!("{:?}", upload.context()).contains(&root.display().to_string()));
    upload.respond(Ok(BrowserResponse::Upload {
        result: BrowserUploadResult {
            files: vec![canonical_file],
            revision: BrowserRevision(4),
        },
    }));

    let completed = run
        .await
        .expect("upload executor task")
        .expect("safe upload replay result");
    assert_eq!(completed.status, BrowserReplayStatus::Completed);

    let outside_root = temp_directory("outside-root");
    let outside_file = outside_root.join("outside.txt");
    std::fs::write(&outside_file, b"outside replay upload").expect("write outside upload fixture");
    let canonical_outside_file = outside_file
        .canonicalize()
        .expect("canonical outside upload fixture");
    let outside_plan = compile_browser_replay(
        &upload_recipe(),
        vec![BrowserReplayPublicInput::new(
            "upload-file",
            BrowserRecipeInputKind::File,
            canonical_outside_file.to_string_lossy(),
        )],
    )
    .unwrap();
    let outside = coordinator
        .start(workspace("upload-outside"), outside_plan)
        .expect("start outside upload replay");
    let outside_instance = outside.instance.clone();
    let outside_controller = bridge.bind(
        outside.instance.workspace_key().clone(),
        Duration::from_secs(1),
    );
    let outside_run = tokio::spawn({
        let coordinator = coordinator.clone();
        let root = root.clone();
        async move {
            execute_browser_replay(
                &outside_controller,
                &coordinator,
                &outside_instance,
                outside.execution,
                BrowserInvocationActor::Agent,
                replay_resource_store(),
                &root,
            )
            .await
        }
    });

    respond_default_setup(&mut inbox, "https://example.test/upload").await;
    let outside_upload = next_request(&mut inbox, "outside-root upload").await;
    assert_eq!(
        outside_upload.command(),
        &BrowserCommand::Upload {
            tab_id: "runtime-setup".to_string(),
            target: action_target("upload"),
            paths: vec![canonical_outside_file.clone()],
        }
    );
    assert_eq!(
        outside_upload.context().declared_risk,
        BrowserRisk::OutsideWorkspaceFile
    );
    assert_eq!(outside_upload.local_project_root(), Some(root.as_path()));
    assert!(!outside_upload.context().intent.contains("outside.txt"));
    outside_upload.respond(Ok(BrowserResponse::Upload {
        result: BrowserUploadResult {
            files: vec![canonical_outside_file.clone()],
            revision: BrowserRevision(5),
        },
    }));
    let outside_completed = outside_run
        .await
        .expect("outside upload executor task")
        .expect("safe outside upload result");
    assert_eq!(outside_completed.status, BrowserReplayStatus::Completed);
    for surface in [
        format!("{outside_completed:?}"),
        serde_json::to_string(&outside_completed).unwrap(),
    ] {
        assert!(!surface.contains(&canonical_outside_file.display().to_string()));
    }

    let escaping_directory = root.join("escaping-directory");
    create_directory_redirect(&outside_root, &escaping_directory)
        .expect("OS must support the executor symlink containment regression");
    let escaping_plan = compile_browser_replay(
        &upload_recipe(),
        vec![BrowserReplayPublicInput::new(
            "upload-file",
            BrowserRecipeInputKind::File,
            Path::new("escaping-directory")
                .join("outside.txt")
                .to_string_lossy(),
        )],
    )
    .unwrap();
    let escaping = coordinator
        .start(workspace("upload-escaping-link"), escaping_plan)
        .expect("start escaping-link replay");
    let escaping_instance = escaping.instance.clone();
    let escaping_controller = bridge.bind(
        escaping.instance.workspace_key().clone(),
        Duration::from_secs(1),
    );
    let escaping_run = tokio::spawn({
        let coordinator = coordinator.clone();
        let root = root.clone();
        async move {
            execute_browser_replay(
                &escaping_controller,
                &coordinator,
                &escaping_instance,
                escaping.execution,
                BrowserInvocationActor::Agent,
                replay_resource_store(),
                &root,
            )
            .await
        }
    });
    respond_default_setup(&mut inbox, "https://example.test/upload").await;
    let escaping_upload = next_request(&mut inbox, "escaping-link upload").await;
    assert_eq!(
        escaping_upload.command(),
        &BrowserCommand::Upload {
            tab_id: "runtime-setup".to_string(),
            target: action_target("upload"),
            paths: vec![canonical_outside_file.clone()],
        }
    );
    assert_eq!(
        escaping_upload.context().declared_risk,
        BrowserRisk::OutsideWorkspaceFile
    );
    escaping_upload.respond(Ok(BrowserResponse::Upload {
        result: BrowserUploadResult {
            files: vec![canonical_outside_file.clone()],
            revision: BrowserRevision(6),
        },
    }));
    let escaping_completed = escaping_run
        .await
        .expect("escaping-link executor task")
        .expect("safe escaping-link upload result");
    assert_eq!(escaping_completed.status, BrowserReplayStatus::Completed);
    remove_directory_redirect(&escaping_directory);

    let missing_plan = compile_browser_replay(
        &upload_recipe(),
        vec![BrowserReplayPublicInput::new(
            "upload-file",
            BrowserRecipeInputKind::File,
            "missing.txt",
        )],
    )
    .unwrap();
    let missing = coordinator
        .start(workspace("upload-missing"), missing_plan)
        .expect("start missing upload replay");
    let missing_instance = missing.instance.clone();
    let missing_controller = bridge.bind(
        missing.instance.workspace_key().clone(),
        Duration::from_secs(1),
    );
    let missing_run = tokio::spawn({
        let coordinator = coordinator.clone();
        let root = root.clone();
        async move {
            execute_browser_replay(
                &missing_controller,
                &coordinator,
                &missing_instance,
                missing.execution,
                BrowserInvocationActor::Agent,
                replay_resource_store(),
                &root,
            )
            .await
        }
    });
    respond_default_setup(&mut inbox, "https://example.test/upload").await;
    assert_no_request(&mut inbox).await;
    let missing_failed = missing_run
        .await
        .expect("missing upload executor task")
        .expect("safe missing upload result");
    assert_eq!(missing_failed.status, BrowserReplayStatus::Failed);
    assert_eq!(
        missing_failed.failure,
        Some(BrowserReplayFailureCode::StepFailed)
    );
    for surface in [
        format!("{missing_failed:?}"),
        serde_json::to_string(&missing_failed).unwrap(),
    ] {
        assert!(!surface.contains("missing.txt"));
        assert!(!surface.contains(&root.display().to_string()));
    }

    std::fs::remove_dir_all(&outside_root).expect("remove outside upload fixture root");
    std::fs::remove_dir_all(&root).expect("remove upload fixture root");
}

#[tokio::test]
async fn replay_runs_action_wait_assertions_and_advances_only_after_success() {
    let recipe = assertion_recipe();
    let assertion_count = recipe.steps[0].assertions.len();
    let plan = compile_browser_replay(&recipe, Vec::new()).unwrap();
    let coordinator = BrowserReplayCoordinator::default();
    let started = coordinator
        .start(workspace("assertion-order"), plan)
        .expect("start assertion replay");
    let instance = started.instance.clone();
    let observed_instance = instance.clone();
    let root = canonical_project_root();
    let (bridge, mut inbox) = browser_command_channel(16);
    let controller = bridge.bind(instance.workspace_key().clone(), Duration::from_secs(1));
    let run = tokio::spawn({
        let coordinator = coordinator.clone();
        async move {
            execute_browser_replay(
                &controller,
                &coordinator,
                &instance,
                started.execution,
                BrowserInvocationActor::Agent,
                replay_resource_store(),
                &root,
            )
            .await
        }
    });

    respond_default_setup(&mut inbox, "https://example.test/assertions").await;
    let action = next_request(&mut inbox, "assertion-step action").await;
    assert_eq!(
        action.command(),
        &BrowserCommand::Act {
            tab_id: "runtime-setup".to_string(),
            actions: vec![BrowserAction::Click {
                target: action_target("submit"),
            }],
        }
    );
    action.respond(Ok(BrowserResponse::Action {
        result: BrowserActionResult {
            completed_actions: 1,
            revision: BrowserRevision(10),
        },
    }));
    assert_eq!(
        coordinator
            .status(&observed_instance)
            .unwrap()
            .current_step_index,
        0
    );

    let expected_waits = vec![
        (
            BrowserWaitCondition::Url {
                value: "https://example.test/after-click".to_string(),
                exact: true,
            },
            2_000,
        ),
        (
            BrowserWaitCondition::Url {
                value: "https://example.test/after-click".to_string(),
                exact: true,
            },
            250,
        ),
        (
            BrowserWaitCondition::Title {
                value: "Ready".to_string(),
                exact: false,
            },
            250,
        ),
        (
            BrowserWaitCondition::TextPresent {
                text: "Saved".to_string(),
            },
            250,
        ),
        (
            BrowserWaitCondition::TextAbsent {
                text: "Error".to_string(),
            },
            250,
        ),
        (
            BrowserWaitCondition::ElementPresent {
                target: action_target("status"),
            },
            250,
        ),
        (
            BrowserWaitCondition::ElementAbsent {
                target: action_target("removed"),
            },
            250,
        ),
        (
            BrowserWaitCondition::ElementVisible {
                target: action_target("visible"),
            },
            250,
        ),
        (
            BrowserWaitCondition::ElementHidden {
                target: action_target("hidden"),
            },
            250,
        ),
        (
            BrowserWaitCondition::ElementValue {
                target: action_target("result"),
                value: "42".to_string(),
            },
            250,
        ),
    ];
    assert_eq!(expected_waits.len(), assertion_count + 1);
    for (index, (condition, timeout_ms)) in expected_waits.into_iter().enumerate() {
        let wait = next_request(&mut inbox, &format!("ordered wait {index}")).await;
        assert_eq!(
            wait.command(),
            &BrowserCommand::Wait {
                tab_id: "runtime-setup".to_string(),
                condition,
                timeout_ms,
            }
        );
        assert!(!wait.context().intent.contains("after-click"));
        assert!(!wait.context().intent.contains("42"));
        assert_eq!(
            coordinator
                .status(&observed_instance)
                .unwrap()
                .current_step_index,
            0,
            "step advanced before wait/assertion {index}"
        );
        wait.respond(Ok(BrowserResponse::Wait {
            result: BrowserWaitResult {
                matched: true,
                elapsed_ms: 1,
                revision: BrowserRevision(11 + index as u64),
            },
        }));
    }

    let completed = run
        .await
        .expect("assertion executor task")
        .expect("safe assertion replay result");
    assert_eq!(completed.status, BrowserReplayStatus::Completed);
    assert_eq!(completed.current_step_index, 1);
}

#[tokio::test]
async fn assertion_failure_stops_before_advance_or_later_assertions() {
    let plan = compile_browser_replay(&assertion_recipe(), Vec::new()).unwrap();
    let coordinator = BrowserReplayCoordinator::default();
    let started = coordinator
        .start(workspace("assertion-failure"), plan)
        .expect("start assertion failure replay");
    let instance = started.instance.clone();
    let root = canonical_project_root();
    let (bridge, mut inbox) = browser_command_channel(8);
    let controller = bridge.bind(instance.workspace_key().clone(), Duration::from_secs(1));
    let run = tokio::spawn({
        let coordinator = coordinator.clone();
        async move {
            execute_browser_replay(
                &controller,
                &coordinator,
                &instance,
                started.execution,
                BrowserInvocationActor::Agent,
                replay_resource_store(),
                &root,
            )
            .await
        }
    });

    respond_default_setup(&mut inbox, "https://example.test/assertions").await;
    let action = next_request(&mut inbox, "failing assertion action").await;
    action.respond(Ok(BrowserResponse::Action {
        result: BrowserActionResult {
            completed_actions: 1,
            revision: BrowserRevision(20),
        },
    }));
    let optional_wait = next_request(&mut inbox, "optional step wait").await;
    optional_wait.respond(Ok(BrowserResponse::Wait {
        result: BrowserWaitResult {
            matched: true,
            elapsed_ms: 1,
            revision: BrowserRevision(21),
        },
    }));
    let assertion = next_request(&mut inbox, "first assertion").await;
    assert!(matches!(
        assertion.command(),
        BrowserCommand::Wait {
            condition: BrowserWaitCondition::Url { .. },
            timeout_ms: 250,
            ..
        }
    ));
    assertion.respond(Ok(BrowserResponse::Wait {
        result: BrowserWaitResult {
            matched: false,
            elapsed_ms: 250,
            revision: BrowserRevision(22),
        },
    }));

    assert_no_request(&mut inbox).await;
    let failed = run
        .await
        .expect("assertion failure executor task")
        .expect("safe assertion failure result");
    assert_eq!(failed.status, BrowserReplayStatus::Failed);
    assert_eq!(failed.current_step_index, 0);
    assert_eq!(
        failed.failure,
        Some(BrowserReplayFailureCode::AssertionFailed)
    );
}
