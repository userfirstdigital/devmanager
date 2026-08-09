use std::fmt;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::domain::id::{
    CommandId, EventId, PromptChainId, PromptChainLinkId, PromptId, PromptVersionId,
};

use super::model::{
    normalized_tags, normalized_variables, ArchivePrompt, ArchivePromptChain, CreatePrompt,
    CreatePromptChain, CreatePromptVersion, InsertPromptChainLink, MovePromptChainLink,
    PromptChain, PromptChainCommand, PromptChainEvent, PromptChainLink, PromptChainLinkContext,
    PromptChainMutationReceipt, PromptCommand, PromptEvent, PromptMutationReceipt,
    PromptProjectionRebuild, PromptSnapshot, PromptValidationError, PromptVersion,
    RemovePromptChainLink, RenamePrompt, RenamePromptChain, RestorePrompt, RestorePromptChain,
    SavedPrompt, SetPromptTags, UpdatePromptChainLinkVersion, MAX_PROMPT_BODY_BYTES,
    MAX_PROMPT_CHAIN_DESCRIPTION_SCALARS, MAX_PROMPT_CHAIN_TITLE_SCALARS, MAX_PROMPT_PAGE_SIZE,
};

const BUSY_TIMEOUT_MS: u64 = 5_000;
const PROMPT_WIRE_SCHEMA_VERSION: u32 = 1;

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct PromptChainCommandWire<'a> {
    schema_version: u32,
    command: &'a PromptChainCommand,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PromptChainCommandWireOwned {
    schema_version: u32,
    command: PromptChainCommand,
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

#[derive(Debug)]
pub struct PromptStore {
    conn: Connection,
}

