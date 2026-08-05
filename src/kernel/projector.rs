//! Deterministic projection functions. No clocks, randomness, filesystem, or network.

use rusqlite::Transaction;

use crate::domain::agent::AgentRole;
use crate::domain::event::{DomainEvent, Event};
use crate::domain::id::{AgentSessionId, ResourceId, TaskId};
use crate::domain::operation::{CancellationReason, OperationErrorCode, OperationUncertaintyCode};
use crate::domain::resource::ResourceLifecycle;
use crate::domain::task::{
    ReviewReadiness, TaskActivity, TaskAttention, TaskConnectivity, TaskLifecycle,
};
use crate::kernel::store::{u64_to_sqlite_i64, StoreError};

/// Apply one event into projection tables (stable or shadow_*).
pub(crate) fn apply_event(
    tx: &Transaction<'_>,
    event: &DomainEvent,
    shadow: bool,
) -> Result<(), StoreError> {
    match &event.payload {
        Event::TaskCreated {
            task,
            connectivity,
            attention,
            activity,
            review_readiness,
        } => {
            let task_id = event.task_id.ok_or_else(|| {
                StoreError::Projection("task.created requires DomainEvent.task_id".into())
            })?;
            if task_id != task.id {
                return Err(StoreError::Projection(
                    "task.created task_id mismatch".into(),
                ));
            }
            if event.task_revision != Some(1) || task.revision != 1 {
                return Err(StoreError::Projection(
                    "task.created requires envelope and payload revision 1".into(),
                ));
            }
            let table = table_name("tasks", shadow);
            tx.execute(
                &format!(
                    "INSERT INTO {table} (
                        task_id, environment_id, project_id, title, description,
                        workspace, assignment, lifecycle, action_epoch, revision,
                        connectivity, attention, activity, review_readiness,
                        primary_agent_session_id, created_at_ms, updated_at_ms
                     ) VALUES (
                        ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10,
                        ?11, ?12, ?13, ?14, NULL, ?15, ?15
                     )"
                ),
                rusqlite::params![
                    task.id.as_bytes().as_slice(),
                    task.environment_id.as_bytes().as_slice(),
                    task.project_id.as_bytes().as_slice(),
                    task.title,
                    task.description,
                    pack(&task.workspace)?,
                    pack(&task.assignment)?,
                    lifecycle_text(task.lifecycle),
                    u64_to_sqlite_i64("tasks.action_epoch", task.action_epoch)?,
                    u64_to_sqlite_i64("tasks.revision", task.revision)?,
                    connectivity_text(*connectivity),
                    attention_text(*attention),
                    activity_text(*activity),
                    review_text(*review_readiness),
                    task.created_at_ms,
                ],
            )?;
        }
        Event::TaskRenamed { title } => {
            let task_id = require_task_id(event)?;
            let next_revision = require_next_revision(tx, shadow, task_id, event)?;
            let table = table_name("tasks", shadow);
            tx.execute(
                &format!(
                    "UPDATE {table}
                     SET title = ?1, revision = ?2, updated_at_ms = ?3
                     WHERE task_id = ?4"
                ),
                rusqlite::params![
                    title,
                    next_revision,
                    event.occurred_at_ms,
                    task_id.as_bytes().as_slice(),
                ],
            )?;
            require_one_change(tx, "task.renamed")?;
        }
        Event::TaskAttentionSet { attention } => {
            let task_id = require_task_id(event)?;
            let next_revision = require_next_revision(tx, shadow, task_id, event)?;
            let table = table_name("tasks", shadow);
            tx.execute(
                &format!(
                    "UPDATE {table}
                     SET attention = ?1, revision = ?2, updated_at_ms = ?3
                     WHERE task_id = ?4"
                ),
                rusqlite::params![
                    attention_text(*attention),
                    next_revision,
                    event.occurred_at_ms,
                    task_id.as_bytes().as_slice(),
                ],
            )?;
            require_one_change(tx, "task.attention_set")?;
        }
        Event::TaskCloseBegun { action_epoch } => {
            let task_id = require_task_id(event)?;
            let next_revision = require_next_revision(tx, shadow, task_id, event)?;
            let table = table_name("tasks", shadow);
            tx.execute(
                &format!(
                    "UPDATE {table}
                     SET lifecycle = ?1, action_epoch = ?2, revision = ?3, updated_at_ms = ?4
                     WHERE task_id = ?5"
                ),
                rusqlite::params![
                    lifecycle_text(TaskLifecycle::Closing),
                    u64_to_sqlite_i64("tasks.action_epoch", *action_epoch)?,
                    next_revision,
                    event.occurred_at_ms,
                    task_id.as_bytes().as_slice(),
                ],
            )?;
            require_one_change(tx, "task.close_begun")?;
        }
        Event::TaskReopened => {
            let task_id = require_task_id(event)?;
            let next_revision = require_next_revision(tx, shadow, task_id, event)?;
            let table = table_name("tasks", shadow);
            tx.execute(
                &format!(
                    "UPDATE {table}
                     SET lifecycle = ?1, revision = ?2, updated_at_ms = ?3
                     WHERE task_id = ?4"
                ),
                rusqlite::params![
                    lifecycle_text(TaskLifecycle::Open),
                    next_revision,
                    event.occurred_at_ms,
                    task_id.as_bytes().as_slice(),
                ],
            )?;
            require_one_change(tx, "task.reopened")?;
        }
        Event::TaskArchived => {
            let task_id = require_task_id(event)?;
            let next_revision = require_next_revision(tx, shadow, task_id, event)?;
            let table = table_name("tasks", shadow);
            tx.execute(
                &format!(
                    "UPDATE {table}
                     SET lifecycle = ?1, revision = ?2, updated_at_ms = ?3
                     WHERE task_id = ?4"
                ),
                rusqlite::params![
                    lifecycle_text(TaskLifecycle::Archived),
                    next_revision,
                    event.occurred_at_ms,
                    task_id.as_bytes().as_slice(),
                ],
            )?;
            require_one_change(tx, "task.archived")?;
        }
        Event::AgentSessionRegistered { agent } => {
            let task_id = require_task_id(event)?;
            if agent.task_id != task_id {
                return Err(StoreError::Projection(
                    "agent session task_id mismatch".into(),
                ));
            }
            let table = table_name("agent_sessions", shadow);
            tx.execute(
                &format!(
                    "INSERT INTO {table} (
                        agent_session_id, task_id, role, provider_kind, provider_session_id,
                        lifecycle, runtime_generation, revision
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"
                ),
                rusqlite::params![
                    agent.id.as_bytes().as_slice(),
                    agent.task_id.as_bytes().as_slice(),
                    pack(&agent.role)?,
                    agent.provider_kind,
                    agent.provider_session_id,
                    agent_lifecycle_text(agent.lifecycle),
                    u64_to_sqlite_i64(
                        "agent_sessions.runtime_generation",
                        agent.runtime_generation
                    )?,
                    u64_to_sqlite_i64("agent_sessions.revision", agent.revision)?,
                ],
            )?;
            bump_task_revision(tx, shadow, task_id, event)?;
        }
        Event::PrimaryAgentSet { agent_session_id } => {
            let task_id = require_task_id(event)?;
            validate_primary_agent(tx, shadow, task_id, *agent_session_id)?;
            let next_revision = require_next_revision(tx, shadow, task_id, event)?;
            let table = table_name("tasks", shadow);
            tx.execute(
                &format!(
                    "UPDATE {table}
                     SET primary_agent_session_id = ?1, revision = ?2, updated_at_ms = ?3
                     WHERE task_id = ?4"
                ),
                rusqlite::params![
                    agent_session_id.as_bytes().as_slice(),
                    next_revision,
                    event.occurred_at_ms,
                    task_id.as_bytes().as_slice(),
                ],
            )?;
            require_one_change(tx, "primary_agent.set")?;
        }
        Event::ArtifactRegistered { artifact } => {
            let task_id = require_task_id(event)?;
            if artifact.task_id != task_id {
                return Err(StoreError::Projection("artifact task_id mismatch".into()));
            }
            let table = table_name("artifacts", shadow);
            tx.execute(
                &format!(
                    "INSERT INTO {table} (
                        artifact_id, task_id, kind, label, content_ref, sha256,
                        privacy_class, created_at_ms
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"
                ),
                rusqlite::params![
                    artifact.id.as_bytes().as_slice(),
                    artifact.task_id.as_bytes().as_slice(),
                    artifact_kind_text(artifact.kind),
                    artifact.label,
                    pack(&artifact.content_ref)?,
                    artifact.sha256.as_slice(),
                    privacy_text(artifact.privacy_class),
                    artifact.created_at_ms,
                ],
            )?;
            bump_task_revision(tx, shadow, task_id, event)?;
        }
        Event::ResourceRegistered { resource } => {
            let task_id = require_task_id(event)?;
            match resource.task_id {
                Some(id) if id == task_id => {}
                _ => {
                    return Err(StoreError::Projection(
                        "resource registration task_id mismatch".into(),
                    ))
                }
            }
            let table = table_name("resources", shadow);
            tx.execute(
                &format!(
                    "INSERT INTO {table} (
                        resource_id, task_id, owner_kind, resource_kind, recipe,
                        lifecycle, runtime_generation, updated_at_ms
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"
                ),
                rusqlite::params![
                    resource.id.as_bytes().as_slice(),
                    resource.task_id.map(|id| id.as_bytes().as_slice().to_vec()),
                    owner_kind_text(resource.owner_kind),
                    resource_kind_text(resource.resource_kind),
                    pack(&resource.recipe)?,
                    resource_lifecycle_text(resource.lifecycle),
                    u64_to_sqlite_i64("resources.runtime_generation", resource.runtime_generation)?,
                    resource.updated_at_ms,
                ],
            )?;
            bump_task_revision(tx, shadow, task_id, event)?;
        }
        Event::ResourceReleaseBegun {
            resource_id,
            runtime_generation,
        } => {
            let task_id = require_task_id(event)?;
            update_resource_lifecycle(
                tx,
                shadow,
                task_id,
                *resource_id,
                *runtime_generation,
                ResourceLifecycle::Active,
                ResourceLifecycle::Releasing,
                event.occurred_at_ms,
            )?;
            bump_task_revision(tx, shadow, task_id, event)?;
        }
        Event::ResourceReleased {
            resource_id,
            runtime_generation,
        } => {
            let task_id = require_task_id(event)?;
            update_resource_lifecycle(
                tx,
                shadow,
                task_id,
                *resource_id,
                *runtime_generation,
                ResourceLifecycle::Releasing,
                ResourceLifecycle::Released,
                event.occurred_at_ms,
            )?;
            bump_task_revision(tx, shadow, task_id, event)?;
        }
        Event::OperationAccepted(fact) => {
            // operations.task_id comes only from DomainEvent.task_id.
            let table = table_name("operations", shadow);
            tx.execute(
                &format!(
                    "INSERT INTO {table} (
                        operation_id, command_id, task_id, resource_id, action_epoch,
                        runtime_generation, state, result, outcome_code,
                        accepted_at_ms, outcome_at_ms
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'accepted', NULL, NULL, ?7, NULL)"
                ),
                rusqlite::params![
                    fact.operation_id.as_bytes().as_slice(),
                    fact.command_id.as_bytes().as_slice(),
                    event.task_id.map(|id| id.as_bytes().as_slice().to_vec()),
                    fact.resource_id.map(|id| id.as_bytes().as_slice().to_vec()),
                    opt_u64("operations.action_epoch", fact.action_epoch)?,
                    opt_u64("operations.runtime_generation", fact.runtime_generation)?,
                    fact.accepted_at_ms,
                ],
            )?;
        }
        Event::OperationSettled(fact) => {
            apply_operation_outcome(
                tx,
                shadow,
                event.task_id,
                fact.operation_id.as_bytes(),
                fact.action_epoch,
                fact.resource_id,
                fact.runtime_generation,
                "settled",
                Some(pack(&fact.result_event_ids)?),
                None,
                fact.settled_at_ms,
            )?;
        }
        Event::OperationFailed(fact) => {
            apply_operation_outcome(
                tx,
                shadow,
                event.task_id,
                fact.operation_id.as_bytes(),
                fact.action_epoch,
                fact.resource_id,
                fact.runtime_generation,
                "failed",
                None,
                Some(error_code_text(fact.code)),
                fact.settled_at_ms,
            )?;
        }
        Event::OperationCancelled(fact) => {
            apply_operation_outcome(
                tx,
                shadow,
                event.task_id,
                fact.operation_id.as_bytes(),
                fact.action_epoch,
                fact.resource_id,
                fact.runtime_generation,
                "cancelled",
                None,
                Some(cancel_text(fact.reason)),
                fact.settled_at_ms,
            )?;
        }
        Event::OperationUncertain(fact) => {
            apply_operation_outcome(
                tx,
                shadow,
                event.task_id,
                fact.operation_id.as_bytes(),
                fact.action_epoch,
                fact.resource_id,
                fact.runtime_generation,
                "uncertain",
                None,
                Some(uncertain_text(fact.code)),
                fact.observed_at_ms,
            )?;
        }
    }
    Ok(())
}

