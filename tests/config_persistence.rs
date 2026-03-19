use devmanager::models::{DefaultTerminal, Project, ProjectFolder, RunCommand, SSHConnection};
use devmanager::persistence::{
    load_config_from_path, load_config_from_str, load_session_from_str, save_config_to_path,
    WorkspaceSnapshot,
};
use devmanager::services::{ConfigImportMode, SessionManager};
use devmanager::state::AppState;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

fn load_fixture_state() -> AppState {
    let config_text =
        std::fs::read_to_string("tests/fixtures/legacy-config.json").expect("config fixture");
    let session_text =
        std::fs::read_to_string("tests/fixtures/legacy-session.json").expect("session fixture");
    let config = load_config_from_str(&config_text).expect("parse config");
    let session = load_session_from_str(&session_text).expect("parse session");
    AppState::from_workspace(WorkspaceSnapshot { config, session })
}

fn unique_temp_path(file_name: &str) -> PathBuf {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let dir = std::env::temp_dir().join(format!("devmanager-config-test-{millis:x}"));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir.join(file_name)
}

#[test]
fn native_config_edits_round_trip_through_disk() {
    let mut state = load_fixture_state();
    let mut settings = state.settings().clone();
    settings.default_terminal = DefaultTerminal::Cmd;
    settings.claude_command = Some("claude --resume".to_string());
    settings.codex_command = Some("codex --resume".to_string());
    settings.notification_sound = Some("chord".to_string());
    settings.restore_session_on_start = Some(false);
    settings.terminal_font_size = Some(16);
    state.update_settings(settings);

    state.upsert_project(Project {
        id: "project-native".to_string(),
        name: "Native".to_string(),
        root_path: "C:/Code/native".to_string(),
        folders: Vec::new(),
        color: Some("#22c55e".to_string()),
        pinned: Some(true),
        notes: Some("Line one\nLine two".to_string()),
        save_log_files: Some(false),
        created_at: "1700000000".to_string(),
        updated_at: "1700000000".to_string(),
    });

    state.upsert_folder(
        "project-native",
        ProjectFolder {
            id: "folder-native".to_string(),
            name: "app".to_string(),
            folder_path: "C:/Code/native/app".to_string(),
            commands: Vec::new(),
            env_file_path: Some(".env.local".to_string()),
            port_variable: Some("PORT".to_string()),
            hidden: Some(false),
        },
    );

    let mut env = HashMap::new();
    env.insert("RUST_LOG".to_string(), "debug".to_string());
    state.upsert_command(
        "project-native",
        "folder-native",
        RunCommand {
            id: "cmd-native".to_string(),
            label: "desktop".to_string(),
            command: "cargo".to_string(),
            args: vec!["run".to_string()],
            env: Some(env),
            port: Some(4000),
            auto_restart: Some(true),
            clear_logs_on_restart: Some(false),
        },
    );

    state.upsert_ssh_connection(SSHConnection {
        id: "ssh-native".to_string(),
        label: "Native Host".to_string(),
        host: "native.example.com".to_string(),
        port: 2222,
        username: "builder".to_string(),
        password: Some("secret".to_string()),
    });

    let config_path = unique_temp_path("config.json");
    save_config_to_path(&config_path, &state.config).expect("save config");
    let reloaded = load_config_from_path(&config_path).expect("reload config");

    assert_eq!(reloaded.settings.default_terminal, DefaultTerminal::Cmd);
    assert_eq!(
        reloaded.settings.notification_sound.as_deref(),
        Some("chord")
    );
    assert_eq!(reloaded.settings.restore_session_on_start, Some(false));
    assert_eq!(reloaded.settings.terminal_font_size, Some(16));

    let project = reloaded
        .projects
        .iter()
        .find(|project| project.id == "project-native")
        .expect("new project");
    assert_eq!(project.name, "Native");
    assert_eq!(project.color.as_deref(), Some("#22c55e"));
    assert_eq!(project.pinned, Some(true));
    assert_eq!(project.notes.as_deref(), Some("Line one\nLine two"));

    let folder = project
        .folders
        .iter()
        .find(|folder| folder.id == "folder-native")
        .expect("new folder");
    assert_eq!(folder.env_file_path.as_deref(), Some(".env.local"));
    assert_eq!(folder.port_variable.as_deref(), Some("PORT"));

    let command = folder
        .commands
        .iter()
        .find(|command| command.id == "cmd-native")
        .expect("new command");
    assert_eq!(command.command, "cargo");
    assert_eq!(command.args, vec!["run".to_string()]);
    assert_eq!(command.port, Some(4000));
    assert_eq!(command.auto_restart, Some(true));
    assert_eq!(
        command
            .env
            .as_ref()
            .and_then(|env| env.get("RUST_LOG"))
            .map(|value| value.as_str()),
        Some("debug")
    );

    let ssh = reloaded
        .ssh_connections
        .iter()
        .find(|connection| connection.id == "ssh-native")
        .expect("new ssh");
    assert_eq!(ssh.port, 2222);
    assert_eq!(ssh.username, "builder");
}

