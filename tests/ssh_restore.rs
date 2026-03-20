use devmanager::models::TabType;
use devmanager::persistence::{load_config_from_str, load_session_from_str, WorkspaceSnapshot};
use devmanager::services::ProcessManager;
use devmanager::state::{AppState, SessionDimensions, SessionRuntimeState, SshLaunchSpec};
use devmanager::terminal::session::TerminalBackend;
use std::path::PathBuf;

fn load_fixture_state() -> AppState {
    let config_text =
        std::fs::read_to_string("tests/fixtures/legacy-config.json").expect("config fixture");
    let session_text =
        std::fs::read_to_string("tests/fixtures/legacy-session.json").expect("session fixture");
    let config = load_config_from_str(&config_text).expect("parse config");
    let session = load_session_from_str(&session_text).expect("parse session");
    AppState::from_workspace(WorkspaceSnapshot { config, session })
}

fn live_ssh_runtime(session_id: &str, tab_id: &str, connection_id: &str) -> SessionRuntimeState {
    let cwd = PathBuf::from("C:/Code/userfirst");
    let mut session = SessionRuntimeState::new(
        session_id.to_string(),
        cwd.clone(),
        SessionDimensions::default(),
        TerminalBackend::PortablePtyFeedingAlacritty,
    );
    session.configure_ssh(SshLaunchSpec {
        tab_id: tab_id.to_string(),
        ssh_connection_id: connection_id.to_string(),
        project_id: "project-userfirst".to_string(),
        cwd,
        program: "ssh".to_string(),
        args: vec![
            "deploy@prod.example.com".to_string(),
            "-p".to_string(),
            "22".to_string(),
        ],
    });
    session.note_start(Some(42));
    session
}

#[test]
fn restore_ssh_tabs_reattaches_live_runtime_by_tab_id() {
    let mut state = load_fixture_state();
    let manager = ProcessManager::new();
    manager.register_runtime_session(live_ssh_runtime("ssh-live", "ssh-prod-tab", "ssh-prod"));

    let report = manager.restore_ssh_tabs(&mut state);
    let tab = state.find_ssh_tab("ssh-prod-tab").expect("ssh tab");

    assert_eq!(report.reattached, 1);
    assert_eq!(report.recovered, 0);
    assert_eq!(report.disconnected, 0);
    assert_eq!(tab.pty_session_id.as_deref(), Some("ssh-live"));
    assert_eq!(
        state
            .open_tabs
            .iter()
            .filter(|tab| tab.id == "ssh-prod-tab")
            .count(),
        1
    );
}

#[test]
fn reconcile_saved_ssh_tabs_recovers_missing_live_tab() {
    let mut state = load_fixture_state();
    state
        .open_tabs
        .retain(|tab| !matches!(tab.tab_type, TabType::Ssh));
    let manager = ProcessManager::new();
    manager.register_runtime_session(live_ssh_runtime("ssh-live", "ssh-prod-tab", "ssh-prod"));

    let recovered = manager.reconcile_saved_ssh_tabs(&mut state);

    assert_eq!(recovered, 1);
    let tab = state
        .find_ssh_tab("ssh-prod-tab")
        .expect("recovered ssh tab");
    assert_eq!(tab.pty_session_id.as_deref(), Some("ssh-live"));
    assert_eq!(tab.ssh_connection_id.as_deref(), Some("ssh-prod"));
}

#[test]
fn restore_ssh_tabs_leaves_missing_runtime_disconnected() {
    let mut state = load_fixture_state();
    let manager = ProcessManager::new();

    let report = manager.restore_ssh_tabs(&mut state);
    let tab = state.find_ssh_tab("ssh-prod-tab").expect("ssh tab");

    assert_eq!(report.reattached, 0);
    assert_eq!(report.recovered, 0);
    assert_eq!(report.disconnected, 1);
    assert_eq!(tab.pty_session_id, None);
    assert_eq!(
        state
            .open_tabs
            .iter()
            .filter(|tab| tab.id == "ssh-prod-tab")
            .count(),
        1
    );
}
