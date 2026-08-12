use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior};
use sha2::{Digest, Sha256};

use crate::domain::command::{CommandEnvelope, CommandReceipt};
use crate::domain::event::{
    AgentSessionRegisteredPayload, ArtifactRegisteredPayload, DomainEvent, Event,
    HostCleanupBranchCompletedPayload, HostCloseBegunPayload, OperationAcceptedFact,
    OperationCancelledFact, OperationFailedFact, OperationSettledFact, OperationUncertainFact,
    PrimaryAgentSetPayload, ProviderApprovalPresentedPayload, ProviderInputAcceptedPayload,
    ProviderInputDeliveredPayload, ProviderQuestionPresentedPayload, ProviderWaitSettledPayload,
    ResourceRegisteredPayload, ResourceReleaseBegunPayload, ResourceReleasedPayload,
    TaskAttentionSetPayload, TaskCloseBegunPayload, TaskCreatedPayload, TaskRenamedPayload,
    TaskUnitPayload, EVENT_SCHEMA_VERSION,
};
use crate::domain::id::{EventId, OperationId, OutboxId, TaskId};
use crate::domain::operation::{
    OperationOutcome, OperationOutcomeKind, OperationState, OutcomeSource, ResourceFence,
};
use crate::kernel::command_bus::{
    self, effect_document_for_terminal_replay, load_outbox_row_by_id,
    refuse_archive_with_live_resources, validate_dispatch_attempt_lineage,
    validate_dispatch_candidate_lineage, OutboxRow,
};
use crate::kernel::dispatch::{
    ambiguity_disposition, decode_absence_receipt, encode_absence_receipt, AbsenceReceiptDocument,
    AmbiguityDisposition, DispatchClaim, DispatchCompletion, DispatchPermit, ReconciliationClaim,
    ReconciliationFinding, ReconciliationOrigin,
};
use crate::kernel::maintenance;
use crate::kernel::outbox::{
    external_idempotency_key, DestinationClass, Effect, ReplayPolicy,
};
use crate::kernel::projector;
use crate::kernel::runtime::RecoveringResource;
use crate::kernel::schema::{self, Migration, PROJECTION_TABLES};
use crate::kernel::StoreMaintenanceReport;

const BUSY_TIMEOUT_MS: i64 = 5_000;
const MAX_DISPATCH_LEASE_MS: i64 = 3_600_000;
pub(crate) const MAX_PROVIDER_EVENT_PAYLOAD_BYTES: usize = 256 * 1024;

/// Opaque SQLite-backed kernel store. No public connection accessor.
pub struct KernelStore {
    path: PathBuf,
    conn: Connection,
}

impl fmt::Debug for KernelStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("KernelStore")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectionRebuild {
    pub events_replayed: u64,
    pub drift_detected: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreError {
    Io(String),
    Sqlite(String),
    Busy,
    Corruption,
    Truncated,
    ConstraintViolation,
    StaleFence,
    ConflictingOutcome,
    MissingOperation,
    IntegrityCheckFailed(String),
    MigrationTooNew { found: i64, supported: i64 },
    MigrationChanged { version: i64 },
    MigrationGap { expected: i64, found: i64 },
    MigrationInterrupted,
    IntegerOutOfRange { field: &'static str, value: u64 },
    CodecMismatch { detail: String },
    EventDecode(String),
    Projection(String),
    InvalidLeaseDuration,
    StaleClaim,
    ExpiredClaim,
    InvalidDispatchTransition,
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(msg) => write!(f, "store io error: {msg}"),
            Self::Sqlite(msg) => write!(f, "sqlite error: {msg}"),
            Self::Busy => write!(f, "sqlite busy"),
            Self::Corruption => write!(f, "database corruption detected"),
            Self::Truncated => write!(f, "database file is truncated"),
            Self::ConstraintViolation => write!(f, "sqlite constraint violation"),
            Self::StaleFence => write!(f, "stale operation fence"),
            Self::ConflictingOutcome => write!(f, "conflicting operation outcome"),
            Self::MissingOperation => write!(f, "operation not found"),
            Self::IntegrityCheckFailed(msg) => write!(f, "integrity check failed: {msg}"),
            Self::MigrationTooNew { found, supported } => {
                write!(
                    f,
                    "database migration version {found} is newer than supported {supported}"
                )
            }
            Self::MigrationChanged { version } => {
                write!(f, "recorded migration {version} does not match manifest")
            }
            Self::MigrationGap { expected, found } => {
                write!(
                    f,
                    "migration history gap: expected version {expected}, found {found}"
                )
            }
            Self::MigrationInterrupted => write!(f, "interrupted schema migration"),
            Self::IntegerOutOfRange { field, value } => {
                write!(f, "integer out of range for {field}: {value}")
            }
            Self::CodecMismatch { detail } => write!(f, "codec mismatch: {detail}"),
            Self::EventDecode(msg) => write!(f, "event decode error: {msg}"),
            Self::Projection(msg) => write!(f, "projection error: {msg}"),
            Self::InvalidLeaseDuration => write!(f, "invalid dispatch lease duration"),
            Self::StaleClaim => write!(f, "stale dispatch claim"),
            Self::ExpiredClaim => write!(f, "expired dispatch claim"),
            Self::InvalidDispatchTransition => write!(f, "invalid dispatch transition"),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<rusqlite::Error> for StoreError {
    fn from(err: rusqlite::Error) -> Self {
        map_sqlite_error(&err)
    }
}

impl KernelStore {
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        classify_path_before_open(path)?;
        let conn = Connection::open(path).map_err(map_open_error)?;
        let canonical = std::fs::canonicalize(path).map_err(|e| StoreError::Io(e.to_string()))?;
        configure_connection(&conn)?;
        let mut store = Self {
            path: canonical,
            conn,
        };
        store.migrate()?;
        store.integrity_check()?;
        Ok(store)
    }

    pub fn rebuild_projections(&mut self) -> Result<ProjectionRebuild, StoreError> {
        let tx = self.conn.transaction()?;
        let result = rebuild_projections_tx(&tx)?;
        tx.commit()?;
        Ok(result)
    }

    /// Execute a command in one IMMEDIATE writer transaction.
    pub fn execute(&mut self, envelope: CommandEnvelope) -> Result<CommandReceipt, StoreError> {
        command_bus::execute(self, envelope)
    }

    /// Load one durable operation state after validating its complete lineage.
    pub fn operation_status(
        &self,
        operation_id: OperationId,
    ) -> Result<Option<OperationState>, StoreError> {
        let conn = self.open_query_connection()?;
        let tx = conn.unchecked_transaction()?;
        let status = command_bus::operation_status_in_tx(&tx, operation_id)?;
        tx.commit()?;
        Ok(status)
    }

    /// Load durable recipes that require process-aware reconciliation.
    ///
    /// The returned values are metadata-only `Recovering` facts. This method
    /// neither probes nor claims that an operating-system process is alive.
    pub fn load_recovering_resources(&self) -> Result<Vec<RecoveringResource>, StoreError> {
        let conn = self.open_query_connection()?;
        let tx = conn.unchecked_transaction()?;
        let resources = command_bus::load_all_resources(&tx)?;
        let recovering = resources
            .into_iter()
            .filter_map(RecoveringResource::from_durable)
            .collect();
        tx.commit()?;
        Ok(recovering)
    }

    /// Run one explicit non-destructive maintenance pass outside command/query hot paths.
    #[allow(dead_code)] // consumed by the bounded host scheduler in a later phase
    pub(crate) fn run_maintenance(&mut self) -> Result<StoreMaintenanceReport, StoreError> {
        maintenance::run(
            &mut self.conn,
            maintenance::DEFAULT_OUTBOX_CLEANUP_BATCH_ROWS,
        )
    }

    /// Claim the next dispatch-ready outbox row under a bounded lease fence.
    pub fn claim_next_dispatch(
        &mut self,
        lease: Duration,
    ) -> Result<Option<DispatchClaim>, StoreError> {
        let lease_ms = validate_dispatch_lease_ms(lease)?;
        self.with_immediate_transaction(|tx| claim_next_dispatch_in_tx(tx, now_ms()?, lease_ms))
    }

    /// Settle the next eligible process-empty task teardown under a bounded lease.
    ///
    /// Selects only accepted, due `task_teardown` rows whose Closing task has no
    /// Active/Releasing Task-owned resources. Claim, begin, exact `BeginTaskTeardown`
    /// validation, and `DispatchCompletion::Settled` run in one IMMEDIATE transaction.
    pub(crate) fn settle_next_process_empty_task_teardown(
        &mut self,
        lease: Duration,
    ) -> Result<Option<(TaskId, OperationId)>, StoreError> {
        let lease_ms = validate_dispatch_lease_ms(lease)?;
        self.with_immediate_transaction(|tx| {
            settle_next_process_empty_task_teardown_in_tx(tx, now_ms()?, lease_ms)
        })
    }

    /// Advance one Closing host-cleanup journal unit under a bounded teardown lease.
    pub(crate) fn advance_next_host_cleanup_unit(
        &mut self,
        lease: Duration,
    ) -> Result<command_bus::HostCleanupUnit, StoreError> {
        let lease_ms = validate_dispatch_lease_ms(lease)?;
        self.with_immediate_transaction(|tx| {
            command_bus::advance_next_host_cleanup_unit_in_tx(tx, now_ms()?, lease_ms)
        })
    }

    /// Explicit all-success host-cleanup settle (c8b post-arm path only).
    ///
    /// Returns the exact persisted terminal [`DomainEvent`], including identical
    /// event id and sequence on idempotent retry.
    pub(crate) fn settle_host_cleanup_success(
        &mut self,
    ) -> Result<crate::domain::event::DomainEvent, StoreError> {
        self.with_immediate_transaction(|tx| {
            command_bus::settle_host_cleanup_success_in_tx(tx, now_ms()?)
        })
    }

    /// Renew an active dispatch lease when the claim generation still matches.
    pub fn renew_dispatch_claim(
        &mut self,
        claim: &DispatchClaim,
        lease: Duration,
    ) -> Result<DispatchClaim, StoreError> {
        let lease_ms = validate_dispatch_lease_ms(lease)?;
        self.with_immediate_transaction(|tx| {
            renew_dispatch_claim_in_tx(tx, now_ms()?, claim, lease_ms)
        })
    }

    /// Release a pre-dispatch claim without incrementing attempts or generation.
    pub fn release_dispatch_claim(
        &mut self,
        claim: &DispatchClaim,
        next_available: Duration,
    ) -> Result<(), StoreError> {
        let delay_ms = validate_dispatch_lease_ms(next_available)?;
        self.with_immediate_transaction(|tx| {
            release_dispatch_claim_in_tx(tx, claim, now_ms()?, delay_ms)
        })
    }

    /// Begin dispatch for a live claim, returning an authorizing permit.
    pub fn begin_dispatch(&mut self, claim: &DispatchClaim) -> Result<DispatchPermit, StoreError> {
        self.with_immediate_transaction(|tx| begin_dispatch_in_tx(tx, now_ms()?, claim))
    }

    /// Record a terminal result for the exact in-flight dispatch attempt.
    pub fn record_dispatch_completion(
        &mut self,
        permit: &DispatchPermit,
        completion: DispatchCompletion,
    ) -> Result<OperationState, StoreError> {
        command_bus::record_dispatch_completion(self, permit, completion)
    }

    /// Conservatively recover one started attempt whose external result is ambiguous.
    pub fn record_dispatch_ambiguity(
        &mut self,
        permit: &DispatchPermit,
        retry_after: Duration,
    ) -> Result<AmbiguityDisposition, StoreError> {
        let delay_ms = validate_dispatch_lease_ms(retry_after)?;
        self.with_immediate_transaction(|tx| {
            record_dispatch_ambiguity_in_tx(tx, permit, now_ms()?, delay_ms)
        })
    }

    /// Recover one expired started attempt without recreating its permit.
    pub fn recover_next_expired_dispatch(
        &mut self,
        retry_after: Duration,
    ) -> Result<Option<AmbiguityDisposition>, StoreError> {
        let delay_ms = validate_dispatch_lease_ms(retry_after)?;
        self.with_immediate_transaction(|tx| {
            recover_next_expired_dispatch_in_tx(tx, now_ms()?, delay_ms)
        })
    }

    /// Claim the next due reconciliation lookup under a bounded lease fence.
    pub fn claim_next_reconciliation(
        &mut self,
        lease: Duration,
    ) -> Result<Option<ReconciliationClaim>, StoreError> {
        let lease_ms = validate_dispatch_lease_ms(lease)?;
        self.with_immediate_transaction(|tx| {
            claim_next_reconciliation_in_tx(tx, now_ms()?, lease_ms)
        })
    }

    /// Renew a live reconciliation lease without changing its opaque generation.
    pub fn renew_reconciliation_claim(
        &mut self,
        claim: &ReconciliationClaim,
        lease: Duration,
    ) -> Result<ReconciliationClaim, StoreError> {
        let lease_ms = validate_dispatch_lease_ms(lease)?;
        self.with_immediate_transaction(|tx| {
            renew_reconciliation_claim_in_tx(tx, claim, now_ms()?, lease_ms)
        })
    }

    /// Release reconciliation work for a later claim without losing ambiguity.
    pub fn release_reconciliation_claim(
        &mut self,
        claim: &ReconciliationClaim,
        retry_after: Duration,
    ) -> Result<(), StoreError> {
        let delay_ms = validate_dispatch_lease_ms(retry_after)?;
        self.with_immediate_transaction(|tx| {
            release_reconciliation_claim_in_tx(tx, claim, now_ms()?, delay_ms)
        })
    }

    /// Record evidence for one live reconciliation claim.
    pub fn record_reconciliation(
        &mut self,
        claim: &ReconciliationClaim,
        finding: ReconciliationFinding,
    ) -> Result<OperationState, StoreError> {
        self.with_immediate_transaction(|tx| {
            record_reconciliation_in_tx(tx, claim, finding, now_ms()?)
        })
    }

    /// Canonical database path retained for private snapshot connections.
    #[allow(dead_code)] // reserved for Task 1.4+ snapshot loading
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Open a crate-private read-only/query-only connection for snapshot loading.
    /// Never exposed on the public API and never writable.
    #[allow(dead_code)] // reserved for Task 1.4+ snapshot loading
    pub(crate) fn open_query_connection(&self) -> Result<Connection, StoreError> {
        open_readonly_query_connection(&self.path)
    }

    pub(crate) fn with_immediate_transaction<T>(
        &mut self,
        body: impl FnOnce(&Transaction<'_>) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let out = body(&tx)?;
        tx.commit()?;
        Ok(out)
    }

    #[cfg(test)]
    pub(crate) fn with_transaction<T>(
        &mut self,
        body: impl FnOnce(&Transaction<'_>) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        let tx = self.conn.transaction()?;
        let out = body(&tx)?;
        tx.commit()?;
        Ok(out)
    }

    pub(crate) fn semantic_journal_ensure_session(
        &mut self,
        record: &crate::kernel::semantic_journal::SemanticJournalAuthorityRecord,
    ) -> Result<[u8; 16], StoreError> {
        self.with_immediate_transaction(|tx| {
            crate::kernel::semantic_journal::ensure_session(tx, record)
        })
    }

    pub(crate) fn semantic_journal_high_water(
        &self,
        digest: &[u8; 32],
    ) -> Result<(u64, Option<i64>), StoreError> {
        crate::kernel::semantic_journal::high_water(&self.conn, digest)
    }

    pub(crate) fn semantic_journal_high_water_validated(
        &self,
        digest: &[u8; 32],
        validate_row: impl FnMut(
            &crate::kernel::semantic_journal::SemanticJournalFactRow,
        ) -> Result<(), StoreError>,
    ) -> Result<(u64, Option<i64>), StoreError> {
        crate::kernel::semantic_journal::high_water_with_validator(&self.conn, digest, validate_row)
    }

    pub(crate) fn semantic_journal_retained_len(
        &self,
        digest: &[u8; 32],
    ) -> Result<usize, StoreError> {
        crate::kernel::semantic_journal::retained_len(&self.conn, digest)
    }

    pub(crate) fn semantic_journal_validate(
        &self,
        digest: &[u8; 32],
        validate_row: impl FnMut(
            &crate::kernel::semantic_journal::SemanticJournalFactRow,
        ) -> Result<(), StoreError>,
    ) -> Result<(u64, Option<i64>, Option<i64>), StoreError> {
        let tx = self.conn.unchecked_transaction()?;
        let result = crate::kernel::semantic_journal::validate_facts(&tx, digest, validate_row)?;
        tx.commit()?;
        Ok(result)
    }

    pub(crate) fn semantic_journal_write_fact(
        &mut self,
        digest: &[u8; 32],
        delivery_id: &str,
        provider_event_id: Option<&str>,
        payload_hash: [u8; 32],
        row: crate::kernel::semantic_journal::SemanticJournalFactRow,
        max_events: u32,
        max_dedupe_keys: u32,
        validate_row: impl FnMut(
            &crate::kernel::semantic_journal::SemanticJournalFactRow,
        ) -> Result<(), StoreError>,
    ) -> Result<crate::kernel::semantic_journal::SemanticJournalWrite, StoreError> {
        self.with_immediate_transaction(|tx| {
            crate::kernel::semantic_journal::write_fact(
                tx,
                digest,
                delivery_id,
                provider_event_id,
                payload_hash,
                row,
                max_events,
                max_dedupe_keys,
                validate_row,
            )
        })
    }

    pub(crate) fn semantic_journal_load_fact(
        &self,
        digest: &[u8; 32],
        sequence: i64,
        validate_row: impl FnMut(
            &crate::kernel::semantic_journal::SemanticJournalFactRow,
        ) -> Result<(), StoreError>,
    ) -> Result<Option<crate::kernel::semantic_journal::SemanticJournalFactRow>, StoreError> {
        let tx = self.conn.unchecked_transaction()?;
        crate::kernel::semantic_journal::validate_facts(&tx, digest, validate_row)?;
        let fact = crate::kernel::semantic_journal::load_fact(&tx, digest, sequence)?;
        tx.commit()?;
        Ok(fact)
    }

    pub(crate) fn semantic_journal_stream_page(
        &self,
        digest: &[u8; 32],
        after_sequence: i64,
        requested_high_water: Option<u64>,
        mut prepare: impl FnMut(
            u64,
            &[crate::kernel::semantic_journal::SemanticJournalPageRowMeta],
        ) -> Result<(), StoreError>,
        mut validate_metadata: impl for<'a> FnMut(
            &crate::kernel::semantic_journal::SemanticJournalFactRef<'a>,
        ) -> Result<(), StoreError>,
        mut preflight: impl for<'a> FnMut(
            u64,
            crate::kernel::semantic_journal::SemanticJournalFactRef<'a>,
        ) -> Result<
            crate::kernel::semantic_journal::SemanticJournalPageRowAction,
            StoreError,
        >,
        mut visit: impl FnMut(
            u64,
            crate::kernel::semantic_journal::SemanticJournalFactRow,
        ) -> Result<bool, StoreError>,
    ) -> Result<u64, StoreError> {
        let tx = self.conn.unchecked_transaction()?;
        let (count, _, _) = crate::kernel::semantic_journal::validate_fact_metadata(
            &tx,
            digest,
            &mut validate_metadata,
        )?;
        let next_sequence = count.checked_add(1).ok_or(StoreError::Corruption)?;
        let high_water = next_sequence.checked_sub(1).ok_or(StoreError::Corruption)?;
        if requested_high_water.is_some_and(|requested| requested != high_water) {
            return Err(StoreError::ConstraintViolation);
        }
        if after_sequence < 0
            || u64::try_from(after_sequence).map_or(true, |after| after > high_water)
        {
            return Err(StoreError::ConstraintViolation);
        }
        let high_water_i64 =
            i64::try_from(high_water).map_err(|_| StoreError::IntegerOutOfRange {
                field: "semantic_journal.high_water",
                value: high_water,
            })?;
        crate::kernel::semantic_journal::stream_page(
            &tx,
            digest,
            after_sequence,
            high_water_i64,
            &mut prepare,
            &mut preflight,
            |row| visit(high_water, row),
        )?;
        tx.commit()?;
        Ok(high_water)
    }

    #[cfg(test)]
    pub(crate) fn debug_delete_semantic_journal_fact(
        &mut self,
        digest: &[u8; 32],
        sequence: i64,
    ) -> Result<(), StoreError> {
        self.with_immediate_transaction(|tx| {
            crate::kernel::semantic_journal::debug_delete_fact(tx, digest, sequence)
        })
    }

    #[cfg(test)]
    pub(crate) fn debug_zero_semantic_journal_event_id(
        &mut self,
        digest: &[u8; 32],
        sequence: i64,
    ) -> Result<(), StoreError> {
        self.with_immediate_transaction(|tx| {
            crate::kernel::semantic_journal::debug_zero_event_id(tx, digest, sequence)
        })
    }

    fn migrate(&mut self) -> Result<(), StoreError> {
        let manifest = schema::migration_manifest();
        loop {
            let applied = load_applied_migrations(&self.conn)?;
            validate_applied_history(&applied, manifest)?;

            if applied.len() >= manifest.len() {
                detect_interrupted_partial_schema(&self.conn, &applied)?;
                return Ok(());
            }

            let next_index = applied.len();
            let Some(migration) = manifest.get(next_index) else {
                return Ok(());
            };

            let expected_version = i64::try_from(next_index.checked_add(1).ok_or(
                StoreError::IntegerOutOfRange {
                    field: "migration_index",
                    value: u64::MAX,
                },
            )?)
            .map_err(|_| StoreError::IntegerOutOfRange {
                field: "migration_version",
                value: u64::MAX,
            })?;

            if migration.version != expected_version {
                return Err(StoreError::MigrationGap {
                    expected: expected_version,
                    found: migration.version,
                });
            }

            detect_interrupted_partial_schema(&self.conn, &applied)?;

            // Apply only the next migration inside its own transaction.
            let tx = self.conn.transaction()?;
            tx.execute_batch(migration.sql)
                .map_err(classify_migration_apply_error)?;
            let applied_at_ms = now_ms()?;
            tx.execute(
                "INSERT INTO schema_migrations(version, name, applied_at_ms, sha256)
                 VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![
                    migration.version,
                    migration.name,
                    applied_at_ms,
                    migration.sha256.as_slice(),
                ],
            )?;
            tx.commit()?;
        }
    }

    fn integrity_check(&self) -> Result<(), StoreError> {
        let result: String = self
            .conn
            .query_row("PRAGMA integrity_check;", [], |row| row.get(0))?;
        if result == "ok" {
            Ok(())
        } else {
            Err(StoreError::IntegrityCheckFailed(result))
        }
    }
}

#[derive(Debug, Clone)]
struct AppliedMigration {
    version: i64,
    name: String,
    sha256: Vec<u8>,
}

fn configure_connection(conn: &Connection) -> Result<(), StoreError> {
    conn.busy_timeout(std::time::Duration::from_millis(
        u64::try_from(BUSY_TIMEOUT_MS).expect("positive"),
    ))?;
    // journal_mode returns the mode; execute_batch is fine for the rest.
    let mode: String = conn.query_row("PRAGMA journal_mode=WAL;", [], |row| row.get(0))?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(StoreError::Sqlite(format!(
            "failed to enable WAL mode, got {mode}"
        )));
    }
    conn.execute_batch(
        "PRAGMA foreign_keys=ON;\n\
         PRAGMA synchronous=NORMAL;",
    )?;
    let foreign_keys: i64 = conn.query_row("PRAGMA foreign_keys;", [], |row| row.get(0))?;
    if foreign_keys != 1 {
        return Err(StoreError::Sqlite(
            "foreign_keys pragma did not enable".into(),
        ));
    }
    let synchronous: i64 = conn.query_row("PRAGMA synchronous;", [], |row| row.get(0))?;
    if synchronous != 1 {
        return Err(StoreError::Sqlite(format!(
            "synchronous pragma expected NORMAL(1), got {synchronous}"
        )));
    }
    let busy_timeout: i64 = conn.query_row("PRAGMA busy_timeout;", [], |row| row.get(0))?;
    if busy_timeout != BUSY_TIMEOUT_MS {
        return Err(StoreError::Sqlite(format!(
            "busy_timeout expected {BUSY_TIMEOUT_MS}, got {busy_timeout}"
        )));
    }
    Ok(())
}

fn classify_path_before_open(path: &Path) -> Result<(), StoreError> {
    if !path.exists() {
        return Ok(());
    }
    let meta = std::fs::metadata(path).map_err(|e| StoreError::Io(e.to_string()))?;
    // Existing files shorter than a minimum valid SQLite header/page are truncated,
    // including a pre-existing zero-byte path (SQLite would otherwise initialize it).
    if meta.len() < 100 {
        return Err(StoreError::Truncated);
    }
    Ok(())
}

fn map_open_error(err: rusqlite::Error) -> StoreError {
    if let rusqlite::Error::SqliteFailure(code, _) = &err {
        match code.code {
            rusqlite::ErrorCode::DatabaseBusy
            | rusqlite::ErrorCode::DatabaseLocked
            | rusqlite::ErrorCode::DatabaseCorrupt
            | rusqlite::ErrorCode::NotADatabase
            | rusqlite::ErrorCode::ConstraintViolation => {
                return map_sqlite_error(&err);
            }
            _ => {}
        }
    }
    // Text fallback only when SQLite did not surface a typed code above.
    let lower = err.to_string().to_lowercase();
    if lower.contains("truncated") {
        StoreError::Truncated
    } else if lower.contains("file is not a database") || lower.contains("corrupt") {
        StoreError::Corruption
    } else {
        map_sqlite_error(&err)
    }
}

#[allow(dead_code)] // reserved for Task 1.4+ snapshot loading
fn open_readonly_query_connection(path: &Path) -> Result<Connection, StoreError> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(map_sqlite_error_owned)?;
    conn.busy_timeout(std::time::Duration::from_millis(
        u64::try_from(BUSY_TIMEOUT_MS).expect("positive"),
    ))
    .map_err(map_sqlite_error_owned)?;
    conn.execute_batch(
        "PRAGMA foreign_keys = ON;\n\
         PRAGMA query_only = ON;",
    )
    .map_err(map_sqlite_error_owned)?;
    let busy_timeout: i64 = conn
        .query_row("PRAGMA busy_timeout;", [], |row| row.get(0))
        .map_err(map_sqlite_error_owned)?;
    if busy_timeout != BUSY_TIMEOUT_MS {
        return Err(StoreError::Sqlite(format!(
            "busy_timeout expected {BUSY_TIMEOUT_MS}, got {busy_timeout}"
        )));
    }
    let foreign_keys: i64 = conn
        .query_row("PRAGMA foreign_keys;", [], |row| row.get(0))
        .map_err(map_sqlite_error_owned)?;
    if foreign_keys != 1 {
        return Err(StoreError::Sqlite(
            "foreign_keys pragma did not enable on query connection".into(),
        ));
    }
    let query_only: i64 = conn
        .query_row("PRAGMA query_only;", [], |row| row.get(0))
        .map_err(map_sqlite_error_owned)?;
    if query_only != 1 {
        return Err(StoreError::Sqlite(format!(
            "query_only pragma expected 1, got {query_only}"
        )));
    }
    Ok(conn)
}

