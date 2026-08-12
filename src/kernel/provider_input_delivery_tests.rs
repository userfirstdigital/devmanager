use super::*;
use crate::domain::command::{Command, CommandEnvelope, CommandReceipt, CreateTaskIntent};
use crate::domain::task::ReviewReadiness;
use crate::domain::OperationState;
use crate::domain::{
    AgentRole, AgentSessionFacts, AgentSessionId, AgentSessionLifecycle, ClientId, CommandId,
    EnvironmentId, ProjectId, ProviderInputAction, ProviderKind, ProviderSessionId,
    SubmitProviderInputIntent, TaskActivity, TaskAssignment, TaskAttention, TaskConnectivity,
    TaskId, TurnId, WorkspaceRef,
};
use crate::kernel::outbox::Effect;
use crate::providers::input::{
    deliver_through_capability, sequence_bounded_input, sequence_provider_action,
    BoundProviderInputPort, ProviderInputDeliveryError, ProviderInputDeliveryIdentity,
    ACTION_PROVIDER_SEND_NOW,
};
use std::time::Duration;
use tempfile::TempDir;

fn fixed_uuid_v7(tail: u8) -> [u8; 16] {
    [
        0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        tail,
    ]
}

fn seed_provider_dispatch(store: &mut KernelStore, tail: u8) -> (OperationId, DispatchPermit) {
    let client_id = ClientId::from_bytes(fixed_uuid_v7(tail)).expect("client");
    let task_id = TaskId::from_bytes(fixed_uuid_v7(tail + 1)).expect("task");
    let agent_session_id = AgentSessionId::from_bytes(fixed_uuid_v7(tail + 2)).expect("agent");
    let create = store
        .execute_for_test(CommandEnvelope {
            command_id: CommandId::from_bytes(fixed_uuid_v7(tail + 3)).expect("create cmd"),
            client_id,
            task_id: None,
            issued_at_ms: 1_725_000_000_100,
            expected_task_revision: None,
            command: Command::CreateTask(CreateTaskIntent {
                id: task_id,
                environment_id: EnvironmentId::from_bytes(fixed_uuid_v7(tail + 4))
                    .expect("environment"),
                title: "Provider input delivery".into(),
                description: None,
                project_id: ProjectId::from_bytes(fixed_uuid_v7(tail + 5)).expect("project"),
                workspace: WorkspaceRef::Main,
                assignment: TaskAssignment::LocalOwner,
                created_at_ms: 1_725_000_000_000,
                connectivity: TaskConnectivity::Connected,
                attention: TaskAttention::None,
                activity: TaskActivity::Idle,
                review_readiness: ReviewReadiness::NotReady,
            }),
        })
        .expect("create task");
    let CommandReceipt::Accepted {
        task_revision: Some(revision),
        ..
    } = create
    else {
        panic!("expected accepted create, got {create:?}");
    };
    let register = store
        .execute_for_test(CommandEnvelope {
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
                    provider_session_id: Some(
                        ProviderSessionId::new(format!("codex-session-{tail:02x}"))
                            .expect("provider session"),
                    ),
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
    let action_epoch: i64 = store
        .conn
        .query_row(
            "SELECT action_epoch FROM tasks WHERE task_id = ?1",
            [task_id.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .expect("task action epoch");
    let command_id = CommandId::from_bytes(fixed_uuid_v7(tail + 10)).expect("command");
    let turn_id = TurnId::from_bytes(fixed_uuid_v7(tail + 11)).expect("turn");
    let accepted = store
        .execute_for_test(CommandEnvelope {
            command_id,
            client_id,
            task_id: Some(task_id),
            issued_at_ms: 1_725_003_000_000,
            expected_task_revision: Some(next_revision),
            command: Command::SubmitProviderInput(
                SubmitProviderInputIntent::try_new(
                    agent_session_id,
                    3,
                    turn_id,
                    u64::try_from(action_epoch).expect("epoch"),
                    None,
                    None,
                    ProviderInputAction::SendNow {
                        text: "typed delivery".into(),
                        wait: false,
                    },
                )
                .expect("intent"),
            ),
        })
        .expect("accept provider input");
    let CommandReceipt::Accepted { operation_id, .. } = accepted else {
        panic!("provider input must be accepted: {accepted:?}");
    };
    let claim = store
        .claim_next_dispatch(Duration::from_secs(30))
        .expect("claim")
        .expect("ready");
    let permit = store.begin_dispatch(&claim).expect("begin");
    (operation_id, permit)
}

fn identity_from_effect(effect: &Effect) -> ProviderInputDeliveryIdentity {
    match effect {
        Effect::DeliverProviderInput {
            task_id,
            operation_id,
            command_id,
            client_id,
            agent_session_id,
            provider_kind,
            provider_session_id,
            runtime_generation,
            action_epoch,
            turn_id,
            question_id,
            approval_id,
            ..
        } => ProviderInputDeliveryIdentity {
            task_id: *task_id,
            operation_id: *operation_id,
            command_id: *command_id,
            client_id: *client_id,
            agent_session_id: *agent_session_id,
            provider_kind: provider_kind.clone(),
            provider_session_id: provider_session_id.clone(),
            runtime_generation: *runtime_generation,
            action_epoch: *action_epoch,
            turn_id: *turn_id,
            question_id: *question_id,
            approval_id: *approval_id,
        },
        other => panic!("expected DeliverProviderInput, got {other:?}"),
    }
}

fn delivered_event_count(store: &KernelStore) -> i64 {
    store
        .conn
        .query_row(
            "SELECT COUNT(*) FROM events WHERE event_type = 'provider_input.delivered'",
            [],
            |row| row.get(0),
        )
        .expect("count delivered")
}

#[test]
fn generic_completion_cannot_settle_provider_input() {
    let dir = TempDir::new().expect("tempdir");
    let mut store = KernelStore::open(&dir.path().join("hold.sqlite3")).expect("open");
    let (operation_id, permit) = seed_provider_dispatch(&mut store, 0x61);
    assert_eq!(
        store.record_dispatch_completion(&permit, DispatchCompletion::Settled),
        Err(StoreError::InvalidDispatchTransition)
    );
    assert!(matches!(
        store.operation_status(operation_id).expect("status"),
        Some(OperationState::Accepted)
    ));
    assert_eq!(delivered_event_count(&store), 0);
}

#[test]
fn plan_only_and_bind_only_cannot_settle_provider_input() {
    let dir = TempDir::new().expect("tempdir");
    let mut store = KernelStore::open(&dir.path().join("bind.sqlite3")).expect("open");
    let (operation_id, permit) = seed_provider_dispatch(&mut store, 0x41);
    let identity = identity_from_effect(permit.effect());
    let plan = sequence_bounded_input(ACTION_PROVIDER_SEND_NOW, b"typed delivery").expect("plan");
    assert_eq!(
        plan.settlement_hold(),
        crate::providers::input::ProviderInputBridgeHold::DestinationOutboxAbsent
    );
    let mut port = BoundProviderInputPort::bind(identity.clone());
    assert_eq!(
        deliver_through_capability(&mut port, identity.clone(), plan.clone()),
        Err(ProviderInputDeliveryError::RuntimeAuthorityAbsent)
    );
    let mut stale = identity;
    stale.runtime_generation = stale.runtime_generation.saturating_add(1);
    assert_eq!(
        deliver_through_capability(&mut port, stale, plan),
        Err(ProviderInputDeliveryError::StaleGeneration)
    );
    assert_eq!(
        store.record_dispatch_completion(&permit, DispatchCompletion::Settled),
        Err(StoreError::InvalidDispatchTransition)
    );
    assert!(matches!(
        store.operation_status(operation_id).expect("status"),
        Some(OperationState::Accepted)
    ));
    assert_eq!(delivered_event_count(&store), 0);
}

#[cfg(windows)]
#[test]
fn live_write_receipt_settles_and_rejects_stale_action_or_bytes() {
    use crate::providers::session::{
        LaunchNonce, ProviderLaunchMode, ProviderLaunchOutcome, ProviderLaunchSpec,
        ProviderRuntimeLaunchRequest, RuntimeCorrelation,
    };
    use crate::services::ProcessManager;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    let dir = TempDir::new().expect("tempdir");
    let mut store = KernelStore::open(&dir.path().join("receipt.sqlite3")).expect("open");
    let (operation_id, permit) = seed_provider_dispatch(&mut store, 0x71);
    let identity = identity_from_effect(permit.effect());
    let action = match permit.effect() {
        Effect::DeliverProviderInput { action, .. } => action.clone(),
        other => panic!("expected DeliverProviderInput, got {other:?}"),
    };
    let plan = sequence_provider_action(&action).expect("plan");
    let manager = ProcessManager::new();
    let mut launcher = manager.provider_process_launcher();
    let executable = crate::providers::capabilities::ProviderExecutable::from_path(PathBuf::from(
        r"C:\Windows\System32\cmd.exe",
    ))
    .expect("cmd");
    let resource_id = crate::domain::ResourceId::new();
    let launch_nonce = LaunchNonce::new();
    let request = ProviderRuntimeLaunchRequest::sealed(
        RuntimeCorrelation::sealed(
            identity.task_id,
            identity.agent_session_id,
            identity.provider_kind,
            identity.runtime_generation,
            identity.action_epoch,
            launch_nonce,
        ),
        ProviderLaunchSpec::sealed(
            identity.provider_kind,
            executable,
            ProviderLaunchMode::ResumeExact(identity.provider_session_id.clone()),
            Vec::new(),
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            BTreeMap::new(),
            crate::providers::capabilities::ProviderCapabilities {
                kind: identity.provider_kind,
                version: crate::providers::capabilities::ProviderVersion::new("1.0.0-test")
                    .expect("version"),
                auth_state: crate::providers::capabilities::ProviderAuthState::Unknown,
                exact_resume: crate::providers::capabilities::CapabilitySupport::Supported,
                semantic_events: crate::providers::capabilities::CapabilitySupport::Unsupported,
                provider_session_id: crate::providers::capabilities::CapabilitySupport::Supported,
                build_launch: crate::providers::capabilities::CapabilitySupport::Supported,
                parse_signal: crate::providers::capabilities::CapabilitySupport::Unsupported,
                cooperative_stop: crate::providers::capabilities::CapabilitySupport::Unsupported,
                observe_quota: crate::providers::capabilities::CapabilitySupport::Unsupported,
                evidence: vec![],
            },
            identity.task_id,
            resource_id,
            crate::domain::TerminalId::new(),
            identity.runtime_generation,
            launch_nonce,
        ),
    );
    let ProviderLaunchOutcome::Started(mut lease) = launcher.launch(&request) else {
        panic!("expected live permit");
    };
    let handle = launcher
        .write_handle(identity.clone(), &lease)
        .expect("write handle");
    let mut stale = identity.clone();
    stale.runtime_generation = stale.runtime_generation.saturating_add(1);
    assert_eq!(
        handle.write_action(&stale, &action, &plan),
        Err(ProviderInputDeliveryError::StaleGeneration)
    );
    let wrong_action = ProviderInputAction::StopTurn;
    assert_eq!(
        handle.write_action(&identity, &wrong_action, &plan),
        Err(ProviderInputDeliveryError::ActionMismatch)
    );
    let wrong_plan =
        sequence_bounded_input(ACTION_PROVIDER_SEND_NOW, b"other bytes").expect("wrong");
    assert_eq!(
        handle.write_action(&identity, &action, &wrong_plan),
        Err(ProviderInputDeliveryError::BytesMismatch)
    );
    let receipt = handle
        .write_action(&identity, &action, &plan)
        .expect("live write");
    let settled = store
        .settle_provider_input_delivery(&permit, &receipt)
        .expect("settle");
    assert!(matches!(settled, OperationState::Settled { .. }));
    assert_eq!(delivered_event_count(&store), 1);
    assert!(matches!(
        store.operation_status(operation_id).expect("status"),
        Some(OperationState::Settled { .. })
    ));
    let _ = launcher.stop_and_join(&mut lease);
}
