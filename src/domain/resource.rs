use serde::{Deserialize, Serialize};

use crate::domain::id::{ResourceId, TaskId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceValidationError {
    InvalidTerminalGeometry,
    EmptyRecipe,
}

impl std::fmt::Display for ResourceValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidTerminalGeometry => {
                write!(f, "terminal cols and rows must be greater than zero")
            }
            Self::EmptyRecipe => write!(f, "resource recipe must be non-empty"),
        }
    }
}

impl std::error::Error for ResourceValidationError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OwnerKind {
    Task,
    Host,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceKind {
    Terminal,
    BrowserContext,
    Service,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceLifecycle {
    Active,
    Releasing,
    Released,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceRecipe {
    Terminal { cols: u16, rows: u16 },
    Browser { start_url: String },
    Service { command: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceFacts {
    pub id: ResourceId,
    pub task_id: Option<TaskId>,
    pub owner_kind: OwnerKind,
    pub resource_kind: ResourceKind,
    pub recipe: ResourceRecipe,
    pub lifecycle: ResourceLifecycle,
    pub runtime_generation: u64,
    pub updated_at_ms: i64,
}

impl ResourceFacts {
    pub fn new(
        task_id: Option<TaskId>,
        owner_kind: OwnerKind,
        resource_kind: ResourceKind,
        recipe: ResourceRecipe,
        updated_at_ms: i64,
    ) -> Result<Self, ResourceValidationError> {
        match &recipe {
            ResourceRecipe::Terminal { cols, rows } if *cols == 0 || *rows == 0 => {
                return Err(ResourceValidationError::InvalidTerminalGeometry);
            }
            ResourceRecipe::Browser { start_url } if start_url.trim().is_empty() => {
                return Err(ResourceValidationError::EmptyRecipe);
            }
            ResourceRecipe::Service { command } if command.trim().is_empty() => {
                return Err(ResourceValidationError::EmptyRecipe);
            }
            _ => {}
        }

        Ok(Self {
            id: ResourceId::new(),
            task_id,
            owner_kind,
            resource_kind,
            recipe,
            lifecycle: ResourceLifecycle::Active,
            runtime_generation: 0,
            updated_at_ms,
        })
    }
}
