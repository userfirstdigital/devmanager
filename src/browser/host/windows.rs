use super::{
    acknowledge_attachment_projection_and_reconcile_pins, browser_user_input_initialization_script,
    validate_browser_url, BrowserHostState, BrowserMemoryTarget,
};
use crate::browser::downloads::{
    prepare_verified_storage_layout, verified_app_config_root, verified_unique_download_path,
    verify_prepared_storage_root,
};
use crate::browser::{
    apply_browser_workflow_review_mutation, browser_cdp_method_risk, browser_lifecycle_control,
    browser_operation_target_tab_id, browser_page_origin_from_url, browser_recording_review_result,
    browser_recording_save_would_overwrite, browser_recording_status_result,
    browser_request_preempts_operation_queue, browser_response_resource_ids,
    browser_workflow_review_projection, build_semantic_snapshot, crop_annotation_png,
    discard_browser_recording, discard_browser_workflow_review, effective_browser_annotation_risk,
    effective_browser_recording_risk, effective_browser_risk, effective_browser_risk_for_targets,
    parse_browser_page_ipc_message, prepare_verified_download_root,
    preview_browser_workflow_review, recording_resource_unavailable, redact_browser_resource_bytes,
    redact_browser_text, remove_verified_profile, save_browser_recording_review,
    save_browser_workflow_review, validate_annotation_candidate_context,
    validate_direct_secret_command, BrowserAction, BrowserActionResult, BrowserActionTarget,
    BrowserAnnotationCandidate, BrowserAnnotationCleanupLedger, BrowserAnnotationDraft,
    BrowserAnnotationLifecycle, BrowserAnnotationRoute, BrowserApprovalPolicy,
    BrowserApprovalRequest, BrowserAttachmentProjection, BrowserBounds, BrowserCommand,
    BrowserCommandRequest, BrowserConsoleEntry, BrowserConsoleOperation, BrowserDiagnosticLevel,
    BrowserDownloadState, BrowserDownloadStore, BrowserError, BrowserHostControl, BrowserHostEvent,
    BrowserHostStatus, BrowserInvocationActor, BrowserJournalActor, BrowserJournalEntry,
    BrowserNetworkEntry, BrowserNetworkOperation, BrowserOperationQueue, BrowserOperationTarget,
    BrowserPageIpcMessage, BrowserPageLoadState, BrowserPageRecordingAuthority,
    BrowserPageRecordingEnvelope, BrowserPageRecordingIngress, BrowserPageRecordingIpc,
    BrowserPageRecordingIpcError, BrowserPageRecordingSubmit, BrowserPageRecordingTransport,
    BrowserPageRecordingTransportFailureKind, BrowserPaneSurface, BrowserPerformanceOperation,
    BrowserPerformanceSnapshot, BrowserRawSemanticElement, BrowserRecipeV1, BrowserRecordingError,
    BrowserRecordingInstance, BrowserRecordingOperation, BrowserRecordingReview,
    BrowserRecordingStatus, BrowserResourceHandle, BrowserResourceId, BrowserResourceKind,
    BrowserResourceLimits, BrowserResourceStore, BrowserResponse, BrowserRuntimeTarget,
    BrowserScreenshotMode, BrowserSnapshotSummary, BrowserStorageLayout, BrowserUploadResult,
    BrowserWaitResult, BrowserWorkflowCoordinator, BrowserWorkflowReviewMutation,
    BrowserWorkflowReviewProjection, BrowserWorkspaceKey, BrowserWorkspaceSnapshot,
    MAX_BROWSER_ACTIONS, MAX_BROWSER_RECIPE_WAIT_MS,
};
use base64::Engine as _;
use rfd::{MessageButtons, MessageDialog, MessageDialogResult, MessageLevel};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Instant;
use webview2_com::Microsoft::Web::WebView2::Win32::{
    COREWEBVIEW2_PERMISSION_KIND, COREWEBVIEW2_PERMISSION_KIND_CAMERA,
    COREWEBVIEW2_PERMISSION_KIND_CLIPBOARD_READ, COREWEBVIEW2_PERMISSION_KIND_FILE_READ_WRITE,
    COREWEBVIEW2_PERMISSION_KIND_GEOLOCATION, COREWEBVIEW2_PERMISSION_KIND_MICROPHONE,
    COREWEBVIEW2_PERMISSION_KIND_NOTIFICATIONS, COREWEBVIEW2_PERMISSION_STATE_ALLOW,
    COREWEBVIEW2_PERMISSION_STATE_DENY,
};
use webview2_com::{
    take_pwstr, CallDevToolsProtocolMethodCompletedHandler, PermissionRequestedEventHandler,
};
use windows::core::{BOOL, HSTRING, PWSTR};
use wry::dpi::{LogicalPosition, LogicalSize};
use wry::{
    MemoryUsageLevel, NewWindowResponse, PageLoadEvent, Rect, WebContext, WebView, WebViewBuilder,
    WebViewExtWindows,
};
use zeroize::{Zeroize, Zeroizing};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct BrowserViewKey {
    workspace_key: BrowserWorkspaceKey,
    tab_id: String,
}

const WORKSPACE_OPERATION_TAB: &str = "__workspace__";
const INLINE_RESULT_LIMIT: usize = 8 * 1024;
const MAX_BROWSER_PAGE_RECORDING_QUEUE: usize = 256;

enum BrowserAsyncPhase {
    Approval {
        risk: crate::browser::BrowserRisk,
        resume: BrowserApprovalResume,
    },
    Snapshot,
    Screenshot,
    Wait,
    InspectActions {
        actions: Vec<BrowserAction>,
    },
    InspectSecretType,
    Act {
        mutating: bool,
    },
    SecretType,
    Console,
    Network,
    Performance,
    UploadMark {
        paths: Vec<PathBuf>,
        token: String,
    },
    UploadRuntime {
        paths: Vec<PathBuf>,
        token: String,
    },
    UploadDescribe {
        paths: Vec<PathBuf>,
        token: String,
    },
    UploadSet {
        paths: Vec<PathBuf>,
        token: String,
    },
    Cdp,
}

enum BrowserApprovalResume {
    Command,
    Annotation,
    Recording { instance_id: u64 },
    Actions(Vec<BrowserAction>),
    SecretType,
}

struct ActiveBrowserRequest {
    request: BrowserCommandRequest,
    phase: BrowserAsyncPhase,
    approved_risk: Option<crate::browser::BrowserRisk>,
    _started_at: Instant,
}

struct BrowserAsyncCompletion {
    target: BrowserOperationTarget,
    operation_id: String,
    result: Result<String, String>,
}

struct PendingAnnotationCapture {
    capture_id: String,
    candidate: BrowserAnnotationCandidate,
}

struct BrowserAnnotationCompletion {
    route: BrowserAnnotationRoute,
    capture_id: String,
    result: Result<String, String>,
}

enum BrowserStartResult {
    Pending(BrowserAsyncPhase),
    Complete(Result<BrowserResponse, BrowserError>),
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BrowserScriptEnvelope {
    ok: bool,
    value: Option<Value>,
    error: Option<String>,
}

struct BrowserProjectRuntime {
    context: WebContext,
}

pub struct BrowserWebViewHost {
    status: BrowserHostStatus,
    trusted_app_config_dir: Option<PathBuf>,
    state: BrowserHostState,
    projects: HashMap<String, BrowserProjectRuntime>,
    views: HashMap<BrowserViewKey, WebView>,
    bounds: BrowserBounds,
    event_sender: Sender<BrowserHostEvent>,
    event_receiver: Receiver<BrowserHostEvent>,
    recording_transport: BrowserPageRecordingTransport,
    recording_ingresses: HashMap<BrowserViewKey, BrowserPageRecordingIngress>,
    workflow_coordinator: BrowserWorkflowCoordinator,
    recording_views: HashMap<BrowserViewKey, BrowserPageRecordingIpc>,
    operation_queue: BrowserOperationQueue<BrowserCommandRequest>,
    active_requests: HashMap<BrowserOperationTarget, ActiveBrowserRequest>,
    async_sender: Sender<BrowserAsyncCompletion>,
    async_receiver: Receiver<BrowserAsyncCompletion>,
    annotation_lifecycle: BrowserAnnotationLifecycle,
    annotation_cleanup: BrowserAnnotationCleanupLedger,
    accepted_annotation_candidates: HashMap<BrowserAnnotationRoute, BrowserAnnotationCandidate>,
    annotation_captures: HashMap<BrowserAnnotationRoute, PendingAnnotationCapture>,
    annotation_sender: Sender<BrowserAnnotationCompletion>,
    annotation_receiver: Receiver<BrowserAnnotationCompletion>,
    _main_thread_only: PhantomData<Rc<()>>,
}

impl BrowserWebViewHost {
    pub fn new(app_config_dir: impl AsRef<Path>) -> Self {
        let app_config_dir = absolute_path(app_config_dir.as_ref());
        let mut status = match wry::webview_version() {
            Ok(version) => BrowserHostStatus {
                available: true,
                platform: std::env::consts::OS.to_string(),
                version: Some(version),
                diagnostic: None,
            },
            Err(error) => BrowserHostStatus {
                available: false,
                platform: std::env::consts::OS.to_string(),
                version: None,
                diagnostic: Some(format!("WebView2 runtime is unavailable: {error}")),
            },
        };
        let trusted_app_config_dir = if status.available {
            match verified_app_config_root(&app_config_dir) {
                Ok(trusted_app_config_dir) => Some(trusted_app_config_dir),
                Err(error) => {
                    status.available = false;
                    status.diagnostic = Some(format!(
                        "Browser storage is unavailable; browser tools are disabled: {error}"
                    ));
                    None
                }
            }
        } else {
            None
        };
        Self::with_status(app_config_dir, trusted_app_config_dir, status)
    }

    pub fn unavailable(diagnostic: impl Into<String>) -> Self {
        Self::with_status(
            PathBuf::new(),
            None,
            BrowserHostStatus {
                available: false,
                platform: std::env::consts::OS.to_string(),
                version: None,
                diagnostic: Some(diagnostic.into()),
            },
        )
    }

    fn with_status(
        app_config_dir: PathBuf,
        trusted_app_config_dir: Option<PathBuf>,
        status: BrowserHostStatus,
    ) -> Self {
        let (event_sender, event_receiver) = mpsc::channel();
        let (async_sender, async_receiver) = mpsc::channel();
        let (annotation_sender, annotation_receiver) = mpsc::channel();
        let state_app_config_dir = trusted_app_config_dir
            .as_ref()
            .unwrap_or(&app_config_dir)
            .clone();
        Self {
            status,
            state: BrowserHostState::new(state_app_config_dir),
            trusted_app_config_dir,
            projects: HashMap::new(),
            views: HashMap::new(),
            bounds: BrowserBounds {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            },
            event_sender,
            event_receiver,
            recording_transport: BrowserPageRecordingTransport::with_capacity(
                MAX_BROWSER_PAGE_RECORDING_QUEUE,
            ),
            recording_ingresses: HashMap::new(),
            workflow_coordinator: BrowserWorkflowCoordinator::default(),
            recording_views: HashMap::new(),
            operation_queue: BrowserOperationQueue::default(),
            active_requests: HashMap::new(),
            async_sender,
            async_receiver,
            annotation_lifecycle: BrowserAnnotationLifecycle::default(),
            annotation_cleanup: BrowserAnnotationCleanupLedger::default(),
            accepted_annotation_candidates: HashMap::new(),
            annotation_captures: HashMap::new(),
            annotation_sender,
            annotation_receiver,
            _main_thread_only: PhantomData,
        }
    }

    pub fn status(&self) -> BrowserHostStatus {
        self.status.clone()
    }

    pub fn trusted_app_config_dir(&self) -> Option<&Path> {
        self.trusted_app_config_dir.as_deref()
    }

    pub fn page_recording_status(
        &self,
        workspace_key: &BrowserWorkspaceKey,
    ) -> BrowserRecordingStatus {
        self.workflow_coordinator.status(workspace_key)
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
        self.fence_workspace_recording_views(workspace_key);
        self.pump_page_recording_ipc();
        self.remove_workspace_recording_views(workspace_key);
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
        workspace_key: &BrowserWorkspaceKey,
    ) -> Result<BrowserRecordingInstance, BrowserPageRecordingIpcError> {
        if !self.status.available || self.state.workspace(workspace_key).is_none() {
            return Err(BrowserPageRecordingIpcError::Unavailable);
        }
        let tab_ids = self
            .state
            .workspace(workspace_key)
            .expect("workspace existence checked")
            .tabs
            .iter()
            .map(|tab| tab.id.clone())
            .collect::<Vec<_>>();
        let selected_tab_id = self
            .state
            .workspace(workspace_key)
            .expect("workspace existence checked")
            .selected_tab_id
            .clone();
        self.pump_page_recording_ipc();
        let instance = match selected_tab_id {
            Some(selected_tab_id) => self
                .workflow_coordinator
                .start_with_selected_tab(workspace_key.clone(), selected_tab_id),
            None => self.workflow_coordinator.start(workspace_key.clone()),
        }
        .map_err(map_page_recording_error)?;
        for tab_id in tab_ids {
            if self.views.contains_key(&view_key(workspace_key, &tab_id))
                && self
                    .install_page_recording_view(workspace_key, &tab_id)
                    .is_err()
            {
                self.remove_workspace_recording_views(workspace_key);
                if self.workflow_coordinator.stop(&instance).is_ok() {
                    let _ = self.workflow_coordinator.discard(&instance);
                }
                return Err(BrowserPageRecordingIpcError::HostFailure);
            }
        }
        Ok(instance)
    }

    pub fn stop_page_recording(
        &mut self,
        instance: &BrowserRecordingInstance,
    ) -> Result<BrowserRecordingReview, BrowserPageRecordingIpcError> {
        if self
            .workflow_coordinator
            .active_instance(instance.workspace_key())
            .is_none_or(|active| active.id() != instance.id())
        {
            return Err(BrowserPageRecordingIpcError::Untrusted);
        }
        self.fence_workspace_recording_views(instance.workspace_key());
        self.pump_page_recording_ipc();
        if self
            .workflow_coordinator
            .active_instance(instance.workspace_key())
            .is_none_or(|active| active.id() != instance.id())
        {
            return Err(BrowserPageRecordingIpcError::TransportInvalidated);
        }
        self.remove_workspace_recording_views(instance.workspace_key());
        self.workflow_coordinator
            .stop(instance)
            .map_err(map_page_recording_error)
    }

    pub fn handle_command(
        &mut self,
        window: &gpui::Window,
        workspace_key: &BrowserWorkspaceKey,
        command: BrowserCommand,
    ) -> Result<BrowserResponse, BrowserError> {
        validate_direct_secret_command(&command)?;
        self.pump_page_recording_ipc();
        self.handle_command_with_user_capture(window, workspace_key, command, true)
    }

    fn handle_command_with_user_capture(
        &mut self,
        window: &gpui::Window,
        workspace_key: &BrowserWorkspaceKey,
        command: BrowserCommand,
        capture_user_chrome: bool,
    ) -> Result<BrowserResponse, BrowserError> {
        let annotation_command = matches!(&command, BrowserCommand::Annotations { .. });
        let diagnostic_tab = command
            .tab_id()
            .map(ToOwned::to_owned)
            .or_else(|| self.selected_tab_id(workspace_key));
        let user_chrome_capture = if capture_user_chrome {
            match self
                .workflow_coordinator
                .begin_user_chrome_capture(workspace_key, &command)
            {
                Ok(capture) => capture,
                Err(error) => {
                    self.emit_diagnostic(
                        workspace_key,
                        diagnostic_tab.as_deref().unwrap_or(WORKSPACE_OPERATION_TAB),
                        format!("browser recording invalidated before chrome action: {error}"),
                    );
                    None
                }
            }
        } else {
            None
        };
        if let Some(control) = browser_lifecycle_control(workspace_key, &command) {
            self.handle_control(control);
        }
        let mut result = self.handle_command_inner(window, workspace_key, command);
        if annotation_command {
            if let Ok(response) = result.as_mut() {
                if let Err(error) =
                    self.finalize_annotation_command_resources(workspace_key, response)
                {
                    if let Some(tab_id) = diagnostic_tab
                        .clone()
                        .or_else(|| self.selected_tab_id(workspace_key))
                    {
                        self.emit_diagnostic(
                            workspace_key,
                            &tab_id,
                            format!("annotation resource pin reconciliation will retry: {error}"),
                        );
                    }
                }
            }
        }
        if let Some(capture) = user_chrome_capture {
            if let Err(error) = self
                .workflow_coordinator
                .complete_user_chrome_capture(capture, &result)
            {
                let tab_id = diagnostic_tab
                    .clone()
                    .or_else(|| self.selected_tab_id(workspace_key));
                self.emit_diagnostic(
                    workspace_key,
                    tab_id.as_deref().unwrap_or(WORKSPACE_OPERATION_TAB),
                    format!("browser recording invalidated after chrome action: {error}"),
                );
            }
        }
        if let Err(error) = &result {
            if let Some(tab_id) = diagnostic_tab.or_else(|| self.selected_tab_id(workspace_key)) {
                self.emit_diagnostic(workspace_key, &tab_id, error.to_string());
            }
        }
        result
    }

    pub fn handle_control(&mut self, control: BrowserHostControl) {
        match control {
            BrowserHostControl::InterruptProject { project_id } => {
                self.cancel_project_annotations(&project_id);
                self.cancel_project_operations(&project_id);
                self.discard_project_page_recordings(&project_id);
            }
            BrowserHostControl::InterruptWorkspace { workspace_key } => {
                self.cancel_workspace_annotations(&workspace_key);
                self.cancel_workspace_operations(&workspace_key);
                self.discard_workflow_state(&workspace_key);
            }
            BrowserHostControl::InterruptTab {
                workspace_key,
                tab_id,
            } => {
                if let Ok(route) = BrowserAnnotationRoute::new(workspace_key.clone(), &tab_id) {
                    self.cancel_annotation_route(&route);
                }
                self.cancel_tab_operations(&workspace_key, &tab_id);
            }
        }
    }

