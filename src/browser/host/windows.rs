use super::{
    browser_user_input_initialization_script, validate_browser_url, BrowserHostState,
    BrowserMemoryTarget,
};
use crate::browser::downloads::{
    prepare_verified_storage_layout, verified_app_config_root, verified_unique_download_path,
    verify_prepared_storage_root,
};
use crate::browser::{
    browser_lifecycle_control, browser_request_preempts_operation_queue, build_semantic_snapshot,
    effective_browser_risk, effective_browser_risk_for_targets, prepare_verified_download_root,
    redact_browser_resource_bytes, redact_browser_text, remove_verified_profile, BrowserAction,
    BrowserActionResult, BrowserApprovalPolicy, BrowserApprovalRequest, BrowserBounds,
    BrowserCommand, BrowserCommandRequest, BrowserConsoleEntry, BrowserConsoleOperation,
    BrowserDiagnosticLevel, BrowserDownloadState, BrowserDownloadStore, BrowserError,
    BrowserHostControl, BrowserHostEvent, BrowserHostStatus, BrowserInvocationActor,
    BrowserJournalActor, BrowserJournalEntry, BrowserNetworkEntry, BrowserNetworkOperation,
    BrowserOperationQueue, BrowserOperationTarget, BrowserPageLoadState,
    BrowserPerformanceOperation, BrowserPerformanceSnapshot, BrowserRawSemanticElement,
    BrowserResourceHandle, BrowserResourceId, BrowserResourceKind, BrowserResourceLimits,
    BrowserResourceStore, BrowserResponse, BrowserRuntimeTarget, BrowserScreenshotMode,
    BrowserSnapshotSummary, BrowserStorageLayout, BrowserUploadResult, BrowserUserInputKind,
    BrowserWaitResult, BrowserWorkspaceKey, BrowserWorkspaceSnapshot, MAX_BROWSER_ACTIONS,
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

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct BrowserViewKey {
    workspace_key: BrowserWorkspaceKey,
    tab_id: String,
}

const WORKSPACE_OPERATION_TAB: &str = "__workspace__";
const INLINE_RESULT_LIMIT: usize = 8 * 1024;

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
    Act {
        mutating: bool,
    },
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
    Actions(Vec<BrowserAction>),
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
    operation_queue: BrowserOperationQueue<BrowserCommandRequest>,
    active_requests: HashMap<BrowserOperationTarget, ActiveBrowserRequest>,
    async_sender: Sender<BrowserAsyncCompletion>,
    async_receiver: Receiver<BrowserAsyncCompletion>,
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
            operation_queue: BrowserOperationQueue::default(),
            active_requests: HashMap::new(),
            async_sender,
            async_receiver,
            _main_thread_only: PhantomData,
        }
    }

    pub fn status(&self) -> BrowserHostStatus {
        self.status.clone()
    }

    pub fn trusted_app_config_dir(&self) -> Option<&Path> {
        self.trusted_app_config_dir.as_deref()
    }

    pub fn handle_command(
        &mut self,
        window: &gpui::Window,
        workspace_key: &BrowserWorkspaceKey,
        command: BrowserCommand,
    ) -> Result<BrowserResponse, BrowserError> {
        if let Some(control) = browser_lifecycle_control(workspace_key, &command) {
            self.handle_control(control);
        }
        let diagnostic_tab = command
            .tab_id()
            .map(ToOwned::to_owned)
            .or_else(|| self.selected_tab_id(workspace_key));
        let result = self.handle_command_inner(window, workspace_key, command);
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
                self.cancel_project_operations(&project_id);
            }
            BrowserHostControl::InterruptWorkspace { workspace_key } => {
                self.cancel_workspace_operations(&workspace_key);
            }
            BrowserHostControl::InterruptTab {
                workspace_key,
                tab_id,
            } => self.cancel_tab_operations(&workspace_key, &tab_id),
        }
    }

    pub fn handle_request(&mut self, window: &gpui::Window, request: BrowserCommandRequest) {
        if !request.cancellation_is_current() {
            request.respond(Err(BrowserError::Interrupted));
            return;
        }
        let workspace_key = request.workspace_key().clone();
        let command = request.command().clone();
        if request.context().actor != BrowserInvocationActor::Agent
            || browser_request_preempts_operation_queue(&command)
        {
            let result = self.handle_command(window, &workspace_key, command);
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
    }

    fn operation_target(
        &self,
        workspace_key: &BrowserWorkspaceKey,
        command: &BrowserCommand,
    ) -> BrowserOperationTarget {
        let tab_id = command
            .tab_id()
            .map(ToOwned::to_owned)
            .or_else(|| self.selected_tab_id(workspace_key))
            .unwrap_or_else(|| WORKSPACE_OPERATION_TAB.to_string());
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
        let workspace_key = request.workspace_key().clone();
        let command = request.command().clone();
        let result = self.handle_command(window, &workspace_key, command);
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
        result: Result<BrowserResponse, BrowserError>,
    ) {
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
        if request.context().actor == BrowserInvocationActor::Agent
            && browser_command_is_journaled(request.command())
        {
            let workspace_key = request.workspace_key().clone();
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
            if self
                .state
                .append_journal_entry(&workspace_key, entry)
                .is_ok()
            {
                if let Some(tab_id) = tab_id {
                    let _ = self
                        .event_sender
                        .send(BrowserHostEvent::AutomationStateChanged {
                            workspace_key,
                            tab_id,
                        });
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
        let path_risk = matches!(
            command,
            BrowserCommand::Downloads {
                operation: crate::browser::BrowserDownloadOperation::Delete,
                ..
            }
        )
        .then_some(crate::browser::BrowserRisk::Destructive);
        let initial_risk = effective_browser_risk(request.context().declared_risk, None, path_risk);
        if !matches!(command, BrowserCommand::Act { .. })
            && BrowserApprovalPolicy::trust_project().requires_confirmation(initial_risk)
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
                let timeout_ms = (*timeout_ms).clamp(1, 60_000);
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
            BrowserAsyncPhase::Approval { .. } => return,
            BrowserAsyncPhase::Act { mutating } => {
                self.complete_action(&active.request, &raw, mutating)
            }
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
            BrowserApprovalResume::Actions(actions) => {
                self.continue_actions(window, target, operation_id.to_string(), active, actions)
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
                operation: "wait".to_string(),
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

    fn validate_wait_reference(
        &self,
        workspace_key: &BrowserWorkspaceKey,
        condition: &crate::browser::BrowserWaitCondition,
    ) -> Result<(), BrowserError> {
        use crate::browser::BrowserWaitCondition;
        let target = match condition {
            BrowserWaitCondition::ElementPresent { target }
            | BrowserWaitCondition::ElementVisible { target }
            | BrowserWaitCondition::ElementHidden { target } => Some(target),
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
        let events: Vec<_> = self.event_receiver.try_iter().collect();
        for event in &events {
            match event {
                BrowserHostEvent::UrlChanged {
                    workspace_key,
                    tab_id,
                    url,
                } => {
                    let _ = self.state.navigate_tab(workspace_key, tab_id, url);
                }
                BrowserHostEvent::TitleChanged {
                    workspace_key,
                    tab_id,
                    title,
                } => {
                    let _ = self.state.apply_title_change(workspace_key, tab_id, title);
                }
                BrowserHostEvent::PageLoad {
                    workspace_key,
                    tab_id,
                    state: BrowserPageLoadState::Finished,
                    url,
                } => {
                    let _ = self.state.apply_page_load(workspace_key, tab_id, url);
                }
                BrowserHostEvent::UserInput {
                    workspace_key,
                    tab_id,
                    ..
                } => {
                    self.cancel_tab_operations(workspace_key, tab_id);
                    let _ = self.state.apply_user_input(workspace_key, tab_id);
                }
                BrowserHostEvent::DomMutation {
                    workspace_key,
                    tab_id,
                } => {
                    let _ = self.state.apply_dom_mutation(workspace_key, tab_id);
                }
                BrowserHostEvent::AutomationStateChanged { .. } => {}
                BrowserHostEvent::ApprovalRequested { .. } => {}
                BrowserHostEvent::PageLoad { .. }
                | BrowserHostEvent::NewWindow { .. }
                | BrowserHostEvent::Download { .. }
                | BrowserHostEvent::Diagnostic { .. } => {}
            }
        }
        events
    }

    pub fn workspace_snapshot(
        &self,
        workspace_key: &BrowserWorkspaceKey,
    ) -> Option<&BrowserWorkspaceSnapshot> {
        self.state.workspace(workspace_key)
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
                self.ensure_selected_view(window, workspace_key)?;
                self.apply_visibility_plan()?;
                Ok(BrowserResponse::Workspace { mutation })
            }
            BrowserCommand::SetPaneOpen { open } => {
                let mutation = self.state.set_pane_open(workspace_key, open)?;
                self.apply_visibility_plan()?;
                Ok(BrowserResponse::Workspace { mutation })
            }
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
                let mutation = self
                    .state
                    .create_tab(workspace_key, url.as_deref().unwrap_or("about:blank"))?;
                self.ensure_selected_view(window, workspace_key)?;
                self.apply_visibility_plan()?;
                Ok(BrowserResponse::Workspace { mutation })
            }
            BrowserCommand::SelectTab { tab_id } => {
                let mutation = self.state.select_tab(workspace_key, &tab_id)?;
                self.ensure_selected_view(window, workspace_key)?;
                self.apply_visibility_plan()?;
                Ok(BrowserResponse::Workspace { mutation })
            }
            BrowserCommand::CloseTab { tab_id } => {
                let key = view_key(workspace_key, &tab_id);
                self.views.remove(&key);
                let mutation = self.state.close_tab(workspace_key, &tab_id)?;
                self.ensure_selected_view(window, workspace_key)?;
                self.apply_visibility_plan()?;
                Ok(BrowserResponse::Workspace { mutation })
            }
            BrowserCommand::Navigate { tab_id, url } => {
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
                self.evaluate_history(window, workspace_key, &tab_id, "history.back()")?;
                Ok(BrowserResponse::Acknowledged)
            }
            BrowserCommand::Forward { tab_id } => {
                self.evaluate_history(window, workspace_key, &tab_id, "history.forward()")?;
                Ok(BrowserResponse::Acknowledged)
            }
            BrowserCommand::Reload { tab_id } => {
                self.ensure_existing_tab_view(window, workspace_key, &tab_id)?;
                self.view(workspace_key, &tab_id)?
                    .reload()
                    .map_err(view_failure)?;
                Ok(BrowserResponse::Acknowledged)
            }
            BrowserCommand::UpdateViewport { tab_id, viewport } => {
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
                self.views
                    .retain(|key, _| key.workspace_key != *workspace_key);
                self.state.reset_workspace(workspace_key);
                self.apply_visibility_plan()?;
                Ok(BrowserResponse::Acknowledged)
            }
            BrowserCommand::Status
            | BrowserCommand::DownloadDirectory
            | BrowserCommand::ClearProjectProfile => unreachable!("handled before availability"),
            BrowserCommand::Snapshot { .. }
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
        self.views.insert(key, webview);
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

        self.views
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
            let event = match serde_json::from_str::<BrowserInputMessage>(request.body()) {
                Ok(BrowserInputMessage::UserInput { kind }) => BrowserHostEvent::UserInput {
                    workspace_key: ipc_workspace.clone(),
                    tab_id: ipc_tab.clone(),
                    kind,
                },
                Ok(BrowserInputMessage::DomMutation) => BrowserHostEvent::DomMutation {
                    workspace_key: ipc_workspace.clone(),
                    tab_id: ipc_tab.clone(),
                },
                Err(_) => BrowserHostEvent::Diagnostic {
                    workspace_key: ipc_workspace.clone(),
                    tab_id: ipc_tab.clone(),
                    level: BrowserDiagnosticLevel::Warning,
                    message: "ignored malformed browser input metadata".to_string(),
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

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", deny_unknown_fields)]
enum BrowserInputMessage {
    UserInput { kind: BrowserUserInputKind },
    DomMutation,
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
        BrowserError::Interrupted => "user_interrupted",
        BrowserError::Timeout { .. } => "timeout",
        BrowserError::NavigationFailure { .. } => "navigation_failure",
        BrowserError::CrashedView { .. } => "crashed_view",
        BrowserError::BlockedPermission { .. } => "blocked_permission",
        BrowserError::UnavailablePlatform { .. } => "unavailable_platform",
        BrowserError::Io { .. } => "io_error",
    }
}

fn browser_response_resource_ids(response: &BrowserResponse) -> Vec<BrowserResourceId> {
    let handle = match response {
        BrowserResponse::Snapshot { resource, .. } | BrowserResponse::Screenshot { resource } => {
            Some(resource)
        }
        BrowserResponse::Console { resource, .. }
        | BrowserResponse::Network { resource, .. }
        | BrowserResponse::Performance { resource, .. }
        | BrowserResponse::Cdp { resource, .. } => resource.as_ref(),
        BrowserResponse::Status { .. }
        | BrowserResponse::WorkspaceState { .. }
        | BrowserResponse::Workspace { .. }
        | BrowserResponse::Tabs { .. }
        | BrowserResponse::DownloadDirectory { .. }
        | BrowserResponse::Wait { .. }
        | BrowserResponse::Action { .. }
        | BrowserResponse::Upload { .. }
        | BrowserResponse::Downloads { .. }
        | BrowserResponse::Acknowledged => None,
    };
    handle
        .map(|resource| vec![resource.id.clone()])
        .unwrap_or_default()
}

fn browser_command_summary(command: &BrowserCommand) -> String {
    match command {
        BrowserCommand::Status => "inspect browser status".to_string(),
        BrowserCommand::WorkspaceState => "inspect browser workspace".to_string(),
        BrowserCommand::Ensure { .. } => "initialize browser workspace".to_string(),
        BrowserCommand::SetPaneOpen { open } => format!("set browser pane open to {open}"),
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
