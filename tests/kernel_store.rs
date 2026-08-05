use std::fs;
use std::path::{Path, PathBuf};

use devmanager::domain::agent::{AgentRole, AgentSessionFacts, AgentSessionLifecycle};
use devmanager::domain::artifact::ArtifactContentRef;
use devmanager::domain::command::{
    Command, CommandEnvelope, CommandReceipt, CreateTaskIntent, RejectionCode, RenameTaskIntent,
};
use devmanager::domain::event::{
    AgentSessionRegisteredPayload, OperationAcceptedFact, OperationSettledFact,
    PrimaryAgentSetPayload, ResourceRegisteredPayload, ResourceReleasedPayload,
    TaskCloseBegunPayload, TaskCreatedPayload, TaskRenamedPayload, TaskUnitPayload,
    EVENT_SCHEMA_VERSION,
};
use devmanager::domain::id::{
    AgentSessionId, ArtifactId, ClientId, CommandId, EnvironmentId, EventId, OperationId,
    ProjectId, ResourceId, TaskId,
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
    seed_accepted_operation(&path, task, cmd, op, None, None);

    let mut store = KernelStore::open(&path).expect("reopen");
    store.rebuild_projections().expect("accept rebuild");
    drop(store);

    {
        let conn = open_raw(&path);
        let settled =
            OperationSettledFact::new(cmd, op, 1_200, vec![event_id(0x90)], None, None, None)
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
    seed_accepted_operation(&path, task, cmd, op, None, None);

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
            None,
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
            OperationSettledFact::new(cmd, op, 1_300, vec![event_id(0x92)], None, None, None)
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
        let reconciled = OperationSettledFact::with_source(
            cmd,
            op,
            1_400,
            vec![event_id(0x93)],
            None,
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
fn command_pure_effectful_command_unsupported() {
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

    let close_cmd = command_id(0xD0);
    let receipt = store
        .execute(command_envelope(
            close_cmd,
            Some(task),
            Some(1),
            Command::BeginCloseTask,
        ))
        .expect("begin close");
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
    let lifecycle: String = conn
        .query_row(
            "SELECT lifecycle FROM tasks WHERE task_id = ?1",
            [task.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(lifecycle, "open");

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
