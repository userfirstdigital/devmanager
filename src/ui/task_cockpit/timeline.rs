//! Virtualized semantic timeline over the sealed journal projection.
//!
//! Production rows come only from [`SemanticJournalView`]. Caller-supplied
//! raw `SemanticEvent` arrays are not part of the public API.

use gpui::{
    div, list, px, AnyElement, App, InteractiveElement, IntoElement, ListAlignment, ListOffset,
    ListState, ParentElement, StatefulInteractiveElement, Styled, Window,
};
use std::cell::Cell;
use std::collections::BTreeSet;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::rc::Rc;

use crate::client::model::ClientModel;
use crate::domain::id::{OperationId, TaskId};
use crate::domain::PlanStepStatus;
use crate::protocol::CapabilitySet;
use crate::ui::conversation::render::{conversation_row_element, conversation_row_height};
use crate::ui::conversation::rows::{
    apply_activity_collapse, conversation_row_key, derive_conversation_rows,
    stable_conversation_rows, ConversationRow, ConversationRowKey, ConversationVerbosity,
};
use crate::ui::renderers::{
    inspect_operation, live_target, CapturedActionTarget, JournalAvailability, RenderModelError,
    RendererRegistry, SemanticJournalView, TimelineActivation, TimelineItemId, TimelineItemModel,
};
use crate::ui::tokens::ThemeTokens;

#[cfg(debug_assertions)]
use crate::domain::EventId;
#[cfg(debug_assertions)]
use crate::ui::components::interaction::{AccessibilityMetadata, AccessibleRole};
#[cfg(debug_assertions)]
use crate::ui::renderers::{
    InteractionEligibility, PlanView, RendererSelection, SemanticKind, TimelineItemContent,
};

pub const DEFAULT_OVERSCAN: usize = 4;
pub const MAX_PAINTED_ROWS: usize = 48;
/// Shared readable measure for the conversation column and floating composer.
pub const CONVERSATION_CONTENT_MAX_WIDTH: f32 = 768.0;
/// Follow re-arm band above the true content bottom. Strict on purpose: a
/// half-viewport "near end" test re-arms live-follow while the user is reading
/// history and yanks them back down on the next streamed chunk.
pub const FOLLOW_REARM_THRESHOLD_PX: u32 = 40;

pub type ActivityToggleHandler = Rc<dyn Fn(String, &mut App)>;

#[cfg(debug_assertions)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewPlanStep {
    pub step_id: String,
    pub title: String,
    pub status: String,
}

fn timeline_row_element(
    row: &ConversationRow,
    tokens: ThemeTokens,
    activity_toggle: Option<ActivityToggleHandler>,
) -> AnyElement {
    let visual = conversation_row_element(row, tokens);
    let ConversationRow::ActivityToggle { group, .. } = row else {
        return visual;
    };
    let Some(activity_toggle) = activity_toggle else {
        return visual;
    };
    let group = group.clone();
    let mut element_hasher = DefaultHasher::new();
    group.hash(&mut element_hasher);
    let element_key = element_hasher.finish();
    div()
        .id(("native-conversation-activity-toggle", element_key))
        .tab_stop(true)
        .cursor_pointer()
        .on_click(move |_event, _window, app| activity_toggle(group.clone(), app))
        .child(visual)
        .into_any_element()
}

/// Row heights are estimated at a fixed baseline density/scale, never the
/// live user theme. Threading the real `ThemeTokens` into this path would
/// mean plumbing it through `TaskCockpitShell::follow_projection` and every
/// mutation-time caller of it in `native_shell.rs` (a single caller today,
/// itself reached from many `sync_cockpit_follow` call sites with no `cx`
/// or theme snapshot in scope) -- none of which currently carry theme
/// state; only paint-time `surface(tokens)` calls do. That is a much wider,
/// less certain conversion than this task's row-indexing fix, so this
/// baseline is scoped to scroll/virtualization bookkeeping only. Nothing
/// visible depends on it: the actual paint in `surface()` always uses the
/// caller's live tokens.
fn height_estimation_tokens() -> ThemeTokens {
    crate::ui::tokens::theme(
        crate::ui::tokens::ThemeMode::Dark,
        crate::ui::tokens::Density::Comfortable,
        crate::ui::tokens::Scale::Scale100,
    )
}

