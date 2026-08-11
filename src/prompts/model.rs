use std::collections::HashSet;
use std::fmt;

use serde::ser::{Error as SerdeError, Serializer};
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
/// Hard ceiling for any one prompt/chain MessagePack frame before decoding.
pub const MAX_PROMPT_WIRE_BYTES: usize = 4 * 1024 * 1024;
/// Compatibility name for the public codec budget. Durable SQLite rows use
/// the smaller 512 KiB bound in `PromptStore`.
pub const MAX_PROMPT_PUBLIC_WIRE_BYTES: usize = MAX_PROMPT_WIRE_BYTES;
/// Maximum number of links a durable prompt chain may contain.
pub const MAX_PROMPT_CHAIN_LINKS: usize = 2_000;
const MAX_PROMPT_WIRE_DEPTH: usize = 64;
const MAX_PROMPT_WIRE_MAP_ENTRIES: usize = 64;
const MAX_PROMPT_WIRE_COLLECTION_ITEMS: usize = MAX_PROMPT_CHAIN_LINKS;
const MAX_PROMPT_WIRE_NODES: usize = 32_768;
const MAX_PROMPT_WIRE_STRING_BYTES: usize = MAX_PROMPT_BODY_BYTES;
const MAX_PROMPT_WIRE_BIN_BYTES: usize = MAX_PROMPT_BODY_BYTES;
const MAX_PROMPT_WIRE_EXT_BYTES: usize = 64;

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

/// Durable chain-command envelope. Its schema is intentionally distinct from
/// the public codec envelope because it records the exact original caller
/// payload alongside the normalized command and version resolution used by
/// the store when a link command was applied.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PromptChainCommandDurableWire<T> {
    pub(crate) schema_version: u32,
    pub(crate) original_command_sha256: [u8; 32],
    pub(crate) original_command_payload: Vec<u8>,
    pub(crate) command: T,
    pub(crate) resolved_prompt_version_id: Option<PromptVersionId>,
}

