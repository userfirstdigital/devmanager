//! Vertically usable Phase 1.8 headless CommandBus boundary.

use std::path::PathBuf;

use devmanager::domain::command::{Command, CommandEnvelope, CommandReceipt, CreateTaskIntent};
use devmanager::domain::id::{ClientId, CommandId, EnvironmentId, ProjectId, RequestId, TaskId};
use devmanager::domain::operation::OperationState;
use devmanager::domain::query::{Query, QueryEnvelope, QueryError, QueryOutcome, QueryResult};
use devmanager::domain::task::{
    ReviewReadiness, TaskActivity, TaskAssignment, TaskAttention, TaskConnectivity, WorkspaceRef,
};
use devmanager::kernel::CommandBus;
use tempfile::TempDir;

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

fn command_id(tail: u8) -> CommandId {
    CommandId::from_bytes(fixed_uuid_v7(tail)).expect("command id")
}

fn client_id(tail: u8) -> ClientId {
    ClientId::from_bytes(fixed_uuid_v7(tail)).expect("client id")
}

fn temp_db_path(dir: &TempDir) -> PathBuf {
    dir.path().join("kernel.sqlite3")
}

fn create_task_intent(task: TaskId) -> CreateTaskIntent {
    CreateTaskIntent {
        id: task,
        environment_id: env_id(0x10),
        title: "Headless boundary".into(),
        description: Some("Phase 1.8".into()),
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

fn create_task_envelope(cmd: CommandId, task: TaskId) -> CommandEnvelope {
    CommandEnvelope {
        command_id: cmd,
        client_id: client_id(0x20),
        task_id: None,
        issued_at_ms: 1_725_000_000_100,
        expected_task_revision: None,
        command: Command::CreateTask(create_task_intent(task)),
    }
}

#[test]
fn command_bus_idempotent_create_survives_reopen() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let task = task_id(0xE1);
    let cmd = command_id(0xE2);
    let envelope = create_task_envelope(cmd, task);

    let mut bus = CommandBus::open(&path).expect("open bus");
    let first = bus.execute(envelope.clone()).expect("create task");
    let CommandReceipt::Accepted {
        operation_id: first_op,
        ..
    } = first.clone()
    else {
        panic!("expected accepted receipt, got {first:?}");
    };

    let retry = bus
        .execute(envelope.clone())
        .expect("retry identical command");
    assert_eq!(
        retry, first,
        "identical command must yield identical receipt"
    );
    assert_eq!(
        retry.accepted_operation_id(),
        Some(first_op),
        "retry must preserve OperationId"
    );

    let settled = bus
        .operation_status(first_op)
        .expect("status before drop")
        .expect("known operation");
    assert!(
        matches!(settled, OperationState::Settled { .. }),
        "create-task must settle, got {settled:?}"
    );

    let snapshot_before = bus
        .task_snapshot(task)
        .expect("snapshot before drop")
        .expect("created task snapshot");
    assert_eq!(snapshot_before.task.id, task);
    assert_eq!(snapshot_before.task.title, "Headless boundary");
    assert_eq!(snapshot_before.task.revision, 1);

    drop(bus);

    let mut reopened = CommandBus::open(&path).expect("reopen bus");
    let after_retry = reopened
        .execute(envelope)
        .expect("retry identical command after reopen");
    assert_eq!(
        after_retry, first,
        "post-reopen identical command must yield identical receipt"
    );
    assert_eq!(
        after_retry.accepted_operation_id(),
        Some(first_op),
        "post-reopen retry must preserve OperationId"
    );

    let after = reopened
        .operation_status(first_op)
        .expect("status after reopen")
        .expect("known operation after reopen");
    assert_eq!(
        after, settled,
        "settled operation state must survive reopen"
    );

    let snapshot_after = reopened
        .task_snapshot(task)
        .expect("snapshot after reopen")
        .expect("created task snapshot after reopen");
    assert_eq!(snapshot_after.task.id, task);
    assert_eq!(snapshot_after.task.title, "Headless boundary");
    assert_eq!(snapshot_after.task.revision, 1);
    assert_eq!(
        snapshot_after, snapshot_before,
        "task snapshot must survive reopen"
    );
}

#[test]
fn command_bus_query_task_snapshot_and_missing_scope() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let task = task_id(0xF1);
    let cmd = command_id(0xF2);
    let client = client_id(0x20);
    let envelope = create_task_envelope(cmd, task);

    let mut bus = CommandBus::open(&path).expect("open bus");
    let _ = bus.execute(envelope).expect("create task");

    let request_id = RequestId::from_bytes(fixed_uuid_v7(0xF3)).expect("request id");
    let reply = bus
        .query(QueryEnvelope {
            request_id,
            client_id: client,
            task_id: Some(task),
            query: Query::TaskSnapshot,
        })
        .expect("task snapshot query");
    assert_eq!(reply.request_id, request_id);
    match reply.outcome {
        QueryOutcome::Ok(QueryResult::TaskSnapshot { snapshot }) => {
            assert_eq!(snapshot.task.id, task);
            assert_eq!(snapshot.task.title, "Headless boundary");
            assert_eq!(snapshot.task.revision, 1);
        }
        other => panic!("expected task snapshot, got {other:?}"),
    }

    let invalid = bus
        .query(QueryEnvelope {
            request_id: RequestId::from_bytes(fixed_uuid_v7(0xF4)).expect("request id"),
            client_id: client,
            task_id: None,
            query: Query::TaskSnapshot,
        })
        .expect("missing scope query");
    assert_eq!(
        invalid.outcome,
        QueryOutcome::Err(QueryError::InvalidRequest)
    );

    let missing = bus
        .query(QueryEnvelope {
            request_id: RequestId::from_bytes(fixed_uuid_v7(0xF5)).expect("request id"),
            client_id: client,
            task_id: Some(task_id(0xF6)),
            query: Query::TaskSnapshot,
        })
        .expect("missing task query");
    assert_eq!(missing.outcome, QueryOutcome::Err(QueryError::NotFound));
}
