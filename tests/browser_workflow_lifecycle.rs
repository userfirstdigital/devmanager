use devmanager::browser::{
    browser_command_channel, compile_browser_replay, BrowserCommand, BrowserHostControl,
    BrowserHostEvent, BrowserRecipeAction, BrowserRecipeStep, BrowserRecipeV1,
    BrowserRecipeViewport, BrowserReplayCoordinator, BrowserReplayStatus, BrowserResponse,
    BrowserUserInputKind, BrowserWorkspaceKey, BROWSER_RECIPE_SCHEMA_VERSION,
};
use std::time::Duration;

fn workspace(project: &str, conversation: &str) -> BrowserWorkspaceKey {
    BrowserWorkspaceKey::new(project, conversation).unwrap()
}

fn replay_plan(label: &str) -> devmanager::browser::BrowserReplayPlan {
    compile_browser_replay(
        &BrowserRecipeV1 {
            schema_version: BROWSER_RECIPE_SCHEMA_VERSION,
            id: format!("lifecycle-{label}"),
            name: "Lifecycle replay".to_string(),
            description: "Shared bridge cancellation fixture".to_string(),
            start_url: "https://example.test/start".to_string(),
            viewport: BrowserRecipeViewport::default(),
            inputs: Vec::new(),
            steps: vec![BrowserRecipeStep {
                id: "reload".to_string(),
                action: BrowserRecipeAction::Reload,
                wait: None,
                assertions: Vec::new(),
            }],
        },
        Vec::new(),
    )
    .unwrap()
}

fn assert_cancelled(
    coordinator: &BrowserReplayCoordinator,
    instance_id: u64,
    key: &BrowserWorkspaceKey,
) {
    let instance = coordinator.exact_instance(key, instance_id).unwrap();
    assert_eq!(
        coordinator.status(&instance).unwrap().status,
        BrowserReplayStatus::Cancelled
    );
}

#[test]
fn browser_bridge_inbox_and_controllers_share_one_replay_coordinator() {
    let first = workspace("shared-project", "first");
    let second = workspace("shared-project", "second");
    let (bridge, inbox) = browser_command_channel(4);
    let coordinator = bridge.replay_coordinator();

    let first_replay = coordinator
        .start(first.clone(), replay_plan("first"))
        .unwrap();
    inbox.interrupt_workspace(&first);
    assert_cancelled(&coordinator, first_replay.instance.id(), &first);

    let second_replay = coordinator
        .start(second.clone(), replay_plan("second"))
        .unwrap();
    let controller = bridge.bind(second.clone(), Duration::from_secs(1));
    controller.interrupt_tab("runtime-tab");
    assert_cancelled(&coordinator, second_replay.instance.id(), &second);
}

#[test]
fn browser_every_host_control_cancels_the_matching_replay_scope() {
    let first = workspace("project-a", "first");
    let second = workspace("project-a", "second");
    let other = workspace("project-b", "other");
    let (bridge, _inbox) = browser_command_channel(4);
    let coordinator = bridge.replay_coordinator();

    let tab = coordinator
        .start(first.clone(), replay_plan("tab"))
        .unwrap();
    bridge.interrupt_tab(&first, "runtime-tab");
    assert_cancelled(&coordinator, tab.instance.id(), &first);
    assert_eq!(
        bridge.drain_host_controls(),
        vec![BrowserHostControl::InterruptTab {
            workspace_key: first.clone(),
            tab_id: "runtime-tab".to_string(),
        }]
    );

    let workspace_replay = coordinator
        .start(first.clone(), replay_plan("workspace"))
        .unwrap();
    bridge.interrupt_workspace(&first);
    assert_cancelled(&coordinator, workspace_replay.instance.id(), &first);
    assert_eq!(
        bridge.drain_host_controls(),
        vec![BrowserHostControl::InterruptWorkspace {
            workspace_key: first.clone(),
        }]
    );

    let project_first = coordinator
        .start(first.clone(), replay_plan("project-first"))
        .unwrap();
    let project_second = coordinator
        .start(second.clone(), replay_plan("project-second"))
        .unwrap();
    let isolated = coordinator
        .start(other.clone(), replay_plan("isolated"))
        .unwrap();
    bridge.interrupt_project("project-a");
    assert_cancelled(&coordinator, project_first.instance.id(), &first);
    assert_cancelled(&coordinator, project_second.instance.id(), &second);
    assert_eq!(
        coordinator.status(&isolated.instance).unwrap().status,
        BrowserReplayStatus::Pending
    );
    assert_eq!(
        bridge.drain_host_controls(),
        vec![BrowserHostControl::InterruptProject {
            project_id: "project-a".to_string(),
        }]
    );
}