pub(crate) const PROMPT_DURABLE_CHAIN_WIRE_SCHEMA_VERSION: u32 = 3;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PromptChainEventWire<T> {
    schema_version: u32,
    event: T,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PromptMutationReceiptEnvelope {
    schema_version: u32,
    receipt: PromptMutationReceiptWire,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PromptChainMutationReceiptEnvelope {
    schema_version: u32,
    receipt: PromptChainMutationReceiptWire,
}

#[derive(Serialize, Deserialize)]
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

#[derive(Serialize, Deserialize)]
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

#[derive(Serialize, Deserialize)]
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

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CreatePromptVersionWire {
    prompt_id: PromptId,
    prompt_version_id: PromptVersionId,
    variables: Vec<String>,
    body: String,
    created_at_ms: i64,
    expected_revision: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RenamePromptWire {
    prompt_id: PromptId,
    title: String,
    expected_revision: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SetPromptTagsWire {
    prompt_id: PromptId,
    tags: Vec<String>,
    expected_revision: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PromptChainWire {
    id: PromptChainId,
    title: String,
    description: Option<String>,
    revision: u64,
    archived_at_ms: Option<i64>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CreatePromptChainWire {
    chain_id: PromptChainId,
    title: String,
    description: Option<String>,
    created_at_ms: i64,
}

#[derive(Serialize, Deserialize)]
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
        links: Vec<PromptChainLinkWire>,
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

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PromptMutationReceiptWire {
    command_id: CommandId,
    prompt_id: PromptId,
    prompt_version_id: PromptVersionId,
    revision: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PromptChainMutationReceiptWire {
    command_id: CommandId,
    chain_id: PromptChainId,
    link_id: Option<PromptChainLinkId>,
    revision: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PromptChainLinkWire {
    id: PromptChainLinkId,
    chain_id: PromptChainId,
    position: u32,
    prompt_id: PromptId,
    prompt_version_id: PromptVersionId,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArchivePromptSerializeWire {
    prompt_id: PromptId,
    archived_at_ms: i64,
    expected_revision: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RestorePromptSerializeWire {
    prompt_id: PromptId,
    expected_revision: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct InsertPromptChainLinkSerializeWire {
    chain_id: PromptChainId,
    link_id: PromptChainLinkId,
    prompt_id: PromptId,
    prompt_version_id: Option<PromptVersionId>,
    before_link_id: Option<PromptChainLinkId>,
    expected_revision: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MovePromptChainLinkSerializeWire {
    chain_id: PromptChainId,
    link_id: PromptChainLinkId,
    before_link_id: Option<PromptChainLinkId>,
    expected_revision: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RemovePromptChainLinkSerializeWire {
    chain_id: PromptChainId,
    link_id: PromptChainLinkId,
    expected_revision: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdatePromptChainLinkVersionSerializeWire {
    chain_id: PromptChainId,
    link_id: PromptChainLinkId,
    expected_revision: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArchivePromptChainSerializeWire {
    chain_id: PromptChainId,
    archived_at_ms: i64,
    expected_revision: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RestorePromptChainSerializeWire {
    chain_id: PromptChainId,
    expected_revision: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum PromptCommandSerializeWire<'a> {
    CreatePrompt(&'a CreatePrompt),
    CreatePromptVersion(&'a CreatePromptVersion),
    RenamePrompt(&'a RenamePrompt),
    SetPromptTags(&'a SetPromptTags),
    ArchivePrompt(&'a ArchivePrompt),
    RestorePrompt(&'a RestorePrompt),
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum PromptEventSerializeWire<'a> {
    PromptCreated {
        prompt: &'a SavedPrompt,
        version: &'a PromptVersion,
    },
    PromptVersionCreated {
        prompt_id: PromptId,
        version: &'a PromptVersion,
        revision: u64,
    },
    PromptRenamed {
        prompt_id: PromptId,
        title: &'a str,
        revision: u64,
    },
    PromptTagsSet {
        prompt_id: PromptId,
        tags: &'a [String],
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

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum PromptChainCommandSerializeWire<'a> {
    CreatePromptChain(&'a CreatePromptChain),
    RenamePromptChain(&'a RenamePromptChain),
    InsertPromptChainLink(&'a InsertPromptChainLink),
    MovePromptChainLink(&'a MovePromptChainLink),
    RemovePromptChainLink(&'a RemovePromptChainLink),
    UpdatePromptChainLinkVersion(&'a UpdatePromptChainLinkVersion),
    ArchivePromptChain(&'a ArchivePromptChain),
    RestorePromptChain(&'a RestorePromptChain),
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum PromptChainEventSerializeWire<'a> {
    PromptChainCreated {
        chain: &'a PromptChain,
    },
    PromptChainRenamed {
        chain_id: PromptChainId,
        title: &'a str,
        revision: u64,
    },
    PromptChainLinksReplaced {
        chain_id: PromptChainId,
        links: &'a [PromptChainLink],
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptValidationError {
    ExpectedRevisionZero,
    VersionZero,
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
    InvalidTag {
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
            Self::VersionZero => f.write_str("prompt version number must be positive"),
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
            Self::InvalidTag { position } => write!(
                f,
                "prompt tag at position {position} must use printable ASCII characters"
            ),
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
        let tag = trim_prompt_whitespace(tag).to_ascii_lowercase();
        if tag.is_empty() {
            return Err(PromptValidationError::EmptyTag { position });
        }
        if tag.bytes().any(|byte| !(0x20..=0x7e).contains(&byte)) {
            return Err(PromptValidationError::InvalidTag { position });
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

/// Keep prompt metadata whitespace identical across Rust model, codec, store,
/// and the SQLite migration's explicit Unicode codepoint set.
pub fn trim_prompt_whitespace(value: &str) -> &str {
    value.trim_matches(char::is_whitespace)
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

#[derive(Clone, PartialEq, Eq, Deserialize)]
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

impl Serialize for SavedPrompt {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        validate_saved_prompt_values(
            self.revision,
            &self.title,
            self.description.as_deref(),
            &self.tags,
        )
        .map_err(S::Error::custom)?;
        SavedPromptWire {
            id: self.id,
            title: self.title.clone(),
            description: self.description.clone(),
            tags: self.tags.clone(),
            current_version_id: self.current_version_id,
            revision: self.revision,
            archived_at_ms: self.archived_at_ms,
        }
        .serialize(serializer)
    }
}

impl fmt::Debug for SavedPrompt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SavedPrompt")
            .field("id", &self.id)
            .field("title_bytes", &self.title.len())
            .field(
                "description_bytes",
                &self.description.as_deref().map(str::len),
            )
            .field("tag_count", &self.tags.len())
            .field("current_version_id", &self.current_version_id)
            .field("revision", &self.revision)
            .field("archived_at_ms", &self.archived_at_ms)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq, Deserialize)]
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

impl Serialize for PromptVersion {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        validate_prompt_version_wire(self).map_err(S::Error::custom)?;
        PromptVersionWire {
            id: self.id,
            prompt_id: self.prompt_id,
            version: self.version,
            body: self.body.clone(),
            variables: self.variables.clone(),
            body_sha256: self.body_sha256,
            created_at_ms: self.created_at_ms,
        }
        .serialize(serializer)
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
        if version == 0 {
            return Err(PromptValidationError::VersionZero);
        }
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
            .field("variable_count", &self.variables.len())
            .field("body_sha256_bytes", &self.body_sha256.len())
            .field("created_at_ms", &self.created_at_ms)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq, Deserialize)]
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
            .field("title_bytes", &self.title.len())
            .field(
                "description_bytes",
                &self.description.as_deref().map(str::len),
            )
            .field("tag_count", &self.tags.len())
            .field("variable_count", &self.variables.len())
            .field("body", &RedactedBody(self.body.len()))
            .field("created_at_ms", &self.created_at_ms)
            .finish()
    }
}

impl Serialize for CreatePrompt {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        validate_prompt_command_wire(self).map_err(S::Error::custom)?;
        CreatePromptWire {
            prompt_id: self.prompt_id,
            prompt_version_id: self.prompt_version_id,
            title: self.title.clone(),
            description: self.description.clone(),
            tags: self.tags.clone(),
            variables: self.variables.clone(),
            body: self.body.clone(),
            created_at_ms: self.created_at_ms,
        }
        .serialize(serializer)
    }
}

#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, try_from = "CreatePromptVersionWire")]
pub struct CreatePromptVersion {
    pub prompt_id: PromptId,
    pub prompt_version_id: PromptVersionId,
    pub variables: Vec<String>,
    pub body: String,
    pub created_at_ms: i64,
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
            .field("variable_count", &self.variables.len())
            .field("body", &RedactedBody(self.body.len()))
            .field("created_at_ms", &self.created_at_ms)
            .field("expected_revision", &self.expected_revision)
            .finish()
    }
}

impl Serialize for CreatePromptVersion {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.validate().map_err(S::Error::custom)?;
        validate_canonical_variables(&self.variables).map_err(S::Error::custom)?;
        CreatePromptVersionWire {
            prompt_id: self.prompt_id,
            prompt_version_id: self.prompt_version_id,
            variables: self.variables.clone(),
            body: self.body.clone(),
            created_at_ms: self.created_at_ms,
            expected_revision: self.expected_revision,
        }
        .serialize(serializer)
    }
}

#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, try_from = "RenamePromptWire")]
pub struct RenamePrompt {
    pub prompt_id: PromptId,
    pub title: String,
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

impl fmt::Debug for RenamePrompt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RenamePrompt")
            .field("prompt_id", &self.prompt_id)
            .field("title_bytes", &self.title.len())
            .field("expected_revision", &self.expected_revision)
            .finish()
    }
}

impl Serialize for RenamePrompt {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.validate().map_err(S::Error::custom)?;
        validate_canonical_title(&self.title, MAX_PROMPT_TITLE_SCALARS)
            .map_err(S::Error::custom)?;
        RenamePromptWire {
            prompt_id: self.prompt_id,
            title: self.title.clone(),
            expected_revision: self.expected_revision,
        }
        .serialize(serializer)
    }
}

#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, try_from = "SetPromptTagsWire")]
pub struct SetPromptTags {
    pub prompt_id: PromptId,
    pub tags: Vec<String>,
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

impl fmt::Debug for SetPromptTags {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SetPromptTags")
            .field("prompt_id", &self.prompt_id)
            .field("tag_count", &self.tags.len())
            .field("expected_revision", &self.expected_revision)
            .finish()
    }
}

impl Serialize for SetPromptTags {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.validate().map_err(S::Error::custom)?;
        validate_canonical_tags(&self.tags).map_err(S::Error::custom)?;
        SetPromptTagsWire {
            prompt_id: self.prompt_id,
            tags: self.tags.clone(),
            expected_revision: self.expected_revision,
        }
        .serialize(serializer)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, try_from = "ArchivePromptSerializeWire")]
pub struct ArchivePrompt {
    pub prompt_id: PromptId,
    pub archived_at_ms: i64,
    pub expected_revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, try_from = "RestorePromptSerializeWire")]
pub struct RestorePrompt {
    pub prompt_id: PromptId,
    pub expected_revision: u64,
}

impl TryFrom<ArchivePromptSerializeWire> for ArchivePrompt {
    type Error = String;

    fn try_from(wire: ArchivePromptSerializeWire) -> Result<Self, Self::Error> {
        validate_expected_revision(wire.expected_revision).map_err(|error| error.to_string())?;
        Ok(Self {
            prompt_id: wire.prompt_id,
            archived_at_ms: wire.archived_at_ms,
            expected_revision: wire.expected_revision,
        })
    }
}

impl TryFrom<RestorePromptSerializeWire> for RestorePrompt {
    type Error = String;

    fn try_from(wire: RestorePromptSerializeWire) -> Result<Self, Self::Error> {
        validate_expected_revision(wire.expected_revision).map_err(|error| error.to_string())?;
        Ok(Self {
            prompt_id: wire.prompt_id,
            expected_revision: wire.expected_revision,
        })
    }
}

impl Serialize for ArchivePrompt {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        validate_expected_revision(self.expected_revision).map_err(S::Error::custom)?;
        ArchivePromptSerializeWire {
            prompt_id: self.prompt_id,
            archived_at_ms: self.archived_at_ms,
            expected_revision: self.expected_revision,
        }
        .serialize(serializer)
    }
}

impl Serialize for RestorePrompt {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        validate_expected_revision(self.expected_revision).map_err(S::Error::custom)?;
        RestorePromptSerializeWire {
            prompt_id: self.prompt_id,
            expected_revision: self.expected_revision,
        }
        .serialize(serializer)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
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
    pub fn canonicalize(&self) -> Result<Self, PromptValidationError> {
        match self {
            Self::CreatePrompt(command) => {
                command.validate()?;
                let mut canonical = command.clone();
                canonical.title = trim_prompt_whitespace(&command.title).to_string();
                canonical.description = command
                    .description
                    .as_deref()
                    .map(trim_prompt_whitespace)
                    .map(str::to_string);
                canonical.tags = normalized_tags(&command.tags)?;
                canonical.variables = normalized_variables(&command.variables)?;
                Ok(Self::CreatePrompt(canonical))
            }
            Self::CreatePromptVersion(command) => {
                command.validate()?;
                let mut canonical = command.clone();
                canonical.variables = normalized_variables(&command.variables)?;
                Ok(Self::CreatePromptVersion(canonical))
            }
            Self::RenamePrompt(command) => {
                command.validate()?;
                let mut canonical = command.clone();
                canonical.title = trim_prompt_whitespace(&command.title).to_string();
                Ok(Self::RenamePrompt(canonical))
            }
            Self::SetPromptTags(command) => {
                command.validate()?;
                let mut canonical = command.clone();
                canonical.tags = normalized_tags(&command.tags)?;
                Ok(Self::SetPromptTags(canonical))
            }
            Self::ArchivePrompt(command) => {
                validate_expected_revision(command.expected_revision)?;
                Ok(Self::ArchivePrompt(command.clone()))
            }
            Self::RestorePrompt(command) => {
                validate_expected_revision(command.expected_revision)?;
                Ok(Self::RestorePrompt(command.clone()))
            }
        }
    }

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

    pub fn fingerprint(&self) -> Result<[u8; 32], PromptCodecError> {
        let canonical = self
            .canonicalize()
            .map_err(|_| PromptCodecError("prompt command validation failed".into()))?;
        canonical.fingerprint_canonical()
    }

    /// Hash the exact canonical wire bytes used for persistence and execution.
    pub(crate) fn fingerprint_canonical(&self) -> Result<[u8; 32], PromptCodecError> {
        let encoded = self.encode_canonical()?;
        let fingerprint = Sha256::digest(encoded).into();
        Ok(fingerprint)
    }

    pub fn encode(&self) -> Result<Vec<u8>, PromptCodecError> {
        self.validate()
            .map_err(|_| PromptCodecError("prompt command validation failed".into()))?;
        validate_prompt_command_canonical(self)
            .map_err(|_| PromptCodecError("prompt command validation failed".into()))?;
        self.encode_canonical()
    }

    fn encode_canonical(&self) -> Result<Vec<u8>, PromptCodecError> {
        bounded_wire_encode(
            &PromptCommandWire {
                schema_version: PROMPT_WIRE_SCHEMA_VERSION,
                command: self,
            },
            "prompt command encoding failed",
        )
    }

    pub fn decode(payload: &[u8]) -> Result<Self, PromptCodecError> {
        validate_wire_payload_size(payload)?;
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

impl Serialize for PromptCommand {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        validate_prompt_command_canonical(self).map_err(S::Error::custom)?;
        let wire = match self {
            Self::CreatePrompt(command) => PromptCommandSerializeWire::CreatePrompt(command),
            Self::CreatePromptVersion(command) => {
                PromptCommandSerializeWire::CreatePromptVersion(command)
            }
            Self::RenamePrompt(command) => PromptCommandSerializeWire::RenamePrompt(command),
            Self::SetPromptTags(command) => PromptCommandSerializeWire::SetPromptTags(command),
            Self::ArchivePrompt(command) => PromptCommandSerializeWire::ArchivePrompt(command),
            Self::RestorePrompt(command) => PromptCommandSerializeWire::RestorePrompt(command),
        };
        wire.serialize(serializer)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, try_from = "PromptMutationReceiptEnvelope")]
pub struct PromptMutationReceipt {
    pub command_id: CommandId,
    pub prompt_id: PromptId,
    pub prompt_version_id: PromptVersionId,
    pub revision: u64,
}

impl TryFrom<PromptMutationReceiptEnvelope> for PromptMutationReceipt {
    type Error = String;

    fn try_from(envelope: PromptMutationReceiptEnvelope) -> Result<Self, Self::Error> {
        if envelope.schema_version != PROMPT_WIRE_SCHEMA_VERSION {
            return Err("unsupported prompt receipt schema".into());
        }
        let wire = envelope.receipt;
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
        if self.revision == 0 {
            return Err(PromptCodecError("prompt receipt validation failed".into()));
        }
        bounded_wire_encode(
            &PromptMutationReceiptEnvelope {
                schema_version: PROMPT_WIRE_SCHEMA_VERSION,
                receipt: PromptMutationReceiptWire {
                    command_id: self.command_id,
                    prompt_id: self.prompt_id,
                    prompt_version_id: self.prompt_version_id,
                    revision: self.revision,
                },
            },
            "prompt receipt encoding failed",
        )
    }

    pub fn decode(payload: &[u8]) -> Result<Self, PromptCodecError> {
        validate_wire_payload_size(payload)?;
        let envelope: PromptMutationReceiptEnvelope = rmp_serde::from_slice(payload)
            .map_err(|_| PromptCodecError("prompt receipt decoding failed".into()))?;
        let receipt = Self::try_from(envelope)
            .map_err(|_| PromptCodecError("prompt receipt validation failed".into()))?;
        if receipt.encode()? != payload {
            return Err(PromptCodecError(
                "prompt receipt payload is not canonical".into(),
            ));
        }
        Ok(receipt)
    }
}

impl Serialize for PromptMutationReceipt {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        if self.revision == 0 {
            return Err(S::Error::custom("prompt receipt revision must be positive"));
        }
        PromptMutationReceiptEnvelope {
            schema_version: PROMPT_WIRE_SCHEMA_VERSION,
            receipt: PromptMutationReceiptWire {
                command_id: self.command_id,
                prompt_id: self.prompt_id,
                prompt_version_id: self.prompt_version_id,
                revision: self.revision,
            },
        }
        .serialize(serializer)
    }
}

#[derive(Clone, PartialEq, Eq, Deserialize)]
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
        validate_prompt_event_wire(self)
            .map_err(|_| PromptCodecError("prompt event validation failed".into()))?;
        bounded_wire_encode(
            &PromptEventWire {
                schema_version: PROMPT_WIRE_SCHEMA_VERSION,
                event: self,
            },
            "prompt event encoding failed",
        )
    }

    pub fn decode(payload: &[u8]) -> Result<Self, PromptCodecError> {
        validate_wire_payload_size(payload)?;
        let wire: PromptEventWire<PromptEventSerde> = rmp_serde::from_slice(payload)
            .map_err(|_| PromptCodecError("prompt event decoding failed".into()))?;
        if wire.schema_version != PROMPT_WIRE_SCHEMA_VERSION {
            return Err(PromptCodecError("unsupported prompt event schema".into()));
        }
        let event = PromptEvent::try_from(wire.event)
            .map_err(|_| PromptCodecError("prompt event validation failed".into()))?;
        let canonical = event.encode()?;
        if canonical != payload {
            return Err(PromptCodecError(
                "prompt event payload is not canonical".into(),
            ));
        }
        Ok(event)
    }
}

impl Serialize for PromptEvent {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        validate_prompt_event_wire(self).map_err(S::Error::custom)?;
        let wire = match self {
            Self::PromptCreated { prompt, version } => {
                PromptEventSerializeWire::PromptCreated { prompt, version }
            }
            Self::PromptVersionCreated {
                prompt_id,
                version,
                revision,
            } => PromptEventSerializeWire::PromptVersionCreated {
                prompt_id: *prompt_id,
                version,
                revision: *revision,
            },
            Self::PromptRenamed {
                prompt_id,
                title,
                revision,
            } => PromptEventSerializeWire::PromptRenamed {
                prompt_id: *prompt_id,
                title,
                revision: *revision,
            },
            Self::PromptTagsSet {
                prompt_id,
                tags,
                revision,
            } => PromptEventSerializeWire::PromptTagsSet {
                prompt_id: *prompt_id,
                tags,
                revision: *revision,
            },
            Self::PromptArchived {
                prompt_id,
                archived_at_ms,
                revision,
            } => PromptEventSerializeWire::PromptArchived {
                prompt_id: *prompt_id,
                archived_at_ms: *archived_at_ms,
                revision: *revision,
            },
            Self::PromptRestored {
                prompt_id,
                revision,
            } => PromptEventSerializeWire::PromptRestored {
                prompt_id: *prompt_id,
                revision: *revision,
            },
        };
        wire.serialize(serializer)
    }
}

impl fmt::Debug for PromptEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PromptCreated { prompt, version } => f
                .debug_struct("PromptEvent::PromptCreated")
                .field("prompt", prompt)
                .field("version", version)
                .finish(),
            Self::PromptVersionCreated {
                prompt_id,
                version,
                revision,
            } => f
                .debug_struct("PromptEvent::PromptVersionCreated")
                .field("prompt_id", prompt_id)
                .field("version", version)
                .field("revision", revision)
                .finish(),
            Self::PromptRenamed {
                prompt_id,
                title,
                revision,
            } => f
                .debug_struct("PromptEvent::PromptRenamed")
                .field("prompt_id", prompt_id)
                .field("title_bytes", &title.len())
                .field("revision", revision)
                .finish(),
            Self::PromptTagsSet {
                prompt_id,
                tags,
                revision,
            } => f
                .debug_struct("PromptEvent::PromptTagsSet")
                .field("prompt_id", prompt_id)
                .field("tag_count", &tags.len())
                .field("revision", revision)
                .finish(),
            Self::PromptArchived {
                prompt_id,
                archived_at_ms,
                revision,
            } => f
                .debug_struct("PromptEvent::PromptArchived")
                .field("prompt_id", prompt_id)
                .field("archived_at_ms", archived_at_ms)
                .field("revision", revision)
                .finish(),
            Self::PromptRestored {
                prompt_id,
                revision,
            } => f
                .debug_struct("PromptEvent::PromptRestored")
                .field("prompt_id", prompt_id)
                .field("revision", revision)
                .finish(),
        }
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

#[derive(Clone, PartialEq, Eq, Deserialize)]
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

impl Serialize for PromptChain {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        if self.revision == 0 {
            return Err(S::Error::custom("prompt chain revision must be positive"));
        }
        validate_canonical_chain_metadata(&self.title, self.description.as_deref())
            .map_err(S::Error::custom)?;
        PromptChainWire {
            id: self.id,
            title: self.title.clone(),
            description: self.description.clone(),
            revision: self.revision,
            archived_at_ms: self.archived_at_ms,
        }
        .serialize(serializer)
    }
}

impl fmt::Debug for PromptChain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PromptChain")
            .field("id", &self.id)
            .field("title_bytes", &self.title.len())
            .field(
                "description_bytes",
                &self.description.as_deref().map(str::len),
            )
            .field("revision", &self.revision)
            .field("archived_at_ms", &self.archived_at_ms)
            .finish()
    }
}

/// A read-only link snapshot issued by the prompt store.
///
/// Wire decoding can construct only a structurally checked event containing
/// these private-field values; it does not establish prompt/version lineage.
/// Replay must still pass the link through `PromptStore` before treating it as
/// authoritative state.
#[derive(Clone, PartialEq, Eq)]
pub struct PromptChainLink {
    id: PromptChainLinkId,
    chain_id: PromptChainId,
    position: u32,
    prompt_id: PromptId,
    prompt_version_id: PromptVersionId,
}

impl PromptChainLink {
    pub fn id(&self) -> PromptChainLinkId {
        self.id
    }

    pub fn chain_id(&self) -> PromptChainId {
        self.chain_id
    }

    pub fn position(&self) -> u32 {
        self.position
    }

    pub fn prompt_id(&self) -> PromptId {
        self.prompt_id
    }

    pub fn prompt_version_id(&self) -> PromptVersionId {
        self.prompt_version_id
    }

    pub(crate) fn store_issued(
        id: PromptChainLinkId,
        chain_id: PromptChainId,
        position: u32,
        prompt_id: PromptId,
        prompt_version_id: PromptVersionId,
    ) -> Self {
        Self {
            id,
            chain_id,
            position,
            prompt_id,
            prompt_version_id,
        }
    }
}

impl fmt::Debug for PromptChainLink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PromptChainLink")
            .field("id", &self.id)
            .field("chain_id", &self.chain_id)
            .field("position", &self.position)
            .field("prompt_id", &self.prompt_id)
            .field("prompt_version_id", &self.prompt_version_id)
            .finish()
    }
}

impl Serialize for PromptChainLink {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        PromptChainLinkWire {
            id: self.id,
            chain_id: self.chain_id,
            position: self.position,
            prompt_id: self.prompt_id,
            prompt_version_id: self.prompt_version_id,
        }
        .serialize(serializer)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PromptChainLinkContext {
    pub link: PromptChainLink,
    pub previous_link_id: Option<PromptChainLinkId>,
    pub next_link_id: Option<PromptChainLinkId>,
    pub update_available: bool,
}

#[derive(Clone, PartialEq, Eq, Deserialize)]
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

impl Serialize for CreatePromptChain {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.validate().map_err(S::Error::custom)?;
        validate_canonical_chain_metadata(&self.title, self.description.as_deref())
            .map_err(S::Error::custom)?;
        CreatePromptChainWire {
            chain_id: self.chain_id,
            title: self.title.clone(),
            description: self.description.clone(),
            created_at_ms: self.created_at_ms,
        }
        .serialize(serializer)
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

impl fmt::Debug for CreatePromptChain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CreatePromptChain")
            .field("chain_id", &self.chain_id)
            .field("title_bytes", &self.title.len())
            .field(
                "description_bytes",
                &self.description.as_deref().map(str::len),
            )
            .field("created_at_ms", &self.created_at_ms)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, try_from = "RenamePromptChainWire")]
pub struct RenamePromptChain {
    pub chain_id: PromptChainId,
    pub title: String,
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

impl fmt::Debug for RenamePromptChain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RenamePromptChain")
            .field("chain_id", &self.chain_id)
            .field("title_bytes", &self.title.len())
            .field("expected_revision", &self.expected_revision)
            .finish()
    }
}

impl Serialize for RenamePromptChain {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.validate().map_err(S::Error::custom)?;
        validate_canonical_title(&self.title, MAX_PROMPT_CHAIN_TITLE_SCALARS)
            .map_err(S::Error::custom)?;
        RenamePromptChainWire {
            chain_id: self.chain_id,
            title: self.title.clone(),
            expected_revision: self.expected_revision,
        }
        .serialize(serializer)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, try_from = "InsertPromptChainLinkSerializeWire")]
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

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, try_from = "MovePromptChainLinkSerializeWire")]
pub struct MovePromptChainLink {
    pub chain_id: PromptChainId,
    pub link_id: PromptChainLinkId,
    pub before_link_id: Option<PromptChainLinkId>,
    pub expected_revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, try_from = "RemovePromptChainLinkSerializeWire")]
pub struct RemovePromptChainLink {
    pub chain_id: PromptChainId,
    pub link_id: PromptChainLinkId,
    pub expected_revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(
    deny_unknown_fields,
    try_from = "UpdatePromptChainLinkVersionSerializeWire"
)]
pub struct UpdatePromptChainLinkVersion {
    pub chain_id: PromptChainId,
    pub link_id: PromptChainLinkId,
    pub expected_revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, try_from = "ArchivePromptChainSerializeWire")]
pub struct ArchivePromptChain {
    pub chain_id: PromptChainId,
    pub archived_at_ms: i64,
    pub expected_revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, try_from = "RestorePromptChainSerializeWire")]
pub struct RestorePromptChain {
    pub chain_id: PromptChainId,
    pub expected_revision: u64,
}

impl TryFrom<InsertPromptChainLinkSerializeWire> for InsertPromptChainLink {
    type Error = String;

    fn try_from(wire: InsertPromptChainLinkSerializeWire) -> Result<Self, Self::Error> {
        validate_expected_revision(wire.expected_revision).map_err(|error| error.to_string())?;
        Ok(Self {
            chain_id: wire.chain_id,
            link_id: wire.link_id,
            prompt_id: wire.prompt_id,
            prompt_version_id: wire.prompt_version_id,
            before_link_id: wire.before_link_id,
            expected_revision: wire.expected_revision,
        })
    }
}

impl TryFrom<MovePromptChainLinkSerializeWire> for MovePromptChainLink {
    type Error = String;

    fn try_from(wire: MovePromptChainLinkSerializeWire) -> Result<Self, Self::Error> {
        validate_expected_revision(wire.expected_revision).map_err(|error| error.to_string())?;
        Ok(Self {
            chain_id: wire.chain_id,
            link_id: wire.link_id,
            before_link_id: wire.before_link_id,
            expected_revision: wire.expected_revision,
        })
    }
}

impl TryFrom<RemovePromptChainLinkSerializeWire> for RemovePromptChainLink {
    type Error = String;

    fn try_from(wire: RemovePromptChainLinkSerializeWire) -> Result<Self, Self::Error> {
        validate_expected_revision(wire.expected_revision).map_err(|error| error.to_string())?;
        Ok(Self {
            chain_id: wire.chain_id,
            link_id: wire.link_id,
            expected_revision: wire.expected_revision,
        })
    }
}

impl TryFrom<UpdatePromptChainLinkVersionSerializeWire> for UpdatePromptChainLinkVersion {
    type Error = String;

    fn try_from(wire: UpdatePromptChainLinkVersionSerializeWire) -> Result<Self, Self::Error> {
        validate_expected_revision(wire.expected_revision).map_err(|error| error.to_string())?;
        Ok(Self {
            chain_id: wire.chain_id,
            link_id: wire.link_id,
            expected_revision: wire.expected_revision,
        })
    }
}

impl TryFrom<ArchivePromptChainSerializeWire> for ArchivePromptChain {
    type Error = String;

    fn try_from(wire: ArchivePromptChainSerializeWire) -> Result<Self, Self::Error> {
        validate_expected_revision(wire.expected_revision).map_err(|error| error.to_string())?;
        Ok(Self {
            chain_id: wire.chain_id,
            archived_at_ms: wire.archived_at_ms,
            expected_revision: wire.expected_revision,
        })
    }
}

impl TryFrom<RestorePromptChainSerializeWire> for RestorePromptChain {
    type Error = String;

    fn try_from(wire: RestorePromptChainSerializeWire) -> Result<Self, Self::Error> {
        validate_expected_revision(wire.expected_revision).map_err(|error| error.to_string())?;
        Ok(Self {
            chain_id: wire.chain_id,
            expected_revision: wire.expected_revision,
        })
    }
}

impl Serialize for InsertPromptChainLink {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        validate_expected_revision(self.expected_revision).map_err(S::Error::custom)?;
        InsertPromptChainLinkSerializeWire {
            chain_id: self.chain_id,
            link_id: self.link_id,
            prompt_id: self.prompt_id,
            prompt_version_id: self.prompt_version_id,
            before_link_id: self.before_link_id,
            expected_revision: self.expected_revision,
        }
        .serialize(serializer)
    }
}

impl Serialize for MovePromptChainLink {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        validate_expected_revision(self.expected_revision).map_err(S::Error::custom)?;
        MovePromptChainLinkSerializeWire {
            chain_id: self.chain_id,
            link_id: self.link_id,
            before_link_id: self.before_link_id,
            expected_revision: self.expected_revision,
        }
        .serialize(serializer)
    }
}

impl Serialize for RemovePromptChainLink {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        validate_expected_revision(self.expected_revision).map_err(S::Error::custom)?;
        RemovePromptChainLinkSerializeWire {
            chain_id: self.chain_id,
            link_id: self.link_id,
            expected_revision: self.expected_revision,
        }
        .serialize(serializer)
    }
}

impl Serialize for UpdatePromptChainLinkVersion {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        validate_expected_revision(self.expected_revision).map_err(S::Error::custom)?;
        UpdatePromptChainLinkVersionSerializeWire {
            chain_id: self.chain_id,
            link_id: self.link_id,
            expected_revision: self.expected_revision,
        }
        .serialize(serializer)
    }
}

impl Serialize for ArchivePromptChain {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        validate_expected_revision(self.expected_revision).map_err(S::Error::custom)?;
        ArchivePromptChainSerializeWire {
            chain_id: self.chain_id,
            archived_at_ms: self.archived_at_ms,
            expected_revision: self.expected_revision,
        }
        .serialize(serializer)
    }
}

impl Serialize for RestorePromptChain {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        validate_expected_revision(self.expected_revision).map_err(S::Error::custom)?;
        RestorePromptChainSerializeWire {
            chain_id: self.chain_id,
            expected_revision: self.expected_revision,
        }
        .serialize(serializer)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
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
    pub fn canonicalize(&self) -> Result<Self, PromptValidationError> {
        match self {
            Self::CreatePromptChain(command) => {
                command.validate()?;
                let mut canonical = command.clone();
                canonical.title = trim_prompt_whitespace(&command.title).to_string();
                canonical.description = command
                    .description
                    .as_deref()
                    .map(trim_prompt_whitespace)
                    .map(str::to_string);
                Ok(Self::CreatePromptChain(canonical))
            }
            Self::RenamePromptChain(command) => {
                command.validate()?;
                let mut canonical = command.clone();
                canonical.title = trim_prompt_whitespace(&command.title).to_string();
                Ok(Self::RenamePromptChain(canonical))
            }
            Self::InsertPromptChainLink(command) => {
                validate_expected_revision(command.expected_revision)?;
                Ok(Self::InsertPromptChainLink(command.clone()))
            }
            Self::MovePromptChainLink(command) => {
                validate_expected_revision(command.expected_revision)?;
                Ok(Self::MovePromptChainLink(command.clone()))
            }
            Self::RemovePromptChainLink(command) => {
                validate_expected_revision(command.expected_revision)?;
                Ok(Self::RemovePromptChainLink(command.clone()))
            }
            Self::UpdatePromptChainLinkVersion(command) => {
                validate_expected_revision(command.expected_revision)?;
                Ok(Self::UpdatePromptChainLinkVersion(command.clone()))
            }
            Self::ArchivePromptChain(command) => {
                validate_expected_revision(command.expected_revision)?;
                Ok(Self::ArchivePromptChain(command.clone()))
            }
            Self::RestorePromptChain(command) => {
                validate_expected_revision(command.expected_revision)?;
                Ok(Self::RestorePromptChain(command.clone()))
            }
        }
    }

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

    pub fn fingerprint(&self) -> Result<[u8; 32], PromptCodecError> {
        let canonical = self
            .canonicalize()
            .map_err(|_| PromptCodecError("prompt chain command validation failed".into()))?;
        canonical.fingerprint_canonical()
    }

    /// Hash the exact canonical wire bytes used for persistence and execution.
    pub(crate) fn fingerprint_canonical(&self) -> Result<[u8; 32], PromptCodecError> {
        let encoded = self.encode_canonical()?;
        let fingerprint = Sha256::digest(encoded).into();
        Ok(fingerprint)
    }

    pub fn encode(&self) -> Result<Vec<u8>, PromptCodecError> {
        self.validate()
            .map_err(|_| PromptCodecError("prompt chain command validation failed".into()))?;
        validate_chain_command_canonical(self)
            .map_err(|_| PromptCodecError("prompt chain command validation failed".into()))?;
        self.encode_canonical()
    }

    fn encode_canonical(&self) -> Result<Vec<u8>, PromptCodecError> {
        bounded_wire_encode(
            &PromptChainCommandWire {
                schema_version: PROMPT_WIRE_SCHEMA_VERSION,
                command: self,
            },
            "prompt chain command encoding failed",
        )
    }

    pub fn decode(payload: &[u8]) -> Result<Self, PromptCodecError> {
        validate_wire_payload_size(payload)?;
        let wire: PromptChainCommandWire<PromptChainCommand> = rmp_serde::from_slice(payload)
            .map_err(|_| PromptCodecError("prompt chain command decoding failed".into()))?;
        if wire.schema_version != PROMPT_WIRE_SCHEMA_VERSION {
            return Err(PromptCodecError(
                "unsupported prompt chain command schema".into(),
            ));
        }
        wire.command
            .validate()
            .map_err(|_| PromptCodecError("prompt chain command validation failed".into()))?;
        let canonical = wire.command.encode()?;
        if canonical != payload {
            return Err(PromptCodecError(
                "prompt chain command payload is not canonical".into(),
            ));
        }
        Ok(wire.command)
    }

    /// Encode the durable envelope using this command as both the original and
    /// normalized command. Store callers use the companion method when
    /// resolution changes the command before it is persisted.
    pub(crate) fn encode_durable(
        &self,
        resolved_prompt_version_id: Option<PromptVersionId>,
    ) -> Result<Vec<u8>, PromptCodecError> {
        self.encode_durable_with_original(self, resolved_prompt_version_id)
    }

    pub(crate) fn encode_durable_with_original(
        &self,
        original_command: &Self,
        resolved_prompt_version_id: Option<PromptVersionId>,
    ) -> Result<Vec<u8>, PromptCodecError> {
        self.validate()
            .map_err(|_| PromptCodecError("prompt chain command validation failed".into()))?;
        original_command.validate().map_err(|_| {
            PromptCodecError("original prompt chain command validation failed".into())
        })?;
        validate_chain_command_canonical(self)
            .map_err(|_| PromptCodecError("prompt chain command validation failed".into()))?;
        validate_chain_command_canonical(original_command).map_err(|_| {
            PromptCodecError("original prompt chain command validation failed".into())
        })?;
        validate_chain_command_resolution_model(self, resolved_prompt_version_id)
            .map_err(|_| PromptCodecError("prompt chain command resolution failed".into()))?;
        validate_chain_command_original_resolution_model(
            original_command,
            self,
            resolved_prompt_version_id,
        )
        .map_err(|_| PromptCodecError("original prompt chain command resolution failed".into()))?;
        let original_command_payload = original_command.encode()?;
        let original_command_sha256 = Sha256::digest(&original_command_payload).into();
        bounded_wire_encode(
            &PromptChainCommandDurableWire {
                schema_version: PROMPT_DURABLE_CHAIN_WIRE_SCHEMA_VERSION,
                original_command_sha256,
                original_command_payload,
                command: self,
                resolved_prompt_version_id,
            },
            "prompt chain command encoding failed",
        )
    }

    pub(crate) fn decode_durable(
        payload: &[u8],
    ) -> Result<(Self, [u8; 32], Self, Option<PromptVersionId>), PromptCodecError> {
        validate_wire_payload_size(payload)?;
        let wire: PromptChainCommandDurableWire<PromptChainCommand> =
            rmp_serde::from_slice(payload)
                .map_err(|_| PromptCodecError("prompt chain command decoding failed".into()))?;
        if wire.schema_version != PROMPT_DURABLE_CHAIN_WIRE_SCHEMA_VERSION {
            return Err(PromptCodecError(
                "unsupported prompt chain command schema".into(),
            ));
        }
        validate_wire_payload_size(&wire.original_command_payload).map_err(|_| {
            PromptCodecError("original prompt chain command payload is invalid".into())
        })?;
        if wire.original_command_payload.is_empty() {
            return Err(PromptCodecError(
                "original prompt chain command payload is empty".into(),
            ));
        }
        let original_command = Self::decode(&wire.original_command_payload).map_err(|_| {
            PromptCodecError("original prompt chain command payload is invalid".into())
        })?;
        let canonical_original_payload = original_command.encode()?;
        if canonical_original_payload != wire.original_command_payload {
            return Err(PromptCodecError(
                "original prompt chain command payload is not canonical".into(),
            ));
        }
        let original_command_sha256: [u8; 32] =
            Sha256::digest(&wire.original_command_payload).into();
        if original_command_sha256 != wire.original_command_sha256 {
            return Err(PromptCodecError(
                "original prompt chain command hash does not match payload".into(),
            ));
        }
        wire.command
            .validate()
            .map_err(|_| PromptCodecError("prompt chain command validation failed".into()))?;
        validate_chain_command_canonical(&wire.command)
            .map_err(|_| PromptCodecError("prompt chain command validation failed".into()))?;
        validate_chain_command_resolution_model(&wire.command, wire.resolved_prompt_version_id)
            .map_err(|_| PromptCodecError("prompt chain command resolution failed".into()))?;
        validate_chain_command_original_resolution_model(
            &original_command,
            &wire.command,
            wire.resolved_prompt_version_id,
        )
        .map_err(|_| PromptCodecError("original prompt chain command resolution failed".into()))?;
        let canonical = wire
            .command
            .encode_durable_with_original(&original_command, wire.resolved_prompt_version_id)?;
        if canonical != payload {
            return Err(PromptCodecError(
                "prompt chain command payload is not canonical".into(),
            ));
        }
        Ok((
            original_command,
            original_command_sha256,
            wire.command,
            wire.resolved_prompt_version_id,
        ))
    }
}

impl Serialize for PromptChainCommand {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        validate_chain_command_canonical(self).map_err(S::Error::custom)?;
        let wire = match self {
            Self::CreatePromptChain(command) => {
                PromptChainCommandSerializeWire::CreatePromptChain(command)
            }
            Self::RenamePromptChain(command) => {
                PromptChainCommandSerializeWire::RenamePromptChain(command)
            }
            Self::InsertPromptChainLink(command) => {
                PromptChainCommandSerializeWire::InsertPromptChainLink(command)
            }
            Self::MovePromptChainLink(command) => {
                PromptChainCommandSerializeWire::MovePromptChainLink(command)
            }
            Self::RemovePromptChainLink(command) => {
                PromptChainCommandSerializeWire::RemovePromptChainLink(command)
            }
            Self::UpdatePromptChainLinkVersion(command) => {
                PromptChainCommandSerializeWire::UpdatePromptChainLinkVersion(command)
            }
            Self::ArchivePromptChain(command) => {
                PromptChainCommandSerializeWire::ArchivePromptChain(command)
            }
            Self::RestorePromptChain(command) => {
                PromptChainCommandSerializeWire::RestorePromptChain(command)
            }
        };
        wire.serialize(serializer)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, try_from = "PromptChainMutationReceiptEnvelope")]
pub struct PromptChainMutationReceipt {
    pub command_id: CommandId,
    pub chain_id: PromptChainId,
    pub link_id: Option<PromptChainLinkId>,
    pub revision: u64,
}

impl TryFrom<PromptChainMutationReceiptEnvelope> for PromptChainMutationReceipt {
    type Error = String;

    fn try_from(envelope: PromptChainMutationReceiptEnvelope) -> Result<Self, Self::Error> {
        if envelope.schema_version != PROMPT_WIRE_SCHEMA_VERSION {
            return Err("unsupported prompt chain receipt schema".into());
        }
        let wire = envelope.receipt;
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
        if self.revision == 0 {
            return Err(PromptCodecError(
                "prompt chain receipt validation failed".into(),
            ));
        }
        bounded_wire_encode(
            &PromptChainMutationReceiptEnvelope {
                schema_version: PROMPT_WIRE_SCHEMA_VERSION,
                receipt: PromptChainMutationReceiptWire {
                    command_id: self.command_id,
                    chain_id: self.chain_id,
                    link_id: self.link_id,
                    revision: self.revision,
                },
            },
            "prompt chain receipt encoding failed",
        )
    }

    pub fn decode(payload: &[u8]) -> Result<Self, PromptCodecError> {
        validate_wire_payload_size(payload)?;
        let envelope: PromptChainMutationReceiptEnvelope = rmp_serde::from_slice(payload)
            .map_err(|_| PromptCodecError("prompt chain receipt decoding failed".into()))?;
        let receipt = Self::try_from(envelope)
            .map_err(|_| PromptCodecError("prompt chain receipt validation failed".into()))?;
        if receipt.encode()? != payload {
            return Err(PromptCodecError(
                "prompt chain receipt payload is not canonical".into(),
            ));
        }
        Ok(receipt)
    }
}

impl Serialize for PromptChainMutationReceipt {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        if self.revision == 0 {
            return Err(S::Error::custom(
                "prompt chain receipt revision must be positive",
            ));
        }
        PromptChainMutationReceiptEnvelope {
            schema_version: PROMPT_WIRE_SCHEMA_VERSION,
            receipt: PromptChainMutationReceiptWire {
                command_id: self.command_id,
                chain_id: self.chain_id,
                link_id: self.link_id,
                revision: self.revision,
            },
        }
        .serialize(serializer)
    }
}

