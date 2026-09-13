//! Versioned durable organization state. Persists bounded metadata only.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use uuid::{Uuid, Version};

use crate::connect::ConnectHostId;
use crate::org::error::{OrgDependency, OrgError};
use crate::org::evidence::EvidenceIntake;
use crate::org::identity::{ExternalAccount, PortalAccountId, PortalDeviceId, PortalTenantId};
use crate::org::local_actions::LocalActionRegistry;
use crate::org::managed::{ManagedTaskLink, TaskLinkReducer};
use crate::org::membership::{HostMembership, MembershipStatus, OrganizationPolicyDocument};
use crate::org::{
    OperatingMode, OrganizationCapabilityDisableReason, OrganizationCapabilityState,
    OrganizationProjection, OrganizationSyncState, MAX_SEEN_FACTS,
};
use crate::prompts::{OrganizationPromptSnapshot, ORG_PROMPT_CACHE_TTL_MS};

pub const ORGANIZATION_STATE_SCHEMA_VERSION: u16 = 1;
pub const ORGANIZATION_STATE_FILE_NAME: &str = "organization-state.json";
pub const MAX_ORGANIZATION_STATE_BYTES: usize = 512 * 1024;
pub const MAX_ORGANIZATION_OUTBOX_INTENTS: usize = 256;
pub const MAX_ORGANIZATION_OUTBOX_INTENT_BYTES: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutboxDeliveryState {
    Queued,
    LocallyAcknowledged,
    Uncertain,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersistedOutboxIntent {
    pub observation_id_hex: String,
    pub intent: String,
    pub publication_queued: bool,
    pub delivery: OutboxDeliveryState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersistedSeenFact {
    pub key: String,
    pub hash_hex: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrganizationStateDocument {
    pub schema_version: u16,
    pub tenant_id: Option<String>,
    pub account_id: Option<String>,
    pub host_id: Option<Uuid>,
    pub device_id: Option<String>,
    pub sync_state: OrganizationSyncState,
    pub authenticated_online: bool,
    pub membership: Option<HostMembership>,
    pub policy: Option<OrganizationPolicyDocument>,
    pub membership_revision: Option<u64>,
    pub membership_fact_hash: Option<String>,
    pub seen_facts: Vec<PersistedSeenFact>,
    pub managed_links: Vec<ManagedTaskLink>,
    pub prompt_snapshot: Option<OrganizationPromptSnapshot>,
    pub local_action_catalog: Vec<crate::org::LocalActionCatalogEntry>,
    pub local_action_states: Vec<crate::org::LocalActionAdmissionState>,
    pub local_action_seen: Vec<String>,
    pub evidence_imported_ids: Vec<String>,
    pub evidence_trusted_signers: Vec<String>,
    pub evidence_e2e_raw_authorized: bool,
    pub telemetry_outbox: Vec<PersistedOutboxIntent>,
}

pub fn validate_outbox_intent(intent: &PersistedOutboxIntent) -> Result<(), OrgError> {
    if intent.observation_id_hex.len() != 64
        || !intent
            .observation_id_hex
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(OrgError::EmptyIdentity);
    }
    if intent.intent.trim().is_empty() || intent.intent.len() > MAX_ORGANIZATION_OUTBOX_INTENT_BYTES
    {
        return Err(OrgError::EmptyIdentity);
    }
    Ok(())
}

#[derive(Debug)]
pub struct OrganizationHelloRestore {
    projection: OrganizationProjection,
    diagnostic: Option<String>,
}

impl OrganizationHelloRestore {
    pub fn projection(&self) -> &OrganizationProjection {
        &self.projection
    }

    pub fn into_projection(self) -> OrganizationProjection {
        self.projection
    }

    pub fn diagnostic(&self) -> Option<&str> {
        self.diagnostic.as_deref()
    }

    pub fn capability(&self) -> OrganizationCapabilityState {
        self.projection.capability_state()
    }
}

#[derive(Debug, Clone)]
pub struct OrganizationStateStore {
    path: PathBuf,
}

impl OrganizationStateStore {
    pub fn open(profile_root: impl AsRef<Path>) -> Self {
        Self {
            path: profile_root.as_ref().join(ORGANIZATION_STATE_FILE_NAME),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn restore_hello(profile_root: impl AsRef<Path>) -> OrganizationHelloRestore {
        let store = Self::open(profile_root);
        match store.load() {
            Ok(projection) => OrganizationHelloRestore {
                projection,
                diagnostic: None,
            },
            Err(OrgError::StandaloneMode) => OrganizationHelloRestore {
                projection: OrganizationProjection::standalone(),
                diagnostic: None,
            },
            Err(error) => OrganizationHelloRestore {
                projection: OrganizationProjection::standalone(),
                diagnostic: Some(format!(
                    "organization state ignored; host remains standalone ({error})"
                )),
            },
        }
    }

    pub fn load(&self) -> Result<OrganizationProjection, OrgError> {
        match fs::read(&self.path) {
            Ok(bytes) => OrganizationProjection::restore_from_bytes(&bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Err(OrgError::StandaloneMode)
            }
            Err(_) => Err(OrgError::CorruptState),
        }
    }

    pub fn save(&self, projection: &OrganizationProjection) -> Result<(), OrgError> {
        let document = projection.export_state()?;
        let encoded = encode_state_document(&document)?;
        atomic_write(&self.path, &encoded)
    }
}

impl OrganizationProjection {
    pub fn export_state(&self) -> Result<OrganizationStateDocument, OrgError> {
        if self.telemetry_outbox().len() > MAX_ORGANIZATION_OUTBOX_INTENTS {
            return Err(OrgError::BoundExceeded);
        }
        let membership = self.membership().cloned();
        let signed_in = match self.mode() {
            OperatingMode::ConnectSignedIn { account } => Some(account.clone()),
            _ => None,
        };
        let tenant_id = membership
            .as_ref()
            .map(|membership| membership.tenant_id.as_str().to_string())
            .or_else(|| {
                signed_in
                    .as_ref()
                    .map(|account| account.tenant_id.as_str().to_string())
            });
        let account_id = membership
            .as_ref()
            .map(|membership| membership.account_id.as_str().to_string())
            .or_else(|| {
                signed_in
                    .as_ref()
                    .map(|account| account.account_id.as_str().to_string())
            });
        Ok(OrganizationStateDocument {
            schema_version: ORGANIZATION_STATE_SCHEMA_VERSION,
            tenant_id,
            account_id,
            host_id: membership
                .as_ref()
                .map(|membership| Uuid::from_bytes(membership.host_id.as_bytes())),
            device_id: membership
                .as_ref()
                .and_then(|membership| membership.device_id.as_ref())
                .or_else(|| {
                    signed_in
                        .as_ref()
                        .and_then(|account| account.device_id.as_ref())
                })
                .map(|device| device.as_str().to_string()),
            sync_state: self.sync_state(),
            authenticated_online: self.authenticated_online(),
            membership,
            policy: self.policy().cloned(),
            membership_revision: self.membership_revision(),
            membership_fact_hash: self.membership_fact_hash_hex(),
            seen_facts: self.persisted_seen_facts(),
            managed_links: self.persisted_links().cloned().collect(),
            prompt_snapshot: self.prompt_snapshot().cloned(),
            local_action_catalog: self.local_actions().persist_catalog(),
            local_action_states: self.local_actions().persist_states(),
            local_action_seen: self.local_actions().persist_seen(),
            evidence_imported_ids: self.evidence().persist_imported_ids(),
            evidence_trusted_signers: self.evidence().persist_trusted_signers(),
            evidence_e2e_raw_authorized: self.evidence().persist_e2e_raw_authorized(),
            telemetry_outbox: self.telemetry_outbox().values().cloned().collect(),
        })
    }

    pub fn restore_from_bytes(bytes: &[u8]) -> Result<Self, OrgError> {
        if bytes.len() > MAX_ORGANIZATION_STATE_BYTES {
            return Err(OrgError::BoundExceeded);
        }
        let document: OrganizationStateDocument =
            serde_json::from_slice(bytes).map_err(|_| OrgError::CorruptState)?;
        Self::restore_from_document(document)
    }

    pub fn restore_from_document(document: OrganizationStateDocument) -> Result<Self, OrgError> {
        validate_document(&document)?;
        // Resolve the mode while the complete document is still borrowable.
        // The fields below are then moved into their bounded restorers.
        let mode = restore_mode(&document)?;
        let local_actions = LocalActionRegistry::restore(
            document.local_action_catalog,
            document.local_action_states,
            document.local_action_seen,
        )?;
        let links = TaskLinkReducer::restore(document.managed_links)?;
        let evidence_tenant = document
            .tenant_id
            .as_deref()
            .map(PortalTenantId::parse)
            .transpose()
            .map_err(|_| OrgError::CorruptState)?;
        let evidence = EvidenceIntake::restore(
            document.evidence_imported_ids,
            document.evidence_trusted_signers,
            evidence_tenant,
            document.evidence_e2e_raw_authorized,
        )?;
        let mut seen_facts = BTreeMap::new();
        for fact in document.seen_facts {
            if seen_facts
                .insert(fact.key, decode_hash32(&fact.hash_hex)?)
                .is_some()
            {
                return Err(OrgError::Replay);
            }
        }
        let membership_fact_hash = document
            .membership_fact_hash
            .as_deref()
            .map(decode_hash32)
            .transpose()?;
        let mut telemetry_outbox = BTreeMap::new();
        for intent in document.telemetry_outbox {
            validate_outbox_intent(&intent)?;
            if telemetry_outbox
                .insert(intent.observation_id_hex.clone(), intent)
                .is_some()
            {
                return Err(OrgError::Replay);
            }
        }
        let prompt_snapshot = document.prompt_snapshot.clone();
        let mut projection = Self::restore_parts(
            mode,
            document.policy,
            document.authenticated_online,
            document.membership_revision,
            membership_fact_hash,
            seen_facts,
            links,
            local_actions,
            evidence,
            document.prompt_snapshot,
            telemetry_outbox,
            document.sync_state,
        )?;
        if let Some(snapshot) = prompt_snapshot {
            let now_ms = projection
                .membership()
                .and_then(|membership| membership.last_seen_at_ms)
                .unwrap_or(1);
            projection.apply_prompt_snapshot(
                snapshot,
                now_ms,
                now_ms.saturating_add(ORG_PROMPT_CACHE_TTL_MS as i64),
            )?;
        }
        Ok(projection)
    }
}

fn restore_mode(document: &OrganizationStateDocument) -> Result<OperatingMode, OrgError> {
    match document.sync_state {
        OrganizationSyncState::Standalone => Ok(OperatingMode::anonymous()),
        OrganizationSyncState::SignedIn => {
            let tenant_id = document
                .tenant_id
                .as_deref()
                .ok_or(OrgError::CorruptState)
                .and_then(|value| {
                    PortalTenantId::parse(value).map_err(|_| OrgError::CorruptState)
                })?;
            let account_id = document
                .account_id
                .as_deref()
                .ok_or(OrgError::CorruptState)
                .and_then(|value| {
                    PortalAccountId::parse(value).map_err(|_| OrgError::CorruptState)
                })?;
            let device_id = document
                .device_id
                .as_deref()
                .map(PortalDeviceId::parse)
                .transpose()
                .map_err(|_| OrgError::CorruptState)?;
            Ok(OperatingMode::ConnectSignedIn {
                account: ExternalAccount::new(tenant_id, account_id, device_id),
            })
        }
        OrganizationSyncState::Enrolled
        | OrganizationSyncState::Unlinked
        | OrganizationSyncState::Revoked
        | OrganizationSyncState::Expired => {
            let membership = document.membership.clone().ok_or(OrgError::CorruptState)?;
            if let Some(host_id) = document.host_id {
                let restored =
                    ConnectHostId::from_uuid(host_id).map_err(|_| OrgError::CorruptState)?;
                if restored != membership.host_id {
                    return Err(OrgError::CorruptState);
                }
            }
            Ok(OperatingMode::HostEnrolled { membership })
        }
    }
}

fn validate_document(document: &OrganizationStateDocument) -> Result<(), OrgError> {
    if document.schema_version != ORGANIZATION_STATE_SCHEMA_VERSION {
        return Err(OrgError::CorruptState);
    }
    if let Some(host_id) = document.host_id {
        require_uuid_v7(host_id)?;
    }
    if let Some(tenant_id) = document.tenant_id.as_deref() {
        PortalTenantId::parse(tenant_id).map_err(|_| OrgError::CorruptState)?;
    }
    if let Some(account_id) = document.account_id.as_deref() {
        PortalAccountId::parse(account_id).map_err(|_| OrgError::CorruptState)?;
    }
    if document.seen_facts.len() > MAX_SEEN_FACTS
        || document.telemetry_outbox.len() > MAX_ORGANIZATION_OUTBOX_INTENTS
    {
        return Err(OrgError::BoundExceeded);
    }
    let mut outbox_ids = std::collections::BTreeSet::new();
    for intent in &document.telemetry_outbox {
        validate_outbox_intent(intent)?;
        if !outbox_ids.insert(intent.observation_id_hex.as_str()) {
            return Err(OrgError::Replay);
        }
    }
    if let (Some(membership), Some(policy)) = (&document.membership, &document.policy) {
        if membership.tenant_id != policy.tenant_id {
            return Err(OrgError::CrossTenant);
        }
        if let Some(tenant_id) = document.tenant_id.as_deref() {
            if membership.tenant_id.as_str() != tenant_id {
                return Err(OrgError::CrossTenant);
            }
        }
        if let Some(host_id) = document.host_id {
            if Uuid::from_bytes(membership.host_id.as_bytes()) != host_id {
                return Err(OrgError::CorruptState);
            }
        }
        if matches!(
            document.sync_state,
            OrganizationSyncState::Enrolled
                | OrganizationSyncState::Revoked
                | OrganizationSyncState::Expired
        ) && membership.policy_revision != policy.revision
        {
            return Err(OrgError::StalePolicy);
        }
    }
    if document.sync_state == OrganizationSyncState::Enrolled {
        let membership = document.membership.as_ref().ok_or(OrgError::CorruptState)?;
        if !membership.is_enrolled() || membership.status != MembershipStatus::Enrolled {
            return Err(OrgError::CorruptState);
        }
        if document.policy.is_none() {
            return Err(OrgError::CorruptState);
        }
        for link in &document.managed_links {
            if link.tenant_id != membership.tenant_id {
                return Err(OrgError::CrossTenant);
            }
            if link.host_id != membership.host_id {
                return Err(OrgError::CorruptState);
            }
        }
    }
    if document.sync_state == OrganizationSyncState::Standalone
        && (document.membership.is_some() || !document.managed_links.is_empty())
    {
        return Err(OrgError::CorruptState);
    }
    Ok(())
}

fn encode_state_document(document: &OrganizationStateDocument) -> Result<Vec<u8>, OrgError> {
    let encoded = serde_json::to_vec(document).map_err(|_| OrgError::CorruptState)?;
    if encoded.len() > MAX_ORGANIZATION_STATE_BYTES {
        return Err(OrgError::BoundExceeded);
    }
    Ok(encoded)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), OrgError> {
    let parent = path
        .parent()
        .ok_or(OrgError::Unavailable(OrgDependency::DurableOutbox))?;
    fs::create_dir_all(parent).map_err(|_| OrgError::Unavailable(OrgDependency::DurableOutbox))?;
    let tmp = parent.join(format!(
        ".organization-state.{}.tmp",
        Uuid::now_v7().as_simple()
    ));
    let mut guard = TempPath(None);
    {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .map_err(|_| OrgError::Unavailable(OrgDependency::DurableOutbox))?;
        guard.0 = Some(tmp.clone());
        file.write_all(bytes)
            .map_err(|_| OrgError::Unavailable(OrgDependency::DurableOutbox))?;
        file.sync_all()
            .map_err(|_| OrgError::Unavailable(OrgDependency::DurableOutbox))?;
    }
    replace_file(&tmp, path)?;
    sync_parent_directory(parent)?;
    guard.0 = None;
    Ok(())
}

struct TempPath(Option<PathBuf>);

impl Drop for TempPath {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = fs::remove_file(path);
        }
    }
}

fn replace_file(from: &Path, to: &Path) -> Result<(), OrgError> {
    #[cfg(unix)]
    {
        fs::rename(from, to).map_err(|_| OrgError::Unavailable(OrgDependency::DurableOutbox))
    }
    #[cfg(windows)]
    {
        replace_file_windows(from, to)
    }
    #[cfg(not(any(unix, windows)))]
    {
        fs::rename(from, to).map_err(|_| OrgError::Unavailable(OrgDependency::DurableOutbox))
    }
}

#[cfg(unix)]
fn sync_parent_directory(parent: &Path) -> Result<(), OrgError> {
    fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| OrgError::Unavailable(OrgDependency::DurableOutbox))
}

#[cfg(windows)]
fn sync_parent_directory(_parent: &Path) -> Result<(), OrgError> {
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn sync_parent_directory(_parent: &Path) -> Result<(), OrgError> {
    Ok(())
}

#[cfg(windows)]
fn replace_file_windows(from: &Path, to: &Path) -> Result<(), OrgError> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    fn wide(path: &Path) -> Vec<u16> {
        path.as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    let from_w = wide(from);
    let to_w = wide(to);
    unsafe {
        MoveFileExW(
            PCWSTR(from_w.as_ptr()),
            PCWSTR(to_w.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
        .map_err(|_| OrgError::Unavailable(OrgDependency::DurableOutbox))
    }
}

fn decode_hash32(hex: &str) -> Result<[u8; 32], OrgError> {
    if hex.len() != 64 {
        return Err(OrgError::CorruptState);
    }
    let mut out = [0u8; 32];
    for (index, chunk) in hex.as_bytes().chunks(2).enumerate() {
        let value = std::str::from_utf8(chunk)
            .ok()
            .and_then(|part| u8::from_str_radix(part, 16).ok())
            .ok_or(OrgError::CorruptState)?;
        out[index] = value;
    }
    Ok(out)
}

fn require_uuid_v7(id: Uuid) -> Result<(), OrgError> {
    if id.get_version() != Some(Version::SortRand) {
        return Err(OrgError::CorruptState);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::org::identity::ExternalAccount;
    use crate::org::{
        HostMembership, MembershipRole, OrganizationPolicyDocument, OrganizationProjection,
        PortalAccountId, PortalTenantId,
    };

    fn tenant() -> PortalTenantId {
        PortalTenantId::parse("acme").expect("tenant")
    }

    fn enroll(projection: &mut OrganizationProjection) -> HostMembership {
        let host_id = ConnectHostId::new();
        let account = ExternalAccount::new(
            tenant(),
            PortalAccountId::parse("owner-1").expect("account"),
            None,
        );
        assert_eq!(projection.sign_in(account.clone()), 0);
        let policy = OrganizationPolicyDocument::deny_minimal(tenant()).expect("policy");
        let pending = HostMembership::pending(
            host_id,
            account,
            MembershipRole::Owner,
            &policy,
            "owner-host",
        )
        .expect("pending");
        projection
            .confirm_enrollment(pending, policy, 1_000)
            .expect("enrolled")
    }

    #[test]
    fn missing_file_is_standalone_and_corrupt_is_rejected() {
        let root = std::env::temp_dir().join(format!(
            "devmanager-org-persist-{}",
            Uuid::now_v7().as_simple()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("temp root");
        let store = OrganizationStateStore::open(&root);
        assert_eq!(store.load().expect_err("missing"), OrgError::StandaloneMode);
        let hello = OrganizationStateStore::restore_hello(&root);
        assert_eq!(
            hello.capability(),
            OrganizationCapabilityState::Disabled(OrganizationCapabilityDisableReason::Standalone)
        );
        fs::write(store.path(), "{not-json").expect("corrupt");
        assert_eq!(store.load().expect_err("corrupt"), OrgError::CorruptState);
        let hello = OrganizationStateStore::restore_hello(&root);
        assert!(hello.diagnostic().is_some());
        assert_eq!(
            hello.projection().sync_state(),
            OrganizationSyncState::Standalone
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn enrolled_state_round_trips_and_cross_tenant_is_rejected() {
        let root = std::env::temp_dir().join(format!(
            "devmanager-org-persist-enroll-{}",
            Uuid::now_v7().as_simple()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("temp root");
        let store = OrganizationStateStore::open(&root);
        let mut projection = OrganizationProjection::standalone();
        enroll(&mut projection);
        store.save(&projection).expect("save");
        let restored = store.load().expect("load");
        assert_eq!(restored.sync_state(), OrganizationSyncState::Enrolled);
        assert_eq!(
            restored
                .membership()
                .map(|membership| membership.tenant_id.clone()),
            Some(tenant())
        );

        let mut document = restored.export_state().expect("export");
        document.tenant_id = Some("other".to_string());
        assert!(matches!(
            OrganizationProjection::restore_from_document(document),
            Err(OrgError::CrossTenant)
        ));
        let _ = fs::remove_dir_all(&root);
    }
}
