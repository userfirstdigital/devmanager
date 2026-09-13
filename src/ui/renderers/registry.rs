use std::collections::HashMap;

use super::{
    generic::project_generic, AgentRenderer, ApprovalRenderer, ArtifactRenderer, GenericStatus,
    MessageRenderer, OperationRenderer, PlanRenderer, QuestionRenderer, RenderModelError,
    RendererSelection, SemanticEvent, SemanticKind, TimelineItemModel, ToolRenderer,
};

pub trait SemanticRenderer: Send + Sync {
    fn kind(&self) -> SemanticKind;
    fn project(&self, event: &SemanticEvent) -> Result<TimelineItemModel, RenderModelError>;
}

pub struct RendererRegistry {
    order: Vec<SemanticKind>,
    renderers: HashMap<SemanticKind, Box<dyn SemanticRenderer>>,
}

impl RendererRegistry {
    pub(crate) fn new() -> Self {
        Self {
            order: Vec::new(),
            renderers: HashMap::new(),
        }
    }

    pub fn standard() -> Result<Self, RenderModelError> {
        let mut registry = Self::new();
        registry.register(Box::new(MessageRenderer))?;
        registry.register(Box::new(ToolRenderer))?;
        registry.register(Box::new(QuestionRenderer))?;
        registry.register(Box::new(ApprovalRenderer))?;
        registry.register(Box::new(OperationRenderer))?;
        registry.register(Box::new(PlanRenderer))?;
        registry.register(Box::new(ArtifactRenderer))?;
        registry.register(Box::new(AgentRenderer))?;
        Ok(registry)
    }

    pub fn register(
        &mut self,
        renderer: Box<dyn SemanticRenderer>,
    ) -> Result<(), RenderModelError> {
        let kind = renderer.kind();
        if self.renderers.contains_key(&kind) {
            return Err(RenderModelError::DuplicateKind(kind));
        }
        self.order.push(kind);
        self.renderers.insert(kind, renderer);
        Ok(())
    }

    pub fn registered_kinds(&self) -> Vec<SemanticKind> {
        self.order.clone()
    }

    pub fn project(&self, event: &SemanticEvent) -> Result<TimelineItemModel, RenderModelError> {
        match event.specialized_kind() {
            Some(kind) => match self.renderers.get(&kind) {
                Some(renderer) => match renderer.project(event) {
                    Ok(item) => Ok(item),
                    Err(_) => project_generic(event, GenericStatus::Malformed),
                },
                None => project_generic(event, GenericStatus::Unknown),
            },
            None => {
                let status = event.generic_status().unwrap_or(GenericStatus::Unknown);
                project_generic(event, status)
            }
        }
    }

    pub fn selection_for(&self, event: &SemanticEvent) -> RendererSelection {
        match event.specialized_kind() {
            Some(kind) if self.renderers.contains_key(&kind) => {
                RendererSelection::Specialized(kind)
            }
            _ => RendererSelection::GenericFallback,
        }
    }
}
