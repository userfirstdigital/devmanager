//! Native shell mount for the single context dock.

use gpui::{
    div, AnyElement, Context, InteractiveElement, IntoElement, ParentElement, Render, Styled,
    Window,
};

use crate::browser::{
    BrowserBounds, BrowserCommand, BrowserGatewayBindingRef, BrowserHostEvent,
    BrowserNativeCallback, BrowserNativeCallbackKind, BrowserNativeControllerError,
    BrowserNativeHostCommand, BrowserNativeIdentity, BrowserNativeLease,
    BrowserNativeShellController, BrowserPageLoadState, BrowserWorkspaceKey,
};
use crate::client::model::ClientModel;
use crate::domain::id::{ApprovalId, QuestionId, RequestId, TaskId};
use crate::domain::SemanticJournalPage;
use crate::domain::TaskCockpitResult;
use crate::protocol::CapabilitySet;
use crate::ui::actions::{
    DockSelectArtifacts, DockSelectBrowser, DockSelectChanges, DockSelectFiles, DockSelectReview,
    DockSelectServices, DockSelectTerminal, DockToggleRawTerminal,
};
use crate::ui::renderers::{RendererRegistry, SemanticJournalView};
use crate::ui::task_cockpit::cockpit_projection::TaskCockpitLiveProjection;
use crate::ui::task_cockpit::composer::{ApprovalProjection, QuestionProjection, TaskComposer};
use crate::ui::task_cockpit::dock::{
    ContextDock, DependencyUnavailable, DockEdge, DockProjectionError, DockTool, PointerPhase,
    PointerPress,
};
#[cfg(debug_assertions)]
use crate::ui::task_cockpit::timeline::PreviewPlanStep;
use crate::ui::task_cockpit::timeline::{ActivityToggleHandler, Timeline, TimelineViewport};
use crate::ui::tokens::{theme, Density, Scale, ThemeMode};

pub struct TaskCockpitShell {
    dock: ContextDock,
    model: Option<ClientModel>,
    capabilities: CapabilitySet,
    projection: Option<TaskCockpitLiveProjection>,
    browser_controller: BrowserNativeShellController,
    browser_projection: Option<BrowserNativeProjection>,
    timeline: Option<Timeline>,
    timeline_error: Option<String>,
    conversation: Option<SemanticJournalPage>,
}

/// Authoritative browser surface projection mounted by the active Task
/// Cockpit. It carries the durable Task/Agent/Context/Resource identity and
/// the disposable host lease/process-session/bounds state together; no UI
/// element is allowed to reconstruct those fields from a tab title or URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserNativeProjection {
    identity: BrowserNativeIdentity,
    workspace_key: BrowserWorkspaceKey,
    gateway: BrowserGatewayBindingRef,
    lease: BrowserNativeLease,
    bounds: BrowserBounds,
    attached: bool,
    focused: bool,
}

impl BrowserNativeProjection {
    pub fn identity(&self) -> BrowserNativeIdentity {
        self.identity
    }

    pub fn workspace_key(&self) -> &BrowserWorkspaceKey {
        &self.workspace_key
    }

    pub fn gateway(&self) -> &BrowserGatewayBindingRef {
        &self.gateway
    }

    pub fn lease(&self) -> BrowserNativeLease {
        self.lease
    }

    pub fn bounds(&self) -> BrowserBounds {
        self.bounds
    }

    pub fn attached(&self) -> bool {
        self.attached
    }

    pub fn focused(&self) -> bool {
        self.focused
    }
}

impl TaskCockpitShell {
    pub fn new(edge: DockEdge) -> Self {
        Self {
            dock: ContextDock::new(edge),
            model: None,
            capabilities: CapabilitySet::empty(),
            projection: None,
            browser_controller: BrowserNativeShellController::for_current_platform(),
            browser_projection: None,
            timeline: None,
            timeline_error: None,
            conversation: None,
        }
    }

    pub fn dock(&self) -> &ContextDock {
        &self.dock
    }

    pub fn dock_mut(&mut self) -> &mut ContextDock {
        &mut self.dock
    }

    pub fn browser_controller(&self) -> &BrowserNativeShellController {
        &self.browser_controller
    }

