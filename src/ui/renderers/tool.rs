use crate::ui::components::interaction::{AccessibilityMetadata, AccessibleRole};

use super::{
    InteractionEligibility, RenderModelError, RendererSelection, SemanticEvent, SemanticEventBody,
    SemanticKind, SemanticRenderer, TimelineItemContent, TimelineItemId, TimelineItemModel,
};

pub struct ToolRenderer;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolView {
    pub tool_id: String,
    pub name: String,
    pub state: String,
    pub summary: String,
    pub provider_specific: bool,
}

impl SemanticRenderer for ToolRenderer {
    fn kind(&self) -> SemanticKind {
        SemanticKind::Tool
    }

    fn project(&self, event: &SemanticEvent) -> Result<TimelineItemModel, RenderModelError> {
        let SemanticEventBody::Tool {
            tool_id,
            name,
            state,
            summary,
        } = &event.body
        else {
            return Err(RenderModelError::MalformedKnown(SemanticKind::Tool));
        };
        if tool_id.trim().is_empty() || name.trim().is_empty() {
            return Err(RenderModelError::MalformedKnown(SemanticKind::Tool));
        }
        Ok(TimelineItemModel {
            id: TimelineItemId::Event(event.event_id),
            task_id: event.task_id,
            renderer_selection: RendererSelection::Specialized(SemanticKind::Tool),
            interaction: InteractionEligibility::None,
            content: TimelineItemContent::Tool(ToolView {
                tool_id: tool_id.clone(),
                name: name.clone(),
                state: state.clone(),
                summary: summary.clone(),
                provider_specific: false,
            }),
            activated_on_enter: false,
            accessibility: AccessibilityMetadata::new(AccessibleRole::Status, name.clone())?,
            turn_id: event.turn_id.clone(),
            related_event_id: event.related_event_id,
        })
    }
}
