//! Presentation-independent client model assembled from one pinned snapshot
//! and advanced by ordered durable events.

use std::collections::{BTreeMap, HashSet};

use crate::domain::agent::AgentSessionFacts;
use crate::domain::artifact::ArtifactSummary;
use crate::domain::event::{apply, DomainEvent, Event};
use crate::domain::id::{AgentSessionId, ArtifactId, OperationId, ResourceId, SnapshotId, TaskId};
use crate::domain::operation::{OperationFacts, OperationState};
use crate::domain::resource::{OwnerKind, ResourceFacts};
use crate::domain::snapshot::{
    EventPage, SnapshotItem, SnapshotPage, SnapshotSection, TaskSnapshot, TaskSnapshotItem,
};

/// Finite bound on snapshot pages admitted while assembling one model.
pub const MAX_CLIENT_MODEL_PAGES: usize = 1_024;
/// Finite bound on snapshot items admitted while assembling one model.
pub const MAX_CLIENT_MODEL_ITEMS: usize = 100_000;
/// Finite bound on distinct resume cursors retained per section.
pub const MAX_CLIENT_MODEL_CURSORS_PER_SECTION: usize = 1_024;
/// Finite bound on frozen replay pages applied to one model.
pub const MAX_CLIENT_REPLAY_PAGES: usize = 1_024;
/// Finite bound on distinct frozen replay continuation cursors.
pub const MAX_CLIENT_REPLAY_CURSORS: usize = 1_024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientModelError {
    MissingSections,
    DuplicateSection,
    SectionItemMismatch,
    DuplicateItem,
    SnapshotDrift,
    SequenceDrift,
    NonProgressingPage,
    PageBoundExceeded,
    ItemBoundExceeded,
    CursorBoundExceeded,
    RepeatedCursor,
    MissingParentTask,
    InvalidOwnership,
    InvalidPrimaryAgent,
    DuplicateOrRegression,
    ApplyFailed,
    OperationIdentityMismatch,
    OperationStateRegression,
    MissingOperation,
    ReplayRangeInvalid,
    ReplayAfterMismatch,
    ReplayThroughDrift,
    ReplayRepeatedCursor,
    ReplayPageBoundExceeded,
    ReplayNonProgressing,
    SnapshotBoundaryMismatch,
    OperationEnvelopeTimestampMismatch,
}

impl std::fmt::Display for ClientModelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingSections => {
                write!(f, "client model is missing required snapshot sections")
            }
            Self::DuplicateSection => write!(f, "snapshot section was repeated"),
            Self::SectionItemMismatch => write!(f, "snapshot page contained a mismatched item"),
            Self::DuplicateItem => write!(f, "snapshot contained a duplicate item id"),
            Self::SnapshotDrift => write!(f, "snapshot identity drifted across pages"),
            Self::SequenceDrift => write!(f, "snapshot through_sequence drifted across pages"),
            Self::NonProgressingPage => write!(f, "snapshot page did not progress"),
            Self::PageBoundExceeded => write!(f, "snapshot page bound exceeded"),
            Self::ItemBoundExceeded => write!(f, "snapshot item bound exceeded"),
            Self::CursorBoundExceeded => write!(f, "snapshot cursor bound exceeded"),
            Self::RepeatedCursor => write!(f, "snapshot resume cursor repeated"),
            Self::MissingParentTask => write!(f, "child snapshot item references a missing task"),
            Self::InvalidOwnership => write!(f, "snapshot item ownership is invalid"),
            Self::InvalidPrimaryAgent => write!(f, "primary agent reference is invalid"),
            Self::DuplicateOrRegression => {
                write!(f, "durable event sequence duplicated or regressed")
            }
            Self::ApplyFailed => write!(f, "durable event could not be applied to the model"),
            Self::OperationIdentityMismatch => {
                write!(f, "operation event identity did not match the projection")
            }
            Self::OperationStateRegression => {
                write!(f, "operation state transition is not monotonic")
            }
            Self::MissingOperation => {
                write!(f, "operation outcome referenced an unknown operation")
            }
            Self::ReplayRangeInvalid => write!(f, "replay page range is inconsistent"),
            Self::ReplayAfterMismatch => {
                write!(f, "replay page after_sequence skipped the applied boundary")
            }
            Self::ReplayThroughDrift => {
                write!(
                    f,
                    "replay page through_sequence drifted from the pinned high-water"
                )
            }
            Self::ReplayRepeatedCursor => write!(f, "replay resume cursor repeated"),
            Self::ReplayPageBoundExceeded => write!(f, "replay page bound exceeded"),
            Self::ReplayNonProgressing => write!(f, "replay page did not progress"),
            Self::SnapshotBoundaryMismatch => {
                write!(
                    f,
                    "snapshot continuation after_item did not match the expected boundary"
                )
            }
            Self::OperationEnvelopeTimestampMismatch => write!(
                f,
                "operation event envelope occurred_at_ms must equal the fact timestamp"
            ),
        }
    }
}

impl std::error::Error for ClientModelError {}

/// Validated presentation-independent client projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientModel {
    tasks: BTreeMap<TaskId, TaskSnapshot>,
    host_resources: BTreeMap<ResourceId, ResourceFacts>,
    operations: BTreeMap<OperationId, OperationFacts>,
    /// Metadata-only artifact index from snapshot pages / durable events.
    /// TaskSnapshot.artifacts is cleared after ArtifactRegistered staging so the
    /// public client model never retains inline bodies or content refs.
    artifact_summaries: BTreeMap<ArtifactId, ArtifactSummary>,
    last_applied_sequence: u64,
    replay_through: Option<u64>,
    replay_page_count: usize,
    replay_cursors: HashSet<Vec<u8>>,
}

impl ClientModel {
    pub fn tasks(&self) -> &BTreeMap<TaskId, TaskSnapshot> {
        &self.tasks
    }

    pub fn host_resources(&self) -> &BTreeMap<ResourceId, ResourceFacts> {
        &self.host_resources
    }

    pub fn operations(&self) -> &BTreeMap<OperationId, OperationFacts> {
        &self.operations
    }

    pub fn artifact_summaries(&self) -> &BTreeMap<ArtifactId, ArtifactSummary> {
        &self.artifact_summaries
    }

    pub fn last_applied_sequence(&self) -> u64 {
        self.last_applied_sequence
    }

    /// Shared bound check for frozen replay continuation cursors/pages.
    pub fn check_replay_continuation_bounds(
        page_count: usize,
        max_pages: usize,
        seen_cursors: &HashSet<Vec<u8>>,
        next_cursor: &Vec<u8>,
    ) -> Result<(), ClientModelError> {
        if page_count >= max_pages {
            return Err(ClientModelError::ReplayPageBoundExceeded);
        }
        if seen_cursors.len() >= MAX_CLIENT_REPLAY_CURSORS {
            return Err(ClientModelError::CursorBoundExceeded);
        }
        if seen_cursors.contains(next_cursor) {
            return Err(ClientModelError::ReplayRepeatedCursor);
        }
        Ok(())
    }

    /// Apply one durable event. Sequences must be strictly greater than the
    /// current cursor; ordinary numeric gaps are allowed.
    ///
    /// Stages only the affected task/operation entries; never clones the whole model.
    pub fn apply_event(&mut self, event: &DomainEvent) -> Result<(), ClientModelError> {
        self.apply_one_event(event)
    }

    /// Apply every event on a frozen/live replay page, then advance the applied
    /// cursor to `through_sequence` when the page completes the frozen range.
    ///
    /// One page-level candidate clone preserves page transactionality; events are
    /// applied through [`Self::apply_one_event`] (no per-event model clone).
    pub fn apply_replay_page(&mut self, page: &EventPage) -> Result<(), ClientModelError> {
        let mut candidate = self.clone();
        candidate.apply_replay_page_inner(page)?;
        *self = candidate;
        Ok(())
    }

