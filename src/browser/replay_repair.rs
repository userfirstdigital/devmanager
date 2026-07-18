use super::{BrowserReplayInstance, BrowserResourceHandle, BrowserRevision, BrowserWorkspaceKey};
use serde::Serialize;
use std::hash::{Hash, Hasher};
use std::num::NonZeroU64;
use std::sync::Arc;

pub(super) struct BrowserReplayRepairAuthorityScope;

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "camelCase")]
pub enum BrowserReplayLocatorSlot {
    PrimaryAction,
    OptionalAction,
    DragSource,
    DragDestination,
    ActionWait,
    StepWait,
    Assertion { index: usize },
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum BrowserReplayRepairPhase {
    AwaitingPreview,
    Previewed,
    Applied,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BrowserReplayRepairProjection {
    pub workspace_key: BrowserWorkspaceKey,
    pub replay_instance_id: u64,
    pub repair_id: u64,
    pub recipe_id: String,
    pub step_id: String,
    pub step_index: usize,
    pub locator_slot: BrowserReplayLocatorSlot,
    pub tab_id: String,
    pub revision: BrowserRevision,
    pub snapshot: BrowserResourceHandle,
    pub screenshot: BrowserResourceHandle,
    pub phase: BrowserReplayRepairPhase,
}

#[derive(Clone)]
pub struct BrowserReplayRepairInstance {
    replay: BrowserReplayInstance,
    repair_id: NonZeroU64,
    scope: Arc<BrowserReplayRepairAuthorityScope>,
}

impl BrowserReplayRepairInstance {
    pub fn workspace_key(&self) -> &BrowserWorkspaceKey {
        self.replay.workspace_key()
    }

    pub fn replay_instance_id(&self) -> u64 {
        self.replay.id()
    }

    pub fn repair_id(&self) -> u64 {
        self.repair_id.get()
    }

    pub(super) fn new(
        replay: BrowserReplayInstance,
        repair_id: NonZeroU64,
        scope: Arc<BrowserReplayRepairAuthorityScope>,
    ) -> Self {
        Self {
            replay,
            repair_id,
            scope,
        }
    }

    pub(super) fn replay(&self) -> &BrowserReplayInstance {
        &self.replay
    }
}

impl PartialEq for BrowserReplayRepairInstance {
    fn eq(&self, other: &Self) -> bool {
        self.replay == other.replay
            && self.repair_id == other.repair_id
            && Arc::ptr_eq(&self.scope, &other.scope)
    }
}

impl Eq for BrowserReplayRepairInstance {}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum BrowserReplayRepairResumeCursor {
    Action,
    ActionWait,
    StepWait,
    Assertion(usize),
}

pub(crate) struct BrowserReplayRepairRetentionAuthority {
    key: BrowserReplayRepairRetentionAuthorityKey,
}

pub(crate) struct BrowserReplayRepairRetentionAuthorityKey {
    owner: BrowserWorkspaceKey,
    replay_id: u64,
    repair_id: u64,
    scope: Arc<BrowserReplayRepairAuthorityScope>,
}

impl BrowserReplayRepairRetentionAuthority {
    pub(super) fn for_repair(instance: &BrowserReplayRepairInstance) -> Self {
        Self {
            key: BrowserReplayRepairRetentionAuthorityKey {
                owner: instance.workspace_key().clone(),
                replay_id: instance.replay_instance_id(),
                repair_id: instance.repair_id(),
                scope: Arc::clone(&instance.scope),
            },
        }
    }

    #[cfg(test)]
    pub(super) fn issue_for_test(
        owner: BrowserWorkspaceKey,
        replay_id: u64,
        repair_id: u64,
    ) -> Self {
        Self {
            key: BrowserReplayRepairRetentionAuthorityKey {
                owner,
                replay_id,
                repair_id,
                scope: Arc::new(BrowserReplayRepairAuthorityScope),
            },
        }
    }

    pub(super) fn key(&self) -> BrowserReplayRepairRetentionAuthorityKey {
        self.key.clone()
    }
}

impl Clone for BrowserReplayRepairRetentionAuthorityKey {
    fn clone(&self) -> Self {
        Self {
            owner: self.owner.clone(),
            replay_id: self.replay_id,
            repair_id: self.repair_id,
            scope: Arc::clone(&self.scope),
        }
    }
}

impl PartialEq for BrowserReplayRepairRetentionAuthorityKey {
    fn eq(&self, other: &Self) -> bool {
        self.owner == other.owner
            && self.replay_id == other.replay_id
            && self.repair_id == other.repair_id
            && Arc::ptr_eq(&self.scope, &other.scope)
    }
}

impl Eq for BrowserReplayRepairRetentionAuthorityKey {}

impl Hash for BrowserReplayRepairRetentionAuthorityKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.owner.hash(state);
        self.replay_id.hash(state);
        self.repair_id.hash(state);
        Arc::as_ptr(&self.scope).hash(state);
    }
}

impl BrowserReplayRepairRetentionAuthorityKey {
    pub(super) fn owner(&self) -> &BrowserWorkspaceKey {
        &self.owner
    }
}
