use std::ops::Deref;

use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSqlOutput, ValueRef};
use rusqlite::ToSql;
use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize};

use crate::domain::canonical;
use crate::domain::id::{AgentSessionId, TaskId};
use crate::providers::ProviderKind;

/// Maximum UTF-8 byte length accepted for an opaque provider-issued session ID.
pub const MAX_PROVIDER_SESSION_ID_BYTES: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderSessionIdError {
    Empty,
    ContainsControlCharacter,
    TooLong,
    NonCanonical,
}

impl std::fmt::Display for ProviderSessionIdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "provider session id must be non-empty"),
            Self::ContainsControlCharacter => {
                write!(f, "provider session id must not contain control characters")
            }
            Self::TooLong => write!(
                f,
                "provider session id exceeds {MAX_PROVIDER_SESSION_ID_BYTES} bytes"
            ),
            Self::NonCanonical => {
                write!(
                    f,
                    "provider session id must not have surrounding whitespace"
                )
            }
        }
    }
}

impl std::error::Error for ProviderSessionIdError {}

/// An exact, provider-issued conversation identity.
///
/// This type intentionally does not trim, normalize, parse, or infer the
/// value. The provider's bytes remain the resume key; only safety bounds are
/// enforced at the boundary.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct ProviderSessionId(String);

impl std::fmt::Debug for ProviderSessionId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProviderSessionId")
            .field("bytes", &self.0.len())
            .finish()
    }
}

impl ProviderSessionId {
    pub fn new(value: impl Into<String>) -> Result<Self, ProviderSessionIdError> {
        let value = value.into();
        if value.is_empty() {
            return Err(ProviderSessionIdError::Empty);
        }
        if value.trim() != value {
            return Err(ProviderSessionIdError::NonCanonical);
        }
        if value.len() > MAX_PROVIDER_SESSION_ID_BYTES {
            return Err(ProviderSessionIdError::TooLong);
        }
        if value.chars().any(is_unsafe_provider_session_character) {
            return Err(ProviderSessionIdError::ContainsControlCharacter);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }

    pub fn into_inner(self) -> String {
        self.0
    }
}

impl Deref for ProviderSessionId {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.as_str()
    }
}

impl AsRef<str> for ProviderSessionId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl std::fmt::Display for ProviderSessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<provider-session:{}-bytes>", self.0.len())
    }
}

impl std::str::FromStr for ProviderSessionId {
    type Err = ProviderSessionIdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl TryFrom<String> for ProviderSessionId {
    type Error = ProviderSessionIdError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl TryFrom<&str> for ProviderSessionId {
    type Error = ProviderSessionIdError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl<'de> Deserialize<'de> for ProviderSessionId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(de::Error::custom)
    }
}

impl ToSql for ProviderSessionId {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        self.validate()
            .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
        self.0.to_sql()
    }
}

impl FromSql for ProviderSessionId {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let value = value.as_str()?;
        Self::new(value.to_owned()).map_err(|error| FromSqlError::Other(Box::new(error)))
    }
}

impl ProviderSessionId {
    fn validate(&self) -> Result<(), ProviderSessionIdError> {
        Self::new(self.0.clone()).map(|_| ())
    }
}

impl AgentSessionFacts {
    fn validate_provider_session_id(
        provider_session_id: Option<ProviderSessionId>,
    ) -> Result<Option<ProviderSessionId>, AgentValidationError> {
        match provider_session_id {
            Some(session) => {
                session
                    .validate()
                    .map_err(|_| AgentValidationError::EmptyProviderSessionId)?;
                if !canonical::is_canonical(session.as_str()) {
                    return Err(AgentValidationError::EmptyProviderSessionId);
                }
                Ok(Some(session))
            }
            None => Ok(None),
        }
    }
}