    pub fn handle_request(&mut self, window: &gpui::Window, request: BrowserCommandRequest) {
        if let Err(error) = request.validate_secret_sidecar() {
            request.respond(Err(error));
            return;
        }
        if !request.cancellation_is_current() {
            request.respond(Err(BrowserError::Interrupted));
            return;
        }
        self.pump_page_recording_ipc();
        let workspace_key = request.workspace_key().clone();
        let command = request.command().clone();
        if request.context().actor == BrowserInvocationActor::Agent {
            if let Err(error) = self.workflow_coordinator.reserve_agent_command(
                &workspace_key,
                &request.context().operation_id,
                &command,
                request.context().declared_risk,
            ) {
                self.respond_request(request, Err(map_agent_recording_error(error)));
                return;
            }
        }
        if request.context().actor != BrowserInvocationActor::Agent
            || browser_request_preempts_operation_queue(&command)
        {
            let capture_user_chrome = request.context().actor == BrowserInvocationActor::User;
            let result = self.handle_command_with_user_capture(
                window,
                &workspace_key,
                command,
                capture_user_chrome,
            );
            self.respond_request(request, result);
            return;
        }
        let target = self.operation_target(&workspace_key, &command);
        let operation_id = request.context().operation_id.clone();
        if let Some(request) = self
            .operation_queue
            .enqueue(target.clone(), operation_id, request)
        {
            self.start_queued_request(window, target, request);
        }
    }

    pub fn pump_async_completions(&mut self, window: &gpui::Window) {
        let completions: Vec<_> = self.async_receiver.try_iter().collect();
        for completion in completions {
            self.complete_async_operation(window, completion);
        }
        let annotation_completions: Vec<_> = self.annotation_receiver.try_iter().collect();
        for completion in annotation_completions {
            self.complete_annotation_capture(completion);
        }
    }

    fn operation_target(
        &self,
        workspace_key: &BrowserWorkspaceKey,
        command: &BrowserCommand,
    ) -> BrowserOperationTarget {
        let selected_tab_id = self.selected_tab_id(workspace_key);
        let tab_id = browser_operation_target_tab_id(command, selected_tab_id.as_deref());
        BrowserOperationTarget::new(workspace_key.clone(), tab_id)
            .expect("host operation target always has a nonblank tab id")
    }

    fn start_queued_request(
        &mut self,
        window: &gpui::Window,
        target: BrowserOperationTarget,
        request: BrowserCommandRequest,
    ) {
        let operation_id = request.context().operation_id.clone();
        if !request.cancellation_is_current() {
            self.finish_queued_request(
                window,
                target,
                operation_id,
                request,
                Err(BrowserError::Interrupted),
            );
            return;
        }
        if let Err(error) = request.validate_secret_sidecar() {
            self.finish_queued_request(window, target, operation_id, request, Err(error));
            return;
        }
        if browser_command_is_automation(request.command()) {
            match self.begin_automation_request(window, &target, &request, None) {
                BrowserStartResult::Pending(phase) => {
                    self.active_requests.insert(
                        target,
                        ActiveBrowserRequest {
                            request,
                            phase,
                            approved_risk: None,
                            _started_at: Instant::now(),
                        },
                    );
                }
                BrowserStartResult::Complete(result) => {
                    self.finish_queued_request(window, target, operation_id, request, result);
                }
            }
            return;
        }
        if matches!(request.command(), BrowserCommand::Annotations { .. }) {
            match self.begin_annotation_request(window, &target, &request, None) {
                BrowserStartResult::Pending(phase) => {
                    self.active_requests.insert(
                        target,
                        ActiveBrowserRequest {
                            request,
                            phase,
                            approved_risk: None,
                            _started_at: Instant::now(),
                        },
                    );
                }
                BrowserStartResult::Complete(result) => {
                    self.finish_queued_request(window, target, operation_id, request, result);
                }
            }
            return;
        }
        if matches!(
            request.command(),
            BrowserCommand::Recording {
                operation: BrowserRecordingOperation::Discard | BrowserRecordingOperation::Save,
            }
        ) {
            match self.begin_recording_request(window, &target, &request, None, None) {
                BrowserStartResult::Pending(phase) => {
                    self.active_requests.insert(
                        target,
                        ActiveBrowserRequest {
                            request,
                            phase,
                            approved_risk: None,
                            _started_at: Instant::now(),
                        },
                    );
                }
                BrowserStartResult::Complete(result) => {
                    self.finish_queued_request(window, target, operation_id, request, result);
                }
            }
            return;
        }
        let workspace_key = request.workspace_key().clone();
        let command = request.command().clone();
        let result = self.handle_command_with_user_capture(window, &workspace_key, command, false);
        self.finish_queued_request(window, target, operation_id, request, result);
    }

    fn finish_queued_request(
        &mut self,
        window: &gpui::Window,
        target: BrowserOperationTarget,
        operation_id: String,
        request: BrowserCommandRequest,
        result: Result<BrowserResponse, BrowserError>,
    ) {
        self.respond_request(request, result);
        if let Some(next) = self.operation_queue.complete(&target, &operation_id) {
            self.start_queued_request(window, target, next);
        }
    }

    fn respond_request(
        &mut self,
        request: BrowserCommandRequest,
        mut result: Result<BrowserResponse, BrowserError>,
    ) {
        let workspace_key = request.workspace_key().clone();
        let annotation_command = matches!(request.command(), BrowserCommand::Annotations { .. });
        let agent_journaled = request.context().actor == BrowserInvocationActor::Agent
            && browser_command_is_journaled(request.command());
        if request.context().actor == BrowserInvocationActor::Agent {
            if let Err(error) = self.workflow_coordinator.complete_agent_command(
                &workspace_key,
                &request.context().operation_id,
                request.command(),
                &result,
            ) {
                let tab_id = request
                    .command()
                    .tab_id()
                    .map(ToOwned::to_owned)
                    .or_else(|| self.selected_tab_id(&workspace_key))
                    .unwrap_or_else(|| WORKSPACE_OPERATION_TAB.to_string());
                self.emit_diagnostic(
                    &workspace_key,
                    &tab_id,
                    format!("browser workflow capture could not finalize: {error}"),
                );
            }
        }
        if matches!(&result, Ok(BrowserResponse::Workspace { .. })) {
            if let Some(tab_id) = request
                .command()
                .tab_id()
                .map(ToOwned::to_owned)
                .or_else(|| self.selected_tab_id(request.workspace_key()))
            {
                let _ = self
                    .event_sender
                    .send(BrowserHostEvent::AutomationStateChanged {
                        workspace_key: request.workspace_key().clone(),
                        tab_id,
                    });
            }
        }
        if agent_journaled {
            let tab_id = request
                .command()
                .tab_id()
                .map(ToOwned::to_owned)
                .or_else(|| self.selected_tab_id(&workspace_key));
            let url = tab_id
                .as_deref()
                .and_then(|tab_id| {
                    self.state
                        .workspace(&workspace_key)
                        .and_then(|snapshot| snapshot.tabs.iter().find(|tab| tab.id == tab_id))
                })
                .map(|tab| tab.url.clone())
                .unwrap_or_else(|| "about:blank".to_string());
            let result_code = match &result {
                Ok(_) => "ok",
                Err(error) => browser_error_code(error),
            };
            let entry = BrowserJournalEntry {
                id: request.context().operation_id.clone(),
                actor: BrowserJournalActor::Agent,
                intent: request.context().intent.clone(),
                url,
                started_at: request.started_at().to_string(),
                duration_ms: request.elapsed_ms(),
                result: result_code.to_string(),
                resource_ids: result
                    .as_ref()
                    .ok()
                    .map(browser_response_resource_ids)
                    .unwrap_or_default(),
            };
            let journal_mutation = self.state.append_journal_entry(&workspace_key, entry);
            if let Ok(mutation) = journal_mutation {
                if annotation_command {
                    if let Ok(response) = result.as_mut() {
                        replace_annotation_response_mutation(response, mutation);
                    }
                }
                if let Some(tab_id) = tab_id {
                    let _ = self
                        .event_sender
                        .send(BrowserHostEvent::AutomationStateChanged {
                            workspace_key: workspace_key.clone(),
                            tab_id,
                        });
                }
            }
        }
        if annotation_command {
            let finalized = match result.as_mut() {
                Ok(response) => {
                    self.finalize_annotation_command_resources(&workspace_key, response)
                }
                Err(_) => self.reconcile_annotation_pins(&workspace_key),
            };
            if let Err(error) = finalized {
                if let Some(tab_id) = self.selected_tab_id(&workspace_key) {
                    self.emit_diagnostic(
                        &workspace_key,
                        &tab_id,
                        format!("annotation resource pin reconciliation will retry: {error}"),
                    );
                }
            }
        } else if agent_journaled {
            if let Err(error) = self.reconcile_annotation_pins(&workspace_key) {
                if let Some(tab_id) = self.selected_tab_id(&workspace_key) {
                    self.emit_diagnostic(
                        &workspace_key,
                        &tab_id,
                        format!("annotation resource pin reconciliation will retry: {error}"),
                    );
                }
            }
        }
        request.respond(result);
    }

    fn cancel_tab_operations(&mut self, workspace_key: &BrowserWorkspaceKey, tab_id: &str) {
        let Ok(target) = BrowserOperationTarget::new(workspace_key.clone(), tab_id) else {
            return;
        };
        let cancellation = self.operation_queue.cancel_tab(&target);
        if let Some(active) = self.active_requests.remove(&target) {
            self.respond_request(active.request, Err(BrowserError::Interrupted));
        }
        for queued in cancellation.queued {
            self.respond_request(queued, Err(BrowserError::Interrupted));
        }
    }

    fn cancel_workspace_operations(&mut self, workspace_key: &BrowserWorkspaceKey) {
        for (target, cancellation) in self.operation_queue.cancel_workspace(workspace_key) {
            if let Some(active) = self.active_requests.remove(&target) {
                self.respond_request(active.request, Err(BrowserError::Interrupted));
            }
            for queued in cancellation.queued {
                self.respond_request(queued, Err(BrowserError::Interrupted));
            }
        }
    }

    fn cancel_project_operations(&mut self, project_id: &str) {
        for (target, cancellation) in self.operation_queue.cancel_project(project_id) {
            if let Some(active) = self.active_requests.remove(&target) {
                self.respond_request(active.request, Err(BrowserError::Interrupted));
            }
            for queued in cancellation.queued {
                self.respond_request(queued, Err(BrowserError::Interrupted));
            }
        }
    }

    fn cancel_annotation_route(&mut self, route: &BrowserAnnotationRoute) {
        self.cancel_annotation_mode(route);
        let drafts = self.annotation_lifecycle.cancel_route(route);
        let canceled_draft = !drafts.is_empty();
        for draft in drafts {
            self.annotation_cleanup
                .enqueue(route.clone(), draft.screenshot_resource);
        }
        self.retry_annotation_cleanups(&route.workspace_key);
        if canceled_draft {
            self.emit_annotation_canceled(route);
        }
    }

    fn cancel_annotation_mode(&mut self, route: &BrowserAnnotationRoute) {
        let canceled_candidate = self.accepted_annotation_candidates.remove(route).is_some();
        let canceled_capture = self.annotation_captures.remove(route).is_some();
        let was_active = self.annotation_lifecycle.is_active(route);
        if was_active {
            if let Ok(view) = self.view(&route.workspace_key, &route.tab_id) {
                let _ = view.evaluate_script("window.__devmanagerBrowser?.annotation?.cancel()");
            }
        }
        let deactivated = self.annotation_lifecycle.deactivate(route);
        if deactivated || canceled_candidate || canceled_capture {
            let _ = self
                .event_sender
                .send(BrowserHostEvent::AnnotationModeChanged {
                    workspace_key: route.workspace_key.clone(),
                    tab_id: route.tab_id.clone(),
                    enabled: false,
                });
        }
    }

    fn emit_annotation_canceled(&self, route: &BrowserAnnotationRoute) {
        let _ = self
            .event_sender
            .send(BrowserHostEvent::AnnotationCanceled {
                workspace_key: route.workspace_key.clone(),
                tab_id: route.tab_id.clone(),
            });
    }

    fn cancel_workspace_annotations(&mut self, workspace_key: &BrowserWorkspaceKey) {
        let routes: Vec<_> = self
            .views
            .keys()
            .filter(|key| &key.workspace_key == workspace_key)
            .filter_map(|key| {
                BrowserAnnotationRoute::new(key.workspace_key.clone(), key.tab_id.clone()).ok()
            })
            .collect();
        for route in routes {
            self.cancel_annotation_route(&route);
        }
        self.annotation_captures
            .retain(|route, _| &route.workspace_key != workspace_key);
        self.accepted_annotation_candidates
            .retain(|route, _| &route.workspace_key != workspace_key);
        for (route, draft) in self.annotation_lifecycle.cancel_workspace(workspace_key) {
            self.annotation_cleanup
                .enqueue(route.clone(), draft.screenshot_resource);
            self.emit_annotation_canceled(&route);
        }
        self.retry_annotation_cleanups(workspace_key);
    }

    fn cancel_project_annotations(&mut self, project_id: &str) {
        let routes: Vec<_> = self
            .views
            .keys()
            .filter(|key| key.workspace_key.project_id == project_id)
            .filter_map(|key| {
                BrowserAnnotationRoute::new(key.workspace_key.clone(), key.tab_id.clone()).ok()
            })
            .collect();
        for route in routes {
            self.cancel_annotation_route(&route);
        }
        self.annotation_captures
            .retain(|route, _| route.workspace_key.project_id != project_id);
        self.accepted_annotation_candidates
            .retain(|route, _| route.workspace_key.project_id != project_id);
        for (route, draft) in self.annotation_lifecycle.cancel_project(project_id) {
            self.annotation_cleanup
                .enqueue(route.clone(), draft.screenshot_resource);
            self.emit_annotation_canceled(&route);
            self.retry_annotation_cleanups(&route.workspace_key);
        }
        let retry_workspaces: Vec<_> = self
            .annotation_cleanup
            .pending_for_project(project_id)
            .into_iter()
            .map(|cleanup| cleanup.route.workspace_key)
            .fold(Vec::new(), |mut workspaces, workspace_key| {
                if !workspaces.contains(&workspace_key) {
                    workspaces.push(workspace_key);
                }
                workspaces
            });
        for workspace_key in retry_workspaces {
            self.retry_annotation_cleanups(&workspace_key);
        }
    }

    fn queue_annotation_cleanup(
        &mut self,
        route: &BrowserAnnotationRoute,
        resource_id: &BrowserResourceId,
    ) {
        self.annotation_cleanup
            .enqueue(route.clone(), resource_id.clone());
        self.retry_annotation_cleanups(&route.workspace_key);
    }

    fn retry_annotation_cleanups(&mut self, workspace_key: &BrowserWorkspaceKey) {
        let mut ledger = std::mem::take(&mut self.annotation_cleanup);
        let failures = ledger.retry_workspace(workspace_key, |cleanup| {
            self.set_resource_pinned(&cleanup.route.workspace_key, &cleanup.resource_id, false)
                .map(|_| ())
        });
        self.annotation_cleanup = ledger;
        for (cleanup, error) in failures {
            self.emit_diagnostic(
                &cleanup.route.workspace_key,
                &cleanup.route.tab_id,
                format!("annotation screenshot cleanup will retry: {error}"),
            );
        }
    }

