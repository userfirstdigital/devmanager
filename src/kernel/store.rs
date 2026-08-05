use std::fmt;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, Transaction};
use sha2::{Digest, Sha256};

use crate::domain::event::{
    AgentSessionRegisteredPayload, ArtifactRegisteredPayload, DomainEvent, Event,
    OperationAcceptedFact, OperationCancelledFact, OperationFailedFact, OperationSettledFact,
    OperationUncertainFact, PrimaryAgentSetPayload, ResourceRegisteredPayload,
    ResourceReleaseBegunPayload, ResourceReleasedPayload, TaskAttentionSetPayload,
    TaskCloseBegunPayload, TaskCreatedPayload, TaskRenamedPayload, TaskUnitPayload,
    EVENT_SCHEMA_VERSION,
};
use crate::domain::id::{EventId, TaskId};
use crate::kernel::projector;
use crate::kernel::schema::{self, Migration, PROJECTION_TABLES};

const BUSY_TIMEOUT_MS: i64 = 5_000;

/// Opaque SQLite-backed kernel store. No public connection accessor.
pub struct KernelStore {
    conn: Connection,
}

impl fmt::Debug for KernelStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("KernelStore")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectionRebuild {
    pub events_replayed: u64,
    pub drift_detected: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreError {
    Io(String),
    Sqlite(String),
    Busy,
    Corruption,
    Truncated,
    IntegrityCheckFailed(String),
    MigrationTooNew { found: i64, supported: i64 },
    MigrationChanged { version: i64 },
    MigrationGap { expected: i64, found: i64 },
    MigrationInterrupted,
    IntegerOutOfRange { field: &'static str, value: u64 },
    CodecMismatch { detail: String },
    EventDecode(String),
    Projection(String),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(msg) => write!(f, "store io error: {msg}"),
            Self::Sqlite(msg) => write!(f, "sqlite error: {msg}"),
            Self::Busy => write!(f, "sqlite busy"),
            Self::Corruption => write!(f, "database corruption detected"),
            Self::Truncated => write!(f, "database file is truncated"),
            Self::IntegrityCheckFailed(msg) => write!(f, "integrity check failed: {msg}"),
            Self::MigrationTooNew { found, supported } => {
                write!(
                    f,
                    "database migration version {found} is newer than supported {supported}"
                )
            }
            Self::MigrationChanged { version } => {
                write!(f, "recorded migration {version} does not match manifest")
            }
            Self::MigrationGap { expected, found } => {
                write!(
                    f,
                    "migration history gap: expected version {expected}, found {found}"
                )
            }
            Self::MigrationInterrupted => write!(f, "interrupted schema migration"),
            Self::IntegerOutOfRange { field, value } => {
                write!(f, "integer out of range for {field}: {value}")
            }
            Self::CodecMismatch { detail } => write!(f, "event codec mismatch: {detail}"),
            Self::EventDecode(msg) => write!(f, "event decode error: {msg}"),
            Self::Projection(msg) => write!(f, "projection error: {msg}"),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<rusqlite::Error> for StoreError {
    fn from(err: rusqlite::Error) -> Self {
        map_sqlite_error(&err)
    }
}

impl KernelStore {
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        classify_path_before_open(path)?;
        let conn = Connection::open(path).map_err(map_open_error)?;
        configure_connection(&conn)?;
        let mut store = Self { conn };
        store.migrate()?;
        store.integrity_check()?;
        Ok(store)
    }

    pub fn rebuild_projections(&mut self) -> Result<ProjectionRebuild, StoreError> {
        let tx = self.conn.transaction()?;
        let result = rebuild_projections_tx(&tx)?;
        tx.commit()?;
        Ok(result)
    }

    fn migrate(&mut self) -> Result<(), StoreError> {
        let manifest = schema::migration_manifest();
        loop {
            let applied = load_applied_migrations(&self.conn)?;
            validate_applied_history(&applied, manifest)?;

            if applied.len() >= manifest.len() {
                return Ok(());
            }

            let next_index = applied.len();
            let Some(migration) = manifest.get(next_index) else {
                return Ok(());
            };

            let expected_version = i64::try_from(next_index.checked_add(1).ok_or(
                StoreError::IntegerOutOfRange {
                    field: "migration_index",
                    value: u64::MAX,
                },
            )?)
            .map_err(|_| StoreError::IntegerOutOfRange {
                field: "migration_version",
                value: u64::MAX,
            })?;

            if migration.version != expected_version {
                return Err(StoreError::MigrationGap {
                    expected: expected_version,
                    found: migration.version,
                });
            }

            if applied.is_empty() {
                detect_interrupted_partial_schema(&self.conn, &applied)?;
            }

            // Apply only the next migration inside its own transaction.
            let tx = self.conn.transaction()?;
            tx.execute_batch(migration.sql)
                .map_err(classify_migration_apply_error)?;
            let applied_at_ms = now_ms()?;
            tx.execute(
                "INSERT INTO schema_migrations(version, name, applied_at_ms, sha256)
                 VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![
                    migration.version,
                    migration.name,
                    applied_at_ms,
                    migration.sha256.as_slice(),
                ],
            )?;
            tx.commit()?;
        }
    }

    fn integrity_check(&self) -> Result<(), StoreError> {
        let result: String = self
            .conn
            .query_row("PRAGMA integrity_check;", [], |row| row.get(0))?;
        if result == "ok" {
            Ok(())
        } else {
            Err(StoreError::IntegrityCheckFailed(result))
        }
    }
}

#[derive(Debug, Clone)]
struct AppliedMigration {
    version: i64,
    name: String,
    sha256: Vec<u8>,
}

fn configure_connection(conn: &Connection) -> Result<(), StoreError> {
    conn.busy_timeout(std::time::Duration::from_millis(
        u64::try_from(BUSY_TIMEOUT_MS).expect("positive"),
    ))?;
    // journal_mode returns the mode; execute_batch is fine for the rest.
    let mode: String = conn.query_row("PRAGMA journal_mode=WAL;", [], |row| row.get(0))?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(StoreError::Sqlite(format!(
            "failed to enable WAL mode, got {mode}"
        )));
    }
    conn.execute_batch(
        "PRAGMA foreign_keys=ON;\n\
         PRAGMA synchronous=NORMAL;",
    )?;
    let foreign_keys: i64 = conn.query_row("PRAGMA foreign_keys;", [], |row| row.get(0))?;
    if foreign_keys != 1 {
        return Err(StoreError::Sqlite(
            "foreign_keys pragma did not enable".into(),
        ));
    }
    let synchronous: i64 = conn.query_row("PRAGMA synchronous;", [], |row| row.get(0))?;
    if synchronous != 1 {
        return Err(StoreError::Sqlite(format!(
            "synchronous pragma expected NORMAL(1), got {synchronous}"
        )));
    }
    let busy_timeout: i64 = conn.query_row("PRAGMA busy_timeout;", [], |row| row.get(0))?;
    if busy_timeout != BUSY_TIMEOUT_MS {
        return Err(StoreError::Sqlite(format!(
            "busy_timeout expected {BUSY_TIMEOUT_MS}, got {busy_timeout}"
        )));
    }
    Ok(())
}

fn classify_path_before_open(path: &Path) -> Result<(), StoreError> {
    if !path.exists() {
        return Ok(());
    }
    let meta = std::fs::metadata(path).map_err(|e| StoreError::Io(e.to_string()))?;
    // Existing files shorter than a minimum valid SQLite header/page are truncated,
    // including a pre-existing zero-byte path (SQLite would otherwise initialize it).
    if meta.len() < 100 {
        return Err(StoreError::Truncated);
    }
    Ok(())
}

fn map_open_error(err: rusqlite::Error) -> StoreError {
    let msg = err.to_string();
    let lower = msg.to_lowercase();
    if lower.contains("truncated") || lower.contains("file is not a database") {
        if lower.contains("truncated") {
            StoreError::Truncated
        } else {
            StoreError::Corruption
        }
    } else if lower.contains("corrupt") {
        StoreError::Corruption
    } else {
        map_sqlite_error(&err)
    }
}

fn map_sqlite_error(err: &rusqlite::Error) -> StoreError {
    match err {
        rusqlite::Error::SqliteFailure(code, _)
            if code.code == rusqlite::ErrorCode::DatabaseBusy
                || code.code == rusqlite::ErrorCode::DatabaseLocked =>
        {
            StoreError::Busy
        }
        rusqlite::Error::SqliteFailure(code, _)
            if code.code == rusqlite::ErrorCode::DatabaseCorrupt =>
        {
            StoreError::Corruption
        }
        other => StoreError::Sqlite(other.to_string()),
    }
}

fn classify_migration_apply_error(err: rusqlite::Error) -> StoreError {
    let msg = err.to_string().to_lowercase();
    if msg.contains("already exists") {
        StoreError::MigrationInterrupted
    } else {
        map_sqlite_error(&err)
    }
}

fn load_applied_migrations(conn: &Connection) -> Result<Vec<AppliedMigration>, StoreError> {
    let table_exists: bool = conn.query_row(
        "SELECT COUNT(*) > 0 FROM sqlite_schema WHERE type = 'table' AND name = 'schema_migrations'",
        [],
        |row| row.get(0),
    )?;
    if !table_exists {
        return Ok(Vec::new());
    }
    let mut stmt =
        conn.prepare("SELECT version, name, sha256 FROM schema_migrations ORDER BY version ASC")?;
    let rows = stmt.query_map([], |row| {
        Ok(AppliedMigration {
            version: row.get(0)?,
            name: row.get(1)?,
            sha256: row.get(2)?,
        })
    })?;
    let mut applied = Vec::new();
    for row in rows {
        applied.push(row?);
    }
    Ok(applied)
}

fn validate_applied_history(
    applied: &[AppliedMigration],
    manifest: &[Migration],
) -> Result<(), StoreError> {
    if applied.is_empty() {
        return Ok(());
    }

    let supported = schema::latest_migration_version();

    for (idx, row) in applied.iter().enumerate() {
        let expected_version = i64::try_from(idx + 1).expect("fits");
        if row.version != expected_version {
            return Err(StoreError::MigrationGap {
                expected: expected_version,
                found: row.version,
            });
        }
        if row.version > supported {
            return Err(StoreError::MigrationTooNew {
                found: row.version,
                supported,
            });
        }
        let Some(expected) = manifest.get(idx) else {
            return Err(StoreError::MigrationTooNew {
                found: row.version,
                supported,
            });
        };
        if row.name != expected.name || row.sha256.as_slice() != expected.sha256.as_slice() {
            return Err(StoreError::MigrationChanged {
                version: row.version,
            });
        }
    }
    Ok(())
}

fn detect_interrupted_partial_schema(
    conn: &Connection,
    applied: &[AppliedMigration],
) -> Result<(), StoreError> {
    if !applied.is_empty() {
        return Ok(());
    }
    // Empty migration history but projection/event tables already present => interrupted apply.
    let partial: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_schema
         WHERE type = 'table'
           AND name IN ('events', 'tasks', 'operations', 'command_receipts', 'outbox',
                        'agent_sessions', 'artifacts', 'resources')",
        [],
        |row| row.get(0),
    )?;
    if partial > 0 {
        return Err(StoreError::MigrationInterrupted);
    }
    Ok(())
}

