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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SavedPromptWire {
    id: PromptId,
    title: String,
    description: Option<String>,
    tags: Vec<String>,
    current_version_id: PromptVersionId,
    revision: u64,
    archived_at_ms: Option<i64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PromptVersionWire {
    id: PromptVersionId,
    prompt_id: PromptId,
    version: u32,
    body: String,
    variables: Vec<String>,
    body_sha256: [u8; 32],
    created_at_ms: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreatePromptWire {
    prompt_id: PromptId,
    prompt_version_id: PromptVersionId,
    title: String,
    description: Option<String>,
    tags: Vec<String>,
    variables: Vec<String>,
    body: String,
    created_at_ms: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreatePromptVersionWire {
    prompt_id: PromptId,
    prompt_version_id: PromptVersionId,
    variables: Vec<String>,
    body: String,
    created_at_ms: i64,
    expected_revision: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RenamePromptWire {
    prompt_id: PromptId,
    title: String,
    expected_revision: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SetPromptTagsWire {
    prompt_id: PromptId,
    tags: Vec<String>,
    expected_revision: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PromptChainWire {
    id: PromptChainId,
    title: String,
    description: Option<String>,
    revision: u64,
    archived_at_ms: Option<i64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreatePromptChainWire {
    chain_id: PromptChainId,
    title: String,
    description: Option<String>,
    created_at_ms: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RenamePromptChainWire {
    chain_id: PromptChainId,
    title: String,
    expected_revision: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum PromptEventSerde {
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

#[derive(Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum PromptChainEventSerde {
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PromptMutationReceiptWire {
    command_id: CommandId,
    prompt_id: PromptId,
    prompt_version_id: PromptVersionId,
    revision: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PromptChainMutationReceiptWire {
    command_id: CommandId,
    chain_id: PromptChainId,
    link_id: Option<PromptChainLinkId>,
    revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptValidationError {
    ExpectedRevisionZero,
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
            Self::ExpectedRevisionZero => f.write_str("expected prompt revision must be positive"),
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

fn deserialize_positive_revision<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let revision = u64::deserialize(deserializer)?;
    if revision == 0 {
        return Err(serde::de::Error::custom(
            PromptValidationError::ExpectedRevisionZero,
        ));
    }
    Ok(revision)
}

pub fn normalized_tags(tags: &[String]) -> Result<Vec<String>, PromptValidationError> {
    let mut normalized = Vec::with_capacity(tags.len());
    for (position, tag) in tags.iter().enumerate() {
        let tag = trim_prompt_whitespace(tag).to_lowercase();
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
        let variable = trim_prompt_whitespace(variable).to_string();
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
    if trim_prompt_whitespace(title).is_empty() {
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
#[serde(deny_unknown_fields, try_from = "SavedPromptWire")]
pub struct SavedPrompt {
    pub id: PromptId,
    pub title: String,
    pub description: Option<String>,
    pub tags: Vec<String>,
    pub current_version_id: PromptVersionId,
    pub revision: u64,
    pub archived_at_ms: Option<i64>,
}

impl TryFrom<SavedPromptWire> for SavedPrompt {
    type Error = String;

    fn try_from(wire: SavedPromptWire) -> Result<Self, Self::Error> {
        validate_saved_prompt_wire(&wire)?;
        Ok(Self {
            id: wire.id,
            title: wire.title,
            description: wire.description,
            tags: wire.tags,
            current_version_id: wire.current_version_id,
            revision: wire.revision,
            archived_at_ms: wire.archived_at_ms,
        })
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, try_from = "PromptVersionWire")]
pub struct PromptVersion {
    pub id: PromptVersionId,
    pub prompt_id: PromptId,
    pub version: u32,
    pub body: String,
    pub variables: Vec<String>,
    pub body_sha256: [u8; 32],
    pub created_at_ms: i64,
}

impl TryFrom<PromptVersionWire> for PromptVersion {
    type Error = String;

    fn try_from(wire: PromptVersionWire) -> Result<Self, Self::Error> {
        let version = Self {
            id: wire.id,
            prompt_id: wire.prompt_id,
            version: wire.version,
            body: wire.body,
            variables: wire.variables,
            body_sha256: wire.body_sha256,
            created_at_ms: wire.created_at_ms,
        };
        validate_prompt_version_wire(&version)?;
        Ok(version)
    }
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
#[serde(deny_unknown_fields, try_from = "CreatePromptWire")]
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

impl TryFrom<CreatePromptWire> for CreatePrompt {
    type Error = String;

    fn try_from(wire: CreatePromptWire) -> Result<Self, Self::Error> {
        let command = Self {
            prompt_id: wire.prompt_id,
            prompt_version_id: wire.prompt_version_id,
            title: wire.title,
            description: wire.description,
            tags: wire.tags,
            variables: wire.variables,
            body: wire.body,
            created_at_ms: wire.created_at_ms,
        };
        validate_prompt_command_wire(&command)?;
        Ok(command)
    }
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
#[serde(deny_unknown_fields, try_from = "CreatePromptVersionWire")]
pub struct CreatePromptVersion {
    pub prompt_id: PromptId,
    pub prompt_version_id: PromptVersionId,
    pub variables: Vec<String>,
    pub body: String,
    pub created_at_ms: i64,
    #[serde(deserialize_with = "deserialize_positive_revision")]
    pub expected_revision: u64,
}

impl TryFrom<CreatePromptVersionWire> for CreatePromptVersion {
    type Error = String;

    fn try_from(wire: CreatePromptVersionWire) -> Result<Self, Self::Error> {
        let command = Self {
            prompt_id: wire.prompt_id,
            prompt_version_id: wire.prompt_version_id,
            variables: wire.variables,
            body: wire.body,
            created_at_ms: wire.created_at_ms,
            expected_revision: wire.expected_revision,
        };
        command.validate().map_err(|error| error.to_string())?;
        validate_canonical_variables(&command.variables)?;
        Ok(command)
    }
}

impl CreatePromptVersion {
    pub fn validate(&self) -> Result<(), PromptValidationError> {
        validate_expected_revision(self.expected_revision)?;
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
#[serde(deny_unknown_fields, try_from = "RenamePromptWire")]
pub struct RenamePrompt {
    pub prompt_id: PromptId,
    pub title: String,
    #[serde(deserialize_with = "deserialize_positive_revision")]
    pub expected_revision: u64,
}

impl TryFrom<RenamePromptWire> for RenamePrompt {
    type Error = String;

    fn try_from(wire: RenamePromptWire) -> Result<Self, Self::Error> {
        let command = Self {
            prompt_id: wire.prompt_id,
            title: wire.title,
            expected_revision: wire.expected_revision,
        };
        command.validate().map_err(|error| error.to_string())?;
        validate_canonical_title(&command.title, MAX_PROMPT_TITLE_SCALARS)?;
        Ok(command)
    }
}

impl RenamePrompt {
    pub fn validate(&self) -> Result<(), PromptValidationError> {
        validate_expected_revision(self.expected_revision)?;
        validate_title(&self.title)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, try_from = "SetPromptTagsWire")]
pub struct SetPromptTags {
    pub prompt_id: PromptId,
    pub tags: Vec<String>,
    #[serde(deserialize_with = "deserialize_positive_revision")]
    pub expected_revision: u64,
}

impl TryFrom<SetPromptTagsWire> for SetPromptTags {
    type Error = String;

    fn try_from(wire: SetPromptTagsWire) -> Result<Self, Self::Error> {
        let command = Self {
            prompt_id: wire.prompt_id,
            tags: wire.tags,
            expected_revision: wire.expected_revision,
        };
        command.validate().map_err(|error| error.to_string())?;
        validate_canonical_tags(&command.tags)?;
        Ok(command)
    }
}

impl SetPromptTags {
    pub fn validate(&self) -> Result<Vec<String>, PromptValidationError> {
        validate_expected_revision(self.expected_revision)?;
        normalized_tags(&self.tags)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchivePrompt {
    pub prompt_id: PromptId,
    pub archived_at_ms: i64,
    #[serde(deserialize_with = "deserialize_positive_revision")]
    pub expected_revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestorePrompt {
    pub prompt_id: PromptId,
    #[serde(deserialize_with = "deserialize_positive_revision")]
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
            Self::ArchivePrompt(command) => validate_expected_revision(command.expected_revision),
            Self::RestorePrompt(command) => validate_expected_revision(command.expected_revision),
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
        wire.command
            .validate()
            .map_err(|_| PromptCodecError("prompt command validation failed".into()))?;
        let canonical = wire.command.encode()?;
        if canonical != payload {
            return Err(PromptCodecError(
                "prompt command payload is not canonical".into(),
            ));
        }
        Ok(wire.command)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, try_from = "PromptMutationReceiptWire")]
pub struct PromptMutationReceipt {
    pub command_id: CommandId,
    pub prompt_id: PromptId,
    pub prompt_version_id: PromptVersionId,
    pub revision: u64,
}

impl TryFrom<PromptMutationReceiptWire> for PromptMutationReceipt {
    type Error = String;

    fn try_from(wire: PromptMutationReceiptWire) -> Result<Self, Self::Error> {
        if wire.revision == 0 {
            return Err("prompt receipt revision must be positive".into());
        }
        Ok(Self {
            command_id: wire.command_id,
            prompt_id: wire.prompt_id,
            prompt_version_id: wire.prompt_version_id,
            revision: wire.revision,
        })
    }
}

impl PromptMutationReceipt {
    pub fn encode(&self) -> Result<Vec<u8>, PromptCodecError> {
        rmp_serde::to_vec_named(self)
            .map_err(|_| PromptCodecError("prompt receipt encoding failed".into()))
    }

    pub fn decode(payload: &[u8]) -> Result<Self, PromptCodecError> {
        rmp_serde::from_slice(payload)
            .map_err(|_| PromptCodecError("prompt receipt decoding failed".into()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    rename_all = "snake_case",
    deny_unknown_fields,
    try_from = "PromptEventSerde"
)]
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

impl TryFrom<PromptEventSerde> for PromptEvent {
    type Error = String;

    fn try_from(wire: PromptEventSerde) -> Result<Self, Self::Error> {
        let event = match wire {
            PromptEventSerde::PromptCreated { prompt, version } => {
                Self::PromptCreated { prompt, version }
            }
            PromptEventSerde::PromptVersionCreated {
                prompt_id,
                version,
                revision,
            } => Self::PromptVersionCreated {
                prompt_id,
                version,
                revision,
            },
            PromptEventSerde::PromptRenamed {
                prompt_id,
                title,
                revision,
            } => Self::PromptRenamed {
                prompt_id,
                title,
                revision,
            },
            PromptEventSerde::PromptTagsSet {
                prompt_id,
                tags,
                revision,
            } => Self::PromptTagsSet {
                prompt_id,
                tags,
                revision,
            },
            PromptEventSerde::PromptArchived {
                prompt_id,
                archived_at_ms,
                revision,
            } => Self::PromptArchived {
                prompt_id,
                archived_at_ms,
                revision,
            },
            PromptEventSerde::PromptRestored {
                prompt_id,
                revision,
            } => Self::PromptRestored {
                prompt_id,
                revision,
            },
        };
        validate_prompt_event_wire(&event)?;
        Ok(event)
    }
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
#[serde(deny_unknown_fields, try_from = "PromptChainWire")]
pub struct PromptChain {
    pub id: PromptChainId,
    pub title: String,
    pub description: Option<String>,
    pub revision: u64,
    pub archived_at_ms: Option<i64>,
}

impl TryFrom<PromptChainWire> for PromptChain {
    type Error = String;

    fn try_from(wire: PromptChainWire) -> Result<Self, Self::Error> {
        if wire.revision == 0 {
            return Err("prompt chain revision must be positive".into());
        }
        validate_canonical_chain_metadata(&wire.title, wire.description.as_deref())?;
        Ok(Self {
            id: wire.id,
            title: wire.title,
            description: wire.description,
            revision: wire.revision,
            archived_at_ms: wire.archived_at_ms,
        })
    }
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
#[serde(deny_unknown_fields, try_from = "CreatePromptChainWire")]
pub struct CreatePromptChain {
    pub chain_id: PromptChainId,
    pub title: String,
    pub description: Option<String>,
    pub created_at_ms: i64,
}

impl TryFrom<CreatePromptChainWire> for CreatePromptChain {
    type Error = String;

    fn try_from(wire: CreatePromptChainWire) -> Result<Self, Self::Error> {
        let command = Self {
            chain_id: wire.chain_id,
            title: wire.title,
            description: wire.description,
            created_at_ms: wire.created_at_ms,
        };
        command.validate().map_err(|error| error.to_string())?;
        validate_canonical_chain_metadata(&command.title, command.description.as_deref())?;
        Ok(command)
    }
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
#[serde(deny_unknown_fields, try_from = "RenamePromptChainWire")]
pub struct RenamePromptChain {
    pub chain_id: PromptChainId,
    pub title: String,
    #[serde(deserialize_with = "deserialize_positive_revision")]
    pub expected_revision: u64,
}

impl TryFrom<RenamePromptChainWire> for RenamePromptChain {
    type Error = String;

    fn try_from(wire: RenamePromptChainWire) -> Result<Self, Self::Error> {
        let command = Self {
            chain_id: wire.chain_id,
            title: wire.title,
            expected_revision: wire.expected_revision,
        };
        command.validate().map_err(|error| error.to_string())?;
        validate_canonical_title(&command.title, MAX_PROMPT_CHAIN_TITLE_SCALARS)?;
        Ok(command)
    }
}

impl RenamePromptChain {
    pub fn validate(&self) -> Result<(), PromptValidationError> {
        validate_expected_revision(self.expected_revision)?;
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
    #[serde(deserialize_with = "deserialize_positive_revision")]
    pub expected_revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MovePromptChainLink {
    pub chain_id: PromptChainId,
    pub link_id: PromptChainLinkId,
    pub before_link_id: Option<PromptChainLinkId>,
    #[serde(deserialize_with = "deserialize_positive_revision")]
    pub expected_revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemovePromptChainLink {
    pub chain_id: PromptChainId,
    pub link_id: PromptChainLinkId,
    #[serde(deserialize_with = "deserialize_positive_revision")]
    pub expected_revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdatePromptChainLinkVersion {
    pub chain_id: PromptChainId,
    pub link_id: PromptChainLinkId,
    #[serde(deserialize_with = "deserialize_positive_revision")]
    pub expected_revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchivePromptChain {
    pub chain_id: PromptChainId,
    pub archived_at_ms: i64,
    #[serde(deserialize_with = "deserialize_positive_revision")]
    pub expected_revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestorePromptChain {
    pub chain_id: PromptChainId,
    #[serde(deserialize_with = "deserialize_positive_revision")]
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
            Self::InsertPromptChainLink(command) => {
                validate_expected_revision(command.expected_revision)
            }
            Self::MovePromptChainLink(command) => {
                validate_expected_revision(command.expected_revision)
            }
            Self::RemovePromptChainLink(command) => {
                validate_expected_revision(command.expected_revision)
            }
            Self::UpdatePromptChainLinkVersion(command) => {
                validate_expected_revision(command.expected_revision)
            }
            Self::ArchivePromptChain(command) => {
                validate_expected_revision(command.expected_revision)
            }
            Self::RestorePromptChain(command) => {
                validate_expected_revision(command.expected_revision)
            }
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
#[serde(deny_unknown_fields, try_from = "PromptChainMutationReceiptWire")]
pub struct PromptChainMutationReceipt {
    pub command_id: CommandId,
    pub chain_id: PromptChainId,
    pub link_id: Option<PromptChainLinkId>,
    pub revision: u64,
}

impl TryFrom<PromptChainMutationReceiptWire> for PromptChainMutationReceipt {
    type Error = String;

    fn try_from(wire: PromptChainMutationReceiptWire) -> Result<Self, Self::Error> {
        if wire.revision == 0 {
            return Err("prompt chain receipt revision must be positive".into());
        }
        Ok(Self {
            command_id: wire.command_id,
            chain_id: wire.chain_id,
            link_id: wire.link_id,
            revision: wire.revision,
        })
    }
}

impl PromptChainMutationReceipt {
    pub fn encode(&self) -> Result<Vec<u8>, PromptCodecError> {
        rmp_serde::to_vec_named(self)
            .map_err(|_| PromptCodecError("prompt chain receipt encoding failed".into()))
    }

    pub fn decode(payload: &[u8]) -> Result<Self, PromptCodecError> {
        rmp_serde::from_slice(payload)
            .map_err(|_| PromptCodecError("prompt chain receipt decoding failed".into()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    rename_all = "snake_case",
    deny_unknown_fields,
    try_from = "PromptChainEventSerde"
)]
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

impl TryFrom<PromptChainEventSerde> for PromptChainEvent {
    type Error = String;

    fn try_from(wire: PromptChainEventSerde) -> Result<Self, Self::Error> {
        let event = match wire {
            PromptChainEventSerde::PromptChainCreated { chain } => {
                Self::PromptChainCreated { chain }
            }
            PromptChainEventSerde::PromptChainRenamed {
                chain_id,
                title,
                revision,
            } => Self::PromptChainRenamed {
                chain_id,
                title,
                revision,
            },
            PromptChainEventSerde::PromptChainLinksReplaced {
                chain_id,
                links,
                revision,
            } => Self::PromptChainLinksReplaced {
                chain_id,
                links,
                revision,
            },
            PromptChainEventSerde::PromptChainArchived {
                chain_id,
                archived_at_ms,
                revision,
            } => Self::PromptChainArchived {
                chain_id,
                archived_at_ms,
                revision,
            },
            PromptChainEventSerde::PromptChainRestored { chain_id, revision } => {
                Self::PromptChainRestored { chain_id, revision }
            }
        };
        validate_chain_event_wire(&event)?;
        Ok(event)
    }
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
        let canonical = wire.event.encode()?;
        if canonical != payload {
            return Err(PromptCodecError(
                "prompt chain event payload is not canonical".into(),
            ));
        }
        Ok(wire.event)
    }
}

fn validate_expected_revision(revision: u64) -> Result<(), PromptValidationError> {
    if revision == 0 {
        Err(PromptValidationError::ExpectedRevisionZero)
    } else {
        Ok(())
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
    if trim_prompt_whitespace(title).is_empty() {
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

fn validate_canonical_title(title: &str, max: usize) -> Result<(), String> {
    validate_title_with_limit(title, max, |actual, max| {
        PromptValidationError::TitleTooLong { actual, max }
    })
    .map_err(|error| error.to_string())?;
    if title != trim_prompt_whitespace(title) {
        return Err("prompt title must be trimmed".into());
    }
    Ok(())
}

fn validate_canonical_description(description: Option<&str>, max: usize) -> Result<(), String> {
    validate_description_with_limit(description, max).map_err(|error| error.to_string())?;
    if description.is_some_and(|description| description != trim_prompt_whitespace(description)) {
        return Err("prompt description must be trimmed".into());
    }
    Ok(())
}

/// Prompt metadata uses Rust's Unicode whitespace definition at every boundary.
/// The SQLite migration mirrors this exact codepoint set with `char(...)`.
pub fn trim_prompt_whitespace(value: &str) -> &str {
    value.trim_matches(char::is_whitespace)
}

fn validate_canonical_tags(tags: &[String]) -> Result<(), String> {
    let normalized = normalized_tags(tags).map_err(|error| error.to_string())?;
    if normalized != tags {
        return Err("prompt tags must be normalized".into());
    }
    Ok(())
}

fn validate_canonical_variables(variables: &[String]) -> Result<(), String> {
    let normalized = normalized_variables(variables).map_err(|error| error.to_string())?;
    if normalized != variables {
        return Err("prompt variables must be normalized".into());
    }
    Ok(())
}

fn validate_canonical_prompt_metadata(
    title: &str,
    description: Option<&str>,
    tags: &[String],
    variables: &[String],
) -> Result<(), String> {
    validate_canonical_title(title, MAX_PROMPT_TITLE_SCALARS)?;
    validate_canonical_description(description, MAX_PROMPT_DESCRIPTION_SCALARS)?;
    validate_canonical_tags(tags)?;
    validate_canonical_variables(variables)?;
    Ok(())
}

fn validate_canonical_chain_metadata(title: &str, description: Option<&str>) -> Result<(), String> {
    validate_canonical_title(title, MAX_PROMPT_CHAIN_TITLE_SCALARS)?;
    validate_canonical_description(description, MAX_PROMPT_CHAIN_DESCRIPTION_SCALARS)?;
    Ok(())
}

fn validate_saved_prompt_wire(wire: &SavedPromptWire) -> Result<(), String> {
    if wire.revision == 0 {
        return Err("saved prompt revision must be positive".into());
    }
    validate_canonical_prompt_metadata(&wire.title, wire.description.as_deref(), &wire.tags, &[])
}

fn validate_prompt_version_wire(version: &PromptVersion) -> Result<(), String> {
    if version.version == 0 {
        return Err("prompt version number must be positive".into());
    }
    validate_body(&version.body).map_err(|error| error.to_string())?;
    validate_canonical_variables(&version.variables)?;
    let expected_hash: [u8; 32] = Sha256::digest(version.body.as_bytes()).into();
    if version.body_sha256 != expected_hash {
        return Err("prompt body hash mismatch".into());
    }
    Ok(())
}

fn validate_prompt_command_wire(command: &CreatePrompt) -> Result<(), String> {
    command.validate().map_err(|error| error.to_string())?;
    validate_canonical_prompt_metadata(
        &command.title,
        command.description.as_deref(),
        &command.tags,
        &command.variables,
    )
}

fn validate_prompt_event_wire(event: &PromptEvent) -> Result<(), String> {
    match event {
        PromptEvent::PromptCreated { prompt, version } => {
            if prompt.revision != 1
                || prompt.archived_at_ms.is_some()
                || prompt.current_version_id != version.id
                || version.prompt_id != prompt.id
                || version.version != 1
            {
                return Err("prompt created event lineage is invalid".into());
            }
        }
        PromptEvent::PromptVersionCreated {
            prompt_id,
            version,
            revision,
        } => {
            if *revision == 0 || version.prompt_id != *prompt_id {
                return Err("prompt version event lineage is invalid".into());
            }
        }
        PromptEvent::PromptRenamed {
            title, revision, ..
        } => {
            if *revision == 0 {
                return Err("prompt rename event revision is invalid".into());
            }
            validate_canonical_title(title, MAX_PROMPT_TITLE_SCALARS)?;
        }
        PromptEvent::PromptTagsSet { tags, revision, .. } => {
            if *revision == 0 {
                return Err("prompt tag event revision is invalid".into());
            }
            validate_canonical_tags(tags)?;
        }
        PromptEvent::PromptArchived { revision, .. }
        | PromptEvent::PromptRestored { revision, .. } => {
            if *revision == 0 {
                return Err("prompt event revision is invalid".into());
            }
        }
    }
    Ok(())
}

fn validate_chain_event_wire(event: &PromptChainEvent) -> Result<(), String> {
    match event {
        PromptChainEvent::PromptChainCreated { chain } => {
            if chain.archived_at_ms.is_some() || chain.revision != 1 {
                return Err("prompt chain created event lineage is invalid".into());
            }
            validate_canonical_chain_metadata(&chain.title, chain.description.as_deref())?;
        }
        PromptChainEvent::PromptChainRenamed {
            title, revision, ..
        } => {
            if *revision == 0 {
                return Err("prompt chain rename event revision is invalid".into());
            }
            validate_canonical_title(title, MAX_PROMPT_CHAIN_TITLE_SCALARS)?;
        }
        PromptChainEvent::PromptChainLinksReplaced {
            chain_id,
            links,
            revision,
        } => {
            if *revision == 0 {
                return Err("prompt chain links event revision is invalid".into());
            }
            for (position, link) in links.iter().enumerate() {
                let expected_position =
                    u32::try_from(position).map_err(|_| "prompt chain is too long".to_string())?;
                if link.chain_id != *chain_id
                    || link.position != expected_position
                    || links[..position]
                        .iter()
                        .any(|previous| previous.id == link.id)
                {
                    return Err("prompt chain links must be a dense ordered prefix".into());
                }
            }
        }
        PromptChainEvent::PromptChainArchived { revision, .. }
        | PromptChainEvent::PromptChainRestored { revision, .. } => {
            if *revision == 0 {
                return Err("prompt chain event revision is invalid".into());
            }
        }
    }
    Ok(())
}
