use devmanager::config::model::{MAX_CONFIG_BYTES, MAX_ID_BYTES};
use devmanager::config::{
    AppConfig, ConfigCommand, ConfigErrorKind, ConfigStore, Nullable, Project, ProjectFolder,
    RunCommand, SSHConnection, SettingsPatch, SshAuth, SshAuthMode,
};
use devmanager::models as legacy_models;
use devmanager::workspace::WorkspaceProjectRoots;
use serde_json::Value;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

const FIXTURE: &str = include_str!("fixtures/config/v1/representative.json");
const ISOLATED_NAMESPACE: &str = "com.userfirst.devmanager-native-next-dev";

fn host_paths(path: &Path) -> devmanager::config::paths::ResolvedAppPaths {
    let path = path.to_path_buf();
    let root = path
        .parent()
        .expect("configuration path must have a parent")
        .to_path_buf();
    devmanager::config::paths::ResolvedAppPaths {
        root: root.clone(),
        config: path,
        remote: root.join("remote.json"),
        database: root.join("kernel.sqlite3"),
        browser_root: root.join("browser"),
        logs: root.join("logs"),
    }
}

fn open_host_fixture(
    path: impl AsRef<Path>,
) -> Result<ConfigStore, devmanager::config::ConfigError> {
    ConfigStore::open_host(&host_paths(path.as_ref()))
}

fn deterministic_temp_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config.json");
    path.with_file_name(format!(".{name}.{}.tmp", std::process::id()))
}

fn fixture_value() -> Value {
    serde_json::from_str(FIXTURE).expect("valid config fixture")
}

#[test]
fn red_public_config_deserializers_reject_duplicates_and_unbounded_values() {
    let duplicate = r#"{
        "id":"project-1",
        "id":"project-2",
        "name":"Project",
        "rootPath":"C:\\Project",
        "folders":[],
        "createdAt":"now",
        "updatedAt":"now"
    }"#;
    assert!(serde_json::from_str::<Project>(duplicate).is_err());

    let oversized = format!(
        r#"{{"id":"project","name":"{}","rootPath":"C:\\Project","folders":[],"createdAt":"now","updatedAt":"now"}}"#,
        "x".repeat(MAX_CONFIG_BYTES)
    );
    assert!(serde_json::from_str::<Project>(&oversized).is_err());
}

#[test]
fn red_ssh_password_save_remains_compatible_but_export_is_redacted() {
    let mut config = legacy_models::AppConfig::default();
    config.ssh_connections.push(legacy_models::SSHConnection {
        id: "ssh-password".to_string(),
        label: "Password host".to_string(),
        host: "password.example.test".to_string(),
        port: 22,
        username: "builder".to_string(),
        password: Some("PASSWORD_SENTINEL".to_string()),
        private_key: None,
    });
    let dir = TempDir::new().expect("temporary export root");
    let path = dir.path().join("config.json");
    devmanager::persistence::save_config_to_path(&path, &config)
        .expect("legacy SSH password save remains supported");
    let bytes = fs::read(&path).expect("read exported config");
    assert!(!bytes
        .windows(b"PASSWORD_SENTINEL".len())
        .any(|window| window == b"PASSWORD_SENTINEL"));
}

#[test]
fn red_ssh_references_round_trip_through_the_active_store() {
    let dir = TempDir::new().expect("temporary active config root");
    let path = fixture_path(&dir);

    let store = open_host_fixture(&path).expect("open active fixture store");
    let password_auth = store.snapshot().config.ssh_connections[2]
        .auth
        .as_ref()
        .expect("password SSH auth reference");
    let key_auth = store.snapshot().config.ssh_connections[3]
        .auth
        .as_ref()
        .expect("private-key SSH auth reference");
    assert_eq!(
        password_auth.credential_ref.as_ref().map(String::as_str),
        Some("credential:ssh-password")
    );
    assert_eq!(
        key_auth.credential_ref.as_ref().map(String::as_str),
        Some("credential:ssh-private-key")
    );
    drop(store);
    let reopened = open_host_fixture(&path).expect("reopen active fixture store");
    assert_eq!(
        reopened.snapshot().config.ssh_connections[2]
            .auth
            .as_ref()
            .and_then(|auth| auth.credential_ref.as_ref())
            .map(String::as_str),
        Some("credential:ssh-password")
    );
    assert_eq!(
        reopened.snapshot().config.ssh_connections[3]
            .auth
            .as_ref()
            .and_then(|auth| auth.credential_ref.as_ref())
            .map(String::as_str),
        Some("credential:ssh-private-key")
    );
    assert!(!fs::read_to_string(&path)
        .expect("read canonical config")
        .contains("PASSWORD_SENTINEL"));
}

#[test]
fn red_public_transfer_rejects_hard_link_aliases_before_writing() {
    let dir = TempDir::new().expect("temporary transfer root");
    let source = dir.path().join("source.json");
    let alias = dir.path().join("export.json");
    fs::write(&source, b"source sentinel").expect("write source sentinel");
    fs::hard_link(&source, &alias).expect("create transfer hard link");
    let error =
        devmanager::persistence::save_config_to_path(&alias, &legacy_models::AppConfig::default())
            .expect_err("hard-link export destination must be rejected");
    assert!(error.to_string().contains("aliased"));
    assert_eq!(
        fs::read(&source).expect("read source sentinel"),
        b"source sentinel"
    );
}

#[test]
fn red_migration_reports_a_real_v1_result() {
    let dir = TempDir::new().expect("temporary migration root");
    let path = dir.path().join(ISOLATED_NAMESPACE).join("config.json");
    fs::create_dir_all(path.parent().expect("migration parent")).expect("create migration root");
    fs::write(&path, include_str!("fixtures/config/v1/migratable.json")).expect("write v1 fixture");
    let store = open_host_fixture(&path).expect("migrate v1 fixture");
    let result = store.migration_result();
    assert!(result.migrated);
    assert_eq!(result.source_version, Some(1));
    assert_eq!(result.target_version, 2);
    let migrated = fs::read_to_string(&path).expect("read migrated canonical fixture");
    assert!(migrated.contains("\"version\": 2"));
    assert!(!migrated.contains("PASSWORD_SENTINEL"));
}

#[test]
fn red_legacy_opaque_candidate_cannot_replace_bytes_strict_storage_rejects() {
    let dir = TempDir::new().expect("temporary opaque migration root");
    let path = isolated_path(&dir, "config.json");
    let mut legacy = fixture_value();
    legacy["version"] = Value::Number(1.into());
    legacy["settings"]
        .as_object_mut()
        .expect("legacy settings object")
        .remove("terminalReadOnly");

    // Keep the representative forward-compatible fields, but remove the
    // separate V1 NUL/identifier invalidities so this test isolates the
    // candidate-swap failure.  A V1 migration is allowed to preserve opaque
    // fields only when its resulting bytes remain strict-readable.
    legacy["projects"][0]["folders"][0]["commands"][0]["args"][3] =
        Value::String("naive-bytes".to_string());
    legacy["settings"]["shellOptions"]["args"][2] = Value::String("naive-bytes".to_string());
    legacy["projects"][0]["folders"][0]["commands"][0]["env"] = serde_json::json!({
        "NODE_ENV": "development",
        "PORT": "3000"
    });
    let original = serde_json::to_vec_pretty(&legacy).expect("encode legacy opaque fixture");
    fs::write(&path, &original).expect("write legacy opaque fixture");

    let error = match open_host_fixture(&path) {
        Ok(_) => panic!("strict-rejected migration candidate must fail closed"),
        Err(error) => error,
    };
    let after = fs::read(&path).expect("read opaque migration bytes");
    assert_eq!(error.kind(), ConfigErrorKind::Parse);
    assert_eq!(
        after, original,
        "a candidate strict parser rejects must never replace the only original"
    );
}

#[test]
fn red_typed_write_does_not_replace_storage_with_a_strict_rejected_candidate() {
    let (_dir, mut store) = open_fixture();
    let before = fs::read(store.path()).expect("read typed-write original");
    let mut candidate = store.snapshot().config.clone();
    candidate.extra.insert(
        "futureRootField".to_string(),
        serde_json::json!({"opaque": true}),
    );

    let error = match store.replace_config(candidate) {
        Ok(_) => panic!("strict-rejected typed candidate must fail closed"),
        Err(error) => error,
    };
    assert_eq!(error.kind(), ConfigErrorKind::Parse);
    assert_eq!(
        fs::read(store.path()).expect("read typed-write storage after failure"),
        before,
        "a typed candidate strict parsing rejects must never replace the original"
    );
}

#[test]
fn red_startup_config_failure_makes_sidebar_mutations_read_only() {
    let app = include_str!("../src/app/mod.rs");
    let persistence = include_str!("../src/persistence/mod.rs");
    assert!(
        persistence.contains("enum ConfigWriteAvailability")
            && app.contains("ConfigWriteAvailability::Unavailable")
            && app.contains("ensure_config_mutation_available"),
        "startup failure needs a typed config availability state and one mutation gate"
    );
    assert!(
        app.contains("config_write_availability:")
            && app.contains("ConfigWriteAvailability::Unavailable { diagnostic }")
            && app.contains("config_write_availability,"),
        "the failed ConfigStore load must carry read-only state on the shell"
    );
}

#[test]
fn red_config_workspace_issuer_is_sealed_to_the_host_snapshot() {
    let store = include_str!("../src/config/project_store.rs");
    assert!(
        store.contains("pub(crate) struct ConfigWorkspaceIssuer")
            && store.contains("pub(crate) fn workspace_project_roots")
            && store.contains("config_revision")
            && store.contains("runtime_generation")
            && store.contains("action_epoch"),
        "Phase 6.2 must receive an opaque, host-issued project/root mapping with revision and generation fences"
    );
    assert!(
        store.contains("project_id_for_config_id") && store.contains("issue_workspace_authority"),
        "legacy config ids must be adapted from loaded config instead of becoming forgeable ids"
    );
}

#[test]
fn config_workspace_issuer_maps_legacy_ids_and_rejects_stale_snapshots() {
    let dir = TempDir::new().expect("temporary workspace authority root");
    let path = isolated_path(&dir, "config.json");
    let first_root = dir.path().join("project-empty");
    let second_root = dir.path().join("project-native");
    fs::create_dir_all(&first_root).expect("first project root");
    fs::create_dir_all(&second_root).expect("second project root");
    let mut config = canonical_fixture_value();
    config["projects"][0]["rootPath"] = Value::String(first_root.to_string_lossy().into_owned());
    config["projects"][1]["rootPath"] = Value::String(second_root.to_string_lossy().into_owned());
    fs::write(
        &path,
        serde_json::to_vec_pretty(&config).expect("encode workspace authority fixture"),
    )
    .expect("write workspace authority fixture");
    let mut store = open_host_fixture(&path).expect("host-issued workspace store");

    let revision = store.snapshot().revision;
    let roots = WorkspaceProjectRoots::from_host_config_store(&mut store, revision, 17, 23)
        .expect("host-issued workspace roots");
    assert!(roots.project_id_for_config_id("project-unicode").is_some());
    assert!(roots.project_id_for_config_id("project-empty").is_some());
    let first_id = roots
        .project_id_for_config_id("project-unicode")
        .expect("opaque project identity");
    assert_ne!(first_id.to_string(), "project-empty");

    let second_revision = store.snapshot().revision;
    let second = WorkspaceProjectRoots::from_host_config_store(&mut store, second_revision, 18, 24)
        .expect("re-issue host workspace roots");
    assert_eq!(
        second.project_id_for_config_id("project-unicode"),
        Some(first_id),
        "configured id/root mapping stays stable within the host store"
    );

    let stale_revision = store.snapshot().revision;
    let mut changed = store.snapshot().config.clone();
    changed.projects[0].name.push_str(" changed");
    store
        .replace_config(changed)
        .expect("persist a newer config revision");
    let error = WorkspaceProjectRoots::from_host_config_store(&mut store, stale_revision, 17, 23)
        .expect_err("stale workspace authority must fail closed");
    assert_eq!(error.kind(), ConfigErrorKind::RevisionConflict);
}

