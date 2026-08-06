//! Real foreground-host lifecycle acceptance.
//!
//! Every fixture uses a process-unique TempDir config base and named debug
//! profile. This target must never resolve or touch installed DevManager data.

#![cfg(windows)]

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command as ProcessCommand, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use tempfile::TempDir;
use tokio::time::{sleep, timeout};
use uuid::Uuid;

use devmanager::client::{
    connect, perform_client_hello, HostClient, HostClientConfig, TrackedOperation,
};
use devmanager::config::paths::{resolve_app_paths, AppProfile, BuildKind, ResolvedAppPaths};
use devmanager::domain::command::{Command, CommandEnvelope, CommandReceipt, CreateTaskIntent};
use devmanager::domain::id::{CommandId, EnvironmentId, ProjectId, RequestId, TaskId};
use devmanager::domain::operation::OperationState;
use devmanager::domain::query::{Query, QueryEnvelope, QueryError, QueryOutcome};
use devmanager::domain::snapshot::{SnapshotItem, SnapshotSection};
use devmanager::domain::task::{
    ReviewReadiness, TaskActivity, TaskAssignment, TaskAttention, TaskConnectivity, WorkspaceRef,
};
use devmanager::domain::ClientId;
use devmanager::host::{
    pipe_endpoint_for_named_profile, profile_fingerprint_for_named_profile, HostIdentity, IpcError,
};
use devmanager::protocol::{Capability, CapabilitySet, ClientHello, FrameLimits};

const READY_TIMEOUT: Duration = Duration::from_secs(15);
const CONNECT_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(2);
const TERMINATE_TIMEOUT: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(25);

fn host_exe() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_devmanager-host"))
}

fn unique_profile() -> String {
    format!("lifecycle{}{}", std::process::id(), Uuid::now_v7().simple())
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
    assert!(paths.database.starts_with(&paths.root));
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
        let child = command.spawn().expect("spawn foreground host");
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
            .map_err(|error| format!("kill exact host pid {pid}: {error}"))?;
        let started = Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(status)) => return Ok(status),
                Ok(None) if started.elapsed() < deadline => thread::sleep(POLL),
                Ok(None) => {
                    return Err(format!(
                        "exact host pid {pid} did not exit within {deadline:?}"
                    ))
                }
                Err(error) => return Err(format!("wait exact host pid {pid}: {error}")),
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
    command
        .arg("--foreground")
        .arg("--profile")
        .arg(profile)
        .arg("--instance-label")
        .arg("Lifecycle Test")
        .arg("--parent-pid")
        .arg(std::process::id().to_string())
        .arg("--config-base")
        .arg(config_base)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    command
}

async fn wait_for_identity(host: &mut ChildGuard, lock_path: &Path) -> HostIdentity {
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
        sleep(POLL).await;
    }
}

async fn connect_bounded(config: &HostClientConfig, host: &mut ChildGuard) -> HostClient {
    let started = Instant::now();
    loop {
        if let Some(status) = host.try_wait().expect("poll host while connecting") {
            let diagnostics = host.exited_diagnostics(status);
            panic!("foreground host exited before client attach: {diagnostics}");
        }
        match timeout(CONNECT_ATTEMPT_TIMEOUT, HostClient::connect(config.clone())).await {
            Ok(Ok(client)) => return client,
            Ok(Err(IpcError::Io(_) | IpcError::Unavailable | IpcError::Timeout)) | Err(_)
                if started.elapsed() < READY_TIMEOUT =>
            {
                sleep(POLL).await
            }
            Ok(Err(error)) => panic!("non-retryable client attach failure: {error}"),
            Err(_) => panic!("client attach attempt exceeded {CONNECT_ATTEMPT_TIMEOUT:?}"),
        }
    }
}