#[allow(dead_code)] // helper for read-only opener
fn map_sqlite_error_owned(err: rusqlite::Error) -> StoreError {
    map_sqlite_error(&err)
}

fn map_sqlite_error(err: &rusqlite::Error) -> StoreError {
    match err {
        rusqlite::Error::SqliteFailure(code, _)
            if code.code == rusqlite::ErrorCode::DatabaseBusy
                || code.code == rusqlite::ErrorCode::DatabaseLocked =>
        {
            StoreError::Busy
        }
        rusqlite::Error::SqliteFailure(code, _)
            if code.code == rusqlite::ErrorCode::DatabaseCorrupt
                || code.code == rusqlite::ErrorCode::NotADatabase =>
        {
            StoreError::Corruption
        }
        rusqlite::Error::SqliteFailure(code, _)
            if code.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            StoreError::ConstraintViolation
        }
        other => StoreError::Sqlite(other.to_string()),
    }
}

fn classify_migration_apply_error(err: rusqlite::Error) -> StoreError {
    let msg = err.to_string().to_lowercase();
    if msg.contains("already exists") {
        StoreError::MigrationInterrupted
    } else {
        map_sqlite_error(&err)
    }
}

fn load_applied_migrations(conn: &Connection) -> Result<Vec<AppliedMigration>, StoreError> {
    let table_exists: bool = conn.query_row(
        "SELECT COUNT(*) > 0 FROM sqlite_schema WHERE type = 'table' AND name = 'schema_migrations'",
        [],
        |row| row.get(0),
    )?;
    if !table_exists {
        return Ok(Vec::new());
    }
    let mut stmt =
        conn.prepare("SELECT version, name, sha256 FROM schema_migrations ORDER BY version ASC")?;
    let rows = stmt.query_map([], |row| {
        Ok(AppliedMigration {
            version: row.get(0)?,
            name: row.get(1)?,
            sha256: row.get(2)?,
        })
    })?;
    let mut applied = Vec::new();
    for row in rows {
        applied.push(row?);
    }
    Ok(applied)
}

fn validate_applied_history(
    applied: &[AppliedMigration],
    manifest: &[Migration],
) -> Result<(), StoreError> {
    if applied.is_empty() {
        return Ok(());
    }

    let supported = schema::latest_migration_version();

    for (idx, row) in applied.iter().enumerate() {
        let expected_version = i64::try_from(idx + 1).expect("fits");
        if row.version != expected_version {
            return Err(StoreError::MigrationGap {
                expected: expected_version,
                found: row.version,
            });
        }
        if row.version > supported {
            return Err(StoreError::MigrationTooNew {
                found: row.version,
                supported,
            });
        }
        let Some(expected) = manifest.get(idx) else {
            return Err(StoreError::MigrationTooNew {
                found: row.version,
                supported,
            });
        };
        if row.name != expected.name || row.sha256.as_slice() != expected.sha256.as_slice() {
            return Err(StoreError::MigrationChanged {
                version: row.version,
            });
        }
    }
    Ok(())
}

fn detect_interrupted_partial_schema(
    conn: &Connection,
    applied: &[AppliedMigration],
) -> Result<(), StoreError> {
    let partial: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_schema
         WHERE type = 'table'
           AND name IN ('events', 'tasks', 'operations', 'command_receipts', 'outbox',
                        'agent_sessions', 'artifacts', 'resources', 'event_retention',
                        'host_admission', 'host_cleanup_branches',
                        'semantic_journal_sessions', 'semantic_journal_facts')",
        [],
        |row| row.get(0),
    )?;
    let semantic_objects: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_schema
         WHERE (type = 'table' AND name IN ('semantic_journal_sessions', 'semantic_journal_facts'))
            OR (type = 'index' AND name = 'idx_semantic_journal_facts_sequence')",
        [],
        |row| row.get(0),
    )?;
    let applied_version = applied
        .last()
        .map(|migration| migration.version)
        .unwrap_or(0);

    // Empty migration history or a migration that predates V7 with any
    // semantic-journal object present indicates an interrupted apply. Once V7
    // is recorded, validate its complete manifest (columns, checks, foreign
    // key, unique constraints, and explicit index) rather than counting
    // objects, which cannot detect a weakened or partially rebuilt object.
    if applied_version < 7 && semantic_objects > 0 {
        return Err(StoreError::MigrationInterrupted);
    }
    if applied_version >= 7 {
        schema::validate_semantic_journal_schema(conn)?;
    }
    if applied.is_empty() && partial > 0 {
        return Err(StoreError::MigrationInterrupted);
    }
    Ok(())
}

fn rebuild_projections_tx(tx: &Transaction<'_>) -> Result<ProjectionRebuild, StoreError> {
    let result = rebuild_projection_tables_tx(tx)?;
    command_bus::validate_all_rebuilt_outbox_metadata(tx)?;
    command_bus::validate_rebuilt_host_admission(tx)?;
    command_bus::validate_rebuilt_host_cleanup_branches(tx)?;
    Ok(result)
}