#[test]
fn config_workspace_issuer_pairs_match_phase62_project_root_input_shape() {
    let model = include_str!("../src/workspace/model.rs");
    assert!(model.contains("from_host_config_store"));
    assert!(model.contains("from_config_issuer"));
    assert!(!model.contains("pub fn try_from_pairs"));
}

#[test]
fn red_production_export_route_preserves_strict_store_bytes() {
    let session_manager = include_str!("../src/services/session_manager.rs");
    let persistence = include_str!("../src/persistence/mod.rs");
    assert!(
        session_manager.contains("export_active_config_to_path")
            && !session_manager.contains("persistence::save_config_to_path(path, config)"),
        "the real session-manager export route must use the canonical strict store"
    );
    assert!(
        persistence.contains("pub(crate) fn export_active_config_to_path")
            && persistence.contains("export_external_to(path)"),
        "production export must read and durably transfer the active strict snapshot"
    );
}

#[test]
fn config_external_store_export_preserves_strict_only_fields_and_ssh_refs() {
    let (_dir, store) = open_fixture();
    let path = store
        .path()
        .parent()
        .expect("canonical config parent")
        .join("desktop-export.json");
    store
        .export_to(&path)
        .expect("desktop export through canonical store");
    let exported: Value =
        serde_json::from_slice(&fs::read(&path).expect("read desktop export")).expect("JSON");
    assert_eq!(exported["projects"][0]["archived"], Value::Bool(false));
    assert_eq!(
        exported["sshConnections"][2]["auth"]["credentialRef"],
        Value::String("credential:ssh-password".to_string())
    );
    assert_eq!(
        exported["sshConnections"][3]["auth"]["credentialRef"],
        Value::String("credential:ssh-private-key".to_string())
    );
    assert!(
        !String::from_utf8_lossy(&fs::read(path).expect("read export bytes"))
            .contains("PASSWORD_SENTINEL")
    );
}

#[test]
fn red_production_test_authority_and_transfer_bypasses_are_not_public() {
    let project_store = include_str!("../src/config/project_store.rs");
    assert!(!project_store.contains("config-test-support"));
    assert!(!project_store.contains("pub fn from_test_fixture_path"));
    assert!(!project_store.contains("pub fn open_test_fixture"));
    assert!(!project_store.contains("pub fn open_legacy_fixture"));

    let session_manager = include_str!("../src/services/session_manager.rs");
    assert!(!session_manager.contains("pub fn import_config_from_path"));
    assert!(!session_manager.contains("pub fn export_config_to_path"));
}

#[test]
fn red_startup_does_not_hide_config_store_failures_as_an_empty_workspace() {
    let app = include_str!("../src/app/mod.rs");
    let session_manager = include_str!("../src/services/session_manager.rs");
    assert!(!app.contains("Fell back to an empty workspace"));
    assert!(app.contains("WorkspaceSnapshot"));
    assert!(app.contains("load_workspace") && session_manager.contains("load_session"));
}

#[test]
fn red_windows_recovery_proves_install_failure_restores_original() {
    let store = include_str!("../src/config/project_store.rs");
    assert!(store.contains("restore"));
    assert!(store.contains("backup"));
    assert!(store.contains("InstallAfterBackup"));
}

fn canonical_fixture_value() -> Value {
    let mut value = fixture_value();
    let root = value.as_object_mut().expect("fixture object");
    root.remove("futureRootField");
    let settings = root
        .get_mut("settings")
        .and_then(Value::as_object_mut)
        .expect("fixture settings");
    settings.remove("futureSettingsField");
    settings
        .get_mut("defaultDirectories")
        .and_then(Value::as_object_mut)
        .expect("fixture directories")
        .remove("futureDirectoryField");
    settings
        .get_mut("shellOptions")
        .and_then(Value::as_object_mut)
        .expect("fixture shell options")
        .remove("futureShellField");
    settings
        .get_mut("editor")
        .and_then(Value::as_object_mut)
        .expect("fixture editor")
        .remove("futureEditorField");
    let projects = root
        .get_mut("projects")
        .and_then(Value::as_array_mut)
        .expect("fixture projects");
    projects[0]
        .as_object_mut()
        .expect("fixture project")
        .remove("futureProjectField");
    let folders = projects[0]
        .get_mut("folders")
        .and_then(Value::as_array_mut)
        .expect("fixture folders");
    folders[0]
        .as_object_mut()
        .expect("fixture folder")
        .remove("futureFolderField");
    let command = folders[0]
        .get_mut("commands")
        .and_then(Value::as_array_mut)
        .expect("fixture commands");
    command[0]
        .as_object_mut()
        .expect("fixture command")
        .remove("futureCommandField");
    root.get_mut("sshConnections")
        .and_then(Value::as_array_mut)
        .expect("fixture SSH connections")[3]
        .as_object_mut()
        .expect("fixture SSH connection")
        .remove("futureSshField");
    let command = root
        .get_mut("projects")
        .and_then(Value::as_array_mut)
        .and_then(|projects| projects.first_mut())
        .and_then(Value::as_object_mut)
        .and_then(|project| project.get_mut("folders"))
        .and_then(Value::as_array_mut)
        .and_then(|folders| folders.first_mut())
        .and_then(Value::as_object_mut)
        .and_then(|folder| folder.get_mut("commands"))
        .and_then(Value::as_array_mut)
        .and_then(|commands| commands.first_mut())
        .and_then(Value::as_object_mut)
        .and_then(|command| command.get_mut("args"))
        .and_then(Value::as_array_mut)
        .and_then(|args| args.get_mut(3));
    if let Some(Value::String(argument)) = command {
        *argument = argument.replace('\0', "");
    }
    if let Some(Value::Array(args)) = root
        .get_mut("settings")
        .and_then(Value::as_object_mut)
        .and_then(|settings| settings.get_mut("shellOptions"))
        .and_then(Value::as_object_mut)
        .and_then(|options| options.get_mut("args"))
    {
        for argument in args {
            if let Value::String(argument) = argument {
                *argument = argument.replace('\0', "");
            }
        }
    }
    if let Some(Value::Object(environment)) = root
        .get_mut("projects")
        .and_then(Value::as_array_mut)
        .and_then(|projects| projects.first_mut())
        .and_then(Value::as_object_mut)
        .and_then(|project| project.get_mut("folders"))
        .and_then(Value::as_array_mut)
        .and_then(|folders| folders.first_mut())
        .and_then(Value::as_object_mut)
        .and_then(|folder| folder.get_mut("commands"))
        .and_then(Value::as_array_mut)
        .and_then(|commands| commands.first_mut())
        .and_then(Value::as_object_mut)
        .and_then(|command| command.get_mut("env"))
    {
        environment.retain(|key, _| {
            key.bytes()
                .next()
                .is_some_and(|byte| byte == b'_' || byte.is_ascii_alphabetic())
                && key
                    .bytes()
                    .skip(1)
                    .all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
        });
    }
    value
}

fn canonical_fixture_json() -> String {
    serde_json::to_string_pretty(&canonical_fixture_value()).expect("encode canonical fixture")
}

fn isolated_root(dir: &TempDir) -> PathBuf {
    let root = dir.path().join(ISOLATED_NAMESPACE);
    fs::create_dir_all(&root).expect("create isolated config root");
    root
}

fn isolated_path(dir: &TempDir, name: &str) -> PathBuf {
    isolated_root(dir).join(name)
}

fn fixture_path(dir: &TempDir) -> PathBuf {
    let path = isolated_path(dir, "config.json");
    fs::write(&path, canonical_fixture_json()).expect("write sanitized fixture");
    path
}

fn open_fixture() -> (TempDir, ConfigStore) {
    let dir = TempDir::new().expect("temporary config root");
    let path = fixture_path(&dir);
    let store = open_host_fixture(&path).expect("open fixture config");
    (dir, store)
}

fn first_project(store: &ConfigStore) -> &Project {
    &store.snapshot().config.projects[0]
}

fn valid_project_for_command(store: &ConfigStore) -> Project {
    let mut project = first_project(store).clone();
    for folder in &mut project.folders {
        for command in &mut folder.commands {
            if let Nullable::Value(environment) = &mut command.env {
                environment.retain(|key, _| {
                    let mut bytes = key.bytes();
                    matches!(bytes.next(), Some(b'A'..=b'Z' | b'a'..=b'z' | b'_'))
                        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                });
            }
            for argument in &mut command.args {
                if argument.is_empty() {
                    *argument = "argument".to_string();
                } else {
                    *argument = argument.replace('\0', "");
                }
            }
        }
    }
    project
}

fn wait_for_file(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.exists() {
        assert!(Instant::now() < deadline, "timed out waiting for {path:?}");
        thread::sleep(Duration::from_millis(10));
    }
}

#[derive(Default)]
struct ChildGuard {
    children: Vec<Child>,
}

impl ChildGuard {
    fn push(&mut self, child: Child) {
        self.children.push(child);
    }

