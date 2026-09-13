use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior};
use sha2::{Digest, Sha256};

use crate::domain::id::{
    CommandId, EventId, PromptChainId, PromptChainLinkId, PromptId, PromptVersionId,
};

use super::model::{
    normalized_tags, normalized_variables, trim_prompt_whitespace, ArchivePrompt,
    ArchivePromptChain, CreatePrompt, CreatePromptChain, CreatePromptVersion,
    InsertPromptChainLink, MovePromptChainLink, PromptChain, PromptChainCommand, PromptChainEvent,
    PromptChainLink, PromptChainLinkContext, PromptChainMutationReceipt, PromptCommand,
    PromptEvent, PromptMutationReceipt, PromptProjectionRebuild, PromptSnapshot,
    PromptValidationError, PromptVersion, RemovePromptChainLink, RenamePrompt, RenamePromptChain,
    RestorePrompt, RestorePromptChain, SavedPrompt, SetPromptTags, UpdatePromptChainLinkVersion,
    MAX_PROMPT_BODY_BYTES, MAX_PROMPT_CHAIN_DESCRIPTION_SCALARS, MAX_PROMPT_CHAIN_LINKS,
    MAX_PROMPT_CHAIN_TITLE_SCALARS, MAX_PROMPT_PAGE_SIZE, MAX_PROMPT_TAGS, MAX_PROMPT_VARIABLES,
};

const BUSY_TIMEOUT_MS: u64 = 5_000;
const MAX_PROMPT_JOURNAL_ROWS: usize = 10_000;
/// Maximum bytes retained in the durable prompt command/event journal.
///
/// The public codec contract has a larger 4 MiB budget. Durable SQLite rows
/// remain bounded to 512 KiB here.
const MAX_PROMPT_DURABLE_WIRE_BYTES: usize = 512 * 1024;
// Keep temporary positions outside the valid dense prefix while a single
// ordered mutation shifts rows. The chain's durable maximum is 2,000 links,
// so this offset cannot overlap a valid position and avoids row-by-row
// delete/reinsert renumbering.
const CHAIN_POSITION_SHIFT: i64 = MAX_PROMPT_CHAIN_LINKS as i64 + 1;
const CURRENT_VERSION_LATEST_TRIGGER_SQL: &str =
    "CREATE TRIGGER saved_prompts_current_version_is_latest\n\
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
END;";
const CURRENT_VERSION_LATEST_INSERT_TRIGGER_SQL: &str =
    "CREATE TRIGGER saved_prompts_current_version_is_latest_insert\n\
  BEFORE INSERT ON saved_prompts\n\
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
END;";

#[derive(Debug, Clone, PartialEq, Eq)]
struct DecodedPromptChainCommand {
    original_command: PromptChainCommand,
    original_command_sha256: [u8; 32],
    command: PromptChainCommand,
    resolved_prompt_version_id: Option<PromptVersionId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptStoreError {
    Database(String),
    ConstraintViolation,
    Corruption(String),
    NotFound,
    AlreadyExists,
    InvalidTransition,
    RevisionConflict { expected: u64, actual: u64 },
    IdempotencyConflict,
    Validation(PromptValidationError),
}

impl fmt::Display for PromptStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(message) => write!(f, "prompt store database error: {message}"),
            Self::ConstraintViolation => f.write_str("prompt store constraint violation"),
            Self::Corruption(message) => write!(f, "prompt store corruption: {message}"),
            Self::NotFound => f.write_str("prompt not found"),
            Self::AlreadyExists => f.write_str("prompt already exists"),
            Self::InvalidTransition => f.write_str("invalid prompt transition"),
            Self::RevisionConflict { expected, actual } => write!(
                f,
                "prompt revision conflict: expected {expected}, current {actual}"
            ),
            Self::IdempotencyConflict => {
                f.write_str("command id was already used for another prompt command")
            }
            Self::Validation(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for PromptStoreError {}

impl From<PromptValidationError> for PromptStoreError {
    fn from(error: PromptValidationError) -> Self {
        Self::Validation(error)
    }
}

impl From<rusqlite::Error> for PromptStoreError {
    fn from(error: rusqlite::Error) -> Self {
        match error {
            rusqlite::Error::SqliteFailure(code, _) => {
                if code.code == rusqlite::ErrorCode::ConstraintViolation {
                    Self::ConstraintViolation
                } else if matches!(
                    code.code,
                    rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                ) {
                    Self::Database("sqlite busy".into())
                } else {
                    Self::Database("sqlite operation failed".into())
                }
            }
            _ => Self::Database("sqlite operation failed".into()),
        }
    }
}

pub struct PromptStore {
    conn: Connection,
}

impl fmt::Debug for PromptStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PromptStore")
    }
}

impl PromptStore {
    /// Open an isolated prompt view over a kernel SQLite database.
    ///
    /// Opening through `KernelStore` applies the compiled, ordered migration
    /// manifest. This store owns only the prompt command/event transaction
    /// surface; the task CommandBus remains a later integration seam.
    pub fn open(path: &Path) -> Result<Self, PromptStoreError> {
        crate::kernel::KernelStore::open(path).map_err(|error| {
            PromptStoreError::Database(format!("prompt database initialization failed: {error}"))
        })?;
        let mut conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_millis(BUSY_TIMEOUT_MS))?;
        let _: String = conn.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))?;
        conn.execute_batch("PRAGMA foreign_keys=ON; PRAGMA synchronous=NORMAL;")?;
        // Begin the validation snapshot before reading any state, quarantine,
        // journal, payload, or projection rows. IMMEDIATE prevents a writer
        // from changing the facts between a check and the lineage replay.
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let blocked: Option<i64> = tx
            .query_row(
                "SELECT blocked FROM prompt_lineage_migration_state
                 WHERE singleton_key = 1",
                [],
                |row| row.get(0),
            )
            .optional()?;
        let Some(blocked) = blocked else {
            return Err(PromptStoreError::Corruption(
                "prompt lineage migration state is missing; exact repair is required".into(),
            ));
        };
        // Preflight all durable blobs before the quarantine repair proof can
        // materialize any command payload from SQLite.
        validate_prompt_wire_lengths(&tx)?;
        validate_prompt_lineage_quarantine_ledger(&tx)?;
        let quarantine_count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM prompt_lineage_quarantine",
            [],
            |row| row.get(0),
        )?;
        let missing_payload_count: i64 = tx.query_row(
            "SELECT
                (SELECT COUNT(*) FROM prompt_command_receipts
                 WHERE command_payload IS NULL OR length(command_payload) = 0)
                + (SELECT COUNT(*) FROM prompt_chain_command_receipts
                   WHERE command_payload IS NULL OR length(command_payload) = 0)",
            [],
            |row| row.get(0),
        )?;
        let derived_blocked = if quarantine_count != 0 || missing_payload_count != 0 {
            1
        } else {
            0
        };
        if blocked != derived_blocked {
            return Err(PromptStoreError::Corruption(
                "prompt lineage migration state disagrees with derived repair state".into(),
            ));
        }
        if blocked != 0 {
            return Err(PromptStoreError::Corruption(
                "legacy prompt lineage is quarantined and requires exact repair".into(),
            ));
        }
        if quarantine_count != 0 || missing_payload_count != 0 {
            return Err(PromptStoreError::Corruption(
                "prompt lineage has unrepaired quarantine or missing command payloads".into(),
            ));
        }
        validate_prompt_journal_bounds(&tx)?;
        validate_prompt_command_payloads(&tx)?;
        validate_prompt_lineage(&tx)?;
        tx.rollback()?;
        Ok(Self { conn })
    }

    /// Run a public read against one SQLite snapshot. A deferred transaction
    /// starts at the first statement, then keeps that snapshot for every
    /// nested row, tag, variable, context, and validation query. Mutation and
    /// projection-rebuild callers already own their Immediate transaction and
    /// call the same helpers directly rather than nesting a read transaction.
    fn with_read_transaction<T>(
        &self,
        body: impl FnOnce(&Transaction<'_>) -> Result<T, PromptStoreError>,
    ) -> Result<T, PromptStoreError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        let value = body(&tx)?;
        tx.commit()?;
        Ok(value)
    }

    /// Store-suite mutation surface. Production host settlement goes through
    /// [`crate::kernel::CommandBus`]; do not treat this as a product API.
    #[doc(hidden)]
    pub fn execute(
        &mut self,
        command_id: CommandId,
        command: PromptCommand,
    ) -> Result<PromptMutationReceipt, PromptStoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let receipt = execute_prompt_command_in_tx(&tx, command_id, command)?;
        tx.commit()?;
        Ok(receipt)
    }

    /// Store-suite compatibility wrapper for the host-routable chain command.
    #[doc(hidden)]
    pub fn execute_chain(
        &mut self,
        command_id: CommandId,
        command: PromptChainCommand,
    ) -> Result<PromptChainMutationReceipt, PromptStoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let receipt = Self::execute_prompt_chain_command_in_tx(&tx, command_id, command)?;
        tx.commit()?;
        Ok(receipt)
    }

    /// Execute one chain mutation inside a caller-owned immediate transaction.
    ///
    /// The host command bus uses this seam to persist the chain receipt/event
    /// atomically with its authenticated command receipt and operation facts.
    /// The store's standalone [`Self::execute_chain`] wrapper remains the
    /// compatibility API for store tests and tools.
    pub(crate) fn execute_prompt_chain_command_in_tx(
        tx: &Transaction<'_>,
        command_id: CommandId,
        command: PromptChainCommand,
    ) -> Result<PromptChainMutationReceipt, PromptStoreError> {
        let requested_command = command.canonicalize()?;
        requested_command.validate()?;

        if let Some((stored_hash, stored_payload)) = load_chain_command_payload(&tx, command_id)? {
            let stored_command = decode_chain_command(&stored_payload)?;
            let requested_original_hash = requested_command
                .fingerprint()
                .map_err(|error| PromptStoreError::Corruption(error.to_string()))?;
            let stored_original_hash = stored_command.original_command_sha256;
            if requested_original_hash != stored_original_hash
                || requested_command != stored_command.original_command
            {
                return Err(PromptStoreError::IdempotencyConflict);
            }
            let stored_sha256 = digest_from_bytes(&stored_hash)?;
            let receipt = load_chain_receipt(
                &tx,
                command_id,
                &stored_command.original_command,
                &stored_command.command,
                &stored_sha256,
                stored_command.resolved_prompt_version_id,
            )?
            .ok_or_else(|| {
                PromptStoreError::Corruption(
                    "prompt chain receipt disappeared while replaying its command".into(),
                )
            })?;
            return Ok(receipt);
        }

        let (command, resolved_prompt_version_id) =
            resolve_chain_command(requested_command.clone(), &tx)?;
        let command_payload =
            encode_chain_command(&requested_command, &command, resolved_prompt_version_id)?;
        let command_sha256 = sha256_bytes(&command_payload);
        let (receipt, event, chain_id, occurred_at_ms) =
            apply_chain_command(&tx, command_id, &command)?;
        let receipt_payload = receipt
            .encode()
            .map_err(|error| PromptStoreError::Corruption(error.to_string()))?;
        validate_wire_size("prompt chain receipt", &receipt_payload)?;
        tx.execute(
            "INSERT INTO prompt_chain_command_receipts(
                command_id, command_sha256, command_payload, chain_id, chain_link_id,
                revision, receipt, created_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                command_id.as_bytes().as_slice(),
                command_sha256.as_slice(),
                command_payload,
                chain_id.as_bytes().as_slice(),
                receipt.link_id.map(|id| id.as_bytes().as_slice().to_vec()),
                to_i64(receipt.revision)?,
                receipt_payload,
                occurred_at_ms,
            ],
        )?;
        if let Some(event) = event {
            let event_payload = event
                .encode()
                .map_err(|error| PromptStoreError::Corruption(error.to_string()))?;
            validate_wire_size("prompt chain event", &event_payload)?;
            tx.execute(
                "INSERT INTO prompt_chain_events(
                    prompt_chain_event_id, command_id, chain_id, event_type,
                    occurred_at_ms, payload
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![
                    EventId::new().as_bytes().as_slice(),
                    command_id.as_bytes().as_slice(),
                    chain_id.as_bytes().as_slice(),
                    event.event_type(),
                    occurred_at_ms,
                    event_payload,
                ],
            )?;
        }
        Ok(receipt)
    }

    #[doc(hidden)]
    pub fn execute_chain_command(
        &mut self,
        command_id: CommandId,
        command: PromptChainCommand,
    ) -> Result<PromptChainMutationReceipt, PromptStoreError> {
        self.execute_chain(command_id, command)
    }

    pub fn get_chain(
        &self,
        chain_id: PromptChainId,
    ) -> Result<Option<PromptChain>, PromptStoreError> {
        self.with_read_transaction(|tx| load_chain(tx, chain_id))
    }

    pub fn get_prompt_chain(
        &self,
        chain_id: PromptChainId,
    ) -> Result<Option<PromptChain>, PromptStoreError> {
        self.get_chain(chain_id)
    }

    pub fn list_chain_links(
        &self,
        chain_id: PromptChainId,
    ) -> Result<Vec<PromptChainLink>, PromptStoreError> {
        self.with_read_transaction(|tx| {
            if load_chain(tx, chain_id)?.is_none() {
                return Err(PromptStoreError::NotFound);
            }
            let links = load_chain_links(tx, chain_id)?;
            validate_chain_links(tx, chain_id, &links)?;
            Ok(links)
        })
    }

    pub fn list_prompt_chain_links(
        &self,
        chain_id: PromptChainId,
    ) -> Result<Vec<PromptChainLink>, PromptStoreError> {
        self.list_chain_links(chain_id)
    }

    pub fn get_chain_link_context(
        &self,
        chain_id: PromptChainId,
        link_id: PromptChainLinkId,
    ) -> Result<Option<PromptChainLinkContext>, PromptStoreError> {
        self.with_read_transaction(|tx| {
            if load_chain(tx, chain_id)?.is_none() {
                return Err(PromptStoreError::NotFound);
            }
            let links = load_chain_links(tx, chain_id)?;
            validate_chain_links(tx, chain_id, &links)?;
            let Some(index) = links.iter().position(|link| link.id() == link_id) else {
                return Ok(None);
            };
            let link = links[index].clone();
            let current_version = load_prompt(tx, link.prompt_id())?
                .ok_or_else(|| {
                    PromptStoreError::Corruption("chain link references missing prompt".into())
                })?
                .current_version_id;
            Ok(Some(PromptChainLinkContext {
                link,
                previous_link_id: index.checked_sub(1).map(|i| links[i].id()),
                next_link_id: links.get(index + 1).map(|link| link.id()),
                update_available: current_version != links[index].prompt_version_id(),
            }))
        })
    }

    pub fn count_chain_events(&self) -> Result<u64, PromptStoreError> {
        self.with_read_transaction(|tx| {
            let count: i64 =
                tx.query_row("SELECT COUNT(*) FROM prompt_chain_events", [], |row| {
                    row.get(0)
                })?;
            u64::try_from(count).map_err(|_| {
                PromptStoreError::Corruption("negative prompt chain event count".into())
            })
        })
    }

    pub fn get_prompt(&self, prompt_id: PromptId) -> Result<Option<SavedPrompt>, PromptStoreError> {
        self.with_read_transaction(|tx| {
            let prompt = load_prompt(tx, prompt_id)?;
            if let Some(prompt) = &prompt {
                validate_saved_prompt_record(tx, prompt)?;
            }
            Ok(prompt)
        })
    }

    pub fn get_version(
        &self,
        version_id: PromptVersionId,
    ) -> Result<Option<PromptVersion>, PromptStoreError> {
        self.with_read_transaction(|tx| load_version(tx, version_id))
    }

    pub fn list_prompts(
        &self,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<SavedPrompt>, PromptStoreError> {
        self.with_read_transaction(|tx| list_prompts_tx(tx, offset, limit))
    }

    /// Durable current-state library revision. Sum of prompt and chain row
    /// revisions is independent of compacted event journals.
    pub fn library_projection_revision(&self) -> Result<u64, PromptStoreError> {
        self.with_read_transaction(library_projection_revision_in_tx)
    }

    pub fn list_prompts_after(
        &self,
        after: Option<PromptId>,
        limit: usize,
    ) -> Result<Vec<SavedPrompt>, PromptStoreError> {
        self.with_read_transaction(|tx| list_prompts_after_tx(tx, after, limit))
    }

    pub fn list_chain_links_after(
        &self,
        chain_id: PromptChainId,
        after: Option<PromptChainLinkId>,
        limit: usize,
    ) -> Result<Vec<PromptChainLink>, PromptStoreError> {
        self.with_read_transaction(|tx| list_chain_links_after_tx(tx, chain_id, after, limit))
    }

    pub(crate) fn snapshot(
        &self,
        offset: usize,
        limit: usize,
    ) -> Result<PromptSnapshot, PromptStoreError> {
        self.with_read_transaction(|tx| {
            let prompts = list_prompts_tx(tx, offset, limit)?;
            let next_offset = (prompts.len() == limit).then_some(offset + prompts.len());
            Ok(PromptSnapshot {
                prompts,
                next_offset,
            })
        })
    }

    pub(crate) fn global_snapshot(
        &self,
        _offset: usize,
        _limit: usize,
    ) -> Result<PromptSnapshot, PromptStoreError> {
        Err(PromptStoreError::InvalidTransition)
    }

    pub fn list_versions(
        &self,
        prompt_id: PromptId,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<PromptVersion>, PromptStoreError> {
        self.with_read_transaction(|tx| list_versions_tx(tx, prompt_id, offset, limit))
    }

    pub fn count_prompts(&self) -> Result<u64, PromptStoreError> {
        self.with_read_transaction(|tx| {
            let count: i64 =
                tx.query_row("SELECT COUNT(*) FROM saved_prompts", [], |row| row.get(0))?;
            u64::try_from(count)
                .map_err(|_| PromptStoreError::Corruption("negative prompt count".into()))
        })
    }

    pub fn count_prompt_events(&self) -> Result<u64, PromptStoreError> {
        self.with_read_transaction(|tx| {
            let count: i64 =
                tx.query_row("SELECT COUNT(*) FROM prompt_events", [], |row| row.get(0))?;
            u64::try_from(count)
                .map_err(|_| PromptStoreError::Corruption("negative prompt event count".into()))
        })
    }

    pub fn rebuild_projection(&mut self) -> Result<PromptProjectionRebuild, PromptStoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let prompt_event_count = bounded_count(&tx, "prompt_events")?;
        let chain_event_count = bounded_count(&tx, "prompt_chain_events")?;
        let prompt_receipt_count = bounded_count(&tx, "prompt_command_receipts")?;
        let chain_receipt_count = bounded_count(&tx, "prompt_chain_command_receipts")?;
        let prompt_version_count = bounded_count(&tx, "prompt_versions")?;
        let journal_rows = prompt_event_count
            .checked_add(chain_event_count)
            .and_then(|count| count.checked_add(prompt_receipt_count))
            .and_then(|count| count.checked_add(chain_receipt_count))
            .and_then(|count| count.checked_add(prompt_version_count))
            .ok_or_else(|| PromptStoreError::Corruption("prompt journal count overflow".into()))?;
        if journal_rows > MAX_PROMPT_JOURNAL_ROWS {
            return Err(PromptStoreError::Corruption(format!(
                "prompt journal exceeds maximum of {MAX_PROMPT_JOURNAL_ROWS} rows"
            )));
        }
        validate_prompt_wire_lengths(&tx)?;
        validate_current_version_projection(&tx)?;
        validate_event_sequence_domain(&tx, "prompt_events")?;
        validate_event_sequence_domain(&tx, "prompt_chain_events")?;
        tx.execute_batch(
            "DROP TRIGGER IF EXISTS saved_prompts_current_version_is_latest;\n\
             DROP TRIGGER IF EXISTS saved_prompts_current_version_is_latest_insert;",
        )?;

        tx.execute_batch(
            "DELETE FROM prompt_chain_links;
             DELETE FROM prompt_chains;
             DELETE FROM prompt_tags;
             DELETE FROM saved_prompts;",
        )?;

        let mut prompt_events_replayed = 0usize;
        let mut last_sequence = 0i64;
        let mut expected_sequence = 1i64;
        let mut event_version_ids =
            HashSet::with_capacity(prompt_event_count.min(MAX_PROMPT_JOURNAL_ROWS));
        let mut prompt_version_cursors =
            HashMap::with_capacity(prompt_event_count.min(MAX_PROMPT_JOURNAL_ROWS));
        while let Some((
            sequence,
            event_id,
            command_id,
            prompt_id,
            event_type,
            occurred_at_ms,
            payload,
        )) = next_prompt_event_row(&tx, last_sequence)?
        {
            if sequence != expected_sequence {
                return Err(PromptStoreError::Corruption(
                    "prompt event sequence is not a contiguous prefix".into(),
                ));
            }
            last_sequence = sequence;
            expected_sequence = expected_sequence.checked_add(1).ok_or_else(|| {
                PromptStoreError::Corruption("prompt event sequence overflow".into())
            })?;
            let (event, occurred_at_ms, receipt_version_id) = validate_prompt_event_row(
                &tx,
                &event_id,
                &command_id,
                &prompt_id,
                &event_type,
                occurred_at_ms,
                &payload,
            )?;
            match &event {
                PromptEvent::PromptCreated { version, .. }
                | PromptEvent::PromptVersionCreated { version, .. } => {
                    event_version_ids.insert(version.id);
                }
                _ => {}
            }
            validate_prompt_event_temporal_lineage(
                &tx,
                &event,
                receipt_version_id,
                &mut prompt_version_cursors,
            )?;
            apply_event(&tx, &event, occurred_at_ms)?;
            prompt_events_replayed = prompt_events_replayed.checked_add(1).ok_or_else(|| {
                PromptStoreError::Corruption("prompt event count overflow".into())
            })?;
        }

        validate_prompt_version_event_coverage(&tx, &event_version_ids)?;
        tx.execute_batch(CURRENT_VERSION_LATEST_TRIGGER_SQL)?;
        tx.execute_batch(CURRENT_VERSION_LATEST_INSERT_TRIGGER_SQL)?;

        let mut chain_events_replayed = 0usize;
        let mut last_chain_sequence = 0i64;
        let mut expected_chain_sequence = 1i64;
        while let Some((
            sequence,
            event_id,
            command_id,
            chain_id,
            event_type,
            occurred_at_ms,
            payload,
        )) = next_chain_event_row(&tx, last_chain_sequence)?
        {
            if sequence != expected_chain_sequence {
                return Err(PromptStoreError::Corruption(
                    "prompt chain event sequence is not a contiguous prefix".into(),
                ));
            }
            last_chain_sequence = sequence;
            expected_chain_sequence = expected_chain_sequence.checked_add(1).ok_or_else(|| {
                PromptStoreError::Corruption("prompt chain event sequence overflow".into())
            })?;
            let (event, occurred_at_ms, command, resolved_prompt_version_id) =
                validate_chain_event_row(
                    &tx,
                    &event_id,
                    &command_id,
                    &chain_id,
                    &event_type,
                    occurred_at_ms,
                    &payload,
                )?;
            apply_chain_event(
                &tx,
                &event,
                occurred_at_ms,
                &command,
                resolved_prompt_version_id,
            )?;
            chain_events_replayed = chain_events_replayed.checked_add(1).ok_or_else(|| {
                PromptStoreError::Corruption("prompt chain event count overflow".into())
            })?;
        }

        validate_all_prompt_receipts(&tx)?;
        validate_all_chain_receipts(&tx)?;
        let events_replayed = prompt_events_replayed
            .checked_add(chain_events_replayed)
            .ok_or_else(|| PromptStoreError::Corruption("prompt event count overflow".into()))?;
        tx.commit()?;
        Ok(PromptProjectionRebuild {
            events_replayed: u64::try_from(events_replayed).map_err(|_| {
                PromptStoreError::Corruption("prompt event count is out of range".into())
            })?,
        })
    }
}

fn list_prompts_after_tx(
    tx: &Transaction<'_>,
    after: Option<PromptId>,
    limit: usize,
) -> Result<Vec<SavedPrompt>, PromptStoreError> {
    validate_page(limit)?;
    let Some(after_id) = after else {
        return list_prompts_tx(tx, 0, limit);
    };
    let Some(after_prompt) = load_prompt(tx, after_id)? else {
        return Err(PromptStoreError::NotFound);
    };
    let archived_rank: i64 = if after_prompt.archived_at_ms.is_some() {
        1
    } else {
        0
    };
    let mut statement = tx.prepare(
        "SELECT prompt_id, title, description, current_version_id, revision,
                archived_at_ms
         FROM saved_prompts
         WHERE (
            CASE WHEN archived_at_ms IS NULL THEN 0 ELSE 1 END > ?1
            OR (
                CASE WHEN archived_at_ms IS NULL THEN 0 ELSE 1 END = ?1
                AND (
                    title COLLATE NOCASE > ?2
                    OR (title COLLATE NOCASE = ?2 AND prompt_id > ?3)
                )
            )
         )
         ORDER BY CASE WHEN archived_at_ms IS NULL THEN 0 ELSE 1 END,
                  title COLLATE NOCASE ASC, prompt_id ASC
         LIMIT ?4",
    )?;
    let rows = statement.query_map(
        rusqlite::params![
            archived_rank,
            after_prompt.title,
            after_id.as_bytes().as_slice(),
            to_i64(limit)?,
        ],
        |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Vec<u8>>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, Option<i64>>(5)?,
            ))
        },
    )?;
    let mut prompts = Vec::new();
    for row in rows {
        let (id, title, description, current_version_id, revision, archived_at_ms) = row?;
        let prompt = SavedPrompt {
            id: prompt_id_from_bytes(&id)?,
            title,
            description,
            tags: load_tags(tx, &id)?,
            current_version_id: version_id_from_bytes(&current_version_id)?,
            revision: from_i64("saved_prompts.revision", revision)?,
            archived_at_ms,
        };
        validate_saved_prompt_record(tx, &prompt)?;
        prompts.push(prompt);
    }
    Ok(prompts)
}

fn list_chain_links_after_tx(
    tx: &Transaction<'_>,
    chain_id: PromptChainId,
    after: Option<PromptChainLinkId>,
    limit: usize,
) -> Result<Vec<PromptChainLink>, PromptStoreError> {
    validate_page(limit)?;
    if load_chain(tx, chain_id)?.is_none() {
        return Err(PromptStoreError::NotFound);
    }
    let links = load_chain_links(tx, chain_id)?;
    validate_chain_links(tx, chain_id, &links)?;
    let start = match after {
        None => 0,
        Some(after_id) => links
            .iter()
            .position(|link| link.id() == after_id)
            .map(|index| index.saturating_add(1))
            .ok_or(PromptStoreError::NotFound)?,
    };
    Ok(links.into_iter().skip(start).take(limit).collect())
}

fn list_prompts_tx(
    tx: &Transaction<'_>,
    offset: usize,
    limit: usize,
) -> Result<Vec<SavedPrompt>, PromptStoreError> {
    validate_page(limit)?;
    let mut statement = tx.prepare(
        "SELECT prompt_id, title, description, current_version_id, revision,
                archived_at_ms
         FROM saved_prompts
         ORDER BY CASE WHEN archived_at_ms IS NULL THEN 0 ELSE 1 END,
                  title COLLATE NOCASE ASC, prompt_id ASC
         LIMIT ?1 OFFSET ?2",
    )?;
    let rows = statement.query_map(rusqlite::params![to_i64(limit)?, to_i64(offset)?], |row| {
        Ok((
            row.get::<_, Vec<u8>>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, Vec<u8>>(3)?,
            row.get::<_, i64>(4)?,
            row.get::<_, Option<i64>>(5)?,
        ))
    })?;
    let mut prompts = Vec::new();
    for row in rows {
        let (id, title, description, current_version_id, revision, archived_at_ms) = row?;
        let prompt = SavedPrompt {
            id: prompt_id_from_bytes(&id)?,
            title,
            description,
            tags: load_tags(tx, &id)?,
            current_version_id: version_id_from_bytes(&current_version_id)?,
            revision: from_i64("saved_prompts.revision", revision)?,
            archived_at_ms,
        };
        validate_saved_prompt_record(tx, &prompt)?;
        prompts.push(prompt);
    }
    Ok(prompts)
}

