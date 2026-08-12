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
    compute_bundle_hash, EvidenceBundle, EvidenceIntake, EvidenceMediaRef, EvidenceSegment,
    TaskDraft, EVIDENCE_BUNDLE_VERSION,
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
    ActionOutcome, ActionRisk, Admission, LocalActionKind, LocalActionReceipt, LocalActionRegistry,
    LocalActionRequest, ReplayPolicy,
};
pub use managed::{
    DualField, EnrollmentState, FieldAuthority, ManagedTaskLink, TaskLinkReducer, TitleConflict,
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

use crate::connect::ConnectHostId;
use crate::protocol::Capability;

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

#[derive(Debug, Default)]
pub struct OrganizationProjection {
    mode: OperatingMode,
    policy: Option<OrganizationPolicyDocument>,
    links: TaskLinkReducer,
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
        }
    }

    pub fn mode(&self) -> &OperatingMode {
        &self.mode
    }

    pub fn sign_in(&mut self, account: ExternalAccount) -> usize {
        if !matches!(self.mode, OperatingMode::HostEnrolled { .. }) {
            self.mode = OperatingMode::sign_in(account);
        }
        0
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
        if self.membership().is_none() {
            return Err(if self.mode.is_standalone() {
                OrgError::StandaloneMode
            } else {
                OrgError::HostUnenrolled
            });
        }
        Ok(&mut self.links)
    }

    pub fn exported_task_count(&self) -> usize {
        0
    }

    pub fn host_id_if_enrolled(&self) -> Option<ConnectHostId> {
        self.membership().map(|membership| membership.host_id)
    }
}

/// Marker used by Connect adapters when no organization overlay is present.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StandaloneOrganization;
