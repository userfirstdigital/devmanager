use std::fs;
use std::path::{Path, PathBuf};

use devmanager::domain::agent::{AgentRole, AgentSessionFacts, AgentSessionLifecycle};
use devmanager::domain::artifact::{ArtifactContentRef, ArtifactFacts, ArtifactKind, PrivacyClass};
use devmanager::domain::command::{
    decide, Command, CommandEnvelope, CreateTaskIntent, RejectionCode, RenameTaskIntent,
    SetTaskAttentionIntent,
};
use devmanager::domain::event::{
    apply, ApplyError, DomainEvent, Event, OperationAcceptedFact, OperationCancelledFact,
    OperationFailedFact, OperationSettledFact, OperationUncertainFact, EVENT_SCHEMA_VERSION,
};
use devmanager::domain::id::{
    AgentSessionId, ArtifactId, ClientId, CommandId, EnvironmentId, EventId, OperationId,
    ProjectId, RequestId, ResourceId, TaskId,
};
use devmanager::domain::operation::{
    CancellationReason, OperationErrorCode, OperationOutcome, OperationOutcomeKind, OperationState,
    OperationUncertaintyCode, OutcomeSource, ResourceFence, MAX_EXTERNAL_IDENTITY_BYTES,
};
use devmanager::domain::query::{
    Query, QueryEnvelope, QueryError, QueryOutcome, QueryReply, QueryResult,
};
use devmanager::domain::resource::{
    OwnerKind, ResourceFacts, ResourceKind, ResourceLifecycle, ResourceRecipe,
};
use devmanager::domain::snapshot::TaskSnapshot;
use devmanager::domain::task::{
    ReviewReadiness, TaskActivity, TaskAssignment, TaskAttention, TaskConnectivity, TaskFacts,
    TaskLifecycle, VisibleTaskStatus, WorkspaceRef,
};

fn fixed_uuid_v7(tail: u8) -> [u8; 16] {
    [
        0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        tail,
    ]
}

fn task_id(tail: u8) -> TaskId {
    TaskId::from_bytes(fixed_uuid_v7(tail)).expect("task id")
}
fn env_id(tail: u8) -> EnvironmentId {
    EnvironmentId::from_bytes(fixed_uuid_v7(tail)).expect("env id")
}
fn project_id(tail: u8) -> ProjectId {
    ProjectId::from_bytes(fixed_uuid_v7(tail)).expect("project id")
}
fn client_id(tail: u8) -> ClientId {
    ClientId::from_bytes(fixed_uuid_v7(tail)).expect("client id")
}
fn command_id(tail: u8) -> CommandId {
    CommandId::from_bytes(fixed_uuid_v7(tail)).expect("command id")
}
fn event_id(tail: u8) -> EventId {
    EventId::from_bytes(fixed_uuid_v7(tail)).expect("event id")
}
fn operation_id(tail: u8) -> OperationId {
    OperationId::from_bytes(fixed_uuid_v7(tail)).expect("operation id")
}
fn agent_id(tail: u8) -> AgentSessionId {
    AgentSessionId::from_bytes(fixed_uuid_v7(tail)).expect("agent id")
}
fn artifact_id(tail: u8) -> ArtifactId {
    ArtifactId::from_bytes(fixed_uuid_v7(tail)).expect("artifact id")
}
fn resource_id(tail: u8) -> ResourceId {
    ResourceId::from_bytes(fixed_uuid_v7(tail)).expect("resource id")
}
fn request_id(tail: u8) -> RequestId {
    RequestId::from_bytes(fixed_uuid_v7(tail)).expect("request id")
}

fn create_intent(task: TaskId) -> CreateTaskIntent {
    CreateTaskIntent {
        id: task,
        environment_id: env_id(0x10),
        title: "Ship kernel".into(),
        description: Some("Phase 1 domain".into()),
        project_id: project_id(0x11),
        workspace: WorkspaceRef::Main,
        assignment: TaskAssignment::LocalOwner,
        created_at_ms: 1_725_000_000_000,
        connectivity: TaskConnectivity::Connected,
        attention: TaskAttention::None,
        activity: TaskActivity::Idle,
        review_readiness: ReviewReadiness::NotReady,
    }
}

fn envelope(
    command_id: CommandId,
    task_id: Option<TaskId>,
    expected_task_revision: Option<u64>,
    command: Command,
) -> CommandEnvelope {
    CommandEnvelope {
        command_id,
        client_id: client_id(0x20),
        task_id,
        issued_at_ms: 1_725_000_000_100,
        expected_task_revision,
        command,
    }
}

fn domain_event(
    id: EventId,
    task_id: Option<TaskId>,
    sequence: u64,
    task_revision: Option<u64>,
    occurred_at_ms: i64,
    payload: Event,
) -> DomainEvent {
    DomainEvent {
        id,
        task_id,
        sequence,
        task_revision,
        occurred_at_ms,
        payload,
    }
}

fn next_revision(snapshot: Option<&TaskSnapshot>) -> u64 {
    match snapshot {
        None => 1,
        Some(snap) => snap.task.revision.checked_add(1).expect("revision"),
    }
}

fn create_task(snapshot: Option<TaskSnapshot>, task: TaskId, seq: u64, evt: u8) -> TaskSnapshot {
    let cmd = envelope(
        command_id(0x30),
        None,
        None,
        Command::CreateTask(create_intent(task)),
    );
    let payloads = decide(snapshot.as_ref(), &cmd).expect("create decide");
    assert_eq!(payloads.len(), 1);
    let event = domain_event(
        event_id(evt),
        Some(task),
        seq,
        Some(1),
        1_725_000_000_100,
        payloads[0].clone(),
    );
    apply(snapshot, &event).expect("create apply")
}

fn apply_decided(
    snapshot: Option<TaskSnapshot>,
    cmd: &CommandEnvelope,
    sequence: u64,
    evt: u8,
    occurred_at_ms: i64,
) -> Result<TaskSnapshot, RejectionCode> {
    let payloads = decide(snapshot.as_ref(), cmd)?;
    let mut current = snapshot;
    for (index, payload) in payloads.into_iter().enumerate() {
        let task_id = cmd
            .task_id
            .or_else(|| current.as_ref().map(|s| s.task.id))
            .or_else(|| match &payload {
                Event::TaskCreated { task, .. } => Some(task.id),
                _ => None,
            });
        let revision = if payload.is_task_mutation() {
            Some(next_revision(current.as_ref()))
        } else {
            current.as_ref().map(|s| s.task.revision)
        };
        let event = domain_event(
            event_id(evt.wrapping_add(index as u8)),
            task_id,
            sequence + index as u64,
            revision,
            occurred_at_ms,
            payload,
        );
        current = Some(apply(current, &event).expect("apply"));
    }
    Ok(current.expect("snapshot after apply"))
}

#[test]
fn create_task_emits_revision_one() {
    let task = task_id(0x01);
    let snap = create_task(None, task, 1, 0x40);
    assert_eq!(snap.task.id, task);
    assert_eq!(snap.task.revision, 1);
    assert_eq!(snap.task.lifecycle, TaskLifecycle::Open);
    assert_eq!(snap.task.action_epoch, 0);
    assert_eq!(snap.connectivity, TaskConnectivity::Connected);
    assert_eq!(snap.visible_status(), VisibleTaskStatus::Idle);
}

#[test]
fn rename_requires_expected_revision() {
    let task = task_id(0x02);
    let snap = create_task(None, task, 1, 0x41);
    let bad = envelope(
        command_id(0x31),
        Some(task),
        Some(0),
        Command::RenameTask(RenameTaskIntent {
            title: "Wrong revision".into(),
        }),
    );
    assert!(matches!(
        decide(Some(&snap), &bad),
        Err(RejectionCode::RevisionConflict)
    ));
    let good = envelope(
        command_id(0x32),
        Some(task),
        Some(1),
        Command::RenameTask(RenameTaskIntent {
            title: "Renamed".into(),
        }),
    );
    let renamed = apply_decided(Some(snap), &good, 2, 0x42, 1_725_000_000_200).expect("rename");
    assert_eq!(renamed.task.title, "Renamed");
    assert_eq!(renamed.task.revision, 2);
}

#[test]
fn closing_is_idempotent() {
    let task = task_id(0x03);
    let snap = create_task(None, task, 1, 0x43);
    let close = envelope(
        command_id(0x33),
        Some(task),
        Some(1),
        Command::BeginCloseTask,
    );
    let closing = apply_decided(Some(snap), &close, 2, 0x44, 1_725_000_000_210).expect("close");
    assert_eq!(closing.task.lifecycle, TaskLifecycle::Closing);
    assert_eq!(closing.task.revision, 2);
    let epoch = closing.task.action_epoch;
    let again = envelope(
        command_id(0x34),
        Some(task),
        Some(2),
        Command::BeginCloseTask,
    );
    let payloads = decide(Some(&closing), &again).expect("idempotent close");
    assert!(payloads.is_empty());
    assert_eq!(closing.task.action_epoch, epoch);
    assert_eq!(closing.task.revision, 2);
}

#[test]
fn closing_idempotent_still_requires_expected_revision() {
    let task = task_id(0xb0);
    let snap = create_task(None, task, 1, 0xb1);
    let close = envelope(
        command_id(0xb2),
        Some(task),
        Some(1),
        Command::BeginCloseTask,
    );
    let closing = apply_decided(Some(snap), &close, 2, 0xb3, 1_725_000_000_211).expect("close");
    let stale = envelope(
        command_id(0xb4),
        Some(task),
        Some(1),
        Command::BeginCloseTask,
    );
    assert!(matches!(
        decide(Some(&closing), &stale),
        Err(RejectionCode::RevisionConflict)
    ));
}

#[test]
fn closing_advances_action_epoch() {
    let task = task_id(0x04);
    let snap = create_task(None, task, 1, 0x45);
    let close = envelope(
        command_id(0x35),
        Some(task),
        Some(1),
        Command::BeginCloseTask,
    );
    let closing = apply_decided(Some(snap), &close, 2, 0x46, 1_725_000_000_220).expect("close");
    assert_eq!(closing.task.lifecycle, TaskLifecycle::Closing);
    assert_eq!(closing.task.action_epoch, 1);
    assert_eq!(closing.task.revision, 2);
}

