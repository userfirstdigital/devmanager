use rusqlite::{Connection, OptionalExtension};
use sha2::{Digest, Sha256};

use crate::kernel::StoreError;

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

const V3_SQL: &str = "\
CREATE TABLE event_retention (\n\
  singleton_key INTEGER PRIMARY KEY CHECK(singleton_key = 1),\n\
  pruned_through_sequence INTEGER NOT NULL CHECK(pruned_through_sequence >= 0)\n\
);\n\
INSERT INTO event_retention(singleton_key, pruned_through_sequence)\n\
VALUES (1, 0);\n\
";

const V4_SQL: &str = "\
ALTER TABLE outbox ADD COLUMN compacted_payload_sha256 BLOB CHECK(compacted_payload_sha256 IS NULL OR length(compacted_payload_sha256) = 32);\n\
CREATE INDEX idx_outbox_cleanup_ready ON outbox(event_sequence, effect_index, outbox_id)\n\
WHERE state IN ('settled', 'failed', 'cancelled') AND compacted_payload_sha256 IS NULL AND length(payload) > 0;\n\
";

const V5_SQL: &str = "\
CREATE TABLE host_admission (\n\
  singleton_key INTEGER PRIMARY KEY CHECK(singleton_key = 1),\n\
  operation_id BLOB NOT NULL CHECK(length(operation_id) = 16),\n\
  action_epoch INTEGER NOT NULL CHECK(action_epoch >= 0),\n\
  inspection_id INTEGER NOT NULL CHECK(inspection_id >= 0),\n\
  updated_at_ms INTEGER NOT NULL\n\
);\n\
";

const V6_SQL: &str = "\
CREATE TABLE host_cleanup_branches (\n\
  operation_id BLOB NOT NULL CHECK(length(operation_id) = 16),\n\
  branch TEXT NOT NULL CHECK(branch IN ('agent_sessions', 'resources', 'outstanding_effects', 'task_teardowns')),\n\
  result TEXT NOT NULL CHECK(result IN ('succeeded', 'failed')),\n\
  remaining_count INTEGER NOT NULL CHECK(remaining_count >= 0),\n\
  completed_at_ms INTEGER NOT NULL,\n\
  PRIMARY KEY (operation_id, branch),\n\
  CHECK(\n\
    (result = 'succeeded' AND remaining_count = 0)\n\
    OR (result = 'failed' AND remaining_count > 0)\n\
  )\n\
);\n\
";

const V7_SQL: &str = "\
CREATE TABLE semantic_journal_sessions (\n\
  authority_digest BLOB PRIMARY KEY CHECK(length(authority_digest) = 32),\n\
  provider_kind TEXT NOT NULL,\n\
  task_id BLOB NOT NULL CHECK(length(task_id) = 16),\n\
  agent_session_id BLOB NOT NULL CHECK(length(agent_session_id) = 16),\n\
  resource_id BLOB NOT NULL CHECK(length(resource_id) = 16),\n\
  runtime_generation INTEGER NOT NULL CHECK(runtime_generation >= 0),\n\
  action_epoch INTEGER NOT NULL CHECK(action_epoch >= 0),\n\
  managed_root BLOB NOT NULL CHECK(length(managed_root) = 32),\n\
  opened_at_ms INTEGER NOT NULL\n\
);\n\
CREATE TABLE semantic_journal_facts (\n\
  authority_digest BLOB NOT NULL REFERENCES semantic_journal_sessions(authority_digest),\n\
  sequence INTEGER NOT NULL CHECK(sequence > 0),\n\
  event_id BLOB NOT NULL CHECK(length(event_id) = 16),\n\
  delivery_id TEXT NOT NULL,\n\
  provider_event_id TEXT,\n\
  content_hash BLOB NOT NULL CHECK(length(content_hash) = 32),\n\
  kind TEXT NOT NULL,\n\
  visibility TEXT NOT NULL,\n\
  privacy_class TEXT NOT NULL,\n\
  redaction_class TEXT NOT NULL,\n\
  occurred_at_ms INTEGER NOT NULL,\n\
  ingested_at_ms INTEGER NOT NULL,\n\
  schema_version INTEGER NOT NULL,\n\
  payload BLOB NOT NULL,\n\
  PRIMARY KEY (authority_digest, sequence),\n\
  UNIQUE (authority_digest, event_id),\n\
  UNIQUE (authority_digest, delivery_id),\n\
  UNIQUE (authority_digest, provider_event_id)\n\
);\n\
CREATE INDEX idx_semantic_journal_facts_sequence\n\
  ON semantic_journal_facts(authority_digest, sequence);\n\
";

