//! Pure, bounded projection for the Task Cockpit inbox.

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashSet};
use std::ops::Range;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{
    atomic::{AtomicU64, Ordering as AtomicOrdering},
    Arc, Mutex, OnceLock, TryLockError,
};
use std::thread;
use std::time::{Duration, Instant};

use gpui::{
    div, AnyElement, App, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    ParentElement, Styled, Window,
};
use serde::{Deserialize, Serialize};

use crate::client::model::MAX_INDEXED_TITLE_CHARS;
use crate::client::{normalize_bounded_search_text, ClientModel, SearchContinuation, SearchPage};
use crate::client::{
    ClientSubscription, InboxHostController, SubscriptionError, SubscriptionUpdate,
};
use crate::domain::agent::AgentSessionLifecycle;
use crate::domain::event::DomainEvent;
use crate::domain::id::TaskId;
use crate::domain::resource::{ResourceKind, ResourceLifecycle};
use crate::domain::task::{
    ReviewReadiness, TaskActivity, TaskAttention, TaskConnectivity, TaskLifecycle,
    VisibleTaskStatus, WorkspaceBindingKind, WorkspaceRef,
};
use crate::ui::components::{AccessibilityMetadata, AccessibleRole};

pub const MAX_TASK_LIST_ITEMS: usize = 5_000;
pub const FIXED_VIRTUAL_OVERSCAN: usize = 32;
pub const DEFAULT_VISIBLE_ROWS: usize = 40;
pub const MAX_VIRTUAL_WINDOW_ROWS: usize = 128;
pub const MAX_TASK_SOURCE_IDS: usize = 100_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InboxOverflow {
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TaskListOverflow {
    pub limit: usize,
    pub total_count: usize,
    pub retained_count: usize,
}

/// A bounded identity source for the native uniform list. The source retains
/// IDs only; row snapshots remain owned by the client model and are resolved
/// for the active GPUI window.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskList {
    task_ids: Arc<Vec<TaskId>>,
    viewport: VirtualWindow,
    overflow: Option<TaskListOverflow>,
    virtual_source: bool,
}

impl TaskList {
    pub fn empty() -> Self {
        Self {
            task_ids: Arc::new(Vec::new()),
            viewport: VirtualWindow::for_item_count(0, DEFAULT_VISIBLE_ROWS, 0),
            overflow: None,
            virtual_source: false,
        }
    }

    pub fn from_model(model: &ClientModel) -> Self {
        let ids: Vec<_> = model
            .tasks()
            .iter()
            .filter(|(_, snapshot)| snapshot.task.lifecycle != TaskLifecycle::Archived)
            .map(|(id, _)| *id)
            .take(MAX_TASK_SOURCE_IDS)
            .collect();
        if ids.len() == MAX_TASK_SOURCE_IDS {
            return Self {
                task_ids: Arc::new(Vec::new()),
                viewport: VirtualWindow::for_item_count(0, DEFAULT_VISIBLE_ROWS, 0),
                overflow: Some(TaskListOverflow {
                    limit: MAX_TASK_SOURCE_IDS,
                    total_count: MAX_TASK_SOURCE_IDS.saturating_add(1),
                    retained_count: 0,
                }),
                virtual_source: false,
            };
        }
        Self::from_ids(ids, false)
    }

    pub fn from_virtual_task_ids(task_ids: Vec<TaskId>) -> Result<Self, TaskListOverflow> {
        if task_ids.len() > MAX_TASK_SOURCE_IDS {
            return Err(TaskListOverflow {
                limit: MAX_TASK_SOURCE_IDS,
                total_count: task_ids.len(),
                retained_count: 0,
            });
        }
        let mut seen = HashSet::with_capacity(task_ids.len());
        if task_ids.iter().any(|id| !seen.insert(*id)) {
            return Err(TaskListOverflow {
                limit: usize::MAX,
                total_count: task_ids.len(),
                retained_count: 0,
            });
        }
        Ok(Self::from_ids(task_ids, true))
    }

    pub fn from_client_model_virtual(model: &ClientModel) -> Result<Self, TaskListOverflow> {
        let ids: Vec<_> = model
            .tasks()
            .iter()
            .filter(|(_, snapshot)| snapshot.task.lifecycle != TaskLifecycle::Archived)
            .map(|(id, _)| *id)
            .collect();
        Self::from_virtual_task_ids(ids)
    }

    fn from_ids(task_ids: Vec<TaskId>, virtual_source: bool) -> Self {
        let viewport = VirtualWindow::for_item_count(0, DEFAULT_VISIBLE_ROWS, task_ids.len());
        Self {
            task_ids: Arc::new(task_ids),
            viewport,
            overflow: None,
            virtual_source,
        }
    }

    pub fn task_ids(&self) -> &[TaskId] {
        self.task_ids.as_slice()
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
            .map(|id| format!("native-task-row-{id}"))
            .unwrap_or_else(|| format!("native-task-row-missing-{index}"))
    }

