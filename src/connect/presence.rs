//! Ephemeral presence hints. They are UX metadata, not authority.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::domain::id::{ClientId, TaskId};

use super::epoch::{FocusEpoch, TurnEpoch};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LastSenderHint {
    pub task_id: TaskId,
    pub client_id: ClientId,
    pub observed_at_ms: i64,
    pub turn_epoch: TurnEpoch,
    pub focus_epoch: FocusEpoch,
}

impl LastSenderHint {
    pub const fn new(
        task_id: TaskId,
        client_id: ClientId,
        observed_at_ms: i64,
        turn_epoch: TurnEpoch,
        focus_epoch: FocusEpoch,
    ) -> Self {
        Self {
            task_id,
            client_id,
            observed_at_ms,
            turn_epoch,
            focus_epoch,
        }
    }
}

/// A sink for transient last-sender UI hints.
pub trait PresenceSink {
    /// Returns false when an older observation was ignored or the bounded sink
    /// could not retain a new task entry.
    fn record(&mut self, hint: LastSenderHint) -> bool;
}

/// In-memory, bounded, non-durable presence state.
#[derive(Debug, Clone)]
pub struct EphemeralPresence {
    max_tasks: usize,
    hints: BTreeMap<TaskId, LastSenderHint>,
}

impl EphemeralPresence {
    pub fn new(max_tasks: usize) -> Self {
        Self {
            max_tasks,
            hints: BTreeMap::new(),
        }
    }

    pub fn last_sender(&self, task_id: TaskId) -> Option<LastSenderHint> {
        self.hints.get(&task_id).copied()
    }

    pub fn clear_task(&mut self, task_id: TaskId) {
        self.hints.remove(&task_id);
    }

    pub fn clear(&mut self) {
        self.hints.clear();
    }

    pub fn len(&self) -> usize {
        self.hints.len()
    }

    pub fn is_empty(&self) -> bool {
        self.hints.is_empty()
    }
}

impl Default for EphemeralPresence {
    fn default() -> Self {
        Self::new(128)
    }
}

impl PresenceSink for EphemeralPresence {
    fn record(&mut self, hint: LastSenderHint) -> bool {
        if self.max_tasks == 0 {
            return false;
        }
        if let Some(previous) = self.hints.get(&hint.task_id) {
            if hint.observed_at_ms < previous.observed_at_ms {
                return false;
            }
            self.hints.insert(hint.task_id, hint);
            return true;
        }

        if self.hints.len() >= self.max_tasks {
            let Some((&victim, _)) = self
                .hints
                .iter()
                .min_by_key(|(task_id, value)| (value.observed_at_ms, **task_id))
            else {
                return false;
            };
            self.hints.remove(&victim);
        }
        self.hints.insert(hint.task_id, hint);
        true
    }
}
