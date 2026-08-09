//! Pure, bounded projection for the Task Cockpit inbox.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BinaryHeap};
use std::ops::Range;

use crate::client::ClientModel;
use crate::domain::agent::AgentSessionLifecycle;
use crate::domain::id::{AgentSessionId, TaskId};
use crate::domain::resource::{ResourceKind, ResourceLifecycle};
use crate::domain::task::{
    ReviewReadiness, TaskActivity, TaskAssignment, TaskAttention, TaskConnectivity, TaskLifecycle,
    VisibleTaskStatus, WorkspaceRef,
};
use crate::ui::components::{AccessibilityMetadata, AccessibleRole};

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
        Inbox::from_model(model).task_list
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

    pub fn contains_active_task(&self, task_id: TaskId) -> bool {
        self.task_ids.iter().any(|candidate| *candidate == task_id)
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

const MAX_UNREAD_CURSOR_ENTRIES: usize = 5_000;

#[derive(Clone, Debug, Eq, PartialEq)]
struct UnreadCursorEntry {
    observed: bool,
    observed_sequence: u64,
    read_sequence: u64,
    unread_count: u64,
}

/// Bounded client-local semantic unread cursor.
///
/// Durable event sequence is the semantic cursor. Replayed or reconnected
/// events at or below the highest observed sequence are idempotent, while
/// `mark_read` advances only local presentation state and never creates host
/// truth.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct UnreadCursor {
    entries: BTreeMap<TaskId, UnreadCursorEntry>,
}

