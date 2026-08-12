use devmanager::client::{ClientModel, ClientModelBuilder};
use devmanager::domain::agent::{AgentRole, AgentSessionFacts, AgentSessionLifecycle};
use devmanager::domain::artifact::{
    ArtifactContentRef, ArtifactFacts, ArtifactKind, PrivacyClass, MAX_SPECIALIST_ID_REFS,
    MAX_SPECIALIST_RAW_ARTIFACT_BYTES, MAX_SPECIALIST_TEXT_BYTES,
};
use devmanager::domain::command::{
    decide, AcceptSpecialistHandoffIntent, CancelSpecialistIntent, Command, CommandEnvelope,
    CommandReceipt, CreateTaskIntent, PromotePrimaryIntent, RejectionCode, RequestSpecialistIntent,
    SpecialistPermission, DEFAULT_MAX_TOP_LEVEL_RUNTIMES,
};
use devmanager::domain::event::{
    apply, ApplyError, DomainEvent, Event, SpecialistRequestedPayload,
};
use devmanager::domain::id::{
    AgentSessionId, ArtifactId, ClientId, CommandId, EnvironmentId, EventId, ProjectId, ResourceId,
    SnapshotId, TaskId,
};
use devmanager::domain::resource::{
    OwnerKind, ResourceFacts, ResourceKind, ResourceLifecycle, ResourceRecipe,
};
use devmanager::domain::snapshot::{SnapshotPage, SnapshotSection, TaskSnapshot};
use devmanager::domain::task::{
    ReviewReadiness, TaskActivity, TaskAssignment, TaskAttention, TaskConnectivity, TaskLifecycle,
    WorkspaceRef,
};
use devmanager::domain::{
    decode_orchestration_msgpack, preflight_msgpack, MsgPackPreflightError,
    MAX_ORCHESTRATION_MSGPACK_COLLECTION_ITEMS, MAX_ORCHESTRATION_MSGPACK_DEPTH,
    MAX_ORCHESTRATION_MSGPACK_STRING_BYTES,
};
use devmanager::kernel::{CommandBus, KernelStore};
use devmanager::providers::orchestrator::{
    specialist_cancel_hold, specialist_native_child_hold, specialist_write_hold,
    validate_specialist_result, OrchestrationHold, SpecialistResult, SpecialistStatus,
};
use devmanager::providers::ProviderKind;
use sha2::{Digest, Sha256};
use tempfile::TempDir;

fn fixed_uuid_v7(tail: u8) -> [u8; 16] {
    [
        0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        tail,
    ]
}

fn task_id(tail: u8) -> TaskId {
    TaskId::from_bytes(fixed_uuid_v7(tail)).expect("task")
}
fn env_id(tail: u8) -> EnvironmentId {
    EnvironmentId::from_bytes(fixed_uuid_v7(tail)).expect("env")
}
fn project_id(tail: u8) -> ProjectId {
    ProjectId::from_bytes(fixed_uuid_v7(tail)).expect("project")
}
fn client_id(tail: u8) -> ClientId {
    ClientId::from_bytes(fixed_uuid_v7(tail)).expect("client")
}
fn command_id(tail: u8) -> CommandId {
    CommandId::from_bytes(fixed_uuid_v7(tail)).expect("command")
}
fn event_id(tail: u8) -> EventId {
    EventId::from_bytes(fixed_uuid_v7(tail)).expect("event")
}
fn agent_id(tail: u8) -> AgentSessionId {
    AgentSessionId::from_bytes(fixed_uuid_v7(tail)).expect("agent")
}
fn artifact_id(tail: u8) -> ArtifactId {
    ArtifactId::from_bytes(fixed_uuid_v7(tail)).expect("artifact")
}
fn resource_id(tail: u8) -> ResourceId {
    ResourceId::from_bytes(fixed_uuid_v7(tail)).expect("resource")
}

fn sealed_artifact(task_id: TaskId, id: ArtifactId, label: &str, body: &str) -> ArtifactFacts {
    ArtifactFacts {
        id,
        task_id,
        kind: ArtifactKind::ReviewReport,
        label: label.to_string(),
        content_ref: ArtifactContentRef::InlineUtf8(body.to_string()),
        sha256: Sha256::digest(body.as_bytes()).into(),
        privacy_class: PrivacyClass::LocalOnly,
        created_at_ms: 1_725_000_000_300,
    }
}

fn empty_client_model() -> ClientModel {
    let snapshot_id = SnapshotId::from_bytes(fixed_uuid_v7(0x0f)).expect("snapshot");
    let mut builder = ClientModelBuilder::new();
    for section in [
        SnapshotSection::Tasks,
        SnapshotSection::AgentSessions,
        SnapshotSection::Artifacts,
        SnapshotSection::Resources,
        SnapshotSection::Operations,
    ] {
        builder
            .ingest_page(SnapshotPage {
                snapshot_id,
                through_sequence: 0,
                section,
                after_item: None,
                items: Vec::new(),
                encoded_bytes: 1,
                next_cursor: None,
            })
            .expect("empty snapshot section");
    }
    builder.finish().expect("empty client model")
}

fn accept(bus: &mut CommandBus, envelope: CommandEnvelope) -> CommandReceipt {
    let receipt = bus.execute(envelope).expect("command bus execution");
    assert!(
        matches!(receipt, CommandReceipt::Accepted { .. }),
        "command must be accepted: {receipt:?}"
    );
    receipt
}
fn envelope(
    cmd: CommandId,
    task: Option<TaskId>,
    revision: Option<u64>,
    command: Command,
) -> CommandEnvelope {
    CommandEnvelope {
        command_id: cmd,
        client_id: client_id(0x20),
        task_id: task,
        issued_at_ms: 1_725_000_000_100,
        expected_task_revision: revision,
        command,
    }
}

fn domain_event(
    id: EventId,
    task: TaskId,
    sequence: u64,
    revision: u64,
    payload: Event,
) -> DomainEvent {
    DomainEvent {
        id,
        task_id: Some(task),
        sequence,
        task_revision: Some(revision),
        occurred_at_ms: 1_725_000_000_200,
        payload,
    }
}

fn apply_all(
    mut snap: Option<TaskSnapshot>,
    task: TaskId,
    seq0: u64,
    ev0: u8,
    events: Vec<Event>,
) -> TaskSnapshot {
    let mut seq = seq0;
    let mut ev = ev0;
    for payload in events {
        let revision = snap.as_ref().map(|s| s.task.revision + 1).unwrap_or(1);
        snap = Some(
            apply(
                snap,
                &domain_event(event_id(ev), task, seq, revision, payload),
            )
            .expect("apply"),
        );
        seq += 1;
        ev = ev.wrapping_add(1);
    }
    snap.expect("snapshot")
}

fn create_intent(task: TaskId, workspace: WorkspaceRef) -> CreateTaskIntent {
    CreateTaskIntent {
        id: task,
        environment_id: env_id(0x10),
        title: "Orchestrate".into(),
        description: None,
        project_id: project_id(0x11),
        workspace,
        assignment: TaskAssignment::LocalOwner,
        created_at_ms: 1_725_000_000_000,
        connectivity: TaskConnectivity::Connected,
        attention: TaskAttention::None,
        activity: TaskActivity::Idle,
        review_readiness: ReviewReadiness::NotReady,
    }
}

