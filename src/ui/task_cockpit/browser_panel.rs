//! Native GPUI browser chrome for the task context dock.
//!
//! Page pixels attach separately. This panel draws tabs, address, status,
//! progress, approvals, artifacts, and diagnostics from the host projection.

use gpui::{div, prelude::*, px, rgb, IntoElement, SharedString};

use crate::browser::BrowserDockSurface;
use crate::protocol::{BrowserProjectionMeta, BrowserSecurityState};
use crate::ui::task_cockpit::context_dock::BrowserContextDock;
use crate::ui::tokens::ThemeTokens;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskBrowserDockModel {
    pub task_title: String,
    pub address: String,
    pub title: String,
    pub security: BrowserSecurityState,
    pub loading: bool,
    pub error: Option<String>,
    pub progress: Option<String>,
    pub tab_labels: Vec<String>,
    pub selected_tab: Option<String>,
    pub diagnostic: Option<String>,
    pub approval: Option<String>,
    pub artifact_count: usize,
}

impl TaskBrowserDockModel {
    pub fn from_dock(dock: &BrowserContextDock) -> Self {
        Self::from_projection(dock.projection())
    }

    pub fn from_projection(meta: &BrowserProjectionMeta) -> Self {
        let selected = meta
            .selected_tab_id
            .and_then(|id| meta.tabs.key_from_id(id));
        let selected_tab = selected.cloned();
        Self {
            task_title: "Task browser".to_string(),
            address: selected.map(|tab| tab.url.clone()).unwrap_or_default(),
            title: selected.map(|tab| tab.title.clone()).unwrap_or_default(),
            security: selected
                .map(|tab| tab.security)
                .unwrap_or(BrowserSecurityState::Unknown),
            loading: selected.map(|tab| tab.loading).unwrap_or(false),
            error: selected.and_then(|tab| tab.error.clone()),
            progress: meta.progress.clone(),
            tab_labels: meta
                .tabs
                .iter()
                .map(|tab| {
                    if tab.title.is_empty() {
                        tab.url.clone()
                    } else {
                        tab.title.clone()
                    }
                })
                .collect(),
            selected_tab: selected_tab.map(|tab| tab.title.clone()),
            diagnostic: None,
            approval: None,
            artifact_count: 0,
        }
    }

    pub fn uses_web_chrome() -> bool {
        BrowserDockSurface::uses_web_chrome()
    }
}

trait TabLookup {
    fn key_from_id(
        &self,
        id: crate::domain::id::BrowserTabId,
    ) -> Option<&crate::protocol::BrowserTabProjection>;
}

impl TabLookup for Vec<crate::protocol::BrowserTabProjection> {
    fn key_from_id(
        &self,
        id: crate::domain::id::BrowserTabId,
    ) -> Option<&crate::protocol::BrowserTabProjection> {
        self.iter().find(|tab| tab.tab_id == id)
    }
}

/// The toolbar row, per the brief: one 28 px row carrying the page's identity.
///
/// The mockup's browser chrome is a single row of 14 px glyphs beside a sunken
/// address field, and that is the shape painted here -- with one deviation,
/// ledgered in `lane-r2-report.md`: the row holds the security glyph and the
/// field but **no navigation buttons**. `TaskBrowserDockModel` carries no back,
/// forward or reload action, and this painter is a pure `model -> element`
/// function with no `Context` to wire a click to, so back/forward/reload here
/// would be three glyphs that do nothing. The row's geometry is the mockup's,
/// so the buttons drop into it unchanged the day the actions exist.
const TOOLBAR_HEIGHT: f32 = 28.0;
/// The tab strip above it, one row shorter than the toolbar because it carries
/// no control -- only labels on the row grid.
const TAB_STRIP_HEIGHT: f32 = 26.0;
/// Rule 3: radius 4 for inputs.
const FIELD_RADIUS: f32 = 4.0;
/// The address field's own padding inside the 28 px row.
const FIELD_PADDING_X: f32 = 8.0;
const FIELD_PADDING_Y: f32 = 3.0;
/// Rule 5's horizontal padding, shared with every other body row.
const ROW_PADDING_X: f32 = super::panel::ROW_PADDING_X;
/// Rule 6: control gap 8.
const CONTROL_GAP: f32 = super::panel::ROW_GAP;
/// Rule 2's sizes, read from the one place the body language defines them.
const BODY_FONT_SIZE: f32 = super::panel::ROW_FONT_SIZE;
const META_FONT_SIZE: f32 = super::panel::META_FONT_SIZE;
/// Rule 10: a lucide glyph is 14 px and `text.muted`.
const GLYPH_SIZE: f32 = super::panel::ROW_ICON_SIZE;

