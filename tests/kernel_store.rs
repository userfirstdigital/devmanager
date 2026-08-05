use std::fs;
use std::path::{Path, PathBuf};

use devmanager::domain::agent::{AgentRole, AgentSessionFacts, AgentSessionLifecycle};
use devmanager::domain::artifact::ArtifactContentRef;
use devmanager::domain::command::{
    Command, CommandEnvelope, CommandReceipt, CreateTaskIntent, RejectionCode, RenameTaskIntent,
};
use devmanager::domain::event::{
    AgentSessionRegisteredPayload, OperationAcceptedFact, OperationCancelledFact,
    OperationFailedFact, OperationSettledFact, OperationUncertainFact, PrimaryAgentSetPayload,
    ResourceRegisteredPayload, ResourceReleaseBegunPayload, ResourceReleasedPayload,
    TaskCloseBegunPayload, TaskCreatedPayload, TaskRenamedPayload, TaskUnitPayload,
    EVENT_SCHEMA_VERSION,
};
use devmanager::domain::id::{
    AgentSessionId, ArtifactId, ClientId, CommandId, EnvironmentId, EventId, OperationId,
    ProjectId, ResourceId, TaskId,
};
use devmanager::domain::operation::{
    CancellationReason, OperationErrorCode, OperationOutcome, OperationOutcomeKind, OperationState,
    OperationUncertaintyCode, OutcomeSource, ResourceFence,
};
use devmanager::domain::resource::{
    OwnerKind, ResourceFacts, ResourceKind, ResourceLifecycle, ResourceRecipe,
};
use devmanager::domain::task::{
    ReviewReadiness, TaskActivity, TaskAssignment, TaskAttention, TaskConnectivity, TaskFacts,
    TaskLifecycle, WorkspaceRef,
};
use devmanager::kernel::{KernelStore, ProjectionRebuild, StoreError};
use rusqlite::{Connection, OptionalExtension};
use serde::Serialize;
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
fn event_id(tail: u8) -> EventId {
    EventId::from_bytes(fixed_uuid_v7(tail)).expect("event id")
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

fn table_names(conn: &Connection) -> Vec<String> {
    let mut stmt = conn
        .prepare(
            "SELECT name FROM sqlite_schema
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%'
             ORDER BY name",
        )
        .expect("prepare tables");
    stmt.query_map([], |row| row.get(0))
        .expect("query tables")
        .map(|r| r.expect("row"))
        .collect()
}

fn index_names(conn: &Connection) -> Vec<String> {
    let mut stmt = conn
        .prepare(
            "SELECT name FROM sqlite_schema
             WHERE type = 'index' AND name NOT LIKE 'sqlite_%'
             ORDER BY name",
        )
        .expect("prepare indexes");
    stmt.query_map([], |row| row.get(0))
        .expect("query indexes")
        .map(|r| r.expect("row"))
        .collect()
}

#[test]
fn schema_open_applies_v1_tables_indexes_and_settings() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);

    let store = KernelStore::open(&path).expect("open empty store");
    drop(store);

    let conn = open_raw(&path);
    assert_eq!(
        table_names(&conn),
        vec![
            "agent_sessions".to_string(),
            "artifacts".to_string(),
            "command_receipts".to_string(),
            "events".to_string(),
            "operations".to_string(),
            "outbox".to_string(),
            "resources".to_string(),
            "schema_migrations".to_string(),
            "tasks".to_string(),
        ]
    );
    assert_eq!(
        index_names(&conn),
        vec![
            "idx_events_task_revision".to_string(),
            "idx_events_task_sequence".to_string(),
            "idx_operations_state".to_string(),
            "idx_outbox_delivery_state".to_string(),
            "idx_resources_active".to_string(),
        ]
    );

    let journal_mode: String = conn
        .query_row("PRAGMA journal_mode;", [], |row| row.get(0))
        .expect("journal_mode");
    assert_eq!(journal_mode.to_lowercase(), "wal");

    // foreign_keys / synchronous / busy_timeout are per-connection and applied on
    // KernelStore::open; FK enforcement is covered by schema_foreign_keys_are_enforced.

    let (version, name, sha): (i64, String, Vec<u8>) = conn
        .query_row(
            "SELECT version, name, sha256 FROM schema_migrations",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("migration row");
    assert_eq!(version, 1);
    assert_eq!(name, "v1_initial");
    assert_eq!(sha.len(), 32);

    let resource_notnull: i64 = conn
        .query_row(
            "SELECT \"notnull\" FROM pragma_table_info('operations') WHERE name = 'resource_id'",
            [],
            |row| row.get(0),
        )
        .expect("resource_id nullability");
    assert_eq!(resource_notnull, 0);

    let primary_notnull: i64 = conn
        .query_row(
            "SELECT \"notnull\" FROM pragma_table_info('tasks') WHERE name = 'primary_agent_session_id'",
            [],
            |row| row.get(0),
        )
        .expect("primary_agent_session_id nullability");
    assert_eq!(primary_notnull, 0);

    // No cyclic FK from tasks.primary_agent_session_id -> agent_sessions.
    let task_fks: Vec<String> = {
        let mut stmt = conn
            .prepare("SELECT \"table\" FROM pragma_foreign_key_list('tasks')")
            .expect("task fks");
        stmt.query_map([], |row| row.get(0))
            .expect("query")
            .map(|r| r.expect("row"))
            .collect()
    };
    assert!(
        !task_fks.iter().any(|t| t == "agent_sessions"),
        "tasks must not FK primary_agent_session_id to agent_sessions"
    );
}

#[test]
fn schema_foreign_keys_are_enforced() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    drop(KernelStore::open(&path).expect("open"));

    let conn = open_raw(&path);
    let err = conn
        .execute(
            "INSERT INTO agent_sessions (
                agent_session_id, task_id, role, provider_kind, lifecycle,
                runtime_generation, revision
             ) VALUES (?1, ?2, ?3, 'claude', 'open', 0, 0)",
            rusqlite::params![
                agent_id(0x31).as_bytes().as_slice(),
                task_id(0x30).as_bytes().as_slice(),
                rmp_serde::to_vec(&AgentRole::Primary).unwrap(),
            ],
        )
        .expect_err("orphan agent_session must fail FK");
    assert!(
        err.to_string().to_lowercase().contains("foreign"),
        "expected FK failure, got {err}"
    );
}

#[test]
fn schema_rejects_newer_changed_and_gapped_migrations() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    drop(KernelStore::open(&path).expect("open"));

    // Newer database.
    {
        let conn = open_raw(&path);
        conn.execute(
            "INSERT INTO schema_migrations(version, name, applied_at_ms, sha256)
             VALUES (2, 'v2_future', 1, ?1)",
            rusqlite::params![vec![0u8; 32]],
        )
        .expect("insert newer");
    }
    let err = KernelStore::open(&path).expect_err("newer db");
    assert!(
        matches!(err, StoreError::MigrationTooNew { .. }),
        "expected MigrationTooNew, got {err:?}"
    );
    fs::remove_file(&path).ok();
    let _ = fs::remove_file(path.with_extension("sqlite3-wal"));
    let _ = fs::remove_file(path.with_extension("sqlite3-shm"));

    // Changed hash for version 1.
    let path = temp_db_path(&dir);
    drop(KernelStore::open(&path).expect("open"));
    {
        let conn = open_raw(&path);
        conn.execute(
            "UPDATE schema_migrations SET sha256 = ?1 WHERE version = 1",
            rusqlite::params![vec![7u8; 32]],
        )
        .expect("mutate hash");
    }
    let err = KernelStore::open(&path).expect_err("changed hash");
    assert!(
        matches!(err, StoreError::MigrationChanged { version: 1 }),
        "expected MigrationChanged, got {err:?}"
    );
    fs::remove_file(&path).ok();

    // Changed name for version 1.
    let path = temp_db_path(&dir);
    drop(KernelStore::open(&path).expect("open"));
    {
        let conn = open_raw(&path);
        conn.execute(
            "UPDATE schema_migrations SET name = 'v1_tampered' WHERE version = 1",
            [],
        )
        .expect("mutate name");
    }
    let err = KernelStore::open(&path).expect_err("changed name");
    assert!(
        matches!(err, StoreError::MigrationChanged { version: 1 }),
        "expected MigrationChanged, got {err:?}"
    );
    fs::remove_file(&path).ok();

    // Gapped history: recorded version 2 without version 1.
    let path = temp_db_path(&dir);
    {
        let conn = Connection::open(&path).expect("create");
        conn.execute_batch(
            "CREATE TABLE schema_migrations (
                version INTEGER PRIMARY KEY,
                name TEXT NOT NULL UNIQUE,
                applied_at_ms INTEGER NOT NULL,
                sha256 BLOB NOT NULL CHECK(length(sha256) = 32)
             );
             INSERT INTO schema_migrations(version, name, applied_at_ms, sha256)
             VALUES (2, 'v2_only', 1, x'0000000000000000000000000000000000000000000000000000000000000000');",
        )
        .expect("gap seed");
    }
    let err = KernelStore::open(&path).expect_err("gapped");
    assert!(
        matches!(err, StoreError::MigrationGap { .. }),
        "expected MigrationGap, got {err:?}"
    );
}

#[test]
fn schema_interrupted_migration_is_safe() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);

    // Partial apply: events table exists, but no migration row.
    {
        let conn = Connection::open(&path).expect("create");
        conn.execute_batch(
            "CREATE TABLE events (
                sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                event_id BLOB NOT NULL UNIQUE CHECK(length(event_id) = 16),
                task_id BLOB,
                task_revision INTEGER,
                event_type TEXT NOT NULL,
                schema_version INTEGER NOT NULL,
                occurred_at_ms INTEGER NOT NULL,
                payload BLOB NOT NULL
             );",
        )
        .expect("partial schema");
    }

    let err = KernelStore::open(&path).expect_err("interrupted");
    assert!(
        matches!(
            err,
            StoreError::MigrationInterrupted | StoreError::Sqlite(_)
        ),
        "expected safe typed failure for interrupted migration, got {err:?}"
    );
}

#[test]
fn schema_corrupt_and_truncated_databases_fail_typed() {
    let dir = TempDir::new().expect("tempdir");

    let zero = dir.path().join("zero.sqlite3");
    fs::write(&zero, b"").expect("write zero-byte file");
    assert!(zero.exists());
    assert_eq!(fs::metadata(&zero).unwrap().len(), 0);
    let err = KernelStore::open(&zero).expect_err("existing zero-byte db");
    assert!(
        matches!(err, StoreError::Truncated),
        "existing zero-byte file must be Truncated, got {err:?}"
    );

    let truncated = dir.path().join("truncated.sqlite3");
    fs::write(&truncated, b"SQLite format 3\0").expect("write truncated");
    let err = KernelStore::open(&truncated).expect_err("truncated");
    assert!(
        matches!(
            err,
            StoreError::Truncated | StoreError::Corruption | StoreError::Sqlite(_)
        ),
        "expected truncated/corrupt typed error, got {err:?}"
    );

    let corrupt = dir.path().join("corrupt.sqlite3");
    drop(KernelStore::open(&corrupt).expect("create valid"));
    let mut bytes = fs::read(&corrupt).expect("read");
    assert!(bytes.len() > 100);
    // Flip bytes inside the first database page payload to force integrity failure.
    for b in bytes.iter_mut().skip(24).take(32) {
        *b ^= 0xFF;
    }
    fs::write(&corrupt, &bytes).expect("write corrupt");
    let err = KernelStore::open(&corrupt).expect_err("corrupt");
    assert!(
        matches!(
            err,
            StoreError::IntegrityCheckFailed(_) | StoreError::Corruption | StoreError::Sqlite(_)
        ),
        "expected integrity/corruption error, got {err:?}"
    );
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
            provider_kind: "claude".into(),
            provider_session_id: Some("sess-1".into()),
            lifecycle: devmanager::domain::agent::AgentSessionLifecycle::Open,
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
    let first = store.rebuild_projections().expect("rebuild");
    assert_eq!(
        first,
        ProjectionRebuild {
            events_replayed: 5,
            drift_detected: true,
        }
    );
    let second = store.rebuild_projections().expect("rebuild again");
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
    let repaired = store.rebuild_projections().expect("repair");
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
fn schema_primary_agent_selection_requires_same_task_primary_role() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    drop(KernelStore::open(&path).expect("open"));

    let task = task_id(0x60);
    let specialist = agent_id(0x61);

    {
        let conn = open_raw(&path);
        seed_task_created(&conn, task, event_id(0x70), 1_000);
        let agent_facts = AgentSessionFacts {
            id: specialist,
            task_id: task,
            role: AgentRole::specialist("reviewer").unwrap(),
            provider_kind: "claude".into(),
            provider_session_id: None,
            lifecycle: devmanager::domain::agent::AgentSessionLifecycle::Open,
            runtime_generation: 0,
            revision: 0,
        };
        insert_event(
            &conn,
            event_id(0x71),
            Some(task),
            Some(2),
            "agent_session.registered",
            i64::from(EVENT_SCHEMA_VERSION),
            1_100,
            &rmp_serde::to_vec(&AgentSessionRegisteredPayload { agent: agent_facts }).unwrap(),
        );
        insert_event(
            &conn,
            event_id(0x72),
            Some(task),
            Some(3),
            "primary_agent.set",
            i64::from(EVENT_SCHEMA_VERSION),
            1_200,
            &rmp_serde::to_vec(&PrimaryAgentSetPayload {
                agent_session_id: specialist,
            })
            .unwrap(),
        );
    }

    let mut store = KernelStore::open(&path).expect("reopen");
    let err = store.rebuild_projections().expect_err("specialist primary");
    assert!(
        matches!(err, StoreError::Projection(_)),
        "expected projection failure for non-primary role, got {err:?}"
    );
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
    store.rebuild_projections().expect("rebuild");
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
    let err = store.rebuild_projections().expect_err("stale generation");
    assert!(
        matches!(err, StoreError::Projection(_)),
        "expected generation fence failure, got {err:?}"
    );
}

#[test]
fn schema_codec_mismatch_and_integer_overflow_are_typed() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    drop(KernelStore::open(&path).expect("open"));

    let task = task_id(0xA0);
    {
        let conn = open_raw(&path);
        // event_type disagrees with payload shape.
        insert_event(
            &conn,
            event_id(0xB0),
            Some(task),
            Some(1),
            "task.renamed",
            i64::from(EVENT_SCHEMA_VERSION),
            1_000,
            &task_created_payload(task),
        );
    }
    let mut store = KernelStore::open(&path).expect("reopen");
    let err = store.rebuild_projections().expect_err("codec mismatch");
    assert!(
        matches!(err, StoreError::CodecMismatch { .. }),
        "expected CodecMismatch, got {err:?}"
    );
    drop(store);

    fs::remove_file(&path).ok();
    drop(KernelStore::open(&path).expect("fresh"));
    let task = task_id(0xA1);
    let resource = resource_id(0xA2);
    {
        let conn = open_raw(&path);
        seed_task_created(&conn, task, event_id(0xB1), 1_000);
        let resource_facts = ResourceFacts {
            id: resource,
            task_id: Some(task),
            owner_kind: OwnerKind::Task,
            resource_kind: ResourceKind::Terminal,
            recipe: ResourceRecipe::Terminal { cols: 80, rows: 24 },
            lifecycle: ResourceLifecycle::Active,
            runtime_generation: u64::MAX,
            updated_at_ms: 1_100,
        };
        insert_event(
            &conn,
            event_id(0xB2),
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
    }
    let mut store = KernelStore::open(&path).expect("reopen");
    let err = store.rebuild_projections().expect_err("overflow");
    assert!(
        matches!(err, StoreError::IntegerOutOfRange { .. }),
        "expected IntegerOutOfRange, got {err:?}"
    );
}

#[test]
fn schema_migration_manifest_hash_is_stable() {
    const EXPECTED_V1_SHA256_HEX: &str =
        "79f0a38f1092f770a884ef3a12848184f00e7741270ffb07b0de823263e2521f";
    let expected_bytes: Vec<u8> = (0..32)
        .map(|i| {
            u8::from_str_radix(&EXPECTED_V1_SHA256_HEX[i * 2..i * 2 + 2], 16).expect("hex nibble")
        })
        .collect();

    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    drop(KernelStore::open(&path).expect("open"));
    let conn = open_raw(&path);
    let (name, sha): (String, Vec<u8>) = conn
        .query_row(
            "SELECT name, sha256 FROM schema_migrations WHERE version = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(name, "v1_initial");
    assert_eq!(sha, expected_bytes);
    assert_eq!(
        sha.iter().map(|b| format!("{b:02x}")).collect::<String>(),
        EXPECTED_V1_SHA256_HEX
    );
    // Re-open must accept the exact recorded hash (no silent rewrite).
    drop(conn);
    KernelStore::open(&path).expect("reopen with exact hash");
}

#[test]
fn schema_nullable_id_columns_reject_malformed_lengths() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    drop(KernelStore::open(&path).expect("open"));
    let conn = open_raw(&path);

    let bad = vec![0u8; 8];
    let err = conn
        .execute(
            "INSERT INTO events(
                event_id, task_id, task_revision, event_type, schema_version, occurred_at_ms, payload
             ) VALUES (?1, ?2, 1, 'task.created', 1, 1, X'00')",
            rusqlite::params![event_id(0x01).as_bytes().as_slice(), bad.as_slice()],
        )
        .expect_err("events.task_id length check");
    assert!(
        err.to_string().to_lowercase().contains("check"),
        "expected CHECK failure, got {err}"
    );

    let err = conn
        .execute(
            "INSERT INTO command_receipts(
                command_id, client_id, task_id, receipt, committed_sequence, created_at_ms
             ) VALUES (?1, ?2, ?3, X'00', NULL, 1)",
            rusqlite::params![
                command_id(0x02).as_bytes().as_slice(),
                client_id(0x03).as_bytes().as_slice(),
                bad.as_slice(),
            ],
        )
        .expect_err("command_receipts.task_id length check");
    assert!(err.to_string().to_lowercase().contains("check"));

    conn.execute(
        "INSERT INTO command_receipts(
            command_id, client_id, task_id, receipt, committed_sequence, created_at_ms
         ) VALUES (?1, ?2, NULL, X'00', NULL, 1)",
        rusqlite::params![
            command_id(0x04).as_bytes().as_slice(),
            client_id(0x05).as_bytes().as_slice(),
        ],
    )
    .expect("null task_id allowed on receipts");

    let err = conn
        .execute(
            "INSERT INTO operations(
                operation_id, command_id, task_id, state, accepted_at_ms
             ) VALUES (?1, ?2, ?3, 'accepted', 1)",
            rusqlite::params![
                operation_id(0x06).as_bytes().as_slice(),
                command_id(0x04).as_bytes().as_slice(),
                bad.as_slice(),
            ],
        )
        .expect_err("operations.task_id length check");
    assert!(err.to_string().to_lowercase().contains("check"));

    let err = conn
        .execute(
            "INSERT INTO resources(
                resource_id, task_id, owner_kind, resource_kind, recipe,
                lifecycle, runtime_generation, updated_at_ms
             ) VALUES (?1, ?2, 'host', 'terminal', X'00', 'active', 0, 1)",
            rusqlite::params![resource_id(0x07).as_bytes().as_slice(), bad.as_slice()],
        )
        .expect_err("resources.task_id length check");
    assert!(err.to_string().to_lowercase().contains("check"));

    // Confirm active index targets task_id + resource_kind.
    let sql: String = conn
        .query_row(
            "SELECT sql FROM sqlite_schema WHERE type = 'index' AND name = 'idx_resources_active'",
            [],
            |row| row.get(0),
        )
        .expect("index sql");
    assert!(sql.contains("task_id"));
    assert!(sql.contains("resource_kind"));
    assert!(sql.to_lowercase().contains("lifecycle = 'active'"));
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
    store.rebuild_projections().expect("accept rebuild");
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
    let err = store.rebuild_projections().expect_err("task fence");
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
    store.rebuild_projections().expect("scoped settle");
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
fn schema_canonical_compare_ignores_rowid_insertion_order() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    drop(KernelStore::open(&path).expect("open"));

    let task_lo = task_id(0x01);
    let task_hi = task_id(0x02);
    {
        let conn = open_raw(&path);
        // Insert high id first so physical rowid order differs from PK order.
        seed_task_created(&conn, task_hi, event_id(0x10), 1_000);
        seed_task_created(&conn, task_lo, event_id(0x11), 1_100);
    }

    let mut store = KernelStore::open(&path).expect("reopen");
    store.rebuild_projections().expect("initial rebuild");
    drop(store);

    {
        let conn = open_raw(&path);
        let rows: Vec<(i64, Vec<u8>)> = {
            let mut stmt = conn
                .prepare("SELECT rowid, task_id FROM tasks ORDER BY rowid ASC")
                .unwrap();
            stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
        };
        assert_eq!(rows.len(), 2);
        // Capture full rows, delete, reinsert in reverse physical order.
        let snapshot: Vec<Vec<rusqlite::types::Value>> = {
            let mut stmt = conn
                .prepare("SELECT * FROM tasks ORDER BY rowid ASC")
                .unwrap();
            let col_count = stmt.column_count();
            let mut out = Vec::new();
            let mut rows = stmt.query([]).unwrap();
            while let Some(row) = rows.next().unwrap() {
                let mut values = Vec::with_capacity(col_count);
                for i in 0..col_count {
                    values.push(row.get::<_, rusqlite::types::Value>(i).unwrap());
                }
                out.push(values);
            }
            out
        };
        conn.execute("DELETE FROM tasks", []).unwrap();
        // Reverse insertion order relative to the previous physical layout.
        for values in snapshot.into_iter().rev() {
            conn.execute(
                "INSERT INTO tasks VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)",
                rusqlite::params_from_iter(values),
            )
            .unwrap();
        }
        let new_order: Vec<Vec<u8>> = {
            let mut stmt = conn
                .prepare("SELECT task_id FROM tasks ORDER BY rowid ASC")
                .unwrap();
            stmt.query_map([], |row| row.get(0))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
        };
        assert_ne!(
            new_order[0], rows[0].1,
            "rowid order must differ after reverse reinsert"
        );
    }

    let mut store = KernelStore::open(&path).expect("reopen");
    let rebuilt = store.rebuild_projections().expect("canonical rebuild");
    assert_eq!(
        rebuilt,
        ProjectionRebuild {
            events_replayed: 2,
            drift_detected: false,
        },
        "identical PK-ordered rows must not report drift"
    );
}

#[test]
fn schema_corrupt_revision_order_fails_projection() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    drop(KernelStore::open(&path).expect("open"));

    let task = task_id(0xE0);
    {
        let conn = open_raw(&path);
        seed_task_created(&conn, task, event_id(0xE1), 1_000);
        // Skip revision 2 — jump to 3.
        insert_event(
            &conn,
            event_id(0xE2),
            Some(task),
            Some(3),
            "task.renamed",
            i64::from(EVENT_SCHEMA_VERSION),
            1_100,
            &rmp_serde::to_vec(&TaskRenamedPayload {
                title: "Skipped".into(),
            })
            .unwrap(),
        );
    }

    let mut store = KernelStore::open(&path).expect("reopen");
    let err = store.rebuild_projections().expect_err("revision gap");
    assert!(
        matches!(err, StoreError::Projection(_)),
        "expected revision projection failure, got {err:?}"
    );
    drop(store);

    // Missing target task for a mutation must also fail (not silently no-op).
    let dir2 = TempDir::new().expect("tempdir");
    let path2 = temp_db_path(&dir2);
    drop(KernelStore::open(&path2).expect("open"));
    {
        let conn = open_raw(&path2);
        insert_event(
            &conn,
            event_id(0xE3),
            Some(task),
            Some(2),
            "task.renamed",
            i64::from(EVENT_SCHEMA_VERSION),
            1_100,
            &rmp_serde::to_vec(&TaskRenamedPayload {
                title: "Orphan".into(),
            })
            .unwrap(),
        );
    }
    let mut store = KernelStore::open(&path2).expect("reopen");
    let err = store.rebuild_projections().expect_err("missing task");
    assert!(matches!(err, StoreError::Projection(_)));
}