    fn begin_automation_request(
        &mut self,
        window: &gpui::Window,
        target: &BrowserOperationTarget,
        request: &BrowserCommandRequest,
        approved_risk: Option<crate::browser::BrowserRisk>,
    ) -> BrowserStartResult {
        let workspace_key = request.workspace_key();
        let command = request.command();
        let tab_id = command
            .tab_id()
            .expect("automation commands always identify a logical tab");
        if let Err(error) = self.ensure_existing_tab_view(window, workspace_key, tab_id) {
            return BrowserStartResult::Complete(Err(error));
        }
        let operation_id = request.context().operation_id.clone();
        let command_risk = match command {
            BrowserCommand::Downloads {
                operation: crate::browser::BrowserDownloadOperation::Delete,
                ..
            } => crate::browser::BrowserRisk::Destructive,
            BrowserCommand::Cdp { method, .. } => browser_cdp_method_risk(method),
            _ => crate::browser::BrowserRisk::Normal,
        };
        let initial_risk =
            effective_browser_risk(request.context().declared_risk, None, Some(command_risk));
        if !matches!(
            command,
            BrowserCommand::Act { .. } | BrowserCommand::SecretType { .. }
        ) && BrowserApprovalPolicy::trust_project().requires_confirmation(initial_risk)
            && approved_risk != Some(initial_risk)
        {
            return self.await_approval(
                target,
                request,
                initial_risk,
                browser_command_summary(command),
                BrowserApprovalResume::Command,
            );
        }
        match command {
            BrowserCommand::SecretType {
                target: action_target,
                ..
            } => {
                if let Err(error) =
                    self.validate_action_target_reference(workspace_key, action_target)
                {
                    return BrowserStartResult::Complete(Err(error));
                }
                let encoded = match serde_json::to_string(action_target) {
                    Ok(encoded) => encoded,
                    Err(error) => {
                        return BrowserStartResult::Complete(Err(BrowserError::CrashedView {
                            message: format!("could not encode browser secret target: {error}"),
                        }))
                    }
                };
                start_result(
                    self.start_script(
                        target,
                        &operation_id,
                        &format!("window.__devmanagerBrowser.inspectSecretTarget({encoded})"),
                    ),
                    BrowserAsyncPhase::InspectSecretType,
                )
            }
            BrowserCommand::Snapshot { .. } => start_result(
                self.start_script(
                    target,
                    &operation_id,
                    "window.__devmanagerBrowser.snapshot()",
                ),
                BrowserAsyncPhase::Snapshot,
            ),
            BrowserCommand::Screenshot { mode, .. } => {
                let params = match mode {
                    BrowserScreenshotMode::Viewport => {
                        json!({"format": "png", "fromSurface": true})
                    }
                    BrowserScreenshotMode::FullPage => json!({
                        "format": "png",
                        "fromSurface": true,
                        "captureBeyondViewport": true
                    }),
                };
                start_result(
                    self.start_cdp(target, &operation_id, "Page.captureScreenshot", &params),
                    BrowserAsyncPhase::Screenshot,
                )
            }
            BrowserCommand::Wait {
                condition,
                timeout_ms,
                ..
            } => {
                if let Err(error) = self.validate_wait_reference(workspace_key, condition) {
                    return BrowserStartResult::Complete(Err(error));
                }
                let timeout_ms = (*timeout_ms).clamp(1, MAX_BROWSER_RECIPE_WAIT_MS);
                let condition = match serde_json::to_string(condition) {
                    Ok(condition) => condition,
                    Err(error) => {
                        return BrowserStartResult::Complete(Err(BrowserError::CrashedView {
                            message: format!("could not encode browser wait condition: {error}"),
                        }))
                    }
                };
                start_result(
                    self.start_script(
                        target,
                        &operation_id,
                        &format!("window.__devmanagerBrowser.wait({condition}, {timeout_ms})"),
                    ),
                    BrowserAsyncPhase::Wait,
                )
            }
            BrowserCommand::Act { actions, .. } => {
                if actions.is_empty() || actions.len() > MAX_BROWSER_ACTIONS {
                    return BrowserStartResult::Complete(Err(BrowserError::InvalidInvocation {
                        field: "actions".to_string(),
                    }));
                }
                if let Err(error) = self.validate_action_references(workspace_key, actions) {
                    return BrowserStartResult::Complete(Err(error));
                }
                let encoded = match serde_json::to_string(actions) {
                    Ok(encoded) => encoded,
                    Err(error) => {
                        return BrowserStartResult::Complete(Err(BrowserError::CrashedView {
                            message: format!("could not encode browser actions: {error}"),
                        }))
                    }
                };
                start_result(
                    self.start_script(
                        target,
                        &operation_id,
                        &format!("window.__devmanagerBrowser.inspectTargets({encoded})"),
                    ),
                    BrowserAsyncPhase::InspectActions {
                        actions: actions.clone(),
                    },
                )
            }
            BrowserCommand::Console { operation, .. } => {
                let operation = match operation {
                    BrowserConsoleOperation::List => "list",
                    BrowserConsoleOperation::Clear => "clear",
                };
                start_result(
                    self.start_script(
                        target,
                        &operation_id,
                        &format!("window.__devmanagerBrowser.console({operation:?})"),
                    ),
                    BrowserAsyncPhase::Console,
                )
            }
            BrowserCommand::Network {
                operation,
                request_id,
                ..
            } => {
                let operation = match operation {
                    BrowserNetworkOperation::List => "list",
                    BrowserNetworkOperation::Clear => "clear",
                    BrowserNetworkOperation::Body => "body",
                };
                let request_id = serde_json::to_string(request_id.as_deref().unwrap_or_default())
                    .unwrap_or_else(|_| "\"\"".to_string());
                start_result(
                    self.start_script(
                        target,
                        &operation_id,
                        &format!("window.__devmanagerBrowser.network({operation:?}, {request_id})"),
                    ),
                    BrowserAsyncPhase::Network,
                )
            }
            BrowserCommand::Performance { operation, .. } => {
                let operation = match operation {
                    BrowserPerformanceOperation::Snapshot => "snapshot",
                    BrowserPerformanceOperation::TraceStart => "traceStart",
                    BrowserPerformanceOperation::TraceStop => "traceStop",
                };
                start_result(
                    self.start_script(
                        target,
                        &operation_id,
                        &format!("window.__devmanagerBrowser.performance({operation:?})"),
                    ),
                    BrowserAsyncPhase::Performance,
                )
            }
            BrowserCommand::Upload {
                target: action_target,
                paths,
                ..
            } => {
                let paths = match self.canonical_upload_paths(paths) {
                    Ok(paths) => paths,
                    Err(error) => return BrowserStartResult::Complete(Err(error)),
                };
                let target_json = match serde_json::to_string(action_target) {
                    Ok(target) => target,
                    Err(error) => {
                        return BrowserStartResult::Complete(Err(BrowserError::CrashedView {
                            message: format!("could not encode browser upload target: {error}"),
                        }))
                    }
                };
                let token = format!(
                    "upload-{}",
                    operation_id.replace(|c: char| !c.is_ascii_alphanumeric(), "")
                );
                let token_json =
                    serde_json::to_string(&token).expect("upload token is serializable");
                start_result(
                    self.start_script(
                        target,
                        &operation_id,
                        &format!(
                            "window.__devmanagerBrowser.markUpload({target_json}, {token_json})"
                        ),
                    ),
                    BrowserAsyncPhase::UploadMark { paths, token },
                )
            }
            BrowserCommand::Downloads { .. } => {
                BrowserStartResult::Complete(self.handle_download_command(request))
            }
            BrowserCommand::Cdp { method, params, .. } => {
                if method.trim().is_empty() || method.trim() != method || !params.is_object() {
                    return BrowserStartResult::Complete(Err(BrowserError::InvalidInvocation {
                        field: "cdp".to_string(),
                    }));
                }
                start_result(
                    self.start_cdp(target, &operation_id, method, params),
                    BrowserAsyncPhase::Cdp,
                )
            }
            _ => BrowserStartResult::Complete(Err(BrowserError::CrashedView {
                message: "unexpected browser automation command".to_string(),
            })),
        }
    }

    fn begin_annotation_request(
        &mut self,
        window: &gpui::Window,
        target: &BrowserOperationTarget,
        request: &BrowserCommandRequest,
        approved_risk: Option<crate::browser::BrowserRisk>,
    ) -> BrowserStartResult {
        let BrowserCommand::Annotations { operation, .. } = request.command() else {
            return BrowserStartResult::Complete(Err(BrowserError::CrashedView {
                message: "unexpected browser annotation command".to_string(),
            }));
        };
        let effective_risk =
            effective_browser_annotation_risk(request.context().declared_risk, *operation);
        if BrowserApprovalPolicy::trust_project().requires_confirmation(effective_risk)
            && approved_risk != Some(effective_risk)
        {
            return self.await_approval(
                target,
                request,
                effective_risk,
                browser_command_summary(request.command()),
                BrowserApprovalResume::Annotation,
            );
        }
        BrowserStartResult::Complete(self.handle_command(
            window,
            request.workspace_key(),
            request.command().clone(),
        ))
    }

    fn begin_recording_request(
        &mut self,
        _window: &gpui::Window,
        target: &BrowserOperationTarget,
        request: &BrowserCommandRequest,
        approved_risk: Option<crate::browser::BrowserRisk>,
        expected_instance_id: Option<u64>,
    ) -> BrowserStartResult {
        let BrowserCommand::Recording { operation } = request.command() else {
            return BrowserStartResult::Complete(Err(BrowserError::CrashedView {
                message: "unexpected browser recording command".to_string(),
            }));
        };
        if !matches!(
            operation,
            BrowserRecordingOperation::Discard | BrowserRecordingOperation::Save
        ) {
            return BrowserStartResult::Complete(Err(BrowserError::CrashedView {
                message: "unexpected browser recording observation on the mutation path"
                    .to_string(),
            }));
        }
        if let Err(error) = self.ensure_runtime_available() {
            return BrowserStartResult::Complete(Err(error));
        }
        let Some(instance) = self
            .workflow_coordinator
            .current_instance(request.workspace_key())
        else {
            return BrowserStartResult::Complete(Err(stale_recording_instance()));
        };
        if expected_instance_id.is_some_and(|expected| expected != instance.id()) {
            return BrowserStartResult::Complete(Err(stale_recording_instance()));
        }
        let instance_id = instance.id();
        let overwrites_existing = match operation {
            BrowserRecordingOperation::Save => {
                let Some(project_root) = request.local_project_root() else {
                    return BrowserStartResult::Complete(Err(BrowserError::InvalidInvocation {
                        field: "localProjectRoot".to_string(),
                    }));
                };
                match browser_recording_save_would_overwrite(
                    &self.workflow_coordinator,
                    request.workspace_key(),
                    instance_id,
                    project_root,
                ) {
                    Ok(overwrites_existing) => overwrites_existing,
                    Err(error) => return BrowserStartResult::Complete(Err(error)),
                }
            }
            BrowserRecordingOperation::Discard => false,
            _ => unreachable!("recording mutation operation checked above"),
        };
        let effective_risk = effective_browser_recording_risk(
            request.context().declared_risk,
            *operation,
            overwrites_existing,
        );
        if BrowserApprovalPolicy::trust_project().requires_confirmation(effective_risk)
            && approved_risk != Some(effective_risk)
        {
            return self.await_approval(
                target,
                request,
                effective_risk,
                browser_command_summary(request.command()),
                BrowserApprovalResume::Recording { instance_id },
            );
        }

        let result = match operation {
            BrowserRecordingOperation::Save => {
                let Some(project_root) = request.local_project_root() else {
                    return BrowserStartResult::Complete(Err(BrowserError::InvalidInvocation {
                        field: "localProjectRoot".to_string(),
                    }));
                };
                save_browser_recording_review(
                    &self.workflow_coordinator,
                    request.workspace_key(),
                    instance_id,
                    project_root,
                    overwrites_existing,
                )
            }
            BrowserRecordingOperation::Discard => {
                self.fence_workspace_recording_views(request.workspace_key());
                self.pump_page_recording_ipc();
                self.remove_workspace_recording_views(request.workspace_key());
                discard_browser_recording(
                    &self.workflow_coordinator,
                    request.workspace_key(),
                    instance_id,
                )
            }
            _ => unreachable!("recording mutation operation checked above"),
        };
        BrowserStartResult::Complete(result.map(|result| BrowserResponse::Recording { result }))
    }

    fn await_approval(
        &mut self,
        target: &BrowserOperationTarget,
        request: &BrowserCommandRequest,
        risk: crate::browser::BrowserRisk,
        action_summary: String,
        resume: BrowserApprovalResume,
    ) -> BrowserStartResult {
        let origin_url = self
            .state
            .workspace(request.workspace_key())
            .and_then(|snapshot| {
                snapshot
                    .tabs
                    .iter()
                    .find(|tab| tab.id == target.tab_id)
                    .map(|tab| tab.url.clone())
            })
            .unwrap_or_else(|| "about:blank".to_string());
        let approval = BrowserApprovalRequest {
            operation_id: request.context().operation_id.clone(),
            actor: request.context().actor,
            intent: redact_browser_text(&request.context().intent),
            effective_risk: risk,
            action_summary: redact_browser_text(&action_summary),
            origin_url: redact_browser_text(&origin_url),
        };
        if let Ok(view) = self.view(request.workspace_key(), &target.tab_id) {
            let _ = view.set_visible(false);
        }
        let _ = self.event_sender.send(BrowserHostEvent::ApprovalRequested {
            workspace_key: request.workspace_key().clone(),
            tab_id: target.tab_id.clone(),
            request: approval,
        });
        BrowserStartResult::Pending(BrowserAsyncPhase::Approval { risk, resume })
    }

