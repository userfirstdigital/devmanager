use super::{
    BrowserAction, BrowserActionResult, BrowserActionTarget, BrowserAnnotationCandidate,
    BrowserAnnotationDetails, BrowserAnnotationDraft, BrowserAnnotationMutationResult,
    BrowserAnnotationOperation, BrowserAnnotationSummary, BrowserConsoleEntry,
    BrowserConsoleOperation, BrowserDownloadEntry, BrowserDownloadOperation, BrowserError,
    BrowserNetworkEntry, BrowserNetworkOperation, BrowserPerformanceOperation,
    BrowserPerformanceSnapshot, BrowserRecipeInputKind, BrowserRecordingStatus,
    BrowserReplaySecretLease, BrowserResourceHandle, BrowserResourceId, BrowserRisk,
    BrowserScreenshotMode, BrowserSnapshotSummary, BrowserTabSnapshot, BrowserUploadResult,
    BrowserViewport, BrowserWaitCondition, BrowserWaitResult, BrowserWorkspaceKey,
    BrowserWorkspaceMutation, BrowserWorkspaceSnapshot,
};
use rmcp::schemars;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::fmt::Write as _;
use std::marker::PhantomData;
#[cfg(windows)]
use std::path::{Component, Prefix};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use tokio::sync::{mpsc, oneshot, watch};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum BrowserInvocationActor {
    User,
    Agent,
    Internal,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BrowserInvocationContext {
    pub actor: BrowserInvocationActor,
    pub intent: String,
    pub declared_risk: BrowserRisk,
    pub operation_id: String,
}

impl BrowserInvocationContext {
    pub fn new(
        actor: BrowserInvocationActor,
        intent: impl Into<String>,
        declared_risk: BrowserRisk,
        operation_id: impl Into<String>,
    ) -> Result<Self, BrowserError> {
        let context = Self {
            actor,
            intent: intent.into(),
            declared_risk,
            operation_id: operation_id.into(),
        };
        context.validate()?;
        Ok(context)
    }

    pub fn agent(
        intent: impl Into<String>,
        declared_risk: BrowserRisk,
    ) -> Result<Self, BrowserError> {
        Self::new(
            BrowserInvocationActor::Agent,
            intent,
            declared_risk,
            random_operation_id()?,
        )
    }

    pub fn for_actor(
        actor: BrowserInvocationActor,
        intent: impl Into<String>,
        declared_risk: BrowserRisk,
    ) -> Result<Self, BrowserError> {
        Self::new(actor, intent, declared_risk, random_operation_id()?)
    }

    pub fn user(
        intent: impl Into<String>,
        declared_risk: BrowserRisk,
    ) -> Result<Self, BrowserError> {
        Self::new(
            BrowserInvocationActor::User,
            intent,
            declared_risk,
            random_operation_id()?,
        )
    }

    pub fn internal(operation: impl Into<String>) -> Self {
        let operation = operation.into();
        Self {
            actor: BrowserInvocationActor::Internal,
            intent: format!("internal lifecycle: {operation}"),
            declared_risk: BrowserRisk::Normal,
            operation_id: random_operation_id()
                .unwrap_or_else(|_| "internal-operation".to_string()),
        }
    }

    pub fn validate(&self) -> Result<(), BrowserError> {
        if self.intent.trim().is_empty() {
            return Err(BrowserError::InvalidInvocation {
                field: "intent".to_string(),
            });
        }
        if self.operation_id.trim().is_empty() {
            return Err(BrowserError::InvalidInvocation {
                field: "operationId".to_string(),
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BrowserApprovalRequest {
    pub operation_id: String,
    pub actor: BrowserInvocationActor,
    pub intent: String,
    pub effective_risk: BrowserRisk,
    pub action_summary: String,
    pub origin_url: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, rmcp::schemars::JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[schemars(rename_all = "camelCase")]
pub enum BrowserRecordingOperation {
    Status,
    Start,
    Stop,
    Review,
    Discard,
    Save,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BrowserRecordingInputSummary {
    pub name: String,
    pub kind: BrowserRecipeInputKind,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BrowserRecordingResult {
    pub operation: BrowserRecordingOperation,
    pub status: BrowserRecordingStatus,
    pub recording_id: Option<u64>,
    pub recipe_id: Option<String>,
    pub step_count: usize,
    pub inputs: Vec<BrowserRecordingInputSummary>,
    pub valid: bool,
    pub resource: Option<BrowserResourceHandle>,
    pub overwrote_existing: Option<bool>,
}

fn random_operation_id() -> Result<String, BrowserError> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|error| BrowserError::CrashedView {
        message: format!("could not generate browser operation id: {error}"),
    })?;
    let mut id = String::with_capacity(35);
    id.push_str("op-");
    for byte in bytes {
        let _ = write!(id, "{byte:02x}");
    }
    Ok(id)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum BrowserCommand {
    Status,
    WorkspaceState,
    Ensure {
        snapshot: BrowserWorkspaceSnapshot,
    },
    SetPaneOpen {
        open: bool,
    },
    SetAnnotationMode {
        tab_id: String,
        enabled: bool,
    },
    CaptureAnnotation {
        tab_id: String,
        candidate: BrowserAnnotationCandidate,
    },
    SaveAnnotationDraft {
        draft_id: String,
        comment: String,
    },
    CancelAnnotationDraft {
        draft_id: String,
    },
    Annotations {
        operation: BrowserAnnotationOperation,
        annotation_id: Option<String>,
    },
    Recording {
        operation: BrowserRecordingOperation,
    },
    ListTabs,
    CreateTab {
        url: Option<String>,
    },
    SelectTab {
        tab_id: String,
    },
    CloseTab {
        tab_id: String,
    },
    Navigate {
        tab_id: String,
        url: String,
    },
    Back {
        tab_id: String,
    },
    Forward {
        tab_id: String,
    },
    Reload {
        tab_id: String,
    },
    UpdateViewport {
        tab_id: String,
        viewport: BrowserViewport,
    },
    OpenDevTools {
        tab_id: String,
    },
    Stop {
        tab_id: Option<String>,
    },
    ResetWorkspace,
    ClearProjectProfile,
    DownloadDirectory,
    SecretType {
        tab_id: String,
        target: BrowserActionTarget,
        input_name: String,
    },
    Snapshot {
        tab_id: String,
    },
    Screenshot {
        tab_id: String,
        mode: BrowserScreenshotMode,
    },
    Wait {
        tab_id: String,
        condition: BrowserWaitCondition,
        timeout_ms: u64,
    },
    Act {
        tab_id: String,
        actions: Vec<BrowserAction>,
    },
    Console {
        tab_id: String,
        operation: BrowserConsoleOperation,
    },
    Network {
        tab_id: String,
        operation: BrowserNetworkOperation,
        request_id: Option<String>,
    },
    Performance {
        tab_id: String,
        operation: BrowserPerformanceOperation,
    },
    Upload {
        tab_id: String,
        target: BrowserActionTarget,
        paths: Vec<PathBuf>,
    },
    Downloads {
        tab_id: String,
        operation: BrowserDownloadOperation,
        download_id: Option<String>,
    },
    Cdp {
        tab_id: String,
        method: String,
        params: serde_json::Value,
    },
}

impl BrowserCommand {
    fn operation_name(&self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::WorkspaceState => "workspaceState",
            Self::Ensure { .. } => "ensure",
            Self::SetPaneOpen { .. } => "setPaneOpen",
            Self::SetAnnotationMode { .. } => "setAnnotationMode",
            Self::CaptureAnnotation { .. } => "captureAnnotation",
            Self::SaveAnnotationDraft { .. } => "saveAnnotationDraft",
            Self::CancelAnnotationDraft { .. } => "cancelAnnotationDraft",
            Self::Annotations { .. } => "annotations",
            Self::Recording { .. } => "recording",
            Self::ListTabs => "listTabs",
            Self::CreateTab { .. } => "createTab",
            Self::SelectTab { .. } => "selectTab",
            Self::CloseTab { .. } => "closeTab",
            Self::Navigate { .. } => "navigate",
            Self::Back { .. } => "back",
            Self::Forward { .. } => "forward",
            Self::Reload { .. } => "reload",
            Self::UpdateViewport { .. } => "updateViewport",
            Self::OpenDevTools { .. } => "openDevTools",
            Self::Stop { .. } => "stop",
            Self::ResetWorkspace => "resetWorkspace",
            Self::ClearProjectProfile => "clearProjectProfile",
            Self::DownloadDirectory => "downloadDirectory",
            Self::SecretType { .. } => "secretType",
            Self::Snapshot { .. } => "snapshot",
            Self::Screenshot { .. } => "screenshot",
            Self::Wait { .. } => "wait",
            Self::Act { .. } => "act",
            Self::Console { .. } => "console",
            Self::Network { .. } => "network",
            Self::Performance { .. } => "performance",
            Self::Upload { .. } => "upload",
            Self::Downloads { .. } => "downloads",
            Self::Cdp { .. } => "cdp",
        }
    }

    pub(crate) fn tab_id(&self) -> Option<&str> {
        match self {
            Self::SelectTab { tab_id }
            | Self::SetAnnotationMode { tab_id, .. }
            | Self::CaptureAnnotation { tab_id, .. }
            | Self::CloseTab { tab_id }
            | Self::Navigate { tab_id, .. }
            | Self::Back { tab_id }
            | Self::Forward { tab_id }
            | Self::Reload { tab_id }
            | Self::UpdateViewport { tab_id, .. }
            | Self::OpenDevTools { tab_id }
            | Self::SecretType { tab_id, .. }
            | Self::Snapshot { tab_id }
            | Self::Screenshot { tab_id, .. }
            | Self::Wait { tab_id, .. }
            | Self::Act { tab_id, .. }
            | Self::Console { tab_id, .. }
            | Self::Network { tab_id, .. }
            | Self::Performance { tab_id, .. }
            | Self::Upload { tab_id, .. }
            | Self::Downloads { tab_id, .. }
            | Self::Cdp { tab_id, .. } => Some(tab_id),
            Self::Stop { tab_id } => tab_id.as_deref(),
            Self::Status
            | Self::WorkspaceState
            | Self::Ensure { .. }
            | Self::SetPaneOpen { .. }
            | Self::SaveAnnotationDraft { .. }
            | Self::CancelAnnotationDraft { .. }
            | Self::Annotations { .. }
            | Self::Recording { .. }
            | Self::ListTabs
            | Self::CreateTab { .. }
            | Self::ResetWorkspace
            | Self::ClearProjectProfile
            | Self::DownloadDirectory => None,
        }
    }
}

const WORKSPACE_OPERATION_TARGET_TAB_ID: &str = "__workspace__";

pub fn browser_operation_target_tab_id(
    command: &BrowserCommand,
    selected_tab_id: Option<&str>,
) -> String {
    if matches!(
        command,
        BrowserCommand::Recording {
            operation: BrowserRecordingOperation::Save | BrowserRecordingOperation::Discard,
        }
    ) {
        return WORKSPACE_OPERATION_TARGET_TAB_ID.to_string();
    }
    command
        .tab_id()
        .or(selected_tab_id)
        .unwrap_or(WORKSPACE_OPERATION_TARGET_TAB_ID)
        .to_string()
}

pub fn browser_lifecycle_control(
    workspace_key: &BrowserWorkspaceKey,
    command: &BrowserCommand,
) -> Option<BrowserHostControl> {
    match command {
        BrowserCommand::Stop {
            tab_id: Some(tab_id),
        }
        | BrowserCommand::CloseTab { tab_id } => Some(BrowserHostControl::InterruptTab {
            workspace_key: workspace_key.clone(),
            tab_id: tab_id.clone(),
        }),
        BrowserCommand::Stop { tab_id: None } | BrowserCommand::ResetWorkspace => {
            Some(BrowserHostControl::InterruptWorkspace {
                workspace_key: workspace_key.clone(),
            })
        }
        BrowserCommand::ClearProjectProfile => Some(BrowserHostControl::InterruptProject {
            project_id: workspace_key.project_id.clone(),
        }),
        _ => None,
    }
}

pub fn browser_request_preempts_operation_queue(command: &BrowserCommand) -> bool {
    matches!(
        command,
        BrowserCommand::Status
            | BrowserCommand::WorkspaceState
            | BrowserCommand::ListTabs
            | BrowserCommand::Recording {
                operation: BrowserRecordingOperation::Status
                    | BrowserRecordingOperation::Start
                    | BrowserRecordingOperation::Stop
                    | BrowserRecordingOperation::Review,
            }
            | BrowserCommand::DownloadDirectory
            | BrowserCommand::Stop { .. }
            | BrowserCommand::CloseTab { .. }
            | BrowserCommand::ResetWorkspace
            | BrowserCommand::ClearProjectProfile
    )
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BrowserHostStatus {
    pub available: bool,
    pub platform: String,
    pub version: Option<String>,
    pub diagnostic: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum BrowserResponse {
    Status {
        status: BrowserHostStatus,
    },
    WorkspaceState {
        snapshot: BrowserWorkspaceSnapshot,
    },
    Workspace {
        mutation: BrowserWorkspaceMutation,
    },
    Tabs {
        tabs: Vec<BrowserTabSnapshot>,
        selected_tab_id: Option<String>,
    },
    DownloadDirectory {
        path: PathBuf,
    },
    Snapshot {
        summary: BrowserSnapshotSummary,
        resource: BrowserResourceHandle,
    },
    Screenshot {
        resource: BrowserResourceHandle,
    },
    Wait {
        result: BrowserWaitResult,
    },
    Action {
        result: BrowserActionResult,
    },
    Console {
        entries: Vec<BrowserConsoleEntry>,
        resource: Option<BrowserResourceHandle>,
    },
    Network {
        entries: Vec<BrowserNetworkEntry>,
        resource: Option<BrowserResourceHandle>,
        body_available: Option<bool>,
    },
    Performance {
        snapshot: Option<BrowserPerformanceSnapshot>,
        resource: Option<BrowserResourceHandle>,
        tracing: bool,
    },
    Upload {
        result: BrowserUploadResult,
    },
    Downloads {
        downloads: Vec<BrowserDownloadEntry>,
    },
    Cdp {
        inline_result: Option<serde_json::Value>,
        resource: Option<BrowserResourceHandle>,
    },
    AnnotationDraft {
        draft: BrowserAnnotationDraft,
    },
    Annotations {
        annotations: Vec<BrowserAnnotationSummary>,
        mutation: BrowserWorkspaceMutation,
    },
    Annotation {
        details: BrowserAnnotationDetails,
        mutation: BrowserWorkspaceMutation,
    },
    AnnotationMutation {
        result: BrowserAnnotationMutationResult,
    },
    Recording {
        result: BrowserRecordingResult,
    },
    Acknowledged,
}

pub fn browser_response_resource_ids(response: &BrowserResponse) -> Vec<BrowserResourceId> {
    match response {
        BrowserResponse::Snapshot { resource, .. } | BrowserResponse::Screenshot { resource } => {
            vec![resource.id.clone()]
        }
        BrowserResponse::AnnotationDraft { draft } => vec![draft.screenshot_resource.clone()],
        BrowserResponse::Annotation { details, .. } => vec![
            details.screenshot.id.clone(),
            details.details_resource.id.clone(),
        ],
        BrowserResponse::AnnotationMutation { result } => vec![result.screenshot.id.clone()],
        BrowserResponse::Recording { result } => result
            .resource
            .as_ref()
            .map(|resource| vec![resource.id.clone()])
            .unwrap_or_default(),
        BrowserResponse::Console { resource, .. }
        | BrowserResponse::Network { resource, .. }
        | BrowserResponse::Performance { resource, .. }
        | BrowserResponse::Cdp { resource, .. } => resource
            .as_ref()
            .map(|resource| vec![resource.id.clone()])
            .unwrap_or_default(),
        BrowserResponse::Status { .. }
        | BrowserResponse::WorkspaceState { .. }
        | BrowserResponse::Workspace { .. }
        | BrowserResponse::Annotations { .. }
        | BrowserResponse::Tabs { .. }
        | BrowserResponse::DownloadDirectory { .. }
        | BrowserResponse::Wait { .. }
        | BrowserResponse::Action { .. }
        | BrowserResponse::Upload { .. }
        | BrowserResponse::Downloads { .. }
        | BrowserResponse::Acknowledged => Vec::new(),
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum BrowserPageLoadState {
    Started,
    Finished,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum BrowserUserInputKind {
    Pointer,
    Keyboard,
    TextInput,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum BrowserDownloadState {
    Started,
    Completed { successful: bool },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum BrowserDiagnosticLevel {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum BrowserHostEvent {
    UrlChanged {
        workspace_key: BrowserWorkspaceKey,
        tab_id: String,
        url: String,
    },
    TitleChanged {
        workspace_key: BrowserWorkspaceKey,
        tab_id: String,
        title: String,
    },
    PageLoad {
        workspace_key: BrowserWorkspaceKey,
        tab_id: String,
        state: BrowserPageLoadState,
        url: String,
    },
    UserInput {
        workspace_key: BrowserWorkspaceKey,
        tab_id: String,
        kind: BrowserUserInputKind,
    },
    DomMutation {
        workspace_key: BrowserWorkspaceKey,
        tab_id: String,
    },
    AnnotationCandidate {
        workspace_key: BrowserWorkspaceKey,
        tab_id: String,
        candidate: BrowserAnnotationCandidate,
    },
    AnnotationCanceled {
        workspace_key: BrowserWorkspaceKey,
        tab_id: String,
    },
    AnnotationDraftReady {
        workspace_key: BrowserWorkspaceKey,
        tab_id: String,
        draft: BrowserAnnotationDraft,
    },
    AnnotationModeChanged {
        workspace_key: BrowserWorkspaceKey,
        tab_id: String,
        enabled: bool,
    },
    AutomationStateChanged {
        workspace_key: BrowserWorkspaceKey,
        tab_id: String,
    },
    ApprovalRequested {
        workspace_key: BrowserWorkspaceKey,
        tab_id: String,
        request: BrowserApprovalRequest,
    },
    NewWindow {
        workspace_key: BrowserWorkspaceKey,
        tab_id: String,
        url: String,
    },
    Download {
        workspace_key: BrowserWorkspaceKey,
        tab_id: String,
        state: BrowserDownloadState,
        url: String,
        path: PathBuf,
    },
    Diagnostic {
        workspace_key: BrowserWorkspaceKey,
        tab_id: String,
        level: BrowserDiagnosticLevel,
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowserHostControl {
    InterruptProject {
        project_id: String,
    },
    InterruptWorkspace {
        workspace_key: BrowserWorkspaceKey,
    },
    InterruptTab {
        workspace_key: BrowserWorkspaceKey,
        tab_id: String,
    },
}

#[derive(Clone)]
pub(crate) struct BrowserRegistrationLease {
    active: Arc<AtomicBool>,
    cancellation: watch::Sender<u64>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct BrowserRegistrationLeaseTicket(u64);

impl BrowserRegistrationLease {
    pub(crate) fn new() -> Self {
        Self {
            active: Arc::new(AtomicBool::new(true)),
            cancellation: watch::channel(0).0,
        }
    }

    pub(crate) fn capture(
        &self,
    ) -> Result<(BrowserRegistrationLeaseTicket, watch::Receiver<u64>), BrowserError> {
        let receiver = self.cancellation.subscribe();
        let ticket = BrowserRegistrationLeaseTicket(*receiver.borrow());
        if !self.is_current(ticket) {
            return Err(BrowserError::Interrupted);
        }
        Ok((ticket, receiver))
    }

    pub(crate) fn is_current(&self, ticket: BrowserRegistrationLeaseTicket) -> bool {
        self.active.load(Ordering::Acquire) && *self.cancellation.borrow() == ticket.0
    }

    fn revoke(&self) {
        if self.active.swap(false, Ordering::AcqRel) {
            advance(&self.cancellation);
        }
    }
}

fn registration_ticket_is_current(
    registration_lease: Option<&BrowserRegistrationLease>,
    ticket: Option<BrowserRegistrationLeaseTicket>,
) -> bool {
    match (registration_lease, ticket) {
        (None, None) => true,
        (Some(registration_lease), Some(ticket)) => registration_lease.is_current(ticket),
        _ => false,
    }
}

struct BrowserCommandEnvelope {
    workspace_key: BrowserWorkspaceKey,
    command: BrowserCommand,
    context: BrowserInvocationContext,
    local_project_root: Option<PathBuf>,
    cancellation_ticket: CancellationTicket,
    registration_lease: Option<BrowserRegistrationLease>,
    replay_secret_lease: Option<BrowserReplaySecretLease>,
    response: oneshot::Sender<Result<BrowserResponse, BrowserError>>,
    pending_work: PendingWorkGuard,
}

#[derive(Clone)]
pub struct BrowserCommandBridge {
    sender: mpsc::Sender<BrowserCommandEnvelope>,
    cancellations: Arc<CancellationEpochs>,
    host_controls: Arc<HostControlQueue>,
    pending_work: Arc<PendingWork>,
}

impl BrowserCommandBridge {
    pub fn bind(&self, workspace_key: BrowserWorkspaceKey, timeout: Duration) -> BrowserController {
        self.bind_with_registration_lease(workspace_key, timeout, None)
    }

    pub(crate) fn bind_with_registration_lease(
        &self,
        workspace_key: BrowserWorkspaceKey,
        timeout: Duration,
        registration_lease: Option<BrowserRegistrationLease>,
    ) -> BrowserController {
        BrowserController {
            workspace_key,
            sender: self.sender.clone(),
            timeout,
            cancellations: Arc::clone(&self.cancellations),
            host_controls: Arc::clone(&self.host_controls),
            pending_work: Arc::clone(&self.pending_work),
            registration_lease,
        }
    }

    pub fn pending_work_count(&self) -> usize {
        self.pending_work.count()
    }

    pub fn observe_host_event(&self, event: &BrowserHostEvent) {
        self.cancellations.observe_host_event(event);
    }

    pub fn interrupt_workspace(&self, workspace_key: &BrowserWorkspaceKey) {
        let control = BrowserHostControl::InterruptWorkspace {
            workspace_key: workspace_key.clone(),
        };
        self.host_controls.push_and(control.clone(), || {
            self.cancellations.interrupt_control(&control)
        });
    }

    pub(crate) fn revoke_registration(
        &self,
        workspace_key: &BrowserWorkspaceKey,
        registration_lease: &BrowserRegistrationLease,
    ) {
        let control = BrowserHostControl::InterruptWorkspace {
            workspace_key: workspace_key.clone(),
        };
        self.host_controls.push_and(control.clone(), || {
            registration_lease.revoke();
            self.cancellations.interrupt_control(&control);
        });
    }

    pub fn interrupt_project(&self, project_id: &str) {
        let control = BrowserHostControl::InterruptProject {
            project_id: project_id.to_string(),
        };
        self.host_controls.push_and(control.clone(), || {
            self.cancellations.interrupt_control(&control)
        });
    }

    pub fn interrupt_tab(&self, workspace_key: &BrowserWorkspaceKey, tab_id: &str) {
        let control = BrowserHostControl::InterruptTab {
            workspace_key: workspace_key.clone(),
            tab_id: tab_id.to_string(),
        };
        self.host_controls.push_and(control.clone(), || {
            self.cancellations.interrupt_control(&control)
        });
    }

    pub fn with_locked_host_controls_for_command<R>(
        &self,
        workspace_key: &BrowserWorkspaceKey,
        command: &BrowserCommand,
        apply: impl FnOnce(Vec<BrowserHostControl>, Vec<BrowserCommandRequest>) -> R,
    ) -> R {
        self.host_controls
            .with_drain_locked(|controls, lifecycle_requests| {
                if let Some(control) = browser_lifecycle_control(workspace_key, command) {
                    self.cancellations.interrupt_control(&control);
                }
                let lifecycle_requests = lifecycle_requests
                    .into_iter()
                    .map(|envelope| {
                        BrowserCommandRequest::from_envelope(
                            envelope,
                            Arc::clone(&self.cancellations),
                        )
                    })
                    .collect();
                apply(controls, lifecycle_requests)
            })
    }

    pub fn drain_host_controls(&self) -> Vec<BrowserHostControl> {
        self.host_controls.drain()
    }

    pub fn with_locked_host_controls<R>(
        &self,
        apply: impl FnOnce(Vec<BrowserHostControl>) -> R,
    ) -> R {
        self.host_controls.with_drain_controls_locked(apply)
    }

    pub fn with_locked_host_work<R>(
        &self,
        apply: impl FnOnce(Vec<BrowserHostControl>, Vec<BrowserCommandRequest>) -> R,
    ) -> R {
        self.host_controls
            .with_drain_locked(|controls, lifecycle_requests| {
                let lifecycle_requests = lifecycle_requests
                    .into_iter()
                    .map(|envelope| {
                        BrowserCommandRequest::from_envelope(
                            envelope,
                            Arc::clone(&self.cancellations),
                        )
                    })
                    .collect();
                apply(controls, lifecycle_requests)
            })
    }
}

#[derive(Clone)]
pub struct BrowserController {
    workspace_key: BrowserWorkspaceKey,
    sender: mpsc::Sender<BrowserCommandEnvelope>,
    timeout: Duration,
    cancellations: Arc<CancellationEpochs>,
    host_controls: Arc<HostControlQueue>,
    pending_work: Arc<PendingWork>,
    registration_lease: Option<BrowserRegistrationLease>,
}

impl BrowserController {
    pub fn workspace_key(&self) -> &BrowserWorkspaceKey {
        &self.workspace_key
    }

    pub fn pending_work_count(&self) -> usize {
        self.pending_work.count()
    }

    pub(crate) fn capture_registration_lease_ticket(
        &self,
    ) -> Result<Option<BrowserRegistrationLeaseTicket>, BrowserError> {
        self.registration_lease
            .as_ref()
            .map(|registration_lease| {
                registration_lease
                    .capture()
                    .map(|(ticket, _cancellation)| ticket)
            })
            .transpose()
    }

    pub(crate) fn registration_lease_is_current(
        &self,
        ticket: Option<BrowserRegistrationLeaseTicket>,
    ) -> bool {
        registration_ticket_is_current(self.registration_lease.as_ref(), ticket)
    }

    pub async fn request(&self, command: BrowserCommand) -> Result<BrowserResponse, BrowserError> {
        let context = BrowserInvocationContext::internal(command.operation_name());
        self.request_with_context(command, context).await
    }

    pub async fn request_with_context(
        &self,
        command: BrowserCommand,
        context: BrowserInvocationContext,
    ) -> Result<BrowserResponse, BrowserError> {
        self.request_with_context_and_local_project_root(command, context, None, None)
            .await
    }

    pub(crate) async fn request_with_local_project_root(
        &self,
        command: BrowserCommand,
        context: BrowserInvocationContext,
        local_project_root: &std::path::Path,
    ) -> Result<BrowserResponse, BrowserError> {
        let canonical = verified_authenticated_local_project_root(local_project_root)?;
        self.request_with_context_and_local_project_root(command, context, Some(canonical), None)
            .await
    }

    #[allow(dead_code)] // The checkpoint-9 replay executor consumes this secure lane in Task 4.
    pub(crate) async fn request_replay_secret_type(
        &self,
        command: BrowserCommand,
        context: BrowserInvocationContext,
        lease: BrowserReplaySecretLease,
    ) -> Result<BrowserResponse, BrowserError> {
        validate_secret_command_authority(&self.workspace_key, &command, &context, &lease)?;
        self.request_with_context_and_local_project_root(command, context, None, Some(lease))
            .await
    }

    async fn request_with_context_and_local_project_root(
        &self,
        command: BrowserCommand,
        context: BrowserInvocationContext,
        local_project_root: Option<PathBuf>,
        replay_secret_lease: Option<BrowserReplaySecretLease>,
    ) -> Result<BrowserResponse, BrowserError> {
        context.validate()?;
        let operation = command.operation_name().to_string();
        let transport_timeout = command_transport_timeout(self.timeout, &command);
        let is_lifecycle = browser_lifecycle_control(&self.workspace_key, &command).is_some();
        let (response, receiver) = oneshot::channel();
        let timeout = tokio::time::sleep(transport_timeout);
        tokio::pin!(timeout);
        let cancellations = if is_lifecycle {
            self.enqueue_lifecycle_command(
                command.clone(),
                context.clone(),
                local_project_root.clone(),
                response,
            )?
        } else {
            let (cancellation_ticket, cancellations) =
                self.cancellation_state_for_command(&command)?;
            let send = self.sender.send(BrowserCommandEnvelope {
                workspace_key: self.workspace_key.clone(),
                command,
                context,
                local_project_root,
                cancellation_ticket,
                registration_lease: self.registration_lease.clone(),
                replay_secret_lease,
                response,
                pending_work: self.pending_work.track(),
            });
            tokio::pin!(send);
            let mut project_cancellation = cancellations.project;
            let mut workspace_cancellation = cancellations.workspace;
            let mut tab_cancellation = cancellations.tab;
            let mut registration_cancellation = cancellations.registration;
            tokio::select! {
                result = &mut send => result.map_err(|_| BrowserError::CrashedView {
                    message: "browser command inbox is closed".to_string(),
                })?,
                _ = project_cancellation.changed() => return Err(BrowserError::Interrupted),
                _ = workspace_cancellation.changed() => return Err(BrowserError::Interrupted),
                _ = wait_for_tab_cancellation(&mut tab_cancellation) => {
                    return Err(BrowserError::Interrupted);
                }
                _ = wait_for_registration_cancellation(&mut registration_cancellation) => {
                    return Err(BrowserError::Interrupted);
                }
                _ = &mut timeout => return Err(BrowserError::Timeout { operation }),
            }
            CancellationSubscriptions {
                project: project_cancellation,
                workspace: workspace_cancellation,
                tab: tab_cancellation,
                registration: registration_cancellation,
            }
        };
        let mut project_cancellation = cancellations.project;
        let mut workspace_cancellation = cancellations.workspace;
        let mut tab_cancellation = cancellations.tab;
        let mut registration_cancellation = cancellations.registration;
        tokio::select! {
            response = receiver => response.unwrap_or_else(|_| {
                Err(BrowserError::CrashedView {
                    message: "browser command request was dropped without a response".to_string(),
                })
            }),
            _ = project_cancellation.changed() => Err(BrowserError::Interrupted),
            _ = workspace_cancellation.changed() => Err(BrowserError::Interrupted),
            _ = wait_for_tab_cancellation(&mut tab_cancellation) => Err(BrowserError::Interrupted),
            _ = wait_for_registration_cancellation(&mut registration_cancellation) => Err(BrowserError::Interrupted),
            _ = &mut timeout => Err(BrowserError::Timeout { operation }),
        }
    }

    pub async fn notify(&self, command: BrowserCommand) -> Result<(), BrowserError> {
        let context = BrowserInvocationContext::internal(command.operation_name());
        self.notify_with_context(command, context).await
    }

    pub async fn notify_with_context(
        &self,
        command: BrowserCommand,
        context: BrowserInvocationContext,
    ) -> Result<(), BrowserError> {
        context.validate()?;
        let (response, receiver) = oneshot::channel();
        drop(receiver);
        if browser_lifecycle_control(&self.workspace_key, &command).is_some() {
            self.enqueue_lifecycle_command(command, context, None, response)?;
            return Ok(());
        }
        let cancellation_ticket = self.cancellation_ticket_for_command(&command)?;
        self.sender
            .send(BrowserCommandEnvelope {
                workspace_key: self.workspace_key.clone(),
                command,
                context,
                local_project_root: None,
                cancellation_ticket,
                registration_lease: self.registration_lease.clone(),
                replay_secret_lease: None,
                response,
                pending_work: self.pending_work.track(),
            })
            .await
            .map_err(|_| BrowserError::CrashedView {
                message: "browser command inbox is closed".to_string(),
            })
    }

    pub fn interrupt_workspace(&self) {
        let control = BrowserHostControl::InterruptWorkspace {
            workspace_key: self.workspace_key.clone(),
        };
        self.host_controls.push_and(control.clone(), || {
            self.cancellations.interrupt_control(&control)
        });
    }

    pub fn interrupt_tab(&self, tab_id: &str) {
        let control = BrowserHostControl::InterruptTab {
            workspace_key: self.workspace_key.clone(),
            tab_id: tab_id.to_string(),
        };
        self.host_controls.push_and(control.clone(), || {
            self.cancellations.interrupt_control(&control)
        });
    }

    fn cancellation_ticket_for_command(
        &self,
        command: &BrowserCommand,
    ) -> Result<CancellationTicket, BrowserError> {
        self.host_controls.with_locked(|| {
            let mut ticket = self
                .cancellations
                .ticket(&self.workspace_key, command.tab_id());
            if let Some(registration_lease) = &self.registration_lease {
                let (registration_ticket, _) = registration_lease.capture()?;
                ticket.registration = Some(registration_ticket);
            }
            Ok(ticket)
        })
    }

    fn cancellation_state_for_command(
        &self,
        command: &BrowserCommand,
    ) -> Result<(CancellationTicket, CancellationSubscriptions), BrowserError> {
        self.host_controls.with_locked(|| {
            let mut ticket = self
                .cancellations
                .ticket(&self.workspace_key, command.tab_id());
            let mut subscriptions = self
                .cancellations
                .subscribe(&self.workspace_key, command.tab_id());
            if let Some(registration_lease) = &self.registration_lease {
                let (registration_ticket, registration_cancellation) =
                    registration_lease.capture()?;
                ticket.registration = Some(registration_ticket);
                subscriptions.registration = Some(registration_cancellation);
            }
            Ok((ticket, subscriptions))
        })
    }

    fn enqueue_lifecycle_command(
        &self,
        command: BrowserCommand,
        context: BrowserInvocationContext,
        local_project_root: Option<PathBuf>,
        response: oneshot::Sender<Result<BrowserResponse, BrowserError>>,
    ) -> Result<CancellationSubscriptions, BrowserError> {
        let control = browser_lifecycle_control(&self.workspace_key, &command)
            .expect("only lifecycle commands use the priority host queue");
        self.host_controls
            .with_lifecycle_queue_locked(|lifecycle_requests| {
                let registration_state = self
                    .registration_lease
                    .as_ref()
                    .map(BrowserRegistrationLease::capture)
                    .transpose()?;
                self.cancellations.interrupt_control(&control);
                let mut cancellation_ticket = self
                    .cancellations
                    .ticket(&self.workspace_key, command.tab_id());
                let mut subscriptions = self
                    .cancellations
                    .subscribe(&self.workspace_key, command.tab_id());
                if let Some((registration_ticket, registration_cancellation)) = registration_state {
                    cancellation_ticket.registration = Some(registration_ticket);
                    subscriptions.registration = Some(registration_cancellation);
                }
                lifecycle_requests.push_back(BrowserCommandEnvelope {
                    workspace_key: self.workspace_key.clone(),
                    command,
                    context,
                    local_project_root,
                    cancellation_ticket,
                    registration_lease: self.registration_lease.clone(),
                    replay_secret_lease: None,
                    response,
                    pending_work: self.pending_work.track(),
                });
                Ok(subscriptions)
            })
    }
}

#[allow(dead_code)] // Used by the Task-4 replay executor through the secure controller method.
fn validate_secret_command_authority(
    workspace_key: &BrowserWorkspaceKey,
    command: &BrowserCommand,
    context: &BrowserInvocationContext,
    lease: &BrowserReplaySecretLease,
) -> Result<(), BrowserError> {
    let BrowserCommand::SecretType { input_name, .. } = command else {
        return Err(invalid_secret_sidecar());
    };
    if context.actor != BrowserInvocationActor::Agent
        || !lease.authorizes(workspace_key, input_name)
    {
        return Err(invalid_secret_sidecar());
    }
    Ok(())
}

fn invalid_secret_sidecar() -> BrowserError {
    BrowserError::InvalidInvocation {
        field: "secretSidecar".to_string(),
    }
}

pub(crate) fn verified_authenticated_local_project_root(
    project_root: &Path,
) -> Result<PathBuf, BrowserError> {
    if browser_project_root_is_remote(project_root) {
        return Err(invalid_local_project_root());
    }
    let canonical = project_root
        .canonicalize()
        .map_err(|_| invalid_local_project_root())?;
    if canonical != project_root
        || !canonical.is_dir()
        || browser_project_root_is_remote(&canonical)
    {
        return Err(invalid_local_project_root());
    }
    Ok(canonical)
}

fn invalid_local_project_root() -> BrowserError {
    BrowserError::InvalidInvocation {
        field: "localProjectRoot".to_string(),
    }
}

fn browser_project_root_is_remote(path: &Path) -> bool {
    #[cfg(windows)]
    {
        matches!(
            path.components().next(),
            Some(Component::Prefix(prefix))
                if matches!(prefix.kind(), Prefix::UNC(_, _) | Prefix::VerbatimUNC(_, _))
        )
    }
    #[cfg(not(windows))]
    {
        let text = path.as_os_str().to_string_lossy();
        text.starts_with(r"\\") || text.starts_with("//")
    }
}

fn command_transport_timeout(base: Duration, command: &BrowserCommand) -> Duration {
    match command {
        BrowserCommand::Wait { timeout_ms, .. } => {
            base.saturating_add(Duration::from_millis(*timeout_ms))
        }
        _ => base,
    }
}

pub struct BrowserCommandInbox {
    receiver: mpsc::Receiver<BrowserCommandEnvelope>,
    cancellations: Arc<CancellationEpochs>,
    host_controls: Arc<HostControlQueue>,
    pending_work: Arc<PendingWork>,
    _main_thread_only: PhantomData<Rc<()>>,
}

impl BrowserCommandInbox {
    pub async fn recv(&mut self) -> Option<BrowserCommandRequest> {
        while let Some(envelope) = self.receiver.recv().await {
            if self.cancellations.is_current(
                &envelope.workspace_key,
                envelope.command.tab_id(),
                envelope.cancellation_ticket,
            ) && registration_ticket_is_current(
                envelope.registration_lease.as_ref(),
                envelope.cancellation_ticket.registration,
            ) {
                return Some(BrowserCommandRequest::from_envelope(
                    envelope,
                    Arc::clone(&self.cancellations),
                ));
            }
            let _ = envelope.response.send(Err(BrowserError::Interrupted));
        }
        None
    }

    pub fn pending_work_count(&self) -> usize {
        self.pending_work.count()
    }

    pub fn interrupt_workspace(&self, workspace_key: &BrowserWorkspaceKey) {
        let control = BrowserHostControl::InterruptWorkspace {
            workspace_key: workspace_key.clone(),
        };
        self.host_controls.push_and(control.clone(), || {
            self.cancellations.interrupt_control(&control)
        });
    }

    pub fn interrupt_tab(&self, workspace_key: &BrowserWorkspaceKey, tab_id: &str) {
        let control = BrowserHostControl::InterruptTab {
            workspace_key: workspace_key.clone(),
            tab_id: tab_id.to_string(),
        };
        self.host_controls.push_and(control.clone(), || {
            self.cancellations.interrupt_control(&control)
        });
    }

    pub fn drain_host_controls(&self) -> Vec<BrowserHostControl> {
        self.host_controls.drain()
    }

    pub fn with_locked_host_work<R>(
        &self,
        apply: impl FnOnce(Vec<BrowserHostControl>, Vec<BrowserCommandRequest>) -> R,
    ) -> R {
        self.host_controls
            .with_drain_locked(|controls, lifecycle_requests| {
                let lifecycle_requests = lifecycle_requests
                    .into_iter()
                    .map(|envelope| {
                        BrowserCommandRequest::from_envelope(
                            envelope,
                            Arc::clone(&self.cancellations),
                        )
                    })
                    .collect();
                apply(controls, lifecycle_requests)
            })
    }

    pub fn observe_host_event(&self, event: &BrowserHostEvent) {
        self.cancellations.observe_host_event(event);
    }
}

pub struct BrowserCommandRequest {
    workspace_key: BrowserWorkspaceKey,
    command: BrowserCommand,
    context: BrowserInvocationContext,
    local_project_root: Option<PathBuf>,
    cancellation_ticket: CancellationTicket,
    cancellations: Arc<CancellationEpochs>,
    registration_lease: Option<BrowserRegistrationLease>,
    replay_secret_lease: Option<BrowserReplaySecretLease>,
    response: oneshot::Sender<Result<BrowserResponse, BrowserError>>,
    _pending_work: PendingWorkGuard,
    started_at: String,
    started: Instant,
}

impl BrowserCommandRequest {
    pub fn workspace_key(&self) -> &BrowserWorkspaceKey {
        &self.workspace_key
    }

    pub fn command(&self) -> &BrowserCommand {
        &self.command
    }

    pub fn context(&self) -> &BrowserInvocationContext {
        &self.context
    }

    pub fn local_project_root(&self) -> Option<&std::path::Path> {
        self.local_project_root.as_deref()
    }

    pub fn cancellation_is_current(&self) -> bool {
        self.cancellations.is_current(
            &self.workspace_key,
            self.command.tab_id(),
            self.cancellation_ticket,
        ) && registration_ticket_is_current(
            self.registration_lease.as_ref(),
            self.cancellation_ticket.registration,
        )
    }

    pub fn validate_secret_sidecar(
        &self,
    ) -> Result<Option<&BrowserReplaySecretLease>, BrowserError> {
        match (&self.command, &self.replay_secret_lease) {
            (BrowserCommand::SecretType { input_name, .. }, Some(lease))
                if self.context.actor == BrowserInvocationActor::Agent
                    && lease.authorizes(&self.workspace_key, input_name) =>
            {
                Ok(Some(lease))
            }
            (BrowserCommand::SecretType { .. }, _) | (_, Some(_)) => Err(invalid_secret_sidecar()),
            (_, None) => Ok(None),
        }
    }

    pub(crate) fn started_at(&self) -> &str {
        &self.started_at
    }

    pub(crate) fn elapsed_ms(&self) -> u64 {
        self.started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
    }

    pub fn respond(self, result: Result<BrowserResponse, BrowserError>) {
        let _ = self.response.send(result);
    }
}

pub fn route_browser_request(
    route_is_open: bool,
    request: BrowserCommandRequest,
    dispatch_open: impl FnOnce(BrowserCommandRequest),
) -> Result<(), BrowserError> {
    if !route_is_open {
        let error = BrowserError::CrashedView {
            message: "browser command route does not match an open AI conversation".to_string(),
        };
        request.respond(Err(error.clone()));
        return Err(error);
    }
    dispatch_open(request);
    Ok(())
}

impl BrowserCommandRequest {
    fn from_envelope(
        envelope: BrowserCommandEnvelope,
        cancellations: Arc<CancellationEpochs>,
    ) -> Self {
        let BrowserCommandEnvelope {
            workspace_key,
            command,
            context,
            local_project_root,
            cancellation_ticket,
            registration_lease,
            replay_secret_lease,
            response,
            pending_work,
        } = envelope;
        Self {
            workspace_key,
            command,
            context,
            local_project_root,
            cancellation_ticket,
            cancellations,
            registration_lease,
            replay_secret_lease,
            response,
            _pending_work: pending_work,
            started_at: OffsetDateTime::now_utc()
                .format(&Rfc3339)
                .unwrap_or_else(|_| "unknown".to_string()),
            started: Instant::now(),
        }
    }
}

pub fn browser_command_channel(capacity: usize) -> (BrowserCommandBridge, BrowserCommandInbox) {
    let (sender, receiver) = mpsc::channel(capacity.max(1));
    let cancellations = Arc::new(CancellationEpochs::default());
    let host_controls = Arc::new(HostControlQueue::default());
    let pending_work = Arc::new(PendingWork::default());
    (
        BrowserCommandBridge {
            sender,
            cancellations: Arc::clone(&cancellations),
            host_controls: Arc::clone(&host_controls),
            pending_work: Arc::clone(&pending_work),
        },
        BrowserCommandInbox {
            receiver,
            cancellations,
            host_controls,
            pending_work,
            _main_thread_only: PhantomData,
        },
    )
}

#[derive(Default)]
struct HostPriorityQueue {
    controls: VecDeque<BrowserHostControl>,
    lifecycle_requests: VecDeque<BrowserCommandEnvelope>,
}

#[derive(Default)]
struct HostControlQueue {
    queued: Mutex<HostPriorityQueue>,
}

impl HostControlQueue {
    fn push_and<R>(&self, control: BrowserHostControl, then: impl FnOnce() -> R) -> R {
        let mut queued = lock(&self.queued);
        queued.controls.push_back(control);
        let result = then();
        drop(queued);
        result
    }

    fn with_locked<R>(&self, apply: impl FnOnce() -> R) -> R {
        let queued = lock(&self.queued);
        let result = apply();
        drop(queued);
        result
    }

    fn drain(&self) -> Vec<BrowserHostControl> {
        lock(&self.queued).controls.drain(..).collect()
    }

    fn with_lifecycle_queue_locked<R>(
        &self,
        apply: impl FnOnce(&mut VecDeque<BrowserCommandEnvelope>) -> R,
    ) -> R {
        let mut queued = lock(&self.queued);
        let result = apply(&mut queued.lifecycle_requests);
        drop(queued);
        result
    }

    fn with_drain_controls_locked<R>(&self, apply: impl FnOnce(Vec<BrowserHostControl>) -> R) -> R {
        let mut queued = lock(&self.queued);
        let controls = queued.controls.drain(..).collect();
        let result = apply(controls);
        drop(queued);
        result
    }

    fn with_drain_locked<R>(
        &self,
        apply: impl FnOnce(Vec<BrowserHostControl>, Vec<BrowserCommandEnvelope>) -> R,
    ) -> R {
        let mut queued = lock(&self.queued);
        let controls = queued.controls.drain(..).collect();
        let lifecycle_requests = queued.lifecycle_requests.drain(..).collect();
        let result = apply(controls, lifecycle_requests);
        drop(queued);
        result
    }
}

#[derive(Default)]
struct PendingWork {
    count: AtomicUsize,
}

impl PendingWork {
    fn track(self: &Arc<Self>) -> PendingWorkGuard {
        self.count.fetch_add(1, Ordering::AcqRel);
        PendingWorkGuard {
            pending_work: Arc::clone(self),
        }
    }

    fn count(&self) -> usize {
        self.count.load(Ordering::Acquire)
    }
}

struct PendingWorkGuard {
    pending_work: Arc<PendingWork>,
}

impl Drop for PendingWorkGuard {
    fn drop(&mut self) {
        let previous = self.pending_work.count.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "browser pending work count underflow");
    }
}

#[derive(Debug, Clone, Copy)]
struct CancellationTicket {
    project: u64,
    workspace: u64,
    tab: Option<u64>,
    registration: Option<BrowserRegistrationLeaseTicket>,
}

#[derive(Default)]
struct CancellationEpochs {
    projects: Mutex<HashMap<String, watch::Sender<u64>>>,
    workspaces: Mutex<HashMap<BrowserWorkspaceKey, watch::Sender<u64>>>,
    tabs: Mutex<HashMap<(BrowserWorkspaceKey, String), watch::Sender<u64>>>,
}

impl CancellationEpochs {
    fn subscribe(
        &self,
        workspace_key: &BrowserWorkspaceKey,
        tab_id: Option<&str>,
    ) -> CancellationSubscriptions {
        let project =
            sender_for(&mut lock(&self.projects), workspace_key.project_id.clone()).subscribe();
        let workspace = sender_for(&mut lock(&self.workspaces), workspace_key.clone()).subscribe();
        let tab = tab_id.map(|tab_id| {
            sender_for(
                &mut lock(&self.tabs),
                (workspace_key.clone(), tab_id.to_string()),
            )
            .subscribe()
        });
        CancellationSubscriptions {
            project,
            workspace,
            tab,
            registration: None,
        }
    }

    fn ticket(
        &self,
        workspace_key: &BrowserWorkspaceKey,
        tab_id: Option<&str>,
    ) -> CancellationTicket {
        let project = current_epoch(&mut lock(&self.projects), workspace_key.project_id.clone());
        let workspace = current_epoch(&mut lock(&self.workspaces), workspace_key.clone());
        let tab = tab_id.map(|tab_id| {
            current_epoch(
                &mut lock(&self.tabs),
                (workspace_key.clone(), tab_id.to_string()),
            )
        });
        CancellationTicket {
            project,
            workspace,
            tab,
            registration: None,
        }
    }

    fn is_current(
        &self,
        workspace_key: &BrowserWorkspaceKey,
        tab_id: Option<&str>,
        ticket: CancellationTicket,
    ) -> bool {
        current_epoch(&mut lock(&self.projects), workspace_key.project_id.clone()) == ticket.project
            && current_epoch(&mut lock(&self.workspaces), workspace_key.clone()) == ticket.workspace
            && tab_id.map(|tab_id| {
                current_epoch(
                    &mut lock(&self.tabs),
                    (workspace_key.clone(), tab_id.to_string()),
                )
            }) == ticket.tab
    }

    fn interrupt_control(&self, control: &BrowserHostControl) {
        match control {
            BrowserHostControl::InterruptProject { project_id } => {
                self.interrupt_project(project_id)
            }
            BrowserHostControl::InterruptWorkspace { workspace_key } => {
                self.interrupt_workspace(workspace_key)
            }
            BrowserHostControl::InterruptTab {
                workspace_key,
                tab_id,
            } => self.interrupt_tab(workspace_key, tab_id),
        }
    }

    fn interrupt_project(&self, project_id: &str) {
        advance(sender_for(
            &mut lock(&self.projects),
            project_id.to_string(),
        ));
    }

    fn interrupt_workspace(&self, workspace_key: &BrowserWorkspaceKey) {
        advance(sender_for(
            &mut lock(&self.workspaces),
            workspace_key.clone(),
        ));
    }

    fn interrupt_tab(&self, workspace_key: &BrowserWorkspaceKey, tab_id: &str) {
        advance(sender_for(
            &mut lock(&self.tabs),
            (workspace_key.clone(), tab_id.to_string()),
        ));
    }

    fn observe_host_event(&self, event: &BrowserHostEvent) {
        if let BrowserHostEvent::UserInput {
            workspace_key,
            tab_id,
            ..
        } = event
        {
            self.interrupt_tab(workspace_key, tab_id);
        }
    }
}

#[cfg(test)]
mod secure_command_tests {
    use super::*;
    use crate::browser::{
        BrowserActionTarget, BrowserReplaySecretLease, BrowserReplaySecretStore,
        BrowserReplaySecretSubmission,
    };

    const SECRET_INPUT: &str = "password";
    const SECRET_VALUE: &str = "value-sentinel-secure-sidecar";

    fn workspace(project_id: &str, ai_tab_id: &str) -> BrowserWorkspaceKey {
        BrowserWorkspaceKey::new(project_id, ai_tab_id).unwrap()
    }

    fn marker(input_name: &str) -> BrowserCommand {
        BrowserCommand::SecretType {
            tab_id: "tab-a".to_string(),
            target: BrowserActionTarget::default(),
            input_name: input_name.to_string(),
        }
    }

    fn agent_context() -> BrowserInvocationContext {
        BrowserInvocationContext::agent("type replay secret", BrowserRisk::AccountSecurity).unwrap()
    }

    fn installed_secret(
        workspace_key: &BrowserWorkspaceKey,
        instance_id: u64,
        input_name: &str,
    ) -> (BrowserReplaySecretStore, BrowserReplaySecretLease) {
        let store = BrowserReplaySecretStore::new(workspace_key.clone(), instance_id);
        store
            .install(
                &[input_name.to_string()],
                BrowserReplaySecretSubmission::from_user_prompt(vec![(
                    input_name.to_string(),
                    SECRET_VALUE.to_string(),
                )]),
            )
            .unwrap();
        let lease = store.lease(input_name).unwrap();
        (store, lease)
    }

    fn forged_request(
        workspace_key: BrowserWorkspaceKey,
        command: BrowserCommand,
        replay_secret_lease: Option<BrowserReplaySecretLease>,
    ) -> BrowserCommandRequest {
        let cancellations = Arc::new(CancellationEpochs::default());
        let cancellation_ticket = cancellations.ticket(&workspace_key, command.tab_id());
        let pending_work = Arc::new(PendingWork::default());
        let (response, _receiver) = oneshot::channel();
        BrowserCommandRequest::from_envelope(
            BrowserCommandEnvelope {
                workspace_key,
                command,
                context: agent_context(),
                local_project_root: None,
                cancellation_ticket,
                registration_lease: None,
                replay_secret_lease,
                response,
                pending_work: pending_work.track(),
            },
            cancellations,
        )
    }

    async fn wait_for_pending(bridge: &BrowserCommandBridge) {
        tokio::time::timeout(Duration::from_secs(1), async {
            while bridge.pending_work_count() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("secure request becomes pending");
    }

    #[tokio::test]
    async fn secure_command_method_enqueues_only_an_exact_agent_marker_and_lease_pair() {
        let (bridge, mut inbox) = browser_command_channel(2);
        let workspace_key = workspace("project-a", "conversation-a");
        let controller = bridge.bind(workspace_key.clone(), Duration::from_secs(1));
        let (_store, lease) = installed_secret(&workspace_key, 1, SECRET_INPUT);
        let task = tokio::spawn(async move {
            controller
                .request_replay_secret_type(marker(SECRET_INPUT), agent_context(), lease)
                .await
        });

        let request = inbox.recv().await.expect("secure marker reaches host");
        assert!(
            matches!(request.command(), BrowserCommand::SecretType { input_name, .. } if input_name == SECRET_INPUT)
        );
        assert!(request
            .validate_secret_sidecar()
            .expect("exact sidecar is valid")
            .is_some());
        request.respond(Ok(BrowserResponse::Acknowledged));
        assert_eq!(task.await.unwrap(), Ok(BrowserResponse::Acknowledged));
    }

    #[tokio::test]
    async fn secure_command_method_rejects_wrong_actor_workspace_input_and_stale_store() {
        let (bridge, mut inbox) = browser_command_channel(4);
        let workspace_key = workspace("project-a", "conversation-a");
        let controller = bridge.bind(workspace_key.clone(), Duration::from_millis(100));

        let (_actor_store, actor_lease) = installed_secret(&workspace_key, 1, SECRET_INPUT);
        let user_context =
            BrowserInvocationContext::user("type replay secret", BrowserRisk::Normal).unwrap();
        assert!(matches!(
            controller
                .request_replay_secret_type(marker(SECRET_INPUT), user_context, actor_lease)
                .await,
            Err(BrowserError::InvalidInvocation { field }) if field == "secretSidecar"
        ));

        let foreign_workspace = workspace("project-b", "conversation-b");
        let (_foreign_store, foreign_lease) = installed_secret(&foreign_workspace, 2, SECRET_INPUT);
        assert!(matches!(
            controller
                .request_replay_secret_type(marker(SECRET_INPUT), agent_context(), foreign_lease)
                .await,
            Err(BrowserError::InvalidInvocation { field }) if field == "secretSidecar"
        ));

        let (_input_store, input_lease) = installed_secret(&workspace_key, 3, SECRET_INPUT);
        assert!(matches!(
            controller
                .request_replay_secret_type(marker("other-input"), agent_context(), input_lease)
                .await,
            Err(BrowserError::InvalidInvocation { field }) if field == "secretSidecar"
        ));

        let (stale_store, stale_lease) = installed_secret(&workspace_key, 4, SECRET_INPUT);
        stale_store.close();
        assert!(matches!(
            controller
                .request_replay_secret_type(marker(SECRET_INPUT), agent_context(), stale_lease)
                .await,
            Err(BrowserError::InvalidInvocation { field }) if field == "secretSidecar"
        ));

        assert!(
            tokio::time::timeout(Duration::from_millis(20), inbox.recv())
                .await
                .is_err()
        );
    }

    #[test]
    fn secure_command_host_validation_rejects_sidecar_command_input_workspace_and_stale_mismatch() {
        let workspace_key = workspace("project-a", "conversation-a");

        let (_command_store, command_lease) = installed_secret(&workspace_key, 1, SECRET_INPUT);
        let wrong_command = forged_request(
            workspace_key.clone(),
            BrowserCommand::Status,
            Some(command_lease),
        );
        assert!(matches!(
            wrong_command.validate_secret_sidecar(),
            Err(BrowserError::InvalidInvocation { field }) if field == "secretSidecar"
        ));

        let (_input_store, input_lease) = installed_secret(&workspace_key, 2, SECRET_INPUT);
        let wrong_input = forged_request(
            workspace_key.clone(),
            marker("other-input"),
            Some(input_lease),
        );
        assert!(matches!(
            wrong_input.validate_secret_sidecar(),
            Err(BrowserError::InvalidInvocation { field }) if field == "secretSidecar"
        ));

        let foreign_workspace = workspace("project-b", "conversation-b");
        let (_workspace_store, workspace_lease) =
            installed_secret(&foreign_workspace, 3, SECRET_INPUT);
        let wrong_workspace = forged_request(
            workspace_key.clone(),
            marker(SECRET_INPUT),
            Some(workspace_lease),
        );
        assert!(matches!(
            wrong_workspace.validate_secret_sidecar(),
            Err(BrowserError::InvalidInvocation { field }) if field == "secretSidecar"
        ));

        let (stale_store, stale_lease) = installed_secret(&workspace_key, 4, SECRET_INPUT);
        stale_store.close();
        let stale = forged_request(workspace_key, marker(SECRET_INPUT), Some(stale_lease));
        assert!(matches!(
            stale.validate_secret_sidecar(),
            Err(BrowserError::InvalidInvocation { field }) if field == "secretSidecar"
        ));
    }

    #[tokio::test]
    async fn secure_command_pending_request_obeys_tab_cancellation() {
        let (bridge, mut inbox) = browser_command_channel(1);
        let workspace_key = workspace("project-a", "conversation-a");
        let controller = bridge.bind(workspace_key.clone(), Duration::from_secs(1));
        let request_controller = controller.clone();
        let (_store, lease) = installed_secret(&workspace_key, 1, SECRET_INPUT);
        let task = tokio::spawn(async move {
            request_controller
                .request_replay_secret_type(marker(SECRET_INPUT), agent_context(), lease)
                .await
        });
        wait_for_pending(&bridge).await;

        controller.interrupt_tab("tab-a");
        assert_eq!(task.await.unwrap(), Err(BrowserError::Interrupted));
        drop(controller);
        drop(bridge);
        assert!(inbox.recv().await.is_none());
        assert_eq!(inbox.pending_work_count(), 0);
    }

    #[tokio::test]
    async fn secure_command_pending_request_obeys_registration_revocation() {
        let (bridge, mut inbox) = browser_command_channel(1);
        let workspace_key = workspace("project-a", "conversation-a");
        let registration = BrowserRegistrationLease::new();
        let controller = bridge.bind_with_registration_lease(
            workspace_key.clone(),
            Duration::from_secs(1),
            Some(registration.clone()),
        );
        let request_controller = controller.clone();
        let (_store, lease) = installed_secret(&workspace_key, 1, SECRET_INPUT);
        let task = tokio::spawn(async move {
            request_controller
                .request_replay_secret_type(marker(SECRET_INPUT), agent_context(), lease)
                .await
        });
        wait_for_pending(&bridge).await;

        bridge.revoke_registration(&workspace_key, &registration);
        assert_eq!(task.await.unwrap(), Err(BrowserError::Interrupted));
        drop(controller);
        drop(bridge);
        assert!(inbox.recv().await.is_none());
        assert_eq!(inbox.pending_work_count(), 0);
    }
}

struct CancellationSubscriptions {
    project: watch::Receiver<u64>,
    workspace: watch::Receiver<u64>,
    tab: Option<watch::Receiver<u64>>,
    registration: Option<watch::Receiver<u64>>,
}

async fn wait_for_tab_cancellation(tab: &mut Option<watch::Receiver<u64>>) {
    match tab {
        Some(tab) => {
            let _ = tab.changed().await;
        }
        None => std::future::pending::<()>().await,
    }
}

async fn wait_for_registration_cancellation(registration: &mut Option<watch::Receiver<u64>>) {
    match registration {
        Some(registration) => {
            let _ = registration.changed().await;
        }
        None => std::future::pending::<()>().await,
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn sender_for<Key>(senders: &mut HashMap<Key, watch::Sender<u64>>, key: Key) -> &watch::Sender<u64>
where
    Key: Eq + std::hash::Hash,
{
    senders.entry(key).or_insert_with(|| watch::channel(0).0)
}

fn current_epoch<Key>(senders: &mut HashMap<Key, watch::Sender<u64>>, key: Key) -> u64
where
    Key: Eq + std::hash::Hash,
{
    let sender = sender_for(senders, key);
    let epoch = *sender.borrow();
    epoch
}

fn advance(sender: &watch::Sender<u64>) {
    let next = (*sender.borrow()).saturating_add(1);
    sender.send_replace(next);
}