    fn apply_replay_page_inner(&mut self, page: &EventPage) -> Result<(), ClientModelError> {
        if page.through_sequence < page.after_sequence {
            return Err(ClientModelError::ReplayRangeInvalid);
        }
        if page.after_sequence != self.last_applied_sequence {
            return Err(ClientModelError::ReplayAfterMismatch);
        }
        match self.replay_through {
            Some(pinned) if pinned != page.through_sequence => {
                return Err(ClientModelError::ReplayThroughDrift);
            }
            Some(_) => {}
            None => self.replay_through = Some(page.through_sequence),
        }
        if page.events.is_empty() && page.next_cursor.is_some() {
            return Err(ClientModelError::ReplayNonProgressing);
        }
        if let Some(cursor) = &page.next_cursor {
            Self::check_replay_continuation_bounds(
                self.replay_page_count,
                MAX_CLIENT_REPLAY_PAGES,
                &self.replay_cursors,
                cursor,
            )?;
            self.replay_cursors.insert(cursor.clone());
        }
        self.replay_page_count = self
            .replay_page_count
            .checked_add(1)
            .ok_or(ClientModelError::ReplayPageBoundExceeded)?;
        if self.replay_page_count > MAX_CLIENT_REPLAY_PAGES {
            return Err(ClientModelError::ReplayPageBoundExceeded);
        }

        for event in &page.events {
            if event.sequence <= page.after_sequence || event.sequence > page.through_sequence {
                return Err(ClientModelError::ReplayRangeInvalid);
            }
            // Intentionally call apply_one_event (staged entry updates only), not a
            // full-model-cloning wrapper, so a large page does not clone per event.
            self.apply_one_event(event)?;
        }
        if page.next_cursor.is_none() {
            if page.through_sequence < self.last_applied_sequence {
                return Err(ClientModelError::ReplayRangeInvalid);
            }
            self.last_applied_sequence = page.through_sequence;
            self.replay_through = None;
            self.replay_page_count = 0;
            self.replay_cursors.clear();
        }
        Ok(())
    }

    /// Validate and commit one event by staging only affected task/operation facts.
    fn apply_one_event(&mut self, event: &DomainEvent) -> Result<(), ClientModelError> {
        if event.sequence <= self.last_applied_sequence {
            return Err(ClientModelError::DuplicateOrRegression);
        }
        let staged = self.stage_event(event)?;
        if let Some((task_id, snapshot)) = staged.task {
            self.tasks.insert(task_id, snapshot);
        }
        if let Some((operation_id, facts)) = staged.operation {
            self.operations.insert(operation_id, facts);
        }
        if let Some(summary) = staged.artifact_summary {
            self.artifact_summaries.insert(summary.id, summary);
        }
        self.last_applied_sequence = event.sequence;
        Ok(())
    }

    fn stage_event(&self, event: &DomainEvent) -> Result<StagedEventCommit, ClientModelError> {
        self.require_operation_envelope_timestamp(event)?;
        match &event.payload {
            Event::OperationAccepted(fact) => {
                let task = if let Some(task_id) = event.task_id {
                    let current = self.tasks.get(&task_id).cloned();
                    let next = apply(current, event).map_err(|_| ClientModelError::ApplyFailed)?;
                    Some((task_id, next))
                } else {
                    None
                };
                if self.operations.contains_key(&fact.operation_id) {
                    return Err(ClientModelError::OperationStateRegression);
                }
                Ok(StagedEventCommit {
                    task,
                    operation: Some((
                        fact.operation_id,
                        OperationFacts {
                            id: fact.operation_id,
                            command_id: fact.command_id,
                            task_id: event.task_id,
                            state: OperationState::Accepted,
                            accepted_at_ms: fact.accepted_at_ms,
                        },
                    )),
                    artifact_summary: None,
                })
            }
            Event::OperationSettled(fact) => {
                let task = self.stage_task_passthrough(event)?;
                let operation = self.stage_operation_outcome(
                    fact.operation_id,
                    fact.command_id,
                    event.task_id,
                    fact.settled_at_ms,
                    Some(&fact.source),
                    OperationState::Settled {
                        settled_at_ms: fact.settled_at_ms,
                        result_event_ids: fact.result_event_ids.clone(),
                    },
                )?;
                Ok(StagedEventCommit {
                    task,
                    operation: Some(operation),
                    artifact_summary: None,
                })
            }
            Event::OperationFailed(fact) => {
                let task = self.stage_task_passthrough(event)?;
                let operation = self.stage_operation_outcome(
                    fact.operation_id,
                    fact.command_id,
                    event.task_id,
                    fact.settled_at_ms,
                    Some(&fact.source),
                    OperationState::Failed {
                        settled_at_ms: fact.settled_at_ms,
                        code: fact.code,
                    },
                )?;
                Ok(StagedEventCommit {
                    task,
                    operation: Some(operation),
                    artifact_summary: None,
                })
            }
            Event::OperationCancelled(fact) => {
                let task = self.stage_task_passthrough(event)?;
                let operation = self.stage_operation_outcome(
                    fact.operation_id,
                    fact.command_id,
                    event.task_id,
                    fact.settled_at_ms,
                    None,
                    OperationState::Cancelled {
                        settled_at_ms: fact.settled_at_ms,
                        reason: fact.reason,
                    },
                )?;
                Ok(StagedEventCommit {
                    task,
                    operation: Some(operation),
                    artifact_summary: None,
                })
            }
            Event::OperationUncertain(fact) => {
                let task = self.stage_task_passthrough(event)?;
                let operation = self.stage_operation_outcome(
                    fact.operation_id,
                    fact.command_id,
                    event.task_id,
                    fact.observed_at_ms,
                    None,
                    OperationState::Uncertain {
                        observed_at_ms: fact.observed_at_ms,
                        code: fact.code,
                    },
                )?;
                Ok(StagedEventCommit {
                    task,
                    operation: Some(operation),
                    artifact_summary: None,
                })
            }
            Event::TaskCreated { task, .. } => {
                if self.tasks.contains_key(&task.id) {
                    return Err(ClientModelError::ApplyFailed);
                }
                let next = apply(None, event).map_err(|_| ClientModelError::ApplyFailed)?;
                Ok(StagedEventCommit {
                    task: Some((task.id, next)),
                    operation: None,
                    artifact_summary: None,
                })
            }
            Event::ArtifactRegistered { artifact } => {
                if self.artifact_summaries.contains_key(&artifact.id) {
                    return Err(ClientModelError::DuplicateItem);
                }
                let task_id = event.task_id.ok_or(ClientModelError::ApplyFailed)?;
                let current = self.tasks.get(&task_id).cloned();
                let mut next = apply(current, event).map_err(|_| ClientModelError::ApplyFailed)?;
                // Domain apply inserts full ArtifactFacts for revision correctness;
                // the public client model retains metadata-only summaries.
                let summary = ArtifactSummary::from_facts(artifact)
                    .map_err(|_| ClientModelError::ApplyFailed)?;
                next.artifacts.remove(&artifact.id);
                Ok(StagedEventCommit {
                    task: Some((task_id, next)),
                    operation: None,
                    artifact_summary: Some(summary),
                })
            }
            Event::HostCloseBegun { .. } => Ok(StagedEventCommit {
                task: None,
                operation: None,
                artifact_summary: None,
            }),
            _ => {
                let task_id = event.task_id.ok_or(ClientModelError::ApplyFailed)?;
                let current = self.tasks.get(&task_id).cloned();
                let next = apply(current, event).map_err(|_| ClientModelError::ApplyFailed)?;
                Ok(StagedEventCommit {
                    task: Some((task_id, next)),
                    operation: None,
                    artifact_summary: None,
                })
            }
        }
    }

    fn require_operation_envelope_timestamp(
        &self,
        event: &DomainEvent,
    ) -> Result<(), ClientModelError> {
        let _ = self;
        let expected = match &event.payload {
            Event::OperationAccepted(fact) => fact.accepted_at_ms,
            Event::OperationSettled(fact) => fact.settled_at_ms,
            Event::OperationFailed(fact) => fact.settled_at_ms,
            Event::OperationCancelled(fact) => fact.settled_at_ms,
            Event::OperationUncertain(fact) => fact.observed_at_ms,
            _ => return Ok(()),
        };
        if event.occurred_at_ms != expected {
            return Err(ClientModelError::OperationEnvelopeTimestampMismatch);
        }
        Ok(())
    }

    fn stage_task_passthrough(
        &self,
        event: &DomainEvent,
    ) -> Result<Option<(TaskId, TaskSnapshot)>, ClientModelError> {
        let Some(task_id) = event.task_id else {
            return Ok(None);
        };
        let current = self.tasks.get(&task_id).cloned();
        let next = apply(current, event).map_err(|_| ClientModelError::ApplyFailed)?;
        Ok(Some((task_id, next)))
    }

