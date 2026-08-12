//! Provider-neutral semantic renderer contract and registry.

mod agent;
mod approval;
mod artifact;
mod generic;
mod journal_view;
mod message;
mod operation;
mod plan;
mod question;
mod registry;
mod tool;

use std::collections::BTreeMap;
use std::fmt::{Display, Formatter};
use std::time::Duration;

#[cfg(any(test, feature = "semantic-conformance"))]
use serde_json::Value;

use crate::domain::id::{
    AgentSessionId, ArtifactId, CommandId, EventId, OperationId, RequestId, TaskId,
};
use crate::ui::components::interaction::{AccessibilityMetadata, ComponentError};

pub use agent::AgentRenderer;
pub use approval::ApprovalRenderer;
pub use artifact::ArtifactRenderer;
pub use generic::{GenericSemanticCard, GenericStatus};
pub(crate) use journal_view::{
    activate_approval_item, activate_question_item, inspect_operation, live_target,
};
pub use journal_view::{
    CapturedActionTarget, JournalAvailability, JournalCursor, JournalOrigin,
    JournalUnavailableReason, SemanticJournalView, TimelineActivation, TimelineHoldReason,
    ANSWER_QUESTION_ACTION_ID, INSPECT_OPERATION_ACTION_ID, MAX_CONFORMANCE_JOURNAL_EVENTS,
    RESOLVE_APPROVAL_ACTION_ID,
};
pub use message::{MarkdownBlock, MarkdownDocument, MessageRenderer, MessageView, PendingLink};
pub use operation::{OperationRenderState, OperationRenderer, OperationView};
pub use plan::{PlanRenderer, PlanView};
pub use question::QuestionRenderer;
pub use registry::{RendererRegistry, SemanticRenderer};
pub use tool::{ToolRenderer, ToolView};

pub const SEMANTIC_SCHEMA: &str = "devmanager.semantic/v1";
pub const SEMANTIC_SCHEMA_VERSION: u16 = 1;

const MAX_GENERIC_TITLE_SCALARS: usize = 160;
const MAX_GENERIC_FIELDS: usize = 32;
const MAX_GENERIC_KEY_SCALARS: usize = 64;
const MAX_GENERIC_VALUE_SCALARS: usize = 512;
const MAX_GENERIC_ENCODED_BYTES: usize = 16 * 1024;
pub const MAX_JOURNAL_STRING_SCALARS: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SemanticKind {
    Message,
    Tool,
    Question,
    Approval,
    Operation,
    Plan,
    Artifact,
    Agent,
}

impl SemanticKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::Tool => "tool",
            Self::Question => "question",
            Self::Approval => "approval",
            Self::Operation => "operation",
            Self::Plan => "plan",
            Self::Artifact => "artifact",
            Self::Agent => "agent",
        }
    }

    #[cfg(any(test, feature = "semantic-conformance"))]
    fn from_wire(value: &str) -> Option<Self> {
        Some(match value {
            "message" => Self::Message,
            "tool" => Self::Tool,
            "question" => Self::Question,
            "approval" => Self::Approval,
            "operation" => Self::Operation,
            "plan" => Self::Plan,
            "artifact" => Self::Artifact,
            "agent" => Self::Agent,
            _ => return None,
        })
    }
}