    fn start_script(
        &self,
        target: &BrowserOperationTarget,
        operation_id: &str,
        expression: &str,
    ) -> Result<(), BrowserError> {
        let sender = self.async_sender.clone();
        let callback_target = target.clone();
        let callback_operation_id = operation_id.to_string();
        let script = format!(
            r#"(async () => {{
              try {{
                const value = await ({expression});
                return {{ ok: true, value }};
              }} catch (error) {{
                const known = ["element_not_found", "unsupported_action"];
                const candidate = String(error && error.message || "automation_failed");
                return {{ ok: false, error: known.includes(candidate) ? candidate : "automation_failed" }};
              }}
            }})()"#
        );
        self.view(&target.workspace_key, &target.tab_id)?
            .evaluate_script_with_callback(&script, move |result| {
                let _ = sender.send(BrowserAsyncCompletion {
                    target: callback_target.clone(),
                    operation_id: callback_operation_id.clone(),
                    result: Ok(result),
                });
            })
            .map_err(view_failure)
    }

    fn start_cdp(
        &self,
        target: &BrowserOperationTarget,
        operation_id: &str,
        method: &str,
        params: &Value,
    ) -> Result<(), BrowserError> {
        let webview = self.view(&target.workspace_key, &target.tab_id)?.webview();
        let method = HSTRING::from(method);
        let params = HSTRING::from(params.to_string());
        let sender = self.async_sender.clone();
        let callback_target = target.clone();
        let callback_operation_id = operation_id.to_string();
        let handler =
            CallDevToolsProtocolMethodCompletedHandler::create(Box::new(move |status, result| {
                let result = status.map(|()| result).map_err(|error| error.to_string());
                let _ = sender.send(BrowserAsyncCompletion {
                    target: callback_target.clone(),
                    operation_id: callback_operation_id.clone(),
                    result,
                });
                Ok(())
            }));
        unsafe {
            webview
                .CallDevToolsProtocolMethod(&method, &params, &handler)
                .map_err(view_failure)
        }
    }

    fn begin_annotation_capture(
        &mut self,
        window: &gpui::Window,
        workspace_key: &BrowserWorkspaceKey,
        tab_id: &str,
        candidate: BrowserAnnotationCandidate,
    ) -> Result<(), BrowserError> {
        let route = BrowserAnnotationRoute::new(workspace_key.clone(), tab_id)?;
        let accepted = self
            .accepted_annotation_candidates
            .remove(&route)
            .ok_or_else(|| BrowserError::BlockedPermission {
                permission: "annotation candidate was not produced by active overlay".to_string(),
            })?;
        if accepted != candidate {
            return Err(BrowserError::BlockedPermission {
                permission: "annotation candidate integrity".to_string(),
            });
        }
        let snapshot = self
            .state
            .workspace(workspace_key)
            .ok_or_else(missing_workspace)?;
        let tab = snapshot
            .tabs
            .iter()
            .find(|tab| tab.id == tab_id)
            .ok_or_else(|| missing_tab(tab_id))?;
        validate_annotation_candidate_context(&candidate, &tab.url, snapshot.revision)?;
        if self.annotation_captures.contains_key(&route) {
            return Err(BrowserError::InvalidAnnotation {
                field: "capture".to_string(),
                message: "is already pending for this tab".to_string(),
            });
        }
        self.ensure_existing_tab_view(window, workspace_key, tab_id)?;
        let capture_id = random_annotation_capture_id()?;
        self.annotation_captures.insert(
            route.clone(),
            PendingAnnotationCapture {
                capture_id: capture_id.clone(),
                candidate,
            },
        );
        let method = HSTRING::from("Page.captureScreenshot");
        let params = HSTRING::from(json!({"format": "png", "fromSurface": true}).to_string());
        let sender = self.annotation_sender.clone();
        let callback_route = route.clone();
        let callback_capture_id = capture_id.clone();
        let handler =
            CallDevToolsProtocolMethodCompletedHandler::create(Box::new(move |status, result| {
                let result = status.map(|()| result).map_err(|error| error.to_string());
                let _ = sender.send(BrowserAnnotationCompletion {
                    route: callback_route.clone(),
                    capture_id: callback_capture_id.clone(),
                    result,
                });
                Ok(())
            }));
        let started = unsafe {
            self.view(workspace_key, tab_id)?
                .webview()
                .CallDevToolsProtocolMethod(&method, &params, &handler)
                .map_err(view_failure)
        };
        if let Err(error) = started {
            self.annotation_captures.remove(&route);
            return Err(error);
        }
        Ok(())
    }

    fn complete_annotation_capture(&mut self, completion: BrowserAnnotationCompletion) {
        let Some(pending) = self.annotation_captures.get(&completion.route) else {
            return;
        };
        if pending.capture_id != completion.capture_id {
            return;
        }
        let pending = self
            .annotation_captures
            .remove(&completion.route)
            .expect("capture was checked above");
        let result = completion
            .result
            .map_err(|_| BrowserError::CrashedView {
                message: "WebView2 annotation screenshot callback failed".to_string(),
            })
            .and_then(|raw| decode_screenshot_png(&raw))
            .and_then(|png| {
                crop_annotation_png(&png, pending.candidate.bounds, &pending.candidate.viewport)
            })
            .and_then(|crop| {
                self.store_pinned_resource(
                    &completion.route.workspace_key,
                    BrowserResourceKind::AnnotationScreenshot,
                    "image/png",
                    crop,
                )
            })
            .and_then(|resource| {
                let draft = match BrowserAnnotationDraft::new(
                    completion.route.tab_id.clone(),
                    pending.candidate,
                    resource.id.clone(),
                ) {
                    Ok(draft) => draft,
                    Err(error) => {
                        self.queue_annotation_cleanup(&completion.route, &resource.id);
                        return Err(error);
                    }
                };
                if let Err(error) = self
                    .annotation_lifecycle
                    .store_draft(completion.route.clone(), draft.clone())
                {
                    self.queue_annotation_cleanup(&completion.route, &resource.id);
                    return Err(error);
                }
                Ok(draft)
            });
        match result {
            Ok(draft) => {
                let _ = self
                    .event_sender
                    .send(BrowserHostEvent::AnnotationDraftReady {
                        workspace_key: completion.route.workspace_key,
                        tab_id: completion.route.tab_id,
                        draft,
                    });
            }
            Err(error) => self.emit_diagnostic(
                &completion.route.workspace_key,
                &completion.route.tab_id,
                error.to_string(),
            ),
        }
    }

    fn complete_async_operation(
        &mut self,
        window: &gpui::Window,
        completion: BrowserAsyncCompletion,
    ) {
        if self.operation_queue.active_operation_id(&completion.target)
            != Some(completion.operation_id.as_str())
        {
            return;
        }
        let Some(mut active) = self.active_requests.remove(&completion.target) else {
            return;
        };
        let operation_id = completion.operation_id;
        if !active.request.cancellation_is_current() {
            self.finish_queued_request(
                window,
                completion.target,
                operation_id,
                active.request,
                Err(BrowserError::Interrupted),
            );
            return;
        }
        if let Err(error) = active.request.validate_secret_sidecar() {
            self.finish_queued_request(
                window,
                completion.target,
                operation_id,
                active.request,
                Err(error),
            );
            return;
        }
        let raw = match completion.result {
            Ok(raw) => raw,
            Err(_) => {
                self.finish_queued_request(
                    window,
                    completion.target,
                    operation_id,
                    active.request,
                    Err(BrowserError::CrashedView {
                        message: "WebView2 callback failed".to_string(),
                    }),
                );
                return;
            }
        };
        let phase = std::mem::replace(&mut active.phase, BrowserAsyncPhase::Cdp);
        let result = match phase {
            BrowserAsyncPhase::Snapshot => self.complete_snapshot(&active.request, &raw),
            BrowserAsyncPhase::Screenshot => self.complete_screenshot(&active.request, &raw),
            BrowserAsyncPhase::Wait => self.complete_wait(&active.request, &raw),
            BrowserAsyncPhase::InspectActions { actions } => {
                let value = match script_value(&raw) {
                    Ok(value) => value,
                    Err(error) => {
                        self.finish_queued_request(
                            window,
                            completion.target,
                            operation_id,
                            active.request,
                            Err(error),
                        );
                        return;
                    }
                };
                let runtime_targets: Vec<BrowserRuntimeTarget> = match serde_json::from_value(value)
                {
                    Ok(targets) => targets,
                    Err(_) => {
                        self.finish_queued_request(
                            window,
                            completion.target,
                            operation_id,
                            active.request,
                            Err(BrowserError::CrashedView {
                                message: "browser runtime target inspection returned invalid data"
                                    .to_string(),
                            }),
                        );
                        return;
                    }
                };
                let effective_risk = effective_browser_risk_for_targets(
                    active.request.context().declared_risk,
                    &runtime_targets,
                    None,
                );
                if let Err(error) = self.workflow_coordinator.inspect_agent_actions(
                    active.request.workspace_key(),
                    &operation_id,
                    active.request.command(),
                    &runtime_targets,
                    effective_risk,
                ) {
                    self.finish_queued_request(
                        window,
                        completion.target,
                        operation_id,
                        active.request,
                        Err(map_agent_recording_error(error)),
                    );
                    return;
                }
                if BrowserApprovalPolicy::trust_project().requires_confirmation(effective_risk)
                    && active.approved_risk != Some(effective_risk)
                {
                    let summary = actions
                        .iter()
                        .map(BrowserAction::redacted_summary)
                        .collect::<Vec<_>>()
                        .join(", ");
                    let BrowserStartResult::Pending(phase) = self.await_approval(
                        &completion.target,
                        &active.request,
                        effective_risk,
                        summary,
                        BrowserApprovalResume::Actions(actions),
                    ) else {
                        unreachable!("approval requests always remain pending")
                    };
                    active.phase = phase;
                    self.active_requests.insert(completion.target, active);
                    return;
                }
                self.continue_actions(window, completion.target, operation_id, active, actions);
                return;
            }
            BrowserAsyncPhase::InspectSecretType => {
                let value = match script_value(&raw) {
                    Ok(value) => value,
                    Err(error) => {
                        self.finish_queued_request(
                            window,
                            completion.target,
                            operation_id,
                            active.request,
                            Err(error),
                        );
                        return;
                    }
                };
                let runtime_target: BrowserRuntimeTarget =
                    match serde_json::from_value::<Option<BrowserRuntimeTarget>>(value) {
                        Ok(Some(target)) => target,
                        Ok(None) => {
                            self.finish_queued_request(
                                window,
                                completion.target,
                                operation_id,
                                active.request,
                                Err(BrowserError::CrashedView {
                                    message: "element_not_found".to_string(),
                                }),
                            );
                            return;
                        }
                        Err(_) => {
                            self.finish_queued_request(
                            window,
                            completion.target,
                            operation_id,
                            active.request,
                            Err(BrowserError::CrashedView {
                                message:
                                    "browser runtime secret target inspection returned invalid data"
                                        .to_string(),
                            }),
                        );
                            return;
                        }
                    };
                let effective_risk = effective_browser_risk_for_targets(
                    active.request.context().declared_risk,
                    std::slice::from_ref(&runtime_target),
                    None,
                );
                if let Err(error) = self.workflow_coordinator.inspect_agent_secret_type(
                    active.request.workspace_key(),
                    &operation_id,
                    active.request.command(),
                    &runtime_target,
                    effective_risk,
                ) {
                    self.finish_queued_request(
                        window,
                        completion.target,
                        operation_id,
                        active.request,
                        Err(map_agent_recording_error(error)),
                    );
                    return;
                }
                if BrowserApprovalPolicy::trust_project().requires_confirmation(effective_risk)
                    && active.approved_risk != Some(effective_risk)
                {
                    let BrowserStartResult::Pending(phase) = self.await_approval(
                        &completion.target,
                        &active.request,
                        effective_risk,
                        browser_command_summary(active.request.command()),
                        BrowserApprovalResume::SecretType,
                    ) else {
                        unreachable!("approval requests always remain pending")
                    };
                    active.phase = phase;
                    self.active_requests.insert(completion.target, active);
                    return;
                }
                self.continue_secret_type(window, completion.target, operation_id, active);
                return;
            }
            BrowserAsyncPhase::Approval { .. } => return,
            BrowserAsyncPhase::Act { mutating } => {
                self.complete_action(&active.request, &raw, mutating)
            }
            BrowserAsyncPhase::SecretType => self.complete_secret_type(&active.request, &raw),
            BrowserAsyncPhase::Console => self.complete_console(&active.request, &raw),
            BrowserAsyncPhase::Network => self.complete_network(&active.request, &raw),
            BrowserAsyncPhase::Performance => self.complete_performance(&active.request, &raw),
            BrowserAsyncPhase::UploadMark { paths, token } => {
                return self.continue_upload_after_mark(
                    window,
                    completion.target,
                    operation_id,
                    active,
                    raw,
                    paths,
                    token,
                );
            }
            BrowserAsyncPhase::UploadRuntime { paths, token } => {
                return self.continue_upload_after_runtime(
                    window,
                    completion.target,
                    operation_id,
                    active,
                    raw,
                    paths,
                    token,
                );
            }
            BrowserAsyncPhase::UploadDescribe { paths, token } => {
                return self.continue_upload_after_describe(
                    window,
                    completion.target,
                    operation_id,
                    active,
                    raw,
                    paths,
                    token,
                );
            }
            BrowserAsyncPhase::UploadSet {
                paths,
                token: _token,
            } => self.complete_upload(&active.request, &raw, paths),
            BrowserAsyncPhase::Cdp => self.complete_cdp(&active.request, &raw),
        };
        self.finish_queued_request(
            window,
            completion.target,
            operation_id,
            active.request,
            result,
        );
    }

    pub fn is_pending_approval(
        &mut self,
        workspace_key: &BrowserWorkspaceKey,
        tab_id: &str,
        operation_id: &str,
    ) -> bool {
        let Ok(target) = BrowserOperationTarget::new(workspace_key.clone(), tab_id) else {
            return false;
        };
        if self.operation_queue.active_operation_id(&target) != Some(operation_id) {
            return false;
        }
        let Some(active) = self.active_requests.get(&target) else {
            return false;
        };
        if !active.request.cancellation_is_current() {
            self.cancel_tab_operations(workspace_key, tab_id);
            return false;
        }
        matches!(&active.phase, BrowserAsyncPhase::Approval { .. })
    }

    pub fn resolve_approval(
        &mut self,
        window: &gpui::Window,
        workspace_key: &BrowserWorkspaceKey,
        tab_id: &str,
        operation_id: &str,
        approved: bool,
    ) -> Result<(), BrowserError> {
        let target = BrowserOperationTarget::new(workspace_key.clone(), tab_id)?;
        if self.operation_queue.active_operation_id(&target) != Some(operation_id) {
            return Err(BrowserError::Interrupted);
        }
        let Some(mut active) = self.active_requests.remove(&target) else {
            return Err(BrowserError::Interrupted);
        };
        if !active.request.cancellation_is_current() {
            self.finish_queued_request(
                window,
                target,
                operation_id.to_string(),
                active.request,
                Err(BrowserError::Interrupted),
            );
            return Err(BrowserError::Interrupted);
        }
        let phase = std::mem::replace(&mut active.phase, BrowserAsyncPhase::Cdp);
        let BrowserAsyncPhase::Approval { risk, resume } = phase else {
            self.active_requests.insert(target, active);
            return Err(BrowserError::InvalidInvocation {
                field: "approvalOperationId".to_string(),
            });
        };
        if !approved {
            self.finish_queued_request(
                window,
                target,
                operation_id.to_string(),
                active.request,
                Err(BrowserError::BlockedPermission {
                    permission: format!("{risk:?}"),
                }),
            );
            self.apply_visibility_plan()?;
            return Ok(());
        }

        if let Err(error) = active.request.validate_secret_sidecar() {
            let returned = error.clone();
            self.finish_queued_request(
                window,
                target,
                operation_id.to_string(),
                active.request,
                Err(error),
            );
            self.apply_visibility_plan()?;
            return Err(returned);
        }

        active.approved_risk = Some(risk);
        match resume {
            BrowserApprovalResume::Command => {
                match self.begin_automation_request(window, &target, &active.request, Some(risk)) {
                    BrowserStartResult::Pending(phase) => {
                        active.phase = phase;
                        self.active_requests.insert(target, active);
                    }
                    BrowserStartResult::Complete(result) => self.finish_queued_request(
                        window,
                        target,
                        operation_id.to_string(),
                        active.request,
                        result,
                    ),
                }
            }
            BrowserApprovalResume::Annotation => {
                match self.begin_annotation_request(window, &target, &active.request, Some(risk)) {
                    BrowserStartResult::Pending(phase) => {
                        active.phase = phase;
                        self.active_requests.insert(target, active);
                    }
                    BrowserStartResult::Complete(result) => self.finish_queued_request(
                        window,
                        target,
                        operation_id.to_string(),
                        active.request,
                        result,
                    ),
                }
            }
            BrowserApprovalResume::Recording { instance_id } => {
                match self.begin_recording_request(
                    window,
                    &target,
                    &active.request,
                    Some(risk),
                    Some(instance_id),
                ) {
                    BrowserStartResult::Pending(phase) => {
                        active.phase = phase;
                        self.active_requests.insert(target, active);
                    }
                    BrowserStartResult::Complete(result) => self.finish_queued_request(
                        window,
                        target,
                        operation_id.to_string(),
                        active.request,
                        result,
                    ),
                }
            }
            BrowserApprovalResume::Actions(actions) => {
                self.continue_actions(window, target, operation_id.to_string(), active, actions)
            }
            BrowserApprovalResume::SecretType => {
                match self.begin_automation_request(window, &target, &active.request, Some(risk)) {
                    BrowserStartResult::Pending(phase) => {
                        active.phase = phase;
                        self.active_requests.insert(target, active);
                    }
                    BrowserStartResult::Complete(result) => self.finish_queued_request(
                        window,
                        target,
                        operation_id.to_string(),
                        active.request,
                        result,
                    ),
                }
            }
        }
        self.apply_visibility_plan()?;
        Ok(())
    }

    fn continue_actions(
        &mut self,
        window: &gpui::Window,
        target: BrowserOperationTarget,
        operation_id: String,
        mut active: ActiveBrowserRequest,
        actions: Vec<BrowserAction>,
    ) {
        let mutating = actions.iter().any(BrowserAction::is_mutating);
        let encoded = match serde_json::to_string(&actions) {
            Ok(encoded) => encoded,
            Err(_) => {
                self.finish_queued_request(
                    window,
                    target,
                    operation_id,
                    active.request,
                    Err(BrowserError::CrashedView {
                        message: "could not encode inspected browser actions".to_string(),
                    }),
                );
                return;
            }
        };
        active.phase = BrowserAsyncPhase::Act { mutating };
        if let Err(error) = self.start_script(
            &target,
            &operation_id,
            &format!("window.__devmanagerBrowser.act({encoded})"),
        ) {
            self.finish_queued_request(window, target, operation_id, active.request, Err(error));
        } else {
            self.active_requests.insert(target, active);
        }
    }

    fn continue_secret_type(
        &mut self,
        window: &gpui::Window,
        target: BrowserOperationTarget,
        operation_id: String,
        mut active: ActiveBrowserRequest,
    ) {
        if !active.request.cancellation_is_current() {
            self.finish_queued_request(
                window,
                target,
                operation_id,
                active.request,
                Err(BrowserError::Interrupted),
            );
            return;
        }
        if let Err(error) = active.request.validate_secret_sidecar() {
            self.finish_queued_request(window, target, operation_id, active.request, Err(error));
            return;
        }
        active.phase = BrowserAsyncPhase::SecretType;
        if let Err(error) = self.start_secret_type(&target, &operation_id, &active.request) {
            self.finish_queued_request(window, target, operation_id, active.request, Err(error));
        } else {
            self.active_requests.insert(target, active);
        }
    }

    fn start_secret_type(
        &self,
        target: &BrowserOperationTarget,
        operation_id: &str,
        request: &BrowserCommandRequest,
    ) -> Result<(), BrowserError> {
        if !request.cancellation_is_current() {
            return Err(BrowserError::Interrupted);
        }
        let BrowserCommand::SecretType {
            target: action_target,
            ..
        } = request.command()
        else {
            return Err(BrowserError::InvalidInvocation {
                field: "secretSidecar".to_string(),
            });
        };
        self.validate_action_target_reference(request.workspace_key(), action_target)?;
        let view = self.view(&target.workspace_key, &target.tab_id)?;
        if !request.cancellation_is_current() {
            return Err(BrowserError::Interrupted);
        }
        let lease =
            request
                .validate_secret_sidecar()?
                .ok_or_else(|| BrowserError::InvalidInvocation {
                    field: "secretSidecar".to_string(),
                })?;

        let sender = self.async_sender.clone();
        let callback_target = target.clone();
        let callback_operation_id = operation_id.to_string();
        lease
            .with_exposed(|value| {
                if !request.cancellation_is_current() {
                    return Err(BrowserError::Interrupted);
                }
                let mut target_json: Zeroizing<String> = Zeroizing::new(
                    serde_json::to_string(action_target).map_err(|_| {
                        BrowserError::CrashedView {
                            message: "could not encode browser secret target".to_string(),
                        }
                    })?,
                );
                let mut value_json: Zeroizing<String> =
                    Zeroizing::new(serde_json::to_string(value).map_err(|_| {
                        BrowserError::CrashedView {
                            message: "could not encode browser secret value".to_string(),
                        }
                    })?);
                let mut script: Zeroizing<String> = Zeroizing::new(format!(
                    r#"(async () => {{
                      try {{
                        const value = await (window.__devmanagerBrowser.typeSecret({}, {}));
                        return {{ ok: true, value }};
                      }} catch (error) {{
                        const candidate = String(error && error.message || "automation_failed");
                        return {{ ok: false, error: candidate === "element_not_found" ? candidate : "automation_failed" }};
                      }}
                    }})()"#,
                    target_json.as_str(),
                    value_json.as_str(),
                ));
                let accepted = view.evaluate_script_with_callback(&script, move |result| {
                    let _ = sender.send(BrowserAsyncCompletion {
                        target: callback_target.clone(),
                        operation_id: callback_operation_id.clone(),
                        result: Ok(result),
                    });
                });
                script.zeroize();
                value_json.zeroize();
                target_json.zeroize();
                accepted.map_err(view_failure)
            })
            .map_err(|_| BrowserError::InvalidInvocation {
                field: "secretSidecar".to_string(),
            })?
    }

    fn complete_snapshot(
        &mut self,
        request: &BrowserCommandRequest,
        raw: &str,
    ) -> Result<BrowserResponse, BrowserError> {
        let value = script_value(raw)?;
        let elements: Vec<BrowserRawSemanticElement> =
            serde_json::from_value(value).map_err(|_| BrowserError::CrashedView {
                message: "browser semantic snapshot returned invalid data".to_string(),
            })?;
        let tab_id = request.command().tab_id().expect("snapshot tab id");
        let workspace = self
            .state
            .workspace(request.workspace_key())
            .ok_or_else(missing_workspace)?;
        let tab = workspace
            .tabs
            .iter()
            .find(|tab| tab.id == tab_id)
            .ok_or_else(|| missing_tab(tab_id))?;
        let snapshot = build_semantic_snapshot(
            workspace.revision,
            tab.url.clone(),
            tab.title.clone(),
            elements,
        );
        let encoded = serde_json::to_vec(&snapshot).map_err(|error| BrowserError::CrashedView {
            message: format!("could not encode browser semantic snapshot: {error}"),
        })?;
        let resource = self.store_resource(
            request.workspace_key(),
            BrowserResourceKind::DomSnapshot,
            "application/json",
            encoded,
        )?;
        Ok(BrowserResponse::Snapshot {
            summary: BrowserSnapshotSummary {
                tab_id: tab_id.to_string(),
                url: snapshot.url,
                revision: snapshot.revision,
                element_count: snapshot.elements.len(),
            },
            resource,
        })
    }

    fn complete_screenshot(
        &mut self,
        request: &BrowserCommandRequest,
        raw: &str,
    ) -> Result<BrowserResponse, BrowserError> {
        let value: Value = serde_json::from_str(raw).map_err(|_| BrowserError::CrashedView {
            message: "browser screenshot callback returned invalid data".to_string(),
        })?;
        let data =
            value
                .get("data")
                .and_then(Value::as_str)
                .ok_or_else(|| BrowserError::CrashedView {
                    message: "browser screenshot callback omitted PNG data".to_string(),
                })?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(|_| BrowserError::CrashedView {
                message: "browser screenshot callback returned invalid PNG data".to_string(),
            })?;
        let resource = self.store_resource(
            request.workspace_key(),
            BrowserResourceKind::Screenshot,
            "image/png",
            bytes,
        )?;
        Ok(BrowserResponse::Screenshot { resource })
    }

    fn complete_wait(
        &self,
        request: &BrowserCommandRequest,
        raw: &str,
    ) -> Result<BrowserResponse, BrowserError> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct WaitProbe {
            matched: bool,
            elapsed_ms: u64,
        }
        let probe: WaitProbe =
            serde_json::from_value(script_value(raw)?).map_err(|_| BrowserError::CrashedView {
                message: "browser wait callback returned invalid data".to_string(),
            })?;
        if !probe.matched {
            return Err(BrowserError::Timeout {
                operation: "pageCondition".to_string(),
            });
        }
        let revision = self
            .state
            .workspace(request.workspace_key())
            .map(|snapshot| snapshot.revision)
            .ok_or_else(missing_workspace)?;
        Ok(BrowserResponse::Wait {
            result: BrowserWaitResult {
                matched: true,
                elapsed_ms: probe.elapsed_ms,
                revision,
            },
        })
    }

    fn complete_action(
        &mut self,
        request: &BrowserCommandRequest,
        raw: &str,
        mutating: bool,
    ) -> Result<BrowserResponse, BrowserError> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct ActionProbe {
            completed_actions: usize,
        }
        let probe: ActionProbe =
            serde_json::from_value(script_value(raw)?).map_err(|_| BrowserError::CrashedView {
                message: "browser action callback returned invalid data".to_string(),
            })?;
        let tab_id = request.command().tab_id().expect("action tab id");
        let revision = if mutating && probe.completed_actions > 0 {
            self.state
                .apply_automation_mutation(request.workspace_key(), tab_id)?
                .revision
        } else {
            self.state
                .workspace(request.workspace_key())
                .map(|snapshot| snapshot.revision)
                .ok_or_else(missing_workspace)?
        };
        let _ = self
            .event_sender
            .send(BrowserHostEvent::AutomationStateChanged {
                workspace_key: request.workspace_key().clone(),
                tab_id: tab_id.to_string(),
            });
        Ok(BrowserResponse::Action {
            result: BrowserActionResult {
                completed_actions: probe.completed_actions,
                revision,
            },
        })
    }

    fn complete_secret_type(
        &mut self,
        request: &BrowserCommandRequest,
        raw: &str,
    ) -> Result<BrowserResponse, BrowserError> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct SecretActionProbe {
            completed_actions: usize,
        }
        let result: SecretActionProbe =
            serde_json::from_value(script_value(raw)?).map_err(|_| BrowserError::CrashedView {
                message: "browser secret action callback returned invalid data".to_string(),
            })?;
        if result.completed_actions != 1 {
            return Err(BrowserError::CrashedView {
                message: "browser secret action callback returned an invalid count".to_string(),
            });
        }
        self.complete_action(request, raw, true)
    }

    fn complete_console(
        &mut self,
        request: &BrowserCommandRequest,
        raw: &str,
    ) -> Result<BrowserResponse, BrowserError> {
        let entries: Vec<BrowserConsoleEntry> = serde_json::from_value(script_value(raw)?)
            .map_err(|_| BrowserError::CrashedView {
                message: "browser console callback returned invalid data".to_string(),
            })?;
        let encoded = serde_json::to_vec(&entries).map_err(|error| BrowserError::CrashedView {
            message: format!("could not encode browser console result: {error}"),
        })?;
        if encoded.len() > INLINE_RESULT_LIMIT {
            let resource = self.store_resource(
                request.workspace_key(),
                BrowserResourceKind::ConsoleLog,
                "application/json",
                encoded,
            )?;
            Ok(BrowserResponse::Console {
                entries: Vec::new(),
                resource: Some(resource),
            })
        } else {
            Ok(BrowserResponse::Console {
                entries,
                resource: None,
            })
        }
    }

    fn complete_network(
        &mut self,
        request: &BrowserCommandRequest,
        raw: &str,
    ) -> Result<BrowserResponse, BrowserError> {
        let operation = match request.command() {
            BrowserCommand::Network { operation, .. } => *operation,
            _ => unreachable!("network completion belongs to network command"),
        };
        if operation == BrowserNetworkOperation::Body {
            let value = script_value(raw)?;
            let available = value
                .get("available")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if !available {
                return Ok(BrowserResponse::Network {
                    entries: Vec::new(),
                    resource: None,
                    body_available: Some(false),
                });
            }
            let body = value
                .get("body")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .as_bytes()
                .to_vec();
            let resource = self.store_resource(
                request.workspace_key(),
                BrowserResourceKind::NetworkBody,
                "text/plain",
                body,
            )?;
            return Ok(BrowserResponse::Network {
                entries: Vec::new(),
                resource: Some(resource),
                body_available: Some(true),
            });
        }
        let entries: Vec<BrowserNetworkEntry> = serde_json::from_value(script_value(raw)?)
            .map_err(|_| BrowserError::CrashedView {
                message: "browser network callback returned invalid data".to_string(),
            })?;
        let encoded = serde_json::to_vec(&entries).map_err(|error| BrowserError::CrashedView {
            message: format!("could not encode browser network result: {error}"),
        })?;
        if encoded.len() > INLINE_RESULT_LIMIT {
            let resource = self.store_resource(
                request.workspace_key(),
                BrowserResourceKind::NetworkLog,
                "application/json",
                encoded,
            )?;
            Ok(BrowserResponse::Network {
                entries: Vec::new(),
                resource: Some(resource),
                body_available: None,
            })
        } else {
            Ok(BrowserResponse::Network {
                entries,
                resource: None,
                body_available: None,
            })
        }
    }

    fn complete_performance(
        &mut self,
        request: &BrowserCommandRequest,
        raw: &str,
    ) -> Result<BrowserResponse, BrowserError> {
        let operation = match request.command() {
            BrowserCommand::Performance { operation, .. } => *operation,
            _ => unreachable!("performance completion belongs to performance command"),
        };
        let value = script_value(raw)?;
        match operation {
            BrowserPerformanceOperation::TraceStart => Ok(BrowserResponse::Performance {
                snapshot: None,
                resource: None,
                tracing: true,
            }),
            BrowserPerformanceOperation::TraceStop => {
                let encoded =
                    serde_json::to_vec(&value).map_err(|error| BrowserError::CrashedView {
                        message: format!("could not encode browser performance trace: {error}"),
                    })?;
                let resource = self.store_resource(
                    request.workspace_key(),
                    BrowserResourceKind::PerformanceTrace,
                    "application/json",
                    encoded,
                )?;
                Ok(BrowserResponse::Performance {
                    snapshot: None,
                    resource: Some(resource),
                    tracing: false,
                })
            }
            BrowserPerformanceOperation::Snapshot => {
                let snapshot: BrowserPerformanceSnapshot =
                    serde_json::from_value(value).map_err(|_| BrowserError::CrashedView {
                        message: "browser performance callback returned invalid data".to_string(),
                    })?;
                let encoded =
                    serde_json::to_vec(&snapshot).map_err(|error| BrowserError::CrashedView {
                        message: format!("could not encode browser performance snapshot: {error}"),
                    })?;
                if encoded.len() > INLINE_RESULT_LIMIT {
                    let resource = self.store_resource(
                        request.workspace_key(),
                        BrowserResourceKind::PerformanceTrace,
                        "application/json",
                        encoded,
                    )?;
                    Ok(BrowserResponse::Performance {
                        snapshot: None,
                        resource: Some(resource),
                        tracing: false,
                    })
                } else {
                    Ok(BrowserResponse::Performance {
                        snapshot: Some(snapshot),
                        resource: None,
                        tracing: false,
                    })
                }
            }
        }
    }

    fn complete_cdp(
        &mut self,
        request: &BrowserCommandRequest,
        raw: &str,
    ) -> Result<BrowserResponse, BrowserError> {
        let redacted = redact_browser_resource_bytes("application/json", raw.as_bytes());
        let value: Value =
            serde_json::from_slice(&redacted).map_err(|_| BrowserError::CrashedView {
                message: "browser CDP callback returned invalid JSON".to_string(),
            })?;
        if redacted.len() > INLINE_RESULT_LIMIT {
            let resource = self.store_resource(
                request.workspace_key(),
                BrowserResourceKind::CdpResult,
                "application/json",
                &redacted,
            )?;
            Ok(BrowserResponse::Cdp {
                inline_result: None,
                resource: Some(resource),
            })
        } else {
            Ok(BrowserResponse::Cdp {
                inline_result: Some(value),
                resource: None,
            })
        }
    }

    fn continue_upload_after_mark(
        &mut self,
        window: &gpui::Window,
        target: BrowserOperationTarget,
        operation_id: String,
        mut active: ActiveBrowserRequest,
        raw: String,
        paths: Vec<PathBuf>,
        token: String,
    ) {
        let marked = script_value(&raw)
            .ok()
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        if !marked {
            self.finish_queued_request(
                window,
                target,
                operation_id,
                active.request,
                Err(BrowserError::MissingFile {
                    path: PathBuf::from("semantic file input target"),
                }),
            );
            return;
        }
        let selector = format!("[data-devmanager-upload=\"{token}\"]");
        let params = json!({
            "expression": format!("document.querySelector({})", serde_json::to_string(&selector).unwrap()),
            "returnByValue": false,
        });
        active.phase = BrowserAsyncPhase::UploadRuntime { paths, token };
        if let Err(error) = self.start_cdp(&target, &operation_id, "Runtime.evaluate", &params) {
            self.finish_queued_request(window, target, operation_id, active.request, Err(error));
        } else {
            self.active_requests.insert(target, active);
        }
    }

    fn continue_upload_after_runtime(
        &mut self,
        window: &gpui::Window,
        target: BrowserOperationTarget,
        operation_id: String,
        mut active: ActiveBrowserRequest,
        raw: String,
        paths: Vec<PathBuf>,
        token: String,
    ) {
        let object_id = serde_json::from_str::<Value>(&raw).ok().and_then(|value| {
            value
                .pointer("/result/objectId")
                .and_then(Value::as_str)
                .map(str::to_string)
        });
        let Some(object_id) = object_id else {
            self.finish_queued_request(
                window,
                target,
                operation_id,
                active.request,
                Err(BrowserError::CrashedView {
                    message: "browser upload target could not be resolved through CDP".to_string(),
                }),
            );
            return;
        };
        active.phase = BrowserAsyncPhase::UploadDescribe { paths, token };
        let params = json!({"objectId": object_id});
        if let Err(error) = self.start_cdp(&target, &operation_id, "DOM.describeNode", &params) {
            self.finish_queued_request(window, target, operation_id, active.request, Err(error));
        } else {
            self.active_requests.insert(target, active);
        }
    }

    fn continue_upload_after_describe(
        &mut self,
        window: &gpui::Window,
        target: BrowserOperationTarget,
        operation_id: String,
        mut active: ActiveBrowserRequest,
        raw: String,
        paths: Vec<PathBuf>,
        token: String,
    ) {
        let backend_node_id = serde_json::from_str::<Value>(&raw)
            .ok()
            .and_then(|value| value.pointer("/node/backendNodeId").and_then(Value::as_u64));
        let Some(backend_node_id) = backend_node_id else {
            self.finish_queued_request(
                window,
                target,
                operation_id,
                active.request,
                Err(BrowserError::CrashedView {
                    message: "browser upload target omitted a CDP backend node id".to_string(),
                }),
            );
            return;
        };
        let files = paths
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        active.phase = BrowserAsyncPhase::UploadSet { paths, token };
        let params = json!({"files": files, "backendNodeId": backend_node_id});
        if let Err(error) = self.start_cdp(&target, &operation_id, "DOM.setFileInputFiles", &params)
        {
            self.finish_queued_request(window, target, operation_id, active.request, Err(error));
        } else {
            self.active_requests.insert(target, active);
        }
    }

    fn complete_upload(
        &mut self,
        request: &BrowserCommandRequest,
        raw: &str,
        paths: Vec<PathBuf>,
    ) -> Result<BrowserResponse, BrowserError> {
        let _: Value = serde_json::from_str(raw).map_err(|_| BrowserError::CrashedView {
            message: "browser upload callback returned invalid data".to_string(),
        })?;
        let tab_id = request.command().tab_id().expect("upload tab id");
        let revision = self
            .state
            .apply_automation_mutation(request.workspace_key(), tab_id)?
            .revision;
        let _ = self
            .event_sender
            .send(BrowserHostEvent::AutomationStateChanged {
                workspace_key: request.workspace_key().clone(),
                tab_id: tab_id.to_string(),
            });
        Ok(BrowserResponse::Upload {
            result: BrowserUploadResult {
                files: paths,
                revision,
            },
        })
    }

    fn store_resource(
        &self,
        workspace_key: &BrowserWorkspaceKey,
        kind: BrowserResourceKind,
        mime_type: &str,
        bytes: impl AsRef<[u8]>,
    ) -> Result<BrowserResourceHandle, BrowserError> {
        let bytes = redact_browser_resource_bytes(mime_type, bytes.as_ref());
        BrowserResourceStore::open_verified(
            self.verified_trusted_app_config_dir()?,
            &workspace_key.project_id,
            BrowserResourceLimits::default(),
        )?
        .put(workspace_key, kind, mime_type, bytes, false)
    }

    fn store_pinned_resource(
        &self,
        workspace_key: &BrowserWorkspaceKey,
        kind: BrowserResourceKind,
        mime_type: &str,
        bytes: impl AsRef<[u8]>,
    ) -> Result<BrowserResourceHandle, BrowserError> {
        let bytes = redact_browser_resource_bytes(mime_type, bytes.as_ref());
        BrowserResourceStore::open_verified(
            self.verified_trusted_app_config_dir()?,
            &workspace_key.project_id,
            BrowserResourceLimits::default(),
        )?
        .put(workspace_key, kind, mime_type, bytes, true)
    }

    fn set_resource_pinned(
        &self,
        workspace_key: &BrowserWorkspaceKey,
        resource_id: &BrowserResourceId,
        pinned: bool,
    ) -> Result<BrowserResourceHandle, BrowserError> {
        BrowserResourceStore::open_verified(
            self.verified_trusted_app_config_dir()?,
            &workspace_key.project_id,
            BrowserResourceLimits::default(),
        )?
        .set_pinned(workspace_key, resource_id, pinned)
    }

    fn reconcile_annotation_pins(
        &self,
        workspace_key: &BrowserWorkspaceKey,
    ) -> Result<(), BrowserError> {
        let mut pinned = self
            .state
            .workspace(workspace_key)
            .ok_or_else(missing_workspace)?
            .pinned_annotation_resource_ids();
        pinned.extend(
            self.annotation_lifecycle
                .draft_resource_ids_for_workspace(workspace_key),
        );
        BrowserResourceStore::open_verified(
            self.verified_trusted_app_config_dir()?,
            &workspace_key.project_id,
            BrowserResourceLimits::default(),
        )?
        .reconcile_annotation_pins(workspace_key, &pinned)
    }

    fn refresh_annotation_response_handles(
        &self,
        workspace_key: &BrowserWorkspaceKey,
        response: &mut BrowserResponse,
    ) -> Result<(), BrowserError> {
        let resources = BrowserResourceStore::open_verified(
            self.verified_trusted_app_config_dir()?,
            &workspace_key.project_id,
            BrowserResourceLimits::default(),
        )?;
        match response {
            BrowserResponse::Annotation { details, .. } => {
                details.screenshot = resources.handle(workspace_key, &details.screenshot.id)?;
                details.details_resource =
                    resources.handle(workspace_key, &details.details_resource.id)?;
            }
            BrowserResponse::AnnotationMutation { result } => {
                result.screenshot = resources.handle(workspace_key, &result.screenshot.id)?;
            }
            _ => {}
        }
        Ok(())
    }

    fn finalize_annotation_command_resources(
        &self,
        workspace_key: &BrowserWorkspaceKey,
        response: &mut BrowserResponse,
    ) -> Result<(), BrowserError> {
        self.reconcile_annotation_pins(workspace_key)?;
        self.refresh_annotation_response_handles(workspace_key, response)
    }

    fn validate_action_references(
        &self,
        workspace_key: &BrowserWorkspaceKey,
        actions: &[BrowserAction],
    ) -> Result<(), BrowserError> {
        let snapshot = self
            .state
            .workspace(workspace_key)
            .ok_or_else(missing_workspace)?;
        for action in actions {
            if let Some(element) = action
                .target()
                .and_then(|target| target.element_ref.as_ref())
            {
                snapshot.validate_element_ref(element)?;
            }
            if let BrowserAction::DragDrop { destination, .. } = action {
                if let Some(element) = destination.element_ref.as_ref() {
                    snapshot.validate_element_ref(element)?;
                }
            }
        }
        Ok(())
    }

    fn validate_action_target_reference(
        &self,
        workspace_key: &BrowserWorkspaceKey,
        target: &BrowserActionTarget,
    ) -> Result<(), BrowserError> {
        if let Some(element) = target.element_ref.as_ref() {
            self.state
                .workspace(workspace_key)
                .ok_or_else(missing_workspace)?
                .validate_element_ref(element)?;
        }
        Ok(())
    }

    fn validate_wait_reference(
        &self,
        workspace_key: &BrowserWorkspaceKey,
        condition: &crate::browser::BrowserWaitCondition,
    ) -> Result<(), BrowserError> {
        use crate::browser::BrowserWaitCondition;
        let target = match condition {
            BrowserWaitCondition::ElementPresent { target }
            | BrowserWaitCondition::ElementAbsent { target }
            | BrowserWaitCondition::ElementVisible { target }
            | BrowserWaitCondition::ElementHidden { target }
            | BrowserWaitCondition::ElementValue { target, .. } => Some(target),
            _ => None,
        };
        if let Some(element) = target.and_then(|target| target.element_ref.as_ref()) {
            self.state
                .workspace(workspace_key)
                .ok_or_else(missing_workspace)?
                .validate_element_ref(element)?;
        }
        Ok(())
    }

    fn canonical_upload_paths(&self, paths: &[PathBuf]) -> Result<Vec<PathBuf>, BrowserError> {
        if paths.is_empty() || paths.len() > 16 {
            return Err(BrowserError::InvalidInvocation {
                field: "paths".to_string(),
            });
        }
        let mut canonical_paths = Vec::with_capacity(paths.len());
        for path in paths {
            let canonical = path.canonicalize().map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    BrowserError::MissingFile { path: path.clone() }
                } else {
                    BrowserError::Io {
                        operation: "canonicalize upload file".to_string(),
                        path: path.clone(),
                        message: error.to_string(),
                    }
                }
            })?;
            let metadata = std::fs::metadata(&canonical).map_err(|error| BrowserError::Io {
                operation: "inspect upload file".to_string(),
                path: canonical.clone(),
                message: error.to_string(),
            })?;
            if !metadata.is_file() {
                return Err(BrowserError::MissingFile { path: canonical });
            }
            canonical_paths.push(canonical);
        }
        Ok(canonical_paths)
    }

    fn handle_download_command(
        &self,
        request: &BrowserCommandRequest,
    ) -> Result<BrowserResponse, BrowserError> {
        let (operation, download_id) = match request.command() {
            BrowserCommand::Downloads {
                operation,
                download_id,
                ..
            } => (*operation, download_id.as_deref()),
            _ => unreachable!("download handler belongs to downloads command"),
        };
        let downloads = BrowserDownloadStore::open_verified(
            self.verified_trusted_app_config_dir()?,
            &request.workspace_key().project_id,
        )?;
        match operation {
            crate::browser::BrowserDownloadOperation::List => Ok(BrowserResponse::Downloads {
                downloads: downloads.list()?,
            }),
            crate::browser::BrowserDownloadOperation::Reveal => {
                let id = download_id.ok_or_else(|| BrowserError::InvalidInvocation {
                    field: "downloadId".to_string(),
                })?;
                let path = downloads.resolve(id)?;
                std::process::Command::new("explorer.exe")
                    .arg(format!("/select,{}", path.display()))
                    .spawn()
                    .map_err(|error| BrowserError::Io {
                        operation: "reveal browser download".to_string(),
                        path,
                        message: error.to_string(),
                    })?;
                Ok(BrowserResponse::Downloads {
                    downloads: Vec::new(),
                })
            }
            crate::browser::BrowserDownloadOperation::Delete => {
                let id = download_id.ok_or_else(|| BrowserError::InvalidInvocation {
                    field: "downloadId".to_string(),
                })?;
                downloads.delete(id)?;
                Ok(BrowserResponse::Downloads {
                    downloads: Vec::new(),
                })
            }
        }
    }

    pub fn set_active_workspace(
        &mut self,
        workspace_key: Option<BrowserWorkspaceKey>,
    ) -> Result<(), BrowserError> {
        if let Some(previous) = self.state.active_workspace().cloned() {
            if workspace_key.as_ref() != Some(&previous) {
                self.cancel_workspace_annotations(&previous);
            }
        }
        self.state.set_active_workspace(workspace_key);
        self.apply_visibility_plan()
    }

    pub fn set_bounds(&mut self, bounds: BrowserBounds) -> Result<(), BrowserError> {
        self.bounds = BrowserBounds {
            width: bounds.width.max(1),
            height: bounds.height.max(1),
            ..bounds
        };
        self.apply_visibility_plan()
    }

    pub fn drain_events(&mut self) -> Vec<BrowserHostEvent> {
        self.pump_page_recording_ipc();
        let incoming: Vec<_> = self.event_receiver.try_iter().collect();
        let mut events = Vec::with_capacity(incoming.len());
        for event in incoming {
            match &event {
                BrowserHostEvent::UrlChanged {
                    workspace_key,
                    tab_id,
                    url,
                } => {
                    let _ = self.remove_page_recording_view(workspace_key, tab_id);
                    if let Ok(route) = BrowserAnnotationRoute::new(workspace_key.clone(), tab_id) {
                        self.cancel_annotation_route(&route);
                    }
                    let _ = self.state.navigate_tab(workspace_key, tab_id, url);
                }
                BrowserHostEvent::TitleChanged {
                    workspace_key,
                    tab_id,
                    title,
                } => {
                    if let Ok(route) = BrowserAnnotationRoute::new(workspace_key.clone(), tab_id) {
                        self.cancel_annotation_mode(&route);
                    }
                    let _ = self.state.apply_title_change(workspace_key, tab_id, title);
                }
                BrowserHostEvent::PageLoad {
                    workspace_key,
                    tab_id,
                    state,
                    url,
                } => {
                    if let Ok(route) = BrowserAnnotationRoute::new(workspace_key.clone(), tab_id) {
                        self.cancel_annotation_route(&route);
                    }
                    if *state == BrowserPageLoadState::Finished {
                        let _ = self.state.apply_page_load(workspace_key, tab_id, url);
                        if self
                            .workflow_coordinator
                            .active_instance(workspace_key)
                            .is_some()
                            && self
                                .install_page_recording_view(workspace_key, tab_id)
                                .is_err()
                        {
                            self.emit_diagnostic(
                                workspace_key,
                                tab_id,
                                "browser recording instrumentation could not be installed"
                                    .to_string(),
                            );
                        }
                    }
                }
                BrowserHostEvent::UserInput {
                    workspace_key,
                    tab_id,
                    ..
                } => {
                    self.cancel_tab_operations(workspace_key, tab_id);
                    if let Ok(route) = BrowserAnnotationRoute::new(workspace_key.clone(), tab_id) {
                        self.cancel_annotation_mode(&route);
                    }
                    let _ = self.state.apply_user_input(workspace_key, tab_id);
                }
                BrowserHostEvent::DomMutation {
                    workspace_key,
                    tab_id,
                } => {
                    if let Ok(route) = BrowserAnnotationRoute::new(workspace_key.clone(), tab_id) {
                        self.cancel_annotation_mode(&route);
                    }
                    let _ = self.state.apply_dom_mutation(workspace_key, tab_id);
                }
                BrowserHostEvent::AnnotationCandidate {
                    workspace_key,
                    tab_id,
                    candidate,
                } => {
                    let route = match BrowserAnnotationRoute::new(workspace_key.clone(), tab_id) {
                        Ok(route) => route,
                        Err(error) => {
                            self.emit_diagnostic(workspace_key, tab_id, error.to_string());
                            continue;
                        }
                    };
                    match self
                        .annotation_lifecycle
                        .accept_candidate(&route, candidate.clone())
                    {
                        Ok(candidate) => {
                            self.accepted_annotation_candidates.insert(route, candidate);
                            let _ =
                                self.event_sender
                                    .send(BrowserHostEvent::AnnotationModeChanged {
                                        workspace_key: workspace_key.clone(),
                                        tab_id: tab_id.clone(),
                                        enabled: false,
                                    });
                        }
                        Err(error) => {
                            self.emit_diagnostic(workspace_key, tab_id, error.to_string());
                            continue;
                        }
                    }
                }
                BrowserHostEvent::AnnotationCanceled {
                    workspace_key,
                    tab_id,
                } => {
                    if let Ok(route) = BrowserAnnotationRoute::new(workspace_key.clone(), tab_id) {
                        self.cancel_annotation_mode(&route);
                    }
                }
                BrowserHostEvent::AnnotationDraftReady { .. }
                | BrowserHostEvent::AnnotationModeChanged { .. } => {}
                BrowserHostEvent::AutomationStateChanged { .. } => {}
                BrowserHostEvent::ApprovalRequested { .. } => {}
                BrowserHostEvent::NewWindow { .. }
                | BrowserHostEvent::Download { .. }
                | BrowserHostEvent::Diagnostic { .. } => {}
            }
            events.push(event);
        }
        events
    }

    fn pump_page_recording_ipc(&mut self) {
        let batch = self.recording_transport.drain();
        for failure in batch.failures {
            if self
                .workflow_coordinator
                .active_instance(&failure.workspace_key)
                .is_some_and(|active| active.id() == failure.instance_id)
            {
                self.invalidate_page_recording_transport(
                    &failure.workspace_key,
                    &failure.tab_id,
                    failure.kind,
                );
            }
        }
        for message in batch.messages {
            if self
                .workflow_coordinator
                .active_instance(&message.workspace_key)
                .is_none_or(|active| active.id() != message.instance_id)
            {
                continue;
            }
            let key = view_key(&message.workspace_key, &message.tab_id);
            let Some(ipc) = self.recording_views.get_mut(&key) else {
                continue;
            };
            let _ = self.workflow_coordinator.with_recorder(|recorder| {
                ipc.ingest_from_origin(recorder, &message.observed_origin, &message.body)
            });
        }
    }

    fn invalidate_page_recording_transport(
        &mut self,
        workspace_key: &BrowserWorkspaceKey,
        tab_id: &str,
        kind: BrowserPageRecordingTransportFailureKind,
    ) {
        let detail = match kind {
            BrowserPageRecordingTransportFailureKind::Overflow => "overflowed",
            BrowserPageRecordingTransportFailureKind::Disconnected => "disconnected",
        };
        self.discard_page_recording(workspace_key);
        self.emit_diagnostic(
            workspace_key,
            tab_id,
            format!("browser recording transport {detail}; the incomplete recording was discarded"),
        );
    }

    fn install_page_recording_view(
        &mut self,
        workspace_key: &BrowserWorkspaceKey,
        tab_id: &str,
    ) -> Result<(), BrowserPageRecordingIpcError> {
        let instance = self
            .workflow_coordinator
            .active_instance(workspace_key)
            .ok_or(BrowserPageRecordingIpcError::Inactive)?;
        let snapshot = self
            .state
            .workspace(workspace_key)
            .ok_or(BrowserPageRecordingIpcError::Untrusted)?;
        let tab = snapshot
            .tabs
            .iter()
            .find(|tab| tab.id == tab_id)
            .ok_or(BrowserPageRecordingIpcError::Untrusted)?;
        let Some(origin) = page_origin(&tab.url) else {
            let _ = self.remove_page_recording_view(workspace_key, tab_id);
            return Ok(());
        };
        let revision = snapshot.revision;
        let key = view_key(workspace_key, tab_id);
        if !self.views.contains_key(&key) {
            return Err(BrowserPageRecordingIpcError::HostFailure);
        }
        let _ = self.remove_page_recording_view(workspace_key, tab_id);
        let nonce = random_page_recording_nonce()?;
        let authority =
            BrowserPageRecordingAuthority::new(instance, tab_id, revision, origin, nonce)?;
        let ingress = self
            .recording_ingresses
            .get(&key)
            .ok_or(BrowserPageRecordingIpcError::HostFailure)?;
        ingress.activate(authority.instance_id(), authority.nonce())?;
        let mut ipc = BrowserPageRecordingIpc::default();
        ipc.activate(authority)?;
        let script = ipc.activation_script()?;
        let install = self
            .views
            .get(&key)
            .ok_or(BrowserPageRecordingIpcError::HostFailure)?
            .evaluate_script(&script)
            .map_err(|_| BrowserPageRecordingIpcError::HostFailure);
        if install.is_err() {
            let _ = ipc.fence_transport(ingress);
            return install;
        }
        self.recording_views.insert(key, ipc);
        Ok(())
    }

    fn remove_page_recording_view(
        &mut self,
        workspace_key: &BrowserWorkspaceKey,
        tab_id: &str,
    ) -> Result<(), BrowserPageRecordingIpcError> {
        let key = view_key(workspace_key, tab_id);
        let Some(mut ipc) = self.recording_views.remove(&key) else {
            return Ok(());
        };
        if let Some(ingress) = self.recording_ingresses.get(&key) {
            let _ = ipc.fence_transport(ingress);
        }
        let script = ipc.deactivation_script()?;
        let result = self
            .views
            .get(&key)
            .map(|view| view.evaluate_script(&script))
            .transpose()
            .map_err(|_| BrowserPageRecordingIpcError::HostFailure);
        ipc.deactivate();
        result.map(|_| ())
    }

    fn fence_workspace_recording_views(&mut self, workspace_key: &BrowserWorkspaceKey) {
        let keys = self
            .recording_views
            .keys()
            .filter(|key| &key.workspace_key == workspace_key)
            .cloned()
            .collect::<Vec<_>>();
        for key in keys {
            let Some(ipc) = self.recording_views.get(&key) else {
                continue;
            };
            if let Some(ingress) = self.recording_ingresses.get(&key) {
                let _ = ipc.fence_transport(ingress);
            }
            if let (Ok(script), Some(view)) = (ipc.deactivation_script(), self.views.get(&key)) {
                let _ = view.evaluate_script(&script);
            }
        }
    }

    fn remove_workspace_recording_views(&mut self, workspace_key: &BrowserWorkspaceKey) {
        let tab_ids = self
            .recording_views
            .keys()
            .filter(|key| &key.workspace_key == workspace_key)
            .map(|key| key.tab_id.clone())
            .collect::<Vec<_>>();
        for tab_id in tab_ids {
            let _ = self.remove_page_recording_view(workspace_key, &tab_id);
        }
    }

    fn discard_page_recording(&mut self, workspace_key: &BrowserWorkspaceKey) {
        self.discard_workflow_state(workspace_key);
    }

    fn discard_project_page_recordings(&mut self, project_id: &str) {
        let workspace_keys = self
            .workflow_coordinator
            .current_project_instances(project_id)
            .into_iter()
            .map(|instance| instance.workspace_key().clone())
            .collect::<Vec<_>>();
        for workspace_key in workspace_keys {
            self.discard_page_recording(&workspace_key);
        }
    }

    pub fn workspace_snapshot(
        &self,
        workspace_key: &BrowserWorkspaceKey,
    ) -> Option<&BrowserWorkspaceSnapshot> {
        self.state.workspace(workspace_key)
    }

    pub fn acknowledge_attachment_projection(
        &mut self,
        projection: &BrowserAttachmentProjection,
    ) -> Result<BrowserWorkspaceSnapshot, BrowserError> {
        let additional_pinned_resource_ids = self
            .annotation_lifecycle
            .draft_resource_ids_for_workspace(&projection.workspace_key);
        let resources = BrowserResourceStore::open_verified(
            self.verified_trusted_app_config_dir()?,
            &projection.workspace_key.project_id,
            BrowserResourceLimits::default(),
        )?;
        acknowledge_attachment_projection_and_reconcile_pins(
            &mut self.state,
            &resources,
            projection,
            additional_pinned_resource_ids,
        )
    }

    fn handle_command_inner(
        &mut self,
        window: &gpui::Window,
        workspace_key: &BrowserWorkspaceKey,
        command: BrowserCommand,
    ) -> Result<BrowserResponse, BrowserError> {
        if command != BrowserCommand::Status {
            self.ensure_runtime_available()?;
        }
        match command {
            BrowserCommand::Status => Ok(BrowserResponse::Status {
                status: self.status(),
            }),
            BrowserCommand::DownloadDirectory => {
                let downloads_dir = prepare_verified_download_root(
                    self.verified_trusted_app_config_dir()?,
                    &workspace_key.project_id,
                )?;
                Ok(BrowserResponse::DownloadDirectory {
                    path: downloads_dir,
                })
            }
            BrowserCommand::ClearProjectProfile => {
                self.clear_project_profile(workspace_key)?;
                Ok(BrowserResponse::Acknowledged)
            }
            command => self.handle_available_command(window, workspace_key, command),
        }
    }

    fn handle_available_command(
        &mut self,
        window: &gpui::Window,
        workspace_key: &BrowserWorkspaceKey,
        command: BrowserCommand,
    ) -> Result<BrowserResponse, BrowserError> {
        match command {
            BrowserCommand::WorkspaceState => {
                let snapshot = self
                    .state
                    .workspace(workspace_key)
                    .cloned()
                    .ok_or_else(missing_workspace)?;
                Ok(BrowserResponse::WorkspaceState { snapshot })
            }
            BrowserCommand::Ensure { snapshot } => {
                let mutation = self
                    .state
                    .ensure_workspace(workspace_key.clone(), snapshot)?;
                self.reconcile_annotation_pins(workspace_key)?;
                self.retry_annotation_cleanups(workspace_key);
                self.ensure_selected_view(window, workspace_key)?;
                self.apply_visibility_plan()?;
                Ok(BrowserResponse::Workspace { mutation })
            }
            BrowserCommand::SetPaneOpen { open } => {
                if !open {
                    self.cancel_workspace_annotations(workspace_key);
                }
                let mutation = self.state.set_pane_open(workspace_key, open)?;
                self.apply_visibility_plan()?;
                Ok(BrowserResponse::Workspace { mutation })
            }
            BrowserCommand::SetAnnotationMode { tab_id, enabled } => {
                let route = BrowserAnnotationRoute::new(workspace_key.clone(), &tab_id)?;
                if !enabled {
                    self.cancel_annotation_route(&route);
                    return Ok(BrowserResponse::Acknowledged);
                }
                self.cancel_workspace_annotations(workspace_key);
                let snapshot = self
                    .state
                    .workspace(workspace_key)
                    .ok_or_else(missing_workspace)?;
                let tab = snapshot
                    .tabs
                    .iter()
                    .find(|tab| tab.id == tab_id)
                    .ok_or_else(|| missing_tab(&tab_id))?;
                let url = tab.url.clone();
                let revision = snapshot.revision;
                self.ensure_existing_tab_view(window, workspace_key, &tab_id)?;
                let context = json!({"url": url, "revision": revision.0});
                self.view(workspace_key, &tab_id)?
                    .evaluate_script(&format!(
                        "window.__devmanagerBrowser.annotation.start({context})"
                    ))
                    .map_err(view_failure)?;
                self.annotation_lifecycle.activate(route, url, revision);
                let _ = self
                    .event_sender
                    .send(BrowserHostEvent::AnnotationModeChanged {
                        workspace_key: workspace_key.clone(),
                        tab_id,
                        enabled: true,
                    });
                Ok(BrowserResponse::Acknowledged)
            }
            BrowserCommand::CaptureAnnotation { tab_id, candidate } => {
                self.begin_annotation_capture(window, workspace_key, &tab_id, candidate)?;
                Ok(BrowserResponse::Acknowledged)
            }
            BrowserCommand::SaveAnnotationDraft { draft_id, comment } => {
                let draft = self
                    .annotation_lifecycle
                    .take_draft(workspace_key, &draft_id)?;
                let route = BrowserAnnotationRoute::new(workspace_key.clone(), &draft.tab_id)?;
                let annotation = match draft.clone().into_annotation(comment) {
                    Ok(annotation) => annotation,
                    Err(error) => {
                        self.annotation_lifecycle.restore_draft(route, draft);
                        return Err(error);
                    }
                };
                let mutation = match self.state.save_annotation(workspace_key, annotation) {
                    Ok(mutation) => mutation,
                    Err(error) => {
                        self.annotation_lifecycle.restore_draft(route, draft);
                        return Err(error);
                    }
                };
                if let Err(error) = self.reconcile_annotation_pins(workspace_key) {
                    self.emit_diagnostic(
                        workspace_key,
                        &draft.tab_id,
                        format!(
                            "saved annotation, but resource pin reconciliation failed: {error}"
                        ),
                    );
                }
                Ok(BrowserResponse::Workspace { mutation })
            }
            BrowserCommand::CancelAnnotationDraft { draft_id } => {
                let draft = self
                    .annotation_lifecycle
                    .take_draft(workspace_key, &draft_id)?;
                let route = BrowserAnnotationRoute::new(workspace_key.clone(), &draft.tab_id)?;
                if let Err(error) =
                    self.set_resource_pinned(workspace_key, &draft.screenshot_resource, false)
                {
                    self.annotation_lifecycle.restore_draft(route, draft);
                    return Err(error);
                }
                Ok(BrowserResponse::Acknowledged)
            }
            BrowserCommand::Annotations {
                operation,
                annotation_id,
            } => {
                let resources = BrowserResourceStore::open_verified(
                    self.verified_trusted_app_config_dir()?,
                    &workspace_key.project_id,
                    BrowserResourceLimits::default(),
                )?;
                match operation {
                    crate::browser::BrowserAnnotationOperation::List => {
                        let annotations = self.state.annotation_summaries(workspace_key)?;
                        let snapshot = self
                            .state
                            .workspace(workspace_key)
                            .cloned()
                            .ok_or_else(missing_workspace)?;
                        Ok(BrowserResponse::Annotations {
                            annotations,
                            mutation: crate::browser::BrowserWorkspaceMutation {
                                revision: snapshot.revision,
                                snapshot,
                            },
                        })
                    }
                    crate::browser::BrowserAnnotationOperation::Get => {
                        let annotation_id = required_annotation_id(annotation_id)?;
                        let details = self.state.annotation_details(
                            workspace_key,
                            &annotation_id,
                            &resources,
                        )?;
                        let snapshot = self
                            .state
                            .workspace(workspace_key)
                            .cloned()
                            .ok_or_else(missing_workspace)?;
                        Ok(BrowserResponse::Annotation {
                            details,
                            mutation: crate::browser::BrowserWorkspaceMutation {
                                revision: snapshot.revision,
                                snapshot,
                            },
                        })
                    }
                    crate::browser::BrowserAnnotationOperation::Resolve
                    | crate::browser::BrowserAnnotationOperation::Unresolve
                    | crate::browser::BrowserAnnotationOperation::Delete => {
                        let annotation_id = required_annotation_id(annotation_id)?;
                        let result = self.state.apply_annotation_operation(
                            workspace_key,
                            operation,
                            &annotation_id,
                            &resources,
                        )?;
                        Ok(BrowserResponse::AnnotationMutation { result })
                    }
                }
            }
            BrowserCommand::Recording { operation } => match operation {
                BrowserRecordingOperation::Status => Ok(BrowserResponse::Recording {
                    result: browser_recording_status_result(
                        &self.workflow_coordinator,
                        workspace_key,
                        operation,
                    ),
                }),
                BrowserRecordingOperation::Start => {
                    self.start_page_recording(workspace_key)
                        .map_err(recording_ipc_browser_error)?;
                    Ok(BrowserResponse::Recording {
                        result: browser_recording_status_result(
                            &self.workflow_coordinator,
                            workspace_key,
                            operation,
                        ),
                    })
                }
                BrowserRecordingOperation::Stop => {
                    let instance = self
                        .workflow_coordinator
                        .active_instance(workspace_key)
                        .ok_or_else(stale_recording_instance)?;
                    self.stop_page_recording(&instance)
                        .map_err(recording_ipc_browser_error)?;
                    let resources = self.recording_review_resource_store(workspace_key)?;
                    Ok(BrowserResponse::Recording {
                        result: browser_recording_review_result(
                            &self.workflow_coordinator,
                            workspace_key,
                            operation,
                            &resources,
                        )?,
                    })
                }
                BrowserRecordingOperation::Review => {
                    let resources = self.recording_review_resource_store(workspace_key)?;
                    Ok(BrowserResponse::Recording {
                        result: browser_recording_review_result(
                            &self.workflow_coordinator,
                            workspace_key,
                            operation,
                            &resources,
                        )?,
                    })
                }
                BrowserRecordingOperation::Discard | BrowserRecordingOperation::Save => {
                    Err(BrowserError::CrashedView {
                        message:
                            "browser recording mutation requires the authenticated request path"
                                .to_string(),
                    })
                }
            },
            BrowserCommand::ListTabs => {
                let snapshot = self
                    .state
                    .workspace(workspace_key)
                    .ok_or_else(missing_workspace)?;
                Ok(BrowserResponse::Tabs {
                    tabs: snapshot.tabs.clone(),
                    selected_tab_id: snapshot.selected_tab_id.clone(),
                })
            }
            BrowserCommand::CreateTab { url } => {
                self.cancel_workspace_annotations(workspace_key);
                let mutation = self
                    .state
                    .create_tab(workspace_key, url.as_deref().unwrap_or("about:blank"))?;
                self.ensure_selected_view(window, workspace_key)?;
                self.apply_visibility_plan()?;
                Ok(BrowserResponse::Workspace { mutation })
            }
            BrowserCommand::SelectTab { tab_id } => {
                self.cancel_workspace_annotations(workspace_key);
                let mutation = self.state.select_tab(workspace_key, &tab_id)?;
                self.ensure_selected_view(window, workspace_key)?;
                self.apply_visibility_plan()?;
                Ok(BrowserResponse::Workspace { mutation })
            }
            BrowserCommand::CloseTab { tab_id } => {
                if let Ok(route) = BrowserAnnotationRoute::new(workspace_key.clone(), &tab_id) {
                    self.cancel_annotation_route(&route);
                }
                let _ = self.remove_page_recording_view(workspace_key, &tab_id);
                let key = view_key(workspace_key, &tab_id);
                self.views.remove(&key);
                self.recording_ingresses.remove(&key);
                let mutation = self.state.close_tab(workspace_key, &tab_id)?;
                self.ensure_selected_view(window, workspace_key)?;
                self.apply_visibility_plan()?;
                Ok(BrowserResponse::Workspace { mutation })
            }
            BrowserCommand::Navigate { tab_id, url } => {
                if let Ok(route) = BrowserAnnotationRoute::new(workspace_key.clone(), &tab_id) {
                    self.cancel_annotation_route(&route);
                }
                let _ = self.remove_page_recording_view(workspace_key, &tab_id);
                let url = validate_browser_url(&url)?;
                self.ensure_existing_tab_view(window, workspace_key, &tab_id)?;
                self.view(workspace_key, &tab_id)?
                    .load_url(&url)
                    .map_err(|error| BrowserError::NavigationFailure {
                        url: url.clone(),
                        message: error.to_string(),
                    })?;
                let mutation = self.state.navigate_tab(workspace_key, &tab_id, &url)?;
                Ok(BrowserResponse::Workspace { mutation })
            }
            BrowserCommand::Back { tab_id } => {
                if let Ok(route) = BrowserAnnotationRoute::new(workspace_key.clone(), &tab_id) {
                    self.cancel_annotation_route(&route);
                }
                let _ = self.remove_page_recording_view(workspace_key, &tab_id);
                self.evaluate_history(window, workspace_key, &tab_id, "history.back()")?;
                Ok(BrowserResponse::Acknowledged)
            }
            BrowserCommand::Forward { tab_id } => {
                if let Ok(route) = BrowserAnnotationRoute::new(workspace_key.clone(), &tab_id) {
                    self.cancel_annotation_route(&route);
                }
                let _ = self.remove_page_recording_view(workspace_key, &tab_id);
                self.evaluate_history(window, workspace_key, &tab_id, "history.forward()")?;
                Ok(BrowserResponse::Acknowledged)
            }
            BrowserCommand::Reload { tab_id } => {
                if let Ok(route) = BrowserAnnotationRoute::new(workspace_key.clone(), &tab_id) {
                    self.cancel_annotation_route(&route);
                }
                let _ = self.remove_page_recording_view(workspace_key, &tab_id);
                self.ensure_existing_tab_view(window, workspace_key, &tab_id)?;
                self.view(workspace_key, &tab_id)?
                    .reload()
                    .map_err(view_failure)?;
                Ok(BrowserResponse::Acknowledged)
            }
            BrowserCommand::UpdateViewport { tab_id, viewport } => {
                let changes_revision = self
                    .state
                    .workspace(workspace_key)
                    .and_then(|snapshot| snapshot.tabs.iter().find(|tab| tab.id == tab_id))
                    .is_some_and(|tab| tab.viewport != viewport);
                if changes_revision {
                    let route = BrowserAnnotationRoute::new(workspace_key.clone(), &tab_id)?;
                    self.cancel_annotation_mode(&route);
                }
                let mutation = self
                    .state
                    .update_viewport(workspace_key, &tab_id, viewport)?;
                Ok(BrowserResponse::Workspace { mutation })
            }
            BrowserCommand::OpenDevTools { tab_id } => {
                self.ensure_existing_tab_view(window, workspace_key, &tab_id)?;
                self.view(workspace_key, &tab_id)?.open_devtools();
                Ok(BrowserResponse::Acknowledged)
            }
            BrowserCommand::Stop { tab_id } => {
                if let Some(tab_id) = tab_id {
                    self.ensure_existing_tab_view(window, workspace_key, &tab_id)?;
                    self.view(workspace_key, &tab_id)?
                        .evaluate_script("window.stop()")
                        .map_err(view_failure)?;
                } else {
                    for (key, view) in &self.views {
                        if key.workspace_key == *workspace_key {
                            view.evaluate_script("window.stop()")
                                .map_err(view_failure)?;
                        }
                    }
                }
                Ok(BrowserResponse::Acknowledged)
            }
            BrowserCommand::ResetWorkspace => {
                self.discard_workflow_state(workspace_key);
                self.views
                    .retain(|key, _| key.workspace_key != *workspace_key);
                self.recording_ingresses
                    .retain(|key, _| key.workspace_key != *workspace_key);
                self.state.reset_workspace(workspace_key);
                self.apply_visibility_plan()?;
                Ok(BrowserResponse::Acknowledged)
            }
            BrowserCommand::Status
            | BrowserCommand::DownloadDirectory
            | BrowserCommand::ClearProjectProfile => unreachable!("handled before availability"),
            BrowserCommand::Snapshot { .. }
            | BrowserCommand::SecretType { .. }
            | BrowserCommand::Screenshot { .. }
            | BrowserCommand::Wait { .. }
            | BrowserCommand::Act { .. }
            | BrowserCommand::Console { .. }
            | BrowserCommand::Network { .. }
            | BrowserCommand::Performance { .. }
            | BrowserCommand::Upload { .. }
            | BrowserCommand::Downloads { .. }
            | BrowserCommand::Cdp { .. } => Err(BrowserError::CrashedView {
                message: "browser automation command requires the asynchronous request path"
                    .to_string(),
            }),
        }
    }

    fn ensure_runtime_available(&self) -> Result<(), BrowserError> {
        if self.status.available {
            Ok(())
        } else {
            Err(BrowserError::CrashedView {
                message: self
                    .status
                    .diagnostic
                    .clone()
                    .unwrap_or_else(|| "WebView2 runtime is unavailable".to_string()),
            })
        }
    }

    fn verified_trusted_app_config_dir(&self) -> Result<&Path, BrowserError> {
        let trusted_app_config_dir =
            self.trusted_app_config_dir
                .as_deref()
                .ok_or_else(|| BrowserError::CrashedView {
                    message: "browser storage trust root is unavailable".to_string(),
                })?;
        verify_prepared_storage_root(trusted_app_config_dir, trusted_app_config_dir)?;
        Ok(trusted_app_config_dir)
    }

    fn recording_review_resource_store(
        &self,
        workspace_key: &BrowserWorkspaceKey,
    ) -> Result<BrowserResourceStore, BrowserError> {
        let trusted_root = self
            .verified_trusted_app_config_dir()
            .map_err(|_| recording_resource_unavailable())?;
        BrowserResourceStore::open_verified(
            trusted_root,
            &workspace_key.project_id,
            BrowserResourceLimits::default(),
        )
        .map_err(|_| recording_resource_unavailable())
    }

    fn ensure_selected_view(
        &mut self,
        window: &gpui::Window,
        workspace_key: &BrowserWorkspaceKey,
    ) -> Result<(), BrowserError> {
        let plan = self
            .state
            .selected_view_plan(workspace_key)
            .ok_or_else(missing_workspace)?;
        self.ensure_view(window, workspace_key, &plan.tab_id, &plan.url)
    }

    fn ensure_existing_tab_view(
        &mut self,
        window: &gpui::Window,
        workspace_key: &BrowserWorkspaceKey,
        tab_id: &str,
    ) -> Result<(), BrowserError> {
        let url = self
            .state
            .workspace(workspace_key)
            .and_then(|snapshot| snapshot.tabs.iter().find(|tab| tab.id == tab_id))
            .map(|tab| tab.url.clone())
            .ok_or_else(|| missing_tab(tab_id))?;
        self.ensure_view(window, workspace_key, tab_id, &url)
    }

    fn ensure_view(
        &mut self,
        window: &gpui::Window,
        workspace_key: &BrowserWorkspaceKey,
        tab_id: &str,
        url: &str,
    ) -> Result<(), BrowserError> {
        let key = view_key(workspace_key, tab_id);
        if self.views.contains_key(&key) {
            return Ok(());
        }
        let url = validate_browser_url(url)?;
        let retained_trust_root = self.verified_trusted_app_config_dir()?.to_path_buf();
        let (trusted_app_config_dir, layout) =
            prepare_verified_storage_layout(&retained_trust_root, &workspace_key.project_id)?;
        if trusted_app_config_dir != retained_trust_root {
            return Err(BrowserError::OutsideWorkspace {
                path: retained_trust_root,
            });
        }
        let downloads_dir = layout.downloads_dir.clone();
        self.projects
            .entry(workspace_key.project_id.clone())
            .or_insert_with(|| BrowserProjectRuntime {
                context: WebContext::new(Some(layout.profile_dir.clone())),
            });

        let sender = self.event_sender.clone();
        let recording_ingress = self
            .recording_transport
            .ingress(workspace_key.clone(), tab_id.to_string());
        let callback_workspace = workspace_key.clone();
        let callback_tab = tab_id.to_string();
        let bounds = wry_bounds(self.bounds);
        let webview = {
            let project = self
                .projects
                .get_mut(&workspace_key.project_id)
                .ok_or_else(|| BrowserError::CrashedView {
                    message: "browser project context was not initialized".to_string(),
                })?;
            let builder = configured_builder(
                &mut project.context,
                sender,
                recording_ingress.clone(),
                callback_workspace,
                callback_tab,
                trusted_app_config_dir,
                downloads_dir,
                url,
                bounds,
            );
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                builder.build_as_child(window)
            })) {
                Ok(Ok(webview)) => webview,
                Ok(Err(error)) => return Err(view_failure(error)),
                Err(payload) => {
                    return Err(BrowserError::CrashedView {
                        message: format!(
                            "Wry panicked while creating a child WebView: {}",
                            panic_message(payload)
                        ),
                    })
                }
            }
        };
        attach_permission_handler(
            &webview,
            self.event_sender.clone(),
            workspace_key.clone(),
            tab_id.to_string(),
        )?;
        webview.set_visible(false).map_err(view_failure)?;
        webview
            .set_memory_usage_level(MemoryUsageLevel::Low)
            .map_err(view_failure)?;
        self.recording_ingresses
            .insert(key.clone(), recording_ingress);
        self.views.insert(key, webview);
        if self
            .workflow_coordinator
            .active_instance(workspace_key)
            .is_some()
        {
            self.install_page_recording_view(workspace_key, tab_id)
                .map_err(|_| BrowserError::CrashedView {
                    message: "browser recording instrumentation could not be installed".to_string(),
                })?;
        }
        Ok(())
    }

    fn evaluate_history(
        &mut self,
        window: &gpui::Window,
        workspace_key: &BrowserWorkspaceKey,
        tab_id: &str,
        script: &str,
    ) -> Result<(), BrowserError> {
        self.ensure_existing_tab_view(window, workspace_key, tab_id)?;
        self.view(workspace_key, tab_id)?
            .evaluate_script(script)
            .map_err(view_failure)
    }

    fn view(
        &self,
        workspace_key: &BrowserWorkspaceKey,
        tab_id: &str,
    ) -> Result<&WebView, BrowserError> {
        self.views
            .get(&view_key(workspace_key, tab_id))
            .ok_or_else(|| missing_tab(tab_id))
    }

    fn selected_tab_id(&self, workspace_key: &BrowserWorkspaceKey) -> Option<String> {
        self.state
            .workspace(workspace_key)
            .and_then(|snapshot| snapshot.selected_tab_id.clone())
    }

    fn apply_visibility_plan(&mut self) -> Result<(), BrowserError> {
        let plans = self.state.visibility_plan();
        let mut first_error = None;
        let mut diagnostics = Vec::new();
        for plan in plans {
            let Some(view) = self.views.get(&view_key(&plan.workspace_key, &plan.tab_id)) else {
                continue;
            };
            let result = if plan.visible {
                view.set_bounds(wry_bounds(self.bounds))
                    .and_then(|_| view.set_memory_usage_level(MemoryUsageLevel::Normal))
                    .and_then(|_| view.set_visible(true))
            } else {
                view.set_visible(false)
                    .and_then(|_| view.set_memory_usage_level(MemoryUsageLevel::Low))
            };
            if let Err(error) = result {
                let message = format!("could not update WebView visibility: {error}");
                diagnostics.push((plan.workspace_key, plan.tab_id, message.clone()));
                first_error.get_or_insert_with(|| BrowserError::CrashedView { message });
            }
            debug_assert_eq!(
                plan.memory_target,
                if plan.visible {
                    BrowserMemoryTarget::Normal
                } else {
                    BrowserMemoryTarget::Low
                }
            );
        }
        for (workspace_key, tab_id, message) in diagnostics {
            self.emit_diagnostic(&workspace_key, &tab_id, message);
        }
        first_error.map_or(Ok(()), Err)
    }

    fn clear_project_profile(
        &mut self,
        workspace_key: &BrowserWorkspaceKey,
    ) -> Result<(), BrowserError> {
        let trusted_app_config_dir =
            self.trusted_app_config_dir
                .clone()
                .ok_or_else(|| BrowserError::CrashedView {
                    message: "browser storage trust root is unavailable".to_string(),
                })?;
        let layout = BrowserStorageLayout::new(&trusted_app_config_dir, &workspace_key.project_id);
        let plan = self
            .state
            .profile_clear_plan(workspace_key, &layout.profile_dir)?;

        self.discard_project_page_recordings(&workspace_key.project_id);
        self.views
            .retain(|key, _| key.workspace_key.project_id != workspace_key.project_id);
        self.recording_ingresses
            .retain(|key, _| key.workspace_key.project_id != workspace_key.project_id);
        self.projects.remove(&workspace_key.project_id);
        self.state
            .clear_project_workspaces(&workspace_key.project_id);
        remove_verified_profile(&trusted_app_config_dir, &plan.profile_dir)
    }

    fn emit_diagnostic(&self, workspace_key: &BrowserWorkspaceKey, tab_id: &str, message: String) {
        let _ = self.event_sender.send(BrowserHostEvent::Diagnostic {
            workspace_key: workspace_key.clone(),
            tab_id: tab_id.to_string(),
            level: BrowserDiagnosticLevel::Error,
            message,
        });
    }
}