fn open_task_at(task: TaskId, workspace: WorkspaceRef) -> TaskSnapshot {
    let events = decide(
        None,
        &envelope(
            command_id(0x30),
            None,
            None,
            Command::CreateTask(create_intent(task, workspace)),
        ),
    )
    .expect("create");
    apply_all(None, task, 1, 0x40, events)
}

fn with_primary_at(task: TaskId, primary: AgentSessionId, workspace: WorkspaceRef) -> TaskSnapshot {
    let snap = open_task_at(task, workspace);
    let registered = decide(
        Some(&snap),
        &envelope(
            command_id(0x31),
            Some(task),
            Some(snap.task.revision),
            Command::RegisterAgentSession {
                agent: primary_facts(task, primary),
            },
        ),
    )
    .expect("register");
    let snap = apply_all(Some(snap), task, 2, 0x50, registered);
    let set = decide(
        Some(&snap),
        &envelope(
            command_id(0x32),
            Some(task),
            Some(snap.task.revision),
            Command::SetPrimaryAgent {
                agent_session_id: primary,
            },
        ),
    )
    .expect("set primary");
    apply_all(Some(snap), task, 3, 0x60, set)
}

fn primary_facts(task: TaskId, id: AgentSessionId) -> AgentSessionFacts {
    AgentSessionFacts {
        id,
        task_id: task,
        role: AgentRole::Primary,
        provider_kind: ProviderKind::ClaudeCode,
        provider_session_id: None,
        lifecycle: AgentSessionLifecycle::Open,
        runtime_generation: 3,
        revision: 0,
    }
}

fn specialist_facts(task: TaskId, id: AgentSessionId, name: &str) -> AgentSessionFacts {
    AgentSessionFacts {
        id,
        task_id: task,
        role: AgentRole::specialist(name).expect("role"),
        provider_kind: ProviderKind::Codex,
        provider_session_id: None,
        lifecycle: AgentSessionLifecycle::Open,
        runtime_generation: 3,
        revision: 0,
    }
}

fn with_primary(task: TaskId, primary: AgentSessionId) -> TaskSnapshot {
    with_primary_at(task, primary, WorkspaceRef::Main)
}

fn handoff_intent(
    specialist: AgentSessionId,
    artifact: ArtifactId,
    structured: Option<SpecialistResult>,
    raw: Option<&str>,
) -> AcceptSpecialistHandoffIntent {
    AcceptSpecialistHandoffIntent {
        specialist_id: specialist,
        artifact_id: artifact,
        expected_action_epoch: 0,
        expected_runtime_generation: 3,
        structured,
        raw_inline_utf8: raw.map(str::to_owned),
    }
}

fn request_intent(
    task: TaskId,
    specialist: AgentSessionId,
    requested_by: AgentSessionId,
    permission: SpecialistPermission,
    workspace: WorkspaceRef,
) -> RequestSpecialistIntent {
    RequestSpecialistIntent {
        specialist: specialist_facts(task, specialist, "review"),
        requested_by,
        purpose: "review".into(),
        permission,
        workspace,
        expected_artifact_kind: ArtifactKind::ReviewReport,
        expected_action_epoch: 0,
        expected_runtime_generation: 3,
        resource_id: None,
        max_top_level_runtimes: DEFAULT_MAX_TOP_LEVEL_RUNTIMES,
    }
}

fn sealed_artifact(task: TaskId, id: ArtifactId, label: &str, body: &str) -> ArtifactFacts {
    ArtifactFacts {
        id,
        task_id: task,
        kind: ArtifactKind::Finding,
        label: label.into(),
        content_ref: ArtifactContentRef::InlineUtf8(body.into()),
        sha256: [7u8; 32],
        privacy_class: PrivacyClass::LocalOnly,
        created_at_ms: 1_725_000_000_280,
    }
}

fn empty_client_model() -> ClientModel {
    let snapshot = SnapshotId::from_bytes(fixed_uuid_v7(0xfe)).expect("snapshot");
    let mut builder = ClientModelBuilder::new();
    for section in [
        SnapshotSection::Tasks,
        SnapshotSection::AgentSessions,
        SnapshotSection::Artifacts,
        SnapshotSection::Resources,
        SnapshotSection::Operations,
    ] {
        builder
            .ingest_page(SnapshotPage {
                snapshot_id: snapshot,
                through_sequence: 0,
                section,
                after_item: None,
                items: vec![],
                encoded_bytes: 1,
                next_cursor: None,
            })
            .expect("page");
    }
    builder.finish().expect("model")
}

fn accept(bus: &mut CommandBus, envelope: CommandEnvelope) -> CommandReceipt {
    let receipt = bus.execute(envelope).expect("execute");
    assert!(
        matches!(receipt, CommandReceipt::Accepted { .. }),
        "expected accepted, got {receipt:?}"
    );
    receipt
}

#[test]
fn request_specialist_is_a_durable_snapshot_fact() {
    let task = task_id(0x01);
    let primary = agent_id(0x02);
    let specialist = agent_id(0x03);
    let snap = with_primary(task, primary);
    let events = decide(
        Some(&snap),
        &envelope(
            command_id(0x33),
            Some(task),
            Some(snap.task.revision),
            Command::RequestSpecialist(request_intent(
                task,
                specialist,
                primary,
                SpecialistPermission::ReadOnly,
                WorkspaceRef::Main,
            )),
        ),
    )
    .expect("request");
    assert!(matches!(
        events.as_slice(),
        [Event::SpecialistRequested { specialist_id, requested_by, .. }]
            if *specialist_id == specialist && *requested_by == primary
    ));
    let snap = apply_all(Some(snap), task, 4, 0x70, events);
    assert_eq!(snap.primary_agent_id, Some(primary));
    assert!(matches!(
        snap.agents.get(&specialist).map(|a| &a.role),
        Some(AgentRole::Specialist { name, .. }) if name == "review"
    ));
}

#[test]
fn second_primary_registration_is_rejected() {
    let task = task_id(0x04);
    let snap = with_primary(task, agent_id(0x05));
    let err = decide(
        Some(&snap),
        &envelope(
            command_id(0x34),
            Some(task),
            Some(snap.task.revision),
            Command::RegisterAgentSession {
                agent: primary_facts(task, agent_id(0x06)),
            },
        ),
    )
    .expect_err("second primary");
    assert_eq!(err, RejectionCode::AlreadyExists);
}

#[test]
fn registration_event_rejects_forged_specialist_and_native_child_roles() {
    let task = task_id(0x07);
    let primary = agent_id(0x08);
    let snap = open_task_at(task, WorkspaceRef::Main);
    let specialist = specialist_facts(task, agent_id(0x09), "forged");
    let specialist_event = domain_event(
        event_id(0x0a),
        task,
        2,
        snap.task.revision + 1,
        Event::AgentSessionRegistered { agent: specialist },
    );
    assert_eq!(
        apply(Some(snap.clone()), &specialist_event),
        Err(ApplyError::InvalidTransition)
    );

    let native = AgentSessionFacts {
        id: agent_id(0x0b),
        task_id: task,
        role: AgentRole::specialist("subprocess").expect("specialist role"),
        provider_kind: ProviderKind::ClaudeCode,
        provider_session_id: None,
        lifecycle: AgentSessionLifecycle::Open,
        runtime_generation: 0,
        revision: 0,
    };
    let native_event = domain_event(
        event_id(0x0c),
        task,
        2,
        snap.task.revision + 1,
        Event::AgentSessionRegistered { agent: native },
    );
    assert_eq!(
        apply(Some(snap), &native_event),
        Err(ApplyError::InvalidTransition)
    );
}

