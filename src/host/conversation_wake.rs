//! Event-driven semantic conversation subscription wake path.
//!
//! Producer records once into the semantic journal, then marks a bounded dirty
//! board (newest high-water per *watched* session). The host executor fans
//! dirtiness to active subscriptions as coalesced ephemeral `ConversationDirty`
//! notices. Question ingress stays on its existing unbounded channel and must
//! never grow a per-token DomainEvent or wake queue.
//!
//! Watched session keys are refcounted by the subscription registry: open tracks
//! before capture, release/detach/rebind untracks the last subscriber. Unwatched
//! producer marks are dropped — they cannot evict or suppress a subscribed task.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::watch;
use uuid::Uuid;

use crate::domain::id::{SubscriptionId, TaskId};
use crate::domain::ClientId;
use crate::kernel::SessionScope;
use crate::remote::presentation::StableSessionKey;

/// Global bound on live conversation subscriptions (executor-owned registry).
/// Sized above browser command-channel watch (64) so one host can serve many
/// concurrent conversation surfaces without a tighter artificial cap.
pub(crate) const MAX_CONVERSATION_SUBSCRIPTIONS: usize = 256;
/// Bound on subscriptions retained for one physical connection output.
/// Matches the browser watch-channel depth (64) so one duplex can mirror that
/// many concurrent conversation subscriptions without per-token maps.
pub(crate) const MAX_CONVERSATION_SUBSCRIPTIONS_PER_OUTPUT: usize = 64;

/// Cross-thread producer → executor dirty edge. Only actively watched session
/// keys retain dirtiness; newest high-water wins per key. Watch edge never
/// loses a wake between mark and drain.
#[derive(Debug)]
pub(crate) struct SemanticDirtyBoard {
    inner: Mutex<DirtyInner>,
    edge: watch::Sender<u64>,
}

#[derive(Debug, Default)]
struct DirtyInner {
    /// Refcount of live subscriptions watching each session key.
    watched: HashMap<StableSessionKey, usize>,
    /// Newest high-water for watched keys only. Bounded by `watched.len()`.
    dirty: HashMap<StableSessionKey, u64>,
}

impl SemanticDirtyBoard {
    pub(crate) fn new() -> (Arc<Self>, watch::Receiver<u64>) {
        let (edge, rx) = watch::channel(0u64);
        (
            Arc::new(Self {
                inner: Mutex::new(DirtyInner::default()),
                edge,
            }),
            rx,
        )
    }

    /// Begin watching `key` before initial page capture. Refcounted.
    pub(crate) fn track(&self, key: StableSessionKey) {
        let mut inner = self.inner.lock().expect("semantic dirty board");
        *inner.watched.entry(key).or_insert(0) += 1;
    }

    /// Drop one watch ref. Clears dirty when the last subscriber leaves.
    pub(crate) fn untrack(&self, key: &StableSessionKey) {
        let mut inner = self.inner.lock().expect("semantic dirty board");
        let Some(count) = inner.watched.get_mut(key) else {
            return;
        };
        *count = count.saturating_sub(1);
        if *count == 0 {
            inner.watched.remove(key);
            inner.dirty.remove(key);
        }
    }

    pub(crate) fn is_watched(&self, key: &StableSessionKey) -> bool {
        self.inner
            .lock()
            .expect("semantic dirty board")
            .watched
            .get(key)
            .is_some_and(|count| *count > 0)
    }

    /// Record the newest observed sequence for a *watched* key and advance the
    /// watch edge. Unwatched marks are dropped so orphan traffic cannot evict
    /// or suppress an active subscription's notice.
    pub(crate) fn mark(&self, key: StableSessionKey, sequence: u64) {
        let woke = {
            let mut inner = self.inner.lock().expect("semantic dirty board");
            if !inner.watched.get(&key).is_some_and(|count| *count > 0) {
                return;
            }
            let entry = inner.dirty.entry(key).or_insert(0);
            let previous = *entry;
            *entry = (*entry).max(sequence);
            *entry != previous || previous == 0
        };
        if woke {
            self.edge.send_modify(|generation| {
                *generation = generation.wrapping_add(1);
            });
        }
    }

    pub(crate) fn high_water(&self, key: &StableSessionKey) -> Option<u64> {
        self.inner
            .lock()
            .expect("semantic dirty board")
            .dirty
            .get(key)
            .copied()
    }

    /// Snapshot dirties for watched keys without clearing. Caller clears with
    /// [`Self::clear_if_at_most`] only after successful delivery.
    pub(crate) fn snapshot_watched(&self) -> Vec<(StableSessionKey, u64)> {
        let inner = self.inner.lock().expect("semantic dirty board");
        inner
            .dirty
            .iter()
            .filter(|(key, _)| inner.watched.get(*key).is_some_and(|count| *count > 0))
            .map(|(key, high_water)| (key.clone(), *high_water))
            .collect()
    }