fn map_page_recording_error(error: BrowserRecordingError) -> BrowserPageRecordingIpcError {
    match error {
        BrowserRecordingError::AlreadyActive => BrowserPageRecordingIpcError::AlreadyActive,
        BrowserRecordingError::StaleInstance | BrowserRecordingError::StaleReservation => {
            BrowserPageRecordingIpcError::Untrusted
        }
        BrowserRecordingError::CapacityExceeded
        | BrowserRecordingError::InvalidAction
        | BrowserRecordingError::InvalidMutation => BrowserPageRecordingIpcError::InvalidEvent,
    }
}

fn recording_ipc_browser_error(error: BrowserPageRecordingIpcError) -> BrowserError {
    match error {
        BrowserPageRecordingIpcError::Unavailable => BrowserError::UnavailablePlatform {
            platform: std::env::consts::OS.to_string(),
        },
        BrowserPageRecordingIpcError::Inactive
        | BrowserPageRecordingIpcError::Untrusted
        | BrowserPageRecordingIpcError::TransportInvalidated => stale_recording_instance(),
        BrowserPageRecordingIpcError::AlreadyActive => BrowserError::InvalidInvocation {
            field: "recording".to_string(),
        },
        _ => BrowserError::CrashedView {
            message: "browser recording host operation failed".to_string(),
        },
    }
}

