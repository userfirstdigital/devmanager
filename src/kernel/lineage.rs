//! Shared derived-result / operation settled adjacency rules for rebuild and
//! strict task-history validation. One coherent model; callers map to
//! Corruption (runtime integrity) or Projection (rebuild/projector).

use crate::domain::event::{Event, OperationSettledFact};
use crate::domain::id::{EventId, ResourceId, TaskId};
use crate::kernel::store::StoreError;

fn mismatch(as_projection: bool, detail: &str) -> StoreError {
    if as_projection {
        StoreError::Projection(detail.into())
    } else {
        StoreError::Corruption
    }
}

/// Supported settled/accepted fence shapes for V1 lineage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SettledLineageKind {
    Pure,
    TaskTeardown,
    HostAdmission,
    Release {
        resource_id: ResourceId,
        runtime_generation: u64,
    },
}

/// Pure = all-none; teardown = action + task scope; host admission = action + global scope;
/// release = action + both resource fields. Any other combination fails closed.
pub(crate) fn classify_settled_lineage_fence(
    action_epoch: Option<u64>,
    resource_id: Option<ResourceId>,
    runtime_generation: Option<u64>,
    task_id: Option<TaskId>,
    as_projection: bool,
) -> Result<SettledLineageKind, StoreError> {
    match (action_epoch, resource_id, runtime_generation, task_id) {
        (None, None, None, _) => Ok(SettledLineageKind::Pure),
        (Some(_), None, None, Some(_)) => Ok(SettledLineageKind::TaskTeardown),
        (Some(_), None, None, None) => Ok(SettledLineageKind::HostAdmission),
        (Some(_), Some(resource_id), Some(runtime_generation), Some(_)) => {
            Ok(SettledLineageKind::Release {
                resource_id,
                runtime_generation,
            })
        }
        _ => Err(mismatch(
            as_projection,
            "unsupported operation fence shape for lineage",
        )),
    }
}

pub(crate) fn classify_operation_settled_fact(
    fact: &OperationSettledFact,
    task_id: Option<TaskId>,
    as_projection: bool,
) -> Result<SettledLineageKind, StoreError> {
    classify_settled_lineage_fence(
        fact.action_epoch,
        fact.resource_id,
        fact.runtime_generation,
        task_id,
        as_projection,
    )
}

pub(crate) fn is_derived_lifecycle_result(event: &Event) -> bool {
    matches!(event, Event::TaskArchived | Event::ResourceReleased { .. })
}

/// Forward + reverse adjacency for a derived lifecycle result immediately
/// followed by its OperationSettled (or reject orphan / missing / wrong pair).
pub(crate) fn validate_derived_settled_adjacency(
    derived_id: EventId,
    derived: &Event,
    derived_revision: Option<u64>,
    derived_occurred_at_ms: i64,
    derived_task_id: Option<TaskId>,
    next: Option<(EventId, &Event, Option<u64>, i64, Option<TaskId>)>,
    as_projection: bool,
) -> Result<(), StoreError> {
    let Some((settled_id, next_event, next_revision, next_occurred_at_ms, next_task_id)) = next
    else {
        return Err(mismatch(
            as_projection,
            "derived lifecycle result is missing immediately following operation.settled",
        ));
    };
    let Event::OperationSettled(fact) = next_event else {
        return Err(mismatch(
            as_projection,
            "derived lifecycle result must be followed immediately by operation.settled",
        ));
    };
    let kind = classify_operation_settled_fact(fact, next_task_id, as_projection)?;
    if matches!(
        kind,
        SettledLineageKind::Pure | SettledLineageKind::HostAdmission
    ) {
        return Err(mismatch(
            as_projection,
            "derived lifecycle result must be followed by a side-effect operation.settled",
        ));
    }
    if next_revision.is_some() {
        return Err(mismatch(
            as_projection,
            "operation.settled must have NULL task_revision",
        ));
    }
    let _ = settled_id;
    validate_side_effect_settled_against_derived(
        fact,
        next_occurred_at_ms,
        next_task_id,
        derived_id,
        derived,
        derived_revision,
        derived_occurred_at_ms,
        derived_task_id,
        as_projection,
    )
}

