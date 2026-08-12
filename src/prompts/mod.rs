//! Published organization prompt libraries and linear guided chains.
//!
//! Connect is authoritative for published content. Selection copies one exact
//! immutable version into the local composer and never sends or advances.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::org::error::OrgError;
use crate::org::identity::{PortalAccountId, PortalTenantId};
use crate::org::ids::{OrgPromptChainId, OrgPromptId, OrgPromptVersionId};
use crate::org::membership::{HostMembership, MembershipRole, OrganizationPolicyDocument};
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

#[derive(Debug, Default)]
pub struct OrganizationPromptProjection {
    prompts: BTreeMap<String, OrgPrompt>,
    versions: BTreeMap<String, OrgPromptVersion>,
    chains: BTreeMap<String, OrgPromptChain>,
    cache: BTreeMap<String, CachedPrompt>,
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
    if membership.role.can_administer() || policy.grants_prompt_maintainer(&membership.account_id) {
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