fn list_versions_tx(
    tx: &Transaction<'_>,
    prompt_id: PromptId,
    offset: usize,
    limit: usize,
) -> Result<Vec<PromptVersion>, PromptStoreError> {
    validate_page(limit)?;
    let Some(prompt) = load_prompt(tx, prompt_id)? else {
        return Err(PromptStoreError::NotFound);
    };
    validate_saved_prompt_record(tx, &prompt)?;
    let mut statement = tx.prepare(
        "SELECT prompt_version_id, prompt_id, version, body, body_sha256, created_at_ms,
                variables_sealed
         FROM prompt_versions
         WHERE prompt_id = ?1
         ORDER BY version DESC
         LIMIT ?2 OFFSET ?3",
    )?;
    let rows = statement.query_map(
        rusqlite::params![
            prompt_id.as_bytes().as_slice(),
            to_i64(limit)?,
            to_i64(offset)?,
        ],
        |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Vec<u8>>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, i64>(6)?,
            ))
        },
    )?;
    rows.map(|row| {
        let (id, owner, version, body, body_sha256, created_at_ms, variables_sealed) = row?;
        if variables_sealed != 1 {
            return Err(PromptStoreError::Corruption(
                "prompt version variables are not sealed".into(),
            ));
        }
        let version = PromptVersion {
            id: version_id_from_bytes(&id)?,
            prompt_id: prompt_id_from_bytes(&owner)?,
            version: u32::try_from(version).map_err(|_| {
                PromptStoreError::Corruption("prompt version number is out of range".into())
            })?,
            body,
            variables: load_version_variables(tx, &id)?,
            body_sha256: digest_from_bytes(&body_sha256)?,
            created_at_ms,
        };
        validate_version_record(&version)?;
        Ok(version)
    })
    .collect()
}

fn bounded_count(tx: &Transaction<'_>, table: &str) -> Result<usize, PromptStoreError> {
    let sql = match table {
        "prompt_events"
        | "prompt_chain_events"
        | "prompt_command_receipts"
        | "prompt_chain_command_receipts"
        | "prompt_versions" => format!("SELECT COUNT(*) FROM {table}"),
        _ => {
            return Err(PromptStoreError::Corruption(
                "unsupported prompt journal table".into(),
            ))
        }
    };
    let count: i64 = tx.query_row(&sql, [], |row| row.get(0))?;
    let count = usize::try_from(count)
        .map_err(|_| PromptStoreError::Corruption("negative prompt journal count".into()))?;
    if count > MAX_PROMPT_JOURNAL_ROWS {
        return Ok(count);
    }
    Ok(count)
}

fn validate_current_version_projection(tx: &Transaction<'_>) -> Result<(), PromptStoreError> {
    let has_stale_current_version: i64 = tx.query_row(
        "SELECT EXISTS(
            SELECT 1
            FROM saved_prompts AS prompt
            WHERE NOT EXISTS (
                SELECT 1
                FROM prompt_versions AS candidate
                WHERE candidate.prompt_id = prompt.prompt_id
                  AND candidate.prompt_version_id = prompt.current_version_id
                  AND candidate.version = (
                      SELECT MAX(latest.version)
                      FROM prompt_versions AS latest
                      WHERE latest.prompt_id = prompt.prompt_id
                  )
            )
        )",
        [],
        |row| row.get(0),
    )?;
    if has_stale_current_version != 0 {
        return Err(PromptStoreError::Corruption(
            "saved prompt current version is not the latest durable version".into(),
        ));
    }
    Ok(())
}

fn validate_prompt_command_payloads(tx: &Transaction<'_>) -> Result<(), PromptStoreError> {
    let mut statement = tx.prepare(
        "SELECT command_sha256, command_payload
         FROM prompt_command_receipts
         ORDER BY command_id ASC",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Option<Vec<u8>>>(1)?))
    })?;
    for row in rows {
        let (stored_hash, payload) = row?;
        let payload = payload.ok_or_else(|| {
            PromptStoreError::Corruption(
                "prompt receipt command payload is missing after migration repair".into(),
            )
        })?;
        validate_wire_size("prompt command", &payload)?;
        if stored_hash.len() != 32 || sha256_bytes(&payload).as_slice() != stored_hash.as_slice() {
            return Err(PromptStoreError::Corruption(
                "prompt receipt command payload does not match its digest".into(),
            ));
        }
        let command = PromptCommand::decode(&payload)
            .map_err(|error| PromptStoreError::Corruption(error.to_string()))?;
        if command
            .encode()
            .map_err(|error| PromptStoreError::Corruption(error.to_string()))?
            != payload
        {
            return Err(PromptStoreError::Corruption(
                "prompt receipt command payload is not canonical".into(),
            ));
        }
    }

    let mut statement = tx.prepare(
        "SELECT command_sha256, command_payload
         FROM prompt_chain_command_receipts
         ORDER BY command_id ASC",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Option<Vec<u8>>>(1)?))
    })?;
    for row in rows {
        let (stored_hash, payload) = row?;
        let payload = payload.ok_or_else(|| {
            PromptStoreError::Corruption(
                "prompt chain receipt command payload is missing after migration repair".into(),
            )
        })?;
        validate_wire_size("prompt chain command", &payload)?;
        if stored_hash.len() != 32 || sha256_bytes(&payload).as_slice() != stored_hash.as_slice() {
            return Err(PromptStoreError::Corruption(
                "prompt chain receipt command payload does not match its digest".into(),
            ));
        }
        let decoded = decode_chain_command(&payload)?;
        if encode_chain_command(
            &decoded.original_command,
            &decoded.command,
            decoded.resolved_prompt_version_id,
        )? != payload
        {
            return Err(PromptStoreError::Corruption(
                "prompt chain receipt command payload is not canonical".into(),
            ));
        }
    }
    Ok(())
}

fn validate_prompt_journal_bounds(tx: &Transaction<'_>) -> Result<(), PromptStoreError> {
    let mut journal_rows = 0usize;
    for table in [
        "prompt_events",
        "prompt_chain_events",
        "prompt_command_receipts",
        "prompt_chain_command_receipts",
        "prompt_versions",
    ] {
        let count: i64 = tx.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })?;
        let count = usize::try_from(count)
            .map_err(|_| PromptStoreError::Corruption("negative prompt journal count".into()))?;
        journal_rows = journal_rows
            .checked_add(count)
            .ok_or_else(|| PromptStoreError::Corruption("prompt journal count overflow".into()))?;
        if journal_rows > MAX_PROMPT_JOURNAL_ROWS {
            return Err(PromptStoreError::Corruption(format!(
                "prompt journal exceeds maximum of {MAX_PROMPT_JOURNAL_ROWS} rows"
            )));
        }
    }
    Ok(())
}

/// Check blob lengths in SQL before any receipt/event query can allocate a
/// `Vec<u8>`. This is deliberately run inside the same IMMEDIATE snapshot as
/// all later lineage checks; a trigger protects new writes while this catches
/// oversized rows already present in a migrated database.
fn validate_prompt_wire_lengths(tx: &Transaction<'_>) -> Result<(), PromptStoreError> {
    for (table, column, kind) in [
        (
            "prompt_command_receipts",
            "command_payload",
            "prompt receipt command payload",
        ),
        (
            "prompt_chain_command_receipts",
            "command_payload",
            "prompt chain receipt command payload",
        ),
        ("prompt_command_receipts", "receipt", "prompt receipt"),
        (
            "prompt_chain_command_receipts",
            "receipt",
            "prompt chain receipt",
        ),
        ("prompt_events", "payload", "prompt event"),
        ("prompt_chain_events", "payload", "prompt chain event"),
    ] {
        let oversized: i64 = tx.query_row(
            &format!(
                "SELECT COUNT(*) FROM {table}
                 WHERE {column} IS NOT NULL
                   AND length(CAST({column} AS BLOB)) > {MAX_PROMPT_DURABLE_WIRE_BYTES}"
            ),
            [],
            |row| row.get(0),
        )?;
        if oversized < 0 {
            return Err(PromptStoreError::Corruption(
                "negative oversized prompt payload count".into(),
            ));
        }
        if oversized > 0 {
            return Err(PromptStoreError::Corruption(format!(
                "{kind} payload exceeds maximum durable size of {MAX_PROMPT_DURABLE_WIRE_BYTES} bytes"
            )));
        }
    }
    Ok(())
}

type QuarantineLedgerRow = (i64, String, Vec<u8>, Option<Vec<u8>>, String, Vec<u8>, i64);

fn validate_prompt_lineage_repair_audit(tx: &Transaction<'_>) -> Result<(), PromptStoreError> {
    let audit_count: i64 = tx.query_row(
        "SELECT COUNT(*) FROM prompt_lineage_quarantine_repair_audit",
        [],
        |row| row.get(0),
    )?;
    let audit_count = usize::try_from(audit_count)
        .map_err(|_| PromptStoreError::Corruption("negative lineage repair audit count".into()))?;
    if audit_count > MAX_PROMPT_JOURNAL_ROWS {
        return Err(PromptStoreError::Corruption(format!(
            "prompt lineage repair audit exceeds maximum of {MAX_PROMPT_JOURNAL_ROWS} rows"
        )));
    }
    let mut statement = tx.prepare(
        "SELECT repair_id, quarantine_id, source_kind, command_id, event_id,
                reason, command_sha256, quarantined_at_ms, origin
         FROM prompt_lineage_quarantine_repair_audit
         ORDER BY repair_id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, Vec<u8>>(3)?,
            row.get::<_, Option<Vec<u8>>>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, Vec<u8>>(6)?,
            row.get::<_, i64>(7)?,
            row.get::<_, String>(8)?,
        ))
    })?;
    for (index, row) in rows.enumerate() {
        let (
            repair_id,
            quarantine_id,
            source_kind,
            command_id,
            event_id,
            reason,
            command_sha256,
            quarantined_at_ms,
            origin,
        ) = row?;
        let expected_repair_id = i64::try_from(index + 1)
            .map_err(|_| PromptStoreError::Corruption("lineage repair audit overflow".into()))?;
        if repair_id != expected_repair_id
            || origin != "quarantine_delete"
            || quarantine_id <= 0
            || command_id.len() != 16
            || event_id.as_ref().is_some_and(|id| id.len() != 16)
            || command_sha256.len() != 32
        {
            return Err(PromptStoreError::Corruption(
                "prompt lineage repair audit provenance is invalid".into(),
            ));
        }
        let ledger = (
            quarantine_id,
            source_kind,
            command_id,
            event_id,
            reason,
            command_sha256,
            quarantined_at_ms,
        );
        let current_quarantine: Option<i64> = tx
            .query_row(
                "SELECT quarantine_id FROM prompt_lineage_quarantine
                 WHERE quarantine_id = ?1",
                [quarantine_id],
                |row| row.get(0),
            )
            .optional()?;
        if current_quarantine.is_some() {
            return Err(PromptStoreError::Corruption(
                "prompt lineage repair audit records a non-deleted quarantine row".into(),
            ));
        }
        let current_ledger: Option<QuarantineLedgerRow> = tx
            .query_row(
                "SELECT quarantine_id, source_kind, command_id, event_id, reason,
                        command_sha256, quarantined_at_ms
                 FROM prompt_lineage_quarantine_ledger
                 WHERE quarantine_id = ?1",
                [quarantine_id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                },
            )
            .optional()?;
        if current_ledger.as_ref() != Some(&ledger) || !quarantine_repair_proof_exists(tx, &ledger)?
        {
            let reason = if is_zero_sha256(&ledger.5) {
                "zero-hash legacy lineage requires explicit external/manual authority and validated supplied bytes"
            } else {
                "prompt lineage repair audit lacks canonical receipt/event proof"
            };
            return Err(PromptStoreError::Corruption(reason.into()));
        }
    }
    Ok(())
}

fn quarantine_repair_audit_exists(
    tx: &Transaction<'_>,
    ledger: &QuarantineLedgerRow,
) -> Result<bool, PromptStoreError> {
    let (
        quarantine_id,
        source_kind,
        command_id,
        event_id,
        reason,
        command_sha256,
        quarantined_at_ms,
    ) = ledger;
    let origin: Option<String> = tx
        .query_row(
            "SELECT origin FROM prompt_lineage_quarantine_repair_audit
             WHERE quarantine_id = ?1
               AND source_kind = ?2
               AND command_id = ?3
               AND (event_id IS ?4)
               AND reason = ?5
               AND command_sha256 = ?6
               AND quarantined_at_ms = ?7",
            rusqlite::params![
                quarantine_id,
                source_kind,
                command_id,
                event_id,
                reason,
                command_sha256,
                quarantined_at_ms,
            ],
            |row| row.get(0),
        )
        .optional()?;
    Ok(origin.as_deref() == Some("quarantine_delete"))
}

fn is_zero_sha256(hash: &[u8]) -> bool {
    hash.len() == 32 && hash.iter().all(|byte| *byte == 0)
}

fn validate_prompt_lineage_creation_commitment(
    tx: &Transaction<'_>,
) -> Result<(), PromptStoreError> {
    let commitment: Option<(i64, i64, i64, i64, i64, Vec<u8>)> = tx
        .query_row(
            "SELECT migration_version, initial_quarantine_count,
                    initial_ledger_count, initial_creation_count, initial_blocked,
                    state_creation_token
             FROM prompt_lineage_migration_commitment
             WHERE singleton_key = 1",
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
        .optional()?;
    let Some((
        migration_version,
        initial_quarantine_count,
        initial_ledger_count,
        initial_creation_count,
        initial_blocked,
        committed_token,
    )) = commitment
    else {
        return Err(PromptStoreError::Corruption(
            "prompt lineage migration creation commitment is missing".into(),
        ));
    };
    if migration_version != 12 || !(0..=1).contains(&initial_blocked) || committed_token.len() != 32
    {
        return Err(PromptStoreError::Corruption(
            "prompt lineage migration creation commitment is invalid".into(),
        ));
    }
    let initial_quarantine_count = usize::try_from(initial_quarantine_count)
        .map_err(|_| PromptStoreError::Corruption("negative initial quarantine count".into()))?;
    let initial_ledger_count = usize::try_from(initial_ledger_count)
        .map_err(|_| PromptStoreError::Corruption("negative initial ledger count".into()))?;
    let initial_creation_count = usize::try_from(initial_creation_count).map_err(|_| {
        PromptStoreError::Corruption("negative initial quarantine creation count".into())
    })?;
    if initial_quarantine_count > MAX_PROMPT_JOURNAL_ROWS
        || initial_ledger_count > MAX_PROMPT_JOURNAL_ROWS
        || initial_creation_count > MAX_PROMPT_JOURNAL_ROWS
        || initial_quarantine_count != initial_ledger_count
        || initial_ledger_count != initial_creation_count
    {
        return Err(PromptStoreError::Corruption(
            "prompt lineage migration creation counts are invalid".into(),
        ));
    }

    let state: Option<(Vec<u8>, i64)> = tx
        .query_row(
            "SELECT creation_token, blocked
             FROM prompt_lineage_migration_state
             WHERE singleton_key = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((state_token, state_blocked)) = state else {
        return Err(PromptStoreError::Corruption(
            "prompt lineage migration state is missing its creation commitment".into(),
        ));
    };
    if state_token.len() != 32
        || state_token != committed_token
        || !(0..=1).contains(&state_blocked)
    {
        return Err(PromptStoreError::Corruption(
            "prompt lineage migration state creation commitment is invalid".into(),
        ));
    }

    let current_quarantine_count: i64 = tx.query_row(
        "SELECT COUNT(*) FROM prompt_lineage_quarantine",
        [],
        |row| row.get(0),
    )?;
    let current_quarantine_count = usize::try_from(current_quarantine_count)
        .map_err(|_| PromptStoreError::Corruption("negative quarantine count".into()))?;
    if current_quarantine_count > MAX_PROMPT_JOURNAL_ROWS {
        return Err(PromptStoreError::Corruption(format!(
            "prompt lineage quarantine exceeds maximum of {MAX_PROMPT_JOURNAL_ROWS} rows"
        )));
    }

    let current_ledger_count: i64 = tx.query_row(
        "SELECT COUNT(*) FROM prompt_lineage_quarantine_ledger",
        [],
        |row| row.get(0),
    )?;
    let current_ledger_count = usize::try_from(current_ledger_count)
        .map_err(|_| PromptStoreError::Corruption("negative lineage ledger count".into()))?;
    if current_ledger_count < initial_ledger_count || current_ledger_count > MAX_PROMPT_JOURNAL_ROWS
    {
        return Err(PromptStoreError::Corruption(
            "prompt lineage ledger count disagrees with its creation commitment".into(),
        ));
    }
    let current_creation_count: i64 = tx.query_row(
        "SELECT COUNT(*) FROM prompt_lineage_quarantine_creation",
        [],
        |row| row.get(0),
    )?;
    let current_creation_count = usize::try_from(current_creation_count)
        .map_err(|_| PromptStoreError::Corruption("negative quarantine creation count".into()))?;
    if current_creation_count != initial_creation_count {
        return Err(PromptStoreError::Corruption(
            "prompt lineage quarantine creation ledger was deleted or recreated".into(),
        ));
    }

    let mut creation_statement = tx.prepare(
        "SELECT quarantine_id, source_kind, command_id, event_id, reason,
                command_sha256, quarantined_at_ms
         FROM prompt_lineage_quarantine_creation
         ORDER BY quarantine_id",
    )?;
    let creation_rows =
        creation_statement.query_map([], |row| -> rusqlite::Result<QuarantineLedgerRow> {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
            ))
        })?;
    let mut creation_rows_vec = Vec::with_capacity(current_creation_count);
    for row in creation_rows {
        creation_rows_vec.push(row?);
    }
    let mut ledger_statement = tx.prepare(
        "SELECT quarantine_id, source_kind, command_id, event_id, reason,
                command_sha256, quarantined_at_ms
         FROM prompt_lineage_quarantine_ledger
         ORDER BY quarantine_id
         LIMIT ?1",
    )?;
    let ledger_rows = ledger_statement.query_map(
        [i64::try_from(initial_ledger_count).map_err(|_| {
            PromptStoreError::Corruption("initial ledger count is out of range".into())
        })?],
        |row| -> rusqlite::Result<QuarantineLedgerRow> {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
            ))
        },
    )?;
    let mut ledger_rows_vec = Vec::with_capacity(initial_ledger_count);
    for row in ledger_rows {
        ledger_rows_vec.push(row?);
    }
    if creation_rows_vec != ledger_rows_vec {
        return Err(PromptStoreError::Corruption(
            "prompt lineage ledger creation provenance was deleted, replaced, or forged".into(),
        ));
    }
    Ok(())
}

fn validate_prompt_lineage_quarantine_ledger(tx: &Transaction<'_>) -> Result<(), PromptStoreError> {
    validate_prompt_lineage_repair_audit(tx)?;
    validate_prompt_lineage_creation_commitment(tx)?;
    let ledger_count: i64 = tx.query_row(
        "SELECT COUNT(*) FROM prompt_lineage_quarantine_ledger",
        [],
        |row| row.get(0),
    )?;
    let ledger_count = usize::try_from(ledger_count)
        .map_err(|_| PromptStoreError::Corruption("negative lineage ledger count".into()))?;
    if ledger_count > MAX_PROMPT_JOURNAL_ROWS {
        return Err(PromptStoreError::Corruption(format!(
            "prompt lineage ledger exceeds maximum of {MAX_PROMPT_JOURNAL_ROWS} rows"
        )));
    }

    let mut statement = tx.prepare(
        "SELECT quarantine_id, source_kind, command_id, event_id, reason,
                command_sha256, quarantined_at_ms
         FROM prompt_lineage_quarantine_ledger
         ORDER BY quarantine_id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get(0)?,
            row.get(1)?,
            row.get(2)?,
            row.get(3)?,
            row.get(4)?,
            row.get(5)?,
            row.get(6)?,
        ))
    })?;
    for (index, row) in rows.enumerate() {
        let ledger: QuarantineLedgerRow = row?;
        let expected_id = i64::try_from(index + 1)
            .map_err(|_| PromptStoreError::Corruption("lineage ledger sequence overflow".into()))?;
        if ledger.0 != expected_id {
            return Err(PromptStoreError::Corruption(
                "prompt lineage ledger ids are not a contiguous migration sequence".into(),
            ));
        }
        let current: Option<QuarantineLedgerRow> = tx
            .query_row(
                "SELECT quarantine_id, source_kind, command_id, event_id, reason,
                        command_sha256, quarantined_at_ms
                 FROM prompt_lineage_quarantine
                 WHERE quarantine_id = ?1",
                [ledger.0],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                },
            )
            .optional()?;
        match current {
            Some(current) if current != ledger => {
                return Err(PromptStoreError::Corruption(
                    "prompt lineage quarantine row disagrees with its immutable ledger".into(),
                ));
            }
            Some(_) => {}
            None if quarantine_repair_audit_exists(tx, &ledger)?
                && quarantine_repair_proof_exists(tx, &ledger)? => {}
            None => {
                let reason = if is_zero_sha256(&ledger.5) {
                    "zero-hash legacy lineage requires explicit external/manual authority and validated supplied bytes"
                } else {
                    "prompt lineage quarantine deletion lacks canonical repair proof"
                };
                return Err(PromptStoreError::Corruption(reason.into()));
            }
        }
    }

    let unknown_quarantine: Option<i64> = tx
        .query_row(
            "SELECT quarantine.quarantine_id
             FROM prompt_lineage_quarantine AS quarantine
             LEFT JOIN prompt_lineage_quarantine_ledger AS ledger
               ON ledger.quarantine_id = quarantine.quarantine_id
             WHERE ledger.quarantine_id IS NULL
             LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if unknown_quarantine.is_some() {
        return Err(PromptStoreError::Corruption(
            "prompt lineage quarantine row has no immutable ledger entry".into(),
        ));
    }
    Ok(())
}