fn stale_recording_instance() -> BrowserError {
    BrowserError::InvalidRecipe {
        message: "recording instance is not active".to_string(),
    }
}

fn map_agent_recording_error(error: BrowserRecordingError) -> BrowserError {
    BrowserError::CrashedView {
        message: format!("browser workflow capture failed: {error}"),
    }
}

fn page_origin(url: &str) -> Option<String> {
    browser_page_origin_from_url(url)
}

fn random_page_recording_nonce() -> Result<String, BrowserPageRecordingIpcError> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| BrowserPageRecordingIpcError::HostFailure)?;
    let mut nonce = String::with_capacity(32);
    use std::fmt::Write as _;
    for byte in bytes {
        let _ = write!(nonce, "{byte:02x}");
    }
    Ok(nonce)
}

fn attach_permission_handler(
    webview: &WebView,
    event_sender: Sender<BrowserHostEvent>,
    workspace_key: BrowserWorkspaceKey,
    tab_id: String,
) -> Result<(), BrowserError> {
    let controller = webview.controller();
    let core_webview = webview.webview();
    let handler = PermissionRequestedEventHandler::create(Box::new(move |_, args| {
        let Some(args) = args else {
            return Ok(());
        };
        let mut kind = COREWEBVIEW2_PERMISSION_KIND::default();
        let mut uri = PWSTR::null();
        unsafe {
            args.PermissionKind(&mut kind)?;
            args.Uri(&mut uri)?;
        }
        let origin = redact_browser_text(&take_pwstr(uri));
        let permission = permission_name(kind);
        let mut was_visible: BOOL = false.into();
        unsafe {
            let _ = controller.IsVisible(&mut was_visible);
            let _ = controller.SetIsVisible(false);
        }
        let description = format!(
            "Actor: User\nIntent: allow website permission\nRisk: OsPermission\nAction: allow {permission}\nOrigin: {origin}"
        );
        let approved = MessageDialog::new()
            .set_level(MessageLevel::Warning)
            .set_title("Confirm Browser Permission")
            .set_description(description)
            .set_buttons(MessageButtons::YesNo)
            .show()
            == MessageDialogResult::Yes;
        let state = if approved {
            COREWEBVIEW2_PERMISSION_STATE_ALLOW
        } else {
            COREWEBVIEW2_PERMISSION_STATE_DENY
        };
        let result = unsafe { args.SetState(state) };
        unsafe {
            let _ = controller.SetIsVisible(was_visible.as_bool());
        }
        let _ = event_sender.send(BrowserHostEvent::Diagnostic {
            workspace_key: workspace_key.clone(),
            tab_id: tab_id.clone(),
            level: BrowserDiagnosticLevel::Info,
            message: format!(
                "{} browser permission {permission}",
                if approved { "Approved" } else { "Denied" }
            ),
        });
        result
    }));
    let mut token = 0_i64;
    unsafe {
        core_webview
            .add_PermissionRequested(&handler, &mut token)
            .map_err(view_failure)
    }
}