fn rebuild_projections_tx(tx: &Transaction<'_>) -> Result<ProjectionRebuild, StoreError> {
    for table in PROJECTION_TABLES {
        let shadow = shadow_name(table);
        tx.execute_batch(&format!(
            "DROP TABLE IF EXISTS {shadow};\n\
             CREATE TEMP TABLE {shadow} AS SELECT * FROM {table} WHERE 0;"
        ))?;
    }

    let mut stmt = tx.prepare(
        "SELECT sequence, event_id, task_id, task_revision, event_type, schema_version,
                occurred_at_ms, payload
         FROM events
         ORDER BY sequence ASC",
    )?;
    let mut rows = stmt.query([])?;
    let mut events_replayed: u64 = 0;
    while let Some(row) = rows.next()? {
        let sequence: i64 = row.get(0)?;
        let event_id_bytes: Vec<u8> = row.get(1)?;
        let task_id_bytes: Option<Vec<u8>> = row.get(2)?;
        let task_revision: Option<i64> = row.get(3)?;
        let event_type: String = row.get(4)?;
        let schema_version: i64 = row.get(5)?;
        let occurred_at_ms: i64 = row.get(6)?;
        let payload: Vec<u8> = row.get(7)?;

        let event = decode_stored_event(&event_type, schema_version, &payload)?;
        let domain = DomainEvent {
            id: event_id_from_bytes(&event_id_bytes)?,
            task_id: match task_id_bytes {
                Some(bytes) => Some(task_id_from_bytes(&bytes)?),
                None => None,
            },
            sequence: u64_from_nonnegative_i64("events.sequence", sequence)?,
            task_revision: match task_revision {
                Some(v) => Some(u64_from_nonnegative_i64("events.task_revision", v)?),
                None => None,
            },
            occurred_at_ms,
            payload: event,
        };
        projector::apply_event(tx, &domain, true)?;
        events_replayed = events_replayed
            .checked_add(1)
            .ok_or(StoreError::IntegerOutOfRange {
                field: "events_replayed",
                value: u64::MAX,
            })?;
    }
    drop(rows);
    drop(stmt);

    let mut drift_detected = false;
    for &table in PROJECTION_TABLES {
        if !projection_tables_equal(tx, table)? {
            drift_detected = true;
            break;
        }
    }

    // Atomically replace stable projection table contents (never rename tables).
    // Delete children before parents to satisfy foreign keys.
    tx.execute_batch(
        "DELETE FROM agent_sessions;\n\
         DELETE FROM artifacts;\n\
         DELETE FROM resources;\n\
         DELETE FROM operations;\n\
         DELETE FROM tasks;",
    )?;
    tx.execute_batch(
        "INSERT INTO tasks SELECT * FROM shadow_tasks;\n\
         INSERT INTO operations SELECT * FROM shadow_operations;\n\
         INSERT INTO agent_sessions SELECT * FROM shadow_agent_sessions;\n\
         INSERT INTO artifacts SELECT * FROM shadow_artifacts;\n\
         INSERT INTO resources SELECT * FROM shadow_resources;",
    )?;
    for table in PROJECTION_TABLES {
        tx.execute(&format!("DROP TABLE {}", shadow_name(table)), [])?;
    }

    Ok(ProjectionRebuild {
        events_replayed,
        drift_detected,
    })
}