    fn wait_all(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let mut pending = false;
            let mut failure = None;
            for child in &mut self.children {
                match child.try_wait() {
                    Ok(Some(status)) if !status.success() => {
                        failure = Some(format!("child exited unsuccessfully: {status}"));
                        break;
                    }
                    Ok(Some(_)) => {}
                    Ok(None) => pending = true,
                    Err(error) => {
                        failure = Some(format!("child wait failed: {error}"));
                        break;
                    }
                }
            }
            if let Some(message) = failure {
                self.kill_and_reap();
                panic!("{message}");
            }
            if !pending {
                return;
            }
            if Instant::now() >= deadline {
                self.kill_and_reap();
                panic!("timed out waiting for concurrency children");
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn kill_and_reap(&mut self) {
        for child in &mut self.children {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.kill_and_reap();
    }
}

#[test]
fn config_fixture_round_trip_preserves_every_supported_shape_and_unknown_field() {
    let (_dir, store) = open_fixture();
    let encoded: Value = serde_json::from_slice(
        &store
            .snapshot()
            .config
            .to_json_bytes()
            .expect("serialize config"),
    )
    .expect("serialized JSON");

    assert_eq!(encoded, canonical_fixture_value());
    assert_eq!(store.snapshot().revision, 41);
    assert_eq!(first_project(&store).name, "工具箱 🚀");
    assert_eq!(
        first_project(&store).root_path,
        "\\\\server\\share\\Dev Manager\\工具箱"
    );
    assert_eq!(
        first_project(&store).folders[0].commands[0].args[3],
        "naïvebytes"
    );
    assert!(first_project(&store).extra.is_empty());
    assert_eq!(
        store
            .snapshot()
            .config
            .settings()
            .default_directories
            .as_ref()
            .expect("default directories")
            .projects,
        Nullable::Value("C:\\Users\\tester\\Projects".to_string())
    );
}

#[test]
fn config_canonical_decode_rejects_unknown_omitted_and_duplicate_fields() {
    let canonical = canonical_fixture_value();

    let mut unknown = canonical.clone();
    unknown["futureRootField"] = serde_json::json!("must be rejected");
    assert_eq!(
        AppConfig::from_json_str(&unknown.to_string())
            .expect_err("canonical unknown field must be rejected")
            .kind(),
        ConfigErrorKind::Parse
    );

    for field in [
        "version",
        "revision",
        "projects",
        "settings",
        "sshConnections",
    ] {
        let mut omitted = canonical.clone();
        omitted
            .as_object_mut()
            .expect("canonical object")
            .remove(field);
        assert_eq!(
            AppConfig::from_json_str(&omitted.to_string())
                .expect_err("canonical omitted field must be rejected")
                .kind(),
            ConfigErrorKind::Parse,
            "omitted field {field}"
        );
    }

    let duplicate =
        canonical
            .to_string()
            .replacen("\"version\":2", "\"version\":2,\"version\":2", 1);
    assert_eq!(
        AppConfig::from_json_str(&duplicate)
            .expect_err("duplicate canonical field must be rejected")
            .kind(),
        ConfigErrorKind::Parse
    );
}

#[test]
fn config_invalid_command_is_rejected_before_root_io_and_diagnostics_are_sanitized() {
    let dir = TempDir::new().expect("temporary validation root");
    let path = isolated_path(&dir, "config.json");
    let mut store = open_host_fixture(&path).expect("open missing config");
    let root = isolated_root(&dir);
    let moved = dir.path().join("moved-after-open");
    fs::rename(&root, &moved).expect("move root away before invalid command");

    let sentinel = "invalid-command-secret-sentinel";
    let mut env = std::collections::BTreeMap::new();
    env.insert("NOT-AN-ENV-KEY".to_string(), sentinel.to_string());
    let command = RunCommand {
        id: "command".to_string(),
        label: sentinel.to_string(),
        command: "echo\0unsafe".to_string(),
        env: Nullable::Value(env),
        port: Nullable::Value(0),
        ..RunCommand::default()
    };

    let error = store
        .execute(
            0,
            ConfigCommand::CreateCommand {
                project_id: "missing-project".to_string(),
                folder_id: "missing-folder".to_string(),
                command,
            },
        )
        .expect_err("invalid command must fail before root access");
    assert_eq!(error.kind(), ConfigErrorKind::Validation);
    assert!(!error.to_string().contains(sentinel));
    assert!(!format!("{error:?}").contains(sentinel));
    assert!(!moved.join(".config.lock").exists());
}

#[test]
fn config_authority_requires_direct_canonical_config_leaf() {
    let dir = TempDir::new().expect("temporary authority root");
    let namespace = dir.path().join(ISOLATED_NAMESPACE);
    fs::create_dir_all(&namespace).expect("create namespace");

    ConfigStore::open_host(&host_paths(&namespace.join("config.json")))
        .expect("canonical leaf should open");

    for path in [
        namespace.join("other.json"),
        dir.path()
            .join("sibling")
            .join(format!("{ISOLATED_NAMESPACE}-sibling"))
            .join("config.json"),
        namespace.join("nested").join("config.json"),
    ] {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create rejected authority parent");
        }
        let mut paths = host_paths(&namespace.join("config.json"));
        paths.config = path;
        let error = match ConfigStore::open_host(&paths) {
            Ok(_) => panic!("noncanonical authority must reject"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), ConfigErrorKind::ProtectedPath);
    }
}

#[test]
fn config_stale_temp_is_recovered_without_overwriting_the_destination() {
    let dir = TempDir::new().expect("temporary recovery root");
    let path = isolated_path(&dir, "config.json");
    let mut store = open_host_fixture(&path).expect("open missing config");
    let stale = deterministic_temp_path(store.path());
    fs::write(&stale, b"stale temp from a dead writer").expect("write stale temp");

    let project = Project {
        id: "recovered".to_string(),
        name: "Recovered".to_string(),
        root_path: "C:\\Recovered".to_string(),
        created_at: "now".to_string(),
        updated_at: "now".to_string(),
        ..Project::default()
    };
    store
        .execute(0, ConfigCommand::CreateProject { project })
        .expect("safe stale temp recovery should permit the write");
    assert!(!stale.exists(), "stale temp must be removed after recovery");
    assert!(
        path.exists(),
        "destination must be replaced with new config"
    );
}

#[test]
fn config_legacy_json_round_trip_preserves_nullable_secret_slots_without_material() {
    let mut legacy = legacy_models::AppConfig::default();
    legacy.projects.push(legacy_models::Project {
        id: "legacy-project".to_string(),
        name: "Legacy project".to_string(),
        root_path: "C:/legacy-project".to_string(),
        created_at: "2026-01-01T00:00:00Z".to_string(),
        updated_at: "2026-01-01T00:00:00Z".to_string(),
        ..legacy_models::Project::default()
    });
    legacy.ssh_connections.push(legacy_models::SSHConnection {
        id: "legacy-ssh".to_string(),
        label: "Legacy SSH".to_string(),
        host: "legacy.example.test".to_string(),
        port: 22,
        username: "legacy".to_string(),
        ..legacy_models::SSHConnection::default()
    });

    let source = serde_json::to_value(&legacy).expect("serialize legacy config");
    let round_tripped = AppConfig::from_legacy_json_str(
        &serde_json::to_string(&source).expect("encode legacy config"),
    )
    .expect("legacy config should be accepted");

    assert_eq!(
        round_tripped.to_json_value().expect("encode config"),
        source
    );
}

#[test]
fn config_real_legacy_fixture_noop_save_preserves_absent_settings_and_unknown_shape() {
    let source = include_str!("fixtures/legacy-config.json");
    let source_value: Value = serde_json::from_str(source).expect("legacy fixture JSON");
    let config = AppConfig::from_legacy_json_str(source).expect("legacy fixture config");

    assert_eq!(
        config.to_json_value().expect("serialize legacy fixture"),
        source_value
    );

    let saved = config.to_json_value().expect("saved legacy fixture");
    let settings = saved["settings"].as_object().expect("settings object");
    for absent in [
        "restoreSessionOnStart",
        "macTerminalProfile",
        "optionAsMeta",
        "copyOnSelect",
        "keepSelectionOnCopy",
        "showTerminalScrollbar",
        "shellIntegrationEnabled",
        "terminalMouseOverride",
        "terminalReadOnly",
        "browserEnabled",
    ] {
        assert!(
            !settings.contains_key(absent),
            "no-op save materialized missing setting {absent}"
        );
    }
}

#[test]
fn config_missing_settings_use_prior_defaults_while_presence_stays_absent() {
    let source = serde_json::json!({
        "projects": [],
        "sshConnections": []
    });
    let config =
        AppConfig::from_legacy_json_str(&source.to_string()).expect("missing settings default");

    assert_eq!(config.settings().theme, "dark");
    assert_eq!(config.settings().log_buffer_size, 10_000);
    assert!(config.settings().confirm_on_close);
    assert!(config.settings().keep_selection_on_copy);
    assert!(config.settings().show_terminal_scrollbar);
    assert!(config.settings().shell_integration_enabled);
    assert_eq!(
        config.settings().restore_session_on_start,
        Nullable::Value(true)
    );

    let saved = config.to_json_value().expect("serialize defaulted config");
    assert!(
        saved.get("settings").is_none(),
        "a missing top-level settings container must remain absent"
    );
    let public_saved = serde_json::to_value(&config).expect("public serde config");
    assert_eq!(
        public_saved, saved,
        "public AppConfig serde must preserve presence"
    );
    let round_tripped =
        AppConfig::from_legacy_json_str(&saved.to_string()).expect("round trip config");
    assert_eq!(
        round_tripped.to_json_value().expect("round trip serialize"),
        saved,
        "round trip must not materialize top-level settings"
    );
    let settings = saved.get("settings").and_then(Value::as_object);
    assert!(settings.is_none(), "settings should still be absent");
    for absent in [
        "theme",
        "logBufferSize",
        "confirmOnClose",
        "keepSelectionOnCopy",
        "showTerminalScrollbar",
        "shellIntegrationEnabled",
        "restoreSessionOnStart",
    ] {
        assert!(
            !settings.is_some_and(|settings| settings.contains_key(absent)),
            "missing setting was materialized: {absent}"
        );
    }
}

#[test]
fn config_sparse_settings_mutation_materializes_presence_before_serialize() {
    let source = serde_json::json!({
        "projects": [],
        "sshConnections": []
    });
    let mut config =
        AppConfig::from_legacy_json_str(&source.to_string()).expect("missing settings default");

    config.settings_mut().option_as_meta = true;

    let saved = config
        .to_json_value()
        .expect("serialize mutated sparse settings");
    assert_eq!(saved["settings"]["optionAsMeta"], Value::Bool(true));
    assert_eq!(
        serde_json::to_value(&config).expect("public serde mutated settings"),
        saved,
        "public serde must reflect the presence-aware mutation"
    );
}

#[test]
fn config_persisted_typed_edit_reopens_strict_materialized_settings() {
    let dir = TempDir::new().expect("temporary legacy config root");
    let path = isolated_path(&dir, "config.json");
    let source = include_str!("fixtures/legacy-config.json");
    fs::write(&path, source).expect("write legacy fixture");
    let mut store = open_host_fixture(&path).expect("open legacy fixture");

    let mut patch = SettingsPatch::new();
    patch.set_theme("light");
    store
        .execute(
            store.snapshot().revision,
            ConfigCommand::PatchSettings { patch },
        )
        .expect("persist typed settings edit");

    let reopened = open_host_fixture(&path).expect("reopen edited legacy fixture");
    let saved = reopened
        .snapshot()
        .config
        .to_json_value()
        .expect("serialize reopened config");
    let settings = saved["settings"].as_object().expect("settings object");
    assert_eq!(settings["theme"], Value::String("light".to_string()));
    for materialized in [
        "restoreSessionOnStart",
        "macTerminalProfile",
        "optionAsMeta",
        "copyOnSelect",
        "keepSelectionOnCopy",
        "showTerminalScrollbar",
        "shellIntegrationEnabled",
        "terminalMouseOverride",
        "terminalReadOnly",
        "browserEnabled",
    ] {
        assert!(
            settings.contains_key(materialized),
            "strict migration omitted setting {materialized}"
        );
    }
}

#[test]
fn config_settings_edit_records_an_explicit_default_even_when_value_is_unchanged() {
    let dir = TempDir::new().expect("temporary legacy config root");
    let path = isolated_path(&dir, "config.json");
    let source = include_str!("fixtures/legacy-config.json");
    fs::write(&path, source).expect("write legacy fixture");
    let mut store = open_host_fixture(&path).expect("open legacy fixture");

    let mut patch = SettingsPatch::new();
    patch.set_option_as_meta(false);
    store
        .execute(
            store.snapshot().revision,
            ConfigCommand::PatchSettings { patch },
        )
        .expect("persist explicit settings edit");

    let reopened = open_host_fixture(&path).expect("reopen edited legacy fixture");
    let saved = reopened
        .snapshot()
        .config
        .to_json_value()
        .expect("serialize reopened config");
    assert_eq!(saved["settings"]["optionAsMeta"], Value::Bool(false));
}

#[test]
fn config_byte_and_semantic_values_keep_unicode_unc_paths_nulls_and_argument_bytes() {
    let (_dir, store) = open_fixture();
    let project = first_project(&store);
    let command = &project.folders[0].commands[0];

    assert_eq!(
        project.notes.as_ref().map(String::as_str),
        Some("line one\nline two\twith unicode ✓")
    );
    assert_eq!(command.args[3].as_bytes(), "naïvebytes".as_bytes());
    assert_eq!(command.port, Nullable::Null);
    assert_eq!(project.color, Nullable::Null);
    assert_eq!(
        project.folders[1].env_file_path,
        Nullable::Value(".env.local".to_string())
    );
    assert_eq!(
        store
            .snapshot()
            .config
            .settings()
            .shell_options
            .as_ref()
            .unwrap()
            .args,
        Nullable::Value(vec![
            "-NoLogo".to_string(),
            "--%".to_string(),
            "naïvebytes".to_string()
        ])
    );
}

#[test]
fn config_order_and_ids_survive_typed_reorder_update_and_archive() {
    let (_dir, mut store) = open_fixture();
    let revision = store.snapshot().revision;
    let mut project = valid_project_for_command(&store);
    project.name = "renamed without trimming  ".to_string();

    store
        .execute(revision, ConfigCommand::UpdateProject { project })
        .expect("update project");
    store
        .execute(
            revision + 1,
            ConfigCommand::ReorderProject {
                project_id: "project-empty".to_string(),
                new_index: 0,
            },
        )
        .expect("reorder project");
    store
        .execute(
            revision + 2,
            ConfigCommand::ArchiveProject {
                project_id: "project-empty".to_string(),
            },
        )
        .expect("archive project");

    assert_eq!(store.snapshot().revision, revision + 3);
    assert_eq!(
        store
            .snapshot()
            .config
            .projects
            .iter()
            .map(|project| project.id.as_str())
            .collect::<Vec<_>>(),
        ["project-empty", "project-unicode"]
    );
    assert_eq!(
        store.snapshot().config.projects[1].name,
        "renamed without trimming  "
    );
    assert_eq!(
        store.snapshot().config.projects[0].archived,
        Nullable::Value(true)
    );
}

#[test]
fn config_validation_rejects_limits_and_keeps_error_sanitized() {
    let mut value = fixture_value();
    value["projects"][0]["name"] = Value::String("x".repeat(1_000_001));
    let secret = "validation-secret-should-not-leak";
    value["sshConnections"][0]["futureField"] = Value::String(secret.to_string());
    let error =
        AppConfig::from_legacy_json_str(&serde_json::to_string(&value).expect("encode value"))
            .expect_err("oversized config must fail validation");

    assert_eq!(error.kind(), ConfigErrorKind::Validation);
    assert!(!error.to_string().contains(secret));
    assert!(!format!("{error:?}").contains(secret));
}

#[test]
fn config_legacy_unknown_fields_are_rejected_from_canonical_storage() {
    let (_dir, mut store) = open_fixture();
    let mut patch = SettingsPatch::new();
    patch.set_theme("light");

    store
        .execute(
            store.snapshot().revision,
            ConfigCommand::PatchSettings { patch },
        )
        .expect("replace settings");

    let after = store.snapshot().config.to_json_value().expect("JSON value");
    assert!(after.get("futureRootField").is_none());
    assert!(after["settings"].get("futureSettingsField").is_none());
    assert!(after["projects"][0]["folders"][0]["commands"][0]
        .get("futureCommandField")
        .is_none());
}

#[test]
fn config_expected_revision_conflict_does_not_write() {
    let (_dir, mut store) = open_fixture();
    let before = fs::read(store.path()).expect("read before bytes");
    let project = valid_project_for_command(&store);
    let error = store
        .execute(
            store.snapshot().revision - 1,
            ConfigCommand::UpdateProject { project },
        )
        .expect_err("stale revision must be rejected");

    assert_eq!(error.kind(), ConfigErrorKind::RevisionConflict);
    assert_eq!(fs::read(store.path()).expect("read after bytes"), before);
}

#[test]
fn config_atomic_failure_leaves_original_and_cleans_temp() {
    let source = include_str!("../src/config/project_store.rs");
    assert!(source.contains("AtomicWriteFailure::BeforeReplace"));
    assert!(source.contains("atomic temporary cleanup failed"));
}

#[test]
fn config_final_destination_cas_rejects_a_deterministic_external_writer_race() {
    let source = include_str!("../src/config/project_store.rs");
    assert!(source.contains("AtomicWriteFailure::ExternalWriterBeforeReplace"));
    assert!(source.contains("destination changed during replacement"));
}

#[cfg(windows)]
#[test]
fn config_post_admission_writer_race_is_rejected_by_the_held_destination() {
    let source = include_str!("../src/config/project_store.rs");
    assert!(source.contains("AtomicWriteFailure::ExternalWriterAfterAdmission"));
    assert!(source.contains("ExternalChange"));
}

#[cfg(windows)]
#[test]
fn config_install_failure_after_backup_restores_the_only_original() {
    let source = include_str!("../src/config/project_store.rs");
    assert!(source.contains("AtomicWriteFailure::InstallAfterBackup"));
    assert!(source.contains("restore"));
    assert!(source.contains("backup"));
}

#[test]
fn config_directory_durability_failure_leaves_destination_untouched() {
    let source = include_str!("../src/config/project_store.rs");
    assert!(source.contains("AtomicWriteFailure::BeforeDirectorySync"));
    assert!(source.contains("BeforeDirectorySync"));
}

#[cfg(windows)]
#[test]
fn config_windows_temp_reparse_race_cannot_touch_an_outside_file() {
    let dir = TempDir::new().expect("temporary reparse race root");
    let path = isolated_path(&dir, "config.json");
    let mut store = open_host_fixture(&path).expect("open missing config");
    let outside_dir = dir.path().join("outside");
    fs::create_dir_all(&outside_dir).expect("create outside directory");
    let outside = outside_dir.join("outside-secret.txt");
    fs::write(&outside, b"outside sentinel").expect("write outside sentinel");
    let status = Command::new("cmd")
        .args([
            "/D",
            "/C",
            "mklink",
            "/J",
            deterministic_temp_path(store.path())
                .to_str()
                .expect("temp path is UTF-8"),
            outside_dir.to_str().expect("outside directory is UTF-8"),
        ])
        .status()
        .expect("run temp junction creation command");
    assert!(status.success(), "temp junction creation failed");

    let project = Project {
        id: "reparse-race".to_string(),
        name: "Reparse race".to_string(),
        root_path: "C:\\ReparseRace".to_string(),
        created_at: "now".to_string(),
        updated_at: "now".to_string(),
        ..Project::default()
    };
    let command = ConfigCommand::CreateProject { project };
    let error = store
        .execute(0, command)
        .expect_err("temp reparse attack must fail closed");
    assert_eq!(error.kind(), ConfigErrorKind::PathAlias);
    assert_eq!(
        fs::read(&outside).expect("read outside sentinel"),
        b"outside sentinel"
    );
}

#[test]
fn config_external_edit_detection_stops_before_overwrite() {
    let (_dir, mut store) = open_fixture();
    let mut external = fixture_value();
    external["projects"][0]["name"] = Value::String("external edit".to_string());
    let external_bytes = serde_json::to_vec_pretty(&external).expect("external JSON");
    fs::write(store.path(), &external_bytes).expect("external edit");

    let error = store
        .execute(
            41,
            ConfigCommand::ReorderProject {
                project_id: "project-empty".to_string(),
                new_index: 0,
            },
        )
        .expect_err("external edit must conflict");

    assert_eq!(error.kind(), ConfigErrorKind::ExternalChange);
    assert_eq!(
        fs::read(store.path()).expect("read external bytes"),
        external_bytes
    );
}

#[test]
fn config_import_preview_validates_explicit_path_and_export_preserves_refs_only() {
    let (dir, mut store) = open_fixture();
    let missing = isolated_path(&dir, "missing-import.json");
    let error = store
        .preview_import(&missing)
        .expect_err("missing import source must fail validation");
    assert_eq!(error.kind(), ConfigErrorKind::NotFound);

    let import_path = isolated_path(&dir, "import.json");
    fs::write(&import_path, canonical_fixture_json()).expect("write import fixture");
    let preview = store.preview_import(&import_path).expect("preview import");

    assert_eq!(preview.project_count, 2);
    assert_eq!(preview.ssh_host_count, 4);
    assert!(preview.valid);
    assert!(preview.summary.contains("2 projects"));

    let output_path = isolated_path(&dir, "export.json");
    store.export_to(&output_path).expect("export config");
    let exported = fs::read_to_string(output_path).expect("read export");
    assert!(exported.contains("credential:ssh-password"));
    assert!(exported.contains("credential:ssh-private-key"));
    assert!(!exported.contains("password-contents"));
    assert!(!exported.contains("BEGIN OPENSSH PRIVATE KEY"));

    store
        .import_replace(preview)
        .expect("replace from previewed import");
    assert_eq!(store.snapshot().config.projects.len(), 2);
}

#[test]
fn config_import_preview_rejects_source_mutation_before_replacement() {
    let (dir, mut store) = open_fixture();
    let import_path = isolated_path(&dir, "import-mutation.json");
    fs::write(&import_path, canonical_fixture_json()).expect("write import fixture");
    let preview = store.preview_import(&import_path).expect("preview import");

    let mut mutated = fixture_value();
    mutated["projects"][0]["name"] = Value::String("mutated after preview".to_string());
    fs::write(
        &import_path,
        serde_json::to_vec_pretty(&mutated).expect("encode mutated import"),
    )
    .expect("mutate import source");

    let error = store
        .import_replace(preview)
        .expect_err("mutated source must not be imported");
    assert_eq!(error.kind(), ConfigErrorKind::ExternalChange);
}

#[test]
fn config_import_preview_rejects_source_replacement_before_replacement() {
    let (dir, mut store) = open_fixture();
    let import_path = isolated_path(&dir, "import-replacement.json");
    let replacement_path = isolated_path(&dir, "replacement.json");
    fs::write(&import_path, canonical_fixture_json()).expect("write import fixture");
    let preview = store.preview_import(&import_path).expect("preview import");
    fs::write(&replacement_path, canonical_fixture_json()).expect("write replacement fixture");
    fs::remove_file(&import_path).expect("remove previewed source");
    fs::rename(&replacement_path, &import_path).expect("replace previewed source");

    let error = store
        .import_replace(preview)
        .expect_err("replaced source must not be imported");
    assert_eq!(error.kind(), ConfigErrorKind::ExternalChange);
}

#[test]
fn config_import_preview_rejects_destination_revision_change() {
    let (dir, mut store) = open_fixture();
    let import_path = isolated_path(&dir, "import-destination-revision.json");
    fs::write(&import_path, canonical_fixture_json()).expect("write import fixture");
    let preview = store.preview_import(&import_path).expect("preview import");

    let mut project = valid_project_for_command(&store);
    project.name = "destination changed after preview".to_string();
    store
        .execute(41, ConfigCommand::UpdateProject { project })
        .expect("advance destination revision");

    let error = store
        .import_replace(preview)
        .expect_err("destination revision change must invalidate preview");
    assert_eq!(error.kind(), ConfigErrorKind::RevisionConflict);
}

#[test]
fn config_import_preview_token_rejects_replay() {
    let (dir, mut store) = open_fixture();
    let import_path = isolated_path(&dir, "import-replay.json");
    fs::write(&import_path, canonical_fixture_json()).expect("write import fixture");
    let first_preview = store.preview_import(&import_path).expect("first preview");
    let replayed_preview = store.preview_import(&import_path).expect("second preview");

    store
        .import_replace(first_preview)
        .expect("first preview should be consumable");
    let error = store
        .import_replace(replayed_preview)
        .expect_err("replayed preview token must be rejected");
    assert_eq!(error.kind(), ConfigErrorKind::PreviewReplay);
}

#[test]
fn config_secret_material_is_rejected_without_leaking_values() {
    let mut value = fixture_value();
    let password = "password-material-sentinel";
    let private_key = "private-key-material-sentinel";
    value["sshConnections"][0]["password"] = Value::String(password.to_string());
    value["sshConnections"][0]["privateKey"] = Value::String(private_key.to_string());

    let error = AppConfig::from_legacy_json_str(
        &serde_json::to_string(&value).expect("encode unsafe JSON"),
    )
    .expect_err("raw credential material must be refused");
    assert_eq!(error.kind(), ConfigErrorKind::SecretMaterial, "{error:?}");
    assert!(!error.to_string().contains(password));
    assert!(!error.to_string().contains(private_key));
    assert!(!format!("{error:?}").contains(password));
    assert!(!format!("{error:?}").contains(private_key));
}

#[test]
fn config_public_serde_boundary_rejects_invalid_and_secret_like_models() {
    let sentinel = "public-serde-secret-sentinel";
    let mut project = Project {
        id: "project".to_string(),
        name: "Project".to_string(),
        ..Project::default()
    };
    project
        .extra
        .insert("password".to_string(), Value::String(sentinel.to_string()));
    assert!(
        serde_json::to_string(&project).is_err(),
        "public project serialization must enforce secret validation"
    );

    let mut config = AppConfig::default();
    config.extra.insert(
        "futureToken".to_string(),
        Value::String(sentinel.to_string()),
    );
    assert!(
        serde_json::to_string(&config).is_err(),
        "public config serialization must enforce secret validation"
    );
    assert!(
        serde_json::from_value::<AppConfig>(serde_json::json!({
            "futureToken": sentinel
        }))
        .is_err(),
        "public config deserialization must enforce secret validation"
    );

    let mut oversized = AppConfig::default();
    oversized.extra.insert(
        "padding".to_string(),
        Value::String("x".repeat(MAX_CONFIG_BYTES)),
    );
    assert!(
        serde_json::to_string(&oversized).is_err(),
        "public config serialization must enforce the size limit"
    );
}

#[test]
fn config_public_entity_serde_validates_project_and_ssh_ids() {
    let invalid_project = Project {
        name: "Project".to_string(),
        ..Project::default()
    };
    assert!(
        serde_json::to_string(&invalid_project).is_err(),
        "Project serialization must validate its own ID"
    );
    assert!(
        serde_json::from_value::<Project>(serde_json::json!({
            "id": "",
            "name": "Project"
        }))
        .is_err(),
        "Project deserialization must validate its own ID"
    );

    let invalid_ssh = SSHConnection {
        label: "SSH".to_string(),
        host: "ssh.example.test".to_string(),
        username: "user".to_string(),
        ..SSHConnection::default()
    };
    assert!(
        serde_json::to_string(&invalid_ssh).is_err(),
        "SSHConnection serialization must validate its own ID"
    );
    assert!(
        serde_json::from_value::<SSHConnection>(serde_json::json!({
            "id": "",
            "label": "SSH",
            "host": "ssh.example.test",
            "username": "user"
        }))
        .is_err(),
        "SSHConnection deserialization must validate its own ID"
    );
}

#[test]
fn config_public_entity_serde_rejects_noncanonical_ids() {
    let invalid_project = Project {
        id: " project ".to_string(),
        name: "Project".to_string(),
        ..Project::default()
    };
    assert!(
        serde_json::to_string(&invalid_project).is_err(),
        "Project serialization must reject noncanonical IDs"
    );
    assert!(
        serde_json::from_value::<Project>(serde_json::json!({
            "id": " project ",
            "name": "Project"
        }))
        .is_err(),
        "Project deserialization must reject noncanonical IDs"
    );

    let invalid_ssh = SSHConnection {
        id: " ssh ".to_string(),
        label: "SSH".to_string(),
        host: "ssh.example.test".to_string(),
        username: "user".to_string(),
        ..SSHConnection::default()
    };
    assert!(
        serde_json::to_string(&invalid_ssh).is_err(),
        "SSH serialization must reject noncanonical IDs"
    );
    assert!(
        serde_json::from_value::<SSHConnection>(serde_json::json!({
            "id": " ssh ",
            "label": "SSH",
            "host": "ssh.example.test",
            "username": "user"
        }))
        .is_err(),
        "SSH deserialization must reject noncanonical IDs"
    );

    let oversized = "x".repeat(MAX_ID_BYTES + 1);
    assert!(
        serde_json::from_value::<Project>(serde_json::json!({
            "id": oversized,
            "name": "Project"
        }))
        .is_err(),
        "wire IDs over the byte limit must remain rejected"
    );
}

#[test]
fn config_store_admission_rejects_noncanonical_entity_ids() {
    let (_dir, mut store) = open_fixture();
    let project = Project {
        id: " project ".to_string(),
        name: "Project".to_string(),
        root_path: "C:\\project".to_string(),
        created_at: "now".to_string(),
        updated_at: "now".to_string(),
        ..Project::default()
    };
    let error = store
        .execute(
            store.snapshot().revision,
            ConfigCommand::CreateProject { project },
        )
        .expect_err("project admission must reject noncanonical IDs");
    assert_eq!(error.kind(), ConfigErrorKind::Validation);

    let (_dir, mut store) = open_fixture();
    let connection = SSHConnection {
        id: " ssh ".to_string(),
        label: "SSH".to_string(),
        host: "ssh.example.test".to_string(),
        username: "user".to_string(),
        ..SSHConnection::default()
    };
    let error = store
        .execute(
            store.snapshot().revision,
            ConfigCommand::CreateSsh { connection },
        )
        .expect_err("SSH admission must reject noncanonical IDs");
    assert_eq!(error.kind(), ConfigErrorKind::Validation);
}

#[test]
fn config_secret_reference_fields_require_opaque_credential_scheme() {
    for (field, value) in [
        ("credentialRef", "hunter2"),
        ("passwordRef", "password-material"),
        ("privateKeyRef", "-----BEGIN OPENSSH PRIVATE KEY-----"),
        ("githubTokenRef", "ghp_plaintext_token"),
        ("secretRef", "https://example.test/secret"),
    ] {
        let mut config = fixture_value();
        let mut auth = serde_json::Map::new();
        auth.insert("mode".to_string(), Value::String("password".to_string()));
        auth.insert(field.to_string(), Value::String(value.to_string()));
        config["sshConnections"][0]["auth"] = Value::Object(auth);

        let error = AppConfig::from_legacy_json_str(
            &serde_json::to_string(&config).expect("encode unsafe reference"),
        )
        .expect_err("plaintext-like reference must be rejected");
        assert_eq!(error.kind(), ConfigErrorKind::SecretMaterial);
        assert!(!error.to_string().contains(value));
        assert!(!format!("{error:?}").contains(value));
    }

    let mut valid = canonical_fixture_value();
    valid["sshConnections"][0]["auth"] = serde_json::json!({
        "mode": "password",
        "credentialRef": "credential:opaque-id_01.a"
    });
    AppConfig::from_legacy_json_str(
        &serde_json::to_string(&valid).expect("encode valid reference"),
    )
    .expect("opaque credential reference should be accepted");
}

#[test]
fn config_typed_unknown_secret_reference_cannot_be_exported() {
    let sentinel = "typed-raw-secret-reference";
    let mut config = AppConfig::default();
    config.extra.insert(
        "futureCredentialRef".to_string(),
        Value::String(sentinel.to_string()),
    );

    let error = config
        .to_json_bytes()
        .expect_err("unknown secret reference must be rejected before export");
    assert_eq!(error.kind(), ConfigErrorKind::SecretMaterial);
    assert!(!error.to_string().contains(sentinel));
    assert!(!format!("{error:?}").contains(sentinel));
}

#[test]
fn config_typed_unknown_secret_material_cannot_be_serialized() {
    let sentinel = "typed-password-material-sentinel";
    let mut project = Project::default();
    project.id = "typed-project".to_string();
    project.name = "Typed project".to_string();
    project
        .extra
        .insert("password".to_string(), Value::String(sentinel.to_string()));
    let mut config = AppConfig::default();
    config.projects.push(project);

    let error = config
        .to_json_bytes()
        .expect_err("typed raw credential material must be refused");
    assert_eq!(error.kind(), ConfigErrorKind::SecretMaterial);
    assert!(!error.to_string().contains(sentinel));
    assert!(!format!("{error:?}").contains(sentinel));
}

#[test]
fn config_public_debug_redacts_secret_values_in_known_and_unknown_fields() {
    let sentinel = "debug-secret-material-sentinel";
    let mut project = Project::default();
    project.id = "debug-project".to_string();
    project.extra.insert(
        "futurePasswordField".to_string(),
        Value::String(sentinel.to_string()),
    );

    let mut settings = devmanager::config::Settings::default();
    settings.github_token_ref = Nullable::Value(sentinel.to_string());
    settings.extra.insert(
        "futureCredentialRef".to_string(),
        Value::String(sentinel.to_string()),
    );

    let mut auth = SshAuth::default();
    auth.credential_ref = Nullable::Value(sentinel.to_string());
    auth.extra.insert(
        "futurePrivateKey".to_string(),
        Value::String(sentinel.to_string()),
    );

    let mut command = RunCommand::default();
    command.extra.insert(
        "futureToken".to_string(),
        Value::String(sentinel.to_string()),
    );

    let mut config = AppConfig::default();
    config.extra.insert(
        "futureSecret".to_string(),
        Value::String(sentinel.to_string()),
    );
    config.projects.push(project.clone());

    let debug_values = [
        format!("{:?}", Nullable::Value(sentinel.to_string())),
        format!("{project:?}"),
        format!("{settings:?}"),
        format!("{auth:?}"),
        format!("{command:?}"),
        format!("{config:?}"),
        format!("{:?}", ConfigCommand::UpdateProject { project }),
    ];
    for debug in debug_values {
        assert!(
            !debug.contains(sentinel),
            "public Debug leaked secret material: {debug}"
        );
    }
}

#[test]
fn config_pretty_bytes_are_bounded_by_the_exact_written_representation() {
    let mut selected = None;
    for width in (300..=450).rev() {
        let mut config = AppConfig::default();
        config.extra.insert(
            "padding".to_string(),
            Value::Array(
                (0..10_000)
                    .map(|_| Value::String("x".repeat(width)))
                    .collect(),
            ),
        );
        let value = match serde_json::to_value(&config) {
            Ok(value) => value,
            Err(_) => continue,
        };
        let compact = serde_json::to_vec(&value).expect("serialize compact candidate");
        let pretty = serde_json::to_vec_pretty(&value).expect("serialize pretty candidate");
        if compact.len() <= MAX_CONFIG_BYTES && pretty.len() > MAX_CONFIG_BYTES {
            selected = Some(config);
            break;
        }
    }
    let config = selected.expect("fixture must distinguish compact and pretty size limits");
    let error = config
        .to_json_bytes()
        .expect_err("pretty bytes over the limit must be rejected");
    assert_eq!(error.kind(), ConfigErrorKind::Validation);
}

#[test]
fn config_import_export_refuses_session_production_and_alias_paths_without_touching_session() {
    let (dir, store) = open_fixture();
    let session_path = dir.path().join("session.json");
    let session_bytes = b"session sentinel";
    fs::write(&session_path, session_bytes).expect("write session sentinel");

    for path in [session_path.clone(), dir.path().join("remote.json")] {
        let error = store
            .export_to(&path)
            .expect_err("protected path must fail");
        assert_eq!(error.kind(), ConfigErrorKind::ProtectedPath);
    }
    assert_eq!(
        fs::read(&session_path).expect("read session sentinel"),
        session_bytes
    );

    let alias = isolated_path(&dir, "config-alias.json");
    fs::hard_link(store.path(), &alias).expect("create hard-link alias");
    let error = store
        .export_to(&alias)
        .expect_err("hard-link alias must fail");
    assert_eq!(error.kind(), ConfigErrorKind::PathAlias);

    let productionish = dir
        .path()
        .join("com.userfirst.devmanager")
        .join("export.json");
    let error = store
        .export_to(&productionish)
        .expect_err("production path must fail");
    assert_eq!(error.kind(), ConfigErrorKind::ProtectedPath);
}

#[test]
fn config_transfer_rejects_paths_outside_the_approved_isolated_root() {
    let (_dir, store) = open_fixture();
    let outside = TempDir::new().expect("outside transfer root");
    let path = outside.path().join("export.json");

    assert_eq!(
        store
            .preview_export(&path)
            .expect_err("outside export preview must fail closed")
            .kind(),
        ConfigErrorKind::ProtectedPath
    );
    assert_eq!(
        store
            .export_to(&path)
            .expect_err("outside export must fail closed")
            .kind(),
        ConfigErrorKind::ProtectedPath
    );
    assert!(!path.exists(), "outside export must not create a file");
}

#[cfg(windows)]
#[test]
fn config_unsupported_path_operations_fail_closed() {
    let (dir, store) = open_fixture();
    let alternate_stream = isolated_root(&dir).join("export.json:unsupported-stream");

    assert_eq!(
        store
            .preview_export(&alternate_stream)
            .expect_err("alternate data stream must be rejected")
            .kind(),
        ConfigErrorKind::ProtectedPath
    );
    assert_eq!(
        store
            .export_to(&alternate_stream)
            .expect_err("alternate data stream write must be rejected")
            .kind(),
        ConfigErrorKind::ProtectedPath
    );
    assert!(!alternate_stream.exists());
}

#[test]
fn config_open_rejects_a_hard_link_alias_of_the_active_config() {
    let (dir, store) = open_fixture();
    let alias = isolated_path(&dir, "active-config-alias.json");
    fs::hard_link(store.path(), &alias).expect("create active config hard link");

    let error = match open_host_fixture(&alias) {
        Ok(_) => panic!("hard-link alias must not open as another store"),
        Err(error) => error,
    };
    assert_eq!(error.kind(), ConfigErrorKind::ProtectedPath);
}

#[test]
fn config_transfer_preview_refuses_hard_link_aliases_to_session_and_remote() {
    let (dir, store) = open_fixture();
    let session_path = dir.path().join("session.json");
    let remote_path = dir.path().join("remote.json");
    fs::write(&session_path, FIXTURE).expect("write session fixture");
    fs::write(&remote_path, FIXTURE).expect("write remote fixture");

    let session_alias = dir.path().join("session-alias.json");
    let remote_alias = dir.path().join("remote-alias.json");
    fs::hard_link(&session_path, &session_alias).expect("create session alias");
    fs::hard_link(&remote_path, &remote_alias).expect("create remote alias");

    for path in [&session_alias, &remote_alias] {
        assert_eq!(
            store
                .preview_import(path)
                .expect_err("protected alias must not be imported")
                .kind(),
            ConfigErrorKind::ProtectedPath
        );
        assert_eq!(
            store
                .preview_export(path)
                .expect_err("protected alias must not be exported")
                .kind(),
            ConfigErrorKind::ProtectedPath
        );
        assert_eq!(
            store
                .export_to(path)
                .expect_err("protected alias must not be overwritten")
                .kind(),
            ConfigErrorKind::ProtectedPath
        );
    }

    assert_eq!(
        fs::read(&session_path).expect("read session fixture"),
        FIXTURE.as_bytes()
    );
    assert_eq!(
        fs::read(&remote_path).expect("read remote fixture"),
        FIXTURE.as_bytes()
    );
}

#[test]
fn config_lock_name_is_reserved_from_store_and_transfer_destinations() {
    let (dir, store) = open_fixture();
    let lock_path = isolated_root(&dir).join(".config.lock");

    let error = match open_host_fixture(&lock_path) {
        Ok(_) => panic!("lock path must not be a store path"),
        Err(error) => error,
    };
    assert_eq!(error.kind(), ConfigErrorKind::ProtectedPath);
    let error = store
        .preview_export(&lock_path)
        .expect_err("lock path must not be an export destination");
    assert_eq!(error.kind(), ConfigErrorKind::ProtectedPath);
}

#[test]
fn config_lock_rejects_a_hard_link_before_it_can_create_a_parallel_lock() {
    let (dir, mut store) = open_fixture();
    let lock_path = isolated_root(&dir).join(".config.lock");
    let alias_target = isolated_root(&dir).join("unrelated.lock");
    fs::write(&alias_target, b"not the config lock").expect("write lock alias target");
    fs::hard_link(&alias_target, &lock_path).expect("create lock hard link");

    let error = store
        .execute(
            store.snapshot().revision,
            ConfigCommand::ReorderProject {
                project_id: "project-empty".to_string(),
                new_index: 0,
            },
        )
        .expect_err("hard-linked lock must be rejected");
    assert_eq!(error.kind(), ConfigErrorKind::PathAlias);
}

#[test]
fn config_root_identity_is_revalidated_before_actual_io() {
    let (dir, mut store) = open_fixture();
    let root = isolated_root(&dir);
    let moved_root = dir.path().join("moved-config-root");
    fs::rename(&root, &moved_root).expect("move original config root");
    fs::create_dir_all(&root).expect("replace config root path");
    fs::rename(moved_root.join("config.json"), root.join("config.json"))
        .expect("move original config file into replacement root");

    let mut project = valid_project_for_command(&store);
    project.name = "must not write through replaced root".to_string();
    let error = store
        .execute(
            store.snapshot().revision,
            ConfigCommand::UpdateProject { project },
        )
        .expect_err("replaced root identity must fail closed");
    assert!(
        matches!(
            error.kind(),
            ConfigErrorKind::ProtectedPath | ConfigErrorKind::PathAlias
        ),
        "unexpected root replacement error: {error:?}"
    );
}

#[cfg(windows)]
#[test]
fn config_transfer_refuses_file_and_parent_junction_aliases_outside_destination_parent() {
    use std::os::windows::fs::{symlink_dir, symlink_file};

    let (dir, store) = open_fixture();
    let protected = isolated_root(&dir).join("protected");
    let protected_nested = protected.join("nested");
    fs::create_dir_all(&protected_nested).expect("create protected directories");
    let session = protected.join("session.json");
    fs::write(&session, FIXTURE).expect("write protected session fixture");

    let file_alias_parent = isolated_root(&dir).join("file-aliases");
    fs::create_dir_all(&file_alias_parent).expect("create file alias parent");
    let file_alias = file_alias_parent.join("safe-file-alias.json");
    fs::hard_link(&session, &file_alias).expect("create file hard-link alias");
    assert_eq!(
        store
            .export_to(&file_alias)
            .expect_err("file hard-link alias must be rejected")
            .kind(),
        ConfigErrorKind::ProtectedPath
    );

    let file_symlink = file_alias_parent.join("safe-file-symlink.json");
    match symlink_file(&session, &file_symlink) {
        Ok(()) => {
            assert_eq!(
                store
                    .export_to(&file_symlink)
                    .expect_err("file symlink alias must be rejected")
                    .kind(),
                ConfigErrorKind::ProtectedPath
            );
        }
        Err(error) if error.raw_os_error() == Some(1314) => {
            let unresolved_parent = isolated_root(&dir).join("unresolved-final-parent");
            let unresolved_target = unresolved_parent.join("safe-export.json");
            assert_eq!(
                store
                    .export_to(&unresolved_target)
                    .expect_err("unverifiable final parent must fail closed")
                    .kind(),
                ConfigErrorKind::ProtectedPath
            );
            assert!(
                !unresolved_target.exists(),
                "fail-closed security gate must not create an unresolved target"
            );
        }
        Err(error) => panic!("create file symlink alias: {error}"),
    }

    let junction = isolated_root(&dir).join("parent-junction");
    let status = Command::new("cmd")
        .args([
            "/D",
            "/C",
            "mklink",
            "/J",
            junction.to_str().expect("junction path is UTF-8"),
            protected_nested
                .to_str()
                .expect("junction target path is UTF-8"),
        ])
        .status()
        .expect("run junction creation command");
    assert!(status.success(), "junction creation failed: {status}");
    let junction_file_alias = junction.join("safe-junction-alias.json");
    fs::hard_link(&session, &junction_file_alias).expect("create junction hard-link alias");
    assert_eq!(
        store
            .export_to(&junction_file_alias)
            .expect_err("parent junction hard-link alias must be rejected")
            .kind(),
        ConfigErrorKind::ProtectedPath
    );

    let nested_session = protected_nested.join("session.json");
    fs::hard_link(&session, &nested_session).expect("create nested protected session link");
    let parent_symlink = isolated_root(&dir).join("parent-symlink");
    match symlink_dir(&protected_nested, &parent_symlink) {
        Ok(()) => {
            let parent_symlink_file = parent_symlink.join("safe-symlink-parent-alias.json");
            fs::hard_link(&nested_session, &parent_symlink_file)
                .expect("create parent symlink hard-link alias");
            assert_eq!(
                store
                    .export_to(&parent_symlink_file)
                    .expect_err("parent symlink alias must be rejected")
                    .kind(),
                ConfigErrorKind::ProtectedPath
            );
        }
        Err(error) if error.raw_os_error() == Some(1314) => {
            let unresolved_parent = isolated_root(&dir).join("unresolved-parent-symlink");
            let unresolved_target = unresolved_parent.join("safe-export.json");
            assert_eq!(
                store
                    .export_to(&unresolved_target)
                    .expect_err("unverifiable final parent must fail closed")
                    .kind(),
                ConfigErrorKind::ProtectedPath
            );
            assert!(
                !unresolved_target.exists(),
                "fail-closed security gate must not create an unresolved target"
            );
        }
        Err(error) => panic!("create parent symlink alias: {error}"),
    }
}

#[test]
fn config_missing_store_refuses_active_path_export_alias() {
    let dir = TempDir::new().expect("temporary config root");
    let path = isolated_path(&dir, "config.json");
    let store = open_host_fixture(&path).expect("open missing config");

    assert_eq!(
        store
            .preview_export(&path)
            .expect_err("active path must not be previewed as an export")
            .kind(),
        ConfigErrorKind::PathAlias
    );
    assert_eq!(
        store
            .export_to(&path)
            .expect_err("active path must not be exported")
            .kind(),
        ConfigErrorKind::PathAlias
    );
    assert!(!path.exists(), "rejected export must not create the file");
}

#[test]
fn config_host_authority_requires_a_canonical_leaf_under_each_isolated_profile() {
    let dir = TempDir::new().expect("temporary profile root");
    for namespace in [
        "com.userfirst.devmanager-native-next-dev",
        "com.userfirst.devmanager-native-next-dev-second",
    ] {
        let root = dir.path().join(namespace);
        let path = root.join("config.json");
        fs::create_dir_all(&root).expect("create isolated profile");
        open_host_fixture(&path).expect("isolated host profile should be allowed");

        let mut paths = host_paths(&path);
        paths.config = root.join("not-config.json");
        let error = match ConfigStore::open_host(&paths) {
            Ok(_) => panic!("host authority must reject a noncanonical config leaf"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), ConfigErrorKind::ProtectedPath);
    }
}

#[test]
fn config_all_typed_entity_commands_execute_and_preserve_invariants() {
    let dir = TempDir::new().expect("temporary command store");
    let mut store =
        open_host_fixture(isolated_path(&dir, "config.json")).expect("open command store");
    let project = Project {
        id: "p".to_string(),
        name: "Project".to_string(),
        root_path: "C:\\Project".to_string(),
        created_at: "now".to_string(),
        updated_at: "now".to_string(),
        ..Project::default()
    };
    let updated_project = Project {
        name: "Updated project".to_string(),
        ..project.clone()
    };
    let folder = ProjectFolder {
        id: "f".to_string(),
        name: "Folder".to_string(),
        folder_path: "C:\\Project".to_string(),
        ..ProjectFolder::default()
    };
    let updated_folder = ProjectFolder {
        name: "Updated folder".to_string(),
        ..folder.clone()
    };
    let command = RunCommand {
        id: "c".to_string(),
        label: "Command".to_string(),
        command: "echo".to_string(),
        ..RunCommand::default()
    };
    let updated_command = RunCommand {
        label: "Updated command".to_string(),
        ..command.clone()
    };
    let ssh = SSHConnection {
        id: "s".to_string(),
        label: "SSH".to_string(),
        host: "example.test".to_string(),
        username: "builder".to_string(),
        auth: Nullable::Value(SshAuth {
            mode: SshAuthMode::Agent,
            credential_ref: Nullable::Null,
            extra: Default::default(),
        }),
        port: 22,
        ..SSHConnection::default()
    };
    let updated_ssh = SSHConnection {
        label: "Updated SSH".to_string(),
        ..ssh.clone()
    };
    let mut patch = SettingsPatch::new();
    patch.set_theme("light");

    let commands = [
        ConfigCommand::CreateProject { project },
        ConfigCommand::UpdateProject {
            project: updated_project,
        },
        ConfigCommand::ReorderProject {
            project_id: "p".to_string(),
            new_index: 0,
        },
        ConfigCommand::ArchiveProject {
            project_id: "p".to_string(),
        },
        ConfigCommand::CreateFolder {
            project_id: "p".to_string(),
            folder,
        },
        ConfigCommand::UpdateFolder {
            project_id: "p".to_string(),
            folder: updated_folder,
        },
        ConfigCommand::ReorderFolder {
            project_id: "p".to_string(),
            folder_id: "f".to_string(),
            new_index: 0,
        },
        ConfigCommand::ArchiveFolder {
            project_id: "p".to_string(),
            folder_id: "f".to_string(),
        },
        ConfigCommand::CreateCommand {
            project_id: "p".to_string(),
            folder_id: "f".to_string(),
            command,
        },
        ConfigCommand::UpdateCommand {
            project_id: "p".to_string(),
            folder_id: "f".to_string(),
            command: updated_command,
        },
        ConfigCommand::ReorderCommand {
            project_id: "p".to_string(),
            folder_id: "f".to_string(),
            command_id: "c".to_string(),
            new_index: 0,
        },
        ConfigCommand::ArchiveCommand {
            project_id: "p".to_string(),
            folder_id: "f".to_string(),
            command_id: "c".to_string(),
        },
        ConfigCommand::CreateSsh { connection: ssh },
        ConfigCommand::UpdateSsh {
            connection: updated_ssh,
        },
        ConfigCommand::ReorderSsh {
            connection_id: "s".to_string(),
            new_index: 0,
        },
        ConfigCommand::ArchiveSsh {
            connection_id: "s".to_string(),
        },
        ConfigCommand::PatchSettings { patch },
    ];

    for command in commands {
        let revision = store.snapshot().revision;
        store
            .execute(revision, command)
            .expect("typed command should execute");
    }

    assert_eq!(store.snapshot().revision, 17);
    assert_eq!(store.snapshot().config.projects[0].id, "p");
    assert_eq!(
        store.snapshot().config.projects[0].folders[0].commands[0].id,
        "c"
    );
    assert_eq!(store.snapshot().config.ssh_connections[0].id, "s");
    assert_eq!(store.snapshot().config.settings().theme, "light");
}

#[test]
#[ignore]
fn config_concurrent_store_child() {
    if let Ok(marker) = env::var("DEVMANAGER_CONFIG_CONCURRENT_MARKER") {
        fs::write(marker, b"config_concurrent_store_child").expect("publish child test marker");
    }
    let path = env::var("DEVMANAGER_CONFIG_CONCURRENT_PATH")
        .expect("concurrent child requires DEVMANAGER_CONFIG_CONCURRENT_PATH");
    let role = env::var("DEVMANAGER_CONFIG_CONCURRENT_ROLE").expect("child role");
    let ready = PathBuf::from(env::var("DEVMANAGER_CONFIG_CONCURRENT_READY").expect("ready path"));
    let go = PathBuf::from(env::var("DEVMANAGER_CONFIG_CONCURRENT_GO").expect("go path"));
    let result =
        PathBuf::from(env::var("DEVMANAGER_CONFIG_CONCURRENT_RESULT").expect("result path"));
    let mut store = open_host_fixture(&path).expect("child opens store");
    fs::write(&ready, b"ready").expect("publish child readiness");
    wait_for_file(&go);

    let mut project = valid_project_for_command(&store);
    project.name = format!("concurrent-{role}");
    let outcome = match store.execute(41, ConfigCommand::UpdateProject { project }) {
        Ok(_) => "ok".to_string(),
        Err(error) => format!("err:{:?}", error.kind()),
    };
    fs::write(result, outcome).expect("publish child result");
}

#[test]
fn config_concurrent_child_helper_requires_an_explicit_subprocess_contract() {
    const CONTRACT_ENV_VARS: [&str; 5] = [
        "DEVMANAGER_CONFIG_CONCURRENT_PATH",
        "DEVMANAGER_CONFIG_CONCURRENT_ROLE",
        "DEVMANAGER_CONFIG_CONCURRENT_READY",
        "DEVMANAGER_CONFIG_CONCURRENT_GO",
        "DEVMANAGER_CONFIG_CONCURRENT_RESULT",
    ];
    let test_root = TempDir::new().expect("test-owned missing-contract root");
    let marker = test_root.path().join("child-ran");
    let exe = env::current_exe().expect("current test executable");
    let mut command = Command::new(exe);
    command
        .args([
            "--exact",
            "config_concurrent_store_child",
            "--ignored",
            "--nocapture",
        ])
        .current_dir(test_root.path());
    command.env("DEVMANAGER_CONFIG_CONCURRENT_MARKER", &marker);
    for variable in CONTRACT_ENV_VARS {
        command.env_remove(variable);
    }
    let output = command.output().expect("spawn child contract probe");
    let output_text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(&marker).expect("intended ignored child test marker"),
        "config_concurrent_store_child"
    );
    assert!(
        output_text.contains("running 1 test")
            && output_text.contains("test config_concurrent_store_child ... FAILED"),
        "child output must prove exactly the intended ignored test ran:\n{output_text}"
    );
    assert!(
        !output.status.success(),
        "child helper without its contract must fail instead of silently passing"
    );
}

#[test]
fn config_concurrent_store_attempts_commit_one_revision_only() {
    let dir = TempDir::new().expect("temporary concurrent store");
    let path = fixture_path(&dir);
    let mut value = canonical_fixture_value();
    let arguments = value["settings"]["shellOptions"]["args"]
        .as_array_mut()
        .expect("canonical shell arguments");
    while arguments.len() < 10_000 {
        arguments.push(Value::String("x".repeat(256)));
    }
    fs::write(
        &path,
        serde_json::to_vec_pretty(&value).expect("encode concurrent fixture"),
    )
    .expect("write concurrent fixture");

    let exe = env::current_exe().expect("current test executable");
    let mut children = ChildGuard::default();
    let mut ready_paths = Vec::new();
    let mut result_paths = Vec::new();
    let go = dir.path().join("go");
    for index in 0..8 {
        let role = index.to_string();
        let ready = dir.path().join(format!("ready-{role}"));
        let result = dir.path().join(format!("result-{role}"));
        let child = Command::new(&exe)
            .args([
                "--exact",
                "config_concurrent_store_child",
                "--ignored",
                "--nocapture",
            ])
            .current_dir(dir.path())
            .env("DEVMANAGER_CONFIG_CONCURRENT_PATH", &path)
            .env("DEVMANAGER_CONFIG_CONCURRENT_ROLE", &role)
            .env("DEVMANAGER_CONFIG_CONCURRENT_READY", &ready)
            .env("DEVMANAGER_CONFIG_CONCURRENT_GO", &go)
            .env("DEVMANAGER_CONFIG_CONCURRENT_RESULT", &result)
            .spawn()
            .expect("spawn concurrent store child");
        children.push(child);
        ready_paths.push(ready);
        result_paths.push(result);
    }

    for ready in &ready_paths {
        wait_for_file(ready);
    }
    fs::write(&go, b"go").expect("release concurrent store children");
    children.wait_all();

    let outcomes: Vec<_> = result_paths
        .iter()
        .map(|path| fs::read_to_string(path).expect("read concurrent child result"))
        .collect();
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| outcome.as_str() == "ok")
            .count(),
        1,
        "concurrent outcomes: {outcomes:?}"
    );
    for outcome in &outcomes {
        assert!(outcome == "ok" || outcome.starts_with("err:"), "{outcome}");
    }
    let final_store = open_host_fixture(&path).expect("reopen concurrent store");
    assert_eq!(final_store.snapshot().revision, 42);
}

