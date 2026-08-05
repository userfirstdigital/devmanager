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
    DomainEvent, Event, OperationAcceptedFact, OperationSettledFact, EVENT_SCHEMA_VERSION,
};
use crate::domain::id::{
    AgentSessionId, ArtifactId, CommandId, EnvironmentId, EventId, OperationId, OutboxId,
    ProjectId, ResourceId, TaskId,
};
use crate::domain::operation::ResourceFence;
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
    let row: Option<(Vec<u8>, Option<Vec<u8>>, Option<i64>)> = tx
        .query_row(
            "SELECT receipt, task_id, committed_sequence
             FROM command_receipts WHERE command_id = ?1",
            [command_id.as_bytes().as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((payload, row_task_id, committed_sequence)) = row else {
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

    let decision_facts = validate_decision_event_batch(
        tx,
        event_ids,
        scope,
        receipt_final_revision,
        first_decision_sequence,
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
    if settled_fact.command_id != command_id
        || settled_fact.operation_id != expected_operation_id
        || settled_fact.result_event_ids.as_slice() != event_ids
        || settled_fact.action_epoch.is_some()
        || settled_fact.resource_id.is_some()
        || settled_fact.runtime_generation.is_some()
        || settled_fact.source != crate::domain::operation::OutcomeSource::Dispatch
        || settled_fact.settled_at_ms != outcome_at_ms
    {
        return Err(StoreError::Corruption);
    }
    let _ = decision_facts;
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

    if operation.state != "accepted"
        || operation.result.is_some()
        || operation.outcome_code.is_some()
        || operation.outcome_at_ms.is_some()
    {
        return Err(StoreError::Corruption);
    }

    let decision_facts = validate_decision_event_batch(
        tx,
        event_ids,
        scope,
        receipt_final_revision,
        first_decision_sequence,
        is_side_effect_decision_fact,
    )?;

    let expected_effects = effects_from_durable_decision_facts(scope, &decision_facts, operation)?;
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

    // Contiguous zero-based indexes; no gaps/extras.
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
        if row.state != "pending"
            || row.attempts != 0
            || row.leased_until_ms.is_some()
            || row.dispatch_started_at_ms.is_some()
            || row.last_error_class.is_some()
            || row.available_at_ms != operation.accepted_at_ms
        {
            return Err(StoreError::Corruption);
        }
        let decoded =
            decode_effect_document(&row.payload, &row.destination_class, &row.replay_policy)?;
        if decoded != planned.document {
            return Err(StoreError::Corruption);
        }
        match &decoded.effect {
            Effect::BeginTaskTeardown {
                task_id,
                action_epoch,
            } => {
                if *task_id != scope || Some(*action_epoch) != fence.action_epoch {
                    return Err(StoreError::Corruption);
                }
            }
            Effect::ReleaseResource {
                task_id,
                action_epoch,
                resource_fence,
            } => {
                if *task_id != scope
                    || Some(*action_epoch) != fence.action_epoch
                    || Some(resource_fence.resource_id) != fence.resource_id
                    || Some(resource_fence.runtime_generation) != fence.runtime_generation
                {
                    return Err(StoreError::Corruption);
                }
            }
        }
    }
    Ok(())
}

fn effects_from_durable_decision_facts(
    scope: TaskId,
    decision_facts: &[(Event, u64)],
    operation: &OperationProjectionRow,
) -> Result<Vec<PlannedEffect>, StoreError> {
    let action_epoch = match operation.action_epoch {
        Some(v) => Some(u64_from_nonnegative_i64("operations.action_epoch", v)?),
        None => None,
    };
    let mut planned = Vec::new();
    for (fact, _) in decision_facts {
        match fact {
            Event::TaskCloseBegun {
                action_epoch: epoch,
            } => {
                if Some(*epoch) != action_epoch || action_epoch.is_none() {
                    return Err(StoreError::Corruption);
                }
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
                let Some(epoch) = action_epoch else {
                    return Err(StoreError::Corruption);
                };
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
    {
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

    let projection_revision = load_task_projection_revision(tx, scope)?;
    if projection_revision < receipt_final_revision {
        return Err(StoreError::Corruption);
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

fn load_task_projection_revision(tx: &Transaction<'_>, scope: TaskId) -> Result<u64, StoreError> {
    let revision: Option<i64> = tx
        .query_row(
            "SELECT revision FROM tasks WHERE task_id = ?1",
            [scope.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .optional()?;
    let Some(revision) = revision else {
        return Err(StoreError::Corruption);
    };
    u64_from_nonnegative_i64("tasks.revision", revision)
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
}

struct OutboxRow {
    #[allow(dead_code)] // validated on read; retained for later dispatch slices
    outbox_id: OutboxId,
    operation_id: OperationId,
    effect_index: i64,
    event_sequence: u64,
    destination_class: String,
    replay_policy: String,
    payload: Vec<u8>,
    state: String,
    available_at_ms: i64,
    leased_until_ms: Option<i64>,
    dispatch_started_at_ms: Option<i64>,
    attempts: i64,
    last_error_class: Option<String>,
}

fn load_outbox_rows(
    tx: &Transaction<'_>,
    operation_id: OperationId,
) -> Result<Vec<OutboxRow>, StoreError> {
    let mut stmt = tx.prepare(
        "SELECT outbox_id, operation_id, effect_index, event_sequence, destination_class,
                replay_policy, payload, state, available_at_ms, leased_until_ms,
                dispatch_started_at_ms, attempts, last_error_class
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
        });
    }
    Ok(out)
}

fn load_event_row_at_sequence(tx: &Transaction<'_>, sequence: u64) -> Result<EventRow, StoreError> {
    let row: Option<(Vec<u8>, Option<Vec<u8>>, Option<i64>, String, i64, Vec<u8>)> = tx
        .query_row(
            "SELECT event_id, task_id, task_revision, event_type, schema_version, payload
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
                ))
            },
        )
        .optional()?;
    let Some((event_id_bytes, task_bytes, task_revision, event_type, schema_version, payload)) =
        row
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