fn shadow_name(table: &str) -> String {
    format!("shadow_{table}")
}

fn projection_tables_equal(tx: &Transaction<'_>, table: &str) -> Result<bool, StoreError> {
    let stable_dump = canonical_table_dump(tx, table, false)?;
    let shadow_dump = canonical_table_dump(tx, table, true)?;
    Ok(stable_dump == shadow_dump)
}

fn canonical_table_dump(
    tx: &Transaction<'_>,
    table: &str,
    shadow: bool,
) -> Result<Vec<u8>, StoreError> {
    // Hard-coded allowlisted statements only — never interpolate arbitrary table names.
    let sql = match (table, shadow) {
        ("tasks", false) => "SELECT * FROM tasks ORDER BY task_id ASC",
        ("tasks", true) => "SELECT * FROM shadow_tasks ORDER BY task_id ASC",
        ("operations", false) => "SELECT * FROM operations ORDER BY operation_id ASC",
        ("operations", true) => "SELECT * FROM shadow_operations ORDER BY operation_id ASC",
        ("agent_sessions", false) => "SELECT * FROM agent_sessions ORDER BY agent_session_id ASC",
        ("agent_sessions", true) => {
            "SELECT * FROM shadow_agent_sessions ORDER BY agent_session_id ASC"
        }
        ("artifacts", false) => "SELECT * FROM artifacts ORDER BY artifact_id ASC",
        ("artifacts", true) => "SELECT * FROM shadow_artifacts ORDER BY artifact_id ASC",
        ("resources", false) => "SELECT * FROM resources ORDER BY resource_id ASC",
        ("resources", true) => "SELECT * FROM shadow_resources ORDER BY resource_id ASC",
        _ => {
            return Err(StoreError::Projection(format!(
                "unsupported projection table for canonical compare: {table}"
            )))
        }
    };

    let mut hasher = Sha256::new();
    let mut stmt = tx.prepare(sql)?;
    let column_count = stmt.column_count();
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        for idx in 0..column_count {
            let value: rusqlite::types::Value = row.get(idx)?;
            match value {
                rusqlite::types::Value::Null => hasher.update([0]),
                rusqlite::types::Value::Integer(v) => {
                    hasher.update([1]);
                    hasher.update(v.to_le_bytes());
                }
                rusqlite::types::Value::Real(v) => {
                    hasher.update([2]);
                    hasher.update(v.to_bits().to_le_bytes());
                }
                rusqlite::types::Value::Text(v) => {
                    hasher.update([3]);
                    let len = u64::try_from(v.len()).unwrap_or(u64::MAX);
                    hasher.update(len.to_le_bytes());
                    hasher.update(v.as_bytes());
                }
                rusqlite::types::Value::Blob(v) => {
                    hasher.update([4]);
                    let len = u64::try_from(v.len()).unwrap_or(u64::MAX);
                    hasher.update(len.to_le_bytes());
                    hasher.update(&v);
                }
            }
        }
        hasher.update([0xFF]);
    }
    Ok(hasher.finalize().to_vec())
}