    /// Clear `key` only when the retained high-water is still `<= expected`.
    pub(crate) fn clear_if_at_most(&self, key: &StableSessionKey, expected: u64) {
        let mut inner = self.inner.lock().expect("semantic dirty board");
        match inner.dirty.get(key).copied() {
            Some(current) if current <= expected => {
                inner.dirty.remove(key);
            }
            _ => {}
        }
    }

    /// Peek without clearing — used by open-subscription journal catch-up.
    pub(crate) fn peek(&self, key: &StableSessionKey) -> Option<u64> {
        self.high_water(key)
    }

    #[cfg(test)]
    pub(crate) fn dirty_len(&self) -> usize {
        self.inner.lock().expect("semantic dirty board").dirty.len()
    }

    #[cfg(test)]
    pub(crate) fn watched_len(&self) -> usize {
        self.inner
            .lock()
            .expect("semantic dirty board")
            .watched
            .len()
    }
}

/// One executor-owned conversation subscription. Ephemeral: never rebound on
/// reconnect; detach/rebind/release remove the entry.
#[derive(Debug)]
pub(crate) struct ConversationSubscriptionEntry {
    pub(crate) owner: ClientId,
    pub(crate) task_id: TaskId,
    pub(crate) session_key: StableSessionKey,
    pub(crate) connection_id: Uuid,
    pub(crate) scope: SessionScope,
    /// Bumped on release/detach so queued ephemeral materializers go inert.
    pub(crate) generation: Arc<AtomicU64>,
    pub(crate) last_notified_high_water: u64,
}

impl ConversationSubscriptionEntry {
    pub(crate) fn current_generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    pub(crate) fn invalidate(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
    }
}

#[derive(Debug, Default)]
pub(crate) struct ConversationSubscriptionRegistry {
    entries: HashMap<SubscriptionId, ConversationSubscriptionEntry>,
    by_session: HashMap<StableSessionKey, HashSet<SubscriptionId>>,
    per_output: HashMap<Uuid, usize>,
}

impl ConversationSubscriptionRegistry {
    pub(crate) fn new() -> Self {
        Self {
            entries: HashMap::with_capacity(MAX_CONVERSATION_SUBSCRIPTIONS),
            by_session: HashMap::new(),
            per_output: HashMap::new(),
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn has_session(&self, key: &StableSessionKey) -> bool {
        self.by_session.get(key).is_some_and(|ids| !ids.is_empty())
    }

    pub(crate) fn subscriptions_for(
        &self,
        key: &StableSessionKey,
    ) -> Vec<(SubscriptionId, TaskId, u64, Arc<AtomicU64>, Uuid, u64)> {
        let Some(ids) = self.by_session.get(key) else {
            return Vec::new();
        };
        ids.iter()
            .filter_map(|id| {
                let entry = self.entries.get(id)?;
                Some((
                    *id,
                    entry.task_id,
                    entry.current_generation(),
                    Arc::clone(&entry.generation),
                    entry.connection_id,
                    entry.last_notified_high_water,
                ))
            })
            .collect()
    }

    pub(crate) fn prepare_insert(&self, connection_id: Uuid) -> bool {
        if self.entries.len() >= MAX_CONVERSATION_SUBSCRIPTIONS {
            return false;
        }
        let per = self.per_output.get(&connection_id).copied().unwrap_or(0);
        per < MAX_CONVERSATION_SUBSCRIPTIONS_PER_OUTPUT
    }

    pub(crate) fn insert(
        &mut self,
        owner: ClientId,
        task_id: TaskId,
        session_key: StableSessionKey,
        connection_id: Uuid,
        scope: SessionScope,
        baseline_high_water: u64,
    ) -> Result<SubscriptionId, ()> {
        if !self.prepare_insert(connection_id) {
            return Err(());
        }
        let subscription_id = SubscriptionId::new();
        let entry = ConversationSubscriptionEntry {
            owner,
            task_id,
            session_key: session_key.clone(),
            connection_id,
            scope,
            generation: Arc::new(AtomicU64::new(1)),
            last_notified_high_water: baseline_high_water,
        };
        self.entries.insert(subscription_id, entry);
        self.by_session
            .entry(session_key)
            .or_default()
            .insert(subscription_id);
        *self.per_output.entry(connection_id).or_insert(0) += 1;
        Ok(subscription_id)
    }

    pub(crate) fn note_notified(&mut self, subscription_id: SubscriptionId, high_water: u64) {
        if let Some(entry) = self.entries.get_mut(&subscription_id) {
            entry.last_notified_high_water = entry.last_notified_high_water.max(high_water);
        }
    }

    pub(crate) fn get(
        &self,
        subscription_id: SubscriptionId,
    ) -> Option<&ConversationSubscriptionEntry> {
        self.entries.get(&subscription_id)
    }

    /// Validate owner/output/task scope, remove, and invalidate generation.
    pub(crate) fn release(
        &mut self,
        subscription_id: SubscriptionId,
        owner: ClientId,
        scope: SessionScope,
    ) -> Result<ConversationSubscriptionEntry, ReleaseError> {
        let Some(entry) = self.entries.get(&subscription_id) else {
            return Err(ReleaseError::NotFound);
        };
        if entry.owner != owner || entry.scope != scope {
            return Err(ReleaseError::Unauthorized);
        }
        Ok(self.remove(subscription_id).expect("present"))
    }

    pub(crate) fn remove(
        &mut self,
        subscription_id: SubscriptionId,
    ) -> Option<ConversationSubscriptionEntry> {
        let entry = self.entries.remove(&subscription_id)?;
        entry.invalidate();
        if let Some(set) = self.by_session.get_mut(&entry.session_key) {
            set.remove(&subscription_id);
            if set.is_empty() {
                self.by_session.remove(&entry.session_key);
            }
        }
        if let Some(count) = self.per_output.get_mut(&entry.connection_id) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.per_output.remove(&entry.connection_id);
            }
        }
        Some(entry)
    }

