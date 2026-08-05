//! Pure and side-effect command acceptance: one IMMEDIATE transaction for lookup,
//! snapshot load, decide, plan, receipt, append, projection, and optional outbox.
//!
//! Side-effect acceptance does not claim settlement or dispatch external work.

use std::collections::BTreeMap;

use rusqlite::{OptionalExtension, Transaction};

use crate::domain::agent::{AgentRole, AgentSessionFacts, AgentSessionLifecycle};
use crate::domain::artifact::{ArtifactFacts, ArtifactKind, PrivacyClass};
use crate::domain::command::{decide, Command, CommandEnvelope, CommandReceipt, RejectionCode};
use crate::domain::event::{
    apply as apply_domain_event, DomainEvent, Event, OperationAcceptedFact, OperationCancelledFact,
    OperationFailedFact, OperationSettledFact, OperationUncertainFact, EVENT_SCHEMA_VERSION,
};
use crate::domain::id::{
    AgentSessionId, ArtifactId, CommandId, EnvironmentId, EventId, OperationId, OutboxId,
    ProjectId, ResourceId, TaskId,
};
use crate::domain::operation::{
    CancellationReason, OperationErrorCode, OperationOutcome, OperationOutcomeKind, OperationState,
    OperationUncertaintyCode, OutcomeSource, ResourceFence,
};
use crate::domain::resource::{OwnerKind, ResourceFacts, ResourceKind, ResourceLifecycle};
use crate::domain::snapshot::TaskSnapshot;
use crate::domain::task::{
    ReviewReadiness, TaskActivity, TaskAttention, TaskConnectivity, TaskFacts, TaskLifecycle,
};
use crate::kernel::outbox::{
    decode_effect_document, decode_receipt_document, encode_effect_document,
    encode_receipt_document, plan_effects, Effect, OperationFence, PlannedEffect,
};
use crate::kernel::projector;
use crate::kernel::store::{
    encode_event_payload, now_ms, u64_from_nonnegative_i64, u64_to_sqlite_i64, KernelStore,
    StoreError,
};

pub(crate) fn execute(
    store: &mut KernelStore,
    envelope: CommandEnvelope,
) -> Result<CommandReceipt, StoreError> {
    store.with_immediate_transaction(|tx| execute_in_tx(tx, envelope))
}

pub(crate) fn record_outcome(
    store: &mut KernelStore,
    outcome: OperationOutcome,
) -> Result<OperationState, StoreError> {
    store.with_immediate_transaction(|tx| record_outcome_in_tx(tx, outcome))
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
    let effect_doc = decode_effect_document(
        &outbox.payload,
        &outbox.destination_class,
        &outbox.replay_policy,
    )?;
    validate_effect_matches_fence(&effect_doc.effect, task_id, fence)?;

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
            if !outcome.source.is_dispatch() {
                return Err(StoreError::ConflictingOutcome);
            }
            match outbox.state.as_str() {
                "claimed" | "dispatching" => {
                    return Err(StoreError::InvalidDispatchTransition);
                }
                "pending" => {}
                _ => return Err(StoreError::Corruption),
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
                "pending",
            )
        }
        "uncertain" => {
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
    let document =
        decode_effect_document(&row.payload, &row.destination_class, &row.replay_policy)?;
    validate_effect_matches_fence(&document.effect, task_id, fence)?;
    require_current_effect_ownership(tx, task_id, &document.effect, fence)?;
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
    tx: &Transaction<'_>,
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
             'operation.cancelled', 'operation.uncertain'
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
        _ => false,
    }
}

fn load_operation_command_id(
    tx: &Transaction<'_>,
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
    tx: &Transaction<'_>,
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
    tx: &Transaction<'_>,
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
    }
    Ok(())
}

