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
    ProviderRuntimeByteWriter, ProviderRuntimeWriteHandle, ACTION_PROVIDER_SEND_NOW,
};
use std::time::Duration;
use tempfile::TempDir;

fn fixed_uuid_v7(tail: u8) -> [u8; 16] {
    [
        0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        tail,
    ]
}

/// One accepted, still unclaimed provider-input effect.
struct AcceptedProviderDispatch {
    client_id: ClientId,
    task_id: TaskId,
    operation_id: OperationId,
}

fn seed_provider_dispatch(store: &mut KernelStore, tail: u8) -> (OperationId, DispatchPermit) {
    let seeded = seed_accepted_provider_dispatch(store, tail);
    let claim = store
        .claim_next_dispatch(Duration::from_secs(30))
        .expect("claim")
        .expect("ready");
    let permit = store.begin_dispatch(&claim).expect("begin");
    (seeded.operation_id, permit)
}

fn seed_accepted_provider_dispatch(store: &mut KernelStore, tail: u8) -> AcceptedProviderDispatch {
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
                        images: Vec::new(),
                    },
                )
                .expect("intent"),
            ),
        })
        .expect("accept provider input");
    let CommandReceipt::Accepted { operation_id, .. } = accepted else {
        panic!("provider input must be accepted: {accepted:?}");
    };
    AcceptedProviderDispatch {
        client_id,
        task_id,
        operation_id,
    }
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

struct AcceptingRuntimeWriter;

impl ProviderRuntimeByteWriter for AcceptingRuntimeWriter {
    fn write_provider_action(
        &self,
        _fence: &crate::process::registry::ManagedProcessFence,
        _identity: &ProviderInputDeliveryIdentity,
        _action: &ProviderInputAction,
        _logical_bytes: &[u8],
    ) -> Result<(), ProviderInputDeliveryError> {
        Ok(())
    }
}

fn settle_with_accepting_runtime(
    store: &mut KernelStore,
    permit: &DispatchPermit,
) -> OperationState {
    let identity = identity_from_effect(permit.effect());
    let action = match permit.effect() {
        Effect::DeliverProviderInput { action, .. } => action.clone(),
        other => panic!("expected DeliverProviderInput, got {other:?}"),
    };
    let plan = sequence_provider_action(&action).expect("provider delivery plan");
    let fence = crate::process::registry::ManagedProcessFence::new(
        crate::domain::operation::ResourceFence::new(
            crate::domain::ResourceId::new(),
            identity.runtime_generation,
        ),
        crate::process::identity::ProcessOwner::Task(identity.task_id),
        crate::process::identity::ManagedProcessIdentity::new(
            crate::process::identity::ManagedProcessId::new(7, 11).expect("pid"),
            std::env::current_exe().expect("test executable"),
        )
        .expect("managed identity"),
    );
    let handle =
        ProviderRuntimeWriteHandle::bind(identity.clone(), fence, Box::new(AcceptingRuntimeWriter))
            .expect("write handle");
    let receipt = handle
        .write_action(&identity, &action, &plan)
        .expect("live write");
    store
        .settle_provider_input_delivery(permit, &receipt)
        .expect("settle provider delivery")
}