async fn reconnect_bounded(client: &mut HostClient, host: &mut ChildGuard) {
    let started = Instant::now();
    loop {
        if let Some(status) = host.try_wait().expect("poll host while reconnecting") {
            let diagnostics = host.exited_diagnostics(status);
            panic!("foreground host exited before client reconnect: {diagnostics}");
        }
        match timeout(CONNECT_ATTEMPT_TIMEOUT, client.reconnect()).await {
            Ok(Ok(())) => return,
            Ok(Err(IpcError::Io(_) | IpcError::Unavailable | IpcError::Timeout)) | Err(_)
                if started.elapsed() < READY_TIMEOUT =>
            {
                sleep(POLL).await
            }
            Ok(Err(error)) => panic!("non-retryable client reconnect failure: {error}"),
            Err(_) => panic!("client reconnect attempt exceeded {CONNECT_ATTEMPT_TIMEOUT:?}"),
        }
    }
}

fn fixed_uuid_v7(tail: u8) -> [u8; 16] {
    [
        0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        tail,
    ]
}

fn create_task_named(
    client_id: ClientId,
    command_tail: u8,
    task_tail: u8,
    environment_tail: u8,
    project_tail: u8,
    title: &str,
) -> (CommandEnvelope, CommandId, TaskId) {
    let command_id = CommandId::from_bytes(fixed_uuid_v7(command_tail)).expect("command id");
    let task_id = TaskId::from_bytes(fixed_uuid_v7(task_tail)).expect("task id");
    (
        CommandEnvelope {
            command_id,
            client_id,
            task_id: None,
            issued_at_ms: 1_725_000_000_100,
            expected_task_revision: None,
            command: Command::CreateTask(CreateTaskIntent {
                id: task_id,
                environment_id: EnvironmentId::from_bytes(fixed_uuid_v7(environment_tail))
                    .expect("environment id"),
                title: title.into(),
                description: None,
                project_id: ProjectId::from_bytes(fixed_uuid_v7(project_tail)).expect("project id"),
                workspace: WorkspaceRef::Main,
                assignment: TaskAssignment::LocalOwner,
                created_at_ms: 1_725_000_000_000,
                connectivity: TaskConnectivity::Connected,
                attention: TaskAttention::None,
                activity: TaskActivity::Idle,
                review_readiness: ReviewReadiness::NotReady,
            }),
        },
        command_id,
        task_id,
    )
}

