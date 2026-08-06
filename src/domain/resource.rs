use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize};

use crate::domain::canonical;
use crate::domain::id::{ResourceId, TaskId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceValidationError {
    InvalidTerminalGeometry,
    EmptyRecipe,
    OwnerBinding,
    KindRecipeMismatch,
    InvalidRegistrationLifecycle,
}

impl std::fmt::Display for ResourceValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidTerminalGeometry => {
                write!(f, "terminal cols and rows must be greater than zero")
            }
            Self::EmptyRecipe => write!(f, "resource recipe must be non-empty"),
            Self::OwnerBinding => {
                write!(f, "Task owner requires task_id; Host owner requires None")
            }
            Self::KindRecipeMismatch => {
                write!(f, "resource_kind must match recipe variant")
            }
            Self::InvalidRegistrationLifecycle => {
                write!(f, "registered resources must start Active")
            }
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceRecipe {
    Terminal { cols: u16, rows: u16 },
    Browser { start_url: String },
    Service { command: String },
}

impl ResourceRecipe {
    pub fn browser(start_url: impl Into<String>) -> Result<Self, ResourceValidationError> {
        let start_url = canonical::canonicalize(start_url.into())
            .ok_or(ResourceValidationError::EmptyRecipe)?;
        Ok(Self::Browser { start_url })
    }

    pub fn service(command: impl Into<String>) -> Result<Self, ResourceValidationError> {
        let command =
            canonical::canonicalize(command.into()).ok_or(ResourceValidationError::EmptyRecipe)?;
        Ok(Self::Service { command })
    }

    pub fn canonicalize(self) -> Result<Self, ResourceValidationError> {
        match self {
            Self::Terminal { cols, rows } => {
                if cols == 0 || rows == 0 {
                    return Err(ResourceValidationError::InvalidTerminalGeometry);
                }
                Ok(Self::Terminal { cols, rows })
            }
            Self::Browser { start_url } => Self::browser(start_url),
            Self::Service { command } => Self::service(command),
        }
    }

    pub fn validate(&self) -> Result<(), ResourceValidationError> {
        match self {
            Self::Terminal { cols, rows } => {
                if *cols == 0 || *rows == 0 {
                    Err(ResourceValidationError::InvalidTerminalGeometry)
                } else {
                    Ok(())
                }
            }
            Self::Browser { start_url } => {
                if canonical::is_canonical(start_url) {
                    Ok(())
                } else {
                    Err(ResourceValidationError::EmptyRecipe)
                }
            }
            Self::Service { command } => {
                if canonical::is_canonical(command) {
                    Ok(())
                } else {
                    Err(ResourceValidationError::EmptyRecipe)
                }
            }
        }
    }
}

impl<'de> Deserialize<'de> for ResourceRecipe {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "snake_case", deny_unknown_fields)]
        enum ResourceRecipeWire {
            Terminal { cols: u16, rows: u16 },
            Browser { start_url: String },
            Service { command: String },
        }

        match ResourceRecipeWire::deserialize(deserializer)? {
            ResourceRecipeWire::Terminal { cols, rows } => Self::Terminal { cols, rows }
                .canonicalize()
                .map_err(de::Error::custom),
            ResourceRecipeWire::Browser { start_url } => {
                Self::browser(start_url).map_err(de::Error::custom)
            }
            ResourceRecipeWire::Service { command } => {
                Self::service(command).map_err(de::Error::custom)
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
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
        let recipe = recipe.canonicalize()?;
        let facts = Self {
            id: ResourceId::new(),
            task_id,
            owner_kind,
            resource_kind,
            recipe,
            lifecycle: ResourceLifecycle::Active,
            runtime_generation: 0,
            updated_at_ms,
        };
        facts.validate()?;
        Ok(facts)
    }

    pub fn validate(&self) -> Result<(), ResourceValidationError> {
        match (self.owner_kind, self.task_id) {
            (OwnerKind::Task, Some(_)) | (OwnerKind::Host, None) => {}
            _ => return Err(ResourceValidationError::OwnerBinding),
        }
        match (&self.resource_kind, &self.recipe) {
            (ResourceKind::Terminal, ResourceRecipe::Terminal { .. })
            | (ResourceKind::BrowserContext, ResourceRecipe::Browser { .. })
            | (ResourceKind::Service, ResourceRecipe::Service { .. }) => self.recipe.validate()?,
            _ => return Err(ResourceValidationError::KindRecipeMismatch),
        }
        Ok(())
    }

    pub fn validate_for_registration(&self) -> Result<(), ResourceValidationError> {
        self.validate()?;
        if self.owner_kind != OwnerKind::Task {
            return Err(ResourceValidationError::OwnerBinding);
        }
        if self.lifecycle != ResourceLifecycle::Active {
            return Err(ResourceValidationError::InvalidRegistrationLifecycle);
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for ResourceFacts {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct ResourceFactsWire {
            id: ResourceId,
            task_id: Option<TaskId>,
            owner_kind: OwnerKind,
            resource_kind: ResourceKind,
            recipe: ResourceRecipe,
            lifecycle: ResourceLifecycle,
            runtime_generation: u64,
            updated_at_ms: i64,
        }

        let wire = ResourceFactsWire::deserialize(deserializer)?;
        let facts = Self {
            id: wire.id,
            task_id: wire.task_id,
            owner_kind: wire.owner_kind,
            resource_kind: wire.resource_kind,
            recipe: wire.recipe,
            lifecycle: wire.lifecycle,
            runtime_generation: wire.runtime_generation,
            updated_at_ms: wire.updated_at_ms,
        };
        facts.validate().map_err(de::Error::custom)?;
        Ok(facts)
    }
}
