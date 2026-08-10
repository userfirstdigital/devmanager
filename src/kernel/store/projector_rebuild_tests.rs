use std::path::Path;

use rusqlite::{Connection, OptionalExtension};
use tempfile::TempDir;

use crate::domain::agent::{AgentRole, AgentSessionFacts, AgentSessionLifecycle};
use crate::domain::event::{
    AgentSessionRegisteredPayload, OperationAcceptedFact, OperationCancelledFact,
    OperationFailedFact, OperationSettledFact, OperationUncertainFact, PrimaryAgentSetPayload,
    ResourceRegisteredPayload, ResourceReleaseBegunPayload, ResourceReleasedPayload,
    TaskCloseBegunPayload, TaskCreatedPayload, TaskUnitPayload, EVENT_SCHEMA_VERSION,
};
use crate::domain::id::{
    AgentSessionId, ClientId, CommandId, EnvironmentId, EventId, OperationId, ProjectId,
    ResourceId, TaskId,
};
use crate::domain::operation::{
    CancellationReason, OperationErrorCode, OperationUncertaintyCode, OutcomeSource,
};
use crate::domain::resource::{
    OwnerKind, ResourceFacts, ResourceKind, ResourceLifecycle, ResourceRecipe,
};
use crate::domain::task::{
    ReviewReadiness, TaskActivity, TaskAssignment, TaskAttention, TaskConnectivity, TaskFacts,
    TaskLifecycle, WorkspaceRef,
};
use crate::providers::ProviderKind;

use super::{rebuild_projection_tables_tx, KernelStore, ProjectionRebuild, StoreError};

fn rebuild_projector_only(store: &mut KernelStore) -> Result<ProjectionRebuild, StoreError> {
    let tx = store.conn.transaction()?;
    let result = rebuild_projection_tables_tx(&tx)?;
    tx.commit()?;
    Ok(result)
}

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

fn event_id(tail: u8) -> EventId {
    EventId::from_bytes(fixed_uuid_v7(tail)).expect("event id")
}

fn resource_id(tail: u8) -> ResourceId {
    ResourceId::from_bytes(fixed_uuid_v7(tail)).expect("resource id")
}

fn command_id(tail: u8) -> CommandId {
    CommandId::from_bytes(fixed_uuid_v7(tail)).expect("command id")
}

fn operation_id(tail: u8) -> OperationId {
    OperationId::from_bytes(fixed_uuid_v7(tail)).expect("operation id")
}

fn client_id(tail: u8) -> ClientId {
    ClientId::from_bytes(fixed_uuid_v7(tail)).expect("client id")
}

fn temp_db_path(dir: &TempDir) -> std::path::PathBuf {
    dir.path().join("kernel.sqlite3")
}

fn open_raw(path: &Path) -> Connection {
    let conn = Connection::open(path).expect("open raw");
    conn.execute_batch("PRAGMA foreign_keys=ON;")
        .expect("foreign_keys");
    conn
}

fn sample_task(task: TaskId) -> TaskFacts {
    TaskFacts {
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
    }
}

fn task_created_payload(task: TaskId) -> Vec<u8> {
    let payload = TaskCreatedPayload {
        task: sample_task(task),
        connectivity: TaskConnectivity::Connected,
        attention: TaskAttention::None,
        activity: TaskActivity::Idle,
        review_readiness: ReviewReadiness::NotReady,
    };
    rmp_serde::to_vec(&payload).expect("pack task.created")
}

fn insert_event(
    conn: &Connection,
    event_id: EventId,
    task_id: Option<TaskId>,
    task_revision: Option<u64>,
    event_type: &str,
    schema_version: i64,
    occurred_at_ms: i64,
    payload: &[u8],
) {
    conn.execute(
        "INSERT INTO events (
            event_id, task_id, task_revision, event_type, schema_version, occurred_at_ms, payload
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            event_id.as_bytes().as_slice(),
            task_id.map(|id| id.as_bytes().as_slice().to_vec()),
            task_revision.map(|v| i64::try_from(v).expect("revision fits")),
            event_type,
            schema_version,
            occurred_at_ms,
            payload,
        ],
    )
    .expect("insert event");
}

fn insert_event_at_sequence(
    conn: &Connection,
    sequence: i64,
    event_id: EventId,
    task_id: Option<TaskId>,
    task_revision: Option<u64>,
    event_type: &str,
    schema_version: i64,
    occurred_at_ms: i64,
    payload: &[u8],
) {
    conn.execute(
        "INSERT INTO events (
            sequence, event_id, task_id, task_revision, event_type, schema_version,
            occurred_at_ms, payload
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        rusqlite::params![
            sequence,
            event_id.as_bytes().as_slice(),
            task_id.map(|id| id.as_bytes().as_slice().to_vec()),
            task_revision.map(|v| i64::try_from(v).expect("revision fits")),
            event_type,
            schema_version,
            occurred_at_ms,
            payload,
        ],
    )
    .expect("insert event at sequence");
}

fn seed_task_created(conn: &Connection, task: TaskId, eid: EventId, occurred_at_ms: i64) {
    insert_event(
        conn,
        eid,
        Some(task),
        Some(1),
        "task.created",
        i64::from(EVENT_SCHEMA_VERSION),
        occurred_at_ms,
        &task_created_payload(task),
    );
}

fn seed_accepted_operation(
    path: &Path,
    task: TaskId,
    cmd: CommandId,
    op: OperationId,
    resource: Option<(ResourceId, u64)>,
    action_epoch: Option<u64>,
) {
    let conn = open_raw(path);
    // Projector-only fixtures retain a minimal FK parent; they intentionally do
    // not exercise command-receipt codecs or active outbox metadata.
    conn.execute(
        "INSERT INTO command_receipts(
            command_id, client_id, task_id, receipt, committed_sequence, created_at_ms
         ) VALUES (?1, ?2, ?3, ?4, 1, 1)",
        rusqlite::params![
            cmd.as_bytes().as_slice(),
            client_id(0x01).as_bytes().as_slice(),
            task.as_bytes().as_slice(),
            vec![1u8, 2, 3],
        ],
    )
    .expect("receipt");
    insert_event(
        &conn,
        event_id(0xE0),
        Some(task),
        Some(1),
        "task.created",
        i64::from(EVENT_SCHEMA_VERSION),
        1_000,
        &task_created_payload(task),
    );
    let (resource_id, runtime_generation) = match resource {
        Some((id, gen)) => (Some(id), Some(gen)),
        None => (None, None),
    };
    let accepted = OperationAcceptedFact::new(
        cmd,
        op,
        1_100,
        action_epoch,
        resource_id,
        runtime_generation,
    )
    .expect("accepted");
    insert_event(
        &conn,
        event_id(0xE1),
        Some(task),
        None,
        "operation.accepted",
        i64::from(EVENT_SCHEMA_VERSION),
        1_100,
        &rmp_serde::to_vec(&accepted).unwrap(),
    );
}

fn insert_terminal_outbox_row(
    conn: &Connection,
    op: OperationId,
    effect_index: i64,
    state: &str,
    last_error_class: Option<&str>,
) {
    let outbox_id = fixed_uuid_v7(0xCD);
    conn.execute(
        "INSERT INTO outbox(
            outbox_id, operation_id, effect_index, event_sequence, destination_class,
            replay_policy, payload, state, available_at_ms, leased_until_ms,
            dispatch_started_at_ms, attempts, last_error_class
         ) VALUES (?1, ?2, ?3, 2, 'task_teardown', 'reconcile_before_retry', X'00',
                   ?4, 1100, NULL, NULL, 0, ?5)",
        rusqlite::params![
            outbox_id.as_slice(),
            op.as_bytes().as_slice(),
            effect_index,
            state,
            last_error_class,
        ],
    )
    .expect("seed outbox");
}

