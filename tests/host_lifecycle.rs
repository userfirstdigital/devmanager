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
    connect, perform_client_hello, ClientSubscription, ClientSubscriptionState, HostClient,
    HostClientConfig, SubscriptionUpdate, TrackedOperation, UnsolicitedServerMessage,
};
use devmanager::config::paths::{resolve_app_paths, AppProfile, BuildKind, ResolvedAppPaths};
use devmanager::domain::command::{Command, CommandEnvelope, CommandReceipt, CreateTaskIntent};
use devmanager::domain::id::{
    ArtifactId, CommandId, EnvironmentId, OperationId, ProjectId, RequestId, TaskId,
};
use devmanager::domain::operation::OperationState;
use devmanager::domain::query::{Query, QueryEnvelope, QueryError, QueryOutcome};
use devmanager::domain::snapshot::{SnapshotItem, SnapshotSection};
use devmanager::domain::task::{
    ReviewReadiness, TaskActivity, TaskAssignment, TaskAttention, TaskConnectivity, WorkspaceRef,
};
use devmanager::domain::{ArtifactContentRef, ArtifactFacts, ArtifactKind, ClientId, PrivacyClass};
use devmanager::host::{
    pipe_endpoint_for_named_profile, profile_fingerprint_for_named_profile, HostIdentity, IpcError,
};
use devmanager::protocol::{Capability, CapabilitySet, ClientHello, FrameLimits};
use sha2::{Digest, Sha256};

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

