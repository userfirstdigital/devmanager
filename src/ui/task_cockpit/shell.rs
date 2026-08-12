//! Native shell mount for the single context dock.

use gpui::{div, Context, InteractiveElement, IntoElement, ParentElement, Render, Window};

use crate::client::model::ClientModel;
use crate::domain::id::{RequestId, TaskId};
use crate::ui::actions::{
    DockSelectArtifacts, DockSelectBrowser, DockSelectChanges, DockSelectFiles, DockSelectReview,
    DockSelectServices, DockSelectTerminal, DockToggleRawTerminal,
};
use crate::ui::task_cockpit::dock::{
    ContextDock, DependencyUnavailable, DockEdge, DockProjectionError, DockTool, PointerPhase,
    PointerPress,
};
use crate::ui::tokens::{theme, Density, Scale, ThemeMode};

pub struct TaskCockpitShell {
    dock: ContextDock,
    model: Option<ClientModel>,
}

impl TaskCockpitShell {
    pub fn new(edge: DockEdge) -> Self {
        Self {
            dock: ContextDock::new(edge),
            model: None,
        }
    }

    pub fn dock(&self) -> &ContextDock {
        &self.dock
    }

    pub fn dock_mut(&mut self) -> &mut ContextDock {
        &mut self.dock
    }

    pub fn native_bin_mount(&self) -> Result<(), DependencyUnavailable> {
        Err(DependencyUnavailable::NativeShellMount)
    }

    pub fn follow_task(&mut self, task_id: TaskId) {
        self.dock.follow_task(task_id);
    }

    pub fn follow_projection(&mut self, model: ClientModel) {
        if let Some(task_id) = self.dock.selected_task() {
            if model.tasks().contains_key(&task_id) {
                let _ = self.dock.bind_from_model(&model);
            }
        }
        self.model = Some(model);
    }

    pub fn handle_tool_action(
        &mut self,
        tool: DockTool,
        request_id: RequestId,
    ) -> Result<(), DockProjectionError> {
        let Some(model) = self.model.clone() else {
            return Err(DockProjectionError::NoTaskSelected);
        };
        let dispatch = self.dock.capture_action(tool, request_id)?;
        self.dock.dispatch_action(dispatch, &model)
    }

    pub fn handle_toggle_raw(&mut self, request_id: RequestId) -> Result<(), DockProjectionError> {
        let Some(model) = self.model.clone() else {
            return Err(DockProjectionError::NoTaskSelected);
        };
        self.dock.dispatch_shortcut(
            crate::ui::task_cockpit::dock::DockShortcut::ToggleRawTerminal,
            request_id,
            &model,
        )
    }

    pub fn handle_gpui_pointer(&mut self, phase: PointerPhase, press: PointerPress) -> bool {
        self.dock.handle_gpui_pointer(phase, press)
    }
}

impl Render for TaskCockpitShell {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tokens = theme(ThemeMode::Dark, Density::Compact, Scale::Scale100);
        div()
            .on_action(cx.listener(|this, _: &DockSelectChanges, _, _| {
                let _ = this.handle_tool_action(DockTool::Changes, RequestId::new());
            }))
            .on_action(cx.listener(|this, _: &DockSelectFiles, _, _| {
                let _ = this.handle_tool_action(DockTool::Files, RequestId::new());
            }))
            .on_action(cx.listener(|this, _: &DockSelectTerminal, _, _| {
                let _ = this.handle_tool_action(DockTool::Terminal, RequestId::new());
            }))
            .on_action(cx.listener(|this, _: &DockSelectBrowser, _, _| {
                let _ = this.handle_tool_action(DockTool::Browser, RequestId::new());
            }))
            .on_action(cx.listener(|this, _: &DockSelectServices, _, _| {
                let _ = this.handle_tool_action(DockTool::Services, RequestId::new());
            }))
            .on_action(cx.listener(|this, _: &DockSelectArtifacts, _, _| {
                let _ = this.handle_tool_action(DockTool::Artifacts, RequestId::new());
            }))
            .on_action(cx.listener(|this, _: &DockSelectReview, _, _| {
                let _ = this.handle_tool_action(DockTool::Review, RequestId::new());
            }))
            .on_action(cx.listener(|this, _: &DockToggleRawTerminal, _, _| {
                let _ = this.handle_toggle_raw(RequestId::new());
            }))
            .child(self.dock.render_context_dock(tokens))
    }
}
