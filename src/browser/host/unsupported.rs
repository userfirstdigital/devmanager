#[cfg(not(target_os = "windows"))]
use super::super::{
    apply_browser_workflow_review_mutation, browser_workflow_review_projection,
    discard_browser_workflow_review, preview_browser_workflow_review, save_browser_workflow_review,
    BrowserBounds, BrowserCommand, BrowserCommandRequest, BrowserHostControl, BrowserHostEvent,
    BrowserPageRecordingIpcError, BrowserPaneSurface, BrowserRecipeV1, BrowserRecordingError,
    BrowserRecordingInstance, BrowserRecordingReview, BrowserRecordingStatus, BrowserResponse,
    BrowserWorkflowCoordinator, BrowserWorkflowReviewMutation, BrowserWorkflowReviewProjection,
    BrowserWorkspaceKey,
};
use super::super::{BrowserError, BrowserHostStatus};
#[cfg(not(target_os = "windows"))]
use super::{BrowserHostState, BrowserWorkspaceSnapshot};
#[cfg(not(target_os = "windows"))]
use crate::browser::BrowserAttachmentProjection;
#[cfg(not(target_os = "windows"))]
use std::{
    marker::PhantomData,
    path::{Path, PathBuf},
    rc::Rc,
};

pub fn unsupported_host_status(platform: impl Into<String>) -> BrowserHostStatus {
    let platform = platform.into();
    BrowserHostStatus {
        available: false,
        diagnostic: Some(format!(
            "embedded browser support is unavailable on {platform}"
        )),
        platform,
        version: None,
    }
}

pub fn unsupported_platform_error(platform: impl Into<String>) -> BrowserError {
    BrowserError::UnavailablePlatform {
        platform: platform.into(),
    }
}

#[cfg(not(target_os = "windows"))]
pub struct BrowserWebViewHost {
    status: BrowserHostStatus,
    #[allow(dead_code)]
    state: BrowserHostState,
    workflow_coordinator: BrowserWorkflowCoordinator,
    _main_thread_only: PhantomData<Rc<()>>,
}

#[cfg(not(target_os = "windows"))]
impl BrowserWebViewHost {
    pub fn new(app_config_dir: impl AsRef<Path>) -> Self {
        Self {
            status: unsupported_host_status(std::env::consts::OS),
            state: BrowserHostState::new(app_config_dir),
            workflow_coordinator: BrowserWorkflowCoordinator::default(),
            _main_thread_only: PhantomData,
        }
    }

    pub fn unavailable(diagnostic: impl Into<String>) -> Self {
        Self {
            status: BrowserHostStatus {
                available: false,
                diagnostic: Some(diagnostic.into()),
                platform: std::env::consts::OS.to_string(),
                version: None,
            },
            state: BrowserHostState::new(PathBuf::new()),
            workflow_coordinator: BrowserWorkflowCoordinator::default(),
            _main_thread_only: PhantomData,
        }
    }

    pub fn status(&self) -> BrowserHostStatus {
        self.status.clone()
    }

    pub fn trusted_app_config_dir(&self) -> Option<&Path> {
        None
    }

    pub fn page_recording_status(
        &self,
        _workspace_key: &BrowserWorkspaceKey,
    ) -> BrowserRecordingStatus {
        BrowserRecordingStatus::Inactive
    }

    pub fn page_recording_instance(
        &self,
        workspace_key: &BrowserWorkspaceKey,
    ) -> Option<BrowserRecordingInstance> {
        self.workflow_coordinator.current_instance(workspace_key)
    }

    pub fn workflow_review_projection(
        &self,
        workspace_key: &BrowserWorkspaceKey,
        surface: BrowserPaneSurface,
    ) -> Option<BrowserWorkflowReviewProjection> {
        browser_workflow_review_projection(&self.workflow_coordinator, workspace_key, surface)
    }

    pub fn apply_workflow_review_mutation(
        &mut self,
        active_workspace: Option<&BrowserWorkspaceKey>,
        action_workspace: &BrowserWorkspaceKey,
        surface: BrowserPaneSurface,
        instance_id: u64,
        mutation: BrowserWorkflowReviewMutation,
    ) -> Result<BrowserWorkflowReviewProjection, BrowserRecordingError> {
        apply_browser_workflow_review_mutation(
            &self.workflow_coordinator,
            active_workspace,
            action_workspace,
            surface,
            instance_id,
            mutation,
        )
    }

    pub fn preview_workflow_review(
        &self,
        active_workspace: Option<&BrowserWorkspaceKey>,
        action_workspace: &BrowserWorkspaceKey,
        surface: BrowserPaneSurface,
        instance_id: u64,
    ) -> Result<BrowserRecipeV1, BrowserError> {
        preview_browser_workflow_review(
            &self.workflow_coordinator,
            active_workspace,
            action_workspace,
            surface,
            instance_id,
        )
    }

