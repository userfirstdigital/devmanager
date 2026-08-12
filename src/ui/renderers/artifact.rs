use crate::ui::components::interaction::{AccessibilityMetadata, AccessibleRole};

use super::{
    ArtifactView, InteractionEligibility, RenderModelError, RendererSelection, SemanticEvent,
    SemanticEventBody, SemanticKind, SemanticRenderer, TimelineItemContent, TimelineItemId,
    TimelineItemModel,
};

pub struct ArtifactRenderer;

impl SemanticRenderer for ArtifactRenderer {
    fn kind(&self) -> SemanticKind {
        SemanticKind::Artifact
    }

    fn project(&self, event: &SemanticEvent) -> Result<TimelineItemModel, RenderModelError> {
        let SemanticEventBody::Artifact {
            artifact_id,
            label,
            kind,
        } = &event.body
        else {
            return Err(RenderModelError::MalformedKnown(SemanticKind::Artifact));
        };
        if label.trim().is_empty() {
            return Err(RenderModelError::MalformedKnown(SemanticKind::Artifact));
        }
        Ok(TimelineItemModel {
            id: TimelineItemId::Event(event.event_id),
            task_id: event.task_id,
            renderer_selection: RendererSelection::Specialized(SemanticKind::Artifact),
            interaction: InteractionEligibility::None,
            content: TimelineItemContent::Artifact(ArtifactView {
                artifact_id: *artifact_id,
                label: label.clone(),
                kind: kind.clone(),
            }),
            activated_on_enter: false,
            accessibility: AccessibilityMetadata::new(AccessibleRole::Status, label.clone())?,
            turn_id: event.turn_id.clone(),
            related_event_id: event.related_event_id,
        })
    }
}