pub fn semantic_kind_discriminants() -> [&'static str; 8] {
    [
        SemanticKind::Message.as_str(),
        SemanticKind::Tool.as_str(),
        SemanticKind::Question.as_str(),
        SemanticKind::Approval.as_str(),
        SemanticKind::Operation.as_str(),
        SemanticKind::Plan.as_str(),
        SemanticKind::Artifact.as_str(),
        SemanticKind::Agent.as_str(),
    ]
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderKind(String);

impl ProviderKind {
    pub fn parse(value: impl Into<String>) -> Result<Self, RenderModelError> {
        let value = value.into();
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return Err(RenderModelError::InvalidProvider);
        }
        if trimmed.chars().count() > MAX_JOURNAL_STRING_SCALARS {
            return Err(RenderModelError::WorkBoundExceeded {
                kind: "provider",
                limit: MAX_JOURNAL_STRING_SCALARS,
            });
        }
        Ok(Self(trimmed.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimelineItemId {
    Event(EventId),
    Operation(OperationId),
    Artifact(ArtifactId),
    Agent(AgentSessionId),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticEvent {
    pub(crate) event_id: EventId,
    pub(crate) task_id: TaskId,
    pub(crate) schema_version: u16,
    pub(crate) provider: ProviderKind,
    pub(crate) source_type: String,
    pub(crate) occurred_at_ms: u64,
    pub(crate) raw_terminal_available: bool,
    pub(crate) turn_id: Option<String>,
    pub(crate) related_event_id: Option<EventId>,
    pub(crate) body: SemanticEventBody,
}

impl SemanticEvent {
    pub fn event_id(&self) -> EventId {
        self.event_id
    }

    pub fn task_id(&self) -> TaskId {
        self.task_id
    }

    pub fn body(&self) -> &SemanticEventBody {
        &self.body
    }

    pub fn specialized_kind(&self) -> Option<SemanticKind> {
        match &self.body {
            SemanticEventBody::Message { .. } => Some(SemanticKind::Message),
            SemanticEventBody::Tool { .. } => Some(SemanticKind::Tool),
            SemanticEventBody::Question { .. } => Some(SemanticKind::Question),
            SemanticEventBody::Approval { .. } => Some(SemanticKind::Approval),
            SemanticEventBody::Operation { .. } => Some(SemanticKind::Operation),
            SemanticEventBody::Plan { .. } => Some(SemanticKind::Plan),
            SemanticEventBody::Artifact { .. } => Some(SemanticKind::Artifact),
            SemanticEventBody::Agent { .. } => Some(SemanticKind::Agent),
            SemanticEventBody::Extension { .. } | SemanticEventBody::Malformed { .. } => None,
        }
    }

    pub fn generic_status(&self) -> Option<GenericStatus> {
        match &self.body {
            SemanticEventBody::Extension { .. } => Some(GenericStatus::Unknown),
            SemanticEventBody::Malformed { .. } => Some(GenericStatus::Malformed),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SemanticEventBody {
    Message {
        role: String,
        text: String,
        streaming: bool,
    },
    Tool {
        tool_id: String,
        name: String,
        state: String,
        summary: String,
    },
    Question {
        request_id: RequestId,
        prompt: String,
        choices: Vec<String>,
        action_epoch: u64,
        runtime_generation: u64,
        capability: bool,
        settled_choice: Option<usize>,
    },
    Approval {
        request_id: RequestId,
        summary: String,
        action_epoch: u64,
        runtime_generation: u64,
        capability: bool,
        settled: bool,
    },
    Operation {
        operation_id: OperationId,
        state: String,
        effect_evidence: Option<String>,
        command_id: Option<CommandId>,
    },
    Plan {
        title: String,
        steps: Vec<String>,
        status: String,
    },
    Artifact {
        artifact_id: ArtifactId,
        label: String,
        kind: String,
    },
    Agent {
        agent_session_id: AgentSessionId,
        role: String,
        specialist_name: Option<String>,
        parent_agent_session_id: Option<AgentSessionId>,
    },
    Extension {
        source_kind: String,
        fields: BTreeMap<String, String>,
    },
    Malformed {
        kind: String,
        fields: BTreeMap<String, String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RendererSelection {
    Specialized(SemanticKind),
    GenericFallback,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InteractionEligibility {
    None,
    NeedsMeWarning,
    Question,
    Approval,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimelineItemContent {
    Message(MessageView),
    Tool(ToolView),
    Question(QuestionView),
    Approval(ApprovalView),
    Operation(OperationView),
    Plan(PlanView),
    Artifact(ArtifactView),
    Agent(AgentView),
    Generic(GenericSemanticCard),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionView {
    pub request_id: RequestId,
    pub prompt: String,
    pub choices: Vec<String>,
    pub action_epoch: u64,
    pub runtime_generation: u64,
    pub capability: bool,
    pub settled_choice: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalView {
    pub request_id: RequestId,
    pub summary: String,
    pub action_epoch: u64,
    pub runtime_generation: u64,
    pub capability: bool,
    pub settled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactView {
    pub artifact_id: ArtifactId,
    pub label: String,
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentView {
    pub agent_session_id: AgentSessionId,
    pub role: String,
    pub specialist_name: Option<String>,
    pub parent_agent_session_id: Option<AgentSessionId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimelineItemModel {
    pub id: TimelineItemId,
    pub task_id: TaskId,
    pub renderer_selection: RendererSelection,
    pub interaction: InteractionEligibility,
    pub content: TimelineItemContent,
    pub activated_on_enter: bool,
    pub accessibility: AccessibilityMetadata,
    pub turn_id: Option<String>,
    pub related_event_id: Option<EventId>,
}

impl TimelineItemModel {
    pub fn estimated_height(&self) -> u32 {
        match &self.content {
            TimelineItemContent::Message(view) => view.markdown.estimated_height(),
            TimelineItemContent::Plan(view) => {
                72 + u32::try_from(view.steps.len() * 20).unwrap_or(u32::MAX)
            }
            TimelineItemContent::Generic(_) => 96,
            TimelineItemContent::Operation(view) if view.needs_me => 120,
            _ => 64,
        }
        .max(48)
        .min(480)
    }

    pub fn id(&self) -> TimelineItemId {
        self.id
    }

    pub fn on_task_entered(&mut self) {
        self.activated_on_enter = false;
    }

    pub fn activate_question(
        &self,
        choice: usize,
        model: &crate::client::model::ClientModel,
        capabilities: crate::protocol::CapabilitySet,
        request_id: RequestId,
    ) -> Result<TimelineActivation, RenderModelError> {
        activate_question_item(self, choice, model, capabilities, request_id)
    }

    pub fn activate_approval(
        &self,
        model: &crate::client::model::ClientModel,
        capabilities: crate::protocol::CapabilitySet,
        request_id: RequestId,
    ) -> Result<TimelineActivation, RenderModelError> {
        activate_approval_item(self, model, capabilities, request_id)
    }

    pub fn choice_labels(&self) -> Vec<&str> {
        match &self.content {
            TimelineItemContent::Question(view) => {
                view.choices.iter().map(String::as_str).collect()
            }
            _ => Vec::new(),
        }
    }

    pub fn traps_keyboard(&self) -> bool {
        false
    }

    pub fn encoded_len(&self) -> usize {
        match &self.content {
            TimelineItemContent::Generic(card) => card.encoded_len(),
            TimelineItemContent::Message(view) => view
                .markdown
                .plain_text()
                .len()
                .min(MAX_GENERIC_ENCODED_BYTES),
            TimelineItemContent::Tool(view) => view.summary.len().min(MAX_GENERIC_ENCODED_BYTES),
            TimelineItemContent::Question(view) => view.prompt.len().min(MAX_GENERIC_ENCODED_BYTES),
            TimelineItemContent::Approval(view) => {
                view.summary.len().min(MAX_GENERIC_ENCODED_BYTES)
            }
            TimelineItemContent::Operation(view) => view
                .effect_evidence
                .as_deref()
                .map(str::len)
                .unwrap_or(0)
                .min(MAX_GENERIC_ENCODED_BYTES),
            TimelineItemContent::Plan(view) => view.title.len().min(MAX_GENERIC_ENCODED_BYTES),
            TimelineItemContent::Artifact(view) => view.label.len().min(MAX_GENERIC_ENCODED_BYTES),
            TimelineItemContent::Agent(view) => view.role.len().min(MAX_GENERIC_ENCODED_BYTES),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConformanceArm {
    Baseline,
    Variant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConformanceReport {
    pub renderer_selection: RendererSelection,
    pub generic_fallback: bool,
    pub interaction_eligible: bool,
    pub update_latency_budget_ms: u128,
    pub encoded_bytes: usize,
    pub records_raw_provider_content: bool,
    pub arm: ConformanceArm,
}

impl ConformanceReport {
    pub fn capture(item: &TimelineItemModel, arm: ConformanceArm, elapsed: Duration) -> Self {
        Self {
            renderer_selection: item.renderer_selection,
            generic_fallback: item.renderer_selection == RendererSelection::GenericFallback,
            interaction_eligible: item.interaction != InteractionEligibility::None,
            update_latency_budget_ms: elapsed.as_millis(),
            encoded_bytes: item.encoded_len(),
            records_raw_provider_content: false,
            arm,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedSemanticEvent {
    Known(SemanticEvent),
    Unknown(SemanticEvent),
    Malformed(SemanticEvent),
}

impl ParsedSemanticEvent {
    pub fn event(&self) -> &SemanticEvent {
        match self {
            Self::Known(event) | Self::Unknown(event) | Self::Malformed(event) => event,
        }
    }

    pub fn into_event(self) -> SemanticEvent {
        match self {
            Self::Known(event) | Self::Unknown(event) | Self::Malformed(event) => event,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenderModelError {
    InvalidProvider,
    DuplicateKind(SemanticKind),
    MissingField(&'static str),
    InvalidIdentity(&'static str),
    MalformedKnown(SemanticKind),
    NotInteractive,
    StaleEpoch,
    StaleGeneration,
    CapabilityDenied,
    AlreadySettled,
    InvalidChoice,
    TaskMismatch,
    WorkBoundExceeded { kind: &'static str, limit: usize },
    Parse(String),
}

impl Display for RenderModelError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidProvider => write!(f, "provider kind is empty"),
            Self::DuplicateKind(kind) => {
                write!(f, "duplicate renderer registration for {}", kind.as_str())
            }
            Self::MissingField(field) => write!(f, "missing field {field}"),
            Self::InvalidIdentity(field) => write!(f, "invalid {field}"),
            Self::MalformedKnown(kind) => write!(f, "malformed {} event", kind.as_str()),
            Self::NotInteractive => write!(f, "item is not interactive"),
            Self::StaleEpoch => write!(f, "stale action epoch"),
            Self::StaleGeneration => write!(f, "stale runtime generation"),
            Self::CapabilityDenied => write!(f, "capability denied"),
            Self::AlreadySettled => write!(f, "already settled"),
            Self::InvalidChoice => write!(f, "invalid choice"),
            Self::TaskMismatch => write!(f, "event task id does not match the selected task"),
            Self::WorkBoundExceeded { kind, limit } => {
                write!(f, "work bound exceeded for {kind} (limit {limit})")
            }
            Self::Parse(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for RenderModelError {}

impl From<ComponentError> for RenderModelError {
    fn from(error: ComponentError) -> Self {
        Self::Parse(error.to_string())
    }
}

#[cfg(any(test, feature = "semantic-conformance"))]
pub fn parse_semantic_event(value: &Value) -> Result<ParsedSemanticEvent, RenderModelError> {
    let kind = required_str(value, "kind")?;
    let envelope = parse_envelope(value)?;
    match SemanticKind::from_wire(kind) {
        Some(known) => match parse_known_body(known, value) {
            Ok(body) => Ok(ParsedSemanticEvent::Known(SemanticEvent {
                body,
                ..envelope
            })),
            Err(error @ RenderModelError::WorkBoundExceeded { .. }) => Err(error),
            Err(_) => Ok(ParsedSemanticEvent::Malformed(SemanticEvent {
                body: SemanticEventBody::Malformed {
                    kind: kind.to_string(),
                    fields: collect_extension_fields(value),
                },
                ..envelope
            })),
        },
        None => Ok(ParsedSemanticEvent::Unknown(SemanticEvent {
            body: SemanticEventBody::Extension {
                source_kind: kind.to_string(),
                fields: collect_extension_fields(value),
            },
            ..envelope
        })),
    }
}

#[cfg(any(test, feature = "semantic-conformance"))]
fn parse_envelope(value: &Value) -> Result<SemanticEvent, RenderModelError> {
    Ok(SemanticEvent {
        event_id: parse_id(value, "event_id", EventId::parse)?,
        task_id: parse_id(value, "task_id", TaskId::parse)?,
        schema_version: value
            .get("schema_version")
            .and_then(Value::as_u64)
            .ok_or(RenderModelError::MissingField("schema_version"))?
            as u16,
        provider: ProviderKind::parse(required_str(value, "provider")?)?,
        source_type: bounded_required_string(value, "source_type")?,
        occurred_at_ms: value
            .get("occurred_at_ms")
            .and_then(Value::as_u64)
            .ok_or(RenderModelError::MissingField("occurred_at_ms"))?,
        raw_terminal_available: value
            .get("raw_terminal_available")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        turn_id: optional_bounded_string(value, "turn_id")?,
        related_event_id: match value.get("related_event_id") {
            None | Some(Value::Null) => None,
            Some(raw) => Some(parse_id_value(raw, "related_event_id", EventId::parse)?),
        },
        body: SemanticEventBody::Extension {
            source_kind: String::new(),
            fields: BTreeMap::new(),
        },
    })
}

#[cfg(any(test, feature = "semantic-conformance"))]
fn parse_known_body(
    kind: SemanticKind,
    value: &Value,
) -> Result<SemanticEventBody, RenderModelError> {
    match kind {
        SemanticKind::Message => Ok(SemanticEventBody::Message {
            role: bounded_required_string(value, "role")?,
            text: bounded_required_string(value, "text")?,
            streaming: value
                .get("streaming")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        }),
        SemanticKind::Tool => Ok(SemanticEventBody::Tool {
            tool_id: bounded_required_string(value, "tool_id")?,
            name: bounded_required_string(value, "name")?,
            state: bounded_required_string(value, "state")?,
            summary: bounded_required_string(value, "summary")?,
        }),
        SemanticKind::Question => Ok(SemanticEventBody::Question {
            request_id: parse_id(value, "request_id", RequestId::parse)?,
            prompt: bounded_required_string(value, "prompt")?,
            choices: required_string_array(value, "choices")?,
            action_epoch: required_u64(value, "action_epoch")?,
            runtime_generation: required_u64(value, "runtime_generation")?,
            capability: value
                .get("capability")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            settled_choice: value
                .get("settled_choice")
                .and_then(Value::as_u64)
                .map(|n| n as usize),
        }),
        SemanticKind::Approval => Ok(SemanticEventBody::Approval {
            request_id: parse_id(value, "request_id", RequestId::parse)?,
            summary: bounded_required_string(value, "summary")?,
            action_epoch: required_u64(value, "action_epoch")?,
            runtime_generation: required_u64(value, "runtime_generation")?,
            capability: value
                .get("capability")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            settled: value
                .get("settled")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        }),
        SemanticKind::Operation => Ok(SemanticEventBody::Operation {
            operation_id: parse_id(value, "operation_id", OperationId::parse)?,
            state: bounded_required_string(value, "state")?,
            effect_evidence: optional_bounded_string(value, "effect_evidence")?,
            command_id: match value.get("command_id") {
                None | Some(Value::Null) => None,
                Some(raw) => Some(parse_id_value(raw, "command_id", CommandId::parse)?),
            },
        }),
        SemanticKind::Plan => Ok(SemanticEventBody::Plan {
            title: bounded_required_string(value, "title")?,
            steps: required_string_array(value, "steps")?,
            status: bounded_required_string(value, "status")?,
        }),
        SemanticKind::Artifact => Ok(SemanticEventBody::Artifact {
            artifact_id: parse_id(value, "artifact_id", ArtifactId::parse)?,
            label: bounded_required_string(value, "label")?,
            kind: bounded_required_string(value, "artifact_kind")?,
        }),
        SemanticKind::Agent => Ok(SemanticEventBody::Agent {
            agent_session_id: parse_id(value, "agent_session_id", AgentSessionId::parse)?,
            role: bounded_required_string(value, "role")?,
            specialist_name: optional_bounded_string(value, "specialist_name")?,
            parent_agent_session_id: match value.get("parent_agent_session_id") {
                None | Some(Value::Null) => None,
                Some(raw) => Some(parse_id_value(
                    raw,
                    "parent_agent_session_id",
                    AgentSessionId::parse,
                )?),
            },
        }),
    }
}

#[cfg(any(test, feature = "semantic-conformance"))]
fn parse_id<T>(
    value: &Value,
    field: &'static str,
    parse: fn(&str) -> Result<T, crate::domain::id::IdError>,
) -> Result<T, RenderModelError> {
    let raw = required_str(value, field)?;
    parse_id_value(&Value::String(raw.to_string()), field, parse)
}

#[cfg(any(test, feature = "semantic-conformance"))]
fn parse_id_value<T>(
    value: &Value,
    field: &'static str,
    parse: fn(&str) -> Result<T, crate::domain::id::IdError>,
) -> Result<T, RenderModelError> {
    let raw = value
        .as_str()
        .ok_or(RenderModelError::InvalidIdentity(field))?;
    parse(raw).map_err(|_| RenderModelError::InvalidIdentity(field))
}

#[cfg(any(test, feature = "semantic-conformance"))]
fn bounded_required_string(value: &Value, field: &'static str) -> Result<String, RenderModelError> {
    let text = required_str(value, field)?;
    if text.chars().count() > MAX_JOURNAL_STRING_SCALARS {
        return Err(RenderModelError::WorkBoundExceeded {
            kind: field,
            limit: MAX_JOURNAL_STRING_SCALARS,
        });
    }
    Ok(text.to_string())
}

#[cfg(any(test, feature = "semantic-conformance"))]
fn optional_bounded_string(
    value: &Value,
    field: &'static str,
) -> Result<Option<String>, RenderModelError> {
    optional_str(value, field)
        .map(|text| {
            if text.chars().count() > MAX_JOURNAL_STRING_SCALARS {
                Err(RenderModelError::WorkBoundExceeded {
                    kind: field,
                    limit: MAX_JOURNAL_STRING_SCALARS,
                })
            } else {
                Ok(text.to_string())
            }
        })
        .transpose()
}

#[cfg(any(test, feature = "semantic-conformance"))]
fn required_str<'a>(value: &'a Value, field: &'static str) -> Result<&'a str, RenderModelError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .ok_or(RenderModelError::MissingField(field))
}

#[cfg(any(test, feature = "semantic-conformance"))]
fn optional_str<'a>(value: &'a Value, field: &'static str) -> Option<&'a str> {
    value.get(field).and_then(Value::as_str)
}

#[cfg(any(test, feature = "semantic-conformance"))]
fn required_u64(value: &Value, field: &'static str) -> Result<u64, RenderModelError> {
    value
        .get(field)
        .and_then(Value::as_u64)
        .ok_or(RenderModelError::MissingField(field))
}

#[cfg(any(test, feature = "semantic-conformance"))]
fn required_string_array(
    value: &Value,
    field: &'static str,
) -> Result<Vec<String>, RenderModelError> {
    let items = value
        .get(field)
        .and_then(Value::as_array)
        .ok_or(RenderModelError::MissingField(field))?;
    items
        .iter()
        .map(|item| {
            let text = item
                .as_str()
                .filter(|text| !text.is_empty())
                .ok_or(RenderModelError::MissingField(field))?;
            if text.chars().count() > MAX_JOURNAL_STRING_SCALARS {
                return Err(RenderModelError::WorkBoundExceeded {
                    kind: field,
                    limit: MAX_JOURNAL_STRING_SCALARS,
                });
            }
            Ok(text.to_string())
        })
        .collect()
}

#[cfg(any(test, feature = "semantic-conformance"))]
fn collect_extension_fields(value: &Value) -> BTreeMap<String, String> {
    const SKIP: &[&str] = &[
        "event_id",
        "task_id",
        "schema_version",
        "provider",
        "source_type",
        "occurred_at_ms",
        "raw_terminal_available",
        "turn_id",
        "related_event_id",
        "kind",
    ];
    let mut fields = BTreeMap::new();
    let Some(object) = value.as_object() else {
        return fields;
    };
    for (key, raw) in object {
        if SKIP.contains(&key.as_str()) {
            continue;
        }
        if is_secret_field(key) || fields.len() >= MAX_GENERIC_FIELDS {
            continue;
        }
        let rendered = match raw {
            Value::String(text) => take_scalars(text, MAX_GENERIC_VALUE_SCALARS),
            Value::Number(number) => number.to_string(),
            Value::Bool(flag) => flag.to_string(),
            Value::Null => continue,
            other => other.to_string(),
        };
        fields.insert(
            take_scalars(key, MAX_GENERIC_KEY_SCALARS),
            take_scalars(&rendered, MAX_GENERIC_VALUE_SCALARS),
        );
    }
    fields
}

pub(crate) fn take_scalars(input: &str, max: usize) -> String {
    input.chars().take(max).collect()
}

pub(crate) fn is_secret_field(key: &str) -> bool {
    let normalized: String = key
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .map(|ch| ch.to_ascii_lowercase())
        .collect();
    [
        "token",
        "password",
        "secret",
        "authorization",
        "apikey",
        "accesskey",
        "accesskeyid",
        "secretaccesskey",
        "credential",
        "bearer",
    ]
    .iter()
    .any(|marker| normalized.contains(marker))
}

pub(crate) fn bound_fields(fields: &BTreeMap<String, String>) -> Vec<(String, String)> {
    fields
        .iter()
        .filter(|(key, _)| !is_secret_field(key))
        .take(MAX_GENERIC_FIELDS)
        .map(|(key, value)| {
            let key = take_scalars(key, MAX_GENERIC_KEY_SCALARS);
            let value = crate::diagnostics::runner::redact_secrets(value);
            (key, take_scalars(&value, MAX_GENERIC_VALUE_SCALARS))
        })
        .collect()
}

pub(crate) fn generic_title(source_type: &str, fields: &BTreeMap<String, String>) -> String {
    let raw = fields
        .get("title")
        .map(String::as_str)
        .filter(|title| !title.trim().is_empty())
        .unwrap_or(source_type);
    let title = take_scalars(raw, MAX_GENERIC_TITLE_SCALARS);
    if title.trim().is_empty() {
        "Unknown event".into()
    } else {
        title
    }
}

pub(crate) const fn max_generic_encoded_bytes() -> usize {
    MAX_GENERIC_ENCODED_BYTES
}