fn rebuild_projection_tables_tx(tx: &Transaction<'_>) -> Result<ProjectionRebuild, StoreError> {
    for table in PROJECTION_TABLES {
        let shadow = shadow_name(table);
        tx.execute_batch(&format!(
            "DROP TABLE IF EXISTS {shadow};\n\
             CREATE TEMP TABLE {shadow} AS SELECT * FROM {table} WHERE 0;"
        ))?;
    }

    let mut stmt = tx.prepare(
        "SELECT sequence, event_id, task_id, task_revision, event_type, schema_version,
                occurred_at_ms, payload
         FROM events
         ORDER BY sequence ASC",
    )?;
    let mut rows = stmt.query([])?;
    let mut events_replayed: u64 = 0;
    while let Some(row) = rows.next()? {
        let sequence: i64 = row.get(0)?;
        let event_id_bytes: Vec<u8> = row.get(1)?;
        let task_id_bytes: Option<Vec<u8>> = row.get(2)?;
        let task_revision: Option<i64> = row.get(3)?;
        let event_type: String = row.get(4)?;
        let schema_version: i64 = row.get(5)?;
        let occurred_at_ms: i64 = row.get(6)?;
        // `ValueRef` keeps the SQLite BLOB borrowed until its length has been
        // checked. Do not materialize an untrusted provider event payload just
        // to reject it in `decode_stored_event`.
        let payload_ref = row.get_ref(7)?;
        let payload_bytes = payload_ref
            .as_blob()
            .map_err(|error| StoreError::CodecMismatch {
                detail: format!("event payload is not a BLOB: {error}"),
            })?;
        if event_type.starts_with("provider_input.")
            && payload_bytes.len() > MAX_PROVIDER_EVENT_PAYLOAD_BYTES
        {
            return Err(StoreError::CodecMismatch {
                detail: format!(
                    "provider event payload exceeds {MAX_PROVIDER_EVENT_PAYLOAD_BYTES} bytes"
                ),
            });
        }
        let payload = payload_bytes.to_vec();

        let domain = decode_stored_domain_event(
            sequence,
            &event_id_bytes,
            task_id_bytes.as_deref(),
            task_revision,
            &event_type,
            schema_version,
            occurred_at_ms,
            &payload,
        )?;
        projector::apply_event(tx, &domain, true)?;
        events_replayed = events_replayed
            .checked_add(1)
            .ok_or(StoreError::IntegerOutOfRange {
                field: "events_replayed",
                value: u64::MAX,
            })?;
    }
    drop(rows);
    drop(stmt);
    projector::ensure_no_trailing_orphan_derived(tx)?;

    let mut drift_detected = false;
    for &table in PROJECTION_TABLES {
        if !projection_tables_equal(tx, table)? {
            drift_detected = true;
            break;
        }
    }

    // Atomically replace stable projection table contents (never rename tables).
    // Delete children before parents to satisfy foreign keys.
    tx.execute_batch(
        "DELETE FROM provider_input_state;\n\
         DELETE FROM agent_sessions;\n\
         DELETE FROM artifacts;\n\
         DELETE FROM resources;\n\
         DELETE FROM operations;\n\
         DELETE FROM tasks;\n\
         DELETE FROM host_cleanup_branches;\n\
         DELETE FROM host_admission;",
    )?;
    tx.execute_batch(
        "INSERT INTO tasks SELECT * FROM shadow_tasks;\n\
         INSERT INTO operations SELECT * FROM shadow_operations;\n\
         INSERT INTO agent_sessions SELECT * FROM shadow_agent_sessions;\n\
         INSERT INTO artifacts SELECT * FROM shadow_artifacts;\n\
         INSERT INTO resources SELECT * FROM shadow_resources;\n\
         INSERT INTO host_admission SELECT * FROM shadow_host_admission;\n\
         INSERT INTO host_cleanup_branches SELECT * FROM shadow_host_cleanup_branches;\n\
         INSERT INTO provider_input_state SELECT * FROM shadow_provider_input_state;",
    )?;
    for table in PROJECTION_TABLES {
        tx.execute(&format!("DROP TABLE {}", shadow_name(table)), [])?;
    }
    Ok(ProjectionRebuild {
        events_replayed,
        drift_detected,
    })
}

#[cfg(test)]
mod projector_rebuild_tests;

fn shadow_name(table: &str) -> String {
    format!("shadow_{table}")
}

fn projection_tables_equal(tx: &Transaction<'_>, table: &str) -> Result<bool, StoreError> {
    let stable_dump = canonical_table_dump(tx, table, false)?;
    let shadow_dump = canonical_table_dump(tx, table, true)?;
    Ok(stable_dump == shadow_dump)
}

fn canonical_table_dump(
    tx: &Transaction<'_>,
    table: &str,
    shadow: bool,
) -> Result<Vec<u8>, StoreError> {
    // Hard-coded allowlisted statements only — never interpolate arbitrary table names.
    let sql = match (table, shadow) {
        ("tasks", false) => "SELECT * FROM tasks ORDER BY task_id ASC",
        ("tasks", true) => "SELECT * FROM shadow_tasks ORDER BY task_id ASC",
        ("operations", false) => "SELECT * FROM operations ORDER BY operation_id ASC",
        ("operations", true) => "SELECT * FROM shadow_operations ORDER BY operation_id ASC",
        ("agent_sessions", false) => "SELECT * FROM agent_sessions ORDER BY agent_session_id ASC",
        ("agent_sessions", true) => {
            "SELECT * FROM shadow_agent_sessions ORDER BY agent_session_id ASC"
        }
        ("artifacts", false) => "SELECT * FROM artifacts ORDER BY artifact_id ASC",
        ("artifacts", true) => "SELECT * FROM shadow_artifacts ORDER BY artifact_id ASC",
        ("resources", false) => "SELECT * FROM resources ORDER BY resource_id ASC",
        ("resources", true) => "SELECT * FROM shadow_resources ORDER BY resource_id ASC",
        ("host_admission", false) => "SELECT * FROM host_admission ORDER BY singleton_key ASC",
        ("host_admission", true) => {
            "SELECT * FROM shadow_host_admission ORDER BY singleton_key ASC"
        }
        ("host_cleanup_branches", false) => {
            "SELECT * FROM host_cleanup_branches ORDER BY operation_id ASC, branch ASC"
        }
        ("host_cleanup_branches", true) => {
            "SELECT * FROM shadow_host_cleanup_branches ORDER BY operation_id ASC, branch ASC"
        }
        ("provider_input_state", false) => {
            "SELECT * FROM provider_input_state ORDER BY agent_session_id ASC"
        }
        ("provider_input_state", true) => {
            "SELECT * FROM shadow_provider_input_state ORDER BY agent_session_id ASC"
        }
        _ => {
            return Err(StoreError::Projection(format!(
                "unsupported projection table for canonical compare: {table}"
            )))
        }
    };

    let mut hasher = Sha256::new();
    let mut stmt = tx.prepare(sql)?;
    let column_count = stmt.column_count();
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        for idx in 0..column_count {
            let value: rusqlite::types::Value = row.get(idx)?;
            match value {
                rusqlite::types::Value::Null => hasher.update([0]),
                rusqlite::types::Value::Integer(v) => {
                    hasher.update([1]);
                    hasher.update(v.to_le_bytes());
                }
                rusqlite::types::Value::Real(v) => {
                    hasher.update([2]);
                    hasher.update(v.to_bits().to_le_bytes());
                }
                rusqlite::types::Value::Text(v) => {
                    hasher.update([3]);
                    let len = u64::try_from(v.len()).unwrap_or(u64::MAX);
                    hasher.update(len.to_le_bytes());
                    hasher.update(v.as_bytes());
                }
                rusqlite::types::Value::Blob(v) => {
                    hasher.update([4]);
                    let len = u64::try_from(v.len()).unwrap_or(u64::MAX);
                    hasher.update(len.to_le_bytes());
                    hasher.update(&v);
                }
            }
        }
        hasher.update([0xFF]);
    }
    Ok(hasher.finalize().to_vec())
}

pub(crate) fn u64_to_sqlite_i64(field: &'static str, value: u64) -> Result<i64, StoreError> {
    i64::try_from(value).map_err(|_| StoreError::IntegerOutOfRange { field, value })
}

pub(crate) fn u64_from_nonnegative_i64(field: &'static str, value: i64) -> Result<u64, StoreError> {
    u64::try_from(value).map_err(|_| StoreError::IntegerOutOfRange {
        field,
        value: value.unsigned_abs(),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EventLogBounds {
    pub pruned_through_sequence: u64,
    pub newest_sequence: u64,
}

/// Read the explicit replay boundary and durable high-water from one SQLite view.
///
/// Missing event sequences are ordinary gaps. Only the singleton retention row
/// declares history unavailable, so this intentionally never consults `MIN(sequence)`.
pub(crate) fn load_event_log_bounds(conn: &Connection) -> Result<EventLogBounds, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT singleton_key, pruned_through_sequence
         FROM event_retention ORDER BY singleton_key",
    )?;
    let mut rows = stmt.query([])?;
    let Some(row) = rows.next()? else {
        return Err(StoreError::Corruption);
    };
    let singleton_key: rusqlite::types::Value = row.get(0)?;
    let pruned_through: rusqlite::types::Value = row.get(1)?;
    if rows.next()?.is_some() {
        return Err(StoreError::Corruption);
    }
    let (rusqlite::types::Value::Integer(1), rusqlite::types::Value::Integer(pruned_through)) =
        (singleton_key, pruned_through)
    else {
        return Err(StoreError::Corruption);
    };
    let pruned_through_sequence =
        u64::try_from(pruned_through).map_err(|_| StoreError::Corruption)?;
    let max_stored: i64 =
        conn.query_row("SELECT COALESCE(MAX(sequence), 0) FROM events", [], |row| {
            row.get(0)
        })?;
    let max_stored_sequence = u64_from_nonnegative_i64("events.sequence", max_stored)?;
    Ok(EventLogBounds {
        pruned_through_sequence,
        newest_sequence: pruned_through_sequence.max(max_stored_sequence),
    })
}

pub(crate) fn now_ms() -> Result<i64, StoreError> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| StoreError::Io(e.to_string()))?;
    i64::try_from(duration.as_millis()).map_err(|_| StoreError::IntegerOutOfRange {
        field: "applied_at_ms",
        value: u64::MAX,
    })
}

fn event_id_from_bytes(bytes: &[u8]) -> Result<EventId, StoreError> {
    let array: [u8; 16] = bytes
        .try_into()
        .map_err(|_| StoreError::EventDecode("event_id must be 16 bytes".into()))?;
    EventId::from_bytes(array).map_err(|e| StoreError::EventDecode(e.to_string()))
}

fn task_id_from_bytes(bytes: &[u8]) -> Result<TaskId, StoreError> {
    let array: [u8; 16] = bytes
        .try_into()
        .map_err(|_| StoreError::EventDecode("task_id must be 16 bytes".into()))?;
    TaskId::from_bytes(array).map_err(|e| StoreError::EventDecode(e.to_string()))
}

pub(crate) fn encode_event_payload(event: &Event) -> Result<Vec<u8>, StoreError> {
    let bytes = match event {
        Event::TaskCreated {
            task,
            connectivity,
            attention,
            activity,
            review_readiness,
        } => rmp_serde::to_vec(&TaskCreatedPayload {
            task: task.clone(),
            connectivity: *connectivity,
            attention: *attention,
            activity: *activity,
            review_readiness: *review_readiness,
        }),
        Event::TaskRenamed { title } => rmp_serde::to_vec(&TaskRenamedPayload {
            title: title.clone(),
        }),
        Event::TaskAttentionSet { attention } => rmp_serde::to_vec(&TaskAttentionSetPayload {
            attention: *attention,
        }),
        Event::TaskCloseBegun { action_epoch } => rmp_serde::to_vec(&TaskCloseBegunPayload {
            action_epoch: *action_epoch,
        }),
        Event::TaskReopened => rmp_serde::to_vec(&TaskUnitPayload {}),
        Event::TaskArchived => rmp_serde::to_vec(&TaskUnitPayload {}),
        Event::AgentSessionRegistered { agent } => {
            rmp_serde::to_vec(&AgentSessionRegisteredPayload {
                agent: agent.clone(),
            })
        }
        Event::PrimaryAgentSet { agent_session_id } => rmp_serde::to_vec(&PrimaryAgentSetPayload {
            agent_session_id: *agent_session_id,
        }),
        Event::ArtifactRegistered { artifact } => rmp_serde::to_vec(&ArtifactRegisteredPayload {
            artifact: artifact.clone(),
        }),
        Event::ResourceRegistered { resource } => rmp_serde::to_vec(&ResourceRegisteredPayload {
            resource: resource.clone(),
        }),
        Event::ResourceReleaseBegun {
            resource_id,
            runtime_generation,
        } => rmp_serde::to_vec(&ResourceReleaseBegunPayload {
            resource_id: *resource_id,
            runtime_generation: *runtime_generation,
        }),
        Event::ResourceReleased {
            resource_id,
            runtime_generation,
        } => rmp_serde::to_vec(&ResourceReleasedPayload {
            resource_id: *resource_id,
            runtime_generation: *runtime_generation,
        }),
        Event::HostCloseBegun {
            operation_id,
            action_epoch,
            inspection_id,
        } => rmp_serde::to_vec(&HostCloseBegunPayload {
            operation_id: *operation_id,
            action_epoch: *action_epoch,
            inspection_id: *inspection_id,
        }),
        Event::HostCleanupBranchCompleted {
            operation_id,
            action_epoch,
            branch,
            outcome,
        } => rmp_serde::to_vec(&HostCleanupBranchCompletedPayload {
            operation_id: *operation_id,
            action_epoch: *action_epoch,
            branch: *branch,
            outcome: *outcome,
        }),
        Event::OperationAccepted(fact) => rmp_serde::to_vec(fact),
        Event::OperationSettled(fact) => rmp_serde::to_vec(fact),
        Event::OperationFailed(fact) => rmp_serde::to_vec(fact),
        Event::OperationCancelled(fact) => rmp_serde::to_vec(fact),
        Event::OperationUncertain(fact) => rmp_serde::to_vec(fact),
        Event::ProviderInputAccepted {
            command_id,
            client_id,
            operation_id,
            agent_session_id,
            provider_kind,
            provider_session_id,
            runtime_generation,
            turn_id,
            action_epoch,
            question_id,
            approval_id,
            action,
            wait,
            delivery,
        } => rmp_serde::to_vec(&ProviderInputAcceptedPayload {
            command_id: *command_id,
            client_id: *client_id,
            operation_id: *operation_id,
            agent_session_id: *agent_session_id,
            provider_kind: provider_kind.clone(),
            provider_session_id: provider_session_id.clone(),
            runtime_generation: *runtime_generation,
            turn_id: *turn_id,
            action_epoch: *action_epoch,
            question_id: *question_id,
            approval_id: *approval_id,
            action: action.clone(),
            wait: *wait,
            delivery: *delivery,
        }),
        Event::ProviderQuestionPresented {
            agent_session_id,
            provider_kind,
            provider_session_id,
            runtime_generation,
            turn_id,
            action_epoch,
            question_id,
        } => rmp_serde::to_vec(&ProviderQuestionPresentedPayload {
            agent_session_id: *agent_session_id,
            provider_kind: provider_kind.clone(),
            provider_session_id: provider_session_id.clone(),
            runtime_generation: *runtime_generation,
            turn_id: *turn_id,
            action_epoch: *action_epoch,
            question_id: *question_id,
        }),
        Event::ProviderApprovalPresented {
            agent_session_id,
            provider_kind,
            provider_session_id,
            runtime_generation,
            turn_id,
            action_epoch,
            approval_id,
        } => rmp_serde::to_vec(&ProviderApprovalPresentedPayload {
            agent_session_id: *agent_session_id,
            provider_kind: provider_kind.clone(),
            provider_session_id: provider_session_id.clone(),
            runtime_generation: *runtime_generation,
            turn_id: *turn_id,
            action_epoch: *action_epoch,
            approval_id: *approval_id,
        }),
        Event::ProviderWaitSettled { fence } => rmp_serde::to_vec(&ProviderWaitSettledPayload {
            fence: fence.clone(),
        }),
        Event::ProviderInputDelivered {
            command_id,
            client_id,
            operation_id,
            agent_session_id,
            provider_kind,
            provider_session_id,
            runtime_generation,
            turn_id,
            action_epoch,
            question_id,
            approval_id,
        } => rmp_serde::to_vec(&ProviderInputDeliveredPayload {
            command_id: *command_id,
            client_id: *client_id,
            operation_id: *operation_id,
            agent_session_id: *agent_session_id,
            provider_kind: provider_kind.clone(),
            provider_session_id: provider_session_id.clone(),
            runtime_generation: *runtime_generation,
            turn_id: *turn_id,
            action_epoch: *action_epoch,
            question_id: *question_id,
            approval_id: *approval_id,
        }),
    }
    .map_err(|e| StoreError::EventDecode(e.to_string()))?;
    if event.event_type().starts_with("provider_input.")
        && bytes.len() > MAX_PROVIDER_EVENT_PAYLOAD_BYTES
    {
        return Err(StoreError::EventDecode(format!(
            "provider event payload exceeds {MAX_PROVIDER_EVENT_PAYLOAD_BYTES} bytes"
        )));
    }
    Ok(bytes)
}