fn quarantine_repair_proof_exists(
    tx: &Transaction<'_>,
    ledger: &QuarantineLedgerRow,
) -> Result<bool, PromptStoreError> {
    let (quarantine_id, source_kind, command_id, event_id, _reason, command_sha256, _at) = ledger;
    if *quarantine_id <= 0 || command_sha256.len() != 32 {
        return Ok(false);
    }
    // A zero digest records that the original durable bytes were unavailable
    // at migration time. Supplied bytes can be checked structurally, but they
    // cannot prove the original receipt/event without explicit external or
    // manual authority. Never treat zero as a wildcard digest.
    if is_zero_sha256(command_sha256) {
        return Ok(false);
    }
    match source_kind.as_str() {
        "prompt_receipt" if event_id.is_none() => {
            let row: Option<(Vec<u8>, Option<Vec<u8>>)> = tx
                .query_row(
                    "SELECT command_sha256, command_payload
                     FROM prompt_command_receipts WHERE command_id = ?1",
                    [command_id.as_slice()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            Ok(row.is_some_and(|(hash, payload)| {
                payload.is_some_and(|payload| {
                    !payload.is_empty() && hash.as_slice() == command_sha256.as_slice()
                })
            }))
        }
        "prompt_chain_receipt" if event_id.is_none() => {
            let row: Option<(Vec<u8>, Option<Vec<u8>>)> = tx
                .query_row(
                    "SELECT command_sha256, command_payload
                     FROM prompt_chain_command_receipts WHERE command_id = ?1",
                    [command_id.as_slice()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            Ok(row.is_some_and(|(hash, payload)| {
                payload.is_some_and(|payload| {
                    !payload.is_empty() && hash.as_slice() == command_sha256.as_slice()
                })
            }))
        }
        "prompt_event" => {
            let Some(event_id) = event_id.as_deref() else {
                return Ok(false);
            };
            let row: Option<(Vec<u8>, Vec<u8>, Option<Vec<u8>>)> = tx
                .query_row(
                    "SELECT event.command_id, receipt.command_sha256, receipt.command_payload
                     FROM prompt_events AS event
                     JOIN prompt_command_receipts AS receipt
                       ON receipt.command_id = event.command_id
                     WHERE event.prompt_event_id = ?1",
                    [event_id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?;
            Ok(row.is_some_and(|(event_command_id, hash, payload)| {
                event_command_id == *command_id
                    && payload.is_some_and(|payload| {
                        !payload.is_empty() && hash.as_slice() == command_sha256.as_slice()
                    })
            }))
        }
        "prompt_chain_event" => {
            let Some(event_id) = event_id.as_deref() else {
                return Ok(false);
            };
            let row: Option<(Vec<u8>, Vec<u8>, Option<Vec<u8>>)> = tx
                .query_row(
                    "SELECT event.command_id, receipt.command_sha256, receipt.command_payload
                     FROM prompt_chain_events AS event
                     JOIN prompt_chain_command_receipts AS receipt
                       ON receipt.command_id = event.command_id
                     WHERE event.prompt_chain_event_id = ?1",
                    [event_id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?;
            Ok(row.is_some_and(|(event_command_id, hash, payload)| {
                event_command_id == *command_id
                    && payload.is_some_and(|payload| {
                        !payload.is_empty() && hash.as_slice() == command_sha256.as_slice()
                    })
            }))
        }
        _ => Ok(false),
    }
}

fn validate_event_sequence_domain(
    tx: &Transaction<'_>,
    table: &str,
) -> Result<(), PromptStoreError> {
    let table = match table {
        "prompt_events" | "prompt_chain_events" => table,
        _ => {
            return Err(PromptStoreError::Corruption(
                "unsupported prompt event table".into(),
            ))
        }
    };
    let has_invalid_sequence: i64 = tx.query_row(
        &format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE sequence < 1)"),
        [],
        |row| row.get(0),
    )?;
    if has_invalid_sequence != 0 {
        return Err(PromptStoreError::Corruption(format!(
            "{table} contains a non-positive sequence"
        )));
    }
    Ok(())
}

fn validate_prompt_lineage(tx: &Transaction<'_>) -> Result<(), PromptStoreError> {
    validate_current_version_projection(tx)?;
    validate_event_sequence_domain(tx, "prompt_events")?;
    validate_event_sequence_domain(tx, "prompt_chain_events")?;
    let projection = capture_projection_snapshot(tx)?;

    let mut last_sequence = 0_i64;
    let mut expected_sequence = 1_i64;
    let mut event_version_ids = HashSet::new();
    let mut prompt_version_cursors = HashMap::new();
    while let Some((
        sequence,
        event_id,
        command_id,
        prompt_id,
        event_type,
        occurred_at_ms,
        payload,
    )) = next_prompt_event_row(tx, last_sequence)?
    {
        if sequence != expected_sequence {
            return Err(PromptStoreError::Corruption(
                "prompt event sequence is not a contiguous prefix".into(),
            ));
        }
        last_sequence = sequence;
        expected_sequence = expected_sequence
            .checked_add(1)
            .ok_or_else(|| PromptStoreError::Corruption("prompt event sequence overflow".into()))?;
        let (event, _occurred_at_ms, receipt_version_id) = validate_prompt_event_row(
            tx,
            &event_id,
            &command_id,
            &prompt_id,
            &event_type,
            occurred_at_ms,
            &payload,
        )?;
        match &event {
            PromptEvent::PromptCreated { version, .. }
            | PromptEvent::PromptVersionCreated { version, .. } => {
                event_version_ids.insert(version.id);
            }
            _ => {}
        }
        validate_prompt_event_temporal_lineage(
            tx,
            &event,
            receipt_version_id,
            &mut prompt_version_cursors,
        )?;
    }
    validate_prompt_version_event_coverage(tx, &event_version_ids)?;

    let mut last_chain_sequence = 0_i64;
    let mut expected_chain_sequence = 1_i64;
    while let Some((
        sequence,
        event_id,
        command_id,
        chain_id,
        event_type,
        occurred_at_ms,
        payload,
    )) = next_chain_event_row(tx, last_chain_sequence)?
    {
        if sequence != expected_chain_sequence {
            return Err(PromptStoreError::Corruption(
                "prompt chain event sequence is not a contiguous prefix".into(),
            ));
        }
        last_chain_sequence = sequence;
        expected_chain_sequence = expected_chain_sequence.checked_add(1).ok_or_else(|| {
            PromptStoreError::Corruption("prompt chain event sequence overflow".into())
        })?;
        validate_chain_event_row(
            tx,
            &event_id,
            &command_id,
            &chain_id,
            &event_type,
            occurred_at_ms,
            &payload,
        )?;
    }

    validate_all_prompt_receipts(tx)?;
    validate_all_chain_receipts(tx)?;
    validate_projection_records(tx)?;
    replay_and_compare_projection(tx, projection)?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProjectionSnapshot {
    prompts: Vec<PromptProjectionRow>,
    tags: Vec<TagProjectionRow>,
    chains: Vec<ChainProjectionRow>,
    links: Vec<LinkProjectionRow>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PromptProjectionRow {
    prompt_id: Vec<u8>,
    title: String,
    description: Option<String>,
    current_version_id: Vec<u8>,
    revision: i64,
    created_at_ms: i64,
    updated_at_ms: i64,
    archived_at_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TagProjectionRow {
    prompt_id: Vec<u8>,
    tag: String,
    position: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ChainProjectionRow {
    chain_id: Vec<u8>,
    title: String,
    description: Option<String>,
    revision: i64,
    created_at_ms: i64,
    updated_at_ms: i64,
    archived_at_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LinkProjectionRow {
    link_id: Vec<u8>,
    chain_id: Vec<u8>,
    position: i64,
    prompt_id: Vec<u8>,
    prompt_version_id: Vec<u8>,
}

fn capture_projection_snapshot(
    tx: &Transaction<'_>,
) -> Result<ProjectionSnapshot, PromptStoreError> {
    let prompt_count = bounded_projection_count(tx, "saved_prompts")?;
    let tag_count = bounded_projection_count(tx, "prompt_tags")?;
    let chain_count = bounded_projection_count(tx, "prompt_chains")?;
    let link_count = bounded_projection_count(tx, "prompt_chain_links")?;

    let mut prompts = Vec::with_capacity(prompt_count);
    let mut statement = tx.prepare(
        "SELECT prompt_id, title, description, current_version_id,
                revision, created_at_ms, updated_at_ms, archived_at_ms
         FROM saved_prompts ORDER BY prompt_id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok(PromptProjectionRow {
            prompt_id: row.get(0)?,
            title: row.get(1)?,
            description: row.get(2)?,
            current_version_id: row.get(3)?,
            revision: row.get(4)?,
            created_at_ms: row.get(5)?,
            updated_at_ms: row.get(6)?,
            archived_at_ms: row.get(7)?,
        })
    })?;
    for row in rows {
        prompts.push(row?);
    }

    let mut tags = Vec::with_capacity(tag_count);
    let mut statement = tx.prepare(
        "SELECT prompt_id, tag, position FROM prompt_tags
         ORDER BY prompt_id, position, tag",
    )?;
    let rows = statement.query_map([], |row| {
        Ok(TagProjectionRow {
            prompt_id: row.get(0)?,
            tag: row.get(1)?,
            position: row.get(2)?,
        })
    })?;
    for row in rows {
        tags.push(row?);
    }

    let mut chains = Vec::with_capacity(chain_count);
    let mut statement = tx.prepare(
        "SELECT chain_id, title, description, revision,
                created_at_ms, updated_at_ms, archived_at_ms
         FROM prompt_chains ORDER BY chain_id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok(ChainProjectionRow {
            chain_id: row.get(0)?,
            title: row.get(1)?,
            description: row.get(2)?,
            revision: row.get(3)?,
            created_at_ms: row.get(4)?,
            updated_at_ms: row.get(5)?,
            archived_at_ms: row.get(6)?,
        })
    })?;
    for row in rows {
        chains.push(row?);
    }

    let mut links = Vec::with_capacity(link_count);
    let mut statement = tx.prepare(
        "SELECT link_id, chain_id, position, prompt_id, prompt_version_id
         FROM prompt_chain_links ORDER BY chain_id, position, link_id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok(LinkProjectionRow {
            link_id: row.get(0)?,
            chain_id: row.get(1)?,
            position: row.get(2)?,
            prompt_id: row.get(3)?,
            prompt_version_id: row.get(4)?,
        })
    })?;
    for row in rows {
        links.push(row?);
    }

    Ok(ProjectionSnapshot {
        prompts,
        tags,
        chains,
        links,
    })
}

fn bounded_projection_count(tx: &Transaction<'_>, table: &str) -> Result<usize, PromptStoreError> {
    let sql = match table {
        "saved_prompts" | "prompt_tags" | "prompt_chains" | "prompt_chain_links" => {
            format!("SELECT COUNT(*) FROM {table}")
        }
        _ => {
            return Err(PromptStoreError::Corruption(
                "unsupported prompt projection table".into(),
            ))
        }
    };
    let count: i64 = tx.query_row(&sql, [], |row| row.get(0))?;
    let count = usize::try_from(count)
        .map_err(|_| PromptStoreError::Corruption("negative prompt projection count".into()))?;
    if count > MAX_PROMPT_JOURNAL_ROWS {
        return Err(PromptStoreError::Corruption(format!(
            "prompt projection exceeds maximum of {MAX_PROMPT_JOURNAL_ROWS} rows"
        )));
    }
    Ok(count)
}

fn replay_and_compare_projection(
    tx: &Transaction<'_>,
    expected: ProjectionSnapshot,
) -> Result<(), PromptStoreError> {
    tx.execute_batch(
        "DROP TRIGGER IF EXISTS saved_prompts_current_version_is_latest;
         DROP TRIGGER IF EXISTS saved_prompts_current_version_is_latest_insert;
         DELETE FROM prompt_chain_links;
         DELETE FROM prompt_chains;
         DELETE FROM prompt_tags;
         DELETE FROM saved_prompts;",
    )?;

    let mut last_sequence = 0_i64;
    while let Some((
        sequence,
        _event_id,
        _command_id,
        _prompt_id,
        _event_type,
        occurred_at_ms,
        payload,
    )) = next_prompt_event_row(tx, last_sequence)?
    {
        last_sequence = sequence;
        let event = PromptEvent::decode(&payload)
            .map_err(|error| PromptStoreError::Corruption(error.to_string()))?;
        apply_event(tx, &event, occurred_at_ms)?;
    }

    let mut last_chain_sequence = 0_i64;
    while let Some((
        sequence,
        _event_id,
        command_id,
        _chain_id,
        _event_type,
        occurred_at_ms,
        payload,
    )) = next_chain_event_row(tx, last_chain_sequence)?
    {
        last_chain_sequence = sequence;
        let event = PromptChainEvent::decode(&payload)
            .map_err(|error| PromptStoreError::Corruption(error.to_string()))?;
        let command_payload: Vec<u8> = tx.query_row(
            "SELECT command_payload FROM prompt_chain_command_receipts WHERE command_id = ?1",
            [command_id.as_slice()],
            |row| row.get(0),
        )?;
        let decoded_command = decode_chain_command(&command_payload)?;
        apply_chain_event(
            tx,
            &event,
            occurred_at_ms,
            &decoded_command.command,
            decoded_command.resolved_prompt_version_id,
        )?;
    }

    let actual = capture_projection_snapshot(tx)?;
    if actual != expected {
        return Err(PromptStoreError::Corruption(
            "current prompt projection does not match exact durable event replay".into(),
        ));
    }
    Ok(())
}

fn validate_projection_records(tx: &Transaction<'_>) -> Result<(), PromptStoreError> {
    let mut statement = tx.prepare("SELECT prompt_id FROM saved_prompts ORDER BY prompt_id")?;
    let rows = statement.query_map([], |row| row.get::<_, Vec<u8>>(0))?;
    for row in rows {
        let prompt_id = prompt_id_from_bytes(&row?)?;
        let prompt = load_prompt(tx, prompt_id)?.ok_or_else(|| {
            PromptStoreError::Corruption("saved prompt projection row disappeared".into())
        })?;
        validate_saved_prompt_record(tx, &prompt)?;
    }

    let mut statement = tx.prepare("SELECT chain_id FROM prompt_chains ORDER BY chain_id")?;
    let rows = statement.query_map([], |row| row.get::<_, Vec<u8>>(0))?;
    for row in rows {
        let chain_id = prompt_chain_id_from_bytes(&row?)?;
        let chain = load_chain(tx, chain_id)?.ok_or_else(|| {
            PromptStoreError::Corruption("prompt chain projection row disappeared".into())
        })?;
        let links = load_chain_links(tx, chain.id)?;
        validate_chain_links(tx, chain.id, &links)?;
    }

    let orphan_version_count: i64 = tx.query_row(
        "SELECT COUNT(*) FROM prompt_versions AS version
         WHERE NOT EXISTS (
           SELECT 1 FROM saved_prompts AS prompt
           WHERE prompt.prompt_id = version.prompt_id
         )",
        [],
        |row| row.get(0),
    )?;
    if orphan_version_count != 0 {
        return Err(PromptStoreError::Corruption(
            "prompt version has no saved prompt projection".into(),
        ));
    }
    Ok(())
}

type PromptEventRow = (i64, Vec<u8>, Vec<u8>, Vec<u8>, String, i64, Vec<u8>);

fn next_prompt_event_row(
    tx: &Transaction<'_>,
    last_sequence: i64,
) -> Result<Option<PromptEventRow>, PromptStoreError> {
    Ok(tx
        .query_row(
            "SELECT sequence, prompt_event_id, command_id, prompt_id, event_type,
                    occurred_at_ms, payload
             FROM prompt_events
             WHERE sequence > ?1
             ORDER BY sequence ASC
             LIMIT 1",
            [last_sequence],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                ))
            },
        )
        .optional()?)
}

type ChainEventRow = (i64, Vec<u8>, Vec<u8>, Vec<u8>, String, i64, Vec<u8>);

fn next_chain_event_row(
    tx: &Transaction<'_>,
    last_sequence: i64,
) -> Result<Option<ChainEventRow>, PromptStoreError> {
    Ok(tx
        .query_row(
            "SELECT sequence, prompt_chain_event_id, command_id, chain_id, event_type,
                    occurred_at_ms, payload
             FROM prompt_chain_events
             WHERE sequence > ?1
             ORDER BY sequence ASC
             LIMIT 1",
            [last_sequence],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                ))
            },
        )
        .optional()?)
}

fn validate_prompt_version_event_coverage(
    tx: &Transaction<'_>,
    event_version_ids: &HashSet<PromptVersionId>,
) -> Result<(), PromptStoreError> {
    let mut statement = tx.prepare("SELECT prompt_version_id FROM prompt_versions")?;
    let rows = statement.query_map([], |row| row.get::<_, Vec<u8>>(0))?;
    for row in rows {
        let version_id = version_id_from_bytes(&row?)?;
        if !event_version_ids.contains(&version_id) {
            return Err(PromptStoreError::Corruption(
                "prompt version has no matching durable prompt event".into(),
            ));
        }
    }
    Ok(())
}

fn validate_sql_tags(tags: &[String]) -> Result<(), PromptStoreError> {
    let normalized = normalized_tags(tags).map_err(|_| {
        PromptStoreError::Corruption("prompt tags must use printable lowercase ASCII".into())
    })?;
    if normalized != tags {
        return Err(PromptStoreError::Corruption(
            "prompt tags must use printable lowercase ASCII".into(),
        ));
    }
    Ok(())
}

fn validate_wire_size(kind: &str, payload: &[u8]) -> Result<(), PromptStoreError> {
    if payload.len() > MAX_PROMPT_DURABLE_WIRE_BYTES {
        return Err(PromptStoreError::Corruption(format!(
            "{kind} payload exceeds maximum durable size of {MAX_PROMPT_DURABLE_WIRE_BYTES} bytes"
        )));
    }
    Ok(())
}

fn resolve_chain_command(
    command: PromptChainCommand,
    tx: &Transaction<'_>,
) -> Result<(PromptChainCommand, Option<PromptVersionId>), PromptStoreError> {
    match command {
        PromptChainCommand::InsertPromptChainLink(mut command) => {
            let prompt = load_prompt(tx, command.prompt_id)?.ok_or(PromptStoreError::NotFound)?;
            validate_saved_prompt_record(tx, &prompt)?;
            let prompt_version_id = command
                .prompt_version_id
                .unwrap_or(prompt.current_version_id);
            command.prompt_version_id = Some(prompt_version_id);
            Ok((
                PromptChainCommand::InsertPromptChainLink(command),
                Some(prompt_version_id),
            ))
        }
        PromptChainCommand::UpdatePromptChainLinkVersion(command) => {
            let chain = load_chain(tx, command.chain_id)?.ok_or(PromptStoreError::NotFound)?;
            let links = load_chain_links(tx, chain.id)?;
            validate_chain_links(tx, chain.id, &links)?;
            let link = links
                .into_iter()
                .find(|link| link.id() == command.link_id)
                .ok_or(PromptStoreError::NotFound)?;
            let prompt = load_prompt(tx, link.prompt_id())?.ok_or_else(|| {
                PromptStoreError::Corruption("chain link references missing prompt".into())
            })?;
            validate_saved_prompt_record(tx, &prompt)?;
            Ok((
                PromptChainCommand::UpdatePromptChainLinkVersion(command),
                Some(prompt.current_version_id),
            ))
        }
        command => Ok((command, None)),
    }
}

fn load_chain_command_payload(
    tx: &Transaction<'_>,
    command_id: CommandId,
) -> Result<Option<(Vec<u8>, Vec<u8>)>, PromptStoreError> {
    let row: Option<(Vec<u8>, Option<Vec<u8>>)> = tx
        .query_row(
            "SELECT command_sha256, command_payload
             FROM prompt_chain_command_receipts WHERE command_id = ?1",
            [command_id.as_bytes().as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((command_sha256, command_payload)) = row else {
        return Ok(None);
    };
    let Some(command_payload) = command_payload else {
        return Err(PromptStoreError::Corruption(
            "prompt chain receipt command payload is missing".into(),
        ));
    };
    Ok(Some((command_sha256, command_payload)))
}

pub(crate) fn library_projection_revision_in_tx(
    tx: &Transaction<'_>,
) -> Result<u64, PromptStoreError> {
    let prompt_sum: i64 = tx.query_row(
        "SELECT COALESCE(SUM(revision), 0) FROM saved_prompts",
        [],
        |row| row.get(0),
    )?;
    let chain_sum: i64 = tx.query_row(
        "SELECT COALESCE(SUM(revision), 0) FROM prompt_chains",
        [],
        |row| row.get(0),
    )?;
    let total = prompt_sum.checked_add(chain_sum).ok_or_else(|| {
        PromptStoreError::Corruption("library projection revision overflow".into())
    })?;
    from_i64("library_projection_revision", total)
}

pub(crate) fn prompt_mutation_receipt_matching_command(
    tx: &Transaction<'_>,
    command_id: CommandId,
    command: &PromptCommand,
) -> Result<Option<PromptMutationReceipt>, PromptStoreError> {
    let command = command.canonicalize()?;
    command.validate()?;
    let command_payload = command
        .encode()
        .map_err(|error| PromptStoreError::Corruption(error.to_string()))?;
    validate_wire_size("prompt command", &command_payload)?;
    let command_sha256 = sha256_bytes(&command_payload);
    load_receipt(tx, command_id, &command, &command_sha256)
}

pub(crate) fn prompt_chain_mutation_receipt_matching_command(
    tx: &Transaction<'_>,
    command_id: CommandId,
    command: &PromptChainCommand,
) -> Result<Option<PromptChainMutationReceipt>, PromptStoreError> {
    let command = command.canonicalize()?;
    command.validate()?;
    let Some((stored_hash, stored_payload)) = load_chain_command_payload(tx, command_id)? else {
        return Ok(None);
    };
    let requested_original_hash = command
        .fingerprint()
        .map_err(|error| PromptStoreError::Corruption(error.to_string()))?;
    let stored_command = decode_chain_command(&stored_payload)?;
    if requested_original_hash != stored_command.original_command_sha256
        || command != stored_command.original_command
    {
        return Err(PromptStoreError::IdempotencyConflict);
    }
    let stored_sha256 = digest_from_bytes(&stored_hash)?;
    load_chain_receipt(
        tx,
        command_id,
        &stored_command.original_command,
        &stored_command.command,
        &stored_sha256,
        stored_command.resolved_prompt_version_id,
    )
}

pub(crate) fn execute_prompt_command_in_tx(
    tx: &Transaction<'_>,
    command_id: CommandId,
    command: PromptCommand,
) -> Result<PromptMutationReceipt, PromptStoreError> {
    let command = command.canonicalize()?;
    match &command {
        PromptCommand::CreatePrompt(command) => validate_sql_tags(&command.tags)?,
        PromptCommand::SetPromptTags(command) => validate_sql_tags(&command.tags)?,
        _ => {}
    }
    command.validate()?;
    let command_payload = command
        .encode()
        .map_err(|error| PromptStoreError::Corruption(error.to_string()))?;
    validate_wire_size("prompt command", &command_payload)?;
    let command_sha256 = sha256_bytes(&command_payload);
    validate_prompt_command_state(tx, &command)?;

    if let Some(receipt) = load_receipt(tx, command_id, &command, &command_sha256)? {
        return Ok(receipt);
    }

    let (receipt, event, prompt_id, occurred_at_ms) = apply_command(tx, command_id, &command)?;
    let receipt_payload = receipt
        .encode()
        .map_err(|error| PromptStoreError::Corruption(error.to_string()))?;
    validate_wire_size("prompt receipt", &receipt_payload)?;
    tx.execute(
        "INSERT INTO prompt_command_receipts(
            command_id, command_sha256, command_payload, prompt_id, prompt_version_id,
            revision, receipt, created_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        rusqlite::params![
            command_id.as_bytes().as_slice(),
            command_sha256.as_slice(),
            command_payload,
            receipt.prompt_id.as_bytes().as_slice(),
            receipt.prompt_version_id.as_bytes().as_slice(),
            to_i64(receipt.revision)?,
            receipt_payload,
            occurred_at_ms,
        ],
    )?;
    if let Some(event) = event {
        let event_payload = event
            .encode()
            .map_err(|error| PromptStoreError::Corruption(error.to_string()))?;
        validate_wire_size("prompt event", &event_payload)?;
        tx.execute(
            "INSERT INTO prompt_events(
                prompt_event_id, command_id, prompt_id, event_type, occurred_at_ms, payload
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                EventId::new().as_bytes().as_slice(),
                command_id.as_bytes().as_slice(),
                prompt_id.as_bytes().as_slice(),
                event.event_type(),
                occurred_at_ms,
                event_payload,
            ],
        )?;
    }
    Ok(receipt)
}

fn apply_command(
    tx: &Transaction<'_>,
    command_id: CommandId,
    command: &PromptCommand,
) -> Result<(PromptMutationReceipt, Option<PromptEvent>, PromptId, i64), PromptStoreError> {
    match command {
        PromptCommand::CreatePrompt(command) => apply_create(tx, command_id, command),
        PromptCommand::CreatePromptVersion(command) => {
            apply_create_version(tx, command_id, command)
        }
        PromptCommand::RenamePrompt(command) => apply_rename(tx, command_id, command),
        PromptCommand::SetPromptTags(command) => apply_tags(tx, command_id, command),
        PromptCommand::ArchivePrompt(command) => apply_archive(tx, command_id, command),
        PromptCommand::RestorePrompt(command) => apply_restore(tx, command_id, command),
    }
}

fn validate_prompt_command_state(
    tx: &Transaction<'_>,
    command: &PromptCommand,
) -> Result<(), PromptStoreError> {
    let prompt_id = match command {
        PromptCommand::CreatePrompt(command) => command.prompt_id,
        PromptCommand::CreatePromptVersion(command) => command.prompt_id,
        PromptCommand::RenamePrompt(command) => command.prompt_id,
        PromptCommand::SetPromptTags(command) => command.prompt_id,
        PromptCommand::ArchivePrompt(command) => command.prompt_id,
        PromptCommand::RestorePrompt(command) => command.prompt_id,
    };
    if let Some(prompt) = load_prompt(tx, prompt_id)? {
        validate_saved_prompt_record(tx, &prompt)?;
    }
    Ok(())
}

fn apply_create(
    tx: &Transaction<'_>,
    command_id: CommandId,
    command: &CreatePrompt,
) -> Result<(PromptMutationReceipt, Option<PromptEvent>, PromptId, i64), PromptStoreError> {
    if prompt_exists(tx, command.prompt_id)? {
        return Err(PromptStoreError::AlreadyExists);
    }
    let tags = command.normalized_tags()?;
    let variables = command.normalized_variables()?;
    let version = PromptVersion::new_with_variables(
        command.prompt_version_id,
        command.prompt_id,
        1,
        command.body.clone(),
        variables,
        command.created_at_ms,
    )?;
    let prompt = SavedPrompt {
        id: command.prompt_id,
        title: trim_prompt_whitespace(&command.title).to_string(),
        description: command
            .description
            .as_deref()
            .map(trim_prompt_whitespace)
            .map(str::to_string),
        tags,
        current_version_id: version.id,
        revision: 1,
        archived_at_ms: None,
    };
    insert_version(tx, &version)?;
    insert_saved_prompt(tx, &prompt, command.created_at_ms)?;
    insert_tags(tx, prompt.id, &prompt.tags)?;
    let receipt = PromptMutationReceipt {
        command_id,
        prompt_id: prompt.id,
        prompt_version_id: version.id,
        revision: prompt.revision,
    };
    Ok((
        receipt,
        Some(PromptEvent::PromptCreated { prompt, version }),
        command.prompt_id,
        command.created_at_ms,
    ))
}

fn apply_create_version(
    tx: &Transaction<'_>,
    command_id: CommandId,
    command: &CreatePromptVersion,
) -> Result<(PromptMutationReceipt, Option<PromptEvent>, PromptId, i64), PromptStoreError> {
    let prompt = load_prompt(tx, command.prompt_id)?.ok_or(PromptStoreError::NotFound)?;
    check_revision(&prompt, command.expected_revision)?;
    ensure_prompt_active(&prompt)?;
    let next_version = next_version_number(tx, command.prompt_id)?;
    let variables = command.normalized_variables()?;
    let version = PromptVersion::new_with_variables(
        command.prompt_version_id,
        command.prompt_id,
        next_version,
        command.body.clone(),
        variables,
        command.created_at_ms,
    )?;
    let revision = next_revision(prompt.revision)?;
    insert_version(tx, &version)?;
    update_current_version(tx, &prompt, version.id, revision, command.created_at_ms)?;
    let receipt = PromptMutationReceipt {
        command_id,
        prompt_id: prompt.id,
        prompt_version_id: version.id,
        revision,
    };
    Ok((
        receipt,
        Some(PromptEvent::PromptVersionCreated {
            prompt_id: prompt.id,
            version,
            revision,
        }),
        prompt.id,
        command.created_at_ms,
    ))
}

fn apply_rename(
    tx: &Transaction<'_>,
    command_id: CommandId,
    command: &RenamePrompt,
) -> Result<(PromptMutationReceipt, Option<PromptEvent>, PromptId, i64), PromptStoreError> {
    let prompt = load_prompt(tx, command.prompt_id)?.ok_or(PromptStoreError::NotFound)?;
    check_revision(&prompt, command.expected_revision)?;
    ensure_prompt_active(&prompt)?;
    command.validate()?;
    let title = trim_prompt_whitespace(&command.title);
    if prompt.title == title {
        let receipt = PromptMutationReceipt {
            command_id,
            prompt_id: prompt.id,
            prompt_version_id: prompt.current_version_id,
            revision: prompt.revision,
        };
        return Ok((receipt, None, prompt.id, now_ms()));
    }
    let revision = next_revision(prompt.revision)?;
    let occurred_at_ms = now_ms();
    let changed = tx.execute(
        "UPDATE saved_prompts SET title = ?1, revision = ?2, updated_at_ms = ?3
         WHERE prompt_id = ?4 AND revision = ?5",
        rusqlite::params![
            title,
            to_i64(revision)?,
            occurred_at_ms,
            prompt.id.as_bytes().as_slice(),
            to_i64(prompt.revision)?,
        ],
    )?;
    if changed != 1 {
        return Err(PromptStoreError::RevisionConflict {
            expected: command.expected_revision,
            actual: prompt.revision,
        });
    }
    let receipt = PromptMutationReceipt {
        command_id,
        prompt_id: prompt.id,
        prompt_version_id: prompt.current_version_id,
        revision,
    };
    Ok((
        receipt,
        Some(PromptEvent::PromptRenamed {
            prompt_id: prompt.id,
            title: title.to_string(),
            revision,
        }),
        prompt.id,
        occurred_at_ms,
    ))
}

fn apply_tags(
    tx: &Transaction<'_>,
    command_id: CommandId,
    command: &SetPromptTags,
) -> Result<(PromptMutationReceipt, Option<PromptEvent>, PromptId, i64), PromptStoreError> {
    let prompt = load_prompt(tx, command.prompt_id)?.ok_or(PromptStoreError::NotFound)?;
    check_revision(&prompt, command.expected_revision)?;
    ensure_prompt_active(&prompt)?;
    let tags = command.validate()?;
    if prompt.tags == tags {
        let receipt = PromptMutationReceipt {
            command_id,
            prompt_id: prompt.id,
            prompt_version_id: prompt.current_version_id,
            revision: prompt.revision,
        };
        return Ok((receipt, None, prompt.id, now_ms()));
    }
    let revision = next_revision(prompt.revision)?;
    let now = now_ms();
    let changed = tx.execute(
        "UPDATE saved_prompts SET revision = ?1, updated_at_ms = ?2
         WHERE prompt_id = ?3 AND revision = ?4",
        rusqlite::params![
            to_i64(revision)?,
            now,
            prompt.id.as_bytes().as_slice(),
            to_i64(prompt.revision)?,
        ],
    )?;
    if changed != 1 {
        return Err(PromptStoreError::RevisionConflict {
            expected: command.expected_revision,
            actual: prompt.revision,
        });
    }
    tx.execute(
        "DELETE FROM prompt_tags WHERE prompt_id = ?1",
        [prompt.id.as_bytes().as_slice()],
    )?;
    insert_tags(tx, prompt.id, &tags)?;
    let receipt = PromptMutationReceipt {
        command_id,
        prompt_id: prompt.id,
        prompt_version_id: prompt.current_version_id,
        revision,
    };
    Ok((
        receipt,
        Some(PromptEvent::PromptTagsSet {
            prompt_id: prompt.id,
            tags,
            revision,
        }),
        prompt.id,
        now,
    ))
}

fn apply_archive(
    tx: &Transaction<'_>,
    command_id: CommandId,
    command: &ArchivePrompt,
) -> Result<(PromptMutationReceipt, Option<PromptEvent>, PromptId, i64), PromptStoreError> {
    let prompt = load_prompt(tx, command.prompt_id)?.ok_or(PromptStoreError::NotFound)?;
    check_revision(&prompt, command.expected_revision)?;
    if prompt.archived_at_ms.is_some() {
        let receipt = PromptMutationReceipt {
            command_id,
            prompt_id: prompt.id,
            prompt_version_id: prompt.current_version_id,
            revision: prompt.revision,
        };
        return Ok((receipt, None, prompt.id, command.archived_at_ms));
    }
    let revision = next_revision(prompt.revision)?;
    update_archive(
        tx,
        &prompt,
        Some(command.archived_at_ms),
        revision,
        command.archived_at_ms,
    )?;
    let receipt = PromptMutationReceipt {
        command_id,
        prompt_id: prompt.id,
        prompt_version_id: prompt.current_version_id,
        revision,
    };
    Ok((
        receipt,
        Some(PromptEvent::PromptArchived {
            prompt_id: prompt.id,
            archived_at_ms: command.archived_at_ms,
            revision,
        }),
        prompt.id,
        command.archived_at_ms,
    ))
}

fn apply_restore(
    tx: &Transaction<'_>,
    command_id: CommandId,
    command: &RestorePrompt,
) -> Result<(PromptMutationReceipt, Option<PromptEvent>, PromptId, i64), PromptStoreError> {
    let prompt = load_prompt(tx, command.prompt_id)?.ok_or(PromptStoreError::NotFound)?;
    check_revision(&prompt, command.expected_revision)?;
    if prompt.archived_at_ms.is_none() {
        let receipt = PromptMutationReceipt {
            command_id,
            prompt_id: prompt.id,
            prompt_version_id: prompt.current_version_id,
            revision: prompt.revision,
        };
        return Ok((receipt, None, prompt.id, now_ms()));
    }
    let revision = next_revision(prompt.revision)?;
    let now = now_ms();
    update_archive(tx, &prompt, None, revision, now)?;
    let receipt = PromptMutationReceipt {
        command_id,
        prompt_id: prompt.id,
        prompt_version_id: prompt.current_version_id,
        revision,
    };
    Ok((
        receipt,
        Some(PromptEvent::PromptRestored {
            prompt_id: prompt.id,
            revision,
        }),
        prompt.id,
        now,
    ))
}

type ChainApplyResult = Result<
    (
        PromptChainMutationReceipt,
        Option<PromptChainEvent>,
        PromptChainId,
        i64,
    ),
    PromptStoreError,
>;

fn apply_chain_command(
    tx: &Transaction<'_>,
    command_id: CommandId,
    command: &PromptChainCommand,
) -> ChainApplyResult {
    match command {
        PromptChainCommand::CreatePromptChain(command) => {
            apply_create_chain(tx, command_id, command)
        }
        PromptChainCommand::RenamePromptChain(command) => {
            apply_rename_chain(tx, command_id, command)
        }
        PromptChainCommand::InsertPromptChainLink(command) => {
            apply_insert_chain_link(tx, command_id, command)
        }
        PromptChainCommand::MovePromptChainLink(command) => {
            apply_move_chain_link(tx, command_id, command)
        }
        PromptChainCommand::RemovePromptChainLink(command) => {
            apply_remove_chain_link(tx, command_id, command)
        }
        PromptChainCommand::UpdatePromptChainLinkVersion(command) => {
            apply_update_chain_link_version(tx, command_id, command)
        }
        PromptChainCommand::ArchivePromptChain(command) => {
            apply_archive_chain(tx, command_id, command)
        }
        PromptChainCommand::RestorePromptChain(command) => {
            apply_restore_chain(tx, command_id, command)
        }
    }
}

fn apply_create_chain(
    tx: &Transaction<'_>,
    command_id: CommandId,
    command: &CreatePromptChain,
) -> ChainApplyResult {
    if load_chain(tx, command.chain_id)?.is_some() {
        return Err(PromptStoreError::AlreadyExists);
    }
    command.validate()?;
    let chain = PromptChain {
        id: command.chain_id,
        title: trim_prompt_whitespace(&command.title).to_string(),
        description: command
            .description
            .as_deref()
            .map(trim_prompt_whitespace)
            .map(str::to_string),
        revision: 1,
        archived_at_ms: None,
    };
    insert_chain(tx, &chain, command.created_at_ms)?;
    let receipt = PromptChainMutationReceipt {
        command_id,
        chain_id: chain.id,
        link_id: None,
        revision: chain.revision,
    };
    Ok((
        receipt,
        Some(PromptChainEvent::PromptChainCreated { chain }),
        command.chain_id,
        command.created_at_ms,
    ))
}

fn apply_rename_chain(
    tx: &Transaction<'_>,
    command_id: CommandId,
    command: &RenamePromptChain,
) -> ChainApplyResult {
    let chain = load_chain(tx, command.chain_id)?.ok_or(PromptStoreError::NotFound)?;
    check_chain_revision(&chain, command.expected_revision)?;
    if chain.archived_at_ms.is_some() {
        return Err(PromptStoreError::InvalidTransition);
    }
    command.validate()?;
    let title = trim_prompt_whitespace(&command.title);
    if chain.title == title {
        return Ok((
            chain_receipt(command_id, &chain, None),
            None,
            chain.id,
            now_ms(),
        ));
    }
    let revision = next_revision(chain.revision)?;
    let occurred_at_ms = now_ms();
    update_chain_metadata(
        tx,
        &chain,
        title,
        chain.description.as_deref(),
        None,
        revision,
        occurred_at_ms,
    )?;
    Ok((
        chain_receipt(command_id, &chain, None).with_revision(revision),
        Some(PromptChainEvent::PromptChainRenamed {
            chain_id: chain.id,
            title: title.to_string(),
            revision,
        }),
        chain.id,
        occurred_at_ms,
    ))
}

fn apply_insert_chain_link(
    tx: &Transaction<'_>,
    command_id: CommandId,
    command: &InsertPromptChainLink,
) -> ChainApplyResult {
    let chain = load_chain(tx, command.chain_id)?.ok_or(PromptStoreError::NotFound)?;
    check_chain_revision(&chain, command.expected_revision)?;
    ensure_chain_active(&chain)?;
    let mut links = load_chain_links(tx, chain.id)?;
    validate_chain_links(tx, chain.id, &links)?;
    if links.iter().any(|link| link.id() == command.link_id) {
        return Err(PromptStoreError::AlreadyExists);
    }
    let prompt = load_prompt(tx, command.prompt_id)?.ok_or(PromptStoreError::NotFound)?;
    validate_saved_prompt_record(tx, &prompt)?;
    let prompt_version_id = match command.prompt_version_id {
        Some(version_id) => {
            let version = load_version(tx, version_id)?.ok_or(PromptStoreError::NotFound)?;
            if version.prompt_id != prompt.id {
                return Err(PromptStoreError::ConstraintViolation);
            }
            version.id
        }
        None => prompt.current_version_id,
    };
    let position = match command.before_link_id {
        Some(before_link_id) => links
            .iter()
            .position(|link| link.id() == before_link_id)
            .ok_or(PromptStoreError::NotFound)?,
        None => links.len(),
    };
    links.insert(
        position,
        PromptChainLink::store_issued(command.link_id, chain.id, 0, prompt.id, prompt_version_id),
    );
    renumber_links(&mut links)?;
    let revision = next_revision(chain.revision)?;
    let occurred_at_ms = now_ms();
    insert_chain_link_rows(
        tx,
        &chain,
        &links[position],
        position,
        revision,
        occurred_at_ms,
    )?;
    Ok((
        PromptChainMutationReceipt {
            command_id,
            chain_id: chain.id,
            link_id: Some(command.link_id),
            revision,
        },
        Some(PromptChainEvent::PromptChainLinksReplaced {
            chain_id: chain.id,
            links,
            revision,
        }),
        chain.id,
        occurred_at_ms,
    ))
}

fn apply_move_chain_link(
    tx: &Transaction<'_>,
    command_id: CommandId,
    command: &MovePromptChainLink,
) -> ChainApplyResult {
    let chain = load_chain(tx, command.chain_id)?.ok_or(PromptStoreError::NotFound)?;
    check_chain_revision(&chain, command.expected_revision)?;
    ensure_chain_active(&chain)?;
    let mut links = load_chain_links(tx, chain.id)?;
    validate_chain_links(tx, chain.id, &links)?;
    let old_order: Vec<PromptChainLinkId> = links.iter().map(|link| link.id()).collect();
    let moving_index = links
        .iter()
        .position(|link| link.id() == command.link_id)
        .ok_or(PromptStoreError::NotFound)?;
    if command.before_link_id == Some(command.link_id) {
        return Ok((
            chain_receipt(command_id, &chain, Some(command.link_id)),
            None,
            chain.id,
            now_ms(),
        ));
    }
    if let Some(before_link_id) = command.before_link_id {
        if !links.iter().any(|link| link.id() == before_link_id) {
            return Err(PromptStoreError::NotFound);
        }
    }
    let target_index = command
        .before_link_id
        .and_then(|before_link_id| links.iter().position(|link| link.id() == before_link_id))
        .unwrap_or(links.len());
    let moving = links.remove(moving_index);
    let position = command
        .before_link_id
        .and_then(|before_link_id| links.iter().position(|link| link.id() == before_link_id))
        .unwrap_or(links.len());
    links.insert(position, moving);
    renumber_links(&mut links)?;
    let new_order: Vec<PromptChainLinkId> = links.iter().map(|link| link.id()).collect();
    if old_order == new_order {
        return Ok((
            chain_receipt(command_id, &chain, Some(command.link_id)),
            None,
            chain.id,
            now_ms(),
        ));
    }
    let revision = next_revision(chain.revision)?;
    let occurred_at_ms = now_ms();
    move_chain_link_rows(
        tx,
        &chain,
        moving_index,
        target_index,
        revision,
        occurred_at_ms,
    )?;
    Ok((
        chain_receipt(command_id, &chain, Some(command.link_id)).with_revision(revision),
        Some(PromptChainEvent::PromptChainLinksReplaced {
            chain_id: chain.id,
            links,
            revision,
        }),
        chain.id,
        occurred_at_ms,
    ))
}

fn apply_remove_chain_link(
    tx: &Transaction<'_>,
    command_id: CommandId,
    command: &RemovePromptChainLink,
) -> ChainApplyResult {
    let chain = load_chain(tx, command.chain_id)?.ok_or(PromptStoreError::NotFound)?;
    check_chain_revision(&chain, command.expected_revision)?;
    ensure_chain_active(&chain)?;
    let mut links = load_chain_links(tx, chain.id)?;
    validate_chain_links(tx, chain.id, &links)?;
    let position = links
        .iter()
        .position(|link| link.id() == command.link_id)
        .ok_or(PromptStoreError::NotFound)?;
    links.remove(position);
    renumber_links(&mut links)?;
    let revision = next_revision(chain.revision)?;
    let occurred_at_ms = now_ms();
    remove_chain_link_rows(tx, &chain, position, revision, occurred_at_ms)?;
    Ok((
        chain_receipt(command_id, &chain, Some(command.link_id)).with_revision(revision),
        Some(PromptChainEvent::PromptChainLinksReplaced {
            chain_id: chain.id,
            links,
            revision,
        }),
        chain.id,
        occurred_at_ms,
    ))
}

fn apply_update_chain_link_version(
    tx: &Transaction<'_>,
    command_id: CommandId,
    command: &UpdatePromptChainLinkVersion,
) -> ChainApplyResult {
    let chain = load_chain(tx, command.chain_id)?.ok_or(PromptStoreError::NotFound)?;
    check_chain_revision(&chain, command.expected_revision)?;
    ensure_chain_active(&chain)?;
    let mut links = load_chain_links(tx, chain.id)?;
    validate_chain_links(tx, chain.id, &links)?;
    let position = links
        .iter()
        .position(|link| link.id() == command.link_id)
        .ok_or(PromptStoreError::NotFound)?;
    let prompt = load_prompt(tx, links[position].prompt_id())?.ok_or_else(|| {
        PromptStoreError::Corruption("chain link references missing prompt".into())
    })?;
    validate_saved_prompt_record(tx, &prompt)?;
    if links[position].prompt_version_id() == prompt.current_version_id {
        return Ok((
            chain_receipt(command_id, &chain, Some(command.link_id)),
            None,
            chain.id,
            now_ms(),
        ));
    }
    let existing = &links[position];
    links[position] = PromptChainLink::store_issued(
        existing.id(),
        existing.chain_id(),
        existing.position(),
        existing.prompt_id(),
        prompt.current_version_id,
    );
    let revision = next_revision(chain.revision)?;
    let occurred_at_ms = now_ms();
    update_chain_link_version_row(
        tx,
        &chain,
        links[position].id(),
        prompt.current_version_id,
        revision,
        occurred_at_ms,
    )?;
    Ok((
        chain_receipt(command_id, &chain, Some(command.link_id)).with_revision(revision),
        Some(PromptChainEvent::PromptChainLinksReplaced {
            chain_id: chain.id,
            links,
            revision,
        }),
        chain.id,
        occurred_at_ms,
    ))
}

fn apply_archive_chain(
    tx: &Transaction<'_>,
    command_id: CommandId,
    command: &ArchivePromptChain,
) -> ChainApplyResult {
    let chain = load_chain(tx, command.chain_id)?.ok_or(PromptStoreError::NotFound)?;
    check_chain_revision(&chain, command.expected_revision)?;
    if chain.archived_at_ms.is_some() {
        return Ok((
            chain_receipt(command_id, &chain, None),
            None,
            chain.id,
            command.archived_at_ms,
        ));
    }
    let revision = next_revision(chain.revision)?;
    update_chain_metadata(
        tx,
        &chain,
        &chain.title,
        chain.description.as_deref(),
        Some(command.archived_at_ms),
        revision,
        command.archived_at_ms,
    )?;
    Ok((
        chain_receipt(command_id, &chain, None).with_revision(revision),
        Some(PromptChainEvent::PromptChainArchived {
            chain_id: chain.id,
            archived_at_ms: command.archived_at_ms,
            revision,
        }),
        chain.id,
        command.archived_at_ms,
    ))
}

fn apply_restore_chain(
    tx: &Transaction<'_>,
    command_id: CommandId,
    command: &RestorePromptChain,
) -> ChainApplyResult {
    let chain = load_chain(tx, command.chain_id)?.ok_or(PromptStoreError::NotFound)?;
    check_chain_revision(&chain, command.expected_revision)?;
    if chain.archived_at_ms.is_none() {
        return Ok((
            chain_receipt(command_id, &chain, None),
            None,
            chain.id,
            now_ms(),
        ));
    }
    let revision = next_revision(chain.revision)?;
    let occurred_at_ms = now_ms();
    update_chain_metadata(
        tx,
        &chain,
        &chain.title,
        chain.description.as_deref(),
        None,
        revision,
        occurred_at_ms,
    )?;
    Ok((
        chain_receipt(command_id, &chain, None).with_revision(revision),
        Some(PromptChainEvent::PromptChainRestored {
            chain_id: chain.id,
            revision,
        }),
        chain.id,
        occurred_at_ms,
    ))
}

fn chain_receipt(
    command_id: CommandId,
    chain: &PromptChain,
    link_id: Option<PromptChainLinkId>,
) -> PromptChainMutationReceipt {
    PromptChainMutationReceipt {
        command_id,
        chain_id: chain.id,
        link_id,
        revision: chain.revision,
    }
}

trait ChainReceiptRevision {
    fn with_revision(self, revision: u64) -> Self;
}

impl ChainReceiptRevision for PromptChainMutationReceipt {
    fn with_revision(mut self, revision: u64) -> Self {
        self.revision = revision;
        self
    }
}

fn ensure_chain_active(chain: &PromptChain) -> Result<(), PromptStoreError> {
    if chain.archived_at_ms.is_some() {
        Err(PromptStoreError::InvalidTransition)
    } else {
        Ok(())
    }
}

fn ensure_prompt_active(prompt: &SavedPrompt) -> Result<(), PromptStoreError> {
    if prompt.archived_at_ms.is_some() {
        Err(PromptStoreError::InvalidTransition)
    } else {
        Ok(())
    }
}

fn renumber_links(links: &mut [PromptChainLink]) -> Result<(), PromptStoreError> {
    if links.len() > MAX_PROMPT_CHAIN_LINKS {
        return Err(PromptStoreError::Corruption(format!(
            "prompt chain exceeds maximum of {MAX_PROMPT_CHAIN_LINKS} links"
        )));
    }
    for (position, link) in links.iter_mut().enumerate() {
        let position = u32::try_from(position)
            .map_err(|_| PromptStoreError::Corruption("prompt chain is too long".into()))?;
        *link = PromptChainLink::store_issued(
            link.id(),
            link.chain_id(),
            position,
            link.prompt_id(),
            link.prompt_version_id(),
        );
    }
    Ok(())
}

fn validate_chain_links(
    conn: &Transaction<'_>,
    chain_id: PromptChainId,
    links: &[PromptChainLink],
) -> Result<(), PromptStoreError> {
    if links.len() > MAX_PROMPT_CHAIN_LINKS {
        return Err(PromptStoreError::Corruption(format!(
            "prompt chain exceeds maximum of {MAX_PROMPT_CHAIN_LINKS} links"
        )));
    }
    let mut link_ids = HashSet::with_capacity(links.len());
    let mut prompt_ids = HashSet::with_capacity(links.len());
    for (position, link) in links.iter().enumerate() {
        if link.chain_id() != chain_id
            || link.position()
                != u32::try_from(position)
                    .map_err(|_| PromptStoreError::Corruption("prompt chain is too long".into()))?
            || !link_ids.insert(link.id())
        {
            return Err(PromptStoreError::Corruption(
                "prompt chain links must be a dense ordered prefix".into(),
            ));
        }
        let version = load_version(conn, link.prompt_version_id())?.ok_or_else(|| {
            PromptStoreError::Corruption("prompt chain link references missing version".into())
        })?;
        if version.prompt_id != link.prompt_id() {
            return Err(PromptStoreError::Corruption(
                "prompt chain link version ownership mismatch".into(),
            ));
        }
        prompt_ids.insert(link.prompt_id());
    }
    for prompt_id in prompt_ids {
        let prompt = load_prompt(conn, prompt_id)?.ok_or_else(|| {
            PromptStoreError::Corruption("prompt chain link references missing prompt".into())
        })?;
        validate_saved_prompt_record(conn, &prompt)?;
    }
    Ok(())
}

fn advance_chain_revision(
    tx: &Transaction<'_>,
    chain: &PromptChain,
    revision: u64,
    updated_at_ms: i64,
) -> Result<(), PromptStoreError> {
    let changed = tx.execute(
        "UPDATE prompt_chains SET revision = ?1, updated_at_ms = ?2
         WHERE chain_id = ?3 AND revision = ?4",
        rusqlite::params![
            to_i64(revision)?,
            updated_at_ms,
            chain.id.as_bytes().as_slice(),
            to_i64(chain.revision)?,
        ],
    )?;
    if changed == 1 {
        Ok(())
    } else {
        Err(PromptStoreError::RevisionConflict {
            expected: chain.revision,
            actual: chain.revision,
        })
    }
}

/// Insert one link while shifting the affected suffix in bounded SQL work.
/// Temporary positions keep the unique `(chain_id, position)` index valid
/// without deleting and reinserting every link.
fn insert_chain_link_rows(
    tx: &Transaction<'_>,
    chain: &PromptChain,
    link: &PromptChainLink,
    position: usize,
    revision: u64,
    updated_at_ms: i64,
) -> Result<(), PromptStoreError> {
    let position = i64::try_from(position)
        .map_err(|_| PromptStoreError::Corruption("prompt chain position overflow".into()))?;
    advance_chain_revision(tx, chain, revision, updated_at_ms)?;
    tx.execute(
        "UPDATE prompt_chain_links
         SET position = position + ?2
         WHERE chain_id = ?1",
        rusqlite::params![chain.id.as_bytes().as_slice(), CHAIN_POSITION_SHIFT],
    )?;
    tx.execute(
        "UPDATE prompt_chain_links
         SET position = position - ?2
             + CASE WHEN position - ?2 >= ?3 THEN 1 ELSE 0 END
         WHERE chain_id = ?1",
        rusqlite::params![
            chain.id.as_bytes().as_slice(),
            CHAIN_POSITION_SHIFT,
            position,
        ],
    )?;
    tx.execute(
        "INSERT INTO prompt_chain_links(
            link_id, chain_id, position, prompt_id, prompt_version_id
         ) VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![
            link.id().as_bytes().as_slice(),
            link.chain_id().as_bytes().as_slice(),
            position,
            link.prompt_id().as_bytes().as_slice(),
            link.prompt_version_id().as_bytes().as_slice(),
        ],
    )?;
    Ok(())
}

/// Move one link by shifting only the affected interval. `moving_position`
/// and `before_position` refer to the original dense order; `before_position`
/// may equal the original link count for an append-to-end move.
fn move_chain_link_rows(
    tx: &Transaction<'_>,
    chain: &PromptChain,
    moving_position: usize,
    before_position: usize,
    revision: u64,
    updated_at_ms: i64,
) -> Result<(), PromptStoreError> {
    let moving_position = i64::try_from(moving_position)
        .map_err(|_| PromptStoreError::Corruption("prompt chain position overflow".into()))?;
    let before_position = i64::try_from(before_position)
        .map_err(|_| PromptStoreError::Corruption("prompt chain position overflow".into()))?;
    advance_chain_revision(tx, chain, revision, updated_at_ms)?;
    tx.execute(
        "UPDATE prompt_chain_links
         SET position = position + ?2
         WHERE chain_id = ?1",
        rusqlite::params![chain.id.as_bytes().as_slice(), CHAIN_POSITION_SHIFT],
    )?;
    if moving_position < before_position {
        tx.execute(
            "UPDATE prompt_chain_links
             SET position = CASE
               WHEN position - ?2 = ?3 THEN ?4 - 1
               WHEN position - ?2 > ?3 AND position - ?2 < ?4
                 THEN position - ?2 - 1
               ELSE position - ?2
             END
             WHERE chain_id = ?1",
            rusqlite::params![
                chain.id.as_bytes().as_slice(),
                CHAIN_POSITION_SHIFT,
                moving_position,
                before_position,
            ],
        )?;
    } else {
        tx.execute(
            "UPDATE prompt_chain_links
             SET position = CASE
               WHEN position - ?2 = ?3 THEN ?4
               WHEN position - ?2 >= ?4 AND position - ?2 < ?3
                 THEN position - ?2 + 1
               ELSE position - ?2
             END
             WHERE chain_id = ?1",
            rusqlite::params![
                chain.id.as_bytes().as_slice(),
                CHAIN_POSITION_SHIFT,
                moving_position,
                before_position,
            ],
        )?;
    }
    Ok(())
}

fn remove_chain_link_rows(
    tx: &Transaction<'_>,
    chain: &PromptChain,
    position: usize,
    revision: u64,
    updated_at_ms: i64,
) -> Result<(), PromptStoreError> {
    let position = i64::try_from(position)
        .map_err(|_| PromptStoreError::Corruption("prompt chain position overflow".into()))?;
    advance_chain_revision(tx, chain, revision, updated_at_ms)?;
    tx.execute(
        "UPDATE prompt_chain_links
         SET position = position + ?2
         WHERE chain_id = ?1",
        rusqlite::params![chain.id.as_bytes().as_slice(), CHAIN_POSITION_SHIFT],
    )?;
    tx.execute(
        "DELETE FROM prompt_chain_links
         WHERE chain_id = ?1 AND position = ?2 + ?3",
        rusqlite::params![
            chain.id.as_bytes().as_slice(),
            CHAIN_POSITION_SHIFT,
            position,
        ],
    )?;
    tx.execute(
        "UPDATE prompt_chain_links
         SET position = position - ?2
             - CASE WHEN position - ?2 > ?3 THEN 1 ELSE 0 END
         WHERE chain_id = ?1",
        rusqlite::params![
            chain.id.as_bytes().as_slice(),
            CHAIN_POSITION_SHIFT,
            position,
        ],
    )?;
    Ok(())
}

fn update_chain_link_version_row(
    tx: &Transaction<'_>,
    chain: &PromptChain,
    link_id: PromptChainLinkId,
    prompt_version_id: PromptVersionId,
    revision: u64,
    updated_at_ms: i64,
) -> Result<(), PromptStoreError> {
    advance_chain_revision(tx, chain, revision, updated_at_ms)?;
    let changed = tx.execute(
        "UPDATE prompt_chain_links
         SET prompt_version_id = ?1
         WHERE chain_id = ?2 AND link_id = ?3",
        rusqlite::params![
            prompt_version_id.as_bytes().as_slice(),
            chain.id.as_bytes().as_slice(),
            link_id.as_bytes().as_slice(),
        ],
    )?;
    if changed == 1 {
        Ok(())
    } else {
        Err(PromptStoreError::Corruption(
            "prompt chain link disappeared while updating its version".into(),
        ))
    }
}

fn write_chain_links(
    tx: &Transaction<'_>,
    chain: &PromptChain,
    links: &[PromptChainLink],
    revision: u64,
    updated_at_ms: i64,
) -> Result<(), PromptStoreError> {
    if links.len() > MAX_PROMPT_CHAIN_LINKS {
        return Err(PromptStoreError::Corruption(format!(
            "prompt chain exceeds maximum of {MAX_PROMPT_CHAIN_LINKS} links"
        )));
    }
    validate_chain_links(tx, chain.id, links)?;
    advance_chain_revision(tx, chain, revision, updated_at_ms)?;
    tx.execute(
        "DELETE FROM prompt_chain_links WHERE chain_id = ?1",
        [chain.id.as_bytes().as_slice()],
    )?;
    for link in links {
        tx.execute(
            "INSERT INTO prompt_chain_links(
                link_id, chain_id, position, prompt_id, prompt_version_id
             ) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                link.id().as_bytes().as_slice(),
                link.chain_id().as_bytes().as_slice(),
                i64::from(link.position()),
                link.prompt_id().as_bytes().as_slice(),
                link.prompt_version_id().as_bytes().as_slice(),
            ],
        )?;
    }
    Ok(())
}

fn insert_chain(
    tx: &Transaction<'_>,
    chain: &PromptChain,
    created_at_ms: i64,
) -> Result<(), PromptStoreError> {
    tx.execute(
        "INSERT INTO prompt_chains(
            chain_id, title, description, revision, created_at_ms,
            updated_at_ms, archived_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6)",
        rusqlite::params![
            chain.id.as_bytes().as_slice(),
            chain.title,
            chain.description,
            to_i64(chain.revision)?,
            created_at_ms,
            chain.archived_at_ms,
        ],
    )?;
    Ok(())
}