#[test]
fn apply_rejects_stale_skipped_and_mismatched_task_revision() {
    let task = task_id(0xb5);
    let snap = create_task(None, task, 1, 0xb6);
    let stale = domain_event(
        event_id(0xb7),
        Some(task),
        2,
        Some(1),
        1_725_000_000_212,
        Event::TaskRenamed {
            title: "stale".into(),
        },
    );
    assert!(matches!(
        apply(Some(snap.clone()), &stale),
        Err(ApplyError::RevisionConflict)
    ));
    let skipped = domain_event(
        event_id(0xb8),
        Some(task),
        3,
        Some(3),
        1_725_000_000_213,
        Event::TaskRenamed {
            title: "skipped".into(),
        },
    );
    assert!(matches!(
        apply(Some(snap.clone()), &skipped),
        Err(ApplyError::RevisionConflict)
    ));
    let missing = domain_event(
        event_id(0xb9),
        Some(task),
        4,
        None,
        1_725_000_000_214,
        Event::TaskRenamed {
            title: "missing".into(),
        },
    );
    assert!(matches!(
        apply(Some(snap.clone()), &missing),
        Err(ApplyError::RevisionConflict)
    ));
    let mismatched_task = domain_event(
        event_id(0xba),
        Some(task_id(0xbb)),
        5,
        Some(2),
        1_725_000_000_215,
        Event::TaskRenamed {
            title: "other".into(),
        },
    );
    assert!(matches!(
        apply(Some(snap), &mismatched_task),
        Err(ApplyError::TaskMismatch)
    ));
}

#[test]
fn apply_rejects_cross_task_fact_injection_and_duplicate_registration() {
    let task = task_id(0xbc);
    let other = task_id(0xbd);
    let snap = create_task(None, task, 1, 0xbe);
    let foreign_agent = AgentSessionFacts {
        id: agent_id(0xbf),
        task_id: other,
        role: AgentRole::Primary,
        provider_kind: "claude".into(),
        provider_session_id: None,
        lifecycle: AgentSessionLifecycle::Open,
        runtime_generation: 0,
        revision: 0,
    };
    assert!(matches!(
        apply(
            Some(snap.clone()),
            &domain_event(
                event_id(0xc0),
                Some(task),
                2,
                Some(2),
                1_725_000_000_216,
                Event::AgentSessionRegistered {
                    agent: foreign_agent
                },
            ),
        ),
        Err(ApplyError::OwnershipConflict)
    ));
    let foreign_artifact = ArtifactFacts {
        id: artifact_id(0xc1),
        task_id: other,
        kind: ArtifactKind::Finding,
        label: "x".into(),
        content_ref: ArtifactContentRef::InlineUtf8("y".into()),
        sha256: [1u8; 32],
        privacy_class: PrivacyClass::LocalOnly,
        created_at_ms: 1,
    };
    assert!(matches!(
        apply(
            Some(snap.clone()),
            &domain_event(
                event_id(0xc2),
                Some(task),
                2,
                Some(2),
                1_725_000_000_217,
                Event::ArtifactRegistered {
                    artifact: foreign_artifact
                },
            ),
        ),
        Err(ApplyError::OwnershipConflict)
    ));
    let foreign_resource = ResourceFacts {
        id: resource_id(0xc3),
        task_id: Some(other),
        owner_kind: OwnerKind::Task,
        resource_kind: ResourceKind::Terminal,
        recipe: ResourceRecipe::Terminal { cols: 80, rows: 24 },
        lifecycle: ResourceLifecycle::Active,
        runtime_generation: 0,
        updated_at_ms: 1,
    };
    assert!(matches!(
        apply(
            Some(snap.clone()),
            &domain_event(
                event_id(0xc4),
                Some(task),
                2,
                Some(2),
                1_725_000_000_218,
                Event::ResourceRegistered {
                    resource: foreign_resource
                },
            ),
        ),
        Err(ApplyError::OwnershipConflict)
    ));
    let owned = ResourceFacts {
        id: resource_id(0xc5),
        task_id: Some(task),
        owner_kind: OwnerKind::Task,
        resource_kind: ResourceKind::Terminal,
        recipe: ResourceRecipe::Terminal { cols: 80, rows: 24 },
        lifecycle: ResourceLifecycle::Active,
        runtime_generation: 0,
        updated_at_ms: 1,
    };
    let after = apply(
        Some(snap),
        &domain_event(
            event_id(0xc6),
            Some(task),
            2,
            Some(2),
            1_725_000_000_219,
            Event::ResourceRegistered {
                resource: owned.clone(),
            },
        ),
    )
    .expect("owned resource");
    assert!(matches!(
        apply(
            Some(after),
            &domain_event(
                event_id(0xc7),
                Some(task),
                3,
                Some(3),
                1_725_000_000_220,
                Event::ResourceRegistered { resource: owned },
            ),
        ),
        Err(ApplyError::AlreadyExists)
    ));
}

#[test]
fn archived_task_rejects_new_runtime() {
    let task = task_id(0x05);
    let snap = create_task(None, task, 1, 0x47);
    let close = envelope(
        command_id(0x36),
        Some(task),
        Some(1),
        Command::BeginCloseTask,
    );
    let closing = apply_decided(Some(snap), &close, 2, 0x48, 1_725_000_000_230).expect("close");
    let archived = apply(
        Some(closing),
        &domain_event(
            event_id(0x49),
            Some(task),
            3,
            Some(3),
            1_725_000_000_240,
            Event::TaskArchived,
        ),
    )
    .expect("archive");
    assert_eq!(archived.task.lifecycle, TaskLifecycle::Archived);
    let agent = AgentSessionFacts {
        id: agent_id(0x50),
        task_id: task,
        role: AgentRole::Primary,
        provider_kind: "claude".into(),
        provider_session_id: None,
        lifecycle: AgentSessionLifecycle::Open,
        runtime_generation: 0,
        revision: 0,
    };
    assert!(matches!(
        decide(
            Some(&archived),
            &envelope(
                command_id(0x37),
                Some(task),
                Some(3),
                Command::RegisterAgentSession { agent },
            ),
        ),
        Err(RejectionCode::InvalidTransition)
    ));
    let resource = ResourceFacts {
        id: resource_id(0x51),
        task_id: Some(task),
        owner_kind: OwnerKind::Task,
        resource_kind: ResourceKind::Terminal,
        recipe: ResourceRecipe::Terminal { cols: 80, rows: 24 },
        lifecycle: ResourceLifecycle::Active,
        runtime_generation: 0,
        updated_at_ms: 1_725_000_000_250,
    };
    assert!(matches!(
        decide(
            Some(&archived),
            &envelope(
                command_id(0x38),
                Some(task),
                Some(3),
                Command::RegisterResource { resource },
            ),
        ),
        Err(RejectionCode::InvalidTransition)
    ));
}

#[test]
fn agent_and_resource_must_reference_same_task() {
    let task = task_id(0x06);
    let other = task_id(0x07);
    let snap = create_task(None, task, 1, 0x4a);
    let agent = AgentSessionFacts {
        id: agent_id(0x52),
        task_id: other,
        role: AgentRole::Primary,
        provider_kind: "codex".into(),
        provider_session_id: Some("sess".into()),
        lifecycle: AgentSessionLifecycle::Open,
        runtime_generation: 0,
        revision: 0,
    };
    assert!(matches!(
        decide(
            Some(&snap),
            &envelope(
                command_id(0x39),
                Some(task),
                Some(1),
                Command::RegisterAgentSession { agent },
            ),
        ),
        Err(RejectionCode::OwnershipConflict)
    ));
    let resource = ResourceFacts {
        id: resource_id(0x53),
        task_id: Some(other),
        owner_kind: OwnerKind::Task,
        resource_kind: ResourceKind::BrowserContext,
        recipe: ResourceRecipe::Browser {
            start_url: "https://example.com".into(),
        },
        lifecycle: ResourceLifecycle::Active,
        runtime_generation: 0,
        updated_at_ms: 1_725_000_000_260,
    };
    assert!(matches!(
        decide(
            Some(&snap),
            &envelope(
                command_id(0x3a),
                Some(task),
                Some(1),
                Command::RegisterResource { resource },
            ),
        ),
        Err(RejectionCode::OwnershipConflict)
    ));
}

#[test]
fn visible_status_precedence_is_deterministic() {
    let task = task_id(0x08);
    let mut snap = create_task(None, task, 1, 0x4b);
    assert_eq!(snap.visible_status(), VisibleTaskStatus::Idle);
    snap.review_readiness = ReviewReadiness::Ready;
    assert_eq!(snap.visible_status(), VisibleTaskStatus::ReadyForReview);
    snap.activity = TaskActivity::Settling;
    assert_eq!(snap.visible_status(), VisibleTaskStatus::Settling);
    snap.activity = TaskActivity::Working;
    assert_eq!(snap.visible_status(), VisibleTaskStatus::Working);
    snap.attention = TaskAttention::NeedsAnswer;
    assert_eq!(snap.visible_status(), VisibleTaskStatus::NeedsAnswer);
    snap.attention = TaskAttention::NeedsApproval;
    assert_eq!(snap.visible_status(), VisibleTaskStatus::NeedsApproval);
    snap.attention = TaskAttention::UncertainOutcome;
    assert_eq!(snap.visible_status(), VisibleTaskStatus::UncertainOutcome);
    snap.attention = TaskAttention::Failed;
    assert_eq!(snap.visible_status(), VisibleTaskStatus::Failed);
    snap.connectivity = TaskConnectivity::Disconnected;
    assert_eq!(snap.visible_status(), VisibleTaskStatus::Disconnected);
}

#[test]
fn accepted_side_effect_is_not_settled() {
    let task = task_id(0x09);
    let snap = create_task(None, task, 1, 0x4c);
    let resource = ResourceFacts {
        id: resource_id(0x54),
        task_id: Some(task),
        owner_kind: OwnerKind::Task,
        resource_kind: ResourceKind::Terminal,
        recipe: ResourceRecipe::Terminal {
            cols: 120,
            rows: 40,
        },
        lifecycle: ResourceLifecycle::Active,
        runtime_generation: 0,
        updated_at_ms: 1_725_000_000_270,
    };
    let payloads = decide(
        Some(&snap),
        &envelope(
            command_id(0x3b),
            Some(task),
            Some(1),
            Command::RegisterResource { resource },
        ),
    )
    .expect("register resource");
    assert!(!payloads
        .iter()
        .any(|event| matches!(event, Event::OperationSettled(_))));
    assert!(matches!(
        payloads.as_slice(),
        [Event::ResourceRegistered { .. }]
    ));
    assert!(!matches!(
        OperationState::Accepted,
        OperationState::Settled { .. }
    ));
    let reply = QueryReply {
        request_id: request_id(0x60),
        outcome: QueryOutcome::Ok(QueryResult::OperationStatus {
            operation_id: operation_id(0x61),
            state: OperationState::Accepted,
        }),
    };
    match reply.outcome {
        QueryOutcome::Ok(QueryResult::OperationStatus { state, .. }) => {
            assert!(matches!(state, OperationState::Accepted));
        }
        QueryOutcome::Err(_) => panic!("expected ok"),
    }
    let _query = QueryEnvelope {
        request_id: request_id(0x62),
        client_id: client_id(0x21),
        task_id: Some(task),
        query: Query::OperationStatus {
            operation_id: operation_id(0x61),
        },
    };
    let _ = QueryError::NotFound;
}

