//! Typed local-action contract for DB Flow and ENV. The local registry, not
//! the remote request, assigns replay policy. Credentials never leave the host.

use serde::{Deserialize, Serialize};

use crate::domain::id::{OperationId, ProjectId};
use crate::org::error::OrgError;
use crate::org::ids::LocalActionId;
use crate::org::membership::{HostMembership, LocalActionApprovalRequirement};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalActionKind {
    DbSchemaIntrospect,
    DbApprovedChangeApply,
    EnvDiff,
    EnvApprovedApply,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplayPolicy {
    IdempotentSafe,
    NeverAssumeRetrySafe,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionRisk {
    Low,
    Production,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Admission {
    Accepted,
    Rejected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionOutcome {
    Settled,
    Failed,
    Cancelled,
    Uncertain,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalActionRequest {
    pub request_id: LocalActionId,
    pub tenant_id: String,
    pub host_id: String,
    pub project_id: ProjectId,
    pub kind: LocalActionKind,
    pub version: u16,
    pub payload: String,
    pub risk: ActionRisk,
    pub required_approvals: u8,
    pub expected_target_fingerprint: String,
    pub expiry_ms: i64,
    pub signature_hex: String,
    pub remote_replay_policy_override: Option<ReplayPolicy>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalActionReceipt {
    pub request_id: LocalActionId,
    pub operation_id: OperationId,
    pub admission: Admission,
    pub outcome: ActionOutcome,
    pub local_actor: String,
    pub started_at_ms: i64,
    pub ended_at_ms: i64,
    pub target_fingerprint: String,
    pub redacted_result: String,
    pub artifact_refs: Vec<String>,
    pub error_class: Option<String>,
}

#[derive(Debug, Default)]
pub struct LocalActionRegistry {
    seen: std::collections::BTreeSet<String>,
}

impl LocalActionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn replay_policy(kind: LocalActionKind, version: u16) -> Result<ReplayPolicy, OrgError> {
        if version == 0 {
            return Err(OrgError::StalePolicy);
        }
        Ok(match kind {
            LocalActionKind::DbSchemaIntrospect | LocalActionKind::EnvDiff => {
                ReplayPolicy::IdempotentSafe
            }
            LocalActionKind::DbApprovedChangeApply | LocalActionKind::EnvApprovedApply => {
                ReplayPolicy::NeverAssumeRetrySafe
            }
        })
    }

    pub fn admit(
        &mut self,
        membership: &HostMembership,
        request: &LocalActionRequest,
        local_host_id: &str,
        local_project: ProjectId,
        local_fingerprint: &str,
        owner_approved: bool,
        now_ms: i64,
    ) -> Result<ReplayPolicy, OrgError> {
        if !membership.is_enrolled() {
            return Err(OrgError::HostUnenrolled);
        }
        if request.tenant_id != membership.tenant_id.as_str() {
            return Err(OrgError::CrossTenant);
        }
        if request.host_id != local_host_id {
            return Err(OrgError::HostUnenrolled);
        }
        if request.project_id != local_project {
            return Err(OrgError::CrossTenant);
        }
        if now_ms >= request.expiry_ms {
            return Err(OrgError::Expired);
        }
        if !self.seen.insert(request.request_id.to_string()) {
            return Err(OrgError::Replay);
        }
        if request.required_approvals > 0
            && !owner_approved
            && membership.role != crate::org::membership::MembershipRole::Owner
        {
            return Err(OrgError::MissingApproval);
        }
        if matches!(
            membership.local_action_requirement(),
            LocalActionApprovalRequirement::OwnerRequired
        ) && !owner_approved
        {
            return Err(OrgError::MissingApproval);
        }
        if request.expected_target_fingerprint != local_fingerprint {
            return Err(OrgError::FingerprintMismatch);
        }
        if request.remote_replay_policy_override.is_some() {
            return Err(OrgError::LastWriteWinsForbidden);
        }
        if contains_secret(&request.payload) {
            return Err(OrgError::ProhibitedField);
        }
        let policy = Self::replay_policy(request.kind, request.version)?;
        if request.risk == ActionRisk::Production && policy != ReplayPolicy::NeverAssumeRetrySafe {
            return Err(OrgError::ProductionRiskNotRetrySafe);
        }
        Ok(policy)
    }

    pub fn settle_ambiguous(&self) -> OrgError {
        OrgError::UncertainOutcome
    }

    pub fn redact(result: &str) -> String {
        if contains_secret(result) {
            "[redacted]".to_string()
        } else {
            result.to_string()
        }
    }
}

impl HostMembership {
    fn local_action_requirement(&self) -> LocalActionApprovalRequirement {
        LocalActionApprovalRequirement::OwnerRequired
    }
}

fn contains_secret(value: &str) -> bool {
    let lowered = value.to_ascii_lowercase();
    lowered.contains("password")
        || lowered.contains("secret")
        || lowered.contains("connection string")
        || lowered.contains("api_key")
        || lowered.contains("token=")
}
