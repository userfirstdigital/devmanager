//! Named-pipe ClientHello/ServerHello handshake acceptance.
//!
//! Fixtures use a process-unique named profile and a TempDir root only as
//! isolation evidence. They must never resolve installed app-data paths or the
//! production pipe namespace.

#![cfg(windows)]

use std::os::windows::fs::OpenOptionsExt;
use std::path::Path;
use std::time::Duration;

use tempfile::TempDir;
use tokio::io::AsyncWriteExt;
use tokio::net::windows::named_pipe::ClientOptions;
use tokio::sync::oneshot;
use tokio::time::timeout;
use uuid::Uuid;

use devmanager::client::{
    connect, perform_client_hello, HostClient, HostClientConfig, TrackedOperation,
};
use devmanager::domain::command::{Command, CommandEnvelope, CommandReceipt, CreateTaskIntent};
use devmanager::domain::id::{CommandId, EnvironmentId, ProjectId, RequestId, TaskId};
use devmanager::domain::operation::OperationState;
use devmanager::domain::query::{Query, QueryEnvelope, QueryOutcome, QueryResult};
use devmanager::domain::task::{
    ReviewReadiness, TaskActivity, TaskAssignment, TaskAttention, TaskConnectivity, WorkspaceRef,
};
use devmanager::domain::ClientId;
use devmanager::host::{
    pipe_endpoint_for_named_profile, profile_fingerprint_for_named_profile, AcceptHelloConfig,
    HelloListener, IpcError,
};
use devmanager::kernel::CommandBus;
use devmanager::protocol::{
    Capability, CapabilitySet, ClientHello, FrameLimits, PhysicalFrameError, ProtocolVersion,
    MAX_PHYSICAL_FRAME_BYTES,
};

const OUTER_TIMEOUT: Duration = Duration::from_secs(30);

fn isolation_root() -> TempDir {
    TempDir::new().expect("temp isolation root")
}

fn assert_isolated_from_app_data(root: &Path) {
    let temp = std::env::temp_dir();
    assert!(
        root.starts_with(&temp),
        "fixture root must live under the process temp dir: {root:?} vs {temp:?}"
    );
    if let Ok(appdata) = std::env::var("APPDATA") {
        let appdata = Path::new(&appdata);
        assert!(
            !root.starts_with(appdata),
            "fixture root must stay outside APPDATA: {root:?}"
        );
        assert!(
            !root.starts_with(appdata.join("com.userfirst.devmanager")),
            "fixture root must never resolve production config namespace"
        );
    }
}

fn unique_profile(label: &str) -> String {
    format!(
        "pipe{label}{}{}",
        std::process::id(),
        Uuid::now_v7().simple()
    )
}

fn protocol_client_id(tail: u8) -> ClientId {
    let mut bytes = [0_u8; 16];
    bytes[0] = 0x01;
    bytes[6] = 0x70;
    bytes[8] = 0x80;
    bytes[15] = tail;
    ClientId::from_bytes(bytes).expect("client id")
}