fn update_chain_metadata(
    tx: &Transaction<'_>,
    chain: &PromptChain,
    title: &str,
    description: Option<&str>,
    archived_at_ms: Option<i64>,
    revision: u64,
    updated_at_ms: i64,
) -> Result<(), PromptStoreError> {
    let changed = tx.execute(
        "UPDATE prompt_chains
         SET title = ?1, description = ?2, archived_at_ms = ?3,
             revision = ?4, updated_at_ms = ?5
         WHERE chain_id = ?6 AND revision = ?7",
        rusqlite::params![
            title,
            description,
            archived_at_ms,
            to_i64(revision)?,
            updated_at_ms,
            chain.id.as_bytes().as_slice(),
            to_i64(chain.revision)?,
        ],
    )?;
    if changed == 1 {
        Ok(())
    } else {
        Err(PromptStoreError::RevisionConflict {
            expected: chain.revision,
            actual: chain.revision,
        })
    }
}

fn apply_event(
    tx: &Transaction<'_>,
    event: &PromptEvent,
    occurred_at_ms: i64,
) -> Result<(), PromptStoreError> {
    match event {
        PromptEvent::PromptCreated { prompt, version } => {
            if prompt.revision != 1
                || prompt.current_version_id != version.id
                || prompt.archived_at_ms.is_some()
                || prompt.title != trim_prompt_whitespace(&prompt.title)
                || prompt
                    .description
                    .as_deref()
                    .is_some_and(|description| description != trim_prompt_whitespace(description))
                || normalized_tags(&prompt.tags).map_err(|_| {
                    PromptStoreError::Corruption("prompt.created tags are invalid".into())
                })? != prompt.tags
                || validate_sql_tags(&prompt.tags).is_err()
            {
                return Err(PromptStoreError::Corruption(
                    "prompt.created metadata or current version mismatch".into(),
                ));
            }
            if version.prompt_id != prompt.id || version.version != 1 {
                return Err(PromptStoreError::Corruption(
                    "prompt.created version ownership mismatch".into(),
                ));
            }
            CreatePrompt {
                prompt_id: prompt.id,
                prompt_version_id: version.id,
                title: prompt.title.clone(),
                description: prompt.description.clone(),
                tags: prompt.tags.clone(),
                variables: version.variables.clone(),
                body: version.body.clone(),
                created_at_ms: version.created_at_ms,
            }
            .validate()
            .map_err(|_| {
                PromptStoreError::Corruption("prompt.created content is invalid".into())
            })?;
            validate_version_record(version)?;
            ensure_version(tx, version)?;
            insert_saved_prompt(tx, prompt, version.created_at_ms)?;
            insert_tags(tx, prompt.id, &prompt.tags)?;
        }
        PromptEvent::PromptVersionCreated {
            prompt_id,
            version,
            revision,
        } => {
            let prompt = load_prompt(tx, *prompt_id)?.ok_or(PromptStoreError::Corruption(
                "prompt version event references missing prompt".into(),
            ))?;
            let expected_revision = next_revision(prompt.revision)?;
            let current_version =
                load_version(tx, prompt.current_version_id)?.ok_or_else(|| {
                    PromptStoreError::Corruption(
                        "prompt version event references missing current version".into(),
                    )
                })?;
            let expected_version = current_version.version.checked_add(1).ok_or_else(|| {
                PromptStoreError::Corruption("prompt version number overflow".into())
            })?;
            if prompt.archived_at_ms.is_some()
                || version.prompt_id != *prompt_id
                || version.version != expected_version
                || *revision != expected_revision
            {
                return Err(PromptStoreError::Corruption(
                    "prompt.version_created transition is invalid".into(),
                ));
            }
            ensure_version(tx, version)?;
            update_current_version(tx, &prompt, version.id, *revision, version.created_at_ms)?;
        }
        PromptEvent::PromptRenamed {
            prompt_id,
            title,
            revision,
        } => {
            let prompt = load_prompt(tx, *prompt_id)?.ok_or(PromptStoreError::Corruption(
                "prompt rename event references missing prompt".into(),
            ))?;
            let expected_revision = next_revision(prompt.revision)?;
            let title_is_valid = trim_prompt_whitespace(title) == title
                && (RenamePrompt {
                    prompt_id: *prompt_id,
                    title: title.clone(),
                    expected_revision: prompt.revision,
                })
                .validate()
                .is_ok();
            if prompt.archived_at_ms.is_some() || *revision != expected_revision || !title_is_valid
            {
                return Err(PromptStoreError::Corruption(
                    "prompt rename event is invalid".into(),
                ));
            }
            tx.execute(
                "UPDATE saved_prompts SET title = ?1, revision = ?2, updated_at_ms = ?3
                 WHERE prompt_id = ?4",
                rusqlite::params![
                    title,
                    to_i64(*revision)?,
                    occurred_at_ms,
                    prompt.id.as_bytes().as_slice()
                ],
            )?;
        }
        PromptEvent::PromptTagsSet {
            prompt_id,
            tags,
            revision,
        } => {
            let prompt = load_prompt(tx, *prompt_id)?.ok_or(PromptStoreError::Corruption(
                "prompt tag event references missing prompt".into(),
            ))?;
            let expected_revision = next_revision(prompt.revision)?;
            let tags_are_valid = normalized_tags(tags)
                .map(|normalized| normalized == *tags && validate_sql_tags(tags).is_ok())
                .unwrap_or(false);
            if prompt.archived_at_ms.is_some() || *revision != expected_revision || !tags_are_valid
            {
                return Err(PromptStoreError::Corruption(
                    "prompt tag event is invalid".into(),
                ));
            }
            tx.execute(
                "UPDATE saved_prompts SET revision = ?1, updated_at_ms = ?2 WHERE prompt_id = ?3",
                rusqlite::params![
                    to_i64(*revision)?,
                    occurred_at_ms,
                    prompt.id.as_bytes().as_slice()
                ],
            )?;
            tx.execute(
                "DELETE FROM prompt_tags WHERE prompt_id = ?1",
                [prompt.id.as_bytes().as_slice()],
            )?;
            insert_tags(tx, prompt.id, tags)?;
        }
        PromptEvent::PromptArchived {
            prompt_id,
            archived_at_ms,
            revision,
        } => {
            let prompt = load_prompt(tx, *prompt_id)?.ok_or(PromptStoreError::Corruption(
                "prompt archive event references missing prompt".into(),
            ))?;
            if prompt.archived_at_ms.is_some() || *revision != next_revision(prompt.revision)? {
                return Err(PromptStoreError::Corruption(
                    "prompt archive event is invalid".into(),
                ));
            }
            update_archive(
                tx,
                &prompt,
                Some(*archived_at_ms),
                *revision,
                occurred_at_ms,
            )?;
        }
        PromptEvent::PromptRestored {
            prompt_id,
            revision,
        } => {
            let prompt = load_prompt(tx, *prompt_id)?.ok_or(PromptStoreError::Corruption(
                "prompt restore event references missing prompt".into(),
            ))?;
            if prompt.archived_at_ms.is_none() || *revision != next_revision(prompt.revision)? {
                return Err(PromptStoreError::Corruption(
                    "prompt restore event is invalid".into(),
                ));
            }
            update_archive(tx, &prompt, None, *revision, occurred_at_ms)?;
        }
    }
    Ok(())
}

