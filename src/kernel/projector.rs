//! Deterministic projection functions. No clocks, randomness, filesystem, or network.

use rusqlite::{OptionalExtension, Transaction};

use crate::domain::agent::{AgentRole, AgentSessionLifecycle, SpecialistPermission};
use crate::domain::artifact::{
    structured_specialist_result, verify_inline_content_digest, ArtifactContentRef, ArtifactKind,
    MAX_SPECIALIST_RAW_ARTIFACT_BYTES,
};
use crate::domain::event::{DomainEvent, Event};
use crate::domain::host::{HostCleanupBranch, HostCleanupBranchOutcome};
use crate::domain::id::{AgentSessionId, CommandId, EventId, ResourceId, TaskId};
use crate::domain::operation::{
    CancellationReason, OperationErrorCode, OperationUncertaintyCode, OutcomeFenceError,
    OutcomeSource,
};
use crate::domain::resource::ResourceLifecycle;
use crate::domain::task::{
    ReviewReadiness, TaskActivity, TaskAttention, TaskConnectivity, TaskLifecycle,
};
use crate::kernel::lineage::{
    classify_operation_settled_fact, classify_settled_lineage_fence, is_derived_lifecycle_result,
    reject_pure_non_settled_terminal, validate_host_admission_cleanup_failed_lineage,
    validate_host_admission_settled_lineage, validate_prompt_library_settled_lineage,
    validate_pure_settled_lineage, validate_side_effect_settled_has_prior_derived,
    SettledLineageKind,
};
use crate::kernel::store::{
    decode_stored_event, u64_from_nonnegative_i64, u64_to_sqlite_i64, StoreError,
};

