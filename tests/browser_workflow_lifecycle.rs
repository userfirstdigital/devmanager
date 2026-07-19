use devmanager::browser::{
    browser_command_channel, compile_browser_replay, route_browser_request, BrowserCommand,
    BrowserHostControl, BrowserHostEvent, BrowserRecipeAction, BrowserRecipeInput,
    BrowserRecipeInputKind, BrowserRecipeLocator, BrowserRecipeStep, BrowserRecipeV1,
    BrowserRecipeValue, BrowserRecipeViewport, BrowserReplayCoordinator, BrowserReplaySecretError,
    BrowserReplaySecretPromptVault, BrowserReplayStatus, BrowserResponse, BrowserUserInputKind,
    BrowserWorkspaceKey, BROWSER_RECIPE_SCHEMA_VERSION,
};
use std::time::Duration;

const SECRET_SENTINEL: &str = "task-4-secret-sentinel";

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

fn secret_replay_plan(label: &str) -> devmanager::browser::BrowserReplayPlan {
    compile_browser_replay(
        &BrowserRecipeV1 {
            schema_version: BROWSER_RECIPE_SCHEMA_VERSION,
            id: format!("lifecycle-secret-{label}"),
            name: "Lifecycle secret replay".to_string(),
            description: "Terminalization and zeroization fixture".to_string(),
            start_url: "https://example.test/secret".to_string(),
            viewport: BrowserRecipeViewport::default(),
            inputs: vec![BrowserRecipeInput {
                name: "password".to_string(),
                kind: BrowserRecipeInputKind::Secret,
                default_value: None,
            }],
            steps: vec![BrowserRecipeStep {
                id: "type-secret".to_string(),
                action: BrowserRecipeAction::Type {
                    locator: BrowserRecipeLocator {
                        test_id: Some("password".to_string()),
                        ..BrowserRecipeLocator::default()
                    },
                    value: BrowserRecipeValue::Input {
                        name: "password".to_string(),
                    },
                },
                wait: None,
                assertions: Vec::new(),
            }],
        },
        Vec::new(),
    )
    .unwrap()
}

fn install_secret(
    coordinator: &BrowserReplayCoordinator,
    started: &devmanager::browser::BrowserReplayStart,
) {
    assert_eq!(
        started.projection.status,
        BrowserReplayStatus::NeedsUserSecret
    );
    let (mut prompt, _) = BrowserReplaySecretPromptVault::install(
        started.instance.clone(),
        vec!["password".to_string()],
    )
    .unwrap();
    prompt
        .edit(&started.instance, "password", SECRET_SENTINEL)
        .unwrap();
    let (submission, _) = prompt.submit(&started.instance).unwrap();
    assert_eq!(
        coordinator
            .submit_secrets(&started.instance, submission)
            .unwrap()
            .status,
        BrowserReplayStatus::Running
    );
    assert!(started.execution.secret_lease("password").is_ok());
}

#[derive(Clone, Copy, Debug)]
enum LifecycleBoundary {
    StopTab,
    StopWorkspace,
    LogicalTabClose,
    ResetWorkspace,
    ClearProject,
    DirectInput,
    SelectConversation,
    RestartServer,
    KillPortRestart,
    RestartAiConversation,
    RestartSsh,
    CloseConversation,
    DeleteProject,
    QuitApplication,
}