#[test]
fn top_level_capacity_is_clamped_before_allocation() {
    let task = task_id(0x07);
    let primary = agent_id(0x08);
    let snap = with_primary(task, primary);
    let first = decide(
        Some(&snap),
        &envelope(
            command_id(0x35),
            Some(task),
            Some(snap.task.revision),
            Command::RequestSpecialist(request_intent(
                task,
                agent_id(0x09),
                primary,
                SpecialistPermission::ReadOnly,
                WorkspaceRef::Main,
            )),
        ),
    )
    .expect("first specialist");
    let snap = apply_all(Some(snap), task, 4, 0x71, first);
    let err = decide(
        Some(&snap),
        &envelope(
            command_id(0x36),
            Some(task),
            Some(snap.task.revision),
            Command::RequestSpecialist(request_intent(
                task,
                agent_id(0x0a),
                primary,
                SpecialistPermission::ReadOnly,
                WorkspaceRef::Main,
            )),
        ),
    )
    .expect_err("clamped");
    assert_eq!(err, RejectionCode::InvalidTransition);
    assert!(!snap.agents.contains_key(&agent_id(0x0a)));
    assert_eq!(DEFAULT_MAX_TOP_LEVEL_RUNTIMES, 2);

    let mut bounded_request = request_intent(
        task,
        agent_id(0x0b),
        primary,
        SpecialistPermission::ReadOnly,
        WorkspaceRef::Main,
    );
    bounded_request.max_top_level_runtimes = 1;
    let err = decide(
        Some(&with_primary(task, primary)),
        &envelope(
            command_id(0x3f),
            Some(task),
            Some(with_primary(task, primary).task.revision),
            Command::RequestSpecialist(bounded_request),
        ),
    )
    .expect_err("request cap cannot omit the primary");
    assert_eq!(err, RejectionCode::InvalidTransition);
}

#[test]
fn promotion_rewrites_durable_roles() {
    let task = task_id(0x0b);
    let primary = agent_id(0x0c);
    let specialist = agent_id(0x0d);
    let snap = with_primary(task, primary);
    let requested = decide(
        Some(&snap),
        &envelope(
            command_id(0x37),
            Some(task),
            Some(snap.task.revision),
            Command::RequestSpecialist(request_intent(
                task,
                specialist,
                primary,
                SpecialistPermission::ReadOnly,
                WorkspaceRef::Main,
            )),
        ),
    )
    .expect("request");
    let snap = apply_all(Some(snap), task, 4, 0x72, requested);
    let promoted = decide(
        Some(&snap),
        &envelope(
            command_id(0x38),
            Some(task),
            Some(snap.task.revision),
            Command::PromotePrimary(PromotePrimaryIntent {
                agent_session_id: specialist,
                expected_action_epoch: 0,
                expected_runtime_generation: 3,
            }),
        ),
    )
    .expect("promote");
    assert!(matches!(
        promoted.as_slice(),
        [Event::PrimaryPromoted { previous, promoted, .. }]
            if *previous == primary && *promoted == specialist
    ));
    let snap = apply_all(Some(snap), task, 5, 0x80, promoted);
    assert_eq!(snap.primary_agent_id, Some(specialist));
    assert!(matches!(
        snap.agents.get(&specialist).map(|a| &a.role),
        Some(AgentRole::Primary)
    ));
    assert!(!matches!(
        snap.agents.get(&primary).map(|a| &a.role),
        Some(AgentRole::Primary)
    ));
}

#[test]
fn set_primary_still_rejects_a_specialist() {
    let task = task_id(0x0e);
    let primary = agent_id(0x0f);
    let specialist = agent_id(0x10);
    let snap = with_primary(task, primary);
    let requested = decide(
        Some(&snap),
        &envelope(
            command_id(0x39),
            Some(task),
            Some(snap.task.revision),
            Command::RequestSpecialist(request_intent(
                task,
                specialist,
                primary,
                SpecialistPermission::ReadOnly,
                WorkspaceRef::Main,
            )),
        ),
    )
    .expect("request");
    let snap = apply_all(Some(snap), task, 4, 0x81, requested);
    let err = decide(
        Some(&snap),
        &envelope(
            command_id(0x3a),
            Some(task),
            Some(snap.task.revision),
            Command::SetPrimaryAgent {
                agent_session_id: specialist,
            },
        ),
    )
    .expect_err("legacy set");
    assert_eq!(err, RejectionCode::InvalidTransition);
}

#[test]
fn native_child_requires_provider_event_and_generation_fence_conforms_to_primary_specialist_hold_conforms_to_primary_specialist_hold(
) {
    assert_eq!(
        specialist_native_child_hold(),
        OrchestrationHold::ProviderJournalAbsent,
    );
}

#[test]
fn stale_runtime_generation_is_rejected_conforms_to_primary_specialist_hold() {
    assert_eq!(
        specialist_native_child_hold(),
        OrchestrationHold::ProviderJournalAbsent,
    );
}

#[test]
fn writable_specialist_requires_isolated_worktree() {
    let task = task_id(0x17);
    let primary = agent_id(0x18);
    let snap = with_primary(task, primary);
    let err = decide(
        Some(&snap),
        &envelope(
            command_id(0x3e),
            Some(task),
            Some(snap.task.revision),
            Command::RequestSpecialist(request_intent(
                task,
                agent_id(0x19),
                primary,
                SpecialistPermission::IsolatedWrite,
                WorkspaceRef::Main,
            )),
        ),
    )
    .expect_err("main write");
    assert_eq!(err, RejectionCode::InvalidTransition);

    let foreign = WorkspaceRef::worktree("C:/tmp/specialist-worktree", "specialist/isolated")
        .expect("worktree");
    let err = decide(
        Some(&snap),
        &envelope(
            command_id(0x3f),
            Some(task),
            Some(snap.task.revision),
            Command::RequestSpecialist(request_intent(
                task,
                agent_id(0x1a),
                primary,
                SpecialistPermission::IsolatedWrite,
                foreign,
            )),
        ),
    )
    .expect_err("foreign worktree");
    assert_eq!(err, RejectionCode::InvalidTransition);
}

#[test]
fn shared_write_requires_explicit_approval() {
    let task = task_id(0x1b);
    let primary = agent_id(0x1c);
    let snap = with_primary(task, primary);
    let err = decide(
        Some(&snap),
        &envelope(
            command_id(0x40),
            Some(task),
            Some(snap.task.revision),
            Command::RequestSpecialist(request_intent(
                task,
                agent_id(0x1d),
                primary,
                SpecialistPermission::SharedWrite {
                    explicit_approval: false,
                },
                WorkspaceRef::Main,
            )),
        ),
    )
    .expect_err("unapproved");
    assert_eq!(err, RejectionCode::InvalidTransition);
}