/// Apply one event into projection tables (stable or shadow_*).
pub(crate) fn apply_event(
    tx: &Transaction<'_>,
    event: &DomainEvent,
    shadow: bool,
) -> Result<(), StoreError> {
    enforce_envelope_task_revision_rule(event)?;
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
            require_close_begun(tx, shadow, task_id, *action_epoch)?;
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
        Event::TaskSettled => {
            let task_id = require_task_id(event)?;
            let next_revision = require_next_revision(tx, shadow, task_id, event)?;
            require_settle(tx, shadow, task_id)?;
            let table = table_name("tasks", shadow);
            tx.execute(
                &format!(
                    "UPDATE {table}
                     SET lifecycle = ?1, revision = ?2, updated_at_ms = ?3
                     WHERE task_id = ?4"
                ),
                rusqlite::params![
                    lifecycle_text(TaskLifecycle::Settled),
                    next_revision,
                    event.occurred_at_ms,
                    task_id.as_bytes().as_slice(),
                ],
            )?;
            require_one_change(tx, "task.settled")?;
        }
        Event::TaskReopened => {
            let task_id = require_task_id(event)?;
            let next_revision = require_next_revision(tx, shadow, task_id, event)?;
            require_reopen(tx, shadow, task_id)?;
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
            require_archive(tx, shadow, task_id)?;
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
        Event::TaskDeleted => {
            let task_id = require_task_id(event)?;
            let next_revision = require_next_revision(tx, shadow, task_id, event)?;
            require_delete(tx, shadow, task_id)?;
            let table = table_name("tasks", shadow);
            tx.execute(
                &format!(
                    "UPDATE {table}
                     SET lifecycle = ?1, revision = ?2, updated_at_ms = ?3
                     WHERE task_id = ?4"
                ),
                rusqlite::params![
                    lifecycle_text(TaskLifecycle::Deleted),
                    next_revision,
                    event.occurred_at_ms,
                    task_id.as_bytes().as_slice(),
                ],
            )?;
            require_one_change(tx, "task.deleted")?;
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
                    agent.provider_kind.wire_name(),
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
            upsert_provider_session_state(tx, shadow, task_id, agent.id, |session| {
                let _ = session;
                Ok(())
            })?;
        }
        Event::AgentProviderSessionBound {
            agent_session_id,
            resource_id,
            provider_session_id,
            runtime_generation,
        } => {
            let task_id = require_task_id(event)?;
            let table = table_name("agent_sessions", shadow);
            let changed = tx.execute(
                &format!(
                    "UPDATE {table}
                     SET provider_session_id = ?1, provider_resource_id = ?2,
                         revision = revision + 1
                     WHERE agent_session_id = ?3 AND task_id = ?4
                       AND lifecycle = 'open' AND runtime_generation = ?5
                       AND provider_session_id IS NULL"
                ),
                rusqlite::params![
                    provider_session_id.as_str(),
                    resource_id.as_bytes().as_slice(),
                    agent_session_id.as_bytes().as_slice(),
                    task_id.as_bytes().as_slice(),
                    u64_to_sqlite_i64("agent_sessions.runtime_generation", *runtime_generation)?,
                ],
            )?;
            if changed == 0 {
                let existing: Option<(String, Vec<u8>)> = tx
                    .query_row(
                        &format!(
                            "SELECT provider_session_id, provider_resource_id FROM {table}
                             WHERE agent_session_id = ?1 AND task_id = ?2
                               AND lifecycle = 'open' AND runtime_generation = ?3"
                        ),
                        rusqlite::params![
                            agent_session_id.as_bytes().as_slice(),
                            task_id.as_bytes().as_slice(),
                            u64_to_sqlite_i64(
                                "agent_sessions.runtime_generation",
                                *runtime_generation
                            )?,
                        ],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .optional()?;
                if existing
                    .as_ref()
                    .is_none_or(|(session_id, stored_resource_id)| {
                        session_id != provider_session_id.as_str()
                            || stored_resource_id.as_slice() != resource_id.as_bytes()
                    })
                {
                    return Err(StoreError::Projection(
                        "provider session binding fence mismatch".into(),
                    ));
                }
            }
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
        Event::UnstartedPrimaryProviderRebound {
            agent_session_id,
            provider_kind,
        } => {
            let task_id = require_task_id(event)?;
            validate_primary_agent(tx, shadow, task_id, *agent_session_id)?;
            // Match TaskSnapshot::is_unstarted_draft: refuse when any provider
            // projection already carries turn/wait/settlement facts, not only a
            // null SessionStart id.
            let mut projection_busy = false;
            upsert_provider_session_state(tx, shadow, task_id, *agent_session_id, |session| {
                projection_busy = session.current_turn.is_some()
                    || session.open_question.is_some()
                    || session.open_approval.is_some()
                    || !session.waits.is_empty()
                    || session.last_settlement.is_some();
                Ok(())
            })?;
            if projection_busy {
                return Err(StoreError::Projection(
                    "unstarted primary provider rebound rejected: provider projection busy".into(),
                ));
            }
            let table = table_name("agent_sessions", shadow);
            let changed = tx.execute(
                &format!(
                    "UPDATE {table}
                     SET provider_kind = ?1, revision = revision + 1
                     WHERE agent_session_id = ?2 AND task_id = ?3
                       AND lifecycle = 'open' AND provider_session_id IS NULL"
                ),
                rusqlite::params![
                    provider_kind.wire_name(),
                    agent_session_id.as_bytes().as_slice(),
                    task_id.as_bytes().as_slice(),
                ],
            )?;
            if changed != 1 {
                return Err(StoreError::Projection(
                    "unstarted primary provider rebound rejected".into(),
                ));
            }
            bump_task_revision(tx, shadow, task_id, event)?;
        }
        Event::SpecialistRequested {
            specialist_id,
            requested_by,
            purpose: _,
            agent,
            permission,
            workspace,
            action_epoch,
            runtime_generation,
            resource_id,
        } => {
            let task_id = require_task_id(event)?;
            let (task_lifecycle, stored_epoch) = read_task_lifecycle_epoch(tx, shadow, task_id)?;
            if task_lifecycle != lifecycle_text(TaskLifecycle::Open)
                || u64_from_nonnegative_i64("tasks.action_epoch", stored_epoch)? != *action_epoch
            {
                return Err(StoreError::Projection(
                    "specialist.requested action fence does not match task".into(),
                ));
            }
            if agent.id != *specialist_id
                || agent.task_id != task_id
                || agent.runtime_generation != *runtime_generation
                || !matches!(agent.role, AgentRole::Specialist { .. })
                || agent.lifecycle != AgentSessionLifecycle::Open
                || agent.revision != 0
                || !matches!(permission, SpecialistPermission::ReadOnly)
            {
                return Err(StoreError::Projection(
                    "specialist.requested agent facts are invalid".into(),
                ));
            }
            agent
                .validate_for_registration()
                .map_err(|err| StoreError::Projection(err.to_string()))?;
            workspace
                .validate()
                .map_err(|err| StoreError::Projection(err.to_string()))?;
            let (requester_task, requester_role, requester_lifecycle, requester_generation) =
                load_agent_projection_state(tx, shadow, *requested_by)?;
            if requester_task != task_id
                || !matches!(requester_role, AgentRole::Primary)
                || requester_lifecycle != AgentSessionLifecycle::Open
                || requester_generation != *runtime_generation
            {
                return Err(StoreError::Projection(
                    "specialist.requested requester fence is invalid".into(),
                ));
            }
            if let Some(resource_id) = resource_id {
                let resource = tx
                    .query_row(
                        &format!(
                            "SELECT task_id, runtime_generation FROM {} WHERE resource_id = ?1",
                            table_name("resources", shadow)
                        ),
                        [resource_id.as_bytes().as_slice()],
                        |row| Ok((row.get::<_, Option<Vec<u8>>>(0)?, row.get::<_, i64>(1)?)),
                    )
                    .optional()?;
                let Some((Some(resource_task), resource_generation)) = resource else {
                    return Err(StoreError::Projection(
                        "specialist.requested resource is missing or unowned".into(),
                    ));
                };
                if resource_task.as_slice() != task_id.as_bytes().as_slice()
                    || resource_generation
                        != u64_to_sqlite_i64("resources.runtime_generation", *runtime_generation)?
                {
                    return Err(StoreError::Projection(
                        "specialist.requested resource fence is invalid".into(),
                    ));
                }
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
                    agent.provider_kind.wire_name(),
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
            upsert_provider_session_state(tx, shadow, task_id, agent.id, |session| {
                let _ = session;
                Ok(())
            })?;
        }
        Event::PrimaryPromoted {
            previous,
            promoted,
            action_epoch,
            runtime_generation,
        } => {
            let task_id = require_task_id(event)?;
            let (task_lifecycle, stored_epoch) = read_task_lifecycle_epoch(tx, shadow, task_id)?;
            if task_lifecycle != lifecycle_text(TaskLifecycle::Open)
                || u64_from_nonnegative_i64("tasks.action_epoch", stored_epoch)? != *action_epoch
            {
                return Err(StoreError::Projection(
                    "primary_agent.promoted action fence does not match task".into(),
                ));
            }
            let current_primary: Option<Vec<u8>> = tx.query_row(
                &format!(
                    "SELECT primary_agent_session_id FROM {} WHERE task_id = ?1",
                    table_name("tasks", shadow)
                ),
                [task_id.as_bytes().as_slice()],
                |row| row.get(0),
            )?;
            if current_primary.as_deref() != Some(previous.as_bytes().as_slice()) {
                return Err(StoreError::Projection(
                    "primary_agent.promoted previous primary mismatch".into(),
                ));
            }
            let (previous_task, previous_role, previous_lifecycle, previous_generation) =
                load_agent_projection_state(tx, shadow, *previous)?;
            let (promoted_task, promoted_role, promoted_lifecycle, promoted_generation) =
                load_agent_projection_state(tx, shadow, *promoted)?;
            if previous_task != task_id
                || promoted_task != task_id
                || !matches!(previous_role, AgentRole::Primary)
                || !matches!(promoted_role, AgentRole::Specialist { .. })
                || previous_lifecycle != AgentSessionLifecycle::Open
                || promoted_lifecycle != AgentSessionLifecycle::Open
                || previous_generation != *runtime_generation
                || promoted_generation != *runtime_generation
            {
                return Err(StoreError::Projection(
                    "primary_agent.promoted agent fences are invalid".into(),
                ));
            }
            let previous_role = AgentRole::specialist("primary")
                .map_err(|err| StoreError::Projection(err.to_string()))?;
            let table = table_name("agent_sessions", shadow);
            tx.execute(
                &format!("UPDATE {table} SET role = ?1 WHERE agent_session_id = ?2"),
                rusqlite::params![pack(&previous_role)?, previous.as_bytes().as_slice(),],
            )?;
            require_one_change(tx, "primary_agent.promoted previous role")?;
            tx.execute(
                &format!("UPDATE {table} SET role = ?1 WHERE agent_session_id = ?2"),
                rusqlite::params![pack(&AgentRole::Primary)?, promoted.as_bytes().as_slice()],
            )?;
            require_one_change(tx, "primary_agent.promoted promoted role")?;
            let task_table = table_name("tasks", shadow);
            tx.execute(
                &format!(
                    "UPDATE {task_table}
                     SET primary_agent_session_id = ?1
                     WHERE task_id = ?2"
                ),
                rusqlite::params![
                    promoted.as_bytes().as_slice(),
                    task_id.as_bytes().as_slice()
                ],
            )?;
            require_one_change(tx, "primary_agent.promoted primary selection")?;
            bump_task_revision(tx, shadow, task_id, event)?;
        }
        Event::SpecialistHandoffRecorded {
            specialist_id,
            artifact,
            structured,
            action_epoch,
            runtime_generation,
        } => {
            let task_id = require_task_id(event)?;
            let (_, stored_epoch) = read_task_lifecycle_epoch(tx, shadow, task_id)?;
            if u64_from_nonnegative_i64("tasks.action_epoch", stored_epoch)? != *action_epoch
                || artifact.task_id != task_id
                || artifact.kind != ArtifactKind::ReviewReport
            {
                return Err(StoreError::Projection(
                    "specialist.handoff_recorded task fence is invalid".into(),
                ));
            }
            artifact
                .validate()
                .map_err(|err| StoreError::Projection(err.to_string()))?;
            verify_inline_content_digest(artifact)
                .map_err(|err| StoreError::Projection(err.to_string()))?;
            let ArtifactContentRef::InlineUtf8(body) = &artifact.content_ref else {
                return Err(StoreError::Projection(
                    "specialist handoff artifact must be inline".into(),
                ));
            };
            if body.len() > MAX_SPECIALIST_RAW_ARTIFACT_BYTES {
                return Err(StoreError::Projection(
                    "specialist handoff artifact exceeds bound".into(),
                ));
            }
            if *structured {
                structured_specialist_result(artifact)
                    .map_err(|err| StoreError::Projection(err.to_string()))?;
            }
            let (specialist_task, specialist_role, specialist_lifecycle, specialist_generation) =
                load_agent_projection_state(tx, shadow, *specialist_id)?;
            if specialist_task != task_id
                || !matches!(specialist_role, AgentRole::Specialist { .. })
                || specialist_lifecycle != AgentSessionLifecycle::Open
                || specialist_generation != *runtime_generation
            {
                return Err(StoreError::Projection(
                    "specialist handoff agent fence is invalid".into(),
                ));
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
            let agent_table = table_name("agent_sessions", shadow);
            tx.execute(
                &format!(
                    "UPDATE {agent_table}
                     SET lifecycle = ?1
                     WHERE agent_session_id = ?2"
                ),
                rusqlite::params![
                    agent_lifecycle_text(AgentSessionLifecycle::Closed),
                    specialist_id.as_bytes().as_slice(),
                ],
            )?;
            require_one_change(tx, "specialist handoff close")?;
            bump_task_revision(tx, shadow, task_id, event)?;
        }
        Event::SpecialistClosed {
            specialist_id,
            action_epoch,
            runtime_generation,
        } => {
            let task_id = require_task_id(event)?;
            let (_, stored_epoch) = read_task_lifecycle_epoch(tx, shadow, task_id)?;
            if u64_from_nonnegative_i64("tasks.action_epoch", stored_epoch)? != *action_epoch {
                return Err(StoreError::Projection(
                    "specialist.closed action fence does not match task".into(),
                ));
            }
            let (specialist_task, specialist_role, specialist_lifecycle, specialist_generation) =
                load_agent_projection_state(tx, shadow, *specialist_id)?;
            if specialist_task != task_id
                || !matches!(specialist_role, AgentRole::Specialist { .. })
                || specialist_lifecycle != AgentSessionLifecycle::Open
                || specialist_generation != *runtime_generation
            {
                return Err(StoreError::Projection(
                    "specialist.closed agent fence is invalid".into(),
                ));
            }
            let table = table_name("agent_sessions", shadow);
            tx.execute(
                &format!("UPDATE {table} SET lifecycle = ?1 WHERE agent_session_id = ?2"),
                rusqlite::params![
                    agent_lifecycle_text(AgentSessionLifecycle::Closed),
                    specialist_id.as_bytes().as_slice(),
                ],
            )?;
            require_one_change(tx, "specialist.closed lifecycle")?;
            bump_task_revision(tx, shadow, task_id, event)?;
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
            // Every terminal gets durable facts; only a plain shell (a terminal
            // recipe carrying a launch) is a user-facing strip tab. Keep this in
            // lockstep with the `ResourceRegistered` arm of `apply_into`.
            if resource.resource_kind == crate::domain::resource::ResourceKind::Terminal {
                let title = match &resource.recipe {
                    crate::domain::resource::ResourceRecipe::Terminal { title, .. } => {
                        title.clone()
                    }
                    _ => None,
                };
                tx.execute(
                    &format!(
                        "INSERT INTO {} (
                            resource_id, task_id, title, live_cwd, exit_code, exit_summary,
                            exited_at_ms, created_at_ms, last_activity_at_ms
                         ) VALUES (?1, ?2, ?3, NULL, NULL, NULL, NULL, ?4, ?4)",
                        table_name("terminal_facts", shadow)
                    ),
                    rusqlite::params![
                        resource.id.as_bytes().as_slice(),
                        task_id.as_bytes().as_slice(),
                        title,
                        event.occurred_at_ms,
                    ],
                )?;
                if resource.recipe.is_plain_shell() {
                    append_to_terminal_strip(tx, shadow, task_id, resource.id)?;
                }
            }
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
            // The resource row survives a release; its terminal facts and its
            // strip entry do not. `apply_into` drops exactly the same two.
            tx.execute(
                &format!(
                    "DELETE FROM {} WHERE resource_id = ?1",
                    table_name("terminal_facts", shadow)
                ),
                rusqlite::params![resource_id.as_bytes().as_slice()],
            )?;
            remove_from_terminal_strip(tx, shadow, task_id, *resource_id)?;
            bump_task_revision(tx, shadow, task_id, event)?;
        }
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
        } => {
            let task_id = require_task_id(event)?;
            let (lifecycle, epoch) = read_task_lifecycle_epoch(tx, shadow, task_id)?;
            let stored_epoch = u64_from_nonnegative_i64("tasks.action_epoch", epoch)?;
            if stored_epoch != *action_epoch {
                return Err(StoreError::Projection(
                    "provider input action_epoch does not match task fence".into(),
                ));
            }
            let reactivate = lifecycle == lifecycle_text(TaskLifecycle::Settled)
                && matches!(action, crate::domain::ProviderInputAction::SendNow { .. });
            if lifecycle != lifecycle_text(TaskLifecycle::Open) && !reactivate {
                return Err(StoreError::Projection(
                    "provider input requires Open, or Settled with SendNow, task lifecycle".into(),
                ));
            }
            if delivery.is_delivered() {
                return Err(StoreError::Projection(
                    "provider input cannot project Delivered without a destination adapter".into(),
                ));
            }
            upsert_provider_session_state(tx, shadow, task_id, *agent_session_id, |session| {
                let context_turn =
                    if matches!(action, crate::domain::ProviderInputAction::SendNow { .. })
                        && session.can_begin_send_now_turn(*turn_id)
                    {
                        None
                    } else {
                        session.current_turn
                    };
                let context = read_provider_fence_context(
                    tx,
                    shadow,
                    task_id,
                    *agent_session_id,
                    context_turn,
                    false,
                )?;
                let fence = crate::domain::provider_input::ProviderFenceIdentity::new_with_identity(
                    Some(*command_id),
                    Some(task_id),
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
                    Some(&context),
                )
                .map_err(|err| StoreError::Projection(err.to_string()))?;
                match action {
                    crate::domain::ProviderInputAction::AnswerQuestion { question_id, .. } => {
                        if session.question_winners.contains_key(question_id) {
                            return Err(StoreError::Projection(
                                "provider question winner already exists".into(),
                            ));
                        }
                        if session.open_question != Some(*question_id) {
                            return Err(StoreError::Projection(
                                "provider question is not open".into(),
                            ));
                        }
                    }
                    crate::domain::ProviderInputAction::ResolveApproval { approval_id, .. } => {
                        if session.approval_winners.contains_key(approval_id) {
                            return Err(StoreError::Projection(
                                "provider approval winner already exists".into(),
                            ));
                        }
                        if session.open_approval != Some(*approval_id) {
                            return Err(StoreError::Projection(
                                "provider approval is not open".into(),
                            ));
                        }
                    }
                    _ => {}
                }
                session.current_turn = Some(*turn_id);
                session.last_settlement =
                    Some(crate::domain::provider_input::ProviderInputSettlement {
                        command_id: *command_id,
                        operation_id: Some(*operation_id),
                        intent: crate::domain::provider_input::ProviderIntentPhase::Accepted,
                        delivery: *delivery,
                    });
                let winner = crate::domain::provider_input::ProviderResolutionWinner {
                    command_id: *command_id,
                    client_id: *client_id,
                    accepted_at_ms: event.occurred_at_ms,
                };
                match action {
                    crate::domain::ProviderInputAction::AnswerQuestion {
                        question_id: qid, ..
                    } => {
                        session
                            .bounded_insert_question_winner(*qid, winner)
                            .map_err(|err| StoreError::Projection(err.to_string()))?;
                        session.open_question = None;
                    }
                    crate::domain::ProviderInputAction::ResolveApproval {
                        approval_id: aid,
                        ..
                    } => {
                        session
                            .bounded_insert_approval_winner(*aid, winner)
                            .map_err(|err| StoreError::Projection(err.to_string()))?;
                        session.open_approval = None;
                    }
                    _ => {}
                }
                if *wait {
                    session
                        .bounded_insert_wait(
                            *command_id,
                            crate::domain::provider_input::ProviderWaitRecord {
                                fence: crate::domain::provider_input::ProviderWaitFence::new_with_identity(
                                    *command_id,
                                    task_id,
                                    *operation_id,
                                    *action_epoch,
                                    *agent_session_id,
                                    provider_kind.clone(),
                                    provider_session_id.clone(),
                                    *runtime_generation,
                                    *turn_id,
                                    *question_id,
                                    *approval_id,
                                ),
                                pending: true,
                            },
                        )
                        .map_err(|err| StoreError::Projection(err.to_string()))?;
                }
                Ok(())
            })?;
            if reactivate {
                let table = table_name("tasks", shadow);
                tx.execute(
                    &format!(
                        "UPDATE {table}
                         SET lifecycle = ?1
                         WHERE task_id = ?2 AND lifecycle = ?3"
                    ),
                    rusqlite::params![
                        lifecycle_text(TaskLifecycle::Open),
                        task_id.as_bytes().as_slice(),
                        lifecycle_text(TaskLifecycle::Settled),
                    ],
                )?;
                require_one_change(tx, "provider input settled task reactivation")?;
            }
            bump_task_revision(tx, shadow, task_id, event)?;
        }
        Event::ProviderQuestionPresented {
            agent_session_id,
            provider_kind,
            provider_session_id,
            runtime_generation,
            turn_id,
            question_id,
            action_epoch,
        } => {
            let task_id = require_task_id(event)?;
            upsert_provider_session_state(tx, shadow, task_id, *agent_session_id, |session| {
                let context = read_provider_fence_context(
                    tx,
                    shadow,
                    task_id,
                    *agent_session_id,
                    session.current_turn,
                    false,
                )?;
                let fence = crate::domain::provider_input::ProviderFenceIdentity::new_with_identity(
                    None,
                    Some(task_id),
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
                crate::domain::provider_input::validate_provider_fence(
                    &fence,
                    None,
                    None,
                    Some(&context),
                )
                .map_err(|err| StoreError::Projection(err.to_string()))?;
                if session.question_winners.contains_key(question_id) {
                    return Err(StoreError::Projection(
                        "provider question winner already exists".into(),
                    ));
                }
                if session.open_question.is_some() {
                    return Err(StoreError::Projection(
                        "provider question is already open".into(),
                    ));
                }
                session.current_turn = Some(*turn_id);
                session.open_question = Some(*question_id);
                Ok(())
            })?;
            bump_task_revision(tx, shadow, task_id, event)?;
        }
        Event::ProviderApprovalPresented {
            agent_session_id,
            provider_kind,
            provider_session_id,
            runtime_generation,
            turn_id,
            approval_id,
            action_epoch,
        } => {
            let task_id = require_task_id(event)?;
            upsert_provider_session_state(tx, shadow, task_id, *agent_session_id, |session| {
                let context = read_provider_fence_context(
                    tx,
                    shadow,
                    task_id,
                    *agent_session_id,
                    session.current_turn,
                    false,
                )?;
                let fence = crate::domain::provider_input::ProviderFenceIdentity::new_with_identity(
                    None,
                    Some(task_id),
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
                crate::domain::provider_input::validate_provider_fence(
                    &fence,
                    None,
                    None,
                    Some(&context),
                )
                .map_err(|err| StoreError::Projection(err.to_string()))?;
                if session.approval_winners.contains_key(approval_id) {
                    return Err(StoreError::Projection(
                        "provider approval winner already exists".into(),
                    ));
                }
                if session.open_approval.is_some() {
                    return Err(StoreError::Projection(
                        "provider approval is already open".into(),
                    ));
                }
                session.current_turn = Some(*turn_id);
                session.open_approval = Some(*approval_id);
                Ok(())
            })?;
            bump_task_revision(tx, shadow, task_id, event)?;
        }
        Event::ProviderWaitSettled { fence } => {
            let task_id = require_task_id(event)?;
            upsert_provider_session_state(
                tx,
                shadow,
                task_id,
                fence.agent_session_id(),
                |session| {
                    let context = read_provider_fence_context(
                        tx,
                        shadow,
                        task_id,
                        fence.agent_session_id(),
                        session.current_turn,
                        true,
                    )?;
                    crate::domain::provider_input::validate_provider_fence(
                        &fence.identity(),
                        None,
                        None,
                        Some(&context),
                    )
                    .map_err(|err| StoreError::Projection(err.to_string()))?;
                    let record = session.waits.get_mut(&fence.command_id()).ok_or_else(|| {
                        StoreError::Projection("provider wait settlement missing wait".into())
                    })?;
                    if !record.fence.matches(fence) {
                        return Err(StoreError::Projection(
                            "provider wait settlement fence mismatch".into(),
                        ));
                    }
                    if !record.pending {
                        return Err(StoreError::Projection(
                            "provider wait settlement is already settled".into(),
                        ));
                    }
                    record.pending = false;
                    Ok(())
                },
            )?;
            bump_task_revision(tx, shadow, task_id, event)?;
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
            let task_id = require_task_id(event)?;
            upsert_provider_session_state(tx, shadow, task_id, *agent_session_id, |session| {
                let context = read_provider_fence_context(
                    tx,
                    shadow,
                    task_id,
                    *agent_session_id,
                    session.current_turn,
                    true,
                )?;
                let fence = crate::domain::provider_input::ProviderFenceIdentity::new_with_identity(
                    Some(*command_id),
                    Some(task_id),
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
                    None,
                    None,
                    Some(&context),
                )
                .map_err(|err| StoreError::Projection(err.to_string()))?;
                let Some(settlement) = session.last_settlement else {
                    return Err(StoreError::Projection(
                        "provider delivery missing accepted settlement".into(),
                    ));
                };
                if settlement.command_id != *command_id
                    || settlement.operation_id != Some(*operation_id)
                {
                    // Acceptance and physical delivery are separate ordered
                    // lanes. A newer accepted input may already own the
                    // presentation slot when an earlier dispatch settles. Its
                    // exact operation/outbox lineage was validated before this
                    // derived event was appended, so preserve the newer slot.
                    return Ok(());
                }
                if settlement.delivery.is_delivered() {
                    return Err(StoreError::Projection(
                        "provider delivery settlement mismatch".into(),
                    ));
                }
                session.last_settlement =
                    Some(crate::domain::provider_input::ProviderInputSettlement {
                        command_id: *command_id,
                        operation_id: Some(*operation_id),
                        intent: crate::domain::provider_input::ProviderIntentPhase::Accepted,
                        delivery:
                            crate::domain::provider_input::ProviderDeliveryVisibility::delivered(),
                    });
                Ok(())
            })?;
        }
        Event::HostCloseBegun {
            operation_id,
            action_epoch,
            inspection_id,
        } => {
            if event.task_id.is_some() || event.task_revision.is_some() {
                return Err(StoreError::Projection(
                    "host.close_begun requires NULL task_id and task_revision".into(),
                ));
            }
            let table = table_name("host_admission", shadow);
            let existing: Option<i64> = tx
                .query_row(
                    &format!("SELECT singleton_key FROM {table} WHERE singleton_key = 1"),
                    [],
                    |row| row.get(0),
                )
                .optional()?;
            if existing.is_some() {
                return Err(StoreError::Projection(
                    "host.close_begun requires Open host admission (no existing row)".into(),
                ));
            }
            tx.execute(
                &format!(
                    "INSERT INTO {table} (
                        singleton_key, operation_id, action_epoch, inspection_id, updated_at_ms
                     ) VALUES (1, ?1, ?2, ?3, ?4)"
                ),
                rusqlite::params![
                    operation_id.as_bytes().as_slice(),
                    u64_to_sqlite_i64("host_admission.action_epoch", *action_epoch)?,
                    u64_to_sqlite_i64("host_admission.inspection_id", *inspection_id)?,
                    event.occurred_at_ms,
                ],
            )?;
        }
        Event::HostCleanupBranchCompleted {
            operation_id,
            action_epoch,
            branch,
            outcome,
        } => {
            if event.task_id.is_some() || event.task_revision.is_some() {
                return Err(StoreError::Projection(
                    "host.cleanup_branch_completed requires NULL task_id and task_revision".into(),
                ));
            }
            let admission: Option<(Vec<u8>, i64)> = tx
                .query_row(
                    &format!(
                        "SELECT operation_id, action_epoch FROM {} WHERE singleton_key = 1",
                        table_name("host_admission", shadow)
                    ),
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            let Some((admission_op, admission_epoch_i64)) = admission else {
                return Err(StoreError::Projection(
                    "host.cleanup_branch_completed requires Closing host admission".into(),
                ));
            };
            if admission_op.as_slice() != operation_id.as_bytes().as_slice() {
                return Err(StoreError::Projection(
                    "host.cleanup_branch_completed operation_id must match Closing admission"
                        .into(),
                ));
            }
            let admission_epoch =
                u64_from_nonnegative_i64("host_admission.action_epoch", admission_epoch_i64)?;
            if admission_epoch != *action_epoch {
                return Err(StoreError::Projection(
                    "host.cleanup_branch_completed action_epoch must match Closing admission"
                        .into(),
                ));
            }
            let remaining_count = outcome.remaining_count();
            match outcome {
                HostCleanupBranchOutcome::Succeeded if remaining_count != 0 => {
                    return Err(StoreError::Projection(
                        "host.cleanup_branch_completed Succeeded requires remaining_count 0".into(),
                    ));
                }
                HostCleanupBranchOutcome::Failed { .. } if remaining_count == 0 => {
                    return Err(StoreError::Projection(
                        "host.cleanup_branch_completed Failed requires remaining_count > 0".into(),
                    ));
                }
                _ => {}
            }
            let table = table_name("host_cleanup_branches", shadow);
            let existing: Option<i64> = tx
                .query_row(
                    &format!("SELECT 1 FROM {table} WHERE operation_id = ?1 AND branch = ?2"),
                    rusqlite::params![operation_id.as_bytes().as_slice(), branch.as_str()],
                    |row| row.get(0),
                )
                .optional()?;
            if existing.is_some() {
                return Err(StoreError::Projection(
                    "host.cleanup_branch_completed duplicate branch is corruption".into(),
                ));
            }
            let expected_index = HostCleanupBranch::ORDER
                .iter()
                .position(|ordered| ordered == branch)
                .ok_or_else(|| {
                    StoreError::Projection("host.cleanup_branch_completed unknown branch".into())
                })?;
            for prior in &HostCleanupBranch::ORDER[..expected_index] {
                let prior_exists: Option<i64> = tx
                    .query_row(
                        &format!("SELECT 1 FROM {table} WHERE operation_id = ?1 AND branch = ?2"),
                        rusqlite::params![operation_id.as_bytes().as_slice(), prior.as_str()],
                        |row| row.get(0),
                    )
                    .optional()?;
                if prior_exists.is_none() {
                    return Err(StoreError::Projection(
                        "host.cleanup_branch_completed requires exact ORDER prefix before branch"
                            .into(),
                    ));
                }
            }
            let inserted = tx.execute(
                &format!(
                    "INSERT INTO {table} (
                        operation_id, branch, result, remaining_count, completed_at_ms
                     ) VALUES (?1, ?2, ?3, ?4, ?5)"
                ),
                rusqlite::params![
                    operation_id.as_bytes().as_slice(),
                    branch.as_str(),
                    outcome.result_str(),
                    u64_to_sqlite_i64("host_cleanup_branches.remaining_count", remaining_count)?,
                    event.occurred_at_ms,
                ],
            )?;
            if inserted != 1 {
                return Err(StoreError::Projection(
                    "host.cleanup_branch_completed insert affected unexpected rows".into(),
                ));
            }
        }
        Event::OperationAccepted(fact) => {
            require_valid_operation_fact(fact.validate())?;
            classify_settled_lineage_fence(
                fact.action_epoch,
                fact.resource_id,
                fact.runtime_generation,
                event.task_id,
                true,
            )?;
            if event.occurred_at_ms != fact.accepted_at_ms {
                return Err(StoreError::Projection(
                    "operation.accepted envelope occurred_at_ms must equal fact.accepted_at_ms"
                        .into(),
                ));
            }
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
            require_valid_operation_fact(fact.validate())?;
            let kind = classify_settled_lineage_fence(
                fact.action_epoch,
                fact.resource_id,
                fact.runtime_generation,
                event.task_id,
                true,
            )?;
            if matches!(kind, SettledLineageKind::HostAdmission) {
                if event.task_id.is_some() || event.task_revision.is_some() {
                    return Err(StoreError::Projection(
                        "host-admission operation.settled requires NULL task_id and task_revision"
                            .into(),
                    ));
                }
                if event.occurred_at_ms != fact.settled_at_ms {
                    return Err(StoreError::Projection(
                        "operation.settled envelope occurred_at_ms must equal fact.settled_at_ms"
                            .into(),
                    ));
                }
                let (journal, journal_max_completed_at_ms) =
                    load_host_cleanup_journal_outcomes(tx, shadow, fact.operation_id)?;
                let ordered_branch_event_ids =
                    load_ordered_host_cleanup_branch_event_ids(tx, shadow, fact.operation_id)?;
                let accepted_at_ms = load_operation_accepted_at_ms(tx, shadow, fact.operation_id)?;
                let prior = load_prior_event_row(tx, event.sequence)?;
                let prior_ref = prior
                    .as_ref()
                    .map(|(id, ev, rev, occurred, task)| (*id, ev, *rev, *occurred, *task));
                validate_host_admission_settled_lineage(
                    fact,
                    event.occurred_at_ms,
                    prior_ref,
                    &ordered_branch_event_ids,
                    &journal,
                    journal_max_completed_at_ms,
                    accepted_at_ms,
                    fact.command_id,
                    fact.operation_id,
                    fact.action_epoch.ok_or(StoreError::Projection(
                        "host-admission OperationSettled requires action_epoch".into(),
                    ))?,
                    true,
                )?;
                apply_operation_outcome(
                    tx,
                    shadow,
                    event.task_id,
                    fact.command_id,
                    fact.operation_id.as_bytes(),
                    fact.action_epoch,
                    fact.resource_id,
                    fact.runtime_generation,
                    Some(&fact.source),
                    "settled",
                    Some(pack(&fact.result_event_ids)?),
                    None,
                    fact.settled_at_ms,
                )?;
                return Ok(());
            }
            if event.occurred_at_ms != fact.settled_at_ms {
                return Err(StoreError::Projection(
                    "operation.settled envelope occurred_at_ms must equal fact.settled_at_ms"
                        .into(),
                ));
            }
            apply_operation_outcome(
                tx,
                shadow,
                event.task_id,
                fact.command_id,
                fact.operation_id.as_bytes(),
                fact.action_epoch,
                fact.resource_id,
                fact.runtime_generation,
                Some(&fact.source),
                "settled",
                Some(pack(&fact.result_event_ids)?),
                None,
                fact.settled_at_ms,
            )?;
            if matches!(kind, SettledLineageKind::PromptLibrary) {
                validate_prompt_library_settled_against_history(tx, shadow, event, fact)?;
            } else if matches!(kind, SettledLineageKind::Pure) {
                validate_pure_settled_against_history(tx, shadow, event, fact)?;
            }
        }
        Event::OperationFailed(fact) => {
            require_valid_operation_fact(fact.validate())?;
            let kind = classify_settled_lineage_fence(
                fact.action_epoch,
                fact.resource_id,
                fact.runtime_generation,
                event.task_id,
                true,
            )?;
            if matches!(kind, SettledLineageKind::HostAdmission) {
                if event.task_id.is_some() || event.task_revision.is_some() {
                    return Err(StoreError::Projection(
                        "host-admission operation.failed requires NULL task_id and task_revision"
                            .into(),
                    ));
                }
                let (journal, journal_max_completed_at_ms) =
                    load_host_cleanup_journal_outcomes(tx, shadow, fact.operation_id)?;
                let prior = load_prior_event_row(tx, event.sequence)?;
                let prior_ref = prior
                    .as_ref()
                    .map(|(id, ev, rev, occurred, task)| (*id, ev, *rev, *occurred, *task));
                validate_host_admission_cleanup_failed_lineage(
                    fact,
                    event.occurred_at_ms,
                    prior_ref,
                    &journal,
                    journal_max_completed_at_ms,
                    fact.command_id,
                    fact.operation_id,
                    fact.action_epoch.ok_or(StoreError::Projection(
                        "host-admission CleanupFailed requires action_epoch".into(),
                    ))?,
                    true,
                )?;
            } else {
                reject_pure_non_settled_terminal(kind, true)?;
            }
            if event.occurred_at_ms != fact.settled_at_ms {
                return Err(StoreError::Projection(
                    "operation.failed envelope occurred_at_ms must equal fact.settled_at_ms".into(),
                ));
            }
            apply_operation_outcome(
                tx,
                shadow,
                event.task_id,
                fact.command_id,
                fact.operation_id.as_bytes(),
                fact.action_epoch,
                fact.resource_id,
                fact.runtime_generation,
                Some(&fact.source),
                "failed",
                None,
                Some(error_code_text(fact.code)),
                fact.settled_at_ms,
            )?;
        }
        Event::OperationCancelled(fact) => {
            require_valid_operation_fact(fact.validate())?;
            let kind = classify_settled_lineage_fence(
                fact.action_epoch,
                fact.resource_id,
                fact.runtime_generation,
                event.task_id,
                true,
            )?;
            reject_pure_non_settled_terminal(kind, true)?;
            if event.occurred_at_ms != fact.settled_at_ms {
                return Err(StoreError::Projection(
                    "operation.cancelled envelope occurred_at_ms must equal fact.settled_at_ms"
                        .into(),
                ));
            }
            apply_operation_outcome(
                tx,
                shadow,
                event.task_id,
                fact.command_id,
                fact.operation_id.as_bytes(),
                fact.action_epoch,
                fact.resource_id,
                fact.runtime_generation,
                None,
                "cancelled",
                None,
                Some(cancel_text(fact.reason)),
                fact.settled_at_ms,
            )?;
        }
        Event::OperationUncertain(fact) => {
            require_valid_operation_fact(fact.validate())?;
            let kind = classify_settled_lineage_fence(
                fact.action_epoch,
                fact.resource_id,
                fact.runtime_generation,
                event.task_id,
                true,
            )?;
            reject_pure_non_settled_terminal(kind, true)?;
            if event.occurred_at_ms != fact.observed_at_ms {
                return Err(StoreError::Projection(
                    "operation.uncertain envelope occurred_at_ms must equal fact.observed_at_ms"
                        .into(),
                ));
            }
            apply_operation_outcome(
                tx,
                shadow,
                event.task_id,
                fact.command_id,
                fact.operation_id.as_bytes(),
                fact.action_epoch,
                fact.resource_id,
                fact.runtime_generation,
                None,
                "uncertain",
                None,
                Some(uncertain_text(fact.code)),
                fact.observed_at_ms,
            )?;
        }
        Event::Browser(fact) => {
            let task_id = event.task_id.ok_or_else(|| {
                StoreError::Projection("browser.fact requires task_id and task_revision".into())
            })?;
            if event.task_revision.is_none() {
                return Err(StoreError::Projection(
                    "browser.fact requires task_id and task_revision".into(),
                ));
            }
            if fact.task_id() != task_id {
                return Err(StoreError::Projection(
                    "browser.fact embedded task_id disagrees with DomainEvent.task_id".into(),
                ));
            }
        }
        Event::TerminalRenamed { resource_id, title } => {
            let task_id = require_task_id(event)?;
            tx.execute(
                &format!(
                    "UPDATE {} SET title = ?1 WHERE resource_id = ?2",
                    table_name("terminal_facts", shadow)
                ),
                rusqlite::params![title, resource_id.as_bytes().as_slice()],
            )?;
            require_one_change(tx, "terminal rename")?;
            // The title also lives in the terminal recipe, and `apply_into`
            // rewrites it there. Leaving the stored recipe behind would make the
            // projection disagree with a durable replay of the same history,
            // which `validate_task_history_and_projection_with_agent_sessions`
            // reads as durable corruption.
            rewrite_terminal_recipe_title(tx, shadow, *resource_id, Some(title.clone()))?;
            bump_task_revision(tx, shadow, task_id, event)?;
        }
        // The three host-reported facts consume no task revision by design, so
        // none of them bumps `tasks.revision`.
        Event::TerminalCwdReported { resource_id, cwd } => {
            tx.execute(
                &format!(
                    "UPDATE {} SET live_cwd = ?1, last_activity_at_ms = ?2 WHERE resource_id = ?3",
                    table_name("terminal_facts", shadow)
                ),
                rusqlite::params![
                    cwd.to_string_lossy().into_owned(),
                    event.occurred_at_ms,
                    resource_id.as_bytes().as_slice(),
                ],
            )?;
            require_one_change(tx, "terminal cwd report")?;
        }
        Event::TerminalExited {
            resource_id,
            code,
            summary,
        } => {
            tx.execute(
                &format!(
                    "UPDATE {} SET exit_code = ?1, exit_summary = ?2, exited_at_ms = ?3 \
                     WHERE resource_id = ?4",
                    table_name("terminal_facts", shadow)
                ),
                rusqlite::params![
                    code,
                    summary,
                    event.occurred_at_ms,
                    resource_id.as_bytes().as_slice(),
                ],
            )?;
            require_one_change(tx, "terminal exit")?;
        }
        Event::TerminalActivity { resource_id } => {
            tx.execute(
                &format!(
                    "UPDATE {} SET last_activity_at_ms = ?1 WHERE resource_id = ?2",
                    table_name("terminal_facts", shadow)
                ),
                rusqlite::params![event.occurred_at_ms, resource_id.as_bytes().as_slice()],
            )?;
            require_one_change(tx, "terminal activity")?;
        }
        Event::TaskTerminalStripSet { strip } => {
            let task_id = require_task_id(event)?;
            write_terminal_strip(tx, shadow, task_id, strip)?;
            bump_task_revision(tx, shadow, task_id, event)?;
        }
    }
    enforce_derived_result_lineage(tx, event)?;
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

fn read_provider_fence_context(
    tx: &Transaction<'_>,
    shadow: bool,
    task_id: TaskId,
    agent_session_id: AgentSessionId,
    current_turn: Option<crate::domain::id::TurnId>,
    allow_closing: bool,
) -> Result<crate::domain::provider_input::ProviderFenceContext, StoreError> {
    let table = table_name("agent_sessions", shadow);
    let row: Result<(Vec<u8>, String, String, Option<String>, i64), rusqlite::Error> = tx.query_row(
        &format!(
            "SELECT task_id, lifecycle, provider_kind, provider_session_id, runtime_generation FROM {table}
             WHERE agent_session_id = ?1"
        ),
        [agent_session_id.as_bytes().as_slice()],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
    );
    let (agent_task_bytes, lifecycle, provider_kind, provider_session_id, runtime_generation) =
        match row {
            Ok(value) => value,
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                return Err(StoreError::Projection(
                    "provider fence agent session not found".into(),
                ))
            }
            Err(err) => return Err(err.into()),
        };
    let agent_task_bytes: [u8; 16] = agent_task_bytes.try_into().map_err(|bytes: Vec<u8>| {
        StoreError::Projection(format!(
            "provider fence agent task id must be 16 bytes, got {}",
            bytes.len()
        ))
    })?;
    let agent_task_id = TaskId::from_bytes(agent_task_bytes)
        .map_err(|err| StoreError::Projection(err.to_string()))?;
    let lifecycle = match lifecycle.as_str() {
        "open" => crate::domain::agent::AgentSessionLifecycle::Open,
        "closing" => crate::domain::agent::AgentSessionLifecycle::Closing,
        "closed" => crate::domain::agent::AgentSessionLifecycle::Closed,
        other => {
            return Err(StoreError::Projection(format!(
                "provider fence agent lifecycle is invalid: {other}"
            )))
        }
    };
    let provider_kind = crate::domain::provider_input::provider_kind_from_wire(&provider_kind)
        .map_err(|err| StoreError::Projection(err.to_string()))?;
    let provider_session_id = provider_session_id
        .map(crate::domain::agent::ProviderSessionId::new)
        .transpose()
        .map_err(|err| StoreError::Projection(err.to_string()))?;
    let (_, action_epoch) = read_task_lifecycle_epoch(tx, shadow, task_id)?;
    Ok(crate::domain::provider_input::ProviderFenceContext {
        task_id,
        agent_session_id,
        agent_task_id,
        provider_kind,
        provider_session_id,
        runtime_generation: u64_from_nonnegative_i64(
            "agent_sessions.runtime_generation",
            runtime_generation,
        )?,
        action_epoch: u64_from_nonnegative_i64("tasks.action_epoch", action_epoch)?,
        lifecycle,
        current_turn,
        allow_closing,
    })
}

fn read_task_lifecycle_epoch(
    tx: &Transaction<'_>,
    shadow: bool,
    task_id: TaskId,
) -> Result<(String, i64), StoreError> {
    let table = table_name("tasks", shadow);
    match tx.query_row(
        &format!("SELECT lifecycle, action_epoch FROM {table} WHERE task_id = ?1"),
        [task_id.as_bytes().as_slice()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    ) {
        Ok(v) => Ok(v),
        Err(rusqlite::Error::QueryReturnedNoRows) => Err(StoreError::Projection(
            "missing task for lifecycle transition".into(),
        )),
        Err(err) => Err(err.into()),
    }
}

fn require_close_begun(
    tx: &Transaction<'_>,
    shadow: bool,
    task_id: TaskId,
    action_epoch: u64,
) -> Result<(), StoreError> {
    let (lifecycle, stored_epoch) = read_task_lifecycle_epoch(tx, shadow, task_id)?;
    if !matches!(lifecycle.as_str(), "open" | "settled") {
        return Err(StoreError::Projection(format!(
            "task.close_begun requires open or settled lifecycle, found {lifecycle}"
        )));
    }
    let stored_u64 = u64::try_from(stored_epoch).map_err(|_| StoreError::IntegerOutOfRange {
        field: "tasks.action_epoch",
        value: stored_epoch.unsigned_abs(),
    })?;
    let expected = stored_u64
        .checked_add(1)
        .ok_or(StoreError::IntegerOutOfRange {
            field: "tasks.action_epoch",
            value: u64::MAX,
        })?;
    if action_epoch != expected {
        return Err(StoreError::Projection(format!(
            "task.close_begun action_epoch must be stored+1: stored {stored_u64}, event {action_epoch}"
        )));
    }
    Ok(())
}

fn require_settle(tx: &Transaction<'_>, shadow: bool, task_id: TaskId) -> Result<(), StoreError> {
    let (lifecycle, _) = read_task_lifecycle_epoch(tx, shadow, task_id)?;
    if lifecycle != lifecycle_text(TaskLifecycle::Open) {
        return Err(StoreError::Projection(format!(
            "task.settled requires open lifecycle, found {lifecycle}"
        )));
    }
    Ok(())
}

fn require_reopen(tx: &Transaction<'_>, shadow: bool, task_id: TaskId) -> Result<(), StoreError> {
    let (lifecycle, _) = read_task_lifecycle_epoch(tx, shadow, task_id)?;
    match lifecycle.as_str() {
        "settled" | "closing" | "archived" => Ok(()),
        other => Err(StoreError::Projection(format!(
            "task.reopened requires settled, closing, or archived lifecycle, found {other}"
        ))),
    }
}

fn require_delete(tx: &Transaction<'_>, shadow: bool, task_id: TaskId) -> Result<(), StoreError> {
    let (lifecycle, _) = read_task_lifecycle_epoch(tx, shadow, task_id)?;
    if lifecycle != lifecycle_text(TaskLifecycle::Archived) {
        return Err(StoreError::Projection(format!(
            "task.deleted requires archived lifecycle, found {lifecycle}"
        )));
    }
    Ok(())
}

fn require_archive(tx: &Transaction<'_>, shadow: bool, task_id: TaskId) -> Result<(), StoreError> {
    let (lifecycle, _) = read_task_lifecycle_epoch(tx, shadow, task_id)?;
    if lifecycle != lifecycle_text(TaskLifecycle::Closing) {
        return Err(StoreError::Projection(format!(
            "task.archived requires closing lifecycle, found {lifecycle}"
        )));
    }
    let table = table_name("resources", shadow);
    let mut stmt = tx.prepare(&format!(
        "SELECT owner_kind, lifecycle FROM {table} WHERE task_id = ?1"
    ))?;
    let rows = stmt.query_map([task_id.as_bytes().as_slice()], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (owner_kind, resource_lifecycle) = row?;
        if owner_kind != "task" {
            return Err(StoreError::Projection(format!(
                "task.archived rejects non-task resource owner_kind {owner_kind}"
            )));
        }
        match resource_lifecycle.as_str() {
            "released" => {}
            "active" | "releasing" => {
                return Err(StoreError::Projection(format!(
                    "task.archived rejects live resource lifecycle {resource_lifecycle}"
                )));
            }
            other => {
                return Err(StoreError::Projection(format!(
                    "task.archived rejects malformed resource lifecycle {other}"
                )));
            }
        }
    }
    Ok(())
}

fn load_agent_projection_state(
    tx: &Transaction<'_>,
    shadow: bool,
    agent_session_id: AgentSessionId,
) -> Result<(TaskId, AgentRole, AgentSessionLifecycle, u64), StoreError> {
    let table = table_name("agent_sessions", shadow);
    let (task_bytes, role_blob, lifecycle, runtime_generation): (Vec<u8>, Vec<u8>, String, i64) =
        tx.query_row(
            &format!(
                "SELECT task_id, role, lifecycle, runtime_generation FROM {table}
             WHERE agent_session_id = ?1"
            ),
            [agent_session_id.as_bytes().as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
    let task_bytes: [u8; 16] = task_bytes
        .try_into()
        .map_err(|_| StoreError::Projection("agent task id must be 16 bytes".into()))?;
    let task_id =
        TaskId::from_bytes(task_bytes).map_err(|err| StoreError::Projection(err.to_string()))?;
    let role: AgentRole = rmp_serde::from_slice(&role_blob)
        .map_err(|err| StoreError::Projection(format!("invalid agent role blob: {err}")))?;
    let lifecycle = match lifecycle.as_str() {
        "open" => AgentSessionLifecycle::Open,
        "closing" => AgentSessionLifecycle::Closing,
        "closed" => AgentSessionLifecycle::Closed,
        other => {
            return Err(StoreError::Projection(format!(
                "invalid agent lifecycle '{other}'"
            )))
        }
    };
    let runtime_generation =
        u64_from_nonnegative_i64("agent_sessions.runtime_generation", runtime_generation)?;
    Ok((task_id, role, lifecycle, runtime_generation))
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
    command_id: CommandId,
    operation_id: &[u8; 16],
    action_epoch: Option<u64>,
    resource_id: Option<crate::domain::id::ResourceId>,
    runtime_generation: Option<u64>,
    source: Option<&OutcomeSource>,
    state: &str,
    result: Option<Vec<u8>>,
    outcome_code: Option<&str>,
    outcome_at_ms: i64,
) -> Result<(), StoreError> {
    let table = table_name("operations", shadow);
    let row: Result<
        (
            Vec<u8>,
            Option<Vec<u8>>,
            Option<i64>,
            Option<Vec<u8>>,
            Option<i64>,
            String,
            i64,
            Option<i64>,
        ),
        rusqlite::Error,
    > = tx.query_row(
        &format!(
            "SELECT command_id, task_id, action_epoch, resource_id, runtime_generation, state,
                    accepted_at_ms, outcome_at_ms
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
                row.get(5)?,
                row.get(6)?,
                row.get(7)?,
            ))
        },
    );
    let (
        stored_command,
        stored_task,
        stored_epoch,
        stored_resource,
        stored_generation,
        stored_state,
        accepted_at_ms,
        prior_outcome_at_ms,
    ) = match row {
        Ok(v) => v,
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            return Err(StoreError::Projection("operation not found".into()))
        }
        Err(err) => return Err(err.into()),
    };

    if outcome_at_ms < accepted_at_ms {
        return Err(StoreError::Projection(
            "operation outcome time cannot predate acceptance".into(),
        ));
    }

    match (stored_state.as_str(), source, state) {
        ("accepted", None, "cancelled" | "uncertain") => {
            if prior_outcome_at_ms.is_some() {
                return Err(StoreError::Projection(
                    "accepted operation must not already carry outcome_at_ms".into(),
                ));
            }
        }
        ("accepted", Some(OutcomeSource::Dispatch), "settled" | "failed") => {
            if prior_outcome_at_ms.is_some() {
                return Err(StoreError::Projection(
                    "accepted operation must not already carry outcome_at_ms".into(),
                ));
            }
        }
        ("uncertain", Some(OutcomeSource::VerifiedReconciliation { .. }), "settled" | "failed") => {
            let Some(uncertain_at) = prior_outcome_at_ms else {
                return Err(StoreError::Projection(
                    "uncertain operation requires prior outcome_at_ms".into(),
                ));
            };
            if outcome_at_ms < uncertain_at {
                return Err(StoreError::Projection(
                    "reconciliation outcome cannot predate uncertainty observation".into(),
                ));
            }
        }
        ("accepted", Some(OutcomeSource::VerifiedReconciliation { .. }), "settled" | "failed") => {
            return Err(StoreError::Projection(
                "accepted operations accept only Dispatch terminal facts".into(),
            ))
        }
        ("uncertain", Some(OutcomeSource::Dispatch), "settled" | "failed") => {
            return Err(StoreError::Projection(
                "uncertain operations accept only VerifiedReconciliation Settled/Failed facts"
                    .into(),
            ))
        }
        (other, _, _) => {
            return Err(StoreError::Projection(format!(
                "operation state {other} cannot transition to {state} with the provided source"
            )))
        }
    }

    if let Some(OutcomeSource::VerifiedReconciliation { effect_index, .. }) = source {
        if matches!(state, "settled" | "failed") {
            validate_verified_reconciliation_outbox(
                tx,
                operation_id,
                *effect_index,
                state,
                shadow,
            )?;
        }
    }

    if stored_command.as_slice() != command_id.as_bytes().as_slice() {
        return Err(StoreError::Projection(
            "operation command_id fence mismatch".into(),
        ));
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

fn enforce_envelope_task_revision_rule(event: &DomainEvent) -> Result<(), StoreError> {
    let is_mutation = event.payload.is_task_mutation();
    match (is_mutation, event.task_revision.is_some()) {
        (true, false) => Err(StoreError::Projection(format!(
            "event {} requires task_revision",
            event.payload.event_type()
        ))),
        (false, true) => Err(StoreError::Projection(format!(
            "event {} requires NULL task_revision",
            event.payload.event_type()
        ))),
        _ => Ok(()),
    }
}

fn validate_verified_reconciliation_outbox(
    tx: &Transaction<'_>,
    operation_id: &[u8; 16],
    effect_index: u32,
    target_state: &str,
    shadow: bool,
) -> Result<(), StoreError> {
    if effect_index != 0 {
        return Err(StoreError::Projection(
            "verified reconciliation requires effect_index 0".into(),
        ));
    }
    let mut stmt = tx.prepare(
        "SELECT effect_index, state, leased_until_ms, last_error_class
         FROM outbox
         WHERE operation_id = ?1
         ORDER BY effect_index ASC",
    )?;
    let rows = stmt.query_map([operation_id.as_slice()], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<i64>>(2)?,
            row.get::<_, Option<String>>(3)?,
        ))
    })?;
    let mut collected = Vec::new();
    for row in rows {
        collected.push(row?);
    }
    if collected.len() != 1 {
        return Err(StoreError::Projection(
            "verified reconciliation requires exactly one durable outbox effect row".into(),
        ));
    }
    let (row_index, row_state, leased_until_ms, last_error_class) = &collected[0];
    if *row_index != 0 {
        return Err(StoreError::Projection(
            "verified reconciliation requires durable outbox effect_index 0".into(),
        ));
    }
    if *row_index != i64::from(effect_index) {
        return Err(StoreError::Projection(format!(
            "verified reconciliation effect_index {effect_index} does not match durable outbox {row_index}"
        )));
    }
    if shadow {
        if row_state != target_state {
            return Err(StoreError::Projection(format!(
                "shadow verified reconciliation requires retained outbox state {target_state}, found {row_state}"
            )));
        }
        if leased_until_ms.is_some() {
            return Err(StoreError::Projection(
                "shadow verified reconciliation requires cleared outbox lease".into(),
            ));
        }
        let expected_error = match target_state {
            "settled" => None,
            "failed" => Some("side_effect_failed"),
            other => {
                return Err(StoreError::Projection(format!(
                    "unsupported verified reconciliation target state {other}"
                )))
            }
        };
        if last_error_class.as_deref() != expected_error {
            return Err(StoreError::Projection(
                "shadow verified reconciliation outbox last_error_class mismatch".into(),
            ));
        }
    } else if row_state != "uncertain" {
        return Err(StoreError::Projection(format!(
            "live verified reconciliation requires uncertain outbox, found {row_state}"
        )));
    }
    Ok(())
}

fn validate_prompt_library_settled_against_history(
    tx: &Transaction<'_>,
    shadow: bool,
    event: &DomainEvent,
    fact: &crate::domain::event::OperationSettledFact,
) -> Result<(), StoreError> {
    let table = table_name("operations", shadow);
    let accepted_at_ms: i64 = tx.query_row(
        &format!("SELECT accepted_at_ms FROM {table} WHERE operation_id = ?1"),
        [fact.operation_id.as_bytes().as_slice()],
        |row| row.get(0),
    )?;
    let prior = load_prior_event_row(tx, event.sequence)?;
    let accepted_arg = prior
        .as_ref()
        .map(|(id, payload, rev, at, task)| (*id, payload, *rev, *at, *task));
    validate_prompt_library_settled_lineage(
        fact,
        event.occurred_at_ms,
        event.task_id,
        event.task_revision,
        accepted_at_ms,
        accepted_arg,
        true,
    )
}

fn validate_pure_settled_against_history(
    tx: &Transaction<'_>,
    shadow: bool,
    event: &DomainEvent,
    fact: &crate::domain::event::OperationSettledFact,
) -> Result<(), StoreError> {
    let table = table_name("operations", shadow);
    let accepted_at_ms: i64 = tx.query_row(
        &format!("SELECT accepted_at_ms FROM {table} WHERE operation_id = ?1"),
        [fact.operation_id.as_bytes().as_slice()],
        |row| row.get(0),
    )?;
    let prior = load_prior_event_row(tx, event.sequence)?;
    let accepted_arg = prior
        .as_ref()
        .map(|(id, payload, rev, at, task)| (*id, payload, *rev, *at, *task));
    let decision_count = fact.result_event_ids.len();
    let accepted_sequence = event
        .sequence
        .checked_sub(1)
        .ok_or_else(|| StoreError::Projection("pure settle missing accepted sequence".into()))?;
    let decision_count_u64 = u64::try_from(decision_count)
        .map_err(|_| StoreError::Projection("pure settle decision count overflow".into()))?;
    let first_decision = accepted_sequence
        .checked_sub(decision_count_u64)
        .ok_or_else(|| StoreError::Projection("pure settle decision window underflows".into()))?;
    let mut owned_decisions = Vec::with_capacity(decision_count);
    for offset in 0..decision_count {
        let offset_u64 = u64::try_from(offset)
            .map_err(|_| StoreError::Projection("pure settle decision offset overflow".into()))?;
        let sequence = first_decision.checked_add(offset_u64).ok_or_else(|| {
            StoreError::Projection("pure settle decision sequence overflow".into())
        })?;
        let row = load_event_at_sequence(tx, sequence)?.ok_or_else(|| {
            StoreError::Projection("pure settle missing contiguous decision fact".into())
        })?;
        owned_decisions.push(row);
    }
    let decision_refs: Vec<(EventId, &Event, Option<u64>, i64, Option<TaskId>)> = owned_decisions
        .iter()
        .map(|(id, ev, rev, at, task)| (*id, ev, *rev, *at, *task))
        .collect();
    // validate_pure_settled_lineage wants owned Event in the slice — pass clones via owned vec
    let decision_owned: Vec<(EventId, Event, Option<u64>, i64, Option<TaskId>)> =
        owned_decisions.clone();
    let _ = decision_refs;
    validate_pure_settled_lineage(
        fact,
        event.occurred_at_ms,
        event.task_id,
        accepted_at_ms,
        accepted_arg,
        &decision_owned,
        true,
    )?;
    Ok(())
}

fn load_event_at_sequence(
    tx: &Transaction<'_>,
    sequence: u64,
) -> Result<Option<(EventId, Event, Option<u64>, i64, Option<TaskId>)>, StoreError> {
    let row: Option<(Vec<u8>, Option<Vec<u8>>, Option<i64>, String, i64, i64, i64)> = tx
        .query_row(
            "SELECT event_id, task_id, task_revision, event_type, schema_version, length(payload),
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
        payload_len,
        occurred_at_ms,
    )) = row
    else {
        return Ok(None);
    };
    let payload_len = usize::try_from(payload_len).map_err(|_| StoreError::CodecMismatch {
        detail: "event payload length is invalid".into(),
    })?;
    if event_type.starts_with("provider_input.")
        && payload_len > crate::kernel::store::MAX_PROVIDER_EVENT_PAYLOAD_BYTES
    {
        return Err(StoreError::CodecMismatch {
            detail: "provider event payload exceeds its byte bound".into(),
        });
    }
    let payload: Vec<u8> = tx.query_row(
        "SELECT payload FROM events WHERE sequence = ?1",
        [u64_to_sqlite_i64("events.sequence", sequence)?],
        |row| row.get(0),
    )?;
    let event_id = event_id_from_bytes_local(&event_id_bytes)?;
    let task_id = match task_bytes {
        Some(bytes) => Some(task_id_from_bytes_local(&bytes)?),
        None => None,
    };
    let task_revision = match task_revision {
        Some(v) => Some(u64_from_nonnegative_i64("events.task_revision", v)?),
        None => None,
    };
    let decoded = decode_stored_event(&event_type, schema_version, &payload)?;
    Ok(Some((
        event_id,
        decoded,
        task_revision,
        occurred_at_ms,
        task_id,
    )))
}

fn enforce_derived_result_lineage(
    tx: &Transaction<'_>,
    event: &DomainEvent,
) -> Result<(), StoreError> {
    let prior = load_prior_event_row(tx, event.sequence)?;
    match &event.payload {
        Event::OperationSettled(fact) => {
            let kind = classify_operation_settled_fact(fact, event.task_id, true)?;
            let prior_arg = prior
                .as_ref()
                .map(|(id, payload, rev, at, task)| (*id, payload, *rev, *at, *task));
            validate_side_effect_settled_has_prior_derived(
                fact,
                event.occurred_at_ms,
                event.task_id,
                prior_arg,
                true,
            )?;
            if matches!(
                kind,
                SettledLineageKind::Pure | SettledLineageKind::PromptLibrary
            ) {
                if let Some((_, payload, _, _, _)) = &prior {
                    if is_derived_lifecycle_result(payload) {
                        return Err(StoreError::Projection(
                            "derived lifecycle result is missing immediately following side-effect operation.settled"
                                .into(),
                        ));
                    }
                }
            }
            Ok(())
        }
        _ => {
            if let Some((_, payload, _, _, _)) = &prior {
                if is_derived_lifecycle_result(payload) {
                    return Err(StoreError::Projection(
                        "derived lifecycle result is missing immediately following operation.settled"
                            .into(),
                    ));
                }
            }
            Ok(())
        }
    }
}

fn load_prior_event_row(
    tx: &Transaction<'_>,
    sequence: u64,
) -> Result<Option<(EventId, Event, Option<u64>, i64, Option<TaskId>)>, StoreError> {
    if sequence == 0 {
        return Ok(None);
    }
    // Adjacency is the immediately previous *existing* durable row, not sequence-1
    // (AUTOINCREMENT gaps must not hide orphan derived results).
    let row: Option<(Vec<u8>, Option<Vec<u8>>, Option<i64>, String, i64, i64, i64)> = tx
        .query_row(
            "SELECT event_id, task_id, task_revision, event_type, schema_version, length(payload),
                    occurred_at_ms
             FROM events
             WHERE sequence < ?1
             ORDER BY sequence DESC
             LIMIT 1",
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
        payload_len,
        occurred_at_ms,
    )) = row
    else {
        return Ok(None);
    };
    let payload_len = usize::try_from(payload_len).map_err(|_| StoreError::CodecMismatch {
        detail: "event payload length is invalid".into(),
    })?;
    if event_type.starts_with("provider_input.")
        && payload_len > crate::kernel::store::MAX_PROVIDER_EVENT_PAYLOAD_BYTES
    {
        return Err(StoreError::CodecMismatch {
            detail: "provider event payload exceeds its byte bound".into(),
        });
    }
    let payload: Vec<u8> = tx.query_row(
        "SELECT payload FROM events
         WHERE sequence < ?1
         ORDER BY sequence DESC
         LIMIT 1",
        [u64_to_sqlite_i64("events.sequence", sequence)?],
        |row| row.get(0),
    )?;
    let event_id = event_id_from_bytes_local(&event_id_bytes)?;
    let task_id = match task_bytes {
        Some(bytes) => Some(task_id_from_bytes_local(&bytes)?),
        None => None,
    };
    let task_revision = match task_revision {
        Some(v) => Some(u64_from_nonnegative_i64("events.task_revision", v)?),
        None => None,
    };
    let decoded = decode_stored_event(&event_type, schema_version, &payload)?;
    Ok(Some((
        event_id,
        decoded,
        task_revision,
        occurred_at_ms,
        task_id,
    )))
}

