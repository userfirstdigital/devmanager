//! Native-next GPUI boundary for the task header and one global top bar.
//!
//! The renderer consumes only the immutable projection supplied by the
//! native-next host-client seam.  It does not read `NativeShell`, session
//! persistence, provider sessions, or any other legacy runtime source.

use gpui::{
    div, App, AppContext, Application, Context, InteractiveElement, IntoElement, KeyBinding,
    ParentElement, Render, SharedString, Styled, Window, WindowOptions,
};
use gpui_component::button::Button;

use crate::assets::AppAssets;
use crate::client::ClientModel;
use crate::ui::shell::Shell;

use super::{
    ActionTarget, AgentProjection, OverflowControl, ProjectedAction, QuotaProjection,
    TaskHeaderModel, TopBarModel, TopBarProjectionController, TopBarStatusLink,
    NARROW_HEADER_WIDTH_PX,
};

gpui::actions!(native_next_task_cockpit, [OpenTaskDetailsAction]);

/// Register the GPUI action and the keyboard shortcut owned by the native-next
/// task cockpit.  The pure [`KeyboardAction`] model remains the source of
/// truth for resolving the shortcut; this binds that action at the actual
/// native renderer boundary.
pub fn bind_native_next_actions(cx: &mut App) {
    cx.bind_keys([KeyBinding::new("ctrl-m", OpenTaskDetailsAction, None)]);
}

/// The only projection consumed by the native-next task cockpit renderer.
///
/// The host/client integration can update this value whenever its pinned
/// snapshot or bounded top-bar observations advance.  In particular, the
/// renderer has no fallback to the legacy `NativeShell` or `session.json`.
#[derive(Clone, Debug, PartialEq)]
pub struct NativeNextTaskCockpitProjection {
    pub header: Option<TaskHeaderModel>,
    pub top_bar: TopBarModel,
}

impl NativeNextTaskCockpitProjection {
    pub fn new(header: Option<TaskHeaderModel>, top_bar: TopBarModel) -> Self {
        Self { header, top_bar }
    }

    /// Build the native-next projection from the host-client's pinned model
    /// and the shell's current selection/epochs.  This is the reusable seam
    /// consumed by a future live `HostClient` subscription; it does not read
    /// the legacy app shell or persisted session state.
    pub fn from_client_model(model: &ClientModel, shell: &Shell, top_bar: TopBarModel) -> Self {
        Self::new(shell.task_header(model), top_bar)
    }

    /// Build the projection directly from the host-client's stateful top-bar
    /// controller. The controller is the sole owner of provider quota replay
    /// ordering; the renderer receives only its immutable model snapshot.
    pub fn from_client_model_with_controller(
        model: &ClientModel,
        shell: &Shell,
        top_bar: &TopBarProjectionController,
    ) -> Self {
        Self::from_client_model(model, shell, top_bar.model())
    }
}

/// One render snapshot used by the native-next surface and its deterministic
/// tests.  There is exactly one `TopBarModel`; legacy remote/updater/quota
/// status models are deliberately not represented here.
#[derive(Clone, Debug, PartialEq)]
pub struct NativeNextTaskCockpitSurface {
    pub header: Option<TaskHeaderModel>,
    pub overflow_control: Option<OverflowControl>,
    pub top_bar: TopBarModel,
}

/// GPUI renderer/controller for the native-next task cockpit.
///
/// `pending_action` is an in-memory handoff to the host-client action
/// dispatcher.  Rendering and input only enqueue a validated projected action;
/// they do not perform host, filesystem, network, or provider work.
pub struct NativeNextTaskCockpit {
    projection: NativeNextTaskCockpitProjection,
    host_model: Option<ClientModel>,
    host_shell: Option<Shell>,
    host_top_bar: Option<TopBarProjectionController>,
    pending_action: Option<ProjectedAction>,
}

impl NativeNextTaskCockpit {
    /// Create a projection-only renderer. Actions stay disabled until the
    /// host-client constructor supplies the current Shell and top-bar
    /// controller; a captured header alone is never dispatch authority.
    pub fn new(projection: NativeNextTaskCockpitProjection) -> Self {
        Self {
            projection,
            host_model: None,
            host_shell: None,
            host_top_bar: None,
            pending_action: None,
        }
    }

