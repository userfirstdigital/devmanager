use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::domain::id::{CommandId, PromptChainId, PromptChainLinkId, PromptId, PromptVersionId};

pub const MAX_PROMPT_TITLE_SCALARS: usize = 160;
pub const MAX_PROMPT_DESCRIPTION_SCALARS: usize = 2_000;
pub const MAX_PROMPT_BODY_BYTES: usize = 256 * 1024;
pub const MAX_PROMPT_TAGS: usize = 32;
pub const MAX_PROMPT_TAG_SCALARS: usize = 48;
pub const MAX_PROMPT_VARIABLES: usize = 32;
pub const MAX_PROMPT_VARIABLE_NAME_SCALARS: usize = 64;
pub const MAX_PROMPT_CHAIN_TITLE_SCALARS: usize = MAX_PROMPT_TITLE_SCALARS;
pub const MAX_PROMPT_CHAIN_DESCRIPTION_SCALARS: usize = MAX_PROMPT_DESCRIPTION_SCALARS;
pub const DEFAULT_PROMPT_PAGE_SIZE: usize = 100;
pub const MAX_PROMPT_PAGE_SIZE: usize = 1_000;
pub const PROMPT_WIRE_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptCodecError(String);

impl fmt::Display for PromptCodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PromptCodecError {}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct PromptCommandWire<'a> {
    schema_version: u32,
    command: &'a PromptCommand,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PromptCommandWireOwned {
    schema_version: u32,
    command: PromptCommand,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PromptEventWire<T> {
    schema_version: u32,
    event: T,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PromptChainCommandWire<T> {
    schema_version: u32,
    command: T,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PromptChainEventWire<T> {
    schema_version: u32,
    event: T,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptValidationError {
    EmptyTitle,
    TitleTooLong {
        actual: usize,
        max: usize,
    },
    DescriptionTooLong {
        actual: usize,
        max: usize,
    },
    BodyTooLarge {
        actual: usize,
        max: usize,
    },
    TooManyTags {
        actual: usize,
        max: usize,
    },
    EmptyTag {
        position: usize,
    },
    TagTooLong {
        position: usize,
        actual: usize,
        max: usize,
    },
    TooManyVariables {
        actual: usize,
        max: usize,
    },
    EmptyVariable {
        position: usize,
    },
    VariableTooLong {
        position: usize,
        actual: usize,
        max: usize,
    },
}

impl fmt::Display for PromptValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyTitle => f.write_str("prompt title must not be empty"),
            Self::TitleTooLong { actual, max } => {
                write!(
                    f,
                    "prompt title exceeds {max} Unicode scalar values ({actual})"
                )
            }
            Self::DescriptionTooLong { actual, max } => write!(
                f,
                "prompt description exceeds {max} Unicode scalar values ({actual})"
            ),
            Self::BodyTooLarge { actual, max } => {
                write!(f, "prompt body exceeds {max} UTF-8 bytes ({actual})")
            }
            Self::TooManyTags { actual, max } => {
                write!(f, "prompt has more than {max} normalized tags ({actual})")
            }
            Self::EmptyTag { position } => write!(f, "prompt tag at position {position} is empty"),
            Self::TagTooLong {
                position,
                actual,
                max,
            } => write!(
                f,
                "prompt tag at position {position} exceeds {max} Unicode scalar values ({actual})"
            ),
            Self::TooManyVariables { actual, max } => {
                write!(f, "prompt has more than {max} variables ({actual})")
            }
            Self::EmptyVariable { position } => {
                write!(f, "prompt variable at position {position} is empty")
            }
            Self::VariableTooLong {
                position,
                actual,
                max,
            } => write!(
                f,
                "prompt variable at position {position} exceeds {max} Unicode scalar values ({actual})"
            ),
        }
    }
}

impl std::error::Error for PromptValidationError {}

pub fn normalized_tags(tags: &[String]) -> Result<Vec<String>, PromptValidationError> {
    let mut normalized = Vec::with_capacity(tags.len());
    for (position, tag) in tags.iter().enumerate() {
        let tag = tag.trim().to_lowercase();
        if tag.is_empty() {
            return Err(PromptValidationError::EmptyTag { position });
        }
        let actual = tag.chars().count();
        if actual > MAX_PROMPT_TAG_SCALARS {
            return Err(PromptValidationError::TagTooLong {
                position,
                actual,
                max: MAX_PROMPT_TAG_SCALARS,
            });
        }
        if !normalized.iter().any(|existing| existing == &tag) {
            normalized.push(tag);
        }
    }
    if normalized.len() > MAX_PROMPT_TAGS {
        return Err(PromptValidationError::TooManyTags {
            actual: normalized.len(),
            max: MAX_PROMPT_TAGS,
        });
    }
    Ok(normalized)
}