pub(crate) fn u64_to_sqlite_i64(field: &'static str, value: u64) -> Result<i64, StoreError> {
    i64::try_from(value).map_err(|_| StoreError::IntegerOutOfRange { field, value })
}

fn u64_from_nonnegative_i64(field: &'static str, value: i64) -> Result<u64, StoreError> {
    u64::try_from(value).map_err(|_| StoreError::IntegerOutOfRange {
        field,
        value: value.unsigned_abs(),
    })
}

fn now_ms() -> Result<i64, StoreError> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| StoreError::Io(e.to_string()))?;
    i64::try_from(duration.as_millis()).map_err(|_| StoreError::IntegerOutOfRange {
        field: "applied_at_ms",
        value: u64::MAX,
    })
}

fn event_id_from_bytes(bytes: &[u8]) -> Result<EventId, StoreError> {
    let array: [u8; 16] = bytes
        .try_into()
        .map_err(|_| StoreError::EventDecode("event_id must be 16 bytes".into()))?;
    EventId::from_bytes(array).map_err(|e| StoreError::EventDecode(e.to_string()))
}

fn task_id_from_bytes(bytes: &[u8]) -> Result<TaskId, StoreError> {
    let array: [u8; 16] = bytes
        .try_into()
        .map_err(|_| StoreError::EventDecode("task_id must be 16 bytes".into()))?;
    TaskId::from_bytes(array).map_err(|e| StoreError::EventDecode(e.to_string()))
}

