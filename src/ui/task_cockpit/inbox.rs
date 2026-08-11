//! Bounded, deterministic task-list projection and local virtual viewport.

use std::collections::HashSet;
use std::ops::Range;
use std::sync::Arc;

use crate::client::ClientModel;
use crate::domain::id::TaskId;
use crate::domain::task::TaskLifecycle;

pub const FIXED_VIRTUAL_OVERSCAN: usize = 32;
pub const DEFAULT_VISIBLE_ROWS: usize = 40;
pub const MAX_VIRTUAL_WINDOW_ROWS: usize = 128;
/// Maximum identity-bearing task source retained by the native uniform list.
/// Validate this bound before allocating duplicate-detection state.
pub const MAX_TASK_SOURCE_IDS: usize = 100_000;

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

    /// Project all ordered task identities from one immutable client model.
    /// Snapshot facts, host status, and subscriptions are not retained here;
    /// the model owner remains the source of truth and the GPUI viewport only
    /// materializes its bounded visible range.
    pub fn from_model(model: &ClientModel) -> Self {
        let mut task_ids = Vec::with_capacity(model.tasks().len().min(MAX_TASK_SOURCE_IDS));
        for (task_id, _task) in model
            .tasks()
            .iter()
            .filter(|(_, task)| task.task.lifecycle != TaskLifecycle::Archived)
        {
            if task_ids.len() == MAX_TASK_SOURCE_IDS {
                return Self::overflowed(MAX_TASK_SOURCE_IDS.saturating_add(1), false);
            }
            task_ids.push(*task_id);
        }
        let viewport = VirtualWindow::for_item_count(0, DEFAULT_VISIBLE_ROWS, task_ids.len());
        Self {
            task_ids: Arc::new(task_ids),
            viewport,
            overflow: None,
            virtual_source: false,
        }
    }

    /// Construct the full source consumed by the real GPUI uniform list. The
    /// source keeps identity-bearing task IDs; only the uniform-list closure
    /// creates row elements for its current viewport.
    pub fn from_virtual_task_ids(task_ids: Vec<TaskId>) -> Result<Self, TaskListOverflow> {
        let total_count = task_ids.len();
        // Reject over-cap sources before allocating any duplicate-detection
        // state. The caller-provided Vec is already owned by this API; no
        // second unbounded collection is permitted here.
        if total_count > MAX_TASK_SOURCE_IDS {
            return Err(TaskListOverflow {
                limit: MAX_TASK_SOURCE_IDS,
                total_count,
                retained_count: 0,
            });
        }
        let mut seen = HashSet::new();
        if task_ids.iter().any(|task_id| !seen.insert(*task_id)) {
            return Err(TaskListOverflow {
                // A duplicate source cannot be safely truncated or repaired:
                // stable row identity would become ambiguous.  Keep the
                // existing overflow error seam and report that no source was
                // retained.
                limit: usize::MAX,
                total_count,
                retained_count: 0,
            });
        }
        Ok(Self {
            task_ids: Arc::new(task_ids),
            viewport: VirtualWindow::for_item_count(0, DEFAULT_VISIBLE_ROWS, total_count),
            overflow: None,
            virtual_source: true,
        })
    }

    /// Build the full source used by the native uniform list when a live
    /// immutable client model is available. This path never materializes row
    /// elements outside the current viewport.
    pub fn from_client_model_virtual(model: &ClientModel) -> Result<Self, TaskListOverflow> {
        let mut task_ids = Vec::with_capacity(model.tasks().len().min(MAX_TASK_SOURCE_IDS));
        for (task_id, _task) in model
            .tasks()
            .iter()
            .filter(|(_, task)| task.task.lifecycle != TaskLifecycle::Archived)
        {
            if task_ids.len() == MAX_TASK_SOURCE_IDS {
                return Err(TaskListOverflow {
                    limit: MAX_TASK_SOURCE_IDS,
                    total_count: MAX_TASK_SOURCE_IDS.saturating_add(1),
                    retained_count: 0,
                });
            }
            task_ids.push(*task_id);
        }
        Self::from_virtual_task_ids(task_ids)
    }

    fn overflowed(total_count: usize, virtual_source: bool) -> Self {
        Self {
            task_ids: Arc::new(Vec::new()),
            viewport: VirtualWindow::for_item_count(0, DEFAULT_VISIBLE_ROWS, 0),
            overflow: Some(TaskListOverflow {
                limit: MAX_TASK_SOURCE_IDS,
                total_count,
                retained_count: 0,
            }),
            virtual_source,
        }
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

    /// Return a bounded keyset page after an identity anchor. A removed anchor
    /// is explicit and yields no rows; callers must request a fresh anchor
    /// rather than silently falling back to an offset that could click through.
    pub fn window_after_id(&self, anchor: Option<TaskId>, limit: usize) -> VirtualKeysetWindow {
        let limit = limit.min(MAX_VIRTUAL_WINDOW_ROWS);
        let start = match anchor {
            None => 0,
            Some(anchor) => match self.task_ids.iter().position(|task_id| *task_id == anchor) {
                Some(index) => index.saturating_add(1),
                None => {
                    return VirtualKeysetWindow {
                        ids: Vec::new(),
                        next_after_id: None,
                        anchor_found: false,
                    }
                }
            },
        };
        let ids: Vec<TaskId> = self
            .task_ids
            .iter()
            .skip(start)
            .take(limit)
            .copied()
            .collect();
        VirtualKeysetWindow {
            next_after_id: ids.last().copied(),
            ids,
            anchor_found: true,
        }
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
pub struct VirtualKeysetWindow {
    pub ids: Vec<TaskId>,
    pub next_after_id: Option<TaskId>,
    pub anchor_found: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Inbox {
    task_list: TaskList,
}

impl Inbox {
    pub fn empty() -> Self {
        Self {
            task_list: TaskList::empty(),
        }
    }

    pub fn from_model(model: &ClientModel) -> Self {
        Self {
            task_list: TaskList::from_model(model),
        }
    }

    pub fn task_list(&self) -> &TaskList {
        &self.task_list
    }

    pub(crate) fn task_list_mut(&mut self) -> &mut TaskList {
        &mut self.task_list
    }

    pub(crate) fn from_task_list(task_list: TaskList) -> Self {
        Self { task_list }
    }
}
