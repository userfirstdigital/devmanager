use devmanager::models::{DefaultTerminal, TabType, CURRENT_CONFIG_VERSION};
use devmanager::persistence::{load_config_from_str, load_session_from_str, WorkspaceSnapshot};
use devmanager::state::AppState;
use std::fs;
use std::path::PathBuf;

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

#[test]
fn loads_legacy_config_fixture_with_migration_defaults() {
    let fixture = fs::read_to_string(fixture_path("legacy-config.json")).unwrap();
    let config = load_config_from_str(&fixture).unwrap();

    assert_eq!(config.version, CURRENT_CONFIG_VERSION);
    assert_eq!(config.projects.len(), 1);
    assert_eq!(config.projects[0].folders.len(), 2);
    assert_eq!(config.projects[0].folders[0].commands[0].label, "dev");
    assert_eq!(
        config.projects[0].notes.as_deref(),
        Some("Legacy notes fixture")
    );
    assert_eq!(config.settings.default_terminal, DefaultTerminal::Bash);
    assert_eq!(config.settings.restore_session_on_start, Some(true));
    assert!(!config.settings.option_as_meta);
    assert!(!config.settings.copy_on_select);
    assert!(config.settings.keep_selection_on_copy);
    assert_eq!(config.ssh_connections.len(), 1);
}

#[test]
fn loads_legacy_session_fixture_and_backfills_server_pty_id() {
    let fixture = fs::read_to_string(fixture_path("legacy-session.json")).unwrap();
    let session = load_session_from_str(&fixture).unwrap();

    assert_eq!(session.open_tabs.len(), 3);
    assert_eq!(session.active_tab_id.as_deref(), Some("claude-1"));
    assert!(!session.sidebar_collapsed);
    assert_eq!(session.open_tabs[0].tab_type, TabType::Server);
    assert_eq!(
        session.open_tabs[0].pty_session_id.as_deref(),
        Some("cmd-dev")
    );
    assert_eq!(session.open_tabs[2].tab_type, TabType::Ssh);
    assert_eq!(
        session.open_tabs[2].ssh_connection_id.as_deref(),
        Some("ssh-prod")
    );
}

#[test]
fn app_state_uses_loaded_legacy_snapshot() {
    let config_fixture = fs::read_to_string(fixture_path("legacy-config.json")).unwrap();
    let session_fixture = fs::read_to_string(fixture_path("legacy-session.json")).unwrap();
    let state = AppState::from_workspace(WorkspaceSnapshot {
        config: load_config_from_str(&config_fixture).unwrap(),
        session: load_session_from_str(&session_fixture).unwrap(),
    });

    assert_eq!(state.projects().len(), 1);
    assert_eq!(state.ai_tabs().count(), 1);
    assert_eq!(
        state.active_tab().map(|tab| tab.id.as_str()),
        Some("claude-1")
    );
    assert_eq!(
        state.active_project().map(|project| project.name.as_str()),
        Some("UserFirst")
    );
    assert_eq!(
        state
            .find_ssh_tab("ssh-prod-tab")
            .and_then(|tab| tab.ssh_connection_id.as_deref()),
        Some("ssh-prod")
    );
}
