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
CREATE TABLE saved_prompts (\n\
  prompt_id BLOB PRIMARY KEY CHECK(length(prompt_id) = 16),\n\
  title TEXT NOT NULL,\n\
  description TEXT,\n\
  current_version_id BLOB NOT NULL CHECK(length(current_version_id) = 16),\n\
  revision INTEGER NOT NULL CHECK(revision > 0),\n\
  created_at_ms INTEGER NOT NULL,\n\
  updated_at_ms INTEGER NOT NULL,\n\
  archived_at_ms INTEGER,\n\
  FOREIGN KEY(prompt_id, current_version_id)\n\
    REFERENCES prompt_versions(prompt_id, prompt_version_id)\n\
    DEFERRABLE INITIALLY DEFERRED\n\
);\n\
CREATE TABLE prompt_versions (\n\
  prompt_version_id BLOB PRIMARY KEY CHECK(length(prompt_version_id) = 16),\n\
  prompt_id BLOB NOT NULL REFERENCES saved_prompts(prompt_id)\n\
    DEFERRABLE INITIALLY DEFERRED,\n\
  version INTEGER NOT NULL CHECK(version > 0),\n\
  body TEXT NOT NULL,\n\
  body_sha256 BLOB NOT NULL CHECK(length(body_sha256) = 32),\n\
  created_at_ms INTEGER NOT NULL,\n\
  UNIQUE(prompt_id, version),\n\
  UNIQUE(prompt_id, prompt_version_id)\n\
);\n\
CREATE TABLE prompt_version_variables (\n\
  prompt_version_id BLOB NOT NULL REFERENCES prompt_versions(prompt_version_id),\n\
  variable TEXT NOT NULL CHECK(length(variable) > 0),\n\
  position INTEGER NOT NULL CHECK(position >= 0),\n\
  PRIMARY KEY(prompt_version_id, variable),\n\
  UNIQUE(prompt_version_id, position)\n\
);\n\
CREATE TRIGGER prompt_version_variables_immutable_update\n\
  BEFORE UPDATE ON prompt_version_variables\n\
BEGIN\n\
  SELECT RAISE(ABORT, 'prompt version variables are immutable');\n\
END;\n\
CREATE TRIGGER prompt_version_variables_immutable_delete\n\
  BEFORE DELETE ON prompt_version_variables\n\
BEGIN\n\
  SELECT RAISE(ABORT, 'prompt version variables are append-only');\n\
END;\n\
CREATE TRIGGER prompt_versions_immutable_update\n\
  BEFORE UPDATE ON prompt_versions\n\
BEGIN\n\
  SELECT RAISE(ABORT, 'prompt version history is immutable');\n\
END;\n\
CREATE TRIGGER prompt_versions_immutable_delete\n\
  BEFORE DELETE ON prompt_versions\n\
BEGIN\n\
  SELECT RAISE(ABORT, 'prompt version history is immutable');\n\
END;\n\
CREATE TABLE prompt_tags (\n\
  prompt_id BLOB NOT NULL REFERENCES saved_prompts(prompt_id),\n\
  tag TEXT NOT NULL CHECK(length(tag) > 0),\n\
  position INTEGER NOT NULL CHECK(position >= 0),\n\
  PRIMARY KEY(prompt_id, tag),\n\
  UNIQUE(prompt_id, position)\n\
);\n\
CREATE TABLE prompt_chains (\n\
  chain_id BLOB PRIMARY KEY CHECK(length(chain_id) = 16),\n\
  title TEXT NOT NULL,\n\
  description TEXT,\n\
  revision INTEGER NOT NULL CHECK(revision > 0),\n\
  created_at_ms INTEGER NOT NULL,\n\
  updated_at_ms INTEGER NOT NULL,\n\
  archived_at_ms INTEGER\n\
);\n\
CREATE TABLE prompt_chain_links (\n\
  link_id BLOB PRIMARY KEY CHECK(length(link_id) = 16),\n\
  chain_id BLOB NOT NULL REFERENCES prompt_chains(chain_id),\n\
  position INTEGER NOT NULL CHECK(position >= 0),\n\
  prompt_id BLOB NOT NULL CHECK(length(prompt_id) = 16),\n\
  prompt_version_id BLOB NOT NULL CHECK(length(prompt_version_id) = 16),\n\
  FOREIGN KEY(prompt_id, prompt_version_id)\n\
    REFERENCES prompt_versions(prompt_id, prompt_version_id),\n\
  UNIQUE(chain_id, position)\n\
);\n\
CREATE TABLE prompt_chain_command_receipts (\n\
  command_id BLOB PRIMARY KEY CHECK(length(command_id) = 16),\n\
  command_sha256 BLOB NOT NULL CHECK(length(command_sha256) = 32),\n\
  chain_id BLOB NOT NULL CHECK(length(chain_id) = 16),\n\
  chain_link_id BLOB CHECK(chain_link_id IS NULL OR length(chain_link_id) = 16),\n\
  revision INTEGER NOT NULL CHECK(revision > 0),\n\
  receipt BLOB NOT NULL,\n\
  created_at_ms INTEGER NOT NULL\n\
);\n\
CREATE TABLE prompt_chain_events (\n\
  sequence INTEGER PRIMARY KEY AUTOINCREMENT,\n\
  prompt_chain_event_id BLOB NOT NULL UNIQUE CHECK(length(prompt_chain_event_id) = 16),\n\
  command_id BLOB NOT NULL REFERENCES prompt_chain_command_receipts(command_id),\n\
  chain_id BLOB NOT NULL CHECK(length(chain_id) = 16),\n\
  event_type TEXT NOT NULL,\n\
  occurred_at_ms INTEGER NOT NULL,\n\
  payload BLOB NOT NULL\n\
);\n\
CREATE TABLE prompt_command_receipts (\n\
  command_id BLOB PRIMARY KEY CHECK(length(command_id) = 16),\n\
  command_sha256 BLOB NOT NULL CHECK(length(command_sha256) = 32),\n\
  prompt_id BLOB NOT NULL CHECK(length(prompt_id) = 16),\n\
  prompt_version_id BLOB NOT NULL CHECK(length(prompt_version_id) = 16),\n\
  revision INTEGER NOT NULL CHECK(revision > 0),\n\
  receipt BLOB NOT NULL,\n\
  created_at_ms INTEGER NOT NULL\n\
);\n\
CREATE TABLE prompt_events (\n\
  sequence INTEGER PRIMARY KEY AUTOINCREMENT,\n\
  prompt_event_id BLOB NOT NULL UNIQUE CHECK(length(prompt_event_id) = 16),\n\
  command_id BLOB NOT NULL REFERENCES prompt_command_receipts(command_id),\n\
  prompt_id BLOB NOT NULL CHECK(length(prompt_id) = 16),\n\
  event_type TEXT NOT NULL,\n\
  occurred_at_ms INTEGER NOT NULL,\n\
  payload BLOB NOT NULL\n\
);\n\
CREATE TRIGGER prompt_command_receipts_immutable_update\n\
  BEFORE UPDATE ON prompt_command_receipts\n\
BEGIN\n\
  SELECT RAISE(ABORT, 'prompt command receipts are immutable');\n\
END;\n\
CREATE TRIGGER prompt_command_receipts_immutable_delete\n\
  BEFORE DELETE ON prompt_command_receipts\n\
BEGIN\n\
  SELECT RAISE(ABORT, 'prompt command receipts are append-only');\n\
END;\n\
CREATE TRIGGER prompt_events_immutable_update\n\
  BEFORE UPDATE ON prompt_events\n\
BEGIN\n\
  SELECT RAISE(ABORT, 'prompt event provenance is immutable');\n\
END;\n\
CREATE TRIGGER prompt_events_immutable_delete\n\
  BEFORE DELETE ON prompt_events\n\
BEGIN\n\
  SELECT RAISE(ABORT, 'prompt event provenance is append-only');\n\
END;\n\
CREATE INDEX idx_prompt_versions_prompt_version\n\
  ON prompt_versions(prompt_id, version DESC);\n\
CREATE INDEX idx_prompt_version_variables_version_position\n\
  ON prompt_version_variables(prompt_version_id, position);\n\
CREATE INDEX idx_prompt_tags_prompt_position\n\
  ON prompt_tags(prompt_id, position);\n\
CREATE INDEX idx_prompt_chain_links_chain_position\n\
  ON prompt_chain_links(chain_id, position);\n\