fn refuse_archive_with_live_resources(
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
    if matches!(outcome.source, OutcomeSource::Dispatch)
        && expected_outbox_state == "pending"
        && outbox.attempts > 0
    {
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
            let result_payload = match effect {
                Effect::BeginTaskTeardown { .. } => Event::TaskArchived,
                Effect::ReleaseResource { resource_fence, .. } => Event::ResourceReleased {
                    resource_id: resource_fence.resource_id,
                    runtime_generation: resource_fence.runtime_generation,
                },
            };
            let next_revision = next_task_revision(tx, task_id)?;
            append_and_project(
                tx,
                result_id,
                Some(task_id),
                Some(next_revision),
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
         SET state = ?1, leased_until_ms = NULL, last_error_class = ?2
         WHERE outbox_id = ?3 AND state = ?4 AND lease_generation = ?5",
        rusqlite::params![
            next_state,
            last_error_class,
            outbox.outbox_id.as_bytes().as_slice(),
            expected_state,
            outbox.lease_generation,
        ],
    )?;
    if changed != 1 {
        return Err(StoreError::InvalidDispatchTransition);
    }
    Ok(())
}

fn execute_in_tx(
    tx: &Transaction<'_>,
    envelope: CommandEnvelope,
) -> Result<CommandReceipt, StoreError> {
    if let Some(existing) = lookup_receipt(tx, envelope.command_id)? {
        return Ok(existing);
    }

    let accepted_at_ms = now_ms()?;
    let effective_task_id = effective_task_scope(&envelope);
    let snapshot = match effective_task_id {
        Some(task_id) => load_task_snapshot(tx, task_id)?,
        None => None,
    };
    let current_revision = snapshot.as_ref().map(|snap| snap.task.revision);

    match decide(snapshot.as_ref(), &envelope) {
        Err(code) => persist_rejection(
            tx,
            &envelope,
            effective_task_id,
            code,
            current_revision,
            accepted_at_ms,
        ),
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
                );
            }
            let Some(task_id) = effective_task_id else {
                return Err(StoreError::Projection(
                    "accepted commands require an effective task scope".into(),
                ));
            };
            let planned = plan_effects(snapshot.as_ref(), task_id, &decision)?;
            if planned.is_empty() {
                persist_pure_acceptance(
                    tx,
                    &envelope,
                    effective_task_id,
                    snapshot.as_ref(),
                    decision,
                    accepted_at_ms,
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
                )
            }
        }
    }
}