#[test]
fn historical_settled_provider_turn_remains_queryable_after_a_later_turn_settles() {
    let dir = TempDir::new().expect("tempdir");
    let mut store = KernelStore::open(&dir.path().join("historical.sqlite3")).expect("open");
    let (first_operation_id, first_permit) = seed_provider_dispatch(&mut store, 0x81);
    let first_identity = identity_from_effect(first_permit.effect());
    assert!(matches!(
        settle_with_accepting_runtime(&mut store, &first_permit),
        OperationState::Settled { .. }
    ));

    let (revision, action_epoch): (i64, i64) = store
        .conn
        .query_row(
            "SELECT revision, action_epoch FROM tasks WHERE task_id = ?1",
            [first_identity.task_id.as_bytes().as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("current task fence");
    let second_command_id = CommandId::from_bytes(fixed_uuid_v7(0x8D)).expect("second command");
    let second = store
        .execute_for_test(CommandEnvelope {
            command_id: second_command_id,
            client_id: first_identity.client_id,
            task_id: Some(first_identity.task_id),
            issued_at_ms: 1_725_003_000_100,
            expected_task_revision: Some(u64::try_from(revision).expect("revision")),
            command: Command::SubmitProviderInput(
                SubmitProviderInputIntent::try_new(
                    first_identity.agent_session_id,
                    first_identity.runtime_generation,
                    first_identity.turn_id,
                    u64::try_from(action_epoch).expect("action epoch"),
                    None,
                    None,
                    ProviderInputAction::SteerCurrentTurn {
                        text: "second delivered turn".into(),
                    },
                )
                .expect("second provider input"),
            ),
        })
        .expect("accept second provider turn");
    let CommandReceipt::Accepted {
        operation_id: second_operation_id,
        ..
    } = second
    else {
        panic!("second provider turn must be accepted: {second:?}");
    };
    let second_claim = store
        .claim_next_dispatch(Duration::from_secs(30))
        .expect("claim second provider turn")
        .expect("second provider turn is dispatchable");
    let second_permit = store
        .begin_dispatch(&second_claim)
        .expect("begin second provider turn");
    assert!(matches!(
        settle_with_accepting_runtime(&mut store, &second_permit),
        OperationState::Settled { .. }
    ));

    for operation_id in [first_operation_id, second_operation_id] {
        assert!(matches!(
            store
                .operation_status(operation_id)
                .expect("provider operation status"),
            Some(OperationState::Settled { .. })
        ));
    }
}

#[test]
fn historical_action_epoch_replays_delivery_between_provider_inputs() {
    let dir = TempDir::new().expect("tempdir");
    let mut store = KernelStore::open(&dir.path().join("historical-epoch.sqlite3")).expect("open");
    let (_first_operation_id, first_permit) = seed_provider_dispatch(&mut store, 0x91);
    let first_identity = identity_from_effect(first_permit.effect());
    assert!(matches!(
        settle_with_accepting_runtime(&mut store, &first_permit),
        OperationState::Settled { .. }
    ));

    let (revision, action_epoch): (i64, i64) = store
        .conn
        .query_row(
            "SELECT revision, action_epoch FROM tasks WHERE task_id = ?1",
            [first_identity.task_id.as_bytes().as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("current task fence");
    let second = store
        .execute_for_test(CommandEnvelope {
            command_id: CommandId::from_bytes(fixed_uuid_v7(0x9D)).expect("second command"),
            client_id: first_identity.client_id,
            task_id: Some(first_identity.task_id),
            issued_at_ms: 1_725_003_000_100,
            expected_task_revision: Some(u64::try_from(revision).expect("revision")),
            command: Command::SubmitProviderInput(
                SubmitProviderInputIntent::try_new(
                    first_identity.agent_session_id,
                    first_identity.runtime_generation,
                    TurnId::from_bytes(fixed_uuid_v7(0x9E)).expect("second turn"),
                    u64::try_from(action_epoch).expect("action epoch"),
                    None,
                    None,
                    ProviderInputAction::SendNow {
                        text: "second delivered input".into(),
                        wait: false,
                        images: Vec::new(),
                    },
                )
                .expect("second provider input"),
            ),
        })
        .expect("accept second provider input");
    assert!(matches!(second, CommandReceipt::Accepted { .. }));
    let through_sequence: i64 = store
        .conn
        .query_row("SELECT MAX(sequence) FROM events", [], |row| row.get(0))
        .expect("latest sequence");

    assert_eq!(
        super::command_bus::historical_action_epoch_through(
            &store.conn,
            first_identity.task_id,
            u64::try_from(through_sequence).expect("sequence"),
        )
        .expect("complete task history must replay"),
        u64::try_from(action_epoch).expect("action epoch")
    );
}

#[test]
fn earlier_provider_write_settles_after_a_later_input_is_accepted() {
    let dir = TempDir::new().expect("tempdir");
    let mut store = KernelStore::open(&dir.path().join("pipelined.sqlite3")).expect("open");
    let (first_operation_id, first_permit) = seed_provider_dispatch(&mut store, 0xA1);
    let first_identity = identity_from_effect(first_permit.effect());

    let (revision, action_epoch): (i64, i64) = store
        .conn
        .query_row(
            "SELECT revision, action_epoch FROM tasks WHERE task_id = ?1",
            [first_identity.task_id.as_bytes().as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("current task fence");
    let second = store
        .execute_for_test(CommandEnvelope {
            command_id: CommandId::from_bytes(fixed_uuid_v7(0xAD)).expect("second command"),
            client_id: first_identity.client_id,
            task_id: Some(first_identity.task_id),
            issued_at_ms: 1_725_003_000_100,
            expected_task_revision: Some(u64::try_from(revision).expect("revision")),
            command: Command::SubmitProviderInput(
                SubmitProviderInputIntent::try_new(
                    first_identity.agent_session_id,
                    first_identity.runtime_generation,
                    first_identity.turn_id,
                    u64::try_from(action_epoch).expect("action epoch"),
                    None,
                    None,
                    ProviderInputAction::SteerCurrentTurn {
                        text: "second queued keystroke".into(),
                    },
                )
                .expect("second provider input"),
            ),
        })
        .expect("accept second provider input");
    let CommandReceipt::Accepted {
        operation_id: second_operation_id,
        ..
    } = second
    else {
        panic!("second provider input must be accepted: {second:?}");
    };

    assert!(matches!(
        settle_with_accepting_runtime(&mut store, &first_permit),
        OperationState::Settled { .. }
    ));
    let second_claim = store
        .claim_next_dispatch(Duration::from_secs(30))
        .expect("claim second provider input")
        .expect("second provider input is dispatchable");
    let second_permit = store
        .begin_dispatch(&second_claim)
        .expect("begin second provider input");
    assert!(matches!(
        settle_with_accepting_runtime(&mut store, &second_permit),
        OperationState::Settled { .. }
    ));

    for operation_id in [first_operation_id, second_operation_id] {
        assert!(matches!(
            store
                .operation_status(operation_id)
                .expect("provider operation status"),
            Some(OperationState::Settled { .. })
        ));
    }
}

#[test]
fn delivered_send_now_can_begin_a_new_conversation_turn() {
    let dir = TempDir::new().expect("tempdir");
    let mut store = KernelStore::open(&dir.path().join("follow-up.sqlite3")).expect("open");
    let (_, first_permit) = seed_provider_dispatch(&mut store, 0x91);
    let first_identity = identity_from_effect(first_permit.effect());
    assert!(matches!(
        settle_with_accepting_runtime(&mut store, &first_permit),
        OperationState::Settled { .. }
    ));

    let (revision, action_epoch): (i64, i64) = store
        .conn
        .query_row(
            "SELECT revision, action_epoch FROM tasks WHERE task_id = ?1",
            [first_identity.task_id.as_bytes().as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("current task fence");
    let follow_up_turn = TurnId::from_bytes(fixed_uuid_v7(0x9D)).expect("follow-up turn");
    assert_ne!(follow_up_turn, first_identity.turn_id);
    let follow_up = store
        .execute_for_test(CommandEnvelope {
            command_id: CommandId::from_bytes(fixed_uuid_v7(0x9E)).expect("follow-up command"),
            client_id: first_identity.client_id,
            task_id: Some(first_identity.task_id),
            issued_at_ms: 1_725_003_000_200,
            expected_task_revision: Some(u64::try_from(revision).expect("revision")),
            command: Command::SubmitProviderInput(
                SubmitProviderInputIntent::try_new(
                    first_identity.agent_session_id,
                    first_identity.runtime_generation,
                    follow_up_turn,
                    u64::try_from(action_epoch).expect("action epoch"),
                    None,
                    None,
                    ProviderInputAction::SendNow {
                        text: "follow-up turn".into(),
                        wait: false,
                        images: Vec::new(),
                    },
                )
                .expect("follow-up intent"),
            ),
        })
        .expect("execute follow-up");
    let CommandReceipt::Accepted {
        operation_id: follow_up_operation,
        ..
    } = follow_up
    else {
        panic!("delivered idle turn must admit a fresh SendNow: {follow_up:?}");
    };
    assert_eq!(
        store
            .operation_status(follow_up_operation)
            .expect("follow-up operation must preserve replay validation"),
        Some(OperationState::Accepted)
    );
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

#[test]
fn live_write_receipt_settles_and_rejects_stale_action_or_bytes() {
    struct AcceptingRuntimeWriter;
    impl ProviderRuntimeByteWriter for AcceptingRuntimeWriter {
        fn write_provider_action(
            &self,
            _fence: &crate::process::registry::ManagedProcessFence,
            _identity: &ProviderInputDeliveryIdentity,
            _action: &ProviderInputAction,
            _logical_bytes: &[u8],
        ) -> Result<(), ProviderInputDeliveryError> {
            Ok(())
        }
    }

    let dir = TempDir::new().expect("tempdir");
    let mut store = KernelStore::open(&dir.path().join("receipt.sqlite3")).expect("open");
    let (operation_id, permit) = seed_provider_dispatch(&mut store, 0x71);
    let identity = identity_from_effect(permit.effect());
    let action = match permit.effect() {
        Effect::DeliverProviderInput { action, .. } => action.clone(),
        other => panic!("expected DeliverProviderInput, got {other:?}"),
    };
    let plan = sequence_provider_action(&action).expect("plan");
    let resource_id = crate::domain::ResourceId::new();
    let fence = crate::process::registry::ManagedProcessFence::new(
        crate::domain::operation::ResourceFence::new(resource_id, identity.runtime_generation),
        crate::process::identity::ProcessOwner::Task(identity.task_id),
        crate::process::identity::ManagedProcessIdentity::new(
            crate::process::identity::ManagedProcessId::new(7, 11).expect("pid"),
            std::env::current_exe().expect("test executable"),
        )
        .expect("managed identity"),
    );
    let handle =
        ProviderRuntimeWriteHandle::bind(identity.clone(), fence, Box::new(AcceptingRuntimeWriter))
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
}

#[test]
fn expired_provider_dispatch_is_recovered_as_uncertain_without_replay() {
    let dir = TempDir::new().expect("tempdir");
    let mut store = KernelStore::open(&dir.path().join("expired.sqlite3")).expect("open");
    let (operation_id, permit) = seed_provider_dispatch(&mut store, 0x31);
    store
        .conn
        .execute(
            "UPDATE outbox SET leased_until_ms = 0 WHERE outbox_id = ?1",
            [permit.outbox_id().as_bytes().as_slice()],
        )
        .expect("expire simulated crashed dispatch");

    assert_eq!(
        store
            .recover_next_expired_dispatch(Duration::from_millis(1))
            .expect("recover"),
        Some(AmbiguityDisposition::Uncertain)
    );
    assert!(matches!(
        store.operation_status(operation_id).expect("status"),
        Some(OperationState::Uncertain { .. })
    ));
    assert!(store
        .claim_next_dispatch_for_destination(
            DestinationClass::ProviderInput,
            Duration::from_secs(30),
        )
        .expect("no retry")
        .is_none());
}

fn task_revision(store: &KernelStore, task_id: TaskId) -> u64 {
    let revision: i64 = store
        .conn
        .query_row(
            "SELECT revision FROM tasks WHERE task_id = ?1",
            [task_id.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .expect("task revision");
    u64::try_from(revision).expect("non-negative revision")
}

/// Advance the task's action epoch exactly as closing it does in the live
/// store; the accepted provider effect keeps the epoch it was planned with.
fn begin_close_task(store: &mut KernelStore, seeded: &AcceptedProviderDispatch, tail: u8) {
    let revision = task_revision(store, seeded.task_id);
    store
        .execute_for_test(CommandEnvelope {
            command_id: CommandId::from_bytes(fixed_uuid_v7(tail)).expect("close cmd"),
            client_id: seeded.client_id,
            task_id: Some(seeded.task_id),
            issued_at_ms: 1_725_004_000_000,
            expected_task_revision: Some(revision),
            command: Command::BeginCloseTask,
        })
        .expect("begin close task");
}

fn provider_outbox_id(store: &KernelStore, operation_id: OperationId) -> OutboxId {
    let bytes: Vec<u8> = store
        .conn
        .query_row(
            "SELECT outbox_id FROM outbox WHERE operation_id = ?1",
            [operation_id.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .expect("outbox id");
    let bytes: [u8; 16] = bytes.as_slice().try_into().expect("outbox id length");
    OutboxId::from_bytes(bytes).expect("outbox id")
}

fn outbox_row_facts(
    store: &KernelStore,
    operation_id: OperationId,
) -> (String, Option<String>, i64, i64) {
    store
        .conn
        .query_row(
            "SELECT state, last_error_class, attempts, lease_generation
             FROM outbox WHERE operation_id = ?1",
            [operation_id.as_bytes().as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("outbox row")
}

/// The live-store shape: an accepted provider effect whose task action epoch
/// moved on. Before this converged, every maintenance tick re-examined the row,
/// drew the same `StaleFence`, and logged one identical line for the life of
/// the store (28,154 lease generations and 8,278 log lines on one dev profile).
#[test]
fn a_permanently_stale_provider_row_is_retired_and_the_next_pass_is_idle() {
    let dir = TempDir::new().expect("tempdir");
    let mut store = KernelStore::open(&dir.path().join("kernel.sqlite3")).expect("open");
    let seeded = seed_accepted_provider_dispatch(&mut store, 0x30);
    begin_close_task(&mut store, &seeded, 0x3c);

    let runtime = crate::providers::dispatch::ProviderDispatchRuntime::unbound_for_test();
    assert_eq!(
        runtime.run_once(&mut store).expect("first pass converges"),
        crate::providers::dispatch::ProviderDispatchOutcome::Idle
    );

    let (state, error_class, attempts, lease_generation) =
        outbox_row_facts(&store, seeded.operation_id);
    assert_eq!(state, "failed");
    assert_eq!(error_class.as_deref(), Some("stale_fence"));
    assert_eq!(attempts, 0, "a retired row is never dispatched");
    assert_eq!(lease_generation, 0, "a retired row is never leased");
    assert!(matches!(
        store
            .operation_status(seeded.operation_id)
            .expect("operation status"),
        Some(OperationState::Failed { .. })
    ));

    assert_eq!(
        runtime.run_once(&mut store).expect("second pass is idle"),
        crate::providers::dispatch::ProviderDispatchOutcome::Idle
    );
    let (state, _, _, lease_generation_after) = outbox_row_facts(&store, seeded.operation_id);
    assert_eq!(state, "failed");
    assert_eq!(
        lease_generation_after, lease_generation,
        "a retired row is never re-leased"
    );
}

/// Only a refusal whose evidence can never move back retires a row. Everything
/// else - a session that may reopen, a generation still catching up, an
/// operation reconciliation still owns - keeps its pending row and its ordinary
/// claim, attempt and defer behaviour.
#[test]
fn a_row_whose_fence_can_still_pass_is_never_retired() {
    use crate::kernel::command_bus::StaleFenceCheck;

    for check in [
        StaleFenceCheck::OperationTerminal,
        StaleFenceCheck::TaskActionEpochAdvanced,
        StaleFenceCheck::ProviderRuntimeGenerationAdvanced,
    ] {
        assert!(check.is_permanent(), "{} must be permanent", check.as_str());
    }
    let transient = [
        StaleFenceCheck::OperationUncertain,
        StaleFenceCheck::TaskMissing,
        StaleFenceCheck::TaskActionEpochBehind,
        StaleFenceCheck::TaskLifecycleMoved,
        StaleFenceCheck::AgentSessionMissing,
        StaleFenceCheck::AgentSessionForeign,
        StaleFenceCheck::AgentSessionClosed,
        StaleFenceCheck::ProviderIdentityMoved,
        StaleFenceCheck::ProviderRuntimeGenerationBehind,
        StaleFenceCheck::ResourceOwnershipMoved,
    ];
    for check in transient {
        assert!(
            !check.is_permanent(),
            "{} must not retire a row",
            check.as_str()
        );
    }

    let dir = TempDir::new().expect("tempdir");
    let mut store = KernelStore::open(&dir.path().join("kernel.sqlite3")).expect("open");
    let seeded = seed_accepted_provider_dispatch(&mut store, 0x50);
    let outbox_id = provider_outbox_id(&store, seeded.operation_id);

    for check in transient {
        let refused = store.with_immediate_transaction(|tx| {
            let row = load_outbox_row_by_id(tx, outbox_id)?.ok_or(StoreError::Corruption)?;
            crate::kernel::command_bus::retire_stale_fenced_dispatch_in_tx(
                tx,
                &row,
                check,
                1_800_000_000_000,
            )
        });
        assert!(
            matches!(refused, Err(StoreError::InvalidDispatchTransition)),
            "{} retired a row: {refused:?}",
            check.as_str()
        );
    }

    let (state, error_class, attempts, lease_generation) =
        outbox_row_facts(&store, seeded.operation_id);
    assert_eq!(state, "pending");
    assert_eq!(error_class, None);
    assert_eq!(attempts, 0);
    assert_eq!(lease_generation, 0);

    // The ordinary lane still claims, attempts and defers this row rather than
    // retiring it: attempts return to zero and only the lease generation moves.
    let runtime = crate::providers::dispatch::ProviderDispatchRuntime::unbound_for_test();
    assert!(matches!(
        runtime.run_once(&mut store).expect("held pass"),
        crate::providers::dispatch::ProviderDispatchOutcome::Held { .. }
    ));
    let (state, error_class, attempts, lease_generation) =
        outbox_row_facts(&store, seeded.operation_id);
    assert_eq!(state, "pending");
    assert_eq!(error_class, None);
    assert_eq!(attempts, 0);
    assert_eq!(lease_generation, 1);
    assert!(matches!(
        store
            .operation_status(seeded.operation_id)
            .expect("operation status"),
        Some(OperationState::Accepted)
    ));
}