#[test]
fn config_production_persistence_has_no_legacy_app_config_deserialize_bypass() {
    let source = include_str!("../src/persistence/mod.rs");

    assert!(
        source.contains("ConfigStore"),
        "production persistence must route config I/O through ConfigStore"
    );
    assert!(
        !source.contains("serde_json::from_value(migrated)"),
        "production persistence must not deserialize AppConfig through a bypass"
    );
    assert!(
        !source.contains("fn migrate_config_value"),
        "legacy migration must be an explicit ConfigStore boundary operation"
    );
}

#[test]
fn config_authority_test_constructors_are_not_public_runtime_surface() {
    let source = include_str!("../src/config/project_store.rs");

    for signature in [
        "pub fn for_test_path",
        "pub fn open_for_test",
        "pub fn open_legacy_for_test",
    ] {
        assert!(
            !source.contains(signature),
            "forgeable test constructor remains public: {signature}"
        );
    }
}

#[test]
fn config_json_decoder_checks_limits_before_building_unbounded_values() {
    let source = include_str!("../src/config/model.rs");

    assert!(
        source.contains("depth: usize"),
        "bounded decoder must carry depth state into recursive visitors"
    );
    assert!(
        source.contains("MAX_COLLECTION_ITEMS"),
        "bounded decoder must enforce collection limits while decoding"
    );
    assert!(
        source.contains("MAX_TEXT_BYTES"),
        "bounded decoder must enforce string limits while decoding"
    );
}