pub(crate) fn decode_stored_event(
    event_type: &str,
    schema_version: i64,
    payload: &[u8],
) -> Result<Event, StoreError> {
    if schema_version != i64::from(EVENT_SCHEMA_VERSION) {
        return Err(StoreError::CodecMismatch {
            detail: format!("schema_version column {schema_version} != {EVENT_SCHEMA_VERSION}"),
        });
    }
    if event_type.starts_with("provider_input.") && payload.len() > MAX_PROVIDER_EVENT_PAYLOAD_BYTES
    {
        return Err(StoreError::CodecMismatch {
            detail: format!(
                "provider event payload exceeds {MAX_PROVIDER_EVENT_PAYLOAD_BYTES} bytes"
            ),
        });
    }

    let event = match event_type {
        "task.created" => {
            let p: TaskCreatedPayload = unpack(payload)?;
            p.task
                .validate_for_create()
                .map_err(|e| StoreError::EventDecode(e.to_string()))?;
            Event::TaskCreated {
                task: p.task,
                connectivity: p.connectivity,
                attention: p.attention,
                activity: p.activity,
                review_readiness: p.review_readiness,
            }
        }
        "task.renamed" => {
            let p: TaskRenamedPayload = unpack(payload)?;
            let title = crate::domain::task::TaskFacts::canonicalize_title(p.title)
                .map_err(|e| StoreError::EventDecode(e.to_string()))?;
            Event::TaskRenamed { title }
        }
        "task.attention_set" => {
            let p: TaskAttentionSetPayload = unpack(payload)?;
            Event::TaskAttentionSet {
                attention: p.attention,
            }
        }
        "task.close_begun" => {
            let p: TaskCloseBegunPayload = unpack(payload)?;
            Event::TaskCloseBegun {
                action_epoch: p.action_epoch,
            }
        }
        "task.reopened" => {
            let _: TaskUnitPayload = unpack(payload)?;
            Event::TaskReopened
        }
        "task.archived" => {
            let _: TaskUnitPayload = unpack(payload)?;
            Event::TaskArchived
        }
        "agent_session.registered" => {
            let p: AgentSessionRegisteredPayload = unpack(payload)?;
            p.agent
                .validate_for_registration()
                .map_err(|e| StoreError::EventDecode(e.to_string()))?;
            Event::AgentSessionRegistered { agent: p.agent }
        }
        "primary_agent.set" => {
            let p: PrimaryAgentSetPayload = unpack(payload)?;
            Event::PrimaryAgentSet {
                agent_session_id: p.agent_session_id,
            }
        }
        "artifact.registered" => {
            let p: ArtifactRegisteredPayload = unpack(payload)?;
            p.artifact
                .validate()
                .map_err(|e| StoreError::EventDecode(e.to_string()))?;
            Event::ArtifactRegistered {
                artifact: p.artifact,
            }
        }
        "resource.registered" => {
            let p: ResourceRegisteredPayload = unpack(payload)?;
            p.resource
                .validate_for_registration()
                .map_err(|e| StoreError::EventDecode(e.to_string()))?;
            Event::ResourceRegistered {
                resource: p.resource,
            }
        }
        "resource.release_begun" => {
            let p: ResourceReleaseBegunPayload = unpack(payload)?;
            Event::ResourceReleaseBegun {
                resource_id: p.resource_id,
                runtime_generation: p.runtime_generation,
            }
        }
        "resource.released" => {
            let p: ResourceReleasedPayload = unpack(payload)?;
            Event::ResourceReleased {
                resource_id: p.resource_id,
                runtime_generation: p.runtime_generation,
            }
        }
        "host.close_begun" => {
            let p: HostCloseBegunPayload = unpack(payload)?;
            Event::HostCloseBegun {
                operation_id: p.operation_id,
                action_epoch: p.action_epoch,
                inspection_id: p.inspection_id,
            }
        }
        "host.cleanup_branch_completed" => {
            let p: HostCleanupBranchCompletedPayload = unpack(payload)?;
            Event::HostCleanupBranchCompleted {
                operation_id: p.operation_id,
                action_epoch: p.action_epoch,
                branch: p.branch,
                outcome: p.outcome,
            }
        }
        "operation.accepted" => {
            let fact: OperationAcceptedFact = unpack(payload)?;
            Event::OperationAccepted(fact)
        }
        "operation.settled" => {
            let fact: OperationSettledFact = unpack(payload)?;
            Event::OperationSettled(fact)
        }
        "operation.failed" => {
            let fact: OperationFailedFact = unpack(payload)?;
            Event::OperationFailed(fact)
        }
        "operation.cancelled" => {
            let fact: OperationCancelledFact = unpack(payload)?;
            Event::OperationCancelled(fact)
        }
        "operation.uncertain" => {
            let fact: OperationUncertainFact = unpack(payload)?;
            Event::OperationUncertain(fact)
        }
        "provider_input.accepted" => {
            let p: ProviderInputAcceptedPayload = unpack(payload)?;
            Event::ProviderInputAccepted {
                command_id: p.command_id,
                client_id: p.client_id,
                operation_id: p.operation_id,
                agent_session_id: p.agent_session_id,
                provider_kind: p.provider_kind,
                provider_session_id: p.provider_session_id,
                runtime_generation: p.runtime_generation,
                turn_id: p.turn_id,
                action_epoch: p.action_epoch,
                question_id: p.question_id,
                approval_id: p.approval_id,
                action: p.action,
                wait: p.wait,
                delivery: p.delivery,
            }
        }
        "provider_input.question_presented" => {
            let p: ProviderQuestionPresentedPayload = unpack(payload)?;
            Event::ProviderQuestionPresented {
                agent_session_id: p.agent_session_id,
                provider_kind: p.provider_kind,
                provider_session_id: p.provider_session_id,
                runtime_generation: p.runtime_generation,
                turn_id: p.turn_id,
                action_epoch: p.action_epoch,
                question_id: p.question_id,
            }
        }
        "provider_input.approval_presented" => {
            let p: ProviderApprovalPresentedPayload = unpack(payload)?;
            Event::ProviderApprovalPresented {
                agent_session_id: p.agent_session_id,
                provider_kind: p.provider_kind,
                provider_session_id: p.provider_session_id,
                runtime_generation: p.runtime_generation,
                turn_id: p.turn_id,
                action_epoch: p.action_epoch,
                approval_id: p.approval_id,
            }
        }
        "provider_input.wait_settled" => {
            let p: ProviderWaitSettledPayload = unpack(payload)?;
            Event::ProviderWaitSettled { fence: p.fence }
        }
        "provider_input.delivered" => {
            let p: ProviderInputDeliveredPayload = unpack(payload)?;
            Event::ProviderInputDelivered {
                command_id: p.command_id,
                client_id: p.client_id,
                operation_id: p.operation_id,
                agent_session_id: p.agent_session_id,
                provider_kind: p.provider_kind,
                provider_session_id: p.provider_session_id,
                runtime_generation: p.runtime_generation,
                turn_id: p.turn_id,
                action_epoch: p.action_epoch,
                question_id: p.question_id,
                approval_id: p.approval_id,
            }
        }
        other => {
            return Err(StoreError::CodecMismatch {
                detail: format!("unknown event_type column '{other}'"),
            })
        }
    };

    if event.event_type() != event_type {
        return Err(StoreError::CodecMismatch {
            detail: format!(
                "decoded type '{}' disagrees with column '{}'",
                event.event_type(),
                event_type
            ),
        });
    }
    match &event {
        Event::ProviderInputAccepted {
            command_id,
            operation_id,
            agent_session_id,
            provider_kind,
            provider_session_id,
            runtime_generation,
            turn_id,
            action_epoch,
            question_id,
            approval_id,
            action,
            wait,
            delivery,
            ..
        } => {
            if delivery.is_delivered() {
                return Err(StoreError::CodecMismatch {
                    detail: "provider input accepted event cannot claim delivery".into(),
                });
            }
            let fence = crate::domain::provider_input::ProviderFenceIdentity::new_with_identity(
                Some(*command_id),
                None,
                *agent_session_id,
                provider_kind.clone(),
                provider_session_id.clone(),
                Some(*operation_id),
                *runtime_generation,
                *action_epoch,
                *turn_id,
                *question_id,
                *approval_id,
            );
            crate::domain::provider_input::validate_provider_fence(
                &fence,
                Some(action),
                Some(*wait),
                None,
            )
            .map_err(|err| StoreError::CodecMismatch {
                detail: err.to_string(),
            })?;
        }
        Event::ProviderQuestionPresented {
            agent_session_id,
            provider_kind,
            provider_session_id,
            runtime_generation,
            turn_id,
            action_epoch,
            question_id,
        } => {
            let fence = crate::domain::provider_input::ProviderFenceIdentity::new_with_identity(
                None,
                None,
                *agent_session_id,
                provider_kind.clone(),
                provider_session_id.clone(),
                None,
                *runtime_generation,
                *action_epoch,
                *turn_id,
                Some(*question_id),
                None,
            );
            crate::domain::provider_input::validate_provider_fence(&fence, None, None, None)
                .map_err(|err| StoreError::CodecMismatch {
                    detail: err.to_string(),
                })?;
        }
        Event::ProviderApprovalPresented {
            agent_session_id,
            provider_kind,
            provider_session_id,
            runtime_generation,
            turn_id,
            action_epoch,
            approval_id,
        } => {
            let fence = crate::domain::provider_input::ProviderFenceIdentity::new_with_identity(
                None,
                None,
                *agent_session_id,
                provider_kind.clone(),
                provider_session_id.clone(),
                None,
                *runtime_generation,
                *action_epoch,
                *turn_id,
                None,
                Some(*approval_id),
            );
            crate::domain::provider_input::validate_provider_fence(&fence, None, None, None)
                .map_err(|err| StoreError::CodecMismatch {
                    detail: err.to_string(),
                })?;
        }
        Event::ProviderWaitSettled { fence } => {
            crate::domain::provider_input::validate_provider_fence(
                &fence.identity(),
                None,
                None,
                None,
            )
            .map_err(|err| StoreError::CodecMismatch {
                detail: err.to_string(),
            })?;
        }
        Event::ProviderInputDelivered {
            command_id,
            operation_id,
            agent_session_id,
            provider_kind,
            provider_session_id,
            runtime_generation,
            turn_id,
            action_epoch,
            question_id,
            approval_id,
            ..
        } => {
            let fence = crate::domain::provider_input::ProviderFenceIdentity::new_with_identity(
                Some(*command_id),
                None,
                *agent_session_id,
                provider_kind.clone(),
                provider_session_id.clone(),
                Some(*operation_id),
                *runtime_generation,
                *action_epoch,
                *turn_id,
                *question_id,
                *approval_id,
            );
            crate::domain::provider_input::validate_provider_fence(&fence, None, None, None)
                .map_err(|err| StoreError::CodecMismatch {
                    detail: err.to_string(),
                })?;
        }
        _ => {}
    }
    Ok(event)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn decode_stored_domain_event(
    sequence: i64,
    event_id_bytes: &[u8],
    task_id_bytes: Option<&[u8]>,
    task_revision: Option<i64>,
    event_type: &str,
    schema_version: i64,
    occurred_at_ms: i64,
    payload: &[u8],
) -> Result<DomainEvent, StoreError> {
    let task_id = match task_id_bytes {
        Some(bytes) => Some(task_id_from_bytes(bytes)?),
        None => None,
    };
    let payload = decode_stored_event(event_type, schema_version, payload)?;
    validate_provider_event_task_identity(&payload, task_id)?;
    Ok(DomainEvent {
        id: event_id_from_bytes(event_id_bytes)?,
        task_id,
        sequence: u64_from_nonnegative_i64("events.sequence", sequence)?,
        task_revision: match task_revision {
            Some(value) => Some(u64_from_nonnegative_i64("events.task_revision", value)?),
            None => None,
        },
        occurred_at_ms,
        payload,
    })
}

fn validate_provider_event_task_identity(
    event: &Event,
    task_id: Option<TaskId>,
) -> Result<(), StoreError> {
    let Some((identity, action, wait)) = (match event {
        Event::ProviderInputAccepted {
            command_id,
            operation_id,
            agent_session_id,
            provider_kind,
            provider_session_id,
            runtime_generation,
            turn_id,
            action_epoch,
            question_id,
            approval_id,
            action,
            wait,
            ..
        } => Some((
            crate::domain::provider_input::ProviderFenceIdentity::new_with_identity(
                Some(*command_id),
                task_id,
                *agent_session_id,
                provider_kind.clone(),
                provider_session_id.clone(),
                Some(*operation_id),
                *runtime_generation,
                *action_epoch,
                *turn_id,
                *question_id,
                *approval_id,
            ),
            Some(action),
            Some(*wait),
        )),
        Event::ProviderQuestionPresented {
            agent_session_id,
            provider_kind,
            provider_session_id,
            runtime_generation,
            turn_id,
            action_epoch,
            question_id,
        } => Some((
            crate::domain::provider_input::ProviderFenceIdentity::new_with_identity(
                None,
                task_id,
                *agent_session_id,
                provider_kind.clone(),
                provider_session_id.clone(),
                None,
                *runtime_generation,
                *action_epoch,
                *turn_id,
                Some(*question_id),
                None,
            ),
            None,
            None,
        )),
        Event::ProviderApprovalPresented {
            agent_session_id,
            provider_kind,
            provider_session_id,
            runtime_generation,
            turn_id,
            action_epoch,
            approval_id,
        } => Some((
            crate::domain::provider_input::ProviderFenceIdentity::new_with_identity(
                None,
                task_id,
                *agent_session_id,
                provider_kind.clone(),
                provider_session_id.clone(),
                None,
                *runtime_generation,
                *action_epoch,
                *turn_id,
                None,
                Some(*approval_id),
            ),
            None,
            None,
        )),
        Event::ProviderWaitSettled { fence } => {
            if task_id != Some(fence.task_id()) {
                return Err(StoreError::CodecMismatch {
                    detail: "provider wait fence task identity disagrees with event scope".into(),
                });
            }
            Some((fence.identity(), None, None))
        }
        Event::ProviderInputDelivered {
            command_id,
            operation_id,
            agent_session_id,
            provider_kind,
            provider_session_id,
            runtime_generation,
            turn_id,
            action_epoch,
            question_id,
            approval_id,
            ..
        } => Some((
            crate::domain::provider_input::ProviderFenceIdentity::new_with_identity(
                Some(*command_id),
                task_id,
                *agent_session_id,
                provider_kind.clone(),
                provider_session_id.clone(),
                Some(*operation_id),
                *runtime_generation,
                *action_epoch,
                *turn_id,
                *question_id,
                *approval_id,
            ),
            None,
            None,
        )),
        _ => None,
    }) else {
        return Ok(());
    };
    if identity.task_id.is_none() {
        return Err(StoreError::CodecMismatch {
            detail: "provider event requires a task scope".into(),
        });
    }
    if matches!(
        event,
        Event::ProviderInputAccepted { .. }
            | Event::ProviderWaitSettled { .. }
            | Event::ProviderInputDelivered { .. }
    ) && identity.operation_id.is_none()
    {
        return Err(StoreError::CodecMismatch {
            detail: "provider acceptance/wait event requires operation identity".into(),
        });
    }
    crate::domain::provider_input::validate_provider_fence(&identity, action, wait, None).map_err(
        |err| StoreError::CodecMismatch {
            detail: err.to_string(),
        },
    )
}

fn unpack<T: serde::de::DeserializeOwned>(payload: &[u8]) -> Result<T, StoreError> {
    rmp_serde::from_slice(payload).map_err(|err| StoreError::CodecMismatch {
        detail: err.to_string(),
    })
}

fn validate_dispatch_lease_ms(lease: Duration) -> Result<i64, StoreError> {
    if lease.is_zero() {
        return Err(StoreError::InvalidLeaseDuration);
    }
    let ms = i64::try_from(lease.as_millis()).map_err(|_| StoreError::InvalidLeaseDuration)?;
    if ms <= 0 || ms > MAX_DISPATCH_LEASE_MS {
        return Err(StoreError::InvalidLeaseDuration);
    }
    Ok(ms)
}

fn lease_deadline(now_ms: i64, lease_ms: i64) -> Result<i64, StoreError> {
    now_ms
        .checked_add(lease_ms)
        .ok_or(StoreError::IntegerOutOfRange {
            field: "leased_until_ms",
            value: u64::MAX,
        })
}

fn renewed_lease_deadline(
    now_ms: i64,
    prior_deadline_ms: i64,
    lease_ms: i64,
) -> Result<i64, StoreError> {
    Ok(prior_deadline_ms.max(lease_deadline(now_ms, lease_ms)?))
}

fn verify_claim_generation(
    row: &OutboxRow,
    claim: &DispatchClaim,
    now_ms: i64,
) -> Result<(), StoreError> {
    if row.lease_generation != claim.lease_generation() {
        return Err(StoreError::StaleClaim);
    }
    let leased_until = row.leased_until_ms.ok_or(StoreError::Corruption)?;
    if leased_until <= now_ms {
        return Err(StoreError::ExpiredClaim);
    }
    Ok(())
}

fn revalidate_outbox_effect(
    tx: &Transaction<'_>,
    row: &OutboxRow,
) -> Result<
    (
        crate::kernel::outbox::PlannedEffectDocument,
        crate::kernel::outbox::OperationFence,
    ),
    StoreError,
> {
    validate_dispatch_candidate_lineage(tx, row.operation_id, row.outbox_id)
}

fn revalidate_outbox_attempt_effect(
    tx: &Transaction<'_>,
    row: &OutboxRow,
) -> Result<
    (
        crate::kernel::outbox::PlannedEffectDocument,
        crate::kernel::outbox::OperationFence,
    ),
    StoreError,
> {
    validate_dispatch_attempt_lineage(tx, row.operation_id, row.outbox_id)
}

fn is_provider_uncertainty_effect(
    effect_doc: &crate::kernel::outbox::PlannedEffectDocument,
) -> bool {
    effect_doc.replay_policy == ReplayPolicy::NoAutomaticRetry
        && matches!(&effect_doc.effect, Effect::DeliverProviderInput { .. })
}

/// Current ownership is required before an external boundary starts. Once it
/// has started, only a typed no-retry provider effect may use its immutable
/// stored identity to record ambiguity after the provider closes or advances.
fn revalidate_ambiguity_effect(
    tx: &Transaction<'_>,
    row: &OutboxRow,
) -> Result<
    (
        crate::kernel::outbox::PlannedEffectDocument,
        crate::kernel::outbox::OperationFence,
    ),
    StoreError,
> {
    // Only the durable provider destination is allowed to use immutable
    // post-boundary identity. Generic RetrySafe and ReconcileBeforeRetry
    // effects must stay on the strict live-ownership validator; attempting
    // provider validation first would turn valid generic ambiguity into a
    // false corruption result (or create an ownership bypass).
    if row.destination_class == DestinationClass::ProviderInput.as_str()
        && row.replay_policy == ReplayPolicy::NoAutomaticRetry.as_str()
    {
        let immutable = revalidate_outbox_attempt_effect(tx, row)?;
        if is_provider_uncertainty_effect(&immutable.0) {
            return Ok(immutable);
        }
        return Err(StoreError::Corruption);
    }
    // Generic effects retain their pre-existing current-ownership rule.
    revalidate_outbox_effect(tx, row)
}

fn parse_outbox_id(bytes: &[u8]) -> Result<OutboxId, StoreError> {
    let array: [u8; 16] = bytes.try_into().map_err(|_| StoreError::Corruption)?;
    OutboxId::from_bytes(array).map_err(|_| StoreError::Corruption)
}

fn parse_operation_id(bytes: &[u8]) -> Result<crate::domain::id::OperationId, StoreError> {
    let array: [u8; 16] = bytes.try_into().map_err(|_| StoreError::Corruption)?;
    crate::domain::id::OperationId::from_bytes(array).map_err(|_| StoreError::Corruption)
}

fn load_next_dispatch_candidate(
    tx: &Transaction<'_>,
    now_ms: i64,
    after: Option<&OutboxRow>,
) -> Result<Option<OutboxRow>, StoreError> {
    let after_available_at = after.map(|row| row.available_at_ms);
    let after_event_sequence = after
        .map(|row| u64_to_sqlite_i64("outbox.event_sequence", row.event_sequence))
        .transpose()?;
    let after_effect_index = after.map(|row| row.effect_index);
    let after_outbox_id = after.map(|row| row.outbox_id.as_bytes().as_slice());
    let row: Option<(
        Vec<u8>,
        Vec<u8>,
        i64,
        i64,
        String,
        String,
        Vec<u8>,
        String,
        i64,
        Option<i64>,
        Option<i64>,
        i64,
        Option<String>,
        i64,
        Option<Vec<u8>>,
        Option<Vec<u8>>,
    )> = tx
        .query_row(
            "SELECT o.outbox_id, o.operation_id, o.effect_index, o.event_sequence,
                    o.destination_class, o.replay_policy, o.payload, o.state,
                    o.available_at_ms, o.leased_until_ms, o.dispatch_started_at_ms,
                    o.attempts, o.last_error_class, o.lease_generation,
                    o.reconciliation_receipt, o.compacted_payload_sha256
             FROM outbox o
             JOIN operations op ON op.operation_id = o.operation_id
             WHERE op.state = 'accepted'
               AND (
                 (o.state = 'pending' AND o.available_at_ms <= ?1)
                 OR (
                   o.state = 'claimed'
                   AND o.available_at_ms <= ?1
                   AND (o.leased_until_ms IS NULL OR o.leased_until_ms <= ?1)
                 )
               )
               AND (
                 ?2 IS NULL
                 OR o.available_at_ms > ?2
                 OR (o.available_at_ms = ?2 AND o.event_sequence > ?3)
                 OR (
                   o.available_at_ms = ?2 AND o.event_sequence = ?3
                   AND o.effect_index > ?4
                 )
                 OR (
                   o.available_at_ms = ?2 AND o.event_sequence = ?3
                   AND o.effect_index = ?4 AND o.outbox_id > ?5
                 )
               )
             ORDER BY o.available_at_ms ASC, o.event_sequence ASC,
                      o.effect_index ASC, o.outbox_id ASC
             LIMIT 1",
            rusqlite::params![
                now_ms,
                after_available_at,
                after_event_sequence,
                after_effect_index,
                after_outbox_id,
            ],
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
                    row.get(8)?,
                    row.get(9)?,
                    row.get(10)?,
                    row.get(11)?,
                    row.get(12)?,
                    row.get(13)?,
                    row.get(14)?,
                    row.get(15)?,
                ))
            },
        )
        .optional()?;
    let Some((
        outbox_id_bytes,
        operation_id_bytes,
        effect_index,
        event_sequence,
        destination_class,
        replay_policy,
        payload,
        state,
        available_at_ms,
        leased_until_ms,
        dispatch_started_at_ms,
        attempts,
        last_error_class,
        lease_generation,
        reconciliation_receipt,
        compacted_payload_sha256,
    )) = row
    else {
        return Ok(None);
    };
    if event_sequence < 0 || lease_generation < 0 {
        return Err(StoreError::Corruption);
    }
    Ok(Some(OutboxRow {
        outbox_id: parse_outbox_id(&outbox_id_bytes)?,
        operation_id: parse_operation_id(&operation_id_bytes)?,
        effect_index,
        event_sequence: u64_from_nonnegative_i64("outbox.event_sequence", event_sequence)?,
        destination_class,
        replay_policy,
        payload,
        state,
        available_at_ms,
        leased_until_ms,
        dispatch_started_at_ms,
        attempts,
        last_error_class,
        lease_generation,
        reconciliation_receipt,
        compacted_payload_sha256,
    }))
}

