//! Phase 2.4 CLI client acceptance: shared action catalog + host ctl.
//!
//! Every fixture uses a process-unique TempDir config base and a unique named
//! debug profile. Never resolve or touch installed DevManager APPDATA, and
//! never read or hash session.json.

#![cfg(windows)]

use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::{Child, Command as ProcessCommand, ExitStatus, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

use serde_json::Value;
use tempfile::TempDir;
use uuid::Uuid;

use devmanager::{
    client::action::{
    self, ActionRisk, ActionScope, ACTION_HOST_ACTIONS, ACTION_HOST_STATUS,
    ACTION_PROVIDER_ANSWER_QUESTION, ACTION_PROVIDER_NEW_CONVERSATION,
    ACTION_PROVIDER_QUEUE_FOLLOW_UP, ACTION_PROVIDER_RESOLVE_APPROVAL, ACTION_PROVIDER_SEND_NOW,
    ACTION_PROVIDER_STEER_CURRENT_TURN, ACTION_PROVIDER_STOP_TURN, ACTION_TASK_CREATE,
    ACTION_TASK_CREATE_V2, ACTION_TASK_LIST, ACTION_TASK_RENAME, ACTION_TASK_SHOW,
    },
    config::paths::{resolve_app_paths, AppProfile, BuildKind, ResolvedAppPaths},
    domain::{
        command::{Command, CommandEnvelope, CommandReceipt, CreateTaskRequestIntent},
        id::{CommandId, EnvironmentId, ProjectId, TaskId},
        task::{ReviewReadiness, TaskActivity, TaskAssignment, TaskAttention, TaskConnectivity},
        ClientId,
    },
    host::{HostIdentity, HostRequestExecutor},
    protocol::{
        Capability, CapabilitySet, ClientRequest, FrameLimits, NegotiatedParameters,
        ProtocolVersion, ServerMessage,
    },
    workspace::WorkspaceRequest,
};

const READY_TIMEOUT: Duration = Duration::from_secs(15);
const CTL_TIMEOUT: Duration = Duration::from_secs(10);
const TERMINATE_TIMEOUT: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(25);

fn host_exe() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_devmanager-host"))
}

fn unique_profile() -> String {
    format!("cli{}{}", std::process::id(), Uuid::now_v7().simple())
}

fn fixed_uuid_v7(tail: u8) -> [u8; 16] {
    [
        0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        tail,
    ]
}

fn seed_task_with_base(paths: &ResolvedAppPaths, base: u8, title: &str) -> TaskId {
    fs::create_dir_all(&paths.root).expect("create isolated profile root");
    let project_id = ProjectId::from_bytes(fixed_uuid_v7(base + 4)).expect("project id");
    let project_config = vec![(
        project_id.to_string(),
        paths.root.to_string_lossy().into_owned(),
    )];
    let client_id = ClientId::from_bytes(fixed_uuid_v7(base)).expect("seed client id");
    let task_id = TaskId::from_bytes(fixed_uuid_v7(base + 1)).expect("seed task id");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("seed host runtime");
    let receipt = runtime.block_on(async move {
        let bus = devmanager::kernel::CommandBus::open(&paths.database)
            .expect("open isolated seed command store");
        let (requests, executor) =
            HostRequestExecutor::start_supervised_with_project_config(bus, project_config)
                .expect("configured seed project roots");
        let response = requests
            .execute(
                NegotiatedParameters {
                    version: ProtocolVersion::current(),
                    client_id,
                    capabilities: CapabilitySet::empty(),
                    limits: FrameLimits::v1_default(),
                },
                ClientRequest::Command(CommandEnvelope {
                    command_id: CommandId::from_bytes(fixed_uuid_v7(base + 2))
                        .expect("seed command id"),
                    client_id,
                    task_id: None,
                    issued_at_ms: 1_725_000_000_100,
                    expected_task_revision: None,
                    command: Command::CreateTaskV2(CreateTaskRequestIntent {
                        id: task_id,
                        environment_id: EnvironmentId::from_bytes(fixed_uuid_v7(base + 3))
                            .expect("environment id"),
                        title: title.into(),
                        description: Some("Read through the host query boundary".into()),
                        project_id,
                        workspace: WorkspaceRequest::confirmed_external(&paths.root),
                        assignment: TaskAssignment::LocalOwner,
                        created_at_ms: 1_725_000_000_000,
                        connectivity: TaskConnectivity::Connected,
                        attention: TaskAttention::None,
                        activity: TaskActivity::Idle,
                        review_readiness: ReviewReadiness::NotReady,
                    }),
                }),
            )
            .await
            .expect("seed task command");
        drop(requests);
        let _ = executor.join.await.expect("seed host executor join");
        let ServerMessage::CommandReceipt(receipt) = response else {
            panic!("seed task must return a command receipt");
        };
        receipt
    });
    assert!(matches!(receipt, CommandReceipt::Accepted { .. }));
    task_id
}

