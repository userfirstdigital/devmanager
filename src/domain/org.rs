//! Durable local Task scope. This is not a BoardCard row and is not inferred
//! from Connect sign-in.

use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize};

use crate::domain::canonical;

/// One local Task is Personal unless the owner explicitly enrolls it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskScope {
    Personal,
    Managed(ManagedScope),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ManagedScope {
    pub link_id: String,
    pub policy_revision: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskScopeError {
    EmptyLinkId,
    ZeroPolicyRevision,
}

impl std::fmt::Display for TaskScopeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyLinkId => write!(f, "managed link id must be non-empty"),
            Self::ZeroPolicyRevision => write!(f, "managed policy revision must be nonzero"),
        }
    }
}

impl std::error::Error for TaskScopeError {}

impl TaskScope {
    pub const fn personal() -> Self {
        Self::Personal
    }

    pub fn managed(
        link_id: impl Into<String>,
        policy_revision: u32,
    ) -> Result<Self, TaskScopeError> {
        Ok(Self::Managed(ManagedScope::new(link_id, policy_revision)?))
    }

    pub const fn is_personal(&self) -> bool {
        matches!(self, Self::Personal)
    }

    pub const fn is_managed(&self) -> bool {
        matches!(self, Self::Managed(_))
    }
}

impl Default for TaskScope {
    fn default() -> Self {
        Self::Personal
    }
}

impl ManagedScope {
    pub fn new(link_id: impl Into<String>, policy_revision: u32) -> Result<Self, TaskScopeError> {
        let link_id = canonical::canonicalize(link_id.into()).ok_or(TaskScopeError::EmptyLinkId)?;
        if policy_revision == 0 {
            return Err(TaskScopeError::ZeroPolicyRevision);
        }
        Ok(Self {
            link_id,
            policy_revision,
        })
    }
}

impl<'de> Deserialize<'de> for TaskScope {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(rename_all = "snake_case", deny_unknown_fields)]
        enum TaskScopeWire {
            Personal,
            Managed {
                link_id: String,
                policy_revision: u32,
            },
        }

        match TaskScopeWire::deserialize(deserializer)? {
            TaskScopeWire::Personal => Ok(Self::Personal),
            TaskScopeWire::Managed {
                link_id,
                policy_revision,
            } => Self::managed(link_id, policy_revision).map_err(de::Error::custom),
        }
    }
}
