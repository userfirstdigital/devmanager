//! Bounded, deterministic task-list projection and local virtual viewport.

use std::ops::Range;
use std::sync::Arc;

use crate::client::ClientModel;
use crate::domain::id::TaskId;
use crate::domain::task::TaskLifecycle;

pub const MAX_TASK_LIST_ITEMS: usize = 5_000;
pub const FIXED_VIRTUAL_OVERSCAN: usize = 32;
pub const DEFAULT_VISIBLE_ROWS: usize = 40;
pub const MAX_VIRTUAL_SOURCE_ROWS: usize = 100_000;

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

/// A uniform-row viewport over a potentially much larger source than the
/// bounded host projection. It stores only counts and offsets; row identities
/// are stable keys generated on demand by the GPUI list closure.
#[derive(Clone, Debug, PartialEq)]
pub struct VirtualListViewport {
    total_rows: usize,
    visible_rows: usize,
    scroll_offset: f32,
    window: VirtualWindow,
}

impl VirtualListViewport {
    pub fn new(total_rows: usize, visible_rows: usize) -> Result<Self, ViewportError> {
        if visible_rows == 0 {
            return Err(ViewportError::ZeroVisibleRows);
        }
        let total_rows = total_rows.min(MAX_VIRTUAL_SOURCE_ROWS);
        Ok(Self {
            total_rows,
            visible_rows,
            scroll_offset: 0.0,
            window: VirtualWindow::for_item_count(0, visible_rows, total_rows),
        })
    }

    pub fn total_rows(&self) -> usize {
        self.total_rows
    }

    pub fn materialized_rows(&self) -> usize {
        0
    }

    pub fn visible_range(&self) -> Range<usize> {
        self.window.visible_range()
    }

    pub fn render_range(&self) -> Range<usize> {
        self.window.render_range(self.total_rows)
    }

    pub fn stable_key(&self, index: usize) -> String {
        format!("task-row-{index:08}")
    }

    pub fn scroll_offset(&self) -> f32 {
        self.scroll_offset
    }

