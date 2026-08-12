//! Virtualized semantic timeline over the sealed journal projection.
//!
//! Production rows come only from [`SemanticJournalView`]. Caller-supplied
//! raw `SemanticEvent` arrays are not part of the public API.

use gpui::{div, px, AnyElement, IntoElement, ParentElement, Styled};

use crate::client::model::ClientModel;
use crate::domain::id::{OperationId, TaskId};
use crate::protocol::CapabilitySet;
use crate::ui::renderers::{
    inspect_operation, live_target, CapturedActionTarget, JournalAvailability, RenderModelError,
    RendererRegistry, SemanticJournalView, TimelineActivation, TimelineItemId, TimelineItemModel,
};

pub const DEFAULT_OVERSCAN: usize = 4;
pub const MAX_PAINTED_ITEMS: usize = 48;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimelineViewport {
    pub height: u32,
    pub scroll_offset: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TimelineAnchor {
    id: TimelineItemId,
    offset_within: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Timeline {
    task_id: TaskId,
    availability: JournalAvailability,
    items: Vec<TimelineItemModel>,
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
        let mut timeline = Self {
            task_id,
            availability: journal.availability(),
            items,
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
        Ok(timeline)
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
    pub fn surface(&self, tokens: crate::ui::tokens::ThemeTokens) -> AnyElement {
        let status = match self.availability {
            JournalAvailability::Unavailable(_) => {
                "Semantic timeline awaiting authenticated journal"
            }
            #[cfg(any(test, feature = "semantic-conformance"))]
            JournalAvailability::ConformanceFixture => "Semantic timeline",
        };
        let rows = self
            .painted_items()
            .iter()
            .map(|item| {
                div()
                    .w_full()
                    .p(px(tokens.density.physical().row_padding as f32))
                    .border_b_1()
                    .border_color(tokens.borders.subtle.to_gpui())
                    .child(item.accessibility.name().to_string())
                    .into_any_element()
            })
            .collect::<Vec<_>>();
        div()
            .id("native-semantic-timeline")
            .w_full()
            .max_h(px(280.0))
            .overflow_y_scroll()
            .bg(tokens.surfaces.raised.to_gpui())
            .child(
                div()
                    .w_full()
                    .p(px(tokens.density.physical().control_padding as f32))
                    .text_color(tokens.text.secondary.to_gpui())
                    .child(format!("{status} · {} item(s)", self.items.len())),
            )
            .children(rows)
            .into_any_element()
    }

    pub fn items(&self) -> &[TimelineItemModel] {
        &self.items
    }

    pub fn item_ids(&self) -> Vec<TimelineItemId> {
        self.items.iter().map(TimelineItemModel::id).collect()
    }

    pub fn item(&self, index: usize) -> Option<&TimelineItemModel> {
        self.items.get(index)
    }

    pub fn painted_items(&self) -> &[TimelineItemModel] {
        if self.items.is_empty() {
            return &[];
        }
        &self.items[self.paint_start..=self.paint_end]
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

    pub fn scroll_to_item(&mut self, index: usize) {
        if index >= self.items.len() {
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

    pub fn visible_anchor_id(&self) -> Option<TimelineItemId> {
        self.anchor.map(|anchor| anchor.id)
    }

    pub fn follow_latest(&self) -> bool {
        self.following && self.at_bottom()
    }

    pub fn at_bottom(&self) -> bool {
        self.content_height <= self.viewport.height
            || self
                .viewport
                .scroll_offset
                .saturating_add(self.viewport.height)
                >= self.content_height
    }

    pub fn show_jump_to_latest(&self) -> bool {
        !self.at_bottom()
    }

    pub fn jump_to_latest(&mut self) {
        self.viewport.scroll_offset = self.content_height.saturating_sub(self.viewport.height);
        self.following = true;
        self.refresh_window();
        self.capture_anchor();
    }

    fn rebuild_heights(&mut self) {
        self.prefix.clear();
        let mut total = 0u32;
        self.prefix.reserve(self.items.len());
        for item in &self.items {
            self.prefix.push(total);
            total = total.saturating_add(item.estimated_height());
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
        if self.items.is_empty() {
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
            n => (n - 1).min(self.items.len() - 1),
        };
        let last = match self.prefix.partition_point(|top| *top < end_y) {
            0 => 0,
            n => (n - 1).min(self.items.len() - 1),
        };
        let start = first.saturating_sub(DEFAULT_OVERSCAN);
        let mut end = last
            .saturating_add(DEFAULT_OVERSCAN)
            .min(self.items.len() - 1);
        if end.saturating_sub(start) + 1 > MAX_PAINTED_ITEMS {
            end = start + MAX_PAINTED_ITEMS - 1;
        }
        self.paint_start = start;
        self.paint_end = end;
    }

    fn capture_anchor(&mut self) {
        if self.items.is_empty() {
            self.anchor = None;
            return;
        }
        let index = match self
            .prefix
            .partition_point(|top| *top <= self.viewport.scroll_offset)
        {
            0 => 0,
            n => (n - 1).min(self.items.len() - 1),
        };
        self.anchor = Some(TimelineAnchor {
            id: self.items[index].id(),
            offset_within: self
                .viewport
                .scroll_offset
                .saturating_sub(self.prefix[index]),
        });
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
        #[cfg(any(test, feature = "semantic-conformance"))]
        JournalAvailability::ConformanceFixture => Ok(()),
    }
}
