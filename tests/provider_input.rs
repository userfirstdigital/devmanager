//! Task 4.7: provider input, questions, approvals, and turn control.

use devmanager::client::action::{
    catalog, ActionRisk, ActionScope, ACTION_PROVIDER_ANSWER_QUESTION,
    ACTION_PROVIDER_NEW_CONVERSATION, ACTION_PROVIDER_QUEUE_FOLLOW_UP,
    ACTION_PROVIDER_RESOLVE_APPROVAL, ACTION_PROVIDER_SEND_NOW, ACTION_PROVIDER_STEER_CURRENT_TURN,
    ACTION_PROVIDER_STOP_TURN,
};
use devmanager::domain::{
    decide, AgentSessionId, ApprovalId, ClientId, Command, CommandEnvelope, CommandId, OperationId,
    PresentProviderApprovalIntent, PresentProviderQuestionIntent, ProviderInputAction,
    ProviderKind, ProviderSessionId, ProviderWaitFence, QuestionId, RejectionCode,
    SubmitProviderInputIntent, TaskId, TurnId,
};

fn fixed_uuid_v7(tail: u8) -> [u8; 16] {
    [
        0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        tail,
    ]
}

fn seed_open_task_with_agent(
    path: &std::path::Path,
    tail: u8,
) -> (
    devmanager::kernel::CommandBus,
    TaskId,
    AgentSessionId,
    u64,
    u64,
    ClientId,
) {
    seed_open_task_with_agent_runtime(path, tail, true)
}

fn seed_open_task_without_provider_runtime(
    path: &std::path::Path,
    tail: u8,
) -> (
    devmanager::kernel::CommandBus,
    TaskId,
    AgentSessionId,
    u64,
    u64,
    ClientId,
) {
    seed_open_task_with_agent_runtime(path, tail, false)
}

fn seed_open_task_with_agent_runtime(
    path: &std::path::Path,
    tail: u8,
    bind_provider_runtime: bool,
) -> (
    devmanager::kernel::CommandBus,
    TaskId,
    AgentSessionId,
    u64,
    u64,
    ClientId,
) {
    use devmanager::config::paths::{resolve_app_paths, AppProfile, BuildKind};
    use devmanager::config::{ConfigCommand, ConfigStore, Project};
    use devmanager::domain::command::CommandReceipt;
    use devmanager::domain::{
        AgentRole, AgentSessionFacts, AgentSessionLifecycle, CreateTaskRequestIntent,
        EnvironmentId, ProjectId, TaskActivity, TaskAssignment, TaskAttention, TaskConnectivity,
        TaskLifecycle,
    };
    use devmanager::host::HostRequestExecutor;
    use devmanager::protocol::{
        CapabilitySet, ClientRequest, FrameLimits, NegotiatedParameters, ProtocolVersion,
        ServerMessage,
    };
    use devmanager::workspace::{WorkspaceProjectRoots, WorkspaceRequest};

    let client_id = ClientId::from_bytes(fixed_uuid_v7(tail)).expect("client");
    let task_id = TaskId::from_bytes(fixed_uuid_v7(tail + 1)).expect("task");
    let agent_session_id = AgentSessionId::from_bytes(fixed_uuid_v7(tail + 2)).expect("agent");
    let profile = AppProfile::named(&format!("providerinput{tail:02x}"))
        .expect("isolated provider-input profile");
    let paths = resolve_app_paths(
        path.parent().expect("provider-input database parent"),
        profile,
        BuildKind::Debug,
    )
    .expect("resolve isolated provider-input paths");
    std::fs::create_dir_all(&paths.root).expect("create isolated provider-input root");
    let configured_id = ProjectId::from_bytes(fixed_uuid_v7(tail + 5))
        .expect("configured project")
        .to_string();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("provider-input host runtime");
    let create = runtime.block_on(async {
        let bus = devmanager::kernel::CommandBus::open(path).expect("open seed command bus");
        let mut config = ConfigStore::open_host(&paths).expect("open isolated host config");
        config
            .execute(
                config.snapshot().revision,
                ConfigCommand::CreateProject {
                    project: Project {
                        id: configured_id.clone(),
                        name: "Provider input fixture".into(),
                        root_path: paths.root.to_string_lossy().into_owned(),
                        created_at: "now".into(),
                        updated_at: "now".into(),
                        ..Project::default()
                    },
                },
            )
            .expect("persist isolated provider-input project");
        let revision = config.snapshot().revision;
        let roots = WorkspaceProjectRoots::from_host_config_store(&mut config, revision, 1, 1)
            .expect("issue isolated provider-input roots");
        let project_id = roots
            .project_id_for_config_id(&configured_id)
            .expect("resolve provider-input project id");
        let (requests, executor) =
            HostRequestExecutor::start_supervised_with_config_store(bus, config, &paths.root)
                .expect("start configured provider-input host");
        let response = requests
            .execute(
                NegotiatedParameters {
                    version: ProtocolVersion::current(),
                    client_id,
                    capabilities: CapabilitySet::empty(),
                    limits: FrameLimits::v1_default(),
                },
                ClientRequest::Command(CommandEnvelope {
                    command_id: CommandId::from_bytes(fixed_uuid_v7(tail + 3)).expect("create cmd"),
                    client_id,
                    task_id: None,
                    issued_at_ms: 1_725_000_000_100,
                    expected_task_revision: None,
                    command: Command::CreateTaskV2(CreateTaskRequestIntent {
                        id: task_id,
                        environment_id: EnvironmentId::from_bytes(fixed_uuid_v7(tail + 4))
                            .expect("environment"),
                        title: "Provider input".into(),
                        description: None,
                        project_id,
                        workspace: WorkspaceRequest::confirmed_external(&paths.root),
                        primary_provider: None,
                        defer_primary_provider_start: false,
                        assignment: TaskAssignment::LocalOwner,
                        created_at_ms: 1_725_000_000_000,
                        connectivity: TaskConnectivity::Connected,
                        attention: TaskAttention::None,
                        activity: TaskActivity::Idle,
                        review_readiness: devmanager::domain::ReviewReadiness::NotReady,
                    }),
                }),
            )
            .await
            .expect("create task through configured host");
        drop(requests);
        let _ = executor
            .join
            .await
            .expect("configured provider-input host join");
        let ServerMessage::CommandReceipt(receipt) = response else {
            panic!("provider-input task create must return a command receipt");
        };
        receipt
    });
    let CommandReceipt::Accepted {
        task_revision: Some(revision),
        ..
    } = create
    else {
        panic!("expected accepted create, got {create:?}");
    };
    let mut bus = devmanager::kernel::CommandBus::open(path).expect("reopen seeded command bus");
    let register = bus
        .execute(CommandEnvelope {
            command_id: CommandId::from_bytes(fixed_uuid_v7(tail + 6)).expect("register cmd"),
            client_id,
            task_id: Some(task_id),
            issued_at_ms: 1_725_000_000_110,
            expected_task_revision: Some(revision),
            command: Command::RegisterAgentSession {
                agent: AgentSessionFacts {
                    id: agent_session_id,
                    task_id,
                    role: AgentRole::Primary,
                    provider_kind: ProviderKind::Codex,
                    provider_session_id: bind_provider_runtime.then(|| {
                        devmanager::domain::ProviderSessionId::new(format!(
                            "codex-session-{tail:02x}"
                        ))
                        .expect("provider session")
                    }),
                    lifecycle: AgentSessionLifecycle::Open,
                    runtime_generation: 3,
                    revision: 0,
                },
            },
        })
        .expect("register agent");
    let CommandReceipt::Accepted {
        task_revision: Some(next_revision),
        ..
    } = register
    else {
        panic!("expected accepted register, got {register:?}");
    };
    let set_primary = bus
        .execute(CommandEnvelope {
            command_id: CommandId::from_bytes(fixed_uuid_v7(tail + 7)).expect("primary cmd"),
            client_id,
            task_id: Some(task_id),
            issued_at_ms: 1_725_000_000_120,
            expected_task_revision: Some(next_revision),
            command: Command::SetPrimaryAgent { agent_session_id },
        })
        .expect("set primary agent");
    let CommandReceipt::Accepted {
        task_revision: Some(primary_revision),
        ..
    } = set_primary
    else {
        panic!("expected accepted primary assignment, got {set_primary:?}");
    };
    let snapshot = bus
        .task_snapshot(task_id)
        .expect("load snapshot")
        .expect("task exists");
    assert_eq!(snapshot.task.lifecycle, TaskLifecycle::Open);
    (
        bus,
        task_id,
        agent_session_id,
        snapshot.task.action_epoch,
        primary_revision,
        client_id,
    )
}

fn send_now_intent(
    agent_session_id: AgentSessionId,
    turn_id: TurnId,
    action_epoch: u64,
    text: &str,
    wait: bool,
) -> SubmitProviderInputIntent {
    SubmitProviderInputIntent::try_new(
        agent_session_id,
        3,
        turn_id,
        action_epoch,
        None,
        None,
        ProviderInputAction::SendNow {
            text: text.into(),
            wait,
        },
    )
    .expect("valid send-now intent")
}

fn bound_wait_fence(
    bus: &devmanager::kernel::CommandBus,
    task_id: TaskId,
    agent_session_id: AgentSessionId,
    command_id: CommandId,
    operation_id: OperationId,
    action_epoch: u64,
    runtime_generation: u64,
    turn_id: TurnId,
) -> ProviderWaitFence {
    let snapshot = bus.task_snapshot(task_id).expect("snapshot").expect("task");
    let agent = snapshot.agents.get(&agent_session_id).expect("agent");
    ProviderWaitFence::new_with_identity(
        command_id,
        task_id,
        operation_id,
        action_epoch,
        agent_session_id,
        agent.provider_kind,
        agent
            .provider_session_id
            .clone()
            .expect("bound provider session"),
        runtime_generation,
        turn_id,
        None,
        None,
    )
}

