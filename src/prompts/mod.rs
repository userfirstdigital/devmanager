pub mod diff;
pub mod diff_service;
pub mod history;
pub mod model;
pub mod projection;
pub mod search;
pub mod service;
pub mod store;

pub use diff::*;
pub use diff_service::*;
pub use history::*;
pub use model::*;
pub use projection::*;
pub use search::*;
pub use service::*;
pub use store::*;

/// Canonical prompt-library UI surface without compiling `ui` twice.
///
/// Nested module (not `pub use crate::ui as ui`) so rustfmt cannot collapse the
/// alias and erase the `prompts::ui::…` path clients/tests rely on.
pub mod ui {
    pub use crate::ui::*;
}

pub mod organization {
    //! Published organization prompt libraries and linear guided chains.
    //!
    //! Connect is authoritative for published content. Selection copies one exact
    //! immutable version into the local composer and never sends or advances.

    use std::collections::BTreeMap;

    use serde::{Deserialize, Serialize};
    use sha2::{Digest, Sha256};

    use crate::org::{
        HostMembership, MembershipRole, OrgError, OrgPromptChainId, OrgPromptId,
        OrgPromptVersionId, OrganizationPolicyDocument, PortalAccountId, PortalTenantId,
        SyncOutcome,
    };
    use crate::protocol::{
        ORGANIZATION_PROMPT_BODY_LIMIT_BYTES, ORGANIZATION_PROMPT_PAGE_ENCODED_LIMIT_BYTES,
        ORGANIZATION_PROMPT_PAGE_ITEM_LIMIT,
    };

