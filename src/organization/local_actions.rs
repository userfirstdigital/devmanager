//! Host-side admission seam for organization DB/ENV actions.
//!
//! This module intentionally stops at authenticated admission.  The DB and
//! ENV providers are optional products and are not invoked by DevManager.  A
//! missing provider therefore produces a durable, idempotent unavailable
//! receipt instead of attempting a shell command or inventing a result.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::connect::ConnectHostId;
use crate::domain::id::{OperationId, ProjectId, TaskId};
use crate::domain::task::WorkspaceRef;
use crate::org::{ActionRisk, LocalActionId, LocalActionKind, MembershipRole, OrgError};
use crate::protocol::{Capability, CapabilitySet};

pub const MAX_ACTION_DIGEST_BYTES: usize = 64;
pub const MAX_RECEIPTS: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthenticatedActionContext {
    pub tenant_id: String,
    pub host_id: ConnectHostId,
    pub role: MembershipRole,
    pub capabilities: CapabilitySet,
    pub task_id: TaskId,
    pub project_id: ProjectId,
    pub workspace: WorkspaceRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalActionRequest {
    pub request_id: LocalActionId,
    pub operation_id: OperationId,
    pub tenant_id: String,
    pub host_id: ConnectHostId,
    pub task_id: TaskId,
    pub project_id: ProjectId,
    pub workspace: WorkspaceRef,
    pub kind: LocalActionKind,
    pub risk: ActionRisk,
    /// A digest of an approved provider-owned payload.  Raw SQL, env values,
    /// credentials, and command text never cross this boundary.
    pub payload_digest: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalActionReceiptStatus {
    Unavailable,
    Rejected,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalActionReceipt {
    pub operation_id: OperationId,
    pub request_id: LocalActionId,
    pub task_id: TaskId,
    pub status: LocalActionReceiptStatus,
    pub error: String,
    pub request_digest: String,
}

#[derive(Debug, Default)]
pub struct LocalActionBridge {
    receipts: BTreeMap<OperationId, (String, LocalActionReceipt)>,
}

impl LocalActionBridge {
    pub fn new() -> Self {
        Self::default()
    }

    /// Admit an authenticated request and return the same receipt for an
    /// identical operation retry.  No local provider is executed here.
    pub fn submit(
        &mut self,
        context: &AuthenticatedActionContext,
        request: &LocalActionRequest,
    ) -> Result<LocalActionReceipt, OrgError> {
        let digest = request_digest(request)?;
        if let Some((existing_digest, receipt)) = self.receipts.get(&request.operation_id) {
            if existing_digest != &digest {
                return Err(OrgError::LastWriteWinsForbidden);
            }
            return Ok(receipt.clone());
        }
        authorize(context, request)?;
        if self.receipts.len() >= MAX_RECEIPTS {
            return Err(OrgError::BoundExceeded);
        }
        let receipt = LocalActionReceipt {
            operation_id: request.operation_id,
            request_id: request.request_id,
            task_id: request.task_id,
            status: LocalActionReceiptStatus::Unavailable,
            error: "local DB/ENV provider is not installed".to_string(),
            request_digest: digest.clone(),
        };
        self.receipts
            .insert(request.operation_id, (digest, receipt.clone()));
        Ok(receipt)
    }

    pub fn receipt(&self, operation_id: OperationId) -> Option<&LocalActionReceipt> {
        self.receipts.get(&operation_id).map(|(_, receipt)| receipt)
    }
}

fn authorize(
    context: &AuthenticatedActionContext,
    request: &LocalActionRequest,
) -> Result<(), OrgError> {
    if context.tenant_id.is_empty() || request.tenant_id != context.tenant_id {
        return Err(OrgError::CrossTenant);
    }
    if request.host_id != context.host_id
        || request.task_id != context.task_id
        || request.project_id != context.project_id
        || request.workspace != context.workspace
    {
        return Err(OrgError::FingerprintMismatch);
    }
    if !context
        .capabilities
        .contains(Capability::OrganizationProjection)
    {
        return Err(OrgError::Unavailable(
            crate::org::OrgDependency::SignedIdentityIssuer,
        ));
    }
    if !context.role.can_read_published() {
        return Err(OrgError::RoleDenied);
    }
    if matches!(
        request.kind,
        LocalActionKind::DbApprovedChangeApply | LocalActionKind::EnvApprovedApply
    ) && !context.role.can_administer()
    {
        return Err(OrgError::OwnerOnly);
    }
    if request.payload_digest.len() != MAX_ACTION_DIGEST_BYTES
        || !request
            .payload_digest
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(OrgError::ProhibitedField);
    }
    Ok(())
}

fn request_digest(request: &LocalActionRequest) -> Result<String, OrgError> {
    let bytes = serde_json::to_vec(request).map_err(|_| OrgError::ProhibitedField)?;
    Ok(hex_encode(&Sha256::digest(bytes)))
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> (AuthenticatedActionContext, LocalActionRequest) {
        let task_id = TaskId::new();
        let project_id = ProjectId::new();
        let workspace = WorkspaceRef::Main;
        let host_id = ConnectHostId::new();
        let context = AuthenticatedActionContext {
            tenant_id: "tenant".into(),
            host_id,
            role: MembershipRole::Manager,
            capabilities: CapabilitySet::from_capabilities([Capability::OrganizationProjection]),
            task_id,
            project_id,
            workspace: workspace.clone(),
        };
        let request = LocalActionRequest {
            request_id: LocalActionId::new(),
            operation_id: OperationId::new(),
            tenant_id: "tenant".into(),
            host_id,
            task_id,
            project_id,
            workspace,
            kind: LocalActionKind::EnvDiff,
            risk: ActionRisk::Low,
            payload_digest: "a".repeat(64),
        };
        (context, request)
    }

    #[test]
    fn provider_missing_returns_idempotent_unavailable_receipt() {
        let (context, request) = request();
        let mut bridge = LocalActionBridge::new();
        let first = bridge.submit(&context, &request).expect("admission");
        let second = bridge.submit(&context, &request).expect("retry");
        assert_eq!(first, second);
        assert_eq!(first.status, LocalActionReceiptStatus::Unavailable);
        assert_eq!(first.task_id, request.task_id);
    }

    #[test]
    fn mismatched_workspace_is_rejected_before_provider_boundary() {
        let (context, mut request) = request();
        request.workspace = WorkspaceRef::external("C:\\other").expect("workspace");
        let error = LocalActionBridge::new().submit(&context, &request);
        assert_eq!(error, Err(OrgError::FingerprintMismatch));
    }
}