CREATE INDEX idx_prompt_chain_events_chain_sequence\n\
  ON prompt_chain_events(chain_id, sequence);\n\
CREATE INDEX idx_prompt_events_prompt_sequence\n\
  ON prompt_events(prompt_id, sequence);\n\
";

const V8_SQL: &str = "\
ALTER TABLE prompt_versions ADD COLUMN variables_sealed INTEGER NOT NULL DEFAULT 1 CHECK(variables_sealed IN (0, 1));\n\
DROP TRIGGER prompt_versions_immutable_update;\n\
CREATE TRIGGER prompt_versions_immutable_update\n\
  BEFORE UPDATE ON prompt_versions\n\
  WHEN NOT (\n\
    OLD.variables_sealed = 0\n\
    AND NEW.variables_sealed = 1\n\
    AND NEW.prompt_version_id = OLD.prompt_version_id\n\
    AND NEW.prompt_id = OLD.prompt_id\n\
    AND NEW.version = OLD.version\n\
    AND NEW.body = OLD.body\n\
    AND NEW.body_sha256 = OLD.body_sha256\n\
    AND NEW.created_at_ms = OLD.created_at_ms\n\
  )\n\
BEGIN\n\
  SELECT RAISE(ABORT, 'prompt version history is immutable');\n\
END;\n\
CREATE TRIGGER prompt_version_variables_insert_requires_unsealed\n\
  BEFORE INSERT ON prompt_version_variables\n\
  WHEN NOT EXISTS (\n\
    SELECT 1 FROM prompt_versions\n\
    WHERE prompt_version_id = NEW.prompt_version_id AND variables_sealed = 0\n\
  )\n\
BEGIN\n\
  SELECT RAISE(ABORT, 'prompt version variables are sealed');\n\
END;\n\
CREATE TRIGGER prompt_versions_body_utf8_byte_limit\n\
  BEFORE INSERT ON prompt_versions\n\
  WHEN typeof(NEW.body) <> 'text' OR length(CAST(NEW.body AS BLOB)) > 262144\n\
BEGIN\n\
  SELECT RAISE(ABORT, 'prompt version body exceeds UTF-8 byte limit');\n\
END;\n\
CREATE TRIGGER prompt_tags_normalized_insert\n\
  BEFORE INSERT ON prompt_tags\n\
  WHEN typeof(NEW.tag) <> 'text'\n\
    OR length(NEW.tag) = 0\n\
    OR length(NEW.tag) > 48\n\
    OR NEW.tag <> trim(NEW.tag)\n\
    OR NEW.tag <> lower(NEW.tag)\n\
BEGIN\n\
  SELECT RAISE(ABORT, 'prompt tag is not normalized');\n\
END;\n\
CREATE TRIGGER prompt_tags_normalized_update\n\
  BEFORE UPDATE OF tag ON prompt_tags\n\
  WHEN typeof(NEW.tag) <> 'text'\n\
    OR length(NEW.tag) = 0\n\
    OR length(NEW.tag) > 48\n\
    OR NEW.tag <> trim(NEW.tag)\n\
    OR NEW.tag <> lower(NEW.tag)\n\
BEGIN\n\
  SELECT RAISE(ABORT, 'prompt tag is not normalized');\n\
END;\n\
CREATE TRIGGER prompt_version_variables_normalized_insert\n\
  BEFORE INSERT ON prompt_version_variables\n\
  WHEN typeof(NEW.variable) <> 'text'\n\
    OR length(NEW.variable) = 0\n\
    OR length(NEW.variable) > 64\n\
    OR NEW.variable <> trim(NEW.variable)\n\
BEGIN\n\
  SELECT RAISE(ABORT, 'prompt variable is not normalized');\n\
END;\n\
CREATE TRIGGER prompt_version_variables_normalized_update\n\
  BEFORE UPDATE OF variable ON prompt_version_variables\n\
  WHEN typeof(NEW.variable) <> 'text'\n\
    OR length(NEW.variable) = 0\n\
    OR length(NEW.variable) > 64\n\
    OR NEW.variable <> trim(NEW.variable)\n\
BEGIN\n\
  SELECT RAISE(ABORT, 'prompt variable is not normalized');\n\
END;\n\
CREATE TRIGGER saved_prompts_current_version_is_latest\n\
  BEFORE UPDATE OF current_version_id ON saved_prompts\n\
  WHEN NOT EXISTS (\n\
    SELECT 1\n\
    FROM prompt_versions AS candidate\n\
    WHERE candidate.prompt_id = NEW.prompt_id\n\
      AND candidate.prompt_version_id = NEW.current_version_id\n\
      AND candidate.version = (\n\
        SELECT MAX(latest.version) FROM prompt_versions AS latest\n\
        WHERE latest.prompt_id = NEW.prompt_id\n\
      )\n\
  )\n\
BEGIN\n\
  SELECT RAISE(ABORT, 'current prompt version must be latest');\n\
END;\n\
CREATE TRIGGER prompt_chain_command_receipts_immutable_update\n\
  BEFORE UPDATE ON prompt_chain_command_receipts\n\
BEGIN\n\
  SELECT RAISE(ABORT, 'prompt chain command receipts are immutable');\n\
END;\n\
CREATE TRIGGER prompt_chain_command_receipts_immutable_delete\n\
  BEFORE DELETE ON prompt_chain_command_receipts\n\
BEGIN\n\
  SELECT RAISE(ABORT, 'prompt chain command receipts are append-only');\n\
END;\n\
CREATE TRIGGER prompt_chain_events_immutable_update\n\
  BEFORE UPDATE ON prompt_chain_events\n\
BEGIN\n\
  SELECT RAISE(ABORT, 'prompt chain event provenance is immutable');\n\
END;\n\
CREATE TRIGGER prompt_chain_events_immutable_delete\n\
  BEFORE DELETE ON prompt_chain_events\n\
BEGIN\n\
  SELECT RAISE(ABORT, 'prompt chain event provenance is append-only');\n\
END;\n\
";

const V9_SQL: &str = "\
ALTER TABLE prompt_command_receipts ADD COLUMN command_payload BLOB
  CHECK(command_payload IS NULL OR length(command_payload) > 0);\n\
ALTER TABLE prompt_chain_command_receipts ADD COLUMN command_payload BLOB
  CHECK(command_payload IS NULL OR length(command_payload) > 0);\n\