fn seed_task(paths: &ResolvedAppPaths) -> TaskId {
    seed_task_with_base(paths, 0x60, "Task Show Target")
}

fn scrub_env(command: &mut ProcessCommand) {
    command
        .env_remove("DEVMANAGER_PROFILE")
        .env_remove("DEVMANAGER_CONFIG_DIR")
        .env_remove("DEVMANAGER_APP_IDENTITY");
}

fn isolated_paths(base: &TempDir, profile: &str) -> ResolvedAppPaths {
    let root = base.path();
    assert!(
        root.starts_with(std::env::temp_dir()),
        "fixture must stay beneath the process temp directory"
    );
    if let Ok(appdata) = std::env::var("APPDATA") {
        assert!(
            !root.starts_with(Path::new(&appdata)),
            "fixture must stay outside APPDATA"
        );
    }

    let paths = resolve_app_paths(
        root,
        AppProfile::named(profile).expect("valid named profile"),
        BuildKind::Debug,
    )
    .expect("resolve isolated debug paths");
    assert_eq!(paths.root.parent(), Some(root));
    paths
}

fn read_identity(path: &Path) -> Option<HostIdentity> {
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

struct ChildGuard {
    child: Option<Child>,
}

impl ChildGuard {
    fn spawn(mut command: ProcessCommand) -> Self {
        let child = command.spawn().expect("spawn child");
        Self { child: Some(child) }
    }

    fn id(&self) -> u32 {
        self.child.as_ref().expect("child still owned").id()
    }

    fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        self.child.as_mut().expect("child still owned").try_wait()
    }

    fn exited_diagnostics(&mut self, status: ExitStatus) -> String {
        let mut stderr = String::new();
        if let Some(mut pipe) = self
            .child
            .as_mut()
            .expect("child still owned")
            .stderr
            .take()
        {
            let _ = pipe.read_to_string(&mut stderr);
        }
        format!("{status}; stderr={stderr:?}")
    }

    fn terminate_and_wait_bounded(&mut self, deadline: Duration) -> Result<ExitStatus, String> {
        let child = self
            .child
            .as_mut()
            .ok_or_else(|| "child already taken".to_string())?;
        if let Some(status) = child
            .try_wait()
            .map_err(|error| format!("poll child before termination: {error}"))?
        {
            return Ok(status);
        }

        let pid = child.id();
        child
            .kill()
            .map_err(|error| format!("kill exact pid {pid}: {error}"))?;
        let started = Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(status)) => return Ok(status),
                Ok(None) if started.elapsed() < deadline => thread::sleep(POLL),
                Ok(None) => {
                    return Err(format!("exact pid {pid} did not exit within {deadline:?}"));
                }
                Err(error) => return Err(format!("wait exact pid {pid}: {error}")),
            }
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        let _ = child.kill();
        let started = Instant::now();
        while started.elapsed() < TERMINATE_TIMEOUT {
            match child.try_wait() {
                Ok(Some(_)) | Err(_) => return,
                Ok(None) => thread::sleep(POLL),
            }
        }
    }
}

fn host_command(config_base: &Path, profile: &str) -> ProcessCommand {
    let mut command = ProcessCommand::new(host_exe());
    scrub_env(&mut command);
    command
        .arg("--foreground")
        .arg("--profile")
        .arg(profile)
        .arg("--instance-label")
        .arg("CLI Client Test")
        .arg("--parent-pid")
        .arg(std::process::id().to_string())
        .arg("--config-base")
        .arg(config_base)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    command
}

fn ctl_command(args: &[&str]) -> ProcessCommand {
    let mut command = ProcessCommand::new(host_exe());
    scrub_env(&mut command);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

fn run_ctl_bounded(args: &[&str]) -> Output {
    let mut child = ChildGuard::spawn(ctl_command(args));
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                let owned = child.child.take().expect("child finished");
                return owned.wait_with_output().expect("collect ctl stdout/stderr");
            }
            Ok(None) if started.elapsed() < CTL_TIMEOUT => thread::sleep(POLL),
            Ok(None) => {
                let _ = child.terminate_and_wait_bounded(TERMINATE_TIMEOUT);
                panic!("ctl {:?} exceeded {CTL_TIMEOUT:?}", args);
            }
            Err(error) => panic!("poll ctl {:?}: {error}", args),
        }
    }
}