#[test]
fn command_contract_projector_accepts_dispatch_from_accepted_only() {
    use crate::domain::operation::OutcomeSource;

    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    drop(KernelStore::open(&path).expect("open"));

    let task = task_id(0x40);
    let cmd = command_id(0x41);
    let op = operation_id(0x42);
    seed_accepted_operation(&path, task, cmd, op, None, Some(1));

    let mut store = KernelStore::open(&path).expect("reopen");
    rebuild_projector_only(&mut store).expect("accept rebuild");
    drop(store);

    {
        let conn = open_raw(&path);
        insert_event(
            &conn,
            event_id(0xE7),
            Some(task),
            Some(2),
            "task.close_begun",
            i64::from(EVENT_SCHEMA_VERSION),
            1_150,
            &rmp_serde::to_vec(&TaskCloseBegunPayload { action_epoch: 1 }).unwrap(),
        );
        let archive_id = event_id(0x90);
        insert_event(
            &conn,
            archive_id,
            Some(task),
            Some(3),
            "task.archived",
            i64::from(EVENT_SCHEMA_VERSION),
            1_200,
            &rmp_serde::to_vec(&TaskUnitPayload {}).unwrap(),
        );
        let settled =
            OperationSettledFact::new(cmd, op, 1_200, vec![archive_id], Some(1), None, None)
                .expect("dispatch settled");
        assert_eq!(settled.source, OutcomeSource::Dispatch);
        insert_event(
            &conn,
            event_id(0xE2),
            Some(task),
            None,
            "operation.settled",
            i64::from(EVENT_SCHEMA_VERSION),
            1_200,
            &rmp_serde::to_vec(&settled).unwrap(),
        );
    }

    let mut store = KernelStore::open(&path).expect("reopen");
    rebuild_projector_only(&mut store).expect("dispatch settle");
    drop(store);

    let conn = open_raw(&path);
    let state: String = conn
        .query_row(
            "SELECT state FROM operations WHERE operation_id = ?1",
            [op.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(state, "settled");
}

#[test]
fn command_contract_projector_rejects_reconciliation_from_accepted() {
    use crate::domain::operation::OutcomeSource;

    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    drop(KernelStore::open(&path).expect("open"));

    let task = task_id(0x50);
    let cmd = command_id(0x51);
    let op = operation_id(0x52);
    seed_accepted_operation(&path, task, cmd, op, None, None);

    let mut store = KernelStore::open(&path).expect("reopen");
    rebuild_projector_only(&mut store).expect("accept rebuild");
    drop(store);

    {
        let conn = open_raw(&path);
        let settled = OperationSettledFact::with_source(
            cmd,
            op,
            1_200,
            vec![event_id(0x91)],
            None,
            None,
            None,
            OutcomeSource::verified_reconciliation(0, "ext-too-early").expect("identity"),
        )
        .expect("reconciled settled");
        insert_event(
            &conn,
            event_id(0xE3),
            Some(task),
            None,
            "operation.settled",
            i64::from(EVENT_SCHEMA_VERSION),
            1_200,
            &rmp_serde::to_vec(&settled).unwrap(),
        );
    }

    let mut store = KernelStore::open(&path).expect("reopen");
    let err = rebuild_projector_only(&mut store)
        .expect_err("accepted cannot take verified reconciliation");
    assert!(
        matches!(err, StoreError::Projection(_)),
        "expected projection fence, got {err:?}"
    );
}

#[test]
fn command_contract_projector_uncertain_resolves_only_via_reconciliation() {
    use crate::domain::event::OperationUncertainFact;
    use crate::domain::operation::{OperationUncertaintyCode, OutcomeSource};

    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    drop(KernelStore::open(&path).expect("open"));

    let task = task_id(0x60);
    let cmd = command_id(0x61);
    let op = operation_id(0x62);
    seed_accepted_operation(&path, task, cmd, op, None, Some(1));

    let mut store = KernelStore::open(&path).expect("reopen");
    rebuild_projector_only(&mut store).expect("accept rebuild");
    drop(store);

    {
        let conn = open_raw(&path);
        let uncertain = OperationUncertainFact::new(
            cmd,
            op,
            1_200,
            OperationUncertaintyCode::AmbiguousDispatch,
            Some(1),
            None,
            None,
        )
        .expect("uncertain");
        insert_event(
            &conn,
            event_id(0xE4),
            Some(task),
            None,
            "operation.uncertain",
            i64::from(EVENT_SCHEMA_VERSION),
            1_200,
            &rmp_serde::to_vec(&uncertain).unwrap(),
        );
    }

    let mut store = KernelStore::open(&path).expect("reopen");
    rebuild_projector_only(&mut store).expect("uncertain rebuild");
    drop(store);

    {
        let conn = open_raw(&path);
        let dispatch_settle =
            OperationSettledFact::new(cmd, op, 1_300, vec![event_id(0x92)], Some(1), None, None)
                .expect("dispatch");
        insert_event(
            &conn,
            event_id(0xE5),
            Some(task),
            None,
            "operation.settled",
            i64::from(EVENT_SCHEMA_VERSION),
            1_300,
            &rmp_serde::to_vec(&dispatch_settle).unwrap(),
        );
    }

    let mut store = KernelStore::open(&path).expect("reopen");
    let err =
        rebuild_projector_only(&mut store).expect_err("uncertain rejects dispatch settlement");
    assert!(
        matches!(err, StoreError::Projection(_)),
        "expected projection error, got {err:?}"
    );
    drop(store);

    {
        let conn = open_raw(&path);
        conn.execute(
            "DELETE FROM events WHERE event_type = 'operation.settled'",
            [],
        )
        .unwrap();
        let archive_id = event_id(0x93);
        insert_event(
            &conn,
            event_id(0xE8),
            Some(task),
            Some(2),
            "task.close_begun",
            i64::from(EVENT_SCHEMA_VERSION),
            1_350,
            &rmp_serde::to_vec(&TaskCloseBegunPayload { action_epoch: 1 }).unwrap(),
        );
        insert_event(
            &conn,
            archive_id,
            Some(task),
            Some(3),
            "task.archived",
            i64::from(EVENT_SCHEMA_VERSION),
            1_400,
            &rmp_serde::to_vec(&TaskUnitPayload {}).unwrap(),
        );
        let reconciled = OperationSettledFact::with_source(
            cmd,
            op,
            1_400,
            vec![archive_id],
            Some(1),
            None,
            None,
            OutcomeSource::verified_reconciliation(0, "provider:proof").expect("identity"),
        )
        .expect("reconciled");
        insert_event(
            &conn,
            event_id(0xE6),
            Some(task),
            None,
            "operation.settled",
            i64::from(EVENT_SCHEMA_VERSION),
            1_400,
            &rmp_serde::to_vec(&reconciled).unwrap(),
        );
        insert_terminal_outbox_row(&conn, op, 0, "settled", None);
    }

    let mut store = KernelStore::open(&path).expect("reopen");
    rebuild_projector_only(&mut store).expect("uncertain resolves via verified reconciliation");
    drop(store);

    let conn = open_raw(&path);
    let state: String = conn
        .query_row(
            "SELECT state FROM operations WHERE operation_id = ?1",
            [op.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(state, "settled");
}

#[test]
fn schema_rebuild_rejects_envelope_and_outcome_chronology_mismatches() {
    // Accepted envelope occurred_at != fact.accepted_at_ms
    {
        let dir = TempDir::new().expect("tempdir");
        let path = temp_db_path(&dir);
        drop(KernelStore::open(&path).expect("open"));
        let task = task_id(0x90);
        let cmd = command_id(0x91);
        let op = operation_id(0x92);
        seed_accepted_operation(&path, task, cmd, op, None, None);
        let conn = open_raw(&path);
        conn.execute(
            "UPDATE events SET occurred_at_ms = 9999 WHERE event_type = 'operation.accepted'",
            [],
        )
        .unwrap();
        drop(conn);
        let mut store = KernelStore::open(&path).expect("reopen");
        let err = rebuild_projector_only(&mut store).expect_err("accepted envelope");
        assert!(matches!(err, StoreError::Projection(_)), "{err:?}");
    }

    // Outcome before acceptance
    {
        let dir = TempDir::new().expect("tempdir");
        let path = temp_db_path(&dir);
        drop(KernelStore::open(&path).expect("open"));
        let task = task_id(0x93);
        let cmd = command_id(0x94);
        let op = operation_id(0x95);
        seed_accepted_operation(&path, task, cmd, op, None, None);
        let mut store = KernelStore::open(&path).expect("reopen");
        rebuild_projector_only(&mut store).expect("accept rebuild");
        drop(store);
        let conn = open_raw(&path);
        let failed = OperationFailedFact::new(
            cmd,
            op,
            1_050,
            OperationErrorCode::SideEffectFailed,
            None,
            None,
            None,
        )
        .unwrap();
        insert_event(
            &conn,
            event_id(0x96),
            Some(task),
            None,
            "operation.failed",
            i64::from(EVENT_SCHEMA_VERSION),
            1_050,
            &rmp_serde::to_vec(&failed).unwrap(),
        );
        drop(conn);
        let mut store = KernelStore::open(&path).expect("reopen");
        let err = rebuild_projector_only(&mut store).expect_err("before acceptance");
        assert!(matches!(err, StoreError::Projection(_)), "{err:?}");
    }

    // Verified reconciliation cannot predate the uncertainty it resolves.
    {
        let dir = TempDir::new().expect("tempdir");
        let path = temp_db_path(&dir);
        drop(KernelStore::open(&path).expect("open"));
        let task = task_id(0x97);
        let cmd = command_id(0x99);
        let op = operation_id(0x9A);
        seed_accepted_operation(&path, task, cmd, op, None, Some(1));
        let mut store = KernelStore::open(&path).expect("reopen accepted history");
        rebuild_projector_only(&mut store).expect("project accepted history");
        drop(store);

        let conn = open_raw(&path);
        let uncertain = OperationUncertainFact::new(
            cmd,
            op,
            1_200,
            OperationUncertaintyCode::AmbiguousDispatch,
            Some(1),
            None,
            None,
        )
        .expect("uncertain");
        insert_event(
            &conn,
            event_id(0x9B),
            Some(task),
            None,
            "operation.uncertain",
            i64::from(EVENT_SCHEMA_VERSION),
            1_200,
            &rmp_serde::to_vec(&uncertain).unwrap(),
        );
        let failed = OperationFailedFact::with_source(
            cmd,
            op,
            1_199,
            OperationErrorCode::SideEffectFailed,
            Some(1),
            None,
            None,
            OutcomeSource::verified_reconciliation(0, "ext").unwrap(),
        )
        .unwrap();
        insert_event(
            &conn,
            event_id(0x9C),
            Some(task),
            None,
            "operation.failed",
            i64::from(EVENT_SCHEMA_VERSION),
            1_199,
            &rmp_serde::to_vec(&failed).unwrap(),
        );
        insert_terminal_outbox_row(&conn, op, 0, "failed", Some("side_effect_failed"));
        drop(conn);

        let mut store = KernelStore::open(&path).expect("reopen");
        let err = rebuild_projector_only(&mut store).expect_err("reconcile before uncertain");
        assert!(matches!(err, StoreError::Projection(_)), "{err:?}");
    }
}

#[test]
fn schema_rebuild_side_effect_settled_result_lineage() {
    // Orphan task.archived rejected.
    {
        let dir = TempDir::new().expect("tempdir");
        let path = temp_db_path(&dir);
        drop(KernelStore::open(&path).expect("open"));
        let task = task_id(0xA0);
        let conn = open_raw(&path);
        seed_task_created(&conn, task, event_id(0xA1), 1_000);
        insert_event(
            &conn,
            event_id(0xA2),
            Some(task),
            Some(2),
            "task.close_begun",
            i64::from(EVENT_SCHEMA_VERSION),
            1_100,
            &rmp_serde::to_vec(&TaskCloseBegunPayload { action_epoch: 1 }).unwrap(),
        );
        insert_event(
            &conn,
            event_id(0xA3),
            Some(task),
            Some(3),
            "task.archived",
            i64::from(EVENT_SCHEMA_VERSION),
            1_200,
            &rmp_serde::to_vec(&TaskUnitPayload {}).unwrap(),
        );
        drop(conn);
        let mut store = KernelStore::open(&path).expect("reopen");
        let err = rebuild_projector_only(&mut store).expect_err("orphan archive");
        assert!(matches!(err, StoreError::Projection(_)), "{err:?}");
    }

    // Side-effect settled missing/wrong/nonadjacent derived result.
    {
        let dir = TempDir::new().expect("tempdir");
        let path = temp_db_path(&dir);
        drop(KernelStore::open(&path).expect("open"));
        let task = task_id(0xA4);
        let cmd = command_id(0xA5);
        let op = operation_id(0xA6);
        seed_accepted_operation(&path, task, cmd, op, None, Some(1));
        let mut store = KernelStore::open(&path).expect("reopen");
        rebuild_projector_only(&mut store).expect("accept");
        drop(store);
        // Inject close_begun + accepted already; force closing via extra close is wrong.
        // Rebuild seed only has created+accepted. Add close_begun then settled without archive.
        let conn = open_raw(&path);
        insert_event(
            &conn,
            event_id(0xA7),
            Some(task),
            Some(2),
            "task.close_begun",
            i64::from(EVENT_SCHEMA_VERSION),
            1_150,
            &rmp_serde::to_vec(&TaskCloseBegunPayload { action_epoch: 1 }).unwrap(),
        );
        // Update accepted fence epoch in projection is rebuilt from events — accepted has epoch 1.
        let settled =
            OperationSettledFact::new(cmd, op, 1_200, vec![event_id(0xA8)], Some(1), None, None)
                .unwrap();
        insert_event(
            &conn,
            event_id(0xA9),
            Some(task),
            None,
            "operation.settled",
            i64::from(EVENT_SCHEMA_VERSION),
            1_200,
            &rmp_serde::to_vec(&settled).unwrap(),
        );
        drop(conn);
        let mut store = KernelStore::open(&path).expect("reopen");
        let err = rebuild_projector_only(&mut store).expect_err("settled without adjacent archive");
        assert!(matches!(err, StoreError::Projection(_)), "{err:?}");
    }

    // Valid archive + settle pair rebuilds.
    {
        let dir = TempDir::new().expect("tempdir");
        let path = temp_db_path(&dir);
        drop(KernelStore::open(&path).expect("open"));
        let task = task_id(0xAA);
        let cmd = command_id(0xAB);
        let op = operation_id(0xAC);
        seed_accepted_operation(&path, task, cmd, op, None, Some(1));
        let mut store = KernelStore::open(&path).expect("reopen");
        rebuild_projector_only(&mut store).expect("accept");
        drop(store);
        let conn = open_raw(&path);
        insert_event(
            &conn,
            event_id(0xAD),
            Some(task),
            Some(2),
            "task.close_begun",
            i64::from(EVENT_SCHEMA_VERSION),
            1_150,
            &rmp_serde::to_vec(&TaskCloseBegunPayload { action_epoch: 1 }).unwrap(),
        );
        let archive_id = event_id(0xAE);
        insert_event(
            &conn,
            archive_id,
            Some(task),
            Some(3),
            "task.archived",
            i64::from(EVENT_SCHEMA_VERSION),
            1_200,
            &rmp_serde::to_vec(&TaskUnitPayload {}).unwrap(),
        );
        let settled =
            OperationSettledFact::new(cmd, op, 1_200, vec![archive_id], Some(1), None, None)
                .unwrap();
        insert_event(
            &conn,
            event_id(0xAF),
            Some(task),
            None,
            "operation.settled",
            i64::from(EVENT_SCHEMA_VERSION),
            1_200,
            &rmp_serde::to_vec(&settled).unwrap(),
        );
        drop(conn);
        let mut store = KernelStore::open(&path).expect("reopen");
        rebuild_projector_only(&mut store).expect("valid pair");
    }
}

#[test]
fn schema_rebuild_accepts_adjacent_derived_settle_with_gap_before_pair() {
    // Gap before the pair is fine; archive and settle remain immediately adjacent existing rows.
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    drop(KernelStore::open(&path).expect("open"));
    let task = task_id(0xB6);
    let cmd = command_id(0xB7);
    let op = operation_id(0xB8);
    seed_accepted_operation(&path, task, cmd, op, None, Some(1));
    let mut store = KernelStore::open(&path).expect("reopen");
    rebuild_projector_only(&mut store).expect("accept rebuild");
    drop(store);
    let conn = open_raw(&path);
    insert_event(
        &conn,
        event_id(0xB9),
        Some(task),
        Some(2),
        "task.close_begun",
        i64::from(EVENT_SCHEMA_VERSION),
        1_150,
        &rmp_serde::to_vec(&TaskCloseBegunPayload { action_epoch: 1 }).unwrap(),
    );
    let archive_id = event_id(0xBA);
    insert_event_at_sequence(
        &conn,
        20,
        archive_id,
        Some(task),
        Some(3),
        "task.archived",
        i64::from(EVENT_SCHEMA_VERSION),
        1_200,
        &rmp_serde::to_vec(&TaskUnitPayload {}).unwrap(),
    );
    let settled =
        OperationSettledFact::new(cmd, op, 1_200, vec![archive_id], Some(1), None, None).unwrap();
    insert_event_at_sequence(
        &conn,
        21,
        event_id(0xBB),
        Some(task),
        None,
        "operation.settled",
        i64::from(EVENT_SCHEMA_VERSION),
        1_200,
        &rmp_serde::to_vec(&settled).unwrap(),
    );
    let sequences: Vec<i64> = {
        let mut stmt = conn
            .prepare("SELECT sequence FROM events ORDER BY sequence ASC")
            .unwrap();
        stmt.query_map([], |row| row.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    };
    assert_eq!(sequences, vec![1, 2, 3, 20, 21]);
    drop(conn);
    let mut store = KernelStore::open(&path).expect("reopen");
    rebuild_projector_only(&mut store)
        .expect("adjacent derived+settle remains valid with earlier gap");
}

#[test]
fn schema_rebuild_rejects_unsupported_operation_fence_shapes() {
    // Supported: pure all-none, teardown action-only, release action+resource+generation.
    // Resource pair with action_epoch None must fail at accepted (and non-settled outcomes).
    {
        let dir = TempDir::new().expect("tempdir");
        let path = temp_db_path(&dir);
        drop(KernelStore::open(&path).expect("open"));
        let task = task_id(0xE4);
        let cmd = command_id(0xE5);
        let op = operation_id(0xE6);
        let resource = resource_id(0xE7);
        seed_accepted_operation(&path, task, cmd, op, Some((resource, 3)), None);
        let mut store = KernelStore::open(&path).expect("reopen");
        let err = rebuild_projector_only(&mut store)
            .expect_err("accepted resource fence without action_epoch");
        assert!(matches!(err, StoreError::Projection(_)), "{err:?}");
    }

    {
        let dir = TempDir::new().expect("tempdir");
        let path = temp_db_path(&dir);
        drop(KernelStore::open(&path).expect("open"));
        let task = task_id(0xE8);
        let cmd = command_id(0xE9);
        let op = operation_id(0xEA);
        seed_accepted_operation(&path, task, cmd, op, None, None);
        let mut store = KernelStore::open(&path).expect("reopen");
        rebuild_projector_only(&mut store).expect("pure accept");
        drop(store);
        let conn = open_raw(&path);
        let cancelled = OperationCancelledFact::new(
            cmd,
            op,
            1_200,
            CancellationReason::Superseded,
            None,
            Some(resource_id(0xEB)),
            Some(1),
        )
        .unwrap();
        insert_event(
            &conn,
            event_id(0xEC),
            Some(task),
            None,
            "operation.cancelled",
            i64::from(EVENT_SCHEMA_VERSION),
            1_200,
            &rmp_serde::to_vec(&cancelled).unwrap(),
        );
        drop(conn);
        let mut store = KernelStore::open(&path).expect("reopen");
        let err = rebuild_projector_only(&mut store)
            .expect_err("cancelled resource fence without action_epoch");
        assert!(matches!(err, StoreError::Projection(_)), "{err:?}");
    }

    // Valid shapes still rebuild.
    for (label, resource, action_epoch) in [
        ("pure", None, None),
        ("teardown", None, Some(1u64)),
        ("release", Some((resource_id(0xED), 2u64)), Some(0u64)),
    ] {
        let dir = TempDir::new().expect("tempdir");
        let path = temp_db_path(&dir);
        drop(KernelStore::open(&path).expect("open"));
        let task = task_id(0xEE);
        let cmd = command_id(0xEF);
        let op = operation_id(0xF0);
        seed_accepted_operation(&path, task, cmd, op, resource, action_epoch);
        let mut store = KernelStore::open(&path).expect("reopen");
        rebuild_projector_only(&mut store)
            .unwrap_or_else(|err| panic!("{label} accept must rebuild: {err:?}"));
    }
}

#[test]
fn schema_rebuild_rejects_task_revision_on_non_mutating_operation_envelopes() {
    // Accepted with Some revision.
    {
        let dir = TempDir::new().expect("tempdir");
        let path = temp_db_path(&dir);
        drop(KernelStore::open(&path).expect("open"));
        let task = task_id(0xF1);
        let cmd = command_id(0xF2);
        let op = operation_id(0xF3);
        let conn = open_raw(&path);
        conn.execute(
            "INSERT INTO command_receipts(
                command_id, client_id, task_id, receipt, committed_sequence, created_at_ms
             ) VALUES (?1, ?2, ?3, X'00', 1, 1)",
            rusqlite::params![
                cmd.as_bytes().as_slice(),
                client_id(0x01).as_bytes().as_slice(),
                task.as_bytes().as_slice(),
            ],
        )
        .unwrap();
        seed_task_created(&conn, task, event_id(0xF4), 1_000);
        let accepted = OperationAcceptedFact::new(cmd, op, 1_100, None, None, None).unwrap();
        insert_event(
            &conn,
            event_id(0xF5),
            Some(task),
            Some(2), // non-mutation must be NULL
            "operation.accepted",
            i64::from(EVENT_SCHEMA_VERSION),
            1_100,
            &rmp_serde::to_vec(&accepted).unwrap(),
        );
        drop(conn);
        let mut store = KernelStore::open(&path).expect("reopen");
        let err =
            rebuild_projector_only(&mut store).expect_err("accepted requires NULL task_revision");
        assert!(matches!(err, StoreError::Projection(_)), "{err:?}");
    }

    // Adjacent archive + settle, but settle envelope carries Some revision.
    {
        let dir = TempDir::new().expect("tempdir");
        let path = temp_db_path(&dir);
        drop(KernelStore::open(&path).expect("open"));
        let task = task_id(0xF6);
        let cmd = command_id(0xF7);
        let op = operation_id(0xF8);
        seed_accepted_operation(&path, task, cmd, op, None, Some(1));
        let mut store = KernelStore::open(&path).expect("reopen");
        rebuild_projector_only(&mut store).expect("accept");
        drop(store);
        let conn = open_raw(&path);
        insert_event(
            &conn,
            event_id(0xF9),
            Some(task),
            Some(2),
            "task.close_begun",
            i64::from(EVENT_SCHEMA_VERSION),
            1_150,
            &rmp_serde::to_vec(&TaskCloseBegunPayload { action_epoch: 1 }).unwrap(),
        );
        let archive_id = event_id(0xFA);
        insert_event(
            &conn,
            archive_id,
            Some(task),
            Some(3),
            "task.archived",
            i64::from(EVENT_SCHEMA_VERSION),
            1_200,
            &rmp_serde::to_vec(&TaskUnitPayload {}).unwrap(),
        );
        let settled =
            OperationSettledFact::new(cmd, op, 1_200, vec![archive_id], Some(1), None, None)
                .unwrap();
        insert_event(
            &conn,
            event_id(0xFB),
            Some(task),
            Some(4), // must be None for operation.settled
            "operation.settled",
            i64::from(EVENT_SCHEMA_VERSION),
            1_200,
            &rmp_serde::to_vec(&settled).unwrap(),
        );
        drop(conn);
        let mut store = KernelStore::open(&path).expect("reopen");
        let err =
            rebuild_projector_only(&mut store).expect_err("settled requires NULL task_revision");
        assert!(matches!(err, StoreError::Projection(_)), "{err:?}");
    }
}

#[test]
fn schema_rebuild_verified_reconciliation_requires_matching_outbox_identity_and_state() {
    // effect_index=1 against durable index-0 outbox must fail closed.
    {
        let dir = TempDir::new().expect("tempdir");
        let path = temp_db_path(&dir);
        drop(KernelStore::open(&path).expect("open"));
        let task = task_id(0x10);
        let cmd = command_id(0x11);
        let op = operation_id(0x12);
        seed_accepted_operation(&path, task, cmd, op, None, Some(1));
        let mut store = KernelStore::open(&path).expect("reopen");
        rebuild_projector_only(&mut store).expect("accept");
        drop(store);
        let conn = open_raw(&path);
        let uncertain = OperationUncertainFact::new(
            cmd,
            op,
            1_200,
            OperationUncertaintyCode::AmbiguousDispatch,
            Some(1),
            None,
            None,
        )
        .unwrap();
        insert_event(
            &conn,
            event_id(0x13),
            Some(task),
            None,
            "operation.uncertain",
            i64::from(EVENT_SCHEMA_VERSION),
            1_200,
            &rmp_serde::to_vec(&uncertain).unwrap(),
        );
        let reconciled = OperationSettledFact::with_source(
            cmd,
            op,
            1_300,
            vec![event_id(0x14)],
            Some(1),
            None,
            None,
            OutcomeSource::verified_reconciliation(1, "provider:wrong-index").unwrap(),
        )
        .unwrap();
        insert_event(
            &conn,
            event_id(0x15),
            Some(task),
            None,
            "operation.settled",
            i64::from(EVENT_SCHEMA_VERSION),
            1_300,
            &rmp_serde::to_vec(&reconciled).unwrap(),
        );
        insert_terminal_outbox_row(&conn, op, 0, "settled", None);
        drop(conn);
        let mut store = KernelStore::open(&path).expect("reopen");
        let err =
            rebuild_projector_only(&mut store).expect_err("effect_index must match durable outbox");
        assert!(matches!(err, StoreError::Projection(_)), "{err:?}");
    }

    // Consistently impossible: fact and sole outbox row both at index 1.
    {
        let dir = TempDir::new().expect("tempdir");
        let path = temp_db_path(&dir);
        drop(KernelStore::open(&path).expect("open"));
        let task = task_id(0x21);
        let cmd = command_id(0x22);
        let op = operation_id(0x23);
        seed_accepted_operation(&path, task, cmd, op, None, Some(1));
        let mut store = KernelStore::open(&path).expect("reopen");
        rebuild_projector_only(&mut store).expect("accept");
        drop(store);
        let conn = open_raw(&path);
        let uncertain = OperationUncertainFact::new(
            cmd,
            op,
            1_200,
            OperationUncertaintyCode::AmbiguousDispatch,
            Some(1),
            None,
            None,
        )
        .unwrap();
        insert_event(
            &conn,
            event_id(0x24),
            Some(task),
            None,
            "operation.uncertain",
            i64::from(EVENT_SCHEMA_VERSION),
            1_200,
            &rmp_serde::to_vec(&uncertain).unwrap(),
        );
        let reconciled = OperationSettledFact::with_source(
            cmd,
            op,
            1_300,
            vec![event_id(0x25)],
            Some(1),
            None,
            None,
            OutcomeSource::verified_reconciliation(1, "provider:both-one").unwrap(),
        )
        .unwrap();
        insert_event(
            &conn,
            event_id(0x26),
            Some(task),
            None,
            "operation.settled",
            i64::from(EVENT_SCHEMA_VERSION),
            1_300,
            &rmp_serde::to_vec(&reconciled).unwrap(),
        );
        insert_terminal_outbox_row(&conn, op, 1, "settled", None);
        drop(conn);
        let mut store = KernelStore::open(&path).expect("reopen");
        let err = rebuild_projector_only(&mut store).expect_err("V1 effect_index must be 0");
        assert!(matches!(err, StoreError::Projection(_)), "{err:?}");
    }

    // Failed reconciliation while retained outbox remains uncertain must fail (shadow expects final).
    {
        let dir = TempDir::new().expect("tempdir");
        let path = temp_db_path(&dir);
        drop(KernelStore::open(&path).expect("open"));
        let task = task_id(0x16);
        let cmd = command_id(0x17);
        let op = operation_id(0x18);
        seed_accepted_operation(&path, task, cmd, op, None, Some(1));
        let mut store = KernelStore::open(&path).expect("reopen");
        rebuild_projector_only(&mut store).expect("accept");
        drop(store);
        let conn = open_raw(&path);
        let uncertain = OperationUncertainFact::new(
            cmd,
            op,
            1_200,
            OperationUncertaintyCode::AmbiguousDispatch,
            Some(1),
            None,
            None,
        )
        .unwrap();
        insert_event(
            &conn,
            event_id(0x19),
            Some(task),
            None,
            "operation.uncertain",
            i64::from(EVENT_SCHEMA_VERSION),
            1_200,
            &rmp_serde::to_vec(&uncertain).unwrap(),
        );
        let failed = OperationFailedFact::with_source(
            cmd,
            op,
            1_300,
            OperationErrorCode::SideEffectFailed,
            Some(1),
            None,
            None,
            OutcomeSource::verified_reconciliation(0, "provider:failed").unwrap(),
        )
        .unwrap();
        insert_event(
            &conn,
            event_id(0x1A),
            Some(task),
            None,
            "operation.failed",
            i64::from(EVENT_SCHEMA_VERSION),
            1_300,
            &rmp_serde::to_vec(&failed).unwrap(),
        );
        insert_terminal_outbox_row(&conn, op, 0, "uncertain", Some("ambiguous_dispatch"));
        drop(conn);
        let mut store = KernelStore::open(&path).expect("reopen");
        let err = rebuild_projector_only(&mut store)
            .expect_err("shadow rebuild requires final outbox state");
        assert!(matches!(err, StoreError::Projection(_)), "{err:?}");
    }

    // Matching identity + final settled outbox: uncertain replay then verified settle is green.
    {
        let dir = TempDir::new().expect("tempdir");
        let path = temp_db_path(&dir);
        drop(KernelStore::open(&path).expect("open"));
        let task = task_id(0x1B);
        let cmd = command_id(0x1C);
        let op = operation_id(0x1D);
        seed_accepted_operation(&path, task, cmd, op, None, Some(1));
        let mut store = KernelStore::open(&path).expect("reopen");
        rebuild_projector_only(&mut store).expect("accept");
        drop(store);
        let conn = open_raw(&path);
        let uncertain = OperationUncertainFact::new(
            cmd,
            op,
            1_200,
            OperationUncertaintyCode::AmbiguousDispatch,
            Some(1),
            None,
            None,
        )
        .unwrap();
        insert_event(
            &conn,
            event_id(0x1E),
            Some(task),
            None,
            "operation.uncertain",
            i64::from(EVENT_SCHEMA_VERSION),
            1_200,
            &rmp_serde::to_vec(&uncertain).unwrap(),
        );
        insert_event(
            &conn,
            event_id(0x27),
            Some(task),
            Some(2),
            "task.close_begun",
            i64::from(EVENT_SCHEMA_VERSION),
            1_350,
            &rmp_serde::to_vec(&TaskCloseBegunPayload { action_epoch: 1 }).unwrap(),
        );
        let archive_id = event_id(0x1F);
        insert_event(
            &conn,
            archive_id,
            Some(task),
            Some(3),
            "task.archived",
            i64::from(EVENT_SCHEMA_VERSION),
            1_400,
            &rmp_serde::to_vec(&TaskUnitPayload {}).unwrap(),
        );
        let reconciled = OperationSettledFact::with_source(
            cmd,
            op,
            1_400,
            vec![archive_id],
            Some(1),
            None,
            None,
            OutcomeSource::verified_reconciliation(0, "provider:proof").unwrap(),
        )
        .unwrap();
        insert_event(
            &conn,
            event_id(0x20),
            Some(task),
            None,
            "operation.settled",
            i64::from(EVENT_SCHEMA_VERSION),
            1_400,
            &rmp_serde::to_vec(&reconciled).unwrap(),
        );
        insert_terminal_outbox_row(&conn, op, 0, "settled", None);
        drop(conn);
        let mut store = KernelStore::open(&path).expect("reopen");
        rebuild_projector_only(&mut store)
            .expect("matching outbox allows uncertain then verified settle");
    }
}

#[test]
fn schema_rebuild_rejects_pure_non_settled_and_invalid_pure_settle() {
    // Pure all-none may not become Failed/Cancelled/Uncertain.
    {
        let dir = TempDir::new().expect("tempdir");
        let path = temp_db_path(&dir);
        drop(KernelStore::open(&path).expect("open"));
        let task = task_id(0x30);
        let cmd = command_id(0x31);
        let op = operation_id(0x32);
        seed_accepted_operation(&path, task, cmd, op, None, None);
        let mut store = KernelStore::open(&path).expect("reopen");
        rebuild_projector_only(&mut store).expect("pure accept");
        drop(store);
        let conn = open_raw(&path);
        let failed = OperationFailedFact::new(
            cmd,
            op,
            1_200,
            OperationErrorCode::SideEffectFailed,
            None,
            None,
            None,
        )
        .unwrap();
        insert_event(
            &conn,
            event_id(0x33),
            Some(task),
            None,
            "operation.failed",
            i64::from(EVENT_SCHEMA_VERSION),
            1_200,
            &rmp_serde::to_vec(&failed).unwrap(),
        );
        drop(conn);
        let mut store = KernelStore::open(&path).expect("reopen");
        let err = rebuild_projector_only(&mut store).expect_err("pure cannot fail");
        assert!(matches!(err, StoreError::Projection(_)), "{err:?}");
    }

    // Pure settled must use Dispatch, accepted_at == settled_at, and exact prior decision IDs.
    {
        let dir = TempDir::new().expect("tempdir");
        let path = temp_db_path(&dir);
        drop(KernelStore::open(&path).expect("open"));
        let task = task_id(0x34);
        let cmd = command_id(0x35);
        let op = operation_id(0x36);
        seed_accepted_operation(&path, task, cmd, op, None, None);
        let mut store = KernelStore::open(&path).expect("reopen");
        rebuild_projector_only(&mut store).expect("pure accept");
        drop(store);
        let conn = open_raw(&path);
        let settled =
            OperationSettledFact::new(cmd, op, 1_999, vec![event_id(0x37)], None, None, None)
                .unwrap();
        insert_event(
            &conn,
            event_id(0x38),
            Some(task),
            None,
            "operation.settled",
            i64::from(EVENT_SCHEMA_VERSION),
            1_999,
            &rmp_serde::to_vec(&settled).unwrap(),
        );
        drop(conn);
        let mut store = KernelStore::open(&path).expect("reopen");
        let err = rebuild_projector_only(&mut store)
            .expect_err("pure settle requires sync decision lineage");
        assert!(matches!(err, StoreError::Projection(_)), "{err:?}");
    }
}

fn agent_id(tail: u8) -> AgentSessionId {
    AgentSessionId::from_bytes(fixed_uuid_v7(tail)).expect("agent id")
}

#[test]
fn schema_rebuild_projections_is_deterministic_and_repairs_drift() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    drop(KernelStore::open(&path).expect("open"));

    let task = task_id(0x40);
    let agent = agent_id(0x41);
    let resource = resource_id(0x42);
    let cmd = command_id(0x43);
    let op = operation_id(0x44);

    {
        let conn = open_raw(&path);
        seed_task_created(&conn, task, event_id(0x50), 1_000);

        let agent_facts = AgentSessionFacts {
            id: agent,
            task_id: task,
            role: AgentRole::Primary,
            provider_kind: ProviderKind::ClaudeCode,
            provider_session_id: Some("sess-1".parse().expect("provider session")),
            lifecycle: AgentSessionLifecycle::Open,
            runtime_generation: 0,
            revision: 0,
        };
        insert_event(
            &conn,
            event_id(0x51),
            Some(task),
            Some(2),
            "agent_session.registered",
            i64::from(EVENT_SCHEMA_VERSION),
            1_100,
            &rmp_serde::to_vec(&AgentSessionRegisteredPayload { agent: agent_facts }).unwrap(),
        );
        insert_event(
            &conn,
            event_id(0x52),
            Some(task),
            Some(3),
            "primary_agent.set",
            i64::from(EVENT_SCHEMA_VERSION),
            1_200,
            &rmp_serde::to_vec(&PrimaryAgentSetPayload {
                agent_session_id: agent,
            })
            .unwrap(),
        );

        let resource_facts = ResourceFacts {
            id: resource,
            task_id: Some(task),
            owner_kind: OwnerKind::Task,
            resource_kind: ResourceKind::Terminal,
            recipe: ResourceRecipe::Terminal {
                cols: 120,
                rows: 40,
            },
            lifecycle: ResourceLifecycle::Active,
            runtime_generation: 0,
            updated_at_ms: 1_300,
        };
        insert_event(
            &conn,
            event_id(0x53),
            Some(task),
            Some(4),
            "resource.registered",
            i64::from(EVENT_SCHEMA_VERSION),
            1_300,
            &rmp_serde::to_vec(&ResourceRegisteredPayload {
                resource: resource_facts,
            })
            .unwrap(),
        );

        conn.execute(
            "INSERT INTO command_receipts(
                command_id, client_id, task_id, receipt, committed_sequence, created_at_ms
             ) VALUES (?1, ?2, ?3, X'00', 5, 1400)",
            rusqlite::params![
                cmd.as_bytes().as_slice(),
                client_id(0x45).as_bytes().as_slice(),
                task.as_bytes().as_slice(),
            ],
        )
        .expect("receipt");

        let accepted = OperationAcceptedFact::new(cmd, op, 1_400, None, None, None).unwrap();
        insert_event(
            &conn,
            event_id(0x54),
            Some(task),
            None,
            "operation.accepted",
            i64::from(EVENT_SCHEMA_VERSION),
            1_400,
            &rmp_serde::to_vec(&accepted).unwrap(),
        );
    }

    let mut store = KernelStore::open(&path).expect("reopen");
    let first = rebuild_projector_only(&mut store).expect("rebuild");
    assert_eq!(
        first,
        ProjectionRebuild {
            events_replayed: 5,
            drift_detected: true,
        }
    );
    let second = rebuild_projector_only(&mut store).expect("rebuild again");
    assert_eq!(
        second,
        ProjectionRebuild {
            events_replayed: 5,
            drift_detected: false,
        }
    );
    drop(store);

    let conn = open_raw(&path);
    let primary: Vec<u8> = conn
        .query_row(
            "SELECT primary_agent_session_id FROM tasks WHERE task_id = ?1",
            [task.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .expect("primary");
    assert_eq!(primary, agent.as_bytes().as_slice());

    let op_task: Vec<u8> = conn
        .query_row(
            "SELECT task_id FROM operations WHERE operation_id = ?1",
            [op.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .expect("operation task_id");
    assert_eq!(
        op_task,
        task.as_bytes().as_slice(),
        "operations.task_id must come from DomainEvent.task_id"
    );

    let lifecycle: String = conn
        .query_row(
            "SELECT lifecycle FROM resources WHERE resource_id = ?1",
            [resource.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .expect("resource");
    assert_eq!(lifecycle, "active");

    // Corrupt a projection row, then rebuild must repair without touching events.
    conn.execute(
        "UPDATE tasks SET title = 'drifted' WHERE task_id = ?1",
        [task.as_bytes().as_slice()],
    )
    .expect("drift");
    let event_count_before: i64 = conn
        .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
        .unwrap();

    let mut store = KernelStore::open(&path).expect("reopen");
    let repaired = rebuild_projector_only(&mut store).expect("repair");
    assert!(repaired.drift_detected);
    drop(store);

    let conn = open_raw(&path);
    let title: String = conn
        .query_row(
            "SELECT title FROM tasks WHERE task_id = ?1",
            [task.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(title, "Ship kernel");
    let event_count_after: i64 = conn
        .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
        .unwrap();
    assert_eq!(event_count_before, event_count_after);
    let shadow: Option<String> = conn
        .query_row(
            "SELECT name FROM sqlite_schema WHERE name LIKE 'shadow_%' LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()
        .unwrap();
    assert!(shadow.is_none(), "shadow tables must not remain");
}

#[test]
fn schema_resource_release_fences_generation_and_uses_occurred_at() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    drop(KernelStore::open(&path).expect("open"));

    let task = task_id(0x80);
    let resource = resource_id(0x81);
    let cmd = command_id(0x82);
    let op = operation_id(0x83);
    let released_id = event_id(0x93);

    {
        let conn = open_raw(&path);
        seed_task_created(&conn, task, event_id(0x90), 1_000);
        let resource_facts = ResourceFacts {
            id: resource,
            task_id: Some(task),
            owner_kind: OwnerKind::Task,
            resource_kind: ResourceKind::Terminal,
            recipe: ResourceRecipe::Terminal { cols: 80, rows: 24 },
            lifecycle: ResourceLifecycle::Active,
            runtime_generation: 3,
            updated_at_ms: 1_100,
        };
        insert_event(
            &conn,
            event_id(0x91),
            Some(task),
            Some(2),
            "resource.registered",
            i64::from(EVENT_SCHEMA_VERSION),
            1_100,
            &rmp_serde::to_vec(&ResourceRegisteredPayload {
                resource: resource_facts,
            })
            .unwrap(),
        );
        insert_event(
            &conn,
            event_id(0x92),
            Some(task),
            Some(3),
            "resource.release_begun",
            i64::from(EVENT_SCHEMA_VERSION),
            1_200,
            &rmp_serde::to_vec(&ResourceReleaseBegunPayload {
                resource_id: resource,
                runtime_generation: 3,
            })
            .unwrap(),
        );
        conn.execute(
            "INSERT INTO command_receipts(
                command_id, client_id, task_id, receipt, committed_sequence, created_at_ms
             ) VALUES (?1, ?2, ?3, X'00', 4, 1200)",
            rusqlite::params![
                cmd.as_bytes().as_slice(),
                client_id(0x01).as_bytes().as_slice(),
                task.as_bytes().as_slice(),
            ],
        )
        .unwrap();
        let accepted =
            OperationAcceptedFact::new(cmd, op, 1_200, Some(0), Some(resource), Some(3)).unwrap();
        insert_event(
            &conn,
            event_id(0x94),
            Some(task),
            None,
            "operation.accepted",
            i64::from(EVENT_SCHEMA_VERSION),
            1_200,
            &rmp_serde::to_vec(&accepted).unwrap(),
        );
        insert_event(
            &conn,
            released_id,
            Some(task),
            Some(4),
            "resource.released",
            i64::from(EVENT_SCHEMA_VERSION),
            1_500,
            &rmp_serde::to_vec(&ResourceReleasedPayload {
                resource_id: resource,
                runtime_generation: 3,
            })
            .unwrap(),
        );
        let settled = OperationSettledFact::new(
            cmd,
            op,
            1_500,
            vec![released_id],
            Some(0),
            Some(resource),
            Some(3),
        )
        .unwrap();
        insert_event(
            &conn,
            event_id(0x95),
            Some(task),
            None,
            "operation.settled",
            i64::from(EVENT_SCHEMA_VERSION),
            1_500,
            &rmp_serde::to_vec(&settled).unwrap(),
        );
    }

    let mut store = KernelStore::open(&path).expect("reopen");
    rebuild_projector_only(&mut store).expect("rebuild");
    drop(store);

    let conn = open_raw(&path);
    let (lifecycle, updated_at_ms, generation): (String, i64, i64) = conn
        .query_row(
            "SELECT lifecycle, updated_at_ms, runtime_generation
             FROM resources WHERE resource_id = ?1",
            [resource.as_bytes().as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(lifecycle, "released");
    assert_eq!(
        updated_at_ms, 1_500,
        "release must use event occurred_at_ms"
    );
    assert_eq!(generation, 3);

    // Stale generation must fail.
    conn.execute("DELETE FROM resources", []).unwrap();
    conn.execute("DELETE FROM tasks", []).unwrap();
    conn.execute("DELETE FROM operations", []).unwrap();
    conn.execute(
        "UPDATE events SET payload = ?1 WHERE event_type = 'resource.released'",
        [rmp_serde::to_vec(&ResourceReleasedPayload {
            resource_id: resource,
            runtime_generation: 99,
        })
        .unwrap()],
    )
    .unwrap();

    let mut store = KernelStore::open(&path).expect("reopen");
    let err = rebuild_projector_only(&mut store).expect_err("stale generation");
    assert!(
        matches!(err, StoreError::Projection(_)),
        "expected generation fence failure, got {err:?}"
    );
}

#[test]
fn schema_operation_outcome_requires_matching_task_fence() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    drop(KernelStore::open(&path).expect("open"));

    let task_a = task_id(0xC0);
    let task_b = task_id(0xC1);
    let resource = resource_id(0xC2);
    let cmd = command_id(0xC3);
    let op = operation_id(0xC4);

    {
        let conn = open_raw(&path);
        seed_task_created(&conn, task_a, event_id(0xD0), 1_000);
        seed_task_created(&conn, task_b, event_id(0xD1), 1_050);

        let resource_facts = ResourceFacts {
            id: resource,
            task_id: Some(task_a),
            owner_kind: OwnerKind::Task,
            resource_kind: ResourceKind::Terminal,
            recipe: ResourceRecipe::Terminal { cols: 80, rows: 24 },
            lifecycle: ResourceLifecycle::Active,
            runtime_generation: 7,
            updated_at_ms: 1_100,
        };
        insert_event(
            &conn,
            event_id(0xD2),
            Some(task_a),
            Some(2),
            "resource.registered",
            i64::from(EVENT_SCHEMA_VERSION),
            1_100,
            &rmp_serde::to_vec(&ResourceRegisteredPayload {
                resource: resource_facts,
            })
            .unwrap(),
        );
        insert_event(
            &conn,
            event_id(0xD6),
            Some(task_a),
            Some(3),
            "resource.release_begun",
            i64::from(EVENT_SCHEMA_VERSION),
            1_200,
            &rmp_serde::to_vec(&ResourceReleaseBegunPayload {
                resource_id: resource,
                runtime_generation: 7,
            })
            .unwrap(),
        );

        conn.execute(
            "INSERT INTO command_receipts(
                command_id, client_id, task_id, receipt, committed_sequence, created_at_ms
             ) VALUES (?1, ?2, ?3, X'00', 5, 1200)",
            rusqlite::params![
                cmd.as_bytes().as_slice(),
                client_id(0xC6).as_bytes().as_slice(),
                task_a.as_bytes().as_slice(),
            ],
        )
        .unwrap();

        let accepted =
            OperationAcceptedFact::new(cmd, op, 1_200, Some(0), Some(resource), Some(7)).unwrap();
        insert_event(
            &conn,
            event_id(0xD3),
            Some(task_a),
            None,
            "operation.accepted",
            i64::from(EVENT_SCHEMA_VERSION),
            1_200,
            &rmp_serde::to_vec(&accepted).unwrap(),
        );
    }

    let mut store = KernelStore::open(&path).expect("reopen");
    rebuild_projector_only(&mut store).expect("accept rebuild");
    drop(store);

    {
        let conn = open_raw(&path);
        let (state, rid, gen): (String, Vec<u8>, i64) = conn
            .query_row(
                "SELECT state, resource_id, runtime_generation FROM operations WHERE operation_id = ?1",
                [op.as_bytes().as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(state, "accepted");
        assert_eq!(rid, resource.as_bytes().as_slice());
        assert_eq!(gen, 7);

        // Wrong task scope on an otherwise matching outcome (still missing released; task fence fails first).
        let settled = OperationSettledFact::new(
            cmd,
            op,
            1_300,
            vec![event_id(0xC5)],
            Some(0),
            Some(resource),
            Some(7),
        )
        .unwrap();
        insert_event(
            &conn,
            event_id(0xD4),
            Some(task_b),
            None,
            "operation.settled",
            i64::from(EVENT_SCHEMA_VERSION),
            1_300,
            &rmp_serde::to_vec(&settled).unwrap(),
        );
    }

    let mut store = KernelStore::open(&path).expect("reopen");
    let err = rebuild_projector_only(&mut store).expect_err("task fence");
    assert!(
        matches!(err, StoreError::Projection(_)),
        "expected task fence projection error, got {err:?}"
    );
    drop(store);

    let conn = open_raw(&path);
    let state: String = conn
        .query_row(
            "SELECT state FROM operations WHERE operation_id = ?1",
            [op.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        state, "accepted",
        "failed rebuild must not alter stable ops"
    );

    // Valid release chain: released then settled on the owning task.
    conn.execute(
        "DELETE FROM events WHERE event_type = 'operation.settled'",
        [],
    )
    .unwrap();
    let released_id = event_id(0xC5);
    insert_event(
        &conn,
        released_id,
        Some(task_a),
        Some(4),
        "resource.released",
        i64::from(EVENT_SCHEMA_VERSION),
        1_400,
        &rmp_serde::to_vec(&ResourceReleasedPayload {
            resource_id: resource,
            runtime_generation: 7,
        })
        .unwrap(),
    );
    let settled = OperationSettledFact::new(
        cmd,
        op,
        1_400,
        vec![released_id],
        Some(0),
        Some(resource),
        Some(7),
    )
    .unwrap();
    insert_event(
        &conn,
        event_id(0xD5),
        Some(task_a),
        None,
        "operation.settled",
        i64::from(EVENT_SCHEMA_VERSION),
        1_400,
        &rmp_serde::to_vec(&settled).unwrap(),
    );
    drop(conn);

    let mut store = KernelStore::open(&path).expect("reopen");
    rebuild_projector_only(&mut store).expect("scoped settle");
    drop(store);

    let conn = open_raw(&path);
    let (state, outcome_at): (String, i64) = conn
        .query_row(
            "SELECT state, outcome_at_ms FROM operations WHERE operation_id = ?1",
            [op.as_bytes().as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(state, "settled");
    assert_eq!(outcome_at, 1_400);
}

#[test]
fn schema_operation_outcome_requires_matching_command_id() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    drop(KernelStore::open(&path).expect("open"));

    let task = task_id(0xA8);
    let resource = resource_id(0xA9);
    let cmd = command_id(0xAA);
    let other_cmd = command_id(0xAB);
    let op = operation_id(0xAC);
    let result_event = event_id(0xAD);

    {
        let conn = open_raw(&path);
        seed_task_created(&conn, task, event_id(0xB0), 1_000);
        let resource_facts = ResourceFacts {
            id: resource,
            task_id: Some(task),
            owner_kind: OwnerKind::Task,
            resource_kind: ResourceKind::Terminal,
            recipe: ResourceRecipe::Terminal { cols: 80, rows: 24 },
            lifecycle: ResourceLifecycle::Active,
            runtime_generation: 2,
            updated_at_ms: 1_100,
        };
        insert_event(
            &conn,
            event_id(0xB1),
            Some(task),
            Some(2),
            "resource.registered",
            i64::from(EVENT_SCHEMA_VERSION),
            1_100,
            &rmp_serde::to_vec(&ResourceRegisteredPayload {
                resource: resource_facts,
            })
            .unwrap(),
        );
        conn.execute(
            "INSERT INTO command_receipts(
                command_id, client_id, task_id, receipt, committed_sequence, created_at_ms
             ) VALUES (?1, ?2, ?3, X'00', 3, 1200)",
            rusqlite::params![
                cmd.as_bytes().as_slice(),
                client_id(0xAE).as_bytes().as_slice(),
                task.as_bytes().as_slice(),
            ],
        )
        .unwrap();
        let accepted =
            OperationAcceptedFact::new(cmd, op, 1_200, Some(0), Some(resource), Some(2)).unwrap();
        insert_event(
            &conn,
            event_id(0xB2),
            Some(task),
            None,
            "operation.accepted",
            i64::from(EVENT_SCHEMA_VERSION),
            1_200,
            &rmp_serde::to_vec(&accepted).unwrap(),
        );
    }

    let mut store = KernelStore::open(&path).expect("reopen");
    rebuild_projector_only(&mut store).expect("accept");
    drop(store);

    {
        let conn = open_raw(&path);
        // Every fence matches except command_id.
        let settled = OperationSettledFact::new(
            other_cmd,
            op,
            1_300,
            vec![result_event],
            Some(0),
            Some(resource),
            Some(2),
        )
        .unwrap();
        insert_event(
            &conn,
            event_id(0xB3),
            Some(task),
            None,
            "operation.settled",
            i64::from(EVENT_SCHEMA_VERSION),
            1_300,
            &rmp_serde::to_vec(&settled).unwrap(),
        );
    }

    let mut store = KernelStore::open(&path).expect("reopen");
    let err = rebuild_projector_only(&mut store).expect_err("command fence");
    assert!(
        matches!(err, StoreError::Projection(_)),
        "expected command_id fence failure, got {err:?}"
    );
    drop(store);

    let conn = open_raw(&path);
    let state: String = conn
        .query_row(
            "SELECT state FROM operations WHERE operation_id = ?1",
            [op.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(state, "accepted");
}
