use devmanager::persistence::{load_config_from_str, load_session_from_str, WorkspaceSnapshot};
use devmanager::services::ProcessManager;
use devmanager::state::{AppState, SessionDimensions, SessionRuntimeState, SessionStatus};
use devmanager::terminal::session::TerminalBackend;
use std::path::PathBuf;
use std::time::Duration;

fn load_fixture_state() -> AppState {
    let config_text =
        std::fs::read_to_string("tests/fixtures/legacy-config.json").expect("config fixture");
    let session_text =
        std::fs::read_to_string("tests/fixtures/legacy-session.json").expect("session fixture");
    let config = load_config_from_str(&config_text).expect("parse config");
    let session = load_session_from_str(&session_text).expect("parse session");
    AppState::from_workspace(WorkspaceSnapshot { config, session })
}

#[test]
fn command_lookup_finds_command_and_folder() {
    let state = load_fixture_state();
    let lookup = state.find_command("cmd-dev").expect("command lookup");
    assert_eq!(lookup.project.id, "project-userfirst");
    assert_eq!(lookup.folder.id, "folder-web");
    assert_eq!(lookup.command.label, "dev");
}

#[test]
fn merge_recovered_server_tabs_adds_missing() {
    let mut state = load_fixture_state();
    state.open_tabs.retain(|tab| tab.id != "cmd-dev");
    state.active_tab_id = None;

    let recovered = devmanager::models::SessionTab {
        id: "cmd-dev".to_string(),
        tab_type: devmanager::models::TabType::Server,
        project_id: "project-userfirst".to_string(),
        command_id: Some("cmd-dev".to_string()),
        pty_session_id: Some("cmd-dev".to_string()),
        label: Some("dev".to_string()),
        ssh_connection_id: None,
    };

    let added = state.merge_recovered_server_tabs(vec![recovered]);
    assert_eq!(added, 1);
    assert!(state.open_tabs.iter().any(|tab| tab.id == "cmd-dev"));
    assert!(state.active_tab_id.is_some());
}

#[test]
fn reconcile_saved_server_tabs_recovers_live_sessions() {
    let mut state = load_fixture_state();
    state
        .open_tabs
        .retain(|tab| tab.tab_type != devmanager::models::TabType::Server);
    state.active_tab_id = None;

    let manager = ProcessManager::new();
    let mut session = SessionRuntimeState::new(
        "cmd-dev".to_string(),
        PathBuf::from("C:/Code/userfirst/web"),
        SessionDimensions::default(),
        TerminalBackend::PortablePtyFeedingAlacritty,
    );
    session.status = SessionStatus::Running;
    session.command_id = Some("cmd-dev".to_string());
    session.project_id = Some("project-userfirst".to_string());
    manager.register_runtime_session(session);

    let recovered = manager.reconcile_saved_server_tabs(&mut state);
    assert_eq!(recovered, 1);
    assert!(state.open_tabs.iter().any(|tab| tab.id == "cmd-dev"));
}

#[test]
fn stop_server_and_wait_transitions_to_stopped() {
    let manager = ProcessManager::new();
    let mut session = SessionRuntimeState::new(
        "cmd-dev".to_string(),
        PathBuf::from("C:/Code/userfirst/web"),
        SessionDimensions::default(),
        TerminalBackend::PortablePtyFeedingAlacritty,
    );
    session.status = SessionStatus::Running;
    session.command_id = Some("cmd-dev".to_string());
    manager.register_runtime_session(session);

    let stopped = manager.stop_server_and_wait("cmd-dev", Duration::from_millis(0));
    assert!(!stopped);
    let runtime = manager.runtime_state();
    let session = runtime.sessions.get("cmd-dev").expect("session present");
    assert_eq!(session.status, SessionStatus::Stopped);
}