#[derive(serde::Serialize)]
struct ProviderEffectWire {
    schema_version: u32,
    destination_class: devmanager::kernel::DestinationClass,
    replay_policy: devmanager::kernel::ReplayPolicy,
    effect: devmanager::kernel::Effect,
}

fn overwrite_provider_effect(
    path: &std::path::Path,
    operation_id: OperationId,
    effect: devmanager::kernel::Effect,
) {
    use devmanager::kernel::{DestinationClass, ReplayPolicy};
    use rusqlite::Connection;

    let payload = rmp_serde::to_vec_named(&ProviderEffectWire {
        schema_version: 1,
        destination_class: DestinationClass::ProviderInput,
        replay_policy: ReplayPolicy::NoAutomaticRetry,
        effect,
    })
    .expect("encode tampered provider effect");
    let conn = Connection::open(path).expect("open provider effect payload");
    assert_eq!(
        conn.execute(
            "UPDATE outbox SET payload = ?1 WHERE operation_id = ?2",
            rusqlite::params![payload, operation_id.as_bytes().as_slice()],
        )
        .expect("tamper provider effect payload"),
        1
    );
}

fn begin_provider_dispatch(
    path: &std::path::Path,
    tail: u8,
) -> (
    TaskId,
    AgentSessionId,
    OperationId,
    devmanager::kernel::DispatchPermit,
) {
    use devmanager::domain::command::CommandReceipt;
    use devmanager::kernel::KernelStore;

    let (mut bus, task_id, agent_session_id, action_epoch, revision, client_id) =
        seed_open_task_with_agent(path, tail);
    let command_id = CommandId::from_bytes(fixed_uuid_v7(tail + 10)).expect("command");
    let turn_id = TurnId::from_bytes(fixed_uuid_v7(tail + 11)).expect("turn");
    let accepted = bus
        .execute(CommandEnvelope {
            command_id,
            client_id,
            task_id: Some(task_id),
            issued_at_ms: 1_725_003_000_000,
            expected_task_revision: Some(revision),
            command: Command::SubmitProviderInput(send_now_intent(
                agent_session_id,
                turn_id,
                action_epoch,
                "bytes may have crossed",
                false,
            )),
        })
        .expect("accept provider input");
    let CommandReceipt::Accepted { operation_id, .. } = accepted else {
        panic!("provider input must be accepted: {accepted:?}");
    };
    drop(bus);

    let mut store = KernelStore::open(path).expect("open kernel store");
    let claim = store
        .claim_next_dispatch(std::time::Duration::from_secs(30))
        .expect("claim provider dispatch")
        .expect("provider dispatch ready");
    let permit = store
        .begin_dispatch(&claim)
        .expect("begin provider dispatch");
    assert_eq!(
        permit.destination_class(),
        devmanager::kernel::DestinationClass::ProviderInput
    );
    assert_eq!(
        permit.replay_policy(),
        devmanager::kernel::ReplayPolicy::NoAutomaticRetry
    );
    match permit.effect() {
        devmanager::kernel::Effect::DeliverProviderInput {
            runtime_generation,
            provider_session_id,
            ..
        } => {
            assert_eq!(*runtime_generation, 3);
            let expected_session = format!("codex-session-{tail:02x}");
            assert_eq!(
                provider_session_id.as_ref().map(ProviderSessionId::as_str),
                Some(expected_session.as_str())
            );
        }
        effect => panic!("expected typed provider effect, got {effect:?}"),
    }
    drop(store);
    (task_id, agent_session_id, operation_id, permit)
}

#[test]
fn send_now_reopens_a_settled_task_before_provider_delivery() {
    use devmanager::domain::command::CommandReceipt;
    use devmanager::domain::TaskLifecycle;
    use tempfile::TempDir;

    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("provider-input-settled-reopen.sqlite3");
    let (mut bus, task_id, agent_session_id, action_epoch, revision, client_id) =
        seed_open_task_with_agent(&path, 0x91);

    let settled = bus
        .execute(CommandEnvelope {
            command_id: CommandId::from_bytes(fixed_uuid_v7(0x9a)).expect("settle command"),
            client_id,
            task_id: Some(task_id),
            issued_at_ms: 1_725_003_000_100,
            expected_task_revision: Some(revision),
            command: Command::SettleTask,
        })
        .expect("settle task");
    let CommandReceipt::Accepted {
        task_revision: Some(settled_revision),
        ..
    } = settled
    else {
        panic!("settle must be accepted: {settled:?}");
    };
    let after_settle = bus
        .task_snapshot(task_id)
        .expect("settled snapshot")
        .expect("task");
    assert_eq!(after_settle.task.lifecycle, TaskLifecycle::Settled);
    assert_eq!(after_settle.task.action_epoch, action_epoch);
    assert_eq!(
        after_settle
            .agents
            .get(&agent_session_id)
            .expect("primary agent")
            .lifecycle,
        devmanager::domain::AgentSessionLifecycle::Open
    );

    let submitted = bus
        .execute(CommandEnvelope {
            command_id: CommandId::from_bytes(fixed_uuid_v7(0x9b)).expect("submit command"),
            client_id,
            task_id: Some(task_id),
            issued_at_ms: 1_725_003_000_200,
            expected_task_revision: Some(settled_revision),
            command: Command::SubmitProviderInput(send_now_intent(
                agent_session_id,
                TurnId::from_bytes(fixed_uuid_v7(0x9c)).expect("turn"),
                action_epoch,
                "resume from Done",
                false,
            )),
        })
        .expect("submit against settled task");
    assert!(matches!(submitted, CommandReceipt::Accepted { .. }));
    let after_submit = bus
        .task_snapshot(task_id)
        .expect("reopened snapshot")
        .expect("task");
    assert_eq!(after_submit.task.lifecycle, TaskLifecycle::Open);
    assert_eq!(after_submit.task.action_epoch, action_epoch);
}

fn register_secondary_agent(path: &std::path::Path, task_id: TaskId, tail: u8) -> AgentSessionId {
    use devmanager::domain::command::CommandReceipt;
    use devmanager::domain::{
        AgentRole, AgentSessionFacts, AgentSessionLifecycle, ArtifactKind, RequestSpecialistIntent,
        SpecialistPermission, DEFAULT_MAX_TOP_LEVEL_RUNTIMES,
    };
    use devmanager::kernel::CommandBus;

    let mut bus = CommandBus::open(path).expect("open secondary-agent bus");
    let snapshot = bus
        .task_snapshot(task_id)
        .expect("secondary-agent snapshot")
        .expect("secondary-agent task");
    let primary_id = snapshot.primary_agent_id.expect("primary agent");
    let primary = snapshot.agents.get(&primary_id).expect("primary facts");
    let agent_session_id = AgentSessionId::from_bytes(fixed_uuid_v7(tail)).expect("secondary id");
    let receipt = bus
        .execute(CommandEnvelope {
            command_id: CommandId::from_bytes(fixed_uuid_v7(tail + 1)).expect("secondary cmd"),
            client_id: ClientId::from_bytes(fixed_uuid_v7(tail + 2)).expect("secondary client"),
            task_id: Some(task_id),
            issued_at_ms: 1_725_003_000_500,
            expected_task_revision: Some(snapshot.task.revision),
            command: Command::RequestSpecialist(RequestSpecialistIntent {
                specialist: AgentSessionFacts {
                    id: agent_session_id,
                    task_id,
                    role: AgentRole::specialist("reviewer").expect("specialist role"),
                    provider_kind: ProviderKind::Codex,
                    provider_session_id: Some(
                        ProviderSessionId::new(format!("codex-secondary-{tail:02x}"))
                            .expect("secondary provider session"),
                    ),
                    lifecycle: AgentSessionLifecycle::Open,
                    runtime_generation: primary.runtime_generation,
                    revision: 0,
                },
                requested_by: primary_id,
                purpose: "verify rebuild checks unrelated agents".into(),
                permission: SpecialistPermission::ReadOnly,
                workspace: snapshot.task.workspace.clone(),
                expected_artifact_kind: ArtifactKind::ReviewReport,
                expected_action_epoch: snapshot.task.action_epoch,
                expected_runtime_generation: primary.runtime_generation,
                resource_id: None,
                max_top_level_runtimes: DEFAULT_MAX_TOP_LEVEL_RUNTIMES,
            }),
        })
        .expect("request secondary agent");
    let CommandReceipt::Accepted {
        task_revision: Some(_),
        ..
    } = receipt
    else {
        panic!("secondary-agent request must be accepted: {receipt:?}");
    };
    agent_session_id
}

