//! Pure, bounded projection for the Task Cockpit inbox.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BinaryHeap};
use std::ops::Range;
use std::sync::Arc;

use gpui::{
    div, AnyElement, App, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    ParentElement, Styled, Window,
};
use serde::{Deserialize, Serialize};

use crate::client::ClientModel;
use crate::client::{ClientSubscription, SubscriptionError, SubscriptionUpdate};
use crate::domain::agent::AgentSessionLifecycle;
use crate::domain::event::DomainEvent;
use crate::domain::id::TaskId;
use crate::domain::resource::{ResourceKind, ResourceLifecycle};
use crate::domain::task::{
    ReviewReadiness, TaskActivity, TaskAttention, TaskConnectivity, TaskLifecycle,
    VisibleTaskStatus, WorkspaceRef,
};
use crate::ui::components::{AccessibilityMetadata, AccessibleRole};

pub const MAX_TASK_LIST_ITEMS: usize = 5_000;
pub const FIXED_VIRTUAL_OVERSCAN: usize = 32;
pub const DEFAULT_VISIBLE_ROWS: usize = 40;

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

    /// A compact, versioned durable representation suitable for session state.
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
    pub fn new(query: impl Into<String>) -> Self {
        Self {
            query: sanitize_bounded_text(&query.into(), MAX_SEARCH_CHARS),
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
        let query = self.query.trim().to_lowercase();
        query.is_empty()
            || sanitize_bounded_text(title, MAX_SEARCH_CHARS)
                .to_lowercase()
                .contains(&query)
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

/// Native-shell row action bridge. The row identity is supplied by the
/// renderer, while the shell callback performs the generation/epoch checks at
/// execution time. The callback is optional so the pure projection renderer
/// remains usable by previews and tests without an application entity.
pub type InboxRowMouseDownHandler =
    Arc<dyn Fn(TaskId, &MouseDownEvent, &mut Window, &mut App) + 'static>;

/// The production native-shell bridge. A shell owns one subscription and one
/// projection; no legacy task-list cache is permitted beside it. Host IO is
/// caller-driven, while this object keeps the durable cursor and performs the
/// small projection update synchronously after each applied event.
#[derive(Debug, Default)]
pub struct InboxRuntime {
    subscription: Option<ClientSubscription>,
    unread: UnreadCursor,
    filter: InboxFilter,
    projection: Option<Inbox>,
    projection_updates: u64,
}

impl InboxRuntime {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn attach_subscription(&mut self, subscription: ClientSubscription) {
        self.subscription = Some(subscription);
        self.rebuild_projection();
    }

    pub fn subscription(&self) -> Option<&ClientSubscription> {
        self.subscription.as_ref()
    }

    pub fn subscription_mut(&mut self) -> Option<&mut ClientSubscription> {
        self.subscription.as_mut()
    }

    pub fn projection(&self) -> Option<&Inbox> {
        self.projection.as_ref()
    }

    pub fn unread_cursor(&self) -> &UnreadCursor {
        &self.unread
    }

    pub fn restore_unread_cursor(&mut self, cursor: UnreadCursor) {
        self.unread = cursor;
        self.rebuild_projection();
    }

    pub fn restore_unread_cursor_bytes(&mut self, bytes: &[u8]) -> Result<(), String> {
        let cursor = UnreadCursor::decode_durable(bytes)?;
        self.restore_unread_cursor(cursor);
        Ok(())
    }

    /// Refresh after the caller's async subscription pump has applied a live
    /// event. This keeps the GPUI render path deterministic and allocation-free
    /// with respect to transport state.
    pub fn refresh_from_subscription(&mut self) {
        self.rebuild_projection();
    }

    pub fn encode_unread_cursor(&self) -> Result<Vec<u8>, String> {
        self.unread.encode_durable()
    }

    pub fn set_filter(&mut self, filter: InboxFilter) {
        self.filter = filter;
        self.rebuild_projection();
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
                let observed = self.unread.observe_durable_event(&event);
                if !observed {
                    // ClientSubscription already applied this event (or the
                    // durable cursor has seen it). Do not rebuild the
                    // projection for an idempotent replay.
                    return Ok(false);
                }
                let unread = self.unread.clone();
                if let (Some(subscription), Some(projection)) =
                    (self.subscription.as_ref(), self.projection.as_mut())
                {
                    if let Some(model) = subscription.model() {
                        projection.set_unread_cursor(unread);
                        projection.apply_model_event(model, event.task_id);
                        self.projection_updates = self.projection_updates.saturating_add(1);
                    }
                } else {
                    self.rebuild_projection();
                }
                Ok(observed)
            }
            SubscriptionUpdate::ResyncRequired { .. } => {
                self.rebuild_projection();
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
        self.projection = self.subscription.as_ref().and_then(|subscription| {
            subscription
                .model()
                .map(|model| Inbox::from_model_with_filter(model, &self.filter, &self.unread))
        });
        self.projection_updates = self.projection_updates.saturating_add(1);
    }
}

/// Minimal GPUI renderer for the native shell. It consumes only the bounded
/// render model, so it cannot reach back into a provider, runtime, path, or
/// terminal while painting a frame.
pub fn render_native_inbox(model: &InboxRenderModel) -> AnyElement {
    render_native_inbox_with_actions(model, None)
}

pub fn render_native_inbox_with_actions(
    model: &InboxRenderModel,
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
                    let task_id = row.task_id;
                    element = element.on_mouse_down(MouseButton::Left, move |event, window, cx| {
                        // A task row owns the pointer gesture. Stopping
                        // propagation here prevents the shell's terminal
                        // surface from interpreting the same click after a
                        // row was reordered or rejected by its action fence.
                        cx.stop_propagation();
                        handler(task_id, event, window, cx);
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
    task_list: InboxList,
    rows: Vec<TaskRowModel>,
    archived_list: InboxList,
    history_rows: Vec<TaskRowModel>,
    unread: UnreadCursor,
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
        let active_ids = if filter.query().trim().is_empty() {
            model
                .task_projection_index()
                .top_active_task_ids(MAX_TASK_LIST_ITEMS)
        } else {
            model.task_projection_index().active_task_ids()
        };
        let (rows, total_count) = project_rows(
            model,
            &active_ids,
            filter,
            unread,
            false,
            model.task_projection_index().active_count(),
        );
        let (history_rows, history_total_count) = if filter.includes_archived() {
            let archived_ids = if filter.query().trim().is_empty() {
                model
                    .task_projection_index()
                    .top_archived_task_ids(MAX_TASK_LIST_ITEMS)
            } else {
                model.task_projection_index().archived_task_ids()
            };
            project_rows(
                model,
                &archived_ids,
                filter,
                unread,
                true,
                model.task_projection_index().archived_count(),
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
        }
    }

    pub fn from_error(error: InboxError) -> Self {
        Self::from_projection(
            Err(error),
            &InboxFilter::default(),
            &UnreadCursor::default(),
        )
    }

    /// Apply the bounded projection delta after `ClientModel` has accepted one
    /// durable event. The keyed ClientModel index supplies only the retained
    /// top window, so this work is bounded by the UI retention cap rather than
    /// the total task count. A query filter may still require its explicit
    /// search scan, but it never becomes the default hot path.
    pub fn apply_model_event(&mut self, model: &ClientModel, _task_id: Option<TaskId>) {
        let next = Self::from_model_with_filter(model, &self.filter, &self.unread);
        *self = next;
    }

    pub fn set_unread_cursor(&mut self, unread: UnreadCursor) {
        self.unread = unread;
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
    if filter.query().trim().is_empty() {
        let total_count = indexed_total_count;
        let rows = task_ids
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
        let row = row_from_snapshot(
            snapshot,
            model
                .task_last_occurred_at_ms(*task_id)
                .unwrap_or(snapshot.task.created_at_ms),
            unread,
            read_only,
        );
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
    let (project, worktree, workspace_path_hidden, worktree_truncated) =
        match &snapshot.task.workspace {
            WorkspaceRef::Main => ("Project".to_string(), "main".to_string(), false, false),
            WorkspaceRef::Worktree { branch, .. } => (
                "Project".to_string(),
                sanitize_bounded_text(branch, MAX_WORKTREE_LABEL_CHARS),
                true,
                text_was_truncated(branch, MAX_WORKTREE_LABEL_CHARS),
            ),
            WorkspaceRef::External { .. } => {
                ("Project".to_string(), "external".to_string(), true, false)
            }
        };
    let (primary_provider, runtime, provider_truncated) = snapshot
        .primary_agent_id
        .and_then(|agent_id| snapshot.agents.get(&agent_id))
        .map(|agent| {
            let icon = if agent.provider_kind.eq_ignore_ascii_case("claude")
                || agent.provider_kind.eq_ignore_ascii_case("claude_code")
            {
                PrimaryProviderIcon::Claude
            } else if agent.provider_kind.eq_ignore_ascii_case("codex") {
                PrimaryProviderIcon::Codex
            } else if agent.provider_kind.eq_ignore_ascii_case("cursor")
                || agent.provider_kind.eq_ignore_ascii_case("cursor_cli")
            {
                PrimaryProviderIcon::Cursor
            } else {
                PrimaryProviderIcon::Other
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
    attention_rank(left.status)
        .cmp(&attention_rank(right.status))
        .then_with(|| right.occurred_at_ms.cmp(&left.occurred_at_ms))
        .then_with(|| right.revision.cmp(&left.revision))
        .then_with(|| left.title.to_lowercase().cmp(&right.title.to_lowercase()))
        .then_with(|| left.title.cmp(&right.title))
        .then_with(|| left.task_id.cmp(&right.task_id))
}
