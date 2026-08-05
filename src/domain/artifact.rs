use serde::{Deserialize, Serialize};

use crate::domain::id::{ArtifactId, TaskId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactValidationError {
    EmptyLabel,
    EmptyContent,
}

impl std::fmt::Display for ArtifactValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyLabel => write!(f, "artifact label must be non-empty"),
            Self::EmptyContent => write!(f, "artifact content must be non-empty"),
        }
    }
}

impl std::error::Error for ArtifactValidationError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    Specification,
    Finding,
    Decision,
    Evidence,
    ReviewReport,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrivacyClass {
    LocalOnly,
    Shareable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactContentRef {
    InlineUtf8(String),
    ContentAddressed { digest_hex: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactFacts {
    pub id: ArtifactId,
    pub task_id: TaskId,
    pub kind: ArtifactKind,
    pub label: String,
    pub content_ref: ArtifactContentRef,
    pub sha256: [u8; 32],
    pub privacy_class: PrivacyClass,
    pub created_at_ms: i64,
}

impl ArtifactFacts {
    pub fn new(
        task_id: TaskId,
        kind: ArtifactKind,
        label: impl Into<String>,
        content_ref: ArtifactContentRef,
        sha256: [u8; 32],
        privacy_class: PrivacyClass,
        created_at_ms: i64,
    ) -> Result<Self, ArtifactValidationError> {
        let label = {
            let value = label.into();
            let trimmed = value.trim();
            if trimmed.is_empty() {
                return Err(ArtifactValidationError::EmptyLabel);
            }
            trimmed.to_string()
        };
        match &content_ref {
            ArtifactContentRef::InlineUtf8(body) if body.is_empty() => {
                return Err(ArtifactValidationError::EmptyContent);
            }
            ArtifactContentRef::ContentAddressed { digest_hex } if digest_hex.trim().is_empty() => {
                return Err(ArtifactValidationError::EmptyContent);
            }
            _ => {}
        }

        Ok(Self {
            id: ArtifactId::new(),
            task_id,
            kind,
            label,
            content_ref,
            sha256,
            privacy_class,
            created_at_ms,
        })
    }
}