-- V8 receipts/events predate durable command bytes. They cannot be replayed
-- safely, so quarantine them in this migration transaction and block the
-- prompt store until an operator supplies an exact reconstruction.
CREATE TABLE prompt_lineage_quarantine (
  quarantine_id INTEGER PRIMARY KEY AUTOINCREMENT,
  source_kind TEXT NOT NULL,
  command_id BLOB NOT NULL CHECK(length(command_id) = 16),
  event_id BLOB CHECK(event_id IS NULL OR length(event_id) = 16),
  reason TEXT NOT NULL,
  command_sha256 BLOB NOT NULL CHECK(length(command_sha256) = 32),
  quarantined_at_ms INTEGER NOT NULL
);\n\
-- This append-only ledger makes a quarantine deletion an auditable repair
-- transition. A missing ledger row can only be cleared after the exact
-- receipt/event rows named by the ledger are present again; PromptStore::open
-- then validates their canonical bytes and full lineage in Rust.
CREATE TABLE prompt_lineage_quarantine_ledger (
  quarantine_id INTEGER PRIMARY KEY,
  source_kind TEXT NOT NULL,
  command_id BLOB NOT NULL CHECK(length(command_id) = 16),
  event_id BLOB CHECK(event_id IS NULL OR length(event_id) = 16),
  reason TEXT NOT NULL,
  command_sha256 BLOB NOT NULL CHECK(length(command_sha256) = 32),
  quarantined_at_ms INTEGER NOT NULL
);\n\
-- This immutable copy binds every initial quarantine row to the migration
-- boundary. It detects partial/raw corruption and trigger-bypass recreation;
-- it is not a cryptographic trust root against an attacker who rewrites every
-- database fact without an external authority.
CREATE TABLE prompt_lineage_quarantine_creation (
  quarantine_id INTEGER PRIMARY KEY,
  source_kind TEXT NOT NULL,
  command_id BLOB NOT NULL CHECK(length(command_id) = 16),
  event_id BLOB CHECK(event_id IS NULL OR length(event_id) = 16),
  reason TEXT NOT NULL,
  command_sha256 BLOB NOT NULL CHECK(length(command_sha256) = 32),
  quarantined_at_ms INTEGER NOT NULL
);\n\
-- A repair ledger row is only authoritative after the quarantine row was
-- actually deleted through the audited transition below. Canonical receipt /
-- event proof is still checked by PromptStore::open.
CREATE TABLE prompt_lineage_quarantine_repair_audit (
  repair_id INTEGER PRIMARY KEY AUTOINCREMENT,
  quarantine_id INTEGER NOT NULL,
  source_kind TEXT NOT NULL,
  command_id BLOB NOT NULL CHECK(length(command_id) = 16),
  event_id BLOB CHECK(event_id IS NULL OR length(event_id) = 16),
  reason TEXT NOT NULL,
  command_sha256 BLOB NOT NULL CHECK(length(command_sha256) = 32),
  quarantined_at_ms INTEGER NOT NULL,
  origin TEXT NOT NULL CHECK(origin = 'quarantine_delete'),
  UNIQUE(quarantine_id)
);\n\
CREATE TABLE prompt_lineage_migration_state (
  singleton_key INTEGER PRIMARY KEY CHECK(singleton_key = 1),
  creation_token BLOB NOT NULL CHECK(length(creation_token) = 32),
  blocked INTEGER NOT NULL CHECK(blocked IN (0, 1))
);\n\
-- Counts and the state token are an internal creation commitment. The
-- immutable row copy above supplies canonical provenance; this singleton
-- catches deletion/recreation of either authority table even when triggers
-- are bypassed. It intentionally does not claim cryptographic tamper proofing
-- without an external trust root.
CREATE TABLE prompt_lineage_migration_commitment (
  singleton_key INTEGER PRIMARY KEY CHECK(singleton_key = 1),
  migration_version INTEGER NOT NULL CHECK(migration_version = 9),
  initial_quarantine_count INTEGER NOT NULL CHECK(initial_quarantine_count >= 0),
  initial_ledger_count INTEGER NOT NULL CHECK(initial_ledger_count >= 0),
  initial_creation_count INTEGER NOT NULL CHECK(initial_creation_count >= 0),
  initial_blocked INTEGER NOT NULL CHECK(initial_blocked IN (0, 1)),
  state_creation_token BLOB NOT NULL CHECK(length(state_creation_token) = 32)
);\n\
INSERT INTO prompt_lineage_quarantine(
  source_kind, command_id, event_id, reason, command_sha256, quarantined_at_ms
)
SELECT 'prompt_receipt', command_id, NULL,
       'legacy prompt receipt has no canonical command bytes', command_sha256, 0
FROM prompt_command_receipts
WHERE command_payload IS NULL;\n\
INSERT INTO prompt_lineage_quarantine(
  source_kind, command_id, event_id, reason, command_sha256, quarantined_at_ms
)
SELECT 'prompt_chain_receipt', command_id, NULL,
       'legacy prompt chain receipt has no canonical command bytes', command_sha256, 0
FROM prompt_chain_command_receipts
WHERE command_payload IS NULL;\n\
INSERT INTO prompt_lineage_quarantine(
  source_kind, command_id, event_id, reason, command_sha256, quarantined_at_ms
)
SELECT 'prompt_event', events.command_id, events.prompt_event_id,
       CASE WHEN receipts.command_id IS NULL
            THEN 'prompt event has no command receipt'
            ELSE 'prompt event receipt has no canonical command bytes' END,
       COALESCE(receipts.command_sha256, zeroblob(32)), 0
FROM prompt_events AS events
LEFT JOIN prompt_command_receipts AS receipts
  ON receipts.command_id = events.command_id
WHERE receipts.command_id IS NULL OR receipts.command_payload IS NULL;\n\
INSERT INTO prompt_lineage_quarantine(
  source_kind, command_id, event_id, reason, command_sha256, quarantined_at_ms
)
SELECT 'prompt_chain_event', events.command_id, events.prompt_chain_event_id,
       CASE WHEN receipts.command_id IS NULL
            THEN 'prompt chain event has no command receipt'
            ELSE 'prompt chain event receipt has no canonical command bytes' END,
       COALESCE(receipts.command_sha256, zeroblob(32)), 0
FROM prompt_chain_events AS events
LEFT JOIN prompt_chain_command_receipts AS receipts
  ON receipts.command_id = events.command_id
WHERE receipts.command_id IS NULL OR receipts.command_payload IS NULL;\n\
INSERT INTO prompt_lineage_quarantine_ledger(
  quarantine_id, source_kind, command_id, event_id, reason,
  command_sha256, quarantined_at_ms
)
SELECT quarantine_id, source_kind, command_id, event_id, reason,
       command_sha256, quarantined_at_ms
FROM prompt_lineage_quarantine;\n\
INSERT INTO prompt_lineage_quarantine_creation(
  quarantine_id, source_kind, command_id, event_id, reason,
  command_sha256, quarantined_at_ms
)
SELECT quarantine_id, source_kind, command_id, event_id, reason,
       command_sha256, quarantined_at_ms
FROM prompt_lineage_quarantine;\n\
INSERT INTO prompt_lineage_migration_state(singleton_key, creation_token, blocked)
VALUES (1, randomblob(32), EXISTS(
  SELECT 1 FROM prompt_lineage_quarantine
));\n\
INSERT INTO prompt_lineage_migration_commitment(
  singleton_key, migration_version, initial_quarantine_count,
  initial_ledger_count, initial_creation_count, initial_blocked,
  state_creation_token
)
SELECT 1, 9,
       (SELECT COUNT(*) FROM prompt_lineage_quarantine),
       (SELECT COUNT(*) FROM prompt_lineage_quarantine_ledger),
       (SELECT COUNT(*) FROM prompt_lineage_quarantine_creation),
       state.blocked,
       state.creation_token
FROM prompt_lineage_migration_state AS state
WHERE state.singleton_key = 1;\n\
DROP TRIGGER prompt_command_receipts_immutable_update;
CREATE TRIGGER prompt_command_receipts_immutable_update
  BEFORE UPDATE ON prompt_command_receipts
  WHEN NOT (
    OLD.command_payload IS NULL
    AND NEW.command_payload IS NOT NULL
    AND length(NEW.command_payload) > 0
    AND NEW.command_id = OLD.command_id
    AND NEW.command_sha256 = OLD.command_sha256
    AND NEW.prompt_id = OLD.prompt_id
    AND NEW.prompt_version_id = OLD.prompt_version_id
    AND NEW.revision = OLD.revision
    AND NEW.receipt = OLD.receipt
    AND NEW.created_at_ms = OLD.created_at_ms
  )\n\
BEGIN
  SELECT RAISE(ABORT, 'prompt command receipts are immutable');
END;\n\
DROP TRIGGER prompt_chain_command_receipts_immutable_update;
CREATE TRIGGER prompt_chain_command_receipts_immutable_update
  BEFORE UPDATE ON prompt_chain_command_receipts
  WHEN NOT (
    OLD.command_payload IS NULL
    AND NEW.command_payload IS NOT NULL
    AND length(NEW.command_payload) > 0
    AND NEW.command_id = OLD.command_id
    AND NEW.command_sha256 = OLD.command_sha256
    AND NEW.chain_id = OLD.chain_id
    AND (NEW.chain_link_id IS OLD.chain_link_id)
    AND NEW.revision = OLD.revision
    AND NEW.receipt = OLD.receipt
    AND NEW.created_at_ms = OLD.created_at_ms
  )\n\
BEGIN
  SELECT RAISE(ABORT, 'prompt chain command receipts are immutable');
END;\n\
CREATE TRIGGER prompt_command_receipts_command_payload_required_insert
  BEFORE INSERT ON prompt_command_receipts
  WHEN NEW.command_payload IS NULL OR length(NEW.command_payload) = 0\n\
BEGIN
  SELECT RAISE(ABORT, 'prompt command receipt payload is required');
END;\n\
CREATE TRIGGER prompt_command_receipts_command_payload_required_update
  BEFORE UPDATE OF command_payload ON prompt_command_receipts
  WHEN NEW.command_payload IS NULL OR length(NEW.command_payload) = 0\n\
BEGIN
  SELECT RAISE(ABORT, 'prompt command receipt payload is required');
END;\n\
CREATE TRIGGER prompt_chain_command_receipts_command_payload_required_insert
  BEFORE INSERT ON prompt_chain_command_receipts
  WHEN NEW.command_payload IS NULL OR length(NEW.command_payload) = 0\n\
