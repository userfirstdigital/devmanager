use super::BrowserWorkspaceKey;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

struct BrowserReplayRepairAuthorityScope;

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