#[test]
fn schema_replay_rejects_domain_invalid_transitions() {
    let dir = TempDir::new().expect("tempdir");

    // Blank rename title must fail decode/replay (domain canonicalize).
    {
        let path = dir.path().join("blank_rename.sqlite3");
        drop(KernelStore::open(&path).expect("open"));
        let task = task_id(0xF0);
        let conn = open_raw(&path);
        seed_task_created(&conn, task, event_id(0xF1), 1_000);
        insert_event(
            &conn,
            event_id(0xF2),
            Some(task),
            Some(2),
            "task.renamed",
            i64::from(EVENT_SCHEMA_VERSION),
            1_100,
            &rmp_serde::to_vec(&TaskRenamedPayload {
                title: "   ".into(),
            })
            .unwrap(),
        );
        drop(conn);
        let mut store = KernelStore::open(&path).expect("reopen");
        let err = store.rebuild_projections().expect_err("blank rename");
        assert!(
            matches!(err, StoreError::EventDecode(_) | StoreError::Projection(_)),
            "blank rename must fail, got {err:?}"
        );
    }

    // CloseBegun requires Open and action_epoch == stored + 1.
    {
        let path = dir.path().join("bad_close.sqlite3");
        drop(KernelStore::open(&path).expect("open"));
        let task = task_id(0xF3);
        let conn = open_raw(&path);
        seed_task_created(&conn, task, event_id(0xF4), 1_000);
        insert_event(
            &conn,
            event_id(0xF5),
            Some(task),
            Some(2),
            "task.close_begun",
            i64::from(EVENT_SCHEMA_VERSION),
            1_100,
            &rmp_serde::to_vec(&TaskCloseBegunPayload { action_epoch: 99 }).unwrap(),
        );
        drop(conn);
        let mut store = KernelStore::open(&path).expect("reopen");
        let err = store.rebuild_projections().expect_err("bad close epoch");
        assert!(matches!(err, StoreError::Projection(_)), "got {err:?}");
    }

    // A second close with the next revision/epoch still fails because the task is Closing.
    {
        let path = dir.path().join("double_close.sqlite3");
        drop(KernelStore::open(&path).expect("open"));
        let task = task_id(0xE2);
        let conn = open_raw(&path);
        seed_task_created(&conn, task, event_id(0xE3), 1_000);
        insert_event(
            &conn,
            event_id(0xE4),
            Some(task),
            Some(2),
            "task.close_begun",
            i64::from(EVENT_SCHEMA_VERSION),
            1_100,
            &rmp_serde::to_vec(&TaskCloseBegunPayload { action_epoch: 1 }).unwrap(),
        );
        insert_event(
            &conn,
            event_id(0xE5),
            Some(task),
            Some(3),
            "task.close_begun",
            i64::from(EVENT_SCHEMA_VERSION),
            1_200,
            &rmp_serde::to_vec(&TaskCloseBegunPayload { action_epoch: 2 }).unwrap(),
        );
        drop(conn);
        let mut store = KernelStore::open(&path).expect("reopen");
        let err = store.rebuild_projections().expect_err("close from closing");
        assert!(matches!(err, StoreError::Projection(_)), "got {err:?}");
    }

    // Reopen from Open is invalid.
    {
        let path = dir.path().join("bad_reopen.sqlite3");
        drop(KernelStore::open(&path).expect("open"));
        let task = task_id(0xF6);
        let conn = open_raw(&path);
        seed_task_created(&conn, task, event_id(0xF7), 1_000);
        insert_event(
            &conn,
            event_id(0xF8),
            Some(task),
            Some(2),
            "task.reopened",
            i64::from(EVENT_SCHEMA_VERSION),
            1_100,
            &rmp_serde::to_vec(&TaskUnitPayload {}).unwrap(),
        );
        drop(conn);
        let mut store = KernelStore::open(&path).expect("reopen");
        let err = store.rebuild_projections().expect_err("reopen from open");
        assert!(matches!(err, StoreError::Projection(_)), "got {err:?}");
    }

    // Archive from Open is invalid (must be Closing).
    {
        let path = dir.path().join("bad_archive.sqlite3");
        drop(KernelStore::open(&path).expect("open"));
        let task = task_id(0xF9);
        let conn = open_raw(&path);
        seed_task_created(&conn, task, event_id(0xFA), 1_000);
        insert_event(
            &conn,
            event_id(0xFB),
            Some(task),
            Some(2),
            "task.archived",
            i64::from(EVENT_SCHEMA_VERSION),
            1_100,
            &rmp_serde::to_vec(&TaskUnitPayload {}).unwrap(),
        );
        drop(conn);
        let mut store = KernelStore::open(&path).expect("reopen");
        let err = store.rebuild_projections().expect_err("archive from open");
        assert!(matches!(err, StoreError::Projection(_)), "got {err:?}");
    }

    // ResourceRegistered must reject non-Active registration lifecycle.
    {
        let path = dir.path().join("bad_resource.sqlite3");
        drop(KernelStore::open(&path).expect("open"));
        let task = task_id(0xFC);
        let resource = resource_id(0xFD);
        let conn = open_raw(&path);
        seed_task_created(&conn, task, event_id(0xFE), 1_000);
        let resource_facts = ResourceFacts {
            id: resource,
            task_id: Some(task),
            owner_kind: OwnerKind::Task,
            resource_kind: ResourceKind::Terminal,
            recipe: ResourceRecipe::Terminal { cols: 80, rows: 24 },
            lifecycle: ResourceLifecycle::Releasing,
            runtime_generation: 0,
            updated_at_ms: 1_100,
        };
        insert_event(
            &conn,
            event_id(0xFF),
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
        drop(conn);
        let mut store = KernelStore::open(&path).expect("reopen");
        let err = store
            .rebuild_projections()
            .expect_err("releasing registration");
        assert!(
            matches!(err, StoreError::EventDecode(_) | StoreError::Projection(_)),
            "releasing registration must fail, got {err:?}"
        );
    }
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
    store.rebuild_projections().expect("accept");
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
    let err = store.rebuild_projections().expect_err("command fence");
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

fn seed_accepted_operation(
    path: &Path,
    task: TaskId,
    cmd: CommandId,
    op: OperationId,
    resource: Option<(ResourceId, u64)>,
    action_epoch: Option<u64>,
) {
    let conn = open_raw(path);
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

#[test]
fn command_contract_projector_accepts_dispatch_from_accepted_only() {
    use devmanager::domain::operation::OutcomeSource;

    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    drop(KernelStore::open(&path).expect("open"));

    let task = task_id(0x40);
    let cmd = command_id(0x41);
    let op = operation_id(0x42);
    seed_accepted_operation(&path, task, cmd, op, None, Some(1));

    let mut store = KernelStore::open(&path).expect("reopen");
    store.rebuild_projections().expect("accept rebuild");
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
    store.rebuild_projections().expect("dispatch settle");
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
    use devmanager::domain::operation::OutcomeSource;

    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    drop(KernelStore::open(&path).expect("open"));

    let task = task_id(0x50);
    let cmd = command_id(0x51);
    let op = operation_id(0x52);
    seed_accepted_operation(&path, task, cmd, op, None, None);

    let mut store = KernelStore::open(&path).expect("reopen");
    store.rebuild_projections().expect("accept rebuild");
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
    let err = store
        .rebuild_projections()
        .expect_err("accepted cannot take verified reconciliation");
    assert!(
        matches!(err, StoreError::Projection(_)),
        "expected projection fence, got {err:?}"
    );
}

#[test]
fn command_contract_projector_uncertain_resolves_only_via_reconciliation() {
    use devmanager::domain::event::OperationUncertainFact;
    use devmanager::domain::operation::{OperationUncertaintyCode, OutcomeSource};

    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    drop(KernelStore::open(&path).expect("open"));

    let task = task_id(0x60);
    let cmd = command_id(0x61);
    let op = operation_id(0x62);
    seed_accepted_operation(&path, task, cmd, op, None, Some(1));

    let mut store = KernelStore::open(&path).expect("reopen");
    store.rebuild_projections().expect("accept rebuild");
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
    store.rebuild_projections().expect("uncertain rebuild");
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
    let err = store
        .rebuild_projections()
        .expect_err("uncertain rejects dispatch settlement");
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
    store
        .rebuild_projections()
        .expect("uncertain resolves via verified reconciliation");
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

fn create_task_intent(task: TaskId) -> CreateTaskIntent {
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

fn command_envelope(
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

fn receipt_blob(conn: &Connection, command_id: CommandId) -> Vec<u8> {
    conn.query_row(
        "SELECT receipt FROM command_receipts WHERE command_id = ?1",
        [command_id.as_bytes().as_slice()],
        |row| row.get(0),
    )
    .expect("receipt blob")
}

fn count_table(conn: &Connection, table: &str) -> i64 {
    conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
        row.get(0)
    })
    .expect("count")
}

fn event_types(conn: &Connection) -> Vec<String> {
    let mut stmt = conn
        .prepare("SELECT event_type FROM events ORDER BY sequence ASC")
        .expect("prepare events");
    stmt.query_map([], |row| row.get(0))
        .expect("query events")
        .map(|r| r.expect("row"))
        .collect()
}

#[test]
fn command_pure_create_persists_decision_operation_receipt_and_sequence() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut store = KernelStore::open(&path).expect("open");

    let task = task_id(0xC1);
    let cmd = command_id(0xC2);
    let envelope = command_envelope(
        cmd,
        None,
        None,
        Command::CreateTask(create_task_intent(task)),
    );

    let receipt = store.execute(envelope).expect("create execute");
    let CommandReceipt::Accepted {
        command_id,
        operation_id,
        task_revision,
        event_ids,
    } = receipt
    else {
        panic!("expected accepted receipt, got {receipt:?}");
    };
    assert_eq!(command_id, cmd);
    assert_eq!(task_revision, Some(1));
    assert_eq!(event_ids.len(), 1);

    drop(store);
    let conn = open_raw(&path);

    assert_eq!(
        event_types(&conn),
        vec![
            "task.created".to_string(),
            "operation.accepted".to_string(),
            "operation.settled".to_string(),
        ]
    );

    let committed: i64 = conn
        .query_row(
            "SELECT committed_sequence FROM command_receipts WHERE command_id = ?1",
            [cmd.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .expect("committed_sequence");
    assert_eq!(committed, 3);

    let (title, revision, lifecycle): (String, i64, String) = conn
        .query_row(
            "SELECT title, revision, lifecycle FROM tasks WHERE task_id = ?1",
            [task.as_bytes().as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("task projection");
    assert_eq!(title, "Ship kernel");
    assert_eq!(revision, 1);
    assert_eq!(lifecycle, "open");

    let (op_state, stored_op): (String, Vec<u8>) = conn
        .query_row(
            "SELECT state, operation_id FROM operations WHERE command_id = ?1",
            [cmd.as_bytes().as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("operation projection");
    assert_eq!(op_state, "settled");
    assert_eq!(stored_op.as_slice(), operation_id.as_bytes().as_slice());
    assert_eq!(count_table(&conn, "outbox"), 0);

    let decision_event_id: Vec<u8> = conn
        .query_row(
            "SELECT event_id FROM events WHERE event_type = 'task.created'",
            [],
            |row| row.get(0),
        )
        .expect("decision event id");
    assert_eq!(
        event_ids[0].as_bytes().as_slice(),
        decision_event_id.as_slice()
    );
}

#[test]
fn command_pure_rename_settles_with_decision_event_ids_only() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut store = KernelStore::open(&path).expect("open");

    let task = task_id(0xC3);
    store
        .execute(command_envelope(
            command_id(0xC4),
            None,
            None,
            Command::CreateTask(create_task_intent(task)),
        ))
        .expect("create");

    let rename_cmd = command_id(0xC5);
    let receipt = store
        .execute(command_envelope(
            rename_cmd,
            Some(task),
            Some(1),
            Command::RenameTask(RenameTaskIntent {
                title: "Renamed kernel".into(),
            }),
        ))
        .expect("rename");

    let CommandReceipt::Accepted {
        event_ids,
        task_revision,
        ..
    } = receipt
    else {
        panic!("expected accepted rename, got {receipt:?}");
    };
    assert_eq!(task_revision, Some(2));
    assert_eq!(event_ids.len(), 1);

    drop(store);
    let conn = open_raw(&path);
    let types = event_types(&conn);
    assert_eq!(
        &types[3..],
        &[
            "task.renamed".to_string(),
            "operation.accepted".to_string(),
            "operation.settled".to_string(),
        ]
    );

    let all_event_ids: Vec<Vec<u8>> = {
        let mut stmt = conn
            .prepare("SELECT event_id FROM events ORDER BY sequence ASC")
            .unwrap();
        stmt.query_map([], |row| row.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    };
    assert_eq!(all_event_ids.len(), 6);
    assert_eq!(
        event_ids[0].as_bytes().as_slice(),
        all_event_ids[3].as_slice(),
        "receipt event_ids must be the decision event only"
    );
    assert_ne!(
        event_ids[0].as_bytes().as_slice(),
        all_event_ids[4].as_slice()
    );
    assert_ne!(
        event_ids[0].as_bytes().as_slice(),
        all_event_ids[5].as_slice()
    );

    let settled_payload: Vec<u8> = conn
        .query_row(
            "SELECT payload FROM events WHERE event_type = 'operation.settled'
             ORDER BY sequence DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let settled: OperationSettledFact = rmp_serde::from_slice(&settled_payload).unwrap();
    assert_eq!(settled.result_event_ids, event_ids);
    assert_eq!(
        settled.source,
        devmanager::domain::operation::OutcomeSource::Dispatch
    );

    let title: String = conn
        .query_row(
            "SELECT title FROM tasks WHERE task_id = ?1",
            [task.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(title, "Renamed kernel");
    assert_eq!(count_table(&conn, "outbox"), 0);
}

#[test]
fn command_pure_retry_returns_byte_equivalent_receipt() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut store = KernelStore::open(&path).expect("open");

    let task = task_id(0xC6);
    let cmd = command_id(0xC7);
    let envelope = command_envelope(
        cmd,
        None,
        None,
        Command::CreateTask(create_task_intent(task)),
    );
    let first = store.execute(envelope.clone()).expect("first execute");
    drop(store);

    let conn = open_raw(&path);
    let original_blob = receipt_blob(&conn, cmd);
    let event_count = count_table(&conn, "events");
    let task_revision: i64 = conn
        .query_row(
            "SELECT revision FROM tasks WHERE task_id = ?1",
            [task.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    drop(conn);

    let mut store = KernelStore::open(&path).expect("reopen");
    let second = store
        .execute(envelope.clone())
        .expect("retry same connection");
    assert_eq!(second, first);

    drop(store);
    let conn = open_raw(&path);
    assert_eq!(receipt_blob(&conn, cmd), original_blob);
    assert_eq!(count_table(&conn, "events"), event_count);
    let revision_after: i64 = conn
        .query_row(
            "SELECT revision FROM tasks WHERE task_id = ?1",
            [task.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(revision_after, task_revision);
    drop(conn);

    let mut store = KernelStore::open(&path).expect("reopen again");
    let third = store.execute(envelope).expect("retry after reopen");
    assert_eq!(third, first);
    drop(store);

    let conn = open_raw(&path);
    assert_eq!(receipt_blob(&conn, cmd), original_blob);
    assert_eq!(count_table(&conn, "events"), event_count);
}

#[test]
fn command_pure_revision_conflict_persists_rejected_receipt() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut store = KernelStore::open(&path).expect("open");

    let task = task_id(0xC8);
    store
        .execute(command_envelope(
            command_id(0xC9),
            None,
            None,
            Command::CreateTask(create_task_intent(task)),
        ))
        .expect("create");

    let conflict_cmd = command_id(0xCA);
    let conflict_envelope = command_envelope(
        conflict_cmd,
        Some(task),
        Some(99),
        Command::RenameTask(RenameTaskIntent {
            title: "Stale rename".into(),
        }),
    );
    let rejected = store.execute(conflict_envelope.clone()).expect("conflict");
    assert_eq!(
        rejected,
        CommandReceipt::Rejected {
            command_id: conflict_cmd,
            code: RejectionCode::RevisionConflict,
            current_revision: Some(1),
        }
    );

    drop(store);
    let conn = open_raw(&path);
    assert_eq!(
        event_types(&conn),
        vec![
            "task.created".to_string(),
            "operation.accepted".to_string(),
            "operation.settled".to_string(),
        ]
    );
    assert_eq!(count_table(&conn, "operations"), 1);
    assert_eq!(count_table(&conn, "outbox"), 0);
    let rejected_blob = receipt_blob(&conn, conflict_cmd);
    let committed: Option<i64> = conn
        .query_row(
            "SELECT committed_sequence FROM command_receipts WHERE command_id = ?1",
            [conflict_cmd.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(committed, None);
    drop(conn);

    let mut store = KernelStore::open(&path).expect("reopen");
    store
        .execute(command_envelope(
            command_id(0xCB),
            Some(task),
            Some(1),
            Command::RenameTask(RenameTaskIntent {
                title: "Advance revision".into(),
            }),
        ))
        .expect("advance revision");

    let retry = store
        .execute(conflict_envelope)
        .expect("retry rejected command");
    assert_eq!(retry, rejected);
    drop(store);

    let conn = open_raw(&path);
    assert_eq!(receipt_blob(&conn, conflict_cmd), rejected_blob);
    let title: String = conn
        .query_row(
            "SELECT title FROM tasks WHERE task_id = ?1",
            [task.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(title, "Advance revision");
    assert_eq!(count_table(&conn, "operations"), 2);
}

#[test]
fn command_pure_create_derives_scope_from_intent_id() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut store = KernelStore::open(&path).expect("open");

    let task = task_id(0xCC);
    let cmd = command_id(0xCD);
    let envelope = command_envelope(
        cmd,
        None,
        None,
        Command::CreateTask(create_task_intent(task)),
    );
    assert_eq!(envelope.task_id, None, "CreateTask envelope stays unscoped");

    let receipt = store.execute(envelope).expect("create");
    let CommandReceipt::Accepted { .. } = receipt else {
        panic!("expected accepted, got {receipt:?}");
    };
    drop(store);

    let conn = open_raw(&path);
    let receipt_task: Vec<u8> = conn
        .query_row(
            "SELECT task_id FROM command_receipts WHERE command_id = ?1",
            [cmd.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .expect("receipt task_id");
    assert_eq!(receipt_task.as_slice(), task.as_bytes().as_slice());

    let event_task_ids: Vec<Option<Vec<u8>>> = {
        let mut stmt = conn
            .prepare("SELECT task_id FROM events ORDER BY sequence ASC")
            .unwrap();
        stmt.query_map([], |row| row.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    };
    assert_eq!(event_task_ids.len(), 3);
    for tid in event_task_ids {
        assert_eq!(
            tid.as_deref(),
            Some(task.as_bytes().as_slice()),
            "effective durable scope is CreateTaskIntent.id"
        );
    }

    let op_task: Vec<u8> = conn
        .query_row(
            "SELECT task_id FROM operations WHERE command_id = ?1",
            [cmd.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(op_task.as_slice(), task.as_bytes().as_slice());
}

#[test]
fn command_pure_effectful_empty_decision_stays_unsupported() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut store = KernelStore::open(&path).expect("open");

    let task = task_id(0xCE);
    store
        .execute(command_envelope(
            command_id(0xCF),
            None,
            None,
            Command::CreateTask(create_task_intent(task)),
        ))
        .expect("create");
    drop(store);

    let conn = open_raw(&path);
    conn.execute(
        "UPDATE tasks SET lifecycle = 'closing', action_epoch = 1 WHERE task_id = ?1",
        [task.as_bytes().as_slice()],
    )
    .unwrap();
    drop(conn);

    let mut store = KernelStore::open(&path).expect("reopen");
    let close_cmd = command_id(0xD0);
    let receipt = store
        .execute(command_envelope(
            close_cmd,
            Some(task),
            Some(1),
            Command::BeginCloseTask,
        ))
        .expect("already closing empty decision");
    assert_eq!(
        receipt,
        CommandReceipt::Rejected {
            command_id: close_cmd,
            code: RejectionCode::UnsupportedCapability,
            current_revision: Some(1),
        }
    );
    drop(store);

    let conn = open_raw(&path);
    assert_eq!(count_table(&conn, "outbox"), 0);
    let committed: Option<i64> = conn
        .query_row(
            "SELECT committed_sequence FROM command_receipts WHERE command_id = ?1",
            [close_cmd.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(committed, None);
}

fn create_open_task(store: &mut KernelStore, task: TaskId, cmd: CommandId) {
    store
        .execute(command_envelope(
            cmd,
            None,
            None,
            Command::CreateTask(create_task_intent(task)),
        ))
        .expect("create task");
}

fn seed_projection_rows(path: &Path, task: TaskId) {
    let conn = open_raw(path);
    let agent = agent_id(0xA1);
    let artifact = artifact_id(0xA2);
    let resource = resource_id(0xA3);
    conn.execute(
        "INSERT INTO agent_sessions(
            agent_session_id, task_id, role, provider_kind, provider_session_id,
            lifecycle, runtime_generation, revision
         ) VALUES (?1, ?2, ?3, 'claude', 'sess-1', 'open', 0, 0)",
        rusqlite::params![
            agent.as_bytes().as_slice(),
            task.as_bytes().as_slice(),
            rmp_serde::to_vec(&AgentRole::Primary).unwrap(),
        ],
    )
    .expect("seed agent");
    conn.execute(
        "INSERT INTO artifacts(
            artifact_id, task_id, kind, label, content_ref, sha256, privacy_class, created_at_ms
         ) VALUES (?1, ?2, 'finding', 'note', ?3, ?4, 'local_only', 1)",
        rusqlite::params![
            artifact.as_bytes().as_slice(),
            task.as_bytes().as_slice(),
            rmp_serde::to_vec(&ArtifactContentRef::InlineUtf8("body".into())).unwrap(),
            vec![0u8; 32],
        ],
    )
    .expect("seed artifact");
    conn.execute(
        "INSERT INTO resources(
            resource_id, task_id, owner_kind, resource_kind, recipe, lifecycle,
            runtime_generation, updated_at_ms
         ) VALUES (?1, ?2, 'task', 'terminal', ?3, 'active', 1, 1)",
        rusqlite::params![
            resource.as_bytes().as_slice(),
            task.as_bytes().as_slice(),
            rmp_serde::to_vec(&ResourceRecipe::Terminal { cols: 80, rows: 24 }).unwrap(),
        ],
    )
    .expect("seed resource");
}

fn assert_store_error_integrity(err: StoreError) {
    assert!(
        matches!(
            err,
            StoreError::Corruption
                | StoreError::CodecMismatch { .. }
                | StoreError::EventDecode(_)
                | StoreError::Projection(_)
        ),
        "expected fail-closed integrity error, got {err:?}"
    );
}

#[test]
fn command_pure_corrupt_accepted_missing_operation() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut store = KernelStore::open(&path).expect("open");
    let task = task_id(0xE1);
    let cmd = command_id(0xE2);
    create_open_task(&mut store, task, cmd);
    drop(store);

    let conn = open_raw(&path);
    conn.execute(
        "DELETE FROM operations WHERE command_id = ?1",
        [cmd.as_bytes().as_slice()],
    )
    .unwrap();
    drop(conn);

    let mut store = KernelStore::open(&path).expect("reopen");
    let err = store
        .execute(command_envelope(
            cmd,
            None,
            None,
            Command::CreateTask(create_task_intent(task)),
        ))
        .expect_err("missing operation must fail closed");
    assert_store_error_integrity(err);
}

#[test]
fn command_pure_corrupt_accepted_operation_id_mismatch() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut store = KernelStore::open(&path).expect("open");
    let task = task_id(0xE3);
    let cmd = command_id(0xE4);
    create_open_task(&mut store, task, cmd);
    drop(store);

    let conn = open_raw(&path);
    let wrong_op = operation_id(0xEE);
    // Replace the operation row with a mismatched operation_id.
    conn.execute(
        "DELETE FROM operations WHERE command_id = ?1",
        [cmd.as_bytes().as_slice()],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO operations(
            operation_id, command_id, task_id, resource_id, action_epoch, runtime_generation,
            state, result, outcome_code, accepted_at_ms, outcome_at_ms
         ) VALUES (?1, ?2, ?3, NULL, NULL, NULL, 'accepted', NULL, NULL, 1, NULL)",
        rusqlite::params![
            wrong_op.as_bytes().as_slice(),
            cmd.as_bytes().as_slice(),
            task.as_bytes().as_slice(),
        ],
    )
    .unwrap();
    drop(conn);

    let mut store = KernelStore::open(&path).expect("reopen");
    let err = store
        .execute(command_envelope(
            cmd,
            None,
            None,
            Command::CreateTask(create_task_intent(task)),
        ))
        .expect_err("operation_id mismatch must fail closed");
    assert_store_error_integrity(err);
}

#[test]
fn command_pure_corrupt_accepted_missing_committed_sequence() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut store = KernelStore::open(&path).expect("open");
    let task = task_id(0xE5);
    let cmd = command_id(0xE6);
    create_open_task(&mut store, task, cmd);
    drop(store);

    let conn = open_raw(&path);
    conn.execute(
        "UPDATE command_receipts SET committed_sequence = NULL WHERE command_id = ?1",
        [cmd.as_bytes().as_slice()],
    )
    .unwrap();
    drop(conn);

    let mut store = KernelStore::open(&path).expect("reopen");
    let err = store
        .execute(command_envelope(
            cmd,
            None,
            None,
            Command::CreateTask(create_task_intent(task)),
        ))
        .expect_err("accepted receipt without committed_sequence must fail");
    assert_store_error_integrity(err);
}

#[test]
fn command_pure_corrupt_accepted_missing_committed_event() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut store = KernelStore::open(&path).expect("open");
    let task = task_id(0xE7);
    let cmd = command_id(0xE8);
    create_open_task(&mut store, task, cmd);
    drop(store);

    let conn = open_raw(&path);
    let committed: i64 = conn
        .query_row(
            "SELECT committed_sequence FROM command_receipts WHERE command_id = ?1",
            [cmd.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    conn.execute("DELETE FROM events WHERE sequence = ?1", [committed])
        .unwrap();
    drop(conn);

    let mut store = KernelStore::open(&path).expect("reopen");
    let err = store
        .execute(command_envelope(
            cmd,
            None,
            None,
            Command::CreateTask(create_task_intent(task)),
        ))
        .expect_err("missing committed event must fail");
    assert_store_error_integrity(err);
}

#[test]
fn command_pure_corrupt_accepted_task_scope_mismatch() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut store = KernelStore::open(&path).expect("open");
    let task = task_id(0xE9);
    let cmd = command_id(0xEA);
    create_open_task(&mut store, task, cmd);
    drop(store);

    let other = task_id(0xEB);
    let conn = open_raw(&path);
    conn.execute(
        "UPDATE operations SET task_id = ?1 WHERE command_id = ?2",
        rusqlite::params![other.as_bytes().as_slice(), cmd.as_bytes().as_slice()],
    )
    .unwrap();
    drop(conn);

    let mut store = KernelStore::open(&path).expect("reopen");
    let err = store
        .execute(command_envelope(
            cmd,
            None,
            None,
            Command::CreateTask(create_task_intent(task)),
        ))
        .expect_err("task scope mismatch must fail");
    assert_store_error_integrity(err);
}

fn invalid_uuid_bytes() -> [u8; 16] {
    // 16 bytes with version nibble 4 — not UUIDv7.
    [
        0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x40, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x99,
    ]
}

fn assert_retry_create_fails_closed(path: &Path, task: TaskId, cmd: CommandId) {
    let events_before = {
        let conn = open_raw(path);
        count_table(&conn, "events")
    };
    let mut store = KernelStore::open(path).expect("reopen");
    let err = store
        .execute(command_envelope(
            cmd,
            None,
            None,
            Command::CreateTask(create_task_intent(task)),
        ))
        .expect_err("corrupt receipt correlation must fail closed");
    assert_store_error_integrity(err);
    drop(store);
    let conn = open_raw(path);
    assert_eq!(count_table(&conn, "events"), events_before);
}

fn encode_accepted_receipt_blob(
    command_id: CommandId,
    operation_id: OperationId,
    task_revision: Option<u64>,
    event_ids: Vec<EventId>,
) -> Vec<u8> {
    #[derive(Serialize)]
    #[serde(tag = "status", rename_all = "snake_case")]
    enum ReceiptBodyWire {
        Accepted {
            command_id: CommandId,
            operation_id: OperationId,
            task_revision: Option<u64>,
            event_ids: Vec<EventId>,
        },
    }
    #[derive(Serialize)]
    struct ReceiptDocumentWire {
        schema_version: u32,
        receipt: ReceiptBodyWire,
    }
    rmp_serde::to_vec_named(&ReceiptDocumentWire {
        schema_version: 1,
        receipt: ReceiptBodyWire::Accepted {
            command_id,
            operation_id,
            task_revision,
            event_ids,
        },
    })
    .expect("encode forged receipt")
}

#[test]
fn command_pure_corrupt_shared_invalid_task_scope() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut store = KernelStore::open(&path).expect("open");
    let task = task_id(0x11);
    let cmd = command_id(0x12);
    create_open_task(&mut store, task, cmd);
    drop(store);

    let invalid = invalid_uuid_bytes();
    let conn = open_raw(&path);
    conn.execute(
        "UPDATE command_receipts SET task_id = ?1 WHERE command_id = ?2",
        rusqlite::params![invalid.as_slice(), cmd.as_bytes().as_slice()],
    )
    .unwrap();
    conn.execute(
        "UPDATE operations SET task_id = ?1 WHERE command_id = ?2",
        rusqlite::params![invalid.as_slice(), cmd.as_bytes().as_slice()],
    )
    .unwrap();
    conn.execute(
        "UPDATE events SET task_id = ?1 WHERE task_id = ?2",
        rusqlite::params![invalid.as_slice(), task.as_bytes().as_slice()],
    )
    .unwrap();
    drop(conn);

    assert_retry_create_fails_closed(&path, task, cmd);
}

#[test]
fn command_pure_corrupt_rejected_invalid_task_scope() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut store = KernelStore::open(&path).expect("open");
    let task = task_id(0x13);
    create_open_task(&mut store, task, command_id(0x14));
    let conflict_cmd = command_id(0x15);
    store
        .execute(command_envelope(
            conflict_cmd,
            Some(task),
            Some(99),
            Command::RenameTask(RenameTaskIntent {
                title: "stale".into(),
            }),
        ))
        .expect("rejected");
    drop(store);

    let conn = open_raw(&path);
    let receipts_before = count_table(&conn, "command_receipts");
    conn.execute(
        "UPDATE command_receipts SET task_id = ?1 WHERE command_id = ?2",
        rusqlite::params![
            invalid_uuid_bytes().as_slice(),
            conflict_cmd.as_bytes().as_slice()
        ],
    )
    .unwrap();
    drop(conn);

    let mut store = KernelStore::open(&path).expect("reopen");
    let err = store
        .execute(command_envelope(
            conflict_cmd,
            Some(task),
            Some(99),
            Command::RenameTask(RenameTaskIntent {
                title: "stale".into(),
            }),
        ))
        .expect_err("rejected invalid task scope must fail");
    assert_store_error_integrity(err);
    drop(store);
    let conn = open_raw(&path);
    assert_eq!(count_table(&conn, "command_receipts"), receipts_before);
}

#[test]
fn command_pure_corrupt_committed_sequence_points_to_unrelated_event() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut store = KernelStore::open(&path).expect("open");
    let task = task_id(0x16);
    let cmd = command_id(0x17);
    create_open_task(&mut store, task, cmd);
    drop(store);

    let conn = open_raw(&path);
    // Redirect to same-task task.created instead of operation.settled.
    conn.execute(
        "UPDATE command_receipts SET committed_sequence = 1 WHERE command_id = ?1",
        [cmd.as_bytes().as_slice()],
    )
    .unwrap();
    drop(conn);

    assert_retry_create_fails_closed(&path, task, cmd);
}

#[test]
fn command_pure_corrupt_settled_fact_ids_mismatch() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut store = KernelStore::open(&path).expect("open");
    let task = task_id(0x18);
    let cmd = command_id(0x19);
    create_open_task(&mut store, task, cmd);
    drop(store);

    let conn = open_raw(&path);
    let forged = OperationSettledFact::new(
        command_id(0x1A),
        operation_id(0x1B),
        1_200,
        vec![event_id(0x1C)],
        None,
        None,
        None,
    )
    .expect("forged settled");
    conn.execute(
        "UPDATE events SET payload = ?1 WHERE event_type = 'operation.settled'",
        [rmp_serde::to_vec(&forged).unwrap()],
    )
    .unwrap();
    drop(conn);

    assert_retry_create_fails_closed(&path, task, cmd);
}

#[test]
fn command_pure_corrupt_decision_event_after_committed_sequence() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut store = KernelStore::open(&path).expect("open");
    let task = task_id(0x1D);
    let cmd = command_id(0x1E);
    create_open_task(&mut store, task, cmd);
    drop(store);

    let conn = open_raw(&path);
    conn.execute(
        "UPDATE events SET sequence = 50 WHERE event_type = 'task.created'",
        [],
    )
    .unwrap();
    drop(conn);

    assert_retry_create_fails_closed(&path, task, cmd);
}

#[test]
fn command_pure_corrupt_decision_event_at_committed_sequence() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut store = KernelStore::open(&path).expect("open");
    let task = task_id(0x21);
    let cmd = command_id(0x22);
    let receipt = store
        .execute(command_envelope(
            cmd,
            None,
            None,
            Command::CreateTask(create_task_intent(task)),
        ))
        .expect("create");
    let CommandReceipt::Accepted {
        operation_id,
        task_revision,
        ..
    } = receipt
    else {
        panic!("expected accepted");
    };
    drop(store);

    let conn = open_raw(&path);
    let settled_event_id = EventId::from_bytes(
        conn.query_row(
            "SELECT event_id FROM events WHERE event_type = 'operation.settled'",
            [],
            |row| {
                let bytes: Vec<u8> = row.get(0)?;
                let array: [u8; 16] = bytes.try_into().expect("16");
                Ok(array)
            },
        )
        .expect("settled id"),
    )
    .expect("event id");
    // Receipt claims the settled/committed event itself as a "decision" id.
    let forged =
        encode_accepted_receipt_blob(cmd, operation_id, task_revision, vec![settled_event_id]);
    conn.execute(
        "UPDATE command_receipts SET receipt = ?1 WHERE command_id = ?2",
        rusqlite::params![forged, cmd.as_bytes().as_slice()],
    )
    .unwrap();
    drop(conn);

    assert_retry_create_fails_closed(&path, task, cmd);
}

fn event_id_for_type(conn: &Connection, event_type: &str) -> EventId {
    let bytes: Vec<u8> = conn
        .query_row(
            "SELECT event_id FROM events WHERE event_type = ?1 ORDER BY sequence ASC LIMIT 1",
            [event_type],
            |row| row.get(0),
        )
        .expect("event id");
    EventId::from_bytes(bytes.try_into().expect("16")).expect("event id")
}

#[test]
fn command_pure_corrupt_forged_task_revision() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut store = KernelStore::open(&path).expect("open");
    let task = task_id(0x31);
    let cmd = command_id(0x32);
    let receipt = store
        .execute(command_envelope(
            cmd,
            None,
            None,
            Command::CreateTask(create_task_intent(task)),
        ))
        .expect("create");
    let CommandReceipt::Accepted {
        operation_id,
        event_ids,
        ..
    } = receipt
    else {
        panic!("expected accepted");
    };
    drop(store);

    let conn = open_raw(&path);
    let forged = encode_accepted_receipt_blob(cmd, operation_id, Some(99), event_ids);
    conn.execute(
        "UPDATE command_receipts SET receipt = ?1 WHERE command_id = ?2",
        rusqlite::params![forged, cmd.as_bytes().as_slice()],
    )
    .unwrap();
    drop(conn);

    assert_retry_create_fails_closed(&path, task, cmd);
}

#[test]
fn command_pure_corrupt_noncontiguous_foreign_earlier_mutation() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut store = KernelStore::open(&path).expect("open");
    let task = task_id(0x33);
    create_open_task(&mut store, task, command_id(0x34));
    let rename_cmd = command_id(0x35);
    let receipt = store
        .execute(command_envelope(
            rename_cmd,
            Some(task),
            Some(1),
            Command::RenameTask(RenameTaskIntent {
                title: "Renamed".into(),
            }),
        ))
        .expect("rename");
    let CommandReceipt::Accepted {
        operation_id,
        task_revision,
        ..
    } = receipt
    else {
        panic!("expected accepted rename");
    };
    drop(store);

    let conn = open_raw(&path);
    let earlier_created = event_id_for_type(&conn, "task.created");
    let forged = encode_accepted_receipt_blob(
        rename_cmd,
        operation_id,
        task_revision,
        vec![earlier_created],
    );
    conn.execute(
        "UPDATE command_receipts SET receipt = ?1 WHERE command_id = ?2",
        rusqlite::params![forged, rename_cmd.as_bytes().as_slice()],
    )
    .unwrap();
    // Keep settled.result_event_ids aligned so layout/contiguity is the failing axis.
    let settled = OperationSettledFact::new(
        rename_cmd,
        operation_id,
        1_200,
        vec![earlier_created],
        None,
        None,
        None,
    )
    .expect("settled");
    conn.execute(
        "UPDATE events SET payload = ?1
         WHERE sequence = (
            SELECT committed_sequence FROM command_receipts WHERE command_id = ?2
         )",
        rusqlite::params![
            rmp_serde::to_vec(&settled).unwrap(),
            rename_cmd.as_bytes().as_slice()
        ],
    )
    .unwrap();
    conn.execute(
        "UPDATE operations SET result = ?1 WHERE command_id = ?2",
        rusqlite::params![
            rmp_serde::to_vec(&vec![earlier_created]).unwrap(),
            rename_cmd.as_bytes().as_slice()
        ],
    )
    .unwrap();
    let events_before = count_table(&conn, "events");
    drop(conn);

    let mut store = KernelStore::open(&path).expect("reopen");
    let err = store
        .execute(command_envelope(
            rename_cmd,
            Some(task),
            Some(1),
            Command::RenameTask(RenameTaskIntent {
                title: "Renamed".into(),
            }),
        ))
        .expect_err("foreign earlier mutation must fail");
    assert_store_error_integrity(err);
    drop(store);
    let conn = open_raw(&path);
    assert_eq!(count_table(&conn, "events"), events_before);
}

#[test]
fn command_pure_corrupt_operation_state_not_settled() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut store = KernelStore::open(&path).expect("open");
    let task = task_id(0x36);
    let cmd = command_id(0x37);
    create_open_task(&mut store, task, cmd);
    drop(store);

    let conn = open_raw(&path);
    conn.execute(
        "UPDATE operations SET state = 'accepted', outcome_at_ms = NULL WHERE command_id = ?1",
        [cmd.as_bytes().as_slice()],
    )
    .unwrap();
    drop(conn);

    assert_retry_create_fails_closed(&path, task, cmd);
}

#[test]
fn command_pure_corrupt_operation_non_null_pure_fence() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut store = KernelStore::open(&path).expect("open");
    let task = task_id(0x38);
    let cmd = command_id(0x39);
    create_open_task(&mut store, task, cmd);
    drop(store);

    let conn = open_raw(&path);
    conn.execute(
        "UPDATE operations SET action_epoch = 1 WHERE command_id = ?1",
        [cmd.as_bytes().as_slice()],
    )
    .unwrap();
    drop(conn);

    assert_retry_create_fails_closed(&path, task, cmd);
}

#[test]
fn command_pure_corrupt_operation_result_mismatch() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut store = KernelStore::open(&path).expect("open");
    let task = task_id(0x3A);
    let cmd = command_id(0x3B);
    create_open_task(&mut store, task, cmd);
    drop(store);

    let conn = open_raw(&path);
    conn.execute(
        "UPDATE operations SET result = ?1 WHERE command_id = ?2",
        rusqlite::params![
            rmp_serde::to_vec(&vec![event_id(0x3C)]).unwrap(),
            cmd.as_bytes().as_slice()
        ],
    )
    .unwrap();
    drop(conn);

    assert_retry_create_fails_closed(&path, task, cmd);
}

#[test]
fn command_pure_corrupt_accepted_scopes_all_null() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut store = KernelStore::open(&path).expect("open");
    let task = task_id(0x41);
    let cmd = command_id(0x42);
    create_open_task(&mut store, task, cmd);
    drop(store);

    let conn = open_raw(&path);
    conn.execute(
        "UPDATE command_receipts SET task_id = NULL WHERE command_id = ?1",
        [cmd.as_bytes().as_slice()],
    )
    .unwrap();
    conn.execute(
        "UPDATE operations SET task_id = NULL WHERE command_id = ?1",
        [cmd.as_bytes().as_slice()],
    )
    .unwrap();
    conn.execute(
        "UPDATE events SET task_id = NULL WHERE task_id = ?1",
        [task.as_bytes().as_slice()],
    )
    .unwrap();
    drop(conn);

    assert_retry_create_fails_closed(&path, task, cmd);
}

#[test]
fn command_pure_corrupt_alternate_scope_keeps_created_payload() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut store = KernelStore::open(&path).expect("open");
    let task = task_id(0x43);
    let cmd = command_id(0x44);
    create_open_task(&mut store, task, cmd);
    drop(store);

    let other = task_id(0x45);
    let conn = open_raw(&path);
    conn.execute(
        "UPDATE command_receipts SET task_id = ?1 WHERE command_id = ?2",
        rusqlite::params![other.as_bytes().as_slice(), cmd.as_bytes().as_slice()],
    )
    .unwrap();
    conn.execute(
        "UPDATE operations SET task_id = ?1 WHERE command_id = ?2",
        rusqlite::params![other.as_bytes().as_slice(), cmd.as_bytes().as_slice()],
    )
    .unwrap();
    conn.execute(
        "UPDATE events SET task_id = ?1 WHERE task_id = ?2",
        rusqlite::params![other.as_bytes().as_slice(), task.as_bytes().as_slice()],
    )
    .unwrap();
    // TaskCreated payload still embeds the original task id.
    drop(conn);

    assert_retry_create_fails_closed(&path, task, cmd);
}

#[test]
fn command_pure_corrupt_create_revisions_forged_away_from_projection() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut store = KernelStore::open(&path).expect("open");
    let task = task_id(0x46);
    let cmd = command_id(0x47);
    let receipt = store
        .execute(command_envelope(
            cmd,
            None,
            None,
            Command::CreateTask(create_task_intent(task)),
        ))
        .expect("create");
    let CommandReceipt::Accepted {
        operation_id,
        event_ids,
        ..
    } = receipt
    else {
        panic!("expected accepted");
    };
    drop(store);

    let conn = open_raw(&path);
    let mut created = sample_task(task);
    created.revision = 99;
    let payload = TaskCreatedPayload {
        task: created,
        connectivity: TaskConnectivity::Connected,
        attention: TaskAttention::None,
        activity: TaskActivity::Idle,
        review_readiness: ReviewReadiness::NotReady,
    };
    conn.execute(
        "UPDATE events SET task_revision = 99, payload = ?1 WHERE event_type = 'task.created'",
        [rmp_serde::to_vec(&payload).unwrap()],
    )
    .unwrap();
    let forged = encode_accepted_receipt_blob(cmd, operation_id, Some(99), event_ids);
    conn.execute(
        "UPDATE command_receipts SET receipt = ?1 WHERE command_id = ?2",
        rusqlite::params![forged, cmd.as_bytes().as_slice()],
    )
    .unwrap();
    let projection_revision: i64 = conn
        .query_row(
            "SELECT revision FROM tasks WHERE task_id = ?1",
            [task.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(projection_revision, 1);
    drop(conn);

    assert_retry_create_fails_closed(&path, task, cmd);
}

#[test]
fn command_pure_corrupt_rename_revision_unanchored_from_prior() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut store = KernelStore::open(&path).expect("open");
    let task = task_id(0x48);
    create_open_task(&mut store, task, command_id(0x49));
    let rename_cmd = command_id(0x4A);
    let receipt = store
        .execute(command_envelope(
            rename_cmd,
            Some(task),
            Some(1),
            Command::RenameTask(RenameTaskIntent {
                title: "Renamed".into(),
            }),
        ))
        .expect("rename");
    let CommandReceipt::Accepted {
        operation_id,
        event_ids,
        ..
    } = receipt
    else {
        panic!("expected accepted rename");
    };
    drop(store);

    let conn = open_raw(&path);
    conn.execute(
        "UPDATE events SET task_revision = 50
         WHERE event_id = ?1",
        [event_ids[0].as_bytes().as_slice()],
    )
    .unwrap();
    let forged =
        encode_accepted_receipt_blob(rename_cmd, operation_id, Some(50), event_ids.clone());
    conn.execute(
        "UPDATE command_receipts SET receipt = ?1 WHERE command_id = ?2",
        rusqlite::params![forged, rename_cmd.as_bytes().as_slice()],
    )
    .unwrap();
    let events_before = count_table(&conn, "events");
    drop(conn);

    let mut store = KernelStore::open(&path).expect("reopen");
    let err = store
        .execute(command_envelope(
            rename_cmd,
            Some(task),
            Some(1),
            Command::RenameTask(RenameTaskIntent {
                title: "Renamed".into(),
            }),
        ))
        .expect_err("unanchored rename revision must fail");
    assert_store_error_integrity(err);
    drop(store);
    let conn = open_raw(&path);
    assert_eq!(count_table(&conn, "events"), events_before);
}

#[test]
fn command_pure_corrupt_operational_events_have_task_revision() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut store = KernelStore::open(&path).expect("open");
    let task = task_id(0x4B);
    let cmd = command_id(0x4C);
    create_open_task(&mut store, task, cmd);
    drop(store);

    let conn = open_raw(&path);
    conn.execute(
        "UPDATE events SET task_revision = 1
         WHERE event_type IN ('operation.accepted', 'operation.settled')",
        [],
    )
    .unwrap();
    drop(conn);

    assert_retry_create_fails_closed(&path, task, cmd);
}

fn primary_agent_facts(task: TaskId, agent: AgentSessionId) -> AgentSessionFacts {
    AgentSessionFacts {
        id: agent,
        task_id: task,
        role: AgentRole::Primary,
        provider_kind: "claude".into(),
        provider_session_id: Some("sess-primary".into()),
        lifecycle: AgentSessionLifecycle::Open,
        runtime_generation: 0,
        revision: 0,
    }
}

fn register_and_set_primary(
    store: &mut KernelStore,
    task: TaskId,
    agent: AgentSessionId,
    register_cmd: CommandId,
    set_cmd: CommandId,
    expected_revision: u64,
) {
    store
        .execute(command_envelope(
            register_cmd,
            Some(task),
            Some(expected_revision),
            Command::RegisterAgentSession {
                agent: primary_agent_facts(task, agent),
            },
        ))
        .expect("register primary");
    store
        .execute(command_envelope(
            set_cmd,
            Some(task),
            Some(expected_revision + 1),
            Command::SetPrimaryAgent {
                agent_session_id: agent,
            },
        ))
        .expect("set primary");
}

#[test]
fn command_pure_corrupt_primary_agent_set_cross_task() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut store = KernelStore::open(&path).expect("open");

    let task_a = task_id(0x51);
    let task_b = task_id(0x52);
    let agent_a = agent_id(0x53);
    let agent_b = agent_id(0x54);
    create_open_task(&mut store, task_a, command_id(0x55));
    create_open_task(&mut store, task_b, command_id(0x56));
    register_and_set_primary(
        &mut store,
        task_a,
        agent_a,
        command_id(0x57),
        command_id(0x58),
        1,
    );
    register_and_set_primary(
        &mut store,
        task_b,
        agent_b,
        command_id(0x59),
        command_id(0x5A),
        1,
    );
    drop(store);

    let set_cmd = command_id(0x58);
    let conn = open_raw(&path);
    let events_before = count_table(&conn, "events");
    conn.execute(
        "UPDATE events SET payload = ?1
         WHERE event_type = 'primary_agent.set' AND task_id = ?2",
        rusqlite::params![
            rmp_serde::to_vec(&PrimaryAgentSetPayload {
                agent_session_id: agent_b,
            })
            .unwrap(),
            task_a.as_bytes().as_slice(),
        ],
    )
    .unwrap();
    drop(conn);

    let mut store = KernelStore::open(&path).expect("reopen");
    let err = store
        .execute(command_envelope(
            set_cmd,
            Some(task_a),
            Some(2),
            Command::SetPrimaryAgent {
                agent_session_id: agent_a,
            },
        ))
        .expect_err("cross-task primary agent must fail closed");
    assert_store_error_integrity(err);
    drop(store);
    let conn = open_raw(&path);
    assert_eq!(count_table(&conn, "events"), events_before);
}

#[test]
fn command_pure_corrupt_primary_agent_set_specialist_same_task() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut store = KernelStore::open(&path).expect("open");

    let task = task_id(0x5B);
    let primary = agent_id(0x5C);
    let specialist = agent_id(0x5D);
    create_open_task(&mut store, task, command_id(0x5E));
    register_and_set_primary(
        &mut store,
        task,
        primary,
        command_id(0x5F),
        command_id(0x60),
        1,
    );
    drop(store);

    let conn = open_raw(&path);
    conn.execute(
        "INSERT INTO agent_sessions(
            agent_session_id, task_id, role, provider_kind, provider_session_id,
            lifecycle, runtime_generation, revision
         ) VALUES (?1, ?2, ?3, 'claude', 'sess-spec', 'open', 0, 0)",
        rusqlite::params![
            specialist.as_bytes().as_slice(),
            task.as_bytes().as_slice(),
            rmp_serde::to_vec(&AgentRole::specialist("reviewer").unwrap()).unwrap(),
        ],
    )
    .unwrap();
    let events_before = count_table(&conn, "events");
    conn.execute(
        "UPDATE events SET payload = ?1
         WHERE event_type = 'primary_agent.set' AND task_id = ?2",
        rusqlite::params![
            rmp_serde::to_vec(&PrimaryAgentSetPayload {
                agent_session_id: specialist,
            })
            .unwrap(),
            task.as_bytes().as_slice(),
        ],
    )
    .unwrap();
    drop(conn);

    let mut store = KernelStore::open(&path).expect("reopen");
    let err = store
        .execute(command_envelope(
            command_id(0x60),
            Some(task),
            Some(2),
            Command::SetPrimaryAgent {
                agent_session_id: primary,
            },
        ))
        .expect_err("specialist primary agent must fail closed");
    assert_store_error_integrity(err);
    drop(store);
    let conn = open_raw(&path);
    assert_eq!(count_table(&conn, "events"), events_before);
}

