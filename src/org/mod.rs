//! DevManager-side organization contract and local projections.
//!
//! Anonymous/local standalone mode is the default. Connect sign-in does not
//! enroll the host or any Task. Portal remains the issuer of tenants, users,
//! Boards, and BoardCards.

mod boundary;
mod enforcement;
mod error;
mod evidence;
mod identity;
mod ids;
mod local_actions;
mod managed;
mod membership;
mod watcher;
mod workflow;

pub use boundary::{
    ProductBoundary, ANONYMOUS_STANDALONE_REMAINS_DEFAULT, LOCAL_OPEN_SOURCE_SURFACE,
    NO_PROPRIETARY_KEY_REQUIRED_FOR_LOCAL, PROPRIETARY_CONNECT_SURFACE, SHARED_OPEN_WIRE_SCHEMAS,
};
pub use enforcement::{OrganizationEnforcer, OrganizationGrant};
pub use error::{OrgDependency, OrgError};
pub use evidence::{
    compute_bundle_hash, EvidenceAccessClass, EvidenceBundle, EvidenceIntake, EvidenceMediaRef,
    EvidenceMetadataProjection, EvidenceSegment, TaskDraft, EVIDENCE_BUNDLE_VERSION,
};
pub use identity::{
    BoardCardId, BoardId, ExternalAccount, IdentityError, PortalAccountId, PortalDeviceId,
    PortalTenantId,
};
pub use ids::{
    EvidenceBundleId, HandoffId, LocalActionId, ManagedLinkId, OrgPromptChainId, OrgPromptId,
    OrgPromptVersionId, TaskDraftId,
};
pub use local_actions::{
    ActionOutcome, ActionRisk, Admission, LocalActionAdmissionState, LocalActionCatalogEntry,
    LocalActionKind, LocalActionReceipt, LocalActionReconcileState, LocalActionRegistry,
    LocalActionRequest, ReplayPolicy,
};
pub use managed::{
    DualField, EnrollmentState, FieldAuthority, ManagedTaskLink, ManagedTaskSnapshot, SyncOutcome,
    TaskLinkReducer, TitleConflict, MAX_MANAGED_LINKS,
};
pub use membership::{
    EnrollmentPreview, HostMembership, LocalActionApprovalRequirement, ManagedMetadataName,
    MembershipRole, MembershipStatus, OrganizationPolicyDocument, RawSharingCeiling,
};
pub use watcher::{
    reject_forbidden_fields, reject_forbidden_label, FleetWatcherView, HostReachability,
    TaskWatcherView, WatcherProjection, ACTIVE_SESSION_RULE, FORBIDDEN_WATCHER_LABELS,
};
pub use workflow::{
    AssignmentAcceptance, BoardWorkflowEvent, ManagedWorkflowProjection, ReviewState,
};

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::connect::ConnectHostId;
use crate::domain::id::{ProjectId, TaskId};
use crate::prompts::{OrganizationPromptProjection, OrganizationPromptSnapshot};
use crate::protocol::Capability;

const MAX_SEEN_FACTS: usize = 1_024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OperatingMode {
    AnonymousLocal,
    ConnectSignedIn { account: ExternalAccount },
    HostEnrolled { membership: HostMembership },
}

impl OperatingMode {
    pub const fn anonymous() -> Self {
        Self::AnonymousLocal
    }

    pub const fn is_standalone(&self) -> bool {
        matches!(self, Self::AnonymousLocal)
    }

    pub const fn organization_capability(&self) -> Option<Capability> {
        match self {
            Self::AnonymousLocal => None,
            Self::ConnectSignedIn { .. } | Self::HostEnrolled { .. } => {
                Some(Capability::OrganizationProjection)
            }
        }
    }