#[allow(dead_code)] // reserved for Task 1.4 event append path
pub(crate) fn encode_event_payload(event: &Event) -> Result<Vec<u8>, StoreError> {
    let bytes = match event {
        Event::TaskCreated {
            task,
            connectivity,
            attention,
            activity,
            review_readiness,
        } => rmp_serde::to_vec(&TaskCreatedPayload {
            task: task.clone(),
            connectivity: *connectivity,
            attention: *attention,
            activity: *activity,
            review_readiness: *review_readiness,
        }),
        Event::TaskRenamed { title } => rmp_serde::to_vec(&TaskRenamedPayload {
            title: title.clone(),
        }),
        Event::TaskAttentionSet { attention } => rmp_serde::to_vec(&TaskAttentionSetPayload {
            attention: *attention,
        }),
        Event::TaskCloseBegun { action_epoch } => rmp_serde::to_vec(&TaskCloseBegunPayload {
            action_epoch: *action_epoch,
        }),
        Event::TaskReopened => rmp_serde::to_vec(&TaskUnitPayload {}),
        Event::TaskArchived => rmp_serde::to_vec(&TaskUnitPayload {}),
        Event::AgentSessionRegistered { agent } => {
            rmp_serde::to_vec(&AgentSessionRegisteredPayload {
                agent: agent.clone(),
            })
        }
        Event::PrimaryAgentSet { agent_session_id } => rmp_serde::to_vec(&PrimaryAgentSetPayload {
            agent_session_id: *agent_session_id,
        }),
        Event::ArtifactRegistered { artifact } => rmp_serde::to_vec(&ArtifactRegisteredPayload {
            artifact: artifact.clone(),
        }),
        Event::ResourceRegistered { resource } => rmp_serde::to_vec(&ResourceRegisteredPayload {
            resource: resource.clone(),
        }),
        Event::ResourceReleaseBegun {
            resource_id,
            runtime_generation,
        } => rmp_serde::to_vec(&ResourceReleaseBegunPayload {
            resource_id: *resource_id,
            runtime_generation: *runtime_generation,
        }),
        Event::ResourceReleased {
            resource_id,
            runtime_generation,
        } => rmp_serde::to_vec(&ResourceReleasedPayload {
            resource_id: *resource_id,
            runtime_generation: *runtime_generation,
        }),
        Event::OperationAccepted(fact) => rmp_serde::to_vec(fact),
        Event::OperationSettled(fact) => rmp_serde::to_vec(fact),
        Event::OperationFailed(fact) => rmp_serde::to_vec(fact),
        Event::OperationCancelled(fact) => rmp_serde::to_vec(fact),
        Event::OperationUncertain(fact) => rmp_serde::to_vec(fact),
    }
    .map_err(|e| StoreError::EventDecode(e.to_string()))?;
    Ok(bytes)
}