BEGIN
  SELECT RAISE(ABORT, 'prompt chain command receipt payload is required');
END;\n\
CREATE TRIGGER prompt_chain_command_receipts_command_payload_required_update
  BEFORE UPDATE OF command_payload ON prompt_chain_command_receipts
  WHEN NEW.command_payload IS NULL OR length(NEW.command_payload) = 0\n\
BEGIN
  SELECT RAISE(ABORT, 'prompt chain command receipt payload is required');
END;\n\
-- Keep the durable wire budget enforced by SQLite as well as Rust. These
-- triggers are added by V9 to existing V8 databases before any store read can
-- materialize an unbounded command, receipt, or event blob.
CREATE TRIGGER prompt_command_receipts_command_payload_size_insert
  BEFORE INSERT ON prompt_command_receipts
  WHEN NEW.command_payload IS NOT NULL
    AND length(CAST(NEW.command_payload AS BLOB)) > 524288\n\
BEGIN
  SELECT RAISE(ABORT, 'prompt command receipt command payload exceeds durable maximum');
END;\n\
CREATE TRIGGER prompt_command_receipts_command_payload_size_update
  BEFORE UPDATE OF command_payload ON prompt_command_receipts
  WHEN NEW.command_payload IS NOT NULL
    AND length(CAST(NEW.command_payload AS BLOB)) > 524288\n\
BEGIN
  SELECT RAISE(ABORT, 'prompt command receipt command payload exceeds durable maximum');
END;\n\
CREATE TRIGGER prompt_chain_command_receipts_command_payload_size_insert
  BEFORE INSERT ON prompt_chain_command_receipts
  WHEN NEW.command_payload IS NOT NULL
    AND length(CAST(NEW.command_payload AS BLOB)) > 524288\n\
BEGIN
  SELECT RAISE(ABORT, 'prompt chain command receipt command payload exceeds durable maximum');
END;\n\
CREATE TRIGGER prompt_chain_command_receipts_command_payload_size_update
  BEFORE UPDATE OF command_payload ON prompt_chain_command_receipts
  WHEN NEW.command_payload IS NOT NULL
    AND length(CAST(NEW.command_payload AS BLOB)) > 524288\n\
BEGIN
  SELECT RAISE(ABORT, 'prompt chain command receipt command payload exceeds durable maximum');
END;\n\
CREATE TRIGGER prompt_command_receipts_receipt_size_insert
  BEFORE INSERT ON prompt_command_receipts
  WHEN length(CAST(NEW.receipt AS BLOB)) > 524288\n\
BEGIN
  SELECT RAISE(ABORT, 'prompt command receipt exceeds durable maximum');
END;\n\
CREATE TRIGGER prompt_command_receipts_receipt_size_update
  BEFORE UPDATE OF receipt ON prompt_command_receipts
  WHEN length(CAST(NEW.receipt AS BLOB)) > 524288\n\
BEGIN
  SELECT RAISE(ABORT, 'prompt command receipt exceeds durable maximum');
END;\n\
CREATE TRIGGER prompt_chain_command_receipts_receipt_size_insert
  BEFORE INSERT ON prompt_chain_command_receipts
  WHEN length(CAST(NEW.receipt AS BLOB)) > 524288\n\
BEGIN
  SELECT RAISE(ABORT, 'prompt chain command receipt exceeds durable maximum');
END;\n\
CREATE TRIGGER prompt_chain_command_receipts_receipt_size_update
  BEFORE UPDATE OF receipt ON prompt_chain_command_receipts
  WHEN length(CAST(NEW.receipt AS BLOB)) > 524288\n\
BEGIN
  SELECT RAISE(ABORT, 'prompt chain command receipt exceeds durable maximum');
END;\n\
CREATE TRIGGER prompt_events_payload_size_insert
  BEFORE INSERT ON prompt_events
  WHEN length(CAST(NEW.payload AS BLOB)) > 524288\n\
BEGIN
  SELECT RAISE(ABORT, 'prompt event payload exceeds durable maximum');
END;\n\
CREATE TRIGGER prompt_events_payload_size_update
  BEFORE UPDATE OF payload ON prompt_events
  WHEN length(CAST(NEW.payload AS BLOB)) > 524288\n\
BEGIN
  SELECT RAISE(ABORT, 'prompt event payload exceeds durable maximum');
END;\n\
CREATE TRIGGER prompt_chain_events_payload_size_insert
  BEFORE INSERT ON prompt_chain_events
  WHEN length(CAST(NEW.payload AS BLOB)) > 524288\n\
BEGIN
  SELECT RAISE(ABORT, 'prompt chain event payload exceeds durable maximum');
END;\n\
CREATE TRIGGER prompt_chain_events_payload_size_update
  BEFORE UPDATE OF payload ON prompt_chain_events
  WHEN length(CAST(NEW.payload AS BLOB)) > 524288\n\
BEGIN
  SELECT RAISE(ABORT, 'prompt chain event payload exceeds durable maximum');
END;\n\
CREATE TRIGGER prompt_lineage_migration_state_unblock_requires_repair
  BEFORE UPDATE OF blocked ON prompt_lineage_migration_state
  WHEN NEW.blocked = 0 AND (
    EXISTS (SELECT 1 FROM prompt_lineage_quarantine)
    OR EXISTS (
      SELECT 1 FROM prompt_command_receipts
      WHERE command_payload IS NULL OR length(command_payload) = 0
    )
    OR EXISTS (
      SELECT 1 FROM prompt_chain_command_receipts
      WHERE command_payload IS NULL OR length(command_payload) = 0
    )
  )\n\
BEGIN
  SELECT RAISE(ABORT, 'prompt lineage requires exact repair before unblock');
END;\n\
CREATE TRIGGER prompt_lineage_migration_state_unblock_requires_ledger_repair
  BEFORE UPDATE OF blocked ON prompt_lineage_migration_state
  WHEN NEW.blocked = 0 AND EXISTS (
    SELECT 1
    FROM prompt_lineage_quarantine_ledger AS ledger
    WHERE ledger.command_sha256 = zeroblob(32)
      OR (
      NOT EXISTS (
      SELECT 1
      FROM prompt_lineage_quarantine AS quarantine
      WHERE quarantine.quarantine_id = ledger.quarantine_id
        AND quarantine.source_kind = ledger.source_kind
        AND quarantine.command_id = ledger.command_id
        AND (quarantine.event_id IS ledger.event_id)
        AND quarantine.reason = ledger.reason
        AND quarantine.command_sha256 = ledger.command_sha256
        AND quarantine.quarantined_at_ms = ledger.quarantined_at_ms
    )
    AND NOT (
      (
        ledger.source_kind = 'prompt_receipt'
        AND ledger.event_id IS NULL
        AND EXISTS (
          SELECT 1 FROM prompt_command_receipts AS receipt
          WHERE receipt.command_id = ledger.command_id
            AND receipt.command_sha256 = ledger.command_sha256
            AND receipt.command_payload IS NOT NULL
            AND length(receipt.command_payload) > 0
        )
      )
      OR (
        ledger.source_kind = 'prompt_chain_receipt'
        AND ledger.event_id IS NULL
        AND EXISTS (
          SELECT 1 FROM prompt_chain_command_receipts AS receipt
          WHERE receipt.command_id = ledger.command_id
            AND receipt.command_sha256 = ledger.command_sha256
            AND receipt.command_payload IS NOT NULL
            AND length(receipt.command_payload) > 0
        )
      )
      OR (
        ledger.source_kind = 'prompt_event'
        AND ledger.event_id IS NOT NULL
        AND EXISTS (
          SELECT 1
          FROM prompt_events AS event
          JOIN prompt_command_receipts AS receipt
            ON receipt.command_id = event.command_id
          WHERE event.prompt_event_id = ledger.event_id
            AND event.command_id = ledger.command_id
            AND receipt.command_sha256 = ledger.command_sha256
            AND receipt.command_payload IS NOT NULL
            AND length(receipt.command_payload) > 0
        )
      )
      OR (
        ledger.source_kind = 'prompt_chain_event'
        AND ledger.event_id IS NOT NULL
        AND EXISTS (
          SELECT 1
          FROM prompt_chain_events AS event
          JOIN prompt_chain_command_receipts AS receipt
            ON receipt.command_id = event.command_id
          WHERE event.prompt_chain_event_id = ledger.event_id
            AND event.command_id = ledger.command_id
            AND receipt.command_sha256 = ledger.command_sha256
            AND receipt.command_payload IS NOT NULL
            AND length(receipt.command_payload) > 0
        )
      )
    )
      )
  )\n\