fn bump_task_revision(
    tx: &Transaction<'_>,
    shadow: bool,
    task_id: TaskId,
    event: &DomainEvent,
) -> Result<(), StoreError> {
    let next_revision = require_next_revision(tx, shadow, task_id, event)?;
    let table = table_name("tasks", shadow);
    tx.execute(
        &format!(
            "UPDATE {table}
             SET revision = ?1, updated_at_ms = ?2
             WHERE task_id = ?3"
        ),
        rusqlite::params![
            next_revision,
            event.occurred_at_ms,
            task_id.as_bytes().as_slice(),
        ],
    )?;
    require_one_change(tx, "task revision bump")?;
    Ok(())
}

fn require_next_revision(
    tx: &Transaction<'_>,
    shadow: bool,
    task_id: TaskId,
    event: &DomainEvent,
) -> Result<i64, StoreError> {
    let observed = event.task_revision.ok_or_else(|| {
        StoreError::Projection(format!(
            "event {} requires task_revision",
            event.payload.event_type()
        ))
    })?;
    let table = table_name("tasks", shadow);
    let current: i64 = match tx.query_row(
        &format!("SELECT revision FROM {table} WHERE task_id = ?1"),
        [task_id.as_bytes().as_slice()],
        |row| row.get(0),
    ) {
        Ok(v) => v,
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            return Err(StoreError::Projection(format!(
                "missing task for event {}",
                event.payload.event_type()
            )))
        }
        Err(err) => return Err(err.into()),
    };
    let current_u64 = u64::try_from(current).map_err(|_| StoreError::IntegerOutOfRange {
        field: "tasks.revision",
        value: current.unsigned_abs(),
    })?;
    let expected = current_u64
        .checked_add(1)
        .ok_or(StoreError::IntegerOutOfRange {
            field: "tasks.revision",
            value: u64::MAX,
        })?;
    if observed != expected {
        return Err(StoreError::Projection(format!(
            "task revision must advance by one: stored {current_u64}, event {observed}"
        )));
    }
    u64_to_sqlite_i64("tasks.revision", observed)
}

