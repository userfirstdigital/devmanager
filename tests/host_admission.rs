//! Pure CommandBus admission tests using temporary databases only.

use std::path::{Path, PathBuf};
use std::time::Duration;

use devmanager::domain::command::{
    Command, CommandEnvelope, CommandReceipt, ConfirmHostQuitIntent, CreateTaskIntent,
    RejectionCode,
};
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
use rusqlite::{Connection, OptionalExtension};
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

fn count_table(path: &Path, table: &str) -> i64 {
    let conn = open_raw(path);
    conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
        row.get(0)
    })
    .expect("count table")
}

fn max_event_sequence(path: &Path) -> i64 {
    let conn = open_raw(path);
    conn.query_row("SELECT COALESCE(MAX(sequence), 0) FROM events", [], |row| {
        row.get(0)
    })
    .expect("max sequence")
}

fn host_close_begun_count(path: &Path) -> i64 {
    let conn = open_raw(path);
    conn.query_row(
        "SELECT COUNT(*) FROM events WHERE event_type = 'host.close_begun'",
        [],
        |row| row.get(0),
    )
    .expect("host.close_begun count")
}

fn operation_accepted_count(path: &Path) -> i64 {
    let conn = open_raw(path);
    conn.query_row(
        "SELECT COUNT(*) FROM events WHERE event_type = 'operation.accepted'",
        [],
        |row| row.get(0),
    )
    .expect("operation.accepted count")
}

fn outbox_count_for_operation(path: &Path, operation_id: OperationId) -> i64 {
    let conn = open_raw(path);
    conn.query_row(
        "SELECT COUNT(*) FROM outbox WHERE operation_id = ?1",
        [operation_id.as_bytes().as_slice()],
        |row| row.get(0),
    )
    .expect("outbox count")
}

fn host_admission_row(path: &Path) -> Option<(Vec<u8>, i64, i64, i64)> {
    let conn = open_raw(path);
    conn.query_row(
        "SELECT operation_id, action_epoch, inspection_id, updated_at_ms
         FROM host_admission WHERE singleton_key = 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )
    .optional()
    .expect("host_admission lookup")
}

#[test]
fn inspect_host_quit_high_water_is_snapshot_consistent() {
    use devmanager::domain::host::HostQuitWorktreeInspection;

    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut bus = CommandBus::open(&path).expect("open bus");
    let task = task_id(0xB1);

    bus.execute(envelope(
        command_id(0xB2),
        None,
        None,
        Command::CreateTask(create_task(task)),
    ))
    .expect("create task");

    let first = bus.inspect_host_quit().expect("first inspect");
    let second = bus.inspect_host_quit().expect("second inspect");
    assert_eq!(first, second, "repeated no-write inspect must be identical");
    assert_eq!(first.worktrees, HostQuitWorktreeInspection::NotInspected);
    assert!(!first.confirmable);
    assert_eq!(
        first.inspection_id,
        u64::try_from(max_event_sequence(&path)).expect("sequence fits u64")
    );

    let events_before = count_table(&path, "events");
    let operations_before = count_table(&path, "operations");
    let outbox_before = count_table(&path, "outbox");
    let _ = bus.inspect_host_quit().expect("third no-write inspect");
    assert_eq!(count_table(&path, "events"), events_before);
    assert_eq!(count_table(&path, "operations"), operations_before);
    assert_eq!(count_table(&path, "outbox"), outbox_before);

    bus.execute(envelope(
        command_id(0xB3),
        Some(task),
        Some(1),
        Command::RenameTask(devmanager::domain::command::RenameTaskIntent {
            title: "Advanced high water".into(),
        }),
    ))
    .expect("rename advances durable events");

    let after = bus.inspect_host_quit().expect("inspect after write");
    assert!(
        after.inspection_id > first.inspection_id,
        "durable event must advance inspection_id: before={} after={}",
        first.inspection_id,
        after.inspection_id
    );
    assert_eq!(
        after.inspection_id,
        u64::try_from(max_event_sequence(&path)).expect("sequence fits u64")
    );
    assert_eq!(after.worktrees, HostQuitWorktreeInspection::NotInspected);
    assert!(!after.confirmable);
}

#[test]
fn confirm_host_quit_requires_current_inspection_and_closes_admission_atomically() {
    use devmanager::domain::host::HostQuitWorktreeInspection;
    use devmanager::domain::operation::OperationState;

    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut bus = CommandBus::open(&path).expect("open bus");
    let task = task_id(0xC1);
    let resource = resource_id(0xC2);

    bus.execute(envelope(
        command_id(0xC3),
        None,
        None,
        Command::CreateTask(create_task(task)),
    ))
    .expect("create task");
    register_active_terminal(&mut bus, task, command_id(0xC4), resource, 1);

    let inspection = bus.inspect_host_quit().expect("inspect before confirm");
    assert_eq!(
        inspection.worktrees,
        HostQuitWorktreeInspection::NotInspected
    );
    assert!(!inspection.confirmable);

    let false_override = bus
        .execute(envelope(
            command_id(0xC5),
            None,
            None,
            Command::ConfirmHostQuit(ConfirmHostQuitIntent {
                inspection_id: inspection.inspection_id,
                allow_uninspected_worktrees: false,
            }),
        ))
        .expect("false override must persist rejection");
    assert!(
        matches!(
            false_override,
            CommandReceipt::Rejected {
                code: RejectionCode::InvalidTransition,
                ..
            }
        ),
        "false override must InvalidTransition, got {false_override:?}"
    );
    assert!(host_admission_row(&path).is_none());
    assert_eq!(host_close_begun_count(&path), 0);

    let stale = bus
        .execute(envelope(
            command_id(0xC6),
            None,
            None,
            Command::ConfirmHostQuit(ConfirmHostQuitIntent {
                inspection_id: inspection.inspection_id.saturating_sub(1),
                allow_uninspected_worktrees: true,
            }),
        ))
        .expect("stale id must persist rejection");
    assert!(
        matches!(
            stale,
            CommandReceipt::Rejected {
                code: RejectionCode::RevisionConflict,
                ..
            }
        ),
        "stale inspection_id must RevisionConflict, got {stale:?}"
    );
    assert!(host_admission_row(&path).is_none());
    assert_eq!(host_close_begun_count(&path), 0);

    let current = bus.inspect_host_quit().expect("fresh inspection");
    let confirm_cmd = command_id(0xC7);
    let accepted = bus
        .execute(envelope(
            confirm_cmd,
            None,
            None,
            Command::ConfirmHostQuit(ConfirmHostQuitIntent {
                inspection_id: current.inspection_id,
                allow_uninspected_worktrees: true,
            }),
        ))
        .expect("confirm");
    let (operation_id, event_ids) = match &accepted {
        CommandReceipt::Accepted {
            operation_id,
            task_revision: None,
            event_ids,
            ..
        } => (*operation_id, event_ids.clone()),
        other => panic!("expected taskless Accepted, got {other:?}"),
    };
    assert_eq!(event_ids.len(), 1);
    assert_eq!(host_close_begun_count(&path), 1);
    assert_eq!(
        operation_accepted_count(&path),
        // CreateTask + RegisterResource each produce OperationAccepted; confirm adds one.
        3
    );
    assert_eq!(count_table(&path, "operations"), 3);
    assert_eq!(outbox_count_for_operation(&path, operation_id), 0);
    let admission = host_admission_row(&path).expect("Closing singleton");
    assert_eq!(admission.0.as_slice(), operation_id.as_bytes().as_slice());
    assert_eq!(admission.1, 1);
    assert_eq!(
        u64::try_from(admission.2).expect("inspection fits"),
        current.inspection_id
    );

    let retry = bus
        .execute(envelope(
            confirm_cmd,
            None,
            None,
            Command::ConfirmHostQuit(ConfirmHostQuitIntent {
                inspection_id: current.inspection_id,
                allow_uninspected_worktrees: true,
            }),
        ))
        .expect("exact CommandId retry");
    assert_eq!(retry, accepted);
    assert_eq!(host_close_begun_count(&path), 1);
    assert_eq!(count_table(&path, "operations"), 3);
    assert_eq!(outbox_count_for_operation(&path, operation_id), 0);

    let pre_close_cmd = command_id(0xC3);
    let pre_close_retry = bus
        .execute(envelope(
            pre_close_cmd,
            None,
            None,
            Command::CreateTask(create_task(task)),
        ))
        .expect("pre-close accepted retry");
    assert!(matches!(pre_close_retry, CommandReceipt::Accepted { .. }));

    let later_create = bus
        .execute(envelope(
            command_id(0xC8),
            None,
            None,
            Command::CreateTask(create_task(task_id(0xC9))),
        ))
        .expect("later create must reject Closing");
    assert!(
        matches!(
            later_create,
            CommandReceipt::Rejected {
                code: RejectionCode::Closing,
                ..
            }
        ),
        "later CreateTask must Closing, got {later_create:?}"
    );

    let later_runtime = bus
        .execute(envelope(
            command_id(0xCA),
            Some(task),
            Some(2),
            Command::ReleaseResource {
                resource_id: resource,
            },
        ))
        .expect("later runtime mutation must reject Closing");
    assert!(
        matches!(
            later_runtime,
            CommandReceipt::Rejected {
                code: RejectionCode::Closing,
                ..
            }
        ),
        "later ReleaseResource must Closing, got {later_runtime:?}"
    );

    let snap = bus
        .task_snapshot(task)
        .expect("snapshot")
        .expect("task present");
    assert_eq!(snap.task.lifecycle, TaskLifecycle::Open);
    assert_eq!(snap.resources.len(), 1);
    assert_eq!(
        snap.resources.get(&resource).map(|r| r.lifecycle),
        Some(devmanager::domain::resource::ResourceLifecycle::Active)
    );
    assert_eq!(
        bus.operation_status(operation_id).expect("quit op status"),
        Some(OperationState::Accepted)
    );
    assert_eq!(outbox_count_for_operation(&path, operation_id), 0);
}

#[test]
fn host_admission_closing_survives_reopen_and_projection_rebuild() {
    use devmanager::domain::operation::OperationState;
    use devmanager::kernel::KernelStore;

    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let quit_op = {
        let mut bus = CommandBus::open(&path).expect("open bus");
        let task = task_id(0xD1);
        bus.execute(envelope(
            command_id(0xD2),
            None,
            None,
            Command::CreateTask(create_task(task)),
        ))
        .expect("create");
        let inspection = bus.inspect_host_quit().expect("inspect");
        let receipt = bus
            .execute(envelope(
                command_id(0xD3),
                None,
                None,
                Command::ConfirmHostQuit(ConfirmHostQuitIntent {
                    inspection_id: inspection.inspection_id,
                    allow_uninspected_worktrees: true,
                }),
            ))
            .expect("confirm");
        match receipt {
            CommandReceipt::Accepted { operation_id, .. } => operation_id,
            other => panic!("expected Accepted, got {other:?}"),
        }
    };

    let before = host_admission_row(&path).expect("Closing before reopen");
    {
        let mut bus = CommandBus::open(&path).expect("reopen bus");
        let rejected = bus
            .execute(envelope(
                command_id(0xD4),
                None,
                None,
                Command::CreateTask(create_task(task_id(0xD5))),
            ))
            .expect("mutation after reopen");
        assert!(matches!(
            rejected,
            CommandReceipt::Rejected {
                code: RejectionCode::Closing,
                ..
            }
        ));
        assert_eq!(
            bus.operation_status(quit_op).expect("status"),
            Some(OperationState::Accepted)
        );
    }

    {
        let mut store = KernelStore::open(&path).expect("store reopen");
        let rebuild = store.rebuild_projections().expect("rebuild");
        assert!(rebuild.events_replayed > 0);
    }
    let after = host_admission_row(&path).expect("Closing after rebuild");
    assert_eq!(
        before, after,
        "rebuild must restore exact Closing singleton"
    );
    {
        let mut bus = CommandBus::open(&path).expect("bus after rebuild");
        let rejected = bus
            .execute(envelope(
                command_id(0xD6),
                None,
                None,
                Command::CreateTask(create_task(task_id(0xD7))),
            ))
            .expect("mutation after rebuild");
        assert!(matches!(
            rejected,
            CommandReceipt::Rejected {
                code: RejectionCode::Closing,
                ..
            }
        ));
    }
}

#[test]
fn host_admission_settlement_is_rejected_and_rolls_back() {
    use devmanager::domain::event::OperationSettledFact;
    use devmanager::domain::id::EventId;
    use devmanager::domain::operation::OutcomeSource;
    use devmanager::kernel::{KernelStore, StoreError};

    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let (quit_op, quit_cmd, begun_event_id, accepted_at_ms) = {
        let mut bus = CommandBus::open(&path).expect("open bus");
        let inspection = bus.inspect_host_quit().expect("inspect");
        let quit_cmd = command_id(0xE1);
        let receipt = bus
            .execute(envelope(
                quit_cmd,
                None,
                None,
                Command::ConfirmHostQuit(ConfirmHostQuitIntent {
                    inspection_id: inspection.inspection_id,
                    allow_uninspected_worktrees: true,
                }),
            ))
            .expect("confirm");
        match receipt {
            CommandReceipt::Accepted {
                operation_id,
                event_ids,
                ..
            } => {
                let conn = open_raw(&path);
                let accepted_at_ms: i64 = conn
                    .query_row(
                        "SELECT accepted_at_ms FROM operations WHERE operation_id = ?1",
                        [operation_id.as_bytes().as_slice()],
                        |row| row.get(0),
                    )
                    .expect("accepted_at");
                (operation_id, quit_cmd, event_ids[0], accepted_at_ms)
            }
            other => panic!("expected Accepted, got {other:?}"),
        }
    };
    let before = host_admission_row(&path).expect("Closing before settle forge");
    let events_before = count_table(&path, "events");
    let ops_before = count_table(&path, "operations");

    {
        let conn = open_raw(&path);
        let settled = OperationSettledFact::with_source(
            quit_cmd,
            quit_op,
            accepted_at_ms + 1,
            vec![begun_event_id],
            Some(1),
            None,
            None,
            OutcomeSource::Dispatch,
        )
        .expect("settled fact");
        conn.execute(
            "INSERT INTO events (
                event_id, task_id, task_revision, event_type, schema_version, occurred_at_ms, payload
             ) VALUES (?1, NULL, NULL, 'operation.settled', ?2, ?3, ?4)",
            rusqlite::params![
                EventId::from_bytes(fixed_uuid_v7(0xE2))
                    .expect("settled event")
                    .as_bytes()
                    .as_slice(),
                i64::from(devmanager::domain::event::EVENT_SCHEMA_VERSION),
                accepted_at_ms + 1,
                rmp_serde::to_vec(&settled).expect("pack settled"),
            ],
        )
        .expect("forge premature host settle");
    }

    let mut store = KernelStore::open(&path).expect("reopen");
    let err = store
        .rebuild_projections()
        .expect_err("HostAdmission settlement must fail closed");
    assert!(
        matches!(&err, StoreError::Projection(detail) if detail.contains("host-admission")),
        "settlement must fail at the HostAdmission lineage gate, got {err:?}"
    );
    drop(store);

    assert_eq!(
        host_admission_row(&path).expect("Closing after failed rebuild"),
        before,
        "failed rebuild must roll back host_admission"
    );
    assert_eq!(count_table(&path, "operations"), ops_before);
    // Forged settle row remains in the durable event log; projection must not settle.
    assert_eq!(count_table(&path, "events"), events_before + 1);
    {
        let conn = open_raw(&path);
        let state: String = conn
            .query_row(
                "SELECT state FROM operations WHERE operation_id = ?1",
                [quit_op.as_bytes().as_slice()],
                |row| row.get(0),
            )
            .expect("op state");
        assert_eq!(state, "accepted");
    }
}