async fn assert_boundary_terminalizes_before_late_response(boundary: LifecycleBoundary) {
    let label = format!("{boundary:?}").to_ascii_lowercase();
    let key = workspace(&format!("project-{label}"), "conversation");
    let same_project = workspace(&format!("project-{label}"), "sibling");
    let isolated_key = workspace(&format!("isolated-{label}"), "conversation");
    let (bridge, mut inbox) = browser_command_channel(8);
    let coordinator = bridge.replay_coordinator();
    let started = coordinator
        .start(key.clone(), secret_replay_plan(&label))
        .unwrap();
    install_secret(&coordinator, &started);
    let same_project_replay = coordinator
        .start(
            same_project.clone(),
            replay_plan(&format!("{label}-sibling")),
        )
        .unwrap();
    let isolated = coordinator
        .start(
            isolated_key.clone(),
            replay_plan(&format!("{label}-isolated")),
        )
        .unwrap();
    let controller = bridge.bind(key.clone(), Duration::from_secs(1));
    let pending = tokio::spawn(async move {
        controller
            .request(BrowserCommand::Reload {
                tab_id: "runtime-tab".to_string(),
            })
            .await
    });
    let request = inbox
        .recv()
        .await
        .expect("retained late controller request");

    match boundary {
        LifecycleBoundary::StopTab => {
            bridge
                .bind(key.clone(), Duration::from_secs(1))
                .notify(BrowserCommand::Stop {
                    tab_id: Some("runtime-tab".to_string()),
                })
                .await
                .unwrap();
        }
        LifecycleBoundary::StopWorkspace => {
            bridge
                .bind(key.clone(), Duration::from_secs(1))
                .notify(BrowserCommand::Stop { tab_id: None })
                .await
                .unwrap();
        }
        LifecycleBoundary::LogicalTabClose => {
            bridge
                .bind(key.clone(), Duration::from_secs(1))
                .notify(BrowserCommand::CloseTab {
                    tab_id: "runtime-tab".to_string(),
                })
                .await
                .unwrap();
        }
        LifecycleBoundary::ResetWorkspace => {
            bridge
                .bind(key.clone(), Duration::from_secs(1))
                .notify(BrowserCommand::ResetWorkspace)
                .await
                .unwrap();
        }
        LifecycleBoundary::ClearProject => {
            bridge
                .bind(key.clone(), Duration::from_secs(1))
                .notify(BrowserCommand::ClearProjectProfile)
                .await
                .unwrap();
        }
        LifecycleBoundary::DirectInput => bridge.observe_host_event(&BrowserHostEvent::user_input(
            key.clone(),
            "runtime-tab",
            BrowserUserInputKind::Keyboard,
        )),
        LifecycleBoundary::SelectConversation
        | LifecycleBoundary::RestartServer
        | LifecycleBoundary::KillPortRestart
        | LifecycleBoundary::RestartAiConversation
        | LifecycleBoundary::RestartSsh
        | LifecycleBoundary::CloseConversation => bridge.interrupt_workspace(&key),
        LifecycleBoundary::DeleteProject => bridge.interrupt_project(&key.project_id),
        LifecycleBoundary::QuitApplication => bridge.interrupt_all(),
    }

    if matches!(
        boundary,
        LifecycleBoundary::StopTab
            | LifecycleBoundary::StopWorkspace
            | LifecycleBoundary::LogicalTabClose
            | LifecycleBoundary::ResetWorkspace
            | LifecycleBoundary::ClearProject
    ) {
        let lifecycle_request = bridge.with_locked_host_work(|controls, mut lifecycle_requests| {
            assert!(controls.is_empty());
            assert_eq!(lifecycle_requests.len(), 1);
            lifecycle_requests.remove(0)
        });
        route_browser_request(true, lifecycle_request, |request| {
            request.respond(Ok(BrowserResponse::Acknowledged));
        })
        .unwrap();
    }

    assert_cancelled(&coordinator, started.instance.id(), &key);
    assert!(
        matches!(
            started.execution.secret_lease("password"),
            Err(BrowserReplaySecretError::ClosedStore)
        ),
        "{boundary:?} must close its secret store"
    );
    request.respond(Ok(BrowserResponse::Acknowledged));
    assert_eq!(
        pending.await.unwrap(),
        Err(devmanager::browser::BrowserError::Interrupted),
        "{boundary:?} must fence a late controller response"
    );
    assert_eq!(
        coordinator
            .status(&started.instance)
            .unwrap()
            .current_step_index,
        0,
        "{boundary:?} must not allow a late step advance or recipe write"
    );

    let same_project_status = coordinator
        .status(&same_project_replay.instance)
        .unwrap()
        .status;
    let isolated_status = coordinator.status(&isolated.instance).unwrap().status;
    match boundary {
        LifecycleBoundary::ClearProject | LifecycleBoundary::DeleteProject => {
            assert_eq!(same_project_status, BrowserReplayStatus::Cancelled);
            assert_eq!(isolated_status, BrowserReplayStatus::Pending);
        }
        LifecycleBoundary::QuitApplication => {
            assert_eq!(same_project_status, BrowserReplayStatus::Cancelled);
            assert_eq!(isolated_status, BrowserReplayStatus::Cancelled);
        }
        _ => {
            assert_eq!(same_project_status, BrowserReplayStatus::Pending);
            assert_eq!(isolated_status, BrowserReplayStatus::Pending);
        }
    }
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

#[tokio::test]
async fn browser_every_native_lifecycle_boundary_terminalizes_replay_and_fences_late_work() {
    for boundary in [
        LifecycleBoundary::StopTab,
        LifecycleBoundary::StopWorkspace,
        LifecycleBoundary::LogicalTabClose,
        LifecycleBoundary::ResetWorkspace,
        LifecycleBoundary::ClearProject,
        LifecycleBoundary::DirectInput,
        LifecycleBoundary::SelectConversation,
        LifecycleBoundary::RestartServer,
        LifecycleBoundary::KillPortRestart,
        LifecycleBoundary::RestartAiConversation,
        LifecycleBoundary::RestartSsh,
        LifecycleBoundary::CloseConversation,
        LifecycleBoundary::DeleteProject,
        LifecycleBoundary::QuitApplication,
    ] {
        assert_boundary_terminalizes_before_late_response(boundary).await;
    }
}

fn source_section<'a>(source: &'a str, start: &str, end: &str) -> &'a str {
    let start = source
        .find(start)
        .unwrap_or_else(|| panic!("missing source boundary: {start}"));
    let end = source[start..]
        .find(end)
        .map(|offset| start + offset)
        .unwrap_or_else(|| panic!("missing source boundary after {start}: {end}"));
    &source[start..end]
}

