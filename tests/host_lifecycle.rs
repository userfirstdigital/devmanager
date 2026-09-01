//! Real foreground-host lifecycle acceptance.
//!
//! Every fixture uses a process-unique TempDir config base and named debug
//! profile. This target must never resolve or touch installed DevManager data.

#![cfg(windows)]

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command as ProcessCommand, ExitStatus, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use tempfile::TempDir;
use tokio::time::{sleep, timeout};
use uuid::Uuid;

use devmanager::client::{
    connect, perform_client_hello, ClientSubscription, ClientSubscriptionState, HostClient,
    HostClientConfig, InboxHostController, InboxPreferenceStore, SubscriptionError,
    SubscriptionUpdate, TrackedOperation, UnsolicitedServerMessage,
};
use devmanager::config::paths::{resolve_app_paths, AppProfile, BuildKind, ResolvedAppPaths};
use devmanager::config::{ConfigCommand, ConfigStore, Project};
use devmanager::domain::command::{
    Command, CommandEnvelope, CommandReceipt, ConfirmHostQuitIntent, CreateTaskIntent,
    CreateTaskRequestIntent, RejectionCode,
};
use devmanager::domain::event::{DomainEvent, Event};
use devmanager::domain::id::{
    AgentSessionId, ArtifactId, CommandId, EnvironmentId, EventId, OperationId, ProjectId,
    RequestId, ResourceId, TaskId,
};
use devmanager::domain::operation::OperationState;
use devmanager::domain::query::{Query, QueryEnvelope, QueryError, QueryOutcome};
use devmanager::domain::resource::{
    OwnerKind, ResourceFacts, ResourceKind, ResourceLifecycle, ResourceRecipe,
};
use devmanager::domain::snapshot::{SnapshotItem, SnapshotSection};
use devmanager::domain::task::{
    ReviewReadiness, TaskActivity, TaskAssignment, TaskAttention, TaskConnectivity, TaskLifecycle,
    WorkspaceRef,
};
use devmanager::domain::{
    AgentRole, AgentSessionFacts, AgentSessionLifecycle, ArtifactContentRef, ArtifactFacts,
    ArtifactKind, ClientId, HostQuitWorktreeInspection, PrivacyClass,
};
use devmanager::host::{
    pipe_endpoint_for_named_profile, profile_fingerprint_for_named_profile, HostIdentity, IpcError,
    HOST_EXIT_ALREADY_RUNNING,
};
use devmanager::protocol::{Capability, CapabilitySet, ClientHello, FrameLimits};
use devmanager::providers::ProviderKind;
use devmanager::ui::shell::{InboxActionKind, Shell};
use devmanager::ui::task_cockpit::{
    InboxFilter, InboxPresentationWidth, InboxRenderItem, NativeNextTaskCockpit,
};
use devmanager::workspace::{WorkspaceProjectRoots, WorkspaceRequest};
use rusqlite::Connection;
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
        let stderr = self.take_exited_stderr();
        format!("{status}; stderr={stderr:?}")
    }

    fn take_exited_stderr(&mut self) -> String {
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
        stderr
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

    /// Drop the OS process handle after exit so stale `host.lock` identity can
    /// clear (Windows keeps the process object alive while any handle remains).
    fn release_exited_process_handle(&mut self) {
        let _ = self.child.take();
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
    assert_eq!(client.host_boot_id(), Some(original_identity.boot_id));
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
    assert_eq!(client.host_boot_id(), Some(original_identity.boot_id));
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
async fn native_next_bootstrap_drives_visible_inbox_from_fixture_host() {
    let config_base = TempDir::new().expect("process-unique config base");
    let profile = unique_profile();
    let paths = isolated_paths(&config_base, &profile);
    let lock_path = paths.root.join("host.lock");

    // Seed an opaque host project before the real foreground host opens the
    // same profile. V1 CreateTask is Security-rejected; CreateTaskV2 needs a
    // WorkspaceProjectRoots-issued ProjectId for paths.root (cli_client precedent).
    fs::create_dir_all(&paths.root).expect("create isolated profile root");
    let configured_id = "native-next-fixture-project".to_string();
    let opaque_project_id = {
        let mut store = ConfigStore::open_host(&paths).expect("open isolated host config");
        store
            .execute(
                store.snapshot().revision,
                ConfigCommand::CreateProject {
                    project: Project {
                        id: configured_id.clone(),
                        name: "Native-next fixture project".to_string(),
                        root_path: paths.root.to_string_lossy().into_owned(),
                        created_at: "now".to_string(),
                        updated_at: "now".to_string(),
                        ..Project::default()
                    },
                },
            )
            .expect("persist isolated host project");
        let revision = store.snapshot().revision;
        let roots = WorkspaceProjectRoots::from_host_config_store(&mut store, revision, 1, 1)
            .expect("issue isolated host project roots");
        let project_id = roots
            .project_id_for_config_id(&configured_id)
            .expect("opaque isolated host project id");
        drop(store);
        project_id
    };

    let mut host = ChildGuard::spawn(host_command(config_base.path(), &profile));
    let _identity = wait_for_identity(&mut host, &lock_path).await;

    let client_id = ClientId::from_bytes(fixed_uuid_v7(0x76)).expect("client id");
    let command_config = HostClientConfig {
        named_profile: profile.clone(),
        client_build: "native-next-inbox-fixture".to_string(),
        client_id,
        requested: CapabilitySet::from_capabilities([Capability::OperationSettlement]),
        limits: FrameLimits::v1_default(),
    };
    let mut command_client = connect_bounded(&command_config, &mut host).await;
    let command_id = CommandId::from_bytes(fixed_uuid_v7(0x77)).expect("command id");
    let task_id = TaskId::from_bytes(fixed_uuid_v7(0x78)).expect("task id");
    let create = CommandEnvelope {
        command_id,
        client_id,
        task_id: None,
        issued_at_ms: 1_725_000_000_100,
        expected_task_revision: None,
        command: Command::CreateTaskV2(CreateTaskRequestIntent {
            id: task_id,
            environment_id: EnvironmentId::from_bytes(fixed_uuid_v7(0x79)).expect("environment id"),
            title: "Native-next fixture inbox".into(),
            description: None,
            project_id: opaque_project_id,
            workspace: WorkspaceRequest::confirmed_external(&paths.root),
            primary_provider: None,
            defer_primary_provider_start: false,
            assignment: TaskAssignment::LocalOwner,
            created_at_ms: 1_725_000_000_000,
            connectivity: TaskConnectivity::Connected,
            attention: TaskAttention::None,
            activity: TaskActivity::Idle,
            review_readiness: ReviewReadiness::NotReady,
        }),
    };
    let receipt = command_client
        .execute_command(create)
        .await
        .expect("fixture task command");
    assert!(
        matches!(receipt, CommandReceipt::Accepted { .. }),
        "CreateTaskV2 fixture must be Accepted, got {receipt:?}"
    );
    command_client.disconnect();

    let subscription_config = HostClientConfig {
        named_profile: profile,
        client_build: "native-next-inbox-fixture".to_string(),
        client_id: ClientId::from_bytes(fixed_uuid_v7(0x7b)).expect("subscription client id"),
        requested: CapabilitySet::from_capabilities([
            Capability::PagedSnapshots,
            Capability::EventReplay,
        ]),
        limits: FrameLimits::v1_default(),
    };
    let mut subscription_client = connect_bounded(&subscription_config, &mut host).await;
    let controller = InboxHostController::new(InboxPreferenceStore::at_profile_root(
        config_base.path().join("client-preferences"),
    ));
    let mut cockpit = NativeNextTaskCockpit::from_controller(controller)
        .expect("native-next bootstrap must load isolated preferences");
    cockpit
        .synchronize(&mut subscription_client)
        .await
        .expect("native-next controller must synchronize fixture host");

    let model = cockpit.render_model(InboxPresentationWidth::Regular);
    assert!(model.items.iter().any(|item| {
        matches!(
            item,
            InboxRenderItem::Row(row)
                if row.task_id == task_id && row.title == "Native-next fixture inbox"
        )
    }));

    let first_subscription = cockpit
        .controller()
        .expect("controller owner")
        .subscription();
    cockpit
        .reconnect_and_synchronize(&mut subscription_client)
        .await
        .expect("reconnect must resynchronize from an authoritative snapshot");
    let second_subscription = cockpit
        .controller()
        .expect("controller owner")
        .subscription();
    assert!(
        !Arc::ptr_eq(&first_subscription, &second_subscription),
        "reconnect must replace the stale subscription generation"
    );
    assert!(cockpit
        .render_model(InboxPresentationWidth::Regular)
        .items
        .iter()
        .any(|item| matches!(item, InboxRenderItem::Row(row) if row.task_id == task_id)));

    cockpit
        .runtime_mut()
        .set_filter(InboxFilter::new("").including_archived());
    let shell = Shell::detached(Some(task_id));
    let inbox = cockpit
        .runtime()
        .projection()
        .expect("visible inbox projection");
    let captured = shell
        .capture_inbox_row_action(
            inbox.active_row(task_id).expect("active fixture row"),
            shell.navigation_epoch(),
            shell.focus_navigation_epoch(),
            InboxActionKind::Archive,
        )
        .expect("shell captures the current archive row");
    assert!(shell.dispatch_inbox_action(captured, inbox).is_ok());
    let archive = captured
        .host_command(
            CommandId::from_bytes(fixed_uuid_v7(0x7c)).expect("archive command id"),
            ClientId::from_bytes(fixed_uuid_v7(0x7b)).expect("subscription client id"),
            1_725_000_001_000,
        )
        .expect("archive action is a real host command");
    assert!(matches!(
        cockpit
            .execute_command(&mut subscription_client, archive)
            .await
            .expect("archive command"),
        CommandReceipt::Accepted { .. }
    ));

    let _ = host.terminate_and_wait_bounded(TERMINATE_TIMEOUT);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeated_native_next_synchronize_retires_old_tails_without_busy_leaks() {
    let config_base = TempDir::new().expect("process-unique config base");
    let profile = unique_profile();
    let paths = isolated_paths(&config_base, &profile);
    let lock_path = paths.root.join("host.lock");

    let mut host = ChildGuard::spawn(host_command(config_base.path(), &profile));
    let _identity = wait_for_identity(&mut host, &lock_path).await;
    let controller_config = HostClientConfig {
        named_profile: profile,
        client_build: "native-next-repeated-sync-fixture".to_string(),
        client_id: ClientId::from_bytes(fixed_uuid_v7(0x7d)).expect("client id"),
        requested: CapabilitySet::from_capabilities([
            Capability::PagedSnapshots,
            Capability::EventReplay,
        ]),
        limits: FrameLimits::v1_default(),
    };
    let mut controller_client = connect_bounded(&controller_config, &mut host).await;
    let mut controller = InboxHostController::new(InboxPreferenceStore::at_profile_root(
        config_base.path().join("client-preferences"),
    ));
    controller.attach_runtime();
    controller
        .synchronize(&mut controller_client)
        .await
        .expect("initial synchronize");
    let old_subscription = controller.subscription();
    let old_subscription_id = old_subscription
        .lock()
        .expect("old subscription lock")
        .subscription_id()
        .expect("old replay subscription id");

    for _ in 0..40 {
        controller
            .synchronize(&mut controller_client)
            .await
            .expect("repeated synchronize must not leak replay sessions");
        assert_eq!(
            controller.subscription_state(),
            ClientSubscriptionState::Ready,
            "every replacement must settle as ready"
        );
    }

    assert_eq!(
        old_subscription
            .lock()
            .expect("old subscription lock after replacement")
            .state(),
        ClientSubscriptionState::Released,
        "old subscription must be explicitly released before replacement"
    );
    let late_tail = DomainEvent {
        id: EventId::from_bytes(fixed_uuid_v7(0x7e)).expect("late event id"),
        task_id: None,
        sequence: 1,
        task_revision: None,
        occurred_at_ms: 1,
        payload: Event::TaskReopened,
    };
    let late_error = old_subscription
        .lock()
        .expect("old subscription lock for late tail")
        .handle_unsolicited_message(UnsolicitedServerMessage::DurableEvent {
            subscription_id: old_subscription_id,
            event: late_tail,
        })
        .expect_err("late old-tail delivery must be fenced");
    assert!(matches!(late_error, SubscriptionError::Released));

    let _ = host.terminate_and_wait_bounded(TERMINATE_TIMEOUT);
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

    assert_eq!(client_a.host_boot_id(), Some(original_identity.boot_id));
    assert_eq!(client_b.host_boot_id(), Some(original_identity.boot_id));
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
async fn replacing_real_subscription_drains_only_retired_queued_frames() {
    let config_base = TempDir::new().expect("process-unique config base");
    let profile = unique_profile();
    let paths = isolated_paths(&config_base, &profile);
    let lock_path = paths.root.join("host.lock");

    let mut host = ChildGuard::spawn(host_command(config_base.path(), &profile));
    let _identity = wait_for_identity(&mut host, &lock_path).await;

    let writer_id = ClientId::from_bytes(fixed_uuid_v7(0xc2)).expect("writer client id");
    let reader_id = ClientId::from_bytes(fixed_uuid_v7(0xc3)).expect("reader client id");
    let requested =
        CapabilitySet::from_capabilities([Capability::PagedSnapshots, Capability::EventReplay]);
    let config = |client_id| HostClientConfig {
        named_profile: profile.clone(),
        client_build: format!("devmanager/{}", env!("CARGO_PKG_VERSION")),
        client_id,
        requested,
        limits: FrameLimits::v1_default(),
    };
    let mut writer = connect_bounded(&config(writer_id), &mut host).await;
    let mut reader = connect_bounded(&config(reader_id), &mut host).await;

    let mut old = ClientSubscription::new();
    old.synchronize(&mut reader)
        .await
        .expect("old subscription synchronize");

    let (first_create, _, first_task_id) =
        create_task_named(writer_id, 0xc4, 0xc5, 0xc6, 0xc7, "Retired first");
    assert!(matches!(
        writer
            .execute_command(first_create)
            .await
            .expect("first event-producing command"),
        CommandReceipt::Accepted { .. }
    ));
    let first_update = timeout(READY_TIMEOUT, old.recv_and_apply(&reader))
        .await
        .expect("old subscription receives first event")
        .expect("old subscription first event apply");
    assert!(matches!(
        first_update,
        SubscriptionUpdate::DurableEvent(event) if event.task_id == Some(first_task_id)
    ));

    // Keep the rest of the first command's three durable events queued, and
    // add more old-generation events while the caller does not drain them.
    for (offset, title) in [(0xc8, "Retired second"), (0xcc, "Retired third")] {
        let (create, _, _) =
            create_task_named(writer_id, offset, offset + 1, offset + 2, offset + 3, title);
        assert!(matches!(
            writer
                .execute_command(create)
                .await
                .expect("queued old-generation command"),
            CommandReceipt::Accepted { .. }
        ));
    }

    old.release(&mut reader)
        .await
        .expect("retire old subscription and drain its queue");

    let mut replacement = ClientSubscription::new();
    replacement
        .synchronize(&mut reader)
        .await
        .expect("replacement subscription synchronize");

    let (replacement_create, _, replacement_task_id) =
        create_task_named(writer_id, 0xd0, 0xd1, 0xd2, 0xd3, "Replacement event");
    assert!(matches!(
        writer
            .execute_command(replacement_create)
            .await
            .expect("replacement event-producing command"),
        CommandReceipt::Accepted { .. }
    ));

    while !replacement
        .model()
        .expect("replacement model")
        .tasks()
        .contains_key(&replacement_task_id)
    {
        let update = timeout(READY_TIMEOUT, replacement.recv_and_apply(&reader))
            .await
            .expect("replacement receives live event")
            .expect("replacement live event apply");
        assert!(
            matches!(update, SubscriptionUpdate::DurableEvent(_)),
            "retired frames must not surface as replacement errors: {update:?}"
        );
    }

    replacement
        .release(&mut reader)
        .await
        .expect("release replacement subscription");
    writer.disconnect();
    reader.disconnect();
    let status = host
        .terminate_and_wait_bounded(TERMINATE_TIMEOUT)
        .expect("terminate exact foreground host");
    assert!(!status.success(), "test termination should stop the host");
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
    assert_eq!(client.host_boot_id(), Some(original_identity.boot_id));

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
    assert_eq!(replacement.host_boot_id(), Some(original_identity.boot_id));
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn begin_close_rejects_new_runtime_registration_with_closing_before_drain() {
    let config_base = TempDir::new().expect("process-unique config base");
    let profile = unique_profile();
    let paths = isolated_paths(&config_base, &profile);
    let lock_path = paths.root.join("host.lock");

    let mut host = ChildGuard::spawn(host_command(config_base.path(), &profile));
    let original_identity = wait_for_identity(&mut host, &lock_path).await;

    let requested = CapabilitySet::from_capabilities([
        Capability::OperationSettlement,
        Capability::PagedSnapshots,
    ]);
    let client_a_id = ClientId::from_bytes(fixed_uuid_v7(0xe0)).expect("client A id");
    let client_a_config = HostClientConfig {
        named_profile: profile.clone(),
        client_build: format!("devmanager/{}", env!("CARGO_PKG_VERSION")),
        client_id: client_a_id,
        requested,
        limits: FrameLimits::v1_default(),
    };
    let mut client_a = connect_bounded(&client_a_config, &mut host).await;

    let client_b_id = ClientId::from_bytes(fixed_uuid_v7(0xe1)).expect("client B id");
    let client_b_config = HostClientConfig {
        named_profile: profile.clone(),
        client_build: format!("devmanager/{}", env!("CARGO_PKG_VERSION")),
        client_id: client_b_id,
        requested,
        limits: FrameLimits::v1_default(),
    };
    let mut client_b = connect_bounded(&client_b_config, &mut host).await;

    assert_eq!(client_a.host_boot_id(), Some(original_identity.boot_id));
    assert_eq!(client_b.host_boot_id(), Some(original_identity.boot_id));

    let (create, _, task_id) = create_task_named(
        client_a_id,
        0xe2,
        0xe3,
        0xe4,
        0xe5,
        "Closing admission barrier",
    );
    assert!(matches!(
        client_a
            .execute_command(create)
            .await
            .expect("client A creates task"),
        CommandReceipt::Accepted { .. }
    ));

    let existing_resource_id =
        ResourceId::from_bytes(fixed_uuid_v7(0xe7)).expect("existing resource id");
    let register_existing = CommandEnvelope {
        command_id: CommandId::from_bytes(fixed_uuid_v7(0xe8))
            .expect("register existing command id"),
        client_id: client_a_id,
        task_id: Some(task_id),
        issued_at_ms: 1_725_000_000_150,
        expected_task_revision: Some(1),
        command: Command::RegisterResource {
            resource: ResourceFacts {
                id: existing_resource_id,
                task_id: Some(task_id),
                owner_kind: OwnerKind::Task,
                resource_kind: ResourceKind::Terminal,
                recipe: ResourceRecipe::Terminal { cols: 80, rows: 24 },
                lifecycle: ResourceLifecycle::Active,
                runtime_generation: 0,
                updated_at_ms: 1_725_000_000_150,
            },
        },
    };
    assert!(matches!(
        client_a
            .execute_command(register_existing)
            .await
            .expect("client A registers active Task-owned terminal"),
        CommandReceipt::Accepted {
            task_revision: Some(2),
            ..
        }
    ));

    let begin_close = CommandEnvelope {
        command_id: CommandId::from_bytes(fixed_uuid_v7(0xe6)).expect("begin close command id"),
        client_id: client_a_id,
        task_id: Some(task_id),
        issued_at_ms: 1_725_000_000_200,
        expected_task_revision: Some(2),
        command: Command::BeginCloseTask,
    };
    let close_receipt = client_a
        .execute_command(begin_close)
        .await
        .expect("client A begin close");
    let (close_operation_id, close_event_ids) = match close_receipt {
        CommandReceipt::Accepted {
            operation_id,
            task_revision,
            event_ids,
            ..
        } => {
            assert_eq!(
                task_revision,
                Some(3),
                "begin close must advance to revision 3 after resource registration"
            );
            assert_eq!(
                event_ids.len(),
                1,
                "begin close must emit exactly one decision event"
            );
            (operation_id, event_ids)
        }
        other => panic!("expected Accepted begin close, got {other:?}"),
    };
    assert_eq!(close_event_ids.len(), 1);

    let close_state = client_a
        .refresh_operation(close_operation_id)
        .await
        .expect("refresh close operation transport")
        .expect("known close operation");
    assert!(
        matches!(close_state, OperationState::Accepted),
        "close must remain Accepted while Task-owned resources block drain; got {close_state:?}"
    );

    let rejected_resource_id =
        ResourceId::from_bytes(fixed_uuid_v7(0xe9)).expect("rejected resource id");
    let register = CommandEnvelope {
        command_id: CommandId::from_bytes(fixed_uuid_v7(0xea)).expect("register command id"),
        client_id: client_b_id,
        task_id: Some(task_id),
        issued_at_ms: 1_725_000_000_300,
        expected_task_revision: Some(3),
        command: Command::RegisterResource {
            resource: ResourceFacts {
                id: rejected_resource_id,
                task_id: Some(task_id),
                owner_kind: OwnerKind::Task,
                resource_kind: ResourceKind::Terminal,
                recipe: ResourceRecipe::Terminal { cols: 80, rows: 24 },
                lifecycle: ResourceLifecycle::Active,
                runtime_generation: 0,
                updated_at_ms: 1_725_000_000_300,
            },
        },
    };
    let rejected = client_b
        .execute_command(register)
        .await
        .expect("client B register resource while closing");
    assert_eq!(
        rejected,
        CommandReceipt::Rejected {
            command_id: CommandId::from_bytes(fixed_uuid_v7(0xea)).expect("register command id"),
            code: RejectionCode::Closing,
            current_revision: Some(3),
            resolution: None,
        }
    );

    let snapshot = client_b
        .task_snapshot(task_id)
        .await
        .expect("task snapshot transport")
        .expect("task snapshot query");
    assert_eq!(snapshot.task.lifecycle, TaskLifecycle::Closing);
    assert_eq!(snapshot.task.action_epoch, 1);
    assert_eq!(snapshot.task.revision, 3);

    let resources = client_b
        .snapshot_page(SnapshotSection::Resources, None, None)
        .await
        .expect("resources snapshot transport")
        .expect("resources snapshot query");
    assert!(
        resources.items.iter().any(|item| matches!(
            item,
            SnapshotItem::Resource(facts)
                if facts.id == existing_resource_id
                    && facts.lifecycle == ResourceLifecycle::Active
                    && facts.owner_kind == OwnerKind::Task
        )),
        "pre-existing Active Task-owned terminal must remain while teardown stays pending"
    );
    assert!(
        !resources.items.iter().any(|item| matches!(
            item,
            SnapshotItem::Resource(facts) if facts.id == rejected_resource_id
        )),
        "rejected resource must be absent from durable resources"
    );
    client_b
        .release_snapshot(resources.snapshot_id)
        .await
        .expect("release resources snapshot transport")
        .expect("release resources snapshot");

    let close_state_after = client_a
        .refresh_operation(close_operation_id)
        .await
        .expect("refresh close operation after rejection")
        .expect("known close operation after rejection");
    assert!(
        matches!(close_state_after, OperationState::Accepted),
        "close must remain Accepted after rejected registration; got {close_state_after:?}"
    );

    let final_identity = read_identity(&lock_path).expect("final host identity");
    assert_eq!(final_identity.pid, original_identity.pid);
    assert_eq!(final_identity.boot_id, original_identity.boot_id);
    assert!(paths.database.exists());

    client_a.disconnect();
    client_b.disconnect();
    let status = host
        .terminate_and_wait_bounded(TERMINATE_TIMEOUT)
        .expect("terminate exact foreground host");
    assert!(!status.success(), "test termination should stop the host");
    assert!(host.try_wait().expect("final host poll").is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_begin_close_settles_and_archives_via_host_maintenance() {
    let config_base = TempDir::new().expect("process-unique config base");
    let profile = unique_profile();
    let paths = isolated_paths(&config_base, &profile);
    let lock_path = paths.root.join("host.lock");

    let mut host = ChildGuard::spawn(host_command(config_base.path(), &profile));
    let original_identity = wait_for_identity(&mut host, &lock_path).await;

    let requested = CapabilitySet::from_capabilities([
        Capability::OperationSettlement,
        Capability::PagedSnapshots,
    ]);
    let client_id = ClientId::from_bytes(fixed_uuid_v7(0xf0)).expect("client id");
    let config = HostClientConfig {
        named_profile: profile.clone(),
        client_build: format!("devmanager/{}", env!("CARGO_PKG_VERSION")),
        client_id,
        requested,
        limits: FrameLimits::v1_default(),
    };
    let mut client = connect_bounded(&config, &mut host).await;
    assert_eq!(client.host_boot_id(), Some(original_identity.boot_id));

    let (create, _, task_id) = create_task_named(
        client_id,
        0xf1,
        0xf2,
        0xf3,
        0xf4,
        "Empty close maintenance settle",
    );
    assert!(matches!(
        client
            .execute_command(create)
            .await
            .expect("create empty task"),
        CommandReceipt::Accepted { .. }
    ));

    let begin_close = CommandEnvelope {
        command_id: CommandId::from_bytes(fixed_uuid_v7(0xf5)).expect("begin close command id"),
        client_id,
        task_id: Some(task_id),
        issued_at_ms: 1_725_000_000_200,
        expected_task_revision: Some(1),
        command: Command::BeginCloseTask,
    };
    let close_receipt = client
        .execute_command(begin_close)
        .await
        .expect("begin close empty task");
    let close_operation_id = match close_receipt {
        CommandReceipt::Accepted {
            operation_id,
            task_revision,
            event_ids,
            ..
        } => {
            assert_eq!(
                task_revision,
                Some(2),
                "empty begin close advances to revision 2"
            );
            assert_eq!(
                event_ids.len(),
                1,
                "begin close must emit exactly one decision event"
            );
            operation_id
        }
        other => panic!("expected Accepted begin close, got {other:?}"),
    };

    // Receipt already proves Accepted. Do not assert intermediate Accepted/Closing
    // via refresh — maintenance may settle before the next poll on a loaded machine.
    let started = Instant::now();
    let (settled_state, archived_snapshot) = loop {
        if let Some(status) = host.try_wait().expect("poll host while waiting for settle") {
            let diagnostics = host.exited_diagnostics(status);
            panic!("foreground host exited before empty-close settle: {diagnostics}");
        }

        let state = client
            .refresh_operation(close_operation_id)
            .await
            .expect("refresh close while waiting for settle")
            .expect("known close operation while waiting");
        let snapshot = client
            .task_snapshot(task_id)
            .await
            .expect("task snapshot while waiting for archive")
            .expect("task present while waiting");

        if matches!(state, OperationState::Settled { .. })
            && snapshot.task.lifecycle == TaskLifecycle::Archived
        {
            break (state, snapshot);
        }

        assert!(
            started.elapsed() < READY_TIMEOUT,
            "timed out waiting for empty BeginClose to settle and archive; last state={state:?} lifecycle={:?}",
            snapshot.task.lifecycle
        );
        sleep(POLL).await;
    };

    assert!(matches!(settled_state, OperationState::Settled { .. }));
    assert_eq!(archived_snapshot.task.lifecycle, TaskLifecycle::Archived);
    assert_eq!(archived_snapshot.task.revision, 3);
    assert_eq!(archived_snapshot.task.action_epoch, 1);

    let final_identity = read_identity(&lock_path).expect("final host identity");
    assert_eq!(final_identity.pid, original_identity.pid);
    assert_eq!(final_identity.boot_id, original_identity.boot_id);
    assert!(paths.database.exists());

    client.disconnect();
    let status = host
        .terminate_and_wait_bounded(TERMINATE_TIMEOUT)
        .expect("terminate exact foreground host");
    assert!(!status.success(), "test termination should stop the host");
    assert!(host.try_wait().expect("final host poll").is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inspect_host_quit_reports_durable_blockers_without_mutation_or_exit() {
    const PRIVATE_PROVIDER_SESSION: &str = "private-provider-session-sentinel-2_6c4";
    const PRIVATE_BROWSER_URL: &str = "https://private.browser.url.sentinel/2_6c4";
    const PRIVATE_SERVICE_COMMAND: &str = "private-service-command-sentinel-2_6c4";
    const TASK_TITLE: &str = "Inspect host quit blockers";

    let config_base = TempDir::new().expect("process-unique config base");
    let profile = unique_profile();
    let paths = isolated_paths(&config_base, &profile);
    let lock_path = paths.root.join("host.lock");

    let mut host = ChildGuard::spawn(host_command(config_base.path(), &profile));
    let original_identity = wait_for_identity(&mut host, &lock_path).await;

    let client_id = ClientId::from_bytes(fixed_uuid_v7(0x20)).expect("client id");
    let requested = CapabilitySet::from_capabilities([
        Capability::OperationSettlement,
        Capability::HostShutdown,
    ]);
    let limits = FrameLimits::v1_default();
    let config = HostClientConfig {
        named_profile: profile.clone(),
        client_build: format!("devmanager/{}", env!("CARGO_PKG_VERSION")),
        client_id,
        requested,
        limits,
    };
    let mut client = connect_bounded(&config, &mut host).await;
    assert!(
        client
            .granted_capabilities()
            .contains(Capability::HostShutdown),
        "debug foreground host must advertise HostShutdown"
    );
    assert_eq!(client.host_boot_id(), Some(original_identity.boot_id));

    let (create, _, task_id) = create_task_named(client_id, 0x21, 0x22, 0x23, 0x24, TASK_TITLE);
    assert!(matches!(
        client.execute_command(create).await.expect("create task"),
        CommandReceipt::Accepted { .. }
    ));

    let agent_session_id =
        AgentSessionId::from_bytes(fixed_uuid_v7(0x25)).expect("agent session id");
    let register_agent = CommandEnvelope {
        command_id: CommandId::from_bytes(fixed_uuid_v7(0x26)).expect("register agent command"),
        client_id,
        task_id: Some(task_id),
        issued_at_ms: 1_725_000_000_150,
        expected_task_revision: Some(1),
        command: Command::RegisterAgentSession {
            agent: AgentSessionFacts {
                id: agent_session_id,
                task_id,
                role: AgentRole::Primary,
                provider_kind: ProviderKind::ClaudeCode,
                provider_session_id: Some(
                    PRIVATE_PROVIDER_SESSION.parse().expect("provider session"),
                ),
                lifecycle: AgentSessionLifecycle::Open,
                runtime_generation: 0,
                revision: 0,
            },
        },
    };
    assert!(matches!(
        client
            .execute_command(register_agent)
            .await
            .expect("register open agent"),
        CommandReceipt::Accepted {
            task_revision: Some(2),
            ..
        }
    ));

    let terminal_id = ResourceId::from_bytes(fixed_uuid_v7(0x27)).expect("terminal id");
    let register_terminal = CommandEnvelope {
        command_id: CommandId::from_bytes(fixed_uuid_v7(0x28)).expect("register terminal command"),
        client_id,
        task_id: Some(task_id),
        issued_at_ms: 1_725_000_000_160,
        expected_task_revision: Some(2),
        command: Command::RegisterResource {
            resource: ResourceFacts {
                id: terminal_id,
                task_id: Some(task_id),
                owner_kind: OwnerKind::Task,
                resource_kind: ResourceKind::Terminal,
                recipe: ResourceRecipe::Terminal { cols: 80, rows: 24 },
                lifecycle: ResourceLifecycle::Active,
                runtime_generation: 0,
                updated_at_ms: 1_725_000_000_160,
            },
        },
    };
    assert!(matches!(
        client
            .execute_command(register_terminal)
            .await
            .expect("register active terminal"),
        CommandReceipt::Accepted {
            task_revision: Some(3),
            ..
        }
    ));

    let browser_id = ResourceId::from_bytes(fixed_uuid_v7(0x29)).expect("browser id");
    let register_browser = CommandEnvelope {
        command_id: CommandId::from_bytes(fixed_uuid_v7(0x2a)).expect("register browser command"),
        client_id,
        task_id: Some(task_id),
        issued_at_ms: 1_725_000_000_170,
        expected_task_revision: Some(3),
        command: Command::RegisterResource {
            resource: ResourceFacts {
                id: browser_id,
                task_id: Some(task_id),
                owner_kind: OwnerKind::Task,
                resource_kind: ResourceKind::BrowserContext,
                recipe: ResourceRecipe::browser(PRIVATE_BROWSER_URL).expect("browser recipe"),
                lifecycle: ResourceLifecycle::Active,
                runtime_generation: 0,
                updated_at_ms: 1_725_000_000_170,
            },
        },
    };
    assert!(matches!(
        client
            .execute_command(register_browser)
            .await
            .expect("register active browser"),
        CommandReceipt::Accepted {
            task_revision: Some(4),
            ..
        }
    ));

    let service_id = ResourceId::from_bytes(fixed_uuid_v7(0x2b)).expect("service id");
    let register_service = CommandEnvelope {
        command_id: CommandId::from_bytes(fixed_uuid_v7(0x2c)).expect("register service command"),
        client_id,
        task_id: Some(task_id),
        issued_at_ms: 1_725_000_000_180,
        expected_task_revision: Some(4),
        command: Command::RegisterResource {
            resource: ResourceFacts {
                id: service_id,
                task_id: Some(task_id),
                owner_kind: OwnerKind::Task,
                resource_kind: ResourceKind::Service,
                recipe: ResourceRecipe::service(PRIVATE_SERVICE_COMMAND).expect("service recipe"),
                lifecycle: ResourceLifecycle::Active,
                runtime_generation: 0,
                updated_at_ms: 1_725_000_000_180,
            },
        },
    };
    assert!(matches!(
        client
            .execute_command(register_service)
            .await
            .expect("register active service"),
        CommandReceipt::Accepted {
            task_revision: Some(5),
            ..
        }
    ));

    let release_service = CommandEnvelope {
        command_id: CommandId::from_bytes(fixed_uuid_v7(0x2d)).expect("release service command"),
        client_id,
        task_id: Some(task_id),
        issued_at_ms: 1_725_000_000_190,
        expected_task_revision: Some(5),
        command: Command::ReleaseResource {
            resource_id: service_id,
        },
    };
    assert!(matches!(
        client
            .execute_command(release_service)
            .await
            .expect("begin service release"),
        CommandReceipt::Accepted {
            task_revision: Some(6),
            ..
        }
    ));

    let scoped_client_id = ClientId::from_bytes(fixed_uuid_v7(0x2e)).expect("scoped client id");
    let scoped_hello = ClientHello::new(
        format!("devmanager/{}", env!("CARGO_PKG_VERSION")),
        scoped_client_id,
        profile_fingerprint_for_named_profile(&profile).expect("profile fingerprint"),
        requested,
        limits,
    )
    .expect("scoped inspect hello");
    let endpoint = pipe_endpoint_for_named_profile(&profile).expect("profile endpoint");
    let scoped_connection = timeout(CONNECT_ATTEMPT_TIMEOUT, connect(&endpoint, &scoped_hello))
        .await
        .expect("scoped inspect attach stayed bounded")
        .expect("scoped inspect attach");
    let scoped_reply = scoped_connection
        .query(QueryEnvelope {
            request_id: RequestId::new(),
            client_id: scoped_client_id,
            task_id: Some(task_id),
            query: Query::InspectHostQuit,
        })
        .await
        .expect("scoped inspect query transport");
    assert_eq!(
        scoped_reply.outcome,
        QueryOutcome::Err(QueryError::InvalidRequest),
        "InspectHostQuit must reject Task scope"
    );
    drop(scoped_connection);

    fn count_table(path: &Path, table: &str) -> i64 {
        let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("open sqlite read-only counts");
        conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .expect("count table")
    }
    let events_before = count_table(&paths.database, "events");
    let operations_before = count_table(&paths.database, "operations");
    let outbox_before = count_table(&paths.database, "outbox");

    let first = client
        .inspect_host_quit()
        .await
        .expect("first inspect transport")
        .expect("first inspect query");
    let second = client
        .inspect_host_quit()
        .await
        .expect("second inspect transport")
        .expect("second inspect query");
    assert_eq!(first, second, "repeat inspect must be deterministic");

    assert_eq!(first.worktrees, HostQuitWorktreeInspection::NotInspected);
    assert!(!first.confirmable);
    assert_eq!(first.agents.len(), 1);
    assert_eq!(first.resources.len(), 3);

    let agent = &first.agents[0];
    assert_eq!(agent.agent_session_id, agent_session_id);
    assert_eq!(agent.task_id, task_id);
    assert_eq!(agent.task_title, TASK_TITLE);
    assert_eq!(agent.role, AgentRole::Primary);
    assert_eq!(agent.provider_kind, ProviderKind::ClaudeCode);
    assert_eq!(agent.lifecycle, AgentSessionLifecycle::Open);
    assert_eq!(agent.runtime_generation, 0);

    let resource_ids: Vec<_> = first.resources.iter().map(|r| r.resource_id).collect();
    let mut sorted_ids = resource_ids.clone();
    sorted_ids.sort();
    assert_eq!(
        resource_ids, sorted_ids,
        "resources must be stable-id ordered"
    );

    let by_id = |id: ResourceId| {
        first
            .resources
            .iter()
            .find(|r| r.resource_id == id)
            .unwrap_or_else(|| panic!("missing resource {id:?}"))
    };
    let terminal = by_id(terminal_id);
    assert_eq!(terminal.task_id, Some(task_id));
    assert_eq!(terminal.task_title.as_deref(), Some(TASK_TITLE));
    assert_eq!(terminal.owner_kind, OwnerKind::Task);
    assert_eq!(terminal.resource_kind, ResourceKind::Terminal);
    assert_eq!(terminal.lifecycle, ResourceLifecycle::Active);

    let browser = by_id(browser_id);
    assert_eq!(browser.resource_kind, ResourceKind::BrowserContext);
    assert_eq!(browser.lifecycle, ResourceLifecycle::Active);

    let service = by_id(service_id);
    assert_eq!(service.resource_kind, ResourceKind::Service);
    assert_eq!(service.lifecycle, ResourceLifecycle::Releasing);

    let encoded = rmp_serde::to_vec_named(&first).expect("encode inspection");
    for sentinel in [
        PRIVATE_PROVIDER_SESSION.as_bytes(),
        PRIVATE_BROWSER_URL.as_bytes(),
        PRIVATE_SERVICE_COMMAND.as_bytes(),
    ] {
        assert!(
            !encoded
                .windows(sentinel.len())
                .any(|window| window == sentinel),
            "inspection must omit private sentinel {:?}",
            std::str::from_utf8(sentinel)
        );
    }

    assert_eq!(count_table(&paths.database, "events"), events_before);
    assert_eq!(
        count_table(&paths.database, "operations"),
        operations_before
    );
    assert_eq!(count_table(&paths.database, "outbox"), outbox_before);

    let final_identity = read_identity(&lock_path).expect("final host identity");
    assert_eq!(final_identity.pid, original_identity.pid);
    assert_eq!(final_identity.boot_id, original_identity.boot_id);
    assert!(
        host.try_wait().expect("poll host after inspect").is_none(),
        "inspect must not exit the foreground host"
    );

    client.disconnect();
    let status = host
        .terminate_and_wait_bounded(TERMINATE_TIMEOUT)
        .expect("terminate exact foreground host");
    assert!(!status.success(), "test termination should stop the host");
    assert!(host.try_wait().expect("final host poll").is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn confirmed_quit_settles_live_and_exits_foreground_host_successfully() {
    use devmanager::domain::event::Event;
    use devmanager::domain::host::{HostCleanupBranch, HostCleanupBranchOutcome};
    use devmanager::host::{HostCleanupWorker, HostRestartDisposition};
    use devmanager::kernel::CommandBus;

    let config_base = TempDir::new().expect("process-unique config base");
    let profile = unique_profile();
    let paths = isolated_paths(&config_base, &profile);
    let lock_path = paths.root.join("host.lock");

    let mut host = ChildGuard::spawn(host_command(config_base.path(), &profile));
    let original_identity = wait_for_identity(&mut host, &lock_path).await;

    let client_id = ClientId::from_bytes(fixed_uuid_v7(0x40)).expect("client");
    let requested = CapabilitySet::from_capabilities([
        Capability::OperationSettlement,
        Capability::HostShutdown,
        Capability::PagedSnapshots,
        Capability::EventReplay,
    ]);
    let config = HostClientConfig {
        named_profile: profile.clone(),
        client_build: format!("devmanager/{}", env!("CARGO_PKG_VERSION")),
        client_id,
        requested,
        limits: FrameLimits::v1_default(),
    };
    let mut client = connect_bounded(&config, &mut host).await;
    assert_eq!(client.host_boot_id(), Some(original_identity.boot_id));

    let mut sub = ClientSubscription::new();
    sub.synchronize(&mut client)
        .await
        .expect("pre-confirm synchronize");
    assert_eq!(sub.state(), ClientSubscriptionState::Ready);

    let inspection = client
        .inspect_host_quit()
        .await
        .expect("inspect transport")
        .expect("inspect query");
    let confirm_command_id =
        CommandId::from_bytes(fixed_uuid_v7(0x41)).expect("confirm command id");
    let confirm = client
        .confirm_host_quit(confirm_command_id, inspection.inspection_id, true)
        .await
        .expect("confirm transport");
    let operation_id = match &confirm {
        CommandReceipt::Accepted {
            operation_id,
            task_revision: None,
            ..
        } => *operation_id,
        other => panic!("expected taskless Accepted, got {other:?}"),
    };

    let expected_order = HostCleanupBranch::ORDER;
    let mut observed = Vec::new();
    let mut branch_event_ids = Vec::new();
    let mut final_branch_sequence = None;
    let mut settled_live = None;
    let mut host_exit = None;
    let started = Instant::now();
    while settled_live.is_none() {
        // Host physical exit can precede client application of already-written
        // buffered frames — store exit status and keep applying until complete.
        if host_exit.is_none() {
            if let Some(status) = host.try_wait().expect("poll host while waiting quit path") {
                host_exit = Some(status);
            }
        }
        let update = match timeout(READY_TIMEOUT, sub.recv_and_apply(&client)).await {
            Ok(Ok(update)) => update,
            Ok(Err(error)) => panic!("live quit apply failed: {error}"),
            Err(_) => panic!(
                "timed out waiting for cleanup + OperationSettled; observed={observed:?} host_exit={host_exit:?}"
            ),
        };
        match update {
            SubscriptionUpdate::DurableEvent(event) => match &event.payload {
                Event::HostCleanupBranchCompleted {
                    operation_id: event_op,
                    action_epoch,
                    branch,
                    outcome,
                } => {
                    assert_eq!(*event_op, operation_id);
                    assert_eq!(*action_epoch, 1);
                    assert_eq!(*outcome, HostCleanupBranchOutcome::succeeded());
                    branch_event_ids.push(event.id);
                    observed.push(*branch);
                    final_branch_sequence = Some(event.sequence);
                }
                Event::OperationSettled(fact) => {
                    assert_eq!(fact.operation_id, operation_id);
                    assert_eq!(fact.action_epoch, Some(1));
                    assert_eq!(fact.command_id, confirm_command_id);
                    assert_eq!(fact.result_event_ids, branch_event_ids);
                    settled_live = Some(event);
                }
                Event::HostCloseBegun { .. } | Event::OperationAccepted(_) => {
                    // ConfirmHostQuit Accepted fan-out; continue waiting for cleanup.
                }
                other => panic!("unexpected durable payload during quit path: {other:?}"),
            },
            other => panic!("unexpected subscription update while waiting quit path: {other:?}"),
        }
        assert!(
            started.elapsed() < READY_TIMEOUT,
            "timed out waiting for cleanup + OperationSettled; observed={observed:?}"
        );
    }
    assert_eq!(observed.as_slice(), &expected_order);
    assert_eq!(branch_event_ids.len(), expected_order.len());
    let final_branch_sequence =
        final_branch_sequence.expect("fourth HostCleanupBranchCompleted sequence");
    let settled_event = settled_live.expect("live OperationSettled");
    assert_eq!(
        settled_event.sequence,
        final_branch_sequence + 1,
        "OperationSettled must be the immediate successor of branch four"
    );

    // Successful bounded host exit: pipe gone, PID dead.
    let exit_deadline = Instant::now() + READY_TIMEOUT;
    let exit_status = match host_exit {
        Some(status) => status,
        None => loop {
            if let Some(status) = host.try_wait().expect("poll host exit after settled") {
                break status;
            }
            assert!(
                Instant::now() < exit_deadline,
                "timed out waiting for intentional foreground host exit"
            );
            sleep(POLL).await;
        },
    };
    assert!(
        exit_status.success(),
        "healthy-client full quit must exit successfully: {}",
        host.exited_diagnostics(exit_status)
    );
    assert!(host.try_wait().expect("final host poll").is_some());
    host.release_exited_process_handle();

    // Pipe must be absent: only ERROR_FILE_NOT_FOUND proves absence.
    assert_named_pipe_absent(&profile, Instant::now() + READY_TIMEOUT).await;

    assert_eq!(count_operation_settled(&paths.database), 1);
    let (settled_sequence, settled_fact) = read_latest_operation_settled_fact(&paths.database);
    assert_eq!(settled_sequence as u64, settled_event.sequence);
    assert_eq!(settled_fact.operation_id, operation_id);
    assert_eq!(settled_fact.action_epoch, Some(1));
    assert_eq!(settled_fact.result_event_ids, branch_event_ids);
    assert_eq!(
        settled_sequence as u64,
        final_branch_sequence + 1,
        "SQLite terminal sequence must immediately follow branch four"
    );
    {
        let bus = CommandBus::open(&paths.database).expect("open bus after quit");
        assert_eq!(
            bus.operation_status(operation_id)
                .expect("operation status")
                .expect("quit operation present"),
            OperationState::Settled {
                settled_at_ms: settled_fact.settled_at_ms,
                result_event_ids: settled_fact.result_event_ids.clone(),
            }
        );
        assert_eq!(
            HostCleanupWorker::restart_disposition(&bus).expect("closed disposition"),
            HostRestartDisposition::Closed {
                operation_id,
                action_epoch: 1,
                settled_at_ms: settled_fact.settled_at_ms,
            }
        );
    }

    // Same-profile lock must be re-acquirable via the existing pre-bind Closed helper.
    spawn_and_require_prebind_exit(config_base.path(), &profile).await;
    assert_eq!(count_operation_settled(&paths.database), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn confirmed_quit_with_residue_terminalizes_cleanup_failed_live_and_keeps_host_running() {
    use devmanager::domain::agent::{AgentRole, AgentSessionFacts, AgentSessionLifecycle};
    use devmanager::domain::event::Event;
    use devmanager::domain::host::{HostCleanupBranch, HostCleanupBranchOutcome};
    use devmanager::domain::id::AgentSessionId;
    use devmanager::domain::operation::OperationErrorCode;
    use devmanager::providers::ProviderKind;

    let config_base = TempDir::new().expect("process-unique config base");
    let profile = unique_profile();
    let paths = isolated_paths(&config_base, &profile);
    let lock_path = paths.root.join("host.lock");

    let mut host = ChildGuard::spawn(host_command(config_base.path(), &profile));
    let original_identity = wait_for_identity(&mut host, &lock_path).await;

    let client_id = ClientId::from_bytes(fixed_uuid_v7(0x50)).expect("client");
    let requested = CapabilitySet::from_capabilities([
        Capability::OperationSettlement,
        Capability::HostShutdown,
        Capability::PagedSnapshots,
        Capability::EventReplay,
    ]);
    let config = HostClientConfig {
        named_profile: profile.clone(),
        client_build: format!("devmanager/{}", env!("CARGO_PKG_VERSION")),
        client_id,
        requested,
        limits: FrameLimits::v1_default(),
    };
    let mut client = connect_bounded(&config, &mut host).await;
    assert_eq!(client.host_boot_id(), Some(original_identity.boot_id));

    let (create, _, task_id) =
        create_task_named(client_id, 0x51, 0x52, 0x53, 0x54, "cleanup-failed residue");
    assert!(matches!(
        client.execute_command(create).await.expect("create task"),
        CommandReceipt::Accepted { .. }
    ));
    let register_agent = CommandEnvelope {
        command_id: CommandId::from_bytes(fixed_uuid_v7(0x55)).expect("register agent"),
        client_id,
        task_id: Some(task_id),
        issued_at_ms: 1_725_000_000_150,
        expected_task_revision: Some(1),
        command: Command::RegisterAgentSession {
            agent: AgentSessionFacts {
                id: AgentSessionId::from_bytes(fixed_uuid_v7(0x56)).expect("agent"),
                task_id,
                role: AgentRole::Primary,
                provider_kind: ProviderKind::ClaudeCode,
                provider_session_id: Some(
                    "cleanup-failed-session".parse().expect("provider session"),
                ),
                lifecycle: AgentSessionLifecycle::Open,
                runtime_generation: 0,
                revision: 0,
            },
        },
    };
    assert!(matches!(
        client
            .execute_command(register_agent)
            .await
            .expect("register open agent"),
        CommandReceipt::Accepted {
            task_revision: Some(2),
            ..
        }
    ));

    let mut sub = ClientSubscription::new();
    sub.synchronize(&mut client)
        .await
        .expect("pre-confirm synchronize");
    assert_eq!(sub.state(), ClientSubscriptionState::Ready);
    let pre_close_cursor = sub.model().expect("model").last_applied_sequence();

    let inspection = client
        .inspect_host_quit()
        .await
        .expect("inspect transport")
        .expect("inspect query");
    assert!(
        !inspection.agents.is_empty(),
        "open agent must appear as durable quit residue"
    );
    let confirm_command_id =
        CommandId::from_bytes(fixed_uuid_v7(0x57)).expect("confirm command id");
    let confirm = client
        .confirm_host_quit(confirm_command_id, inspection.inspection_id, true)
        .await
        .expect("confirm transport");
    let operation_id = match &confirm {
        CommandReceipt::Accepted {
            operation_id,
            task_revision: None,
            ..
        } => *operation_id,
        other => panic!("expected taskless Accepted, got {other:?}"),
    };

    let expected_order = HostCleanupBranch::ORDER;
    let mut saw_close_begun = false;
    let mut saw_operation_accepted = false;
    let mut observed_branches = Vec::new();
    let mut failed_fact = None;
    let started = Instant::now();
    while failed_fact.is_none() {
        if let Some(status) = host
            .try_wait()
            .expect("poll host while waiting cleanup failure")
        {
            let diagnostics = host.exited_diagnostics(status);
            panic!("foreground host exited during cleanup failure journal: {diagnostics}");
        }
        let update = timeout(READY_TIMEOUT, sub.recv_and_apply(&client))
            .await
            .expect("live cleanup/failure apply stayed bounded")
            .expect("live cleanup/failure apply");
        match update {
            SubscriptionUpdate::DurableEvent(event) => match event.payload {
                Event::HostCloseBegun {
                    operation_id: event_op,
                    action_epoch,
                    inspection_id,
                } => {
                    assert!(!saw_close_begun, "HostCloseBegun must be unique");
                    assert!(
                        observed_branches.is_empty(),
                        "HostCloseBegun must precede cleanup branches"
                    );
                    assert_eq!(event_op, operation_id);
                    assert_eq!(action_epoch, 1);
                    assert_eq!(inspection_id, inspection.inspection_id);
                    saw_close_begun = true;
                }
                Event::OperationAccepted(fact) => {
                    assert!(
                        saw_close_begun,
                        "OperationAccepted must follow HostCloseBegun"
                    );
                    assert!(
                        !saw_operation_accepted,
                        "host quit OperationAccepted must be unique"
                    );
                    assert_eq!(fact.command_id, confirm_command_id);
                    assert_eq!(fact.operation_id, operation_id);
                    assert_eq!(fact.action_epoch, Some(1));
                    assert!(fact.resource_id.is_none());
                    assert!(fact.runtime_generation.is_none());
                    saw_operation_accepted = true;
                }
                Event::HostCleanupBranchCompleted {
                    operation_id: event_op,
                    action_epoch,
                    branch,
                    outcome,
                } => {
                    assert!(
                        saw_close_begun && saw_operation_accepted,
                        "cleanup branches must follow the durable admission pair"
                    );
                    assert_eq!(event_op, operation_id);
                    assert_eq!(action_epoch, 1);
                    assert!(
                        matches!(
                            outcome,
                            HostCleanupBranchOutcome::Succeeded
                                | HostCleanupBranchOutcome::Failed { .. }
                        ),
                        "branch outcome must be Succeeded or Failed, got {outcome:?}"
                    );
                    observed_branches.push(branch);
                }
                Event::OperationFailed(fact) => {
                    assert!(
                        saw_close_begun && saw_operation_accepted,
                        "CleanupFailed must follow the durable admission pair"
                    );
                    assert_eq!(fact.operation_id, operation_id);
                    assert_eq!(fact.command_id, confirm_command_id);
                    assert_eq!(fact.code, OperationErrorCode::CleanupFailed);
                    assert_eq!(fact.action_epoch, Some(1));
                    assert_eq!(observed_branches.as_slice(), &expected_order);
                    failed_fact = Some((event.sequence, fact.settled_at_ms));
                }
                other => {
                    panic!("unexpected durable payload while waiting CleanupFailed: {other:?}")
                }
            },
            other => {
                panic!("unexpected subscription update while waiting CleanupFailed: {other:?}")
            }
        }
        assert!(
            started.elapsed() < READY_TIMEOUT,
            "timed out waiting for CleanupFailed; branches={observed_branches:?}"
        );
    }
    let (failed_sequence, settled_at_ms) = failed_fact.expect("failed fact");
    assert!(
        observed_branches
            .iter()
            .zip(expected_order.iter())
            .all(|(got, expected)| got == expected),
        "live cleanup branches must follow fixed ORDER: {observed_branches:?}"
    );

    let status = client
        .refresh_operation(operation_id)
        .await
        .expect("status transport")
        .expect("status query");
    assert_eq!(
        status,
        OperationState::Failed {
            settled_at_ms,
            code: OperationErrorCode::CleanupFailed,
        }
    );

    let mid_identity = read_identity(&lock_path).expect("mid host identity");
    assert_eq!(mid_identity.pid, original_identity.pid);
    assert_eq!(mid_identity.boot_id, original_identity.boot_id);
    assert!(
        host.try_wait()
            .expect("poll host after CleanupFailed")
            .is_none(),
        "CleanupFailed must keep the foreground host running"
    );

    let retry = client
        .confirm_host_quit(confirm_command_id, inspection.inspection_id, true)
        .await
        .expect("exact CommandId retry transport");
    assert_eq!(
        retry, confirm,
        "caller-retained CommandId retry must return struct-identical Accepted"
    );

    let client_id_b = ClientId::from_bytes(fixed_uuid_v7(0x59)).expect("client b");
    let mut client_b = connect_bounded(
        &HostClientConfig {
            named_profile: profile.clone(),
            client_build: format!("devmanager/{}", env!("CARGO_PKG_VERSION")),
            client_id: client_id_b,
            requested: CapabilitySet::from_capabilities([
                Capability::OperationSettlement,
                Capability::HostShutdown,
                Capability::PagedSnapshots,
                Capability::EventReplay,
            ]),
            limits: FrameLimits::v1_default(),
        },
        &mut host,
    )
    .await;
    let (create_b, _, _) =
        create_task_named(client_id_b, 0x5a, 0x5b, 0x5c, 0x5d, "after cleanup failed");
    let closing = client_b
        .execute_command(create_b)
        .await
        .expect("second client mutation transport");
    assert!(
        matches!(
            closing,
            CommandReceipt::Rejected {
                code: RejectionCode::Closing,
                ..
            }
        ),
        "second client mutation must Closing, got {closing:?}"
    );
    let after = client
        .inspect_host_quit()
        .await
        .expect("post-failed inspect transport")
        .expect("post-failed inspect");
    assert!(!after.confirmable);
    client_b.disconnect();

    client.disconnect();

    let fresh_id = ClientId::from_bytes(fixed_uuid_v7(0x58)).expect("fresh client");
    let mut fresh = connect_bounded(
        &HostClientConfig {
            named_profile: profile.clone(),
            client_build: format!("devmanager/{}", env!("CARGO_PKG_VERSION")),
            client_id: fresh_id,
            requested,
            limits: FrameLimits::v1_default(),
        },
        &mut host,
    )
    .await;
    assert_eq!(
        fresh.host_boot_id(),
        Some(original_identity.boot_id),
        "fresh client must attach to the same host boot"
    );

    let mut replayed_branches = Vec::new();
    let mut replayed_failures = 0u32;
    let mut batch = fresh
        .open_event_replay(pre_close_cursor)
        .await
        .expect("replay open transport")
        .expect("replay open query");
    loop {
        for event in &batch.page.events {
            match &event.payload {
                Event::HostCleanupBranchCompleted {
                    operation_id: event_op,
                    branch,
                    ..
                } if *event_op == operation_id => {
                    replayed_branches.push(*branch);
                }
                Event::OperationFailed(fact) if fact.operation_id == operation_id => {
                    assert_eq!(fact.code, OperationErrorCode::CleanupFailed);
                    assert_eq!(event.sequence, failed_sequence);
                    replayed_failures += 1;
                }
                _ => {}
            }
        }
        let Some(cursor) = batch.page.next_cursor.clone() else {
            break;
        };
        batch = fresh
            .continue_event_replay(batch.subscription_id, cursor)
            .await
            .expect("replay continue transport")
            .expect("replay continue query");
    }
    fresh
        .release_event_replay(batch.subscription_id)
        .await
        .expect("release replay transport")
        .expect("release replay query");
    assert_eq!(
        replayed_branches.as_slice(),
        &expected_order,
        "reconnect replay must yield the same four cleanup branch events"
    );
    assert_eq!(
        replayed_failures, 1,
        "reconnect replay must include CleanupFailed exactly once"
    );

    let status = fresh
        .refresh_operation(operation_id)
        .await
        .expect("fresh status transport")
        .expect("fresh status query");
    assert_eq!(
        status,
        OperationState::Failed {
            settled_at_ms,
            code: OperationErrorCode::CleanupFailed,
        }
    );

    let final_identity = read_identity(&lock_path).expect("final host identity");
    assert_eq!(final_identity.pid, original_identity.pid);
    assert_eq!(final_identity.boot_id, original_identity.boot_id);
    assert!(
        host.try_wait()
            .expect("poll host after fresh attach")
            .is_none(),
        "listener must still accept connections after CleanupFailed"
    );

    fresh.disconnect();
    let status = host
        .terminate_and_wait_bounded(TERMINATE_TIMEOUT)
        .expect("terminate exact ChildGuard");
    assert!(!status.success(), "test termination should stop the host");
    assert!(host.try_wait().expect("final host poll").is_some());
}

fn read_ordered_cleanup_branch_events(path: &Path) -> (Vec<EventId>, u64) {
    let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .expect("open sqlite read-only cleanup branch events");
    let mut stmt = conn
        .prepare(
            "SELECT sequence, event_id FROM events
             WHERE event_type = 'host.cleanup_branch_completed'
             ORDER BY sequence ASC",
        )
        .expect("prepare cleanup branch events");
    let rows = stmt
        .query_map([], |row| {
            let sequence: i64 = row.get(0)?;
            let event_id: Vec<u8> = row.get(1)?;
            Ok((sequence, event_id))
        })
        .expect("query cleanup branch events")
        .map(|row| row.expect("cleanup branch event row"))
        .collect::<Vec<_>>();
    assert_eq!(
        rows.len(),
        4,
        "seeded Ready DB must have exactly four cleanup branch events"
    );
    let final_sequence = rows[3].0 as u64;
    let ids = rows
        .into_iter()
        .map(|(_, bytes)| {
            let arr: [u8; 16] = bytes.try_into().expect("event_id bytes");
            EventId::from_bytes(arr).expect("event_id uuidv7")
        })
        .collect();
    (ids, final_sequence)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NamedPipePresence {
    Absent,
    Present,
}

/// One-shot raw Windows named-pipe probe for the profile endpoint.
///
/// Successful `CreateFileW` (handle closed immediately) and `ERROR_PIPE_BUSY`
/// both mean Present. Only `ERROR_FILE_NOT_FOUND` means Absent. Any other
/// error is a hard test failure.
fn probe_named_pipe(profile: &str) -> NamedPipePresence {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{CloseHandle, ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAGS_AND_ATTRIBUTES, FILE_GENERIC_READ,
        FILE_GENERIC_WRITE, FILE_SHARE_MODE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };

    let endpoint = pipe_endpoint_for_named_profile(profile).expect("profile endpoint");
    let wide: Vec<u16> = endpoint.encode_utf16().chain(std::iter::once(0)).collect();
    let result = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            FILE_GENERIC_READ.0 | FILE_GENERIC_WRITE.0,
            FILE_SHARE_MODE(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0),
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(FILE_ATTRIBUTE_NORMAL.0),
            None,
        )
    };
    match result {
        Ok(handle) => {
            unsafe {
                let _ = CloseHandle(handle);
            }
            NamedPipePresence::Present
        }
        Err(error) => {
            // CreateFileW surfaces Win32 codes as HRESULT_FROM_WIN32 (0x8007xxxx).
            let win32 = (error.code().0 as u32) & 0xFFFF;
            if win32 == ERROR_FILE_NOT_FOUND.0 {
                NamedPipePresence::Absent
            } else if win32 == ERROR_PIPE_BUSY.0 {
                NamedPipePresence::Present
            } else {
                panic!(
                    "unexpected named-pipe probe error for {endpoint}: {error:?} (win32={win32})"
                );
            }
        }
    }
}

/// Poll until the named pipe is Absent, or fail at `deadline`.
async fn assert_named_pipe_absent(profile: &str, deadline: Instant) {
    let endpoint = pipe_endpoint_for_named_profile(profile).expect("profile endpoint");
    loop {
        match probe_named_pipe(profile) {
            NamedPipePresence::Absent => return,
            NamedPipePresence::Present => {}
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for named pipe absence: {endpoint}"
        );
        sleep(POLL).await;
    }
}

fn count_events(path: &Path) -> i64 {
    let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .expect("open sqlite read-only event count");
    conn.query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
        .expect("count events")
}

fn count_operation_settled(path: &Path) -> i64 {
    let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .expect("open sqlite read-only settled count");
    conn.query_row(
        "SELECT COUNT(*) FROM events WHERE event_type = 'operation.settled'",
        [],
        |row| row.get(0),
    )
    .expect("count operation.settled")
}

fn read_latest_operation_settled_fact(
    path: &Path,
) -> (i64, devmanager::domain::event::OperationSettledFact) {
    use devmanager::domain::event::OperationSettledFact;

    let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .expect("open sqlite read-only settled payload");
    let (sequence, payload): (i64, Vec<u8>) = conn
        .query_row(
            "SELECT sequence, payload FROM events
             WHERE event_type = 'operation.settled'
             ORDER BY sequence DESC
             LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("latest operation.settled row");
    let fact: OperationSettledFact = rmp_serde::from_slice(&payload).expect("decode settled fact");
    (sequence, fact)
}

/// Poll the launched host until it exits without ever binding the named pipe.
///
/// While the child is alive, each loop samples the raw CreateFileW probe and
/// fails immediately if the pipe is Present. This is a sampled watch (POLL
/// cadence), not a continuous proof — HostClient Io/Unavailable/timeout is
/// never treated as evidence. On child exit, require the probe to be Absent.
async fn wait_for_exit_before_pipe_bind(
    host: &mut ChildGuard,
    profile: &str,
    overall_deadline: Instant,
) -> ExitStatus {
    let endpoint = pipe_endpoint_for_named_profile(profile).expect("profile endpoint");
    loop {
        assert_eq!(
            probe_named_pipe(profile),
            NamedPipePresence::Absent,
            "named pipe became Present during pre-bind host launch: {endpoint}"
        );
        if let Some(status) = host.try_wait().expect("poll host for pre-bind exit") {
            assert_eq!(
                probe_named_pipe(profile),
                NamedPipePresence::Absent,
                "pre-bind exit must leave named pipe absent: {endpoint}"
            );
            return status;
        }
        assert!(
            Instant::now() < overall_deadline,
            "timed out waiting for pre-bind disposition exit"
        );
        sleep(POLL).await;
    }
}

/// Retry only exact HostLock AlreadyRunning (exit 75) or known transient stale-prior
/// verification diagnostics from HostLock after a killed child handle is released.
fn is_retryable_host_lock_contention(status: ExitStatus, stderr: &str) -> bool {
    if status.code() == Some(i32::from(HOST_EXIT_ALREADY_RUNNING)) {
        return true;
    }
    stderr.contains("unable to verify creation ticks for prior host pid")
        || stderr.contains("unable to verify executable path for prior host pid")
}

/// Spawn isolated hosts until one completes the real HostLock acquire → pre-bind
/// disposition path successfully. Retries only narrow HostLock contention/stale-prior
/// shapes after releasing each exited child handle. Never edits host.lock and never
/// HostLock::acquire from the test process. While waiting, the raw named-pipe probe
/// must stay Absent (sampled at POLL); a Present pipe fails the launch immediately.
async fn spawn_and_require_prebind_exit(config_base: &Path, profile: &str) {
    let overall_deadline = Instant::now() + READY_TIMEOUT;
    loop {
        assert!(
            Instant::now() < overall_deadline,
            "timed out retrying real host relaunch for pre-bind disposition exit"
        );
        let mut host = ChildGuard::spawn(host_command(config_base, profile));
        let status = wait_for_exit_before_pipe_bind(&mut host, profile, overall_deadline).await;
        if status.success() {
            assert!(host.try_wait().expect("pre-bind success poll").is_some());
            host.release_exited_process_handle();
            return;
        }
        let stderr = host.take_exited_stderr();
        let diagnostics = format!("{status}; stderr={stderr:?}");
        if !is_retryable_host_lock_contention(status, &stderr) {
            panic!(
                "non-retryable pre-bind host failure (disposition/settle/startup must fail closed): {diagnostics}"
            );
        }
        host.release_exited_process_handle();
        sleep(POLL).await;
    }
}

/// Seed all-success Accepted/Ready offline, then prove Ready settle and Closed
/// no-op both exit successfully before HelloListener::bind (raw pipe stays Absent).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ready_restart_settles_before_bind_then_closed_exits_without_event() {
    use devmanager::domain::host::{HostCleanupBranch, HostCleanupBranchOutcome};
    use devmanager::host::{HostCleanupProgress, HostCleanupWorker, HostRestartDisposition};
    use devmanager::kernel::CommandBus;

    let config_base = TempDir::new().expect("process-unique config base");
    let profile = unique_profile();
    let paths = isolated_paths(&config_base, &profile);

    // Seed the exact all-success Accepted/Ready database in-process (no live-host race).
    let confirm_command_id =
        CommandId::from_bytes(fixed_uuid_v7(0x61)).expect("confirm command id");
    let client_id = ClientId::from_bytes(fixed_uuid_v7(0x60)).expect("client");
    let operation_id = {
        fs::create_dir_all(&paths.root).expect("create profile root for seed");
        let mut bus = CommandBus::open(&paths.database).expect("open seed bus");
        let inspection = bus.inspect_host_quit().expect("inspect");
        let confirm = bus
            .execute(CommandEnvelope {
                command_id: confirm_command_id,
                client_id,
                task_id: None,
                issued_at_ms: 1_725_000_000_200,
                expected_task_revision: None,
                command: Command::ConfirmHostQuit(ConfirmHostQuitIntent {
                    inspection_id: inspection.inspection_id,
                    allow_uninspected_worktrees: true,
                }),
            })
            .expect("confirm quit");
        let operation_id = match confirm {
            CommandReceipt::Accepted {
                operation_id,
                task_revision: None,
                ..
            } => operation_id,
            other => panic!("expected taskless Accepted, got {other:?}"),
        };
        for branch in HostCleanupBranch::ORDER {
            assert_eq!(
                HostCleanupWorker::run_once(&mut bus).expect("cleanup branch"),
                HostCleanupProgress::BranchCompleted {
                    operation_id,
                    action_epoch: 1,
                    branch,
                    outcome: HostCleanupBranchOutcome::succeeded(),
                }
            );
        }
        assert_eq!(
            HostCleanupWorker::run_once(&mut bus).expect("ready"),
            HostCleanupProgress::ReadyToExit {
                operation_id,
                action_epoch: 1,
            }
        );
        assert_eq!(
            count_operation_settled(&paths.database),
            0,
            "Ready seed must not yet have OperationSettled"
        );
        assert_eq!(
            HostCleanupWorker::restart_disposition(&bus).expect("ready disposition"),
            HostRestartDisposition::ReadyToArmAndSettle {
                operation_id,
                action_epoch: 1,
            }
        );
        drop(bus);
        operation_id
    };
    let (branch_event_ids, final_branch_sequence) =
        read_ordered_cleanup_branch_events(&paths.database);
    let events_at_ready = count_events(&paths.database);

    // Ready relaunch: settle exactly once, exit before bind, release HostLock.
    spawn_and_require_prebind_exit(config_base.path(), &profile).await;

    assert_eq!(
        count_events(&paths.database),
        events_at_ready + 1,
        "Ready relaunch must append exactly one durable event"
    );
    assert_eq!(count_operation_settled(&paths.database), 1);
    let (settled_sequence, settled_fact) = read_latest_operation_settled_fact(&paths.database);
    assert_eq!(settled_fact.operation_id, operation_id);
    assert_eq!(settled_fact.command_id, confirm_command_id);
    assert_eq!(settled_fact.action_epoch, Some(1));
    assert_eq!(
        settled_fact.result_event_ids, branch_event_ids,
        "settled result_event_ids must equal the exact ordered cleanup branch event ids"
    );
    assert!(settled_fact.settled_at_ms > 0);
    assert_eq!(
        settled_sequence as u64,
        final_branch_sequence + 1,
        "OperationSettled must be the immediate successor of the fourth cleanup branch"
    );

    {
        let bus = CommandBus::open(&paths.database).expect("open bus after settle");
        assert_eq!(
            bus.operation_status(operation_id)
                .expect("operation status")
                .expect("quit operation present"),
            OperationState::Settled {
                settled_at_ms: settled_fact.settled_at_ms,
                result_event_ids: settled_fact.result_event_ids.clone(),
            }
        );
        assert_eq!(
            HostCleanupWorker::restart_disposition(&bus).expect("closed disposition"),
            HostRestartDisposition::Closed {
                operation_id,
                action_epoch: 1,
                settled_at_ms: settled_fact.settled_at_ms,
            }
        );
    }

    let events_after_settle = count_events(&paths.database);

    // Closed relaunch: exit before bind with no additional event; lock free again.
    spawn_and_require_prebind_exit(config_base.path(), &profile).await;

    assert_eq!(
        count_events(&paths.database),
        events_after_settle,
        "Closed relaunch must append no event"
    );
    assert_eq!(count_operation_settled(&paths.database), 1);
    let (again_sequence, again_fact) = read_latest_operation_settled_fact(&paths.database);
    assert_eq!(again_sequence, settled_sequence);
    assert_eq!(again_fact, settled_fact);

    // Second Closed pass proves the prior Closed exit released HostLock.
    spawn_and_require_prebind_exit(config_base.path(), &profile).await;
    assert_eq!(count_events(&paths.database), events_after_settle);
    assert_eq!(count_operation_settled(&paths.database), 1);
}