#[test]
fn orphan_host_close_begun_rebuild_is_rejected() {
    use devmanager::domain::event::{HostCloseBegunPayload, EVENT_SCHEMA_VERSION};
    use devmanager::domain::id::{EventId, OperationId};
    use devmanager::kernel::{KernelStore, StoreError};

    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    drop(CommandBus::open(&path).expect("create schema"));
    let op = OperationId::from_bytes(fixed_uuid_v7(0xE3)).expect("op");
    {
        let conn = open_raw(&path);
        let payload = HostCloseBegunPayload {
            operation_id: op,
            action_epoch: 1,
            inspection_id: 0,
        };
        conn.execute(
            "INSERT INTO events (
                event_id, task_id, task_revision, event_type, schema_version, occurred_at_ms, payload
             ) VALUES (?1, NULL, NULL, 'host.close_begun', ?2, ?3, ?4)",
            rusqlite::params![
                EventId::from_bytes(fixed_uuid_v7(0xE4))
                    .expect("eid")
                    .as_bytes()
                    .as_slice(),
                i64::from(EVENT_SCHEMA_VERSION),
                1_725_000_000_500i64,
                rmp_serde::to_vec(&payload).expect("pack"),
            ],
        )
        .expect("orphan HostCloseBegun");
    }
    assert!(host_admission_row(&path).is_none());

    let mut store = KernelStore::open(&path).expect("reopen");
    let err = store
        .rebuild_projections()
        .expect_err("orphan HostCloseBegun must fail rebuild");
    assert!(
        matches!(err, StoreError::Projection(_) | StoreError::Corruption),
        "unexpected orphan rejection: {err:?}"
    );
    drop(store);
    assert!(
        host_admission_row(&path).is_none(),
        "failed rebuild must not leave Closing singleton"
    );
}

#[test]
fn host_admission_duplicate_global_accepted_fact_is_corruption() {
    use devmanager::domain::event::{OperationAcceptedFact, EVENT_SCHEMA_VERSION};
    use devmanager::domain::id::EventId;
    use devmanager::kernel::StoreError;

    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let (quit_op, quit_cmd, accepted_at_ms) = {
        let mut bus = CommandBus::open(&path).expect("open bus");
        let inspection = bus.inspect_host_quit().expect("inspect");
        let quit_cmd = command_id(0xE5);
        let receipt = bus
            .execute(envelope(
                quit_cmd,
                None,
                None,
                Command::ConfirmHostQuit(ConfirmHostQuitIntent {
                    inspection_id: inspection.inspection_id,
                    allow_uninspected_worktrees: true,
                }),
            ))
            .expect("confirm");
        match receipt {
            CommandReceipt::Accepted { operation_id, .. } => {
                let conn = open_raw(&path);
                let accepted_at_ms: i64 = conn
                    .query_row(
                        "SELECT accepted_at_ms FROM operations WHERE operation_id = ?1",
                        [operation_id.as_bytes().as_slice()],
                        |row| row.get(0),
                    )
                    .expect("accepted_at");
                (operation_id, quit_cmd, accepted_at_ms)
            }
            other => panic!("expected Accepted, got {other:?}"),
        }
    };

    {
        let conn = open_raw(&path);
        let alias =
            OperationAcceptedFact::new(quit_cmd, quit_op, accepted_at_ms, Some(1), None, None)
                .expect("alias accepted");
        conn.execute(
            "INSERT INTO events (
                event_id, task_id, task_revision, event_type, schema_version, occurred_at_ms, payload
             ) VALUES (?1, NULL, NULL, 'operation.accepted', ?2, ?3, ?4)",
            rusqlite::params![
                EventId::from_bytes(fixed_uuid_v7(0xE6))
                    .expect("alias eid")
                    .as_bytes()
                    .as_slice(),
                i64::from(EVENT_SCHEMA_VERSION),
                accepted_at_ms,
                rmp_serde::to_vec(&alias).expect("pack alias"),
            ],
        )
        .expect("forge alias OperationAccepted");
    }

    let mut bus = CommandBus::open(&path).expect("reopen");
    let err = bus
        .execute(envelope(
            quit_cmd,
            None,
            None,
            Command::ConfirmHostQuit(ConfirmHostQuitIntent {
                inspection_id: 0,
                allow_uninspected_worktrees: true,
            }),
        ))
        .expect_err("duplicate global accepted fact must corrupt exact retry lookup");
    assert_eq!(err, StoreError::Corruption);
}

fn confirm_host_quit(bus: &mut CommandBus) -> OperationId {
    let inspection = bus.inspect_host_quit().expect("inspect");
    let receipt = bus
        .execute(envelope(
            command_id(0xF0),
            None,
            None,
            Command::ConfirmHostQuit(ConfirmHostQuitIntent {
                inspection_id: inspection.inspection_id,
                allow_uninspected_worktrees: true,
            }),
        ))
        .expect("confirm quit");
    match receipt {
        CommandReceipt::Accepted { operation_id, .. } => operation_id,
        other => panic!("expected Accepted quit, got {other:?}"),
    }
}

fn host_cleanup_branch_rows(path: &Path) -> Vec<(Vec<u8>, String, String, i64, i64)> {
    let conn = open_raw(path);
    let mut stmt = conn
        .prepare(
            "SELECT operation_id, branch, result, remaining_count, completed_at_ms
             FROM host_cleanup_branches
             ORDER BY
               CASE branch
                 WHEN 'agent_sessions' THEN 0
                 WHEN 'resources' THEN 1
                 WHEN 'outstanding_effects' THEN 2
                 WHEN 'task_teardowns' THEN 3
                 ELSE 99
               END",
        )
        .expect("prepare cleanup branches");
    stmt.query_map([], |row| {
        Ok((
            row.get(0)?,
            row.get(1)?,
            row.get(2)?,
            row.get(3)?,
            row.get(4)?,
        ))
    })
    .expect("query cleanup branches")
    .map(|row| row.expect("cleanup branch row"))
    .collect()
}

fn cleanup_event_count(path: &Path) -> i64 {
    let conn = open_raw(path);
    conn.query_row(
        "SELECT COUNT(*) FROM events WHERE event_type = 'host.cleanup_branch_completed'",
        [],
        |row| row.get(0),
    )
    .expect("cleanup event count")
}

fn fabricated_cleanup_mutation_count(path: &Path) -> i64 {
    let conn = open_raw(path);
    conn.query_row(
        "SELECT COUNT(*) FROM events
         WHERE event_type IN (
           'agent_session.registered',
           'resource.release_begun',
           'resource.released',
           'task.close_begun',
           'task.archived',
           'operation.settled',
           'operation.failed',
           'operation.cancelled',
           'operation.uncertain'
         )",
        [],
        |row| row.get(0),
    )
    .expect("fabricated mutation count")
}

fn register_open_agent(bus: &mut CommandBus, task: TaskId, cmd: CommandId, rev: u64) {
    use devmanager::domain::agent::{AgentRole, AgentSessionFacts, AgentSessionLifecycle};
    use devmanager::domain::id::AgentSessionId;

    let receipt = bus
        .execute(envelope(
            cmd,
            Some(task),
            Some(rev),
            Command::RegisterAgentSession {
                agent: AgentSessionFacts {
                    id: AgentSessionId::from_bytes(fixed_uuid_v7(0xA1)).expect("agent id"),
                    task_id: task,
                    role: AgentRole::Primary,
                    provider_kind: "claude".into(),
                    provider_session_id: Some("session-sentinel".into()),
                    lifecycle: AgentSessionLifecycle::Open,
                    runtime_generation: 0,
                    revision: 0,
                },
            },
        ))
        .expect("register agent");
    assert!(matches!(receipt, CommandReceipt::Accepted { .. }));
}

#[test]
fn host_cleanup_advances_one_durable_branch_per_pass_and_resumes_after_reopen() {
    use devmanager::domain::host::{HostCleanupBranch, HostCleanupBranchOutcome};
    use devmanager::domain::operation::OperationState;
    use devmanager::host::{HostCleanupProgress, HostCleanupWorker};
    use devmanager::kernel::KernelStore;

    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut bus = CommandBus::open(&path).expect("open bus");
    let quit_op = confirm_host_quit(&mut bus);
    assert_eq!(
        bus.operation_status(quit_op).expect("status"),
        Some(OperationState::Accepted)
    );

    let expected_order = [
        HostCleanupBranch::AgentSessions,
        HostCleanupBranch::Resources,
        HostCleanupBranch::OutstandingEffects,
        HostCleanupBranch::TaskTeardowns,
    ];
    for (idx, branch) in expected_order.into_iter().enumerate() {
        let progress = HostCleanupWorker::run_once(&mut bus).expect("advance");
        assert_eq!(
            progress,
            HostCleanupProgress::BranchCompleted {
                operation_id: quit_op,
                action_epoch: 1,
                branch,
                outcome: HostCleanupBranchOutcome::succeeded(),
            },
            "pass {idx} must complete {branch:?}"
        );
        let rows = host_cleanup_branch_rows(&path);
        assert_eq!(rows.len(), idx + 1);
        assert_eq!(rows[idx].1, branch.as_str());
        assert_eq!(rows[idx].2, "succeeded");
        assert_eq!(rows[idx].3, 0);
        assert_eq!(cleanup_event_count(&path), (idx + 1) as i64);
        assert_eq!(
            bus.operation_status(quit_op).expect("status"),
            Some(OperationState::Accepted)
        );
        assert!(host_admission_row(&path).is_some());
    }

    assert_eq!(
        HostCleanupWorker::run_once(&mut bus).expect("ready after four successes"),
        HostCleanupProgress::ReadyToExit {
            operation_id: quit_op,
            action_epoch: 1,
        }
    );
    drop(bus);

    {
        let mut bus = CommandBus::open(&path).expect("reopen after crash barrier");
        assert_eq!(
            HostCleanupWorker::run_once(&mut bus).expect("resume ready"),
            HostCleanupProgress::ReadyToExit {
                operation_id: quit_op,
                action_epoch: 1,
            }
        );
    }

    let before = host_cleanup_branch_rows(&path);
    assert_eq!(before.len(), 4);
    {
        let mut store = KernelStore::open(&path).expect("rebuild store");
        let rebuild = store.rebuild_projections().expect("rebuild");
        assert!(rebuild.events_replayed > 0);
    }
    assert_eq!(host_cleanup_branch_rows(&path), before);

    // Crash after first terminal branch resumes at next absent branch.
    let dir2 = TempDir::new().expect("tempdir2");
    let path2 = temp_db_path(&dir2);
    let quit_op2 = {
        let mut bus = CommandBus::open(&path2).expect("open");
        let quit_op = confirm_host_quit(&mut bus);
        assert!(matches!(
            HostCleanupWorker::run_once(&mut bus).expect("first"),
            HostCleanupProgress::BranchCompleted {
                branch: HostCleanupBranch::AgentSessions,
                outcome: HostCleanupBranchOutcome::Succeeded,
                ..
            }
        ));
        quit_op
    };
    let mut bus = CommandBus::open(&path2).expect("reopen mid-journal");
    assert_eq!(
        HostCleanupWorker::run_once(&mut bus).expect("resume resources"),
        HostCleanupProgress::BranchCompleted {
            operation_id: quit_op2,
            action_epoch: 1,
            branch: HostCleanupBranch::Resources,
            outcome: HostCleanupBranchOutcome::succeeded(),
        }
    );
    assert_eq!(host_cleanup_branch_rows(&path2).len(), 2);
}

#[test]
fn host_cleanup_reports_agent_resource_and_effect_residue_without_fabricating_cleanup() {
    use devmanager::domain::host::{HostCleanupBranch, HostCleanupBranchOutcome};
    use devmanager::domain::operation::OperationState;
    use devmanager::domain::task::TaskLifecycle;
    use devmanager::host::{HostCleanupProgress, HostCleanupWorker};

    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut bus = CommandBus::open(&path).expect("open bus");
    let task = task_id(0x91);
    let resource = resource_id(0x92);
    bus.execute(envelope(
        command_id(0x93),
        None,
        None,
        Command::CreateTask(create_task(task)),
    ))
    .expect("create");
    register_open_agent(&mut bus, task, command_id(0x94), 1);
    register_active_terminal(&mut bus, task, command_id(0x95), resource, 2);
    let release_op = {
        let receipt = bus
            .execute(envelope(
                command_id(0x96),
                Some(task),
                Some(3),
                Command::ReleaseResource {
                    resource_id: resource,
                },
            ))
            .expect("begin release");
        match receipt {
            CommandReceipt::Accepted { operation_id, .. } => operation_id,
            other => panic!("expected release accepted, got {other:?}"),
        }
    };
    let close_op = accept_begin_close(&mut bus, task, command_id(0x97), 4);
    let fabricated_before = fabricated_cleanup_mutation_count(&path);
    let quit_op = confirm_host_quit(&mut bus);
    let outbox_before = count_table(&path, "outbox");

    let agent = HostCleanupWorker::run_once(&mut bus).expect("agent branch");
    assert_eq!(
        agent,
        HostCleanupProgress::BranchCompleted {
            operation_id: quit_op,
            action_epoch: 1,
            branch: HostCleanupBranch::AgentSessions,
            outcome: HostCleanupBranchOutcome::failed(1).expect("nonzero"),
        }
    );

    let resources = HostCleanupWorker::run_once(&mut bus).expect("resource branch");
    assert_eq!(
        resources,
        HostCleanupProgress::BranchCompleted {
            operation_id: quit_op,
            action_epoch: 1,
            branch: HostCleanupBranch::Resources,
            outcome: HostCleanupBranchOutcome::failed(1).expect("nonzero"),
        }
    );

    let effects = HostCleanupWorker::run_once(&mut bus).expect("effects branch");
    assert_eq!(
        effects,
        HostCleanupProgress::BranchCompleted {
            operation_id: quit_op,
            action_epoch: 1,
            branch: HostCleanupBranch::OutstandingEffects,
            outcome: HostCleanupBranchOutcome::failed(1).expect("nonzero"),
        }
    );

    let teardowns = HostCleanupWorker::run_once(&mut bus).expect("teardown branch");
    assert_eq!(
        teardowns,
        HostCleanupProgress::BranchCompleted {
            operation_id: quit_op,
            action_epoch: 1,
            branch: HostCleanupBranch::TaskTeardowns,
            outcome: HostCleanupBranchOutcome::failed(1).expect("nonzero"),
        }
    );

    let snap = bus.task_snapshot(task).expect("snapshot").expect("present");
    assert_eq!(snap.task.lifecycle, TaskLifecycle::Closing);
    assert_eq!(snap.agents.len(), 1);
    assert_eq!(
        snap.resources.get(&resource).map(|r| r.lifecycle),
        Some(devmanager::domain::resource::ResourceLifecycle::Releasing)
    );
    assert_eq!(count_table(&path, "outbox"), outbox_before);
    {
        let conn = open_raw(&path);
        assert_eq!(outbox_dispatch_fields(&conn, release_op).0, "pending");
        assert_eq!(outbox_dispatch_fields(&conn, close_op).0, "pending");
    }
    assert_eq!(
        fabricated_cleanup_mutation_count(&path),
        fabricated_before,
        "must not fabricate close/release/archive/host-terminal events"
    );
    assert_eq!(
        bus.operation_status(quit_op).expect("quit"),
        Some(OperationState::Accepted)
    );
    assert!(host_admission_row(&path).is_some());
    assert_eq!(outbox_count_for_operation(&path, quit_op), 0);
}