fn assert_before(section: &str, earlier: &str, later: &str) {
    let earlier_label = earlier;
    let later_label = later;
    let earlier = section
        .find(earlier_label)
        .unwrap_or_else(|| panic!("missing lifecycle call: {earlier_label}"));
    let later = section
        .find(later_label)
        .unwrap_or_else(|| panic!("missing protected mutation: {later_label}"));
    assert!(
        earlier < later,
        "{earlier_label} must run before {later_label}"
    );
}

#[test]
fn queued_user_input_enters_shared_cancellation_before_host_revision_mutation() {
    let app = include_str!("../src/app/mod.rs").replace("\r\n", "\n");
    let windows = include_str!("../src/browser/host/windows.rs").replace("\r\n", "\n");
    let unsupported = include_str!("../src/browser/host/unsupported.rs").replace("\r\n", "\n");

    let pump = source_section(
        &app,
        "fn pump_browser_events(",
        "fn with_browser_host_control_barrier",
    );
    assert!(pump.contains("drain_events_with_pre_apply_observer"));
    assert!(!pump.contains("let mut events = browser_host.drain_events();"));

    let drain = source_section(
        &windows,
        "pub fn drain_events_with_pre_apply_observer",
        "pub fn workspace_snapshot(",
    );
    assert_before(drain, "before_apply(&event", "apply_user_input");
    assert!(unsupported.contains("pub fn drain_events_with_pre_apply_observer"));
}

#[test]
fn blank_server_commands_are_preflighted_before_every_lifecycle_and_process_boundary() {
    let app = include_str!("../src/app/mod.rs").replace("\r\n", "\n");
    let process_manager = include_str!("../src/services/process_manager.rs").replace("\r\n", "\n");

    let start = source_section(&app, "fn start_server_action(", "fn stop_server_action(");
    assert!(
        start.matches("validate_server_launch").count() >= 2,
        "start must preflight before scheduling and again after the async port check"
    );
    assert_before(
        start,
        "validate_server_launch",
        "interrupt_active_browser_replay_before_route_change",
    );
    assert!(
        start.rfind("validate_server_launch").unwrap()
            < start
                .rfind("interrupt_active_browser_replay_before_route_change")
                .unwrap()
    );

    for (start_label, end_label, process_call) in [
        (
            "fn restart_server_action(",
            "fn clear_server_output_action(",
            ".restart_server(",
        ),
        (
            "fn kill_server_port_action(",
            "fn select_server_tab_action(",
            "schedule_kill_port_and_restart",
        ),
    ] {
        let section = source_section(&app, start_label, end_label);
        assert_before(
            section,
            "validate_server_launch",
            "interrupt_active_browser_replay_before_route_change",
        );
        assert_before(section, "validate_server_launch", process_call);
    }

    let remote = source_section(
        &app,
        "fn pump_remote_host_requests(",
        "fn handle_window_should_close(",
    );
    for (start_label, end_label, process_call) in [
        (
            "RemoteAction::StartServer {",
            "RemoteAction::StopServer {",
            "start_server_with_remote_response",
        ),
        (
            "RemoteAction::RestartServer {",
            "RemoteAction::LaunchAi {",
            "restart_server_with_remote_response",
        ),
    ] {
        let section = source_section(remote, start_label, end_label);
        assert_before(
            section,
            "validate_server_launch",
            "interrupt_active_browser_replay_before_route_change",
        );
        assert_before(section, "validate_server_launch", process_call);
    }

    for (start_label, end_label) in [
        ("fn schedule_start_server(", "fn schedule_restart_server("),
        (
            "fn schedule_restart_server(",
            "fn schedule_stop_server_and_wait(",
        ),
        (
            "pub fn schedule_kill_port_and_restart(",
            "fn prepare_start_server(",
        ),
    ] {
        assert!(
            source_section(&process_manager, start_label, end_label)
                .contains("validate_server_launch"),
            "{start_label} must reject blank commands before mutation or queue submission"
        );
    }
}