/// Rebuild flush: last durable event must not be an orphan derived result.
pub(crate) fn ensure_no_trailing_orphan_derived(tx: &Transaction<'_>) -> Result<(), StoreError> {
    let mut statement = tx.prepare(
        "SELECT event_type, schema_version, payload FROM events
         ORDER BY sequence DESC LIMIT 1",
    )?;
    let mut rows = statement.query([])?;
    let Some(row) = rows.next()? else {
        return Ok(());
    };
    let event_type: String = row.get(0)?;
    let schema_version: i64 = row.get(1)?;
    let payload_ref = row.get_ref(2)?;
    let payload_bytes = payload_ref
        .as_blob()
        .map_err(|error| StoreError::CodecMismatch {
            detail: format!("provider event payload is not a BLOB: {error}"),
        })?;
    if event_type.starts_with("provider_input.")
        && payload_bytes.len() > crate::kernel::store::MAX_PROVIDER_EVENT_PAYLOAD_BYTES
    {
        return Err(StoreError::CodecMismatch {
            detail: format!(
                "provider event payload exceeds {} bytes",
                crate::kernel::store::MAX_PROVIDER_EVENT_PAYLOAD_BYTES
            ),
        });
    }
    let payload = payload_bytes.to_vec();
    let decoded = decode_stored_event(&event_type, schema_version, &payload)?;
    if is_derived_lifecycle_result(&decoded) {
        return Err(StoreError::Projection(
            "derived lifecycle result is missing immediately following operation.settled".into(),
        ));
    }
    Ok(())
}