BEGIN
  SELECT RAISE(ABORT, 'prompt lineage requires ledger-backed exact repair before unblock');
END;\n\
CREATE TRIGGER prompt_lineage_migration_state_append_only_insert
  BEFORE INSERT ON prompt_lineage_migration_state
BEGIN
  SELECT RAISE(ABORT, 'prompt lineage migration state is migration-owned');
END;\n\
CREATE TRIGGER prompt_lineage_migration_state_unblock_requires_repair_insert
  BEFORE INSERT ON prompt_lineage_migration_state
  WHEN NEW.blocked = 0 AND (
    EXISTS (SELECT 1 FROM prompt_lineage_quarantine)
    OR EXISTS (
      SELECT 1 FROM prompt_command_receipts
      WHERE command_payload IS NULL OR length(command_payload) = 0
    )
    OR EXISTS (
      SELECT 1 FROM prompt_chain_command_receipts
      WHERE command_payload IS NULL OR length(command_payload) = 0
    )
  )\n\
BEGIN
  SELECT RAISE(ABORT, 'prompt lineage requires exact repair before unblock');
END;\n\
CREATE TRIGGER prompt_lineage_migration_state_unblock_requires_ledger_repair_insert
  BEFORE INSERT ON prompt_lineage_migration_state
  WHEN NEW.blocked = 0 AND EXISTS (
    SELECT 1 FROM prompt_lineage_quarantine_ledger AS ledger
    WHERE ledger.command_sha256 = zeroblob(32)
      OR NOT EXISTS (
      SELECT 1 FROM prompt_lineage_quarantine AS quarantine
      WHERE quarantine.quarantine_id = ledger.quarantine_id
        AND quarantine.source_kind = ledger.source_kind
        AND quarantine.command_id = ledger.command_id
        AND (quarantine.event_id IS ledger.event_id)
        AND quarantine.reason = ledger.reason
        AND quarantine.command_sha256 = ledger.command_sha256
        AND quarantine.quarantined_at_ms = ledger.quarantined_at_ms
    )
  )\n\
BEGIN
  SELECT RAISE(ABORT, 'prompt lineage requires ledger-backed exact repair before unblock');
END;\n\
CREATE TRIGGER prompt_lineage_migration_state_immutable_delete
  BEFORE DELETE ON prompt_lineage_migration_state
BEGIN
  SELECT RAISE(ABORT, 'prompt lineage migration state is immutable');
END;\n\
CREATE TRIGGER prompt_lineage_migration_state_immutable_update
  BEFORE UPDATE ON prompt_lineage_migration_state
  WHEN NOT (
    OLD.singleton_key = 1
    AND NEW.singleton_key = 1
    AND NEW.creation_token = OLD.creation_token
    AND (
      OLD.blocked = NEW.blocked
      OR (
        OLD.blocked = 0
        AND NEW.blocked = 1
        AND (
          EXISTS (SELECT 1 FROM prompt_lineage_quarantine)
          OR EXISTS (
            SELECT 1 FROM prompt_command_receipts
            WHERE command_payload IS NULL OR length(command_payload) = 0
          )
          OR EXISTS (
            SELECT 1 FROM prompt_chain_command_receipts
            WHERE command_payload IS NULL OR length(command_payload) = 0
          )
        )
      )
      OR (
        OLD.blocked = 1
        AND NEW.blocked = 0
        AND NOT EXISTS (SELECT 1 FROM prompt_lineage_quarantine)
        AND NOT EXISTS (
          SELECT 1 FROM prompt_command_receipts
          WHERE command_payload IS NULL OR length(command_payload) = 0
        )
        AND NOT EXISTS (
          SELECT 1 FROM prompt_chain_command_receipts
          WHERE command_payload IS NULL OR length(command_payload) = 0
        )
      )
    )
  )\n\
BEGIN
  SELECT RAISE(ABORT, 'prompt lineage migration state requires exact repair');
END;\n\
CREATE TRIGGER prompt_lineage_quarantine_creation_append_only_insert
  BEFORE INSERT ON prompt_lineage_quarantine_creation
BEGIN
  SELECT RAISE(ABORT, 'prompt lineage quarantine creation is migration-owned');
END;\n\
CREATE TRIGGER prompt_lineage_quarantine_creation_immutable_update
  BEFORE UPDATE ON prompt_lineage_quarantine_creation
BEGIN
  SELECT RAISE(ABORT, 'prompt lineage quarantine creation is immutable');
END;\n\
CREATE TRIGGER prompt_lineage_quarantine_creation_immutable_delete
  BEFORE DELETE ON prompt_lineage_quarantine_creation
BEGIN
  SELECT RAISE(ABORT, 'prompt lineage quarantine creation is append-only');
END;\n\
CREATE TRIGGER prompt_lineage_migration_commitment_append_only_insert
  BEFORE INSERT ON prompt_lineage_migration_commitment
BEGIN
  SELECT RAISE(ABORT, 'prompt lineage migration commitment is migration-owned');
END;\n\
CREATE TRIGGER prompt_lineage_migration_commitment_immutable_update
  BEFORE UPDATE ON prompt_lineage_migration_commitment
BEGIN
  SELECT RAISE(ABORT, 'prompt lineage migration commitment is immutable');
END;\n\
CREATE TRIGGER prompt_lineage_migration_commitment_immutable_delete
  BEFORE DELETE ON prompt_lineage_migration_commitment
BEGIN
  SELECT RAISE(ABORT, 'prompt lineage migration commitment is append-only');
END;\n\
CREATE TRIGGER prompt_lineage_quarantine_append_only_insert
  BEFORE INSERT ON prompt_lineage_quarantine
BEGIN
  SELECT RAISE(ABORT, 'prompt lineage quarantine is migration-owned');
END;\n\
CREATE TRIGGER prompt_lineage_quarantine_immutable_update
  BEFORE UPDATE ON prompt_lineage_quarantine
BEGIN
  SELECT RAISE(ABORT, 'prompt lineage quarantine is immutable');
END;\n\
CREATE TRIGGER prompt_lineage_quarantine_immutable_delete
  BEFORE DELETE ON prompt_lineage_quarantine
  WHEN NOT EXISTS (
    SELECT 1
    FROM prompt_lineage_quarantine_ledger AS ledger
    WHERE ledger.quarantine_id = OLD.quarantine_id
      AND ledger.source_kind = OLD.source_kind
      AND ledger.command_id = OLD.command_id
      AND (ledger.event_id IS OLD.event_id)
      AND ledger.reason = OLD.reason
      AND ledger.command_sha256 = OLD.command_sha256
      AND ledger.quarantined_at_ms = OLD.quarantined_at_ms
  )\n\
BEGIN
  SELECT RAISE(ABORT, 'prompt lineage quarantine deletion lacks repair provenance');
END;\n\
CREATE TRIGGER prompt_lineage_quarantine_repair_audit_after_delete
  AFTER DELETE ON prompt_lineage_quarantine
BEGIN
  INSERT INTO prompt_lineage_quarantine_repair_audit(
    quarantine_id, source_kind, command_id, event_id, reason,
    command_sha256, quarantined_at_ms, origin
  ) VALUES (
    OLD.quarantine_id, OLD.source_kind, OLD.command_id, OLD.event_id, OLD.reason,
    OLD.command_sha256, OLD.quarantined_at_ms, 'quarantine_delete'
  );
END;\n\
CREATE TRIGGER prompt_lineage_quarantine_repair_audit_append_only_insert
  BEFORE INSERT ON prompt_lineage_quarantine_repair_audit
  WHEN NEW.origin <> 'quarantine_delete'
BEGIN
  SELECT RAISE(ABORT, 'prompt lineage quarantine repair audit is migration-owned');
END;\n\
CREATE TRIGGER prompt_lineage_quarantine_repair_audit_immutable_update
  BEFORE UPDATE ON prompt_lineage_quarantine_repair_audit
BEGIN
  SELECT RAISE(ABORT, 'prompt lineage quarantine repair audit is immutable');
END;\n\
CREATE TRIGGER prompt_lineage_quarantine_repair_audit_immutable_delete
  BEFORE DELETE ON prompt_lineage_quarantine_repair_audit
BEGIN
  SELECT RAISE(ABORT, 'prompt lineage quarantine repair audit is append-only');