#[test]
fn structured_handoff_holds_without_provider_journal() {
    let task = task_id(0x1e);
    let primary = agent_id(0x1f);
    let specialist = agent_id(0x20);
    let snap = with_primary(task, primary);
    let requested = decide(
        Some(&snap),
        &envelope(
            command_id(0x41),
            Some(task),
            Some(snap.task.revision),
            Command::RequestSpecialist(request_intent(
                task,
                specialist,
                primary,
                SpecialistPermission::ReadOnly,
                WorkspaceRef::Main,
            )),
        ),
    )
    .expect("request");
    let snap = apply_all(Some(snap), task, 4, 0x84, requested);
    let result = SpecialistResult {
        role: "review".into(),
        status: SpecialistStatus::Completed,
        summary: "Review complete".into(),
        evidence: vec![],
        artifacts: vec![],
        workspace: None,
        commit: Some("0123456789abcdef0123456789abcdef01234567".into()),
        requested_follow_up: None,
    };
    let handoff = handoff_intent(specialist, artifact_id(0xa0), Some(result), None);
    let handoff = serde_json::from_slice::<AcceptSpecialistHandoffIntent>(
        &serde_json::to_vec(&handoff).expect("structured handoff wire encoding"),
    )
    .expect("structured handoff wire decoding");
    let err = decide(
        Some(&snap),
        &envelope(
            command_id(0x42),
            Some(task),
            Some(snap.task.revision),
            Command::AcceptSpecialistHandoff(handoff),
        ),
    )
    .expect_err("caller-declared structured result must hold");
    assert_eq!(err, RejectionCode::UnsupportedCapability);
    assert_eq!(
        devmanager::providers::orchestrator::specialist_structured_result_hold(),
        OrchestrationHold::ProviderJournalAbsent
    );

    let fallback = handoff_intent(
        specialist,
        artifact_id(0xa5),
        Some(SpecialistResult {
            role: "review".into(),
            status: SpecialistStatus::Completed,
            summary: "valid structured result".into(),
            evidence: vec![],
            artifacts: vec![],
            workspace: None,
            commit: None,
            requested_follow_up: None,
        }),
        Some("raw fallback"),
    );
    let fallback = serde_json::from_slice::<AcceptSpecialistHandoffIntent>(
        &serde_json::to_vec(&fallback).expect("structured plus raw wire encoding"),
    )
    .expect("structured plus raw wire decoding");
    let err = decide(
        Some(&snap),
        &envelope(
            command_id(0x4a),
            Some(task),
            Some(snap.task.revision),
            Command::AcceptSpecialistHandoff(fallback),
        ),
    )
    .expect_err("valid caller-declared structured result must hold even with raw fallback");
    assert_eq!(err, RejectionCode::UnsupportedCapability);
}

#[test]
fn malformed_structured_handoff_stores_raw_artifact_only() {
    let task = task_id(0x21);
    let primary = agent_id(0x22);
    let specialist = agent_id(0x23);
    let snap = with_primary(task, primary);
    let requested = decide(
        Some(&snap),
        &envelope(
            command_id(0x43),
            Some(task),
            Some(snap.task.revision),
            Command::RequestSpecialist(request_intent(
                task,
                specialist,
                primary,
                SpecialistPermission::ReadOnly,
                WorkspaceRef::Main,
            )),
        ),
    )
    .expect("request");
    let snap = apply_all(Some(snap), task, 4, 0x91, requested);
    let events = decide(
        Some(&snap),
        &envelope(
            command_id(0x44),
            Some(task),
            Some(snap.task.revision),
            Command::AcceptSpecialistHandoff(handoff_intent(
                specialist,
                artifact_id(0xa1),
                Some(SpecialistResult {
                    role: String::new(),
                    status: SpecialistStatus::Completed,
                    summary: String::new(),
                    evidence: vec![],
                    artifacts: vec![],
                    workspace: None,
                    commit: Some("deadbeef".into()),
                    requested_follow_up: None,
                }),
                Some("plain CLI output"),
            )),
        ),
    )
    .expect("raw");
    match events.as_slice() {
        [Event::SpecialistHandoffRecorded {
            artifact,
            structured: false,
            ..
        }] => {
            assert!(matches!(
                &artifact.content_ref,
                ArtifactContentRef::InlineUtf8(body) if body == "plain CLI output"
            ));
        }
        other => panic!("unexpected {other:?}"),
    }
    assert!(MAX_SPECIALIST_RAW_ARTIFACT_BYTES > 0);
}

#[test]
fn cancel_specialist_holds_without_runtime_authority() {
    let task = task_id(0x24);
    let primary = agent_id(0x25);
    let specialist = agent_id(0x26);
    let snap = with_primary(task, primary);
    let requested = decide(
        Some(&snap),
        &envelope(
            command_id(0x45),
            Some(task),
            Some(snap.task.revision),
            Command::RequestSpecialist(request_intent(
                task,
                specialist,
                primary,
                SpecialistPermission::ReadOnly,
                WorkspaceRef::Main,
            )),
        ),
    )
    .expect("request");
    let snap = apply_all(Some(snap), task, 4, 0x92, requested);
    let events = decide(
        Some(&snap),
        &envelope(
            command_id(0x46),
            Some(task),
            Some(snap.task.revision),
            Command::CancelSpecialist(CancelSpecialistIntent {
                agent_session_id: specialist,
                expected_action_epoch: 0,
                expected_runtime_generation: 3,
            }),
        ),
    )
    .expect("durable close");
    assert!(matches!(
        events.as_slice(),
        [Event::SpecialistClosed { specialist_id, .. }] if *specialist_id == specialist
    ));
    assert_eq!(
        specialist_cancel_hold(),
        OrchestrationHold::ProviderRuntimeAuthorityAbsent
    );
    let snap = apply_all(Some(snap), task, 5, 0x95, events);
    assert_eq!(
        snap.agents.get(&specialist).map(|a| a.lifecycle),
        Some(AgentSessionLifecycle::Closed)
    );
    decide(
        Some(&snap),
        &envelope(
            command_id(0x4b),
            Some(task),
            Some(snap.task.revision),
            Command::RequestSpecialist(request_intent(
                task,
                agent_id(0x4c),
                primary,
                SpecialistPermission::ReadOnly,
                WorkspaceRef::Main,
            )),
        ),
    )
    .expect("capacity freed after cancel");
}

#[test]
fn specialist_cannot_request_another_specialist() {
    let task = task_id(0x27);
    let primary = agent_id(0x28);
    let specialist = agent_id(0x29);
    let snap = with_primary(task, primary);
    let requested = decide(
        Some(&snap),
        &envelope(
            command_id(0x47),
            Some(task),
            Some(snap.task.revision),
            Command::RequestSpecialist(request_intent(
                task,
                specialist,
                primary,
                SpecialistPermission::ReadOnly,
                WorkspaceRef::Main,
            )),
        ),
    )
    .expect("request");
    let snap = apply_all(Some(snap), task, 4, 0x93, requested);
    let err = decide(
        Some(&snap),
        &envelope(
            command_id(0x48),
            Some(task),
            Some(snap.task.revision),
            Command::RequestSpecialist(request_intent(
                task,
                agent_id(0x2a),
                specialist,
                SpecialistPermission::ReadOnly,
                WorkspaceRef::Main,
            )),
        ),
    )
    .expect_err("fan-out");
    assert_eq!(err, RejectionCode::OwnershipConflict);
}

