//! Connect-facing managed Task link and Personal/Managed scope projection.

pub use crate::org::{
    DualField, EnrollmentState, FieldAuthority, ManagedTaskLink, TaskLinkReducer, TitleConflict,
};

use crate::domain::id::TaskId;
use crate::domain::org::TaskScope;

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
}