fn validate_chain_command_effect(
    tx: &Transaction<'_>,
    command: &PromptChainCommand,
    resolved_prompt_version_id: Option<PromptVersionId>,
    event: &PromptChainEvent,
) -> Result<(), PromptStoreError> {
    validate_chain_event_resolution(command, resolved_prompt_version_id, event)?;
    let (chain_id, expected_revision) = match command {
        PromptChainCommand::InsertPromptChainLink(command) => {
            (command.chain_id, command.expected_revision)
        }
        PromptChainCommand::MovePromptChainLink(command) => {
            (command.chain_id, command.expected_revision)
        }
        PromptChainCommand::RemovePromptChainLink(command) => {
            (command.chain_id, command.expected_revision)
        }
        PromptChainCommand::UpdatePromptChainLinkVersion(command) => {
            (command.chain_id, command.expected_revision)
        }
        _ => return Ok(()),
    };
    let PromptChainEvent::PromptChainLinksReplaced { links, .. } = event else {
        return Err(PromptStoreError::Corruption(
            "prompt chain link command has a non-link event".into(),
        ));
    };
    let chain = load_chain(tx, chain_id)?.ok_or_else(|| {
        PromptStoreError::Corruption("prompt chain link command references missing chain".into())
    })?;
    if chain.revision != expected_revision || chain.archived_at_ms.is_some() {
        return Err(PromptStoreError::Corruption(
            "prompt chain link command revision or archive state is invalid".into(),
        ));
    }
    let current_links = load_chain_links(tx, chain_id)?;
    validate_chain_links(tx, chain_id, &current_links)?;
    let mut expected_links = current_links.clone();
    validate_chain_links(tx, chain_id, links)?;
    match command {
        PromptChainCommand::InsertPromptChainLink(command) => {
            if expected_links
                .iter()
                .any(|link| link.id() == command.link_id)
            {
                return Err(PromptStoreError::Corruption(
                    "prompt chain insert command link already exists".into(),
                ));
            }
            let prompt = load_prompt(tx, command.prompt_id)?.ok_or_else(|| {
                PromptStoreError::Corruption("prompt chain insert command prompt is missing".into())
            })?;
            validate_saved_prompt_record(tx, &prompt)?;
            let event_link = links
                .iter()
                .find(|link| link.id() == command.link_id)
                .ok_or_else(|| {
                    PromptStoreError::Corruption(
                        "prompt chain insert effect is missing its command link".into(),
                    )
                })?;
            if event_link.prompt_id() != prompt.id {
                return Err(PromptStoreError::Corruption(
                    "prompt chain insert effect prompt ownership is invalid".into(),
                ));
            }
            if command
                .prompt_version_id
                .is_some_and(|version_id| version_id != event_link.prompt_version_id())
            {
                return Err(PromptStoreError::Corruption(
                    "prompt chain insert effect version disagrees with its command".into(),
                ));
            }
            let version = load_version(tx, event_link.prompt_version_id())?.ok_or_else(|| {
                PromptStoreError::Corruption("prompt chain insert effect version is missing".into())
            })?;
            if version.prompt_id != prompt.id {
                return Err(PromptStoreError::Corruption(
                    "prompt chain insert effect version ownership is invalid".into(),
                ));
            }
            let position = command
                .before_link_id
                .and_then(|before| expected_links.iter().position(|link| link.id() == before))
                .unwrap_or(expected_links.len());
            if command.before_link_id.is_some() && position == expected_links.len() {
                return Err(PromptStoreError::Corruption(
                    "prompt chain insert command before-link is missing".into(),
                ));
            }
            expected_links.insert(
                position,
                PromptChainLink::store_issued(
                    command.link_id,
                    chain_id,
                    0,
                    prompt.id,
                    event_link.prompt_version_id(),
                ),
            );
        }
        PromptChainCommand::MovePromptChainLink(command) => {
            let position = expected_links
                .iter()
                .position(|link| link.id() == command.link_id)
                .ok_or_else(|| {
                    PromptStoreError::Corruption("prompt chain move link is missing".into())
                })?;
            if command.before_link_id == Some(command.link_id) {
                return Err(PromptStoreError::Corruption(
                    "prompt chain move no-op must not have an event".into(),
                ));
            }
            let moving = expected_links.remove(position);
            let target = command
                .before_link_id
                .and_then(|before| expected_links.iter().position(|link| link.id() == before))
                .unwrap_or(expected_links.len());
            if command.before_link_id.is_some() && target == expected_links.len() {
                return Err(PromptStoreError::Corruption(
                    "prompt chain move before-link is missing".into(),
                ));
            }
            expected_links.insert(target, moving);
            if expected_links == current_links {
                return Err(PromptStoreError::Corruption(
                    "prompt chain move no-op must not have an event".into(),
                ));
            }
        }
        PromptChainCommand::RemovePromptChainLink(command) => {
            let position = expected_links
                .iter()
                .position(|link| link.id() == command.link_id)
                .ok_or_else(|| {
                    PromptStoreError::Corruption("prompt chain remove link is missing".into())
                })?;
            expected_links.remove(position);
        }
        PromptChainCommand::UpdatePromptChainLinkVersion(command) => {
            let position = expected_links
                .iter()
                .position(|link| link.id() == command.link_id)
                .ok_or_else(|| {
                    PromptStoreError::Corruption("prompt chain update link is missing".into())
                })?;
            let prompt =
                load_prompt(tx, expected_links[position].prompt_id())?.ok_or_else(|| {
                    PromptStoreError::Corruption("prompt chain update prompt is missing".into())
                })?;
            validate_saved_prompt_record(tx, &prompt)?;
            let event_link = links
                .iter()
                .find(|link| link.id() == command.link_id)
                .ok_or_else(|| {
                    PromptStoreError::Corruption(
                        "prompt chain update effect is missing its command link".into(),
                    )
                })?;
            if event_link.prompt_id() != expected_links[position].prompt_id()
                || event_link.chain_id() != chain_id
            {
                return Err(PromptStoreError::Corruption(
                    "prompt chain update effect target is invalid".into(),
                ));
            }
            if expected_links[position].prompt_version_id() == event_link.prompt_version_id() {
                return Err(PromptStoreError::Corruption(
                    "prompt chain update no-op must not have an event".into(),
                ));
            }
            let version = load_version(tx, event_link.prompt_version_id())?.ok_or_else(|| {
                PromptStoreError::Corruption("prompt chain update effect version is missing".into())
            })?;
            if version.prompt_id != prompt.id {
                return Err(PromptStoreError::Corruption(
                    "prompt chain update effect version ownership is invalid".into(),
                ));
            }
            let current = &expected_links[position];
            expected_links[position] = PromptChainLink::store_issued(
                current.id(),
                current.chain_id(),
                current.position(),
                current.prompt_id(),
                event_link.prompt_version_id(),
            );
        }
        _ => unreachable!(),
    }
    renumber_links(&mut expected_links)?;
    if expected_links != *links {
        return Err(PromptStoreError::Corruption(
            "prompt chain event links do not match its exact command".into(),
        ));
    }
    Ok(())
}

fn apply_chain_event(
    tx: &Transaction<'_>,
    event: &PromptChainEvent,
    occurred_at_ms: i64,
    command: &PromptChainCommand,
    resolved_prompt_version_id: Option<PromptVersionId>,
) -> Result<(), PromptStoreError> {
    validate_chain_command_effect(tx, command, resolved_prompt_version_id, event)?;
    match event {
        PromptChainEvent::PromptChainCreated { chain } => {
            if chain.revision != 1
                || chain.archived_at_ms.is_some()
                || trim_prompt_whitespace(&chain.title).is_empty()
                || chain.title != trim_prompt_whitespace(&chain.title)
                || chain.title.chars().count() > MAX_PROMPT_CHAIN_TITLE_SCALARS
                || chain.description.as_deref().is_some_and(|description| {
                    description.chars().count() > MAX_PROMPT_CHAIN_DESCRIPTION_SCALARS
                        || description != trim_prompt_whitespace(description)
                })
            {
                return Err(PromptStoreError::Corruption(
                    "prompt chain.created metadata is invalid".into(),
                ));
            }
            insert_chain(tx, chain, occurred_at_ms)?;
        }
        PromptChainEvent::PromptChainRenamed {
            chain_id,
            title,
            revision,
        } => {
            let chain = load_chain(tx, *chain_id)?.ok_or_else(|| {
                PromptStoreError::Corruption("prompt chain rename references missing chain".into())
            })?;
            if chain.archived_at_ms.is_some()
                || *revision != next_revision(chain.revision)?
                || trim_prompt_whitespace(title).is_empty()
                || title != trim_prompt_whitespace(title)
                || title.chars().count() > MAX_PROMPT_CHAIN_TITLE_SCALARS
            {
                return Err(PromptStoreError::Corruption(
                    "prompt chain rename event is invalid".into(),
                ));
            }
            update_chain_metadata(
                tx,
                &chain,
                title,
                chain.description.as_deref(),
                chain.archived_at_ms,
                *revision,
                occurred_at_ms,
            )?;
        }
        PromptChainEvent::PromptChainLinksReplaced {
            chain_id,
            links,
            revision,
        } => {
            let chain = load_chain(tx, *chain_id)?.ok_or_else(|| {
                PromptStoreError::Corruption(
                    "prompt chain links event references missing chain".into(),
                )
            })?;
            if chain.archived_at_ms.is_some() || *revision != next_revision(chain.revision)? {
                return Err(PromptStoreError::Corruption(
                    "prompt chain links event revision is invalid".into(),
                ));
            }
            write_chain_links(tx, &chain, links, *revision, occurred_at_ms)?;
        }
        PromptChainEvent::PromptChainArchived {
            chain_id,
            archived_at_ms,
            revision,
        } => {
            let chain = load_chain(tx, *chain_id)?.ok_or_else(|| {
                PromptStoreError::Corruption("prompt chain archive references missing chain".into())
            })?;
            if chain.archived_at_ms.is_some() || *revision != next_revision(chain.revision)? {
                return Err(PromptStoreError::Corruption(
                    "prompt chain archive event is invalid".into(),
                ));
            }
            update_chain_metadata(
                tx,
                &chain,
                &chain.title,
                chain.description.as_deref(),
                Some(*archived_at_ms),
                *revision,
                occurred_at_ms,
            )?;
        }
        PromptChainEvent::PromptChainRestored { chain_id, revision } => {
            let chain = load_chain(tx, *chain_id)?.ok_or_else(|| {
                PromptStoreError::Corruption("prompt chain restore references missing chain".into())
            })?;
            if chain.archived_at_ms.is_none() || *revision != next_revision(chain.revision)? {
                return Err(PromptStoreError::Corruption(
                    "prompt chain restore event is invalid".into(),
                ));
            }
            update_chain_metadata(
                tx,
                &chain,
                &chain.title,
                chain.description.as_deref(),
                None,
                *revision,
                occurred_at_ms,
            )?;
        }
    }
    Ok(())
}

fn insert_saved_prompt(
    tx: &Transaction<'_>,
    prompt: &SavedPrompt,
    created_at_ms: i64,
) -> Result<(), PromptStoreError> {
    tx.execute(
        "INSERT INTO saved_prompts(
            prompt_id, title, description, current_version_id, revision,
            created_at_ms, updated_at_ms, archived_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?7)",
        rusqlite::params![
            prompt.id.as_bytes().as_slice(),
            prompt.title,
            prompt.description,
            prompt.current_version_id.as_bytes().as_slice(),
            to_i64(prompt.revision)?,
            created_at_ms,
            prompt.archived_at_ms,
        ],
    )?;
    Ok(())
}

fn insert_version(tx: &Transaction<'_>, version: &PromptVersion) -> Result<(), PromptStoreError> {
    validate_version_record(version)?;
    tx.execute(
        "INSERT INTO prompt_versions(
            prompt_version_id, prompt_id, version, body, body_sha256, created_at_ms,
            variables_sealed
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0)",
        rusqlite::params![
            version.id.as_bytes().as_slice(),
            version.prompt_id.as_bytes().as_slice(),
            i64::from(version.version),
            version.body,
            version.body_sha256.as_slice(),
            version.created_at_ms,
        ],
    )?;
    for (position, variable) in version.variables.iter().enumerate() {
        tx.execute(
            "INSERT INTO prompt_version_variables(
                prompt_version_id, variable, position
             ) VALUES (?1, ?2, ?3)",
            rusqlite::params![
                version.id.as_bytes().as_slice(),
                variable,
                to_i64(position)?,
            ],
        )?;
    }
    tx.execute(
        "UPDATE prompt_versions SET variables_sealed = 1
         WHERE prompt_version_id = ?1 AND variables_sealed = 0",
        [version.id.as_bytes().as_slice()],
    )?;
    Ok(())
}

fn ensure_version(tx: &Transaction<'_>, version: &PromptVersion) -> Result<(), PromptStoreError> {
    if let Some(existing) = load_version(tx, version.id)? {
        if existing == *version {
            return Ok(());
        }
        return Err(PromptStoreError::Corruption(
            "immutable prompt version does not match its event".into(),
        ));
    }
    insert_version(tx, version)
}

fn insert_tags(
    tx: &Transaction<'_>,
    prompt_id: PromptId,
    tags: &[String],
) -> Result<(), PromptStoreError> {
    for (position, tag) in tags.iter().enumerate() {
        tx.execute(
            "INSERT INTO prompt_tags(prompt_id, tag, position) VALUES (?1, ?2, ?3)",
            rusqlite::params![prompt_id.as_bytes().as_slice(), tag, to_i64(position)?,],
        )?;
    }
    Ok(())
}

fn update_current_version(
    tx: &Transaction<'_>,
    prompt: &SavedPrompt,
    version_id: PromptVersionId,
    revision: u64,
    updated_at_ms: i64,
) -> Result<(), PromptStoreError> {
    let changed = tx.execute(
        "UPDATE saved_prompts SET current_version_id = ?1, revision = ?2, updated_at_ms = ?3
         WHERE prompt_id = ?4 AND revision = ?5",
        rusqlite::params![
            version_id.as_bytes().as_slice(),
            to_i64(revision)?,
            updated_at_ms,
            prompt.id.as_bytes().as_slice(),
            to_i64(prompt.revision)?,
        ],
    )?;
    if changed == 1 {
        Ok(())
    } else {
        Err(PromptStoreError::RevisionConflict {
            expected: prompt.revision,
            actual: prompt.revision,
        })
    }
}

fn update_archive(
    tx: &Transaction<'_>,
    prompt: &SavedPrompt,
    archived_at_ms: Option<i64>,
    revision: u64,
    updated_at_ms: i64,
) -> Result<(), PromptStoreError> {
    let changed = tx.execute(
        "UPDATE saved_prompts SET archived_at_ms = ?1, revision = ?2, updated_at_ms = ?3
         WHERE prompt_id = ?4 AND revision = ?5",
        rusqlite::params![
            archived_at_ms,
            to_i64(revision)?,
            updated_at_ms,
            prompt.id.as_bytes().as_slice(),
            to_i64(prompt.revision)?,
        ],
    )?;
    if changed == 1 {
        Ok(())
    } else {
        Err(PromptStoreError::RevisionConflict {
            expected: prompt.revision,
            actual: prompt.revision,
        })
    }
}

