//! Bounded, deterministic task-list projection and local virtual viewport.

use std::ops::Range;

use crate::client::ClientModel;
use crate::domain::id::TaskId;
use crate::domain::task::TaskLifecycle;

pub const MAX_TASK_LIST_ITEMS: usize = 5_000;
pub const FIXED_VIRTUAL_OVERSCAN: usize = 32;
pub const DEFAULT_VISIBLE_ROWS: usize = 40;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TaskListOverflow {
    pub limit: usize,
    pub total_count: usize,
    pub retained_count: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ViewportError {
    ZeroVisibleRows,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VirtualWindow {
    visible_start: usize,
    visible_end: usize,
    overscan: usize,
}

impl VirtualWindow {
    fn for_item_count(visible_start: usize, visible_count: usize, item_count: usize) -> Self {
        let visible_start = visible_start.min(item_count);
        let visible_end = visible_start.saturating_add(visible_count).min(item_count);
        Self {
            visible_start,
            visible_end,
            overscan: FIXED_VIRTUAL_OVERSCAN,
        }
    }

    pub fn visible_range(&self) -> Range<usize> {
        self.visible_start..self.visible_end
    }

    pub fn overscan(&self) -> usize {
        self.overscan
    }

    pub fn render_range(&self, item_count: usize) -> Range<usize> {
        let visible = self.visible_range();
        let visible_start = visible.start.min(item_count);
        let visible_end = visible.end.min(item_count).max(visible_start);
        visible_start.saturating_sub(self.overscan)
            ..visible_end.saturating_add(self.overscan).min(item_count)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskList {
    task_ids: Vec<TaskId>,
    viewport: VirtualWindow,
    overflow: Option<TaskListOverflow>,
}

impl TaskList {
    /// Construct the empty projection used by the isolated native shell until
    /// its explicitly scoped dev/test host supplies a snapshot.
    pub fn empty() -> Self {
        Self {
            task_ids: Vec::new(),
            viewport: VirtualWindow::for_item_count(0, DEFAULT_VISIBLE_ROWS, 0),
            overflow: None,
        }
    }

    /// Project only the ordered task identities from one immutable client
    /// model. Snapshot facts, host status, and subscriptions are not retained.
    pub fn from_model(model: &ClientModel) -> Self {
        let total_count = model
            .tasks()
            .values()
            .filter(|task| task.task.lifecycle != TaskLifecycle::Archived)
            .count();
        let task_ids: Vec<TaskId> = model
            .tasks()
            .iter()
            .filter(|(_, task)| task.task.lifecycle != TaskLifecycle::Archived)
            .map(|(task_id, _)| *task_id)
            .take(MAX_TASK_LIST_ITEMS)
            .collect();
        let overflow = (total_count > MAX_TASK_LIST_ITEMS).then_some(TaskListOverflow {
            limit: MAX_TASK_LIST_ITEMS,
            total_count,
            retained_count: task_ids.len(),
        });
        let viewport = VirtualWindow::for_item_count(0, DEFAULT_VISIBLE_ROWS, task_ids.len());
        Self {
            task_ids,
            viewport,
            overflow,
        }
    }

    pub fn task_ids(&self) -> &[TaskId] {
        &self.task_ids
    }

    pub fn len(&self) -> usize {
        self.task_ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.task_ids.is_empty()
    }

    pub fn overflow(&self) -> Option<TaskListOverflow> {
        self.overflow
    }

    pub fn virtual_window(&self) -> VirtualWindow {
        self.viewport
    }

    pub fn set_viewport(
        &mut self,
        first_visible: usize,
        visible_rows: usize,
    ) -> Result<(), ViewportError> {
        if visible_rows == 0 {
            return Err(ViewportError::ZeroVisibleRows);
        }
        self.viewport = VirtualWindow::for_item_count(first_visible, visible_rows, self.len());
        Ok(())
    }

    pub fn visible_task_ids(&self) -> &[TaskId] {
        let range = self.viewport.visible_range();
        &self.task_ids[range]
    }

    pub fn rendered_task_ids(&self) -> &[TaskId] {
        let range = self.viewport.render_range(self.len());
        &self.task_ids[range]
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Inbox {
    task_list: TaskList,
}

impl Inbox {
    pub fn from_model(model: &ClientModel) -> Self {
        Self {
            task_list: TaskList::from_model(model),
        }
    }

    pub fn task_list(&self) -> &TaskList {
        &self.task_list
    }

    pub fn task_list_mut(&mut self) -> &mut TaskList {
        &mut self.task_list
    }
}
