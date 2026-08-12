//! Native shell mount for the single context dock.

use gpui::{div, Context, InteractiveElement, IntoElement, ParentElement, Render, Window};

use crate::client::model::ClientModel;
use crate::domain::id::{RequestId, TaskId};
use crate::domain::TaskCockpitResult;
use crate::ui::actions::{
    DockSelectArtifacts, DockSelectBrowser, DockSelectChanges, DockSelectFiles, DockSelectReview,
    DockSelectServices, DockSelectTerminal, DockToggleRawTerminal,
};
use crate::ui::task_cockpit::cockpit_projection::TaskCockpitLiveProjection;
use crate::ui::task_cockpit::dock::{
    ContextDock, DependencyUnavailable, DockEdge, DockProjectionError, DockTool, PointerPhase,
    PointerPress,
};
use crate::ui::tokens::{theme, Density, Scale, ThemeMode};

pub struct TaskCockpitShell {
    dock: ContextDock,
    model: Option<ClientModel>,
    projection: Option<TaskCockpitLiveProjection>,
}

impl TaskCockpitShell {
    pub fn new(edge: DockEdge) -> Self {
        Self {
            dock: ContextDock::new(edge),
            model: None,
            projection: None,
        }
    }

    pub fn dock(&self) -> &ContextDock {
        &self.dock
    }

    pub fn dock_mut(&mut self) -> &mut ContextDock {
        &mut self.dock
    }

    pub fn native_bin_mount(&self) -> Result<(), DependencyUnavailable> {
        // The production entrypoint now owns this shell directly; mounting is
        // therefore complete as soon as the wrapper has been constructed.
        Ok(())
    }

    pub fn follow_task(&mut self, task_id: TaskId) {
        if self
            .projection
            .as_ref()
            .is_some_and(|projection| projection.task_id != task_id)
        {
            self.projection = None;
        }
        self.dock.follow_task(task_id);
    }

    pub fn begin_cockpit_query(&mut self, task_id: TaskId, action_id: &'static str) {
        let mut projection = self
            .projection
            .take()
            .filter(|projection| projection.task_id == task_id)
            .unwrap_or_else(|| TaskCockpitLiveProjection::empty(task_id));
        projection.begin_query(action_id);
        self.dock.bind_cockpit_projection(projection.clone());
        self.projection = Some(projection);
    }

    pub fn apply_cockpit_result(&mut self, result: &TaskCockpitResult) {
        let Some(task_id) = self.dock.selected_task() else {
            return;
        };
        let mut projection = self
            .projection
            .take()
            .filter(|projection| projection.task_id == task_id)
            .unwrap_or_else(|| TaskCockpitLiveProjection::empty(task_id));
        projection.apply_result(result);
        self.dock.bind_cockpit_projection(projection.clone());
        self.projection = Some(projection);
    }

    pub fn live_projection(&self) -> Option<&TaskCockpitLiveProjection> {
        self.projection.as_ref()
    }

    pub fn follow_projection(&mut self, model: &ClientModel) {
        if let Some(task_id) = self.dock.selected_task() {
            if model.tasks().contains_key(&task_id) {
                if matches!(
                    self.dock.bind_from_model(model),
                    Err(DockProjectionError::BindingMismatch
                        | DockProjectionError::ForeignIdentity
                        | DockProjectionError::Unbound)
                ) {
                    // A task may retain its UI selection while the host rotates
                    // its agent/resource identity. Drop the old pane memory so
                    // a stale terminal cannot survive across that fence.
                    let edge = self.dock.edge();
                    self.dock = ContextDock::new(edge);
                    self.dock.follow_task(task_id);
                    let _ = self.dock.bind_from_model(model);
                }
            }
        }
        self.model = Some(model.clone());
    }

    pub fn selected_task(&self) -> Option<TaskId> {
        self.dock.selected_task()
    }

    pub fn active_tool(&self) -> DockTool {
        self.dock.active_tool()
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