pub fn render_task_browser_dock(
    model: TaskBrowserDockModel,
    tokens: ThemeTokens,
) -> impl IntoElement {
    // Rule 1: colour is information. An error is the only thing in this panel
    // that earns red; "Insecure" is a fact about the page, not a failure of
    // the app, so it stays grey and says so in words.
    let (status, status_colour) = match model.error.clone() {
        Some(error) => (error, tokens.status.destructive),
        None => (
            model
                .progress
                .clone()
                .or_else(|| model.loading.then(|| "Loading…".to_string()))
                .unwrap_or_else(|| match model.security {
                    BrowserSecurityState::Secure => "Secure".to_string(),
                    BrowserSecurityState::Insecure => "Insecure".to_string(),
                    BrowserSecurityState::Unknown => "Unknown".to_string(),
                }),
            tokens.text.muted,
        ),
    };
    let has_tabs = !model.tab_labels.is_empty();
    let selected_tab = model.selected_tab.clone();
    let tabs = model.tab_labels.into_iter().map(move |label| {
        // Rule 5: a selected row is `surfaces.selection` with a white title;
        // an unselected one is muted and fills only on hover. No radius, so
        // the strip reads as bands rather than as a row of pills.
        let selected = selected_tab.as_deref() == Some(label.as_str());
        let tab = div()
            .flex_none()
            .flex()
            .items_center()
            .h_full()
            .px(px(ROW_PADDING_X))
            .text_size(px(BODY_FONT_SIZE));
        if selected {
            tab.bg(rgb(tokens.surfaces.selection.to_u32()))
                .text_color(rgb(tokens.text.emphasis.to_u32()))
        } else {
            tab.text_color(rgb(tokens.text.muted.to_u32()))
                .hover(|style| style.bg(rgb(tokens.surfaces.hover.to_u32())))
        }
        .child(SharedString::from(label))
        .into_any_element()
    });
    // Rule 5's metadata line, folded into one caption instead of the three
    // separate bands this panel used to paint. A body that spends a row on
    // "artifacts 0" is spending the panel's height on nothing.
    let mut meta = vec![status];
    if model.artifact_count > 0 {
        meta.push(format!("{} artifact(s)", model.artifact_count));
    }
    if let Some(diagnostic) = model.diagnostic.clone() {
        meta.push(diagnostic);
    }
    let meta_line = meta.join(" · ");
    let address = if model.address.is_empty() {
        model.title.clone()
    } else {
        model.address.clone()
    };
    let address_is_empty = address.is_empty();
    div()
        .h_full()
        .flex()
        .flex_col()
        .bg(rgb(tokens.surfaces.canvas.to_u32()))
        .children(has_tabs.then(|| {
            div()
                .flex_none()
                .h(px(TAB_STRIP_HEIGHT))
                .flex()
                .overflow_hidden()
                // Rule 3: a 1 px `borders.subtle` rule between regions.
                .border_b_1()
                .border_color(rgb(tokens.borders.subtle.to_u32()))
                .children(tabs)
        }))
        .child(
            div()
                .flex_none()
                .h(px(TOOLBAR_HEIGHT))
                .flex()
                .items_center()
                .gap(px(CONTROL_GAP))
                .px(px(ROW_PADDING_X))
                .border_b_1()
                .border_color(rgb(tokens.borders.subtle.to_u32()))
                .child(crate::icons::app_icon(
                    crate::icons::GLOBE,
                    GLYPH_SIZE,
                    tokens.text.muted.to_u32(),
                ))
                .child(
                    // Rule 3: inputs are `surfaces.sunken` inside a 1 px
                    // `borders.default`, radius 4.
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .px(px(FIELD_PADDING_X))
                        .py(px(FIELD_PADDING_Y))
                        .rounded(px(FIELD_RADIUS))
                        .bg(rgb(tokens.surfaces.sunken.to_u32()))
                        .border_1()
                        .border_color(rgb(tokens.borders.default.to_u32()))
                        .text_size(px(BODY_FONT_SIZE))
                        .text_color(rgb(if address_is_empty {
                            tokens.text.muted.to_u32()
                        } else {
                            tokens.text.primary.to_u32()
                        }))
                        .truncate()
                        .child(SharedString::from(if address_is_empty {
                            "No page attached".to_string()
                        } else {
                            address
                        })),
                ),
        )
        .child(
            div()
                .flex_none()
                .flex()
                .items_center()
                .gap(px(CONTROL_GAP))
                .px(px(ROW_PADDING_X))
                .py(px(super::panel::ROW_PADDING_Y))
                .text_size(px(META_FONT_SIZE))
                .text_color(rgb(status_colour.to_u32()))
                .child(SharedString::from(meta_line)),
        )
        .children(model.approval.map(|approval| {
            // The one place this panel spends colour: an approval is a "needs
            // you", which rule 1 gives to `status.attention`.
            div()
                .flex_none()
                .px(px(ROW_PADDING_X))
                .py(px(super::panel::ROW_PADDING_Y))
                .text_size(px(BODY_FONT_SIZE))
                .text_color(rgb(tokens.status.attention.to_u32()))
                .child(SharedString::from(approval))
        }))
        .child(
            // Where the page pixels attach. The ground is the app's canvas,
            // not the terminal palette's background -- rule 1 forbids a tint
            // borrowed from an unrelated surface.
            div()
                .flex_1()
                .min_h(px(0.0))
                .bg(rgb(tokens.surfaces.canvas.to_u32())),
        )
}