    /// Apply a GPUI scroll-wheel pixel delta to the uniform viewport. The
    /// caller supplies the measured viewport and row dimensions, so this
    /// method is deterministic in headless tests as well as in a real window.
    pub fn apply_scroll_delta(
        &mut self,
        delta_pixels: f32,
        viewport_height: f32,
        row_height: f32,
    ) -> Result<(), ViewportError> {
        if viewport_height <= 0.0 || row_height <= 0.0 {
            return Err(ViewportError::ZeroVisibleRows);
        }
        let visible_rows = (viewport_height / row_height).ceil().max(1.0) as usize;
        self.visible_rows = visible_rows;
        let max_offset = self
            .total_rows
            .saturating_sub(visible_rows)
            .saturating_mul(row_height as usize) as f32;
        self.scroll_offset = (self.scroll_offset + delta_pixels).clamp(0.0, max_offset);
        let first_visible = (self.scroll_offset / row_height).floor() as usize;
        self.window = VirtualWindow::for_item_count(first_visible, visible_rows, self.total_rows);
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskList {
    task_ids: Arc<Vec<TaskId>>,
    viewport: VirtualWindow,
    overflow: Option<TaskListOverflow>,
    virtual_source: bool,
}

impl TaskList {
    /// Construct the empty projection used by the isolated native shell until
    /// its explicitly scoped dev/test host supplies a snapshot.
    pub fn empty() -> Self {
        Self {
            task_ids: Arc::new(Vec::new()),
            viewport: VirtualWindow::for_item_count(0, DEFAULT_VISIBLE_ROWS, 0),
            overflow: None,
            virtual_source: false,
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
            task_ids: Arc::new(task_ids),
            viewport,
            overflow,
            virtual_source: false,
        }
    }

    /// Construct the source consumed by the real GPUI uniform list. The
    /// source is bounded at the host contract's 100k row limit, while the
    /// uniform list only asks for its visible range during layout.
    pub fn from_virtual_task_ids(task_ids: Vec<TaskId>) -> Result<Self, TaskListOverflow> {
        if task_ids.len() > MAX_VIRTUAL_SOURCE_ROWS {
            return Err(TaskListOverflow {
                limit: MAX_VIRTUAL_SOURCE_ROWS,
                total_count: task_ids.len(),
                retained_count: MAX_VIRTUAL_SOURCE_ROWS,
            });
        }
        let total_count = task_ids.len();
        Ok(Self {
            task_ids: Arc::new(task_ids),
            viewport: VirtualWindow::for_item_count(0, DEFAULT_VISIBLE_ROWS, total_count),
            overflow: None,
            virtual_source: true,
        })
    }

    /// Build the full bounded source used by the native uniform list when a
    /// live client model is available. The legacy `from_model` projection
    /// keeps its smaller compatibility bound; this explicit path is the
    /// canonical native shell path and never materializes row elements.
    pub fn from_client_model_virtual(model: &ClientModel) -> Result<Self, TaskListOverflow> {
        let task_ids = model
            .tasks()
            .iter()
            .filter(|(_, task)| task.task.lifecycle != TaskLifecycle::Archived)
            .map(|(task_id, _)| *task_id)
            .collect();
        Self::from_virtual_task_ids(task_ids)
    }

    pub fn task_ids(&self) -> &[TaskId] {
        self.task_ids.as_slice()
    }

    pub(crate) fn shared_task_ids(&self) -> Arc<Vec<TaskId>> {
        Arc::clone(&self.task_ids)
    }

    pub fn len(&self) -> usize {
        self.task_ids.len()
    }

    pub fn total_count(&self) -> usize {
        self.task_ids.len()
    }

    pub fn stable_key_for(&self, index: usize) -> String {
        self.task_ids
            .get(index)
            .map(|task_id| format!("native-task-row-{task_id}"))
            .unwrap_or_else(|| format!("native-task-row-missing-{index}"))
    }

    pub fn uses_gpui_uniform_list(&self) -> bool {
        self.virtual_source
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

    /// Apply a pixel scroll delta to the bounded task projection. GPUI's
    /// scroll callback calls this without constructing a second list model.
    pub fn apply_scroll_delta(
        &mut self,
        delta_pixels: f32,
        viewport_height: f32,
        row_height: f32,
    ) -> Result<(), ViewportError> {
        if viewport_height <= 0.0 || row_height <= 0.0 {
            return Err(ViewportError::ZeroVisibleRows);
        }
        let visible_rows = (viewport_height / row_height).ceil().max(1.0) as usize;
        let current_start = self.viewport.visible_range().start;
        let current_offset = current_start as f32 * row_height;
        let max_offset = self
            .len()
            .saturating_sub(visible_rows)
            .saturating_mul(row_height as usize) as f32;
        let offset = (current_offset + delta_pixels).clamp(0.0, max_offset);
        self.set_viewport((offset / row_height).floor() as usize, visible_rows)
    }

    pub fn visible_task_ids(&self) -> &[TaskId] {
        let range = self.viewport.visible_range();
        &self.task_ids[range]
    }

    pub fn rendered_task_ids(&self) -> &[TaskId] {
        let range = self.viewport.render_range(self.len());
        &self.task_ids[range]
    }

    pub fn set_scroll_offset_pixels(
        &mut self,
        offset_pixels: f32,
        viewport_height: f32,
        row_height: f32,
    ) -> Result<(), ViewportError> {
        if viewport_height <= 0.0 || row_height <= 0.0 {
            return Err(ViewportError::ZeroVisibleRows);
        }
        let visible_rows = (viewport_height / row_height).ceil().max(1.0) as usize;
        let max_offset = self
            .len()
            .saturating_sub(visible_rows)
            .saturating_mul(row_height as usize) as f32;
        let offset = offset_pixels.clamp(0.0, max_offset);
        self.set_viewport((offset / row_height).floor() as usize, visible_rows)
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
