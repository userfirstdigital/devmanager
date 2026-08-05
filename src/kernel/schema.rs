use sha2::{Digest, Sha256};

/// One immutable compiled migration entry.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Migration {
    pub version: i64,
    pub name: &'static str,
    pub sql: &'static str,
    pub sha256: [u8; 32],
}

const V1_SQL: &str = "\
CREATE TABLE schema_migrations (\n\
  version INTEGER PRIMARY KEY,\n\
  name TEXT NOT NULL UNIQUE,\n\
  applied_at_ms INTEGER NOT NULL,\n\
  sha256 BLOB NOT NULL CHECK(length(sha256) = 32)\n\
);\n\
CREATE TABLE events (\n\
  sequence INTEGER PRIMARY KEY AUTOINCREMENT,\n\
  event_id BLOB NOT NULL UNIQUE CHECK(length(event_id) = 16),\n\
  task_id BLOB CHECK(task_id IS NULL OR length(task_id) = 16),\n\
  task_revision INTEGER,\n\
  event_type TEXT NOT NULL,\n\
  schema_version INTEGER NOT NULL,\n\
  occurred_at_ms INTEGER NOT NULL,\n\
  payload BLOB NOT NULL\n\
);\n\
CREATE TABLE command_receipts (\n\
  command_id BLOB PRIMARY KEY CHECK(length(command_id) = 16),\n\
  client_id BLOB NOT NULL CHECK(length(client_id) = 16),\n\
  task_id BLOB CHECK(task_id IS NULL OR length(task_id) = 16),\n\
  receipt BLOB NOT NULL,\n\
  committed_sequence INTEGER,\n\
  created_at_ms INTEGER NOT NULL\n\
);\n\
CREATE TABLE operations (\n\
  operation_id BLOB PRIMARY KEY CHECK(length(operation_id) = 16),\n\
  command_id BLOB NOT NULL UNIQUE REFERENCES command_receipts(command_id) DEFERRABLE INITIALLY DEFERRED,\n\
  task_id BLOB CHECK(task_id IS NULL OR length(task_id) = 16),\n\
  resource_id BLOB CHECK(resource_id IS NULL OR length(resource_id) = 16),\n\
  action_epoch INTEGER,\n\
  runtime_generation INTEGER,\n\
  state TEXT NOT NULL,\n\
  result BLOB,\n\
  outcome_code TEXT,\n\
  accepted_at_ms INTEGER NOT NULL,\n\
  outcome_at_ms INTEGER\n\
);\n\
CREATE TABLE tasks (\n\
  task_id BLOB PRIMARY KEY CHECK(length(task_id) = 16),\n\
  environment_id BLOB NOT NULL CHECK(length(environment_id) = 16),\n\
  project_id BLOB NOT NULL CHECK(length(project_id) = 16),\n\
  title TEXT NOT NULL,\n\
  description TEXT,\n\
  workspace BLOB NOT NULL,\n\
  assignment BLOB NOT NULL,\n\
  lifecycle TEXT NOT NULL,\n\
  action_epoch INTEGER NOT NULL,\n\
  revision INTEGER NOT NULL,\n\
  connectivity TEXT NOT NULL,\n\
  attention TEXT NOT NULL,\n\
  activity TEXT NOT NULL,\n\
  review_readiness TEXT NOT NULL,\n\
  primary_agent_session_id BLOB CHECK(primary_agent_session_id IS NULL OR length(primary_agent_session_id) = 16),\n\
  created_at_ms INTEGER NOT NULL,\n\
  updated_at_ms INTEGER NOT NULL\n\
);\n\
CREATE TABLE agent_sessions (\n\
  agent_session_id BLOB PRIMARY KEY CHECK(length(agent_session_id) = 16),\n\
  task_id BLOB NOT NULL REFERENCES tasks(task_id),\n\
  role BLOB NOT NULL,\n\
  provider_kind TEXT NOT NULL,\n\
  provider_session_id TEXT,\n\
  lifecycle TEXT NOT NULL,\n\
  runtime_generation INTEGER NOT NULL,\n\
  revision INTEGER NOT NULL\n\
);\n\
CREATE TABLE artifacts (\n\
  artifact_id BLOB PRIMARY KEY CHECK(length(artifact_id) = 16),\n\
  task_id BLOB NOT NULL REFERENCES tasks(task_id),\n\
  kind TEXT NOT NULL,\n\
  label TEXT NOT NULL,\n\
  content_ref BLOB NOT NULL,\n\
  sha256 BLOB NOT NULL CHECK(length(sha256) = 32),\n\
  privacy_class TEXT NOT NULL,\n\
  created_at_ms INTEGER NOT NULL\n\
);\n\
CREATE TABLE resources (\n\
  resource_id BLOB PRIMARY KEY CHECK(length(resource_id) = 16),\n\
  task_id BLOB REFERENCES tasks(task_id) CHECK(task_id IS NULL OR length(task_id) = 16),\n\
  owner_kind TEXT NOT NULL,\n\
  resource_kind TEXT NOT NULL,\n\
  recipe BLOB NOT NULL,\n\
  lifecycle TEXT NOT NULL,\n\
  runtime_generation INTEGER NOT NULL,\n\
  updated_at_ms INTEGER NOT NULL\n\
);\n\
CREATE TABLE outbox (\n\
  outbox_id BLOB PRIMARY KEY CHECK(length(outbox_id) = 16),\n\
  operation_id BLOB NOT NULL REFERENCES operations(operation_id) DEFERRABLE INITIALLY DEFERRED,\n\
  effect_index INTEGER NOT NULL CHECK(effect_index >= 0),\n\
  event_sequence INTEGER NOT NULL REFERENCES events(sequence),\n\
  destination_class TEXT NOT NULL,\n\
  replay_policy TEXT NOT NULL,\n\
  payload BLOB NOT NULL,\n\
  state TEXT NOT NULL,\n\
  available_at_ms INTEGER NOT NULL,\n\
  leased_until_ms INTEGER,\n\
  dispatch_started_at_ms INTEGER,\n\
  attempts INTEGER NOT NULL DEFAULT 0,\n\
  last_error_class TEXT,\n\
  UNIQUE(operation_id, effect_index)\n\
);\n\
CREATE INDEX idx_events_task_sequence ON events(task_id, sequence);\n\
CREATE INDEX idx_events_task_revision ON events(task_id, task_revision);\n\
CREATE INDEX idx_operations_state ON operations(state);\n\
CREATE INDEX idx_outbox_delivery_state ON outbox(state, available_at_ms);\n\
CREATE INDEX idx_resources_active ON resources(task_id, resource_kind) WHERE lifecycle = 'active';\n\
";