fn is_unsafe_provider_session_character(character: char) -> bool {
    character.is_control()
        || matches!(
            character,
            '\u{200b}'
                | '\u{200c}'
                | '\u{200d}'
                | '\u{200e}'
                | '\u{200f}'
                | '\u{202a}'..='\u{202e}'
                | '\u{2060}'
                | '\u{2061}'..='\u{2064}'
                | '\u{2066}'..='\u{2069}'
                | '\u{feff}'
        )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentValidationError {
    EmptyProviderKind,
    UnsupportedProviderKind,
    NonCanonicalProviderKind,
    EmptyProviderSessionId,
    EmptySpecialistName,
    InvalidRegistrationState,
}

impl std::fmt::Display for AgentValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyProviderKind => write!(f, "provider kind must be non-empty"),
            Self::UnsupportedProviderKind => write!(f, "provider kind is not supported"),
            Self::NonCanonicalProviderKind => write!(f, "provider kind is not canonical"),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum SpecialistPermission {
    ReadOnly,
    IsolatedWrite,
    SharedWrite { explicit_approval: bool },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentRole {
    Primary,
    Specialist { name: String },
}

impl AgentRole {
    pub fn specialist(name: impl Into<String>) -> Result<Self, AgentValidationError> {
        let name = canonical::bounded_canonical(&name.into())
            .ok_or(AgentValidationError::EmptySpecialistName)?;
        Ok(Self::Specialist { name })
    }

    pub fn validate(&self) -> Result<(), AgentValidationError> {
        match self {
            Self::Primary => Ok(()),
            Self::Specialist { name } => {
                if canonical::is_bounded_canonical(name) {
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
        #[serde(rename_all = "snake_case", deny_unknown_fields)]
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
    pub provider_kind: ProviderKind,
    pub provider_session_id: Option<ProviderSessionId>,
    pub lifecycle: AgentSessionLifecycle,
    pub runtime_generation: u64,
    pub revision: u64,
}

impl AgentSessionFacts {
    pub fn new(
        task_id: TaskId,
        role: AgentRole,
        provider_kind: ProviderKind,
        provider_session_id: Option<ProviderSessionId>,
    ) -> Result<Self, AgentValidationError> {
        let provider_session_id = Self::validate_provider_session_id(provider_session_id)?;
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
    ) -> Result<ProviderKind, AgentValidationError> {
        let value = value.into();
        if value.is_empty() {
            return Err(AgentValidationError::EmptyProviderKind);
        }
        ProviderKind::parse_wire(&value).ok_or_else(|| {
            if value == "claude_code" {
                AgentValidationError::NonCanonicalProviderKind
            } else {
                AgentValidationError::UnsupportedProviderKind
            }
        })
    }

    pub fn canonicalize_provider_session_id(
        value: Option<String>,
    ) -> Result<Option<ProviderSessionId>, AgentValidationError> {
        match value {
            Some(session) => {
                Ok(Some(ProviderSessionId::new(session).map_err(|_| {
                    AgentValidationError::EmptyProviderSessionId
                })?))
            }
            None => Ok(None),
        }
    }

    pub fn validate(&self) -> Result<(), AgentValidationError> {
        self.role.validate()?;
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
        #[serde(deny_unknown_fields)]
        struct AgentSessionFactsWire {
            id: AgentSessionId,
            task_id: TaskId,
            role: AgentRole,
            provider_kind: ProviderKind,
            provider_session_id: Option<ProviderSessionId>,
            lifecycle: AgentSessionLifecycle,
            runtime_generation: u64,
            revision: u64,
        }

        let wire = AgentSessionFactsWire::deserialize(deserializer)?;
        let facts = Self {
            id: wire.id,
            task_id: wire.task_id,
            role: wire.role,
            provider_kind: wire.provider_kind,
            provider_session_id: Self::validate_provider_session_id(wire.provider_session_id)
                .map_err(de::Error::custom)?,
            lifecycle: wire.lifecycle,
            runtime_generation: wire.runtime_generation,
            revision: wire.revision,
        };
        facts.validate().map_err(de::Error::custom)?;
        Ok(facts)
    }
}