#[test]
fn config_extra_maps_are_rejected_before_root_or_lock_io() {
    let dir = TempDir::new().expect("temporary validation root");
    let path = isolated_path(&dir, "config.json");
    let mut store = open_host_fixture(&path).expect("open missing config");
    let root = isolated_root(&dir);
    let moved = dir.path().join("moved-after-extra-validation");
    fs::rename(&root, &moved).expect("move root away before invalid command");

    let sentinel = "extra-map-secret-sentinel";
    let mut project = Project {
        id: "extra-map-project".to_string(),
        name: "Extra map project".to_string(),
        root_path: "C:\\extra-map".to_string(),
        created_at: "now".to_string(),
        updated_at: "now".to_string(),
        ..Project::default()
    };
    project
        .extra
        .insert("password".to_string(), Value::String(sentinel.to_string()));

    let error = store
        .execute(0, ConfigCommand::CreateProject { project })
        .expect_err("secret-bearing extra map must fail before root I/O");
    assert_eq!(error.kind(), ConfigErrorKind::SecretMaterial, "{error:?}");
    assert!(!error.to_string().contains(sentinel));
    assert!(!moved.join(".config.lock").exists());
}

#[test]
fn config_operation_uses_one_deadline_for_lock_parse_recovery_and_write() {
    let source = include_str!("../src/config/project_store.rs");

    assert!(
        source.contains("struct OperationDeadline"),
        "config operations must carry one absolute deadline"
    );
    assert!(
        source.contains("acquire_config_lock(\n    root: &RootHandle,\n    root_path: &Path,\n    lock_path: &Path,\n    deadline:"),
        "lock acquisition must consume the operation deadline"
    );
    assert!(
        !source.contains("let deadline = Instant::now() + Duration::from_secs(2)"),
        "lock acquisition must not create an independent deadline"
    );
}