fn event_id_from_bytes_local(bytes: &[u8]) -> Result<EventId, StoreError> {
    let arr: [u8; 16] = bytes.try_into().map_err(|_| {
        StoreError::Projection(format!(
            "events.event_id must be 16 bytes, got {}",
            bytes.len()
        ))
    })?;
    EventId::from_bytes(arr).map_err(|e| StoreError::Projection(e.to_string()))
}

fn task_id_from_bytes_local(bytes: &[u8]) -> Result<TaskId, StoreError> {
    let arr: [u8; 16] = bytes.try_into().map_err(|_| {
        StoreError::Projection(format!(
            "events.task_id must be 16 bytes, got {}",
            bytes.len()
        ))
    })?;
    TaskId::from_bytes(arr).map_err(|e| StoreError::Projection(e.to_string()))
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

fn upsert_provider_session_state(
    tx: &Transaction<'_>,
    shadow: bool,
    task_id: TaskId,
    agent_session_id: AgentSessionId,
    mutate: impl FnOnce(
        &mut crate::domain::provider_input::ProviderSessionProjection,
    ) -> Result<(), StoreError>,
) -> Result<(), StoreError> {
    let table = table_name("provider_input_state", shadow);
    // Probe lengths before asking rusqlite to materialize either BLOB. This is
    // the SQL-side half of the projection bound: malformed persisted state must
    // fail closed without allocating an attacker-sized MessagePack buffer.
    let lengths: Option<(i64, i64)> = tx
        .query_row(
            &format!(
                "SELECT length(task_id), length(state) FROM {table}
                 WHERE agent_session_id = ?1"
            ),
            [agent_session_id.as_bytes().as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let had_row = lengths.is_some();
    let mut session = match lengths {
        Some((task_id_len, state_len)) => {
            if task_id_len != 16 {
                return Err(StoreError::Projection(
                    "provider_input_state task id must be 16 bytes".into(),
                ));
            }
            let max_state_len = i64::try_from(crate::domain::MAX_PROVIDER_SESSION_STATE_BYTES)
                .map_err(|_| {
                    StoreError::Projection("provider state bound is out of range".into())
                })?;
            if state_len < 0 || state_len > max_state_len {
                return Err(StoreError::Projection(
                    "provider_input_state.state exceeds its byte bound".into(),
                ));
            }
            let (stored_task_bytes, bytes): (Vec<u8>, Vec<u8>) = tx.query_row(
                &format!("SELECT task_id, state FROM {table} WHERE agent_session_id = ?1"),
                [agent_session_id.as_bytes().as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            if task_id_from_bytes_local(&stored_task_bytes)? != task_id {
                return Err(StoreError::Projection(
                    "provider_input_state task scope mismatch".into(),
                ));
            }
            if i64::try_from(bytes.len()).ok() != Some(state_len) {
                return Err(StoreError::Projection(
                    "provider_input_state.state length changed during read".into(),
                ));
            }
            rmp_serde::from_slice(&bytes).map_err(|err| {
                StoreError::Projection(format!("provider_input_state.state decode: {err}"))
            })?
        }
        None => crate::domain::provider_input::ProviderSessionProjection::default(),
    };
    session
        .validate_bounds()
        .map_err(|err| StoreError::Projection(err.to_string()))?;
    mutate(&mut session)?;
    session
        .validate_bounds()
        .map_err(|err| StoreError::Projection(err.to_string()))?;
    let packed = pack(&session)?;
    if packed.len() > crate::domain::MAX_PROVIDER_SESSION_STATE_BYTES {
        return Err(StoreError::Projection(
            "provider_input_state exceeded its byte bound".into(),
        ));
    }
    if had_row {
        tx.execute(
            &format!("UPDATE {table} SET task_id = ?1, state = ?2 WHERE agent_session_id = ?3"),
            rusqlite::params![
                task_id.as_bytes().as_slice(),
                packed,
                agent_session_id.as_bytes().as_slice(),
            ],
        )?;
    } else {
        tx.execute(
            &format!("INSERT INTO {table}(agent_session_id, task_id, state) VALUES (?1, ?2, ?3)"),
            rusqlite::params![
                agent_session_id.as_bytes().as_slice(),
                task_id.as_bytes().as_slice(),
                packed,
            ],
        )?;
    }
    Ok(())
}

/// Pack a projection MessagePack blob. Same encoding the store uses when writing.
pub(crate) fn pack<T: serde::Serialize>(value: &T) -> Result<Vec<u8>, StoreError> {
    rmp_serde::to_vec(value).map_err(|e| StoreError::Projection(e.to_string()))
}

fn lifecycle_text(value: TaskLifecycle) -> &'static str {
    match value {
        TaskLifecycle::Open => "open",
        TaskLifecycle::Settled => "settled",
        TaskLifecycle::Closing => "closing",
        TaskLifecycle::Archived => "archived",
        TaskLifecycle::Deleted => "deleted",
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
        OperationErrorCode::CleanupFailed => "cleanup_failed",
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

fn load_host_cleanup_journal_outcomes(
    tx: &Transaction<'_>,
    shadow: bool,
    operation_id: crate::domain::id::OperationId,
) -> Result<(Vec<(HostCleanupBranch, HostCleanupBranchOutcome)>, i64), StoreError> {
    let table = table_name("host_cleanup_branches", shadow);
    let mut journal = Vec::with_capacity(HostCleanupBranch::ORDER.len());
    let mut max_completed_at_ms = i64::MIN;
    for branch in HostCleanupBranch::ORDER {
        let row: Option<(String, i64, i64)> = tx
            .query_row(
                &format!(
                    "SELECT result, remaining_count, completed_at_ms FROM {table}
                     WHERE operation_id = ?1 AND branch = ?2"
                ),
                rusqlite::params![operation_id.as_bytes().as_slice(), branch.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let Some((result, remaining_count, completed_at_ms)) = row else {
            break;
        };
        let remaining =
            u64_from_nonnegative_i64("host_cleanup_branches.remaining_count", remaining_count)?;
        let outcome = match result.as_str() {
            "succeeded" if remaining == 0 => HostCleanupBranchOutcome::succeeded(),
            "failed" => HostCleanupBranchOutcome::failed(remaining).ok_or_else(|| {
                StoreError::Projection(
                    "host_cleanup_branches Failed requires remaining_count > 0".into(),
                )
            })?,
            _ => {
                return Err(StoreError::Projection(
                    "host_cleanup_branches result must be succeeded or failed".into(),
                ))
            }
        };
        max_completed_at_ms = max_completed_at_ms.max(completed_at_ms);
        journal.push((branch, outcome));
    }
    if journal.is_empty() {
        return Ok((journal, 0));
    }
    Ok((journal, max_completed_at_ms))
}

fn load_ordered_host_cleanup_branch_event_ids(
    tx: &Transaction<'_>,
    _shadow: bool,
    operation_id: crate::domain::id::OperationId,
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
        let (event_id_bytes, task_id, task_revision, schema_version, payload) = row?;
        if task_id.is_some() || task_revision.is_some() {
            return Err(StoreError::Projection(
                "host.cleanup_branch_completed requires NULL task scope".into(),
            ));
        }
        let event_id = EventId::from_bytes({
            let mut bytes = [0u8; 16];
            bytes.copy_from_slice(&event_id_bytes);
            bytes
        })
        .map_err(|err| StoreError::Projection(err.to_string()))?;
        let decoded =
            decode_stored_event("host.cleanup_branch_completed", schema_version, &payload)?;
        let Event::HostCleanupBranchCompleted {
            operation_id: fact_operation_id,
            branch,
            ..
        } = decoded
        else {
            return Err(StoreError::Projection(
                "host.cleanup_branch_completed decode mismatch".into(),
            ));
        };
        if fact_operation_id != operation_id {
            continue;
        }
        observed.push((branch, event_id));
    }
    if observed.len() != HostCleanupBranch::ORDER.len() {
        return Err(StoreError::Projection(
            "host-admission settlement requires all ORDER branch event ids".into(),
        ));
    }
    let mut ordered = Vec::with_capacity(HostCleanupBranch::ORDER.len());
    for (idx, (branch, event_id)) in observed.into_iter().enumerate() {
        if branch != HostCleanupBranch::ORDER[idx] {
            return Err(StoreError::Projection(
                "host.cleanup_branch_completed durable sequence must match HostCleanupBranch::ORDER"
                    .into(),
            ));
        }
        ordered.push(event_id);
    }
    Ok(ordered)
}

fn load_operation_accepted_at_ms(
    tx: &Transaction<'_>,
    shadow: bool,
    operation_id: crate::domain::id::OperationId,
) -> Result<i64, StoreError> {
    let table = table_name("operations", shadow);
    let accepted_at_ms: i64 = tx.query_row(
        &format!("SELECT accepted_at_ms FROM {table} WHERE operation_id = ?1"),
        [operation_id.as_bytes().as_slice()],
        |row| row.get(0),
    )?;
    Ok(accepted_at_ms)
}

fn require_valid_operation_fact(result: Result<(), OutcomeFenceError>) -> Result<(), StoreError> {
    result.map_err(|err| StoreError::Projection(err.to_string()))
}

/// Rewrite the `title` inside a stored terminal recipe, exactly as the
/// `TerminalRenamed` arm of `apply_into` rewrites it on the in-memory resource.
/// A non-terminal recipe is left untouched, matching that arm's `if let`.
fn rewrite_terminal_recipe_title(
    tx: &Transaction<'_>,
    shadow: bool,
    resource_id: ResourceId,
    title: Option<String>,
) -> Result<(), StoreError> {
    let table = table_name("resources", shadow);
    let recipe_bytes: Option<Vec<u8>> = tx
        .query_row(
            &format!("SELECT recipe FROM {table} WHERE resource_id = ?1"),
            [resource_id.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .optional()?;
    let Some(recipe_bytes) = recipe_bytes else {
        return Err(StoreError::Projection(
            "terminal rename: no resources row for this terminal".into(),
        ));
    };
    let mut recipe: crate::domain::resource::ResourceRecipe = rmp_serde::from_slice(&recipe_bytes)
        .map_err(|err| StoreError::Projection(format!("resources.recipe decode: {err}")))?;
    if let crate::domain::resource::ResourceRecipe::Terminal {
        title: recipe_title,
        ..
    } = &mut recipe
    {
        *recipe_title = title;
    } else {
        return Ok(());
    }
    tx.execute(
        &format!("UPDATE {table} SET recipe = ?1 WHERE resource_id = ?2"),
        rusqlite::params![pack(&recipe)?, resource_id.as_bytes().as_slice()],
    )?;
    require_one_change(tx, "terminal recipe title rewrite")?;
    Ok(())
}

/// The stored strip for a task, or `None` when the task has never had one.
///
/// The caller distinguishes the two: appending seeds a fresh strip, removing
/// from an absent one must not mint an empty row.
fn read_terminal_strip_row(
    tx: &Transaction<'_>,
    shadow: bool,
    task_id: TaskId,
) -> Result<Option<crate::domain::terminal_facts::TaskTerminalStrip>, StoreError> {
    let row: Option<(Vec<u8>, Option<Vec<u8>>)> = tx
        .query_row(
            &format!(
                "SELECT order_msgpack, focused_resource_id FROM {} WHERE task_id = ?1",
                table_name("task_terminal_strip", shadow)
            ),
            rusqlite::params![task_id.as_bytes().as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((order_bytes, focused)) = row else {
        return Ok(None);
    };
    let order: Vec<ResourceId> = rmp_serde::from_slice(&order_bytes).map_err(|err| {
        StoreError::Projection(format!("task_terminal_strip.order_msgpack: {err}"))
    })?;
    let focused = match focused {
        Some(bytes) => {
            let array: [u8; 16] = bytes.as_slice().try_into().map_err(|_| {
                StoreError::Projection(
                    "task_terminal_strip.focused_resource_id must be 16 bytes".into(),
                )
            })?;
            Some(ResourceId::from_bytes(array).map_err(|err| {
                StoreError::Projection(format!("task_terminal_strip.focused_resource_id: {err}"))
            })?)
        }
        None => None,
    };
    Ok(Some(crate::domain::terminal_facts::TaskTerminalStrip {
        order,
        focused,
    }))
}

/// Write the strip. `order_msgpack` uses [`pack`] because the snapshot loader
/// re-encodes it byte-for-byte to prove the blob is lossless.
///
/// UPDATE-then-INSERT rather than `ON CONFLICT`, following
/// [`upsert_provider_session_state`]: a shadow twin is built with
/// `CREATE TEMP TABLE ... AS SELECT`, which copies no PRIMARY KEY, so an upsert
/// clause parses on the stable table and fails on every rebuild.
fn write_terminal_strip(
    tx: &Transaction<'_>,
    shadow: bool,
    task_id: TaskId,
    strip: &crate::domain::terminal_facts::TaskTerminalStrip,
) -> Result<(), StoreError> {
    let table = table_name("task_terminal_strip", shadow);
    let order = pack(&strip.order)?;
    let focused = strip.focused.map(|id| id.as_bytes().to_vec());
    let changed = tx.execute(
        &format!(
            "UPDATE {table} SET order_msgpack = ?1, focused_resource_id = ?2 WHERE task_id = ?3"
        ),
        rusqlite::params![order, focused, task_id.as_bytes().as_slice()],
    )?;
    if changed == 0 {
        tx.execute(
            &format!(
                "INSERT INTO {table} (task_id, order_msgpack, focused_resource_id) \
                 VALUES (?1, ?2, ?3)"
            ),
            rusqlite::params![task_id.as_bytes().as_slice(), order, focused],
        )?;
    }
    Ok(())
}

fn append_to_terminal_strip(
    tx: &Transaction<'_>,
    shadow: bool,
    task_id: TaskId,
    resource_id: ResourceId,
) -> Result<(), StoreError> {
    let mut strip = read_terminal_strip_row(tx, shadow, task_id)?.unwrap_or_default();
    if !strip.order.contains(&resource_id) {
        strip.order.push(resource_id);
    }
    if strip.focused.is_none() {
        strip.focused = Some(resource_id);
    }
    write_terminal_strip(tx, shadow, task_id, &strip)
}

/// Remove a released terminal from the strip, clearing focus when it pointed at
/// it. Delegates to [`crate::domain::terminal_facts::TaskTerminalStrip::remove`]
/// so the projection and `apply_into` cannot drift on the focus rule.
fn remove_from_terminal_strip(
    tx: &Transaction<'_>,
    shadow: bool,
    task_id: TaskId,
    resource_id: ResourceId,
) -> Result<(), StoreError> {
    let Some(mut strip) = read_terminal_strip_row(tx, shadow, task_id)? else {
        return Ok(());
    };
    strip.remove(resource_id);
    write_terminal_strip(tx, shadow, task_id, &strip)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::event::{
        OperationAcceptedFact, OperationCancelledFact, OperationUncertainFact,
    };
    use crate::domain::id::{CommandId, EventId, OperationId, ResourceId, TaskId};
    use crate::domain::operation::CancellationReason;
    use crate::kernel::store::KernelStore;
    use tempfile::TempDir;

    fn fixed_uuid_v7(tail: u8) -> [u8; 16] {
        [
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, tail,
        ]
    }

    fn domain(payload: Event) -> DomainEvent {
        DomainEvent {
            id: EventId::from_bytes(fixed_uuid_v7(0x90)).unwrap(),
            task_id: Some(TaskId::from_bytes(fixed_uuid_v7(0x40)).unwrap()),
            sequence: 1,
            task_revision: None,
            occurred_at_ms: 1,
            payload,
        }
    }

    #[test]
    fn command_contract_projector_rejects_forged_partial_fence_facts() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("kernel.sqlite3");
        let mut store = KernelStore::open(&path).expect("open");

        let cmd = CommandId::from_bytes(fixed_uuid_v7(0x41)).unwrap();
        let op = OperationId::from_bytes(fixed_uuid_v7(0x42)).unwrap();
        let resource = ResourceId::from_bytes(fixed_uuid_v7(0x43)).unwrap();

        let forged_accepted = OperationAcceptedFact {
            command_id: cmd,
            operation_id: op,
            accepted_at_ms: 1,
            action_epoch: None,
            resource_id: Some(resource),
            runtime_generation: None,
        };
        let err = store
            .with_transaction(|tx| {
                apply_event(
                    tx,
                    &domain(Event::OperationAccepted(forged_accepted)),
                    false,
                )
            })
            .expect_err("accepted partial fence");
        assert!(
            matches!(err, StoreError::Projection(_)),
            "expected projection error, got {err:?}"
        );

        let forged_cancelled = OperationCancelledFact {
            command_id: cmd,
            operation_id: op,
            settled_at_ms: 1,
            reason: CancellationReason::Superseded,
            action_epoch: None,
            resource_id: None,
            runtime_generation: Some(2),
        };
        let err = store
            .with_transaction(|tx| {
                apply_event(
                    tx,
                    &domain(Event::OperationCancelled(forged_cancelled)),
                    false,
                )
            })
            .expect_err("cancelled partial fence");
        assert!(matches!(err, StoreError::Projection(_)), "got {err:?}");

        let forged_uncertain = OperationUncertainFact {
            command_id: cmd,
            operation_id: op,
            observed_at_ms: 1,
            code: OperationUncertaintyCode::AmbiguousDispatch,
            action_epoch: Some(1),
            resource_id: Some(resource),
            runtime_generation: None,
        };
        let err = store
            .with_transaction(|tx| {
                apply_event(
                    tx,
                    &domain(Event::OperationUncertain(forged_uncertain)),
                    false,
                )
            })
            .expect_err("uncertain partial fence");
        assert!(matches!(err, StoreError::Projection(_)), "got {err:?}");
    }

    #[test]
    fn browser_fact_rejects_embedded_task_identity_mismatch() {
        use crate::domain::browser::BrowserDurableFact;
        use crate::domain::id::BrowserContextId;

        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("kernel.sqlite3");
        let mut store = KernelStore::open(&path).expect("open");

        let envelope_task = TaskId::from_bytes(fixed_uuid_v7(0x40)).unwrap();
        let embedded_task = TaskId::from_bytes(fixed_uuid_v7(0x41)).unwrap();
        let mut event = domain(Event::Browser(BrowserDurableFact::ContextClosed {
            context_id: BrowserContextId::from_bytes(fixed_uuid_v7(0x42)).unwrap(),
            task_id: embedded_task,
            generation: 1,
        }));
        event.task_id = Some(envelope_task);
        event.task_revision = Some(1);

        let err = store
            .with_transaction(|tx| apply_event(tx, &event, false))
            .expect_err("mismatched browser task identity");
        assert!(
            matches!(err, StoreError::Projection(ref detail) if detail.contains("embedded task_id")),
            "got {err:?}"
        );
    }
}
