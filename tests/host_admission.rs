//! Pure CommandBus admission tests using temporary databases only.

use std::path::{Path, PathBuf};
use std::time::Duration;

use devmanager::domain::command::{Command, CommandEnvelope, CommandReceipt, CreateTaskIntent};
use devmanager::domain::id::{
    ClientId, CommandId, EnvironmentId, OperationId, ProjectId, ResourceId, TaskId,
};
use devmanager::domain::operation::{CancellationReason, OperationState};
use devmanager::domain::resource::{
    OwnerKind, ResourceFacts, ResourceKind, ResourceLifecycle, ResourceRecipe,
};
use devmanager::domain::task::{
    ReviewReadiness, TaskActivity, TaskAssignment, TaskAttention, TaskConnectivity, TaskLifecycle,
    WorkspaceRef,
};
use devmanager::host::{ProcessEmptyTeardown, ProcessEmptyTeardownWorker};
use devmanager::kernel::{CommandBus, KernelStore, StoreError};
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

fn resource_id(tail: u8) -> ResourceId {
    ResourceId::from_bytes(fixed_uuid_v7(tail)).expect("resource id")
}

fn accept_begin_close(bus: &mut CommandBus, task: TaskId, cmd: CommandId, rev: u64) -> OperationId {
    let receipt = bus
        .execute(envelope(
            cmd,
            Some(task),
            Some(rev),
            Command::BeginCloseTask,
        ))
        .expect("begin close");
    match receipt {
        CommandReceipt::Accepted { operation_id, .. } => operation_id,
        other => panic!("expected accepted close, got {other:?}"),
    }
}

fn outbox_dispatch_fields(
    conn: &Connection,
    operation_id: OperationId,
) -> (String, Option<String>, Option<i64>, i64, Option<i64>, i64) {
    conn.query_row(
        "SELECT state, last_error_class, leased_until_ms, attempts, dispatch_started_at_ms,
                lease_generation
         FROM outbox WHERE operation_id = ?1",
        [operation_id.as_bytes().as_slice()],
        |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
            ))
        },
    )
    .expect("outbox row")
}