pub fn normalized_variables(variables: &[String]) -> Result<Vec<String>, PromptValidationError> {
    let mut normalized = Vec::with_capacity(variables.len());
    for (position, variable) in variables.iter().enumerate() {
        let variable = variable.trim().to_string();
        if variable.is_empty() {
            return Err(PromptValidationError::EmptyVariable { position });
        }
        let actual = variable.chars().count();
        if actual > MAX_PROMPT_VARIABLE_NAME_SCALARS {
            return Err(PromptValidationError::VariableTooLong {
                position,
                actual,
                max: MAX_PROMPT_VARIABLE_NAME_SCALARS,
            });
        }
        if !normalized.iter().any(|existing| existing == &variable) {
            normalized.push(variable);
        }
    }
    if normalized.len() > MAX_PROMPT_VARIABLES {
        return Err(PromptValidationError::TooManyVariables {
            actual: normalized.len(),
            max: MAX_PROMPT_VARIABLES,
        });
    }
    Ok(normalized)
}

fn validate_title(title: &str) -> Result<(), PromptValidationError> {
    let actual = title.chars().count();
    if title.trim().is_empty() {
        return Err(PromptValidationError::EmptyTitle);
    }
    if actual > MAX_PROMPT_TITLE_SCALARS {
        return Err(PromptValidationError::TitleTooLong {
            actual,
            max: MAX_PROMPT_TITLE_SCALARS,
        });
    }
    Ok(())
}

fn validate_description(description: Option<&str>) -> Result<(), PromptValidationError> {
    if let Some(description) = description {
        let actual = description.chars().count();
        if actual > MAX_PROMPT_DESCRIPTION_SCALARS {
            return Err(PromptValidationError::DescriptionTooLong {
                actual,
                max: MAX_PROMPT_DESCRIPTION_SCALARS,
            });
        }
    }
    Ok(())
}

pub fn validate_body(body: &str) -> Result<(), PromptValidationError> {
    let actual = body.len();
    if actual > MAX_PROMPT_BODY_BYTES {
        return Err(PromptValidationError::BodyTooLarge {
            actual,
            max: MAX_PROMPT_BODY_BYTES,
        });
    }
    Ok(())
}

fn validate_common(
    title: &str,
    description: Option<&str>,
    tags: &[String],
    variables: &[String],
) -> Result<Vec<String>, PromptValidationError> {
    validate_title(title)?;
    validate_description(description)?;
    let tags = normalized_tags(tags)?;
    normalized_variables(variables)?;
    Ok(tags)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedPrompt {
    pub id: PromptId,
    pub title: String,
    pub description: Option<String>,
    pub tags: Vec<String>,
    pub current_version_id: PromptVersionId,
    pub revision: u64,
    pub archived_at_ms: Option<i64>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptVersion {
    pub id: PromptVersionId,
    pub prompt_id: PromptId,
    pub version: u32,
    pub body: String,
    pub variables: Vec<String>,
    pub body_sha256: [u8; 32],
    pub created_at_ms: i64,
}

impl PromptVersion {
    pub fn new(
        id: PromptVersionId,
        prompt_id: PromptId,
        version: u32,
        body: String,
        created_at_ms: i64,
    ) -> Result<Self, PromptValidationError> {
        Self::new_with_variables(id, prompt_id, version, body, Vec::new(), created_at_ms)
    }

    pub fn new_with_variables(
        id: PromptVersionId,
        prompt_id: PromptId,
        version: u32,
        body: String,
        variables: Vec<String>,
        created_at_ms: i64,
    ) -> Result<Self, PromptValidationError> {
        validate_body(&body)?;
        let variables = normalized_variables(&variables)?;
        let body_sha256: [u8; 32] = Sha256::digest(body.as_bytes()).into();
        Ok(Self {
            id,
            prompt_id,
            version,
            body,
            variables,
            body_sha256,
            created_at_ms,
        })
    }
}

struct RedactedBody(usize);

impl fmt::Debug for RedactedBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<redacted prompt body: {} bytes>", self.0)
    }
}

impl fmt::Debug for PromptVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PromptVersion")
            .field("id", &self.id)
            .field("prompt_id", &self.prompt_id)
            .field("version", &self.version)
            .field("body", &RedactedBody(self.body.len()))
            .field("variables", &self.variables)
            .field("body_sha256", &self.body_sha256)
            .field("created_at_ms", &self.created_at_ms)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreatePrompt {
    pub prompt_id: PromptId,
    pub prompt_version_id: PromptVersionId,
    pub title: String,
    pub description: Option<String>,
    pub tags: Vec<String>,
    pub variables: Vec<String>,
    pub body: String,
    pub created_at_ms: i64,
}