#[test]
fn host_cleanup_task_branch_reuses_bounded_process_empty_teardown() {
    use devmanager::domain::host::{HostCleanupBranch, HostCleanupBranchOutcome};
    use devmanager::domain::operation::OperationState;
    use devmanager::domain::task::TaskLifecycle;
    use devmanager::host::{HostCleanupProgress, HostCleanupWorker};

    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut bus = CommandBus::open(&path).expect("open bus");
    let task_a = task_id(0xA2);
    let task_b = task_id(0xA5);
    bus.execute(envelope(
        command_id(0xA3),
        None,
        None,
        Command::CreateTask(create_task(task_a)),
    ))
    .expect("create A");
    bus.execute(envelope(
        command_id(0xA6),
        None,
        None,
        Command::CreateTask(create_task(task_b)),
    ))
    .expect("create B");
    let close_a = accept_begin_close(&mut bus, task_a, command_id(0xA4), 1);
    let close_b = accept_begin_close(&mut bus, task_b, command_id(0xA7), 1);
    let quit_op = confirm_host_quit(&mut bus);

    for branch in [
        HostCleanupBranch::AgentSessions,
        HostCleanupBranch::Resources,
        HostCleanupBranch::OutstandingEffects,
    ] {
        assert_eq!(
            HostCleanupWorker::run_once(&mut bus).expect("clean branch"),
            HostCleanupProgress::BranchCompleted {
                operation_id: quit_op,
                action_epoch: 1,
                branch,
                outcome: HostCleanupBranchOutcome::succeeded(),
            }
        );
    }

    assert_eq!(
        HostCleanupWorker::run_once(&mut bus).expect("progress first teardown only"),
        HostCleanupProgress::Progressed {
            task_id: task_a,
            operation_id: close_a,
        }
    );
    assert_eq!(host_cleanup_branch_rows(&path).len(), 3);
    assert_eq!(
        bus.task_snapshot(task_a)
            .expect("A snap")
            .expect("A present")
            .task
            .lifecycle,
        TaskLifecycle::Archived
    );
    assert_eq!(
        bus.task_snapshot(task_b)
            .expect("B snap")
            .expect("B present")
            .task
            .lifecycle,
        TaskLifecycle::Closing
    );
    assert!(matches!(
        bus.operation_status(close_a).expect("A settled"),
        Some(OperationState::Settled { .. })
    ));
    assert_eq!(
        bus.operation_status(close_b).expect("B still accepted"),
        Some(OperationState::Accepted)
    );

    assert_eq!(
        HostCleanupWorker::run_once(&mut bus).expect("progress second teardown"),
        HostCleanupProgress::Progressed {
            task_id: task_b,
            operation_id: close_b,
        }
    );
    assert_eq!(host_cleanup_branch_rows(&path).len(), 3);
    assert_eq!(
        bus.task_snapshot(task_b)
            .expect("B after")
            .expect("B present")
            .task
            .lifecycle,
        TaskLifecycle::Archived
    );

    assert_eq!(
        HostCleanupWorker::run_once(&mut bus).expect("complete teardown branch"),
        HostCleanupProgress::BranchCompleted {
            operation_id: quit_op,
            action_epoch: 1,
            branch: HostCleanupBranch::TaskTeardowns,
            outcome: HostCleanupBranchOutcome::succeeded(),
        }
    );
    assert_eq!(
        HostCleanupWorker::run_once(&mut bus).expect("ready after teardown branch"),
        HostCleanupProgress::ReadyToExit {
            operation_id: quit_op,
            action_epoch: 1,
        }
    );
    assert_eq!(
        bus.operation_status(quit_op).expect("quit still accepted"),
        Some(OperationState::Accepted)
    );
}

#[test]
fn host_cleanup_projection_only_forged_row_is_corruption_and_appends_nothing() {
    use devmanager::domain::host::HostCleanupBranch;
    use devmanager::host::HostCleanupWorker;
    use devmanager::kernel::StoreError;

    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let quit_op = {
        let mut bus = CommandBus::open(&path).expect("open");
        confirm_host_quit(&mut bus)
    };
    let events_before = count_table(&path, "events");
    let admission_before = host_admission_row(&path).expect("closing");
    {
        let conn = open_raw(&path);
        let inserted = conn
            .execute(
                "INSERT INTO host_cleanup_branches (
                    operation_id, branch, result, remaining_count, completed_at_ms
                 ) VALUES (?1, ?2, 'succeeded', 0, 1)",
                rusqlite::params![
                    quit_op.as_bytes().as_slice(),
                    HostCleanupBranch::AgentSessions.as_str(),
                ],
            )
            .expect("forge projection-only row");
        assert_eq!(inserted, 1);
    }
    assert_eq!(host_cleanup_branch_rows(&path).len(), 1);
    assert_eq!(cleanup_event_count(&path), 0);

    let mut bus = CommandBus::open(&path).expect("reopen");
    let err =
        HostCleanupWorker::run_once(&mut bus).expect_err("projection-only forge must fail closed");
    assert_eq!(err, StoreError::Corruption);
    assert_eq!(count_table(&path, "events"), events_before);
    assert_eq!(cleanup_event_count(&path), 0);
    assert_eq!(host_cleanup_branch_rows(&path).len(), 1);
    assert_eq!(
        host_admission_row(&path).expect("still closing"),
        admission_before
    );
}

#[test]
fn host_cleanup_out_of_order_durable_event_rebuild_fails_and_preserves_projections() {
    use devmanager::domain::event::{HostCleanupBranchCompletedPayload, EVENT_SCHEMA_VERSION};
    use devmanager::domain::host::{HostCleanupBranch, HostCleanupBranchOutcome};
    use devmanager::domain::id::EventId;
    use devmanager::kernel::{KernelStore, StoreError};

    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let quit_op = {
        let mut bus = CommandBus::open(&path).expect("open");
        confirm_host_quit(&mut bus)
    };
    let before_rows = host_cleanup_branch_rows(&path);
    let admission_before = host_admission_row(&path).expect("closing");
    let events_before = count_table(&path, "events");
    {
        let conn = open_raw(&path);
        let payload = HostCleanupBranchCompletedPayload {
            operation_id: quit_op,
            action_epoch: 1,
            branch: HostCleanupBranch::Resources,
            outcome: HostCleanupBranchOutcome::succeeded(),
        };
        conn.execute(
            "INSERT INTO events (
                event_id, task_id, task_revision, event_type, schema_version, occurred_at_ms, payload
             ) VALUES (?1, NULL, NULL, 'host.cleanup_branch_completed', ?2, ?3, ?4)",
            rusqlite::params![
                EventId::from_bytes(fixed_uuid_v7(0xC0))
                    .expect("eid")
                    .as_bytes()
                    .as_slice(),
                i64::from(EVENT_SCHEMA_VERSION),
                1_725_000_000_950i64,
                rmp_serde::to_vec(&payload).expect("pack"),
            ],
        )
        .expect("forge out-of-order Resources without AgentSessions");
    }

    let mut store = KernelStore::open(&path).expect("reopen");
    let err = store
        .rebuild_projections()
        .expect_err("out-of-order branch must fail rebuild");
    assert!(
        matches!(
            &err,
            StoreError::Projection(detail) if detail.contains("prefix") || detail.contains("order")
        ),
        "out-of-order must fail at projection prefix gate, got {err:?}"
    );
    drop(store);
    assert_eq!(host_cleanup_branch_rows(&path), before_rows);
    assert_eq!(
        host_admission_row(&path).expect("still closing"),
        admission_before
    );
    assert_eq!(count_table(&path, "events"), events_before + 1);
}

#[test]
fn host_cleanup_corrupt_pending_non_teardown_effect_is_corruption_not_failed_outcome() {
    use devmanager::domain::host::HostCleanupBranch;
    use devmanager::host::{HostCleanupProgress, HostCleanupWorker};
    use devmanager::kernel::StoreError;

    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut bus = CommandBus::open(&path).expect("open bus");
    let task = task_id(0xD1);
    let resource = resource_id(0xD2);
    bus.execute(envelope(
        command_id(0xD3),
        None,
        None,
        Command::CreateTask(create_task(task)),
    ))
    .expect("create");
    register_active_terminal(&mut bus, task, command_id(0xD4), resource, 1);
    let release_op = {
        let receipt = bus
            .execute(envelope(
                command_id(0xD5),
                Some(task),
                Some(2),
                Command::ReleaseResource {
                    resource_id: resource,
                },
            ))
            .expect("begin release");
        match receipt {
            CommandReceipt::Accepted { operation_id, .. } => operation_id,
            other => panic!("expected release accepted, got {other:?}"),
        }
    };
    let quit_op = confirm_host_quit(&mut bus);
    drop(bus);

    {
        let conn = open_raw(&path);
        let changed = conn
            .execute(
                "UPDATE outbox SET payload = X'DEADBEEF' WHERE operation_id = ?1",
                [release_op.as_bytes().as_slice()],
            )
            .expect("corrupt pending non-teardown payload");
        assert_eq!(changed, 1);
    }

    let mut bus = CommandBus::open(&path).expect("reopen");
    assert!(matches!(
        HostCleanupWorker::run_once(&mut bus).expect("agents"),
        HostCleanupProgress::BranchCompleted {
            branch: HostCleanupBranch::AgentSessions,
            ..
        }
    ));
    assert!(matches!(
        HostCleanupWorker::run_once(&mut bus).expect("resources"),
        HostCleanupProgress::BranchCompleted {
            branch: HostCleanupBranch::Resources,
            ..
        }
    ));
    let events_before = count_table(&path, "events");
    let branches_before = host_cleanup_branch_rows(&path).len();
    let err = HostCleanupWorker::run_once(&mut bus)
        .expect_err("corrupt pending non-teardown must fail closed");
    assert_eq!(err, StoreError::Corruption);
    assert_eq!(count_table(&path, "events"), events_before);
    assert_eq!(host_cleanup_branch_rows(&path).len(), branches_before);
    assert_eq!(
        bus.operation_status(quit_op).expect("quit"),
        Some(devmanager::domain::operation::OperationState::Accepted)
    );
    assert!(!host_cleanup_branch_rows(&path)
        .iter()
        .any(|row| row.1 == HostCleanupBranch::OutstandingEffects.as_str()));
}

#[test]
fn host_cleanup_wrong_event_sequence_pending_effect_is_corruption_not_residue() {
    use devmanager::domain::host::HostCleanupBranch;
    use devmanager::host::{HostCleanupProgress, HostCleanupWorker};
    use devmanager::kernel::StoreError;

    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut bus = CommandBus::open(&path).expect("open bus");
    let task = task_id(0xE1);
    let resource = resource_id(0xE2);
    bus.execute(envelope(
        command_id(0xE3),
        None,
        None,
        Command::CreateTask(create_task(task)),
    ))
    .expect("create");
    register_active_terminal(&mut bus, task, command_id(0xE4), resource, 1);
    let release_op = {
        let receipt = bus
            .execute(envelope(
                command_id(0xE5),
                Some(task),
                Some(2),
                Command::ReleaseResource {
                    resource_id: resource,
                },
            ))
            .expect("begin release");
        match receipt {
            CommandReceipt::Accepted { operation_id, .. } => operation_id,
            other => panic!("expected release accepted, got {other:?}"),
        }
    };
    let quit_op = confirm_host_quit(&mut bus);
    drop(bus);

    {
        let conn = open_raw(&path);
        let correct: i64 = conn
            .query_row(
                "SELECT event_sequence FROM outbox WHERE operation_id = ?1",
                [release_op.as_bytes().as_slice()],
                |row| row.get(0),
            )
            .expect("read event_sequence");
        let wrong: i64 = conn
            .query_row(
                "SELECT sequence FROM events WHERE sequence != ?1 ORDER BY sequence ASC LIMIT 1",
                [correct],
                |row| row.get(0),
            )
            .expect("pick existing but incorrect event_sequence");
        assert_ne!(wrong, correct);
        let changed = conn
            .execute(
                "UPDATE outbox SET event_sequence = ?1 WHERE operation_id = ?2",
                rusqlite::params![wrong, release_op.as_bytes().as_slice()],
            )
            .expect("forge wrong event_sequence on structurally valid pending effect");
        assert_eq!(changed, 1);
    }

    let mut bus = CommandBus::open(&path).expect("reopen");
    assert!(matches!(
        HostCleanupWorker::run_once(&mut bus).expect("agents"),
        HostCleanupProgress::BranchCompleted {
            branch: HostCleanupBranch::AgentSessions,
            ..
        }
    ));
    assert!(matches!(
        HostCleanupWorker::run_once(&mut bus).expect("resources"),
        HostCleanupProgress::BranchCompleted {
            branch: HostCleanupBranch::Resources,
            ..
        }
    ));
    let events_before = count_table(&path, "events");
    let branches_before = host_cleanup_branch_rows(&path).len();
    let err = HostCleanupWorker::run_once(&mut bus)
        .expect_err("wrong event_sequence must fail closed as Corruption");
    assert_eq!(err, StoreError::Corruption);
    assert_eq!(count_table(&path, "events"), events_before);
    assert_eq!(host_cleanup_branch_rows(&path).len(), branches_before);
    assert_eq!(
        bus.operation_status(quit_op).expect("quit"),
        Some(devmanager::domain::operation::OperationState::Accepted)
    );
    assert!(!host_cleanup_branch_rows(&path)
        .iter()
        .any(|row| row.1 == HostCleanupBranch::OutstandingEffects.as_str()));
}

#[test]
fn host_cleanup_task_teardown_revalidates_lineage_after_outstanding_effects_crash_boundary() {
    use devmanager::domain::host::{HostCleanupBranch, HostCleanupBranchOutcome};
    use devmanager::host::{HostCleanupProgress, HostCleanupWorker};
    use devmanager::kernel::StoreError;

    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut bus = CommandBus::open(&path).expect("open bus");
    let task = task_id(0xF1);
    let resource = resource_id(0xF2);
    bus.execute(envelope(
        command_id(0xF3),
        None,
        None,
        Command::CreateTask(create_task(task)),
    ))
    .expect("create");
    register_active_terminal(&mut bus, task, command_id(0xF4), resource, 1);
    let close_op = accept_begin_close(&mut bus, task, command_id(0xF5), 2);
    let quit_op = confirm_host_quit(&mut bus);

    assert!(matches!(
        HostCleanupWorker::run_once(&mut bus).expect("agents"),
        HostCleanupProgress::BranchCompleted {
            branch: HostCleanupBranch::AgentSessions,
            outcome: HostCleanupBranchOutcome::Succeeded,
            ..
        }
    ));
    assert_eq!(
        HostCleanupWorker::run_once(&mut bus).expect("resources"),
        HostCleanupProgress::BranchCompleted {
            operation_id: quit_op,
            action_epoch: 1,
            branch: HostCleanupBranch::Resources,
            outcome: HostCleanupBranchOutcome::failed(1).expect("one live resource"),
        }
    );
    assert!(matches!(
        HostCleanupWorker::run_once(&mut bus).expect("outstanding effects"),
        HostCleanupProgress::BranchCompleted {
            branch: HostCleanupBranch::OutstandingEffects,
            outcome: HostCleanupBranchOutcome::Succeeded,
            ..
        }
    ));
    drop(bus);

    // Simulate corruption after the durable OutstandingEffects crash boundary.
    // The active resource keeps this teardown out of the process-empty candidate query.
    {
        let conn = open_raw(&path);
        let correct: i64 = conn
            .query_row(
                "SELECT event_sequence FROM outbox WHERE operation_id = ?1",
                [close_op.as_bytes().as_slice()],
                |row| row.get(0),
            )
            .expect("read teardown event_sequence");
        let wrong: i64 = conn
            .query_row(
                "SELECT sequence FROM events WHERE sequence != ?1 ORDER BY sequence ASC LIMIT 1",
                [correct],
                |row| row.get(0),
            )
            .expect("pick existing but incorrect event_sequence");
        assert_ne!(wrong, correct);
        assert_eq!(
            conn.execute(
                "UPDATE outbox SET event_sequence = ?1 WHERE operation_id = ?2",
                rusqlite::params![wrong, close_op.as_bytes().as_slice()],
            )
            .expect("forge blocked teardown lineage"),
            1
        );
    }

    let events_before = count_table(&path, "events");
    let branches_before = host_cleanup_branch_rows(&path);
    assert_eq!(branches_before.len(), 3);
    let mut bus = CommandBus::open(&path).expect("reopen after crash boundary");
    let err = HostCleanupWorker::run_once(&mut bus)
        .expect_err("blocked teardown lineage corruption must fail closed");
    assert_eq!(err, StoreError::Corruption);
    assert_eq!(count_table(&path, "events"), events_before);
    assert_eq!(host_cleanup_branch_rows(&path), branches_before);
    assert!(!host_cleanup_branch_rows(&path)
        .iter()
        .any(|row| row.1 == HostCleanupBranch::TaskTeardowns.as_str()));
}