fn load_next_process_empty_task_teardown_candidate(
    tx: &Transaction<'_>,
    now_ms: i64,
    after: Option<&OutboxRow>,
) -> Result<Option<OutboxRow>, StoreError> {
    let after_available_at = after.map(|row| row.available_at_ms);
    let after_event_sequence = after
        .map(|row| u64_to_sqlite_i64("outbox.event_sequence", row.event_sequence))
        .transpose()?;
    let after_effect_index = after.map(|row| row.effect_index);
    let after_outbox_id = after.map(|row| row.outbox_id.as_bytes().as_slice());
    let outbox_id_bytes: Option<Vec<u8>> = tx
        .query_row(
            "SELECT o.outbox_id
             FROM outbox o
             JOIN operations op ON op.operation_id = o.operation_id
             JOIN tasks t ON t.task_id = op.task_id
             WHERE op.state = 'accepted'
               AND o.destination_class = 'task_teardown'
               AND t.lifecycle = 'closing'
               AND op.action_epoch IS NOT NULL
               AND t.action_epoch = op.action_epoch
               AND (
                 (o.state = 'pending' AND o.available_at_ms <= ?1)
                 OR (
                   o.state = 'claimed'
                   AND o.available_at_ms <= ?1
                   AND (o.leased_until_ms IS NULL OR o.leased_until_ms <= ?1)
                 )
               )
               AND (
                 ?2 IS NULL
                 OR o.available_at_ms > ?2
                 OR (o.available_at_ms = ?2 AND o.event_sequence > ?3)
                 OR (
                   o.available_at_ms = ?2 AND o.event_sequence = ?3
                   AND o.effect_index > ?4
                 )
                 OR (
                   o.available_at_ms = ?2 AND o.event_sequence = ?3
                   AND o.effect_index = ?4 AND o.outbox_id > ?5
                 )
               )
               AND NOT EXISTS (
                 SELECT 1 FROM resources r
                 WHERE r.task_id = t.task_id
                   AND r.owner_kind = 'task'
                   AND r.lifecycle IN ('active', 'releasing')
               )
             ORDER BY o.available_at_ms ASC, o.event_sequence ASC,
                      o.effect_index ASC, o.outbox_id ASC
             LIMIT 1",
            rusqlite::params![
                now_ms,
                after_available_at,
                after_event_sequence,
                after_effect_index,
                after_outbox_id,
            ],
            |row| row.get(0),
        )
        .optional()?;
    let Some(outbox_id_bytes) = outbox_id_bytes else {
        return Ok(None);
    };
    let outbox_id = parse_outbox_id(&outbox_id_bytes)?;
    load_outbox_row_by_id(tx, outbox_id)?
        .ok_or(StoreError::Corruption)
        .map(Some)
}

fn build_dispatch_permit(
    row: &OutboxRow,
    effect_doc: crate::kernel::outbox::PlannedEffectDocument,
    fence: crate::kernel::outbox::OperationFence,
) -> Result<DispatchPermit, StoreError> {
    let effect_index = u32::try_from(row.effect_index).map_err(|_| StoreError::Corruption)?;
    let resource_fence = ResourceFence::from_parts(fence.resource_id, fence.runtime_generation)
        .map_err(|_| StoreError::Corruption)?;
    let attempt = u64_from_nonnegative_i64("outbox.attempts", row.attempts)?;
    Ok(DispatchPermit::new(
        row.outbox_id,
        row.lease_generation,
        row.operation_id,
        effect_index,
        attempt,
        effect_doc,
        external_idempotency_key(row.operation_id, effect_index),
        fence.action_epoch,
        resource_fence,
    ))
}

fn validate_dispatch_permit(
    row: &OutboxRow,
    permit: &DispatchPermit,
    effect_doc: &crate::kernel::outbox::PlannedEffectDocument,
    fence: crate::kernel::outbox::OperationFence,
) -> Result<(), StoreError> {
    let effect_index = u32::try_from(row.effect_index).map_err(|_| StoreError::Corruption)?;
    let attempt = u64_from_nonnegative_i64("outbox.attempts", row.attempts)?;
    let resource_fence = ResourceFence::from_parts(fence.resource_id, fence.runtime_generation)
        .map_err(|_| StoreError::Corruption)?;
    if row.outbox_id != permit.outbox_id()
        || row.lease_generation != permit.lease_generation()
        || row.operation_id != permit.operation_id()
        || effect_index != permit.effect_index()
        || attempt != permit.attempt()
        || effect_doc != permit.document()
        || external_idempotency_key(row.operation_id, effect_index)
            != permit.external_idempotency_key()
        || fence.action_epoch != permit.action_epoch()
        || resource_fence != permit.resource_fence()
    {
        return Err(StoreError::StaleClaim);
    }
    Ok(())
}

fn validate_absence_authorization(row: &OutboxRow) -> Result<(), StoreError> {
    let payload = row
        .reconciliation_receipt
        .as_deref()
        .ok_or(StoreError::Corruption)?;
    let receipt = decode_absence_receipt(payload)?;
    let effect_index = u32::try_from(row.effect_index).map_err(|_| StoreError::Corruption)?;
    let completed_attempt = u64_from_nonnegative_i64("outbox.attempts", row.attempts)?;
    if !receipt.authorizes(
        row.outbox_id,
        row.operation_id,
        effect_index,
        completed_attempt,
        &external_idempotency_key(row.operation_id, effect_index),
        row.dispatch_started_at_ms.ok_or(StoreError::Corruption)?,
        row.available_at_ms,
    ) {
        return Err(StoreError::Corruption);
    }
    Ok(())
}

fn pending_dispatch_is_authorized(
    row: &OutboxRow,
    replay_policy: ReplayPolicy,
) -> Result<bool, StoreError> {
    if row.attempts == 0 {
        return Ok(true);
    }
    if row.lease_generation == 0 {
        return Ok(false);
    }
    match replay_policy {
        ReplayPolicy::RetrySafe => {
            if row.reconciliation_receipt.is_some() {
                return Err(StoreError::Corruption);
            }
            Ok(true)
        }
        ReplayPolicy::ReconcileBeforeRetry => {
            validate_absence_authorization(row)?;
            Ok(true)
        }
        ReplayPolicy::NoAutomaticRetry => Ok(false),
    }
}

fn build_reconciliation_claim(
    row: &OutboxRow,
    origin: ReconciliationOrigin,
    effect_doc: crate::kernel::outbox::PlannedEffectDocument,
    fence: crate::kernel::outbox::OperationFence,
) -> Result<ReconciliationClaim, StoreError> {
    let effect_index = u32::try_from(row.effect_index).map_err(|_| StoreError::Corruption)?;
    let completed_attempt = u64_from_nonnegative_i64("outbox.attempts", row.attempts)?;
    if completed_attempt == 0 {
        return Err(StoreError::Corruption);
    }
    let resource_fence = ResourceFence::from_parts(fence.resource_id, fence.runtime_generation)
        .map_err(|_| StoreError::Corruption)?;
    Ok(ReconciliationClaim::new(
        row.outbox_id,
        row.lease_generation,
        row.operation_id,
        effect_index,
        completed_attempt,
        origin,
        effect_doc,
        external_idempotency_key(row.operation_id, effect_index),
        fence.action_epoch,
        resource_fence,
    ))
}

fn validate_reconciliation_claim(
    row: &OutboxRow,
    claim: &ReconciliationClaim,
    effect_doc: &crate::kernel::outbox::PlannedEffectDocument,
    fence: crate::kernel::outbox::OperationFence,
) -> Result<(), StoreError> {
    validate_reconciliation_claim_identity(row, claim, effect_doc)?;
    let resource_fence = ResourceFence::from_parts(fence.resource_id, fence.runtime_generation)
        .map_err(|_| StoreError::Corruption)?;
    if fence.action_epoch != claim.action_epoch() || resource_fence != claim.resource_fence() {
        return Err(StoreError::StaleClaim);
    }
    Ok(())
}

fn validate_reconciliation_claim_identity(
    row: &OutboxRow,
    claim: &ReconciliationClaim,
    effect_doc: &crate::kernel::outbox::PlannedEffectDocument,
) -> Result<(), StoreError> {
    let effect_index = u32::try_from(row.effect_index).map_err(|_| StoreError::Corruption)?;
    let attempt = u64_from_nonnegative_i64("outbox.attempts", row.attempts)?;
    if row.outbox_id != claim.outbox_id()
        || row.lease_generation != claim.lease_generation()
        || row.operation_id != claim.operation_id()
        || effect_index != claim.effect_index()
        || attempt != claim.completed_attempt()
        || effect_doc != claim.document()
        || external_idempotency_key(row.operation_id, effect_index) != claim.lookup_identity()
    {
        return Err(StoreError::StaleClaim);
    }
    Ok(())
}

fn claim_outbox_row(
    tx: &Transaction<'_>,
    row: &OutboxRow,
    now_ms: i64,
    lease_ms: i64,
    expected_state: &str,
) -> Result<DispatchClaim, StoreError> {
    let leased_until = lease_deadline(now_ms, lease_ms)?;
    let next_generation = row
        .lease_generation
        .checked_add(1)
        .ok_or(StoreError::Corruption)?;
    let changed = tx.execute(
        "UPDATE outbox
         SET state = 'claimed', lease_generation = ?1, leased_until_ms = ?2
         WHERE outbox_id = ?3 AND state = ?4 AND lease_generation = ?5",
        rusqlite::params![
            next_generation,
            leased_until,
            row.outbox_id.as_bytes().as_slice(),
            expected_state,
            row.lease_generation,
        ],
    )?;
    if changed != 1 {
        return Err(StoreError::InvalidDispatchTransition);
    }
    Ok(DispatchClaim::new(row.outbox_id, next_generation))
}

