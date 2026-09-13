//! Typed local-action contract for DB Flow and ENV. The local registry, not
//! the remote request, assigns replay policy. Credentials never leave the host.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::domain::id::{OperationId, ProjectId};
use crate::org::error::OrgError;
use crate::org::ids::LocalActionId;
use crate::org::managed::SyncOutcome;
use crate::org::membership::{HostMembership, LocalActionApprovalRequirement};

pub const MAX_LOCAL_ACTION_PAYLOAD_BYTES: usize = 16_384;
pub const MAX_LOCAL_ACTION_SEEN: usize = 128;
pub const MAX_LOCAL_ACTION_CATALOG: usize = 32;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalActionReconcileState {
    AwaitingHostExecution,
    Rejected,
    Uncertain,
    Settled,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalActionCatalogEntry {
    pub kind: LocalActionKind,
    pub version: u16,
    pub replay_policy: ReplayPolicy,
    pub risk: ActionRisk,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalActionAdmissionState {
    pub request_id: LocalActionId,
    pub admission: Admission,
    pub replay_policy: Option<ReplayPolicy>,
    pub outcome: Option<ActionOutcome>,
    pub reconcile: LocalActionReconcileState,
}

#[derive(Debug, Default)]
pub struct LocalActionRegistry {
    seen: std::collections::BTreeSet<String>,
    catalog: BTreeMap<(u8, u16), LocalActionCatalogEntry>,
    states: BTreeMap<String, LocalActionAdmissionState>,
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

    pub fn bind_server_catalog(
        &mut self,
        entries: Vec<LocalActionCatalogEntry>,
    ) -> Result<(), OrgError> {
        if entries.len() > MAX_LOCAL_ACTION_CATALOG {
            return Err(OrgError::BoundExceeded);
        }
        let mut catalog = BTreeMap::new();
        for entry in entries {
            if entry.version == 0 {
                return Err(OrgError::StalePolicy);
            }
            let expected = Self::replay_policy(entry.kind, entry.version)?;
            if entry.replay_policy != expected {
                return Err(OrgError::LastWriteWinsForbidden);
            }
            if entry.risk == ActionRisk::Production
                && entry.replay_policy != ReplayPolicy::NeverAssumeRetrySafe
            {
                return Err(OrgError::ProductionRiskNotRetrySafe);
            }
            let key = (action_kind_tag(entry.kind), entry.version);
            if catalog.insert(key, entry).is_some() {
                return Err(OrgError::DuplicateLink);
            }
        }
        self.catalog = catalog;
        Ok(())
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
        Ok(self
            .admit_with_state(
                membership,
                request,
                local_host_id,
                local_project,
                local_fingerprint,
                owner_approved,
                now_ms,
            )?
            .replay_policy
            .ok_or(OrgError::StalePolicy)?)
    }

    pub fn admit_with_state(
        &mut self,
        membership: &HostMembership,
        request: &LocalActionRequest,
        local_host_id: &str,
        local_project: ProjectId,
        local_fingerprint: &str,
        owner_approved: bool,
        now_ms: i64,
    ) -> Result<LocalActionAdmissionState, OrgError> {
        if request.payload.len() > MAX_LOCAL_ACTION_PAYLOAD_BYTES {
            return Err(OrgError::BoundExceeded);
        }
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
        if let Some(entry) = self
            .catalog
            .get(&(action_kind_tag(request.kind), request.version))
        {
            if entry.replay_policy != policy || entry.risk != request.risk {
                return Err(OrgError::StalePolicy);
            }
        } else if !self.catalog.is_empty() {
            return Err(OrgError::StalePolicy);
        }
        if request.risk == ActionRisk::Production && policy != ReplayPolicy::NeverAssumeRetrySafe {
            return Err(OrgError::ProductionRiskNotRetrySafe);
        }
        let request_key = request.request_id.to_string();
        if self.seen.contains(&request_key) {
            return Err(OrgError::Replay);
        }
        if self.seen.len() >= MAX_LOCAL_ACTION_SEEN {
            return Err(OrgError::BoundExceeded);
        }
        self.seen.insert(request_key.clone());
        let state = LocalActionAdmissionState {
            request_id: request.request_id,
            admission: Admission::Accepted,
            replay_policy: Some(policy),
            outcome: None,
            reconcile: LocalActionReconcileState::AwaitingHostExecution,
        };
        self.states.insert(request_key, state.clone());
        Ok(state)
    }

    pub fn mark_uncertain(
        &mut self,
        request_id: LocalActionId,
    ) -> Result<LocalActionAdmissionState, OrgError> {
        let state = self
            .states
            .get_mut(&request_id.to_string())
            .ok_or(OrgError::Unlinked)?;
        if state.reconcile == LocalActionReconcileState::AwaitingHostExecution {
            state.outcome = Some(ActionOutcome::Uncertain);
            state.reconcile = LocalActionReconcileState::Uncertain;
        }
        if state.reconcile == LocalActionReconcileState::Uncertain {
            return Ok(state.clone());
        }
        Err(OrgError::UncertainOutcome)
    }

    pub fn retry_uncertain(&self, request_id: LocalActionId) -> OrgError {
        match self.states.get(&request_id.to_string()) {
            Some(state) if state.reconcile == LocalActionReconcileState::Uncertain => {
                OrgError::UncertainOutcome
            }
            _ => OrgError::Unlinked,
        }
    }

    pub fn admission_state(&self, request_id: LocalActionId) -> Option<&LocalActionAdmissionState> {
        self.states.get(&request_id.to_string())
    }

    /// Apply an authoritative Portal action status to an action already
    /// admitted locally. Unknown request ids are rejected so a remote tenant
    /// cannot create local action state by merely appearing in a sync page.
    pub fn reconcile_remote_state(
        &mut self,
        request_id: LocalActionId,
        admission: Admission,
        outcome: Option<ActionOutcome>,
    ) -> Result<SyncOutcome, OrgError> {
        let state = self
            .states
            .get_mut(&request_id.to_string())
            .ok_or(OrgError::Unlinked)?;
        if state.outcome.is_some() && state.outcome != outcome {
            return Err(OrgError::LastWriteWinsForbidden);
        }
        let reconcile = match (admission, outcome) {
            (Admission::Rejected, _) => LocalActionReconcileState::Rejected,
            (Admission::Accepted, Some(ActionOutcome::Settled)) => {
                LocalActionReconcileState::Settled
            }
            (Admission::Accepted, Some(ActionOutcome::Failed)) => LocalActionReconcileState::Failed,
            (Admission::Accepted, Some(ActionOutcome::Cancelled)) => {
                LocalActionReconcileState::Cancelled
            }
            (Admission::Accepted, Some(ActionOutcome::Uncertain)) => {
                LocalActionReconcileState::Uncertain
            }
            (Admission::Accepted, None) => LocalActionReconcileState::AwaitingHostExecution,
        };
        if state.admission == admission && state.outcome == outcome && state.reconcile == reconcile
        {
            return Ok(SyncOutcome::Duplicate);
        }
        state.admission = admission;
        state.outcome = outcome;
        state.reconcile = reconcile;
        Ok(SyncOutcome::Applied)
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

    pub fn persist_states(&self) -> Vec<LocalActionAdmissionState> {
        self.states.values().cloned().collect()
    }

    pub fn persist_catalog(&self) -> Vec<LocalActionCatalogEntry> {
        self.catalog.values().cloned().collect()
    }

    pub fn persist_seen(&self) -> Vec<String> {
        self.seen.iter().cloned().collect()
    }

    pub fn restore(
        catalog: Vec<LocalActionCatalogEntry>,
        states: Vec<LocalActionAdmissionState>,
        seen: Vec<String>,
    ) -> Result<Self, OrgError> {
        let mut registry = Self::new();
        registry.bind_server_catalog(catalog)?;
        if states.len() > MAX_LOCAL_ACTION_SEEN || seen.len() > MAX_LOCAL_ACTION_SEEN {
            return Err(OrgError::BoundExceeded);
        }
        let mut restored_seen = std::collections::BTreeSet::new();
        for id in seen {
            if id.trim().is_empty() {
                return Err(OrgError::EmptyIdentity);
            }
            if !restored_seen.insert(id) {
                return Err(OrgError::Replay);
            }
        }
        let mut restored_states = BTreeMap::new();
        for state in states {
            let key = state.request_id.to_string();
            if restored_states.contains_key(&key) {
                return Err(OrgError::Replay);
            }
            restored_seen.insert(key.clone());
            restored_states.insert(key, state);
        }
        if restored_seen.len() > MAX_LOCAL_ACTION_SEEN {
            return Err(OrgError::BoundExceeded);
        }
        registry.seen = restored_seen;
        registry.states = restored_states;
        Ok(registry)
    }
}

impl HostMembership {
    fn local_action_requirement(&self) -> LocalActionApprovalRequirement {
        LocalActionApprovalRequirement::OwnerRequired
    }
}

fn action_kind_tag(kind: LocalActionKind) -> u8 {
    match kind {
        LocalActionKind::DbSchemaIntrospect => 1,
        LocalActionKind::DbApprovedChangeApply => 2,
        LocalActionKind::EnvDiff => 3,
        LocalActionKind::EnvApprovedApply => 4,
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