#[tokio::test]
async fn browser_direct_input_and_each_lifecycle_command_cancel_before_late_work_can_advance() {
    for (label, command) in [
        (
            "stop-tab",
            BrowserCommand::Stop {
                tab_id: Some("runtime-tab".to_string()),
            },
        ),
        ("stop-workspace", BrowserCommand::Stop { tab_id: None }),
        (
            "close-tab",
            BrowserCommand::CloseTab {
                tab_id: "runtime-tab".to_string(),
            },
        ),
        ("reset-workspace", BrowserCommand::ResetWorkspace),
        ("clear-project", BrowserCommand::ClearProjectProfile),
    ] {
        let key = workspace(&format!("project-{label}"), "conversation");
        let (bridge, _inbox) = browser_command_channel(4);
        let coordinator = bridge.replay_coordinator();
        let started = coordinator.start(key.clone(), replay_plan(label)).unwrap();
        let controller = bridge.bind(key.clone(), Duration::from_secs(1));

        controller.notify(command).await.unwrap();
        assert_cancelled(&coordinator, started.instance.id(), &key);
    }

    let key = workspace("input-project", "conversation");
    let (bridge, _inbox) = browser_command_channel(4);
    let coordinator = bridge.replay_coordinator();
    let started = coordinator
        .start(key.clone(), replay_plan("direct-input"))
        .unwrap();
    let isolated_key = workspace("input-project", "other-conversation");
    let isolated = coordinator
        .start(isolated_key, replay_plan("direct-input-isolated"))
        .unwrap();
    bridge.observe_host_event(&BrowserHostEvent::UserInput {
        workspace_key: key.clone(),
        tab_id: "runtime-tab".to_string(),
        kind: BrowserUserInputKind::Pointer,
    });
    assert_cancelled(&coordinator, started.instance.id(), &key);
    assert_eq!(
        coordinator.status(&isolated.instance).unwrap().status,
        BrowserReplayStatus::Pending
    );
}

#[tokio::test]
async fn browser_lifecycle_cancellation_wins_before_a_retained_late_host_response() {
    let key = workspace("late-project", "conversation");
    let (bridge, mut inbox) = browser_command_channel(4);
    let coordinator = bridge.replay_coordinator();
    let started = coordinator.start(key.clone(), replay_plan("late")).unwrap();
    let controller = bridge.bind(key.clone(), Duration::from_secs(1));
    let pending = tokio::spawn(async move {
        controller
            .request(BrowserCommand::Reload {
                tab_id: "runtime-tab".to_string(),
            })
            .await
    });
    let request = inbox.recv().await.expect("retained host request");

    bridge.interrupt_tab(&key, "runtime-tab");
    assert_cancelled(&coordinator, started.instance.id(), &key);
    request.respond(Ok(BrowserResponse::Acknowledged));

    assert_eq!(
        pending.await.unwrap(),
        Err(devmanager::browser::BrowserError::Interrupted)
    );
}

#[tokio::test]
async fn browser_interrupt_all_cancels_every_replay_and_pending_request_without_a_second_owner() {
    let first = workspace("project-a", "first");
    let second = workspace("project-b", "second");
    let (bridge, mut inbox) = browser_command_channel(4);
    let coordinator = bridge.replay_coordinator();
    let first_replay = coordinator
        .start(first.clone(), replay_plan("all-first"))
        .unwrap();
    let second_replay = coordinator
        .start(second.clone(), replay_plan("all-second"))
        .unwrap();
    let controller = bridge.bind(first.clone(), Duration::from_secs(1));
    let pending = tokio::spawn(async move {
        controller
            .request(BrowserCommand::Reload {
                tab_id: "runtime-tab".to_string(),
            })
            .await
    });
    let request = inbox.recv().await.expect("pending request before shutdown");

    bridge.interrupt_all();

    assert_cancelled(&coordinator, first_replay.instance.id(), &first);
    assert_cancelled(&coordinator, second_replay.instance.id(), &second);
    request.respond(Ok(BrowserResponse::Acknowledged));
    assert_eq!(
        pending.await.unwrap(),
        Err(devmanager::browser::BrowserError::Interrupted)
    );
}