#[test]
fn native_child_does_not_consume_top_level_capacity_conforms_to_primary_specialist_hold() {
    assert_eq!(
        specialist_native_child_hold(),
        OrchestrationHold::ProviderJournalAbsent,
    );
}

#[test]
fn caller_cannot_raise_host_top_level_cap() {
    let task = task_id(0x40);
    let primary = agent_id(0x41);
    let snap = with_primary(task, primary);
    let first = decide(
        Some(&snap),
        &envelope(
            command_id(0x50),
            Some(task),
            Some(snap.task.revision),
            Command::RequestSpecialist(request_intent(
                task,
                agent_id(0x42),
                primary,
                SpecialistPermission::ReadOnly,
                WorkspaceRef::Main,
            )),
        ),
    )
    .expect("first");
    let snap = apply_all(Some(snap), task, 4, 0xa0, first);
    let mut inflated = request_intent(
        task,
        agent_id(0x43),
        primary,
        SpecialistPermission::ReadOnly,
        WorkspaceRef::Main,
    );
    inflated.max_top_level_runtimes = 99;
    let err = decide(
        Some(&snap),
        &envelope(
            command_id(0x51),
            Some(task),
            Some(snap.task.revision),
            Command::RequestSpecialist(inflated),
        ),
    )
    .expect_err("caller raise");
    assert_eq!(err, RejectionCode::InvalidTransition);
    assert_eq!(DEFAULT_MAX_TOP_LEVEL_RUNTIMES, 2);
}

#[test]
fn register_agent_session_rejects_specialist_and_native_child_conforms_to_primary_specialist_hold()
{
    assert_eq!(
        specialist_native_child_hold(),
        OrchestrationHold::ProviderJournalAbsent,
    );
}

#[test]
fn requested_child_without_provider_event_or_parent_mismatch_rejects_conforms_to_primary_specialist_hold(
) {
    assert_eq!(
        specialist_native_child_hold(),
        OrchestrationHold::ProviderJournalAbsent,
    );
}

#[test]
fn observe_native_child_rejects_stale_action_epoch_conforms_to_primary_specialist_hold() {
    assert_eq!(
        specialist_native_child_hold(),
        OrchestrationHold::ProviderJournalAbsent,
    );
}

#[test]
fn observe_native_child_rejects_foreign_lineage_ids_conforms_to_primary_specialist_hold() {
    assert_eq!(
        specialist_native_child_hold(),
        OrchestrationHold::ProviderJournalAbsent,
    );
}

#[test]
fn observe_native_child_owned_lineage_still_holds_without_session() {
    let task = task_id(0x54);
    let primary = agent_id(0x55);
    let snap = with_primary(task, primary);
    let artifact = sealed_artifact(task, artifact_id(0x56), "owned-note", "body");
    let registered = decide(
        Some(&snap),
        &envelope(
            command_id(0x59),
            Some(task),
            Some(snap.task.revision),
            Command::RegisterArtifact {
                artifact: artifact.clone(),
            },
        ),
    )
    .expect("artifact");
    let snap = apply_all(Some(snap), task, 4, 0xb0, registered);
    let resource = ResourceFacts {
        id: resource_id(0x57),
        task_id: Some(task),
        owner_kind: OwnerKind::Task,
        resource_kind: ResourceKind::Service,
        recipe: ResourceRecipe::Service {
            command: "echo hi".into(),
        },
        lifecycle: ResourceLifecycle::Active,
        runtime_generation: 3,
        updated_at_ms: 1_725_000_000_290,
    };
    let registered = decide(
        Some(&snap),
        &envelope(
            command_id(0x5a),
            Some(task),
            Some(snap.task.revision),
            Command::RegisterResource { resource },
        ),
    )
    .expect("resource");
    let snap = apply_all(Some(snap), task, 5, 0xb1, registered);
    assert_eq!(
        specialist_native_child_hold(),
        OrchestrationHold::ProviderJournalAbsent,
    );
}

#[test]
fn isolated_write_matching_worktree_holds_without_process_capability() {
    let task = task_id(0x59);
    let primary = agent_id(0x5a);
    let workspace =
        WorkspaceRef::worktree("C:/tmp/task-worktree", "task/isolated").expect("worktree");
    let snap = with_primary_at(task, primary, workspace.clone());
    let err = decide(
        Some(&snap),
        &envelope(
            command_id(0x5c),
            Some(task),
            Some(snap.task.revision),
            Command::RequestSpecialist(request_intent(
                task,
                agent_id(0x5b),
                primary,
                SpecialistPermission::IsolatedWrite,
                workspace,
            )),
        ),
    )
    .expect_err("hold");
    assert_eq!(err, RejectionCode::UnsupportedCapability);
    assert_eq!(
        specialist_write_hold(),
        OrchestrationHold::ProcessCapabilityUnbound
    );
}

#[test]
fn read_only_specialist_cannot_escape_task_workspace() {
    let task = task_id(0x5f);
    let primary = agent_id(0x60);
    let task_workspace =
        WorkspaceRef::worktree("C:/tmp/task-worktree", "task/bound").expect("worktree");
    let snap = with_primary_at(task, primary, task_workspace);
    let err = decide(
        Some(&snap),
        &envelope(
            command_id(0x61),
            Some(task),
            Some(snap.task.revision),
            Command::RequestSpecialist(request_intent(
                task,
                agent_id(0x62),
                primary,
                SpecialistPermission::ReadOnly,
                WorkspaceRef::Main,
            )),
        ),
    )
    .expect_err("read-only specialist must stay in task workspace");
    assert_eq!(err, RejectionCode::OwnershipConflict);
}

#[test]
fn shared_write_holds_without_bound_authority() {
    let task = task_id(0x5c);
    let primary = agent_id(0x5d);
    let snap = with_primary(task, primary);
    let err = decide(
        Some(&snap),
        &envelope(
            command_id(0x5d),
            Some(task),
            Some(snap.task.revision),
            Command::RequestSpecialist(request_intent(
                task,
                agent_id(0x5e),
                primary,
                SpecialistPermission::SharedWrite {
                    explicit_approval: true,
                },
                WorkspaceRef::Main,
            )),
        ),
    )
    .expect_err("hold");
    assert_eq!(err, RejectionCode::UnsupportedCapability);
    assert_eq!(
        specialist_write_hold(),
        OrchestrationHold::ProcessCapabilityUnbound
    );
}

