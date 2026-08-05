use serde::{Deserialize, Serialize};

use crate::domain::id::{AgentSessionId, TaskId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentValidationError {
    EmptyProviderKind,
    EmptyProviderSessionId,
    EmptySpecialistName,
}

impl std::fmt::Display for AgentValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyProviderKind => write!(f, "provider kind must be non-empty"),
            Self::EmptyProviderSessionId => {
                write!(f, "provider session id must be non-empty when present")
            }
            Self::EmptySpecialistName => write!(f, "specialist role name must be non-empty"),
        }
    }
}

impl std::error::Error for AgentValidationError {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentRole {
    Primary,
    Specialist { name: String },
}

impl AgentRole {
    pub fn specialist(name: impl Into<String>) -> Result<Self, AgentValidationError> {
        let name = name.into();
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return Err(AgentValidationError::EmptySpecialistName);
        }
        Ok(Self::Specialist {
            name: trimmed.to_string(),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentSessionLifecycle {
    Open,
    Closing,
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSessionFacts {
    pub id: AgentSessionId,
    pub task_id: TaskId,
    pub role: AgentRole,
    pub provider_kind: String,
    pub provider_session_id: Option<String>,
    pub lifecycle: AgentSessionLifecycle,
    pub runtime_generation: u64,
    pub revision: u64,
}

impl AgentSessionFacts {
    pub fn new(
        task_id: TaskId,
        role: AgentRole,
        provider_kind: impl Into<String>,
        provider_session_id: Option<String>,
    ) -> Result<Self, AgentValidationError> {
        let provider_kind = {
            let value = provider_kind.into();
            let trimmed = value.trim();
            if trimmed.is_empty() {
                return Err(AgentValidationError::EmptyProviderKind);
            }
            trimmed.to_string()
        };
        let provider_session_id = match provider_session_id {
            Some(value) => {
                let trimmed = value.trim();
                if trimmed.is_empty() {
                    return Err(AgentValidationError::EmptyProviderSessionId);
                }
                Some(trimmed.to_string())
            }
            None => None,
        };

        Ok(Self {
            id: AgentSessionId::new(),
            task_id,
            role,
            provider_kind,
            provider_session_id,
            lifecycle: AgentSessionLifecycle::Open,
            runtime_generation: 0,
            revision: 0,
        })
    }
}