fn permission_name(kind: COREWEBVIEW2_PERMISSION_KIND) -> &'static str {
    match kind {
        COREWEBVIEW2_PERMISSION_KIND_CAMERA => "camera",
        COREWEBVIEW2_PERMISSION_KIND_MICROPHONE => "microphone",
        COREWEBVIEW2_PERMISSION_KIND_GEOLOCATION => "geolocation",
        COREWEBVIEW2_PERMISSION_KIND_NOTIFICATIONS => "notifications",
        COREWEBVIEW2_PERMISSION_KIND_CLIPBOARD_READ => "clipboard read",
        COREWEBVIEW2_PERMISSION_KIND_FILE_READ_WRITE => "file read/write",
        _ => "operating-system capability",
    }
}

fn configured_builder<'a>(
    context: &'a mut WebContext,
    event_sender: Sender<BrowserHostEvent>,
    recording_ingress: BrowserPageRecordingIngress,
    workspace_key: BrowserWorkspaceKey,
    tab_id: String,
    trusted_app_config_dir: PathBuf,
    downloads_dir: PathBuf,
    url: String,
    bounds: Rect,
) -> WebViewBuilder<'a> {
    let navigation_sender = event_sender.clone();
    let navigation_workspace = workspace_key.clone();
    let navigation_tab = tab_id.clone();
    let title_sender = event_sender.clone();
    let title_workspace = workspace_key.clone();
    let title_tab = tab_id.clone();
    let load_sender = event_sender.clone();
    let load_workspace = workspace_key.clone();
    let load_tab = tab_id.clone();
    let ipc_sender = event_sender.clone();
    let ipc_failure_sender = event_sender.clone();
    let ipc_recording_ingress = recording_ingress;
    let ipc_workspace = workspace_key.clone();
    let ipc_tab = tab_id.clone();
    let window_sender = event_sender.clone();
    let window_workspace = workspace_key.clone();
    let window_tab = tab_id.clone();
    let download_sender = event_sender.clone();
    let download_workspace = workspace_key.clone();
    let download_tab = tab_id.clone();
    let completion_workspace = workspace_key;
    let completion_tab = tab_id;
    let completion_downloads_dir = downloads_dir.clone();

    WebViewBuilder::new_with_web_context(context)
        .with_url(url)
        .with_bounds(bounds)
        .with_visible(false)
        .with_focused(false)
        .with_clipboard(true)
        .with_initialization_script(browser_user_input_initialization_script())
        .with_navigation_handler(move |url| match validate_browser_url(&url) {
            Ok(_) => {
                let _ = navigation_sender.send(BrowserHostEvent::UrlChanged {
                    workspace_key: navigation_workspace.clone(),
                    tab_id: navigation_tab.clone(),
                    url,
                });
                true
            }
            Err(error) => {
                let _ = navigation_sender.send(BrowserHostEvent::Diagnostic {
                    workspace_key: navigation_workspace.clone(),
                    tab_id: navigation_tab.clone(),
                    level: BrowserDiagnosticLevel::Warning,
                    message: error.to_string(),
                });
                false
            }
        })
        .with_document_title_changed_handler(move |title| {
            let _ = title_sender.send(BrowserHostEvent::TitleChanged {
                workspace_key: title_workspace.clone(),
                tab_id: title_tab.clone(),
                title,
            });
        })
        .with_on_page_load_handler(move |state, url| {
            let state = match state {
                PageLoadEvent::Started => BrowserPageLoadState::Started,
                PageLoadEvent::Finished => BrowserPageLoadState::Finished,
            };
            let _ = load_sender.send(BrowserHostEvent::PageLoad {
                workspace_key: load_workspace.clone(),
                tab_id: load_tab.clone(),
                state,
                url,
            });
        })
        .with_ipc_handler(move |request| {
            let body = request.body();
            if BrowserPageRecordingEnvelope::parse(body).is_ok() {
                let observed_origin = request
                    .uri()
                    .scheme_str()
                    .zip(request.uri().authority())
                    .map(|(scheme, authority)| format!("{scheme}://{}", authority.as_str()))
                    .unwrap_or_default();
                let submitted = ipc_recording_ingress.submit(&observed_origin, body.to_string());
                if matches!(
                    submitted,
                    BrowserPageRecordingSubmit::Overflow
                        | BrowserPageRecordingSubmit::Disconnected
                ) {
                    let _ = ipc_failure_sender.send(BrowserHostEvent::Diagnostic {
                        workspace_key: ipc_workspace.clone(),
                        tab_id: ipc_tab.clone(),
                        level: BrowserDiagnosticLevel::Error,
                        message: "browser recording transport failed; the incomplete recording will be discarded"
                            .to_string(),
                    });
                }
                return;
            }
            let event = match parse_browser_page_ipc_message(body) {
                Ok(BrowserPageIpcMessage::UserInput { kind }) => BrowserHostEvent::UserInput {
                    workspace_key: ipc_workspace.clone(),
                    tab_id: ipc_tab.clone(),
                    kind,
                },
                Ok(BrowserPageIpcMessage::DomMutation) => BrowserHostEvent::DomMutation {
                    workspace_key: ipc_workspace.clone(),
                    tab_id: ipc_tab.clone(),
                },
                Ok(BrowserPageIpcMessage::AnnotationCandidate { candidate }) => {
                    BrowserHostEvent::AnnotationCandidate {
                        workspace_key: ipc_workspace.clone(),
                        tab_id: ipc_tab.clone(),
                        candidate,
                    }
                }
                Ok(BrowserPageIpcMessage::AnnotationCanceled) => {
                    BrowserHostEvent::AnnotationCanceled {
                        workspace_key: ipc_workspace.clone(),
                        tab_id: ipc_tab.clone(),
                    }
                }
                Err(_) => BrowserHostEvent::Diagnostic {
                    workspace_key: ipc_workspace.clone(),
                    tab_id: ipc_tab.clone(),
                    level: BrowserDiagnosticLevel::Warning,
                    message: "ignored malformed or oversized browser input metadata".to_string(),
                },
            };
            let _ = ipc_sender.send(event);
        })
        .with_new_window_req_handler(move |url, _features| {
            let _ = window_sender.send(BrowserHostEvent::NewWindow {
                workspace_key: window_workspace.clone(),
                tab_id: window_tab.clone(),
                url,
            });
            NewWindowResponse::Deny
        })
        .with_download_started_handler(move |url, suggested_path| {
            match verified_unique_download_path(
                &trusted_app_config_dir,
                &downloads_dir,
                &*suggested_path,
            ) {
                Ok(path) => {
                    *suggested_path = path.clone();
                    let _ = download_sender.send(BrowserHostEvent::Download {
                        workspace_key: download_workspace.clone(),
                        tab_id: download_tab.clone(),
                        state: BrowserDownloadState::Started,
                        url,
                        path,
                    });
                    true
                }
                Err(error) => {
                    let _ = download_sender.send(BrowserHostEvent::Diagnostic {
                        workspace_key: download_workspace.clone(),
                        tab_id: download_tab.clone(),
                        level: BrowserDiagnosticLevel::Error,
                        message: error.to_string(),
                    });
                    false
                }
            }
        })
        .with_download_completed_handler(move |url, path, successful| {
            let _ = event_sender.send(BrowserHostEvent::Download {
                workspace_key: completion_workspace.clone(),
                tab_id: completion_tab.clone(),
                state: BrowserDownloadState::Completed { successful },
                url,
                path: path.unwrap_or_else(|| completion_downloads_dir.clone()),
            });
        })
}

