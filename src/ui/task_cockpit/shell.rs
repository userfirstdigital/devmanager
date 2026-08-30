//! Native shell mount for the single context dock.

use gpui::{
    div, AnyElement, Context, InteractiveElement, IntoElement, ParentElement, Render, Styled,
    Window,
};
use std::collections::BTreeMap;

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
use crate::ui::renderers::{live_target, RendererRegistry, SemanticJournalView};
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
    /// Single authoritative timeline per open task. ListState, expansion, and
    /// follow intent live here only — never duplicated as a focused clone.
    timelines: BTreeMap<TaskId, Timeline>,
    /// When set, the focused conversation surface paints this task's map entry.
    /// Cleared on projection holds so the error/hold footer can show without
    /// deleting the retained map entry used by background panes.
    focused_timeline_task: Option<TaskId>,
    timeline_error: Option<String>,
    conversation: Option<SemanticJournalPage>,
    attachment_banner: Option<String>,
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
            timelines: BTreeMap::new(),
            focused_timeline_task: None,
            timeline_error: None,
            conversation: None,
            attachment_banner: None,
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
            self.timeline_error = None;
            self.conversation = None;
            self.attachment_banner = None;
        }
        // Prefer the authoritative map entry; never clone into a second
        // current timeline that can drift from retained state.
        self.focused_timeline_task = self.timelines.contains_key(&task_id).then_some(task_id);
        self.dock.follow_task(task_id);
    }

    /// Clear live cockpit surfaces while retaining ContextDock per-task memory
    /// (including TerminalPresentation) so returning to a task restores the view.
    pub fn clear_live_surfaces_preserving_dock_memory(&mut self) {
        self.projection = None;
        self.timelines.clear();
        self.focused_timeline_task = None;
        self.timeline_error = None;
        self.conversation = None;
        self.attachment_banner = None;
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

    /// Project the merged admitted conversation page into the exact owner task
    /// timeline. Admission paths must call this instead of painting raw query
    /// pages through the selected-task cockpit helper.
    pub fn install_admitted_conversation_page(
        &mut self,
        task_id: TaskId,
        page: &SemanticJournalPage,
    ) {
        if self.dock.selected_task() == Some(task_id) {
            self.conversation = Some(page.clone());
        }
        self.project_admitted_page_into_timeline(task_id, page);
    }

    fn project_admitted_page_into_timeline(&mut self, task_id: TaskId, page: &SemanticJournalPage) {
        let high_water = page.high_water.max(page.through_sequence);
        let presentation_signature = conversation_presentation_signature(page);
        if let Some(model) = self.model.as_ref() {
            if let (Ok(target), Some(snapshot)) =
                (live_target(model, task_id), model.tasks().get(&task_id))
            {
                if self.timelines.get(&task_id).is_some_and(|timeline| {
                    timeline.matches_projection_identity(
                        high_water,
                        self.capabilities,
                        target,
                        snapshot.task.revision,
                        presentation_signature,
                    )
                }) {
                    if self.dock.selected_task() == Some(task_id) {
                        self.focused_timeline_task = Some(task_id);
                        self.timeline_error = None;
                    }
                    return;
                }
            }
        }
        self.project_page_into_timeline(task_id, page, presentation_signature);
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
            let high_water = page.high_water.max(page.through_sequence);
            let unchanged_page = self.conversation.as_ref().is_some_and(|previous| {
                previous.high_water.max(previous.through_sequence) == high_water
                    && previous.after_sequence == page.after_sequence
                    && previous.facts.len() == page.facts.len()
            });
            self.conversation = Some(page.clone());
            if let Some(model) = self.model.clone() {
                if unchanged_page {
                    // Still fence runtime/capability identity; project_timeline
                    // no-ops when the full projection identity matches.
                }
                self.project_timeline(&model, self.capabilities);
            }
        }
    }

    /// Runtime attachment became unavailable. Keep persisted conversation rows
    /// visible and surface the diagnostic as a banner, never as task failure.
    pub fn set_attachment_unavailable(&mut self, message: impl Into<String>) {
        self.attachment_banner = Some(message.into());
        if let Some(model) = self.model.clone() {
            self.project_timeline(&model, self.capabilities);
        }
    }

    pub fn set_attachment_available(&mut self) {
        self.attachment_banner = None;
    }

    pub fn attachment_banner(&self) -> Option<&str> {
        self.attachment_banner.as_deref()
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
            self.focused_timeline_task = None;
            self.timeline_error = None;
            return;
        };
        let Some(task) = model.tasks().get(&task_id) else {
            self.focused_timeline_task = None;
            self.timeline_error = Some("This task is no longer available.".to_string());
            return;
        };
        if task.attention == crate::domain::task::TaskAttention::Failed {
            // Restore/attachment failure must not erase persisted history. Keep
            // the conversation surface when facts are already installed and
            // surface the diagnostic beside it.
            if self.conversation.is_none() {
                self.focused_timeline_task = None;
                self.timeline_error = Some(
                    "The agent didn't start. Check Settings, then use +Claude or +Codex again."
                        .to_string(),
                );
                return;
            }
        }
        if task.primary_agent_id.is_none() {
            self.focused_timeline_task = None;
            self.timeline_error =
                Some("This task is ready. An agent has not been connected yet.".to_string());
            return;
        }
        if task
            .primary_agent_id
            .is_some_and(|agent_id| !task.agents.contains_key(&agent_id))
        {
            self.focused_timeline_task = None;
            self.timeline_error = Some(
                "The agent connection is still starting. Conversation will appear when it is ready."
                    .to_string(),
            );
            return;
        }
        let high_water = self
            .conversation
            .as_ref()
            .map(|page| page.high_water.max(page.through_sequence))
            .unwrap_or(0);
        let task_revision = task.task.revision;
        let Ok(target) = live_target(model, task_id) else {
            self.focused_timeline_task = None;
            self.timeline_error = Some(
                "The agent connection is still starting. Conversation will appear when it is ready."
                    .to_string(),
            );
            return;
        };
        let presentation_signature = self
            .conversation
            .as_ref()
            .map(conversation_presentation_signature)
            .unwrap_or(0);
        if self.timelines.get(&task_id).is_some_and(|timeline| {
            timeline.matches_projection_identity(
                high_water,
                capabilities,
                target,
                task_revision,
                presentation_signature,
            )
        }) {
            self.focused_timeline_task = Some(task_id);
            self.timeline_error = None;
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
                if let Some(previous) = self.timelines.remove(&task_id) {
                    timeline.preserve_view_state_from(&previous);
                }
                timeline.note_projection_identity(
                    high_water,
                    capabilities,
                    target,
                    task_revision,
                    presentation_signature,
                );
                self.timelines.insert(task_id, timeline);
                self.focused_timeline_task = Some(task_id);
                self.timeline_error = None;
            }
            Err(error) => {
                if self.dock.selected_task() == Some(task_id) {
                    self.focused_timeline_task = None;
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
    }

    pub fn timeline(&self) -> Option<&Timeline> {
        let task_id = self.focused_timeline_task?;
        self.timelines.get(&task_id)
    }

    pub fn timeline_for(&self, task_id: TaskId) -> Option<&Timeline> {
        self.timelines.get(&task_id)
    }

    pub fn timeline_mut(&mut self) -> Option<&mut Timeline> {
        let task_id = self.focused_timeline_task?;
        self.timelines.get_mut(&task_id)
    }

    pub fn timeline_mut_for(&mut self, task_id: TaskId) -> Option<&mut Timeline> {
        self.timelines.get_mut(&task_id)
    }

    pub fn clear_timeline(&mut self) {
        self.focused_timeline_task = None;
        self.timeline_error = None;
    }

    pub fn retain_open_timelines(&mut self, task_ids: &[TaskId]) {
        let valid: std::collections::BTreeSet<_> = task_ids.iter().copied().collect();
        self.timelines.retain(|task_id, _| valid.contains(task_id));
        if self
            .focused_timeline_task
            .is_some_and(|task_id| !valid.contains(&task_id))
        {
            self.focused_timeline_task = None;
        }
    }

    /// Hydrate a missing timeline once from an admitted page. Paint paths must
    /// call this instead of reprojecting every frame.
    pub fn hydrate_timeline_if_absent(&mut self, task_id: TaskId, page: &SemanticJournalPage) {
        if self.timelines.contains_key(&task_id) {
            return;
        }
        self.project_page_into_timeline(task_id, page, conversation_presentation_signature(page));
    }

    /// Project or refresh a retained timeline when admission advances the
    /// journal high-water. Unchanged high-water is a no-op.
    pub fn ensure_retained_timeline_from_page(
        &mut self,
        task_id: TaskId,
        page: &SemanticJournalPage,
    ) {
        let high_water = page.high_water.max(page.through_sequence);
        let presentation_signature = conversation_presentation_signature(page);
        if let Some(model) = self.model.as_ref() {
            if let (Ok(target), Some(snapshot)) =
                (live_target(model, task_id), model.tasks().get(&task_id))
            {
                if self.timelines.get(&task_id).is_some_and(|timeline| {
                    timeline.matches_projection_identity(
                        high_water,
                        self.capabilities,
                        target,
                        snapshot.task.revision,
                        presentation_signature,
                    )
                }) {
                    return;
                }
            } else if self.timelines.get(&task_id).is_some_and(|timeline| {
                timeline.projected_high_water() >= high_water
                    && timeline.projected_presentation_signature() == presentation_signature
            }) {
                return;
            }
        } else if self.timelines.get(&task_id).is_some_and(|timeline| {
            timeline.projected_high_water() >= high_water
                && timeline.projected_presentation_signature() == presentation_signature
        }) {
            return;
        }
        self.project_page_into_timeline(task_id, page, presentation_signature);
    }

    fn project_page_into_timeline(
        &mut self,
        task_id: TaskId,
        page: &SemanticJournalPage,
        presentation_signature: u64,
    ) {
        let Some(model) = self.model.clone() else {
            return;
        };
        let Ok(journal) = SemanticJournalView::from_live_page(&model, task_id, page) else {
            return;
        };
        let Ok(registry) = RendererRegistry::standard() else {
            return;
        };
        let Ok(mut timeline) = Timeline::project(
            &model,
            task_id,
            self.capabilities,
            &journal,
            &registry,
            TimelineViewport {
                height: 280,
                scroll_offset: 0,
            },
        ) else {
            return;
        };
        let high_water = page.high_water.max(page.through_sequence);
        if let Some(previous) = self.timelines.remove(&task_id) {
            timeline.preserve_view_state_from(&previous);
        }
        let task_revision = model
            .tasks()
            .get(&task_id)
            .map(|snapshot| snapshot.task.revision)
            .unwrap_or(0);
        if let Ok(target) = live_target(&model, task_id) {
            timeline.note_projection_identity(
                high_water,
                self.capabilities,
                target,
                task_revision,
                presentation_signature,
            );
        } else {
            timeline.note_projected_high_water(high_water);
        }
        self.timelines.insert(task_id, timeline);
        if self.dock.selected_task() == Some(task_id) {
            self.focused_timeline_task = Some(task_id);
            self.timeline_error = None;
        }
    }

    pub fn conversation_timeline_surface_for(
        &self,
        task_id: TaskId,
        tokens: crate::ui::tokens::ThemeTokens,
        activity_toggle: Option<ActivityToggleHandler>,
    ) -> AnyElement {
        self.timeline_for(task_id)
            .map(|timeline| {
                div()
                    .id(("native-semantic-timeline-pane", {
                        u64::from_be_bytes(
                            task_id.as_bytes()[8..]
                                .try_into()
                                .expect("task identity tail is exactly eight bytes"),
                        )
                    }))
                    .w_full()
                    .flex_1()
                    .min_h(gpui::px(0.0))
                    .overflow_hidden()
                    .child(timeline.surface_with_activity_handler(tokens, activity_toggle))
                    .into_any_element()
            })
            .unwrap_or_else(|| {
                div()
                    .id(("native-semantic-timeline-pane-hold", {
                        u64::from_be_bytes(
                            task_id.as_bytes()[8..]
                                .try_into()
                                .expect("task identity tail is exactly eight bytes"),
                        )
                    }))
                    .w_full()
                    .flex_1()
                    .min_h(gpui::px(0.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .p(gpui::px(tokens.density.spacing.xl))
                    .child(
                        div()
                            .text_color(tokens.text.muted.to_gpui())
                            .child("Conversation is live; waiting for messages."),
                    )
                    .into_any_element()
            })
    }

    #[cfg(debug_assertions)]
    pub(crate) fn install_preview_plan_steps(
        &mut self,
        task_id: TaskId,
        steps: &[PreviewPlanStep],
    ) {
        self.install_preview_conversation(task_id, steps, &[]);
    }

    #[cfg(debug_assertions)]
    pub(crate) fn install_preview_conversation(
        &mut self,
        task_id: TaskId,
        steps: &[PreviewPlanStep],
        messages: &[crate::ui::task_cockpit::timeline::PreviewConversationMessage],
    ) {
        let timeline = Timeline::for_preview_conversation(task_id, steps, messages);
        self.timelines.insert(task_id, timeline);
        self.focused_timeline_task = Some(task_id);
        self.timeline_error = None;
    }

    pub fn conversation_hold_message(&self) -> Option<&str> {
        self.attachment_banner
            .as_deref()
            .or_else(|| {
                let task_id = self.dock.selected_task()?;
                let task = self.model.as_ref()?.tasks().get(&task_id)?;
                (task.attention == crate::domain::task::TaskAttention::Failed).then_some(
                    "The agent didn't start. Check Settings, then use +Claude or +Codex again.",
                )
            })
            .or(self.timeline_error.as_deref())
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

    /// Mount a conversation pane for an exact task without consulting the
    /// cockpit's globally focused task. Recursive workspaces can keep several
    /// conversations visible at once, so background panes must not inherit the
    /// focused pane's timeline.
    pub fn conversation_surface_with_footer_for_task(
        &self,
        task_id: TaskId,
        tokens: crate::ui::tokens::ThemeTokens,
        footer: AnyElement,
        activity_toggle: Option<ActivityToggleHandler>,
    ) -> AnyElement {
        div()
            .id(("native-task-conversation-surface", {
                u64::from_be_bytes(
                    task_id.as_bytes()[8..]
                        .try_into()
                        .expect("task identity tail is exactly eight bytes"),
                )
            }))
            .w_full()
            .flex_1()
            .min_h(gpui::px(0.0))
            .flex()
            .flex_col()
            .child(self.conversation_timeline_surface_for(task_id, tokens, activity_toggle))
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
        self.timeline()
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

pub(crate) fn conversation_presentation_signature(page: &SemanticJournalPage) -> u64 {
    use crate::domain::SemanticJournalPayload;
    use std::hash::{Hash, Hasher};

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    page.facts.len().hash(&mut hasher);
    for fact in &page.facts {
        fact.sequence.hash(&mut hasher);
        fact.id.as_bytes().hash(&mut hasher);
        match &fact.payload {
            SemanticJournalPayload::UserMessage { text } => {
                1u8.hash(&mut hasher);
                text.hash(&mut hasher);
            }
            SemanticJournalPayload::AssistantText { text } => {
                2u8.hash(&mut hasher);
                text.hash(&mut hasher);
            }
            _ => fact.kind.hash(&mut hasher),
        }
    }
    hasher.finish()
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
            oldest_sequence: 0,
            cursor_rolled_over: false,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        EventId, PrivacyClass, SemanticJournalFact, SemanticJournalPage, SemanticJournalPayload,
    };
    use crate::ui::task_cockpit::dock::DockEdge;

    struct ShellProjectionView {
        timeline_text: String,
        attachment_banner: String,
    }

    impl TaskCockpitShell {
        fn test_projection(&self) -> ShellProjectionView {
            let timeline_text = self
                .conversation
                .as_ref()
                .map(|page| {
                    page.facts
                        .iter()
                        .filter_map(|fact| match &fact.payload {
                            SemanticJournalPayload::AssistantText { text }
                            | SemanticJournalPayload::UserMessage { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default();
            ShellProjectionView {
                timeline_text,
                attachment_banner: self.attachment_banner().unwrap_or("").to_string(),
            }
        }
    }

    fn cockpit_with_conversation(text: &str) -> TaskCockpitShell {
        let mut shell = TaskCockpitShell::new(DockEdge::Right);
        let task_id = TaskId::new();
        shell.follow_task(task_id);
        let page = SemanticJournalPage {
            oldest_sequence: 0,
            cursor_rolled_over: false,
            after_sequence: 0,
            through_sequence: 1,
            high_water: 1,
            encoded_bytes: 1,
            next_sequence: None,
            facts: vec![SemanticJournalFact {
                id: EventId::new(),
                sequence: 1,
                occurred_at_ms: None,
                provider: "test".into(),
                schema_version: 1,
                kind: "assistant_text".into(),
                visibility: "conversation".into(),
                privacy_class: PrivacyClass::LocalOnly,
                redacted: false,
                payload: SemanticJournalPayload::AssistantText { text: text.into() },
            }],
        };
        shell.apply_cockpit_result(&TaskCockpitResult::Conversation(page));
        shell
    }

    #[test]
    fn attachment_failure_keeps_persisted_conversation_rows_visible() {
        let mut shell = cockpit_with_conversation("saved answer");
        shell.set_attachment_unavailable("resume failed");

        let projection = shell.test_projection();

        assert!(projection.timeline_text.contains("saved answer"));
        assert!(projection.attachment_banner.contains("resume failed"));
    }

    #[test]
    fn successful_reattachment_clears_only_the_transient_attachment_banner() {
        let mut shell = cockpit_with_conversation("saved answer");
        shell.set_attachment_unavailable("resume failed");

        shell.set_attachment_available();
        let projection = shell.test_projection();

        assert!(projection.timeline_text.contains("saved answer"));
        assert!(projection.attachment_banner.is_empty());
    }

    #[test]
    fn unchanged_high_water_skips_timeline_reproject() {
        use crate::ui::conversation::fixtures::message_item;
        use crate::ui::renderers::MessageRole;

        let mut shell = TaskCockpitShell::new(DockEdge::Right);
        let mut timeline =
            Timeline::for_test_items(vec![message_item(MessageRole::Assistant, "cached")]);
        let task_id = timeline.task_id();
        timeline.note_projected_high_water(4);
        let rows_ptr = timeline.rows().as_ptr();
        shell.dock.follow_task(task_id);
        shell.timelines.insert(task_id, timeline);
        shell.focused_timeline_task = Some(task_id);

        let page = SemanticJournalPage {
            oldest_sequence: 0,
            cursor_rolled_over: false,
            after_sequence: 0,
            through_sequence: 4,
            high_water: 4,
            encoded_bytes: 0,
            next_sequence: None,
            facts: Vec::new(),
        };
        shell.ensure_retained_timeline_from_page(task_id, &page);
        shell.hydrate_timeline_if_absent(task_id, &page);

        let retained = shell.timelines.get(&task_id).expect("timeline retained");
        assert_eq!(retained.projected_high_water(), 4);
        assert_eq!(retained.rows().as_ptr(), rows_ptr);
    }

    #[test]
    fn conversation_presentation_signature_distinguishes_same_high_water_rows() {
        let first = SemanticJournalPage {
            oldest_sequence: 0,
            cursor_rolled_over: false,
            after_sequence: 4,
            through_sequence: 5,
            high_water: 7,
            encoded_bytes: 0,
            next_sequence: None,
            facts: vec![SemanticJournalFact {
                id: EventId::new(),
                sequence: 5,
                occurred_at_ms: None,
                provider: "test".into(),
                schema_version: 1,
                kind: "user_message".into(),
                visibility: "conversation".into(),
                privacy_class: PrivacyClass::LocalOnly,
                redacted: false,
                payload: SemanticJournalPayload::UserMessage {
                    text: "first".into(),
                },
            }],
        };
        let second = SemanticJournalPage {
            facts: vec![SemanticJournalFact {
                payload: SemanticJournalPayload::UserMessage {
                    text: "second".into(),
                },
                ..first.facts[0].clone()
            }],
            ..first.clone()
        };
        assert_ne!(
            super::conversation_presentation_signature(&first),
            super::conversation_presentation_signature(&second),
            "presentation rows must invalidate cache identity at unchanged durable high-water"
        );
    }

    #[test]
    fn activity_expand_survives_focus_switch_on_single_timeline_map() {
        use crate::ui::conversation::fixtures::{message_item, tool_item};
        use crate::ui::renderers::MessageRole;

        let mut shell = TaskCockpitShell::new(DockEdge::Right);
        let mut timeline = Timeline::for_test_items(vec![
            tool_item("tool-1", "Read", "completed"),
            tool_item("tool-2", "Read", "completed"),
            tool_item("tool-3", "Bash", "completed"),
            message_item(MessageRole::Assistant, "tail"),
        ]);
        let task_id = timeline.task_id();
        let group = timeline
            .rows()
            .iter()
            .find_map(|row| match row {
                crate::ui::conversation::rows::ConversationRow::ActivityToggle {
                    group, ..
                } => Some(group.clone()),
                _ => None,
            })
            .expect("toggle");
        assert!(timeline.toggle_activity_group(&group));

        let other = TaskId::new();
        let other_timeline = Timeline::for_test_task_items(
            other,
            vec![message_item(MessageRole::Assistant, "other")],
        );

        shell.dock.follow_task(task_id);
        shell.timelines.insert(task_id, timeline);
        shell.timelines.insert(other, other_timeline);
        shell.focused_timeline_task = Some(task_id);
        shell.projection = Some(super::TaskCockpitLiveProjection::empty(task_id));

        shell.follow_task(other);
        assert_eq!(shell.focused_timeline_task, Some(other));
        shell.follow_task(task_id);
        assert_eq!(shell.focused_timeline_task, Some(task_id));
        let restored = shell
            .timeline_for(task_id)
            .expect("authoritative map entry");
        assert!(restored.rows().iter().any(|row| matches!(
            row,
            crate::ui::conversation::rows::ConversationRow::Activity { entries, .. }
                if entries.len() == 3
        )));
    }

    #[test]
    fn retain_open_timelines_keeps_focused_membership_only() {
        use crate::ui::conversation::fixtures::message_item;
        use crate::ui::renderers::MessageRole;

        let mut shell = TaskCockpitShell::new(DockEdge::Right);
        let first = Timeline::for_test_items(vec![message_item(MessageRole::Assistant, "one")]);
        let first_id = first.task_id();
        let second_id = TaskId::new();
        let second = Timeline::for_test_task_items(
            second_id,
            vec![message_item(MessageRole::Assistant, "two")],
        );
        shell.timelines.insert(first_id, first);
        shell.timelines.insert(second_id, second);
        shell.focused_timeline_task = Some(first_id);

        shell.retain_open_timelines(&[first_id]);
        assert!(shell.timelines.contains_key(&first_id));
        assert!(!shell.timelines.contains_key(&second_id));
        assert_eq!(shell.focused_timeline_task, Some(first_id));
    }
}