#[test]
fn release_acceptance_yields_releasing_not_released() {
    let task = task_id(0xd0);
    let snap = create_task(None, task, 1, 0xd1);
    let resource = ResourceFacts {
        id: resource_id(0xd2),
        task_id: Some(task),
        owner_kind: OwnerKind::Task,
        resource_kind: ResourceKind::Terminal,
        recipe: ResourceRecipe::Terminal { cols: 80, rows: 24 },
        lifecycle: ResourceLifecycle::Active,
        runtime_generation: 7,
        updated_at_ms: 1,
    };
    let registered = apply_decided(
        Some(snap),
        &envelope(
            command_id(0xd3),
            Some(task),
            Some(1),
            Command::RegisterResource {
                resource: resource.clone(),
            },
        ),
        2,
        0xd4,
        1_725_000_000_600,
    )
    .expect("register");
    let payloads = decide(
        Some(&registered),
        &envelope(
            command_id(0xd5),
            Some(task),
            Some(2),
            Command::ReleaseResource {
                resource_id: resource.id,
            },
        ),
    )
    .expect("release decide");
    assert!(
        !payloads
            .iter()
            .any(|event| matches!(event, Event::ResourceReleased { .. })),
        "acceptance must not claim ResourceReleased"
    );
    assert!(matches!(
        payloads.as_slice(),
        [Event::ResourceReleaseBegun {
            resource_id,
            runtime_generation: 7,
        }] if *resource_id == resource.id
    ));
    let releasing = apply_decided(
        Some(registered),
        &envelope(
            command_id(0xd5),
            Some(task),
            Some(2),
            Command::ReleaseResource {
                resource_id: resource.id,
            },
        ),
        3,
        0xd6,
        1_725_000_000_610,
    )
    .expect("release apply");
    assert_eq!(
        releasing.resources.get(&resource.id).map(|r| r.lifecycle),
        Some(ResourceLifecycle::Releasing)
    );
    assert_eq!(releasing.task.revision, 3);

    let retry = decide(
        Some(&releasing),
        &envelope(
            command_id(0xd7),
            Some(task),
            Some(3),
            Command::ReleaseResource {
                resource_id: resource.id,
            },
        ),
    )
    .expect("idempotent releasing retry");
    assert!(
        retry.is_empty(),
        "Releasing retry must be idempotent no-event"
    );

    let stale_retry = decide(
        Some(&releasing),
        &envelope(
            command_id(0xd8),
            Some(task),
            Some(2),
            Command::ReleaseResource {
                resource_id: resource.id,
            },
        ),
    );
    assert!(matches!(stale_retry, Err(RejectionCode::RevisionConflict)));
}

#[test]
fn resource_release_completion_is_generation_fenced() {
    let task = task_id(0xd9);
    let snap = create_task(None, task, 1, 0xda);
    let resource = ResourceFacts {
        id: resource_id(0xdb),
        task_id: Some(task),
        owner_kind: OwnerKind::Task,
        resource_kind: ResourceKind::Service,
        recipe: ResourceRecipe::Service {
            command: "echo".into(),
        },
        lifecycle: ResourceLifecycle::Active,
        runtime_generation: 3,
        updated_at_ms: 1,
    };
    let registered = apply_decided(
        Some(snap),
        &envelope(
            command_id(0xdc),
            Some(task),
            Some(1),
            Command::RegisterResource {
                resource: resource.clone(),
            },
        ),
        2,
        0xdd,
        1_725_000_000_620,
    )
    .expect("register");

    assert!(matches!(
        apply(
            Some(registered.clone()),
            &domain_event(
                event_id(0xf1),
                Some(task),
                3,
                Some(3),
                1_725_000_000_625,
                Event::ResourceReleased {
                    resource_id: resource.id,
                    runtime_generation: 3,
                },
            ),
        ),
        Err(ApplyError::InvalidTransition)
    ));

    let releasing = apply_decided(
        Some(registered),
        &envelope(
            command_id(0xde),
            Some(task),
            Some(2),
            Command::ReleaseResource {
                resource_id: resource.id,
            },
        ),
        3,
        0xdf,
        1_725_000_000_630,
    )
    .expect("begin release");

    assert!(matches!(
        apply(
            Some(releasing.clone()),
            &domain_event(
                event_id(0xe0),
                Some(task),
                4,
                Some(4),
                1_725_000_000_640,
                Event::ResourceReleased {
                    resource_id: resource.id,
                    runtime_generation: 99,
                },
            ),
        ),
        Err(ApplyError::InvalidTransition)
    ));
    assert!(matches!(
        apply(
            Some(create_task(None, task, 1, 0xe1)),
            &domain_event(
                event_id(0xe2),
                Some(task),
                2,
                Some(2),
                1_725_000_000_650,
                Event::ResourceReleased {
                    resource_id: resource.id,
                    runtime_generation: 3,
                },
            ),
        ),
        Err(ApplyError::NotFound)
    ));

    let active_direct = apply(
        Some(releasing.clone()),
        &domain_event(
            event_id(0xe3),
            Some(task),
            4,
            Some(4),
            1_725_000_000_660,
            Event::ResourceReleaseBegun {
                resource_id: resource.id,
                runtime_generation: 3,
            },
        ),
    );
    // already Releasing — duplicate begun should fail
    assert!(matches!(
        active_direct,
        Err(ApplyError::InvalidTransition | ApplyError::AlreadyExists)
    ));

    let completed = apply(
        Some(releasing.clone()),
        &domain_event(
            event_id(0xe4),
            Some(task),
            4,
            Some(4),
            1_725_000_000_670,
            Event::ResourceReleased {
                resource_id: resource.id,
                runtime_generation: 3,
            },
        ),
    )
    .expect("complete release");
    assert_eq!(
        completed.resources.get(&resource.id).map(|r| r.lifecycle),
        Some(ResourceLifecycle::Released)
    );
    assert_eq!(
        releasing
            .resources
            .get(&resource.id)
            .map(|r| r.updated_at_ms),
        Some(1_725_000_000_630),
        "ReleaseBegun must stamp updated_at_ms from occurred_at"
    );
    assert_eq!(
        completed
            .resources
            .get(&resource.id)
            .map(|r| r.updated_at_ms),
        Some(1_725_000_000_670),
        "ResourceReleased must stamp updated_at_ms from occurred_at"
    );

    assert!(matches!(
        apply(
            Some(completed.clone()),
            &domain_event(
                event_id(0xe5),
                Some(task),
                5,
                Some(5),
                1_725_000_000_680,
                Event::ResourceReleased {
                    resource_id: resource.id,
                    runtime_generation: 3,
                },
            ),
        ),
        Err(ApplyError::InvalidTransition)
    ));
    assert!(matches!(
        decide(
            Some(&completed),
            &envelope(
                command_id(0xe6),
                Some(task),
                Some(4),
                Command::ReleaseResource {
                    resource_id: resource.id,
                },
            ),
        ),
        Err(RejectionCode::InvalidTransition)
    ));
}

#[test]
fn task_archived_rejects_live_resources_but_allows_later_reopen_register() {
    let task = task_id(0xf0);
    let snap = create_task(None, task, 1, 0xf1);
    let resource = ResourceFacts {
        id: resource_id(0xf2),
        task_id: Some(task),
        owner_kind: OwnerKind::Task,
        resource_kind: ResourceKind::Terminal,
        recipe: ResourceRecipe::Terminal { cols: 80, rows: 24 },
        lifecycle: ResourceLifecycle::Active,
        runtime_generation: 1,
        updated_at_ms: 1,
    };
    let with_resource = apply(
        Some(snap),
        &domain_event(
            event_id(0xf3),
            Some(task),
            2,
            Some(2),
            1_725_000_000_700,
            Event::ResourceRegistered {
                resource: resource.clone(),
            },
        ),
    )
    .expect("register");
    let closing = apply(
        Some(with_resource),
        &domain_event(
            event_id(0xf4),
            Some(task),
            3,
            Some(3),
            1_725_000_000_710,
            Event::TaskCloseBegun { action_epoch: 1 },
        ),
    )
    .expect("close begun");
    assert!(
        matches!(
            apply(
                Some(closing.clone()),
                &domain_event(
                    event_id(0xf5),
                    Some(task),
                    4,
                    Some(4),
                    1_725_000_000_720,
                    Event::TaskArchived,
                ),
            ),
            Err(ApplyError::InvalidTransition)
        ),
        "archive with Active resource must fail"
    );

    let releasing = apply(
        Some(closing),
        &domain_event(
            event_id(0xf6),
            Some(task),
            4,
            Some(4),
            1_725_000_000_730,
            Event::ResourceReleaseBegun {
                resource_id: resource.id,
                runtime_generation: 1,
            },
        ),
    )
    .expect("release begun");
    let released = apply(
        Some(releasing),
        &domain_event(
            event_id(0xf7),
            Some(task),
            5,
            Some(5),
            1_725_000_000_740,
            Event::ResourceReleased {
                resource_id: resource.id,
                runtime_generation: 1,
            },
        ),
    )
    .expect("released");
    let archived = apply(
        Some(released),
        &domain_event(
            event_id(0xf8),
            Some(task),
            6,
            Some(6),
            1_725_000_000_750,
            Event::TaskArchived,
        ),
    )
    .expect("archive after release");
    assert_eq!(archived.task.lifecycle, TaskLifecycle::Archived);

    let reopened = apply(
        Some(archived),
        &domain_event(
            event_id(0xf9),
            Some(task),
            7,
            Some(7),
            1_725_000_000_760,
            Event::TaskReopened,
        ),
    )
    .expect("reopen");
    let later = ResourceFacts {
        id: resource_id(0xfa),
        task_id: Some(task),
        owner_kind: OwnerKind::Task,
        resource_kind: ResourceKind::Terminal,
        recipe: ResourceRecipe::Terminal { cols: 40, rows: 12 },
        lifecycle: ResourceLifecycle::Active,
        runtime_generation: 0,
        updated_at_ms: 1,
    };
    let after_register = apply(
        Some(reopened),
        &domain_event(
            event_id(0xfb),
            Some(task),
            8,
            Some(8),
            1_725_000_000_770,
            Event::ResourceRegistered { resource: later },
        ),
    )
    .expect("register after reopen");
    assert_eq!(after_register.task.lifecycle, TaskLifecycle::Open);
    assert!(after_register.resources.contains_key(&resource_id(0xfa)));
}