#[test]
fn command_pure_corrupt_rejected_with_operation() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut store = KernelStore::open(&path).expect("open");
    let task = task_id(0xEC);
    create_open_task(&mut store, task, command_id(0xED));

    let conflict_cmd = command_id(0xEE);
    let rejected = store
        .execute(command_envelope(
            conflict_cmd,
            Some(task),
            Some(99),
            Command::RenameTask(RenameTaskIntent {
                title: "stale".into(),
            }),
        ))
        .expect("rejected");
    assert!(matches!(rejected, CommandReceipt::Rejected { .. }));
    drop(store);

    let conn = open_raw(&path);
    conn.execute(
        "INSERT INTO operations(
            operation_id, command_id, task_id, resource_id, action_epoch, runtime_generation,
            state, result, outcome_code, accepted_at_ms, outcome_at_ms
         ) VALUES (?1, ?2, ?3, NULL, NULL, NULL, 'accepted', NULL, NULL, 1, NULL)",
        rusqlite::params![
            operation_id(0xEF).as_bytes().as_slice(),
            conflict_cmd.as_bytes().as_slice(),
            task.as_bytes().as_slice(),
        ],
    )
    .unwrap();
    drop(conn);

    let mut store = KernelStore::open(&path).expect("reopen");
    let err = store
        .execute(command_envelope(
            conflict_cmd,
            Some(task),
            Some(99),
            Command::RenameTask(RenameTaskIntent {
                title: "stale".into(),
            }),
        ))
        .expect_err("rejected receipt with operation must fail");
    assert_store_error_integrity(err);
}

#[test]
fn command_pure_already_closing_begin_close_unsupported() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut store = KernelStore::open(&path).expect("open");
    let task = task_id(0xF1);
    create_open_task(&mut store, task, command_id(0xF2));
    drop(store);

    let conn = open_raw(&path);
    conn.execute(
        "UPDATE tasks SET lifecycle = 'closing', action_epoch = 1 WHERE task_id = ?1",
        [task.as_bytes().as_slice()],
    )
    .unwrap();
    let events_before = count_table(&conn, "events");
    let ops_before = count_table(&conn, "operations");
    let receipts_before = count_table(&conn, "command_receipts");
    drop(conn);

    let mut store = KernelStore::open(&path).expect("reopen");
    let close_cmd = command_id(0xF3);
    let receipt = store
        .execute(command_envelope(
            close_cmd,
            Some(task),
            Some(1),
            Command::BeginCloseTask,
        ))
        .expect("already closing");
    assert_eq!(
        receipt,
        CommandReceipt::Rejected {
            command_id: close_cmd,
            code: RejectionCode::UnsupportedCapability,
            current_revision: Some(1),
        }
    );
    let retry = store
        .execute(command_envelope(
            close_cmd,
            Some(task),
            Some(1),
            Command::BeginCloseTask,
        ))
        .expect("retry");
    assert_eq!(retry, receipt);
    drop(store);

    let conn = open_raw(&path);
    assert_eq!(count_table(&conn, "events"), events_before);
    assert_eq!(count_table(&conn, "operations"), ops_before);
    assert_eq!(count_table(&conn, "command_receipts"), receipts_before + 1);
    assert_eq!(count_table(&conn, "outbox"), 0);
}

#[test]
fn command_pure_already_releasing_release_resource_unsupported() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut store = KernelStore::open(&path).expect("open");
    let task = task_id(0xF4);
    create_open_task(&mut store, task, command_id(0xF5));
    drop(store);

    let resource = resource_id(0xF6);
    let conn = open_raw(&path);
    conn.execute(
        "INSERT INTO resources(
            resource_id, task_id, owner_kind, resource_kind, recipe, lifecycle,
            runtime_generation, updated_at_ms
         ) VALUES (?1, ?2, 'task', 'terminal', ?3, 'releasing', 3, 1)",
        rusqlite::params![
            resource.as_bytes().as_slice(),
            task.as_bytes().as_slice(),
            rmp_serde::to_vec(&ResourceRecipe::Terminal { cols: 80, rows: 24 }).unwrap(),
        ],
    )
    .unwrap();
    let events_before = count_table(&conn, "events");
    let ops_before = count_table(&conn, "operations");
    drop(conn);

    let mut store = KernelStore::open(&path).expect("reopen");
    let release_cmd = command_id(0xF7);
    let receipt = store
        .execute(command_envelope(
            release_cmd,
            Some(task),
            Some(1),
            Command::ReleaseResource {
                resource_id: resource,
            },
        ))
        .expect("already releasing");
    assert_eq!(
        receipt,
        CommandReceipt::Rejected {
            command_id: release_cmd,
            code: RejectionCode::UnsupportedCapability,
            current_revision: Some(1),
        }
    );
    drop(store);

    let conn = open_raw(&path);
    assert_eq!(count_table(&conn, "events"), events_before);
    assert_eq!(count_table(&conn, "operations"), ops_before);
    assert_eq!(count_table(&conn, "outbox"), 0);
}

#[test]
fn command_pure_effectful_domain_rejection_still_wins() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut store = KernelStore::open(&path).expect("open");
    let task = task_id(0xF8);
    create_open_task(&mut store, task, command_id(0xF9));

    let receipt = store
        .execute(command_envelope(
            command_id(0xFA),
            Some(task),
            Some(99),
            Command::BeginCloseTask,
        ))
        .expect("revision conflict");
    assert_eq!(
        receipt,
        CommandReceipt::Rejected {
            command_id: command_id(0xFA),
            code: RejectionCode::RevisionConflict,
            current_revision: Some(1),
        }
    );
}

#[test]
fn command_pure_populated_snapshot_loads_valid_rows() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut store = KernelStore::open(&path).expect("open");
    let task = task_id(0xB1);
    create_open_task(&mut store, task, command_id(0xB2));
    drop(store);

    seed_projection_rows(&path, task);

    let mut store = KernelStore::open(&path).expect("reopen");
    let receipt = store
        .execute(command_envelope(
            command_id(0xB3),
            Some(task),
            Some(1),
            Command::RenameTask(RenameTaskIntent {
                title: "With snapshot peers".into(),
            }),
        ))
        .expect("rename with populated snapshot");
    assert!(matches!(
        receipt,
        CommandReceipt::Accepted {
            task_revision: Some(2),
            ..
        }
    ));
}

fn corrupt_blob_and_rename_must_roll_back(
    path: &Path,
    task: TaskId,
    mutate: impl FnOnce(&Connection),
) {
    let conn = open_raw(path);
    let events_before = count_table(&conn, "events");
    let receipts_before = count_table(&conn, "command_receipts");
    let title_before: String = conn
        .query_row(
            "SELECT title FROM tasks WHERE task_id = ?1",
            [task.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    mutate(&conn);
    drop(conn);

    let mut store = KernelStore::open(path).expect("reopen");
    let rename_cmd = command_id(0xD1);
    let err = store
        .execute(command_envelope(
            rename_cmd,
            Some(task),
            Some(1),
            Command::RenameTask(RenameTaskIntent {
                title: "Should not commit".into(),
            }),
        ))
        .expect_err("noncanonical projection blob must fail closed");
    assert_store_error_integrity(err);
    drop(store);

    let conn = open_raw(path);
    assert_eq!(count_table(&conn, "events"), events_before);
    assert_eq!(count_table(&conn, "command_receipts"), receipts_before);
    let title_after: String = conn
        .query_row(
            "SELECT title FROM tasks WHERE task_id = ?1",
            [task.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(title_after, title_before);
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM command_receipts WHERE command_id = ?1",
            [rename_cmd.as_bytes().as_slice()],
            |row| row.get::<_, i64>(0),
        )
        .unwrap(),
        0
    );
}

#[test]
fn command_pure_corrupt_noncanonical_workspace_branch_rolls_back() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut store = KernelStore::open(&path).expect("open");
    let task = task_id(0xB4);
    create_open_task(&mut store, task, command_id(0xB5));
    drop(store);

    #[derive(Serialize)]
    #[serde(rename_all = "snake_case")]
    enum RawWorkspace {
        Worktree { path: PathBuf, branch: String },
    }

    corrupt_blob_and_rename_must_roll_back(&path, task, |conn| {
        conn.execute(
            "UPDATE tasks SET workspace = ?1 WHERE task_id = ?2",
            rusqlite::params![
                rmp_serde::to_vec(&RawWorkspace::Worktree {
                    path: PathBuf::from("wt"),
                    branch: " main ".into(),
                })
                .unwrap(),
                task.as_bytes().as_slice(),
            ],
        )
        .unwrap();
    });
}

#[test]
fn command_pure_corrupt_noncanonical_assignment_principal_rolls_back() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut store = KernelStore::open(&path).expect("open");
    let task = task_id(0xB6);
    create_open_task(&mut store, task, command_id(0xB7));
    drop(store);

    #[derive(Serialize)]
    #[serde(rename_all = "snake_case")]
    enum RawAssignment {
        ExternalPrincipal { authority: String, subject: String },
    }

    corrupt_blob_and_rename_must_roll_back(&path, task, |conn| {
        conn.execute(
            "UPDATE tasks SET assignment = ?1 WHERE task_id = ?2",
            rusqlite::params![
                rmp_serde::to_vec(&RawAssignment::ExternalPrincipal {
                    authority: " org ".into(),
                    subject: "user".into(),
                })
                .unwrap(),
                task.as_bytes().as_slice(),
            ],
        )
        .unwrap();
    });
}

#[test]
fn command_pure_corrupt_noncanonical_agent_specialist_rolls_back() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut store = KernelStore::open(&path).expect("open");
    let task = task_id(0xB8);
    create_open_task(&mut store, task, command_id(0xB9));
    drop(store);
    seed_projection_rows(&path, task);

    #[derive(Serialize)]
    #[serde(rename_all = "snake_case")]
    enum RawRole {
        Specialist { name: String },
    }

    corrupt_blob_and_rename_must_roll_back(&path, task, |conn| {
        conn.execute(
            "UPDATE agent_sessions SET role = ?1 WHERE task_id = ?2",
            rusqlite::params![
                rmp_serde::to_vec(&RawRole::Specialist {
                    name: " reviewer ".into(),
                })
                .unwrap(),
                task.as_bytes().as_slice(),
            ],
        )
        .unwrap();
    });
}

#[test]
fn command_pure_corrupt_noncanonical_artifact_digest_rolls_back() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut store = KernelStore::open(&path).expect("open");
    let task = task_id(0xBA);
    create_open_task(&mut store, task, command_id(0xBB));
    drop(store);
    seed_projection_rows(&path, task);

    #[derive(Serialize)]
    #[serde(rename_all = "snake_case")]
    enum RawContentRef {
        ContentAddressed { digest_hex: String },
    }

    corrupt_blob_and_rename_must_roll_back(&path, task, |conn| {
        conn.execute(
            "UPDATE artifacts SET content_ref = ?1 WHERE task_id = ?2",
            rusqlite::params![
                rmp_serde::to_vec(&RawContentRef::ContentAddressed {
                    digest_hex: " abc ".into(),
                })
                .unwrap(),
                task.as_bytes().as_slice(),
            ],
        )
        .unwrap();
    });
}

#[test]
fn command_pure_corrupt_noncanonical_resource_recipe_rolls_back() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut store = KernelStore::open(&path).expect("open");
    let task = task_id(0xBC);
    create_open_task(&mut store, task, command_id(0xBD));
    drop(store);
    seed_projection_rows(&path, task);

    #[derive(Serialize)]
    #[serde(rename_all = "snake_case")]
    enum RawRecipe {
        Browser { start_url: String },
    }

    corrupt_blob_and_rename_must_roll_back(&path, task, |conn| {
        conn.execute(
            "UPDATE resources SET resource_kind = 'browser_context', recipe = ?1 WHERE task_id = ?2",
            rusqlite::params![
                rmp_serde::to_vec(&RawRecipe::Browser {
                    start_url: " https://example.com ".into(),
                })
                .unwrap(),
                task.as_bytes().as_slice(),
            ],
        )
        .unwrap();
    });
}

fn seed_active_resource(path: &Path, task: TaskId, resource: ResourceId, generation: u64) {
    let conn = open_raw(path);
    let current_revision: i64 = conn
        .query_row(
            "SELECT revision FROM tasks WHERE task_id = ?1",
            [task.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .expect("task revision");
    let next_revision = current_revision + 1;
    let resource_facts = ResourceFacts {
        id: resource,
        task_id: Some(task),
        owner_kind: OwnerKind::Task,
        resource_kind: ResourceKind::Terminal,
        recipe: ResourceRecipe::Terminal { cols: 80, rows: 24 },
        lifecycle: ResourceLifecycle::Active,
        runtime_generation: generation,
        updated_at_ms: 1,
    };
    insert_event(
        &conn,
        EventId::new(),
        Some(task),
        Some(next_revision as u64),
        "resource.registered",
        i64::from(EVENT_SCHEMA_VERSION),
        1,
        &rmp_serde::to_vec(&ResourceRegisteredPayload {
            resource: resource_facts,
        })
        .unwrap(),
    );
    conn.execute(
        "UPDATE tasks SET revision = ?1, updated_at_ms = 1 WHERE task_id = ?2",
        rusqlite::params![next_revision, task.as_bytes().as_slice()],
    )
    .expect("bump task revision for seeded resource");
    conn.execute(
        "INSERT INTO resources(
            resource_id, task_id, owner_kind, resource_kind, recipe, lifecycle,
            runtime_generation, updated_at_ms
         ) VALUES (?1, ?2, 'task', 'terminal', ?3, 'active', ?4, 1)",
        rusqlite::params![
            resource.as_bytes().as_slice(),
            task.as_bytes().as_slice(),
            rmp_serde::to_vec(&ResourceRecipe::Terminal { cols: 80, rows: 24 }).unwrap(),
            generation as i64,
        ],
    )
    .expect("seed active resource");
}

fn external_idempotency_key(operation_id: OperationId, effect_index: u32) -> String {
    format!("{operation_id}:{effect_index}")
}

fn load_outbox_row(
    conn: &Connection,
    operation_id: OperationId,
) -> (
    Vec<u8>,
    i64,
    i64,
    String,
    String,
    Vec<u8>,
    String,
    i64,
    Option<i64>,
    Option<i64>,
    i64,
    Option<String>,
) {
    conn.query_row(
        "SELECT outbox_id, effect_index, event_sequence, destination_class, replay_policy,
                payload, state, available_at_ms, leased_until_ms, dispatch_started_at_ms,
                attempts, last_error_class
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
                row.get(6)?,
                row.get(7)?,
                row.get(8)?,
                row.get(9)?,
                row.get(10)?,
                row.get(11)?,
            ))
        },
    )
    .expect("outbox row")
}

