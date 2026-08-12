use serde_json::json;

use crate::domain::id::EventId;
use crate::ui::components::interaction::{AccessibilityMetadata, AccessibleRole};

use super::{
    bound_fields, generic_title, max_generic_encoded_bytes, take_scalars, InteractionEligibility,
    ProviderKind, RenderModelError, RendererSelection, SemanticEvent, SemanticEventBody,
    TimelineItemContent, TimelineItemId, TimelineItemModel,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GenericStatus {
    Unknown,
    Malformed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenericSemanticCard {
    pub event_id: EventId,
    pub provider: ProviderKind,
    pub source_type: String,
    pub schema_version: u16,
    pub status: GenericStatus,
    pub title: String,
    pub redacted_fields: Vec<(String, String)>,
    pub raw_terminal_available: bool,
}

impl GenericSemanticCard {
    pub fn encoded_len(&self) -> usize {
        self.encoded_payload().len()
    }

    fn encoded_payload(&self) -> Vec<u8> {
        let payload = json!({
            "event_id": self.event_id.to_string(),
            "provider": self.provider.as_str(),
            "source_type": self.source_type,
            "schema_version": self.schema_version,
            "status": match self.status {
                GenericStatus::Unknown => "unknown",
                GenericStatus::Malformed => "malformed",
            },
            "title": self.title,
            "fields": self.redacted_fields,
            "raw_terminal_available": self.raw_terminal_available,
        });
        serde_json::to_vec(&payload).unwrap_or_default()
    }

    fn enforce_encoded_bound(&mut self) {
        // The envelope is provider-neutral too: unknown providers must not
        // bypass the card's total encoded-size bound.
        self.provider = ProviderKind::parse(take_scalars(self.provider.as_str(), 64))
            .expect("bounded provider remains non-empty");
        self.source_type = take_scalars(&self.source_type, 64);
        while self.encoded_len() > max_generic_encoded_bytes() {
            if self.redacted_fields.pop().is_none() {
                self.title = super::take_scalars(&self.title, 32);
                break;
            }
        }
        debug_assert!(self.encoded_len() <= max_generic_encoded_bytes());
    }
}

pub(crate) fn project_generic(
    event: &SemanticEvent,
    status: GenericStatus,
) -> Result<TimelineItemModel, RenderModelError> {
    let fields = match &event.body {
        SemanticEventBody::Extension { fields, .. }
        | SemanticEventBody::Malformed { fields, .. } => fields.clone(),
        _ => Default::default(),
    };
    let title = generic_title(&event.source_type, &fields);
    let mut card = GenericSemanticCard {
        event_id: event.event_id,
        provider: event.provider.clone(),
        source_type: take_scalars(&event.source_type, 64),
        schema_version: event.schema_version,
        status,
        title,
        redacted_fields: bound_fields(&fields),
        raw_terminal_available: event.raw_terminal_available,
    };
    card.enforce_encoded_bound();
    let accessibility = AccessibilityMetadata::new(AccessibleRole::Status, card.title.clone())
        .or_else(|_| AccessibilityMetadata::new(AccessibleRole::Status, "Generic event"))?;
    Ok(TimelineItemModel {
        id: TimelineItemId::Event(event.event_id),
        task_id: event.task_id,
        renderer_selection: RendererSelection::GenericFallback,
        interaction: InteractionEligibility::None,
        content: TimelineItemContent::Generic(card),
        activated_on_enter: false,
        accessibility,
        turn_id: event.turn_id.clone(),
        related_event_id: event.related_event_id,
    })
}