fn wait_for_identity(host: &mut ChildGuard, lock_path: &Path) -> HostIdentity {
    let started = Instant::now();
    loop {
        if let Some(status) = host.try_wait().expect("poll host while waiting for lock") {
            let diagnostics = host.exited_diagnostics(status);
            panic!("foreground host exited before lock readiness: {diagnostics}");
        }
        if let Some(identity) = read_identity(lock_path) {
            if identity.pid == host.id() {
                return identity;
            }
        }
        assert!(
            started.elapsed() < READY_TIMEOUT,
            "timed out waiting for host identity at {}",
            lock_path.display()
        );
        thread::sleep(POLL);
    }
}

#[test]
fn action_catalog_ids_are_unique_and_classified() {
    let catalog = action::catalog();
    assert_eq!(
        catalog.len(),
        action::registered_actions().count(),
        "catalog() is the single host action registry"
    );

    let mut ids = Vec::new();
    for action in catalog {
        assert!(!action.id.is_empty(), "action id must be nonempty");
        assert!(
            !ids.contains(&action.id),
            "duplicate stable action id: {}",
            action.id
        );
        ids.push(action.id);
        let expected_risk = if matches!(
            action.id,
            ACTION_TASK_CREATE
                | ACTION_TASK_CREATE_V2
                | ACTION_TASK_RENAME
                | ACTION_PROVIDER_SEND_NOW
                | ACTION_PROVIDER_STEER_CURRENT_TURN
                | ACTION_PROVIDER_QUEUE_FOLLOW_UP
                | ACTION_PROVIDER_ANSWER_QUESTION
                | ACTION_PROVIDER_RESOLVE_APPROVAL
                | ACTION_PROVIDER_STOP_TURN
                | ACTION_PROVIDER_NEW_CONVERSATION
        ) {
            ActionRisk::Mutating
        } else {
            ActionRisk::ReadOnly
        };
        assert_eq!(action.risk, expected_risk);
        let expected_scope = if matches!(
            action.id,
            ACTION_TASK_SHOW
                | ACTION_TASK_RENAME
                | ACTION_PROVIDER_SEND_NOW
                | ACTION_PROVIDER_STEER_CURRENT_TURN
                | ACTION_PROVIDER_QUEUE_FOLLOW_UP
                | ACTION_PROVIDER_ANSWER_QUESTION
                | ACTION_PROVIDER_RESOLVE_APPROVAL
                | ACTION_PROVIDER_STOP_TURN
                | ACTION_PROVIDER_NEW_CONVERSATION
        ) {
            ActionScope::Task
        } else {
            ActionScope::Host
        };
        assert_eq!(action.scope, expected_scope);
        assert!(!action.title.is_empty());
        assert!(!action.description.is_empty());
        assert!(!action.keywords.is_empty());
    }

    assert!(ids.contains(&ACTION_HOST_ACTIONS));
    assert!(ids.contains(&ACTION_HOST_STATUS));
    assert!(ids.contains(&ACTION_TASK_LIST));
    assert!(ids.contains(&ACTION_TASK_SHOW));
    assert!(!ids.contains(&ACTION_TASK_CREATE));
    assert!(ids.contains(&ACTION_TASK_CREATE_V2));
    assert!(ids.contains(&ACTION_TASK_RENAME));
    assert!(action::require_unique_ids().is_ok());
    let task_list = catalog
        .iter()
        .find(|action| action.id == ACTION_TASK_LIST)
        .expect("task.list action");
    assert_eq!(
        task_list.required_capability,
        Some(Capability::PagedSnapshots)
    );
}