impl CreatePrompt {
    pub fn validate(&self) -> Result<(), PromptValidationError> {
        validate_common(
            &self.title,
            self.description.as_deref(),
            &self.tags,
            &self.variables,
        )?;
        validate_body(&self.body)
    }

    pub fn normalized_tags(&self) -> Result<Vec<String>, PromptValidationError> {
        self.validate()?;
        normalized_tags(&self.tags)
    }

    pub fn normalized_variables(&self) -> Result<Vec<String>, PromptValidationError> {
        self.validate()?;
        normalized_variables(&self.variables)
    }
}

impl fmt::Debug for CreatePrompt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CreatePrompt")
            .field("prompt_id", &self.prompt_id)
            .field("prompt_version_id", &self.prompt_version_id)
            .field("title", &self.title)
            .field("description", &self.description)
            .field("tags", &self.tags)
            .field("variables", &self.variables)
            .field("body", &RedactedBody(self.body.len()))
            .field("created_at_ms", &self.created_at_ms)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreatePromptVersion {
    pub prompt_id: PromptId,
    pub prompt_version_id: PromptVersionId,
    pub variables: Vec<String>,
    pub body: String,
    pub created_at_ms: i64,
    pub expected_revision: u64,
}

impl CreatePromptVersion {
    pub fn validate(&self) -> Result<(), PromptValidationError> {
        normalized_variables(&self.variables)?;
        validate_body(&self.body)
    }

    pub fn normalized_variables(&self) -> Result<Vec<String>, PromptValidationError> {
        self.validate()?;
        normalized_variables(&self.variables)
    }
}

impl fmt::Debug for CreatePromptVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CreatePromptVersion")
            .field("prompt_id", &self.prompt_id)
            .field("prompt_version_id", &self.prompt_version_id)
            .field("variables", &self.variables)
            .field("body", &RedactedBody(self.body.len()))
            .field("created_at_ms", &self.created_at_ms)
            .field("expected_revision", &self.expected_revision)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenamePrompt {
    pub prompt_id: PromptId,
    pub title: String,
    pub expected_revision: u64,
}