END;\n\
CREATE TRIGGER prompt_lineage_quarantine_ledger_immutable_insert
  BEFORE INSERT ON prompt_lineage_quarantine_ledger
BEGIN
  SELECT RAISE(ABORT, 'prompt lineage quarantine ledger is migration-owned');
END;\n\
CREATE TRIGGER prompt_lineage_quarantine_ledger_immutable_update
  BEFORE UPDATE ON prompt_lineage_quarantine_ledger
BEGIN
  SELECT RAISE(ABORT, 'prompt lineage quarantine ledger is immutable');
END;\n\
CREATE TRIGGER prompt_lineage_quarantine_ledger_immutable_delete
  BEFORE DELETE ON prompt_lineage_quarantine_ledger
BEGIN
  SELECT RAISE(ABORT, 'prompt lineage quarantine ledger is append-only');
END;\n\
CREATE TRIGGER saved_prompts_current_version_is_latest_insert
  BEFORE INSERT ON saved_prompts
  WHEN NOT EXISTS (
    SELECT 1
    FROM prompt_versions AS candidate
    WHERE candidate.prompt_id = NEW.prompt_id
      AND candidate.prompt_version_id = NEW.current_version_id
      AND candidate.version = (
        SELECT MAX(latest.version) FROM prompt_versions AS latest
        WHERE latest.prompt_id = NEW.prompt_id
      )
  )\n\
BEGIN
  SELECT RAISE(ABORT, 'current prompt version must be latest');
END;\n\
CREATE TRIGGER prompt_versions_next_sequence_insert
  BEFORE INSERT ON prompt_versions
  WHEN NEW.version <> COALESCE((
    SELECT MAX(existing.version) + 1
    FROM prompt_versions AS existing
    WHERE existing.prompt_id = NEW.prompt_id
  ), 1)\n\
BEGIN
  SELECT RAISE(ABORT, 'prompt version history must be contiguous');
END;\n\
CREATE TRIGGER prompt_versions_advance_current_after_insert
  AFTER INSERT ON prompt_versions
  WHEN NEW.version = (
    SELECT MAX(latest.version) FROM prompt_versions AS latest
    WHERE latest.prompt_id = NEW.prompt_id
  )\n\
BEGIN
  UPDATE saved_prompts
  SET current_version_id = NEW.prompt_version_id
  WHERE prompt_id = NEW.prompt_id
    AND current_version_id <> NEW.prompt_version_id;
END;\n\
CREATE TRIGGER prompt_command_receipts_lineage_insert
  BEFORE INSERT ON prompt_command_receipts
  WHEN NOT EXISTS (
    SELECT 1
    FROM prompt_versions
    WHERE prompt_version_id = NEW.prompt_version_id
      AND prompt_id = NEW.prompt_id
  )\n\
BEGIN
  SELECT RAISE(ABORT, 'prompt command receipt lineage is missing');
END;\n\
CREATE TRIGGER prompt_chain_command_receipts_lineage_insert
  BEFORE INSERT ON prompt_chain_command_receipts
  WHEN NOT EXISTS (
    SELECT 1 FROM prompt_chains WHERE chain_id = NEW.chain_id
  )\n\
BEGIN
  SELECT RAISE(ABORT, 'prompt chain command receipt lineage is missing');
END;\n\
-- SQLite trim(X) only removes ASCII spaces. Keep this explicit Unicode
-- White_Space set in lockstep with prompts::model::trim_prompt_whitespace.
CREATE TRIGGER saved_prompts_metadata_insert_bounds
  BEFORE INSERT ON saved_prompts
  WHEN typeof(NEW.title) <> 'text'
    OR length(NEW.title) = 0
    OR length(NEW.title) > 160
    OR length(CAST(NEW.title AS BLOB)) > 640
    OR NEW.title <> trim(NEW.title, char(9, 10, 11, 12, 13, 32, 133, 160, 5760, 8192, 8193, 8194, 8195, 8196, 8197, 8198, 8199, 8200, 8201, 8202, 8232, 8233, 8239, 8287, 12288))
    OR (NEW.description IS NOT NULL AND (
      typeof(NEW.description) <> 'text'
      OR length(NEW.description) > 2000
      OR length(CAST(NEW.description AS BLOB)) > 8000
      OR NEW.description <> trim(NEW.description, char(9, 10, 11, 12, 13, 32, 133, 160, 5760, 8192, 8193, 8194, 8195, 8196, 8197, 8198, 8199, 8200, 8201, 8202, 8232, 8233, 8239, 8287, 12288))
    ))\n\
BEGIN
  SELECT RAISE(ABORT, 'saved prompt title or description is out of bounds');
END;\n\
CREATE TRIGGER saved_prompts_metadata_update_bounds
  BEFORE UPDATE OF title, description ON saved_prompts
  WHEN typeof(NEW.title) <> 'text'
    OR length(NEW.title) = 0
    OR length(NEW.title) > 160
    OR length(CAST(NEW.title AS BLOB)) > 640
    OR NEW.title <> trim(NEW.title, char(9, 10, 11, 12, 13, 32, 133, 160, 5760, 8192, 8193, 8194, 8195, 8196, 8197, 8198, 8199, 8200, 8201, 8202, 8232, 8233, 8239, 8287, 12288))
    OR (NEW.description IS NOT NULL AND (
      typeof(NEW.description) <> 'text'
      OR length(NEW.description) > 2000
      OR length(CAST(NEW.description AS BLOB)) > 8000
      OR NEW.description <> trim(NEW.description, char(9, 10, 11, 12, 13, 32, 133, 160, 5760, 8192, 8193, 8194, 8195, 8196, 8197, 8198, 8199, 8200, 8201, 8202, 8232, 8233, 8239, 8287, 12288))
    ))\n\
BEGIN
  SELECT RAISE(ABORT, 'saved prompt title or description is out of bounds');
END;\n\
CREATE TRIGGER prompt_chains_metadata_insert_bounds
  BEFORE INSERT ON prompt_chains
  WHEN typeof(NEW.title) <> 'text'
    OR length(NEW.title) = 0
    OR length(NEW.title) > 160
    OR length(CAST(NEW.title AS BLOB)) > 640
    OR NEW.title <> trim(NEW.title, char(9, 10, 11, 12, 13, 32, 133, 160, 5760, 8192, 8193, 8194, 8195, 8196, 8197, 8198, 8199, 8200, 8201, 8202, 8232, 8233, 8239, 8287, 12288))
    OR (NEW.description IS NOT NULL AND (
      typeof(NEW.description) <> 'text'
      OR length(NEW.description) > 2000
      OR length(CAST(NEW.description AS BLOB)) > 8000
      OR NEW.description <> trim(NEW.description, char(9, 10, 11, 12, 13, 32, 133, 160, 5760, 8192, 8193, 8194, 8195, 8196, 8197, 8198, 8199, 8200, 8201, 8202, 8232, 8233, 8239, 8287, 12288))
    ))\n\
BEGIN
  SELECT RAISE(ABORT, 'prompt chain title or description is out of bounds');
END;\n\
CREATE TRIGGER prompt_chains_metadata_update_bounds
  BEFORE UPDATE OF title, description ON prompt_chains
  WHEN typeof(NEW.title) <> 'text'
    OR length(NEW.title) = 0
    OR length(NEW.title) > 160
    OR length(CAST(NEW.title AS BLOB)) > 640
    OR NEW.title <> trim(NEW.title, char(9, 10, 11, 12, 13, 32, 133, 160, 5760, 8192, 8193, 8194, 8195, 8196, 8197, 8198, 8199, 8200, 8201, 8202, 8232, 8233, 8239, 8287, 12288))
    OR (NEW.description IS NOT NULL AND (
      typeof(NEW.description) <> 'text'
      OR length(NEW.description) > 2000
      OR length(CAST(NEW.description AS BLOB)) > 8000
      OR NEW.description <> trim(NEW.description, char(9, 10, 11, 12, 13, 32, 133, 160, 5760, 8192, 8193, 8194, 8195, 8196, 8197, 8198, 8199, 8200, 8201, 8202, 8232, 8233, 8239, 8287, 12288))
    ))\n\
BEGIN
  SELECT RAISE(ABORT, 'prompt chain title or description is out of bounds');
END;\n\
CREATE TRIGGER prompt_tags_max_count_insert
  BEFORE INSERT ON prompt_tags
  WHEN (SELECT COUNT(*) FROM prompt_tags WHERE prompt_id = NEW.prompt_id) >= 32\n\