pub(crate) fn conversation_activity_summary(
    items: &[TimelineItemModel],
    status: &str,
) -> Option<String> {
    use crate::ui::renderers::TimelineItemContent;

    let mut activity = ActivityCounts::default();
    for item in items {
        match &item.content {
            TimelineItemContent::Tool(view) => {
                activity.observe_tool(&view.tool_id, &view.name, &view.state);
            }
            TimelineItemContent::Plan(view) => {
                activity.observe_plan(view.step_id.as_deref(), &view.status)
            }
            _ => {}
        }
    }

    let mut parts = Vec::new();
    if !activity.running_commands.is_empty() {
        if !activity.running_shells.is_empty() {
            parts.push(format!(
                "Running {} shell command(s)",
                activity.running_shells.len()
            ));
        }
        let other_tools = activity
            .running_commands
            .len()
            .saturating_sub(activity.running_shells.len());
        if other_tools > 0 {
            parts.push(format!("{other_tools} other tool(s) running"));
        }
    }
    if !activity.active_subagents.is_empty() {
        parts.push(format!(
            "{} subagent(s) active",
            activity.active_subagents.len()
        ));
    }
    if !activity.open_task_steps.is_empty() {
        parts.push(format!(
            "Goal active · {} task step(s) open",
            activity.open_task_steps.len()
        ));
    }
    if parts.is_empty() {
        return None;
    }
    let mut summary = vec![status.to_string()];
    summary.append(&mut parts);
    Some(summary.join(" · "))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimelineViewport {
    pub height: u32,
    pub scroll_offset: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TimelineAnchor {
    key: ConversationRowKey,
    offset_within: u32,
}

#[derive(Debug, Clone)]
pub struct Timeline {
    task_id: TaskId,
    availability: JournalAvailability,
    items: Vec<TimelineItemModel>,
    rows: Rc<Vec<ConversationRow>>,
    /// Cached on admission/reproject; render must not rescan full history.
    activity_summary: Option<String>,
    expanded_activity: Vec<String>,
    prefix: Vec<u32>,
    content_height: u32,
    viewport: TimelineViewport,
    /// Live-follow intent. Kept in a cell so the GPUI list scroll handler can
    /// detach without requiring an entity update path. ListState is the
    /// production authority; this mirrors `!is_scrolled` for Bottom lists.
    following: Rc<Cell<bool>>,
    list_state: ListState,
    /// High-water of the journal page last projected into this timeline.
    projected_high_water: u64,
    projected_capabilities: Option<CapabilitySet>,
    projected_target: Option<CapturedActionTarget>,
    projected_task_revision: Option<u64>,
    anchor: Option<TimelineAnchor>,
    paint_start: usize,
    paint_end: usize,
    captured_target: CapturedActionTarget,
}

impl Timeline {
    pub fn project(
        model: &ClientModel,
        task_id: TaskId,
        capabilities: CapabilitySet,
        journal: &SemanticJournalView,
        registry: &RendererRegistry,
        viewport: TimelineViewport,
    ) -> Result<Self, RenderModelError> {
        assert_task_projection(model, task_id, journal)?;
        let captured_target = live_target(model, task_id)?;
        let items = journal.project_items(registry, capabilities)?;
        Ok(Self::assemble(
            task_id,
            journal.availability(),
            items,
            viewport,
            captured_target,
        ))
    }

    /// Shared by [`Self::project`] and the test-only [`Self::for_test_items`]
    /// so a test exercises the same row derivation and height/window
    /// bookkeeping production runs, rather than a hand-assembled struct with
    /// pre-baked results.
    fn assemble(
        task_id: TaskId,
        availability: JournalAvailability,
        items: Vec<TimelineItemModel>,
        viewport: TimelineViewport,
        captured_target: CapturedActionTarget,
    ) -> Self {
        let rows = Rc::new(apply_activity_collapse(
            derive_conversation_rows(&items, ConversationVerbosity::Calm),
            &[],
        ));
        let status = match availability {
            JournalAvailability::Unavailable(_) => {
                "Semantic timeline awaiting authenticated journal"
            }
            JournalAvailability::LiveProjection => "Conversation",
            #[cfg(any(test, feature = "semantic-conformance"))]
            JournalAvailability::ConformanceFixture => "Semantic timeline",
        };
        let activity_summary = conversation_activity_summary(&items, status);
        let following = Rc::new(Cell::new(true));
        let list_state = ListState::new(rows.len(), ListAlignment::Bottom, px(2048.0));
        let following_for_handler = following.clone();
        list_state.set_scroll_handler(move |event, _window, _cx| {
            following_for_handler.set(!event.is_scrolled);
        });
        let mut timeline = Self {
            task_id,
            availability,
            items,
            rows,
            activity_summary,
            expanded_activity: Vec::new(),
            prefix: Vec::new(),
            content_height: 0,
            viewport,
            following,
            list_state,
            projected_high_water: 0,
            projected_capabilities: None,
            projected_target: Some(captured_target),
            projected_task_revision: None,
            anchor: None,
            paint_start: 0,
            paint_end: 0,
            captured_target,
        };
        timeline.rebuild_heights();
        timeline.following.set(true);
        timeline.jump_to_latest();
        timeline
    }

    /// Test-only constructor that runs the identical `assemble` pipeline as
    /// production, over hand-built items, without the ClientModel/journal/
    /// registry plumbing `project` needs. Exists so a test can assert on a
    /// real `Timeline` (rows, heights, windowing) rather than a struct
    /// literal that bypasses the code under test.
    #[cfg(test)]
    pub(crate) fn for_test_items(items: Vec<TimelineItemModel>) -> Self {
        Self::for_test_task_items(TaskId::new(), items)
    }

    #[cfg(debug_assertions)]
    pub(crate) fn for_preview_plan_steps(task_id: TaskId, steps: &[PreviewPlanStep]) -> Self {
        let items = steps
            .iter()
            .map(|step| TimelineItemModel {
                id: TimelineItemId::Event(EventId::new()),
                task_id,
                renderer_selection: RendererSelection::Specialized(SemanticKind::Plan),
                interaction: InteractionEligibility::None,
                content: TimelineItemContent::Plan(PlanView {
                    step_id: Some(step.step_id.clone()),
                    title: step.title.clone(),
                    steps: vec![step.title.clone()],
                    status: step.status.clone(),
                }),
                activated_on_enter: false,
                accessibility: AccessibilityMetadata::new(
                    AccessibleRole::Status,
                    step.title.clone(),
                )
                .expect("validated preview plan title"),
                turn_id: None,
                related_event_id: None,
            })
            .collect();
        Self::assemble(
            task_id,
            JournalAvailability::LiveProjection,
            items,
            TimelineViewport {
                height: 1_200,
                scroll_offset: 0,
            },
            CapturedActionTarget {
                task_id,
                agent_session_id: None,
                runtime_generation: 0,
                request_id: None,
                action_epoch: 0,
            },
        )
    }

    #[cfg(test)]
    pub(crate) fn for_test_task_items(task_id: TaskId, items: Vec<TimelineItemModel>) -> Self {
        Self::assemble(
            task_id,
            JournalAvailability::LiveProjection,
            items,
            TimelineViewport {
                height: 280,
                scroll_offset: 0,
            },
            CapturedActionTarget {
                task_id,
                agent_session_id: None,
                runtime_generation: 0,
                request_id: None,
                action_epoch: 0,
            },
        )
    }

    pub fn item_count(&self) -> usize {
        self.items.len()
    }

    pub fn task_id(&self) -> TaskId {
        self.task_id
    }

    pub fn availability(&self) -> JournalAvailability {
        self.availability
    }

    /// Render the semantic surface without deriving transcript content from
    /// the native terminal. An unavailable journal remains visibly mounted as
    /// a typed hold until an authenticated semantic page is admitted.
    pub fn surface(&self, tokens: ThemeTokens) -> AnyElement {
        self.surface_with_activity_handler(tokens, None)
    }

    pub fn surface_with_activity_handler(
        &self,
        tokens: ThemeTokens,
        activity_toggle: Option<ActivityToggleHandler>,
    ) -> AnyElement {
        let activity = self.activity_summary.clone();
        let rows = Rc::clone(&self.rows);
        let list_state = self.list_state.clone();
        let activity_toggle_for_rows = activity_toggle.clone();
        let task_key = self.task_element_key();
        div()
            .id(("native-semantic-timeline", task_key))
            .w_full()
            .h_full()
            .bg(tokens.surfaces.canvas.to_gpui())
            .child(
                div()
                    .w_full()
                    .h_full()
                    .flex()
                    .justify_center()
                    .px(px(16.0))
                    .child(
                        div()
                            .id(("native-conversation-column", task_key))
                            // A definite width is required here. The earlier
                            // `w_full().max_w(..)` form did not clamp in GPUI.
                            .w(px(CONVERSATION_CONTENT_MAX_WIDTH))
                            .max_w_full()
                            .h_full()
                            .py(px(tokens.density.spacing.lg))
                            .flex()
                            .flex_col()
                            .gap(px(tokens.density.spacing.md))
                            .children(activity.map(|summary| {
                                div()
                                    .w_full()
                                    .flex_none()
                                    .text_size(px(tokens.density.typography.caption))
                                    .text_color(tokens.text.secondary.to_gpui())
                                    .child(summary)
                                    .into_any_element()
                            }))
                            .child(
                                list(
                                    list_state,
                                    move |ix, _window: &mut Window, _cx: &mut App| {
                                        rows.get(ix)
                                            .map(|row| {
                                                timeline_row_element(
                                                    row,
                                                    tokens,
                                                    activity_toggle_for_rows.clone(),
                                                )
                                            })
                                            .unwrap_or_else(|| div().into_any_element())
                                    },
                                )
                                .flex_1(),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn task_element_key(&self) -> u64 {
        u64::from_be_bytes(
            self.task_id.as_bytes()[8..]
                .try_into()
                .expect("task identity tail is exactly eight bytes"),
        )
    }

    pub fn projected_high_water(&self) -> u64 {
        self.projected_high_water
    }

    pub fn note_projected_high_water(&mut self, high_water: u64) {
        self.projected_high_water = high_water;
    }

    pub fn note_projection_identity(
        &mut self,
        high_water: u64,
        capabilities: CapabilitySet,
        target: CapturedActionTarget,
        task_revision: u64,
    ) {
        self.projected_high_water = high_water;
        self.projected_capabilities = Some(capabilities);
        self.projected_target = Some(target);
        self.projected_task_revision = Some(task_revision);
        self.captured_target = target;
    }

    pub fn matches_projection_identity(
        &self,
        high_water: u64,
        capabilities: CapabilitySet,
        target: CapturedActionTarget,
        task_revision: u64,
    ) -> bool {
        self.projected_high_water == high_water
            && self.projected_capabilities == Some(capabilities)
            && self.projected_target == Some(target)
            && self.projected_task_revision == Some(task_revision)
    }

    /// Replace render rows, invalidate measured heights for overlapping
    /// indices (including same-count streamed text edits), and restore the
    /// authoritative ListState scroll offset.
    fn replace_rows(
        &mut self,
        new_rows: Vec<ConversationRow>,
        following: bool,
        anchor_key: Option<ConversationRowKey>,
        offset_in_item: gpui::Pixels,
    ) {
        let old_len = self.list_state.item_count();
        let prefix = self
            .rows
            .iter()
            .zip(&new_rows)
            .take_while(|(old, new)| old == new)
            .count();
        let suffix = self.rows[prefix..]
            .iter()
            .rev()
            .zip(new_rows[prefix..].iter().rev())
            .take_while(|(old, new)| old == new)
            .count();
        self.rows = Rc::new(new_rows);
        let new_len = self.rows.len();
        let status = match self.availability {
            JournalAvailability::Unavailable(_) => {
                "Semantic timeline awaiting authenticated journal"
            }
            JournalAvailability::LiveProjection => "Conversation",
            #[cfg(any(test, feature = "semantic-conformance"))]
            JournalAvailability::ConformanceFixture => "Semantic timeline",
        };
        self.activity_summary = conversation_activity_summary(&self.items, status);
        if prefix + suffix < old_len || prefix + suffix < new_len {
            self.list_state
                .splice(prefix..old_len - suffix, new_len - prefix - suffix);
        }
        self.rebuild_heights();
        if following {
            // Splicing preserves the Bottom list's implicit end anchor. Reset
            // would discard all measured heights and drop the next scroll input.
            self.viewport.scroll_offset = self.content_height.saturating_sub(self.viewport.height);
            self.following.set(true);
            self.refresh_window();
            self.capture_anchor_from_list();
            return;
        }
        if let Some(key) = anchor_key {
            if let Some(index) = self
                .rows
                .iter()
                .position(|row| conversation_row_key(row) == key)
            {
                self.list_state.scroll_to(ListOffset {
                    item_ix: index,
                    offset_in_item,
                });
            }
        }
        self.following.set(false);
        self.capture_anchor_from_list();
    }

    pub fn toggle_activity_group(&mut self, group: &str) -> bool {
        if !self.rows.iter().any(
            |row| matches!(row, ConversationRow::ActivityToggle { group: row_group, .. } if row_group == group),
        ) {
            return false;
        }
        if let Some(index) = self
            .expanded_activity
            .iter()
            .position(|candidate| candidate == group)
        {
            self.expanded_activity.remove(index);
        } else {
            self.expanded_activity.push(group.to_string());
        }
        let scroll = self.list_state.logical_scroll_top();
        let anchor_key = self.rows.get(scroll.item_ix).map(conversation_row_key);
        let following = self.following.get();
        let next_rows = apply_activity_collapse(
            derive_conversation_rows(&self.items, ConversationVerbosity::Calm),
            &self.expanded_activity,
        );
        self.replace_rows(next_rows, following, anchor_key, scroll.offset_in_item);
        true
    }

    pub fn items(&self) -> &[TimelineItemModel] {
        &self.items
    }

    pub fn rows(&self) -> &[ConversationRow] {
        self.rows.as_ref()
    }

    pub fn content_height(&self) -> u32 {
        self.content_height
    }

    pub fn item_ids(&self) -> Vec<TimelineItemId> {
        self.items.iter().map(TimelineItemModel::id).collect()
    }

    pub fn item(&self, index: usize) -> Option<&TimelineItemModel> {
        self.items.get(index)
    }

    pub fn painted_rows(&self) -> &[ConversationRow] {
        if self.rows.is_empty() {
            return &[];
        }
        &self.rows[self.paint_start..=self.paint_end]
    }

    pub fn on_task_entered(&mut self) {
        for item in &mut self.items {
            item.on_task_entered();
        }
    }

    pub fn activate_inspect(
        &self,
        operation_id: OperationId,
        model: &ClientModel,
        capabilities: CapabilitySet,
    ) -> Result<TimelineActivation, RenderModelError> {
        let present = self.items.iter().any(|item| {
            matches!(
                &item.content,
                crate::ui::renderers::TimelineItemContent::Operation(view)
                    if view.operation_id == operation_id
            ) || item.id() == TimelineItemId::Operation(operation_id)
        });
        inspect_operation(
            present,
            model,
            self.task_id,
            operation_id,
            self.captured_target,
            capabilities,
        )
    }

    pub fn scroll_to_row(&mut self, index: usize) {
        if index >= self.rows.len() {
            return;
        }
        self.viewport.scroll_offset = self.prefix[index];
        self.following.set(false);
        self.list_state.scroll_to(ListOffset {
            item_ix: index,
            offset_in_item: px(0.0),
        });
        self.refresh_window();
        self.capture_anchor_from_list();
    }

    /// Legacy keyboard/test helper. Production mouse scrolling is owned by
    /// GPUI ListState; do not wire transcript wheel events through this.
    pub fn scroll_page(&mut self, down: bool) {
        let step = self.viewport.height.max(1) / 2;
        let distance = px(step as f32);
        if down {
            self.viewport.scroll_offset = self.viewport.scroll_offset.saturating_add(step);
            self.list_state.scroll_by(distance);
        } else {
            self.viewport.scroll_offset = self.viewport.scroll_offset.saturating_sub(step);
            self.list_state.scroll_by(-distance);
        }
        self.clamp_scroll();
        self.following.set(!self.list_state_is_scrolled_away());
        self.refresh_window();
        self.capture_anchor_from_list();
    }

    pub fn visible_anchor_key(&self) -> Option<ConversationRowKey> {
        self.anchor.as_ref().map(|anchor| anchor.key.clone())
    }

    pub fn follow_latest(&self) -> bool {
        self.following.get()
    }

    /// Legacy estimated viewport helper for older tests. Production follow
    /// decisions must use [`Self::follow_latest`] / ListState scroll state.
    pub fn at_bottom(&self) -> bool {
        let visible_end = self
            .viewport
            .scroll_offset
            .saturating_add(self.viewport.height);
        self.content_height.saturating_sub(visible_end) <= FOLLOW_REARM_THRESHOLD_PX
    }

    pub fn show_jump_to_latest(&self) -> bool {
        !self.follow_latest()
    }

    fn list_state_is_scrolled_away(&self) -> bool {
        self.list_state.logical_scroll_top().item_ix < self.rows.len()
    }

    /// Carry reader intent across a fresh journal projection. ListState
    /// logical scroll is authoritative for detached readers; estimated
    /// viewport anchors are never used to override real mouse scroll.
    pub(crate) fn preserve_view_state_from(&mut self, previous: &Self) {
        if self.task_id != previous.task_id {
            return;
        }

        let scroll_top = previous.list_state.logical_scroll_top();
        let list_following = scroll_top.item_ix >= previous.rows.len();
        let was_following = previous.following.get() || list_following;
        let anchor_key = previous
            .rows
            .get(scroll_top.item_ix)
            .map(conversation_row_key);
        let offset_in_item = scroll_top.offset_in_item;

        self.expanded_activity = previous.expanded_activity.clone();
        self.activity_summary = previous.activity_summary.clone();
        self.following = previous.following.clone();
        self.list_state = previous.list_state.clone();
        self.projected_high_water = previous.projected_high_water;
        self.projected_capabilities = previous.projected_capabilities;
        self.projected_target = previous.projected_target;
        self.projected_task_revision = previous.projected_task_revision;
        self.viewport = previous.viewport;

        let next_rows = apply_activity_collapse(
            derive_conversation_rows(&self.items, ConversationVerbosity::Calm),
            &previous.expanded_activity,
        );
        let next_rows = stable_conversation_rows(previous.rows.as_ref(), next_rows);
        self.rows = previous.rows.clone();
        self.replace_rows(next_rows, was_following, anchor_key, offset_in_item);
    }

    /// Keep the virtual window aligned with the actual conversation canvas.
    /// The old inspector used a fixed 280px viewport; a full-height chat must
    /// paint enough rows for the space the window really gives it.
    pub fn set_viewport_height(&mut self, height: u32) {
        let height = height.max(1);
        if self.viewport.height == height {
            return;
        }
        let was_following = self.following.get();
        self.viewport.height = height;
        self.clamp_scroll();
        if was_following {
            self.jump_to_latest();
        } else {
            self.refresh_window();
            self.capture_anchor_from_list();
        }
    }

    pub fn jump_to_latest(&mut self) {
        self.viewport.scroll_offset = self.content_height.saturating_sub(self.viewport.height);
        self.following.set(true);
        // Bottom-aligned ListState treats reset as "stick to the end".
        self.list_state.reset(self.rows.len());
        self.refresh_window();
        self.capture_anchor_from_list();
    }

    pub fn list_state(&self) -> &ListState {
        &self.list_state
    }

    /// Scroll/virtualization math walks the derived `rows`, not the raw
    /// `items` list. A row can fold several items into one (an Activity
    /// group) or none (a suppressed item that produces no row), so keying
    /// height off items would reserve scroll space for content that never
    /// paints. Keying off rows means a suppressed item costs zero height,
    /// because it never produced a row to have a height at all.
    fn rebuild_heights(&mut self) {
        self.prefix.clear();
        let mut total = 0u32;
        self.prefix.reserve(self.rows.len());
        let tokens = height_estimation_tokens();
        for row in self.rows.iter() {
            self.prefix.push(total);
            total = total.saturating_add(conversation_row_height(row, tokens));
        }
        self.content_height = total;
        self.clamp_scroll();
        self.refresh_window();
    }

    fn clamp_scroll(&mut self) {
        let max_scroll = self.content_height.saturating_sub(self.viewport.height);
        if self.viewport.scroll_offset > max_scroll {
            self.viewport.scroll_offset = max_scroll;
        }
    }

    fn refresh_window(&mut self) {
        if self.rows.is_empty() {
            self.paint_start = 0;
            self.paint_end = 0;
            return;
        }
        let start_y = self.viewport.scroll_offset;
        let end_y = self
            .viewport
            .scroll_offset
            .saturating_add(self.viewport.height);
        let first = match self.prefix.partition_point(|top| *top <= start_y) {
            0 => 0,
            n => (n - 1).min(self.rows.len() - 1),
        };
        let last = match self.prefix.partition_point(|top| *top < end_y) {
            0 => 0,
            n => (n - 1).min(self.rows.len() - 1),
        };
        let start = first.saturating_sub(DEFAULT_OVERSCAN);
        let mut end = last
            .saturating_add(DEFAULT_OVERSCAN)
            .min(self.rows.len() - 1);
        if end.saturating_sub(start) + 1 > MAX_PAINTED_ROWS {
            end = start + MAX_PAINTED_ROWS - 1;
        }
        self.paint_start = start;
        self.paint_end = end;
    }

    fn capture_anchor_from_list(&mut self) {
        if self.rows.is_empty() {
            self.anchor = None;
            return;
        }
        let scroll_top = self.list_state.logical_scroll_top();
        if scroll_top.item_ix >= self.rows.len() {
            self.anchor = self.rows.last().map(|row| TimelineAnchor {
                key: conversation_row_key(row),
                offset_within: 0,
            });
            return;
        }
        self.anchor = Some(TimelineAnchor {
            key: conversation_row_key(&self.rows[scroll_top.item_ix]),
            offset_within: scroll_top.offset_in_item.to_f64().max(0.0) as u32,
        });
        // Keep legacy estimated viewport roughly aligned for older tests.
        if let Some(prefix) = self.prefix.get(scroll_top.item_ix) {
            self.viewport.scroll_offset =
                prefix.saturating_add(scroll_top.offset_in_item.to_f64().max(0.0) as u32);
        }
    }

    #[cfg(test)]
    fn scroll_to_offset_for_test(&mut self, offset: u32) {
        self.viewport.scroll_offset = offset;
        self.clamp_scroll();
        let at_bottom = self.at_bottom();
        self.following.set(at_bottom);
        if !at_bottom {
            let index = match self.prefix.partition_point(|top| *top <= offset) {
                0 => 0,
                n => (n - 1).min(self.rows.len().saturating_sub(1)),
            };
            let within = offset.saturating_sub(self.prefix.get(index).copied().unwrap_or(0));
            self.list_state.scroll_to(ListOffset {
                item_ix: index,
                offset_in_item: px(within as f32),
            });
        } else {
            self.list_state.reset(self.rows.len());
        }
        self.refresh_window();
        self.capture_anchor_from_list();
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct ActivityCounts {
    running_commands: BTreeSet<String>,
    running_shells: BTreeSet<String>,
    active_subagents: BTreeSet<String>,
    open_task_steps: BTreeSet<String>,
}

impl ActivityCounts {
    fn observe_tool(&mut self, tool_id: &str, name: &str, state: &str) {
        match state {
            "pending" | "running" => {
                self.running_commands.insert(tool_id.to_string());
                let name = name.to_ascii_lowercase();
                if ["shell", "bash", "powershell", "terminal", "command"]
                    .iter()
                    .any(|needle| name.contains(needle))
                {
                    self.running_shells.insert(tool_id.to_string());
                }
            }
            "completed" | "failed" => {
                self.running_commands.remove(tool_id);
                self.running_shells.remove(tool_id);
            }
            _ => {}
        }
    }

    fn observe_plan(&mut self, step_id: Option<&str>, status: &str) {
        let Some(step_id) = step_id else {
            return;
        };
        let Some(status) = PlanStepStatus::from_wire(status) else {
            return;
        };
        let target = if step_id.starts_with("subagent:") {
            &mut self.active_subagents
        } else if step_id.starts_with("task:") {
            &mut self.open_task_steps
        } else {
            return;
        };
        match status {
            PlanStepStatus::Pending | PlanStepStatus::Active => {
                target.insert(step_id.to_string());
            }
            PlanStepStatus::Completed | PlanStepStatus::Failed => {
                target.remove(step_id);
            }
        }
    }
}

fn assert_task_projection(
    model: &ClientModel,
    task_id: TaskId,
    journal: &SemanticJournalView,
) -> Result<(), RenderModelError> {
    if journal.task_id() != task_id {
        return Err(RenderModelError::TaskMismatch);
    }
    match journal.availability() {
        JournalAvailability::Unavailable(_) => live_target(model, task_id).map(|_| ()),
        JournalAvailability::LiveProjection => live_target(model, task_id).map(|_| ()),
        #[cfg(any(test, feature = "semantic-conformance"))]
        JournalAvailability::ConformanceFixture => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        conversation_activity_summary, ActivityCounts, Timeline, CONVERSATION_CONTENT_MAX_WIDTH,
        FOLLOW_REARM_THRESHOLD_PX,
    };
    use crate::ui::conversation::fixtures::{generic_item, message_item, tool_item};
    use crate::ui::conversation::rows::{derive_conversation_rows, ConversationVerbosity};
    use crate::ui::renderers::MessageRole;

    #[test]
    fn an_unmapped_kind_produces_no_conversation_row_without_a_denylist() {
        let items = vec![
            generic_item("session_state"),
            generic_item("a_kind_invented_after_this_test_was_written"),
        ];
        let rows = derive_conversation_rows(&items, ConversationVerbosity::Calm);
        assert!(rows.is_empty());
    }

    #[test]
    fn suppressed_items_produce_no_rows_and_reserve_no_space() {
        // Guards the real defect surface: the Timeline must paint from
        // derived rows, not from raw items. A transcript of nothing but
        // suppressed lifecycle events must render nothing AND occupy no
        // height -- an invisible 96px gap per event is just a debug card
        // you cannot see. Built through the real `project`-sharing
        // constructor, not a hand-assembled struct, so this exercises the
        // actual row derivation and height bookkeeping.
        let items = vec![
            generic_item("session_state"),
            generic_item("usage_observation"),
            generic_item("a_kind_invented_after_this_test_was_written"),
        ];
        let timeline = Timeline::for_test_items(items);
        assert!(
            timeline.rows().is_empty(),
            "suppressed kinds must produce no rows"
        );
        assert_eq!(
            timeline.content_height(),
            0,
            "suppressed kinds must reserve no scroll space"
        );
    }

    #[test]
    fn ai_acceptance_activity_counts_track_open_lifecycles_only() {
        let mut activity = ActivityCounts::default();

        activity.observe_tool("shell-1", "Bash", "running");
        activity.observe_tool("shell-2", "PowerShell", "running");
        activity.observe_plan(Some("subagent:1"), "active");
        activity.observe_plan(Some("task:1"), "pending");
        assert_eq!(activity.running_commands.len(), 2);
        assert_eq!(activity.running_shells.len(), 2);
        assert_eq!(activity.active_subagents.len(), 1);
        assert_eq!(activity.open_task_steps.len(), 1);

        activity.observe_tool("shell-1", "Bash", "completed");
        activity.observe_plan(Some("subagent:1"), "completed");
        activity.observe_plan(Some("task:1"), "completed");
        assert_eq!(activity.running_commands.len(), 1);
        assert!(activity.active_subagents.is_empty());
        assert!(activity.open_task_steps.is_empty());

        activity.observe_tool("shell-2", "PowerShell", "failed");
        activity.observe_plan(Some("subagent:1"), "completed");
        activity.observe_plan(Some("task:1"), "completed");
        assert_eq!(activity, ActivityCounts::default());
    }

    #[test]
    fn activity_toggle_expands_and_collapses_the_real_timeline_projection() {
        let mut timeline = Timeline::for_test_items(vec![
            tool_item("tool-1", "Read", "completed"),
            tool_item("tool-2", "Read", "completed"),
            tool_item("tool-3", "Bash", "completed"),
        ]);
        let group = timeline
            .rows()
            .iter()
            .find_map(|row| match row {
                crate::ui::conversation::rows::ConversationRow::ActivityToggle {
                    group, ..
                } => Some(group.clone()),
                _ => None,
            })
            .expect("collapsed timeline toggle");

        assert!(timeline.toggle_activity_group(&group));
        let expanded_count = timeline
            .rows()
            .iter()
            .find_map(|row| match row {
                crate::ui::conversation::rows::ConversationRow::Activity { entries, .. } => {
                    Some(entries.len())
                }
                _ => None,
            })
            .expect("expanded activity row");
        assert_eq!(expanded_count, 3);

        assert!(timeline.toggle_activity_group(&group));
        let collapsed_count = timeline
            .rows()
            .iter()
            .find_map(|row| match row {
                crate::ui::conversation::rows::ConversationRow::Activity { entries, .. } => {
                    Some(entries.len())
                }
                _ => None,
            })
            .expect("collapsed activity row");
        assert_eq!(collapsed_count, 1);
    }

    #[test]
    fn idle_activity_summary_omits_raw_event_count_and_stays_quiet() {
        let items = vec![
            message_item(MessageRole::User, "hello"),
            message_item(MessageRole::Assistant, "hi"),
            generic_item("session_state"),
        ];
        let summary = conversation_activity_summary(&items, "Conversation");
        assert!(
            summary.is_none(),
            "idle transcripts must not surface a raw event count: {summary:?}"
        );
        assert_eq!(CONVERSATION_CONTENT_MAX_WIDTH, 768.0);
    }

    #[test]
    fn useful_activity_summary_reports_running_tools_without_event_count() {
        let items = vec![
            message_item(MessageRole::User, "run tests"),
            tool_item("shell-1", "Bash", "running"),
            generic_item("turn_state"),
        ];
        let summary = conversation_activity_summary(&items, "Conversation")
            .expect("running tools are useful activity");
        assert!(summary.contains("Running"));
        assert!(summary.contains("shell"));
        assert!(
            !summary.contains("event(s)"),
            "useful activity must still omit the raw event count: {summary}"
        );
    }

    fn long_timeline(viewport_height: u32) -> Timeline {
        let items = (0..80)
            .map(|index| message_item(MessageRole::Assistant, &format!("message {index}")))
            .collect();
        let mut timeline = Timeline::for_test_items(items);
        timeline.set_viewport_height(viewport_height);
        timeline
    }

    #[test]
    fn follow_rearms_within_a_pixel_band_not_only_at_the_exact_bottom() {
        let mut timeline = long_timeline(400);
        let offset = timeline
            .content_height()
            .saturating_sub(400)
            .saturating_sub(20);
        timeline.scroll_to_offset_for_test(offset);
        assert!(
            timeline.at_bottom(),
            "a 20px gap is inside the {FOLLOW_REARM_THRESHOLD_PX}px re-arm band"
        );
        assert!(timeline.follow_latest());
    }

    #[test]
    fn follow_does_not_rearm_while_reading_history() {
        let mut timeline = long_timeline(400);
        timeline.scroll_to_offset_for_test(400);
        assert!(!timeline.at_bottom());
        assert!(!timeline.follow_latest());
    }

    #[test]
    fn follow_band_is_strictly_bounded_to_forty_pixels() {
        let mut timeline = long_timeline(400);
        let offset = timeline
            .content_height()
            .saturating_sub(400)
            .saturating_sub(FOLLOW_REARM_THRESHOLD_PX + 1);
        timeline.scroll_to_offset_for_test(offset);
        assert!(!timeline.at_bottom());
    }

    #[test]
    fn a_streaming_append_keeps_a_detached_readers_anchor_stable() {
        let mut previous = long_timeline(400);
        previous.scroll_to_offset_for_test(400);
        let anchor = previous.visible_anchor_key().expect("reader anchor");
        let offset = previous.viewport.scroll_offset;

        let mut grown_items = previous.items.clone();
        grown_items.push(message_item(MessageRole::Assistant, "new streamed tail"));
        let mut next = Timeline::for_test_task_items(previous.task_id, grown_items);
        next.preserve_view_state_from(&previous);

        assert_eq!(next.visible_anchor_key(), Some(anchor));
        assert_eq!(next.viewport.scroll_offset, offset);
        assert!(!next.follow_latest());
    }

    #[test]
    fn a_streaming_append_moves_a_following_reader_to_the_new_bottom() {
        let previous = long_timeline(400);
        assert!(previous.follow_latest());
        let mut grown_items = previous.items.clone();
        grown_items.push(message_item(MessageRole::Assistant, "new streamed tail"));
        let mut next = Timeline::for_test_task_items(previous.task_id, grown_items);
        next.preserve_view_state_from(&previous);

        assert!(next.follow_latest());
        assert_eq!(
            next.viewport.scroll_offset,
            next.content_height().saturating_sub(next.viewport.height)
        );
    }

    #[test]
    fn same_count_streamed_text_keeps_list_offset_while_invalidating() {
        use crate::ui::renderers::{MarkdownBlock, TimelineItemContent};
        use gpui::px;

        let items = (0..40)
            .map(|index| message_item(MessageRole::Assistant, &format!("row {index}")))
            .collect::<Vec<_>>();
        let mut previous = Timeline::for_test_items(items);
        previous.set_viewport_height(400);
        previous.scroll_to_offset_for_test(180);
        let before = previous.list_state().logical_scroll_top();
        assert!(!previous.follow_latest());

        let mut grown = previous.items.clone();
        let target = before.item_ix.min(grown.len().saturating_sub(1));
        if let TimelineItemContent::Message(message) = &mut grown[target].content {
            message.markdown.blocks = vec![MarkdownBlock::Paragraph {
                text:
                    "same-row streamed text that wraps across more measured lines\nand another line"
                        .into(),
            }];
        }
        let mut next = Timeline::for_test_task_items(previous.task_id, grown);
        next.preserve_view_state_from(&previous);

        let after = next.list_state().logical_scroll_top();
        assert_eq!(after.item_ix, before.item_ix);
        assert!(!next.follow_latest());
        assert_eq!(next.rows().len(), previous.rows().len());
    }

    #[test]
    fn activity_expansion_survives_preserve_across_reproject() {
        let mut previous = Timeline::for_test_items(vec![
            tool_item("tool-1", "Read", "completed"),
            tool_item("tool-2", "Read", "completed"),
            tool_item("tool-3", "Bash", "completed"),
            message_item(MessageRole::Assistant, "tail"),
        ]);
        let group = previous
            .rows()
            .iter()
            .find_map(|row| match row {
                crate::ui::conversation::rows::ConversationRow::ActivityToggle {
                    group, ..
                } => Some(group.clone()),
                _ => None,
            })
            .expect("toggle");
        assert!(previous.toggle_activity_group(&group));
        assert!(previous.rows().iter().any(|row| matches!(
            row,
            crate::ui::conversation::rows::ConversationRow::Activity { entries, .. }
                if entries.len() == 3
        )));

        let mut next = Timeline::for_test_task_items(previous.task_id, previous.items.clone());
        next.preserve_view_state_from(&previous);
        assert!(next.rows().iter().any(|row| matches!(
            row,
            crate::ui::conversation::rows::ConversationRow::Activity { entries, .. }
                if entries.len() == 3
        )));
    }

    #[test]
    fn list_state_offset_is_authoritative_over_estimated_viewport_anchor() {
        use gpui::px;

        let mut timeline = long_timeline(400);
        timeline.scroll_to_offset_for_test(220);
        // Corrupt the legacy estimated viewport; production must still trust ListState.
        timeline.viewport.scroll_offset = 0;
        timeline.anchor = None;
        let list_top = timeline.list_state().logical_scroll_top();
        assert!(list_top.item_ix > 0 || list_top.offset_in_item > px(0.0));

        let mut next = Timeline::for_test_task_items(timeline.task_id, timeline.items.clone());
        next.preserve_view_state_from(&timeline);
        let restored = next.list_state().logical_scroll_top();
        assert_eq!(restored.item_ix, list_top.item_ix);
        assert!(!next.follow_latest());
    }

    #[test]
    fn replace_rows_append_and_shrink_keep_list_state_item_count_aligned() {
        let items = (0..8)
            .map(|index| message_item(MessageRole::Assistant, &format!("row {index}")))
            .collect::<Vec<_>>();
        let mut timeline = Timeline::for_test_items(items);
        assert_eq!(timeline.list_state().item_count(), timeline.rows().len());

        let mut grown = timeline.items.clone();
        grown.push(message_item(MessageRole::Assistant, "appended"));
        let mut next = Timeline::for_test_task_items(timeline.task_id, grown);
        next.preserve_view_state_from(&timeline);
        assert_eq!(
            next.list_state().item_count(),
            next.rows().len(),
            "append must grow ListState to the new row count"
        );

        let shrunk_items = next.items[..3].to_vec();
        let mut shrunk = Timeline::for_test_task_items(next.task_id, shrunk_items);
        shrunk.preserve_view_state_from(&next);
        assert_eq!(
            shrunk.list_state().item_count(),
            shrunk.rows().len(),
            "shrink must drop trailing ListState entries"
        );
        assert!(shrunk.rows().len() < next.rows().len());
    }

    #[test]
    fn distinct_tasks_retain_independent_list_scroll_state() {
        let first_id = crate::domain::TaskId::new();
        let second_id = crate::domain::TaskId::new();
        let items = (0..40)
            .map(|index| message_item(MessageRole::Assistant, &format!("message {index}")))
            .collect::<Vec<_>>();
        let mut first = Timeline::for_test_task_items(first_id, items.clone());
        let second = Timeline::for_test_task_items(second_id, items);
        first.set_viewport_height(400);
        first.scroll_to_offset_for_test(120);
        assert!(!first.follow_latest());
        assert!(second.follow_latest());
        assert!(first.list_state().item_count() > 0);
        assert_eq!(
            first.list_state().item_count(),
            second.list_state().item_count()
        );
    }
}