fn claim_next_dispatch_in_tx(
    tx: &Transaction<'_>,
    now_ms: i64,
    lease_ms: i64,
) -> Result<Option<DispatchClaim>, StoreError> {
    let mut prior_candidate = None;
    let mut saw_stale_fence = false;
    loop {
        let Some(row) = load_next_dispatch_candidate(tx, now_ms, prior_candidate.as_ref())? else {
            return if saw_stale_fence {
                Err(StoreError::StaleFence)
            } else {
                Ok(None)
            };
        };
        let effect_doc = match revalidate_outbox_effect(tx, &row) {
            Ok((effect_doc, _)) => effect_doc,
            Err(StoreError::StaleFence) => {
                saw_stale_fence = true;
                prior_candidate = Some(row);
                continue;
            }
            Err(error) => return Err(error),
        };
        match row.state.as_str() {
            "pending" if !pending_dispatch_is_authorized(&row, effect_doc.replay_policy)? => {
                prior_candidate = Some(row);
            }
            "pending" | "claimed" => {
                return Ok(Some(claim_outbox_row(
                    tx,
                    &row,
                    now_ms,
                    lease_ms,
                    row.state.as_str(),
                )?));
            }
            _ => return Err(StoreError::Corruption),
        }
    }
}

fn settle_next_process_empty_task_teardown_in_tx(
    tx: &Transaction<'_>,
    now_ms: i64,
    lease_ms: i64,
) -> Result<Option<(TaskId, OperationId)>, StoreError> {
    settle_next_process_empty_task_teardown_in_tx_for_cleanup(tx, now_ms, lease_ms)
}

/// Shared process-empty teardown primitive for ordinary maintenance and host cleanup.
pub(crate) fn settle_next_process_empty_task_teardown_in_tx_for_cleanup(
    tx: &Transaction<'_>,
    now_ms: i64,
    lease_ms: i64,
) -> Result<Option<(TaskId, OperationId)>, StoreError> {
    let mut prior_candidate = None;
    loop {
        let Some(row) =
            load_next_process_empty_task_teardown_candidate(tx, now_ms, prior_candidate.as_ref())?
        else {
            return Ok(None);
        };
        // Earliest selected candidate: lineage/effect/fence failures fail closed immediately.
        let (effect_doc, _fence) = revalidate_outbox_effect(tx, &row)?;
        let Effect::BeginTaskTeardown { task_id, .. } = effect_doc.effect else {
            return Err(StoreError::Corruption);
        };
        // Final defensive guard; SQL already excluded Active/Releasing Task-owned rows.
        refuse_archive_with_live_resources(tx, task_id)?;
        match row.state.as_str() {
            "pending" if !pending_dispatch_is_authorized(&row, effect_doc.replay_policy)? => {
                prior_candidate = Some(row);
                continue;
            }
            "pending" | "claimed" => {
                let claim = claim_outbox_row(tx, &row, now_ms, lease_ms, row.state.as_str())?;
                let permit = begin_dispatch_in_tx(tx, now_ms, &claim)?;
                let Effect::BeginTaskTeardown {
                    task_id: settled_task,
                    ..
                } = permit.effect()
                else {
                    return Err(StoreError::Corruption);
                };
                let settled_task = *settled_task;
                let operation_id = permit.operation_id();
                let state = command_bus::record_dispatch_completion_in_tx(
                    tx,
                    &permit,
                    DispatchCompletion::Settled,
                    now_ms,
                )?;
                if !matches!(state, OperationState::Settled { .. }) {
                    return Err(StoreError::Corruption);
                }
                return Ok(Some((settled_task, operation_id)));
            }
            _ => return Err(StoreError::Corruption),
        }
    }
}

fn release_dispatch_claim_in_tx(
    tx: &Transaction<'_>,
    claim: &DispatchClaim,
    now_ms: i64,
    delay_ms: i64,
) -> Result<(), StoreError> {
    let row = load_outbox_row_by_id(tx, claim.outbox_id())?.ok_or(StoreError::StaleClaim)?;
    if row.state != "claimed" {
        return Err(StoreError::InvalidDispatchTransition);
    }
    revalidate_outbox_effect(tx, &row)?;
    verify_claim_generation(&row, claim, now_ms)?;
    let available_at = lease_deadline(now_ms.max(row.available_at_ms), delay_ms)?;
    let changed = tx.execute(
        "UPDATE outbox
         SET state = 'pending', leased_until_ms = NULL, available_at_ms = ?1
         WHERE outbox_id = ?2 AND state = 'claimed' AND lease_generation = ?3",
        rusqlite::params![
            available_at,
            claim.outbox_id().as_bytes().as_slice(),
            claim.lease_generation(),
        ],
    )?;
    if changed != 1 {
        return Err(StoreError::InvalidDispatchTransition);
    }
    Ok(())
}

fn renew_dispatch_claim_in_tx(
    tx: &Transaction<'_>,
    now_ms: i64,
    claim: &DispatchClaim,
    lease_ms: i64,
) -> Result<DispatchClaim, StoreError> {
    let row = load_outbox_row_by_id(tx, claim.outbox_id())?.ok_or(StoreError::StaleClaim)?;
    if row.state != "claimed" && row.state != "dispatching" {
        return Err(StoreError::InvalidDispatchTransition);
    }
    revalidate_outbox_effect(tx, &row)?;
    verify_claim_generation(&row, claim, now_ms)?;
    let prior = row.leased_until_ms.ok_or(StoreError::StaleClaim)?;
    let leased_until = renewed_lease_deadline(now_ms, prior, lease_ms)?;
    let changed = tx.execute(
        "UPDATE outbox
         SET leased_until_ms = ?1
         WHERE outbox_id = ?2 AND lease_generation = ?3 AND state IN ('claimed', 'dispatching')",
        rusqlite::params![
            leased_until,
            claim.outbox_id().as_bytes().as_slice(),
            claim.lease_generation(),
        ],
    )?;
    if changed != 1 {
        return Err(StoreError::StaleClaim);
    }
    Ok(DispatchClaim::new(
        claim.outbox_id(),
        claim.lease_generation(),
    ))
}

fn begin_dispatch_in_tx(
    tx: &Transaction<'_>,
    now_ms: i64,
    claim: &DispatchClaim,
) -> Result<DispatchPermit, StoreError> {
    let row = load_outbox_row_by_id(tx, claim.outbox_id())?.ok_or(StoreError::StaleClaim)?;
    if row.state != "claimed" {
        return Err(StoreError::InvalidDispatchTransition);
    }
    let (effect_doc, fence) = revalidate_outbox_effect(tx, &row)?;
    verify_claim_generation(&row, claim, now_ms)?;
    let dispatch_started_at = now_ms.max(row.available_at_ms);
    let leased_until = row.leased_until_ms.ok_or(StoreError::Corruption)?;
    if dispatch_started_at >= leased_until {
        return Err(StoreError::Corruption);
    }
    let next_attempts = row.attempts.checked_add(1).ok_or(StoreError::Corruption)?;
    let changed = tx.execute(
        "UPDATE outbox
         SET state = 'dispatching', attempts = ?1, dispatch_started_at_ms = ?2,
             reconciliation_receipt = NULL
         WHERE outbox_id = ?3 AND state = 'claimed' AND lease_generation = ?4",
        rusqlite::params![
            next_attempts,
            dispatch_started_at,
            claim.outbox_id().as_bytes().as_slice(),
            claim.lease_generation(),
        ],
    )?;
    if changed != 1 {
        return Err(StoreError::InvalidDispatchTransition);
    }
    let mut permit_row = row;
    permit_row.state = "dispatching".into();
    permit_row.attempts = next_attempts;
    permit_row.dispatch_started_at_ms = Some(dispatch_started_at);
    permit_row.reconciliation_receipt = None;
    build_dispatch_permit(&permit_row, effect_doc, fence)
}

fn record_dispatch_ambiguity_in_tx(
    tx: &Transaction<'_>,
    permit: &DispatchPermit,
    now_ms: i64,
    delay_ms: i64,
) -> Result<AmbiguityDisposition, StoreError> {
    let row = load_outbox_row_by_id(tx, permit.outbox_id())?.ok_or(StoreError::StaleClaim)?;
    let (effect_doc, fence) = revalidate_ambiguity_effect(tx, &row)?;
    validate_dispatch_permit(&row, permit, &effect_doc, fence)?;
    let disposition = ambiguity_disposition(effect_doc.replay_policy);

    let idempotent_state = match disposition {
        AmbiguityDisposition::RetryScheduled => "pending",
        AmbiguityDisposition::ReconciliationRequired => "reconcile_required",
        AmbiguityDisposition::Uncertain => "uncertain",
    };
    if row.state == idempotent_state {
        return Ok(disposition);
    }
    if row.state != "dispatching" {
        return Err(StoreError::InvalidDispatchTransition);
    }
    if row.reconciliation_receipt.is_some() {
        return Err(StoreError::Corruption);
    }

    if disposition == AmbiguityDisposition::Uncertain {
        command_bus::record_no_retry_dispatch_uncertainty_in_tx(
            tx,
            &row,
            &effect_doc,
            fence,
            now_ms,
        )?;
        return Ok(disposition);
    }

    let started_at = row.dispatch_started_at_ms.ok_or(StoreError::Corruption)?;
    let available_at = lease_deadline(now_ms.max(started_at).max(row.available_at_ms), delay_ms)?;
    let (next_state, last_error_class) = match disposition {
        AmbiguityDisposition::RetryScheduled => ("pending", None),
        AmbiguityDisposition::ReconciliationRequired => {
            ("reconcile_required", Some("ambiguous_dispatch"))
        }
        AmbiguityDisposition::Uncertain => unreachable!("handled above"),
    };
    let changed = tx.execute(
        "UPDATE outbox
         SET state = ?1, available_at_ms = ?2, leased_until_ms = NULL,
             last_error_class = ?3, reconciliation_receipt = NULL
         WHERE outbox_id = ?4 AND state = 'dispatching'
           AND lease_generation = ?5 AND attempts = ?6",
        rusqlite::params![
            next_state,
            available_at,
            last_error_class,
            row.outbox_id.as_bytes().as_slice(),
            row.lease_generation,
            row.attempts,
        ],
    )?;
    if changed != 1 {
        return Err(StoreError::StaleClaim);
    }
    Ok(disposition)
}

fn load_next_expired_dispatch_candidate(
    tx: &Transaction<'_>,
    now_ms: i64,
    after: Option<&OutboxRow>,
) -> Result<Option<OutboxRow>, StoreError> {
    let after_available_at = after.map(|row| row.available_at_ms);
    let after_event_sequence = after
        .map(|row| u64_to_sqlite_i64("outbox.event_sequence", row.event_sequence))
        .transpose()?;
    let after_effect_index = after.map(|row| row.effect_index);
    let after_outbox_id = after.map(|row| row.outbox_id.as_bytes().as_slice());
    let selected: Option<Vec<u8>> = tx
        .query_row(
            "SELECT o.outbox_id
             FROM outbox o
             JOIN operations op ON op.operation_id = o.operation_id
             WHERE op.state = 'accepted'
               AND o.state = 'dispatching'
               AND (o.leased_until_ms IS NULL OR o.leased_until_ms <= ?1)
               AND (
                 ?2 IS NULL
                 OR o.available_at_ms > ?2
                 OR (o.available_at_ms = ?2 AND o.event_sequence > ?3)
                 OR (
                   o.available_at_ms = ?2 AND o.event_sequence = ?3
                   AND o.effect_index > ?4
                 )
                 OR (
                   o.available_at_ms = ?2 AND o.event_sequence = ?3
                   AND o.effect_index = ?4 AND o.outbox_id > ?5
                 )
               )
             ORDER BY o.available_at_ms ASC, o.event_sequence ASC,
                      o.effect_index ASC, o.outbox_id ASC
             LIMIT 1",
            rusqlite::params![
                now_ms,
                after_available_at,
                after_event_sequence,
                after_effect_index,
                after_outbox_id,
            ],
            |row| row.get(0),
        )
        .optional()?;
    let Some(outbox_id_bytes) = selected else {
        return Ok(None);
    };
    let outbox_id = parse_outbox_id(&outbox_id_bytes)?;
    Ok(Some(
        load_outbox_row_by_id(tx, outbox_id)?.ok_or(StoreError::Corruption)?,
    ))
}

fn recover_next_expired_dispatch_in_tx(
    tx: &Transaction<'_>,
    now_ms: i64,
    delay_ms: i64,
) -> Result<Option<AmbiguityDisposition>, StoreError> {
    let mut prior_candidate = None;
    let mut saw_stale_fence = false;
    loop {
        let Some(row) = load_next_expired_dispatch_candidate(tx, now_ms, prior_candidate.as_ref())?
        else {
            return if saw_stale_fence {
                Err(StoreError::StaleFence)
            } else {
                Ok(None)
            };
        };
        let (effect_doc, fence) = match revalidate_ambiguity_effect(tx, &row) {
            Ok(validated) => validated,
            Err(StoreError::StaleFence) => {
                saw_stale_fence = true;
                prior_candidate = Some(row);
                continue;
            }
            Err(error) => return Err(error),
        };
        if row.state != "dispatching" || row.attempts <= 0 || row.reconciliation_receipt.is_some() {
            return Err(StoreError::Corruption);
        }
        let leased_until = row.leased_until_ms.ok_or(StoreError::Corruption)?;
        if leased_until > now_ms {
            return Err(StoreError::InvalidDispatchTransition);
        }
        let disposition = ambiguity_disposition(effect_doc.replay_policy);
        if disposition == AmbiguityDisposition::Uncertain {
            command_bus::record_no_retry_dispatch_uncertainty_in_tx(
                tx,
                &row,
                &effect_doc,
                fence,
                now_ms,
            )?;
            return Ok(Some(disposition));
        }
        let (next_state, last_error_class) = match disposition {
            AmbiguityDisposition::RetryScheduled => ("pending", None),
            AmbiguityDisposition::ReconciliationRequired => {
                ("reconcile_required", Some("ambiguous_dispatch"))
            }
            AmbiguityDisposition::Uncertain => unreachable!("handled above"),
        };
        let started_at = row.dispatch_started_at_ms.ok_or(StoreError::Corruption)?;
        let available_at =
            lease_deadline(now_ms.max(started_at).max(row.available_at_ms), delay_ms)?;
        let next_generation = row
            .lease_generation
            .checked_add(1)
            .ok_or(StoreError::Corruption)?;
        let changed = tx.execute(
            "UPDATE outbox
             SET state = ?1, available_at_ms = ?2, leased_until_ms = NULL,
                 last_error_class = ?3, reconciliation_receipt = NULL,
                 lease_generation = ?4
             WHERE outbox_id = ?5 AND state = 'dispatching'
               AND lease_generation = ?6 AND attempts = ?7
               AND leased_until_ms IS NOT NULL AND leased_until_ms <= ?8",
            rusqlite::params![
                next_state,
                available_at,
                last_error_class,
                next_generation,
                row.outbox_id.as_bytes().as_slice(),
                row.lease_generation,
                row.attempts,
                now_ms,
            ],
        )?;
        if changed != 1 {
            return Err(StoreError::StaleClaim);
        }
        return Ok(Some(disposition));
    }
}

