use crate::ui::components::interaction::{AccessibilityMetadata, AccessibleRole};

use super::{
    ApprovalView, InteractionEligibility, RenderModelError, RendererSelection, SemanticEvent,
    SemanticEventBody, SemanticKind, SemanticRenderer, TimelineItemContent, TimelineItemId,
    TimelineItemModel,
};

pub struct ApprovalRenderer;

impl SemanticRenderer for ApprovalRenderer {
    fn kind(&self) -> SemanticKind {
        SemanticKind::Approval
    }

    fn project(&self, event: &SemanticEvent) -> Result<TimelineItemModel, RenderModelError> {
        let SemanticEventBody::Approval {
            request_id,
            summary,
            action_epoch,
            runtime_generation,
            capability,
            settled,
        } = &event.body
        else {
            return Err(RenderModelError::MalformedKnown(SemanticKind::Approval));
        };
        if summary.trim().is_empty() {
            return Err(RenderModelError::MalformedKnown(SemanticKind::Approval));
        }
        let interaction = if *capability && !*settled {
            InteractionEligibility::Approval
        } else {
            InteractionEligibility::None
        };
        Ok(TimelineItemModel {
            id: TimelineItemId::Event(event.event_id),
            task_id: event.task_id,
            renderer_selection: RendererSelection::Specialized(SemanticKind::Approval),
            interaction,
            content: TimelineItemContent::Approval(ApprovalView {
                request_id: *request_id,
                summary: summary.clone(),
                action_epoch: *action_epoch,
                runtime_generation: *runtime_generation,
                capability: *capability,
                settled: *settled,
            }),
            activated_on_enter: false,
            accessibility: AccessibilityMetadata::new(AccessibleRole::Region, summary.clone())?,
            turn_id: event.turn_id.clone(),
            related_event_id: event.related_event_id,
        })
    }
}