const V2_SQL: &str = "\
ALTER TABLE outbox ADD COLUMN lease_generation INTEGER NOT NULL DEFAULT 0 CHECK(lease_generation >= 0);\n\
ALTER TABLE outbox ADD COLUMN reconciliation_receipt BLOB;\n\
CREATE INDEX idx_outbox_claim_ready ON outbox(state, available_at_ms, leased_until_ms);\n\
";

/// Compiled SHA-256 of [`V1_SQL`]. Do not change V1_SQL without updating this literal.
pub(crate) const V1_SHA256: [u8; 32] = [
    0x79, 0xf0, 0xa3, 0x8f, 0x10, 0x92, 0xf7, 0x70, 0xa8, 0x84, 0xef, 0x3a, 0x12, 0x84, 0x81, 0x84,
    0xf0, 0x0e, 0x77, 0x41, 0x27, 0x0f, 0xfb, 0x07, 0xb0, 0xde, 0x82, 0x32, 0x63, 0xe2, 0x52, 0x1f,
];

/// Compiled SHA-256 of [`V2_SQL`]. Do not change V2_SQL without updating this literal.
pub(crate) const V2_SHA256: [u8; 32] = [
    0xa7, 0x80, 0x18, 0xb1, 0x02, 0x8f, 0xca, 0x90, 0x82, 0x46, 0x57, 0x61, 0x7f, 0xce, 0x53, 0x66,
    0x94, 0x31, 0x2f, 0x51, 0x27, 0x4a, 0x84, 0x98, 0x24, 0xa2, 0x0a, 0x01, 0xa1, 0x46, 0x78, 0xcb,
];