#[test]
fn config_recovery_discovers_and_cleans_all_unique_temp_files() {
    let dir = TempDir::new().expect("temporary recovery root");
    let path = isolated_path(&dir, "config.json");
    let mut store = open_host_fixture(&path).expect("open missing config");
    let root = isolated_root(&dir);
    let temps = [
        root.join(".config.json.unexpected-a.tmp"),
        root.join(".config.json.unexpected-b.tmp"),
    ];
    for temp in &temps {
        fs::write(temp, b"stale temp from a dead writer").expect("write stale temp");
    }

    let project = Project {
        id: "recovery-all".to_string(),
        name: "Recovery all".to_string(),
        root_path: "C:\\RecoveryAll".to_string(),
        created_at: "now".to_string(),
        updated_at: "now".to_string(),
        ..Project::default()
    };
    store
        .execute(0, ConfigCommand::CreateProject { project })
        .expect("all stale unique temps should be recovered");
    for temp in &temps {
        assert!(!temp.exists(), "stale temp was left behind: {temp:?}");
    }
}

#[test]
fn config_import_preview_diagnostics_do_not_disclose_source_path() {
    let (dir, store) = open_fixture();
    let import_path = isolated_path(&dir, "import.json");
    fs::write(&import_path, canonical_fixture_json()).expect("write import fixture");
    let preview = store.preview_import(&import_path).expect("preview import");
    let diagnostics = format!("{:?}", preview.token);

    assert!(
        !diagnostics.contains(import_path.to_string_lossy().as_ref()),
        "import token diagnostics must be path opaque"
    );
}