fn fixed_uuid_v7(tail: u8) -> [u8; 16] {
    [
        0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        tail,
    ]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pipe_request_create_retry_then_task_read() {
    let root = isolation_root();
    assert_isolated_from_app_data(root.path());
    let db_path = root.path().join("kernel.sqlite3");

    let profile_uuid = Uuid::now_v7();
    let profile = format!(
        "pipe{}{}{}",
        "rq",
        std::process::id(),
        profile_uuid.simple()
    );
    assert!(
        profile.contains(&std::process::id().to_string()),
        "profile must include pid"
    );
    assert!(
        profile.ends_with(&profile_uuid.simple().to_string()),
        "profile must include full uuid simple form"
    );
    assert_eq!(profile_uuid.get_version_num(), 7);

    let fingerprint = profile_fingerprint_for_named_profile(&profile).expect("fingerprint");
    let endpoint = pipe_endpoint_for_named_profile(&profile).expect("endpoint");
    let host_boot_id = Uuid::now_v7();
    let client_id = protocol_client_id(0x51);
    let task = TaskId::from_bytes(fixed_uuid_v7(0x52)).expect("task");
    let command_id = CommandId::from_bytes(fixed_uuid_v7(0x53)).expect("command");
    let request_id = RequestId::from_bytes(fixed_uuid_v7(0x54)).expect("request");

    let create = CommandEnvelope {
        command_id,
        client_id,
        task_id: None,
        issued_at_ms: 1_725_000_000_100,
        expected_task_revision: None,
        command: Command::CreateTask(CreateTaskIntent {
            id: task,
            environment_id: EnvironmentId::from_bytes(fixed_uuid_v7(0x55)).expect("env"),
            title: "Pipe create retry".into(),
            description: None,
            project_id: ProjectId::from_bytes(fixed_uuid_v7(0x56)).expect("project"),
            workspace: WorkspaceRef::Main,
            assignment: TaskAssignment::LocalOwner,
            created_at_ms: 1_725_000_000_000,
            connectivity: TaskConnectivity::Connected,
            attention: TaskAttention::None,
            activity: TaskActivity::Idle,
            review_readiness: ReviewReadiness::NotReady,
        }),
    };

    let listener = HelloListener::bind(
        &profile,
        AcceptHelloConfig {
            host_boot_id,
            server_build: "devmanager-host/0.4.2".to_string(),
            supported: CapabilitySet::from_capabilities([Capability::OperationSettlement]),
            local_limits: FrameLimits::v1_default(),
        },
    )
    .expect("bind");

    let server_task = tokio::spawn(async move {
        let mut connection = listener.accept().await.expect("accept connection");
        let mut bus = CommandBus::open(&db_path).expect("open host command bus");
        connection
            .serve_request(&mut bus)
            .await
            .expect("serve create");
        connection
            .serve_request(&mut bus)
            .await
            .expect("serve retry");
        connection
            .serve_request(&mut bus)
            .await
            .expect("serve task query");
        connection.accepted_hello()
    });

    let client_task = tokio::spawn({
        let endpoint = endpoint.clone();
        let create = create.clone();
        async move {
            let hello = ClientHello::new(
                "devmanager/0.4.2",
                client_id,
                fingerprint,
                CapabilitySet::from_capabilities([Capability::OperationSettlement]),
                FrameLimits::v1_default(),
            )
            .expect("client hello");
            let mut client = connect(&endpoint, &hello).await.expect("connect");
            assert_eq!(client.client_id(), client_id);

            let first = client
                .execute_command(create.clone())
                .await
                .expect("create command");
            let retry = client
                .execute_command(create)
                .await
                .expect("retry identical command");
            assert_eq!(
                first, retry,
                "identical create must return identical receipt"
            );
            let operation_id = first
                .accepted_operation_id()
                .expect("create must be accepted");
            assert_eq!(
                retry.accepted_operation_id(),
                Some(operation_id),
                "retry must preserve OperationId"
            );

            let reply = client
                .query(QueryEnvelope {
                    request_id,
                    client_id,
                    task_id: Some(task),
                    query: Query::TaskSnapshot,
                })
                .await
                .expect("task snapshot query");
            assert_eq!(reply.request_id, request_id);
            match reply.outcome {
                QueryOutcome::Ok(QueryResult::TaskSnapshot { snapshot }) => {
                    assert_eq!(snapshot.task.id, task);
                    assert_eq!(snapshot.task.title, "Pipe create retry");
                    assert_eq!(snapshot.task.revision, 1);
                }
                other => panic!("expected task snapshot, got {other:?}"),
            }
            (first, client.server_hello().clone())
        }
    });

    let accepted = timeout(OUTER_TIMEOUT, server_task)
        .await
        .expect("server join timeout")
        .expect("server task");
    let (receipt, server_hello) = timeout(OUTER_TIMEOUT, client_task)
        .await
        .expect("client join timeout")
        .expect("client task");

    assert_eq!(accepted.client_id, client_id);
    assert_eq!(server_hello.host_boot_id, host_boot_id);
    assert!(matches!(receipt, CommandReceipt::Accepted { .. }));
    drop(root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pipe_hello_round_trip_negotiates_minimums() {
    let root = isolation_root();
    assert_isolated_from_app_data(root.path());

    let profile = unique_profile("rt");
    let fingerprint = profile_fingerprint_for_named_profile(&profile).expect("fingerprint");
    let endpoint = pipe_endpoint_for_named_profile(&profile).expect("endpoint");
    assert!(
        endpoint.starts_with(r"\\.\pipe\devmanager-"),
        "endpoint must use DevManager product namespace: {endpoint}"
    );
    assert_eq!(
        endpoint,
        pipe_endpoint_for_named_profile(&profile.to_ascii_uppercase()).expect("normalized"),
        "endpoint identity must be independent of profile case"
    );
    assert_eq!(
        fingerprint,
        profile_fingerprint_for_named_profile(&profile.to_ascii_uppercase()).expect("case"),
    );
    let production_endpoint =
        pipe_endpoint_for_named_profile("production").expect("named production label is distinct");
    assert_ne!(
        endpoint, production_endpoint,
        "process-unique profile must not collide with a static production label endpoint"
    );

    let host_boot_id = Uuid::now_v7();
    let server_build = "devmanager-host/0.4.2";
    let supported = CapabilitySet::from_capabilities([
        Capability::PagedSnapshots,
        Capability::OperationSettlement,
        Capability::EventReplay,
    ]);
    let server_limits = FrameLimits::v1_default();

    let listener = HelloListener::bind(
        &profile,
        AcceptHelloConfig {
            host_boot_id,
            server_build: server_build.to_string(),
            supported,
            local_limits: server_limits,
        },
    )
    .expect("bind listener");
    assert_eq!(listener.endpoint(), endpoint);
    assert_eq!(listener.expected_fingerprint(), fingerprint);

    let client_id = protocol_client_id(0x42);
    let requested = CapabilitySet::from_capabilities([
        Capability::PagedSnapshots,
        Capability::EventReplay,
        Capability::ChunkResume,
    ]);
    let offered_limits = FrameLimits {
        max_physical_frame_bytes: 64 * 1024,
        max_reassembled_message_bytes: 32 * 1024 * 1024,
        max_page_items: 250,
        max_page_encoded_bytes: 1024 * 1024,
    };
    let hello = ClientHello::new(
        "devmanager/0.4.2",
        client_id,
        fingerprint,
        requested,
        offered_limits,
    )
    .expect("client hello");

    let server_task = tokio::spawn(async move { listener.accept_hello().await });
    let client_task = tokio::spawn({
        let endpoint = endpoint.clone();
        async move { perform_client_hello(&endpoint, &hello).await }
    });

    let accepted = timeout(OUTER_TIMEOUT, server_task)
        .await
        .expect("server join timeout")
        .expect("server task")
        .expect("accept hello");
    let server_hello = timeout(OUTER_TIMEOUT, client_task)
        .await
        .expect("client join timeout")
        .expect("client task")
        .expect("client hello");

    assert_eq!(accepted.client_id, client_id);
    assert_eq!(accepted.negotiated.client_id, client_id);
    assert_eq!(accepted.negotiated.version, ProtocolVersion::current());
    assert_eq!(
        accepted.negotiated.capabilities,
        CapabilitySet::from_capabilities([Capability::PagedSnapshots, Capability::EventReplay])
    );
    assert_eq!(
        accepted.negotiated.limits,
        FrameLimits {
            max_physical_frame_bytes: 64 * 1024,
            max_reassembled_message_bytes: 16 * 1024 * 1024,
            max_page_items: 250,
            max_page_encoded_bytes: 512 * 1024,
        }
    );

    assert_eq!(
        server_hello.protocol_major,
        ProtocolVersion::current().major
    );
    assert_eq!(
        server_hello.protocol_minor,
        ProtocolVersion::current().minor
    );
    assert_eq!(server_hello.server_build, server_build);
    assert_eq!(server_hello.host_boot_id, host_boot_id);
    assert_eq!(server_hello.profile_fingerprint, fingerprint);
    assert_ne!(server_hello.connection_id, Uuid::nil());
    assert_eq!(server_hello.connection_id.get_version_num(), 7);
    assert_eq!(
        server_hello.granted,
        CapabilitySet::from_capabilities([Capability::PagedSnapshots, Capability::EventReplay])
    );
    assert_eq!(server_hello.limits, accepted.negotiated.limits);
    assert_eq!(accepted.server_hello, server_hello);

    drop(root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pipe_hello_rejects_wrong_profile_fingerprint() {
    let bound_profile = unique_profile("ok");
    let wrong_profile = unique_profile("bad");
    let wrong_fingerprint =
        profile_fingerprint_for_named_profile(&wrong_profile).expect("wrong fingerprint");
    assert_ne!(
        profile_fingerprint_for_named_profile(&bound_profile).expect("bound"),
        wrong_fingerprint
    );

    let listener = HelloListener::bind(
        &bound_profile,
        AcceptHelloConfig {
            host_boot_id: Uuid::now_v7(),
            server_build: "devmanager-host/0.4.2".to_string(),
            supported: CapabilitySet::from_capabilities([Capability::PagedSnapshots]),
            local_limits: FrameLimits::v1_default(),
        },
    )
    .expect("bind");
    let endpoint = listener.endpoint().to_string();

    let hello = ClientHello::new(
        "devmanager/0.4.2",
        protocol_client_id(0x55),
        wrong_fingerprint,
        CapabilitySet::from_capabilities([Capability::PagedSnapshots]),
        FrameLimits::v1_default(),
    )
    .expect("valid hello with wrong fingerprint");

    let server_task = tokio::spawn(async move { listener.accept_hello().await });
    let client_task = tokio::spawn(async move { perform_client_hello(&endpoint, &hello).await });

    let server_result = timeout(OUTER_TIMEOUT, server_task)
        .await
        .expect("server join timeout")
        .expect("server task");
    assert!(
        matches!(server_result, Err(IpcError::ProfileMismatch)),
        "expected ProfileMismatch, got {server_result:?}"
    );

    let client_result = timeout(OUTER_TIMEOUT, client_task)
        .await
        .expect("client join timeout")
        .expect("client task");
    assert!(
        client_result.is_err(),
        "client must not observe a successful ServerHello after profile mismatch"
    );
}

#[test]
fn pipe_endpoints_differ_for_distinct_profiles() {
    let a = pipe_endpoint_for_named_profile("alpha-one").expect("alpha");
    let b = pipe_endpoint_for_named_profile("beta-two").expect("beta");
    assert_ne!(a, b);
    assert_ne!(
        profile_fingerprint_for_named_profile("alpha-one").unwrap(),
        profile_fingerprint_for_named_profile("beta-two").unwrap()
    );
    assert!(matches!(
        pipe_endpoint_for_named_profile(""),
        Err(IpcError::InvalidProfile(_))
    ));
    assert!(matches!(
        pipe_endpoint_for_named_profile(r"C:\evil"),
        Err(IpcError::InvalidProfile(_))
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pipe_oversized_header_is_rejected_before_payload_allocation() {
    let profile = unique_profile("ov");
    let host_boot_id = Uuid::now_v7();
    let listener = HelloListener::bind(
        &profile,
        AcceptHelloConfig {
            host_boot_id,
            server_build: "devmanager-host/0.4.2".to_string(),
            supported: CapabilitySet::empty(),
            local_limits: FrameLimits::v1_default(),
        },
    )
    .expect("bind");
    let endpoint = listener.endpoint().to_string();

    let server_task = tokio::spawn(async move { listener.accept_hello().await });

    let mut client = ClientOptions::new().open(&endpoint).expect("open client");
    let oversized = (MAX_PHYSICAL_FRAME_BYTES + 1).to_be_bytes();
    client
        .write_all(&oversized)
        .await
        .expect("write oversized header");
    client.flush().await.expect("flush oversized header");
    drop(client);

    let result = timeout(OUTER_TIMEOUT, server_task)
        .await
        .expect("server join timeout")
        .expect("server task");
    match result {
        Err(IpcError::Frame(PhysicalFrameError::Oversized { declared, maximum })) => {
            assert_eq!(declared, u64::from(MAX_PHYSICAL_FRAME_BYTES) + 1);
            assert_eq!(maximum, MAX_PHYSICAL_FRAME_BYTES);
        }
        other => panic!("expected oversized frame rejection, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_reconnect_resolves_tracked_operation_while_host_database_is_locked() {
    let root = isolation_root();
    assert_isolated_from_app_data(root.path());
    let db_path = root.path().join("kernel.sqlite3");

    let profile = unique_profile("hc");
    let endpoint = pipe_endpoint_for_named_profile(&profile).expect("endpoint");
    let host_boot_id = Uuid::now_v7();
    let client_id = protocol_client_id(0x61);
    let task = TaskId::from_bytes(fixed_uuid_v7(0x62)).expect("task");
    let command_id = CommandId::from_bytes(fixed_uuid_v7(0x63)).expect("command");
    let requested = CapabilitySet::from_capabilities([Capability::OperationSettlement]);
    let hello_config = AcceptHelloConfig {
        host_boot_id,
        server_build: "devmanager-host/0.4.2".to_string(),
        supported: requested,
        local_limits: FrameLimits::v1_default(),
    };

    let create = CommandEnvelope {
        command_id,
        client_id,
        task_id: None,
        issued_at_ms: 1_725_000_000_100,
        expected_task_revision: None,
        command: Command::CreateTask(CreateTaskIntent {
            id: task,
            environment_id: EnvironmentId::from_bytes(fixed_uuid_v7(0x64)).expect("env"),
            title: "HostClient reconnect settle".into(),
            description: None,
            project_id: ProjectId::from_bytes(fixed_uuid_v7(0x65)).expect("project"),
            workspace: WorkspaceRef::Main,
            assignment: TaskAssignment::LocalOwner,
            created_at_ms: 1_725_000_000_000,
            connectivity: TaskConnectivity::Connected,
            attention: TaskAttention::None,
            activity: TaskActivity::Idle,
            review_readiness: ReviewReadiness::NotReady,
        }),
    };

    let (bound1_tx, bound1_rx) = oneshot::channel::<()>();
    let (hello1_tx, hello1_rx) = oneshot::channel();
    let db_path_host1 = db_path.clone();
    let profile_host1 = profile.clone();
    let hello_config_host1 = hello_config.clone();
    let host1 = tokio::spawn(async move {
        let listener = HelloListener::bind(&profile_host1, hello_config_host1).expect("bind1");
        let _ = bound1_tx.send(());
        let mut connection = listener.accept().await.expect("accept1");
        let accepted = connection.accepted_hello();
        let mut bus = CommandBus::open(&db_path_host1).expect("open bus1");
        connection
            .serve_request(&mut bus)
            .await
            .expect("serve create");
        let _ = hello1_tx.send(accepted);
        drop(connection);
        drop(bus);
    });

    timeout(OUTER_TIMEOUT, bound1_rx)
        .await
        .expect("bound1 timeout")
        .expect("bound1");

    let mut client = timeout(OUTER_TIMEOUT, async {
        HostClient::connect(HostClientConfig {
            named_profile: profile.clone(),
            client_build: "devmanager/0.4.2".to_string(),
            client_id,
            requested,
            limits: FrameLimits::v1_default(),
        })
        .await
    })
    .await
    .expect("connect timeout")
    .expect("HostClient connect");

    assert_eq!(client.client_id(), client_id);
    assert_eq!(client.endpoint(), endpoint);
    assert_eq!(client.granted_capabilities(), requested);

    let receipt = timeout(OUTER_TIMEOUT, client.execute_command(create))
        .await
        .expect("execute timeout")
        .expect("execute create");
    let operation_id = match receipt {
        CommandReceipt::Accepted {
            operation_id,
            command_id: accepted_command,
            ..
        } => {
            assert_eq!(accepted_command, command_id);
            operation_id
        }
        other => panic!("expected Accepted receipt, got {other:?}"),
    };
    assert!(matches!(
        client.tracked_operation(operation_id),
        Some(TrackedOperation::Pending {
            command_id: tracked_command
        }) if *tracked_command == command_id
    ));

    let accepted1 = timeout(OUTER_TIMEOUT, hello1_rx)
        .await
        .expect("hello1 timeout")
        .expect("hello1");
    assert_eq!(accepted1.client_id, client_id);
    let connection_id_1 = accepted1.server_hello.connection_id;
    assert_eq!(client.connection_id(), connection_id_1);
    timeout(OUTER_TIMEOUT, host1)
        .await
        .expect("host1 join timeout")
        .expect("host1");

    client.disconnect();
    assert!(
        matches!(
            client.tracked_operation(operation_id),
            Some(TrackedOperation::Pending { .. })
        ),
        "disconnect must preserve pending tracking"
    );

    let (bound2_tx, bound2_rx) = oneshot::channel::<()>();
    let (hello2_tx, hello2_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel::<()>();
    let db_path_host2 = db_path.clone();
    let profile_host2 = profile.clone();
    let hello_config_host2 = hello_config.clone();
    let host2 = tokio::spawn(async move {
        let listener = HelloListener::bind(&profile_host2, hello_config_host2).expect("bind2");
        let _ = bound2_tx.send(());
        let mut connection = listener.accept().await.expect("accept2");
        let accepted = connection.accepted_hello();
        let _ = hello2_tx.send(accepted);
        release_rx.await.expect("release signal");
        let mut bus = CommandBus::open(&db_path_host2).expect("open bus2 after release");
        connection
            .serve_request(&mut bus)
            .await
            .expect("serve operation status");
    });

    timeout(OUTER_TIMEOUT, bound2_rx)
        .await
        .expect("bound2 timeout")
        .expect("bound2");

    // This is a focused attach-path proof: reconnect does not need the active
    // host database. The later child-process gate observes canonical client
    // handles across the complete lifecycle.
    let canary = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(&db_path)
        .expect("exclusive hold on kernel.sqlite3");

    timeout(OUTER_TIMEOUT, client.reconnect())
        .await
        .expect("reconnect timeout")
        .expect("reconnect while kernel db exclusively held");

    let accepted2 = timeout(OUTER_TIMEOUT, hello2_rx)
        .await
        .expect("hello2 timeout")
        .expect("hello2");
    assert_eq!(accepted2.client_id, client_id);
    assert_eq!(accepted1.client_id, accepted2.client_id);
    let connection_id_2 = accepted2.server_hello.connection_id;
    assert_ne!(connection_id_1, connection_id_2);
    assert_eq!(client.client_id(), client_id);
    assert_eq!(client.connection_id(), connection_id_2);
    assert_eq!(client.granted_capabilities(), requested);
    assert!(matches!(
        client.tracked_operation(operation_id),
        Some(TrackedOperation::Pending { .. })
    ));

    drop(canary);
    let _ = release_tx.send(());

    let state = timeout(OUTER_TIMEOUT, client.refresh_operation(operation_id))
        .await
        .expect("refresh timeout")
        .expect("refresh operation")
        .expect("operation status query outcome");
    assert!(
        matches!(state, OperationState::Settled { .. }),
        "expected Settled after refresh, got {state:?}"
    );
    assert!(matches!(
        client.tracked_operation(operation_id),
        Some(TrackedOperation::Resolved {
            command_id: tracked_command,
            state: OperationState::Settled { .. },
        }) if *tracked_command == command_id
    ));

    timeout(OUTER_TIMEOUT, host2)
        .await
        .expect("host2 join timeout")
        .expect("host2");

    drop(root);
}
