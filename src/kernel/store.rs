use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior};
use sha2::{Digest, Sha256};

use crate::domain::agent_resource::AgentResourceBinding;
use crate::domain::command::{CommandEnvelope, CommandReceipt};
use crate::domain::event::{
    AgentProviderSessionBoundPayload, AgentSessionRegisteredPayload, ArtifactRegisteredPayload,
    DomainEvent, Event, HostCleanupBranchCompletedPayload, HostCloseBegunPayload,
    OperationAcceptedFact, OperationCancelledFact, OperationFailedFact, OperationSettledFact,
    OperationUncertainFact, PrimaryAgentSetPayload, PrimaryPromotedPayload,
    ProviderApprovalPresentedPayload, ProviderInputAcceptedPayload, ProviderInputDeliveredPayload,
    ProviderQuestionPresentedPayload, ProviderWaitSettledPayload, ResourceRegisteredPayload,
    ResourceReleaseBegunPayload, ResourceReleasedPayload, SpecialistClosedPayload,
    SpecialistHandoffRecordedPayload, SpecialistRequestedPayload, TaskAttentionSetPayload,
    TaskCloseBegunPayload, TaskCreatedPayload, TaskRenamedPayload, TaskTerminalStripSetPayload,
    TaskUnitPayload, TerminalActivityPayload, TerminalCwdReportedPayload, TerminalExitedPayload,
    TerminalRenamedPayload, UnstartedPrimaryProviderReboundPayload, EVENT_SCHEMA_VERSION,
};
use crate::domain::id::{EventId, OperationId, OutboxId, ResourceId, TaskId};
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
use crate::kernel::outbox::{external_idempotency_key, DestinationClass, Effect, ReplayPolicy};
use crate::kernel::projector;
use crate::kernel::runtime::RecoveringResource;
use crate::kernel::schema::{self, Migration, PROJECTION_TABLES};
use crate::kernel::StoreMaintenanceReport;
use crate::workspace::WorkspaceAuthorization;

const BUSY_TIMEOUT_MS: i64 = 5_000;

/// How many pending `ReleaseResource` rows one maintenance scan examines.
///
/// A bound rather than a page: the scan runs on the reaper tick beside every
/// other maintenance unit, and anything it does not reach this tick it reaches
/// on the next one.
const MAX_PENDING_RESOURCE_RELEASES_PER_SCAN: usize = 64;
const MAX_DISPATCH_LEASE_MS: i64 = 3_600_000;
pub(crate) const MAX_PROVIDER_EVENT_PAYLOAD_BYTES: usize = 256 * 1024;
const MAX_CONNECT_IDENTITY_BYTES: usize = 64 * 1024;

/// Opaque SQLite-backed kernel store. No public connection accessor.
pub struct KernelStore {
    path: PathBuf,
    conn: Connection,
    /// The projection rebuild `open` ran because it applied migrations, if it
    /// did. `None` means the schema was already current and nothing was
    /// replayed -- which is what a second open of the same store must report.
    startup_rebuild: Option<ProjectionRebuild>,
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

/// Which migrations one `migrate()` call actually applied.
///
/// A migration that adds a projection table leaves that table EMPTY while the
/// event log already contains the facts it projects, so the caller has to know
/// that something was applied in order to replay the log into it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MigrationOutcome {
    pub applied_versions: Vec<i64>,
}

impl MigrationOutcome {
    pub fn applied_any(&self) -> bool {
        !self.applied_versions.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreError {
    Io(String),
    Sqlite(String),
    HostAuthorityRequired,
    Busy,
    Corruption,
    Truncated,
    ConstraintViolation,
    CommandIdConflict,
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
            Self::HostAuthorityRequired => {
                write!(f, "host authority is required for task creation")
            }
            Self::Busy => write!(f, "sqlite busy"),
            Self::Corruption => write!(f, "database corruption detected"),
            Self::Truncated => write!(f, "database file is truncated"),
            Self::ConstraintViolation => write!(f, "sqlite constraint violation"),
            Self::CommandIdConflict => write!(f, "command id is already bound to another scope"),
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
            startup_rebuild: None,
        };
        let migrated = store.migrate()?;
        // A migration that introduces a projection table creates it EMPTY, and
        // nothing else in the process ever replays the log into it. The next
        // command would then compare a durable replay against a projection
        // missing every row for that table and report Corruption. Replaying
        // once here is what makes an upgrade openable; it is deliberately not a
        // SQL backfill, because the facts live in msgpack recipes and must
        // carry each registration event's own occurred_at_ms.
        if migrated.applied_any() {
            match store.rebuild_projection_tables() {
                Ok(rebuild) => {
                    eprintln!(
                        "devmanager-kernel: applied migrations {:?}; projection rebuild replayed {} events, drift_detected={}",
                        migrated.applied_versions, rebuild.events_replayed, rebuild.drift_detected
                    );
                    store.startup_rebuild = Some(rebuild);
                }
                Err(error) => {
                    eprintln!(
                        "devmanager-kernel: applied migrations {:?}; projection rebuild failed: {error:?}",
                        migrated.applied_versions
                    );
                    return Err(error);
                }
            }
        }
        store.integrity_check()?;
        Ok(store)
    }

    /// The rebuild `open` ran after applying migrations, or `None` when the
    /// schema was already current. Opening an up-to-date store must not replay.
    pub(crate) fn startup_rebuild(&self) -> Option<&ProjectionRebuild> {
        self.startup_rebuild.as_ref()
    }

    pub fn rebuild_projections(&mut self) -> Result<ProjectionRebuild, StoreError> {
        self.with_rebuild_transaction(rebuild_projections_tx)
    }

