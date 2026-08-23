//! Virtualized semantic timeline over the sealed journal projection.
//!
//! Production rows come only from [`SemanticJournalView`]. Caller-supplied
//! raw `SemanticEvent` arrays are not part of the public API.

use gpui::{
    div, px, AnyElement, InteractiveElement, IntoElement, ParentElement,
    StatefulInteractiveElement, Styled,
};
use std::collections::BTreeSet;

use crate::client::model::ClientModel;
use crate::domain::id::{OperationId, TaskId};
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

pub const DEFAULT_OVERSCAN: usize = 4;
pub const MAX_PAINTED_ROWS: usize = 48;
/// Shared readable measure for the conversation column and floating composer.
pub const CONVERSATION_CONTENT_MAX_WIDTH: f32 = 860.0;
/// Follow re-arm band above the true content bottom. Strict on purpose: a
/// half-viewport "near end" test re-arms live-follow while the user is reading
/// history and yanks them back down on the next streamed chunk.
pub const FOLLOW_REARM_THRESHOLD_PX: u32 = 40;

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
            TimelineItemContent::Plan(view) => activity.observe_plan(&view.status),
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
    if activity.active_subagents > 0 {
        parts.push(format!("{} subagent(s) active", activity.active_subagents));
    }
    if activity.open_task_steps > 0 {
        parts.push(format!(
            "Goal active · {} task step(s) open",
            activity.open_task_steps
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Timeline {
    task_id: TaskId,
    availability: JournalAvailability,
    items: Vec<TimelineItemModel>,
    rows: Vec<ConversationRow>,
    expanded_activity: Vec<String>,
    prefix: Vec<u32>,
    content_height: u32,
    viewport: TimelineViewport,
    following: bool,
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
        let rows = apply_activity_collapse(
            derive_conversation_rows(&items, ConversationVerbosity::Calm),
            &[],
        );
        let mut timeline = Self {
            task_id,
            availability,
            items,
            rows,
            expanded_activity: Vec::new(),
            prefix: Vec::new(),
            content_height: 0,
            viewport,
            following: true,
            anchor: None,
            paint_start: 0,
            paint_end: 0,
            captured_target,
        };
        timeline.rebuild_heights();
        timeline.following = true;
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

    #[cfg(test)]
    fn for_test_task_items(task_id: TaskId, items: Vec<TimelineItemModel>) -> Self {
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
        let status = match self.availability {
            JournalAvailability::Unavailable(_) => {
                "Semantic timeline awaiting authenticated journal"
            }
            JournalAvailability::LiveProjection => "Conversation",
            #[cfg(any(test, feature = "semantic-conformance"))]
            JournalAvailability::ConformanceFixture => "Semantic timeline",
        };
        let activity = conversation_activity_summary(&self.items, status);
        let painted_rows = self
            .painted_rows()
            .iter()
            .map(|row| conversation_row_element(row, tokens))
            .collect::<Vec<_>>();
        div()
            .id("native-semantic-timeline")
            .w_full()
            .h_full()
            .overflow_y_scroll()
            .bg(tokens.surfaces.canvas.to_gpui())
            .child(
                div().w_full().flex().justify_center().px(px(16.0)).child(
                    div()
                        .id("native-conversation-column")
                        // A definite width is required here. The earlier
                        // `w_full().max_w(..)` form did not clamp in GPUI.
                        .w(px(CONVERSATION_CONTENT_MAX_WIDTH))
                        .max_w_full()
                        .py(px(tokens.density.spacing.lg))
                        .flex()
                        .flex_col()
                        .gap(px(tokens.density.spacing.md))
                        .children(activity.map(|summary| {
                            div()
                                .w_full()
                                .text_size(px(tokens.density.typography.caption))
                                .text_color(tokens.text.secondary.to_gpui())
                                .child(summary)
                                .into_any_element()
                        }))
                        .children(painted_rows),
                ),
            )
            .into_any_element()
    }

    pub fn items(&self) -> &[TimelineItemModel] {
        &self.items
    }

    pub fn rows(&self) -> &[ConversationRow] {
        &self.rows
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
        self.following = false;
        self.refresh_window();
        self.capture_anchor();
    }

    pub fn scroll_page(&mut self, down: bool) {
        let step = self.viewport.height.max(1) / 2;
        if down {
            self.viewport.scroll_offset = self.viewport.scroll_offset.saturating_add(step);
        } else {
            self.viewport.scroll_offset = self.viewport.scroll_offset.saturating_sub(step);
        }
        self.clamp_scroll();
        self.following = self.at_bottom();
        self.refresh_window();
        self.capture_anchor();
    }

    pub fn visible_anchor_key(&self) -> Option<ConversationRowKey> {
        self.anchor.as_ref().map(|anchor| anchor.key.clone())
    }

    pub fn follow_latest(&self) -> bool {
        self.following && self.at_bottom()
    }

    pub fn at_bottom(&self) -> bool {
        let visible_end = self
            .viewport
            .scroll_offset
            .saturating_add(self.viewport.height);
        self.content_height.saturating_sub(visible_end) <= FOLLOW_REARM_THRESHOLD_PX
    }

    pub fn show_jump_to_latest(&self) -> bool {
        !self.at_bottom()
    }

    /// Carry reader intent across a fresh journal projection. Projecting a
    /// streaming delta constructs new item data, but it must not silently
    /// reset the viewport to the bottom. A following reader moves to the new
    /// bottom; a detached reader keeps the same durable row anchor and pixel
    /// offset while the new tail grows below it.
    pub(crate) fn preserve_view_state_from(&mut self, previous: &Self) {
        if self.task_id != previous.task_id {
            return;
        }

        let next_rows = apply_activity_collapse(
            derive_conversation_rows(&self.items, ConversationVerbosity::Calm),
            &previous.expanded_activity,
        );
        self.rows = stable_conversation_rows(&previous.rows, next_rows);
        self.expanded_activity = previous.expanded_activity.clone();
        self.viewport = previous.viewport;
        self.following = previous.following;
        self.anchor = previous.anchor.clone();
        self.rebuild_heights();

        if previous.following {
            self.jump_to_latest();
            return;
        }

        if let Some(anchor) = previous.anchor.as_ref() {
            if let Some(index) = self
                .rows
                .iter()
                .position(|row| conversation_row_key(row) == anchor.key)
            {
                self.viewport.scroll_offset =
                    self.prefix[index].saturating_add(anchor.offset_within);
            }
        }
        self.clamp_scroll();
        self.following = false;
        self.refresh_window();
        self.capture_anchor();
    }

    /// Keep the virtual window aligned with the actual conversation canvas.
    /// The old inspector used a fixed 280px viewport; a full-height chat must
    /// paint enough rows for the space the window really gives it.
    pub fn set_viewport_height(&mut self, height: u32) {
        let height = height.max(1);
        if self.viewport.height == height {
            return;
        }
        let was_following = self.following;
        self.viewport.height = height;
        self.clamp_scroll();
        if was_following {
            self.jump_to_latest();
        } else {
            self.refresh_window();
            self.capture_anchor();
        }
    }

    pub fn jump_to_latest(&mut self) {
        self.viewport.scroll_offset = self.content_height.saturating_sub(self.viewport.height);
        self.following = true;
        self.refresh_window();
        self.capture_anchor();
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
        for row in &self.rows {
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

    fn capture_anchor(&mut self) {
        if self.rows.is_empty() {
            self.anchor = None;
            return;
        }
        let index = match self
            .prefix
            .partition_point(|top| *top <= self.viewport.scroll_offset)
        {
            0 => 0,
            n => (n - 1).min(self.rows.len() - 1),
        };
        self.anchor = Some(TimelineAnchor {
            key: conversation_row_key(&self.rows[index]),
            offset_within: self
                .viewport
                .scroll_offset
                .saturating_sub(self.prefix[index]),
        });
    }

    #[cfg(test)]
    fn scroll_to_offset_for_test(&mut self, offset: u32) {
        self.viewport.scroll_offset = offset;
        self.clamp_scroll();
        self.following = self.at_bottom();
        self.refresh_window();
        self.capture_anchor();
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct ActivityCounts {
    running_commands: BTreeSet<String>,
    running_shells: BTreeSet<String>,
    active_subagents: usize,
    open_task_steps: usize,
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

    fn observe_plan(&mut self, status: &str) {
        match status {
            "subagentStarted" => self.active_subagents += 1,
            // Claude emits a correlated SubagentStop hook for every completed
            // subagent. The later task notification is presentation-only and
            // must not decrement a second time.
            "subagentStopped" => self.active_subagents = self.active_subagents.saturating_sub(1),
            "taskCreated" => self.open_task_steps += 1,
            "taskCompleted" => self.open_task_steps = self.open_task_steps.saturating_sub(1),
            _ => {}
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
        activity.observe_plan("subagentStarted");
        activity.observe_plan("taskCreated");
        assert_eq!(activity.running_commands.len(), 2);
        assert_eq!(activity.running_shells.len(), 2);
        assert_eq!(activity.active_subagents, 1);
        assert_eq!(activity.open_task_steps, 1);

        activity.observe_tool("shell-1", "Bash", "completed");
        activity.observe_plan("subagentStopped");
        activity.observe_plan("subagentCompleted");
        activity.observe_plan("taskCompleted");
        assert_eq!(activity.running_commands.len(), 1);
        assert_eq!(activity.active_subagents, 0);
        assert_eq!(activity.open_task_steps, 0);

        activity.observe_tool("shell-2", "PowerShell", "failed");
        activity.observe_plan("subagentStopped");
        activity.observe_plan("taskCompleted");
        assert_eq!(activity, ActivityCounts::default());
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
        assert_eq!(CONVERSATION_CONTENT_MAX_WIDTH, 860.0);
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
}