/// Forward check used when the settled fact is the current event.
pub(crate) fn validate_side_effect_settled_against_derived(
    fact: &OperationSettledFact,
    settled_occurred_at_ms: i64,
    settled_task_id: Option<TaskId>,
    derived_id: EventId,
    derived: &Event,
    derived_revision: Option<u64>,
    derived_occurred_at_ms: i64,
    derived_task_id: Option<TaskId>,
    as_projection: bool,
) -> Result<(), StoreError> {
    let kind = classify_operation_settled_fact(fact, settled_task_id, as_projection)?;
    if matches!(kind, SettledLineageKind::Pure) {
        return Ok(());
    }
    if matches!(kind, SettledLineageKind::HostAdmission) {
        return Err(mismatch(
            as_projection,
            "host-admission settlement is not permitted until branch-aware settlement",
        ));
    }
    if fact.result_event_ids.len() != 1 || fact.result_event_ids[0] != derived_id {
        return Err(mismatch(
            as_projection,
            "side-effect operation.settled must reference exactly one immediately preceding derived result",
        ));
    }
    if settled_task_id != derived_task_id {
        return Err(mismatch(
            as_projection,
            "derived result and operation.settled task scope mismatch",
        ));
    }
    if settled_occurred_at_ms != derived_occurred_at_ms
        || settled_occurred_at_ms != fact.settled_at_ms
    {
        return Err(mismatch(
            as_projection,
            "derived result and operation.settled occurred_at mismatch",
        ));
    }
    let Some(_revision) = derived_revision else {
        return Err(mismatch(
            as_projection,
            "derived lifecycle result requires non-null task_revision",
        ));
    };
    match (kind, derived) {
        (SettledLineageKind::TaskTeardown, Event::TaskArchived) => Ok(()),
        (
            SettledLineageKind::Release {
                resource_id,
                runtime_generation,
            },
            Event::ResourceReleased {
                resource_id: derived_resource,
                runtime_generation: derived_generation,
            },
        ) if resource_id == *derived_resource && runtime_generation == *derived_generation => {
            Ok(())
        }
        (SettledLineageKind::TaskTeardown, _) => Err(mismatch(
            as_projection,
            "task teardown settle requires immediately preceding task.archived",
        )),
        (SettledLineageKind::Release { .. }, _) => Err(mismatch(
            as_projection,
            "resource release settle requires matching immediately preceding resource.released",
        )),
        (SettledLineageKind::Pure | SettledLineageKind::HostAdmission, _) => {
            unreachable!("pure returned early; host admission rejected above")
        }
    }
}

/// When current is a side-effect settled fact, require the immediate prior event
/// to be the matching derived result. Malformed fences fail closed (never pure).
pub(crate) fn validate_side_effect_settled_has_prior_derived(
    fact: &OperationSettledFact,
    settled_occurred_at_ms: i64,
    settled_task_id: Option<TaskId>,
    prior: Option<(EventId, &Event, Option<u64>, i64, Option<TaskId>)>,
    as_projection: bool,
) -> Result<(), StoreError> {
    let kind = classify_operation_settled_fact(fact, settled_task_id, as_projection)?;
    if matches!(kind, SettledLineageKind::Pure) {
        return Ok(());
    }
    if matches!(kind, SettledLineageKind::HostAdmission) {
        return Err(mismatch(
            as_projection,
            "host-admission settlement is not permitted until branch-aware settlement",
        ));
    }
    let Some((derived_id, derived, derived_revision, derived_occurred, derived_task)) = prior
    else {
        return Err(mismatch(
            as_projection,
            "side-effect operation.settled is missing immediately preceding derived result",
        ));
    };
    if !is_derived_lifecycle_result(derived) {
        return Err(mismatch(
            as_projection,
            "side-effect operation.settled prior event is not a derived lifecycle result",
        ));
    }
    validate_side_effect_settled_against_derived(
        fact,
        settled_occurred_at_ms,
        settled_task_id,
        derived_id,
        derived,
        derived_revision,
        derived_occurred,
        derived_task,
        as_projection,
    )
}