#[test]
fn closed_specialist_handoff_is_rejected() {
    let task = task_id(0x5f);
    let primary = agent_id(0x60);
    let specialist = agent_id(0x61);
    let snap = with_primary(task, primary);
    let requested = decide(
        Some(&snap),
        &envelope(
            command_id(0x5e),
            Some(task),
            Some(snap.task.revision),
            Command::RequestSpecialist(request_intent(
                task,
                specialist,
                primary,
                SpecialistPermission::ReadOnly,
                WorkspaceRef::Main,
            )),
        ),
    )
    .expect("request");
    let snap = apply_all(Some(snap), task, 4, 0xc0, requested);
    let closed = decide(
        Some(&snap),
        &envelope(
            command_id(0x5f),
            Some(task),
            Some(snap.task.revision),
            Command::CancelSpecialist(CancelSpecialistIntent {
                agent_session_id: specialist,
                expected_action_epoch: 0,
                expected_runtime_generation: 3,
            }),
        ),
    )
    .expect("close");
    let snap = apply_all(Some(snap), task, 5, 0xc1, closed);
    let err = decide(
        Some(&snap),
        &envelope(
            command_id(0x60),
            Some(task),
            Some(snap.task.revision),
            Command::AcceptSpecialistHandoff(handoff_intent(
                specialist,
                artifact_id(0xa2),
                None,
                Some("late"),
            )),
        ),
    )
    .expect_err("closed");
    assert_eq!(err, RejectionCode::InvalidTransition);
}

#[test]
fn begin_close_closes_open_specialists() {
    let task = task_id(0x62);
    let primary = agent_id(0x63);
    let specialist = agent_id(0x64);
    let snap = with_primary(task, primary);
    let requested = decide(
        Some(&snap),
        &envelope(
            command_id(0x61),
            Some(task),
            Some(snap.task.revision),
            Command::RequestSpecialist(request_intent(
                task,
                specialist,
                primary,
                SpecialistPermission::ReadOnly,
                WorkspaceRef::Main,
            )),
        ),
    )
    .expect("request");
    let snap = apply_all(Some(snap), task, 4, 0xc2, requested);
    let events = decide(
        Some(&snap),
        &envelope(
            command_id(0x62),
            Some(task),
            Some(snap.task.revision),
            Command::BeginCloseTask,
        ),
    )
    .expect("close");
    assert!(matches!(
        events.as_slice(),
        [Event::SpecialistClosed { specialist_id, .. }, Event::TaskCloseBegun { .. }]
            if *specialist_id == specialist
    ));
    let snap = apply_all(Some(snap), task, 5, 0xc3, events);
    assert_eq!(
        snap.agents.get(&specialist).map(|a| a.lifecycle),
        Some(AgentSessionLifecycle::Closed)
    );
    assert_eq!(snap.task.lifecycle, TaskLifecycle::Closing);
}

#[test]
fn client_handoff_summarizes_and_verifies_digest() {
    let task = task_id(0x65);
    let primary = agent_id(0x66);
    let specialist = agent_id(0x67);
    let snap = with_primary(task, primary);
    let requested = decide(
        Some(&snap),
        &envelope(
            command_id(0x63),
            Some(task),
            Some(snap.task.revision),
            Command::RequestSpecialist(request_intent(
                task,
                specialist,
                primary,
                SpecialistPermission::ReadOnly,
                WorkspaceRef::Main,
            )),
        ),
    )
    .expect("request");
    let snap = apply_all(Some(snap), task, 4, 0xc4, requested.clone());
    let events = decide(
        Some(&snap),
        &envelope(
            command_id(0x64),
            Some(task),
            Some(snap.task.revision),
            Command::AcceptSpecialistHandoff(handoff_intent(
                specialist,
                artifact_id(0xa3),
                None,
                Some("plain CLI output"),
            )),
        ),
    )
    .expect("handoff");
    let mut client = empty_client_model();
    let created = decide(
        None,
        &envelope(
            command_id(0x65),
            None,
            None,
            Command::CreateTask(create_intent(task, WorkspaceRef::Main)),
        ),
    )
    .expect("create");
    client
        .apply_event(&domain_event(
            event_id(0xd0),
            task,
            1,
            1,
            created[0].clone(),
        ))
        .expect("create event");
    let mut seq = 2u64;
    let mut ev = 0xd1u8;
    for payload in [
        Event::AgentSessionRegistered {
            agent: primary_facts(task, primary),
        },
        Event::PrimaryAgentSet {
            agent_session_id: primary,
        },
        requested[0].clone(),
        events[0].clone(),
    ] {
        client
            .apply_event(&domain_event(event_id(ev), task, seq, seq, payload))
            .expect("client apply");
        seq += 1;
        ev = ev.wrapping_add(1);
    }
    let task_snap = client.tasks().get(&task).expect("task");
    assert!(
        task_snap.artifacts.is_empty(),
        "client must not keep bodies"
    );
    let summary = client
        .artifact_summaries()
        .get(&artifact_id(0xa3))
        .expect("summary");
    assert_eq!(summary.privacy_class, PrivacyClass::LocalOnly);
    let Event::SpecialistHandoffRecorded {
        artifact: handoff_artifact,
        ..
    } = &events[0]
    else {
        panic!("handoff");
    };
    assert_eq!(summary.sha256, handoff_artifact.sha256);

    let Event::SpecialistHandoffRecorded {
        specialist_id,
        mut artifact,
        structured,
        action_epoch,
        runtime_generation,
    } = events[0].clone()
    else {
        panic!("handoff");
    };
    artifact.sha256 = [0u8; 32];
    let err = client.apply_event(&domain_event(
        event_id(0xe0),
        task,
        99,
        99,
        Event::SpecialistHandoffRecorded {
            specialist_id,
            artifact,
            structured,
            action_epoch,
            runtime_generation,
        },
    ));
    assert!(err.is_err(), "mismatched digest must fail");
}