#[test]
fn host_cleanup_branch_wrong_operation_epoch_or_duplicate_is_corruption_and_rolls_back() {
    use devmanager::domain::event::{HostCleanupBranchCompletedPayload, EVENT_SCHEMA_VERSION};
    use devmanager::domain::host::{HostCleanupBranch, HostCleanupBranchOutcome};
    use devmanager::domain::id::EventId;
    use devmanager::host::{HostCleanupProgress, HostCleanupWorker};
    use devmanager::kernel::{KernelStore, StoreError};

    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let quit_op = {
        let mut bus = CommandBus::open(&path).expect("open");
        let quit_op = confirm_host_quit(&mut bus);
        assert!(matches!(
            HostCleanupWorker::run_once(&mut bus).expect("agent"),
            HostCleanupProgress::BranchCompleted {
                branch: HostCleanupBranch::AgentSessions,
                ..
            }
        ));
        quit_op
    };
    let before_rows = host_cleanup_branch_rows(&path);
    let events_before = count_table(&path, "events");
    let admission_before = host_admission_row(&path).expect("closing");

    {
        let conn = open_raw(&path);
        let payload = HostCleanupBranchCompletedPayload {
            operation_id: quit_op,
            action_epoch: 99,
            branch: HostCleanupBranch::Resources,
            outcome: HostCleanupBranchOutcome::succeeded(),
        };
        conn.execute(
            "INSERT INTO events (
                event_id, task_id, task_revision, event_type, schema_version, occurred_at_ms, payload
             ) VALUES (?1, NULL, NULL, 'host.cleanup_branch_completed', ?2, ?3, ?4)",
            rusqlite::params![
                EventId::from_bytes(fixed_uuid_v7(0xB0))
                    .expect("eid")
                    .as_bytes()
                    .as_slice(),
                i64::from(EVENT_SCHEMA_VERSION),
                1_725_000_000_900i64,
                rmp_serde::to_vec(&payload).expect("pack"),
            ],
        )
        .expect("forge wrong-epoch cleanup fact");
    }

    let mut store = KernelStore::open(&path).expect("reopen");
    let err = store
        .rebuild_projections()
        .expect_err("wrong epoch must fail closed");
    assert!(
        matches!(&err, StoreError::Projection(detail) if detail.contains("action_epoch")),
        "wrong epoch must fail at projection gate, got {err:?}"
    );
    drop(store);

    assert_eq!(host_cleanup_branch_rows(&path), before_rows);
    assert_eq!(
        host_admission_row(&path).expect("still closing"),
        admission_before
    );
    assert_eq!(count_table(&path, "events"), events_before + 1);

    // Remove the wrong-epoch forged fact so the duplicate case is isolated.
    {
        let conn = open_raw(&path);
        let deleted = conn
            .execute(
                "DELETE FROM events WHERE event_type = 'host.cleanup_branch_completed'
                 AND sequence = (SELECT MAX(sequence) FROM events)",
                [],
            )
            .expect("remove wrong-epoch forge");
        assert_eq!(deleted, 1);
    }
    assert_eq!(count_table(&path, "events"), events_before);

    {
        let conn = open_raw(&path);
        let payload = HostCleanupBranchCompletedPayload {
            operation_id: quit_op,
            action_epoch: 1,
            branch: HostCleanupBranch::AgentSessions,
            outcome: HostCleanupBranchOutcome::succeeded(),
        };
        conn.execute(
            "INSERT INTO events (
                event_id, task_id, task_revision, event_type, schema_version, occurred_at_ms, payload
             ) VALUES (?1, NULL, NULL, 'host.cleanup_branch_completed', ?2, ?3, ?4)",
            rusqlite::params![
                EventId::from_bytes(fixed_uuid_v7(0xB1))
                    .expect("dup eid")
                    .as_bytes()
                    .as_slice(),
                i64::from(EVENT_SCHEMA_VERSION),
                1_725_000_000_901i64,
                rmp_serde::to_vec(&payload).expect("pack dup"),
            ],
        )
        .expect("forge duplicate agent branch");
    }
    let mut store = KernelStore::open(&path).expect("reopen dup");
    let err = store
        .rebuild_projections()
        .expect_err("duplicate branch must fail closed");
    assert!(
        matches!(&err, StoreError::Projection(detail) if detail.contains("duplicate")),
        "duplicate must fail at projection gate, got {err:?}"
    );
    assert_eq!(host_cleanup_branch_rows(&path), before_rows);
}

fn drive_four_cleanup_branches(bus: &mut CommandBus, quit_op: OperationId) {
    use devmanager::domain::host::HostCleanupBranch;
    use devmanager::host::{HostCleanupProgress, HostCleanupWorker};

    for branch in HostCleanupBranch::ORDER {
        let progress = HostCleanupWorker::run_once(bus).expect("branch");
        match progress {
            HostCleanupProgress::BranchCompleted {
                operation_id,
                action_epoch,
                branch: got,
                ..
            } => {
                assert_eq!(operation_id, quit_op);
                assert_eq!(action_epoch, 1);
                assert_eq!(got, branch);
            }
            other => panic!("expected BranchCompleted for {branch:?}, got {other:?}"),
        }
    }
}

fn operation_failed_event_count(path: &Path) -> i64 {
    let conn = open_raw(path);
    conn.query_row(
        "SELECT COUNT(*) FROM events WHERE event_type = 'operation.failed'",
        [],
        |row| row.get(0),
    )
    .expect("count failed")
}

fn quit_command_id() -> CommandId {
    command_id(0xF0)
}

#[test]
fn host_cleanup_failed_journal_terminalizes_once_as_cleanup_failed() {
    use devmanager::domain::operation::{OperationErrorCode, OperationState};
    use devmanager::host::{HostCleanupProgress, HostCleanupWorker};

    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut bus = CommandBus::open(&path).expect("open");
    let task = task_id(0x11);
    bus.execute(envelope(
        command_id(0x12),
        None,
        None,
        Command::CreateTask(create_task(task)),
    ))
    .expect("create");
    register_open_agent(&mut bus, task, command_id(0x13), 1);
    let quit_op = confirm_host_quit(&mut bus);
    let outbox_before = count_table(&path, "outbox");
    let events_before_terminal = {
        drive_four_cleanup_branches(&mut bus, quit_op);
        assert!(host_cleanup_branch_rows(&path)
            .iter()
            .any(|row| row.2 == "failed"));
        assert_eq!(
            bus.operation_status(quit_op).expect("pre-terminal"),
            Some(OperationState::Accepted)
        );
        count_table(&path, "events")
    };

    let failed = HostCleanupWorker::run_once(&mut bus).expect("terminalize");
    let HostCleanupProgress::Failed {
        operation_id,
        action_epoch,
        settled_at_ms,
    } = failed
    else {
        panic!("expected Failed progress, got {failed:?}");
    };
    assert_eq!(operation_id, quit_op);
    assert_eq!(action_epoch, 1);
    assert!(settled_at_ms > 0);
    assert_eq!(count_table(&path, "events"), events_before_terminal + 1);
    assert_eq!(operation_failed_event_count(&path), 1);
    assert_eq!(
        bus.operation_status(quit_op).expect("failed status"),
        Some(OperationState::Failed {
            settled_at_ms,
            code: OperationErrorCode::CleanupFailed,
        })
    );
    assert!(
        host_admission_row(&path).is_some(),
        "admission stays Closing"
    );
    assert_eq!(count_table(&path, "outbox"), outbox_before);
    assert_eq!(outbox_count_for_operation(&path, quit_op), 0);

    assert_eq!(
        HostCleanupWorker::run_once(&mut bus).expect("idempotent"),
        HostCleanupProgress::Idle
    );
    assert_eq!(operation_failed_event_count(&path), 1);
    assert_eq!(
        bus.operation_status(quit_op).expect("still failed"),
        Some(OperationState::Failed {
            settled_at_ms,
            code: OperationErrorCode::CleanupFailed,
        })
    );
}

#[test]
fn host_cleanup_all_success_journal_reports_ready_to_exit_without_settling() {
    use devmanager::domain::operation::OperationState;
    use devmanager::host::{HostCleanupProgress, HostCleanupWorker};

    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut bus = CommandBus::open(&path).expect("open");
    let quit_op = confirm_host_quit(&mut bus);
    let events_after_journal = {
        drive_four_cleanup_branches(&mut bus, quit_op);
        count_table(&path, "events")
    };

    assert_eq!(
        HostCleanupWorker::run_once(&mut bus).expect("ready"),
        HostCleanupProgress::ReadyToExit {
            operation_id: quit_op,
            action_epoch: 1,
        }
    );
    assert_eq!(count_table(&path, "events"), events_after_journal);
    assert_eq!(operation_failed_event_count(&path), 0);
    assert_eq!(
        bus.operation_status(quit_op).expect("accepted"),
        Some(OperationState::Accepted)
    );
    assert!(host_admission_row(&path).is_some());

    assert_eq!(
        HostCleanupWorker::run_once(&mut bus).expect("ready stable"),
        HostCleanupProgress::ReadyToExit {
            operation_id: quit_op,
            action_epoch: 1,
        }
    );
    assert_eq!(count_table(&path, "events"), events_after_journal);
}

#[test]
fn host_cleanup_failed_terminal_resumes_once_across_reopen() {
    use devmanager::domain::operation::{OperationErrorCode, OperationState};
    use devmanager::host::{HostCleanupProgress, HostCleanupWorker};

    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let quit_op = {
        let mut bus = CommandBus::open(&path).expect("open");
        let task = task_id(0x21);
        bus.execute(envelope(
            command_id(0x22),
            None,
            None,
            Command::CreateTask(create_task(task)),
        ))
        .expect("create");
        register_open_agent(&mut bus, task, command_id(0x23), 1);
        let quit_op = confirm_host_quit(&mut bus);
        drive_four_cleanup_branches(&mut bus, quit_op);
        quit_op
    };
    assert_eq!(operation_failed_event_count(&path), 0);
    assert_eq!(
        {
            let bus = CommandBus::open(&path).expect("status");
            bus.operation_status(quit_op).expect("status")
        },
        Some(OperationState::Accepted)
    );

    let settled_at_ms = {
        let mut bus = CommandBus::open(&path).expect("reopen terminal pass");
        let failed = HostCleanupWorker::run_once(&mut bus).expect("terminalize after crash");
        let HostCleanupProgress::Failed {
            operation_id,
            settled_at_ms,
            ..
        } = failed
        else {
            panic!("expected Failed, got {failed:?}");
        };
        assert_eq!(operation_id, quit_op);
        settled_at_ms
    };
    assert_eq!(operation_failed_event_count(&path), 1);

    {
        let mut bus = CommandBus::open(&path).expect("reopen after terminal");
        assert_eq!(
            HostCleanupWorker::run_once(&mut bus).expect("idle"),
            HostCleanupProgress::Idle
        );
        assert_eq!(
            bus.operation_status(quit_op).expect("failed"),
            Some(OperationState::Failed {
                settled_at_ms,
                code: OperationErrorCode::CleanupFailed,
            })
        );
    }
    assert_eq!(operation_failed_event_count(&path), 1);
}

