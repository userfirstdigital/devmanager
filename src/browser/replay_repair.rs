use super::{
    BrowserRecipeLocator, BrowserReplayInstance, BrowserReplayRepairCandidate,
    BrowserResourceHandle, BrowserRevision, BrowserWorkspaceKey,
};
use serde::Serialize;
use std::hash::{Hash, Hasher};
use std::num::NonZeroU64;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

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

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct BrowserReplayRecipeLocatorTarget {
    step_index: usize,
    step_id: String,
    locator_slot: BrowserReplayLocatorSlot,
    old_locator: BrowserRecipeLocator,
}

impl BrowserReplayRecipeLocatorTarget {
    pub(super) fn new(
        step_index: usize,
        step_id: String,
        locator_slot: BrowserReplayLocatorSlot,
        old_locator: BrowserRecipeLocator,
    ) -> Self {
        Self {
            step_index,
            step_id,
            locator_slot,
            old_locator,
        }
    }

    pub(crate) fn step_index(&self) -> usize {
        self.step_index
    }

    pub(crate) fn step_id(&self) -> &str {
        &self.step_id
    }

    pub(crate) fn locator_slot(&self) -> BrowserReplayLocatorSlot {
        self.locator_slot
    }

    pub(crate) fn old_locator(&self) -> &BrowserRecipeLocator {
        &self.old_locator
    }
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

#[derive(Clone)]
pub(crate) struct BrowserReplayRepairHighlightToken {
    repair: BrowserReplayRepairInstance,
    preview_id: NonZeroU64,
    tab_id: String,
    wire: Arc<str>,
}

impl BrowserReplayRepairHighlightToken {
    pub(super) fn new(
        repair: BrowserReplayRepairInstance,
        preview_id: NonZeroU64,
        tab_id: String,
        wire: String,
    ) -> Self {
        Self {
            repair,
            preview_id,
            tab_id,
            wire: Arc::from(wire),
        }
    }

    pub(crate) fn repair(&self) -> &BrowserReplayRepairInstance {
        &self.repair
    }

    pub(crate) fn tab_id(&self) -> &str {
        &self.tab_id
    }

    pub(crate) fn wire(&self) -> &str {
        &self.wire
    }
}

impl PartialEq for BrowserReplayRepairHighlightToken {
    fn eq(&self, other: &Self) -> bool {
        self.repair == other.repair
            && self.preview_id == other.preview_id
            && self.tab_id == other.tab_id
            && self.wire == other.wire
    }
}

impl Eq for BrowserReplayRepairHighlightToken {}

#[derive(Clone)]
struct BrowserReplayRepairPreviewLease(Arc<AtomicBool>);

impl BrowserReplayRepairPreviewLease {
    fn new() -> Self {
        Self(Arc::new(AtomicBool::new(true)))
    }