    /// Construct the renderer from the isolated host-client seam.  The host
    /// snapshot and Shell stay alongside the renderer so every GPUI action is
    /// revalidated against Shell-owned current epochs at dispatch time.
    pub fn from_host(
        model: ClientModel,
        shell: Shell,
        top_bar: TopBarProjectionController,
    ) -> Self {
        let projection = NativeNextTaskCockpitProjection::from_client_model_with_controller(
            &model, &shell, &top_bar,
        );
        Self {
            projection,
            host_model: Some(model),
            host_shell: Some(shell),
            host_top_bar: Some(top_bar),
            pending_action: None,
        }
    }

    /// Replace only the current Shell state while retaining the captured
    /// projection. Navigation/focus/connection changes therefore invalidate a
    /// previously rendered action before it can reach the host dispatcher.
    pub fn update_shell_state(&mut self, shell: Shell) {
        if self.host_model.is_some() {
            self.host_shell = Some(shell);
        }
    }

    /// Explicit unavailable state used until the Phase 4 host subscription is
    /// attached. It contains no action-dispatch authority or fabricated facts.
    pub fn unavailable() -> Self {
        Self::new(NativeNextTaskCockpitProjection::new(
            None,
            TopBarModel::unavailable(),
        ))
    }

    pub fn projection(&self) -> &NativeNextTaskCockpitProjection {
        &self.projection
    }

    pub fn render_surface(&self, width_px: u16) -> NativeNextTaskCockpitSurface {
        let overflow_control = self
            .projection
            .header
            .as_ref()
            .and_then(|header| header.responsive_layout(width_px).overflow_control);
        NativeNextTaskCockpitSurface {
            header: self.projection.header.clone(),
            overflow_control,
            top_bar: self.projection.top_bar.clone(),
        }
    }

    /// Dispatch the current task's details action, equivalent to activating
    /// the responsive overflow control with Ctrl+M.  This is also used by the
    /// GPUI action callback, so the test seam exercises the production action
    /// path rather than merely inspecting accessibility metadata.
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

    pub fn take_dispatched_action(&mut self) -> Option<ProjectedAction> {
        self.pending_action.take()
    }

    fn dispatch_projected_action(&mut self, action: &ProjectedAction) -> bool {
        let accepted = match (
            self.host_model.as_ref(),
            self.host_shell.as_ref(),
            self.host_top_bar.as_ref(),
        ) {
            (Some(model), Some(shell), Some(top_bar)) => match action.target() {
                ActionTarget::Task(_) | ActionTarget::Agent(_) => {
                    shell.dispatch_task_action(model, action)
                }
                ActionTarget::Host { .. }
                | ActionTarget::Connect { .. }
                | ActionTarget::Update { .. }
                | ActionTarget::QuotaSummary { .. }
                | ActionTarget::Quota { .. } => top_bar.model().accepts_action(action),
            },
            _ => false,
        };
        if !accepted {
            return false;
        }
        self.pending_action = Some(action.clone());
        true
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

    fn render_overflow(cx: &mut Context<Self>, control: &OverflowControl) -> gpui::AnyElement {
        Self::render_projected_button(
            cx,
            "native-next-task-overflow",
            control.label.clone(),
            control.tooltip.clone(),
            control.action.clone(),
        )
    }

    fn element(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let surface = self.render_surface(NARROW_HEADER_WIDTH_PX);
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

        let mut header = div().flex().flex_col().gap_2();
        if let Some(task_header) = &surface.header {
            header = header.child(SharedString::from(task_header.title.clone()));
            header = header.child(Self::render_projected_button(
                cx,
                "native-next-task-status",
                task_header.status.label.clone(),
                task_header.status.tooltip.clone(),
                task_header.status.action.clone(),
            ));
            if let super::PrimaryAgentProjection::Present(agent) = &task_header.primary {
                header = header.child(Self::render_agent(cx, 0, agent));
            }
            for (index, agent) in task_header.specialists.iter().enumerate() {
                header = header.child(Self::render_agent(cx, index + 1, agent));
            }
            if let Some(control) = &surface.overflow_control {
                header = header.child(Self::render_overflow(cx, control));
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
            .child(header)
    }
}

impl Render for NativeNextTaskCockpit {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.element(window, cx)
    }
}

/// Start the actual native-next GPUI shell.  Host/client synchronization is a
/// caller-owned seam; until the Phase 4 transport is attached, the shell
/// renders an explicit unavailable state rather than fabricating quota or
/// task facts.
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

/// Return whether an action targets the details surface.  Keeping this small
/// helper in the renderer makes the GPUI callback's intent explicit while the
/// projected action remains opaque outside the action boundary.
pub fn is_task_details_action(action: &ProjectedAction) -> bool {
    action.id() == crate::client::action::ACTION_TASK_SHOW
        && matches!(action.target(), ActionTarget::Task(_))
}
