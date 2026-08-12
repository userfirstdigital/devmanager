use std::fmt;
use std::path::Path;
use std::time::{Duration, Instant};

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior};
use sha2::{Digest, Sha256};

use crate::domain::id::{
    AgentSessionId, EventId, PromptChainId, PromptChainLinkId, PromptHistoryId, PromptId,
    PromptVersionId, RequestId, TaskId,
};

use super::model::validate_body;
use super::search::{clear_search_index, delete_search_row, execute_search, upsert_search_row};

pub use super::search::{
    HighlightRange, PromptSearchBudget, PromptSearchCursor, PromptSearchHit, PromptSearchPage,
    PromptSearchQuery, PromptSearchSource, PromptSearchStatus, MAX_PROMPT_SEARCH_PAGE,
    MAX_PROMPT_SEARCH_QUERY_BYTES, MAX_PROMPT_SEARCH_TERMS,
};

pub const DEFAULT_HISTORY_INDEX_CAPACITY: u32 = 1_024;
pub const HISTORY_INDEX_BATCH_ROWS: u32 = 50;
pub const HISTORY_INDEX_BATCH_MS: u64 = 250;
pub(crate) const SOURCE_HISTORY: &str = "history";
pub(crate) const SOURCE_SAVED: &str = "saved";

const BUSY_TIMEOUT_MS: u64 = 5_000;
const MIN_RETENTION_DAYS: u16 = 1;
const MAX_RETENTION_DAYS: u16 = 365;
const MIN_HISTORY_ENTRIES: u32 = 100;
const MAX_HISTORY_ENTRIES: u32 = 100_000;
const DEFAULT_RETENTION_DAYS: u16 = 90;
const DEFAULT_MAX_ENTRIES: u32 = 10_000;
const MS_PER_DAY: i64 = 86_400_000;
const MAX_PROVIDER_KIND_BYTES: usize = 32;
const HISTORY_RECENT_QUERY: &[u8] = b"history:recent:v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptHistoryErrorCode {
    Storage,
    Validation,
    InvalidPolicy,
    InvalidQuery,
    QueryTooLong,
    PageTooLarge,
    ProvenanceMismatch,
    Cancelled,
    Conflict,
    ProviderInputUnavailable,
    Unconfirmed,
    LineageQuarantine,
    IndexUnscheduled,
}

pub struct PromptHistoryError {
    code: PromptHistoryErrorCode,
}

impl PromptHistoryError {
    pub fn code(&self) -> PromptHistoryErrorCode {
        self.code
    }

    pub(crate) fn from_code(code: PromptHistoryErrorCode) -> Self {
        Self { code }
    }
}

impl fmt::Debug for PromptHistoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "HistoryError({:?})", self.code)
    }
}

impl fmt::Display for PromptHistoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = match self.code {
            PromptHistoryErrorCode::Storage => "storage",
            PromptHistoryErrorCode::Validation => "validation",
            PromptHistoryErrorCode::InvalidPolicy => "invalid-policy",
            PromptHistoryErrorCode::InvalidQuery => "invalid-query",
            PromptHistoryErrorCode::QueryTooLong => "query-too-long",
            PromptHistoryErrorCode::PageTooLarge => "page-too-large",
            PromptHistoryErrorCode::ProvenanceMismatch => "provenance-mismatch",
            PromptHistoryErrorCode::Cancelled => "cancelled",
            PromptHistoryErrorCode::Conflict => "conflict",
            PromptHistoryErrorCode::ProviderInputUnavailable => "provider-input-unavailable",
            PromptHistoryErrorCode::Unconfirmed => "unconfirmed",
            PromptHistoryErrorCode::LineageQuarantine => "lineage-quarantine",
            PromptHistoryErrorCode::IndexUnscheduled => "index-unscheduled",
        };
        write!(f, "history error: {label}")
    }
}

impl std::error::Error for PromptHistoryError {}

impl From<rusqlite::Error> for PromptHistoryError {
    fn from(_: rusqlite::Error) -> Self {
        Self::from_code(PromptHistoryErrorCode::Storage)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PromptHistoryPolicy {
    pub enabled: bool,
    pub retention_days: u16,
    pub max_entries: u32,
}

impl Default for PromptHistoryPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            retention_days: DEFAULT_RETENTION_DAYS,
            max_entries: DEFAULT_MAX_ENTRIES,
        }
    }
}

