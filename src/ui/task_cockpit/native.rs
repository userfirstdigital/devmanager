//! Native-next GPUI boundary for the task header and one global top bar.
//!
//! The renderer consumes only bounded projections supplied by the isolated
//! host attachment. It never reads `NativeShell`, session persistence, or
//! provider runtime state.

use gpui::{
    div, App, AppContext, Application, Context, InteractiveElement, IntoElement, KeyBinding,
    ParentElement, Render, SharedString, Styled, Window, WindowOptions,
};
use gpui_component::button::Button;
use gpui_component::menu::{DropdownMenu, PopupMenuItem};

use crate::assets::AppAssets;
use crate::client::ClientModel;
use crate::ui::components::AccessibleRole;
use crate::ui::shell::Shell;

use super::header::TitleLayout;
use super::{
    ActionTarget, AgentProjection, HeaderField, HeaderLayout, OverflowControl,
    PrimaryAgentProjection, ProjectedAction, QuotaProjection, TaskHeaderModel, TopBarModel,
    TopBarProjectionController, TopBarProjectionError, TopBarProjectionInput, TopBarStatusLink,
    WorkspaceProjection, NARROW_HEADER_WIDTH_PX,
};

gpui::actions!(native_next_task_cockpit, [OpenTaskDetailsAction]);

/// A typed sink owned by the host attachment. The renderer dispatches directly
/// to this sink; it never accumulates actions in an unbounded local queue.
pub trait NativeNextActionDispatcher {
    fn dispatch(&mut self, action: ProjectedAction) -> bool;
}

/// Register the GPUI action and keyboard shortcut owned by the native-next
/// task cockpit.
pub fn bind_native_next_actions(cx: &mut App) {
    cx.bind_keys([KeyBinding::new("ctrl-m", OpenTaskDetailsAction, None)]);
}

/// The only immutable projection consumed by the native-next renderer.
#[derive(Clone, Debug, PartialEq)]
pub struct NativeNextTaskCockpitProjection {
    pub header: Option<TaskHeaderModel>,
    pub top_bar: TopBarModel,
}

impl NativeNextTaskCockpitProjection {
    pub fn new(header: Option<TaskHeaderModel>, top_bar: TopBarModel) -> Self {
        Self { header, top_bar }
    }

    pub fn from_client_model(model: &ClientModel, shell: &Shell, top_bar: TopBarModel) -> Self {
        Self::new(shell.task_header(model), top_bar)
    }