    pub fn save_workflow_review(
        &mut self,
        active_workspace: Option<&BrowserWorkspaceKey>,
        action_workspace: &BrowserWorkspaceKey,
        surface: BrowserPaneSurface,
        instance_id: u64,
        project_root: impl AsRef<Path>,
        remote_client: bool,
    ) -> Result<PathBuf, BrowserError> {
        save_browser_workflow_review(
            &self.workflow_coordinator,
            active_workspace,
            action_workspace,
            surface,
            instance_id,
            project_root,
            remote_client,
        )
    }

    pub fn discard_workflow_review(
        &mut self,
        active_workspace: Option<&BrowserWorkspaceKey>,
        action_workspace: &BrowserWorkspaceKey,
        surface: BrowserPaneSurface,
        instance_id: u64,
    ) -> Result<(), BrowserError> {
        discard_browser_workflow_review(
            &self.workflow_coordinator,
            active_workspace,
            action_workspace,
            surface,
            instance_id,
        )
    }

    pub fn discard_workflow_state(&mut self, workspace_key: &BrowserWorkspaceKey) {
        let Some(instance) = self.workflow_coordinator.current_instance(workspace_key) else {
            return;
        };
        if self.workflow_coordinator.status(workspace_key) == BrowserRecordingStatus::Recording {
            let _ = self.workflow_coordinator.stop(&instance);
        }
        let _ = self.workflow_coordinator.discard(&instance);
    }

    pub fn start_page_recording(
        &mut self,
        _workspace_key: &BrowserWorkspaceKey,
    ) -> Result<BrowserRecordingInstance, BrowserPageRecordingIpcError> {
        Err(BrowserPageRecordingIpcError::Unavailable)
    }

    pub fn stop_page_recording(
        &mut self,
        _instance: &BrowserRecordingInstance,
    ) -> Result<BrowserRecordingReview, BrowserPageRecordingIpcError> {
        Err(BrowserPageRecordingIpcError::Unavailable)
    }

    pub fn handle_command(
        &mut self,
        _window: &gpui::Window,
        _workspace_key: &super::super::BrowserWorkspaceKey,
        command: BrowserCommand,
    ) -> Result<BrowserResponse, BrowserError> {
        if command == BrowserCommand::Status {
            Ok(BrowserResponse::Status {
                status: self.status(),
            })
        } else {
            Err(unsupported_platform_error(std::env::consts::OS))
        }
    }

    pub fn handle_request(&mut self, window: &gpui::Window, request: BrowserCommandRequest) {
        if !request.cancellation_is_current() {
            request.respond(Err(BrowserError::Interrupted));
            return;
        }
        let result =
            self.handle_command(window, request.workspace_key(), request.command().clone());
        request.respond(result);
    }

    pub fn handle_control(&mut self, _control: BrowserHostControl) {}

    pub fn pump_async_completions(&mut self, _window: &gpui::Window) {}

    pub fn is_pending_approval(
        &self,
        _workspace_key: &super::super::BrowserWorkspaceKey,
        _tab_id: &str,
        _operation_id: &str,
    ) -> bool {
        false
    }

    pub fn resolve_approval(
        &mut self,
        _window: &gpui::Window,
        _workspace_key: &super::super::BrowserWorkspaceKey,
        _tab_id: &str,
        _operation_id: &str,
        _approved: bool,
    ) -> Result<(), BrowserError> {
        Err(unsupported_platform_error(std::env::consts::OS))
    }

    pub fn set_active_workspace(
        &mut self,
        _workspace_key: Option<super::super::BrowserWorkspaceKey>,
    ) -> Result<(), BrowserError> {
        Err(unsupported_platform_error(std::env::consts::OS))
    }

    pub fn set_bounds(&mut self, _bounds: BrowserBounds) -> Result<(), BrowserError> {
        Err(unsupported_platform_error(std::env::consts::OS))
    }

    pub fn drain_events(&mut self) -> Vec<BrowserHostEvent> {
        Vec::new()
    }

    pub fn workspace_snapshot(
        &self,
        workspace_key: &super::super::BrowserWorkspaceKey,
    ) -> Option<&BrowserWorkspaceSnapshot> {
        self.state.workspace(workspace_key)
    }

    pub fn acknowledge_attachment_projection(
        &mut self,
        projection: &BrowserAttachmentProjection,
    ) -> Result<BrowserWorkspaceSnapshot, BrowserError> {
        self.state
            .acknowledge_attachment_projection(
                &projection.workspace_key,
                projection.revision,
                &projection.pending_annotation_ids,
                &projection.tombstone_annotation_ids,
            )
            .map(|mutation| mutation.snapshot)
    }
}