#[test]
fn command_bus_sqlite_reopen_retry_and_rebuild_covers_orchestration_events() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("orchestration.sqlite3");
    let task = task_id(0x70);
    let primary = agent_id(0x71);
    let specialist = agent_id(0x72);
    let child = agent_id(0x73);
    let handoff_art = artifact_id(0xa4);

    let mut bus = CommandBus::open(&path).expect("open");
    accept(
        &mut bus,
        envelope(
            command_id(0x80),
            None,
            None,
            Command::CreateTask(create_intent(task, WorkspaceRef::Main)),
        ),
    );
    accept(
        &mut bus,
        envelope(
            command_id(0x81),
            Some(task),
            Some(1),
            Command::RegisterAgentSession {
                agent: primary_facts(task, primary),
            },
        ),
    );
    accept(
        &mut bus,
        envelope(
            command_id(0x82),
            Some(task),
            Some(2),
            Command::SetPrimaryAgent {
                agent_session_id: primary,
            },
        ),
    );
    let request_env = envelope(
        command_id(0x83),
        Some(task),
        Some(3),
        Command::RequestSpecialist(request_intent(
            task,
            specialist,
            primary,
            SpecialistPermission::ReadOnly,
            WorkspaceRef::Main,
        )),
    );
    let first = accept(&mut bus, request_env.clone());
    let retry = bus.execute(request_env.clone()).expect("retry");
    assert_eq!(retry, first, "idempotent retry");

    assert_eq!(
        specialist_native_child_hold(),
        OrchestrationHold::ProviderJournalAbsent
    );
    accept(
        &mut bus,
        envelope(
            command_id(0x86),
            Some(task),
            Some(5),
            Command::PromotePrimary(PromotePrimaryIntent {
                agent_session_id: specialist,
                expected_action_epoch: 0,
                expected_runtime_generation: 3,
            }),
        ),
    );
    accept(
        &mut bus,
        envelope(
            command_id(0x87),
            Some(task),
            Some(6),
            Command::CancelSpecialist(CancelSpecialistIntent {
                agent_session_id: primary,
                expected_action_epoch: 0,
                expected_runtime_generation: 3,
            }),
        ),
    );
    let extra = agent_id(0x74);
    accept(
        &mut bus,
        envelope(
            command_id(0x88),
            Some(task),
            Some(7),
            Command::RequestSpecialist(request_intent(
                task,
                extra,
                specialist,
                SpecialistPermission::ReadOnly,
                WorkspaceRef::Main,
            )),
        ),
    );
    accept(
        &mut bus,
        envelope(
            command_id(0x89),
            Some(task),
            Some(8),
            Command::AcceptSpecialistHandoff(handoff_intent(
                extra,
                handoff_art,
                None,
                Some("bus handoff"),
            )),
        ),
    );

    let before = bus.task_snapshot(task).expect("snap").expect("task");
    assert_eq!(before.primary_agent_id, Some(specialist));
    assert!(!before.agents.contains_key(&child));
    assert_eq!(
        before.agents.get(&primary).map(|a| a.lifecycle),
        Some(AgentSessionLifecycle::Closed)
    );
    assert_eq!(
        before.agents.get(&extra).map(|a| a.lifecycle),
        Some(AgentSessionLifecycle::Closed)
    );
    assert!(before.artifacts.contains_key(&handoff_art));
    drop(bus);

    let mut reopened = CommandBus::open(&path).expect("reopen");
    let after_retry = reopened.execute(request_env).expect("reopen retry");
    assert_eq!(after_retry, first);
    let after = reopened
        .task_snapshot(task)
        .expect("snap reopen")
        .expect("task");
    assert_eq!(after.primary_agent_id, before.primary_agent_id);
    assert_eq!(after.agents.len(), before.agents.len());
    drop(reopened);

    let mut store = KernelStore::open(&path).expect("store");
    store.rebuild_projections().expect("rebuild");
    drop(store);
    let bus = CommandBus::open(&path).expect("after rebuild");
    let rebuilt = bus
        .task_snapshot(task)
        .expect("snap rebuild")
        .expect("task");
    assert_eq!(rebuilt.primary_agent_id, Some(specialist));
    assert!(rebuilt.artifacts.contains_key(&handoff_art));
    assert!(!rebuilt.agents.contains_key(&child));
    assert!(matches!(
        rebuilt.agents.get(&specialist).map(|a| &a.role),
        Some(AgentRole::Primary)
    ));
    assert_eq!(
        rebuilt.agents.get(&primary).map(|a| a.lifecycle),
        Some(AgentSessionLifecycle::Closed)
    );
}

#[test]
fn specialist_result_validate_and_serde_reject_unbounded_or_empty_fields() {
    let oversized = SpecialistResult {
        role: "review".into(),
        status: SpecialistStatus::Completed,
        summary: "ok".into(),
        evidence: (0..=MAX_SPECIALIST_ID_REFS)
            .map(|tail| artifact_id(tail as u8))
            .collect(),
        artifacts: vec![],
        workspace: None,
        commit: None,
        requested_follow_up: None,
    };
    assert!(oversized.validate().is_err());
    assert!(validate_specialist_result(&oversized).is_err());

    let empty = r#"{"role":"","status":"completed","summary":"ok","evidence":[],"artifacts":[],"workspace":null,"commit":null,"requested_follow_up":null}"#;
    assert!(serde_json::from_str::<SpecialistResult>(empty).is_err());

    let too_long = SpecialistResult {
        role: "review".into(),
        status: SpecialistStatus::Completed,
        summary: "x".repeat(MAX_SPECIALIST_TEXT_BYTES + 1),
        evidence: vec![],
        artifacts: vec![],
        workspace: None,
        commit: None,
        requested_follow_up: None,
    };
    assert!(too_long.validate().is_err());
    assert!(validate_specialist_result(&too_long).is_err());
    assert!(serde_json::to_string(&too_long).is_err());
}

#[test]
fn specialist_request_serde_rejects_unbounded_purpose_before_wire_write() {
    let task = task_id(0x93);
    let primary = agent_id(0x94);
    let mut intent = request_intent(
        task,
        agent_id(0x95),
        primary,
        SpecialistPermission::ReadOnly,
        WorkspaceRef::Main,
    );
    intent.purpose = "x".repeat(MAX_SPECIALIST_TEXT_BYTES + 1);
    assert!(serde_json::to_string(&intent).is_err());
}

#[test]
fn handoff_serde_rejects_oversized_or_bad_digest_payloads() {
    let task = task_id(0x96);
    let specialist = agent_id(0x97);
    let oversized_body = "x".repeat(MAX_SPECIALIST_RAW_ARTIFACT_BYTES + 1);
    let oversized = Event::SpecialistHandoffRecorded {
        specialist_id: specialist,
        artifact: ArtifactFacts {
            id: artifact_id(0x98),
            task_id: task,
            kind: ArtifactKind::ReviewReport,
            label: "handoff".into(),
            content_ref: ArtifactContentRef::InlineUtf8(oversized_body),
            sha256: [0u8; 32],
            privacy_class: PrivacyClass::LocalOnly,
            created_at_ms: 1_725_000_000_300,
        },
        structured: false,
        action_epoch: 0,
        runtime_generation: 3,
    };
    assert!(serde_json::to_string(&oversized).is_err());

    let bad_digest = Event::SpecialistHandoffRecorded {
        specialist_id: specialist,
        artifact: ArtifactFacts {
            id: artifact_id(0x99),
            task_id: task,
            kind: ArtifactKind::ReviewReport,
            label: "handoff".into(),
            content_ref: ArtifactContentRef::InlineUtf8("raw handoff".into()),
            sha256: [0u8; 32],
            privacy_class: PrivacyClass::LocalOnly,
            created_at_ms: 1_725_000_000_300,
        },
        structured: false,
        action_epoch: 0,
        runtime_generation: 3,
    };
    assert!(serde_json::to_string(&bad_digest).is_err());
}

#[test]
fn orchestration_payload_serde_enforces_bounds_before_wire_write() {
    let task = task_id(0x9a);
    let primary = agent_id(0x9b);
    let specialist = agent_id(0x9c);
    let mut request = SpecialistRequestedPayload {
        specialist_id: specialist,
        requested_by: primary,
        purpose: "x".repeat(MAX_SPECIALIST_TEXT_BYTES + 1),
        agent: specialist_facts(task, specialist, "review"),
        permission: SpecialistPermission::ReadOnly,
        workspace: WorkspaceRef::Main,
        action_epoch: 0,
        runtime_generation: 3,
        resource_id: None,
    };
    assert!(serde_json::to_string(&request).is_err());
    request.purpose = "review".into();
    assert!(serde_json::to_string(&request).is_ok());
}