fn load_receipt(
    tx: &Transaction<'_>,
    command_id: CommandId,
    command: &PromptCommand,
    command_sha256: &[u8; 32],
) -> Result<Option<PromptMutationReceipt>, PromptStoreError> {
    let row: Option<(Vec<u8>, Option<Vec<u8>>, Vec<u8>, Vec<u8>, Vec<u8>, i64)> = tx
        .query_row(
            "SELECT command_sha256, command_payload, prompt_id, prompt_version_id, receipt, revision
             FROM prompt_command_receipts WHERE command_id = ?1",
            [command_id.as_bytes().as_slice()],
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
        .optional()?;
    let Some((
        stored_hash,
        stored_command_payload,
        stored_prompt_id,
        stored_version_id,
        receipt_payload,
        stored_revision,
    )) = row
    else {
        return Ok(None);
    };
    if stored_hash.len() != 32 {
        return Err(PromptStoreError::Corruption(
            "prompt receipt command hash must be 32 bytes".into(),
        ));
    }
    let Some(stored_command_payload) = stored_command_payload else {
        return Err(PromptStoreError::Corruption(
            "prompt receipt command payload is missing".into(),
        ));
    };
    validate_wire_size("prompt command", &stored_command_payload)?;
    if stored_command_payload.is_empty() {
        return Err(PromptStoreError::Corruption(
            "prompt receipt command payload is empty".into(),
        ));
    }
    validate_wire_size("prompt receipt", &receipt_payload)?;
    if stored_hash.as_slice() != command_sha256 {
        return Err(PromptStoreError::IdempotencyConflict);
    }
    let stored_command = PromptCommand::decode(&stored_command_payload)
        .map_err(|error| PromptStoreError::Corruption(error.to_string()))?;
    if sha256_bytes(&stored_command_payload).as_slice() != stored_hash.as_slice()
        || stored_command
            .encode()
            .map_err(|error| PromptStoreError::Corruption(error.to_string()))?
            != stored_command_payload
    {
        return Err(PromptStoreError::Corruption(
            "prompt receipt command payload digest or encoding is invalid".into(),
        ));
    }
    let command_payload = command
        .encode()
        .map_err(|error| PromptStoreError::Corruption(error.to_string()))?;
    if stored_command_payload != command_payload {
        return Err(PromptStoreError::IdempotencyConflict);
    }
    let receipt = PromptMutationReceipt::decode(&receipt_payload)
        .map_err(|error| PromptStoreError::Corruption(error.to_string()))?;
    if receipt
        .encode()
        .map_err(|error| PromptStoreError::Corruption(error.to_string()))?
        != receipt_payload
    {
        return Err(PromptStoreError::Corruption(
            "prompt receipt payload is not canonical".into(),
        ));
    }
    let stored_prompt_id = prompt_id_from_bytes(&stored_prompt_id)?;
    let stored_version_id = version_id_from_bytes(&stored_version_id)?;
    let stored_revision = from_i64("prompt receipt.revision", stored_revision)?;
    if receipt.command_id != command_id
        || receipt.prompt_id != stored_prompt_id
        || receipt.prompt_version_id != stored_version_id
        || receipt.revision != stored_revision
    {
        return Err(PromptStoreError::Corruption(
            "prompt receipt fields disagree with their row".into(),
        ));
    }
    validate_prompt_receipt_command(tx, &stored_command, &receipt)?;
    validate_prompt_receipt_effect(tx, command_id, &stored_command, &receipt)?;
    Ok(Some(receipt))
}

fn load_chain_receipt(
    tx: &Transaction<'_>,
    command_id: CommandId,
    original_command: &PromptChainCommand,
    command: &PromptChainCommand,
    command_sha256: &[u8; 32],
    resolved_prompt_version_id: Option<PromptVersionId>,
) -> Result<Option<PromptChainMutationReceipt>, PromptStoreError> {
    let row: Option<(
        Vec<u8>,
        Option<Vec<u8>>,
        Vec<u8>,
        Option<Vec<u8>>,
        Vec<u8>,
        i64,
    )> = tx
        .query_row(
            "SELECT command_sha256, command_payload, chain_id, chain_link_id, receipt, revision
             FROM prompt_chain_command_receipts WHERE command_id = ?1",
            [command_id.as_bytes().as_slice()],
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
        .optional()?;
    let Some((
        stored_hash,
        stored_command_payload,
        stored_chain_id,
        stored_link_id,
        receipt_payload,
        stored_revision,
    )) = row
    else {
        return Ok(None);
    };
    if stored_hash.len() != 32 {
        return Err(PromptStoreError::Corruption(
            "prompt chain receipt command hash must be 32 bytes".into(),
        ));
    }
    let Some(stored_command_payload) = stored_command_payload else {
        return Err(PromptStoreError::Corruption(
            "prompt chain receipt command payload is missing".into(),
        ));
    };
    validate_wire_size("prompt chain command", &stored_command_payload)?;
    if stored_command_payload.is_empty() {
        return Err(PromptStoreError::Corruption(
            "prompt chain receipt command payload is empty".into(),
        ));
    }
    validate_wire_size("prompt chain receipt", &receipt_payload)?;
    if stored_hash.as_slice() != command_sha256 {
        return Err(PromptStoreError::IdempotencyConflict);
    }
    let stored_command = decode_chain_command(&stored_command_payload)?;
    if sha256_bytes(&stored_command_payload).as_slice() != stored_hash.as_slice()
        || encode_chain_command(
            &stored_command.original_command,
            &stored_command.command,
            stored_command.resolved_prompt_version_id,
        )? != stored_command_payload
    {
        return Err(PromptStoreError::Corruption(
            "prompt chain receipt command payload digest or encoding is invalid".into(),
        ));
    }
    let original_command_hash = original_command
        .fingerprint()
        .map_err(|error| PromptStoreError::Corruption(error.to_string()))?;
    if original_command_hash != stored_command.original_command_sha256
        || stored_command.original_command != *original_command
        || stored_command.resolved_prompt_version_id != resolved_prompt_version_id
        || stored_command.command != *command
    {
        return Err(PromptStoreError::IdempotencyConflict);
    }
    let command_payload =
        encode_chain_command(original_command, command, resolved_prompt_version_id)?;
    if stored_command_payload != command_payload {
        return Err(PromptStoreError::IdempotencyConflict);
    }
    let receipt = PromptChainMutationReceipt::decode(&receipt_payload)
        .map_err(|error| PromptStoreError::Corruption(error.to_string()))?;
    if receipt
        .encode()
        .map_err(|error| PromptStoreError::Corruption(error.to_string()))?
        != receipt_payload
    {
        return Err(PromptStoreError::Corruption(
            "prompt chain receipt payload is not canonical".into(),
        ));
    }
    let stored_chain_id = prompt_chain_id_from_bytes(&stored_chain_id)?;
    let stored_link_id = stored_link_id
        .as_deref()
        .map(prompt_chain_link_id_from_bytes)
        .transpose()?;
    let stored_revision = from_i64("prompt chain receipt.revision", stored_revision)?;
    if receipt.command_id != command_id
        || receipt.chain_id != stored_chain_id
        || receipt.link_id != stored_link_id
        || receipt.revision != stored_revision
    {
        return Err(PromptStoreError::Corruption(
            "prompt chain receipt fields disagree with their row".into(),
        ));
    }
    validate_chain_receipt_command(tx, &stored_command.command, &receipt)?;
    validate_chain_receipt_effect(
        tx,
        command_id,
        &stored_command.command,
        stored_command.resolved_prompt_version_id,
        &receipt,
    )?;
    Ok(Some(receipt))
}

fn validate_prompt_event_row(
    tx: &Transaction<'_>,
    event_id_bytes: &[u8],
    command_id_bytes: &[u8],
    row_prompt_id_bytes: &[u8],
    row_event_type: &str,
    occurred_at_ms: i64,
    payload: &[u8],
) -> Result<(PromptEvent, i64, PromptVersionId), PromptStoreError> {
    let _event_id = event_id_from_bytes(event_id_bytes)?;
    let command_id = command_id_from_bytes(command_id_bytes)?;
    let row_prompt_id = prompt_id_from_bytes(row_prompt_id_bytes)?;
    let receipt_row: Option<(
        Vec<u8>,
        Vec<u8>,
        Option<Vec<u8>>,
        Vec<u8>,
        Vec<u8>,
        i64,
        Vec<u8>,
        i64,
    )> = tx
        .query_row(
            "SELECT command_id, command_sha256, command_payload, prompt_id,
                    prompt_version_id, revision, receipt, created_at_ms
             FROM prompt_command_receipts WHERE command_id = ?1",
            [command_id.as_bytes().as_slice()],
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
        .optional()?;
    let Some((
        stored_command_id,
        command_sha256,
        command_payload,
        stored_prompt_id,
        stored_version_id,
        revision,
        receipt_payload,
        created_at_ms,
    )) = receipt_row
    else {
        return Err(PromptStoreError::Corruption(
            "prompt event references a missing command receipt".into(),
        ));
    };
    if command_sha256.len() != 32 {
        return Err(PromptStoreError::Corruption(
            "prompt event command hash must be 32 bytes".into(),
        ));
    }
    let Some(command_payload) = command_payload else {
        return Err(PromptStoreError::Corruption(
            "prompt event command payload is missing".into(),
        ));
    };
    validate_wire_size("prompt command", &command_payload)?;
    if command_payload.is_empty() {
        return Err(PromptStoreError::Corruption(
            "prompt event command payload is empty".into(),
        ));
    }
    validate_wire_size("prompt receipt", &receipt_payload)?;
    validate_wire_size("prompt event", payload)?;
    if command_id_from_bytes(&stored_command_id)? != command_id {
        return Err(PromptStoreError::Corruption(
            "prompt event command receipt key mismatch".into(),
        ));
    }
    let command = PromptCommand::decode(&command_payload)
        .map_err(|error| PromptStoreError::Corruption(error.to_string()))?;
    if sha256_bytes(&command_payload).as_slice() != command_sha256.as_slice()
        || command
            .encode()
            .map_err(|error| PromptStoreError::Corruption(error.to_string()))?
            != command_payload
    {
        return Err(PromptStoreError::Corruption(
            "prompt event command payload digest or encoding is invalid".into(),
        ));
    }
    let receipt = PromptMutationReceipt::decode(&receipt_payload)
        .map_err(|error| PromptStoreError::Corruption(error.to_string()))?;
    if receipt
        .encode()
        .map_err(|error| PromptStoreError::Corruption(error.to_string()))?
        != receipt_payload
    {
        return Err(PromptStoreError::Corruption(
            "prompt receipt payload is not canonical".into(),
        ));
    }
    let stored_prompt_id = prompt_id_from_bytes(&stored_prompt_id)?;
    let stored_version_id = version_id_from_bytes(&stored_version_id)?;
    let revision = from_i64("prompt event receipt.revision", revision)?;
    if receipt.command_id != command_id
        || receipt.prompt_id != stored_prompt_id
        || receipt.prompt_version_id != stored_version_id
        || receipt.revision != revision
        || row_prompt_id != receipt.prompt_id
        || occurred_at_ms != created_at_ms
    {
        return Err(PromptStoreError::Corruption(
            "prompt event row disagrees with its command receipt".into(),
        ));
    }
    let stored_version = load_version(tx, receipt.prompt_version_id)?.ok_or_else(|| {
        PromptStoreError::Corruption("prompt event receipt references a missing version".into())
    })?;
    if stored_version.prompt_id != receipt.prompt_id {
        return Err(PromptStoreError::Corruption(
            "prompt event receipt version ownership mismatch".into(),
        ));
    }
    let event = PromptEvent::decode(payload)
        .map_err(|error| PromptStoreError::Corruption(error.to_string()))?;
    if event
        .encode()
        .map_err(|error| PromptStoreError::Corruption(error.to_string()))?
        != payload
    {
        return Err(PromptStoreError::Corruption(
            "prompt event payload is not canonical".into(),
        ));
    }
    if event.event_type() != row_event_type {
        return Err(PromptStoreError::Corruption(
            "prompt event type disagrees with payload".into(),
        ));
    }
    validate_prompt_event_payload(
        &event,
        row_prompt_id,
        &stored_version,
        receipt.prompt_version_id,
        receipt.revision,
        occurred_at_ms,
    )?;
    validate_prompt_event_command(&command, &event, &receipt, occurred_at_ms)?;
    Ok((event, occurred_at_ms, receipt.prompt_version_id))
}

fn validate_prompt_event_payload(
    event: &PromptEvent,
    row_prompt_id: PromptId,
    stored_version: &PromptVersion,
    receipt_version_id: PromptVersionId,
    receipt_revision: u64,
    occurred_at_ms: i64,
) -> Result<(), PromptStoreError> {
    let invalid = || PromptStoreError::Corruption("prompt event payload lineage is invalid".into());
    match event {
        PromptEvent::PromptCreated { prompt, version } => {
            if prompt.id != row_prompt_id
                || prompt.revision != 1
                || prompt.current_version_id != version.id
                || version.id != receipt_version_id
                || version.prompt_id != row_prompt_id
                || version.version != 1
                || prompt.revision != receipt_revision
                || version != stored_version
            {
                return Err(invalid());
            }
        }
        PromptEvent::PromptVersionCreated {
            prompt_id,
            version,
            revision,
        } => {
            if *prompt_id != row_prompt_id
                || version.id != receipt_version_id
                || version.prompt_id != row_prompt_id
                || *revision != receipt_revision
                || version != stored_version
            {
                return Err(invalid());
            }
        }
        PromptEvent::PromptRenamed {
            prompt_id,
            revision,
            ..
        }
        | PromptEvent::PromptTagsSet {
            prompt_id,
            revision,
            ..
        }
        | PromptEvent::PromptRestored {
            prompt_id,
            revision,
        } => {
            if *prompt_id != row_prompt_id || *revision != receipt_revision {
                return Err(invalid());
            }
        }
        PromptEvent::PromptArchived {
            prompt_id,
            archived_at_ms,
            revision,
        } => {
            if *prompt_id != row_prompt_id
                || *revision != receipt_revision
                || *archived_at_ms != occurred_at_ms
            {
                return Err(invalid());
            }
        }
    }
    Ok(())
}

fn validate_prompt_event_command(
    command: &PromptCommand,
    event: &PromptEvent,
    receipt: &PromptMutationReceipt,
    occurred_at_ms: i64,
) -> Result<(), PromptStoreError> {
    let invalid = || PromptStoreError::Corruption("prompt command/event lineage is invalid".into());
    match (command, event) {
        (PromptCommand::CreatePrompt(command), PromptEvent::PromptCreated { prompt, version }) => {
            let expected_version = PromptVersion::new_with_variables(
                command.prompt_version_id,
                command.prompt_id,
                1,
                command.body.clone(),
                command.normalized_variables()?,
                command.created_at_ms,
            )?;
            let expected_prompt = SavedPrompt {
                id: command.prompt_id,
                title: trim_prompt_whitespace(&command.title).to_string(),
                description: command
                    .description
                    .as_deref()
                    .map(trim_prompt_whitespace)
                    .map(str::to_string),
                tags: normalized_tags(&command.tags)?,
                current_version_id: command.prompt_version_id,
                revision: 1,
                archived_at_ms: None,
            };
            if receipt.revision != 1
                || occurred_at_ms != command.created_at_ms
                || prompt != &expected_prompt
                || version != &expected_version
            {
                return Err(invalid());
            }
        }
        (
            PromptCommand::CreatePromptVersion(command),
            PromptEvent::PromptVersionCreated {
                prompt_id,
                version,
                revision,
            },
        ) => {
            let expected_version = PromptVersion::new_with_variables(
                command.prompt_version_id,
                command.prompt_id,
                version.version,
                command.body.clone(),
                command.normalized_variables()?,
                command.created_at_ms,
            )?;
            if *prompt_id != command.prompt_id
                || *revision != next_revision(command.expected_revision)?
                || receipt.revision != *revision
                || occurred_at_ms != command.created_at_ms
                || version != &expected_version
            {
                return Err(invalid());
            }
        }
        (
            PromptCommand::RenamePrompt(command),
            PromptEvent::PromptRenamed {
                prompt_id,
                title,
                revision,
            },
        ) => {
            if *prompt_id != command.prompt_id
                || title != trim_prompt_whitespace(&command.title)
                || *revision != next_revision(command.expected_revision)?
                || receipt.revision != *revision
            {
                return Err(invalid());
            }
        }
        (
            PromptCommand::SetPromptTags(command),
            PromptEvent::PromptTagsSet {
                prompt_id,
                tags,
                revision,
            },
        ) => {
            if *prompt_id != command.prompt_id
                || tags != &command.validate()?
                || *revision != next_revision(command.expected_revision)?
                || receipt.revision != *revision
            {
                return Err(invalid());
            }
        }
        (
            PromptCommand::ArchivePrompt(command),
            PromptEvent::PromptArchived {
                prompt_id,
                archived_at_ms,
                revision,
            },
        ) => {
            if *prompt_id != command.prompt_id
                || *archived_at_ms != command.archived_at_ms
                || *revision != next_revision(command.expected_revision)?
                || receipt.revision != *revision
                || occurred_at_ms != command.archived_at_ms
            {
                return Err(invalid());
            }
        }
        (
            PromptCommand::RestorePrompt(command),
            PromptEvent::PromptRestored {
                prompt_id,
                revision,
            },
        ) => {
            if *prompt_id != command.prompt_id
                || *revision != next_revision(command.expected_revision)?
                || receipt.revision != *revision
            {
                return Err(invalid());
            }
        }
        _ => return Err(invalid()),
    }
    Ok(())
}

fn validate_prompt_event_temporal_lineage(
    tx: &Transaction<'_>,
    event: &PromptEvent,
    receipt_version_id: PromptVersionId,
    prompt_version_cursors: &mut HashMap<PromptId, PromptVersionId>,
) -> Result<(), PromptStoreError> {
    match event {
        PromptEvent::PromptCreated { prompt, version } => {
            if version.id != receipt_version_id
                || version.prompt_id != prompt.id
                || prompt_version_cursors
                    .insert(prompt.id, version.id)
                    .is_some()
            {
                return Err(PromptStoreError::Corruption(
                    "prompt event version receipt lineage is invalid".into(),
                ));
            }
        }
        PromptEvent::PromptVersionCreated {
            prompt_id, version, ..
        } => {
            if version.id != receipt_version_id || version.prompt_id != *prompt_id {
                return Err(PromptStoreError::Corruption(
                    "prompt event version receipt lineage is invalid".into(),
                ));
            }
            let Some(previous_version_id) = prompt_version_cursors.get(prompt_id).copied() else {
                return Err(PromptStoreError::Corruption(
                    "prompt version event has no replay-time predecessor".into(),
                ));
            };
            let Some(previous_version_number) = version.version.checked_sub(1) else {
                return Err(PromptStoreError::Corruption(
                    "prompt version event sequence is invalid".into(),
                ));
            };
            let durable_previous_id: Option<Vec<u8>> = tx
                .query_row(
                    "SELECT prompt_version_id
                     FROM prompt_versions
                     WHERE prompt_id = ?1 AND version = ?2",
                    rusqlite::params![
                        prompt_id.as_bytes().as_slice(),
                        i64::from(previous_version_number),
                    ],
                    |row| row.get(0),
                )
                .optional()?;
            let Some(durable_previous_id) = durable_previous_id else {
                return Err(PromptStoreError::Corruption(
                    "prompt version event predecessor is missing".into(),
                ));
            };
            if durable_previous_id.as_slice() != previous_version_id.as_bytes() {
                return Err(PromptStoreError::Corruption(
                    "prompt version event predecessor disagrees with replay cursor".into(),
                ));
            }
            prompt_version_cursors.insert(*prompt_id, version.id);
        }
        PromptEvent::PromptRenamed { prompt_id, .. }
        | PromptEvent::PromptTagsSet { prompt_id, .. }
        | PromptEvent::PromptArchived { prompt_id, .. }
        | PromptEvent::PromptRestored { prompt_id, .. } => {
            if prompt_version_cursors.get(prompt_id).copied() != Some(receipt_version_id) {
                return Err(PromptStoreError::Corruption(
                    "prompt event version receipt lineage is invalid".into(),
                ));
            }
        }
    }
    Ok(())
}

fn validate_chain_event_row(
    tx: &Transaction<'_>,
    event_id_bytes: &[u8],
    command_id_bytes: &[u8],
    row_chain_id_bytes: &[u8],
    row_event_type: &str,
    occurred_at_ms: i64,
    payload: &[u8],
) -> Result<
    (
        PromptChainEvent,
        i64,
        PromptChainCommand,
        Option<PromptVersionId>,
    ),
    PromptStoreError,
> {
    let _event_id = event_id_from_bytes(event_id_bytes)?;
    let command_id = command_id_from_bytes(command_id_bytes)?;
    let row_chain_id = prompt_chain_id_from_bytes(row_chain_id_bytes)?;
    let receipt_row: Option<(
        Vec<u8>,
        Vec<u8>,
        Option<Vec<u8>>,
        Vec<u8>,
        Option<Vec<u8>>,
        i64,
        Vec<u8>,
        i64,
    )> = tx
        .query_row(
            "SELECT command_id, command_sha256, command_payload, chain_id,
                    chain_link_id, revision, receipt, created_at_ms
             FROM prompt_chain_command_receipts WHERE command_id = ?1",
            [command_id.as_bytes().as_slice()],
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
        .optional()?;
    let Some((
        stored_command_id,
        command_sha256,
        command_payload,
        stored_chain_id,
        stored_link_id,
        revision,
        receipt_payload,
        created_at_ms,
    )) = receipt_row
    else {
        return Err(PromptStoreError::Corruption(
            "prompt chain event references a missing command receipt".into(),
        ));
    };
    if command_sha256.len() != 32 {
        return Err(PromptStoreError::Corruption(
            "prompt chain event command hash must be 32 bytes".into(),
        ));
    }
    let Some(command_payload) = command_payload else {
        return Err(PromptStoreError::Corruption(
            "prompt chain event command payload is missing".into(),
        ));
    };
    validate_wire_size("prompt chain command", &command_payload)?;
    if command_payload.is_empty() {
        return Err(PromptStoreError::Corruption(
            "prompt chain event command payload is empty".into(),
        ));
    }
    validate_wire_size("prompt chain receipt", &receipt_payload)?;
    validate_wire_size("prompt chain event", payload)?;
    if command_id_from_bytes(&stored_command_id)? != command_id {
        return Err(PromptStoreError::Corruption(
            "prompt chain event command receipt key mismatch".into(),
        ));
    }
    let command = decode_chain_command(&command_payload)?;
    if sha256_bytes(&command_payload).as_slice() != command_sha256.as_slice()
        || encode_chain_command(
            &command.original_command,
            &command.command,
            command.resolved_prompt_version_id,
        )? != command_payload
    {
        return Err(PromptStoreError::Corruption(
            "prompt chain event command payload digest or encoding is invalid".into(),
        ));
    }
    let receipt = PromptChainMutationReceipt::decode(&receipt_payload)
        .map_err(|error| PromptStoreError::Corruption(error.to_string()))?;
    if receipt
        .encode()
        .map_err(|error| PromptStoreError::Corruption(error.to_string()))?
        != receipt_payload
    {
        return Err(PromptStoreError::Corruption(
            "prompt chain receipt payload is not canonical".into(),
        ));
    }
    let stored_chain_id = prompt_chain_id_from_bytes(&stored_chain_id)?;
    let stored_link_id = stored_link_id
        .as_deref()
        .map(prompt_chain_link_id_from_bytes)
        .transpose()?;
    let revision = from_i64("prompt chain event receipt.revision", revision)?;
    if receipt.command_id != command_id
        || receipt.chain_id != stored_chain_id
        || receipt.link_id != stored_link_id
        || receipt.revision != revision
        || row_chain_id != receipt.chain_id
        || occurred_at_ms != created_at_ms
    {
        return Err(PromptStoreError::Corruption(
            "prompt chain event row disagrees with its command receipt".into(),
        ));
    }
    let event = decode_prompt_chain_event(payload)?;
    if event
        .encode()
        .map_err(|error| PromptStoreError::Corruption(error.to_string()))?
        != payload
    {
        return Err(PromptStoreError::Corruption(
            "prompt chain event payload is not canonical".into(),
        ));
    }
    if event.event_type() != row_event_type {
        return Err(PromptStoreError::Corruption(
            "prompt chain event type disagrees with payload".into(),
        ));
    }
    validate_chain_event_payload(&event, row_chain_id, receipt.revision, occurred_at_ms)?;
    validate_chain_event_command(
        &command.command,
        command.resolved_prompt_version_id,
        &event,
        &receipt,
        occurred_at_ms,
    )?;
    Ok((
        event,
        occurred_at_ms,
        command.command,
        command.resolved_prompt_version_id,
    ))
}

fn validate_chain_event_payload(
    event: &PromptChainEvent,
    row_chain_id: PromptChainId,
    receipt_revision: u64,
    occurred_at_ms: i64,
) -> Result<(), PromptStoreError> {
    let invalid =
        || PromptStoreError::Corruption("prompt chain event payload lineage is invalid".into());
    match event {
        PromptChainEvent::PromptChainCreated { chain } => {
            if chain.id != row_chain_id || chain.revision != 1 || chain.revision != receipt_revision
            {
                return Err(invalid());
            }
        }
        PromptChainEvent::PromptChainRenamed {
            chain_id, revision, ..
        }
        | PromptChainEvent::PromptChainLinksReplaced {
            chain_id, revision, ..
        }
        | PromptChainEvent::PromptChainRestored { chain_id, revision } => {
            if *chain_id != row_chain_id || *revision != receipt_revision {
                return Err(invalid());
            }
        }
        PromptChainEvent::PromptChainArchived {
            chain_id,
            archived_at_ms,
            revision,
        } => {
            if *chain_id != row_chain_id
                || *revision != receipt_revision
                || *archived_at_ms != occurred_at_ms
            {
                return Err(invalid());
            }
        }
    }
    Ok(())
}

fn validate_chain_event_command(
    command: &PromptChainCommand,
    resolved_prompt_version_id: Option<PromptVersionId>,
    event: &PromptChainEvent,
    receipt: &PromptChainMutationReceipt,
    occurred_at_ms: i64,
) -> Result<(), PromptStoreError> {
    let invalid =
        || PromptStoreError::Corruption("prompt chain command/event lineage is invalid".into());
    match (command, event) {
        (
            PromptChainCommand::CreatePromptChain(command),
            PromptChainEvent::PromptChainCreated { chain },
        ) => {
            if chain.id != command.chain_id
                || chain.title != trim_prompt_whitespace(&command.title)
                || chain.description
                    != command
                        .description
                        .as_deref()
                        .map(trim_prompt_whitespace)
                        .map(str::to_string)
                || chain.revision != 1
                || chain.archived_at_ms.is_some()
                || receipt.chain_id != command.chain_id
                || receipt.link_id.is_some()
                || receipt.revision != 1
                || occurred_at_ms != command.created_at_ms
            {
                return Err(invalid());
            }
        }
        (
            PromptChainCommand::RenamePromptChain(command),
            PromptChainEvent::PromptChainRenamed {
                chain_id,
                title,
                revision,
            },
        ) => {
            if *chain_id != command.chain_id
                || title != trim_prompt_whitespace(&command.title)
                || *revision != next_revision(command.expected_revision)?
                || receipt.chain_id != command.chain_id
                || receipt.link_id.is_some()
                || receipt.revision != *revision
            {
                return Err(invalid());
            }
        }
        (
            command,
            PromptChainEvent::PromptChainLinksReplaced {
                chain_id, revision, ..
            },
        ) if matches!(
            command,
            PromptChainCommand::InsertPromptChainLink(_)
                | PromptChainCommand::MovePromptChainLink(_)
                | PromptChainCommand::RemovePromptChainLink(_)
                | PromptChainCommand::UpdatePromptChainLinkVersion(_)
        ) =>
        {
            let (expected_chain_id, link_id, expected_revision) = match command {
                PromptChainCommand::InsertPromptChainLink(command) => {
                    (command.chain_id, command.link_id, command.expected_revision)
                }
                PromptChainCommand::MovePromptChainLink(command) => {
                    (command.chain_id, command.link_id, command.expected_revision)
                }
                PromptChainCommand::RemovePromptChainLink(command) => {
                    (command.chain_id, command.link_id, command.expected_revision)
                }
                PromptChainCommand::UpdatePromptChainLinkVersion(command) => {
                    (command.chain_id, command.link_id, command.expected_revision)
                }
                _ => unreachable!(),
            };
            if *chain_id != expected_chain_id
                || *revision != next_revision(expected_revision)?
                || receipt.chain_id != expected_chain_id
                || receipt.link_id != Some(link_id)
                || receipt.revision != *revision
            {
                return Err(invalid());
            }
        }
        (
            PromptChainCommand::ArchivePromptChain(command),
            PromptChainEvent::PromptChainArchived {
                chain_id,
                archived_at_ms,
                revision,
            },
        ) => {
            if *chain_id != command.chain_id
                || *archived_at_ms != command.archived_at_ms
                || *revision != next_revision(command.expected_revision)?
                || receipt.chain_id != command.chain_id
                || receipt.link_id.is_some()
                || receipt.revision != *revision
                || occurred_at_ms != command.archived_at_ms
            {
                return Err(invalid());
            }
        }
        (
            PromptChainCommand::RestorePromptChain(command),
            PromptChainEvent::PromptChainRestored { chain_id, revision },
        ) => {
            if *chain_id != command.chain_id
                || *revision != next_revision(command.expected_revision)?
                || receipt.chain_id != command.chain_id
                || receipt.link_id.is_some()
                || receipt.revision != *revision
            {
                return Err(invalid());
            }
        }
        _ => return Err(invalid()),
    }
    validate_chain_event_resolution(command, resolved_prompt_version_id, event)?;
    Ok(())
}

fn validate_chain_event_resolution(
    command: &PromptChainCommand,
    resolved_prompt_version_id: Option<PromptVersionId>,
    event: &PromptChainEvent,
) -> Result<(), PromptStoreError> {
    command
        .encode_durable(resolved_prompt_version_id)
        .map_err(|error| PromptStoreError::Corruption(error.to_string()))?;
    let expected_event_version = match command {
        PromptChainCommand::InsertPromptChainLink(command) => {
            let PromptChainEvent::PromptChainLinksReplaced { links, .. } = event else {
                return Err(PromptStoreError::Corruption(
                    "prompt chain insert command has a non-link event".into(),
                ));
            };
            links
                .iter()
                .find(|link| link.id() == command.link_id)
                .map(|link| link.prompt_version_id())
        }
        PromptChainCommand::UpdatePromptChainLinkVersion(command) => {
            let PromptChainEvent::PromptChainLinksReplaced { links, .. } = event else {
                return Err(PromptStoreError::Corruption(
                    "prompt chain update command has a non-link event".into(),
                ));
            };
            links
                .iter()
                .find(|link| link.id() == command.link_id)
                .map(|link| link.prompt_version_id())
        }
        _ => None,
    };
    if expected_event_version != resolved_prompt_version_id {
        return Err(PromptStoreError::Corruption(
            "prompt chain command resolution disagrees with its durable effect".into(),
        ));
    }
    Ok(())
}

fn count_command_effects(
    tx: &Transaction<'_>,
    table: &str,
    command_id: CommandId,
) -> Result<usize, PromptStoreError> {
    let sql = match table {
        "prompt_events" | "prompt_chain_events" => {
            format!("SELECT COUNT(*) FROM {table} WHERE command_id = ?1")
        }
        _ => {
            return Err(PromptStoreError::Corruption(
                "unsupported prompt effect table".into(),
            ))
        }
    };
    let count: i64 = tx.query_row(&sql, [command_id.as_bytes().as_slice()], |row| row.get(0))?;
    usize::try_from(count)
        .map_err(|_| PromptStoreError::Corruption("negative prompt effect count".into()))
}

fn validate_prompt_receipt_effect(
    tx: &Transaction<'_>,
    command_id: CommandId,
    command: &PromptCommand,
    receipt: &PromptMutationReceipt,
) -> Result<(), PromptStoreError> {
    let effect_count = count_command_effects(tx, "prompt_events", command_id)?;
    let (expected_revision, effect_only) = match command {
        PromptCommand::CreatePrompt(_command) => (1, true),
        PromptCommand::CreatePromptVersion(command) => {
            (next_revision(command.expected_revision)?, true)
        }
        PromptCommand::RenamePrompt(command) => (command.expected_revision, false),
        PromptCommand::SetPromptTags(command) => (command.expected_revision, false),
        PromptCommand::ArchivePrompt(command) => (command.expected_revision, false),
        PromptCommand::RestorePrompt(command) => (command.expected_revision, false),
    };
    if receipt.revision == expected_revision && !effect_only {
        if effect_count != 0 {
            return Err(PromptStoreError::Corruption(
                "semantic no-op prompt receipt has a durable effect".into(),
            ));
        }
        validate_prompt_noop_state(tx, command, receipt)
    } else if receipt.revision
        == match command {
            PromptCommand::CreatePrompt(_) => 1,
            PromptCommand::CreatePromptVersion(command) => {
                next_revision(command.expected_revision)?
            }
            PromptCommand::RenamePrompt(command) => next_revision(command.expected_revision)?,
            PromptCommand::SetPromptTags(command) => next_revision(command.expected_revision)?,
            PromptCommand::ArchivePrompt(command) => next_revision(command.expected_revision)?,
            PromptCommand::RestorePrompt(command) => next_revision(command.expected_revision)?,
        }
    {
        if effect_count != 1 {
            return Err(PromptStoreError::Corruption(
                "effectful prompt receipt must have exactly one durable effect".into(),
            ));
        }
        let row: (Vec<u8>, Vec<u8>, String, i64, Vec<u8>) = tx.query_row(
            "SELECT prompt_event_id, prompt_id, event_type, occurred_at_ms, payload
             FROM prompt_events WHERE command_id = ?1",
            [command_id.as_bytes().as_slice()],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )?;
        let (event, _, _) = validate_prompt_event_row(
            tx,
            &row.0,
            &command_id.as_bytes().to_vec(),
            &row.1,
            &row.2,
            row.3,
            &row.4,
        )?;
        let _ = event;
        Ok(())
    } else {
        Err(PromptStoreError::Corruption(
            "prompt receipt revision does not prove a no-op or one effect".into(),
        ))
    }
}

fn validate_prompt_noop_state(
    tx: &Transaction<'_>,
    command: &PromptCommand,
    receipt: &PromptMutationReceipt,
) -> Result<(), PromptStoreError> {
    let prompt_id = match command {
        PromptCommand::RenamePrompt(command) => command.prompt_id,
        PromptCommand::SetPromptTags(command) => command.prompt_id,
        PromptCommand::ArchivePrompt(command) => command.prompt_id,
        PromptCommand::RestorePrompt(command) => command.prompt_id,
        PromptCommand::CreatePrompt(_) | PromptCommand::CreatePromptVersion(_) => {
            return Err(PromptStoreError::Corruption(
                "prompt creation cannot be a semantic no-op".into(),
            ))
        }
    };
    let prompt = load_prompt(tx, prompt_id)?.ok_or_else(|| {
        PromptStoreError::Corruption("semantic no-op prompt receipt target is missing".into())
    })?;
    validate_saved_prompt_record(tx, &prompt)?;
    let expected_revision = match command {
        PromptCommand::RenamePrompt(command) => command.expected_revision,
        PromptCommand::SetPromptTags(command) => command.expected_revision,
        PromptCommand::ArchivePrompt(command) => command.expected_revision,
        PromptCommand::RestorePrompt(command) => command.expected_revision,
        PromptCommand::CreatePrompt(_) | PromptCommand::CreatePromptVersion(_) => {
            return Err(PromptStoreError::Corruption(
                "prompt creation cannot be a semantic no-op".into(),
            ))
        }
    };
    if receipt.prompt_id != prompt.id || receipt.revision != expected_revision {
        return Err(PromptStoreError::Corruption(
            "semantic no-op prompt receipt does not match its immutable precondition".into(),
        ));
    }
    let durable_prompt =
        load_prompt_at_revision(tx, prompt.id, prompt.revision)?.ok_or_else(|| {
            PromptStoreError::Corruption(
                "prompt projection is missing from its durable event history".into(),
            )
        })?;
    if durable_prompt != prompt {
        return Err(PromptStoreError::Corruption(
            "prompt projection does not match its durable event history".into(),
        ));
    }
    let original_prompt =
        load_prompt_at_revision(tx, prompt.id, expected_revision)?.ok_or_else(|| {
            PromptStoreError::Corruption(
                "semantic no-op prompt precondition is missing from its event history".into(),
            )
        })?;
    if original_prompt.revision != expected_revision
        || receipt.prompt_version_id != original_prompt.current_version_id
    {
        return Err(PromptStoreError::Corruption(
            "semantic no-op prompt receipt does not match its immutable precondition".into(),
        ));
    }
    let unchanged = match command {
        PromptCommand::RenamePrompt(command) => {
            original_prompt.archived_at_ms.is_none()
                && original_prompt.title == trim_prompt_whitespace(&command.title)
        }
        PromptCommand::SetPromptTags(command) => {
            let tags = command.validate()?;
            validate_sql_tags(&tags)?;
            original_prompt.archived_at_ms.is_none() && original_prompt.tags == tags
        }
        PromptCommand::ArchivePrompt(_) => original_prompt.archived_at_ms.is_some(),
        PromptCommand::RestorePrompt(_) => original_prompt.archived_at_ms.is_none(),
        PromptCommand::CreatePrompt(_) | PromptCommand::CreatePromptVersion(_) => false,
    };
    if unchanged {
        Ok(())
    } else {
        Err(PromptStoreError::Corruption(
            "semantic no-op prompt receipt does not match exact current state".into(),
        ))
    }
}

fn load_prompt_at_revision(
    tx: &Transaction<'_>,
    prompt_id: PromptId,
    revision: u64,
) -> Result<Option<SavedPrompt>, PromptStoreError> {
    let mut statement = tx.prepare(
        "SELECT payload FROM prompt_events
         WHERE prompt_id = ?1 ORDER BY sequence ASC",
    )?;
    let rows = statement.query_map([prompt_id.as_bytes().as_slice()], |row| {
        row.get::<_, Vec<u8>>(0)
    })?;
    let mut durable_prompt = None;
    for row in rows {
        let payload = row?;
        validate_wire_size("prompt event", &payload)?;
        let event = PromptEvent::decode(&payload)
            .map_err(|error| PromptStoreError::Corruption(error.to_string()))?;
        let event_revision = match &event {
            PromptEvent::PromptCreated { prompt, .. } => prompt.revision,
            PromptEvent::PromptVersionCreated { revision, .. }
            | PromptEvent::PromptRenamed { revision, .. }
            | PromptEvent::PromptTagsSet { revision, .. }
            | PromptEvent::PromptArchived { revision, .. }
            | PromptEvent::PromptRestored { revision, .. } => *revision,
        };
        if event_revision > revision {
            break;
        }
        match event {
            PromptEvent::PromptCreated { prompt, version } => {
                if prompt.id != prompt_id
                    || version.prompt_id != prompt_id
                    || durable_prompt.is_some()
                {
                    return Err(PromptStoreError::Corruption(
                        "prompt event history has an invalid creation state".into(),
                    ));
                }
                durable_prompt = Some(prompt);
            }
            PromptEvent::PromptVersionCreated {
                prompt_id: event_prompt_id,
                version,
                revision,
            } => {
                let prompt = durable_prompt.as_mut().ok_or_else(|| {
                    PromptStoreError::Corruption(
                        "prompt version event has no creation state".into(),
                    )
                })?;
                if event_prompt_id != prompt_id
                    || version.prompt_id != prompt_id
                    || revision != next_revision(prompt.revision)?
                {
                    return Err(PromptStoreError::Corruption(
                        "prompt version event history is not contiguous".into(),
                    ));
                }
                prompt.current_version_id = version.id;
                prompt.revision = revision;
            }
            PromptEvent::PromptRenamed {
                prompt_id: event_prompt_id,
                title,
                revision,
            } => {
                let prompt = durable_prompt.as_mut().ok_or_else(|| {
                    PromptStoreError::Corruption("prompt rename event has no creation state".into())
                })?;
                if event_prompt_id != prompt_id || revision != next_revision(prompt.revision)? {
                    return Err(PromptStoreError::Corruption(
                        "prompt rename event history is not contiguous".into(),
                    ));
                }
                prompt.title = title;
                prompt.revision = revision;
            }
            PromptEvent::PromptTagsSet {
                prompt_id: event_prompt_id,
                tags,
                revision,
            } => {
                let prompt = durable_prompt.as_mut().ok_or_else(|| {
                    PromptStoreError::Corruption("prompt tags event has no creation state".into())
                })?;
                if event_prompt_id != prompt_id || revision != next_revision(prompt.revision)? {
                    return Err(PromptStoreError::Corruption(
                        "prompt tags event history is not contiguous".into(),
                    ));
                }
                prompt.tags = tags;
                prompt.revision = revision;
            }
            PromptEvent::PromptArchived {
                prompt_id: event_prompt_id,
                archived_at_ms,
                revision,
            } => {
                let prompt = durable_prompt.as_mut().ok_or_else(|| {
                    PromptStoreError::Corruption(
                        "prompt archive event has no creation state".into(),
                    )
                })?;
                if event_prompt_id != prompt_id || revision != next_revision(prompt.revision)? {
                    return Err(PromptStoreError::Corruption(
                        "prompt archive event history is not contiguous".into(),
                    ));
                }
                prompt.archived_at_ms = Some(archived_at_ms);
                prompt.revision = revision;
            }
            PromptEvent::PromptRestored {
                prompt_id: event_prompt_id,
                revision,
            } => {
                let prompt = durable_prompt.as_mut().ok_or_else(|| {
                    PromptStoreError::Corruption(
                        "prompt restore event has no creation state".into(),
                    )
                })?;
                if event_prompt_id != prompt_id || revision != next_revision(prompt.revision)? {
                    return Err(PromptStoreError::Corruption(
                        "prompt restore event history is not contiguous".into(),
                    ));
                }
                prompt.archived_at_ms = None;
                prompt.revision = revision;
            }
        }
    }
    if durable_prompt
        .as_ref()
        .is_some_and(|prompt| prompt.revision != revision)
    {
        return Ok(None);
    }
    Ok(durable_prompt)
}

fn validate_chain_receipt_effect(
    tx: &Transaction<'_>,
    command_id: CommandId,
    command: &PromptChainCommand,
    resolved_prompt_version_id: Option<PromptVersionId>,
    receipt: &PromptChainMutationReceipt,
) -> Result<(), PromptStoreError> {
    let effect_count = count_command_effects(tx, "prompt_chain_events", command_id)?;
    let expected_revision = match command {
        PromptChainCommand::CreatePromptChain(_) => 1,
        PromptChainCommand::RenamePromptChain(command) => command.expected_revision,
        PromptChainCommand::InsertPromptChainLink(command) => command.expected_revision,
        PromptChainCommand::MovePromptChainLink(command) => command.expected_revision,
        PromptChainCommand::RemovePromptChainLink(command) => command.expected_revision,
        PromptChainCommand::UpdatePromptChainLinkVersion(command) => command.expected_revision,
        PromptChainCommand::ArchivePromptChain(command) => command.expected_revision,
        PromptChainCommand::RestorePromptChain(command) => command.expected_revision,
    };
    let effect_only = matches!(
        command,
        PromptChainCommand::CreatePromptChain(_)
            | PromptChainCommand::InsertPromptChainLink(_)
            | PromptChainCommand::RemovePromptChainLink(_)
    );
    if receipt.revision == expected_revision && !effect_only {
        if effect_count != 0 {
            return Err(PromptStoreError::Corruption(
                "semantic no-op prompt chain receipt has a durable effect".into(),
            ));
        }
        validate_chain_noop_state(tx, command, resolved_prompt_version_id, receipt)
    } else if receipt.revision
        == if matches!(command, PromptChainCommand::CreatePromptChain(_)) {
            1
        } else {
            next_revision(expected_revision)?
        }
    {
        if effect_count != 1 {
            return Err(PromptStoreError::Corruption(
                "effectful prompt chain receipt must have exactly one durable effect".into(),
            ));
        }
        let row: (Vec<u8>, Vec<u8>, String, i64, Vec<u8>) = tx.query_row(
            "SELECT prompt_chain_event_id, chain_id, event_type, occurred_at_ms, payload
             FROM prompt_chain_events WHERE command_id = ?1",
            [command_id.as_bytes().as_slice()],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )?;
        let (event, _, stored_command, _) = validate_chain_event_row(
            tx,
            &row.0,
            &command_id.as_bytes().to_vec(),
            &row.1,
            &row.2,
            row.3,
            &row.4,
        )?;
        if stored_command != command.clone() {
            return Err(PromptStoreError::Corruption(
                "prompt chain effect command disagrees with its receipt command".into(),
            ));
        }
        let _ = event;
        Ok(())
    } else {
        Err(PromptStoreError::Corruption(
            "prompt chain receipt revision does not prove a no-op or one effect".into(),
        ))
    }
}

fn validate_chain_noop_state(
    tx: &Transaction<'_>,
    command: &PromptChainCommand,
    resolved_prompt_version_id: Option<PromptVersionId>,
    receipt: &PromptChainMutationReceipt,
) -> Result<(), PromptStoreError> {
    let chain_id = match command {
        PromptChainCommand::RenamePromptChain(command) => command.chain_id,
        PromptChainCommand::MovePromptChainLink(command) => command.chain_id,
        PromptChainCommand::UpdatePromptChainLinkVersion(command) => command.chain_id,
        PromptChainCommand::ArchivePromptChain(command) => command.chain_id,
        PromptChainCommand::RestorePromptChain(command) => command.chain_id,
        PromptChainCommand::CreatePromptChain(_)
        | PromptChainCommand::InsertPromptChainLink(_)
        | PromptChainCommand::RemovePromptChainLink(_) => {
            return Err(PromptStoreError::Corruption(
                "prompt chain command cannot be a semantic no-op".into(),
            ))
        }
    };
    let chain = load_chain(tx, chain_id)?.ok_or_else(|| {
        PromptStoreError::Corruption("semantic no-op prompt chain target is missing".into())
    })?;
    if receipt.chain_id != chain.id || receipt.revision != expected_chain_revision(command)? {
        return Err(PromptStoreError::Corruption(
            "semantic no-op prompt chain receipt does not match its immutable precondition".into(),
        ));
    }

    let current_links = load_chain_links(tx, chain.id)?;
    validate_chain_links(tx, chain.id, &current_links)?;
    let durable_chain =
        load_chain_metadata_at_revision(tx, chain.id, chain.revision)?.ok_or_else(|| {
            PromptStoreError::Corruption(
                "semantic no-op prompt chain current state is missing from its event history"
                    .into(),
            )
        })?;
    if durable_chain != chain {
        return Err(PromptStoreError::Corruption(
            "prompt chain projection does not match its durable event history".into(),
        ));
    }
    let durable_links =
        load_chain_links_at_revision(tx, chain.id, chain.revision)?.ok_or_else(|| {
            PromptStoreError::Corruption(
                "prompt chain link projection is missing from its event history".into(),
            )
        })?;
    if durable_links != current_links {
        return Err(PromptStoreError::Corruption(
            "prompt chain link projection does not match its durable event history".into(),
        ));
    }

    let original_chain =
        load_chain_metadata_at_revision(tx, chain.id, expected_chain_revision(command)?)?
            .ok_or_else(|| {
                PromptStoreError::Corruption(
                    "semantic no-op prompt chain precondition is missing from its event history"
                        .into(),
                )
            })?;
    let original_links =
        load_chain_links_at_revision(tx, chain.id, expected_chain_revision(command)?)?.ok_or_else(
            || {
                PromptStoreError::Corruption(
            "semantic no-op prompt chain link precondition is missing from its event history"
                .into(),
        )
            },
        )?;
    validate_chain_links(tx, chain.id, &original_links)?;
    if original_chain.revision != expected_chain_revision(command)? {
        return Err(PromptStoreError::Corruption(
            "semantic no-op prompt chain precondition revision is invalid".into(),
        ));
    }
    let unchanged = match command {
        PromptChainCommand::RenamePromptChain(command) => {
            original_chain.archived_at_ms.is_none()
                && original_chain.title == trim_prompt_whitespace(&command.title)
        }
        PromptChainCommand::MovePromptChainLink(command) => {
            if original_chain.archived_at_ms.is_some() {
                false
            } else {
                let mut moved = original_links.clone();
                let position = moved
                    .iter()
                    .position(|link| link.id() == command.link_id)
                    .ok_or_else(|| {
                        PromptStoreError::Corruption(
                            "semantic no-op move link is missing from its precondition".into(),
                        )
                    })?;
                if command.before_link_id == Some(command.link_id) {
                    true
                } else {
                    if let Some(before) = command.before_link_id {
                        if !moved.iter().any(|link| link.id() == before) {
                            return Err(PromptStoreError::Corruption(
                                "semantic no-op move before-link is missing from its precondition"
                                    .into(),
                            ));
                        }
                    }
                    let moving = moved.remove(position);
                    let target = command
                        .before_link_id
                        .and_then(|before| moved.iter().position(|link| link.id() == before))
                        .unwrap_or(moved.len());
                    moved.insert(target, moving);
                    renumber_links(&mut moved)?;
                    moved == original_links
                }
            }
        }
        PromptChainCommand::UpdatePromptChainLinkVersion(command) => {
            if original_chain.archived_at_ms.is_some() {
                false
            } else {
                let link = original_links
                    .iter()
                    .find(|link| link.id() == command.link_id)
                    .ok_or_else(|| {
                        PromptStoreError::Corruption(
                            "semantic no-op update link is missing from its precondition".into(),
                        )
                    })?;
                resolved_prompt_version_id
                    .is_some_and(|version_id| version_id == link.prompt_version_id())
            }
        }
        PromptChainCommand::ArchivePromptChain(_) => original_chain.archived_at_ms.is_some(),
        PromptChainCommand::RestorePromptChain(_) => original_chain.archived_at_ms.is_none(),
        PromptChainCommand::CreatePromptChain(_)
        | PromptChainCommand::InsertPromptChainLink(_)
        | PromptChainCommand::RemovePromptChainLink(_) => false,
    };
    if unchanged {
        Ok(())
    } else {
        Err(PromptStoreError::Corruption(
            "semantic no-op prompt chain receipt does not match exact current state".into(),
        ))
    }
}

fn expected_chain_revision(command: &PromptChainCommand) -> Result<u64, PromptStoreError> {
    match command {
        PromptChainCommand::CreatePromptChain(_) => Ok(1),
        PromptChainCommand::RenamePromptChain(command) => Ok(command.expected_revision),
        PromptChainCommand::InsertPromptChainLink(command) => Ok(command.expected_revision),
        PromptChainCommand::MovePromptChainLink(command) => Ok(command.expected_revision),
        PromptChainCommand::RemovePromptChainLink(command) => Ok(command.expected_revision),
        PromptChainCommand::UpdatePromptChainLinkVersion(command) => Ok(command.expected_revision),
        PromptChainCommand::ArchivePromptChain(command) => Ok(command.expected_revision),
        PromptChainCommand::RestorePromptChain(command) => Ok(command.expected_revision),
    }
}

fn load_chain_links_at_revision(
    tx: &Transaction<'_>,
    chain_id: PromptChainId,
    revision: u64,
) -> Result<Option<Vec<PromptChainLink>>, PromptStoreError> {
    let mut statement = tx.prepare(
        "SELECT payload FROM prompt_chain_events
         WHERE chain_id = ?1 ORDER BY sequence ASC",
    )?;
    let rows = statement.query_map([chain_id.as_bytes().as_slice()], |row| {
        row.get::<_, Vec<u8>>(0)
    })?;
    let mut durable_links = None;
    for row in rows {
        let payload = row?;
        validate_wire_size("prompt chain event", &payload)?;
        let event = decode_prompt_chain_event(&payload)?;
        let event_revision = match &event {
            PromptChainEvent::PromptChainCreated { chain } => chain.revision,
            PromptChainEvent::PromptChainLinksReplaced { revision, .. } => *revision,
            PromptChainEvent::PromptChainRenamed { revision, .. }
            | PromptChainEvent::PromptChainArchived { revision, .. }
            | PromptChainEvent::PromptChainRestored { revision, .. } => *revision,
        };
        if event_revision > revision {
            break;
        }
        match event {
            PromptChainEvent::PromptChainCreated { .. } => durable_links = Some(Vec::new()),
            PromptChainEvent::PromptChainLinksReplaced { links, .. } => durable_links = Some(links),
            PromptChainEvent::PromptChainRenamed { .. }
            | PromptChainEvent::PromptChainArchived { .. }
            | PromptChainEvent::PromptChainRestored { .. } => {}
        }
    }
    Ok(durable_links)
}

fn load_chain_metadata_at_revision(
    tx: &Transaction<'_>,
    chain_id: PromptChainId,
    revision: u64,
) -> Result<Option<PromptChain>, PromptStoreError> {
    let mut statement = tx.prepare(
        "SELECT payload FROM prompt_chain_events
         WHERE chain_id = ?1 ORDER BY sequence ASC",
    )?;
    let rows = statement.query_map([chain_id.as_bytes().as_slice()], |row| {
        row.get::<_, Vec<u8>>(0)
    })?;
    let mut durable_chain = None;
    for row in rows {
        let payload = row?;
        validate_wire_size("prompt chain event", &payload)?;
        let event = decode_prompt_chain_event(&payload)?;
        let event_revision = match &event {
            PromptChainEvent::PromptChainCreated { chain } => chain.revision,
            PromptChainEvent::PromptChainLinksReplaced { revision, .. }
            | PromptChainEvent::PromptChainRenamed { revision, .. }
            | PromptChainEvent::PromptChainArchived { revision, .. }
            | PromptChainEvent::PromptChainRestored { revision, .. } => *revision,
        };
        if event_revision > revision {
            break;
        }
        match event {
            PromptChainEvent::PromptChainCreated { chain } => {
                if chain.id != chain_id || durable_chain.is_some() {
                    return Err(PromptStoreError::Corruption(
                        "prompt chain event history has an invalid creation state".into(),
                    ));
                }
                durable_chain = Some(chain);
            }
            PromptChainEvent::PromptChainRenamed {
                chain_id: event_chain_id,
                title,
                revision,
            } => {
                let chain = durable_chain.as_mut().ok_or_else(|| {
                    PromptStoreError::Corruption(
                        "prompt chain rename event has no creation state".into(),
                    )
                })?;
                if event_chain_id != chain_id || revision != next_revision(chain.revision)? {
                    return Err(PromptStoreError::Corruption(
                        "prompt chain rename event history is not contiguous".into(),
                    ));
                }
                chain.title = title;
                chain.revision = revision;
            }
            PromptChainEvent::PromptChainLinksReplaced {
                chain_id: event_chain_id,
                revision,
                ..
            } => {
                let chain = durable_chain.as_mut().ok_or_else(|| {
                    PromptStoreError::Corruption(
                        "prompt chain link event has no creation state".into(),
                    )
                })?;
                if event_chain_id != chain_id || revision != next_revision(chain.revision)? {
                    return Err(PromptStoreError::Corruption(
                        "prompt chain link event history is not contiguous".into(),
                    ));
                }
                chain.revision = revision;
            }
            PromptChainEvent::PromptChainArchived {
                chain_id: event_chain_id,
                archived_at_ms,
                revision,
            } => {
                let chain = durable_chain.as_mut().ok_or_else(|| {
                    PromptStoreError::Corruption(
                        "prompt chain archive event has no creation state".into(),
                    )
                })?;
                if event_chain_id != chain_id || revision != next_revision(chain.revision)? {
                    return Err(PromptStoreError::Corruption(
                        "prompt chain archive event history is not contiguous".into(),
                    ));
                }
                chain.archived_at_ms = Some(archived_at_ms);
                chain.revision = revision;
            }
            PromptChainEvent::PromptChainRestored {
                chain_id: event_chain_id,
                revision,
            } => {
                let chain = durable_chain.as_mut().ok_or_else(|| {
                    PromptStoreError::Corruption(
                        "prompt chain restore event has no creation state".into(),
                    )
                })?;
                if event_chain_id != chain_id || revision != next_revision(chain.revision)? {
                    return Err(PromptStoreError::Corruption(
                        "prompt chain restore event history is not contiguous".into(),
                    ));
                }
                chain.archived_at_ms = None;
                chain.revision = revision;
            }
        }
    }
    if durable_chain
        .as_ref()
        .is_some_and(|chain| chain.revision != revision)
    {
        return Ok(None);
    }
    Ok(durable_chain)
}

fn validate_all_prompt_receipts(tx: &Transaction<'_>) -> Result<(), PromptStoreError> {
    let mut last_command_id = Vec::new();
    while let Some(command_id) = tx
        .query_row(
            "SELECT command_id FROM prompt_command_receipts
             WHERE command_id > ?1 ORDER BY command_id ASC LIMIT 1",
            [&last_command_id],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .optional()?
    {
        let row: (Vec<u8>, Option<Vec<u8>>) = tx.query_row(
            "SELECT command_sha256, command_payload
             FROM prompt_command_receipts WHERE command_id = ?1",
            [command_id.as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let Some(payload) = row.1 else {
            return Err(PromptStoreError::Corruption(
                "prompt receipt command payload is missing".into(),
            ));
        };
        validate_wire_size("prompt command", &payload)?;
        if row.0.len() != 32 || sha256_bytes(&payload).as_slice() != row.0.as_slice() {
            return Err(PromptStoreError::Corruption(
                "prompt receipt command hash is invalid".into(),
            ));
        }
        let command = PromptCommand::decode(&payload)
            .map_err(|error| PromptStoreError::Corruption(error.to_string()))?;
        let command_id = command_id_from_bytes(&command_id)?;
        let command_hash = digest_from_bytes(&row.0)?;
        load_receipt(tx, command_id, &command, &command_hash)?.ok_or_else(|| {
            PromptStoreError::Corruption("prompt receipt disappeared during rebuild".into())
        })?;
        last_command_id = command_id.as_bytes().to_vec();
    }
    Ok(())
}

fn validate_all_chain_receipts(tx: &Transaction<'_>) -> Result<(), PromptStoreError> {
    let mut last_command_id = Vec::new();
    while let Some(command_id) = tx
        .query_row(
            "SELECT command_id FROM prompt_chain_command_receipts
             WHERE command_id > ?1 ORDER BY command_id ASC LIMIT 1",
            [&last_command_id],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .optional()?
    {
        let row: (Vec<u8>, Option<Vec<u8>>) = tx.query_row(
            "SELECT command_sha256, command_payload
             FROM prompt_chain_command_receipts WHERE command_id = ?1",
            [command_id.as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let Some(payload) = row.1 else {
            return Err(PromptStoreError::Corruption(
                "prompt chain receipt command payload is missing".into(),
            ));
        };
        validate_wire_size("prompt chain command", &payload)?;
        if row.0.len() != 32 || sha256_bytes(&payload).as_slice() != row.0.as_slice() {
            return Err(PromptStoreError::Corruption(
                "prompt chain receipt command hash is invalid".into(),
            ));
        }
        let command = decode_chain_command(&payload)?;
        let command_id = command_id_from_bytes(&command_id)?;
        let command_hash = digest_from_bytes(&row.0)?;
        load_chain_receipt(
            tx,
            command_id,
            &command.original_command,
            &command.command,
            &command_hash,
            command.resolved_prompt_version_id,
        )?
        .ok_or_else(|| {
            PromptStoreError::Corruption("prompt chain receipt disappeared during rebuild".into())
        })?;
        last_command_id = command_id.as_bytes().to_vec();
    }
    Ok(())
}

fn validate_prompt_receipt_command(
    tx: &Transaction<'_>,
    command: &PromptCommand,
    receipt: &PromptMutationReceipt,
) -> Result<(), PromptStoreError> {
    let version = load_version(tx, receipt.prompt_version_id)?.ok_or_else(|| {
        PromptStoreError::Corruption("prompt receipt references a missing version".into())
    })?;
    if version.prompt_id != receipt.prompt_id {
        return Err(PromptStoreError::Corruption(
            "prompt receipt version ownership mismatch".into(),
        ));
    }
    match command {
        PromptCommand::CreatePrompt(command) => {
            if receipt.prompt_id != command.prompt_id
                || receipt.prompt_version_id != command.prompt_version_id
                || receipt.revision != 1
            {
                return Err(PromptStoreError::Corruption(
                    "prompt create receipt disagrees with its command".into(),
                ));
            }
        }
        PromptCommand::CreatePromptVersion(command) => {
            if receipt.prompt_id != command.prompt_id
                || receipt.prompt_version_id != command.prompt_version_id
                || receipt.revision != next_revision(command.expected_revision)?
            {
                return Err(PromptStoreError::Corruption(
                    "prompt version receipt disagrees with its command".into(),
                ));
            }
        }
        PromptCommand::RenamePrompt(command) => {
            validate_prompt_receipt_target_and_revision(
                receipt,
                command.prompt_id,
                command.expected_revision,
            )?;
        }
        PromptCommand::SetPromptTags(command) => {
            validate_prompt_receipt_target_and_revision(
                receipt,
                command.prompt_id,
                command.expected_revision,
            )?;
        }
        PromptCommand::ArchivePrompt(command) => {
            validate_prompt_receipt_target_and_revision(
                receipt,
                command.prompt_id,
                command.expected_revision,
            )?;
        }
        PromptCommand::RestorePrompt(command) => {
            validate_prompt_receipt_target_and_revision(
                receipt,
                command.prompt_id,
                command.expected_revision,
            )?;
        }
    }
    Ok(())
}

fn validate_prompt_receipt_target_and_revision(
    receipt: &PromptMutationReceipt,
    prompt_id: PromptId,
    expected_revision: u64,
) -> Result<(), PromptStoreError> {
    if receipt.prompt_id != prompt_id
        || (receipt.revision != expected_revision
            && receipt.revision != next_revision(expected_revision)?)
    {
        return Err(PromptStoreError::Corruption(
            "prompt receipt target or revision disagrees with its command".into(),
        ));
    }
    Ok(())
}

fn validate_chain_receipt_command(
    tx: &Transaction<'_>,
    command: &PromptChainCommand,
    receipt: &PromptChainMutationReceipt,
) -> Result<(), PromptStoreError> {
    if load_chain(tx, receipt.chain_id)?.is_none() {
        return Err(PromptStoreError::Corruption(
            "prompt chain receipt references a missing chain".into(),
        ));
    }
    let (chain_id, link_id, expected_revision) = match command {
        PromptChainCommand::CreatePromptChain(command) => (command.chain_id, None, None),
        PromptChainCommand::RenamePromptChain(command) => {
            (command.chain_id, None, Some(command.expected_revision))
        }
        PromptChainCommand::InsertPromptChainLink(command) => (
            command.chain_id,
            Some(command.link_id),
            Some(command.expected_revision),
        ),
        PromptChainCommand::MovePromptChainLink(command) => (
            command.chain_id,
            Some(command.link_id),
            Some(command.expected_revision),
        ),
        PromptChainCommand::RemovePromptChainLink(command) => (
            command.chain_id,
            Some(command.link_id),
            Some(command.expected_revision),
        ),
        PromptChainCommand::UpdatePromptChainLinkVersion(command) => (
            command.chain_id,
            Some(command.link_id),
            Some(command.expected_revision),
        ),
        PromptChainCommand::ArchivePromptChain(command) => {
            (command.chain_id, None, Some(command.expected_revision))
        }
        PromptChainCommand::RestorePromptChain(command) => {
            (command.chain_id, None, Some(command.expected_revision))
        }
    };
    if receipt.chain_id != chain_id || receipt.link_id != link_id {
        return Err(PromptStoreError::Corruption(
            "prompt chain receipt target disagrees with its command".into(),
        ));
    }
    match expected_revision {
        Some(expected_revision) => {
            if receipt.revision != expected_revision
                && receipt.revision != next_revision(expected_revision)?
            {
                return Err(PromptStoreError::Corruption(
                    "prompt chain receipt revision disagrees with its command".into(),
                ));
            }
        }
        None if receipt.revision != 1 => {
            return Err(PromptStoreError::Corruption(
                "prompt chain creation receipt revision is invalid".into(),
            ));
        }
        None => {}
    }
    Ok(())
}

fn load_prompt(
    conn: &Transaction<'_>,
    prompt_id: PromptId,
) -> Result<Option<SavedPrompt>, PromptStoreError> {
    let row: Option<(String, Option<String>, Vec<u8>, i64, Option<i64>)> = conn
        .query_row(
            "SELECT title, description, current_version_id, revision, archived_at_ms
             FROM saved_prompts WHERE prompt_id = ?1",
            [prompt_id.as_bytes().as_slice()],
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
        .optional()?;
    let Some((title, description, current_version_id, revision, archived_at_ms)) = row else {
        return Ok(None);
    };
    let id = prompt_id.as_bytes().to_vec();
    let prompt = SavedPrompt {
        id: prompt_id,
        title,
        description,
        tags: load_tags(conn, &id)?,
        current_version_id: version_id_from_bytes(&current_version_id)?,
        revision: from_i64("saved_prompts.revision", revision)?,
        archived_at_ms,
    };
    Ok(Some(prompt))
}

fn validate_saved_prompt_record(
    conn: &Transaction<'_>,
    prompt: &SavedPrompt,
) -> Result<(), PromptStoreError> {
    if prompt.revision == 0 {
        return Err(PromptStoreError::Corruption(
            "saved prompt revision must be positive".into(),
        ));
    }
    if prompt.title != trim_prompt_whitespace(&prompt.title)
        || prompt
            .description
            .as_deref()
            .is_some_and(|description| description != trim_prompt_whitespace(description))
    {
        return Err(PromptStoreError::Corruption(
            "saved prompt metadata is not canonical".into(),
        ));
    }
    let normalized_tags = normalized_tags(&prompt.tags)
        .map_err(|_| PromptStoreError::Corruption("saved prompt tags are invalid".into()))?;
    if normalized_tags != prompt.tags {
        return Err(PromptStoreError::Corruption(
            "saved prompt tags are not canonical".into(),
        ));
    }
    validate_sql_tags(&prompt.tags)?;
    CreatePrompt {
        prompt_id: prompt.id,
        prompt_version_id: prompt.current_version_id,
        title: prompt.title.clone(),
        description: prompt.description.clone(),
        tags: prompt.tags.clone(),
        variables: Vec::new(),
        body: String::new(),
        created_at_ms: 0,
    }
    .validate()
    .map_err(|_| PromptStoreError::Corruption("saved prompt metadata is invalid".into()))?;
    let current_version = load_version(conn, prompt.current_version_id)?.ok_or_else(|| {
        PromptStoreError::Corruption("saved prompt current version is missing".into())
    })?;
    if current_version.prompt_id != prompt.id {
        return Err(PromptStoreError::Corruption(
            "saved prompt current version ownership mismatch".into(),
        ));
    }
    let latest_version: Option<(Vec<u8>, i64)> = conn
        .query_row(
            "SELECT prompt_version_id, version
             FROM prompt_versions
             WHERE prompt_id = ?1
             ORDER BY version DESC
             LIMIT 1",
            [prompt.id.as_bytes().as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((latest_version_id, latest_version_number)) = latest_version else {
        return Err(PromptStoreError::Corruption(
            "saved prompt has no prompt versions".into(),
        ));
    };
    if version_id_from_bytes(&latest_version_id)? != prompt.current_version_id
        || u32::try_from(latest_version_number).map_err(|_| {
            PromptStoreError::Corruption("latest prompt version is out of range".into())
        })? != current_version.version
    {
        return Err(PromptStoreError::Corruption(
            "saved prompt current version is not latest".into(),
        ));
    }
    Ok(())
}

fn validate_version_record(version: &PromptVersion) -> Result<(), PromptStoreError> {
    if version.version == 0 {
        return Err(PromptStoreError::Corruption(
            "prompt version number must be positive".into(),
        ));
    }
    let normalized = normalized_variables(&version.variables)
        .map_err(|_| PromptStoreError::Corruption("prompt variables are not normalized".into()))?;
    if normalized != version.variables {
        return Err(PromptStoreError::Corruption(
            "prompt variables are not normalized".into(),
        ));
    }
    if version.body.len() > MAX_PROMPT_BODY_BYTES {
        return Err(PromptStoreError::Corruption(
            "prompt version body exceeds its bound".into(),
        ));
    }
    if body_hash(&version.body) != version.body_sha256 {
        return Err(PromptStoreError::Corruption(
            "prompt body hash mismatch".into(),
        ));
    }
    Ok(())
}

fn load_chain(
    conn: &Transaction<'_>,
    chain_id: PromptChainId,
) -> Result<Option<PromptChain>, PromptStoreError> {
    let row: Option<(String, Option<String>, i64, Option<i64>)> = conn
        .query_row(
            "SELECT title, description, revision, archived_at_ms
             FROM prompt_chains WHERE chain_id = ?1",
            [chain_id.as_bytes().as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;
    let Some((title, description, revision, archived_at_ms)) = row else {
        return Ok(None);
    };
    let chain = PromptChain {
        id: chain_id,
        title,
        description,
        revision: from_i64("prompt_chains.revision", revision)?,
        archived_at_ms,
    };
    if chain.revision == 0 {
        return Err(PromptStoreError::Corruption(
            "prompt chain revision must be positive".into(),
        ));
    }
    CreatePromptChain {
        chain_id: chain.id,
        title: chain.title.clone(),
        description: chain.description.clone(),
        created_at_ms: 0,
    }
    .validate()
    .map_err(|_| PromptStoreError::Corruption("prompt chain metadata is invalid".into()))?;
    if chain.title != trim_prompt_whitespace(&chain.title)
        || chain
            .description
            .as_deref()
            .is_some_and(|description| description != trim_prompt_whitespace(description))
    {
        return Err(PromptStoreError::Corruption(
            "prompt chain metadata is not canonical".into(),
        ));
    }
    Ok(Some(chain))
}

fn load_chain_links(
    conn: &Transaction<'_>,
    chain_id: PromptChainId,
) -> Result<Vec<PromptChainLink>, PromptStoreError> {
    let sql = format!(
        "SELECT link_id, position, prompt_id, prompt_version_id
         FROM prompt_chain_links WHERE chain_id = ?1 ORDER BY position ASC LIMIT {}",
        MAX_PROMPT_CHAIN_LINKS + 1
    );
    let mut statement = conn.prepare(&sql)?;
    let rows = statement.query_map([chain_id.as_bytes().as_slice()], |row| {
        Ok((
            row.get::<_, Vec<u8>>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, Vec<u8>>(2)?,
            row.get::<_, Vec<u8>>(3)?,
        ))
    })?;
    let mut links = Vec::with_capacity(MAX_PROMPT_CHAIN_LINKS);
    let mut link_ids = HashSet::with_capacity(MAX_PROMPT_CHAIN_LINKS);
    for row in rows {
        if links.len() >= MAX_PROMPT_CHAIN_LINKS {
            return Err(PromptStoreError::Corruption(format!(
                "prompt chain exceeds maximum of {MAX_PROMPT_CHAIN_LINKS} links"
            )));
        }
        let (link_id, position, prompt_id, prompt_version_id) = row?;
        let expected_position = u32::try_from(links.len())
            .map_err(|_| PromptStoreError::Corruption("prompt chain is too long".into()))?;
        let position = u32::try_from(position)
            .map_err(|_| PromptStoreError::Corruption("prompt chain position is invalid".into()))?;
        if position != expected_position {
            return Err(PromptStoreError::Corruption(
                "prompt chain links are not dense".into(),
            ));
        }
        let link_id = prompt_chain_link_id_from_bytes(&link_id)?;
        if !link_ids.insert(link_id) {
            return Err(PromptStoreError::Corruption(
                "prompt chain links must be a dense ordered prefix".into(),
            ));
        }
        let prompt_id = prompt_id_from_bytes(&prompt_id)?;
        let prompt_version_id = version_id_from_bytes(&prompt_version_id)?;
        let version = load_version(conn, prompt_version_id)?.ok_or_else(|| {
            PromptStoreError::Corruption("prompt chain link references missing version".into())
        })?;
        if version.prompt_id != prompt_id {
            return Err(PromptStoreError::Corruption(
                "prompt chain link version ownership mismatch".into(),
            ));
        }
        load_prompt(conn, prompt_id)?.ok_or_else(|| {
            PromptStoreError::Corruption("prompt chain link references missing prompt".into())
        })?;
        links.push(PromptChainLink::store_issued(
            link_id,
            chain_id,
            position,
            prompt_id,
            prompt_version_id,
        ));
    }
    Ok(links)
}

fn load_version(
    conn: &Transaction<'_>,
    version_id: PromptVersionId,
) -> Result<Option<PromptVersion>, PromptStoreError> {
    let row: Option<(Vec<u8>, i64, String, Vec<u8>, i64, i64)> = conn
        .query_row(
            "SELECT prompt_id, version, body, body_sha256, created_at_ms, variables_sealed
             FROM prompt_versions WHERE prompt_version_id = ?1",
            [version_id.as_bytes().as_slice()],
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
        .optional()?;
    let Some((prompt_id, version, body, body_sha256, created_at_ms, variables_sealed)) = row else {
        return Ok(None);
    };
    if variables_sealed != 1 {
        return Err(PromptStoreError::Corruption(
            "prompt version variables are not sealed".into(),
        ));
    }
    let version = PromptVersion {
        id: version_id,
        prompt_id: prompt_id_from_bytes(&prompt_id)?,
        version: u32::try_from(version)
            .map_err(|_| PromptStoreError::Corruption("prompt version out of range".into()))?,
        body,
        variables: load_version_variables(conn, version_id.as_bytes())?,
        body_sha256: digest_from_bytes(&body_sha256)?,
        created_at_ms,
    };
    validate_version_record(&version)?;
    Ok(Some(version))
}

fn load_tags(conn: &Transaction<'_>, prompt_id: &[u8]) -> Result<Vec<String>, PromptStoreError> {
    let sql = format!(
        "SELECT tag, position FROM prompt_tags
             WHERE prompt_id = ?1 ORDER BY position ASC LIMIT {}",
        MAX_PROMPT_TAGS + 1
    );
    let mut statement = conn.prepare(&sql)?;
    let rows = statement.query_map([prompt_id], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })?;
    let mut values = Vec::with_capacity(MAX_PROMPT_TAGS);
    for (expected, row) in rows.enumerate() {
        if expected >= MAX_PROMPT_TAGS {
            return Err(PromptStoreError::Corruption(format!(
                "prompt tags exceed maximum of {MAX_PROMPT_TAGS}"
            )));
        }
        let (tag, position) = row?;
        let position = usize::try_from(position)
            .map_err(|_| PromptStoreError::Corruption("prompt tag position is invalid".into()))?;
        if position != expected {
            return Err(PromptStoreError::Corruption(
                "prompt tags are not a dense ordered prefix".into(),
            ));
        }
        values.push(tag);
    }
    let normalized = normalized_tags(&values)
        .map_err(|_| PromptStoreError::Corruption("prompt tags are not normalized".into()))?;
    if normalized != values {
        return Err(PromptStoreError::Corruption(
            "prompt tags are not normalized".into(),
        ));
    }
    validate_sql_tags(&values)?;
    Ok(values)
}

fn load_version_variables(
    conn: &Transaction<'_>,
    version_id: &[u8],
) -> Result<Vec<String>, PromptStoreError> {
    let sql = format!(
        "SELECT variable, position FROM prompt_version_variables
         WHERE prompt_version_id = ?1 ORDER BY position ASC LIMIT {}",
        MAX_PROMPT_VARIABLES + 1
    );
    let mut statement = conn.prepare(&sql)?;
    let rows = statement.query_map([version_id], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })?;
    let mut variables = Vec::with_capacity(MAX_PROMPT_VARIABLES);
    for (expected, row) in rows.enumerate() {
        if expected >= MAX_PROMPT_VARIABLES {
            return Err(PromptStoreError::Corruption(format!(
                "prompt variables exceed maximum of {MAX_PROMPT_VARIABLES}"
            )));
        }
        let (variable, position) = row?;
        let position = usize::try_from(position).map_err(|_| {
            PromptStoreError::Corruption("prompt variable position is invalid".into())
        })?;
        if position != expected {
            return Err(PromptStoreError::Corruption(
                "prompt variables are not a dense ordered prefix".into(),
            ));
        }
        variables.push(variable);
    }
    let normalized = normalized_variables(&variables)
        .map_err(|_| PromptStoreError::Corruption("prompt variables are not normalized".into()))?;
    if normalized != variables {
        return Err(PromptStoreError::Corruption(
            "prompt variables are not normalized".into(),
        ));
    }
    Ok(variables)
}

fn prompt_exists(conn: &Transaction<'_>, prompt_id: PromptId) -> Result<bool, PromptStoreError> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM saved_prompts WHERE prompt_id = ?1)",
        [prompt_id.as_bytes().as_slice()],
        |row| row.get(0),
    )?)
}

fn next_version_number(
    conn: &Transaction<'_>,
    prompt_id: PromptId,
) -> Result<u32, PromptStoreError> {
    let value: i64 = conn.query_row(
        "SELECT COALESCE(MAX(version), 0) + 1 FROM prompt_versions WHERE prompt_id = ?1",
        [prompt_id.as_bytes().as_slice()],
        |row| row.get(0),
    )?;
    u32::try_from(value)
        .map_err(|_| PromptStoreError::Corruption("prompt version number overflow".into()))
}

fn check_revision(prompt: &SavedPrompt, expected: u64) -> Result<(), PromptStoreError> {
    if prompt.revision == expected {
        Ok(())
    } else {
        Err(PromptStoreError::RevisionConflict {
            expected,
            actual: prompt.revision,
        })
    }
}

fn next_revision(revision: u64) -> Result<u64, PromptStoreError> {
    revision
        .checked_add(1)
        .ok_or_else(|| PromptStoreError::Corruption("prompt revision overflow".into()))
}

fn validate_page(limit: usize) -> Result<(), PromptStoreError> {
    if limit == 0 || limit > MAX_PROMPT_PAGE_SIZE {
        return Err(PromptStoreError::Database(format!(
            "prompt page limit must be between 1 and {MAX_PROMPT_PAGE_SIZE}"
        )));
    }
    Ok(())
}

fn sha256_bytes(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn body_hash(body: &str) -> [u8; 32] {
    sha256_bytes(body.as_bytes())
}

fn decode_prompt_chain_event(payload: &[u8]) -> Result<PromptChainEvent, PromptStoreError> {
    validate_wire_size("prompt chain event", payload)?;
    let event = PromptChainEvent::decode(payload)
        .map_err(|error| PromptStoreError::Corruption(error.to_string()))?;
    validate_prompt_chain_event_structure(&event)?;
    Ok(event)
}

fn validate_prompt_chain_event_structure(event: &PromptChainEvent) -> Result<(), PromptStoreError> {
    let invalid = || PromptStoreError::Corruption("prompt chain event payload is invalid".into());
    match event {
        PromptChainEvent::PromptChainCreated { chain } => {
            if chain.revision != 1
                || chain.archived_at_ms.is_some()
                || trim_prompt_whitespace(&chain.title).is_empty()
                || chain.title != trim_prompt_whitespace(&chain.title)
                || chain.title.chars().count() > MAX_PROMPT_CHAIN_TITLE_SCALARS
                || chain.description.as_deref().is_some_and(|description| {
                    description != trim_prompt_whitespace(description)
                        || description.chars().count() > MAX_PROMPT_CHAIN_DESCRIPTION_SCALARS
                })
            {
                return Err(invalid());
            }
        }
        PromptChainEvent::PromptChainRenamed {
            title, revision, ..
        } => {
            if *revision == 0
                || trim_prompt_whitespace(title).is_empty()
                || title != trim_prompt_whitespace(title)
                || title.chars().count() > MAX_PROMPT_CHAIN_TITLE_SCALARS
            {
                return Err(invalid());
            }
        }
        PromptChainEvent::PromptChainLinksReplaced {
            chain_id,
            links,
            revision,
        } => {
            if *revision == 0 || links.len() > MAX_PROMPT_CHAIN_LINKS {
                return Err(invalid());
            }
            let mut link_ids = HashSet::with_capacity(links.len());
            for (position, link) in links.iter().enumerate() {
                let position = u32::try_from(position).map_err(|_| invalid())?;
                if link.chain_id() != *chain_id
                    || link.position() != position
                    || !link_ids.insert(link.id())
                {
                    return Err(invalid());
                }
            }
        }
        PromptChainEvent::PromptChainArchived { revision, .. }
        | PromptChainEvent::PromptChainRestored { revision, .. } => {
            if *revision == 0 {
                return Err(invalid());
            }
        }
    }
    Ok(())
}

fn encode_chain_command(
    original_command: &PromptChainCommand,
    command: &PromptChainCommand,
    resolved_prompt_version_id: Option<PromptVersionId>,
) -> Result<Vec<u8>, PromptStoreError> {
    let payload = command
        .encode_durable_with_original(original_command, resolved_prompt_version_id)
        .map_err(|error| PromptStoreError::Corruption(error.to_string()))?;
    validate_wire_size("prompt chain command", &payload)?;
    Ok(payload)
}

fn decode_chain_command(payload: &[u8]) -> Result<DecodedPromptChainCommand, PromptStoreError> {
    validate_wire_size("prompt chain command", payload)?;
    let (original_command, original_command_sha256, command, resolved_prompt_version_id) =
        PromptChainCommand::decode_durable(payload)
            .map_err(|error| PromptStoreError::Corruption(error.to_string()))?;
    Ok(DecodedPromptChainCommand {
        original_command,
        original_command_sha256,
        command,
        resolved_prompt_version_id,
    })
}

fn digest_from_bytes(bytes: &[u8]) -> Result<[u8; 32], PromptStoreError> {
    bytes
        .try_into()
        .map_err(|_| PromptStoreError::Corruption("prompt body digest must be 32 bytes".into()))
}

fn command_id_from_bytes(bytes: &[u8]) -> Result<CommandId, PromptStoreError> {
    let bytes: [u8; 16] = bytes
        .try_into()
        .map_err(|_| PromptStoreError::Corruption("command id must be 16 bytes".into()))?;
    CommandId::from_bytes(bytes).map_err(|error| PromptStoreError::Corruption(error.to_string()))
}

fn event_id_from_bytes(bytes: &[u8]) -> Result<EventId, PromptStoreError> {
    let bytes: [u8; 16] = bytes
        .try_into()
        .map_err(|_| PromptStoreError::Corruption("prompt event id must be 16 bytes".into()))?;
    EventId::from_bytes(bytes).map_err(|error| PromptStoreError::Corruption(error.to_string()))
}

fn prompt_id_from_bytes(bytes: &[u8]) -> Result<PromptId, PromptStoreError> {
    let bytes: [u8; 16] = bytes
        .try_into()
        .map_err(|_| PromptStoreError::Corruption("prompt id must be 16 bytes".into()))?;
    PromptId::from_bytes(bytes).map_err(|error| PromptStoreError::Corruption(error.to_string()))
}

fn version_id_from_bytes(bytes: &[u8]) -> Result<PromptVersionId, PromptStoreError> {
    let bytes: [u8; 16] = bytes
        .try_into()
        .map_err(|_| PromptStoreError::Corruption("prompt version id must be 16 bytes".into()))?;
    PromptVersionId::from_bytes(bytes)
        .map_err(|error| PromptStoreError::Corruption(error.to_string()))
}

fn prompt_chain_id_from_bytes(bytes: &[u8]) -> Result<PromptChainId, PromptStoreError> {
    let bytes: [u8; 16] = bytes
        .try_into()
        .map_err(|_| PromptStoreError::Corruption("prompt chain id must be 16 bytes".into()))?;
    PromptChainId::from_bytes(bytes)
        .map_err(|error| PromptStoreError::Corruption(error.to_string()))
}

fn prompt_chain_link_id_from_bytes(bytes: &[u8]) -> Result<PromptChainLinkId, PromptStoreError> {
    let bytes: [u8; 16] = bytes.try_into().map_err(|_| {
        PromptStoreError::Corruption("prompt chain link id must be 16 bytes".into())
    })?;
    PromptChainLinkId::from_bytes(bytes)
        .map_err(|error| PromptStoreError::Corruption(error.to_string()))
}

fn check_chain_revision(chain: &PromptChain, expected: u64) -> Result<(), PromptStoreError> {
    if chain.revision == expected {
        Ok(())
    } else {
        Err(PromptStoreError::RevisionConflict {
            expected,
            actual: chain.revision,
        })
    }
}

fn to_i64<T>(value: T) -> Result<i64, PromptStoreError>
where
    T: TryInto<i64>,
{
    value
        .try_into()
        .map_err(|_| PromptStoreError::Corruption("prompt integer out of range".into()))
}

fn from_i64(field: &'static str, value: i64) -> Result<u64, PromptStoreError> {
    u64::try_from(value).map_err(|_| PromptStoreError::Corruption(format!("{field} is negative")))
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod coherent_read_tests {
    use super::*;
    use std::ffi::c_void;
    use std::ptr;
    use std::sync::{atomic::AtomicBool, atomic::Ordering, Arc, Barrier};
    use std::thread;
    use std::time::Duration;

    use rusqlite::ffi;
    use tempfile::TempDir;

    struct TraceGate {
        writer_go: Barrier,
        writer_done: Barrier,
        fired: AtomicBool,
    }

    unsafe extern "C" fn release_writer_after_first_row(
        trace_code: std::os::raw::c_uint,
        context: *mut c_void,
        _statement: *mut c_void,
        _x: *mut c_void,
    ) -> std::os::raw::c_int {
        if trace_code == ffi::SQLITE_TRACE_ROW && !context.is_null() {
            // SAFETY: the test keeps one Arc strong reference alive until the
            // trace is unregistered, and SQLite invokes this callback only on
            // the connection whose handle received that context pointer.
            let gate = unsafe { &*(context.cast::<TraceGate>()) };
            if !gate.fired.swap(true, Ordering::AcqRel) {
                gate.writer_go.wait();
                gate.writer_done.wait();
            }
        }
        ffi::SQLITE_OK
    }

    fn read_during_writer_commit<T>(
        read: impl FnOnce(
            &PromptStore,
            PromptId,
            PromptVersionId,
            PromptChainId,
            PromptChainLinkId,
        ) -> Result<T, PromptStoreError>,
    ) -> T {
        let directory = TempDir::new().expect("create isolated prompt database directory");
        let path = directory.path().join("prompts.sqlite3");
        let prompt_id = PromptId::new();
        let first_version_id = PromptVersionId::new();
        let second_version_id = PromptVersionId::new();
        let chain_id = PromptChainId::new();
        let chain_link_id = PromptChainLinkId::new();
        let mut store = PromptStore::open(&path).expect("open isolated prompt store");
        store
            .execute(
                CommandId::new(),
                PromptCommand::CreatePrompt(CreatePrompt {
                    prompt_id,
                    prompt_version_id: first_version_id,
                    title: "Concurrent prompt".into(),
                    description: None,
                    tags: vec!["old".into()],
                    variables: Vec::new(),
                    body: "first version".into(),
                    created_at_ms: 1,
                }),
            )
            .expect("create initial prompt");
        store
            .execute_chain(
                CommandId::new(),
                PromptChainCommand::CreatePromptChain(CreatePromptChain {
                    chain_id,
                    title: "Concurrent chain".into(),
                    description: None,
                    created_at_ms: 1,
                }),
            )
            .expect("create initial chain");
        store
            .execute_chain(
                CommandId::new(),
                PromptChainCommand::InsertPromptChainLink(InsertPromptChainLink {
                    chain_id,
                    link_id: chain_link_id,
                    prompt_id,
                    prompt_version_id: None,
                    before_link_id: None,
                    expected_revision: 1,
                }),
            )
            .expect("insert initial chain link");

        let gate = Arc::new(TraceGate {
            writer_go: Barrier::new(2),
            writer_done: Barrier::new(2),
            fired: AtomicBool::new(false),
        });
        let writer_ready = Arc::new(Barrier::new(2));
        let writer_gate = Arc::clone(&gate);
        let writer_ready_for_thread = Arc::clone(&writer_ready);
        let writer_path = path.clone();
        let writer = thread::spawn(move || {
            let writer = Connection::open(writer_path).expect("open writer connection");
            writer
                .busy_timeout(Duration::from_secs(5))
                .expect("set writer busy timeout");
            writer
                .execute_batch("BEGIN IMMEDIATE")
                .expect("begin writer transaction");
            let body = "second version";
            let body_sha256 = body_hash(body);
            writer
                .execute(
                    "INSERT INTO prompt_versions(
                        prompt_version_id, prompt_id, version, body, body_sha256, created_at_ms
                     ) VALUES (?1, ?2, 2, ?3, ?4, 2)",
                    rusqlite::params![
                        second_version_id.as_bytes().as_slice(),
                        prompt_id.as_bytes().as_slice(),
                        body,
                        body_sha256.as_slice(),
                    ],
                )
                .expect("insert writer version");
            writer
                .execute(
                    "DELETE FROM prompt_tags WHERE prompt_id = ?1",
                    [prompt_id.as_bytes().as_slice()],
                )
                .expect("delete old prompt tags");
            writer
                .execute(
                    "INSERT INTO prompt_tags(prompt_id, tag, position)
                     VALUES (?1, 'new', 0)",
                    [prompt_id.as_bytes().as_slice()],
                )
                .expect("insert new prompt tag");
            writer_ready_for_thread.wait();
            writer_gate.writer_go.wait();
            writer
                .execute_batch("COMMIT")
                .expect("commit writer transaction");
            writer_gate.writer_done.wait();
        });

        writer_ready.wait();
        let context = Arc::into_raw(Arc::clone(&gate)) as *mut c_void;
        // SAFETY: PromptStore exclusively owns this connection, and the gate
        // remains alive until the callback is unregistered below.
        let handle = unsafe { store.conn.handle() };
        let trace_result = unsafe {
            ffi::sqlite3_trace_v2(
                handle,
                ffi::SQLITE_TRACE_ROW,
                Some(release_writer_after_first_row),
                context,
            )
        };
        assert_eq!(trace_result, ffi::SQLITE_OK);

        let result = read(&store, prompt_id, first_version_id, chain_id, chain_link_id);

        // SAFETY: no statement is executing after the read returned, and
        // unregistering releases SQLite's borrowed callback context.
        unsafe {
            ffi::sqlite3_trace_v2(handle, 0, None, ptr::null_mut());
            drop(Arc::from_raw(context.cast::<TraceGate>()));
        }
        writer.join().expect("join writer thread");

        result.expect("read across concurrent commit")
    }

    #[test]
    fn multi_query_reads_do_not_mix_rows_across_a_writer_commit() {
        let (prompt, first_version_id) =
            read_during_writer_commit(|store, prompt_id, first_version_id, _, _| {
                store
                    .get_prompt(prompt_id)
                    .map(|prompt| (prompt.expect("prompt exists"), first_version_id))
            });
        assert_eq!(prompt.current_version_id, first_version_id);
        assert_eq!(prompt.tags, vec!["old"]);

        let (prompts, first_version_id) =
            read_during_writer_commit(|store, _, first_version_id, _, _| {
                store
                    .list_prompts(0, 10)
                    .map(|prompts| (prompts, first_version_id))
            });
        assert_eq!(prompts.len(), 1);
        assert_eq!(prompts[0].current_version_id, first_version_id);
        assert_eq!(prompts[0].tags, vec!["old"]);

        let (snapshot, first_version_id) =
            read_during_writer_commit(|store, _, first_version_id, _, _| {
                store
                    .snapshot(0, 10)
                    .map(|snapshot| (snapshot, first_version_id))
            });
        assert_eq!(snapshot.prompts.len(), 1);
        assert_eq!(snapshot.prompts[0].current_version_id, first_version_id);
        assert_eq!(snapshot.prompts[0].tags, vec!["old"]);
        assert_eq!(snapshot.next_offset, None);

        let (versions, first_version_id) =
            read_during_writer_commit(|store, prompt_id, first_version_id, _, _| {
                store
                    .list_versions(prompt_id, 0, 10)
                    .map(|versions| (versions, first_version_id))
            });
        assert_eq!(versions.len(), 1);
        assert_eq!(versions[0].id, first_version_id);
        assert_eq!(versions[0].body, "first version");

        let links = read_during_writer_commit(|store, _, first_version_id, chain_id, _| {
            store
                .list_chain_links(chain_id)
                .map(|links| (links, first_version_id))
        });
        assert_eq!(links.0.len(), 1);
        assert_eq!(links.0[0].prompt_version_id(), links.1);

        let (context, first_version_id) =
            read_during_writer_commit(|store, _, first_version_id, chain_id, chain_link_id| {
                store
                    .get_chain_link_context(chain_id, chain_link_id)
                    .map(|context| (context, first_version_id))
            });
        let context = context.expect("chain link context exists");
        assert_eq!(context.link.prompt_version_id(), first_version_id);
        assert!(!context.update_available);
    }
}