impl PromptStore {
    /// Open an isolated prompt view over a kernel SQLite database.
    ///
    /// Opening through `KernelStore` applies the compiled, ordered migration
    /// manifest. This store owns only the prompt command/event transaction
    /// surface; the task CommandBus remains a later integration seam.
    pub fn open(path: &Path) -> Result<Self, PromptStoreError> {
        crate::kernel::KernelStore::open(path)
            .map_err(|error| PromptStoreError::Database(error.to_string()))?;
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_millis(BUSY_TIMEOUT_MS))?;
        let _: String = conn.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))?;
        conn.execute_batch("PRAGMA foreign_keys=ON; PRAGMA synchronous=NORMAL;")?;
        Ok(Self { conn })
    }

    pub fn execute(
        &mut self,
        command_id: CommandId,
        command: PromptCommand,
    ) -> Result<PromptMutationReceipt, PromptStoreError> {
        let command = canonical_prompt_command(command)?;
        command.validate()?;
        let command_payload = command
            .encode()
            .map_err(|error| PromptStoreError::Corruption(error.to_string()))?;
        let command_sha256 = sha256_bytes(&command_payload);
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        validate_prompt_command_state(&tx, &command)?;

        if let Some(receipt) = load_receipt(&tx, command_id, &command, &command_sha256)? {
            tx.commit()?;
            return Ok(receipt);
        }

        let (receipt, event, prompt_id, occurred_at_ms) = apply_command(&tx, command_id, &command)?;
        let receipt_payload = receipt
            .encode()
            .map_err(|error| PromptStoreError::Corruption(error.to_string()))?;
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
        tx.commit()?;
        Ok(receipt)
    }

    pub fn execute_chain(
        &mut self,
        command_id: CommandId,
        command: PromptChainCommand,
    ) -> Result<PromptChainMutationReceipt, PromptStoreError> {
        let command = canonical_prompt_chain_command(command)?;
        command.validate()?;
        let command_payload = encode_chain_command(&command)?;
        let command_sha256 = sha256_bytes(&command_payload);
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;

        if let Some(receipt) = load_chain_receipt(&tx, command_id, &command, &command_sha256)? {
            tx.commit()?;
            return Ok(receipt);
        }

        let (receipt, event, chain_id, occurred_at_ms) =
            apply_chain_command(&tx, command_id, &command)?;
        let receipt_payload = receipt
            .encode()
            .map_err(|error| PromptStoreError::Corruption(error.to_string()))?;
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
        tx.commit()?;
        Ok(receipt)
    }

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
        load_chain(&self.conn, chain_id)
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
        if load_chain(&self.conn, chain_id)?.is_none() {
            return Err(PromptStoreError::NotFound);
        }
        let links = load_chain_links(&self.conn, chain_id)?;
        validate_chain_links(&self.conn, chain_id, &links)?;
        Ok(links)
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
        let links = self.list_chain_links(chain_id)?;
        let Some(index) = links.iter().position(|link| link.id == link_id) else {
            return Ok(None);
        };
        let link = links[index].clone();
        let current_version = load_prompt(&self.conn, link.prompt_id)?
            .ok_or_else(|| {
                PromptStoreError::Corruption("chain link references missing prompt".into())
            })?
            .current_version_id;
        Ok(Some(PromptChainLinkContext {
            link,
            previous_link_id: index.checked_sub(1).map(|i| links[i].id),
            next_link_id: links.get(index + 1).map(|link| link.id),
            update_available: current_version != links[index].prompt_version_id,
        }))
    }

    pub fn count_chain_events(&self) -> Result<u64, PromptStoreError> {
        let count: i64 =
            self.conn
                .query_row("SELECT COUNT(*) FROM prompt_chain_events", [], |row| {
                    row.get(0)
                })?;
        u64::try_from(count)
            .map_err(|_| PromptStoreError::Corruption("negative prompt chain event count".into()))
    }

    pub fn get_prompt(&self, prompt_id: PromptId) -> Result<Option<SavedPrompt>, PromptStoreError> {
        let prompt = load_prompt(&self.conn, prompt_id)?;
        if let Some(prompt) = &prompt {
            validate_saved_prompt_record(&self.conn, prompt)?;
        }
        Ok(prompt)
    }

    pub fn get_version(
        &self,
        version_id: PromptVersionId,
    ) -> Result<Option<PromptVersion>, PromptStoreError> {
        load_version(&self.conn, version_id)
    }

    pub fn list_prompts(
        &self,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<SavedPrompt>, PromptStoreError> {
        validate_page(limit)?;
        let mut statement = self.conn.prepare(
            "SELECT prompt_id, title, description, current_version_id, revision,
                    archived_at_ms
             FROM saved_prompts
             ORDER BY CASE WHEN archived_at_ms IS NULL THEN 0 ELSE 1 END,
                      title COLLATE NOCASE ASC, prompt_id ASC
             LIMIT ?1 OFFSET ?2",
        )?;
        let rows =
            statement.query_map(rusqlite::params![to_i64(limit)?, to_i64(offset)?], |row| {
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
                tags: load_tags(&self.conn, &id)?,
                current_version_id: version_id_from_bytes(&current_version_id)?,
                revision: from_i64("saved_prompts.revision", revision)?,
                archived_at_ms,
            };
            validate_saved_prompt_record(&self.conn, &prompt)?;
            prompts.push(prompt);
        }
        Ok(prompts)
    }

    pub fn snapshot(
        &self,
        offset: usize,
        limit: usize,
    ) -> Result<PromptSnapshot, PromptStoreError> {
        let prompts = self.list_prompts(offset, limit)?;
        let next_offset = (prompts.len() == limit).then_some(offset + prompts.len());
        Ok(PromptSnapshot {
            prompts,
            next_offset,
        })
    }

    pub fn global_snapshot(
        &self,
        offset: usize,
        limit: usize,
    ) -> Result<PromptSnapshot, PromptStoreError> {
        self.snapshot(offset, limit)
    }

    pub fn list_versions(
        &self,
        prompt_id: PromptId,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<PromptVersion>, PromptStoreError> {
        validate_page(limit)?;
        let Some(prompt) = load_prompt(&self.conn, prompt_id)? else {
            return Err(PromptStoreError::NotFound);
        };
        validate_saved_prompt_record(&self.conn, &prompt)?;
        let mut statement = self.conn.prepare(
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
                variables: load_version_variables(&self.conn, &id)?,
                body_sha256: digest_from_bytes(&body_sha256)?,
                created_at_ms,
            };
            validate_version_record(&version)?;
            Ok(version)
        })
        .collect()
    }

    pub fn count_prompts(&self) -> Result<u64, PromptStoreError> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM saved_prompts", [], |row| row.get(0))?;
        u64::try_from(count)
            .map_err(|_| PromptStoreError::Corruption("negative prompt count".into()))
    }

    pub fn count_prompt_events(&self) -> Result<u64, PromptStoreError> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM prompt_events", [], |row| row.get(0))?;
        u64::try_from(count)
            .map_err(|_| PromptStoreError::Corruption("negative prompt event count".into()))
    }

    pub fn rebuild_projection(&mut self) -> Result<PromptProjectionRebuild, PromptStoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let prompt_rows: Vec<(Vec<u8>, Vec<u8>, Vec<u8>, String, i64, Vec<u8>)> = {
            let mut statement = tx.prepare(
                "SELECT prompt_event_id, command_id, prompt_id, event_type,
                        occurred_at_ms, payload
                 FROM prompt_events ORDER BY sequence ASC",
            )?;
            let rows = statement
                .query_map([], |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                })?
                .collect::<Result<_, _>>()?;
            rows
        };
        let events = prompt_rows
            .into_iter()
            .map(
                |(event_id, command_id, prompt_id, event_type, occurred_at_ms, payload)| {
                    validate_prompt_event_row(
                        &tx,
                        &event_id,
                        &command_id,
                        &prompt_id,
                        &event_type,
                        occurred_at_ms,
                        &payload,
                    )
                },
            )
            .collect::<Result<Vec<_>, _>>()?;

        let chain_rows: Vec<(Vec<u8>, Vec<u8>, Vec<u8>, String, i64, Vec<u8>)> = {
            let mut statement = tx.prepare(
                "SELECT prompt_chain_event_id, command_id, chain_id, event_type,
                        occurred_at_ms, payload
                 FROM prompt_chain_events ORDER BY sequence ASC",
            )?;
            let rows = statement
                .query_map([], |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                })?
                .collect::<Result<_, _>>()?;
            rows
        };
        let chain_events = chain_rows
            .into_iter()
            .map(
                |(event_id, command_id, chain_id, event_type, occurred_at_ms, payload)| {
                    validate_chain_event_row(
                        &tx,
                        &event_id,
                        &command_id,
                        &chain_id,
                        &event_type,
                        occurred_at_ms,
                        &payload,
                    )
                },
            )
            .collect::<Result<Vec<_>, _>>()?;
        tx.execute_batch(
            "DELETE FROM prompt_chain_links;
             DELETE FROM prompt_chains;
             DELETE FROM prompt_tags;
             DELETE FROM saved_prompts;",
        )?;
        for (event, occurred_at_ms, receipt_version_id) in &events {
            validate_prompt_event_temporal_lineage(&tx, event, *receipt_version_id)?;
            apply_event(&tx, event, *occurred_at_ms)?;
        }
        for (event, occurred_at_ms, command) in &chain_events {
            apply_chain_event(&tx, event, *occurred_at_ms, command)?;
        }
        let events_replayed = events
            .len()
            .checked_add(chain_events.len())
            .ok_or_else(|| PromptStoreError::Corruption("prompt event count overflow".into()))?;
        tx.commit()?;
        Ok(PromptProjectionRebuild {
            events_replayed: u64::try_from(events_replayed).map_err(|_| {
                PromptStoreError::Corruption("prompt event count is out of range".into())
            })?,
        })
    }
}