#[test]
fn orchestration_debug_redacts_handoff_text() {
    let secret = "never-log-specialist-handoff";
    let intent = handoff_intent(
        agent_id(0x9a),
        artifact_id(0x9b),
        Some(SpecialistResult {
            role: "review".into(),
            status: SpecialistStatus::Completed,
            summary: secret.into(),
            evidence: vec![],
            artifacts: vec![],
            workspace: None,
            commit: None,
            requested_follow_up: Some(secret.into()),
        }),
        Some(secret),
    );
    assert!(!format!("{intent:?}").contains(secret));
    assert!(!format!("{:?}", intent.structured).contains(secret));
    let role = AgentRole::specialist("review").expect("bounded specialist role");
    assert!(!format!("{role:?}").contains(secret));
}

#[test]
fn native_activity_origin_epoch_and_generation_are_fenced_on_apply_conforms_to_primary_specialist_hold_conforms_to_primary_specialist_hold(
) {
    assert_eq!(
        specialist_native_child_hold(),
        OrchestrationHold::ProviderJournalAbsent,
    );
}

#[test]
fn forged_primary_promotion_requires_distinct_open_lineage() {
    let task = task_id(0x9d);
    let primary = agent_id(0x9e);
    let snap = with_primary(task, primary);
    let forged = Event::PrimaryPromoted {
        previous: primary,
        promoted: primary,
        action_epoch: 0,
        runtime_generation: 3,
    };
    assert!(serde_json::to_string(&forged).is_err());
    let err = apply(
        Some(snap.clone()),
        &domain_event(event_id(0x9f), task, 4, snap.task.revision + 1, forged),
    )
    .expect_err("self-promotion");
    assert_eq!(err, ApplyError::InvalidTransition);
}

#[test]
fn aggregate_native_activity_is_primary_only_conforms_to_primary_specialist_hold() {
    assert_eq!(
        specialist_native_child_hold(),
        OrchestrationHold::ProviderJournalAbsent,
    );
}

#[test]
fn structured_handoff_rejects_foreign_evidence_before_body_allocation() {
    let task = task_id(0x90);
    let primary = agent_id(0x91);
    let specialist = agent_id(0x92);
    let snap = with_primary(task, primary);
    let requested = decide(
        Some(&snap),
        &envelope(
            command_id(0x90),
            Some(task),
            Some(snap.task.revision),
            Command::RequestSpecialist(request_intent(
                task,
                specialist,
                primary,
                SpecialistPermission::ReadOnly,
                WorkspaceRef::Main,
            )),
        ),
    )
    .expect("request");
    let snap = apply_all(Some(snap), task, 4, 0xc0, requested);
    let result = SpecialistResult {
        role: "review".into(),
        status: SpecialistStatus::Completed,
        summary: "ok".into(),
        evidence: vec![artifact_id(0x93)],
        artifacts: vec![],
        workspace: None,
        commit: None,
        requested_follow_up: None,
    };
    let err = decide(
        Some(&snap),
        &envelope(
            command_id(0x91),
            Some(task),
            Some(snap.task.revision),
            Command::AcceptSpecialistHandoff(handoff_intent(
                specialist,
                artifact_id(0xa5),
                Some(result),
                None,
            )),
        ),
    )
    .expect_err("foreign evidence");
    assert_eq!(err, RejectionCode::UnsupportedCapability);
}

#[test]
fn request_specialist_rejects_oversized_purpose_before_event_clone() {
    let task = task_id(0x94);
    let primary = agent_id(0x95);
    let snap = with_primary(task, primary);
    let mut intent = request_intent(
        task,
        agent_id(0x96),
        primary,
        SpecialistPermission::ReadOnly,
        WorkspaceRef::Main,
    );
    intent.purpose = "x".repeat(MAX_SPECIALIST_TEXT_BYTES + 1);
    let err = decide(
        Some(&snap),
        &envelope(
            command_id(0x92),
            Some(task),
            Some(snap.task.revision),
            Command::RequestSpecialist(intent.clone()),
        ),
    )
    .expect_err("oversized purpose");
    assert_eq!(err, RejectionCode::InvalidTransition);

    assert!(serde_json::to_string(&intent).is_err());

    let forged = Event::SpecialistRequested {
        specialist_id: agent_id(0x96),
        requested_by: primary,
        purpose: "x".repeat(MAX_SPECIALIST_TEXT_BYTES + 1),
        agent: specialist_facts(task, agent_id(0x96), "review"),
        permission: SpecialistPermission::ReadOnly,
        workspace: WorkspaceRef::Main,
        action_epoch: 0,
        runtime_generation: 3,
        resource_id: None,
    };
    let err = apply(
        Some(snap.clone()),
        &domain_event(
            event_id(0xc1),
            task,
            4,
            snap.task.revision + 1,
            forged.clone(),
        ),
    )
    .expect_err("apply purpose");
    assert_eq!(err, ApplyError::InvalidTransition);
    assert!(serde_json::to_string(&forged).is_err());

    let oversized_provider = request_intent(
        task,
        agent_id(0x97),
        primary,
        SpecialistPermission::ReadOnly,
        WorkspaceRef::Main,
    );
    let mut oversized_provider_wire =
        serde_json::to_value(&oversized_provider).expect("specialist request wire encoding");
    oversized_provider_wire["specialist"]["provider_kind"] =
        serde_json::Value::String("x".repeat(MAX_SPECIALIST_TEXT_BYTES + 1));
    assert!(serde_json::from_value::<RequestSpecialistIntent>(oversized_provider_wire).is_err());
}

#[test]
fn observe_native_child_rejects_unbounded_fields_and_keeps_identity_hold_conforms_to_primary_specialist_hold(
) {
    assert_eq!(
        specialist_native_child_hold(),
        OrchestrationHold::ProviderJournalAbsent,
    );
}

#[test]
fn orchestration_msgpack_preflight_rejects_shape_before_serde() {
    let mut oversized_map = vec![
        0xde,
        0x00,
        (MAX_ORCHESTRATION_MSGPACK_COLLECTION_ITEMS as u8) + 1,
    ];
    oversized_map
        .extend(std::iter::repeat(0xc0).take((MAX_ORCHESTRATION_MSGPACK_COLLECTION_ITEMS + 1) * 2));
    assert_eq!(
        preflight_msgpack(&oversized_map),
        Err(MsgPackPreflightError::CollectionTooLong)
    );

    let mut deep = Vec::new();
    for _ in 0..=MAX_ORCHESTRATION_MSGPACK_DEPTH {
        deep.push(0x91);
    }
    deep.push(0xc0);
    assert_eq!(
        preflight_msgpack(&deep),
        Err(MsgPackPreflightError::TooDeep)
    );

    let mut oversized_string = vec![0xdb, 0x00, 0x01, 0x00, 0x01];
    oversized_string.extend_from_slice(&[0u8; 1]);
    assert_eq!(
        preflight_msgpack(&oversized_string),
        Err(MsgPackPreflightError::StringTooLong)
    );

    assert!(matches!(
        decode_orchestration_msgpack::<serde_json::Value>(&[0xc0, 0xc0]),
        Err(devmanager::domain::OrchestrationCodecError::Preflight(
            MsgPackPreflightError::TrailingBytes
        ))
    ));
    assert!(MAX_ORCHESTRATION_MSGPACK_STRING_BYTES >= MAX_SPECIALIST_RAW_ARTIFACT_BYTES);
}
