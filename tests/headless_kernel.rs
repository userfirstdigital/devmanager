//! Vertically usable Phase 1.8 headless CommandBus boundary.

use std::path::PathBuf;

use devmanager::domain::command::{Command, CommandEnvelope, CreateTaskIntent};
use devmanager::domain::id::{ClientId, CommandId, EnvironmentId, ProjectId, RequestId, TaskId};
use devmanager::domain::query::{Query, QueryEnvelope, QueryError, QueryOutcome};
use devmanager::domain::task::{
    ReviewReadiness, TaskActivity, TaskAssignment, TaskAttention, TaskConnectivity, WorkspaceRef,
};
use devmanager::kernel::{CommandBus, StoreError};
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
fn public_command_bus_rejects_host_only_create_before_and_after_reopen() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let task = task_id(0xE1);
    let cmd = command_id(0xE2);
    let envelope = create_task_envelope(cmd, task);

    let mut bus = CommandBus::open(&path).expect("open bus");
    assert_eq!(
        bus.execute(envelope.clone()),
        Err(StoreError::HostAuthorityRequired)
    );
    assert_eq!(
        bus.execute(envelope.clone()),
        Err(StoreError::HostAuthorityRequired),
        "an identical retry must remain outside the host-only boundary"
    );
    assert!(bus
        .task_snapshot(task)
        .expect("snapshot before drop")
        .is_none());

    drop(bus);

    let mut reopened = CommandBus::open(&path).expect("reopen bus");
    assert_eq!(
        reopened.execute(envelope),
        Err(StoreError::HostAuthorityRequired),
        "reopening must not weaken the host-only boundary"
    );
    assert!(reopened
        .task_snapshot(task)
        .expect("snapshot after reopen")
        .is_none());
}

#[test]
fn command_bus_query_rejects_missing_scope_and_reports_unknown_task() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let task = task_id(0xF1);
    let client = client_id(0x20);

    let bus = CommandBus::open(&path).expect("open bus");

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
            task_id: Some(task),
            query: Query::TaskSnapshot,
        })
        .expect("missing task query");
    assert_eq!(missing.outcome, QueryOutcome::Err(QueryError::NotFound));
}