#[derive(Clone, PartialEq, Eq, Deserialize)]
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
            } => {
                if links.len() > MAX_PROMPT_CHAIN_LINKS {
                    return Err("prompt chain contains too many links".into());
                }
                let mut checked_links = Vec::with_capacity(links.len());
                for link in links {
                    checked_links.push(PromptChainLink::store_issued(
                        link.id,
                        link.chain_id,
                        link.position,
                        link.prompt_id,
                        link.prompt_version_id,
                    ));
                }
                Self::PromptChainLinksReplaced {
                    chain_id,
                    links: checked_links,
                    revision,
                }
            }
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
        validate_chain_event_wire(self)
            .map_err(|_| PromptCodecError("prompt chain event validation failed".into()))?;
        bounded_wire_encode(
            &PromptChainEventWire {
                schema_version: PROMPT_WIRE_SCHEMA_VERSION,
                event: self,
            },
            "prompt chain event encoding failed",
        )
    }

    /// Decode a structurally checked event. Link ownership and prompt/version
    /// lineage remain a store concern and are not settled by this codec.
    pub fn decode(payload: &[u8]) -> Result<Self, PromptCodecError> {
        validate_wire_payload_size(payload)?;
        let wire: PromptChainEventWire<PromptChainEventSerde> = rmp_serde::from_slice(payload)
            .map_err(|_| PromptCodecError("prompt chain event decoding failed".into()))?;
        if wire.schema_version != PROMPT_WIRE_SCHEMA_VERSION {
            return Err(PromptCodecError(
                "unsupported prompt chain event schema".into(),
            ));
        }
        let event = PromptChainEvent::try_from(wire.event)
            .map_err(|_| PromptCodecError("prompt chain event validation failed".into()))?;
        let canonical = event.encode()?;
        if canonical != payload {
            return Err(PromptCodecError(
                "prompt chain event payload is not canonical".into(),
            ));
        }
        Ok(event)
    }
}

