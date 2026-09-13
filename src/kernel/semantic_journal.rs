//! Durable semantic-journal rows. Identity is the authority digest plus
//! unique delivery / provider-native IDs. This table is not a DomainEvent
//! projection and never feeds `apply()`.

use std::collections::HashSet;

use rusqlite::types::{Type, ValueRef};
use rusqlite::{Connection, OptionalExtension, Transaction};
use sha2::{Digest, Sha256};

#[cfg(test)]
use std::cell::Cell;

use crate::domain::EventId;
use crate::kernel::StoreError;

const MAX_JOURNAL_EVENTS: usize = 4_096;
const MAX_JOURNAL_PAYLOAD_BYTES: usize = 16 * 1024 * 1024;
const MAX_JOURNAL_TEXT_FIELD_BYTES: usize = 256;
const ALLOWED_JOURNAL_KINDS: &[&str] = &[
    "user_message",
    "assistant_text",
    "reasoning_summary",
    "tool_call",
    "tool_result",
    "approval_request",
    "approval_result",
    "question",
    "plan_step",
    "usage_observation",
    "error",
    "turn_state",
    "session_state",
    "artifact_reference",
    "unknown_provider_event",
];
const ALLOWED_JOURNAL_VISIBILITIES: &[&str] = &["semantic", "diagnostic", "runtime_only"];
const ALLOWED_JOURNAL_PRIVACY_CLASSES: &[&str] = &["local_only", "shareable"];
const ALLOWED_JOURNAL_REDACTION_CLASSES: &[&str] = &[
    "persistable",
    "persistable_local_only",
    "redact_on_persist",
    "metadata_only",
    "never_persist",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SemanticJournalAuthorityRecord {
    pub digest: [u8; 32],
    pub provider_kind: String,
    pub task_id: [u8; 16],
    pub agent_session_id: [u8; 16],
    pub resource_id: [u8; 16],
    pub runtime_generation: i64,
    pub action_epoch: i64,
    pub managed_root: [u8; 32],
    pub opened_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SemanticJournalFactRow {
    pub sequence: i64,
    pub event_id: [u8; 16],
    pub delivery_id: String,
    pub provider_event_id: Option<String>,
    pub content_hash: [u8; 32],
    pub kind: String,
    pub visibility: String,
    pub privacy_class: String,
    pub redaction_class: String,
    pub occurred_at_ms: i64,
    pub ingested_at_ms: i64,
    pub schema_version: i64,
    pub payload: Vec<u8>,
}

/// A SQLite-borrowed row view used to decide page admission. It intentionally
/// carries no owned payload or row strings: callers must make the byte-budget
/// decision before asking the kernel to materialize a [`SemanticJournalFactRow`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct SemanticJournalFactRef<'a> {
    pub sequence: i64,
    pub event_id: &'a [u8],
    pub delivery_id: &'a str,
    pub provider_event_id: Option<&'a str>,
    pub content_hash: &'a [u8],
    pub kind: &'a str,
    pub visibility: &'a str,
    pub privacy_class: &'a str,
    pub redaction_class: &'a str,
    pub occurred_at_ms: i64,
    pub ingested_at_ms: i64,
    pub schema_version: i64,
    pub payload: &'a [u8],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SemanticJournalPageRowAction {
    Fetch,
    Skip,
    Stop,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct SemanticJournalPageRowMeta {
    pub sequence: u64,
    pub runtime_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SemanticJournalDedupeHit {
    pub event_id: [u8; 16],
    pub content_hash: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SemanticJournalWrite {
    Inserted {
        sequence: u64,
    },
    Duplicate {
        event_id: [u8; 16],
        content_hash: [u8; 32],
    },
    Conflict {
        event_id: [u8; 16],
        content_hash: [u8; 32],
    },
    KeyConflict {
        delivery_event_id: [u8; 16],
        provider_event_id: [u8; 16],
    },
    EventCapacity,
    DedupeCapacity,
    SequenceOverflow,
    TimestampRegression,
}

const ALLOWED_PROVIDER_KINDS: &[&str] = &["claude_code", "codex", "cursor"];
const STORE_INSTANCE_KIND: &str = "store_instance";

#[cfg(test)]
thread_local! {
    static SEMANTIC_JOURNAL_ROW_MATERIALIZATIONS: Cell<usize> = const { Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn debug_reset_semantic_journal_materialization_counters() {
    SEMANTIC_JOURNAL_ROW_MATERIALIZATIONS.with(|counter| counter.set(0));
}

#[cfg(test)]
pub(crate) fn debug_semantic_journal_materialization_counters() -> usize {
    SEMANTIC_JOURNAL_ROW_MATERIALIZATIONS.with(Cell::get)
}

#[cfg(test)]
fn debug_record_semantic_journal_row_materialization() {
    SEMANTIC_JOURNAL_ROW_MATERIALIZATIONS.with(|counter| {
        counter.set(counter.get().saturating_add(1));
    });
}

fn store_instance_digest() -> [u8; 32] {
    Sha256::digest(b"devmanager.semantic_journal.store_instance.v1").into()
}

pub(crate) fn ensure_store_instance(
    tx: &Transaction<'_>,
    opened_at_ms: i64,
) -> Result<[u8; 16], StoreError> {
    let digest = store_instance_digest();
    let existing: Option<(String, Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>)> = tx
        .query_row(
            "SELECT provider_kind, task_id, agent_session_id, resource_id, managed_root
             FROM semantic_journal_sessions
             WHERE authority_digest = ?1",
            [digest.as_slice()],
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
    if let Some((provider_kind, task_id, agent_session_id, resource_id, managed_root)) = existing {
        if provider_kind != STORE_INSTANCE_KIND {
            return Err(StoreError::ConstraintViolation);
        }
        let store_id = exact16(task_id)?;
        let agent_session_id = exact16(agent_session_id)?;
        let resource_id = exact16(resource_id)?;
        let managed_root = exact32(managed_root)?;
        let mut expected_root = [0u8; 32];
        expected_root[..16].copy_from_slice(&store_id);
        expected_root[16..].copy_from_slice(&store_id);
        if agent_session_id != store_id || resource_id != store_id || managed_root != expected_root
        {
            return Err(StoreError::Corruption);
        }
        return Ok(store_id);
    }
    let store_id = *EventId::new().as_bytes();
    let mut managed_root = [0u8; 32];
    managed_root[..16].copy_from_slice(&store_id);
    managed_root[16..].copy_from_slice(&store_id);
    tx.execute(
        "INSERT INTO semantic_journal_sessions(
            authority_digest, provider_kind, task_id, agent_session_id, resource_id,
            runtime_generation, action_epoch, managed_root, opened_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        rusqlite::params![
            digest.as_slice(),
            STORE_INSTANCE_KIND,
            store_id.as_slice(),
            store_id.as_slice(),
            store_id.as_slice(),
            0_i64,
            0_i64,
            managed_root.as_slice(),
            opened_at_ms,
        ],
    )?;
    Ok(store_id)
}

pub(crate) fn ensure_session(
    tx: &Transaction<'_>,
    record: &SemanticJournalAuthorityRecord,
) -> Result<[u8; 16], StoreError> {
    let store_id = ensure_store_instance(tx, record.opened_at_ms)?;
    let existing: Option<(String, Vec<u8>, Vec<u8>, Vec<u8>, i64, i64, Vec<u8>)> = tx
        .query_row(
            "SELECT provider_kind, task_id, agent_session_id, resource_id,
                    runtime_generation, action_epoch, managed_root
             FROM semantic_journal_sessions
             WHERE authority_digest = ?1",
            [record.digest.as_slice()],
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
    if let Some((
        provider_kind,
        task_id,
        agent_session_id,
        resource_id,
        runtime_generation,
        action_epoch,
        managed_root,
    )) = existing
    {
        let task_id = exact16(task_id)?;
        let agent_session_id = exact16(agent_session_id)?;
        let resource_id = exact16(resource_id)?;
        let managed_root = exact32(managed_root)?;
        if !ALLOWED_PROVIDER_KINDS.contains(&provider_kind.as_str())
            || provider_kind != record.provider_kind
            || task_id != record.task_id
            || agent_session_id != record.agent_session_id
            || resource_id != record.resource_id
            || runtime_generation != record.runtime_generation
            || action_epoch != record.action_epoch
            || managed_root != record.managed_root
        {
            return Err(StoreError::ConstraintViolation);
        }
        return Ok(store_id);
    }
    if !ALLOWED_PROVIDER_KINDS.contains(&record.provider_kind.as_str()) {
        return Err(StoreError::ConstraintViolation);
    }
    tx.execute(
        "INSERT INTO semantic_journal_sessions(
            authority_digest, provider_kind, task_id, agent_session_id, resource_id,
            runtime_generation, action_epoch, managed_root, opened_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        rusqlite::params![
            record.digest.as_slice(),
            record.provider_kind,
            record.task_id.as_slice(),
            record.agent_session_id.as_slice(),
            record.resource_id.as_slice(),
            record.runtime_generation,
            record.action_epoch,
            record.managed_root.as_slice(),
            record.opened_at_ms,
        ],
    )?;
    Ok(store_id)
}

pub(crate) fn high_water(
    conn: &Connection,
    digest: &[u8; 32],
) -> Result<(u64, Option<i64>), StoreError> {
    high_water_with_validator(conn, digest, |_| Ok(()))
}

pub(crate) fn high_water_with_validator(
    conn: &Connection,
    digest: &[u8; 32],
    validate_row: impl FnMut(&SemanticJournalFactRow) -> Result<(), StoreError>,
) -> Result<(u64, Option<i64>), StoreError> {
    let (count, last_occurred_at_ms, _) = validate_facts(conn, digest, validate_row)?;
    let next = if count == 0 {
        1
    } else {
        count.checked_add(1).ok_or(StoreError::IntegerOutOfRange {
            field: "semantic_journal.sequence",
            value: u64::MAX,
        })?
    };
    Ok((next, last_occurred_at_ms))
}

pub(crate) fn retained_len(conn: &Connection, digest: &[u8; 32]) -> Result<usize, StoreError> {
    let (count, _, _) = validate_facts(conn, digest, |_| Ok(()))?;
    usize::try_from(count).map_err(|_| StoreError::IntegerOutOfRange {
        field: "semantic_journal.count",
        value: u64::MAX,
    })
}

/// Validate every durable fact's bounded envelope before exposing its count or
/// allowing another write. This is intentionally a row scan rather than an
/// aggregate: SQLite constraints protect shape, while this check protects the
/// application-level identity, sequence, and payload invariants after an
/// already-open handle observes an external mutation.
pub(crate) fn validate_facts(
    conn: &Connection,
    digest: &[u8; 32],
    mut validate_row: impl FnMut(&SemanticJournalFactRow) -> Result<(), StoreError>,
) -> Result<(u64, Option<i64>, Option<i64>), StoreError> {
    // Return both timestamp high-waters from this same ordered scan. Writers
    // use the pair inside their IMMEDIATE transaction, after dedupe lookups,
    // so neither stream can regress around a concurrent handle.
    let mut stmt = conn.prepare(
        "SELECT sequence, event_id, delivery_id, provider_event_id, content_hash,
                kind, visibility, privacy_class, redaction_class, occurred_at_ms,
                ingested_at_ms, schema_version, payload
         FROM semantic_journal_facts
         WHERE authority_digest = ?1
         ORDER BY sequence ASC",
    )?;
    let mut rows = stmt.query([digest.as_slice()])?;
    let mut expected_sequence = 1_i64;
    let mut count = 0_u64;
    let mut last_occurred_at_ms: Option<i64> = None;
    let mut last_ingested_at_ms: Option<i64> = None;
    let mut event_ids = HashSet::new();
    let mut delivery_ids = HashSet::new();
    let mut provider_event_ids: HashSet<String> = HashSet::new();
    while let Some(row) = rows.next()? {
        count = count.checked_add(1).ok_or(StoreError::Corruption)?;
        if count > MAX_JOURNAL_EVENTS as u64 {
            return Err(StoreError::Corruption);
        }
        let fact = finalize_fact(map_raw_fact(row)?)?;
        if fact.sequence != expected_sequence {
            return Err(StoreError::Corruption);
        }
        if !event_ids.insert(fact.event_id)
            || !delivery_ids.insert(fact.delivery_id.clone())
            || fact
                .provider_event_id
                .as_ref()
                .is_some_and(|provider_event_id| {
                    !provider_event_ids.insert(provider_event_id.clone())
                })
        {
            return Err(StoreError::Corruption);
        }
        if last_occurred_at_ms.is_some_and(|last| fact.occurred_at_ms < last) {
            return Err(StoreError::Corruption);
        }
        if last_ingested_at_ms.is_some_and(|last| fact.ingested_at_ms < last) {
            return Err(StoreError::Corruption);
        }
        expected_sequence = expected_sequence
            .checked_add(1)
            .ok_or(StoreError::Corruption)?;
        last_occurred_at_ms = Some(fact.occurred_at_ms);
        last_ingested_at_ms = Some(fact.ingested_at_ms);
        validate_row(&fact)?;
    }
    Ok((count, last_occurred_at_ms, last_ingested_at_ms))
}

/// Validate the bounded SQLite row contract without copying any persisted row
/// into owned storage. The payload remains borrowed from SQLite so the caller
/// can run a bounded body-integrity pass for every row before page admission.
pub(crate) fn validate_fact_metadata(
    conn: &Connection,
    digest: &[u8; 32],
    mut validate_row: impl for<'a> FnMut(&SemanticJournalFactRef<'a>) -> Result<(), StoreError>,
) -> Result<(u64, Option<i64>, Option<i64>), StoreError> {
    let mut stmt = conn.prepare(
        "SELECT sequence, event_id, delivery_id, provider_event_id, content_hash,
                kind, visibility, privacy_class, redaction_class, occurred_at_ms,
                ingested_at_ms, schema_version, payload
         FROM semantic_journal_facts
         WHERE authority_digest = ?1
         ORDER BY sequence ASC",
    )?;
    let mut rows = stmt.query([digest.as_slice()])?;
    let mut expected_sequence = 1_i64;
    let mut count = 0_u64;
    let mut last_occurred_at_ms: Option<i64> = None;
    let mut last_ingested_at_ms: Option<i64> = None;
    let mut event_ids = HashSet::new();
    let mut delivery_ids = HashSet::new();
    let mut provider_event_ids = HashSet::new();
    while let Some(row) = rows.next()? {
        count = count.checked_add(1).ok_or(StoreError::Corruption)?;
        if count > MAX_JOURNAL_EVENTS as u64 {
            return Err(StoreError::Corruption);
        }
        let metadata = map_fact_metadata(row)?;
        let event_id = exact16_ref(metadata.event_id)?;
        EventId::from_bytes(event_id).map_err(|_| StoreError::Corruption)?;
        let content_hash = exact32_ref(metadata.content_hash)?;
        if content_hash == [0u8; 32]
            || metadata.sequence != expected_sequence
            || metadata.sequence <= 0
            || metadata.occurred_at_ms < 0
            || metadata.ingested_at_ms < 0
            || metadata.schema_version <= 0
            || metadata.payload.is_empty()
            || metadata.payload.len() > MAX_JOURNAL_PAYLOAD_BYTES
            || !ALLOWED_JOURNAL_KINDS.contains(&metadata.kind)
            || !ALLOWED_JOURNAL_VISIBILITIES.contains(&metadata.visibility)
            || !ALLOWED_JOURNAL_PRIVACY_CLASSES.contains(&metadata.privacy_class)
            || !ALLOWED_JOURNAL_REDACTION_CLASSES.contains(&metadata.redaction_class)
        {
            return Err(StoreError::Corruption);
        }
        validate_text_field(metadata.delivery_id)?;
        if let Some(provider_event_id) = metadata.provider_event_id {
            validate_text_field(provider_event_id)?;
        }
        validate_text_field(metadata.kind)?;
        validate_text_field(metadata.visibility)?;
        validate_text_field(metadata.privacy_class)?;
        validate_text_field(metadata.redaction_class)?;
        if !event_ids.insert(event_id)
            || !delivery_ids.insert(metadata.delivery_id.to_owned())
            || metadata.provider_event_id.is_some_and(|provider_event_id| {
                !provider_event_ids.insert(provider_event_id.to_owned())
            })
        {
            return Err(StoreError::Corruption);
        }
        if last_occurred_at_ms.is_some_and(|last| metadata.occurred_at_ms < last)
            || last_ingested_at_ms.is_some_and(|last| metadata.ingested_at_ms < last)
        {
            return Err(StoreError::Corruption);
        }
        let row_ref = SemanticJournalFactRef {
            sequence: metadata.sequence,
            event_id: metadata.event_id,
            delivery_id: metadata.delivery_id,
            provider_event_id: metadata.provider_event_id,
            content_hash: metadata.content_hash,
            kind: metadata.kind,
            visibility: metadata.visibility,
            privacy_class: metadata.privacy_class,
            redaction_class: metadata.redaction_class,
            occurred_at_ms: metadata.occurred_at_ms,
            ingested_at_ms: metadata.ingested_at_ms,
            schema_version: metadata.schema_version,
            payload: metadata.payload,
        };
        validate_row(&row_ref)?;
        expected_sequence = expected_sequence
            .checked_add(1)
            .ok_or(StoreError::Corruption)?;
        last_occurred_at_ms = Some(metadata.occurred_at_ms);
        last_ingested_at_ms = Some(metadata.ingested_at_ms);
    }
    Ok((count, last_occurred_at_ms, last_ingested_at_ms))
}

pub(crate) fn lookup_delivery(
    conn: &Connection,
    digest: &[u8; 32],
    delivery_id: &str,
) -> Result<Option<SemanticJournalDedupeHit>, StoreError> {
    conn.query_row(
        "SELECT event_id, content_hash
         FROM semantic_journal_facts
         WHERE authority_digest = ?1 AND delivery_id = ?2",
        rusqlite::params![digest.as_slice(), delivery_id],
        map_dedupe_hit,
    )
    .optional()
    .map_err(StoreError::from)
    .and_then(|hit| hit.map(finalize_dedupe_hit).transpose())
}

pub(crate) fn lookup_provider_event(
    conn: &Connection,
    digest: &[u8; 32],
    provider_event_id: &str,
) -> Result<Option<SemanticJournalDedupeHit>, StoreError> {
    conn.query_row(
        "SELECT event_id, content_hash
         FROM semantic_journal_facts
         WHERE authority_digest = ?1 AND provider_event_id = ?2",
        rusqlite::params![digest.as_slice(), provider_event_id],
        map_dedupe_hit,
    )
    .optional()
    .map_err(StoreError::from)
    .and_then(|hit| hit.map(finalize_dedupe_hit).transpose())
}

pub(crate) fn write_fact(
    tx: &Transaction<'_>,
    digest: &[u8; 32],
    delivery_id: &str,
    provider_event_id: Option<&str>,
    payload_hash: [u8; 32],
    mut row: SemanticJournalFactRow,
    max_events: u32,
    max_dedupe_keys: u32,
    mut validate_row: impl FnMut(&SemanticJournalFactRow) -> Result<(), StoreError>,
) -> Result<SemanticJournalWrite, StoreError> {
    // Validate the complete pinned journal before any write decision. Dedupe
    // lookups then precede the candidate timestamp check, so an older retry
    // remains a duplicate rather than becoming a timestamp regression.
    let (count, last_occurred_at_ms, last_ingested_at_ms) =
        validate_facts(tx, digest, &mut validate_row)?;
    let next_sequence = if count == 0 {
        1
    } else {
        count.checked_add(1).ok_or(StoreError::Corruption)?
    };
    let delivery_hit = lookup_delivery(tx, digest, delivery_id)?;
    let provider_hit = if let Some(provider_event_id) = provider_event_id {
        lookup_provider_event(tx, digest, provider_event_id)?
    } else {
        None
    };
    if let (Some(delivery_hit), Some(provider_hit)) = (&delivery_hit, &provider_hit) {
        if delivery_hit.event_id != provider_hit.event_id
            || delivery_hit.content_hash != provider_hit.content_hash
        {
            return Ok(SemanticJournalWrite::KeyConflict {
                delivery_event_id: delivery_hit.event_id,
                provider_event_id: provider_hit.event_id,
            });
        }
    }
    if let Some(hit) = delivery_hit.or(provider_hit) {
        return Ok(classify_hit(hit, payload_hash));
    }
    if last_occurred_at_ms.is_some_and(|last| row.occurred_at_ms < last) {
        return Ok(SemanticJournalWrite::TimestampRegression);
    }
    if last_ingested_at_ms.is_some_and(|last| row.ingested_at_ms < last) {
        return Ok(SemanticJournalWrite::TimestampRegression);
    }
    if count as u32 >= max_events {
        return Ok(SemanticJournalWrite::EventCapacity);
    }
    if count.saturating_mul(2) as u32 >= max_dedupe_keys {
        return Ok(SemanticJournalWrite::DedupeCapacity);
    }
    if next_sequence == u64::MAX {
        return Ok(SemanticJournalWrite::SequenceOverflow);
    }
    let sequence = i64::try_from(next_sequence).map_err(|_| StoreError::IntegerOutOfRange {
        field: "semantic_journal.sequence",
        value: next_sequence,
    })?;
    row.sequence = sequence;
    validate_fact_fields(&row)?;
    validate_row(&row)?;
    insert_fact(tx, digest, &row)?;
    Ok(SemanticJournalWrite::Inserted {
        sequence: next_sequence,
    })
}

fn classify_hit(hit: SemanticJournalDedupeHit, payload_hash: [u8; 32]) -> SemanticJournalWrite {
    if hit.content_hash == payload_hash {
        SemanticJournalWrite::Duplicate {
            event_id: hit.event_id,
            content_hash: hit.content_hash,
        }
    } else {
        SemanticJournalWrite::Conflict {
            event_id: hit.event_id,
            content_hash: hit.content_hash,
        }
    }
}

pub(crate) fn insert_fact(
    tx: &Transaction<'_>,
    digest: &[u8; 32],
    row: &SemanticJournalFactRow,
) -> Result<(), StoreError> {
    tx.execute(
        "INSERT INTO semantic_journal_facts(
            authority_digest, sequence, event_id, delivery_id, provider_event_id,
            content_hash, kind, visibility, privacy_class, redaction_class,
            occurred_at_ms, ingested_at_ms, schema_version, payload
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        rusqlite::params![
            digest.as_slice(),
            row.sequence,
            row.event_id.as_slice(),
            row.delivery_id,
            row.provider_event_id,
            row.content_hash.as_slice(),
            row.kind,
            row.visibility,
            row.privacy_class,
            row.redaction_class,
            row.occurred_at_ms,
            row.ingested_at_ms,
            row.schema_version,
            row.payload,
        ],
    )?;
    Ok(())
}

pub(crate) fn load_fact(
    conn: &Connection,
    digest: &[u8; 32],
    sequence: i64,
) -> Result<Option<SemanticJournalFactRow>, StoreError> {
    conn.query_row(
        "SELECT sequence, event_id, delivery_id, provider_event_id, content_hash,
                kind, visibility, privacy_class, redaction_class, occurred_at_ms,
                ingested_at_ms, schema_version, payload
         FROM semantic_journal_facts
         WHERE authority_digest = ?1 AND sequence = ?2",
        rusqlite::params![digest.as_slice(), sequence],
        map_raw_fact,
    )
    .optional()
    .map_err(StoreError::from)
    .and_then(|row| row.map(finalize_fact).transpose())
}

/// Stream facts from one pinned read transaction. A borrowed row preflight is
/// invoked while SQLite still owns the row, before any payload or scalar value
/// is copied into a [`SemanticJournalFactRow`]. Only admitted rows take that
/// owned path; callers may stop without fetching the next candidate.
pub(crate) fn stream_page(
    tx: &Transaction<'_>,
    digest: &[u8; 32],
    after_sequence: i64,
    high_water: i64,
    mut prepare: impl FnMut(u64, &[SemanticJournalPageRowMeta]) -> Result<(), StoreError>,
    mut preflight: impl for<'a> FnMut(
        u64,
        SemanticJournalFactRef<'a>,
    ) -> Result<SemanticJournalPageRowAction, StoreError>,
    mut visit: impl FnMut(SemanticJournalFactRow) -> Result<bool, StoreError>,
) -> Result<(), StoreError> {
    let high_water_i64 = high_water;
    let high_water = u64::try_from(high_water_i64).map_err(|_| StoreError::Corruption)?;
    let mut metadata_stmt = tx.prepare(
        "SELECT sequence, visibility = 'runtime_only'
         FROM semantic_journal_facts
         WHERE authority_digest = ?1
           AND sequence > ?2
           AND sequence <= ?3
         ORDER BY sequence ASC",
    )?;
    let mut metadata_rows = metadata_stmt.query(rusqlite::params![
        digest.as_slice(),
        after_sequence,
        high_water_i64,
    ])?;
    let mut metadata = Vec::new();
    while let Some(row) = metadata_rows.next()? {
        let sequence = u64::try_from(row.get::<_, i64>(0)?).map_err(|_| StoreError::Corruption)?;
        let runtime_only = row.get::<_, i64>(1)? != 0;
        metadata.push(SemanticJournalPageRowMeta {
            sequence,
            runtime_only,
        });
    }
    drop(metadata_rows);
    drop(metadata_stmt);
    prepare(high_water, &metadata)?;

    let mut stmt = tx.prepare(
        "SELECT sequence, event_id, delivery_id, provider_event_id, content_hash,
                kind, visibility, privacy_class, redaction_class, occurred_at_ms,
                ingested_at_ms, schema_version, payload
         FROM semantic_journal_facts
         WHERE authority_digest = ?1
           AND sequence > ?2
           AND sequence <= ?3
         ORDER BY sequence ASC",
    )?;
    let mut rows = stmt.query(rusqlite::params![
        digest.as_slice(),
        after_sequence,
        high_water_i64,
    ])?;
    while let Some(row) = rows.next()? {
        let action = {
            let row_ref = map_fact_ref(row)?;
            preflight(high_water, row_ref)?
        };
        match action {
            SemanticJournalPageRowAction::Stop => break,
            SemanticJournalPageRowAction::Skip => continue,
            SemanticJournalPageRowAction::Fetch => {}
        }
        let fact = finalize_fact(map_raw_fact(row)?)?;
        if !visit(fact)? {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn debug_delete_fact(
    tx: &Transaction<'_>,
    digest: &[u8; 32],
    sequence: i64,
) -> Result<(), StoreError> {
    tx.execute(
        "DELETE FROM semantic_journal_facts
         WHERE authority_digest = ?1 AND sequence = ?2",
        rusqlite::params![digest.as_slice(), sequence],
    )?;
    Ok(())
}

#[cfg(test)]
pub(crate) fn debug_zero_event_id(
    tx: &Transaction<'_>,
    digest: &[u8; 32],
    sequence: i64,
) -> Result<(), StoreError> {
    tx.execute(
        "UPDATE semantic_journal_facts SET event_id = ?1
         WHERE authority_digest = ?2 AND sequence = ?3",
        rusqlite::params![[0u8; 16].as_slice(), digest.as_slice(), sequence],
    )?;
    Ok(())
}

struct RawDedupeHit {
    event_id: Vec<u8>,
    content_hash: Vec<u8>,
}

struct RawFact {
    sequence: i64,
    event_id: Vec<u8>,
    delivery_id: String,
    provider_event_id: Option<String>,
    content_hash: Vec<u8>,
    kind: String,
    visibility: String,
    privacy_class: String,
    redaction_class: String,
    occurred_at_ms: i64,
    ingested_at_ms: i64,
    schema_version: i64,
    payload: Vec<u8>,
}

struct SemanticJournalFactMetadata<'a> {
    sequence: i64,
    event_id: &'a [u8],
    delivery_id: &'a str,
    provider_event_id: Option<&'a str>,
    content_hash: &'a [u8],
    kind: &'a str,
    visibility: &'a str,
    privacy_class: &'a str,
    redaction_class: &'a str,
    occurred_at_ms: i64,
    ingested_at_ms: i64,
    schema_version: i64,
    payload: &'a [u8],
}

fn map_dedupe_hit(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawDedupeHit> {
    Ok(RawDedupeHit {
        event_id: row.get(0)?,
        content_hash: row.get(1)?,
    })
}

fn finalize_dedupe_hit(raw: RawDedupeHit) -> Result<SemanticJournalDedupeHit, StoreError> {
    let event_id = exact16(raw.event_id)?;
    EventId::from_bytes(event_id).map_err(|_| StoreError::Corruption)?;
    Ok(SemanticJournalDedupeHit {
        event_id,
        content_hash: exact32(raw.content_hash)?,
    })
}

fn map_raw_fact(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawFact> {
    #[cfg(test)]
    debug_record_semantic_journal_row_materialization();
    Ok(RawFact {
        sequence: row.get(0)?,
        event_id: row.get(1)?,
        delivery_id: row.get(2)?,
        provider_event_id: row.get(3)?,
        content_hash: row.get(4)?,
        kind: row.get(5)?,
        visibility: row.get(6)?,
        privacy_class: row.get(7)?,
        redaction_class: row.get(8)?,
        occurred_at_ms: row.get(9)?,
        ingested_at_ms: row.get(10)?,
        schema_version: row.get(11)?,
        payload: row.get(12)?,
    })
}

fn map_fact_metadata<'a>(
    row: &'a rusqlite::Row<'a>,
) -> rusqlite::Result<SemanticJournalFactMetadata<'a>> {
    Ok(SemanticJournalFactMetadata {
        sequence: row.get(0)?,
        event_id: blob_ref(row, 1)?,
        delivery_id: text_ref(row, 2)?,
        provider_event_id: optional_text_ref(row, 3)?,
        content_hash: blob_ref(row, 4)?,
        kind: text_ref(row, 5)?,
        visibility: text_ref(row, 6)?,
        privacy_class: text_ref(row, 7)?,
        redaction_class: text_ref(row, 8)?,
        occurred_at_ms: row.get(9)?,
        ingested_at_ms: row.get(10)?,
        schema_version: row.get(11)?,
        payload: blob_ref(row, 12)?,
    })
}

fn map_fact_ref<'a>(row: &'a rusqlite::Row<'a>) -> rusqlite::Result<SemanticJournalFactRef<'a>> {
    Ok(SemanticJournalFactRef {
        sequence: row.get(0)?,
        event_id: blob_ref(row, 1)?,
        delivery_id: text_ref(row, 2)?,
        provider_event_id: optional_text_ref(row, 3)?,
        content_hash: blob_ref(row, 4)?,
        kind: text_ref(row, 5)?,
        visibility: text_ref(row, 6)?,
        privacy_class: text_ref(row, 7)?,
        redaction_class: text_ref(row, 8)?,
        occurred_at_ms: row.get(9)?,
        ingested_at_ms: row.get(10)?,
        schema_version: row.get(11)?,
        payload: blob_ref(row, 12)?,
    })
}

fn blob_ref<'a>(row: &'a rusqlite::Row<'_>, index: usize) -> rusqlite::Result<&'a [u8]> {
    match row.get_ref(index)? {
        ValueRef::Blob(value) => Ok(value),
        ValueRef::Null => Err(rusqlite::Error::InvalidColumnType(
            index,
            "blob".into(),
            Type::Blob,
        )),
        ValueRef::Integer(_) | ValueRef::Real(_) | ValueRef::Text(_) => Err(
            rusqlite::Error::InvalidColumnType(index, "blob".into(), Type::Blob),
        ),
    }
}

fn text_ref<'a>(row: &'a rusqlite::Row<'_>, index: usize) -> rusqlite::Result<&'a str> {
    match row.get_ref(index)? {
        ValueRef::Text(value) => std::str::from_utf8(value).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(index, Type::Text, Box::new(error))
        }),
        ValueRef::Null | ValueRef::Blob(_) | ValueRef::Integer(_) | ValueRef::Real(_) => Err(
            rusqlite::Error::InvalidColumnType(index, "text".into(), Type::Text),
        ),
    }
}

fn optional_text_ref<'a>(
    row: &'a rusqlite::Row<'_>,
    index: usize,
) -> rusqlite::Result<Option<&'a str>> {
    match row.get_ref(index)? {
        ValueRef::Null => Ok(None),
        ValueRef::Text(value) => std::str::from_utf8(value).map(Some).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(index, Type::Text, Box::new(error))
        }),
        ValueRef::Blob(_) | ValueRef::Integer(_) | ValueRef::Real(_) => Err(
            rusqlite::Error::InvalidColumnType(index, "text".into(), Type::Text),
        ),
    }
}

fn finalize_fact(raw: RawFact) -> Result<SemanticJournalFactRow, StoreError> {
    if raw.sequence <= 0
        || raw.occurred_at_ms < 0
        || raw.ingested_at_ms < 0
        || raw.schema_version <= 0
        || raw.payload.is_empty()
        || raw.payload.len() > MAX_JOURNAL_PAYLOAD_BYTES
    {
        return Err(StoreError::Corruption);
    }
    let event_id = exact16(raw.event_id)?;
    EventId::from_bytes(event_id).map_err(|_| StoreError::Corruption)?;
    let fact = SemanticJournalFactRow {
        sequence: raw.sequence,
        event_id,
        delivery_id: raw.delivery_id,
        provider_event_id: raw.provider_event_id,
        content_hash: exact32(raw.content_hash)?,
        kind: raw.kind,
        visibility: raw.visibility,
        privacy_class: raw.privacy_class,
        redaction_class: raw.redaction_class,
        occurred_at_ms: raw.occurred_at_ms,
        ingested_at_ms: raw.ingested_at_ms,
        schema_version: raw.schema_version,
        payload: raw.payload,
    };
    validate_fact_fields(&fact)?;
    Ok(fact)
}

fn validate_fact_fields(fact: &SemanticJournalFactRow) -> Result<(), StoreError> {
    if fact.sequence <= 0
        || fact.occurred_at_ms < 0
        || fact.ingested_at_ms < 0
        || fact.schema_version <= 0
        || fact.payload.is_empty()
        || fact.payload.len() > MAX_JOURNAL_PAYLOAD_BYTES
        || !ALLOWED_JOURNAL_KINDS.contains(&fact.kind.as_str())
        || !ALLOWED_JOURNAL_VISIBILITIES.contains(&fact.visibility.as_str())
        || !ALLOWED_JOURNAL_PRIVACY_CLASSES.contains(&fact.privacy_class.as_str())
        || !ALLOWED_JOURNAL_REDACTION_CLASSES.contains(&fact.redaction_class.as_str())
    {
        return Err(StoreError::Corruption);
    }
    validate_text_field(&fact.delivery_id)?;
    if let Some(provider_event_id) = &fact.provider_event_id {
        validate_text_field(provider_event_id)?;
    }
    validate_text_field(&fact.kind)?;
    validate_text_field(&fact.visibility)?;
    validate_text_field(&fact.privacy_class)?;
    validate_text_field(&fact.redaction_class)?;
    EventId::from_bytes(fact.event_id).map_err(|_| StoreError::Corruption)?;
    if fact.content_hash == [0u8; 32] {
        return Err(StoreError::Corruption);
    }
    Ok(())
}

fn validate_text_field(value: &str) -> Result<(), StoreError> {
    if value.is_empty()
        || value.len() > MAX_JOURNAL_TEXT_FIELD_BYTES
        || value.chars().any(char::is_control)
    {
        return Err(StoreError::Corruption);
    }
    Ok(())
}

pub(crate) fn exact16(bytes: Vec<u8>) -> Result<[u8; 16], StoreError> {
    <[u8; 16]>::try_from(bytes).map_err(|_| StoreError::Corruption)
}

fn exact16_ref(bytes: &[u8]) -> Result<[u8; 16], StoreError> {
    <[u8; 16]>::try_from(bytes).map_err(|_| StoreError::Corruption)
}

fn exact32_ref(bytes: &[u8]) -> Result<[u8; 32], StoreError> {
    <[u8; 32]>::try_from(bytes).map_err(|_| StoreError::Corruption)
}

pub(crate) fn exact32(bytes: Vec<u8>) -> Result<[u8; 32], StoreError> {
    <[u8; 32]>::try_from(bytes).map_err(|_| StoreError::Corruption)
}
