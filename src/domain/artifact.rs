use std::fmt;

use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::domain::canonical;
use crate::domain::id::{ArtifactId, TaskId};
use crate::domain::task::WorkspaceRef;

pub const MAX_SPECIALIST_RAW_ARTIFACT_BYTES: usize = 64 * 1024;
pub use crate::domain::canonical::{MAX_SPECIALIST_ID_REFS, MAX_SPECIALIST_TEXT_BYTES};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactValidationError {
    EmptyLabel,
    EmptyContent,
    ContentDigestMismatch,
    InvalidSpecialistResult,
}

impl std::fmt::Display for ArtifactValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyLabel => write!(f, "artifact label must be non-empty"),
            Self::EmptyContent => write!(f, "artifact content must be non-empty"),
            Self::ContentDigestMismatch => {
                write!(
                    f,
                    "artifact inline content SHA-256 does not match declared digest"
                )
            }
            Self::InvalidSpecialistResult => {
                write!(f, "structured specialist result failed validation")
            }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpecialistStatus {
    Completed,
    Failed,
    Cancelled,
}

#[derive(Clone, PartialEq, Eq)]
pub struct SpecialistResult {
    pub role: String,
    pub status: SpecialistStatus,
    pub summary: String,
    pub evidence: Vec<ArtifactId>,
    pub artifacts: Vec<ArtifactId>,
    pub workspace: Option<WorkspaceRef>,
    pub commit: Option<String>,
    pub requested_follow_up: Option<String>,
}

impl fmt::Debug for SpecialistResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SpecialistResult")
            .field("role", &self.role)
            .field("status", &self.status)
            .field(
                "summary",
                &format_args!("<redacted {} bytes>", self.summary.len()),
            )
            .field("evidence_count", &self.evidence.len())
            .field("artifact_count", &self.artifacts.len())
            .field("workspace", &self.workspace.as_ref().map(|_| "<redacted>"))
            .field("commit", &self.commit)
            .field(
                "requested_follow_up",
                &self
                    .requested_follow_up
                    .as_ref()
                    .map(|text| format!("<redacted {} bytes>", text.len())),
            )
            .finish()
    }
}

impl Serialize for SpecialistResult {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.validate().map_err(serde::ser::Error::custom)?;
        #[derive(Serialize)]
        struct SpecialistResultWire<'a> {
            role: &'a str,
            status: SpecialistStatus,
            summary: &'a str,
            evidence: &'a [ArtifactId],
            artifacts: &'a [ArtifactId],
            workspace: &'a Option<WorkspaceRef>,
            commit: &'a Option<String>,
            requested_follow_up: &'a Option<String>,
        }
        SpecialistResultWire {
            role: &self.role,
            status: self.status,
            summary: &self.summary,
            evidence: &self.evidence,
            artifacts: &self.artifacts,
            workspace: &self.workspace,
            commit: &self.commit,
            requested_follow_up: &self.requested_follow_up,
        }
        .serialize(serializer)
    }
}

