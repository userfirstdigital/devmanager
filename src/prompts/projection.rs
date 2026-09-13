//! Transport-neutral personal prompt library projection.
//!
//! Frames are host-local / future owner-device E2E payloads. They are not
//! Connect persistence DTOs and never upload personal prompts.

use std::io::{self, Write};

use hmac::{Hmac, Mac};
use serde::de::{self, MapAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};

use crate::domain::id::{
    ClientId, CommandId, PromptChainId, PromptChainLinkId, PromptHistoryId, PromptId,
    PromptVersionId, RequestId, TaskId,
};
use crate::prompts::diff::{diff_versions, encode_public_diff};
use crate::prompts::model::{
    PromptChain, PromptMutationReceipt, PromptVersion, SavedPrompt, MAX_PROMPT_CHAIN_LINKS,
    MAX_PROMPT_DESCRIPTION_SCALARS, MAX_PROMPT_TAGS, MAX_PROMPT_TAG_SCALARS,
    MAX_PROMPT_TITLE_SCALARS,
};
use crate::prompts::store::PromptStore;

/// Same bit as [`crate::protocol::Capability::PromptProjection`].
pub const PERSONAL_PROMPT_LIBRARY_BIT: u64 = 1_u64 << 8;
pub const PROMPT_PROJECTION_SCHEMA_VERSION: u32 = 1;
pub const PROMPT_METADATA_PAGE_ITEMS: usize = 100;
pub const PROMPT_METADATA_PAGE_BYTES: usize = 512 * 1024;
pub const PROMPT_BODY_CHUNK_BYTES: usize = 256 * 1024;
pub const PROMPT_TRANSFER_CEILING_BYTES: usize = 16 * 1024 * 1024;
pub const PROMPT_SEARCH_MAX_RESULTS: usize = 100;
pub const PROMPT_SEARCH_MAX_QUERY_BYTES: usize = 512;
const MAX_TEST_SOURCE_ITEMS: usize = 32;
const MAX_UNKNOWN_FIELDS: usize = 8;
const MAX_UNKNOWN_FIELD_NAME_BYTES: usize = 64;
const MAX_CODEC_DEPTH: usize = 16;
const MAX_CODEC_MAP_ENTRIES: usize = 64;
const MAX_CODEC_ARRAY_ITEMS: usize = MAX_PROMPT_CHAIN_LINKS;
const MAX_CODEC_STRING_BYTES: usize = 8 * 1024;
const MAX_CODEC_NODES: usize = 32_768;
const CURSOR_LEN: usize = 64;
const OWNER_GRANT_DOMAIN: &[u8] = b"devmanager.prompt.sealed-owner.v1\0";
const PAIRED_OWNER_GRANT_DOMAIN: &[u8] = b"devmanager.prompt.sealed-paired-owner.v1\0";

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptNamespace {
    Personal,
    Organization,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptPrivacyClass {
    LocalOnly,
    OrganizationReadOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptProjectionQueryKind {
    Metadata,
    Version,
    Diff,
    Search,
    Chain,
    History,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GenericFieldKind {
    Null,
    Bool,
    Number,
    String,
    Bytes,
    Array,
    Map,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GenericPromptField {
    name: String,
    kind: GenericFieldKind,
}

impl GenericPromptField {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn kind(&self) -> GenericFieldKind {
        self.kind
    }

    pub fn as_action_id(&self) -> Option<&str> {
        None
    }

    pub fn as_domain_transition(&self) -> Option<&str> {
        None
    }

    pub fn as_prompt_command(&self) -> Option<crate::prompts::PromptCommand> {
        None
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptProjectionError {
    PermissionDenied,
    StaleCursor,
    StaleRevision {
        expected: u64,
        actual: u64,
    },
    UnsupportedCapability,
    InvalidRequest,
    NotFound,
    NamespaceReadOnly,
    SearchQueryTooLong,
    TransferCeilingExceeded,
    CodecBound,
    DuplicateKey,
    SerializationFailure,
    CapExceeded,
    Unavailable {
        subsystem: PromptProjectionSubsystem,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptProjectionSubsystem {
    SearchIndex,
    HistoryStore,
    ChainDirectory,
    OrganizationNamespace,
    OwnerDeviceSession,
    NegotiatedTransportLimit,
}

#[derive(Clone, PartialEq, Eq)]
pub struct OwnerDeviceCapability {
    session: [u8; 16],
    binding: [u8; 32],
}

impl std::fmt::Debug for OwnerDeviceCapability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OwnerDeviceCapability")
            .field("session", &self.session)
            .finish_non_exhaustive()
    }
}

impl OwnerDeviceCapability {
    pub fn from_authenticated_session(_session: &[u8]) -> Result<Self, PromptProjectionError> {
        Err(PromptProjectionError::Unavailable {
            subsystem: PromptProjectionSubsystem::OwnerDeviceSession,
        })
    }

    fn bind_principal(client_id: ClientId, domain: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(domain);
        hasher.update(client_id.as_bytes());
        Self {
            session: *client_id.as_bytes(),
            binding: hasher.finalize().into(),
        }
    }

    fn bind_owner(client_id: ClientId) -> Self {
        Self::bind_principal(client_id, OWNER_GRANT_DOMAIN)
    }

    fn bind_paired_owner(client_id: ClientId) -> Self {
        Self::bind_principal(client_id, PAIRED_OWNER_GRANT_DOMAIN)
    }

    pub fn binds_client(&self, client_id: ClientId) -> bool {
        self.session == *client_id.as_bytes()
    }

    /// Host-only capability derived from the already-authenticated paired
    /// owner identity. The transport must perform the role/capability check
    /// before calling this constructor; it is never exposed on the wire.
    pub(crate) fn paired_owner_for_authenticated_client(client_id: ClientId) -> Self {
        Self::bind_paired_owner(client_id)
    }

    fn binding(&self) -> &[u8; 32] {
        &self.binding
    }
}

fn require_bound_client(
    capability: &OwnerDeviceCapability,
    client_id: ClientId,
) -> Result<(), PromptProjectionError> {
    if capability.binds_client(client_id) {
        Ok(())
    } else {
        Err(PromptProjectionError::PermissionDenied)
    }
}

#[doc(hidden)]
pub mod testing {
    use crate::domain::id::ClientId;

    use super::{OwnerDeviceCapability, PromptProjectionError};

    /// Sealed owner grant for tests. Hello intersection cannot mint this.
    pub fn owner_grant(
        client_id: ClientId,
    ) -> Result<OwnerDeviceCapability, PromptProjectionError> {
        Ok(OwnerDeviceCapability::bind_owner(client_id))
    }

    /// Sealed paired-owner grant for tests. Distinct binding from owner.
    pub fn paired_owner_grant(
        client_id: ClientId,
    ) -> Result<OwnerDeviceCapability, PromptProjectionError> {
        Ok(OwnerDeviceCapability::bind_paired_owner(client_id))
    }

    pub fn watcher_grant(
        _client_id: ClientId,
    ) -> Result<OwnerDeviceCapability, PromptProjectionError> {
        Err(PromptProjectionError::PermissionDenied)
    }

    pub fn collaborator_grant(
        _client_id: ClientId,
    ) -> Result<OwnerDeviceCapability, PromptProjectionError> {
        Err(PromptProjectionError::PermissionDenied)
    }

    pub fn serialization_failure_from_failing_payload() -> super::PromptProjectionError {
        super::encode_named(&FailingPayload).expect_err("forced serialize must fail closed")
    }

    struct FailingPayload;

    impl serde::Serialize for FailingPayload {
        fn serialize<S: serde::Serializer>(&self, _serializer: S) -> Result<S::Ok, S::Error> {
            Err(serde::ser::Error::custom(
                "forced prompt projection serialization failure",
            ))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptCursor {
    bytes: [u8; CURSOR_LEN],
}

impl PromptCursor {
    pub fn as_bytes(&self) -> &[u8; CURSOR_LEN] {
        &self.bytes
    }

    pub fn from_public_fields_for_adversary(
        _kind: PromptProjectionQueryKind,
        _namespace: PromptNamespace,
        _revision: u64,
        _last_id: Option<[u8; 16]>,
        _sequence: u32,
    ) -> Result<Self, PromptProjectionError> {
        Err(PromptProjectionError::InvalidRequest)
    }

    fn issue(
        capability: &OwnerDeviceCapability,
        kind: PromptProjectionQueryKind,
        namespace: PromptNamespace,
        revision: u64,
        last_id: Option<[u8; 16]>,
        sequence: u32,
        request_id: RequestId,
        binding: CursorBinding,
    ) -> Result<Self, PromptProjectionError> {
        let mut bytes = [0u8; CURSOR_LEN];
        bytes[0] = kind_byte(kind);
        bytes[1] = match namespace {
            PromptNamespace::Personal => 1,
            PromptNamespace::Organization => 2,
        };
        bytes[2..10].copy_from_slice(&revision.to_be_bytes());
        if let Some(id) = last_id {
            bytes[10..26].copy_from_slice(&id);
        }
        bytes[26..30].copy_from_slice(&sequence.to_be_bytes());
        bytes[30..46].copy_from_slice(request_id.as_bytes());
        let mac = cursor_mac(capability, &bytes[..46], binding)?;
        bytes[46..62].copy_from_slice(&mac);
        Ok(Self { bytes })
    }

    fn open(
        &self,
        capability: &OwnerDeviceCapability,
        kind: PromptProjectionQueryKind,
        namespace: PromptNamespace,
        revision: u64,
        binding: CursorBinding,
    ) -> Result<(Option<[u8; 16]>, u32), PromptProjectionError> {
        let mac = cursor_mac(capability, &self.bytes[..46], binding)?;
        if mac != self.bytes[46..62] {
            return Err(PromptProjectionError::StaleCursor);
        }
        if self.bytes[0] != kind_byte(kind) {
            return Err(PromptProjectionError::StaleCursor);
        }
        let ns = match self.bytes[1] {
            1 => PromptNamespace::Personal,
            2 => PromptNamespace::Organization,
            _ => return Err(PromptProjectionError::StaleCursor),
        };
        if ns != namespace {
            return Err(PromptProjectionError::StaleCursor);
        }
        let stored_revision = u64::from_be_bytes(self.bytes[2..10].try_into().unwrap());
        if stored_revision != revision {
            return Err(PromptProjectionError::StaleCursor);
        }
        let last = self.bytes[10..26].try_into().unwrap();
        let last_id = if last == [0u8; 16] { None } else { Some(last) };
        let sequence = u32::from_be_bytes(self.bytes[26..30].try_into().unwrap());
        Ok((last_id, sequence))
    }

    fn issued_request_id(&self) -> Result<RequestId, PromptProjectionError> {
        RequestId::from_bytes(self.bytes[30..46].try_into().unwrap())
            .map_err(|_| PromptProjectionError::StaleCursor)
    }

    fn require_kind(&self, kind: PromptProjectionQueryKind) -> Result<(), PromptProjectionError> {
        if self.bytes[0] != kind_byte(kind) {
            return Err(PromptProjectionError::StaleCursor);
        }
        Ok(())
    }
}

impl Serialize for PromptCursor {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(&self.bytes)
    }
}

impl<'de> Deserialize<'de> for PromptCursor {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let bytes = <Vec<u8>>::deserialize(deserializer).map_err(serde::de::Error::custom)?;
        if bytes.len() != CURSOR_LEN {
            return Err(serde::de::Error::custom("prompt cursor must be 64 bytes"));
        }
        let mut fixed = [0u8; CURSOR_LEN];
        fixed.copy_from_slice(&bytes);
        Ok(Self { bytes: fixed })
    }
}

fn kind_byte(kind: PromptProjectionQueryKind) -> u8 {
    match kind {
        PromptProjectionQueryKind::Metadata => 1,
        PromptProjectionQueryKind::Version => 2,
        PromptProjectionQueryKind::Diff => 3,
        PromptProjectionQueryKind::Search => 4,
        PromptProjectionQueryKind::Chain => 5,
        PromptProjectionQueryKind::History => 6,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CursorBinding {
    Metadata {
        request_id: RequestId,
        namespace: PromptNamespace,
        high_water: u64,
    },
    Version {
        request_id: RequestId,
        resource_id: PromptId,
        version_id: PromptVersionId,
        high_water: u64,
    },
    Diff {
        request_id: RequestId,
        resource_id: PromptId,
        old_version_id: PromptVersionId,
        new_version_id: PromptVersionId,
        high_water: u64,
    },
    Chain {
        request_id: RequestId,
        chain_id: PromptChainId,
        high_water: u64,
    },
}

fn cursor_mac(
    capability: &OwnerDeviceCapability,
    data: &[u8],
    binding: CursorBinding,
) -> Result<[u8; 16], PromptProjectionError> {
    let mut mac = HmacSha256::new_from_slice(capability.binding())
        .map_err(|_| PromptProjectionError::InvalidRequest)?;
    mac.update(data);
    match binding {
        CursorBinding::Metadata {
            request_id,
            namespace,
            high_water,
        } => {
            mac.update(&[0x01]);
            mac.update(request_id.as_bytes());
            mac.update(&[match namespace {
                PromptNamespace::Personal => 1,
                PromptNamespace::Organization => 2,
            }]);
            mac.update(&high_water.to_be_bytes());
        }
        CursorBinding::Version {
            request_id,
            resource_id,
            version_id,
            high_water,
        } => {
            mac.update(&[0x02]);
            mac.update(request_id.as_bytes());
            mac.update(resource_id.as_bytes());
            mac.update(version_id.as_bytes());
            mac.update(&high_water.to_be_bytes());
        }
        CursorBinding::Diff {
            request_id,
            resource_id,
            old_version_id,
            new_version_id,
            high_water,
        } => {
            mac.update(&[0x03]);
            mac.update(request_id.as_bytes());
            mac.update(resource_id.as_bytes());
            mac.update(old_version_id.as_bytes());
            mac.update(new_version_id.as_bytes());
            mac.update(&high_water.to_be_bytes());
        }
        CursorBinding::Chain {
            request_id,
            chain_id,
            high_water,
        } => {
            mac.update(&[0x05]);
            mac.update(request_id.as_bytes());
            mac.update(chain_id.as_bytes());
            mac.update(&high_water.to_be_bytes());
        }
    }
    let full = mac.finalize().into_bytes();
    let mut out = [0u8; 16];
    out.copy_from_slice(&full[..16]);
    Ok(out)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum PromptLibraryQuery {
    MetadataPage {
        namespace: PromptNamespace,
        cursor: Option<PromptCursor>,
        expected_revision: Option<u64>,
    },
    ExactVersion {
        version_id: PromptVersionId,
        cursor: Option<PromptCursor>,
    },
    Diff {
        old_version_id: PromptVersionId,
        new_version_id: PromptVersionId,
        cursor: Option<PromptCursor>,
    },
    Search {
        namespace: PromptNamespace,
        query: String,
        cursor: Option<PromptCursor>,
    },
    ChainPage {
        chain_id: Option<PromptChainId>,
        cursor: Option<PromptCursor>,
        expected_revision: Option<u64>,
    },
    HistoryPage {
        cursor: Option<PromptCursor>,
        expected_revision: Option<u64>,
    },
}

impl PromptLibraryQuery {
    pub fn validate_bounds(&self) -> Result<(), PromptProjectionError> {
        if let Self::Search { query, .. } = self {
            if query.len() > PROMPT_SEARCH_MAX_QUERY_BYTES {
                return Err(PromptProjectionError::SearchQueryTooLong);
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptLibraryRequest {
    request_id: RequestId,
    client_id: ClientId,
    task_id: Option<TaskId>,
    query: PromptLibraryQuery,
}

impl PromptLibraryRequest {
    pub fn request_id(&self) -> RequestId {
        self.request_id
    }

    pub fn client_id(&self) -> ClientId {
        self.client_id
    }

    pub fn task_id(&self) -> Option<TaskId> {
        self.task_id
    }

    pub fn expected_library_revision(&self) -> Option<u64> {
        match &self.query {
            PromptLibraryQuery::MetadataPage {
                expected_revision, ..
            }
            | PromptLibraryQuery::ChainPage {
                expected_revision, ..
            }
            | PromptLibraryQuery::HistoryPage {
                expected_revision, ..
            } => *expected_revision,
            _ => None,
        }
    }

    pub fn metadata_page(
        request_id: RequestId,
        client_id: ClientId,
        capability: &OwnerDeviceCapability,
        namespace: PromptNamespace,
        expected_revision: Option<u64>,
        cursor: Option<PromptCursor>,
    ) -> Result<Self, PromptProjectionError> {
        require_bound_client(capability, client_id)?;
        Ok(Self {
            request_id,
            client_id,
            task_id: None,
            query: PromptLibraryQuery::MetadataPage {
                namespace,
                cursor,
                expected_revision,
            },
        })
    }

    pub fn exact_version(
        request_id: RequestId,
        client_id: ClientId,
        capability: &OwnerDeviceCapability,
        version_id: PromptVersionId,
        cursor: Option<PromptCursor>,
    ) -> Result<Self, PromptProjectionError> {
        require_bound_client(capability, client_id)?;
        Ok(Self {
            request_id,
            client_id,
            task_id: None,
            query: PromptLibraryQuery::ExactVersion { version_id, cursor },
        })
    }

    pub fn diff(
        request_id: RequestId,
        client_id: ClientId,
        capability: &OwnerDeviceCapability,
        old_version_id: PromptVersionId,
        new_version_id: PromptVersionId,
        cursor: Option<PromptCursor>,
    ) -> Result<Self, PromptProjectionError> {
        require_bound_client(capability, client_id)?;
        Ok(Self {
            request_id,
            client_id,
            task_id: None,
            query: PromptLibraryQuery::Diff {
                old_version_id,
                new_version_id,
                cursor,
            },
        })
    }

    pub fn search(
        request_id: RequestId,
        client_id: ClientId,
        capability: &OwnerDeviceCapability,
        namespace: PromptNamespace,
        query: String,
        cursor: Option<PromptCursor>,
    ) -> Result<Self, PromptProjectionError> {
        require_bound_client(capability, client_id)?;
        if query.len() > PROMPT_SEARCH_MAX_QUERY_BYTES {
            return Err(PromptProjectionError::SearchQueryTooLong);
        }
        Ok(Self {
            request_id,
            client_id,
            task_id: None,
            query: PromptLibraryQuery::Search {
                namespace,
                query,
                cursor,
            },
        })
    }

    pub fn chain_page(
        request_id: RequestId,
        client_id: ClientId,
        capability: &OwnerDeviceCapability,
        chain_id: Option<PromptChainId>,
        expected_revision: Option<u64>,
        cursor: Option<PromptCursor>,
    ) -> Result<Self, PromptProjectionError> {
        require_bound_client(capability, client_id)?;
        Ok(Self {
            request_id,
            client_id,
            task_id: None,
            query: PromptLibraryQuery::ChainPage {
                chain_id,
                cursor,
                expected_revision,
            },
        })
    }

    pub fn history_page(
        request_id: RequestId,
        client_id: ClientId,
        capability: &OwnerDeviceCapability,
        expected_revision: Option<u64>,
        cursor: Option<PromptCursor>,
    ) -> Result<Self, PromptProjectionError> {
        require_bound_client(capability, client_id)?;
        Ok(Self {
            request_id,
            client_id,
            task_id: None,
            query: PromptLibraryQuery::HistoryPage {
                cursor,
                expected_revision,
            },
        })
    }

    pub fn from_authenticated_query(
        request_id: RequestId,
        client_id: ClientId,
        task_id: Option<TaskId>,
        query: PromptLibraryQuery,
        capability: &OwnerDeviceCapability,
    ) -> Result<Self, PromptProjectionError> {
        require_bound_client(capability, client_id)?;
        query.validate_bounds()?;
        Ok(Self {
            request_id,
            client_id,
            task_id,
            query,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptBodyChunk {
    sequence: u32,
    bytes: Vec<u8>,
    more: bool,
}

impl PromptBodyChunk {
    pub fn sequence(&self) -> u32 {
        self.sequence
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn more(&self) -> bool {
        self.more
    }
}

struct ChunkBytes<'a>(&'a [u8]);

impl Serialize for ChunkBytes<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(self.0)
    }
}

impl Serialize for PromptBodyChunk {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(3))?;
        map.serialize_entry("offset", &self.sequence)?;
        map.serialize_entry("bytes", &ChunkBytes(&self.bytes))?;
        map.serialize_entry("more", &self.more)?;
        map.end()
    }
}

impl<'de> Deserialize<'de> for PromptBodyChunk {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_map(PromptBodyChunkVisitor)
    }
}

struct PromptBodyChunkVisitor;

impl<'de> Visitor<'de> for PromptBodyChunkVisitor {
    type Value = PromptBodyChunk;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a named prompt body chunk")
    }

    fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
        let mut sequence = None;
        let mut bytes = None;
        let mut more = None;
        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "offset" => {
                    if sequence.is_some() {
                        return Err(de::Error::duplicate_field("offset"));
                    }
                    sequence = Some(map.next_value()?);
                }
                "bytes" => {
                    if bytes.is_some() {
                        return Err(de::Error::duplicate_field("bytes"));
                    }
                    bytes = Some(map.next_value::<Vec<u8>>()?);
                }
                "more" => {
                    if more.is_some() {
                        return Err(de::Error::duplicate_field("more"));
                    }
                    more = Some(map.next_value()?);
                }
                other => {
                    return Err(de::Error::unknown_field(
                        other,
                        &["offset", "bytes", "more"],
                    ));
                }
            }
        }
        let bytes = bytes.ok_or_else(|| de::Error::missing_field("bytes"))?;
        if bytes.len() > PROMPT_BODY_CHUNK_BYTES {
            return Err(de::Error::custom("prompt body chunk exceeds bound"));
        }
        Ok(PromptBodyChunk {
            sequence: sequence.ok_or_else(|| de::Error::missing_field("offset"))?,
            bytes,
            more: more.ok_or_else(|| de::Error::missing_field("more"))?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptMetadataItem {
    id: PromptId,
    title: String,
    description: Option<String>,
    tags: Vec<String>,
    current_version_id: PromptVersionId,
    revision: u64,
    archived_at_ms: Option<i64>,
    namespace: PromptNamespace,
    privacy_class: PromptPrivacyClass,
    #[serde(default)]
    unknown_fields: Vec<GenericPromptField>,
}

impl PromptMetadataItem {
    pub fn id(&self) -> PromptId {
        self.id
    }

    pub fn current_version_id(&self) -> PromptVersionId {
        self.current_version_id
    }

    pub fn privacy_class(&self) -> PromptPrivacyClass {
        self.privacy_class
    }

    pub fn unknown_fields(&self) -> &[GenericPromptField] {
        &self.unknown_fields
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptMetadataPage {
    schema_version: u32,
    library_revision: u64,
    namespace: PromptNamespace,
    items: Vec<PromptMetadataItem>,
    next_cursor: Option<PromptCursor>,
    encoded_bytes: u32,
}

impl PromptMetadataPage {
    pub fn schema_version(&self) -> u32 {
        self.schema_version
    }
    pub fn library_revision(&self) -> u64 {
        self.library_revision
    }
    pub fn namespace(&self) -> PromptNamespace {
        self.namespace
    }
    pub fn items(&self) -> &[PromptMetadataItem] {
        &self.items
    }
    pub fn next_cursor(&self) -> Option<&PromptCursor> {
        self.next_cursor.as_ref()
    }
    pub fn encoded_bytes(&self) -> u32 {
        self.encoded_bytes
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptVersionPage {
    schema_version: u32,
    library_revision: u64,
    version_id: PromptVersionId,
    prompt_id: PromptId,
    version: u32,
    body_sha256: [u8; 32],
    created_at_ms: i64,
    variables: Vec<String>,
    chunk: PromptBodyChunk,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_cursor: Option<PromptCursor>,
}

impl PromptVersionPage {
    pub fn version_id(&self) -> PromptVersionId {
        self.version_id
    }
    pub fn prompt_id(&self) -> PromptId {
        self.prompt_id
    }
    pub fn version(&self) -> u32 {
        self.version
    }
    pub fn body_sha256(&self) -> &[u8; 32] {
        &self.body_sha256
    }
    pub fn chunk(&self) -> &PromptBodyChunk {
        &self.chunk
    }
    pub fn next_cursor(&self) -> Option<&PromptCursor> {
        self.next_cursor.as_ref()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptDiffPage {
    schema_version: u32,
    library_revision: u64,
    old_version_id: PromptVersionId,
    new_version_id: PromptVersionId,
    chunk: PromptBodyChunk,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_cursor: Option<PromptCursor>,
}

impl PromptDiffPage {
    pub fn old_version_id(&self) -> PromptVersionId {
        self.old_version_id
    }
    pub fn new_version_id(&self) -> PromptVersionId {
        self.new_version_id
    }
    pub fn chunk(&self) -> &PromptBodyChunk {
        &self.chunk
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptSearchHit {
    source: PromptSearchSource,
    prompt_id: Option<PromptId>,
    history_id: Option<PromptHistoryId>,
    title: String,
    rank: u32,
    namespace: PromptNamespace,
    privacy_class: PromptPrivacyClass,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptSearchSource {
    SavedPrompt,
    History,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptSearchPage {
    schema_version: u32,
    library_revision: u64,
    namespace: PromptNamespace,
    hits: Vec<PromptSearchHit>,
    next_cursor: Option<PromptCursor>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptChainLinkRecord {
    id: PromptChainLinkId,
    chain_id: PromptChainId,
    position: u32,
    prompt_id: PromptId,
    prompt_version_id: PromptVersionId,
    previous_link_id: Option<PromptChainLinkId>,
    next_link_id: Option<PromptChainLinkId>,
    update_available: bool,
}

impl PromptChainLinkRecord {
    pub fn try_new(
        id: PromptChainLinkId,
        chain_id: PromptChainId,
        position: u32,
        prompt_id: PromptId,
        prompt_version_id: PromptVersionId,
        previous_link_id: Option<PromptChainLinkId>,
        next_link_id: Option<PromptChainLinkId>,
        update_available: bool,
    ) -> Result<Self, PromptProjectionError> {
        if position == 0 {
            return Err(PromptProjectionError::InvalidRequest);
        }
        Ok(Self {
            id,
            chain_id,
            position,
            prompt_id,
            prompt_version_id,
            previous_link_id,
            next_link_id,
            update_available,
        })
    }

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

    pub fn update_available(&self) -> bool {
        self.update_available
    }

    pub fn previous_link_id(&self) -> Option<PromptChainLinkId> {
        self.previous_link_id
    }

    pub fn next_link_id(&self) -> Option<PromptChainLinkId> {
        self.next_link_id
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptChainRecord {
    chain: PromptChain,
    links: Vec<PromptChainLinkRecord>,
}

impl PromptChainRecord {
    pub fn try_new(
        chain: PromptChain,
        links: Vec<PromptChainLinkRecord>,
    ) -> Result<Self, PromptProjectionError> {
        if links.len() > MAX_PROMPT_CHAIN_LINKS {
            return Err(PromptProjectionError::CapExceeded);
        }
        Ok(Self { chain, links })
    }

    pub fn links(&self) -> &[PromptChainLinkRecord] {
        &self.links
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptChainPage {
    schema_version: u32,
    library_revision: u64,
    chains: Vec<PromptChainRecord>,
    next_cursor: Option<PromptCursor>,
}

impl PromptChainPage {
    pub fn chains(&self) -> &[PromptChainRecord] {
        &self.chains
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptHistoryPage {
    schema_version: u32,
    library_revision: u64,
    items: Vec<PromptHistoryItem>,
    next_cursor: Option<PromptCursor>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptHistoryItem {
    id: PromptHistoryId,
    submitted_at_ms: i64,
    provider_kind: String,
    body_sha256: [u8; 32],
    namespace: PromptNamespace,
    privacy_class: PromptPrivacyClass,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptMutationSettlement {
    schema_version: u32,
    receipt: PromptMutationReceipt,
    library_revision: u64,
    request_id: RequestId,
    verified: bool,
}

impl PromptMutationSettlement {
    pub fn verified(&self) -> bool {
        self.verified
    }

    pub fn settled(&self) -> bool {
        self.verified
    }
}

impl Serialize for PromptMutationSettlement {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        SettlementWire {
            schema_version: self.schema_version,
            receipt: SettlementReceiptWire {
                command_id: self.receipt.command_id,
                prompt_id: self.receipt.prompt_id,
                prompt_version_id: self.receipt.prompt_version_id,
                revision: self.receipt.revision,
            },
            library_revision: self.library_revision,
            request_id: self.request_id,
            settled: self.verified,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for PromptMutationSettlement {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = SettlementWire::deserialize(deserializer)?;
        if wire.schema_version != PROMPT_PROJECTION_SCHEMA_VERSION {
            return Err(de::Error::custom("unsupported prompt settlement schema"));
        }
        if wire.receipt.revision == 0 {
            return Err(de::Error::custom(
                "prompt receipt revision must be positive",
            ));
        }
        Ok(Self {
            schema_version: wire.schema_version,
            receipt: PromptMutationReceipt {
                command_id: wire.receipt.command_id,
                prompt_id: wire.receipt.prompt_id,
                prompt_version_id: wire.receipt.prompt_version_id,
                revision: wire.receipt.revision,
            },
            library_revision: wire.library_revision,
            request_id: wire.request_id,
            verified: false,
        })
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SettlementReceiptWire {
    command_id: CommandId,
    prompt_id: PromptId,
    prompt_version_id: PromptVersionId,
    revision: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SettlementWire {
    schema_version: u32,
    receipt: SettlementReceiptWire,
    library_revision: u64,
    request_id: RequestId,
    settled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum PromptProjectionReply {
    MetadataPage(PromptMetadataPage),
    VersionPage(PromptVersionPage),
    DiffPage(PromptDiffPage),
    SearchPage(PromptSearchPage),
    ChainPage(PromptChainPage),
    HistoryPage(PromptHistoryPage),
    MutationSettlement(PromptMutationSettlement),
}

pub trait PromptProjectionSource {
    fn library_revision(&self) -> Result<u64, PromptProjectionError>;
    fn page_personal_metadata(
        &self,
        after: Option<PromptId>,
        limit: usize,
    ) -> Result<Vec<SavedPrompt>, PromptProjectionError>;
    fn get_version(
        &self,
        id: PromptVersionId,
    ) -> Result<Option<PromptVersion>, PromptProjectionError>;
    fn page_chain_links(
        &self,
        chain_id: PromptChainId,
        after: Option<PromptChainLinkId>,
        limit: usize,
    ) -> Result<Option<(PromptChain, Vec<PromptChainLinkRecord>)>, PromptProjectionError>;
    fn examined_rows(&self) -> u64 {
        0
    }
}

#[derive(Debug, Clone)]
pub struct BoundedTestSource {
    library_revision: u64,
    prompts: Vec<SavedPrompt>,
    versions: Vec<PromptVersion>,
    chains: Vec<PromptChainRecord>,
}

impl BoundedTestSource {
    pub fn try_new(
        library_revision: u64,
        prompts: Vec<SavedPrompt>,
        versions: Vec<PromptVersion>,
        chains: Vec<PromptChainRecord>,
    ) -> Result<Self, PromptProjectionError> {
        if prompts.len() > MAX_TEST_SOURCE_ITEMS
            || versions.len() > MAX_TEST_SOURCE_ITEMS
            || chains.len() > MAX_TEST_SOURCE_ITEMS
        {
            return Err(PromptProjectionError::CapExceeded);
        }
        for prompt in &prompts {
            validate_prompt_caps(prompt)?;
        }
        Ok(Self {
            library_revision,
            prompts,
            versions,
            chains,
        })
    }

    pub fn try_from_organization_records(
        _records: Vec<SavedPrompt>,
    ) -> Result<Self, PromptProjectionError> {
        Err(PromptProjectionError::Unavailable {
            subsystem: PromptProjectionSubsystem::OrganizationNamespace,
        })
    }
}

impl PromptProjectionSource for BoundedTestSource {
    fn library_revision(&self) -> Result<u64, PromptProjectionError> {
        Ok(self.library_revision)
    }

    fn page_personal_metadata(
        &self,
        after: Option<PromptId>,
        limit: usize,
    ) -> Result<Vec<SavedPrompt>, PromptProjectionError> {
        let start = after
            .and_then(|id| self.prompts.iter().position(|prompt| prompt.id == id))
            .map(|index| index.saturating_add(1))
            .unwrap_or(0);
        Ok(self
            .prompts
            .iter()
            .skip(start)
            .take(limit)
            .cloned()
            .collect())
    }

    fn get_version(
        &self,
        id: PromptVersionId,
    ) -> Result<Option<PromptVersion>, PromptProjectionError> {
        Ok(self
            .versions
            .iter()
            .find(|version| version.id == id)
            .cloned())
    }

    fn page_chain_links(
        &self,
        chain_id: PromptChainId,
        after: Option<PromptChainLinkId>,
        limit: usize,
    ) -> Result<Option<(PromptChain, Vec<PromptChainLinkRecord>)>, PromptProjectionError> {
        let Some(record) = self
            .chains
            .iter()
            .find(|record| record.chain.id == chain_id)
        else {
            return Ok(None);
        };
        let start = after
            .and_then(|id| record.links.iter().position(|link| link.id == id))
            .map(|index| index.saturating_add(1))
            .unwrap_or(0);
        Ok(Some((
            record.chain.clone(),
            record
                .links
                .iter()
                .skip(start)
                .take(limit)
                .cloned()
                .collect(),
        )))
    }
}

impl PromptProjectionSource for PromptStore {
    fn library_revision(&self) -> Result<u64, PromptProjectionError> {
        self.library_projection_revision()
            .map_err(|_| PromptProjectionError::InvalidRequest)
    }

    fn page_personal_metadata(
        &self,
        after: Option<PromptId>,
        limit: usize,
    ) -> Result<Vec<SavedPrompt>, PromptProjectionError> {
        self.list_prompts_after(after, limit)
            .map_err(|_| PromptProjectionError::InvalidRequest)
    }

    fn get_version(
        &self,
        id: PromptVersionId,
    ) -> Result<Option<PromptVersion>, PromptProjectionError> {
        PromptStore::get_version(self, id).map_err(|_| PromptProjectionError::InvalidRequest)
    }

    fn page_chain_links(
        &self,
        chain_id: PromptChainId,
        after: Option<PromptChainLinkId>,
        limit: usize,
    ) -> Result<Option<(PromptChain, Vec<PromptChainLinkRecord>)>, PromptProjectionError> {
        let Some(chain) = self
            .get_chain(chain_id)
            .map_err(|_| PromptProjectionError::InvalidRequest)?
        else {
            return Ok(None);
        };
        let links = self
            .list_chain_links_after(chain_id, after, limit)
            .map_err(|_| PromptProjectionError::InvalidRequest)?;
        let mut records = Vec::with_capacity(links.len());
        for link in links {
            let context = self
                .get_chain_link_context(chain_id, link.id())
                .map_err(|_| PromptProjectionError::InvalidRequest)?
                .ok_or(PromptProjectionError::NotFound)?;
            records.push(PromptChainLinkRecord::try_new(
                context.link.id(),
                context.link.chain_id(),
                context.link.position(),
                context.link.prompt_id(),
                context.link.prompt_version_id(),
                context.previous_link_id,
                context.next_link_id,
                context.update_available,
            )?);
        }
        Ok(Some((chain, records)))
    }
}

pub fn project_without_capability(
    _request: &PromptLibraryRequest,
    _source: &dyn PromptProjectionSource,
) -> Result<PromptProjectionReply, PromptProjectionError> {
    Err(PromptProjectionError::PermissionDenied)
}

fn require_negotiated_transport_limit(
    max_document_bytes: u32,
) -> Result<u32, PromptProjectionError> {
    if max_document_bytes == 0 {
        return Err(PromptProjectionError::Unavailable {
            subsystem: PromptProjectionSubsystem::NegotiatedTransportLimit,
        });
    }
    Ok(max_document_bytes)
}

pub fn project_prompt_store(
    capability: &OwnerDeviceCapability,
    request: &PromptLibraryRequest,
    store: &PromptStore,
    max_document_bytes: u32,
) -> Result<PromptProjectionReply, PromptProjectionError> {
    let max_document_bytes = require_negotiated_transport_limit(max_document_bytes)?;
    let reply = project_prompt_library(capability, request, store, max_document_bytes)?;
    encode_prompt_projection_document_limited(&reply, max_document_bytes)?;
    Ok(reply)
}

pub fn project_prompt_library(
    capability: &OwnerDeviceCapability,
    request: &PromptLibraryRequest,
    source: &dyn PromptProjectionSource,
    max_document_bytes: u32,
) -> Result<PromptProjectionReply, PromptProjectionError> {
    require_negotiated_transport_limit(max_document_bytes)?;
    let revision = source.library_revision()?;
    let reply = match &request.query {
        PromptLibraryQuery::Search { .. } => Err(PromptProjectionError::Unavailable {
            subsystem: PromptProjectionSubsystem::SearchIndex,
        }),
        PromptLibraryQuery::HistoryPage { .. } => Err(PromptProjectionError::Unavailable {
            subsystem: PromptProjectionSubsystem::HistoryStore,
        }),
        PromptLibraryQuery::MetadataPage {
            namespace: PromptNamespace::Organization,
            ..
        } => Err(PromptProjectionError::Unavailable {
            subsystem: PromptProjectionSubsystem::OrganizationNamespace,
        }),
        PromptLibraryQuery::MetadataPage {
            namespace,
            cursor,
            expected_revision,
        } => {
            check_revision(revision, *expected_revision)?;
            let after = match cursor {
                Some(cursor) => {
                    let (last, _) = cursor.open(
                        capability,
                        PromptProjectionQueryKind::Metadata,
                        *namespace,
                        revision,
                        CursorBinding::Metadata {
                            request_id: cursor.issued_request_id()?,
                            namespace: *namespace,
                            high_water: revision,
                        },
                    )?;
                    last.and_then(|bytes| PromptId::from_bytes(bytes).ok())
                }
                None => None,
            };
            let fetched = source
                .page_personal_metadata(after, PROMPT_METADATA_PAGE_ITEMS.saturating_add(1))?;
            let has_more = fetched.len() > PROMPT_METADATA_PAGE_ITEMS;
            let items: Vec<_> = fetched
                .into_iter()
                .take(PROMPT_METADATA_PAGE_ITEMS)
                .map(|prompt| metadata_item(prompt, *namespace))
                .collect();
            let next_cursor = if has_more {
                items.last().and_then(|item| {
                    PromptCursor::issue(
                        capability,
                        PromptProjectionQueryKind::Metadata,
                        *namespace,
                        revision,
                        Some(*item.id.as_bytes()),
                        0,
                        request.request_id,
                        CursorBinding::Metadata {
                            request_id: request.request_id,
                            namespace: *namespace,
                            high_water: revision,
                        },
                    )
                    .ok()
                })
            } else {
                None
            };
            let mut page = PromptMetadataPage {
                schema_version: PROMPT_PROJECTION_SCHEMA_VERSION,
                library_revision: revision,
                namespace: *namespace,
                items,
                next_cursor,
                encoded_bytes: 0,
            };
            let encoded = encode_named(&page)?;
            page.encoded_bytes = u32::try_from(encoded.len())
                .map_err(|_| PromptProjectionError::SerializationFailure)?;
            Ok(PromptProjectionReply::MetadataPage(page))
        }
        PromptLibraryQuery::ExactVersion { version_id, cursor } => {
            if let Some(cursor) = cursor {
                cursor.require_kind(PromptProjectionQueryKind::Version)?;
            }
            let version = source
                .get_version(*version_id)?
                .ok_or(PromptProjectionError::NotFound)?;
            let sequence = match cursor {
                Some(cursor) => {
                    cursor
                        .open(
                            capability,
                            PromptProjectionQueryKind::Version,
                            PromptNamespace::Personal,
                            revision,
                            CursorBinding::Version {
                                request_id: cursor.issued_request_id()?,
                                resource_id: version.prompt_id,
                                version_id: version.id,
                                high_water: revision,
                            },
                        )?
                        .1
                }
                None => 0,
            };
            let chunk = chunk_bytes(version.body.as_bytes(), sequence)?;
            let next_cursor = next_chunk_cursor(
                capability,
                PromptProjectionQueryKind::Version,
                revision,
                Some(*version.id.as_bytes()),
                sequence,
                request.request_id,
                CursorBinding::Version {
                    request_id: request.request_id,
                    resource_id: version.prompt_id,
                    version_id: version.id,
                    high_water: revision,
                },
                chunk.more,
            )?;
            Ok(PromptProjectionReply::VersionPage(PromptVersionPage {
                schema_version: PROMPT_PROJECTION_SCHEMA_VERSION,
                library_revision: revision,
                version_id: version.id,
                prompt_id: version.prompt_id,
                version: version.version,
                body_sha256: version.body_sha256,
                created_at_ms: version.created_at_ms,
                variables: version.variables.clone(),
                chunk,
                next_cursor,
            }))
        }
        PromptLibraryQuery::Diff {
            old_version_id,
            new_version_id,
            cursor,
        } => {
            if let Some(cursor) = cursor {
                cursor.require_kind(PromptProjectionQueryKind::Diff)?;
            }
            let old = source
                .get_version(*old_version_id)?
                .ok_or(PromptProjectionError::NotFound)?;
            let new = source
                .get_version(*new_version_id)?
                .ok_or(PromptProjectionError::NotFound)?;
            let sequence = match cursor {
                Some(cursor) => {
                    cursor
                        .open(
                            capability,
                            PromptProjectionQueryKind::Diff,
                            PromptNamespace::Personal,
                            revision,
                            CursorBinding::Diff {
                                request_id: cursor.issued_request_id()?,
                                resource_id: old.prompt_id,
                                old_version_id: *old_version_id,
                                new_version_id: *new_version_id,
                                high_water: revision,
                            },
                        )?
                        .1
                }
                None => 0,
            };
            let encoded = encode_public_diff(&diff_versions(&old.body, &new.body))
                .map_err(|_| PromptProjectionError::SerializationFailure)?;
            let chunk = chunk_bytes(&encoded, sequence)?;
            let next_cursor = next_chunk_cursor(
                capability,
                PromptProjectionQueryKind::Diff,
                revision,
                Some(*new_version_id.as_bytes()),
                sequence,
                request.request_id,
                CursorBinding::Diff {
                    request_id: request.request_id,
                    resource_id: old.prompt_id,
                    old_version_id: *old_version_id,
                    new_version_id: *new_version_id,
                    high_water: revision,
                },
                chunk.more,
            )?;
            Ok(PromptProjectionReply::DiffPage(PromptDiffPage {
                schema_version: PROMPT_PROJECTION_SCHEMA_VERSION,
                library_revision: revision,
                old_version_id: *old_version_id,
                new_version_id: *new_version_id,
                chunk,
                next_cursor,
            }))
        }
        PromptLibraryQuery::ChainPage {
            chain_id,
            cursor,
            expected_revision,
        } => {
            check_revision(revision, *expected_revision)?;
            let Some(chain_id) = chain_id else {
                return Err(PromptProjectionError::Unavailable {
                    subsystem: PromptProjectionSubsystem::ChainDirectory,
                });
            };
            if let Some(cursor) = cursor {
                cursor.require_kind(PromptProjectionQueryKind::Chain)?;
            }
            let after = match cursor {
                Some(cursor) => {
                    let (last, _) = cursor.open(
                        capability,
                        PromptProjectionQueryKind::Chain,
                        PromptNamespace::Personal,
                        revision,
                        CursorBinding::Chain {
                            request_id: cursor.issued_request_id()?,
                            chain_id: *chain_id,
                            high_water: revision,
                        },
                    )?;
                    last.and_then(|bytes| PromptChainLinkId::from_bytes(bytes).ok())
                }
                None => None,
            };
            let Some((chain, links)) = source.page_chain_links(
                *chain_id,
                after,
                PROMPT_METADATA_PAGE_ITEMS.saturating_add(1),
            )?
            else {
                return Err(PromptProjectionError::NotFound);
            };
            let has_more = links.len() > PROMPT_METADATA_PAGE_ITEMS;
            let links: Vec<_> = links.into_iter().take(PROMPT_METADATA_PAGE_ITEMS).collect();
            let next_cursor = if has_more {
                links.last().and_then(|link| {
                    PromptCursor::issue(
                        capability,
                        PromptProjectionQueryKind::Chain,
                        PromptNamespace::Personal,
                        revision,
                        Some(*link.id.as_bytes()),
                        0,
                        request.request_id,
                        CursorBinding::Chain {
                            request_id: request.request_id,
                            chain_id: *chain_id,
                            high_water: revision,
                        },
                    )
                    .ok()
                })
            } else {
                None
            };
            Ok(PromptProjectionReply::ChainPage(PromptChainPage {
                schema_version: PROMPT_PROJECTION_SCHEMA_VERSION,
                library_revision: revision,
                chains: vec![PromptChainRecord::try_new(chain, links)?],
                next_cursor,
            }))
        }
    }?;
    encode_prompt_projection_document_limited(&reply, max_document_bytes)?;
    Ok(reply)
}

pub(crate) fn settle_prompt_mutation(
    _capability: &OwnerDeviceCapability,
    request: &PromptLibraryRequest,
    receipt: &PromptMutationReceipt,
    store: &PromptStore,
) -> Result<PromptMutationSettlement, PromptProjectionError> {
    let prompt = store
        .get_prompt(receipt.prompt_id)
        .map_err(|_| PromptProjectionError::InvalidRequest)?
        .ok_or(PromptProjectionError::NotFound)?;
    if prompt.revision != receipt.revision {
        return Err(PromptProjectionError::StaleRevision {
            expected: receipt.revision,
            actual: prompt.revision,
        });
    }
    Ok(PromptMutationSettlement {
        schema_version: PROMPT_PROJECTION_SCHEMA_VERSION,
        receipt: receipt.clone(),
        library_revision: store
            .library_projection_revision()
            .map_err(|_| PromptProjectionError::InvalidRequest)?,
        request_id: request.request_id,
        verified: true,
    })
}

pub fn encode_prompt_projection_document(
    reply: &PromptProjectionReply,
) -> Result<Vec<u8>, PromptProjectionError> {
    encode_prompt_projection_document_limited(reply, PROMPT_METADATA_PAGE_BYTES as u32)
}

pub fn encode_prompt_projection_document_limited(
    reply: &PromptProjectionReply,
    max_document_bytes: u32,
) -> Result<Vec<u8>, PromptProjectionError> {
    let max = (max_document_bytes as usize)
        .min(PROMPT_METADATA_PAGE_BYTES)
        .min(PROMPT_TRANSFER_CEILING_BYTES);
    encode_named_limited(reply, max)
}

fn encode_named<T: Serialize>(value: &T) -> Result<Vec<u8>, PromptProjectionError> {
    encode_named_limited(value, PROMPT_METADATA_PAGE_BYTES)
}

fn encode_named_limited<T: Serialize>(
    value: &T,
    max_bytes: usize,
) -> Result<Vec<u8>, PromptProjectionError> {
    let mut writer = BoundedBuf::new(max_bytes);
    match rmp_serde::encode::write_named(&mut writer, value) {
        Ok(()) => Ok(writer.into_inner()),
        Err(_) if writer.hit_bound => Err(PromptProjectionError::CodecBound),
        Err(_) => Err(PromptProjectionError::SerializationFailure),
    }
}

struct BoundedBuf {
    buf: Vec<u8>,
    max: usize,
    hit_bound: bool,
}

impl BoundedBuf {
    fn new(max: usize) -> Self {
        Self {
            buf: Vec::new(),
            max,
            hit_bound: false,
        }
    }

    fn into_inner(self) -> Vec<u8> {
        self.buf
    }
}

impl Write for BoundedBuf {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let next = match self.buf.len().checked_add(data.len()) {
            Some(next) => next,
            None => {
                self.hit_bound = true;
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "prompt document overflow",
                ));
            }
        };
        if next > self.max {
            self.hit_bound = true;
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "prompt document exceeds negotiated bound",
            ));
        }
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub fn decode_prompt_projection_document(
    payload: &[u8],
) -> Result<PromptProjectionReply, PromptProjectionError> {
    decode_prompt_projection_document_limited(payload, PROMPT_METADATA_PAGE_BYTES as u32)
}

pub fn decode_prompt_projection_document_limited(
    payload: &[u8],
    max_document_bytes: u32,
) -> Result<PromptProjectionReply, PromptProjectionError> {
    let max = (max_document_bytes as usize)
        .min(PROMPT_METADATA_PAGE_BYTES)
        .min(PROMPT_TRANSFER_CEILING_BYTES);
    if payload.len() > max {
        return Err(PromptProjectionError::CodecBound);
    }
    reject_duplicate_keys(payload)?;
    preflight(payload)?;
    rmp_serde::from_slice(payload).map_err(|_| PromptProjectionError::InvalidRequest)
}

fn metadata_item(prompt: SavedPrompt, namespace: PromptNamespace) -> PromptMetadataItem {
    PromptMetadataItem {
        id: prompt.id,
        title: prompt.title,
        description: prompt.description,
        tags: prompt.tags,
        current_version_id: prompt.current_version_id,
        revision: prompt.revision,
        archived_at_ms: prompt.archived_at_ms,
        namespace,
        privacy_class: PromptPrivacyClass::LocalOnly,
        unknown_fields: Vec::new(),
    }
}

fn check_revision(actual: u64, expected: Option<u64>) -> Result<(), PromptProjectionError> {
    if let Some(expected) = expected {
        if expected != actual {
            return Err(PromptProjectionError::StaleRevision { expected, actual });
        }
    }
    Ok(())
}

fn next_chunk_cursor(
    capability: &OwnerDeviceCapability,
    kind: PromptProjectionQueryKind,
    revision: u64,
    last_id: Option<[u8; 16]>,
    sequence: u32,
    request_id: RequestId,
    binding: CursorBinding,
    more: bool,
) -> Result<Option<PromptCursor>, PromptProjectionError> {
    if !more {
        return Ok(None);
    }
    Ok(Some(PromptCursor::issue(
        capability,
        kind,
        PromptNamespace::Personal,
        revision,
        last_id,
        sequence.saturating_add(1),
        request_id,
        binding,
    )?))
}

fn chunk_bytes(bytes: &[u8], sequence: u32) -> Result<PromptBodyChunk, PromptProjectionError> {
    let start = (sequence as usize).saturating_mul(PROMPT_BODY_CHUNK_BYTES);
    if start > PROMPT_TRANSFER_CEILING_BYTES {
        return Err(PromptProjectionError::TransferCeilingExceeded);
    }
    if start > bytes.len() {
        return Err(PromptProjectionError::InvalidRequest);
    }
    let end = (start + PROMPT_BODY_CHUNK_BYTES)
        .min(bytes.len())
        .min(PROMPT_TRANSFER_CEILING_BYTES);
    Ok(PromptBodyChunk {
        sequence,
        bytes: bytes[start..end].to_vec(),
        more: end < bytes.len() && end < PROMPT_TRANSFER_CEILING_BYTES,
    })
}

fn validate_prompt_caps(prompt: &SavedPrompt) -> Result<(), PromptProjectionError> {
    if prompt.title.chars().count() > MAX_PROMPT_TITLE_SCALARS {
        return Err(PromptProjectionError::CapExceeded);
    }
    if prompt
        .description
        .as_ref()
        .map(|value| value.chars().count())
        .unwrap_or(0)
        > MAX_PROMPT_DESCRIPTION_SCALARS
    {
        return Err(PromptProjectionError::CapExceeded);
    }
    if prompt.tags.len() > MAX_PROMPT_TAGS
        || prompt
            .tags
            .iter()
            .any(|tag| tag.chars().count() > MAX_PROMPT_TAG_SCALARS)
    {
        return Err(PromptProjectionError::CapExceeded);
    }
    Ok(())
}

fn preflight(payload: &[u8]) -> Result<(), PromptProjectionError> {
    if payload.is_empty() {
        return Err(PromptProjectionError::InvalidRequest);
    }
    let mut nodes = 0usize;
    scan_value(payload, 0, 0, &mut nodes).map(|_| ())
}

fn reject_duplicate_keys(payload: &[u8]) -> Result<(), PromptProjectionError> {
    let mut nodes = 0usize;
    scan_value(payload, 0, 0, &mut nodes).map(|_| ())
}

fn scan_value(
    payload: &[u8],
    mut offset: usize,
    depth: usize,
    nodes: &mut usize,
) -> Result<usize, PromptProjectionError> {
    if offset >= payload.len() {
        return Err(PromptProjectionError::InvalidRequest);
    }
    if depth > MAX_CODEC_DEPTH {
        return Err(PromptProjectionError::CodecBound);
    }
    *nodes = nodes
        .checked_add(1)
        .ok_or(PromptProjectionError::CodecBound)?;
    if *nodes > MAX_CODEC_NODES {
        return Err(PromptProjectionError::CodecBound);
    }
    let marker = payload[offset];
    offset += 1;
    match marker {
        0x80..=0x8f => scan_map(payload, offset, (marker & 0x0f) as usize, depth, nodes),
        0xde if offset + 1 < payload.len() => {
            let len = u16::from_be_bytes([payload[offset], payload[offset + 1]]) as usize;
            scan_map(payload, offset + 2, len, depth, nodes)
        }
        0xdf if offset + 3 < payload.len() => {
            let len = u32::from_be_bytes(payload[offset..offset + 4].try_into().unwrap()) as usize;
            scan_map(payload, offset + 4, len, depth, nodes)
        }
        0x90..=0x9f => scan_array(payload, offset, (marker & 0x0f) as usize, depth, nodes),
        0xdc if offset + 1 < payload.len() => {
            let len = u16::from_be_bytes([payload[offset], payload[offset + 1]]) as usize;
            scan_array(payload, offset + 2, len, depth, nodes)
        }
        0xdd if offset + 3 < payload.len() => {
            let len = u32::from_be_bytes(payload[offset..offset + 4].try_into().unwrap()) as usize;
            scan_array(payload, offset + 4, len, depth, nodes)
        }
        0xa0..=0xbf => finish_string(offset, (marker & 0x1f) as usize, payload.len()),
        0xd9 if offset < payload.len() => {
            finish_string(offset + 1, payload[offset] as usize, payload.len())
        }
        0xda if offset + 1 < payload.len() => finish_string(
            offset + 2,
            u16::from_be_bytes([payload[offset], payload[offset + 1]]) as usize,
            payload.len(),
        ),
        0xdb if offset + 3 < payload.len() => finish_string(
            offset + 4,
            u32::from_be_bytes(payload[offset..offset + 4].try_into().unwrap()) as usize,
            payload.len(),
        ),
        0xc4 if offset < payload.len() => {
            finish_bin(offset + 1, payload[offset] as usize, payload.len())
        }
        0xc5 if offset + 1 < payload.len() => finish_bin(
            offset + 2,
            u16::from_be_bytes([payload[offset], payload[offset + 1]]) as usize,
            payload.len(),
        ),
        0xc6 if offset + 3 < payload.len() => finish_bin(
            offset + 4,
            u32::from_be_bytes(payload[offset..offset + 4].try_into().unwrap()) as usize,
            payload.len(),
        ),
        0xc0 | 0xc2 | 0xc3 | 0x00..=0x7f | 0xe0..=0xff => Ok(offset),
        0xcc | 0xd0 => Ok(offset + 1),
        0xcd | 0xd1 => Ok(offset + 2),
        0xce | 0xd2 | 0xca => Ok(offset + 4),
        0xcf | 0xd3 | 0xcb => Ok(offset + 8),
        _ => Err(PromptProjectionError::InvalidRequest),
    }
}

fn finish_string(
    start: usize,
    len: usize,
    payload_len: usize,
) -> Result<usize, PromptProjectionError> {
    if len > MAX_CODEC_STRING_BYTES {
        return Err(PromptProjectionError::CodecBound);
    }
    let end = start
        .checked_add(len)
        .ok_or(PromptProjectionError::CodecBound)?;
    if end > payload_len {
        return Err(PromptProjectionError::InvalidRequest);
    }
    Ok(end)
}

fn finish_bin(
    start: usize,
    len: usize,
    payload_len: usize,
) -> Result<usize, PromptProjectionError> {
    if len > PROMPT_BODY_CHUNK_BYTES {
        return Err(PromptProjectionError::CodecBound);
    }
    let end = start
        .checked_add(len)
        .ok_or(PromptProjectionError::CodecBound)?;
    if end > payload_len {
        return Err(PromptProjectionError::InvalidRequest);
    }
    Ok(end)
}

fn scan_map(
    payload: &[u8],
    mut offset: usize,
    len: usize,
    depth: usize,
    nodes: &mut usize,
) -> Result<usize, PromptProjectionError> {
    if len > MAX_CODEC_MAP_ENTRIES {
        return Err(PromptProjectionError::CodecBound);
    }
    let mut seen = Vec::with_capacity(len);
    for _ in 0..len {
        let key_start = offset;
        offset = scan_value(payload, offset, depth + 1, nodes)?;
        let key = payload.get(key_start..offset).unwrap_or(&[]);
        if seen.iter().any(|prior: &&[u8]| *prior == key) {
            return Err(PromptProjectionError::DuplicateKey);
        }
        seen.push(key);
        offset = scan_value(payload, offset, depth + 1, nodes)?;
    }
    Ok(offset)
}

fn scan_array(
    payload: &[u8],
    mut offset: usize,
    len: usize,
    depth: usize,
    nodes: &mut usize,
) -> Result<usize, PromptProjectionError> {
    if len > MAX_CODEC_ARRAY_ITEMS {
        return Err(PromptProjectionError::CodecBound);
    }
    for _ in 0..len {
        offset = scan_value(payload, offset, depth + 1, nodes)?;
    }
    Ok(offset)
}

#[cfg(test)]
mod sealed_codec_tests {
    use super::{
        decode_prompt_projection_document, testing, PromptProjectionError,
        PROMPT_METADATA_PAGE_BYTES,
    };

    #[test]
    fn decode_rejects_oversized_duplicate_key_and_forced_serialization_failure() {
        let oversized = vec![0x00; PROMPT_METADATA_PAGE_BYTES + 1];
        assert_eq!(
            decode_prompt_projection_document(&oversized).expect_err("oversized"),
            PromptProjectionError::CodecBound
        );

        let mut duplicate = Vec::new();
        duplicate.push(0x82);
        duplicate.extend_from_slice(&[0xa7]);
        duplicate.extend_from_slice(b"version");
        duplicate.push(0x01);
        duplicate.extend_from_slice(&[0xa7]);
        duplicate.extend_from_slice(b"version");
        duplicate.push(0x02);
        assert_eq!(
            decode_prompt_projection_document(&duplicate).expect_err("duplicate key"),
            PromptProjectionError::DuplicateKey
        );
        assert_eq!(
            testing::serialization_failure_from_failing_payload(),
            PromptProjectionError::SerializationFailure
        );
    }
}