impl PromptHistoryPolicy {
    fn validate(self) -> Result<Self, PromptHistoryError> {
        if !(MIN_RETENTION_DAYS..=MAX_RETENTION_DAYS).contains(&self.retention_days)
            || !(MIN_HISTORY_ENTRIES..=MAX_HISTORY_ENTRIES).contains(&self.max_entries)
        {
            return Err(PromptHistoryError::from_code(
                PromptHistoryErrorCode::InvalidPolicy,
            ));
        }
        Ok(self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PromptHistoryProvenance {
    pub prompt_id: Option<PromptId>,
    pub prompt_version_id: Option<PromptVersionId>,
    pub chain_id: Option<PromptChainId>,
    pub chain_link_id: Option<PromptChainLinkId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordedPromptHistory {
    pub history_id: PromptHistoryId,
    pub request_id: RequestId,
}

#[derive(Clone, PartialEq, Eq)]
pub struct PromptHistoryEntry {
    pub history_id: PromptHistoryId,
    pub request_id: RequestId,
    pub submitted_event_id: EventId,
    pub task_id: TaskId,
    pub agent_session_id: AgentSessionId,
    pub provider_kind: String,
    pub body: String,
    pub submitted_at_ms: i64,
    pub prompt_id: Option<PromptId>,
    pub prompt_version_id: Option<PromptVersionId>,
    pub chain_id: Option<PromptChainId>,
    pub chain_link_id: Option<PromptChainLinkId>,
}

impl fmt::Debug for PromptHistoryEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HistoryEntry")
            .field("history_id", &self.history_id)
            .field("request_id", &self.request_id)
            .field("submitted_event_id", &self.submitted_event_id)
            .field("task_id", &self.task_id)
            .field("submitted_at_ms", &self.submitted_at_ms)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryCursor {
    submitted_at_ms: i64,
    history_id: PromptHistoryId,
    source: PromptSearchSource,
    query_sha256: [u8; 32],
    epoch: i64,
    high_water: i64,
    schema_version: i64,
}

impl HistoryCursor {
    #[cfg(test)]
    pub(crate) fn with_bind(
        &self,
        source: PromptSearchSource,
        query_sha256: [u8; 32],
        epoch: i64,
        high_water: i64,
        schema_version: i64,
    ) -> Self {
        Self {
            submitted_at_ms: self.submitted_at_ms,
            history_id: self.history_id,
            source,
            query_sha256,
            epoch,
            high_water,
            schema_version,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryPage {
    pub entries: Vec<PromptHistoryEntry>,
    pub next: Option<HistoryCursor>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PolicyPreview {
    revision: u64,
}

impl PolicyPreview {
    pub fn confirmation(&self) -> PolicyConfirmation {
        PolicyConfirmation {
            revision: self.revision,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PolicyConfirmation {
    revision: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MaintenancePreview {
    pub expired: u64,
    pub overflow: u64,
    revision: u64,
}

impl MaintenancePreview {
    pub fn confirmation(&self) -> MaintenanceConfirmation {
        MaintenanceConfirmation {
            revision: self.revision,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MaintenanceConfirmation {
    revision: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MaintenanceProgress {
    pub removed: u64,
    pub done: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexProgress {
    pub processed: u32,
    pub remaining: u64,
}

/// Opaque proof that a provider-input settlement accepted a user prompt.
///
/// Production callers must obtain this from a current, provider-owned durable
/// settlement via [`ProviderDurableSettlement`]. Transcript, cwd, and
/// timestamp inference are never accepted. Providers that lack durable
/// settlement support return [`PromptHistoryErrorCode::ProviderInputUnavailable`].
pub struct ValidatedDeliveredInputProof {
    history_id: PromptHistoryId,
    request_id: RequestId,
    submitted_event_id: EventId,
    task_id: TaskId,
    agent_session_id: AgentSessionId,
    provider_kind: String,
    body: String,
    accepted_at_ms: i64,
    provenance: PromptHistoryProvenance,
}

impl fmt::Debug for ValidatedDeliveredInputProof {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeliveredProof")
            .field("history_id", &self.history_id)
            .field("request_id", &self.request_id)
            .field("submitted_event_id", &self.submitted_event_id)
            .finish()
    }
}

impl ValidatedDeliveredInputProof {
    /// Construct a proof only from a current provider-owned durable settlement.
    pub fn from_provider_durable_settlement(
        settlement: &dyn ProviderDurableSettlement,
    ) -> Result<Self, PromptHistoryError> {
        let owned = settlement.current_durable_settlement()?;
        if owned.settlement_generation == 0 {
            return Err(PromptHistoryError::from_code(
                PromptHistoryErrorCode::Validation,
            ));
        }
        let proof = Self {
            history_id: owned.history_id,
            request_id: owned.request_id,
            submitted_event_id: owned.submitted_event_id,
            task_id: owned.task_id,
            agent_session_id: owned.agent_session_id,
            provider_kind: owned.provider_kind,
            body: owned.body,
            accepted_at_ms: owned.accepted_at_ms,
            provenance: owned.provenance,
        };
        validate_proof(&proof)?;
        Ok(proof)
    }

    /// Compatibility alias for callers whose provider has no durable
    /// settlement boundary. This remains fail-closed.
    pub fn from_provider_input_settlement() -> Result<Self, PromptHistoryError> {
        Err(PromptHistoryError::from_code(
            PromptHistoryErrorCode::ProviderInputUnavailable,
        ))
    }
}

/// Provider-owned durable settlement fact used to construct delivered-history
/// proofs. The generation is supplied by the provider participant and prevents
/// stale or inferred delivery claims from becoming history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderOwnedDurableSettlement {
    pub history_id: PromptHistoryId,
    pub request_id: RequestId,
    pub submitted_event_id: EventId,
    pub task_id: TaskId,
    pub agent_session_id: AgentSessionId,
    pub provider_kind: String,
    pub body: String,
    pub accepted_at_ms: i64,
    pub provenance: PromptHistoryProvenance,
    pub settlement_generation: u64,
}

/// Typed boundary for production-capable delivered-history settlement.
pub trait ProviderDurableSettlement {
    fn current_durable_settlement(
        &self,
    ) -> Result<ProviderOwnedDurableSettlement, PromptHistoryError>;
}

/// Marker for providers that do not expose durable settlement.
#[derive(Debug, Default, Clone, Copy)]
pub struct UnsupportedProviderDurableSettlement;

impl ProviderDurableSettlement for UnsupportedProviderDurableSettlement {
    fn current_durable_settlement(
        &self,
    ) -> Result<ProviderOwnedDurableSettlement, PromptHistoryError> {
        Err(PromptHistoryError::from_code(
            PromptHistoryErrorCode::ProviderInputUnavailable,
        ))
    }
}

/// Adapts a provider settlement receipt onto the existing prompt-history
/// transaction and validation path. Application is atomic in the caller's
/// IMMEDIATE transaction; unsupported receipts do not insert history.
pub struct DurableProviderDeliveryAdapter<'a> {
    settlement: &'a dyn ProviderDurableSettlement,
}

impl<'a> DurableProviderDeliveryAdapter<'a> {
    pub fn new(settlement: &'a dyn ProviderDurableSettlement) -> Self {
        Self { settlement }
    }

    pub fn commit(
        &self,
        store: &mut PromptHistoryStore,
    ) -> Result<Option<RecordedPromptHistory>, PromptHistoryError> {
        store.commit_provider_durable_settlement(self.settlement)
    }
}

pub struct PromptHistoryStore {
    conn: Connection,
    fail_next_index: bool,
    index_scheduler_claimed: bool,
}

impl fmt::Debug for PromptHistoryStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PromptHistoryStore")
    }
}

impl PromptHistoryStore {
    pub fn open(path: &Path) -> Result<Self, PromptHistoryError> {
        crate::kernel::KernelStore::open(path)
            .map_err(|_| PromptHistoryError::from_code(PromptHistoryErrorCode::Storage))?;
        super::store::PromptStore::open(path).map_err(|_| {
            PromptHistoryError::from_code(PromptHistoryErrorCode::LineageQuarantine)
        })?;
        let conn = configure_connection(path)?;
        Ok(Self {
            conn,
            fail_next_index: false,
            index_scheduler_claimed: false,
        })
    }

    pub fn claim_index_scheduler(&mut self) {
        self.index_scheduler_claimed = true;
    }

    fn require_index_scheduler(&self) -> Result<(), PromptHistoryError> {
        if self.index_scheduler_claimed {
            return Ok(());
        }
        if self.is_search_dirty()? || self.is_search_overflow()? {
            return Err(PromptHistoryError::from_code(
                PromptHistoryErrorCode::IndexUnscheduled,
            ));
        }
        Ok(())
    }

    /// Apply a delivered-input fact into the caller's existing settlement
    /// transaction. This does not open a second transaction.
    pub fn apply_delivered_in_tx(
        tx: &Transaction<'_>,
        proof: &ValidatedDeliveredInputProof,
    ) -> Result<Option<RecordedPromptHistory>, PromptHistoryError> {
        validate_proof(proof)?;
        let policy = load_policy(tx)?;
        if !policy.enabled {
            return Ok(None);
        }
        let body_sha256: [u8; 32] = Sha256::digest(proof.body.as_bytes()).into();
        if let Some(existing) = load_identity_collision(tx, proof)? {
            verify_stored_body_hash(&existing.entry.body, &existing.body_sha256)?;
            if identity_matches(&existing, proof, &body_sha256) {
                mark_pending(tx, SOURCE_HISTORY, existing.entry.history_id.as_bytes())?;
                return Ok(Some(RecordedPromptHistory {
                    history_id: existing.entry.history_id,
                    request_id: existing.entry.request_id,
                }));
            }
            return Err(PromptHistoryError::from_code(
                PromptHistoryErrorCode::Conflict,
            ));
        }
        validate_provenance(tx, &proof.provenance)?;
        reserve_pending_or_overflow(tx)?;
        tx.execute(
            "INSERT INTO prompt_history(
                prompt_history_id, request_id, submitted_event_id, task_id,
                agent_session_id, provider_kind, body, body_sha256, submitted_at_ms,
                prompt_id, prompt_version_id, chain_id, chain_link_id
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            rusqlite::params![
                proof.history_id.as_bytes().as_slice(),
                proof.request_id.as_bytes().as_slice(),
                proof.submitted_event_id.as_bytes().as_slice(),
                proof.task_id.as_bytes().as_slice(),
                proof.agent_session_id.as_bytes().as_slice(),
                proof.provider_kind,
                proof.body,
                body_sha256.as_slice(),
                proof.accepted_at_ms,
                proof.provenance.prompt_id.map(|id| id.as_bytes().to_vec()),
                proof
                    .provenance
                    .prompt_version_id
                    .map(|id| id.as_bytes().to_vec()),
                proof.provenance.chain_id.map(|id| id.as_bytes().to_vec()),
                proof
                    .provenance
                    .chain_link_id
                    .map(|id| id.as_bytes().to_vec()),
            ],
        )?;
        mark_pending(tx, SOURCE_HISTORY, proof.history_id.as_bytes())?;
        Ok(Some(RecordedPromptHistory {
            history_id: proof.history_id,
            request_id: proof.request_id,
        }))
    }

    /// Apply one provider-owned durable settlement within the caller's
    /// existing IMMEDIATE transaction. The provider fact is resolved and
    /// validated before any history row is written.
    pub fn apply_provider_durable_settlement_in_tx(
        tx: &Transaction<'_>,
        settlement: &dyn ProviderDurableSettlement,
    ) -> Result<Option<RecordedPromptHistory>, PromptHistoryError> {
        let proof = ValidatedDeliveredInputProof::from_provider_durable_settlement(settlement)?;
        Self::apply_delivered_in_tx(tx, &proof)
    }

    pub fn recent(
        &self,
        limit: usize,
        after: Option<HistoryCursor>,
    ) -> Result<HistoryPage, PromptHistoryError> {
        if limit == 0 || limit > MAX_PROMPT_SEARCH_PAGE {
            return Err(PromptHistoryError::from_code(
                PromptHistoryErrorCode::PageTooLarge,
            ));
        }
        let (epoch, high_water) = load_index_seqs(&self.conn)?;
        let schema_version = cursor_schema_version(&self.conn)?;
        let query_sha256 = history_recent_query_sha256();
        if let Some(cursor) = after.as_ref() {
            if cursor.source != PromptSearchSource::History
                || cursor.query_sha256 != query_sha256
                || cursor.epoch != epoch
                || cursor.high_water != high_water
                || cursor.schema_version != schema_version
            {
                return Err(PromptHistoryError::from_code(
                    PromptHistoryErrorCode::InvalidQuery,
                ));
            }
        }
        let fetch_limit =
            i64::from(u32::try_from(limit).map_err(|_| {
                PromptHistoryError::from_code(PromptHistoryErrorCode::PageTooLarge)
            })?) + 1;
        let cursor_ms = after.as_ref().map(|cursor| cursor.submitted_at_ms);
        let cursor_id = after
            .as_ref()
            .map(|cursor| cursor.history_id.as_bytes().to_vec());
        let mut stmt = self.conn.prepare(
            "SELECT prompt_history_id, request_id, submitted_event_id, task_id,
                    agent_session_id, provider_kind, body, submitted_at_ms,
                    prompt_id, prompt_version_id, chain_id, chain_link_id, body_sha256
             FROM prompt_history
             WHERE ?1 IS NULL
                OR submitted_at_ms < ?1
                OR (submitted_at_ms = ?1 AND prompt_history_id < ?2)
             ORDER BY submitted_at_ms DESC, prompt_history_id DESC
             LIMIT ?3",
        )?;
        let rows = stmt.query_map(
            rusqlite::params![cursor_ms, cursor_id, fetch_limit],
            read_history_entry_with_hash,
        )?;
        let mut entries = Vec::new();
        for row in rows {
            let (entry, stored_hash) = row?;
            if entry.submitted_at_ms < 0 {
                return Err(PromptHistoryError::from_code(
                    PromptHistoryErrorCode::Storage,
                ));
            }
            verify_stored_body_hash(&entry.body, &stored_hash)?;
            entries.push(entry);
        }
        let next = if entries.len() > limit {
            entries.pop();
            entries.last().map(|entry| HistoryCursor {
                submitted_at_ms: entry.submitted_at_ms,
                history_id: entry.history_id,
                source: PromptSearchSource::History,
                query_sha256,
                epoch,
                high_water,
                schema_version,
            })
        } else {
            None
        };
        Ok(HistoryPage { entries, next })
    }

    pub fn search(
        &self,
        query: &PromptSearchQuery,
        budget: PromptSearchBudget<'_>,
    ) -> Result<PromptSearchPage, PromptHistoryError> {
        self.require_index_scheduler()?;
        execute_search(&self.conn, query, budget)
    }

    pub fn preview_policy(&self) -> Result<PolicyPreview, PromptHistoryError> {
        Ok(PolicyPreview {
            revision: load_revision(&self.conn)?,
        })
    }

    pub fn set_policy(
        &mut self,
        policy: PromptHistoryPolicy,
        confirmation: PolicyConfirmation,
    ) -> Result<(), PromptHistoryError> {
        let policy = policy.validate()?;
        let changed = self.conn.execute(
            "UPDATE prompt_history_policy
             SET enabled = ?1, retention_days = ?2, max_entries = ?3,
                 revision = revision + 1
             WHERE singleton_key = 1 AND revision = ?4",
            rusqlite::params![
                i64::from(policy.enabled),
                i64::from(policy.retention_days),
                i64::from(policy.max_entries),
                i64::try_from(confirmation.revision).map_err(|_| PromptHistoryError::from_code(
                    PromptHistoryErrorCode::Unconfirmed
                ))?,
            ],
        )?;
        if changed != 1 {
            return Err(PromptHistoryError::from_code(
                PromptHistoryErrorCode::Unconfirmed,
            ));
        }
        Ok(())
    }

    pub fn preview_retention(&self, now_ms: i64) -> Result<MaintenancePreview, PromptHistoryError> {
        if now_ms < 0 {
            return Err(PromptHistoryError::from_code(
                PromptHistoryErrorCode::Validation,
            ));
        }
        let policy = load_policy(&self.conn)?;
        let cutoff = now_ms.saturating_sub(i64::from(policy.retention_days) * MS_PER_DAY);
        let expired: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM prompt_history WHERE submitted_at_ms < ?1",
            [cutoff],
            |row| row.get(0),
        )?;
        let kept_after_expiry: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM prompt_history WHERE submitted_at_ms >= ?1",
            [cutoff],
            |row| row.get(0),
        )?;
        let overflow = u64::try_from(kept_after_expiry)
            .unwrap_or(0)
            .saturating_sub(u64::from(policy.max_entries));
        Ok(MaintenancePreview {
            expired: u64::try_from(expired).unwrap_or(0),
            overflow,
            revision: load_revision(&self.conn)?,
        })
    }

    pub fn apply_retention(
        &mut self,
        now_ms: i64,
        confirmation: MaintenanceConfirmation,
        budget: PromptSearchBudget<'_>,
    ) -> Result<MaintenanceProgress, PromptHistoryError> {
        if now_ms < 0 {
            return Err(PromptHistoryError::from_code(
                PromptHistoryErrorCode::Validation,
            ));
        }
        budget.check(0, 0)?;
        confirm_revision(&self.conn, confirmation.revision)?;
        let policy = load_policy(&self.conn)?;
        let cutoff = now_ms.saturating_sub(i64::from(policy.retention_days) * MS_PER_DAY);
        let deadline = Instant::now() + Duration::from_millis(HISTORY_INDEX_BATCH_MS);
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut removed = 0_u64;
        let mut remaining_slots = HISTORY_INDEX_BATCH_ROWS;
        let mut work_used = 0_usize;
        while remaining_slots > 0 {
            budget.check(work_used, 0)?;
            work_used = work_used.saturating_add(1);
            if Instant::now() >= deadline {
                break;
            }
            let deleted = delete_history_batch(
                &tx,
                "SELECT prompt_history_id FROM prompt_history
                 WHERE submitted_at_ms < ?1
                 ORDER BY submitted_at_ms ASC, prompt_history_id ASC
                 LIMIT ?2",
                rusqlite::params![cutoff, i64::from(remaining_slots)],
                &budget,
                &mut work_used,
            )?;
            if deleted == 0 {
                break;
            }
            removed += u64::from(deleted);
            remaining_slots = remaining_slots.saturating_sub(deleted);
        }
        if remaining_slots > 0 {
            let count: i64 =
                tx.query_row("SELECT COUNT(*) FROM prompt_history", [], |row| row.get(0))?;
            let overflow = u64::try_from(count)
                .unwrap_or(0)
                .saturating_sub(u64::from(policy.max_entries));
            let overflow_batch =
                u32::try_from(overflow.min(u64::from(remaining_slots))).unwrap_or(0);
            if overflow_batch > 0 && Instant::now() < deadline {
                let deleted = delete_history_batch(
                    &tx,
                    "SELECT prompt_history_id FROM prompt_history
                     ORDER BY submitted_at_ms ASC, prompt_history_id ASC
                     LIMIT ?1",
                    rusqlite::params![i64::from(overflow_batch)],
                    &budget,
                    &mut work_used,
                )?;
                removed += u64::from(deleted);
            }
        }
        tx.commit()?;
        let preview = self.preview_retention(now_ms)?;
        Ok(MaintenanceProgress {
            removed,
            done: preview.expired == 0 && preview.overflow == 0,
        })
    }

    pub fn preview_clear(&self) -> Result<MaintenancePreview, PromptHistoryError> {
        let total: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM prompt_history", [], |row| row.get(0))?;
        Ok(MaintenancePreview {
            expired: 0,
            overflow: u64::try_from(total).unwrap_or(0),
            revision: load_revision(&self.conn)?,
        })
    }

    pub fn apply_clear(
        &mut self,
        confirmation: MaintenanceConfirmation,
        budget: PromptSearchBudget<'_>,
    ) -> Result<MaintenanceProgress, PromptHistoryError> {
        budget.check(0, 0)?;
        confirm_revision(&self.conn, confirmation.revision)?;
        let deadline = Instant::now() + Duration::from_millis(HISTORY_INDEX_BATCH_MS);
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut removed = 0_u64;
        let mut remaining_slots = HISTORY_INDEX_BATCH_ROWS;
        let mut work_used = 0_usize;
        while remaining_slots > 0 {
            if !budget.check(work_used, 0)? {
                break;
            }
            work_used = work_used.saturating_add(1);
            if Instant::now() >= deadline {
                break;
            }
            let deleted = delete_history_batch(
                &tx,
                "SELECT prompt_history_id FROM prompt_history
                 ORDER BY submitted_at_ms ASC, prompt_history_id ASC
                 LIMIT ?1",
                rusqlite::params![i64::from(remaining_slots)],
                &budget,
                &mut work_used,
            )?;
            if deleted == 0 {
                break;
            }
            removed += u64::from(deleted);
            remaining_slots = remaining_slots.saturating_sub(deleted);
        }
        tx.commit()?;
        let remaining: i64 =
            self.conn
                .query_row("SELECT COUNT(*) FROM prompt_history", [], |row| row.get(0))?;
        Ok(MaintenanceProgress {
            removed,
            done: remaining == 0,
        })
    }

    pub fn drain_index(
        &mut self,
        budget: PromptSearchBudget<'_>,
    ) -> Result<IndexProgress, PromptHistoryError> {
        self.claim_index_scheduler();
        budget.check(0, 0)?;
        if self.fail_next_index {
            self.fail_next_index = false;
            mark_dirty(&self.conn)?;
            return Err(PromptHistoryError::from_code(
                PromptHistoryErrorCode::Storage,
            ));
        }
        let deadline = Instant::now() + Duration::from_millis(HISTORY_INDEX_BATCH_MS);
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let pending: Vec<(String, Vec<u8>, i64)> = {
            let mut stmt = tx.prepare(
                "SELECT source_kind, source_id, enqueue_seq FROM prompt_search_pending
                 ORDER BY source_kind, source_id
                 LIMIT ?1",
            )?;
            let rows = stmt.query_map([i64::from(HISTORY_INDEX_BATCH_ROWS)], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        let mut processed = 0_u32;
        for (source_kind, source_id, enqueue_seq) in pending {
            if !budget.check(processed as usize, 0)? {
                break;
            }
            if Instant::now() >= deadline {
                break;
            }
            if let Err(error) = apply_index_work(&tx, &source_kind, &source_id) {
                mark_dirty(&tx)?;
                tx.commit()?;
                return Err(error);
            }
            tx.execute(
                "DELETE FROM prompt_search_pending
                 WHERE source_kind = ?1 AND source_id = ?2 AND enqueue_seq = ?3",
                rusqlite::params![source_kind, source_id, enqueue_seq],
            )?;
            processed += 1;
        }
        maybe_clear_dirty(&tx)?;
        let remaining: i64 =
            tx.query_row("SELECT COUNT(*) FROM prompt_search_pending", [], |row| {
                row.get(0)
            })?;
        tx.commit()?;
        Ok(IndexProgress {
            processed,
            remaining: u64::try_from(remaining).unwrap_or(0),
        })
    }

    pub fn rebuild_search(
        &mut self,
        budget: PromptSearchBudget<'_>,
    ) -> Result<IndexProgress, PromptHistoryError> {
        self.claim_index_scheduler();
        budget.check(0, 0)?;
        let deadline = Instant::now() + Duration::from_millis(HISTORY_INDEX_BATCH_MS);
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut state = load_rebuild_state(&tx)?;
        if state.phase == RebuildPhase::Idle {
            clear_search_index(&tx)?;
            capture_rebuild_high_water(&tx)?;
            mark_dirty(&tx)?;
            state.phase = RebuildPhase::History;
            state.submitted_at_ms = None;
            state.source_id = None;
            persist_rebuild_state(&tx, &state)?;
        }
        let mut processed = 0_u32;
        while processed < HISTORY_INDEX_BATCH_ROWS && Instant::now() < deadline {
            if !budget.check(processed as usize, 0)? {
                break;
            }
            match state.phase {
                RebuildPhase::Idle => break,
                RebuildPhase::History => {
                    let row = match next_history_rebuild_row(&tx, &state) {
                        Ok(Some(row)) => row,
                        Ok(None) => {
                            state.phase = RebuildPhase::Saved;
                            state.submitted_at_ms = None;
                            state.source_id = None;
                            persist_rebuild_state(&tx, &state)?;
                            continue;
                        }
                        Err(error) => {
                            mark_dirty(&tx)?;
                            tx.commit()?;
                            return Err(error);
                        }
                    };
                    upsert_search_row(&tx, SOURCE_HISTORY, &row.source_id, "", &row.body, "")?;
                    state.submitted_at_ms = Some(row.submitted_at_ms);
                    state.source_id = Some(row.source_id);
                    persist_rebuild_state(&tx, &state)?;
                    processed += 1;
                }
                RebuildPhase::Saved => {
                    let row = match next_saved_rebuild_row(&tx, &state) {
                        Ok(Some(row)) => row,
                        Ok(None) => {
                            state.phase = RebuildPhase::Pending;
                            state.submitted_at_ms = None;
                            state.source_id = None;
                            persist_rebuild_state(&tx, &state)?;
                            continue;
                        }
                        Err(error) => {
                            mark_dirty(&tx)?;
                            tx.commit()?;
                            return Err(error);
                        }
                    };
                    upsert_search_row(
                        &tx,
                        SOURCE_SAVED,
                        &row.source_id,
                        &row.title,
                        &row.body,
                        &row.tags,
                    )?;
                    state.source_id = Some(row.source_id);
                    persist_rebuild_state(&tx, &state)?;
                    processed += 1;
                }
                RebuildPhase::Pending => {
                    let Some((source_kind, source_id, enqueue_seq)) = next_pending_row(&tx)? else {
                        state.phase = RebuildPhase::Idle;
                        state.submitted_at_ms = None;
                        state.source_id = None;
                        persist_rebuild_state(&tx, &state)?;
                        break;
                    };
                    if let Err(error) = apply_index_work(&tx, &source_kind, &source_id) {
                        mark_dirty(&tx)?;
                        tx.commit()?;
                        return Err(error);
                    }
                    tx.execute(
                        "DELETE FROM prompt_search_pending
                         WHERE source_kind = ?1 AND source_id = ?2 AND enqueue_seq = ?3",
                        rusqlite::params![source_kind, source_id, enqueue_seq],
                    )?;
                    processed += 1;
                }
            }
        }
        let mut remaining = count_rebuild_remaining(&tx, &state)?;
        if remaining == 0 && state.phase == RebuildPhase::Idle {
            let history: i64 =
                tx.query_row("SELECT COUNT(*) FROM prompt_history", [], |row| row.get(0))?;
            let saved: i64 =
                tx.query_row("SELECT COUNT(*) FROM saved_prompts", [], |row| row.get(0))?;
            let indexed: i64 =
                tx.query_row("SELECT COUNT(*) FROM prompt_search", [], |row| row.get(0))?;
            let (current_seq, high_water_seq) = load_index_seqs(&tx)?;
            if indexed == history + saved && current_seq >= high_water_seq {
                tx.execute(
                    "UPDATE prompt_search_state
                     SET dirty = 0, overflow = 0, high_water_seq = current_seq
                     WHERE singleton_key = 1",
                    [],
                )?;
            } else if current_seq > high_water_seq {
                clear_search_index(&tx)?;
                capture_rebuild_high_water(&tx)?;
                state.phase = RebuildPhase::History;
                state.submitted_at_ms = None;
                state.source_id = None;
                persist_rebuild_state(&tx, &state)?;
                mark_dirty(&tx)?;
                remaining = count_rebuild_remaining(&tx, &state)?;
            } else {
                state.phase = RebuildPhase::History;
                state.submitted_at_ms = None;
                state.source_id = None;
                persist_rebuild_state(&tx, &state)?;
                mark_dirty(&tx)?;
                remaining = count_rebuild_remaining(&tx, &state)?;
            }
        } else {
            mark_dirty(&tx)?;
        }
        tx.commit()?;
        Ok(IndexProgress {
            processed,
            remaining,
        })
    }

    pub fn is_search_dirty(&self) -> Result<bool, PromptHistoryError> {
        let dirty: i64 = self.conn.query_row(
            "SELECT dirty FROM prompt_search_state WHERE singleton_key = 1",
            [],
            |row| row.get(0),
        )?;
        Ok(dirty != 0)
    }

    pub fn is_search_overflow(&self) -> Result<bool, PromptHistoryError> {
        let overflow: i64 = self.conn.query_row(
            "SELECT overflow FROM prompt_search_state WHERE singleton_key = 1",
            [],
            |row| row.get(0),
        )?;
        Ok(overflow != 0)
    }

    pub fn pending_count(&self) -> Result<u64, PromptHistoryError> {
        let count: i64 =
            self.conn
                .query_row("SELECT COUNT(*) FROM prompt_search_pending", [], |row| {
                    row.get(0)
                })?;
        u64::try_from(count)
            .map_err(|_| PromptHistoryError::from_code(PromptHistoryErrorCode::Storage))
    }

    pub fn schema_has_history_and_search(&self) -> Result<bool, PromptHistoryError> {
        let history: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM sqlite_schema WHERE type = 'table' AND name = 'prompt_history'",
            [],
            |row| row.get(0),
        )?;
        let search: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM sqlite_schema WHERE name = 'prompt_search'",
            [],
            |row| row.get(0),
        )?;
        let pending: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM sqlite_schema WHERE type = 'table' AND name = 'prompt_search_pending'",
            [],
            |row| row.get(0),
        )?;
        Ok(history == 1 && search == 1 && pending == 1)
    }

    /// Commit one provider-owned durable settlement atomically with the
    /// history row and search pending state. Providers without this boundary
    /// fail closed before SQLite's write lock is taken.
    pub fn commit_provider_durable_settlement(
        &mut self,
        settlement: &dyn ProviderDurableSettlement,
    ) -> Result<Option<RecordedPromptHistory>, PromptHistoryError> {
        let proof = ValidatedDeliveredInputProof::from_provider_durable_settlement(settlement)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let recorded = Self::apply_delivered_in_tx(&tx, &proof)?;
        tx.commit()?;
        Ok(recorded)
    }
}

#[cfg(test)]
pub(crate) mod history_testing {
    use rusqlite::{OptionalExtension, TransactionBehavior};

    use super::{
        proof_from_attempt, PromptHistoryAttemptInternal, PromptHistoryError, PromptHistoryStore,
        RecordedPromptHistory, ValidatedDeliveredInputProof,
    };

    pub enum PromptHistoryAttempt {
        AcceptedForDelivery {
            history_id: super::PromptHistoryId,
            request_id: super::RequestId,
            submitted_event_id: super::EventId,
            task_id: super::TaskId,
            agent_session_id: super::AgentSessionId,
            provider_kind: String,
            body: String,
            accepted_at_ms: i64,
            provenance: super::PromptHistoryProvenance,
        },
        RejectedDraft {
            request_id: super::RequestId,
            body: String,
        },
        Failed {
            request_id: super::RequestId,
            body: String,
        },
        Cancelled {
            request_id: super::RequestId,
            body: String,
        },
        Synthetic {
            request_id: super::RequestId,
            body: String,
        },
        ProviderInternal {
            request_id: super::RequestId,
            body: String,
        },
        RawTerminal {
            request_id: super::RequestId,
            body: String,
        },
        Secret {
            request_id: super::RequestId,
            body: String,
        },
    }

    pub fn commit_delivered(
        store: &mut PromptHistoryStore,
        attempt: PromptHistoryAttempt,
    ) -> Result<Option<RecordedPromptHistory>, PromptHistoryError> {
        let Some(proof) = proof_from_attempt(attempt)? else {
            return Ok(None);
        };
        let tx = store
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let recorded = PromptHistoryStore::apply_delivered_in_tx(&tx, &proof)?;
        tx.commit()?;
        Ok(recorded)
    }

    pub fn apply_then_rollback(
        store: &mut PromptHistoryStore,
        attempt: PromptHistoryAttempt,
    ) -> Result<(), PromptHistoryError> {
        let Some(proof) = proof_from_attempt(attempt)? else {
            return Ok(());
        };
        let tx = store
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        PromptHistoryStore::apply_delivered_in_tx(&tx, &proof)?;
        tx.rollback()?;
        Ok(())
    }

    pub fn force_next_index_error(store: &mut PromptHistoryStore) {
        store.fail_next_index = true;
    }

    pub fn seed_pending_rows(
        store: &mut PromptHistoryStore,
        count: u32,
    ) -> Result<(), PromptHistoryError> {
        let tx = store
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        for index in 0..count {
            let mut source_id = [0_u8; 16];
            source_id[0] = 0x01;
            source_id[6] = 0x70;
            source_id[8] = 0x80;
            source_id[12..16].copy_from_slice(&index.to_be_bytes());
            tx.execute(
                "INSERT OR IGNORE INTO prompt_search_pending(source_kind, source_id, enqueue_seq)
                 VALUES ('history', ?1, ?2)",
                rusqlite::params![source_id.as_slice(), i64::from(index) + 1],
            )?;
        }
        tx.execute(
            "UPDATE prompt_search_state SET dirty = 1 WHERE singleton_key = 1",
            [],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn pending_enqueue_seq(
        store: &PromptHistoryStore,
        prompt_id: super::PromptId,
    ) -> Result<Option<i64>, PromptHistoryError> {
        store
            .conn
            .query_row(
                "SELECT enqueue_seq FROM prompt_search_pending
                 WHERE source_kind = 'saved' AND source_id = ?1",
                [prompt_id.as_bytes().as_slice()],
                |row| row.get(0),
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn index_seqs(store: &PromptHistoryStore) -> Result<(i64, i64), PromptHistoryError> {
        store
            .conn
            .query_row(
                "SELECT current_seq, high_water_seq
                 FROM prompt_search_state WHERE singleton_key = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(Into::into)
    }

    pub fn schema_version(store: &PromptHistoryStore) -> Result<i64, PromptHistoryError> {
        super::cursor_schema_version(&store.conn)
    }

    pub fn set_high_water_seq(
        store: &PromptHistoryStore,
        high_water: i64,
    ) -> Result<(), PromptHistoryError> {
        store.conn.execute(
            "UPDATE prompt_search_state SET high_water_seq = ?1 WHERE singleton_key = 1",
            [high_water],
        )?;
        Ok(())
    }

    pub fn debug_proof(body: &str) -> ValidatedDeliveredInputProof {
        ValidatedDeliveredInputProof {
            history_id: super::PromptHistoryId::from_bytes([
                0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x10,
                0x00, 0x01,
            ])
            .expect("history id"),
            request_id: super::RequestId::from_bytes([
                0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x11,
                0x00, 0x01,
            ])
            .expect("request id"),
            submitted_event_id: super::EventId::from_bytes([
                0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x12,
                0x00, 0x01,
            ])
            .expect("event id"),
            task_id: super::TaskId::from_bytes([
                0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x13,
                0x00, 0x01,
            ])
            .expect("task id"),
            agent_session_id: super::AgentSessionId::from_bytes([
                0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x14,
                0x00, 0x01,
            ])
            .expect("session id"),
            provider_kind: "claude".into(),
            body: body.to_string(),
            accepted_at_ms: 1_000,
            provenance: super::PromptHistoryProvenance::default(),
        }
    }

    impl From<PromptHistoryAttempt> for PromptHistoryAttemptInternal {
        fn from(value: PromptHistoryAttempt) -> Self {
            match value {
                PromptHistoryAttempt::AcceptedForDelivery {
                    history_id,
                    request_id,
                    submitted_event_id,
                    task_id,
                    agent_session_id,
                    provider_kind,
                    body,
                    accepted_at_ms,
                    provenance,
                } => Self::AcceptedForDelivery {
                    history_id,
                    request_id,
                    submitted_event_id,
                    task_id,
                    agent_session_id,
                    provider_kind,
                    body,
                    accepted_at_ms,
                    provenance,
                },
                PromptHistoryAttempt::RejectedDraft { request_id, body } => {
                    Self::RejectedDraft { request_id, body }
                }
                PromptHistoryAttempt::Failed { request_id, body } => {
                    Self::Failed { request_id, body }
                }
                PromptHistoryAttempt::Cancelled { request_id, body } => {
                    Self::Cancelled { request_id, body }
                }
                PromptHistoryAttempt::Synthetic { request_id, body } => {
                    Self::Synthetic { request_id, body }
                }
                PromptHistoryAttempt::ProviderInternal { request_id, body } => {
                    Self::ProviderInternal { request_id, body }
                }
                PromptHistoryAttempt::RawTerminal { request_id, body } => {
                    Self::RawTerminal { request_id, body }
                }
                PromptHistoryAttempt::Secret { request_id, body } => {
                    Self::Secret { request_id, body }
                }
            }
        }
    }
}

#[cfg(test)]
#[allow(dead_code)]
enum PromptHistoryAttemptInternal {
    AcceptedForDelivery {
        history_id: PromptHistoryId,
        request_id: RequestId,
        submitted_event_id: EventId,
        task_id: TaskId,
        agent_session_id: AgentSessionId,
        provider_kind: String,
        body: String,
        accepted_at_ms: i64,
        provenance: PromptHistoryProvenance,
    },
    RejectedDraft {
        request_id: RequestId,
        body: String,
    },
    Failed {
        request_id: RequestId,
        body: String,
    },
    Cancelled {
        request_id: RequestId,
        body: String,
    },
    Synthetic {
        request_id: RequestId,
        body: String,
    },
    ProviderInternal {
        request_id: RequestId,
        body: String,
    },
    RawTerminal {
        request_id: RequestId,
        body: String,
    },
    Secret {
        request_id: RequestId,
        body: String,
    },
}

#[cfg(test)]
fn proof_from_attempt(
    attempt: history_testing::PromptHistoryAttempt,
) -> Result<Option<ValidatedDeliveredInputProof>, PromptHistoryError> {
    match PromptHistoryAttemptInternal::from(attempt) {
        PromptHistoryAttemptInternal::AcceptedForDelivery {
            history_id,
            request_id,
            submitted_event_id,
            task_id,
            agent_session_id,
            provider_kind,
            body,
            accepted_at_ms,
            provenance,
        } => {
            let proof = ValidatedDeliveredInputProof {
                history_id,
                request_id,
                submitted_event_id,
                task_id,
                agent_session_id,
                provider_kind,
                body,
                accepted_at_ms,
                provenance,
            };
            validate_proof(&proof)?;
            Ok(Some(proof))
        }
        PromptHistoryAttemptInternal::RejectedDraft { .. }
        | PromptHistoryAttemptInternal::Failed { .. }
        | PromptHistoryAttemptInternal::Cancelled { .. }
        | PromptHistoryAttemptInternal::Synthetic { .. }
        | PromptHistoryAttemptInternal::ProviderInternal { .. }
        | PromptHistoryAttemptInternal::RawTerminal { .. }
        | PromptHistoryAttemptInternal::Secret { .. } => Ok(None),
    }
}

fn validate_proof(proof: &ValidatedDeliveredInputProof) -> Result<(), PromptHistoryError> {
    if proof.accepted_at_ms < 0 {
        return Err(PromptHistoryError::from_code(
            PromptHistoryErrorCode::Validation,
        ));
    }
    validate_provider_kind(&proof.provider_kind)?;
    validate_body(&proof.body)
        .map_err(|_| PromptHistoryError::from_code(PromptHistoryErrorCode::Validation))?;
    Ok(())
}

fn validate_provider_kind(kind: &str) -> Result<(), PromptHistoryError> {
    if kind.is_empty()
        || kind.len() > MAX_PROVIDER_KIND_BYTES
        || !kind
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err(PromptHistoryError::from_code(
            PromptHistoryErrorCode::Validation,
        ));
    }
    Ok(())
}

fn validate_provenance(
    conn: &Connection,
    provenance: &PromptHistoryProvenance,
) -> Result<(), PromptHistoryError> {
    match (
        provenance.prompt_id,
        provenance.prompt_version_id,
        provenance.chain_id,
        provenance.chain_link_id,
    ) {
        (None, None, None, None) => Ok(()),
        (Some(prompt_id), Some(version_id), None, None) => {
            let matches: Option<i64> = conn
                .query_row(
                    "SELECT 1 FROM prompt_versions
                     WHERE prompt_id = ?1 AND prompt_version_id = ?2",
                    rusqlite::params![
                        prompt_id.as_bytes().as_slice(),
                        version_id.as_bytes().as_slice()
                    ],
                    |row| row.get(0),
                )
                .optional()?;
            if matches.is_some() {
                Ok(())
            } else {
                Err(PromptHistoryError::from_code(
                    PromptHistoryErrorCode::ProvenanceMismatch,
                ))
            }
        }
        (Some(prompt_id), Some(version_id), Some(chain_id), Some(link_id)) => {
            let matches: Option<i64> = conn
                .query_row(
                    "SELECT 1 FROM prompt_chain_links
                     WHERE link_id = ?1 AND chain_id = ?2
                       AND prompt_id = ?3 AND prompt_version_id = ?4",
                    rusqlite::params![
                        link_id.as_bytes().as_slice(),
                        chain_id.as_bytes().as_slice(),
                        prompt_id.as_bytes().as_slice(),
                        version_id.as_bytes().as_slice(),
                    ],
                    |row| row.get(0),
                )
                .optional()?;
            if matches.is_some() {
                Ok(())
            } else {
                Err(PromptHistoryError::from_code(
                    PromptHistoryErrorCode::ProvenanceMismatch,
                ))
            }
        }
        _ => Err(PromptHistoryError::from_code(
            PromptHistoryErrorCode::ProvenanceMismatch,
        )),
    }
}

struct IdentityRow {
    entry: PromptHistoryEntry,
    body_sha256: [u8; 32],
}

fn load_identity_collision(
    tx: &Transaction<'_>,
    proof: &ValidatedDeliveredInputProof,
) -> Result<Option<IdentityRow>, PromptHistoryError> {
    let mut stmt = tx.prepare(
        "SELECT prompt_history_id, request_id, submitted_event_id, task_id,
                agent_session_id, provider_kind, body, submitted_at_ms,
                prompt_id, prompt_version_id, chain_id, chain_link_id, body_sha256
         FROM prompt_history
         WHERE prompt_history_id = ?1 OR submitted_event_id = ?2 OR request_id = ?3",
    )?;
    let mut rows = stmt.query(rusqlite::params![
        proof.history_id.as_bytes().as_slice(),
        proof.submitted_event_id.as_bytes().as_slice(),
        proof.request_id.as_bytes().as_slice()
    ])?;
    let Some(first) = rows.next()? else {
        return Ok(None);
    };
    let entry = read_history_entry(first)?;
    let digest: Vec<u8> = first.get(12)?;
    let body_sha256: [u8; 32] = digest
        .as_slice()
        .try_into()
        .map_err(|_| PromptHistoryError::from_code(PromptHistoryErrorCode::Storage))?;
    if rows.next()?.is_some() {
        return Err(PromptHistoryError::from_code(
            PromptHistoryErrorCode::Storage,
        ));
    }
    Ok(Some(IdentityRow { entry, body_sha256 }))
}

fn identity_matches(
    existing: &IdentityRow,
    proof: &ValidatedDeliveredInputProof,
    recomputed_hash: &[u8; 32],
) -> bool {
    existing.entry.history_id == proof.history_id
        && existing.entry.request_id == proof.request_id
        && existing.entry.submitted_event_id == proof.submitted_event_id
        && existing.entry.task_id == proof.task_id
        && existing.entry.agent_session_id == proof.agent_session_id
        && existing.entry.provider_kind == proof.provider_kind
        && existing.entry.submitted_at_ms == proof.accepted_at_ms
        && existing.entry.body.as_bytes() == proof.body.as_bytes()
        && existing.body_sha256 == *recomputed_hash
        && Sha256::digest(existing.entry.body.as_bytes()).as_slice() == recomputed_hash.as_slice()
        && existing.entry.prompt_id == proof.provenance.prompt_id
        && existing.entry.prompt_version_id == proof.provenance.prompt_version_id
        && existing.entry.chain_id == proof.provenance.chain_id
        && existing.entry.chain_link_id == proof.provenance.chain_link_id
}

fn read_history_entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<PromptHistoryEntry> {
    Ok(PromptHistoryEntry {
        history_id: blob_to_id_row(row, 0)?,
        request_id: blob_to_id_row(row, 1)?,
        submitted_event_id: blob_to_id_row(row, 2)?,
        task_id: blob_to_id_row(row, 3)?,
        agent_session_id: blob_to_id_row(row, 4)?,
        provider_kind: row.get(5)?,
        body: row.get(6)?,
        submitted_at_ms: row.get(7)?,
        prompt_id: optional_blob_to_id_row(row, 8)?,
        prompt_version_id: optional_blob_to_id_row(row, 9)?,
        chain_id: optional_blob_to_id_row(row, 10)?,
        chain_link_id: optional_blob_to_id_row(row, 11)?,
    })
}

fn read_history_entry_with_hash(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<(PromptHistoryEntry, [u8; 32])> {
    let entry = read_history_entry(row)?;
    let digest: Vec<u8> = row.get(12)?;
    let body_sha256: [u8; 32] = digest
        .as_slice()
        .try_into()
        .map_err(|_| rusqlite::Error::InvalidQuery)?;
    Ok((entry, body_sha256))
}

fn verify_stored_body_hash(body: &str, stored_hash: &[u8; 32]) -> Result<(), PromptHistoryError> {
    let recomputed: [u8; 32] = Sha256::digest(body.as_bytes()).into();
    if &recomputed != stored_hash {
        return Err(PromptHistoryError::from_code(
            PromptHistoryErrorCode::Storage,
        ));
    }
    Ok(())
}

fn digest32(bytes: &[u8]) -> Result<[u8; 32], PromptHistoryError> {
    bytes
        .try_into()
        .map_err(|_| PromptHistoryError::from_code(PromptHistoryErrorCode::Storage))
}

fn provenance_from_optional_blobs(
    prompt_id: Option<Vec<u8>>,
    prompt_version_id: Option<Vec<u8>>,
    chain_id: Option<Vec<u8>>,
    chain_link_id: Option<Vec<u8>>,
) -> Result<PromptHistoryProvenance, PromptHistoryError> {
    Ok(PromptHistoryProvenance {
        prompt_id: optional_id_from_blob(prompt_id)?,
        prompt_version_id: optional_id_from_blob(prompt_version_id)?,
        chain_id: optional_id_from_blob(chain_id)?,
        chain_link_id: optional_id_from_blob(chain_link_id)?,
    })
}

fn optional_id_from_blob<T>(bytes: Option<Vec<u8>>) -> Result<Option<T>, PromptHistoryError>
where
    T: FromUuidBytes,
{
    match bytes {
        None => Ok(None),
        Some(bytes) => {
            let bytes: [u8; 16] = bytes
                .as_slice()
                .try_into()
                .map_err(|_| PromptHistoryError::from_code(PromptHistoryErrorCode::Storage))?;
            T::from_bytes(bytes)
                .map(Some)
                .map_err(|_| PromptHistoryError::from_code(PromptHistoryErrorCode::Storage))
        }
    }
}

fn verify_history_index_payload(
    conn: &Connection,
    body: &str,
    digest: &[u8],
    prompt_id: Option<Vec<u8>>,
    prompt_version_id: Option<Vec<u8>>,
    chain_id: Option<Vec<u8>>,
    chain_link_id: Option<Vec<u8>>,
) -> Result<(), PromptHistoryError> {
    verify_stored_body_hash(body, &digest32(digest)?)?;
    validate_provenance(
        conn,
        &provenance_from_optional_blobs(prompt_id, prompt_version_id, chain_id, chain_link_id)?,
    )
}

fn verify_saved_index_payload(body: &str, digest: &[u8]) -> Result<(), PromptHistoryError> {
    verify_stored_body_hash(body, &digest32(digest)?)
}

fn load_policy(conn: &Connection) -> Result<PromptHistoryPolicy, PromptHistoryError> {
    let (enabled, retention_days, max_entries): (i64, i64, i64) = conn.query_row(
        "SELECT enabled, retention_days, max_entries
         FROM prompt_history_policy WHERE singleton_key = 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    PromptHistoryPolicy {
        enabled: enabled != 0,
        retention_days: u16::try_from(retention_days)
            .map_err(|_| PromptHistoryError::from_code(PromptHistoryErrorCode::InvalidPolicy))?,
        max_entries: u32::try_from(max_entries)
            .map_err(|_| PromptHistoryError::from_code(PromptHistoryErrorCode::InvalidPolicy))?,
    }
    .validate()
}

fn load_revision(conn: &Connection) -> Result<u64, PromptHistoryError> {
    let revision: i64 = conn.query_row(
        "SELECT revision FROM prompt_history_policy WHERE singleton_key = 1",
        [],
        |row| row.get(0),
    )?;
    u64::try_from(revision)
        .map_err(|_| PromptHistoryError::from_code(PromptHistoryErrorCode::InvalidPolicy))
}

fn confirm_revision(conn: &Connection, revision: u64) -> Result<(), PromptHistoryError> {
    if load_revision(conn)? != revision {
        return Err(PromptHistoryError::from_code(
            PromptHistoryErrorCode::Unconfirmed,
        ));
    }
    Ok(())
}

fn reserve_pending_or_overflow(conn: &Connection) -> Result<(), PromptHistoryError> {
    bump_current_seq(conn)?;
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM prompt_search_pending", [], |row| {
        row.get(0)
    })?;
    if count >= i64::from(DEFAULT_HISTORY_INDEX_CAPACITY) {
        conn.execute(
            "UPDATE prompt_search_state SET dirty = 1, overflow = 1 WHERE singleton_key = 1",
            [],
        )?;
        return Ok(());
    }
    mark_dirty(conn)
}

fn mark_pending(
    conn: &Connection,
    source_kind: &str,
    source_id: &[u8; 16],
) -> Result<(), PromptHistoryError> {
    bump_current_seq(conn)?;
    let overflow: i64 = conn.query_row(
        "SELECT overflow FROM prompt_search_state WHERE singleton_key = 1",
        [],
        |row| row.get(0),
    )?;
    let queued: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM prompt_search_pending
             WHERE source_kind = ?1 AND source_id = ?2",
            rusqlite::params![source_kind, source_id.as_slice()],
            |row| row.get(0),
        )
        .optional()?;
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM prompt_search_pending", [], |row| {
        row.get(0)
    })?;
    if queued.is_none() && (overflow != 0 || count >= i64::from(DEFAULT_HISTORY_INDEX_CAPACITY)) {
        conn.execute(
            "UPDATE prompt_search_state SET dirty = 1, overflow = 1 WHERE singleton_key = 1",
            [],
        )?;
        return Ok(());
    }
    conn.execute(
        "INSERT INTO prompt_search_pending(source_kind, source_id, enqueue_seq)
         SELECT ?1, ?2, current_seq
         FROM prompt_search_state
         WHERE singleton_key = 1
           AND (
                ?3 != 0
                OR (
                    overflow = 0
                    AND (SELECT COUNT(*) FROM prompt_search_pending) < ?4
                )
           )
         ON CONFLICT(source_kind, source_id) DO UPDATE SET
           enqueue_seq = excluded.enqueue_seq",
        rusqlite::params![
            source_kind,
            source_id.as_slice(),
            i64::from(queued.is_some()),
            i64::from(DEFAULT_HISTORY_INDEX_CAPACITY)
        ],
    )?;
    mark_dirty(conn)
}

fn bump_current_seq(conn: &Connection) -> Result<(), PromptHistoryError> {
    conn.execute(
        "UPDATE prompt_search_state SET current_seq = current_seq + 1 WHERE singleton_key = 1",
        [],
    )?;
    Ok(())
}

fn capture_rebuild_high_water(conn: &Connection) -> Result<(), PromptHistoryError> {
    conn.execute(
        "UPDATE prompt_search_state SET high_water_seq = current_seq WHERE singleton_key = 1",
        [],
    )?;
    Ok(())
}

fn load_index_seqs(conn: &Connection) -> Result<(i64, i64), PromptHistoryError> {
    conn.query_row(
        "SELECT current_seq, high_water_seq
         FROM prompt_search_state WHERE singleton_key = 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .map_err(Into::into)
}

fn cursor_schema_version(conn: &Connection) -> Result<i64, PromptHistoryError> {
    conn.query_row("SELECT MAX(version) FROM schema_migrations", [], |row| {
        row.get(0)
    })
    .map_err(Into::into)
}

pub(crate) fn history_recent_query_sha256() -> [u8; 32] {
    Sha256::digest(HISTORY_RECENT_QUERY).into()
}

fn next_pending_row(
    conn: &Connection,
) -> Result<Option<(String, Vec<u8>, i64)>, PromptHistoryError> {
    conn.query_row(
        "SELECT source_kind, source_id, enqueue_seq FROM prompt_search_pending
         ORDER BY source_kind, source_id
         LIMIT 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )
    .optional()
    .map_err(Into::into)
}

fn mark_dirty(conn: &Connection) -> Result<(), PromptHistoryError> {
    conn.execute(
        "UPDATE prompt_search_state SET dirty = 1 WHERE singleton_key = 1",
        [],
    )?;
    Ok(())
}

fn maybe_clear_dirty(conn: &Connection) -> Result<(), PromptHistoryError> {
    let pending: i64 = conn.query_row("SELECT COUNT(*) FROM prompt_search_pending", [], |row| {
        row.get(0)
    })?;
    let phase: String = conn.query_row(
        "SELECT rebuild_phase FROM prompt_search_state WHERE singleton_key = 1",
        [],
        |row| row.get(0),
    )?;
    let overflow: i64 = conn.query_row(
        "SELECT overflow FROM prompt_search_state WHERE singleton_key = 1",
        [],
        |row| row.get(0),
    )?;
    if pending != 0 || phase != "idle" || overflow != 0 {
        return Ok(());
    }
    let history: i64 =
        conn.query_row("SELECT COUNT(*) FROM prompt_history", [], |row| row.get(0))?;
    let saved: i64 = conn.query_row("SELECT COUNT(*) FROM saved_prompts", [], |row| row.get(0))?;
    let indexed: i64 =
        conn.query_row("SELECT COUNT(*) FROM prompt_search", [], |row| row.get(0))?;
    if indexed == history + saved {
        conn.execute(
            "UPDATE prompt_search_state SET dirty = 0 WHERE singleton_key = 1",
            [],
        )?;
    }
    Ok(())
}

fn delete_history_batch(
    tx: &Transaction<'_>,
    select_sql: &str,
    params: impl rusqlite::Params,
    budget: &PromptSearchBudget<'_>,
    work_used: &mut usize,
) -> Result<u32, PromptHistoryError> {
    let ids: Vec<Vec<u8>> = {
        let mut stmt = tx.prepare(select_sql)?;
        let rows = stmt.query_map(params, |row| row.get(0))?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    let mut deleted = 0_u32;
    for source_id in &ids {
        if !budget.check(*work_used, 0)? {
            break;
        }
        *work_used = work_used.saturating_add(1);
        delete_search_row(tx, SOURCE_HISTORY, source_id)?;
        tx.execute(
            "DELETE FROM prompt_search_pending
             WHERE source_kind = ?1 AND source_id = ?2",
            rusqlite::params![SOURCE_HISTORY, source_id.as_slice()],
        )?;
        tx.execute(
            "DELETE FROM prompt_history WHERE prompt_history_id = ?1",
            [source_id.as_slice()],
        )?;
        deleted = deleted.saturating_add(1);
    }
    Ok(deleted)
}

fn apply_index_work(
    conn: &Connection,
    source_kind: &str,
    source_id: &[u8],
) -> Result<(), PromptHistoryError> {
    match source_kind {
        SOURCE_HISTORY => {
            let row: Option<(
                String,
                Vec<u8>,
                Option<Vec<u8>>,
                Option<Vec<u8>>,
                Option<Vec<u8>>,
                Option<Vec<u8>>,
            )> = conn
                .query_row(
                    "SELECT body, body_sha256, prompt_id, prompt_version_id,
                            chain_id, chain_link_id
                     FROM prompt_history WHERE prompt_history_id = ?1",
                    [source_id],
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
            let Some((body, digest, prompt_id, prompt_version_id, chain_id, chain_link_id)) = row
            else {
                return Ok(());
            };
            verify_history_index_payload(
                conn,
                &body,
                &digest,
                prompt_id,
                prompt_version_id,
                chain_id,
                chain_link_id,
            )?;
            upsert_search_row(conn, SOURCE_HISTORY, source_id, "", &body, "")
        }
        SOURCE_SAVED => {
            let row: Option<(String, String, Vec<u8>, String)> = conn
                .query_row(
                    "SELECT p.title, v.body, v.body_sha256,
                            COALESCE((
                                SELECT group_concat(t.tag, ' ')
                                FROM (
                                    SELECT tag FROM prompt_tags
                                    WHERE prompt_id = p.prompt_id
                                    ORDER BY position
                                ) AS t
                            ), '')
                     FROM saved_prompts AS p
                     JOIN prompt_versions AS v
                       ON v.prompt_id = p.prompt_id
                      AND v.prompt_version_id = p.current_version_id
                     WHERE p.prompt_id = ?1",
                    [source_id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .optional()?;
            let Some((title, body, digest, tags)) = row else {
                return Ok(());
            };
            verify_saved_index_payload(&body, &digest)?;
            upsert_search_row(conn, SOURCE_SAVED, source_id, &title, &body, &tags)
        }
        _ => Err(PromptHistoryError::from_code(
            PromptHistoryErrorCode::Storage,
        )),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RebuildPhase {
    Idle,
    History,
    Saved,
    Pending,
}

struct RebuildState {
    phase: RebuildPhase,
    submitted_at_ms: Option<i64>,
    source_id: Option<[u8; 16]>,
}

struct HistoryRebuildRow {
    source_id: [u8; 16],
    submitted_at_ms: i64,
    body: String,
}

struct SavedRebuildRow {
    source_id: [u8; 16],
    title: String,
    body: String,
    tags: String,
}

fn load_rebuild_state(conn: &Connection) -> Result<RebuildState, PromptHistoryError> {
    let (phase, submitted_at_ms, source_id): (String, Option<i64>, Option<Vec<u8>>) = conn
        .query_row(
            "SELECT rebuild_phase, rebuild_submitted_at_ms, rebuild_source_id
             FROM prompt_search_state WHERE singleton_key = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
    Ok(RebuildState {
        phase: match phase.as_str() {
            "history" => RebuildPhase::History,
            "saved" => RebuildPhase::Saved,
            "pending" => RebuildPhase::Pending,
            _ => RebuildPhase::Idle,
        },
        submitted_at_ms,
        source_id: match source_id {
            Some(bytes) => Some(
                bytes
                    .as_slice()
                    .try_into()
                    .map_err(|_| PromptHistoryError::from_code(PromptHistoryErrorCode::Storage))?,
            ),
            None => None,
        },
    })
}

fn persist_rebuild_state(
    conn: &Connection,
    state: &RebuildState,
) -> Result<(), PromptHistoryError> {
    let phase = match state.phase {
        RebuildPhase::Idle => "idle",
        RebuildPhase::History => "history",
        RebuildPhase::Saved => "saved",
        RebuildPhase::Pending => "pending",
    };
    conn.execute(
        "UPDATE prompt_search_state
         SET rebuild_phase = ?1,
             rebuild_submitted_at_ms = ?2,
             rebuild_source_id = ?3
         WHERE singleton_key = 1",
        rusqlite::params![
            phase,
            state.submitted_at_ms,
            state.source_id.map(|id| id.to_vec()),
        ],
    )?;
    Ok(())
}

fn next_history_rebuild_row(
    conn: &Connection,
    state: &RebuildState,
) -> Result<Option<HistoryRebuildRow>, PromptHistoryError> {
    let loaded: Option<(
        Vec<u8>,
        i64,
        String,
        Vec<u8>,
        Option<Vec<u8>>,
        Option<Vec<u8>>,
        Option<Vec<u8>>,
        Option<Vec<u8>>,
    )> = conn
        .query_row(
            "SELECT prompt_history_id, submitted_at_ms, body, body_sha256,
                    prompt_id, prompt_version_id, chain_id, chain_link_id
             FROM prompt_history
             WHERE ?1 IS NULL
                OR submitted_at_ms < ?1
                OR (submitted_at_ms = ?1 AND prompt_history_id < ?2)
             ORDER BY submitted_at_ms DESC, prompt_history_id DESC
             LIMIT 1",
            rusqlite::params![state.submitted_at_ms, state.source_id.map(|id| id.to_vec())],
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
        source_id,
        submitted_at_ms,
        body,
        digest,
        prompt_id,
        prompt_version_id,
        chain_id,
        chain_link_id,
    )) = loaded
    else {
        return Ok(None);
    };
    verify_history_index_payload(
        conn,
        &body,
        &digest,
        prompt_id,
        prompt_version_id,
        chain_id,
        chain_link_id,
    )?;
    Ok(Some(HistoryRebuildRow {
        source_id: source_id
            .as_slice()
            .try_into()
            .map_err(|_| PromptHistoryError::from_code(PromptHistoryErrorCode::Storage))?,
        submitted_at_ms,
        body,
    }))
}

fn next_saved_rebuild_row(
    conn: &Connection,
    state: &RebuildState,
) -> Result<Option<SavedRebuildRow>, PromptHistoryError> {
    let loaded: Option<(Vec<u8>, String, String, Vec<u8>, String)> = conn
        .query_row(
            "SELECT p.prompt_id, p.title, v.body, v.body_sha256,
                    COALESCE((
                        SELECT group_concat(t.tag, ' ')
                        FROM (
                            SELECT tag FROM prompt_tags
                            WHERE prompt_id = p.prompt_id
                            ORDER BY position
                        ) AS t
                    ), '')
             FROM saved_prompts AS p
             JOIN prompt_versions AS v
               ON v.prompt_id = p.prompt_id
              AND v.prompt_version_id = p.current_version_id
             WHERE ?1 IS NULL OR p.prompt_id < ?1
             ORDER BY p.prompt_id DESC
             LIMIT 1",
            [state.source_id.map(|id| id.to_vec())],
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
    let Some((source_id, title, body, digest, tags)) = loaded else {
        return Ok(None);
    };
    verify_saved_index_payload(&body, &digest)?;
    Ok(Some(SavedRebuildRow {
        source_id: source_id
            .as_slice()
            .try_into()
            .map_err(|_| PromptHistoryError::from_code(PromptHistoryErrorCode::Storage))?,
        title,
        body,
        tags,
    }))
}

fn count_rebuild_remaining(
    conn: &Connection,
    state: &RebuildState,
) -> Result<u64, PromptHistoryError> {
    let history = match state.phase {
        RebuildPhase::Idle | RebuildPhase::Pending => 0,
        RebuildPhase::History => {
            let count: i64 = conn.query_row(
                "SELECT COUNT(*) FROM prompt_history
                 WHERE ?1 IS NULL
                    OR submitted_at_ms < ?1
                    OR (submitted_at_ms = ?1 AND prompt_history_id < ?2)",
                rusqlite::params![state.submitted_at_ms, state.source_id.map(|id| id.to_vec())],
                |row| row.get(0),
            )?;
            count
        }
        RebuildPhase::Saved => 0,
    };
    let saved = match state.phase {
        RebuildPhase::Idle | RebuildPhase::Pending => 0,
        RebuildPhase::History | RebuildPhase::Saved => {
            let count: i64 = conn.query_row(
                "SELECT COUNT(*) FROM saved_prompts
                 WHERE ?1 IS NULL OR prompt_id < ?1",
                [if state.phase == RebuildPhase::Saved {
                    state.source_id.map(|id| id.to_vec())
                } else {
                    None
                }],
                |row| row.get(0),
            )?;
            count
        }
    };
    let pending: i64 = match state.phase {
        RebuildPhase::Idle => 0,
        RebuildPhase::History | RebuildPhase::Saved | RebuildPhase::Pending => {
            conn.query_row("SELECT COUNT(*) FROM prompt_search_pending", [], |row| {
                row.get(0)
            })?
        }
    };
    Ok(u64::try_from(history.saturating_add(saved).saturating_add(pending)).unwrap_or(0))
}

fn configure_connection(path: &Path) -> Result<Connection, PromptHistoryError> {
    let conn = Connection::open(path)?;
    conn.busy_timeout(Duration::from_millis(BUSY_TIMEOUT_MS))?;
    let _: String = conn.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))?;
    conn.execute_batch("PRAGMA foreign_keys=ON; PRAGMA synchronous=NORMAL;")?;
    Ok(conn)
}

fn blob_to_id_row<T>(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<T>
where
    T: FromUuidBytes,
{
    let bytes: Vec<u8> = row.get(index)?;
    let bytes: [u8; 16] = bytes
        .try_into()
        .map_err(|_| rusqlite::Error::InvalidQuery)?;
    T::from_bytes(bytes).map_err(|_| rusqlite::Error::InvalidQuery)
}

fn optional_blob_to_id_row<T>(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<Option<T>>
where
    T: FromUuidBytes,
{
    let bytes: Option<Vec<u8>> = row.get(index)?;
    match bytes {
        None => Ok(None),
        Some(bytes) => {
            let bytes: [u8; 16] = bytes
                .try_into()
                .map_err(|_| rusqlite::Error::InvalidQuery)?;
            T::from_bytes(bytes)
                .map(Some)
                .map_err(|_| rusqlite::Error::InvalidQuery)
        }
    }
}

trait FromUuidBytes: Sized {
    fn from_bytes(bytes: [u8; 16]) -> Result<Self, crate::domain::IdError>;
}

macro_rules! impl_from_uuid_bytes {
    ($($ty:ty),+) => {
        $(
            impl FromUuidBytes for $ty {
                fn from_bytes(bytes: [u8; 16]) -> Result<Self, crate::domain::IdError> {
                    <$ty>::from_bytes(bytes)
                }
            }
        )+
    };
}

impl_from_uuid_bytes!(
    PromptHistoryId,
    RequestId,
    EventId,
    TaskId,
    AgentSessionId,
    PromptId,
    PromptVersionId,
    PromptChainId,
    PromptChainLinkId
);

#[cfg(test)]
#[path = "history_tests.rs"]
mod history_tests;