impl Serialize for PromptChainEvent {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        validate_chain_event_wire(self).map_err(S::Error::custom)?;
        let wire = match self {
            Self::PromptChainCreated { chain } => {
                PromptChainEventSerializeWire::PromptChainCreated { chain }
            }
            Self::PromptChainRenamed {
                chain_id,
                title,
                revision,
            } => PromptChainEventSerializeWire::PromptChainRenamed {
                chain_id: *chain_id,
                title,
                revision: *revision,
            },
            Self::PromptChainLinksReplaced {
                chain_id,
                links,
                revision,
            } => PromptChainEventSerializeWire::PromptChainLinksReplaced {
                chain_id: *chain_id,
                links,
                revision: *revision,
            },
            Self::PromptChainArchived {
                chain_id,
                archived_at_ms,
                revision,
            } => PromptChainEventSerializeWire::PromptChainArchived {
                chain_id: *chain_id,
                archived_at_ms: *archived_at_ms,
                revision: *revision,
            },
            Self::PromptChainRestored { chain_id, revision } => {
                PromptChainEventSerializeWire::PromptChainRestored {
                    chain_id: *chain_id,
                    revision: *revision,
                }
            }
        };
        wire.serialize(serializer)
    }
}

impl fmt::Debug for PromptChainEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PromptChainCreated { chain } => f
                .debug_struct("PromptChainEvent::PromptChainCreated")
                .field("chain", chain)
                .finish(),
            Self::PromptChainRenamed {
                chain_id,
                title,
                revision,
            } => f
                .debug_struct("PromptChainEvent::PromptChainRenamed")
                .field("chain_id", chain_id)
                .field("title_bytes", &title.len())
                .field("revision", revision)
                .finish(),
            Self::PromptChainLinksReplaced {
                chain_id,
                links,
                revision,
            } => f
                .debug_struct("PromptChainEvent::PromptChainLinksReplaced")
                .field("chain_id", chain_id)
                .field("link_count", &links.len())
                .field("revision", revision)
                .finish(),
            Self::PromptChainArchived {
                chain_id,
                archived_at_ms,
                revision,
            } => f
                .debug_struct("PromptChainEvent::PromptChainArchived")
                .field("chain_id", chain_id)
                .field("archived_at_ms", archived_at_ms)
                .field("revision", revision)
                .finish(),
            Self::PromptChainRestored { chain_id, revision } => f
                .debug_struct("PromptChainEvent::PromptChainRestored")
                .field("chain_id", chain_id)
                .field("revision", revision)
                .finish(),
        }
    }
}

