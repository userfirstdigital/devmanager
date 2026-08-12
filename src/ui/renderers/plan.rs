use crate::ui::components::interaction::{AccessibilityMetadata, AccessibleRole};

use super::{
    InteractionEligibility, RenderModelError, RendererSelection, SemanticEvent, SemanticEventBody,
    SemanticKind, SemanticRenderer, TimelineItemContent, TimelineItemId, TimelineItemModel,
};

pub struct PlanRenderer;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanView {
    pub title: String,
    pub steps: Vec<String>,
    pub status: String,
}

impl SemanticRenderer for PlanRenderer {
    fn kind(&self) -> SemanticKind {
        SemanticKind::Plan
    }

    fn project(&self, event: &SemanticEvent) -> Result<TimelineItemModel, RenderModelError> {
        let SemanticEventBody::Plan {
            title,
            steps,
            status,
        } = &event.body
        else {
            return Err(RenderModelError::MalformedKnown(SemanticKind::Plan));
        };
        if title.trim().is_empty() {
            return Err(RenderModelError::MalformedKnown(SemanticKind::Plan));
        }
        Ok(TimelineItemModel {
            id: TimelineItemId::Event(event.event_id),
            task_id: event.task_id,
            renderer_selection: RendererSelection::Specialized(SemanticKind::Plan),
            interaction: InteractionEligibility::None,
            content: TimelineItemContent::Plan(PlanView {
                title: title.clone(),
                steps: steps.clone(),
                status: status.clone(),
            }),
            activated_on_enter: false,
            accessibility: AccessibilityMetadata::new(AccessibleRole::Region, title.clone())?,
            turn_id: event.turn_id.clone(),
            related_event_id: event.related_event_id,
        })
    }
}
