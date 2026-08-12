//! Sealed semantic journal view.
//!
//! Production cannot invent a conversation journal from ClientModel inventory.
//! Until Phase 4.6 supplies an authenticated journal page, the view is
//! [`JournalAvailability::Unavailable`].

#[cfg(any(test, feature = "semantic-conformance"))]
use super::ParsedSemanticEvent;
use super::{
    InteractionEligibility, RenderModelError, SemanticEvent, TimelineItemContent, TimelineItemModel,
};
use crate::client::model::ClientModel;
use crate::domain::id::{AgentSessionId, OperationId, RequestId, TaskId};
use crate::protocol::{Capability, CapabilitySet};

pub const ANSWER_QUESTION_ACTION_ID: &str = "task.answer_question";
pub const RESOLVE_APPROVAL_ACTION_ID: &str = "task.resolve_approval";
pub const INSPECT_OPERATION_ACTION_ID: &str = "operation.inspect";
pub const MAX_CONFORMANCE_JOURNAL_EVENTS: usize = 20_000;

const FORBIDDEN_TASK_SHOW: &str = "task.show";

const fn bytes_eq(left: &str, right: &str) -> bool {
    let left = left.as_bytes();
    let right = right.as_bytes();
    if left.len() != right.len() {
        return false;
    }
    let mut index = 0;
    while index < left.len() {
        if left[index] != right[index] {
            return false;
        }
        index += 1;
    }
    true
}

