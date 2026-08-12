//! One-to-one local Task ↔ BoardCard links. Title has two writers; conflicts
//! stay visible. Last-write-wins is forbidden.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::connect::ConnectHostId;
use crate::domain::canonical;
use crate::domain::id::TaskId;
use crate::domain::org::TaskScope;
use crate::org::error::OrgError;
use crate::org::identity::{BoardCardId, PortalTenantId};
use crate::org::ids::ManagedLinkId;
use crate::org::membership::HostMembership;

pub const MAX_MANAGED_LINKS: usize = 128;

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedTaskSnapshot {
    pub host_id: ConnectHostId,
    pub local_task_id: TaskId,
    pub board_card_id: BoardCardId,
    pub enrollment_state: EnrollmentState,
    pub portal_revision: u64,
    pub metadata_policy_version: u32,
    pub linked_by: String,
    pub linked_at: i64,
    pub unlinked_at: Option<i64>,
    pub link_id: ManagedLinkId,
    pub tenant_id: PortalTenantId,
    pub portal_title: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncOutcome {
    Applied,
    Duplicate,
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
        let local_title = local_title.into();
        let portal_title = portal_title.into();
        let link = self
            .by_task
            .get_mut(&host_task_key(host_id, local_task_id))
            .ok_or(OrgError::Unlinked)?;
        if local_title == portal_title {
            link.title_conflict = None;
            return Ok(link);
        }
        if local_revision == portal_revision {
            return Err(OrgError::LastWriteWinsForbidden);
        }
        link.title_conflict = Some(TitleConflict {
            local_title,
            portal_title,
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

    pub fn enrolled_count(&self) -> usize {
        self.by_task
            .values()
            .filter(|link| link.enrollment_state == EnrollmentState::Enrolled)
            .count()
    }

    pub fn len(&self) -> usize {
        self.by_task.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_task.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &ManagedTaskLink> {
        self.by_task.values()
    }

    pub fn apply_portal_snapshot(
        &mut self,
        membership: &HostMembership,
        snapshot: ManagedTaskSnapshot,
    ) -> Result<SyncOutcome, OrgError> {
        validate_snapshot_identities(membership, &snapshot)?;
        let incoming = link_from_snapshot(snapshot);
        let task_key = host_task_key(incoming.host_id, incoming.local_task_id);
        match self.by_task.get(&task_key) {
            None => {
                if incoming.enrollment_state == EnrollmentState::Unlinked
                    || incoming.enrollment_state == EnrollmentState::Closed
                {
                    return Ok(SyncOutcome::Duplicate);
                }
                if self.by_task.len() >= MAX_MANAGED_LINKS {
                    return Err(OrgError::BoundExceeded);
                }
                if self.by_card.contains_key(incoming.board_card_id.as_str()) {
                    return Err(OrgError::DuplicateLink);
                }
                if incoming.enrollment_state == EnrollmentState::Enrolled {
                    self.by_card.insert(
                        incoming.board_card_id.as_str().to_string(),
                        incoming.link_id,
                    );
                }
                self.by_task.insert(task_key, incoming);
                Ok(SyncOutcome::Applied)
            }
            Some(existing) => {
                if existing.link_id != incoming.link_id
                    || existing.board_card_id != incoming.board_card_id
                    || existing.tenant_id != incoming.tenant_id
                    || existing.host_id != incoming.host_id
                    || existing.local_task_id != incoming.local_task_id
                {
                    return Err(OrgError::LinkConflict);
                }
                if incoming.portal_revision < existing.portal_revision {
                    return Err(OrgError::StalePolicy);
                }
                if incoming.portal_revision == existing.portal_revision {
                    return if snapshot_fields_match(existing, &incoming) {
                        Ok(SyncOutcome::Duplicate)
                    } else {
                        Err(OrgError::LastWriteWinsForbidden)
                    };
                }
                let card_key = existing.board_card_id.as_str().to_string();
                let mut updated = existing.clone();
                updated.enrollment_state = incoming.enrollment_state;
                updated.portal_revision = incoming.portal_revision;
                updated.metadata_policy_version = incoming.metadata_policy_version;
                updated.linked_by = incoming.linked_by;
                updated.linked_at = incoming.linked_at;
                updated.unlinked_at = incoming.unlinked_at;
                updated.title_conflict = incoming.title_conflict;
                if updated.enrollment_state != EnrollmentState::Enrolled {
                    self.by_card.remove(&card_key);
                } else {
                    self.by_card.insert(card_key, updated.link_id);
                }
                self.by_task.insert(task_key, updated);
                Ok(SyncOutcome::Applied)
            }
        }
    }
}

fn validate_snapshot_identities(
    membership: &HostMembership,
    snapshot: &ManagedTaskSnapshot,
) -> Result<(), OrgError> {
    if !membership.is_enrolled() {
        return Err(match membership.status {
            crate::org::membership::MembershipStatus::Revoked => OrgError::MembershipRevoked,
            crate::org::membership::MembershipStatus::PendingLocalConfirm => {
                OrgError::EnrollmentNotConfirmed
            }
            _ => OrgError::HostUnenrolled,
        });
    }
    if membership.host_id != snapshot.host_id {
        return Err(OrgError::HostUnenrolled);
    }
    if membership.tenant_id != snapshot.tenant_id {
        return Err(OrgError::CrossTenant);
    }
    if snapshot.portal_revision == 0 || snapshot.metadata_policy_version == 0 {
        return Err(OrgError::StalePolicy);
    }
    if canonical::canonicalize(snapshot.linked_by.as_str()).is_none() {
        return Err(OrgError::EmptyIdentity);
    }
    Ok(())
}

fn link_from_snapshot(snapshot: ManagedTaskSnapshot) -> ManagedTaskLink {
    ManagedTaskLink {
        host_id: snapshot.host_id,
        local_task_id: snapshot.local_task_id,
        board_card_id: snapshot.board_card_id,
        enrollment_state: snapshot.enrollment_state,
        local_revision: 1,
        portal_revision: snapshot.portal_revision,
        metadata_policy_version: snapshot.metadata_policy_version,
        linked_by: snapshot.linked_by,
        linked_at: snapshot.linked_at,
        unlinked_at: snapshot.unlinked_at,
        link_id: snapshot.link_id,
        tenant_id: snapshot.tenant_id,
        title_conflict: None,
    }
}

fn snapshot_fields_match(existing: &ManagedTaskLink, incoming: &ManagedTaskLink) -> bool {
    existing.enrollment_state == incoming.enrollment_state
        && existing.metadata_policy_version == incoming.metadata_policy_version
        && existing.linked_by == incoming.linked_by
        && existing.linked_at == incoming.linked_at
        && existing.unlinked_at == incoming.unlinked_at
        && existing.board_card_id == incoming.board_card_id
        && fact_hash(existing) == fact_hash(incoming)
}

fn fact_hash(link: &ManagedTaskLink) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(link.host_id.as_bytes());
    hasher.update(link.local_task_id.as_bytes());
    hasher.update(link.board_card_id.as_str().as_bytes());
    hasher.update([match link.enrollment_state {
        EnrollmentState::PendingOwnerAccept => 1,
        EnrollmentState::Enrolled => 2,
        EnrollmentState::Unlinked => 3,
        EnrollmentState::Closed => 4,
    }]);
    hasher.update(link.portal_revision.to_le_bytes());
    hasher.update(link.metadata_policy_version.to_le_bytes());
    hasher.update(link.linked_by.as_bytes());
    hasher.update(link.linked_at.to_le_bytes());
    hasher.update(link.link_id.to_string().as_bytes());
    hasher.update(link.tenant_id.as_str().as_bytes());
    hasher.finalize().into()
}

fn host_task_key(host_id: ConnectHostId, task_id: TaskId) -> (u128, u128) {
    (
        u128::from_be_bytes(host_id.as_bytes()),
        u128::from_be_bytes(*task_id.as_bytes()),
    )
}
