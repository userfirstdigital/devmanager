//! Sealed semantic journal view.

#[cfg(any(test, feature = "semantic-conformance"))]
use super::ParsedSemanticEvent;
use super::{
    InteractionEligibility, MessageRole, RenderModelError, SemanticEvent, SemanticEventBody,
    TimelineItemContent, TimelineItemModel,
};
use crate::client::model::ClientModel;
use crate::domain::id::{AgentSessionId, OperationId, RequestId, TaskId};
use crate::domain::{SemanticJournalFact, SemanticJournalPage, SemanticJournalPayload};
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
    LiveProjection,
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
    Event(SemanticEvent),
}

#[derive(Debug, Clone)]
pub struct SemanticJournalView {
    origin: JournalOrigin,
    availability: JournalAvailability,
    task_id: TaskId,
    rows: Vec<JournalRow>,
    cursor: Option<JournalCursor>,
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
            cursor: None,
        })
    }

    /// Bind an authenticated host page to the selected Task. Provider-neutral
    /// facts are converted only after the live task/agent fence is available;
    /// raw terminal bytes and provider envelopes never enter this renderer.
    pub fn from_live_page(
        model: &ClientModel,
        task_id: TaskId,
        page: &SemanticJournalPage,
    ) -> Result<Self, RenderModelError> {
        let target = live_target(model, task_id)?;
        if page.facts.len() > MAX_CONFORMANCE_JOURNAL_EVENTS {
            return Err(RenderModelError::WorkBoundExceeded {
                kind: "events",
                limit: MAX_CONFORMANCE_JOURNAL_EVENTS,
            });
        }
        let rows = page
            .facts
            .iter()
            .map(|fact| live_fact_event(task_id, target, fact).map(JournalRow::Event))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            origin: JournalOrigin::LiveProjection,
            availability: JournalAvailability::LiveProjection,
            task_id,
            rows,
            cursor: Some(JournalCursor {
                through_sequence: page.through_sequence,
            }),
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
            rows.push(JournalRow::Event(event.clone()));
        }
        Ok(Self {
            origin: JournalOrigin::ConformanceFixture,
            availability: JournalAvailability::ConformanceFixture,
            task_id,
            rows,
            cursor: None,
        })
    }

    pub fn origin(&self) -> JournalOrigin {
        self.origin
    }

    pub fn availability(&self) -> JournalAvailability {
        self.availability
    }

    pub fn journal_cursor(&self) -> Option<JournalCursor> {
        self.cursor
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
            let JournalRow::Event(event) = row;
            let mut item = registry.project(event)?;
            gate_interaction(&mut item, capabilities);
            items.push(item);
        }
        Ok(items)
    }
}