fn canonical_prompt_command(mut command: PromptCommand) -> Result<PromptCommand, PromptStoreError> {
    match &mut command {
        PromptCommand::CreatePrompt(command) => {
            command.title = command.title.trim().to_string();
            command.description = command
                .description
                .take()
                .map(|description| description.trim().to_string());
            command.tags = normalized_tags(&command.tags)?;
            command.variables = normalized_variables(&command.variables)?;
        }
        PromptCommand::CreatePromptVersion(command) => {
            command.variables = normalized_variables(&command.variables)?;
        }
        PromptCommand::RenamePrompt(command) => {
            command.title = command.title.trim().to_string();
        }
        PromptCommand::SetPromptTags(command) => {
            command.tags = normalized_tags(&command.tags)?;
        }
        PromptCommand::ArchivePrompt(_) | PromptCommand::RestorePrompt(_) => {}
    }
    command.validate()?;
    Ok(command)
}

fn canonical_prompt_chain_command(
    mut command: PromptChainCommand,
) -> Result<PromptChainCommand, PromptStoreError> {
    match &mut command {
        PromptChainCommand::CreatePromptChain(command) => {
            command.title = command.title.trim().to_string();
            command.description = command
                .description
                .take()
                .map(|description| description.trim().to_string());
        }
        PromptChainCommand::RenamePromptChain(command) => {
            command.title = command.title.trim().to_string();
        }
        PromptChainCommand::InsertPromptChainLink(_)
        | PromptChainCommand::MovePromptChainLink(_)
        | PromptChainCommand::RemovePromptChainLink(_)
        | PromptChainCommand::UpdatePromptChainLinkVersion(_)
        | PromptChainCommand::ArchivePromptChain(_)
        | PromptChainCommand::RestorePromptChain(_) => {}
    }
    command.validate()?;
    Ok(command)
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
        title: command.title.trim().to_string(),
        description: command
            .description
            .as_deref()
            .map(str::trim)
            .map(str::to_string),
        tags,
        current_version_id: version.id,
        revision: 1,
        archived_at_ms: None,
    };
    insert_saved_prompt(tx, &prompt, command.created_at_ms)?;
    insert_version(tx, &version)?;
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
    let title = command.title.trim();
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
        title: command.title.trim().to_string(),
        description: command
            .description
            .as_deref()
            .map(str::trim)
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
    let title = command.title.trim();
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
    if links.iter().any(|link| link.id == command.link_id) {
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
            .position(|link| link.id == before_link_id)
            .ok_or(PromptStoreError::NotFound)?,
        None => links.len(),
    };
    links.insert(
        position,
        PromptChainLink {
            id: command.link_id,
            chain_id: chain.id,
            position: 0,
            prompt_id: prompt.id,
            prompt_version_id,
        },
    );
    renumber_links(&mut links)?;
    let revision = next_revision(chain.revision)?;
    let occurred_at_ms = now_ms();
    write_chain_links(tx, &chain, &links, revision, occurred_at_ms)?;
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
    let old_order: Vec<PromptChainLinkId> = links.iter().map(|link| link.id).collect();
    let moving_index = links
        .iter()
        .position(|link| link.id == command.link_id)
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
        if !links.iter().any(|link| link.id == before_link_id) {
            return Err(PromptStoreError::NotFound);
        }
    }
    let moving = links.remove(moving_index);
    let position = command
        .before_link_id
        .and_then(|before_link_id| links.iter().position(|link| link.id == before_link_id))
        .unwrap_or(links.len());
    links.insert(position, moving);
    renumber_links(&mut links)?;
    let new_order: Vec<PromptChainLinkId> = links.iter().map(|link| link.id).collect();
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
    write_chain_links(tx, &chain, &links, revision, occurred_at_ms)?;
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
        .position(|link| link.id == command.link_id)
        .ok_or(PromptStoreError::NotFound)?;
    links.remove(position);
    renumber_links(&mut links)?;
    let revision = next_revision(chain.revision)?;
    let occurred_at_ms = now_ms();
    write_chain_links(tx, &chain, &links, revision, occurred_at_ms)?;
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
        .position(|link| link.id == command.link_id)
        .ok_or(PromptStoreError::NotFound)?;
    let prompt = load_prompt(tx, links[position].prompt_id)?.ok_or_else(|| {
        PromptStoreError::Corruption("chain link references missing prompt".into())
    })?;
    validate_saved_prompt_record(tx, &prompt)?;
    if links[position].prompt_version_id == prompt.current_version_id {
        return Ok((
            chain_receipt(command_id, &chain, Some(command.link_id)),
            None,
            chain.id,
            now_ms(),
        ));
    }
    links[position].prompt_version_id = prompt.current_version_id;
    let revision = next_revision(chain.revision)?;
    let occurred_at_ms = now_ms();
    write_chain_links(tx, &chain, &links, revision, occurred_at_ms)?;
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
    for (position, link) in links.iter_mut().enumerate() {
        link.position = u32::try_from(position)
            .map_err(|_| PromptStoreError::Corruption("prompt chain is too long".into()))?;
    }
    Ok(())
}

