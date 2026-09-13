//! Versioned organization wire names and bounded payload documents.
//!
//! Extension type identifiers remain generic Connect tags. Payloads are
//! host-side wire structs: local UUIDv7 identifiers stay UUID fields, and
//! Portal tenant/account/device/Board/BoardCard identifiers stay opaque
//! strings that are never parsed as UUIDs.

use serde::{Deserialize, Serialize};
use uuid::{Uuid, Version};

use super::Capability;

pub const ORGANIZATION_SCHEMA_VERSION: u16 = 1;
pub const ORGANIZATION_PROMPT_BODY_LIMIT_BYTES: u32 = 256 * 1024;
pub const ORGANIZATION_PROMPT_PAGE_ITEM_LIMIT: u32 = 100;
pub const ORGANIZATION_PROMPT_PAGE_ENCODED_LIMIT_BYTES: u32 = 512 * 1024;
pub const MAX_ORGANIZATION_PAYLOAD_BYTES: usize =
    ORGANIZATION_PROMPT_PAGE_ENCODED_LIMIT_BYTES as usize;
pub const MAX_ORGANIZATION_COLLECTION_ITEMS: usize = 128;
pub const MAX_ORGANIZATION_LABEL_BYTES: usize = 256;
pub const MAX_ORGANIZATION_OPAQUE_ID_BYTES: usize = 128;

/// Reserved generic-extension type identifiers for organization projections.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u16)]
pub enum OrganizationExtensionKind {
    Membership = 1001,
    ManagedTask = 1002,
    OrganizationPrompt = 1003,
    WatcherView = 1004,
    LocalAction = 1005,
    EvidenceBundle = 1006,
    BoardWorkflow = 1007,
}

impl OrganizationExtensionKind {
    pub const fn type_id(self) -> u16 {
        self as u16
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Membership => "organization_membership",
            Self::ManagedTask => "managed_task_link",
            Self::OrganizationPrompt => "organization_prompt",
            Self::WatcherView => "organization_watcher",
            Self::LocalAction => "local_action",
            Self::EvidenceBundle => "evidence_bundle",
            Self::BoardWorkflow => "board_workflow",
        }
    }
}

pub const fn organization_extension_type(kind: OrganizationExtensionKind) -> u16 {
    kind.type_id()
}

