use crate::domain::id::OperationId;
use crate::ui::components::interaction::{AccessibilityMetadata, AccessibleRole};

use super::{
    InteractionEligibility, RenderModelError, RendererSelection, SemanticEvent, SemanticEventBody,
    SemanticKind, SemanticRenderer, TimelineItemContent, TimelineItemId, TimelineItemModel,
};

pub struct OperationRenderer;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationRenderState {
    Pending,
    Success,
    Failure,
    Cancelled,
    Uncertain,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationView {
    pub operation_id: OperationId,
    pub state: OperationRenderState,
    pub effect_evidence: Option<String>,
    pub needs_me: bool,
    pub inspect_available: bool,
    pub reconcile_available: bool,
    pub retry_warns_prior_effect_may_have_happened: bool,
    pub new_command_id_required: bool,
    pub implicit_resend: bool,
}

impl SemanticRenderer for OperationRenderer {
    fn kind(&self) -> SemanticKind {
        SemanticKind::Operation
    }

    fn project(&self, event: &SemanticEvent) -> Result<TimelineItemModel, RenderModelError> {
        let SemanticEventBody::Operation {
            operation_id,
            state,
            effect_evidence,
            ..
        } = &event.body
        else {
            return Err(RenderModelError::MalformedKnown(SemanticKind::Operation));
        };
        let state =
            parse_state(state).ok_or(RenderModelError::MalformedKnown(SemanticKind::Operation))?;
        let uncertain = state == OperationRenderState::Uncertain;
        let name = match state {
            OperationRenderState::Uncertain => "Operation needs review",
            OperationRenderState::Pending => "Operation pending",
            OperationRenderState::Success => "Operation succeeded",
            OperationRenderState::Failure => "Operation failed",
            OperationRenderState::Cancelled => "Operation cancelled",
        };
        Ok(TimelineItemModel {
            id: TimelineItemId::Event(event.event_id),
            task_id: event.task_id,
            renderer_selection: RendererSelection::Specialized(SemanticKind::Operation),
            interaction: if uncertain {
                InteractionEligibility::NeedsMeWarning
            } else {
                InteractionEligibility::None
            },
            content: TimelineItemContent::Operation(OperationView {
                operation_id: *operation_id,
                state,
                effect_evidence: effect_evidence.clone(),
                needs_me: uncertain,
                inspect_available: uncertain,
                reconcile_available: uncertain,
                retry_warns_prior_effect_may_have_happened: uncertain,
                new_command_id_required: uncertain,
                implicit_resend: false,
            }),
            activated_on_enter: false,
            accessibility: AccessibilityMetadata::new(
                if uncertain {
                    AccessibleRole::Alert
                } else {
                    AccessibleRole::Status
                },
                name,
            )?,
            turn_id: event.turn_id.clone(),
            related_event_id: event.related_event_id,
        })
    }
}

#[allow(dead_code)]
pub(crate) fn project_operation_state(
    state: &crate::domain::operation::OperationState,
) -> OperationRenderState {
    use crate::domain::operation::OperationState;
    match state {
        OperationState::Accepted => OperationRenderState::Pending,
        OperationState::Settled { .. } => OperationRenderState::Success,
        OperationState::Failed { .. } => OperationRenderState::Failure,
        OperationState::Cancelled { .. } => OperationRenderState::Cancelled,
        OperationState::Uncertain { .. } => OperationRenderState::Uncertain,
    }
}

fn parse_state(state: &str) -> Option<OperationRenderState> {
    Some(match state {
        "pending" => OperationRenderState::Pending,
        "success" => OperationRenderState::Success,
        "failure" => OperationRenderState::Failure,
        "cancelled" => OperationRenderState::Cancelled,
        "uncertain" => OperationRenderState::Uncertain,
        _ => return None,
    })
}