    pub fn from_client_model_with_controller(
        model: &ClientModel,
        shell: &Shell,
        top_bar: &TopBarProjectionController,
    ) -> Self {
        Self::from_client_model(model, shell, top_bar.model())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeNextHeaderMenuItem {
    pub field: HeaderField,
    pub label: String,
    pub description: String,
    pub tooltip: String,
    pub role: AccessibleRole,
    pub focusable: bool,
    pub action: ProjectedAction,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeNextHeaderMenu {
    pub label: String,
    pub description: String,
    pub tooltip: String,
    pub accessible_description: String,
    pub role: AccessibleRole,
    pub focusable: bool,
    pub items: Vec<NativeNextHeaderMenuItem>,
}

/// One render snapshot. `header_layout` is the renderer's source of truth for
/// width-dependent fields; `overflow_menu` is present only when fields are
/// actually hidden behind the accessible menu trigger.
#[derive(Clone, Debug, PartialEq)]
pub struct NativeNextTaskCockpitSurface {
    pub header: Option<TaskHeaderModel>,
    pub header_layout: Option<HeaderLayout>,
    pub overflow_control: Option<OverflowControl>,
    pub overflow_menu: Option<NativeNextHeaderMenu>,
    pub top_bar: TopBarModel,
}

/// Bounded host-client state and typed action sink consumed by the renderer.
pub struct NativeNextHostAttachment {
    model: ClientModel,
    shell: Shell,
    top_bar: TopBarProjectionController,
    dispatcher: Box<dyn NativeNextActionDispatcher>,
}

impl NativeNextHostAttachment {
    pub fn new<D>(
        model: ClientModel,
        shell: Shell,
        top_bar: TopBarProjectionController,
        dispatcher: D,
    ) -> Self
    where
        D: NativeNextActionDispatcher + 'static,
    {
        Self {
            model,
            shell,
            top_bar,
            dispatcher: Box::new(dispatcher),
        }
    }

    pub fn projection(&self) -> NativeNextTaskCockpitProjection {
        NativeNextTaskCockpitProjection::from_client_model_with_controller(
            &self.model,
            &self.shell,
            &self.top_bar,
        )
    }

    pub fn apply_projection(
        &mut self,
        model: ClientModel,
        top_bar: TopBarProjectionInput,
    ) -> Result<bool, TopBarProjectionError> {
        top_bar.preflight()?;
        let top_bar_changed = self.top_bar.apply(top_bar)?;
        let model_changed = if model.last_applied_sequence() < self.model.last_applied_sequence() {
            false
        } else if model.last_applied_sequence() == self.model.last_applied_sequence()
            && model != self.model
        {
            false
        } else if model != self.model {
            if !self.shell.sync_client_epoch(model.last_applied_sequence()) {
                return Ok(top_bar_changed);
            }
            self.model = model;
            true
        } else {
            false
        };
        Ok(top_bar_changed || model_changed)
    }

    pub fn apply_top_bar_projection(
        &mut self,
        top_bar: TopBarProjectionInput,
    ) -> Result<bool, TopBarProjectionError> {
        self.top_bar.apply(top_bar)
    }

    pub fn update_shell(&mut self, shell: Shell) {
        self.shell = shell;
    }

    fn dispatch_projected_action(&mut self, action: &ProjectedAction) -> bool {
        let accepted = match action.target() {
            ActionTarget::Task(_) | ActionTarget::Agent(_) => {
                self.shell.dispatch_task_action(&self.model, action)
            }
            ActionTarget::Host { .. }
            | ActionTarget::Connect { .. }
            | ActionTarget::Update { .. }
            | ActionTarget::QuotaSummary { .. }
            | ActionTarget::Quota { .. } => self.top_bar.model().accepts_action(action),
        };
        accepted && self.dispatcher.dispatch(action.clone())
    }
}

/// GPUI renderer/controller for the native-next task cockpit.
pub struct NativeNextTaskCockpit {
    projection: NativeNextTaskCockpitProjection,
    attachment: Option<NativeNextHostAttachment>,
}

impl NativeNextTaskCockpit {
    pub fn new(projection: NativeNextTaskCockpitProjection) -> Self {
        Self {
            projection,
            attachment: None,
        }
    }

    pub fn from_host<D>(
        model: ClientModel,
        shell: Shell,
        top_bar: TopBarProjectionController,
        dispatcher: D,
    ) -> Self
    where
        D: NativeNextActionDispatcher + 'static,
    {
        Self::from_attachment(NativeNextHostAttachment::new(
            model, shell, top_bar, dispatcher,
        ))
    }

    pub fn from_attachment(attachment: NativeNextHostAttachment) -> Self {
        let projection = attachment.projection();
        Self {
            projection,
            attachment: Some(attachment),
        }
    }

    pub fn unavailable() -> Self {
        Self::new(NativeNextTaskCockpitProjection::new(
            None,
            TopBarModel::unavailable(),
        ))
    }

    pub fn projection(&self) -> &NativeNextTaskCockpitProjection {
        &self.projection
    }

    pub fn apply_host_projection(
        &mut self,
        model: ClientModel,
        top_bar: TopBarProjectionInput,
    ) -> Result<bool, TopBarProjectionError> {
        let Some(attachment) = self.attachment.as_mut() else {
            return Ok(false);
        };
        let changed = attachment.apply_projection(model, top_bar)?;
        if changed {
            self.projection = attachment.projection();
        }
        Ok(changed)
    }

    pub fn apply_top_bar_projection(
        &mut self,
        top_bar: TopBarProjectionInput,
    ) -> Result<bool, TopBarProjectionError> {
        let Some(attachment) = self.attachment.as_mut() else {
            return Ok(false);
        };
        let changed = attachment.apply_top_bar_projection(top_bar)?;
        if changed {
            self.projection = attachment.projection();
        }
        Ok(changed)
    }

    pub fn update_shell_state(&mut self, shell: Shell) {
        if let Some(attachment) = self.attachment.as_mut() {
            attachment.update_shell(shell);
        }
    }

    pub fn render_surface(&self, width_px: u16) -> NativeNextTaskCockpitSurface {
        let (header_layout, overflow_control, overflow_menu) = self
            .projection
            .header
            .as_ref()
            .map(|header| {
                let layout = header.responsive_layout(width_px);
                let overflow_control = layout.overflow_control.clone();
                let overflow_menu = build_header_menu(header, &layout);
                (Some(layout), overflow_control, overflow_menu)
            })
            .unwrap_or((None, None, None));
        NativeNextTaskCockpitSurface {
            header: self.projection.header.clone(),
            header_layout,
            overflow_control,
            overflow_menu,
            top_bar: self.projection.top_bar.clone(),
        }
    }

    pub fn activate_open_task_details(&mut self) -> bool {
        let Some(header) = self.projection.header.as_ref() else {
            return false;
        };
        let Some(control) = header
            .responsive_layout(NARROW_HEADER_WIDTH_PX.saturating_sub(1))
            .overflow_control
        else {
            return false;
        };
        self.dispatch_projected_action(&control.action)
    }

    fn dispatch_projected_action(&mut self, action: &ProjectedAction) -> bool {
        self.attachment
            .as_mut()
            .is_some_and(|attachment| attachment.dispatch_projected_action(action))
    }

    fn render_projected_button(
        cx: &mut Context<Self>,
        id: impl Into<String>,
        label: impl Into<SharedString>,
        tooltip: impl Into<SharedString>,
        action: ProjectedAction,
    ) -> gpui::AnyElement {
        let id = id.into();
        let handler = cx.listener(move |this: &mut Self, _: &gpui::ClickEvent, _window, cx| {
            if this.dispatch_projected_action(&action) {
                cx.notify();
            }
        });
        Button::new(SharedString::from(id))
            .label(label)
            .tooltip(tooltip)
            .tab_stop(true)
            .on_click(handler)
            .into_any_element()
    }

    fn render_status_link(
        cx: &mut Context<Self>,
        prefix: &str,
        link: &TopBarStatusLink,
    ) -> gpui::AnyElement {
        Self::render_projected_button(
            cx,
            format!("native-next-top-bar-{prefix}"),
            link.label.clone(),
            link.tooltip.clone(),
            link.action.clone(),
        )
    }

    fn render_quota(
        cx: &mut Context<Self>,
        index: usize,
        quota: &QuotaProjection,
    ) -> gpui::AnyElement {
        Self::render_projected_button(
            cx,
            format!("native-next-top-bar-quota-{index}"),
            format!("{}: {}", quota.provider, quota.detail),
            quota.tooltip.clone(),
            quota.action.clone(),
        )
    }

    fn render_agent(
        cx: &mut Context<Self>,
        index: usize,
        agent: &AgentProjection,
    ) -> gpui::AnyElement {
        Self::render_projected_button(
            cx,
            format!("native-next-task-agent-{index}"),
            agent.label.clone(),
            agent.tooltip.clone(),
            agent.action.clone(),
        )
    }

    fn render_header_field(
        cx: &mut Context<Self>,
        header: &TaskHeaderModel,
        layout: &HeaderLayout,
        field: HeaderField,
    ) -> gpui::AnyElement {
        match field {
            HeaderField::Title => div().child(title_text(&layout.title)).into_any_element(),
            HeaderField::Project => div()
                .child(format!("Project: {}", header.project.label))
                .into_any_element(),
            HeaderField::Workspace => div()
                .child(format!("Workspace: {}", workspace_label(&header.workspace)))
                .into_any_element(),
            HeaderField::Primary => match &header.primary {
                PrimaryAgentProjection::Present(agent) => Self::render_agent(cx, 0, agent),
                PrimaryAgentProjection::Unavailable { label, .. } => {
                    div().child(label.clone()).into_any_element()
                }
            },
            HeaderField::Specialists => {
                let mut specialists = div().flex().gap_1();
                for (index, agent) in header.specialists.iter().enumerate() {
                    specialists = specialists.child(Self::render_agent(cx, index + 1, agent));
                }
                specialists.into_any_element()
            }
            HeaderField::TurnStatus => Self::render_projected_button(
                cx,
                "native-next-task-status",
                header.status.label.clone(),
                header.status.tooltip.clone(),
                header.status.action.clone(),
            ),
        }
    }

    fn render_overflow_menu(
        cx: &mut Context<Self>,
        menu: &NativeNextHeaderMenu,
    ) -> gpui::AnyElement {
        let menu_items = menu.items.clone();
        let entity = cx.entity();
        Button::new("native-next-task-overflow")
            .label(menu.label.clone())
            .tooltip(menu.tooltip.clone())
            .tab_stop(menu.focusable)
            .dropdown_menu(move |popup, _, _| {
                menu_items.iter().cloned().fold(popup, |popup, item| {
                    let action = item.action.clone();
                    let entity = entity.clone();
                    let label = format!("{} — {}", item.label, item.description);
                    popup.item(PopupMenuItem::new(label).on_click(move |_, _, cx| {
                        entity.update(cx, |this, cx| {
                            if this.dispatch_projected_action(&action) {
                                cx.notify();
                            }
                        });
                    }))
                })
            })
            .into_any_element()
    }

    fn element(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let width_px = window_width_px(window);
        let surface = self.render_surface(width_px);
        let mut top_bar = div().flex().items_center().gap_2();
        if let Some(host) = &surface.top_bar.host {
            top_bar = top_bar.child(Self::render_status_link(cx, "host", host));
        }
        if let Some(connect) = &surface.top_bar.connect {
            top_bar = top_bar.child(Self::render_status_link(cx, "connect", connect));
        }
        if let Some(update) = &surface.top_bar.update {
            top_bar = top_bar.child(Self::render_status_link(cx, "update", update));
        }
        for (index, quota) in surface.top_bar.quotas.iter().enumerate() {
            top_bar = top_bar.child(Self::render_quota(cx, index, quota));
        }

        let mut header_element = div().flex().items_center().gap_2();
        if let (Some(header), Some(layout)) = (&surface.header, &surface.header_layout) {
            for field in &layout.inline {
                header_element =
                    header_element.child(Self::render_header_field(cx, header, layout, *field));
            }
            if let Some(menu) = &surface.overflow_menu {
                header_element = header_element.child(Self::render_overflow_menu(cx, menu));
            }
        }

        div()
            .size_full()
            .flex()
            .flex_col()
            .gap_2()
            .on_action::<OpenTaskDetailsAction>(cx.listener(|this, _, _, cx| {
                if this.activate_open_task_details() {
                    cx.notify();
                }
            }))
            .child(top_bar)
            .child(header_element)
    }
}

impl Render for NativeNextTaskCockpit {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.element(window, cx)
    }
}

fn build_header_menu(
    header: &TaskHeaderModel,
    layout: &HeaderLayout,
) -> Option<NativeNextHeaderMenu> {
    if layout.overflow.is_empty() {
        return None;
    }
    let items = layout
        .overflow
        .iter()
        .map(|field| {
            let label = header_field_label(header, *field);
            let description = header_field_description(*field);
            NativeNextHeaderMenuItem {
                field: *field,
                label: label.clone(),
                description: description.clone(),
                tooltip: description,
                role: AccessibleRole::Button,
                focusable: true,
                action: header.status.action.clone(),
            }
        })
        .collect();
    Some(NativeNextHeaderMenu {
        label: layout
            .overflow_control
            .as_ref()
            .map(|control| control.label.clone())
            .unwrap_or_else(|| "More task details".to_string()),
        description: "Open additional task header details.".to_string(),
        tooltip: layout
            .overflow_control
            .as_ref()
            .map(|control| control.tooltip.clone())
            .unwrap_or_else(|| "Open More task details".to_string()),
        accessible_description: layout.accessible_description.clone(),
        role: AccessibleRole::Menu,
        focusable: true,
        items,
    })
}

fn header_field_label(header: &TaskHeaderModel, field: HeaderField) -> String {
    match field {
        HeaderField::Title => format!("Title: {}", header.title),
        HeaderField::Project => format!("Project: {}", header.project.label),
        HeaderField::Workspace => format!("Workspace: {}", workspace_label(&header.workspace)),
        HeaderField::Primary => match &header.primary {
            PrimaryAgentProjection::Present(agent) => format!("Primary agent: {}", agent.label),
            PrimaryAgentProjection::Unavailable { label, .. } => {
                format!("Primary agent: {label}")
            }
        },
        HeaderField::Specialists => format!(
            "Specialists: {} shown, {} hidden",
            header.specialists.len(),
            header.specialist_hidden_count
        ),
        HeaderField::TurnStatus => format!("Status: {}", header.status.label),
    }
}

fn header_field_description(field: HeaderField) -> String {
    match field {
        HeaderField::Title => "Task title.".to_string(),
        HeaderField::Project => "Project identity for this task.".to_string(),
        HeaderField::Workspace => "Workspace and branch for this task.".to_string(),
        HeaderField::Primary => "Primary agent and provider for this task.".to_string(),
        HeaderField::Specialists => "Specialist agents attached to this task.".to_string(),
        HeaderField::TurnStatus => "Current task turn status.".to_string(),
    }
}

fn workspace_label(workspace: &WorkspaceProjection) -> String {
    match workspace {
        WorkspaceProjection::Main => "main workspace".to_string(),
        WorkspaceProjection::Worktree { branch, .. } => format!("worktree {branch}"),
        WorkspaceProjection::External { .. } => "external workspace".to_string(),
    }
}

fn title_text(layout: &TitleLayout) -> String {
    match layout {
        TitleLayout::SingleLine(value) | TitleLayout::Truncated(value) => value.clone(),
        TitleLayout::Wrapped(lines) => lines.join(" "),
    }
}

fn window_width_px(window: &Window) -> u16 {
    window
        .bounds()
        .size
        .width
        .to_f64()
        .round()
        .clamp(0.0, f64::from(u16::MAX)) as u16
}

/// Start the actual native-next GPUI shell. Host/client synchronization is a
/// caller-owned seam; until the Phase 4 transport is attached, the shell
/// renders an explicit unavailable state rather than fabricating facts.
pub fn run_native_next() {
    Application::new()
        .with_assets(AppAssets::new())
        .run(|cx: &mut App| {
            crate::ui::init(cx);
            bind_native_next_actions(cx);
            cx.open_window(WindowOptions::default(), |_, cx| {
                cx.new(|_| NativeNextTaskCockpit::unavailable())
            })
            .expect("native-next GPUI window must open");
        });
}

pub fn is_task_details_action(action: &ProjectedAction) -> bool {
    action.id() == crate::client::action::ACTION_TASK_SHOW
        && matches!(action.target(), ActionTarget::Task(_))
}