impl RenamePrompt {
    pub fn validate(&self) -> Result<(), PromptValidationError> {
        validate_title(&self.title)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetPromptTags {
    pub prompt_id: PromptId,
    pub tags: Vec<String>,
    pub expected_revision: u64,
}

impl SetPromptTags {
    pub fn validate(&self) -> Result<Vec<String>, PromptValidationError> {
        normalized_tags(&self.tags)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchivePrompt {
    pub prompt_id: PromptId,
    pub archived_at_ms: i64,
    pub expected_revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestorePrompt {
    pub prompt_id: PromptId,
    pub expected_revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum PromptCommand {
    CreatePrompt(CreatePrompt),
    CreatePromptVersion(CreatePromptVersion),
    RenamePrompt(RenamePrompt),
    SetPromptTags(SetPromptTags),
    ArchivePrompt(ArchivePrompt),
    RestorePrompt(RestorePrompt),
}

impl PromptCommand {
    pub fn validate(&self) -> Result<(), PromptValidationError> {
        match self {
            Self::CreatePrompt(command) => command.validate(),
            Self::CreatePromptVersion(command) => command.validate(),
            Self::RenamePrompt(command) => command.validate(),
            Self::SetPromptTags(command) => command.validate().map(|_| ()),
            Self::ArchivePrompt(_) | Self::RestorePrompt(_) => Ok(()),
        }
    }

    pub fn fingerprint(&self) -> [u8; 32] {
        let encoded = self.encode().expect("prompt commands are serializable");
        Sha256::digest(encoded).into()
    }

    pub fn encode(&self) -> Result<Vec<u8>, PromptCodecError> {
        rmp_serde::to_vec_named(&PromptCommandWire {
            schema_version: PROMPT_WIRE_SCHEMA_VERSION,
            command: self,
        })
        .map_err(|_| PromptCodecError("prompt command encoding failed".into()))
    }

    pub fn decode(payload: &[u8]) -> Result<Self, PromptCodecError> {
        let wire: PromptCommandWireOwned = rmp_serde::from_slice(payload)
            .map_err(|_| PromptCodecError("prompt command decoding failed".into()))?;
        if wire.schema_version != PROMPT_WIRE_SCHEMA_VERSION {
            return Err(PromptCodecError("unsupported prompt command schema".into()));
        }
        Ok(wire.command)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptMutationReceipt {
    pub command_id: CommandId,
    pub prompt_id: PromptId,
    pub prompt_version_id: PromptVersionId,
    pub revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum PromptEvent {
    PromptCreated {
        prompt: SavedPrompt,
        version: PromptVersion,
    },
    PromptVersionCreated {
        prompt_id: PromptId,
        version: PromptVersion,
        revision: u64,
    },
    PromptRenamed {
        prompt_id: PromptId,
        title: String,
        revision: u64,
    },
    PromptTagsSet {
        prompt_id: PromptId,
        tags: Vec<String>,
        revision: u64,
    },
    PromptArchived {
        prompt_id: PromptId,
        archived_at_ms: i64,
        revision: u64,
    },
    PromptRestored {
        prompt_id: PromptId,
        revision: u64,
    },
}

impl PromptEvent {
    pub fn event_type(&self) -> &'static str {
        match self {
            Self::PromptCreated { .. } => "prompt.created",
            Self::PromptVersionCreated { .. } => "prompt.version_created",
            Self::PromptRenamed { .. } => "prompt.renamed",
            Self::PromptTagsSet { .. } => "prompt.tags_set",
            Self::PromptArchived { .. } => "prompt.archived",
            Self::PromptRestored { .. } => "prompt.restored",
        }
    }

    pub fn encode(&self) -> Result<Vec<u8>, PromptCodecError> {
        rmp_serde::to_vec_named(&PromptEventWire {
            schema_version: PROMPT_WIRE_SCHEMA_VERSION,
            event: self,
        })
        .map_err(|_| PromptCodecError("prompt event encoding failed".into()))
    }

    pub fn decode(payload: &[u8]) -> Result<Self, PromptCodecError> {
        let wire: PromptEventWire<PromptEvent> = rmp_serde::from_slice(payload)
            .map_err(|_| PromptCodecError("prompt event decoding failed".into()))?;
        if wire.schema_version != PROMPT_WIRE_SCHEMA_VERSION {
            return Err(PromptCodecError("unsupported prompt event schema".into()));
        }
        Ok(wire.event)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptSnapshot {
    pub prompts: Vec<SavedPrompt>,
    pub next_offset: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptProjectionRebuild {
    pub events_replayed: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptChain {
    pub id: PromptChainId,
    pub title: String,
    pub description: Option<String>,
    pub revision: u64,
    pub archived_at_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptChainLink {
    pub id: PromptChainLinkId,
    pub chain_id: PromptChainId,
    pub position: u32,
    pub prompt_id: PromptId,
    pub prompt_version_id: PromptVersionId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptChainLinkContext {
    pub link: PromptChainLink,
    pub previous_link_id: Option<PromptChainLinkId>,
    pub next_link_id: Option<PromptChainLinkId>,
    pub update_available: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreatePromptChain {
    pub chain_id: PromptChainId,
    pub title: String,
    pub description: Option<String>,
    pub created_at_ms: i64,
}

impl CreatePromptChain {
    pub fn validate(&self) -> Result<(), PromptValidationError> {
        validate_title_with_limit(
            &self.title,
            MAX_PROMPT_CHAIN_TITLE_SCALARS,
            |actual, max| PromptValidationError::TitleTooLong { actual, max },
        )?;
        validate_description_with_limit(
            self.description.as_deref(),
            MAX_PROMPT_CHAIN_DESCRIPTION_SCALARS,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenamePromptChain {
    pub chain_id: PromptChainId,
    pub title: String,
    pub expected_revision: u64,
}

impl RenamePromptChain {
    pub fn validate(&self) -> Result<(), PromptValidationError> {
        validate_title_with_limit(
            &self.title,
            MAX_PROMPT_CHAIN_TITLE_SCALARS,
            |actual, max| PromptValidationError::TitleTooLong { actual, max },
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InsertPromptChainLink {
    pub chain_id: PromptChainId,
    pub link_id: PromptChainLinkId,
    pub prompt_id: PromptId,
    /// `None` selects the prompt's current version at insertion time. The
    /// stored link always contains the resolved immutable version ID.
    pub prompt_version_id: Option<PromptVersionId>,
    pub before_link_id: Option<PromptChainLinkId>,
    pub expected_revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MovePromptChainLink {
    pub chain_id: PromptChainId,
    pub link_id: PromptChainLinkId,
    pub before_link_id: Option<PromptChainLinkId>,
    pub expected_revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemovePromptChainLink {
    pub chain_id: PromptChainId,
    pub link_id: PromptChainLinkId,
    pub expected_revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdatePromptChainLinkVersion {
    pub chain_id: PromptChainId,
    pub link_id: PromptChainLinkId,
    pub expected_revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchivePromptChain {
    pub chain_id: PromptChainId,
    pub archived_at_ms: i64,
    pub expected_revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestorePromptChain {
    pub chain_id: PromptChainId,
    pub expected_revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum PromptChainCommand {
    CreatePromptChain(CreatePromptChain),
    RenamePromptChain(RenamePromptChain),
    InsertPromptChainLink(InsertPromptChainLink),
    MovePromptChainLink(MovePromptChainLink),
    RemovePromptChainLink(RemovePromptChainLink),
    UpdatePromptChainLinkVersion(UpdatePromptChainLinkVersion),
    ArchivePromptChain(ArchivePromptChain),
    RestorePromptChain(RestorePromptChain),
}

impl PromptChainCommand {
    pub fn validate(&self) -> Result<(), PromptValidationError> {
        match self {
            Self::CreatePromptChain(command) => command.validate(),
            Self::RenamePromptChain(command) => command.validate(),
            Self::InsertPromptChainLink(_)
            | Self::MovePromptChainLink(_)
            | Self::RemovePromptChainLink(_)
            | Self::UpdatePromptChainLinkVersion(_)
            | Self::ArchivePromptChain(_)
            | Self::RestorePromptChain(_) => Ok(()),
        }
    }

    pub fn fingerprint(&self) -> [u8; 32] {
        let encoded = rmp_serde::to_vec_named(&PromptChainCommandWire {
            schema_version: PROMPT_WIRE_SCHEMA_VERSION,
            command: self,
        })
        .expect("prompt chain commands are serializable");
        Sha256::digest(encoded).into()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptChainMutationReceipt {
    pub command_id: CommandId,
    pub chain_id: PromptChainId,
    pub link_id: Option<PromptChainLinkId>,
    pub revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum PromptChainEvent {
    PromptChainCreated {
        chain: PromptChain,
    },
    PromptChainRenamed {
        chain_id: PromptChainId,
        title: String,
        revision: u64,
    },
    PromptChainLinksReplaced {
        chain_id: PromptChainId,
        links: Vec<PromptChainLink>,
        revision: u64,
    },
    PromptChainArchived {
        chain_id: PromptChainId,
        archived_at_ms: i64,
        revision: u64,
    },
    PromptChainRestored {
        chain_id: PromptChainId,
        revision: u64,
    },
}

impl PromptChainEvent {
    pub fn event_type(&self) -> &'static str {
        match self {
            Self::PromptChainCreated { .. } => "prompt_chain.created",
            Self::PromptChainRenamed { .. } => "prompt_chain.renamed",
            Self::PromptChainLinksReplaced { .. } => "prompt_chain.links_replaced",
            Self::PromptChainArchived { .. } => "prompt_chain.archived",
            Self::PromptChainRestored { .. } => "prompt_chain.restored",
        }
    }

    pub fn encode(&self) -> Result<Vec<u8>, PromptCodecError> {
        rmp_serde::to_vec_named(&PromptChainEventWire {
            schema_version: PROMPT_WIRE_SCHEMA_VERSION,
            event: self,
        })
        .map_err(|_| PromptCodecError("prompt chain event encoding failed".into()))
    }

    pub fn decode(payload: &[u8]) -> Result<Self, PromptCodecError> {
        let wire: PromptChainEventWire<PromptChainEvent> = rmp_serde::from_slice(payload)
            .map_err(|_| PromptCodecError("prompt chain event decoding failed".into()))?;
        if wire.schema_version != PROMPT_WIRE_SCHEMA_VERSION {
            return Err(PromptCodecError(
                "unsupported prompt chain event schema".into(),
            ));
        }
        Ok(wire.event)
    }
}

fn validate_title_with_limit<F>(
    title: &str,
    max: usize,
    error: F,
) -> Result<(), PromptValidationError>
where
    F: FnOnce(usize, usize) -> PromptValidationError,
{
    let actual = title.chars().count();
    if title.trim().is_empty() {
        return Err(PromptValidationError::EmptyTitle);
    }
    if actual > max {
        return Err(error(actual, max));
    }
    Ok(())
}

fn validate_description_with_limit(
    description: Option<&str>,
    max: usize,
) -> Result<(), PromptValidationError> {
    if let Some(description) = description {
        let actual = description.chars().count();
        if actual > max {
            return Err(PromptValidationError::DescriptionTooLong { actual, max });
        }
    }
    Ok(())
}
