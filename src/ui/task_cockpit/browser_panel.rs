//! Native GPUI browser chrome for the task context dock.
//!
//! Page pixels attach separately. This panel draws tabs, address, status,
//! progress, approvals, artifacts, and diagnostics from the host projection.

use gpui::{div, prelude::*, px, rgb, IntoElement, SharedString};

use crate::browser::surface::BrowserDockSurface;
use crate::protocol::{BrowserProjectionMeta, BrowserSecurityState};
use crate::theme;
use crate::ui::task_cockpit::context_dock::BrowserContextDock;

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
            address: selected
                .map(|tab| tab.url.clone())
                .unwrap_or_default(),
            title: selected
                .map(|tab| tab.title.clone())
                .unwrap_or_default(),
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

pub fn render_task_browser_dock(model: TaskBrowserDockModel) -> impl IntoElement {
    let status = model
        .error
        .clone()
        .or_else(|| model.progress.clone())
        .or_else(|| model.loading.then(|| "Loading...".to_string()))
        .unwrap_or_else(|| match model.security {
            BrowserSecurityState::Secure => "Secure".to_string(),
            BrowserSecurityState::Insecure => "Insecure".to_string(),
            BrowserSecurityState::Unknown => "Unknown".to_string(),
        });
    let tabs = model.tab_labels.into_iter().map(|label| {
        let selected = model.selected_tab.as_deref() == Some(label.as_str());
        div()
            .px(px(6.0))
            .py(px(3.0))
            .text_xs()
            .bg(rgb(if selected {
                theme::TAB_ACTIVE_BG
            } else {
                theme::TOPBAR_BG
            }))
            .text_color(rgb(theme::TEXT_PRIMARY))
            .child(SharedString::from(label))
            .into_any_element()
    });
    div()
        .h_full()
        .flex()
        .flex_col()
        .bg(rgb(theme::PANEL_BG))
        .child(
            div()
                .h(px(26.0))
                .flex()
                .items_center()
                .border_b_1()
                .border_color(rgb(theme::BORDER_PRIMARY))
                .children(tabs),
        )
        .child(
            div()
                .h(px(28.0))
                .flex()
                .items_center()
                .px(px(6.0))
                .text_xs()
                .text_color(rgb(theme::TEXT_PRIMARY))
                .child(SharedString::from(if model.address.is_empty() {
                    model.title
                } else {
                    model.address
                })),
        )
        .child(
            div()
                .h(px(22.0))
                .flex()
                .items_center()
                .px(px(6.0))
                .text_xs()
                .text_color(rgb(theme::TEXT_MUTED))
                .child(SharedString::from(status)),
        )
        .children(model.approval.map(|approval| {
            div()
                .px(px(6.0))
                .text_xs()
                .text_color(rgb(theme::TEXT_PRIMARY))
                .child(SharedString::from(approval))
        }))
        .child(
            div()
                .px(px(6.0))
                .text_xs()
                .text_color(rgb(theme::TEXT_DIM))
                .child(SharedString::from(format!(
                    "artifacts {}",
                    model.artifact_count
                ))),
        )
        .children(model.diagnostic.map(|diagnostic| {
            div()
                .px(px(6.0))
                .text_xs()
                .text_color(rgb(theme::TEXT_MUTED))
                .child(SharedString::from(diagnostic))
        }))
        .child(div().flex_1().min_h(px(0.0)).bg(rgb(theme::TERMINAL_BG)))
}