const _: () = assert!(!bytes_eq(ANSWER_QUESTION_ACTION_ID, FORBIDDEN_TASK_SHOW));
const _: () = assert!(!bytes_eq(RESOLVE_APPROVAL_ACTION_ID, FORBIDDEN_TASK_SHOW));
const _: () = assert!(!bytes_eq(INSPECT_OPERATION_ACTION_ID, FORBIDDEN_TASK_SHOW));

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JournalOrigin {
    LiveProjection,
    #[cfg(any(test, feature = "semantic-conformance"))]
    ConformanceFixture,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JournalUnavailableReason {
    MissingAuthenticatedJournalPage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JournalAvailability {
    Unavailable(JournalUnavailableReason),
    #[cfg(any(test, feature = "semantic-conformance"))]
    ConformanceFixture,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JournalCursor {
    pub through_sequence: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapturedActionTarget {
    pub task_id: TaskId,
    pub agent_session_id: Option<AgentSessionId>,
    pub runtime_generation: u64,
    pub request_id: Option<RequestId>,
    pub action_epoch: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimelineHoldReason {
    MissingCatalogAction { action_id: &'static str },
    CapabilityDenied { capability: Capability },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimelineActivation {
    Hold {
        reason: TimelineHoldReason,
        target: CapturedActionTarget,
    },
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
enum JournalRow {
    Fixture(SemanticEvent),
}

#[derive(Debug, Clone)]
pub struct SemanticJournalView {
    origin: JournalOrigin,
    availability: JournalAvailability,
    task_id: TaskId,
    rows: Vec<JournalRow>,
}

impl SemanticJournalView {
    /// Bind the live task/agent/generation target and return an empty
    /// Unavailable view. This never synthesizes journal rows from snapshot
    /// inventory. Phase 4.6 has no journal `SnapshotSection` on this base.
    pub fn from_live_projection(
        model: &ClientModel,
        task_id: TaskId,
    ) -> Result<Self, RenderModelError> {
        let _target = live_target(model, task_id)?;
        Ok(Self {
            origin: JournalOrigin::LiveProjection,
            availability: JournalAvailability::Unavailable(
                JournalUnavailableReason::MissingAuthenticatedJournalPage,
            ),
            task_id,
            rows: Vec::new(),
        })
    }

    #[cfg(any(test, feature = "semantic-conformance"))]
    pub fn from_conformance_fixtures(
        task_id: TaskId,
        events: &[ParsedSemanticEvent],
    ) -> Result<Self, RenderModelError> {
        if events.len() > MAX_CONFORMANCE_JOURNAL_EVENTS {
            return Err(RenderModelError::WorkBoundExceeded {
                kind: "events",
                limit: MAX_CONFORMANCE_JOURNAL_EVENTS,
            });
        }
        let mut rows = Vec::with_capacity(events.len());
        let mut seen = std::collections::BTreeSet::new();
        for parsed in events {
            let event = parsed.event();
            if event.task_id != task_id {
                return Err(RenderModelError::TaskMismatch);
            }
            if !seen.insert(event.event_id) {
                return Err(RenderModelError::InvalidIdentity("event_id"));
            }
            rows.push(JournalRow::Fixture(event.clone()));
        }
        Ok(Self {
            origin: JournalOrigin::ConformanceFixture,
            availability: JournalAvailability::ConformanceFixture,
            task_id,
            rows,
        })
    }

    pub fn origin(&self) -> JournalOrigin {
        self.origin
    }

    pub fn availability(&self) -> JournalAvailability {
        self.availability
    }

    pub fn journal_cursor(&self) -> Option<JournalCursor> {
        None
    }

    pub fn task_id(&self) -> TaskId {
        self.task_id
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub(crate) fn project_items(
        &self,
        registry: &super::RendererRegistry,
        capabilities: CapabilitySet,
    ) -> Result<Vec<TimelineItemModel>, RenderModelError> {
        if matches!(self.availability, JournalAvailability::Unavailable(_)) {
            return Ok(Vec::new());
        }
        let mut items = Vec::with_capacity(self.rows.len());
        for row in &self.rows {
            let JournalRow::Fixture(event) = row;
            let mut item = registry.project(event)?;
            gate_interaction(&mut item, capabilities);
            items.push(item);
        }
        Ok(items)
    }
}

fn gate_interaction(item: &mut TimelineItemModel, capabilities: CapabilitySet) {
    if matches!(
        item.content,
        TimelineItemContent::Question(_) | TimelineItemContent::Approval(_)
    ) && !capabilities.contains(Capability::SemanticConversation)
    {
        item.interaction = InteractionEligibility::None;
    }
}

pub(crate) fn live_target(
    model: &ClientModel,
    task_id: TaskId,
) -> Result<CapturedActionTarget, RenderModelError> {
    let task = model
        .tasks()
        .get(&task_id)
        .ok_or(RenderModelError::TaskMismatch)?;
    let agent_session_id = task
        .primary_agent_id
        .ok_or(RenderModelError::MissingField("agent_session_id"))?;
    let agent = task
        .agents
        .get(&agent_session_id)
        .ok_or(RenderModelError::MissingField("agent_session_id"))?;
    Ok(CapturedActionTarget {
        task_id,
        agent_session_id: Some(agent_session_id),
        runtime_generation: agent.runtime_generation,
        request_id: None,
        action_epoch: task.task.action_epoch,
    })
}

fn catalog_contains(action_id: &str) -> bool {
    crate::client::action::catalog()
        .iter()
        .any(|descriptor| descriptor.id == action_id)
}

fn hold_missing_catalog(
    action_id: &'static str,
    target: CapturedActionTarget,
) -> Result<TimelineActivation, RenderModelError> {
    if bytes_eq(action_id, FORBIDDEN_TASK_SHOW) || catalog_contains(action_id) {
        return Err(RenderModelError::NotInteractive);
    }
    Ok(TimelineActivation::Hold {
        reason: TimelineHoldReason::MissingCatalogAction { action_id },
        target,
    })
}

fn replacement_invalidates_wait(
    live: &CapturedActionTarget,
    action_epoch: u64,
    runtime_generation: u64,
) -> Result<(), RenderModelError> {
    if live.action_epoch != action_epoch {
        return Err(RenderModelError::StaleEpoch);
    }
    if live.runtime_generation != runtime_generation {
        return Err(RenderModelError::StaleGeneration);
    }
    Ok(())
}

pub(crate) fn inspect_operation(
    present: bool,
    model: &ClientModel,
    task_id: TaskId,
    _operation_id: OperationId,
    captured: CapturedActionTarget,
    _capabilities: CapabilitySet,
) -> Result<TimelineActivation, RenderModelError> {
    if !present {
        return Err(RenderModelError::MissingField("operation"));
    }
    let live = live_target(model, task_id)?;
    replacement_invalidates_wait(&live, captured.action_epoch, captured.runtime_generation)?;
    if live.agent_session_id != captured.agent_session_id {
        return Err(RenderModelError::StaleGeneration);
    }
    hold_missing_catalog(INSPECT_OPERATION_ACTION_ID, live)
}

pub(crate) fn activate_question_item(
    item: &TimelineItemModel,
    choice: usize,
    model: &ClientModel,
    capabilities: CapabilitySet,
    request_id: RequestId,
) -> Result<TimelineActivation, RenderModelError> {
    let TimelineItemContent::Question(view) = &item.content else {
        return Err(RenderModelError::NotInteractive);
    };
    if view.settled_choice.is_some() {
        return Err(RenderModelError::AlreadySettled);
    }
    if !view.capability {
        return Err(RenderModelError::CapabilityDenied);
    }
    if item.interaction != InteractionEligibility::Question {
        return Err(RenderModelError::NotInteractive);
    }
    if !capabilities.contains(Capability::SemanticConversation) {
        return Err(RenderModelError::CapabilityDenied);
    }
    if choice >= view.choices.len() {
        return Err(RenderModelError::InvalidChoice);
    }
    if request_id != view.request_id {
        return Err(RenderModelError::InvalidIdentity("request_id"));
    }
    let live = live_target(model, item.task_id)?;
    replacement_invalidates_wait(&live, view.action_epoch, view.runtime_generation)?;
    hold_missing_catalog(
        ANSWER_QUESTION_ACTION_ID,
        CapturedActionTarget {
            request_id: Some(view.request_id),
            ..live
        },
    )
}

pub(crate) fn activate_approval_item(
    item: &TimelineItemModel,
    model: &ClientModel,
    capabilities: CapabilitySet,
    request_id: RequestId,
) -> Result<TimelineActivation, RenderModelError> {
    let TimelineItemContent::Approval(view) = &item.content else {
        return Err(RenderModelError::NotInteractive);
    };
    if view.settled {
        return Err(RenderModelError::AlreadySettled);
    }
    if !view.capability {
        return Err(RenderModelError::CapabilityDenied);
    }
    if item.interaction != InteractionEligibility::Approval {
        return Err(RenderModelError::NotInteractive);
    }
    if !capabilities.contains(Capability::SemanticConversation) {
        return Err(RenderModelError::CapabilityDenied);
    }
    if request_id != view.request_id {
        return Err(RenderModelError::InvalidIdentity("request_id"));
    }
    let live = live_target(model, item.task_id)?;
    replacement_invalidates_wait(&live, view.action_epoch, view.runtime_generation)?;
    hold_missing_catalog(
        RESOLVE_APPROVAL_ACTION_ID,
        CapturedActionTarget {
            request_id: Some(view.request_id),
            ..live
        },
    )
}