fn view_key(workspace_key: &BrowserWorkspaceKey, tab_id: &str) -> BrowserViewKey {
    BrowserViewKey {
        workspace_key: workspace_key.clone(),
        tab_id: tab_id.to_string(),
    }
}

fn browser_command_is_automation(command: &BrowserCommand) -> bool {
    matches!(
        command,
        BrowserCommand::Snapshot { .. }
            | BrowserCommand::SecretType { .. }
            | BrowserCommand::Screenshot { .. }
            | BrowserCommand::Wait { .. }
            | BrowserCommand::Act { .. }
            | BrowserCommand::Console { .. }
            | BrowserCommand::Network { .. }
            | BrowserCommand::Performance { .. }
            | BrowserCommand::Upload { .. }
            | BrowserCommand::Downloads { .. }
            | BrowserCommand::Cdp { .. }
    )
}

fn required_annotation_id(annotation_id: Option<String>) -> Result<String, BrowserError> {
    annotation_id
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| BrowserError::InvalidInvocation {
            field: "annotationId".to_string(),
        })
}

fn browser_command_is_journaled(command: &BrowserCommand) -> bool {
    !matches!(
        command,
        BrowserCommand::Ensure { .. }
            | BrowserCommand::SetPaneOpen { .. }
            | BrowserCommand::WorkspaceState
    )
}

fn browser_error_code(error: &BrowserError) -> &'static str {
    match error {
        BrowserError::InvalidWorkspaceKey { .. } => "invalid_workspace_key",
        BrowserError::InvalidInvocation { .. } => "invalid_request",
        BrowserError::InvalidAnnotation { .. } => "invalid_annotation",
        BrowserError::MissingAnnotation { .. } => "missing_annotation",
        BrowserError::StaleReference { .. } => "stale_reference",
        BrowserError::MissingFile { .. } => "missing_file",
        BrowserError::MissingResource { .. } => "missing_resource",
        BrowserError::ResourceTooLarge { .. } => "resource_too_large",
        BrowserError::OutsideWorkspace { .. } => "outside_workspace_file",
        BrowserError::InvalidRecipe { .. } | BrowserError::UnsupportedRecipeVersion { .. } => {
            "invalid_recipe"
        }
        BrowserError::RecordingResourceUnavailable => "recording_resource_unavailable",
        BrowserError::Interrupted => "user_interrupted",
        BrowserError::Timeout { .. } => "timeout",
        BrowserError::NavigationFailure { .. } => "navigation_failure",
        BrowserError::CrashedView { .. } => "crashed_view",
        BrowserError::BlockedPermission { .. } => "blocked_permission",
        BrowserError::UnavailablePlatform { .. } => "unavailable_platform",
        BrowserError::Io { .. } => "io_error",
    }
}

fn replace_annotation_response_mutation(
    response: &mut BrowserResponse,
    mutation: crate::browser::BrowserWorkspaceMutation,
) {
    match response {
        BrowserResponse::Annotations {
            mutation: current, ..
        }
        | BrowserResponse::Annotation {
            mutation: current, ..
        } => *current = mutation,
        BrowserResponse::AnnotationMutation { result } => result.mutation = mutation,
        _ => {}
    }
}

fn browser_command_summary(command: &BrowserCommand) -> String {
    match command {
        BrowserCommand::Status => "inspect browser status".to_string(),
        BrowserCommand::WorkspaceState => "inspect browser workspace".to_string(),
        BrowserCommand::Ensure { .. } => "initialize browser workspace".to_string(),
        BrowserCommand::SetPaneOpen { open } => format!("set browser pane open to {open}"),
        BrowserCommand::SetAnnotationMode { enabled, .. } => {
            format!("set browser annotation mode to {enabled}")
        }
        BrowserCommand::CaptureAnnotation { .. } => "capture browser annotation".to_string(),
        BrowserCommand::SaveAnnotationDraft { .. } => "save browser annotation".to_string(),
        BrowserCommand::CancelAnnotationDraft { .. } => "cancel browser annotation".to_string(),
        BrowserCommand::Annotations { operation, .. } => {
            format!("browser annotations {operation:?}").to_ascii_lowercase()
        }
        BrowserCommand::Recording { operation } => {
            format!("browser recording {operation:?}").to_ascii_lowercase()
        }
        BrowserCommand::ListTabs => "list browser tabs".to_string(),
        BrowserCommand::CreateTab { .. } => "create browser tab".to_string(),
        BrowserCommand::SelectTab { .. } => "select browser tab".to_string(),
        BrowserCommand::CloseTab { .. } => "close browser tab".to_string(),
        BrowserCommand::Navigate { url, .. } => {
            format!("navigate to {}", redact_browser_text(url))
        }
        BrowserCommand::Back { .. } => "navigate back".to_string(),
        BrowserCommand::Forward { .. } => "navigate forward".to_string(),
        BrowserCommand::Reload { .. } => "reload browser tab".to_string(),
        BrowserCommand::UpdateViewport { .. } => "update browser viewport".to_string(),
        BrowserCommand::OpenDevTools { .. } => "open browser devtools".to_string(),
        BrowserCommand::Stop { .. } => "stop browser activity".to_string(),
        BrowserCommand::ResetWorkspace => "reset browser workspace".to_string(),
        BrowserCommand::ClearProjectProfile => "clear browser profile".to_string(),
        BrowserCommand::DownloadDirectory => "open browser downloads".to_string(),
        BrowserCommand::SecretType { .. } => "type secret input".to_string(),
        BrowserCommand::Snapshot { .. } => "capture semantic snapshot".to_string(),
        BrowserCommand::Screenshot { .. } => "capture page screenshot".to_string(),
        BrowserCommand::Wait { .. } => "wait for page condition".to_string(),
        BrowserCommand::Act { actions, .. } => actions
            .iter()
            .map(BrowserAction::redacted_summary)
            .collect::<Vec<_>>()
            .join(", "),
        BrowserCommand::Console { operation, .. } => {
            format!("browser console {operation:?}").to_ascii_lowercase()
        }
        BrowserCommand::Network { operation, .. } => {
            format!("browser network {operation:?}").to_ascii_lowercase()
        }
        BrowserCommand::Performance { operation, .. } => {
            format!("browser performance {operation:?}").to_ascii_lowercase()
        }
        BrowserCommand::Upload { paths, .. } => format!("upload {} file(s)", paths.len()),
        BrowserCommand::Downloads { operation, .. } => {
            format!("browser downloads {operation:?}").to_ascii_lowercase()
        }
        BrowserCommand::Cdp { method, .. } => {
            format!("call browser CDP method {}", redact_browser_text(method))
        }
    }
}

fn start_result(result: Result<(), BrowserError>, phase: BrowserAsyncPhase) -> BrowserStartResult {
    match result {
        Ok(()) => BrowserStartResult::Pending(phase),
        Err(error) => BrowserStartResult::Complete(Err(error)),
    }
}

fn script_value(raw: &str) -> Result<Value, BrowserError> {
    let envelope: BrowserScriptEnvelope =
        serde_json::from_str(raw).map_err(|_| BrowserError::CrashedView {
            message: "browser automation returned an invalid response".to_string(),
        })?;
    if envelope.ok {
        envelope.value.ok_or_else(|| BrowserError::CrashedView {
            message: "browser automation returned no value".to_string(),
        })
    } else {
        Err(BrowserError::CrashedView {
            message: envelope
                .error
                .unwrap_or_else(|| "automation_failed".to_string()),
        })
    }
}

fn decode_screenshot_png(raw: &str) -> Result<Vec<u8>, BrowserError> {
    let value: Value = serde_json::from_str(raw).map_err(|_| BrowserError::CrashedView {
        message: "browser screenshot callback returned invalid data".to_string(),
    })?;
    let data =
        value
            .get("data")
            .and_then(Value::as_str)
            .ok_or_else(|| BrowserError::CrashedView {
                message: "browser screenshot callback omitted PNG data".to_string(),
            })?;
    base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|_| BrowserError::CrashedView {
            message: "browser screenshot callback returned invalid PNG data".to_string(),
        })
}

fn random_annotation_capture_id() -> Result<String, BrowserError> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|error| BrowserError::CrashedView {
        message: format!("could not generate annotation capture id: {error}"),
    })?;
    let mut id = String::from("capture-");
    use std::fmt::Write as _;
    for byte in bytes {
        let _ = write!(id, "{byte:02x}");
    }
    Ok(id)
}

fn wry_bounds(bounds: BrowserBounds) -> Rect {
    Rect {
        position: LogicalPosition::new(bounds.x, bounds.y).into(),
        size: LogicalSize::new(bounds.width.max(1), bounds.height.max(1)).into(),
    }
}

fn absolute_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    }
}

fn missing_workspace() -> BrowserError {
    BrowserError::CrashedView {
        message: "browser workspace has not been ensured".to_string(),
    }
}

fn missing_tab(tab_id: &str) -> BrowserError {
    BrowserError::CrashedView {
        message: format!("browser tab {tab_id:?} does not exist"),
    }
}

fn view_failure(error: impl std::fmt::Display) -> BrowserError {
    BrowserError::CrashedView {
        message: error.to_string(),
    }
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "unknown panic payload".to_string()
    }
}
