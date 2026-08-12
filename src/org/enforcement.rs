//! Local organization grant checks. Hosted routing eligibility is not local
//! authority; this evaluator fails closed without a current matching grant.

use crate::connect::{
    ActionId, ConnectRole, PermissionDecision, PermissionEvaluator, PermissionRequest,
};
use crate::domain::id::TaskId;
use crate::domain::org::TaskScope;
use crate::org::error::OrgError;
use crate::org::identity::PortalTenantId;
use crate::org::managed::ManagedTaskLink;
use crate::org::membership::{HostMembership, MembershipRole, MembershipStatus};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrganizationGrant {
    pub tenant_id: PortalTenantId,
    pub task_id: TaskId,
    pub policy_revision: u32,
    pub expires_at_ms: i64,
    pub role: MembershipRole,
    pub collaborator: bool,
    pub raw_content: bool,
}

pub struct OrganizationEnforcer {
    evaluator: PermissionEvaluator,
}

impl OrganizationEnforcer {
    pub fn new() -> Self {
        Self {
            evaluator: PermissionEvaluator::owner_only(),
        }
    }

    pub fn with_collaborator_writes(enabled: bool) -> Self {
        Self {
            evaluator: PermissionEvaluator::new(enabled),
        }
    }

    pub fn authorize(
        &self,
        mode_is_standalone: bool,
        membership: Option<&HostMembership>,
        scope: &TaskScope,
        link: Option<&ManagedTaskLink>,
        grant: Option<&OrganizationGrant>,
        action: ActionId,
        now_ms: i64,
    ) -> Result<(), OrgError> {
        if mode_is_standalone {
            return Err(OrgError::StandaloneMode);
        }
        let membership = membership.ok_or(OrgError::HostUnenrolled)?;
        match membership.status {
            MembershipStatus::Revoked => return Err(OrgError::MembershipRevoked),
            MembershipStatus::Unenrolled | MembershipStatus::PendingLocalConfirm => {
                return Err(OrgError::HostUnenrolled);
            }
            MembershipStatus::Enrolled => {}
        }
        if scope.is_personal() {
            return Err(OrgError::PersonalTask);
        }
        let link = link.ok_or(OrgError::Unlinked)?;
        if link.tenant_id != membership.tenant_id {
            return Err(OrgError::CrossTenant);
        }
        let grant = grant.ok_or(OrgError::StaleGrant)?;
        if grant.tenant_id != membership.tenant_id || grant.tenant_id != link.tenant_id {
            return Err(OrgError::CrossTenant);
        }
        if grant.task_id != link.local_task_id {
            return Err(OrgError::StaleGrant);
        }
        if grant.policy_revision != link.metadata_policy_version {
            return Err(OrgError::StalePolicy);
        }
        if now_ms >= grant.expires_at_ms {
            return Err(OrgError::Expired);
        }
        if grant.raw_content {
            return Err(OrgError::ProhibitedField);
        }

        let role = if grant.collaborator {
            ConnectRole::Collaborator {
                task_id: grant.task_id,
            }
        } else {
            ConnectRole::Watcher {
                task_id: grant.task_id,
            }
        };
        match self.evaluator.evaluate(PermissionRequest {
            role,
            task_id: Some(grant.task_id),
            action,
            // Organization grants do not carry a device credential. Keep the
            // request explicit so the evaluator's current scoped-grant and
            // credential gates remain fail-closed.
            credential: None,
        }) {
            PermissionDecision::Allow => Ok(()),
            PermissionDecision::Denied(reason) => match reason {
                crate::connect::PermissionDenyReason::WatcherReadOnly => {
                    Err(OrgError::WatcherReadOnly)
                }
                crate::connect::PermissionDenyReason::OwnerOnly => Err(OrgError::OwnerOnly),
                _ => Err(OrgError::RoleDenied),
            },
        }
    }
}

impl Default for OrganizationEnforcer {
    fn default() -> Self {
        Self::new()
    }
}
