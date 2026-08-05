use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize};

use crate::domain::canonical;
use crate::domain::id::{AgentSessionId, TaskId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentValidationError {
    EmptyProviderKind,
    EmptyProviderSessionId,
    EmptySpecialistName,
    InvalidRegistrationState,
}

impl std::fmt::Display for AgentValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyProviderKind => write!(f, "provider kind must be non-empty"),
            Self::EmptyProviderSessionId => {
                write!(f, "provider session id must be non-empty when present")
            }
            Self::EmptySpecialistName => write!(f, "specialist role name must be non-empty"),
            Self::InvalidRegistrationState => {
                write!(f, "registered agents must be Open with revision 0")
            }
        }
    }
}

impl std::error::Error for AgentValidationError {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentRole {
    Primary,
    Specialist { name: String },
}

impl AgentRole {
    pub fn specialist(name: impl Into<String>) -> Result<Self, AgentValidationError> {
        let name = canonical::canonicalize(name.into())
            .ok_or(AgentValidationError::EmptySpecialistName)?;
        Ok(Self::Specialist { name })
    }

    pub fn validate(&self) -> Result<(), AgentValidationError> {
        match self {
            Self::Primary => Ok(()),
            Self::Specialist { name } => {
                if canonical::is_canonical(name) {
                    Ok(())
                } else {
                    Err(AgentValidationError::EmptySpecialistName)
                }
            }
        }
    }
}

impl<'de> Deserialize<'de> for AgentRole {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "snake_case")]
        enum AgentRoleWire {
            Primary,
            Specialist { name: String },
        }

        match AgentRoleWire::deserialize(deserializer)? {
            AgentRoleWire::Primary => Ok(Self::Primary),
            AgentRoleWire::Specialist { name } => Self::specialist(name).map_err(de::Error::custom),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentSessionLifecycle {
    Open,
    Closing,
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
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
        let provider_kind = Self::canonicalize_provider_kind(provider_kind)?;
        let provider_session_id = Self::canonicalize_provider_session_id(provider_session_id)?;
        role.validate()?;
        let facts = Self {
            id: AgentSessionId::new(),
            task_id,
            role,
            provider_kind,
            provider_session_id,
            lifecycle: AgentSessionLifecycle::Open,
            runtime_generation: 0,
            revision: 0,
        };
        facts.validate()?;
        Ok(facts)
    }

    pub fn canonicalize_provider_kind(
        value: impl Into<String>,
    ) -> Result<String, AgentValidationError> {
        canonical::canonicalize(value.into()).ok_or(AgentValidationError::EmptyProviderKind)
    }

    pub fn canonicalize_provider_session_id(
        value: Option<String>,
    ) -> Result<Option<String>, AgentValidationError> {
        match value {
            Some(session) => Ok(Some(
                canonical::canonicalize(session)
                    .ok_or(AgentValidationError::EmptyProviderSessionId)?,
            )),
            None => Ok(None),
        }
    }

    pub fn validate(&self) -> Result<(), AgentValidationError> {
        self.role.validate()?;
        if !canonical::is_canonical(&self.provider_kind) {
            return Err(AgentValidationError::EmptyProviderKind);
        }
        if let Some(session) = &self.provider_session_id {
            if !canonical::is_canonical(session) {
                return Err(AgentValidationError::EmptyProviderSessionId);
            }
        }
        Ok(())
    }

    pub fn validate_for_registration(&self) -> Result<(), AgentValidationError> {
        self.validate()?;
        if self.lifecycle != AgentSessionLifecycle::Open || self.revision != 0 {
            return Err(AgentValidationError::InvalidRegistrationState);
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for AgentSessionFacts {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct AgentSessionFactsWire {
            id: AgentSessionId,
            task_id: TaskId,
            role: AgentRole,
            provider_kind: String,
            provider_session_id: Option<String>,
            lifecycle: AgentSessionLifecycle,
            runtime_generation: u64,
            revision: u64,
        }

        let wire = AgentSessionFactsWire::deserialize(deserializer)?;
        let facts = Self {
            id: wire.id,
            task_id: wire.task_id,
            role: wire.role,
            provider_kind: Self::canonicalize_provider_kind(wire.provider_kind)
                .map_err(de::Error::custom)?,
            provider_session_id: Self::canonicalize_provider_session_id(wire.provider_session_id)
                .map_err(de::Error::custom)?,
            lifecycle: wire.lifecycle,
            runtime_generation: wire.runtime_generation,
            revision: wire.revision,
        };
        facts.validate().map_err(de::Error::custom)?;
        Ok(facts)
    }
}