const V8_SQL: &str = "\
ALTER TABLE command_receipts ADD COLUMN payload_digest BLOB CHECK(payload_digest IS NULL OR length(payload_digest) = 32);\n\
CREATE TABLE provider_input_state (\n\
  agent_session_id BLOB PRIMARY KEY CHECK(length(agent_session_id) = 16),\n\
  task_id BLOB NOT NULL CHECK(length(task_id) = 16) REFERENCES tasks(task_id),\n\
  state BLOB NOT NULL\n\
);\n\
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

/// Compiled SHA-256 of [`V3_SQL`]. Do not change V3_SQL without updating this literal.
pub(crate) const V3_SHA256: [u8; 32] = [
    0x0e, 0xb7, 0x70, 0x7c, 0x6e, 0x72, 0x17, 0xfa, 0xdd, 0xd0, 0x9a, 0x06, 0x05, 0x7f, 0xa8, 0x0e,
    0x7b, 0x9e, 0x80, 0x2b, 0x73, 0xc5, 0xf1, 0x95, 0x8f, 0xca, 0xf1, 0x5f, 0x10, 0xeb, 0xa8, 0xe5,
];

/// Compiled SHA-256 of [`V4_SQL`]. Do not change V4_SQL without updating this literal.
pub(crate) const V4_SHA256: [u8; 32] = [
    0xb4, 0x38, 0x36, 0x29, 0x1f, 0xac, 0x5e, 0xd0, 0x21, 0x19, 0x92, 0x39, 0xa4, 0x73, 0x76, 0x87,
    0xf4, 0x9b, 0x55, 0xeb, 0x20, 0x05, 0x08, 0x85, 0xb4, 0x44, 0xa2, 0xa6, 0xc0, 0x89, 0x18, 0xf3,
];

/// Compiled SHA-256 of [`V5_SQL`]. Do not change V5_SQL without updating this literal.
pub(crate) const V5_SHA256: [u8; 32] = [
    0x17, 0x20, 0x21, 0x30, 0x94, 0xca, 0xfb, 0xc8, 0x01, 0x9b, 0x4b, 0x84, 0xb2, 0x58, 0x28, 0xc4,
    0xff, 0xcf, 0x57, 0x33, 0x45, 0x0c, 0x54, 0x58, 0xcb, 0xaa, 0xd0, 0xe8, 0xfc, 0x9c, 0x82, 0x20,
];

/// Compiled SHA-256 of [`V6_SQL`]. Do not change V6_SQL without updating this literal.
pub(crate) const V6_SHA256: [u8; 32] = [
    0x11, 0xce, 0x61, 0x1c, 0xf9, 0xc1, 0x3e, 0xcd, 0xd8, 0x44, 0x21, 0xd5, 0x0d, 0xc7, 0xa7, 0x71,
    0x0b, 0x27, 0x47, 0x02, 0x05, 0x31, 0xad, 0x6f, 0x68, 0x34, 0x16, 0x4c, 0xa0, 0xe6, 0x2e, 0x67,
];

/// Compiled SHA-256 of [`V7_SQL`]. Do not change V7_SQL without updating this literal.
pub(crate) const V7_SHA256: [u8; 32] = [
    0xe5, 0x15, 0x64, 0xcc, 0x51, 0x8f, 0x00, 0xa8, 0xb2, 0x40, 0x50, 0x6d, 0xda, 0x22, 0x3b, 0x96,
    0x4f, 0x6f, 0xd5, 0x9e, 0xa6, 0xde, 0xf9, 0xa6, 0x4a, 0x70, 0x26, 0x05, 0x23, 0x41, 0x0e, 0x2c,
];