fn register_active_terminal(
    bus: &mut CommandBus,
    task: TaskId,
    cmd: CommandId,
    resource: ResourceId,
    rev: u64,
) {
    let receipt = bus
        .execute(envelope(
            cmd,
            Some(task),
            Some(rev),
            Command::RegisterResource {
                resource: ResourceFacts {
                    id: resource,
                    task_id: Some(task),
                    owner_kind: OwnerKind::Task,
                    resource_kind: ResourceKind::Terminal,
                    recipe: ResourceRecipe::Terminal { cols: 80, rows: 24 },
                    lifecycle: ResourceLifecycle::Active,
                    runtime_generation: 0,
                    updated_at_ms: 1_725_000_000_050,
                },
            },
        ))
        .expect("register resource");
    assert!(
        matches!(receipt, CommandReceipt::Accepted { .. }),
        "register must accept, got {receipt:?}"
    );
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

#[test]
fn process_empty_teardown_settles_and_reopen_atomically_cancels_pending_close() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut bus = CommandBus::open(&path).expect("open bus");

    // --- Task A: empty close settles once ---
    let task_a = task_id(0xB1);
    bus.execute(envelope(
        command_id(0xB2),
        None,
        None,
        Command::CreateTask(create_task(task_a)),
    ))
    .expect("create A");
    let close_a = accept_begin_close(&mut bus, task_a, command_id(0xB3), 1);
    assert_eq!(
        ProcessEmptyTeardownWorker::run_once(&mut bus).expect("settle A"),
        ProcessEmptyTeardown::Settled {
            task_id: task_a,
            operation_id: close_a,
        }
    );
    assert!(matches!(
        bus.operation_status(close_a).expect("A close status"),
        Some(OperationState::Settled { .. })
    ));
    let snap_a = bus
        .task_snapshot(task_a)
        .expect("A snapshot")
        .expect("A present");
    assert_eq!(snap_a.task.lifecycle, TaskLifecycle::Archived);
    assert_eq!(snap_a.task.revision, 3);
    assert_eq!(snap_a.task.action_epoch, 1);
    assert_eq!(
        ProcessEmptyTeardownWorker::run_once(&mut bus).expect("A idle"),
        ProcessEmptyTeardown::Idle
    );

    // --- Task B: reopen cancels untouched close; worker stays Idle ---
    let task_b = task_id(0xB4);
    bus.execute(envelope(
        command_id(0xB5),
        None,
        None,
        Command::CreateTask(create_task(task_b)),
    ))
    .expect("create B");
    let close_b = accept_begin_close(&mut bus, task_b, command_id(0xB6), 1);
    let reopen_b = bus
        .execute(envelope(
            command_id(0xB7),
            Some(task_b),
            Some(2),
            Command::ReopenTask,
        ))
        .expect("reopen B");
    assert!(matches!(reopen_b, CommandReceipt::Accepted { .. }));
    let snap_b = bus
        .task_snapshot(task_b)
        .expect("B snapshot")
        .expect("B present");
    assert_eq!(snap_b.task.lifecycle, TaskLifecycle::Open);
    assert_eq!(snap_b.task.revision, 3);
    assert_eq!(snap_b.task.action_epoch, 1);
    assert!(matches!(
        bus.operation_status(close_b).expect("B close status"),
        Some(OperationState::Cancelled {
            reason: CancellationReason::Superseded,
            ..
        })
    ));
    {
        let conn = open_raw(&path);
        let (state, error, lease, attempts, started, generation) =
            outbox_dispatch_fields(&conn, close_b);
        assert_eq!(state, "cancelled");
        assert_eq!(error.as_deref(), Some("superseded"));
        assert!(lease.is_none());
        assert_eq!(attempts, 0);
        assert!(started.is_none());
        assert_eq!(generation, 0);
        assert_eq!(cancelled_fact_count(&conn, close_b), 1);
    }
    assert_eq!(
        ProcessEmptyTeardownWorker::run_once(&mut bus).expect("B idle"),
        ProcessEmptyTeardown::Idle
    );

    // --- Task C: live Active resource blocks process-empty ---
    let task_c = task_id(0xB8);
    let resource_c = resource_id(0xB9);
    bus.execute(envelope(
        command_id(0xBA),
        None,
        None,
        Command::CreateTask(create_task(task_c)),
    ))
    .expect("create C");
    register_active_terminal(&mut bus, task_c, command_id(0xBB), resource_c, 1);
    let close_c = accept_begin_close(&mut bus, task_c, command_id(0xBC), 2);
    assert_eq!(
        ProcessEmptyTeardownWorker::run_once(&mut bus).expect("C idle"),
        ProcessEmptyTeardown::Idle
    );
    assert!(matches!(
        bus.operation_status(close_c).expect("C close status"),
        Some(OperationState::Accepted)
    ));
    let snap_c = bus
        .task_snapshot(task_c)
        .expect("C snapshot")
        .expect("C present");
    assert_eq!(snap_c.task.lifecycle, TaskLifecycle::Closing);
    {
        let conn = open_raw(&path);
        let lifecycle: String = conn
            .query_row(
                "SELECT lifecycle FROM resources WHERE resource_id = ?1",
                [resource_c.as_bytes().as_slice()],
                |row| row.get(0),
            )
            .expect("C resource");
        assert_eq!(lifecycle, "active");
        let (state, error, lease, attempts, started, generation) =
            outbox_dispatch_fields(&conn, close_c);
        assert_eq!(state, "pending");
        assert!(error.is_none());
        assert!(lease.is_none());
        assert_eq!(attempts, 0);
        assert!(started.is_none());
        assert_eq!(generation, 0);
    }

    // --- Earlier unrelated ResourceRelease must not be claimed ---
    // After ReleaseResource (rev 3), BeginClose advances R to Closing/epoch1 while the
    // resource stays Releasing — the older release fence is stale and R's own teardown
    // remains resource-bearing/ineligible.
    let task_r = task_id(0xBD);
    let resource_r = resource_id(0xBE);
    bus.execute(envelope(
        command_id(0xBF),
        None,
        None,
        Command::CreateTask(create_task(task_r)),
    ))
    .expect("create R");
    register_active_terminal(&mut bus, task_r, command_id(0xC0), resource_r, 1);
    let release_r = {
        let receipt = bus
            .execute(envelope(
                command_id(0xC1),
                Some(task_r),
                Some(2),
                Command::ReleaseResource {
                    resource_id: resource_r,
                },
            ))
            .expect("begin release R");
        match receipt {
            CommandReceipt::Accepted { operation_id, .. } => operation_id,
            other => panic!("expected accepted release, got {other:?}"),
        }
    };
    let close_r = accept_begin_close(&mut bus, task_r, command_id(0xC5), 3);
    let snap_r = bus
        .task_snapshot(task_r)
        .expect("R snapshot")
        .expect("R present");
    assert_eq!(snap_r.task.lifecycle, TaskLifecycle::Closing);
    assert_eq!(snap_r.task.action_epoch, 1);
    {
        let conn = open_raw(&path);
        let lifecycle: String = conn
            .query_row(
                "SELECT lifecycle FROM resources WHERE resource_id = ?1",
                [resource_r.as_bytes().as_slice()],
                |row| row.get(0),
            )
            .expect("R resource");
        assert_eq!(lifecycle, "releasing");
    }

    let task_t = task_id(0xC2);
    bus.execute(envelope(
        command_id(0xC3),
        None,
        None,
        Command::CreateTask(create_task(task_t)),
    ))
    .expect("create T");
    let close_t = accept_begin_close(&mut bus, task_t, command_id(0xC4), 1);
    assert_eq!(
        ProcessEmptyTeardownWorker::run_once(&mut bus).expect("settle T past release"),
        ProcessEmptyTeardown::Settled {
            task_id: task_t,
            operation_id: close_t,
        }
    );
    assert!(matches!(
        bus.operation_status(close_t).expect("T close status"),
        Some(OperationState::Settled { .. })
    ));
    assert_eq!(
        ProcessEmptyTeardownWorker::run_once(&mut bus).expect("idle after T with stale release"),
        ProcessEmptyTeardown::Idle
    );
    {
        let conn = open_raw(&path);
        let release_after = outbox_dispatch_fields(&conn, release_r);
        assert_eq!(release_after.0, "pending");
        assert!(release_after.1.is_none());
        assert!(release_after.2.is_none());
        assert_eq!(release_after.3, 0);
        assert!(release_after.4.is_none());
        assert_eq!(release_after.5, 0);
        let close_r_after = outbox_dispatch_fields(&conn, close_r);
        assert_eq!(close_r_after.0, "pending");
        assert!(close_r_after.1.is_none());
        assert!(close_r_after.2.is_none());
        assert_eq!(close_r_after.3, 0);
        assert!(close_r_after.4.is_none());
        assert_eq!(close_r_after.5, 0);
        let lifecycle: String = conn
            .query_row(
                "SELECT lifecycle FROM resources WHERE resource_id = ?1",
                [resource_r.as_bytes().as_slice()],
                |row| row.get(0),
            )
            .expect("R resource after idle");
        assert_eq!(lifecycle, "releasing");
    }

    // --- Two eligible empties: one settle per call, oldest first ---
    let task_d1 = task_id(0xC6);
    let task_d2 = task_id(0xC7);
    bus.execute(envelope(
        command_id(0xC8),
        None,
        None,
        Command::CreateTask(create_task(task_d1)),
    ))
    .expect("create D1");
    bus.execute(envelope(
        command_id(0xC9),
        None,
        None,
        Command::CreateTask(create_task(task_d2)),
    ))
    .expect("create D2");
    let close_d1 = accept_begin_close(&mut bus, task_d1, command_id(0xCA), 1);
    let close_d2 = accept_begin_close(&mut bus, task_d2, command_id(0xCB), 1);
    assert_eq!(
        ProcessEmptyTeardownWorker::run_once(&mut bus).expect("settle oldest"),
        ProcessEmptyTeardown::Settled {
            task_id: task_d1,
            operation_id: close_d1,
        }
    );
    assert!(matches!(
        bus.operation_status(close_d2).expect("D2 still accepted"),
        Some(OperationState::Accepted)
    ));
    let snap_d2 = bus
        .task_snapshot(task_d2)
        .expect("D2 snapshot")
        .expect("D2 present");
    assert_eq!(snap_d2.task.lifecycle, TaskLifecycle::Closing);
    assert_eq!(
        ProcessEmptyTeardownWorker::run_once(&mut bus).expect("settle next"),
        ProcessEmptyTeardown::Settled {
            task_id: task_d2,
            operation_id: close_d2,
        }
    );
    assert_eq!(
        ProcessEmptyTeardownWorker::run_once(&mut bus).expect("final idle"),
        ProcessEmptyTeardown::Idle
    );
}

