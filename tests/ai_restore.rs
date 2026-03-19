use devmanager::persistence::{load_config_from_str, load_session_from_str, WorkspaceSnapshot};
use devmanager::services::ProcessManager;
use devmanager::state::{
    AiActivity, AiLaunchSpec, AppState, SessionDimensions, SessionKind, SessionRuntimeState,
    SessionStatus,
};
use devmanager::terminal::session::TerminalBackend;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn load_fixture_state() -> AppState {
    let config_text =
        std::fs::read_to_string("tests/fixtures/legacy-config.json").expect("config fixture");
    let session_text =
        std::fs::read_to_string("tests/fixtures/legacy-session.json").expect("session fixture");
    let config = load_config_from_str(&config_text).expect("parse config");
    let session = load_session_from_str(&session_text).expect("parse session");
    AppState::from_workspace(WorkspaceSnapshot { config, session })
}

fn state_with_temp_paths() -> AppState {
    let mut state = load_fixture_state();
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let root = std::env::temp_dir().join(format!("devmanager-ai-test-{unique:x}"));
    let web = root.join("web");
    let api = root.join("api");
    std::fs::create_dir_all(&web).expect("create web temp dir");
    std::fs::create_dir_all(&api).expect("create api temp dir");

    if let Some(project) = state.config.projects.first_mut() {
        project.root_path = root.to_string_lossy().to_string();
        if let Some(folder) = project.folders.first_mut() {
            folder.folder_path = web.to_string_lossy().to_string();
        }
        if let Some(folder) = project.folders.get_mut(1) {
            folder.folder_path = api.to_string_lossy().to_string();
        }
    }
    state.config.settings.claude_command = Some("echo claude-ready".to_string());
    state.config.settings.codex_command = Some("echo codex-ready".to_string());
    state
}

fn live_ai_runtime(session_id: &str, tab_id: &str) -> SessionRuntimeState {
    let cwd = PathBuf::from("C:/Code/userfirst");
    let mut session = SessionRuntimeState::new(
        session_id.to_string(),
        cwd.clone(),
        SessionDimensions::default(),
        TerminalBackend::PortablePtyFeedingAlacritty,
    );
    session.configure_ai(AiLaunchSpec {
        tab_id: tab_id.to_string(),
        project_id: "project-userfirst".to_string(),
        tool: SessionKind::Claude,
        cwd,
        shell_program: "cmd".to_string(),
        shell_args: Vec::new(),
        startup_command: "echo claude-ready".to_string(),
    });
    session.status = SessionStatus::Running;
    session.ai_activity = Some(AiActivity::Idle);
    session.note_start(Some(42));
    session
}

#[test]
fn restore_ai_tabs_reattaches_live_runtime_by_tab_id() {
    let mut state = load_fixture_state();
    let manager = ProcessManager::new();
    manager.register_runtime_session(live_ai_runtime("claude-live", "claude-1"));

    let report = manager.restore_ai_tabs(&mut state, SessionDimensions::default());
    let tab = state.find_ai_tab("claude-1").expect("claude tab");

    assert_eq!(report.reattached, 1);
    assert_eq!(tab.pty_session_id.as_deref(), Some("claude-live"));
    assert_eq!(
        state
            .open_tabs
            .iter()
            .filter(|tab| tab.id == "claude-1")
            .count(),
        1
    );
}

#[test]
fn reconcile_saved_ai_tabs_recovers_missing_live_tab() {
    let mut state = load_fixture_state();
    state
        .open_tabs
        .retain(|tab| !matches!(tab.tab_type, devmanager::models::TabType::Claude));
    let manager = ProcessManager::new();
    manager.register_runtime_session(live_ai_runtime("claude-live", "claude-1"));

    let recovered = manager.reconcile_saved_ai_tabs(&mut state);

    assert_eq!(recovered, 1);
    let tab = state.find_ai_tab("claude-1").expect("recovered claude tab");
    assert_eq!(tab.pty_session_id.as_deref(), Some("claude-live"));
}

#[test]
fn restart_ai_session_updates_pty_session_id_without_duplicate_tabs() {
    let mut state = state_with_temp_paths();
    let manager = ProcessManager::new();

    let first_session_id = manager
        .ensure_ai_session_for_tab(
            &mut state,
            "claude-1",
            SessionDimensions::default(),
            true,
            true,
        )
        .expect("launch claude tab");
    thread::sleep(Duration::from_millis(100));

    let second_session_id = manager
        .restart_ai_session(&mut state, "claude-1", SessionDimensions::default())
        .expect("restart claude tab");

    let tab = state
        .find_ai_tab("claude-1")
        .expect("claude tab after restart");
    assert_ne!(first_session_id, second_session_id);
    assert_eq!(
        tab.pty_session_id.as_deref(),
        Some(second_session_id.as_str())
    );
    assert_eq!(
        state
            .open_tabs
            .iter()
            .filter(|tab| tab.id == "claude-1")
            .count(),
        1
    );

    manager
        .close_ai_session(&mut state, "claude-1")
        .expect("close restarted claude tab");
}

#[test]
fn set_active_session_clears_unseen_ready() {
    let manager = ProcessManager::new();
    let mut session = live_ai_runtime("claude-live", "claude-1");
    session.unseen_ready = true;
    manager.register_runtime_session(session);

    manager.set_active_session("claude-live");

    let runtime = manager.runtime_state();
    let session = runtime
        .sessions
        .get("claude-live")
        .expect("runtime session");
    assert!(!session.unseen_ready);
}