/// Compiled SHA-256 of [`V8_SQL`]. The input-authority migration keeps the
/// journal migration immutable and follows it as a separate schema version.
pub(crate) const V8_SHA256: [u8; 32] = [
    0xd1, 0xd0, 0xd1, 0x80, 0xb6, 0x41, 0x1e, 0x65, 0xb8, 0x95, 0xe8, 0xaa, 0x5c, 0x12, 0x55, 0x9c,
    0x2f, 0x55, 0x02, 0x13, 0xbf, 0x59, 0x9c, 0x67, 0x80, 0x96, 0xb5, 0x6f, 0x26, 0x60, 0x45, 0x17,
];

/// Stable hex form of [`V1_SHA256`] for internal diagnostics.
pub(crate) const V1_SHA256_HEX: &str =
    "79f0a38f1092f770a884ef3a12848184f00e7741270ffb07b0de823263e2521f";

/// Digest of the compiled `&str` bytes. Callers must not hash checkout-file
/// bytes: CRLF vs LF on disk must not change an applied migration digest.
/// [`verify_manifest`] rejects compiled SQL that contains CR so the digest
/// stays LF-canonical without rewriting existing V1–V7 SQL or hash literals.
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
            assert_eq!(
                V3_SHA256,
                sha256_bytes(V3_SQL),
                "V3_SHA256 literal must match V3_SQL bytes"
            );
            assert_eq!(
                V4_SHA256,
                sha256_bytes(V4_SQL),
                "V4_SHA256 literal must match V4_SQL bytes"
            );
            assert_eq!(
                V5_SHA256,
                sha256_bytes(V5_SQL),
                "V5_SHA256 literal must match V5_SQL bytes"
            );
            assert_eq!(
                V6_SHA256,
                sha256_bytes(V6_SQL),
                "V6_SHA256 literal must match V6_SQL bytes"
            );
            assert_eq!(
                V7_SHA256,
                sha256_bytes(V7_SQL),
                "V7_SHA256 literal must match V7_SQL bytes"
            );
            assert_eq!(
                V8_SHA256,
                sha256_bytes(V8_SQL),
                "V8_SHA256 literal must match V8_SQL bytes"
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
                Migration {
                    version: 3,
                    name: "v3_event_retention_boundary",
                    sql: V3_SQL,
                    sha256: V3_SHA256,
                },
                Migration {
                    version: 4,
                    name: "v4_terminal_outbox_payload_compaction",
                    sql: V4_SQL,
                    sha256: V4_SHA256,
                },
                Migration {
                    version: 5,
                    name: "v5_host_admission",
                    sql: V5_SQL,
                    sha256: V5_SHA256,
                },
                Migration {
                    version: 6,
                    name: "v6_host_cleanup_branches",
                    sql: V6_SQL,
                    sha256: V6_SHA256,
                },
                Migration {
                    version: 7,
                    name: "v7_semantic_journal",
                    sql: V7_SQL,
                    sha256: V7_SHA256,
                },
                Migration {
                    version: 8,
                    name: "v8_provider_input_authority",
                    sql: V8_SQL,
                    sha256: V8_SHA256,
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
        assert!(
            !migration.sql.as_bytes().contains(&b'\r'),
            "migration {} SQL must be LF-canonical compiled bytes",
            migration.name
        );
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

/// Validate the complete V7 journal contract after migration history says it
/// is installed. Object counts are insufficient: a table with a dropped
/// column, weakened check, foreign key, or unique index can still have the
/// expected object count while accepting unsafe state.
pub(crate) fn validate_semantic_journal_schema(conn: &Connection) -> Result<(), StoreError> {
    const SESSIONS: &[(&str, &str, i64, i64)] = &[
        ("authority_digest", "BLOB", 0, 1),
        ("provider_kind", "TEXT", 1, 0),
        ("task_id", "BLOB", 1, 0),
        ("agent_session_id", "BLOB", 1, 0),
        ("resource_id", "BLOB", 1, 0),
        ("runtime_generation", "INTEGER", 1, 0),
        ("action_epoch", "INTEGER", 1, 0),
        ("managed_root", "BLOB", 1, 0),
        ("opened_at_ms", "INTEGER", 1, 0),
    ];
    const FACTS: &[(&str, &str, i64, i64)] = &[
        ("authority_digest", "BLOB", 1, 1),
        ("sequence", "INTEGER", 1, 2),
        ("event_id", "BLOB", 1, 0),
        ("delivery_id", "TEXT", 1, 0),
        ("provider_event_id", "TEXT", 0, 0),
        ("content_hash", "BLOB", 1, 0),
        ("kind", "TEXT", 1, 0),
        ("visibility", "TEXT", 1, 0),
        ("privacy_class", "TEXT", 1, 0),
        ("redaction_class", "TEXT", 1, 0),
        ("occurred_at_ms", "INTEGER", 1, 0),
        ("ingested_at_ms", "INTEGER", 1, 0),
        ("schema_version", "INTEGER", 1, 0),
        ("payload", "BLOB", 1, 0),
    ];
    validate_table_info(conn, "semantic_journal_sessions", SESSIONS)?;
    validate_table_info(conn, "semantic_journal_facts", FACTS)?;

    let session_sql = table_sql(conn, "semantic_journal_sessions")?;
    require_sql_fragments(
        &session_sql,
        &[
            "primarykey",
            "check(length(authority_digest)=32)",
            "check(length(task_id)=16)",
            "check(length(agent_session_id)=16)",
            "check(length(resource_id)=16)",
            "check(runtime_generation>=0)",
            "check(action_epoch>=0)",
            "check(length(managed_root)=32)",
        ],
    )?;
    let facts_sql = table_sql(conn, "semantic_journal_facts")?;
    require_sql_fragments(
        &facts_sql,
        &[
            "referencessemantic_journal_sessions(authority_digest)",
            "check(sequence>0)",
            "check(length(event_id)=16)",
            "check(length(content_hash)=32)",
            "primarykey(authority_digest,sequence)",
            "unique(authority_digest,event_id)",
            "unique(authority_digest,delivery_id)",
            "unique(authority_digest,provider_event_id)",
        ],
    )?;

    let foreign_key: Option<(String, String, String)> = conn
        .query_row(
            "PRAGMA foreign_key_list('semantic_journal_facts')",
            [],
            |row| Ok((row.get(2)?, row.get(3)?, row.get(4)?)),
        )
        .optional()
        .map_err(StoreError::from)?;
    if foreign_key.as_ref().map(|(table, from, to)| {
        table == "semantic_journal_sessions"
            && from == "authority_digest"
            && to == "authority_digest"
    }) != Some(true)
    {
        return Err(StoreError::MigrationInterrupted);
    }

    let mut indexes = conn
        .prepare("PRAGMA index_list('semantic_journal_facts')")
        .map_err(StoreError::from)?;
    let mut rows = indexes.query([]).map_err(StoreError::from)?;
    let mut sequence_index_found = false;
    while let Some(row) = rows.next().map_err(StoreError::from)? {
        let name: String = row.get(1).map_err(StoreError::from)?;
        if name == "idx_semantic_journal_facts_sequence" {
            sequence_index_found = true;
            break;
        }
    }
    if !sequence_index_found {
        return Err(StoreError::MigrationInterrupted);
    }
    let mut index_columns = conn
        .prepare("PRAGMA index_info('idx_semantic_journal_facts_sequence')")
        .map_err(StoreError::from)?;
    let mut index_rows = index_columns.query([]).map_err(StoreError::from)?;
    let mut columns = Vec::new();
    while let Some(row) = index_rows.next().map_err(StoreError::from)? {
        columns.push(row.get::<_, String>(2).map_err(StoreError::from)?);
    }
    if columns != ["authority_digest", "sequence"] {
        return Err(StoreError::MigrationInterrupted);
    }
    Ok(())
}

fn validate_table_info(
    conn: &Connection,
    table: &str,
    expected: &[(&str, &str, i64, i64)],
) -> Result<(), StoreError> {
    let pragma = format!("PRAGMA table_info('{table}')");
    let mut statement = conn.prepare(&pragma).map_err(StoreError::from)?;
    let mut rows = statement.query([]).map_err(StoreError::from)?;
    let mut actual = Vec::new();
    while let Some(row) = rows.next().map_err(StoreError::from)? {
        actual.push((
            row.get::<_, String>(1).map_err(StoreError::from)?,
            row.get::<_, String>(2).map_err(StoreError::from)?,
            row.get::<_, i64>(3).map_err(StoreError::from)?,
            row.get::<_, i64>(5).map_err(StoreError::from)?,
        ));
    }
    if actual.len() != expected.len()
        || actual.iter().zip(expected).any(|(actual, expected)| {
            actual.0 != expected.0
                || actual.1 != expected.1
                || actual.2 != expected.2
                || actual.3 != expected.3
        })
    {
        return Err(StoreError::MigrationInterrupted);
    }
    Ok(())
}

fn table_sql(conn: &Connection, table: &str) -> Result<String, StoreError> {
    let sql: Option<String> = conn
        .query_row(
            "SELECT sql FROM sqlite_schema WHERE type = 'table' AND name = ?1",
            [table],
            |row| row.get(0),
        )
        .optional()
        .map_err(StoreError::from)?;
    let Some(sql) = sql else {
        return Err(StoreError::MigrationInterrupted);
    };
    Ok(sql
        .to_ascii_lowercase()
        .split_whitespace()
        .collect::<String>())
}

fn require_sql_fragments(sql: &str, fragments: &[&str]) -> Result<(), StoreError> {
    if fragments.iter().any(|fragment| !sql.contains(*fragment)) {
        return Err(StoreError::MigrationInterrupted);
    }
    Ok(())
}

pub(crate) const PROJECTION_TABLES: &[&str] = &[
    "tasks",
    "operations",
    "agent_sessions",
    "artifacts",
    "resources",
    "host_admission",
    "host_cleanup_branches",
    "provider_input_state",
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernel::StoreError;
    use rusqlite::Connection;
    use tempfile::TempDir;

    #[test]
    fn v7_manifest_rejects_missing_semantic_sequence_index() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("manifest.sqlite3");
        let store = crate::kernel::KernelStore::open(&path).expect("initial open");
        drop(store);
        let conn = Connection::open(&path).expect("raw reopen");
        conn.execute("DROP INDEX idx_semantic_journal_facts_sequence", [])
            .expect("drop index");
        drop(conn);
        assert!(matches!(
            crate::kernel::KernelStore::open(&path),
            Err(StoreError::MigrationInterrupted)
        ));
    }

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
    fn schema_v2_upgrades_to_v3_with_explicit_zero_prune_boundary() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("v2.sqlite3");
        {
            let conn = Connection::open(&path).expect("open");
            conn.execute_batch(V1_SQL).expect("apply v1");
            conn.execute_batch(V2_SQL).expect("apply v2");
            conn.execute(
                "INSERT INTO schema_migrations(version, name, applied_at_ms, sha256)
                 VALUES (1, 'v1_initial', 1, ?1),
                        (2, 'v2_outbox_dispatch_fence', 2, ?2)",
                rusqlite::params![V1_SHA256.as_slice(), V2_SHA256.as_slice()],
            )
            .expect("record v1 and v2");
        }

        let store = crate::kernel::KernelStore::open(&path).expect("upgrade open");
        drop(store);

        let conn = Connection::open(&path).expect("reopen raw");
        let retention: (i64, i64) = conn
            .query_row(
                "SELECT singleton_key, pruned_through_sequence
                 FROM event_retention WHERE singleton_key = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("V3 retention singleton");
        assert_eq!(retention, (1, 0));

        let history: Vec<(i64, String, Vec<u8>)> = {
            let mut stmt = conn
                .prepare("SELECT version, name, sha256 FROM schema_migrations ORDER BY version")
                .unwrap();
            stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
                .unwrap()
                .map(|row| row.unwrap())
                .collect()
        };
        assert_eq!(history.len(), 8);
        assert_eq!(history[0], (1, "v1_initial".into(), V1_SHA256.to_vec()));
        assert_eq!(
            history[1],
            (2, "v2_outbox_dispatch_fence".into(), V2_SHA256.to_vec())
        );
        assert_eq!(history[2].0, 3);
        assert_eq!(history[2].1, "v3_event_retention_boundary");
        assert_eq!(history[2].2, V3_SHA256.to_vec());
        assert_eq!(history[3].0, 4);
        assert_eq!(history[3].1, "v4_terminal_outbox_payload_compaction");
        assert_eq!(history[3].2, V4_SHA256.to_vec());
        assert_eq!(history[4].0, 5);
        assert_eq!(history[4].1, "v5_host_admission");
        assert_eq!(history[4].2, V5_SHA256.to_vec());
        assert_eq!(history[5].0, 6);
        assert_eq!(history[5].1, "v6_host_cleanup_branches");
        assert_eq!(history[5].2, V6_SHA256.to_vec());
        assert_eq!(history[6].0, 7);
        assert_eq!(history[6].1, "v7_semantic_journal");
        assert_eq!(history[6].2, V7_SHA256.to_vec());
        assert_eq!(history[7].0, 8);
        assert_eq!(history[7].1, "v8_provider_input_authority");
        assert_eq!(history[7].2, V8_SHA256.to_vec());

        let compacted_column: (String, i64) = conn
            .query_row(
                "SELECT type, \"notnull\" FROM pragma_table_info('outbox')
                 WHERE name = 'compacted_payload_sha256'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("V4 compacted payload digest column");
        assert_eq!(compacted_column, ("BLOB".into(), 0));
        let cleanup_index: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_schema
                 WHERE type = 'index' AND name = 'idx_outbox_cleanup_ready'",
                [],
                |row| row.get(0),
            )
            .expect("V4 cleanup index");
        assert_eq!(cleanup_index, 1);
    }

    #[test]
    fn schema_v3_upgrades_to_v4_without_rewriting_existing_payloads() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("v3.sqlite3");
        let original_payload = vec![0x91, 0x01, 0x02, 0x03];
        {
            let conn = Connection::open(&path).expect("open");
            conn.execute_batch(V1_SQL).expect("apply v1");
            conn.execute_batch(V2_SQL).expect("apply v2");
            conn.execute_batch(V3_SQL).expect("apply v3");
            conn.execute(
                "INSERT INTO schema_migrations(version, name, applied_at_ms, sha256)
                 VALUES (1, 'v1_initial', 1, ?1),
                        (2, 'v2_outbox_dispatch_fence', 2, ?2),
                        (3, 'v3_event_retention_boundary', 3, ?3)",
                rusqlite::params![
                    V1_SHA256.as_slice(),
                    V2_SHA256.as_slice(),
                    V3_SHA256.as_slice(),
                ],
            )
            .expect("record v1 through v3");
            conn.pragma_update(None, "foreign_keys", false)
                .expect("disable foreign keys for isolated legacy-row fixture");
            conn.execute(
                "INSERT INTO outbox(
                    outbox_id, operation_id, effect_index, event_sequence, destination_class,
                    replay_policy, payload, state, available_at_ms, leased_until_ms,
                    dispatch_started_at_ms, attempts, last_error_class, lease_generation,
                    reconciliation_receipt
                 ) VALUES (?1, ?2, 0, 7, 'task_teardown', 'retry_safe', ?3,
                           'settled', 11, NULL, 11, 1, NULL, 1, NULL)",
                rusqlite::params![&[0x31u8; 16], &[0x32u8; 16], &original_payload],
            )
            .expect("seed existing V3 outbox payload");
        }

        drop(crate::kernel::KernelStore::open(&path).expect("upgrade open"));

        let conn = Connection::open(&path).expect("reopen raw");
        let (payload, compacted_digest): (Vec<u8>, Option<Vec<u8>>) = conn
            .query_row(
                "SELECT payload, compacted_payload_sha256 FROM outbox",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("preserved V3 row");
        assert_eq!(payload, original_payload);
        assert!(compacted_digest.is_none());
        let v4: (String, Vec<u8>) = conn
            .query_row(
                "SELECT name, sha256 FROM schema_migrations WHERE version = 4",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("V4 migration record");
        assert_eq!(v4.0, "v4_terminal_outbox_payload_compaction");
        assert_eq!(v4.1, V4_SHA256.to_vec());
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

    #[test]
    fn schema_v7_partial_tables_fail_closed_even_when_history_claims_v7() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("v7-partial.sqlite3");
        drop(crate::kernel::KernelStore::open(&path).expect("open"));

        {
            let conn = Connection::open(&path).expect("reopen raw");
            conn.execute("DROP TABLE semantic_journal_facts", [])
                .expect("remove one V7 table");
        }

        let error = crate::kernel::KernelStore::open(&path).expect_err("partial V7 schema");
        assert!(
            matches!(
                error,
                StoreError::MigrationInterrupted | StoreError::Corruption
            ),
            "partial V7 schema must fail closed, got {error:?}"
        );
    }
}