    fn stage_operation_outcome(
        &self,
        operation_id: OperationId,
        command_id: crate::domain::id::CommandId,
        task_id: Option<TaskId>,
        outcome_at_ms: i64,
        source: Option<&crate::domain::operation::OutcomeSource>,
        next_state: OperationState,
    ) -> Result<(OperationId, OperationFacts), ClientModelError> {
        use crate::domain::operation::OutcomeSource;

        let mut operation = self
            .operations
            .get(&operation_id)
            .cloned()
            .ok_or(ClientModelError::MissingOperation)?;
        if operation.command_id != command_id || operation.task_id != task_id {
            return Err(ClientModelError::OperationIdentityMismatch);
        }
        if outcome_at_ms < operation.accepted_at_ms {
            return Err(ClientModelError::OperationStateRegression);
        }

        let next_kind = match &next_state {
            OperationState::Settled { .. } => "settled",
            OperationState::Failed { .. } => "failed",
            OperationState::Cancelled { .. } => "cancelled",
            OperationState::Uncertain { .. } => "uncertain",
            OperationState::Accepted => {
                return Err(ClientModelError::OperationStateRegression);
            }
        };

        match (&operation.state, source, next_kind) {
            (OperationState::Accepted, None, "cancelled" | "uncertain") => {}
            (OperationState::Accepted, Some(OutcomeSource::Dispatch), "settled" | "failed") => {}
            (
                OperationState::Uncertain { observed_at_ms, .. },
                Some(OutcomeSource::VerifiedReconciliation { .. }),
                "settled" | "failed",
            ) => {
                if outcome_at_ms < *observed_at_ms {
                    return Err(ClientModelError::OperationStateRegression);
                }
            }
            (
                OperationState::Accepted,
                Some(OutcomeSource::VerifiedReconciliation { .. }),
                "settled" | "failed",
            ) => {
                return Err(ClientModelError::OperationStateRegression);
            }
            (
                OperationState::Uncertain { .. },
                Some(OutcomeSource::Dispatch),
                "settled" | "failed",
            ) => {
                return Err(ClientModelError::OperationStateRegression);
            }
            _ => return Err(ClientModelError::OperationStateRegression),
        }

        operation.state = next_state;
        Ok((operation_id, operation))
    }
}

struct StagedEventCommit {
    task: Option<(TaskId, TaskSnapshot)>,
    operation: Option<(OperationId, OperationFacts)>,
    artifact_summary: Option<ArtifactSummary>,
}

#[derive(Debug, Default)]
struct SectionAssembly {
    started: bool,
    finished: bool,
    seen_cursors: HashSet<Vec<u8>>,
    expected_after: Option<crate::domain::snapshot::SnapshotItemKey>,
}

fn section_index(section: SnapshotSection) -> usize {
    match section {
        SnapshotSection::Tasks => 0,
        SnapshotSection::AgentSessions => 1,
        SnapshotSection::Artifacts => 2,
        SnapshotSection::Resources => 3,
        SnapshotSection::Operations => 4,
    }
}

fn snapshot_item_key(item: &SnapshotItem) -> crate::domain::snapshot::SnapshotItemKey {
    match item {
        SnapshotItem::Task(task) => crate::domain::snapshot::SnapshotItemKey::Task(task.task.id),
        SnapshotItem::AgentSession(agent) => {
            crate::domain::snapshot::SnapshotItemKey::AgentSession(agent.id)
        }
        SnapshotItem::Artifact(artifact) => {
            crate::domain::snapshot::SnapshotItemKey::Artifact(artifact.id)
        }
        SnapshotItem::Resource(resource) => {
            crate::domain::snapshot::SnapshotItemKey::Resource(resource.id)
        }
        SnapshotItem::Operation(operation) => {
            crate::domain::snapshot::SnapshotItemKey::Operation(operation.id)
        }
    }
}

/// Pure builder that admits bounded snapshot pages from one pinned view.
#[derive(Debug)]
pub struct ClientModelBuilder {
    snapshot_id: Option<SnapshotId>,
    through_sequence: Option<u64>,
    page_count: usize,
    item_count: usize,
    sections: [SectionAssembly; 5],
    tasks: BTreeMap<TaskId, TaskSnapshotItem>,
    agents: BTreeMap<AgentSessionId, AgentSessionFacts>,
    artifacts: BTreeMap<ArtifactId, ArtifactSummary>,
    resources: BTreeMap<ResourceId, ResourceFacts>,
    operations: BTreeMap<OperationId, OperationFacts>,
}

impl Default for ClientModelBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl ClientModelBuilder {
    pub fn new() -> Self {
        Self {
            snapshot_id: None,
            through_sequence: None,
            page_count: 0,
            item_count: 0,
            sections: Default::default(),
            tasks: BTreeMap::new(),
            agents: BTreeMap::new(),
            artifacts: BTreeMap::new(),
            resources: BTreeMap::new(),
            operations: BTreeMap::new(),
        }
    }

    pub fn ingest_page(&mut self, page: SnapshotPage) -> Result<(), ClientModelError> {
        if self.page_count >= MAX_CLIENT_MODEL_PAGES {
            return Err(ClientModelError::PageBoundExceeded);
        }
        self.page_count += 1;

        match self.snapshot_id {
            Some(expected) if expected != page.snapshot_id => {
                return Err(ClientModelError::SnapshotDrift);
            }
            Some(_) => {}
            None => self.snapshot_id = Some(page.snapshot_id),
        }
        match self.through_sequence {
            Some(expected) if expected != page.through_sequence => {
                return Err(ClientModelError::SequenceDrift);
            }
            Some(_) => {}
            None => self.through_sequence = Some(page.through_sequence),
        }

        let section_idx = section_index(page.section);
        {
            let section = &mut self.sections[section_idx];
            if section.finished {
                return Err(ClientModelError::DuplicateSection);
            }
            if !section.started {
                if page.after_item.is_some() {
                    return Err(ClientModelError::NonProgressingPage);
                }
                section.started = true;
            } else {
                match (&section.expected_after, &page.after_item) {
                    (Some(expected), Some(actual)) if expected == actual => {}
                    _ => return Err(ClientModelError::SnapshotBoundaryMismatch),
                }
            }
        }

        if page.items.is_empty() && page.next_cursor.is_some() {
            return Err(ClientModelError::NonProgressingPage);
        }

        for item in &page.items {
            if self.item_count >= MAX_CLIENT_MODEL_ITEMS {
                return Err(ClientModelError::ItemBoundExceeded);
            }
            self.item_count += 1;
            self.admit_item(page.section, item)?;
        }

        let last_key = page.items.last().map(snapshot_item_key);
        let section = &mut self.sections[section_idx];
        match page.next_cursor {
            Some(cursor) => {
                if section.seen_cursors.len() >= MAX_CLIENT_MODEL_CURSORS_PER_SECTION {
                    return Err(ClientModelError::CursorBoundExceeded);
                }
                if !section.seen_cursors.insert(cursor) {
                    return Err(ClientModelError::RepeatedCursor);
                }
                let Some(last_key) = last_key else {
                    return Err(ClientModelError::NonProgressingPage);
                };
                section.expected_after = Some(last_key);
            }
            None => {
                section.expected_after = None;
                section.finished = true;
            }
        }
        Ok(())
    }

    pub fn finish(self) -> Result<ClientModel, ClientModelError> {
        if self.sections.iter().any(|section| !section.finished) {
            return Err(ClientModelError::MissingSections);
        }
        let through_sequence = self
            .through_sequence
            .ok_or(ClientModelError::MissingSections)?;

        let mut tasks = BTreeMap::new();
        for (task_id, item) in self.tasks {
            tasks.insert(
                task_id,
                TaskSnapshot {
                    task: item.task,
                    connectivity: item.connectivity,
                    attention: item.attention,
                    activity: item.activity,
                    review_readiness: item.review_readiness,
                    agents: BTreeMap::new(),
                    primary_agent_id: item.primary_agent_id,
                    artifacts: BTreeMap::new(),
                    resources: BTreeMap::new(),
                },
            );
        }

        for (agent_id, agent) in self.agents {
            let task = tasks
                .get_mut(&agent.task_id)
                .ok_or(ClientModelError::MissingParentTask)?;
            if task.agents.insert(agent_id, agent).is_some() {
                return Err(ClientModelError::DuplicateItem);
            }
        }

        // Snapshot ArtifactSummary items never invent content_ref into TaskSnapshot.
        for summary in self.artifacts.values() {
            if !tasks.contains_key(&summary.task_id) {
                return Err(ClientModelError::MissingParentTask);
            }
        }

        let mut host_resources = BTreeMap::new();
        for (resource_id, resource) in self.resources {
            match resource.owner_kind {
                OwnerKind::Host => {
                    if resource.task_id.is_some() {
                        return Err(ClientModelError::InvalidOwnership);
                    }
                    if host_resources.insert(resource_id, resource).is_some() {
                        return Err(ClientModelError::DuplicateItem);
                    }
                }
                OwnerKind::Task => {
                    let task_id = resource.task_id.ok_or(ClientModelError::InvalidOwnership)?;
                    let task = tasks
                        .get_mut(&task_id)
                        .ok_or(ClientModelError::MissingParentTask)?;
                    if task.resources.insert(resource_id, resource).is_some() {
                        return Err(ClientModelError::DuplicateItem);
                    }
                }
            }
        }

        for task in tasks.values() {
            if let Some(primary) = task.primary_agent_id {
                let Some(agent) = task.agents.get(&primary) else {
                    return Err(ClientModelError::InvalidPrimaryAgent);
                };
                if !matches!(agent.role, crate::domain::agent::AgentRole::Primary) {
                    return Err(ClientModelError::InvalidPrimaryAgent);
                }
            }
        }

        for operation in self.operations.values() {
            if let Some(task_id) = operation.task_id {
                if !tasks.contains_key(&task_id) {
                    return Err(ClientModelError::MissingParentTask);
                }
            }
        }

        Ok(ClientModel {
            tasks,
            host_resources,
            operations: self.operations,
            artifact_summaries: self.artifacts,
            last_applied_sequence: through_sequence,
            replay_through: None,
            replay_page_count: 0,
            replay_cursors: HashSet::new(),
        })
    }

