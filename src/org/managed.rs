//! One-to-one local Task ↔ BoardCard links. Title has two writers; conflicts
//! stay visible. Last-write-wins is forbidden.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::connect::ConnectHostId;
use crate::domain::id::TaskId;
use crate::domain::org::TaskScope;
use crate::org::error::OrgError;
use crate::org::identity::{BoardCardId, PortalTenantId};
use crate::org::ids::ManagedLinkId;
use crate::org::membership::HostMembership;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnrollmentState {
    PendingOwnerAccept,
    Enrolled,
    Unlinked,
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedTaskLink {
    pub host_id: ConnectHostId,
    pub local_task_id: TaskId,
    pub board_card_id: BoardCardId,
    pub enrollment_state: EnrollmentState,
    pub local_revision: u64,
    pub portal_revision: u64,
    pub metadata_policy_version: u32,
    pub linked_by: String,
    pub linked_at: i64,
    pub unlinked_at: Option<i64>,
    pub link_id: ManagedLinkId,
    pub tenant_id: PortalTenantId,
    pub title_conflict: Option<TitleConflict>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TitleConflict {
    pub local_title: String,
    pub portal_title: String,
    pub local_revision: u64,
    pub portal_revision: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldAuthority {
    BoardCard,
    LocalTask,
    DualWriter,
}

impl FieldAuthority {
    pub const fn for_field(field: DualField) -> Self {
        match field {
            DualField::Assignment
            | DualField::BoardColumn
            | DualField::Deadline
            | DualField::Phase
            | DualField::Dependency
            | DualField::Comment
            | DualField::Handoff => Self::BoardCard,
            DualField::RuntimeLifecycle
            | DualField::Attention
            | DualField::Provider
            | DualField::Resource => Self::LocalTask,
            DualField::Title => Self::DualWriter,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DualField {
    Assignment,
    BoardColumn,
    Deadline,
    Phase,
    Dependency,
    Comment,
    Handoff,
    RuntimeLifecycle,
    Attention,
    Provider,
    Resource,
    Title,
}

#[derive(Debug, Default)]
pub struct TaskLinkReducer {
    by_task: BTreeMap<(u128, u128), ManagedTaskLink>,
    by_card: BTreeMap<String, ManagedLinkId>,
}

impl TaskLinkReducer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn link(
        &mut self,
        membership: &HostMembership,
        local_task_id: TaskId,
        board_card_id: BoardCardId,
        linked_by: impl Into<String>,
        linked_at: i64,
        policy_revision: u32,
    ) -> Result<ManagedTaskLink, OrgError> {
        if !membership.is_enrolled() {
            return Err(match membership.status {
                crate::org::membership::MembershipStatus::Revoked => OrgError::MembershipRevoked,
                crate::org::membership::MembershipStatus::PendingLocalConfirm => {
                    OrgError::EnrollmentNotConfirmed
                }
                _ => OrgError::HostUnenrolled,
            });
        }
        if membership.role.is_disabled() {
            return Err(OrgError::DisabledMember);
        }
        let task_key = host_task_key(membership.host_id, local_task_id);
        if self.by_task.contains_key(&task_key) {
            return Err(OrgError::DuplicateLink);
        }
        if self.by_card.contains_key(board_card_id.as_str()) {
            return Err(OrgError::DuplicateLink);
        }
        let link = ManagedTaskLink {
            host_id: membership.host_id,
            local_task_id,
            board_card_id: board_card_id.clone(),
            enrollment_state: EnrollmentState::Enrolled,
            local_revision: 1,
            portal_revision: 1,
            metadata_policy_version: policy_revision,
            linked_by: linked_by.into(),
            linked_at,
            unlinked_at: None,
            link_id: ManagedLinkId::new(),
            tenant_id: membership.tenant_id.clone(),
            title_conflict: None,
        };
        self.by_card
            .insert(board_card_id.as_str().to_string(), link.link_id);
        self.by_task.insert(task_key, link.clone());
        Ok(link)
    }

    pub fn get(&self, host_id: ConnectHostId, local_task_id: TaskId) -> Option<&ManagedTaskLink> {
        self.by_task.get(&host_task_key(host_id, local_task_id))
    }

    pub fn scope_for(&self, host_id: ConnectHostId, local_task_id: TaskId) -> TaskScope {
        match self.get(host_id, local_task_id) {
            Some(link) if link.enrollment_state == EnrollmentState::Enrolled => {
                TaskScope::managed(link.link_id.to_string(), link.metadata_policy_version)
                    .expect("stored link is valid")
            }
            _ => TaskScope::personal(),
        }
    }

    pub fn record_title_conflict(
        &mut self,
        host_id: ConnectHostId,
        local_task_id: TaskId,
        local_title: impl Into<String>,
        portal_title: impl Into<String>,
        local_revision: u64,
        portal_revision: u64,
    ) -> Result<&ManagedTaskLink, OrgError> {
        let link = self
            .by_task
            .get_mut(&host_task_key(host_id, local_task_id))
            .ok_or(OrgError::Unlinked)?;
        if local_title.as_ref() == portal_title.as_ref() {
            link.title_conflict = None;
            return Ok(link);
        }
        if local_revision == portal_revision {
            return Err(OrgError::LastWriteWinsForbidden);
        }
        link.title_conflict = Some(TitleConflict {
            local_title: local_title.into(),
            portal_title: portal_title.into(),
            local_revision,
            portal_revision,
        });
        Err(OrgError::LinkConflict)
    }

    pub fn unlink(
        &mut self,
        host_id: ConnectHostId,
        local_task_id: TaskId,
        now_ms: i64,
    ) -> Result<ManagedTaskLink, OrgError> {
        let link = self
            .by_task
            .get_mut(&host_task_key(host_id, local_task_id))
            .ok_or(OrgError::Unlinked)?;
        link.enrollment_state = EnrollmentState::Unlinked;
        link.unlinked_at = Some(now_ms);
        self.by_card.remove(link.board_card_id.as_str());
        Ok(link.clone())
    }

    pub fn close(
        &mut self,
        host_id: ConnectHostId,
        local_task_id: TaskId,
    ) -> Result<&ManagedTaskLink, OrgError> {
        let link = self
            .by_task
            .get_mut(&host_task_key(host_id, local_task_id))
            .ok_or(OrgError::Unlinked)?;
        link.enrollment_state = EnrollmentState::Closed;
        Ok(link)
    }
}

fn host_task_key(host_id: ConnectHostId, task_id: TaskId) -> (u128, u128) {
    (
        u128::from_be_bytes(host_id.as_bytes()),
        u128::from_be_bytes(*task_id.as_bytes()),
    )
}