fn lookup_receipt(
    tx: &Transaction<'_>,
    command_id: CommandId,
) -> Result<Option<CommandReceipt>, StoreError> {
    let row: Option<(Vec<u8>, Option<Vec<u8>>, Option<i64>, i64)> = tx
        .query_row(
            "SELECT receipt, task_id, committed_sequence, created_at_ms
             FROM command_receipts WHERE command_id = ?1",
            [command_id.as_bytes().as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;
    let Some((payload, row_task_id, committed_sequence, created_at_ms)) = row else {
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
    Ok(Some(receipt))
}

fn validate_accepted_receipt_correlation(
    tx: &Transaction<'_>,
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
    let Some(scope) = receipt_task_id else {
        return Err(StoreError::Corruption);
    };
    let Some(receipt_final_revision) = receipt_task_revision else {
        return Err(StoreError::Corruption);
    };

    let operation = load_operation_projection(tx, command_id)?;
    if operation.operation_id != expected_operation_id {
        return Err(StoreError::Corruption);
    }
    if operation.task_id != Some(scope) {
        return Err(StoreError::Corruption);
    }
    if receipt_created_at_ms != operation.accepted_at_ms {
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

fn validate_pure_accepted_receipt(
    tx: &Transaction<'_>,
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
        scope,
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
        scope,
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
    tx: &Transaction<'_>,
    command_id: CommandId,
    expected_operation_id: OperationId,
    event_ids: &[EventId],
    receipt_final_revision: u64,
    scope: TaskId,
    committed_sequence: u64,
    operation: &OperationProjectionRow,
    outbox_rows: &[OutboxRow],
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

    let expected_effects =
        effects_from_durable_decision_facts(tx, scope, first_decision_sequence, &decision_facts)?;
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
        scope,
        operation.accepted_at_ms,
        fence,
    )?;
    ensure_unique_operation_accepted_fact(
        tx,
        command_id,
        expected_operation_id,
        scope,
        operation.accepted_at_ms,
        committed_sequence,
        &accepted_row,
        fence,
    )?;
    // Durable task mutation chain is the source of truth for revision/lifecycle.
    let _ = validate_task_history_and_projection(tx, scope)?;

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

fn validate_side_effect_active_outbox(
    outbox_rows: &[OutboxRow],
    expected_effects: &[PlannedEffect],
    expected_operation_id: OperationId,
    committed_sequence: u64,
    accepted_at_ms: i64,
    scope: TaskId,
    fence: OperationFence,
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
        match row.state.as_str() {
            "pending" | "claimed" | "dispatching" => {
                validate_nonterminal_outbox_dispatch_metadata(row, accepted_at_ms)?;
            }
            // e2 reserved names (reconcile_required/reconciling/uncertain) are never
            // legitimate on an accepted-operation receipt in this slice.
            _ => return Err(StoreError::Corruption),
        }
        if row.state == "pending" && row.last_error_class.is_some() {
            return Err(StoreError::Corruption);
        }
        let decoded =
            decode_effect_document(&row.payload, &row.destination_class, &row.replay_policy)?;
        if decoded != planned.document {
            return Err(StoreError::Corruption);
        }
        validate_effect_matches_fence(&decoded.effect, scope, fence)?;
    }
    Ok(())
}

fn validate_side_effect_terminal_receipt(
    tx: &Transaction<'_>,
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
        )?;
        let decoded =
            decode_effect_document(&row.payload, &row.destination_class, &row.replay_policy)?;
        if decoded != planned.document {
            return Err(StoreError::Corruption);
        }
        validate_effect_matches_fence(&decoded.effect, scope, fence)?;
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
) -> Result<(), StoreError> {
    if row.lease_generation < 0 {
        return Err(StoreError::Corruption);
    }
    if row.attempts < 0 {
        return Err(StoreError::Corruption);
    }
    match row.state.as_str() {
        "pending" => {
            if row.attempts == 0 {
                if row.dispatch_started_at_ms.is_some()
                    || row.leased_until_ms.is_some()
                    || (row.lease_generation == 0 && row.available_at_ms != accepted_at_ms)
                    || (row.lease_generation > 0 && row.available_at_ms < accepted_at_ms)
                {
                    return Err(StoreError::Corruption);
                }
            } else {
                let Some(started) = row.dispatch_started_at_ms else {
                    return Err(StoreError::Corruption);
                };
                if row.available_at_ms < accepted_at_ms || row.available_at_ms > started {
                    return Err(StoreError::Corruption);
                }
                if let Some(lease) = row.leased_until_ms {
                    if lease < started {
                        return Err(StoreError::Corruption);
                    }
                }
            }
        }
        "claimed" => {
            if row.lease_generation <= 0
                || row.attempts != 0
                || row.leased_until_ms.is_none()
                || row.dispatch_started_at_ms.is_some()
                || row.available_at_ms < accepted_at_ms
                || row.last_error_class.is_some()
            {
                return Err(StoreError::Corruption);
            }
            if row.leased_until_ms.ok_or(StoreError::Corruption)? <= row.available_at_ms {
                return Err(StoreError::Corruption);
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
            if lease <= started {
                return Err(StoreError::Corruption);
            }
        }
        _ => return Err(StoreError::Corruption),
    }
    if row.reconciliation_receipt.is_some() {
        return Err(StoreError::Corruption);
    }
    Ok(())
}

fn validate_terminal_outbox_dispatch_metadata(
    row: &OutboxRow,
    accepted_at_ms: i64,
    dispatch_upper_bound_ms: i64,
) -> Result<(), StoreError> {
    if row.attempts < 0 {
        return Err(StoreError::Corruption);
    }
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
        // accepted_at <= available_at <= dispatch_started <= uncertain_or_final
        if row.available_at_ms < accepted_at_ms
            || row.available_at_ms > started
            || started > dispatch_upper_bound_ms
        {
            return Err(StoreError::Corruption);
        }
    }
    Ok(())
}

fn validate_terminal_outcome_history(
    tx: &Transaction<'_>,
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
    tx: &Transaction<'_>,
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
    tx: &Transaction<'_>,
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
    tx: &Transaction<'_>,
    scope: TaskId,
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
    if snap != projected {
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
    tx: &Transaction<'_>,
    sequence: u64,
) -> Result<Option<(EventId, Event, Option<u64>, i64, Option<TaskId>)>, StoreError> {
    load_global_event_adjacent(tx, sequence, /*after=*/ false)
}

fn load_global_event_after(
    tx: &Transaction<'_>,
    sequence: u64,
) -> Result<Option<(EventId, Event, Option<u64>, i64, Option<TaskId>)>, StoreError> {
    load_global_event_adjacent(tx, sequence, /*after=*/ true)
}

fn load_global_event_adjacent(
    tx: &Transaction<'_>,
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
                TaskLifecycle::Open | TaskLifecycle::Closing
            ) {
                return Err(StoreError::Corruption);
            }
        }
        Event::AgentSessionRegistered { .. }
        | Event::PrimaryAgentSet { .. }
        | Event::ResourceRegistered { .. } => {
            if snap.task.lifecycle != TaskLifecycle::Open {
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
    tx: &Transaction<'_>,
    scope: TaskId,
    first_decision_sequence: u64,
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
            _ => return Err(StoreError::Corruption),
        }
    }
    Ok(planned)
}

/// Replay task mutations through `through_sequence` and return the resulting action_epoch.
fn historical_action_epoch_through(
    tx: &Transaction<'_>,
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
    scope: TaskId,
    accepted_at_ms: i64,
    fence: OperationFence,
) -> Result<(), StoreError> {
    if accepted_row.task_id != Some(scope) || accepted_row.task_revision.is_some() {
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
fn ensure_unique_operation_accepted_fact(
    tx: &Transaction<'_>,
    command_id: CommandId,
    expected_operation_id: OperationId,
    scope: TaskId,
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

fn validate_decision_event_batch(
    tx: &Transaction<'_>,
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
    tx: &Transaction<'_>,
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
    tx: &Transaction<'_>,
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
    tx: &Transaction<'_>,
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
    tx: &Transaction<'_>,
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
    tx: &Transaction<'_>,
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
}

fn load_outbox_rows(
    tx: &Transaction<'_>,
    operation_id: OperationId,
) -> Result<Vec<OutboxRow>, StoreError> {
    let mut stmt = tx.prepare(
        "SELECT outbox_id, operation_id, effect_index, event_sequence, destination_class,
                replay_policy, payload, state, available_at_ms, leased_until_ms,
                dispatch_started_at_ms, attempts, last_error_class, lease_generation,
                reconciliation_receipt
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
    )> = tx
        .query_row(
            "SELECT operation_id, effect_index, event_sequence, destination_class,
                    replay_policy, payload, state, available_at_ms, leased_until_ms,
                    dispatch_started_at_ms, attempts, last_error_class, lease_generation,
                    reconciliation_receipt
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
    }))
}

fn load_event_row_at_sequence(tx: &Transaction<'_>, sequence: u64) -> Result<EventRow, StoreError> {
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

fn is_pure_slice_decision_fact(event: &Event) -> bool {
    matches!(
        event,
        Event::TaskCreated { .. }
            | Event::TaskRenamed { .. }
            | Event::TaskAttentionSet { .. }
            | Event::TaskReopened
            | Event::AgentSessionRegistered { .. }
            | Event::PrimaryAgentSet { .. }
            | Event::ArtifactRegistered { .. }
            | Event::ResourceRegistered { .. }
    )
}

fn is_side_effect_decision_fact(event: &Event) -> bool {
    matches!(
        event,
        Event::TaskCloseBegun { .. } | Event::ResourceReleaseBegun { .. }
    )
}

fn validate_rejected_receipt_correlation(
    tx: &Transaction<'_>,
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
) -> Result<CommandReceipt, StoreError> {
    let receipt = CommandReceipt::Rejected {
        command_id: envelope.command_id,
        code,
        current_revision,
    };
    insert_receipt_row(
        tx,
        envelope,
        effective_task_id,
        &receipt,
        None,
        created_at_ms,
    )?;
    Ok(receipt)
}

fn persist_pure_acceptance(
    tx: &Transaction<'_>,
    envelope: &CommandEnvelope,
    effective_task_id: Option<TaskId>,
    snapshot: Option<&TaskSnapshot>,
    decision: Vec<Event>,
    accepted_at_ms: i64,
) -> Result<CommandReceipt, StoreError> {
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
    };
    insert_receipt_row(
        tx,
        envelope,
        effective_task_id,
        &receipt,
        None,
        accepted_at_ms,
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

    let operation_id = OperationId::new();
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
    };
    insert_receipt_row(tx, envelope, Some(task_id), &receipt, None, accepted_at_ms)?;

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
) -> Result<(), StoreError> {
    let payload = encode_receipt_document(receipt)?;
    tx.execute(
        "INSERT INTO command_receipts(
            command_id, client_id, task_id, receipt, committed_sequence, created_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
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
        ],
    )?;
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

fn load_task_snapshot(
    tx: &Transaction<'_>,
    task_id: TaskId,
) -> Result<Option<TaskSnapshot>, StoreError> {
    let Some(task_row) = load_task_row(tx, task_id)? else {
        return Ok(None);
    };

    let agents = load_agents(tx, task_id)?;
    let artifacts = load_artifacts(tx, task_id)?;
    let resources = load_resources(tx, task_id)?;

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
                ))
            }
        }
    }

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
    }))
}