fn host_command_with_slow_durable_reader(
    config_base: &Path,
    profile: &str,
    slow_client_id: ClientId,
) -> ProcessCommand {
    let mut command = host_command(config_base, profile);
    command
        .arg("--test-slow-durable-reader-client-id")
        .arg(slow_client_id.to_string());
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn durable_event_replay_transitions_to_live_without_gap_or_duplicate() {
    let config_base = TempDir::new().expect("process-unique config base");
    let profile = unique_profile();
    let paths = isolated_paths(&config_base, &profile);
    let lock_path = paths.root.join("host.lock");

    let mut host = ChildGuard::spawn(host_command(config_base.path(), &profile));
    let original_identity = wait_for_identity(&mut host, &lock_path).await;

    let writer_id = ClientId::from_bytes(fixed_uuid_v7(0xb0)).expect("writer client id");
    let reader_id = ClientId::from_bytes(fixed_uuid_v7(0xb1)).expect("reader client id");
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

    let (first_create, _, _) =
        create_task_named(writer_id, 0xb2, 0xb3, 0xb4, 0xb5, "Live tail first task");
    let (second_create, _, _) =
        create_task_named(writer_id, 0xb6, 0xb7, 0xb8, 0xb9, "Live tail second task");
    assert!(matches!(
        writer
            .execute_command(first_create)
            .await
            .expect("create first live-tail task"),
        CommandReceipt::Accepted { .. }
    ));
    assert!(matches!(
        writer
            .execute_command(second_create)
            .await
            .expect("create second live-tail task"),
        CommandReceipt::Accepted { .. }
    ));

    let first = reader
        .open_event_replay(0)
        .await
        .expect("open event replay transport")
        .expect("open event replay query");
    let subscription_id = first.subscription_id;
    let frozen_through = first.page.through_sequence;
    let mut observed = first
        .page
        .events
        .iter()
        .map(|event| event.sequence)
        .collect::<Vec<_>>();
    let mut cursor = first.page.next_cursor.clone();
    assert!(
        cursor.is_some(),
        "one-item pages must leave frozen replay incomplete"
    );

    let (post_freeze_create, post_command_id, _) = create_task_named(
        writer_id,
        0xba,
        0xbb,
        0xbc,
        0xbd,
        "After open before frozen complete",
    );
    let post_receipt = writer
        .execute_command(post_freeze_create.clone())
        .await
        .expect("create task after open before frozen completion");
    assert!(
        matches!(post_receipt, CommandReceipt::Accepted { .. }),
        "expected accepted receipt after open, got {post_receipt:?}"
    );

    let probe = writer
        .open_event_replay(frozen_through)
        .await
        .expect("probe replay transport")
        .expect("probe replay query");
    let post_through = probe.page.through_sequence;
    assert!(
        post_through > frozen_through,
        "post-open command must advance durable high-water beyond frozen through"
    );
    writer
        .release_event_replay(probe.subscription_id)
        .await
        .expect("release probe transport")
        .expect("release probe query");

    while let Some(resume_cursor) = cursor {
        let page = reader
            .continue_event_replay(subscription_id, resume_cursor)
            .await
            .expect("continue frozen replay transport")
            .expect("continue frozen replay query");
        assert_eq!(page.subscription_id, subscription_id);
        assert_eq!(page.page.through_sequence, frozen_through);
        assert!(
            page.page
                .events
                .iter()
                .all(|event| event.sequence <= frozen_through),
            "frozen pagination must not include post-freeze sequences"
        );
        observed.extend(page.page.events.iter().map(|event| event.sequence));
        cursor = page.page.next_cursor;
    }
    assert_eq!(observed.last().copied(), Some(frozen_through));
    assert!(observed.windows(2).all(|pair| pair[0] < pair[1]));

    let expected_live = (frozen_through + 1)..=post_through;
    let expected_count = usize::try_from(post_through - frozen_through).expect("range fits");
    let mut live_sequences = Vec::with_capacity(expected_count);
    while live_sequences.len() < expected_count {
        let live = timeout(READY_TIMEOUT, reader.recv_unsolicited())
            .await
            .expect("live durable event stayed bounded")
            .expect("live durable event transport");
        let UnsolicitedServerMessage::DurableEvent {
            subscription_id: live_sub,
            event: live_event,
        } = live
        else {
            panic!("expected live DurableEvent, got {live:?}");
        };
        assert_eq!(live_sub, subscription_id);
        assert!(
            expected_live.contains(&live_event.sequence),
            "live sequence {} outside {}..={}",
            live_event.sequence,
            frozen_through + 1,
            post_through
        );
        assert!(
            live_sequences
                .last()
                .is_none_or(|previous| *previous < live_event.sequence),
            "live durable events must be strictly ordered without duplicates"
        );
        live_sequences.push(live_event.sequence);
    }
    assert_eq!(
        live_sequences,
        expected_live.collect::<Vec<_>>(),
        "live delivery must be exactly frozen_through+1..=post_through once"
    );

    let retry = writer
        .execute_command(post_freeze_create)
        .await
        .expect("exact command retry transport");
    assert!(
        matches!(
            retry,
            CommandReceipt::Accepted {
                command_id,
                ..
            } if command_id == post_command_id
        ),
        "exact retry must remain accepted for the same command id"
    );
    let duplicate = timeout(Duration::from_millis(300), reader.recv_unsolicited()).await;
    assert!(
        duplicate.is_err(),
        "exact command retry must not redeliver already-admitted live sequences"
    );

    reader
        .release_event_replay(subscription_id)
        .await
        .expect("release live subscription transport")
        .expect("release live subscription query");

    let (after_release_create, _, _) =
        create_task_named(writer_id, 0xbe, 0xbf, 0xc0, 0xc1, "After release");
    assert!(matches!(
        writer
            .execute_command(after_release_create)
            .await
            .expect("create task after release"),
        CommandReceipt::Accepted { .. }
    ));
    let post_release = timeout(Duration::from_millis(300), reader.recv_unsolicited()).await;
    assert!(
        post_release.is_err(),
        "release must stop later live durable delivery"
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pinned_snapshot_retains_id_across_section_restart_without_cursor() {
    let config_base = TempDir::new().expect("process-unique config base");
    let profile = unique_profile();
    let paths = isolated_paths(&config_base, &profile);
    let lock_path = paths.root.join("host.lock");

    let mut host = ChildGuard::spawn(host_command(config_base.path(), &profile));
    let original_identity = wait_for_identity(&mut host, &lock_path).await;

    let client_id = ClientId::from_bytes(fixed_uuid_v7(0xd0)).expect("retain client id");
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

    let (first_create, _, first_task_id) =
        create_task_named(client_id, 0xd1, 0xd2, 0xd3, 0xd4, "Retain first");
    let (second_create, _, second_task_id) =
        create_task_named(client_id, 0xd5, 0xd6, 0xd7, 0xd8, "Retain second");
    assert!(matches!(
        client
            .execute_command(first_create)
            .await
            .expect("create first retain task"),
        CommandReceipt::Accepted { .. }
    ));
    assert!(matches!(
        client
            .execute_command(second_create)
            .await
            .expect("create second retain task"),
        CommandReceipt::Accepted { .. }
    ));

    let first_page = client
        .snapshot_page(SnapshotSection::Tasks, None, None)
        .await
        .expect("open retained snapshot transport")
        .expect("open retained snapshot query");
    assert_eq!(first_page.items.len(), 1);
    let valid_cursor = first_page
        .next_cursor
        .clone()
        .expect("tasks page must continue under max_page_items=1");

    let second_page = client
        .snapshot_page(
            SnapshotSection::Tasks,
            Some(first_page.snapshot_id),
            Some(valid_cursor),
        )
        .await
        .expect("finish tasks section transport")
        .expect("finish tasks section query");
    assert_eq!(second_page.snapshot_id, first_page.snapshot_id);
    assert_eq!(second_page.through_sequence, first_page.through_sequence);
    assert!(second_page.next_cursor.is_none());
    let SnapshotItem::Task(second_item) = &second_page.items[0] else {
        panic!("tasks page must contain only task items");
    };
    assert_eq!(second_item.task.id, second_task_id);
    assert_ne!(second_item.task.id, first_task_id);

    let operations_page = client
        .snapshot_page(
            SnapshotSection::Operations,
            Some(first_page.snapshot_id),
            None,
        )
        .await
        .expect("begin operations section without cursor transport")
        .expect("begin operations section without cursor query");
    assert_eq!(operations_page.snapshot_id, first_page.snapshot_id);
    assert_eq!(
        operations_page.through_sequence,
        first_page.through_sequence
    );
    assert_eq!(operations_page.section, SnapshotSection::Operations);
    assert!(!operations_page.items.is_empty());

    client
        .release_snapshot(first_page.snapshot_id)
        .await
        .expect("explicit snapshot release transport")
        .expect("explicit snapshot release query");
    let released = client
        .snapshot_page(
            SnapshotSection::Operations,
            Some(first_page.snapshot_id),
            None,
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
async fn two_clients_assemble_same_initial_model_and_converge_live() {
    let config_base = TempDir::new().expect("process-unique config base");
    let profile = unique_profile();
    let paths = isolated_paths(&config_base, &profile);
    let lock_path = paths.root.join("host.lock");

    let mut host = ChildGuard::spawn(host_command(config_base.path(), &profile));
    let original_identity = wait_for_identity(&mut host, &lock_path).await;

    let writer_id = ClientId::from_bytes(fixed_uuid_v7(0xe0)).expect("writer client id");
    let reader_a_id = ClientId::from_bytes(fixed_uuid_v7(0xe1)).expect("reader a id");
    let reader_b_id = ClientId::from_bytes(fixed_uuid_v7(0xe2)).expect("reader b id");
    let mut limits = FrameLimits::v1_default();
    limits.max_page_items = 1;
    let requested =
        CapabilitySet::from_capabilities([Capability::PagedSnapshots, Capability::EventReplay]);
    let writer_config = HostClientConfig {
        named_profile: profile.clone(),
        client_build: format!("devmanager/{}", env!("CARGO_PKG_VERSION")),
        client_id: writer_id,
        requested,
        limits,
    };
    let reader_a_config = HostClientConfig {
        named_profile: profile.clone(),
        client_build: format!("devmanager/{}", env!("CARGO_PKG_VERSION")),
        client_id: reader_a_id,
        requested,
        limits,
    };
    let reader_b_config = HostClientConfig {
        named_profile: profile.clone(),
        client_build: format!("devmanager/{}", env!("CARGO_PKG_VERSION")),
        client_id: reader_b_id,
        requested,
        limits,
    };
    let mut writer = connect_bounded(&writer_config, &mut host).await;
    let mut reader_a = connect_bounded(&reader_a_config, &mut host).await;
    let mut reader_b = connect_bounded(&reader_b_config, &mut host).await;

    for (idx, title) in ["Seed one", "Seed two", "Seed three"]
        .into_iter()
        .enumerate()
    {
        let tail = 0xe3 + (idx as u8) * 4;
        let (create, _, _) =
            create_task_named(writer_id, tail, tail + 1, tail + 2, tail + 3, title);
        assert!(matches!(
            writer.execute_command(create).await.expect("seed create"),
            CommandReceipt::Accepted { .. }
        ));
    }

    let mut sub_a = ClientSubscription::new();
    let mut sub_b = ClientSubscription::new();
    sub_a
        .synchronize(&mut reader_a)
        .await
        .expect("reader a initial synchronize");
    sub_b
        .synchronize(&mut reader_b)
        .await
        .expect("reader b initial synchronize");
    assert_eq!(sub_a.state(), ClientSubscriptionState::Ready);
    assert_eq!(sub_b.state(), ClientSubscriptionState::Ready);
    let model_a = sub_a.model().expect("reader a model").clone();
    let model_b = sub_b.model().expect("reader b model").clone();
    assert_eq!(model_a, model_b);
    assert_eq!(
        model_a.last_applied_sequence(),
        model_b.last_applied_sequence()
    );
    assert_eq!(model_a.tasks().len(), 3);
    assert_eq!(model_a.operations().len(), 3);
    let sync_sequence = model_a.last_applied_sequence();

    let (live_create, live_command_id, live_task_id) =
        create_task_named(writer_id, 0xf0, 0xf1, 0xf2, 0xf3, "Live converge task");
    assert!(matches!(
        writer
            .execute_command(live_create.clone())
            .await
            .expect("create live converge task"),
        CommandReceipt::Accepted { .. }
    ));

    let probe = writer
        .open_event_replay(sync_sequence)
        .await
        .expect("probe high-water transport")
        .expect("probe high-water query");
    let high_water = probe.page.through_sequence;
    assert!(
        high_water > sync_sequence,
        "live create must advance durable high-water"
    );
    writer
        .release_event_replay(probe.subscription_id)
        .await
        .expect("release probe transport")
        .expect("release probe query");

    async fn drain_to_high_water(
        sub: &mut ClientSubscription,
        client: &HostClient,
        high_water: u64,
    ) {
        while sub
            .model()
            .expect("model while draining")
            .last_applied_sequence()
            < high_water
        {
            let update = timeout(READY_TIMEOUT, sub.recv_and_apply(client))
                .await
                .expect("live apply stayed bounded")
                .expect("live apply");
            match update {
                devmanager::client::SubscriptionUpdate::DurableEvent(event) => {
                    assert!(event.sequence <= high_water);
                }
                other => panic!("expected durable event while draining, got {other:?}"),
            }
        }
    }

    drain_to_high_water(&mut sub_a, &reader_a, high_water).await;
    drain_to_high_water(&mut sub_b, &reader_b, high_water).await;

    let converged_a = sub_a.model().expect("reader a after live").clone();
    let converged_b = sub_b.model().expect("reader b after live").clone();
    assert_eq!(converged_a, converged_b);
    assert_eq!(converged_a.last_applied_sequence(), high_water);
    assert!(converged_a.tasks().contains_key(&live_task_id));
    assert_eq!(converged_a.tasks().len(), 4);
    assert_eq!(converged_a.operations().len(), 4);

    let retry = writer
        .execute_command(live_create)
        .await
        .expect("exact live command retry");
    assert!(
        matches!(
            retry,
            CommandReceipt::Accepted {
                command_id,
                ..
            } if command_id == live_command_id
        ),
        "exact retry must remain accepted"
    );
    let dup_a = timeout(Duration::from_millis(300), sub_a.recv_and_apply(&reader_a)).await;
    let dup_b = timeout(Duration::from_millis(300), sub_b.recv_and_apply(&reader_b)).await;
    assert!(dup_a.is_err(), "reader a must not receive a duplicate");
    assert!(dup_b.is_err(), "reader b must not receive a duplicate");

    sub_a
        .release(&mut reader_a)
        .await
        .expect("release reader a subscription");
    sub_b
        .release(&mut reader_b)
        .await
        .expect("release reader b subscription");
    assert_eq!(sub_a.state(), ClientSubscriptionState::Released);
    assert_eq!(sub_b.state(), ClientSubscriptionState::Released);

    let final_identity = read_identity(&lock_path).expect("final host identity");
    assert_eq!(final_identity.pid, original_identity.pid);
    assert_eq!(final_identity.boot_id, original_identity.boot_id);

    writer.disconnect();
    reader_a.disconnect();
    reader_b.disconnect();
    let status = host
        .terminate_and_wait_bounded(TERMINATE_TIMEOUT)
        .expect("terminate exact foreground host");
    assert!(!status.success(), "test termination should stop the host");
    assert!(host.try_wait().expect("final host poll").is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn artifact_content_pages_are_scoped_resumable_and_side_effect_free() {
    let config_base = TempDir::new().expect("process-unique config base");
    let profile = unique_profile();
    let paths = isolated_paths(&config_base, &profile);
    let lock_path = paths.root.join("host.lock");

    let mut host = ChildGuard::spawn(host_command(config_base.path(), &profile));
    let original_identity = wait_for_identity(&mut host, &lock_path).await;

    let owner_id = ClientId::from_bytes(fixed_uuid_v7(0xa0)).expect("owner client id");
    let foreign_id = ClientId::from_bytes(fixed_uuid_v7(0xa1)).expect("foreign client id");
    let mut limits = FrameLimits::v1_default();
    // Force multi-page artifact content under a tight page budget.
    limits.max_page_encoded_bytes = 1_024;
    let requested = CapabilitySet::from_capabilities([
        Capability::PagedSnapshots,
        Capability::ChunkResume,
        Capability::EventReplay,
    ]);
    let owner_config = HostClientConfig {
        named_profile: profile.clone(),
        client_build: format!("devmanager/{}", env!("CARGO_PKG_VERSION")),
        client_id: owner_id,
        requested,
        limits,
    };
    let foreign_config = HostClientConfig {
        named_profile: profile.clone(),
        client_build: format!("devmanager/{}", env!("CARGO_PKG_VERSION")),
        client_id: foreign_id,
        requested,
        limits,
    };
    let mut owner = connect_bounded(&owner_config, &mut host).await;
    let mut foreign = connect_bounded(&foreign_config, &mut host).await;
    assert!(owner
        .granted_capabilities()
        .contains(Capability::ChunkResume));
    assert!(foreign
        .granted_capabilities()
        .contains(Capability::ChunkResume));

    let (create, _, task_id) =
        create_task_named(owner_id, 0xa2, 0xa3, 0xa4, 0xa5, "Artifact content task");
    assert!(matches!(
        owner
            .execute_command(create)
            .await
            .expect("create task for artifact content"),
        CommandReceipt::Accepted { .. }
    ));

    let distinctive = "ARTIFACT_CONTENT_BODY_TOKEN_9c2e";
    let body = format!(
        "{}{}",
        distinctive,
        "αβγδεζηθικλμνξοπρστυφχψω"
            .repeat(80)
            .chars()
            .cycle()
            .take(2_000)
            .collect::<String>()
    );
    let sha256: [u8; 32] = Sha256::digest(body.as_bytes()).into();
    let artifact_id = ArtifactId::from_bytes(fixed_uuid_v7(0xa6)).expect("artifact id");
    let register = CommandEnvelope {
        command_id: CommandId::from_bytes(fixed_uuid_v7(0xa7)).expect("register command id"),
        client_id: owner_id,
        task_id: Some(task_id),
        issued_at_ms: 1_725_000_000_200,
        expected_task_revision: Some(1),
        command: Command::RegisterArtifact {
            artifact: ArtifactFacts {
                id: artifact_id,
                task_id,
                kind: ArtifactKind::Evidence,
                label: "Paged evidence".into(),
                content_ref: ArtifactContentRef::inline_utf8(&body).expect("inline body"),
                sha256,
                privacy_class: PrivacyClass::LocalOnly,
                created_at_ms: 1_725_000_000_200,
            },
        },
    };
    assert!(matches!(
        owner
            .execute_command(register)
            .await
            .expect("register multi-page artifact"),
        CommandReceipt::Accepted { .. }
    ));

    for client in [&mut owner, &mut foreign] {
        let page = client
            .snapshot_page(SnapshotSection::Artifacts, None, None)
            .await
            .expect("artifacts snapshot transport")
            .expect("artifacts snapshot query");
        assert_eq!(page.section, SnapshotSection::Artifacts);
        assert_eq!(page.items.len(), 1);
        let SnapshotItem::Artifact(summary) = &page.items[0] else {
            panic!("artifacts page must contain ArtifactSummary items");
        };
        assert_eq!(summary.id, artifact_id);
        assert_eq!(summary.sha256, sha256);
        let encoded = rmp_serde::to_vec_named(&page).expect("encode snapshot page");
        assert!(
            !encoded
                .windows(distinctive.as_bytes().len())
                .any(|window| window == distinctive.as_bytes()),
            "snapshot page encoding must omit artifact body token"
        );
        client
            .release_snapshot(page.snapshot_id)
            .await
            .expect("release artifacts snapshot transport")
            .expect("release artifacts snapshot");
    }

    let baseline = owner
        .open_event_replay(0)
        .await
        .expect("baseline replay transport")
        .expect("baseline replay query");
    let baseline_through = baseline.page.through_sequence;
    let baseline_events = baseline.page.events.len();
    owner
        .release_event_replay(baseline.subscription_id)
        .await
        .expect("release baseline replay transport")
        .expect("release baseline replay");

    let open = owner
        .open_artifact_content(task_id, artifact_id)
        .await
        .expect("open artifact content transport")
        .expect("open artifact content query");
    assert_eq!(open.page.artifact_id, artifact_id);
    assert_eq!(open.page.sha256, sha256);
    assert_eq!(open.page.offset, 0);
    let subscription_id = open.subscription_id;
    let mut reconstructed = open.page.payload;
    let mut next = open.page.next_cursor;
    let mut pages = 1usize;
    while let Some(cursor) = next {
        let continued = owner
            .continue_artifact_content(task_id, subscription_id, cursor)
            .await
            .expect("continue artifact content transport")
            .expect("continue artifact content query");
        assert_eq!(continued.subscription_id, subscription_id);
        assert_eq!(continued.page.artifact_id, artifact_id);
        reconstructed.extend_from_slice(&continued.page.payload);
        next = continued.page.next_cursor;
        pages += 1;
    }
    assert!(
        pages > 1,
        "tight page budget must require multiple content pages"
    );
    assert_eq!(reconstructed, body.as_bytes());
    let reconstructed_digest: [u8; 32] = Sha256::digest(&reconstructed).into();
    assert_eq!(reconstructed_digest, sha256);

    let foreign_continue = foreign
        .continue_artifact_content(task_id, subscription_id, vec![0x01, 0x02])
        .await
        .expect("foreign continue transport");
    assert_eq!(foreign_continue, Err(QueryError::Unauthorized));

    let wrong_task = TaskId::from_bytes(fixed_uuid_v7(0xa8)).expect("wrong task id");
    let wrong_task_continue = owner
        .continue_artifact_content(wrong_task, subscription_id, vec![0x01, 0x02])
        .await
        .expect("wrong-task continue transport");
    assert_eq!(wrong_task_continue, Err(QueryError::Unauthorized));

    owner
        .release_artifact_content(task_id, subscription_id)
        .await
        .expect("release artifact content transport")
        .expect("release artifact content");
    owner
        .release_artifact_content(task_id, subscription_id)
        .await
        .expect("idempotent release transport")
        .expect("idempotent release");

    let after = owner
        .open_event_replay(0)
        .await
        .expect("post-read replay transport")
        .expect("post-read replay query");
    assert_eq!(after.page.through_sequence, baseline_through);
    assert_eq!(after.page.events.len(), baseline_events);
    owner
        .release_event_replay(after.subscription_id)
        .await
        .expect("release post-read replay transport")
        .expect("release post-read replay");

    // Correction 4: V1-oversized body must return InvalidRequest without poisoning.
    let oversized_body = "O".repeat(6_000);
    let oversized_sha: [u8; 32] = Sha256::digest(oversized_body.as_bytes()).into();
    let oversized_id = ArtifactId::from_bytes(fixed_uuid_v7(0xa9)).expect("oversized artifact id");
    let register_oversized = CommandEnvelope {
        command_id: CommandId::from_bytes(fixed_uuid_v7(0xaa)).expect("oversized command id"),
        client_id: owner_id,
        task_id: Some(task_id),
        issued_at_ms: 1_725_000_000_300,
        expected_task_revision: Some(2),
        command: Command::RegisterArtifact {
            artifact: ArtifactFacts {
                id: oversized_id,
                task_id,
                kind: ArtifactKind::Evidence,
                label: "Oversized evidence".into(),
                content_ref: ArtifactContentRef::inline_utf8(&oversized_body).expect("body"),
                sha256: oversized_sha,
                privacy_class: PrivacyClass::LocalOnly,
                created_at_ms: 1_725_000_000_300,
            },
        },
    };
    assert!(matches!(
        owner
            .execute_command(register_oversized)
            .await
            .expect("register oversized artifact"),
        CommandReceipt::Accepted { .. }
    ));

    let tight_id = ClientId::from_bytes(fixed_uuid_v7(0xab)).expect("tight client id");
    let mut tight_limits = FrameLimits::v1_default();
    tight_limits.max_page_encoded_bytes = 1_024;
    tight_limits.max_reassembled_message_bytes = 4_096;
    let tight_config = HostClientConfig {
        named_profile: profile.clone(),
        client_build: format!("devmanager/{}", env!("CARGO_PKG_VERSION")),
        client_id: tight_id,
        requested: CapabilitySet::from_capabilities([
            Capability::PagedSnapshots,
            Capability::ChunkResume,
            Capability::EventReplay,
        ]),
        limits: tight_limits,
    };
    let mut tight = connect_bounded(&tight_config, &mut host).await;
    assert!(
        tight
            .granted_capabilities()
            .contains(Capability::ChunkResume),
        "tight client must still receive ChunkResume"
    );
    let oversized_open = tight
        .open_artifact_content(task_id, oversized_id)
        .await
        .expect("oversized open transport");
    assert_eq!(
        oversized_open,
        Err(QueryError::InvalidRequest),
        "BodyTooLarge must map to InvalidRequest, not poison transport"
    );
    let still_alive = tight
        .snapshot_page(SnapshotSection::Artifacts, None, None)
        .await
        .expect("post-InvalidRequest snapshot transport")
        .expect("post-InvalidRequest snapshot query");
    assert_eq!(still_alive.section, SnapshotSection::Artifacts);
    assert!(still_alive.items.len() >= 2);
    tight
        .release_snapshot(still_alive.snapshot_id)
        .await
        .expect("release tight snapshot transport")
        .expect("release tight snapshot");
    tight.disconnect();

    let final_identity = read_identity(&lock_path).expect("final host identity");
    assert_eq!(final_identity.pid, original_identity.pid);
    assert_eq!(final_identity.boot_id, original_identity.boot_id);

    owner.disconnect();
    foreign.disconnect();
    let status = host
        .terminate_and_wait_bounded(TERMINATE_TIMEOUT)
        .expect("terminate exact foreground host");
    assert!(!status.success(), "test termination should stop the host");
    assert!(host.try_wait().expect("final host poll").is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn artifact_content_retry_same_cursor_after_connection_replacement_is_byte_exact() {
    let config_base = TempDir::new().expect("process-unique config base");
    let profile = unique_profile();
    let paths = isolated_paths(&config_base, &profile);
    let lock_path = paths.root.join("host.lock");

    let mut host = ChildGuard::spawn(host_command(config_base.path(), &profile));
    let original_identity = wait_for_identity(&mut host, &lock_path).await;

    let client_id = ClientId::from_bytes(fixed_uuid_v7(0xc0)).expect("artifact retry client id");
    let mut limits = FrameLimits::v1_default();
    // Force at least four content pages under a tight negotiated page budget.
    limits.max_page_encoded_bytes = 1_024;
    let requested = CapabilitySet::from_capabilities([
        Capability::PagedSnapshots,
        Capability::ChunkResume,
        Capability::EventReplay,
        Capability::ExplicitDetach,
    ]);
    let config = HostClientConfig {
        named_profile: profile.clone(),
        client_build: format!("devmanager/{}", env!("CARGO_PKG_VERSION")),
        client_id,
        requested,
        limits,
    };
    let mut client = connect_bounded(&config, &mut host).await;
    assert!(client
        .granted_capabilities()
        .contains(Capability::ChunkResume));
    assert!(
        client
            .granted_capabilities()
            .contains(Capability::ExplicitDetach),
        "lifecycle detach proof requires negotiated ExplicitDetach"
    );
    let first_connection_id = client.connection_id();
    assert_eq!(client.host_boot_id(), original_identity.boot_id);

    let (create, _, task_id) = create_task_named(
        client_id,
        0xc1,
        0xc2,
        0xc3,
        0xc4,
        "Artifact content retry task",
    );
    assert!(matches!(
        client
            .execute_command(create)
            .await
            .expect("create task for artifact content retry"),
        CommandReceipt::Accepted { .. }
    ));

    let body = format!(
        "ARTIFACT_CONTENT_RETRY_TOKEN_7f3a{}",
        "αβγδεζηθικλμνξοπρστυφχψω"
            .repeat(120)
            .chars()
            .cycle()
            .take(3_200)
            .collect::<String>()
    );
    let sha256: [u8; 32] = Sha256::digest(body.as_bytes()).into();
    let artifact_id = ArtifactId::from_bytes(fixed_uuid_v7(0xc5)).expect("artifact id");
    let register = CommandEnvelope {
        command_id: CommandId::from_bytes(fixed_uuid_v7(0xc6)).expect("register command id"),
        client_id,
        task_id: Some(task_id),
        issued_at_ms: 1_725_000_000_200,
        expected_task_revision: Some(1),
        command: Command::RegisterArtifact {
            artifact: ArtifactFacts {
                id: artifact_id,
                task_id,
                kind: ArtifactKind::Evidence,
                label: "Retryable paged evidence".into(),
                content_ref: ArtifactContentRef::inline_utf8(&body).expect("inline body"),
                sha256,
                privacy_class: PrivacyClass::LocalOnly,
                created_at_ms: 1_725_000_000_200,
            },
        },
    };
    assert!(matches!(
        client
            .execute_command(register)
            .await
            .expect("register multi-page artifact for retry"),
        CommandReceipt::Accepted { .. }
    ));

    let open = client
        .open_artifact_content(task_id, artifact_id)
        .await
        .expect("open artifact content transport")
        .expect("open artifact content query");
    assert_eq!(open.page.artifact_id, artifact_id);
    assert_eq!(open.page.sha256, sha256);
    assert_eq!(open.page.offset, 0);
    assert_eq!(open.page.total_bytes, body.len() as u64);
    let subscription_id = open.subscription_id;
    let cursor_c0 = open
        .page
        .next_cursor
        .clone()
        .expect("first page must leave a continuation cursor");

    let page_p1 = client
        .continue_artifact_content(task_id, subscription_id, cursor_c0.clone())
        .await
        .expect("first continue with C0 transport")
        .expect("first continue with C0 query");
    assert_eq!(page_p1.subscription_id, subscription_id);
    assert_eq!(page_p1.page.artifact_id, artifact_id);
    assert_ne!(
        page_p1.page.offset, 0,
        "continuation after open must not silently reopen from offset zero"
    );
    // Deliberately keep C0 as the last committed client cursor (do not advance).

    let detached_id = client
        .detach()
        .await
        .expect("host-acknowledged detach must complete before replacement");
    assert_eq!(
        detached_id, first_connection_id,
        "detach ack must name the interrupted connection"
    );
    assert!(
        !client.is_connected(),
        "local connection must drop only after detach ack"
    );
    let after_detach = read_identity(&lock_path).expect("host identity after detach");
    assert_eq!(after_detach.pid, original_identity.pid);
    assert_eq!(after_detach.boot_id, original_identity.boot_id);

    let mut replacement = connect_bounded(&config, &mut host).await;
    assert_eq!(replacement.client_id(), client_id);
    assert_ne!(
        replacement.connection_id(),
        first_connection_id,
        "replacement connection must be distinct from the interrupted one"
    );
    assert_eq!(replacement.host_boot_id(), original_identity.boot_id);
    let mid_identity = read_identity(&lock_path).expect("host identity after replacement");
    assert_eq!(mid_identity.pid, original_identity.pid);
    assert_eq!(mid_identity.boot_id, original_identity.boot_id);

    let retried_p1 = replacement
        .continue_artifact_content(task_id, subscription_id, cursor_c0.clone())
        .await
        .expect("retry C0 after connection replacement transport")
        .expect("retry C0 after connection replacement query");
    assert_eq!(
        retried_p1, page_p1,
        "retried C0 must equal the original P1 at the typed-value level"
    );
    assert_eq!(retried_p1.subscription_id, subscription_id);
    assert_eq!(retried_p1.page.artifact_id, artifact_id);
    assert_eq!(retried_p1.page.offset, page_p1.page.offset);
    assert_eq!(retried_p1.page.total_bytes, page_p1.page.total_bytes);
    assert_eq!(retried_p1.page.sha256, page_p1.page.sha256);
    assert_eq!(retried_p1.page.payload, page_p1.page.payload);
    assert_eq!(retried_p1.page.encoded_bytes, page_p1.page.encoded_bytes);
    assert_eq!(retried_p1.page.next_cursor, page_p1.page.next_cursor);
    assert_ne!(
        retried_p1.page.offset, 0,
        "same-cursor retry must not silently reopen from offset zero"
    );

    // Commit the retried page once, then continue to completion from its next cursor.
    let open_payload_len = open.page.payload.len() as u64;
    let mut reconstructed = open.page.payload;
    reconstructed.extend_from_slice(&retried_p1.page.payload);
    let mut expected_offset = open_payload_len;
    assert_eq!(retried_p1.page.offset, expected_offset);
    expected_offset += retried_p1.page.payload.len() as u64;

    let mut next = retried_p1.page.next_cursor;
    let mut pages = 2usize;
    while let Some(cursor) = next {
        let continued = replacement
            .continue_artifact_content(task_id, subscription_id, cursor)
            .await
            .expect("continue after committed retry transport")
            .expect("continue after committed retry query");
        assert_eq!(continued.subscription_id, subscription_id);
        assert_eq!(continued.page.artifact_id, artifact_id);
        assert_eq!(continued.page.sha256, sha256);
        assert_eq!(continued.page.total_bytes, body.len() as u64);
        assert_eq!(
            continued.page.offset, expected_offset,
            "committed page offsets must be contiguous without duplicated bytes"
        );
        reconstructed.extend_from_slice(&continued.page.payload);
        expected_offset += continued.page.payload.len() as u64;
        next = continued.page.next_cursor;
        pages += 1;
    }
    assert!(
        pages >= 4,
        "tight page budget must require at least four content pages, got {pages}"
    );
    assert_eq!(reconstructed.len() as u64, body.len() as u64);
    assert_eq!(reconstructed, body.as_bytes());
    let reconstructed_digest: [u8; 32] = Sha256::digest(&reconstructed).into();
    assert_eq!(reconstructed_digest, sha256);

    replacement
        .release_artifact_content(task_id, subscription_id)
        .await
        .expect("release artifact content transport")
        .expect("release artifact content");

    let final_identity = read_identity(&lock_path).expect("final host identity");
    assert_eq!(final_identity.pid, original_identity.pid);
    assert_eq!(final_identity.boot_id, original_identity.boot_id);

    replacement.disconnect();
    let status = host
        .terminate_and_wait_bounded(TERMINATE_TIMEOUT)
        .expect("terminate exact foreground host");
    assert!(!status.success(), "test termination should stop the host");
    assert!(host.try_wait().expect("final host poll").is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slow_durable_reader_does_not_delay_other_client_command_receipt() {
    let config_base = TempDir::new().expect("process-unique config base");
    let profile = unique_profile();
    let paths = isolated_paths(&config_base, &profile);
    let lock_path = paths.root.join("host.lock");

    let healthy_id = ClientId::from_bytes(fixed_uuid_v7(0xc0)).expect("healthy client id");
    let slow_id = ClientId::from_bytes(fixed_uuid_v7(0xc1)).expect("slow client id");

    let mut host = ChildGuard::spawn(host_command_with_slow_durable_reader(
        config_base.path(),
        &profile,
        slow_id,
    ));
    let original_identity = wait_for_identity(&mut host, &lock_path).await;

    let requested =
        CapabilitySet::from_capabilities([Capability::PagedSnapshots, Capability::EventReplay]);
    let healthy_config = HostClientConfig {
        named_profile: profile.clone(),
        client_build: format!("devmanager/{}", env!("CARGO_PKG_VERSION")),
        client_id: healthy_id,
        requested,
        limits: FrameLimits::v1_default(),
    };
    let slow_config = HostClientConfig {
        named_profile: profile.clone(),
        client_build: format!("devmanager/{}", env!("CARGO_PKG_VERSION")),
        client_id: slow_id,
        requested,
        limits: FrameLimits::v1_default(),
    };

    let mut healthy = connect_bounded(&healthy_config, &mut host).await;
    let mut slow = connect_bounded(&slow_config, &mut host).await;

    let mut healthy_sub = ClientSubscription::new();
    let mut slow_sub = ClientSubscription::new();
    healthy_sub
        .synchronize(&mut healthy)
        .await
        .expect("healthy initial synchronize");
    slow_sub
        .synchronize(&mut slow)
        .await
        .expect("slow initial synchronize");
    assert_eq!(healthy_sub.state(), ClientSubscriptionState::Ready);
    assert_eq!(slow_sub.state(), ClientSubscriptionState::Ready);
    let slow_baseline = slow_sub
        .model()
        .expect("slow model")
        .last_applied_sequence();

    async fn drain_until_task_and_settled_operation(
        sub: &mut ClientSubscription,
        client: &HostClient,
        after_sequence: u64,
        task_id: TaskId,
        operation_id: OperationId,
    ) -> u64 {
        let mut last_seen = after_sequence;
        loop {
            let model = sub.model().expect("model while draining");
            let task_ready = model.tasks().contains_key(&task_id);
            let operation_settled = matches!(
                model.operations().get(&operation_id).map(|op| &op.state),
                Some(OperationState::Settled { .. })
            );
            if task_ready && operation_settled {
                return model.last_applied_sequence();
            }

            let update = timeout(READY_TIMEOUT, sub.recv_and_apply(client))
                .await
                .expect("healthy durable drain stayed bounded")
                .expect("healthy durable drain apply");
            match update {
                SubscriptionUpdate::DurableEvent(event) => {
                    assert!(
                        event.sequence > last_seen,
                        "live durable events must remain strictly ordered"
                    );
                    last_seen = event.sequence;
                }
                SubscriptionUpdate::Stream(_) => {}
                other => panic!("expected durable event while draining, got {other:?}"),
            }
        }
    }

    let (first_create, _first_command_id, first_task_id) =
        create_task_named(healthy_id, 0xc2, 0xc3, 0xc4, 0xc5, "Slow-reader first task");
    let first_receipt = healthy
        .execute_command(first_create)
        .await
        .expect("first create transport");
    let CommandReceipt::Accepted {
        operation_id: first_operation_id,
        event_ids: first_event_ids,
        ..
    } = first_receipt
    else {
        panic!("first create must be accepted, got {first_receipt:?}");
    };
    assert_eq!(
        first_event_ids.len(),
        1,
        "pure CreateTask acceptance batch has exactly one decision event"
    );

    let first_command_high_water = drain_until_task_and_settled_operation(
        &mut healthy_sub,
        &healthy,
        slow_baseline,
        first_task_id,
        first_operation_id,
    )
    .await;
    assert_eq!(
        first_command_high_water,
        slow_baseline + 3,
        "first CreateTask must advance exactly three durable events"
    );

    let (second_create, second_command_id, second_task_id) = create_task_named(
        healthy_id,
        0xc6,
        0xc7,
        0xc8,
        0xc9,
        "Slow-reader second task",
    );
    let second_receipt = timeout(
        Duration::from_secs(2),
        healthy.execute_command(second_create.clone()),
    )
    .await
    .expect("second create must not be delayed by slow durable reader")
    .expect("second create transport");
    let CommandReceipt::Accepted {
        operation_id: second_operation_id,
        event_ids: second_event_ids,
        ..
    } = second_receipt
    else {
        panic!("second create must be accepted, got {second_receipt:?}");
    };
    assert_eq!(
        second_event_ids.len(),
        1,
        "pure CreateTask acceptance batch has exactly one decision event"
    );

    let second_command_high_water = drain_until_task_and_settled_operation(
        &mut healthy_sub,
        &healthy,
        first_command_high_water,
        second_task_id,
        second_operation_id,
    )
    .await;
    assert_eq!(
        second_command_high_water,
        first_command_high_water + 3,
        "second CreateTask must advance exactly three durable events"
    );

    let healthy_model = healthy_sub.model().expect("healthy model after live");
    assert!(healthy_model.tasks().contains_key(&first_task_id));
    assert!(healthy_model.tasks().contains_key(&second_task_id));
    assert_eq!(
        healthy_model.last_applied_sequence(),
        second_command_high_water
    );

    let retry = healthy
        .execute_command(second_create)
        .await
        .expect("exact second create retry");
    assert!(
        matches!(
            retry,
            CommandReceipt::Accepted {
                command_id,
                ..
            } if command_id == second_command_id
        ),
        "healthy subscription must remain usable for exact accepted retry"
    );

    let resync = loop {
        let update = timeout(READY_TIMEOUT, slow_sub.recv_and_apply(&slow))
            .await
            .expect("slow resync stayed bounded")
            .expect("slow resync apply");
        match update {
            SubscriptionUpdate::ResyncRequired {
                last_delivered_sequence,
                newest_sequence,
            } => break (last_delivered_sequence, newest_sequence),
            SubscriptionUpdate::Stream(_) => continue,
            other => panic!("expected critical ResyncRequired for slow client, got {other:?}"),
        }
    };
    assert_eq!(
        resync.0, slow_baseline,
        "slow last_delivered_sequence must stay at pre-test physical baseline"
    );
    assert!(
        resync.1 >= second_command_high_water,
        "slow newest_sequence must include the complete second CreateTask envelope ({second_command_high_water}), got {}",
        resync.1
    );
    assert_eq!(slow_sub.state(), ClientSubscriptionState::NeedsResync);

    let final_identity = read_identity(&lock_path).expect("final host identity");
    assert_eq!(final_identity.pid, original_identity.pid);
    assert_eq!(final_identity.boot_id, original_identity.boot_id);

    healthy.disconnect();
    slow.disconnect();
    let status = host
        .terminate_and_wait_bounded(TERMINATE_TIMEOUT)
        .expect("terminate exact foreground host");
    assert!(!status.success(), "test termination should stop the host");
    assert!(host.try_wait().expect("final host poll").is_some());
}