#[test]
fn invalid_server_preflight_precedes_control_ownership_mutation() {
    let app = include_str!("../src/app/mod.rs").replace("\r\n", "\n");
    for (start_label, end_label) in [
        ("fn start_server_action(", "fn stop_server_action("),
        (
            "fn restart_server_action(",
            "fn clear_server_output_action(",
        ),
    ] {
        let section = source_section(&app, start_label, end_label);
        assert_before(section, "validate_server_launch", "ensure_mutation_control");
        let validation = section.find("validate_server_launch").unwrap();
        let control = section.find("ensure_mutation_control").unwrap();
        let rejection = &section[validation..control];
        assert!(rejection.contains("return;"));
        assert!(!rejection.contains("interrupt_active_browser_replay_before_route_change"));
        assert!(!rejection.contains("remote_send_action"));
    }
}

#[test]
fn server_selection_rejects_an_active_ai_tab_before_replay_cancellation() {
    let app = include_str!("../src/app/mod.rs").replace("\r\n", "\n");
    let select = source_section(&app, "fn select_server_tab_action(", "fn launch_ai_action(");
    assert!(select.contains("existing_server_tab(&self.state, command_id)"));
    assert_before(
        select,
        "existing_server_tab(&self.state, command_id)",
        "interrupt_active_browser_replay_before_route_change",
    );
    let helper = source_section(&app, "fn existing_server_tab", "#[cfg(test)]");
    assert!(helper.contains(".filter(|tab| tab.tab_type == TabType::Server)"));
}

#[test]
fn project_deletion_validates_exact_ownership_before_any_replay_cancellation() {
    let app = include_str!("../src/app/mod.rs").replace("\r\n", "\n");
    let editor = source_section(
        source_section(
            &app,
            "fn delete_editor_action(",
            "fn delete_project_action(",
        ),
        "EditorPanel::Project(draft) => {",
        "EditorPanel::Folder(draft) => {",
    );
    assert_before(
        editor,
        "validate_project_deletion(&self.state, &project_id)",
        "interrupt_browser_project_before_mutation",
    );

    let local = source_section(
        &app,
        "fn delete_project_action(",
        "fn delete_folder_action(",
    );
    assert_before(
        local,
        "validate_project_deletion(&self.state, project_id)",
        "interrupt_browser_project_before_mutation",
    );

    let remote = source_section(
        source_section(
            &app,
            "fn pump_remote_host_requests(",
            "fn handle_window_should_close(",
        ),
        "RemoteAction::DeleteProject { project_id } => {",
        "RemoteAction::SaveFolder {",
    );
    assert_before(
        remote,
        "validate_project_deletion(&self.state, &project_id)",
        "delete_project_action",
    );
    assert!(remote.contains("RemoteActionResult::error"));
    assert_before(remote, "delete_project_action", "did_change = true");
    let helper = source_section(&app, "fn validate_project_deletion", "#[cfg(test)]");
    assert!(helper.contains("find_project(project_id)"));
}

#[test]
fn unknown_project_preflight_precedes_control_ownership_mutation() {
    let app = include_str!("../src/app/mod.rs").replace("\r\n", "\n");
    let section = source_section(
        &app,
        "fn delete_project_action(",
        "fn delete_folder_action(",
    );
    assert_before(
        section,
        "validate_project_deletion(&self.state, project_id)",
        "ensure_mutation_control",
    );
    let validation = section
        .find("validate_project_deletion(&self.state, project_id)")
        .unwrap();
    let control = section.find("ensure_mutation_control").unwrap();
    let rejection = &section[validation..control];
    assert!(rejection.contains("return;"));
    assert!(!rejection.contains("interrupt_browser_project_before_mutation"));
    assert!(!rejection.contains("spawn_remote_request"));
}

#[test]
fn remote_host_state_transitions_interrupt_all_local_browser_work_before_replacement_or_restore() {
    let app = include_str!("../src/app/mod.rs").replace("\r\n", "\n");
    let interruption = "self.interrupt_all_browser_replays_before_shutdown();";

    let connect = source_section(
        &app,
        "fn apply_connected_remote_host(",
        "fn begin_remote_reconnect(",
    );
    let body_start = connect
        .find(") {")
        .map(|offset| offset + ") {".len())
        .expect("remote-connect function body");
    assert!(
        connect[body_start..].trim_start().starts_with(interruption),
        "remote connect must interrupt every local browser replay as its first executable statement"
    );
    for mutation in [
        "client.take_control()",
        "self.local_state_backup",
        "self.remote_mode",
        "self.state = self.merge_remote_snapshot_into_state",
    ] {
        assert_before(connect, interruption, mutation);
    }

    let disconnect = source_section(
        &app,
        "fn disconnect_remote_host(",
        "fn current_runtime_snapshot(",
    );
    assert_before(disconnect, interruption, "self.local_state_backup.take()");
    assert_before(disconnect, interruption, "self.state = local_state");
}