    pub fn sign_in(account: ExternalAccount) -> Self {
        Self::ConnectSignedIn { account }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrganizationSyncState {
    #[default]
    Standalone,
    SignedIn,
    Enrolled,
    Unlinked,
    Revoked,
    Expired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum OrganizationFact {
    Membership {
        host_id: ConnectHostId,
        tenant_id: PortalTenantId,
        account_id: PortalAccountId,
        status: MembershipStatus,
        revision: u64,
        revoked_at_ms: Option<i64>,
        expires_at_ms: Option<i64>,
    },
    ManagedTask(ManagedTaskSnapshot),
}

#[derive(Debug, Default)]
pub struct OrganizationProjection {
    mode: OperatingMode,
    policy: Option<OrganizationPolicyDocument>,
    links: TaskLinkReducer,
    prompts: OrganizationPromptProjection,
    local_actions: LocalActionRegistry,
    evidence: EvidenceIntake,
    sync_state: OrganizationSyncState,
    seen_facts: BTreeMap<String, [u8; 32]>,
    membership_revision: Option<u64>,
    membership_fact_hash: Option<[u8; 32]>,
}

impl Default for OperatingMode {
    fn default() -> Self {
        Self::AnonymousLocal
    }
}

impl OrganizationProjection {
    pub fn standalone() -> Self {
        Self {
            mode: OperatingMode::anonymous(),
            policy: None,
            links: TaskLinkReducer::new(),
            prompts: OrganizationPromptProjection::new(),
            local_actions: LocalActionRegistry::new(),
            evidence: EvidenceIntake::new([]),
            sync_state: OrganizationSyncState::Standalone,
            seen_facts: BTreeMap::new(),
            membership_revision: None,
            membership_fact_hash: None,
        }
    }

    pub fn mode(&self) -> &OperatingMode {
        &self.mode
    }

    pub fn sync_state(&self) -> OrganizationSyncState {
        self.sync_state
    }

    pub fn sign_in(&mut self, account: ExternalAccount) -> usize {
        if matches!(
            self.mode,
            OperatingMode::HostEnrolled { ref membership } if membership.is_enrolled()
        ) {
            return self.exported_task_count();
        }
        self.mode = OperatingMode::sign_in(account);
        if !matches!(
            self.sync_state,
            OrganizationSyncState::Revoked | OrganizationSyncState::Expired
        ) {
            self.sync_state = OrganizationSyncState::SignedIn;
        }
        self.exported_task_count()
    }

    pub fn enroll_without_local_confirmation(&self) -> Result<(), OrgError> {
        Err(if self.mode.is_standalone() {
            OrgError::StandaloneMode
        } else {
            OrgError::ConnectSignInDoesNotEnroll
        })
    }

    pub fn preview_enrollment(
        &self,
        account: &ExternalAccount,
        policy: &OrganizationPolicyDocument,
    ) -> Result<EnrollmentPreview, OrgError> {
        if self.mode.is_standalone() {
            return Err(OrgError::StandaloneMode);
        }
        Ok(EnrollmentPreview::from_policy(account, policy))
    }

    pub fn confirm_enrollment(
        &mut self,
        mut membership: HostMembership,
        policy: OrganizationPolicyDocument,
        now_ms: i64,
    ) -> Result<HostMembership, OrgError> {
        if self.mode.is_standalone() {
            return Err(OrgError::StandaloneMode);
        }
        membership.confirm_locally(now_ms, &policy)?;
        self.policy = Some(policy);
        self.mode = OperatingMode::HostEnrolled {
            membership: membership.clone(),
        };
        self.membership_revision = None;
        self.membership_fact_hash = None;
        self.sync_state = OrganizationSyncState::Enrolled;
        self.evidence.bind_tenant(membership.tenant_id.clone());
        Ok(membership)
    }

    pub fn cancel_enrollment(&self) -> Result<(), OrgError> {
        if !matches!(self.mode, OperatingMode::HostEnrolled { .. }) {
            return Ok(());
        }
        Err(OrgError::HostUnenrolled)
    }

    pub fn unenroll_offline(&mut self, now_ms: i64) -> Result<(), OrgError> {
        match &mut self.mode {
            OperatingMode::HostEnrolled { membership } => {
                membership.unenroll_offline(now_ms);
                self.policy = None;
                self.sync_state = OrganizationSyncState::Unlinked;
                self.links = TaskLinkReducer::new();
                self.membership_revision = None;
                self.membership_fact_hash = None;
                self.prompts.purge();
                self.evidence = EvidenceIntake::new([]);
                Ok(())
            }
            OperatingMode::AnonymousLocal => Err(OrgError::StandaloneMode),
            OperatingMode::ConnectSignedIn { .. } => Err(OrgError::HostUnenrolled),
        }
    }

    pub fn membership(&self) -> Option<&HostMembership> {
        match &self.mode {
            OperatingMode::HostEnrolled { membership } => Some(membership),
            _ => None,
        }
    }

    pub fn links(&mut self) -> Result<&mut TaskLinkReducer, OrgError> {
        self.require_enrolled()?;
        Ok(&mut self.links)
    }

    pub fn exported_task_count(&self) -> usize {
        if self.sync_state != OrganizationSyncState::Enrolled || self.membership().is_none() {
            return 0;
        }
        self.links.enrolled_count()
    }

    pub fn host_id_if_enrolled(&self) -> Option<ConnectHostId> {
        (self.sync_state == OrganizationSyncState::Enrolled)
            .then(|| self.membership())
            .flatten()
            .filter(|membership| membership.is_enrolled())
            .map(|membership| membership.host_id)
    }

    pub fn prompts(&self) -> Result<&OrganizationPromptProjection, OrgError> {
        self.require_enrolled()?;
        Ok(&self.prompts)
    }

    pub fn prompts_mut(&mut self) -> Result<&mut OrganizationPromptProjection, OrgError> {
        self.require_enrolled()?;
        Ok(&mut self.prompts)
    }

    pub fn apply_authoritative_fact(
        &mut self,
        fact: OrganizationFact,
        now_ms: i64,
    ) -> Result<SyncOutcome, OrgError> {
        if self.mode.is_standalone() {
            return Err(OrgError::StandaloneMode);
        }
        match fact {
            OrganizationFact::Membership {
                host_id,
                tenant_id,
                account_id,
                status,
                revision,
                revoked_at_ms,
                expires_at_ms,
            } => self.reconcile_membership_fact(
                host_id,
                tenant_id,
                account_id,
                status,
                revision,
                revoked_at_ms,
                expires_at_ms,
                now_ms,
            ),
            OrganizationFact::ManagedTask(snapshot) => {
                let (key, hash) = managed_fact_identity(&snapshot);
                if let Some(existing) = self.seen_facts.get(&key) {
                    return if *existing == hash {
                        Ok(SyncOutcome::Duplicate)
                    } else {
                        Err(OrgError::LastWriteWinsForbidden)
                    };
                }
                if self.seen_facts.len() >= MAX_SEEN_FACTS {
                    return Err(OrgError::BoundExceeded);
                }
                let membership = self.membership().cloned().ok_or(OrgError::HostUnenrolled)?;
                let outcome = self.links.apply_portal_snapshot(&membership, snapshot)?;
                self.seen_facts.insert(key, hash);
                Ok(outcome)
            }
        }
    }

    pub fn apply_prompt_snapshot(
        &mut self,
        snapshot: OrganizationPromptSnapshot,
        now_ms: i64,
        entitlement_expires_at_ms: i64,
    ) -> Result<SyncOutcome, OrgError> {
        let membership = self.require_enrolled()?.clone();
        self.prompts.apply_authoritative_snapshot(
            &membership,
            snapshot,
            now_ms,
            entitlement_expires_at_ms,
        )
    }

    pub fn personal_scope_without_link(&self, task_id: TaskId) -> crate::domain::org::TaskScope {
        match (self.sync_state == OrganizationSyncState::Enrolled)
            .then(|| self.host_id_if_enrolled())
            .flatten()
        {
            Some(host_id) => self.links.scope_for(host_id, task_id),
            None => crate::domain::org::TaskScope::personal(),
        }
    }

    pub fn admit_local_action(
        &mut self,
        request: &LocalActionRequest,
        local_host_id: &str,
        local_project: ProjectId,
        local_fingerprint: &str,
        owner_approved: bool,
        now_ms: i64,
    ) -> Result<LocalActionAdmissionState, OrgError> {
        let membership = self.require_enrolled()?.clone();
        self.local_actions.admit_with_state(
            &membership,
            request,
            local_host_id,
            local_project,
            local_fingerprint,
            owner_approved,
            now_ms,
        )
    }

    pub fn bind_local_action_catalog(
        &mut self,
        entries: Vec<LocalActionCatalogEntry>,
    ) -> Result<(), OrgError> {
        self.require_enrolled()?;
        self.local_actions.bind_server_catalog(entries)
    }

    pub fn local_action_state(
        &self,
        request_id: LocalActionId,
    ) -> Result<Option<&LocalActionAdmissionState>, OrgError> {
        self.require_enrolled()?;
        Ok(self.local_actions.admission_state(request_id))
    }

    pub fn mark_local_action_uncertain(
        &mut self,
        request_id: LocalActionId,
    ) -> Result<LocalActionAdmissionState, OrgError> {
        self.require_enrolled()?;
        self.local_actions.mark_uncertain(request_id)
    }

    pub fn ingest_evidence(
        &mut self,
        bundle: &EvidenceBundle,
    ) -> Result<EvidenceMetadataProjection, OrgError> {
        let membership = self.require_enrolled()?.clone();
        self.evidence
            .ingest_for_tenant(Some(&membership.tenant_id), bundle)
    }

    pub fn trust_evidence_signer(&mut self, signer: impl Into<String>) -> Result<(), OrgError> {
        self.require_enrolled()?;
        self.evidence.trust_signer(signer)
    }

    pub fn authorize_evidence_e2e_raw(&mut self, authorized: bool) -> Result<(), OrgError> {
        self.require_enrolled()?;
        self.evidence.authorize_e2e_raw(authorized);
        Ok(())
    }

    pub fn evidence_raw_segments<'a>(
        &self,
        access: EvidenceAccessClass,
        bundle: &'a EvidenceBundle,
    ) -> Result<&'a [EvidenceSegment], OrgError> {
        self.require_enrolled()?;
        self.evidence.raw_evidence(access, bundle)
    }

    fn require_enrolled(&self) -> Result<&HostMembership, OrgError> {
        if self.mode.is_standalone() {
            return Err(OrgError::StandaloneMode);
        }
        match self.sync_state {
            OrganizationSyncState::Revoked => return Err(OrgError::MembershipRevoked),
            OrganizationSyncState::Expired => return Err(OrgError::Expired),
            OrganizationSyncState::Unlinked => return Err(OrgError::HostUnenrolled),
            OrganizationSyncState::Standalone => return Err(OrgError::StandaloneMode),
            OrganizationSyncState::SignedIn | OrganizationSyncState::Enrolled => {}
        }
        self.membership().ok_or(OrgError::HostUnenrolled)
    }

    fn reconcile_membership_fact(
        &mut self,
        host_id: ConnectHostId,
        tenant_id: PortalTenantId,
        account_id: PortalAccountId,
        status: MembershipStatus,
        revision: u64,
        revoked_at_ms: Option<i64>,
        expires_at_ms: Option<i64>,
        now_ms: i64,
    ) -> Result<SyncOutcome, OrgError> {
        if revision == 0 {
            return Err(OrgError::StalePolicy);
        }
        let hash = membership_fact_hash(
            host_id,
            &tenant_id,
            &account_id,
            status,
            revision,
            revoked_at_ms,
            expires_at_ms,
        );
        if let Some(current) = self.membership_revision {
            if revision < current {
                return Err(OrgError::StalePolicy);
            }
            if revision == current {
                return if self.membership_fact_hash == Some(hash) {
                    Ok(SyncOutcome::Duplicate)
                } else {
                    Err(OrgError::LastWriteWinsForbidden)
                };
            }
        }
        let outcome = self.apply_membership_fact(
            host_id,
            tenant_id,
            account_id,
            status,
            revoked_at_ms,
            expires_at_ms,
            now_ms,
        )?;
        self.membership_revision = Some(revision);
        self.membership_fact_hash = Some(hash);
        Ok(outcome)
    }

    fn apply_membership_fact(
        &mut self,
        host_id: ConnectHostId,
        tenant_id: PortalTenantId,
        account_id: PortalAccountId,
        status: MembershipStatus,
        revoked_at_ms: Option<i64>,
        expires_at_ms: Option<i64>,
        now_ms: i64,
    ) -> Result<SyncOutcome, OrgError> {
        if let Some(account) = self.signed_in_account() {
            if account.tenant_id != tenant_id || account.account_id != account_id {
                return Err(OrgError::CrossTenant);
            }
        }
        if let Some(membership) = self.membership() {
            if membership.host_id != host_id || membership.tenant_id != tenant_id {
                return Err(OrgError::CrossTenant);
            }
        }
        if let Some(expires_at_ms) = expires_at_ms {
            if now_ms >= expires_at_ms {
                self.sync_state = OrganizationSyncState::Expired;
                self.prompts.purge();
                return Ok(SyncOutcome::Applied);
            }
        }
        match status {
            MembershipStatus::Revoked => {
                if let OperatingMode::HostEnrolled { membership } = &mut self.mode {
                    membership.revoke(revoked_at_ms.unwrap_or(now_ms));
                }
                self.sync_state = OrganizationSyncState::Revoked;
                self.policy = None;
                self.prompts.purge();
                Ok(SyncOutcome::Applied)
            }
            MembershipStatus::Unenrolled => {
                if matches!(self.mode, OperatingMode::HostEnrolled { .. }) {
                    self.unenroll_offline(now_ms)?;
                } else {
                    self.sync_state = OrganizationSyncState::Unlinked;
                }
                Ok(SyncOutcome::Applied)
            }
            MembershipStatus::PendingLocalConfirm | MembershipStatus::Enrolled => {
                if !matches!(self.mode, OperatingMode::HostEnrolled { .. }) {
                    self.sync_state = OrganizationSyncState::SignedIn;
                }
                Ok(SyncOutcome::Applied)
            }
        }
    }

    fn signed_in_account(&self) -> Option<&ExternalAccount> {
        match &self.mode {
            OperatingMode::ConnectSignedIn { account } => Some(account),
            _ => None,
        }
    }
}

fn membership_fact_hash(
    host_id: ConnectHostId,
    tenant_id: &PortalTenantId,
    account_id: &PortalAccountId,
    status: MembershipStatus,
    revision: u64,
    revoked_at_ms: Option<i64>,
    expires_at_ms: Option<i64>,
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"membership");
    hasher.update(host_id.as_bytes());
    hash_len_prefixed(&mut hasher, tenant_id.as_str().as_bytes());
    hash_len_prefixed(&mut hasher, account_id.as_str().as_bytes());
    hasher.update([match status {
        MembershipStatus::PendingLocalConfirm => 1,
        MembershipStatus::Enrolled => 2,
        MembershipStatus::Revoked => 3,
        MembershipStatus::Unenrolled => 4,
    }]);
    hasher.update(revision.to_le_bytes());
    hasher.update(revoked_at_ms.unwrap_or(0).to_le_bytes());
    hasher.update(expires_at_ms.unwrap_or(0).to_le_bytes());
    hasher.finalize().into()
}