    pub fn browser_projection(&self) -> Option<&BrowserNativeProjection> {
        self.browser_projection.as_ref()
    }

    pub fn bind_browser_native(
        &mut self,
        identity: BrowserNativeIdentity,
        workspace_key: BrowserWorkspaceKey,
        gateway: BrowserGatewayBindingRef,
        bounds: BrowserBounds,
    ) -> Result<BrowserNativeLease, BrowserNativeControllerError> {
        let lease =
            self.browser_controller
                .bind(identity, workspace_key.clone(), gateway.clone())?;
        self.browser_projection = Some(BrowserNativeProjection {
            identity,
            workspace_key,
            gateway,
            lease,
            bounds,
            attached: self.browser_controller.is_attached(),
            focused: false,
        });
        Ok(lease)
    }

    pub fn attach_browser_native(
        &mut self,
        destination: crate::browser::BrowserNativeDestination,
        bounds: BrowserBounds,
    ) -> Result<BrowserNativeHostCommand, BrowserNativeControllerError> {
        let projection = self
            .browser_projection
            .as_mut()
            .ok_or(BrowserNativeControllerError::GatewayUnbound)?;
        let command = self.browser_controller.attach_with_gateway(
            &projection.lease,
            &projection.gateway,
            destination,
            bounds,
        )?;
        projection.bounds = bounds;
        projection.attached = true;
        Ok(command)
    }

    pub fn resize_browser_native(
        &mut self,
        bounds: BrowserBounds,
    ) -> Result<BrowserNativeHostCommand, BrowserNativeControllerError> {
        let projection = self
            .browser_projection
            .as_mut()
            .ok_or(BrowserNativeControllerError::GatewayUnbound)?;
        let command = self.browser_controller.resize(&projection.lease, bounds)?;
        projection.bounds = bounds;
        Ok(command)
    }

    pub fn focus_browser_native(
        &mut self,
        focused: bool,
    ) -> Result<BrowserNativeHostCommand, BrowserNativeControllerError> {
        let projection = self
            .browser_projection
            .as_mut()
            .ok_or(BrowserNativeControllerError::GatewayUnbound)?;
        let command = self.browser_controller.focus(&projection.lease, focused)?;
        projection.focused = focused;
        Ok(command)
    }

    pub fn submit_browser_command(
        &mut self,
        command: BrowserCommand,
    ) -> Result<BrowserNativeHostCommand, BrowserNativeControllerError> {
        let projection = self
            .browser_projection
            .as_ref()
            .ok_or(BrowserNativeControllerError::GatewayUnbound)?;
        self.browser_controller
            .submit_command(&projection.lease, command)
    }

    pub fn detach_browser_native(
        &mut self,
    ) -> Result<BrowserNativeHostCommand, BrowserNativeControllerError> {
        let projection = self
            .browser_projection
            .as_mut()
            .ok_or(BrowserNativeControllerError::GatewayUnbound)?;
        let command = self.browser_controller.detach(&projection.lease)?;
        projection.attached = false;
        projection.focused = false;
        Ok(command)
    }

    /// Clear the UI projection only after the host has accepted the matching
    /// detach command. This keeps stale callbacks visible until teardown has
    /// actually drained.
    pub fn finish_browser_detach(&mut self) -> Result<(), BrowserNativeControllerError> {
        let Some(projection) = self.browser_projection.as_ref() else {
            return Ok(());
        };
        self.browser_controller.close(&projection.lease)?;
        self.browser_projection = None;
        Ok(())
    }