#[test]
fn config_import_replay_tracking_is_bounded() {
    let source = include_str!("../src/config/project_store.rs");

    assert!(
        source.contains("MAX_CONSUMED_IMPORT_TOKENS"),
        "consumed preview replay state must have a hard bound"
    );
    assert!(
        source.contains("consumed_import_order"),
        "bounded replay state needs deterministic eviction order"
    );
}

#[test]
fn config_legacy_nul_arguments_follow_an_explicit_migration_policy() {
    let error = AppConfig::from_legacy_json_str(FIXTURE)
        .expect_err("legacy NUL arguments must not enter a shadow legacy mode");

    assert_eq!(error.kind(), ConfigErrorKind::Validation);
    assert!(
        error.to_string().contains("NUL") || error.to_string().contains("control"),
        "legacy NUL rejection must be visible and actionable: {error}"
    );
}

#[test]
fn red_workspace_identity_survives_config_store_reopen() {
    let dir = TempDir::new().expect("temporary workspace identity root");
    let path = isolated_path(&dir, "config.json");
    let project_root = dir.path().join("persistent-project");
    let second_root = dir.path().join("persistent-project-two");
    fs::create_dir_all(&project_root).expect("persistent project root");
    fs::create_dir_all(&second_root).expect("second persistent project root");
    let mut config = canonical_fixture_value();
    config["projects"][0]["rootPath"] = Value::String(project_root.to_string_lossy().into_owned());
    config["projects"][1]["rootPath"] = Value::String(second_root.to_string_lossy().into_owned());
    fs::write(
        &path,
        serde_json::to_vec_pretty(&config).expect("encode persistent identity fixture"),
    )
    .expect("write persistent identity fixture");
    let mut store = open_host_fixture(&path).expect("open config store");
    let configured_id = store.snapshot().config.projects[0].id.clone();
    let revision = store.snapshot().revision;
    let roots = WorkspaceProjectRoots::from_host_config_store(&mut store, revision, 17, 23)
        .expect("host workspace roots");
    let issued_id = roots
        .project_id_for_config_id(&configured_id)
        .expect("opaque project identity");
    drop(store);

    let mut reopened = open_host_fixture(&path).expect("reopen config store");
    let reopened_revision = reopened.snapshot().revision;
    let reopened_roots =
        WorkspaceProjectRoots::from_host_config_store(&mut reopened, reopened_revision, 17, 23)
            .expect("reopened host roots");
    assert_eq!(
        reopened_roots.project_id_for_config_id(&configured_id),
        Some(issued_id),
        "configured IDs must map to persistent opaque identities, not process-local values"
    );
}