#[test]
fn host_owned_resource_cannot_enter_task_snapshot() {
    let task = task_id(0xe7);
    let snap = create_task(None, task, 1, 0xe8);
    let host = ResourceFacts {
        id: resource_id(0xe9),
        task_id: Some(task),
        owner_kind: OwnerKind::Host,
        resource_kind: ResourceKind::Terminal,
        recipe: ResourceRecipe::Terminal { cols: 80, rows: 24 },
        lifecycle: ResourceLifecycle::Active,
        runtime_generation: 0,
        updated_at_ms: 1,
    };
    assert!(matches!(
        decide(
            Some(&snap),
            &envelope(
                command_id(0xea),
                Some(task),
                Some(1),
                Command::RegisterResource {
                    resource: host.clone()
                },
            ),
        ),
        Err(RejectionCode::OwnershipConflict)
    ));
    assert!(matches!(
        apply(
            Some(snap),
            &domain_event(
                event_id(0xeb),
                Some(task),
                2,
                Some(2),
                1_725_000_000_690,
                Event::ResourceRegistered { resource: host },
            ),
        ),
        Err(ApplyError::OwnershipConflict)
    ));
}

#[test]
fn forged_close_epoch_is_rejected() {
    let task = task_id(0xec);
    let snap = create_task(None, task, 1, 0xed);
    assert_eq!(snap.task.action_epoch, 0);
    assert!(matches!(
        apply(
            Some(snap.clone()),
            &domain_event(
                event_id(0xee),
                Some(task),
                2,
                Some(2),
                1_725_000_000_700,
                Event::TaskCloseBegun { action_epoch: 0 },
            ),
        ),
        Err(ApplyError::InvalidTransition)
    ));
    assert!(matches!(
        apply(
            Some(snap.clone()),
            &domain_event(
                event_id(0xef),
                Some(task),
                2,
                Some(2),
                1_725_000_000_710,
                Event::TaskCloseBegun { action_epoch: 2 },
            ),
        ),
        Err(ApplyError::InvalidTransition)
    ));
    let closing = apply(
        Some(snap),
        &domain_event(
            event_id(0xf0),
            Some(task),
            2,
            Some(2),
            1_725_000_000_720,
            Event::TaskCloseBegun { action_epoch: 1 },
        ),
    )
    .expect("valid close");
    assert_eq!(closing.task.action_epoch, 1);
    assert_eq!(closing.task.lifecycle, TaskLifecycle::Closing);
}

#[test]
fn replay_derives_identical_snapshot() {
    let task = task_id(0x0a);
    let agent = AgentSessionFacts {
        id: agent_id(0x55),
        task_id: task,
        role: AgentRole::Primary,
        provider_kind: "claude".into(),
        provider_session_id: None,
        lifecycle: AgentSessionLifecycle::Open,
        runtime_generation: 0,
        revision: 0,
    };
    let artifact = ArtifactFacts {
        id: artifact_id(0x56),
        task_id: task,
        kind: ArtifactKind::Finding,
        label: "note".into(),
        content_ref: ArtifactContentRef::InlineUtf8("body".into()),
        sha256: [7u8; 32],
        privacy_class: PrivacyClass::LocalOnly,
        created_at_ms: 1_725_000_000_280,
    };
    let resource = ResourceFacts {
        id: resource_id(0x57),
        task_id: Some(task),
        owner_kind: OwnerKind::Task,
        resource_kind: ResourceKind::Service,
        recipe: ResourceRecipe::Service {
            command: "echo hi".into(),
        },
        lifecycle: ResourceLifecycle::Active,
        runtime_generation: 0,
        updated_at_ms: 1_725_000_000_290,
    };
    let commands = [
        envelope(
            command_id(0x3c),
            None,
            None,
            Command::CreateTask(create_intent(task)),
        ),
        envelope(
            command_id(0x3d),
            Some(task),
            Some(1),
            Command::RenameTask(RenameTaskIntent {
                title: "Replay title".into(),
            }),
        ),
        envelope(
            command_id(0x3e),
            Some(task),
            Some(2),
            Command::SetTaskAttention(SetTaskAttentionIntent {
                attention: TaskAttention::NeedsAnswer,
            }),
        ),
        envelope(
            command_id(0x3f),
            Some(task),
            Some(3),
            Command::RegisterAgentSession {
                agent: agent.clone(),
            },
        ),
        envelope(
            command_id(0x70),
            Some(task),
            Some(4),
            Command::SetPrimaryAgent {
                agent_session_id: agent.id,
            },
        ),
        envelope(
            command_id(0x71),
            Some(task),
            Some(5),
            Command::RegisterArtifact {
                artifact: artifact.clone(),
            },
        ),
        envelope(
            command_id(0x72),
            Some(task),
            Some(6),
            Command::RegisterResource {
                resource: resource.clone(),
            },
        ),
        envelope(
            command_id(0x73),
            Some(task),
            Some(7),
            Command::ReleaseResource {
                resource_id: resource.id,
            },
        ),
        envelope(
            command_id(0x74),
            Some(task),
            Some(9),
            Command::BeginCloseTask,
        ),
        envelope(command_id(0x75), Some(task), Some(10), Command::ReopenTask),
    ];
    let mut live: Option<TaskSnapshot> = None;
    let mut events = Vec::new();
    let mut sequence = 1u64;
    let mut evt_tail = 0x80u8;
    for cmd in &commands {
        let payloads = decide(live.as_ref(), cmd).expect("decide");
        for payload in payloads {
            let task_for_event = cmd.task_id.or(Some(task));
            let revision = if payload.is_task_mutation() {
                Some(next_revision(live.as_ref()))
            } else {
                live.as_ref().map(|s| s.task.revision)
            };
            let event = domain_event(
                event_id(evt_tail),
                task_for_event,
                sequence,
                revision,
                1_725_000_000_300 + sequence as i64,
                payload,
            );
            live = Some(apply(live, &event).expect("apply live"));
            events.push(event);
            sequence += 1;
            evt_tail = evt_tail.wrapping_add(1);

            // Release acceptance only begins teardown; completion is a later apply-only fact.
            if matches!(
                events.last().map(|e| &e.payload),
                Some(Event::ResourceReleaseBegun { .. })
            ) {
                let completion = domain_event(
                    event_id(evt_tail),
                    Some(task),
                    sequence,
                    Some(next_revision(live.as_ref())),
                    1_725_000_000_300 + sequence as i64,
                    Event::ResourceReleased {
                        resource_id: resource.id,
                        runtime_generation: resource.runtime_generation,
                    },
                );
                live = Some(apply(live, &completion).expect("release complete"));
                events.push(completion);
                sequence += 1;
                evt_tail = evt_tail.wrapping_add(1);
            }
        }
    }
    let live = live.expect("live");
    let mut replayed: Option<TaskSnapshot> = None;
    for event in &events {
        replayed = Some(apply(replayed, event).expect("replay"));
    }
    let replayed = replayed.expect("replayed");
    assert_eq!(live, replayed);
    assert_eq!(live.task.title, "Replay title");
    assert_eq!(live.attention, TaskAttention::NeedsAnswer);
    assert_eq!(live.primary_agent_id, Some(agent.id));
    assert!(live.agents.contains_key(&agent.id));
    assert!(live.artifacts.contains_key(&artifact.id));
    assert_eq!(
        live.resources.get(&resource.id).map(|r| r.lifecycle),
        Some(ResourceLifecycle::Released)
    );
    assert_eq!(live.task.lifecycle, TaskLifecycle::Open);
    assert_eq!(live.task.action_epoch, 1);
}

fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/domain/v1")
        .join(name)
}

fn assert_golden_event(fixture_name: &str, event: &Event) {
    let encoded = serde_json::to_value(event).expect("encode event");
    assert_eq!(encoded["schema_version"], EVENT_SCHEMA_VERSION);
    let expected_raw = fs::read_to_string(fixture_path(fixture_name)).expect("read fixture");
    let expected: serde_json::Value =
        serde_json::from_str(expected_raw.trim()).expect("parse fixture");
    assert_eq!(encoded, expected, "fixture mismatch {fixture_name}");
    let round_trip: Event = serde_json::from_value(encoded.clone()).expect("json round trip");
    assert_eq!(round_trip, *event);
    let packed = rmp_serde::to_vec(event).expect("msgpack encode");
    let msg_restored: Event = rmp_serde::from_slice(&packed).expect("msgpack decode");
    assert_eq!(msg_restored, *event);
}