/// Pure all-none operations are synchronous Dispatch settles only.
pub(crate) fn reject_pure_non_settled_terminal(
    kind: SettledLineageKind,
    as_projection: bool,
) -> Result<(), StoreError> {
    if matches!(
        kind,
        SettledLineageKind::Pure | SettledLineageKind::HostAdmission
    ) {
        return Err(mismatch(
            as_projection,
            "pure/host-admission operation cannot become failed, cancelled, or uncertain",
        ));
    }
    Ok(())
}

fn is_pure_decision_fact(event: &Event) -> bool {
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

/// Validate a Pure OperationSettled against the contiguous decision facts immediately
/// before its OperationAccepted. Callers supply already-loaded bounded rows.
pub(crate) fn validate_pure_settled_lineage(
    fact: &OperationSettledFact,
    settled_occurred_at_ms: i64,
    settled_task_id: Option<TaskId>,
    accepted_at_ms: i64,
    accepted: Option<(EventId, &Event, Option<u64>, i64, Option<TaskId>)>,
    decisions: &[(EventId, Event, Option<u64>, i64, Option<TaskId>)],
    as_projection: bool,
) -> Result<(), StoreError> {
    let kind = classify_operation_settled_fact(fact, settled_task_id, as_projection)?;
    if !matches!(kind, SettledLineageKind::Pure) {
        return Ok(());
    }
    if !fact.source.is_dispatch() {
        return Err(mismatch(
            as_projection,
            "pure operation.settled requires Dispatch source",
        ));
    }
    if settled_occurred_at_ms != fact.settled_at_ms || fact.settled_at_ms != accepted_at_ms {
        return Err(mismatch(
            as_projection,
            "pure operation.settled must equal accepted_at_ms",
        ));
    }
    let Some((
        _accepted_id,
        Event::OperationAccepted(accepted_fact),
        accepted_revision,
        accepted_occurred,
        accepted_task,
    )) = accepted
    else {
        return Err(mismatch(
            as_projection,
            "pure operation.settled requires immediately preceding operation.accepted",
        ));
    };
    if accepted_revision.is_some()
        || accepted_occurred != accepted_at_ms
        || accepted_task != settled_task_id
        || accepted_fact.operation_id != fact.operation_id
        || accepted_fact.command_id != fact.command_id
        || accepted_fact.accepted_at_ms != accepted_at_ms
        || accepted_fact.action_epoch.is_some()
        || accepted_fact.resource_id.is_some()
        || accepted_fact.runtime_generation.is_some()
    {
        return Err(mismatch(
            as_projection,
            "pure operation.accepted fence/time/scope mismatch",
        ));
    }
    if decisions.len() != fact.result_event_ids.len() || decisions.is_empty() {
        return Err(mismatch(
            as_projection,
            "pure operation.settled result_event_ids must match contiguous prior decision facts",
        ));
    }
    let mut previous_revision: Option<u64> = None;
    for (idx, (event_id, event, revision, occurred_at, task_id)) in decisions.iter().enumerate() {
        if *event_id != fact.result_event_ids[idx] {
            return Err(mismatch(
                as_projection,
                "pure settle result_event_ids must identify exact prior decision event ids",
            ));
        }
        if *task_id != settled_task_id || *occurred_at != accepted_at_ms {
            return Err(mismatch(
                as_projection,
                "pure decision facts must share task scope and accepted time",
            ));
        }
        if !is_pure_decision_fact(event) || !event.is_task_mutation() {
            return Err(mismatch(
                as_projection,
                "pure settle decisions must be contiguous task-mutation decision facts",
            ));
        }
        let Some(rev) = revision else {
            return Err(mismatch(
                as_projection,
                "pure decision facts require task_revision",
            ));
        };
        match previous_revision {
            None => previous_revision = Some(*rev),
            Some(prev) => {
                let expected = prev
                    .checked_add(1)
                    .ok_or_else(|| mismatch(as_projection, "pure decision revision overflow"))?;
                if *rev != expected {
                    return Err(mismatch(
                        as_projection,
                        "pure decision facts require contiguous revisions",
                    ));
                }
                previous_revision = Some(*rev);
            }
        }
    }
    Ok(())
}