    /// Remove every subscription bound to a physical output (detach / rebind /
    /// release_output). Never rebinds; reconnect must open a fresh subscription.
    pub(crate) fn remove_for_output(
        &mut self,
        connection_id: Uuid,
    ) -> Vec<(SubscriptionId, ConversationSubscriptionEntry)> {
        let ids: Vec<_> = self
            .entries
            .iter()
            .filter(|(_, entry)| entry.connection_id == connection_id)
            .map(|(id, _)| *id)
            .collect();
        ids.into_iter()
            .filter_map(|id| self.remove(id).map(|entry| (id, entry)))
            .collect()
    }

    pub(crate) fn session_key_for_task(task_id: TaskId) -> StableSessionKey {
        StableSessionKey::from_tab(task_id.to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReleaseError {
    NotFound,
    Unauthorized,
}

/// Catch-up high-water after initial page capture using journal metadata (and
/// any watched board mark retained during the tracked capture window).
pub(crate) fn race_catch_up_high_water(
    page_high_water: u64,
    board_high_water: Option<u64>,
    journal_high_water: Option<u64>,
) -> u64 {
    page_high_water
        .max(board_high_water.unwrap_or(0))
        .max(journal_high_water.unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::id::TaskId;
    use crate::domain::ClientId;
    use crate::kernel::SessionScope;
    use uuid::Uuid;

    fn scope(client: ClientId, task: TaskId, connection: Uuid) -> SessionScope {
        SessionScope {
            client_id: Some(client),
            task_id: Some(task),
            connection_id: Some(connection),
            action_epoch: None,
            runtime_generation: None,
        }
    }

    #[test]
    fn dirty_board_coalesces_watched_and_drops_unwatched() {
        let (board, mut rx) = SemanticDirtyBoard::new();
        let key = StableSessionKey::from_tab("task-a");
        let _ = rx.borrow_and_update();

        board.mark(key.clone(), 99);
        assert_eq!(board.dirty_len(), 0, "unwatched marks must drop");
        assert!(!rx.has_changed().unwrap());

        board.track(key.clone());
        for sequence in 1..=200u64 {
            board.mark(key.clone(), sequence);
        }
        assert_eq!(board.high_water(&key), Some(200));
        assert_eq!(board.dirty_len(), 1);
        assert!(rx.has_changed().unwrap());

        board.untrack(&key);
        assert_eq!(board.dirty_len(), 0);
        assert_eq!(board.watched_len(), 0);
    }

    #[test]
    fn orphan_marks_beyond_capacity_cannot_suppress_subscribed_final_notice() {
        let (board, _) = SemanticDirtyBoard::new();
        let subscribed = StableSessionKey::from_tab("subscribed-task");
        board.track(subscribed.clone());
        board.mark(subscribed.clone(), 7);

        // Flood with distinct unwatched session keys — previously this could
        // evict the subscribed high-water by comparing unrelated sequences.
        for index in 0..(MAX_CONVERSATION_SUBSCRIPTIONS * 2) {
            let orphan = StableSessionKey::from_tab(format!("orphan-{index}"));
            board.mark(orphan, u64::MAX.saturating_sub(index as u64));
        }

        assert_eq!(
            board.high_water(&subscribed),
            Some(7),
            "subscribed dirty must survive unbounded orphan mark attempts"
        );
        board.mark(subscribed.clone(), 11);
        assert_eq!(board.high_water(&subscribed), Some(11));
        assert_eq!(board.dirty_len(), 1);
        assert_eq!(board.watched_len(), 1);
    }

    #[test]
    fn race_catch_up_takes_max_of_page_board_and_journal() {
        assert_eq!(race_catch_up_high_water(10, Some(12), Some(11)), 12);
        assert_eq!(race_catch_up_high_water(10, None, Some(15)), 15);
        assert_eq!(race_catch_up_high_water(10, Some(9), None), 10);
    }

    #[test]
    fn registry_release_validates_owner_and_scope_and_invalidates_generation() {
        let mut registry = ConversationSubscriptionRegistry::new();
        let owner = ClientId::new();
        let foreign = ClientId::new();
        let task = TaskId::new();
        let connection = Uuid::now_v7();
        let session_key = ConversationSubscriptionRegistry::session_key_for_task(task);
        let owned_scope = scope(owner, task, connection);
        let id = registry
            .insert(owner, task, session_key, connection, owned_scope, 5)
            .expect("insert");
        let entry = registry.get(id).expect("entry");
        let generation = Arc::clone(&entry.generation);
        let before = generation.load(Ordering::SeqCst);

        assert!(matches!(
            registry.release(id, foreign, owned_scope),
            Err(ReleaseError::Unauthorized)
        ));
        assert!(matches!(
            registry.release(id, owner, scope(owner, task, Uuid::now_v7())),
            Err(ReleaseError::Unauthorized)
        ));

        let released = registry.release(id, owner, owned_scope).expect("release");
        assert_eq!(released.task_id, task);
        assert!(generation.load(Ordering::SeqCst) > before);
        assert!(registry.get(id).is_none());
        assert!(matches!(
            registry.release(id, owner, owned_scope),
            Err(ReleaseError::NotFound)
        ));
    }

    #[test]
    fn registry_fanout_retains_the_registered_task_id() {
        let mut registry = ConversationSubscriptionRegistry::new();
        let owner = ClientId::new();
        let task = TaskId::new();
        let connection = Uuid::now_v7();
        let key = ConversationSubscriptionRegistry::session_key_for_task(task);
        let id = registry
            .insert(
                owner,
                task,
                key.clone(),
                connection,
                scope(owner, task, connection),
                3,
            )
            .expect("insert");
        let targets = registry.subscriptions_for(&key);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].0, id);
        assert_eq!(targets[0].1, task);
    }

    #[test]
    fn remove_for_output_drops_all_bound_subscriptions_without_rebind() {
        let mut registry = ConversationSubscriptionRegistry::new();
        let owner = ClientId::new();
        let task = TaskId::new();
        let connection = Uuid::now_v7();
        let other = Uuid::now_v7();
        let session_key = ConversationSubscriptionRegistry::session_key_for_task(task);
        let a = registry
            .insert(
                owner,
                task,
                session_key.clone(),
                connection,
                scope(owner, task, connection),
                1,
            )
            .unwrap();
        let b = registry
            .insert(
                owner,
                task,
                session_key,
                other,
                scope(owner, task, other),
                1,
            )
            .unwrap();
        let removed = registry.remove_for_output(connection);
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].0, a);
        assert!(registry.get(a).is_none());
        assert!(registry.get(b).is_some());
    }

    #[test]
    fn registry_enforces_global_and_per_output_caps() {
        let mut registry = ConversationSubscriptionRegistry::new();
        let owner = ClientId::new();
        let connection = Uuid::now_v7();
        for _ in 0..MAX_CONVERSATION_SUBSCRIPTIONS_PER_OUTPUT {
            let task = TaskId::new();
            assert!(registry
                .insert(
                    owner,
                    task,
                    ConversationSubscriptionRegistry::session_key_for_task(task),
                    connection,
                    scope(owner, task, connection),
                    0,
                )
                .is_ok());
        }
        let overflow_task = TaskId::new();
        assert!(registry
            .insert(
                owner,
                overflow_task,
                ConversationSubscriptionRegistry::session_key_for_task(overflow_task),
                connection,
                scope(owner, overflow_task, connection),
                0,
            )
            .is_err());
        assert_eq!(
            MAX_CONVERSATION_SUBSCRIPTIONS_PER_OUTPUT, 64,
            "per-output must cover browser watch depth"
        );
        assert_eq!(MAX_CONVERSATION_SUBSCRIPTIONS, 256);
    }
}
