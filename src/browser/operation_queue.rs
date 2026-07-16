use super::{BrowserError, BrowserWorkspaceKey};
use std::collections::{HashMap, VecDeque};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BrowserOperationTarget {
    pub workspace_key: BrowserWorkspaceKey,
    pub tab_id: String,
}

impl BrowserOperationTarget {
    pub fn new(
        workspace_key: BrowserWorkspaceKey,
        tab_id: impl Into<String>,
    ) -> Result<Self, BrowserError> {
        let tab_id = tab_id.into();
        if tab_id.trim().is_empty() {
            return Err(BrowserError::InvalidInvocation {
                field: "tabId".to_string(),
            });
        }
        Ok(Self {
            workspace_key,
            tab_id,
        })
    }
}

#[derive(Debug)]
struct QueuedOperation<T> {
    operation_id: String,
    value: T,
}

#[derive(Debug, PartialEq, Eq)]
pub struct BrowserQueueCancellation<T> {
    pub active_operation_id: Option<String>,
    pub queued: Vec<T>,
}

#[derive(Debug)]
pub struct BrowserOperationQueue<T> {
    active: HashMap<BrowserOperationTarget, String>,
    queued: HashMap<BrowserOperationTarget, VecDeque<QueuedOperation<T>>>,
}

impl<T> Default for BrowserOperationQueue<T> {
    fn default() -> Self {
        Self {
            active: HashMap::new(),
            queued: HashMap::new(),
        }
    }
}

impl<T> BrowserOperationQueue<T> {
    /// Enqueues work for a logical tab. `Some(value)` means the caller owns the
    /// newly active operation and should start it now; `None` means it remains
    /// queued behind that tab's active operation.
    pub fn enqueue(
        &mut self,
        target: BrowserOperationTarget,
        operation_id: impl Into<String>,
        value: T,
    ) -> Option<T> {
        let operation_id = operation_id.into();
        debug_assert!(!operation_id.trim().is_empty());
        if self.active.contains_key(&target) {
            self.queued
                .entry(target)
                .or_default()
                .push_back(QueuedOperation {
                    operation_id,
                    value,
                });
            None
        } else {
            self.active.insert(target, operation_id);
            Some(value)
        }
    }

    pub fn active_operation_id(&self, target: &BrowserOperationTarget) -> Option<&str> {
        self.active.get(target).map(String::as_str)
    }

    /// Completes only the currently active operation. A callback carrying an
    /// old/cancelled operation id is ignored and cannot advance the queue.
    pub fn complete(&mut self, target: &BrowserOperationTarget, operation_id: &str) -> Option<T> {
        if self.active_operation_id(target) != Some(operation_id) {
            return None;
        }
        self.active.remove(target);
        let next = self.queued.get_mut(target).and_then(VecDeque::pop_front);
        if self.queued.get(target).is_some_and(VecDeque::is_empty) {
            self.queued.remove(target);
        }
        if let Some(next) = next {
            self.active
                .insert(target.clone(), next.operation_id.clone());
            Some(next.value)
        } else {
            None
        }
    }

    pub fn cancel_tab(&mut self, target: &BrowserOperationTarget) -> BrowserQueueCancellation<T> {
        let active_operation_id = self.active.remove(target);
        let queued = self
            .queued
            .remove(target)
            .unwrap_or_default()
            .into_iter()
            .map(|operation| operation.value)
            .collect();
        BrowserQueueCancellation {
            active_operation_id,
            queued,
        }
    }

    pub fn cancel_workspace(
        &mut self,
        workspace_key: &BrowserWorkspaceKey,
    ) -> Vec<(BrowserOperationTarget, BrowserQueueCancellation<T>)> {
        let mut targets: Vec<_> = self
            .active
            .keys()
            .chain(self.queued.keys())
            .filter(|target| &target.workspace_key == workspace_key)
            .cloned()
            .collect();
        targets.sort_by(|left, right| left.tab_id.cmp(&right.tab_id));
        targets.dedup();
        targets
            .into_iter()
            .map(|target| {
                let cancellation = self.cancel_tab(&target);
                (target, cancellation)
            })
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.active.is_empty() && self.queued.is_empty()
    }
}