impl UnreadCursor {
    pub fn observe_event(&mut self, task_id: TaskId, sequence: u64) -> bool {
        if !self.entries.contains_key(&task_id) && self.entries.len() >= MAX_UNREAD_CURSOR_ENTRIES {
            let evict = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| (entry.observed_sequence, entry.read_sequence))
                .map(|(task_id, _)| *task_id);
            if let Some(evict) = evict {
                self.entries.remove(&evict);
            }
        }
        let entry = self.entries.entry(task_id).or_insert(UnreadCursorEntry {
            observed: false,
            observed_sequence: 0,
            read_sequence: 0,
            unread_count: 0,
        });
        if entry.observed && sequence <= entry.observed_sequence {
            return false;
        }
        entry.observed = true;
        entry.observed_sequence = sequence;
        if sequence > entry.read_sequence {
            entry.unread_count = entry.unread_count.saturating_add(1);
        }
        true
    }

    pub fn mark_read(&mut self, task_id: TaskId) {
        if let Some(entry) = self.entries.get_mut(&task_id) {
            entry.read_sequence = entry.observed_sequence;
            entry.unread_count = 0;
        }
    }

    pub fn unread_count(&self, task_id: TaskId) -> u64 {
        self.entries
            .get(&task_id)
            .map(|entry| entry.unread_count)
            .unwrap_or(0)
    }

    pub fn prune(&mut self, model: &ClientModel) {
        self.entries
            .retain(|task_id, _| model.tasks().contains_key(task_id));
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

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

    pub fn label(self) -> &'static str {
        match self {
            Self::NeedsMe => "Needs Me",
            Self::Running => "Running",
            Self::Ready => "Ready",
            Self::Recent => "Recent",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::NeedsMe => "Tasks that need a decision, recovery, or connection.",
            Self::Running => "Tasks with work in progress.",
            Self::Ready => "Tasks ready for review.",
            Self::Recent => "Recently active or idle tasks.",
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

pub const MAX_PROJECT_LABEL_CHARS: usize = 96;
pub const MAX_WORKTREE_LABEL_CHARS: usize = 128;
pub const MAX_PROVIDER_LABEL_CHARS: usize = 48;
pub const MAX_SECONDARY_LABEL_CHARS: usize = 128;
pub const MAX_ACCESSIBLE_NAME_CHARS: usize = 240;
pub const MAX_ACCESSIBLE_DESCRIPTION_CHARS: usize = 360;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PrimaryProviderState {
    Present { kind: String },
    Missing,
}

impl PrimaryProviderState {
    fn label(&self) -> &str {
        match self {
            Self::Present { kind } => kind,
            Self::Missing => "Provider missing",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeSummary {
    Missing,
    Present {
        lifecycle: AgentSessionLifecycle,
        generation: u64,
    },
}

impl RuntimeSummary {
    fn label(self) -> String {
        match self {
            Self::Missing => "Runtime unavailable".to_string(),
            Self::Present {
                lifecycle: AgentSessionLifecycle::Open,
                generation,
            } => format!("Running · generation {generation}"),
            Self::Present {
                lifecycle: AgentSessionLifecycle::Closing,
                generation,
            } => format!("Stopping · generation {generation}"),
            Self::Present {
                lifecycle: AgentSessionLifecycle::Closed,
                generation,
            } => format!("Stopped · generation {generation}"),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ResourceSummary {
    pub total_count: usize,
    pub active_count: usize,
    pub releasing_count: usize,
    pub terminal_count: usize,
    pub browser_count: usize,
    pub service_count: usize,
}

impl ResourceSummary {
    fn compact_label(self) -> String {
        if self.total_count == 0 {
            return "No resources".to_string();
        }
        let mut parts = Vec::new();
        if self.terminal_count > 0 {
            parts.push(format!("{} terminal", self.terminal_count));
        }
        if self.browser_count > 0 {
            parts.push(format!("{} browser", self.browser_count));
        }
        if self.service_count > 0 {
            parts.push(format!("{} service", self.service_count));
        }
        if self.releasing_count > 0 {
            parts.push(format!("{} releasing", self.releasing_count));
        }
        parts.join(", ")
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskRowDisplay {
    pub project: String,
    pub worktree: String,
    pub primary_provider: PrimaryProviderState,
    pub runtime: RuntimeSummary,
    pub resources: ResourceSummary,
    pub workspace_path_hidden: bool,
    pub display_truncated: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InboxPresentationWidth {
    Regular,
    Narrow,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InboxItemKey {
    Section(InboxSection),
    HistorySection,
    Row(TaskId),
    HistoryRow(TaskId),
    Empty,
    FilteredEmpty,
    Error,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InboxRenderRow {
    pub key: InboxItemKey,
    pub task_id: TaskId,
    pub title: String,
    pub secondary_text: String,
    pub accessible_name: String,
    pub accessible_description: String,
    pub accessibility: AccessibilityMetadata,
    pub display: TaskRowDisplay,
    pub read_only: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InboxRenderItem {
    SectionHeader {
        key: InboxItemKey,
        section: InboxSection,
        name: String,
        description: String,
    },
    HistoryHeader {
        key: InboxItemKey,
        name: String,
        description: String,
    },
    Row(InboxRenderRow),
    HistoryRow(InboxRenderRow),
    State {
        key: InboxItemKey,
        name: String,
        description: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InboxRenderModel {
    pub width: InboxPresentationWidth,
    pub state: InboxState,
    pub items: Vec<InboxRenderItem>,
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
    pub display: TaskRowDisplay,
    pub read_only: bool,
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
    history_rows: Vec<TaskRowModel>,
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
        let (rows, total_count) = project_rows(
            model,
            model.task_projection_index().active_task_ids(),
            filter,
            unread,
            false,
        );
        let (history_rows, _) = if filter.includes_archived() {
            project_rows(
                model,
                model.task_projection_index().archived_task_ids(),
                filter,
                unread,
                true,
            )
        } else {
            (Vec::new(), 0)
        };

        let mut grouped: [Vec<TaskRowModel>; 4] = std::array::from_fn(|_| Vec::new());
        for row in rows {
            grouped[row.section.index()].push(row);
        }

        for rows in &mut grouped {
            rows.sort_by(compare_rows);
        }

        let mut active_rows = Vec::with_capacity(total_count.min(MAX_TASK_LIST_ITEMS));
        let mut section_ranges = [0..0, 0..0, 0..0, 0..0];
        for section in InboxSection::ALL {
            let start = active_rows.len();
            active_rows.extend(grouped[section.index()].drain(..));
            section_ranges[section.index()] = start..active_rows.len();
        }

        let state = if active_rows.is_empty() && history_rows.is_empty() {
            if filter.is_filtered() {
                InboxState::FilteredEmpty
            } else {
                InboxState::Empty
            }
        } else {
            InboxState::Ready
        };
        let task_ids = active_rows.iter().map(|row| row.task_id).collect();

        Self {
            task_list: TaskList::from_ordered_ids(task_ids, total_count),
            rows: active_rows,
            history_rows,
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
            history_rows: Vec::new(),
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
        self.rows.is_empty() && self.history_rows.is_empty()
    }

    pub fn row(&self, task_id: TaskId) -> Option<&TaskRowModel> {
        self.rows
            .iter()
            .chain(self.history_rows.iter())
            .find(|row| row.task_id == task_id)
    }

    pub fn active_row(&self, task_id: TaskId) -> Option<&TaskRowModel> {
        self.rows.iter().find(|row| row.task_id == task_id)
    }

    pub fn history_rows(&self) -> &[TaskRowModel] {
        &self.history_rows
    }

    pub fn history_row(&self, task_id: TaskId) -> Option<&TaskRowModel> {
        self.history_rows.iter().find(|row| row.task_id == task_id)
    }

    pub fn active_rows(&self) -> &[TaskRowModel] {
        &self.rows
    }

    pub fn contains_active_task(&self, task_id: TaskId) -> bool {
        self.active_row(task_id).is_some()
    }

    /// Selection is always resolved against the durable identity, never a row
    /// position that can change after a projection update.
    pub fn select_task(&self, task_id: TaskId) -> Option<TaskId> {
        self.active_row(task_id).map(|row| row.task_id)
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

    /// Build the compact row/section/state contract consumed by the shell.
    /// Native GPUI widgets can render this model without re-reading model or
    /// runtime state, and every item has a stable identity key.
    pub fn render_model(&self, width: InboxPresentationWidth) -> InboxRenderModel {
        let mut items = Vec::new();
        let mut rendered_section = None;
        for row in self.rendered_rows() {
            if rendered_section != Some(row.section) {
                let section = row.section;
                items.push(InboxRenderItem::SectionHeader {
                    key: InboxItemKey::Section(section),
                    section,
                    name: section.label().to_string(),
                    description: section.description().to_string(),
                });
                rendered_section = Some(section);
            }
            items.push(InboxRenderItem::Row(render_row(row, width)));
        }
        if !self.history_rows.is_empty() {
            items.push(InboxRenderItem::HistoryHeader {
                key: InboxItemKey::HistorySection,
                name: "History".to_string(),
                description: "Archived tasks are read-only and cannot be activated.".to_string(),
            });
            items.extend(
                self.history_rows
                    .iter()
                    .map(|row| InboxRenderItem::HistoryRow(render_row(row, width))),
            );
        }
        if items.is_empty() {
            let (key, name, description) = match &self.state {
                InboxState::Ready => (
                    InboxItemKey::Empty,
                    "No tasks".to_string(),
                    "There are no tasks to show.".to_string(),
                ),
                InboxState::Empty => (
                    InboxItemKey::Empty,
                    "No tasks".to_string(),
                    "There are no active tasks yet.".to_string(),
                ),
                InboxState::FilteredEmpty => (
                    InboxItemKey::FilteredEmpty,
                    "No matching tasks".to_string(),
                    "Try a different task filter.".to_string(),
                ),
                InboxState::Error(_) => (
                    InboxItemKey::Error,
                    "Inbox unavailable".to_string(),
                    "Task projection data is temporarily unavailable.".to_string(),
                ),
            };
            items.push(InboxRenderItem::State {
                key,
                name,
                description,
            });
        }
        InboxRenderModel {
            width,
            state: self.state(),
            items,
        }
    }
}

fn project_rows(
    model: &ClientModel,
    task_ids: &[TaskId],
    filter: &InboxFilter,
    unread: &UnreadCursor,
    read_only: bool,
) -> (Vec<TaskRowModel>, usize) {
    if filter.query().trim().is_empty() {
        let total_count = task_ids.len();
        let rows = task_ids
            .iter()
            .take(MAX_TASK_LIST_ITEMS)
            .filter_map(|task_id| {
                let snapshot = model.tasks().get(task_id)?;
                if !filter.matches(&snapshot.task.title, snapshot.task.lifecycle) {
                    return None;
                }
                Some(row_from_snapshot(snapshot, unread, read_only))
            })
            .collect();
        return (rows, total_count);
    }

    let mut retained = BinaryHeap::new();
    let mut total_count = 0;
    for task_id in task_ids {
        let Some(snapshot) = model.tasks().get(task_id) else {
            continue;
        };
        if !filter.matches(&snapshot.task.title, snapshot.task.lifecycle) {
            continue;
        }
        total_count += 1;
        let row = row_from_snapshot(snapshot, unread, read_only);
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
    let mut rows = retained
        .into_iter()
        .map(|RetainedRow(row)| row)
        .collect::<Vec<_>>();
    rows.sort_by(compare_rows);
    (rows, total_count)
}

fn row_from_snapshot(
    snapshot: &crate::domain::snapshot::TaskSnapshot,
    unread: &UnreadCursor,
    read_only: bool,
) -> TaskRowModel {
    let status = snapshot.visible_status();
    TaskRowModel {
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
        unread_event_count: unread.unread_count(snapshot.task.id),
        revision: snapshot.task.revision,
        created_at_ms: snapshot.task.created_at_ms,
        section: section_for(status),
        display: display_for_snapshot(snapshot),
        read_only,
    }
}

fn display_for_snapshot(snapshot: &crate::domain::snapshot::TaskSnapshot) -> TaskRowDisplay {
    let (project, worktree, workspace_path_hidden, worktree_truncated) =
        match &snapshot.task.workspace {
            WorkspaceRef::Main => (
                bounded_text(
                    &snapshot.task.project_id.to_string(),
                    MAX_PROJECT_LABEL_CHARS,
                ),
                "main".to_string(),
                false,
                false,
            ),
            WorkspaceRef::Worktree { branch, .. } => (
                bounded_text(
                    &snapshot.task.project_id.to_string(),
                    MAX_PROJECT_LABEL_CHARS,
                ),
                bounded_text(branch, MAX_WORKTREE_LABEL_CHARS),
                true,
                text_was_truncated(branch, MAX_WORKTREE_LABEL_CHARS),
            ),
            WorkspaceRef::External { .. } => (
                bounded_text(
                    &snapshot.task.project_id.to_string(),
                    MAX_PROJECT_LABEL_CHARS,
                ),
                "external".to_string(),
                true,
                false,
            ),
        };
    let (primary_provider, runtime, provider_truncated) = snapshot
        .primary_agent_id
        .and_then(|agent_id| snapshot.agents.get(&agent_id))
        .map(|agent| {
            (
                PrimaryProviderState::Present {
                    kind: bounded_text(&agent.provider_kind, MAX_PROVIDER_LABEL_CHARS),
                },
                RuntimeSummary::Present {
                    lifecycle: agent.lifecycle,
                    generation: agent.runtime_generation,
                },
                text_was_truncated(&agent.provider_kind, MAX_PROVIDER_LABEL_CHARS),
            )
        })
        .unwrap_or((
            PrimaryProviderState::Missing,
            RuntimeSummary::Missing,
            false,
        ));
    let mut resources = ResourceSummary::default();
    for resource in snapshot.resources.values() {
        resources.total_count += 1;
        match resource.lifecycle {
            ResourceLifecycle::Active => resources.active_count += 1,
            ResourceLifecycle::Releasing => resources.releasing_count += 1,
            ResourceLifecycle::Released => {}
        }
        match resource.resource_kind {
            ResourceKind::Terminal => resources.terminal_count += 1,
            ResourceKind::BrowserContext => resources.browser_count += 1,
            ResourceKind::Service => resources.service_count += 1,
        }
    }
    TaskRowDisplay {
        project,
        worktree,
        primary_provider,
        runtime,
        resources,
        workspace_path_hidden,
        display_truncated: worktree_truncated || provider_truncated,
    }
}

fn render_row(row: &TaskRowModel, width: InboxPresentationWidth) -> InboxRenderRow {
    let provider = row.display.primary_provider.label();
    let runtime = row.display.runtime.label();
    let resources = row.display.resources.compact_label();
    let secondary_text = match width {
        InboxPresentationWidth::Narrow => {
            format!("{} · {} · {}", status_label(row.status), provider, runtime)
        }
        InboxPresentationWidth::Regular => format!(
            "{} · {} · {} · {} · {} · {}",
            row.display.project,
            row.display.worktree,
            provider,
            runtime,
            status_label(row.status),
            resources
        ),
    };
    let read_only_suffix = row.read_only.then_some(" · read-only").unwrap_or_default();
    let title = bounded_text(&row.title, MAX_ACCESSIBLE_NAME_CHARS);
    let mut announcements = Vec::new();
    if row.read_only {
        announcements.push("Archived task is read-only; actions unavailable.");
    }
    if row.display.workspace_path_hidden {
        announcements.push("Workspace path hidden for privacy.");
    }
    if row.display.display_truncated || text_was_truncated(&row.title, MAX_ACCESSIBLE_NAME_CHARS) {
        announcements.push("Some row details truncated.");
    }
    let announcement = if announcements.is_empty() {
        String::new()
    } else {
        format!(" · {}", announcements.join(" "))
    };
    let unread = if row.unread_event_count == 0 {
        "No unread events".to_string()
    } else {
        format!("{} unread events", row.unread_event_count)
    };
    let accessible_name = bounded_text(
        &format!(
            "{} · {}{}",
            title,
            status_label(row.status),
            read_only_suffix
        ),
        MAX_ACCESSIBLE_NAME_CHARS,
    );
    let accessible_description = bounded_text(
        &format!(
            "Project {} · workspace {} · provider {} · runtime {} · state {} · {}{}",
            row.display.project,
            row.display.worktree,
            provider,
            runtime,
            status_label(row.status),
            unread,
            announcement
        ),
        MAX_ACCESSIBLE_DESCRIPTION_CHARS,
    );
    let accessibility = AccessibilityMetadata {
        role: if row.read_only {
            AccessibleRole::Region
        } else {
            AccessibleRole::Button
        },
        name: accessible_name.clone(),
        description: accessible_description.clone(),
        error: matches!(
            row.status,
            VisibleTaskStatus::Failed | VisibleTaskStatus::UncertainOutcome
        )
        .then(|| status_label(row.status).to_string()),
        disabled: row.read_only,
        busy: matches!(
            row.status,
            VisibleTaskStatus::Working | VisibleTaskStatus::Settling
        ),
        focused: false,
        invalid: matches!(
            row.status,
            VisibleTaskStatus::Failed | VisibleTaskStatus::UncertainOutcome
        ),
        read_only: row.read_only,
        value: Some(status_label(row.status).to_string()),
    };
    InboxRenderRow {
        key: if row.read_only {
            InboxItemKey::HistoryRow(row.task_id)
        } else {
            InboxItemKey::Row(row.task_id)
        },
        task_id: row.task_id,
        title,
        secondary_text: bounded_text(&secondary_text, MAX_SECONDARY_LABEL_CHARS),
        accessible_name,
        accessible_description,
        accessibility,
        display: row.display.clone(),
        read_only: row.read_only,
    }
}

fn status_label(status: VisibleTaskStatus) -> &'static str {
    match status {
        VisibleTaskStatus::Disconnected => "Disconnected",
        VisibleTaskStatus::Failed => "Failed",
        VisibleTaskStatus::UncertainOutcome => "Uncertain outcome",
        VisibleTaskStatus::NeedsApproval => "Needs approval",
        VisibleTaskStatus::NeedsAnswer => "Needs answer",
        VisibleTaskStatus::Working => "Working",
        VisibleTaskStatus::Settling => "Settling",
        VisibleTaskStatus::ReadyForReview => "Ready for review",
        VisibleTaskStatus::Idle => "Idle",
    }
}

fn bounded_text(value: &str, max_chars: usize) -> String {
    if max_chars <= 1 {
        return value.chars().take(max_chars).collect();
    }
    let mut chars = value.chars();
    let bounded = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_none() {
        bounded
    } else {
        let mut truncated = bounded.chars().take(max_chars - 1).collect::<String>();
        truncated.push('…');
        truncated
    }
}

fn text_was_truncated(value: &str, max_chars: usize) -> bool {
    value.chars().nth(max_chars).is_some()
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
