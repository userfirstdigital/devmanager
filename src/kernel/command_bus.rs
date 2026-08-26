//! Pure and side-effect command acceptance: one IMMEDIATE transaction for lookup,
//! snapshot load, decide, plan, receipt, append, projection, and optional outbox.
//!
//! Side-effect acceptance does not claim settlement or dispatch external work.
//!
//! [`CommandBus`] is the host-facing facade that owns [`KernelStore`] without
//! exposing SQLite.

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, Transaction};
use sha2::{Digest, Sha256};

use crate::domain::agent::{
    AgentRole, AgentSessionFacts, AgentSessionLifecycle, ProviderSessionId,
};
use crate::domain::agent_resource::AgentResourceBinding;
use crate::domain::artifact::{ArtifactFacts, ArtifactKind, PrivacyClass};
use crate::domain::command::{
    decide, Command, CommandEnvelope, CommandReceipt, ConfirmHostQuitIntent, RejectionCode,
    StartProviderSessionIntent,
};
use crate::domain::event::{
    apply as apply_domain_event, DomainEvent, Event, OperationAcceptedFact, OperationCancelledFact,
    OperationFailedFact, OperationSettledFact, OperationUncertainFact, EVENT_SCHEMA_VERSION,
};
use crate::domain::host::{
    HostCleanupBranch, HostCleanupBranchOutcome, HostQuitAgentBlocker, HostQuitInspection,
    HostQuitResourceBlocker, HostQuitWorktreeInspection,
};
use crate::domain::id::{
    AgentSessionId, ArtifactId, ClientId, CommandId, EnvironmentId, EventId, OperationId, OutboxId,
    ProjectId, RequestId, ResourceId, TaskId,
};
use crate::domain::operation::{
    CancellationReason, OperationErrorCode, OperationFacts, OperationOutcome, OperationOutcomeKind,
    OperationState, OperationUncertaintyCode, OutcomeSource, ResourceFence,
};
use crate::domain::query::{
    Query, QueryEnvelope, QueryError, QueryOutcome, QueryReply, QueryResult,
};
use crate::domain::resource::{OwnerKind, ResourceFacts, ResourceKind, ResourceLifecycle};
use crate::domain::snapshot::{PageLimits, TaskSnapshot, TaskSnapshotItem};
use crate::domain::task::{
    ReviewReadiness, TaskActivity, TaskAttention, TaskConnectivity, TaskFacts, TaskLifecycle,
};
use crate::kernel::artifact_content::{ArtifactContentError, ArtifactContentSession};
use crate::kernel::dispatch::{decode_absence_receipt, DispatchCompletion, DispatchPermit};
use crate::kernel::outbox::{
    decode_effect_document, decode_receipt_document, effect_document_sha256,
    encode_effect_document, encode_receipt_document, external_idempotency_key,
    is_pure_slice_decision_fact, is_side_effect_decision_fact, plan_effects, DestinationClass,
    Effect, OperationFence, PlannedEffect, PlannedEffectDocument, ReplayPolicy,
};
use crate::kernel::projector;
use crate::kernel::replay::{EventReplaySession, ReplayError};
use crate::kernel::snapshot::{SnapshotError, SnapshotSession};
use crate::kernel::store::{
    encode_event_payload, now_ms, u64_from_nonnegative_i64, u64_to_sqlite_i64, KernelStore,
    StoreError,
};
use crate::kernel::SessionScope;
use crate::prompts::projection::{
    project_prompt_store, OwnerDeviceCapability, PromptLibraryRequest, PromptProjectionError,
    PromptProjectionSubsystem,
};
use crate::prompts::store::{
    execute_prompt_command_in_tx, library_projection_revision_in_tx,
    prompt_chain_mutation_receipt_matching_command, prompt_mutation_receipt_matching_command,
    PromptStore, PromptStoreError,
};
use crate::protocol::CapabilitySet;
use crate::providers::ProviderKind;
use crate::workspace::{
    WorkspaceAuthorization, WorkspaceError, WorkspaceProjectRoots, WorkspaceResourceCoordinator,
    WorkspaceService,
};
use uuid::Uuid;

/// One advancement unit from the Closing host-cleanup journal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HostCleanupUnit {
    Idle,
    Progressed {
        task_id: TaskId,
        operation_id: OperationId,
    },
    BranchCompleted {
        operation_id: OperationId,
        action_epoch: u64,
        branch: HostCleanupBranch,
        outcome: HostCleanupBranchOutcome,
    },
    ReadyToExit {
        operation_id: OperationId,
        action_epoch: u64,
    },
    Failed {
        operation_id: OperationId,
        action_epoch: u64,
        settled_at_ms: i64,
    },
}

/// Read-only durable restart disposition unit for host bind/serve decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HostRestartDispositionUnit {
    ServeResume,
    ServeInspection {
        operation_id: OperationId,
        action_epoch: u64,
        settled_at_ms: i64,
    },
    ReadyToArmAndSettle {
        operation_id: OperationId,
        action_epoch: u64,
    },
    Closed {
        operation_id: OperationId,
        action_epoch: u64,
        settled_at_ms: i64,
    },
}

/// Host-facing command facade. Owns the durable store; does not expose SQLite.
pub struct CommandBus {
    store: KernelStore,
    prompts: PromptStore,
}

/// Internal receipt namespace. These values never become part of the public
/// receipt document, but are persisted beside it so a reused command ID cannot
/// replay a receipt across clients, sessions, tasks, or runtime epochs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ReceiptScope {
    connection_id: Option<Uuid>,
    request_id: Option<RequestId>,
    action_epoch: Option<u64>,
    runtime_generation: Option<u64>,
}

const MAX_RECEIPT_ROWS: i64 = 4_096;

/// Runtime task state with its host-reconstructed workspace projection.
/// Resource services must receive this result before they start work; the
/// durable WorkspaceRef remains the only persisted task truth.
pub struct LoadedTaskRuntime {
    pub snapshot: TaskSnapshot,
    pub workspace: WorkspaceService,
}

#[derive(Debug)]
pub enum TaskRuntimeLoadError {
    Store(StoreError),
    Workspace(WorkspaceError),
    ProjectNotConfigured(ProjectId),
}

impl std::fmt::Display for TaskRuntimeLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(error) => write!(f, "task runtime store load failed: {error}"),
            Self::Workspace(error) => write!(f, "task runtime workspace load failed: {error}"),
            Self::ProjectNotConfigured(project_id) => write!(
                f,
                "task runtime project is not configured on the host: {project_id}"
            ),
        }
    }
}

impl std::error::Error for TaskRuntimeLoadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Store(error) => Some(error),
            Self::Workspace(error) => Some(error),
            Self::ProjectNotConfigured(_) => None,
        }
    }
}

impl fmt::Debug for CommandBus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CommandBus")
    }
}

impl CommandBus {
    /// Open (or create) the kernel database at `path`.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        Ok(Self {
            store: KernelStore::open(path)?,
            prompts: PromptStore::open(path).map_err(map_prompt_store_error)?,
        })
    }

    /// Run one provider-input outbox pass on the host maintenance lane.
    pub(crate) fn run_provider_dispatch(
        &mut self,
        runtime: &crate::providers::dispatch::ProviderDispatchRuntime,
    ) -> Result<crate::providers::dispatch::ProviderDispatchOutcome, StoreError> {
        runtime.run_once(&mut self.store)
    }

    /// Execute a command through the owned store.
    pub fn execute(&mut self, envelope: CommandEnvelope) -> Result<CommandReceipt, StoreError> {
        if matches!(
            envelope.command,
            Command::PromptLibrary(_) | Command::PromptChain(_)
        ) {
            return self
                .store
                .with_immediate_transaction(|tx| execute_prompt_library_in_tx(tx, None, envelope));
        }
        if matches!(
            &envelope.command,
            Command::CreateTask(_)
                | Command::CreateTaskV2(_)
                | Command::BindProviderSession { .. }
                | Command::ServiceControl(_)
                | Command::StartProviderSession(_)
        ) {
            return Err(StoreError::HostAuthorityRequired);
        }
        self.store.execute(envelope)
    }

    /// Prompt-library mutation that requires a sealed owner or paired-owner grant.
    pub fn execute_with_owner_grant(
        &mut self,
        grant: &OwnerDeviceCapability,
        envelope: CommandEnvelope,
    ) -> Result<CommandReceipt, StoreError> {
        if !matches!(
            envelope.command,
            Command::PromptLibrary(_) | Command::PromptChain(_)
        ) {
            return self.execute(envelope);
        }
        self.store.with_immediate_transaction(|tx| {
            execute_prompt_library_in_tx(tx, Some(grant), envelope)
        })
    }

    /// Execute a host-normalized CreateTask admitted by a configured project.
    ///
    /// The authorization is opaque and cannot be constructed by callers. It
    /// is consumed here so the persistence seam cannot accept a caller-chosen
    /// durable workspace reference without a preceding host resolution.
    pub(crate) fn execute_authorized(
        &mut self,
        envelope: CommandEnvelope,
        authorization: WorkspaceAuthorization,
        request_id: RequestId,
        connection_id: Uuid,
    ) -> Result<CommandReceipt, StoreError> {
        execute_authorized_with_context(
            &mut self.store,
            envelope,
            authorization,
            request_id,
            connection_id,
        )
    }

    /// Execute a command after the authenticated host has normalized it.
    pub(crate) fn execute_host_authorized(
        &mut self,
        envelope: CommandEnvelope,
        authorization: Option<WorkspaceAuthorization>,
        request_id: RequestId,
        connection_id: Uuid,
    ) -> Result<CommandReceipt, StoreError> {
        if matches!(
            envelope.command,
            Command::PromptLibrary(_) | Command::PromptChain(_)
        ) {
            // The host transport has already authenticated the paired owner
            // and capability-gated this request. Mint the sealed in-process
            // grant only at this boundary; it never crosses the envelope.
            let grant =
                OwnerDeviceCapability::paired_owner_for_authenticated_client(envelope.client_id);
            return self.store.with_immediate_transaction(|tx| {
                execute_prompt_library_in_tx(tx, Some(&grant), envelope)
            });
        }
        if matches!(
            envelope.command,
            Command::ServiceControl(_) | Command::StartProviderSession(_)
        ) {
            return Err(StoreError::HostAuthorityRequired);
        }
        match authorization {
            Some(authorization) => {
                self.execute_authorized(envelope, authorization, request_id, connection_id)
            }
            None => execute_with_scope(
                &mut self.store,
                envelope,
                ReceiptScope {
                    connection_id: (!connection_id.is_nil()).then_some(connection_id),
                    request_id: (!connection_id.is_nil()).then_some(request_id),
                    ..ReceiptScope::default()
                },
            ),
        }
    }

    #[cfg(test)]
    pub(crate) fn execute_for_test(
        &mut self,
        envelope: CommandEnvelope,
    ) -> Result<CommandReceipt, StoreError> {
        execute_for_test(&mut self.store, envelope)
    }

    /// Query durable operation status without creating operations.
    pub fn operation_status(
        &self,
        operation_id: OperationId,
    ) -> Result<Option<OperationState>, StoreError> {
        self.store.operation_status(operation_id)
    }

    /// Load one task snapshot through a short-lived read-only query transaction.
    pub fn task_snapshot(&self, task_id: TaskId) -> Result<Option<TaskSnapshot>, StoreError> {
        let conn = self.store.open_query_connection()?;
        let tx = conn.unchecked_transaction()?;
        let snapshot = load_task_snapshot(&tx, task_id)?;
        tx.commit()?;
        Ok(snapshot)
    }

    /// Claim the exact durable provider resource identity for a later stock
    /// provider launch. The store performs the join; callers cannot replace
    /// the provider session identity or substitute a PTY-derived resource.
    pub fn claim_agent_resource(
        &self,
        requested: AgentResourceBinding,
    ) -> Result<AgentResourceBinding, StoreError> {
        self.store.claim_agent_resource(requested)
    }

    /// Resolve and claim the exact durable identity for a provider launch.
    ///
    /// This is deliberately a host effect port: the caller supplies only the
    /// explicit task/agent/resource fence, while the store supplies the
    /// current facts and performs the authoritative join. No PTY, cwd,
    /// timestamp, or transcript may participate in this lookup.
    pub fn prepare_provider_start(
        &self,
        intent: &StartProviderSessionIntent,
    ) -> Result<(AgentResourceBinding, AgentSessionFacts, TaskSnapshot), StoreError> {
        let snapshot = self
            .task_snapshot(intent.task_id)?
            .ok_or_else(|| StoreError::Projection("provider task not found".into()))?;
        if snapshot.task.revision != intent.expected_task_revision
            || snapshot.task.action_epoch != intent.expected_action_epoch
        {
            return Err(StoreError::StaleFence);
        }
        let agent = snapshot
            .agents
            .get(&intent.agent_session_id)
            .cloned()
            .ok_or_else(|| StoreError::Projection("provider agent not found".into()))?;
        if agent.provider_kind != intent.provider_kind {
            return Err(StoreError::Projection(
                "provider kind fence mismatch".into(),
            ));
        }
        let resource = snapshot
            .resources
            .get(&intent.resource_id)
            .ok_or_else(|| StoreError::Projection("provider resource not found".into()))?;
        let binding = AgentResourceBinding::from_facts(&agent, resource)
            .map_err(|error| StoreError::Projection(error.to_string()))?;
        if binding.task_id != intent.task_id
            || binding.agent_session_id != intent.agent_session_id
            || binding.resource_id != intent.resource_id
            || binding.provider_kind != intent.provider_kind
        {
            return Err(StoreError::Projection(
                "provider resource fence mismatch".into(),
            ));
        }
        let claimed = self.claim_agent_resource(binding)?;
        Ok((claimed, agent, snapshot))
    }

    /// Enumerate a bounded set of exact provider conversations that can be
    /// resumed after a host/watch restart. Both the opaque provider identity
    /// and its original resource id must have been durably bound by the same
    /// correlated SessionStart event; partial legacy rows are skipped.
    pub(crate) fn restorable_provider_starts(
        &self,
        limit: usize,
    ) -> Result<Vec<StartProviderSessionIntent>, StoreError> {
        let limit = limit.min(64);
        if limit == 0 {
            return Ok(Vec::new());
        }
        let conn = self.store.open_query_connection()?;
        let mut statement = conn.prepare(
            "SELECT t.task_id, t.revision, t.action_epoch,
                    a.agent_session_id, a.provider_kind, a.provider_resource_id
             FROM tasks t
             JOIN agent_sessions a
               ON a.agent_session_id = t.primary_agent_session_id
              AND a.task_id = t.task_id
             WHERE t.lifecycle = 'open'
               AND a.lifecycle = 'open'
               AND a.provider_session_id IS NOT NULL
               AND a.provider_resource_id IS NOT NULL
             ORDER BY t.updated_at_ms DESC, t.task_id ASC
             LIMIT ?1",
        )?;
        let mut rows = statement.query(rusqlite::params![
            i64::try_from(limit).map_err(|_| StoreError::Corruption)?
        ])?;
        let mut starts = Vec::with_capacity(limit);
        while let Some(row) = rows.next()? {
            let task_bytes: Vec<u8> = row.get(0)?;
            let revision = u64_from_nonnegative_i64("tasks.revision", row.get(1)?)?;
            let action_epoch = u64_from_nonnegative_i64("tasks.action_epoch", row.get(2)?)?;
            let agent_bytes: Vec<u8> = row.get(3)?;
            let provider_wire: String = row.get(4)?;
            let resource_bytes: Vec<u8> = row.get(5)?;
            let provider_kind = ProviderKind::parse_wire(&provider_wire)
                .ok_or_else(|| StoreError::Projection("invalid provider kind".into()))?;
            starts.push(StartProviderSessionIntent {
                task_id: id16("tasks.task_id", &task_bytes)?,
                agent_session_id: id16("agent_sessions.agent_session_id", &agent_bytes)?,
                resource_id: id16("agent_sessions.provider_resource_id", &resource_bytes)?,
                provider_kind,
                mode: crate::domain::command::ProviderStartMode::ResumeExact,
                expected_task_revision: revision,
                expected_action_epoch: action_epoch,
            });
        }
        Ok(starts)
    }

    /// Load the exact durable provider identity/resource pair for one agent.
    /// Partial rows are corruption: a provider conversation is never valid
    /// without the resource established by the same correlated hook fact.
    pub(crate) fn durable_provider_binding(
        &self,
        agent_session_id: AgentSessionId,
    ) -> Result<Option<(ProviderSessionId, ResourceId)>, StoreError> {
        let conn = self.store.open_query_connection()?;
        let row: Option<(Option<String>, Option<Vec<u8>>)> = conn
            .query_row(
                "SELECT provider_session_id, provider_resource_id
                 FROM agent_sessions WHERE agent_session_id = ?1",
                [agent_session_id.as_bytes().as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        match row {
            None | Some((None, None)) => Ok(None),
            Some((Some(provider_session_id), Some(resource_id))) => Ok(Some((
                ProviderSessionId::new(provider_session_id)
                    .map_err(|_| StoreError::Projection("invalid provider session id".into()))?,
                id16("agent_sessions.provider_resource_id", &resource_id)?,
            ))),
            Some(_) => Err(StoreError::Corruption),
        }
    }

    /// Load the durable task and reconstruct its host-owned workspace before
    /// any runtime resource can be handed out.
    pub fn load_task_runtime(
        &self,
        task_id: TaskId,
        workspace_projects: &WorkspaceProjectRoots,
    ) -> Result<Option<LoadedTaskRuntime>, TaskRuntimeLoadError> {
        self.load_task_runtime_with_coordinator(
            task_id,
            workspace_projects,
            WorkspaceResourceCoordinator::new(),
        )
    }

    pub(crate) fn load_task_runtime_with_coordinator(
        &self,
        task_id: TaskId,
        workspace_projects: &WorkspaceProjectRoots,
        coordinator: WorkspaceResourceCoordinator,
    ) -> Result<Option<LoadedTaskRuntime>, TaskRuntimeLoadError> {
        let Some(snapshot) = self
            .task_snapshot(task_id)
            .map_err(TaskRuntimeLoadError::Store)?
        else {
            return Ok(None);
        };
        let workspace = WorkspaceService::from_durable_with_task_coordinator(
            snapshot.task.project_id,
            snapshot.task.id,
            workspace_projects,
            &snapshot.task.workspace,
            coordinator,
        )
        .map_err(|error| match error {
            WorkspaceError::ProjectNotConfigured(project_id) => {
                TaskRuntimeLoadError::ProjectNotConfigured(project_id)
            }
            error => TaskRuntimeLoadError::Workspace(error),
        })?;
        Ok(Some(LoadedTaskRuntime {
            snapshot,
            workspace,
        }))
    }

    /// Inspect durable host-quit blockers through one short read-only transaction.
    ///
    /// Side-effect-free: no writes, operation allocation, or outbox work. Worktrees
    /// are always [`HostQuitWorktreeInspection::NotInspected`] and `confirmable` is
    /// always false in this slice.
    pub fn inspect_host_quit(&self) -> Result<HostQuitInspection, StoreError> {
        let conn = self.store.open_query_connection()?;
        let tx = conn.unchecked_transaction()?;
        let inspection = inspect_host_quit_in_tx(&tx)?;
        tx.commit()?;
        Ok(inspection)
    }

    /// Settle the next eligible process-empty `BeginTaskTeardown`, if any.
    ///
    /// Claim, begin, exact-effect validation, and settled completion run inside one
    /// IMMEDIATE transaction. Only SQL-filtered `task_teardown` candidates are examined.
    pub(crate) fn settle_next_process_empty_task_teardown(
        &mut self,
        lease: Duration,
    ) -> Result<Option<(TaskId, OperationId)>, StoreError> {
        self.store.settle_next_process_empty_task_teardown(lease)
    }

    /// Settle the next `ReleaseResource` outbox row as process-empty.
    ///
    /// Failed launches register a task-owned terminal without creating an OS
    /// process. Archive still requires that resource to reach `Released` before
    /// `BeginTaskTeardown` can settle.
    pub(crate) fn settle_next_resource_release(
        &mut self,
        lease: Duration,
    ) -> Result<Option<(TaskId, OperationId)>, StoreError> {
        let Some(claim) = self
            .store
            .claim_next_dispatch_for_destination(DestinationClass::ResourceRelease, lease)?
        else {
            return Ok(None);
        };
        let permit = self.store.begin_dispatch(&claim)?;
        let Effect::ReleaseResource { task_id, .. } = permit.effect() else {
            return Err(StoreError::Corruption);
        };
        let task_id = *task_id;
        let operation_id = permit.operation_id();
        let state = self
            .store
            .record_dispatch_completion(&permit, DispatchCompletion::Settled)?;
        if !matches!(state, OperationState::Settled { .. }) {
            return Err(StoreError::Corruption);
        }
        Ok(Some((task_id, operation_id)))
    }

    /// Whether the durable host admission singleton is Closing.
    pub(crate) fn host_admission_is_closing(&self) -> Result<bool, StoreError> {
        let conn = self.store.open_query_connection()?;
        let tx = conn.unchecked_transaction()?;
        let closing = host_admission_is_closing(&tx)?;
        tx.commit()?;
        Ok(closing)
    }

    /// Advance exactly one host-cleanup unit under Closing admission.
    ///
    /// Resumes at the first absent fixed branch. TaskTeardowns may Progress by
    /// settling one process-empty teardown without terminalizing the branch.
    pub(crate) fn advance_next_host_cleanup_unit(
        &mut self,
        lease: Duration,
    ) -> Result<HostCleanupUnit, StoreError> {
        self.store.advance_next_host_cleanup_unit(lease)
    }

    /// Explicit all-success host-cleanup settle for c8b after physical exit is armed.
    ///
    /// Maintenance must not call this. Exact retry is idempotent and returns the
    /// same persisted terminal [`DomainEvent`] (event id + sequence).
    pub(crate) fn settle_host_cleanup_success(&mut self) -> Result<DomainEvent, StoreError> {
        self.store.settle_host_cleanup_success()
    }

    /// Read-only durable restart disposition derived from Closing admission state.
    pub(crate) fn host_restart_disposition(
        &self,
    ) -> Result<HostRestartDispositionUnit, StoreError> {
        let conn = self.store.open_query_connection()?;
        let tx = conn.unchecked_transaction()?;
        let disposition = host_restart_disposition_in_tx(&tx)?;
        tx.commit()?;
        Ok(disposition)
    }

    /// Serve a side-effect-free query through the owned store projections only.
    ///
    /// Paged snapshot, event-replay, and artifact-content open/resume/release require
    /// the host executor registry and return [`QueryError::UnsupportedCapability`] here.
    pub fn query(&self, envelope: QueryEnvelope) -> Result<QueryReply, StoreError> {
        let outcome = match envelope.query {
            Query::OperationStatus { operation_id } => match self.operation_status(operation_id)? {
                Some(state) => QueryOutcome::Ok(QueryResult::OperationStatus {
                    operation_id,
                    state,
                }),
                None => QueryOutcome::Err(QueryError::NotFound),
            },
            Query::TaskSnapshot => {
                let Some(task_id) = envelope.task_id else {
                    return Ok(QueryReply {
                        request_id: envelope.request_id,
                        outcome: QueryOutcome::Err(QueryError::InvalidRequest),
                    });
                };
                match self.task_snapshot(task_id)? {
                    Some(snapshot) => QueryOutcome::Ok(QueryResult::TaskSnapshot {
                        snapshot: TaskSnapshotItem {
                            task: snapshot.task,
                            connectivity: snapshot.connectivity,
                            attention: snapshot.attention,
                            activity: snapshot.activity,
                            review_readiness: snapshot.review_readiness,
                            primary_agent_id: snapshot.primary_agent_id,
                        },
                    }),
                    None => QueryOutcome::Err(QueryError::NotFound),
                }
            }
            Query::InspectHostQuit => {
                if envelope.task_id.is_some() {
                    return Ok(QueryReply {
                        request_id: envelope.request_id,
                        outcome: QueryOutcome::Err(QueryError::InvalidRequest),
                    });
                }
                QueryOutcome::Ok(QueryResult::HostQuitInspection {
                    inspection: self.inspect_host_quit()?,
                })
            }
            Query::PromptLibrary(_) => QueryOutcome::Err(QueryError::UnsupportedCapability),
            Query::TaskCockpit(_) => QueryOutcome::Err(QueryError::UnsupportedCapability),
            Query::SnapshotPage { .. }
            | Query::ReleaseSnapshot { .. }
            | Query::OpenEventReplay { .. }
            | Query::ContinueEventReplay { .. }
            | Query::ReleaseEventReplay { .. }
            | Query::OpenArtifactContent { .. }
            | Query::ContinueArtifactContent { .. }
            | Query::ReleaseArtifactContent { .. } => {
                QueryOutcome::Err(QueryError::UnsupportedCapability)
            }
        };
        Ok(QueryReply {
            request_id: envelope.request_id,
            outcome,
        })
    }

    pub fn query_with_capabilities(
        &self,
        granted: CapabilitySet,
        max_document_bytes: u32,
        envelope: QueryEnvelope,
    ) -> Result<QueryReply, StoreError> {
        if matches!(envelope.query, Query::PromptLibrary(_)) {
            if !granted.grants_personal_prompt_library() {
                return Ok(QueryReply {
                    request_id: envelope.request_id,
                    outcome: QueryOutcome::Err(QueryError::UnsupportedCapability),
                });
            }
            return self.query_host_authorized(max_document_bytes, envelope);
        }
        self.query(envelope)
    }

    /// Prompt-library query after the authenticated host has capability-gated
    /// the request. Mints the sealed in-process paired-owner grant only at this
    /// boundary; it never crosses the envelope.
    pub(crate) fn query_host_authorized(
        &self,
        max_document_bytes: u32,
        envelope: QueryEnvelope,
    ) -> Result<QueryReply, StoreError> {
        if matches!(envelope.query, Query::PromptLibrary(_)) {
            let grant =
                OwnerDeviceCapability::paired_owner_for_authenticated_client(envelope.client_id);
            return self.query_with_owner_grant(&grant, max_document_bytes, envelope);
        }
        self.query(envelope)
    }

    pub fn query_with_owner_grant(
        &self,
        grant: &OwnerDeviceCapability,
        max_document_bytes: u32,
        envelope: QueryEnvelope,
    ) -> Result<QueryReply, StoreError> {
        let Query::PromptLibrary(query) = envelope.query else {
            return self.query(envelope);
        };
        if !grant.binds_client(envelope.client_id) {
            return Ok(QueryReply {
                request_id: envelope.request_id,
                outcome: QueryOutcome::Err(QueryError::UnsupportedCapability),
            });
        }
        let outcome = query_prompt_library(
            &self.prompts,
            grant,
            max_document_bytes,
            envelope.request_id,
            envelope.client_id,
            envelope.task_id,
            query,
        )?;
        Ok(QueryReply {
            request_id: envelope.request_id,
            outcome,
        })
    }

    /// Pin an immutable read snapshot through the store boundary (no SQLite escape).
    pub(crate) fn begin_snapshot(
        &self,
        limits: PageLimits,
    ) -> Result<SnapshotSession, SnapshotError> {
        self.store.begin_snapshot(limits)
    }

    pub(crate) fn begin_snapshot_scoped(
        &self,
        limits: PageLimits,
        scope: SessionScope,
    ) -> Result<SnapshotSession, SnapshotError> {
        self.store.begin_snapshot_scoped(limits, scope)
    }

    /// Pin an immutable event replay through the store boundary (no SQLite escape).
    pub(crate) fn begin_event_replay(
        &self,
        after_sequence: u64,
        limits: PageLimits,
    ) -> Result<EventReplaySession, ReplayError> {
        self.store.begin_event_replay(after_sequence, limits)
    }

    pub(crate) fn begin_event_replay_scoped(
        &self,
        after_sequence: u64,
        limits: PageLimits,
        scope: SessionScope,
    ) -> Result<EventReplaySession, ReplayError> {
        self.store
            .begin_event_replay_scoped(after_sequence, limits, scope)
    }

    /// Load InlineUtf8 artifact content into a paged session (no SQLite escape).
    pub(crate) fn begin_artifact_content(
        &self,
        client_id: ClientId,
        task_id: TaskId,
        artifact_id: ArtifactId,
        limits: PageLimits,
        max_reassembled_message_bytes: u32,
        max_physical_frame_bytes: u32,
    ) -> Result<ArtifactContentSession, ArtifactContentError> {
        self.store.begin_artifact_content(
            client_id,
            task_id,
            artifact_id,
            limits,
            max_reassembled_message_bytes,
            max_physical_frame_bytes,
        )
    }

    pub(crate) fn begin_artifact_content_scoped(
        &self,
        scope: SessionScope,
        request_id: RequestId,
        artifact_id: ArtifactId,
        limits: PageLimits,
        max_reassembled_message_bytes: u32,
        max_physical_frame_bytes: u32,
    ) -> Result<ArtifactContentSession, ArtifactContentError> {
        self.store.begin_artifact_content_scoped(
            scope,
            request_id,
            artifact_id,
            limits,
            max_reassembled_message_bytes,
            max_physical_frame_bytes,
        )
    }
}

fn map_prompt_store_error(error: PromptStoreError) -> StoreError {
    match error {
        PromptStoreError::Database(message) | PromptStoreError::Corruption(message) => {
            StoreError::Sqlite(message)
        }
        PromptStoreError::ConstraintViolation => StoreError::ConstraintViolation,
        other => StoreError::Projection(other.to_string()),
    }
}

fn prompt_unavailable_reason(subsystem: PromptProjectionSubsystem) -> &'static str {
    match subsystem {
        PromptProjectionSubsystem::SearchIndex => "search_index",
        PromptProjectionSubsystem::HistoryStore => "history_store",
        PromptProjectionSubsystem::OrganizationNamespace => "organization_namespace",
        PromptProjectionSubsystem::ChainDirectory => "chain_directory",
        PromptProjectionSubsystem::OwnerDeviceSession => "owner_device_session",
        PromptProjectionSubsystem::NegotiatedTransportLimit => "negotiated_transport_limit",
    }
}

fn map_prompt_projection_error(error: PromptProjectionError) -> QueryError {
    match error {
        PromptProjectionError::Unavailable { subsystem } => QueryError::Unavailable {
            reason: prompt_unavailable_reason(subsystem),
        },
        PromptProjectionError::PermissionDenied | PromptProjectionError::UnsupportedCapability => {
            QueryError::UnsupportedCapability
        }
        PromptProjectionError::NotFound => QueryError::NotFound,
        _ => QueryError::InvalidRequest,
    }
}

fn query_prompt_library(
    store: &PromptStore,
    grant: &OwnerDeviceCapability,
    max_document_bytes: u32,
    request_id: RequestId,
    client_id: ClientId,
    task_id: Option<TaskId>,
    query: crate::prompts::projection::PromptLibraryQuery,
) -> Result<QueryOutcome, StoreError> {
    let request = match PromptLibraryRequest::from_authenticated_query(
        request_id, client_id, task_id, query, grant,
    ) {
        Ok(request) => request,
        Err(error) => return Ok(QueryOutcome::Err(map_prompt_projection_error(error))),
    };
    match project_prompt_store(grant, &request, store, max_document_bytes) {
        Ok(reply) => Ok(QueryOutcome::Ok(QueryResult::PromptLibrary(reply))),
        Err(error) => Ok(QueryOutcome::Err(map_prompt_projection_error(error))),
    }
}

fn execute_prompt_library_in_tx(
    tx: &Transaction<'_>,
    grant: Option<&OwnerDeviceCapability>,
    envelope: CommandEnvelope,
) -> Result<CommandReceipt, StoreError> {
    if let Some(existing) = lookup_receipt(tx, envelope.command_id)? {
        return replay_prompt_library_receipt(tx, &envelope, existing);
    }
    let Some(grant) = grant else {
        return persist_rejection(
            tx,
            &envelope,
            None,
            RejectionCode::UnsupportedCapability,
            None,
            now_ms()?,
            ReceiptScope::default(),
        );
    };
    if !grant.binds_client(envelope.client_id) {
        return persist_rejection(
            tx,
            &envelope,
            None,
            RejectionCode::UnsupportedCapability,
            None,
            now_ms()?,
            ReceiptScope::default(),
        );
    }
    persist_prompt_library(tx, &envelope)
}

fn replay_prompt_library_receipt(
    tx: &Transaction<'_>,
    envelope: &CommandEnvelope,
    existing: CommandReceipt,
) -> Result<CommandReceipt, StoreError> {
    match &envelope.command {
        Command::PromptLibrary(command) => {
            match prompt_mutation_receipt_matching_command(tx, envelope.command_id, command) {
                Ok(Some(mutation)) => match existing {
                    CommandReceipt::Accepted {
                        command_id,
                        operation_id,
                        task_revision,
                        event_ids,
                        prompt_mutation: _,
                    } => Ok(CommandReceipt::Accepted {
                        command_id,
                        operation_id,
                        task_revision,
                        event_ids,
                        prompt_mutation: Some(mutation),
                    }),
                    rejected => Ok(rejected),
                },
                Ok(None) => match existing {
                    CommandReceipt::Rejected { .. } => Ok(existing),
                    CommandReceipt::Accepted { .. } => Err(StoreError::Corruption),
                },
                Err(PromptStoreError::IdempotencyConflict) => {
                    let current =
                        library_projection_revision_in_tx(tx).map_err(map_prompt_store_error)?;
                    Ok(CommandReceipt::Rejected {
                        command_id: envelope.command_id,
                        code: RejectionCode::IdempotencyConflict,
                        current_revision: Some(current),
                        resolution: None,
                    })
                }
                Err(error) => Err(map_prompt_store_error(error)),
            }
        }
        Command::PromptChain(command) => {
            match prompt_chain_mutation_receipt_matching_command(tx, envelope.command_id, command) {
                Ok(Some(_)) => Ok(existing),
                Ok(None) => match existing {
                    CommandReceipt::Rejected { .. } => Ok(existing),
                    CommandReceipt::Accepted { .. } => Err(StoreError::Corruption),
                },
                Err(PromptStoreError::IdempotencyConflict) => {
                    let current =
                        library_projection_revision_in_tx(tx).map_err(map_prompt_store_error)?;
                    Ok(CommandReceipt::Rejected {
                        command_id: envelope.command_id,
                        code: RejectionCode::IdempotencyConflict,
                        current_revision: Some(current),
                        resolution: None,
                    })
                }
                Err(error) => Err(map_prompt_store_error(error)),
            }
        }
        _ => Err(StoreError::Projection(
            "prompt library replay requires a prompt mutation".into(),
        )),
    }
}

fn persist_prompt_library(
    tx: &Transaction<'_>,
    envelope: &CommandEnvelope,
) -> Result<CommandReceipt, StoreError> {
    let accepted_at_ms = now_ms()?;
    let command = envelope.command.clone();
    if envelope.task_id.is_some() {
        return persist_rejection(
            tx,
            envelope,
            None,
            RejectionCode::InvalidTransition,
            None,
            accepted_at_ms,
            ReceiptScope::default(),
        );
    }
    let current = library_projection_revision_in_tx(tx).map_err(map_prompt_store_error)?;
    if let Some(expected) = envelope.expected_task_revision {
        if expected != current {
            return persist_rejection(
                tx,
                envelope,
                None,
                RejectionCode::RevisionConflict,
                Some(current),
                accepted_at_ms,
                ReceiptScope::default(),
            );
        }
    }
    let mutation = match command {
        Command::PromptLibrary(command) => {
            execute_prompt_command_in_tx(tx, envelope.command_id, command).map(Some)
        }
        Command::PromptChain(command) => {
            PromptStore::execute_prompt_chain_command_in_tx(tx, envelope.command_id, command)
                .map(|_| None)
        }
        _ => {
            return Err(StoreError::Projection(
                "persist_prompt_library requires a prompt mutation".into(),
            ))
        }
    };
    match mutation {
        Ok(mutation) => persist_prompt_library_acceptance(tx, envelope, mutation, accepted_at_ms),
        Err(PromptStoreError::RevisionConflict { actual, .. }) => persist_rejection(
            tx,
            envelope,
            None,
            RejectionCode::RevisionConflict,
            Some(actual),
            accepted_at_ms,
            ReceiptScope::default(),
        ),
        Err(PromptStoreError::NotFound) => persist_rejection(
            tx,
            envelope,
            None,
            RejectionCode::NotFound,
            Some(current),
            accepted_at_ms,
            ReceiptScope::default(),
        ),
        Err(PromptStoreError::AlreadyExists) => persist_rejection(
            tx,
            envelope,
            None,
            RejectionCode::AlreadyExists,
            Some(current),
            accepted_at_ms,
            ReceiptScope::default(),
        ),
        Err(PromptStoreError::InvalidTransition | PromptStoreError::Validation(_)) => {
            persist_rejection(
                tx,
                envelope,
                None,
                RejectionCode::InvalidTransition,
                Some(current),
                accepted_at_ms,
                ReceiptScope::default(),
            )
        }
        Err(PromptStoreError::IdempotencyConflict) => persist_rejection(
            tx,
            envelope,
            None,
            RejectionCode::AlreadyExists,
            Some(current),
            accepted_at_ms,
            ReceiptScope::default(),
        ),
        Err(error) => Err(map_prompt_store_error(error)),
    }
}

fn persist_prompt_library_acceptance(
    tx: &Transaction<'_>,
    envelope: &CommandEnvelope,
    mutation: Option<crate::prompts::PromptMutationReceipt>,
    accepted_at_ms: i64,
) -> Result<CommandReceipt, StoreError> {
    let operation_id = OperationId::new();
    let accepted_event_id = EventId::new();
    let settled_event_id = EventId::new();
    let receipt = CommandReceipt::Accepted {
        command_id: envelope.command_id,
        operation_id,
        task_revision: None,
        event_ids: vec![accepted_event_id],
        prompt_mutation: mutation,
    };
    insert_receipt_row(
        tx,
        envelope,
        None,
        &receipt,
        None,
        accepted_at_ms,
        ReceiptScope::default(),
    )?;
    let accepted = OperationAcceptedFact::new(
        envelope.command_id,
        operation_id,
        accepted_at_ms,
        None,
        None,
        None,
    )
    .map_err(|err| StoreError::Projection(err.to_string()))?;
    append_and_project(
        tx,
        accepted_event_id,
        None,
        None,
        accepted_at_ms,
        Event::OperationAccepted(accepted),
    )?;
    let settled = OperationSettledFact::new(
        envelope.command_id,
        operation_id,
        accepted_at_ms,
        vec![accepted_event_id],
        None,
        None,
        None,
    )
    .map_err(|err| StoreError::Projection(err.to_string()))?;
    let committed_sequence = append_and_project(
        tx,
        settled_event_id,
        None,
        None,
        accepted_at_ms,
        Event::OperationSettled(settled),
    )?;
    set_committed_sequence(tx, envelope.command_id, committed_sequence)?;
    Ok(receipt)
}

pub(crate) fn execute(
    store: &mut KernelStore,
    envelope: CommandEnvelope,
) -> Result<CommandReceipt, StoreError> {
    if matches!(
        &envelope.command,
        Command::CreateTask(_)
            | Command::CreateTaskV2(_)
            | Command::ServiceControl(_)
            | Command::StartProviderSession(_)
    ) {
        return Err(StoreError::HostAuthorityRequired);
    }
    store.with_immediate_transaction(|tx| execute_in_tx(tx, envelope, ReceiptScope::default()))
}

fn execute_with_scope(
    store: &mut KernelStore,
    envelope: CommandEnvelope,
    scope: ReceiptScope,
) -> Result<CommandReceipt, StoreError> {
    store.with_immediate_transaction(|tx| execute_in_tx(tx, envelope, scope))
}

pub(crate) fn execute_authorized(
    store: &mut KernelStore,
    envelope: CommandEnvelope,
    authorization: WorkspaceAuthorization,
) -> Result<CommandReceipt, StoreError> {
    let request_id = authorization.request_id();
    let connection_id = authorization.connection_id();
    execute_authorized_with_context(store, envelope, authorization, request_id, connection_id)
}

pub(crate) fn execute_authorized_with_context(
    store: &mut KernelStore,
    envelope: CommandEnvelope,
    authorization: WorkspaceAuthorization,
    request_id: RequestId,
    connection_id: Uuid,
) -> Result<CommandReceipt, StoreError> {
    let Command::CreateTask(intent) = &envelope.command else {
        return Err(StoreError::HostAuthorityRequired);
    };
    let task_id = intent.id;
    let project_id = intent.project_id;
    let client_id = envelope.client_id;
    let command_id = envelope.command_id;
    let workspace = intent.workspace.clone();
    if envelope.task_id.is_some()
        || !authorization.permits(
            task_id,
            project_id,
            client_id,
            connection_id,
            request_id,
            command_id,
            &workspace,
        )
    {
        return Err(StoreError::HostAuthorityRequired);
    }
    store.with_immediate_transaction(|tx| {
        if !authorization.permits(
            task_id,
            project_id,
            client_id,
            connection_id,
            request_id,
            command_id,
            &workspace,
        ) {
            return Err(StoreError::HostAuthorityRequired);
        }
        let receipt = execute_in_tx(
            tx,
            envelope,
            ReceiptScope {
                connection_id: (!connection_id.is_nil()).then_some(connection_id),
                request_id: (!connection_id.is_nil()).then_some(request_id),
                action_epoch: Some(authorization.action_epoch()),
                runtime_generation: Some(authorization.runtime_generation()),
            },
        )?;
        // Keep the host-captured pins live through the mutation and perform
        // the final identity/content check before this closure returns and
        // SQLite can commit. A rewrite or replacement observed here rolls
        // the command back instead of committing an unverified task fact.
        if !authorization.permits(
            task_id,
            project_id,
            client_id,
            connection_id,
            request_id,
            command_id,
            &workspace,
        ) {
            return Err(StoreError::HostAuthorityRequired);
        }
        Ok(receipt)
    })
}

/// Test-only fixture adapter. It resolves the fixed Main workspace against
/// the checked-out repository before using the same opaque authority as the
/// host. Tests for paging, replay, and maintenance can therefore exercise
/// ordinary command behavior without reopening the production raw-command
/// seam.
#[cfg(test)]
pub(crate) fn execute_for_test(
    store: &mut KernelStore,
    envelope: CommandEnvelope,
) -> Result<CommandReceipt, StoreError> {
    let CommandEnvelope {
        command,
        command_id,
        client_id,
        task_id,
        issued_at_ms,
        expected_task_revision,
    } = envelope;
    let Command::CreateTask(mut intent) = command else {
        return execute(
            store,
            CommandEnvelope {
                command,
                command_id,
                client_id,
                task_id,
                issued_at_ms,
                expected_task_revision,
            },
        );
    };
    if !matches!(intent.workspace, crate::domain::task::WorkspaceRef::Main) {
        return Err(StoreError::HostAuthorityRequired);
    }
    let project = tempfile::tempdir().map_err(|error| StoreError::Io(error.to_string()))?;
    std::fs::create_dir(project.path().join(".git"))
        .map_err(|error| StoreError::Io(error.to_string()))?;
    std::fs::write(
        project.path().join(".git").join("HEAD"),
        "ref: refs/heads/main\n",
    )
    .map_err(|error| StoreError::Io(error.to_string()))?;
    let workspace_projects =
        WorkspaceProjectRoots::try_from_pairs([(intent.project_id, project.path().to_path_buf())])
            .map_err(|_| StoreError::HostAuthorityRequired)?;
    let mut service = WorkspaceService::with_task_coordinator(
        intent.project_id,
        intent.id,
        &workspace_projects,
        WorkspaceResourceCoordinator::new(),
    )
    .map_err(|_| StoreError::HostAuthorityRequired)?;
    let request_id = RequestId::new();
    let connection_id = Uuid::now_v7();
    let (binding, authorization) = service
        .bind_authorized(
            crate::workspace::WorkspaceRequest::main(),
            intent.id,
            client_id,
            connection_id,
            request_id,
            command_id,
        )
        .map_err(|_| StoreError::HostAuthorityRequired)?;
    intent.workspace = binding.durable_ref().clone();
    execute_authorized_with_context(
        store,
        CommandEnvelope {
            command: Command::CreateTask(intent),
            command_id,
            client_id,
            task_id,
            issued_at_ms,
            expected_task_revision,
        },
        authorization,
        request_id,
        connection_id,
    )
}

pub(crate) fn operation_status_in_tx(
    tx: &Transaction<'_>,
    operation_id: OperationId,
) -> Result<Option<OperationState>, StoreError> {
    Ok(load_operation_facts(tx, operation_id)?.map(|facts| facts.state))
}

pub(crate) fn load_operation_facts(
    conn: &Connection,
    operation_id: OperationId,
) -> Result<Option<OperationFacts>, StoreError> {
    let Some(operation) = load_operation_projection_by_id(conn, operation_id)? else {
        return if durable_operation_lineage_exists(conn, operation_id)? {
            Err(StoreError::Corruption)
        } else {
            Ok(None)
        };
    };

    let command_id = load_operation_command_id(conn, operation_id)?;
    match lookup_receipt(conn, command_id)? {
        Some(CommandReceipt::Accepted {
            operation_id: receipt_operation_id,
            ..
        }) if receipt_operation_id == operation_id => {}
        _ => return Err(StoreError::Corruption),
    }

    let state = operation_state_from_validated_projection(&operation)?;
    Ok(Some(OperationFacts {
        id: operation_id,
        command_id,
        task_id: operation.task_id,
        state,
        accepted_at_ms: operation.accepted_at_ms,
    }))
}

fn operation_state_from_validated_projection(
    operation: &OperationProjectionRow,
) -> Result<OperationState, StoreError> {
    match operation.state.as_str() {
        "accepted" => {
            if operation.result.is_some()
                || operation.outcome_code.is_some()
                || operation.outcome_at_ms.is_some()
            {
                return Err(StoreError::Corruption);
            }
            Ok(OperationState::Accepted)
        }
        "settled" => {
            let settled_at_ms = operation.outcome_at_ms.ok_or(StoreError::Corruption)?;
            if settled_at_ms < operation.accepted_at_ms || operation.outcome_code.is_some() {
                return Err(StoreError::Corruption);
            }
            let result_event_ids = unpack_projection_blob::<Vec<EventId>>(
                "operations.result",
                operation.result.as_deref().ok_or(StoreError::Corruption)?,
            )?;
            if result_event_ids.is_empty() {
                return Err(StoreError::Corruption);
            }
            Ok(OperationState::Settled {
                settled_at_ms,
                result_event_ids,
            })
        }
        "failed" => {
            let settled_at_ms = operation.outcome_at_ms.ok_or(StoreError::Corruption)?;
            if settled_at_ms < operation.accepted_at_ms || operation.result.is_some() {
                return Err(StoreError::Corruption);
            }
            let code = match operation.outcome_code.as_deref() {
                Some("side_effect_failed") => OperationErrorCode::SideEffectFailed,
                Some("cleanup_failed") => OperationErrorCode::CleanupFailed,
                _ => return Err(StoreError::Corruption),
            };
            Ok(OperationState::Failed {
                settled_at_ms,
                code,
            })
        }
        "cancelled" => {
            let settled_at_ms = operation.outcome_at_ms.ok_or(StoreError::Corruption)?;
            if settled_at_ms < operation.accepted_at_ms
                || operation.result.is_some()
                || operation.outcome_code.as_deref() != Some("superseded")
            {
                return Err(StoreError::Corruption);
            }
            Ok(OperationState::Cancelled {
                settled_at_ms,
                reason: CancellationReason::Superseded,
            })
        }
        "uncertain" => {
            let observed_at_ms = operation.outcome_at_ms.ok_or(StoreError::Corruption)?;
            if observed_at_ms < operation.accepted_at_ms
                || operation.result.is_some()
                || operation.outcome_code.as_deref() != Some("ambiguous_dispatch")
            {
                return Err(StoreError::Corruption);
            }
            Ok(OperationState::Uncertain {
                observed_at_ms,
                code: OperationUncertaintyCode::AmbiguousDispatch,
            })
        }
        _ => Err(StoreError::Corruption),
    }
}

#[cfg(test)]
mod workspace_authority_tests {
    use super::*;

    #[test]
    fn direct_command_bus_task_creation_requires_host_normalization() {
        let directory = tempfile::tempdir().expect("temporary command bus directory");
        let mut bus =
            CommandBus::open(&directory.path().join("tasks.sqlite")).expect("command bus");
        let task_id = TaskId::new();
        let result = bus.execute(CommandEnvelope {
            command_id: CommandId::new(),
            client_id: ClientId::new(),
            task_id: None,
            issued_at_ms: 1_725_000_000_100,
            expected_task_revision: None,
            command: Command::CreateTask(crate::domain::command::CreateTaskIntent {
                id: task_id,
                environment_id: EnvironmentId::new(),
                title: "unresolved direct task".into(),
                description: None,
                project_id: ProjectId::new(),
                workspace: crate::domain::task::WorkspaceRef::Main,
                assignment: crate::domain::task::TaskAssignment::LocalOwner,
                created_at_ms: 1_725_000_000_100,
                connectivity: crate::domain::task::TaskConnectivity::Connected,
                attention: crate::domain::task::TaskAttention::None,
                activity: crate::domain::task::TaskActivity::Idle,
                review_readiness: crate::domain::task::ReviewReadiness::NotReady,
            }),
        });

        assert_eq!(result, Err(StoreError::HostAuthorityRequired));
        assert!(bus.task_snapshot(task_id).expect("task lookup").is_none());
    }
}

#[cfg(test)]
mod provider_restart_identity_tests {
    use super::*;
    use crate::domain::command::{CreateTaskIntent, ProviderStartMode};
    use crate::domain::resource::ResourceRecipe;
    use crate::domain::task::{TaskAssignment, TaskAttention, TaskConnectivity, WorkspaceRef};

    fn accepted_revision(receipt: CommandReceipt) -> u64 {
        match receipt {
            CommandReceipt::Accepted {
                task_revision: Some(revision),
                ..
            } => revision,
            other => panic!("expected accepted task receipt, got {other:?}"),
        }
    }

    fn task_envelope(
        client_id: ClientId,
        task_id: TaskId,
        revision: u64,
        command: Command,
    ) -> CommandEnvelope {
        CommandEnvelope {
            command_id: CommandId::new(),
            client_id,
            task_id: Some(task_id),
            issued_at_ms: 1_725_000_100_000,
            expected_task_revision: Some(revision),
            command,
        }
    }

    fn host_execute(bus: &mut CommandBus, envelope: CommandEnvelope) -> CommandReceipt {
        bus.execute_host_authorized(envelope, None, RequestId::new(), Uuid::now_v7())
            .expect("host-authorized command")
    }

    #[test]
    fn correlated_provider_binding_is_exact_write_once_and_restorable() {
        let directory = tempfile::tempdir().expect("provider restart directory");
        let mut bus =
            CommandBus::open(&directory.path().join("tasks.sqlite")).expect("command bus");
        let task_id = TaskId::new();
        let client_id = ClientId::new();
        let project_id = ProjectId::new();
        let created = bus
            .execute_for_test(CommandEnvelope {
                command_id: CommandId::new(),
                client_id,
                task_id: None,
                issued_at_ms: 1_725_000_000_000,
                expected_task_revision: None,
                command: Command::CreateTask(CreateTaskIntent {
                    id: task_id,
                    environment_id: EnvironmentId::new(),
                    title: "Durable provider restart".into(),
                    description: None,
                    project_id,
                    workspace: WorkspaceRef::Main,
                    assignment: TaskAssignment::LocalOwner,
                    created_at_ms: 1_725_000_000_000,
                    connectivity: TaskConnectivity::Connected,
                    attention: TaskAttention::None,
                    activity: TaskActivity::Idle,
                    review_readiness: ReviewReadiness::NotReady,
                }),
            })
            .expect("create task");
        let mut revision = accepted_revision(created);
        let agent_session_id = AgentSessionId::new();
        revision = accepted_revision(
            bus.execute(task_envelope(
                client_id,
                task_id,
                revision,
                Command::RegisterAgentSession {
                    agent: AgentSessionFacts {
                        id: agent_session_id,
                        task_id,
                        role: AgentRole::Primary,
                        provider_kind: ProviderKind::Codex,
                        provider_session_id: None,
                        lifecycle: AgentSessionLifecycle::Open,
                        runtime_generation: 7,
                        revision: 0,
                    },
                },
            ))
            .expect("register agent"),
        );
        revision = accepted_revision(
            bus.execute(task_envelope(
                client_id,
                task_id,
                revision,
                Command::SetPrimaryAgent { agent_session_id },
            ))
            .expect("set primary agent"),
        );
        let resource_id = ResourceId::new();
        let alternate_resource_id = ResourceId::new();
        for id in [resource_id, alternate_resource_id] {
            revision = accepted_revision(
                bus.execute(task_envelope(
                    client_id,
                    task_id,
                    revision,
                    Command::RegisterResource {
                        resource: ResourceFacts {
                            id,
                            task_id: Some(task_id),
                            owner_kind: OwnerKind::Task,
                            resource_kind: ResourceKind::Terminal,
                            recipe: ResourceRecipe::Terminal {
                                cols: 120,
                                rows: 40,
                            },
                            lifecycle: ResourceLifecycle::Active,
                            runtime_generation: 7,
                            updated_at_ms: 1_725_000_000_100,
                        },
                    },
                ))
                .expect("register resource"),
            );
        }
        assert!(bus
            .restorable_provider_starts(64)
            .expect("pre-bind restore query")
            .is_empty());

        let provider_session_id =
            ProviderSessionId::new("codex-durable-session").expect("provider session");
        let bind_command =
            |resource_id, provider_session_id, generation| Command::BindProviderSession {
                agent_session_id,
                resource_id,
                provider_session_id,
                expected_runtime_generation: generation,
            };
        assert_eq!(
            bus.execute(task_envelope(
                client_id,
                task_id,
                revision,
                bind_command(resource_id, provider_session_id.clone(), 7),
            )),
            Err(StoreError::HostAuthorityRequired),
            "a client cannot inject provider identity"
        );
        revision = accepted_revision(host_execute(
            &mut bus,
            task_envelope(
                client_id,
                task_id,
                revision,
                bind_command(resource_id, provider_session_id.clone(), 7),
            ),
        ));

        let starts = bus
            .restorable_provider_starts(64)
            .expect("post-bind restore query");
        assert_eq!(starts.len(), 1);
        let start = &starts[0];
        assert_eq!(start.task_id, task_id);
        assert_eq!(start.agent_session_id, agent_session_id);
        assert_eq!(start.resource_id, resource_id);
        assert_eq!(start.provider_kind, ProviderKind::Codex);
        assert_eq!(start.mode, ProviderStartMode::ResumeExact);
        assert_eq!(start.expected_task_revision, revision);

        let duplicate = host_execute(
            &mut bus,
            task_envelope(
                client_id,
                task_id,
                revision,
                bind_command(resource_id, provider_session_id.clone(), 7),
            ),
        );
        assert!(
            matches!(
                duplicate,
                CommandReceipt::Rejected {
                    code: RejectionCode::AlreadyExists,
                    ..
                }
            ),
            "duplicate binding returned {duplicate:?}"
        );
        let wrong_resource = host_execute(
            &mut bus,
            task_envelope(
                client_id,
                task_id,
                revision,
                bind_command(alternate_resource_id, provider_session_id.clone(), 7),
            ),
        );
        assert!(matches!(
            wrong_resource,
            CommandReceipt::Rejected {
                code: RejectionCode::OwnershipConflict,
                ..
            }
        ));
        let wrong_session = host_execute(
            &mut bus,
            task_envelope(
                client_id,
                task_id,
                revision,
                bind_command(
                    resource_id,
                    ProviderSessionId::new("replacement-session").expect("replacement"),
                    7,
                ),
            ),
        );
        assert!(matches!(
            wrong_session,
            CommandReceipt::Rejected {
                code: RejectionCode::OwnershipConflict,
                ..
            }
        ));
        let stale_generation = host_execute(
            &mut bus,
            task_envelope(
                client_id,
                task_id,
                revision,
                bind_command(resource_id, provider_session_id, 8),
            ),
        );
        assert!(matches!(
            stale_generation,
            CommandReceipt::Rejected {
                code: RejectionCode::InvalidTransition,
                ..
            }
        ));
        assert_eq!(
            bus.restorable_provider_starts(64)
                .expect("stable restore query")[0]
                .resource_id,
            resource_id,
            "conflicting retries must not retarget the durable conversation"
        );

        let rebuild = bus
            .store
            .rebuild_projections()
            .expect("rebuild provider projections");
        assert!(rebuild.events_replayed > 0);
        assert_eq!(
            bus.durable_provider_binding(agent_session_id)
                .expect("rebuilt durable binding"),
            Some((
                ProviderSessionId::new("codex-durable-session").expect("provider session"),
                resource_id,
            ))
        );
        assert_eq!(
            bus.restorable_provider_starts(64)
                .expect("rebuilt restore query")[0]
                .resource_id,
            resource_id,
            "projection rebuild must preserve the exact restart resource"
        );
    }
}

#[cfg(test)]
mod operation_status_tests {
    use super::*;

    #[test]
    fn operation_status_reconstructs_every_durable_state_exactly() {
        let result_event_id = EventId::new();
        let cases = vec![
            ("accepted", None, None, None, OperationState::Accepted),
            (
                "settled",
                Some(projector::pack(&vec![result_event_id]).expect("pack result")),
                None,
                Some(110),
                OperationState::Settled {
                    settled_at_ms: 110,
                    result_event_ids: vec![result_event_id],
                },
            ),
            (
                "failed",
                None,
                Some("side_effect_failed"),
                Some(120),
                OperationState::Failed {
                    settled_at_ms: 120,
                    code: OperationErrorCode::SideEffectFailed,
                },
            ),
            (
                "failed",
                None,
                Some("cleanup_failed"),
                Some(125),
                OperationState::Failed {
                    settled_at_ms: 125,
                    code: OperationErrorCode::CleanupFailed,
                },
            ),
            (
                "cancelled",
                None,
                Some("superseded"),
                Some(130),
                OperationState::Cancelled {
                    settled_at_ms: 130,
                    reason: CancellationReason::Superseded,
                },
            ),
            (
                "uncertain",
                None,
                Some("ambiguous_dispatch"),
                Some(140),
                OperationState::Uncertain {
                    observed_at_ms: 140,
                    code: OperationUncertaintyCode::AmbiguousDispatch,
                },
            ),
        ];

        for (state, result, outcome_code, outcome_at_ms, expected) in cases {
            let operation = OperationProjectionRow {
                operation_id: OperationId::new(),
                task_id: None,
                state: state.into(),
                action_epoch: None,
                resource_id: None,
                runtime_generation: None,
                result,
                outcome_code: outcome_code.map(str::to_owned),
                accepted_at_ms: 100,
                outcome_at_ms,
            };
            assert_eq!(
                operation_state_from_validated_projection(&operation).expect("valid state"),
                expected,
                "wrong mapping for {state}",
            );
        }
    }
}

#[cfg(test)]
mod receipt_scope_tests {
    use super::*;

    fn missing_task_envelope(
        command_id: CommandId,
        client_id: ClientId,
        task_id: TaskId,
    ) -> CommandEnvelope {
        CommandEnvelope {
            command_id,
            client_id,
            task_id: Some(task_id),
            issued_at_ms: 1_725_000_000_777,
            expected_task_revision: None,
            command: Command::RenameTask(crate::domain::command::RenameTaskIntent {
                title: "scope-bound receipt".into(),
            }),
        }
    }

    fn scoped(
        connection_id: Uuid,
        request_id: RequestId,
        epoch: u64,
        generation: u64,
    ) -> ReceiptScope {
        ReceiptScope {
            connection_id: Some(connection_id),
            request_id: Some(request_id),
            action_epoch: Some(epoch),
            runtime_generation: Some(generation),
        }
    }

    #[test]
    fn receipt_replay_requires_exact_client_session_task_epoch_and_command_identity() {
        let directory = tempfile::tempdir().expect("receipt scope directory");
        let mut store = KernelStore::open(&directory.path().join("tasks.sqlite")).expect("store");
        let command_id = CommandId::new();
        let client_id = ClientId::new();
        let task_id = TaskId::new();
        let connection_id = Uuid::now_v7();
        let request_id = RequestId::new();
        let base = missing_task_envelope(command_id, client_id, task_id);
        let scope = scoped(connection_id, request_id, 4, 9);

        let original = execute_with_scope(&mut store, base.clone(), scope).expect("first receipt");
        let replay =
            execute_with_scope(&mut store, base.clone(), scope).expect("same scope replay");
        assert_eq!(replay, original);

        let mut altered = base.clone();
        altered.client_id = ClientId::new();
        assert_eq!(
            execute_with_scope(&mut store, altered, scope),
            Err(StoreError::CommandIdConflict)
        );

        let mut altered = base.clone();
        altered.task_id = Some(TaskId::new());
        assert_eq!(
            execute_with_scope(&mut store, altered, scope),
            Err(StoreError::CommandIdConflict)
        );

        for altered_scope in [
            ReceiptScope {
                connection_id: Some(Uuid::now_v7()),
                ..scope
            },
            ReceiptScope {
                request_id: Some(RequestId::new()),
                ..scope
            },
            ReceiptScope {
                action_epoch: Some(scope.action_epoch.unwrap() + 1),
                ..scope
            },
            ReceiptScope {
                runtime_generation: Some(scope.runtime_generation.unwrap() + 1),
                ..scope
            },
        ] {
            assert_eq!(
                execute_with_scope(&mut store, base.clone(), altered_scope),
                Err(StoreError::CommandIdConflict)
            );
        }

        for partial_scope in [
            ReceiptScope {
                connection_id: None,
                ..scope
            },
            ReceiptScope {
                request_id: None,
                ..scope
            },
            ReceiptScope {
                action_epoch: None,
                ..scope
            },
            ReceiptScope {
                runtime_generation: None,
                ..scope
            },
        ] {
            assert_eq!(
                execute_with_scope(&mut store, base.clone(), partial_scope),
                Err(StoreError::CommandIdConflict)
            );
        }

        let mut altered = base.clone();
        altered.issued_at_ms += 1;
        assert_eq!(
            execute_with_scope(&mut store, altered, scope),
            Err(StoreError::CommandIdConflict)
        );

        let mut altered = base;
        altered.command = Command::RenameTask(crate::domain::command::RenameTaskIntent {
            title: "different command".into(),
        });
        assert_eq!(
            execute_with_scope(&mut store, altered, scope),
            Err(StoreError::CommandIdConflict)
        );
    }
}

fn record_outcome_in_tx(
    tx: &Transaction<'_>,
    outcome: OperationOutcome,
) -> Result<OperationState, StoreError> {
    outcome
        .validate()
        .map_err(|_| StoreError::ConstraintViolation)?;

    let operation = match load_operation_projection_by_id(tx, outcome.operation_id)? {
        Some(row) => row,
        None => {
            if durable_operation_lineage_exists(tx, outcome.operation_id)? {
                return Err(StoreError::Corruption);
            }
            return Err(StoreError::MissingOperation);
        }
    };
    let command_id = load_operation_command_id(tx, outcome.operation_id)?;
    let Some(task_id) = operation.task_id else {
        return Err(StoreError::Corruption);
    };

    let receipt_row = load_receipt_correlation(tx, command_id)?;
    let CommandReceipt::Accepted {
        command_id: receipt_command_id,
        operation_id: receipt_op,
        event_ids,
        task_revision: receipt_revision,
        prompt_mutation: _,
    } = &receipt_row.receipt
    else {
        return Err(StoreError::Corruption);
    };
    if *receipt_command_id != command_id
        || *receipt_op != outcome.operation_id
        || receipt_row.task_id != Some(task_id)
    {
        return Err(StoreError::Corruption);
    }

    // Strict accepted receipt/decision/accepted-fact/outbox correlation before any writes
    // or idempotent returns. Corrupt lineage fails closed with zero writes.
    let committed_sequence_i64 = receipt_row
        .committed_sequence
        .map(|seq| u64_to_sqlite_i64("command_receipts.committed_sequence", seq))
        .transpose()?;
    validate_accepted_receipt_correlation(
        tx,
        command_id,
        *receipt_op,
        event_ids,
        *receipt_revision,
        receipt_row.task_id,
        committed_sequence_i64,
        receipt_row.created_at_ms,
    )?;

    // Reload operation after correlation (authoritative projection).
    let operation = load_operation_projection_by_id(tx, outcome.operation_id)?
        .ok_or(StoreError::MissingOperation)?;
    let outbox_rows = load_outbox_rows(tx, outcome.operation_id)?;
    if outbox_rows.is_empty() {
        return Err(StoreError::ConflictingOutcome);
    }
    if outbox_rows.len() != 1 {
        return Err(StoreError::Corruption);
    }
    let outbox = &outbox_rows[0];
    let committed_sequence = receipt_row
        .committed_sequence
        .ok_or(StoreError::Corruption)?;
    let fence = operation_fence_from_projection(&operation)?;

    // Accepted-fence comparison precedes idempotent matching.
    if !outcome_fences_match_accepted(&outcome, fence) {
        return Err(StoreError::StaleFence);
    }

    let history = load_operation_outcome_history(
        tx,
        task_id,
        committed_sequence,
        command_id,
        outcome.operation_id,
    )?;

    // Exact historical match: receipt correlation already proved replay/projection integrity.
    if history
        .iter()
        .any(|fact| outcome_matches_history(&outcome, fact))
    {
        return current_operation_state_from_durable(&operation, &history);
    }

    match operation.state.as_str() {
        "accepted" => {
            let effect_doc = decode_full_outbox_payload(outbox)?;
            validate_effect_matches_fence(&effect_doc.effect, task_id, fence)?;
            if !outcome.source.is_dispatch() {
                return Err(StoreError::ConflictingOutcome);
            }
            if outbox.state != "dispatching" {
                return if matches!(outbox.state.as_str(), "pending" | "claimed" | "dispatching") {
                    Err(StoreError::InvalidDispatchTransition)
                } else {
                    Err(StoreError::Corruption)
                };
            }
            if outcome.occurred_at_ms < operation.accepted_at_ms {
                return Err(StoreError::StaleFence);
            }
            require_current_effect_ownership(tx, task_id, &effect_doc.effect, fence)?;
            apply_new_outcome(
                tx,
                command_id,
                task_id,
                &outcome,
                &effect_doc.effect,
                outbox,
                "dispatching",
            )
        }
        "uncertain" => {
            let effect_doc = decode_full_outbox_payload(outbox)?;
            validate_effect_matches_fence(&effect_doc.effect, task_id, fence)?;
            let OutcomeSource::VerifiedReconciliation {
                effect_index,
                external_identity: _,
            } = &outcome.source
            else {
                return Err(StoreError::ConflictingOutcome);
            };
            if i64::from(*effect_index) != outbox.effect_index {
                return Err(StoreError::ConflictingOutcome);
            }
            if outbox.state != "uncertain" {
                return Err(StoreError::Corruption);
            }
            match &outcome.kind {
                OperationOutcomeKind::Settled { .. } | OperationOutcomeKind::Failed { .. } => {}
                _ => return Err(StoreError::ConflictingOutcome),
            }
            let uncertain_at = history
                .iter()
                .rev()
                .find_map(|fact| match fact {
                    HistoricalOutcome::Uncertain { observed_at_ms, .. } => Some(*observed_at_ms),
                    _ => None,
                })
                .ok_or(StoreError::Corruption)?;
            if outcome.occurred_at_ms < uncertain_at {
                return Err(StoreError::StaleFence);
            }
            require_current_effect_ownership(tx, task_id, &effect_doc.effect, fence)?;
            apply_new_outcome(
                tx,
                command_id,
                task_id,
                &outcome,
                &effect_doc.effect,
                outbox,
                "uncertain",
            )
        }
        "settled" | "failed" | "cancelled" => Err(StoreError::ConflictingOutcome),
        _ => Err(StoreError::Corruption),
    }
}

pub(crate) fn record_dispatch_completion(
    store: &mut KernelStore,
    permit: &DispatchPermit,
    completion: DispatchCompletion,
) -> Result<OperationState, StoreError> {
    store.with_immediate_transaction(|tx| {
        record_dispatch_completion_in_tx(tx, permit, completion, now_ms()?)
    })
}

pub(crate) fn settle_provider_input_delivery(
    store: &mut KernelStore,
    permit: &DispatchPermit,
    receipt: &crate::providers::input::ProviderInputWriteReceipt,
) -> Result<OperationState, StoreError> {
    store.with_immediate_transaction(|tx| {
        record_dispatch_completion_with_provider_receipt_in_tx(
            tx,
            permit,
            DispatchCompletion::Settled,
            now_ms()?,
            Some(receipt),
        )
    })
}

pub(crate) fn record_dispatch_completion_in_tx(
    tx: &Transaction<'_>,
    permit: &DispatchPermit,
    completion: DispatchCompletion,
    observed_at_ms: i64,
) -> Result<OperationState, StoreError> {
    record_dispatch_completion_with_provider_receipt_in_tx(
        tx,
        permit,
        completion,
        observed_at_ms,
        None,
    )
}

fn record_dispatch_completion_with_provider_receipt_in_tx(
    tx: &Transaction<'_>,
    permit: &DispatchPermit,
    completion: DispatchCompletion,
    observed_at_ms: i64,
    receipt: Option<&crate::providers::input::ProviderInputWriteReceipt>,
) -> Result<OperationState, StoreError> {
    let outbox = load_outbox_row_by_id(tx, permit.outbox_id())?.ok_or(StoreError::StaleClaim)?;
    let operation =
        load_operation_projection_by_id(tx, outbox.operation_id)?.ok_or(StoreError::Corruption)?;
    let fence = operation_fence_from_projection(&operation)?;
    let effect_doc = match operation.state.as_str() {
        "accepted" => decode_full_outbox_payload(&outbox)?,
        "settled" | "failed" | "cancelled" => {
            effect_document_for_terminal_replay(&outbox, permit.document())?
        }
        "uncertain" => return Err(StoreError::InvalidDispatchTransition),
        _ => return Err(StoreError::Corruption),
    };
    validate_dispatch_permit_identity(&outbox, permit, &effect_doc, fence)?;

    let occurred_at_ms = match operation.state.as_str() {
        "accepted" => {
            if outbox.state != "dispatching" {
                return Err(StoreError::InvalidDispatchTransition);
            }
            let (validated_doc, validated_fence) =
                validate_dispatch_candidate_lineage(tx, outbox.operation_id, outbox.outbox_id)?;
            validate_dispatch_permit_identity(&outbox, permit, &validated_doc, validated_fence)?;
            observed_at_ms
                .max(operation.accepted_at_ms)
                .max(outbox.available_at_ms)
                .max(
                    outbox
                        .dispatch_started_at_ms
                        .ok_or(StoreError::Corruption)?,
                )
        }
        "settled" | "failed" | "cancelled" => {
            operation.outcome_at_ms.ok_or(StoreError::Corruption)?
        }
        "uncertain" => return Err(StoreError::InvalidDispatchTransition),
        _ => return Err(StoreError::Corruption),
    };

    // Generic dispatch completion cannot manufacture ProviderInputDelivered.
    // Only a live managed-session write receipt that matches the exact Effect
    // identity, action, and bounded bytes may settle this destination.
    if operation.state == "accepted"
        && matches!(&effect_doc.effect, Effect::DeliverProviderInput { .. })
    {
        let Some(receipt) = receipt else {
            return Err(StoreError::InvalidDispatchTransition);
        };
        if !provider_input_receipt_matches_effect(receipt, &effect_doc.effect) {
            return Err(StoreError::InvalidDispatchTransition);
        }
    }

    let kind = match completion {
        DispatchCompletion::Settled => OperationOutcomeKind::Settled {
            result_event_ids: settled_result_ids_for_callback(tx, permit.operation_id())?,
        },
        DispatchCompletion::Failed { code } => OperationOutcomeKind::Failed { code },
        DispatchCompletion::Cancelled { reason } => OperationOutcomeKind::Cancelled { reason },
    };
    let outcome = OperationOutcome::new(
        permit.operation_id(),
        occurred_at_ms,
        permit.action_epoch(),
        permit.resource_fence(),
        OutcomeSource::Dispatch,
        kind,
    )
    .map_err(|_| StoreError::ConstraintViolation)?;
    record_outcome_in_tx(tx, outcome)
}

/// Permanently record an ambiguous provider dispatch. NoAutomaticRetry effects
/// must become durable Uncertain once bytes may have crossed the adapter
/// boundary; callers never receive a reusable dispatch permit afterward.
pub(crate) fn record_no_retry_dispatch_uncertainty_in_tx(
    tx: &Transaction<'_>,
    row: &OutboxRow,
    effect_doc: &PlannedEffectDocument,
    fence: OperationFence,
    observed_at_ms: i64,
) -> Result<(), StoreError> {
    if effect_doc.replay_policy != ReplayPolicy::NoAutomaticRetry
        || row.state != "dispatching"
        || row.attempts <= 0
        || row.reconciliation_receipt.is_some()
    {
        return Err(StoreError::InvalidDispatchTransition);
    }
    let operation =
        load_operation_projection_by_id(tx, row.operation_id)?.ok_or(StoreError::Corruption)?;
    if operation.state != "accepted" {
        return Err(StoreError::InvalidDispatchTransition);
    }
    let task_id = operation.task_id.ok_or(StoreError::Corruption)?;
    if !matches!(
        &effect_doc.effect,
        Effect::DeliverProviderInput { operation_id, .. } if *operation_id == row.operation_id
    ) {
        return Err(StoreError::Corruption);
    }
    let (stored_effect, stored_fence) =
        validate_dispatch_attempt_lineage(tx, row.operation_id, row.outbox_id)?;
    if stored_effect != *effect_doc || stored_fence != fence {
        return Err(StoreError::StaleClaim);
    }
    validate_effect_matches_fence(&effect_doc.effect, task_id, fence)?;
    // The dispatch begin transaction already proved live ownership before the
    // external boundary. Recovery must still be able to record uncertainty if
    // the provider session crashed or closed after bytes may have crossed. The
    // typed effect remains the provider identity authority; the generic fence
    // contributes only the action epoch for this provider destination.
    let command_id = load_operation_command_id(tx, row.operation_id)?;
    let (resource_id, runtime_generation) = ResourceFence::into_parts(
        ResourceFence::from_parts(fence.resource_id, fence.runtime_generation)
            .map_err(|_| StoreError::Corruption)?,
    );
    let uncertain = OperationUncertainFact::new(
        command_id,
        row.operation_id,
        observed_at_ms.max(operation.accepted_at_ms),
        OperationUncertaintyCode::AmbiguousDispatch,
        fence.action_epoch,
        resource_id,
        runtime_generation,
    )
    .map_err(|_| StoreError::ConstraintViolation)?;
    append_and_project(
        tx,
        EventId::new(),
        Some(task_id),
        None,
        observed_at_ms.max(operation.accepted_at_ms),
        Event::OperationUncertain(uncertain),
    )?;
    transition_outbox(
        tx,
        row,
        "dispatching",
        "uncertain",
        Some("ambiguous_dispatch"),
    )?;
    Ok(())
}

pub(crate) fn validate_dispatch_permit_identity(
    row: &OutboxRow,
    permit: &DispatchPermit,
    effect_doc: &crate::kernel::outbox::PlannedEffectDocument,
    fence: OperationFence,
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

/// Resolve provider-present reconciliation evidence. The caller has already
/// correlated an opaque reconciliation claim with the live row; this helper
/// owns the required durable sequence: uncertainty first, then the verified
/// terminal outcome in the same transaction. Exact terminal replays skip the
/// uncertainty write and use the ordinary durable-history matcher.
pub(crate) fn record_present_reconciliation_in_tx(
    tx: &Transaction<'_>,
    outcome: OperationOutcome,
    expected_outbox_id: OutboxId,
    expected_lease_generation: i64,
) -> Result<OperationState, StoreError> {
    outcome
        .validate()
        .map_err(|_| StoreError::ConstraintViolation)?;
    let OutcomeSource::VerifiedReconciliation { effect_index, .. } = &outcome.source else {
        return Err(StoreError::ConflictingOutcome);
    };
    if !matches!(
        &outcome.kind,
        OperationOutcomeKind::Settled { .. } | OperationOutcomeKind::Failed { .. }
    ) {
        return Err(StoreError::ConflictingOutcome);
    }

    let rows = load_outbox_rows(tx, outcome.operation_id)?;
    if rows.len() != 1 {
        return Err(StoreError::Corruption);
    }
    let outbox = &rows[0];
    if outbox.outbox_id != expected_outbox_id
        || outbox.lease_generation != expected_lease_generation
        || outbox.effect_index != i64::from(*effect_index)
    {
        return Err(StoreError::StaleClaim);
    }

    if matches!(outbox.state.as_str(), "settled" | "failed") {
        return record_outcome_in_tx(tx, outcome);
    }
    if outbox.state != "reconciling" {
        return Err(StoreError::InvalidDispatchTransition);
    }

    let operation = load_operation_projection_by_id(tx, outcome.operation_id)?
        .ok_or(StoreError::MissingOperation)?;
    if operation.state != "accepted" {
        return Err(StoreError::InvalidDispatchTransition);
    }
    let Some(task_id) = operation.task_id else {
        return Err(StoreError::Corruption);
    };
    let fence = operation_fence_from_projection(&operation)?;
    if !outcome_fences_match_accepted(&outcome, fence) {
        return Err(StoreError::StaleFence);
    }
    let started_at = outbox
        .dispatch_started_at_ms
        .ok_or(StoreError::Corruption)?;
    if outcome.occurred_at_ms < operation.accepted_at_ms
        || outcome.occurred_at_ms < started_at
        || outcome.occurred_at_ms < outbox.available_at_ms
    {
        return Err(StoreError::StaleFence);
    }

    let command_id = load_operation_command_id(tx, outcome.operation_id)?;
    let (resource_id, runtime_generation) = ResourceFence::into_parts(outcome.resource_fence);
    let uncertain = OperationUncertainFact::new(
        command_id,
        outcome.operation_id,
        outcome.occurred_at_ms,
        OperationUncertaintyCode::AmbiguousDispatch,
        outcome.action_epoch,
        resource_id,
        runtime_generation,
    )
    .map_err(|_| StoreError::ConstraintViolation)?;
    append_and_project(
        tx,
        EventId::new(),
        Some(task_id),
        None,
        outcome.occurred_at_ms,
        Event::OperationUncertain(uncertain),
    )?;
    transition_outbox(
        tx,
        outbox,
        "reconciling",
        "uncertain",
        Some("ambiguous_dispatch"),
    )?;
    record_outcome_in_tx(tx, outcome)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum HistoricalOutcome {
    Settled {
        event_id: EventId,
        sequence: u64,
        command_id: CommandId,
        operation_id: OperationId,
        settled_at_ms: i64,
        result_event_ids: Vec<EventId>,
        action_epoch: Option<u64>,
        resource_id: Option<ResourceId>,
        runtime_generation: Option<u64>,
        source: OutcomeSource,
    },
    Failed {
        event_id: EventId,
        sequence: u64,
        command_id: CommandId,
        operation_id: OperationId,
        settled_at_ms: i64,
        code: OperationErrorCode,
        action_epoch: Option<u64>,
        resource_id: Option<ResourceId>,
        runtime_generation: Option<u64>,
        source: OutcomeSource,
    },
    Cancelled {
        event_id: EventId,
        sequence: u64,
        command_id: CommandId,
        operation_id: OperationId,
        settled_at_ms: i64,
        reason: CancellationReason,
        action_epoch: Option<u64>,
        resource_id: Option<ResourceId>,
        runtime_generation: Option<u64>,
    },
    Uncertain {
        event_id: EventId,
        sequence: u64,
        command_id: CommandId,
        operation_id: OperationId,
        observed_at_ms: i64,
        code: OperationUncertaintyCode,
        action_epoch: Option<u64>,
        resource_id: Option<ResourceId>,
        runtime_generation: Option<u64>,
    },
}

struct ReceiptCorrelation {
    receipt: CommandReceipt,
    task_id: Option<TaskId>,
    committed_sequence: Option<u64>,
    created_at_ms: i64,
}

fn load_receipt_correlation(
    tx: &Transaction<'_>,
    command_id: CommandId,
) -> Result<ReceiptCorrelation, StoreError> {
    let row: Option<(Vec<u8>, Option<Vec<u8>>, Option<i64>, i64)> = tx
        .query_row(
            "SELECT receipt, task_id, committed_sequence, created_at_ms
             FROM command_receipts WHERE command_id = ?1",
            [command_id.as_bytes().as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;
    let Some((payload, row_task_id, committed_sequence, created_at_ms)) = row else {
        return Err(StoreError::Corruption);
    };
    let receipt = decode_receipt_document(&payload)?;
    let committed_sequence = match committed_sequence {
        Some(v) => Some(u64_from_nonnegative_i64(
            "command_receipts.committed_sequence",
            v,
        )?),
        None => None,
    };
    Ok(ReceiptCorrelation {
        receipt,
        task_id: parse_optional_task_scope("command_receipts.task_id", row_task_id)?,
        committed_sequence,
        created_at_ms,
    })
}

/// Revalidate the complete accepted side-effect lineage before a claim can expose
/// or begin external work. This intentionally reuses the same strict receipt path
/// as duplicate command/outcome handling so no forged extra outbox row can dispatch.
pub(crate) fn validate_dispatch_candidate_lineage(
    tx: &Transaction<'_>,
    operation_id: OperationId,
    outbox_id: OutboxId,
) -> Result<(crate::kernel::outbox::PlannedEffectDocument, OperationFence), StoreError> {
    validate_dispatch_lineage(tx, operation_id, outbox_id, true)
}

/// Validate the immutable identity of one dispatch attempt after the external
/// boundary has started. Provider ownership may have closed or advanced by the
/// time ambiguity recovery runs, so this deliberately does not require the
/// current agent row to still match the stored typed effect. Recovery may also
/// present an expired lease; its state/attempt/clock checks remain mandatory.
pub(crate) fn validate_dispatch_attempt_lineage(
    tx: &Transaction<'_>,
    operation_id: OperationId,
    outbox_id: OutboxId,
) -> Result<(crate::kernel::outbox::PlannedEffectDocument, OperationFence), StoreError> {
    let operation =
        load_operation_projection_by_id(tx, operation_id)?.ok_or(StoreError::Corruption)?;
    require_accepted_dispatch_operation(&operation)?;
    let command_id = load_operation_command_id(tx, operation_id)?;
    let receipt_row = load_receipt_correlation(tx, command_id)?;
    let CommandReceipt::Accepted {
        command_id: receipt_command_id,
        operation_id: receipt_operation_id,
        event_ids,
        task_revision,
        prompt_mutation: _,
    } = &receipt_row.receipt
    else {
        return Err(StoreError::Corruption);
    };
    if *receipt_command_id != command_id || *receipt_operation_id != operation_id {
        return Err(StoreError::Corruption);
    }
    let Some(scope) = receipt_row.task_id else {
        return Err(StoreError::Corruption);
    };
    let Some(receipt_final_revision) = *task_revision else {
        return Err(StoreError::Corruption);
    };
    if operation.task_id != Some(scope) {
        return Err(StoreError::Corruption);
    }
    let rows = load_outbox_rows(tx, operation_id)?;
    if rows.len() != 1 || rows[0].outbox_id != outbox_id {
        return Err(StoreError::Corruption);
    }
    validate_side_effect_accepted_receipt_without_agent_sessions(
        tx,
        command_id,
        operation_id,
        event_ids,
        receipt_final_revision,
        scope,
        receipt_row
            .committed_sequence
            .ok_or(StoreError::Corruption)?,
        &operation,
        &rows,
    )?;

    let row = &rows[0];
    let task_id = operation.task_id.ok_or(StoreError::Corruption)?;
    let fence = operation_fence_from_projection(&operation)?;
    let document = decode_full_outbox_payload(row)?;
    validate_effect_matches_fence(&document.effect, task_id, fence)?;
    if !matches!(
        &document.effect,
        Effect::DeliverProviderInput { operation_id: effect_operation_id, .. }
            if *effect_operation_id == operation_id
    ) || document.replay_policy != ReplayPolicy::NoAutomaticRetry
    {
        return Err(StoreError::Corruption);
    }
    Ok((document, fence))
}

fn validate_dispatch_lineage(
    tx: &Transaction<'_>,
    operation_id: OperationId,
    outbox_id: OutboxId,
    require_current_ownership: bool,
) -> Result<(crate::kernel::outbox::PlannedEffectDocument, OperationFence), StoreError> {
    let operation =
        load_operation_projection_by_id(tx, operation_id)?.ok_or(StoreError::Corruption)?;
    require_accepted_dispatch_operation(&operation)?;
    let command_id = load_operation_command_id(tx, operation_id)?;
    let receipt_row = load_receipt_correlation(tx, command_id)?;
    let CommandReceipt::Accepted {
        command_id: receipt_command_id,
        operation_id: receipt_operation_id,
        event_ids,
        task_revision,
        prompt_mutation: _,
    } = &receipt_row.receipt
    else {
        return Err(StoreError::Corruption);
    };
    if *receipt_command_id != command_id || *receipt_operation_id != operation_id {
        return Err(StoreError::Corruption);
    }
    let committed_sequence = receipt_row
        .committed_sequence
        .map(|sequence| u64_to_sqlite_i64("command_receipts.committed_sequence", sequence))
        .transpose()?;
    validate_accepted_receipt_correlation(
        tx,
        command_id,
        operation_id,
        event_ids,
        *task_revision,
        receipt_row.task_id,
        committed_sequence,
        receipt_row.created_at_ms,
    )?;

    let operation =
        load_operation_projection_by_id(tx, operation_id)?.ok_or(StoreError::Corruption)?;
    require_accepted_dispatch_operation(&operation)?;
    let rows = load_outbox_rows(tx, operation_id)?;
    if rows.len() != 1 || rows[0].outbox_id != outbox_id {
        return Err(StoreError::Corruption);
    }
    let row = &rows[0];
    let task_id = operation.task_id.ok_or(StoreError::Corruption)?;
    let fence = operation_fence_from_projection(&operation)?;
    let document = decode_full_outbox_payload(row)?;
    validate_effect_matches_fence(&document.effect, task_id, fence)?;
    if require_current_ownership {
        require_current_effect_ownership(tx, task_id, &document.effect, fence)?;
    }
    Ok((document, fence))
}

fn require_accepted_dispatch_operation(
    operation: &OperationProjectionRow,
) -> Result<(), StoreError> {
    match operation.state.as_str() {
        "accepted" => Ok(()),
        "settled" | "failed" | "cancelled" | "uncertain" => Err(StoreError::StaleFence),
        _ => Err(StoreError::Corruption),
    }
}

/// When the operations projection row is absent, distinguish a genuinely unknown
/// OperationId from durable lineage that still references it (Corruption).
fn durable_operation_lineage_exists(
    tx: &Connection,
    operation_id: OperationId,
) -> Result<bool, StoreError> {
    let outbox_hits: i64 = tx.query_row(
        "SELECT COUNT(*) FROM outbox WHERE operation_id = ?1",
        [operation_id.as_bytes().as_slice()],
        |row| row.get(0),
    )?;
    if outbox_hits > 0 {
        return Ok(true);
    }

    let mut event_stmt = tx.prepare(
        "SELECT event_type, schema_version, payload
         FROM events
         WHERE event_type IN (
             'operation.accepted', 'operation.settled', 'operation.failed',
             'operation.cancelled', 'operation.uncertain',
             'host.close_begun', 'host.cleanup_branch_completed'
         )",
    )?;
    let mut event_rows = event_stmt.query([])?;
    while let Some(row) = event_rows.next()? {
        let event_type: String = row.get(0)?;
        let schema_version: i64 = row.get(1)?;
        let payload: Vec<u8> = row.get(2)?;
        let decoded =
            crate::kernel::store::decode_stored_event(&event_type, schema_version, &payload)?;
        if event_references_operation(&decoded, operation_id) {
            return Ok(true);
        }
    }
    drop(event_rows);
    drop(event_stmt);

    let mut receipt_stmt = tx.prepare("SELECT receipt FROM command_receipts")?;
    let mut receipt_rows = receipt_stmt.query([])?;
    while let Some(row) = receipt_rows.next()? {
        let payload: Vec<u8> = row.get(0)?;
        match decode_receipt_document(&payload)? {
            CommandReceipt::Accepted {
                operation_id: receipt_op,
                ..
            } if receipt_op == operation_id => return Ok(true),
            _ => {}
        }
    }
    Ok(false)
}

fn event_references_operation(event: &Event, operation_id: OperationId) -> bool {
    match event {
        Event::OperationAccepted(fact) => fact.operation_id == operation_id,
        Event::OperationSettled(fact) => fact.operation_id == operation_id,
        Event::OperationFailed(fact) => fact.operation_id == operation_id,
        Event::OperationCancelled(fact) => fact.operation_id == operation_id,
        Event::OperationUncertain(fact) => fact.operation_id == operation_id,
        Event::HostCloseBegun {
            operation_id: begun_op,
            ..
        }
        | Event::HostCleanupBranchCompleted {
            operation_id: begun_op,
            ..
        } => *begun_op == operation_id,
        _ => false,
    }
}

fn load_operation_command_id(
    tx: &Connection,
    operation_id: OperationId,
) -> Result<CommandId, StoreError> {
    let bytes: Vec<u8> = tx
        .query_row(
            "SELECT command_id FROM operations WHERE operation_id = ?1",
            [operation_id.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .map_err(|err| match err {
            rusqlite::Error::QueryReturnedNoRows => StoreError::MissingOperation,
            other => other.into(),
        })?;
    id16::<CommandId>("operations.command_id", &bytes)
}

fn load_operation_projection_by_id(
    tx: &Connection,
    operation_id: OperationId,
) -> Result<Option<OperationProjectionRow>, StoreError> {
    let row: Option<(
        Option<Vec<u8>>,
        String,
        Option<i64>,
        Option<Vec<u8>>,
        Option<i64>,
        Option<Vec<u8>>,
        Option<String>,
        i64,
        Option<i64>,
    )> = tx
        .query_row(
            "SELECT task_id, state, action_epoch, resource_id, runtime_generation,
                    result, outcome_code, accepted_at_ms, outcome_at_ms
             FROM operations WHERE operation_id = ?1",
            [operation_id.as_bytes().as_slice()],
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
                ))
            },
        )
        .optional()?;
    let Some((
        task_bytes,
        state,
        action_epoch,
        resource_id,
        runtime_generation,
        result,
        outcome_code,
        accepted_at_ms,
        outcome_at_ms,
    )) = row
    else {
        return Ok(None);
    };
    Ok(Some(OperationProjectionRow {
        operation_id,
        task_id: parse_optional_task_scope("operations.task_id", task_bytes)?,
        state,
        action_epoch,
        resource_id,
        runtime_generation,
        result,
        outcome_code,
        accepted_at_ms,
        outcome_at_ms,
    }))
}

/// Allocate a result-event identity inside the outcome transaction, or recover
/// the already-committed identity for an exact callback replay.
pub(crate) fn settled_result_ids_for_callback(
    tx: &Transaction<'_>,
    operation_id: OperationId,
) -> Result<Vec<EventId>, StoreError> {
    let operation =
        load_operation_projection_by_id(tx, operation_id)?.ok_or(StoreError::Corruption)?;
    match operation.state.as_str() {
        "accepted" | "uncertain" => {
            if operation.result.is_some() {
                return Err(StoreError::Corruption);
            }
            Ok(vec![EventId::new()])
        }
        "settled" => {
            let result_event_ids = unpack_projection_blob::<Vec<EventId>>(
                "operations.result",
                operation.result.as_deref().ok_or(StoreError::Corruption)?,
            )?;
            if result_event_ids.len() != 1 {
                return Err(StoreError::Corruption);
            }
            Ok(result_event_ids)
        }
        "failed" | "cancelled" => {
            if operation.result.is_some() {
                return Err(StoreError::Corruption);
            }
            Ok(vec![EventId::new()])
        }
        _ => Err(StoreError::Corruption),
    }
}

fn operation_fence_from_projection(
    operation: &OperationProjectionRow,
) -> Result<OperationFence, StoreError> {
    Ok(OperationFence {
        action_epoch: match operation.action_epoch {
            Some(v) => Some(u64_from_nonnegative_i64("operations.action_epoch", v)?),
            None => None,
        },
        resource_id: match &operation.resource_id {
            Some(bytes) => Some(id16::<ResourceId>("operations.resource_id", bytes)?),
            None => None,
        },
        runtime_generation: match operation.runtime_generation {
            Some(v) => Some(u64_from_nonnegative_i64(
                "operations.runtime_generation",
                v,
            )?),
            None => None,
        },
    })
}

fn provider_input_receipt_matches_effect(
    receipt: &crate::providers::input::ProviderInputWriteReceipt,
    effect: &Effect,
) -> bool {
    let Effect::DeliverProviderInput {
        task_id,
        operation_id,
        command_id,
        client_id,
        agent_session_id,
        provider_kind,
        provider_session_id,
        runtime_generation,
        action_epoch,
        turn_id,
        question_id,
        approval_id,
        action,
        ..
    } = effect
    else {
        return false;
    };
    let identity = receipt.identity();
    identity.task_id == *task_id
        && identity.operation_id == *operation_id
        && identity.command_id == *command_id
        && identity.client_id == *client_id
        && identity.agent_session_id == *agent_session_id
        && identity.provider_kind == *provider_kind
        && identity.provider_session_id == *provider_session_id
        && identity.runtime_generation == *runtime_generation
        && identity.action_epoch == *action_epoch
        && identity.turn_id == *turn_id
        && identity.question_id == *question_id
        && identity.approval_id == *approval_id
        && receipt.action() == action
        && crate::providers::input::provider_input_action_bytes(action)
            .is_ok_and(|expected| expected == receipt.as_bytes())
        && receipt.resource_fence().runtime_generation == *runtime_generation
}

fn validate_effect_matches_fence(
    effect: &Effect,
    task_id: TaskId,
    fence: OperationFence,
) -> Result<(), StoreError> {
    match effect {
        Effect::BeginTaskTeardown {
            task_id: effect_task,
            action_epoch,
        } => {
            if *effect_task != task_id
                || fence.resource_id.is_some()
                || fence.runtime_generation.is_some()
                || fence.action_epoch != Some(*action_epoch)
            {
                return Err(StoreError::Corruption);
            }
        }
        Effect::ReleaseResource {
            task_id: effect_task,
            action_epoch,
            resource_fence,
        } => {
            if *effect_task != task_id
                || fence.action_epoch != Some(*action_epoch)
                || fence.resource_id != Some(resource_fence.resource_id)
                || fence.runtime_generation != Some(resource_fence.runtime_generation)
            {
                return Err(StoreError::Corruption);
            }
        }
        Effect::DeliverProviderInput {
            task_id: effect_task,
            action_epoch,
            runtime_generation: _,
            ..
        } => {
            if *effect_task != task_id
                || fence.resource_id.is_some()
                || fence.action_epoch != Some(*action_epoch)
                // Provider runtime identity is validated from the typed
                // effect and current ownership fence, not from the generic
                // resource fence (which must be all-or-nothing).
                || fence.runtime_generation.is_some()
            {
                return Err(StoreError::Corruption);
            }
        }
        Effect::HoldBrowserHost {
            task_id: effect_task,
            action_epoch,
            generation,
            ..
        } => {
            if *effect_task != task_id
                || fence.resource_id.is_some()
                || fence.runtime_generation.is_some()
                || fence.action_epoch != Some(*action_epoch)
                || *generation == 0
            {
                return Err(StoreError::Corruption);
            }
        }
    }
    Ok(())
}

fn outcome_fences_match_accepted(outcome: &OperationOutcome, fence: OperationFence) -> bool {
    let (resource_id, runtime_generation) = ResourceFence::into_parts(outcome.resource_fence);
    outcome.action_epoch == fence.action_epoch
        && resource_id == fence.resource_id
        && runtime_generation == fence.runtime_generation
}

fn outcome_matches_history(outcome: &OperationOutcome, fact: &HistoricalOutcome) -> bool {
    match (&outcome.kind, fact) {
        (
            OperationOutcomeKind::Settled { result_event_ids },
            HistoricalOutcome::Settled {
                settled_at_ms,
                result_event_ids: hist_ids,
                action_epoch,
                resource_id,
                runtime_generation,
                source,
                operation_id,
                ..
            },
        ) => {
            outcome.operation_id == *operation_id
                && outcome.occurred_at_ms == *settled_at_ms
                && outcome.action_epoch == *action_epoch
                && ResourceFence::into_parts(outcome.resource_fence)
                    == (*resource_id, *runtime_generation)
                && outcome.source == *source
                && result_event_ids == hist_ids
        }
        (
            OperationOutcomeKind::Failed { code },
            HistoricalOutcome::Failed {
                settled_at_ms,
                code: hist_code,
                action_epoch,
                resource_id,
                runtime_generation,
                source,
                operation_id,
                ..
            },
        ) => {
            outcome.operation_id == *operation_id
                && outcome.occurred_at_ms == *settled_at_ms
                && outcome.action_epoch == *action_epoch
                && ResourceFence::into_parts(outcome.resource_fence)
                    == (*resource_id, *runtime_generation)
                && outcome.source == *source
                && code == hist_code
        }
        (
            OperationOutcomeKind::Cancelled { reason },
            HistoricalOutcome::Cancelled {
                settled_at_ms,
                reason: hist_reason,
                action_epoch,
                resource_id,
                runtime_generation,
                operation_id,
                ..
            },
        ) => {
            outcome.operation_id == *operation_id
                && outcome.occurred_at_ms == *settled_at_ms
                && outcome.action_epoch == *action_epoch
                && ResourceFence::into_parts(outcome.resource_fence)
                    == (*resource_id, *runtime_generation)
                && outcome.source.is_dispatch()
                && reason == hist_reason
        }
        (
            OperationOutcomeKind::Uncertain { code },
            HistoricalOutcome::Uncertain {
                observed_at_ms,
                code: hist_code,
                action_epoch,
                resource_id,
                runtime_generation,
                operation_id,
                ..
            },
        ) => {
            outcome.operation_id == *operation_id
                && outcome.occurred_at_ms == *observed_at_ms
                && outcome.action_epoch == *action_epoch
                && ResourceFence::into_parts(outcome.resource_fence)
                    == (*resource_id, *runtime_generation)
                && outcome.source.is_dispatch()
                && code == hist_code
        }
        _ => false,
    }
}

fn load_operation_outcome_history(
    tx: &Connection,
    task_id: TaskId,
    after_sequence: u64,
    command_id: CommandId,
    operation_id: OperationId,
) -> Result<Vec<HistoricalOutcome>, StoreError> {
    // V1 stores operation_id only inside the payload, so matching rows must be decoded.
    // Bounded tradeoff: scan terminal operation.* rows rather than an indexed operation_id column.
    let mut stmt = tx.prepare(
        "SELECT sequence, event_id, task_id, task_revision, event_type, schema_version,
                payload, occurred_at_ms
         FROM events
         WHERE event_type IN (
             'operation.settled',
             'operation.failed',
             'operation.cancelled',
             'operation.uncertain'
         )
         ORDER BY sequence ASC",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, Vec<u8>>(1)?,
            row.get::<_, Option<Vec<u8>>>(2)?,
            row.get::<_, Option<i64>>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, i64>(5)?,
            row.get::<_, Vec<u8>>(6)?,
            row.get::<_, i64>(7)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (
            sequence_i64,
            event_id_bytes,
            task_bytes,
            task_revision,
            event_type,
            schema_version,
            payload,
            occurred_at_ms,
        ) = row?;
        let event =
            crate::kernel::store::decode_stored_event(&event_type, schema_version, &payload)?;
        let (fact_command_id, fact_operation_id) = match &event {
            Event::OperationSettled(fact) => (fact.command_id, fact.operation_id),
            Event::OperationFailed(fact) => (fact.command_id, fact.operation_id),
            Event::OperationCancelled(fact) => (fact.command_id, fact.operation_id),
            Event::OperationUncertain(fact) => (fact.command_id, fact.operation_id),
            _ => return Err(StoreError::Corruption),
        };
        let cmd_match = fact_command_id == command_id;
        let op_match = fact_operation_id == operation_id;
        if !cmd_match && !op_match {
            continue;
        }
        if !(cmd_match && op_match) {
            // Half-match (same command, different operation or vice versa) is Corruption.
            return Err(StoreError::Corruption);
        }
        // Any matching fact must belong to this task and occur after acceptance.
        let fact_task = parse_optional_task_scope("events.task_id", task_bytes)?;
        if fact_task != Some(task_id) {
            return Err(StoreError::Corruption);
        }
        if task_revision.is_some() {
            return Err(StoreError::Corruption);
        }
        let sequence = u64_from_nonnegative_i64("events.sequence", sequence_i64)?;
        if sequence <= after_sequence {
            return Err(StoreError::Corruption);
        }
        let event_id = id16::<EventId>("events.event_id", &event_id_bytes)?;
        match event {
            Event::OperationSettled(fact) => {
                if occurred_at_ms != fact.settled_at_ms {
                    return Err(StoreError::Corruption);
                }
                out.push(HistoricalOutcome::Settled {
                    event_id,
                    sequence,
                    command_id: fact.command_id,
                    operation_id: fact.operation_id,
                    settled_at_ms: fact.settled_at_ms,
                    result_event_ids: fact.result_event_ids,
                    action_epoch: fact.action_epoch,
                    resource_id: fact.resource_id,
                    runtime_generation: fact.runtime_generation,
                    source: fact.source,
                });
            }
            Event::OperationFailed(fact) => {
                if occurred_at_ms != fact.settled_at_ms {
                    return Err(StoreError::Corruption);
                }
                out.push(HistoricalOutcome::Failed {
                    event_id,
                    sequence,
                    command_id: fact.command_id,
                    operation_id: fact.operation_id,
                    settled_at_ms: fact.settled_at_ms,
                    code: fact.code,
                    action_epoch: fact.action_epoch,
                    resource_id: fact.resource_id,
                    runtime_generation: fact.runtime_generation,
                    source: fact.source,
                });
            }
            Event::OperationCancelled(fact) => {
                if occurred_at_ms != fact.settled_at_ms {
                    return Err(StoreError::Corruption);
                }
                out.push(HistoricalOutcome::Cancelled {
                    event_id,
                    sequence,
                    command_id: fact.command_id,
                    operation_id: fact.operation_id,
                    settled_at_ms: fact.settled_at_ms,
                    reason: fact.reason,
                    action_epoch: fact.action_epoch,
                    resource_id: fact.resource_id,
                    runtime_generation: fact.runtime_generation,
                });
            }
            Event::OperationUncertain(fact) => {
                if occurred_at_ms != fact.observed_at_ms {
                    return Err(StoreError::Corruption);
                }
                out.push(HistoricalOutcome::Uncertain {
                    event_id,
                    sequence,
                    command_id: fact.command_id,
                    operation_id: fact.operation_id,
                    observed_at_ms: fact.observed_at_ms,
                    code: fact.code,
                    action_epoch: fact.action_epoch,
                    resource_id: fact.resource_id,
                    runtime_generation: fact.runtime_generation,
                });
            }
            _ => return Err(StoreError::Corruption),
        }
    }
    Ok(out)
}

fn current_operation_state_from_durable(
    operation: &OperationProjectionRow,
    history: &[HistoricalOutcome],
) -> Result<OperationState, StoreError> {
    match operation.state.as_str() {
        "accepted" => Ok(OperationState::Accepted),
        "settled" => {
            let Some(HistoricalOutcome::Settled {
                settled_at_ms,
                result_event_ids,
                ..
            }) = history
                .iter()
                .rev()
                .find(|f| matches!(f, HistoricalOutcome::Settled { .. }))
            else {
                return Err(StoreError::Corruption);
            };
            Ok(OperationState::Settled {
                settled_at_ms: *settled_at_ms,
                result_event_ids: result_event_ids.clone(),
            })
        }
        "failed" => {
            let Some(HistoricalOutcome::Failed {
                settled_at_ms,
                code,
                ..
            }) = history
                .iter()
                .rev()
                .find(|f| matches!(f, HistoricalOutcome::Failed { .. }))
            else {
                return Err(StoreError::Corruption);
            };
            Ok(OperationState::Failed {
                settled_at_ms: *settled_at_ms,
                code: *code,
            })
        }
        "cancelled" => {
            let Some(HistoricalOutcome::Cancelled {
                settled_at_ms,
                reason,
                ..
            }) = history
                .iter()
                .rev()
                .find(|f| matches!(f, HistoricalOutcome::Cancelled { .. }))
            else {
                return Err(StoreError::Corruption);
            };
            Ok(OperationState::Cancelled {
                settled_at_ms: *settled_at_ms,
                reason: *reason,
            })
        }
        "uncertain" => {
            let Some(HistoricalOutcome::Uncertain {
                observed_at_ms,
                code,
                ..
            }) = history
                .iter()
                .rev()
                .find(|f| matches!(f, HistoricalOutcome::Uncertain { .. }))
            else {
                return Err(StoreError::Corruption);
            };
            Ok(OperationState::Uncertain {
                observed_at_ms: *observed_at_ms,
                code: *code,
            })
        }
        _ => Err(StoreError::Corruption),
    }
}

fn require_current_effect_ownership(
    tx: &Transaction<'_>,
    task_id: TaskId,
    effect: &Effect,
    fence: OperationFence,
) -> Result<(), StoreError> {
    match effect {
        Effect::BeginTaskTeardown { action_epoch, .. } => {
            let (lifecycle, stored_epoch): (String, i64) = tx
                .query_row(
                    "SELECT lifecycle, action_epoch FROM tasks WHERE task_id = ?1",
                    [task_id.as_bytes().as_slice()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .map_err(|err| match err {
                    rusqlite::Error::QueryReturnedNoRows => StoreError::StaleFence,
                    other => other.into(),
                })?;
            let stored_epoch = u64_from_nonnegative_i64("tasks.action_epoch", stored_epoch)?;
            if lifecycle != "closing"
                || Some(stored_epoch) != fence.action_epoch
                || stored_epoch != *action_epoch
            {
                return Err(StoreError::StaleFence);
            }
        }
        Effect::HoldBrowserHost { action_epoch, .. } => {
            let (lifecycle, stored_epoch): (String, i64) = tx
                .query_row(
                    "SELECT lifecycle, action_epoch FROM tasks WHERE task_id = ?1",
                    [task_id.as_bytes().as_slice()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .map_err(|err| match err {
                    rusqlite::Error::QueryReturnedNoRows => StoreError::StaleFence,
                    other => other.into(),
                })?;
            let stored_epoch = u64_from_nonnegative_i64("tasks.action_epoch", stored_epoch)?;
            if lifecycle != "open"
                || Some(stored_epoch) != fence.action_epoch
                || stored_epoch != *action_epoch
            {
                return Err(StoreError::StaleFence);
            }
        }
        Effect::ReleaseResource {
            resource_fence,
            action_epoch,
            ..
        } => {
            let (_lifecycle, epoch): (String, i64) = tx
                .query_row(
                    "SELECT lifecycle, action_epoch FROM tasks WHERE task_id = ?1",
                    [task_id.as_bytes().as_slice()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .map_err(|err| match err {
                    rusqlite::Error::QueryReturnedNoRows => StoreError::StaleFence,
                    other => other.into(),
                })?;
            let epoch = u64_from_nonnegative_i64("tasks.action_epoch", epoch)?;
            if Some(epoch) != fence.action_epoch || epoch != *action_epoch {
                return Err(StoreError::StaleFence);
            }
            let row: Option<(Option<Vec<u8>>, String, String, i64)> = tx
                .query_row(
                    "SELECT task_id, owner_kind, lifecycle, runtime_generation
                     FROM resources WHERE resource_id = ?1",
                    [resource_fence.resource_id.as_bytes().as_slice()],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .optional()?;
            let Some((owned_task, owner_kind, lifecycle, generation)) = row else {
                return Err(StoreError::StaleFence);
            };
            let generation = u64_from_nonnegative_i64("resources.runtime_generation", generation)?;
            let owned_ok = matches!(
                owned_task.as_deref(),
                Some(bytes) if bytes == task_id.as_bytes().as_slice()
            );
            if !owned_ok
                || owner_kind != "task"
                || lifecycle != "releasing"
                || generation != resource_fence.runtime_generation
            {
                return Err(StoreError::StaleFence);
            }
        }
        Effect::DeliverProviderInput {
            agent_session_id,
            provider_kind,
            provider_session_id,
            action_epoch,
            runtime_generation,
            ..
        } => {
            let row: Option<(Vec<u8>, String, Option<String>, String, i64)> = tx
                .query_row(
                    "SELECT task_id, provider_kind, provider_session_id, lifecycle,
                            runtime_generation
                     FROM agent_sessions WHERE agent_session_id = ?1",
                    [agent_session_id.as_bytes().as_slice()],
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
            let Some((owned_task, current_kind, current_session, lifecycle, generation)) = row
            else {
                return Err(StoreError::StaleFence);
            };
            let generation =
                u64_from_nonnegative_i64("agent_sessions.runtime_generation", generation)?;
            let expected_session = current_session
                .map(crate::domain::agent::ProviderSessionId::new)
                .transpose()
                .map_err(|_| StoreError::Corruption)?;
            let owned_ok = owned_task == task_id.as_bytes().to_vec();
            if !owned_ok
                || lifecycle != "open"
                || current_kind != provider_kind.wire_name()
                || expected_session != *provider_session_id
                || generation != *runtime_generation
            {
                return Err(StoreError::StaleFence);
            }
            let (_task_lifecycle, epoch): (String, i64) = tx
                .query_row(
                    "SELECT lifecycle, action_epoch FROM tasks WHERE task_id = ?1",
                    [task_id.as_bytes().as_slice()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .map_err(|err| match err {
                    rusqlite::Error::QueryReturnedNoRows => StoreError::StaleFence,
                    other => other.into(),
                })?;
            let epoch = u64_from_nonnegative_i64("tasks.action_epoch", epoch)?;
            if Some(epoch) != fence.action_epoch || epoch != *action_epoch {
                return Err(StoreError::StaleFence);
            }
        }
    }
    Ok(())
}

pub(crate) fn refuse_archive_with_live_resources(
    tx: &Transaction<'_>,
    task_id: TaskId,
) -> Result<(), StoreError> {
    let mut stmt = tx.prepare("SELECT owner_kind, lifecycle FROM resources WHERE task_id = ?1")?;
    let rows = stmt.query_map([task_id.as_bytes().as_slice()], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (owner_kind, lifecycle) = row?;
        if owner_kind != "task" {
            return Err(StoreError::Corruption);
        }
        match lifecycle.as_str() {
            "released" => {}
            "active" | "releasing" => return Err(StoreError::StaleFence),
            _ => return Err(StoreError::Corruption),
        }
    }
    Ok(())
}

fn apply_new_outcome(
    tx: &Transaction<'_>,
    command_id: CommandId,
    task_id: TaskId,
    outcome: &OperationOutcome,
    effect: &Effect,
    outbox: &OutboxRow,
    expected_outbox_state: &str,
) -> Result<OperationState, StoreError> {
    let (resource_id, runtime_generation) = ResourceFence::into_parts(outcome.resource_fence);
    let outcome_event_id = EventId::new();

    // Dispatch outcomes against a row that already started must not predate start.
    if matches!(outcome.source, OutcomeSource::Dispatch) && outbox.attempts > 0 {
        let Some(started) = outbox.dispatch_started_at_ms else {
            return Err(StoreError::Corruption);
        };
        if started > outcome.occurred_at_ms {
            return Err(StoreError::Corruption);
        }
    }

    let state = match &outcome.kind {
        OperationOutcomeKind::Settled { result_event_ids } => {
            if matches!(effect, Effect::BeginTaskTeardown { .. }) {
                refuse_archive_with_live_resources(tx, task_id)?;
            }
            let result_id = require_single_unused_result_id(tx, result_event_ids)?;
            let (result_revision, result_payload) = match effect {
                Effect::BeginTaskTeardown { .. } => {
                    (Some(next_task_revision(tx, task_id)?), Event::TaskArchived)
                }
                Effect::ReleaseResource { resource_fence, .. } => (
                    Some(next_task_revision(tx, task_id)?),
                    Event::ResourceReleased {
                        resource_id: resource_fence.resource_id,
                        runtime_generation: resource_fence.runtime_generation,
                    },
                ),
                Effect::DeliverProviderInput {
                    operation_id,
                    client_id,
                    agent_session_id,
                    provider_kind,
                    provider_session_id,
                    runtime_generation,
                    action_epoch,
                    turn_id,
                    question_id,
                    approval_id,
                    ..
                } => (
                    None,
                    Event::ProviderInputDelivered {
                        command_id,
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
                    },
                ),
                Effect::HoldBrowserHost { .. } => {
                    return Err(StoreError::InvalidDispatchTransition);
                }
            };
            append_and_project(
                tx,
                result_id,
                Some(task_id),
                result_revision,
                outcome.occurred_at_ms,
                result_payload,
            )?;
            let settled = OperationSettledFact::with_source(
                command_id,
                outcome.operation_id,
                outcome.occurred_at_ms,
                vec![result_id],
                outcome.action_epoch,
                resource_id,
                runtime_generation,
                outcome.source.clone(),
            )
            .map_err(|_| StoreError::ConstraintViolation)?;
            append_and_project(
                tx,
                outcome_event_id,
                Some(task_id),
                None,
                outcome.occurred_at_ms,
                Event::OperationSettled(settled),
            )?;
            transition_outbox(tx, outbox, expected_outbox_state, "settled", None)?;
            OperationState::Settled {
                settled_at_ms: outcome.occurred_at_ms,
                result_event_ids: vec![result_id],
            }
        }
        OperationOutcomeKind::Failed { code } => {
            let failed = OperationFailedFact::with_source(
                command_id,
                outcome.operation_id,
                outcome.occurred_at_ms,
                *code,
                outcome.action_epoch,
                resource_id,
                runtime_generation,
                outcome.source.clone(),
            )
            .map_err(|_| StoreError::ConstraintViolation)?;
            append_and_project(
                tx,
                outcome_event_id,
                Some(task_id),
                None,
                outcome.occurred_at_ms,
                Event::OperationFailed(failed),
            )?;
            transition_outbox(
                tx,
                outbox,
                expected_outbox_state,
                "failed",
                Some("side_effect_failed"),
            )?;
            OperationState::Failed {
                settled_at_ms: outcome.occurred_at_ms,
                code: *code,
            }
        }
        OperationOutcomeKind::Cancelled { reason } => {
            let cancelled = OperationCancelledFact::new(
                command_id,
                outcome.operation_id,
                outcome.occurred_at_ms,
                *reason,
                outcome.action_epoch,
                resource_id,
                runtime_generation,
            )
            .map_err(|_| StoreError::ConstraintViolation)?;
            append_and_project(
                tx,
                outcome_event_id,
                Some(task_id),
                None,
                outcome.occurred_at_ms,
                Event::OperationCancelled(cancelled),
            )?;
            transition_outbox(
                tx,
                outbox,
                expected_outbox_state,
                "cancelled",
                Some("superseded"),
            )?;
            OperationState::Cancelled {
                settled_at_ms: outcome.occurred_at_ms,
                reason: *reason,
            }
        }
        OperationOutcomeKind::Uncertain { code } => {
            let uncertain = OperationUncertainFact::new(
                command_id,
                outcome.operation_id,
                outcome.occurred_at_ms,
                *code,
                outcome.action_epoch,
                resource_id,
                runtime_generation,
            )
            .map_err(|_| StoreError::ConstraintViolation)?;
            append_and_project(
                tx,
                outcome_event_id,
                Some(task_id),
                None,
                outcome.occurred_at_ms,
                Event::OperationUncertain(uncertain),
            )?;
            transition_outbox(
                tx,
                outbox,
                expected_outbox_state,
                "uncertain",
                Some("ambiguous_dispatch"),
            )?;
            OperationState::Uncertain {
                observed_at_ms: outcome.occurred_at_ms,
                code: *code,
            }
        }
    };
    Ok(state)
}

fn require_single_unused_result_id(
    tx: &Transaction<'_>,
    result_event_ids: &[EventId],
) -> Result<EventId, StoreError> {
    if result_event_ids.len() != 1 {
        return Err(StoreError::ConflictingOutcome);
    }
    let result_id = result_event_ids[0];
    // Reject duplicates inside the caller-provided list (len==1 already unique).
    let exists: Option<i64> = tx
        .query_row(
            "SELECT 1 FROM events WHERE event_id = ?1",
            [result_id.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .optional()?;
    if exists.is_some() {
        return Err(StoreError::ConflictingOutcome);
    }
    Ok(result_id)
}

fn next_task_revision(tx: &Transaction<'_>, task_id: TaskId) -> Result<u64, StoreError> {
    let (_lifecycle, _epoch, durable_revision) = validate_task_history_and_projection(tx, task_id)?;
    durable_revision
        .checked_add(1)
        .ok_or(StoreError::IntegerOutOfRange {
            field: "events.task_revision",
            value: u64::MAX,
        })
}

fn transition_outbox(
    tx: &Transaction<'_>,
    outbox: &OutboxRow,
    expected_state: &str,
    next_state: &str,
    last_error_class: Option<&str>,
) -> Result<(), StoreError> {
    let changed = tx.execute(
        "UPDATE outbox
         SET state = ?1, leased_until_ms = NULL, last_error_class = ?2,
             reconciliation_receipt = NULL
         WHERE outbox_id = ?3 AND state = ?4 AND lease_generation = ?5
           AND attempts = ?6",
        rusqlite::params![
            next_state,
            last_error_class,
            outbox.outbox_id.as_bytes().as_slice(),
            expected_state,
            outbox.lease_generation,
            outbox.attempts,
        ],
    )?;
    if changed != 1 {
        return Err(StoreError::InvalidDispatchTransition);
    }
    Ok(())
}

/// Atomically supersede the exact untouched BeginTaskTeardown lineage before Reopen
/// commits TaskReopened. Missing/duplicate/wrong/started close ownership fails closed.
fn cancel_untouched_begin_close_for_reopen(
    tx: &Transaction<'_>,
    task_id: TaskId,
    action_epoch: u64,
    settled_at_ms: i64,
) -> Result<(), StoreError> {
    let epoch_i64 = u64_to_sqlite_i64("tasks.action_epoch", action_epoch)?;
    let operation_ids = {
        let mut stmt = tx.prepare(
            "SELECT operation_id FROM operations
             WHERE task_id = ?1 AND action_epoch = ?2 AND state = 'accepted'
             ORDER BY operation_id",
        )?;
        let rows = stmt.query_map(
            rusqlite::params![task_id.as_bytes().as_slice(), epoch_i64],
            |row| row.get::<_, Vec<u8>>(0),
        )?;
        let mut ids = Vec::new();
        for row in rows {
            ids.push(id16::<OperationId>("operations.operation_id", &row?)?);
        }
        ids
    };
    if operation_ids.len() != 1 {
        return Err(StoreError::Corruption);
    }
    let operation_id = operation_ids[0];

    let command_id = load_operation_command_id(tx, operation_id)?;
    let receipt_row = load_receipt_correlation(tx, command_id)?;
    let CommandReceipt::Accepted {
        command_id: receipt_command_id,
        operation_id: receipt_operation_id,
        event_ids,
        task_revision,
        prompt_mutation: _,
    } = &receipt_row.receipt
    else {
        return Err(StoreError::Corruption);
    };
    if *receipt_command_id != command_id
        || *receipt_operation_id != operation_id
        || receipt_row.task_id != Some(task_id)
    {
        return Err(StoreError::Corruption);
    }
    let committed_sequence = receipt_row
        .committed_sequence
        .map(|sequence| u64_to_sqlite_i64("command_receipts.committed_sequence", sequence))
        .transpose()?;
    validate_accepted_receipt_correlation(
        tx,
        command_id,
        operation_id,
        event_ids,
        *task_revision,
        receipt_row.task_id,
        committed_sequence,
        receipt_row.created_at_ms,
    )?;

    let operation =
        load_operation_projection_by_id(tx, operation_id)?.ok_or(StoreError::Corruption)?;
    require_accepted_dispatch_operation(&operation)?;
    if operation.task_id != Some(task_id) {
        return Err(StoreError::Corruption);
    }
    let fence = operation_fence_from_projection(&operation)?;
    if fence.action_epoch != Some(action_epoch)
        || fence.resource_id.is_some()
        || fence.runtime_generation.is_some()
    {
        return Err(StoreError::Corruption);
    }
    if settled_at_ms < operation.accepted_at_ms {
        return Err(StoreError::Corruption);
    }

    let outbox_rows = load_outbox_rows(tx, operation_id)?;
    if outbox_rows.len() != 1 {
        return Err(StoreError::Corruption);
    }
    let outbox = &outbox_rows[0];
    let document = decode_full_outbox_payload(outbox)?;
    validate_effect_matches_fence(&document.effect, task_id, fence)?;
    let Effect::BeginTaskTeardown {
        task_id: effect_task,
        action_epoch: effect_epoch,
    } = &document.effect
    else {
        return Err(StoreError::Corruption);
    };
    if *effect_task != task_id || *effect_epoch != action_epoch {
        return Err(StoreError::Corruption);
    }
    require_current_effect_ownership(tx, task_id, &document.effect, fence)?;
    validate_nonterminal_outbox_dispatch_metadata(
        outbox,
        operation.accepted_at_ms,
        document.replay_policy,
    )?;
    match outbox.state.as_str() {
        "pending" | "claimed" => {}
        _ => return Err(StoreError::Corruption),
    }
    if outbox.attempts != 0
        || outbox.dispatch_started_at_ms.is_some()
        || outbox.reconciliation_receipt.is_some()
    {
        return Err(StoreError::Corruption);
    }

    let cancelled = OperationCancelledFact::new(
        command_id,
        operation_id,
        settled_at_ms,
        CancellationReason::Superseded,
        fence.action_epoch,
        fence.resource_id,
        fence.runtime_generation,
    )
    .map_err(|_| StoreError::ConstraintViolation)?;
    append_and_project(
        tx,
        EventId::new(),
        Some(task_id),
        None,
        settled_at_ms,
        Event::OperationCancelled(cancelled),
    )?;
    transition_outbox(tx, outbox, &outbox.state, "cancelled", Some("superseded"))?;
    Ok(())
}

fn execute_in_tx(
    tx: &Transaction<'_>,
    envelope: CommandEnvelope,
    scope: ReceiptScope,
) -> Result<CommandReceipt, StoreError> {
    if let Some(existing) = lookup_receipt_for_scope(tx, &envelope, scope)? {
        return Ok(existing);
    }

    if host_admission_is_closing(tx)? {
        let accepted_at_ms = now_ms()?;
        let effective_task_id = effective_task_scope(&envelope);
        let snapshot = match effective_task_id {
            Some(task_id) => load_task_snapshot(tx, task_id)?,
            None => None,
        };
        let current_revision = snapshot.as_ref().map(|snap| snap.task.revision);
        return persist_rejection(
            tx,
            &envelope,
            effective_task_id,
            RejectionCode::Closing,
            current_revision,
            accepted_at_ms,
            scope,
        );
    }

    if matches!(envelope.command, Command::ConfirmHostQuit(_)) {
        return persist_confirm_host_quit(tx, envelope, scope);
    }

    let accepted_at_ms = now_ms()?;
    let effective_task_id = effective_task_scope(&envelope);
    let snapshot = match effective_task_id {
        Some(task_id) => load_task_snapshot(tx, task_id)?,
        None => None,
    };
    let current_revision = snapshot.as_ref().map(|snap| snap.task.revision);

    if let Command::BindProviderSession {
        agent_session_id,
        resource_id,
        provider_session_id,
        ..
    } = &envelope.command
    {
        let existing: Option<(Option<String>, Option<Vec<u8>>)> = tx
            .query_row(
                "SELECT provider_session_id, provider_resource_id
                 FROM agent_sessions
                 WHERE agent_session_id = ?1",
                [agent_session_id.as_bytes().as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((existing_session, existing_resource)) = existing {
            match (existing_session, existing_resource) {
                (None, None) => {}
                (Some(existing_session), Some(existing_resource))
                    if existing_session == provider_session_id.as_str()
                        && existing_resource.as_slice() == resource_id.as_bytes() => {}
                (Some(_), Some(_)) => {
                    return persist_rejection(
                        tx,
                        &envelope,
                        effective_task_id,
                        RejectionCode::OwnershipConflict,
                        current_revision,
                        accepted_at_ms,
                        scope,
                    );
                }
                _ => return Err(StoreError::Corruption),
            }
        }
    }

    match decide(snapshot.as_ref(), &envelope) {
        Err(code) => {
            let resolution = provider_resolution_for_rejection(snapshot.as_ref(), &envelope);
            // A concurrent answer/approval can advance the task revision or close the
            // request before this command reaches the decision gate. Preserve the typed
            // first-winner contract instead of leaking a generic rejection to the loser.
            let code = if resolution.is_some() {
                RejectionCode::AlreadyResolved
            } else {
                code
            };
            persist_rejection_with_resolution(
                tx,
                &envelope,
                effective_task_id,
                code,
                current_revision,
                accepted_at_ms,
                resolution,
                scope,
            )
        }
        Ok(decision) => {
            // Empty authoritative decisions for already Closing/Releasing stay unsupported
            // rather than inventing a duplicate in-flight operation/effect.
            if decision.is_empty() && command_is_effectful(&envelope.command) {
                return persist_rejection(
                    tx,
                    &envelope,
                    effective_task_id,
                    RejectionCode::UnsupportedCapability,
                    current_revision,
                    accepted_at_ms,
                    scope,
                );
            }
            let Some(task_id) = effective_task_id else {
                return Err(StoreError::Projection(
                    "accepted commands require an effective task scope".into(),
                ));
            };
            let planned = plan_effects(snapshot.as_ref(), task_id, &decision)?;
            if planned.is_empty() {
                if matches!(envelope.command, Command::ReopenTask) {
                    if let Some(snap) = snapshot.as_ref() {
                        if snap.task.lifecycle == TaskLifecycle::Closing {
                            cancel_untouched_begin_close_for_reopen(
                                tx,
                                task_id,
                                snap.task.action_epoch,
                                accepted_at_ms,
                            )?;
                        }
                    }
                }
                persist_pure_acceptance(
                    tx,
                    &envelope,
                    effective_task_id,
                    snapshot.as_ref(),
                    decision,
                    accepted_at_ms,
                    scope,
                )
            } else {
                persist_side_effect_acceptance(
                    tx,
                    &envelope,
                    task_id,
                    snapshot.as_ref(),
                    decision,
                    planned,
                    accepted_at_ms,
                    scope,
                )
            }
        }
    }
}

fn lookup_receipt(
    tx: &Connection,
    command_id: CommandId,
) -> Result<Option<CommandReceipt>, StoreError> {
    Ok(lookup_receipt_with_digest(tx, command_id)?.map(|(receipt, _)| receipt))
}

fn lookup_receipt_with_digest(
    tx: &Connection,
    command_id: CommandId,
) -> Result<Option<(CommandReceipt, Option<[u8; 32]>)>, StoreError> {
    let row: Option<(Vec<u8>, Option<Vec<u8>>, Option<i64>, i64, Option<Vec<u8>>)> = tx
        .query_row(
            "SELECT receipt, task_id, committed_sequence, created_at_ms, payload_digest
             FROM command_receipts WHERE command_id = ?1",
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
        )
        .optional()?;
    let Some((payload, row_task_id, committed_sequence, created_at_ms, digest_bytes)) = row else {
        return Ok(None);
    };
    let receipt = decode_receipt_document(&payload)?;
    let receipt_command_id = match &receipt {
        CommandReceipt::Accepted { command_id, .. }
        | CommandReceipt::Rejected { command_id, .. } => *command_id,
    };
    if receipt_command_id != command_id {
        return Err(StoreError::CodecMismatch {
            detail: "stored receipt command_id disagrees with lookup key".into(),
        });
    }

    match &receipt {
        CommandReceipt::Accepted {
            operation_id,
            event_ids,
            task_revision,
            ..
        } => {
            let receipt_task_id =
                parse_optional_task_scope("command_receipts.task_id", row_task_id)?;
            validate_accepted_receipt_correlation(
                tx,
                command_id,
                *operation_id,
                event_ids,
                *task_revision,
                receipt_task_id,
                committed_sequence,
                created_at_ms,
            )?;
        }
        CommandReceipt::Rejected { .. } => {
            // Rejected receipts still carry a typed durable task scope when present.
            let _receipt_task_id =
                parse_optional_task_scope("command_receipts.task_id", row_task_id)?;
            validate_rejected_receipt_correlation(tx, command_id, committed_sequence)?;
        }
    }
    let digest = match digest_bytes {
        None => None,
        Some(bytes) => {
            let arr: [u8; 32] = bytes.try_into().map_err(|_| StoreError::CodecMismatch {
                detail: "command_receipts.payload_digest must be 32 bytes".into(),
            })?;
            Some(arr)
        }
    };
    Ok(Some((receipt, digest)))
}

fn command_fingerprint(envelope: &CommandEnvelope) -> Result<[u8; 32], StoreError> {
    // The command ID is only idempotent for one exact envelope identity. In
    // particular, a caller cannot reuse an ID with a different task revision
    // or task scope and receive the first caller's receipt.
    let encoded = rmp_serde::to_vec(&(
        envelope.task_id,
        envelope.issued_at_ms,
        envelope.expected_task_revision,
        &envelope.command,
    ))
    .map_err(|_| StoreError::CodecMismatch {
        detail: "command identity encoding failed".into(),
    })?;
    Ok(Sha256::digest(encoded).into())
}

fn lookup_receipt_for_scope(
    tx: &Transaction<'_>,
    envelope: &CommandEnvelope,
    scope: ReceiptScope,
) -> Result<Option<CommandReceipt>, StoreError> {
    let row: Option<(
        Vec<u8>,
        Option<Vec<u8>>,
        Option<Vec<u8>>,
        Option<Vec<u8>>,
        Option<i64>,
        Option<i64>,
        Option<Vec<u8>>,
        Option<Vec<u8>>,
    )> = tx
        .query_row(
            "SELECT client_id, task_id, connection_id, request_id,
                    action_epoch, runtime_generation, command_fingerprint, payload_digest
             FROM command_receipts WHERE command_id = ?1",
            [envelope.command_id.as_bytes().as_slice()],
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
        client_bytes,
        task_bytes,
        connection_bytes,
        request_bytes,
        epoch,
        generation,
        fingerprint,
        payload_digest,
    )) = row
    else {
        return Ok(None);
    };
    let client_id = parse_client_receipt_id("command_receipts.client_id", &client_bytes)?;
    let task_id = parse_optional_task_scope("command_receipts.task_id", task_bytes)?;
    let connection_id = connection_bytes
        .as_deref()
        .map(|bytes| id16_uuid("command_receipts.connection_id", bytes))
        .transpose()?;
    let request_id = request_bytes
        .as_deref()
        .map(|bytes| parse_request_receipt_id("command_receipts.request_id", bytes))
        .transpose()?;
    let stored_scope = ReceiptScope {
        connection_id,
        request_id,
        action_epoch: epoch
            .map(|value| u64_from_nonnegative_i64("command_receipts.action_epoch", value))
            .transpose()?,
        runtime_generation: generation
            .map(|value| u64_from_nonnegative_i64("command_receipts.runtime_generation", value))
            .transpose()?,
    };
    let expected_fingerprint = command_fingerprint(envelope)?;
    let expected_payload_digest = crate::domain::command::command_payload_digest(envelope)
        .map_err(|detail| StoreError::CodecMismatch { detail })?;
    // V7 cannot reconstruct the original envelope identity for a pre-V7 row.
    // Treat that row as a conflict instead of returning a receipt whose
    // command/task scope cannot be proven exact.
    let fingerprint_matches = fingerprint
        .as_deref()
        .is_some_and(|bytes| bytes == expected_fingerprint.as_slice());
    let stored_payload_digest: Option<[u8; 32]> = payload_digest
        .as_deref()
        .map(|bytes| {
            bytes.try_into().map_err(|_| StoreError::CodecMismatch {
                detail: "command_receipts.payload_digest must be 32 bytes".into(),
            })
        })
        .transpose()?;
    let payload_matches = stored_payload_digest
        .as_ref()
        .is_some_and(|digest| digest == &expected_payload_digest);
    let expected_task_id = effective_task_scope(envelope);
    if client_id != envelope.client_id {
        return Err(StoreError::CommandIdConflict);
    }
    let typed_idempotency_conflict = stored_payload_digest.is_some()
        && !payload_matches
        && matches!(
            &envelope.command,
            Command::ConfirmHostQuit(_) | Command::SubmitProviderInput(_)
        );
    // These two duplex commands expose a typed, caller-owned collision result.
    // A same-client command-id reuse must remain writable even when the older
    // command occupied a different task scope; it must never be mistaken for
    // an accepted quit/input effect. Cross-client collisions still fail closed.
    if task_id != expected_task_id {
        if typed_idempotency_conflict {
            let current_revision = expected_task_id
                .map(|task_id| load_task_snapshot(tx, task_id))
                .transpose()?
                .flatten()
                .map(|snapshot| snapshot.task.revision);
            return Ok(Some(CommandReceipt::Rejected {
                command_id: envelope.command_id,
                code: RejectionCode::IdempotencyConflict,
                current_revision,
                resolution: None,
            }));
        }
        return Err(StoreError::CommandIdConflict);
    }
    if !fingerprint_matches {
        if typed_idempotency_conflict {
            let current_revision = expected_task_id
                .map(|task_id| load_task_snapshot(tx, task_id))
                .transpose()?
                .flatten()
                .map(|snapshot| snapshot.task.revision);
            return Ok(Some(CommandReceipt::Rejected {
                command_id: envelope.command_id,
                code: RejectionCode::IdempotencyConflict,
                current_revision,
                resolution: None,
            }));
        }
        return Err(StoreError::CommandIdConflict);
    }
    if typed_idempotency_conflict {
        let current_revision = expected_task_id
            .map(|task_id| load_task_snapshot(tx, task_id))
            .transpose()?
            .flatten()
            .map(|snapshot| snapshot.task.revision);
        return Ok(Some(CommandReceipt::Rejected {
            command_id: envelope.command_id,
            code: RejectionCode::IdempotencyConflict,
            current_revision,
            resolution: None,
        }));
    }

    // A receipt first persisted without an output connection is a delivery
    // recovery token, not a cross-session replay token. Claim it exactly once
    // for the new output after rechecking the client/task/command identity.
    let can_claim_unbound = fingerprint.is_some()
        && stored_scope.connection_id.is_none()
        && stored_scope.request_id.is_none()
        && scope.connection_id.is_some()
        && scope.request_id.is_some()
        && (stored_scope.action_epoch.is_none() || stored_scope.action_epoch == scope.action_epoch)
        && (stored_scope.runtime_generation.is_none()
            || stored_scope.runtime_generation == scope.runtime_generation);
    if can_claim_unbound {
        tx.execute(
            "UPDATE command_receipts
             SET connection_id = ?2, request_id = ?3,
                 action_epoch = ?4, runtime_generation = ?5
             WHERE command_id = ?1 AND connection_id IS NULL AND request_id IS NULL",
            rusqlite::params![
                envelope.command_id.as_bytes().as_slice(),
                scope.connection_id.map(|id| id.as_bytes().to_vec()),
                scope.request_id.map(|id| id.as_bytes().to_vec()),
                scope
                    .action_epoch
                    .map(|epoch| u64_to_sqlite_i64("command_receipts.action_epoch", epoch))
                    .transpose()?,
                scope
                    .runtime_generation
                    .map(|generation| {
                        u64_to_sqlite_i64("command_receipts.runtime_generation", generation)
                    })
                    .transpose()?,
            ],
        )?;
        return lookup_receipt(tx, envelope.command_id);
    }

    // Every scope dimension must be present and equal before returning a
    // receipt. Treating a missing incoming dimension as a wildcard would let
    // an otherwise valid partial host context replay another connection's
    // receipt. The only exception is the explicit one-shot unbound claim
    // above, which atomically fills both connection and request identity.
    if stored_scope != scope {
        return Err(StoreError::CommandIdConflict);
    }
    lookup_receipt(tx, envelope.command_id)
}

// `Uuid` itself is not one of the domain ID wrappers. Keep parsing the 16-byte
// session/connection value in one checked helper rather than accepting an
// arbitrary byte vector from SQLite.
fn id16_uuid(field: &'static str, bytes: &[u8]) -> Result<Uuid, StoreError> {
    if bytes.len() != 16 {
        return Err(StoreError::CodecMismatch {
            detail: format!("{field} must be 16 bytes"),
        });
    }
    let mut raw = [0u8; 16];
    raw.copy_from_slice(bytes);
    Ok(Uuid::from_bytes(raw))
}

fn parse_client_receipt_id(field: &'static str, bytes: &[u8]) -> Result<ClientId, StoreError> {
    let raw: [u8; 16] = bytes.try_into().map_err(|_| StoreError::CodecMismatch {
        detail: format!("{field} must be 16 bytes"),
    })?;
    ClientId::from_bytes(raw).map_err(|_| StoreError::Corruption)
}

fn parse_request_receipt_id(field: &'static str, bytes: &[u8]) -> Result<RequestId, StoreError> {
    let raw: [u8; 16] = bytes.try_into().map_err(|_| StoreError::CodecMismatch {
        detail: format!("{field} must be 16 bytes"),
    })?;
    RequestId::from_bytes(raw).map_err(|_| StoreError::Corruption)
}

/// Validate durable e2 dispatch metadata after an explicit projection rebuild.
/// Strict receipt-backed operations are re-correlated end to end so a missing,
/// extra, or terminally corrupted outbox row cannot survive repair. The final
/// outbox scan independently rejects orphan rows that have no receipt root.
/// Uncertain rows are intentionally still decoded through the receipt-backed
/// terminal validator: their typed provider effect is the durable G1 identity,
/// so projection repair must reject payload tampering rather than silently
/// rebuilding an ambiguity with a different provider session.
pub(crate) fn validate_all_rebuilt_outbox_metadata(tx: &Transaction<'_>) -> Result<(), StoreError> {
    let receipt_backed_commands = {
        let mut stmt = tx.prepare("SELECT command_id FROM operations ORDER BY operation_id")?;
        let rows = stmt.query_map([], |row| row.get::<_, Vec<u8>>(0))?;
        let mut command_ids = Vec::new();
        for row in rows {
            command_ids.push(id16::<CommandId>("operations.command_id", &row?)?);
        }
        command_ids
    };
    for command_id in receipt_backed_commands {
        lookup_receipt(tx, command_id)?.ok_or(StoreError::Corruption)?;
    }

    let operation_ids = {
        let mut stmt =
            tx.prepare("SELECT DISTINCT operation_id FROM outbox ORDER BY operation_id")?;
        let rows = stmt.query_map([], |row| row.get::<_, Vec<u8>>(0))?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    for bytes in operation_ids {
        let operation_id = id16::<OperationId>("outbox.operation_id", &bytes)?;
        let operation =
            load_operation_projection_by_id(tx, operation_id)?.ok_or(StoreError::Corruption)?;
        let rows = load_outbox_rows(tx, operation_id)?;
        for row in rows {
            match row.state.as_str() {
                "pending" | "claimed" | "dispatching" | "reconcile_required" | "reconciling" => {
                    if operation.state != "accepted" {
                        return Err(StoreError::Corruption);
                    }
                    let decoded = decode_full_outbox_payload(&row)?;
                    validate_nonterminal_outbox_dispatch_metadata(
                        &row,
                        operation.accepted_at_ms,
                        decoded.replay_policy,
                    )?;
                    if row.state == "pending" && row.last_error_class.is_some() {
                        return Err(StoreError::Corruption);
                    }
                }
                "settled" | "failed" | "cancelled" | "uncertain" => {
                    if row.leased_until_ms.is_some() || row.reconciliation_receipt.is_some() {
                        return Err(StoreError::Corruption);
                    }
                }
                _ => return Err(StoreError::Corruption),
            }
        }
    }
    Ok(())
}

/// After projection rebuild, Closing host_admission must resolve to the exact global
/// Accepted operation and receipt. Orphan HostCloseBegun (Closing without roots) fails closed.
pub(crate) fn validate_rebuilt_host_admission(tx: &Transaction<'_>) -> Result<(), StoreError> {
    let Some(admission) = load_host_admission_row(tx)? else {
        return Ok(());
    };
    let operation = load_operation_projection_by_id(tx, admission.operation_id)?.ok_or(
        StoreError::Projection(
            "host_admission Closing requires exact global Accepted operation".into(),
        ),
    )?;
    if operation.task_id.is_some()
        || operation.resource_id.is_some()
        || operation.runtime_generation.is_some()
    {
        return Err(StoreError::Corruption);
    }
    let Some(action_epoch_i64) = operation.action_epoch else {
        return Err(StoreError::Corruption);
    };
    let action_epoch = u64_from_nonnegative_i64("operations.action_epoch", action_epoch_i64)?;
    if admission.action_epoch != action_epoch || admission.updated_at_ms != operation.accepted_at_ms
    {
        return Err(StoreError::Corruption);
    }

    match operation.state.as_str() {
        "accepted" => {
            if operation.result.is_some()
                || operation.outcome_code.is_some()
                || operation.outcome_at_ms.is_some()
            {
                return Err(StoreError::Corruption);
            }
        }
        "failed" => {
            if operation.result.is_some()
                || operation.outcome_code.as_deref() != Some("cleanup_failed")
                || operation.outcome_at_ms.is_none()
            {
                return Err(StoreError::Corruption);
            }
            let journal = load_ordered_host_cleanup_journal_outcomes(tx, admission.operation_id)?;
            if !journal
                .iter()
                .any(|(_, outcome)| matches!(outcome, HostCleanupBranchOutcome::Failed { .. }))
            {
                return Err(StoreError::Corruption);
            }
        }
        "settled" => {
            if operation.outcome_code.is_some()
                || operation.result.is_none()
                || operation.outcome_at_ms.is_none()
            {
                return Err(StoreError::Corruption);
            }
            let journal = load_ordered_host_cleanup_journal_outcomes(tx, admission.operation_id)?;
            if journal.len() != HostCleanupBranch::ORDER.len()
                || journal
                    .iter()
                    .any(|(_, outcome)| !matches!(outcome, HostCleanupBranchOutcome::Succeeded))
            {
                return Err(StoreError::Corruption);
            }
        }
        _ => return Err(StoreError::Corruption),
    }

    let command_bytes: Vec<u8> = tx.query_row(
        "SELECT command_id FROM operations WHERE operation_id = ?1",
        [admission.operation_id.as_bytes().as_slice()],
        |row| row.get(0),
    )?;
    let command_id = id16::<CommandId>("operations.command_id", &command_bytes)?;
    let receipt = lookup_receipt(tx, command_id)?.ok_or(StoreError::Corruption)?;
    match receipt {
        CommandReceipt::Accepted {
            operation_id,
            task_revision: None,
            event_ids,
            ..
        } if operation_id == admission.operation_id && event_ids.len() == 1 => Ok(()),
        _ => Err(StoreError::Corruption),
    }
}

/// After projection rebuild, each `host_cleanup_branches` row must match exactly one
/// durable `HostCleanupBranchCompleted` fact for the Closing admission lineage.
pub(crate) fn validate_rebuilt_host_cleanup_branches(
    tx: &Transaction<'_>,
) -> Result<(), StoreError> {
    let admission = load_host_admission_row(tx)?;
    let projected = load_host_cleanup_branch_projection_map(tx)?;
    if admission.is_none() && !projected.is_empty() {
        return Err(StoreError::Corruption);
    }
    let facts = load_host_cleanup_branch_event_map(tx, admission.as_ref())?;
    if facts != projected {
        return Err(StoreError::Corruption);
    }
    validate_host_cleanup_branch_prefix_map(&projected)?;
    Ok(())
}

fn load_host_cleanup_branch_projection_map(
    tx: &Connection,
) -> Result<std::collections::BTreeMap<(Vec<u8>, String), (String, i64, i64)>, StoreError> {
    let mut projected = std::collections::BTreeMap::new();
    let mut stmt = tx.prepare(
        "SELECT operation_id, branch, result, remaining_count, completed_at_ms
         FROM host_cleanup_branches",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, Vec<u8>>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, i64>(4)?,
        ))
    })?;
    for row in rows {
        let (operation_id, branch, result, remaining_count, completed_at_ms) = row?;
        if HostCleanupBranch::parse(&branch).is_none() {
            return Err(StoreError::Corruption);
        }
        let key = (operation_id, branch);
        if projected
            .insert(key, (result, remaining_count, completed_at_ms))
            .is_some()
        {
            return Err(StoreError::Corruption);
        }
    }
    Ok(projected)
}

fn load_host_cleanup_branch_event_map(
    tx: &Connection,
    admission: Option<&HostAdmissionRow>,
) -> Result<std::collections::BTreeMap<(Vec<u8>, String), (String, i64, i64)>, StoreError> {
    let mut facts = std::collections::BTreeMap::new();
    let mut event_stmt = tx.prepare(
        "SELECT task_id, task_revision, schema_version, payload, occurred_at_ms
         FROM events
         WHERE event_type = 'host.cleanup_branch_completed'
         ORDER BY sequence ASC",
    )?;
    let event_rows = event_stmt.query_map([], |row| {
        Ok((
            row.get::<_, Option<Vec<u8>>>(0)?,
            row.get::<_, Option<i64>>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, Vec<u8>>(3)?,
            row.get::<_, i64>(4)?,
        ))
    })?;
    for row in event_rows {
        let (task_id, task_revision, schema_version, payload, occurred_at_ms) = row?;
        if task_id.is_some() || task_revision.is_some() {
            return Err(StoreError::Corruption);
        }
        let decoded = crate::kernel::store::decode_stored_event(
            "host.cleanup_branch_completed",
            schema_version,
            &payload,
        )?;
        let Event::HostCleanupBranchCompleted {
            operation_id,
            action_epoch,
            branch,
            outcome,
        } = decoded
        else {
            return Err(StoreError::Corruption);
        };
        let Some(admission) = admission else {
            return Err(StoreError::Corruption);
        };
        if operation_id != admission.operation_id || action_epoch != admission.action_epoch {
            return Err(StoreError::Corruption);
        }
        let key = (
            operation_id.as_bytes().to_vec(),
            branch.as_str().to_string(),
        );
        let value = (
            outcome.result_str().to_string(),
            i64::try_from(outcome.remaining_count()).map_err(|_| StoreError::Corruption)?,
            occurred_at_ms,
        );
        if facts.insert(key, value).is_some() {
            return Err(StoreError::Corruption);
        }
    }
    validate_host_cleanup_branch_prefix_map(&facts)?;
    Ok(facts)
}

fn validate_host_cleanup_branch_prefix_map(
    rows: &std::collections::BTreeMap<(Vec<u8>, String), (String, i64, i64)>,
) -> Result<(), StoreError> {
    let mut by_operation: std::collections::BTreeMap<Vec<u8>, Vec<HostCleanupBranch>> =
        std::collections::BTreeMap::new();
    for (operation_id, branch_name) in rows.keys() {
        let branch = HostCleanupBranch::parse(branch_name).ok_or(StoreError::Corruption)?;
        by_operation
            .entry(operation_id.clone())
            .or_default()
            .push(branch);
    }
    for branches in by_operation.values_mut() {
        branches.sort_by_key(|branch| {
            HostCleanupBranch::ORDER
                .iter()
                .position(|ordered| ordered == branch)
                .unwrap_or(usize::MAX)
        });
        for (idx, branch) in branches.iter().enumerate() {
            if HostCleanupBranch::ORDER.get(idx) != Some(branch) {
                return Err(StoreError::Corruption);
            }
        }
    }
    Ok(())
}

/// Ensure current cleanup projection rows are an exact event-backed ORDER prefix.
fn validate_current_host_cleanup_journal(
    tx: &Connection,
    admission: &HostAdmissionRow,
) -> Result<(), StoreError> {
    let projected = load_host_cleanup_branch_projection_map(tx)?;
    let facts = load_host_cleanup_branch_event_map(tx, Some(admission))?;
    if projected != facts {
        return Err(StoreError::Corruption);
    }
    let mut present = Vec::new();
    for branch in HostCleanupBranch::ORDER {
        let key = (
            admission.operation_id.as_bytes().to_vec(),
            branch.as_str().to_string(),
        );
        if projected.contains_key(&key) {
            present.push(branch);
        }
    }
    for (idx, branch) in present.iter().enumerate() {
        if HostCleanupBranch::ORDER.get(idx) != Some(branch) {
            return Err(StoreError::Corruption);
        }
    }
    if present.len() != projected.len() {
        // Foreign operation_id rows under Closing admission are corruption.
        return Err(StoreError::Corruption);
    }
    Ok(())
}

fn validate_accepted_receipt_correlation(
    tx: &Connection,
    command_id: CommandId,
    expected_operation_id: OperationId,
    event_ids: &[EventId],
    receipt_task_revision: Option<u64>,
    receipt_task_id: Option<TaskId>,
    committed_sequence: Option<i64>,
    receipt_created_at_ms: i64,
) -> Result<(), StoreError> {
    let committed_sequence = committed_sequence.ok_or(StoreError::Corruption)?;
    if committed_sequence < 0 {
        return Err(StoreError::Corruption);
    }
    let committed_sequence =
        u64_from_nonnegative_i64("command_receipts.committed_sequence", committed_sequence)?;

    if event_ids.is_empty() {
        return Err(StoreError::Corruption);
    }

    let operation = load_operation_projection(tx, command_id)?;
    if operation.operation_id != expected_operation_id {
        return Err(StoreError::Corruption);
    }
    if receipt_created_at_ms != operation.accepted_at_ms {
        return Err(StoreError::Corruption);
    }

    if receipt_task_id.is_none() && receipt_task_revision.is_none() {
        return validate_host_admission_accepted_receipt(
            tx,
            command_id,
            expected_operation_id,
            event_ids,
            committed_sequence,
            &operation,
        );
    }

    let Some(scope) = receipt_task_id else {
        return Err(StoreError::Corruption);
    };
    let Some(receipt_final_revision) = receipt_task_revision else {
        return Err(StoreError::Corruption);
    };

    if operation.task_id != Some(scope) {
        return Err(StoreError::Corruption);
    }

    let outbox_rows = load_outbox_rows(tx, expected_operation_id)?;
    if !outbox_rows.is_empty() {
        return validate_side_effect_accepted_receipt(
            tx,
            command_id,
            expected_operation_id,
            event_ids,
            receipt_final_revision,
            scope,
            committed_sequence,
            &operation,
            &outbox_rows,
        );
    }
    // Missing outbox must not fall through into the pure validator for accepted ops.
    if operation.state == "accepted" {
        return Err(StoreError::Corruption);
    }
    validate_pure_accepted_receipt(
        tx,
        command_id,
        expected_operation_id,
        event_ids,
        receipt_final_revision,
        scope,
        committed_sequence,
        &operation,
    )
}

fn validate_host_admission_accepted_receipt(
    tx: &Connection,
    command_id: CommandId,
    expected_operation_id: OperationId,
    event_ids: &[EventId],
    committed_sequence: u64,
    operation: &OperationProjectionRow,
) -> Result<(), StoreError> {
    if event_ids.len() != 1 {
        return Err(StoreError::Corruption);
    }
    if operation.task_id.is_some()
        || operation.resource_id.is_some()
        || operation.runtime_generation.is_some()
    {
        return Err(StoreError::Corruption);
    }
    let Some(action_epoch) = operation.action_epoch else {
        return Err(StoreError::Corruption);
    };
    let action_epoch = u64_from_nonnegative_i64("operations.action_epoch", action_epoch)?;

    let outbox_rows = load_outbox_rows(tx, expected_operation_id)?;
    if !outbox_rows.is_empty() {
        return Err(StoreError::Corruption);
    }

    let accepted_sequence = committed_sequence;
    let decision_sequence = accepted_sequence
        .checked_sub(1)
        .ok_or(StoreError::Corruption)?;

    let decision_row = load_event_row_at_sequence(tx, decision_sequence)?;
    if decision_row.task_id.is_some()
        || decision_row.task_revision.is_some()
        || decision_row.occurred_at_ms != operation.accepted_at_ms
        || decision_row.event_id != event_ids[0]
    {
        return Err(StoreError::Corruption);
    }
    let decision_event = crate::kernel::store::decode_stored_event(
        &decision_row.event_type,
        decision_row.schema_version,
        &decision_row.payload,
    )?;
    let Event::HostCloseBegun {
        operation_id,
        action_epoch: begun_epoch,
        inspection_id,
    } = decision_event
    else {
        return Err(StoreError::Corruption);
    };
    if operation_id != expected_operation_id || begun_epoch != action_epoch {
        return Err(StoreError::Corruption);
    }

    let accepted_row = load_event_row_at_sequence(tx, accepted_sequence)?;
    validate_accepted_fact_row(
        &accepted_row,
        command_id,
        expected_operation_id,
        None,
        operation.accepted_at_ms,
        OperationFence {
            action_epoch: Some(action_epoch),
            resource_id: None,
            runtime_generation: None,
        },
    )?;
    ensure_unique_host_close_begun_fact(
        tx,
        expected_operation_id,
        action_epoch,
        inspection_id,
        decision_sequence,
        &decision_row,
        operation.accepted_at_ms,
    )?;
    ensure_unique_operation_accepted_fact(
        tx,
        command_id,
        expected_operation_id,
        None,
        operation.accepted_at_ms,
        accepted_sequence,
        &accepted_row,
        OperationFence {
            action_epoch: Some(action_epoch),
            resource_id: None,
            runtime_generation: None,
        },
    )?;

    let admission = load_host_admission_row(tx)?.ok_or(StoreError::Corruption)?;
    if admission.operation_id != expected_operation_id
        || admission.action_epoch != action_epoch
        || admission.inspection_id != inspection_id
        || admission.updated_at_ms != operation.accepted_at_ms
    {
        return Err(StoreError::Corruption);
    }

    match operation.state.as_str() {
        "accepted" => {
            if operation.result.is_some()
                || operation.outcome_code.is_some()
                || operation.outcome_at_ms.is_some()
            {
                return Err(StoreError::Corruption);
            }
            // Event-only forged host terminals must fail closed while projection
            // still reports Accepted.
            require_host_operation_terminal_match_count(tx, command_id, expected_operation_id, 0)?;
            Ok(())
        }
        "failed" => {
            if operation.result.is_some()
                || operation.outcome_code.as_deref() != Some("cleanup_failed")
            {
                return Err(StoreError::Corruption);
            }
            let settled_at_ms = operation.outcome_at_ms.ok_or(StoreError::Corruption)?;
            validate_current_host_cleanup_journal(tx, &admission)?;
            let journal = load_ordered_host_cleanup_journal_outcomes(tx, expected_operation_id)?;
            let journal_max_completed_at_ms =
                host_cleanup_journal_max_completed_at(tx, expected_operation_id)?;
            let terminal =
                require_exact_host_cleanup_failed_terminal(tx, command_id, expected_operation_id)?;
            if terminal.occurred_at_ms != settled_at_ms
                || terminal.fact.settled_at_ms != settled_at_ms
                || terminal.fact.code != OperationErrorCode::CleanupFailed
                || !terminal.fact.source.is_dispatch()
                || terminal.fact.action_epoch != Some(action_epoch)
                || terminal.fact.command_id != command_id
                || terminal.fact.operation_id != expected_operation_id
                || terminal.fact.resource_id.is_some()
                || terminal.fact.runtime_generation.is_some()
                || terminal.task_id.is_some()
                || terminal.task_revision.is_some()
            {
                return Err(StoreError::Corruption);
            }
            let prior = load_global_event_before(tx, terminal.sequence)?;
            crate::kernel::lineage::validate_host_admission_cleanup_failed_lineage(
                &terminal.fact,
                settled_at_ms,
                prior
                    .as_ref()
                    .map(|(id, ev, rev, occurred, task)| (*id, ev, *rev, *occurred, *task)),
                &journal,
                journal_max_completed_at_ms,
                command_id,
                expected_operation_id,
                action_epoch,
                false,
            )?;
            Ok(())
        }
        "settled" => {
            if operation.outcome_code.is_some() {
                return Err(StoreError::Corruption);
            }
            let settled_at_ms = operation.outcome_at_ms.ok_or(StoreError::Corruption)?;
            let projected_result = unpack_projection_blob::<Vec<EventId>>(
                "operations.result",
                operation.result.as_deref().ok_or(StoreError::Corruption)?,
            )?;
            validate_current_host_cleanup_journal(tx, &admission)?;
            let journal = load_ordered_host_cleanup_journal_outcomes(tx, expected_operation_id)?;
            let journal_max_completed_at_ms =
                host_cleanup_journal_max_completed_at(tx, expected_operation_id)?;
            let ordered_branch_ids = load_ordered_host_cleanup_branch_event_ids(
                tx,
                expected_operation_id,
                action_epoch,
            )?;
            if projected_result != ordered_branch_ids {
                return Err(StoreError::Corruption);
            }
            let terminal =
                require_exact_host_cleanup_settled_terminal(tx, command_id, expected_operation_id)?;
            if terminal.occurred_at_ms != settled_at_ms
                || terminal.fact.settled_at_ms != settled_at_ms
                || terminal.fact.result_event_ids != ordered_branch_ids
                || !terminal.fact.source.is_dispatch()
                || terminal.fact.action_epoch != Some(action_epoch)
                || terminal.fact.command_id != command_id
                || terminal.fact.operation_id != expected_operation_id
                || terminal.fact.resource_id.is_some()
                || terminal.fact.runtime_generation.is_some()
                || terminal.task_id.is_some()
                || terminal.task_revision.is_some()
            {
                return Err(StoreError::Corruption);
            }
            let prior = load_global_event_before(tx, terminal.sequence)?;
            crate::kernel::lineage::validate_host_admission_settled_lineage(
                &terminal.fact,
                settled_at_ms,
                prior
                    .as_ref()
                    .map(|(id, ev, rev, occurred, task)| (*id, ev, *rev, *occurred, *task)),
                &ordered_branch_ids,
                &journal,
                journal_max_completed_at_ms,
                operation.accepted_at_ms,
                command_id,
                expected_operation_id,
                action_epoch,
                false,
            )?;
            Ok(())
        }
        _ => Err(StoreError::Corruption),
    }
}

struct HostCleanupFailedTerminal {
    sequence: u64,
    occurred_at_ms: i64,
    task_id: Option<TaskId>,
    task_revision: Option<u64>,
    fact: OperationFailedFact,
}

enum HostOperationTerminalKind {
    Settled(OperationSettledFact),
    Failed(OperationFailedFact),
    Cancelled,
    Uncertain,
}

struct HostOperationTerminalMatch {
    event_id: EventId,
    sequence: u64,
    occurred_at_ms: i64,
    task_id: Option<TaskId>,
    task_revision: Option<u64>,
    kind: HostOperationTerminalKind,
}

/// Scan all durable operation terminal kinds for host quit identity matches.
/// One-sided command_id/operation_id matches are Corruption; unrelated terminals are ignored.
fn scan_host_operation_terminals(
    tx: &Connection,
    expected_command_id: CommandId,
    expected_operation_id: OperationId,
) -> Result<Vec<HostOperationTerminalMatch>, StoreError> {
    let mut stmt = tx.prepare(
        "SELECT sequence, event_id, task_id, task_revision, event_type, schema_version, payload, occurred_at_ms
         FROM events
         WHERE event_type IN (
             'operation.settled',
             'operation.failed',
             'operation.cancelled',
             'operation.uncertain'
         )
         ORDER BY sequence ASC",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, Vec<u8>>(1)?,
            row.get::<_, Option<Vec<u8>>>(2)?,
            row.get::<_, Option<i64>>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, i64>(5)?,
            row.get::<_, Vec<u8>>(6)?,
            row.get::<_, i64>(7)?,
        ))
    })?;
    let mut matches = Vec::new();
    for row in rows {
        let (
            sequence_i64,
            event_id_bytes,
            task_bytes,
            task_revision,
            event_type,
            schema_version,
            payload,
            occurred_at_ms,
        ) = row?;
        let sequence = u64_from_nonnegative_i64("events.sequence", sequence_i64)?;
        // Fail closed on any selected terminal row with invalid event_id bytes.
        let event_id = id16::<EventId>("events.event_id", &event_id_bytes)
            .map_err(|_| StoreError::Corruption)?;
        let event =
            crate::kernel::store::decode_stored_event(&event_type, schema_version, &payload)?;
        let (command_id, operation_id, kind) = match event {
            Event::OperationSettled(fact) => (
                fact.command_id,
                fact.operation_id,
                HostOperationTerminalKind::Settled(fact),
            ),
            Event::OperationFailed(fact) => (
                fact.command_id,
                fact.operation_id,
                HostOperationTerminalKind::Failed(fact),
            ),
            Event::OperationCancelled(fact) => (
                fact.command_id,
                fact.operation_id,
                HostOperationTerminalKind::Cancelled,
            ),
            Event::OperationUncertain(fact) => (
                fact.command_id,
                fact.operation_id,
                HostOperationTerminalKind::Uncertain,
            ),
            _ => return Err(StoreError::Corruption),
        };
        let command_match = command_id == expected_command_id;
        let operation_match = operation_id == expected_operation_id;
        match (command_match, operation_match) {
            (false, false) => continue,
            (true, true) => {
                let task_id = parse_optional_task_scope("events.task_id", task_bytes)?;
                let task_revision = match task_revision {
                    Some(v) => Some(u64_from_nonnegative_i64("events.task_revision", v)?),
                    None => None,
                };
                matches.push(HostOperationTerminalMatch {
                    event_id,
                    sequence,
                    occurred_at_ms,
                    task_id,
                    task_revision,
                    kind,
                });
            }
            _ => return Err(StoreError::Corruption),
        }
    }
    Ok(matches)
}

fn require_host_operation_terminal_match_count(
    tx: &Connection,
    expected_command_id: CommandId,
    expected_operation_id: OperationId,
    expected_count: usize,
) -> Result<(), StoreError> {
    let matches = scan_host_operation_terminals(tx, expected_command_id, expected_operation_id)?;
    if matches.len() != expected_count {
        return Err(StoreError::Corruption);
    }
    Ok(())
}

fn require_exact_host_cleanup_failed_terminal(
    tx: &Connection,
    expected_command_id: CommandId,
    expected_operation_id: OperationId,
) -> Result<HostCleanupFailedTerminal, StoreError> {
    let mut matches =
        scan_host_operation_terminals(tx, expected_command_id, expected_operation_id)?;
    if matches.len() != 1 {
        return Err(StoreError::Corruption);
    }
    let matched = matches.remove(0);
    let HostOperationTerminalKind::Failed(fact) = matched.kind else {
        return Err(StoreError::Corruption);
    };
    Ok(HostCleanupFailedTerminal {
        sequence: matched.sequence,
        occurred_at_ms: matched.occurred_at_ms,
        task_id: matched.task_id,
        task_revision: matched.task_revision,
        fact,
    })
}

struct HostCleanupSettledTerminal {
    event_id: EventId,
    sequence: u64,
    occurred_at_ms: i64,
    task_id: Option<TaskId>,
    task_revision: Option<u64>,
    fact: OperationSettledFact,
}

fn require_exact_host_cleanup_settled_terminal(
    tx: &Connection,
    expected_command_id: CommandId,
    expected_operation_id: OperationId,
) -> Result<HostCleanupSettledTerminal, StoreError> {
    let mut matches =
        scan_host_operation_terminals(tx, expected_command_id, expected_operation_id)?;
    if matches.len() != 1 {
        return Err(StoreError::Corruption);
    }
    let matched = matches.remove(0);
    let HostOperationTerminalKind::Settled(fact) = matched.kind else {
        return Err(StoreError::Corruption);
    };
    Ok(HostCleanupSettledTerminal {
        event_id: matched.event_id,
        sequence: matched.sequence,
        occurred_at_ms: matched.occurred_at_ms,
        task_id: matched.task_id,
        task_revision: matched.task_revision,
        fact,
    })
}

fn load_ordered_host_cleanup_branch_event_ids(
    tx: &Connection,
    operation_id: OperationId,
    action_epoch: u64,
) -> Result<Vec<EventId>, StoreError> {
    let mut stmt = tx.prepare(
        "SELECT event_id, task_id, task_revision, schema_version, payload
         FROM events
         WHERE event_type = 'host.cleanup_branch_completed'
         ORDER BY sequence ASC",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, Vec<u8>>(0)?,
            row.get::<_, Option<Vec<u8>>>(1)?,
            row.get::<_, Option<i64>>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, Vec<u8>>(4)?,
        ))
    })?;
    // Preserve observed durable sequence order; do not rebuild from enum ORDER via HashMap.
    let mut observed = Vec::with_capacity(HostCleanupBranch::ORDER.len());
    for row in rows {
        let (event_id_bytes, task_bytes, task_revision, schema_version, payload) = row?;
        if task_bytes.is_some() || task_revision.is_some() {
            return Err(StoreError::Corruption);
        }
        let event_id = id16::<EventId>("events.event_id", &event_id_bytes)?;
        let decoded = crate::kernel::store::decode_stored_event(
            "host.cleanup_branch_completed",
            schema_version,
            &payload,
        )?;
        let Event::HostCleanupBranchCompleted {
            operation_id: fact_operation_id,
            action_epoch: fact_epoch,
            branch,
            ..
        } = decoded
        else {
            return Err(StoreError::Corruption);
        };
        if fact_operation_id != operation_id || fact_epoch != action_epoch {
            return Err(StoreError::Corruption);
        }
        observed.push((branch, event_id));
    }
    if observed.len() != HostCleanupBranch::ORDER.len() {
        return Err(StoreError::Corruption);
    }
    let mut ordered = Vec::with_capacity(HostCleanupBranch::ORDER.len());
    for (idx, (branch, event_id)) in observed.into_iter().enumerate() {
        if branch != HostCleanupBranch::ORDER[idx] {
            return Err(StoreError::Corruption);
        }
        ordered.push(event_id);
    }
    Ok(ordered)
}

fn host_cleanup_journal_max_completed_at(
    tx: &Connection,
    operation_id: OperationId,
) -> Result<i64, StoreError> {
    let max_completed: Option<i64> = tx.query_row(
        "SELECT MAX(completed_at_ms) FROM host_cleanup_branches WHERE operation_id = ?1",
        [operation_id.as_bytes().as_slice()],
        |row| row.get(0),
    )?;
    max_completed.ok_or(StoreError::Corruption)
}

fn validate_pure_accepted_receipt(
    tx: &Connection,
    command_id: CommandId,
    expected_operation_id: OperationId,
    event_ids: &[EventId],
    receipt_final_revision: u64,
    scope: TaskId,
    committed_sequence: u64,
    operation: &OperationProjectionRow,
) -> Result<(), StoreError> {
    // Pure-command path: committed_sequence is operation.settled.
    let decision_count = u64::try_from(event_ids.len()).map_err(|_| StoreError::Corruption)?;
    let accepted_sequence = committed_sequence
        .checked_sub(1)
        .ok_or(StoreError::Corruption)?;
    let first_decision_sequence = accepted_sequence
        .checked_sub(decision_count)
        .ok_or(StoreError::Corruption)?;

    if operation.state != "settled"
        || operation.action_epoch.is_some()
        || operation.resource_id.is_some()
        || operation.runtime_generation.is_some()
        || operation.outcome_code.is_some()
    {
        return Err(StoreError::Corruption);
    }
    let projected_result = unpack_projection_blob::<Vec<EventId>>(
        "operations.result",
        operation.result.as_deref().ok_or(StoreError::Corruption)?,
    )?;
    if projected_result.as_slice() != event_ids {
        return Err(StoreError::Corruption);
    }

    validate_decision_event_batch(
        tx,
        event_ids,
        scope,
        receipt_final_revision,
        first_decision_sequence,
        operation.accepted_at_ms,
        is_pure_slice_decision_fact,
    )?;

    let accepted_row = load_event_row_at_sequence(tx, accepted_sequence)?;
    validate_accepted_fact_row(
        &accepted_row,
        command_id,
        expected_operation_id,
        Some(scope),
        operation.accepted_at_ms,
        OperationFence {
            action_epoch: None,
            resource_id: None,
            runtime_generation: None,
        },
    )?;
    ensure_unique_operation_accepted_fact(
        tx,
        command_id,
        expected_operation_id,
        Some(scope),
        operation.accepted_at_ms,
        accepted_sequence,
        &accepted_row,
        OperationFence {
            action_epoch: None,
            resource_id: None,
            runtime_generation: None,
        },
    )?;

    let settled_row = load_event_row_at_sequence(tx, committed_sequence)?;
    if settled_row.task_id != Some(scope) || settled_row.task_revision.is_some() {
        return Err(StoreError::Corruption);
    }
    let settled_event = crate::kernel::store::decode_stored_event(
        &settled_row.event_type,
        settled_row.schema_version,
        &settled_row.payload,
    )?;
    let Event::OperationSettled(settled_fact) = settled_event else {
        return Err(StoreError::Corruption);
    };
    let Some(outcome_at_ms) = operation.outcome_at_ms else {
        return Err(StoreError::Corruption);
    };
    // Pure settle is synchronous with acceptance: envelope, fact, and projection times match.
    if settled_fact.command_id != command_id
        || settled_fact.operation_id != expected_operation_id
        || settled_fact.result_event_ids.as_slice() != event_ids
        || settled_fact.action_epoch.is_some()
        || settled_fact.resource_id.is_some()
        || settled_fact.runtime_generation.is_some()
        || settled_fact.source != crate::domain::operation::OutcomeSource::Dispatch
        || settled_row.occurred_at_ms != settled_fact.settled_at_ms
        || settled_fact.settled_at_ms != outcome_at_ms
        || outcome_at_ms != operation.accepted_at_ms
    {
        return Err(StoreError::Corruption);
    }
    Ok(())
}

fn validate_side_effect_accepted_receipt(
    tx: &Connection,
    command_id: CommandId,
    expected_operation_id: OperationId,
    event_ids: &[EventId],
    receipt_final_revision: u64,
    scope: TaskId,
    committed_sequence: u64,
    operation: &OperationProjectionRow,
    outbox_rows: &[OutboxRow],
) -> Result<(), StoreError> {
    validate_side_effect_accepted_receipt_with_projection(
        tx,
        command_id,
        expected_operation_id,
        event_ids,
        receipt_final_revision,
        scope,
        committed_sequence,
        operation,
        outbox_rows,
        true,
        false,
    )
}

fn validate_side_effect_accepted_receipt_without_agent_sessions(
    tx: &Connection,
    command_id: CommandId,
    expected_operation_id: OperationId,
    event_ids: &[EventId],
    receipt_final_revision: u64,
    scope: TaskId,
    committed_sequence: u64,
    operation: &OperationProjectionRow,
    outbox_rows: &[OutboxRow],
) -> Result<(), StoreError> {
    validate_side_effect_accepted_receipt_with_projection(
        tx,
        command_id,
        expected_operation_id,
        event_ids,
        receipt_final_revision,
        scope,
        committed_sequence,
        operation,
        outbox_rows,
        false,
        true,
    )
}

fn validate_side_effect_accepted_receipt_with_projection(
    tx: &Connection,
    command_id: CommandId,
    expected_operation_id: OperationId,
    event_ids: &[EventId],
    receipt_final_revision: u64,
    scope: TaskId,
    committed_sequence: u64,
    operation: &OperationProjectionRow,
    outbox_rows: &[OutboxRow],
    validate_current_agent_sessions: bool,
    allow_expired_dispatch: bool,
) -> Result<(), StoreError> {
    // Side-effect path: committed_sequence is operation.accepted. Do not re-plan from
    // the current snapshot — correlate durable decision facts, accepted fence, and rows.
    let decision_count = u64::try_from(event_ids.len()).map_err(|_| StoreError::Corruption)?;
    let first_decision_sequence = committed_sequence
        .checked_sub(decision_count)
        .ok_or(StoreError::Corruption)?;

    let decision_facts = validate_decision_event_batch(
        tx,
        event_ids,
        scope,
        receipt_final_revision,
        first_decision_sequence,
        operation.accepted_at_ms,
        is_side_effect_decision_fact,
    )?;

    let expected_effects = effects_from_durable_decision_facts(
        tx,
        scope,
        first_decision_sequence,
        expected_operation_id,
        &decision_facts,
    )?;
    if expected_effects.len() != outbox_rows.len() {
        return Err(StoreError::Corruption);
    }

    let fence = OperationFence {
        action_epoch: match operation.action_epoch {
            Some(v) => Some(u64_from_nonnegative_i64("operations.action_epoch", v)?),
            None => None,
        },
        resource_id: match &operation.resource_id {
            Some(bytes) => Some(id16::<ResourceId>("operations.resource_id", bytes)?),
            None => None,
        },
        runtime_generation: match operation.runtime_generation {
            Some(v) => Some(u64_from_nonnegative_i64(
                "operations.runtime_generation",
                v,
            )?),
            None => None,
        },
    };
    for planned in &expected_effects {
        if planned.fence != fence {
            return Err(StoreError::Corruption);
        }
    }

    let accepted_row = load_event_row_at_sequence(tx, committed_sequence)?;
    validate_accepted_fact_row(
        &accepted_row,
        command_id,
        expected_operation_id,
        Some(scope),
        operation.accepted_at_ms,
        fence,
    )?;
    ensure_unique_operation_accepted_fact(
        tx,
        command_id,
        expected_operation_id,
        Some(scope),
        operation.accepted_at_ms,
        committed_sequence,
        &accepted_row,
        fence,
    )?;
    // Durable task mutation chain is the source of truth for revision/lifecycle.
    // A live claim and every ordinary receipt path must also match the current
    // projection. Once provider bytes may have crossed, the typed outbox
    // effect is the immutable attempt authority; the named provider session
    // row may legitimately have closed or advanced while this operation is
    // settled as Uncertain. The post-boundary path still compares every other
    // agent row and every other projection field.
    let provider_uncertainty = operation.state == "uncertain"
        && expected_effects.len() == 1
        && expected_effects[0].document.replay_policy == ReplayPolicy::NoAutomaticRetry
        && matches!(
            &expected_effects[0].document.effect,
            Effect::DeliverProviderInput { .. }
        );
    let immutable_provider_agent = if !validate_current_agent_sessions || provider_uncertainty {
        Some(provider_attempt_agent_session_id(&expected_effects)?)
    } else {
        None
    };
    let _ = validate_task_history_and_projection_with_agent_sessions(
        tx,
        scope,
        immutable_provider_agent,
    )?;

    match operation.state.as_str() {
        "accepted" => {
            if operation.result.is_some()
                || operation.outcome_code.is_some()
                || operation.outcome_at_ms.is_some()
            {
                return Err(StoreError::Corruption);
            }
            validate_side_effect_active_outbox(
                outbox_rows,
                &expected_effects,
                expected_operation_id,
                committed_sequence,
                operation.accepted_at_ms,
                scope,
                fence,
                allow_expired_dispatch,
            )?;
            let history = load_operation_outcome_history(
                tx,
                scope,
                committed_sequence,
                command_id,
                expected_operation_id,
            )?;
            if !history.is_empty() {
                return Err(StoreError::Corruption);
            }
            Ok(())
        }
        "settled" | "failed" | "cancelled" | "uncertain" => validate_side_effect_terminal_receipt(
            tx,
            command_id,
            expected_operation_id,
            scope,
            committed_sequence,
            operation,
            outbox_rows,
            &expected_effects,
            fence,
        ),
        _ => Err(StoreError::Corruption),
    }
}

fn provider_attempt_agent_session_id(
    expected_effects: &[PlannedEffect],
) -> Result<AgentSessionId, StoreError> {
    let [planned] = expected_effects else {
        return Err(StoreError::Corruption);
    };
    if planned.document.destination_class != DestinationClass::ProviderInput
        || planned.document.replay_policy != ReplayPolicy::NoAutomaticRetry
    {
        return Err(StoreError::Corruption);
    }
    match &planned.document.effect {
        Effect::DeliverProviderInput {
            agent_session_id, ..
        } => Ok(*agent_session_id),
        _ => Err(StoreError::Corruption),
    }
}

fn validate_side_effect_active_outbox(
    outbox_rows: &[OutboxRow],
    expected_effects: &[PlannedEffect],
    expected_operation_id: OperationId,
    committed_sequence: u64,
    accepted_at_ms: i64,
    scope: TaskId,
    fence: OperationFence,
    allow_expired_dispatch: bool,
) -> Result<(), StoreError> {
    for (expected_index, (row, planned)) in
        outbox_rows.iter().zip(expected_effects.iter()).enumerate()
    {
        let expected_index = i64::try_from(expected_index).map_err(|_| StoreError::Corruption)?;
        if row.effect_index != expected_index {
            return Err(StoreError::Corruption);
        }
        if row.operation_id != expected_operation_id {
            return Err(StoreError::Corruption);
        }
        if row.event_sequence != committed_sequence {
            return Err(StoreError::Corruption);
        }
        if row.lease_generation < 0 {
            return Err(StoreError::Corruption);
        }
        let decoded = decode_full_outbox_payload(row)?;
        if decoded != planned.document {
            return Err(StoreError::Corruption);
        }
        match row.state.as_str() {
            "pending" | "claimed" | "dispatching" | "reconcile_required" | "reconciling" => {
                validate_nonterminal_outbox_dispatch_metadata_for_attempt(
                    row,
                    accepted_at_ms,
                    decoded.replay_policy,
                    allow_expired_dispatch,
                )?;
            }
            _ => return Err(StoreError::Corruption),
        }
        if row.state == "pending" && row.last_error_class.is_some() {
            return Err(StoreError::Corruption);
        }
        validate_effect_matches_fence(&decoded.effect, scope, fence)?;
    }
    Ok(())
}

fn decode_full_outbox_payload(row: &OutboxRow) -> Result<PlannedEffectDocument, StoreError> {
    if row.payload.is_empty() || row.compacted_payload_sha256.is_some() {
        return Err(StoreError::Corruption);
    }
    decode_effect_document(&row.payload, &row.destination_class, &row.replay_policy)
}

fn validate_compacted_payload_marker(
    row: &OutboxRow,
    expected: &PlannedEffectDocument,
) -> Result<bool, StoreError> {
    if !row.payload.is_empty()
        || !matches!(row.state.as_str(), "settled" | "failed" | "cancelled")
        || row.destination_class != expected.destination_class.as_str()
        || row.replay_policy != expected.replay_policy.as_str()
    {
        return Err(StoreError::Corruption);
    }
    let stored = row
        .compacted_payload_sha256
        .as_deref()
        .ok_or(StoreError::Corruption)?;
    let stored: [u8; 32] = stored.try_into().map_err(|_| StoreError::Corruption)?;
    Ok(stored == effect_document_sha256(expected)?)
}

fn validate_terminal_outbox_payload(
    row: &OutboxRow,
    expected: &PlannedEffectDocument,
) -> Result<(), StoreError> {
    if row.payload.is_empty() {
        if validate_compacted_payload_marker(row, expected)? {
            return Ok(());
        }
        return Err(StoreError::Corruption);
    }
    let decoded = decode_full_outbox_payload(row)?;
    if decoded != *expected {
        return Err(StoreError::Corruption);
    }
    Ok(())
}

/// Recover the canonical effect document from an opaque callback token after
/// terminal payload cleanup. The complete durable receipt lineage is validated
/// again by the outcome path before any idempotent return.
pub(crate) fn effect_document_for_terminal_replay(
    row: &OutboxRow,
    callback_document: &PlannedEffectDocument,
) -> Result<PlannedEffectDocument, StoreError> {
    if row.payload.is_empty() {
        if !validate_compacted_payload_marker(row, callback_document)? {
            return Err(StoreError::StaleClaim);
        }
        return Ok(callback_document.clone());
    }
    decode_full_outbox_payload(row)
}

pub(crate) fn compact_terminal_outbox_payloads_in_tx(
    tx: &Transaction<'_>,
    batch_limit: u32,
) -> Result<(u64, u64, bool), StoreError> {
    if batch_limit == 0 {
        return Err(StoreError::ConstraintViolation);
    }
    let candidate_ids = {
        let mut stmt = tx.prepare(
            "SELECT outbox_id FROM outbox
             WHERE state IN ('settled', 'failed', 'cancelled')
               AND compacted_payload_sha256 IS NULL
               AND length(payload) > 0
             ORDER BY event_sequence, effect_index, outbox_id
             LIMIT ?1",
        )?;
        let rows = stmt.query_map([i64::from(batch_limit)], |row| row.get::<_, Vec<u8>>(0))?;
        rows.collect::<Result<Vec<_>, _>>()?
    };

    let mut rows_compacted = 0u64;
    let mut payload_bytes_reclaimed = 0u64;
    for candidate in candidate_ids {
        let outbox_id = id16::<OutboxId>("outbox.outbox_id", &candidate)?;
        let row = load_outbox_row_by_id(tx, outbox_id)?.ok_or(StoreError::Corruption)?;
        if !matches!(row.state.as_str(), "settled" | "failed" | "cancelled")
            || row.payload.is_empty()
            || row.compacted_payload_sha256.is_some()
        {
            return Err(StoreError::Corruption);
        }

        let operation =
            load_operation_projection_by_id(tx, row.operation_id)?.ok_or(StoreError::Corruption)?;
        let command_id = load_operation_command_id(tx, row.operation_id)?;
        let receipt_row = load_receipt_correlation(tx, command_id)?;
        let CommandReceipt::Accepted {
            command_id: receipt_command_id,
            operation_id: receipt_operation_id,
            event_ids,
            task_revision,
            prompt_mutation: _,
        } = &receipt_row.receipt
        else {
            return Err(StoreError::Corruption);
        };
        if *receipt_command_id != command_id || *receipt_operation_id != row.operation_id {
            return Err(StoreError::Corruption);
        }
        let committed_sequence = receipt_row
            .committed_sequence
            .map(|sequence| u64_to_sqlite_i64("command_receipts.committed_sequence", sequence))
            .transpose()?;
        validate_accepted_receipt_correlation(
            tx,
            command_id,
            row.operation_id,
            event_ids,
            *task_revision,
            receipt_row.task_id,
            committed_sequence,
            receipt_row.created_at_ms,
        )?;
        if operation.state != row.state {
            return Err(StoreError::Corruption);
        }

        let document = decode_full_outbox_payload(&row)?;
        let canonical_payload = encode_effect_document(&document)?;
        if canonical_payload != row.payload {
            return Err(StoreError::Corruption);
        }
        let digest = effect_document_sha256(&document)?;
        let payload_bytes = u64::try_from(row.payload.len()).map_err(|_| StoreError::Corruption)?;
        payload_bytes_reclaimed = payload_bytes_reclaimed
            .checked_add(payload_bytes)
            .ok_or(StoreError::Corruption)?;
        let changed = tx.execute(
            "UPDATE outbox
             SET payload = X'', compacted_payload_sha256 = ?1
             WHERE outbox_id = ?2 AND state = ?3
               AND compacted_payload_sha256 IS NULL AND payload = ?4",
            rusqlite::params![
                digest.as_slice(),
                row.outbox_id.as_bytes().as_slice(),
                row.state,
                row.payload,
            ],
        )?;
        if changed != 1 {
            return Err(StoreError::Corruption);
        }
        rows_compacted = rows_compacted
            .checked_add(1)
            .ok_or(StoreError::Corruption)?;
    }

    let has_more: i64 = tx.query_row(
        "SELECT EXISTS(
           SELECT 1 FROM outbox
           WHERE state IN ('settled', 'failed', 'cancelled')
             AND compacted_payload_sha256 IS NULL
             AND length(payload) > 0
         )",
        [],
        |row| row.get(0),
    )?;
    match has_more {
        0 => Ok((rows_compacted, payload_bytes_reclaimed, false)),
        1 => Ok((rows_compacted, payload_bytes_reclaimed, true)),
        _ => Err(StoreError::Corruption),
    }
}

fn validate_side_effect_terminal_receipt(
    tx: &Connection,
    command_id: CommandId,
    expected_operation_id: OperationId,
    scope: TaskId,
    committed_sequence: u64,
    operation: &OperationProjectionRow,
    outbox_rows: &[OutboxRow],
    expected_effects: &[PlannedEffect],
    fence: OperationFence,
) -> Result<(), StoreError> {
    let expected_outbox_state = operation.state.as_str();
    let expected_error = match expected_outbox_state {
        "settled" => None,
        "failed" => Some("side_effect_failed"),
        "cancelled" => Some("superseded"),
        "uncertain" => Some("ambiguous_dispatch"),
        _ => return Err(StoreError::Corruption),
    };
    let outcome_at = operation.outcome_at_ms.ok_or(StoreError::Corruption)?;
    if outbox_rows.len() != expected_effects.len() {
        return Err(StoreError::Corruption);
    }
    let history = load_operation_outcome_history(
        tx,
        scope,
        committed_sequence,
        command_id,
        expected_operation_id,
    )?;
    let dispatch_upper_bound = history
        .iter()
        .find_map(|fact| match fact {
            HistoricalOutcome::Uncertain { observed_at_ms, .. } => Some(*observed_at_ms),
            _ => None,
        })
        .unwrap_or(outcome_at);
    let was_reconciled = history
        .iter()
        .any(|fact| matches!(fact, HistoricalOutcome::Uncertain { .. }));
    for (expected_index, (row, planned)) in
        outbox_rows.iter().zip(expected_effects.iter()).enumerate()
    {
        let expected_index = i64::try_from(expected_index).map_err(|_| StoreError::Corruption)?;
        if row.effect_index != expected_index
            || row.operation_id != expected_operation_id
            || row.event_sequence != committed_sequence
            || row.state != expected_outbox_state
            || row.leased_until_ms.is_some()
            || row.last_error_class.as_deref() != expected_error
            || row.lease_generation < 0
            || row.reconciliation_receipt.is_some()
        {
            return Err(StoreError::Corruption);
        }
        validate_terminal_outbox_dispatch_metadata(
            row,
            operation.accepted_at_ms,
            dispatch_upper_bound,
            was_reconciled,
        )?;
        validate_terminal_outbox_payload(row, &planned.document)?;
        validate_effect_matches_fence(&planned.document.effect, scope, fence)?;
    }

    validate_terminal_outcome_history(
        tx,
        command_id,
        expected_operation_id,
        scope,
        operation,
        expected_effects,
        outbox_rows,
        fence,
        &history,
    )?;
    Ok(())
}

fn validate_nonterminal_outbox_dispatch_metadata(
    row: &OutboxRow,
    accepted_at_ms: i64,
    replay_policy: ReplayPolicy,
) -> Result<(), StoreError> {
    validate_nonterminal_outbox_dispatch_metadata_inner(row, accepted_at_ms, replay_policy, false)
}

fn validate_nonterminal_outbox_dispatch_metadata_for_attempt(
    row: &OutboxRow,
    accepted_at_ms: i64,
    replay_policy: ReplayPolicy,
    allow_expired_dispatch: bool,
) -> Result<(), StoreError> {
    // Recovery proves the lease is expired against its transaction clock after
    // this lineage check. Only the immutable typed provider path may therefore
    // accept a lease that ended before dispatch_started_at; generic/live paths
    // continue to require an active lease here.
    if allow_expired_dispatch
        && (replay_policy != ReplayPolicy::NoAutomaticRetry || row.state != "dispatching")
    {
        return Err(StoreError::Corruption);
    }
    validate_nonterminal_outbox_dispatch_metadata_inner(
        row,
        accepted_at_ms,
        replay_policy,
        allow_expired_dispatch,
    )
}

fn validate_nonterminal_outbox_dispatch_metadata_inner(
    row: &OutboxRow,
    accepted_at_ms: i64,
    replay_policy: ReplayPolicy,
    allow_expired_dispatch: bool,
) -> Result<(), StoreError> {
    validate_dispatch_attempt_generation(row)?;
    match row.state.as_str() {
        "pending" => {
            if row.attempts == 0 {
                if row.dispatch_started_at_ms.is_some()
                    || row.leased_until_ms.is_some()
                    || row.reconciliation_receipt.is_some()
                    || (row.lease_generation == 0 && row.available_at_ms != accepted_at_ms)
                    || (row.lease_generation > 0 && row.available_at_ms < accepted_at_ms)
                {
                    return Err(StoreError::Corruption);
                }
            } else {
                let Some(started) = row.dispatch_started_at_ms else {
                    return Err(StoreError::Corruption);
                };
                if row.available_at_ms < accepted_at_ms
                    || (row.lease_generation == 0 && row.available_at_ms > started)
                    || (row.lease_generation > 0 && row.available_at_ms < started)
                    || (row.lease_generation == 0 && row.reconciliation_receipt.is_some())
                {
                    return Err(StoreError::Corruption);
                }
                if let Some(lease) = row.leased_until_ms {
                    if lease < started {
                        return Err(StoreError::Corruption);
                    }
                }
                if row.lease_generation > 0 {
                    if row.leased_until_ms.is_some() {
                        return Err(StoreError::Corruption);
                    }
                    validate_retry_authorization(row, replay_policy)?;
                }
            }
        }
        "claimed" => {
            if row.lease_generation <= 0
                || row.leased_until_ms.is_none()
                || row.available_at_ms < accepted_at_ms
                || row.last_error_class.is_some()
            {
                return Err(StoreError::Corruption);
            }
            if row.leased_until_ms.ok_or(StoreError::Corruption)? <= row.available_at_ms {
                return Err(StoreError::Corruption);
            }
            if row.attempts == 0 {
                if row.dispatch_started_at_ms.is_some() || row.reconciliation_receipt.is_some() {
                    return Err(StoreError::Corruption);
                }
            } else {
                let Some(started) = row.dispatch_started_at_ms else {
                    return Err(StoreError::Corruption);
                };
                if row.available_at_ms < started || row.lease_generation <= row.attempts {
                    return Err(StoreError::Corruption);
                }
                validate_retry_authorization(row, replay_policy)?;
            }
        }
        "dispatching" => {
            if row.lease_generation <= 0
                || row.attempts <= 0
                || row.leased_until_ms.is_none()
                || row.last_error_class.is_some()
            {
                return Err(StoreError::Corruption);
            }
            let Some(started) = row.dispatch_started_at_ms else {
                return Err(StoreError::Corruption);
            };
            if row.available_at_ms < accepted_at_ms || row.available_at_ms > started {
                return Err(StoreError::Corruption);
            }
            let Some(lease) = row.leased_until_ms else {
                return Err(StoreError::Corruption);
            };
            if lease < 0 || (!allow_expired_dispatch && lease <= started) {
                return Err(StoreError::Corruption);
            }
            if row.reconciliation_receipt.is_some() {
                return Err(StoreError::Corruption);
            }
        }
        "reconcile_required" => {
            if replay_policy != ReplayPolicy::ReconcileBeforeRetry
                || row.lease_generation <= 0
                || row.attempts <= 0
                || row.lease_generation < row.attempts
                || row.leased_until_ms.is_some()
                || row.reconciliation_receipt.is_some()
                || row.last_error_class.as_deref() != Some("ambiguous_dispatch")
            {
                return Err(StoreError::Corruption);
            }
            let started = row.dispatch_started_at_ms.ok_or(StoreError::Corruption)?;
            if started < accepted_at_ms || row.available_at_ms < started {
                return Err(StoreError::Corruption);
            }
        }
        "reconciling" => {
            if replay_policy != ReplayPolicy::ReconcileBeforeRetry
                || row.lease_generation <= 0
                || row.attempts <= 0
                || row.lease_generation <= row.attempts
                || row.leased_until_ms.is_none()
                || row.reconciliation_receipt.is_some()
                || row.last_error_class.as_deref() != Some("ambiguous_dispatch")
            {
                return Err(StoreError::Corruption);
            }
            let started = row.dispatch_started_at_ms.ok_or(StoreError::Corruption)?;
            let lease = row.leased_until_ms.ok_or(StoreError::Corruption)?;
            if started < accepted_at_ms
                || row.available_at_ms < started
                || lease <= row.available_at_ms
            {
                return Err(StoreError::Corruption);
            }
        }
        _ => return Err(StoreError::Corruption),
    }
    Ok(())
}

fn validate_retry_authorization(
    row: &OutboxRow,
    replay_policy: ReplayPolicy,
) -> Result<(), StoreError> {
    match replay_policy {
        ReplayPolicy::RetrySafe => {
            if row.reconciliation_receipt.is_some() {
                return Err(StoreError::Corruption);
            }
            Ok(())
        }
        ReplayPolicy::ReconcileBeforeRetry => {
            let payload = row
                .reconciliation_receipt
                .as_deref()
                .ok_or(StoreError::Corruption)?;
            let receipt = decode_absence_receipt(payload)?;
            let effect_index =
                u32::try_from(row.effect_index).map_err(|_| StoreError::Corruption)?;
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
        ReplayPolicy::NoAutomaticRetry => Err(StoreError::Corruption),
    }
}

fn validate_terminal_outbox_dispatch_metadata(
    row: &OutboxRow,
    accepted_at_ms: i64,
    dispatch_upper_bound_ms: i64,
    was_reconciled: bool,
) -> Result<(), StoreError> {
    validate_dispatch_attempt_generation(row)?;
    if row.attempts == 0 {
        if row.dispatch_started_at_ms.is_some()
            || (row.lease_generation == 0 && row.available_at_ms != accepted_at_ms)
            || (row.lease_generation > 0 && row.available_at_ms < accepted_at_ms)
        {
            return Err(StoreError::Corruption);
        }
    } else {
        let Some(started) = row.dispatch_started_at_ms else {
            return Err(StoreError::Corruption);
        };
        // Recovery scheduling may move availability after the dispatch start;
        // both durable clocks must still precede uncertainty/finality.
        if row.available_at_ms < accepted_at_ms
            || started < accepted_at_ms
            || (!was_reconciled && row.available_at_ms > started)
            || row.available_at_ms > dispatch_upper_bound_ms
            || started > dispatch_upper_bound_ms
        {
            return Err(StoreError::Corruption);
        }
    }
    Ok(())
}

/// A dispatch attempt can only be created in a claim generation at or after
/// the attempt count: a worker may claim/release before bytes cross, so the
/// generation may be ahead, but durable recovery/rebuild must never accept an
/// attempt whose generation trails the number of starts. All state transitions
/// additionally compare the exact stored generation in their CAS predicate.
fn validate_dispatch_attempt_generation(row: &OutboxRow) -> Result<(), StoreError> {
    if row.lease_generation < 0
        || row.attempts < 0
        || (row.attempts > 0 && row.lease_generation < row.attempts)
    {
        return Err(StoreError::Corruption);
    }
    Ok(())
}

fn validate_terminal_outcome_history(
    tx: &Connection,
    command_id: CommandId,
    expected_operation_id: OperationId,
    scope: TaskId,
    operation: &OperationProjectionRow,
    expected_effects: &[PlannedEffect],
    outbox_rows: &[OutboxRow],
    fence: OperationFence,
    history: &[HistoricalOutcome],
) -> Result<(), StoreError> {
    let outcome_at = operation.outcome_at_ms.ok_or(StoreError::Corruption)?;
    // Global terminal chronology: no terminal observation may predate acceptance.
    if outcome_at < operation.accepted_at_ms {
        return Err(StoreError::Corruption);
    }
    match operation.state.as_str() {
        "uncertain" => {
            if history.len() != 1 {
                return Err(StoreError::Corruption);
            }
            let HistoricalOutcome::Uncertain {
                command_id: fact_cmd,
                operation_id: fact_op,
                observed_at_ms,
                code,
                action_epoch,
                resource_id,
                runtime_generation,
                ..
            } = &history[0]
            else {
                return Err(StoreError::Corruption);
            };
            if *fact_cmd != command_id
                || *fact_op != expected_operation_id
                || *observed_at_ms != outcome_at
                || *action_epoch != fence.action_epoch
                || *resource_id != fence.resource_id
                || *runtime_generation != fence.runtime_generation
                || operation.result.is_some()
                || operation.outcome_code.as_deref() != Some(uncertain_code_text(*code))
            {
                return Err(StoreError::Corruption);
            }
            Ok(())
        }
        "cancelled" => {
            if history.len() != 1 {
                return Err(StoreError::Corruption);
            }
            let HistoricalOutcome::Cancelled {
                command_id: fact_cmd,
                operation_id: fact_op,
                settled_at_ms,
                reason,
                action_epoch,
                resource_id,
                runtime_generation,
                ..
            } = &history[0]
            else {
                return Err(StoreError::Corruption);
            };
            if *fact_cmd != command_id
                || *fact_op != expected_operation_id
                || *settled_at_ms != outcome_at
                || *action_epoch != fence.action_epoch
                || *resource_id != fence.resource_id
                || *runtime_generation != fence.runtime_generation
                || operation.result.is_some()
                || operation.outcome_code.as_deref() != Some(cancel_code_text(*reason))
            {
                return Err(StoreError::Corruption);
            }
            Ok(())
        }
        "failed" => {
            let (uncertain_prefix, failed) = match history {
                [HistoricalOutcome::Failed { .. }] => (None, &history[0]),
                [HistoricalOutcome::Uncertain { .. }, HistoricalOutcome::Failed { .. }] => {
                    (Some(&history[0]), &history[1])
                }
                _ => return Err(StoreError::Corruption),
            };
            if let Some(HistoricalOutcome::Uncertain {
                command_id: u_cmd,
                operation_id: u_op,
                observed_at_ms,
                code: u_code,
                action_epoch,
                resource_id,
                runtime_generation,
                ..
            }) = uncertain_prefix
            {
                if *u_cmd != command_id
                    || *u_op != expected_operation_id
                    || *action_epoch != fence.action_epoch
                    || *resource_id != fence.resource_id
                    || *runtime_generation != fence.runtime_generation
                    || *observed_at_ms < operation.accepted_at_ms
                    || *observed_at_ms > outcome_at
                    || *u_code != OperationUncertaintyCode::AmbiguousDispatch
                {
                    return Err(StoreError::Corruption);
                }
            }
            let HistoricalOutcome::Failed {
                command_id: fact_cmd,
                operation_id: fact_op,
                settled_at_ms,
                code,
                action_epoch,
                resource_id,
                runtime_generation,
                source,
                ..
            } = failed
            else {
                return Err(StoreError::Corruption);
            };
            if *fact_cmd != command_id
                || *fact_op != expected_operation_id
                || *settled_at_ms != outcome_at
                || *action_epoch != fence.action_epoch
                || *resource_id != fence.resource_id
                || *runtime_generation != fence.runtime_generation
                || operation.result.is_some()
                || operation.outcome_code.as_deref() != Some(error_code_text(*code))
            {
                return Err(StoreError::Corruption);
            }
            if uncertain_prefix.is_some() {
                validate_verified_reconciliation_source(source, outbox_rows)?;
            } else if !source.is_dispatch() {
                return Err(StoreError::Corruption);
            }
            Ok(())
        }
        "settled" => {
            let (uncertain_prefix, settled) = match history {
                [HistoricalOutcome::Settled { .. }] => (None, &history[0]),
                [HistoricalOutcome::Uncertain { .. }, HistoricalOutcome::Settled { .. }] => {
                    (Some(&history[0]), &history[1])
                }
                _ => return Err(StoreError::Corruption),
            };
            if let Some(HistoricalOutcome::Uncertain {
                command_id: u_cmd,
                operation_id: u_op,
                observed_at_ms,
                code: u_code,
                action_epoch,
                resource_id,
                runtime_generation,
                ..
            }) = uncertain_prefix
            {
                if *u_cmd != command_id
                    || *u_op != expected_operation_id
                    || *action_epoch != fence.action_epoch
                    || *resource_id != fence.resource_id
                    || *runtime_generation != fence.runtime_generation
                    || *observed_at_ms < operation.accepted_at_ms
                    || *observed_at_ms > outcome_at
                    || *u_code != OperationUncertaintyCode::AmbiguousDispatch
                {
                    return Err(StoreError::Corruption);
                }
            }
            let HistoricalOutcome::Settled {
                sequence: settled_sequence,
                command_id: fact_cmd,
                operation_id: fact_op,
                settled_at_ms,
                result_event_ids,
                action_epoch,
                resource_id,
                runtime_generation,
                source,
                ..
            } = settled
            else {
                return Err(StoreError::Corruption);
            };
            if *fact_cmd != command_id
                || *fact_op != expected_operation_id
                || *settled_at_ms != outcome_at
                || *action_epoch != fence.action_epoch
                || *resource_id != fence.resource_id
                || *runtime_generation != fence.runtime_generation
                || operation.outcome_code.is_some()
            {
                return Err(StoreError::Corruption);
            }
            let projected_result = unpack_projection_blob::<Vec<EventId>>(
                "operations.result",
                operation.result.as_deref().ok_or(StoreError::Corruption)?,
            )?;
            if projected_result.as_slice() != result_event_ids.as_slice() {
                return Err(StoreError::Corruption);
            }
            if uncertain_prefix.is_some() {
                validate_verified_reconciliation_source(source, outbox_rows)?;
            } else if !source.is_dispatch() {
                return Err(StoreError::Corruption);
            }
            if result_event_ids.len() != 1 || expected_effects.len() != 1 {
                return Err(StoreError::Corruption);
            }
            validate_settled_result_fact(
                tx,
                scope,
                &expected_effects[0].document.effect,
                result_event_ids[0],
                *settled_at_ms,
                *settled_sequence,
            )?;
            Ok(())
        }
        _ => Err(StoreError::Corruption),
    }
}

fn validate_verified_reconciliation_source(
    source: &OutcomeSource,
    outbox_rows: &[OutboxRow],
) -> Result<(), StoreError> {
    let OutcomeSource::VerifiedReconciliation {
        effect_index,
        external_identity,
    } = source
    else {
        return Err(StoreError::Corruption);
    };
    if *effect_index != 0 {
        return Err(StoreError::Corruption);
    }
    if outbox_rows.len() != 1 {
        return Err(StoreError::Corruption);
    }
    if outbox_rows[0].effect_index != 0 {
        return Err(StoreError::Corruption);
    }
    if i64::from(*effect_index) != outbox_rows[0].effect_index {
        return Err(StoreError::Corruption);
    }
    if external_identity.is_empty() {
        return Err(StoreError::Corruption);
    }
    Ok(())
}

fn validate_settled_result_fact(
    tx: &Connection,
    scope: TaskId,
    effect: &Effect,
    result_id: EventId,
    settled_at_ms: i64,
    settled_sequence: u64,
) -> Result<(), StoreError> {
    let row: Option<(
        i64,
        Option<Vec<u8>>,
        Option<i64>,
        String,
        i64,
        Vec<u8>,
        i64,
    )> = tx
        .query_row(
            "SELECT sequence, task_id, task_revision, event_type, schema_version, payload, occurred_at_ms
             FROM events WHERE event_id = ?1",
            [result_id.as_bytes().as_slice()],
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
    let Some((
        result_sequence_i64,
        task_bytes,
        task_revision,
        event_type,
        schema_version,
        payload,
        occurred_at,
    )) = row
    else {
        return Err(StoreError::Corruption);
    };
    let result_sequence = u64_from_nonnegative_i64("events.sequence", result_sequence_i64)?;
    let result_task = parse_optional_task_scope("events.task_id", task_bytes)?;
    if result_task != Some(scope) || occurred_at != settled_at_ms {
        return Err(StoreError::Corruption);
    }
    if let Effect::DeliverProviderInput {
        operation_id,
        command_id,
        client_id,
        agent_session_id,
        provider_kind,
        provider_session_id,
        runtime_generation,
        action_epoch,
        turn_id,
        question_id,
        approval_id,
        ..
    } = effect
    {
        if task_revision.is_some()
            || settled_sequence
                != result_sequence
                    .checked_add(1)
                    .ok_or(StoreError::Corruption)?
        {
            return Err(StoreError::Corruption);
        }
        let decoded =
            crate::kernel::store::decode_stored_event(&event_type, schema_version, &payload)?;
        match decoded {
            Event::ProviderInputDelivered {
                command_id: result_command,
                client_id: result_client,
                operation_id: result_operation,
                agent_session_id: result_agent,
                provider_kind: result_kind,
                provider_session_id: result_session,
                runtime_generation: result_generation,
                turn_id: result_turn,
                action_epoch: result_epoch,
                question_id: result_question,
                approval_id: result_approval,
            } if event_type == "provider_input.delivered"
                && result_command == *command_id
                && result_client == *client_id
                && result_operation == *operation_id
                && result_agent == *agent_session_id
                && result_kind == *provider_kind
                && result_session == *provider_session_id
                && result_generation == *runtime_generation
                && result_turn == *turn_id
                && result_epoch == *action_epoch
                && result_question == *question_id
                && result_approval == *approval_id => {}
            _ => return Err(StoreError::Corruption),
        }
        // A provider session keeps only its latest settlement. Requiring that
        // current projection to still name this operation makes every earlier
        // delivered turn look corrupt as soon as a later turn settles. The
        // immutable delivered fact above proves this operation's result; the
        // complete ordered replay below proves the latest provider projection.
        let _ = validate_task_history_and_projection(tx, scope)?;
        return Ok(());
    }
    let Some(result_revision) = task_revision else {
        return Err(StoreError::Corruption);
    };
    let result_revision = u64_from_nonnegative_i64("events.task_revision", result_revision)?;
    ensure_unique_task_revision(tx, scope, result_revision, result_id)?;
    let prior = load_latest_prior_task_mutation(tx, scope, result_sequence)?;
    let expected_revision = prior.checked_add(1).ok_or(StoreError::IntegerOutOfRange {
        field: "events.task_revision",
        value: u64::MAX,
    })?;
    if result_revision != expected_revision {
        return Err(StoreError::Corruption);
    }
    let decoded = crate::kernel::store::decode_stored_event(&event_type, schema_version, &payload)?;
    match (effect, decoded) {
        (Effect::BeginTaskTeardown { .. }, Event::TaskArchived)
            if event_type == "task.archived" => {}
        (
            Effect::ReleaseResource { resource_fence, .. },
            Event::ResourceReleased {
                resource_id,
                runtime_generation,
            },
        ) if event_type == "resource.released"
            && resource_id == resource_fence.resource_id
            && runtime_generation == resource_fence.runtime_generation => {}
        (Effect::HoldBrowserHost { .. }, _) => return Err(StoreError::Corruption),
        _ => return Err(StoreError::Corruption),
    }
    if settled_sequence
        != result_sequence
            .checked_add(1)
            .ok_or(StoreError::Corruption)?
    {
        return Err(StoreError::Corruption);
    }
    validate_settled_projections(tx, scope, effect, result_revision, occurred_at)?;
    Ok(())
}

fn validate_settled_projections(
    tx: &Connection,
    scope: TaskId,
    effect: &Effect,
    result_revision: u64,
    result_occurred_at_ms: i64,
) -> Result<(), StoreError> {
    let (_lifecycle, _epoch, durable_revision) = validate_task_history_and_projection(tx, scope)?;
    if durable_revision < result_revision {
        return Err(StoreError::Corruption);
    }

    if let Effect::ReleaseResource { resource_fence, .. } = effect {
        let row: Option<(Option<Vec<u8>>, String, String, i64, i64)> = tx
            .query_row(
                "SELECT task_id, owner_kind, lifecycle, runtime_generation, updated_at_ms
                 FROM resources WHERE resource_id = ?1",
                [resource_fence.resource_id.as_bytes().as_slice()],
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
        let Some((owned_task, owner_kind, res_lifecycle, generation, updated_at_ms)) = row else {
            return Err(StoreError::Corruption);
        };
        let generation = u64_from_nonnegative_i64("resources.runtime_generation", generation)?;
        let owned_ok = matches!(
            owned_task.as_deref(),
            Some(bytes) if bytes == scope.as_bytes().as_slice()
        );
        if !owned_ok
            || owner_kind != "task"
            || res_lifecycle != "released"
            || generation != resource_fence.runtime_generation
            || updated_at_ms != result_occurred_at_ms
        {
            return Err(StoreError::Corruption);
        }
    }
    Ok(())
}

/// Strict durable task-history validator via ordered domain `apply` replay.
/// Contiguous revisions and entity/ownership transitions come from `apply`;
/// command-decision lifecycle gates that `apply` does not enforce are checked separately.
/// Final replayed snapshot must equal the complete current projection.
fn validate_task_history_and_projection(
    tx: &Connection,
    scope: TaskId,
) -> Result<(String, u64, u64), StoreError> {
    validate_task_history_and_projection_with_agent_sessions(tx, scope, None)
}

fn validate_task_history_and_projection_with_agent_sessions(
    tx: &Connection,
    scope: TaskId,
    immutable_provider_agent: Option<AgentSessionId>,
) -> Result<(String, u64, u64), StoreError> {
    let mut stmt = tx.prepare(
        "SELECT sequence, event_id, task_revision, event_type, schema_version, payload,
                occurred_at_ms
         FROM events
         WHERE task_id = ?1
         ORDER BY sequence ASC",
    )?;
    let rows = stmt.query_map([scope.as_bytes().as_slice()], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, Vec<u8>>(1)?,
            row.get::<_, Option<i64>>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, i64>(4)?,
            row.get::<_, Vec<u8>>(5)?,
            row.get::<_, i64>(6)?,
        ))
    })?;

    let mut snapshot: Option<TaskSnapshot> = None;
    let mut durable_revision: Option<u64> = None;
    let mut last_mutation_at_ms: Option<i64> = None;

    for row in rows {
        let (
            sequence_i64,
            event_id_bytes,
            task_revision,
            event_type,
            schema_version,
            payload,
            occurred_at_ms,
        ) = row?;
        let sequence = u64_from_nonnegative_i64("events.sequence", sequence_i64)?;
        let event_id = id16::<EventId>("events.event_id", &event_id_bytes)?;
        let decoded =
            crate::kernel::store::decode_stored_event(&event_type, schema_version, &payload)?;
        let is_mutation = decoded.is_task_mutation();
        let revision = match (is_mutation, task_revision) {
            (true, None) | (false, Some(_)) => return Err(StoreError::Corruption),
            (false, None) => None,
            (true, Some(rev_i64)) => {
                Some(u64_from_nonnegative_i64("events.task_revision", rev_i64)?)
            }
        };

        if crate::kernel::lineage::is_derived_lifecycle_result(&decoded) {
            // Global durable adjacency: next existing row (any task), not next same-task row.
            let next = load_global_event_after(tx, sequence)?;
            let next_arg = next
                .as_ref()
                .map(|(id, ev, rev, at, task)| (*id, ev, *rev, *at, *task));
            crate::kernel::lineage::validate_derived_settled_adjacency(
                event_id,
                &decoded,
                revision,
                occurred_at_ms,
                Some(scope),
                next_arg,
                false,
            )?;
        } else if let Event::OperationSettled(fact) = &decoded {
            let prior = load_global_event_before(tx, sequence)?;
            let prior_arg = prior
                .as_ref()
                .map(|(id, ev, rev, at, task)| (*id, ev, *rev, *at, *task));
            crate::kernel::lineage::validate_side_effect_settled_has_prior_derived(
                fact,
                occurred_at_ms,
                Some(scope),
                prior_arg,
                false,
            )?;
        }

        if is_mutation {
            enforce_command_decision_lifecycle_gate(&decoded, snapshot.as_ref())?;
        }
        let domain = DomainEvent {
            id: event_id,
            task_id: Some(scope),
            sequence,
            task_revision: revision,
            occurred_at_ms,
            payload: decoded.clone(),
        };
        snapshot = Some(apply_domain_event(snapshot, &domain).map_err(|_| StoreError::Corruption)?);
        if let Some(rev) = revision {
            durable_revision = Some(rev);
            last_mutation_at_ms = Some(occurred_at_ms);
        }
    }
    let Some(snap) = snapshot else {
        return Err(StoreError::Corruption);
    };
    let Some(durable_revision) = durable_revision else {
        return Err(StoreError::Corruption);
    };
    let Some(last_mutation_at_ms) = last_mutation_at_ms else {
        return Err(StoreError::Corruption);
    };
    if snap.task.revision != durable_revision {
        return Err(StoreError::Corruption);
    }

    let projected = match load_task_snapshot(tx, scope) {
        Ok(Some(projected)) => projected,
        Ok(None) => return Err(StoreError::Corruption),
        // Unreadable or ownership-broken projection is a durable integrity failure.
        Err(StoreError::Projection(_)) | Err(StoreError::CodecMismatch { .. }) => {
            return Err(StoreError::Corruption);
        }
        Err(err) => return Err(err),
    };
    let agents_match = match immutable_provider_agent {
        None => snap.agents == projected.agents,
        Some(exception) => {
            // The immutable escape is scoped to exactly the provider session
            // named by the accepted effect. Every other durable agent row,
            // including additions/removals, remains part of projection
            // equality; a broad map skip would hide unrelated tampering.
            snap.agents.len() == projected.agents.len()
                && snap.agents.iter().all(|(agent_id, durable)| {
                    let Some(current) = projected.agents.get(agent_id) else {
                        return false;
                    };
                    *agent_id == exception || durable == current
                })
        }
    };
    let same_projection = snap.task == projected.task
        && snap.connectivity == projected.connectivity
        && snap.attention == projected.attention
        && snap.activity == projected.activity
        && snap.review_readiness == projected.review_readiness
        && snap.primary_agent_id == projected.primary_agent_id
        && agents_match
        && snap.artifacts == projected.artifacts
        && snap.resources == projected.resources
        && snap.provider_sessions == projected.provider_sessions;
    if !same_projection {
        return Err(StoreError::Corruption);
    }

    let proj_updated_at: i64 = tx.query_row(
        "SELECT updated_at_ms FROM tasks WHERE task_id = ?1",
        [scope.as_bytes().as_slice()],
        |row| row.get(0),
    )?;
    if proj_updated_at != last_mutation_at_ms {
        return Err(StoreError::Corruption);
    }

    let expected_lifecycle = match snap.task.lifecycle {
        TaskLifecycle::Open => "open",
        TaskLifecycle::Settled => "settled",
        TaskLifecycle::Closing => "closing",
        TaskLifecycle::Archived => "archived",
    };
    Ok((
        expected_lifecycle.to_string(),
        snap.task.action_epoch,
        durable_revision,
    ))
}

fn load_global_event_before(
    tx: &Connection,
    sequence: u64,
) -> Result<Option<(EventId, Event, Option<u64>, i64, Option<TaskId>)>, StoreError> {
    load_global_event_adjacent(tx, sequence, /*after=*/ false)
}

fn load_global_event_after(
    tx: &Connection,
    sequence: u64,
) -> Result<Option<(EventId, Event, Option<u64>, i64, Option<TaskId>)>, StoreError> {
    load_global_event_adjacent(tx, sequence, /*after=*/ true)
}

fn load_global_event_adjacent(
    tx: &Connection,
    sequence: u64,
    after: bool,
) -> Result<Option<(EventId, Event, Option<u64>, i64, Option<TaskId>)>, StoreError> {
    let sql = if after {
        "SELECT event_id, task_id, task_revision, event_type, schema_version, payload,
                occurred_at_ms
         FROM events
         WHERE sequence > ?1
         ORDER BY sequence ASC
         LIMIT 1"
    } else {
        "SELECT event_id, task_id, task_revision, event_type, schema_version, payload,
                occurred_at_ms
         FROM events
         WHERE sequence < ?1
         ORDER BY sequence DESC
         LIMIT 1"
    };
    let row: Option<(
        Vec<u8>,
        Option<Vec<u8>>,
        Option<i64>,
        String,
        i64,
        Vec<u8>,
        i64,
    )> = tx
        .query_row(
            sql,
            [u64_to_sqlite_i64("events.sequence", sequence)?],
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
    let Some((
        event_id_bytes,
        task_bytes,
        task_revision,
        event_type,
        schema_version,
        payload,
        occurred_at_ms,
    )) = row
    else {
        return Ok(None);
    };
    let event_id = id16::<EventId>("events.event_id", &event_id_bytes)?;
    let task_id = parse_optional_task_scope("events.task_id", task_bytes)?;
    let task_revision = match task_revision {
        Some(v) => Some(u64_from_nonnegative_i64("events.task_revision", v)?),
        None => None,
    };
    let decoded = crate::kernel::store::decode_stored_event(&event_type, schema_version, &payload)?;
    Ok(Some((
        event_id,
        decoded,
        task_revision,
        occurred_at_ms,
        task_id,
    )))
}

fn enforce_command_decision_lifecycle_gate(
    event: &Event,
    snapshot: Option<&TaskSnapshot>,
) -> Result<(), StoreError> {
    let Some(snap) = snapshot else {
        return Ok(());
    };
    match event {
        Event::TaskRenamed { .. }
        | Event::TaskAttentionSet { .. }
        | Event::ArtifactRegistered { .. }
        | Event::ResourceReleaseBegun { .. }
        | Event::ResourceReleased { .. } => {
            if !matches!(
                snap.task.lifecycle,
                TaskLifecycle::Open | TaskLifecycle::Settled | TaskLifecycle::Closing
            ) {
                return Err(StoreError::Corruption);
            }
        }
        Event::AgentSessionRegistered { .. }
        | Event::PrimaryAgentSet { .. }
        | Event::ResourceRegistered { .. }
        | Event::ProviderQuestionPresented { .. }
        | Event::ProviderApprovalPresented { .. } => {
            if !matches!(
                snap.task.lifecycle,
                TaskLifecycle::Open | TaskLifecycle::Settled
            ) {
                return Err(StoreError::Corruption);
            }
        }
        Event::ProviderInputAccepted { action, .. } => {
            let valid = snap.task.lifecycle == TaskLifecycle::Open
                || (snap.task.lifecycle == TaskLifecycle::Settled
                    && matches!(action, crate::domain::ProviderInputAction::SendNow { .. }));
            if !valid {
                return Err(StoreError::Corruption);
            }
        }
        Event::ProviderWaitSettled { .. } => {
            if !matches!(
                snap.task.lifecycle,
                TaskLifecycle::Open | TaskLifecycle::Settled | TaskLifecycle::Closing
            ) {
                return Err(StoreError::Corruption);
            }
        }
        _ => {}
    }
    Ok(())
}

fn error_code_text(value: OperationErrorCode) -> &'static str {
    match value {
        OperationErrorCode::SideEffectFailed => "side_effect_failed",
        OperationErrorCode::CleanupFailed => "cleanup_failed",
    }
}

fn cancel_code_text(value: CancellationReason) -> &'static str {
    match value {
        CancellationReason::Superseded => "superseded",
    }
}

fn uncertain_code_text(value: OperationUncertaintyCode) -> &'static str {
    match value {
        OperationUncertaintyCode::AmbiguousDispatch => "ambiguous_dispatch",
    }
}

fn effects_from_durable_decision_facts(
    tx: &Connection,
    scope: TaskId,
    first_decision_sequence: u64,
    expected_operation_id: OperationId,
    decision_facts: &[(Event, u64)],
) -> Result<Vec<PlannedEffect>, StoreError> {
    let mut planned = Vec::new();
    for (offset, (fact, _)) in decision_facts.iter().enumerate() {
        let offset_u64 = u64::try_from(offset).map_err(|_| StoreError::Corruption)?;
        let decision_sequence = first_decision_sequence
            .checked_add(offset_u64)
            .ok_or(StoreError::Corruption)?;
        match fact {
            Event::TaskCloseBegun {
                action_epoch: epoch,
            } => {
                planned.push(PlannedEffect {
                    document: crate::kernel::outbox::PlannedEffectDocument::new(
                        Effect::BeginTaskTeardown {
                            task_id: scope,
                            action_epoch: *epoch,
                        },
                        crate::kernel::outbox::ReplayPolicy::RetrySafe,
                    ),
                    fence: OperationFence {
                        action_epoch: Some(*epoch),
                        resource_id: None,
                        runtime_generation: None,
                    },
                });
            }
            Event::Browser(crate::domain::browser::BrowserDurableFact::RequestAccepted {
                request_id,
                task_id: fact_task,
                context_id,
                generation,
                action,
                action_epoch,
                ..
            }) if action.requires_host_settlement() => {
                if *fact_task != scope || *generation == 0 {
                    return Err(StoreError::Corruption);
                }
                planned.push(PlannedEffect {
                    document: crate::kernel::outbox::PlannedEffectDocument::new(
                        Effect::HoldBrowserHost {
                            task_id: scope,
                            action_epoch: *action_epoch,
                            request_id: *request_id,
                            context_id: *context_id,
                            generation: *generation,
                            hold: crate::domain::browser::BrowserIntegrationHold::WebViewSurfaceAbsent,
                        },
                        crate::kernel::outbox::ReplayPolicy::NoAutomaticRetry,
                    ),
                    fence: OperationFence {
                        action_epoch: Some(*action_epoch),
                        resource_id: None,
                        runtime_generation: None,
                    },
                });
            }
            Event::ResourceReleaseBegun {
                resource_id,
                runtime_generation,
            } => {
                // Historical epoch from replayed task state at this decision — never from
                // the operations projection row (avoids circular consistent tampering).
                let epoch = historical_action_epoch_through(tx, scope, decision_sequence)?;
                planned.push(PlannedEffect {
                    document: crate::kernel::outbox::PlannedEffectDocument::new(
                        Effect::ReleaseResource {
                            task_id: scope,
                            action_epoch: epoch,
                            resource_fence: ResourceFence::new(*resource_id, *runtime_generation),
                        },
                        crate::kernel::outbox::ReplayPolicy::ReconcileBeforeRetry,
                    ),
                    fence: OperationFence {
                        action_epoch: Some(epoch),
                        resource_id: Some(*resource_id),
                        runtime_generation: Some(*runtime_generation),
                    },
                });
            }
            Event::ProviderInputAccepted {
                operation_id,
                command_id,
                client_id,
                agent_session_id,
                provider_kind,
                provider_session_id,
                runtime_generation,
                action_epoch,
                turn_id,
                question_id,
                approval_id,
                action,
                wait,
                ..
            } => {
                if *operation_id != expected_operation_id {
                    return Err(StoreError::Corruption);
                }
                planned.push(PlannedEffect {
                    document: crate::kernel::outbox::PlannedEffectDocument::new(
                        Effect::DeliverProviderInput {
                            task_id: scope,
                            operation_id: *operation_id,
                            command_id: *command_id,
                            client_id: *client_id,
                            agent_session_id: *agent_session_id,
                            provider_kind: provider_kind.clone(),
                            provider_session_id: provider_session_id.clone(),
                            runtime_generation: *runtime_generation,
                            action_epoch: *action_epoch,
                            turn_id: *turn_id,
                            question_id: *question_id,
                            approval_id: *approval_id,
                            action: action.clone(),
                            wait: *wait,
                        },
                        crate::kernel::outbox::ReplayPolicy::NoAutomaticRetry,
                    ),
                    fence: OperationFence {
                        action_epoch: Some(*action_epoch),
                        resource_id: None,
                        runtime_generation: None,
                    },
                });
            }
            _ => return Err(StoreError::Corruption),
        }
    }
    Ok(planned)
}

/// Replay task mutations through `through_sequence` and return the resulting action_epoch.
fn historical_action_epoch_through(
    tx: &Connection,
    scope: TaskId,
    through_sequence: u64,
) -> Result<u64, StoreError> {
    let mut stmt = tx.prepare(
        "SELECT sequence, event_id, task_revision, event_type, schema_version, payload,
                occurred_at_ms
         FROM events
         WHERE task_id = ?1 AND sequence <= ?2
         ORDER BY sequence ASC",
    )?;
    let rows = stmt.query_map(
        rusqlite::params![
            scope.as_bytes().as_slice(),
            u64_to_sqlite_i64("events.sequence", through_sequence)?
        ],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, Option<i64>>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, Vec<u8>>(5)?,
                row.get::<_, i64>(6)?,
            ))
        },
    )?;
    let mut snapshot: Option<TaskSnapshot> = None;
    for row in rows {
        let (
            sequence_i64,
            event_id_bytes,
            task_revision,
            event_type,
            schema_version,
            payload,
            occurred_at_ms,
        ) = row?;
        let sequence = u64_from_nonnegative_i64("events.sequence", sequence_i64)?;
        let event_id = id16::<EventId>("events.event_id", &event_id_bytes)?;
        let decoded =
            crate::kernel::store::decode_stored_event(&event_type, schema_version, &payload)?;
        if !decoded.is_task_mutation() {
            continue;
        }
        let Some(rev_i64) = task_revision else {
            return Err(StoreError::Corruption);
        };
        let revision = u64_from_nonnegative_i64("events.task_revision", rev_i64)?;
        let domain = DomainEvent {
            id: event_id,
            task_id: Some(scope),
            sequence,
            task_revision: Some(revision),
            occurred_at_ms,
            payload: decoded,
        };
        snapshot = Some(apply_domain_event(snapshot, &domain).map_err(|_| StoreError::Corruption)?);
    }
    let Some(snap) = snapshot else {
        return Err(StoreError::Corruption);
    };
    Ok(snap.task.action_epoch)
}

fn validate_accepted_fact_row(
    accepted_row: &EventRow,
    command_id: CommandId,
    expected_operation_id: OperationId,
    scope: Option<TaskId>,
    accepted_at_ms: i64,
    fence: OperationFence,
) -> Result<(), StoreError> {
    if accepted_row.task_id != scope || accepted_row.task_revision.is_some() {
        return Err(StoreError::Corruption);
    }
    let accepted_event = crate::kernel::store::decode_stored_event(
        &accepted_row.event_type,
        accepted_row.schema_version,
        &accepted_row.payload,
    )?;
    let Event::OperationAccepted(accepted_fact) = accepted_event else {
        return Err(StoreError::Corruption);
    };
    let expected_resource = fence.resource_id;
    let expected_generation = fence.runtime_generation;
    if accepted_fact.command_id != command_id
        || accepted_fact.operation_id != expected_operation_id
        || accepted_fact.action_epoch != fence.action_epoch
        || accepted_fact.resource_id != expected_resource
        || accepted_fact.runtime_generation != expected_generation
        || accepted_fact.accepted_at_ms != accepted_at_ms
        || accepted_row.occurred_at_ms != accepted_at_ms
        || accepted_row.occurred_at_ms != accepted_fact.accepted_at_ms
    {
        return Err(StoreError::Corruption);
    }
    Ok(())
}

/// V1 stores `operation_id` only in the accepted payload. Scan every
/// `operation.accepted` row and require exactly one matching fact for this
/// operation; extras in any task scope/sequence are Corruption.
/// `scope` is `Some(task)` for task operations and `None` for global host admission.
fn ensure_unique_operation_accepted_fact(
    tx: &Connection,
    command_id: CommandId,
    expected_operation_id: OperationId,
    scope: Option<TaskId>,
    accepted_at_ms: i64,
    expected_sequence: u64,
    expected_row: &EventRow,
    fence: OperationFence,
) -> Result<(), StoreError> {
    let mut stmt = tx.prepare(
        "SELECT sequence, event_id, task_id, task_revision, schema_version, payload, occurred_at_ms
         FROM events
         WHERE event_type = 'operation.accepted'
         ORDER BY sequence ASC",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, Vec<u8>>(1)?,
            row.get::<_, Option<Vec<u8>>>(2)?,
            row.get::<_, Option<i64>>(3)?,
            row.get::<_, i64>(4)?,
            row.get::<_, Vec<u8>>(5)?,
            row.get::<_, i64>(6)?,
        ))
    })?;

    let mut match_count = 0u64;
    for row in rows {
        let (
            sequence_i64,
            event_id_bytes,
            task_bytes,
            task_revision,
            schema_version,
            payload,
            occurred_at_ms,
        ) = row?;
        let sequence = u64_from_nonnegative_i64("events.sequence", sequence_i64)?;
        let event_id = id16::<EventId>("events.event_id", &event_id_bytes)?;
        let task_id = parse_optional_task_scope("events.task_id", task_bytes)?;
        let task_revision = match task_revision {
            Some(v) => Some(u64_from_nonnegative_i64("events.task_revision", v)?),
            None => None,
        };
        let decoded = crate::kernel::store::decode_stored_event(
            "operation.accepted",
            schema_version,
            &payload,
        )?;
        let Event::OperationAccepted(fact) = decoded else {
            return Err(StoreError::Corruption);
        };
        let cmd_match = fact.command_id == command_id;
        let op_match = fact.operation_id == expected_operation_id;
        if !cmd_match && !op_match {
            continue;
        }
        if !(cmd_match && op_match) {
            return Err(StoreError::Corruption);
        }
        match_count = match_count.checked_add(1).ok_or(StoreError::Corruption)?;
        let candidate = EventRow {
            event_id,
            task_id,
            task_revision,
            event_type: "operation.accepted".to_string(),
            schema_version,
            payload,
            occurred_at_ms,
        };
        if sequence != expected_sequence
            || candidate.event_id != expected_row.event_id
            || candidate.task_id != expected_row.task_id
            || candidate.task_revision != expected_row.task_revision
            || candidate.occurred_at_ms != expected_row.occurred_at_ms
            || candidate.schema_version != expected_row.schema_version
            || candidate.payload != expected_row.payload
        {
            return Err(StoreError::Corruption);
        }
        validate_accepted_fact_row(
            &candidate,
            command_id,
            expected_operation_id,
            scope,
            accepted_at_ms,
            fence,
        )?;
    }
    if match_count != 1 {
        return Err(StoreError::Corruption);
    }
    Ok(())
}

/// Require exactly one global `host.close_begun` for this singleton admission,
/// at the exact expected sequence, event id, scope, time, payload, fence, and inspection.
fn ensure_unique_host_close_begun_fact(
    tx: &Connection,
    expected_operation_id: OperationId,
    expected_action_epoch: u64,
    expected_inspection_id: u64,
    expected_sequence: u64,
    expected_row: &EventRow,
    expected_occurred_at_ms: i64,
) -> Result<(), StoreError> {
    let mut stmt = tx.prepare(
        "SELECT sequence, event_id, task_id, task_revision, schema_version, payload, occurred_at_ms
         FROM events
         WHERE event_type = 'host.close_begun'
         ORDER BY sequence ASC",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, Vec<u8>>(1)?,
            row.get::<_, Option<Vec<u8>>>(2)?,
            row.get::<_, Option<i64>>(3)?,
            row.get::<_, i64>(4)?,
            row.get::<_, Vec<u8>>(5)?,
            row.get::<_, i64>(6)?,
        ))
    })?;

    let mut match_count = 0u64;
    for row in rows {
        let (
            sequence_i64,
            event_id_bytes,
            task_bytes,
            task_revision,
            schema_version,
            payload,
            occurred_at_ms,
        ) = row?;
        let sequence = u64_from_nonnegative_i64("events.sequence", sequence_i64)?;
        let event_id = id16::<EventId>("events.event_id", &event_id_bytes)?;
        let task_id = parse_optional_task_scope("events.task_id", task_bytes)?;
        let task_revision = match task_revision {
            Some(v) => Some(u64_from_nonnegative_i64("events.task_revision", v)?),
            None => None,
        };
        let decoded = crate::kernel::store::decode_stored_event(
            "host.close_begun",
            schema_version,
            &payload,
        )?;
        let Event::HostCloseBegun {
            operation_id,
            action_epoch,
            inspection_id,
        } = decoded
        else {
            return Err(StoreError::Corruption);
        };
        if operation_id != expected_operation_id {
            // A second close_begun for a different operation is still a singleton violation
            // once Closing exists; treat any extra host.close_begun as Corruption.
            return Err(StoreError::Corruption);
        }
        match_count = match_count.checked_add(1).ok_or(StoreError::Corruption)?;
        if sequence != expected_sequence
            || event_id != expected_row.event_id
            || task_id != expected_row.task_id
            || task_revision != expected_row.task_revision
            || occurred_at_ms != expected_row.occurred_at_ms
            || schema_version != expected_row.schema_version
            || payload != expected_row.payload
            || action_epoch != expected_action_epoch
            || inspection_id != expected_inspection_id
            || occurred_at_ms != expected_occurred_at_ms
            || task_id.is_some()
            || task_revision.is_some()
        {
            return Err(StoreError::Corruption);
        }
    }
    if match_count != 1 {
        return Err(StoreError::Corruption);
    }
    Ok(())
}

fn validate_decision_event_batch(
    tx: &Connection,
    event_ids: &[EventId],
    scope: TaskId,
    receipt_final_revision: u64,
    first_decision_sequence: u64,
    accepted_at_ms: i64,
    is_allowed: fn(&Event) -> bool,
) -> Result<Vec<(Event, u64)>, StoreError> {
    let mut decision_facts = Vec::with_capacity(event_ids.len());
    let mut previous_revision: Option<u64> = None;
    for (offset, expected_event_id) in event_ids.iter().enumerate() {
        let offset_u64 = u64::try_from(offset).map_err(|_| StoreError::Corruption)?;
        let expected_sequence = first_decision_sequence
            .checked_add(offset_u64)
            .ok_or(StoreError::Corruption)?;
        let row = load_event_row_at_sequence(tx, expected_sequence)?;
        if row.event_id != *expected_event_id {
            return Err(StoreError::Corruption);
        }
        if row.task_id != Some(scope) {
            return Err(StoreError::Corruption);
        }
        if row.occurred_at_ms != accepted_at_ms {
            return Err(StoreError::Corruption);
        }
        let decoded = crate::kernel::store::decode_stored_event(
            &row.event_type,
            row.schema_version,
            &row.payload,
        )?;
        if !is_allowed(&decoded) {
            return Err(StoreError::Corruption);
        }
        validate_decision_fact_ownership(tx, &decoded, scope)?;
        let Some(task_revision) = row.task_revision else {
            return Err(StoreError::Corruption);
        };
        ensure_unique_task_revision(tx, scope, task_revision, row.event_id)?;
        match previous_revision {
            None => previous_revision = Some(task_revision),
            Some(prev) => {
                let expected = prev.checked_add(1).ok_or(StoreError::IntegerOutOfRange {
                    field: "events.task_revision",
                    value: u64::MAX,
                })?;
                if task_revision != expected {
                    return Err(StoreError::Corruption);
                }
                previous_revision = Some(task_revision);
            }
        }
        decision_facts.push((decoded, task_revision));
    }
    if previous_revision != Some(receipt_final_revision) {
        return Err(StoreError::Corruption);
    }

    let is_create_batch = decision_facts
        .iter()
        .any(|(fact, _)| matches!(fact, Event::TaskCreated { .. }));
    if is_create_batch {
        if decision_facts.len() != 1 {
            return Err(StoreError::Corruption);
        }
        let (Event::TaskCreated { task, .. }, revision) = &decision_facts[0] else {
            return Err(StoreError::Corruption);
        };
        if *revision != 1 || task.revision != 1 || receipt_final_revision != 1 {
            return Err(StoreError::Corruption);
        }
        let prior_mutations: i64 = tx.query_row(
            "SELECT COUNT(*) FROM events
             WHERE task_id = ?1
               AND task_revision IS NOT NULL
               AND sequence < ?2",
            rusqlite::params![
                scope.as_bytes().as_slice(),
                u64_to_sqlite_i64("events.sequence", first_decision_sequence)?
            ],
            |row| row.get(0),
        )?;
        if prior_mutations != 0 {
            return Err(StoreError::Corruption);
        }
    } else {
        let prior = load_latest_prior_task_mutation(tx, scope, first_decision_sequence)?;
        let expected_first = prior.checked_add(1).ok_or(StoreError::IntegerOutOfRange {
            field: "events.task_revision",
            value: u64::MAX,
        })?;
        let first_revision = decision_facts[0].1;
        if first_revision != expected_first {
            return Err(StoreError::Corruption);
        }
    }

    Ok(decision_facts)
}

fn validate_decision_fact_ownership(
    tx: &Connection,
    event: &Event,
    scope: TaskId,
) -> Result<(), StoreError> {
    match event {
        Event::TaskCreated { task, .. } => {
            if task.id != scope {
                return Err(StoreError::Corruption);
            }
        }
        Event::AgentSessionRegistered { agent } => {
            if agent.task_id != scope {
                return Err(StoreError::Corruption);
            }
        }
        Event::PrimaryAgentSet { agent_session_id } => {
            validate_primary_agent_set_ownership(tx, *agent_session_id, scope)?;
        }
        Event::ArtifactRegistered { artifact } => {
            if artifact.task_id != scope {
                return Err(StoreError::Corruption);
            }
        }
        Event::ResourceRegistered { resource } => {
            if resource.owner_kind != OwnerKind::Task || resource.task_id != Some(scope) {
                return Err(StoreError::Corruption);
            }
        }
        _ => {}
    }
    Ok(())
}

fn validate_primary_agent_set_ownership(
    tx: &Connection,
    agent_session_id: AgentSessionId,
    scope: TaskId,
) -> Result<(), StoreError> {
    let row: Option<(Vec<u8>, Vec<u8>)> = tx
        .query_row(
            "SELECT task_id, role FROM agent_sessions WHERE agent_session_id = ?1",
            [agent_session_id.as_bytes().as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((task_id_bytes, role_bytes)) = row else {
        return Err(StoreError::Corruption);
    };
    let agent_task_id = id16::<TaskId>("agent_sessions.task_id", &task_id_bytes)?;
    if agent_task_id != scope {
        return Err(StoreError::Corruption);
    }
    let role: AgentRole = unpack_projection_blob("agent_sessions.role", &role_bytes)?;
    if role != AgentRole::Primary {
        return Err(StoreError::Corruption);
    }
    Ok(())
}

fn ensure_unique_task_revision(
    tx: &Connection,
    scope: TaskId,
    task_revision: u64,
    event_id: EventId,
) -> Result<(), StoreError> {
    let row: Option<Vec<u8>> = tx
        .query_row(
            "SELECT event_id FROM events
             WHERE task_id = ?1 AND task_revision = ?2",
            rusqlite::params![
                scope.as_bytes().as_slice(),
                u64_to_sqlite_i64("events.task_revision", task_revision)?
            ],
            |row| row.get(0),
        )
        .optional()?;
    let Some(found) = row else {
        return Err(StoreError::Corruption);
    };
    let found_id = id16::<EventId>("events.event_id", &found)?;
    if found_id != event_id {
        return Err(StoreError::Corruption);
    }
    let count: i64 = tx.query_row(
        "SELECT COUNT(*) FROM events
         WHERE task_id = ?1 AND task_revision = ?2",
        rusqlite::params![
            scope.as_bytes().as_slice(),
            u64_to_sqlite_i64("events.task_revision", task_revision)?
        ],
        |row| row.get(0),
    )?;
    if count != 1 {
        return Err(StoreError::Corruption);
    }
    Ok(())
}

fn load_latest_prior_task_mutation(
    tx: &Connection,
    scope: TaskId,
    before_sequence: u64,
) -> Result<u64, StoreError> {
    let row: Option<(i64, String, i64, Vec<u8>, i64)> = tx
        .query_row(
            "SELECT sequence, event_type, schema_version, payload, task_revision
             FROM events
             WHERE task_id = ?1
               AND task_revision IS NOT NULL
               AND sequence < ?2
             ORDER BY sequence DESC
             LIMIT 1",
            rusqlite::params![
                scope.as_bytes().as_slice(),
                u64_to_sqlite_i64("events.sequence", before_sequence)?
            ],
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
    let Some((_sequence, event_type, schema_version, payload, task_revision)) = row else {
        return Err(StoreError::Corruption);
    };
    let decoded = crate::kernel::store::decode_stored_event(&event_type, schema_version, &payload)?;
    if !decoded.is_task_mutation() {
        return Err(StoreError::Corruption);
    }
    u64_from_nonnegative_i64("events.task_revision", task_revision)
}

struct OperationProjectionRow {
    operation_id: OperationId,
    task_id: Option<TaskId>,
    state: String,
    action_epoch: Option<i64>,
    resource_id: Option<Vec<u8>>,
    runtime_generation: Option<i64>,
    result: Option<Vec<u8>>,
    outcome_code: Option<String>,
    accepted_at_ms: i64,
    outcome_at_ms: Option<i64>,
}

fn load_operation_projection(
    tx: &Connection,
    command_id: CommandId,
) -> Result<OperationProjectionRow, StoreError> {
    let row: Option<(
        Vec<u8>,
        Option<Vec<u8>>,
        String,
        Option<i64>,
        Option<Vec<u8>>,
        Option<i64>,
        Option<Vec<u8>>,
        Option<String>,
        i64,
        Option<i64>,
    )> = tx
        .query_row(
            "SELECT operation_id, task_id, state, action_epoch, resource_id, runtime_generation,
                    result, outcome_code, accepted_at_ms, outcome_at_ms
             FROM operations WHERE command_id = ?1",
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
                    row.get(8)?,
                    row.get(9)?,
                ))
            },
        )
        .optional()?;
    let Some((
        operation_id_bytes,
        task_bytes,
        state,
        action_epoch,
        resource_id,
        runtime_generation,
        result,
        outcome_code,
        accepted_at_ms,
        outcome_at_ms,
    )) = row
    else {
        return Err(StoreError::Corruption);
    };
    Ok(OperationProjectionRow {
        operation_id: id16::<OperationId>("operations.operation_id", &operation_id_bytes)?,
        task_id: parse_optional_task_scope("operations.task_id", task_bytes)?,
        state,
        action_epoch,
        resource_id,
        runtime_generation,
        result,
        outcome_code,
        accepted_at_ms,
        outcome_at_ms,
    })
}

struct EventRow {
    event_id: EventId,
    task_id: Option<TaskId>,
    task_revision: Option<u64>,
    event_type: String,
    schema_version: i64,
    payload: Vec<u8>,
    occurred_at_ms: i64,
}

pub(crate) struct OutboxRow {
    pub(crate) outbox_id: OutboxId,
    pub(crate) operation_id: OperationId,
    pub(crate) effect_index: i64,
    pub(crate) event_sequence: u64,
    pub(crate) destination_class: String,
    pub(crate) replay_policy: String,
    pub(crate) payload: Vec<u8>,
    pub(crate) state: String,
    pub(crate) available_at_ms: i64,
    pub(crate) leased_until_ms: Option<i64>,
    pub(crate) dispatch_started_at_ms: Option<i64>,
    pub(crate) attempts: i64,
    pub(crate) last_error_class: Option<String>,
    pub(crate) lease_generation: i64,
    /// V2 column reserved for e2; must remain NULL and unused in e1.
    #[allow(dead_code)]
    pub(crate) reconciliation_receipt: Option<Vec<u8>>,
    /// Present only after intentional cleanup of an eligible terminal payload.
    pub(crate) compacted_payload_sha256: Option<Vec<u8>>,
}

fn load_outbox_rows(
    tx: &Connection,
    operation_id: OperationId,
) -> Result<Vec<OutboxRow>, StoreError> {
    let mut stmt = tx.prepare(
        "SELECT outbox_id, operation_id, effect_index, event_sequence, destination_class,
                replay_policy, payload, state, available_at_ms, leased_until_ms,
                dispatch_started_at_ms, attempts, last_error_class, lease_generation,
                reconciliation_receipt, compacted_payload_sha256
         FROM outbox
         WHERE operation_id = ?1
         ORDER BY effect_index ASC",
    )?;
    let rows = stmt.query_map([operation_id.as_bytes().as_slice()], |row| {
        Ok((
            row.get::<_, Vec<u8>>(0)?,
            row.get::<_, Vec<u8>>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, Vec<u8>>(6)?,
            row.get::<_, String>(7)?,
            row.get::<_, i64>(8)?,
            row.get::<_, Option<i64>>(9)?,
            row.get::<_, Option<i64>>(10)?,
            row.get::<_, i64>(11)?,
            row.get::<_, Option<String>>(12)?,
            row.get::<_, i64>(13)?,
            row.get::<_, Option<Vec<u8>>>(14)?,
            row.get::<_, Option<Vec<u8>>>(15)?,
        ))
    })?;
    let mut out = Vec::new();
    let mut expected_index = 0i64;
    for row in rows {
        let (
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
        ) = row?;
        if effect_index != expected_index {
            return Err(StoreError::Corruption);
        }
        expected_index = expected_index
            .checked_add(1)
            .ok_or(StoreError::Corruption)?;
        if event_sequence < 0 {
            return Err(StoreError::Corruption);
        }
        if lease_generation < 0 {
            return Err(StoreError::Corruption);
        }
        out.push(OutboxRow {
            outbox_id: id16::<OutboxId>("outbox.outbox_id", &outbox_id_bytes)?,
            operation_id: id16::<OperationId>("outbox.operation_id", &operation_id_bytes)?,
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
        });
    }
    Ok(out)
}

pub(crate) fn load_outbox_row_by_id(
    tx: &Transaction<'_>,
    outbox_id: OutboxId,
) -> Result<Option<OutboxRow>, StoreError> {
    let row: Option<(
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
            "SELECT operation_id, effect_index, event_sequence, destination_class,
                    replay_policy, payload, state, available_at_ms, leased_until_ms,
                    dispatch_started_at_ms, attempts, last_error_class, lease_generation,
                    reconciliation_receipt, compacted_payload_sha256
             FROM outbox WHERE outbox_id = ?1",
            [outbox_id.as_bytes().as_slice()],
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
                ))
            },
        )
        .optional()?;
    let Some((
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
        outbox_id,
        operation_id: id16::<OperationId>("outbox.operation_id", &operation_id_bytes)?,
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

fn load_event_row_at_sequence(tx: &Connection, sequence: u64) -> Result<EventRow, StoreError> {
    let row: Option<(
        Vec<u8>,
        Option<Vec<u8>>,
        Option<i64>,
        String,
        i64,
        Vec<u8>,
        i64,
    )> = tx
        .query_row(
            "SELECT event_id, task_id, task_revision, event_type, schema_version, payload,
                    occurred_at_ms
             FROM events WHERE sequence = ?1",
            [u64_to_sqlite_i64("events.sequence", sequence)?],
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
    let Some((
        event_id_bytes,
        task_bytes,
        task_revision,
        event_type,
        schema_version,
        payload,
        occurred_at_ms,
    )) = row
    else {
        return Err(StoreError::Corruption);
    };
    Ok(EventRow {
        event_id: id16::<EventId>("events.event_id", &event_id_bytes)?,
        task_id: parse_optional_task_scope("events.task_id", task_bytes)?,
        task_revision: match task_revision {
            Some(v) => Some(u64_from_nonnegative_i64("events.task_revision", v)?),
            None => None,
        },
        event_type,
        schema_version,
        payload,
        occurred_at_ms,
    })
}

fn validate_rejected_receipt_correlation(
    tx: &Connection,
    command_id: CommandId,
    committed_sequence: Option<i64>,
) -> Result<(), StoreError> {
    if committed_sequence.is_some() {
        return Err(StoreError::Corruption);
    }
    let operation_count: i64 = tx.query_row(
        "SELECT COUNT(*) FROM operations WHERE command_id = ?1",
        [command_id.as_bytes().as_slice()],
        |row| row.get(0),
    )?;
    if operation_count != 0 {
        return Err(StoreError::Corruption);
    }
    Ok(())
}

fn parse_optional_task_scope(
    field: &'static str,
    bytes: Option<Vec<u8>>,
) -> Result<Option<TaskId>, StoreError> {
    match bytes {
        None => Ok(None),
        Some(bytes) => Ok(Some(id16::<TaskId>(field, &bytes)?)),
    }
}

fn effective_task_scope(envelope: &CommandEnvelope) -> Option<TaskId> {
    match &envelope.command {
        Command::CreateTask(intent) => Some(intent.id),
        Command::CreateTaskV2(intent) => Some(intent.id),
        _ => envelope.task_id,
    }
}

fn command_is_effectful(command: &Command) -> bool {
    matches!(
        command,
        Command::BeginCloseTask | Command::ReleaseResource { .. }
    )
}

fn persist_rejection(
    tx: &Transaction<'_>,
    envelope: &CommandEnvelope,
    effective_task_id: Option<TaskId>,
    code: RejectionCode,
    current_revision: Option<u64>,
    created_at_ms: i64,
    scope: ReceiptScope,
) -> Result<CommandReceipt, StoreError> {
    persist_rejection_with_resolution(
        tx,
        envelope,
        effective_task_id,
        code,
        current_revision,
        created_at_ms,
        None,
        scope,
    )
}

fn persist_rejection_with_resolution(
    tx: &Transaction<'_>,
    envelope: &CommandEnvelope,
    effective_task_id: Option<TaskId>,
    code: RejectionCode,
    current_revision: Option<u64>,
    created_at_ms: i64,
    resolution: Option<crate::domain::ProviderResolutionWinner>,
    scope: ReceiptScope,
) -> Result<CommandReceipt, StoreError> {
    let receipt = CommandReceipt::Rejected {
        command_id: envelope.command_id,
        code,
        current_revision,
        resolution,
    };
    insert_receipt_row(
        tx,
        envelope,
        effective_task_id,
        &receipt,
        None,
        created_at_ms,
        scope,
    )?;
    Ok(receipt)
}

fn provider_resolution_for_rejection(
    snapshot: Option<&TaskSnapshot>,
    envelope: &CommandEnvelope,
) -> Option<crate::domain::ProviderResolutionWinner> {
    let snapshot = snapshot?;
    let Command::SubmitProviderInput(intent) = &envelope.command else {
        return None;
    };
    let session = snapshot.provider_sessions.get(&intent.agent_session_id())?;
    match intent.action() {
        crate::domain::ProviderInputAction::AnswerQuestion { question_id, .. } => {
            session.question_winners.get(question_id).copied()
        }
        crate::domain::ProviderInputAction::ResolveApproval { approval_id, .. } => {
            session.approval_winners.get(approval_id).copied()
        }
        _ => None,
    }
}

/// Commits pure decision facts plus OperationAccepted/Settled in one transaction.
/// Provider input is deliberately excluded: it always requires a concrete
/// NoAutomaticRetry outbox effect and therefore must use the side-effect path.
fn persist_pure_acceptance(
    tx: &Transaction<'_>,
    envelope: &CommandEnvelope,
    effective_task_id: Option<TaskId>,
    snapshot: Option<&TaskSnapshot>,
    decision: Vec<Event>,
    accepted_at_ms: i64,
    scope: ReceiptScope,
) -> Result<CommandReceipt, StoreError> {
    if decision
        .iter()
        .any(|event| matches!(event, Event::ProviderInputAccepted { .. }))
    {
        return Err(StoreError::Projection(
            "provider input acceptance requires a durable outbox effect".into(),
        ));
    }
    let operation_id = OperationId::new();
    let decision_event_ids: Vec<EventId> = (0..decision.len()).map(|_| EventId::new()).collect();
    let accepted_event_id = EventId::new();
    let settled_event_id = EventId::new();

    let mut next_revision = snapshot.map(|snap| snap.task.revision);
    let mut decision_revisions = Vec::with_capacity(decision.len());
    for payload in &decision {
        if payload.is_task_mutation() {
            let revision = match next_revision {
                None => 1u64,
                Some(current) => current
                    .checked_add(1)
                    .ok_or(StoreError::IntegerOutOfRange {
                        field: "tasks.revision",
                        value: u64::MAX,
                    })?,
            };
            next_revision = Some(revision);
            decision_revisions.push(Some(revision));
        } else {
            decision_revisions.push(next_revision);
        }
    }
    let final_task_revision = next_revision;

    let receipt = CommandReceipt::Accepted {
        command_id: envelope.command_id,
        operation_id,
        task_revision: final_task_revision,
        event_ids: decision_event_ids.clone(),
        prompt_mutation: None,
    };
    insert_receipt_row(
        tx,
        envelope,
        effective_task_id,
        &receipt,
        None,
        accepted_at_ms,
        scope,
    )?;

    for ((payload, event_id), task_revision) in decision
        .into_iter()
        .zip(decision_event_ids.iter().copied())
        .zip(decision_revisions)
    {
        append_and_project(
            tx,
            event_id,
            effective_task_id,
            task_revision,
            accepted_at_ms,
            payload,
        )?;
    }

    let accepted = OperationAcceptedFact::new(
        envelope.command_id,
        operation_id,
        accepted_at_ms,
        None,
        None,
        None,
    )
    .map_err(|err| StoreError::Projection(err.to_string()))?;
    append_and_project(
        tx,
        accepted_event_id,
        effective_task_id,
        None,
        accepted_at_ms,
        Event::OperationAccepted(accepted),
    )?;

    let settled = OperationSettledFact::new(
        envelope.command_id,
        operation_id,
        accepted_at_ms,
        decision_event_ids,
        None,
        None,
        None,
    )
    .map_err(|err| StoreError::Projection(err.to_string()))?;
    let committed_sequence = append_and_project(
        tx,
        settled_event_id,
        effective_task_id,
        None,
        accepted_at_ms,
        Event::OperationSettled(settled),
    )?;

    set_committed_sequence(tx, envelope.command_id, committed_sequence)?;
    Ok(receipt)
}

fn persist_side_effect_acceptance(
    tx: &Transaction<'_>,
    envelope: &CommandEnvelope,
    task_id: TaskId,
    snapshot: Option<&TaskSnapshot>,
    decision: Vec<Event>,
    planned: Vec<PlannedEffect>,
    accepted_at_ms: i64,
    scope: ReceiptScope,
) -> Result<CommandReceipt, StoreError> {
    if planned.is_empty() {
        return Err(StoreError::Projection(
            "side-effect acceptance requires a non-empty plan".into(),
        ));
    }
    let fence = planned[0].fence;
    for effect in &planned[1..] {
        if effect.fence != fence {
            return Err(StoreError::Projection(
                "planned effects disagree on accepted operation fence".into(),
            ));
        }
    }

    let operation_id = decision
        .iter()
        .find_map(|event| match event {
            Event::ProviderInputAccepted { operation_id, .. } => Some(*operation_id),
            _ => None,
        })
        .unwrap_or_else(OperationId::new);
    for planned_effect in &planned {
        if let Effect::DeliverProviderInput {
            operation_id: effect_operation,
            ..
        } = &planned_effect.document.effect
        {
            if *effect_operation != operation_id {
                return Err(StoreError::Projection(
                    "provider input effect operation identity disagrees with accepted operation"
                        .into(),
                ));
            }
        }
    }
    let decision_event_ids: Vec<EventId> = (0..decision.len()).map(|_| EventId::new()).collect();
    let accepted_event_id = EventId::new();
    let outbox_ids: Vec<OutboxId> = (0..planned.len()).map(|_| OutboxId::new()).collect();

    let mut next_revision = snapshot.map(|snap| snap.task.revision);
    let mut decision_revisions = Vec::with_capacity(decision.len());
    for payload in &decision {
        if payload.is_task_mutation() {
            let revision = match next_revision {
                None => 1u64,
                Some(current) => current
                    .checked_add(1)
                    .ok_or(StoreError::IntegerOutOfRange {
                        field: "tasks.revision",
                        value: u64::MAX,
                    })?,
            };
            next_revision = Some(revision);
            decision_revisions.push(Some(revision));
        } else {
            decision_revisions.push(next_revision);
        }
    }
    let final_task_revision = next_revision;

    let receipt = CommandReceipt::Accepted {
        command_id: envelope.command_id,
        operation_id,
        task_revision: final_task_revision,
        event_ids: decision_event_ids.clone(),
        prompt_mutation: None,
    };
    insert_receipt_row(
        tx,
        envelope,
        Some(task_id),
        &receipt,
        None,
        accepted_at_ms,
        scope,
    )?;

    for ((payload, event_id), task_revision) in decision
        .into_iter()
        .zip(decision_event_ids.iter().copied())
        .zip(decision_revisions)
    {
        append_and_project(
            tx,
            event_id,
            Some(task_id),
            task_revision,
            accepted_at_ms,
            payload,
        )?;
    }

    let accepted = OperationAcceptedFact::new(
        envelope.command_id,
        operation_id,
        accepted_at_ms,
        fence.action_epoch,
        fence.resource_id,
        fence.runtime_generation,
    )
    .map_err(|err| StoreError::Projection(err.to_string()))?;
    let committed_sequence = append_and_project(
        tx,
        accepted_event_id,
        Some(task_id),
        None,
        accepted_at_ms,
        Event::OperationAccepted(accepted),
    )?;

    for (index, (planned_effect, outbox_id)) in planned.into_iter().zip(outbox_ids).enumerate() {
        let effect_index = i64::try_from(index).map_err(|_| StoreError::IntegerOutOfRange {
            field: "outbox.effect_index",
            value: u64::MAX,
        })?;
        let payload = encode_effect_document(&planned_effect.document)?;
        tx.execute(
            "INSERT INTO outbox(
                outbox_id, operation_id, effect_index, event_sequence, destination_class,
                replay_policy, payload, state, available_at_ms, leased_until_ms,
                dispatch_started_at_ms, attempts, last_error_class
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'pending', ?8, NULL, NULL, 0, NULL)",
            rusqlite::params![
                outbox_id.as_bytes().as_slice(),
                operation_id.as_bytes().as_slice(),
                effect_index,
                u64_to_sqlite_i64("outbox.event_sequence", committed_sequence)?,
                planned_effect.document.destination_class.as_str(),
                planned_effect.document.replay_policy.as_str(),
                payload,
                accepted_at_ms,
            ],
        )?;
    }

    set_committed_sequence(tx, envelope.command_id, committed_sequence)?;
    Ok(receipt)
}

fn insert_receipt_row(
    tx: &Transaction<'_>,
    envelope: &CommandEnvelope,
    effective_task_id: Option<TaskId>,
    receipt: &CommandReceipt,
    committed_sequence: Option<u64>,
    created_at_ms: i64,
    scope: ReceiptScope,
) -> Result<(), StoreError> {
    let payload = encode_receipt_document(receipt)?;
    let fingerprint = command_fingerprint(envelope)?;
    let payload_digest = crate::domain::command::command_payload_digest(envelope)
        .map_err(|detail| StoreError::CodecMismatch { detail })?;
    tx.execute(
        "INSERT INTO command_receipts(
            command_id, client_id, task_id, receipt, committed_sequence, created_at_ms,
            connection_id, request_id, action_epoch, runtime_generation,
            command_fingerprint, payload_digest
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        rusqlite::params![
            envelope.command_id.as_bytes().as_slice(),
            envelope.client_id.as_bytes().as_slice(),
            effective_task_id.map(|id| id.as_bytes().as_slice().to_vec()),
            payload,
            match committed_sequence {
                Some(seq) => Some(u64_to_sqlite_i64(
                    "command_receipts.committed_sequence",
                    seq
                )?),
                None => None,
            },
            created_at_ms,
            scope.connection_id.map(|id| id.as_bytes().to_vec()),
            scope.request_id.map(|id| id.as_bytes().to_vec()),
            match scope.action_epoch {
                Some(epoch) => Some(u64_to_sqlite_i64("command_receipts.action_epoch", epoch)?),
                None => None,
            },
            match scope.runtime_generation {
                Some(generation) => Some(u64_to_sqlite_i64(
                    "command_receipts.runtime_generation",
                    generation,
                )?),
                None => None,
            },
            fingerprint.to_vec(),
            payload_digest.to_vec(),
        ],
    )?;
    trim_receipt_ledger(tx, envelope.command_id)?;
    Ok(())
}

fn trim_receipt_ledger(tx: &Transaction<'_>, newest: CommandId) -> Result<(), StoreError> {
    let count: i64 = tx.query_row("SELECT COUNT(*) FROM command_receipts", [], |row| {
        row.get(0)
    })?;
    let excess = count.saturating_sub(MAX_RECEIPT_ROWS);
    if excess <= 0 {
        return Ok(());
    }
    let mut stmt = tx.prepare(
        "SELECT command_id FROM command_receipts
         WHERE command_id <> ?1
           AND NOT EXISTS (SELECT 1 FROM operations WHERE operations.command_id = command_receipts.command_id)
         ORDER BY created_at_ms ASC, command_id ASC LIMIT ?2",
    )?;
    let ids = stmt
        .query_map(
            rusqlite::params![newest.as_bytes().as_slice(), excess],
            |row| row.get::<_, Vec<u8>>(0),
        )?
        .collect::<Result<Vec<_>, _>>()?;
    drop(stmt);
    if ids.len() < usize::try_from(excess).unwrap_or(usize::MAX) {
        // Every older row is still referenced by an operation. Do not let a
        // durable receipt ledger grow without bound; the surrounding
        // transaction rolls back this new command and the caller can retry
        // after operation retention has released a row.
        return Err(StoreError::Busy);
    }
    for id in ids {
        tx.execute("DELETE FROM command_receipts WHERE command_id = ?1", [id])?;
    }
    Ok(())
}

fn set_committed_sequence(
    tx: &Transaction<'_>,
    command_id: CommandId,
    sequence: u64,
) -> Result<(), StoreError> {
    tx.execute(
        "UPDATE command_receipts SET committed_sequence = ?1 WHERE command_id = ?2",
        rusqlite::params![
            u64_to_sqlite_i64("command_receipts.committed_sequence", sequence)?,
            command_id.as_bytes().as_slice(),
        ],
    )?;
    Ok(())
}

fn append_and_project(
    tx: &Transaction<'_>,
    event_id: EventId,
    task_id: Option<TaskId>,
    task_revision: Option<u64>,
    occurred_at_ms: i64,
    payload: Event,
) -> Result<u64, StoreError> {
    let event_type = payload.event_type();
    let packed = encode_event_payload(&payload)?;
    tx.execute(
        "INSERT INTO events(
            event_id, task_id, task_revision, event_type, schema_version, occurred_at_ms, payload
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            event_id.as_bytes().as_slice(),
            task_id.map(|id| id.as_bytes().as_slice().to_vec()),
            match task_revision {
                Some(rev) => Some(u64_to_sqlite_i64("events.task_revision", rev)?),
                None => None,
            },
            event_type,
            i64::from(EVENT_SCHEMA_VERSION),
            occurred_at_ms,
            packed,
        ],
    )?;
    let sequence_i64: i64 = tx.query_row("SELECT last_insert_rowid()", [], |row| row.get(0))?;
    let sequence = u64_from_nonnegative_i64("events.sequence", sequence_i64)?;
    let domain = DomainEvent {
        id: event_id,
        task_id,
        sequence,
        task_revision,
        occurred_at_ms,
        payload,
    };
    projector::apply_event(tx, &domain, false)?;
    Ok(sequence)
}

pub(crate) fn load_task_snapshot(
    conn: &Connection,
    task_id: TaskId,
) -> Result<Option<TaskSnapshot>, StoreError> {
    let Some(task_row) = load_task_row(conn, task_id).map_err(|error| {
        StoreError::Projection(format!("task {task_id} row is invalid: {error}"))
    })?
    else {
        return Ok(None);
    };

    let agents = load_agents(conn, task_id).map_err(|error| {
        StoreError::Projection(format!(
            "task {task_id} agent projection is invalid: {error}"
        ))
    })?;
    let artifacts = load_artifacts(conn, task_id).map_err(|error| {
        StoreError::Projection(format!(
            "task {task_id} artifact projection is invalid: {error}"
        ))
    })?;
    let resources = load_resources(conn, task_id).map_err(|error| {
        StoreError::Projection(format!(
            "task {task_id} resource projection is invalid: {error}"
        ))
    })?;

    if let Some(primary_id) = task_row.primary_agent_id {
        let Some(agent) = agents.get(&primary_id) else {
            return Err(StoreError::Projection(
                "primary_agent_session_id does not reference a registered agent".into(),
            ));
        };
        if agent.task_id != task_id {
            return Err(StoreError::Projection(
                "primary agent belongs to a different task".into(),
            ));
        }
        if !matches!(agent.role, AgentRole::Primary) {
            return Err(StoreError::Projection(
                "primary agent selection requires Primary role".into(),
            ));
        }
    }

    for agent in agents.values() {
        if agent.task_id != task_id {
            return Err(StoreError::Projection(
                "agent_sessions row task ownership mismatch".into(),
            ));
        }
    }
    for artifact in artifacts.values() {
        if artifact.task_id != task_id {
            return Err(StoreError::Projection(
                "artifacts row task ownership mismatch".into(),
            ));
        }
    }
    for resource in resources.values() {
        match resource.task_id {
            Some(id) if id == task_id => {}
            _ => {
                return Err(StoreError::Projection(
                    "resources row task ownership mismatch".into(),
                ));
            }
        }
    }

    let task_lifecycle = task_row.task.lifecycle;
    Ok(Some(TaskSnapshot {
        task: task_row.task,
        connectivity: task_row.connectivity,
        attention: task_row.attention,
        activity: task_row.activity,
        review_readiness: task_row.review_readiness,
        agents,
        primary_agent_id: task_row.primary_agent_id,
        artifacts,
        resources,
        provider_sessions: load_provider_sessions(conn, task_id).map_err(|error| {
            StoreError::Projection(format!(
                "task {task_id} provider projection is invalid: {error}"
            ))
        })?,
        browser: load_browser_book(conn, task_id, task_lifecycle).map_err(|error| {
            StoreError::Projection(format!(
                "task {task_id} browser projection is invalid: {error}"
            ))
        })?,
    }))
}

fn load_browser_book(
    conn: &Connection,
    task_id: TaskId,
    lifecycle: TaskLifecycle,
) -> Result<crate::domain::browser::BrowserBook, StoreError> {
    let mut book = crate::domain::browser::BrowserBook::new();
    book.open_task(task_id)
        .map_err(|err| StoreError::Projection(err.to_string()))?;
    let mut stmt = conn.prepare(
        "SELECT event_type, schema_version, payload
         FROM events
         WHERE task_id = ?1
         ORDER BY sequence ASC",
    )?;
    let rows = stmt.query_map([task_id.as_bytes().as_slice()], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, Vec<u8>>(2)?,
        ))
    })?;
    for row in rows {
        let (event_type, schema_version, payload) = row?;
        let decoded =
            crate::kernel::store::decode_stored_event(&event_type, schema_version, &payload)?;
        if let Event::Browser(fact) = decoded {
            if fact.task_id() != task_id {
                return Err(StoreError::CodecMismatch {
                    detail: "browser fact task identity disagrees with event scope".into(),
                });
            }
            book.apply_facts(&[fact])
                .map_err(|err| StoreError::Projection(err.to_string()))?;
        }
    }
    if matches!(lifecycle, TaskLifecycle::Closing | TaskLifecycle::Archived) {
        book.close_task(task_id)
            .map_err(|err| StoreError::Projection(err.to_string()))?;
    }
    Ok(book)
}

struct LoadedTaskRow {
    task: TaskFacts,
    connectivity: TaskConnectivity,
    attention: TaskAttention,
    activity: TaskActivity,
    review_readiness: ReviewReadiness,
    primary_agent_id: Option<AgentSessionId>,
}

fn load_task_row(conn: &Connection, task_id: TaskId) -> Result<Option<LoadedTaskRow>, StoreError> {
    let row = conn
        .query_row(
            "SELECT environment_id, project_id, title, description, workspace, assignment,
                    lifecycle, action_epoch, revision, connectivity, attention, activity,
                    review_readiness, primary_agent_session_id, created_at_ms
             FROM tasks WHERE task_id = ?1",
            [task_id.as_bytes().as_slice()],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Vec<u8>>(4)?,
                    row.get::<_, Vec<u8>>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, i64>(8)?,
                    row.get::<_, String>(9)?,
                    row.get::<_, String>(10)?,
                    row.get::<_, String>(11)?,
                    row.get::<_, String>(12)?,
                    row.get::<_, Option<Vec<u8>>>(13)?,
                    row.get::<_, i64>(14)?,
                ))
            },
        )
        .optional()?;
    let Some((
        environment_id,
        project_id,
        title,
        description,
        workspace,
        assignment,
        lifecycle,
        action_epoch,
        revision,
        connectivity,
        attention,
        activity,
        review_readiness,
        primary_agent,
        created_at_ms,
    )) = row
    else {
        return Ok(None);
    };

    let task = TaskFacts {
        id: task_id,
        environment_id: id16::<EnvironmentId>("tasks.environment_id", &environment_id)?,
        title,
        description,
        project_id: id16::<ProjectId>("tasks.project_id", &project_id)?,
        workspace: unpack_projection_blob("tasks.workspace", &workspace)?,
        assignment: unpack_projection_blob("tasks.assignment", &assignment)?,
        lifecycle: parse_lifecycle(&lifecycle)?,
        action_epoch: u64_from_nonnegative_i64("tasks.action_epoch", action_epoch)?,
        revision: u64_from_nonnegative_i64("tasks.revision", revision)?,
        created_at_ms,
    };
    task.validate_content()
        .map_err(|err| StoreError::Projection(err.to_string()))?;

    Ok(Some(LoadedTaskRow {
        task,
        connectivity: parse_connectivity(&connectivity)?,
        attention: parse_attention(&attention)?,
        activity: parse_activity(&activity)?,
        review_readiness: parse_review(&review_readiness)?,
        primary_agent_id: match primary_agent {
            Some(bytes) => Some(id16::<AgentSessionId>(
                "tasks.primary_agent_session_id",
                &bytes,
            )?),
            None => None,
        },
    }))
}

fn load_provider_sessions(
    conn: &Connection,
    task_id: TaskId,
) -> Result<
    BTreeMap<AgentSessionId, crate::domain::provider_input::ProviderSessionProjection>,
    StoreError,
> {
    let mut stmt = conn.prepare(
        "SELECT agent_session_id, state FROM provider_input_state WHERE task_id = ?1
         ORDER BY agent_session_id ASC",
    )?;
    let rows = stmt.query_map([task_id.as_bytes().as_slice()], |row| {
        Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?))
    })?;
    let mut sessions = BTreeMap::new();
    for row in rows {
        let (agent_bytes, state) = row?;
        if state.len() > crate::domain::MAX_PROVIDER_SESSION_STATE_BYTES {
            return Err(StoreError::Projection(
                "provider_input_state.state exceeds its byte bound".into(),
            ));
        }
        let agent_session_id =
            id16::<AgentSessionId>("provider_input_state.agent_session_id", &agent_bytes)?;
        let projection: crate::domain::provider_input::ProviderSessionProjection =
            unpack_projection_blob("provider_input_state.state", &state)?;
        projection
            .validate_bounds()
            .map_err(|err| StoreError::Projection(err.to_string()))?;
        if !sessions.insert(agent_session_id, projection).is_none() {
            return Err(StoreError::Projection(
                "duplicate provider_input_state agent_session_id".into(),
            ));
        }
    }
    Ok(sessions)
}

fn load_agents(
    conn: &Connection,
    task_id: TaskId,
) -> Result<BTreeMap<AgentSessionId, AgentSessionFacts>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT agent_session_id, role, provider_kind, provider_session_id, lifecycle,
                runtime_generation, revision
         FROM agent_sessions WHERE task_id = ?1 ORDER BY agent_session_id ASC",
    )?;
    let rows = stmt.query_map([task_id.as_bytes().as_slice()], |row| {
        Ok((
            row.get::<_, Vec<u8>>(0)?,
            row.get::<_, Vec<u8>>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, i64>(5)?,
            row.get::<_, i64>(6)?,
        ))
    })?;
    let mut agents = BTreeMap::new();
    for row in rows {
        let (
            id_bytes,
            role,
            provider_kind,
            provider_session_id,
            lifecycle,
            runtime_generation,
            revision,
        ) = row?;
        let id = id16::<AgentSessionId>("agent_sessions.agent_session_id", &id_bytes)?;
        let agent = decode_agent_projection(
            id,
            task_id,
            role,
            provider_kind,
            provider_session_id,
            lifecycle,
            runtime_generation,
            revision,
        )?;
        agents.insert(id, agent);
    }
    Ok(agents)
}

pub(crate) fn load_agent_session(
    conn: &Connection,
    agent_session_id: AgentSessionId,
) -> Result<Option<AgentSessionFacts>, StoreError> {
    let row: Option<(Vec<u8>, Vec<u8>, String, Option<String>, String, i64, i64)> = conn
        .query_row(
            "SELECT task_id, role, provider_kind, provider_session_id, lifecycle,
                    runtime_generation, revision
             FROM agent_sessions WHERE agent_session_id = ?1",
            [agent_session_id.as_bytes().as_slice()],
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
    let Some((
        task_id,
        role,
        provider_kind,
        provider_session_id,
        lifecycle,
        runtime_generation,
        revision,
    )) = row
    else {
        return Ok(None);
    };
    let task_id = id16::<TaskId>("agent_sessions.task_id", &task_id)?;
    load_task_row(conn, task_id)?.ok_or(StoreError::Corruption)?;
    Ok(Some(decode_agent_projection(
        agent_session_id,
        task_id,
        role,
        provider_kind,
        provider_session_id,
        lifecycle,
        runtime_generation,
        revision,
    )?))
}

#[allow(clippy::too_many_arguments)]
fn decode_agent_projection(
    id: AgentSessionId,
    task_id: TaskId,
    role: Vec<u8>,
    provider_kind: String,
    provider_session_id: Option<String>,
    lifecycle: String,
    runtime_generation: i64,
    revision: i64,
) -> Result<AgentSessionFacts, StoreError> {
    let provider_kind = ProviderKind::parse_wire(&provider_kind).ok_or_else(|| {
        StoreError::Projection("agent_sessions.provider_kind is not canonical".to_string())
    })?;
    let provider_session_id = provider_session_id
        .map(ProviderSessionId::new)
        .transpose()
        .map_err(|error| StoreError::Projection(error.to_string()))?;
    let agent = AgentSessionFacts {
        id,
        task_id,
        role: unpack_projection_blob("agent_sessions.role", &role)?,
        provider_kind,
        provider_session_id,
        lifecycle: parse_agent_lifecycle(&lifecycle)?,
        runtime_generation: u64_from_nonnegative_i64(
            "agent_sessions.runtime_generation",
            runtime_generation,
        )?,
        revision: u64_from_nonnegative_i64("agent_sessions.revision", revision)?,
    };
    agent
        .validate()
        .map_err(|err| StoreError::Projection(err.to_string()))?;
    Ok(agent)
}

fn load_artifacts(
    conn: &Connection,
    task_id: TaskId,
) -> Result<BTreeMap<ArtifactId, ArtifactFacts>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT artifact_id, kind, label, content_ref, sha256, privacy_class, created_at_ms
         FROM artifacts WHERE task_id = ?1 ORDER BY artifact_id ASC",
    )?;
    let rows = stmt.query_map([task_id.as_bytes().as_slice()], |row| {
        Ok((
            row.get::<_, Vec<u8>>(0)?,
            ArtifactProjectionFields {
                kind: row.get(1)?,
                label: row.get(2)?,
                content_ref: row.get(3)?,
                sha256: row.get(4)?,
                privacy_class: row.get(5)?,
                created_at_ms: row.get(6)?,
            },
        ))
    })?;
    let mut artifacts = BTreeMap::new();
    for row in rows {
        let (id_bytes, fields) = row?;
        let id = id16::<ArtifactId>("artifacts.artifact_id", &id_bytes)?;
        let artifact = decode_artifact_projection(id, task_id, fields)?;
        artifacts.insert(id, artifact);
    }
    Ok(artifacts)
}

struct ArtifactProjectionFields {
    kind: String,
    label: String,
    content_ref: Vec<u8>,
    sha256: Vec<u8>,
    privacy_class: String,
    created_at_ms: i64,
}

pub(crate) fn load_artifact(
    conn: &Connection,
    artifact_id: ArtifactId,
) -> Result<Option<ArtifactFacts>, StoreError> {
    let row: Option<(Vec<u8>, ArtifactProjectionFields)> = conn
        .query_row(
            "SELECT task_id, kind, label, content_ref, sha256, privacy_class, created_at_ms
             FROM artifacts WHERE artifact_id = ?1",
            [artifact_id.as_bytes().as_slice()],
            |row| {
                Ok((
                    row.get(0)?,
                    ArtifactProjectionFields {
                        kind: row.get(1)?,
                        label: row.get(2)?,
                        content_ref: row.get(3)?,
                        sha256: row.get(4)?,
                        privacy_class: row.get(5)?,
                        created_at_ms: row.get(6)?,
                    },
                ))
            },
        )
        .optional()?;
    let Some((task_id, fields)) = row else {
        return Ok(None);
    };
    let task_id = id16::<TaskId>("artifacts.task_id", &task_id)?;
    load_task_row(conn, task_id)?.ok_or(StoreError::Corruption)?;
    Ok(Some(decode_artifact_projection(
        artifact_id,
        task_id,
        fields,
    )?))
}

fn decode_artifact_projection(
    id: ArtifactId,
    task_id: TaskId,
    fields: ArtifactProjectionFields,
) -> Result<ArtifactFacts, StoreError> {
    let sha256: [u8; 32] =
        fields
            .sha256
            .as_slice()
            .try_into()
            .map_err(|_| StoreError::CodecMismatch {
                detail: "artifacts.sha256 must be 32 bytes".into(),
            })?;
    let artifact = ArtifactFacts {
        id,
        task_id,
        kind: parse_artifact_kind(&fields.kind)?,
        label: fields.label,
        content_ref: unpack_projection_blob("artifacts.content_ref", &fields.content_ref)?,
        sha256,
        privacy_class: parse_privacy(&fields.privacy_class)?,
        created_at_ms: fields.created_at_ms,
    };
    artifact
        .validate()
        .map_err(|err| StoreError::Projection(err.to_string()))?;
    Ok(artifact)
}

fn load_resources(
    conn: &Connection,
    task_id: TaskId,
) -> Result<BTreeMap<ResourceId, ResourceFacts>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT resource_id, owner_kind, resource_kind, recipe, lifecycle,
                runtime_generation, updated_at_ms
         FROM resources WHERE task_id = ?1 ORDER BY resource_id ASC",
    )?;
    let rows = stmt.query_map([task_id.as_bytes().as_slice()], |row| {
        Ok((
            row.get::<_, Vec<u8>>(0)?,
            ResourceProjectionFields {
                owner_kind: row.get(1)?,
                resource_kind: row.get(2)?,
                recipe: row.get(3)?,
                lifecycle: row.get(4)?,
                runtime_generation: row.get(5)?,
                updated_at_ms: row.get(6)?,
            },
        ))
    })?;
    let mut resources = BTreeMap::new();
    for row in rows {
        let (id_bytes, fields) = row?;
        let id = id16::<ResourceId>("resources.resource_id", &id_bytes)?;
        let resource = decode_resource_projection(id, Some(task_id), fields)?;
        resources.insert(id, resource);
    }
    Ok(resources)
}

pub(crate) fn load_all_resources(conn: &Connection) -> Result<Vec<ResourceFacts>, StoreError> {
    let resource_ids = {
        let mut stmt =
            conn.prepare("SELECT resource_id FROM resources ORDER BY resource_id ASC")?;
        let rows = stmt.query_map([], |row| row.get::<_, Vec<u8>>(0))?;
        let mut resource_ids = Vec::new();
        for row in rows {
            resource_ids.push(id16::<ResourceId>("resources.resource_id", &row?)?);
        }
        resource_ids
    };

    let mut resources = Vec::with_capacity(resource_ids.len());
    for resource_id in resource_ids {
        resources.push(load_resource(conn, resource_id)?.ok_or(StoreError::Corruption)?);
    }
    Ok(resources)
}

fn load_all_agent_sessions(conn: &Connection) -> Result<Vec<AgentSessionFacts>, StoreError> {
    let agent_ids = {
        let mut stmt = conn
            .prepare("SELECT agent_session_id FROM agent_sessions ORDER BY agent_session_id ASC")?;
        let rows = stmt.query_map([], |row| row.get::<_, Vec<u8>>(0))?;
        let mut agent_ids = Vec::new();
        for row in rows {
            agent_ids.push(id16::<AgentSessionId>(
                "agent_sessions.agent_session_id",
                &row?,
            )?);
        }
        agent_ids
    };

    let mut agents = Vec::with_capacity(agent_ids.len());
    for agent_id in agent_ids {
        agents.push(load_agent_session(conn, agent_id)?.ok_or(StoreError::Corruption)?);
    }
    Ok(agents)
}

fn inspect_host_quit_in_tx(conn: &Connection) -> Result<HostQuitInspection, StoreError> {
    let mut agents = Vec::new();
    for agent in load_all_agent_sessions(conn)? {
        if !matches!(
            agent.lifecycle,
            AgentSessionLifecycle::Open | AgentSessionLifecycle::Closing
        ) {
            continue;
        }
        let task = load_task_row(conn, agent.task_id)?.ok_or(StoreError::Corruption)?;
        agents.push(HostQuitAgentBlocker {
            agent_session_id: agent.id,
            task_id: agent.task_id,
            task_title: task.task.title,
            role: agent.role,
            provider_kind: agent.provider_kind,
            lifecycle: agent.lifecycle,
            runtime_generation: agent.runtime_generation,
        });
    }

    let mut resources = Vec::new();
    for resource in load_all_resources(conn)? {
        if !matches!(
            resource.lifecycle,
            ResourceLifecycle::Active | ResourceLifecycle::Releasing
        ) {
            continue;
        }
        let (task_id, task_title) = match resource.task_id {
            Some(task_id) => {
                let task = load_task_row(conn, task_id)?.ok_or(StoreError::Corruption)?;
                (Some(task_id), Some(task.task.title))
            }
            None => (None, None),
        };
        resources.push(HostQuitResourceBlocker {
            resource_id: resource.id,
            task_id,
            task_title,
            owner_kind: resource.owner_kind,
            resource_kind: resource.resource_kind,
            lifecycle: resource.lifecycle,
            runtime_generation: resource.runtime_generation,
        });
    }

    Ok(HostQuitInspection {
        inspection_id: durable_event_high_water(conn)?,
        agents,
        resources,
        worktrees: HostQuitWorktreeInspection::NotInspected,
        confirmable: false,
    })
}

fn durable_event_high_water(conn: &Connection) -> Result<u64, StoreError> {
    let max_sequence: i64 =
        conn.query_row("SELECT COALESCE(MAX(sequence), 0) FROM events", [], |row| {
            row.get(0)
        })?;
    u64_from_nonnegative_i64("events.sequence", max_sequence)
}

fn host_admission_is_closing(conn: &Connection) -> Result<bool, StoreError> {
    Ok(load_host_admission_row(conn)?.is_some())
}

struct HostAdmissionRow {
    operation_id: OperationId,
    action_epoch: u64,
    inspection_id: u64,
    updated_at_ms: i64,
}

fn load_host_admission_row(conn: &Connection) -> Result<Option<HostAdmissionRow>, StoreError> {
    let row: Option<(Vec<u8>, i64, i64, i64)> = conn
        .query_row(
            "SELECT operation_id, action_epoch, inspection_id, updated_at_ms
             FROM host_admission WHERE singleton_key = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;
    let Some((operation_id, action_epoch, inspection_id, updated_at_ms)) = row else {
        return Ok(None);
    };
    Ok(Some(HostAdmissionRow {
        operation_id: id16::<OperationId>("host_admission.operation_id", &operation_id)?,
        action_epoch: u64_from_nonnegative_i64("host_admission.action_epoch", action_epoch)?,
        inspection_id: u64_from_nonnegative_i64("host_admission.inspection_id", inspection_id)?,
        updated_at_ms,
    }))
}

fn persist_confirm_host_quit(
    tx: &Transaction<'_>,
    envelope: CommandEnvelope,
    scope: ReceiptScope,
) -> Result<CommandReceipt, StoreError> {
    let accepted_at_ms = now_ms()?;
    let Command::ConfirmHostQuit(ConfirmHostQuitIntent {
        inspection_id,
        allow_uninspected_worktrees,
    }) = &envelope.command
    else {
        return Err(StoreError::Projection(
            "persist_confirm_host_quit requires ConfirmHostQuit".into(),
        ));
    };

    if envelope.task_id.is_some() || envelope.expected_task_revision.is_some() {
        return persist_rejection(
            tx,
            &envelope,
            None,
            RejectionCode::InvalidTransition,
            None,
            accepted_at_ms,
            scope,
        );
    }
    if host_admission_is_closing(tx)? {
        return persist_rejection(
            tx,
            &envelope,
            None,
            RejectionCode::Closing,
            None,
            accepted_at_ms,
            scope,
        );
    }
    let current_high_water = durable_event_high_water(tx)?;
    if *inspection_id != current_high_water {
        return persist_rejection(
            tx,
            &envelope,
            None,
            RejectionCode::RevisionConflict,
            None,
            accepted_at_ms,
            scope,
        );
    }
    if !*allow_uninspected_worktrees {
        return persist_rejection(
            tx,
            &envelope,
            None,
            RejectionCode::InvalidTransition,
            None,
            accepted_at_ms,
            scope,
        );
    }

    let operation_id = OperationId::new();
    let begun_event_id = EventId::new();
    let accepted_event_id = EventId::new();
    const HOST_CLOSE_ACTION_EPOCH: u64 = 1;

    let receipt = CommandReceipt::Accepted {
        command_id: envelope.command_id,
        operation_id,
        task_revision: None,
        event_ids: vec![begun_event_id],
        prompt_mutation: None,
    };
    insert_receipt_row(tx, &envelope, None, &receipt, None, accepted_at_ms, scope)?;

    append_and_project(
        tx,
        begun_event_id,
        None,
        None,
        accepted_at_ms,
        Event::HostCloseBegun {
            operation_id,
            action_epoch: HOST_CLOSE_ACTION_EPOCH,
            inspection_id: *inspection_id,
        },
    )?;

    let accepted = OperationAcceptedFact::new(
        envelope.command_id,
        operation_id,
        accepted_at_ms,
        Some(HOST_CLOSE_ACTION_EPOCH),
        None,
        None,
    )
    .map_err(|err| StoreError::Projection(err.to_string()))?;
    let committed_sequence = append_and_project(
        tx,
        accepted_event_id,
        None,
        None,
        accepted_at_ms,
        Event::OperationAccepted(accepted),
    )?;
    set_committed_sequence(tx, envelope.command_id, committed_sequence)?;
    Ok(receipt)
}

pub(crate) fn advance_next_host_cleanup_unit_in_tx(
    tx: &Transaction<'_>,
    now_ms: i64,
    lease_ms: i64,
) -> Result<HostCleanupUnit, StoreError> {
    validate_rebuilt_host_admission(tx)?;
    let Some(admission) = load_host_admission_row(tx)? else {
        return Ok(HostCleanupUnit::Idle);
    };
    validate_current_host_cleanup_journal(tx, &admission)?;
    let Some(branch) = next_absent_host_cleanup_branch(tx, admission.operation_id)? else {
        return finalize_complete_host_cleanup_journal_in_tx(tx, &admission, now_ms);
    };

    match branch {
        HostCleanupBranch::AgentSessions => {
            let remaining = count_open_or_closing_agents(tx)?;
            complete_host_cleanup_branch(
                tx,
                admission.operation_id,
                admission.action_epoch,
                branch,
                remaining,
                now_ms,
            )
        }
        HostCleanupBranch::Resources => {
            let remaining = count_active_or_releasing_resources(tx)?;
            complete_host_cleanup_branch(
                tx,
                admission.operation_id,
                admission.action_epoch,
                branch,
                remaining,
                now_ms,
            )
        }
        HostCleanupBranch::OutstandingEffects => {
            // Nonterminal = pending/claimed/dispatching/reconcile_required/reconciling.
            // Outbox `uncertain` is terminal under existing dispatch contracts and is
            // intentionally excluded from outstanding residue counts.
            // Full receipt-backed outbox correlation (event_sequence / effect fence)
            // before counting — structurally valid wrong lineage must be Corruption,
            // not residue. Decode/codec failures fail closed as Corruption so host
            // cleanup never treats a corrupt pending effect as countable residue.
            validate_host_cleanup_outbox_lineage(tx)?;
            let remaining = count_nonterminal_non_teardown_outbox(tx)?;
            complete_host_cleanup_branch(
                tx,
                admission.operation_id,
                admission.action_epoch,
                branch,
                remaining,
                now_ms,
            )
        }
        HostCleanupBranch::TaskTeardowns => {
            // Revalidate after the durable OutstandingEffects crash boundary.
            // A blocked/noncandidate teardown must not be counted as residue when
            // its receipt, event sequence, payload, or effect fence is corrupt.
            validate_host_cleanup_outbox_lineage(tx)?;
            if let Some((task_id, operation_id)) =
                crate::kernel::store::settle_next_process_empty_task_teardown_in_tx_for_cleanup(
                    tx, now_ms, lease_ms,
                )?
            {
                return Ok(HostCleanupUnit::Progressed {
                    task_id,
                    operation_id,
                });
            }
            let remaining = count_nonterminal_teardown_outbox(tx)?;
            complete_host_cleanup_branch(
                tx,
                admission.operation_id,
                admission.action_epoch,
                branch,
                remaining,
                now_ms,
            )
        }
    }
}

fn validate_host_cleanup_outbox_lineage(tx: &Transaction<'_>) -> Result<(), StoreError> {
    match validate_all_rebuilt_outbox_metadata(tx) {
        Ok(()) => Ok(()),
        Err(StoreError::CodecMismatch { .. }) | Err(StoreError::EventDecode(_)) => {
            Err(StoreError::Corruption)
        }
        Err(err) => Err(err),
    }
}

fn next_absent_host_cleanup_branch(
    tx: &Connection,
    operation_id: OperationId,
) -> Result<Option<HostCleanupBranch>, StoreError> {
    let mut saw_absent = false;
    let mut next = None;
    for branch in HostCleanupBranch::ORDER {
        let exists: i64 = tx.query_row(
            "SELECT COUNT(*) FROM host_cleanup_branches
             WHERE operation_id = ?1 AND branch = ?2",
            rusqlite::params![operation_id.as_bytes().as_slice(), branch.as_str()],
            |row| row.get(0),
        )?;
        match exists {
            0 => {
                if next.is_none() {
                    next = Some(branch);
                }
                saw_absent = true;
            }
            1 => {
                if saw_absent {
                    return Err(StoreError::Corruption);
                }
            }
            _ => return Err(StoreError::Corruption),
        }
    }
    Ok(next)
}

fn load_ordered_host_cleanup_journal_outcomes(
    tx: &Connection,
    operation_id: OperationId,
) -> Result<Vec<(HostCleanupBranch, HostCleanupBranchOutcome)>, StoreError> {
    let mut journal = Vec::with_capacity(HostCleanupBranch::ORDER.len());
    for branch in HostCleanupBranch::ORDER {
        let row: Option<(String, i64)> = tx
            .query_row(
                "SELECT result, remaining_count FROM host_cleanup_branches
                 WHERE operation_id = ?1 AND branch = ?2",
                rusqlite::params![operation_id.as_bytes().as_slice(), branch.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((result, remaining_count)) = row else {
            break;
        };
        let remaining =
            u64_from_nonnegative_i64("host_cleanup_branches.remaining_count", remaining_count)?;
        let outcome = match result.as_str() {
            "succeeded" if remaining == 0 => HostCleanupBranchOutcome::succeeded(),
            "failed" => {
                HostCleanupBranchOutcome::failed(remaining).ok_or(StoreError::Corruption)?
            }
            _ => return Err(StoreError::Corruption),
        };
        journal.push((branch, outcome));
    }
    if journal.len() != HostCleanupBranch::ORDER.len() {
        return Err(StoreError::Corruption);
    }
    Ok(journal)
}

fn finalize_complete_host_cleanup_journal_in_tx(
    tx: &Transaction<'_>,
    admission: &HostAdmissionRow,
    now_ms: i64,
) -> Result<HostCleanupUnit, StoreError> {
    let operation = load_operation_projection_by_id(tx, admission.operation_id)?
        .ok_or(StoreError::Corruption)?;
    match operation.state.as_str() {
        "failed" => {
            if operation.outcome_code.as_deref() != Some("cleanup_failed")
                || operation.result.is_some()
                || operation.outcome_at_ms.is_none()
            {
                return Err(StoreError::Corruption);
            }
            Ok(HostCleanupUnit::Idle)
        }
        "settled" => {
            // All-success settle is explicit; maintenance stays Idle after settle.
            if operation.outcome_code.is_some() || operation.result.is_none() {
                return Err(StoreError::Corruption);
            }
            Ok(HostCleanupUnit::Idle)
        }
        "accepted" => {
            if operation.result.is_some()
                || operation.outcome_code.is_some()
                || operation.outcome_at_ms.is_some()
            {
                return Err(StoreError::Corruption);
            }
            let journal = load_ordered_host_cleanup_journal_outcomes(tx, admission.operation_id)?;
            let any_failed = journal
                .iter()
                .any(|(_, outcome)| matches!(outcome, HostCleanupBranchOutcome::Failed { .. }));
            if !any_failed {
                return Ok(HostCleanupUnit::ReadyToExit {
                    operation_id: admission.operation_id,
                    action_epoch: admission.action_epoch,
                });
            }
            terminalize_host_cleanup_failed_in_tx(tx, admission, &operation, &journal, now_ms)
        }
        _ => Err(StoreError::Corruption),
    }
}

/// Explicit all-success host-cleanup settle for c8b after physical exit is armed.
///
/// Exact retry is idempotent and returns the same persisted terminal
/// [`DomainEvent`] (event id + sequence). Maintenance must never call this.
pub(crate) fn settle_host_cleanup_success_in_tx(
    tx: &Transaction<'_>,
    now_ms: i64,
) -> Result<DomainEvent, StoreError> {
    let admission = load_host_admission_row(tx)?.ok_or(StoreError::Corruption)?;
    let operation = load_operation_projection_by_id(tx, admission.operation_id)?
        .ok_or(StoreError::Corruption)?;
    if operation.task_id.is_some()
        || operation.resource_id.is_some()
        || operation.runtime_generation.is_some()
    {
        return Err(StoreError::Corruption);
    }
    let Some(action_epoch_i64) = operation.action_epoch else {
        return Err(StoreError::Corruption);
    };
    let action_epoch = u64_from_nonnegative_i64("operations.action_epoch", action_epoch_i64)?;
    if admission.action_epoch != action_epoch || admission.updated_at_ms != operation.accepted_at_ms
    {
        return Err(StoreError::Corruption);
    }
    let command_bytes: Vec<u8> = tx.query_row(
        "SELECT command_id FROM operations WHERE operation_id = ?1",
        [admission.operation_id.as_bytes().as_slice()],
        |row| row.get(0),
    )?;
    let command_id = id16::<CommandId>("operations.command_id", &command_bytes)?;
    let receipt = lookup_receipt(tx, command_id)?.ok_or(StoreError::Corruption)?;
    match &receipt {
        CommandReceipt::Accepted {
            operation_id,
            task_revision: None,
            event_ids,
            ..
        } if *operation_id == admission.operation_id && event_ids.len() == 1 => {}
        _ => return Err(StoreError::Corruption),
    }
    validate_current_host_cleanup_journal(tx, &admission)?;
    let journal = load_ordered_host_cleanup_journal_outcomes(tx, admission.operation_id)?;
    let journal_max_completed_at_ms =
        host_cleanup_journal_max_completed_at(tx, admission.operation_id)?;
    let ordered_branch_ids =
        load_ordered_host_cleanup_branch_event_ids(tx, admission.operation_id, action_epoch)?;

    match operation.state.as_str() {
        "settled" => {
            if operation.outcome_code.is_some() {
                return Err(StoreError::Corruption);
            }
            let settled_at_ms = operation.outcome_at_ms.ok_or(StoreError::Corruption)?;
            let projected_result = unpack_projection_blob::<Vec<EventId>>(
                "operations.result",
                operation.result.as_deref().ok_or(StoreError::Corruption)?,
            )?;
            if projected_result != ordered_branch_ids {
                return Err(StoreError::Corruption);
            }
            let terminal = require_exact_host_cleanup_settled_terminal(
                tx,
                command_id,
                admission.operation_id,
            )?;
            if terminal.occurred_at_ms != settled_at_ms
                || terminal.task_id.is_some()
                || terminal.task_revision.is_some()
            {
                return Err(StoreError::Corruption);
            }
            let prior = load_global_event_before(tx, terminal.sequence)?;
            crate::kernel::lineage::validate_host_admission_settled_lineage(
                &terminal.fact,
                settled_at_ms,
                prior
                    .as_ref()
                    .map(|(id, ev, rev, occurred, task)| (*id, ev, *rev, *occurred, *task)),
                &ordered_branch_ids,
                &journal,
                journal_max_completed_at_ms,
                operation.accepted_at_ms,
                command_id,
                admission.operation_id,
                action_epoch,
                false,
            )?;
            Ok(DomainEvent {
                id: terminal.event_id,
                task_id: None,
                sequence: terminal.sequence,
                task_revision: None,
                occurred_at_ms: settled_at_ms,
                payload: Event::OperationSettled(terminal.fact),
            })
        }
        "accepted" => {
            if operation.result.is_some()
                || operation.outcome_code.is_some()
                || operation.outcome_at_ms.is_some()
            {
                return Err(StoreError::Corruption);
            }
            require_host_operation_terminal_match_count(tx, command_id, admission.operation_id, 0)?;
            if journal.len() != HostCleanupBranch::ORDER.len()
                || journal
                    .iter()
                    .any(|(_, outcome)| !matches!(outcome, HostCleanupBranchOutcome::Succeeded))
            {
                return Err(StoreError::Corruption);
            }
            if operation.accepted_at_ms > now_ms || journal_max_completed_at_ms > now_ms {
                return Err(StoreError::Corruption);
            }
            let max_sequence: i64 =
                tx.query_row("SELECT COALESCE(MAX(sequence), 0) FROM events", [], |row| {
                    row.get(0)
                })?;
            let next_sequence = u64_from_nonnegative_i64("events.sequence", max_sequence)?
                .checked_add(1)
                .ok_or(StoreError::Corruption)?;
            let prior = load_global_event_before(tx, next_sequence)?;
            let settled = OperationSettledFact::with_source(
                command_id,
                admission.operation_id,
                now_ms,
                ordered_branch_ids.clone(),
                Some(action_epoch),
                None,
                None,
                OutcomeSource::Dispatch,
            )
            .map_err(|err| StoreError::Projection(err.to_string()))?;
            crate::kernel::lineage::validate_host_admission_settled_lineage(
                &settled,
                now_ms,
                prior
                    .as_ref()
                    .map(|(id, ev, rev, occurred, task)| (*id, ev, *rev, *occurred, *task)),
                &ordered_branch_ids,
                &journal,
                journal_max_completed_at_ms,
                operation.accepted_at_ms,
                command_id,
                admission.operation_id,
                action_epoch,
                false,
            )?;
            let event_id = EventId::new();
            let sequence = append_and_project(
                tx,
                event_id,
                None,
                None,
                now_ms,
                Event::OperationSettled(settled.clone()),
            )?;
            Ok(DomainEvent {
                id: event_id,
                task_id: None,
                sequence,
                task_revision: None,
                occurred_at_ms: now_ms,
                payload: Event::OperationSettled(settled),
            })
        }
        _ => Err(StoreError::Corruption),
    }
}

/// Read-only durable restart disposition for bind/serve decisions.
pub(crate) fn host_restart_disposition_in_tx(
    tx: &Connection,
) -> Result<HostRestartDispositionUnit, StoreError> {
    let Some(admission) = load_host_admission_row(tx)? else {
        assert_pristine_of_host_quit_lineage(tx)?;
        return Ok(HostRestartDispositionUnit::ServeResume);
    };
    let operation = load_operation_projection_by_id(tx, admission.operation_id)?
        .ok_or(StoreError::Corruption)?;
    if operation.task_id.is_some()
        || operation.resource_id.is_some()
        || operation.runtime_generation.is_some()
    {
        return Err(StoreError::Corruption);
    }
    let Some(action_epoch_i64) = operation.action_epoch else {
        return Err(StoreError::Corruption);
    };
    let action_epoch = u64_from_nonnegative_i64("operations.action_epoch", action_epoch_i64)?;
    if admission.action_epoch != action_epoch || admission.updated_at_ms != operation.accepted_at_ms
    {
        return Err(StoreError::Corruption);
    }
    let command_bytes: Vec<u8> = tx.query_row(
        "SELECT command_id FROM operations WHERE operation_id = ?1",
        [admission.operation_id.as_bytes().as_slice()],
        |row| row.get(0),
    )?;
    let command_id = id16::<CommandId>("operations.command_id", &command_bytes)?;
    let receipt = lookup_receipt(tx, command_id)?.ok_or(StoreError::Corruption)?;
    match &receipt {
        CommandReceipt::Accepted {
            operation_id,
            task_revision: None,
            event_ids,
            ..
        } if *operation_id == admission.operation_id && event_ids.len() == 1 => {}
        _ => return Err(StoreError::Corruption),
    }

    match operation.state.as_str() {
        "accepted" => {
            if operation.result.is_some()
                || operation.outcome_code.is_some()
                || operation.outcome_at_ms.is_some()
            {
                return Err(StoreError::Corruption);
            }
            require_host_operation_terminal_match_count(tx, command_id, admission.operation_id, 0)?;
            let projected = load_host_cleanup_branch_projection_map(tx)?;
            let facts = load_host_cleanup_branch_event_map(tx, Some(&admission))?;
            if facts != projected {
                return Err(StoreError::Corruption);
            }
            validate_host_cleanup_branch_prefix_map(&projected)?;
            if projected.len() != HostCleanupBranch::ORDER.len() {
                return Ok(HostRestartDispositionUnit::ServeResume);
            }
            let journal = load_ordered_host_cleanup_journal_outcomes(tx, admission.operation_id)?;
            if journal
                .iter()
                .any(|(_, outcome)| matches!(outcome, HostCleanupBranchOutcome::Failed { .. }))
            {
                // Complete failed journal awaiting CleanupFailed terminalization.
                return Ok(HostRestartDispositionUnit::ServeResume);
            }
            if journal
                .iter()
                .any(|(_, outcome)| !matches!(outcome, HostCleanupBranchOutcome::Succeeded))
            {
                return Err(StoreError::Corruption);
            }
            Ok(HostRestartDispositionUnit::ReadyToArmAndSettle {
                operation_id: admission.operation_id,
                action_epoch,
            })
        }
        "failed" => {
            if operation.result.is_some()
                || operation.outcome_code.as_deref() != Some("cleanup_failed")
            {
                return Err(StoreError::Corruption);
            }
            let settled_at_ms = operation.outcome_at_ms.ok_or(StoreError::Corruption)?;
            validate_current_host_cleanup_journal(tx, &admission)?;
            let journal = load_ordered_host_cleanup_journal_outcomes(tx, admission.operation_id)?;
            let journal_max_completed_at_ms =
                host_cleanup_journal_max_completed_at(tx, admission.operation_id)?;
            let terminal =
                require_exact_host_cleanup_failed_terminal(tx, command_id, admission.operation_id)?;
            if terminal.occurred_at_ms != settled_at_ms
                || terminal.task_id.is_some()
                || terminal.task_revision.is_some()
            {
                return Err(StoreError::Corruption);
            }
            let prior = load_global_event_before(tx, terminal.sequence)?;
            crate::kernel::lineage::validate_host_admission_cleanup_failed_lineage(
                &terminal.fact,
                settled_at_ms,
                prior
                    .as_ref()
                    .map(|(id, ev, rev, occurred, task)| (*id, ev, *rev, *occurred, *task)),
                &journal,
                journal_max_completed_at_ms,
                command_id,
                admission.operation_id,
                action_epoch,
                false,
            )?;
            Ok(HostRestartDispositionUnit::ServeInspection {
                operation_id: admission.operation_id,
                action_epoch,
                settled_at_ms,
            })
        }
        "settled" => {
            if operation.outcome_code.is_some() {
                return Err(StoreError::Corruption);
            }
            let settled_at_ms = operation.outcome_at_ms.ok_or(StoreError::Corruption)?;
            let projected_result = unpack_projection_blob::<Vec<EventId>>(
                "operations.result",
                operation.result.as_deref().ok_or(StoreError::Corruption)?,
            )?;
            validate_current_host_cleanup_journal(tx, &admission)?;
            let journal = load_ordered_host_cleanup_journal_outcomes(tx, admission.operation_id)?;
            let journal_max_completed_at_ms =
                host_cleanup_journal_max_completed_at(tx, admission.operation_id)?;
            let ordered_branch_ids = load_ordered_host_cleanup_branch_event_ids(
                tx,
                admission.operation_id,
                action_epoch,
            )?;
            if projected_result != ordered_branch_ids {
                return Err(StoreError::Corruption);
            }
            let terminal = require_exact_host_cleanup_settled_terminal(
                tx,
                command_id,
                admission.operation_id,
            )?;
            if terminal.occurred_at_ms != settled_at_ms
                || terminal.task_id.is_some()
                || terminal.task_revision.is_some()
            {
                return Err(StoreError::Corruption);
            }
            let prior = load_global_event_before(tx, terminal.sequence)?;
            crate::kernel::lineage::validate_host_admission_settled_lineage(
                &terminal.fact,
                settled_at_ms,
                prior
                    .as_ref()
                    .map(|(id, ev, rev, occurred, task)| (*id, ev, *rev, *occurred, *task)),
                &ordered_branch_ids,
                &journal,
                journal_max_completed_at_ms,
                operation.accepted_at_ms,
                command_id,
                admission.operation_id,
                action_epoch,
                false,
            )?;
            Ok(HostRestartDispositionUnit::Closed {
                operation_id: admission.operation_id,
                action_epoch,
                settled_at_ms,
            })
        }
        _ => Err(StoreError::Corruption),
    }
}

fn assert_pristine_of_host_quit_lineage(tx: &Connection) -> Result<(), StoreError> {
    let close_begun: i64 = tx.query_row(
        "SELECT COUNT(*) FROM events WHERE event_type = 'host.close_begun'",
        [],
        |row| row.get(0),
    )?;
    if close_begun != 0 {
        return Err(StoreError::Corruption);
    }
    let branch_events: i64 = tx.query_row(
        "SELECT COUNT(*) FROM events WHERE event_type = 'host.cleanup_branch_completed'",
        [],
        |row| row.get(0),
    )?;
    if branch_events != 0 {
        return Err(StoreError::Corruption);
    }
    let branch_rows: i64 =
        tx.query_row("SELECT COUNT(*) FROM host_cleanup_branches", [], |row| {
            row.get(0)
        })?;
    if branch_rows != 0 {
        return Err(StoreError::Corruption);
    }

    // HostAdmission operations projection: global/taskless + action_epoch present.
    let mut ops = tx
        .prepare("SELECT task_id, action_epoch, resource_id, runtime_generation FROM operations")?;
    let op_rows = ops.query_map([], |row| {
        Ok((
            row.get::<_, Option<Vec<u8>>>(0)?,
            row.get::<_, Option<i64>>(1)?,
            row.get::<_, Option<Vec<u8>>>(2)?,
            row.get::<_, Option<i64>>(3)?,
        ))
    })?;
    for row in op_rows {
        let (task_bytes, action_epoch, resource_id, runtime_generation) = row?;
        let task_id = parse_optional_task_scope("operations.task_id", task_bytes)?;
        if task_id.is_none()
            && action_epoch.is_some()
            && resource_id.is_none()
            && runtime_generation.is_none()
        {
            return Err(StoreError::Corruption);
        }
    }

    // Global/taskless accepted + terminal facts carrying HostAdmission fence, even if projection missing.
    let mut events = tx.prepare(
        "SELECT task_id, task_revision, event_type, schema_version, payload
         FROM events
         WHERE event_type IN (
             'operation.accepted',
             'operation.settled',
             'operation.failed',
             'operation.cancelled',
             'operation.uncertain'
         )
         ORDER BY sequence ASC",
    )?;
    let event_rows = events.query_map([], |row| {
        Ok((
            row.get::<_, Option<Vec<u8>>>(0)?,
            row.get::<_, Option<i64>>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, Vec<u8>>(4)?,
        ))
    })?;
    for row in event_rows {
        let (task_bytes, task_revision, event_type, schema_version, payload) = row?;
        let task_id = parse_optional_task_scope("events.task_id", task_bytes)?;
        if task_id.is_some() {
            continue;
        }
        if task_revision.is_some() {
            return Err(StoreError::Corruption);
        }
        let event =
            crate::kernel::store::decode_stored_event(&event_type, schema_version, &payload)?;
        let (action_epoch, resource_id, runtime_generation) = match &event {
            Event::OperationAccepted(fact) => {
                (fact.action_epoch, fact.resource_id, fact.runtime_generation)
            }
            Event::OperationSettled(fact) => {
                (fact.action_epoch, fact.resource_id, fact.runtime_generation)
            }
            Event::OperationFailed(fact) => {
                (fact.action_epoch, fact.resource_id, fact.runtime_generation)
            }
            Event::OperationCancelled(fact) => {
                (fact.action_epoch, fact.resource_id, fact.runtime_generation)
            }
            Event::OperationUncertain(fact) => {
                (fact.action_epoch, fact.resource_id, fact.runtime_generation)
            }
            _ => return Err(StoreError::Corruption),
        };
        match crate::kernel::lineage::classify_settled_lineage_fence(
            action_epoch,
            resource_id,
            runtime_generation,
            None,
            false,
        )? {
            crate::kernel::lineage::SettledLineageKind::HostAdmission => {
                return Err(StoreError::Corruption);
            }
            _ => {}
        }
    }
    Ok(())
}

fn terminalize_host_cleanup_failed_in_tx(
    tx: &Transaction<'_>,
    admission: &HostAdmissionRow,
    operation: &OperationProjectionRow,
    journal: &[(HostCleanupBranch, HostCleanupBranchOutcome)],
    now_ms: i64,
) -> Result<HostCleanupUnit, StoreError> {
    let command_bytes: Vec<u8> = tx.query_row(
        "SELECT command_id FROM operations WHERE operation_id = ?1",
        [admission.operation_id.as_bytes().as_slice()],
        |row| row.get(0),
    )?;
    let command_id = id16::<CommandId>("operations.command_id", &command_bytes)?;
    if operation.accepted_at_ms > now_ms {
        return Err(StoreError::Corruption);
    }
    let failed = OperationFailedFact::with_source(
        command_id,
        admission.operation_id,
        now_ms,
        OperationErrorCode::CleanupFailed,
        Some(admission.action_epoch),
        None,
        None,
        OutcomeSource::Dispatch,
    )
    .map_err(|err| StoreError::Projection(err.to_string()))?;
    let max_sequence: i64 =
        tx.query_row("SELECT COALESCE(MAX(sequence), 0) FROM events", [], |row| {
            row.get(0)
        })?;
    let next_sequence = u64_from_nonnegative_i64("events.sequence", max_sequence)?
        .checked_add(1)
        .ok_or(StoreError::Corruption)?;
    let prior = load_global_event_before(tx, next_sequence)?;
    let journal_max_completed_at_ms =
        host_cleanup_journal_max_completed_at(tx, admission.operation_id)?;
    crate::kernel::lineage::validate_host_admission_cleanup_failed_lineage(
        &failed,
        now_ms,
        prior
            .as_ref()
            .map(|(id, ev, rev, occurred, task)| (*id, ev, *rev, *occurred, *task)),
        journal,
        journal_max_completed_at_ms,
        command_id,
        admission.operation_id,
        admission.action_epoch,
        false,
    )?;
    append_and_project(
        tx,
        EventId::new(),
        None,
        None,
        now_ms,
        Event::OperationFailed(failed),
    )?;
    Ok(HostCleanupUnit::Failed {
        operation_id: admission.operation_id,
        action_epoch: admission.action_epoch,
        settled_at_ms: now_ms,
    })
}

fn complete_host_cleanup_branch(
    tx: &Transaction<'_>,
    operation_id: OperationId,
    action_epoch: u64,
    branch: HostCleanupBranch,
    remaining: u64,
    now_ms: i64,
) -> Result<HostCleanupUnit, StoreError> {
    let outcome = if remaining == 0 {
        HostCleanupBranchOutcome::succeeded()
    } else {
        HostCleanupBranchOutcome::failed(remaining).ok_or(StoreError::Corruption)?
    };
    append_and_project(
        tx,
        EventId::new(),
        None,
        None,
        now_ms,
        Event::HostCleanupBranchCompleted {
            operation_id,
            action_epoch,
            branch,
            outcome,
        },
    )?;
    Ok(HostCleanupUnit::BranchCompleted {
        operation_id,
        action_epoch,
        branch,
        outcome,
    })
}

fn count_open_or_closing_agents(tx: &Connection) -> Result<u64, StoreError> {
    let count: i64 = tx.query_row(
        "SELECT COUNT(*) FROM agent_sessions
         WHERE lifecycle IN ('open', 'closing')",
        [],
        |row| row.get(0),
    )?;
    u64_from_nonnegative_i64("agent_sessions.open_or_closing", count)
}

fn count_active_or_releasing_resources(tx: &Connection) -> Result<u64, StoreError> {
    let count: i64 = tx.query_row(
        "SELECT COUNT(*) FROM resources
         WHERE lifecycle IN ('active', 'releasing')",
        [],
        |row| row.get(0),
    )?;
    u64_from_nonnegative_i64("resources.active_or_releasing", count)
}

fn count_nonterminal_non_teardown_outbox(tx: &Connection) -> Result<u64, StoreError> {
    let count: i64 = tx.query_row(
        "SELECT COUNT(*) FROM outbox
         WHERE state IN ('pending', 'claimed', 'dispatching', 'reconcile_required', 'reconciling')
           AND destination_class != 'task_teardown'",
        [],
        |row| row.get(0),
    )?;
    u64_from_nonnegative_i64("outbox.nonterminal_non_teardown", count)
}

fn count_nonterminal_teardown_outbox(tx: &Connection) -> Result<u64, StoreError> {
    let count: i64 = tx.query_row(
        "SELECT COUNT(*) FROM outbox
         WHERE state IN ('pending', 'claimed', 'dispatching', 'reconcile_required', 'reconciling')
           AND destination_class = 'task_teardown'",
        [],
        |row| row.get(0),
    )?;
    u64_from_nonnegative_i64("outbox.nonterminal_teardown", count)
}

struct ResourceProjectionFields {
    owner_kind: String,
    resource_kind: String,
    recipe: Vec<u8>,
    lifecycle: String,
    runtime_generation: i64,
    updated_at_ms: i64,
}

pub(crate) fn load_resource(
    conn: &Connection,
    resource_id: ResourceId,
) -> Result<Option<ResourceFacts>, StoreError> {
    let row: Option<(Option<Vec<u8>>, ResourceProjectionFields)> = conn
        .query_row(
            "SELECT task_id, owner_kind, resource_kind, recipe, lifecycle,
                    runtime_generation, updated_at_ms
             FROM resources WHERE resource_id = ?1",
            [resource_id.as_bytes().as_slice()],
            |row| {
                Ok((
                    row.get(0)?,
                    ResourceProjectionFields {
                        owner_kind: row.get(1)?,
                        resource_kind: row.get(2)?,
                        recipe: row.get(3)?,
                        lifecycle: row.get(4)?,
                        runtime_generation: row.get(5)?,
                        updated_at_ms: row.get(6)?,
                    },
                ))
            },
        )
        .optional()?;
    let Some((task_id, fields)) = row else {
        return Ok(None);
    };
    let task_id = parse_optional_task_scope("resources.task_id", task_id)?;
    let resource = decode_resource_projection(resource_id, task_id, fields)?;
    if let Some(task_id) = task_id {
        let task = load_task_row(conn, task_id)?.ok_or(StoreError::Corruption)?;
        if task.task.lifecycle == TaskLifecycle::Archived
            && matches!(
                resource.lifecycle,
                ResourceLifecycle::Active | ResourceLifecycle::Releasing
            )
        {
            return Err(StoreError::Projection(
                "archived task cannot own an active or releasing resource".into(),
            ));
        }
    }
    Ok(Some(resource))
}

fn decode_resource_projection(
    id: ResourceId,
    task_id: Option<TaskId>,
    fields: ResourceProjectionFields,
) -> Result<ResourceFacts, StoreError> {
    let resource = ResourceFacts {
        id,
        task_id,
        owner_kind: parse_owner_kind(&fields.owner_kind)?,
        resource_kind: parse_resource_kind(&fields.resource_kind)?,
        recipe: unpack_projection_blob("resources.recipe", &fields.recipe)?,
        lifecycle: parse_resource_lifecycle(&fields.lifecycle)?,
        runtime_generation: u64_from_nonnegative_i64(
            "resources.runtime_generation",
            fields.runtime_generation,
        )?,
        updated_at_ms: fields.updated_at_ms,
    };
    resource
        .validate()
        .map_err(|err| StoreError::Projection(err.to_string()))?;
    Ok(resource)
}

fn unpack_projection_blob<T>(field: &str, bytes: &[u8]) -> Result<T, StoreError>
where
    T: serde::de::DeserializeOwned + serde::Serialize,
{
    let value: T = rmp_serde::from_slice(bytes).map_err(|err| StoreError::CodecMismatch {
        detail: format!("{field}: {err}"),
    })?;
    let reencoded = projector::pack(&value)?;
    if reencoded.as_slice() != bytes {
        return Err(StoreError::CodecMismatch {
            detail: format!("{field}: persisted projection blob is not lossless"),
        });
    }
    Ok(value)
}

fn id16<T>(field: &'static str, bytes: &[u8]) -> Result<T, StoreError>
where
    T: TryFromBytes16,
{
    let array: [u8; 16] = bytes.try_into().map_err(|_| StoreError::CodecMismatch {
        detail: format!("{field} must be 16 bytes"),
    })?;
    T::try_from_bytes16(array).map_err(|err| StoreError::CodecMismatch {
        detail: format!("{field}: {err}"),
    })
}

trait TryFromBytes16: Sized {
    fn try_from_bytes16(bytes: [u8; 16]) -> Result<Self, String>;
}

macro_rules! impl_try_from_bytes16 {
    ($($ty:ty),+ $(,)?) => {
        $(
            impl TryFromBytes16 for $ty {
                fn try_from_bytes16(bytes: [u8; 16]) -> Result<Self, String> {
                    Self::from_bytes(bytes).map_err(|err| err.to_string())
                }
            }
        )+
    };
}

impl_try_from_bytes16!(
    TaskId,
    EnvironmentId,
    ProjectId,
    AgentSessionId,
    ArtifactId,
    ResourceId,
    OperationId,
    EventId,
    OutboxId,
    CommandId,
);

fn parse_lifecycle(value: &str) -> Result<TaskLifecycle, StoreError> {
    match value {
        "open" => Ok(TaskLifecycle::Open),
        "settled" => Ok(TaskLifecycle::Settled),
        "closing" => Ok(TaskLifecycle::Closing),
        "archived" => Ok(TaskLifecycle::Archived),
        other => Err(StoreError::CodecMismatch {
            detail: format!("unknown tasks.lifecycle '{other}'"),
        }),
    }
}

fn parse_connectivity(value: &str) -> Result<TaskConnectivity, StoreError> {
    match value {
        "connected" => Ok(TaskConnectivity::Connected),
        "disconnected" => Ok(TaskConnectivity::Disconnected),
        other => Err(StoreError::CodecMismatch {
            detail: format!("unknown tasks.connectivity '{other}'"),
        }),
    }
}

fn parse_attention(value: &str) -> Result<TaskAttention, StoreError> {
    match value {
        "none" => Ok(TaskAttention::None),
        "needs_answer" => Ok(TaskAttention::NeedsAnswer),
        "needs_approval" => Ok(TaskAttention::NeedsApproval),
        "uncertain_outcome" => Ok(TaskAttention::UncertainOutcome),
        "failed" => Ok(TaskAttention::Failed),
        other => Err(StoreError::CodecMismatch {
            detail: format!("unknown tasks.attention '{other}'"),
        }),
    }
}

fn parse_activity(value: &str) -> Result<TaskActivity, StoreError> {
    match value {
        "idle" => Ok(TaskActivity::Idle),
        "working" => Ok(TaskActivity::Working),
        "settling" => Ok(TaskActivity::Settling),
        other => Err(StoreError::CodecMismatch {
            detail: format!("unknown tasks.activity '{other}'"),
        }),
    }
}

fn parse_review(value: &str) -> Result<ReviewReadiness, StoreError> {
    match value {
        "not_ready" => Ok(ReviewReadiness::NotReady),
        "ready" => Ok(ReviewReadiness::Ready),
        other => Err(StoreError::CodecMismatch {
            detail: format!("unknown tasks.review_readiness '{other}'"),
        }),
    }
}

fn parse_agent_lifecycle(value: &str) -> Result<AgentSessionLifecycle, StoreError> {
    match value {
        "open" => Ok(AgentSessionLifecycle::Open),
        "closing" => Ok(AgentSessionLifecycle::Closing),
        "closed" => Ok(AgentSessionLifecycle::Closed),
        other => Err(StoreError::CodecMismatch {
            detail: format!("unknown agent_sessions.lifecycle '{other}'"),
        }),
    }
}

fn parse_artifact_kind(value: &str) -> Result<ArtifactKind, StoreError> {
    match value {
        "specification" => Ok(ArtifactKind::Specification),
        "finding" => Ok(ArtifactKind::Finding),
        "decision" => Ok(ArtifactKind::Decision),
        "evidence" => Ok(ArtifactKind::Evidence),
        "review_report" => Ok(ArtifactKind::ReviewReport),
        other => Err(StoreError::CodecMismatch {
            detail: format!("unknown artifacts.kind '{other}'"),
        }),
    }
}

fn parse_privacy(value: &str) -> Result<PrivacyClass, StoreError> {
    match value {
        "local_only" => Ok(PrivacyClass::LocalOnly),
        "shareable" => Ok(PrivacyClass::Shareable),
        other => Err(StoreError::CodecMismatch {
            detail: format!("unknown artifacts.privacy_class '{other}'"),
        }),
    }
}

fn parse_owner_kind(value: &str) -> Result<OwnerKind, StoreError> {
    match value {
        "task" => Ok(OwnerKind::Task),
        "host" => Ok(OwnerKind::Host),
        other => Err(StoreError::CodecMismatch {
            detail: format!("unknown resources.owner_kind '{other}'"),
        }),
    }
}

fn parse_resource_kind(value: &str) -> Result<ResourceKind, StoreError> {
    match value {
        "terminal" => Ok(ResourceKind::Terminal),
        "browser_context" => Ok(ResourceKind::BrowserContext),
        "service" => Ok(ResourceKind::Service),
        other => Err(StoreError::CodecMismatch {
            detail: format!("unknown resources.resource_kind '{other}'"),
        }),
    }
}

fn parse_resource_lifecycle(value: &str) -> Result<ResourceLifecycle, StoreError> {
    match value {
        "active" => Ok(ResourceLifecycle::Active),
        "releasing" => Ok(ResourceLifecycle::Releasing),
        "released" => Ok(ResourceLifecycle::Released),
        other => Err(StoreError::CodecMismatch {
            detail: format!("unknown resources.lifecycle '{other}'"),
        }),
    }
}