fn decode_stored_event(
    event_type: &str,
    schema_version: i64,
    payload: &[u8],
) -> Result<Event, StoreError> {
    if schema_version != i64::from(EVENT_SCHEMA_VERSION) {
        return Err(StoreError::CodecMismatch {
            detail: format!("schema_version column {schema_version} != {EVENT_SCHEMA_VERSION}"),
        });
    }

    let event = match event_type {
        "task.created" => {
            let p: TaskCreatedPayload = unpack(payload)?;
            p.task
                .validate_for_create()
                .map_err(|e| StoreError::EventDecode(e.to_string()))?;
            Event::TaskCreated {
                task: p.task,
                connectivity: p.connectivity,
                attention: p.attention,
                activity: p.activity,
                review_readiness: p.review_readiness,
            }
        }
        "task.renamed" => {
            let p: TaskRenamedPayload = unpack(payload)?;
            let title = crate::domain::task::TaskFacts::canonicalize_title(p.title)
                .map_err(|e| StoreError::EventDecode(e.to_string()))?;
            Event::TaskRenamed { title }
        }
        "task.attention_set" => {
            let p: TaskAttentionSetPayload = unpack(payload)?;
            Event::TaskAttentionSet {
                attention: p.attention,
            }
        }
        "task.close_begun" => {
            let p: TaskCloseBegunPayload = unpack(payload)?;
            Event::TaskCloseBegun {
                action_epoch: p.action_epoch,
            }
        }
        "task.reopened" => {
            let _: TaskUnitPayload = unpack(payload)?;
            Event::TaskReopened
        }
        "task.archived" => {
            let _: TaskUnitPayload = unpack(payload)?;
            Event::TaskArchived
        }
        "agent_session.registered" => {
            let p: AgentSessionRegisteredPayload = unpack(payload)?;
            p.agent
                .validate_for_registration()
                .map_err(|e| StoreError::EventDecode(e.to_string()))?;
            Event::AgentSessionRegistered { agent: p.agent }
        }
        "primary_agent.set" => {
            let p: PrimaryAgentSetPayload = unpack(payload)?;
            Event::PrimaryAgentSet {
                agent_session_id: p.agent_session_id,
            }
        }
        "artifact.registered" => {
            let p: ArtifactRegisteredPayload = unpack(payload)?;
            p.artifact
                .validate()
                .map_err(|e| StoreError::EventDecode(e.to_string()))?;
            Event::ArtifactRegistered {
                artifact: p.artifact,
            }
        }
        "resource.registered" => {
            let p: ResourceRegisteredPayload = unpack(payload)?;
            p.resource
                .validate_for_registration()
                .map_err(|e| StoreError::EventDecode(e.to_string()))?;
            Event::ResourceRegistered {
                resource: p.resource,
            }
        }
        "resource.release_begun" => {
            let p: ResourceReleaseBegunPayload = unpack(payload)?;
            Event::ResourceReleaseBegun {
                resource_id: p.resource_id,
                runtime_generation: p.runtime_generation,
            }
        }
        "resource.released" => {
            let p: ResourceReleasedPayload = unpack(payload)?;
            Event::ResourceReleased {
                resource_id: p.resource_id,
                runtime_generation: p.runtime_generation,
            }
        }
        "operation.accepted" => {
            let fact: OperationAcceptedFact = unpack(payload)?;
            Event::OperationAccepted(fact)
        }
        "operation.settled" => {
            let fact: OperationSettledFact = unpack(payload)?;
            Event::OperationSettled(fact)
        }
        "operation.failed" => {
            let fact: OperationFailedFact = unpack(payload)?;
            Event::OperationFailed(fact)
        }
        "operation.cancelled" => {
            let fact: OperationCancelledFact = unpack(payload)?;
            Event::OperationCancelled(fact)
        }
        "operation.uncertain" => {
            let fact: OperationUncertainFact = unpack(payload)?;
            Event::OperationUncertain(fact)
        }
        other => {
            return Err(StoreError::CodecMismatch {
                detail: format!("unknown event_type column '{other}'"),
            })
        }
    };

    if event.event_type() != event_type {
        return Err(StoreError::CodecMismatch {
            detail: format!(
                "decoded type '{}' disagrees with column '{}'",
                event.event_type(),
                event_type
            ),
        });
    }
    Ok(event)
}

fn unpack<T: serde::de::DeserializeOwned>(payload: &[u8]) -> Result<T, StoreError> {
    rmp_serde::from_slice(payload).map_err(|err| StoreError::CodecMismatch {
        detail: err.to_string(),
    })
}