#[test]
fn ctl_actions_json_is_stable_unique_and_offline() {
    let first = run_ctl_bounded(&["ctl", "actions", "--json"]);
    assert!(
        first.status.success(),
        "ctl actions --json must succeed offline; status={}; stderr={}",
        first.status,
        String::from_utf8_lossy(&first.stderr)
    );
    let second = run_ctl_bounded(&["ctl", "actions", "--json"]);
    assert!(second.status.success());
    assert_eq!(
        first.stdout, second.stdout,
        "actions JSON must be byte-stable across invocations"
    );

    let doc: Value = serde_json::from_slice(&first.stdout).expect("actions JSON");
    assert_eq!(doc["schema_version"], 1);
    let actions = doc["actions"].as_array().expect("actions array");
    let mut ids = Vec::new();
    for action in actions {
        let id = action["id"].as_str().expect("action id string");
        assert!(!ids.contains(&id.to_string()), "duplicate id in JSON: {id}");
        ids.push(id.to_string());
        let expected_risk = if matches!(
            id,
            ACTION_TASK_CREATE
                | ACTION_TASK_CREATE_V2
                | ACTION_TASK_RENAME
                | ACTION_PROVIDER_SEND_NOW
                | ACTION_PROVIDER_STEER_CURRENT_TURN
                | ACTION_PROVIDER_QUEUE_FOLLOW_UP
                | ACTION_PROVIDER_ANSWER_QUESTION
                | ACTION_PROVIDER_RESOLVE_APPROVAL
                | ACTION_PROVIDER_STOP_TURN
                | ACTION_PROVIDER_NEW_CONVERSATION
        ) {
            "mutating"
        } else {
            "read_only"
        };
        assert_eq!(action["risk"], expected_risk);
        let expected_scope = if matches!(
            id,
            ACTION_TASK_SHOW
                | ACTION_TASK_RENAME
                | ACTION_PROVIDER_SEND_NOW
                | ACTION_PROVIDER_STEER_CURRENT_TURN
                | ACTION_PROVIDER_QUEUE_FOLLOW_UP
                | ACTION_PROVIDER_ANSWER_QUESTION
                | ACTION_PROVIDER_RESOLVE_APPROVAL
                | ACTION_PROVIDER_STOP_TURN
                | ACTION_PROVIDER_NEW_CONVERSATION
        ) {
            "task"
        } else {
            "host"
        };
        assert_eq!(action["scope"], expected_scope);
        assert!(action["title"].as_str().unwrap().len() > 0);
        assert!(action["description"].as_str().unwrap().len() > 0);
        assert!(action["keywords"].as_array().unwrap().len() > 0);
        assert!(action.get("required_capability").is_some());
        let schema = action["argument_schema"]
            .as_object()
            .expect("each action exposes an argument schema");
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["additionalProperties"], false);
        if id == ACTION_TASK_CREATE_V2 {
            let required = schema["required"]
                .as_array()
                .expect("create required fields");
            let fields = vec![
                "task_id",
                "environment_id",
                "title",
                "project_id",
                "workspace",
            ];
            for field in fields {
                assert!(
                    required.iter().any(|value| value == field),
                    "task.create schema must require {field}"
                );
            }
            assert!(schema["properties"].get("project_root").is_none());
        }
        if id == ACTION_TASK_RENAME {
            let required = schema["required"]
                .as_array()
                .expect("rename required fields");
            for field in ["task_id", "title"] {
                assert!(
                    required.iter().any(|value| value == field),
                    "task.rename schema must require {field}"
                );
            }
        }
    }
    assert!(ids.iter().any(|id| id == ACTION_HOST_ACTIONS));
    assert!(ids.iter().any(|id| id == ACTION_HOST_STATUS));
    assert!(ids.iter().any(|id| id == ACTION_TASK_LIST));
    assert!(ids.iter().any(|id| id == ACTION_TASK_SHOW));
    assert!(!ids.iter().any(|id| id == ACTION_TASK_CREATE));
    assert!(ids.iter().any(|id| id == ACTION_TASK_CREATE_V2));
    assert!(ids.iter().any(|id| id == ACTION_TASK_RENAME));
    assert!(
        String::from_utf8_lossy(&first.stderr).trim().is_empty(),
        "successful actions JSON must not emit failure diagnostics on stderr"
    );
}