#[test]
fn golden_event_serialization_fixtures() {
    let task = task_id(0x0b);
    let facts = TaskFacts {
        id: task,
        environment_id: env_id(0x10),
        title: "Ship kernel".into(),
        description: Some("Phase 1 domain".into()),
        project_id: project_id(0x11),
        workspace: WorkspaceRef::Main,
        assignment: TaskAssignment::LocalOwner,
        lifecycle: TaskLifecycle::Open,
        action_epoch: 0,
        revision: 1,
        created_at_ms: 1_725_000_000_000,
    };
    assert_golden_event(
        "task_created.json",
        &Event::TaskCreated {
            task: facts,
            connectivity: TaskConnectivity::Connected,
            attention: TaskAttention::None,
            activity: TaskActivity::Idle,
            review_readiness: ReviewReadiness::NotReady,
        },
    );
    assert_golden_event(
        "task_renamed.json",
        &Event::TaskRenamed {
            title: "Renamed".into(),
        },
    );
    assert_golden_event(
        "task_attention_set.json",
        &Event::TaskAttentionSet {
            attention: TaskAttention::NeedsApproval,
        },
    );
    assert_golden_event(
        "task_close_begun.json",
        &Event::TaskCloseBegun { action_epoch: 1 },
    );
    assert_golden_event("task_reopened.json", &Event::TaskReopened);
    assert_golden_event("task_archived.json", &Event::TaskArchived);
    let agent = AgentSessionFacts {
        id: agent_id(0x55),
        task_id: task,
        role: AgentRole::Primary,
        provider_kind: "claude".into(),
        provider_session_id: None,
        lifecycle: AgentSessionLifecycle::Open,
        runtime_generation: 0,
        revision: 0,
    };
    assert_golden_event(
        "agent_session_registered.json",
        &Event::AgentSessionRegistered {
            agent: agent.clone(),
        },
    );
    assert_golden_event(
        "primary_agent_set.json",
        &Event::PrimaryAgentSet {
            agent_session_id: agent.id,
        },
    );
    let artifact = ArtifactFacts {
        id: artifact_id(0x56),
        task_id: task,
        kind: ArtifactKind::Finding,
        label: "note".into(),
        content_ref: ArtifactContentRef::InlineUtf8("body".into()),
        sha256: [7u8; 32],
        privacy_class: PrivacyClass::LocalOnly,
        created_at_ms: 1_725_000_000_280,
    };
    assert_golden_event(
        "artifact_registered.json",
        &Event::ArtifactRegistered { artifact },
    );
    let resource = ResourceFacts {
        id: resource_id(0x57),
        task_id: Some(task),
        owner_kind: OwnerKind::Task,
        resource_kind: ResourceKind::Service,
        recipe: ResourceRecipe::Service {
            command: "echo hi".into(),
        },
        lifecycle: ResourceLifecycle::Active,
        runtime_generation: 0,
        updated_at_ms: 1_725_000_000_290,
    };
    assert_golden_event(
        "resource_registered.json",
        &Event::ResourceRegistered {
            resource: resource.clone(),
        },
    );
    assert_golden_event(
        "resource_release_begun.json",
        &Event::ResourceReleaseBegun {
            resource_id: resource.id,
            runtime_generation: 0,
        },
    );
    assert_golden_event(
        "resource_released.json",
        &Event::ResourceReleased {
            resource_id: resource.id,
            runtime_generation: 0,
        },
    );
    assert_golden_event(
        "operation_settled.json",
        &Event::OperationSettled(
            OperationSettledFact::new(
                command_id(0x3b),
                operation_id(0x61),
                1_725_000_000_400,
                vec![event_id(0x80)],
                Some(1),
                Some(resource_id(0x57)),
                Some(2),
            )
            .expect("settled"),
        ),
    );
    assert_golden_event(
        "operation_failed.json",
        &Event::OperationFailed(
            OperationFailedFact::new(
                command_id(0x3b),
                operation_id(0x61),
                1_725_000_000_410,
                OperationErrorCode::SideEffectFailed,
                Some(1),
                None,
                None,
            )
            .expect("failed"),
        ),
    );
    assert_golden_event(
        "operation_cancelled.json",
        &Event::OperationCancelled(
            OperationCancelledFact::new(
                command_id(0x3b),
                operation_id(0x61),
                1_725_000_000_420,
                CancellationReason::Superseded,
                Some(1),
                Some(resource_id(0x57)),
                Some(2),
            )
            .expect("cancelled"),
        ),
    );
    assert_golden_event(
        "operation_uncertain.json",
        &Event::OperationUncertain(
            OperationUncertainFact::new(
                command_id(0x3b),
                operation_id(0x61),
                1_725_000_000_430,
                OperationUncertaintyCode::AmbiguousDispatch,
                Some(1),
                Some(resource_id(0x57)),
                Some(2),
            )
            .expect("uncertain"),
        ),
    );

    let domain = DomainEvent {
        id: event_id(0x90),
        task_id: Some(task),
        sequence: 9,
        task_revision: Some(2),
        occurred_at_ms: 1_725_000_000_500,
        payload: Event::TaskRenamed {
            title: "wire".into(),
        },
    };
    let json = serde_json::to_string(&domain).expect("domain json");
    let json_rt: DomainEvent = serde_json::from_str(&json).expect("domain json rt");
    assert_eq!(json_rt, domain);
    let packed = rmp_serde::to_vec(&domain).expect("domain msgpack");
    let msg_rt: DomainEvent = rmp_serde::from_slice(&packed).expect("domain msgpack rt");
    assert_eq!(msg_rt, domain);

    let unknown_type = serde_json::json!({
        "event_type": "task.not_a_real_event",
        "schema_version": 1,
        "payload": {}
    });
    assert!(serde_json::from_value::<Event>(unknown_type).is_err());
    let unknown_version = serde_json::json!({
        "event_type": "task.renamed",
        "schema_version": 2,
        "payload": { "title": "x" }
    });
    assert!(serde_json::from_value::<Event>(unknown_version).is_err());
    let partial_fence = serde_json::json!({
        "event_type": "operation.settled",
        "schema_version": 1,
        "payload": {
            "command_id": command_id(0x3b).to_string(),
            "operation_id": operation_id(0x61).to_string(),
            "settled_at_ms": 1,
            "result_event_ids": [],
            "action_epoch": 1,
            "resource_id": null,
            "runtime_generation": 2
        }
    });
    assert!(serde_json::from_value::<Event>(partial_fence).is_err());

    assert_golden_event(
        "operation_accepted.json",
        &Event::OperationAccepted(
            OperationAcceptedFact::new(
                command_id(0x3b),
                operation_id(0x61),
                1_725_000_000_390,
                Some(1),
                Some(resource_id(0x57)),
                Some(2),
            )
            .expect("accepted"),
        ),
    );
}

#[test]
fn forged_task_created_and_renamed_are_rejected() {
    let task = task_id(0xf1);
    let mut forged = TaskFacts {
        id: task,
        environment_id: env_id(0x10),
        title: "ok".into(),
        description: None,
        project_id: project_id(0x11),
        workspace: WorkspaceRef::Main,
        assignment: TaskAssignment::LocalOwner,
        lifecycle: TaskLifecycle::Closing,
        action_epoch: 0,
        revision: 1,
        created_at_ms: 1,
    };
    assert!(
        matches!(
            apply(
                None,
                &domain_event(
                    event_id(0xf2),
                    Some(task),
                    1,
                    Some(1),
                    1,
                    Event::TaskCreated {
                        task: forged.clone(),
                        connectivity: TaskConnectivity::Connected,
                        attention: TaskAttention::None,
                        activity: TaskActivity::Idle,
                        review_readiness: ReviewReadiness::NotReady,
                    },
                ),
            ),
            Err(ApplyError::InvalidTransition)
        ),
        "TaskCreated must reject non-Open lifecycle"
    );
    forged.lifecycle = TaskLifecycle::Open;
    forged.action_epoch = 3;
    assert!(matches!(
        apply(
            None,
            &domain_event(
                event_id(0xf3),
                Some(task),
                1,
                Some(1),
                1,
                Event::TaskCreated {
                    task: forged.clone(),
                    connectivity: TaskConnectivity::Connected,
                    attention: TaskAttention::None,
                    activity: TaskActivity::Idle,
                    review_readiness: ReviewReadiness::NotReady,
                },
            ),
        ),
        Err(ApplyError::InvalidTransition)
    ));
    forged.action_epoch = 0;
    forged.title = "ok".into();
    // Bypass WorkspaceRef constructors to forge an empty-path worktree.
    forged.workspace = WorkspaceRef::Worktree {
        path: std::path::PathBuf::from(""),
        branch: "main".into(),
    };
    assert!(
        matches!(
            apply(
                None,
                &domain_event(
                    event_id(0xf4),
                    Some(task),
                    1,
                    Some(1),
                    1,
                    Event::TaskCreated {
                        task: forged.clone(),
                        connectivity: TaskConnectivity::Connected,
                        attention: TaskAttention::None,
                        activity: TaskActivity::Idle,
                        review_readiness: ReviewReadiness::NotReady,
                    },
                ),
            ),
            Err(ApplyError::InvalidTransition)
        ),
        "TaskCreated must reject empty workspace path"
    );
    forged.workspace = WorkspaceRef::External {
        path: std::path::PathBuf::from("C:\\code\\proj\0bad"),
    };
    assert!(
        matches!(
            apply(
                None,
                &domain_event(
                    event_id(0xa0),
                    Some(task),
                    1,
                    Some(1),
                    1,
                    Event::TaskCreated {
                        task: forged.clone(),
                        connectivity: TaskConnectivity::Connected,
                        attention: TaskAttention::None,
                        activity: TaskActivity::Idle,
                        review_readiness: ReviewReadiness::NotReady,
                    },
                ),
            ),
            Err(ApplyError::InvalidTransition)
        ),
        "TaskCreated must reject NUL workspace path"
    );
    forged.workspace = WorkspaceRef::Worktree {
        path: std::path::PathBuf::from(r"C:\code\proj"),
        branch: "   ".into(),
    };
    assert!(matches!(
        apply(
            None,
            &domain_event(
                event_id(0xa1),
                Some(task),
                1,
                Some(1),
                1,
                Event::TaskCreated {
                    task: forged.clone(),
                    connectivity: TaskConnectivity::Connected,
                    attention: TaskAttention::None,
                    activity: TaskActivity::Idle,
                    review_readiness: ReviewReadiness::NotReady,
                },
            ),
        ),
        Err(ApplyError::InvalidTransition)
    ));
    forged.workspace = WorkspaceRef::Main;
    forged.assignment = TaskAssignment::ExternalPrincipal {
        authority: "   ".into(),
        subject: "user".into(),
    };
    assert!(matches!(
        apply(
            None,
            &domain_event(
                event_id(0xa2),
                Some(task),
                1,
                Some(1),
                1,
                Event::TaskCreated {
                    task: forged.clone(),
                    connectivity: TaskConnectivity::Connected,
                    attention: TaskAttention::None,
                    activity: TaskActivity::Idle,
                    review_readiness: ReviewReadiness::NotReady,
                },
            ),
        ),
        Err(ApplyError::InvalidTransition)
    ));
    forged.assignment = TaskAssignment::LocalOwner;
    forged.title = "   ".into();
    assert!(matches!(
        apply(
            None,
            &domain_event(
                event_id(0xa3),
                Some(task),
                1,
                Some(1),
                1,
                Event::TaskCreated {
                    task: forged,
                    connectivity: TaskConnectivity::Connected,
                    attention: TaskAttention::None,
                    activity: TaskActivity::Idle,
                    review_readiness: ReviewReadiness::NotReady,
                },
            ),
        ),
        Err(ApplyError::InvalidTransition)
    ));

    let snap = create_task(None, task, 1, 0xf5);
    assert!(matches!(
        apply(
            Some(snap.clone()),
            &domain_event(
                event_id(0xf6),
                Some(task),
                2,
                Some(2),
                2,
                Event::TaskRenamed {
                    title: "   ".into(),
                },
            ),
        ),
        Err(ApplyError::InvalidTransition)
    ));

    let blank_created = serde_json::json!({
        "event_type": "task.created",
        "schema_version": 1,
        "payload": {
            "task": {
                "id": task.to_string(),
                "environment_id": env_id(0x10).to_string(),
                "title": "   ",
                "description": null,
                "project_id": project_id(0x11).to_string(),
                "workspace": "main",
                "assignment": "local_owner",
                "lifecycle": "open",
                "action_epoch": 0,
                "revision": 1,
                "created_at_ms": 1
            },
            "connectivity": "connected",
            "attention": "none",
            "activity": "idle",
            "review_readiness": "not_ready"
        }
    });
    assert!(serde_json::from_value::<Event>(blank_created).is_err());
    let blank_rename = serde_json::json!({
        "event_type": "task.renamed",
        "schema_version": 1,
        "payload": { "title": "  " }
    });
    assert!(serde_json::from_value::<Event>(blank_rename).is_err());

    #[derive(serde::Serialize)]
    struct EventWire<P> {
        schema_version: u32,
        event_type: &'static str,
        payload: P,
    }
    #[derive(serde::Serialize)]
    struct BlankTitle {
        title: &'static str,
    }
    let mp_rename = rmp_serde::to_vec(&EventWire {
        schema_version: 1,
        event_type: "task.renamed",
        payload: BlankTitle { title: "   " },
    })
    .expect("msgpack blank rename");
    assert!(rmp_serde::from_slice::<Event>(&mp_rename).is_err());

    #[derive(serde::Serialize)]
    struct CreatedPayload {
        task: MalformedTask,
        connectivity: &'static str,
        attention: &'static str,
        activity: &'static str,
        review_readiness: &'static str,
    }
    #[derive(serde::Serialize)]
    struct MalformedTask {
        id: String,
        environment_id: String,
        title: &'static str,
        description: Option<&'static str>,
        project_id: String,
        workspace: MalformedWorkspace,
        assignment: &'static str,
        lifecycle: &'static str,
        action_epoch: u64,
        revision: u64,
        created_at_ms: i64,
    }
    #[derive(serde::Serialize)]
    #[serde(rename_all = "snake_case")]
    enum MalformedWorkspace {
        Worktree {
            path: &'static str,
            branch: &'static str,
        },
    }
    let mp_created = rmp_serde::to_vec(&EventWire {
        schema_version: 1,
        event_type: "task.created",
        payload: CreatedPayload {
            task: MalformedTask {
                id: task.to_string(),
                environment_id: env_id(0x10).to_string(),
                title: "ok",
                description: None,
                project_id: project_id(0x11).to_string(),
                workspace: MalformedWorkspace::Worktree {
                    path: "",
                    branch: "main",
                },
                assignment: "local_owner",
                lifecycle: "open",
                action_epoch: 0,
                revision: 1,
                created_at_ms: 1,
            },
            connectivity: "connected",
            attention: "none",
            activity: "idle",
            review_readiness: "not_ready",
        },
    })
    .expect("msgpack malformed created");
    assert!(
        rmp_serde::from_slice::<Event>(&mp_created).is_err(),
        "MessagePack TaskCreated with empty path must fail closed"
    );
}

