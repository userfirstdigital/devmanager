//! Pure CommandBus admission tests using temporary databases only.

use std::path::{Path, PathBuf};
use std::time::Duration;

use devmanager::domain::command::{Command, CommandEnvelope, CommandReceipt, CreateTaskIntent};
use devmanager::domain::id::{ClientId, CommandId, EnvironmentId, OperationId, ProjectId, TaskId};
use devmanager::domain::operation::{CancellationReason, OperationState};
use devmanager::domain::task::{
    ReviewReadiness, TaskActivity, TaskAssignment, TaskAttention, TaskConnectivity, TaskLifecycle,
    WorkspaceRef,
};
use devmanager::kernel::{CommandBus, KernelStore};
use rusqlite::Connection;
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

fn command_id(tail: u8) -> CommandId {
    CommandId::from_bytes(fixed_uuid_v7(tail)).expect("command id")
}

fn temp_db_path(dir: &TempDir) -> PathBuf {
    dir.path().join("kernel.sqlite3")
}

fn open_raw(path: &Path) -> Connection {
    let conn = Connection::open(path).expect("open raw");
    conn.execute_batch("PRAGMA foreign_keys=ON;")
        .expect("foreign keys");
    conn
}

fn envelope(
    command_id: CommandId,
    task_id: Option<TaskId>,
    expected_task_revision: Option<u64>,
    command: Command,
) -> CommandEnvelope {
    CommandEnvelope {
        command_id,
        client_id: ClientId::from_bytes(fixed_uuid_v7(0x20)).expect("client id"),
        task_id,
        issued_at_ms: 1_725_000_000_100,
        expected_task_revision,
        command,
    }
}

fn create_task(task_id: TaskId) -> CreateTaskIntent {
    CreateTaskIntent {
        id: task_id,
        environment_id: EnvironmentId::from_bytes(fixed_uuid_v7(0x21)).expect("environment id"),
        title: "Atomic reopen".into(),
        description: None,
        project_id: ProjectId::from_bytes(fixed_uuid_v7(0x22)).expect("project id"),
        workspace: WorkspaceRef::Main,
        assignment: TaskAssignment::LocalOwner,
        created_at_ms: 1_725_000_000_000,
        connectivity: TaskConnectivity::Connected,
        attention: TaskAttention::None,
        activity: TaskActivity::Idle,
        review_readiness: ReviewReadiness::NotReady,
    }
}

fn cancelled_fact_count(conn: &Connection, operation_id: OperationId) -> i64 {
    let mut stmt = conn
        .prepare(
            "SELECT payload FROM events WHERE event_type = 'operation.cancelled' ORDER BY sequence",
        )
        .expect("prepare cancelled facts");
    let rows = stmt
        .query_map([], |row| row.get::<_, Vec<u8>>(0))
        .expect("query cancelled facts");
    rows.map(|row| {
        let payload = row.expect("cancelled payload");
        rmp_serde::from_slice::<devmanager::domain::event::OperationCancelledFact>(&payload)
            .expect("decode cancelled fact")
    })
    .filter(|fact| fact.operation_id == operation_id)
    .count() as i64
}

#[test]
fn reopen_atomically_cancels_pending_close() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut bus = CommandBus::open(&path).expect("open bus");
    let task = task_id(0xA1);

    bus.execute(envelope(
        command_id(0xA2),
        None,
        None,
        Command::CreateTask(create_task(task)),
    ))
    .expect("create task");

    let close_envelope = envelope(
        command_id(0xA3),
        Some(task),
        Some(1),
        Command::BeginCloseTask,
    );
    let close_receipt = bus
        .execute(close_envelope.clone())
        .expect("accept begin close");
    let close_operation = match &close_receipt {
        CommandReceipt::Accepted { operation_id, .. } => *operation_id,
        other => panic!("expected accepted close, got {other:?}"),
    };

    let reopen_envelope = envelope(command_id(0xA4), Some(task), Some(2), Command::ReopenTask);
    let reopen_receipt = bus.execute(reopen_envelope.clone()).expect("reopen task");
    assert!(matches!(&reopen_receipt, CommandReceipt::Accepted { .. }));

    let snapshot = bus
        .task_snapshot(task)
        .expect("task snapshot")
        .expect("task present");
    assert_eq!(snapshot.task.lifecycle, TaskLifecycle::Open);
    assert_eq!(snapshot.task.revision, 3);
    assert_eq!(snapshot.task.action_epoch, 1);
    assert!(matches!(
        bus.operation_status(close_operation)
            .expect("close operation status"),
        Some(OperationState::Cancelled {
            reason: CancellationReason::Superseded,
            ..
        })
    ));

    {
        let conn = open_raw(&path);
        let (state, error, lease, attempts, started): (
            String,
            Option<String>,
            Option<i64>,
            i64,
            Option<i64>,
        ) = conn
            .query_row(
                "SELECT state, last_error_class, leased_until_ms, attempts, dispatch_started_at_ms
                 FROM outbox WHERE operation_id = ?1",
                [close_operation.as_bytes().as_slice()],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .expect("close outbox row");
        assert_eq!(state, "cancelled");
        assert_eq!(error.as_deref(), Some("superseded"));
        assert!(lease.is_none());
        assert_eq!(attempts, 0);
        assert!(started.is_none());
        assert_eq!(cancelled_fact_count(&conn, close_operation), 1);
    }

    assert_eq!(
        bus.execute(close_envelope).expect("retry original close"),
        close_receipt,
        "original command retry must retain its accepted receipt"
    );
    assert_eq!(
        bus.execute(reopen_envelope).expect("retry reopen"),
        reopen_receipt,
        "reopen retry must not emit a second cancellation"
    );
    drop(bus);

    let conn = open_raw(&path);
    assert_eq!(cancelled_fact_count(&conn, close_operation), 1);
    drop(conn);
    let mut store = KernelStore::open(&path).expect("reopen store");
    assert_eq!(
        store
            .claim_next_dispatch(Duration::from_secs(30))
            .expect("scan after reopen"),
        None,
        "superseded close must leave no claimable teardown"
    );
}