impl SpecialistResult {
    pub fn validate(&self) -> Result<(), ArtifactValidationError> {
        if canonical::bounded_canonical(&self.role).is_none()
            || canonical::bounded_canonical(&self.summary).is_none()
        {
            return Err(ArtifactValidationError::InvalidSpecialistResult);
        }
        if !canonical::specialist_id_refs_ok(self.evidence.len())
            || !canonical::specialist_id_refs_ok(self.artifacts.len())
        {
            return Err(ArtifactValidationError::InvalidSpecialistResult);
        }
        if let Some(commit) = &self.commit {
            let len = commit.len();
            if !(len == 40 || len == 64) || !commit.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err(ArtifactValidationError::InvalidSpecialistResult);
            }
        }
        if let Some(follow_up) = &self.requested_follow_up {
            if canonical::bounded_canonical(follow_up).is_none() {
                return Err(ArtifactValidationError::InvalidSpecialistResult);
            }
        }
        if let Some(workspace) = &self.workspace {
            workspace
                .validate()
                .map_err(|_| ArtifactValidationError::InvalidSpecialistResult)?;
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for SpecialistResult {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct SpecialistResultWire {
            role: String,
            status: SpecialistStatus,
            summary: String,
            evidence: Vec<ArtifactId>,
            artifacts: Vec<ArtifactId>,
            workspace: Option<WorkspaceRef>,
            commit: Option<String>,
            requested_follow_up: Option<String>,
        }

        let wire = SpecialistResultWire::deserialize(deserializer)?;
        let result = Self {
            role: wire.role,
            status: wire.status,
            summary: wire.summary,
            evidence: wire.evidence,
            artifacts: wire.artifacts,
            workspace: wire.workspace,
            commit: wire.commit,
            requested_follow_up: wire.requested_follow_up,
        };
        result.validate().map_err(de::Error::custom)?;
        Ok(result)
    }
}

pub fn structured_specialist_result(
    artifact: &ArtifactFacts,
) -> Result<SpecialistResult, ArtifactValidationError> {
    let ArtifactContentRef::InlineUtf8(body) = &artifact.content_ref else {
        return Err(ArtifactValidationError::InvalidSpecialistResult);
    };
    if body.len() > MAX_SPECIALIST_RAW_ARTIFACT_BYTES {
        return Err(ArtifactValidationError::InvalidSpecialistResult);
    }
    serde_json::from_str(body).map_err(|_| ArtifactValidationError::InvalidSpecialistResult)
}

#[derive(Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactContentRef {
    InlineUtf8(String),
    ContentAddressed { digest_hex: String },
}

impl fmt::Debug for ArtifactContentRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InlineUtf8(body) => formatter
                .debug_struct("InlineUtf8")
                .field("bytes", &body.len())
                .finish(),
            Self::ContentAddressed { digest_hex } => formatter
                .debug_struct("ContentAddressed")
                .field("digest_hex", digest_hex)
                .finish(),
        }
    }
}

impl ArtifactContentRef {
    pub fn inline_utf8(body: impl Into<String>) -> Result<Self, ArtifactValidationError> {
        let body = body.into();
        // Inline bodies are opaque content: reject empty, never trim.
        if body.is_empty() {
            return Err(ArtifactValidationError::EmptyContent);
        }
        Ok(Self::InlineUtf8(body))
    }

    pub fn content_addressed(
        digest_hex: impl Into<String>,
    ) -> Result<Self, ArtifactValidationError> {
        let digest_hex = canonical::canonicalize(digest_hex.into())
            .ok_or(ArtifactValidationError::EmptyContent)?;
        Ok(Self::ContentAddressed { digest_hex })
    }

    pub fn canonicalize(self) -> Result<Self, ArtifactValidationError> {
        match self {
            Self::InlineUtf8(body) => Self::inline_utf8(body),
            Self::ContentAddressed { digest_hex } => Self::content_addressed(digest_hex),
        }
    }

    pub fn validate(&self) -> Result<(), ArtifactValidationError> {
        match self {
            Self::InlineUtf8(body) if body.is_empty() => Err(ArtifactValidationError::EmptyContent),
            Self::InlineUtf8(_) => Ok(()),
            Self::ContentAddressed { digest_hex } => {
                if canonical::is_canonical(digest_hex) {
                    Ok(())
                } else {
                    Err(ArtifactValidationError::EmptyContent)
                }
            }
        }
    }
}