    pub(super) fn is_live(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    pub(super) fn close(&self) {
        self.0.store(false, Ordering::Release);
    }

    pub(super) fn same(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

struct BrowserReplayRepairPreviewReceiptState {
    repair: BrowserReplayRepairInstance,
    preview_id: NonZeroU64,
    candidate: BrowserReplayRepairCandidate,
    recipe_locator: BrowserRecipeLocator,
    token: BrowserReplayRepairHighlightToken,
    acknowledged: bool,
    consumed: bool,
}

#[derive(Clone)]
pub(crate) struct BrowserReplayRepairPreviewAuthority {
    repair: BrowserReplayRepairInstance,
    preview_id: NonZeroU64,
    tab_id: String,
    revision: BrowserRevision,
    candidate: BrowserReplayRepairCandidate,
    token: BrowserReplayRepairHighlightToken,
    expected_previous_token: Option<BrowserReplayRepairHighlightToken>,
    lease: BrowserReplayRepairPreviewLease,
    receipt: Arc<Mutex<BrowserReplayRepairPreviewReceiptState>>,
}

pub(crate) struct BrowserReplayRepairPreviewReceipt {
    lease: BrowserReplayRepairPreviewLease,
    state: Arc<Mutex<BrowserReplayRepairPreviewReceiptState>>,
}

pub(crate) struct BrowserReplayRepairPreviewAcknowledgement {
    pub(super) repair: BrowserReplayRepairInstance,
    pub(super) preview_id: NonZeroU64,
    pub(super) candidate: BrowserReplayRepairCandidate,
    pub(super) recipe_locator: BrowserRecipeLocator,
    pub(super) token: BrowserReplayRepairHighlightToken,
    lease: BrowserReplayRepairPreviewLease,
}

impl BrowserReplayRepairPreviewAuthority {
    pub(super) fn issue(
        repair: BrowserReplayRepairInstance,
        preview_id: NonZeroU64,
        tab_id: String,
        revision: BrowserRevision,
        candidate: BrowserReplayRepairCandidate,
        recipe_locator: BrowserRecipeLocator,
        token: BrowserReplayRepairHighlightToken,
        expected_previous_token: Option<BrowserReplayRepairHighlightToken>,
    ) -> (Self, BrowserReplayRepairPreviewReceipt) {
        let lease = BrowserReplayRepairPreviewLease::new();
        let receipt = Arc::new(Mutex::new(BrowserReplayRepairPreviewReceiptState {
            repair: repair.clone(),
            preview_id,
            candidate: candidate.clone(),
            recipe_locator,
            token: token.clone(),
            acknowledged: false,
            consumed: false,
        }));
        (
            Self {
                repair,
                preview_id,
                tab_id,
                revision,
                candidate,
                token,
                expected_previous_token,
                lease: lease.clone(),
                receipt: Arc::clone(&receipt),
            },
            BrowserReplayRepairPreviewReceipt {
                lease,
                state: receipt,
            },
        )
    }

    pub(crate) fn repair(&self) -> &BrowserReplayRepairInstance {
        &self.repair
    }

    pub(crate) fn preview_id(&self) -> u64 {
        self.preview_id.get()
    }

    pub(crate) fn tab_id(&self) -> &str {
        &self.tab_id
    }

    pub(crate) fn revision(&self) -> BrowserRevision {
        self.revision
    }

    pub(crate) fn candidate(&self) -> &BrowserReplayRepairCandidate {
        &self.candidate
    }

    pub(crate) fn token(&self) -> &BrowserReplayRepairHighlightToken {
        &self.token
    }

    pub(crate) fn expected_previous_token(&self) -> Option<&BrowserReplayRepairHighlightToken> {
        self.expected_previous_token.as_ref()
    }

    pub(crate) fn is_live(&self) -> bool {
        self.lease.is_live()
    }

    pub(crate) fn acknowledge_exact(&self, wire: &str) -> bool {
        if !self.is_live() || wire != self.token.wire() {
            return false;
        }
        let mut receipt = self
            .receipt
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if receipt.consumed || receipt.token != self.token {
            return false;
        }
        receipt.acknowledged = true;
        true
    }

    #[cfg(test)]
    pub(super) fn acknowledge_for_test(&self) -> bool {
        self.acknowledge_exact(self.token.wire())
    }

    #[cfg(test)]
    pub(super) fn expected_previous_token_is_some_for_test(&self) -> bool {
        self.expected_previous_token.is_some()
    }

    pub(super) fn close(&self) {
        self.lease.close();
    }

    pub(super) fn same_lease(&self, other: &BrowserReplayRepairPreviewAcknowledgement) -> bool {
        self.lease.same(&other.lease)
    }
}

impl BrowserReplayRepairPreviewReceipt {
    pub(crate) fn consume_exact(
        &self,
        repair: &BrowserReplayRepairInstance,
    ) -> Option<BrowserReplayRepairPreviewAcknowledgement> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.consumed {
            return None;
        }
        state.consumed = true;
        let exact = self.lease.is_live() && state.acknowledged && state.repair == *repair;
        exact.then(|| BrowserReplayRepairPreviewAcknowledgement {
            repair: state.repair.clone(),
            preview_id: state.preview_id,
            candidate: state.candidate.clone(),
            recipe_locator: state.recipe_locator.clone(),
            token: state.token.clone(),
            lease: self.lease.clone(),
        })
    }
}

pub(crate) struct BrowserReplayRepairHighlightCleanup {
    action: Option<Box<dyn FnOnce() + Send + 'static>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum BrowserReplayRepairPreviewAbortDisposition {
    RestorePrevious,
    ClearExactOnly,
}

impl BrowserReplayRepairHighlightCleanup {
    pub(crate) fn new(action: impl FnOnce() + Send + 'static) -> Self {
        Self {
            action: Some(Box::new(action)),
        }
    }

    pub(super) fn disarm(&mut self) {
        self.action.take();
    }

    #[cfg(test)]
    pub(super) fn for_test(counter: Arc<std::sync::atomic::AtomicUsize>) -> Self {
        Self::new(move || {
            counter.fetch_add(1, Ordering::AcqRel);
        })
    }
}

impl Drop for BrowserReplayRepairHighlightCleanup {
    fn drop(&mut self) {
        if let Some(action) = self.action.take() {
            action();
        }
    }
}

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