#[test]
fn command_side_effect_begin_close_accepts_pending_outbox_without_settlement() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut store = KernelStore::open(&path).expect("open");
    let task = task_id(0x01);
    create_open_task(&mut store, task, command_id(0x02));

    let close_cmd = command_id(0x03);
    let receipt = store
        .execute(command_envelope(
            close_cmd,
            Some(task),
            Some(1),
            Command::BeginCloseTask,
        ))
        .expect("begin close");
    let CommandReceipt::Accepted {
        command_id,
        operation_id,
        task_revision,
        event_ids,
    } = receipt.clone()
    else {
        panic!("expected accepted begin close, got {receipt:?}");
    };
    assert_eq!(command_id, close_cmd);
    assert_eq!(task_revision, Some(2));
    assert_eq!(event_ids.len(), 1);
    drop(store);

    let conn = open_raw(&path);
    let types = event_types(&conn);
    assert_eq!(
        &types[3..],
        &[
            "task.close_begun".to_string(),
            "operation.accepted".to_string(),
        ]
    );
    assert!(
        !types[3..].iter().any(|t| t == "operation.settled"),
        "side-effect acceptance must not append operation.settled"
    );

    let (lifecycle, action_epoch, revision): (String, i64, i64) = conn
        .query_row(
            "SELECT lifecycle, action_epoch, revision FROM tasks WHERE task_id = ?1",
            [task.as_bytes().as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(lifecycle, "closing");
    assert_eq!(action_epoch, 1);
    assert_eq!(revision, 2);

    let (op_state, op_epoch, op_resource, op_gen, outcome_at): (
        String,
        Option<i64>,
        Option<Vec<u8>>,
        Option<i64>,
        Option<i64>,
    ) = conn
        .query_row(
            "SELECT state, action_epoch, resource_id, runtime_generation, outcome_at_ms
             FROM operations WHERE command_id = ?1",
            [close_cmd.as_bytes().as_slice()],
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
        .unwrap();
    assert_eq!(op_state, "accepted");
    assert_eq!(op_epoch, Some(1));
    assert!(op_resource.is_none());
    assert!(op_gen.is_none());
    assert!(outcome_at.is_none());

    let accepted_payload: Vec<u8> = conn
        .query_row(
            "SELECT payload FROM events WHERE event_type = 'operation.accepted'
             ORDER BY sequence DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let accepted: OperationAcceptedFact = rmp_serde::from_slice(&accepted_payload).unwrap();
    assert_eq!(accepted.operation_id, operation_id);
    assert_eq!(accepted.action_epoch, Some(1));
    assert_eq!(accepted.resource_id, None);
    assert_eq!(accepted.runtime_generation, None);

    let committed: i64 = conn
        .query_row(
            "SELECT committed_sequence FROM command_receipts WHERE command_id = ?1",
            [close_cmd.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    let accepted_sequence: i64 = conn
        .query_row(
            "SELECT sequence FROM events WHERE event_type = 'operation.accepted'
             ORDER BY sequence DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(committed, accepted_sequence);

    let decision_event_id: Vec<u8> = conn
        .query_row(
            "SELECT event_id FROM events WHERE event_type = 'task.close_begun'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        event_ids[0].as_bytes().as_slice(),
        decision_event_id.as_slice()
    );

    assert_eq!(count_table(&conn, "outbox"), 1);
    let (
        outbox_id,
        effect_index,
        event_sequence,
        destination,
        policy,
        payload,
        state,
        available_at,
        leased,
        dispatch_started,
        attempts,
        last_error,
    ) = load_outbox_row(&conn, operation_id);
    assert_eq!(outbox_id.len(), 16);
    assert_eq!(effect_index, 0);
    assert_eq!(event_sequence, accepted_sequence);
    assert_eq!(destination, "task_teardown");
    assert_eq!(policy, "retry_safe");
    assert_eq!(state, "pending");
    assert!(available_at > 0);
    assert!(leased.is_none());
    assert!(dispatch_started.is_none());
    assert_eq!(attempts, 0);
    assert!(last_error.is_none());
    assert!(!payload.is_empty());
    assert_eq!(
        external_idempotency_key(operation_id, 0),
        format!("{operation_id}:0")
    );
}

#[test]
fn command_side_effect_release_resource_fences_task_epoch_and_generation() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut store = KernelStore::open(&path).expect("open");
    let task = task_id(0x04);
    create_open_task(&mut store, task, command_id(0x05));
    drop(store);

    let resource = resource_id(0x06);
    seed_active_resource(&path, task, resource, 7);

    let mut store = KernelStore::open(&path).expect("reopen");
    let release_cmd = command_id(0x07);
    let receipt = store
        .execute(command_envelope(
            release_cmd,
            Some(task),
            Some(2),
            Command::ReleaseResource {
                resource_id: resource,
            },
        ))
        .expect("release resource");
    let CommandReceipt::Accepted {
        operation_id,
        task_revision,
        event_ids,
        ..
    } = receipt
    else {
        panic!("expected accepted release, got {receipt:?}");
    };
    assert_eq!(task_revision, Some(3));
    assert_eq!(event_ids.len(), 1);
    drop(store);

    let conn = open_raw(&path);
    let types = event_types(&conn);
    assert_eq!(
        &types[types.len() - 2..],
        &[
            "resource.release_begun".to_string(),
            "operation.accepted".to_string(),
        ]
    );
    assert!(
        types.iter().any(|t| t == "resource.registered"),
        "seeded resource must leave a durable registration fact"
    );
    assert!(
        !types[types.len() - 2..]
            .iter()
            .any(|t| t == "operation.settled"),
        "side-effect acceptance must not append operation.settled"
    );

    let (lifecycle, generation): (String, i64) = conn
        .query_row(
            "SELECT lifecycle, runtime_generation FROM resources WHERE resource_id = ?1",
            [resource.as_bytes().as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(lifecycle, "releasing");
    assert_eq!(generation, 7);

    let (op_state, op_epoch, op_resource, op_gen): (
        String,
        Option<i64>,
        Option<Vec<u8>>,
        Option<i64>,
    ) = conn
        .query_row(
            "SELECT state, action_epoch, resource_id, runtime_generation
             FROM operations WHERE command_id = ?1",
            [release_cmd.as_bytes().as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(op_state, "accepted");
    assert_eq!(
        op_epoch,
        Some(0),
        "fence uses pre-command task action_epoch"
    );
    assert_eq!(op_resource.as_deref(), Some(resource.as_bytes().as_slice()));
    assert_eq!(op_gen, Some(7));

    let accepted_payload: Vec<u8> = conn
        .query_row(
            "SELECT payload FROM events WHERE event_type = 'operation.accepted'
             ORDER BY sequence DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let accepted: OperationAcceptedFact = rmp_serde::from_slice(&accepted_payload).unwrap();
    assert_eq!(accepted.action_epoch, Some(0));
    assert_eq!(accepted.resource_id, Some(resource));
    assert_eq!(accepted.runtime_generation, Some(7));

    let (
        _outbox_id,
        effect_index,
        event_sequence,
        destination,
        policy,
        _payload,
        state,
        _available_at,
        leased,
        dispatch_started,
        attempts,
        last_error,
    ) = load_outbox_row(&conn, operation_id);
    assert_eq!(effect_index, 0);
    assert_eq!(destination, "resource_release");
    assert_eq!(policy, "reconcile_before_retry");
    assert_eq!(state, "pending");
    assert!(leased.is_none());
    assert!(dispatch_started.is_none());
    assert_eq!(attempts, 0);
    assert!(last_error.is_none());

    let committed: i64 = conn
        .query_row(
            "SELECT committed_sequence FROM command_receipts WHERE command_id = ?1",
            [release_cmd.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(committed, event_sequence);
}

#[test]
fn command_side_effect_retry_before_and_after_reopen_is_stable() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut store = KernelStore::open(&path).expect("open");
    let task = task_id(0x08);
    create_open_task(&mut store, task, command_id(0x09));

    let close_cmd = command_id(0x0A);
    let envelope = command_envelope(close_cmd, Some(task), Some(1), Command::BeginCloseTask);
    let first = store.execute(envelope.clone()).expect("first close");
    let CommandReceipt::Accepted {
        operation_id,
        event_ids,
        ..
    } = first.clone()
    else {
        panic!("expected accepted, got {first:?}");
    };
    let retry_same = store
        .execute(envelope.clone())
        .expect("retry same connection");
    assert_eq!(retry_same, first);
    drop(store);

    let conn = open_raw(&path);
    let original_receipt = receipt_blob(&conn, close_cmd);
    let events_before = count_table(&conn, "events");
    let outbox_before = count_table(&conn, "outbox");
    let (
        outbox_id,
        effect_index,
        event_sequence,
        destination,
        policy,
        payload,
        state,
        available_at,
        leased,
        dispatch_started,
        attempts,
        last_error,
    ) = load_outbox_row(&conn, operation_id);
    let key = external_idempotency_key(operation_id, effect_index as u32);
    drop(conn);

    let mut store = KernelStore::open(&path).expect("reopen");
    let retry_reopen = store.execute(envelope).expect("retry after reopen");
    assert_eq!(retry_reopen, first);
    drop(store);

    let conn = open_raw(&path);
    assert_eq!(receipt_blob(&conn, close_cmd), original_receipt);
    assert_eq!(count_table(&conn, "events"), events_before);
    assert_eq!(count_table(&conn, "outbox"), outbox_before);
    let (
        outbox_id2,
        effect_index2,
        event_sequence2,
        destination2,
        policy2,
        payload2,
        state2,
        available_at2,
        leased2,
        dispatch_started2,
        attempts2,
        last_error2,
    ) = load_outbox_row(&conn, operation_id);
    assert_eq!(outbox_id2, outbox_id);
    assert_eq!(effect_index2, effect_index);
    assert_eq!(event_sequence2, event_sequence);
    assert_eq!(destination2, destination);
    assert_eq!(policy2, policy);
    assert_eq!(payload2, payload);
    assert_eq!(state2, state);
    assert_eq!(available_at2, available_at);
    assert_eq!(leased2, leased);
    assert_eq!(dispatch_started2, dispatch_started);
    assert_eq!(attempts2, attempts);
    assert_eq!(last_error2, last_error);
    assert_eq!(
        external_idempotency_key(operation_id, effect_index2 as u32),
        key
    );
    assert_eq!(event_ids.len(), 1);
}

#[test]
fn command_side_effect_pure_command_never_gets_outbox_row() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut store = KernelStore::open(&path).expect("open");
    let task = task_id(0x0B);
    create_open_task(&mut store, task, command_id(0x0C));
    store
        .execute(command_envelope(
            command_id(0x0D),
            Some(task),
            Some(1),
            Command::RenameTask(RenameTaskIntent {
                title: "Pure rename".into(),
            }),
        ))
        .expect("rename");
    drop(store);

    let conn = open_raw(&path);
    assert_eq!(count_table(&conn, "outbox"), 0);
    assert!(event_types(&conn).iter().any(|t| t == "operation.settled"));
}

#[test]
fn command_side_effect_corrupt_outbox_axes_fail_closed() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut store = KernelStore::open(&path).expect("open");
    let task = task_id(0x0E);
    create_open_task(&mut store, task, command_id(0x0F));
    let close_cmd = command_id(0x10);
    let envelope = command_envelope(close_cmd, Some(task), Some(1), Command::BeginCloseTask);
    let first = store.execute(envelope.clone()).expect("close");
    let CommandReceipt::Accepted { .. } = first else {
        panic!("expected accepted");
    };
    drop(store);

    let forged_operation = operation_id(0xEE);
    let create_operation: Vec<u8> = {
        let conn = open_raw(&path);
        conn.query_row(
            "SELECT operation_id FROM operations WHERE command_id = ?1",
            [command_id(0x0F).as_bytes().as_slice()],
            |row| row.get(0),
        )
        .expect("create operation")
    };
    let cases: Vec<(&str, Box<dyn Fn(&Connection)>)> = vec![
        (
            "tampered effect_index",
            Box::new(|conn| {
                conn.execute("UPDATE outbox SET effect_index = 1", [])
                    .unwrap();
            }),
        ),
        (
            "tampered operation_id",
            Box::new(move |conn| {
                // Point at another real operation so FK stays satisfied while lookup fails closed.
                let _ = forged_operation;
                conn.execute(
                    "UPDATE outbox SET operation_id = ?1",
                    [create_operation.as_slice()],
                )
                .unwrap();
            }),
        ),
        (
            "tampered event_sequence",
            Box::new(|conn| {
                conn.execute("UPDATE outbox SET event_sequence = 1", [])
                    .unwrap();
            }),
        ),
        (
            "tampered destination",
            Box::new(|conn| {
                conn.execute(
                    "UPDATE outbox SET destination_class = 'resource_release'",
                    [],
                )
                .unwrap();
            }),
        ),
        (
            "tampered policy",
            Box::new(|conn| {
                conn.execute(
                    "UPDATE outbox SET replay_policy = 'reconcile_before_retry'",
                    [],
                )
                .unwrap();
            }),
        ),
        (
            "tampered payload",
            Box::new(|conn| {
                conn.execute("UPDATE outbox SET payload = X'00'", [])
                    .unwrap();
            }),
        ),
        (
            "tampered action_epoch fence",
            Box::new(|conn| {
                conn.execute(
                    "UPDATE operations SET action_epoch = 99 WHERE command_id = ?1",
                    [close_cmd.as_bytes().as_slice()],
                )
                .unwrap();
            }),
        ),
        (
            "tampered resource fence on teardown op",
            Box::new(|conn| {
                conn.execute(
                    "UPDATE operations SET resource_id = ?1, runtime_generation = 1
                     WHERE command_id = ?2",
                    rusqlite::params![
                        resource_id(0xE0).as_bytes().as_slice(),
                        close_cmd.as_bytes().as_slice(),
                    ],
                )
                .unwrap();
            }),
        ),
        (
            "tampered task scope on accepted event",
            Box::new(move |conn| {
                let other = task_id(0xE1);
                // Create a decoy task row so FK/checks stay quiet if any exist.
                let _ = other;
                conn.execute(
                    "UPDATE events SET task_id = ?1 WHERE event_type = 'operation.accepted'
                     AND sequence = (
                        SELECT committed_sequence FROM command_receipts WHERE command_id = ?2
                     )",
                    rusqlite::params![
                        other.as_bytes().as_slice(),
                        close_cmd.as_bytes().as_slice(),
                    ],
                )
                .unwrap();
            }),
        ),
        (
            "missing outbox row",
            Box::new(|conn| {
                conn.execute("DELETE FROM outbox", []).unwrap();
            }),
        ),
        (
            "extra outbox row",
            Box::new(|conn| {
                let (op_bytes, event_sequence, destination, policy, payload, available_at): (
                    Vec<u8>,
                    i64,
                    String,
                    String,
                    Vec<u8>,
                    i64,
                ) = conn
                    .query_row(
                        "SELECT operation_id, event_sequence, destination_class, replay_policy,
                                payload, available_at_ms FROM outbox LIMIT 1",
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
                    .unwrap();
                let extra_outbox = operation_id(0xE2);
                conn.execute(
                    "INSERT INTO outbox(
                        outbox_id, operation_id, effect_index, event_sequence, destination_class,
                        replay_policy, payload, state, available_at_ms, leased_until_ms,
                        dispatch_started_at_ms, attempts, last_error_class
                     ) VALUES (?1, ?2, 1, ?3, ?4, ?5, ?6, 'pending', ?7, NULL, NULL, 0, NULL)",
                    rusqlite::params![
                        extra_outbox.as_bytes().as_slice(),
                        op_bytes.as_slice(),
                        event_sequence,
                        destination,
                        policy,
                        payload,
                        available_at,
                    ],
                )
                .unwrap();
            }),
        ),
        (
            "invalid non-v7 outbox_id",
            Box::new(|conn| {
                // Version nibble cleared -> not UUIDv7.
                let mut bad = [0u8; 16];
                bad[6] = 0x40; // version 4
                bad[8] = 0x80; // RFC variant
                conn.execute("UPDATE outbox SET outbox_id = ?1", [bad.as_slice()])
                    .unwrap();
            }),
        ),
        (
            "tampered state",
            Box::new(|conn| {
                conn.execute("UPDATE outbox SET state = 'leased'", [])
                    .unwrap();
            }),
        ),
        (
            "tampered attempts without dispatch_started",
            Box::new(|conn| {
                conn.execute("UPDATE outbox SET attempts = 1", []).unwrap();
            }),
        ),
        (
            "tampered dispatch_started",
            Box::new(|conn| {
                conn.execute("UPDATE outbox SET dispatch_started_at_ms = 9", [])
                    .unwrap();
            }),
        ),
        (
            "tampered last_error",
            Box::new(|conn| {
                conn.execute("UPDATE outbox SET last_error_class = 'x'", [])
                    .unwrap();
            }),
        ),
        (
            "tampered available_at",
            Box::new(|conn| {
                conn.execute(
                    "UPDATE outbox SET available_at_ms = available_at_ms + 1",
                    [],
                )
                .unwrap();
            }),
        ),
        (
            "attempts with available after started",
            Box::new(|conn| {
                let accepted: i64 = conn
                    .query_row("SELECT available_at_ms FROM outbox LIMIT 1", [], |row| {
                        row.get(0)
                    })
                    .unwrap();
                conn.execute(
                    "UPDATE outbox
                     SET attempts = 2,
                         available_at_ms = ?1,
                         dispatch_started_at_ms = ?2",
                    rusqlite::params![accepted + 5, accepted + 1],
                )
                .unwrap();
            }),
        ),
    ];

    for (label, mutate) in cases {
        let case_dir = TempDir::new().expect("case tempdir");
        let case_path = temp_db_path(&case_dir);
        fs::copy(&path, &case_path).expect("copy db");
        let conn = open_raw(&case_path);
        mutate(&conn);
        drop(conn);

        let mut store = KernelStore::open(&case_path).expect("reopen");
        let err = store
            .execute(envelope.clone())
            .expect_err(&format!("{label} must fail closed"));
        assert_store_error_integrity(err);
    }
}

#[test]
fn command_side_effect_corrupt_release_generation_fence_fails_closed() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut store = KernelStore::open(&path).expect("open");
    let task = task_id(0x14);
    create_open_task(&mut store, task, command_id(0x15));
    drop(store);

    let resource = resource_id(0x16);
    seed_active_resource(&path, task, resource, 7);

    let mut store = KernelStore::open(&path).expect("reopen");
    let release_cmd = command_id(0x17);
    let envelope = command_envelope(
        release_cmd,
        Some(task),
        Some(2),
        Command::ReleaseResource {
            resource_id: resource,
        },
    );
    let first = store.execute(envelope.clone()).expect("release");
    let CommandReceipt::Accepted { .. } = first else {
        panic!("expected accepted release");
    };
    drop(store);

    let cases: Vec<(&str, Box<dyn Fn(&Connection)>)> = vec![
        (
            "tampered runtime_generation fence",
            Box::new(|conn| {
                conn.execute(
                    "UPDATE operations SET runtime_generation = 99 WHERE command_id = ?1",
                    [release_cmd.as_bytes().as_slice()],
                )
                .unwrap();
            }),
        ),
        (
            "tampered resource_id fence",
            Box::new(|conn| {
                conn.execute(
                    "UPDATE operations SET resource_id = ?1 WHERE command_id = ?2",
                    rusqlite::params![
                        resource_id(0x18).as_bytes().as_slice(),
                        release_cmd.as_bytes().as_slice(),
                    ],
                )
                .unwrap();
            }),
        ),
        (
            "tampered available_at on release outbox",
            Box::new(|conn| {
                conn.execute("UPDATE outbox SET available_at_ms = 1", [])
                    .unwrap();
            }),
        ),
    ];

    for (label, mutate) in cases {
        let case_dir = TempDir::new().expect("case tempdir");
        let case_path = temp_db_path(&case_dir);
        fs::copy(&path, &case_path).expect("copy db");
        let conn = open_raw(&case_path);
        mutate(&conn);
        drop(conn);

        let mut store = KernelStore::open(&case_path).expect("reopen");
        let err = store
            .execute(envelope.clone())
            .expect_err(&format!("{label} must fail closed"));
        assert_store_error_integrity(err);
    }
}

#[test]
fn command_side_effect_outbox_insert_trigger_rolls_back_acceptance() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut store = KernelStore::open(&path).expect("open");
    let task = task_id(0x11);
    create_open_task(&mut store, task, command_id(0x12));
    drop(store);

    let conn = open_raw(&path);
    let events_before = count_table(&conn, "events");
    let receipts_before = count_table(&conn, "command_receipts");
    let ops_before = count_table(&conn, "operations");
    let outbox_before = count_table(&conn, "outbox");
    let lifecycle_before: String = conn
        .query_row(
            "SELECT lifecycle FROM tasks WHERE task_id = ?1",
            [task.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    conn.execute_batch(
        "CREATE TRIGGER test_abort_outbox_insert
         BEFORE INSERT ON outbox
         BEGIN
           SELECT RAISE(ABORT, 'test outbox insert abort');
         END;",
    )
    .expect("install outbox abort trigger");
    drop(conn);

    let mut store = KernelStore::open(&path).expect("reopen");
    let close_cmd = command_id(0x13);
    let err = store
        .execute(command_envelope(
            close_cmd,
            Some(task),
            Some(1),
            Command::BeginCloseTask,
        ))
        .expect_err("outbox insert abort must fail transaction");
    assert!(
        matches!(
            err,
            StoreError::ConstraintViolation | StoreError::Sqlite(_) | StoreError::Projection(_)
        ),
        "unexpected error: {err:?}"
    );
    drop(store);

    let conn = open_raw(&path);
    assert_eq!(count_table(&conn, "events"), events_before);
    assert_eq!(count_table(&conn, "command_receipts"), receipts_before);
    assert_eq!(count_table(&conn, "operations"), ops_before);
    assert_eq!(count_table(&conn, "outbox"), outbox_before);
    let lifecycle_after: String = conn
        .query_row(
            "SELECT lifecycle FROM tasks WHERE task_id = ?1",
            [task.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(lifecycle_after, lifecycle_before);
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM command_receipts WHERE command_id = ?1",
            [close_cmd.as_bytes().as_slice()],
            |row| row.get::<_, i64>(0),
        )
        .unwrap(),
        0
    );
}

fn accepted_at_ms(conn: &Connection, operation_id: OperationId) -> i64 {
    conn.query_row(
        "SELECT accepted_at_ms FROM operations WHERE operation_id = ?1",
        [operation_id.as_bytes().as_slice()],
        |row| row.get(0),
    )
    .expect("accepted_at_ms")
}

fn operation_projection(
    conn: &Connection,
    operation_id: OperationId,
) -> (
    String,
    Option<i64>,
    Option<Vec<u8>>,
    Option<i64>,
    Option<i64>,
) {
    conn.query_row(
        "SELECT state, action_epoch, resource_id, runtime_generation, outcome_at_ms
         FROM operations WHERE operation_id = ?1",
        [operation_id.as_bytes().as_slice()],
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
    .expect("operation projection")
}

fn accept_begin_close(
    store: &mut KernelStore,
    task: TaskId,
    cmd: CommandId,
    expected_revision: u64,
) -> (OperationId, CommandReceipt) {
    let receipt = store
        .execute(command_envelope(
            cmd,
            Some(task),
            Some(expected_revision),
            Command::BeginCloseTask,
        ))
        .expect("begin close");
    let CommandReceipt::Accepted { operation_id, .. } = receipt.clone() else {
        panic!("expected accepted begin close, got {receipt:?}");
    };
    (operation_id, receipt)
}

fn accept_release_resource(
    store: &mut KernelStore,
    task: TaskId,
    cmd: CommandId,
    resource: ResourceId,
    expected_revision: u64,
) -> (OperationId, CommandReceipt) {
    let receipt = store
        .execute(command_envelope(
            cmd,
            Some(task),
            Some(expected_revision),
            Command::ReleaseResource {
                resource_id: resource,
            },
        ))
        .expect("release resource");
    let CommandReceipt::Accepted { operation_id, .. } = receipt.clone() else {
        panic!("expected accepted release, got {receipt:?}");
    };
    (operation_id, receipt)
}

fn settle_close_outcome(
    operation_id: OperationId,
    occurred_at_ms: i64,
    action_epoch: u64,
    result_id: EventId,
) -> OperationOutcome {
    OperationOutcome::new(
        operation_id,
        occurred_at_ms,
        Some(action_epoch),
        None,
        OutcomeSource::Dispatch,
        OperationOutcomeKind::Settled {
            result_event_ids: vec![result_id],
        },
    )
    .expect("close settle outcome")
}

fn settle_release_outcome(
    operation_id: OperationId,
    occurred_at_ms: i64,
    action_epoch: u64,
    resource: ResourceId,
    generation: u64,
    result_id: EventId,
) -> OperationOutcome {
    OperationOutcome::new(
        operation_id,
        occurred_at_ms,
        Some(action_epoch),
        Some(ResourceFence::new(resource, generation)),
        OutcomeSource::Dispatch,
        OperationOutcomeKind::Settled {
            result_event_ids: vec![result_id],
        },
    )
    .expect("release settle outcome")
}

#[test]
fn command_outcome_close_success_derives_task_archived_and_settles() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut store = KernelStore::open(&path).expect("open");
    let task = task_id(0xD1);
    create_open_task(&mut store, task, command_id(0xD2));
    let close_cmd = command_id(0xD3);
    let (operation_id, acceptance) = accept_begin_close(&mut store, task, close_cmd, 1);
    drop(store);

    let conn = open_raw(&path);
    let accepted_ms = accepted_at_ms(&conn, operation_id);
    let committed_before: i64 = conn
        .query_row(
            "SELECT committed_sequence FROM command_receipts WHERE command_id = ?1",
            [close_cmd.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    let events_before = count_table(&conn, "events");
    let receipt_before = receipt_blob(&conn, close_cmd);
    drop(conn);

    let result_id = event_id(0xE1);
    let mut store = KernelStore::open(&path).expect("reopen");
    let state = store
        .record_outcome(settle_close_outcome(
            operation_id,
            accepted_ms + 10,
            1,
            result_id,
        ))
        .expect("settle close");
    assert_eq!(
        state,
        OperationState::Settled {
            settled_at_ms: accepted_ms + 10,
            result_event_ids: vec![result_id],
        }
    );
    drop(store);

    let conn = open_raw(&path);
    assert_eq!(count_table(&conn, "events"), events_before + 2);
    let types = event_types(&conn);
    assert_eq!(types[types.len() - 2], "task.archived");
    assert_eq!(types[types.len() - 1], "operation.settled");

    let archived_id: Vec<u8> = conn
        .query_row(
            "SELECT event_id FROM events WHERE event_type = 'task.archived'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(archived_id.as_slice(), result_id.as_bytes().as_slice());

    let archived_revision: i64 = conn
        .query_row(
            "SELECT task_revision FROM events WHERE event_type = 'task.archived'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(archived_revision, 3);

    let settled_revision: Option<i64> = conn
        .query_row(
            "SELECT task_revision FROM events WHERE event_type = 'operation.settled'
             ORDER BY sequence DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(settled_revision.is_none());

    let settled_payload: Vec<u8> = conn
        .query_row(
            "SELECT payload FROM events WHERE event_type = 'operation.settled'
             ORDER BY sequence DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let settled: OperationSettledFact = rmp_serde::from_slice(&settled_payload).unwrap();
    assert_eq!(settled.command_id, close_cmd);
    assert_eq!(settled.operation_id, operation_id);
    assert_eq!(settled.result_event_ids, vec![result_id]);
    assert_eq!(settled.action_epoch, Some(1));
    assert_eq!(settled.source, OutcomeSource::Dispatch);

    let (lifecycle, revision): (String, i64) = conn
        .query_row(
            "SELECT lifecycle, revision FROM tasks WHERE task_id = ?1",
            [task.as_bytes().as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(lifecycle, "archived");
    assert_eq!(revision, 3);

    let (op_state, outcome_at, last_error, outbox_state) = {
        let (state, _, _, _, outcome_at) = operation_projection(&conn, operation_id);
        let (
            _id,
            _idx,
            _seq,
            _dest,
            _pol,
            _pay,
            outbox_state,
            _avail,
            leased,
            _disp,
            _att,
            last_error,
        ) = load_outbox_row(&conn, operation_id);
        assert!(leased.is_none());
        (state, outcome_at, last_error, outbox_state)
    };
    assert_eq!(op_state, "settled");
    assert_eq!(outcome_at, Some(accepted_ms + 10));
    assert_eq!(outbox_state, "settled");
    assert!(last_error.is_none());

    let committed_after: i64 = conn
        .query_row(
            "SELECT committed_sequence FROM command_receipts WHERE command_id = ?1",
            [close_cmd.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(committed_after, committed_before);
    assert_eq!(receipt_blob(&conn, close_cmd), receipt_before);
    let _ = acceptance;
}

#[test]
fn command_outcome_release_success_derives_generation_fenced_resource_released() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut store = KernelStore::open(&path).expect("open");
    let task = task_id(0xD4);
    create_open_task(&mut store, task, command_id(0xD5));
    drop(store);

    let resource = resource_id(0xD6);
    seed_active_resource(&path, task, resource, 9);

    let mut store = KernelStore::open(&path).expect("reopen");
    let release_cmd = command_id(0xD7);
    let (operation_id, _) = accept_release_resource(&mut store, task, release_cmd, resource, 2);
    drop(store);

    let conn = open_raw(&path);
    let accepted_ms = accepted_at_ms(&conn, operation_id);
    let events_before = count_table(&conn, "events");
    drop(conn);

    let result_id = event_id(0xE2);
    let mut store = KernelStore::open(&path).expect("reopen");
    let state = store
        .record_outcome(settle_release_outcome(
            operation_id,
            accepted_ms + 5,
            0,
            resource,
            9,
            result_id,
        ))
        .expect("settle release");
    assert_eq!(
        state,
        OperationState::Settled {
            settled_at_ms: accepted_ms + 5,
            result_event_ids: vec![result_id],
        }
    );
    drop(store);

    let conn = open_raw(&path);
    assert_eq!(count_table(&conn, "events"), events_before + 2);
    let types = event_types(&conn);
    assert_eq!(types[types.len() - 2], "resource.released");
    assert_eq!(types[types.len() - 1], "operation.settled");

    let released_payload: Vec<u8> = conn
        .query_row(
            "SELECT payload FROM events WHERE event_type = 'resource.released'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let released: ResourceReleasedPayload = rmp_serde::from_slice(&released_payload).unwrap();
    assert_eq!(released.resource_id, resource);
    assert_eq!(released.runtime_generation, 9);

    let (lifecycle, generation, task_rev): (String, i64, i64) = conn
        .query_row(
            "SELECT r.lifecycle, r.runtime_generation, t.revision
             FROM resources r JOIN tasks t ON t.task_id = r.task_id
             WHERE r.resource_id = ?1",
            [resource.as_bytes().as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(lifecycle, "released");
    assert_eq!(generation, 9);
    assert_eq!(task_rev, 4);

    let (op_state, _, _, _, _) = operation_projection(&conn, operation_id);
    assert_eq!(op_state, "settled");
    let (_, _, _, _, _, _, outbox_state, _, _, _, _, last_error) =
        load_outbox_row(&conn, operation_id);
    assert_eq!(outbox_state, "settled");
    assert!(last_error.is_none());
}

#[test]
fn command_outcome_failed_cancelled_uncertain_append_only_outcome_facts() {
    for (label, kind, expected_state, expected_outbox, expected_error, event_type) in [
        (
            "failed",
            OperationOutcomeKind::Failed {
                code: OperationErrorCode::SideEffectFailed,
            },
            "failed",
            "failed",
            Some("side_effect_failed"),
            "operation.failed",
        ),
        (
            "cancelled",
            OperationOutcomeKind::Cancelled {
                reason: CancellationReason::Superseded,
            },
            "cancelled",
            "cancelled",
            Some("superseded"),
            "operation.cancelled",
        ),
        (
            "uncertain",
            OperationOutcomeKind::Uncertain {
                code: OperationUncertaintyCode::AmbiguousDispatch,
            },
            "uncertain",
            "uncertain",
            Some("ambiguous_dispatch"),
            "operation.uncertain",
        ),
    ] {
        let dir = TempDir::new().expect("tempdir");
        let path = temp_db_path(&dir);
        let mut store = KernelStore::open(&path).expect("open");
        let task = task_id(0xD8);
        create_open_task(&mut store, task, command_id(0xD9));
        let close_cmd = command_id(0xDA);
        let (operation_id, _) = accept_begin_close(&mut store, task, close_cmd, 1);
        drop(store);

        let conn = open_raw(&path);
        let accepted_ms = accepted_at_ms(&conn, operation_id);
        let events_before = count_table(&conn, "events");
        drop(conn);

        let outcome = OperationOutcome::new(
            operation_id,
            accepted_ms + 1,
            Some(1),
            None,
            OutcomeSource::Dispatch,
            kind,
        )
        .expect(label);
        let mut store = KernelStore::open(&path).expect("reopen");
        store.record_outcome(outcome).expect(label);
        drop(store);

        let conn = open_raw(&path);
        assert_eq!(
            count_table(&conn, "events"),
            events_before + 1,
            "{label} must append only the outcome fact"
        );
        assert!(
            !event_types(&conn)
                .iter()
                .any(|t| t == "task.archived" || t == "resource.released"),
            "{label} must not archive/release"
        );
        assert!(event_types(&conn).iter().any(|t| t == event_type));

        let (lifecycle, _): (String, i64) = conn
            .query_row(
                "SELECT lifecycle, revision FROM tasks WHERE task_id = ?1",
                [task.as_bytes().as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(lifecycle, "closing", "{label} leaves task closing");

        let (op_state, _, _, _, _) = operation_projection(&conn, operation_id);
        assert_eq!(op_state, expected_state);
        let (_, _, _, _, _, _, outbox_state, _, leased, _, _, last_error) =
            load_outbox_row(&conn, operation_id);
        assert_eq!(outbox_state, expected_outbox);
        assert!(leased.is_none());
        assert_eq!(last_error.as_deref(), expected_error);
    }
}

#[test]
fn command_outcome_stale_fences_and_backward_timestamp_leave_store_unchanged() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut store = KernelStore::open(&path).expect("open");
    let task = task_id(0xDB);
    create_open_task(&mut store, task, command_id(0xDC));
    drop(store);
    let resource = resource_id(0xDD);
    seed_active_resource(&path, task, resource, 3);

    let mut store = KernelStore::open(&path).expect("reopen");
    let release_cmd = command_id(0xDE);
    let (operation_id, _) = accept_release_resource(&mut store, task, release_cmd, resource, 2);
    drop(store);

    let conn = open_raw(&path);
    let accepted_ms = accepted_at_ms(&conn, operation_id);
    let snapshot = (
        count_table(&conn, "events"),
        count_table(&conn, "outbox"),
        operation_projection(&conn, operation_id),
        {
            let (lifecycle, gen): (String, i64) = conn
                .query_row(
                    "SELECT lifecycle, runtime_generation FROM resources WHERE resource_id = ?1",
                    [resource.as_bytes().as_slice()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            (lifecycle, gen)
        },
    );
    drop(conn);

    let result_id = event_id(0xE3);
    let cases: Vec<(&str, OperationOutcome)> = vec![
        (
            "stale action epoch",
            settle_release_outcome(operation_id, accepted_ms + 1, 99, resource, 3, result_id),
        ),
        (
            "stale resource id",
            settle_release_outcome(
                operation_id,
                accepted_ms + 1,
                0,
                resource_id(0xEF),
                3,
                result_id,
            ),
        ),
        (
            "stale generation",
            settle_release_outcome(operation_id, accepted_ms + 1, 0, resource, 99, result_id),
        ),
        (
            "backward timestamp",
            settle_release_outcome(operation_id, accepted_ms - 1, 0, resource, 3, result_id),
        ),
    ];

    for (label, outcome) in cases {
        let mut store = KernelStore::open(&path).expect("reopen");
        let err = store
            .record_outcome(outcome)
            .expect_err(&format!("{label} must fail"));
        assert_eq!(err, StoreError::StaleFence, "{label}");
        drop(store);

        let conn = open_raw(&path);
        assert_eq!(count_table(&conn, "events"), snapshot.0, "{label}");
        assert_eq!(count_table(&conn, "outbox"), snapshot.1, "{label}");
        assert_eq!(
            operation_projection(&conn, operation_id),
            snapshot.2,
            "{label}"
        );
        let (lifecycle, gen): (String, i64) = conn
            .query_row(
                "SELECT lifecycle, runtime_generation FROM resources WHERE resource_id = ?1",
                [resource.as_bytes().as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!((lifecycle, gen), snapshot.3, "{label}");
    }

    // Ownership/lifecycle break: force resource out of releasing — durable
    // replay/projection mismatch is Corruption (not a caller StaleFence).
    {
        let conn = open_raw(&path);
        conn.execute(
            "UPDATE resources SET lifecycle = 'active' WHERE resource_id = ?1",
            [resource.as_bytes().as_slice()],
        )
        .unwrap();
    }
    let mut store = KernelStore::open(&path).expect("reopen");
    let err = store
        .record_outcome(settle_release_outcome(
            operation_id,
            accepted_ms + 1,
            0,
            resource,
            3,
            result_id,
        ))
        .expect_err("lifecycle mismatch");
    assert_eq!(err, StoreError::Corruption);
}

#[test]
fn command_outcome_missing_and_pure_operations_fail_typed() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut store = KernelStore::open(&path).expect("open");
    let task = task_id(0xE4);
    create_open_task(&mut store, task, command_id(0xE5));
    // Pure rename settles in acceptance — no outbox.
    let rename_cmd = command_id(0xE6);
    let rename_receipt = store
        .execute(command_envelope(
            rename_cmd,
            Some(task),
            Some(1),
            Command::RenameTask(RenameTaskIntent {
                title: "Pure".into(),
            }),
        ))
        .expect("rename");
    let CommandReceipt::Accepted {
        operation_id: pure_op,
        ..
    } = rename_receipt
    else {
        panic!("expected accepted rename");
    };

    let missing = OperationOutcome::new(
        operation_id(0xEE),
        1,
        None,
        None,
        OutcomeSource::Dispatch,
        OperationOutcomeKind::Failed {
            code: OperationErrorCode::SideEffectFailed,
        },
    )
    .unwrap();
    assert_eq!(
        store.record_outcome(missing).unwrap_err(),
        StoreError::MissingOperation
    );

    let pure = OperationOutcome::new(
        pure_op,
        2,
        None,
        None,
        OutcomeSource::Dispatch,
        OperationOutcomeKind::Failed {
            code: OperationErrorCode::SideEffectFailed,
        },
    )
    .unwrap();
    assert_eq!(
        store.record_outcome(pure).unwrap_err(),
        StoreError::ConflictingOutcome
    );
}

#[test]
fn command_outcome_exact_duplicate_is_idempotent_before_and_after_reopen() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut store = KernelStore::open(&path).expect("open");
    let task = task_id(0xF1);
    create_open_task(&mut store, task, command_id(0xF2));
    let close_cmd = command_id(0xF3);
    let (operation_id, _) = accept_begin_close(&mut store, task, close_cmd, 1);
    drop(store);

    let conn = open_raw(&path);
    let accepted_ms = accepted_at_ms(&conn, operation_id);
    drop(conn);

    let result_id = event_id(0xF4);
    let outcome = settle_close_outcome(operation_id, accepted_ms + 7, 1, result_id);
    let mut store = KernelStore::open(&path).expect("reopen");
    let first = store.record_outcome(outcome.clone()).expect("first settle");
    let events_after_first = {
        drop(store);
        let conn = open_raw(&path);
        count_table(&conn, "events")
    };

    let mut store = KernelStore::open(&path).expect("reopen");
    let dup = store
        .record_outcome(outcome.clone())
        .expect("exact duplicate");
    assert_eq!(dup, first);
    drop(store);
    let conn = open_raw(&path);
    assert_eq!(count_table(&conn, "events"), events_after_first);
    drop(conn);

    // Advance task via reopen after archive, then exact earlier settle still returns current.
    let mut store = KernelStore::open(&path).expect("reopen");
    store
        .execute(command_envelope(
            command_id(0xF5),
            Some(task),
            Some(3),
            Command::ReopenTask,
        ))
        .expect("reopen archived");
    let after_reopen = store
        .record_outcome(outcome.clone())
        .expect("exact duplicate after reopen");
    assert_eq!(
        after_reopen,
        OperationState::Settled {
            settled_at_ms: accepted_ms + 7,
            result_event_ids: vec![result_id],
        }
    );
    drop(store);
    let conn = open_raw(&path);
    assert_eq!(count_table(&conn, "events"), events_after_first + 3); // reopen: decision+accepted+settled
    drop(conn);

    let mut store = KernelStore::open(&path).expect("reopen");
    let differing_kind = OperationOutcome::new(
        operation_id,
        accepted_ms + 7,
        Some(1),
        None,
        OutcomeSource::Dispatch,
        OperationOutcomeKind::Failed {
            code: OperationErrorCode::SideEffectFailed,
        },
    )
    .unwrap();
    assert_eq!(
        store.record_outcome(differing_kind).unwrap_err(),
        StoreError::ConflictingOutcome
    );
    let differing_time = settle_close_outcome(operation_id, accepted_ms + 8, 1, result_id);
    assert_eq!(
        store.record_outcome(differing_time).unwrap_err(),
        StoreError::ConflictingOutcome
    );
    let differing_result = settle_close_outcome(operation_id, accepted_ms + 7, 1, event_id(0xF6));
    assert_eq!(
        store.record_outcome(differing_result).unwrap_err(),
        StoreError::ConflictingOutcome
    );
    let differing_source = OperationOutcome::new(
        operation_id,
        accepted_ms + 7,
        Some(1),
        None,
        OutcomeSource::verified_reconciliation(0, "provider:x").unwrap(),
        OperationOutcomeKind::Settled {
            result_event_ids: vec![result_id],
        },
    )
    .unwrap();
    assert_eq!(
        store.record_outcome(differing_source).unwrap_err(),
        StoreError::ConflictingOutcome
    );
}

#[test]
fn command_outcome_reconciliation_rules_and_uncertain_resolution() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut store = KernelStore::open(&path).expect("open");
    let task = task_id(0xA1);
    create_open_task(&mut store, task, command_id(0xA2));
    let close_cmd = command_id(0xA3);
    let (operation_id, _) = accept_begin_close(&mut store, task, close_cmd, 1);
    drop(store);

    let conn = open_raw(&path);
    let accepted_ms = accepted_at_ms(&conn, operation_id);
    drop(conn);

    let result_id = event_id(0xA4);
    let direct_reconcile = OperationOutcome::new(
        operation_id,
        accepted_ms + 1,
        Some(1),
        None,
        OutcomeSource::verified_reconciliation(0, "provider:early").unwrap(),
        OperationOutcomeKind::Settled {
            result_event_ids: vec![result_id],
        },
    )
    .unwrap();
    let mut store = KernelStore::open(&path).expect("reopen");
    assert_eq!(
        store.record_outcome(direct_reconcile).unwrap_err(),
        StoreError::ConflictingOutcome
    );

    let uncertain = OperationOutcome::new(
        operation_id,
        accepted_ms + 2,
        Some(1),
        None,
        OutcomeSource::Dispatch,
        OperationOutcomeKind::Uncertain {
            code: OperationUncertaintyCode::AmbiguousDispatch,
        },
    )
    .unwrap();
    store.record_outcome(uncertain.clone()).expect("uncertain");

    let dispatch_from_uncertain = settle_close_outcome(operation_id, accepted_ms + 3, 1, result_id);
    assert_eq!(
        store.record_outcome(dispatch_from_uncertain).unwrap_err(),
        StoreError::ConflictingOutcome
    );

    let bad_index = OperationOutcome::new(
        operation_id,
        accepted_ms + 4,
        Some(1),
        None,
        OutcomeSource::verified_reconciliation(1, "provider:proof").unwrap(),
        OperationOutcomeKind::Settled {
            result_event_ids: vec![result_id],
        },
    )
    .unwrap();
    assert_eq!(
        store.record_outcome(bad_index).unwrap_err(),
        StoreError::ConflictingOutcome
    );

    // Exact earlier uncertain retry after resolution path setup: first resolve to settled.
    let reconcile_settle = OperationOutcome::new(
        operation_id,
        accepted_ms + 5,
        Some(1),
        None,
        OutcomeSource::verified_reconciliation(0, "provider:proof").unwrap(),
        OperationOutcomeKind::Settled {
            result_event_ids: vec![result_id],
        },
    )
    .unwrap();
    let settled = store
        .record_outcome(reconcile_settle.clone())
        .expect("uncertain -> settled");
    assert_eq!(
        settled,
        OperationState::Settled {
            settled_at_ms: accepted_ms + 5,
            result_event_ids: vec![result_id],
        }
    );

    let events_after = {
        drop(store);
        let conn = open_raw(&path);
        let settled_payload: Vec<u8> = conn
            .query_row(
                "SELECT payload FROM events WHERE event_type = 'operation.settled'
                 ORDER BY sequence DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let fact: OperationSettledFact = rmp_serde::from_slice(&settled_payload).unwrap();
        assert!(matches!(
            fact.source,
            OutcomeSource::VerifiedReconciliation {
                effect_index: 0,
                ..
            }
        ));
        assert_eq!(
            match &fact.source {
                OutcomeSource::VerifiedReconciliation {
                    external_identity, ..
                } => external_identity.as_str(),
                _ => panic!("expected verified source"),
            },
            "provider:proof"
        );
        let (_, _, _, _, _, _, outbox_state, _, _, _, _, last_error) =
            load_outbox_row(&conn, operation_id);
        assert_eq!(outbox_state, "settled");
        assert!(last_error.is_none());
        count_table(&conn, "events")
    };

    let mut store = KernelStore::open(&path).expect("reopen");
    let uncertain_retry = store
        .record_outcome(uncertain)
        .expect("exact earlier uncertain after resolution");
    assert_eq!(uncertain_retry, settled);
    drop(store);
    let conn = open_raw(&path);
    assert_eq!(count_table(&conn, "events"), events_after);
    drop(conn);

    // Separate close op for Uncertain -> Failed.
    let mut store = KernelStore::open(&path).expect("reopen");
    store
        .execute(command_envelope(
            command_id(0xA5),
            Some(task),
            Some(3),
            Command::ReopenTask,
        ))
        .expect("reopen");
    let (op2, _) = accept_begin_close(&mut store, task, command_id(0xA6), 4);
    drop(store);
    let conn = open_raw(&path);
    let accepted2 = accepted_at_ms(&conn, op2);
    drop(conn);
    let mut store = KernelStore::open(&path).expect("reopen");
    store
        .record_outcome(
            OperationOutcome::new(
                op2,
                accepted2 + 1,
                Some(2),
                None,
                OutcomeSource::Dispatch,
                OperationOutcomeKind::Uncertain {
                    code: OperationUncertaintyCode::AmbiguousDispatch,
                },
            )
            .unwrap(),
        )
        .expect("uncertain2");
    let failed = store
        .record_outcome(
            OperationOutcome::new(
                op2,
                accepted2 + 2,
                Some(2),
                None,
                OutcomeSource::verified_reconciliation(0, "provider:fail").unwrap(),
                OperationOutcomeKind::Failed {
                    code: OperationErrorCode::SideEffectFailed,
                },
            )
            .unwrap(),
        )
        .expect("uncertain -> failed");
    assert_eq!(
        failed,
        OperationState::Failed {
            settled_at_ms: accepted2 + 2,
            code: OperationErrorCode::SideEffectFailed,
        }
    );
    drop(store);
    let conn = open_raw(&path);
    let (_, _, _, _, _, _, outbox_state, _, _, _, _, last_error) = load_outbox_row(&conn, op2);
    assert_eq!(outbox_state, "failed");
    assert_eq!(last_error.as_deref(), Some("side_effect_failed"));
    let failed_payload: Vec<u8> = conn
        .query_row(
            "SELECT payload FROM events WHERE event_type = 'operation.failed'
             ORDER BY sequence DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let failed_fact: OperationFailedFact = rmp_serde::from_slice(&failed_payload).unwrap();
    assert!(matches!(
        failed_fact.source,
        OutcomeSource::VerifiedReconciliation { .. }
    ));
}

#[test]
fn command_outcome_duplicate_command_after_every_outcome_returns_original_receipt() {
    for (label, make_outcome) in [
        (
            "settled",
            Box::new(|op, ms| settle_close_outcome(op, ms + 1i64, 1, event_id(0xB1)))
                as Box<dyn Fn(OperationId, i64) -> OperationOutcome>,
        ),
        (
            "failed",
            Box::new(|op, ms| {
                OperationOutcome::new(
                    op,
                    ms + 1i64,
                    Some(1),
                    None,
                    OutcomeSource::Dispatch,
                    OperationOutcomeKind::Failed {
                        code: OperationErrorCode::SideEffectFailed,
                    },
                )
                .unwrap()
            }),
        ),
        (
            "cancelled",
            Box::new(|op, ms| {
                OperationOutcome::new(
                    op,
                    ms + 1i64,
                    Some(1),
                    None,
                    OutcomeSource::Dispatch,
                    OperationOutcomeKind::Cancelled {
                        reason: CancellationReason::Superseded,
                    },
                )
                .unwrap()
            }),
        ),
        (
            "uncertain",
            Box::new(|op, ms| {
                OperationOutcome::new(
                    op,
                    ms + 1i64,
                    Some(1),
                    None,
                    OutcomeSource::Dispatch,
                    OperationOutcomeKind::Uncertain {
                        code: OperationUncertaintyCode::AmbiguousDispatch,
                    },
                )
                .unwrap()
            }),
        ),
    ] {
        let dir = TempDir::new().expect("tempdir");
        let path = temp_db_path(&dir);
        let mut store = KernelStore::open(&path).expect("open");
        let task = task_id(0xB0);
        create_open_task(&mut store, task, command_id(0xB3));
        let close_cmd = command_id(0xB4);
        let envelope = command_envelope(close_cmd, Some(task), Some(1), Command::BeginCloseTask);
        let first = store.execute(envelope.clone()).expect("accept");
        let CommandReceipt::Accepted { operation_id, .. } = first.clone() else {
            panic!("accepted");
        };
        drop(store);
        let conn = open_raw(&path);
        let accepted_ms = accepted_at_ms(&conn, operation_id);
        let original_blob = receipt_blob(&conn, close_cmd);
        drop(conn);

        let mut store = KernelStore::open(&path).expect("reopen");
        store
            .record_outcome(make_outcome(operation_id, accepted_ms))
            .expect(label);
        let retry = store.execute(envelope).expect("dup command");
        assert_eq!(retry, first, "{label}");
        drop(store);
        let conn = open_raw(&path);
        assert_eq!(receipt_blob(&conn, close_cmd), original_blob, "{label}");
    }

    // Tamper cases fail closed.
    let tampers: &[(&str, fn(&Connection, OperationId))] = &[
        ("tampered terminal fact", |conn, _op| {
            conn.execute(
                "UPDATE events SET payload = X'00' WHERE event_type = 'operation.settled'
                 AND sequence = (SELECT MAX(sequence) FROM events)",
                [],
            )
            .unwrap();
        }),
        ("tampered result fact", |conn, _op| {
            conn.execute(
                "UPDATE events SET payload = X'00' WHERE event_type = 'task.archived'",
                [],
            )
            .unwrap();
        }),
        ("tampered operation projection", |conn, op| {
            conn.execute(
                "UPDATE operations SET state = 'accepted' WHERE operation_id = ?1",
                [op.as_bytes().as_slice()],
            )
            .unwrap();
        }),
        ("tampered outbox terminal state", |conn, op| {
            conn.execute(
                "UPDATE outbox SET state = 'pending' WHERE operation_id = ?1",
                [op.as_bytes().as_slice()],
            )
            .unwrap();
        }),
        ("tampered outbox error code", |conn, op| {
            conn.execute(
                "UPDATE outbox SET last_error_class = 'side_effect_failed'
                 WHERE operation_id = ?1",
                [op.as_bytes().as_slice()],
            )
            .unwrap();
        }),
    ];

    for (label, tamper) in tampers {
        let dir = TempDir::new().expect("tempdir");
        let path = temp_db_path(&dir);
        let mut store = KernelStore::open(&path).expect("open");
        let task = task_id(0xB5);
        create_open_task(&mut store, task, command_id(0xB6));
        let close_cmd = command_id(0xB7);
        let envelope = command_envelope(close_cmd, Some(task), Some(1), Command::BeginCloseTask);
        let first = store.execute(envelope.clone()).expect("accept");
        let CommandReceipt::Accepted { operation_id, .. } = first else {
            panic!("accepted");
        };
        drop(store);
        let conn = open_raw(&path);
        let accepted_ms = accepted_at_ms(&conn, operation_id);
        drop(conn);
        let mut store = KernelStore::open(&path).expect("reopen");
        store
            .record_outcome(settle_close_outcome(
                operation_id,
                accepted_ms + 1,
                1,
                event_id(0xB8),
            ))
            .expect("settle");
        drop(store);
        {
            let conn = open_raw(&path);
            tamper(&conn, operation_id);
        }
        let mut store = KernelStore::open(&path).expect("reopen");
        let err = store
            .execute(envelope)
            .expect_err(&format!("{label} must fail closed"));
        assert!(
            matches!(
                err,
                StoreError::Corruption
                    | StoreError::CodecMismatch { .. }
                    | StoreError::EventDecode(_)
                    | StoreError::Projection(_)
            ),
            "{label}: {err:?}"
        );
    }
}

#[test]
fn command_outcome_result_id_errors_and_trigger_roll_back_atomically() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut store = KernelStore::open(&path).expect("open");
    let task = task_id(0xC0);
    create_open_task(&mut store, task, command_id(0xC1));
    let close_cmd = command_id(0xC2);
    let (operation_id, _) = accept_begin_close(&mut store, task, close_cmd, 1);
    drop(store);

    let conn = open_raw(&path);
    let accepted_ms = accepted_at_ms(&conn, operation_id);
    let snap = (
        count_table(&conn, "events"),
        operation_projection(&conn, operation_id),
        {
            let (_, _, _, _, _, _, state, _, _, _, _, err) = load_outbox_row(&conn, operation_id);
            (state, err)
        },
        {
            let (lifecycle, revision): (String, i64) = conn
                .query_row(
                    "SELECT lifecycle, revision FROM tasks WHERE task_id = ?1",
                    [task.as_bytes().as_slice()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            (lifecycle, revision)
        },
    );
    // Reuse an already-persisted event id as "used".
    let used_id = event_id(0xC3);
    // Grab an existing event id from the log.
    let existing_id: Vec<u8> = conn
        .query_row(
            "SELECT event_id FROM events ORDER BY sequence ASC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let used = EventId::from_bytes(existing_id.try_into().unwrap()).unwrap();
    drop(conn);

    let wrong_count = OperationOutcome::new(
        operation_id,
        accepted_ms + 1,
        Some(1),
        None,
        OutcomeSource::Dispatch,
        OperationOutcomeKind::Settled {
            result_event_ids: vec![event_id(0xC4), event_id(0xC5)],
        },
    )
    .unwrap();
    let empty = OperationOutcome::new(
        operation_id,
        accepted_ms + 1,
        Some(1),
        None,
        OutcomeSource::Dispatch,
        OperationOutcomeKind::Settled {
            result_event_ids: vec![],
        },
    )
    .unwrap();
    let dup_ids = OperationOutcome::new(
        operation_id,
        accepted_ms + 1,
        Some(1),
        None,
        OutcomeSource::Dispatch,
        OperationOutcomeKind::Settled {
            result_event_ids: vec![event_id(0xC6), event_id(0xC6)],
        },
    )
    .unwrap();
    let used_ids = settle_close_outcome(operation_id, accepted_ms + 1, 1, used);

    for (label, outcome) in [
        ("wrong count", wrong_count),
        ("empty", empty),
        ("duplicate ids", dup_ids),
        ("used id", used_ids),
    ] {
        let mut store = KernelStore::open(&path).expect("reopen");
        let err = store
            .record_outcome(outcome)
            .expect_err(&format!("{label} must fail"));
        assert!(
            matches!(
                err,
                StoreError::ConflictingOutcome | StoreError::ConstraintViolation
            ),
            "{label}: {err:?}"
        );
        drop(store);
        let conn = open_raw(&path);
        assert_eq!(count_table(&conn, "events"), snap.0, "{label}");
        assert_eq!(operation_projection(&conn, operation_id), snap.1, "{label}");
        let (_, _, _, _, _, _, state, _, _, _, _, err_class) = load_outbox_row(&conn, operation_id);
        assert_eq!((state, err_class), snap.2, "{label}");
        let (lifecycle, revision): (String, i64) = conn
            .query_row(
                "SELECT lifecycle, revision FROM tasks WHERE task_id = ?1",
                [task.as_bytes().as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!((lifecycle, revision), snap.3, "{label}");
        let _ = used_id;
    }

    // Trigger abort during outcome append rolls everything back.
    {
        let conn = open_raw(&path);
        conn.execute_batch(
            "CREATE TRIGGER test_abort_outcome_insert
             BEFORE INSERT ON events
             WHEN NEW.event_type = 'operation.settled'
             BEGIN
               SELECT RAISE(ABORT, 'test outcome abort');
             END;",
        )
        .expect("trigger");
    }
    let mut store = KernelStore::open(&path).expect("reopen");
    let err = store
        .record_outcome(settle_close_outcome(
            operation_id,
            accepted_ms + 1,
            1,
            event_id(0xC7),
        ))
        .expect_err("trigger abort");
    assert!(
        matches!(
            err,
            StoreError::ConstraintViolation | StoreError::Sqlite(_) | StoreError::Projection(_)
        ),
        "{err:?}"
    );
    drop(store);
    let conn = open_raw(&path);
    assert_eq!(count_table(&conn, "events"), snap.0);
    assert_eq!(operation_projection(&conn, operation_id), snap.1);
    let (_, _, _, _, _, _, state, _, _, _, _, err_class) = load_outbox_row(&conn, operation_id);
    assert_eq!((state, err_class), snap.2);
    let (lifecycle, revision): (String, i64) = conn
        .query_row(
            "SELECT lifecycle, revision FROM tasks WHERE task_id = ?1",
            [task.as_bytes().as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!((lifecycle, revision), snap.3);
    // No leaked archived fact from the aborted settle.
    assert!(!event_types(&conn).iter().any(|t| t == "task.archived"));
}

#[test]
fn command_outcome_corrupt_lineage_before_new_transition_writes_nothing() {
    let tampers: &[(&str, fn(&Connection, OperationId, CommandId))] = &[
        ("tampered decision event id ownership", |conn, _op, _cmd| {
            conn.execute(
                "UPDATE events SET task_id = NULL WHERE event_type = 'task.close_begun'",
                [],
            )
            .unwrap();
        }),
        ("tampered accepted fence epoch", |conn, _op, _cmd| {
            // Corrupt accepted fact payload action_epoch while leaving projection.
            let payload: Vec<u8> = conn
                .query_row(
                    "SELECT payload FROM events WHERE event_type = 'operation.accepted'
                     ORDER BY sequence DESC LIMIT 1",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            let mut accepted: OperationAcceptedFact = rmp_serde::from_slice(&payload).unwrap();
            accepted.action_epoch = Some(99);
            conn.execute(
                "UPDATE events SET payload = ?1 WHERE event_type = 'operation.accepted'
                 AND sequence = (SELECT MAX(sequence) FROM events WHERE event_type = 'operation.accepted')",
                [rmp_serde::to_vec(&accepted).unwrap()],
            )
            .unwrap();
        }),
        ("tampered outbox effect index", |conn, op, _cmd| {
            conn.execute(
                "UPDATE outbox SET effect_index = 1 WHERE operation_id = ?1",
                [op.as_bytes().as_slice()],
            )
            .unwrap();
        }),
        ("tampered outbox destination", |conn, op, _cmd| {
            conn.execute(
                "UPDATE outbox SET destination_class = 'resource_release' WHERE operation_id = ?1",
                [op.as_bytes().as_slice()],
            )
            .unwrap();
        }),
        ("tampered receipt event_ids empty", |conn, _op, cmd| {
            let payload: Vec<u8> = conn
                .query_row(
                    "SELECT receipt FROM command_receipts WHERE command_id = ?1",
                    [cmd.as_bytes().as_slice()],
                    |row| row.get(0),
                )
                .unwrap();
            // Corrupt receipt blob so correlation fails closed.
            let mut bad = payload;
            if let Some(last) = bad.last_mut() {
                *last ^= 0xff;
            }
            conn.execute(
                "UPDATE command_receipts SET receipt = ?1 WHERE command_id = ?2",
                rusqlite::params![bad, cmd.as_bytes().as_slice()],
            )
            .unwrap();
        }),
    ];

    for (label, tamper) in tampers {
        let dir = TempDir::new().expect("tempdir");
        let path = temp_db_path(&dir);
        let mut store = KernelStore::open(&path).expect("open");
        let task = task_id(0x31);
        create_open_task(&mut store, task, command_id(0x32));
        let close_cmd = command_id(0x33);
        let (operation_id, _) = accept_begin_close(&mut store, task, close_cmd, 1);
        drop(store);

        let conn = open_raw(&path);
        let accepted_ms = accepted_at_ms(&conn, operation_id);
        let events_before = count_table(&conn, "events");
        let op_before = operation_projection(&conn, operation_id);
        let outbox_before = {
            let (_, _, _, _, _, _, state, _, _, _, _, err) = load_outbox_row(&conn, operation_id);
            (state, err)
        };
        tamper(&conn, operation_id, close_cmd);
        drop(conn);

        let mut store = KernelStore::open(&path).expect("reopen");
        let err = store
            .record_outcome(settle_close_outcome(
                operation_id,
                accepted_ms + 1,
                1,
                event_id(0x34),
            ))
            .expect_err(&format!("{label} must fail closed"));
        assert!(
            matches!(
                err,
                StoreError::Corruption
                    | StoreError::CodecMismatch { .. }
                    | StoreError::EventDecode(_)
                    | StoreError::Projection(_)
                    | StoreError::ConstraintViolation
                    | StoreError::MissingOperation
            ),
            "{label}: {err:?}"
        );
        drop(store);

        let conn = open_raw(&path);
        assert_eq!(count_table(&conn, "events"), events_before, "{label}");
        if let Ok(op_after) = conn.query_row(
            "SELECT state FROM operations WHERE operation_id = ?1",
            [operation_id.as_bytes().as_slice()],
            |row| row.get::<_, String>(0),
        ) {
            assert_eq!(op_after, op_before.0, "{label}");
            let (_, _, _, _, _, _, state, _, _, _, _, err_class) =
                load_outbox_row(&conn, operation_id);
            assert_eq!((state, err_class), outbox_before, "{label}");
        }
    }
}

#[test]
fn command_outcome_exact_retry_rejects_tampered_durable_state() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut store = KernelStore::open(&path).expect("open");
    let task = task_id(0x35);
    create_open_task(&mut store, task, command_id(0x36));
    let close_cmd = command_id(0x37);
    let (operation_id, _) = accept_begin_close(&mut store, task, close_cmd, 1);
    drop(store);

    let conn = open_raw(&path);
    let accepted_ms = accepted_at_ms(&conn, operation_id);
    drop(conn);

    let result_id = event_id(0x38);
    let outcome = settle_close_outcome(operation_id, accepted_ms + 3, 1, result_id);
    let mut store = KernelStore::open(&path).expect("reopen");
    let first = store.record_outcome(outcome.clone()).expect("settle");
    drop(store);

    let tampers: &[(&str, fn(&Connection, OperationId))] = &[
        ("tampered operation projection state", |conn, op| {
            conn.execute(
                "UPDATE operations SET state = 'failed', outcome_code = 'side_effect_failed'
                 WHERE operation_id = ?1",
                [op.as_bytes().as_slice()],
            )
            .unwrap();
        }),
        ("tampered outbox terminal state", |conn, op| {
            conn.execute(
                "UPDATE outbox SET state = 'failed', last_error_class = 'side_effect_failed'
                 WHERE operation_id = ?1",
                [op.as_bytes().as_slice()],
            )
            .unwrap();
        }),
        ("tampered result fact payload", |conn, _op| {
            conn.execute(
                "UPDATE events SET payload = X'00' WHERE event_type = 'task.archived'",
                [],
            )
            .unwrap();
        }),
        ("tampered settled fact result ids", |conn, _op| {
            let payload: Vec<u8> = conn
                .query_row(
                    "SELECT payload FROM events WHERE event_type = 'operation.settled'
                     ORDER BY sequence DESC LIMIT 1",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            let mut settled: OperationSettledFact = rmp_serde::from_slice(&payload).unwrap();
            settled.result_event_ids = vec![event_id(0x99)];
            conn.execute(
                "UPDATE events SET payload = ?1 WHERE event_type = 'operation.settled'
                 AND sequence = (SELECT MAX(sequence) FROM events WHERE event_type = 'operation.settled')",
                [rmp_serde::to_vec(&settled).unwrap()],
            )
            .unwrap();
        }),
    ];

    for (label, tamper) in tampers {
        let dir = TempDir::new().expect("tempdir");
        let path = temp_db_path(&dir);
        let mut store = KernelStore::open(&path).expect("open");
        let task = task_id(0x35);
        create_open_task(&mut store, task, command_id(0x36));
        let close_cmd = command_id(0x37);
        let (operation_id, _) = accept_begin_close(&mut store, task, close_cmd, 1);
        drop(store);
        let conn = open_raw(&path);
        let accepted_ms = accepted_at_ms(&conn, operation_id);
        drop(conn);
        let outcome = settle_close_outcome(operation_id, accepted_ms + 3, 1, result_id);
        let mut store = KernelStore::open(&path).expect("reopen");
        store.record_outcome(outcome.clone()).expect("settle");
        drop(store);
        {
            let conn = open_raw(&path);
            tamper(&conn, operation_id);
        }
        let mut store = KernelStore::open(&path).expect("reopen");
        let err = store
            .record_outcome(outcome)
            .expect_err(&format!("{label} exact retry must fail"));
        assert!(
            matches!(
                err,
                StoreError::Corruption
                    | StoreError::CodecMismatch { .. }
                    | StoreError::EventDecode(_)
                    | StoreError::Projection(_)
            ),
            "{label}: {err:?}"
        );
        let _ = first;
    }
}

#[test]
fn command_outcome_interleaved_task_events_do_not_break_settle_or_retries() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut store = KernelStore::open(&path).expect("open");
    let task = task_id(0x41);
    create_open_task(&mut store, task, command_id(0x42));
    drop(store);

    let resource = resource_id(0x43);
    seed_active_resource(&path, task, resource, 5);

    let mut store = KernelStore::open(&path).expect("reopen");
    let release_cmd = command_id(0x44);
    let envelope = command_envelope(
        release_cmd,
        Some(task),
        Some(2),
        Command::ReleaseResource {
            resource_id: resource,
        },
    );
    let first = store.execute(envelope.clone()).expect("accept release");
    let CommandReceipt::Accepted { operation_id, .. } = first.clone() else {
        panic!("accepted");
    };

    // Legitimate unrelated mutation while release is pending.
    store
        .execute(command_envelope(
            command_id(0x45),
            Some(task),
            Some(3),
            Command::RenameTask(RenameTaskIntent {
                title: "Interleaved rename".into(),
            }),
        ))
        .expect("rename while pending");
    drop(store);

    let conn = open_raw(&path);
    let accepted_ms = accepted_at_ms(&conn, operation_id);
    let events_mid = count_table(&conn, "events");
    assert!(events_mid > 5, "rename must append while release pending");
    drop(conn);

    let result_id = event_id(0x46);
    let outcome = settle_release_outcome(operation_id, accepted_ms + 2, 0, resource, 5, result_id);
    let mut store = KernelStore::open(&path).expect("reopen");
    let settled = store
        .record_outcome(outcome.clone())
        .expect("settle release");
    assert_eq!(
        settled,
        OperationState::Settled {
            settled_at_ms: accepted_ms + 2,
            result_event_ids: vec![result_id],
        }
    );

    // Exact duplicate outcome after interleaving remains valid.
    let dup = store.record_outcome(outcome.clone()).expect("dup outcome");
    assert_eq!(dup, settled);

    // Duplicate command remains valid.
    let retry = store.execute(envelope).expect("dup command");
    assert_eq!(retry, first);

    // Later unrelated events after settlement must not break retries.
    let lifecycle: String = {
        drop(store);
        let conn = open_raw(&path);
        conn.query_row(
            "SELECT lifecycle FROM resources WHERE resource_id = ?1",
            [resource.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap()
    };
    assert_eq!(lifecycle, "released");

    let mut store = KernelStore::open(&path).expect("reopen");
    // Task is still Open after resource release; another rename is fine.
    let rev: i64 = {
        let conn = open_raw(&path);
        conn.query_row(
            "SELECT revision FROM tasks WHERE task_id = ?1",
            [task.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap()
    };
    store
        .execute(command_envelope(
            command_id(0x47),
            Some(task),
            Some(rev as u64),
            Command::RenameTask(RenameTaskIntent {
                title: "After settle".into(),
            }),
        ))
        .expect("rename after settle");
    let after = store
        .record_outcome(outcome)
        .expect("dup after later events");
    assert_eq!(after, settled);
    let retry2 = store
        .execute(command_envelope(
            release_cmd,
            Some(task),
            Some(1),
            Command::ReleaseResource {
                resource_id: resource,
            },
        ))
        .expect("dup command after later events");
    assert_eq!(retry2, first);
}

#[test]
fn command_outcome_reconciliation_wire_tampers_fail_closed() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut store = KernelStore::open(&path).expect("open");
    let task = task_id(0x51);
    create_open_task(&mut store, task, command_id(0x52));
    let close_cmd = command_id(0x53);
    let (operation_id, _) = accept_begin_close(&mut store, task, close_cmd, 1);
    drop(store);

    let conn = open_raw(&path);
    let accepted_ms = accepted_at_ms(&conn, operation_id);
    drop(conn);

    let mut store = KernelStore::open(&path).expect("reopen");
    store
        .record_outcome(
            OperationOutcome::new(
                operation_id,
                accepted_ms + 1,
                Some(1),
                None,
                OutcomeSource::Dispatch,
                OperationOutcomeKind::Uncertain {
                    code: OperationUncertaintyCode::AmbiguousDispatch,
                },
            )
            .unwrap(),
        )
        .expect("uncertain");
    let result_id = event_id(0x54);
    store
        .record_outcome(
            OperationOutcome::new(
                operation_id,
                accepted_ms + 2,
                Some(1),
                None,
                OutcomeSource::verified_reconciliation(0, "provider:wire").unwrap(),
                OperationOutcomeKind::Settled {
                    result_event_ids: vec![result_id],
                },
            )
            .unwrap(),
        )
        .expect("reconcile settle");
    drop(store);

    // Wrong terminal command_id (valid wire).
    {
        let conn = open_raw(&path);
        let payload: Vec<u8> = conn
            .query_row(
                "SELECT payload FROM events WHERE event_type = 'operation.settled'
                 ORDER BY sequence DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let mut settled: OperationSettledFact = rmp_serde::from_slice(&payload).unwrap();
        settled.command_id = command_id(0x5E);
        conn.execute(
            "UPDATE events SET payload = ?1 WHERE event_type = 'operation.settled'
             AND sequence = (SELECT MAX(sequence) FROM events WHERE event_type = 'operation.settled')",
            [rmp_serde::to_vec(&settled).unwrap()],
        )
        .unwrap();
    }
    let mut store = KernelStore::open(&path).expect("reopen");
    let err = store
        .execute(command_envelope(
            close_cmd,
            Some(task),
            Some(1),
            Command::BeginCloseTask,
        ))
        .expect_err("wrong terminal command_id");
    assert!(
        matches!(
            err,
            StoreError::Corruption | StoreError::CodecMismatch { .. }
        ),
        "{err:?}"
    );
    drop(store);

    // Rebuild clean DB for remaining tampers.
    for (label, tamper) in [
        (
            "uncertain prefix wrong fence",
            Box::new(|conn: &Connection| {
                let payload: Vec<u8> = conn
                    .query_row(
                        "SELECT payload FROM events WHERE event_type = 'operation.uncertain'",
                        [],
                        |row| row.get(0),
                    )
                    .unwrap();
                let mut uncertain: OperationUncertainFact =
                    rmp_serde::from_slice(&payload).unwrap();
                uncertain.action_epoch = Some(99);
                conn.execute(
                    "UPDATE events SET payload = ?1 WHERE event_type = 'operation.uncertain'",
                    [rmp_serde::to_vec(&uncertain).unwrap()],
                )
                .unwrap();
            }) as Box<dyn Fn(&Connection)>,
        ),
        (
            "uncertain prefix wrong observed time",
            Box::new(|conn: &Connection| {
                let payload: Vec<u8> = conn
                    .query_row(
                        "SELECT payload FROM events WHERE event_type = 'operation.uncertain'",
                        [],
                        |row| row.get(0),
                    )
                    .unwrap();
                let mut uncertain: OperationUncertainFact =
                    rmp_serde::from_slice(&payload).unwrap();
                uncertain.observed_at_ms = 1;
                conn.execute(
                    "UPDATE events SET payload = ?1 WHERE event_type = 'operation.uncertain'",
                    [rmp_serde::to_vec(&uncertain).unwrap()],
                )
                .unwrap();
            }),
        ),
        (
            "reconciliation effect_index mismatch",
            Box::new(|conn: &Connection| {
                let payload: Vec<u8> = conn
                    .query_row(
                        "SELECT payload FROM events WHERE event_type = 'operation.settled'
                         ORDER BY sequence DESC LIMIT 1",
                        [],
                        |row| row.get(0),
                    )
                    .unwrap();
                let mut settled: OperationSettledFact = rmp_serde::from_slice(&payload).unwrap();
                settled.source =
                    OutcomeSource::verified_reconciliation(7, "provider:wire").unwrap();
                conn.execute(
                    "UPDATE events SET payload = ?1 WHERE event_type = 'operation.settled'
                     AND sequence = (SELECT MAX(sequence) FROM events WHERE event_type = 'operation.settled')",
                    [rmp_serde::to_vec(&settled).unwrap()],
                )
                .unwrap();
            }),
        ),
    ] {
        let dir = TempDir::new().expect("tempdir");
        let path = temp_db_path(&dir);
        let mut store = KernelStore::open(&path).expect("open");
        let task = task_id(0x51);
        create_open_task(&mut store, task, command_id(0x52));
        let close_cmd = command_id(0x53);
        let (operation_id, _) = accept_begin_close(&mut store, task, close_cmd, 1);
        drop(store);
        let conn = open_raw(&path);
        let accepted_ms = accepted_at_ms(&conn, operation_id);
        drop(conn);
        let mut store = KernelStore::open(&path).expect("reopen");
        store
            .record_outcome(
                OperationOutcome::new(
                    operation_id,
                    accepted_ms + 1,
                    Some(1),
                    None,
                    OutcomeSource::Dispatch,
                    OperationOutcomeKind::Uncertain {
                        code: OperationUncertaintyCode::AmbiguousDispatch,
                    },
                )
                .unwrap(),
            )
            .unwrap();
        store
            .record_outcome(
                OperationOutcome::new(
                    operation_id,
                    accepted_ms + 2,
                    Some(1),
                    None,
                    OutcomeSource::verified_reconciliation(0, "provider:wire").unwrap(),
                    OperationOutcomeKind::Settled {
                        result_event_ids: vec![event_id(0x54)],
                    },
                )
                .unwrap(),
            )
            .unwrap();
        drop(store);
        {
            let conn = open_raw(&path);
            tamper(&conn);
        }
        let mut store = KernelStore::open(&path).expect("reopen");
        let err = store
            .execute(command_envelope(
                close_cmd,
                Some(task),
                Some(1),
                Command::BeginCloseTask,
            ))
            .expect_err(label);
        assert!(
            matches!(
                err,
                StoreError::Corruption
                    | StoreError::CodecMismatch { .. }
                    | StoreError::EventDecode(_)
                    | StoreError::Projection(_)
            ),
            "{label}: {err:?}"
        );
    }
}

#[test]
fn command_outcome_terminal_outbox_dispatch_metadata_rules() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut store = KernelStore::open(&path).expect("open");
    let task = task_id(0x61);
    create_open_task(&mut store, task, command_id(0x62));
    let close_cmd = command_id(0x63);
    let envelope = command_envelope(close_cmd, Some(task), Some(1), Command::BeginCloseTask);
    let first = store.execute(envelope.clone()).expect("accept");
    let CommandReceipt::Accepted { operation_id, .. } = first.clone() else {
        panic!("accepted");
    };
    drop(store);

    let conn = open_raw(&path);
    let accepted_ms = accepted_at_ms(&conn, operation_id);
    drop(conn);

    let mut store = KernelStore::open(&path).expect("reopen");
    store
        .record_outcome(settle_close_outcome(
            operation_id,
            accepted_ms + 10,
            1,
            event_id(0x64),
        ))
        .expect("settle");
    drop(store);

    // Positive: simulated completed dispatch metadata is tolerated.
    {
        let conn = open_raw(&path);
        conn.execute(
            "UPDATE outbox
             SET attempts = 2,
                 available_at_ms = ?1,
                 dispatch_started_at_ms = ?2
             WHERE operation_id = ?3",
            rusqlite::params![
                accepted_ms + 1,
                accepted_ms + 2,
                operation_id.as_bytes().as_slice()
            ],
        )
        .unwrap();
    }
    let mut store = KernelStore::open(&path).expect("reopen");
    let retry = store
        .execute(envelope.clone())
        .expect("dup with dispatch meta");
    assert_eq!(retry, first);
    drop(store);

    // Negative malformed metadata cases.
    for (label, sql_args) in [
        (
            "attempts>0 but dispatch_started NULL",
            (2i64, None::<i64>, Some(accepted_ms)),
        ),
        (
            "attempts=0 but dispatch_started set",
            (0i64, Some(accepted_ms + 1), Some(accepted_ms)),
        ),
        (
            "dispatch_started before accepted",
            (1i64, Some(accepted_ms - 1), Some(accepted_ms)),
        ),
        (
            "available_at before accepted",
            (1i64, Some(accepted_ms + 1), Some(accepted_ms - 1)),
        ),
        ("negative attempts", (-1i64, None::<i64>, Some(accepted_ms))),
    ] {
        let dir = TempDir::new().expect("tempdir");
        let path = temp_db_path(&dir);
        let mut store = KernelStore::open(&path).expect("open");
        let task = task_id(0x61);
        create_open_task(&mut store, task, command_id(0x62));
        let close_cmd = command_id(0x63);
        let envelope = command_envelope(close_cmd, Some(task), Some(1), Command::BeginCloseTask);
        let first = store.execute(envelope.clone()).expect("accept");
        let CommandReceipt::Accepted { operation_id, .. } = first else {
            panic!("accepted");
        };
        drop(store);
        let conn = open_raw(&path);
        let accepted_ms = accepted_at_ms(&conn, operation_id);
        drop(conn);
        let mut store = KernelStore::open(&path).expect("reopen");
        store
            .record_outcome(settle_close_outcome(
                operation_id,
                accepted_ms + 10,
                1,
                event_id(0x64),
            ))
            .unwrap();
        drop(store);
        {
            let conn = open_raw(&path);
            let (attempts, dispatch_started, available_at) = sql_args;
            // Recompute relative to this DB's accepted_ms.
            let dispatch_started = dispatch_started.map(|d| {
                if d == accepted_ms + 1 || d == accepted_ms - 1 {
                    // values already absolute from outer; rebuild from pattern via label
                    d
                } else {
                    d
                }
            });
            let _ = dispatch_started;
            match label {
                "attempts>0 but dispatch_started NULL" => {
                    conn.execute(
                        "UPDATE outbox SET attempts = 2, dispatch_started_at_ms = NULL,
                         available_at_ms = ?1 WHERE operation_id = ?2",
                        rusqlite::params![accepted_ms, operation_id.as_bytes().as_slice()],
                    )
                    .unwrap();
                }
                "attempts=0 but dispatch_started set" => {
                    conn.execute(
                        "UPDATE outbox SET attempts = 0, dispatch_started_at_ms = ?1,
                         available_at_ms = ?2 WHERE operation_id = ?3",
                        rusqlite::params![
                            accepted_ms + 1,
                            accepted_ms,
                            operation_id.as_bytes().as_slice()
                        ],
                    )
                    .unwrap();
                }
                "dispatch_started before accepted" => {
                    conn.execute(
                        "UPDATE outbox SET attempts = 1, dispatch_started_at_ms = ?1,
                         available_at_ms = ?2 WHERE operation_id = ?3",
                        rusqlite::params![
                            accepted_ms - 1,
                            accepted_ms,
                            operation_id.as_bytes().as_slice()
                        ],
                    )
                    .unwrap();
                }
                "available_at before accepted" => {
                    conn.execute(
                        "UPDATE outbox SET attempts = 1, dispatch_started_at_ms = ?1,
                         available_at_ms = ?2 WHERE operation_id = ?3",
                        rusqlite::params![
                            accepted_ms + 1,
                            accepted_ms - 1,
                            operation_id.as_bytes().as_slice()
                        ],
                    )
                    .unwrap();
                }
                "negative attempts" => {
                    conn.execute(
                        "UPDATE outbox SET attempts = -1, dispatch_started_at_ms = NULL,
                         available_at_ms = ?1 WHERE operation_id = ?2",
                        rusqlite::params![accepted_ms, operation_id.as_bytes().as_slice()],
                    )
                    .unwrap();
                }
                _ => unreachable!(),
            }
            let _ = (attempts, available_at);
        }
        let mut store = KernelStore::open(&path).expect("reopen");
        let err = store
            .execute(envelope)
            .expect_err(&format!("{label} must fail"));
        assert!(matches!(err, StoreError::Corruption), "{label}: {err:?}");
    }
}

#[test]
fn command_outcome_close_refuses_live_resources_and_projection_tampers_fail() {
    // Live Active resource blocks new close settlement.
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut store = KernelStore::open(&path).expect("open");
    let task = task_id(0x71);
    create_open_task(&mut store, task, command_id(0x72));
    drop(store);
    let live = resource_id(0x73);
    seed_active_resource(&path, task, live, 1);

    let mut store = KernelStore::open(&path).expect("reopen");
    let close_cmd = command_id(0x74);
    let (operation_id, _) = accept_begin_close(&mut store, task, close_cmd, 2);
    drop(store);

    let conn = open_raw(&path);
    let accepted_ms = accepted_at_ms(&conn, operation_id);
    let events_before = count_table(&conn, "events");
    // Resource remains Active while task is Closing.
    let lifecycle: String = conn
        .query_row(
            "SELECT lifecycle FROM resources WHERE resource_id = ?1",
            [live.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(lifecycle, "active");
    drop(conn);

    let mut store = KernelStore::open(&path).expect("reopen");
    let err = store
        .record_outcome(settle_close_outcome(
            operation_id,
            accepted_ms + 1,
            1,
            event_id(0x75),
        ))
        .expect_err("live resource must block archive");
    assert_eq!(err, StoreError::StaleFence);
    drop(store);
    let conn = open_raw(&path);
    assert_eq!(count_table(&conn, "events"), events_before);
    let (op_state, _, _, _, _) = operation_projection(&conn, operation_id);
    assert_eq!(op_state, "accepted");
    drop(conn);

    // Projection tampers after successful settle (no live resources) fail closed on retries.
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut store = KernelStore::open(&path).expect("open");
    let task = task_id(0x76);
    create_open_task(&mut store, task, command_id(0x77));
    let close_cmd = command_id(0x78);
    let (operation_id, _) = accept_begin_close(&mut store, task, close_cmd, 1);
    drop(store);
    let conn = open_raw(&path);
    let accepted_ms = accepted_at_ms(&conn, operation_id);
    drop(conn);
    let result_id = event_id(0x79);
    let outcome = settle_close_outcome(operation_id, accepted_ms + 1, 1, result_id);
    let mut store = KernelStore::open(&path).expect("reopen");
    store.record_outcome(outcome.clone()).expect("settle");
    drop(store);

    {
        let conn = open_raw(&path);
        conn.execute(
            "UPDATE tasks SET lifecycle = 'closing' WHERE task_id = ?1",
            [task.as_bytes().as_slice()],
        )
        .unwrap();
    }
    let mut store = KernelStore::open(&path).expect("reopen");
    let err = store
        .record_outcome(outcome.clone())
        .expect_err("task projection must remain archived");
    assert!(
        matches!(err, StoreError::Corruption | StoreError::Projection(_)),
        "{err:?}"
    );
    drop(store);

    // Release projection tamper.
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut store = KernelStore::open(&path).expect("open");
    let task = task_id(0x7A);
    create_open_task(&mut store, task, command_id(0x7B));
    drop(store);
    let resource = resource_id(0x7C);
    seed_active_resource(&path, task, resource, 4);
    let mut store = KernelStore::open(&path).expect("reopen");
    let (operation_id, _) =
        accept_release_resource(&mut store, task, command_id(0x7D), resource, 2);
    drop(store);
    let conn = open_raw(&path);
    let accepted_ms = accepted_at_ms(&conn, operation_id);
    drop(conn);
    let outcome = settle_release_outcome(
        operation_id,
        accepted_ms + 1,
        0,
        resource,
        4,
        event_id(0x7E),
    );
    let mut store = KernelStore::open(&path).expect("reopen");
    store
        .record_outcome(outcome.clone())
        .expect("settle release");
    drop(store);
    {
        let conn = open_raw(&path);
        conn.execute(
            "UPDATE resources SET lifecycle = 'releasing' WHERE resource_id = ?1",
            [resource.as_bytes().as_slice()],
        )
        .unwrap();
    }
    let mut store = KernelStore::open(&path).expect("reopen");
    let err = store
        .record_outcome(outcome)
        .expect_err("resource projection must remain released");
    assert!(
        matches!(err, StoreError::Corruption | StoreError::Projection(_)),
        "{err:?}"
    );
}

fn backdate_terminal_history(
    conn: &Connection,
    operation_id: OperationId,
    event_type: &str,
    backdated_ms: i64,
) {
    conn.execute(
        "UPDATE operations SET outcome_at_ms = ?1 WHERE operation_id = ?2",
        rusqlite::params![backdated_ms, operation_id.as_bytes().as_slice()],
    )
    .unwrap();
    let payload: Vec<u8> = conn
        .query_row(
            "SELECT payload FROM events WHERE event_type = ?1
             ORDER BY sequence DESC LIMIT 1",
            [event_type],
            |row| row.get(0),
        )
        .unwrap();
    let new_payload = match event_type {
        "operation.settled" => {
            let mut fact: OperationSettledFact = rmp_serde::from_slice(&payload).unwrap();
            fact.settled_at_ms = backdated_ms;
            rmp_serde::to_vec(&fact).unwrap()
        }
        "operation.failed" => {
            let mut fact: OperationFailedFact = rmp_serde::from_slice(&payload).unwrap();
            fact.settled_at_ms = backdated_ms;
            rmp_serde::to_vec(&fact).unwrap()
        }
        "operation.cancelled" => {
            let mut fact: OperationCancelledFact = rmp_serde::from_slice(&payload).unwrap();
            fact.settled_at_ms = backdated_ms;
            rmp_serde::to_vec(&fact).unwrap()
        }
        "operation.uncertain" => {
            let mut fact: OperationUncertainFact = rmp_serde::from_slice(&payload).unwrap();
            fact.observed_at_ms = backdated_ms;
            rmp_serde::to_vec(&fact).unwrap()
        }
        other => panic!("unexpected event type {other}"),
    };
    conn.execute(
        "UPDATE events
         SET payload = ?1, occurred_at_ms = ?2
         WHERE event_type = ?3
           AND sequence = (
             SELECT MAX(sequence) FROM events WHERE event_type = ?3
           )",
        rusqlite::params![new_payload, backdated_ms, event_type],
    )
    .unwrap();
}

#[test]
fn command_outcome_terminal_chronology_rejects_backdated_terminals() {
    for (label, kind, event_type) in [
        (
            "settled",
            OperationOutcomeKind::Settled {
                result_event_ids: vec![event_id(0x81)],
            },
            "operation.settled",
        ),
        (
            "failed",
            OperationOutcomeKind::Failed {
                code: OperationErrorCode::SideEffectFailed,
            },
            "operation.failed",
        ),
        (
            "cancelled",
            OperationOutcomeKind::Cancelled {
                reason: CancellationReason::Superseded,
            },
            "operation.cancelled",
        ),
        (
            "uncertain",
            OperationOutcomeKind::Uncertain {
                code: OperationUncertaintyCode::AmbiguousDispatch,
            },
            "operation.uncertain",
        ),
    ] {
        let dir = TempDir::new().expect("tempdir");
        let path = temp_db_path(&dir);
        let mut store = KernelStore::open(&path).expect("open");
        let task = task_id(0x80);
        create_open_task(&mut store, task, command_id(0x82));
        let close_cmd = command_id(0x83);
        let envelope = command_envelope(close_cmd, Some(task), Some(1), Command::BeginCloseTask);
        let first = store.execute(envelope.clone()).expect("accept");
        let CommandReceipt::Accepted { operation_id, .. } = first.clone() else {
            panic!("accepted");
        };
        drop(store);

        let conn = open_raw(&path);
        let accepted_ms = accepted_at_ms(&conn, operation_id);
        drop(conn);

        let outcome = match &kind {
            OperationOutcomeKind::Settled { result_event_ids } => {
                settle_close_outcome(operation_id, accepted_ms + 5, 1, result_event_ids[0])
            }
            other => OperationOutcome::new(
                operation_id,
                accepted_ms + 5,
                Some(1),
                None,
                OutcomeSource::Dispatch,
                other.clone(),
            )
            .unwrap(),
        };
        let mut store = KernelStore::open(&path).expect("reopen");
        store.record_outcome(outcome.clone()).expect(label);
        drop(store);

        {
            let conn = open_raw(&path);
            backdate_terminal_history(&conn, operation_id, event_type, accepted_ms - 1);
        }

        let mut store = KernelStore::open(&path).expect("reopen");
        let err_cmd = store
            .execute(envelope)
            .expect_err(&format!("{label} dup command"));
        assert!(
            matches!(err_cmd, StoreError::Corruption),
            "{label} command: {err_cmd:?}"
        );
        let err_out = store
            .record_outcome(outcome)
            .expect_err(&format!("{label} dup outcome"));
        assert!(
            matches!(err_out, StoreError::Corruption),
            "{label} outcome: {err_out:?}"
        );
    }
}

#[test]
fn command_outcome_dispatch_available_must_not_follow_started() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut store = KernelStore::open(&path).expect("open");
    let task = task_id(0x84);
    create_open_task(&mut store, task, command_id(0x85));
    let close_cmd = command_id(0x86);
    let envelope = command_envelope(close_cmd, Some(task), Some(1), Command::BeginCloseTask);
    let first = store.execute(envelope.clone()).expect("accept");
    let CommandReceipt::Accepted { operation_id, .. } = first.clone() else {
        panic!("accepted");
    };
    drop(store);

    let conn = open_raw(&path);
    let accepted_ms = accepted_at_ms(&conn, operation_id);
    drop(conn);

    let mut store = KernelStore::open(&path).expect("reopen");
    store
        .record_outcome(settle_close_outcome(
            operation_id,
            accepted_ms + 20,
            1,
            event_id(0x87),
        ))
        .expect("settle");
    drop(store);

    // Positive: accepted <= available <= started <= outcome
    {
        let conn = open_raw(&path);
        conn.execute(
            "UPDATE outbox
             SET attempts = 3,
                 available_at_ms = ?1,
                 dispatch_started_at_ms = ?2
             WHERE operation_id = ?3",
            rusqlite::params![
                accepted_ms + 2,
                accepted_ms + 5,
                operation_id.as_bytes().as_slice()
            ],
        )
        .unwrap();
    }
    let mut store = KernelStore::open(&path).expect("reopen");
    let ok = store
        .execute(envelope.clone())
        .expect("valid dispatch metadata");
    assert_eq!(ok, first);
    drop(store);

    // Negative: available_at after dispatch_started
    {
        let conn = open_raw(&path);
        conn.execute(
            "UPDATE outbox
             SET attempts = 3,
                 available_at_ms = ?1,
                 dispatch_started_at_ms = ?2
             WHERE operation_id = ?3",
            rusqlite::params![
                accepted_ms + 9,
                accepted_ms + 5,
                operation_id.as_bytes().as_slice()
            ],
        )
        .unwrap();
    }
    let mut store = KernelStore::open(&path).expect("reopen");
    let err = store
        .execute(envelope)
        .expect_err("available after started must fail");
    assert_eq!(err, StoreError::Corruption);
}

#[test]
fn command_outcome_settled_projection_lineage_matches_durable_lifecycle_chain() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut store = KernelStore::open(&path).expect("open");
    let task = task_id(0x88);
    create_open_task(&mut store, task, command_id(0x89));
    let close_cmd = command_id(0x8A);
    let envelope = command_envelope(close_cmd, Some(task), Some(1), Command::BeginCloseTask);
    let first = store.execute(envelope.clone()).expect("accept close");
    let CommandReceipt::Accepted { operation_id, .. } = first.clone() else {
        panic!("accepted");
    };
    drop(store);

    let conn = open_raw(&path);
    let accepted_ms = accepted_at_ms(&conn, operation_id);
    drop(conn);

    let result_id = event_id(0x8B);
    let outcome = settle_close_outcome(operation_id, accepted_ms + 1, 1, result_id);
    let mut store = KernelStore::open(&path).expect("reopen");
    store.record_outcome(outcome.clone()).expect("archive");

    // Legitimate archive -> reopen advances revision and lifecycle via durable facts.
    store
        .execute(command_envelope(
            command_id(0x8C),
            Some(task),
            Some(3),
            Command::ReopenTask,
        ))
        .expect("reopen");
    let after_reopen = store
        .record_outcome(outcome.clone())
        .expect("exact retry after reopen");
    assert_eq!(
        after_reopen,
        OperationState::Settled {
            settled_at_ms: accepted_ms + 1,
            result_event_ids: vec![result_id],
        }
    );
    let retry = store
        .execute(envelope.clone())
        .expect("dup command after reopen");
    assert_eq!(retry, first);
    drop(store);

    // Valid-wire projection lifecycle tamper without matching durable event.
    {
        let conn = open_raw(&path);
        conn.execute(
            "UPDATE tasks SET lifecycle = 'closing' WHERE task_id = ?1",
            [task.as_bytes().as_slice()],
        )
        .unwrap();
    }
    let mut store = KernelStore::open(&path).expect("reopen");
    let err = store
        .record_outcome(outcome.clone())
        .expect_err("lifecycle tamper");
    assert_eq!(err, StoreError::Corruption);
    drop(store);

    // Restore lifecycle, tamper action_epoch.
    {
        let conn = open_raw(&path);
        conn.execute(
            "UPDATE tasks SET lifecycle = 'open', action_epoch = 99 WHERE task_id = ?1",
            [task.as_bytes().as_slice()],
        )
        .unwrap();
    }
    let mut store = KernelStore::open(&path).expect("reopen");
    let err = store
        .record_outcome(outcome.clone())
        .expect_err("action_epoch tamper");
    assert_eq!(err, StoreError::Corruption);
    drop(store);

    // Restore epoch, tamper revision ahead of durable max.
    {
        let conn = open_raw(&path);
        conn.execute(
            "UPDATE tasks SET action_epoch = 1, revision = 99 WHERE task_id = ?1",
            [task.as_bytes().as_slice()],
        )
        .unwrap();
    }
    let mut store = KernelStore::open(&path).expect("reopen");
    let err = store
        .record_outcome(outcome)
        .expect_err("revision ahead of durable max");
    assert_eq!(err, StoreError::Corruption);
    let err_cmd = store
        .execute(envelope)
        .expect_err("dup command after revision tamper");
    assert_eq!(err_cmd, StoreError::Corruption);
}

#[test]
fn command_outcome_preserves_pre_transition_dispatch_metadata() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut store = KernelStore::open(&path).expect("open");
    let task = task_id(0x91);
    create_open_task(&mut store, task, command_id(0x92));
    let close_cmd = command_id(0x93);
    let (operation_id, _) = accept_begin_close(&mut store, task, close_cmd, 1);
    drop(store);

    let conn = open_raw(&path);
    let accepted_ms = accepted_at_ms(&conn, operation_id);
    let available = accepted_ms + 3;
    let started = accepted_ms + 5;
    let lease = accepted_ms + 50;
    conn.execute(
        "UPDATE outbox
         SET available_at_ms = ?1,
             dispatch_started_at_ms = ?2,
             attempts = 2,
             leased_until_ms = ?3
         WHERE operation_id = ?4",
        rusqlite::params![
            available,
            started,
            lease,
            operation_id.as_bytes().as_slice()
        ],
    )
    .unwrap();
    let before = load_outbox_row(&conn, operation_id);
    drop(conn);

    let result_id = event_id(0x94);
    let mut store = KernelStore::open(&path).expect("reopen");
    store
        .record_outcome(settle_close_outcome(
            operation_id,
            started + 2,
            1,
            result_id,
        ))
        .expect("settle claimed pending row");
    drop(store);

    let conn = open_raw(&path);
    let after = load_outbox_row(&conn, operation_id);
    assert_eq!(after.0, before.0, "outbox_id");
    assert_eq!(after.1, before.1, "effect_index");
    assert_eq!(after.2, before.2, "event_sequence");
    assert_eq!(after.3, before.3, "destination_class");
    assert_eq!(after.4, before.4, "replay_policy");
    assert_eq!(after.5, before.5, "payload");
    assert_eq!(after.7, available, "available_at preserved");
    assert_eq!(after.9, Some(started), "dispatch_started preserved");
    assert_eq!(after.10, 2, "attempts preserved");
    assert_eq!(after.8, None, "lease cleared");
    assert_eq!(after.6, "settled");
    assert_eq!(after.11, None);
}

#[test]
fn command_outcome_task_history_validator_projection_and_lineage() {
    // Zero-write: projection revision ahead of durable history before settlement.
    {
        let dir = TempDir::new().expect("tempdir");
        let path = temp_db_path(&dir);
        let mut store = KernelStore::open(&path).expect("open");
        let task = task_id(0x95);
        create_open_task(&mut store, task, command_id(0x96));
        let (operation_id, _) = accept_begin_close(&mut store, task, command_id(0x97), 1);
        drop(store);
        let conn = open_raw(&path);
        let accepted_ms = accepted_at_ms(&conn, operation_id);
        let events_before = count_table(&conn, "events");
        let durable_max: i64 = conn
            .query_row(
                "SELECT MAX(task_revision) FROM events WHERE task_id = ?1",
                [task.as_bytes().as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        conn.execute(
            "UPDATE tasks SET revision = ?1 WHERE task_id = ?2",
            rusqlite::params![durable_max + 5, task.as_bytes().as_slice()],
        )
        .unwrap();
        drop(conn);
        let mut store = KernelStore::open(&path).expect("reopen");
        let err = store
            .record_outcome(settle_close_outcome(
                operation_id,
                accepted_ms + 1,
                1,
                event_id(0x98),
            ))
            .expect_err("projection revision ahead");
        assert_eq!(err, StoreError::Corruption);
        drop(store);
        let conn = open_raw(&path);
        assert_eq!(count_table(&conn, "events"), events_before);
        let (state, _, _, _, _) = operation_projection(&conn, operation_id);
        assert_eq!(state, "accepted");
    }

    // Zero-write: projection revision behind durable history before settlement.
    {
        let dir = TempDir::new().expect("tempdir");
        let path = temp_db_path(&dir);
        let mut store = KernelStore::open(&path).expect("open");
        let task = task_id(0x99);
        create_open_task(&mut store, task, command_id(0x9A));
        let (operation_id, _) = accept_begin_close(&mut store, task, command_id(0x9B), 1);
        drop(store);
        let conn = open_raw(&path);
        let accepted_ms = accepted_at_ms(&conn, operation_id);
        let events_before = count_table(&conn, "events");
        conn.execute(
            "UPDATE tasks SET revision = 1 WHERE task_id = ?1",
            [task.as_bytes().as_slice()],
        )
        .unwrap();
        drop(conn);
        let mut store = KernelStore::open(&path).expect("reopen");
        let err = store
            .record_outcome(settle_close_outcome(
                operation_id,
                accepted_ms + 1,
                1,
                event_id(0x9C),
            ))
            .expect_err("projection revision behind");
        assert_eq!(err, StoreError::Corruption);
        drop(store);
        let conn = open_raw(&path);
        assert_eq!(count_table(&conn, "events"), events_before);
    }

    // Exact-retry: later durable revision gap / duplicate / invalid reopen->archive swap.
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut store = KernelStore::open(&path).expect("open");
    let task = task_id(0x9D);
    create_open_task(&mut store, task, command_id(0x9E));
    let close_cmd = command_id(0x9F);
    let envelope = command_envelope(close_cmd, Some(task), Some(1), Command::BeginCloseTask);
    let first = store.execute(envelope.clone()).expect("accept");
    let CommandReceipt::Accepted { operation_id, .. } = first.clone() else {
        panic!("accepted");
    };
    drop(store);
    let conn = open_raw(&path);
    let accepted_ms = accepted_at_ms(&conn, operation_id);
    drop(conn);
    let result_id = event_id(0xA0);
    let outcome = settle_close_outcome(operation_id, accepted_ms + 1, 1, result_id);
    let mut store = KernelStore::open(&path).expect("reopen");
    store.record_outcome(outcome.clone()).expect("settle");
    store
        .execute(command_envelope(
            command_id(0xA5),
            Some(task),
            Some(3),
            Command::ReopenTask,
        ))
        .expect("reopen");
    store
        .record_outcome(outcome.clone())
        .expect("exact retry after valid reopen");
    drop(store);

    // Gap: bump a later mutation revision, leaving a hole.
    {
        let conn = open_raw(&path);
        conn.execute(
            "UPDATE events SET task_revision = 99
             WHERE event_type = 'task.reopened' AND task_id = ?1",
            [task.as_bytes().as_slice()],
        )
        .unwrap();
        conn.execute(
            "UPDATE tasks SET revision = 99 WHERE task_id = ?1",
            [task.as_bytes().as_slice()],
        )
        .unwrap();
    }
    let mut store = KernelStore::open(&path).expect("reopen");
    let err = store
        .record_outcome(outcome.clone())
        .expect_err("revision gap");
    assert_eq!(err, StoreError::Corruption);
    drop(store);

    // Restore gap, then duplicate a revision onto reopen.
    {
        let conn = open_raw(&path);
        conn.execute(
            "UPDATE events SET task_revision = 4
             WHERE event_type = 'task.reopened' AND task_id = ?1",
            [task.as_bytes().as_slice()],
        )
        .unwrap();
        // Force duplicate by copying archived revision onto reopen.
        conn.execute(
            "UPDATE events SET task_revision = 3
             WHERE event_type = 'task.reopened' AND task_id = ?1",
            [task.as_bytes().as_slice()],
        )
        .unwrap();
        conn.execute(
            "UPDATE tasks SET revision = 3, lifecycle = 'open', action_epoch = 1 WHERE task_id = ?1",
            [task.as_bytes().as_slice()],
        )
        .unwrap();
    }
    let mut store = KernelStore::open(&path).expect("reopen");
    let err = store
        .record_outcome(outcome.clone())
        .expect_err("revision duplicate");
    assert_eq!(err, StoreError::Corruption);
    drop(store);

    // Restore duplicate, then valid-wire swap TaskReopened payload into TaskArchived type
    // after open (illegal archive-from-open) with matching projection.
    {
        let conn = open_raw(&path);
        conn.execute(
            "UPDATE events SET task_revision = 4
             WHERE event_type = 'task.reopened' AND task_id = ?1",
            [task.as_bytes().as_slice()],
        )
        .unwrap();
        let payload: Vec<u8> = conn
            .query_row(
                "SELECT payload FROM events WHERE event_type = 'task.reopened' AND task_id = ?1",
                [task.as_bytes().as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        // Keep unit payload; change type to task.archived (illegal after open semantics).
        conn.execute(
            "UPDATE events SET event_type = 'task.archived'
             WHERE event_type = 'task.reopened' AND task_id = ?1",
            [task.as_bytes().as_slice()],
        )
        .unwrap();
        let _ = payload;
        conn.execute(
            "UPDATE tasks SET lifecycle = 'archived', revision = 4, action_epoch = 1 WHERE task_id = ?1",
            [task.as_bytes().as_slice()],
        )
        .unwrap();
    }
    let mut store = KernelStore::open(&path).expect("reopen");
    let err = store
        .record_outcome(outcome.clone())
        .expect_err("illegal reopen->archived swap");
    assert_eq!(err, StoreError::Corruption);
    let err_cmd = store
        .execute(envelope)
        .expect_err("dup command after illegal swap");
    assert_eq!(err_cmd, StoreError::Corruption);
}

#[test]
fn command_outcome_envelope_and_complete_terminal_history_correlation() {
    // Accepted envelope-only timestamp tamper.
    {
        let dir = TempDir::new().expect("tempdir");
        let path = temp_db_path(&dir);
        let mut store = KernelStore::open(&path).expect("open");
        let task = task_id(0xB0);
        create_open_task(&mut store, task, command_id(0xB1));
        let close_cmd = command_id(0xB2);
        let envelope = command_envelope(close_cmd, Some(task), Some(1), Command::BeginCloseTask);
        let first = store.execute(envelope.clone()).expect("accept");
        let CommandReceipt::Accepted { operation_id, .. } = first else {
            panic!("accepted");
        };
        drop(store);
        let conn = open_raw(&path);
        let accepted_ms = accepted_at_ms(&conn, operation_id);
        conn.execute(
            "UPDATE events SET occurred_at_ms = ?1
             WHERE event_type = 'operation.accepted'
               AND sequence = (
                 SELECT MAX(sequence) FROM events WHERE event_type = 'operation.accepted'
               )",
            [accepted_ms + 99],
        )
        .unwrap();
        drop(conn);
        let mut store = KernelStore::open(&path).expect("reopen");
        let err = store
            .record_outcome(settle_close_outcome(
                operation_id,
                accepted_ms + 1,
                1,
                event_id(0xB3),
            ))
            .expect_err("accepted envelope timestamp");
        assert_eq!(err, StoreError::Corruption);
        let err_cmd = store
            .execute(envelope)
            .expect_err("dup after envelope tamper");
        assert_eq!(err_cmd, StoreError::Corruption);
    }

    // Terminal envelope-only timestamp tamper after settle.
    {
        let dir = TempDir::new().expect("tempdir");
        let path = temp_db_path(&dir);
        let mut store = KernelStore::open(&path).expect("open");
        let task = task_id(0xB4);
        create_open_task(&mut store, task, command_id(0xB5));
        let (operation_id, _) = accept_begin_close(&mut store, task, command_id(0xB6), 1);
        drop(store);
        let conn = open_raw(&path);
        let accepted_ms = accepted_at_ms(&conn, operation_id);
        drop(conn);
        let outcome = settle_close_outcome(operation_id, accepted_ms + 1, 1, event_id(0xB7));
        let mut store = KernelStore::open(&path).expect("reopen");
        store.record_outcome(outcome.clone()).expect("settle");
        drop(store);
        {
            let conn = open_raw(&path);
            conn.execute(
                "UPDATE events SET occurred_at_ms = ?1
                 WHERE event_type = 'operation.settled'
                   AND sequence = (
                     SELECT MAX(sequence) FROM events WHERE event_type = 'operation.settled'
                   )",
                [accepted_ms + 50],
            )
            .unwrap();
        }
        let mut store = KernelStore::open(&path).expect("reopen");
        let err = store
            .record_outcome(outcome)
            .expect_err("terminal envelope timestamp");
        assert_eq!(err, StoreError::Corruption);
    }

    // Non-null terminal task_revision.
    {
        let dir = TempDir::new().expect("tempdir");
        let path = temp_db_path(&dir);
        let mut store = KernelStore::open(&path).expect("open");
        let task = task_id(0xB8);
        create_open_task(&mut store, task, command_id(0xB9));
        let (operation_id, _) = accept_begin_close(&mut store, task, command_id(0xBA), 1);
        drop(store);
        let conn = open_raw(&path);
        let accepted_ms = accepted_at_ms(&conn, operation_id);
        drop(conn);
        let outcome = settle_close_outcome(operation_id, accepted_ms + 1, 1, event_id(0xBB));
        let mut store = KernelStore::open(&path).expect("reopen");
        store.record_outcome(outcome.clone()).expect("settle");
        drop(store);
        {
            let conn = open_raw(&path);
            conn.execute(
                "UPDATE events SET task_revision = 7
                 WHERE event_type = 'operation.settled'
                   AND sequence = (
                     SELECT MAX(sequence) FROM events WHERE event_type = 'operation.settled'
                   )",
                [],
            )
            .unwrap();
        }
        let mut store = KernelStore::open(&path).expect("reopen");
        let err = store
            .record_outcome(outcome)
            .expect_err("terminal task_revision must be null");
        assert_eq!(err, StoreError::Corruption);
    }

    // Extra same-operation terminal fact under another task scope.
    {
        let dir = TempDir::new().expect("tempdir");
        let path = temp_db_path(&dir);
        let mut store = KernelStore::open(&path).expect("open");
        let task = task_id(0xBC);
        let other = task_id(0xBD);
        create_open_task(&mut store, task, command_id(0xBE));
        create_open_task(&mut store, other, command_id(0xBF));
        let (operation_id, _) = accept_begin_close(&mut store, task, command_id(0xC8), 1);
        drop(store);
        let conn = open_raw(&path);
        let accepted_ms = accepted_at_ms(&conn, operation_id);
        drop(conn);
        let outcome = settle_close_outcome(operation_id, accepted_ms + 1, 1, event_id(0xC9));
        let mut store = KernelStore::open(&path).expect("reopen");
        store.record_outcome(outcome.clone()).expect("settle");
        drop(store);
        {
            let conn = open_raw(&path);
            let (payload, schema, occurred): (Vec<u8>, i64, i64) = conn
                .query_row(
                    "SELECT payload, schema_version, occurred_at_ms FROM events
                     WHERE event_type = 'operation.settled'
                     ORDER BY sequence DESC LIMIT 1",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .unwrap();
            let fake_id = event_id(0xCA);
            conn.execute(
                "INSERT INTO events(
                    event_id, task_id, task_revision, event_type, schema_version,
                    occurred_at_ms, payload
                 ) VALUES (?1, ?2, NULL, 'operation.settled', ?3, ?4, ?5)",
                rusqlite::params![
                    fake_id.as_bytes().as_slice(),
                    other.as_bytes().as_slice(),
                    schema,
                    occurred,
                    payload,
                ],
            )
            .unwrap();
        }
        let mut store = KernelStore::open(&path).expect("reopen");
        let err = store
            .record_outcome(outcome)
            .expect_err("extra terminal under other task");
        assert_eq!(err, StoreError::Corruption);
    }

    // Extra same-operation terminal fact before acceptance sequence.
    {
        let dir = TempDir::new().expect("tempdir");
        let path = temp_db_path(&dir);
        let mut store = KernelStore::open(&path).expect("open");
        let task = task_id(0xCB);
        create_open_task(&mut store, task, command_id(0xCC));
        let (operation_id, _) = accept_begin_close(&mut store, task, command_id(0xCD), 1);
        drop(store);
        let conn = open_raw(&path);
        let accepted_ms = accepted_at_ms(&conn, operation_id);
        let accepted_seq: i64 = conn
            .query_row(
                "SELECT sequence FROM events WHERE event_type = 'operation.accepted'
                 ORDER BY sequence DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        drop(conn);
        let outcome = settle_close_outcome(operation_id, accepted_ms + 1, 1, event_id(0xCE));
        let mut store = KernelStore::open(&path).expect("reopen");
        store.record_outcome(outcome.clone()).expect("settle");
        drop(store);
        {
            let conn = open_raw(&path);
            let (payload, schema, occurred): (Vec<u8>, i64, i64) = conn
                .query_row(
                    "SELECT payload, schema_version, occurred_at_ms FROM events
                     WHERE event_type = 'operation.settled'
                     ORDER BY sequence DESC LIMIT 1",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .unwrap();
            // Insert with a lower sequence by rewriting: delete/reinsert is hard; instead
            // clone payload onto a new row then force sequence via UPDATE if possible.
            // SQLite AUTOINCREMENT sequences only increase, so plant the clone with
            // occurred time and then move its sequence backward by swapping with a gap
            // is unavailable. Instead update an early unrelated operation.* row is not
            // possible. Use a direct INSERT and then UPDATE sequence if the column allows.
            let fake_id = event_id(0xCF);
            conn.execute(
                "INSERT INTO events(
                    event_id, task_id, task_revision, event_type, schema_version,
                    occurred_at_ms, payload
                 ) VALUES (?1, ?2, NULL, 'operation.settled', ?3, ?4, ?5)",
                rusqlite::params![
                    fake_id.as_bytes().as_slice(),
                    task.as_bytes().as_slice(),
                    schema,
                    occurred,
                    payload,
                ],
            )
            .unwrap();
            let inserted_seq: i64 = conn
                .query_row(
                    "SELECT sequence FROM events WHERE event_id = ?1",
                    [fake_id.as_bytes().as_slice()],
                    |row| row.get(0),
                )
                .unwrap();
            // Swap sequences with a pre-acceptance event by exchanging sequence numbers.
            // Prefer rewriting the planted row's sequence down if UNIQUE permits via temp.
            conn.execute_batch("PRAGMA defer_foreign_keys = ON;").ok();
            let early_id: Vec<u8> = conn
                .query_row(
                    "SELECT event_id FROM events WHERE sequence = 1",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            let temp_seq = inserted_seq + 1000;
            conn.execute(
                "UPDATE events SET sequence = ?1 WHERE event_id = ?2",
                rusqlite::params![temp_seq, fake_id.as_bytes().as_slice()],
            )
            .unwrap();
            conn.execute(
                "UPDATE events SET sequence = ?1 WHERE event_id = ?2",
                rusqlite::params![inserted_seq, early_id.as_slice()],
            )
            .unwrap();
            conn.execute(
                "UPDATE events SET sequence = ?1 WHERE event_id = ?2",
                rusqlite::params![1i64, fake_id.as_bytes().as_slice()],
            )
            .unwrap();
            assert!(1 < accepted_seq);
        }
        let mut store = KernelStore::open(&path).expect("reopen");
        let err = store
            .record_outcome(outcome)
            .expect_err("pre-acceptance terminal fact");
        assert_eq!(err, StoreError::Corruption);
    }
}

#[test]
fn command_outcome_live_resources_only_block_settled_archive() {
    for (label, kind, task_tail, live_tail, cmd_tail) in [
        (
            "failed",
            OperationOutcomeKind::Failed {
                code: OperationErrorCode::SideEffectFailed,
            },
            0xD0u8,
            0x11u8,
            0x12u8,
        ),
        (
            "cancelled",
            OperationOutcomeKind::Cancelled {
                reason: CancellationReason::Superseded,
            },
            0xD1,
            0x13,
            0x14,
        ),
        (
            "uncertain",
            OperationOutcomeKind::Uncertain {
                code: OperationUncertaintyCode::AmbiguousDispatch,
            },
            0xD2,
            0x15,
            0x16,
        ),
    ] {
        let dir = TempDir::new().expect("tempdir");
        let path = temp_db_path(&dir);
        let mut store = KernelStore::open(&path).expect("open");
        let task = task_id(task_tail);
        create_open_task(&mut store, task, command_id(0x10));
        drop(store);
        let live = resource_id(live_tail);
        seed_active_resource(&path, task, live, 1);
        let mut store = KernelStore::open(&path).expect("reopen");
        let (operation_id, _) = accept_begin_close(&mut store, task, command_id(cmd_tail), 2);
        drop(store);
        let conn = open_raw(&path);
        let accepted_ms = accepted_at_ms(&conn, operation_id);
        drop(conn);
        let outcome = OperationOutcome::new(
            operation_id,
            accepted_ms + 1,
            Some(1),
            None,
            OutcomeSource::Dispatch,
            kind,
        )
        .unwrap();
        let mut store = KernelStore::open(&path).expect("reopen");
        store
            .record_outcome(outcome)
            .unwrap_or_else(|e| panic!("{label} with live resource must succeed: {e:?}"));
    }

    // Verified reconciliation Failed with live resources.
    {
        let dir = TempDir::new().expect("tempdir");
        let path = temp_db_path(&dir);
        let mut store = KernelStore::open(&path).expect("open");
        let task = task_id(0xD4);
        create_open_task(&mut store, task, command_id(0xD5));
        drop(store);
        seed_active_resource(&path, task, resource_id(0xD6), 2);
        let mut store = KernelStore::open(&path).expect("reopen");
        let (operation_id, _) = accept_begin_close(&mut store, task, command_id(0xD7), 2);
        drop(store);
        let conn = open_raw(&path);
        let accepted_ms = accepted_at_ms(&conn, operation_id);
        drop(conn);
        let mut store = KernelStore::open(&path).expect("reopen");
        store
            .record_outcome(
                OperationOutcome::new(
                    operation_id,
                    accepted_ms + 1,
                    Some(1),
                    None,
                    OutcomeSource::Dispatch,
                    OperationOutcomeKind::Uncertain {
                        code: OperationUncertaintyCode::AmbiguousDispatch,
                    },
                )
                .unwrap(),
            )
            .expect("uncertain");
        store
            .record_outcome(
                OperationOutcome::new(
                    operation_id,
                    accepted_ms + 2,
                    Some(1),
                    None,
                    OutcomeSource::verified_reconciliation(0, "provider:fail").unwrap(),
                    OperationOutcomeKind::Failed {
                        code: OperationErrorCode::SideEffectFailed,
                    },
                )
                .unwrap(),
            )
            .expect("reconcile failed with live resource");
    }

    // Settled close still blocked by live Active resource.
    {
        let dir = TempDir::new().expect("tempdir");
        let path = temp_db_path(&dir);
        let mut store = KernelStore::open(&path).expect("open");
        let task = task_id(0xD8);
        create_open_task(&mut store, task, command_id(0xD9));
        drop(store);
        seed_active_resource(&path, task, resource_id(0xDA), 1);
        let mut store = KernelStore::open(&path).expect("reopen");
        let (operation_id, _) = accept_begin_close(&mut store, task, command_id(0xDB), 2);
        drop(store);
        let conn = open_raw(&path);
        let accepted_ms = accepted_at_ms(&conn, operation_id);
        let events_before = count_table(&conn, "events");
        drop(conn);
        let mut store = KernelStore::open(&path).expect("reopen");
        let err = store
            .record_outcome(settle_close_outcome(
                operation_id,
                accepted_ms + 1,
                1,
                event_id(0xDC),
            ))
            .expect_err("settled archive blocked");
        assert_eq!(err, StoreError::StaleFence);
        drop(store);
        let conn = open_raw(&path);
        assert_eq!(count_table(&conn, "events"), events_before);
    }

    // Invalid resource lifecycle on task-bound row => Corruption, zero writes.
    {
        let dir = TempDir::new().expect("tempdir");
        let path = temp_db_path(&dir);
        let mut store = KernelStore::open(&path).expect("open");
        let task = task_id(0xDD);
        create_open_task(&mut store, task, command_id(0xDE));
        drop(store);
        let bad = resource_id(0xDF);
        seed_active_resource(&path, task, bad, 1);
        let mut store = KernelStore::open(&path).expect("reopen");
        let (operation_id, _) = accept_begin_close(&mut store, task, command_id(0xE0), 2);
        drop(store);
        let conn = open_raw(&path);
        let accepted_ms = accepted_at_ms(&conn, operation_id);
        conn.execute(
            "UPDATE resources SET lifecycle = 'bogus' WHERE resource_id = ?1",
            [bad.as_bytes().as_slice()],
        )
        .unwrap();
        let events_before = count_table(&conn, "events");
        let op_before = operation_projection(&conn, operation_id);
        drop(conn);
        let mut store = KernelStore::open(&path).expect("reopen");
        let err = store
            .record_outcome(settle_close_outcome(
                operation_id,
                accepted_ms + 1,
                1,
                event_id(0xE1),
            ))
            .expect_err("invalid lifecycle");
        assert_eq!(err, StoreError::Corruption);
        drop(store);
        let conn = open_raw(&path);
        assert_eq!(count_table(&conn, "events"), events_before);
        assert_eq!(operation_projection(&conn, operation_id), op_before);
    }
}

#[test]
fn command_outcome_reconciled_dispatch_started_before_uncertain() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut store = KernelStore::open(&path).expect("open");
    let task = task_id(0xE2);
    create_open_task(&mut store, task, command_id(0xE3));
    let (operation_id, _) = accept_begin_close(&mut store, task, command_id(0xE8), 1);
    drop(store);
    let conn = open_raw(&path);
    let accepted_ms = accepted_at_ms(&conn, operation_id);
    drop(conn);

    let mut store = KernelStore::open(&path).expect("reopen");
    let uncertain_at = accepted_ms + 4;
    store
        .record_outcome(
            OperationOutcome::new(
                operation_id,
                uncertain_at,
                Some(1),
                None,
                OutcomeSource::Dispatch,
                OperationOutcomeKind::Uncertain {
                    code: OperationUncertaintyCode::AmbiguousDispatch,
                },
            )
            .unwrap(),
        )
        .expect("uncertain");
    let final_at = uncertain_at + 10;
    let settle = OperationOutcome::new(
        operation_id,
        final_at,
        Some(1),
        None,
        OutcomeSource::verified_reconciliation(0, "provider:ok").unwrap(),
        OperationOutcomeKind::Settled {
            result_event_ids: vec![event_id(0xE9)],
        },
    )
    .unwrap();
    store
        .record_outcome(settle.clone())
        .expect("reconcile settle");
    drop(store);

    // Valid nonzero-attempt reconciled case: accepted <= available <= started <= uncertain.
    {
        let conn = open_raw(&path);
        conn.execute(
            "UPDATE outbox
             SET attempts = 2,
                 available_at_ms = ?1,
                 dispatch_started_at_ms = ?2
             WHERE operation_id = ?3",
            rusqlite::params![
                accepted_ms + 1,
                accepted_ms + 2,
                operation_id.as_bytes().as_slice()
            ],
        )
        .unwrap();
    }
    let mut store = KernelStore::open(&path).expect("reopen");
    store
        .record_outcome(settle.clone())
        .expect("valid reconciled dispatch timing");
    drop(store);

    // Tamper: accepted <= available <= uncertain < started <= final.
    {
        let conn = open_raw(&path);
        conn.execute(
            "UPDATE outbox
             SET attempts = 2,
                 available_at_ms = ?1,
                 dispatch_started_at_ms = ?2
             WHERE operation_id = ?3",
            rusqlite::params![
                accepted_ms + 1,
                uncertain_at + 1,
                operation_id.as_bytes().as_slice()
            ],
        )
        .unwrap();
    }
    let mut store = KernelStore::open(&path).expect("reopen");
    let err = store
        .record_outcome(settle)
        .expect_err("started after uncertain must fail");
    assert_eq!(err, StoreError::Corruption);
}

#[test]
fn command_outcome_release_projection_updated_at_matches_result() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut store = KernelStore::open(&path).expect("open");
    let task = task_id(0xEA);
    create_open_task(&mut store, task, command_id(0xEB));
    drop(store);
    let resource = resource_id(0xEC);
    seed_active_resource(&path, task, resource, 6);
    let mut store = KernelStore::open(&path).expect("reopen");
    let (operation_id, _) =
        accept_release_resource(&mut store, task, command_id(0xED), resource, 2);
    drop(store);
    let conn = open_raw(&path);
    let accepted_ms = accepted_at_ms(&conn, operation_id);
    drop(conn);
    let result_id = event_id(0xEE);
    let occurred = accepted_ms + 3;
    let outcome = settle_release_outcome(operation_id, occurred, 0, resource, 6, result_id);
    let mut store = KernelStore::open(&path).expect("reopen");
    store
        .record_outcome(outcome.clone())
        .expect("settle release");
    drop(store);
    let conn = open_raw(&path);
    let updated: i64 = conn
        .query_row(
            "SELECT updated_at_ms FROM resources WHERE resource_id = ?1",
            [resource.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(updated, occurred);
    drop(conn);

    {
        let conn = open_raw(&path);
        conn.execute(
            "UPDATE resources SET updated_at_ms = ?1 WHERE resource_id = ?2",
            rusqlite::params![occurred + 99, resource.as_bytes().as_slice()],
        )
        .unwrap();
    }
    let mut store = KernelStore::open(&path).expect("reopen");
    let err = store
        .record_outcome(outcome)
        .expect_err("updated_at tamper");
    assert_eq!(err, StoreError::Corruption);
}

#[test]
fn command_outcome_outbox_update_abort_rolls_back_completely() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut store = KernelStore::open(&path).expect("open");
    let task = task_id(0xEF);
    create_open_task(&mut store, task, command_id(0xF0));
    let (operation_id, _) = accept_begin_close(&mut store, task, command_id(0xF7), 1);
    drop(store);

    let conn = open_raw(&path);
    let accepted_ms = accepted_at_ms(&conn, operation_id);
    // Simulate pre-transition dispatch metadata that must remain unchanged on rollback.
    conn.execute(
        "UPDATE outbox
         SET available_at_ms = ?1,
             dispatch_started_at_ms = ?2,
             attempts = 3,
             leased_until_ms = ?3
         WHERE operation_id = ?4",
        rusqlite::params![
            accepted_ms + 1,
            accepted_ms + 2,
            accepted_ms + 99,
            operation_id.as_bytes().as_slice()
        ],
    )
    .unwrap();
    let event_ids_before: Vec<Vec<u8>> = {
        let mut stmt = conn
            .prepare("SELECT event_id FROM events ORDER BY sequence ASC")
            .unwrap();
        stmt.query_map([], |row| row.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    };
    let events_before = event_ids_before.len() as i64;
    let op_before: (
        String,
        Option<Vec<u8>>,
        Option<String>,
        i64,
        Option<i64>,
        Option<i64>,
        Option<Vec<u8>>,
        Option<i64>,
    ) = conn
        .query_row(
            "SELECT state, result, outcome_code, accepted_at_ms, outcome_at_ms,
                    action_epoch, resource_id, runtime_generation
             FROM operations WHERE operation_id = ?1",
            [operation_id.as_bytes().as_slice()],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                ))
            },
        )
        .unwrap();
    let task_before: (String, i64, i64) = conn
        .query_row(
            "SELECT lifecycle, action_epoch, revision FROM tasks WHERE task_id = ?1",
            [task.as_bytes().as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    let outbox_before = load_outbox_row(&conn, operation_id);
    conn.execute_batch(
        "CREATE TRIGGER test_abort_outbox_terminal_update
         BEFORE UPDATE OF state ON outbox
         WHEN NEW.state = 'settled'
         BEGIN
           SELECT RAISE(ABORT, 'test outbox terminal abort');
         END;",
    )
    .expect("install outbox update abort trigger");
    drop(conn);

    let mut store = KernelStore::open(&path).expect("reopen");
    let err = store
        .record_outcome(settle_close_outcome(
            operation_id,
            accepted_ms + 5,
            1,
            event_id(0xF8),
        ))
        .expect_err("outbox update abort");
    assert_eq!(err, StoreError::ConstraintViolation);
    drop(store);

    let conn = open_raw(&path);
    let event_ids_after: Vec<Vec<u8>> = {
        let mut stmt = conn
            .prepare("SELECT event_id FROM events ORDER BY sequence ASC")
            .unwrap();
        stmt.query_map([], |row| row.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    };
    assert_eq!(event_ids_after, event_ids_before);
    assert_eq!(count_table(&conn, "events"), events_before);
    let op_after: (
        String,
        Option<Vec<u8>>,
        Option<String>,
        i64,
        Option<i64>,
        Option<i64>,
        Option<Vec<u8>>,
        Option<i64>,
    ) = conn
        .query_row(
            "SELECT state, result, outcome_code, accepted_at_ms, outcome_at_ms,
                    action_epoch, resource_id, runtime_generation
             FROM operations WHERE operation_id = ?1",
            [operation_id.as_bytes().as_slice()],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(op_after, op_before);
    let task_after: (String, i64, i64) = conn
        .query_row(
            "SELECT lifecycle, action_epoch, revision FROM tasks WHERE task_id = ?1",
            [task.as_bytes().as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(task_after, task_before);
    assert_eq!(load_outbox_row(&conn, operation_id), outbox_before);
    assert!(!event_types(&conn).iter().any(|t| t == "task.archived"));
}
#[test]
fn command_outcome_task_history_rejects_payload_identity_and_impossible_release() {
    // TaskCreated embedded task.id diverges from event task scope.
    {
        let dir = TempDir::new().expect("tempdir");
        let path = temp_db_path(&dir);
        let mut store = KernelStore::open(&path).expect("open");
        let task = task_id(0x01);
        create_open_task(&mut store, task, command_id(0x02));
        let (operation_id, _) = accept_begin_close(&mut store, task, command_id(0x03), 1);
        drop(store);
        let conn = open_raw(&path);
        let accepted_ms = accepted_at_ms(&conn, operation_id);
        let events_before = count_table(&conn, "events");
        let payload: Vec<u8> = conn
            .query_row(
                "SELECT payload FROM events WHERE event_type = 'task.created' AND task_id = ?1",
                [task.as_bytes().as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        let mut created: TaskCreatedPayload = rmp_serde::from_slice(&payload).unwrap();
        created.task.id = task_id(0xFE);
        conn.execute(
            "UPDATE events SET payload = ?1
             WHERE event_type = 'task.created' AND task_id = ?2",
            rusqlite::params![
                rmp_serde::to_vec(&created).unwrap(),
                task.as_bytes().as_slice()
            ],
        )
        .unwrap();
        drop(conn);
        let mut store = KernelStore::open(&path).expect("reopen");
        let err = store
            .record_outcome(settle_close_outcome(
                operation_id,
                accepted_ms + 1,
                1,
                event_id(0x04),
            ))
            .expect_err("mismatched TaskCreated id");
        assert_eq!(err, StoreError::Corruption);
        drop(store);
        let conn = open_raw(&path);
        assert_eq!(count_table(&conn, "events"), events_before);
        let (state, _, _, _, _) = operation_projection(&conn, operation_id);
        assert_eq!(state, "accepted");
    }

    // Contiguous ResourceReleased without a valid prior same-task/generation lifecycle.
    {
        let dir = TempDir::new().expect("tempdir");
        let path = temp_db_path(&dir);
        let mut store = KernelStore::open(&path).expect("open");
        let task = task_id(0x05);
        create_open_task(&mut store, task, command_id(0x06));
        let (operation_id, _) = accept_begin_close(&mut store, task, command_id(0x07), 1);
        drop(store);
        let conn = open_raw(&path);
        let accepted_ms = accepted_at_ms(&conn, operation_id);
        let events_before = count_table(&conn, "events");
        let ghost = resource_id(0x08);
        let released = ResourceReleasedPayload {
            resource_id: ghost,
            runtime_generation: 1,
        };
        insert_event(
            &conn,
            event_id(0x09),
            Some(task),
            Some(3),
            "resource.released",
            i64::from(EVENT_SCHEMA_VERSION),
            accepted_ms,
            &rmp_serde::to_vec(&released).unwrap(),
        );
        conn.execute(
            "UPDATE tasks SET revision = 3 WHERE task_id = ?1",
            [task.as_bytes().as_slice()],
        )
        .unwrap();
        drop(conn);
        let mut store = KernelStore::open(&path).expect("reopen");
        let err = store
            .record_outcome(settle_close_outcome(
                operation_id,
                accepted_ms + 1,
                1,
                event_id(0x0A),
            ))
            .expect_err("orphan ResourceReleased");
        assert_eq!(err, StoreError::Corruption);
        drop(store);
        let conn = open_raw(&path);
        assert_eq!(count_table(&conn, "events"), events_before + 1);
        let (state, _, _, _, _) = operation_projection(&conn, operation_id);
        assert_eq!(state, "accepted");
    }
}

#[test]
fn command_outcome_pending_outbox_lease_metadata_rules() {
    // attempts == 0 forbids a live lease (zero writes on settle).
    {
        let dir = TempDir::new().expect("tempdir");
        let path = temp_db_path(&dir);
        let mut store = KernelStore::open(&path).expect("open");
        let task = task_id(0x0B);
        create_open_task(&mut store, task, command_id(0x0C));
        let (operation_id, _) = accept_begin_close(&mut store, task, command_id(0x0D), 1);
        drop(store);
        let conn = open_raw(&path);
        let accepted_ms = accepted_at_ms(&conn, operation_id);
        let events_before = count_table(&conn, "events");
        conn.execute(
            "UPDATE outbox SET leased_until_ms = ?1 WHERE operation_id = ?2",
            rusqlite::params![accepted_ms + 10, operation_id.as_bytes().as_slice()],
        )
        .unwrap();
        drop(conn);
        let mut store = KernelStore::open(&path).expect("reopen");
        let err = store
            .record_outcome(settle_close_outcome(
                operation_id,
                accepted_ms + 1,
                1,
                event_id(0x0E),
            ))
            .expect_err("attempts=0 with lease");
        assert_eq!(err, StoreError::Corruption);
        drop(store);
        let conn = open_raw(&path);
        assert_eq!(count_table(&conn, "events"), events_before);
    }

    // attempts > 0 with lease predating dispatch_started fails closed.
    {
        let dir = TempDir::new().expect("tempdir");
        let path = temp_db_path(&dir);
        let mut store = KernelStore::open(&path).expect("open");
        let task = task_id(0x0F);
        create_open_task(&mut store, task, command_id(0x10));
        let (operation_id, _) = accept_begin_close(&mut store, task, command_id(0x11), 1);
        drop(store);
        let conn = open_raw(&path);
        let accepted_ms = accepted_at_ms(&conn, operation_id);
        let events_before = count_table(&conn, "events");
        conn.execute(
            "UPDATE outbox
             SET attempts = 2,
                 available_at_ms = ?1,
                 dispatch_started_at_ms = ?2,
                 leased_until_ms = ?3
             WHERE operation_id = ?4",
            rusqlite::params![
                accepted_ms + 1,
                accepted_ms + 3,
                accepted_ms + 2,
                operation_id.as_bytes().as_slice()
            ],
        )
        .unwrap();
        drop(conn);
        let mut store = KernelStore::open(&path).expect("reopen");
        let err = store
            .record_outcome(settle_close_outcome(
                operation_id,
                accepted_ms + 5,
                1,
                event_id(0x12),
            ))
            .expect_err("lease before started");
        assert_eq!(err, StoreError::Corruption);
        drop(store);
        let conn = open_raw(&path);
        assert_eq!(count_table(&conn, "events"), events_before);
    }
}

#[test]
fn command_pure_terminal_settled_timestamp_correlation() {
    // Envelope-only terminal timestamp tamper.
    {
        let dir = TempDir::new().expect("tempdir");
        let path = temp_db_path(&dir);
        let mut store = KernelStore::open(&path).expect("open");
        let task = task_id(0x13);
        create_open_task(&mut store, task, command_id(0x14));
        let rename_cmd = command_id(0x15);
        let envelope = command_envelope(
            rename_cmd,
            Some(task),
            Some(1),
            Command::RenameTask(RenameTaskIntent {
                title: "Pure stamp".into(),
            }),
        );
        let first = store.execute(envelope.clone()).expect("rename");
        drop(store);
        {
            let conn = open_raw(&path);
            let accepted: i64 = conn
                .query_row(
                    "SELECT accepted_at_ms FROM operations WHERE command_id = ?1",
                    [rename_cmd.as_bytes().as_slice()],
                    |row| row.get(0),
                )
                .unwrap();
            conn.execute(
                "UPDATE events SET occurred_at_ms = ?1
                 WHERE event_type = 'operation.settled'
                   AND sequence = (
                     SELECT MAX(sequence) FROM events WHERE event_type = 'operation.settled'
                   )",
                [accepted + 99],
            )
            .unwrap();
        }
        let mut store = KernelStore::open(&path).expect("reopen");
        let err = store
            .execute(envelope)
            .expect_err("settled envelope timestamp");
        assert_eq!(err, StoreError::Corruption);
        let _ = first;
    }

    // Coherent pre-acceptance fact/projection times (not equal to accepted_at).
    {
        let dir = TempDir::new().expect("tempdir");
        let path = temp_db_path(&dir);
        let mut store = KernelStore::open(&path).expect("open");
        let task = task_id(0x16);
        create_open_task(&mut store, task, command_id(0x17));
        let rename_cmd = command_id(0x18);
        let envelope = command_envelope(
            rename_cmd,
            Some(task),
            Some(1),
            Command::RenameTask(RenameTaskIntent {
                title: "Preaccept".into(),
            }),
        );
        store.execute(envelope.clone()).expect("rename");
        drop(store);
        {
            let conn = open_raw(&path);
            let accepted: i64 = conn
                .query_row(
                    "SELECT accepted_at_ms FROM operations WHERE command_id = ?1",
                    [rename_cmd.as_bytes().as_slice()],
                    |row| row.get(0),
                )
                .unwrap();
            let forged = accepted - 5;
            assert!(forged < accepted);
            let payload: Vec<u8> = conn
                .query_row(
                    "SELECT payload FROM events WHERE event_type = 'operation.settled'
                     ORDER BY sequence DESC LIMIT 1",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            let mut settled: OperationSettledFact = rmp_serde::from_slice(&payload).unwrap();
            settled.settled_at_ms = forged;
            conn.execute(
                "UPDATE events
                 SET payload = ?1, occurred_at_ms = ?2
                 WHERE event_type = 'operation.settled'
                   AND sequence = (
                     SELECT MAX(sequence) FROM events WHERE event_type = 'operation.settled'
                   )",
                rusqlite::params![rmp_serde::to_vec(&settled).unwrap(), forged],
            )
            .unwrap();
            conn.execute(
                "UPDATE operations SET outcome_at_ms = ?1 WHERE command_id = ?2",
                rusqlite::params![forged, rename_cmd.as_bytes().as_slice()],
            )
            .unwrap();
        }
        let mut store = KernelStore::open(&path).expect("reopen");
        let err = store
            .execute(envelope)
            .expect_err("pre-acceptance coherent times");
        assert_eq!(err, StoreError::Corruption);
    }
}
#[test]
fn command_outcome_full_snapshot_and_archive_integrity() {
    // Deleting a live resource projection before settle cannot hide it: durable replay diverges.
    {
        let dir = TempDir::new().expect("tempdir");
        let path = temp_db_path(&dir);
        let mut store = KernelStore::open(&path).expect("open");
        let task = task_id(0x21);
        create_open_task(&mut store, task, command_id(0x22));
        drop(store);
        let live = resource_id(0x23);
        seed_active_resource(&path, task, live, 1);
        let mut store = KernelStore::open(&path).expect("reopen");
        let (operation_id, _) = accept_begin_close(&mut store, task, command_id(0x24), 2);
        drop(store);
        let conn = open_raw(&path);
        let accepted_ms = accepted_at_ms(&conn, operation_id);
        let events_before = count_table(&conn, "events");
        conn.execute(
            "DELETE FROM resources WHERE resource_id = ?1",
            [live.as_bytes().as_slice()],
        )
        .unwrap();
        drop(conn);
        let mut store = KernelStore::open(&path).expect("reopen");
        let err = store
            .record_outcome(settle_close_outcome(
                operation_id,
                accepted_ms + 1,
                1,
                event_id(0x25),
            ))
            .expect_err("deleted live resource must not hide archive integrity");
        assert!(
            matches!(err, StoreError::Corruption | StoreError::Projection(_)),
            "{err:?}"
        );
        drop(store);
        let conn = open_raw(&path);
        assert_eq!(count_table(&conn, "events"), events_before);
    }

    // Re-scoping a live resource projection likewise fails closed.
    {
        let dir = TempDir::new().expect("tempdir");
        let path = temp_db_path(&dir);
        let mut store = KernelStore::open(&path).expect("open");
        let task = task_id(0x26);
        create_open_task(&mut store, task, command_id(0x27));
        drop(store);
        let live = resource_id(0x28);
        seed_active_resource(&path, task, live, 2);
        let mut store = KernelStore::open(&path).expect("reopen");
        let (operation_id, _) = accept_begin_close(&mut store, task, command_id(0x29), 2);
        drop(store);
        let conn = open_raw(&path);
        let accepted_ms = accepted_at_ms(&conn, operation_id);
        let events_before = count_table(&conn, "events");
        conn.execute(
            "UPDATE resources SET task_id = NULL WHERE resource_id = ?1",
            [live.as_bytes().as_slice()],
        )
        .unwrap();
        drop(conn);
        let mut store = KernelStore::open(&path).expect("reopen");
        let err = store
            .record_outcome(settle_close_outcome(
                operation_id,
                accepted_ms + 1,
                1,
                event_id(0x2A),
            ))
            .expect_err("rescoped live resource");
        assert!(
            matches!(err, StoreError::Corruption | StoreError::Projection(_)),
            "{err:?}"
        );
        drop(store);
        let conn = open_raw(&path);
        assert_eq!(count_table(&conn, "events"), events_before);
    }

    // Legitimate live Active resource still returns StaleFence on new settle.
    {
        let dir = TempDir::new().expect("tempdir");
        let path = temp_db_path(&dir);
        let mut store = KernelStore::open(&path).expect("open");
        let task = task_id(0x2B);
        create_open_task(&mut store, task, command_id(0x2C));
        drop(store);
        seed_active_resource(&path, task, resource_id(0x2D), 1);
        let mut store = KernelStore::open(&path).expect("reopen");
        let (operation_id, _) = accept_begin_close(&mut store, task, command_id(0x2E), 2);
        drop(store);
        let conn = open_raw(&path);
        let accepted_ms = accepted_at_ms(&conn, operation_id);
        drop(conn);
        let mut store = KernelStore::open(&path).expect("reopen");
        let err = store
            .record_outcome(settle_close_outcome(
                operation_id,
                accepted_ms + 1,
                1,
                event_id(0x2F),
            ))
            .expect_err("live resource StaleFence");
        assert_eq!(err, StoreError::StaleFence);
    }

    // Non-lifecycle projection tamper (title) fails exact outcome and dup command.
    {
        let dir = TempDir::new().expect("tempdir");
        let path = temp_db_path(&dir);
        let mut store = KernelStore::open(&path).expect("open");
        let task = task_id(0x30);
        create_open_task(&mut store, task, command_id(0x31));
        let close_cmd = command_id(0x32);
        let envelope = command_envelope(close_cmd, Some(task), Some(1), Command::BeginCloseTask);
        let first = store.execute(envelope.clone()).expect("accept");
        let CommandReceipt::Accepted { operation_id, .. } = first else {
            panic!("accepted");
        };
        drop(store);
        let conn = open_raw(&path);
        let accepted_ms = accepted_at_ms(&conn, operation_id);
        drop(conn);
        let outcome = settle_close_outcome(operation_id, accepted_ms + 1, 1, event_id(0x33));
        let mut store = KernelStore::open(&path).expect("reopen");
        store.record_outcome(outcome.clone()).expect("settle");
        drop(store);
        let events_before = {
            let conn = open_raw(&path);
            conn.execute(
                "UPDATE tasks SET title = 'tampered-title' WHERE task_id = ?1",
                [task.as_bytes().as_slice()],
            )
            .unwrap();
            count_table(&conn, "events")
        };
        let mut store = KernelStore::open(&path).expect("reopen");
        let err = store
            .record_outcome(outcome)
            .expect_err("title tamper exact retry");
        assert_eq!(err, StoreError::Corruption);
        let err_cmd = store
            .execute(envelope)
            .expect_err("title tamper dup command");
        assert_eq!(err_cmd, StoreError::Corruption);
        drop(store);
        let conn = open_raw(&path);
        assert_eq!(count_table(&conn, "events"), events_before);
    }

    // Forged historical TaskArchived while a durable Active resource exists.
    {
        let dir = TempDir::new().expect("tempdir");
        let path = temp_db_path(&dir);
        let mut store = KernelStore::open(&path).expect("open");
        let task = task_id(0x34);
        create_open_task(&mut store, task, command_id(0x35));
        drop(store);
        seed_active_resource(&path, task, resource_id(0x36), 1);
        let mut store = KernelStore::open(&path).expect("reopen");
        let close_cmd = command_id(0x37);
        let envelope = command_envelope(close_cmd, Some(task), Some(2), Command::BeginCloseTask);
        let first = store.execute(envelope.clone()).expect("accept");
        let CommandReceipt::Accepted { operation_id, .. } = first else {
            panic!("accepted");
        };
        drop(store);
        let conn = open_raw(&path);
        let accepted_ms = accepted_at_ms(&conn, operation_id);
        // Force projection to Closing@3 with a forged archived fact while resource stays Active.
        let archive_id = event_id(0x38);
        insert_event(
            &conn,
            archive_id,
            Some(task),
            Some(3),
            "task.archived",
            i64::from(EVENT_SCHEMA_VERSION),
            accepted_ms + 1,
            &rmp_serde::to_vec(&TaskUnitPayload {}).unwrap(),
        );
        conn.execute(
            "UPDATE tasks SET lifecycle = 'archived', revision = 3 WHERE task_id = ?1",
            [task.as_bytes().as_slice()],
        )
        .unwrap();
        let events_before = count_table(&conn, "events");
        drop(conn);
        let mut store = KernelStore::open(&path).expect("reopen");
        let err = store
            .record_outcome(settle_close_outcome(
                operation_id,
                accepted_ms + 2,
                1,
                event_id(0x39),
            ))
            .expect_err("archive with live durable resource");
        assert_eq!(err, StoreError::Corruption);
        let err_cmd = store
            .execute(envelope)
            .expect_err("dup after forged archive");
        assert_eq!(err_cmd, StoreError::Corruption);
        drop(store);
        let conn = open_raw(&path);
        assert_eq!(count_table(&conn, "events"), events_before);
    }
}

#[test]
fn command_outcome_decision_envelope_time_and_unique_accepted() {
    // Envelope-only task.close_begun timestamp tamper.
    {
        let dir = TempDir::new().expect("tempdir");
        let path = temp_db_path(&dir);
        let mut store = KernelStore::open(&path).expect("open");
        let task = task_id(0x3A);
        create_open_task(&mut store, task, command_id(0x3B));
        let close_cmd = command_id(0x3C);
        let envelope = command_envelope(close_cmd, Some(task), Some(1), Command::BeginCloseTask);
        let first = store.execute(envelope.clone()).expect("accept");
        let CommandReceipt::Accepted { operation_id, .. } = first else {
            panic!("accepted");
        };
        drop(store);
        let conn = open_raw(&path);
        let accepted_ms = accepted_at_ms(&conn, operation_id);
        let events_before = count_table(&conn, "events");
        conn.execute(
            "UPDATE events SET occurred_at_ms = ?1
             WHERE event_type = 'task.close_begun'
               AND sequence = (
                 SELECT MAX(sequence) FROM events WHERE event_type = 'task.close_begun'
               )",
            [accepted_ms + 77],
        )
        .unwrap();
        drop(conn);
        let mut store = KernelStore::open(&path).expect("reopen");
        let err = store
            .record_outcome(settle_close_outcome(
                operation_id,
                accepted_ms + 1,
                1,
                event_id(0x3D),
            ))
            .expect_err("close_begun envelope time");
        assert_eq!(err, StoreError::Corruption);
        let err_cmd = store
            .execute(envelope)
            .expect_err("dup after decision time tamper");
        assert_eq!(err_cmd, StoreError::Corruption);
        drop(store);
        let conn = open_raw(&path);
        assert_eq!(count_table(&conn, "events"), events_before);
    }

    // Extra cloned OperationAccepted for the same operation fails closed.
    {
        let dir = TempDir::new().expect("tempdir");
        let path = temp_db_path(&dir);
        let mut store = KernelStore::open(&path).expect("open");
        let task = task_id(0x3E);
        create_open_task(&mut store, task, command_id(0x3F));
        let close_cmd = command_id(0x40);
        let envelope = command_envelope(close_cmd, Some(task), Some(1), Command::BeginCloseTask);
        let first = store.execute(envelope.clone()).expect("accept");
        let CommandReceipt::Accepted { operation_id, .. } = first else {
            panic!("accepted");
        };
        drop(store);
        let conn = open_raw(&path);
        let accepted_ms = accepted_at_ms(&conn, operation_id);
        let (payload, schema, occurred, task_bytes): (Vec<u8>, i64, i64, Vec<u8>) = conn
            .query_row(
                "SELECT payload, schema_version, occurred_at_ms, task_id
                 FROM events WHERE event_type = 'operation.accepted'
                 ORDER BY sequence DESC LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        let events_before = count_table(&conn, "events");
        conn.execute(
            "INSERT INTO events(
                event_id, task_id, task_revision, event_type, schema_version,
                occurred_at_ms, payload
             ) VALUES (?1, ?2, NULL, 'operation.accepted', ?3, ?4, ?5)",
            rusqlite::params![
                event_id(0x41).as_bytes().as_slice(),
                task_bytes,
                schema,
                occurred,
                payload,
            ],
        )
        .unwrap();
        drop(conn);
        let mut store = KernelStore::open(&path).expect("reopen");
        let err = store
            .record_outcome(settle_close_outcome(
                operation_id,
                accepted_ms + 1,
                1,
                event_id(0x42),
            ))
            .expect_err("duplicate accepted fact");
        assert_eq!(err, StoreError::Corruption);
        let err_cmd = store
            .execute(envelope)
            .expect_err("dup command after extra accepted");
        assert_eq!(err_cmd, StoreError::Corruption);
        drop(store);
        let conn = open_raw(&path);
        assert_eq!(count_table(&conn, "events"), events_before + 1);
    }
}
#[test]
fn command_outcome_half_match_command_operation_lineage() {
    // Same command_id, different operation_id on an extra accepted fact.
    {
        let dir = TempDir::new().expect("tempdir");
        let path = temp_db_path(&dir);
        let mut store = KernelStore::open(&path).expect("open");
        let task = task_id(0x70);
        create_open_task(&mut store, task, command_id(0x71));
        let close_cmd = command_id(0x72);
        let envelope = command_envelope(close_cmd, Some(task), Some(1), Command::BeginCloseTask);
        let first = store.execute(envelope.clone()).expect("accept");
        let CommandReceipt::Accepted {
            operation_id: op, ..
        } = first
        else {
            panic!("accepted");
        };
        drop(store);
        let conn = open_raw(&path);
        let accepted_ms = accepted_at_ms(&conn, op);
        let (payload, schema, occurred, task_bytes): (Vec<u8>, i64, i64, Vec<u8>) = conn
            .query_row(
                "SELECT payload, schema_version, occurred_at_ms, task_id
                 FROM events WHERE event_type = 'operation.accepted'
                 ORDER BY sequence DESC LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        let mut fact: OperationAcceptedFact = rmp_serde::from_slice(&payload).unwrap();
        fact.operation_id = operation_id(0x73);
        let events_before = count_table(&conn, "events");
        conn.execute(
            "INSERT INTO events(
                event_id, task_id, task_revision, event_type, schema_version,
                occurred_at_ms, payload
             ) VALUES (?1, ?2, NULL, 'operation.accepted', ?3, ?4, ?5)",
            rusqlite::params![
                event_id(0x74).as_bytes().as_slice(),
                task_bytes,
                schema,
                occurred,
                rmp_serde::to_vec(&fact).unwrap(),
            ],
        )
        .unwrap();
        drop(conn);
        let mut store = KernelStore::open(&path).expect("reopen");
        let err = store
            .record_outcome(settle_close_outcome(op, accepted_ms + 1, 1, event_id(0x75)))
            .expect_err("half-match accepted");
        assert_eq!(err, StoreError::Corruption);
        let err_cmd = store
            .execute(envelope)
            .expect_err("dup after half-match accepted");
        assert_eq!(err_cmd, StoreError::Corruption);
        drop(store);
        let conn = open_raw(&path);
        assert_eq!(count_table(&conn, "events"), events_before + 1);
    }

    // Same command_id, different operation_id on a terminal fact.
    {
        let dir = TempDir::new().expect("tempdir");
        let path = temp_db_path(&dir);
        let mut store = KernelStore::open(&path).expect("open");
        let task = task_id(0x76);
        create_open_task(&mut store, task, command_id(0x77));
        let close_cmd = command_id(0x78);
        let envelope = command_envelope(close_cmd, Some(task), Some(1), Command::BeginCloseTask);
        let first = store.execute(envelope.clone()).expect("accept");
        let CommandReceipt::Accepted {
            operation_id: op, ..
        } = first
        else {
            panic!("accepted");
        };
        drop(store);
        let conn = open_raw(&path);
        let accepted_ms = accepted_at_ms(&conn, op);
        let forged = OperationFailedFact::new(
            close_cmd,
            operation_id(0x79),
            accepted_ms + 5,
            OperationErrorCode::SideEffectFailed,
            Some(1),
            None,
            None,
        )
        .unwrap();
        let events_before = count_table(&conn, "events");
        insert_event(
            &conn,
            event_id(0x7A),
            Some(task),
            None,
            "operation.failed",
            i64::from(EVENT_SCHEMA_VERSION),
            accepted_ms + 5,
            &rmp_serde::to_vec(&forged).unwrap(),
        );
        drop(conn);
        let mut store = KernelStore::open(&path).expect("reopen");
        let err = store
            .record_outcome(settle_close_outcome(op, accepted_ms + 1, 1, event_id(0x7B)))
            .expect_err("half-match terminal");
        assert_eq!(err, StoreError::Corruption);
        let err_cmd = store
            .execute(envelope)
            .expect_err("dup after half-match terminal");
        assert_eq!(err_cmd, StoreError::Corruption);
        drop(store);
        let conn = open_raw(&path);
        assert_eq!(count_table(&conn, "events"), events_before + 1);
    }
}

#[test]
fn command_outcome_accepted_receipt_created_at_matches_accepted_time() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut store = KernelStore::open(&path).expect("open");
    let task = task_id(0x7C);
    create_open_task(&mut store, task, command_id(0x7D));
    let close_cmd = command_id(0x7E);
    let envelope = command_envelope(close_cmd, Some(task), Some(1), Command::BeginCloseTask);
    let first = store.execute(envelope.clone()).expect("accept");
    let CommandReceipt::Accepted { operation_id, .. } = first else {
        panic!("accepted");
    };
    drop(store);
    let conn = open_raw(&path);
    let accepted_ms = accepted_at_ms(&conn, operation_id);
    let events_before = count_table(&conn, "events");
    conn.execute(
        "UPDATE command_receipts SET created_at_ms = ?1 WHERE command_id = ?2",
        rusqlite::params![accepted_ms + 99, close_cmd.as_bytes().as_slice()],
    )
    .unwrap();
    drop(conn);
    let mut store = KernelStore::open(&path).expect("reopen");
    let err = store
        .record_outcome(settle_close_outcome(
            operation_id,
            accepted_ms + 1,
            1,
            event_id(0x7F),
        ))
        .expect_err("receipt created_at tamper");
    assert_eq!(err, StoreError::Corruption);
    let err_cmd = store
        .execute(envelope)
        .expect_err("dup after receipt created_at tamper");
    assert_eq!(err_cmd, StoreError::Corruption);
    drop(store);
    let conn = open_raw(&path);
    assert_eq!(count_table(&conn, "events"), events_before);
}

#[test]
fn command_outcome_task_updated_at_matches_latest_mutation() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut store = KernelStore::open(&path).expect("open");
    let task = task_id(0x80);
    create_open_task(&mut store, task, command_id(0x81));
    let close_cmd = command_id(0x82);
    let envelope = command_envelope(close_cmd, Some(task), Some(1), Command::BeginCloseTask);
    let first = store.execute(envelope.clone()).expect("accept");
    let CommandReceipt::Accepted { operation_id, .. } = first else {
        panic!("accepted");
    };
    drop(store);
    let conn = open_raw(&path);
    let accepted_ms = accepted_at_ms(&conn, operation_id);
    let events_before = count_table(&conn, "events");
    conn.execute(
        "UPDATE tasks SET updated_at_ms = ?1 WHERE task_id = ?2",
        rusqlite::params![accepted_ms + 1234, task.as_bytes().as_slice()],
    )
    .unwrap();
    drop(conn);
    let mut store = KernelStore::open(&path).expect("reopen");
    let err = store
        .record_outcome(settle_close_outcome(
            operation_id,
            accepted_ms + 1,
            1,
            event_id(0x83),
        ))
        .expect_err("updated_at tamper");
    assert_eq!(err, StoreError::Corruption);
    let err_cmd = store
        .execute(envelope)
        .expect_err("dup after updated_at tamper");
    assert_eq!(err_cmd, StoreError::Corruption);
    drop(store);
    let conn = open_raw(&path);
    assert_eq!(count_table(&conn, "events"), events_before);
}

#[test]
fn command_outcome_orphan_derived_archive_is_corruption_not_stale_fence() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut store = KernelStore::open(&path).expect("open");
    let task = task_id(0x84);
    create_open_task(&mut store, task, command_id(0x85));
    let close_cmd = command_id(0x86);
    let envelope = command_envelope(close_cmd, Some(task), Some(1), Command::BeginCloseTask);
    let first = store.execute(envelope.clone()).expect("accept");
    let CommandReceipt::Accepted { operation_id, .. } = first else {
        panic!("accepted");
    };
    drop(store);
    let conn = open_raw(&path);
    let accepted_ms = accepted_at_ms(&conn, operation_id);
    let archive_id = event_id(0x87);
    insert_event(
        &conn,
        archive_id,
        Some(task),
        Some(3),
        "task.archived",
        i64::from(EVENT_SCHEMA_VERSION),
        accepted_ms + 1,
        &rmp_serde::to_vec(&TaskUnitPayload {}).unwrap(),
    );
    conn.execute(
        "UPDATE tasks SET lifecycle = 'archived', revision = 3, updated_at_ms = ?1 WHERE task_id = ?2",
        rusqlite::params![accepted_ms + 1, task.as_bytes().as_slice()],
    )
    .unwrap();
    let events_before = count_table(&conn, "events");
    drop(conn);
    let mut store = KernelStore::open(&path).expect("reopen");
    let err = store
        .record_outcome(settle_close_outcome(
            operation_id,
            accepted_ms + 2,
            1,
            event_id(0x88),
        ))
        .expect_err("orphan archive");
    assert_eq!(err, StoreError::Corruption);
    let err_cmd = store
        .execute(envelope)
        .expect_err("dup after orphan archive");
    assert_eq!(err_cmd, StoreError::Corruption);
    drop(store);
    let conn = open_raw(&path);
    assert_eq!(count_table(&conn, "events"), events_before);
}

#[test]
fn schema_rebuild_rejects_archive_with_live_resource() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    drop(KernelStore::open(&path).expect("open"));
    let task = task_id(0x89);
    let resource = resource_id(0x8A);
    let conn = open_raw(&path);
    seed_task_created(&conn, task, event_id(0x8B), 1_000);
    let resource_facts = ResourceFacts {
        id: resource,
        task_id: Some(task),
        owner_kind: OwnerKind::Task,
        resource_kind: ResourceKind::Terminal,
        recipe: ResourceRecipe::Terminal { cols: 80, rows: 24 },
        lifecycle: ResourceLifecycle::Active,
        runtime_generation: 1,
        updated_at_ms: 1_100,
    };
    insert_event(
        &conn,
        event_id(0x8C),
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
        event_id(0x8D),
        Some(task),
        Some(3),
        "task.close_begun",
        i64::from(EVENT_SCHEMA_VERSION),
        1_200,
        &rmp_serde::to_vec(&TaskCloseBegunPayload { action_epoch: 1 }).unwrap(),
    );
    insert_event(
        &conn,
        event_id(0x8E),
        Some(task),
        Some(4),
        "task.archived",
        i64::from(EVENT_SCHEMA_VERSION),
        1_300,
        &rmp_serde::to_vec(&TaskUnitPayload {}).unwrap(),
    );
    drop(conn);
    let mut store = KernelStore::open(&path).expect("reopen");
    let err = store
        .rebuild_projections()
        .expect_err("archive with live resource");
    assert!(matches!(err, StoreError::Projection(_)), "{err:?}");
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
        let err = store.rebuild_projections().expect_err("accepted envelope");
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
        store.rebuild_projections().expect("accept rebuild");
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
        let err = store.rebuild_projections().expect_err("before acceptance");
        assert!(matches!(err, StoreError::Projection(_)), "{err:?}");
    }

    // Reconciliation before uncertainty
    {
        let dir = TempDir::new().expect("tempdir");
        let path = temp_db_path(&dir);
        let mut store = KernelStore::open(&path).expect("open");
        let task = task_id(0x97);
        create_open_task(&mut store, task, command_id(0x98));
        let close_cmd = command_id(0x99);
        let (op, _) = accept_begin_close(&mut store, task, close_cmd, 1);
        drop(store);
        let conn = open_raw(&path);
        let accepted_ms = accepted_at_ms(&conn, op);
        drop(conn);
        let uncertain_at = accepted_ms + 100;
        let uncertain = OperationOutcome::new(
            op,
            uncertain_at,
            Some(1),
            None,
            OutcomeSource::Dispatch,
            OperationOutcomeKind::Uncertain {
                code: OperationUncertaintyCode::AmbiguousDispatch,
            },
        )
        .unwrap();
        let mut store = KernelStore::open(&path).expect("reopen");
        store.record_outcome(uncertain).expect("mark uncertain");
        drop(store);

        let conn = open_raw(&path);
        // Shadow rebuild of VerifiedReconciliation Failed expects retained final outbox.
        conn.execute(
            "UPDATE outbox
             SET state = 'failed', leased_until_ms = NULL, last_error_class = 'side_effect_failed'
             WHERE operation_id = ?1",
            [op.as_bytes().as_slice()],
        )
        .unwrap();
        let failed = OperationFailedFact::with_source(
            close_cmd,
            op,
            uncertain_at - 1,
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
            uncertain_at - 1,
            &rmp_serde::to_vec(&failed).unwrap(),
        );
        drop(conn);
        let mut store = KernelStore::open(&path).expect("reopen");
        let err = store
            .rebuild_projections()
            .expect_err("reconcile before uncertain");
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
        let err = store.rebuild_projections().expect_err("orphan archive");
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
        store.rebuild_projections().expect("accept");
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
        let err = store
            .rebuild_projections()
            .expect_err("settled without adjacent archive");
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
        store.rebuild_projections().expect("accept");
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
        store.rebuild_projections().expect("valid pair");
    }
}

#[test]
fn schema_rebuild_rejects_orphan_derived_across_sequence_gap() {
    // Orphan task.archived then unrelated event at a higher nonconsecutive sequence.
    // Numeric sequence-1 adjacency would miss the orphan; existing-row adjacency must reject.
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    drop(KernelStore::open(&path).expect("open"));
    let task = task_id(0xB0);
    let other = task_id(0xB1);
    let conn = open_raw(&path);
    seed_task_created(&conn, task, event_id(0xB2), 1_000);
    insert_event(
        &conn,
        event_id(0xB3),
        Some(task),
        Some(2),
        "task.close_begun",
        i64::from(EVENT_SCHEMA_VERSION),
        1_100,
        &rmp_serde::to_vec(&TaskCloseBegunPayload { action_epoch: 1 }).unwrap(),
    );
    insert_event(
        &conn,
        event_id(0xB4),
        Some(task),
        Some(3),
        "task.archived",
        i64::from(EVENT_SCHEMA_VERSION),
        1_200,
        &rmp_serde::to_vec(&TaskUnitPayload {}).unwrap(),
    );
    insert_event_at_sequence(
        &conn,
        10,
        event_id(0xB5),
        Some(other),
        Some(1),
        "task.created",
        i64::from(EVENT_SCHEMA_VERSION),
        1_300,
        &task_created_payload(other),
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
    assert_eq!(
        sequences,
        vec![1, 2, 3, 10],
        "gap fixture must be nonconsecutive"
    );
    drop(conn);
    let mut store = KernelStore::open(&path).expect("reopen");
    let err = store
        .rebuild_projections()
        .expect_err("orphan derived across sequence gap");
    assert!(matches!(err, StoreError::Projection(_)), "{err:?}");
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
    store.rebuild_projections().expect("accept rebuild");
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
    store
        .rebuild_projections()
        .expect("adjacent derived+settle remains valid with earlier gap");
}

#[test]
fn schema_rebuild_rejects_resource_fenced_settle_without_action_epoch() {
    // Constructors allow resource fence with action_epoch None; must not take the pure path.
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    drop(KernelStore::open(&path).expect("open"));
    let task = task_id(0xBE);
    let cmd = command_id(0xBF);
    let op = operation_id(0xC0);
    let resource = resource_id(0xC1);
    let conn = open_raw(&path);
    conn.execute(
        "INSERT INTO command_receipts(
            command_id, client_id, task_id, receipt, committed_sequence, created_at_ms
         ) VALUES (?1, ?2, ?3, X'00', 1, 1100)",
        rusqlite::params![
            cmd.as_bytes().as_slice(),
            client_id(0x01).as_bytes().as_slice(),
            task.as_bytes().as_slice(),
        ],
    )
    .unwrap();
    seed_task_created(&conn, task, event_id(0xC2), 1_000);
    let resource_facts = ResourceFacts {
        id: resource,
        task_id: Some(task),
        owner_kind: OwnerKind::Task,
        resource_kind: ResourceKind::Terminal,
        recipe: ResourceRecipe::Terminal { cols: 80, rows: 24 },
        lifecycle: ResourceLifecycle::Active,
        runtime_generation: 3,
        updated_at_ms: 1_050,
    };
    insert_event(
        &conn,
        event_id(0xC3),
        Some(task),
        Some(2),
        "resource.registered",
        i64::from(EVENT_SCHEMA_VERSION),
        1_050,
        &rmp_serde::to_vec(&ResourceRegisteredPayload {
            resource: resource_facts,
        })
        .unwrap(),
    );
    let accepted =
        OperationAcceptedFact::new(cmd, op, 1_100, None, Some(resource), Some(3)).unwrap();
    insert_event(
        &conn,
        event_id(0xC4),
        Some(task),
        None,
        "operation.accepted",
        i64::from(EVENT_SCHEMA_VERSION),
        1_100,
        &rmp_serde::to_vec(&accepted).unwrap(),
    );
    let settled = OperationSettledFact::new(
        cmd,
        op,
        1_200,
        vec![event_id(0xC5)],
        None,
        Some(resource),
        Some(3),
    )
    .unwrap();
    insert_event(
        &conn,
        event_id(0xC6),
        Some(task),
        None,
        "operation.settled",
        i64::from(EVENT_SCHEMA_VERSION),
        1_200,
        &rmp_serde::to_vec(&settled).unwrap(),
    );
    drop(conn);
    let mut store = KernelStore::open(&path).expect("reopen");
    let err = store
        .rebuild_projections()
        .expect_err("resource-fenced settle without action epoch");
    assert!(matches!(err, StoreError::Projection(_)), "{err:?}");
}

#[test]
fn command_outcome_rejects_global_interleave_between_derived_and_settle() {
    // Archive for op1, then an unrelated task-B event, then op1 settle. Same-task pairing
    // would miss the interleave; a later op2 settle must fail closed.
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut store = KernelStore::open(&path).expect("open");
    let task_a = task_id(0xD0);
    let task_b = task_id(0xD1);
    create_open_task(&mut store, task_a, command_id(0xD2));
    let (op1, _) = accept_begin_close(&mut store, task_a, command_id(0xD3), 1);
    drop(store);

    let conn = open_raw(&path);
    let accepted_ms = accepted_at_ms(&conn, op1);
    let archive_id = event_id(0xD4);
    let settle_at = accepted_ms + 1;
    insert_event(
        &conn,
        archive_id,
        Some(task_a),
        Some(3),
        "task.archived",
        i64::from(EVENT_SCHEMA_VERSION),
        settle_at,
        &rmp_serde::to_vec(&TaskUnitPayload {}).unwrap(),
    );
    // Global interleave: unrelated task-B event between derived and settle.
    insert_event(
        &conn,
        event_id(0xD5),
        Some(task_b),
        Some(1),
        "task.created",
        i64::from(EVENT_SCHEMA_VERSION),
        settle_at,
        &task_created_payload(task_b),
    );
    let settled = OperationSettledFact::new(
        command_id(0xD3),
        op1,
        settle_at,
        vec![archive_id],
        Some(1),
        None,
        None,
    )
    .unwrap();
    insert_event(
        &conn,
        event_id(0xD6),
        Some(task_a),
        None,
        "operation.settled",
        i64::from(EVENT_SCHEMA_VERSION),
        settle_at,
        &rmp_serde::to_vec(&settled).unwrap(),
    );
    conn.execute(
        "UPDATE operations
         SET state = 'settled', outcome_at_ms = ?1, result = ?2
         WHERE operation_id = ?3",
        rusqlite::params![
            settle_at,
            rmp_serde::to_vec(&vec![archive_id]).unwrap(),
            op1.as_bytes().as_slice(),
        ],
    )
    .unwrap();
    conn.execute(
        "UPDATE outbox SET state = 'settled', leased_until_ms = NULL WHERE operation_id = ?1",
        [op1.as_bytes().as_slice()],
    )
    .unwrap();
    conn.execute(
        "UPDATE tasks
         SET lifecycle = 'archived', revision = 3, updated_at_ms = ?1
         WHERE task_id = ?2",
        rusqlite::params![settle_at, task_a.as_bytes().as_slice()],
    )
    .unwrap();
    // Mirror task-B projection enough for later opens (history replay uses events).
    conn.execute(
        "INSERT INTO tasks(
            task_id, environment_id, project_id, title, description, workspace, assignment,
            lifecycle, action_epoch, revision, connectivity, attention, activity,
            review_readiness, primary_agent_session_id, created_at_ms, updated_at_ms
         )
         SELECT ?1, environment_id, project_id, 'B', NULL, workspace, assignment,
                'open', 0, 1, connectivity, attention, activity, review_readiness,
                NULL, ?2, ?2
         FROM tasks WHERE task_id = ?3",
        rusqlite::params![
            task_b.as_bytes().as_slice(),
            settle_at,
            task_a.as_bytes().as_slice(),
        ],
    )
    .unwrap();
    drop(conn);

    let mut store = KernelStore::open(&path).expect("reopen");
    store
        .execute(command_envelope(
            command_id(0xD7),
            Some(task_a),
            Some(3),
            Command::ReopenTask,
        ))
        .expect("reopen A");
    let (op2, _) = accept_begin_close(&mut store, task_a, command_id(0xD8), 4);
    drop(store);
    let conn = open_raw(&path);
    let accepted2 = accepted_at_ms(&conn, op2);
    let events_before = count_table(&conn, "events");
    drop(conn);
    let mut store = KernelStore::open(&path).expect("reopen");
    let err = store
        .record_outcome(settle_close_outcome(op2, accepted2 + 1, 2, event_id(0xD9)))
        .expect_err("global interleave must fail later settle");
    assert_eq!(err, StoreError::Corruption);
    drop(store);
    let conn = open_raw(&path);
    assert_eq!(count_table(&conn, "events"), events_before);
}

#[test]
fn command_outcome_allows_unrelated_events_outside_derived_settle_pair() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut store = KernelStore::open(&path).expect("open");
    let task_a = task_id(0xDA);
    let task_b = task_id(0xDB);
    create_open_task(&mut store, task_a, command_id(0xDC));
    create_open_task(&mut store, task_b, command_id(0xDD));
    let (op, _) = accept_begin_close(&mut store, task_a, command_id(0xDE), 1);
    drop(store);
    let conn = open_raw(&path);
    let accepted_ms = accepted_at_ms(&conn, op);
    drop(conn);
    let outcome = settle_close_outcome(op, accepted_ms + 1, 1, event_id(0xDF));
    let mut store = KernelStore::open(&path).expect("reopen");
    let first = store
        .record_outcome(outcome.clone())
        .expect("settle with B present");
    assert!(matches!(first, OperationState::Settled { .. }));
    let again = store.record_outcome(outcome).expect("exact retry");
    assert!(matches!(again, OperationState::Settled { .. }));
}

#[test]
fn command_outcome_missing_projection_with_durable_lineage_is_corruption() {
    // Projection row deleted while accepted/outbox/receipt lineage remains is Corruption,
    // not MissingOperation. Zero new writes.
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut store = KernelStore::open(&path).expect("open");
    let task = task_id(0xE0);
    create_open_task(&mut store, task, command_id(0xE1));
    let (op, _) = accept_begin_close(&mut store, task, command_id(0xE2), 1);
    drop(store);

    let conn = open_raw(&path);
    let accepted_ms = accepted_at_ms(&conn, op);
    let events_before = count_table(&conn, "events");
    let outbox_before = count_table(&conn, "outbox");
    let receipts_before = count_table(&conn, "command_receipts");
    conn.execute_batch("PRAGMA foreign_keys=OFF;").unwrap();
    conn.execute(
        "DELETE FROM operations WHERE operation_id = ?1",
        [op.as_bytes().as_slice()],
    )
    .unwrap();
    conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
    drop(conn);

    let mut store = KernelStore::open(&path).expect("reopen");
    let err = store
        .record_outcome(settle_close_outcome(op, accepted_ms + 1, 1, event_id(0xE3)))
        .expect_err("durable lineage without projection is corruption");
    assert_eq!(err, StoreError::Corruption);
    drop(store);

    let conn = open_raw(&path);
    assert_eq!(count_table(&conn, "events"), events_before);
    assert_eq!(count_table(&conn, "outbox"), outbox_before);
    assert_eq!(count_table(&conn, "command_receipts"), receipts_before);
    let ops: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM operations WHERE operation_id = ?1",
            [op.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(ops, 0);
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
        let err = store
            .rebuild_projections()
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
        store.rebuild_projections().expect("pure accept");
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
        let err = store
            .rebuild_projections()
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
        store
            .rebuild_projections()
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
        let err = store
            .rebuild_projections()
            .expect_err("accepted requires NULL task_revision");
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
        store.rebuild_projections().expect("accept");
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
        let err = store
            .rebuild_projections()
            .expect_err("settled requires NULL task_revision");
        assert!(matches!(err, StoreError::Projection(_)), "{err:?}");
    }
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
        store.rebuild_projections().expect("accept");
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
        let err = store
            .rebuild_projections()
            .expect_err("effect_index must match durable outbox");
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
        store.rebuild_projections().expect("accept");
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
        let err = store
            .rebuild_projections()
            .expect_err("V1 effect_index must be 0");
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
        store.rebuild_projections().expect("accept");
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
        let err = store
            .rebuild_projections()
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
        store.rebuild_projections().expect("accept");
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
        store
            .rebuild_projections()
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
        store.rebuild_projections().expect("pure accept");
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
        let err = store.rebuild_projections().expect_err("pure cannot fail");
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
        store.rebuild_projections().expect("pure accept");
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
        let err = store
            .rebuild_projections()
            .expect_err("pure settle requires sync decision lineage");
        assert!(matches!(err, StoreError::Projection(_)), "{err:?}");
    }

    // Valid pure create rebuild: created decision id is the settle result.
    {
        let dir = TempDir::new().expect("tempdir");
        let path = temp_db_path(&dir);
        let mut store = KernelStore::open(&path).expect("open");
        let task = task_id(0x39);
        store
            .execute(command_envelope(
                command_id(0x3A),
                None,
                None,
                Command::CreateTask(create_task_intent(task)),
            ))
            .expect("create");
        drop(store);
        let mut store = KernelStore::open(&path).expect("reopen");
        store
            .rebuild_projections()
            .expect("valid pure create rebuild");
    }
}

#[test]
fn command_outcome_release_epoch_tamper_detected_from_task_history() {
    let dir = TempDir::new().expect("tempdir");
    let path = temp_db_path(&dir);
    let mut store = KernelStore::open(&path).expect("open");
    let task = task_id(0x50);
    create_open_task(&mut store, task, command_id(0x51));
    drop(store);
    let resource = resource_id(0x52);
    seed_active_resource(&path, task, resource, 3);
    let mut store = KernelStore::open(&path).expect("reopen");
    let release_cmd = command_id(0x53);
    let (op, _) = accept_release_resource(&mut store, task, release_cmd, resource, 2);
    drop(store);
    let conn = open_raw(&path);
    let accepted_ms = accepted_at_ms(&conn, op);
    drop(conn);
    let result_id = event_id(0x54);
    let outcome = settle_release_outcome(op, accepted_ms + 1, 0, resource, 3, result_id);
    let mut store = KernelStore::open(&path).expect("reopen");
    store
        .record_outcome(outcome.clone())
        .expect("settle release");
    // Later close/reopen so current task epoch is no longer the release epoch.
    let (close_op, _) = accept_begin_close(&mut store, task, command_id(0x55), 4);
    drop(store);
    let conn = open_raw(&path);
    let close_accepted = accepted_at_ms(&conn, close_op);
    drop(conn);
    let mut store = KernelStore::open(&path).expect("reopen");
    store
        .record_outcome(settle_close_outcome(
            close_op,
            close_accepted + 1,
            1,
            event_id(0x56),
        ))
        .expect("archive");
    store
        .execute(command_envelope(
            command_id(0x57),
            Some(task),
            Some(6),
            Command::ReopenTask,
        ))
        .expect("reopen");
    drop(store);

    let conn = open_raw(&path);
    let events_before = count_table(&conn, "events");
    // Consistently rewrite duplicated fence fields to epoch 99; task history remains epoch 0 at release.
    conn.execute(
        "UPDATE operations SET action_epoch = 99 WHERE operation_id = ?1",
        [op.as_bytes().as_slice()],
    )
    .unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT sequence, payload FROM events WHERE event_type = 'operation.accepted'
             ORDER BY sequence ASC",
        )
        .unwrap();
    let rows: Vec<(i64, Vec<u8>)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    drop(stmt);
    for (sequence, payload) in rows {
        let mut fact: OperationAcceptedFact = rmp_serde::from_slice(&payload).unwrap();
        if fact.operation_id == op {
            fact.action_epoch = Some(99);
            conn.execute(
                "UPDATE events SET payload = ?1 WHERE sequence = ?2",
                rusqlite::params![rmp_serde::to_vec(&fact).unwrap(), sequence],
            )
            .unwrap();
        }
    }
    let mut stmt = conn
        .prepare(
            "SELECT sequence, payload FROM events WHERE event_type = 'operation.settled'
             ORDER BY sequence ASC",
        )
        .unwrap();
    let rows: Vec<(i64, Vec<u8>)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    drop(stmt);
    for (sequence, payload) in rows {
        let mut fact: OperationSettledFact = rmp_serde::from_slice(&payload).unwrap();
        if fact.operation_id == op {
            fact.action_epoch = Some(99);
            conn.execute(
                "UPDATE events SET payload = ?1 WHERE sequence = ?2",
                rusqlite::params![rmp_serde::to_vec(&fact).unwrap(), sequence],
            )
            .unwrap();
        }
    }
    let (_dest, _policy, payload): (String, String, Vec<u8>) = conn
        .query_row(
            "SELECT destination_class, replay_policy, payload FROM outbox WHERE operation_id = ?1",
            [op.as_bytes().as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    #[derive(serde::Serialize, serde::Deserialize)]
    struct EffectDocTamper {
        schema_version: u32,
        destination_class: String,
        replay_policy: String,
        effect: ReleaseEffectTamper,
    }
    #[derive(serde::Serialize, serde::Deserialize)]
    struct ResourceFenceTamper {
        resource_id: ResourceId,
        runtime_generation: u64,
    }
    #[derive(serde::Serialize, serde::Deserialize)]
    #[serde(rename_all = "snake_case")]
    enum ReleaseEffectTamper {
        ReleaseResource {
            task_id: TaskId,
            action_epoch: u64,
            resource_fence: ResourceFenceTamper,
        },
    }
    let mut doc: EffectDocTamper = rmp_serde::from_slice(&payload).expect("effect doc");
    match &mut doc.effect {
        ReleaseEffectTamper::ReleaseResource { action_epoch, .. } => *action_epoch = 99,
    }
    conn.execute(
        "UPDATE outbox SET payload = ?1 WHERE operation_id = ?2",
        rusqlite::params![
            rmp_serde::to_vec_named(&doc).unwrap(),
            op.as_bytes().as_slice()
        ],
    )
    .unwrap();
    drop(conn);

    let mut store = KernelStore::open(&path).expect("reopen");
    let err = store
        .record_outcome(outcome)
        .expect_err("history epoch must beat circular operation epoch");
    assert_eq!(err, StoreError::Corruption);
    drop(store);
    let conn = open_raw(&path);
    assert_eq!(count_table(&conn, "events"), events_before);
}