/// Stable hex form of [`V1_SHA256`] for internal diagnostics.
pub(crate) const V1_SHA256_HEX: &str =
    "79f0a38f1092f770a884ef3a12848184f00e7741270ffb07b0de823263e2521f";

fn sha256_bytes(input: &str) -> [u8; 32] {
    let digest = Sha256::digest(input.as_bytes());
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

/// Contiguous, exact migration manifest compiled into the binary.
pub(crate) fn migration_manifest() -> &'static [Migration] {
    // Lazily verify contiguity and literal hash once; panic on programmer error.
    static MANIFEST: std::sync::OnceLock<Vec<Migration>> = std::sync::OnceLock::new();
    MANIFEST
        .get_or_init(|| {
            assert_eq!(
                V1_SHA256,
                sha256_bytes(V1_SQL),
                "V1_SHA256 literal must match V1_SQL bytes"
            );
            assert_eq!(
                V1_SHA256_HEX,
                hex_lower(&V1_SHA256),
                "V1_SHA256_HEX must match V1_SHA256"
            );
            assert_eq!(
                V2_SHA256,
                sha256_bytes(V2_SQL),
                "V2_SHA256 literal must match V2_SQL bytes"
            );
            let migrations = vec![
                Migration {
                    version: 1,
                    name: "v1_initial",
                    sql: V1_SQL,
                    sha256: V1_SHA256,
                },
                Migration {
                    version: 2,
                    name: "v2_outbox_dispatch_fence",
                    sql: V2_SQL,
                    sha256: V2_SHA256,
                },
            ];
            verify_manifest(&migrations);
            migrations
        })
        .as_slice()
}

fn hex_lower(bytes: &[u8; 32]) -> String {
    let mut out = String::with_capacity(64);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(out, "{b:02x}");
    }
    out
}

fn verify_manifest(migrations: &[Migration]) {
    for (idx, migration) in migrations.iter().enumerate() {
        let expected = i64::try_from(idx + 1).expect("migration index fits i64");
        assert_eq!(
            migration.version, expected,
            "migration versions must be contiguous starting at 1"
        );
        assert_eq!(
            migration.sha256,
            sha256_bytes(migration.sql),
            "migration sha256 must match SQL for {}",
            migration.name
        );
        assert!(!migration.name.is_empty(), "migration name required");
        assert!(!migration.sql.is_empty(), "migration sql required");
        if idx > 0 {
            assert_eq!(
                migration.version,
                migrations[idx - 1].version + 1,
                "gapped migration manifest"
            );
        }
    }
}

pub(crate) fn latest_migration_version() -> i64 {
    migration_manifest().last().map(|m| m.version).unwrap_or(0)
}