#[test]
fn process_empty_teardown_fails_closed_on_corrupt_oldest_resource_fence() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut bus = CommandBus::open(&path).expect("open bus");

    let task_old = task_id(0xD1);
    let task_new = task_id(0xD2);
    bus.execute(envelope(
        command_id(0xD3),
        None,
        None,
        Command::CreateTask(create_task(task_old)),
    ))
    .expect("create oldest");
    bus.execute(envelope(
        command_id(0xD4),
        None,
        None,
        Command::CreateTask(create_task(task_new)),
    ))
    .expect("create later");
    let close_old = accept_begin_close(&mut bus, task_old, command_id(0xD5), 1);
    let close_new = accept_begin_close(&mut bus, task_new, command_id(0xD6), 1);
    drop(bus);

    let forged_resource = resource_id(0xD7);
    {
        let conn = open_raw(&path);
        let changed = conn
            .execute(
                "UPDATE operations
                 SET resource_id = ?1, runtime_generation = 7
                 WHERE operation_id = ?2",
                rusqlite::params![
                    forged_resource.as_bytes().as_slice(),
                    close_old.as_bytes().as_slice(),
                ],
            )
            .expect("forge resource fence on oldest close");
        assert_eq!(changed, 1);
    }

    let mut bus = CommandBus::open(&path).expect("reopen bus");
    let err = ProcessEmptyTeardownWorker::run_once(&mut bus)
        .expect_err("corrupt oldest fence must fail closed");
    assert_eq!(err, StoreError::Corruption);

    // Later clean op still reconstructs through the facade; forged oldest fails closed
    // on status reconstruction, so prove its Accepted projection via the temp DB.
    assert!(matches!(
        bus.operation_status(close_new).expect("later op"),
        Some(OperationState::Accepted)
    ));
    let snap_old = bus
        .task_snapshot(task_old)
        .expect("oldest snapshot")
        .expect("oldest present");
    let snap_new = bus
        .task_snapshot(task_new)
        .expect("later snapshot")
        .expect("later present");
    assert_eq!(snap_old.task.lifecycle, TaskLifecycle::Closing);
    assert_eq!(snap_new.task.lifecycle, TaskLifecycle::Closing);
    {
        let conn = open_raw(&path);
        let oldest_state: String = conn
            .query_row(
                "SELECT state FROM operations WHERE operation_id = ?1",
                [close_old.as_bytes().as_slice()],
                |row| row.get(0),
            )
            .expect("oldest operation projection");
        assert_eq!(oldest_state, "accepted");
        let later_state: String = conn
            .query_row(
                "SELECT state FROM operations WHERE operation_id = ?1",
                [close_new.as_bytes().as_slice()],
                |row| row.get(0),
            )
            .expect("later operation projection");
        assert_eq!(later_state, "accepted");
        for operation_id in [close_old, close_new] {
            let (state, error, lease, attempts, started, generation) =
                outbox_dispatch_fields(&conn, operation_id);
            assert_eq!(state, "pending");
            assert!(error.is_none());
            assert!(lease.is_none());
            assert_eq!(attempts, 0);
            assert!(started.is_none());
            assert_eq!(generation, 0);
        }
    }
}