#[test]
fn host_cleanup_premature_or_wrong_failure_is_rejected_at_runtime_and_rebuild() {
    use devmanager::domain::event::{OperationFailedFact, EVENT_SCHEMA_VERSION};
    use devmanager::domain::id::EventId;
    use devmanager::domain::operation::{OperationErrorCode, OutcomeSource};
    use devmanager::kernel::{KernelStore, StoreError};

    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let (quit_op, accepted_at_ms) = {
        let mut bus = CommandBus::open(&path).expect("open");
        let quit_op = confirm_host_quit(&mut bus);
        let accepted_at_ms: i64 = {
            let conn = open_raw(&path);
            conn.query_row(
                "SELECT accepted_at_ms FROM operations WHERE operation_id = ?1",
                [quit_op.as_bytes().as_slice()],
                |row| row.get(0),
            )
            .expect("accepted_at")
        };
        (quit_op, accepted_at_ms)
    };
    let admission_before = host_admission_row(&path).expect("closing");
    let ops_state_before = {
        let conn = open_raw(&path);
        let state: String = conn
            .query_row(
                "SELECT state FROM operations WHERE operation_id = ?1",
                [quit_op.as_bytes().as_slice()],
                |row| row.get(0),
            )
            .expect("state");
        state
    };

    {
        let conn = open_raw(&path);
        let failed = OperationFailedFact::with_source(
            quit_command_id(),
            quit_op,
            accepted_at_ms + 1,
            OperationErrorCode::CleanupFailed,
            Some(1),
            None,
            None,
            OutcomeSource::Dispatch,
        )
        .expect("fact");
        conn.execute(
            "INSERT INTO events (
                event_id, task_id, task_revision, event_type, schema_version, occurred_at_ms, payload
             ) VALUES (?1, NULL, NULL, 'operation.failed', ?2, ?3, ?4)",
            rusqlite::params![
                EventId::from_bytes(fixed_uuid_v7(0x31))
                    .expect("eid")
                    .as_bytes()
                    .as_slice(),
                i64::from(EVENT_SCHEMA_VERSION),
                accepted_at_ms + 1,
                rmp_serde::to_vec(&failed).expect("pack"),
            ],
        )
        .expect("forge premature failure");
    }
    {
        let mut store = KernelStore::open(&path).expect("reopen");
        let err = store
            .rebuild_projections()
            .expect_err("premature failure must fail closed");
        assert!(
            matches!(
                &err,
                StoreError::Projection(detail)
                    if detail.contains("host-admission")
                        || detail.contains("cleanup")
                        || detail.contains("failed")
            ),
            "premature host-admission failure must fail at projection gate, got {err:?}"
        );
    }
    assert_eq!(
        host_admission_row(&path).expect("still closing"),
        admission_before
    );
    {
        let conn = open_raw(&path);
        let state: String = conn
            .query_row(
                "SELECT state FROM operations WHERE operation_id = ?1",
                [quit_op.as_bytes().as_slice()],
                |row| row.get(0),
            )
            .expect("state");
        assert_eq!(state, ops_state_before);
    }

    // Success journal must not accept a following CleanupFailed fact.
    let dir2 = TempDir::new().expect("tempdir2");
    let path2 = temp_db_path(&dir2);
    let (quit_op2, accepted_at_ms2) = {
        let mut bus = CommandBus::open(&path2).expect("open");
        let quit_op = confirm_host_quit(&mut bus);
        drive_four_cleanup_branches(&mut bus, quit_op);
        assert!(host_cleanup_branch_rows(&path2)
            .iter()
            .all(|row| row.2 == "succeeded"));
        let accepted_at_ms: i64 = {
            let conn = open_raw(&path2);
            conn.query_row(
                "SELECT accepted_at_ms FROM operations WHERE operation_id = ?1",
                [quit_op.as_bytes().as_slice()],
                |row| row.get(0),
            )
            .expect("accepted_at")
        };
        (quit_op, accepted_at_ms)
    };
    let before_rows = host_cleanup_branch_rows(&path2);
    {
        let conn = open_raw(&path2);
        let failed = OperationFailedFact::with_source(
            quit_command_id(),
            quit_op2,
            accepted_at_ms2 + 50,
            OperationErrorCode::CleanupFailed,
            Some(1),
            None,
            None,
            OutcomeSource::Dispatch,
        )
        .expect("fact");
        conn.execute(
            "INSERT INTO events (
                event_id, task_id, task_revision, event_type, schema_version, occurred_at_ms, payload
             ) VALUES (?1, NULL, NULL, 'operation.failed', ?2, ?3, ?4)",
            rusqlite::params![
                EventId::from_bytes(fixed_uuid_v7(0x32))
                    .expect("eid")
                    .as_bytes()
                    .as_slice(),
                i64::from(EVENT_SCHEMA_VERSION),
                accepted_at_ms2 + 50,
                rmp_serde::to_vec(&failed).expect("pack"),
            ],
        )
        .expect("forge success-journal failure");
    }
    {
        let mut store = KernelStore::open(&path2).expect("reopen");
        let err = store
            .rebuild_projections()
            .expect_err("success journal failure must fail closed");
        assert!(
            matches!(&err, StoreError::Projection(_)),
            "success-journal CleanupFailed must fail rebuild, got {err:?}"
        );
    }
    assert_eq!(host_cleanup_branch_rows(&path2), before_rows);

    // Wrong error code on an otherwise complete failed journal.
    let dir3 = TempDir::new().expect("tempdir3");
    let path3 = temp_db_path(&dir3);
    let (quit_op3, accepted_at_ms3) = {
        let mut bus = CommandBus::open(&path3).expect("open");
        let task = task_id(0x33);
        bus.execute(envelope(
            command_id(0x34),
            None,
            None,
            Command::CreateTask(create_task(task)),
        ))
        .expect("create");
        register_open_agent(&mut bus, task, command_id(0x35), 1);
        let quit_op = confirm_host_quit(&mut bus);
        drive_four_cleanup_branches(&mut bus, quit_op);
        let accepted_at_ms: i64 = {
            let conn = open_raw(&path3);
            conn.query_row(
                "SELECT accepted_at_ms FROM operations WHERE operation_id = ?1",
                [quit_op.as_bytes().as_slice()],
                |row| row.get(0),
            )
            .expect("accepted_at")
        };
        (quit_op, accepted_at_ms)
    };
    let before_rows3 = host_cleanup_branch_rows(&path3);
    {
        let conn = open_raw(&path3);
        let failed = OperationFailedFact::with_source(
            quit_command_id(),
            quit_op3,
            accepted_at_ms3 + 50,
            OperationErrorCode::SideEffectFailed,
            Some(1),
            None,
            None,
            OutcomeSource::Dispatch,
        )
        .expect("fact");
        conn.execute(
            "INSERT INTO events (
                event_id, task_id, task_revision, event_type, schema_version, occurred_at_ms, payload
             ) VALUES (?1, NULL, NULL, 'operation.failed', ?2, ?3, ?4)",
            rusqlite::params![
                EventId::from_bytes(fixed_uuid_v7(0x36))
                    .expect("eid")
                    .as_bytes()
                    .as_slice(),
                i64::from(EVENT_SCHEMA_VERSION),
                accepted_at_ms3 + 50,
                rmp_serde::to_vec(&failed).expect("pack"),
            ],
        )
        .expect("forge wrong code");
    }
    {
        let mut store = KernelStore::open(&path3).expect("reopen");
        let err = store
            .rebuild_projections()
            .expect_err("wrong code must fail closed");
        assert!(
            matches!(&err, StoreError::Projection(_)),
            "wrong host-admission failure code must fail rebuild, got {err:?}"
        );
    }
    assert_eq!(host_cleanup_branch_rows(&path3), before_rows3);
}

#[test]
fn host_cleanup_event_only_forged_failure_is_runtime_corruption_while_accepted() {
    use devmanager::domain::event::{OperationFailedFact, EVENT_SCHEMA_VERSION};
    use devmanager::domain::id::EventId;
    use devmanager::domain::operation::{OperationErrorCode, OutcomeSource};
    use devmanager::kernel::StoreError;

    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let (quit_op, accepted_at_ms) = {
        let mut bus = CommandBus::open(&path).expect("open");
        let quit_op = confirm_host_quit(&mut bus);
        let accepted_at_ms: i64 = {
            let conn = open_raw(&path);
            conn.query_row(
                "SELECT accepted_at_ms FROM operations WHERE operation_id = ?1",
                [quit_op.as_bytes().as_slice()],
                |row| row.get(0),
            )
            .expect("accepted_at")
        };
        (quit_op, accepted_at_ms)
    };
    {
        let conn = open_raw(&path);
        let failed = OperationFailedFact::with_source(
            quit_command_id(),
            quit_op,
            accepted_at_ms + 1,
            OperationErrorCode::CleanupFailed,
            Some(1),
            None,
            None,
            OutcomeSource::Dispatch,
        )
        .expect("fact");
        conn.execute(
            "INSERT INTO events (
                event_id, task_id, task_revision, event_type, schema_version, occurred_at_ms, payload
             ) VALUES (?1, NULL, NULL, 'operation.failed', ?2, ?3, ?4)",
            rusqlite::params![
                EventId::from_bytes(fixed_uuid_v7(0x51))
                    .expect("eid")
                    .as_bytes()
                    .as_slice(),
                i64::from(EVENT_SCHEMA_VERSION),
                accepted_at_ms + 1,
                rmp_serde::to_vec(&failed).expect("pack"),
            ],
        )
        .expect("forge event-only failure");
        let state: String = conn
            .query_row(
                "SELECT state FROM operations WHERE operation_id = ?1",
                [quit_op.as_bytes().as_slice()],
                |row| row.get(0),
            )
            .expect("projection still accepted");
        assert_eq!(state, "accepted");
    }

    let bus = CommandBus::open(&path).expect("reopen");
    let err = bus
        .operation_status(quit_op)
        .expect_err("event-only forged failure must corrupt Accepted receipt lookup");
    assert_eq!(err, StoreError::Corruption);
}

#[test]
fn host_cleanup_event_only_forged_non_failed_terminals_are_runtime_corruption_while_accepted() {
    use devmanager::domain::event::{
        OperationCancelledFact, OperationSettledFact, OperationUncertainFact, EVENT_SCHEMA_VERSION,
    };
    use devmanager::domain::id::EventId;
    use devmanager::domain::operation::{
        CancellationReason, OperationUncertaintyCode, OutcomeSource,
    };
    use devmanager::kernel::StoreError;

    #[derive(Clone, Copy)]
    enum ForgedKind {
        Settled,
        Cancelled,
        Uncertain,
    }

    for (idx, kind) in [
        ForgedKind::Settled,
        ForgedKind::Cancelled,
        ForgedKind::Uncertain,
    ]
    .into_iter()
    .enumerate()
    {
        let dir = TempDir::new().expect("tempdir");
        let path = temp_db_path(&dir);
        let (quit_op, accepted_at_ms) = {
            let mut bus = CommandBus::open(&path).expect("open");
            let quit_op = confirm_host_quit(&mut bus);
            let accepted_at_ms: i64 = {
                let conn = open_raw(&path);
                conn.query_row(
                    "SELECT accepted_at_ms FROM operations WHERE operation_id = ?1",
                    [quit_op.as_bytes().as_slice()],
                    |row| row.get(0),
                )
                .expect("accepted_at")
            };
            (quit_op, accepted_at_ms)
        };
        let occurred_at = accepted_at_ms + 1;
        let (event_type, payload) = match kind {
            ForgedKind::Settled => (
                "operation.settled",
                rmp_serde::to_vec(
                    &OperationSettledFact::with_source(
                        quit_command_id(),
                        quit_op,
                        occurred_at,
                        vec![],
                        Some(1),
                        None,
                        None,
                        OutcomeSource::Dispatch,
                    )
                    .expect("settled"),
                )
                .expect("pack"),
            ),
            ForgedKind::Cancelled => (
                "operation.cancelled",
                rmp_serde::to_vec(
                    &OperationCancelledFact::new(
                        quit_command_id(),
                        quit_op,
                        occurred_at,
                        CancellationReason::Superseded,
                        Some(1),
                        None,
                        None,
                    )
                    .expect("cancelled"),
                )
                .expect("pack"),
            ),
            ForgedKind::Uncertain => (
                "operation.uncertain",
                rmp_serde::to_vec(
                    &OperationUncertainFact::new(
                        quit_command_id(),
                        quit_op,
                        occurred_at,
                        OperationUncertaintyCode::AmbiguousDispatch,
                        Some(1),
                        None,
                        None,
                    )
                    .expect("uncertain"),
                )
                .expect("pack"),
            ),
        };
        {
            let conn = open_raw(&path);
            conn.execute(
                "INSERT INTO events (
                    event_id, task_id, task_revision, event_type, schema_version, occurred_at_ms, payload
                 ) VALUES (?1, NULL, NULL, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    EventId::from_bytes(fixed_uuid_v7(0x70 + idx as u8))
                        .expect("eid")
                        .as_bytes()
                        .as_slice(),
                    event_type,
                    i64::from(EVENT_SCHEMA_VERSION),
                    occurred_at,
                    payload,
                ],
            )
            .expect("forge event-only terminal");
            let state: String = conn
                .query_row(
                    "SELECT state FROM operations WHERE operation_id = ?1",
                    [quit_op.as_bytes().as_slice()],
                    |row| row.get(0),
                )
                .expect("projection still accepted");
            assert_eq!(state, "accepted", "{event_type} forge must leave Accepted");
        }

        let bus = CommandBus::open(&path).expect("reopen");
        let err = bus.operation_status(quit_op).expect_err(&format!(
            "event-only forged {event_type} must corrupt Accepted receipt lookup"
        ));
        assert_eq!(err, StoreError::Corruption, "{event_type}");
    }
}

#[test]
fn host_cleanup_extra_matching_terminal_beside_cleanup_failed_is_runtime_corruption() {
    use devmanager::domain::event::{OperationSettledFact, EVENT_SCHEMA_VERSION};
    use devmanager::domain::id::EventId;
    use devmanager::domain::operation::{OperationErrorCode, OperationState, OutcomeSource};
    use devmanager::host::{HostCleanupProgress, HostCleanupWorker};
    use devmanager::kernel::StoreError;

    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let (quit_op, settled_at_ms) = {
        let mut bus = CommandBus::open(&path).expect("open");
        let task = task_id(0x71);
        bus.execute(envelope(
            command_id(0x72),
            None,
            None,
            Command::CreateTask(create_task(task)),
        ))
        .expect("create");
        register_open_agent(&mut bus, task, command_id(0x73), 1);
        let quit_op = confirm_host_quit(&mut bus);
        drive_four_cleanup_branches(&mut bus, quit_op);
        let failed = HostCleanupWorker::run_once(&mut bus).expect("terminalize");
        let HostCleanupProgress::Failed {
            operation_id,
            settled_at_ms,
            ..
        } = failed
        else {
            panic!("expected Failed, got {failed:?}");
        };
        assert_eq!(operation_id, quit_op);
        assert_eq!(
            bus.operation_status(quit_op).expect("failed status"),
            Some(OperationState::Failed {
                settled_at_ms,
                code: OperationErrorCode::CleanupFailed,
            })
        );
        (quit_op, settled_at_ms)
    };

    {
        let conn = open_raw(&path);
        let settled = OperationSettledFact::with_source(
            quit_command_id(),
            quit_op,
            settled_at_ms + 1,
            vec![],
            Some(1),
            None,
            None,
            OutcomeSource::Dispatch,
        )
        .expect("extra settled");
        conn.execute(
            "INSERT INTO events (
                event_id, task_id, task_revision, event_type, schema_version, occurred_at_ms, payload
             ) VALUES (?1, NULL, NULL, 'operation.settled', ?2, ?3, ?4)",
            rusqlite::params![
                EventId::from_bytes(fixed_uuid_v7(0x74))
                    .expect("eid")
                    .as_bytes()
                    .as_slice(),
                i64::from(EVENT_SCHEMA_VERSION),
                settled_at_ms + 1,
                rmp_serde::to_vec(&settled).expect("pack"),
            ],
        )
        .expect("forge extra matching settled terminal");
        let state: String = conn
            .query_row(
                "SELECT state FROM operations WHERE operation_id = ?1",
                [quit_op.as_bytes().as_slice()],
                |row| row.get(0),
            )
            .expect("projection remains failed");
        assert_eq!(state, "failed");
    }

    let bus = CommandBus::open(&path).expect("reopen");
    let err = bus
        .operation_status(quit_op)
        .expect_err("extra matching terminal beside CleanupFailed must corrupt");
    assert_eq!(err, StoreError::Corruption);
}

#[test]
fn host_cleanup_failed_fact_predating_final_branch_rebuild_rolls_back() {
    use devmanager::domain::event::{OperationFailedFact, EVENT_SCHEMA_VERSION};
    use devmanager::domain::id::EventId;
    use devmanager::domain::operation::{OperationErrorCode, OutcomeSource};
    use devmanager::kernel::{KernelStore, StoreError};

    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let (quit_op, final_branch_at, admission_before, ops_before) = {
        let mut bus = CommandBus::open(&path).expect("open");
        let task = task_id(0x52);
        bus.execute(envelope(
            command_id(0x53),
            None,
            None,
            Command::CreateTask(create_task(task)),
        ))
        .expect("create");
        register_open_agent(&mut bus, task, command_id(0x54), 1);
        let quit_op = confirm_host_quit(&mut bus);
        drive_four_cleanup_branches(&mut bus, quit_op);
        let final_branch_at: i64 = {
            let conn = open_raw(&path);
            conn.query_row(
                "SELECT completed_at_ms FROM host_cleanup_branches
                 WHERE operation_id = ?1 AND branch = 'task_teardowns'",
                [quit_op.as_bytes().as_slice()],
                |row| row.get(0),
            )
            .expect("final branch")
        };
        (
            quit_op,
            final_branch_at,
            host_admission_row(&path).expect("closing"),
            count_table(&path, "operations"),
        )
    };

    {
        let conn = open_raw(&path);
        let early = final_branch_at.saturating_sub(1);
        assert!(early < final_branch_at);
        let failed = OperationFailedFact::with_source(
            quit_command_id(),
            quit_op,
            early,
            OperationErrorCode::CleanupFailed,
            Some(1),
            None,
            None,
            OutcomeSource::Dispatch,
        )
        .expect("fact");
        conn.execute(
            "INSERT INTO events (
                event_id, task_id, task_revision, event_type, schema_version, occurred_at_ms, payload
             ) VALUES (?1, NULL, NULL, 'operation.failed', ?2, ?3, ?4)",
            rusqlite::params![
                EventId::from_bytes(fixed_uuid_v7(0x55))
                    .expect("eid")
                    .as_bytes()
                    .as_slice(),
                i64::from(EVENT_SCHEMA_VERSION),
                early,
                rmp_serde::to_vec(&failed).expect("pack"),
            ],
        )
        .expect("forge predating failure immediately after final branch");
    }

    let before_rows = host_cleanup_branch_rows(&path);
    {
        let mut store = KernelStore::open(&path).expect("reopen");
        let err = store
            .rebuild_projections()
            .expect_err("predating CleanupFailed must fail closed");
        assert!(
            matches!(&err, StoreError::Projection(_)),
            "predating failure must fail rebuild, got {err:?}"
        );
    }
    assert_eq!(host_cleanup_branch_rows(&path), before_rows);
    assert_eq!(
        host_admission_row(&path).expect("closing"),
        admission_before
    );
    assert_eq!(count_table(&path, "operations"), ops_before);
    {
        let conn = open_raw(&path);
        let state: String = conn
            .query_row(
                "SELECT state FROM operations WHERE operation_id = ?1",
                [quit_op.as_bytes().as_slice()],
                |row| row.get(0),
            )
            .expect("state");
        assert_eq!(state, "accepted");
    }
}