fn require_one_change(tx: &Transaction<'_>, context: &str) -> Result<(), StoreError> {
    if tx.changes() != 1 {
        return Err(StoreError::Projection(format!(
            "{context}: expected exactly one affected row"
        )));
    }
    Ok(())
}

fn validate_primary_agent(
    tx: &Transaction<'_>,
    shadow: bool,
    task_id: TaskId,
    agent_session_id: AgentSessionId,
) -> Result<(), StoreError> {
    let table = table_name("agent_sessions", shadow);
    let row: Result<(Vec<u8>, Vec<u8>), rusqlite::Error> = tx.query_row(
        &format!("SELECT task_id, role FROM {table} WHERE agent_session_id = ?1"),
        [agent_session_id.as_bytes().as_slice()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    );
    let (session_task, role_blob) = match row {
        Ok(v) => v,
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            return Err(StoreError::Projection(
                "primary agent session not found".into(),
            ))
        }
        Err(err) => return Err(err.into()),
    };
    if session_task.as_slice() != task_id.as_bytes().as_slice() {
        return Err(StoreError::Projection(
            "primary agent session belongs to a different task".into(),
        ));
    }
    let role: AgentRole = rmp_serde::from_slice(&role_blob)
        .map_err(|e| StoreError::Projection(format!("invalid agent role blob: {e}")))?;
    if !matches!(role, AgentRole::Primary) {
        return Err(StoreError::Projection(
            "primary agent selection requires Primary role".into(),
        ));
    }
    Ok(())
}