struct LoadedTaskRow {
    task: TaskFacts,
    connectivity: TaskConnectivity,
    attention: TaskAttention,
    activity: TaskActivity,
    review_readiness: ReviewReadiness,
    primary_agent_id: Option<AgentSessionId>,
}

fn load_task_row(
    tx: &Transaction<'_>,
    task_id: TaskId,
) -> Result<Option<LoadedTaskRow>, StoreError> {
    let row = tx
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

fn load_agents(
    tx: &Transaction<'_>,
    task_id: TaskId,
) -> Result<BTreeMap<AgentSessionId, AgentSessionFacts>, StoreError> {
    let mut stmt = tx.prepare(
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
        agents.insert(id, agent);
    }
    Ok(agents)
}

fn load_artifacts(
    tx: &Transaction<'_>,
    task_id: TaskId,
) -> Result<BTreeMap<ArtifactId, ArtifactFacts>, StoreError> {
    let mut stmt = tx.prepare(
        "SELECT artifact_id, kind, label, content_ref, sha256, privacy_class, created_at_ms
         FROM artifacts WHERE task_id = ?1 ORDER BY artifact_id ASC",
    )?;
    let rows = stmt.query_map([task_id.as_bytes().as_slice()], |row| {
        Ok((
            row.get::<_, Vec<u8>>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, Vec<u8>>(3)?,
            row.get::<_, Vec<u8>>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, i64>(6)?,
        ))
    })?;
    let mut artifacts = BTreeMap::new();
    for row in rows {
        let (id_bytes, kind, label, content_ref, sha256, privacy_class, created_at_ms) = row?;
        let id = id16::<ArtifactId>("artifacts.artifact_id", &id_bytes)?;
        let sha256_array: [u8; 32] =
            sha256
                .as_slice()
                .try_into()
                .map_err(|_| StoreError::CodecMismatch {
                    detail: "artifacts.sha256 must be 32 bytes".into(),
                })?;
        let artifact = ArtifactFacts {
            id,
            task_id,
            kind: parse_artifact_kind(&kind)?,
            label,
            content_ref: unpack_projection_blob("artifacts.content_ref", &content_ref)?,
            sha256: sha256_array,
            privacy_class: parse_privacy(&privacy_class)?,
            created_at_ms,
        };
        artifact
            .validate()
            .map_err(|err| StoreError::Projection(err.to_string()))?;
        artifacts.insert(id, artifact);
    }
    Ok(artifacts)
}

fn load_resources(
    tx: &Transaction<'_>,
    task_id: TaskId,
) -> Result<BTreeMap<ResourceId, ResourceFacts>, StoreError> {
    let mut stmt = tx.prepare(
        "SELECT resource_id, owner_kind, resource_kind, recipe, lifecycle,
                runtime_generation, updated_at_ms
         FROM resources WHERE task_id = ?1 ORDER BY resource_id ASC",
    )?;
    let rows = stmt.query_map([task_id.as_bytes().as_slice()], |row| {
        Ok((
            row.get::<_, Vec<u8>>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, Vec<u8>>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, i64>(5)?,
            row.get::<_, i64>(6)?,
        ))
    })?;
    let mut resources = BTreeMap::new();
    for row in rows {
        let (
            id_bytes,
            owner_kind,
            resource_kind,
            recipe,
            lifecycle,
            runtime_generation,
            updated_at_ms,
        ) = row?;
        let id = id16::<ResourceId>("resources.resource_id", &id_bytes)?;
        let resource = ResourceFacts {
            id,
            task_id: Some(task_id),
            owner_kind: parse_owner_kind(&owner_kind)?,
            resource_kind: parse_resource_kind(&resource_kind)?,
            recipe: unpack_projection_blob("resources.recipe", &recipe)?,
            lifecycle: parse_resource_lifecycle(&lifecycle)?,
            runtime_generation: u64_from_nonnegative_i64(
                "resources.runtime_generation",
                runtime_generation,
            )?,
            updated_at_ms,
        };
        resource
            .validate()
            .map_err(|err| StoreError::Projection(err.to_string()))?;
        resources.insert(id, resource);
    }
    Ok(resources)
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