#[test]
fn host_cleanup_failed_lineage_survives_later_valid_side_effect_settlement() {
    use std::time::Duration;

    use devmanager::domain::host::HostCleanupBranch;
    use devmanager::domain::operation::{OperationErrorCode, OperationState, ResourceFence};
    use devmanager::host::{HostCleanupProgress, HostCleanupWorker};
    use devmanager::kernel::{DispatchCompletion, Effect, KernelStore};

    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let (quit_op, release_op, task, resource, settled_at_ms) = {
        let mut bus = CommandBus::open(&path).expect("open");
        let task = task_id(0x61);
        let resource = resource_id(0x62);
        bus.execute(envelope(
            command_id(0x63),
            None,
            None,
            Command::CreateTask(create_task(task)),
        ))
        .expect("create");
        register_active_terminal(&mut bus, task, command_id(0x64), resource, 1);
        let release_op = {
            let receipt = bus
                .execute(envelope(
                    command_id(0x65),
                    Some(task),
                    Some(2),
                    Command::ReleaseResource {
                        resource_id: resource,
                    },
                ))
                .expect("begin release");
            match receipt {
                CommandReceipt::Accepted { operation_id, .. } => operation_id,
                other => panic!("expected release accepted, got {other:?}"),
            }
        };
        let quit_op = confirm_host_quit(&mut bus);
        drive_four_cleanup_branches(&mut bus, quit_op);
        assert!(host_cleanup_branch_rows(&path).iter().any(|row| {
            row.1 == HostCleanupBranch::OutstandingEffects.as_str() && row.2 == "failed"
        }));
        let failed = HostCleanupWorker::run_once(&mut bus).expect("terminalize");
        let HostCleanupProgress::Failed {
            operation_id,
            settled_at_ms,
            ..
        } = failed
        else {
            panic!("expected Failed, got {failed:?}");
        };
        assert_eq!(operation_id, quit_op);
        (quit_op, release_op, task, resource, settled_at_ms)
    };

    {
        let mut store = KernelStore::open(&path).expect("dispatch store");
        let claim = store
            .claim_next_dispatch(Duration::from_secs(30))
            .expect("claim")
            .expect("pending release ready");
        let permit = store.begin_dispatch(&claim).expect("begin");
        assert_eq!(
            permit.effect(),
            &Effect::ReleaseResource {
                task_id: task,
                action_epoch: 0,
                resource_fence: ResourceFence::new(resource, 0),
            }
        );
        let state = store
            .record_dispatch_completion(&permit, DispatchCompletion::Settled)
            .expect("settle release after host CleanupFailed");
        assert!(matches!(state, OperationState::Settled { .. }));
        assert_eq!(
            store.operation_status(release_op).expect("release status"),
            Some(state)
        );
    }

    {
        let mut bus = CommandBus::open(&path).expect("reopen bus");
        assert_eq!(
            bus.operation_status(quit_op).expect("quit status"),
            Some(OperationState::Failed {
                settled_at_ms,
                code: OperationErrorCode::CleanupFailed,
            })
        );
        assert_eq!(
            HostCleanupWorker::run_once(&mut bus).expect("idle"),
            HostCleanupProgress::Idle
        );
    }

    {
        let mut store = KernelStore::open(&path).expect("rebuild store");
        let rebuild = store
            .rebuild_projections()
            .expect("rebuild after later settle");
        assert!(rebuild.events_replayed > 0);
        assert_eq!(
            store.operation_status(quit_op).expect("quit after rebuild"),
            Some(OperationState::Failed {
                settled_at_ms,
                code: OperationErrorCode::CleanupFailed,
            })
        );
    }

    let mut bus = CommandBus::open(&path).expect("final idle");
    assert_eq!(
        HostCleanupWorker::run_once(&mut bus).expect("still idle"),
        HostCleanupProgress::Idle
    );
}

fn host_cleanup_branch_event_ids_in_order(path: &Path) -> Vec<devmanager::domain::id::EventId> {
    use std::collections::HashMap;

    use devmanager::domain::event::HostCleanupBranchCompletedPayload;
    use devmanager::domain::host::HostCleanupBranch;
    use devmanager::domain::id::EventId;

    let conn = open_raw(path);
    let mut stmt = conn
        .prepare(
            "SELECT event_id, payload
             FROM events
             WHERE event_type = 'host.cleanup_branch_completed'
             ORDER BY sequence ASC",
        )
        .expect("prepare");
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?))
        })
        .expect("query");
    let mut by_branch = HashMap::new();
    for row in rows {
        let (event_id_bytes, payload) = row.expect("row");
        let event_id = EventId::from_bytes({
            let mut bytes = [0u8; 16];
            bytes.copy_from_slice(&event_id_bytes);
            bytes
        })
        .expect("event id");
        let decoded: HostCleanupBranchCompletedPayload =
            rmp_serde::from_slice(&payload).expect("decode branch payload");
        assert!(
            by_branch.insert(decoded.branch, event_id).is_none(),
            "duplicate branch event"
        );
    }
    HostCleanupBranch::ORDER
        .iter()
        .map(|branch| {
            *by_branch
                .get(branch)
                .unwrap_or_else(|| panic!("missing {branch:?}"))
        })
        .collect()
}

#[test]
fn host_cleanup_all_success_settle_once_idempotent_reopen_and_rebuild() {
    use devmanager::domain::event::{Event, OperationSettledFact, EVENT_SCHEMA_VERSION};
    use devmanager::domain::host::HostCleanupBranch;
    use devmanager::domain::operation::{OperationState, OutcomeSource};
    use devmanager::host::{
        HostCleanupProgress, HostCleanupSuccessSettlement, HostCleanupWorker,
        HostRestartDisposition,
    };

    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut bus = CommandBus::open(&path).expect("open");
    let quit_op = confirm_host_quit(&mut bus);
    drive_four_cleanup_branches(&mut bus, quit_op);
    assert_eq!(
        HostCleanupWorker::run_once(&mut bus).expect("ready"),
        HostCleanupProgress::ReadyToExit {
            operation_id: quit_op,
            action_epoch: 1,
        }
    );
    let expected_ids = host_cleanup_branch_event_ids_in_order(&path);
    assert_eq!(expected_ids.len(), HostCleanupBranch::ORDER.len());
    let events_before = count_table(&path, "events");

    let first = HostCleanupWorker::settle_success(&mut bus).expect("settle once");
    assert_eq!(
        first,
        HostCleanupSuccessSettlement {
            operation_id: quit_op,
            action_epoch: 1,
            settled_at_ms: first.settled_at_ms,
            result_event_ids: expected_ids.clone(),
            terminal_event: first.terminal_event.clone(),
        }
    );
    assert!(first.settled_at_ms > 0);
    assert!(first.terminal_event.sequence > 0);
    assert_eq!(first.terminal_event.occurred_at_ms, first.settled_at_ms);
    match &first.terminal_event.payload {
        Event::OperationSettled(fact) => {
            assert_eq!(fact.operation_id, quit_op);
            assert_eq!(fact.action_epoch, Some(1));
            assert_eq!(fact.result_event_ids, expected_ids);
            assert_eq!(fact.settled_at_ms, first.settled_at_ms);
        }
        other => panic!("expected OperationSettled terminal payload, got {other:?}"),
    }
    assert_eq!(count_table(&path, "events"), events_before + 1);
    assert_eq!(
        bus.operation_status(quit_op).expect("settled"),
        Some(OperationState::Settled {
            settled_at_ms: first.settled_at_ms,
            result_event_ids: expected_ids.clone(),
        })
    );
    assert_eq!(
        HostCleanupWorker::restart_disposition(&bus).expect("closed"),
        HostRestartDisposition::Closed {
            operation_id: quit_op,
            action_epoch: 1,
            settled_at_ms: first.settled_at_ms,
        }
    );
    assert_eq!(
        HostCleanupWorker::run_once(&mut bus).expect("idle after settle"),
        HostCleanupProgress::Idle
    );

    let second = HostCleanupWorker::settle_success(&mut bus).expect("idempotent");
    assert_eq!(second, first);
    assert_eq!(second.terminal_event.id, first.terminal_event.id);
    assert_eq!(
        second.terminal_event.sequence,
        first.terminal_event.sequence
    );
    assert_eq!(count_table(&path, "events"), events_before + 1);

    drop(bus);
    {
        let mut bus = CommandBus::open(&path).expect("reopen");
        let third = HostCleanupWorker::settle_success(&mut bus).expect("reopen idempotent");
        assert_eq!(third, first);
        assert_eq!(
            HostCleanupWorker::run_once(&mut bus).expect("idle reopen"),
            HostCleanupProgress::Idle
        );
        assert_eq!(
            HostCleanupWorker::restart_disposition(&bus).expect("still closed"),
            HostRestartDisposition::Closed {
                operation_id: quit_op,
                action_epoch: 1,
                settled_at_ms: first.settled_at_ms,
            }
        );
    }

    {
        let conn = open_raw(&path);
        let (event_type, schema_version, payload, occurred_at, task_id, task_revision): (
            String,
            i64,
            Vec<u8>,
            i64,
            Option<Vec<u8>>,
            Option<i64>,
        ) = conn
            .query_row(
                "SELECT event_type, schema_version, payload, occurred_at_ms, task_id, task_revision
                 FROM events
                 WHERE event_type = 'operation.settled'
                 ORDER BY sequence DESC
                 LIMIT 1",
                [],
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
            .expect("settled row");
        assert_eq!(event_type, "operation.settled");
        assert_eq!(schema_version, i64::from(EVENT_SCHEMA_VERSION));
        assert!(task_id.is_none());
        assert!(task_revision.is_none());
        assert_eq!(occurred_at, first.settled_at_ms);
        let fact: OperationSettledFact = rmp_serde::from_slice(&payload).expect("fact");
        assert_eq!(fact.command_id, quit_command_id());
        assert_eq!(fact.operation_id, quit_op);
        assert_eq!(fact.settled_at_ms, first.settled_at_ms);
        assert_eq!(fact.result_event_ids, expected_ids);
        assert_eq!(fact.action_epoch, Some(1));
        assert!(fact.resource_id.is_none());
        assert!(fact.runtime_generation.is_none());
        assert_eq!(fact.source, OutcomeSource::Dispatch);
        let _ = Event::OperationSettled(fact);
    }

    {
        let mut store = KernelStore::open(&path).expect("rebuild");
        let rebuild = store.rebuild_projections().expect("rebuild");
        assert!(rebuild.events_replayed > 0);
        assert_eq!(
            store.operation_status(quit_op).expect("status"),
            Some(OperationState::Settled {
                settled_at_ms: first.settled_at_ms,
                result_event_ids: expected_ids,
            })
        );
    }
}

#[test]
fn host_cleanup_forged_success_settlement_table_is_runtime_and_rebuild_corruption() {
    use devmanager::domain::event::{OperationSettledFact, EVENT_SCHEMA_VERSION};
    use devmanager::domain::host::HostCleanupBranch;
    use devmanager::domain::id::EventId;
    use devmanager::domain::operation::OutcomeSource;
    use devmanager::host::{HostCleanupProgress, HostCleanupWorker};
    use devmanager::kernel::StoreError;

    #[derive(Clone, Copy)]
    enum ForgeCase {
        SwappedResultIds,
        ForeignResultId,
        MissingResultId,
        WrongSource,
        WrongEpoch,
        TaskScoped,
        PredatingFinalBranch,
        ExtraMatchingTerminal,
        EventOnlyWhileAccepted,
        ProjectionOnlyMismatch,
    }

    for (idx, case) in [
        ForgeCase::SwappedResultIds,
        ForgeCase::ForeignResultId,
        ForgeCase::MissingResultId,
        ForgeCase::WrongSource,
        ForgeCase::WrongEpoch,
        ForgeCase::TaskScoped,
        ForgeCase::PredatingFinalBranch,
        ForgeCase::ExtraMatchingTerminal,
        ForgeCase::EventOnlyWhileAccepted,
        ForgeCase::ProjectionOnlyMismatch,
    ]
    .into_iter()
    .enumerate()
    {
        let dir = TempDir::new().expect("tempdir");
        let path = temp_db_path(&dir);
        let (quit_op, branch_ids, accepted_at, final_branch_at) = {
            let mut bus = CommandBus::open(&path).expect("open");
            let quit_op = confirm_host_quit(&mut bus);
            drive_four_cleanup_branches(&mut bus, quit_op);
            assert!(matches!(
                HostCleanupWorker::run_once(&mut bus).expect("ready"),
                HostCleanupProgress::ReadyToExit { .. }
            ));
            let branch_ids = host_cleanup_branch_event_ids_in_order(&path);
            let accepted_at: i64 = {
                let conn = open_raw(&path);
                conn.query_row(
                    "SELECT accepted_at_ms FROM operations WHERE operation_id = ?1",
                    [quit_op.as_bytes().as_slice()],
                    |row| row.get(0),
                )
                .expect("accepted")
            };
            let final_branch_at: i64 = {
                let conn = open_raw(&path);
                conn.query_row(
                    "SELECT completed_at_ms FROM host_cleanup_branches
                     WHERE operation_id = ?1 AND branch = ?2",
                    rusqlite::params![
                        quit_op.as_bytes().as_slice(),
                        HostCleanupBranch::TaskTeardowns.as_str(),
                    ],
                    |row| row.get(0),
                )
                .expect("final branch")
            };
            (quit_op, branch_ids, accepted_at, final_branch_at)
        };

        match case {
            ForgeCase::EventOnlyWhileAccepted => {
                let conn = open_raw(&path);
                let ids = branch_ids.clone();
                let settled = OperationSettledFact::with_source(
                    quit_command_id(),
                    quit_op,
                    final_branch_at + 1,
                    ids,
                    Some(1),
                    None,
                    None,
                    OutcomeSource::Dispatch,
                )
                .expect("fact");
                conn.execute(
                    "INSERT INTO events (
                        event_id, task_id, task_revision, event_type, schema_version, occurred_at_ms, payload
                     ) VALUES (?1, NULL, NULL, 'operation.settled', ?2, ?3, ?4)",
                    rusqlite::params![
                        EventId::from_bytes(fixed_uuid_v7(0x80 + idx as u8))
                            .expect("eid")
                            .as_bytes()
                            .as_slice(),
                        i64::from(EVENT_SCHEMA_VERSION),
                        final_branch_at + 1,
                        rmp_serde::to_vec(&settled).expect("pack"),
                    ],
                )
                .expect("forge event-only");
                let state: String = conn
                    .query_row(
                        "SELECT state FROM operations WHERE operation_id = ?1",
                        [quit_op.as_bytes().as_slice()],
                        |row| row.get(0),
                    )
                    .expect("state");
                assert_eq!(state, "accepted");
                let bus = CommandBus::open(&path).expect("reopen");
                let err = bus
                    .operation_status(quit_op)
                    .expect_err("event-only settle must corrupt Accepted");
                assert_eq!(err, StoreError::Corruption);
                continue;
            }
            ForgeCase::ProjectionOnlyMismatch => {
                let mut bus = CommandBus::open(&path).expect("open settle");
                let settlement =
                    HostCleanupWorker::settle_success(&mut bus).expect("legitimate settle");
                drop(bus);
                let conn = open_raw(&path);
                conn.execute(
                    "UPDATE operations SET result = NULL WHERE operation_id = ?1",
                    [quit_op.as_bytes().as_slice()],
                )
                .expect("tamper projection");
                let _ = settlement;
                let bus = CommandBus::open(&path).expect("reopen");
                let err = bus
                    .operation_status(quit_op)
                    .expect_err("projection-only mismatch must corrupt");
                assert_eq!(err, StoreError::Corruption);
                continue;
            }
            ForgeCase::ExtraMatchingTerminal => {
                let mut bus = CommandBus::open(&path).expect("open settle");
                let settlement =
                    HostCleanupWorker::settle_success(&mut bus).expect("legitimate settle");
                drop(bus);
                let conn = open_raw(&path);
                let settled = OperationSettledFact::with_source(
                    quit_command_id(),
                    quit_op,
                    settlement.settled_at_ms + 1,
                    branch_ids.clone(),
                    Some(1),
                    None,
                    None,
                    OutcomeSource::Dispatch,
                )
                .expect("extra");
                conn.execute(
                    "INSERT INTO events (
                        event_id, task_id, task_revision, event_type, schema_version, occurred_at_ms, payload
                     ) VALUES (?1, NULL, NULL, 'operation.settled', ?2, ?3, ?4)",
                    rusqlite::params![
                        EventId::from_bytes(fixed_uuid_v7(0x90 + idx as u8))
                            .expect("eid")
                            .as_bytes()
                            .as_slice(),
                        i64::from(EVENT_SCHEMA_VERSION),
                        settlement.settled_at_ms + 1,
                        rmp_serde::to_vec(&settled).expect("pack"),
                    ],
                )
                .expect("extra terminal");
                let bus = CommandBus::open(&path).expect("reopen");
                let err = bus
                    .operation_status(quit_op)
                    .expect_err("extra matching terminal must corrupt");
                assert_eq!(err, StoreError::Corruption);
                continue;
            }
            _ => {}
        }

        let (result_ids, source, epoch, task_scope, occurred_at) = match case {
            ForgeCase::SwappedResultIds => {
                let mut ids = branch_ids.clone();
                ids.swap(0, 1);
                (
                    ids,
                    OutcomeSource::Dispatch,
                    Some(1u64),
                    false,
                    final_branch_at + 1,
                )
            }
            ForgeCase::ForeignResultId => {
                let mut ids = branch_ids.clone();
                ids[2] = EventId::from_bytes(fixed_uuid_v7(0xA0 + idx as u8)).expect("foreign");
                (
                    ids,
                    OutcomeSource::Dispatch,
                    Some(1),
                    false,
                    final_branch_at + 1,
                )
            }
            ForgeCase::MissingResultId => (
                branch_ids[..3].to_vec(),
                OutcomeSource::Dispatch,
                Some(1),
                false,
                final_branch_at + 1,
            ),
            ForgeCase::WrongSource => (
                branch_ids.clone(),
                OutcomeSource::verified_reconciliation(0, "foreign-host-settle").expect("source"),
                Some(1),
                false,
                final_branch_at + 1,
            ),
            ForgeCase::WrongEpoch => (
                branch_ids.clone(),
                OutcomeSource::Dispatch,
                Some(2),
                false,
                final_branch_at + 1,
            ),
            ForgeCase::TaskScoped => (
                branch_ids.clone(),
                OutcomeSource::Dispatch,
                Some(1),
                true,
                final_branch_at + 1,
            ),
            ForgeCase::PredatingFinalBranch => (
                branch_ids.clone(),
                OutcomeSource::Dispatch,
                Some(1),
                false,
                final_branch_at.saturating_sub(1).max(accepted_at),
            ),
            _ => unreachable!(),
        };

        let occurred_at = if matches!(case, ForgeCase::PredatingFinalBranch) {
            // Force strictly before final branch completion when clocks allow.
            if final_branch_at > accepted_at {
                final_branch_at - 1
            } else {
                accepted_at
            }
        } else {
            occurred_at
        };

        {
            let conn = open_raw(&path);
            let settled = OperationSettledFact::with_source(
                quit_command_id(),
                quit_op,
                occurred_at,
                result_ids,
                epoch,
                None,
                None,
                source,
            )
            .expect("fact");
            let task_bytes = if task_scope {
                Some(task_id(0xB0 + idx as u8).as_bytes().to_vec())
            } else {
                None
            };
            conn.execute(
                "INSERT INTO events (
                    event_id, task_id, task_revision, event_type, schema_version, occurred_at_ms, payload
                 ) VALUES (?1, ?2, NULL, 'operation.settled', ?3, ?4, ?5)",
                rusqlite::params![
                    EventId::from_bytes(fixed_uuid_v7(0xC0 + idx as u8))
                        .expect("eid")
                        .as_bytes()
                        .as_slice(),
                    task_bytes,
                    i64::from(EVENT_SCHEMA_VERSION),
                    occurred_at,
                    rmp_serde::to_vec(&settled).expect("pack"),
                ],
            )
            .expect("forge settled");
        }

        let before_ops = count_table(&path, "operations");
        let before_admission = host_admission_row(&path);
        let mut store = KernelStore::open(&path).expect("rebuild store");
        let err = store
            .rebuild_projections()
            .expect_err(&format!("forge case {idx} must fail rebuild"));
        assert!(
            matches!(err, StoreError::Projection(_) | StoreError::Corruption),
            "case {idx} expected fail-closed, got {err:?}"
        );
        drop(store);
        assert_eq!(count_table(&path, "operations"), before_ops);
        assert_eq!(host_admission_row(&path), before_admission);
    }
}

