//! Task 4.7: provider input, questions, approvals, and turn control.

use devmanager::client::action::{
    catalog, ActionRisk, ActionScope, ACTION_PROVIDER_ANSWER_QUESTION,
    ACTION_PROVIDER_NEW_CONVERSATION, ACTION_PROVIDER_QUEUE_FOLLOW_UP,
    ACTION_PROVIDER_RESOLVE_APPROVAL, ACTION_PROVIDER_SEND_NOW, ACTION_PROVIDER_STEER_CURRENT_TURN,
    ACTION_PROVIDER_STOP_TURN,
};
use devmanager::domain::{
    decide, AgentSessionId, ClientId, Command, CommandEnvelope, CommandId,
    PresentProviderApprovalIntent, PresentProviderQuestionIntent, ProviderInputAction, QuestionId,
    RejectionCode, SubmitProviderInputIntent, TaskId, TurnId,
};

fn fixed_uuid_v7(tail: u8) -> [u8; 16] {
    [
        0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        tail,
    ]
}

fn seed_open_task_with_agent(
    bus: &mut devmanager::kernel::CommandBus,
    tail: u8,
) -> (TaskId, AgentSessionId, u64, u64, ClientId) {
    use devmanager::domain::command::CommandReceipt;
    use devmanager::domain::{
        AgentRole, AgentSessionFacts, AgentSessionLifecycle, CreateTaskIntent, EnvironmentId,
        ProjectId, TaskActivity, TaskAssignment, TaskAttention, TaskConnectivity, TaskLifecycle,
        WorkspaceRef,
    };

    let client_id = ClientId::from_bytes(fixed_uuid_v7(tail)).expect("client");
    let task_id = TaskId::from_bytes(fixed_uuid_v7(tail + 1)).expect("task");
    let agent_session_id = AgentSessionId::from_bytes(fixed_uuid_v7(tail + 2)).expect("agent");
    let create = bus
        .execute(CommandEnvelope {
            command_id: CommandId::from_bytes(fixed_uuid_v7(tail + 3)).expect("create cmd"),
            client_id,
            task_id: None,
            issued_at_ms: 1_725_000_000_100,
            expected_task_revision: None,
            command: Command::CreateTask(CreateTaskIntent {
                id: task_id,
                environment_id: EnvironmentId::from_bytes(fixed_uuid_v7(tail + 4))
                    .expect("environment"),
                title: "Provider input".into(),
                description: None,
                project_id: ProjectId::from_bytes(fixed_uuid_v7(tail + 5)).expect("project"),
                workspace: WorkspaceRef::Main,
                assignment: TaskAssignment::LocalOwner,
                created_at_ms: 1_725_000_000_000,
                connectivity: TaskConnectivity::Connected,
                attention: TaskAttention::None,
                activity: TaskActivity::Idle,
                review_readiness: devmanager::domain::ReviewReadiness::NotReady,
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
                    provider_kind: "codex".into(),
                    provider_session_id: None,
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
    let snapshot = bus
        .task_snapshot(task_id)
        .expect("load snapshot")
        .expect("task exists");
    assert_eq!(snapshot.task.lifecycle, TaskLifecycle::Open);
    (
        task_id,
        agent_session_id,
        snapshot.task.action_epoch,
        next_revision,
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
    let mut bus = CommandBus::open(&path).expect("open");
    let (task_id, agent_session_id, action_epoch, revision, client_id) =
        seed_open_task_with_agent(&mut bus, 0x40);
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
    use devmanager::kernel::CommandBus;
    use tempfile::TempDir;

    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("provider-input-digest.sqlite3");
    let mut bus = CommandBus::open(&path).expect("open");
    let (task_id, agent_session_id, action_epoch, revision, client_id) =
        seed_open_task_with_agent(&mut bus, 0x50);
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
    let mut bus = CommandBus::open(&path).expect("open");
    let (task_id, agent_session_id, action_epoch, mut revision, client_id) =
        seed_open_task_with_agent(&mut bus, 0x60);
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
    use devmanager::domain::{ProviderWaitFence, SettleProviderWaitIntent};
    use devmanager::kernel::CommandBus;
    use tempfile::TempDir;

    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("provider-input-wait.sqlite3");
    let mut bus = CommandBus::open(&path).expect("open");
    let (task_id, agent_session_id, action_epoch, revision, client_id) =
        seed_open_task_with_agent(&mut bus, 0x70);
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
        task_revision: Some(after_wait),
        ..
    } = accepted
    else {
        panic!("expected accepted wait, got {accepted:?}");
    };

    let fence = ProviderWaitFence::new(
        command_id,
        task_id,
        action_epoch,
        agent_session_id,
        3,
        turn_id,
        None,
        None,
    );
    let bad = ProviderWaitFence::new(
        command_id,
        task_id,
        action_epoch,
        agent_session_id,
        4,
        turn_id,
        None,
        None,
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
    use devmanager::domain::{ProviderWaitFence, SettleProviderWaitIntent};
    use devmanager::kernel::CommandBus;
    use tempfile::TempDir;

    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("provider-input-wait-capacity.sqlite3");
    let mut bus = CommandBus::open(&path).expect("open");
    let (task_id, agent_session_id, action_epoch, mut revision, client_id) =
        seed_open_task_with_agent(&mut bus, 0xE0);
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
            task_revision: Some(after_wait),
            ..
        } = accepted
        else {
            panic!("wait input must be accepted");
        };

        let fence = ProviderWaitFence::new(
            wait_command_id,
            task_id,
            action_epoch,
            agent_session_id,
            3,
            turn_id,
            None,
            None,
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
    use devmanager::kernel::CommandBus;
    use tempfile::TempDir;

    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("provider-input-approval.sqlite3");
    let mut bus = CommandBus::open(&path).expect("open");
    let (task_id, agent_session_id, action_epoch, mut revision, client_id) =
        seed_open_task_with_agent(&mut bus, 0xF0);
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

    let loser = bus
        .execute(CommandEnvelope {
            command_id: CommandId::from_bytes(fixed_uuid_v7(0xFE)).expect("loser"),
            client_id,
            task_id: Some(task_id),
            issued_at_ms: winner_at + 1,
            expected_task_revision: Some(revision + 1),
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
    assert!(matches!(
        loser,
        CommandReceipt::Rejected {
            code: RejectionCode::AlreadyResolved,
            resolution: Some(_),
            ..
        }
    ));

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
        !ids.iter()
            .any(|id| id.to_ascii_lowercase().contains("restart")),
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
    use devmanager::kernel::CommandBus;
    use tempfile::TempDir;

    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("provider-input-catalog.sqlite3");
    let mut bus = CommandBus::open(&path).expect("open");
    let (task_id, agent_session_id, action_epoch, revision, client_id) =
        seed_open_task_with_agent(&mut bus, 0x80);
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
    use devmanager::domain::{ProviderWaitFence, SettleProviderWaitIntent};
    use devmanager::kernel::CommandBus;
    use tempfile::TempDir;

    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("provider-input-duplicates.sqlite3");
    let mut bus = CommandBus::open(&path).expect("open");
    let (task_id, agent_session_id, action_epoch, revision, client_id) =
        seed_open_task_with_agent(&mut bus, 0xA0);
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
    let mut bus = CommandBus::open(&path).expect("open wait store");
    let (task_id, agent_session_id, action_epoch, revision, client_id) =
        seed_open_task_with_agent(&mut bus, 0xB0);
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
        task_revision: Some(after_wait),
        ..
    } = accepted
    else {
        panic!("expected accepted wait, got {accepted:?}");
    };
    let fence = ProviderWaitFence::new(
        wait_command_id,
        task_id,
        action_epoch,
        agent_session_id,
        3,
        turn_id,
        None,
        None,
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
    let mut bus = CommandBus::open(&path).expect("open");
    let (task_id, agent_session_id, ..) = seed_open_task_with_agent(&mut bus, 0xC0);
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
        agent_session_id: AgentSessionId::from_bytes(fixed_uuid_v7(0xD4)).expect("agent"),
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
    let error = serde_json::from_slice::<Event>(&encoded).expect_err("invalid event payload");
    let rendered = error.to_string();
    assert!(
        rendered.contains("inconsistent") || rendered.contains("wait"),
        "unexpected error: {rendered}"
    );
}
