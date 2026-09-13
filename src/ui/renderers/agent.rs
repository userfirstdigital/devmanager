use crate::ui::components::interaction::{AccessibilityMetadata, AccessibleRole};

use super::{
    AgentView, InteractionEligibility, RenderModelError, RendererSelection, SemanticEvent,
    SemanticEventBody, SemanticKind, SemanticRenderer, TimelineItemContent, TimelineItemId,
    TimelineItemModel,
};

pub struct AgentRenderer;

impl SemanticRenderer for AgentRenderer {
    fn kind(&self) -> SemanticKind {
        SemanticKind::Agent
    }

    fn project(&self, event: &SemanticEvent) -> Result<TimelineItemModel, RenderModelError> {
        let SemanticEventBody::Agent {
            agent_session_id,
            role,
            specialist_name,
            parent_agent_session_id,
        } = &event.body
        else {
            return Err(RenderModelError::MalformedKnown(SemanticKind::Agent));
        };
        if role.trim().is_empty() {
            return Err(RenderModelError::MalformedKnown(SemanticKind::Agent));
        }
        let name = specialist_name
            .clone()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| role.clone());
        Ok(TimelineItemModel {
            id: TimelineItemId::Event(event.event_id),
            task_id: event.task_id,
            renderer_selection: RendererSelection::Specialized(SemanticKind::Agent),
            interaction: InteractionEligibility::None,
            content: TimelineItemContent::Agent(AgentView {
                agent_session_id: *agent_session_id,
                role: role.clone(),
                specialist_name: specialist_name.clone(),
                parent_agent_session_id: *parent_agent_session_id,
            }),
            activated_on_enter: false,
            accessibility: AccessibilityMetadata::new(AccessibleRole::Status, name)?,
            turn_id: event.turn_id.clone(),
            related_event_id: event.related_event_id,
        })
    }
}