BEGIN
  SELECT RAISE(ABORT, 'prompt tag count exceeds maximum');
END;\n\
CREATE TRIGGER prompt_tags_max_count_update
  BEFORE UPDATE OF prompt_id ON prompt_tags
  WHEN NEW.prompt_id <> OLD.prompt_id
    AND (SELECT COUNT(*) FROM prompt_tags WHERE prompt_id = NEW.prompt_id) >= 32\n\
BEGIN
  SELECT RAISE(ABORT, 'prompt tag count exceeds maximum');
END;\n\
CREATE TRIGGER prompt_tags_ascii_lower_insert
  BEFORE INSERT ON prompt_tags
  WHEN typeof(NEW.tag) <> 'text'
    OR length(NEW.tag) = 0
    OR NEW.tag <> trim(NEW.tag, char(9, 10, 11, 12, 13, 32, 133, 160, 5760, 8192, 8193, 8194, 8195, 8196, 8197, 8198, 8199, 8200, 8201, 8202, 8232, 8233, 8239, 8287, 12288))
    OR NEW.tag <> lower(NEW.tag)
    OR NEW.tag GLOB '*[^ -~]*'\n\
BEGIN
  SELECT RAISE(ABORT, 'prompt tags must use printable lowercase ASCII');
END;\n\
CREATE TRIGGER prompt_tags_ascii_lower_update
  BEFORE UPDATE OF tag ON prompt_tags
  WHEN typeof(NEW.tag) <> 'text'
    OR length(NEW.tag) = 0
    OR NEW.tag <> trim(NEW.tag, char(9, 10, 11, 12, 13, 32, 133, 160, 5760, 8192, 8193, 8194, 8195, 8196, 8197, 8198, 8199, 8200, 8201, 8202, 8232, 8233, 8239, 8287, 12288))
    OR NEW.tag <> lower(NEW.tag)
    OR NEW.tag GLOB '*[^ -~]*'\n\
BEGIN
  SELECT RAISE(ABORT, 'prompt tags must use printable lowercase ASCII');
END;\n\
CREATE TRIGGER prompt_version_variables_unicode_whitespace_insert
  BEFORE INSERT ON prompt_version_variables
  WHEN typeof(NEW.variable) <> 'text'
    OR NEW.variable <> trim(NEW.variable, char(9, 10, 11, 12, 13, 32, 133, 160, 5760, 8192, 8193, 8194, 8195, 8196, 8197, 8198, 8199, 8200, 8201, 8202, 8232, 8233, 8239, 8287, 12288))\n\
BEGIN
  SELECT RAISE(ABORT, 'prompt variable is not normalized');
END;\n\
CREATE TRIGGER prompt_version_variables_unicode_whitespace_update
  BEFORE UPDATE OF variable ON prompt_version_variables
  WHEN typeof(NEW.variable) <> 'text'
    OR NEW.variable <> trim(NEW.variable, char(9, 10, 11, 12, 13, 32, 133, 160, 5760, 8192, 8193, 8194, 8195, 8196, 8197, 8198, 8199, 8200, 8201, 8202, 8232, 8233, 8239, 8287, 12288))\n\
BEGIN
  SELECT RAISE(ABORT, 'prompt variable is not normalized');
END;\n\
CREATE TRIGGER prompt_version_variables_max_count_insert
  BEFORE INSERT ON prompt_version_variables
  WHEN (SELECT COUNT(*) FROM prompt_version_variables
        WHERE prompt_version_id = NEW.prompt_version_id) >= 32\n\
BEGIN
  SELECT RAISE(ABORT, 'prompt variable count exceeds maximum');
END;\n\
CREATE TRIGGER prompt_chain_links_max_count_insert
  BEFORE INSERT ON prompt_chain_links
  WHEN (SELECT COUNT(*) FROM prompt_chain_links WHERE chain_id = NEW.chain_id) >= 2000\n\
BEGIN
  SELECT RAISE(ABORT, 'prompt chain link count exceeds maximum');
END;\n\
CREATE TRIGGER prompt_chain_links_max_count_update
  BEFORE UPDATE OF chain_id ON prompt_chain_links
  WHEN NEW.chain_id <> OLD.chain_id
    AND (SELECT COUNT(*) FROM prompt_chain_links WHERE chain_id = NEW.chain_id) >= 2000\n\
BEGIN
  SELECT RAISE(ABORT, 'prompt chain link count exceeds maximum');