    fn admit_item(
        &mut self,
        section: SnapshotSection,
        item: &SnapshotItem,
    ) -> Result<(), ClientModelError> {
        match (section, item) {
            (SnapshotSection::Tasks, SnapshotItem::Task(task_item)) => {
                if self
                    .tasks
                    .insert(task_item.task.id, task_item.clone())
                    .is_some()
                {
                    return Err(ClientModelError::DuplicateItem);
                }
            }
            (SnapshotSection::AgentSessions, SnapshotItem::AgentSession(agent)) => {
                if self.agents.insert(agent.id, agent.clone()).is_some() {
                    return Err(ClientModelError::DuplicateItem);
                }
            }
            (SnapshotSection::Artifacts, SnapshotItem::Artifact(artifact)) => {
                if self
                    .artifacts
                    .insert(artifact.id, artifact.clone())
                    .is_some()
                {
                    return Err(ClientModelError::DuplicateItem);
                }
            }
            (SnapshotSection::Resources, SnapshotItem::Resource(resource)) => {
                resource
                    .validate()
                    .map_err(|_| ClientModelError::InvalidOwnership)?;
                if self
                    .resources
                    .insert(resource.id, resource.clone())
                    .is_some()
                {
                    return Err(ClientModelError::DuplicateItem);
                }
            }
            (SnapshotSection::Operations, SnapshotItem::Operation(operation)) => {
                if self
                    .operations
                    .insert(operation.id, operation.clone())
                    .is_some()
                {
                    return Err(ClientModelError::DuplicateItem);
                }
            }
            _ => return Err(ClientModelError::SectionItemMismatch),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::agent::{AgentRole, AgentSessionFacts, AgentSessionLifecycle};
    use crate::domain::artifact::{ArtifactKind, ArtifactSummary, PrivacyClass};
    use crate::domain::event::{
        OperationAcceptedFact, OperationCancelledFact, OperationFailedFact, OperationSettledFact,
        OperationUncertainFact,
    };
    use crate::domain::id::{
        AgentSessionId, ArtifactId, CommandId, EnvironmentId, EventId, OperationId, ProjectId,
        ResourceId, SnapshotId, TaskId,
    };
    use crate::domain::operation::{
        CancellationReason, OperationErrorCode, OperationState, OperationUncertaintyCode,
        OutcomeSource,
    };
    use crate::domain::resource::{
        OwnerKind, ResourceFacts, ResourceKind, ResourceLifecycle, ResourceRecipe,
    };
    use crate::domain::task::{
        ReviewReadiness, TaskActivity, TaskAssignment, TaskAttention, TaskConnectivity, TaskFacts,
        TaskLifecycle, WorkspaceRef,
    };

    fn fixed_uuid_v7(tail: u8) -> [u8; 16] {
        [
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, tail,
        ]
    }

    fn snapshot_id(tail: u8) -> SnapshotId {
        SnapshotId::from_bytes(fixed_uuid_v7(tail)).expect("snapshot id")
    }
    fn task_id(tail: u8) -> TaskId {
        TaskId::from_bytes(fixed_uuid_v7(tail)).expect("task id")
    }
    fn agent_id(tail: u8) -> AgentSessionId {
        AgentSessionId::from_bytes(fixed_uuid_v7(tail)).expect("agent id")
    }
    fn artifact_id(tail: u8) -> ArtifactId {
        ArtifactId::from_bytes(fixed_uuid_v7(tail)).expect("artifact id")
    }
    fn resource_id(tail: u8) -> ResourceId {
        ResourceId::from_bytes(fixed_uuid_v7(tail)).expect("resource id")
    }
    fn operation_id(tail: u8) -> OperationId {
        OperationId::from_bytes(fixed_uuid_v7(tail)).expect("operation id")
    }
    fn command_id(tail: u8) -> CommandId {
        CommandId::from_bytes(fixed_uuid_v7(tail)).expect("command id")
    }
    fn event_id(tail: u8) -> EventId {
        EventId::from_bytes(fixed_uuid_v7(tail)).expect("event id")
    }
    fn env_id(tail: u8) -> EnvironmentId {
        EnvironmentId::from_bytes(fixed_uuid_v7(tail)).expect("env id")
    }
    fn project_id(tail: u8) -> ProjectId {
        ProjectId::from_bytes(fixed_uuid_v7(tail)).expect("project id")
    }

    fn task_facts(id: TaskId, title: &str) -> TaskFacts {
        TaskFacts {
            id,
            environment_id: env_id(0x10),
            title: title.into(),
            description: None,
            project_id: project_id(0x11),
            workspace: WorkspaceRef::Main,
            assignment: TaskAssignment::LocalOwner,
            lifecycle: TaskLifecycle::Open,
            action_epoch: 0,
            revision: 1,
            created_at_ms: 1_725_000_000_000,
        }
    }

    fn task_item(id: TaskId, title: &str, primary: Option<AgentSessionId>) -> TaskSnapshotItem {
        TaskSnapshotItem {
            task: task_facts(id, title),
            connectivity: TaskConnectivity::Connected,
            attention: TaskAttention::None,
            activity: TaskActivity::Idle,
            review_readiness: ReviewReadiness::NotReady,
            primary_agent_id: primary,
        }
    }

    fn page(
        snapshot: SnapshotId,
        through: u64,
        section: SnapshotSection,
        after: Option<crate::domain::snapshot::SnapshotItemKey>,
        items: Vec<SnapshotItem>,
        next: Option<Vec<u8>>,
    ) -> SnapshotPage {
        SnapshotPage {
            snapshot_id: snapshot,
            through_sequence: through,
            section,
            after_item: after,
            items,
            encoded_bytes: 1,
            next_cursor: next,
        }
    }

    fn empty_section_pages(snapshot: SnapshotId, through: u64) -> Vec<SnapshotPage> {
        [
            SnapshotSection::AgentSessions,
            SnapshotSection::Artifacts,
            SnapshotSection::Resources,
            SnapshotSection::Operations,
        ]
        .into_iter()
        .map(|section| page(snapshot, through, section, None, Vec::new(), None))
        .collect()
    }

    fn assemble_all_sections(
        snapshot: SnapshotId,
        through: u64,
        task_pages: Vec<SnapshotPage>,
        extras: Vec<SnapshotPage>,
    ) -> ClientModel {
        let mut builder = ClientModelBuilder::new();
        for page in task_pages {
            builder.ingest_page(page).expect("ingest task page");
        }
        for page in &extras {
            builder
                .ingest_page(page.clone())
                .expect("ingest section page");
        }
        for page in empty_section_pages(snapshot, through) {
            if extras
                .iter()
                .any(|existing| existing.section == page.section)
            {
                continue;
            }
            builder.ingest_page(page).expect("ingest empty section");
        }
        builder.finish().expect("finish model")
    }

    #[test]
    fn assembles_all_sections_with_nested_ownership() {
        let snap = snapshot_id(0x01);
        let through = 9;
        let task = task_id(0x21);
        let agent = agent_id(0x22);
        let artifact = artifact_id(0x23);
        let task_resource = resource_id(0x24);
        let host_resource = resource_id(0x25);
        let operation = operation_id(0x26);

        let mut builder = ClientModelBuilder::new();
        builder
            .ingest_page(page(
                snap,
                through,
                SnapshotSection::Tasks,
                None,
                vec![SnapshotItem::Task(task_item(task, "Nested", Some(agent)))],
                None,
            ))
            .expect("tasks page");

        builder
            .ingest_page(page(
                snap,
                through,
                SnapshotSection::AgentSessions,
                None,
                vec![SnapshotItem::AgentSession(AgentSessionFacts {
                    id: agent,
                    task_id: task,
                    role: AgentRole::Primary,
                    provider_kind: "claude".into(),
                    provider_session_id: Some("sess-1".into()),
                    lifecycle: AgentSessionLifecycle::Open,
                    runtime_generation: 0,
                    revision: 0,
                })],
                None,
            ))
            .expect("agents");
        builder
            .ingest_page(page(
                snap,
                through,
                SnapshotSection::Artifacts,
                None,
                vec![SnapshotItem::Artifact(ArtifactSummary {
                    id: artifact,
                    task_id: task,
                    kind: ArtifactKind::Finding,
                    label: "note".into(),
                    sha256: [1u8; 32],
                    privacy_class: PrivacyClass::LocalOnly,
                    created_at_ms: 1,
                })],
                None,
            ))
            .expect("artifacts");
        builder
            .ingest_page(page(
                snap,
                through,
                SnapshotSection::Resources,
                None,
                vec![
                    SnapshotItem::Resource(ResourceFacts {
                        id: task_resource,
                        task_id: Some(task),
                        owner_kind: OwnerKind::Task,
                        resource_kind: ResourceKind::Terminal,
                        recipe: ResourceRecipe::Terminal { cols: 80, rows: 24 },
                        lifecycle: ResourceLifecycle::Active,
                        runtime_generation: 0,
                        updated_at_ms: 1,
                    }),
                    SnapshotItem::Resource(ResourceFacts {
                        id: host_resource,
                        task_id: None,
                        owner_kind: OwnerKind::Host,
                        resource_kind: ResourceKind::Service,
                        recipe: ResourceRecipe::Service {
                            command: "echo host".into(),
                        },
                        lifecycle: ResourceLifecycle::Active,
                        runtime_generation: 0,
                        updated_at_ms: 1,
                    }),
                ],
                None,
            ))
            .expect("resources");
        builder
            .ingest_page(page(
                snap,
                through,
                SnapshotSection::Operations,
                None,
                vec![SnapshotItem::Operation(OperationFacts {
                    id: operation,
                    command_id: command_id(0x27),
                    task_id: Some(task),
                    state: OperationState::Accepted,
                    accepted_at_ms: 1,
                })],
                None,
            ))
            .expect("operations");

        let model = builder.finish().expect("assembled model");
        assert_eq!(model.last_applied_sequence(), through);
        let nested = model.tasks().get(&task).expect("task present");
        assert_eq!(nested.agents.len(), 1);
        assert_eq!(nested.primary_agent_id, Some(agent));
        assert!(nested.artifacts.is_empty());
        assert!(model.artifact_summaries().contains_key(&artifact));
        assert!(nested.resources.contains_key(&task_resource));
        assert!(!nested.resources.contains_key(&host_resource));
        assert!(model.host_resources().contains_key(&host_resource));
        assert_eq!(
            model.operations().get(&operation).map(|op| &op.state),
            Some(&OperationState::Accepted)
        );
    }

    #[test]
    fn artifact_registered_event_retains_summary_only_without_inline_body() {
        // Catches: apply_event keeping full ArtifactFacts (inline body) in TaskSnapshot.
        use crate::domain::artifact::{ArtifactContentRef, ArtifactFacts};
        use sha2::{Digest, Sha256};

        let snap = snapshot_id(0xA0);
        let task = task_id(0xA1);
        let mut model = assemble_all_sections(
            snap,
            1,
            vec![page(
                snap,
                1,
                SnapshotSection::Tasks,
                None,
                vec![SnapshotItem::Task(task_item(task, "Live", None))],
                None,
            )],
            Vec::new(),
        );
        assert_eq!(model.last_applied_sequence(), 1);
        assert!(model.tasks().get(&task).expect("task").artifacts.is_empty());

        const BODY: &str = "CLIENT_MODEL_INLINE_BODY_TOKEN_2_5E";
        let artifact = artifact_id(0xA2);
        let mut hasher = Sha256::new();
        hasher.update(BODY.as_bytes());
        let digest: [u8; 32] = hasher.finalize().into();
        let facts = ArtifactFacts {
            id: artifact,
            task_id: task,
            kind: ArtifactKind::Evidence,
            label: "LiveEvidence".into(),
            content_ref: ArtifactContentRef::inline_utf8(BODY).expect("body"),
            sha256: digest,
            privacy_class: PrivacyClass::LocalOnly,
            created_at_ms: 2,
        };
        model
            .apply_event(&DomainEvent {
                id: event_id(0xA3),
                task_id: Some(task),
                sequence: 2,
                task_revision: Some(2),
                occurred_at_ms: 2,
                payload: Event::ArtifactRegistered {
                    artifact: facts.clone(),
                },
            })
            .expect("artifact registration applies");

        assert_eq!(model.last_applied_sequence(), 2);
        let nested = model.tasks().get(&task).expect("task present");
        assert_eq!(nested.task.revision, 2);
        assert!(
            nested.artifacts.is_empty(),
            "task snapshot must not retain full artifact facts"
        );
        let summary = model
            .artifact_summaries()
            .get(&artifact)
            .expect("summary retained");
        assert_eq!(summary.id, artifact);
        assert_eq!(summary.sha256, digest);
        assert_eq!(summary.label, "LiveEvidence");
        let model_debug = format!("{model:?}");
        assert!(
            !model_debug.contains(BODY),
            "public client model must not retain distinctive inline body"
        );
        let encoded = rmp_serde::to_vec_named(summary).expect("encode summary");
        assert!(
            !encoded
                .windows(BODY.len())
                .any(|window| window == BODY.as_bytes()),
            "summary encoding must omit body"
        );
    }

    #[test]
    fn artifact_registered_rejects_snapshot_duplicate_id_without_mutation() {
        // Catches: live ArtifactRegistered overwriting a snapshot-held summary ID.
        use crate::domain::artifact::{ArtifactContentRef, ArtifactFacts};
        use sha2::{Digest, Sha256};

        let snap = snapshot_id(0xB0);
        let task = task_id(0xB1);
        let artifact = artifact_id(0xB2);
        const ORIGINAL_LABEL: &str = "SnapshotHeld";
        let original_digest = [0x11u8; 32];
        let mut model = assemble_all_sections(
            snap,
            1,
            vec![page(
                snap,
                1,
                SnapshotSection::Tasks,
                None,
                vec![SnapshotItem::Task(task_item(task, "Dup", None))],
                None,
            )],
            vec![page(
                snap,
                1,
                SnapshotSection::Artifacts,
                None,
                vec![SnapshotItem::Artifact(ArtifactSummary {
                    id: artifact,
                    task_id: task,
                    kind: ArtifactKind::Finding,
                    label: ORIGINAL_LABEL.into(),
                    sha256: original_digest,
                    privacy_class: PrivacyClass::LocalOnly,
                    created_at_ms: 1,
                })],
                None,
            )],
        );
        assert_eq!(model.last_applied_sequence(), 1);
        assert_eq!(model.tasks().get(&task).expect("task").task.revision, 1);
        assert_eq!(
            model
                .artifact_summaries()
                .get(&artifact)
                .expect("summary")
                .label,
            ORIGINAL_LABEL
        );

        const BODY: &str = "SNAPSHOT_DUP_BODY_TOKEN";
        let mut hasher = Sha256::new();
        hasher.update(BODY.as_bytes());
        let digest: [u8; 32] = hasher.finalize().into();
        let err = model.apply_event(&DomainEvent {
            id: event_id(0xB3),
            task_id: Some(task),
            sequence: 2,
            task_revision: Some(2),
            occurred_at_ms: 2,
            payload: Event::ArtifactRegistered {
                artifact: ArtifactFacts {
                    id: artifact,
                    task_id: task,
                    kind: ArtifactKind::Evidence,
                    label: "Overwritten".into(),
                    content_ref: ArtifactContentRef::inline_utf8(BODY).expect("body"),
                    sha256: digest,
                    privacy_class: PrivacyClass::LocalOnly,
                    created_at_ms: 2,
                },
            },
        });
        assert_eq!(err, Err(ClientModelError::DuplicateItem));
        assert_eq!(model.last_applied_sequence(), 1);
        assert_eq!(model.tasks().get(&task).expect("task").task.revision, 1);
        let summary = model
            .artifact_summaries()
            .get(&artifact)
            .expect("original summary retained");
        assert_eq!(summary.label, ORIGINAL_LABEL);
        assert_eq!(summary.sha256, original_digest);
    }

    #[test]
    fn artifact_registered_rejects_live_duplicate_id_without_mutation() {
        // Catches: second live ArtifactRegistered silently overwriting the first summary.
        use crate::domain::artifact::{ArtifactContentRef, ArtifactFacts};
        use sha2::{Digest, Sha256};

        let snap = snapshot_id(0xC0);
        let task = task_id(0xC1);
        let artifact = artifact_id(0xC2);
        let mut model = assemble_all_sections(
            snap,
            1,
            vec![page(
                snap,
                1,
                SnapshotSection::Tasks,
                None,
                vec![SnapshotItem::Task(task_item(task, "LiveDup", None))],
                None,
            )],
            Vec::new(),
        );

        let body_a = "LIVE_DUP_BODY_A";
        let mut hasher = Sha256::new();
        hasher.update(body_a.as_bytes());
        let digest_a: [u8; 32] = hasher.finalize().into();
        model
            .apply_event(&DomainEvent {
                id: event_id(0xC3),
                task_id: Some(task),
                sequence: 2,
                task_revision: Some(2),
                occurred_at_ms: 2,
                payload: Event::ArtifactRegistered {
                    artifact: ArtifactFacts {
                        id: artifact,
                        task_id: task,
                        kind: ArtifactKind::Evidence,
                        label: "First".into(),
                        content_ref: ArtifactContentRef::inline_utf8(body_a).expect("body"),
                        sha256: digest_a,
                        privacy_class: PrivacyClass::LocalOnly,
                        created_at_ms: 2,
                    },
                },
            })
            .expect("first registration");
        assert_eq!(model.last_applied_sequence(), 2);
        assert_eq!(model.tasks().get(&task).expect("task").task.revision, 2);

        let body_b = "LIVE_DUP_BODY_B";
        let mut hasher = Sha256::new();
        hasher.update(body_b.as_bytes());
        let digest_b: [u8; 32] = hasher.finalize().into();
        let err = model.apply_event(&DomainEvent {
            id: event_id(0xC4),
            task_id: Some(task),
            sequence: 3,
            task_revision: Some(3),
            occurred_at_ms: 3,
            payload: Event::ArtifactRegistered {
                artifact: ArtifactFacts {
                    id: artifact,
                    task_id: task,
                    kind: ArtifactKind::Evidence,
                    label: "Second".into(),
                    content_ref: ArtifactContentRef::inline_utf8(body_b).expect("body"),
                    sha256: digest_b,
                    privacy_class: PrivacyClass::LocalOnly,
                    created_at_ms: 3,
                },
            },
        });
        assert_eq!(err, Err(ClientModelError::DuplicateItem));
        assert_eq!(model.last_applied_sequence(), 2);
        assert_eq!(model.tasks().get(&task).expect("task").task.revision, 2);
        let summary = model
            .artifact_summaries()
            .get(&artifact)
            .expect("first summary retained");
        assert_eq!(summary.label, "First");
        assert_eq!(summary.sha256, digest_a);
    }

    #[test]
    fn applies_exact_events_allows_gaps_rejects_duplicates_and_regressions() {
        let snap = snapshot_id(0x02);
        let task = task_id(0x31);
        let mut model = assemble_all_sections(
            snap,
            1,
            vec![page(
                snap,
                1,
                SnapshotSection::Tasks,
                None,
                vec![SnapshotItem::Task(task_item(task, "Gap", None))],
                None,
            )],
            Vec::new(),
        );
        assert_eq!(model.last_applied_sequence(), 1);

        let renamed = DomainEvent {
            id: event_id(0x32),
            task_id: Some(task),
            sequence: 4,
            task_revision: Some(2),
            occurred_at_ms: 2,
            payload: Event::TaskRenamed {
                title: "Gap filled".into(),
            },
        };
        model.apply_event(&renamed).expect("gap 1 -> 4 is valid");
        assert_eq!(model.last_applied_sequence(), 4);
        assert_eq!(model.tasks()[&task].task.title, "Gap filled");

        assert_eq!(
            model.apply_event(&renamed),
            Err(ClientModelError::DuplicateOrRegression)
        );
        let regression = DomainEvent {
            id: event_id(0x33),
            task_id: Some(task),
            sequence: 3,
            task_revision: Some(3),
            occurred_at_ms: 3,
            payload: Event::TaskAttentionSet {
                attention: TaskAttention::NeedsAnswer,
            },
        };
        assert_eq!(
            model.apply_event(&regression),
            Err(ClientModelError::DuplicateOrRegression)
        );
        assert_eq!(model.last_applied_sequence(), 4);
    }

    #[test]
    fn operation_terminal_state_is_monotonic() {
        let snap = snapshot_id(0x03);
        let task = task_id(0x41);
        let operation = operation_id(0x42);
        let command = command_id(0x43);
        let mut model = assemble_all_sections(
            snap,
            2,
            vec![page(
                snap,
                2,
                SnapshotSection::Tasks,
                None,
                vec![SnapshotItem::Task(task_item(task, "Ops", None))],
                None,
            )],
            vec![page(
                snap,
                2,
                SnapshotSection::Operations,
                None,
                vec![SnapshotItem::Operation(OperationFacts {
                    id: operation,
                    command_id: command,
                    task_id: Some(task),
                    state: OperationState::Accepted,
                    accepted_at_ms: 1,
                })],
                None,
            )],
        );

        let settled = DomainEvent {
            id: event_id(0x44),
            task_id: Some(task),
            sequence: 5,
            task_revision: None,
            occurred_at_ms: 5,
            payload: Event::OperationSettled(
                OperationSettledFact::with_source(
                    command,
                    operation,
                    5,
                    vec![event_id(0x45)],
                    None,
                    None,
                    None,
                    OutcomeSource::Dispatch,
                )
                .expect("settled"),
            ),
        };
        model.apply_event(&settled).expect("accepted -> settled");
        assert!(matches!(
            model.operations()[&operation].state,
            OperationState::Settled { .. }
        ));

        let failed = DomainEvent {
            id: event_id(0x46),
            task_id: Some(task),
            sequence: 6,
            task_revision: None,
            occurred_at_ms: 6,
            payload: Event::OperationFailed(
                OperationFailedFact::with_source(
                    command,
                    operation,
                    6,
                    OperationErrorCode::SideEffectFailed,
                    None,
                    None,
                    None,
                    OutcomeSource::Dispatch,
                )
                .expect("failed"),
            ),
        };
        assert_eq!(
            model.apply_event(&failed),
            Err(ClientModelError::OperationStateRegression)
        );
        assert_eq!(model.last_applied_sequence(), 5);
        assert!(matches!(
            model.operations()[&operation].state,
            OperationState::Settled { .. }
        ));
    }

    #[test]
    fn replay_page_advances_cursor_to_through_sequence_on_completion() {
        let snap = snapshot_id(0x04);
        let task = task_id(0x51);
        let mut model = assemble_all_sections(
            snap,
            3,
            vec![page(
                snap,
                3,
                SnapshotSection::Tasks,
                None,
                vec![SnapshotItem::Task(task_item(task, "Replay", None))],
                None,
            )],
            Vec::new(),
        );

        let page = EventPage {
            after_sequence: 3,
            through_sequence: 7,
            events: vec![DomainEvent {
                id: event_id(0x52),
                task_id: Some(task),
                sequence: 5,
                task_revision: Some(2),
                occurred_at_ms: 5,
                payload: Event::TaskRenamed {
                    title: "Replayed".into(),
                },
            }],
            next_cursor: None,
        };
        model.apply_replay_page(&page).expect("replay page");
        assert_eq!(model.last_applied_sequence(), 7);
        assert_eq!(model.tasks()[&task].task.title, "Replayed");
    }

    #[test]
    fn rejects_operation_accepted_duplicate_identity_mismatch() {
        let snap = snapshot_id(0x05);
        let task = task_id(0x61);
        let mut model = assemble_all_sections(
            snap,
            1,
            vec![page(
                snap,
                1,
                SnapshotSection::Tasks,
                None,
                vec![SnapshotItem::Task(task_item(task, "Accept", None))],
                None,
            )],
            Vec::new(),
        );

        let operation = operation_id(0x62);
        let command = command_id(0x63);
        let accepted = DomainEvent {
            id: event_id(0x64),
            task_id: Some(task),
            sequence: 2,
            task_revision: None,
            occurred_at_ms: 2,
            payload: Event::OperationAccepted(
                OperationAcceptedFact::new(command, operation, 2, None, None, None)
                    .expect("accepted"),
            ),
        };
        model.apply_event(&accepted).expect("first accept");
        assert_eq!(
            model.apply_event(&DomainEvent {
                id: event_id(0x65),
                task_id: Some(task),
                sequence: 3,
                task_revision: None,
                occurred_at_ms: 3,
                payload: Event::OperationAccepted(
                    OperationAcceptedFact::new(command, operation, 3, None, None, None)
                        .expect("dup"),
                ),
            }),
            Err(ClientModelError::OperationStateRegression)
        );
    }

    #[test]
    fn failed_task_event_leaves_model_byte_equal() {
        let snap = snapshot_id(0x10);
        let task = task_id(0x71);
        let mut model = assemble_all_sections(
            snap,
            1,
            vec![page(
                snap,
                1,
                SnapshotSection::Tasks,
                None,
                vec![SnapshotItem::Task(task_item(task, "Stable", None))],
                None,
            )],
            Vec::new(),
        );
        let before = model.clone();
        let bad = DomainEvent {
            id: event_id(0x72),
            task_id: Some(task),
            sequence: 2,
            task_revision: Some(99),
            occurred_at_ms: 2,
            payload: Event::TaskRenamed {
                title: "Should not stick".into(),
            },
        };
        assert_eq!(model.apply_event(&bad), Err(ClientModelError::ApplyFailed));
        assert_eq!(model, before);
        assert!(model.tasks().contains_key(&task));
        assert_eq!(model.last_applied_sequence(), 1);
    }

    #[test]
    fn failed_operation_identity_leaves_model_byte_equal() {
        let snap = snapshot_id(0x11);
        let task = task_id(0x73);
        let operation = operation_id(0x74);
        let command = command_id(0x75);
        let mut model = assemble_all_sections(
            snap,
            2,
            vec![page(
                snap,
                2,
                SnapshotSection::Tasks,
                None,
                vec![SnapshotItem::Task(task_item(task, "OpStable", None))],
                None,
            )],
            vec![page(
                snap,
                2,
                SnapshotSection::Operations,
                None,
                vec![SnapshotItem::Operation(OperationFacts {
                    id: operation,
                    command_id: command,
                    task_id: Some(task),
                    state: OperationState::Accepted,
                    accepted_at_ms: 1,
                })],
                None,
            )],
        );
        let before = model.clone();
        let bad = DomainEvent {
            id: event_id(0x76),
            task_id: Some(task),
            sequence: 3,
            task_revision: None,
            occurred_at_ms: 3,
            payload: Event::OperationSettled(
                OperationSettledFact::with_source(
                    command_id(0x77),
                    operation,
                    3,
                    vec![event_id(0x78)],
                    None,
                    None,
                    None,
                    OutcomeSource::Dispatch,
                )
                .expect("settled"),
            ),
        };
        assert_eq!(
            model.apply_event(&bad),
            Err(ClientModelError::OperationIdentityMismatch)
        );
        assert_eq!(model, before);
    }

    #[test]
    fn uncertain_reconciliation_follows_kernel_source_and_time_rules() {
        let snap = snapshot_id(0x12);
        let task = task_id(0x80);
        let operation = operation_id(0x81);
        let command = command_id(0x82);
        let mut model = assemble_all_sections(
            snap,
            2,
            vec![page(
                snap,
                2,
                SnapshotSection::Tasks,
                None,
                vec![SnapshotItem::Task(task_item(task, "Reconcile", None))],
                None,
            )],
            vec![page(
                snap,
                2,
                SnapshotSection::Operations,
                None,
                vec![SnapshotItem::Operation(OperationFacts {
                    id: operation,
                    command_id: command,
                    task_id: Some(task),
                    state: OperationState::Accepted,
                    accepted_at_ms: 10,
                })],
                None,
            )],
        );

        let rejected_verified_from_accepted = DomainEvent {
            id: event_id(0x83),
            task_id: Some(task),
            sequence: 3,
            task_revision: None,
            occurred_at_ms: 20,
            payload: Event::OperationSettled(
                OperationSettledFact::with_source(
                    command,
                    operation,
                    20,
                    vec![event_id(0x84)],
                    None,
                    None,
                    None,
                    OutcomeSource::verified_reconciliation(0, "ext-1").expect("source"),
                )
                .expect("settled"),
            ),
        };
        let before = model.clone();
        assert_eq!(
            model.apply_event(&rejected_verified_from_accepted),
            Err(ClientModelError::OperationStateRegression)
        );
        assert_eq!(model, before);

        model
            .apply_event(&DomainEvent {
                id: event_id(0x85),
                task_id: Some(task),
                sequence: 3,
                task_revision: None,
                occurred_at_ms: 20,
                payload: Event::OperationUncertain(
                    OperationUncertainFact::new(
                        command,
                        operation,
                        20,
                        OperationUncertaintyCode::AmbiguousDispatch,
                        None,
                        None,
                        None,
                    )
                    .expect("uncertain"),
                ),
            })
            .expect("accepted -> uncertain");

        let before_bad_time = model.clone();
        let early_reconcile = DomainEvent {
            id: event_id(0x86),
            task_id: Some(task),
            sequence: 4,
            task_revision: None,
            occurred_at_ms: 15,
            payload: Event::OperationSettled(
                OperationSettledFact::with_source(
                    command,
                    operation,
                    15,
                    vec![event_id(0x87)],
                    None,
                    None,
                    None,
                    OutcomeSource::verified_reconciliation(0, "ext-2").expect("source"),
                )
                .expect("settled"),
            ),
        };
        assert_eq!(
            model.apply_event(&early_reconcile),
            Err(ClientModelError::OperationStateRegression)
        );
        assert_eq!(model, before_bad_time);

        let before_dispatch = model.clone();
        let dispatch_from_uncertain = DomainEvent {
            id: event_id(0x88),
            task_id: Some(task),
            sequence: 4,
            task_revision: None,
            occurred_at_ms: 30,
            payload: Event::OperationSettled(
                OperationSettledFact::with_source(
                    command,
                    operation,
                    30,
                    vec![event_id(0x89)],
                    None,
                    None,
                    None,
                    OutcomeSource::Dispatch,
                )
                .expect("settled"),
            ),
        };
        assert_eq!(
            model.apply_event(&dispatch_from_uncertain),
            Err(ClientModelError::OperationStateRegression)
        );
        assert_eq!(model, before_dispatch);

        model
            .apply_event(&DomainEvent {
                id: event_id(0x8a),
                task_id: Some(task),
                sequence: 4,
                task_revision: None,
                occurred_at_ms: 30,
                payload: Event::OperationSettled(
                    OperationSettledFact::with_source(
                        command,
                        operation,
                        30,
                        vec![event_id(0x8b)],
                        None,
                        None,
                        None,
                        OutcomeSource::verified_reconciliation(0, "ext-3").expect("source"),
                    )
                    .expect("settled"),
                ),
            })
            .expect("uncertain -> settled via verified reconciliation");
        assert!(matches!(
            model.operations()[&operation].state,
            OperationState::Settled {
                settled_at_ms: 30,
                ..
            }
        ));
    }

    #[test]
    fn replay_chain_rejects_after_skip_and_through_drift() {
        let snap = snapshot_id(0x13);
        let task = task_id(0x90);
        let mut model = assemble_all_sections(
            snap,
            3,
            vec![page(
                snap,
                3,
                SnapshotSection::Tasks,
                None,
                vec![SnapshotItem::Task(task_item(task, "Chain", None))],
                None,
            )],
            Vec::new(),
        );
        let before = model.clone();
        assert_eq!(
            model.apply_replay_page(&EventPage {
                after_sequence: 5,
                through_sequence: 9,
                events: vec![],
                next_cursor: None,
            }),
            Err(ClientModelError::ReplayAfterMismatch)
        );
        assert_eq!(model, before);

        model
            .apply_replay_page(&EventPage {
                after_sequence: 3,
                through_sequence: 9,
                events: vec![DomainEvent {
                    id: event_id(0x91),
                    task_id: Some(task),
                    sequence: 5,
                    task_revision: Some(2),
                    occurred_at_ms: 5,
                    payload: Event::TaskRenamed {
                        title: "Page one".into(),
                    },
                }],
                next_cursor: Some(vec![0x01]),
            })
            .expect("first frozen page");
        assert_eq!(model.last_applied_sequence(), 5);

        let mid = model.clone();
        assert_eq!(
            model.apply_replay_page(&EventPage {
                after_sequence: 5,
                through_sequence: 10,
                events: vec![DomainEvent {
                    id: event_id(0x92),
                    task_id: Some(task),
                    sequence: 8,
                    task_revision: Some(3),
                    occurred_at_ms: 8,
                    payload: Event::TaskAttentionSet {
                        attention: TaskAttention::NeedsAnswer,
                    },
                }],
                next_cursor: None,
            }),
            Err(ClientModelError::ReplayThroughDrift)
        );
        assert_eq!(model, mid);
    }

    #[test]
    fn replay_chain_rejects_repeated_cursor_and_page_bound() {
        let snap = snapshot_id(0x14);
        let task = task_id(0x93);
        let mut model = assemble_all_sections(
            snap,
            1,
            vec![page(
                snap,
                1,
                SnapshotSection::Tasks,
                None,
                vec![SnapshotItem::Task(task_item(task, "Bound", None))],
                None,
            )],
            Vec::new(),
        );
        let cursor = vec![0xaa];
        model
            .apply_replay_page(&EventPage {
                after_sequence: 1,
                through_sequence: 4,
                events: vec![DomainEvent {
                    id: event_id(0x94),
                    task_id: Some(task),
                    sequence: 2,
                    task_revision: Some(2),
                    occurred_at_ms: 2,
                    payload: Event::TaskRenamed {
                        title: "First".into(),
                    },
                }],
                next_cursor: Some(cursor.clone()),
            })
            .expect("open continuing page");
        let before = model.clone();
        assert_eq!(
            model.apply_replay_page(&EventPage {
                after_sequence: 2,
                through_sequence: 4,
                events: vec![DomainEvent {
                    id: event_id(0x95),
                    task_id: Some(task),
                    sequence: 3,
                    task_revision: Some(3),
                    occurred_at_ms: 3,
                    payload: Event::TaskRenamed {
                        title: "Loop".into(),
                    },
                }],
                next_cursor: Some(cursor),
            }),
            Err(ClientModelError::ReplayRepeatedCursor)
        );
        assert_eq!(model, before);

        let err = ClientModel::check_replay_continuation_bounds(
            MAX_CLIENT_REPLAY_PAGES,
            MAX_CLIENT_REPLAY_PAGES,
            &HashSet::new(),
            &vec![1],
        );
        assert_eq!(err, Err(ClientModelError::ReplayPageBoundExceeded));
        let err = ClientModel::check_replay_continuation_bounds(
            1,
            MAX_CLIENT_REPLAY_PAGES,
            &{
                let mut seen = HashSet::new();
                seen.insert(vec![9]);
                seen
            },
            &vec![9],
        );
        assert_eq!(err, Err(ClientModelError::ReplayRepeatedCursor));
    }

    #[test]
    fn snapshot_continuation_rejects_mismatched_after_item() {
        let snap = snapshot_id(0x16);
        let first = task_id(0xa0);
        let second = task_id(0xa1);
        let spoofed = task_id(0xa2);
        let mut builder = ClientModelBuilder::new();
        builder
            .ingest_page(page(
                snap,
                4,
                SnapshotSection::Tasks,
                None,
                vec![SnapshotItem::Task(task_item(first, "One", None))],
                Some(vec![0x01]),
            ))
            .expect("first page");
        let err = builder.ingest_page(page(
            snap,
            4,
            SnapshotSection::Tasks,
            Some(crate::domain::snapshot::SnapshotItemKey::Task(spoofed)),
            vec![SnapshotItem::Task(task_item(second, "Two", None))],
            None,
        ));
        assert_eq!(err, Err(ClientModelError::SnapshotBoundaryMismatch));
        assert!(builder.finish().is_err());
    }

    #[test]
    fn operation_envelope_timestamps_must_match_facts() {
        let snap = snapshot_id(0x20);
        let task = task_id(0xc0);
        let operation = operation_id(0xc1);
        let command = command_id(0xc2);
        let mut model = assemble_all_sections(
            snap,
            2,
            vec![page(
                snap,
                2,
                SnapshotSection::Tasks,
                None,
                vec![SnapshotItem::Task(task_item(task, "Envelope", None))],
                None,
            )],
            vec![page(
                snap,
                2,
                SnapshotSection::Operations,
                None,
                vec![SnapshotItem::Operation(OperationFacts {
                    id: operation,
                    command_id: command,
                    task_id: Some(task),
                    state: OperationState::Accepted,
                    accepted_at_ms: 10,
                })],
                None,
            )],
        );

        let before = model.clone();
        assert_eq!(
            model.apply_event(&DomainEvent {
                id: event_id(0xc3),
                task_id: Some(task),
                sequence: 3,
                task_revision: None,
                occurred_at_ms: 99,
                payload: Event::OperationAccepted(
                    OperationAcceptedFact::new(
                        command_id(0xc4),
                        operation_id(0xc5),
                        3,
                        None,
                        None,
                        None
                    )
                    .expect("accepted"),
                ),
            }),
            Err(ClientModelError::OperationEnvelopeTimestampMismatch)
        );
        assert_eq!(model, before);

        assert_eq!(
            model.apply_event(&DomainEvent {
                id: event_id(0xc6),
                task_id: Some(task),
                sequence: 3,
                task_revision: None,
                occurred_at_ms: 11,
                payload: Event::OperationSettled(
                    OperationSettledFact::with_source(
                        command,
                        operation,
                        10,
                        vec![event_id(0xc7)],
                        None,
                        None,
                        None,
                        OutcomeSource::Dispatch,
                    )
                    .expect("settled"),
                ),
            }),
            Err(ClientModelError::OperationEnvelopeTimestampMismatch)
        );
        assert_eq!(model, before);

        assert_eq!(
            model.apply_event(&DomainEvent {
                id: event_id(0xc8),
                task_id: Some(task),
                sequence: 3,
                task_revision: None,
                occurred_at_ms: 12,
                payload: Event::OperationFailed(
                    OperationFailedFact::with_source(
                        command,
                        operation,
                        10,
                        OperationErrorCode::SideEffectFailed,
                        None,
                        None,
                        None,
                        OutcomeSource::Dispatch,
                    )
                    .expect("failed"),
                ),
            }),
            Err(ClientModelError::OperationEnvelopeTimestampMismatch)
        );
        assert_eq!(model, before);

        assert_eq!(
            model.apply_event(&DomainEvent {
                id: event_id(0xc9),
                task_id: Some(task),
                sequence: 3,
                task_revision: None,
                occurred_at_ms: 13,
                payload: Event::OperationCancelled(
                    OperationCancelledFact::new(
                        command,
                        operation,
                        10,
                        CancellationReason::Superseded,
                        None,
                        None,
                        None,
                    )
                    .expect("cancelled"),
                ),
            }),
            Err(ClientModelError::OperationEnvelopeTimestampMismatch)
        );
        assert_eq!(model, before);

        assert_eq!(
            model.apply_event(&DomainEvent {
                id: event_id(0xca),
                task_id: Some(task),
                sequence: 3,
                task_revision: None,
                occurred_at_ms: 14,
                payload: Event::OperationUncertain(
                    OperationUncertainFact::new(
                        command,
                        operation,
                        10,
                        OperationUncertaintyCode::AmbiguousDispatch,
                        None,
                        None,
                        None,
                    )
                    .expect("uncertain"),
                ),
            }),
            Err(ClientModelError::OperationEnvelopeTimestampMismatch)
        );
        assert_eq!(model, before);

        model
            .apply_event(&DomainEvent {
                id: event_id(0xcb),
                task_id: Some(task),
                sequence: 3,
                task_revision: None,
                occurred_at_ms: 20,
                payload: Event::OperationUncertain(
                    OperationUncertainFact::new(
                        command,
                        operation,
                        20,
                        OperationUncertaintyCode::AmbiguousDispatch,
                        None,
                        None,
                        None,
                    )
                    .expect("uncertain"),
                ),
            })
            .expect("matching uncertain envelope remains valid");
        assert!(matches!(
            model.operations()[&operation].state,
            OperationState::Uncertain {
                observed_at_ms: 20,
                ..
            }
        ));
    }

    #[test]
    fn replay_page_late_failure_leaves_public_model_unchanged() {
        let snap = snapshot_id(0x21);
        let task = task_id(0xd0);
        let mut model = assemble_all_sections(
            snap,
            3,
            vec![page(
                snap,
                3,
                SnapshotSection::Tasks,
                None,
                vec![SnapshotItem::Task(task_item(task, "LateFail", None))],
                None,
            )],
            Vec::new(),
        );
        let before = model.clone();
        let err = model.apply_replay_page(&EventPage {
            after_sequence: 3,
            through_sequence: 8,
            events: vec![
                DomainEvent {
                    id: event_id(0xd1),
                    task_id: Some(task),
                    sequence: 5,
                    task_revision: Some(2),
                    occurred_at_ms: 5,
                    payload: Event::TaskRenamed {
                        title: "Should roll back".into(),
                    },
                },
                DomainEvent {
                    id: event_id(0xd2),
                    task_id: Some(task),
                    sequence: 6,
                    task_revision: Some(99),
                    occurred_at_ms: 6,
                    payload: Event::TaskRenamed {
                        title: "Bad revision".into(),
                    },
                },
            ],
            next_cursor: None,
        });
        assert_eq!(err, Err(ClientModelError::ApplyFailed));
        assert_eq!(model, before);
        assert_eq!(model.tasks()[&task].task.title, "LateFail");
        assert_eq!(model.last_applied_sequence(), 3);
    }
}
