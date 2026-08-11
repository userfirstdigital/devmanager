//! Durable semantic-journal rows. Identity is the authority digest plus
//! unique delivery / provider-native IDs. This table is not a DomainEvent
//! projection and never feeds `apply()`.

use rusqlite::{Connection, OptionalExtension, Transaction};
use sha2::{Digest, Sha256};

use crate::domain::EventId;
use crate::domain::MAX_SNAPSHOT_PAGE_ITEMS;
use crate::kernel::StoreError;

const MAX_JOURNAL_PAGE_LIMIT: u32 = MAX_SNAPSHOT_PAGE_ITEMS.saturating_add(1);

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
    EventCapacity,
    DedupeCapacity,
    SequenceOverflow,
}

const ALLOWED_PROVIDER_KINDS: &[&str] = &["claude_code", "codex", "cursor"];
const STORE_INSTANCE_KIND: &str = "store_instance";

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
    let row: (i64, Option<i64>, Option<i64>) = conn.query_row(
        "SELECT COUNT(*), MAX(sequence), MAX(occurred_at_ms)
         FROM semantic_journal_facts
         WHERE authority_digest = ?1",
        [digest.as_slice()],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    let count = u64::try_from(row.0).map_err(|_| StoreError::Corruption)?;
    let next = match row.1 {
        None if count == 0 => 1,
        None => return Err(StoreError::Corruption),
        Some(max) => {
            let max = u64::try_from(max).map_err(|_| StoreError::Corruption)?;
            if max == 0 || max != count {
                return Err(StoreError::Corruption);
            }
            max.checked_add(1).ok_or(StoreError::IntegerOutOfRange {
                field: "semantic_journal.sequence",
                value: u64::MAX,
            })?
        }
    };
    Ok((next, row.2))
}

pub(crate) fn retained_len(conn: &Connection, digest: &[u8; 32]) -> Result<usize, StoreError> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM semantic_journal_facts WHERE authority_digest = ?1",
        [digest.as_slice()],
        |row| row.get(0),
    )?;
    usize::try_from(count).map_err(|_| StoreError::IntegerOutOfRange {
        field: "semantic_journal.count",
        value: u64::MAX,
    })
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
) -> Result<SemanticJournalWrite, StoreError> {
    // Validate the persisted sequence before any dedupe fast path. A corrupted
    // journal must not become writable merely because the incoming delivery
    // happens to repeat an existing key.
    let (next_sequence, _) = high_water(tx, digest)?;
    if let Some(hit) = lookup_delivery(tx, digest, delivery_id)? {
        return Ok(classify_hit(hit, payload_hash));
    }
    if let Some(provider_event_id) = provider_event_id {
        if let Some(hit) = lookup_provider_event(tx, digest, provider_event_id)? {
            return Ok(classify_hit(hit, payload_hash));
        }
    }
    let retained = retained_len(tx, digest)?;
    if retained as u32 >= max_events {
        return Ok(SemanticJournalWrite::EventCapacity);
    }
    if retained.saturating_mul(2) as u32 >= max_dedupe_keys {
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

pub(crate) fn load_page(
    conn: &Connection,
    digest: &[u8; 32],
    after_sequence: i64,
    limit: u32,
) -> Result<Vec<SemanticJournalFactRow>, StoreError> {
    if limit == 0 || limit > MAX_JOURNAL_PAGE_LIMIT {
        return Err(StoreError::IntegerOutOfRange {
            field: "semantic_journal.page_limit",
            value: u64::from(limit),
        });
    }
    let capacity = usize::try_from(limit).map_err(|_| StoreError::IntegerOutOfRange {
        field: "semantic_journal.page_limit",
        value: u64::from(limit),
    })?;
    let mut stmt = conn.prepare(
        "SELECT sequence, event_id, delivery_id, provider_event_id, content_hash,
                kind, visibility, privacy_class, redaction_class, occurred_at_ms,
                ingested_at_ms, schema_version, payload
         FROM semantic_journal_facts
         WHERE authority_digest = ?1
           AND sequence > ?2
           AND sequence <= (
                SELECT COALESCE(MAX(sequence), 0)
                FROM semantic_journal_facts
                WHERE authority_digest = ?1
           )
         ORDER BY sequence ASC
         LIMIT ?3",
    )?;
    let mut rows = stmt.query(rusqlite::params![
        digest.as_slice(),
        after_sequence,
        i64::from(limit),
    ])?;
    let mut facts = Vec::with_capacity(capacity);
    while let Some(row) = rows.next()? {
        if facts.len() >= capacity {
            return Err(StoreError::IntegerOutOfRange {
                field: "semantic_journal.page_limit",
                value: u64::from(limit),
            });
        }
        facts.push(finalize_fact(map_raw_fact(row)?)?);
    }
    Ok(facts)
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

fn finalize_fact(raw: RawFact) -> Result<SemanticJournalFactRow, StoreError> {
    let event_id = exact16(raw.event_id)?;
    EventId::from_bytes(event_id).map_err(|_| StoreError::Corruption)?;
    Ok(SemanticJournalFactRow {
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
    })
}

pub(crate) fn exact16(bytes: Vec<u8>) -> Result<[u8; 16], StoreError> {
    <[u8; 16]>::try_from(bytes).map_err(|_| StoreError::Corruption)
}

pub(crate) fn exact32(bytes: Vec<u8>) -> Result<[u8; 32], StoreError> {
    <[u8; 32]>::try_from(bytes).map_err(|_| StoreError::Corruption)
}