fn rebuild_uncertain_provider_and_reject_payload_tamper(
    path: &std::path::Path,
    operation_id: OperationId,
) {
    use devmanager::domain::OperationState;
    use devmanager::kernel::KernelStore;
    use rusqlite::Connection;

    let mut store = KernelStore::open(path).expect("reopen uncertain provider store");
    let rebuild = store
        .rebuild_projections()
        .expect("rebuild uncertain provider state");
    assert!(rebuild.events_replayed > 0);
    assert!(rebuild.drift_detected, "projection tamper must be reported");
    assert!(matches!(
        store
            .operation_status(operation_id)
            .expect("rebuilt operation status"),
        Some(OperationState::Uncertain {
            code: devmanager::domain::OperationUncertaintyCode::AmbiguousDispatch,
            ..
        })
    ));
    drop(store);

    let conn = Connection::open(path).expect("open uncertain provider payload");
    let mut payload: Vec<u8> = conn
        .query_row(
            "SELECT payload FROM outbox WHERE operation_id = ?1",
            [operation_id.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .expect("provider effect payload");
    assert!(!payload.is_empty());
    payload[0] ^= 0xff;
    conn.execute(
        "UPDATE outbox SET payload = ?1 WHERE operation_id = ?2",
        rusqlite::params![payload, operation_id.as_bytes().as_slice()],
    )
    .expect("tamper provider effect payload");
    drop(conn);

    let mut store = KernelStore::open(path).expect("reopen tampered provider store");
    assert!(
        store.rebuild_projections().is_err(),
        "rebuild must reject tampered uncertain provider identity"
    );
}

#[test]
fn provider_dispatch_ambiguity_survives_provider_close_after_bytes_may_cross() {
    use devmanager::domain::OperationState;
    use devmanager::kernel::{AmbiguityDisposition, KernelStore};
    use rusqlite::Connection;
    use tempfile::TempDir;

    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("provider-input-close-ambiguity.sqlite3");
    let (task_id, agent_session_id, operation_id, permit) = begin_provider_dispatch(&path, 0x11);

    let conn = Connection::open(&path).expect("open sqlite");
    conn.execute(
        "UPDATE agent_sessions SET lifecycle = 'closed' WHERE agent_session_id = ?1",
        [agent_session_id.as_bytes().as_slice()],
    )
    .expect("close provider session after dispatch began");
    drop(conn);

    let mut store = KernelStore::open(&path).expect("reopen kernel store");
    assert_eq!(
        store
            .record_dispatch_ambiguity(&permit, std::time::Duration::from_millis(1))
            .expect("closed provider attempt must become uncertain"),
        AmbiguityDisposition::Uncertain,
    );
    assert!(matches!(
        store
            .operation_status(operation_id)
            .expect("operation status"),
        Some(OperationState::Uncertain {
            code: devmanager::domain::OperationUncertaintyCode::AmbiguousDispatch,
            ..
        })
    ));
    assert!(store
        .claim_next_dispatch(std::time::Duration::from_secs(30))
        .expect("no automatic provider retry")
        .is_none());
    assert!(store
        .record_dispatch_completion(&permit, devmanager::kernel::DispatchCompletion::Settled,)
        .is_err());

    let conn = Connection::open(&path).expect("reopen sqlite");
    let (outbox_state, attempts, lease, error_class): (String, i64, Option<i64>, Option<String>) =
        conn.query_row(
            "SELECT state, attempts, leased_until_ms, last_error_class
             FROM outbox WHERE operation_id = ?1",
            [operation_id.as_bytes().as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("uncertain outbox row");
    assert_eq!(outbox_state, "uncertain");
    assert_eq!(attempts, 1);
    assert!(lease.is_none());
    assert_eq!(error_class.as_deref(), Some("ambiguous_dispatch"));
    let (lifecycle, task): (String, Vec<u8>) = conn
        .query_row(
            "SELECT lifecycle, task_id FROM agent_sessions WHERE agent_session_id = ?1",
            [agent_session_id.as_bytes().as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("provider identity remains closed");
    assert_eq!(lifecycle, "closed");
    assert_eq!(task, task_id.as_bytes().to_vec());
    // Provider generation is intentionally carried by the typed outbox effect;
    // the generic operation fence is presentation-only for this destination.
    let generic_generation: Option<i64> = conn
        .query_row(
            "SELECT runtime_generation FROM operations WHERE operation_id = ?1",
            [operation_id.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .expect("generic operation fence");
    assert!(generic_generation.is_none());
    drop(store);
    drop(conn);
    rebuild_uncertain_provider_and_reject_payload_tamper(&path, operation_id);
}

#[test]
fn expired_provider_dispatch_recovery_preserves_g1_after_runtime_replacement() {
    use devmanager::domain::OperationState;
    use devmanager::kernel::{AmbiguityDisposition, KernelStore};
    use rusqlite::Connection;
    use tempfile::TempDir;

    let dir = TempDir::new().expect("tempdir");
    let path = dir
        .path()
        .join("provider-input-replacement-ambiguity.sqlite3");
    let (task_id, agent_session_id, operation_id, permit) = begin_provider_dispatch(&path, 0x21);
    let conn = Connection::open(&path).expect("open sqlite");
    conn.execute(
        "UPDATE agent_sessions
         SET provider_session_id = 'codex-session-g2', runtime_generation = 4
         WHERE agent_session_id = ?1",
        [agent_session_id.as_bytes().as_slice()],
    )
    .expect("replace provider runtime with G2");
    conn.execute(
        "UPDATE outbox SET leased_until_ms = 0 WHERE operation_id = ?1",
        [operation_id.as_bytes().as_slice()],
    )
    .expect("expire G1 dispatch lease");
    drop(conn);

    let mut store = KernelStore::open(&path).expect("reopen kernel store");
    assert_eq!(
        store
            .recover_next_expired_dispatch(std::time::Duration::from_millis(1))
            .expect("recover exact G1 attempt"),
        Some(AmbiguityDisposition::Uncertain),
    );
    assert!(matches!(
        store
            .operation_status(operation_id)
            .expect("operation status"),
        Some(OperationState::Uncertain {
            code: devmanager::domain::OperationUncertaintyCode::AmbiguousDispatch,
            ..
        })
    ));
    assert!(store
        .claim_next_dispatch(std::time::Duration::from_secs(30))
        .expect("G1 uncertainty must not authorize resend")
        .is_none());
    assert!(store
        .record_dispatch_completion(&permit, devmanager::kernel::DispatchCompletion::Settled,)
        .is_err());
    drop(store);

    let conn = Connection::open(&path).expect("reopen sqlite");
    let (provider_session, generation, lifecycle, task): (String, i64, String, Vec<u8>) = conn
        .query_row(
            "SELECT provider_session_id, runtime_generation, lifecycle, task_id
             FROM agent_sessions WHERE agent_session_id = ?1",
            [agent_session_id.as_bytes().as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("current G2 identity");
    assert_eq!(provider_session, "codex-session-g2");
    assert_eq!(generation, 4);
    assert_eq!(lifecycle, "open");
    assert_eq!(task, task_id.as_bytes().to_vec());
    let (outbox_state, attempts, lease): (String, i64, Option<i64>) = conn
        .query_row(
            "SELECT state, attempts, leased_until_ms FROM outbox WHERE operation_id = ?1",
            [operation_id.as_bytes().as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("G1 outbox state");
    assert_eq!(outbox_state, "uncertain");
    assert_eq!(attempts, 1);
    assert!(lease.is_none());
    let (destination, replay_policy, payload_len): (String, String, i64) = conn
        .query_row(
            "SELECT destination_class, replay_policy, length(payload)
             FROM outbox WHERE operation_id = ?1",
            [operation_id.as_bytes().as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("retained typed provider effect");
    assert_eq!(destination, "provider_input");
    assert_eq!(replay_policy, "no_automatic_retry");
    assert!(
        payload_len > 0,
        "uncertainty must retain provider identity payload"
    );
    match permit.effect() {
        devmanager::kernel::Effect::DeliverProviderInput {
            runtime_generation,
            provider_session_id,
            ..
        } => {
            assert_eq!(*runtime_generation, 3, "uncertainty must audit G1 attempt");
            assert_eq!(
                provider_session_id.as_ref().map(ProviderSessionId::as_str),
                Some("codex-session-21")
            );
        }
        effect => panic!("expected typed provider effect, got {effect:?}"),
    }
    let generic_generation: Option<i64> = conn
        .query_row(
            "SELECT runtime_generation FROM operations WHERE operation_id = ?1",
            [operation_id.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .expect("generic operation fence");
    assert!(generic_generation.is_none());
    drop(conn);
    rebuild_uncertain_provider_and_reject_payload_tamper(&path, operation_id);
}

#[test]
fn generic_ambiguity_fails_closed_on_tampered_task_fence() {
    use devmanager::domain::command::CommandReceipt;
    use devmanager::kernel::{Effect, KernelStore, StoreError};
    use rusqlite::Connection;
    use tempfile::TempDir;

    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("generic-ambiguity-stale-fence.sqlite3");
    let (mut bus, task_id, _agent_session_id, _action_epoch, revision, client_id) =
        seed_open_task_with_agent(&path, 0x31);
    let accepted = bus
        .execute(CommandEnvelope {
            command_id: CommandId::from_bytes(fixed_uuid_v7(0x3b)).expect("close command"),
            client_id,
            task_id: Some(task_id),
            issued_at_ms: 1_725_003_100_000,
            expected_task_revision: Some(revision),
            command: Command::BeginCloseTask,
        })
        .expect("accept generic close operation");
    let CommandReceipt::Accepted { operation_id, .. } = accepted else {
        panic!("generic close must be accepted: {accepted:?}");
    };
    drop(bus);

    let mut store = KernelStore::open(&path).expect("open generic store");
    let claim = store
        .claim_next_dispatch(std::time::Duration::from_secs(30))
        .expect("claim generic dispatch")
        .expect("generic dispatch ready");
    let permit = store
        .begin_dispatch(&claim)
        .expect("begin generic dispatch");
    assert!(matches!(permit.effect(), Effect::BeginTaskTeardown { .. }));
    drop(store);

    let conn = Connection::open(&path).expect("open generic projection");
    conn.execute(
        "UPDATE tasks SET action_epoch = action_epoch + 1 WHERE task_id = ?1",
        [task_id.as_bytes().as_slice()],
    )
    .expect("advance current task fence");
    drop(conn);

    let mut store = KernelStore::open(&path).expect("reopen stale generic store");
    assert!(
        matches!(
            store.record_dispatch_ambiguity(&permit, std::time::Duration::from_millis(1)),
            Err(StoreError::Corruption | StoreError::StaleFence)
        ),
        "generic effects must retain current ownership validation"
    );
    drop(store);

    let conn = Connection::open(&path).expect("expire generic lease");
    let (operation_state, outbox_state): (String, String) = conn
        .query_row(
            "SELECT op.state, o.state
             FROM operations op JOIN outbox o ON o.operation_id = op.operation_id
             WHERE op.operation_id = ?1",
            [operation_id.as_bytes().as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("generic dispatch remains in flight");
    assert_eq!(operation_state, "accepted");
    assert_eq!(outbox_state, "dispatching");
    conn.execute(
        "UPDATE outbox SET leased_until_ms = 0 WHERE operation_id = ?1",
        [operation_id.as_bytes().as_slice()],
    )
    .expect("expire generic dispatch");
    drop(conn);
    let mut store = KernelStore::open(&path).expect("reopen expired generic store");
    assert!(
        matches!(
            store.recover_next_expired_dispatch(std::time::Duration::from_millis(1)),
            Err(StoreError::Corruption | StoreError::StaleFence)
        ),
        "generic expiry recovery must not use immutable provider escape"
    );
}

#[test]
fn generic_retry_safe_ambiguity_keeps_current_dispatch_path() {
    use devmanager::domain::command::CommandReceipt;
    use devmanager::kernel::{AmbiguityDisposition, Effect, KernelStore};
    use rusqlite::Connection;
    use tempfile::TempDir;

    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("generic-retry-safe-ambiguity.sqlite3");
    let (mut bus, task_id, _agent_session_id, _action_epoch, revision, client_id) =
        seed_open_task_with_agent(&path, 0x37);
    let accepted = bus
        .execute(CommandEnvelope {
            command_id: CommandId::from_bytes(fixed_uuid_v7(0x46)).expect("close command"),
            client_id,
            task_id: Some(task_id),
            issued_at_ms: 1_725_003_200_000,
            expected_task_revision: Some(revision),
            command: Command::BeginCloseTask,
        })
        .expect("accept generic close operation");
    let CommandReceipt::Accepted { operation_id, .. } = accepted else {
        panic!("generic close must be accepted: {accepted:?}");
    };
    drop(bus);

    let mut store = KernelStore::open(&path).expect("open generic store");
    let claim = store
        .claim_next_dispatch(std::time::Duration::from_secs(30))
        .expect("claim generic dispatch")
        .expect("generic dispatch ready");
    let permit = store
        .begin_dispatch(&claim)
        .expect("begin generic dispatch");
    assert!(matches!(permit.effect(), Effect::BeginTaskTeardown { .. }));
    assert_eq!(
        store
            .record_dispatch_ambiguity(&permit, std::time::Duration::from_millis(1))
            .expect("generic ambiguity should retain its retry policy"),
        AmbiguityDisposition::RetryScheduled
    );
    drop(store);

    let conn = Connection::open(&path).expect("open generic state");
    let (state, attempts): (String, i64) = conn
        .query_row(
            "SELECT state, attempts FROM outbox WHERE operation_id = ?1",
            [operation_id.as_bytes().as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("generic outbox row");
    assert_eq!(state, "pending");
    assert_eq!(attempts, 1);
}

#[test]
fn expired_provider_recovery_rejects_attempts_behind_lease_generation() {
    use devmanager::kernel::{KernelStore, StoreError};
    use rusqlite::Connection;
    use tempfile::TempDir;

    let dir = TempDir::new().expect("tempdir");
    let path = dir
        .path()
        .join("provider-input-recovery-generation.sqlite3");
    let (_, _, operation_id, _permit) = begin_provider_dispatch(&path, 0x39);
    let conn = Connection::open(&path).expect("open provider state");
    conn.execute(
        "UPDATE outbox SET attempts = 2, leased_until_ms = 0 WHERE operation_id = ?1",
        [operation_id.as_bytes().as_slice()],
    )
    .expect("tamper attempt lineage");
    drop(conn);

    let mut store = KernelStore::open(&path).expect("reopen provider store");
    assert!(matches!(
        store.recover_next_expired_dispatch(std::time::Duration::from_millis(1)),
        Err(StoreError::Corruption)
    ));
    drop(store);
    let conn = Connection::open(&path).expect("reopen provider state");
    let (state, attempts): (String, i64) = conn
        .query_row(
            "SELECT state, attempts FROM outbox WHERE operation_id = ?1",
            [operation_id.as_bytes().as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("provider outbox row");
    assert_eq!(state, "dispatching");
    assert_eq!(attempts, 2);
}

#[test]
fn uncertain_provider_rebuild_checks_other_agents_and_attempt_generation() {
    use devmanager::kernel::{AmbiguityDisposition, KernelStore};
    use rusqlite::Connection;
    use tempfile::TempDir;

    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("provider-input-rebuild-generation.sqlite3");
    let (task_id, _agent_session_id, operation_id, permit) = begin_provider_dispatch(&path, 0x3f);
    let mut store = KernelStore::open(&path).expect("reopen provider store");
    assert_eq!(
        store
            .record_dispatch_ambiguity(&permit, std::time::Duration::from_millis(1))
            .expect("provider ambiguity"),
        AmbiguityDisposition::Uncertain
    );
    drop(store);
    let secondary_id = register_secondary_agent(&path, task_id, 0x4f);

    let conn = Connection::open(&path).expect("open provider projection");
    conn.execute(
        "UPDATE agent_sessions SET runtime_generation = 4 WHERE agent_session_id = ?1",
        [secondary_id.as_bytes().as_slice()],
    )
    .expect("tamper non-provider agent row");
    drop(conn);
    let mut store = KernelStore::open(&path).expect("reopen tampered provider store");
    assert!(
        store.rebuild_projections().is_err(),
        "uncertainty may skip only the immutable provider agent row"
    );
    drop(store);

    let conn = Connection::open(&path).expect("reopen provider projection");
    conn.execute(
        "UPDATE agent_sessions SET runtime_generation = 3 WHERE agent_session_id = ?1",
        [secondary_id.as_bytes().as_slice()],
    )
    .expect("restore secondary agent row");
    conn.execute(
        "UPDATE outbox SET attempts = 2 WHERE operation_id = ?1",
        [operation_id.as_bytes().as_slice()],
    )
    .expect("tamper terminal attempt lineage");
    drop(conn);
    let mut store = KernelStore::open(&path).expect("reopen generation-tampered store");
    assert!(
        store.rebuild_projections().is_err(),
        "terminal rebuild must bind attempts to the stored lease generation"
    );
}

#[test]
fn provider_dispatch_ambiguity_rejects_immutable_attempt_tampering() {
    use devmanager::kernel::{Effect, KernelStore, StoreError};
    use rusqlite::Connection;
    use tempfile::TempDir;

    type EffectMutator = fn(Effect) -> Effect;

    let cases: [(&str, u8, EffectMutator); 13] = [
        ("operation_id", 0x41, |mut effect| {
            if let Effect::DeliverProviderInput { operation_id, .. } = &mut effect {
                *operation_id = OperationId::new();
            }
            effect
        }),
        ("task_id", 0x51, |mut effect| {
            if let Effect::DeliverProviderInput { task_id, .. } = &mut effect {
                *task_id = TaskId::new();
            }
            effect
        }),
        ("agent_session_id", 0x61, |mut effect| {
            if let Effect::DeliverProviderInput {
                agent_session_id, ..
            } = &mut effect
            {
                *agent_session_id = AgentSessionId::new();
            }
            effect
        }),
        ("provider_kind", 0x71, |mut effect| {
            if let Effect::DeliverProviderInput { provider_kind, .. } = &mut effect {
                *provider_kind = ProviderKind::ClaudeCode;
            }
            effect
        }),
        ("provider_session_id", 0x81, |mut effect| {
            if let Effect::DeliverProviderInput {
                provider_session_id,
                ..
            } = &mut effect
            {
                *provider_session_id = Some(
                    ProviderSessionId::new("codex-session-tampered").expect("provider session"),
                );
            }
            effect
        }),
        ("runtime_generation", 0x91, |mut effect| {
            if let Effect::DeliverProviderInput {
                runtime_generation, ..
            } = &mut effect
            {
                *runtime_generation = (*runtime_generation).saturating_add(1);
            }
            effect
        }),
        ("action_epoch", 0xa1, |mut effect| {
            if let Effect::DeliverProviderInput { action_epoch, .. } = &mut effect {
                *action_epoch = (*action_epoch).saturating_add(1);
            }
            effect
        }),
        ("turn_id", 0xb1, |mut effect| {
            if let Effect::DeliverProviderInput { turn_id, .. } = &mut effect {
                *turn_id = TurnId::new();
            }
            effect
        }),
        ("action_payload", 0xc1, |mut effect| {
            if let Effect::DeliverProviderInput { action, .. } = &mut effect {
                *action = ProviderInputAction::SendNow {
                    text: "tampered provider payload".into(),
                    wait: false,
                };
            }
            effect
        }),
        ("command_id", 0xd1, |mut effect| {
            if let Effect::DeliverProviderInput { command_id, .. } = &mut effect {
                *command_id = CommandId::new();
            }
            effect
        }),
        ("client_id", 0xe1, |mut effect| {
            if let Effect::DeliverProviderInput { client_id, .. } = &mut effect {
                *client_id = ClientId::new();
            }
            effect
        }),
        ("question_id", 0xf1, |mut effect| {
            if let Effect::DeliverProviderInput { question_id, .. } = &mut effect {
                *question_id = Some(QuestionId::new());
            }
            effect
        }),
        ("approval_id", 0x32, |mut effect| {
            if let Effect::DeliverProviderInput { approval_id, .. } = &mut effect {
                *approval_id = Some(ApprovalId::new());
            }
            effect
        }),
    ];

    for (label, tail, mutate) in cases {
        let dir = TempDir::new().expect("tempdir");
        let path = dir
            .path()
            .join(format!("provider-input-tamper-{label}.sqlite3"));
        let (_, _, operation_id, permit) = begin_provider_dispatch(&path, tail);
        overwrite_provider_effect(&path, operation_id, mutate(permit.effect().clone()));

        let mut store = KernelStore::open(&path).expect("reopen tampered provider store");
        let result = store.record_dispatch_ambiguity(&permit, std::time::Duration::from_millis(1));
        assert!(
            result.is_err(),
            "immutable provider {label} tamper must be rejected: {result:?}"
        );
        drop(store);

        let conn = Connection::open(&path).expect("reopen tampered provider state");
        let (state, attempts): (String, i64) = conn
            .query_row(
                "SELECT state, attempts FROM outbox WHERE operation_id = ?1",
                [operation_id.as_bytes().as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("tampered provider outbox row");
        assert_eq!(
            state, "dispatching",
            "tampered {label} row must stay in flight"
        );
        assert_eq!(attempts, 1, "tampered {label} row must not advance attempt");
    }

    let dir = TempDir::new().expect("attempt tempdir");
    let path = dir.path().join("provider-input-tamper-attempt.sqlite3");
    let (_, _, operation_id, permit) = begin_provider_dispatch(&path, 0xe1);
    let conn = Connection::open(&path).expect("open attempt tamper");
    assert_eq!(
        conn.execute(
            "UPDATE outbox SET attempts = 2 WHERE operation_id = ?1",
            [operation_id.as_bytes().as_slice()],
        )
        .expect("tamper dispatch attempt"),
        1
    );
    drop(conn);

    let mut store = KernelStore::open(&path).expect("reopen attempt-tampered provider store");
    assert!(
        matches!(
            store.record_dispatch_ambiguity(&permit, std::time::Duration::from_millis(1)),
            Err(StoreError::StaleClaim) | Err(StoreError::Corruption) | Err(StoreError::StaleFence)
        ),
        "dispatch-attempt tamper must fail closed"
    );
}

#[test]
fn kernel_submit_provider_input_without_task_is_not_found() {
    let envelope = CommandEnvelope {
        command_id: CommandId::new(),
        client_id: ClientId::new(),
        task_id: Some(TaskId::new()),
        issued_at_ms: 1_725_000_000_100,
        expected_task_revision: Some(1),
        command: Command::SubmitProviderInput(send_now_intent(
            AgentSessionId::new(),
            TurnId::new(),
            0,
            "no snapshot",
            false,
        )),
    };
    assert_eq!(decide(None, &envelope), Err(RejectionCode::NotFound));
}

#[test]
fn submit_provider_input_receipt_survives_close_reopen_only_after_durable_commit() {
    use devmanager::domain::command::CommandReceipt;
    use devmanager::kernel::CommandBus;
    use tempfile::TempDir;

    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("provider-input.sqlite3");
    let (mut bus, task_id, agent_session_id, action_epoch, revision, client_id) =
        seed_open_task_with_agent(&path, 0x40);
    let turn_id = TurnId::from_bytes(fixed_uuid_v7(0x4A)).expect("turn");
    let command_id = CommandId::from_bytes(fixed_uuid_v7(0x4B)).expect("submit cmd");
    let envelope = CommandEnvelope {
        command_id,
        client_id,
        task_id: Some(task_id),
        issued_at_ms: 1_725_000_000_200,
        expected_task_revision: Some(revision),
        command: Command::SubmitProviderInput(send_now_intent(
            agent_session_id,
            turn_id,
            action_epoch,
            "ship the smallest fix",
            false,
        )),
    };
    let first = bus.execute(envelope.clone()).expect("first submit");
    let CommandReceipt::Accepted {
        command_id: accepted_id,
        operation_id,
        ..
    } = first
    else {
        panic!("receipt may say persisted only after durable commit, got {first:?}");
    };
    assert_eq!(accepted_id, command_id);
    assert!(matches!(
        bus.operation_status(operation_id)
            .expect("operation status"),
        Some(devmanager::domain::OperationState::Accepted)
    ));
    drop(bus);

    let mut reopened = CommandBus::open(&path).expect("reopen");
    let replayed = reopened.execute(envelope).expect("replay after reopen");
    match replayed {
        CommandReceipt::Accepted {
            command_id: replayed_id,
            operation_id: replayed_op,
            ..
        } => {
            assert_eq!(replayed_id, command_id);
            assert_eq!(replayed_op, operation_id);
        }
        other => panic!("close/reopen must return the same accepted receipt, got {other:?}"),
    }
}

#[test]
fn submit_provider_input_same_command_id_different_payload_is_typed_conflict() {
    use devmanager::domain::command::CommandReceipt;
    use tempfile::TempDir;

    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("provider-input-digest.sqlite3");
    let (mut bus, task_id, agent_session_id, action_epoch, revision, client_id) =
        seed_open_task_with_agent(&path, 0x50);
    let turn_id = TurnId::from_bytes(fixed_uuid_v7(0x5A)).expect("turn");
    let command_id = CommandId::from_bytes(fixed_uuid_v7(0x5B)).expect("submit cmd");
    let first_envelope = CommandEnvelope {
        command_id,
        client_id,
        task_id: Some(task_id),
        issued_at_ms: 1_725_000_000_200,
        expected_task_revision: Some(revision),
        command: Command::SubmitProviderInput(send_now_intent(
            agent_session_id,
            turn_id,
            action_epoch,
            "first payload",
            false,
        )),
    };
    let first = bus.execute(first_envelope).expect("first submit");
    let CommandReceipt::Accepted { operation_id, .. } = first else {
        panic!("expected accepted first submit, got {first:?}");
    };

    let conflicting = CommandEnvelope {
        command_id,
        client_id,
        task_id: Some(task_id),
        issued_at_ms: 1_725_000_000_200,
        expected_task_revision: Some(revision),
        command: Command::SubmitProviderInput(send_now_intent(
            agent_session_id,
            turn_id,
            action_epoch,
            "different payload",
            false,
        )),
    };
    let second = bus.execute(conflicting).expect("conflicting submit");
    match second {
        CommandReceipt::Rejected {
            code: RejectionCode::IdempotencyConflict,
            command_id: rejected_id,
            ..
        } => assert_eq!(rejected_id, command_id),
        other => panic!("reuse with a different payload must be a typed conflict, got {other:?}"),
    }
    assert!(
        !matches!(second, CommandReceipt::Accepted { operation_id: op, .. } if op == operation_id),
        "conflicting payload must never return the old accepted receipt"
    );
}

#[test]
fn first_answer_wins_uses_host_order_and_survives_reopen_and_second_connection() {
    use devmanager::domain::command::CommandReceipt;
    use devmanager::kernel::CommandBus;
    use tempfile::TempDir;

    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("provider-input-faw.sqlite3");
    let (mut bus, task_id, agent_session_id, action_epoch, mut revision, client_id) =
        seed_open_task_with_agent(&path, 0x60);
    let turn_id = TurnId::from_bytes(fixed_uuid_v7(0x6A)).expect("turn");
    let question_id = QuestionId::from_bytes(fixed_uuid_v7(0x6B)).expect("question");

    let presented = bus
        .execute(CommandEnvelope {
            command_id: CommandId::from_bytes(fixed_uuid_v7(0x6C)).expect("present cmd"),
            client_id,
            task_id: Some(task_id),
            issued_at_ms: 1_725_000_000_180,
            expected_task_revision: Some(revision),
            command: Command::PresentProviderQuestion(
                PresentProviderQuestionIntent::try_new(
                    agent_session_id,
                    3,
                    turn_id,
                    action_epoch,
                    question_id,
                )
                .expect("present intent"),
            ),
        })
        .expect("present question");
    let CommandReceipt::Accepted {
        task_revision: Some(next_revision),
        ..
    } = presented
    else {
        panic!("expected presented question, got {presented:?}");
    };
    revision = next_revision;

    let winner_cmd = CommandId::from_bytes(fixed_uuid_v7(0x6D)).expect("winner cmd");
    let winner = bus
        .execute(CommandEnvelope {
            command_id: winner_cmd,
            client_id,
            task_id: Some(task_id),
            issued_at_ms: 9_999_999_999_999,
            expected_task_revision: Some(revision),
            command: Command::SubmitProviderInput(
                SubmitProviderInputIntent::try_new(
                    agent_session_id,
                    3,
                    turn_id,
                    action_epoch,
                    Some(question_id),
                    None,
                    ProviderInputAction::AnswerQuestion {
                        question_id,
                        answer: "first device".into(),
                    },
                )
                .expect("winner intent"),
            ),
        })
        .expect("winner answer");
    let CommandReceipt::Accepted {
        operation_id: winner_op,
        task_revision: Some(after_winner),
        ..
    } = winner
    else {
        panic!("expected accepted winner, got {winner:?}");
    };

    let mut other = CommandBus::open(&path).expect("second connection");
    let loser = other
        .execute(CommandEnvelope {
            command_id: CommandId::from_bytes(fixed_uuid_v7(0x6E)).expect("loser cmd"),
            client_id,
            task_id: Some(task_id),
            issued_at_ms: 1,
            expected_task_revision: Some(after_winner),
            command: Command::SubmitProviderInput(
                SubmitProviderInputIntent::try_new(
                    agent_session_id,
                    3,
                    turn_id,
                    action_epoch,
                    Some(question_id),
                    None,
                    ProviderInputAction::AnswerQuestion {
                        question_id,
                        answer: "second device".into(),
                    },
                )
                .expect("loser intent"),
            ),
        })
        .expect("loser answer");
    match loser {
        CommandReceipt::Rejected {
            code: RejectionCode::AlreadyResolved,
            resolution: Some(winner),
            ..
        } => {
            assert_eq!(winner.command_id, winner_cmd);
            assert_eq!(winner.client_id, client_id);
            assert!(winner.accepted_at_ms > 0);
        }
        other => panic!("second device must not change the winner, got {other:?}"),
    }
    drop(bus);
    drop(other);

    let reopened = CommandBus::open(&path).expect("reopen");
    let snapshot = reopened
        .task_snapshot(task_id)
        .expect("snapshot")
        .expect("task");
    let session = snapshot
        .provider_sessions
        .get(&agent_session_id)
        .expect("provider session");
    assert_eq!(session.open_question, None);
    let stored = session
        .question_winners
        .get(&question_id)
        .expect("persisted winner");
    assert_eq!(stored.command_id, winner_cmd);
    assert_eq!(stored.client_id, client_id);
    assert!(stored.accepted_at_ms > 0);
    let settlement = session.last_settlement.expect("intent settlement");
    assert_eq!(settlement.command_id, winner_cmd);
    assert_eq!(
        settlement.intent,
        devmanager::domain::ProviderIntentPhase::Accepted
    );
    assert!(!settlement.is_delivered());
    assert_eq!(
        settlement.delivery,
        devmanager::domain::ProviderDeliveryVisibility::hold_until_destination_adapter()
    );
    let _ = winner_op;
}

#[test]
fn wait_settlement_requires_exact_fence_and_rejects_replacement_generation() {
    use devmanager::domain::command::CommandReceipt;
    use devmanager::domain::SettleProviderWaitIntent;
    use tempfile::TempDir;

    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("provider-input-wait.sqlite3");
    let (mut bus, task_id, agent_session_id, action_epoch, revision, client_id) =
        seed_open_task_with_agent(&path, 0x70);
    let turn_id = TurnId::from_bytes(fixed_uuid_v7(0x7A)).expect("turn");
    let command_id = CommandId::from_bytes(fixed_uuid_v7(0x7B)).expect("wait cmd");
    let accepted = bus
        .execute(CommandEnvelope {
            command_id,
            client_id,
            task_id: Some(task_id),
            issued_at_ms: 1_725_000_000_200,
            expected_task_revision: Some(revision),
            command: Command::SubmitProviderInput(send_now_intent(
                agent_session_id,
                turn_id,
                action_epoch,
                "wait for turn",
                true,
            )),
        })
        .expect("send and wait");
    let CommandReceipt::Accepted {
        operation_id,
        task_revision: Some(after_wait),
        ..
    } = accepted
    else {
        panic!("expected accepted wait, got {accepted:?}");
    };

    let fence = bound_wait_fence(
        &bus,
        task_id,
        agent_session_id,
        command_id,
        operation_id,
        action_epoch,
        3,
        turn_id,
    );
    let bad = bound_wait_fence(
        &bus,
        task_id,
        agent_session_id,
        command_id,
        operation_id,
        action_epoch,
        4,
        turn_id,
    );
    let rejected = bus
        .execute(CommandEnvelope {
            command_id: CommandId::from_bytes(fixed_uuid_v7(0x7C)).expect("bad settle"),
            client_id,
            task_id: Some(task_id),
            issued_at_ms: 1_725_000_000_210,
            expected_task_revision: Some(after_wait),
            command: Command::SettleProviderWait(
                SettleProviderWaitIntent::try_new(bad).expect("bad settle intent"),
            ),
        })
        .expect("bad settle");
    assert!(
        matches!(
            rejected,
            CommandReceipt::Rejected {
                code: RejectionCode::InvalidTransition,
                ..
            }
        ),
        "replacement generation cannot settle wait, got {rejected:?}"
    );

    let settled = bus
        .execute(CommandEnvelope {
            command_id: CommandId::from_bytes(fixed_uuid_v7(0x7D)).expect("good settle"),
            client_id,
            task_id: Some(task_id),
            issued_at_ms: 1_725_000_000_220,
            expected_task_revision: Some(after_wait),
            command: Command::SettleProviderWait(
                SettleProviderWaitIntent::try_new(fence).expect("settle intent"),
            ),
        })
        .expect("good settle");
    assert!(
        matches!(settled, CommandReceipt::Accepted { .. }),
        "exact fence must settle, got {settled:?}"
    );
}

#[test]
fn settled_waits_reclaim_capacity_across_sixty_five_cycles() {
    use devmanager::domain::command::CommandReceipt;
    use devmanager::domain::SettleProviderWaitIntent;
    use tempfile::TempDir;

    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("provider-input-wait-capacity.sqlite3");
    let (mut bus, task_id, agent_session_id, action_epoch, mut revision, client_id) =
        seed_open_task_with_agent(&path, 0xE0);
    let turn_id = TurnId::from_bytes(fixed_uuid_v7(0xEA)).expect("turn");

    for index in 0..65_u8 {
        let wait_command_id =
            CommandId::from_bytes(fixed_uuid_v7(index.wrapping_add(1))).expect("wait command");
        let accepted = bus
            .execute(CommandEnvelope {
                command_id: wait_command_id,
                client_id,
                task_id: Some(task_id),
                issued_at_ms: 1_725_001_000_000 + i64::from(index) * 2,
                expected_task_revision: Some(revision),
                command: Command::SubmitProviderInput(send_now_intent(
                    agent_session_id,
                    turn_id,
                    action_epoch,
                    "cycle",
                    true,
                )),
            })
            .expect("wait input");
        let CommandReceipt::Accepted {
            operation_id,
            task_revision: Some(after_wait),
            ..
        } = accepted
        else {
            panic!("wait input must be accepted");
        };

        let fence = bound_wait_fence(
            &bus,
            task_id,
            agent_session_id,
            wait_command_id,
            operation_id,
            action_epoch,
            3,
            turn_id,
        );
        let settled = bus
            .execute(CommandEnvelope {
                command_id: CommandId::from_bytes(fixed_uuid_v7(index.wrapping_add(100)))
                    .expect("settle command"),
                client_id,
                task_id: Some(task_id),
                issued_at_ms: 1_725_001_000_001 + i64::from(index) * 2,
                expected_task_revision: Some(after_wait),
                command: Command::SettleProviderWait(
                    SettleProviderWaitIntent::try_new(fence).expect("settle intent"),
                ),
            })
            .expect("settle wait");
        let CommandReceipt::Accepted {
            task_revision: Some(after_settle),
            ..
        } = settled
        else {
            panic!("wait settlement must be accepted");
        };
        revision = after_settle;
    }

    let snapshot = bus.task_snapshot(task_id).expect("snapshot").expect("task");
    let session = snapshot
        .provider_sessions
        .get(&agent_session_id)
        .expect("provider session");
    assert!(session.waits.len() <= devmanager::domain::MAX_PROVIDER_WAITS);
    assert!(session.waits.values().all(|record| !record.pending));
}

#[test]
fn first_approval_wins_reports_typed_resolution_and_clears_open_approval() {
    use devmanager::domain::command::CommandReceipt;
    use tempfile::TempDir;

    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("provider-input-approval.sqlite3");
    let (mut bus, task_id, agent_session_id, action_epoch, mut revision, client_id) =
        seed_open_task_with_agent(&path, 0xF0);
    let turn_id = TurnId::from_bytes(fixed_uuid_v7(0xFA)).expect("turn");
    let approval_id =
        devmanager::domain::ApprovalId::from_bytes(fixed_uuid_v7(0xFB)).expect("approval");

    let presented = bus
        .execute(CommandEnvelope {
            command_id: CommandId::from_bytes(fixed_uuid_v7(0xFC)).expect("present command"),
            client_id,
            task_id: Some(task_id),
            issued_at_ms: 1_725_002_000_000,
            expected_task_revision: Some(revision),
            command: Command::PresentProviderApproval(
                PresentProviderApprovalIntent::try_new(
                    agent_session_id,
                    3,
                    turn_id,
                    action_epoch,
                    approval_id,
                )
                .expect("present approval"),
            ),
        })
        .expect("present approval");
    let CommandReceipt::Accepted {
        task_revision: Some(after_present),
        ..
    } = presented
    else {
        panic!("approval presentation must be accepted");
    };
    revision = after_present;

    let winner_command_id = CommandId::from_bytes(fixed_uuid_v7(0xFD)).expect("winner");
    let winner_at = 1_725_002_000_010;
    let winner = bus
        .execute(CommandEnvelope {
            command_id: winner_command_id,
            client_id,
            task_id: Some(task_id),
            issued_at_ms: winner_at,
            expected_task_revision: Some(revision),
            command: Command::SubmitProviderInput(
                SubmitProviderInputIntent::try_new(
                    agent_session_id,
                    3,
                    turn_id,
                    action_epoch,
                    None,
                    Some(approval_id),
                    ProviderInputAction::ResolveApproval {
                        approval_id,
                        allow: true,
                    },
                )
                .expect("resolve approval"),
            ),
        })
        .expect("resolve approval");
    let CommandReceipt::Accepted { .. } = winner else {
        panic!("approval winner must be accepted: {winner:?}");
    };
    let winner_snapshot = bus
        .task_snapshot(task_id)
        .expect("winner snapshot")
        .expect("winner task");
    let winner_timestamp = winner_snapshot
        .provider_sessions
        .get(&agent_session_id)
        .and_then(|session| session.approval_winners.get(&approval_id))
        .map(|winner| winner.accepted_at_ms)
        .expect("durable approval winner timestamp");

    let loser = bus
        .execute(CommandEnvelope {
            command_id: CommandId::from_bytes(fixed_uuid_v7(0xFE)).expect("loser"),
            client_id,
            task_id: Some(task_id),
            issued_at_ms: winner_at + 1,
            // Deliberately retain the pre-winner revision: a concurrent loser
            // must still receive the typed first-winner resolution.
            expected_task_revision: Some(revision),
            command: Command::SubmitProviderInput(
                SubmitProviderInputIntent::try_new(
                    agent_session_id,
                    3,
                    turn_id,
                    action_epoch,
                    None,
                    Some(approval_id),
                    ProviderInputAction::ResolveApproval {
                        approval_id,
                        allow: false,
                    },
                )
                .expect("resolve approval loser"),
            ),
        })
        .expect("loser approval");
    let CommandReceipt::Rejected {
        code,
        resolution: Some(resolution),
        ..
    } = loser
    else {
        unreachable!("already-resolved loser must carry winner metadata");
    };
    assert_eq!(code, RejectionCode::AlreadyResolved);
    assert_eq!(resolution.command_id, winner_command_id);
    assert_eq!(resolution.client_id, client_id);
    assert_eq!(resolution.accepted_at_ms, winner_timestamp);

    let snapshot = bus.task_snapshot(task_id).expect("snapshot").expect("task");
    let session = snapshot
        .provider_sessions
        .get(&agent_session_id)
        .expect("provider session");
    assert_eq!(session.open_approval, None);
}

#[test]
fn extraneous_nested_ids_are_rejected_before_write() {
    let question = QuestionId::new();
    let err = SubmitProviderInputIntent::try_new(
        AgentSessionId::new(),
        3,
        TurnId::new(),
        1,
        Some(question),
        None,
        ProviderInputAction::SendNow {
            text: "no question on send".into(),
            wait: false,
        },
    )
    .expect_err("extraneous question id");
    assert_eq!(
        err,
        devmanager::domain::ProviderInputIntentError::InconsistentNestedIds
    );
}

#[test]
fn provider_input_debug_redacts_raw_text() {
    let action = ProviderInputAction::SendNow {
        text: "secret prompt".into(),
        wait: false,
    };
    let rendered = format!("{action:?}");
    assert!(!rendered.contains("secret prompt"), "{rendered}");
    assert!(rendered.contains("text_bytes"), "{rendered}");
}

#[test]
fn provider_kind_decode_rejects_unbounded_or_noncanonical_identity() {
    let oversized = format!(
        "\"{}\"",
        "x".repeat(devmanager::domain::MAX_PROVIDER_KIND_BYTES + 1)
    );
    assert!(serde_json::from_str::<ProviderKind>(&oversized).is_err());
    assert!(serde_json::from_str::<ProviderKind>(r#"" codex""#).is_err());
}

#[test]
fn action_catalog_exposes_executable_provider_controls_without_restart_or_new_conversation() {
    let ids: Vec<&str> = catalog().iter().map(|action| action.id).collect();
    for expected in [
        ACTION_PROVIDER_SEND_NOW,
        ACTION_PROVIDER_STEER_CURRENT_TURN,
        ACTION_PROVIDER_QUEUE_FOLLOW_UP,
        ACTION_PROVIDER_ANSWER_QUESTION,
        ACTION_PROVIDER_RESOLVE_APPROVAL,
        ACTION_PROVIDER_STOP_TURN,
    ] {
        assert!(ids.contains(&expected), "missing catalog action {expected}");
    }
    assert!(!ids.contains(&ACTION_PROVIDER_NEW_CONVERSATION));
    assert!(
        !ids.iter().any(|id| {
            id.starts_with("provider.") && id.to_ascii_lowercase().contains("restart")
        }),
        "provider catalog must not expose Restart"
    );

    let send = catalog()
        .iter()
        .find(|action| action.id == ACTION_PROVIDER_SEND_NOW)
        .expect("send now");
    assert_eq!(send.scope, ActionScope::Task);
    assert_eq!(send.risk, ActionRisk::Mutating);

    let unavailable = devmanager::providers::input::new_conversation_availability();
    assert_eq!(
        unavailable,
        devmanager::providers::input::ProviderActionUnavailable::NewConversationRequiresProviderRuntime
    );
    assert_eq!(unavailable.reason_code(), "provider_runtime_not_wired");
}

#[test]
fn available_actions_omit_turn_controls_until_a_turn_exists() {
    use tempfile::TempDir;

    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("provider-input-catalog.sqlite3");
    let (mut bus, task_id, agent_session_id, action_epoch, revision, client_id) =
        seed_open_task_with_agent(&path, 0x80);
    let snapshot = bus.task_snapshot(task_id).expect("snapshot").expect("task");
    let before = devmanager::providers::input::available_action_ids(&snapshot, agent_session_id);
    assert_eq!(before, vec![ACTION_PROVIDER_SEND_NOW]);
    assert!(!before.contains(&ACTION_PROVIDER_STEER_CURRENT_TURN));
    assert!(!before.contains(&ACTION_PROVIDER_QUEUE_FOLLOW_UP));
    assert!(!before.contains(&ACTION_PROVIDER_STOP_TURN));

    let turn_id = TurnId::from_bytes(fixed_uuid_v7(0x8A)).expect("turn");
    bus.execute(CommandEnvelope {
        command_id: CommandId::from_bytes(fixed_uuid_v7(0x8B)).expect("send cmd"),
        client_id,
        task_id: Some(task_id),
        issued_at_ms: 1_725_000_000_240,
        expected_task_revision: Some(revision),
        command: Command::SubmitProviderInput(send_now_intent(
            agent_session_id,
            turn_id,
            action_epoch,
            "adopt turn",
            false,
        )),
    })
    .expect("send now");
    let after = bus.task_snapshot(task_id).expect("snapshot").expect("task");
    let ids = devmanager::providers::input::available_action_ids(&after, agent_session_id);
    assert!(ids.contains(&ACTION_PROVIDER_SEND_NOW));
    assert!(ids.contains(&ACTION_PROVIDER_STEER_CURRENT_TURN));
    assert!(ids.contains(&ACTION_PROVIDER_QUEUE_FOLLOW_UP));
    assert!(ids.contains(&ACTION_PROVIDER_STOP_TURN));
}

#[test]
fn oversized_provider_text_is_rejected_on_deserialize() {
    let huge = "x".repeat(devmanager::domain::MAX_PROVIDER_INPUT_TEXT_BYTES + 1);
    let json = format!(r#"{{"send_now":{{"text":"{huge}","wait":false}}}}"#);
    let err = serde_json::from_str::<ProviderInputAction>(&json)
        .expect_err("oversized text must not deserialize");
    let rendered = err.to_string();
    assert!(
        rendered.contains("exceeds")
            || rendered.contains("too large")
            || rendered.contains("65536"),
        "{rendered}"
    );
}

#[test]
fn payload_digest_includes_fence_identities_and_excludes_issued_at() {
    use devmanager::domain::command::command_payload_digest;
    use devmanager::domain::{ProviderWaitFence, SettleProviderWaitIntent};

    let client_id = ClientId::from_bytes(fixed_uuid_v7(0x90)).expect("client");
    let task_id = TaskId::from_bytes(fixed_uuid_v7(0x91)).expect("task");
    let agent = AgentSessionId::from_bytes(fixed_uuid_v7(0x92)).expect("agent");
    let turn = TurnId::from_bytes(fixed_uuid_v7(0x93)).expect("turn");
    let command_id = CommandId::from_bytes(fixed_uuid_v7(0x94)).expect("cmd");
    let fence = ProviderWaitFence::new(command_id, task_id, 1, agent, 3, turn, None, None);
    let other_generation =
        ProviderWaitFence::new(command_id, task_id, 1, agent, 4, turn, None, None);
    let envelope = |issued_at_ms: i64, fence: ProviderWaitFence| CommandEnvelope {
        command_id,
        client_id,
        task_id: Some(task_id),
        issued_at_ms,
        expected_task_revision: Some(2),
        command: Command::SettleProviderWait(
            SettleProviderWaitIntent::try_new(fence).expect("settle"),
        ),
    };
    let first = command_payload_digest(&envelope(10, fence.clone())).expect("digest");
    let same_later = command_payload_digest(&envelope(99, fence)).expect("digest");
    let different_fence = command_payload_digest(&envelope(10, other_generation)).expect("digest");
    assert_eq!(first, same_later);
    assert_ne!(first, different_fence);

    let submit_a = CommandEnvelope {
        command_id,
        client_id,
        task_id: Some(task_id),
        issued_at_ms: 1,
        expected_task_revision: Some(2),
        command: Command::SubmitProviderInput(send_now_intent(agent, turn, 1, "same", false)),
    };
    let submit_b = CommandEnvelope {
        issued_at_ms: 9_999,
        ..submit_a.clone()
    };
    assert_eq!(
        command_payload_digest(&submit_a).expect("submit a"),
        command_payload_digest(&submit_b).expect("submit b")
    );
}

#[test]
fn raw_pty_composer_is_forbidden_only_when_provider_input_capability_selects_ai_sessions() {
    use devmanager::protocol::CapabilitySet;
    use devmanager::providers::input::{
        provider_input_capability, provider_input_capability_selected, raw_pty_composer_forbidden,
    };
    use devmanager::state::SessionKind;

    let selected = CapabilitySet::from_capabilities([provider_input_capability()]);
    assert!(provider_input_capability_selected(selected));
    assert!(!provider_input_capability_selected(CapabilitySet::empty()));
    assert!(raw_pty_composer_forbidden(true, SessionKind::Claude));
    assert!(raw_pty_composer_forbidden(true, SessionKind::Codex));
    assert!(!raw_pty_composer_forbidden(true, SessionKind::Shell));
    assert!(!raw_pty_composer_forbidden(false, SessionKind::Claude));
}

#[test]
fn duplicate_journal_only_provider_inputs_do_not_create_empty_operations() {
    use devmanager::domain::command::CommandReceipt;
    use devmanager::domain::SettleProviderWaitIntent;
    use tempfile::TempDir;

    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("provider-input-duplicates.sqlite3");
    let (mut bus, task_id, agent_session_id, action_epoch, revision, client_id) =
        seed_open_task_with_agent(&path, 0xA0);
    let turn_id = TurnId::from_bytes(fixed_uuid_v7(0xAA)).expect("turn");
    let question_id = QuestionId::from_bytes(fixed_uuid_v7(0xAB)).expect("question");
    let presented = bus
        .execute(CommandEnvelope {
            command_id: CommandId::from_bytes(fixed_uuid_v7(0xAC)).expect("present cmd"),
            client_id,
            task_id: Some(task_id),
            issued_at_ms: 1_725_000_001_000,
            expected_task_revision: Some(revision),
            command: Command::PresentProviderQuestion(
                PresentProviderQuestionIntent::try_new(
                    agent_session_id,
                    3,
                    turn_id,
                    action_epoch,
                    question_id,
                )
                .expect("present intent"),
            ),
        })
        .expect("present question");
    let CommandReceipt::Accepted {
        task_revision: Some(after_present),
        ..
    } = presented
    else {
        panic!("expected accepted presentation, got {presented:?}");
    };
    let duplicate = bus
        .execute(CommandEnvelope {
            command_id: CommandId::from_bytes(fixed_uuid_v7(0xAD)).expect("duplicate cmd"),
            client_id,
            task_id: Some(task_id),
            issued_at_ms: 1_725_000_001_010,
            expected_task_revision: Some(after_present),
            command: Command::PresentProviderQuestion(
                PresentProviderQuestionIntent::try_new(
                    agent_session_id,
                    3,
                    turn_id,
                    action_epoch,
                    question_id,
                )
                .expect("duplicate intent"),
            ),
        })
        .expect("duplicate question");
    assert!(matches!(
        duplicate,
        CommandReceipt::Rejected {
            code: RejectionCode::AlreadyExists,
            ..
        }
    ));

    let dir = TempDir::new().expect("wait tempdir");
    let path = dir
        .path()
        .join("provider-input-settlement-duplicates.sqlite3");
    let (mut bus, task_id, agent_session_id, action_epoch, revision, client_id) =
        seed_open_task_with_agent(&path, 0xB0);
    let turn_id = TurnId::from_bytes(fixed_uuid_v7(0xBA)).expect("wait turn");
    let wait_command_id = CommandId::from_bytes(fixed_uuid_v7(0xBB)).expect("wait cmd");
    let accepted = bus
        .execute(CommandEnvelope {
            command_id: wait_command_id,
            client_id,
            task_id: Some(task_id),
            issued_at_ms: 1_725_000_001_020,
            expected_task_revision: Some(revision),
            command: Command::SubmitProviderInput(send_now_intent(
                agent_session_id,
                turn_id,
                action_epoch,
                "wait",
                true,
            )),
        })
        .expect("wait input");
    let CommandReceipt::Accepted {
        operation_id,
        task_revision: Some(after_wait),
        ..
    } = accepted
    else {
        panic!("expected accepted wait, got {accepted:?}");
    };
    let fence = bound_wait_fence(
        &bus,
        task_id,
        agent_session_id,
        wait_command_id,
        operation_id,
        action_epoch,
        3,
        turn_id,
    );
    let settled = bus
        .execute(CommandEnvelope {
            command_id: CommandId::from_bytes(fixed_uuid_v7(0xBC)).expect("settle cmd"),
            client_id,
            task_id: Some(task_id),
            issued_at_ms: 1_725_000_001_030,
            expected_task_revision: Some(after_wait),
            command: Command::SettleProviderWait(
                SettleProviderWaitIntent::try_new(fence.clone()).expect("settle intent"),
            ),
        })
        .expect("settle wait");
    let CommandReceipt::Accepted {
        task_revision: Some(after_settle),
        ..
    } = settled
    else {
        panic!("expected accepted settlement, got {settled:?}");
    };
    let duplicate_settlement = bus
        .execute(CommandEnvelope {
            command_id: CommandId::from_bytes(fixed_uuid_v7(0xBD)).expect("duplicate settle cmd"),
            client_id,
            task_id: Some(task_id),
            issued_at_ms: 1_725_000_001_040,
            expected_task_revision: Some(after_settle),
            command: Command::SettleProviderWait(
                SettleProviderWaitIntent::try_new(fence).expect("duplicate settle intent"),
            ),
        })
        .expect("duplicate settlement");
    assert!(matches!(
        duplicate_settlement,
        CommandReceipt::Rejected {
            code: RejectionCode::AlreadyExists,
            ..
        }
    ));
}

#[test]
fn oversized_provider_session_projection_is_rejected_before_decode() {
    use devmanager::kernel::CommandBus;
    use tempfile::TempDir;

    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("provider-input-state-bound.sqlite3");
    let (bus, task_id, agent_session_id, ..) = seed_open_task_with_agent(&path, 0xC0);
    drop(bus);

    let oversized = vec![0_u8; devmanager::domain::MAX_PROVIDER_SESSION_STATE_BYTES + 1];
    let connection = rusqlite::Connection::open(&path).expect("open sqlite");
    connection
        .execute(
            "UPDATE provider_input_state SET state = ?1 WHERE agent_session_id = ?2",
            rusqlite::params![oversized, agent_session_id.as_bytes().as_slice()],
        )
        .expect("corrupt projection state");
    drop(connection);

    let reopened = CommandBus::open(&path).expect("reopen");
    let error = reopened
        .task_snapshot(task_id)
        .expect_err("oversized state must fail closed");
    assert!(
        matches!(&error, devmanager::kernel::StoreError::Projection(message) if message.contains("exceeds")),
        "unexpected error: {error:?}"
    );
}

#[test]
fn provider_session_projection_map_bounds_apply_on_write_and_decode() {
    use std::collections::BTreeMap;

    use serde::Serialize;

    let mut projection = devmanager::domain::ProviderSessionProjection::default();
    for _ in 0..=devmanager::domain::MAX_PROVIDER_QUESTION_WINS {
        projection.question_winners.insert(
            QuestionId::new(),
            devmanager::domain::ProviderResolutionWinner {
                command_id: CommandId::new(),
                client_id: ClientId::new(),
                accepted_at_ms: 1,
            },
        );
    }
    assert!(projection.validate_bounds().is_err());
    assert!(rmp_serde::to_vec_named(&projection).is_err());

    #[derive(Serialize)]
    struct OversizedProjectionWire {
        current_turn: Option<TurnId>,
        open_question: Option<QuestionId>,
        open_approval: Option<devmanager::domain::ApprovalId>,
        question_winners: BTreeMap<QuestionId, devmanager::domain::ProviderResolutionWinner>,
        approval_winners:
            BTreeMap<devmanager::domain::ApprovalId, devmanager::domain::ProviderResolutionWinner>,
        waits: BTreeMap<CommandId, devmanager::domain::ProviderWaitRecord>,
        last_settlement: Option<devmanager::domain::ProviderInputSettlement>,
    }
    let encoded = rmp_serde::to_vec_named(&OversizedProjectionWire {
        current_turn: None,
        open_question: None,
        open_approval: None,
        question_winners: projection.question_winners,
        approval_winners: BTreeMap::new(),
        waits: BTreeMap::new(),
        last_settlement: None,
    })
    .expect("encode malformed projection fixture");
    let error = rmp_serde::from_slice::<devmanager::domain::ProviderSessionProjection>(&encoded)
        .expect_err("oversized map must fail before projection use");
    assert!(error.to_string().contains("winner map") || error.to_string().contains("entries"));
}

#[test]
fn provider_input_event_decode_rejects_mismatched_nested_identity_and_wait_flag() {
    use devmanager::domain::{Event, ProviderDeliveryVisibility};

    let question_id = QuestionId::from_bytes(fixed_uuid_v7(0xD0)).expect("question");
    let other_question_id = QuestionId::from_bytes(fixed_uuid_v7(0xD1)).expect("other question");
    let event = Event::ProviderInputAccepted {
        command_id: CommandId::from_bytes(fixed_uuid_v7(0xD2)).expect("command"),
        client_id: ClientId::from_bytes(fixed_uuid_v7(0xD3)).expect("client"),
        operation_id: OperationId::from_bytes(fixed_uuid_v7(0xD6)).expect("operation"),
        agent_session_id: AgentSessionId::from_bytes(fixed_uuid_v7(0xD4)).expect("agent"),
        provider_kind: ProviderKind::Codex,
        provider_session_id: Some(
            devmanager::domain::ProviderSessionId::new("session-d4").expect("provider session"),
        ),
        runtime_generation: 3,
        turn_id: TurnId::from_bytes(fixed_uuid_v7(0xD5)).expect("turn"),
        action_epoch: 0,
        question_id: Some(question_id),
        approval_id: None,
        action: ProviderInputAction::AnswerQuestion {
            question_id: other_question_id,
            answer: "answer".into(),
        },
        wait: true,
        delivery: ProviderDeliveryVisibility::hold_until_destination_adapter(),
    };
    let encoded = serde_json::to_vec(&event).expect("encode event");
    serde_json::from_slice::<Event>(&encoded).expect_err("invalid event payload");
}

#[test]
fn codex_provider_input_without_bound_session_is_accepted_for_first_turn() {
    use devmanager::domain::command::CommandReceipt;
    use tempfile::TempDir;

    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("provider-input-unavailable.sqlite3");
    let (mut bus, task_id, agent_session_id, action_epoch, revision, client_id) =
        seed_open_task_without_provider_runtime(&path, 0xF0);
    let receipt = bus
        .execute(CommandEnvelope {
            command_id: CommandId::from_bytes(fixed_uuid_v7(0xFA)).expect("command"),
            client_id,
            task_id: Some(task_id),
            issued_at_ms: 1_725_000_002_000,
            expected_task_revision: Some(revision),
            command: Command::SubmitProviderInput(send_now_intent(
                agent_session_id,
                TurnId::from_bytes(fixed_uuid_v7(0xFB)).expect("turn"),
                action_epoch,
                "runtime unavailable",
                false,
            )),
        })
        .expect("Codex first-turn input must return a typed receipt");
    assert!(matches!(
        receipt,
        CommandReceipt::Accepted {
            task_revision: Some(_),
            ..
        }
    ));
}