    pub fn window_after_id(&self, anchor: Option<TaskId>, limit: usize) -> VirtualKeysetWindow {
        let limit = limit.min(MAX_VIRTUAL_WINDOW_ROWS);
        let start = match anchor {
            None => 0,
            Some(anchor) => match self.task_ids.iter().position(|id| *id == anchor) {
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
        let ids: Vec<_> = self
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
pub struct VirtualKeysetWindow {
    pub ids: Vec<TaskId>,
    pub next_after_id: Option<TaskId>,
    pub anchor_found: bool,
}

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

/// The old flat task-list contract remains useful to the shell, but is backed
/// by the same finite viewport rules as the attention inbox.
#[derive(Clone, Debug, Eq, PartialEq)]
struct InboxList {
    task_ids: Vec<TaskId>,
    viewport: VirtualWindow,
    overflow: Option<InboxOverflow>,
}

impl InboxList {
    fn from_ordered_ids(task_ids: Vec<TaskId>, total_count: usize) -> Self {
        let overflow = (total_count > MAX_TASK_LIST_ITEMS).then_some(InboxOverflow {
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

    fn len(&self) -> usize {
        self.task_ids.len()
    }

    fn overflow(&self) -> Option<InboxOverflow> {
        self.overflow
    }

    fn virtual_window(&self) -> VirtualWindow {
        self.viewport
    }

    fn set_viewport(
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
}

const MAX_UNREAD_CURSOR_ENTRIES: usize = 5_000;
const MAX_UNREAD_CURSOR_BYTES: usize = 1_048_576;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
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
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct UnreadCursor {
    #[serde(default)]
    last_seen_sequence: u64,
    #[serde(default)]
    seen_event_ids: Vec<crate::domain::id::EventId>,
    #[serde(default)]
    entries: BTreeMap<TaskId, UnreadCursorEntry>,
}

impl UnreadCursor {
    pub const SCHEMA: &'static str = "devmanager.ui.inbox-cursor/v1";

    pub fn last_seen_sequence(&self) -> u64 {
        self.last_seen_sequence
    }

    /// Observe one durable event. The cursor is deliberately client-local:
    /// event identity/sequence is the only resume truth, and task rows remain
    /// owned by `ClientModel`.
    pub fn observe_durable_event(&mut self, event: &DomainEvent) -> bool {
        if self.seen_event_ids.contains(&event.id) || event.sequence <= self.last_seen_sequence {
            return false;
        }
        self.last_seen_sequence = self.last_seen_sequence.max(event.sequence);
        if self.seen_event_ids.len() >= MAX_UNREAD_CURSOR_ENTRIES {
            self.seen_event_ids.remove(0);
        }
        self.seen_event_ids.push(event.id);
        event
            .task_id
            .map(|task_id| self.observe_event(task_id, event.sequence))
            .unwrap_or(true)
    }

    /// A compact, versioned durable representation for the isolated client
    /// preference store.
    /// Invalid/truncated data is rejected rather than silently resetting unread
    /// state, so reconnects cannot manufacture a false read cursor.
    pub fn encode_durable(&self) -> Result<Vec<u8>, String> {
        #[derive(Serialize)]
        struct Wire<'a> {
            schema: &'static str,
            cursor: &'a UnreadCursor,
        }
        rmp_serde::to_vec_named(&Wire {
            schema: Self::SCHEMA,
            cursor: self,
        })
        .map_err(|error| format!("encode inbox cursor: {error}"))
    }

    pub fn decode_durable(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() > MAX_UNREAD_CURSOR_BYTES {
            return Err("inbox cursor exceeds byte bound".to_string());
        }
        #[derive(Deserialize)]
        struct Wire {
            schema: String,
            cursor: UnreadCursor,
        }
        let wire: Wire = rmp_serde::from_slice(bytes)
            .map_err(|error| format!("decode inbox cursor: {error}"))?;
        if wire.schema != Self::SCHEMA {
            return Err("unsupported inbox cursor schema".to_string());
        }
        if wire.cursor.seen_event_ids.len() > MAX_UNREAD_CURSOR_ENTRIES
            || wire.cursor.entries.len() > MAX_UNREAD_CURSOR_ENTRIES
        {
            return Err("inbox cursor exceeds retained-entry bound".to_string());
        }
        Ok(wire.cursor)
    }

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
    pub fn new(query: impl AsRef<str>) -> Self {
        // Keep the UI query on the same 160-char caseless bound as the index
        // search path. Presentation sanitizing (ellipsis, path-sep folding)
        // would diverge from indexed title truth at expanding Unicode edges.
        let query = normalize_bounded_search_text(query.as_ref(), MAX_SEARCH_CHARS).0;
        Self {
            query,
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
        let title = normalize_bounded_search_text(title, MAX_INDEXED_TITLE_CHARS).0;
        query.is_empty() || title.contains(query)
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
pub const MAX_SEARCH_CHARS: usize = 160;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PrimaryProviderIcon {
    Claude,
    Codex,
    Cursor,
    Other,
}

impl PrimaryProviderIcon {
    pub fn label(self) -> &'static str {
        match self {
            Self::Claude => "Claude",
            Self::Codex => "Codex",
            Self::Cursor => "Cursor",
            Self::Other => "Provider",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PrimaryProviderState {
    Present {
        icon: PrimaryProviderIcon,
        /// A bounded display label only; this is never a session/account ID.
        kind: String,
    },
    Missing,
}

impl PrimaryProviderState {
    fn label(&self) -> &str {
        match self {
            Self::Present { kind, .. } => kind,
            Self::Missing => "Provider missing",
        }
    }

    fn icon_label(&self) -> &'static str {
        match self {
            Self::Present { icon, .. } => icon.label(),
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
    pub revision: u64,
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

/// Epochs captured by the shell at render/input handoff time. They are copied
/// into every row action token so a callback cannot accidentally dispatch a
/// row from an older navigation or focus generation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct InboxActionEpochs {
    pub navigation_epoch: u64,
    pub focus_epoch: u64,
}

/// Immutable row facts captured by the renderer. The shell must revalidate all
/// of these facts before executing the action; a `TaskId` alone is not an
/// adequate fence after reorder, resync, or archive transitions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InboxRowActionCapture {
    pub task_id: TaskId,
    pub row_revision: u64,
    pub runtime_generation: Option<u64>,
    pub navigation_epoch: u64,
    pub focus_epoch: u64,
    pub read_only: bool,
}

#[cfg(test)]
fn capture_row_action(row: &TaskRowModel, epochs: InboxActionEpochs) -> InboxRowActionCapture {
    let runtime_generation = match row.display.runtime {
        RuntimeSummary::Present { generation, .. } => Some(generation),
        RuntimeSummary::Missing => None,
    };
    InboxRowActionCapture {
        task_id: row.task_id,
        row_revision: row.revision,
        runtime_generation,
        navigation_epoch: epochs.navigation_epoch,
        focus_epoch: epochs.focus_epoch,
        read_only: row.read_only,
    }
}

fn capture_render_row_action(
    row: &InboxRenderRow,
    epochs: InboxActionEpochs,
) -> InboxRowActionCapture {
    let runtime_generation = match row.display.runtime {
        RuntimeSummary::Present { generation, .. } => Some(generation),
        RuntimeSummary::Missing => None,
    };
    InboxRowActionCapture {
        task_id: row.task_id,
        row_revision: row.revision,
        runtime_generation,
        navigation_epoch: epochs.navigation_epoch,
        focus_epoch: epochs.focus_epoch,
        read_only: row.read_only,
    }
}

/// Native-shell row action bridge. The callback receives the complete row
/// identity/fence captured at render time. The callback is optional so the
/// pure projection renderer remains usable by previews and tests.
pub type InboxRowMouseDownHandler =
    Arc<dyn Fn(InboxRowActionCapture, &MouseDownEvent, &mut Window, &mut App) + 'static>;

pub type LiveClientSubscription = Arc<Mutex<ClientSubscription>>;

#[derive(Debug)]
struct BackgroundSearchResult {
    generation: u64,
    model_revision: u64,
    archived: bool,
    page: SearchPage,
}

#[derive(Debug)]
struct BackgroundSearchRequest {
    generation: u64,
    archived: bool,
    continuation: Option<SearchContinuation>,
}

const BACKGROUND_WORKER_JOIN_BUDGET: Duration = Duration::from_millis(25);
const BACKGROUND_WORKER_REAPER_CAPACITY: usize = 8;
static BACKGROUND_WORKER_REAPER: OnceLock<mpsc::SyncSender<thread::JoinHandle<()>>> =
    OnceLock::new();
static BACKGROUND_WORKER_REAPER_PENDING: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

fn background_worker_reaper() -> &'static mpsc::SyncSender<thread::JoinHandle<()>> {
    BACKGROUND_WORKER_REAPER.get_or_init(|| {
        let (sender, receiver) =
            mpsc::sync_channel::<thread::JoinHandle<()>>(BACKGROUND_WORKER_REAPER_CAPACITY);
        thread::Builder::new()
            .name("devmanager-inbox-worker-reaper".to_string())
            .spawn(move || {
                while let Ok(join) = receiver.recv() {
                    let _ = join.join();
                    BACKGROUND_WORKER_REAPER_PENDING.fetch_sub(1, AtomicOrdering::AcqRel);
                }
            })
            .expect("spawn inbox worker reaper");
        sender
    })
}

fn settle_background_worker(join: thread::JoinHandle<()>) {
    if join.is_finished() {
        let _ = join.join();
        return;
    }
    let sender = background_worker_reaper();
    BACKGROUND_WORKER_REAPER_PENDING.fetch_add(1, AtomicOrdering::AcqRel);
    if let Err(error) = sender.send(join) {
        BACKGROUND_WORKER_REAPER_PENDING.fetch_sub(1, AtomicOrdering::AcqRel);
        let _ = error.0.join();
    }
}

#[cfg(test)]
fn background_reaper_pending_for_test() -> usize {
    BACKGROUND_WORKER_REAPER_PENDING.load(AtomicOrdering::Acquire)
}

#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub enum SearchWorkerState {
    #[default]
    Idle,
    Running,
    Retiring,
}

#[derive(Debug)]
struct BackgroundSearchWorker {
    cancellation: Arc<AtomicU64>,
    results: Receiver<BackgroundSearchResult>,
    join: Option<thread::JoinHandle<()>>,
    retiring: bool,
}

impl BackgroundSearchWorker {
    fn cancel(&mut self, invalid_generation: u64) {
        self.retiring = true;
        self.cancellation
            .store(invalid_generation, AtomicOrdering::Release);
    }

    fn state(&self) -> SearchWorkerState {
        if self.retiring {
            SearchWorkerState::Retiring
        } else {
            SearchWorkerState::Running
        }
    }
}

fn run_background_search_page(
    model: Arc<ClientModel>,
    filter: InboxFilter,
    request: BackgroundSearchRequest,
    cancelled_generation: Arc<AtomicU64>,
    results: mpsc::SyncSender<BackgroundSearchResult>,
) {
    if cancelled_generation.load(AtomicOrdering::Acquire) != request.generation {
        return;
    }
    let page = model.search_task_ids_page(
        filter.query(),
        request.archived,
        request.continuation.as_ref(),
    );
    if cancelled_generation.load(AtomicOrdering::Acquire) != request.generation {
        return;
    }
    let _ = results.try_send(BackgroundSearchResult {
        generation: request.generation,
        model_revision: model.task_projection_index().revision(),
        archived: request.archived,
        page,
    });
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SearchProgress {
    pub published: bool,
    pub requested: bool,
    pub complete: bool,
    pub worker_state: SearchWorkerState,
}

/// The production Inbox projection bridge. The canonical lane owns one
/// projection; no legacy task-list cache is permitted beside it. Transport IO is
/// caller-driven, while this object keeps the durable cursor and performs the
/// small projection update synchronously after each applied event.
#[derive(Debug, Default)]
pub struct InboxRuntime {
    subscription: Option<ClientSubscription>,
    live_subscription: Option<LiveClientSubscription>,
    unread: UnreadCursor,
    filter: InboxFilter,
    projection: Option<Inbox>,
    projection_stale: bool,
    projection_updates: u64,
    search_generation: u64,
    background_worker: Option<BackgroundSearchWorker>,
    background_model: Option<Arc<ClientModel>>,
    background_active_page: Option<SearchPage>,
    background_archived_page: Option<SearchPage>,
}

impl InboxRuntime {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn attach_subscription(&mut self, subscription: ClientSubscription) {
        self.cancel_background_search();
        self.live_subscription = None;
        self.background_model = subscription.model().cloned().map(Arc::new);
        self.subscription = Some(subscription);
        self.projection_stale = false;
        self.rebuild_projection();
    }

    /// Attach the caller-driven production subscription without cloning its
    /// potentially large ClientModel. The pump owns the mutex while it awaits
    /// Transport IO; the UI only takes a short `try_lock` during update/render
    /// handoff and therefore never waits on transport from paint/input.
    pub fn attach_live_subscription(&mut self, subscription: LiveClientSubscription) {
        self.cancel_background_search();
        self.subscription = None;
        self.background_model = None;
        self.live_subscription = Some(subscription);
        self.projection_stale = false;
        self.refresh_from_subscription();
    }

    /// Native-next attaches the one canonical-lane subscription here. The
    /// controller remains responsible for host I/O; this method only hands
    /// the shared model projection to the caller's Inbox runtime.
    pub fn attach_host_controller(&mut self, controller: &InboxHostController) {
        self.attach_live_subscription(controller.subscription());
    }

    pub fn subscription(&self) -> Option<&ClientSubscription> {
        self.subscription.as_ref()
    }

    pub fn subscription_mut(&mut self) -> Option<&mut ClientSubscription> {
        self.subscription.as_mut()
    }

    pub fn live_subscription(&self) -> Option<LiveClientSubscription> {
        self.live_subscription.clone()
    }

    pub fn projection(&self) -> Option<&Inbox> {
        self.projection.as_ref()
    }

    pub fn projection_stale(&self) -> bool {
        self.projection_stale
    }

    pub fn invalidate_for_resync(&mut self) {
        self.cancel_background_search();
        self.projection_stale = true;
        self.projection = None;
        self.projection_updates = self.projection_updates.saturating_add(1);
    }

    pub fn unread_cursor(&self) -> &UnreadCursor {
        &self.unread
    }

    pub fn restore_unread_cursor(&mut self, cursor: UnreadCursor) {
        self.cancel_background_search();
        self.unread = cursor;
        self.rebuild_projection();
        self.start_background_search();
    }

    pub fn restore_unread_cursor_bytes(&mut self, bytes: &[u8]) -> Result<(), String> {
        let cursor = UnreadCursor::decode_durable(bytes)?;
        self.restore_unread_cursor(cursor);
        Ok(())
    }

    /// Restore the client-local cursor owned by the native-next controller.
    /// Missing preferences are the backwards-compatible empty cursor; a
    /// present but malformed/version-mismatched cursor fails closed.
    pub fn restore_unread_cursor_from_controller(
        &mut self,
        controller: &InboxHostController,
    ) -> Result<(), String> {
        match controller
            .restore_unread_cursor()
            .map_err(|error| error.to_string())?
        {
            Some(bytes) => self.restore_unread_cursor_bytes(&bytes),
            None => {
                self.restore_unread_cursor(UnreadCursor::default());
                Ok(())
            }
        }
    }

    /// Publish the current bounded cursor through the explicit native-next
    /// preference authority. Legacy `SessionState` is never involved.
    pub fn persist_unread_cursor_to_controller(
        &self,
        controller: &InboxHostController,
    ) -> Result<(), String> {
        let bytes = self.encode_unread_cursor()?;
        controller
            .persist_unread_cursor(Some(&bytes))
            .map_err(|error| error.to_string())
    }

    /// Refresh after the caller's async subscription pump has applied a live
    /// event. This keeps the GPUI render path deterministic and allocation-free
    /// with respect to transport state.
    pub fn refresh_from_subscription(&mut self) {
        self.cancel_background_search();
        if let Some(subscription) = self.live_subscription.clone() {
            if let Ok(mut subscription) = subscription.try_lock() {
                if subscription.state() != crate::client::ClientSubscriptionState::Ready {
                    self.projection_stale = true;
                    self.projection = None;
                    return;
                }
                for event in subscription.take_replay_events() {
                    let _ = self.unread.observe_durable_event(&event);
                }
                let filter = self.filter.clone();
                let unread = self.unread.clone();
                self.projection = subscription.model().map(|model| {
                    if filter.query().trim().is_empty() {
                        Inbox::from_model_with_filter(model, &filter, &unread)
                    } else {
                        Inbox::from_model_with_search_pages(
                            model,
                            &filter,
                            &unread,
                            empty_background_search_page(),
                            None,
                        )
                    }
                });
                self.projection_stale = false;
                self.projection_updates = self.projection_updates.saturating_add(1);
            }
        } else {
            self.rebuild_projection();
        }
        self.start_background_search();
    }

    pub fn encode_unread_cursor(&self) -> Result<Vec<u8>, String> {
        self.unread.encode_durable()
    }

    pub fn set_filter(&mut self, filter: InboxFilter) {
        self.cancel_background_search();
        self.filter = filter;
        self.rebuild_projection();
        self.start_background_search();
    }

    /// Apply one bounded continuation result produced by the background search
    /// worker. Callers invoke this from their task/controller lane; rendering
    /// and input only consume the already-published projection. A generation
    /// or model-revision mismatch is discarded and can never replace rows for
    /// a newer filter or subscription generation.
    pub fn poll_background_search(&mut self) -> bool {
        let (result, disconnected) = {
            let Some(worker) = self.background_worker.as_mut() else {
                return false;
            };
            match worker.results.try_recv() {
                Ok(result) => (Some(result), false),
                Err(TryRecvError::Empty) => (None, false),
                Err(TryRecvError::Disconnected) => (None, true),
            }
        };
        if disconnected {
            self.reap_background_worker();
            return false;
        }
        let Some(result) = result else {
            self.reap_background_worker();
            return false;
        };
        if result.generation != self.search_generation || self.projection_stale {
            self.reap_background_worker();
            return false;
        }
        if let Some(subscription) = self.live_subscription.clone() {
            let Ok(subscription) = subscription.try_lock() else {
                return false;
            };
            if subscription.state() != crate::client::ClientSubscriptionState::Ready {
                return false;
            }
            let Some(model) = subscription.model() else {
                return false;
            };
            if model.task_projection_index().revision() != result.model_revision {
                drop(subscription);
                self.rebuild_projection();
                self.background_active_page = None;
                self.background_archived_page = None;
                self.reap_background_worker();
                return false;
            }
            let published = self.publish_background_result(result, model);
            self.reap_background_worker();
            return published;
        }
        let Some(model) = self.background_model.clone() else {
            self.reap_background_worker();
            return false;
        };
        if model.task_projection_index().revision() != result.model_revision {
            self.rebuild_projection();
            self.background_active_page = None;
            self.background_archived_page = None;
            self.reap_background_worker();
            return false;
        }
        let published = self.publish_background_result(result, &model);
        self.reap_background_worker();
        published
    }

    /// Advance one bounded continuation on the canonical controller lane.
    /// Polling and scheduling are nonblocking; at most one page worker exists
    /// and at most one next page is requested per tick.
    pub fn tick_background_search(&mut self) -> SearchProgress {
        let published = self.poll_background_search();
        let requested = self.request_background_search_page();
        SearchProgress {
            published,
            requested,
            complete: self.background_search_complete(),
            worker_state: self.background_search_state(),
        }
    }

    fn background_search_complete(&self) -> bool {
        if self.filter.query().trim().is_empty() {
            return true;
        }
        let active_complete = self
            .background_active_page
            .as_ref()
            .is_some_and(|page| page.continuation().is_none());
        let archived_complete = !self.filter.includes_archived()
            || self
                .background_archived_page
                .as_ref()
                .is_some_and(|page| page.continuation().is_none());
        active_complete && archived_complete
    }

    /// Request exactly one page. The caller must invoke this again after the
    /// returned page is published if the continuation still has demand.
    pub fn request_background_search_page(&mut self) -> bool {
        if self.projection_stale
            || self.filter.query().trim().is_empty()
            || self.background_worker.is_some()
        {
            return false;
        }
        let (archived, continuation) = if self.background_active_page.is_none() {
            (false, None)
        } else if self
            .background_active_page
            .as_ref()
            .is_some_and(|page| page.continuation().is_some())
        {
            (
                false,
                self.background_active_page
                    .as_ref()
                    .and_then(|page| page.continuation().cloned()),
            )
        } else if !self.filter.includes_archived() {
            return false;
        } else if self.background_archived_page.is_none() {
            (true, None)
        } else if self
            .background_archived_page
            .as_ref()
            .is_some_and(|page| page.continuation().is_some())
        {
            (
                true,
                self.background_archived_page
                    .as_ref()
                    .and_then(|page| page.continuation().cloned()),
            )
        } else {
            return false;
        };

        let subscription = self.live_subscription.clone();
        let model = self.background_model.clone();
        if subscription.is_none() && model.is_none() {
            return false;
        }
        let generation = self.search_generation;
        let cancellation = Arc::new(AtomicU64::new(generation));
        let (results_tx, results_rx) = mpsc::sync_channel(1);
        let filter = self.filter.clone();
        let worker_cancellation = Arc::clone(&cancellation);
        let worker_cancellation_for_model = Arc::clone(&worker_cancellation);
        let join = thread::spawn(move || {
            let model = model.or_else(|| {
                subscription.and_then(|subscription| {
                    loop {
                        if worker_cancellation_for_model.load(AtomicOrdering::Acquire) != generation
                        {
                            return None;
                        }
                        match subscription.try_lock() {
                            Ok(subscription) => {
                                break subscription.model().cloned().map(Arc::new);
                            }
                            Err(TryLockError::Poisoned(_)) => return None,
                            Err(TryLockError::WouldBlock) => {
                                // Keep the owned worker cancellable while the
                                // controller briefly holds the borrowed
                                // subscription for transport handoff.
                                thread::park_timeout(Duration::from_millis(1));
                            }
                        }
                    }
                })
            });
            if let Some(model) = model {
                run_background_search_page(
                    model,
                    filter,
                    BackgroundSearchRequest {
                        generation,
                        archived,
                        continuation,
                    },
                    worker_cancellation,
                    results_tx,
                );
            }
        });
        self.background_worker = Some(BackgroundSearchWorker {
            cancellation,
            results: results_rx,
            join: Some(join),
            retiring: false,
        });
        true
    }

    fn publish_background_result(
        &mut self,
        result: BackgroundSearchResult,
        model: &ClientModel,
    ) -> bool {
        if result.archived {
            self.background_archived_page = Some(result.page);
        } else {
            self.background_active_page = Some(result.page);
        }
        let Some(active_page) = self.background_active_page.clone() else {
            return false;
        };
        let archived_page = self.background_archived_page.clone();
        self.projection = Some(Inbox::from_model_with_search_pages(
            model,
            &self.filter,
            &self.unread,
            active_page,
            archived_page,
        ));
        self.projection_stale = false;
        self.projection_updates = self.projection_updates.saturating_add(1);
        true
    }

    pub fn background_search_pending(&self) -> bool {
        self.background_worker.is_some()
    }

    pub fn background_search_state(&self) -> SearchWorkerState {
        self.background_worker
            .as_ref()
            .map(BackgroundSearchWorker::state)
            .unwrap_or(SearchWorkerState::Idle)
    }

    /// Mark one task read in the bounded client-local cursor and update only
    /// retained rows. This never emits a host command: read state is a local
    /// presentation preference, while task lifecycle remains host-owned.
    pub fn mark_read(&mut self, task_id: TaskId) -> bool {
        if self.unread.unread_count(task_id) == 0 {
            return false;
        }
        self.unread.mark_read(task_id);
        if let Some(projection) = self.projection.as_mut() {
            projection.set_unread_cursor(self.unread.clone());
        }
        true
    }

    pub fn set_active_viewport(
        &mut self,
        first_visible: usize,
        visible_rows: usize,
    ) -> Result<(), ViewportError> {
        self.cancel_background_search();
        self.projection
            .as_mut()
            .ok_or(ViewportError::ZeroVisibleRows)?
            .set_active_viewport(first_visible, visible_rows)
    }

    pub fn set_archived_viewport(
        &mut self,
        first_visible: usize,
        visible_rows: usize,
    ) -> Result<(), ViewportError> {
        self.cancel_background_search();
        self.projection
            .as_mut()
            .ok_or(ViewportError::ZeroVisibleRows)?
            .set_archived_viewport(first_visible, visible_rows)
    }

    pub fn filter(&self) -> &InboxFilter {
        &self.filter
    }

    pub fn projection_updates(&self) -> u64 {
        self.projection_updates
    }

    pub fn apply_subscription_update(
        &mut self,
        update: SubscriptionUpdate,
    ) -> Result<bool, SubscriptionError> {
        match update {
            SubscriptionUpdate::DurableEvent(event) => {
                if self.projection_stale {
                    return Ok(false);
                }
                let observed = self.unread.observe_durable_event(&event);
                if !observed {
                    // ClientSubscription already applied this event (or the
                    // durable cursor has seen it). Do not rebuild the
                    // projection for an idempotent replay.
                    return Ok(false);
                }
                let unread = self.unread.clone();
                let mut applied = false;
                if let Some(subscription) = self.live_subscription.clone() {
                    if let Ok(subscription) = subscription.try_lock() {
                        if let Some(model) = subscription.model() {
                            if let Some(projection) = self.projection.as_mut() {
                                projection.set_unread_cursor(unread);
                                projection.apply_model_event(model, event.task_id);
                                applied = true;
                            }
                        }
                    }
                } else if let Some(subscription) = self.subscription.as_ref() {
                    if let Some(model) = subscription.model() {
                        if let Some(projection) = self.projection.as_mut() {
                            projection.set_unread_cursor(unread);
                            projection.apply_model_event(model, event.task_id);
                            applied = true;
                        }
                    }
                }
                if applied {
                    self.projection_updates = self.projection_updates.saturating_add(1);
                } else {
                    self.rebuild_projection();
                }
                if self.live_subscription.is_none() {
                    self.background_model = self
                        .subscription
                        .as_ref()
                        .and_then(|subscription| subscription.model().cloned())
                        .map(Arc::new);
                }
                self.cancel_background_search();
                self.start_background_search();
                Ok(observed)
            }
            SubscriptionUpdate::ResyncRequired { .. } => {
                self.cancel_background_search();
                self.projection_stale = true;
                self.projection = None;
                self.projection_updates = self.projection_updates.saturating_add(1);
                Ok(false)
            }
            SubscriptionUpdate::Stream(_) => Ok(false),
        }
    }

    pub fn render_model(&self, width: InboxPresentationWidth) -> InboxRenderModel {
        self.projection
            .as_ref()
            .map(|inbox| inbox.render_model(width))
            .unwrap_or_else(|| {
                Inbox::from_error(InboxError::ProjectionUnavailable).render_model(width)
            })
    }

    fn rebuild_projection(&mut self) {
        if self.projection_stale {
            self.projection = None;
            return;
        }
        let filter = self.filter.clone();
        if let Some(subscription) = self.subscription.as_mut() {
            for event in subscription.take_replay_events() {
                let _ = self.unread.observe_durable_event(&event);
            }
            let unread = self.unread.clone();
            self.projection = subscription.model().map(|model| {
                if filter.query().trim().is_empty() {
                    Inbox::from_model_with_filter(model, &filter, &unread)
                } else {
                    Inbox::from_model_with_search_pages(
                        model,
                        &filter,
                        &unread,
                        empty_background_search_page(),
                        None,
                    )
                }
            });
        } else if let Some(model) = self.background_model.as_deref() {
            self.projection = Some(if filter.query().trim().is_empty() {
                Inbox::from_model_with_filter(model, &filter, &self.unread)
            } else {
                Inbox::from_model_with_search_pages(
                    model,
                    &filter,
                    &self.unread,
                    empty_background_search_page(),
                    None,
                )
            });
        } else {
            self.projection = self.live_subscription.clone().and_then(|subscription| {
                let mut subscription = subscription.try_lock().ok()?;
                for event in subscription.take_replay_events() {
                    let _ = self.unread.observe_durable_event(&event);
                }
                let unread = self.unread.clone();
                subscription.model().map(|model| {
                    if filter.query().trim().is_empty() {
                        Inbox::from_model_with_filter(model, &filter, &unread)
                    } else {
                        Inbox::from_model_with_search_pages(
                            model,
                            &filter,
                            &unread,
                            empty_background_search_page(),
                            None,
                        )
                    }
                })
            });
        }
        self.projection_stale = self.projection.is_none();
        self.projection_updates = self.projection_updates.saturating_add(1);
    }

    fn cancel_background_search(&mut self) {
        self.search_generation = self.search_generation.wrapping_add(1);
        if let Some(worker) = self.background_worker.as_mut() {
            worker.cancel(self.search_generation);
        }
        self.join_background_worker_until(Instant::now() + BACKGROUND_WORKER_JOIN_BUDGET);
        self.background_active_page = None;
        self.background_archived_page = None;
    }

    fn reap_background_worker(&mut self) -> bool {
        let finished = self.background_worker.as_ref().is_some_and(|worker| {
            worker
                .join
                .as_ref()
                .is_some_and(thread::JoinHandle::is_finished)
        });
        if !finished {
            return false;
        }
        let Some(mut worker) = self.background_worker.take() else {
            return false;
        };
        if let Some(join) = worker.join.take() {
            let _ = join.join();
        }
        true
    }

    fn join_background_worker_until(&mut self, deadline: Instant) {
        while self.background_worker.is_some() && Instant::now() < deadline {
            if self.reap_background_worker() {
                return;
            }
            thread::park_timeout(Duration::from_millis(1));
        }
        let _ = self.reap_background_worker();
    }

    fn start_background_search(&mut self) {
        let _ = self.request_background_search_page();
    }
}

impl Drop for InboxRuntime {
    fn drop(&mut self) {
        self.cancel_background_search();
        if let Some(mut worker) = self.background_worker.take() {
            worker.cancel(self.search_generation);
            if let Some(join) = worker.join.take() {
                let deadline = Instant::now() + BACKGROUND_WORKER_JOIN_BUDGET;
                while !join.is_finished() && Instant::now() < deadline {
                    thread::park_timeout(Duration::from_millis(1));
                }
                if join.is_finished() {
                    let _ = join.join();
                } else {
                    // The UI drop budget bounds the caller, not worker
                    // lifetime. Transfer unfinished ownership to the bounded
                    // reaper; dropping JoinHandle would detach the worker.
                    settle_background_worker(join);
                }
            }
        }
    }
}

/// Minimal GPUI renderer for the native shell. It consumes only the bounded
/// render model, so it cannot reach back into a provider, runtime, path, or
/// terminal while painting a frame.
pub fn render_native_inbox(model: &InboxRenderModel) -> AnyElement {
    render_native_inbox_with_actions(model, InboxActionEpochs::default(), None)
}

pub fn render_native_inbox_with_actions(
    model: &InboxRenderModel,
    epochs: InboxActionEpochs,
    row_handler: Option<InboxRowMouseDownHandler>,
) -> AnyElement {
    let mut items = Vec::with_capacity(model.items.len());
    for item in &model.items {
        let element = match item {
            InboxRenderItem::SectionHeader {
                name, description, ..
            }
            | InboxRenderItem::HistoryHeader {
                name, description, ..
            }
            | InboxRenderItem::State {
                name, description, ..
            } => div()
                .flex()
                .flex_col()
                .child(name.clone())
                .child(description.clone())
                .into_any_element(),
            InboxRenderItem::Row(row) => {
                let mut element = div()
                    .flex()
                    .flex_col()
                    .child(row.title.clone())
                    .child(row.secondary_text.clone());
                if let Some(handler) = row_handler.clone() {
                    let action = capture_render_row_action(row, epochs);
                    element = element.on_mouse_down(MouseButton::Left, move |event, window, cx| {
                        // A task row owns the pointer gesture. Stopping
                        // propagation here prevents the shell's terminal
                        // surface from interpreting the same click after a
                        // row was reordered or rejected by its action fence.
                        cx.stop_propagation();
                        handler(action, event, window, cx);
                    });
                }
                element.into_any_element()
            }
            InboxRenderItem::HistoryRow(row) => div()
                .flex()
                .flex_col()
                .child(row.title.clone())
                .child(row.secondary_text.clone())
                .into_any_element(),
        };
        items.push(element);
    }
    div().flex().flex_col().children(items).into_any_element()
}

/// All fields are copied from `ClientModel` plus the client-local unread
/// cursor. The UI never needs to ask a runtime or provider for row truth.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskRowModel {
    pub task_id: TaskId,
    pub title: String,
    pub lifecycle: TaskLifecycle,
    pub connectivity: TaskConnectivity,
    pub attention: TaskAttention,
    pub activity: TaskActivity,
    pub review_readiness: ReviewReadiness,
    pub status: VisibleTaskStatus,
    pub unread_event_count: u64,
    pub revision: u64,
    pub created_at_ms: i64,
    pub occurred_at_ms: i64,
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

#[derive(Clone, Debug)]
struct InboxSearchProjection {
    ids: Vec<TaskId>,
    total_count: usize,
}

impl InboxSearchProjection {
    fn from_page(page: SearchPage) -> Self {
        let total_count = page.exact_total.unwrap_or_else(|| {
            // A partial page that filled the retained window knows only that
            // there is at least one more match. Preserve that truth in the
            // UI overflow contract instead of presenting 5,000 as complete.
            if page.is_partial() && page.ids.len() >= MAX_TASK_LIST_ITEMS {
                // The index may know a large posting count while the page is
                // still ordering/filtering only a bounded prefix. Do not
                // manufacture a more precise total from that lower bound.
                MAX_TASK_LIST_ITEMS.saturating_add(1)
            } else {
                page.known_total
            }
        });
        Self {
            ids: page.ids,
            total_count,
        }
    }
}

fn search_projection_page(
    model: &ClientModel,
    filter: &InboxFilter,
    archived: bool,
    continuation: Option<&SearchContinuation>,
) -> InboxSearchProjection {
    if filter.query().trim().is_empty() && continuation.is_none() {
        return InboxSearchProjection {
            ids: if archived {
                model
                    .task_projection_index()
                    .top_archived_task_ids(MAX_TASK_LIST_ITEMS)
            } else {
                model
                    .task_projection_index()
                    .top_active_task_ids(MAX_TASK_LIST_ITEMS)
            },
            total_count: if archived {
                model.task_projection_index().archived_count()
            } else {
                model.task_projection_index().active_count()
            },
        };
    }
    InboxSearchProjection::from_page(model.search_task_ids_page(
        filter.query(),
        archived,
        continuation,
    ))
}

fn empty_background_search_page() -> SearchPage {
    SearchPage::pending()
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Inbox {
    task_list: InboxList,
    rows: Vec<TaskRowModel>,
    archived_list: InboxList,
    history_rows: Vec<TaskRowModel>,
    unread: UnreadCursor,
    section_ranges: [Range<usize>; 4],
    filter: InboxFilter,
    state: InboxState,
    full_rebuilds: u64,
    incremental_updates: u64,
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
        let active_page = search_projection_page(model, filter, false, None);
        let (rows, total_count) = project_rows(
            model,
            &active_page.ids,
            filter,
            unread,
            false,
            active_page.total_count,
        );
        let archived_page = if filter.includes_archived() {
            Some(search_projection_page(model, filter, true, None))
        } else {
            None
        };
        let (history_rows, history_total_count) = if let Some(archived_page) = archived_page {
            project_rows(
                model,
                &archived_page.ids,
                filter,
                unread,
                true,
                archived_page.total_count,
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
            task_list: InboxList::from_ordered_ids(task_ids, total_count),
            rows: active_rows,
            archived_list: InboxList::from_ordered_ids(
                history_rows.iter().map(|row| row.task_id).collect(),
                history_total_count,
            ),
            history_rows,
            unread: unread.clone(),
            section_ranges,
            filter: filter.clone(),
            state,
            full_rebuilds: 1,
            incremental_updates: 0,
        }
    }

    fn from_model_with_search_pages(
        model: &ClientModel,
        filter: &InboxFilter,
        unread: &UnreadCursor,
        active_page: SearchPage,
        archived_page: Option<SearchPage>,
    ) -> Self {
        let active_page = InboxSearchProjection::from_page(active_page);
        let archived_page = archived_page.map(InboxSearchProjection::from_page);
        let (rows, total_count) = project_rows(
            model,
            &active_page.ids,
            filter,
            unread,
            false,
            active_page.total_count,
        );
        let (history_rows, history_total_count) = archived_page
            .as_ref()
            .map(|page| project_rows(model, &page.ids, filter, unread, true, page.total_count))
            .unwrap_or_else(|| (Vec::new(), 0));

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
            task_list: InboxList::from_ordered_ids(task_ids, total_count),
            rows: active_rows,
            archived_list: InboxList::from_ordered_ids(
                history_rows.iter().map(|row| row.task_id).collect(),
                history_total_count,
            ),
            history_rows,
            unread: unread.clone(),
            section_ranges,
            filter: filter.clone(),
            state,
            full_rebuilds: 1,
            incremental_updates: 0,
        }
    }

    pub fn from_error(error: InboxError) -> Self {
        Self::from_projection(
            Err(error),
            &InboxFilter::default(),
            &UnreadCursor::default(),
        )
    }

    /// Apply one task delta to the retained rows. The ClientModel index has
    /// already updated the task's order/search/archive entry; this method
    /// touches only the retained active/history windows and never rebuilds
    /// the complete Inbox from the 100k-task model.
    pub fn apply_model_event(&mut self, model: &ClientModel, task_id: Option<TaskId>) {
        if task_id.is_none() {
            return;
        }
        let active_page = search_projection_page(model, &self.filter, false, None);
        let archived_page = if self.filter.includes_archived() {
            Some(search_projection_page(model, &self.filter, true, None))
        } else {
            None
        };
        // Refill only the bounded pages affected by one index update. This
        // handles a task entering/leaving the retained cap without rebuilding
        // the complete model or scanning 100k tasks.
        self.rows =
            project_indexed_page(model, &active_page.ids, &self.filter, &self.unread, false);
        self.history_rows = archived_page
            .as_ref()
            .map(|page| project_indexed_page(model, &page.ids, &self.filter, &self.unread, true))
            .unwrap_or_default();
        self.rebuild_section_ranges();
        self.task_list = InboxList::from_ordered_ids(
            self.rows.iter().map(|row| row.task_id).collect(),
            active_page.total_count,
        );
        self.archived_list = InboxList::from_ordered_ids(
            self.history_rows.iter().map(|row| row.task_id).collect(),
            archived_page
                .as_ref()
                .map(|page| page.total_count)
                .unwrap_or(0),
        );
        self.state = if self.rows.is_empty() && self.history_rows.is_empty() {
            if self.filter.is_filtered() {
                InboxState::FilteredEmpty
            } else {
                InboxState::Empty
            }
        } else {
            InboxState::Ready
        };
        self.incremental_updates = self.incremental_updates.saturating_add(1);
    }

    pub fn set_unread_cursor(&mut self, unread: UnreadCursor) {
        self.unread = unread;
        for row in self.rows.iter_mut().chain(self.history_rows.iter_mut()) {
            row.unread_event_count = self.unread.unread_count(row.task_id);
        }
    }

    pub fn full_rebuilds(&self) -> u64 {
        self.full_rebuilds
    }

    pub fn incremental_updates(&self) -> u64 {
        self.incremental_updates
    }

    fn rebuild_section_ranges(&mut self) {
        let mut grouped: [Vec<TaskRowModel>; 4] = std::array::from_fn(|_| Vec::new());
        for row in self.rows.drain(..) {
            grouped[row.section.index()].push(row);
        }
        let mut rows =
            Vec::with_capacity(MAX_TASK_LIST_ITEMS.min(grouped.iter().map(Vec::len).sum()));
        let mut section_ranges = [0..0, 0..0, 0..0, 0..0];
        for section in InboxSection::ALL {
            let start = rows.len();
            rows.extend(grouped[section.index()].drain(..));
            section_ranges[section.index()] = start..rows.len();
        }
        self.rows = rows;
        self.section_ranges = section_ranges;
    }

    fn empty(filter: &InboxFilter, state: InboxState) -> Self {
        Self {
            task_list: InboxList::from_ordered_ids(Vec::new(), 0),
            rows: Vec::new(),
            archived_list: InboxList::from_ordered_ids(Vec::new(), 0),
            history_rows: Vec::new(),
            unread: UnreadCursor::default(),
            section_ranges: [0..0, 0..0, 0..0, 0..0],
            filter: filter.clone(),
            state,
            full_rebuilds: 1,
            incremental_updates: 0,
        }
    }

    pub fn state(&self) -> InboxState {
        self.state.clone()
    }

    pub fn filter(&self) -> &InboxFilter {
        &self.filter
    }

    pub fn active_virtual_window(&self) -> VirtualWindow {
        self.task_list.virtual_window()
    }

    pub fn archived_virtual_window(&self) -> VirtualWindow {
        self.archived_list.virtual_window()
    }

    pub fn active_overflow(&self) -> Option<InboxOverflow> {
        self.task_list.overflow()
    }

    pub fn archived_overflow(&self) -> Option<InboxOverflow> {
        self.archived_list.overflow()
    }

    pub fn set_active_viewport(
        &mut self,
        first_visible: usize,
        visible_rows: usize,
    ) -> Result<(), ViewportError> {
        self.task_list.set_viewport(first_visible, visible_rows)
    }

    pub fn set_archived_viewport(
        &mut self,
        first_visible: usize,
        visible_rows: usize,
    ) -> Result<(), ViewportError> {
        self.archived_list.set_viewport(first_visible, visible_rows)
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

    pub fn overflow(&self) -> Option<InboxOverflow> {
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
            let range = self
                .archived_list
                .virtual_window()
                .render_range(self.history_rows.len());
            items.extend(
                self.history_rows[range]
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
    indexed_total_count: usize,
) -> (Vec<TaskRowModel>, usize) {
    (
        project_indexed_page(model, task_ids, filter, unread, read_only),
        indexed_total_count,
    )
}

fn project_indexed_page(
    model: &ClientModel,
    task_ids: &[TaskId],
    filter: &InboxFilter,
    unread: &UnreadCursor,
    read_only: bool,
) -> Vec<TaskRowModel> {
    // The index already resolved exact matches and provides deterministic
    // ordering. Consume only its bounded first page; never walk the complete
    // ClientModel from the render/search or incremental-update path.
    task_ids
        .iter()
        .take(MAX_TASK_LIST_ITEMS)
        .filter_map(|task_id| {
            let snapshot = model.tasks().get(task_id)?;
            if !filter.matches(&snapshot.task.title, snapshot.task.lifecycle) {
                return None;
            }
            Some(row_from_snapshot(
                snapshot,
                model
                    .task_last_occurred_at_ms(*task_id)
                    .unwrap_or(snapshot.task.created_at_ms),
                unread,
                read_only,
            ))
        })
        .collect()
}

fn row_from_snapshot(
    snapshot: &crate::domain::snapshot::TaskSnapshot,
    occurred_at_ms: i64,
    unread: &UnreadCursor,
    read_only: bool,
) -> TaskRowModel {
    let status = snapshot.visible_status();
    let mut display = display_for_snapshot(snapshot);
    display.display_truncated |=
        text_was_truncated(&snapshot.task.title, MAX_ACCESSIBLE_NAME_CHARS);
    TaskRowModel {
        task_id: snapshot.task.id,
        title: sanitize_bounded_text(&snapshot.task.title, MAX_ACCESSIBLE_NAME_CHARS),
        lifecycle: snapshot.task.lifecycle,
        connectivity: snapshot.connectivity,
        attention: snapshot.attention,
        activity: snapshot.activity,
        review_readiness: snapshot.review_readiness,
        status,
        unread_event_count: unread.unread_count(snapshot.task.id),
        revision: snapshot.task.revision,
        created_at_ms: snapshot.task.created_at_ms,
        occurred_at_ms,
        section: section_for(status),
        display,
        read_only,
    }
}

fn display_for_snapshot(snapshot: &crate::domain::snapshot::TaskSnapshot) -> TaskRowDisplay {
    let (project, worktree, workspace_path_hidden, worktree_truncated) = match &snapshot
        .task
        .workspace
    {
        WorkspaceRef::Main | WorkspaceRef::MainWithFingerprint { .. } => {
            ("Project".to_string(), "main".to_string(), false, false)
        }
        WorkspaceRef::Worktree { branch, .. }
        | WorkspaceRef::WorktreeWithFingerprint { branch, .. } => (
            "Project".to_string(),
            sanitize_bounded_text(branch, MAX_WORKTREE_LABEL_CHARS),
            true,
            text_was_truncated(branch, MAX_WORKTREE_LABEL_CHARS),
        ),
        WorkspaceRef::External { .. } | WorkspaceRef::ExternalWithFingerprint { .. } => {
            ("Project".to_string(), "external".to_string(), true, false)
        }
        WorkspaceRef::HostBound { binding } => match binding.kind() {
            WorkspaceBindingKind::Main => ("Project".to_string(), "main".to_string(), false, false),
            WorkspaceBindingKind::Worktree => {
                let branch = binding.branch().unwrap_or("worktree");
                (
                    "Project".to_string(),
                    sanitize_bounded_text(branch, MAX_WORKTREE_LABEL_CHARS),
                    true,
                    text_was_truncated(branch, MAX_WORKTREE_LABEL_CHARS),
                )
            }
            WorkspaceBindingKind::External => {
                ("Project".to_string(), "external".to_string(), true, false)
            }
        },
    };
    let (primary_provider, runtime, provider_truncated) = snapshot
        .primary_agent_id
        .and_then(|agent_id| snapshot.agents.get(&agent_id))
        .map(|agent| {
            let icon = match agent.provider_kind {
                crate::providers::ProviderKind::ClaudeCode => PrimaryProviderIcon::Claude,
                crate::providers::ProviderKind::Codex => PrimaryProviderIcon::Codex,
                crate::providers::ProviderKind::Cursor => PrimaryProviderIcon::Cursor,
            };
            (
                PrimaryProviderState::Present {
                    icon,
                    // Keep only the allowlisted provider identity in the UI
                    // model. Account/session labels are never retained here.
                    kind: icon.label().to_string(),
                },
                RuntimeSummary::Present {
                    lifecycle: agent.lifecycle,
                    generation: agent.runtime_generation,
                },
                false,
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
    let provider_icon = row.display.primary_provider.icon_label();
    let runtime = row.display.runtime.label();
    let resources = row.display.resources.compact_label();
    let provider_summary = if provider == provider_icon {
        provider.to_string()
    } else {
        format!("{provider_icon} · {provider}")
    };
    let secondary_text = match width {
        InboxPresentationWidth::Narrow => {
            format!(
                "{} · {} · {}",
                status_label(row.status),
                provider_summary,
                runtime
            )
        }
        InboxPresentationWidth::Regular => format!(
            "{} · {} · {} · {} · {} · {}",
            row.display.project,
            row.display.worktree,
            provider_summary,
            runtime,
            status_label(row.status),
            resources
        ),
    };
    let read_only_suffix = row.read_only.then_some(" · read-only").unwrap_or_default();
    let title = sanitize_bounded_text(&row.title, MAX_ACCESSIBLE_NAME_CHARS);
    let mut announcements = Vec::new();
    if row.read_only {
        announcements.push("Archived task is read-only; actions unavailable.");
    }
    if row.display.workspace_path_hidden {
        announcements.push("Workspace path hidden for privacy.");
    }
    if row.display.display_truncated {
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
    let accessible_name = sanitize_bounded_text(
        &format!(
            "{} · {}{}",
            title,
            status_label(row.status),
            read_only_suffix
        ),
        MAX_ACCESSIBLE_NAME_CHARS,
    );
    let accessible_description = sanitize_bounded_text(
        &format!(
            "Project {} · workspace {} · provider icon {} ({}) · runtime {} · state {} · {}{}",
            row.display.project,
            row.display.worktree,
            provider_icon,
            provider,
            runtime,
            status_label(row.status),
            unread,
            announcement
        ),
        MAX_ACCESSIBLE_DESCRIPTION_CHARS,
    );
    let mut accessibility = AccessibilityMetadata::new(
        if row.read_only {
            AccessibleRole::Region
        } else {
            AccessibleRole::Button
        },
        accessible_name.clone(),
    )
    .expect("bounded inbox accessibility name");
    accessibility
        .set_description(accessible_description.clone())
        .expect("bounded inbox accessibility description");
    let failed = matches!(
        row.status,
        VisibleTaskStatus::Failed | VisibleTaskStatus::UncertainOutcome
    );
    accessibility
        .set_error(failed.then(|| status_label(row.status).to_string()))
        .expect("bounded inbox accessibility error");
    accessibility.set_disabled(row.read_only);
    accessibility.set_busy(matches!(
        row.status,
        VisibleTaskStatus::Working | VisibleTaskStatus::Settling
    ));
    accessibility.set_focused(false);
    accessibility.set_invalid(failed);
    accessibility.set_read_only(row.read_only);
    accessibility.set_value(Some(status_label(row.status).to_string()));
    InboxRenderRow {
        key: if row.read_only {
            InboxItemKey::HistoryRow(row.task_id)
        } else {
            InboxItemKey::Row(row.task_id)
        },
        task_id: row.task_id,
        revision: row.revision,
        title,
        secondary_text: sanitize_bounded_text(&secondary_text, MAX_SECONDARY_LABEL_CHARS),
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

/// Presentation ingress is a privacy and accessibility boundary. Control
/// characters, bidi overrides, and path separators are never copied into
/// native labels; all output is bounded by Unicode scalar count.
fn sanitize_bounded_text(value: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let mut output = String::new();
    let mut retained = 0;
    let mut truncated = false;
    for ch in value.chars() {
        if ch.is_control() || matches!(ch, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') {
            continue;
        }
        if retained == max_chars {
            truncated = true;
            break;
        }
        output.push(if matches!(ch, '\\' | '/') { '·' } else { ch });
        retained += 1;
    }
    if truncated && max_chars > 1 {
        output.pop();
        output.push('…');
    }
    output
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
    let left_title = normalize_bounded_search_text(&left.title, MAX_ACCESSIBLE_NAME_CHARS).0;
    let right_title = normalize_bounded_search_text(&right.title, MAX_ACCESSIBLE_NAME_CHARS).0;
    attention_rank(left.status)
        .cmp(&attention_rank(right.status))
        .then_with(|| right.occurred_at_ms.cmp(&left.occurred_at_ms))
        .then_with(|| right.revision.cmp(&left.revision))
        .then_with(|| left_title.cmp(&right_title))
        .then_with(|| left.title.cmp(&right.title))
        .then_with(|| left.task_id.cmp(&right.task_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::ClientModelBuilder;
    use crate::domain::id::{EnvironmentId, ProjectId, SnapshotId};
    use crate::domain::snapshot::{SnapshotItem, SnapshotPage, SnapshotSection, TaskSnapshotItem};
    use crate::domain::task::{
        ReviewReadiness, TaskActivity, TaskAssignment, TaskAttention, TaskConnectivity, TaskFacts,
        TaskLifecycle, WorkspaceRef,
    };
    use std::time::Duration;

    fn search_model(task_count: u64) -> ClientModel {
        search_model_with_title_prefix(task_count, "Task")
    }

    fn search_model_with_title_prefix(task_count: u64, title_prefix: &str) -> ClientModel {
        let snapshot_id = SnapshotId::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x01,
        ])
        .expect("snapshot");
        let tasks = (0..task_count)
            .map(|index| {
                let id = crate::domain::id::TaskId::from_bytes({
                    let mut bytes = [0u8; 16];
                    bytes[..8].copy_from_slice(&[0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01]);
                    bytes[8] = 0x80;
                    bytes[9..].copy_from_slice(&index.to_be_bytes()[1..]);
                    bytes
                })
                .expect("task");
                SnapshotItem::Task(TaskSnapshotItem {
                    task: TaskFacts {
                        id,
                        environment_id: EnvironmentId::from_bytes([
                            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00,
                            0x00, 0x00, 0x00, 0x11,
                        ])
                        .expect("environment"),
                        title: format!("{title_prefix} {index}"),
                        description: None,
                        project_id: ProjectId::from_bytes([
                            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00,
                            0x00, 0x00, 0x00, 0x12,
                        ])
                        .expect("project"),
                        workspace: WorkspaceRef::Main,
                        assignment: TaskAssignment::LocalOwner,
                        lifecycle: TaskLifecycle::Open,
                        action_epoch: 0,
                        revision: index,
                        created_at_ms: index as i64,
                    },
                    connectivity: TaskConnectivity::Connected,
                    attention: TaskAttention::None,
                    activity: TaskActivity::Idle,
                    review_readiness: ReviewReadiness::NotReady,
                    primary_agent_id: None,
                })
            })
            .collect::<Vec<_>>();
        let mut builder = ClientModelBuilder::new();
        for (section, items) in [
            (SnapshotSection::Tasks, tasks),
            (SnapshotSection::AgentSessions, Vec::new()),
            (SnapshotSection::Artifacts, Vec::new()),
            (SnapshotSection::Resources, Vec::new()),
            (SnapshotSection::Operations, Vec::new()),
        ] {
            builder
                .ingest_page(SnapshotPage {
                    snapshot_id,
                    through_sequence: 1,
                    section,
                    after_item: None,
                    items,
                    encoded_bytes: 1,
                    next_cursor: None,
                })
                .expect("snapshot page");
        }
        builder.finish().expect("client model")
    }

    #[test]
    fn row_action_capture_retains_revision_runtime_epochs_and_read_only_facts() {
        let model = search_model(1);
        let snapshot = model.tasks().values().next().expect("task snapshot");
        let mut row = row_from_snapshot(snapshot, 7, &UnreadCursor::default(), true);
        row.display.runtime = RuntimeSummary::Present {
            lifecycle: crate::domain::agent::AgentSessionLifecycle::Open,
            generation: 17,
        };
        let capture = capture_row_action(
            &row,
            InboxActionEpochs {
                navigation_epoch: 11,
                focus_epoch: 12,
            },
        );

        assert_eq!(capture.task_id, row.task_id);
        assert_eq!(capture.row_revision, row.revision);
        assert_eq!(capture.runtime_generation, Some(17));
        assert_eq!(capture.navigation_epoch, 11);
        assert_eq!(capture.focus_epoch, 12);
        assert!(capture.read_only);
    }

    #[test]
    fn background_search_worker_publishes_one_requested_page_then_returns() {
        let model = Arc::new(search_model(10_001));
        let first = model.search_task_ids_page("task", false, None);
        let continuation = first
            .continuation()
            .cloned()
            .expect("fixture must require a continuation");
        let request = BackgroundSearchRequest {
            generation: 7,
            archived: false,
            continuation: Some(continuation),
        };
        let cancellation = Arc::new(AtomicU64::new(7));
        let (results_tx, results_rx) = mpsc::sync_channel(1);

        run_background_search_page(
            model,
            InboxFilter::new("task"),
            request,
            cancellation,
            results_tx,
        );

        let result = results_rx.try_recv().expect("one page result");
        assert_eq!(result.page.ids.len(), MAX_TASK_LIST_ITEMS);
        assert!(
            results_rx.try_recv().is_err(),
            "one worker invocation must not prequeue a second continuation"
        );
    }

    #[test]
    fn background_search_worker_discards_cancelled_generation_before_publish() {
        let model = Arc::new(search_model(10_001));
        let cancellation = Arc::new(AtomicU64::new(8));
        let (results_tx, results_rx) = mpsc::sync_channel(1);

        run_background_search_page(
            model,
            InboxFilter::new("task"),
            BackgroundSearchRequest {
                generation: 7,
                archived: false,
                continuation: None,
            },
            cancellation,
            results_tx,
        );

        assert!(results_rx.try_recv().is_err());
    }

    #[test]
    fn runtime_requests_one_page_only_after_the_previous_page_is_published() {
        let model = Arc::new(search_model(10_001));
        let mut runtime = InboxRuntime::new();
        runtime.background_model = Some(Arc::clone(&model));
        runtime.filter = InboxFilter::new("task");
        runtime.projection_stale = false;
        runtime.projection = Some(Inbox::from_model_with_search_pages(
            &model,
            &runtime.filter,
            &runtime.unread,
            SearchPage::pending(),
            None,
        ));

        assert!(runtime.request_background_search_page());
        assert!(runtime.background_search_pending());
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !runtime.poll_background_search() {
            assert!(
                std::time::Instant::now() < deadline,
                "one bounded page must become available"
            );
            thread::yield_now();
        }
        assert!(!runtime.background_search_pending());
        assert_eq!(
            runtime
                .projection()
                .expect("published first page")
                .active_rows()
                .len(),
            MAX_TASK_LIST_ITEMS
        );

        let progress = runtime.tick_background_search();
        assert!(progress.requested, "the next page is explicit tick demand");
        assert!(runtime.background_search_pending());
        assert!(
            !runtime.tick_background_search().requested,
            "a paused page cannot prequeue another worker"
        );
    }

    #[test]
    fn runtime_filter_change_cancels_and_cannot_publish_an_old_generation() {
        let model = Arc::new(search_model(10_001));
        let mut runtime = InboxRuntime::new();
        runtime.background_model = Some(Arc::clone(&model));
        runtime.filter = InboxFilter::new("task");
        runtime.projection_stale = false;
        runtime.projection = Some(Inbox::from_model_with_search_pages(
            &model,
            &runtime.filter,
            &runtime.unread,
            SearchPage::pending(),
            None,
        ));
        assert!(runtime.request_background_search_page());

        runtime.set_filter(InboxFilter::new("different"));

        assert!(
            runtime.background_search_pending(),
            "the replacement generation may have one page in flight"
        );
        assert_eq!(runtime.filter().query(), "different");
        assert_eq!(
            runtime
                .projection()
                .expect("new filter placeholder")
                .active_rows()
                .len(),
            0
        );
        runtime.cancel_background_search();
    }

    #[test]
    fn cancelled_search_retains_worker_ownership_until_the_worker_exits() {
        let subscription = Arc::new(Mutex::new(ClientSubscription::new()));
        let model = search_model(1);
        let mut runtime = InboxRuntime::new();
        runtime.live_subscription = Some(Arc::clone(&subscription));
        runtime.filter = InboxFilter::new("task");
        runtime.projection_stale = false;
        runtime.projection = Some(Inbox::from_model_with_search_pages(
            &model,
            &runtime.filter,
            &runtime.unread,
            SearchPage::pending(),
            None,
        ));

        let guard = subscription.lock().expect("hold subscription lock");
        assert!(runtime.request_background_search_page());
        runtime.cancel_background_search();

        assert!(
            !runtime.background_search_pending(),
            "a worker waiting for the borrowed subscription must observe cancellation"
        );
        assert_eq!(runtime.background_search_state(), SearchWorkerState::Idle);
        drop(guard);
    }

    #[test]
    fn cancelled_search_retains_owned_worker_when_join_budget_expires() {
        let (release_tx, release_rx) = mpsc::sync_channel(0);
        let (results_tx, results_rx) = mpsc::sync_channel(1);
        let cancellation = Arc::new(AtomicU64::new(4));
        let join = thread::spawn(move || {
            let _ = release_rx.recv();
        });
        let mut runtime = InboxRuntime::new();
        runtime.background_worker = Some(BackgroundSearchWorker {
            cancellation: Arc::clone(&cancellation),
            results: results_rx,
            join: Some(join),
            retiring: false,
        });

        runtime.cancel_background_search();
        assert!(runtime.background_search_pending());
        assert_eq!(
            runtime.background_search_state(),
            SearchWorkerState::Retiring
        );
        assert_ne!(cancellation.load(AtomicOrdering::Acquire), 4);

        release_tx.send(()).expect("release exact worker");
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while runtime.background_search_pending() {
            assert!(
                std::time::Instant::now() < deadline,
                "retiring worker must exit"
            );
            runtime.poll_background_search();
            thread::yield_now();
        }
        assert_eq!(runtime.background_search_state(), SearchWorkerState::Idle);
        drop(results_tx);
    }

    #[test]
    fn dropping_runtime_cancels_a_worker_waiting_on_the_borrowed_subscription() {
        let subscription = Arc::new(Mutex::new(ClientSubscription::new()));
        let guard = subscription.lock().expect("hold subscription lock");
        let start = std::time::Instant::now();
        {
            let model = search_model(1);
            let mut runtime = InboxRuntime::new();
            runtime.live_subscription = Some(Arc::clone(&subscription));
            runtime.filter = InboxFilter::new("task");
            runtime.projection_stale = false;
            runtime.projection = Some(Inbox::from_model_with_search_pages(
                &model,
                &runtime.filter,
                &runtime.unread,
                SearchPage::pending(),
                None,
            ));
            assert!(runtime.request_background_search_page());
        }
        assert!(
            start.elapsed() < Duration::from_millis(250),
            "Drop must not wait on a borrowed subscription lock"
        );
        drop(guard);
    }

    #[test]
    fn dropping_runtime_hands_an_unfinished_worker_to_the_owned_reaper() {
        let (release_tx, release_rx) = mpsc::sync_channel(0);
        let settled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let settled_for_worker = Arc::clone(&settled);
        let join = thread::spawn(move || {
            let _ = release_rx.recv();
            settled_for_worker.store(true, AtomicOrdering::Release);
        });
        let baseline = background_reaper_pending_for_test();
        let mut runtime = InboxRuntime::new();
        runtime.background_worker = Some(BackgroundSearchWorker {
            cancellation: Arc::new(AtomicU64::new(0)),
            results: mpsc::sync_channel(1).1,
            join: Some(join),
            retiring: false,
        });

        drop(runtime);
        assert!(
            background_reaper_pending_for_test() > baseline,
            "an unfinished worker must remain owned by the bounded reaper after runtime drop"
        );

        release_tx.send(()).expect("release worker");
        let deadline = Instant::now() + Duration::from_secs(1);
        while background_reaper_pending_for_test() > baseline {
            assert!(
                Instant::now() < deadline,
                "owned reaper must settle the worker"
            );
            thread::yield_now();
        }
        assert!(settled.load(AtomicOrdering::Acquire));
    }

    #[test]
    fn runtime_keeps_partial_overflow_truth_until_exact_total_arrives() {
        let model = Arc::new(search_model_with_title_prefix(100_000, "aaaaaaaaa"));
        let mut runtime = InboxRuntime::new();
        runtime.background_model = Some(Arc::clone(&model));
        runtime.filter = InboxFilter::new("aaaaaaaaa");
        runtime.projection_stale = false;
        runtime.projection = Some(Inbox::from_model_with_search_pages(
            &model,
            &runtime.filter,
            &runtime.unread,
            SearchPage::pending(),
            None,
        ));

        assert!(runtime.request_background_search_page());
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        while !runtime.poll_background_search() {
            assert!(
                std::time::Instant::now() < deadline,
                "the first bounded page must become available"
            );
            thread::yield_now();
        }
        assert_eq!(
            runtime
                .projection()
                .expect("first page projection")
                .active_overflow()
                .expect("partial search must expose overflow")
                .total_count,
            MAX_TASK_LIST_ITEMS + 1
        );

        while !runtime.tick_background_search().complete {
            assert!(
                std::time::Instant::now() < deadline,
                "bounded continuation must eventually reach its exact total"
            );
            thread::yield_now();
        }
        assert_eq!(
            runtime
                .projection()
                .expect("complete search projection")
                .active_overflow()
                .expect("100000 results still exceed the retained window")
                .total_count,
            100_000
        );
    }
}