fn create_task(client_id: ClientId) -> (CommandEnvelope, CommandId, TaskId) {
    create_task_named(
        client_id,
        0x71,
        0x72,
        0x73,
        0x74,
        "Foreground host reconnect",
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreground_host_retains_lock_and_bus_across_client_reconnect() {
    let config_base = TempDir::new().expect("process-unique config base");
    let profile = unique_profile();
    let paths = isolated_paths(&config_base, &profile);
    let lock_path = paths.root.join("host.lock");

    let mut host = ChildGuard::spawn(host_command(config_base.path(), &profile));
    let original_identity = wait_for_identity(&mut host, &lock_path).await;
    assert_eq!(original_identity.pid, host.id());
    assert_eq!(original_identity.profile, profile);

    let client_id = ClientId::from_bytes(fixed_uuid_v7(0x70)).expect("client id");
    let requested = CapabilitySet::from_capabilities([Capability::OperationSettlement]);
    let config = HostClientConfig {
        named_profile: profile.clone(),
        client_build: format!("devmanager/{}", env!("CARGO_PKG_VERSION")),
        client_id,
        requested,
        limits: FrameLimits::v1_default(),
    };
    let mut client = connect_bounded(&config, &mut host).await;
    let first_connection_id = client.connection_id();
    assert_eq!(client.client_id(), client_id);
    assert_eq!(client.host_boot_id(), original_identity.boot_id);
    assert_eq!(client.granted_capabilities(), requested);

    let (create, command_id, _task_id) = create_task(client_id);
    let receipt = client
        .execute_command(create)
        .await
        .expect("create task through foreground host");
    let operation_id = match receipt {
        CommandReceipt::Accepted {
            command_id: accepted,
            operation_id,
            ..
        } => {
            assert_eq!(accepted, command_id);
            operation_id
        }
        other => panic!("expected Accepted receipt, got {other:?}"),
    };
    assert!(matches!(
        client.tracked_operation(operation_id),
        Some(TrackedOperation::Pending { command_id: tracked }) if *tracked == command_id
    ));

    client.disconnect();
    assert!(!client.is_connected());
    assert!(
        host.try_wait().expect("poll host after detach").is_none(),
        "client detach must not stop the host"
    );
    let after_detach = read_identity(&lock_path).expect("identity after detach");
    assert_eq!(after_detach.pid, original_identity.pid);
    assert_eq!(after_detach.boot_id, original_identity.boot_id);

    let endpoint = pipe_endpoint_for_named_profile(&profile).expect("profile endpoint");
    let wrong_fingerprint = profile_fingerprint_for_named_profile(&unique_profile())
        .expect("different profile fingerprint");
    let wrong_hello = ClientHello::new(
        format!("devmanager/{}", env!("CARGO_PKG_VERSION")),
        ClientId::from_bytes(fixed_uuid_v7(0x75)).expect("rejected client id"),
        wrong_fingerprint,
        requested,
        FrameLimits::v1_default(),
    )
    .expect("well-formed hello for the wrong profile");
    let rejected = timeout(
        CONNECT_ATTEMPT_TIMEOUT,
        perform_client_hello(&endpoint, &wrong_hello),
    )
    .await
    .expect("wrong-profile handshake stayed bounded");
    assert!(rejected.is_err(), "wrong-profile handshake must fail");
    assert!(
        host.try_wait()
            .expect("poll host after rejected handshake")
            .is_none(),
        "rejected handshake must not stop the host"
    );
    let after_rejection = read_identity(&lock_path).expect("identity after rejected handshake");
    assert_eq!(after_rejection.pid, original_identity.pid);
    assert_eq!(after_rejection.boot_id, original_identity.boot_id);

    reconnect_bounded(&mut client, &mut host).await;
    assert_ne!(client.connection_id(), first_connection_id);
    assert_eq!(client.client_id(), client_id);
    assert_eq!(client.host_boot_id(), original_identity.boot_id);
    assert_eq!(client.granted_capabilities(), requested);
    assert!(matches!(
        client.tracked_operation(operation_id),
        Some(TrackedOperation::Pending { command_id: tracked }) if *tracked == command_id
    ));

    let state = client
        .refresh_operation(operation_id)
        .await
        .expect("refresh operation transport")
        .expect("known operation");
    assert!(matches!(state, OperationState::Settled { .. }));
    assert!(matches!(
        client.tracked_operation(operation_id),
        Some(TrackedOperation::Resolved {
            command_id: tracked,
            state: OperationState::Settled { .. },
        }) if *tracked == command_id
    ));

    let final_identity = read_identity(&lock_path).expect("final host identity");
    assert_eq!(final_identity.pid, original_identity.pid);
    assert_eq!(final_identity.boot_id, original_identity.boot_id);
    assert!(
        paths.database.exists(),
        "host must create isolated kernel.sqlite3"
    );
    assert!(paths.database.starts_with(config_base.path()));

    client.disconnect();
    let status = host
        .terminate_and_wait_bounded(TERMINATE_TIMEOUT)
        .expect("terminate exact foreground host");
    assert!(!status.success(), "test termination should stop the host");
    assert!(host.try_wait().expect("final host poll").is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_clients_attach_concurrently_and_share_one_command_bus() {
    let config_base = TempDir::new().expect("process-unique config base");
    let profile = unique_profile();
    let paths = isolated_paths(&config_base, &profile);
    let lock_path = paths.root.join("host.lock");

    let mut host = ChildGuard::spawn(host_command(config_base.path(), &profile));
    let original_identity = wait_for_identity(&mut host, &lock_path).await;

    let requested = CapabilitySet::from_capabilities([Capability::OperationSettlement]);
    let client_a_id = ClientId::from_bytes(fixed_uuid_v7(0x80)).expect("client A id");
    let client_a_config = HostClientConfig {
        named_profile: profile.clone(),
        client_build: format!("devmanager/{}", env!("CARGO_PKG_VERSION")),
        client_id: client_a_id,
        requested,
        limits: FrameLimits::v1_default(),
    };
    let mut client_a = connect_bounded(&client_a_config, &mut host).await;

    let client_b_id = ClientId::from_bytes(fixed_uuid_v7(0x81)).expect("client B id");
    let client_b_config = HostClientConfig {
        named_profile: profile.clone(),
        client_build: format!("devmanager/{}", env!("CARGO_PKG_VERSION")),
        client_id: client_b_id,
        requested,
        limits: FrameLimits::v1_default(),
    };
    let mut client_b = timeout(
        CONNECT_ATTEMPT_TIMEOUT,
        HostClient::connect(client_b_config),
    )
    .await
    .expect("second client attach must not wait for client A to disconnect")
    .expect("second client attach");

    assert_eq!(client_a.host_boot_id(), original_identity.boot_id);
    assert_eq!(client_b.host_boot_id(), original_identity.boot_id);
    assert_ne!(client_a.connection_id(), client_b.connection_id());

    let (create, _command_id, task_id) = create_task(client_a_id);
    let receipt = client_a
        .execute_command(create)
        .await
        .expect("client A creates through shared command bus");
    assert!(matches!(receipt, CommandReceipt::Accepted { .. }));

    let snapshot = client_b
        .task_snapshot(task_id)
        .await
        .expect("client B task query transport")
        .expect("client B sees client A task");
    assert_eq!(snapshot.task.id, task_id);
    assert_eq!(snapshot.task.title, "Foreground host reconnect");

    client_a.disconnect();
    let snapshot_while_a_is_detached = client_b
        .task_snapshot(task_id)
        .await
        .expect("client B stays usable after client A disconnects")
        .expect("client B still sees client A task");
    assert_eq!(snapshot_while_a_is_detached.task.id, task_id);

    reconnect_bounded(&mut client_a, &mut host).await;
    let snapshot_after_a_reconnects = client_a
        .task_snapshot(task_id)
        .await
        .expect("client A query after reconnect transport")
        .expect("client A sees shared task after reconnect");
    assert_eq!(snapshot_after_a_reconnects.task.id, task_id);

    let final_identity = read_identity(&lock_path).expect("final host identity");
    assert_eq!(final_identity.pid, original_identity.pid);
    assert_eq!(final_identity.boot_id, original_identity.boot_id);

    client_a.disconnect();
    client_b.disconnect();
    let status = host
        .terminate_and_wait_bounded(TERMINATE_TIMEOUT)
        .expect("terminate exact foreground host");
    assert!(!status.success(), "test termination should stop the host");
    assert!(host.try_wait().expect("final host poll").is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn paged_task_snapshot_is_immutable_tamper_evident_and_releasable() {
    let config_base = TempDir::new().expect("process-unique config base");
    let profile = unique_profile();
    let paths = isolated_paths(&config_base, &profile);
    let lock_path = paths.root.join("host.lock");

    let mut host = ChildGuard::spawn(host_command(config_base.path(), &profile));
    let original_identity = wait_for_identity(&mut host, &lock_path).await;

    let client_id = ClientId::from_bytes(fixed_uuid_v7(0x90)).expect("snapshot client id");
    let mut limits = FrameLimits::v1_default();
    limits.max_page_items = 1;
    let config = HostClientConfig {
        named_profile: profile.clone(),
        client_build: format!("devmanager/{}", env!("CARGO_PKG_VERSION")),
        client_id,
        requested: CapabilitySet::from_capabilities([Capability::PagedSnapshots]),
        limits,
    };
    let mut client = connect_bounded(&config, &mut host).await;
    assert!(client
        .granted_capabilities()
        .contains(Capability::PagedSnapshots));

    let (first_create, _, first_task_id) =
        create_task_named(client_id, 0x91, 0x92, 0x93, 0x94, "First paged task");
    let (second_create, _, second_task_id) =
        create_task_named(client_id, 0x95, 0x96, 0x97, 0x98, "Second paged task");
    assert!(matches!(
        client
            .execute_command(first_create)
            .await
            .expect("create first paged task"),
        CommandReceipt::Accepted { .. }
    ));
    assert!(matches!(
        client
            .execute_command(second_create)
            .await
            .expect("create second paged task"),
        CommandReceipt::Accepted { .. }
    ));

    let first_page = client
        .snapshot_page(SnapshotSection::Tasks, None, None)
        .await
        .expect("first snapshot page transport")
        .expect("first snapshot page query");
    assert_eq!(first_page.section, SnapshotSection::Tasks);
    assert_eq!(first_page.items.len(), 1);
    let SnapshotItem::Task(first_item) = &first_page.items[0] else {
        panic!("tasks page must contain only task items");
    };
    assert_eq!(first_item.task.id, first_task_id);
    let valid_cursor = first_page
        .next_cursor
        .clone()
        .expect("first page must continue");

    let mut tampered_cursor = valid_cursor.clone();
    tampered_cursor[0] ^= 0x01;
    let tampered = client
        .snapshot_page(
            SnapshotSection::Tasks,
            Some(first_page.snapshot_id),
            Some(tampered_cursor),
        )
        .await
        .expect("tampered cursor transport");
    assert_eq!(
        tampered,
        Err(devmanager::domain::query::QueryError::InvalidRequest)
    );

    let (third_create, _, third_task_id) =
        create_task_named(client_id, 0x99, 0x9a, 0x9b, 0x9c, "Post-snapshot task");
    assert!(matches!(
        client
            .execute_command(third_create)
            .await
            .expect("create task after snapshot pin"),
        CommandReceipt::Accepted { .. }
    ));

    let second_page = client
        .snapshot_page(
            SnapshotSection::Tasks,
            Some(first_page.snapshot_id),
            Some(valid_cursor.clone()),
        )
        .await
        .expect("second snapshot page transport")
        .expect("second snapshot page query");
    assert_eq!(second_page.snapshot_id, first_page.snapshot_id);
    assert_eq!(second_page.through_sequence, first_page.through_sequence);
    assert_eq!(second_page.items.len(), 1);
    let SnapshotItem::Task(second_item) = &second_page.items[0] else {
        panic!("tasks page must contain only task items");
    };
    assert_eq!(second_item.task.id, second_task_id);
    assert_ne!(second_item.task.id, third_task_id);
    assert!(second_page.next_cursor.is_none());

    client
        .release_snapshot(first_page.snapshot_id)
        .await
        .expect("release snapshot transport")
        .expect("release snapshot query");
    let released = client
        .snapshot_page(
            SnapshotSection::Tasks,
            Some(first_page.snapshot_id),
            Some(valid_cursor),
        )
        .await
        .expect("released snapshot lookup transport");
    assert_eq!(
        released,
        Err(devmanager::domain::query::QueryError::NotFound)
    );

    let final_identity = read_identity(&lock_path).expect("final host identity");
    assert_eq!(final_identity.pid, original_identity.pid);
    assert_eq!(final_identity.boot_id, original_identity.boot_id);

    client.disconnect();
    let status = host
        .terminate_and_wait_bounded(TERMINATE_TIMEOUT)
        .expect("terminate exact foreground host");
    assert!(!status.success(), "test termination should stop the host");
    assert!(host.try_wait().expect("final host poll").is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn durable_event_replay_is_ordered_frozen_tamper_evident_and_reconnectable() {
    let config_base = TempDir::new().expect("process-unique config base");
    let profile = unique_profile();
    let paths = isolated_paths(&config_base, &profile);
    let lock_path = paths.root.join("host.lock");

    let mut host = ChildGuard::spawn(host_command(config_base.path(), &profile));
    let original_identity = wait_for_identity(&mut host, &lock_path).await;

    let writer_id = ClientId::from_bytes(fixed_uuid_v7(0xa0)).expect("writer client id");
    let reader_id = ClientId::from_bytes(fixed_uuid_v7(0xa1)).expect("reader client id");
    let mut limits = FrameLimits::v1_default();
    limits.max_page_items = 1;
    let requested = CapabilitySet::from_capabilities([Capability::EventReplay]);
    let writer_config = HostClientConfig {
        named_profile: profile.clone(),
        client_build: format!("devmanager/{}", env!("CARGO_PKG_VERSION")),
        client_id: writer_id,
        requested,
        limits,
    };
    let reader_config = HostClientConfig {
        named_profile: profile.clone(),
        client_build: format!("devmanager/{}", env!("CARGO_PKG_VERSION")),
        client_id: reader_id,
        requested,
        limits,
    };
    let mut writer = connect_bounded(&writer_config, &mut host).await;
    let mut reader = connect_bounded(&reader_config, &mut host).await;
    assert!(writer
        .granted_capabilities()
        .contains(Capability::EventReplay));
    assert!(reader
        .granted_capabilities()
        .contains(Capability::EventReplay));

    let scoped_client_id = ClientId::from_bytes(fixed_uuid_v7(0xae)).expect("scoped client id");
    let scoped_hello = ClientHello::new(
        format!("devmanager/{}", env!("CARGO_PKG_VERSION")),
        scoped_client_id,
        profile_fingerprint_for_named_profile(&profile).expect("profile fingerprint"),
        requested,
        limits,
    )
    .expect("scoped replay hello");
    let endpoint = pipe_endpoint_for_named_profile(&profile).expect("profile endpoint");
    let mut scoped_connection = timeout(CONNECT_ATTEMPT_TIMEOUT, connect(&endpoint, &scoped_hello))
        .await
        .expect("scoped replay attach stayed bounded")
        .expect("scoped replay attach");
    let scoped_reply = scoped_connection
        .query(QueryEnvelope {
            request_id: RequestId::new(),
            client_id: scoped_client_id,
            task_id: Some(TaskId::new()),
            query: Query::OpenEventReplay { after_sequence: 0 },
        })
        .await
        .expect("scoped replay query transport");
    assert_eq!(
        scoped_reply.outcome,
        QueryOutcome::Err(QueryError::InvalidRequest),
        "global event replay must reject a silently ignored Task scope"
    );
    drop(scoped_connection);

    let (first_create, _, _) =
        create_task_named(writer_id, 0xa2, 0xa3, 0xa4, 0xa5, "First replay task");
    let (second_create, _, _) =
        create_task_named(writer_id, 0xa6, 0xa7, 0xa8, 0xa9, "Second replay task");
    assert!(matches!(
        writer
            .execute_command(first_create)
            .await
            .expect("create first replay task"),
        CommandReceipt::Accepted { .. }
    ));
    assert!(matches!(
        writer
            .execute_command(second_create)
            .await
            .expect("create second replay task"),
        CommandReceipt::Accepted { .. }
    ));

    let first = reader
        .open_event_replay(0)
        .await
        .expect("open event replay transport")
        .expect("open event replay query");
    assert_eq!(first.page.after_sequence, 0);
    assert_eq!(first.page.events.len(), 1);
    let replay_id = first.subscription_id;
    let frozen_through = first.page.through_sequence;
    let valid_cursor = first
        .page
        .next_cursor
        .clone()
        .expect("one-item first page must continue");

    let mut tampered_cursor = valid_cursor.clone();
    tampered_cursor[0] ^= 0x01;
    let tampered = reader
        .continue_event_replay(replay_id, tampered_cursor)
        .await
        .expect("tampered replay cursor transport");
    assert_eq!(
        tampered,
        Err(devmanager::domain::query::QueryError::InvalidRequest)
    );

    let foreign = writer
        .continue_event_replay(replay_id, valid_cursor.clone())
        .await
        .expect("foreign replay lookup transport");
    assert_eq!(
        foreign,
        Err(devmanager::domain::query::QueryError::Unauthorized)
    );

    let (post_open_create, _, _) =
        create_task_named(writer_id, 0xaa, 0xab, 0xac, 0xad, "Post-replay task");
    assert!(matches!(
        writer
            .execute_command(post_open_create)
            .await
            .expect("create task after replay pin"),
        CommandReceipt::Accepted { .. }
    ));

    reader.disconnect();
    let mut reduced_limits = limits;
    reduced_limits.max_page_encoded_bytes /= 2;
    let reduced_reader_config = HostClientConfig {
        named_profile: profile.clone(),
        client_build: format!("devmanager/{}", env!("CARGO_PKG_VERSION")),
        client_id: reader_id,
        requested,
        limits: reduced_limits,
    };
    let mut reduced_reader = connect_bounded(&reduced_reader_config, &mut host).await;
    let mismatched_limits = reduced_reader
        .continue_event_replay(replay_id, valid_cursor.clone())
        .await
        .expect("reduced-limit replay transport");
    assert_eq!(
        mismatched_limits,
        Err(QueryError::InvalidRequest),
        "a retained replay must not emit under different reconnect page limits"
    );
    reduced_reader.disconnect();
    reconnect_bounded(&mut reader, &mut host).await;

    let mut observed_sequences = vec![first.page.events[0].sequence];
    let mut cursor = Some(valid_cursor.clone());
    while let Some(resume_cursor) = cursor {
        let page = reader
            .continue_event_replay(replay_id, resume_cursor)
            .await
            .expect("resume event replay transport")
            .expect("resume event replay query");
        assert_eq!(page.subscription_id, replay_id);
        assert_eq!(page.page.through_sequence, frozen_through);
        assert_eq!(page.page.events.len(), 1);
        observed_sequences.push(page.page.events[0].sequence);
        cursor = page.page.next_cursor;
    }
    assert_eq!(
        observed_sequences.last().copied(),
        Some(frozen_through),
        "the pinned replay must end exactly at its original high-water mark"
    );
    assert!(
        observed_sequences.windows(2).all(|pair| pair[0] < pair[1]),
        "durable replay must be strictly ordered without duplicates"
    );
    assert!(
        observed_sequences
            .iter()
            .all(|sequence| *sequence <= frozen_through),
        "events committed after replay open must not enter the frozen range"
    );

    reader
        .release_event_replay(replay_id)
        .await
        .expect("idempotent replay release transport")
        .expect("idempotent replay release query");
    let released = reader
        .continue_event_replay(replay_id, valid_cursor)
        .await
        .expect("released replay lookup transport");
    assert_eq!(
        released,
        Err(devmanager::domain::query::QueryError::NotFound)
    );

    let final_identity = read_identity(&lock_path).expect("final host identity");
    assert_eq!(final_identity.pid, original_identity.pid);
    assert_eq!(final_identity.boot_id, original_identity.boot_id);

    writer.disconnect();
    reader.disconnect();
    let status = host
        .terminate_and_wait_bounded(TERMINATE_TIMEOUT)
        .expect("terminate exact foreground host");
    assert!(!status.success(), "test termination should stop the host");
    assert!(host.try_wait().expect("final host poll").is_some());
}