pub(crate) const PROJECTION_TABLES: &[&str] = &[
    "tasks",
    "operations",
    "agent_sessions",
    "artifacts",
    "resources",
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernel::StoreError;
    use rusqlite::Connection;
    use tempfile::TempDir;

    #[test]
    fn schema_v1_upgrades_to_v2_without_rewriting_v1_record() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("v1.sqlite3");
        {
            let conn = Connection::open(&path).expect("open");
            conn.execute_batch(V1_SQL).expect("apply v1");
            conn.execute(
                "INSERT INTO schema_migrations(version, name, applied_at_ms, sha256)
                 VALUES (1, 'v1_initial', 1, ?1)",
                rusqlite::params![V1_SHA256.as_slice()],
            )
            .expect("record v1");
            conn.pragma_update(None, "foreign_keys", false)
                .expect("disable foreign keys for isolated legacy-row fixture");
            conn.execute(
                "INSERT INTO outbox(
                    outbox_id, operation_id, effect_index, event_sequence, destination_class,
                    replay_policy, payload, state, available_at_ms, leased_until_ms,
                    dispatch_started_at_ms, attempts, last_error_class
                 ) VALUES (?1, ?2, 0, 7, 'task_teardown', 'retry_safe', X'0102',
                           'pending', 11, NULL, NULL, 0, NULL)",
                rusqlite::params![&[0x11u8; 16], &[0x22u8; 16]],
            )
            .expect("seed existing v1 outbox row");
        }

        let store = crate::kernel::KernelStore::open(&path).expect("upgrade open");
        drop(store);

        let conn = Connection::open(&path).expect("reopen raw");
        let (v1_name, v1_applied_at, v1_sha): (String, i64, Vec<u8>) = conn
            .query_row(
                "SELECT name, applied_at_ms, sha256 FROM schema_migrations WHERE version = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(v1_name, "v1_initial");
        assert_eq!(v1_applied_at, 1);
        assert_eq!(v1_sha.as_slice(), V1_SHA256.as_slice());

        let (v2_name, v2_sha): (String, Vec<u8>) = conn
            .query_row(
                "SELECT name, sha256 FROM schema_migrations WHERE version = 2",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(v2_name, "v2_outbox_dispatch_fence");
        assert_eq!(v2_sha.as_slice(), V2_SHA256.as_slice());

        let cols: Vec<String> = {
            let mut stmt = conn.prepare("PRAGMA table_info(outbox)").unwrap();
            stmt.query_map([], |row| row.get::<_, String>(1))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
        };
        assert!(cols.iter().any(|c| c == "lease_generation"));
        assert!(cols.iter().any(|c| c == "reconciliation_receipt"));

        let existing: Vec<rusqlite::types::Value> = conn
            .query_row(
                "SELECT outbox_id, operation_id, effect_index, event_sequence,
                        destination_class, replay_policy, payload, state, available_at_ms,
                        leased_until_ms, dispatch_started_at_ms, attempts, last_error_class,
                        lease_generation, reconciliation_receipt
                 FROM outbox",
                [],
                |row| (0..15).map(|column| row.get(column)).collect(),
            )
            .unwrap();
        assert_eq!(
            existing,
            vec![
                rusqlite::types::Value::Blob(vec![0x11; 16]),
                rusqlite::types::Value::Blob(vec![0x22; 16]),
                rusqlite::types::Value::Integer(0),
                rusqlite::types::Value::Integer(7),
                rusqlite::types::Value::Text("task_teardown".into()),
                rusqlite::types::Value::Text("retry_safe".into()),
                rusqlite::types::Value::Blob(vec![0x01, 0x02]),
                rusqlite::types::Value::Text("pending".into()),
                rusqlite::types::Value::Integer(11),
                rusqlite::types::Value::Null,
                rusqlite::types::Value::Null,
                rusqlite::types::Value::Integer(0),
                rusqlite::types::Value::Null,
                rusqlite::types::Value::Integer(0),
                rusqlite::types::Value::Null,
            ],
            "V2 must preserve every V1 outbox column and initialize only its two additions",
        );

        let idx: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_schema
                 WHERE type = 'index' AND name = 'idx_outbox_claim_ready'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(idx, 1);
    }

    #[test]
    fn schema_v2_failure_rolls_back_columns_and_history() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("v1-conflict.sqlite3");
        {
            let conn = Connection::open(&path).expect("open");
            conn.execute_batch(V1_SQL).expect("apply v1");
            conn.execute(
                "INSERT INTO schema_migrations(version, name, applied_at_ms, sha256)
                 VALUES (1, 'v1_initial', 1, ?1)",
                rusqlite::params![V1_SHA256.as_slice()],
            )
            .expect("record v1");
            conn.execute("CREATE INDEX idx_outbox_claim_ready ON tasks(task_id)", [])
                .expect("reserve v2 index name");
        }

        let error = crate::kernel::KernelStore::open(&path).expect_err("v2 must fail");
        assert!(
            matches!(
                error,
                StoreError::MigrationInterrupted | StoreError::Sqlite(_)
            ),
            "unexpected migration failure: {error:?}"
        );

        let conn = Connection::open(&path).expect("reopen raw");
        let columns: Vec<String> = {
            let mut stmt = conn.prepare("PRAGMA table_info(outbox)").unwrap();
            stmt.query_map([], |row| row.get::<_, String>(1))
                .unwrap()
                .map(|row| row.unwrap())
                .collect()
        };
        assert!(!columns.iter().any(|column| column == "lease_generation"));
        assert!(!columns
            .iter()
            .any(|column| column == "reconciliation_receipt"));
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM schema_migrations", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap(),
            1
        );
    }
}