fn live_fact_event(
    task_id: TaskId,
    target: CapturedActionTarget,
    fact: &SemanticJournalFact,
) -> Result<SemanticEvent, RenderModelError> {
    use std::collections::BTreeMap;

    let provider = super::ProviderKind::parse(fact.provider.clone())?;
    let body = if fact.redacted {
        SemanticEventBody::Extension {
            source_kind: fact.kind.clone(),
            fields: BTreeMap::from([("status".to_string(), "redacted".to_string())]),
        }
    } else {
        match &fact.payload {
            SemanticJournalPayload::UserMessage { text } => SemanticEventBody::Message {
                role: "You".to_string(),
                role_kind: MessageRole::User,
                text: text.clone(),
                streaming: false,
            },
            SemanticJournalPayload::AssistantText { text } => SemanticEventBody::Message {
                role: "Assistant".to_string(),
                role_kind: MessageRole::Assistant,
                text: text.clone(),
                streaming: false,
            },
            SemanticJournalPayload::ReasoningSummary { text } => SemanticEventBody::Message {
                role: "Reasoning".to_string(),
                role_kind: MessageRole::Reasoning,
                text: text.clone(),
                streaming: false,
            },
            SemanticJournalPayload::ToolCall { tool_name, call_id } => SemanticEventBody::Tool {
                tool_id: call_id.clone(),
                name: tool_name.clone(),
                state: "running".to_string(),
                summary: String::new(),
            },
            SemanticJournalPayload::ToolResult { call_id, status } => SemanticEventBody::Tool {
                tool_id: call_id.clone(),
                name: "Tool result".to_string(),
                state: "completed".to_string(),
                summary: status.clone(),
            },
            SemanticJournalPayload::Question {
                question_id,
                prompt,
                options,
            } => SemanticEventBody::Question {
                request_id: RequestId::parse(question_id)
                    .or_else(|_| RequestId::from_bytes(*fact.id.as_bytes()))
                    .map_err(|_| RenderModelError::InvalidIdentity("request_id"))?,
                prompt: prompt.clone(),
                choices: options.clone(),
                action_epoch: target.action_epoch,
                runtime_generation: target.runtime_generation,
                capability: true,
                settled_choice: None,
            },
            SemanticJournalPayload::ApprovalRequest {
                request_id,
                summary,
            } => SemanticEventBody::Approval {
                request_id: RequestId::parse(request_id)
                    .or_else(|_| RequestId::from_bytes(*fact.id.as_bytes()))
                    .map_err(|_| RenderModelError::InvalidIdentity("request_id"))?,
                summary: summary.clone(),
                action_epoch: target.action_epoch,
                runtime_generation: target.runtime_generation,
                capability: true,
                settled: false,
            },
            SemanticJournalPayload::ApprovalResult {
                request_id,
                decision,
            } => SemanticEventBody::Approval {
                request_id: RequestId::parse(request_id)
                    .or_else(|_| RequestId::from_bytes(*fact.id.as_bytes()))
                    .map_err(|_| RenderModelError::InvalidIdentity("request_id"))?,
                summary: decision.clone(),
                action_epoch: target.action_epoch,
                runtime_generation: target.runtime_generation,
                capability: true,
                settled: true,
            },
            SemanticJournalPayload::PlanStep { title, status, .. } => SemanticEventBody::Plan {
                title: title.clone(),
                steps: vec![title.clone()],
                status: status.clone(),
            },
            SemanticJournalPayload::Error { code, message } => SemanticEventBody::Message {
                role: format!("Error ({code})"),
                role_kind: MessageRole::Error,
                text: message.clone(),
                streaming: false,
            },
            SemanticJournalPayload::TurnState { state }
            | SemanticJournalPayload::SessionState { state } => SemanticEventBody::Extension {
                source_kind: fact.kind.clone(),
                fields: BTreeMap::from([("state".to_string(), state.clone())]),
            },
            SemanticJournalPayload::UsageObservation { remaining_percent } => {
                SemanticEventBody::Extension {
                    source_kind: fact.kind.clone(),
                    fields: BTreeMap::from([(
                        "remaining".to_string(),
                        remaining_percent
                            .map(|value| format!("{value}%"))
                            .unwrap_or_else(|| "unknown".to_string()),
                    )]),
                }
            }
            SemanticJournalPayload::ArtifactReference { label } => SemanticEventBody::Extension {
                source_kind: fact.kind.clone(),
                fields: BTreeMap::from([("artifact".to_string(), label.clone())]),
            },
            SemanticJournalPayload::Unknown {
                source_type,
                diagnostic_ref,
                ..
            } => SemanticEventBody::Extension {
                source_kind: source_type.clone(),
                fields: BTreeMap::from([("detail".to_string(), diagnostic_ref.clone())]),
            },
        }
    };
    Ok(SemanticEvent {
        event_id: fact.id,
        task_id,
        schema_version: u16::try_from(fact.schema_version).unwrap_or(u16::MAX),
        provider,
        source_type: fact.kind.clone(),
        occurred_at_ms: fact
            .occurred_at_ms
            .and_then(|value| u64::try_from(value).ok())
            .unwrap_or(0),
        raw_terminal_available: false,
        turn_id: None,
        related_event_id: None,
        body,
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::id::EventId;
    use crate::domain::PrivacyClass;

    #[test]
    fn live_message_time_comes_from_the_fact_not_its_sequence() {
        let task_id = TaskId::new();
        let occurred_at_ms = 1_725_000_001_234_i64;
        let fact = SemanticJournalFact {
            id: EventId::new(),
            sequence: 17,
            occurred_at_ms: Some(occurred_at_ms),
            provider: "codex".to_string(),
            schema_version: 1,
            kind: "user_message".to_string(),
            visibility: "task".to_string(),
            privacy_class: PrivacyClass::LocalOnly,
            redacted: false,
            payload: SemanticJournalPayload::UserMessage {
                text: "hello".to_string(),
            },
        };
        let event = live_fact_event(
            task_id,
            CapturedActionTarget {
                task_id,
                agent_session_id: None,
                runtime_generation: 4,
                request_id: None,
                action_epoch: 9,
            },
            &fact,
        )
        .expect("fact projects");

        assert_eq!(event.occurred_at_ms, occurred_at_ms as u64);
        assert_ne!(event.occurred_at_ms, fact.sequence);
    }
}
