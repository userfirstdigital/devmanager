//! Shared test fixtures for building `TimelineItemModel`s.
//!
//! Both `src/ui/task_cockpit/timeline.rs` and `src/ui/conversation/rows.rs`
//! import these instead of each defining their own; two near-identical
//! fixture sets would drift.

use crate::domain::id::{EventId, TaskId};
use crate::ui::components::interaction::{AccessibilityMetadata, AccessibleRole};
use crate::ui::renderers::{
    GenericSemanticCard, GenericStatus, InteractionEligibility, MarkdownBlock, MarkdownDocument,
    MessageRole, MessageView, PlanView, ProviderKind, RendererSelection, SemanticKind,
    TimelineItemContent, TimelineItemId, TimelineItemModel, ToolView,
};

fn role_label(role_kind: MessageRole) -> &'static str {
    match role_kind {
        MessageRole::User => "You",
        MessageRole::Assistant => "Assistant",
        MessageRole::Reasoning => "Reasoning",
        MessageRole::Error => "Error",
    }
}

pub(crate) fn message_item(role_kind: MessageRole, text: &str) -> TimelineItemModel {
    let role = role_label(role_kind);
    TimelineItemModel {
        id: TimelineItemId::Event(EventId::new()),
        task_id: TaskId::new(),
        renderer_selection: RendererSelection::Specialized(SemanticKind::Message),
        interaction: InteractionEligibility::None,
        content: TimelineItemContent::Message(MessageView {
            role: role.to_string(),
            role_kind,
            occurred_at_ms: None,
            streaming: false,
            markdown: MarkdownDocument {
                selectable: true,
                copyable: true,
                html_executed: false,
                prose_wraps: true,
                blocks: vec![MarkdownBlock::Paragraph {
                    text: text.to_string(),
                }],
                pending_links: Vec::new(),
            },
        }),
        activated_on_enter: false,
        accessibility: AccessibilityMetadata::new(AccessibleRole::Region, role)
            .expect("accessible name"),
        turn_id: None,
        related_event_id: None,
    }
}

pub(crate) fn tool_item(tool_id: &str, name: &str, state: &str) -> TimelineItemModel {
    TimelineItemModel {
        id: TimelineItemId::Event(EventId::new()),
        task_id: TaskId::new(),
        renderer_selection: RendererSelection::Specialized(SemanticKind::Tool),
        interaction: InteractionEligibility::None,
        content: TimelineItemContent::Tool(ToolView {
            tool_id: tool_id.to_string(),
            name: name.to_string(),
            state: state.to_string(),
            summary: String::new(),
            provider_specific: false,
        }),
        activated_on_enter: false,
        accessibility: AccessibilityMetadata::new(AccessibleRole::Status, name)
            .expect("accessible name"),
        turn_id: None,
        related_event_id: None,
    }
}

pub(crate) fn plan_item(step_id: &str, title: &str, status: &str) -> TimelineItemModel {
    TimelineItemModel {
        id: TimelineItemId::Event(EventId::new()),
        task_id: TaskId::new(),
        renderer_selection: RendererSelection::Specialized(SemanticKind::Plan),
        interaction: InteractionEligibility::None,
        content: TimelineItemContent::Plan(PlanView {
            step_id: Some(step_id.to_string()),
            title: title.to_string(),
            steps: vec![title.to_string()],
            status: status.to_string(),
        }),
        activated_on_enter: false,
        accessibility: AccessibilityMetadata::new(AccessibleRole::Status, title)
            .expect("accessible name"),
        turn_id: None,
        related_event_id: None,
    }
}

pub(crate) fn generic_item(source_type: &str) -> TimelineItemModel {
    let event_id = EventId::new();
    TimelineItemModel {
        id: TimelineItemId::Event(event_id),
        task_id: TaskId::new(),
        renderer_selection: RendererSelection::GenericFallback,
        interaction: InteractionEligibility::None,
        content: TimelineItemContent::Generic(GenericSemanticCard {
            event_id,
            provider: ProviderKind::parse("claude").expect("provider"),
            source_type: source_type.to_string(),
            schema_version: 1,
            status: GenericStatus::Unknown,
            title: source_type.to_string(),
            redacted_fields: Vec::new(),
            raw_terminal_available: false,
        }),
        activated_on_enter: false,
        accessibility: AccessibilityMetadata::new(AccessibleRole::Status, source_type)
            .expect("accessible name"),
        turn_id: None,
        related_event_id: None,
    }
}