fn update_resource_lifecycle(
    tx: &Transaction<'_>,
    shadow: bool,
    task_id: TaskId,
    resource_id: ResourceId,
    runtime_generation: u64,
    expected: ResourceLifecycle,
    next: ResourceLifecycle,
    occurred_at_ms: i64,
) -> Result<(), StoreError> {
    let table = table_name("resources", shadow);
    let row: Result<(Option<Vec<u8>>, String, i64), rusqlite::Error> = tx.query_row(
        &format!(
            "SELECT task_id, lifecycle, runtime_generation FROM {table} WHERE resource_id = ?1"
        ),
        [resource_id.as_bytes().as_slice()],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    );
    let (stored_task, lifecycle, stored_generation) = match row {
        Ok(v) => v,
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            return Err(StoreError::Projection("resource not found".into()))
        }
        Err(err) => return Err(err.into()),
    };
    match stored_task {
        Some(bytes) if bytes.as_slice() == task_id.as_bytes().as_slice() => {}
        _ => {
            return Err(StoreError::Projection(
                "resource task ownership mismatch".into(),
            ))
        }
    }
    if lifecycle != resource_lifecycle_text(expected) {
        return Err(StoreError::Projection(format!(
            "resource lifecycle {lifecycle} expected {}",
            resource_lifecycle_text(expected)
        )));
    }
    let expected_generation =
        u64_to_sqlite_i64("resources.runtime_generation", runtime_generation)?;
    if stored_generation != expected_generation {
        return Err(StoreError::Projection(format!(
            "resource generation fence: stored {stored_generation} != event {expected_generation}"
        )));
    }
    tx.execute(
        &format!(
            "UPDATE {table}
             SET lifecycle = ?1, updated_at_ms = ?2
             WHERE resource_id = ?3"
        ),
        rusqlite::params![
            resource_lifecycle_text(next),
            occurred_at_ms,
            resource_id.as_bytes().as_slice(),
        ],
    )?;
    require_one_change(tx, "resource lifecycle")?;
    Ok(())
}