#[test]
fn browser_route_admission_requires_the_exact_active_visible_local_workspace() {
    let app = include_str!("../src/app/mod.rs").replace("\r\n", "\n");
    let route = source_section(
        &app,
        "fn active_open_browser_route(",
        "fn dispatch_browser_command(",
    );

    assert!(
        route.contains("self.remote_mode.is_none()"),
        "a matching remote snapshot must never admit the local browser route"
    );
    assert!(
        route.contains("self.state.settings().browser_enabled"),
        "disabled Browser settings must close every local route"
    );
    assert!(
        route.contains("self.browser_host.status().available"),
        "an unavailable native host must close every local route"
    );
    assert!(
        route.contains("active_browser_workspace()") || route.contains("active_tab()"),
        "route admission must use the selected conversation, not any open AI tab"
    );
    assert!(
        route.contains("pane_open"),
        "a collapsed pane must not admit hidden provider automation"
    );

    let dispatch = source_section(
        &app,
        "fn dispatch_browser_command(",
        "fn synchronize_browser_response(",
    );
    assert_before(
        dispatch,
        "browser_route_is_open(workspace_key)",
        "with_locked_host_work_for_command",
    );
    let async_route = source_section(
        &app,
        "fn handle_browser_request(",
        "fn pump_browser_events(",
    );
    assert_before(
        async_route,
        "active_open_browser_route()",
        "route_browser_request_for_active_workspace(",
    );

    assert_before(
        dispatch,
        "active_open_browser_route()",
        "route_browser_request_for_active_workspace(",
    );
    let host_barrier = source_section(
        &app,
        "fn with_browser_host_control_barrier",
        "fn browser_pane_context(",
    );
    assert_before(
        host_barrier,
        "active_open_browser_route()",
        "route_browser_request_for_active_workspace(",
    );

    let mcp = include_str!("../src/browser/mcp.rs").replace("\r\n", "\n");
    let workflow = source_section(
        &mcp,
        "async fn browser_workflow(",
        "fn browser_workflow_status_payload(",
    );
    assert_before(
        workflow,
        "capture_replay_admission()",
        "validate_and_ensure(&context).await",
    );
    assert_before(workflow, "validate_and_ensure(&context).await", ".replay(");
}

#[test]
fn priority_lifecycle_enqueue_is_side_effect_free_until_strict_route_admission() {
    let commands = include_str!("../src/browser/commands.rs").replace("\r\n", "\n");
    let enqueue = source_section(
        &commands,
        "    fn enqueue_lifecycle_command(",
        "#[allow(dead_code)]",
    );
    assert!(enqueue.contains("lifecycle_requests.push_back"));
    assert!(
        !enqueue.contains("apply_lifecycle_control("),
        "enqueue must not cancel replays or advance epochs before route admission"
    );

    let route = source_section(
        &commands,
        "pub fn route_browser_request(",
        "impl BrowserCommandRequest",
    );
    assert_before(
        route,
        "if !route_is_open",
        "request.admit_lifecycle_control()",
    );
    assert_before(
        route,
        "request.admit_lifecycle_control()",
        "dispatch_open(request)",
    );
}

#[test]
fn replace_import_disabling_browser_interrupts_before_assignment_then_reconciles_authority() {
    let app = include_str!("../src/app/mod.rs").replace("\r\n", "\n");
    let imported = source_section(
        &app,
        "fn apply_imported_config(",
        "fn check_for_updates_action(",
    );
    let interruption = "interrupt_all_browser_replays_before_shutdown";
    let assignment = "self.state.config = config";

    assert!(
        imported.contains("ConfigImportMode::Replace") && imported.contains("browser_enabled"),
        "replace import must classify the enabled-to-disabled Browser transition"
    );
    assert_before(imported, interruption, assignment);
    assert_before(imported, assignment, "reconcile_browser_gateway()");
    assert_before(
        imported,
        "reconcile_browser_gateway()",
        "sync_browser_host_visibility(None)",
    );
}

