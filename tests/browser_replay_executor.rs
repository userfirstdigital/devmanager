use devmanager::browser::{
    browser_command_channel, compile_browser_replay, execute_browser_replay, BrowserCommand,
    BrowserInvocationActor, BrowserRecipeAction, BrowserRecipeStep, BrowserRecipeV1,
    BrowserRecipeViewport, BrowserReplayCoordinator, BrowserReplayFailureCode,
    BrowserReplayProjection, BrowserReplayStatus, BrowserResponse, BrowserRevision,
    BrowserTabSnapshot, BrowserViewport, BrowserWorkspaceKey, BrowserWorkspaceMutation,
    BrowserWorkspaceSnapshot, BROWSER_RECIPE_SCHEMA_VERSION,
};
use std::path::{Path, PathBuf};
use std::time::Duration;

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
    BrowserResponse::Workspace {
        mutation: BrowserWorkspaceMutation {
            revision: BrowserRevision(1),
            snapshot: BrowserWorkspaceSnapshot {
                revision: BrowserRevision(1),
                tabs: vec![BrowserTabSnapshot {
                    id: tab_id.to_string(),
                    title: "Replay".to_string(),
                    url: url.to_string(),
                    viewport,
                }],
                selected_tab_id: Some(tab_id.to_string()),
                ..BrowserWorkspaceSnapshot::default()
            },
        },
    }
}

async fn assert_no_request(inbox: &mut devmanager::browser::BrowserCommandInbox) {
    assert!(
        tokio::time::timeout(Duration::from_millis(25), inbox.recv())
            .await
            .is_err(),
        "executor queued another command before the prior response"
    );
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
                &invalid_root,
            )
            .await
        }
    });
    assert_no_request(&mut inbox).await;
    let failed = invalid_run
        .await
        .expect("invalid-root executor task")
        .expect("safe failed projection");
    assert_eq!(failed.status, BrowserReplayStatus::Failed);
    assert_eq!(failed.failure, Some(BrowserReplayFailureCode::StepFailed));
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