fn apply_operation_outcome(
    tx: &Transaction<'_>,
    shadow: bool,
    event_task_id: Option<TaskId>,
    operation_id: &[u8; 16],
    action_epoch: Option<u64>,
    resource_id: Option<crate::domain::id::ResourceId>,
    runtime_generation: Option<u64>,
    state: &str,
    result: Option<Vec<u8>>,
    outcome_code: Option<&str>,
    outcome_at_ms: i64,
) -> Result<(), StoreError> {
    let table = table_name("operations", shadow);
    let row: Result<
        (
            Option<Vec<u8>>,
            Option<i64>,
            Option<Vec<u8>>,
            Option<i64>,
            String,
        ),
        rusqlite::Error,
    > = tx.query_row(
        &format!(
            "SELECT task_id, action_epoch, resource_id, runtime_generation, state
             FROM {table} WHERE operation_id = ?1"
        ),
        [operation_id.as_slice()],
        |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        },
    );
    let (stored_task, stored_epoch, stored_resource, stored_generation, stored_state) = match row {
        Ok(v) => v,
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            return Err(StoreError::Projection("operation not found".into()))
        }
        Err(err) => return Err(err.into()),
    };
    if stored_state != "accepted" {
        return Err(StoreError::Projection(format!(
            "operation state {stored_state} is not accepted"
        )));
    }
    let expected_task = event_task_id.map(|id| id.as_bytes().as_slice().to_vec());
    if stored_task != expected_task {
        return Err(StoreError::Projection(
            "operation task_id fence mismatch".into(),
        ));
    }
    let expected_epoch = opt_u64("operations.action_epoch", action_epoch)?;
    if stored_epoch != expected_epoch {
        return Err(StoreError::Projection(
            "operation action_epoch fence mismatch".into(),
        ));
    }
    let expected_resource = resource_id.map(|id| id.as_bytes().as_slice().to_vec());
    if stored_resource != expected_resource {
        return Err(StoreError::Projection(
            "operation resource_id fence mismatch".into(),
        ));
    }
    let expected_generation = opt_u64("operations.runtime_generation", runtime_generation)?;
    if stored_generation != expected_generation {
        return Err(StoreError::Projection(
            "operation runtime_generation fence mismatch".into(),
        ));
    }
    tx.execute(
        &format!(
            "UPDATE {table}
             SET state = ?1, result = ?2, outcome_code = ?3, outcome_at_ms = ?4
             WHERE operation_id = ?5"
        ),
        rusqlite::params![
            state,
            result,
            outcome_code,
            outcome_at_ms,
            operation_id.as_slice(),
        ],
    )?;
    require_one_change(tx, "operation outcome")?;
    Ok(())
}

fn table_name(base: &str, shadow: bool) -> String {
    if shadow {
        format!("shadow_{base}")
    } else {
        base.to_string()
    }
}