fn validate_expected_revision(revision: u64) -> Result<(), PromptValidationError> {
    if revision == 0 {
        Err(PromptValidationError::ExpectedRevisionZero)
    } else {
        Ok(())
    }
}

fn validate_wire_payload_size(payload: &[u8]) -> Result<(), PromptCodecError> {
    if payload.len() > MAX_PROMPT_WIRE_BYTES {
        return Err(PromptCodecError(
            "prompt wire payload exceeds configured limit".into(),
        ));
    }
    MessagePackPreflight::new(payload).run()
}

fn bounded_wire_encode<T: Serialize>(
    value: &T,
    error_message: &'static str,
) -> Result<Vec<u8>, PromptCodecError> {
    let payload =
        rmp_serde::to_vec_named(value).map_err(|_| PromptCodecError(error_message.into()))?;
    if payload.len() > MAX_PROMPT_WIRE_BYTES {
        return Err(PromptCodecError(
            "prompt wire payload exceeds configured limit".into(),
        ));
    }
    Ok(payload)
}

struct MessagePackPreflight<'a> {
    payload: &'a [u8],
    offset: usize,
    depth: usize,
    nodes: usize,
}

impl<'a> MessagePackPreflight<'a> {
    fn new(payload: &'a [u8]) -> Self {
        Self {
            payload,
            offset: 0,
            depth: 0,
            nodes: 0,
        }
    }

