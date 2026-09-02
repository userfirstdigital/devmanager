//! Shared derived-result / operation settled adjacency rules for rebuild and
//! strict task-history validation. One coherent model; callers map to
//! Corruption (runtime integrity) or Projection (rebuild/projector).

use crate::domain::event::{Event, OperationFailedFact, OperationSettledFact};
use crate::domain::host::{HostCleanupBranch, HostCleanupBranchOutcome};
use crate::domain::id::{CommandId, EventId, OperationId, ResourceId, TaskId};
use crate::domain::operation::OperationErrorCode;
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
    PromptLibrary,
    /// Task-scoped action-only side effects are distinguished by their
    /// immediately preceding derived result (`task.archived` or
    /// `provider_input.delivered`), not by an impossible partial resource
    /// fence.
    TaskScopedSideEffect,
    HostAdmission,
    Release {
        resource_id: ResourceId,
        runtime_generation: u64,
    },
}

/// Pure = task-scoped all-none fence (task-mutation adjacency).
/// PromptLibrary = host-scoped all-none fence (accepted+settled only; never task decisions).
/// TaskScopedSideEffect = action + task scope; host admission = action + global scope;
/// release = action + both resource fields. Any other combination fails closed.
pub(crate) fn classify_settled_lineage_fence(
    action_epoch: Option<u64>,
    resource_id: Option<ResourceId>,
    runtime_generation: Option<u64>,
    task_id: Option<TaskId>,
    as_projection: bool,
) -> Result<SettledLineageKind, StoreError> {
    match (action_epoch, resource_id, runtime_generation, task_id) {
        (None, None, None, Some(_)) => Ok(SettledLineageKind::Pure),
        (None, None, None, None) => Ok(SettledLineageKind::PromptLibrary),
        (Some(_), None, None, Some(_)) => Ok(SettledLineageKind::TaskScopedSideEffect),
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
    matches!(
        event,
        Event::TaskArchived | Event::ResourceReleased { .. } | Event::ProviderInputDelivered { .. }
    )
}

/// Forward + reverse adjacency for a derived side-effect result immediately
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
        SettledLineageKind::Pure
            | SettledLineageKind::PromptLibrary
            | SettledLineageKind::HostAdmission
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
    if matches!(
        kind,
        SettledLineageKind::Pure | SettledLineageKind::PromptLibrary
    ) {
        return Ok(());
    }
    if matches!(kind, SettledLineageKind::HostAdmission) {
        return Err(mismatch(
            as_projection,
            "host-admission settlement is not derived from a lifecycle result",
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
    match (kind, derived) {
        (
            SettledLineageKind::TaskScopedSideEffect,
            Event::ProviderInputDelivered {
                command_id,
                operation_id,
                action_epoch,
                ..
            },
        ) if derived_revision.is_none()
            && *command_id == fact.command_id
            && *operation_id == fact.operation_id
            && fact.action_epoch == Some(*action_epoch) =>
        {
            Ok(())
        }
        (SettledLineageKind::TaskScopedSideEffect, Event::ProviderInputDelivered { .. }) => {
            Err(mismatch(
                as_projection,
                "provider input delivery and operation.settled identity mismatch",
            ))
        }
        (SettledLineageKind::TaskScopedSideEffect, Event::TaskArchived)
            if derived_revision.is_some() =>
        {
            Ok(())
        }
        (
            SettledLineageKind::Release {
                resource_id,
                runtime_generation,
            },
            Event::ResourceReleased {
                resource_id: derived_resource,
                runtime_generation: derived_generation,
            },
        ) if derived_revision.is_some()
            && resource_id == *derived_resource
            && runtime_generation == *derived_generation =>
        {
            Ok(())
        }
        (SettledLineageKind::TaskScopedSideEffect, _) => Err(mismatch(
            as_projection,
            "task-scoped side-effect settle requires matching immediately preceding derived result",
        )),
        (SettledLineageKind::Release { .. }, _) => Err(mismatch(
            as_projection,
            "resource release settle requires matching immediately preceding resource.released",
        )),
        (
            SettledLineageKind::Pure
            | SettledLineageKind::PromptLibrary
            | SettledLineageKind::HostAdmission,
            _,
        ) => {
            unreachable!("pure/prompt-library returned early; host admission rejected above")
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
    if matches!(
        kind,
        SettledLineageKind::Pure | SettledLineageKind::PromptLibrary
    ) {
        return Ok(());
    }
    if matches!(kind, SettledLineageKind::HostAdmission) {
        return Err(mismatch(
            as_projection,
            "host-admission settlement is not derived from a lifecycle result",
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
/// Host-admission failure is permitted only through the exact CleanupFailed journal path.
pub(crate) fn reject_pure_non_settled_terminal(
    kind: SettledLineageKind,
    as_projection: bool,
) -> Result<(), StoreError> {
    if matches!(
        kind,
        SettledLineageKind::Pure | SettledLineageKind::PromptLibrary
    ) {
        return Err(mismatch(
            as_projection,
            "pure/prompt-library operation cannot become failed, cancelled, or uncertain",
        ));
    }
    if matches!(kind, SettledLineageKind::HostAdmission) {
        return Err(mismatch(
            as_projection,
            "host-admission operation cannot become cancelled, uncertain, or non-cleanup failed",
        ));
    }
    Ok(())
}

/// Host-admission `operation.failed` is valid only for CleanupFailed after a complete
/// event-backed four-branch journal that contains at least one Failed branch, with the
/// failed fact immediately following the final branch event.
pub(crate) fn validate_host_admission_cleanup_failed_lineage(
    fact: &OperationFailedFact,
    failed_occurred_at_ms: i64,
    prior: Option<(EventId, &Event, Option<u64>, i64, Option<TaskId>)>,
    journal: &[(HostCleanupBranch, HostCleanupBranchOutcome)],
    journal_max_completed_at_ms: i64,
    expected_command_id: CommandId,
    expected_operation_id: OperationId,
    expected_action_epoch: u64,
    as_projection: bool,
) -> Result<(), StoreError> {
    if failed_occurred_at_ms != fact.settled_at_ms {
        return Err(mismatch(
            as_projection,
            "host-admission operation.failed occurred_at_ms must equal fact.settled_at_ms",
        ));
    }
    if fact.command_id != expected_command_id
        || fact.operation_id != expected_operation_id
        || fact.action_epoch != Some(expected_action_epoch)
        || fact.resource_id.is_some()
        || fact.runtime_generation.is_some()
        || fact.code != OperationErrorCode::CleanupFailed
        || !fact.source.is_dispatch()
    {
        return Err(mismatch(
            as_projection,
            "host-admission CleanupFailed fact must match accepted ConfirmHostQuit identity/fence",
        ));
    }
    if journal.len() != HostCleanupBranch::ORDER.len() {
        return Err(mismatch(
            as_projection,
            "host-admission CleanupFailed requires a complete four-branch cleanup journal",
        ));
    }
    let mut any_failed = false;
    for (idx, (branch, outcome)) in journal.iter().enumerate() {
        if HostCleanupBranch::ORDER.get(idx) != Some(branch) {
            return Err(mismatch(
                as_projection,
                "host-admission CleanupFailed journal must follow fixed branch ORDER",
            ));
        }
        if matches!(outcome, HostCleanupBranchOutcome::Failed { .. }) {
            any_failed = true;
        }
    }
    if !any_failed {
        return Err(mismatch(
            as_projection,
            "host-admission CleanupFailed requires at least one failed cleanup branch",
        ));
    }
    let Some((_, prior_event, prior_revision, prior_occurred, prior_task)) = prior else {
        return Err(mismatch(
            as_projection,
            "host-admission CleanupFailed must immediately follow the final cleanup branch event",
        ));
    };
    if prior_revision.is_some() || prior_task.is_some() {
        return Err(mismatch(
            as_projection,
            "host-admission CleanupFailed prior cleanup event must be global-scoped",
        ));
    }
    let Event::HostCleanupBranchCompleted {
        operation_id,
        action_epoch,
        branch,
        ..
    } = prior_event
    else {
        return Err(mismatch(
            as_projection,
            "host-admission CleanupFailed must immediately follow host.cleanup_branch_completed",
        ));
    };
    if *operation_id != expected_operation_id
        || *action_epoch != expected_action_epoch
        || *branch != HostCleanupBranch::TaskTeardowns
    {
        return Err(mismatch(
            as_projection,
            "host-admission CleanupFailed must immediately follow the TaskTeardowns branch event",
        ));
    }
    if failed_occurred_at_ms < prior_occurred || failed_occurred_at_ms < journal_max_completed_at_ms
    {
        return Err(mismatch(
            as_projection,
            "host-admission CleanupFailed must not predate the final cleanup branch timestamp",
        ));
    }
    Ok(())
}

/// Host-admission `operation.settled` is valid only after a complete all-Succeeded
/// four-branch cleanup journal. The settle fact must immediately follow the exact
/// global TaskTeardowns `HostCleanupBranchCompleted` for the same operation/epoch;
/// that predecessor event_id must be the fourth ordered `result_event_id`. Later
/// unrelated durable events after the settle terminal remain allowed.
pub(crate) fn validate_host_admission_settled_lineage(
    fact: &OperationSettledFact,
    settled_occurred_at_ms: i64,
    prior: Option<(EventId, &Event, Option<u64>, i64, Option<TaskId>)>,
    ordered_branch_event_ids: &[EventId],
    journal: &[(HostCleanupBranch, HostCleanupBranchOutcome)],
    journal_max_completed_at_ms: i64,
    accepted_at_ms: i64,
    expected_command_id: CommandId,
    expected_operation_id: OperationId,
    expected_action_epoch: u64,
    as_projection: bool,
) -> Result<(), StoreError> {
    if settled_occurred_at_ms != fact.settled_at_ms {
        return Err(mismatch(
            as_projection,
            "host-admission operation.settled occurred_at_ms must equal fact.settled_at_ms",
        ));
    }
    if fact.command_id != expected_command_id
        || fact.operation_id != expected_operation_id
        || fact.action_epoch != Some(expected_action_epoch)
        || fact.resource_id.is_some()
        || fact.runtime_generation.is_some()
        || !fact.source.is_dispatch()
    {
        return Err(mismatch(
            as_projection,
            "host-admission OperationSettled fact must match accepted ConfirmHostQuit identity/fence",
        ));
    }
    if journal.len() != HostCleanupBranch::ORDER.len() {
        return Err(mismatch(
            as_projection,
            "host-admission OperationSettled requires a complete four-branch cleanup journal",
        ));
    }
    for (idx, (branch, outcome)) in journal.iter().enumerate() {
        if *branch != HostCleanupBranch::ORDER[idx] {
            return Err(mismatch(
                as_projection,
                "host-admission OperationSettled journal must follow fixed branch ORDER",
            ));
        }
        if !matches!(outcome, HostCleanupBranchOutcome::Succeeded) {
            return Err(mismatch(
                as_projection,
                "host-admission OperationSettled requires every cleanup branch Succeeded",
            ));
        }
    }
    if ordered_branch_event_ids.len() != HostCleanupBranch::ORDER.len()
        || fact.result_event_ids.as_slice() != ordered_branch_event_ids
    {
        return Err(mismatch(
            as_projection,
            "host-admission OperationSettled result_event_ids must equal ORDER branch event ids",
        ));
    }
    let Some((prior_id, prior_event, prior_revision, _prior_occurred, prior_task)) = prior else {
        return Err(mismatch(
            as_projection,
            "host-admission OperationSettled must immediately follow the final cleanup branch event",
        ));
    };
    if prior_task.is_some() || prior_revision.is_some() {
        return Err(mismatch(
            as_projection,
            "host-admission OperationSettled prior cleanup event must be global-scoped",
        ));
    }
    let Event::HostCleanupBranchCompleted {
        operation_id: prior_operation_id,
        action_epoch: prior_epoch,
        branch: prior_branch,
        outcome: prior_outcome,
    } = prior_event
    else {
        return Err(mismatch(
            as_projection,
            "host-admission OperationSettled must immediately follow host.cleanup_branch_completed",
        ));
    };
    if *prior_operation_id != expected_operation_id
        || *prior_epoch != expected_action_epoch
        || *prior_branch != HostCleanupBranch::TaskTeardowns
        || !matches!(prior_outcome, HostCleanupBranchOutcome::Succeeded)
    {
        return Err(mismatch(
            as_projection,
            "host-admission OperationSettled must immediately follow the TaskTeardowns branch event",
        ));
    }
    let fourth = ordered_branch_event_ids.last().copied().ok_or_else(|| {
        mismatch(
            as_projection,
            "host-admission OperationSettled requires four ordered branch event ids",
        )
    })?;
    if prior_id != fourth || fact.result_event_ids.last().copied() != Some(fourth) {
        return Err(mismatch(
            as_projection,
            "host-admission OperationSettled predecessor must be the fourth ordered result_event_id",
        ));
    }
    if settled_occurred_at_ms < accepted_at_ms
        || settled_occurred_at_ms < journal_max_completed_at_ms
    {
        return Err(mismatch(
            as_projection,
            "host-admission OperationSettled must not predate acceptance or final branch completion",
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
            | Event::TaskSettled
            | Event::TaskReopened
            | Event::TaskArchived
            | Event::TaskDeleted
            | Event::AgentSessionRegistered { .. }
            | Event::AgentProviderSessionBound { .. }
            | Event::PrimaryAgentSet { .. }
            | Event::UnstartedPrimaryProviderRebound { .. }
            | Event::SpecialistRequested { .. }
            | Event::PrimaryPromoted { .. }
            | Event::SpecialistHandoffRecorded { .. }
            | Event::SpecialistClosed { .. }
            | Event::ArtifactRegistered { .. }
            | Event::ResourceRegistered { .. }
            | Event::ResourceReleased { .. }
            | Event::ProviderQuestionPresented { .. }
            | Event::ProviderApprovalPresented { .. }
            | Event::ProviderWaitSettled { .. }
            // Task-mutation terminal facts; keep in lockstep with
            // `outbox::is_pure_slice_decision_fact`. The host-reported terminal
            // facts are deliberately absent: they consume no task revision, so
            // this validator's `is_task_mutation` test would reject them anyway.
            | Event::TerminalRenamed { .. }
            | Event::TaskTerminalStripSet { .. }
            | Event::Browser(_)
    )
}

/// Validate a host-scoped prompt-library OperationSettled.
///
/// This path never inspects task-mutation decision facts. The only legal
/// predecessor is OperationAccepted; `result_event_ids` is that accepted id.
pub(crate) fn validate_prompt_library_settled_lineage(
    fact: &OperationSettledFact,
    settled_occurred_at_ms: i64,
    settled_task_id: Option<TaskId>,
    settled_task_revision: Option<u64>,
    accepted_at_ms: i64,
    accepted: Option<(EventId, &Event, Option<u64>, i64, Option<TaskId>)>,
    as_projection: bool,
) -> Result<(), StoreError> {
    let kind = classify_operation_settled_fact(fact, settled_task_id, as_projection)?;
    if !matches!(kind, SettledLineageKind::PromptLibrary) {
        return Err(mismatch(
            as_projection,
            "prompt-library settle validator cannot accept a task-mutation fence",
        ));
    }
    if settled_task_id.is_some() || settled_task_revision.is_some() {
        return Err(mismatch(
            as_projection,
            "prompt-library operation.settled requires NULL task_id and task_revision",
        ));
    }
    if !fact.source.is_dispatch()
        || fact.action_epoch.is_some()
        || fact.resource_id.is_some()
        || fact.runtime_generation.is_some()
    {
        return Err(mismatch(
            as_projection,
            "prompt-library operation.settled requires Dispatch and an all-none fence",
        ));
    }
    if settled_occurred_at_ms != fact.settled_at_ms || fact.settled_at_ms != accepted_at_ms {
        return Err(mismatch(
            as_projection,
            "prompt-library operation.settled must equal accepted_at_ms",
        ));
    }
    let Some((
        accepted_id,
        Event::OperationAccepted(accepted_fact),
        accepted_revision,
        accepted_occurred,
        accepted_task,
    )) = accepted
    else {
        return Err(mismatch(
            as_projection,
            "prompt-library operation.settled requires immediately preceding operation.accepted",
        ));
    };
    if fact.result_event_ids.len() != 1 || fact.result_event_ids[0] != accepted_id {
        return Err(mismatch(
            as_projection,
            "prompt-library operation.settled result_event_ids must be the accepted event",
        ));
    }
    if accepted_revision.is_some()
        || accepted_task.is_some()
        || accepted_occurred != accepted_at_ms
        || accepted_fact.operation_id != fact.operation_id
        || accepted_fact.command_id != fact.command_id
        || accepted_fact.accepted_at_ms != accepted_at_ms
        || accepted_fact.action_epoch.is_some()
        || accepted_fact.resource_id.is_some()
        || accepted_fact.runtime_generation.is_some()
    {
        return Err(mismatch(
            as_projection,
            "prompt-library operation.accepted fence/time/scope mismatch",
        ));
    }
    Ok(())
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