fn managed_fact_identity(snapshot: &ManagedTaskSnapshot) -> (String, [u8; 32]) {
    let mut hasher = Sha256::new();
    hasher.update(b"managed_task");
    hasher.update(snapshot.host_id.as_bytes());
    hasher.update(snapshot.local_task_id.as_bytes());
    hash_len_prefixed(&mut hasher, snapshot.board_card_id.as_str().as_bytes());
    hasher.update([match snapshot.enrollment_state {
        EnrollmentState::PendingOwnerAccept => 1,
        EnrollmentState::Enrolled => 2,
        EnrollmentState::Unlinked => 3,
        EnrollmentState::Closed => 4,
    }]);
    hasher.update(snapshot.portal_revision.to_le_bytes());
    hasher.update(snapshot.metadata_policy_version.to_le_bytes());
    hash_len_prefixed(&mut hasher, snapshot.linked_by.as_bytes());
    hasher.update(snapshot.linked_at.to_le_bytes());
    hash_len_prefixed(&mut hasher, snapshot.link_id.to_string().as_bytes());
    hash_len_prefixed(&mut hasher, snapshot.tenant_id.as_str().as_bytes());
    (
        format!(
            "managed:{}:{}:{}",
            snapshot.host_id, snapshot.local_task_id, snapshot.portal_revision
        ),
        hasher.finalize().into(),
    )
}

fn hash_len_prefixed(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

/// Marker used by Connect adapters when no organization overlay is present.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StandaloneOrganization;