    /// Convert host lifecycle events into controller-fenced callbacks. Events
    /// for another workspace are deliberately ignored before they can mutate
    /// the active dock projection.
    pub fn forward_browser_host_event(
        &self,
        event: &BrowserHostEvent,
    ) -> Option<BrowserNativeCallbackKind> {
        let projection = self.browser_projection.as_ref()?;
        let matches_workspace = match event {
            BrowserHostEvent::UrlChanged { workspace_key, .. }
            | BrowserHostEvent::TitleChanged { workspace_key, .. }
            | BrowserHostEvent::PageLoad { workspace_key, .. }
            | BrowserHostEvent::UserInput { workspace_key, .. }
            | BrowserHostEvent::DomMutation { workspace_key, .. }
            | BrowserHostEvent::AnnotationCandidate { workspace_key, .. }
            | BrowserHostEvent::AnnotationCanceled { workspace_key, .. }
            | BrowserHostEvent::AnnotationDraftReady { workspace_key, .. }
            | BrowserHostEvent::AnnotationModeChanged { workspace_key, .. }
            | BrowserHostEvent::AutomationStateChanged { workspace_key, .. }
            | BrowserHostEvent::ApprovalRequested { workspace_key, .. }
            | BrowserHostEvent::NewWindow { workspace_key, .. }
            | BrowserHostEvent::Download { workspace_key, .. }
            | BrowserHostEvent::Diagnostic { workspace_key, .. } => {
                workspace_key == &projection.workspace_key
            }
        };
        if !matches_workspace {
            return None;
        }
        let kind = match event {
            BrowserHostEvent::UrlChanged { .. }
            | BrowserHostEvent::TitleChanged { .. }
            | BrowserHostEvent::NewWindow { .. }
            | BrowserHostEvent::PageLoad {
                state: BrowserPageLoadState::Finished,
                ..
            } => BrowserNativeCallbackKind::NavigationComplete,
            BrowserHostEvent::UserInput { .. } => BrowserNativeCallbackKind::SurfaceFocused,
            _ => BrowserNativeCallbackKind::SurfaceResized,
        };
        self.browser_controller
            .take_callback(BrowserNativeCallback {
                generation: projection.lease.generation(),
                lease: projection.lease,
                kind,
            })
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
            self.timeline = None;
            self.timeline_error = None;
            self.conversation = None;
        }
        self.dock.follow_task(task_id);
    }

    /// Clear live cockpit surfaces while retaining ContextDock per-task memory
    /// (including TerminalPresentation) so returning to a task restores the view.
    pub fn clear_live_surfaces_preserving_dock_memory(&mut self) {
        self.projection = None;
        self.timeline = None;
        self.timeline_error = None;
        self.conversation = None;
        self.browser_projection = None;
        self.dock.clear_selection();
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
        if let TaskCockpitResult::Conversation(page) = result {
            self.conversation = Some(page.clone());
            if let Some(model) = self.model.clone() {
                self.project_timeline(&model, self.capabilities);
            }
        }
    }

    pub fn live_projection(&self) -> Option<&TaskCockpitLiveProjection> {
        self.projection.as_ref()
    }

    pub fn follow_projection(&mut self, model: &ClientModel, capabilities: CapabilitySet) {
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
        self.capabilities = capabilities;
        self.project_timeline(model, capabilities);
    }

    fn project_timeline(&mut self, model: &ClientModel, capabilities: CapabilitySet) {
        let Some(task_id) = self.dock.selected_task() else {
            self.timeline = None;
            self.timeline_error = None;
            return;
        };
        let Some(task) = model.tasks().get(&task_id) else {
            self.timeline = None;
            self.timeline_error = Some("This task is no longer available.".to_string());
            return;
        };
        if task.attention == crate::domain::task::TaskAttention::Failed {
            self.timeline = None;
            self.timeline_error = Some(
                "The agent didn't start. Check Settings, then use +Claude or +Codex again."
                    .to_string(),
            );
            return;
        }
        if task.primary_agent_id.is_none() {
            self.timeline = None;
            self.timeline_error =
                Some("This task is ready. An agent has not been connected yet.".to_string());
            return;
        }
        if task
            .primary_agent_id
            .is_some_and(|agent_id| !task.agents.contains_key(&agent_id))
        {
            self.timeline = None;
            self.timeline_error = Some(
                "The agent connection is still starting. Conversation will appear when it is ready."
                    .to_string(),
            );
            return;
        }
        let result = (|| {
            let journal = match self.conversation.as_ref() {
                Some(page) => SemanticJournalView::from_live_page(model, task_id, page),
                None => SemanticJournalView::from_live_projection(model, task_id),
            }
            .map_err(|error| error.to_string())?;
            let registry = RendererRegistry::standard().map_err(|error| error.to_string())?;
            Timeline::project(
                model,
                task_id,
                capabilities,
                &journal,
                &registry,
                TimelineViewport {
                    height: 280,
                    scroll_offset: 0,
                },
            )
            .map_err(|error| error.to_string())
        })();
        match result {
            Ok(mut timeline) => {
                if let Some(previous) = self.timeline.as_ref() {
                    timeline.preserve_view_state_from(previous);
                }
                self.timeline = Some(timeline);
                self.timeline_error = None;
            }
            Err(error) => {
                self.timeline = None;
                self.timeline_error = Some(
                    if error.contains("missing field") || error.contains("agent_session_id") {
                        "The agent didn't start. Check Settings, then use +Claude or +Codex again."
                            .to_string()
                    } else {
                        error
                    },
                );
            }
        }
    }

    pub fn timeline(&self) -> Option<&Timeline> {
        self.timeline.as_ref()
    }

    pub fn timeline_mut(&mut self) -> Option<&mut Timeline> {
        self.timeline.as_mut()
    }

    pub fn clear_timeline(&mut self) {
        self.timeline = None;
        self.timeline_error = None;
    }

    #[cfg(debug_assertions)]
    pub(crate) fn install_preview_plan_steps(
        &mut self,
        task_id: TaskId,
        steps: &[PreviewPlanStep],
    ) {
        self.timeline = Some(Timeline::for_preview_plan_steps(task_id, steps));
        self.timeline_error = None;
    }

    pub fn conversation_hold_message(&self) -> Option<&str> {
        self.timeline_error.as_deref()
    }

    pub fn pending_question_projection(
        &self,
        open_question: QuestionId,
        fallback_revision: u64,
    ) -> QuestionProjection {
        self.conversation
            .as_ref()
            .and_then(|page| question_projection_from_page(page, open_question))
            .unwrap_or(QuestionProjection {
                request_id: RequestId::from_bytes(*open_question.as_bytes())
                    .expect("provider question ids are UUIDv7"),
                state_revision: fallback_revision,
                options: Vec::new(),
            })
    }

    pub fn pending_approval_projection(
        &self,
        open_approval: ApprovalId,
        fallback_revision: u64,
    ) -> ApprovalProjection {
        ApprovalProjection {
            request_id: RequestId::from_bytes(*open_approval.as_bytes())
                .expect("provider approval ids are UUIDv7"),
            state_revision: self
                .conversation
                .as_ref()
                .map(|page| page.through_sequence)
                .unwrap_or(fallback_revision),
        }
    }

    pub fn conversation_surface(
        &self,
        tokens: crate::ui::tokens::ThemeTokens,
        composer: Option<&TaskComposer>,
    ) -> AnyElement {
        let footer = composer
            .map(|composer| composer.surface(tokens))
            .unwrap_or_else(|| self.conversation_hold_footer(tokens));
        self.conversation_surface_with_footer(tokens, footer)
    }

    /// Mount the timeline with the one interactive composer owned by the
    /// caller. The native shell uses this seam so it does not paint a second,
    /// disconnected prompt row below the real task composer.
    pub fn conversation_surface_with_footer(
        &self,
        tokens: crate::ui::tokens::ThemeTokens,
        footer: AnyElement,
    ) -> AnyElement {
        self.conversation_surface_with_footer_and_activity_handler(tokens, footer, None)
    }

    pub fn conversation_surface_with_footer_and_activity_handler(
        &self,
        tokens: crate::ui::tokens::ThemeTokens,
        footer: AnyElement,
        activity_toggle: Option<ActivityToggleHandler>,
    ) -> AnyElement {
        div()
            .id("native-task-conversation-surface")
            .w_full()
            .flex_1()
            .min_h(gpui::px(0.0))
            .flex()
            .flex_col()
            .child(self.conversation_timeline_surface(tokens, activity_toggle))
            .child(footer)
            .into_any_element()
    }

    fn conversation_timeline_surface(
        &self,
        tokens: crate::ui::tokens::ThemeTokens,
        activity_toggle: Option<ActivityToggleHandler>,
    ) -> AnyElement {
        let timeline_error = self.timeline_error.clone().unwrap_or_else(|| {
            if self.dock.selected_task().is_none() {
                "Add a project, then create a task. The conversation fills in once a task is selected.".to_string()
            } else {
                "Semantic timeline unavailable until an authenticated journal is admitted".to_string()
            }
        });
        self.timeline
            .as_ref()
            .map(|timeline| {
                div()
                    .id("native-semantic-timeline")
                    .w_full()
                    .flex_1()
                    .min_h(gpui::px(0.0))
                    .overflow_hidden()
                    .child(timeline.surface_with_activity_handler(tokens, activity_toggle))
                    .into_any_element()
            })
            .unwrap_or_else(move || {
                // A hold is an expected state, not an error surface. Centred
                // secondary copy keeps the panel composed while it waits, so a
                // bound timeline is what draws the eye once one is admitted.
                div()
                    .id("native-semantic-timeline-hold")
                    .w_full()
                    .flex_1()
                    .min_h(gpui::px(0.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .p(gpui::px(tokens.density.spacing.xl))
                    .child(
                        div()
                            .w_full()
                            .min_w(gpui::px(0.0))
                            .max_w(gpui::px(340.0))
                            .text_center()
                            .text_size(gpui::px(tokens.density.typography.caption))
                            .line_height(gpui::px(tokens.density.typography.caption_line_height))
                            .text_color(tokens.text.muted.to_gpui())
                            .child(timeline_error),
                    )
                    .into_any_element()
            })
    }

    fn conversation_hold_footer(&self, tokens: crate::ui::tokens::ThemeTokens) -> AnyElement {
        div()
            .id("native-task-composer-hold")
            .w_full()
            .flex_none()
            .p(gpui::px(tokens.density.spacing.md))
            .border_t(gpui::px(1.0))
            .border_color(tokens.borders.subtle.to_gpui())
            .child(
                div()
                    .w_full()
                    .px(gpui::px(tokens.density.spacing.md))
                    .py(gpui::px(tokens.density.spacing.sm))
                    .rounded(gpui::px(tokens.density.radii.md))
                    .bg(tokens.surfaces.disabled.to_gpui())
                    .border(gpui::px(1.0))
                    .border_color(tokens.borders.subtle.to_gpui())
                    .text_size(gpui::px(tokens.density.typography.caption))
                    .line_height(gpui::px(tokens.density.typography.caption_line_height))
                    .text_color(tokens.text.disabled.to_gpui())
                    .child("Task composer unavailable until a primary agent is bound"),
            )
            .into_any_element()
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

fn question_projection_from_page(
    page: &SemanticJournalPage,
    open_question: QuestionId,
) -> Option<QuestionProjection> {
    let options = page.facts.iter().rev().find_map(|fact| {
        if fact.redacted {
            return None;
        }
        match &fact.payload {
            crate::domain::SemanticJournalPayload::Question { options, .. } => {
                Some(options.clone())
            }
            _ => None,
        }
    })?;
    Some(QuestionProjection {
        request_id: RequestId::from_bytes(*open_question.as_bytes()).ok()?,
        state_revision: page.through_sequence,
        options,
    })
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

#[cfg(test)]
mod ai_acceptance_tests {
    use super::*;
    use crate::domain::{
        EventId, PrivacyClass, QuestionId, SemanticJournalFact, SemanticJournalPage,
        SemanticJournalPayload,
    };

    #[test]
    fn ai_acceptance_open_question_binds_authenticated_options_and_revision() {
        let open_question = QuestionId::new();
        let fact_id = EventId::new();
        let page = SemanticJournalPage {
            after_sequence: 0,
            through_sequence: 17,
            high_water: 17,
            encoded_bytes: 0,
            next_sequence: None,
            facts: vec![SemanticJournalFact {
                id: fact_id,
                sequence: 17,
                occurred_at_ms: None,
                provider: "claude_code".into(),
                schema_version: 1,
                kind: "question".into(),
                visibility: "normal".into(),
                privacy_class: PrivacyClass::Shareable,
                redacted: false,
                payload: SemanticJournalPayload::Question {
                    question_id: "provider-opaque-question".into(),
                    prompt: "Choose the acceptance color".into(),
                    options: vec!["Green".into(), "Blue".into()],
                },
            }],
        };

        let projection = question_projection_from_page(&page, open_question)
            .expect("the exact open provider question must bind");
        assert_eq!(projection.request_id.as_bytes(), open_question.as_bytes());
        assert_eq!(projection.state_revision, 17);
        assert_eq!(projection.options, ["Green", "Blue"]);
    }
}