#[test]
fn forged_resource_and_agent_registration_are_rejected() {
    let task = task_id(0xf7);
    let snap = create_task(None, task, 1, 0xf8);

    let released = ResourceFacts {
        id: resource_id(0xf9),
        task_id: Some(task),
        owner_kind: OwnerKind::Task,
        resource_kind: ResourceKind::Terminal,
        recipe: ResourceRecipe::Terminal { cols: 80, rows: 24 },
        lifecycle: ResourceLifecycle::Released,
        runtime_generation: 0,
        updated_at_ms: 1,
    };
    assert!(matches!(
        decide(
            Some(&snap),
            &envelope(
                command_id(0xfa),
                Some(task),
                Some(1),
                Command::RegisterResource {
                    resource: released.clone(),
                },
            ),
        ),
        Err(RejectionCode::InvalidTransition)
    ));
    assert!(matches!(
        apply(
            Some(snap.clone()),
            &domain_event(
                event_id(0xfb),
                Some(task),
                2,
                Some(2),
                2,
                Event::ResourceRegistered {
                    resource: released.clone(),
                },
            ),
        ),
        Err(ApplyError::InvalidTransition)
    ));

    let mismatched = ResourceFacts {
        id: resource_id(0xfc),
        task_id: Some(task),
        owner_kind: OwnerKind::Task,
        resource_kind: ResourceKind::Terminal,
        recipe: ResourceRecipe::Service {
            command: "echo".into(),
        },
        lifecycle: ResourceLifecycle::Active,
        runtime_generation: 0,
        updated_at_ms: 1,
    };
    assert!(
        ResourceFacts::new(
            Some(task),
            OwnerKind::Task,
            ResourceKind::Terminal,
            ResourceRecipe::Service {
                command: "echo".into(),
            },
            1,
        )
        .is_err(),
        "kind/recipe mismatch must fail construction"
    );
    let mismatched_json = serde_json::to_value(&mismatched).expect("serialize mismatched");
    assert!(
        serde_json::from_value::<ResourceFacts>(mismatched_json).is_err(),
        "kind/recipe mismatch must fail JSON deserialize"
    );
    #[derive(serde::Serialize)]
    struct ResourceWire {
        id: String,
        task_id: String,
        owner_kind: &'static str,
        resource_kind: &'static str,
        recipe: KindMismatchRecipe,
        lifecycle: &'static str,
        runtime_generation: u64,
        updated_at_ms: i64,
    }
    #[derive(serde::Serialize)]
    #[serde(rename_all = "snake_case")]
    enum KindMismatchRecipe {
        Service { command: &'static str },
    }
    let mp_resource = rmp_serde::to_vec(&ResourceWire {
        id: resource_id(0xfc).to_string(),
        task_id: task.to_string(),
        owner_kind: "task",
        resource_kind: "terminal",
        recipe: KindMismatchRecipe::Service { command: "echo" },
        lifecycle: "active",
        runtime_generation: 0,
        updated_at_ms: 1,
    })
    .expect("msgpack mismatched resource");
    assert!(
        rmp_serde::from_slice::<ResourceFacts>(&mp_resource).is_err(),
        "MessagePack ResourceFacts kind/recipe mismatch must fail closed"
    );
    #[derive(serde::Serialize)]
    struct EventWire<P> {
        schema_version: u32,
        event_type: &'static str,
        payload: P,
    }
    #[derive(serde::Serialize)]
    struct RegisteredPayload {
        resource: ResourceWire,
    }
    let mp_registered = rmp_serde::to_vec(&EventWire {
        schema_version: 1,
        event_type: "resource.registered",
        payload: RegisteredPayload {
            resource: ResourceWire {
                id: resource_id(0xfc).to_string(),
                task_id: task.to_string(),
                owner_kind: "task",
                resource_kind: "terminal",
                recipe: KindMismatchRecipe::Service { command: "echo" },
                lifecycle: "active",
                runtime_generation: 0,
                updated_at_ms: 1,
            },
        },
    })
    .expect("msgpack registered");
    assert!(
        rmp_serde::from_slice::<Event>(&mp_registered).is_err(),
        "MessagePack ResourceRegistered with kind/recipe mismatch must fail closed"
    );

    assert!(ResourceFacts::new(
        None,
        OwnerKind::Task,
        ResourceKind::Terminal,
        ResourceRecipe::Terminal { cols: 1, rows: 1 },
        1,
    )
    .is_err());
    assert!(ResourceFacts::new(
        Some(task),
        OwnerKind::Host,
        ResourceKind::Terminal,
        ResourceRecipe::Terminal { cols: 1, rows: 1 },
        1,
    )
    .is_err());

    // Valid persisted Releasing facts must still deserialize.
    let releasing = ResourceFacts {
        id: resource_id(0xfd),
        task_id: Some(task),
        owner_kind: OwnerKind::Task,
        resource_kind: ResourceKind::Service,
        recipe: ResourceRecipe::Service {
            command: "echo".into(),
        },
        lifecycle: ResourceLifecycle::Releasing,
        runtime_generation: 2,
        updated_at_ms: 3,
    };
    let json = serde_json::to_value(&releasing).expect("serialize releasing");
    let restored: ResourceFacts = serde_json::from_value(json).expect("deserialize releasing");
    assert_eq!(restored.lifecycle, ResourceLifecycle::Releasing);
    let packed = rmp_serde::to_vec(&releasing).expect("msgpack releasing");
    let mp: ResourceFacts = rmp_serde::from_slice(&packed).expect("msgpack restoring");
    assert_eq!(mp, releasing);

    let released_persisted = ResourceFacts {
        id: resource_id(0xfd),
        task_id: Some(task),
        owner_kind: OwnerKind::Task,
        resource_kind: ResourceKind::Service,
        recipe: ResourceRecipe::Service {
            command: "echo".into(),
        },
        lifecycle: ResourceLifecycle::Released,
        runtime_generation: 2,
        updated_at_ms: 4,
    };
    let released_json = serde_json::to_value(&released_persisted).expect("serialize released");
    let released_restored: ResourceFacts =
        serde_json::from_value(released_json).expect("deserialize released");
    assert_eq!(released_restored.lifecycle, ResourceLifecycle::Released);
    let released_packed = rmp_serde::to_vec(&released_persisted).expect("msgpack released");
    let released_mp: ResourceFacts =
        rmp_serde::from_slice(&released_packed).expect("msgpack restore released");
    assert_eq!(released_mp, released_persisted);

    let bad_owner = serde_json::json!({
        "id": resource_id(0xfe).to_string(),
        "task_id": null,
        "owner_kind": "task",
        "resource_kind": "terminal",
        "recipe": { "terminal": { "cols": 1, "rows": 1 } },
        "lifecycle": "active",
        "runtime_generation": 0,
        "updated_at_ms": 1
    });
    assert!(serde_json::from_value::<ResourceFacts>(bad_owner).is_err());

    let specialist = AgentSessionFacts {
        id: agent_id(0x01),
        task_id: task,
        role: AgentRole::specialist("reviewer").expect("role"),
        provider_kind: "claude".into(),
        provider_session_id: None,
        lifecycle: AgentSessionLifecycle::Open,
        runtime_generation: 0,
        revision: 0,
    };
    let with_agent = apply_decided(
        Some(snap),
        &envelope(
            command_id(0x02),
            Some(task),
            Some(1),
            Command::RegisterAgentSession {
                agent: specialist.clone(),
            },
        ),
        2,
        0x03,
        3,
    )
    .expect("register specialist");
    assert!(matches!(
        decide(
            Some(&with_agent),
            &envelope(
                command_id(0x04),
                Some(task),
                Some(2),
                Command::SetPrimaryAgent {
                    agent_session_id: specialist.id,
                },
            ),
        ),
        Err(RejectionCode::InvalidTransition)
    ));

    let closed_agent = AgentSessionFacts {
        id: agent_id(0x05),
        task_id: task,
        role: AgentRole::Primary,
        provider_kind: "claude".into(),
        provider_session_id: None,
        lifecycle: AgentSessionLifecycle::Closed,
        runtime_generation: 0,
        revision: 0,
    };
    assert!(matches!(
        decide(
            Some(&with_agent),
            &envelope(
                command_id(0x06),
                Some(task),
                Some(2),
                Command::RegisterAgentSession {
                    agent: closed_agent.clone(),
                },
            ),
        ),
        Err(RejectionCode::InvalidTransition)
    ));
    assert!(matches!(
        apply(
            Some(with_agent),
            &domain_event(
                event_id(0x07),
                Some(task),
                3,
                Some(3),
                4,
                Event::AgentSessionRegistered {
                    agent: closed_agent,
                },
            ),
        ),
        Err(ApplyError::InvalidTransition)
    ));
}

#[test]
fn operation_accepted_is_rebuildable_and_not_settlement() {
    let task = task_id(0x08);
    let snap = create_task(None, task, 1, 0x09);
    let fact = OperationAcceptedFact::new(
        command_id(0x0a),
        operation_id(0x0b),
        10,
        Some(0),
        Some(resource_id(0x57)),
        Some(2),
    )
    .expect("accepted fact");
    let event = domain_event(
        event_id(0x0c),
        Some(task),
        2,
        Some(1),
        11,
        Event::OperationAccepted(fact.clone()),
    );
    assert!(!Event::OperationAccepted(fact.clone()).is_task_mutation());
    let after = apply(Some(snap.clone()), &event).expect("apply accepted");
    assert_eq!(after.task.revision, snap.task.revision);
    assert!(!matches!(
        Event::OperationAccepted(fact.clone()),
        Event::OperationSettled(_)
    ));

    // One-truth: DomainEvent.task_id is sole task scope; accepted payload has no task_id.
    let json = serde_json::to_value(&event).expect("domain json");
    assert_eq!(json["task_id"], serde_json::json!(task.to_string()));
    assert!(
        json["payload"]["payload"].get("task_id").is_none(),
        "accepted Event payload must not carry task_id"
    );
    let json_rt: DomainEvent = serde_json::from_value(json.clone()).expect("domain json rt");
    assert_eq!(json_rt.task_id, Some(task));
    assert_eq!(json_rt, event);

    let packed = rmp_serde::to_vec(&event).expect("domain msgpack");
    let mp_rt: DomainEvent = rmp_serde::from_slice(&packed).expect("domain msgpack rt");
    assert_eq!(mp_rt.task_id, Some(task));
    assert_eq!(mp_rt, event);

    let golden = fs::read_to_string(fixture_path("operation_accepted.json")).expect("fixture");
    let golden_val: serde_json::Value = serde_json::from_str(golden.trim()).expect("parse golden");
    assert!(
        golden_val["payload"].get("task_id").is_none(),
        "golden accepted payload must omit task_id"
    );
    assert_eq!(
        golden_val["payload"]["resource_id"],
        serde_json::json!(resource_id(0x57).to_string())
    );
    assert_eq!(golden_val["payload"]["runtime_generation"], 2);
    let golden_event: Event =
        serde_json::from_value(golden_val.clone()).expect("golden Event decode");
    let golden_domain = DomainEvent {
        id: event_id(0x3c),
        task_id: Some(task),
        sequence: 9,
        task_revision: Some(1),
        occurred_at_ms: 1_725_000_000_390,
        payload: golden_event.clone(),
    };
    let golden_domain_json = serde_json::to_value(&golden_domain).expect("golden domain json");
    assert_eq!(
        golden_domain_json["task_id"],
        serde_json::json!(task.to_string())
    );
    assert_eq!(golden_domain_json["payload"], golden_val);
    let golden_domain_rt: DomainEvent =
        serde_json::from_value(golden_domain_json).expect("golden domain json rt");
    assert_eq!(golden_domain_rt.task_id, Some(task));
    assert_eq!(golden_domain_rt.payload, golden_event);
    let golden_packed = rmp_serde::to_vec(&golden_domain).expect("golden domain msgpack");
    let golden_mp: DomainEvent =
        rmp_serde::from_slice(&golden_packed).expect("golden domain msgpack rt");
    assert_eq!(golden_mp.task_id, Some(task));
    assert_eq!(golden_mp.payload, golden_event);

    let stale_payload_task = serde_json::json!({
        "schema_version": 1,
        "event_type": "operation.accepted",
        "payload": {
            "command_id": command_id(0x0a).to_string(),
            "operation_id": operation_id(0x0b).to_string(),
            "task_id": task.to_string(),
            "accepted_at_ms": 10,
            "action_epoch": 0,
            "resource_id": null,
            "runtime_generation": null
        }
    });
    assert!(
        serde_json::from_value::<Event>(stale_payload_task).is_err(),
        "accepted payload must reject unknown/duplicate task_id field"
    );

    assert!(matches!(
        apply(
            Some(snap),
            &domain_event(
                event_id(0x0e),
                Some(task_id(0x0d)),
                2,
                Some(1),
                11,
                Event::OperationAccepted(fact),
            ),
        ),
        Err(ApplyError::TaskMismatch)
    ));
}

#[test]
fn operation_accepted_resource_fence_is_paired() {
    assert!(
        OperationAcceptedFact::new(
            command_id(0x3b),
            operation_id(0x61),
            1,
            Some(0),
            Some(resource_id(0x57)),
            None,
        )
        .is_err(),
        "resource_id without runtime_generation must fail"
    );
    assert!(
        OperationAcceptedFact::new(
            command_id(0x3b),
            operation_id(0x61),
            1,
            Some(0),
            None,
            Some(2),
        )
        .is_err(),
        "runtime_generation without resource_id must fail"
    );

    #[derive(serde::Serialize)]
    struct EventWire<P> {
        schema_version: u32,
        event_type: &'static str,
        payload: P,
    }
    #[derive(serde::Serialize)]
    struct AcceptedPartial {
        command_id: String,
        operation_id: String,
        accepted_at_ms: i64,
        action_epoch: Option<u64>,
        resource_id: Option<String>,
        runtime_generation: Option<u64>,
    }

    let partial_resource_only = serde_json::json!({
        "schema_version": 1,
        "event_type": "operation.accepted",
        "payload": {
            "command_id": command_id(0x3b).to_string(),
            "operation_id": operation_id(0x61).to_string(),
            "accepted_at_ms": 1,
            "action_epoch": 0,
            "resource_id": resource_id(0x57).to_string(),
            "runtime_generation": null
        }
    });
    assert!(serde_json::from_value::<Event>(partial_resource_only).is_err());
    let partial_generation_only = serde_json::json!({
        "schema_version": 1,
        "event_type": "operation.accepted",
        "payload": {
            "command_id": command_id(0x3b).to_string(),
            "operation_id": operation_id(0x61).to_string(),
            "accepted_at_ms": 1,
            "action_epoch": 0,
            "resource_id": null,
            "runtime_generation": 2
        }
    });
    assert!(serde_json::from_value::<Event>(partial_generation_only).is_err());

    let mp_resource_only = rmp_serde::to_vec(&EventWire {
        schema_version: 1,
        event_type: "operation.accepted",
        payload: AcceptedPartial {
            command_id: command_id(0x3b).to_string(),
            operation_id: operation_id(0x61).to_string(),
            accepted_at_ms: 1,
            action_epoch: Some(0),
            resource_id: Some(resource_id(0x57).to_string()),
            runtime_generation: None,
        },
    })
    .expect("msgpack resource-only");
    assert!(rmp_serde::from_slice::<Event>(&mp_resource_only).is_err());
    let mp_generation_only = rmp_serde::to_vec(&EventWire {
        schema_version: 1,
        event_type: "operation.accepted",
        payload: AcceptedPartial {
            command_id: command_id(0x3b).to_string(),
            operation_id: operation_id(0x61).to_string(),
            accepted_at_ms: 1,
            action_epoch: Some(0),
            resource_id: None,
            runtime_generation: Some(2),
        },
    })
    .expect("msgpack generation-only");
    assert!(rmp_serde::from_slice::<Event>(&mp_generation_only).is_err());

    let pure = OperationAcceptedFact::new(
        command_id(0x3b),
        operation_id(0x61),
        42,
        Some(0),
        None,
        None,
    )
    .expect("pure accepted fact");
    let pure_event = Event::OperationAccepted(pure.clone());
    assert!(!pure_event.is_task_mutation());
    assert!(!matches!(pure_event, Event::OperationSettled(_)));
    let pure_json = serde_json::to_value(&pure_event).expect("pure json");
    assert!(pure_json["payload"].get("task_id").is_none());
    assert_eq!(pure_json["payload"]["resource_id"], serde_json::Value::Null);
    assert_eq!(
        pure_json["payload"]["runtime_generation"],
        serde_json::Value::Null
    );
    let pure_json_rt: Event = serde_json::from_value(pure_json).expect("pure json rt");
    assert_eq!(pure_json_rt, pure_event);
    let pure_packed = rmp_serde::to_vec(&pure_event).expect("pure msgpack");
    let pure_mp: Event = rmp_serde::from_slice(&pure_packed).expect("pure msgpack rt");
    assert_eq!(pure_mp, pure_event);
}

#[test]
fn command_contract_outcome_rejects_invalid_source_kind_and_identity() {
    let op = operation_id(0x71);
    let fence = ResourceFence::new(resource_id(0x57), 2);

    let settled = OperationOutcome::new(
        op,
        1_000,
        Some(1),
        Some(fence),
        OutcomeSource::Dispatch,
        OperationOutcomeKind::Settled {
            result_event_ids: vec![event_id(0x80)],
        },
    )
    .expect("dispatch settled");
    assert!(settled.resource_fence.is_some());
    let encoded = serde_json::to_value(&settled).expect("outcome json");
    assert!(
        encoded.get("command_id").is_none(),
        "OperationOutcome must not carry command_id"
    );
    let round: OperationOutcome = serde_json::from_value(encoded).expect("outcome json rt");
    assert_eq!(round, settled);

    let packed = rmp_serde::to_vec(&settled).expect("outcome msgpack");
    let mp: OperationOutcome = rmp_serde::from_slice(&packed).expect("outcome msgpack rt");
    assert_eq!(mp, settled);

    assert!(
        OperationOutcome::new(
            op,
            1_000,
            None,
            None,
            OutcomeSource::verified_reconciliation(0, "ext-1").expect("identity"),
            OperationOutcomeKind::Cancelled {
                reason: CancellationReason::Superseded,
            },
        )
        .is_err(),
        "verified reconciliation cannot cancel"
    );
    assert!(
        OperationOutcome::new(
            op,
            1_000,
            None,
            None,
            OutcomeSource::verified_reconciliation(0, "ext-1").expect("identity"),
            OperationOutcomeKind::Uncertain {
                code: OperationUncertaintyCode::AmbiguousDispatch,
            },
        )
        .is_err(),
        "verified reconciliation cannot mark uncertain"
    );
    assert!(
        OutcomeSource::verified_reconciliation(0, "   ").is_err(),
        "blank external identity must fail"
    );
    assert!(
        OutcomeSource::verified_reconciliation(0, "x".repeat(MAX_EXTERNAL_IDENTITY_BYTES + 1))
            .is_err(),
        "oversized external identity must fail"
    );

    let bad_identity = serde_json::json!({
        "operation_id": op.to_string(),
        "occurred_at_ms": 1,
        "action_epoch": null,
        "resource_fence": null,
        "source": {
            "verified_reconciliation": {
                "effect_index": 0,
                "external_identity": "  padded  "
            }
        },
        "kind": {
            "settled": { "result_event_ids": [event_id(0x80).to_string()] }
        }
    });
    assert!(
        serde_json::from_value::<OperationOutcome>(bad_identity).is_err(),
        "serde must not accept non-canonical external identity"
    );

    let partial_fence = serde_json::json!({
        "operation_id": op.to_string(),
        "occurred_at_ms": 1,
        "action_epoch": null,
        "resource_fence": {
            "resource_id": resource_id(0x57).to_string()
        },
        "source": "dispatch",
        "kind": { "failed": { "code": "side_effect_failed" } }
    });
    assert!(
        serde_json::from_value::<OperationOutcome>(partial_fence).is_err(),
        "partial resource fence must fail"
    );

    let unknown_kind_field = serde_json::json!({
        "operation_id": op.to_string(),
        "occurred_at_ms": 1,
        "action_epoch": null,
        "resource_fence": null,
        "source": "dispatch",
        "kind": {
            "settled": {
                "result_event_ids": [event_id(0x80).to_string()],
                "extra": true
            }
        }
    });
    assert!(
        serde_json::from_value::<OperationOutcome>(unknown_kind_field.clone()).is_err(),
        "unknown fields inside outcome kind must fail JSON decode"
    );
    let unknown_kind_mp = rmp_serde::to_vec_named(&unknown_kind_field).expect("pack unknown kind");
    assert!(
        rmp_serde::from_slice::<OperationOutcome>(&unknown_kind_mp).is_err(),
        "unknown fields inside outcome kind must fail MessagePack decode"
    );

    let unknown_source_field = serde_json::json!({
        "operation_id": op.to_string(),
        "occurred_at_ms": 1,
        "action_epoch": null,
        "resource_fence": null,
        "source": {
            "verified_reconciliation": {
                "effect_index": 0,
                "external_identity": "ext-1",
                "extra": true
            }
        },
        "kind": { "failed": { "code": "side_effect_failed" } }
    });
    assert!(
        serde_json::from_value::<OperationOutcome>(unknown_source_field).is_err(),
        "unknown fields inside outcome source must fail"
    );
}

#[test]
fn command_contract_settled_failed_facts_persist_source() {
    let dispatch_settled = OperationSettledFact::new(
        command_id(0x3b),
        operation_id(0x61),
        1_725_000_000_400,
        vec![event_id(0x80)],
        Some(1),
        Some(resource_id(0x57)),
        Some(2),
    )
    .expect("dispatch convenience");
    assert_eq!(dispatch_settled.source, OutcomeSource::Dispatch);

    let reconciled = OperationSettledFact::with_source(
        command_id(0x3b),
        operation_id(0x61),
        1_725_000_000_401,
        vec![event_id(0x81)],
        Some(1),
        Some(resource_id(0x57)),
        Some(2),
        OutcomeSource::verified_reconciliation(1, "provider:job-9").expect("identity"),
    )
    .expect("reconciled settled");
    assert!(matches!(
        reconciled.source,
        OutcomeSource::VerifiedReconciliation { .. }
    ));

    let failed = OperationFailedFact::new(
        command_id(0x3b),
        operation_id(0x61),
        1_725_000_000_410,
        OperationErrorCode::SideEffectFailed,
        Some(1),
        None,
        None,
    )
    .expect("dispatch failed convenience");
    assert_eq!(failed.source, OutcomeSource::Dispatch);

    let failed_reconciled = OperationFailedFact::with_source(
        command_id(0x3b),
        operation_id(0x61),
        1_725_000_000_411,
        OperationErrorCode::SideEffectFailed,
        Some(1),
        None,
        None,
        OutcomeSource::verified_reconciliation(0, "ext-fail").expect("identity"),
    )
    .expect("reconciled failed");

    for event in [
        Event::OperationSettled(dispatch_settled.clone()),
        Event::OperationSettled(reconciled.clone()),
        Event::OperationFailed(failed.clone()),
        Event::OperationFailed(failed_reconciled.clone()),
    ] {
        let json = serde_json::to_value(&event).expect("json");
        assert!(json["payload"].get("source").is_some());
        let rt: Event = serde_json::from_value(json).expect("json rt");
        assert_eq!(rt, event);
        let packed = rmp_serde::to_vec(&event).expect("msgpack");
        let mp: Event = rmp_serde::from_slice(&packed).expect("msgpack rt");
        assert_eq!(mp, event);
    }

    let missing_source = serde_json::json!({
        "schema_version": 1,
        "event_type": "operation.settled",
        "payload": {
            "command_id": command_id(0x3b).to_string(),
            "operation_id": operation_id(0x61).to_string(),
            "settled_at_ms": 1,
            "result_event_ids": [],
            "action_epoch": null,
            "resource_id": null,
            "runtime_generation": null
        }
    });
    assert!(
        serde_json::from_value::<Event>(missing_source).is_err(),
        "settled facts must require source on the wire"
    );

    assert_golden_event(
        "operation_settled.json",
        &Event::OperationSettled(dispatch_settled),
    );
    assert_golden_event("operation_failed.json", &Event::OperationFailed(failed));
}

#[test]
fn command_contract_forged_public_values_fail_closed_on_serialize() {
    fn assert_json_and_msgpack_reject<T: serde::Serialize>(value: &T, label: &str) {
        assert!(
            serde_json::to_value(value).is_err(),
            "{label}: JSON serialize must reject forged value"
        );
        assert!(
            rmp_serde::to_vec(value).is_err(),
            "{label}: MessagePack serialize must reject forged value"
        );
    }

    let blank_identity = OutcomeSource::VerifiedReconciliation {
        effect_index: 0,
        external_identity: "   ".into(),
    };
    assert_json_and_msgpack_reject(&blank_identity, "blank reconciliation identity");

    let oversize_identity = OutcomeSource::VerifiedReconciliation {
        effect_index: 0,
        external_identity: "x".repeat(MAX_EXTERNAL_IDENTITY_BYTES + 1),
    };
    assert_json_and_msgpack_reject(&oversize_identity, "oversize reconciliation identity");

    let invalid_pairing = OperationOutcome {
        operation_id: operation_id(0x71),
        occurred_at_ms: 1,
        action_epoch: None,
        resource_fence: None,
        source: OutcomeSource::VerifiedReconciliation {
            effect_index: 0,
            external_identity: "ext-1".into(),
        },
        kind: OperationOutcomeKind::Cancelled {
            reason: CancellationReason::Superseded,
        },
    };
    assert_json_and_msgpack_reject(&invalid_pairing, "invalid source/kind pairing");

    let partial_fence_settled = OperationSettledFact {
        command_id: command_id(0x3b),
        operation_id: operation_id(0x61),
        settled_at_ms: 1,
        result_event_ids: Vec::new(),
        action_epoch: None,
        resource_id: Some(resource_id(0x57)),
        runtime_generation: None,
        source: OutcomeSource::Dispatch,
    };
    assert_json_and_msgpack_reject(&partial_fence_settled, "partial resource fence on settled");
    assert_json_and_msgpack_reject(
        &Event::OperationSettled(partial_fence_settled),
        "partial fence via settled event",
    );

    let invalid_source_failed = OperationFailedFact {
        command_id: command_id(0x3b),
        operation_id: operation_id(0x61),
        settled_at_ms: 1,
        code: OperationErrorCode::SideEffectFailed,
        action_epoch: None,
        resource_id: None,
        runtime_generation: None,
        source: OutcomeSource::VerifiedReconciliation {
            effect_index: 0,
            external_identity: String::new(),
        },
    };
    assert_json_and_msgpack_reject(
        &invalid_source_failed,
        "invalid terminal fact source on failed",
    );
    assert_json_and_msgpack_reject(
        &Event::OperationFailed(invalid_source_failed),
        "invalid terminal fact source via failed event",
    );

    let forged_accepted = OperationAcceptedFact {
        command_id: command_id(0x3b),
        operation_id: operation_id(0x61),
        accepted_at_ms: 1,
        action_epoch: None,
        resource_id: Some(resource_id(0x57)),
        runtime_generation: None,
    };
    assert_json_and_msgpack_reject(&forged_accepted, "partial fence on accepted");
    assert_json_and_msgpack_reject(
        &Event::OperationAccepted(forged_accepted),
        "partial fence via accepted event",
    );

    let forged_cancelled = OperationCancelledFact {
        command_id: command_id(0x3b),
        operation_id: operation_id(0x61),
        settled_at_ms: 1,
        reason: CancellationReason::Superseded,
        action_epoch: None,
        resource_id: None,
        runtime_generation: Some(2),
    };
    assert_json_and_msgpack_reject(&forged_cancelled, "partial fence on cancelled");
    assert_json_and_msgpack_reject(
        &Event::OperationCancelled(forged_cancelled),
        "partial fence via cancelled event",
    );

    let forged_uncertain = OperationUncertainFact {
        command_id: command_id(0x3b),
        operation_id: operation_id(0x61),
        observed_at_ms: 1,
        code: OperationUncertaintyCode::AmbiguousDispatch,
        action_epoch: Some(1),
        resource_id: Some(resource_id(0x57)),
        runtime_generation: None,
    };
    assert_json_and_msgpack_reject(&forged_uncertain, "partial fence on uncertain");
    assert_json_and_msgpack_reject(
        &Event::OperationUncertain(forged_uncertain),
        "partial fence via uncertain event",
    );
}