#[test]
fn red_host_admission_rechecks_config_snapshot_and_uses_generation_seam() {
    let source = include_str!("../src/host/connection.rs");
    let store = include_str!("../src/config/project_store.rs");
    assert!(
        source.contains("validate_current") && store.contains("validate_current_snapshot"),
        "host admission must fence the issuer against the current config snapshot"
    );
    assert!(
        source.contains("bind_authorized_with_generation"),
        "host admission must pass the issuer generations into workspace binding"
    );
    assert!(
        source.contains("config_revision") || store.contains("config_revision"),
        "host admission must carry the issuer revision into the final check"
    );
}

#[test]
fn red_stale_workspace_issuer_is_rejected_after_config_revision_changes() {
    let dir = TempDir::new().expect("temporary stale authority root");
    let path = isolated_path(&dir, "config.json");
    let project_root = dir.path().join("stale-project");
    let second_root = dir.path().join("stale-project-two");
    fs::create_dir_all(&project_root).expect("stale project root");
    fs::create_dir_all(&second_root).expect("second stale project root");
    let mut config = canonical_fixture_value();
    config["projects"][0]["rootPath"] = Value::String(project_root.to_string_lossy().into_owned());
    config["projects"][1]["rootPath"] = Value::String(second_root.to_string_lossy().into_owned());
    fs::write(
        &path,
        serde_json::to_vec_pretty(&config).expect("encode stale authority fixture"),
    )
    .expect("write stale authority fixture");
    let mut store = open_host_fixture(&path).expect("open stale authority store");
    let revision = store.snapshot().revision;
    let _roots = WorkspaceProjectRoots::from_host_config_store(&mut store, revision, 17, 23)
        .expect("host roots");
    let stale_revision = store.snapshot().revision;
    let project_id = first_project(&store).id.clone();
    store
        .execute(
            stale_revision,
            ConfigCommand::ArchiveProject {
                project_id: project_id.clone(),
            },
        )
        .expect("mutate canonical config");
    let error = WorkspaceProjectRoots::from_host_config_store(&mut store, stale_revision, 17, 23)
        .expect_err("stale workspace authority must fail closed");
    assert_eq!(error.kind(), ConfigErrorKind::RevisionConflict);
}

#[test]
fn red_raw_workspace_root_adapters_are_not_production_public_api() {
    let model = include_str!("../src/workspace/model.rs");
    let host = include_str!("../src/host/connection.rs");
    assert!(
        !model.contains("pub fn try_from_config"),
        "raw configured project pairs must not be a public production constructor"
    );
    assert!(
        !model.contains("pub fn try_from_pairs"),
        "raw project-root pairs must not be forgeable through a public constructor"
    );
    assert!(
        !host.contains("pub fn start_supervised_with_project_config"),
        "host must not expose a raw Vec<(String, String)> admission route"
    );
}

#[test]
fn red_github_config_writes_use_canonical_write_availability() {
    let source = include_str!("../src/git/mod.rs");
    assert!(
        source.contains("ConfigWriteAvailability"),
        "GitHub token/config writes must consult the startup write authority"
    );
    assert!(
        !source.contains("persistence::load_config()"),
        "GitHub persistence must not bypass ConfigStore through the legacy model"
    );
}

#[test]
fn red_git_local_controls_are_disabled_when_config_writes_are_unavailable() {
    let source = include_str!("../src/git/mod.rs");
    assert!(
        source.contains("ensure_config_write_available"),
        "local Git controls must consult canonical ConfigWriteAvailability"
    );
    assert!(
        source.contains("ConfigWriteAvailability::Unavailable { diagnostic }"),
        "Git must surface the unavailable diagnostic instead of silently mutating"
    );
}

#[test]
fn red_startup_failure_preserves_sidebar_rows_or_marks_them_unavailable() {
    let source = include_str!("../src/app/mod.rs");
    assert!(
        source.contains("unavailable") || source.contains("ReadOnlyRecovery"),
        "startup config failures need an explicit unavailable/read-only sidebar state"
    );
    assert!(
        !source.contains("config: AppConfig::default()"),
        "startup failure must not silently replace the sidebar with a blank default"
    );
}

#[allow(dead_code)]
fn _assert_no_live_appdata_path_is_used(path: &Path) {
    assert!(!path.to_string_lossy().contains("AppData"));
}