impl<'de> Deserialize<'de> for ArtifactContentRef {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "snake_case", deny_unknown_fields)]
        enum ArtifactContentRefWire {
            InlineUtf8(String),
            ContentAddressed { digest_hex: String },
        }

        match ArtifactContentRefWire::deserialize(deserializer)? {
            ArtifactContentRefWire::InlineUtf8(body) => {
                Self::inline_utf8(body).map_err(de::Error::custom)
            }
            ArtifactContentRefWire::ContentAddressed { digest_hex } => {
                Self::content_addressed(digest_hex).map_err(de::Error::custom)
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
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
        let label = Self::canonicalize_label(label)?;
        let content_ref = content_ref.canonicalize()?;
        let facts = Self {
            id: ArtifactId::new(),
            task_id,
            kind,
            label,
            content_ref,
            sha256,
            privacy_class,
            created_at_ms,
        };
        facts.validate()?;
        Ok(facts)
    }

    pub fn canonicalize_label(label: impl Into<String>) -> Result<String, ArtifactValidationError> {
        canonical::canonicalize(label.into()).ok_or(ArtifactValidationError::EmptyLabel)
    }

    pub fn validate(&self) -> Result<(), ArtifactValidationError> {
        if !canonical::is_canonical(&self.label) {
            return Err(ArtifactValidationError::EmptyLabel);
        }
        self.content_ref.validate()
    }
}

impl<'de> Deserialize<'de> for ArtifactFacts {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct ArtifactFactsWire {
            id: ArtifactId,
            task_id: TaskId,
            kind: ArtifactKind,
            label: String,
            content_ref: ArtifactContentRef,
            sha256: [u8; 32],
            privacy_class: PrivacyClass,
            created_at_ms: i64,
        }

        let wire = ArtifactFactsWire::deserialize(deserializer)?;
        let facts = Self {
            id: wire.id,
            task_id: wire.task_id,
            kind: wire.kind,
            label: Self::canonicalize_label(wire.label).map_err(de::Error::custom)?,
            content_ref: wire.content_ref,
            sha256: wire.sha256,
            privacy_class: wire.privacy_class,
            created_at_ms: wire.created_at_ms,
        };
        facts.validate().map_err(de::Error::custom)?;
        Ok(facts)
    }
}

/// Client snapshot metadata for one artifact. Never carries body bytes or a
/// content-ref; SHA-256 is the content identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ArtifactSummary {
    pub id: ArtifactId,
    pub task_id: TaskId,
    pub kind: ArtifactKind,
    pub label: String,
    pub sha256: [u8; 32],
    pub privacy_class: PrivacyClass,
    pub created_at_ms: i64,
}

impl ArtifactSummary {
    pub fn from_facts(facts: &ArtifactFacts) -> Result<Self, ArtifactValidationError> {
        facts.validate()?;
        verify_inline_content_digest(facts)?;
        Ok(Self {
            id: facts.id,
            task_id: facts.task_id,
            kind: facts.kind,
            label: facts.label.clone(),
            sha256: facts.sha256,
            privacy_class: facts.privacy_class,
            created_at_ms: facts.created_at_ms,
        })
    }
}

/// Recompute SHA-256 for locally available InlineUtf8 bodies. ContentAddressed
/// bytes are not required here and are left unchecked.
pub fn verify_inline_content_digest(facts: &ArtifactFacts) -> Result<(), ArtifactValidationError> {
    match &facts.content_ref {
        ArtifactContentRef::InlineUtf8(body) => {
            let mut hasher = Sha256::new();
            hasher.update(body.as_bytes());
            let computed: [u8; 32] = hasher.finalize().into();
            if computed != facts.sha256 {
                return Err(ArtifactValidationError::ContentDigestMismatch);
            }
            Ok(())
        }
        ArtifactContentRef::ContentAddressed { .. } => Ok(()),
    }
}

impl<'de> Deserialize<'de> for ArtifactSummary {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct ArtifactSummaryWire {
            id: ArtifactId,
            task_id: TaskId,
            kind: ArtifactKind,
            label: String,
            sha256: [u8; 32],
            privacy_class: PrivacyClass,
            created_at_ms: i64,
        }

        let wire = ArtifactSummaryWire::deserialize(deserializer)?;
        let label = ArtifactFacts::canonicalize_label(wire.label).map_err(de::Error::custom)?;
        Ok(Self {
            id: wire.id,
            task_id: wire.task_id,
            kind: wire.kind,
            label,
            sha256: wire.sha256,
            privacy_class: wire.privacy_class,
            created_at_ms: wire.created_at_ms,
        })
    }
}