    fn run(mut self) -> Result<(), PromptCodecError> {
        self.visit_value(0)?;
        if self.offset != self.payload.len() {
            return Err(Self::error());
        }
        Ok(())
    }

    fn visit_value(&mut self, depth: usize) -> Result<(), PromptCodecError> {
        if depth > MAX_PROMPT_WIRE_DEPTH {
            return Err(Self::error());
        }
        self.depth = self.depth.max(depth);
        self.nodes = self.nodes.checked_add(1).ok_or_else(Self::error)?;
        if self.nodes > MAX_PROMPT_WIRE_NODES {
            return Err(Self::error());
        }
        let marker = self.read_u8()?;
        match marker {
            0x00..=0x7f | 0xe0..=0xff | 0xc0 | 0xc2 | 0xc3 => Ok(()),
            0xc1 => Err(Self::error()),
            0xcc | 0xd0 => self.skip(1),
            0xcd | 0xd1 => self.skip(2),
            0xce | 0xd2 => self.skip(4),
            0xcf | 0xd3 => self.skip(8),
            0xca => self.skip(4),
            0xcb => self.skip(8),
            0xd4 => self.skip_ext(1),
            0xd5 => self.skip_ext(2),
            0xd6 => self.skip_ext(4),
            0xd7 => self.skip_ext(8),
            0xd8 => self.skip_ext(16),
            0xc7 => {
                let length = self.read_u8()? as usize;
                self.visit_ext(length)
            }
            0xc8 => {
                let length = self.read_u16()? as usize;
                self.visit_ext(length)
            }
            0xc9 => {
                let length = self.read_u32()? as usize;
                self.visit_ext(length)
            }
            0xa0..=0xbf => self.visit_string((marker & 0x1f) as usize),
            0xd9 => {
                let length = self.read_u8()? as usize;
                self.visit_string(length)
            }
            0xda => {
                let length = self.read_u16()? as usize;
                self.visit_string(length)
            }
            0xdb => {
                let length = self.read_u32()? as usize;
                self.visit_string(length)
            }
            0xc4 => {
                let length = self.read_u8()? as usize;
                self.visit_bin(length)
            }
            0xc5 => {
                let length = self.read_u16()? as usize;
                self.visit_bin(length)
            }
            0xc6 => {
                let length = self.read_u32()? as usize;
                self.visit_bin(length)
            }
            0x90..=0x9f => self.visit_array((marker & 0x0f) as usize, depth + 1),
            0xdc => {
                let count = self.read_u16()? as usize;
                self.visit_array(count, depth + 1)
            }
            0xdd => {
                let count = self.read_u32()? as usize;
                self.visit_array(count, depth + 1)
            }
            0x80..=0x8f => self.visit_map((marker & 0x0f) as usize, depth + 1),
            0xde => {
                let count = self.read_u16()? as usize;
                self.visit_map(count, depth + 1)
            }
            0xdf => {
                let count = self.read_u32()? as usize;
                self.visit_map(count, depth + 1)
            }
        }
    }

