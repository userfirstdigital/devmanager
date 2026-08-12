//! Connect adapter for typed DB Flow/ENV local actions.

pub use crate::org::{
    LocalActionAdmissionState, LocalActionCatalogEntry, LocalActionKind, LocalActionReceipt,
    LocalActionReconcileState, LocalActionRegistry, LocalActionRequest, ReplayPolicy,
};

use crate::domain::id::ProjectId;
use crate::org::{HostMembership, LocalActionId, OrgError};

/// Host-executed local-action adapter. Admission never claims dispatch success.
pub struct LocalActionAdapter {
    registry: LocalActionRegistry,
}

impl LocalActionAdapter {
    pub fn new() -> Self {
        Self {
            registry: LocalActionRegistry::new(),
        }
    }

    pub fn bind_catalog(&mut self, entries: Vec<LocalActionCatalogEntry>) -> Result<(), OrgError> {
        self.registry.bind_server_catalog(entries)
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
    ) -> Result<LocalActionAdmissionState, OrgError> {
        self.registry.admit_with_state(
            membership,
            request,
            local_host_id,
            local_project,
            local_fingerprint,
            owner_approved,
            now_ms,
        )
    }

    pub fn mark_uncertain(
        &mut self,
        request_id: LocalActionId,
    ) -> Result<LocalActionAdmissionState, OrgError> {
        self.registry.mark_uncertain(request_id)
    }

    pub fn retry_uncertain(&self, request_id: LocalActionId) -> OrgError {
        self.registry.retry_uncertain(request_id)
    }

    pub fn state(&self, request_id: LocalActionId) -> Option<&LocalActionAdmissionState> {
        self.registry.admission_state(request_id)
    }
}

impl Default for LocalActionAdapter {
    fn default() -> Self {
        Self::new()
    }
}