#[test]
fn saved_config_keeps_legacy_compatible_camel_case_shape() {
    let mut state = load_fixture_state();
    state.upsert_ssh_connection(SSHConnection {
        id: "ssh-shape".to_string(),
        label: "Shape".to_string(),
        host: "shape.example.com".to_string(),
        port: 22,
        username: "shape".to_string(),
        password: None,
    });

    let config_path = unique_temp_path("shape-config.json");
    save_config_to_path(&config_path, &state.config).expect("save config");
    let saved_json = std::fs::read_to_string(&config_path).expect("read config");

    assert!(saved_json.contains("\"rootPath\""));
    assert!(saved_json.contains("\"defaultTerminal\""));
    assert!(saved_json.contains("\"sshConnections\""));
    assert!(saved_json.contains("\"notes\""));

    let reparsed = load_config_from_str(&saved_json).expect("reparse saved json");
    assert!(reparsed
        .ssh_connections
        .iter()
        .any(|connection| connection.id == "ssh-shape"));
}

#[test]
fn import_replace_uses_imported_config() {
    let state = load_fixture_state();
    let imported = load_config_from_str(
        r#"{
          "version": 2,
          "projects": [
            {
              "id": "project-imported",
              "name": "Imported Workspace",
              "rootPath": "C:/Imported",
              "folders": [],
              "notes": "Imported notes",
              "createdAt": "1700000100",
              "updatedAt": "1700000100"
            }
          ],
          "settings": {
            "theme": "dark",
            "logBufferSize": 10000,
            "confirmOnClose": true,
            "minimizeToTray": false,
            "restoreSessionOnStart": false,
            "defaultTerminal": "cmd",
            "notificationSound": "none",
            "terminalFontSize": 15
          },
          "sshConnections": [
            {
              "id": "ssh-imported",
              "label": "Imported Host",
              "host": "imported.example.com",
              "port": 2022,
              "username": "ops"
            }
          ]
        }"#,
    )
    .expect("parse imported config");

    let replaced =
        SessionManager::apply_import_mode(&state.config, imported, ConfigImportMode::Replace);

    assert_eq!(replaced.projects.len(), 1);
    assert_eq!(replaced.projects[0].name, "Imported Workspace");
    assert_eq!(
        replaced.projects[0].notes.as_deref(),
        Some("Imported notes")
    );
    assert_eq!(replaced.settings.default_terminal, DefaultTerminal::Cmd);
    assert_eq!(replaced.settings.restore_session_on_start, Some(false));
    assert_eq!(replaced.ssh_connections.len(), 1);
    assert_eq!(replaced.ssh_connections[0].id, "ssh-imported");
}

#[test]
fn import_merge_adds_only_non_duplicate_project_names() {
    let state = load_fixture_state();
    let imported = load_config_from_str(
        r#"{
          "version": 2,
          "projects": [
            {
              "id": "project-duplicate",
              "name": "UserFirst",
              "rootPath": "C:/Duplicate",
              "folders": [],
              "notes": "Should not merge",
              "createdAt": "1700000200",
              "updatedAt": "1700000200"
            },
            {
              "id": "project-tools",
              "name": "Imported Tools",
              "rootPath": "C:/Tools",
              "folders": [],
              "notes": "Imported tool notes",
              "createdAt": "1700000300",
              "updatedAt": "1700000300"
            }
          ],
          "settings": {
            "theme": "dark",
            "logBufferSize": 10000,
            "confirmOnClose": true,
            "minimizeToTray": false,
            "restoreSessionOnStart": false,
            "defaultTerminal": "cmd",
            "notificationSound": "none",
            "terminalFontSize": 19
          },
          "sshConnections": [
            {
              "id": "ssh-imported",
              "label": "Imported Host",
              "host": "imported.example.com",
              "port": 2022,
              "username": "ops"
            }
          ]
        }"#,
    )
    .expect("parse imported config");

    let merged =
        SessionManager::apply_import_mode(&state.config, imported, ConfigImportMode::Merge);

    assert_eq!(merged.projects.len(), state.config.projects.len() + 1);
    assert!(merged
        .projects
        .iter()
        .any(|project| project.name == "Imported Tools"
            && project.notes.as_deref() == Some("Imported tool notes")));
    assert_eq!(
        merged
            .projects
            .iter()
            .filter(|project| project.name == "UserFirst")
            .count(),
        1
    );
    assert_eq!(merged.settings, state.config.settings);
    assert_eq!(merged.ssh_connections, state.config.ssh_connections);
}