fn load_next_reconciliation_candidate(
    tx: &Transaction<'_>,
    now_ms: i64,
    after: Option<&OutboxRow>,
) -> Result<Option<(OutboxRow, ReconciliationOrigin)>, StoreError> {
    let after_available_at = after.map(|row| row.available_at_ms);
    let after_event_sequence = after
        .map(|row| u64_to_sqlite_i64("outbox.event_sequence", row.event_sequence))
        .transpose()?;
    let after_effect_index = after.map(|row| row.effect_index);
    let after_outbox_id = after.map(|row| row.outbox_id.as_bytes().as_slice());
    let selected: Option<(Vec<u8>, String)> = tx
        .query_row(
            "SELECT o.outbox_id, op.state
             FROM outbox o
             JOIN operations op ON op.operation_id = o.operation_id
             WHERE op.state = 'accepted'
               AND (
                 (o.state = 'reconcile_required' AND o.available_at_ms <= ?1)
                 OR (
                   o.state = 'reconciling' AND o.available_at_ms <= ?1
                   AND (o.leased_until_ms IS NULL OR o.leased_until_ms <= ?1)
                 )
               )
               AND (
                 ?2 IS NULL
                 OR o.available_at_ms > ?2
                 OR (o.available_at_ms = ?2 AND o.event_sequence > ?3)
                 OR (
                   o.available_at_ms = ?2 AND o.event_sequence = ?3
                   AND o.effect_index > ?4
                 )
                 OR (
                   o.available_at_ms = ?2 AND o.event_sequence = ?3
                   AND o.effect_index = ?4 AND o.outbox_id > ?5
                 )
               )
             ORDER BY o.available_at_ms ASC, o.event_sequence ASC,
                      o.effect_index ASC, o.outbox_id ASC
             LIMIT 1",
            rusqlite::params![
                now_ms,
                after_available_at,
                after_event_sequence,
                after_effect_index,
                after_outbox_id,
            ],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((outbox_id_bytes, operation_state)) = selected else {
        return Ok(None);
    };
    let outbox_id = parse_outbox_id(&outbox_id_bytes)?;
    let row = load_outbox_row_by_id(tx, outbox_id)?.ok_or(StoreError::Corruption)?;
    let origin = match operation_state.as_str() {
        "accepted" => ReconciliationOrigin::Accepted,
        "uncertain" => ReconciliationOrigin::Uncertain,
        _ => return Err(StoreError::Corruption),
    };
    Ok(Some((row, origin)))
}

fn claim_next_reconciliation_in_tx(
    tx: &Transaction<'_>,
    now_ms: i64,
    lease_ms: i64,
) -> Result<Option<ReconciliationClaim>, StoreError> {
    let mut prior_candidate = None;
    let mut saw_stale_fence = false;
    loop {
        let Some((row, origin)) =
            load_next_reconciliation_candidate(tx, now_ms, prior_candidate.as_ref())?
        else {
            return if saw_stale_fence {
                Err(StoreError::StaleFence)
            } else {
                Ok(None)
            };
        };
        let (effect_doc, fence) = match origin {
            ReconciliationOrigin::Accepted => match revalidate_outbox_effect(tx, &row) {
                Ok(validated) => validated,
                Err(StoreError::StaleFence) => {
                    saw_stale_fence = true;
                    prior_candidate = Some(row);
                    continue;
                }
                Err(error) => return Err(error),
            },
            ReconciliationOrigin::Uncertain => return Err(StoreError::InvalidDispatchTransition),
        };
        if effect_doc.replay_policy != ReplayPolicy::ReconcileBeforeRetry
            || (row.state != "reconcile_required" && row.state != "reconciling")
        {
            return Err(StoreError::Corruption);
        }
        let leased_until = lease_deadline(now_ms, lease_ms)?;
        let next_generation = row
            .lease_generation
            .checked_add(1)
            .ok_or(StoreError::Corruption)?;
        let changed = tx.execute(
            "UPDATE outbox
             SET state = 'reconciling', lease_generation = ?1, leased_until_ms = ?2
             WHERE outbox_id = ?3 AND state = ?4 AND lease_generation = ?5",
            rusqlite::params![
                next_generation,
                leased_until,
                row.outbox_id.as_bytes().as_slice(),
                row.state.as_str(),
                row.lease_generation,
            ],
        )?;
        if changed != 1 {
            return Err(StoreError::InvalidDispatchTransition);
        }
        let mut claimed_row = row;
        claimed_row.state = "reconciling".into();
        claimed_row.lease_generation = next_generation;
        claimed_row.leased_until_ms = Some(leased_until);
        return Ok(Some(build_reconciliation_claim(
            &claimed_row,
            origin,
            effect_doc,
            fence,
        )?));
    }
}

fn verify_reconciliation_claim_generation(
    row: &OutboxRow,
    claim: &ReconciliationClaim,
    now_ms: i64,
) -> Result<(), StoreError> {
    if row.lease_generation != claim.lease_generation() {
        return Err(StoreError::StaleClaim);
    }
    let leased_until = row.leased_until_ms.ok_or(StoreError::Corruption)?;
    if leased_until <= now_ms {
        return Err(StoreError::ExpiredClaim);
    }
    Ok(())
}

fn renew_reconciliation_claim_in_tx(
    tx: &Transaction<'_>,
    claim: &ReconciliationClaim,
    now_ms: i64,
    lease_ms: i64,
) -> Result<ReconciliationClaim, StoreError> {
    let row = load_outbox_row_by_id(tx, claim.outbox_id())?.ok_or(StoreError::StaleClaim)?;
    if row.state != "reconciling" {
        return Err(StoreError::InvalidDispatchTransition);
    }
    let (effect_doc, fence) = match claim.origin() {
        ReconciliationOrigin::Accepted => revalidate_outbox_effect(tx, &row)?,
        ReconciliationOrigin::Uncertain => return Err(StoreError::InvalidDispatchTransition),
    };
    validate_reconciliation_claim(&row, claim, &effect_doc, fence)?;
    verify_reconciliation_claim_generation(&row, claim, now_ms)?;
    let prior = row.leased_until_ms.ok_or(StoreError::Corruption)?;
    let leased_until = renewed_lease_deadline(now_ms, prior, lease_ms)?;
    let changed = tx.execute(
        "UPDATE outbox
         SET leased_until_ms = ?1
         WHERE outbox_id = ?2 AND state = 'reconciling'
           AND lease_generation = ?3 AND attempts = ?4",
        rusqlite::params![
            leased_until,
            row.outbox_id.as_bytes().as_slice(),
            row.lease_generation,
            row.attempts,
        ],
    )?;
    if changed != 1 {
        return Err(StoreError::StaleClaim);
    }
    Ok(claim.clone())
}

fn release_reconciliation_claim_in_tx(
    tx: &Transaction<'_>,
    claim: &ReconciliationClaim,
    now_ms: i64,
    delay_ms: i64,
) -> Result<(), StoreError> {
    let row = load_outbox_row_by_id(tx, claim.outbox_id())?.ok_or(StoreError::StaleClaim)?;
    if row.state != "reconciling" {
        return Err(StoreError::InvalidDispatchTransition);
    }
    let (effect_doc, fence) = match claim.origin() {
        ReconciliationOrigin::Accepted => revalidate_outbox_effect(tx, &row)?,
        ReconciliationOrigin::Uncertain => return Err(StoreError::InvalidDispatchTransition),
    };
    validate_reconciliation_claim(&row, claim, &effect_doc, fence)?;
    verify_reconciliation_claim_generation(&row, claim, now_ms)?;
    let started_at = row.dispatch_started_at_ms.ok_or(StoreError::Corruption)?;
    let available_at = lease_deadline(now_ms.max(started_at).max(row.available_at_ms), delay_ms)?;
    let next_generation = row
        .lease_generation
        .checked_add(1)
        .ok_or(StoreError::Corruption)?;
    let changed = tx.execute(
        "UPDATE outbox
         SET state = 'reconcile_required', available_at_ms = ?1,
             leased_until_ms = NULL, last_error_class = 'ambiguous_dispatch',
             reconciliation_receipt = NULL, lease_generation = ?2
         WHERE outbox_id = ?3 AND state = 'reconciling'
           AND lease_generation = ?4 AND attempts = ?5",
        rusqlite::params![
            available_at,
            next_generation,
            row.outbox_id.as_bytes().as_slice(),
            row.lease_generation,
            row.attempts,
        ],
    )?;
    if changed != 1 {
        return Err(StoreError::StaleClaim);
    }
    Ok(())
}

fn record_reconciliation_in_tx(
    tx: &Transaction<'_>,
    claim: &ReconciliationClaim,
    finding: ReconciliationFinding,
    now_ms: i64,
) -> Result<OperationState, StoreError> {
    let row = load_outbox_row_by_id(tx, claim.outbox_id())?.ok_or(StoreError::StaleClaim)?;
    if finding.lookup_identity() != claim.lookup_identity() {
        return Err(StoreError::ConflictingOutcome);
    }
    if matches!(row.state.as_str(), "settled" | "failed") {
        if claim.origin() != ReconciliationOrigin::Accepted {
            return Err(StoreError::InvalidDispatchTransition);
        }
        let effect_doc = effect_document_for_terminal_replay(&row, claim.document())?;
        validate_reconciliation_claim_identity(&row, claim, &effect_doc)?;
        let durable_outcome_at: Option<i64> = tx.query_row(
            "SELECT outcome_at_ms FROM operations WHERE operation_id = ?1",
            [claim.operation_id().as_bytes().as_slice()],
            |row| row.get(0),
        )?;
        let outcome = build_present_reconciliation_outcome(
            tx,
            claim,
            &finding,
            durable_outcome_at.ok_or(StoreError::Corruption)?,
        )?
        .ok_or(StoreError::InvalidDispatchTransition)?;
        return command_bus::record_present_reconciliation_in_tx(
            tx,
            outcome,
            row.outbox_id,
            row.lease_generation,
        );
    }
    let (effect_doc, fence) = match claim.origin() {
        ReconciliationOrigin::Accepted => revalidate_outbox_effect(tx, &row)?,
        ReconciliationOrigin::Uncertain => return Err(StoreError::InvalidDispatchTransition),
    };
    validate_reconciliation_claim(&row, claim, &effect_doc, fence)?;

    match (&row.state[..], &finding) {
        ("pending", ReconciliationFinding::Absent { .. }) => {
            validate_absence_authorization(&row)?;
            return Ok(OperationState::Accepted);
        }
        ("reconcile_required", ReconciliationFinding::Inconclusive { .. }) => {
            return Ok(OperationState::Accepted);
        }
        _ => {}
    }
    if row.state != "reconciling" {
        return Err(StoreError::InvalidDispatchTransition);
    }
    let leased_until = row.leased_until_ms.ok_or(StoreError::Corruption)?;
    if leased_until <= now_ms {
        return Err(StoreError::ExpiredClaim);
    }
    if effect_doc.replay_policy != ReplayPolicy::ReconcileBeforeRetry {
        return Err(StoreError::Corruption);
    }
    let started_at = row.dispatch_started_at_ms.ok_or(StoreError::Corruption)?;
    let proof_or_observation_at = now_ms.max(started_at).max(row.available_at_ms);
    let present_outcome =
        build_present_reconciliation_outcome(tx, claim, &finding, proof_or_observation_at)?;

    match finding {
        ReconciliationFinding::Absent {
            lookup_identity,
            retry_after,
        } => {
            let delay_ms = validate_dispatch_lease_ms(retry_after)?;
            let effect_index =
                u32::try_from(row.effect_index).map_err(|_| StoreError::Corruption)?;
            let completed_attempt = u64_from_nonnegative_i64("outbox.attempts", row.attempts)?;
            let receipt = AbsenceReceiptDocument::new(
                row.outbox_id,
                row.operation_id,
                effect_index,
                completed_attempt,
                lookup_identity,
                proof_or_observation_at,
            )?;
            let receipt = encode_absence_receipt(&receipt)?;
            let available_at = lease_deadline(proof_or_observation_at, delay_ms)?;
            let changed = tx.execute(
                "UPDATE outbox
                 SET state = 'pending', available_at_ms = ?1, leased_until_ms = NULL,
                     last_error_class = NULL, reconciliation_receipt = ?2
                 WHERE outbox_id = ?3 AND state = 'reconciling'
                   AND lease_generation = ?4 AND attempts = ?5",
                rusqlite::params![
                    available_at,
                    receipt,
                    row.outbox_id.as_bytes().as_slice(),
                    row.lease_generation,
                    row.attempts,
                ],
            )?;
            if changed != 1 {
                return Err(StoreError::StaleClaim);
            }
            Ok(OperationState::Accepted)
        }
        ReconciliationFinding::Inconclusive {
            lookup_identity: _,
            retry_after,
        } => {
            let delay_ms = validate_dispatch_lease_ms(retry_after)?;
            let available_at = lease_deadline(proof_or_observation_at, delay_ms)?;
            let changed = tx.execute(
                "UPDATE outbox
                 SET state = 'reconcile_required', available_at_ms = ?1,
                     leased_until_ms = NULL, last_error_class = 'ambiguous_dispatch',
                     reconciliation_receipt = NULL
                 WHERE outbox_id = ?2 AND state = 'reconciling'
                   AND lease_generation = ?3 AND attempts = ?4",
                rusqlite::params![
                    available_at,
                    row.outbox_id.as_bytes().as_slice(),
                    row.lease_generation,
                    row.attempts,
                ],
            )?;
            if changed != 1 {
                return Err(StoreError::StaleClaim);
            }
            Ok(OperationState::Accepted)
        }
        ReconciliationFinding::PresentSettled { .. }
        | ReconciliationFinding::PresentFailed { .. } => {
            command_bus::record_present_reconciliation_in_tx(
                tx,
                present_outcome.ok_or(StoreError::InvalidDispatchTransition)?,
                row.outbox_id,
                row.lease_generation,
            )
        }
    }
}

fn build_present_reconciliation_outcome(
    tx: &Transaction<'_>,
    claim: &ReconciliationClaim,
    finding: &ReconciliationFinding,
    occurred_at_ms: i64,
) -> Result<Option<OperationOutcome>, StoreError> {
    let (external_identity, kind) = match finding {
        ReconciliationFinding::PresentSettled {
            external_identity, ..
        } => (
            external_identity,
            OperationOutcomeKind::Settled {
                result_event_ids: command_bus::settled_result_ids_for_callback(
                    tx,
                    claim.operation_id(),
                )?,
            },
        ),
        ReconciliationFinding::PresentFailed {
            external_identity,
            code,
            ..
        } => (
            external_identity,
            OperationOutcomeKind::Failed { code: *code },
        ),
        ReconciliationFinding::Absent { .. } | ReconciliationFinding::Inconclusive { .. } => {
            return Ok(None);
        }
    };
    let source = OutcomeSource::verified_reconciliation(claim.effect_index(), external_identity)
        .map_err(|_| StoreError::ConstraintViolation)?;
    let outcome = OperationOutcome::new(
        claim.operation_id(),
        occurred_at_ms,
        claim.action_epoch(),
        claim.resource_fence(),
        source,
        kind,
    )
    .map_err(|_| StoreError::ConstraintViolation)?;
    Ok(Some(outcome))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::command::{Command, CreateTaskIntent};
    use crate::domain::id::{ClientId, CommandId, EnvironmentId, ProjectId, TaskId};
    use crate::domain::operation::{CancellationReason, OperationErrorCode};
    use crate::domain::task::{
        ReviewReadiness, TaskActivity, TaskAssignment, TaskAttention, TaskConnectivity,
        WorkspaceRef,
    };
    use crate::kernel::maintenance::OutboxPayloadCleanup;
    use crate::kernel::WalCheckpointOutcome;
    use rusqlite::ffi::Error as FfiError;
    use tempfile::TempDir;

    #[derive(Clone)]
    struct TerminalCloseFixture {
        command: CommandEnvelope,
        receipt: CommandReceipt,
        operation_id: OperationId,
        permit: DispatchPermit,
        completion: DispatchCompletion,
        state: OperationState,
    }

    fn maintenance_envelope(
        command_id: CommandId,
        task_id: Option<TaskId>,
        expected_task_revision: Option<u64>,
        command: Command,
    ) -> CommandEnvelope {
        CommandEnvelope {
            command_id,
            client_id: ClientId::new(),
            task_id,
            issued_at_ms: 1_725_000_000_100,
            expected_task_revision,
            command,
        }
    }

    fn seed_open_task(store: &mut KernelStore) -> TaskId {
        let task_id = TaskId::new();
        store
            .execute(maintenance_envelope(
                CommandId::new(),
                None,
                None,
                Command::CreateTask(CreateTaskIntent {
                    id: task_id,
                    environment_id: EnvironmentId::new(),
                    title: "Maintenance fixture".into(),
                    description: None,
                    project_id: ProjectId::new(),
                    workspace: WorkspaceRef::Main,
                    assignment: TaskAssignment::LocalOwner,
                    created_at_ms: 1_725_000_000_000,
                    connectivity: TaskConnectivity::Connected,
                    attention: TaskAttention::None,
                    activity: TaskActivity::Idle,
                    review_readiness: ReviewReadiness::NotReady,
                }),
            ))
            .expect("create maintenance fixture task");
        task_id
    }

    fn seed_terminal_close(
        store: &mut KernelStore,
        completion: DispatchCompletion,
    ) -> TerminalCloseFixture {
        let task_id = seed_open_task(store);
        let command = maintenance_envelope(
            CommandId::new(),
            Some(task_id),
            Some(1),
            Command::BeginCloseTask,
        );
        let receipt = store.execute(command.clone()).expect("accept close");
        let CommandReceipt::Accepted { operation_id, .. } = receipt.clone() else {
            panic!("close must be accepted: {receipt:?}");
        };
        let claim = store
            .claim_next_dispatch(Duration::from_secs(30))
            .expect("claim close")
            .expect("close dispatch ready");
        let permit = store.begin_dispatch(&claim).expect("begin close dispatch");
        let state = store
            .record_dispatch_completion(&permit, completion.clone())
            .expect("finish close dispatch");
        TerminalCloseFixture {
            command,
            receipt,
            operation_id,
            permit,
            completion,
            state,
        }
    }

    fn seed_pending_close(store: &mut KernelStore) -> OperationId {
        let task_id = seed_open_task(store);
        let receipt = store
            .execute(maintenance_envelope(
                CommandId::new(),
                Some(task_id),
                Some(1),
                Command::BeginCloseTask,
            ))
            .expect("accept pending close");
        let CommandReceipt::Accepted { operation_id, .. } = receipt else {
            panic!("pending close must be accepted: {receipt:?}");
        };
        operation_id
    }

    #[test]
    fn command_contract_maps_sqlite_constraint_busy_and_corruption() {
        let constraint = rusqlite::Error::SqliteFailure(
            FfiError::new(rusqlite::ffi::SQLITE_CONSTRAINT),
            Some("UNIQUE constraint failed".into()),
        );
        assert_eq!(
            map_sqlite_error(&constraint),
            StoreError::ConstraintViolation
        );

        let busy = rusqlite::Error::SqliteFailure(
            FfiError::new(rusqlite::ffi::SQLITE_BUSY),
            Some("database is locked".into()),
        );
        assert_eq!(map_sqlite_error(&busy), StoreError::Busy);

        let locked = rusqlite::Error::SqliteFailure(
            FfiError::new(rusqlite::ffi::SQLITE_LOCKED),
            Some("database table is locked".into()),
        );
        assert_eq!(map_sqlite_error(&locked), StoreError::Busy);

        let corrupt = rusqlite::Error::SqliteFailure(
            FfiError::new(rusqlite::ffi::SQLITE_CORRUPT),
            Some("database disk image is malformed".into()),
        );
        assert_eq!(map_sqlite_error(&corrupt), StoreError::Corruption);

        let not_a_db = rusqlite::Error::SqliteFailure(
            FfiError::new(rusqlite::ffi::SQLITE_NOTADB),
            Some("file is not a database".into()),
        );
        assert_eq!(
            map_sqlite_error(&not_a_db),
            StoreError::Corruption,
            "NotADatabase must map by typed code"
        );
        assert_eq!(
            map_open_error(rusqlite::Error::SqliteFailure(
                FfiError::new(rusqlite::ffi::SQLITE_NOTADB),
                Some("file is not a database".into()),
            )),
            StoreError::Corruption
        );

        // Typed variants exercised by command execution and completion paths.
        assert_ne!(StoreError::StaleFence, StoreError::ConflictingOutcome);
        assert_ne!(
            StoreError::MissingOperation,
            StoreError::ConstraintViolation
        );
    }

    #[test]
    fn command_contract_readonly_connection_rejects_writes() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("kernel.sqlite3");
        let via_dot = dir.path().join(".").join("kernel.sqlite3");
        let store = KernelStore::open(&via_dot).expect("open");
        let canonical = std::fs::canonicalize(&path).expect("canonicalize");
        assert_eq!(
            store.path(),
            canonical.as_path(),
            "store must retain canonical absolute path"
        );
        let debug = format!("{store:?}");
        assert_eq!(debug, "KernelStore");
        assert!(
            !debug.to_lowercase().contains("sqlite"),
            "Debug must stay opaque, got {debug}"
        );
        assert!(
            !debug.contains(canonical.to_string_lossy().as_ref()),
            "Debug must not reveal filesystem path"
        );

        let conn = store.open_query_connection().expect("readonly");
        let busy_timeout: i64 = conn
            .query_row("PRAGMA busy_timeout;", [], |row| row.get(0))
            .expect("busy_timeout");
        assert_eq!(busy_timeout, BUSY_TIMEOUT_MS);
        let foreign_keys: i64 = conn
            .query_row("PRAGMA foreign_keys;", [], |row| row.get(0))
            .expect("foreign_keys");
        assert_eq!(foreign_keys, 1);
        let query_only: i64 = conn
            .query_row("PRAGMA query_only;", [], |row| row.get(0))
            .expect("query_only");
        assert_eq!(query_only, 1);

        let err = conn
            .execute("CREATE TABLE forbidden(x INTEGER)", [])
            .expect_err("writes must fail");
        let msg = err.to_string().to_lowercase();
        assert!(
            msg.contains("readonly") || msg.contains("read-only") || msg.contains("query_only"),
            "expected read-only failure, got {err}"
        );
    }

    #[test]
    fn command_contract_codec_mismatch_display_is_generic() {
        let err = StoreError::CodecMismatch {
            detail: "receipt schema_version 2 != 1".into(),
        };
        let text = err.to_string();
        assert!(
            text.contains("codec mismatch"),
            "display should describe codec mismatch, got {text}"
        );
        assert!(
            !text.contains("event codec mismatch"),
            "display must not hard-code event-only wording, got {text}"
        );
    }

    fn synchronous_mode(conn: &Connection) -> i64 {
        conn.query_row("PRAGMA synchronous", [], |row| row.get(0))
            .expect("synchronous mode")
    }

    #[test]
    fn passive_maintenance_preserves_pinned_reader_and_retries_to_completion() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("kernel.sqlite3");
        let mut store = KernelStore::open(&path).expect("open");
        let original_applied_at: i64 = store
            .conn
            .query_row(
                "SELECT applied_at_ms FROM schema_migrations WHERE version = 3",
                [],
                |row| row.get(0),
            )
            .expect("original migration timestamp");

        let reader = store.open_query_connection().expect("open pinned reader");
        reader
            .execute_batch("BEGIN DEFERRED")
            .expect("begin reader");
        let _: i64 = reader
            .query_row(
                "SELECT pruned_through_sequence FROM event_retention WHERE singleton_key = 1",
                [],
                |row| row.get(0),
            )
            .expect("pin read view");

        store
            .conn
            .execute(
                "UPDATE schema_migrations SET applied_at_ms = applied_at_ms + 1 WHERE version = 3",
                [],
            )
            .expect("append WAL frame after reader pin");
        let first = store
            .run_maintenance()
            .expect("passive maintenance with reader");
        let WalCheckpointOutcome::Partial {
            log_frames,
            checkpointed_frames,
        } = first.wal
        else {
            panic!("pinned reader must yield a partial checkpoint: {first:?}");
        };
        assert!(checkpointed_frames < log_frames);
        assert_eq!(synchronous_mode(&store.conn), 1, "must restore NORMAL");
        let pinned_applied_at: i64 = reader
            .query_row(
                "SELECT applied_at_ms FROM schema_migrations WHERE version = 3",
                [],
                |row| row.get(0),
            )
            .expect("read original pinned value after maintenance");
        assert_eq!(pinned_applied_at, original_applied_at);

        reader.execute_batch("ROLLBACK").expect("release reader");
        drop(reader);
        let second = store
            .run_maintenance()
            .expect("retry maintenance after reader release");
        assert!(matches!(
            second.wal,
            WalCheckpointOutcome::Complete {
                log_frames,
                checkpointed_frames,
            } if log_frames == checkpointed_frames
        ));
        assert_eq!(synchronous_mode(&store.conn), 1, "must remain NORMAL");
        assert_eq!(
            store
                .conn
                .query_row("SELECT COUNT(*) FROM schema_migrations", [], |row| row
                    .get::<_, i64>(0))
                .expect("migration count"),
            i64::try_from(schema::migration_manifest().len()).expect("migration count fits i64"),
            "maintenance must not delete durable rows"
        );
    }

    #[test]
    fn full_synchronous_scope_restores_normal_after_action_failure() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("kernel.sqlite3");
        let mut store = KernelStore::open(&path).expect("open");

        let error = maintenance::with_full_synchronous(&mut store.conn, |conn| {
            assert_eq!(synchronous_mode(conn), 2, "action must run at FULL");
            Err::<(), _>(StoreError::Busy)
        })
        .expect_err("injected action failure");
        assert_eq!(error, StoreError::Busy);
        assert_eq!(synchronous_mode(&store.conn), 1, "must restore NORMAL");
    }

    #[test]
    fn passive_maintenance_surfaces_integrity_failure() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("kernel.sqlite3");
        let mut store = KernelStore::open(&path).expect("open");

        let conn = Connection::open(&path).expect("open corruption fixture");
        conn.execute_batch(
            "PRAGMA ignore_check_constraints = ON;
             UPDATE event_retention SET pruned_through_sequence = -1 WHERE singleton_key = 1;",
        )
        .expect("violate retained boundary check");
        drop(conn);

        assert!(matches!(
            store.run_maintenance(),
            Err(StoreError::IntegrityCheckFailed(_))
        ));
        assert_eq!(synchronous_mode(&store.conn), 1, "must remain NORMAL");
    }

    #[test]
    fn terminal_outbox_payload_cleanup_is_bounded_and_preserves_idempotency() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("kernel.sqlite3");
        let mut store = KernelStore::open(&path).expect("open");
        let fixtures = [
            seed_terminal_close(&mut store, DispatchCompletion::Settled),
            seed_terminal_close(
                &mut store,
                DispatchCompletion::Failed {
                    code: OperationErrorCode::SideEffectFailed,
                },
            ),
            seed_terminal_close(
                &mut store,
                DispatchCompletion::Cancelled {
                    reason: CancellationReason::Superseded,
                },
            ),
        ];
        let pending_operation = seed_pending_close(&mut store);

        let first_payload_bytes: i64 = store
            .conn
            .query_row(
                "SELECT length(payload) FROM outbox WHERE operation_id = ?1",
                [fixtures[0].operation_id.as_bytes().as_slice()],
                |row| row.get(0),
            )
            .expect("terminal payload bytes");
        assert!(first_payload_bytes > 0);

        let first = maintenance::run(&mut store.conn, 1).expect("first bounded maintenance");
        assert_eq!(
            first.outbox_payloads,
            OutboxPayloadCleanup {
                rows_compacted: 1,
                payload_bytes_reclaimed: u64::try_from(first_payload_bytes)
                    .expect("payload bytes fit"),
                has_more: true,
            }
        );

        let compacted_operation_bytes: Vec<u8> = store
            .conn
            .query_row(
                "SELECT operation_id FROM outbox
                 WHERE compacted_payload_sha256 IS NOT NULL",
                [],
                |row| row.get(0),
            )
            .expect("one compacted row");
        let compacted_operation = OperationId::from_bytes(
            compacted_operation_bytes
                .try_into()
                .expect("operation id bytes"),
        )
        .expect("operation id");
        let compacted = fixtures
            .iter()
            .find(|fixture| fixture.operation_id == compacted_operation)
            .expect("compacted fixture");
        let (payload_len, digest_len): (i64, i64) = store
            .conn
            .query_row(
                "SELECT length(payload), length(compacted_payload_sha256)
                 FROM outbox WHERE operation_id = ?1",
                [compacted.operation_id.as_bytes().as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("compacted payload marker");
        assert_eq!((payload_len, digest_len), (0, 32));

        assert_eq!(
            store
                .execute(compacted.command.clone())
                .expect("duplicate command"),
            compacted.receipt
        );
        assert_eq!(
            store
                .record_dispatch_completion(&compacted.permit, compacted.completion.clone())
                .expect("duplicate completion"),
            compacted.state
        );
        assert_eq!(
            store
                .operation_status(compacted.operation_id)
                .expect("operation status"),
            Some(compacted.state.clone())
        );
        store
            .rebuild_projections()
            .expect("rebuild after compaction");

        let pending: (i64, Option<Vec<u8>>) = store
            .conn
            .query_row(
                "SELECT length(payload), compacted_payload_sha256
                 FROM outbox WHERE operation_id = ?1",
                [pending_operation.as_bytes().as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("pending payload");
        assert!(pending.0 > 0);
        assert!(pending.1.is_none());

        let second = maintenance::run(&mut store.conn, 2).expect("second bounded maintenance");
        assert_eq!(second.outbox_payloads.rows_compacted, 2);
        assert!(!second.outbox_payloads.has_more);
        drop(store);

        let mut store = KernelStore::open(&path).expect("reopen compacted store");
        for fixture in fixtures {
            assert_eq!(
                store
                    .execute(fixture.command)
                    .expect("duplicate after reopen"),
                fixture.receipt
            );
            assert_eq!(
                store
                    .operation_status(fixture.operation_id)
                    .expect("status after reopen"),
                Some(fixture.state)
            );
        }
    }

    #[test]
    fn terminal_outbox_payload_cleanup_rolls_back_a_corrupt_batch() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("kernel.sqlite3");
        let mut store = KernelStore::open(&path).expect("open");
        let first = seed_terminal_close(&mut store, DispatchCompletion::Settled);
        let second = seed_terminal_close(
            &mut store,
            DispatchCompletion::Cancelled {
                reason: CancellationReason::Superseded,
            },
        );
        store
            .conn
            .execute(
                "UPDATE outbox SET payload = X'00' WHERE operation_id = ?1",
                [second.operation_id.as_bytes().as_slice()],
            )
            .expect("corrupt one terminal payload");

        assert!(maintenance::run(&mut store.conn, 2).is_err());
        let rows: Vec<(i64, Option<Vec<u8>>)> = {
            let mut stmt = store
                .conn
                .prepare(
                    "SELECT length(payload), compacted_payload_sha256 FROM outbox
                     WHERE operation_id IN (?1, ?2) ORDER BY operation_id",
                )
                .expect("prepare rollback check");
            stmt.query_map(
                rusqlite::params![
                    first.operation_id.as_bytes().as_slice(),
                    second.operation_id.as_bytes().as_slice(),
                ],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("query rollback check")
            .map(|row| row.expect("rollback row"))
            .collect()
        };
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|(_, digest)| digest.is_none()));
        assert!(rows.iter().all(|(payload_len, _)| *payload_len > 0));
    }

    #[test]
    fn compacted_payload_marker_fails_closed_when_forged() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("kernel.sqlite3");
        let mut store = KernelStore::open(&path).expect("open");
        let terminal = seed_terminal_close(&mut store, DispatchCompletion::Settled);
        let pending = seed_pending_close(&mut store);

        let original_terminal_payload: Vec<u8> = store
            .conn
            .query_row(
                "SELECT payload FROM outbox WHERE operation_id = ?1",
                [terminal.operation_id.as_bytes().as_slice()],
                |row| row.get(0),
            )
            .expect("terminal payload");
        store
            .conn
            .execute(
                "UPDATE outbox SET compacted_payload_sha256 = zeroblob(32)
                 WHERE operation_id = ?1",
                [terminal.operation_id.as_bytes().as_slice()],
            )
            .expect("forge marker beside full payload");
        assert_eq!(
            store.operation_status(terminal.operation_id),
            Err(StoreError::Corruption)
        );
        store
            .conn
            .execute(
                "UPDATE outbox SET payload = X'', compacted_payload_sha256 = NULL
                 WHERE operation_id = ?1",
                [terminal.operation_id.as_bytes().as_slice()],
            )
            .expect("forge empty payload without marker");
        assert_eq!(
            store.operation_status(terminal.operation_id),
            Err(StoreError::Corruption)
        );
        store
            .conn
            .execute(
                "UPDATE outbox SET payload = ?1, compacted_payload_sha256 = NULL
                 WHERE operation_id = ?2",
                rusqlite::params![
                    original_terminal_payload,
                    terminal.operation_id.as_bytes().as_slice(),
                ],
            )
            .expect("restore valid terminal payload");

        store
            .conn
            .execute(
                "UPDATE outbox SET payload = X'', compacted_payload_sha256 = zeroblob(32)
                 WHERE operation_id = ?1",
                [pending.as_bytes().as_slice()],
            )
            .expect("forge active compaction marker");
        assert_eq!(store.operation_status(pending), Err(StoreError::Corruption));

        let report = maintenance::run(&mut store.conn, 1).expect("compact terminal row");
        assert_eq!(report.outbox_payloads.rows_compacted, 1);
        store
            .conn
            .execute(
                "UPDATE outbox SET compacted_payload_sha256 = zeroblob(32)
                 WHERE operation_id = ?1",
                [terminal.operation_id.as_bytes().as_slice()],
            )
            .expect("forge terminal digest");
        assert_eq!(
            store.operation_status(terminal.operation_id),
            Err(StoreError::Corruption)
        );
    }
}