    /// Open the one writer transaction every projection rebuild runs in.
    ///
    /// IMMEDIATE, never DEFERRED. A rebuild reads before it writes: its first
    /// statements copy each projection table's shape into a TEMP shadow, which
    /// takes a read snapshot of the main database, and only the replay that
    /// follows writes to it. Under a DEFERRED transaction any other process
    /// that commits in between -- and the client, the host and `devmanager-ctl`
    /// all open this store -- leaves that snapshot stale, so the first write
    /// fails `SQLITE_BUSY_SNAPSHOT` outright; the busy handler cannot rescue a
    /// snapshot that can never advance, and no caller retries. Taking the
    /// writer lock at BEGIN instead makes the same contention a wait the busy
    /// timeout already covers.
    ///
    /// The cost of getting this wrong is not a visible error. `open` fails the
    /// boot rebuild, the store is left schema-current with empty terminal
    /// projections, the durable pending-rebuild marker is never retried
    /// (decision 6), and every live shell is then closed as `UnknownTerminal`.
    fn with_rebuild_transaction<T>(
        &mut self,
        body: impl FnOnce(&Transaction<'_>) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let result = body(&tx)?;
        tx.commit()?;
        Ok(result)
    }

    /// Replay the durable log into the projection tables and nothing else.
    ///
    /// Deliberately NOT [`Self::rebuild_projections`]: that one additionally runs
    /// three durable-integrity validators over `outbox`, `host_admission` and
    /// `host_cleanup_branches`, which `open` has never run. Importing them into
    /// startup would refuse to open a store for reasons unrelated to the
    /// projection being repaired -- a strictly worse outcome than the gap. Those
    /// validators still run for every explicit `rebuild_projections` caller, and
    /// the same invariants are enforced per command at use time.
    fn rebuild_projection_tables(&mut self) -> Result<ProjectionRebuild, StoreError> {
        self.with_rebuild_transaction(rebuild_projection_tables_tx)
    }

    /// Execute a command in one IMMEDIATE writer transaction.
    pub fn execute(&mut self, envelope: CommandEnvelope) -> Result<CommandReceipt, StoreError> {
        command_bus::execute(self, envelope)
    }

    /// Execute a host-normalized CreateTask with opaque workspace authority.
    pub(crate) fn execute_authorized(
        &mut self,
        envelope: CommandEnvelope,
        authorization: WorkspaceAuthorization,
    ) -> Result<CommandReceipt, StoreError> {
        command_bus::execute_authorized(self, envelope, authorization)
    }

    #[cfg(test)]
    pub(crate) fn execute_for_test(
        &mut self,
        envelope: CommandEnvelope,
    ) -> Result<CommandReceipt, StoreError> {
        command_bus::execute_for_test(self, envelope)
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

    /// Validate and claim one exact task-owned provider terminal identity.
    ///
    /// The caller must provide the durable `ResourceId` admitted for the
    /// provider tab. This method joins that resource with the durable agent
    /// projection and checks task, provider, lifecycle, and generation in one
    /// read transaction. It never searches by generation or PTY identity and
    /// never accepts a caller-supplied provider session ID; that ID remains
    /// solely the value captured in `AgentSessionFacts` by the correlated
    /// current-generation hook path.
    pub fn claim_agent_resource(
        &self,
        requested: AgentResourceBinding,
    ) -> Result<AgentResourceBinding, StoreError> {
        let conn = self.open_query_connection()?;
        let tx = conn.unchecked_transaction()?;
        let Some(agent) = command_bus::load_agent_session(&tx, requested.agent_session_id)? else {
            return Err(StoreError::StaleFence);
        };
        let Some(resource) = command_bus::load_resource(&tx, requested.resource_id)? else {
            return Err(StoreError::StaleFence);
        };
        let actual = AgentResourceBinding::from_facts(&agent, &resource)
            .map_err(|_| StoreError::StaleFence)?;
        if !actual.matches(requested) {
            return Err(StoreError::StaleFence);
        }
        tx.commit()?;
        Ok(actual)
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
        self.with_immediate_transaction(|tx| {
            claim_next_dispatch_in_tx(tx, now_ms()?, lease_ms, None)
        })
    }

    /// Claim only one destination class. Maintenance lanes must not consume a
    /// durable effect owned by another authority (for example, the provider
    /// lane must never claim browser or teardown work).
    pub(crate) fn claim_next_dispatch_for_destination(
        &mut self,
        destination: DestinationClass,
        lease: Duration,
    ) -> Result<Option<DispatchClaim>, StoreError> {
        let lease_ms = validate_dispatch_lease_ms(lease)?;
        self.with_immediate_transaction(|tx| {
            claim_next_dispatch_in_tx(tx, now_ms()?, lease_ms, Some(destination))
        })
    }

    /// Settle the pending `ReleaseResource` row for exactly one resource.
    ///
    /// Claim, begin, exact effect validation and `DispatchCompletion::Settled`
    /// all run in ONE IMMEDIATE transaction, and rows for other resources are
    /// stepped over without ever being claimed: a claim-then-inspect-then-release
    /// loop would take and hand back leases on unrelated releases, which is
    /// visible to every other claimant as contention it cannot explain.
    ///
    /// `Ok(None)` means there is no settleable row for this resource, which is
    /// the ordinary answer when the release has already been settled.
    pub(crate) fn settle_resource_release_for(
        &mut self,
        resource_id: ResourceId,
        lease: Duration,
    ) -> Result<Option<(TaskId, OperationId)>, StoreError> {
        let lease_ms = validate_dispatch_lease_ms(lease)?;
        self.with_immediate_transaction(|tx| {
            settle_resource_release_for_in_tx(tx, now_ms()?, lease_ms, resource_id)
        })
    }

    /// Every settleable `ReleaseResource` row, as `(task_id, resource_id)`.
    ///
    /// Read-only, and used by maintenance to converge after a crash between
    /// closing a shell and settling its release. The caller decides which of
    /// these are plain shells it no longer holds; this deliberately does not,
    /// so "what is a plain shell" keeps one definition
    /// (`ResourceRecipe::is_plain_shell`).
    ///
    /// Bounded: a maintenance tick examines at most
    /// `MAX_PENDING_RESOURCE_RELEASES_PER_SCAN` rows so one enormous backlog
    /// cannot stall the tick it runs on.
    pub(crate) fn pending_resource_releases(
        &self,
    ) -> Result<Vec<(TaskId, ResourceId)>, StoreError> {
        let conn = self.open_query_connection()?;
        let tx = conn.unchecked_transaction()?;
        let now = now_ms()?;
        let mut pending = Vec::new();
        let mut prior_candidate = None;
        for _ in 0..MAX_PENDING_RESOURCE_RELEASES_PER_SCAN {
            let Some(row) = load_next_dispatch_candidate(
                &tx,
                now,
                prior_candidate.as_ref(),
                Some(DestinationClass::ResourceRelease),
            )?
            else {
                break;
            };
            match revalidate_outbox_effect(&tx, &row) {
                Ok((effect_doc, _fence)) => {
                    if let Effect::ReleaseResource {
                        task_id,
                        resource_fence,
                        ..
                    } = &effect_doc.effect
                    {
                        pending.push((*task_id, resource_fence.resource_id));
                    }
                }
                // Not settleable now, and not this scan's business to explain.
                Err(StoreError::StaleFence) => {}
                Err(error) => return Err(error),
            }
            prior_candidate = Some(row);
        }
        tx.commit()?;
        Ok(pending)
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

    /// Return a dispatch to pending when the destination proves that no
    /// external write boundary was crossed. This reverses the provisional
    /// attempt recorded by `begin_dispatch`; it must never be used after any
    /// provider byte may have been written.
    pub(crate) fn defer_dispatch_before_boundary(
        &mut self,
        permit: &DispatchPermit,
        next_available: Duration,
    ) -> Result<(), StoreError> {
        let delay_ms = validate_dispatch_lease_ms(next_available)?;
        self.with_immediate_transaction(|tx| {
            defer_dispatch_before_boundary_in_tx(tx, permit, now_ms()?, delay_ms)
        })
    }

    /// Record a terminal result for the exact in-flight dispatch attempt.
    pub fn record_dispatch_completion(
        &mut self,
        permit: &DispatchPermit,
        completion: DispatchCompletion,
    ) -> Result<OperationState, StoreError> {
        command_bus::record_dispatch_completion(self, permit, completion)
    }

    /// Settle DeliverProviderInput only after a live managed-session write
    /// receipt matches the exact Effect identity, action, and bounded bytes.
    pub fn settle_provider_input_delivery(
        &mut self,
        permit: &DispatchPermit,
        receipt: &crate::providers::input::ProviderInputWriteReceipt,
    ) -> Result<OperationState, StoreError> {
        command_bus::settle_provider_input_delivery(self, permit, receipt)
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

    fn migrate(&mut self) -> Result<MigrationOutcome, StoreError> {
        let manifest = schema::migration_manifest();
        let mut outcome = MigrationOutcome::default();
        loop {
            let applied = load_applied_migrations(&self.conn)?;
            validate_applied_history(&applied, manifest)?;

            if applied.len() >= manifest.len() {
                detect_interrupted_partial_schema(&self.conn, &applied)?;
                return Ok(outcome);
            }

            let next_index = applied.len();
            let Some(migration) = manifest.get(next_index) else {
                return Ok(outcome);
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
            outcome.applied_versions.push(migration.version);
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

    /// Current Connect identity CAS epoch. Zero means no durable document.
    pub(crate) fn connect_identity_revision(&self) -> Result<u64, StoreError> {
        let revision: Option<i64> = self
            .conn
            .query_row(
                "SELECT cas_epoch FROM connect_identity WHERE singleton_key = 1",
                [],
                |row| row.get(0),
            )
            .optional()?;
        match revision {
            None => Ok(0),
            Some(value) => u64_from_nonnegative_i64("connect_identity.cas_epoch", value),
        }
    }

    /// Read the durable Connect identity document under a hard byte bound.
    pub(crate) fn read_connect_identity_bounded(
        &self,
        max_bytes: usize,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        let payload: Option<Vec<u8>> = self
            .conn
            .query_row(
                "SELECT payload FROM connect_identity WHERE singleton_key = 1",
                [],
                |row| row.get(0),
            )
            .optional()?;
        match payload {
            None => Ok(None),
            Some(bytes) if bytes.len() > max_bytes => Err(StoreError::CodecMismatch {
                detail: format!("connect identity exceeds {max_bytes} bytes"),
            }),
            Some(bytes) if bytes.is_empty() => Err(StoreError::Corruption),
            Some(bytes) => Ok(Some(bytes)),
        }
    }

    /// CAS replace Connect identity bytes. Leaves store unchanged on conflict.
    pub(crate) fn compare_and_swap_connect_identity(
        &mut self,
        expected_revision: u64,
        expected_bytes: Option<&[u8]>,
        bytes: &[u8],
    ) -> Result<u64, StoreError> {
        if bytes.is_empty() || bytes.len() > MAX_CONNECT_IDENTITY_BYTES {
            return Err(StoreError::CodecMismatch {
                detail: format!(
                    "connect identity payload must be 1..={MAX_CONNECT_IDENTITY_BYTES} bytes"
                ),
            });
        }
        self.with_immediate_transaction(|tx| {
            let current: Option<(i64, Vec<u8>)> = tx
                .query_row(
                    "SELECT cas_epoch, payload FROM connect_identity WHERE singleton_key = 1",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            let (current_revision, current_bytes) = match current {
                None => (0_u64, None),
                Some((epoch, payload)) => {
                    if payload.is_empty() {
                        return Err(StoreError::Corruption);
                    }
                    (
                        u64_from_nonnegative_i64("connect_identity.cas_epoch", epoch)?,
                        Some(payload),
                    )
                }
            };
            if current_revision != expected_revision || current_bytes.as_deref() != expected_bytes {
                return Err(StoreError::ConstraintViolation);
            }
            let next_revision =
                current_revision
                    .checked_add(1)
                    .ok_or(StoreError::IntegerOutOfRange {
                        field: "connect_identity.cas_epoch",
                        value: u64::MAX,
                    })?;
            let next_i64 = u64_to_sqlite_i64("connect_identity.cas_epoch", next_revision)?;
            let updated_at = now_ms()?;
            tx.execute(
                "INSERT INTO connect_identity(singleton_key, cas_epoch, payload, updated_at_ms)
                 VALUES (1, ?1, ?2, ?3)
                 ON CONFLICT(singleton_key) DO UPDATE SET
                   cas_epoch = excluded.cas_epoch,
                   payload = excluded.payload,
                   updated_at_ms = excluded.updated_at_ms",
                rusqlite::params![next_i64, bytes, updated_at],
            )?;
            Ok(next_revision)
        })
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
                         'semantic_journal_sessions', 'semantic_journal_facts',
                         'saved_prompts', 'prompt_versions', 'prompt_tags',
                         'prompt_version_variables', 'prompt_chains',
                         'prompt_chain_links', 'prompt_chain_command_receipts',
                         'prompt_chain_events', 'prompt_command_receipts',
                         'prompt_events', 'prompt_history', 'prompt_history_policy',
                         'prompt_search_state', 'prompt_search_pending',
                         'connect_identity')",
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

    // Empty migration history or a migration that predates V9 with any
    // semantic-journal object present indicates an interrupted apply. Once V9
    // is recorded, validate its complete manifest (columns, checks, foreign
    // key, unique constraints, and explicit index) rather than counting
    // objects, which cannot detect a weakened or partially rebuilt object.
    if applied_version < 9 && semantic_objects > 0 {
        return Err(StoreError::MigrationInterrupted);
    }
    if applied_version >= 9 {
        schema::validate_semantic_journal_schema(conn)?;
    }
    if applied.is_empty() && partial > 0 {
        return Err(StoreError::MigrationInterrupted);
    }
    let latest = applied.last().map(|row| row.version).unwrap_or(0);
    if latest < 13 {
        let v10_partial: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_schema
             WHERE name IN (
                'prompt_history', 'prompt_history_policy', 'prompt_search_state',
                'prompt_search_pending', 'prompt_search', 'idx_prompt_history_submitted',
                'prompt_search_data', 'prompt_search_idx', 'prompt_search_content',
                'prompt_search_docsize', 'prompt_search_config'
             )",
            [],
            |row| row.get(0),
        )?;
        if v10_partial > 0 {
            return Err(StoreError::MigrationInterrupted);
        }
    }
    if latest < 14 {
        let v14_partial: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_schema
             WHERE type = 'table' AND name = 'connect_identity'",
            [],
            |row| row.get(0),
        )?;
        if v14_partial > 0 {
            return Err(StoreError::MigrationInterrupted);
        }
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
         DELETE FROM task_terminal_strip;\n\
         DELETE FROM terminal_facts;\n\
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
         INSERT INTO terminal_facts SELECT * FROM shadow_terminal_facts;\n\
         INSERT INTO task_terminal_strip SELECT * FROM shadow_task_terminal_strip;\n\
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
#[cfg(test)]
#[path = "provider_input_delivery_tests.rs"]
mod provider_input_delivery_tests;

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
        ("terminal_facts", false) => "SELECT * FROM terminal_facts ORDER BY resource_id ASC",
        ("terminal_facts", true) => "SELECT * FROM shadow_terminal_facts ORDER BY resource_id ASC",
        ("task_terminal_strip", false) => "SELECT * FROM task_terminal_strip ORDER BY task_id ASC",
        ("task_terminal_strip", true) => {
            "SELECT * FROM shadow_task_terminal_strip ORDER BY task_id ASC"
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
        Event::TaskSettled => rmp_serde::to_vec(&TaskUnitPayload {}),
        Event::TaskReopened => rmp_serde::to_vec(&TaskUnitPayload {}),
        Event::TaskArchived => rmp_serde::to_vec(&TaskUnitPayload {}),
        Event::TaskDeleted => rmp_serde::to_vec(&TaskUnitPayload {}),
        Event::AgentSessionRegistered { agent } => {
            rmp_serde::to_vec(&AgentSessionRegisteredPayload {
                agent: agent.clone(),
            })
        }
        Event::AgentProviderSessionBound {
            agent_session_id,
            resource_id,
            provider_session_id,
            runtime_generation,
        } => rmp_serde::to_vec(&AgentProviderSessionBoundPayload {
            agent_session_id: *agent_session_id,
            resource_id: *resource_id,
            provider_session_id: provider_session_id.clone(),
            runtime_generation: *runtime_generation,
        }),
        Event::PrimaryAgentSet { agent_session_id } => rmp_serde::to_vec(&PrimaryAgentSetPayload {
            agent_session_id: *agent_session_id,
        }),
        Event::UnstartedPrimaryProviderRebound {
            agent_session_id,
            provider_kind,
        } => rmp_serde::to_vec(&UnstartedPrimaryProviderReboundPayload {
            agent_session_id: *agent_session_id,
            provider_kind: *provider_kind,
        }),
        Event::SpecialistRequested {
            specialist_id,
            requested_by,
            purpose,
            agent,
            permission,
            workspace,
            action_epoch,
            runtime_generation,
            resource_id,
        } => rmp_serde::to_vec(&SpecialistRequestedPayload {
            specialist_id: *specialist_id,
            requested_by: *requested_by,
            purpose: purpose.clone(),
            agent: agent.clone(),
            permission: *permission,
            workspace: workspace.clone(),
            action_epoch: *action_epoch,
            runtime_generation: *runtime_generation,
            resource_id: *resource_id,
        }),
        Event::PrimaryPromoted {
            previous,
            promoted,
            action_epoch,
            runtime_generation,
        } => rmp_serde::to_vec(&PrimaryPromotedPayload {
            previous: *previous,
            promoted: *promoted,
            action_epoch: *action_epoch,
            runtime_generation: *runtime_generation,
        }),
        Event::SpecialistHandoffRecorded {
            specialist_id,
            artifact,
            structured,
            action_epoch,
            runtime_generation,
        } => rmp_serde::to_vec(&SpecialistHandoffRecordedPayload {
            specialist_id: *specialist_id,
            artifact: artifact.clone(),
            structured: *structured,
            action_epoch: *action_epoch,
            runtime_generation: *runtime_generation,
        }),
        Event::SpecialistClosed {
            specialist_id,
            action_epoch,
            runtime_generation,
        } => rmp_serde::to_vec(&SpecialistClosedPayload {
            specialist_id: *specialist_id,
            action_epoch: *action_epoch,
            runtime_generation: *runtime_generation,
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
        Event::TerminalRenamed { resource_id, title } => {
            rmp_serde::to_vec(&TerminalRenamedPayload {
                resource_id: *resource_id,
                title: title.clone(),
            })
        }
        Event::TerminalCwdReported { resource_id, cwd } => {
            rmp_serde::to_vec(&TerminalCwdReportedPayload {
                resource_id: *resource_id,
                cwd: cwd.clone(),
            })
        }
        Event::TerminalExited {
            resource_id,
            code,
            summary,
        } => rmp_serde::to_vec(&TerminalExitedPayload {
            resource_id: *resource_id,
            code: *code,
            summary: summary.clone(),
        }),
        Event::TerminalActivity { resource_id } => rmp_serde::to_vec(&TerminalActivityPayload {
            resource_id: *resource_id,
        }),
        Event::TaskTerminalStripSet { strip } => rmp_serde::to_vec(&TaskTerminalStripSetPayload {
            strip: strip.clone(),
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
        Event::Browser(fact) => rmp_serde::to_vec(fact),
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
        "task.settled" => {
            let _: TaskUnitPayload = unpack(payload)?;
            Event::TaskSettled
        }
        "task.reopened" => {
            let _: TaskUnitPayload = unpack(payload)?;
            Event::TaskReopened
        }
        "task.archived" => {
            let _: TaskUnitPayload = unpack(payload)?;
            Event::TaskArchived
        }
        "task.deleted" => {
            let _: TaskUnitPayload = unpack(payload)?;
            Event::TaskDeleted
        }
        "agent_session.registered" => {
            let p: AgentSessionRegisteredPayload = unpack(payload)?;
            p.agent
                .validate_for_registration()
                .map_err(|e| StoreError::EventDecode(e.to_string()))?;
            Event::AgentSessionRegistered { agent: p.agent }
        }
        "agent_session.provider_bound" => {
            let p: AgentProviderSessionBoundPayload = unpack(payload)?;
            Event::AgentProviderSessionBound {
                agent_session_id: p.agent_session_id,
                resource_id: p.resource_id,
                provider_session_id: p.provider_session_id,
                runtime_generation: p.runtime_generation,
            }
        }
        "primary_agent.set" => {
            let p: PrimaryAgentSetPayload = unpack(payload)?;
            Event::PrimaryAgentSet {
                agent_session_id: p.agent_session_id,
            }
        }
        "agent_session.unstarted_provider_rebound" => {
            let p: UnstartedPrimaryProviderReboundPayload = unpack(payload)?;
            Event::UnstartedPrimaryProviderRebound {
                agent_session_id: p.agent_session_id,
                provider_kind: p.provider_kind,
            }
        }
        "specialist.requested" => {
            let p: SpecialistRequestedPayload = unpack(payload)?;
            p.validate()
                .map_err(|e| StoreError::EventDecode(e.to_string()))?;
            Event::SpecialistRequested {
                specialist_id: p.specialist_id,
                requested_by: p.requested_by,
                purpose: p.purpose,
                agent: p.agent,
                permission: p.permission,
                workspace: p.workspace,
                action_epoch: p.action_epoch,
                runtime_generation: p.runtime_generation,
                resource_id: p.resource_id,
            }
        }
        "primary_agent.promoted" => {
            let p: PrimaryPromotedPayload = unpack(payload)?;
            p.validate()
                .map_err(|e| StoreError::EventDecode(e.to_string()))?;
            Event::PrimaryPromoted {
                previous: p.previous,
                promoted: p.promoted,
                action_epoch: p.action_epoch,
                runtime_generation: p.runtime_generation,
            }
        }
        "specialist.handoff_recorded" => {
            let p: SpecialistHandoffRecordedPayload = unpack(payload)?;
            p.validate()
                .map_err(|e| StoreError::EventDecode(e.to_string()))?;
            Event::SpecialistHandoffRecorded {
                specialist_id: p.specialist_id,
                artifact: p.artifact,
                structured: p.structured,
                action_epoch: p.action_epoch,
                runtime_generation: p.runtime_generation,
            }
        }
        "specialist.closed" => {
            let p: SpecialistClosedPayload = unpack(payload)?;
            Event::SpecialistClosed {
                specialist_id: p.specialist_id,
                action_epoch: p.action_epoch,
                runtime_generation: p.runtime_generation,
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
        "terminal.renamed" => {
            let p: TerminalRenamedPayload = unpack(payload)?;
            crate::domain::resource::validate_terminal_title(&p.title)
                .map_err(|error| StoreError::EventDecode(error.to_string()))?;
            Event::TerminalRenamed {
                resource_id: p.resource_id,
                title: p.title,
            }
        }
        "terminal.cwd_reported" => {
            let p: TerminalCwdReportedPayload = unpack(payload)?;
            if !p.cwd.is_absolute() {
                return Err(StoreError::EventDecode(
                    "terminal cwd must be absolute".into(),
                ));
            }
            Event::TerminalCwdReported {
                resource_id: p.resource_id,
                cwd: p.cwd,
            }
        }
        "terminal.exited" => {
            let p: TerminalExitedPayload = unpack(payload)?;
            Event::TerminalExited {
                resource_id: p.resource_id,
                code: p.code,
                summary: p.summary,
            }
        }
        "terminal.activity" => {
            let p: TerminalActivityPayload = unpack(payload)?;
            Event::TerminalActivity {
                resource_id: p.resource_id,
            }
        }
        "task.terminal_strip_set" => {
            let p: TaskTerminalStripSetPayload = unpack(payload)?;
            Event::TaskTerminalStripSet { strip: p.strip }
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
        "browser.fact" => {
            let fact: crate::domain::browser::BrowserDurableFact = unpack(payload)?;
            Event::Browser(fact)
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
    validate_browser_event_task_identity(&payload, task_id)?;
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

fn validate_browser_event_task_identity(
    event: &Event,
    task_id: Option<TaskId>,
) -> Result<(), StoreError> {
    let Event::Browser(fact) = event else {
        return Ok(());
    };
    let Some(envelope_task) = task_id else {
        return Err(StoreError::CodecMismatch {
            detail: "browser fact requires a task scope".into(),
        });
    };
    if fact.task_id() != envelope_task {
        return Err(StoreError::CodecMismatch {
            detail: "browser fact task identity disagrees with event scope".into(),
        });
    }
    Ok(())
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
    destination: Option<DestinationClass>,
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
               AND (?6 IS NULL OR o.destination_class = ?6)
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
                destination.map(DestinationClass::as_str),
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
    if matches!(replay_policy, ReplayPolicy::NoAutomaticRetry) {
        // Browser NoAutomaticRetry rows are durable holds and remain
        // unclaimable. Provider input has an explicit destination-owned
        // dispatcher, so it alone may make its first (attempt 0) claim.
        return Ok(
            row.destination_class == DestinationClass::ProviderInput.as_str() && row.attempts == 0,
        );
    }
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
    destination: Option<DestinationClass>,
) -> Result<Option<DispatchClaim>, StoreError> {
    let mut prior_candidate = None;
    let mut saw_stale_fence = false;
    loop {
        let Some(row) =
            load_next_dispatch_candidate(tx, now_ms, prior_candidate.as_ref(), destination)?
        else {
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

/// Settle the pending `ReleaseResource` row naming exactly `resource_id`.
///
/// The resource lives inside the msgpack effect payload, not in a column, so
/// it cannot be a SQL predicate. Candidates are walked in the same order the
/// generic claimer uses and each one's effect is decoded BEFORE anything is
/// claimed; a row for another resource simply advances the cursor.
///
/// A stale-fenced candidate is stepped over rather than failing the whole
/// call. It is not settleable now by definition, and refusing here would mean
/// one unrelated stale row could stop every shell close on the host. The
/// maintenance sweep revisits anything left behind, so convergence is kept
/// without a fail-closed guard on a cross-cutting path.
fn settle_resource_release_for_in_tx(
    tx: &Transaction<'_>,
    now_ms: i64,
    lease_ms: i64,
    resource_id: ResourceId,
) -> Result<Option<(TaskId, OperationId)>, StoreError> {
    let mut prior_candidate = None;
    loop {
        let Some(row) = load_next_dispatch_candidate(
            tx,
            now_ms,
            prior_candidate.as_ref(),
            Some(DestinationClass::ResourceRelease),
        )?
        else {
            return Ok(None);
        };
        let effect_doc = match revalidate_outbox_effect(tx, &row) {
            Ok((effect_doc, _fence)) => effect_doc,
            Err(StoreError::StaleFence) => {
                prior_candidate = Some(row);
                continue;
            }
            Err(error) => return Err(error),
        };
        let Effect::ReleaseResource { resource_fence, .. } = &effect_doc.effect else {
            // The destination class promises this effect; anything else means
            // the row and its class disagree.
            return Err(StoreError::Corruption);
        };
        if resource_fence.resource_id != resource_id {
            prior_candidate = Some(row);
            continue;
        }
        match row.state.as_str() {
            "pending" if !pending_dispatch_is_authorized(&row, effect_doc.replay_policy)? => {
                prior_candidate = Some(row);
                continue;
            }
            "pending" | "claimed" => {
                let claim = claim_outbox_row(tx, &row, now_ms, lease_ms, row.state.as_str())?;
                let permit = begin_dispatch_in_tx(tx, now_ms, &claim)?;
                let Effect::ReleaseResource {
                    task_id,
                    resource_fence,
                    ..
                } = permit.effect()
                else {
                    return Err(StoreError::Corruption);
                };
                // `begin_dispatch_in_tx` re-reads the row, so prove the thing
                // about to be settled is still the one that was filtered for.
                if resource_fence.resource_id != resource_id {
                    return Err(StoreError::Corruption);
                }
                let task_id = *task_id;
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
                return Ok(Some((task_id, operation_id)));
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

fn defer_dispatch_before_boundary_in_tx(
    tx: &Transaction<'_>,
    permit: &DispatchPermit,
    now_ms: i64,
    delay_ms: i64,
) -> Result<(), StoreError> {
    let row = load_outbox_row_by_id(tx, permit.outbox_id())?.ok_or(StoreError::StaleClaim)?;
    let (effect_doc, fence) = revalidate_ambiguity_effect(tx, &row)?;
    validate_dispatch_permit(&row, permit, &effect_doc, fence)?;
    if row.state != "dispatching" || row.attempts <= 0 {
        return Err(StoreError::InvalidDispatchTransition);
    }
    let restored_attempts = row.attempts.checked_sub(1).ok_or(StoreError::Corruption)?;
    let available_at = lease_deadline(now_ms.max(row.available_at_ms), delay_ms)?;
    let changed = tx.execute(
        "UPDATE outbox
         SET state = 'pending', attempts = ?1, dispatch_started_at_ms = NULL,
             leased_until_ms = NULL, available_at_ms = ?2,
             reconciliation_receipt = NULL
         WHERE outbox_id = ?3 AND state = 'dispatching'
           AND lease_generation = ?4 AND attempts = ?5",
        rusqlite::params![
            restored_attempts,
            available_at,
            permit.outbox_id().as_bytes().as_slice(),
            permit.lease_generation(),
            row.attempts,
        ],
    )?;
    if changed != 1 {
        return Err(StoreError::InvalidDispatchTransition);
    }
    Ok(())
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

    /// A projection rebuild must contend for the writer lock at BEGIN.
    ///
    /// This is the whole of decision F7 stated as something that can fail. A
    /// rebuild reads before it writes -- the TEMP shadow copies take a read
    /// snapshot of main, the replay writes to it afterwards -- so under a
    /// DEFERRED transaction another process committing in between leaves the
    /// snapshot stale and the first write fails `SQLITE_BUSY_SNAPSHOT` with no
    /// retry possible. The property that rules that out is that the
    /// transaction takes the writer lock up front, and the way to observe it
    /// is a second connection already holding one: an IMMEDIATE transaction
    /// cannot open at all, while a DEFERRED one opens happily even here.
    ///
    /// The body is empty on purpose. A body that wrote would take the same
    /// `Busy` under either behaviour, which is exactly why the review found
    /// this by reading rather than by a failing test.
    #[test]
    fn a_projection_rebuild_takes_the_writer_lock_before_it_reads() {
        let directory = TempDir::new().expect("tempdir");
        let path = directory.path().join("rebuild-writer-lock.sqlite");
        let mut store = KernelStore::open(&path).expect("store");

        let blocker = Connection::open(&path).expect("second connection");
        blocker
            .execute_batch("BEGIN IMMEDIATE;")
            .expect("another process holds the writer lock");

        let contended = store.with_rebuild_transaction(|_tx| Ok(()));
        assert!(
            matches!(contended, Err(StoreError::Busy)),
            "a rebuild transaction must contend for the writer lock at BEGIN, not after its \
             first read; got {contended:?}"
        );

        // And it is only the contention: once the other writer is gone the
        // same call succeeds, so the assertion above cannot be satisfied by a
        // rebuild that is simply broken.
        blocker
            .execute_batch("ROLLBACK;")
            .expect("release the writer lock");
        store
            .with_rebuild_transaction(|_tx| Ok(()))
            .expect("an uncontended rebuild transaction opens and commits");
    }

    use crate::domain::agent::{
        AgentRole, AgentSessionFacts, AgentSessionLifecycle, ProviderSessionId,
    };
    use crate::domain::agent_resource::AgentResourceBinding;
    use crate::domain::command::{Command, CreateTaskIntent};
    use crate::domain::id::{
        AgentSessionId, ClientId, CommandId, EnvironmentId, ProjectId, ResourceId, TaskId, TurnId,
    };
    use crate::domain::operation::{CancellationReason, OperationErrorCode};
    use crate::domain::resource::{
        OwnerKind, ResourceFacts, ResourceKind, ResourceLifecycle, ResourceRecipe,
    };
    use crate::domain::task::{
        ReviewReadiness, TaskActivity, TaskAssignment, TaskAttention, TaskConnectivity,
        WorkspaceRef,
    };
    use crate::domain::{ProviderInputAction, SubmitProviderInputIntent};
    use crate::kernel::maintenance::OutboxPayloadCleanup;
    use crate::kernel::WalCheckpointOutcome;
    use crate::providers::ProviderKind;
    use rusqlite::ffi::Error as FfiError;
    use tempfile::TempDir;

    #[test]
    fn terminal_events_round_trip_through_the_store_codec() {
        let resource_id = ResourceId::new();
        let events = vec![
            Event::TerminalRenamed {
                resource_id,
                title: "build".to_string(),
            },
            Event::TerminalCwdReported {
                resource_id,
                cwd: std::path::PathBuf::from("C:/Code/demo"),
            },
            Event::TerminalExited {
                resource_id,
                code: Some(3),
                summary: "Shell exited with code 3".to_string(),
            },
            Event::TerminalActivity { resource_id },
            Event::TaskTerminalStripSet {
                strip: crate::domain::terminal_facts::TaskTerminalStrip {
                    order: vec![resource_id],
                    focused: Some(resource_id),
                },
            },
        ];
        for event in events {
            let packed = encode_event_payload(&event).expect("encode terminal event");
            let decoded =
                decode_stored_event(event.event_type(), i64::from(EVENT_SCHEMA_VERSION), &packed)
                    .expect("decode terminal event");
            assert_eq!(decoded, event);
        }

        // The decoder is the durable gate for the title rule.
        let bad_title = rmp_serde::to_vec(&TerminalRenamedPayload {
            resource_id,
            title: "  padded  ".to_string(),
        })
        .expect("pack bad title");
        assert!(matches!(
            decode_stored_event(
                "terminal.renamed",
                i64::from(EVENT_SCHEMA_VERSION),
                &bad_title
            ),
            Err(StoreError::EventDecode(_))
        ));
        let relative_cwd = rmp_serde::to_vec(&TerminalCwdReportedPayload {
            resource_id,
            cwd: std::path::PathBuf::from("demo"),
        })
        .expect("pack relative cwd");
        assert!(matches!(
            decode_stored_event(
                "terminal.cwd_reported",
                i64::from(EVENT_SCHEMA_VERSION),
                &relative_cwd
            ),
            Err(StoreError::EventDecode(_))
        ));
    }

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
            .execute_for_test(maintenance_envelope(
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
    fn claim_agent_resource_requires_exact_durable_identity_and_generation() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("kernel.sqlite3");
        let mut store = KernelStore::open(&path).expect("open");
        let task_id = seed_open_task(&mut store);
        let agent_session_id = AgentSessionId::new();
        let resource_id = ResourceId::new();
        let agent = AgentSessionFacts {
            id: agent_session_id,
            task_id,
            role: AgentRole::Primary,
            provider_kind: ProviderKind::Codex,
            provider_session_id: Some(ProviderSessionId::new("hook-session").expect("session")),
            lifecycle: AgentSessionLifecycle::Open,
            runtime_generation: 7,
            revision: 0,
        };
        store
            .execute(maintenance_envelope(
                CommandId::new(),
                Some(task_id),
                Some(1),
                Command::RegisterAgentSession { agent },
            ))
            .expect("register agent");
        let resource = ResourceFacts {
            id: resource_id,
            task_id: Some(task_id),
            owner_kind: OwnerKind::Task,
            resource_kind: ResourceKind::Terminal,
            recipe: ResourceRecipe::terminal(120, 40),
            lifecycle: ResourceLifecycle::Active,
            runtime_generation: 7,
            updated_at_ms: 1,
        };
        store
            .execute(maintenance_envelope(
                CommandId::new(),
                Some(task_id),
                Some(2),
                Command::RegisterResource { resource },
            ))
            .expect("register resource");

        let claim = AgentResourceBinding {
            task_id,
            agent_session_id,
            resource_id,
            provider_kind: ProviderKind::Codex,
            runtime_generation: 7,
        };
        assert_eq!(store.claim_agent_resource(claim).expect("claim"), claim);
        assert_eq!(
            store.claim_agent_resource(AgentResourceBinding {
                runtime_generation: 8,
                ..claim
            }),
            Err(StoreError::StaleFence)
        );
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

    #[test]
    fn decode_rejects_browser_fact_task_identity_mismatch() {
        use crate::domain::browser::BrowserDurableFact;
        use crate::domain::id::{BrowserContextId, EventId};

        let envelope = TaskId::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x50,
        ])
        .unwrap();
        let embedded = TaskId::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x51,
        ])
        .unwrap();
        let fact = BrowserDurableFact::ContextClosed {
            context_id: BrowserContextId::from_bytes([
                0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x52,
            ])
            .unwrap(),
            task_id: embedded,
            generation: 1,
        };
        let payload = rmp_serde::to_vec(&fact).expect("encode browser fact");
        let event_id = EventId::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x53,
        ])
        .unwrap();
        let err = decode_stored_domain_event(
            1,
            event_id.as_bytes(),
            Some(envelope.as_bytes()),
            Some(1),
            "browser.fact",
            i64::from(EVENT_SCHEMA_VERSION),
            1,
            &payload,
        )
        .expect_err("mismatched browser identity");
        assert!(
            matches!(
                err,
                StoreError::CodecMismatch { ref detail }
                    if detail.contains("browser fact task identity")
            ),
            "got {err:?}"
        );
    }

    #[test]
    fn connect_identity_cas_round_trip_and_conflict() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("kernel.sqlite3");
        let mut store = KernelStore::open(&path).expect("open");
        assert_eq!(store.connect_identity_revision().expect("rev"), 0);
        assert_eq!(
            store
                .read_connect_identity_bounded(MAX_CONNECT_IDENTITY_BYTES)
                .expect("empty"),
            None
        );
        let first = b"identity-doc-v1";
        let rev = store
            .compare_and_swap_connect_identity(0, None, first)
            .expect("cas");
        assert_eq!(rev, 1);
        assert_eq!(
            store
                .read_connect_identity_bounded(MAX_CONNECT_IDENTITY_BYTES)
                .expect("read"),
            Some(first.to_vec())
        );
        assert!(matches!(
            store.compare_and_swap_connect_identity(0, None, b"stale"),
            Err(StoreError::ConstraintViolation)
        ));
        let second = b"identity-doc-v2";
        assert_eq!(
            store
                .compare_and_swap_connect_identity(1, Some(first), second)
                .expect("exact"),
            2
        );
    }

    #[test]
    fn codex_provider_input_accepts_without_unavailable_external_session_identity() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("codex-input-without-session-id.sqlite3");
        let mut store = KernelStore::open(&path).expect("open");
        let task_id = seed_open_task(&mut store);
        let client_id = ClientId::new();
        let agent_session_id = AgentSessionId::new();
        let registered = store
            .execute_for_test(CommandEnvelope {
                command_id: CommandId::new(),
                client_id,
                task_id: Some(task_id),
                issued_at_ms: 1_725_000_000_110,
                expected_task_revision: Some(1),
                command: Command::RegisterAgentSession {
                    agent: AgentSessionFacts {
                        id: agent_session_id,
                        task_id,
                        role: AgentRole::Primary,
                        provider_kind: ProviderKind::Codex,
                        provider_session_id: None,
                        lifecycle: AgentSessionLifecycle::Open,
                        runtime_generation: 1,
                        revision: 0,
                    },
                },
            })
            .expect("register Codex agent without upstream session identity");
        let CommandReceipt::Accepted {
            task_revision: Some(revision),
            ..
        } = registered
        else {
            panic!("agent registration must be accepted: {registered:?}");
        };

        let receipt = store
            .execute_for_test(CommandEnvelope {
                command_id: CommandId::new(),
                client_id,
                task_id: Some(task_id),
                issued_at_ms: 1_725_000_000_120,
                expected_task_revision: Some(revision),
                command: Command::SubmitProviderInput(
                    SubmitProviderInputIntent::try_new(
                        agent_session_id,
                        1,
                        TurnId::new(),
                        0,
                        None,
                        None,
                        ProviderInputAction::SendNow {
                            text: "Codex prompt".into(),
                            wait: false,
                            images: Vec::new(),
                        },
                    )
                    .expect("intent"),
                ),
            })
            .expect("Codex prompt must not require an unsupported external session id");
        assert!(matches!(receipt, CommandReceipt::Accepted { .. }));
    }

    #[test]
    fn provider_no_retry_allows_first_attempt_and_pre_boundary_deferral() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("provider-first-attempt.sqlite3");
        let mut store = KernelStore::open(&path).expect("open");
        let task_id = seed_open_task(&mut store);
        let client_id = ClientId::new();
        let agent_session_id = AgentSessionId::new();
        let registered = store
            .execute_for_test(CommandEnvelope {
                command_id: CommandId::new(),
                client_id,
                task_id: Some(task_id),
                issued_at_ms: 1_725_000_000_110,
                expected_task_revision: Some(1),
                command: Command::RegisterAgentSession {
                    agent: AgentSessionFacts {
                        id: agent_session_id,
                        task_id,
                        role: AgentRole::Primary,
                        provider_kind: ProviderKind::Codex,
                        provider_session_id: Some(
                            ProviderSessionId::new("codex-first-attempt").expect("session"),
                        ),
                        lifecycle: AgentSessionLifecycle::Open,
                        runtime_generation: 3,
                        revision: 0,
                    },
                },
            })
            .expect("register agent");
        let CommandReceipt::Accepted {
            task_revision: Some(revision),
            ..
        } = registered
        else {
            panic!("agent registration must be accepted: {registered:?}");
        };
        let action_epoch: i64 = store
            .conn
            .query_row(
                "SELECT action_epoch FROM tasks WHERE task_id = ?1",
                [task_id.as_bytes().as_slice()],
                |row| row.get(0),
            )
            .expect("action epoch");
        let accepted = store
            .execute_for_test(CommandEnvelope {
                command_id: CommandId::new(),
                client_id,
                task_id: Some(task_id),
                issued_at_ms: 1_725_000_000_120,
                expected_task_revision: Some(revision),
                command: Command::SubmitProviderInput(
                    SubmitProviderInputIntent::try_new(
                        agent_session_id,
                        3,
                        TurnId::new(),
                        u64::try_from(action_epoch).expect("epoch"),
                        None,
                        None,
                        ProviderInputAction::SendNow {
                            text: "first attempt".into(),
                            wait: false,
                            images: Vec::new(),
                        },
                    )
                    .expect("intent"),
                ),
            })
            .expect("accept provider input");
        let CommandReceipt::Accepted { operation_id, .. } = accepted else {
            panic!("provider input must be accepted: {accepted:?}");
        };

        let claim = store
            .claim_next_dispatch(Duration::from_secs(30))
            .expect("claim")
            .expect("first provider NoAutomaticRetry dispatch must be claimable");
        let permit = store.begin_dispatch(&claim).expect("begin");
        store
            .defer_dispatch_before_boundary(&permit, Duration::from_millis(1))
            .expect("defer before boundary");
        std::thread::sleep(Duration::from_millis(5));
        let claim = store
            .claim_next_dispatch(Duration::from_secs(30))
            .expect("reclaim")
            .expect("pre-boundary deferral must preserve first attempt");
        let permit = store.begin_dispatch(&claim).expect("begin after deferral");
        assert_eq!(
            store
                .record_dispatch_ambiguity(&permit, Duration::from_millis(1))
                .expect("record crossed-boundary ambiguity"),
            AmbiguityDisposition::Uncertain
        );
        assert!(matches!(
            store.operation_status(operation_id).expect("status"),
            Some(OperationState::Uncertain { .. })
        ));
        assert!(store
            .claim_next_dispatch(Duration::from_secs(30))
            .expect("no retry")
            .is_none());
    }
}
