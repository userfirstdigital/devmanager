use crate::ui::components::interaction::{AccessibilityMetadata, AccessibleRole};

use super::{
    InteractionEligibility, QuestionView, RenderModelError, RendererSelection, SemanticEvent,
    SemanticEventBody, SemanticKind, SemanticRenderer, TimelineItemContent, TimelineItemId,
    TimelineItemModel,
};

pub struct QuestionRenderer;

impl SemanticRenderer for QuestionRenderer {
    fn kind(&self) -> SemanticKind {
        SemanticKind::Question
    }

    fn project(&self, event: &SemanticEvent) -> Result<TimelineItemModel, RenderModelError> {
        let SemanticEventBody::Question {
            request_id,
            prompt,
            choices,
            action_epoch,
            runtime_generation,
            capability,
            settled_choice,
        } = &event.body
        else {
            return Err(RenderModelError::MalformedKnown(SemanticKind::Question));
        };
        if prompt.trim().is_empty() || choices.is_empty() {
            return Err(RenderModelError::MalformedKnown(SemanticKind::Question));
        }
        let interaction = if *capability && settled_choice.is_none() {
            InteractionEligibility::Question
        } else {
            InteractionEligibility::None
        };
        Ok(TimelineItemModel {
            id: TimelineItemId::Event(event.event_id),
            task_id: event.task_id,
            renderer_selection: RendererSelection::Specialized(SemanticKind::Question),
            interaction,
            content: TimelineItemContent::Question(QuestionView {
                request_id: *request_id,
                prompt: prompt.clone(),
                choices: choices.clone(),
                action_epoch: *action_epoch,
                runtime_generation: *runtime_generation,
                capability: *capability,
                settled_choice: *settled_choice,
            }),
            activated_on_enter: false,
            accessibility: AccessibilityMetadata::new(AccessibleRole::Region, prompt.clone())?,
            turn_id: event.turn_id.clone(),
            related_event_id: event.related_event_id,
        })
    }
}