fn require_task_id(event: &DomainEvent) -> Result<TaskId, StoreError> {
    event.task_id.ok_or_else(|| {
        StoreError::Projection(format!(
            "event {} requires DomainEvent.task_id",
            event.payload.event_type()
        ))
    })
}

fn opt_u64(field: &'static str, value: Option<u64>) -> Result<Option<i64>, StoreError> {
    match value {
        Some(v) => Ok(Some(u64_to_sqlite_i64(field, v)?)),
        None => Ok(None),
    }
}

fn pack<T: serde::Serialize>(value: &T) -> Result<Vec<u8>, StoreError> {
    rmp_serde::to_vec(value).map_err(|e| StoreError::Projection(e.to_string()))
}

fn lifecycle_text(value: TaskLifecycle) -> &'static str {
    match value {
        TaskLifecycle::Open => "open",
        TaskLifecycle::Closing => "closing",
        TaskLifecycle::Archived => "archived",
    }
}

fn connectivity_text(value: TaskConnectivity) -> &'static str {
    match value {
        TaskConnectivity::Connected => "connected",
        TaskConnectivity::Disconnected => "disconnected",
    }
}

fn attention_text(value: TaskAttention) -> &'static str {
    match value {
        TaskAttention::None => "none",
        TaskAttention::NeedsAnswer => "needs_answer",
        TaskAttention::NeedsApproval => "needs_approval",
        TaskAttention::UncertainOutcome => "uncertain_outcome",
        TaskAttention::Failed => "failed",
    }
}

fn activity_text(value: TaskActivity) -> &'static str {
    match value {
        TaskActivity::Idle => "idle",
        TaskActivity::Working => "working",
        TaskActivity::Settling => "settling",
    }
}

fn review_text(value: ReviewReadiness) -> &'static str {
    match value {
        ReviewReadiness::NotReady => "not_ready",
        ReviewReadiness::Ready => "ready",
    }
}

fn agent_lifecycle_text(value: crate::domain::agent::AgentSessionLifecycle) -> &'static str {
    match value {
        crate::domain::agent::AgentSessionLifecycle::Open => "open",
        crate::domain::agent::AgentSessionLifecycle::Closing => "closing",
        crate::domain::agent::AgentSessionLifecycle::Closed => "closed",
    }
}

fn artifact_kind_text(value: crate::domain::artifact::ArtifactKind) -> &'static str {
    match value {
        crate::domain::artifact::ArtifactKind::Specification => "specification",
        crate::domain::artifact::ArtifactKind::Finding => "finding",
        crate::domain::artifact::ArtifactKind::Decision => "decision",
        crate::domain::artifact::ArtifactKind::Evidence => "evidence",
        crate::domain::artifact::ArtifactKind::ReviewReport => "review_report",
    }
}

fn privacy_text(value: crate::domain::artifact::PrivacyClass) -> &'static str {
    match value {
        crate::domain::artifact::PrivacyClass::LocalOnly => "local_only",
        crate::domain::artifact::PrivacyClass::Shareable => "shareable",
    }
}

fn owner_kind_text(value: crate::domain::resource::OwnerKind) -> &'static str {
    match value {
        crate::domain::resource::OwnerKind::Task => "task",
        crate::domain::resource::OwnerKind::Host => "host",
    }
}

fn resource_kind_text(value: crate::domain::resource::ResourceKind) -> &'static str {
    match value {
        crate::domain::resource::ResourceKind::Terminal => "terminal",
        crate::domain::resource::ResourceKind::BrowserContext => "browser_context",
        crate::domain::resource::ResourceKind::Service => "service",
    }
}

fn resource_lifecycle_text(value: ResourceLifecycle) -> &'static str {
    match value {
        ResourceLifecycle::Active => "active",
        ResourceLifecycle::Releasing => "releasing",
        ResourceLifecycle::Released => "released",
    }
}

fn error_code_text(value: OperationErrorCode) -> &'static str {
    match value {
        OperationErrorCode::SideEffectFailed => "side_effect_failed",
    }
}

fn cancel_text(value: CancellationReason) -> &'static str {
    match value {
        CancellationReason::Superseded => "superseded",
    }
}

fn uncertain_text(value: OperationUncertaintyCode) -> &'static str {
    match value {
        OperationUncertaintyCode::AmbiguousDispatch => "ambiguous_dispatch",
    }
}
