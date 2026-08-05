use std::fs;
use std::path::Path;

use devmanager::domain::agent::{AgentRole, AgentSessionFacts};
use devmanager::domain::event::{
    AgentSessionRegisteredPayload, OperationAcceptedFact, OperationSettledFact,
    PrimaryAgentSetPayload, ResourceRegisteredPayload, ResourceReleasedPayload, TaskCreatedPayload,
    TaskRenamedPayload, EVENT_SCHEMA_VERSION,
};
use devmanager::domain::id::{
    AgentSessionId, ClientId, CommandId, EnvironmentId, EventId, OperationId, ProjectId,
    ResourceId, TaskId,
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
            &rmp_serde::to_vec(&devmanager::domain::event::ResourceReleaseBegunPayload {
                resource_id: resource,
                runtime_generation: 3,
            })
            .unwrap(),
        );
        insert_event(
            &conn,
            event_id(0x93),
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
    let result_event = event_id(0xC5);

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

        conn.execute(
            "INSERT INTO command_receipts(
                command_id, client_id, task_id, receipt, committed_sequence, created_at_ms
             ) VALUES (?1, ?2, ?3, X'00', 4, 1200)",
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

        // Wrong task scope on an otherwise matching outcome.
        let settled = OperationSettledFact::new(
            cmd,
            op,
            1_300,
            vec![result_event],
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

    // Fix outcome task scope and settle successfully.
    conn.execute(
        "DELETE FROM events WHERE event_type = 'operation.settled'",
        [],
    )
    .unwrap();
    let settled = OperationSettledFact::new(
        cmd,
        op,
        1_400,
        vec![result_event],
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