#[test]
fn ctl_status_attaches_without_taking_host_lock() {
    let config_base = TempDir::new().expect("process-unique config base");
    let profile = unique_profile();
    let paths = isolated_paths(&config_base, &profile);
    let lock_path = paths.root.join("host.lock");

    let mut host = ChildGuard::spawn(host_command(config_base.path(), &profile));
    let original = wait_for_identity(&mut host, &lock_path);

    let output = run_ctl_bounded(&["ctl", "status", "--profile", &profile, "--json"]);
    assert!(
        output.status.success(),
        "ctl status must attach; status={}; stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    let after = read_identity(&lock_path).expect("host lock after ctl status");
    assert_eq!(after.pid, original.pid);
    assert_eq!(after.boot_id, original.boot_id);
    assert_eq!(after.profile, original.profile);
    assert!(
        host.try_wait().expect("poll host after ctl").is_none(),
        "ctl status must leave the foreground host running"
    );

    let doc: Value = serde_json::from_slice(&output.stdout).expect("status JSON");
    assert_eq!(doc["schema_version"], 1);
    assert_eq!(doc["action_id"], ACTION_HOST_STATUS);
    assert_eq!(doc["profile"], profile);
    assert_eq!(doc["host_boot_id"], original.boot_id.to_string());
    assert!(doc["connection_id"].as_str().unwrap().len() > 0);
    assert!(doc.get("granted_capabilities").is_some());
    assert!(
        doc["server_build"]
            .as_str()
            .unwrap_or("")
            .starts_with("devmanager-host/"),
        "server_build should come from ServerHello"
    );
    assert_eq!(doc["protocol_major"], 1);
    assert!(doc["protocol_minor"].as_u64().is_some());

    let status = host
        .terminate_and_wait_bounded(TERMINATE_TIMEOUT)
        .expect("terminate exact foreground host");
    assert!(!status.success(), "test termination should stop the host");
    assert!(host.try_wait().expect("final host poll").is_some());
}

#[test]
fn ctl_status_exits_nonzero_when_host_absent() {
    let profile = unique_profile();
    let output = run_ctl_bounded(&["ctl", "status", "--profile", &profile, "--json"]);
    assert!(
        !output.status.success(),
        "status without a host must fail nonzero"
    );
    assert!(
        output.stdout.is_empty(),
        "failure must not emit a JSON success document"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.trim().is_empty(),
        "failure must emit bounded diagnostics on stderr"
    );
    assert!(
        stderr.len() < 8 * 1024,
        "stderr diagnostics must stay bounded"
    );
}

#[test]
fn ctl_task_show_queries_seeded_task_without_taking_host_lock() {
    let config_base = TempDir::new().expect("process-unique config base");
    let profile = unique_profile();
    let paths = isolated_paths(&config_base, &profile);
    let task_id = seed_task(&paths);
    let lock_path = paths.root.join("host.lock");

    let mut host = ChildGuard::spawn(host_command(config_base.path(), &profile));
    let original = wait_for_identity(&mut host, &lock_path);

    let output = run_ctl_bounded(&[
        "ctl",
        "task-show",
        "--profile",
        &profile,
        "--task-id",
        &task_id.to_string(),
        "--json",
    ]);
    assert!(
        output.status.success(),
        "ctl task-show must succeed; status={}; stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let doc: Value = serde_json::from_slice(&output.stdout).expect("task-show JSON");
    assert_eq!(doc["schema_version"], 1);
    assert_eq!(doc["action_id"], ACTION_TASK_SHOW);
    assert_eq!(doc["profile"], profile);
    assert_eq!(doc["task_id"], task_id.to_string());
    assert_eq!(doc["snapshot"]["task"]["id"], task_id.to_string());
    assert_eq!(doc["snapshot"]["task"]["title"], "Task Show Target");

    let after = read_identity(&lock_path).expect("host lock after task-show");
    assert_eq!(after.pid, original.pid);
    assert_eq!(after.boot_id, original.boot_id);
    assert!(host
        .try_wait()
        .expect("poll host after task-show")
        .is_none());

    let missing_id = TaskId::from_bytes(fixed_uuid_v7(0x69)).expect("missing task id");
    let missing = run_ctl_bounded(&[
        "ctl",
        "task-show",
        "--profile",
        &profile,
        "--task-id",
        &missing_id.to_string(),
        "--json",
    ]);
    assert!(!missing.status.success());
    assert!(missing.stdout.is_empty());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("not found"));
    let after_missing = read_identity(&lock_path).expect("host lock after missing task query");
    assert_eq!(after_missing.pid, original.pid);
    assert_eq!(after_missing.boot_id, original.boot_id);
    assert!(host
        .try_wait()
        .expect("poll host after missing task query")
        .is_none());

    let status = host
        .terminate_and_wait_bounded(TERMINATE_TIMEOUT)
        .expect("terminate exact foreground host");
    assert!(!status.success(), "test termination should stop the host");
    assert!(host.try_wait().expect("final host poll").is_some());
}

#[test]
fn ctl_tasks_lists_seeded_tasks_through_paged_snapshot_without_taking_host_lock() {
    let config_base = TempDir::new().expect("process-unique config base");
    let profile = unique_profile();
    let paths = isolated_paths(&config_base, &profile);
    let first_task_id = seed_task_with_base(&paths, 0x60, "First Listed Task");
    let second_task_id = seed_task_with_base(&paths, 0x65, "Second Listed Task");
    let lock_path = paths.root.join("host.lock");

    let mut host = ChildGuard::spawn(host_command(config_base.path(), &profile));
    let original = wait_for_identity(&mut host, &lock_path);

    let output = run_ctl_bounded(&["ctl", "tasks", "--profile", &profile, "--json"]);
    assert!(
        output.status.success(),
        "ctl tasks must succeed; status={}; stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).trim().is_empty(),
        "successful task list must not emit diagnostics"
    );

    let doc: Value = serde_json::from_slice(&output.stdout).expect("tasks JSON");
    assert_eq!(doc["schema_version"], 1);
    assert_eq!(doc["action_id"], ACTION_TASK_LIST);
    assert_eq!(doc["profile"], profile);
    assert!(doc["snapshot_id"].as_str().is_some());
    assert!(doc["through_sequence"].as_u64().unwrap_or_default() >= 2);
    assert!(doc["page_count"].as_u64().unwrap_or_default() >= 1);
    let tasks = doc["tasks"].as_array().expect("tasks array");
    assert_eq!(tasks.len(), 2);
    assert_eq!(tasks[0]["task"]["id"], first_task_id.to_string());
    assert_eq!(tasks[0]["task"]["title"], "First Listed Task");
    assert_eq!(tasks[1]["task"]["id"], second_task_id.to_string());
    assert_eq!(tasks[1]["task"]["title"], "Second Listed Task");

    let after = read_identity(&lock_path).expect("host lock after ctl tasks");
    assert_eq!(after.pid, original.pid);
    assert_eq!(after.boot_id, original.boot_id);
    assert!(host
        .try_wait()
        .expect("poll host after ctl tasks")
        .is_none());

    let status = host
        .terminate_and_wait_bounded(TERMINATE_TIMEOUT)
        .expect("terminate exact foreground host");
    assert!(!status.success(), "test termination should stop the host");
    assert!(host.try_wait().expect("final host poll").is_some());
}

#[test]
fn ctl_invoke_task_create_uses_shared_mutation_without_taking_host_lock() {
    let config_base = TempDir::new().expect("process-unique config base");
    let profile = unique_profile();
    let paths = isolated_paths(&config_base, &profile);
    let configured_root = config_base.path().join("configured-project-root");
    let caller_external_path = config_base.path().join("caller-external-path");
    fs::create_dir(&paths.root).expect("create isolated profile root");
    fs::create_dir(&configured_root).expect("create configured project root");
    fs::create_dir(&caller_external_path).expect("create caller external path");
    let lock_path = paths.root.join("host.lock");

    let task_id = TaskId::new();
    let project_id = ProjectId::new();
    fs::write(
        &paths.config,
        serde_json::json!({
            "projects": [{
                "id": project_id.to_string(),
                "rootPath": configured_root
            }]
        })
        .to_string(),
    )
    .expect("host-owned project config");

    let mut host = ChildGuard::spawn(host_command(config_base.path(), &profile));
    let original = wait_for_identity(&mut host, &lock_path);

    let arguments = serde_json::json!({
        "task_id": task_id,
        "environment_id": EnvironmentId::new(),
        "title": "CLI Created Task",
        "description": "Created through task.create",
        "project_id": project_id,
        "workspace": {
            "choice": "external",
            "path": caller_external_path,
            "branch": null,
            "external_confirmed": true
        }
    })
    .to_string();
    let output = run_ctl_bounded(&[
        "ctl",
        "invoke",
        "--profile",
        &profile,
        "--action",
        ACTION_TASK_CREATE_V2,
        "--arguments-json",
        &arguments,
        "--json",
    ]);
    assert!(
        output.status.success(),
        "ctl invoke task.create must succeed; status={}; stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let doc: Value = serde_json::from_slice(&output.stdout).expect("invoke JSON");
    assert_eq!(doc["schema_version"], 1);
    assert_eq!(doc["action_id"], ACTION_TASK_CREATE_V2);
    assert_eq!(doc["profile"], profile);
    assert_eq!(doc["task_id"], task_id.to_string());
    assert_eq!(doc["receipt"]["accepted"]["task_revision"], 1);
    assert!(doc["receipt"]["accepted"]["command_id"].as_str().is_some());
    assert!(doc["receipt"]["accepted"]["operation_id"]
        .as_str()
        .is_some());

    let after = read_identity(&lock_path).expect("host lock after task.create");
    assert_eq!(after.pid, original.pid);
    assert_eq!(after.boot_id, original.boot_id);
    assert!(host.try_wait().expect("poll host after invoke").is_none());

    let shown = run_ctl_bounded(&[
        "ctl",
        "task-show",
        "--profile",
        &profile,
        "--task-id",
        &task_id.to_string(),
        "--json",
    ]);
    assert!(shown.status.success(), "created task must be queryable");
    let shown_doc: Value = serde_json::from_slice(&shown.stdout).expect("task-show JSON");
    assert_eq!(shown_doc["snapshot"]["task"]["title"], "CLI Created Task");
    let workspace = &shown_doc["snapshot"]["task"]["workspace"];
    let external_binding = &workspace["external_bound"]["binding"];
    assert_eq!(
        external_binding["kind"], "external",
        "durable workspace must come from the host-issued external authority"
    );
    assert!(
        external_binding["workspace_root"]["workspace_id"]
            .as_str()
            .is_some_and(|value| !value.is_empty()),
        "durable workspace must expose only an opaque workspace id"
    );
    let workspace_json = serde_json::to_string(workspace).expect("workspace JSON");
    assert!(
        !workspace_json.contains(configured_root.to_string_lossy().as_ref()),
        "configured host path must remain outside the client projection"
    );
    assert!(
        !workspace_json.contains(caller_external_path.to_string_lossy().as_ref()),
        "caller-supplied path must remain outside the client projection"
    );

    let duplicate = run_ctl_bounded(&[
        "ctl",
        "invoke",
        "--profile",
        &profile,
        "--action",
        ACTION_TASK_CREATE_V2,
        "--arguments-json",
        &arguments,
        "--json",
    ]);
    assert!(
        !duplicate.status.success(),
        "a new command for the same task id must reject"
    );
    assert!(duplicate.stdout.is_empty());
    assert!(String::from_utf8_lossy(&duplicate.stderr).contains("already_exists"));
    assert!(host
        .try_wait()
        .expect("poll host after rejection")
        .is_none());

    let status = host
        .terminate_and_wait_bounded(TERMINATE_TIMEOUT)
        .expect("terminate exact foreground host");
    assert!(!status.success(), "test termination should stop the host");
    assert!(host.try_wait().expect("final host poll").is_some());
}

#[test]
fn ctl_invoke_task_rename_requires_current_revision_without_taking_host_lock() {
    let config_base = TempDir::new().expect("process-unique config base");
    let profile = unique_profile();
    let paths = isolated_paths(&config_base, &profile);
    let task_id = seed_task(&paths);
    let lock_path = paths.root.join("host.lock");

    let mut host = ChildGuard::spawn(host_command(config_base.path(), &profile));
    let original = wait_for_identity(&mut host, &lock_path);
    let arguments = serde_json::json!({
        "task_id": task_id,
        "title": "Renamed Through CLI"
    })
    .to_string();

    let output = run_ctl_bounded(&[
        "ctl",
        "invoke",
        "--profile",
        &profile,
        "--action",
        ACTION_TASK_RENAME,
        "--arguments-json",
        &arguments,
        "--expected-task-revision",
        "1",
        "--json",
    ]);
    assert!(
        output.status.success(),
        "ctl invoke task.rename must succeed; status={}; stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let doc: Value = serde_json::from_slice(&output.stdout).expect("rename JSON");
    assert_eq!(doc["schema_version"], 1);
    assert_eq!(doc["action_id"], ACTION_TASK_RENAME);
    assert_eq!(doc["profile"], profile);
    assert_eq!(doc["task_id"], task_id.to_string());
    assert_eq!(doc["receipt"]["accepted"]["task_revision"], 2);

    let shown = run_ctl_bounded(&[
        "ctl",
        "task-show",
        "--profile",
        &profile,
        "--task-id",
        &task_id.to_string(),
        "--json",
    ]);
    assert!(shown.status.success(), "renamed task must remain queryable");
    let shown_doc: Value = serde_json::from_slice(&shown.stdout).expect("task-show JSON");
    assert_eq!(
        shown_doc["snapshot"]["task"]["title"],
        "Renamed Through CLI"
    );
    assert_eq!(shown_doc["snapshot"]["task"]["revision"], 2);

    let stale = run_ctl_bounded(&[
        "ctl",
        "invoke",
        "--profile",
        &profile,
        "--action",
        ACTION_TASK_RENAME,
        "--arguments-json",
        &arguments,
        "--expected-task-revision",
        "1",
        "--json",
    ]);
    assert!(!stale.status.success(), "stale rename must reject");
    assert!(stale.stdout.is_empty());
    assert!(String::from_utf8_lossy(&stale.stderr).contains("revision_conflict"));

    let after = read_identity(&lock_path).expect("host lock after task.rename");
    assert_eq!(after.pid, original.pid);
    assert_eq!(after.boot_id, original.boot_id);
    assert!(host.try_wait().expect("poll host after rename").is_none());

    let status = host
        .terminate_and_wait_bounded(TERMINATE_TIMEOUT)
        .expect("terminate exact foreground host");
    assert!(!status.success(), "test termination should stop the host");
    assert!(host.try_wait().expect("final host poll").is_some());
}

#[test]
fn ctl_rejects_unknown_commands_and_invalid_profiles() {
    let unknown = run_ctl_bounded(&["ctl", "not-a-command", "--json"]);
    assert!(!unknown.status.success());
    assert!(unknown.stdout.is_empty());
    assert!(!String::from_utf8_lossy(&unknown.stderr).trim().is_empty());

    let missing_json = run_ctl_bounded(&["ctl", "actions"]);
    assert!(!missing_json.status.success());
    assert!(missing_json.stdout.is_empty());

    let production = run_ctl_bounded(&["ctl", "status", "--profile", "production", "--json"]);
    assert!(!production.status.success());
    assert!(production.stdout.is_empty());
    let production_err = String::from_utf8_lossy(&production.stderr);
    assert!(
        production_err.to_ascii_lowercase().contains("production")
            || production_err.to_ascii_lowercase().contains("profile"),
        "invalid production profile must be rejected with a profile diagnostic"
    );

    let empty = run_ctl_bounded(&["ctl", "status", "--profile", "", "--json"]);
    assert!(!empty.status.success());
    assert!(empty.stdout.is_empty());

    let duplicate = run_ctl_bounded(&[
        "ctl",
        "status",
        "--profile",
        "dupprofile",
        "--profile",
        "other",
        "--json",
    ]);
    assert!(!duplicate.status.success());
    assert!(duplicate.stdout.is_empty());

    let invalid_task = run_ctl_bounded(&[
        "ctl",
        "task-show",
        "--profile",
        "validprofile",
        "--task-id",
        "not-a-uuid",
        "--json",
    ]);
    assert!(!invalid_task.status.success());
    assert!(invalid_task.stdout.is_empty());

    let unknown_action = run_ctl_bounded(&[
        "ctl",
        "invoke",
        "--profile",
        "validprofile",
        "--action",
        "task.not-real",
        "--arguments-json",
        "{}",
        "--json",
    ]);
    assert!(!unknown_action.status.success());
    assert!(unknown_action.stdout.is_empty());
    assert!(String::from_utf8_lossy(&unknown_action.stderr).contains("unsupported action id"));

    let unknown_field = run_ctl_bounded(&[
        "ctl",
        "invoke",
        "--profile",
        "validprofile",
        "--action",
        ACTION_TASK_CREATE_V2,
        "--arguments-json",
        r#"{"unknown":true}"#,
        "--json",
    ]);
    assert!(!unknown_field.status.success());
    assert!(unknown_field.stdout.is_empty());
    assert!(String::from_utf8_lossy(&unknown_field.stderr).contains("unknown field"));

    let create_revision = run_ctl_bounded(&[
        "ctl",
        "invoke",
        "--profile",
        "validprofile",
        "--action",
        ACTION_TASK_CREATE_V2,
        "--arguments-json",
        "{}",
        "--expected-task-revision",
        "0",
        "--json",
    ]);
    assert!(!create_revision.status.success());
    assert!(create_revision.stdout.is_empty());
    assert!(String::from_utf8_lossy(&create_revision.stderr)
        .contains("requires expected-task-revision"));

    let rename_without_revision = run_ctl_bounded(&[
        "ctl",
        "invoke",
        "--profile",
        "validprofile",
        "--action",
        ACTION_TASK_RENAME,
        "--arguments-json",
        "{}",
        "--json",
    ]);
    assert!(!rename_without_revision.status.success());
    assert!(rename_without_revision.stdout.is_empty());
    assert!(String::from_utf8_lossy(&rename_without_revision.stderr)
        .contains("requires expected-task-revision"));

    let junk = run_ctl_bounded(&["ctl", "actions", "--json", "--extra"]);
    assert!(!junk.status.success());
    assert!(junk.stdout.is_empty());

    let oversized = format!("--{}", "x".repeat(16 * 1024));
    let bounded = run_ctl_bounded(&["ctl", "actions", "--json", &oversized]);
    assert!(!bounded.status.success());
    assert!(bounded.stdout.is_empty());
    assert!(
        bounded.stderr.len() < 2 * 1024,
        "all CLI diagnostics must be bounded"
    );
}
