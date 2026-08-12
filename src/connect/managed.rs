//! Connect-facing managed Task link and Personal/Managed scope projection.

pub use crate::org::{
    DualField, EnrollmentState, FieldAuthority, ManagedTaskLink, ManagedTaskSnapshot, SyncOutcome,
    TaskLinkReducer, TitleConflict,
};

use crate::domain::id::TaskId;
use crate::domain::org::TaskScope;
use crate::org::{HostMembership, OrgError};

use super::ConnectHostId;

pub struct ManagedTaskProjection<'a> {
    reducer: &'a TaskLinkReducer,
    host_id: ConnectHostId,
}

impl<'a> ManagedTaskProjection<'a> {
    pub fn new(reducer: &'a TaskLinkReducer, host_id: ConnectHostId) -> Self {
        Self { reducer, host_id }
    }

    pub fn scope(&self, task_id: TaskId) -> TaskScope {
        self.reducer.scope_for(self.host_id, task_id)
    }

    pub fn link(&self, task_id: TaskId) -> Option<&'a ManagedTaskLink> {
        self.reducer.get(self.host_id, task_id)
    }

    pub fn exported_count(&self) -> usize {
        self.reducer.enrolled_count()
    }
}

/// Typed adapter that applies Portal-authoritative managed Task facts onto the
/// local one-to-one link reducer. Last-write-wins is rejected.
pub struct ManagedTaskAdapter;

impl ManagedTaskAdapter {
    pub fn reconcile(
        reducer: &mut TaskLinkReducer,
        membership: &HostMembership,
        snapshot: ManagedTaskSnapshot,
    ) -> Result<SyncOutcome, OrgError> {
        reducer.apply_portal_snapshot(membership, snapshot)
    }

    pub fn scope_for(
        reducer: &TaskLinkReducer,
        host_id: ConnectHostId,
        task_id: TaskId,
    ) -> TaskScope {
        reducer.scope_for(host_id, task_id)
    }
}