    fn visit_array(&mut self, count: usize, depth: usize) -> Result<(), PromptCodecError> {
        if count > MAX_PROMPT_WIRE_COLLECTION_ITEMS {
            return Err(Self::error());
        }
        for _ in 0..count {
            self.visit_value(depth)?;
        }
        Ok(())
    }

    fn visit_map(&mut self, count: usize, depth: usize) -> Result<(), PromptCodecError> {
        if count > MAX_PROMPT_WIRE_MAP_ENTRIES {
            return Err(Self::error());
        }
        let mut keys = HashSet::with_capacity(count);
        for _ in 0..count {
            let key = self.read_string()?.to_vec();
            if !keys.insert(key) {
                return Err(Self::error());
            }
            self.visit_value(depth)?;
        }
        Ok(())
    }

    fn visit_string(&mut self, length: usize) -> Result<(), PromptCodecError> {
        if length > MAX_PROMPT_WIRE_STRING_BYTES {
            return Err(Self::error());
        }
        let bytes = self.take(length)?;
        std::str::from_utf8(bytes).map_err(|_| Self::error())?;
        Ok(())
    }

    fn read_string(&mut self) -> Result<&'a [u8], PromptCodecError> {
        let marker = self.read_u8()?;
        let length = match marker {
            0xa0..=0xbf => (marker & 0x1f) as usize,
            0xd9 => self.read_u8()? as usize,
            0xda => self.read_u16()? as usize,
            0xdb => self.read_u32()? as usize,
            _ => return Err(Self::error()),
        };
        if length > MAX_PROMPT_WIRE_STRING_BYTES {
            return Err(Self::error());
        }
        let bytes = self.take(length)?;
        std::str::from_utf8(bytes).map_err(|_| Self::error())?;
        Ok(bytes)
    }

    fn visit_bin(&mut self, length: usize) -> Result<(), PromptCodecError> {
        if length > MAX_PROMPT_WIRE_BIN_BYTES {
            return Err(Self::error());
        }
        self.skip(length)
    }

    fn visit_ext(&mut self, length: usize) -> Result<(), PromptCodecError> {
        if length > MAX_PROMPT_WIRE_EXT_BYTES {
            return Err(Self::error());
        }
        self.skip_ext(length)
    }

    fn skip_ext(&mut self, length: usize) -> Result<(), PromptCodecError> {
        self.skip(length.checked_add(1).ok_or_else(Self::error)?)
    }

    fn skip(&mut self, length: usize) -> Result<(), PromptCodecError> {
        self.take(length).map(|_| ())
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8], PromptCodecError> {
        let end = self.offset.checked_add(length).ok_or_else(Self::error)?;
        if end > self.payload.len() {
            return Err(Self::error());
        }
        let bytes = &self.payload[self.offset..end];
        self.offset = end;
        Ok(bytes)
    }

    fn read_u8(&mut self) -> Result<u8, PromptCodecError> {
        Ok(*self.take(1)?.first().ok_or_else(Self::error)?)
    }

    fn read_u16(&mut self) -> Result<u16, PromptCodecError> {
        let bytes = self.take(2)?;
        Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    fn read_u32(&mut self) -> Result<u32, PromptCodecError> {
        let bytes = self.take(4)?;
        Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn error() -> PromptCodecError {
        PromptCodecError("prompt wire payload is malformed or exceeds bounds".into())
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
    validate_saved_prompt_values(
        wire.revision,
        &wire.title,
        wire.description.as_deref(),
        &wire.tags,
    )
}

fn validate_saved_prompt_values(
    revision: u64,
    title: &str,
    description: Option<&str>,
    tags: &[String],
) -> Result<(), String> {
    if revision == 0 {
        return Err("saved prompt revision must be positive".into());
    }
    validate_canonical_prompt_metadata(title, description, tags, &[])
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

fn validate_prompt_command_canonical(command: &PromptCommand) -> Result<(), String> {
    match command {
        PromptCommand::CreatePrompt(command) => validate_prompt_command_wire(command),
        PromptCommand::CreatePromptVersion(command) => {
            validate_canonical_variables(&command.variables)
        }
        PromptCommand::RenamePrompt(command) => {
            validate_canonical_title(&command.title, MAX_PROMPT_TITLE_SCALARS)
        }
        PromptCommand::SetPromptTags(command) => validate_canonical_tags(&command.tags),
        PromptCommand::ArchivePrompt(_) | PromptCommand::RestorePrompt(_) => Ok(()),
    }
}

fn validate_chain_command_canonical(command: &PromptChainCommand) -> Result<(), String> {
    match command {
        PromptChainCommand::CreatePromptChain(command) => {
            command.validate().map_err(|error| error.to_string())?;
            validate_canonical_chain_metadata(&command.title, command.description.as_deref())
        }
        PromptChainCommand::RenamePromptChain(command) => {
            command.validate().map_err(|error| error.to_string())?;
            validate_canonical_title(&command.title, MAX_PROMPT_CHAIN_TITLE_SCALARS)
        }
        PromptChainCommand::InsertPromptChainLink(command) => {
            validate_expected_revision(command.expected_revision).map_err(|error| error.to_string())
        }
        PromptChainCommand::MovePromptChainLink(command) => {
            validate_expected_revision(command.expected_revision).map_err(|error| error.to_string())
        }
        PromptChainCommand::RemovePromptChainLink(command) => {
            validate_expected_revision(command.expected_revision).map_err(|error| error.to_string())
        }
        PromptChainCommand::UpdatePromptChainLinkVersion(command) => {
            validate_expected_revision(command.expected_revision).map_err(|error| error.to_string())
        }
        PromptChainCommand::ArchivePromptChain(command) => {
            validate_expected_revision(command.expected_revision).map_err(|error| error.to_string())
        }
        PromptChainCommand::RestorePromptChain(command) => {
            validate_expected_revision(command.expected_revision).map_err(|error| error.to_string())
        }
    }
}

fn validate_chain_command_resolution_model(
    command: &PromptChainCommand,
    resolved_prompt_version_id: Option<PromptVersionId>,
) -> Result<(), String> {
    match command {
        PromptChainCommand::InsertPromptChainLink(command) => {
            if command.prompt_version_id.is_none()
                || command.prompt_version_id != resolved_prompt_version_id
            {
                return Err(
                    "prompt chain insert command has no exact resolved prompt version".into(),
                );
            }
        }
        PromptChainCommand::UpdatePromptChainLinkVersion(_) => {
            if resolved_prompt_version_id.is_none() {
                return Err(
                    "prompt chain update command has no exact resolved prompt version".into(),
                );
            }
        }
        PromptChainCommand::CreatePromptChain(_)
        | PromptChainCommand::RenamePromptChain(_)
        | PromptChainCommand::MovePromptChainLink(_)
        | PromptChainCommand::RemovePromptChainLink(_)
        | PromptChainCommand::ArchivePromptChain(_)
        | PromptChainCommand::RestorePromptChain(_) => {
            if resolved_prompt_version_id.is_some() {
                return Err(
                    "prompt chain command has an unexpected resolved prompt version".into(),
                );
            }
        }
    }
    Ok(())
}

fn validate_chain_command_original_resolution_model(
    original_command: &PromptChainCommand,
    command: &PromptChainCommand,
    resolved_prompt_version_id: Option<PromptVersionId>,
) -> Result<(), String> {
    match (original_command, command) {
        (
            PromptChainCommand::InsertPromptChainLink(original),
            PromptChainCommand::InsertPromptChainLink(command),
        ) => {
            if original.chain_id != command.chain_id
                || original.link_id != command.link_id
                || original.prompt_id != command.prompt_id
                || original.before_link_id != command.before_link_id
                || original.expected_revision != command.expected_revision
                || original
                    .prompt_version_id
                    .is_some_and(|version_id| Some(version_id) != command.prompt_version_id)
                || command.prompt_version_id != resolved_prompt_version_id
            {
                return Err("prompt chain insert command resolution changed its payload".into());
            }
        }
        _ if original_command != command => {
            return Err("prompt chain command resolution changed its payload".into());
        }
        _ => {}
    }
    Ok(())
}

fn validate_prompt_event_wire(event: &PromptEvent) -> Result<(), String> {
    match event {
        PromptEvent::PromptCreated { prompt, version } => {
            validate_saved_prompt_values(
                prompt.revision,
                &prompt.title,
                prompt.description.as_deref(),
                &prompt.tags,
            )?;
            validate_prompt_version_wire(version)?;
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
            version,
            prompt_id,
            revision,
        } => {
            validate_prompt_version_wire(version)?;
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
            if links.len() > MAX_PROMPT_CHAIN_LINKS {
                return Err("prompt chain contains too many links".into());
            }
            let mut seen = HashSet::with_capacity(links.len());
            for (position, link) in links.iter().enumerate() {
                let expected_position =
                    u32::try_from(position).map_err(|_| "prompt chain is too long".to_string())?;
                if link.chain_id() != *chain_id
                    || link.position() != expected_position
                    || !seen.insert(link.id())
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
