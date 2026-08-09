//! Pure, bounded projection for the Task Cockpit inbox.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BinaryHeap};
use std::ops::Range;

use crate::client::ClientModel;
use crate::domain::id::{AgentSessionId, TaskId};
use crate::domain::task::{
    ReviewReadiness, TaskActivity, TaskAssignment, TaskAttention, TaskConnectivity, TaskLifecycle,
    VisibleTaskStatus, WorkspaceRef,
};

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

/// The old flat task-list contract remains useful to the shell, but is backed
/// by the same finite viewport rules as the attention inbox.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskList {
    task_ids: Vec<TaskId>,
    viewport: VirtualWindow,
    overflow: Option<TaskListOverflow>,
}

impl TaskList {
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
        Self::from_ordered_ids(task_ids, total_count)
    }

    fn from_ordered_ids(task_ids: Vec<TaskId>, total_count: usize) -> Self {
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

/// Client-local unread counts keyed by durable task identity.
pub type UnreadCursor = BTreeMap<TaskId, u64>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InboxSection {
    NeedsMe,
    Running,
    Ready,
    Recent,
}

impl InboxSection {
    const ALL: [Self; 4] = [Self::NeedsMe, Self::Running, Self::Ready, Self::Recent];

    fn index(self) -> usize {
        match self {
            Self::NeedsMe => 0,
            Self::Running => 1,
            Self::Ready => 2,
            Self::Recent => 3,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InboxError {
    ProjectionUnavailable,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InboxState {
    Ready,
    Empty,
    FilteredEmpty,
    Error(InboxError),
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct InboxFilter {
    query: String,
    include_archived: bool,
}

impl InboxFilter {
    pub fn new(query: impl Into<String>) -> Self {
        Self {
            query: query.into(),
            include_archived: false,
        }
    }

    pub fn including_archived(mut self) -> Self {
        self.include_archived = true;
        self
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn includes_archived(&self) -> bool {
        self.include_archived
    }

    fn matches(&self, title: &str, lifecycle: TaskLifecycle) -> bool {
        if lifecycle == TaskLifecycle::Archived && !self.include_archived {
            return false;
        }
        let query = self.query.trim();
        query.is_empty() || title.to_lowercase().contains(&query.to_lowercase())
    }

    fn is_filtered(&self) -> bool {
        !self.query.trim().is_empty() || self.include_archived
    }
}

/// All fields are copied from `ClientModel` plus the client-local unread
/// cursor. The UI never needs to ask a runtime or provider for row truth.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskRowModel {
    pub task_id: TaskId,
    pub title: String,
    pub project_id: crate::domain::id::ProjectId,
    pub workspace: WorkspaceRef,
    pub assignment: TaskAssignment,
    pub lifecycle: TaskLifecycle,
    pub connectivity: TaskConnectivity,
    pub attention: TaskAttention,
    pub activity: TaskActivity,
    pub review_readiness: ReviewReadiness,
    pub primary_agent_id: Option<AgentSessionId>,
    pub status: VisibleTaskStatus,
    pub unread_event_count: u64,
    pub revision: u64,
    pub created_at_ms: i64,
    pub section: InboxSection,
}

pub type TaskRow = TaskRowModel;

impl TaskRowModel {
    pub fn task_id(&self) -> TaskId {
        self.task_id
    }

    pub fn status(&self) -> VisibleTaskStatus {
        self.status
    }
}

/// A max-heap whose head is the least valuable retained row. This keeps the
/// projection bounded while still applying the complete attention ordering
/// before the finite 5,000-row cap.
struct RetainedRow(TaskRowModel);

impl PartialEq for RetainedRow {
    fn eq(&self, other: &Self) -> bool {
        compare_rows(&self.0, &other.0) == Ordering::Equal
    }
}

impl Eq for RetainedRow {}

impl PartialOrd for RetainedRow {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for RetainedRow {
    fn cmp(&self, other: &Self) -> Ordering {
        compare_rows(&self.0, &other.0)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Inbox {
    task_list: TaskList,
    rows: Vec<TaskRowModel>,
    section_ranges: [Range<usize>; 4],
    filter: InboxFilter,
    state: InboxState,
}

impl Inbox {
    pub fn from_model(model: &ClientModel) -> Self {
        Self::from_model_with_filter(model, &InboxFilter::default(), &UnreadCursor::default())
    }

    pub fn from_model_with_unread(model: &ClientModel, unread: &UnreadCursor) -> Self {
        Self::from_model_with_filter(model, &InboxFilter::default(), unread)
    }

    pub fn from_model_with_filter(
        model: &ClientModel,
        filter: &InboxFilter,
        unread: &UnreadCursor,
    ) -> Self {
        Self::from_projection(Ok(model), filter, unread)
    }

    pub fn from_projection(
        projection: Result<&ClientModel, InboxError>,
        filter: &InboxFilter,
        unread: &UnreadCursor,
    ) -> Self {
        let model = match projection {
            Ok(model) => model,
            Err(error) => return Self::empty(filter, InboxState::Error(error)),
        };
        let mut retained = BinaryHeap::new();
        let mut total_count = 0;

        for snapshot in model.tasks().values() {
            if !filter.matches(&snapshot.task.title, snapshot.task.lifecycle) {
                continue;
            }
            total_count += 1;

            let status = snapshot.visible_status();
            let section = section_for(status);
            let row = TaskRowModel {
                task_id: snapshot.task.id,
                title: snapshot.task.title.clone(),
                project_id: snapshot.task.project_id,
                workspace: snapshot.task.workspace.clone(),
                assignment: snapshot.task.assignment.clone(),
                lifecycle: snapshot.task.lifecycle,
                connectivity: snapshot.connectivity,
                attention: snapshot.attention,
                activity: snapshot.activity,
                review_readiness: snapshot.review_readiness,
                primary_agent_id: snapshot.primary_agent_id,
                status,
                unread_event_count: unread.get(&snapshot.task.id).copied().unwrap_or(0),
                revision: snapshot.task.revision,
                created_at_ms: snapshot.task.created_at_ms,
                section,
            };
            if retained.len() < MAX_TASK_LIST_ITEMS {
                retained.push(RetainedRow(row));
            } else if retained
                .peek()
                .is_some_and(|worst| compare_rows(&row, &worst.0).is_lt())
            {
                retained.pop();
                retained.push(RetainedRow(row));
            }
        }

        let mut grouped: [Vec<TaskRowModel>; 4] = std::array::from_fn(|_| Vec::new());
        for RetainedRow(row) in retained {
            grouped[row.section.index()].push(row);
        }

        for rows in &mut grouped {
            rows.sort_by(compare_rows);
        }

        let retained_count = total_count.min(MAX_TASK_LIST_ITEMS);
        let mut rows = Vec::with_capacity(retained_count);
        let mut section_ranges = [0..0, 0..0, 0..0, 0..0];
        for section in InboxSection::ALL {
            let start = rows.len();
            rows.extend(grouped[section.index()].drain(..));
            section_ranges[section.index()] = start..rows.len();
        }

        let state = if rows.is_empty() {
            if filter.is_filtered() {
                InboxState::FilteredEmpty
            } else {
                InboxState::Empty
            }
        } else {
            InboxState::Ready
        };
        let task_ids = rows.iter().map(|row| row.task_id).collect();

        Self {
            task_list: TaskList::from_ordered_ids(task_ids, total_count),
            rows,
            section_ranges,
            filter: filter.clone(),
            state,
        }
    }

    pub fn from_error(error: InboxError) -> Self {
        Self::from_projection(
            Err(error),
            &InboxFilter::default(),
            &UnreadCursor::default(),
        )
    }

    fn empty(filter: &InboxFilter, state: InboxState) -> Self {
        Self {
            task_list: TaskList::from_ordered_ids(Vec::new(), 0),
            rows: Vec::new(),
            section_ranges: [0..0, 0..0, 0..0, 0..0],
            filter: filter.clone(),
            state,
        }
    }

    pub fn state(&self) -> InboxState {
        self.state.clone()
    }

    pub fn filter(&self) -> &InboxFilter {
        &self.filter
    }

    pub fn task_list(&self) -> &TaskList {
        &self.task_list
    }

    pub fn task_list_mut(&mut self) -> &mut TaskList {
        &mut self.task_list
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn row(&self, task_id: TaskId) -> Option<&TaskRowModel> {
        self.rows.iter().find(|row| row.task_id == task_id)
    }

    /// Selection is always resolved against the durable identity, never a row
    /// position that can change after a projection update.
    pub fn select_task(&self, task_id: TaskId) -> Option<TaskId> {
        self.row(task_id).map(|row| row.task_id)
    }

    pub fn section_rows(&self, section: InboxSection) -> &[TaskRowModel] {
        let range = &self.section_ranges[section.index()];
        &self.rows[range.clone()]
    }

    pub fn task_ids(&self) -> impl Iterator<Item = TaskId> + '_ {
        self.rows.iter().map(|row| row.task_id)
    }

    pub fn rendered_rows(&self) -> &[TaskRowModel] {
        let range = self
            .task_list
            .virtual_window()
            .render_range(self.rows.len());
        &self.rows[range]
    }

    pub fn visible_rows(&self) -> &[TaskRowModel] {
        let range = self.task_list.virtual_window().visible_range();
        &self.rows[range]
    }

    pub fn virtual_window(&self) -> VirtualWindow {
        self.task_list.virtual_window()
    }

    pub fn overflow(&self) -> Option<TaskListOverflow> {
        self.task_list.overflow()
    }

    pub fn set_viewport(
        &mut self,
        first_visible: usize,
        visible_rows: usize,
    ) -> Result<(), ViewportError> {
        if visible_rows == 0 {
            return Err(ViewportError::ZeroVisibleRows);
        }
        self.task_list.set_viewport(first_visible, visible_rows)
    }
}

fn section_for(status: VisibleTaskStatus) -> InboxSection {
    match status {
        VisibleTaskStatus::Disconnected
        | VisibleTaskStatus::Failed
        | VisibleTaskStatus::UncertainOutcome
        | VisibleTaskStatus::NeedsApproval
        | VisibleTaskStatus::NeedsAnswer => InboxSection::NeedsMe,
        VisibleTaskStatus::Working | VisibleTaskStatus::Settling => InboxSection::Running,
        VisibleTaskStatus::ReadyForReview => InboxSection::Ready,
        VisibleTaskStatus::Idle => InboxSection::Recent,
    }
}

fn attention_rank(status: VisibleTaskStatus) -> u8 {
    match status {
        VisibleTaskStatus::Disconnected => 0,
        VisibleTaskStatus::Failed => 1,
        VisibleTaskStatus::UncertainOutcome => 2,
        VisibleTaskStatus::NeedsApproval => 3,
        VisibleTaskStatus::NeedsAnswer => 4,
        VisibleTaskStatus::Working => 5,
        VisibleTaskStatus::Settling => 6,
        VisibleTaskStatus::ReadyForReview => 7,
        VisibleTaskStatus::Idle => 8,
    }
}

fn compare_rows(left: &TaskRowModel, right: &TaskRowModel) -> Ordering {
    attention_rank(left.status)
        .cmp(&attention_rank(right.status))
        .then_with(|| right.revision.cmp(&left.revision))
        .then_with(|| left.title.to_lowercase().cmp(&right.title.to_lowercase()))
        .then_with(|| left.title.cmp(&right.title))
        .then_with(|| left.task_id.cmp(&right.task_id))
}
