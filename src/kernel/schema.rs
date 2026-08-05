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

/// Compiled SHA-256 of [`V1_SQL`]. Do not change V1_SQL without updating this literal.
pub(crate) const V1_SHA256: [u8; 32] = [
    0x79, 0xf0, 0xa3, 0x8f, 0x10, 0x92, 0xf7, 0x70, 0xa8, 0x84, 0xef, 0x3a, 0x12, 0x84, 0x81, 0x84,
    0xf0, 0x0e, 0x77, 0x41, 0x27, 0x0f, 0xfb, 0x07, 0xb0, 0xde, 0x82, 0x32, 0x63, 0xe2, 0x52, 0x1f,
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
            let migrations = vec![Migration {
                version: 1,
                name: "v1_initial",
                sql: V1_SQL,
                sha256: V1_SHA256,
            }];
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