END;\n\
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
                    name: "phase07-prompts-v1",
                    sql: V7_SQL,
                    sha256: sha256_bytes(V7_SQL),
                },
                Migration {
                    version: 8,
                    name: "phase07-prompts-corrections-v2",
                    sql: V8_SQL,
                    sha256: sha256_bytes(V8_SQL),
                },
                Migration {
                    version: 9,
                    name: "phase07-prompts-lineage-authority-v3",
                    sql: V9_SQL,
                    sha256: sha256_bytes(V9_SQL),
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
    "host_admission",
    "host_cleanup_branches",
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
        assert_eq!(history.len(), 9);
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
        assert_eq!(history[6].1, "phase07-prompts-v1");
        assert_eq!(history[6].2.len(), 32);
        assert_eq!(history[7].0, 8);
        assert_eq!(history[7].1, "phase07-prompts-corrections-v2");
        assert_eq!(history[7].2.len(), 32);
        assert_eq!(history[8].0, 9);
        assert_eq!(history[8].1, "phase07-prompts-lineage-authority-v3");
        assert_eq!(history[8].2.len(), 32);

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
    fn schema_upgrade_matrix_reaches_corrections_before_prompt_lifecycle() {
        for prior_version in 1_i64..=8 {
            let dir = TempDir::new().expect("tempdir");
            let path = dir.path().join(format!("prior-v{prior_version}.sqlite3"));
            {
                let conn = Connection::open(&path).expect("open prior schema");
                for migration in migration_manifest()
                    .iter()
                    .take(usize::try_from(prior_version).expect("version fits"))
                {
                    conn.execute_batch(migration.sql)
                        .expect("apply prior migration SQL");
                    conn.execute(
                        "INSERT INTO schema_migrations(version, name, applied_at_ms, sha256)
                         VALUES (?1, ?2, ?3, ?4)",
                        rusqlite::params![
                            migration.version,
                            migration.name,
                            migration.version,
                            migration.sha256.as_slice(),
                        ],
                    )
                    .expect("record prior migration");
                }
            }

            {
                if prior_version == 8 {
                    use crate::domain::id::{
                        CommandId, EventId, PromptChainId, PromptId, PromptVersionId,
                    };
                    use crate::prompts::{
                        CreatePrompt, CreatePromptChain, PromptChain, PromptChainCommand,
                        PromptChainEvent, PromptChainMutationReceipt, PromptCommand, PromptEvent,
                        PromptMutationReceipt, PromptVersion, SavedPrompt,
                    };
                    let prompt_id = PromptId::new();
                    let prompt_version_id = PromptVersionId::new();
                    let prompt_command_id = CommandId::new();
                    let prompt_version = PromptVersion::new(
                        prompt_version_id,
                        prompt_id,
                        1,
                        "legacy prompt body".into(),
                        1,
                    )
                    .expect("legacy prompt version");
                    let prompt = SavedPrompt {
                        id: prompt_id,
                        title: "Legacy prompt".into(),
                        description: None,
                        tags: Vec::new(),
                        current_version_id: prompt_version_id,
                        revision: 1,
                        archived_at_ms: None,
                    };
                    let prompt_command = PromptCommand::CreatePrompt(CreatePrompt {
                        prompt_id,
                        prompt_version_id,
                        title: prompt.title.clone(),
                        description: None,
                        tags: Vec::new(),
                        variables: Vec::new(),
                        body: prompt_version.body.clone(),
                        created_at_ms: 1,
                    });
                    let prompt_receipt = PromptMutationReceipt {
                        command_id: prompt_command_id,
                        prompt_id,
                        prompt_version_id,
                        revision: 1,
                    };
                    let prompt_event = PromptEvent::PromptCreated {
                        prompt: prompt.clone(),
                        version: prompt_version.clone(),
                    };
                    let chain_id = PromptChainId::new();
                    let chain_command_id = CommandId::new();
                    let chain = PromptChain {
                        id: chain_id,
                        title: "Legacy chain".into(),
                        description: None,
                        revision: 1,
                        archived_at_ms: None,
                    };
                    let chain_command = PromptChainCommand::CreatePromptChain(CreatePromptChain {
                        chain_id,
                        title: chain.title.clone(),
                        description: None,
                        created_at_ms: 1,
                    });
                    let chain_receipt = PromptChainMutationReceipt {
                        command_id: chain_command_id,
                        chain_id,
                        link_id: None,
                        revision: 1,
                    };
                    let chain_event = PromptChainEvent::PromptChainCreated { chain };
                    let conn = Connection::open(&path).expect("open legacy lineage fixture");
                    let transaction = conn
                        .unchecked_transaction()
                        .expect("start legacy prompt fixture transaction");
                    transaction
                        .execute(
                            "INSERT INTO prompt_versions(
                            prompt_version_id, prompt_id, version, body,
                            body_sha256, created_at_ms, variables_sealed
                         ) VALUES (?1, ?2, 1, ?3, ?4, 1, 1)",
                            rusqlite::params![
                                prompt_version_id.as_bytes().as_slice(),
                                prompt_id.as_bytes().as_slice(),
                                prompt_version.body,
                                prompt_version.body_sha256.as_slice(),
                            ],
                        )
                        .expect("seed legacy prompt version");
                    transaction
                        .execute(
                            "INSERT INTO saved_prompts(
                            prompt_id, title, description, current_version_id, revision,
                            created_at_ms, updated_at_ms, archived_at_ms
                         ) VALUES (?1, ?2, NULL, ?3, 1, 1, 1, NULL)",
                            rusqlite::params![
                                prompt_id.as_bytes().as_slice(),
                                prompt.title,
                                prompt_version_id.as_bytes().as_slice(),
                            ],
                        )
                        .expect("seed legacy prompt projection");
                    transaction.commit().expect("commit legacy prompt fixture");
                    conn.execute(
                        "INSERT INTO prompt_command_receipts(
                            command_id, command_sha256, prompt_id, prompt_version_id,
                            revision, receipt, created_at_ms
                         ) VALUES (?1, ?2, ?3, ?4, 1, ?5, 1)",
                        rusqlite::params![
                            prompt_command_id.as_bytes().as_slice(),
                            prompt_command
                                .fingerprint()
                                .expect("legacy prompt command fingerprint")
                                .as_slice(),
                            prompt_id.as_bytes().as_slice(),
                            prompt_version_id.as_bytes().as_slice(),
                            prompt_receipt.encode().expect("legacy prompt receipt"),
                        ],
                    )
                    .expect("seed legacy prompt receipt");
                    conn.execute(
                        "INSERT INTO prompt_events(
                            prompt_event_id, command_id, prompt_id, event_type,
                            occurred_at_ms, payload
                         ) VALUES (?1, ?2, ?3, ?4, 1, ?5)",
                        rusqlite::params![
                            EventId::new().as_bytes().as_slice(),
                            prompt_command_id.as_bytes().as_slice(),
                            prompt_id.as_bytes().as_slice(),
                            prompt_event.event_type(),
                            prompt_event.encode().expect("legacy prompt event"),
                        ],
                    )
                    .expect("seed legacy prompt event");
                    conn.execute(
                        "INSERT INTO prompt_chain_command_receipts(
                            command_id, command_sha256, chain_id, chain_link_id,
                            revision, receipt, created_at_ms
                         ) VALUES (?1, ?2, ?3, NULL, 1, ?4, 1)",
                        rusqlite::params![
                            chain_command_id.as_bytes().as_slice(),
                            chain_command
                                .fingerprint()
                                .expect("legacy chain command fingerprint")
                                .as_slice(),
                            chain_id.as_bytes().as_slice(),
                            chain_receipt.encode().expect("legacy chain receipt"),
                        ],
                    )
                    .expect("seed legacy chain receipt");
                    conn.execute(
                        "INSERT INTO prompt_chain_events(
                            prompt_chain_event_id, command_id, chain_id, event_type,
                            occurred_at_ms, payload
                         ) VALUES (?1, ?2, ?3, ?4, 1, ?5)",
                        rusqlite::params![
                            EventId::new().as_bytes().as_slice(),
                            chain_command_id.as_bytes().as_slice(),
                            chain_id.as_bytes().as_slice(),
                            chain_event.event_type(),
                            chain_event.encode().expect("legacy chain event"),
                        ],
                    )
                    .expect("seed legacy chain event");
                    drop(conn);

                    let error = crate::prompts::PromptStore::open(&path)
                        .expect_err("legacy lineage must block the prompt store");
                    assert!(error.to_string().contains("legacy prompt lineage"));
                    let conn = Connection::open(&path).expect("reopen quarantined legacy schema");
                    let quarantine_count: i64 = conn
                        .query_row(
                            "SELECT COUNT(*) FROM prompt_lineage_quarantine",
                            [],
                            |row| row.get(0),
                        )
                        .expect("legacy rows must be quarantined");
                    assert_eq!(quarantine_count, 4);
                    let blocked: i64 = conn
                        .query_row(
                            "SELECT blocked FROM prompt_lineage_migration_state WHERE singleton_key = 1",
                            [],
                            |row| row.get(0),
                        )
                        .expect("legacy lineage block marker");
                    assert_eq!(blocked, 1);
                    continue;
                }

                use crate::domain::id::{CommandId, PromptId, PromptVersionId};
                use crate::prompts::{
                    ArchivePrompt, CreatePrompt, CreatePromptVersion, PromptCommand, PromptStore,
                    RestorePrompt,
                };

                let mut store = PromptStore::open(&path).expect("upgrade prior schema");
                let prompt_id = PromptId::new();
                let first_version_id = PromptVersionId::new();
                store
                    .execute(
                        CommandId::new(),
                        PromptCommand::CreatePrompt(CreatePrompt {
                            prompt_id,
                            prompt_version_id: first_version_id,
                            title: "Upgrade matrix".into(),
                            description: None,
                            tags: vec!["upgrade".into()],
                            variables: Vec::new(),
                            body: "initial body".into(),
                            created_at_ms: 1,
                        }),
                    )
                    .expect("create after upgrade");
                store
                    .execute(
                        CommandId::new(),
                        PromptCommand::CreatePromptVersion(CreatePromptVersion {
                            prompt_id,
                            prompt_version_id: PromptVersionId::new(),
                            variables: vec!["reviewer".into()],
                            body: "edited body".into(),
                            created_at_ms: 2,
                            expected_revision: 1,
                        }),
                    )
                    .expect("edit after upgrade");
                store
                    .execute(
                        CommandId::new(),
                        PromptCommand::ArchivePrompt(ArchivePrompt {
                            prompt_id,
                            archived_at_ms: 3,
                            expected_revision: 2,
                        }),
                    )
                    .expect("archive after upgrade");
                store
                    .execute(
                        CommandId::new(),
                        PromptCommand::RestorePrompt(RestorePrompt {
                            prompt_id,
                            expected_revision: 3,
                        }),
                    )
                    .expect("restore after upgrade");
                store.rebuild_projection().expect("rebuild after upgrade");
                assert_eq!(store.list_versions(prompt_id, 0, 10).unwrap().len(), 2);
                drop(store);

                let mut reopened = PromptStore::open(&path).expect("reopen upgraded schema");
                assert!(reopened
                    .get_prompt(prompt_id)
                    .expect("query reopened prompt")
                    .expect("reopened prompt")
                    .archived_at_ms
                    .is_none());
                reopened.rebuild_projection().expect("rebuild after reopen");
            }

            let conn = Connection::open(&path).expect("open upgraded raw schema");
            let migration_count: i64 = conn
                .query_row("SELECT COUNT(*) FROM schema_migrations", [], |row| {
                    row.get(0)
                })
                .expect("migration count");
            assert_eq!(migration_count, 9, "prior schema V{prior_version}");
            let missing_prompt_command_payloads: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM prompt_command_receipts
                     WHERE command_payload IS NULL",
                    [],
                    |row| row.get(0),
                )
                .expect("prompt command payload count");
            assert_eq!(
                missing_prompt_command_payloads, 0,
                "new prompt receipts must persist canonical command bytes for prior schema V{prior_version}"
            );
            let latest_name: String = conn
                .query_row(
                    "SELECT name FROM schema_migrations WHERE version = 9",
                    [],
                    |row| row.get(0),
                )
                .expect("corrective migration record");
            assert_eq!(latest_name, "phase07-prompts-lineage-authority-v3");
        }
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
}
