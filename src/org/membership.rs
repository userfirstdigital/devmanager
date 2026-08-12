//! Host enrollment against an external Portal tenant/account. Local
//! confirmation is required; offline unenroll remains available.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::connect::{ConnectHostId, ManagedField, ACTIVE_SESSION_IDLE_LIMIT_MS};
use crate::org::error::OrgError;
use crate::org::identity::{ExternalAccount, PortalAccountId, PortalDeviceId, PortalTenantId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MembershipRole {
    Owner,
    Admin,
    Manager,
    Member,
    Disabled,
}

impl MembershipRole {
    pub const fn can_administer(self) -> bool {
        matches!(self, Self::Owner | Self::Admin)
    }

    pub const fn can_watch(self) -> bool {
        matches!(self, Self::Owner | Self::Admin | Self::Manager)
    }

    pub const fn can_read_published(self) -> bool {
        !matches!(self, Self::Disabled)
    }

    pub const fn is_disabled(self) -> bool {
        matches!(self, Self::Disabled)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MembershipStatus {
    PendingLocalConfirm,
    Enrolled,
    Revoked,
    Unenrolled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RawSharingCeiling {
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalActionApprovalRequirement {
    OwnerRequired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrganizationPolicyDocument {
    pub revision: u32,
    pub tenant_id: PortalTenantId,
    pub allowed_metadata_fields: BTreeSet<ManagedMetadataName>,
    pub retention_ms: u64,
    pub idle_interval_ms: u64,
    pub raw_sharing_ceiling: RawSharingCeiling,
    pub local_action_approval: LocalActionApprovalRequirement,
    pub prompt_maintainer_accounts: BTreeSet<String>,
    pub content_hash_hex: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagedMetadataName {
    TaskState,
    TaskAttention,
    TaskAssignmentReference,
    ProviderKind,
    ProviderState,
    SourceTimestamp,
    ObservedTimestamp,
    ProviderReportedUsage,
    HumanMessageCount,
    HumanTurnCount,
    ActiveSessionInterval,
    GitSummary,
    HostHealth,
    ApprovedArtifactReference,
}

impl ManagedMetadataName {
    pub const MINIMAL: &'static [Self] = &[
        Self::TaskState,
        Self::TaskAttention,
        Self::SourceTimestamp,
        Self::ObservedTimestamp,
    ];

    pub const fn as_managed_field(self) -> Option<ManagedField> {
        Some(match self {
            Self::TaskState => ManagedField::TaskState,
            Self::TaskAttention => ManagedField::TaskAttention,
            Self::TaskAssignmentReference => ManagedField::TaskAssignmentReference,
            Self::ProviderKind => ManagedField::ProviderKind,
            Self::ProviderState => ManagedField::ProviderState,
            Self::SourceTimestamp => ManagedField::SourceTimestamp,
            Self::ObservedTimestamp => ManagedField::ObservedTimestamp,
            Self::ProviderReportedUsage => ManagedField::ProviderReportedUsage,
            Self::HumanMessageCount => ManagedField::HumanMessageCount,
            Self::HumanTurnCount => ManagedField::HumanTurnCount,
            Self::ActiveSessionInterval => ManagedField::ActiveSessionInterval,
            Self::GitSummary => ManagedField::GitSummary,
            Self::HostHealth => ManagedField::HostHealth,
            Self::ApprovedArtifactReference => ManagedField::ApprovedArtifactReference,
        })
    }
}

impl OrganizationPolicyDocument {
    pub fn deny_minimal(tenant_id: PortalTenantId) -> Result<Self, OrgError> {
        let mut allowed_metadata_fields = BTreeSet::new();
        for field in ManagedMetadataName::MINIMAL {
            allowed_metadata_fields.insert(*field);
        }
        Self::finalize(Self {
            revision: 1,
            tenant_id,
            allowed_metadata_fields,
            retention_ms: 24 * 60 * 60 * 1_000,
            idle_interval_ms: ACTIVE_SESSION_IDLE_LIMIT_MS,
            raw_sharing_ceiling: RawSharingCeiling::None,
            local_action_approval: LocalActionApprovalRequirement::OwnerRequired,
            prompt_maintainer_accounts: BTreeSet::new(),
            content_hash_hex: String::new(),
        })
    }

    pub fn finalize(mut self) -> Result<Self, OrgError> {
        if self.revision == 0 {
            return Err(OrgError::StalePolicy);
        }
        if self.idle_interval_ms != ACTIVE_SESSION_IDLE_LIMIT_MS {
            return Err(OrgError::ProhibitedField);
        }
        if self.raw_sharing_ceiling != RawSharingCeiling::None {
            return Err(OrgError::ProhibitedField);
        }
        for field in &self.allowed_metadata_fields {
            let Some(managed) = field.as_managed_field() else {
                return Err(OrgError::ProhibitedField);
            };
            if managed.is_explicitly_denied() || managed.is_unknown() {
                return Err(OrgError::ProhibitedField);
            }
        }
        self.content_hash_hex = self.compute_hash_hex();
        Ok(self)
    }

    pub fn grants_prompt_maintainer(&self, account_id: &PortalAccountId) -> bool {
        self.prompt_maintainer_accounts
            .iter()
            .any(|entry| entry == account_id.as_str())
    }

    fn compute_hash_hex(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.revision.to_le_bytes());
        hasher.update(self.tenant_id.as_str().as_bytes());
        hasher.update(self.retention_ms.to_le_bytes());
        hasher.update(self.idle_interval_ms.to_le_bytes());
        for field in &self.allowed_metadata_fields {
            hasher.update(format!("{field:?}").as_bytes());
        }
        hex_encode(&hasher.finalize())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnrollmentPreview {
    pub tenant_id: PortalTenantId,
    pub account_id: PortalAccountId,
    pub metadata_classes: Vec<ManagedMetadataName>,
    pub retention_ms: u64,
    pub idle_interval_ms: u64,
    pub manager_visibility: &'static str,
    pub raw_content: &'static str,
}

impl EnrollmentPreview {
    pub fn from_policy(account: &ExternalAccount, policy: &OrganizationPolicyDocument) -> Self {
        Self {
            tenant_id: account.tenant_id.clone(),
            account_id: account.account_id.clone(),
            metadata_classes: policy.allowed_metadata_fields.iter().copied().collect(),
            retention_ms: policy.retention_ms,
            idle_interval_ms: policy.idle_interval_ms,
            manager_visibility: "read-only Watcher of enrolled metadata only",
            raw_content: "off",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostMembership {
    pub host_id: ConnectHostId,
    pub tenant_id: PortalTenantId,
    pub account_id: PortalAccountId,
    pub device_id: Option<PortalDeviceId>,
    pub role: MembershipRole,
    pub status: MembershipStatus,
    pub enrolled_at_ms: Option<i64>,
    pub last_seen_at_ms: Option<i64>,
    pub revoked_at_ms: Option<i64>,
    pub display_label: String,
    pub policy_revision: u32,
    pub policy_hash_hex: String,
}

impl HostMembership {
    pub fn pending(
        host_id: ConnectHostId,
        account: ExternalAccount,
        role: MembershipRole,
        policy: &OrganizationPolicyDocument,
        display_label: impl Into<String>,
    ) -> Result<Self, OrgError> {
        if role.is_disabled() {
            return Err(OrgError::DisabledMember);
        }
        if account.tenant_id != policy.tenant_id {
            return Err(OrgError::CrossTenant);
        }
        Ok(Self {
            host_id,
            tenant_id: account.tenant_id,
            account_id: account.account_id,
            device_id: account.device_id,
            role,
            status: MembershipStatus::PendingLocalConfirm,
            enrolled_at_ms: None,
            last_seen_at_ms: None,
            revoked_at_ms: None,
            display_label: display_label.into(),
            policy_revision: policy.revision,
            policy_hash_hex: policy.content_hash_hex.clone(),
        })
    }

    pub fn confirm_locally(
        &mut self,
        now_ms: i64,
        policy: &OrganizationPolicyDocument,
    ) -> Result<(), OrgError> {
        if self.status == MembershipStatus::Revoked {
            return Err(OrgError::MembershipRevoked);
        }
        if self.policy_revision != policy.revision
            || self.policy_hash_hex != policy.content_hash_hex
        {
            return Err(OrgError::StalePolicy);
        }
        self.status = MembershipStatus::Enrolled;
        self.enrolled_at_ms = Some(now_ms);
        self.last_seen_at_ms = Some(now_ms);
        Ok(())
    }

    pub fn unenroll_offline(&mut self, now_ms: i64) {
        self.status = MembershipStatus::Unenrolled;
        self.revoked_at_ms = Some(now_ms);
    }

    pub fn revoke(&mut self, now_ms: i64) {
        self.status = MembershipStatus::Revoked;
        self.revoked_at_ms = Some(now_ms);
    }

    pub fn is_enrolled(&self) -> bool {
        self.status == MembershipStatus::Enrolled
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}