#[test]
fn host_restart_disposition_covers_incomplete_failed_ready_and_closed() {
    use devmanager::domain::operation::OperationErrorCode;
    use devmanager::host::{HostCleanupProgress, HostCleanupWorker, HostRestartDisposition};

    // No Closing admission.
    {
        let dir = TempDir::new().expect("tempdir");
        let path = temp_db_path(&dir);
        let bus = CommandBus::open(&path).expect("open");
        assert_eq!(
            HostCleanupWorker::restart_disposition(&bus).expect("open"),
            HostRestartDisposition::ServeResume
        );
    }

    // Incomplete Accepted journal.
    {
        let dir = TempDir::new().expect("tempdir");
        let path = temp_db_path(&dir);
        let mut bus = CommandBus::open(&path).expect("open");
        let quit_op = confirm_host_quit(&mut bus);
        assert!(matches!(
            HostCleanupWorker::run_once(&mut bus).expect("first branch"),
            HostCleanupProgress::BranchCompleted { .. }
        ));
        assert_eq!(
            HostCleanupWorker::restart_disposition(&bus).expect("incomplete"),
            HostRestartDisposition::ServeResume
        );
        assert_eq!(
            bus.operation_status(quit_op).expect("accepted"),
            Some(devmanager::domain::operation::OperationState::Accepted)
        );
    }

    // Exact CleanupFailed.
    {
        let dir = TempDir::new().expect("tempdir");
        let path = temp_db_path(&dir);
        let mut bus = CommandBus::open(&path).expect("open");
        let task = task_id(0xD1);
        bus.execute(envelope(
            command_id(0xD2),
            None,
            None,
            Command::CreateTask(create_task(task)),
        ))
        .expect("create");
        register_open_agent(&mut bus, task, command_id(0xD3), 1);
        let quit_op = confirm_host_quit(&mut bus);
        drive_four_cleanup_branches(&mut bus, quit_op);
        let failed = HostCleanupWorker::run_once(&mut bus).expect("terminalize");
        let HostCleanupProgress::Failed {
            operation_id,
            action_epoch,
            settled_at_ms,
        } = failed
        else {
            panic!("expected Failed, got {failed:?}");
        };
        assert_eq!(operation_id, quit_op);
        assert_eq!(
            HostCleanupWorker::restart_disposition(&bus).expect("inspect"),
            HostRestartDisposition::ServeInspection {
                operation_id: quit_op,
                action_epoch,
                settled_at_ms,
            }
        );
        assert_eq!(
            bus.operation_status(quit_op).expect("failed"),
            Some(devmanager::domain::operation::OperationState::Failed {
                settled_at_ms,
                code: OperationErrorCode::CleanupFailed,
            })
        );
    }

    // Complete all-success Accepted => ReadyToArmAndSettle.
    {
        let dir = TempDir::new().expect("tempdir");
        let path = temp_db_path(&dir);
        let mut bus = CommandBus::open(&path).expect("open");
        let quit_op = confirm_host_quit(&mut bus);
        drive_four_cleanup_branches(&mut bus, quit_op);
        assert_eq!(
            HostCleanupWorker::run_once(&mut bus).expect("ready"),
            HostCleanupProgress::ReadyToExit {
                operation_id: quit_op,
                action_epoch: 1,
            }
        );
        assert_eq!(
            HostCleanupWorker::restart_disposition(&bus).expect("ready"),
            HostRestartDisposition::ReadyToArmAndSettle {
                operation_id: quit_op,
                action_epoch: 1,
            }
        );
        // Maintenance still does not settle.
        assert_eq!(
            HostCleanupWorker::run_once(&mut bus).expect("still ready"),
            HostCleanupProgress::ReadyToExit {
                operation_id: quit_op,
                action_epoch: 1,
            }
        );
    }

    // Exact Settled => Closed (covered primarily by settle test; assert here too).
    {
        let dir = TempDir::new().expect("tempdir");
        let path = temp_db_path(&dir);
        let mut bus = CommandBus::open(&path).expect("open");
        let quit_op = confirm_host_quit(&mut bus);
        drive_four_cleanup_branches(&mut bus, quit_op);
        let _ = HostCleanupWorker::run_once(&mut bus).expect("ready");
        let settlement = HostCleanupWorker::settle_success(&mut bus).expect("settle");
        assert_eq!(
            HostCleanupWorker::restart_disposition(&bus).expect("closed"),
            HostRestartDisposition::Closed {
                operation_id: quit_op,
                action_epoch: 1,
                settled_at_ms: settlement.settled_at_ms,
            }
        );
    }
}

#[test]
fn host_cleanup_settled_must_immediately_follow_task_teardowns_predecessor() {
    use devmanager::domain::event::{
        OperationSettledFact, TaskRenamedPayload, EVENT_SCHEMA_VERSION,
    };
    use devmanager::domain::id::EventId;
    use devmanager::domain::operation::OutcomeSource;
    use devmanager::host::{HostCleanupProgress, HostCleanupWorker};
    use devmanager::kernel::StoreError;

    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let (quit_op, branch_ids, final_at, task) = {
        let mut bus = CommandBus::open(&path).expect("open");
        let task = task_id(0x9A);
        bus.execute(envelope(
            command_id(0x9B),
            None,
            None,
            Command::CreateTask(create_task(task)),
        ))
        .expect("create task before quit");
        let quit_op = confirm_host_quit(&mut bus);
        drive_four_cleanup_branches(&mut bus, quit_op);
        assert!(matches!(
            HostCleanupWorker::run_once(&mut bus).expect("ready"),
            HostCleanupProgress::ReadyToExit { .. }
        ));
        let branch_ids = host_cleanup_branch_event_ids_in_order(&path);
        let final_at: i64 = {
            let conn = open_raw(&path);
            conn.query_row(
                "SELECT MAX(completed_at_ms) FROM host_cleanup_branches WHERE operation_id = ?1",
                [quit_op.as_bytes().as_slice()],
                |row| row.get(0),
            )
            .expect("max completed")
        };
        (quit_op, branch_ids, final_at, task)
    };

    // Replay-valid unrelated task mutation between final TaskTeardowns and host settle.
    {
        let conn = open_raw(&path);
        let payload = TaskRenamedPayload {
            title: "separator-before-host-settle".into(),
        };
        conn.execute(
            "INSERT INTO events (
                event_id, task_id, task_revision, event_type, schema_version, occurred_at_ms, payload
             ) VALUES (?1, ?2, ?3, 'task.renamed', ?4, ?5, ?6)",
            rusqlite::params![
                EventId::from_bytes(fixed_uuid_v7(0x9C))
                    .expect("eid")
                    .as_bytes()
                    .as_slice(),
                task.as_bytes().as_slice(),
                2_i64,
                i64::from(EVENT_SCHEMA_VERSION),
                final_at + 1,
                rmp_serde::to_vec(&payload).expect("pack"),
            ],
        )
        .expect("separator task.renamed");
    }

    {
        let mut bus = CommandBus::open(&path).expect("open");
        let err = HostCleanupWorker::settle_success(&mut bus)
            .expect_err("separator before settle must fail closed");
        assert_eq!(err, StoreError::Corruption);
    }

    {
        let conn = open_raw(&path);
        let settled = OperationSettledFact::with_source(
            quit_command_id(),
            quit_op,
            final_at + 2,
            branch_ids.clone(),
            Some(1),
            None,
            None,
            OutcomeSource::Dispatch,
        )
        .expect("fact");
        conn.execute(
            "INSERT INTO events (
                event_id, task_id, task_revision, event_type, schema_version, occurred_at_ms, payload
             ) VALUES (?1, NULL, NULL, 'operation.settled', ?2, ?3, ?4)",
            rusqlite::params![
                EventId::from_bytes(fixed_uuid_v7(0x91))
                    .expect("eid")
                    .as_bytes()
                    .as_slice(),
                i64::from(EVENT_SCHEMA_VERSION),
                final_at + 2,
                rmp_serde::to_vec(&settled).expect("pack"),
            ],
        )
        .expect("forge separated settle");
        conn.execute(
            "UPDATE operations
             SET state = 'settled', result = ?1, outcome_at_ms = ?2, outcome_code = NULL
             WHERE operation_id = ?3",
            rusqlite::params![
                rmp_serde::to_vec(&branch_ids).expect("pack ids"),
                final_at + 2,
                quit_op.as_bytes().as_slice(),
            ],
        )
        .expect("project settle");
    }

    let bus = CommandBus::open(&path).expect("reopen");
    let err = bus
        .operation_status(quit_op)
        .expect_err("separated settle must corrupt runtime status");
    assert_eq!(err, StoreError::Corruption);
    drop(bus);

    let before_ops = count_table(&path, "operations");
    let mut store = KernelStore::open(&path).expect("rebuild store");
    let err = store
        .rebuild_projections()
        .expect_err("separated settle must fail rebuild due to predecessor violation");
    assert!(
        matches!(err, StoreError::Projection(_) | StoreError::Corruption),
        "got {err:?}"
    );
    drop(store);
    assert_eq!(count_table(&path, "operations"), before_ops);
}