/// Anonymous/local standalone Hello must not advertise organization projection.
pub const fn organization_capability_for_standalone() -> Capability {
    Capability::OrganizationProjection
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrganizationCodecError {
    UnknownSchema,
    WrongIdentity,
    ZeroRevision,
    BoundExceeded,
    DuplicateId,
    RawEvidence,
    Malformed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrganizationEnvelopeWire {
    pub schema_version: u16,
    pub tenant_id: String,
    pub account_id: String,
    pub host_id: Uuid,
    pub session_id: String,
    pub revision: u64,
    pub payload: OrganizationWirePayload,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum OrganizationWirePayload {
    Membership(OrganizationMembershipWire),
    Policy(OrganizationPolicyWire),
    ManagedTask(OrganizationManagedTaskWire),
    PromptSnapshot(OrganizationPromptSnapshotWire),
    LocalActionCatalog(OrganizationLocalActionCatalogWire),
    LocalActionState(OrganizationLocalActionStateWire),
    EvidenceMetadata(OrganizationEvidenceMetadataWire),
    TelemetryIntent(OrganizationTelemetryIntentWire),
    FleetWatcher(OrganizationFleetWatcherWire),
    TaskWatcher(OrganizationTaskWatcherWire),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrganizationMembershipWire {
    pub schema_version: u16,
    pub tenant_id: String,
    pub account_id: String,
    pub host_id: Uuid,
    pub device_id: String,
    pub role: String,
    pub status: String,
    pub display_name: String,
    pub policy_revision: u32,
    pub enrolled_at_ms: i64,
    pub last_seen_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrganizationPolicyWire {
    pub schema_version: u16,
    pub tenant_id: String,
    pub revision: u32,
    pub allowed_metadata_fields: Vec<String>,
    pub retention_ms: u64,
    pub idle_interval_ms: u64,
    pub raw_sharing_ceiling: String,
    pub local_action_approval: String,
    pub prompt_maintainer_accounts: Vec<String>,
    pub content_hash_hex: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrganizationManagedTaskWire {
    pub schema_version: u16,
    pub tenant_id: String,
    pub host_id: Uuid,
    pub local_task_id: Uuid,
    pub link_id: Uuid,
    pub board_card_id: String,
    pub enrollment_state: String,
    pub portal_revision: u64,
    pub metadata_policy_version: u32,
    pub linked_by: String,
    pub linked_at: i64,
    pub unlinked_at: Option<i64>,
    pub portal_title: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrganizationPromptSnapshotWire {
    pub schema_version: u16,
    pub tenant_id: String,
    pub revision: u32,
    pub prompts: Vec<OrganizationPromptWire>,
    pub versions: Vec<OrganizationPromptVersionWire>,
    pub chains: Vec<OrganizationPromptChainWire>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrganizationPromptWire {
    pub prompt_id: Uuid,
    pub tenant_id: String,
    pub namespace: String,
    pub name: String,
    pub current_version_id: Uuid,
    pub lifecycle: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrganizationPromptVersionWire {
    pub prompt_id: Uuid,
    pub version_id: Uuid,
    pub author: String,
    pub title: String,
    pub tags: Vec<String>,
    pub body: String,
    pub content_hash_hex: String,
    pub published_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrganizationPromptChainWire {
    pub chain_id: Uuid,
    pub tenant_id: String,
    pub revision: u32,
    pub links: Vec<OrganizationPromptChainLinkWire>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrganizationPromptChainLinkWire {
    pub position: u32,
    pub version_id: Uuid,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrganizationLocalActionCatalogWire {
    pub schema_version: u16,
    pub tenant_id: String,
    pub host_id: Uuid,
    pub entries: Vec<OrganizationLocalActionCatalogEntryWire>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrganizationLocalActionCatalogEntryWire {
    pub kind: String,
    pub version: u16,
    pub replay_policy: String,
    pub risk: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrganizationLocalActionStateWire {
    pub schema_version: u16,
    pub tenant_id: String,
    pub host_id: Uuid,
    pub request_id: Uuid,
    pub admission: String,
    pub replay_policy: Option<String>,
    pub outcome: Option<String>,
    pub reconcile: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrganizationEvidenceMetadataWire {
    pub schema_version: u16,
    pub tenant_id: String,
    pub bundle_id: Uuid,
    pub draft_id: Uuid,
    pub title: String,
    pub summary: String,
    pub content_hash_hex: String,
    pub signer: String,
    pub capture_started_at_ms: i64,
    pub capture_ended_at_ms: i64,
    pub raw_content_included: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrganizationTelemetryIntentWire {
    pub schema_version: u16,
    pub tenant_id: String,
    pub host_id: Uuid,
    pub observation_id_hex: String,
    pub intent: String,
    pub publication_queued: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrganizationFleetWatcherWire {
    pub schema_version: u16,
    pub tenant_id: String,
    pub host_id: Uuid,
    pub reachability: String,
    pub assigned: u32,
    pub in_progress: u32,
    pub waiting: u32,
    pub blocked: u32,
    pub review: u32,
    pub last_activity_ms: Option<i64>,
    pub mutation_allowed: bool,
    pub freshness: String,
    pub completeness: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrganizationTaskWatcherWire {
    pub schema_version: u16,
    pub tenant_id: String,
    pub host_id: Uuid,
    pub task_id: Uuid,
    pub board_card_id: String,
    pub lifecycle: String,
    pub attention: String,
    pub host_reachability: String,
    pub usage_source_label: Option<String>,
    pub git_summary: Option<String>,
    pub freshness: String,
    pub completeness: String,
    pub mutation_allowed: bool,
}

pub fn encode_organization_envelope(
    envelope: &OrganizationEnvelopeWire,
) -> Result<Vec<u8>, OrganizationCodecError> {
    validate_organization_envelope(envelope)?;
    canonical_bytes(envelope)
}

pub fn decode_organization_envelope(
    bytes: &[u8],
) -> Result<OrganizationEnvelopeWire, OrganizationCodecError> {
    if bytes.len() > MAX_ORGANIZATION_PAYLOAD_BYTES {
        return Err(OrganizationCodecError::BoundExceeded);
    }
    let envelope: OrganizationEnvelopeWire =
        serde_json::from_slice(bytes).map_err(|_| OrganizationCodecError::Malformed)?;
    validate_organization_envelope(&envelope)?;
    Ok(envelope)
}

pub fn encode_organization_payload(
    payload: &OrganizationWirePayload,
) -> Result<Vec<u8>, OrganizationCodecError> {
    validate_organization_payload(payload, None, None)?;
    canonical_bytes(payload)
}

pub fn decode_organization_payload(
    bytes: &[u8],
) -> Result<OrganizationWirePayload, OrganizationCodecError> {
    if bytes.len() > MAX_ORGANIZATION_PAYLOAD_BYTES {
        return Err(OrganizationCodecError::BoundExceeded);
    }
    let payload: OrganizationWirePayload =
        serde_json::from_slice(bytes).map_err(|_| OrganizationCodecError::Malformed)?;
    validate_organization_payload(&payload, None, None)?;
    Ok(payload)
}

pub fn organization_envelope_canonical_bytes(
    envelope: &OrganizationEnvelopeWire,
) -> Result<Vec<u8>, OrganizationCodecError> {
    validate_organization_envelope(envelope)?;
    canonical_bytes(envelope)
}

fn canonical_bytes<T: Serialize>(value: &T) -> Result<Vec<u8>, OrganizationCodecError> {
    let encoded = serde_json::to_vec(value).map_err(|_| OrganizationCodecError::Malformed)?;
    if encoded.len() > MAX_ORGANIZATION_PAYLOAD_BYTES {
        return Err(OrganizationCodecError::BoundExceeded);
    }
    Ok(encoded)
}

pub fn validate_organization_envelope(
    envelope: &OrganizationEnvelopeWire,
) -> Result<(), OrganizationCodecError> {
    if envelope.schema_version != ORGANIZATION_SCHEMA_VERSION {
        return Err(OrganizationCodecError::UnknownSchema);
    }
    require_opaque_id(&envelope.tenant_id)?;
    require_opaque_id(&envelope.account_id)?;
    require_opaque_id(&envelope.session_id)?;
    require_uuid_v7(envelope.host_id)?;
    if envelope.revision == 0 {
        return Err(OrganizationCodecError::ZeroRevision);
    }
    validate_organization_payload(
        &envelope.payload,
        Some(envelope.tenant_id.as_str()),
        Some(envelope.host_id),
    )
}

pub fn validate_organization_payload(
    payload: &OrganizationWirePayload,
    expected_tenant: Option<&str>,
    expected_host: Option<Uuid>,
) -> Result<(), OrganizationCodecError> {
    match payload {
        OrganizationWirePayload::Membership(membership) => {
            require_schema(membership.schema_version)?;
            require_opaque_id(&membership.tenant_id)?;
            require_opaque_id(&membership.account_id)?;
            require_opaque_id(&membership.device_id)?;
            require_bounded_label(&membership.role)?;
            require_bounded_label(&membership.status)?;
            require_bounded_label(&membership.display_name)?;
            require_uuid_v7(membership.host_id)?;
            if membership.policy_revision == 0 {
                return Err(OrganizationCodecError::ZeroRevision);
            }
            require_same_tenant(expected_tenant, &membership.tenant_id)?;
            require_same_host(expected_host, membership.host_id)?;
        }
        OrganizationWirePayload::Policy(policy) => {
            require_schema(policy.schema_version)?;
            require_opaque_id(&policy.tenant_id)?;
            if policy.revision == 0 {
                return Err(OrganizationCodecError::ZeroRevision);
            }
            if policy.allowed_metadata_fields.len() > MAX_ORGANIZATION_COLLECTION_ITEMS
                || policy.prompt_maintainer_accounts.len() > MAX_ORGANIZATION_COLLECTION_ITEMS
            {
                return Err(OrganizationCodecError::BoundExceeded);
            }
            require_unique_strings(&policy.allowed_metadata_fields)?;
            require_unique_strings(&policy.prompt_maintainer_accounts)?;
            require_same_tenant(expected_tenant, &policy.tenant_id)?;
        }
        OrganizationWirePayload::ManagedTask(task) => {
            require_schema(task.schema_version)?;
            require_opaque_id(&task.tenant_id)?;
            require_opaque_id(&task.board_card_id)?;
            require_uuid_v7(task.host_id)?;
            require_uuid_v7(task.local_task_id)?;
            require_uuid_v7(task.link_id)?;
            if task.portal_revision == 0 || task.metadata_policy_version == 0 {
                return Err(OrganizationCodecError::ZeroRevision);
            }
            require_same_tenant(expected_tenant, &task.tenant_id)?;
            require_same_host(expected_host, task.host_id)?;
        }
        OrganizationWirePayload::PromptSnapshot(snapshot) => {
            require_schema(snapshot.schema_version)?;
            require_opaque_id(&snapshot.tenant_id)?;
            if snapshot.revision == 0 {
                return Err(OrganizationCodecError::ZeroRevision);
            }
            let item_limit = ORGANIZATION_PROMPT_PAGE_ITEM_LIMIT as usize;
            if snapshot.prompts.len() > item_limit
                || snapshot.versions.len() > item_limit
                || snapshot.chains.len() > item_limit
            {
                return Err(OrganizationCodecError::BoundExceeded);
            }
            let mut prompt_ids = std::collections::BTreeSet::new();
            for prompt in &snapshot.prompts {
                require_uuid_v7(prompt.prompt_id)?;
                require_uuid_v7(prompt.current_version_id)?;
                require_opaque_id(&prompt.tenant_id)?;
                require_bounded_label(&prompt.namespace)?;
                require_bounded_label(&prompt.name)?;
                require_bounded_label(&prompt.lifecycle)?;
                require_same_tenant(Some(snapshot.tenant_id.as_str()), &prompt.tenant_id)?;
                if !prompt_ids.insert(prompt.prompt_id) {
                    return Err(OrganizationCodecError::DuplicateId);
                }
            }
            let mut version_ids = std::collections::BTreeSet::new();
            for version in &snapshot.versions {
                require_uuid_v7(version.prompt_id)?;
                require_uuid_v7(version.version_id)?;
                require_opaque_id(&version.author)?;
                if version.body.len() > ORGANIZATION_PROMPT_BODY_LIMIT_BYTES as usize
                    || version.title.len() > ORGANIZATION_PROMPT_BODY_LIMIT_BYTES as usize
                    || version.tags.len() > item_limit
                {
                    return Err(OrganizationCodecError::BoundExceeded);
                }
                if !version_ids.insert(version.version_id) {
                    return Err(OrganizationCodecError::DuplicateId);
                }
            }
            let mut chain_ids = std::collections::BTreeSet::new();
            for chain in &snapshot.chains {
                require_uuid_v7(chain.chain_id)?;
                require_opaque_id(&chain.tenant_id)?;
                require_same_tenant(Some(snapshot.tenant_id.as_str()), &chain.tenant_id)?;
                if chain.revision == 0 {
                    return Err(OrganizationCodecError::ZeroRevision);
                }
                if chain.links.len() > item_limit {
                    return Err(OrganizationCodecError::BoundExceeded);
                }
                if !chain_ids.insert(chain.chain_id) {
                    return Err(OrganizationCodecError::DuplicateId);
                }
                let mut positions = std::collections::BTreeSet::new();
                for link in &chain.links {
                    require_uuid_v7(link.version_id)?;
                    if !positions.insert(link.position) {
                        return Err(OrganizationCodecError::DuplicateId);
                    }
                }
            }
            require_same_tenant(expected_tenant, &snapshot.tenant_id)?;
        }
        OrganizationWirePayload::LocalActionCatalog(catalog) => {
            require_schema(catalog.schema_version)?;
            require_opaque_id(&catalog.tenant_id)?;
            require_uuid_v7(catalog.host_id)?;
            if catalog.entries.len() > MAX_ORGANIZATION_COLLECTION_ITEMS {
                return Err(OrganizationCodecError::BoundExceeded);
            }
            let mut kinds = std::collections::BTreeSet::new();
            for entry in &catalog.entries {
                if entry.version == 0 {
                    return Err(OrganizationCodecError::ZeroRevision);
                }
                if !kinds.insert((entry.kind.clone(), entry.version)) {
                    return Err(OrganizationCodecError::DuplicateId);
                }
            }
            require_same_tenant(expected_tenant, &catalog.tenant_id)?;
            require_same_host(expected_host, catalog.host_id)?;
        }
        OrganizationWirePayload::LocalActionState(state) => {
            require_schema(state.schema_version)?;
            require_opaque_id(&state.tenant_id)?;
            require_uuid_v7(state.host_id)?;
            require_uuid_v7(state.request_id)?;
            require_same_tenant(expected_tenant, &state.tenant_id)?;
            require_same_host(expected_host, state.host_id)?;
        }
        OrganizationWirePayload::EvidenceMetadata(evidence) => {
            require_schema(evidence.schema_version)?;
            require_opaque_id(&evidence.tenant_id)?;
            require_uuid_v7(evidence.bundle_id)?;
            require_uuid_v7(evidence.draft_id)?;
            if evidence.raw_content_included {
                return Err(OrganizationCodecError::RawEvidence);
            }
            require_same_tenant(expected_tenant, &evidence.tenant_id)?;
        }
        OrganizationWirePayload::TelemetryIntent(intent) => {
            require_schema(intent.schema_version)?;
            require_opaque_id(&intent.tenant_id)?;
            require_uuid_v7(intent.host_id)?;
            if intent.observation_id_hex.len() != 64
                || !intent
                    .observation_id_hex
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit())
            {
                return Err(OrganizationCodecError::Malformed);
            }
            require_same_tenant(expected_tenant, &intent.tenant_id)?;
            require_same_host(expected_host, intent.host_id)?;
        }
        OrganizationWirePayload::FleetWatcher(view) => {
            require_schema(view.schema_version)?;
            require_opaque_id(&view.tenant_id)?;
            require_uuid_v7(view.host_id)?;
            if view.mutation_allowed {
                return Err(OrganizationCodecError::Malformed);
            }
            require_same_tenant(expected_tenant, &view.tenant_id)?;
            require_same_host(expected_host, view.host_id)?;
        }
        OrganizationWirePayload::TaskWatcher(view) => {
            require_schema(view.schema_version)?;
            require_opaque_id(&view.tenant_id)?;
            require_opaque_id(&view.board_card_id)?;
            require_uuid_v7(view.host_id)?;
            require_uuid_v7(view.task_id)?;
            if view.mutation_allowed {
                return Err(OrganizationCodecError::Malformed);
            }
            if let Some(label) = view.usage_source_label.as_deref() {
                require_bounded_label(label)?;
            }
            require_same_tenant(expected_tenant, &view.tenant_id)?;
            require_same_host(expected_host, view.host_id)?;
        }
    }
    Ok(())
}

fn require_schema(schema_version: u16) -> Result<(), OrganizationCodecError> {
    if schema_version != ORGANIZATION_SCHEMA_VERSION {
        Err(OrganizationCodecError::UnknownSchema)
    } else {
        Ok(())
    }
}

fn require_uuid_v7(id: Uuid) -> Result<(), OrganizationCodecError> {
    if id.get_version() != Some(Version::SortRand) {
        return Err(OrganizationCodecError::Malformed);
    }
    Ok(())
}

fn require_opaque_id(value: &str) -> Result<(), OrganizationCodecError> {
    if value.trim().is_empty() || value.len() > MAX_ORGANIZATION_OPAQUE_ID_BYTES {
        return Err(OrganizationCodecError::WrongIdentity);
    }
    Ok(())
}

fn require_bounded_label(value: &str) -> Result<(), OrganizationCodecError> {
    if value.trim().is_empty() || value.len() > MAX_ORGANIZATION_LABEL_BYTES {
        return Err(OrganizationCodecError::BoundExceeded);
    }
    Ok(())
}

fn require_same_tenant(expected: Option<&str>, actual: &str) -> Result<(), OrganizationCodecError> {
    match expected {
        Some(expected) if expected != actual => Err(OrganizationCodecError::WrongIdentity),
        _ => Ok(()),
    }
}

fn require_same_host(expected: Option<Uuid>, actual: Uuid) -> Result<(), OrganizationCodecError> {
    match expected {
        Some(expected) if expected != actual => Err(OrganizationCodecError::WrongIdentity),
        _ => Ok(()),
    }
}

fn require_unique_strings(values: &[String]) -> Result<(), OrganizationCodecError> {
    let mut seen = std::collections::BTreeSet::new();
    for value in values {
        require_bounded_label(value)?;
        if !seen.insert(value.as_str()) {
            return Err(OrganizationCodecError::DuplicateId);
        }
    }
    Ok(())
}

fn require_unique_uuids(values: &[Uuid]) -> Result<(), OrganizationCodecError> {
    let mut seen = std::collections::BTreeSet::new();
    for id in values {
        require_uuid_v7(*id)?;
        if !seen.insert(*id) {
            return Err(OrganizationCodecError::DuplicateId);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::CapabilitySet;

    #[test]
    fn standalone_hello_excludes_organization_capability() {
        let advertised = CapabilitySet::empty();
        assert!(!advertised.contains(organization_capability_for_standalone()));
        assert_eq!(OrganizationExtensionKind::Membership.type_id(), 1001);
        assert_eq!(ORGANIZATION_SCHEMA_VERSION, 1);
        assert_eq!(ORGANIZATION_PROMPT_BODY_LIMIT_BYTES, 256 * 1024);
        assert_eq!(ORGANIZATION_PROMPT_PAGE_ITEM_LIMIT, 100);
    }

    #[test]
    fn membership_payload_round_trips_and_keeps_opaque_ids() {
        let host_id = Uuid::now_v7();
        let payload = OrganizationWirePayload::Membership(OrganizationMembershipWire {
            schema_version: ORGANIZATION_SCHEMA_VERSION,
            tenant_id: "acme".to_string(),
            account_id: "owner-1".to_string(),
            host_id,
            device_id: "device-9".to_string(),
            role: "owner".to_string(),
            status: "enrolled".to_string(),
            display_name: "owner-host".to_string(),
            policy_revision: 1,
            enrolled_at_ms: 1_000,
            last_seen_ms: 1_000,
        });
        let encoded = encode_organization_payload(&payload).expect("encode");
        let decoded = decode_organization_payload(&encoded).expect("decode");
        assert_eq!(decoded, payload);
        match decoded {
            OrganizationWirePayload::Membership(membership) => {
                assert_eq!(membership.tenant_id, "acme");
                assert_eq!(membership.host_id, host_id);
                assert_eq!(membership.host_id.get_version(), Some(Version::SortRand));
            }
            other => panic!("unexpected payload: {other:?}"),
        }
    }

    #[test]
    fn unknown_schema_and_zero_revision_fail_closed() {
        let host_id = Uuid::now_v7();
        let mut payload = OrganizationWirePayload::ManagedTask(OrganizationManagedTaskWire {
            schema_version: 2,
            tenant_id: "acme".to_string(),
            host_id,
            local_task_id: Uuid::now_v7(),
            link_id: Uuid::now_v7(),
            board_card_id: "board-card-1".to_string(),
            enrollment_state: "enrolled".to_string(),
            portal_revision: 1,
            metadata_policy_version: 1,
            linked_by: "portal".to_string(),
            linked_at: 1,
            unlinked_at: None,
            portal_title: None,
        });
        assert_eq!(
            encode_organization_payload(&payload),
            Err(OrganizationCodecError::UnknownSchema)
        );
        payload = OrganizationWirePayload::ManagedTask(OrganizationManagedTaskWire {
            schema_version: ORGANIZATION_SCHEMA_VERSION,
            portal_revision: 0,
            ..match payload {
                OrganizationWirePayload::ManagedTask(task) => task,
                _ => unreachable!(),
            }
        });
        assert_eq!(
            encode_organization_payload(&payload),
            Err(OrganizationCodecError::ZeroRevision)
        );
    }

    #[test]
    fn raw_evidence_payload_is_rejected() {
        let payload = OrganizationWirePayload::EvidenceMetadata(OrganizationEvidenceMetadataWire {
            schema_version: ORGANIZATION_SCHEMA_VERSION,
            tenant_id: "acme".to_string(),
            bundle_id: Uuid::now_v7(),
            draft_id: Uuid::now_v7(),
            title: "draft".to_string(),
            summary: "summary".to_string(),
            content_hash_hex: "ab".repeat(32),
            signer: "owner".to_string(),
            capture_started_at_ms: 1,
            capture_ended_at_ms: 2,
            raw_content_included: true,
        });
        assert_eq!(
            encode_organization_payload(&payload),
            Err(OrganizationCodecError::RawEvidence)
        );
    }
}