#[test]
fn browser_provider_and_native_shell_lifecycle_boundaries_reach_the_shared_bridge_first() {
    let app = include_str!("../src/app/mod.rs").replace("\r\n", "\n");
    let process_manager = include_str!("../src/services/process_manager.rs").replace("\r\n", "\n");

    for helper in [
        "fn interrupt_active_browser_replay_before_route_change(",
        "fn interrupt_browser_workspace_before_teardown(",
        "fn interrupt_browser_project_before_mutation(",
        "fn interrupt_all_browser_replays_before_shutdown(",
    ] {
        assert!(app.contains(helper), "missing NativeShell helper: {helper}");
    }

    let discard_helper = source_section(
        &app,
        "fn discard_browser_workflow_state_after_replay_interrupt(",
        "fn interrupt_active_browser_replay_before_route_change(",
    );
    assert!(discard_helper.contains("self.browser_host.discard_workflow_state"));

    let route_helper = source_section(
        &app,
        "fn interrupt_active_browser_replay_before_route_change(",
        "fn interrupt_browser_workspace_before_teardown(",
    );
    assert_before(
        route_helper,
        "next_workspace == Some(&previous)",
        "self.browser_bridge.interrupt_workspace",
    );
    assert_before(
        route_helper,
        "self.browser_bridge.interrupt_workspace",
        "discard_browser_workflow_state_after_replay_interrupt",
    );
    let workspace_helper = source_section(
        &app,
        "fn interrupt_browser_workspace_before_teardown(",
        "fn interrupt_browser_project_before_mutation(",
    );
    assert_before(
        workspace_helper,
        "self.browser_bridge.interrupt_workspace",
        "discard_browser_workflow_state_after_replay_interrupt",
    );
    let project_helper = source_section(
        &app,
        "fn interrupt_browser_project_before_mutation(",
        "fn interrupt_all_browser_replays_before_shutdown(",
    );
    assert_before(
        project_helper,
        "self.browser_bridge.interrupt_project",
        "discard_browser_workflow_state_after_replay_interrupt",
    );
    let shutdown_helper = source_section(
        &app,
        "fn interrupt_all_browser_replays_before_shutdown(",
        "fn sync_browser_host_visibility(",
    );
    assert!(shutdown_helper.contains("interrupt_all_with_host_cleanup"));
    assert!(shutdown_helper.contains("self.browser_host.interrupt_all_local_work()"));
    assert_before(
        shutdown_helper,
        "interrupt_all_with_host_cleanup",
        "open_browser_workspace_keys()",
    );

    let command_dispatch = source_section(
        &app,
        "fn dispatch_browser_command(",
        "fn synchronize_browser_response(",
    );
    assert_before(
        command_dispatch,
        "with_locked_host_work_for_command",
        "retire_browser_replay_ui_after_interrupt",
    );
    assert_before(
        command_dispatch,
        "retire_browser_replay_ui_after_interrupt",
        "browser_host.handle_command",
    );

    for (start, end, mutation) in [
        (
            "fn select_server_tab_action(",
            "fn launch_ai_action(",
            "self.state.select_tab",
        ),
        (
            "fn launch_ai_action(",
            "fn select_ai_tab_action(",
            "process_manager.start_ai_session",
        ),
        (
            "fn select_ai_tab_action(",
            "fn close_ai_tab_action(",
            "self.state.select_tab",
        ),
        (
            "fn open_ssh_tab_action(",
            "fn connect_ssh_action(",
            ".open_ssh_tab(",
        ),
        (
            "fn connect_ssh_action(",
            "fn restart_ssh_action(",
            "start_ssh_session",
        ),
    ] {
        assert_before(
            source_section(&app, start, end),
            "interrupt_active_browser_replay_before_route_change",
            mutation,
        );
    }

    let open_ssh = source_section(&app, "fn open_ssh_tab_action(", "fn connect_ssh_action(");
    assert_before(
        open_ssh,
        "find_ssh_connection(connection_id)",
        "interrupt_active_browser_replay_before_route_change",
    );

    for (start, end, validation, mutation) in [
        (
            "fn restart_server_action(",
            "fn clear_server_output_action(",
            "validate_server_launch",
            ".restart_server(",
        ),
        (
            "fn kill_server_port_action(",
            "fn select_server_tab_action(",
            "validate_server_launch",
            "schedule_kill_port_and_restart",
        ),
        (
            "fn restart_ai_tab_action(",
            "fn close_ai_tab_action(",
            "find_ai_tab(tab_id)",
            "restart_ai_session",
        ),
        (
            "fn restart_ssh_action(",
            "fn disconnect_ssh_action(",
            "find_ssh_connection(connection_id)",
            "restart_ssh_session",
        ),
    ] {
        let section = source_section(&app, start, end);
        assert_before(
            section,
            validation,
            "interrupt_active_browser_replay_before_route_change",
        );
        assert_before(
            section,
            "interrupt_active_browser_replay_before_route_change",
            mutation,
        );
    }

    let restart_ai = source_section(&app, "fn restart_ai_tab_action(", "fn close_ai_tab_action(");
    assert_before(
        restart_ai,
        "ensure_mutation_control",
        "interrupt_active_browser_replay_before_route_change",
    );
    assert_before(
        restart_ai,
        ".validate_ai_restart(",
        "interrupt_active_browser_replay_before_route_change",
    );
    assert_before(
        restart_ai,
        "interrupt_browser_workspace_before_teardown",
        ".restart_ai_session(",
    );
    assert!(
        !restart_ai.contains("close_browser_replay_secret_prompt_for_route"),
        "AI restart must retire secret UI only after bridge cancellation"
    );

    let remote_requests = source_section(
        &app,
        "fn pump_remote_host_requests(",
        "fn handle_window_should_close(",
    );
    let remote_restart_server = source_section(
        remote_requests,
        "RemoteAction::RestartServer {",
        "RemoteAction::LaunchAi {",
    );
    assert_before(
        remote_restart_server,
        "validate_server_launch",
        "interrupt_active_browser_replay_before_route_change",
    );
    assert_before(
        remote_restart_server,
        "interrupt_active_browser_replay_before_route_change",
        "restart_server_with_remote_response",
    );
    let remote_launch_ai = source_section(
        remote_requests,
        "RemoteAction::LaunchAi {",
        "RemoteAction::OpenAiTab {",
    );
    assert!(
        !remote_launch_ai.contains("interrupt_active_browser_replay_before_route_change"),
        "background remote AI launch must not cancel the selected conversation"
    );
    let remote_open_ai = source_section(
        remote_requests,
        "RemoteAction::OpenAiTab {",
        "RemoteAction::RestartAiTab {",
    );
    assert!(
        !remote_open_ai.contains("interrupt_active_browser_replay_before_route_change"),
        "background remote AI restore must not cancel the selected conversation"
    );
    let remote_restart_ai = source_section(
        remote_requests,
        "RemoteAction::RestartAiTab {",
        "RemoteAction::CloseAiTab {",
    );
    assert_before(
        remote_restart_ai,
        "find_ai_tab(&tab_id)",
        "interrupt_browser_workspace_before_teardown",
    );
    assert_before(
        remote_restart_ai,
        ".validate_ai_restart(",
        "interrupt_browser_workspace_before_teardown",
    );
    assert_before(
        remote_restart_ai,
        "interrupt_browser_workspace_before_teardown",
        "restart_ai_session_activate_with_response",
    );
    assert!(
        !remote_restart_ai.contains("interrupt_active_browser_replay_before_route_change"),
        "background remote AI restart must cancel only its provider workspace"
    );

    for (start, end, validation, mutation) in [
        (
            "RemoteAction::StartServer {",
            "RemoteAction::StopServer {",
            "validate_server_launch",
            "start_server_with_remote_response",
        ),
        (
            "RemoteAction::ConnectSsh {",
            "RemoteAction::RestartSsh {",
            "find_ssh_connection(&connection_id)",
            "start_ssh_session_with_response",
        ),
        (
            "RemoteAction::RestartSsh {",
            "RemoteAction::DisconnectSsh {",
            "find_ssh_connection(&connection_id)",
            "restart_ssh_session_with_response",
        ),
    ] {
        let section = source_section(remote_requests, start, end);
        assert_before(
            section,
            validation,
            "interrupt_active_browser_replay_before_route_change",
        );
        assert_before(
            section,
            "interrupt_active_browser_replay_before_route_change",
            mutation,
        );
    }

    let close_tab = source_section(&app, "fn close_tab_action(", "fn confirm_live_tab_close(");
    assert_before(
        close_tab,
        "interrupt_browser_workspace_before_teardown",
        "process_manager.close_tab",
    );
    let close_ai = source_section(&app, "fn close_ai_tab_action(", "fn open_ssh_tab_action(");
    assert_before(
        close_ai,
        "interrupt_browser_workspace_before_teardown",
        "process_manager.close_tab",
    );
    let imported = source_section(
        &app,
        "fn apply_imported_config(",
        "fn check_for_updates_action(",
    );
    assert_before(
        imported,
        "interrupt_browser_workspace_before_teardown",
        "self.state.config = config",
    );
    let delete_project = source_section(
        &app,
        "fn delete_project_action(",
        "fn delete_folder_action(",
    );
    assert_before(
        delete_project,
        "interrupt_browser_project_before_mutation",
        "close_ai_session",
    );

    for (start, end, mutation) in [
        (
            "fn handle_window_should_close(",
            "fn preview_notification_sound_action(",
            "schedule_shutdown",
        ),
        (
            "fn install_update_action(",
            "fn force_quit_action(",
            "schedule_shutdown",
        ),
        (
            "fn force_quit_action(",
            "fn terminal_font_size(",
            "std::process::exit",
        ),
    ] {
        assert_before(
            source_section(&app, start, end),
            "interrupt_all_browser_replays_before_shutdown",
            mutation,
        );
    }

    let terminal_exit = source_section(
        &process_manager,
        "fn cleanup_browser_provider_session_if_matches(",
        "fn session_change_notifier(",
    );
    assert_before(
        terminal_exit,
        "expected.access().bearer_token()",
        "sessions.remove(session_id)",
    );
    assert_before(
        terminal_exit,
        "sessions.remove(session_id)",
        "removed.registrar.revoke",
    );
    let generic_spawn = source_section(
        &process_manager,
        "fn spawn_ai_session_with_writer_and_attachment_binding",
        "fn shutdown_managed_processes_inner(",
    );
    assert!(
        !generic_spawn.contains("BrowserGatewayRegistrar")
            && !generic_spawn.contains("register_with_project_root"),
        "registration ownership must remain outside generic process spawning"
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
        let lifecycle_request = bridge.with_locked_host_work(|controls, mut lifecycle_requests| {
            assert!(controls.is_empty());
            assert_eq!(lifecycle_requests.len(), 1);
            lifecycle_requests.remove(0)
        });
        route_browser_request(true, lifecycle_request, |request| {
            request.respond(Ok(BrowserResponse::Acknowledged));
        })
        .unwrap();
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
    bridge.observe_host_event(&BrowserHostEvent::user_input(
        key.clone(),
        "runtime-tab",
        BrowserUserInputKind::Pointer,
    ));
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

#[tokio::test]
async fn remote_mode_boundaries_cancel_local_secrets_late_work_and_hidden_replay_without_touching_remote_state(
) {
    let first = workspace("local-project-a", "first");
    let second = workspace("local-project-b", "second");
    let (bridge, mut inbox) = browser_command_channel(4);
    let coordinator = bridge.replay_coordinator();

    let first_replay = coordinator
        .start(first.clone(), secret_replay_plan("remote-connect-first"))
        .unwrap();
    install_secret(&coordinator, &first_replay);
    let second_replay = coordinator
        .start(second.clone(), secret_replay_plan("remote-connect-second"))
        .unwrap();
    install_secret(&coordinator, &second_replay);

    let controller = bridge.bind(first.clone(), Duration::from_secs(1));
    let pending = tokio::spawn(async move {
        controller
            .request(BrowserCommand::Reload {
                tab_id: "runtime-tab".to_string(),
            })
            .await
    });
    let retained = inbox.recv().await.expect("retained local request");

    let (remote_bridge, _remote_inbox) = browser_command_channel(1);
    let remote_coordinator = remote_bridge.replay_coordinator();
    let remote_key = workspace("remote-project", "remote-conversation");
    let remote_replay = remote_coordinator
        .start(remote_key, replay_plan("remote-isolated"))
        .unwrap();
    remote_coordinator.begin(&remote_replay.instance).unwrap();

    // This is the shared-bridge operation the native connect boundary must
    // execute before replacing local application state.
    bridge.interrupt_all();
    assert_cancelled(&coordinator, first_replay.instance.id(), &first);
    assert_cancelled(&coordinator, second_replay.instance.id(), &second);
    for replay in [&first_replay, &second_replay] {
        assert!(matches!(
            replay.execution.secret_lease("password"),
            Err(BrowserReplaySecretError::ClosedStore)
        ));
    }
    retained.respond(Ok(BrowserResponse::Acknowledged));
    assert_eq!(
        pending.await.unwrap(),
        Err(devmanager::browser::BrowserError::Interrupted)
    );
    assert_eq!(
        remote_coordinator
            .status(&remote_replay.instance)
            .unwrap()
            .status,
        BrowserReplayStatus::Running
    );

    // A stale local MCP registration could otherwise create this invisible
    // prompt while the remote snapshot owns the UI.
    let hidden = coordinator
        .replace(
            first.clone(),
            secret_replay_plan("hidden-during-remote-mode"),
        )
        .unwrap();
    assert_eq!(
        hidden.projection.status,
        BrowserReplayStatus::NeedsUserSecret
    );

    // Disconnect must cancel any such hidden work before restoring the saved
    // local snapshot, so reconciliation cannot resurrect its secret prompt.
    bridge.interrupt_all();
    assert_cancelled(&coordinator, hidden.instance.id(), &first);
    assert!(matches!(
        hidden.execution.secret_lease("password"),
        Err(BrowserReplaySecretError::ClosedStore)
    ));
    assert!(coordinator.active_state(&first).is_none());
    assert_eq!(
        remote_coordinator
            .status(&remote_replay.instance)
            .unwrap()
            .status,
        BrowserReplayStatus::Running
    );
}