#[test]
fn host_cleanup_settled_survives_later_unrelated_valid_global_event() {
    use devmanager::domain::event::{TaskRenamedPayload, EVENT_SCHEMA_VERSION};
    use devmanager::domain::id::EventId;
    use devmanager::domain::operation::OperationState;
    use devmanager::host::{HostCleanupWorker, HostRestartDisposition};

    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut bus = CommandBus::open(&path).expect("open");
    let task = task_id(0x97);
    bus.execute(envelope(
        command_id(0x98),
        None,
        None,
        Command::CreateTask(create_task(task)),
    ))
    .expect("create task before quit");
    let quit_op = confirm_host_quit(&mut bus);
    drive_four_cleanup_branches(&mut bus, quit_op);
    let _ = HostCleanupWorker::run_once(&mut bus).expect("ready");
    let settlement = HostCleanupWorker::settle_success(&mut bus).expect("settle");
    let expected_ids = settlement.result_event_ids.clone();
    drop(bus);

    {
        let conn = open_raw(&path);
        let payload = TaskRenamedPayload {
            title: "later-unrelated-rename".into(),
        };
        conn.execute(
            "INSERT INTO events (
                event_id, task_id, task_revision, event_type, schema_version, occurred_at_ms, payload
             ) VALUES (?1, ?2, ?3, 'task.renamed', ?4, ?5, ?6)",
            rusqlite::params![
                EventId::from_bytes(fixed_uuid_v7(0x99))
                    .expect("eid")
                    .as_bytes()
                    .as_slice(),
                task.as_bytes().as_slice(),
                2_i64,
                i64::from(EVENT_SCHEMA_VERSION),
                settlement.settled_at_ms + 5,
                rmp_serde::to_vec(&payload).expect("pack"),
            ],
        )
        .expect("later unrelated task.renamed");
    }

    let mut bus = CommandBus::open(&path).expect("reopen");
    assert_eq!(
        bus.operation_status(quit_op).expect("status"),
        Some(OperationState::Settled {
            settled_at_ms: settlement.settled_at_ms,
            result_event_ids: expected_ids.clone(),
        })
    );
    assert_eq!(
        HostCleanupWorker::restart_disposition(&bus).expect("closed"),
        HostRestartDisposition::Closed {
            operation_id: quit_op,
            action_epoch: 1,
            settled_at_ms: settlement.settled_at_ms,
        }
    );
    let again = HostCleanupWorker::settle_success(&mut bus).expect("idempotent after later event");
    assert_eq!(again, settlement);

    let mut store = KernelStore::open(&path).expect("rebuild");
    store
        .rebuild_projections()
        .expect("rebuild after later event");
    assert_eq!(
        store.operation_status(quit_op).expect("status"),
        Some(OperationState::Settled {
            settled_at_ms: settlement.settled_at_ms,
            result_event_ids: expected_ids,
        })
    );
}

#[test]
fn host_restart_disposition_orphaned_host_quit_lineage_without_admission_is_corruption() {
    use devmanager::host::{HostCleanupWorker, HostRestartDisposition};
    use devmanager::kernel::StoreError;

    {
        let dir = TempDir::new().expect("pristine");
        let path = temp_db_path(&dir);
        let bus = CommandBus::open(&path).expect("open");
        assert_eq!(
            HostCleanupWorker::restart_disposition(&bus).expect("pristine"),
            HostRestartDisposition::ServeResume
        );
    }

    // Just-confirmed quit with no cleanup branches: delete only admission + HostCloseBegun.
    {
        let dir = TempDir::new().expect("tempdir");
        let path = temp_db_path(&dir);
        let mut bus = CommandBus::open(&path).expect("open");
        let _quit_op = confirm_host_quit(&mut bus);
        drop(bus);
        {
            let conn = open_raw(&path);
            assert_eq!(
                conn.execute("DELETE FROM host_admission", [])
                    .expect("delete admission"),
                1
            );
            assert_eq!(
                conn.execute(
                    "DELETE FROM events WHERE event_type = 'host.close_begun'",
                    [],
                )
                .expect("delete close begun"),
                1
            );
            let branches: i64 = conn
                .query_row("SELECT COUNT(*) FROM host_cleanup_branches", [], |row| {
                    row.get(0)
                })
                .expect("branches");
            assert_eq!(branches, 0);
        }
        let bus = CommandBus::open(&path).expect("reopen");
        let err = HostCleanupWorker::restart_disposition(&bus)
            .expect_err("remaining accepted HostAdmission operation/fact/receipt must corrupt");
        assert_eq!(err, StoreError::Corruption);
    }

    // Settled: delete admission + close + cleanup branch events/projection; leave accepted/settled lineage.
    {
        let dir = TempDir::new().expect("tempdir");
        let path = temp_db_path(&dir);
        let mut bus = CommandBus::open(&path).expect("open");
        let quit_op = confirm_host_quit(&mut bus);
        drive_four_cleanup_branches(&mut bus, quit_op);
        let _ = HostCleanupWorker::run_once(&mut bus).expect("ready");
        HostCleanupWorker::settle_success(&mut bus).expect("settle");
        drop(bus);
        {
            let conn = open_raw(&path);
            assert_eq!(
                conn.execute("DELETE FROM host_admission", [])
                    .expect("delete admission"),
                1
            );
            assert!(
                conn.execute(
                    "DELETE FROM events WHERE event_type = 'host.close_begun'",
                    [],
                )
                .expect("delete close")
                    >= 1
            );
            assert!(
                conn.execute(
                    "DELETE FROM events WHERE event_type = 'host.cleanup_branch_completed'",
                    [],
                )
                .expect("delete branch events")
                    >= 4
            );
            assert!(
                conn.execute("DELETE FROM host_cleanup_branches", [])
                    .expect("delete branch projection")
                    >= 4
            );
            let accepted: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM events WHERE event_type = 'operation.accepted'",
                    [],
                    |row| row.get(0),
                )
                .expect("accepted remains");
            assert!(accepted >= 1);
            let settled: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM events WHERE event_type = 'operation.settled'",
                    [],
                    |row| row.get(0),
                )
                .expect("settled remains");
            assert!(settled >= 1);
        }
        let bus = CommandBus::open(&path).expect("reopen");
        let err = HostCleanupWorker::restart_disposition(&bus)
            .expect_err("remaining settled HostAdmission lineage must corrupt");
        assert_eq!(err, StoreError::Corruption);
    }
}

#[test]
fn host_cleanup_revision_only_terminal_scope_is_projector_corruption() {
    use devmanager::domain::event::{
        OperationFailedFact, OperationSettledFact, EVENT_SCHEMA_VERSION,
    };
    use devmanager::domain::id::EventId;
    use devmanager::domain::operation::{OperationErrorCode, OutcomeSource};
    use devmanager::host::{HostCleanupProgress, HostCleanupWorker};
    use devmanager::kernel::StoreError;

    // Settled with task_revision only.
    {
        let dir = TempDir::new().expect("tempdir");
        let path = temp_db_path(&dir);
        let (quit_op, branch_ids, final_at) = {
            let mut bus = CommandBus::open(&path).expect("open");
            let quit_op = confirm_host_quit(&mut bus);
            drive_four_cleanup_branches(&mut bus, quit_op);
            let _ = HostCleanupWorker::run_once(&mut bus).expect("ready");
            let branch_ids = host_cleanup_branch_event_ids_in_order(&path);
            let final_at: i64 = {
                let conn = open_raw(&path);
                conn.query_row(
                    "SELECT MAX(completed_at_ms) FROM host_cleanup_branches WHERE operation_id = ?1",
                    [quit_op.as_bytes().as_slice()],
                    |row| row.get(0),
                )
                .expect("max")
            };
            (quit_op, branch_ids, final_at)
        };
        {
            let conn = open_raw(&path);
            let settled = OperationSettledFact::with_source(
                quit_command_id(),
                quit_op,
                final_at + 1,
                branch_ids,
                Some(1),
                None,
                None,
                OutcomeSource::Dispatch,
            )
            .expect("fact");
            conn.execute(
                "INSERT INTO events (
                    event_id, task_id, task_revision, event_type, schema_version, occurred_at_ms, payload
                 ) VALUES (?1, NULL, ?2, 'operation.settled', ?3, ?4, ?5)",
                rusqlite::params![
                    EventId::from_bytes(fixed_uuid_v7(0x92))
                        .expect("eid")
                        .as_bytes()
                        .as_slice(),
                    1_i64,
                    i64::from(EVENT_SCHEMA_VERSION),
                    final_at + 1,
                    rmp_serde::to_vec(&settled).expect("pack"),
                ],
            )
            .expect("forge revision-only settle");
        }
        let before = host_admission_row(&path);
        let mut store = KernelStore::open(&path).expect("rebuild");
        let err = store
            .rebuild_projections()
            .expect_err("revision-only settle must fail");
        assert!(matches!(err, StoreError::Projection(_)), "got {err:?}");
        drop(store);
        assert_eq!(host_admission_row(&path), before);
    }

    // CleanupFailed with task_revision only.
    {
        let dir = TempDir::new().expect("tempdir");
        let path = temp_db_path(&dir);
        let (quit_op, final_at) = {
            let mut bus = CommandBus::open(&path).expect("open");
            let task = task_id(0x93);
            bus.execute(envelope(
                command_id(0x94),
                None,
                None,
                Command::CreateTask(create_task(task)),
            ))
            .expect("create");
            register_open_agent(&mut bus, task, command_id(0x95), 1);
            let quit_op = confirm_host_quit(&mut bus);
            drive_four_cleanup_branches(&mut bus, quit_op);
            let final_at: i64 = {
                let conn = open_raw(&path);
                conn.query_row(
                    "SELECT completed_at_ms FROM host_cleanup_branches
                     WHERE operation_id = ?1 AND branch = 'task_teardowns'",
                    [quit_op.as_bytes().as_slice()],
                    |row| row.get(0),
                )
                .expect("final")
            };
            (quit_op, final_at)
        };
        {
            let conn = open_raw(&path);
            let failed = OperationFailedFact::with_source(
                quit_command_id(),
                quit_op,
                final_at + 1,
                OperationErrorCode::CleanupFailed,
                Some(1),
                None,
                None,
                OutcomeSource::Dispatch,
            )
            .expect("fact");
            conn.execute(
                "INSERT INTO events (
                    event_id, task_id, task_revision, event_type, schema_version, occurred_at_ms, payload
                 ) VALUES (?1, NULL, ?2, 'operation.failed', ?3, ?4, ?5)",
                rusqlite::params![
                    EventId::from_bytes(fixed_uuid_v7(0x96))
                        .expect("eid")
                        .as_bytes()
                        .as_slice(),
                    1_i64,
                    i64::from(EVENT_SCHEMA_VERSION),
                    final_at + 1,
                    rmp_serde::to_vec(&failed).expect("pack"),
                ],
            )
            .expect("forge revision-only failed");
        }
        let before = host_admission_row(&path);
        let mut store = KernelStore::open(&path).expect("rebuild");
        let err = store
            .rebuild_projections()
            .expect_err("revision-only CleanupFailed must fail");
        assert!(matches!(err, StoreError::Projection(_)), "got {err:?}");
        drop(store);
        assert_eq!(host_admission_row(&path), before);
        let _ = HostCleanupProgress::Idle;
    }
}

#[test]
fn host_cleanup_matching_terminal_with_invalid_event_id_is_runtime_corruption() {
    use devmanager::host::HostCleanupWorker;
    use devmanager::kernel::StoreError;

    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let quit_op = {
        let mut bus = CommandBus::open(&path).expect("open");
        let quit_op = confirm_host_quit(&mut bus);
        drive_four_cleanup_branches(&mut bus, quit_op);
        let _ = HostCleanupWorker::run_once(&mut bus).expect("ready");
        HostCleanupWorker::settle_success(&mut bus).expect("settle");
        quit_op
    };

    {
        let conn = open_raw(&path);
        // 16 bytes, not UUID v7 (version nibble is 0).
        let invalid = [0u8; 16];
        let updated = conn
            .execute(
                "UPDATE events SET event_id = ?1 WHERE event_type = 'operation.settled'",
                [invalid.as_slice()],
            )
            .expect("corrupt event_id");
        assert_eq!(updated, 1);
    }

    let bus = CommandBus::open(&path).expect("reopen");
    let err = bus
        .operation_status(quit_op)
        .expect_err("invalid terminal event_id must corrupt without panic");
    assert_eq!(err, StoreError::Corruption);
}

#[test]
fn host_cleanup_swapped_earlier_branch_sequences_fail_runtime_and_rebuild() {
    use devmanager::domain::event::HostCleanupBranchCompletedPayload;
    use devmanager::domain::host::HostCleanupBranch;
    use devmanager::domain::operation::OperationState;
    use devmanager::host::{HostCleanupWorker, HostRestartDisposition};
    use devmanager::kernel::StoreError;

    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let (quit_op, settlement) = {
        let mut bus = CommandBus::open(&path).expect("open");
        let quit_op = confirm_host_quit(&mut bus);
        drive_four_cleanup_branches(&mut bus, quit_op);
        let _ = HostCleanupWorker::run_once(&mut bus).expect("ready");
        let settlement = HostCleanupWorker::settle_success(&mut bus).expect("settle");
        assert_eq!(
            bus.operation_status(quit_op).expect("valid status"),
            Some(OperationState::Settled {
                settled_at_ms: settlement.settled_at_ms,
                result_event_ids: settlement.result_event_ids.clone(),
            })
        );
        assert_eq!(
            HostCleanupWorker::restart_disposition(&bus).expect("closed"),
            HostRestartDisposition::Closed {
                operation_id: quit_op,
                action_epoch: 1,
                settled_at_ms: settlement.settled_at_ms,
            }
        );
        (quit_op, settlement)
    };

    {
        let conn = open_raw(&path);
        let mut stmt = conn
            .prepare(
                "SELECT sequence, payload FROM events
                 WHERE event_type = 'host.cleanup_branch_completed'
                 ORDER BY sequence ASC",
            )
            .expect("prepare");
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?))
            })
            .expect("query");
        let mut agent_seq = None;
        let mut resources_seq = None;
        let mut teardowns_seq = None;
        for row in rows {
            let (sequence, payload) = row.expect("row");
            let decoded: HostCleanupBranchCompletedPayload =
                rmp_serde::from_slice(&payload).expect("decode");
            match decoded.branch {
                HostCleanupBranch::AgentSessions => agent_seq = Some(sequence),
                HostCleanupBranch::Resources => resources_seq = Some(sequence),
                HostCleanupBranch::TaskTeardowns => teardowns_seq = Some(sequence),
                HostCleanupBranch::OutstandingEffects => {}
            }
        }
        let agent_seq = agent_seq.expect("agent branch");
        let resources_seq = resources_seq.expect("resources branch");
        let teardowns_seq = teardowns_seq.expect("teardowns branch");
        assert!(agent_seq < resources_seq && resources_seq < teardowns_seq);

        // Swap only the two earlier branch sequences; leave TaskTeardowns in place.
        // Use a temporary negative PK so sqlite_sequence is not advanced.
        conn.execute(
            "UPDATE events SET sequence = -1 WHERE sequence = ?1",
            [agent_seq],
        )
        .expect("park agent");
        conn.execute(
            "UPDATE events SET sequence = ?1 WHERE sequence = ?2",
            rusqlite::params![agent_seq, resources_seq],
        )
        .expect("move resources into agent slot");
        conn.execute(
            "UPDATE events SET sequence = ?1 WHERE sequence = -1",
            [resources_seq],
        )
        .expect("move agent into resources slot");
    }

    let mut bus = CommandBus::open(&path).expect("reopen after swap");
    let err = bus
        .operation_status(quit_op)
        .expect_err("swapped earlier branch sequences must corrupt status");
    assert_eq!(err, StoreError::Corruption);
    let err = HostCleanupWorker::restart_disposition(&bus)
        .expect_err("swapped earlier branch sequences must corrupt disposition");
    assert_eq!(err, StoreError::Corruption);
    let err = HostCleanupWorker::settle_success(&mut bus)
        .expect_err("swapped earlier branch sequences must corrupt idempotent settle");
    assert_eq!(err, StoreError::Corruption);
    drop(bus);

    let before_admission = host_admission_row(&path);
    let before_ops = count_table(&path, "operations");
    let mut store = KernelStore::open(&path).expect("rebuild store");
    let err = store
        .rebuild_projections()
        .expect_err("swapped earlier branch sequences must fail rebuild");
    assert!(
        matches!(err, StoreError::Projection(_) | StoreError::Corruption),
        "got {err:?}"
    );
    drop(store);
    assert_eq!(host_admission_row(&path), before_admission);
    assert_eq!(count_table(&path, "operations"), before_ops);
    let _ = settlement;
}