fn validate_chain_links(
    conn: &Connection,
    chain_id: PromptChainId,
    links: &[PromptChainLink],
) -> Result<(), PromptStoreError> {
    for (position, link) in links.iter().enumerate() {
        if link.chain_id != chain_id
            || link.position
                != u32::try_from(position)
                    .map_err(|_| PromptStoreError::Corruption("prompt chain is too long".into()))?
            || links[..position]
                .iter()
                .any(|previous| previous.id == link.id)
        {
            return Err(PromptStoreError::Corruption(
                "prompt chain links must be a dense ordered prefix".into(),
            ));
        }
        let version = load_version(conn, link.prompt_version_id)?.ok_or_else(|| {
            PromptStoreError::Corruption("prompt chain link references missing version".into())
        })?;
        let prompt = load_prompt(conn, link.prompt_id)?.ok_or_else(|| {
            PromptStoreError::Corruption("prompt chain link references missing prompt".into())
        })?;
        validate_saved_prompt_record(conn, &prompt)?;
        if version.prompt_id != link.prompt_id {
            return Err(PromptStoreError::Corruption(
                "prompt chain link version ownership mismatch".into(),
            ));
        }
    }
    Ok(())
}

fn write_chain_links(
    tx: &Transaction<'_>,
    chain: &PromptChain,
    links: &[PromptChainLink],
    revision: u64,
    updated_at_ms: i64,
) -> Result<(), PromptStoreError> {
    validate_chain_links(tx, chain.id, links)?;
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
    if changed != 1 {
        return Err(PromptStoreError::RevisionConflict {
            expected: chain.revision,
            actual: chain.revision,
        });
    }
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
                link.id.as_bytes().as_slice(),
                link.chain_id.as_bytes().as_slice(),
                i64::from(link.position),
                link.prompt_id.as_bytes().as_slice(),
                link.prompt_version_id.as_bytes().as_slice(),
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
                || prompt.title != prompt.title.trim()
                || prompt
                    .description
                    .as_deref()
                    .is_some_and(|description| description != description.trim())
                || normalized_tags(&prompt.tags).map_err(|_| {
                    PromptStoreError::Corruption("prompt.created tags are invalid".into())
                })? != prompt.tags
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
            insert_saved_prompt(tx, prompt, version.created_at_ms)?;
            ensure_version(tx, version)?;
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
            let title_is_valid = title.trim() == title
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
                .map(|normalized| normalized == *tags)
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
    event: &PromptChainEvent,
) -> Result<(), PromptStoreError> {
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
    let mut expected_links = current_links.clone();
    match command {
        PromptChainCommand::InsertPromptChainLink(command) => {
            if expected_links.iter().any(|link| link.id == command.link_id) {
                return Err(PromptStoreError::Corruption(
                    "prompt chain insert command link already exists".into(),
                ));
            }
            let prompt = load_prompt(tx, command.prompt_id)?.ok_or_else(|| {
                PromptStoreError::Corruption("prompt chain insert command prompt is missing".into())
            })?;
            validate_saved_prompt_record(tx, &prompt)?;
            let prompt_version_id = match command.prompt_version_id {
                Some(version_id) => {
                    let version = load_version(tx, version_id)?.ok_or_else(|| {
                        PromptStoreError::Corruption(
                            "prompt chain insert command version is missing".into(),
                        )
                    })?;
                    if version.prompt_id != prompt.id {
                        return Err(PromptStoreError::Corruption(
                            "prompt chain insert command version ownership is invalid".into(),
                        ));
                    }
                    version.id
                }
                None => prompt.current_version_id,
            };
            let position = command
                .before_link_id
                .and_then(|before| expected_links.iter().position(|link| link.id == before))
                .unwrap_or(expected_links.len());
            if command.before_link_id.is_some() && position == expected_links.len() {
                return Err(PromptStoreError::Corruption(
                    "prompt chain insert command before-link is missing".into(),
                ));
            }
            expected_links.insert(
                position,
                PromptChainLink {
                    id: command.link_id,
                    chain_id,
                    position: 0,
                    prompt_id: prompt.id,
                    prompt_version_id,
                },
            );
        }
        PromptChainCommand::MovePromptChainLink(command) => {
            let position = expected_links
                .iter()
                .position(|link| link.id == command.link_id)
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
                .and_then(|before| expected_links.iter().position(|link| link.id == before))
                .unwrap_or(expected_links.len());
            if command.before_link_id.is_some() && target == expected_links.len() {
                return Err(PromptStoreError::Corruption(
                    "prompt chain move before-link is missing".into(),
                ));
            }
            expected_links.insert(target, moving);
        }
        PromptChainCommand::RemovePromptChainLink(command) => {
            let position = expected_links
                .iter()
                .position(|link| link.id == command.link_id)
                .ok_or_else(|| {
                    PromptStoreError::Corruption("prompt chain remove link is missing".into())
                })?;
            expected_links.remove(position);
        }
        PromptChainCommand::UpdatePromptChainLinkVersion(command) => {
            let position = expected_links
                .iter()
                .position(|link| link.id == command.link_id)
                .ok_or_else(|| {
                    PromptStoreError::Corruption("prompt chain update link is missing".into())
                })?;
            let prompt = load_prompt(tx, expected_links[position].prompt_id)?.ok_or_else(|| {
                PromptStoreError::Corruption("prompt chain update prompt is missing".into())
            })?;
            validate_saved_prompt_record(tx, &prompt)?;
            if expected_links[position].prompt_version_id == prompt.current_version_id {
                return Err(PromptStoreError::Corruption(
                    "prompt chain update no-op must not have an event".into(),
                ));
            }
            expected_links[position].prompt_version_id = prompt.current_version_id;
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
) -> Result<(), PromptStoreError> {
    validate_chain_command_effect(tx, command, event)?;
    match event {
        PromptChainEvent::PromptChainCreated { chain } => {
            if chain.revision != 1
                || chain.archived_at_ms.is_some()
                || chain.title.trim().is_empty()
                || chain.title != chain.title.trim()
                || chain.title.chars().count() > MAX_PROMPT_CHAIN_TITLE_SCALARS
                || chain.description.as_deref().is_some_and(|description| {
                    description.chars().count() > MAX_PROMPT_CHAIN_DESCRIPTION_SCALARS
                        || description != description.trim()
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
                || title.trim().is_empty()
                || title != title.trim()
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
    if stored_command_payload.is_empty() {
        return Err(PromptStoreError::Corruption(
            "prompt receipt command payload is empty".into(),
        ));
    }
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
    Ok(Some(receipt))
}

fn load_chain_receipt(
    tx: &Transaction<'_>,
    command_id: CommandId,
    command: &PromptChainCommand,
    command_sha256: &[u8; 32],
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
    if stored_command_payload.is_empty() {
        return Err(PromptStoreError::Corruption(
            "prompt chain receipt command payload is empty".into(),
        ));
    }
    if stored_hash.as_slice() != command_sha256 {
        return Err(PromptStoreError::IdempotencyConflict);
    }
    let stored_command = decode_chain_command(&stored_command_payload)?;
    if sha256_bytes(&stored_command_payload).as_slice() != stored_hash.as_slice()
        || encode_chain_command(&stored_command)? != stored_command_payload
    {
        return Err(PromptStoreError::Corruption(
            "prompt chain receipt command payload digest or encoding is invalid".into(),
        ));
    }
    let command_payload = encode_chain_command(command)?;
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
    validate_chain_receipt_command(tx, &stored_command, &receipt)?;
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
    if command_payload.is_empty() {
        return Err(PromptStoreError::Corruption(
            "prompt event command payload is empty".into(),
        ));
    }
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
                title: command.title.trim().to_string(),
                description: command
                    .description
                    .as_deref()
                    .map(str::trim)
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
                || title != command.title.trim()
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
) -> Result<(), PromptStoreError> {
    match event {
        PromptEvent::PromptCreated { version, .. }
        | PromptEvent::PromptVersionCreated { version, .. }
            if version.id != receipt_version_id =>
        {
            return Err(PromptStoreError::Corruption(
                "prompt event version receipt lineage is invalid".into(),
            ));
        }
        PromptEvent::PromptCreated { .. } | PromptEvent::PromptVersionCreated { .. } => {}
        PromptEvent::PromptRenamed { prompt_id, .. }
        | PromptEvent::PromptTagsSet { prompt_id, .. }
        | PromptEvent::PromptArchived { prompt_id, .. }
        | PromptEvent::PromptRestored { prompt_id, .. } => {
            let prompt = load_prompt(tx, *prompt_id)?.ok_or_else(|| {
                PromptStoreError::Corruption(
                    "prompt event version lineage references a missing prompt".into(),
                )
            })?;
            if prompt.current_version_id != receipt_version_id {
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
) -> Result<(PromptChainEvent, i64, PromptChainCommand), PromptStoreError> {
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
    if command_payload.is_empty() {
        return Err(PromptStoreError::Corruption(
            "prompt chain event command payload is empty".into(),
        ));
    }
    if command_id_from_bytes(&stored_command_id)? != command_id {
        return Err(PromptStoreError::Corruption(
            "prompt chain event command receipt key mismatch".into(),
        ));
    }
    let command = decode_chain_command(&command_payload)?;
    if sha256_bytes(&command_payload).as_slice() != command_sha256.as_slice()
        || encode_chain_command(&command)? != command_payload
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
    let event = PromptChainEvent::decode(payload)
        .map_err(|error| PromptStoreError::Corruption(error.to_string()))?;
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
    validate_chain_event_command(&command, &event, &receipt, occurred_at_ms)?;
    Ok((event, occurred_at_ms, command))
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
                || chain.title != command.title.trim()
                || chain.description
                    != command
                        .description
                        .as_deref()
                        .map(str::trim)
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
                || title != command.title.trim()
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
    conn: &Connection,
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
    conn: &Connection,
    prompt: &SavedPrompt,
) -> Result<(), PromptStoreError> {
    if prompt.revision == 0 {
        return Err(PromptStoreError::Corruption(
            "saved prompt revision must be positive".into(),
        ));
    }
    if prompt.title != prompt.title.trim()
        || prompt
            .description
            .as_deref()
            .is_some_and(|description| description != description.trim())
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
    conn: &Connection,
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
    if chain.title != chain.title.trim()
        || chain
            .description
            .as_deref()
            .is_some_and(|description| description != description.trim())
    {
        return Err(PromptStoreError::Corruption(
            "prompt chain metadata is not canonical".into(),
        ));
    }
    Ok(Some(chain))
}

fn load_chain_links(
    conn: &Connection,
    chain_id: PromptChainId,
) -> Result<Vec<PromptChainLink>, PromptStoreError> {
    let mut statement = conn.prepare(
        "SELECT link_id, position, prompt_id, prompt_version_id
         FROM prompt_chain_links WHERE chain_id = ?1 ORDER BY position ASC",
    )?;
    let rows = statement.query_map([chain_id.as_bytes().as_slice()], |row| {
        Ok((
            row.get::<_, Vec<u8>>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, Vec<u8>>(2)?,
            row.get::<_, Vec<u8>>(3)?,
        ))
    })?;
    let mut links = Vec::new();
    for row in rows {
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
        let prompt = load_prompt(conn, prompt_id)?.ok_or_else(|| {
            PromptStoreError::Corruption("prompt chain link references missing prompt".into())
        })?;
        validate_saved_prompt_record(conn, &prompt)?;
        links.push(PromptChainLink {
            id: link_id,
            chain_id,
            position,
            prompt_id,
            prompt_version_id,
        });
    }
    Ok(links)
}

fn load_version(
    conn: &Connection,
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

fn load_tags(conn: &Connection, prompt_id: &[u8]) -> Result<Vec<String>, PromptStoreError> {
    let mut statement = conn.prepare(
        "SELECT tag, position FROM prompt_tags
             WHERE prompt_id = ?1 ORDER BY position ASC",
    )?;
    let tags = statement
        .query_map([prompt_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut values = Vec::with_capacity(tags.len());
    for (expected, (tag, position)) in tags.into_iter().enumerate() {
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
    Ok(values)
}

fn load_version_variables(
    conn: &Connection,
    version_id: &[u8],
) -> Result<Vec<String>, PromptStoreError> {
    let mut statement = conn.prepare(
        "SELECT variable, position FROM prompt_version_variables
         WHERE prompt_version_id = ?1 ORDER BY position ASC",
    )?;
    let rows = statement
        .query_map([version_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut variables = Vec::with_capacity(rows.len());
    for (expected, (variable, position)) in rows.into_iter().enumerate() {
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

fn prompt_exists(conn: &Connection, prompt_id: PromptId) -> Result<bool, PromptStoreError> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM saved_prompts WHERE prompt_id = ?1)",
        [prompt_id.as_bytes().as_slice()],
        |row| row.get(0),
    )?)
}

fn next_version_number(conn: &Connection, prompt_id: PromptId) -> Result<u32, PromptStoreError> {
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

fn encode_chain_command(command: &PromptChainCommand) -> Result<Vec<u8>, PromptStoreError> {
    rmp_serde::to_vec_named(&PromptChainCommandWire {
        schema_version: PROMPT_WIRE_SCHEMA_VERSION,
        command,
    })
    .map_err(|_| PromptStoreError::Corruption("prompt chain command encoding failed".into()))
}

fn decode_chain_command(payload: &[u8]) -> Result<PromptChainCommand, PromptStoreError> {
    let wire: PromptChainCommandWireOwned = rmp_serde::from_slice(payload)
        .map_err(|_| PromptStoreError::Corruption("prompt chain command decoding failed".into()))?;
    if wire.schema_version != PROMPT_WIRE_SCHEMA_VERSION {
        return Err(PromptStoreError::Corruption(
            "unsupported prompt chain command schema".into(),
        ));
    }
    wire.command.validate().map_err(|_| {
        PromptStoreError::Corruption("prompt chain command validation failed".into())
    })?;
    let canonical = encode_chain_command(&wire.command)?;
    if canonical != payload {
        return Err(PromptStoreError::Corruption(
            "prompt chain command payload is not canonical".into(),
        ));
    }
    Ok(wire.command)
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