    pub const ORG_PROMPT_CACHE_TTL_MS: u64 = 24 * 60 * 60 * 1_000;

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum PromptLifecycle {
        Published,
        Deprecated,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct OrgPromptVersion {
        pub prompt_id: OrgPromptId,
        pub version_id: OrgPromptVersionId,
        pub author: PortalAccountId,
        pub title: String,
        pub tags: Vec<String>,
        pub body: String,
        pub content_hash_hex: String,
        pub published_at_ms: i64,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct OrgPrompt {
        pub prompt_id: OrgPromptId,
        pub tenant_id: PortalTenantId,
        pub namespace: String,
        pub name: String,
        pub current_version_id: OrgPromptVersionId,
        pub lifecycle: PromptLifecycle,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct OrgPromptChainLink {
        pub position: u32,
        pub version_id: OrgPromptVersionId,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct OrgPromptChain {
        pub chain_id: OrgPromptChainId,
        pub tenant_id: PortalTenantId,
        pub revision: u32,
        pub links: Vec<OrgPromptChainLink>,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct ComposerInsertion {
        pub version_id: OrgPromptVersionId,
        pub body: String,
        pub sent: bool,
        pub advanced: bool,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct CachedPrompt {
        pub version: OrgPromptVersion,
        pub cached_at_ms: i64,
        pub entitlement_expires_at_ms: i64,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct OrganizationPromptSnapshot {
        pub tenant_id: PortalTenantId,
        pub revision: u32,
        pub prompts: Vec<OrgPrompt>,
        pub versions: Vec<OrgPromptVersion>,
        pub chains: Vec<OrgPromptChain>,
    }

    #[derive(Debug, Default)]
    pub struct OrganizationPromptProjection {
        prompts: BTreeMap<String, OrgPrompt>,
        versions: BTreeMap<String, OrgPromptVersion>,
        chains: BTreeMap<String, OrgPromptChain>,
        cache: BTreeMap<String, CachedPrompt>,
        snapshot_revision: u32,
        accepted_snapshot: Option<OrganizationPromptSnapshot>,
    }

    impl OrganizationPromptProjection {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn publish(
            &mut self,
            membership: &HostMembership,
            policy: &OrganizationPolicyDocument,
            tenant_id: PortalTenantId,
            namespace: impl Into<String>,
            name: impl Into<String>,
            title: impl Into<String>,
            tags: Vec<String>,
            body: impl Into<String>,
            now_ms: i64,
        ) -> Result<OrgPromptVersion, OrgError> {
            authorize_publish(membership, policy)?;
            if membership.tenant_id != tenant_id {
                return Err(OrgError::CrossTenant);
            }
            let body = body.into();
            if body.len() > ORGANIZATION_PROMPT_BODY_LIMIT_BYTES as usize {
                return Err(OrgError::BoundExceeded);
            }
            let name = name.into();
            let namespace = namespace.into();
            let key = prompt_key(&tenant_id, &namespace, &name);
            if let Some(existing) = self.prompts.get(&key) {
                if existing.lifecycle == PromptLifecycle::Published {
                    return Err(OrgError::DuplicateLink);
                }
            }
            let version = OrgPromptVersion {
                prompt_id: OrgPromptId::new(),
                version_id: OrgPromptVersionId::new(),
                author: membership.account_id.clone(),
                title: title.into(),
                tags,
                content_hash_hex: hash_body(&body),
                body,
                published_at_ms: now_ms,
            };
            self.prompts.insert(
                key,
                OrgPrompt {
                    prompt_id: version.prompt_id,
                    tenant_id,
                    namespace,
                    name,
                    current_version_id: version.version_id,
                    lifecycle: PromptLifecycle::Published,
                },
            );
            self.versions
                .insert(version.version_id.to_string(), version.clone());
            Ok(version)
        }

        pub fn supersede(
            &mut self,
            membership: &HostMembership,
            policy: &OrganizationPolicyDocument,
            version_id: OrgPromptVersionId,
            body: impl Into<String>,
            now_ms: i64,
        ) -> Result<OrgPromptVersion, OrgError> {
            authorize_publish(membership, policy)?;
            let previous = self
                .versions
                .get(&version_id.to_string())
                .cloned()
                .ok_or(OrgError::Unlinked)?;
            let body = body.into();
            if body.len() > ORGANIZATION_PROMPT_BODY_LIMIT_BYTES as usize {
                return Err(OrgError::BoundExceeded);
            }
            let next = OrgPromptVersion {
                prompt_id: previous.prompt_id,
                version_id: OrgPromptVersionId::new(),
                author: membership.account_id.clone(),
                title: previous.title,
                tags: previous.tags,
                content_hash_hex: hash_body(&body),
                body,
                published_at_ms: now_ms,
            };
            if let Some(prompt) = self
                .prompts
                .values_mut()
                .find(|prompt| prompt.prompt_id == previous.prompt_id)
            {
                prompt.current_version_id = next.version_id;
            }
            self.versions
                .insert(next.version_id.to_string(), next.clone());
            Ok(next)
        }

        pub fn mutate_old_version(
            &mut self,
            version_id: OrgPromptVersionId,
            _body: &str,
        ) -> Result<(), OrgError> {
            if self.versions.contains_key(&version_id.to_string()) {
                return Err(OrgError::ImmutableVersion);
            }
            Err(OrgError::Unlinked)
        }

        pub fn create_chain(
            &mut self,
            membership: &HostMembership,
            policy: &OrganizationPolicyDocument,
            version_ids: &[OrgPromptVersionId],
        ) -> Result<OrgPromptChain, OrgError> {
            authorize_publish(membership, policy)?;
            if version_ids.len() > ORGANIZATION_PROMPT_PAGE_ITEM_LIMIT as usize {
                return Err(OrgError::BoundExceeded);
            }
            let chain = OrgPromptChain {
                chain_id: OrgPromptChainId::new(),
                tenant_id: membership.tenant_id.clone(),
                revision: 1,
                links: version_ids
                    .iter()
                    .enumerate()
                    .map(|(index, version_id)| OrgPromptChainLink {
                        position: index as u32,
                        version_id: *version_id,
                    })
                    .collect(),
            };
            self.chains
                .insert(chain.chain_id.to_string(), chain.clone());
            Ok(chain)
        }

        pub fn insert_between(
            &mut self,
            membership: &HostMembership,
            policy: &OrganizationPolicyDocument,
            chain_id: OrgPromptChainId,
            after_position: u32,
            version_id: OrgPromptVersionId,
        ) -> Result<OrgPromptChain, OrgError> {
            authorize_publish(membership, policy)?;
            let chain = self
                .chains
                .get_mut(&chain_id.to_string())
                .ok_or(OrgError::Unlinked)?;
            let insert_at = chain
                .links
                .iter()
                .position(|link| link.position == after_position)
                .map(|index| index + 1)
                .unwrap_or(chain.links.len());
            chain.links.insert(
                insert_at,
                OrgPromptChainLink {
                    position: 0,
                    version_id,
                },
            );
            for (index, link) in chain.links.iter_mut().enumerate() {
                link.position = index as u32;
            }
            chain.revision = chain.revision.saturating_add(1);
            Ok(chain.clone())
        }

        pub fn cache_version(
            &mut self,
            membership: &HostMembership,
            version: OrgPromptVersion,
            now_ms: i64,
            entitlement_expires_at_ms: i64,
        ) -> Result<(), OrgError> {
            if !membership.is_enrolled() || !membership.role.can_read_published() {
                return Err(OrgError::RoleDenied);
            }
            self.cache.insert(
                version.version_id.to_string(),
                CachedPrompt {
                    version,
                    cached_at_ms: now_ms,
                    entitlement_expires_at_ms,
                },
            );
            Ok(())
        }

        pub fn read_cached_body(
            &self,
            version_id: OrgPromptVersionId,
            now_ms: i64,
        ) -> Result<&str, OrgError> {
            let cached = self
                .cache
                .get(&version_id.to_string())
                .ok_or(OrgError::Unlinked)?;
            let cache_deadline = cached
                .cached_at_ms
                .saturating_add(ORG_PROMPT_CACHE_TTL_MS as i64);
            if now_ms >= cached.entitlement_expires_at_ms || now_ms >= cache_deadline {
                return Err(OrgError::Expired);
            }
            Ok(cached.version.body.as_str())
        }

        pub fn purge(&mut self) {
            self.cache.clear();
            self.prompts.clear();
            self.versions.clear();
            self.chains.clear();
            self.snapshot_revision = 0;
            self.accepted_snapshot = None;
        }

        pub fn put_in_composer(
            &self,
            version_id: OrgPromptVersionId,
            now_ms: i64,
        ) -> Result<ComposerInsertion, OrgError> {
            let body = self.read_cached_body(version_id, now_ms)?.to_string();
            Ok(ComposerInsertion {
                version_id,
                body,
                sent: false,
                advanced: false,
            })
        }

        pub fn encoded_page_limit() -> u32 {
            ORGANIZATION_PROMPT_PAGE_ENCODED_LIMIT_BYTES
        }

        pub fn snapshot_revision(&self) -> u32 {
            self.snapshot_revision
        }

        pub fn prompt_count(&self) -> usize {
            self.prompts.len()
        }

        pub fn version(&self, version_id: OrgPromptVersionId) -> Option<&OrgPromptVersion> {
            self.versions.get(&version_id.to_string())
        }

        pub fn apply_authoritative_snapshot(
            &mut self,
            membership: &HostMembership,
            snapshot: OrganizationPromptSnapshot,
            now_ms: i64,
            entitlement_expires_at_ms: i64,
        ) -> Result<SyncOutcome, OrgError> {
            if !membership.is_enrolled() || !membership.role.can_read_published() {
                return Err(OrgError::RoleDenied);
            }
            if membership.tenant_id != snapshot.tenant_id {
                return Err(OrgError::CrossTenant);
            }
            if snapshot.revision == 0 {
                return Err(OrgError::StalePolicy);
            }
            if snapshot.prompts.len() > ORGANIZATION_PROMPT_PAGE_ITEM_LIMIT as usize
                || snapshot.versions.len() > ORGANIZATION_PROMPT_PAGE_ITEM_LIMIT as usize
                || snapshot.chains.len() > ORGANIZATION_PROMPT_PAGE_ITEM_LIMIT as usize
            {
                return Err(OrgError::BoundExceeded);
            }
            if self.snapshot_revision > snapshot.revision {
                return Err(OrgError::StalePolicy);
            }
            if self.snapshot_revision == snapshot.revision && self.snapshot_revision != 0 {
                return if self.accepted_snapshot.as_ref() == Some(&snapshot) {
                    Ok(SyncOutcome::Duplicate)
                } else {
                    Err(OrgError::LastWriteWinsForbidden)
                };
            }
            validate_prompt_snapshot(self, &snapshot)?;
            let mut prompts = self.prompts.clone();
            let mut versions = self.versions.clone();
            let mut chains = self.chains.clone();
            let mut cache = self.cache.clone();
            for prompt in &snapshot.prompts {
                prompts.insert(
                    prompt_key(&prompt.tenant_id, &prompt.namespace, &prompt.name),
                    prompt.clone(),
                );
            }
            for version in &snapshot.versions {
                versions.insert(version.version_id.to_string(), version.clone());
                cache.insert(
                    version.version_id.to_string(),
                    CachedPrompt {
                        version: version.clone(),
                        cached_at_ms: now_ms,
                        entitlement_expires_at_ms,
                    },
                );
            }
            for chain in &snapshot.chains {
                chains.insert(chain.chain_id.to_string(), chain.clone());
            }
            self.prompts = prompts;
            self.versions = versions;
            self.chains = chains;
            self.cache = cache;
            self.snapshot_revision = snapshot.revision;
            self.accepted_snapshot = Some(snapshot);
            Ok(SyncOutcome::Applied)
        }
    }

    fn validate_prompt_snapshot(
        projection: &OrganizationPromptProjection,
        snapshot: &OrganizationPromptSnapshot,
    ) -> Result<(), OrgError> {
        let mut snapshot_versions = BTreeMap::new();
        for version in &snapshot.versions {
            if version.body.len() > ORGANIZATION_PROMPT_BODY_LIMIT_BYTES as usize
                || version.title.len() > ORGANIZATION_PROMPT_BODY_LIMIT_BYTES as usize
                || version.tags.len() > ORGANIZATION_PROMPT_PAGE_ITEM_LIMIT as usize
            {
                return Err(OrgError::BoundExceeded);
            }
            if version.content_hash_hex != hash_body(&version.body) {
                return Err(OrgError::ImmutableVersion);
            }
            if let Some(existing) = projection.versions.get(&version.version_id.to_string()) {
                if existing != version {
                    return Err(OrgError::ImmutableVersion);
                }
            }
            if snapshot_versions
                .insert(version.version_id, version)
                .is_some()
            {
                return Err(OrgError::DuplicateLink);
            }
        }
        for prompt in &snapshot.prompts {
            if prompt.tenant_id != snapshot.tenant_id {
                return Err(OrgError::CrossTenant);
            }
            let current = snapshot_versions
                .get(&prompt.current_version_id)
                .copied()
                .or_else(|| {
                    projection
                        .versions
                        .get(&prompt.current_version_id.to_string())
                });
            let Some(current) = current else {
                return Err(OrgError::Unlinked);
            };
            if current.prompt_id != prompt.prompt_id {
                return Err(OrgError::Unlinked);
            }
        }
        for chain in &snapshot.chains {
            if chain.tenant_id != snapshot.tenant_id {
                return Err(OrgError::CrossTenant);
            }
            if chain.links.len() > ORGANIZATION_PROMPT_PAGE_ITEM_LIMIT as usize {
                return Err(OrgError::BoundExceeded);
            }
            for (index, link) in chain.links.iter().enumerate() {
                if link.position != index as u32 {
                    return Err(OrgError::LastWriteWinsForbidden);
                }
                let known = snapshot_versions.contains_key(&link.version_id)
                    || projection
                        .versions
                        .contains_key(&link.version_id.to_string());
                if !known {
                    return Err(OrgError::Unlinked);
                }
            }
        }
        Ok(())
    }

    fn authorize_publish(
        membership: &HostMembership,
        policy: &OrganizationPolicyDocument,
    ) -> Result<(), OrgError> {
        if !membership.is_enrolled() {
            return Err(OrgError::HostUnenrolled);
        }
        if membership.role.is_disabled() {
            return Err(OrgError::DisabledMember);
        }
        if membership.role.can_administer()
            || policy.grants_prompt_maintainer(&membership.account_id)
        {
            return Ok(());
        }
        if membership.role == MembershipRole::Member || membership.role == MembershipRole::Manager {
            return Err(OrgError::RoleDenied);
        }
        Err(OrgError::RoleDenied)
    }

    fn prompt_key(tenant_id: &PortalTenantId, namespace: &str, name: &str) -> String {
        format!(
            "{}::{}::{}",
            tenant_id.as_str().to_ascii_lowercase(),
            namespace.to_ascii_lowercase(),
            name.to_ascii_lowercase()
        )
    }

    fn hash_body(body: &str) -> String {
        let digest = Sha256::digest(body.as_bytes());
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut out = String::with_capacity(digest.len() * 2);
        for byte in digest {
            out.push(HEX[(byte >> 4) as usize] as char);
            out.push(HEX[(byte & 0x0f) as usize] as char);
        }
        out
    }
}

pub use organization::*;
