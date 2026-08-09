use std::fmt;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior};

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
        let command_sha256 = command.fingerprint();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;

        if let Some(receipt) = load_receipt(&tx, command_id, &command_sha256)? {
            tx.commit()?;
            return Ok(receipt);
        }

        command.validate()?;
        let (receipt, event, prompt_id, occurred_at_ms) = apply_command(&tx, command_id, &command)?;
        let receipt_payload = rmp_serde::to_vec_named(&receipt)
            .map_err(|error| PromptStoreError::Corruption(error.to_string()))?;
        tx.execute(
            "INSERT INTO prompt_command_receipts(
                command_id, command_sha256, prompt_id, prompt_version_id, revision,
                receipt, created_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                command_id.as_bytes().as_slice(),
                command_sha256.as_slice(),
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
        let command_sha256 = command.fingerprint();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;

        if let Some(receipt) = load_chain_receipt(&tx, command_id, &command_sha256)? {
            tx.commit()?;
            return Ok(receipt);
        }

        command.validate()?;
        let (receipt, event, chain_id, occurred_at_ms) =
            apply_chain_command(&tx, command_id, &command)?;
        let receipt_payload = rmp_serde::to_vec_named(&receipt)
            .map_err(|error| PromptStoreError::Corruption(error.to_string()))?;
        tx.execute(
            "INSERT INTO prompt_chain_command_receipts(
                command_id, command_sha256, chain_id, chain_link_id, revision,
                receipt, created_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                command_id.as_bytes().as_slice(),
                command_sha256.as_slice(),
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
        load_chain_links(&self.conn, chain_id)
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
        load_prompt(&self.conn, prompt_id)
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
        if load_prompt(&self.conn, prompt_id)?.is_none() {
            return Err(PromptStoreError::NotFound);
        }
        let mut statement = self.conn.prepare(
            "SELECT prompt_version_id, prompt_id, version, body, body_sha256, created_at_ms
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
                ))
            },
        )?;
        rows.map(|row| {
            let (id, owner, version, body, body_sha256, created_at_ms) = row?;
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
        let mut statement = tx.prepare(
            "SELECT event_type, occurred_at_ms, payload
             FROM prompt_events ORDER BY sequence ASC",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Vec<u8>>(2)?,
            ))
        })?;
        let mut events = Vec::new();
        for row in rows {
            let (event_type, occurred_at_ms, payload) = row?;
            let event = PromptEvent::decode(&payload)
                .map_err(|error| PromptStoreError::Corruption(error.to_string()))?;
            if event.event_type() != event_type {
                return Err(PromptStoreError::Corruption(
                    "prompt event type disagrees with payload".into(),
                ));
            }
            events.push((event, occurred_at_ms));
        }
        drop(statement);
        let mut chain_statement = tx.prepare(
            "SELECT event_type, occurred_at_ms, payload
             FROM prompt_chain_events ORDER BY sequence ASC",
        )?;
        let chain_rows = chain_statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Vec<u8>>(2)?,
            ))
        })?;
        let mut chain_events = Vec::new();
        for row in chain_rows {
            let (event_type, occurred_at_ms, payload) = row?;
            let event = PromptChainEvent::decode(&payload)
                .map_err(|error| PromptStoreError::Corruption(error.to_string()))?;
            if event.event_type() != event_type {
                return Err(PromptStoreError::Corruption(
                    "prompt chain event type disagrees with payload".into(),
                ));
            }
            chain_events.push((event, occurred_at_ms));
        }
        drop(chain_statement);
        tx.execute_batch(
            "DELETE FROM prompt_chain_links;
             DELETE FROM prompt_chains;
             DELETE FROM prompt_tags;
             DELETE FROM saved_prompts;",
        )?;
        for (event, occurred_at_ms) in &events {
            apply_event(&tx, event, *occurred_at_ms)?;
        }
        for (event, occurred_at_ms) in &chain_events {
            apply_chain_event(&tx, event, *occurred_at_ms)?;
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
    if prompt.archived_at_ms.is_some() {
        return Err(PromptStoreError::InvalidTransition);
    }
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
    let current_version = load_version(tx, prompt.current_version_id)?
        .ok_or_else(|| PromptStoreError::Corruption("prompt current version is missing".into()))?;
    if current_version.body == version.body && current_version.variables == version.variables {
        let receipt = PromptMutationReceipt {
            command_id,
            prompt_id: prompt.id,
            prompt_version_id: prompt.current_version_id,
            revision: prompt.revision,
        };
        return Ok((receipt, None, prompt.id, command.created_at_ms));
    }
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
    if links.iter().any(|link| link.id == command.link_id) {
        return Err(PromptStoreError::AlreadyExists);
    }
    let prompt = load_prompt(tx, command.prompt_id)?.ok_or(PromptStoreError::NotFound)?;
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
    let position = links
        .iter()
        .position(|link| link.id == command.link_id)
        .ok_or(PromptStoreError::NotFound)?;
    let prompt = load_prompt(tx, links[position].prompt_id)?.ok_or_else(|| {
        PromptStoreError::Corruption("chain link references missing prompt".into())
    })?;
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

fn renumber_links(links: &mut [PromptChainLink]) -> Result<(), PromptStoreError> {
    for (position, link) in links.iter_mut().enumerate() {
        link.position = u32::try_from(position)
            .map_err(|_| PromptStoreError::Corruption("prompt chain is too long".into()))?;
    }
    Ok(())
}

fn validate_chain_links(
    tx: &Transaction<'_>,
    chain_id: PromptChainId,
    links: &[PromptChainLink],
) -> Result<(), PromptStoreError> {
    for (position, link) in links.iter().enumerate() {
        if link.chain_id != chain_id
            || link.position
                != u32::try_from(position)
                    .map_err(|_| PromptStoreError::Corruption("prompt chain is too long".into()))?
        {
            return Err(PromptStoreError::Corruption(
                "prompt chain links must be a dense ordered prefix".into(),
            ));
        }
        let version = load_version(tx, link.prompt_version_id)?.ok_or_else(|| {
            PromptStoreError::Corruption("prompt chain link references missing version".into())
        })?;
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

fn apply_chain_event(
    tx: &Transaction<'_>,
    event: &PromptChainEvent,
    occurred_at_ms: i64,
) -> Result<(), PromptStoreError> {
    match event {
        PromptChainEvent::PromptChainCreated { chain } => {
            if chain.revision != 1
                || chain.title.trim().is_empty()
                || chain.title.chars().count() > MAX_PROMPT_CHAIN_TITLE_SCALARS
                || chain.description.as_deref().is_some_and(|description| {
                    description.chars().count() > MAX_PROMPT_CHAIN_DESCRIPTION_SCALARS
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
            prompt_version_id, prompt_id, version, body, body_sha256, created_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
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
    command_sha256: &[u8; 32],
) -> Result<Option<PromptMutationReceipt>, PromptStoreError> {
    let row: Option<(Vec<u8>, Vec<u8>)> = tx
        .query_row(
            "SELECT command_sha256, receipt FROM prompt_command_receipts WHERE command_id = ?1",
            [command_id.as_bytes().as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((stored_hash, receipt)) = row else {
        return Ok(None);
    };
    if stored_hash.as_slice() != command_sha256 {
        return Err(PromptStoreError::IdempotencyConflict);
    }
    let receipt = rmp_serde::from_slice(&receipt)
        .map_err(|error| PromptStoreError::Corruption(error.to_string()))?;
    Ok(Some(receipt))
}

fn load_chain_receipt(
    tx: &Transaction<'_>,
    command_id: CommandId,
    command_sha256: &[u8; 32],
) -> Result<Option<PromptChainMutationReceipt>, PromptStoreError> {
    let row: Option<(Vec<u8>, Vec<u8>)> = tx
        .query_row(
            "SELECT command_sha256, receipt
             FROM prompt_chain_command_receipts WHERE command_id = ?1",
            [command_id.as_bytes().as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((stored_hash, receipt)) = row else {
        return Ok(None);
    };
    if stored_hash.as_slice() != command_sha256 {
        return Err(PromptStoreError::IdempotencyConflict);
    }
    let receipt = rmp_serde::from_slice(&receipt)
        .map_err(|error| PromptStoreError::Corruption(error.to_string()))?;
    Ok(Some(receipt))
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
    validate_saved_prompt_record(conn, &prompt)?;
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
    let row: Option<(Vec<u8>, i64, String, Vec<u8>, i64)> = conn
        .query_row(
            "SELECT prompt_id, version, body, body_sha256, created_at_ms
             FROM prompt_versions WHERE prompt_version_id = ?1",
            [version_id.as_bytes().as_slice()],
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
    let Some((prompt_id, version, body, body_sha256, created_at_ms)) = row else {
        return Ok(None);
    };
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

fn body_hash(body: &str) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    Sha256::digest(body.as_bytes()).into()
}

fn digest_from_bytes(bytes: &[u8]) -> Result<[u8; 32], PromptStoreError> {
    bytes
        .try_into()
        .map_err(|_| PromptStoreError::Corruption("prompt body digest must be 32 bytes".into()))
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
