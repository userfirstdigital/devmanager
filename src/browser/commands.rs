use super::replay::{BrowserReplayRepairApplyCommit, BrowserReplayRepairCaptureAuthority};
#[cfg(test)]
use super::replay::{BrowserReplayRepairCaptureReceipt, BrowserReplayRepairCapturedEvidence};
use super::replay_repair::{
    BrowserReplayRepairApplyAuthority, BrowserReplayRepairApplyReceipt,
    BrowserReplayRepairHighlightCleanup, BrowserReplayRepairHighlightToken,
    BrowserReplayRepairPreviewAbortDisposition, BrowserReplayRepairPreviewAuthority,
    BrowserReplayRepairPreviewReceipt,
};
use super::{
    BrowserAction, BrowserActionResult, BrowserActionTarget, BrowserAnnotationCandidate,
    BrowserAnnotationDetails, BrowserAnnotationDraft, BrowserAnnotationMutationResult,
    BrowserAnnotationOperation, BrowserAnnotationSummary, BrowserConsoleEntry,
    BrowserConsoleOperation, BrowserDownloadEntry, BrowserDownloadOperation, BrowserError,
    BrowserNetworkEntry, BrowserNetworkOperation, BrowserPerformanceOperation,
    BrowserPerformanceSnapshot, BrowserRecipeInputKind, BrowserRecordingStatus,
    BrowserReplayCoordinator, BrowserReplayError, BrowserReplayInstance,
    BrowserReplayRepairCandidate, BrowserReplayRepairInstance, BrowserReplaySecretLease,
    BrowserResourceHandle, BrowserResourceId, BrowserResourceKind, BrowserResourceStore,
    BrowserRisk, BrowserScreenshotMode, BrowserSnapshotSummary, BrowserTabSnapshot,
    BrowserUploadResult, BrowserViewport, BrowserWaitCondition, BrowserWaitResult,
    BrowserWorkspaceKey, BrowserWorkspaceMutation, BrowserWorkspaceSnapshot,
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
use std::sync::{Arc, Mutex, MutexGuard, Weak};
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BrowserRepairValidateSeal;

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
    #[serde(skip)]
    RepairHighlight {
        tab_id: String,
    },
    #[serde(skip)]
    RepairClearHighlight {
        tab_id: String,
    },
    #[serde(skip)]
    #[allow(private_interfaces)]
    RepairValidate {
        tab_id: String,
        _seal: BrowserRepairValidateSeal,
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
            Self::RepairHighlight { .. } => "repairHighlight",
            Self::RepairClearHighlight { .. } => "repairClearHighlight",
            Self::RepairValidate { .. } => "repairValidate",
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
            | Self::Cdp { tab_id, .. }
            | Self::RepairHighlight { tab_id }
            | Self::RepairClearHighlight { tab_id }
            | Self::RepairValidate { tab_id, .. } => Some(tab_id),
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

fn interrupt_replay_for_control(
    coordinator: &BrowserReplayCoordinator,
    control: &BrowserHostControl,
) {
    match control {
        BrowserHostControl::InterruptProject { project_id } => {
            coordinator.interrupt_project(project_id);
        }
        BrowserHostControl::InterruptWorkspace { workspace_key }
        | BrowserHostControl::InterruptTab { workspace_key, .. } => {
            coordinator.interrupt_workspace(workspace_key);
        }
    }
}

fn apply_lifecycle_control(
    response_linearization: &Mutex<()>,
    cancellations: &CancellationEpochs,
    coordinator: &BrowserReplayCoordinator,
    control: &BrowserHostControl,
) {
    apply_lifecycle_control_with_hook(
        response_linearization,
        cancellations,
        coordinator,
        control,
        || {},
    );
}

fn apply_lifecycle_control_with_hook(
    response_linearization: &Mutex<()>,
    cancellations: &CancellationEpochs,
    coordinator: &BrowserReplayCoordinator,
    control: &BrowserHostControl,
    after_lock: impl FnOnce(),
) {
    let _response_order = lock(response_linearization);
    after_lock();
    interrupt_replay_for_control(coordinator, control);
    cancellations.interrupt_control(control);
}

fn apply_host_event(
    response_linearization: &Mutex<()>,
    cancellations: &CancellationEpochs,
    coordinator: &BrowserReplayCoordinator,
    event: &BrowserHostEvent,
) {
    if let BrowserHostEvent::UserInput {
        workspace_key,
        tab_id,
        ..
    } = event
    {
        apply_lifecycle_control(
            response_linearization,
            cancellations,
            coordinator,
            &BrowserHostControl::InterruptTab {
                workspace_key: workspace_key.clone(),
                tab_id: tab_id.clone(),
            },
        );
    }
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

struct BrowserReplaySecretSidecar {
    expected_instance: BrowserReplayInstance,
    lease: BrowserReplaySecretLease,
}

struct BrowserReplayRepairRetentionSidecar {
    authority: BrowserReplayRepairCaptureAuthority,
}

enum BrowserReplayRepairPreviewSidecar {
    Highlight {
        authority: BrowserReplayRepairPreviewAuthority,
    },
    Apply {
        authority: BrowserReplayRepairApplyAuthority,
    },
}

const MAX_BROWSER_REPAIR_HIGHLIGHT_CLEANUPS: usize = 64;

#[derive(Clone)]
pub(crate) struct BrowserReplayRepairCleanupAdmission {
    _inner: Arc<BrowserReplayRepairCleanupAdmissionInner>,
}

struct BrowserReplayRepairCleanupAdmissionInner {
    queue: Weak<HostControlQueue>,
}

impl Drop for BrowserReplayRepairCleanupAdmissionInner {
    fn drop(&mut self) {
        if let Some(queue) = self.queue.upgrade() {
            queue.release_repair_cleanup_admission();
        }
    }
}

#[derive(Clone)]
pub(crate) struct BrowserReplayRepairCleanupWork {
    token: BrowserReplayRepairHighlightToken,
    restore: Option<BrowserReplayRepairHighlightToken>,
    context: BrowserInvocationContext,
    started_at: String,
    enqueued_at: Instant,
    _admission: BrowserReplayRepairCleanupAdmission,
}

impl BrowserReplayRepairCleanupWork {
    fn new(
        token: BrowserReplayRepairHighlightToken,
        restore: Option<BrowserReplayRepairHighlightToken>,
        actor: BrowserInvocationActor,
        admission: BrowserReplayRepairCleanupAdmission,
    ) -> Self {
        let context = BrowserInvocationContext::for_actor(
            actor,
            "clear replay repair preview highlight",
            BrowserRisk::Normal,
        )
        .expect("fixed replay repair cleanup context is valid");
        Self {
            token,
            restore,
            context,
            started_at: OffsetDateTime::now_utc()
                .format(&Rfc3339)
                .unwrap_or_else(|_| "unknown".to_string()),
            enqueued_at: Instant::now(),
            _admission: admission,
        }
    }

    pub(crate) fn workspace_key(&self) -> &BrowserWorkspaceKey {
        self.token.repair().workspace_key()
    }

    pub(crate) fn tab_id(&self) -> &str {
        self.token.tab_id()
    }

    pub(crate) fn token(&self) -> &BrowserReplayRepairHighlightToken {
        &self.token
    }

    pub(crate) fn restore(&self) -> Option<&BrowserReplayRepairHighlightToken> {
        self.restore.as_ref()
    }

    pub(crate) fn context(&self) -> &BrowserInvocationContext {
        &self.context
    }

    pub(crate) fn started_at(&self) -> &str {
        &self.started_at
    }

    pub(crate) fn elapsed_ms(&self) -> u64 {
        u64::try_from(self.enqueued_at.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    pub(crate) fn enqueued_at(&self) -> Instant {
        self.enqueued_at
    }

    fn clear_exact_only(&mut self) {
        self.restore = None;
    }
}

struct BrowserReplayRepairRequestGuard {
    coordinator: BrowserReplayCoordinator,
    repair: BrowserReplayRepairInstance,
    armed: bool,
}

impl BrowserReplayRepairRequestGuard {
    fn new(coordinator: &BrowserReplayCoordinator, repair: &BrowserReplayRepairInstance) -> Self {
        Self {
            coordinator: coordinator.clone(),
            repair: repair.clone(),
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for BrowserReplayRepairRequestGuard {
    fn drop(&mut self) {
        if self.armed {
            self.coordinator.abort_locator_repair_capture(&self.repair);
        }
    }
}

struct BrowserReplayRepairPreviewRequestGuard {
    coordinator: BrowserReplayCoordinator,
    host_controls: Arc<HostControlQueue>,
    authority: BrowserReplayRepairPreviewAuthority,
    actor: BrowserInvocationActor,
    admission: BrowserReplayRepairCleanupAdmission,
    armed: bool,
}

impl BrowserReplayRepairPreviewRequestGuard {
    fn new(
        coordinator: &BrowserReplayCoordinator,
        controller: &BrowserController,
        authority: BrowserReplayRepairPreviewAuthority,
        actor: BrowserInvocationActor,
        admission: BrowserReplayRepairCleanupAdmission,
    ) -> Self {
        Self {
            coordinator: coordinator.clone(),
            host_controls: Arc::clone(&controller.host_controls),
            authority,
            actor,
            admission,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for BrowserReplayRepairPreviewRequestGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let restore = match self
            .coordinator
            .abort_locator_repair_preview(&self.authority)
        {
            BrowserReplayRepairPreviewAbortDisposition::RestorePrevious => {
                self.authority.expected_previous_token().cloned()
            }
            BrowserReplayRepairPreviewAbortDisposition::ClearExactOnly => None,
        };
        self.host_controls.enqueue_repair_cleanup(
            self.authority.token().clone(),
            self.actor,
            restore,
            self.admission.clone(),
        );
    }
}

struct BrowserReplayRepairApplyRequestGuard {
    coordinator: BrowserReplayCoordinator,
    authority: BrowserReplayRepairApplyAuthority,
    armed: bool,
}

impl BrowserReplayRepairApplyRequestGuard {
    fn new(
        coordinator: &BrowserReplayCoordinator,
        authority: BrowserReplayRepairApplyAuthority,
    ) -> Self {
        Self {
            coordinator: coordinator.clone(),
            authority,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for BrowserReplayRepairApplyRequestGuard {
    fn drop(&mut self) {
        if self.armed {
            self.coordinator.abort_locator_repair_apply(&self.authority);
        }
    }
}

struct BrowserCommandEnvelope {
    workspace_key: BrowserWorkspaceKey,
    command: BrowserCommand,
    context: BrowserInvocationContext,
    local_project_root: Option<PathBuf>,
    cancellation_ticket: CancellationTicket,
    registration_lease: Option<BrowserRegistrationLease>,
    replay_secret_sidecar: Option<BrowserReplaySecretSidecar>,
    replay_repair_sidecar: Option<BrowserReplayRepairRetentionSidecar>,
    replay_repair_preview_sidecar: Option<BrowserReplayRepairPreviewSidecar>,
    response: oneshot::Sender<Result<BrowserResponse, BrowserError>>,
    pending_work: PendingWorkGuard,
}

#[derive(Clone)]
pub struct BrowserCommandBridge {
    sender: mpsc::Sender<BrowserCommandEnvelope>,
    cancellations: Arc<CancellationEpochs>,
    host_controls: Arc<HostControlQueue>,
    response_linearization: Arc<Mutex<()>>,
    pending_work: Arc<PendingWork>,
    replay_coordinator: BrowserReplayCoordinator,
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
            response_linearization: Arc::clone(&self.response_linearization),
            pending_work: Arc::clone(&self.pending_work),
            registration_lease,
            replay_coordinator: self.replay_coordinator.clone(),
        }
    }

    pub fn replay_coordinator(&self) -> BrowserReplayCoordinator {
        self.replay_coordinator.clone()
    }

    pub fn pending_work_count(&self) -> usize {
        self.pending_work.count()
    }

    pub fn observe_host_event(&self, event: &BrowserHostEvent) {
        self.host_controls.with_locked(|| {
            apply_host_event(
                &self.response_linearization,
                &self.cancellations,
                &self.replay_coordinator,
                event,
            )
        });
    }

    pub fn interrupt_workspace(&self, workspace_key: &BrowserWorkspaceKey) {
        self.interrupt_control_with_linearization_hook(
            BrowserHostControl::InterruptWorkspace {
                workspace_key: workspace_key.clone(),
            },
            || {},
        );
    }

    pub(crate) fn revoke_registration(
        &self,
        workspace_key: &BrowserWorkspaceKey,
        registration_lease: &BrowserRegistrationLease,
    ) {
        self.revoke_registration_with_linearization_hook(workspace_key, registration_lease, || {});
    }

    fn revoke_registration_with_linearization_hook(
        &self,
        workspace_key: &BrowserWorkspaceKey,
        registration_lease: &BrowserRegistrationLease,
        after_lock: impl FnOnce(),
    ) {
        let control = BrowserHostControl::InterruptWorkspace {
            workspace_key: workspace_key.clone(),
        };
        self.host_controls.push_and(control.clone(), || {
            let _response_order = lock(&self.response_linearization);
            after_lock();
            interrupt_replay_for_control(&self.replay_coordinator, &control);
            registration_lease.revoke();
            self.cancellations.interrupt_control(&control);
        });
    }

    pub fn interrupt_project(&self, project_id: &str) {
        self.interrupt_control_with_linearization_hook(
            BrowserHostControl::InterruptProject {
                project_id: project_id.to_string(),
            },
            || {},
        );
    }

    pub fn interrupt_tab(&self, workspace_key: &BrowserWorkspaceKey, tab_id: &str) {
        self.interrupt_control_with_linearization_hook(
            BrowserHostControl::InterruptTab {
                workspace_key: workspace_key.clone(),
                tab_id: tab_id.to_string(),
            },
            || {},
        );
    }

    fn interrupt_control_with_linearization_hook(
        &self,
        control: BrowserHostControl,
        after_lock: impl FnOnce(),
    ) {
        self.host_controls.push_and(control.clone(), || {
            apply_lifecycle_control_with_hook(
                &self.response_linearization,
                &self.cancellations,
                &self.replay_coordinator,
                &control,
                after_lock,
            )
        });
    }

    pub fn interrupt_all(&self) {
        self.host_controls.with_locked(|| {
            let _response_order = lock(&self.response_linearization);
            self.replay_coordinator.interrupt_all();
            self.cancellations.interrupt_all();
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
                    apply_lifecycle_control(
                        &self.response_linearization,
                        &self.cancellations,
                        &self.replay_coordinator,
                        &control,
                    );
                }
                let lifecycle_requests = lifecycle_requests
                    .into_iter()
                    .map(|envelope| {
                        BrowserCommandRequest::from_envelope(
                            envelope,
                            Arc::clone(&self.cancellations),
                            Arc::clone(&self.response_linearization),
                        )
                    })
                    .collect();
                apply(controls, lifecycle_requests)
            })
    }

    pub(crate) fn with_locked_host_work_for_command<R>(
        &self,
        workspace_key: &BrowserWorkspaceKey,
        command: &BrowserCommand,
        apply: impl FnOnce(
            Vec<BrowserHostControl>,
            Vec<BrowserCommandRequest>,
            Vec<BrowserReplayRepairCleanupWork>,
        ) -> R,
    ) -> R {
        self.host_controls
            .with_drain_all_locked(|controls, lifecycle_requests, repair_cleanups| {
                if let Some(control) = browser_lifecycle_control(workspace_key, command) {
                    apply_lifecycle_control(
                        &self.response_linearization,
                        &self.cancellations,
                        &self.replay_coordinator,
                        &control,
                    );
                }
                let lifecycle_requests = lifecycle_requests
                    .into_iter()
                    .map(|envelope| {
                        BrowserCommandRequest::from_envelope(
                            envelope,
                            Arc::clone(&self.cancellations),
                            Arc::clone(&self.response_linearization),
                        )
                    })
                    .collect();
                apply(controls, lifecycle_requests, repair_cleanups)
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
                            Arc::clone(&self.response_linearization),
                        )
                    })
                    .collect();
                apply(controls, lifecycle_requests)
            })
    }

    pub(crate) fn with_locked_host_work_and_repair_cleanups<R>(
        &self,
        apply: impl FnOnce(
            Vec<BrowserHostControl>,
            Vec<BrowserCommandRequest>,
            Vec<BrowserReplayRepairCleanupWork>,
        ) -> R,
    ) -> R {
        self.host_controls
            .with_drain_all_locked(|controls, lifecycle_requests, repair_cleanups| {
                let lifecycle_requests = lifecycle_requests
                    .into_iter()
                    .map(|envelope| {
                        BrowserCommandRequest::from_envelope(
                            envelope,
                            Arc::clone(&self.cancellations),
                            Arc::clone(&self.response_linearization),
                        )
                    })
                    .collect();
                apply(controls, lifecycle_requests, repair_cleanups)
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
    response_linearization: Arc<Mutex<()>>,
    pending_work: Arc<PendingWork>,
    registration_lease: Option<BrowserRegistrationLease>,
    replay_coordinator: BrowserReplayCoordinator,
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
        self.request_with_context_and_local_project_root(command, context, None, None, None, None)
            .await
    }

    pub(crate) async fn request_with_local_project_root(
        &self,
        command: BrowserCommand,
        context: BrowserInvocationContext,
        local_project_root: &std::path::Path,
    ) -> Result<BrowserResponse, BrowserError> {
        let canonical = verified_authenticated_local_project_root(local_project_root)?;
        self.request_with_context_and_local_project_root(
            command,
            context,
            Some(canonical),
            None,
            None,
            None,
        )
        .await
    }

    #[allow(dead_code)] // The checkpoint-9 replay executor consumes this secure lane in Task 4.
    pub(crate) async fn request_replay_secret_type(
        &self,
        command: BrowserCommand,
        context: BrowserInvocationContext,
        expected_instance: BrowserReplayInstance,
        lease: BrowserReplaySecretLease,
    ) -> Result<BrowserResponse, BrowserError> {
        validate_secret_command_authority(
            &self.workspace_key,
            &command,
            &context,
            &expected_instance,
            &lease,
        )?;
        self.request_with_context_and_local_project_root(
            command,
            context,
            None,
            Some(BrowserReplaySecretSidecar {
                expected_instance,
                lease,
            }),
            None,
            None,
        )
        .await
    }

    pub(crate) async fn request_replay_repair_capture(
        &self,
        coordinator: &BrowserReplayCoordinator,
        repair: &BrowserReplayRepairInstance,
        command: BrowserCommand,
        context: BrowserInvocationContext,
    ) -> Result<BrowserResponse, BrowserError> {
        let kind = repair_capture_kind(&command).ok_or_else(invalid_repair_sidecar)?;
        if context.actor != BrowserInvocationActor::Agent
            || repair.workspace_key() != &self.workspace_key
            || command.tab_id().is_none()
        {
            return Err(invalid_repair_sidecar());
        }
        let (authority, receipt) = coordinator
            .issue_locator_repair_capture_authority(repair, kind)
            .map_err(|_| invalid_repair_sidecar())?;
        let mut guard = BrowserReplayRepairRequestGuard::new(coordinator, repair);
        if command.tab_id() != Some(authority.tab_id()) {
            return Err(invalid_repair_sidecar());
        }
        let expected_tab_id = authority.tab_id().to_string();
        let expected_revision = authority.revision();
        let response = self
            .request_with_context_and_local_project_root(
                command,
                context,
                None,
                None,
                Some(BrowserReplayRepairRetentionSidecar { authority }),
                None,
            )
            .await;
        let response = match response {
            Ok(response) => response,
            Err(error) => return Err(contain_repair_capture_error(error)),
        };
        let handle = match (&response, kind) {
            (
                BrowserResponse::Snapshot { summary, resource },
                BrowserResourceKind::ReplayRepairSnapshot,
            ) if summary.tab_id == expected_tab_id && summary.revision == expected_revision => {
                resource
            }
            (
                BrowserResponse::Screenshot { resource },
                BrowserResourceKind::ReplayRepairScreenshot,
            ) => resource,
            _ => return Err(invalid_repair_sidecar()),
        };
        let evidence = receipt
            .consume_exact(repair, kind, handle)
            .ok_or_else(invalid_repair_sidecar)?;
        if coordinator
            .record_locator_repair_evidence(evidence)
            .is_err()
        {
            return Err(invalid_repair_sidecar());
        }
        guard.disarm();
        Ok(response)
    }

    pub async fn request_replay_repair_preview(
        &self,
        coordinator: &BrowserReplayCoordinator,
        repair: &BrowserReplayRepairInstance,
        candidate: BrowserReplayRepairCandidate,
        actor: BrowserInvocationActor,
    ) -> Result<super::BrowserReplayRepairProjection, BrowserError> {
        if !matches!(
            actor,
            BrowserInvocationActor::User | BrowserInvocationActor::Agent
        ) || repair.workspace_key() != &self.workspace_key
        {
            return Err(invalid_repair_preview_sidecar());
        }
        let admission = self
            .host_controls
            .try_admit_repair_cleanup()
            .ok_or(BrowserError::ResourceRootBusy)?;
        let context = BrowserInvocationContext::for_actor(
            actor,
            "preview replay repair locator",
            BrowserRisk::Normal,
        )
        .map_err(|_| invalid_repair_preview_sidecar())?;
        let (authority, receipt): (
            BrowserReplayRepairPreviewAuthority,
            BrowserReplayRepairPreviewReceipt,
        ) = coordinator
            .reserve_locator_repair_preview(repair, candidate)
            .map_err(|_| invalid_repair_preview_sidecar())?;
        let mut guard = BrowserReplayRepairPreviewRequestGuard::new(
            coordinator,
            self,
            authority.clone(),
            actor,
            admission.clone(),
        );
        let response = self
            .request_with_context_and_local_project_root(
                BrowserCommand::RepairHighlight {
                    tab_id: authority.tab_id().to_string(),
                },
                context,
                None,
                None,
                None,
                Some(BrowserReplayRepairPreviewSidecar::Highlight {
                    authority: authority.clone(),
                }),
            )
            .await
            .map_err(contain_repair_preview_error)?;
        if response != BrowserResponse::Acknowledged {
            return Err(invalid_repair_preview_sidecar());
        }
        let acknowledgement = receipt
            .consume_exact(repair)
            .ok_or_else(invalid_repair_preview_sidecar)?;
        let cleanup_queue = Arc::clone(&self.host_controls);
        let cleanup_token = acknowledgement.token.clone();
        let projection = coordinator
            .commit_locator_repair_preview(acknowledgement, move || {
                BrowserReplayRepairHighlightCleanup::new(move || {
                    cleanup_queue.enqueue_repair_cleanup(cleanup_token, actor, None, admission);
                })
            })
            .map_err(|_| invalid_repair_preview_sidecar())?;
        guard.disarm();
        Ok(projection)
    }

    pub(crate) async fn request_replay_repair_apply(
        &self,
        coordinator: &BrowserReplayCoordinator,
        repair: &BrowserReplayRepairInstance,
        confirmed: bool,
        resume: bool,
        context: BrowserInvocationContext,
    ) -> Result<BrowserReplayRepairApplyCommit, BrowserError> {
        self.request_replay_repair_apply_with_post_context_factory(
            coordinator,
            repair,
            confirmed,
            resume,
            context,
            |actor| {
                BrowserInvocationContext::for_actor(
                    actor,
                    "validate applied replay repair locator before resume",
                    BrowserRisk::Normal,
                )
            },
        )
        .await
    }

    async fn request_replay_repair_apply_with_post_context_factory<F>(
        &self,
        coordinator: &BrowserReplayCoordinator,
        repair: &BrowserReplayRepairInstance,
        confirmed: bool,
        resume: bool,
        context: BrowserInvocationContext,
        post_context_factory: F,
    ) -> Result<BrowserReplayRepairApplyCommit, BrowserError>
    where
        F: FnOnce(BrowserInvocationActor) -> Result<BrowserInvocationContext, BrowserError>,
    {
        if repair.workspace_key() != &self.workspace_key {
            return Err(invalid_repair_apply_sidecar());
        }
        let repair_phase = coordinator
            .locator_repair_status(repair)
            .map_err(contain_repair_apply_replay_error)?
            .phase;
        if repair_phase == super::BrowserReplayRepairPhase::Applied && !resume {
            return Err(BrowserError::InvalidInvocation {
                field: "resume".to_string(),
            });
        }
        let (authority, receipt): (
            BrowserReplayRepairApplyAuthority,
            BrowserReplayRepairApplyReceipt,
        ) = coordinator
            .reserve_locator_repair_apply(repair, confirmed, &context)
            .map_err(contain_repair_apply_replay_error)?;
        let mut guard = BrowserReplayRepairApplyRequestGuard::new(coordinator, authority.clone());
        let response = self
            .request_with_context_and_local_project_root(
                BrowserCommand::RepairValidate {
                    tab_id: authority.token().tab_id().to_string(),
                    _seal: BrowserRepairValidateSeal,
                },
                context.clone(),
                None,
                None,
                None,
                Some(BrowserReplayRepairPreviewSidecar::Apply {
                    authority: authority.clone(),
                }),
            )
            .await
            .map_err(contain_repair_apply_error)?;
        if response != BrowserResponse::Acknowledged {
            return Err(invalid_repair_apply_sidecar());
        }
        let acknowledgement = receipt
            .consume_exact(repair)
            .ok_or_else(invalid_repair_apply_sidecar)?;
        let post_context =
            post_context_factory(context.actor).map_err(|_| invalid_repair_apply_sidecar())?;
        let mut commit = coordinator
            .commit_locator_repair_apply(acknowledgement)
            .map_err(contain_repair_apply_replay_error)?;
        guard.disarm();
        if !commit.recipe_written {
            return Ok(commit);
        }

        let (post_authority, post_receipt): (
            BrowserReplayRepairApplyAuthority,
            BrowserReplayRepairApplyReceipt,
        ) = match coordinator.reserve_locator_repair_post_commit_validation(repair, &post_context) {
            Ok(reservation) => reservation,
            Err(_) => {
                if let Ok(projection) = coordinator.status(repair.replay()) {
                    commit.replay = projection;
                }
                return Ok(commit);
            }
        };
        let mut post_guard =
            BrowserReplayRepairApplyRequestGuard::new(coordinator, post_authority.clone());
        let post_response = self
            .request_with_context_and_local_project_root(
                BrowserCommand::RepairValidate {
                    tab_id: post_authority.token().tab_id().to_string(),
                    _seal: BrowserRepairValidateSeal,
                },
                post_context,
                None,
                None,
                None,
                Some(BrowserReplayRepairPreviewSidecar::Apply {
                    authority: post_authority.clone(),
                }),
            )
            .await;
        let post_acknowledgement = match post_response {
            Ok(BrowserResponse::Acknowledged) => post_receipt.consume_exact(repair),
            Ok(_) | Err(_) => None,
        };
        let Some(post_acknowledgement) = post_acknowledgement else {
            coordinator.abort_locator_repair_apply(&post_authority);
            post_guard.disarm();
            if let Ok(projection) = coordinator.status(repair.replay()) {
                commit.replay = projection;
            }
            return Ok(commit);
        };
        match coordinator.complete_locator_repair_post_commit_validation(
            post_acknowledgement,
            &mut commit,
            resume,
        ) {
            Ok(()) => {
                post_guard.disarm();
                Ok(commit)
            }
            Err(_) => {
                coordinator.abort_locator_repair_apply(&post_authority);
                post_guard.disarm();
                if let Ok(projection) = coordinator.status(repair.replay()) {
                    commit.replay = projection;
                }
                Ok(commit)
            }
        }
    }

    async fn request_with_context_and_local_project_root(
        &self,
        command: BrowserCommand,
        context: BrowserInvocationContext,
        local_project_root: Option<PathBuf>,
        replay_secret_sidecar: Option<BrowserReplaySecretSidecar>,
        replay_repair_sidecar: Option<BrowserReplayRepairRetentionSidecar>,
        replay_repair_preview_sidecar: Option<BrowserReplayRepairPreviewSidecar>,
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
                replay_secret_sidecar,
                replay_repair_sidecar,
                replay_repair_preview_sidecar,
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
            biased;
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
                replay_secret_sidecar: None,
                replay_repair_sidecar: None,
                replay_repair_preview_sidecar: None,
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
            apply_lifecycle_control(
                &self.response_linearization,
                &self.cancellations,
                &self.replay_coordinator,
                &control,
            )
        });
    }

    pub fn interrupt_tab(&self, tab_id: &str) {
        let control = BrowserHostControl::InterruptTab {
            workspace_key: self.workspace_key.clone(),
            tab_id: tab_id.to_string(),
        };
        self.host_controls.push_and(control.clone(), || {
            apply_lifecycle_control(
                &self.response_linearization,
                &self.cancellations,
                &self.replay_coordinator,
                &control,
            )
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
                apply_lifecycle_control(
                    &self.response_linearization,
                    &self.cancellations,
                    &self.replay_coordinator,
                    &control,
                );
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
                    replay_secret_sidecar: None,
                    replay_repair_sidecar: None,
                    replay_repair_preview_sidecar: None,
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
    expected_instance: &BrowserReplayInstance,
    lease: &BrowserReplaySecretLease,
) -> Result<(), BrowserError> {
    let BrowserCommand::SecretType { input_name, .. } = command else {
        return Err(invalid_secret_sidecar());
    };
    if context.actor != BrowserInvocationActor::Agent
        || expected_instance.workspace_key() != workspace_key
        || !lease.authorizes(expected_instance, input_name)
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

fn repair_capture_kind(command: &BrowserCommand) -> Option<BrowserResourceKind> {
    match command {
        BrowserCommand::Snapshot { .. } => Some(BrowserResourceKind::ReplayRepairSnapshot),
        BrowserCommand::Screenshot {
            mode: BrowserScreenshotMode::Viewport,
            ..
        } => Some(BrowserResourceKind::ReplayRepairScreenshot),
        _ => None,
    }
}

fn invalid_repair_sidecar() -> BrowserError {
    BrowserError::InvalidInvocation {
        field: "repairSidecar".to_string(),
    }
}

fn invalid_repair_preview_sidecar() -> BrowserError {
    BrowserError::InvalidInvocation {
        field: "repairPreviewSidecar".to_string(),
    }
}

fn invalid_repair_apply_sidecar() -> BrowserError {
    BrowserError::InvalidInvocation {
        field: "repairApplySidecar".to_string(),
    }
}

fn contain_repair_capture_error(error: BrowserError) -> BrowserError {
    match error {
        safe @ BrowserError::Interrupted
        | safe @ BrowserError::ResourceTooLarge { .. }
        | safe @ BrowserError::StaleReference { .. }
        | safe @ BrowserError::LocatorNotFound { .. }
        | safe @ BrowserError::ResourceRootBusy
        | safe @ BrowserError::ResourceRootUnavailable => safe,
        BrowserError::Timeout { operation }
            if matches!(operation.as_str(), "snapshot" | "screenshot") =>
        {
            BrowserError::Timeout { operation }
        }
        BrowserError::InvalidInvocation { field } if field == "repairSidecar" => {
            BrowserError::InvalidInvocation { field }
        }
        BrowserError::UnavailablePlatform { .. } => BrowserError::UnavailablePlatform {
            platform: std::env::consts::OS.to_string(),
        },
        _ => BrowserError::ResourceRootUnavailable,
    }
}

fn contain_repair_preview_error(error: BrowserError) -> BrowserError {
    match error {
        safe @ BrowserError::Interrupted
        | safe @ BrowserError::StaleReference { .. }
        | safe @ BrowserError::LocatorNotFound { .. } => safe,
        BrowserError::Timeout { .. } => BrowserError::Timeout {
            operation: "repairHighlight".to_string(),
        },
        BrowserError::UnavailablePlatform { .. } => BrowserError::UnavailablePlatform {
            platform: std::env::consts::OS.to_string(),
        },
        BrowserError::InvalidInvocation { field } if field == "repairPreviewSidecar" => {
            BrowserError::InvalidInvocation { field }
        }
        _ => invalid_repair_preview_sidecar(),
    }
}

fn contain_repair_apply_error(error: BrowserError) -> BrowserError {
    match error {
        safe @ BrowserError::Interrupted
        | safe @ BrowserError::StaleReference { .. }
        | safe @ BrowserError::LocatorNotFound { .. }
        | safe @ BrowserError::BlockedPermission { .. } => safe,
        BrowserError::Timeout { .. } => BrowserError::Timeout {
            operation: "repairValidate".to_string(),
        },
        BrowserError::UnavailablePlatform { .. } => BrowserError::UnavailablePlatform {
            platform: std::env::consts::OS.to_string(),
        },
        BrowserError::InvalidInvocation { field } if field == "repairApplySidecar" => {
            BrowserError::InvalidInvocation { field }
        }
        _ => invalid_repair_apply_sidecar(),
    }
}

fn contain_repair_apply_replay_error(error: BrowserReplayError) -> BrowserError {
    match error {
        BrowserReplayError::RepairConfirmationRequired => BrowserError::InvalidInvocation {
            field: "confirm".to_string(),
        },
        BrowserReplayError::RecipeRootUnavailable => BrowserError::ResourceRootUnavailable,
        BrowserReplayError::RepairRecipeChanged => BrowserError::InvalidRecipe {
            message: "repair recipe changed before apply".to_string(),
        },
        BrowserReplayError::RepairCandidateInvalid => BrowserError::InvalidRecipe {
            message: "repair candidate is no longer valid".to_string(),
        },
        BrowserReplayError::RepairWriteFailed => BrowserError::Io {
            operation: "write repaired browser workflow".to_string(),
            path: PathBuf::new(),
            message: "atomic repair write failed".to_string(),
        },
        BrowserReplayError::TerminalState | BrowserReplayError::StaleInstance => {
            BrowserError::Interrupted
        }
        _ => invalid_repair_apply_sidecar(),
    }
}

pub(crate) fn validate_direct_secret_command(command: &BrowserCommand) -> Result<(), BrowserError> {
    if matches!(command, BrowserCommand::SecretType { .. }) {
        return Err(invalid_secret_sidecar());
    }
    Ok(())
}

pub(crate) fn validate_direct_repair_preview_command(
    command: &BrowserCommand,
) -> Result<(), BrowserError> {
    if matches!(
        command,
        BrowserCommand::RepairHighlight { .. }
            | BrowserCommand::RepairClearHighlight { .. }
            | BrowserCommand::RepairValidate { .. }
    ) {
        return Err(invalid_repair_preview_sidecar());
    }
    Ok(())
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
    response_linearization: Arc<Mutex<()>>,
    pending_work: Arc<PendingWork>,
    replay_coordinator: BrowserReplayCoordinator,
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
                    Arc::clone(&self.response_linearization),
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
            apply_lifecycle_control(
                &self.response_linearization,
                &self.cancellations,
                &self.replay_coordinator,
                &control,
            )
        });
    }

    pub fn interrupt_tab(&self, workspace_key: &BrowserWorkspaceKey, tab_id: &str) {
        let control = BrowserHostControl::InterruptTab {
            workspace_key: workspace_key.clone(),
            tab_id: tab_id.to_string(),
        };
        self.host_controls.push_and(control.clone(), || {
            apply_lifecycle_control(
                &self.response_linearization,
                &self.cancellations,
                &self.replay_coordinator,
                &control,
            )
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
                            Arc::clone(&self.response_linearization),
                        )
                    })
                    .collect();
                apply(controls, lifecycle_requests)
            })
    }

    pub fn observe_host_event(&self, event: &BrowserHostEvent) {
        self.host_controls.with_locked(|| {
            apply_host_event(
                &self.response_linearization,
                &self.cancellations,
                &self.replay_coordinator,
                event,
            )
        });
    }
}

pub struct BrowserCommandRequest {
    workspace_key: BrowserWorkspaceKey,
    command: BrowserCommand,
    context: BrowserInvocationContext,
    local_project_root: Option<PathBuf>,
    cancellation_ticket: CancellationTicket,
    cancellations: Arc<CancellationEpochs>,
    response_linearization: Arc<Mutex<()>>,
    registration_lease: Option<BrowserRegistrationLease>,
    replay_secret_sidecar: Option<BrowserReplaySecretSidecar>,
    replay_repair_sidecar: Option<BrowserReplayRepairRetentionSidecar>,
    replay_repair_preview_sidecar: Option<BrowserReplayRepairPreviewSidecar>,
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
        match (&self.command, &self.replay_secret_sidecar) {
            (BrowserCommand::SecretType { input_name, .. }, Some(sidecar))
                if self.context.actor == BrowserInvocationActor::Agent
                    && sidecar.expected_instance.workspace_key() == &self.workspace_key
                    && sidecar
                        .lease
                        .authorizes(&sidecar.expected_instance, input_name) =>
            {
                Ok(Some(&sidecar.lease))
            }
            (BrowserCommand::SecretType { .. }, _) | (_, Some(_)) => Err(invalid_secret_sidecar()),
            (_, None) => Ok(None),
        }
    }

    pub(crate) fn validate_repair_retention_sidecar(
        &self,
    ) -> Result<Option<&BrowserReplayRepairCaptureAuthority>, BrowserError> {
        let Some(sidecar) = &self.replay_repair_sidecar else {
            return Ok(None);
        };
        let authority = &sidecar.authority;
        if self.context.actor != BrowserInvocationActor::Agent
            || authority.repair().workspace_key() != &self.workspace_key
            || self.command.tab_id() != Some(authority.tab_id())
            || repair_capture_kind(&self.command) != Some(authority.kind())
            || !authority.is_live()
        {
            return Err(invalid_repair_sidecar());
        }
        Ok(Some(authority))
    }

    pub(crate) fn validate_repair_preview_sidecar(&self) -> Result<(), BrowserError> {
        match (&self.command, &self.replay_repair_preview_sidecar) {
            (
                BrowserCommand::RepairHighlight { tab_id },
                Some(BrowserReplayRepairPreviewSidecar::Highlight { authority }),
            ) if matches!(
                self.context.actor,
                BrowserInvocationActor::User | BrowserInvocationActor::Agent
            ) && authority.repair().workspace_key() == &self.workspace_key
                && authority.tab_id() == tab_id
                && authority.token().tab_id() == tab_id
                && authority.is_live() =>
            {
                Ok(())
            }
            (
                BrowserCommand::RepairValidate { .. },
                Some(BrowserReplayRepairPreviewSidecar::Apply { .. }),
            ) => Ok(()),
            (BrowserCommand::RepairHighlight { .. }, _)
            | (BrowserCommand::RepairClearHighlight { .. }, _)
            | (_, Some(BrowserReplayRepairPreviewSidecar::Highlight { .. })) => {
                Err(invalid_repair_preview_sidecar())
            }
            _ => Ok(()),
        }
    }

    pub(crate) fn validate_repair_apply_sidecar(&self) -> Result<(), BrowserError> {
        match (&self.command, &self.replay_repair_preview_sidecar) {
            (
                BrowserCommand::RepairValidate { tab_id, .. },
                Some(BrowserReplayRepairPreviewSidecar::Apply { authority }),
            ) if matches!(
                self.context.actor,
                BrowserInvocationActor::User | BrowserInvocationActor::Agent
            ) && self.context.actor == authority.actor()
                && self.context.operation_id == authority.operation_id()
                && authority.repair().workspace_key() == &self.workspace_key
                && authority.token().tab_id() == tab_id
                && authority.revision() == authority.candidate().element_ref().revision
                && authority.is_live() =>
            {
                Ok(())
            }
            (BrowserCommand::RepairValidate { .. }, _)
            | (_, Some(BrowserReplayRepairPreviewSidecar::Apply { .. })) => {
                Err(invalid_repair_apply_sidecar())
            }
            _ => Ok(()),
        }
    }

    pub(crate) fn repair_preview_highlight_authority(
        &self,
    ) -> Option<&BrowserReplayRepairPreviewAuthority> {
        match &self.replay_repair_preview_sidecar {
            Some(BrowserReplayRepairPreviewSidecar::Highlight { authority }) => Some(authority),
            _ => None,
        }
    }

    pub(crate) fn repair_apply_authority(&self) -> Option<&BrowserReplayRepairApplyAuthority> {
        match &self.replay_repair_preview_sidecar {
            Some(BrowserReplayRepairPreviewSidecar::Apply { authority }) => Some(authority),
            _ => None,
        }
    }

    pub(crate) fn records_workflow_recipe_action(&self) -> bool {
        self.context.actor == BrowserInvocationActor::Agent
            && self.replay_repair_sidecar.is_none()
            && self.replay_repair_preview_sidecar.is_none()
    }

    pub(crate) fn retain_repair_resource(
        &self,
        store: &BrowserResourceStore,
        kind: BrowserResourceKind,
        mime_type: &str,
        bytes: impl AsRef<[u8]>,
    ) -> Result<BrowserResourceHandle, BrowserError> {
        let authority = self
            .validate_repair_retention_sidecar()?
            .ok_or_else(invalid_repair_sidecar)?;
        authority.retain(
            store,
            &self.workspace_key,
            self.command.tab_id().ok_or_else(invalid_repair_sidecar)?,
            kind,
            mime_type,
            bytes,
        )
    }

    #[cfg(test)]
    fn retain_repair_resource_for_test(
        &self,
        store: &BrowserResourceStore,
        kind: BrowserResourceKind,
        mime_type: &str,
        bytes: impl AsRef<[u8]>,
    ) -> Result<BrowserResourceHandle, BrowserError> {
        self.retain_repair_resource(store, kind, mime_type, bytes)
    }

    pub(crate) fn started_at(&self) -> &str {
        &self.started_at
    }

    pub(crate) fn elapsed_ms(&self) -> u64 {
        self.started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
    }

    pub fn respond(self, result: Result<BrowserResponse, BrowserError>) {
        self.respond_with_linearization_hook(result, || {});
    }

    fn respond_with_linearization_hook(
        self,
        result: Result<BrowserResponse, BrowserError>,
        after_lock: impl FnOnce(),
    ) {
        let response_linearization = Arc::clone(&self.response_linearization);
        let _response_order = lock(&response_linearization);
        after_lock();
        let result = if self.cancellation_is_current() {
            result
        } else {
            Err(BrowserError::Interrupted)
        };
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
        response_linearization: Arc<Mutex<()>>,
    ) -> Self {
        let BrowserCommandEnvelope {
            workspace_key,
            command,
            context,
            local_project_root,
            cancellation_ticket,
            registration_lease,
            replay_secret_sidecar,
            replay_repair_sidecar,
            replay_repair_preview_sidecar,
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
            response_linearization,
            registration_lease,
            replay_secret_sidecar,
            replay_repair_sidecar,
            replay_repair_preview_sidecar,
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
    let response_linearization = Arc::new(Mutex::new(()));
    let pending_work = Arc::new(PendingWork::default());
    let replay_coordinator = BrowserReplayCoordinator::default();
    (
        BrowserCommandBridge {
            sender,
            cancellations: Arc::clone(&cancellations),
            host_controls: Arc::clone(&host_controls),
            response_linearization: Arc::clone(&response_linearization),
            pending_work: Arc::clone(&pending_work),
            replay_coordinator: replay_coordinator.clone(),
        },
        BrowserCommandInbox {
            receiver,
            cancellations,
            host_controls,
            response_linearization,
            pending_work,
            replay_coordinator,
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
pub(crate) struct HostControlQueue {
    queued: Mutex<HostPriorityQueue>,
    repair_cleanups: Mutex<VecDeque<BrowserReplayRepairCleanupWork>>,
    repair_cleanup_admissions: AtomicUsize,
}

impl HostControlQueue {
    fn try_admit_repair_cleanup(self: &Arc<Self>) -> Option<BrowserReplayRepairCleanupAdmission> {
        self.repair_cleanup_admissions
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                (current < MAX_BROWSER_REPAIR_HIGHLIGHT_CLEANUPS).then_some(current + 1)
            })
            .ok()?;
        Some(BrowserReplayRepairCleanupAdmission {
            _inner: Arc::new(BrowserReplayRepairCleanupAdmissionInner {
                queue: Arc::downgrade(self),
            }),
        })
    }

    fn release_repair_cleanup_admission(&self) {
        let previous = self
            .repair_cleanup_admissions
            .fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "repair cleanup admission underflow");
    }

    #[cfg(test)]
    pub(crate) fn hold_repair_cleanup_admission_for_test(
        self: &Arc<Self>,
    ) -> Option<BrowserReplayRepairCleanupAdmission> {
        self.try_admit_repair_cleanup()
    }

    #[cfg(test)]
    pub(crate) fn repair_cleanup_work_for_test(
        self: &Arc<Self>,
        token: BrowserReplayRepairHighlightToken,
        restore: Option<BrowserReplayRepairHighlightToken>,
        actor: BrowserInvocationActor,
    ) -> Option<BrowserReplayRepairCleanupWork> {
        Some(BrowserReplayRepairCleanupWork::new(
            token,
            restore,
            actor,
            self.try_admit_repair_cleanup()?,
        ))
    }

    #[cfg(test)]
    pub(crate) fn repair_cleanup_admission_count_for_test(&self) -> usize {
        self.repair_cleanup_admissions.load(Ordering::Acquire)
    }

    fn enqueue_repair_cleanup(
        &self,
        token: BrowserReplayRepairHighlightToken,
        actor: BrowserInvocationActor,
        restore: Option<BrowserReplayRepairHighlightToken>,
        admission: BrowserReplayRepairCleanupAdmission,
    ) {
        debug_assert!(matches!(
            actor,
            BrowserInvocationActor::User | BrowserInvocationActor::Agent
        ));
        let mut repair_cleanups = lock(&self.repair_cleanups);
        if let Some(existing) = repair_cleanups
            .iter_mut()
            .find(|cleanup| cleanup.token() == &token)
        {
            if restore.is_none() {
                existing.clear_exact_only();
            }
            return;
        }
        repair_cleanups.push_back(BrowserReplayRepairCleanupWork::new(
            token, restore, actor, admission,
        ));
    }

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

    fn with_drain_all_locked<R>(
        &self,
        apply: impl FnOnce(
            Vec<BrowserHostControl>,
            Vec<BrowserCommandEnvelope>,
            Vec<BrowserReplayRepairCleanupWork>,
        ) -> R,
    ) -> R {
        let mut queued = lock(&self.queued);
        let controls = queued.controls.drain(..).collect();
        let lifecycle_requests = queued.lifecycle_requests.drain(..).collect();
        let repair_cleanups = lock(&self.repair_cleanups).drain(..).collect();
        let result = apply(controls, lifecycle_requests, repair_cleanups);
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

    fn interrupt_all(&self) {
        for sender in lock(&self.projects).values() {
            advance(sender);
        }
        for sender in lock(&self.workspaces).values() {
            advance(sender);
        }
        for sender in lock(&self.tabs).values() {
            advance(sender);
        }
    }
}

#[cfg(test)]
mod secure_command_tests {
    use super::*;
    use crate::browser::{
        compile_browser_replay, recipe_path, save_recipe, BrowserActionTarget, BrowserElementRef,
        BrowserLocator, BrowserRecipeAction, BrowserRecipeInput, BrowserRecipeInputKind,
        BrowserRecipeLocator, BrowserRecipeStep, BrowserRecipeV1, BrowserRecipeValue,
        BrowserRecipeViewport, BrowserReplayCoordinator, BrowserReplayInstance,
        BrowserReplayLocatorSlot, BrowserReplayRepairInstance, BrowserReplayRepairResumeCursor,
        BrowserReplaySecretLease, BrowserReplaySecretSubmission, BrowserReplayStatus,
        BrowserResourceKind, BrowserResourceLimits, BrowserResourceStore, BrowserRevision,
        BROWSER_RECIPE_SCHEMA_VERSION,
    };
    use static_assertions::assert_not_impl_any;
    use std::num::NonZeroU64;

    assert_not_impl_any!(BrowserReplayRepairRetentionSidecar: Clone, std::fmt::Debug, serde::Serialize);
    assert_not_impl_any!(BrowserReplayRepairCaptureAuthority: Clone, std::fmt::Debug, serde::Serialize);
    assert_not_impl_any!(BrowserReplayRepairCaptureReceipt: Clone, std::fmt::Debug, serde::Serialize);
    assert_not_impl_any!(BrowserReplayRepairCapturedEvidence: Clone, std::fmt::Debug, serde::Serialize);
    assert_not_impl_any!(BrowserReplayRepairPreviewSidecar: Clone, std::fmt::Debug, serde::Serialize);
    assert_not_impl_any!(BrowserReplayRepairPreviewAuthority: std::fmt::Debug, serde::Serialize);
    assert_not_impl_any!(BrowserReplayRepairHighlightToken: std::fmt::Debug, serde::Serialize);
    assert_not_impl_any!(BrowserReplayRepairCleanupWork: std::fmt::Debug, serde::Serialize);

    const SECRET_INPUT: &str = "password";
    const SECRET_VALUE: &str = "value-sentinel-secure-sidecar";

    #[test]
    fn repair_capture_error_boundary_is_a_closed_value_free_allowlist() {
        const SENTINEL: &str = "DM_REPAIR_ERROR_SENTINEL_6E2A";
        for unsafe_error in [
            BrowserError::CrashedView {
                message: SENTINEL.to_string(),
            },
            BrowserError::NavigationFailure {
                url: SENTINEL.to_string(),
                message: SENTINEL.to_string(),
            },
            BrowserError::InvalidAnnotation {
                field: SENTINEL.to_string(),
                message: SENTINEL.to_string(),
            },
            BrowserError::InvalidRecipe {
                message: SENTINEL.to_string(),
            },
            BrowserError::MissingResource {
                id: BrowserResourceId(SENTINEL.to_string()),
            },
            BrowserError::BlockedPermission {
                permission: SENTINEL.to_string(),
            },
            BrowserError::Timeout {
                operation: SENTINEL.to_string(),
            },
            BrowserError::InvalidInvocation {
                field: SENTINEL.to_string(),
            },
        ] {
            let contained = contain_repair_capture_error(unsafe_error);
            assert_eq!(contained, BrowserError::ResourceRootUnavailable);
            for surface in [
                format!("{contained:?}"),
                serde_json::to_string(&contained).unwrap(),
            ] {
                assert!(!surface.contains(SENTINEL), "{surface}");
            }
        }

        let platform = contain_repair_capture_error(BrowserError::UnavailablePlatform {
            platform: SENTINEL.to_string(),
        });
        assert_eq!(
            platform,
            BrowserError::UnavailablePlatform {
                platform: std::env::consts::OS.to_string(),
            }
        );
        assert!(!format!("{platform:?}").contains(SENTINEL));

        for safe_error in [
            BrowserError::Interrupted,
            BrowserError::Timeout {
                operation: "snapshot".to_string(),
            },
            BrowserError::Timeout {
                operation: "screenshot".to_string(),
            },
            BrowserError::ResourceTooLarge {
                byte_size: 8,
                limit: 4,
            },
            BrowserError::StaleReference {
                expected: BrowserRevision(9),
                actual: BrowserRevision(10),
            },
            BrowserError::LocatorNotFound {
                target: crate::browser::BrowserLocatorFailureTarget::Primary,
            },
            BrowserError::ResourceRootBusy,
            BrowserError::ResourceRootUnavailable,
            BrowserError::InvalidInvocation {
                field: "repairSidecar".to_string(),
            },
        ] {
            assert_eq!(contain_repair_capture_error(safe_error.clone()), safe_error);
        }
    }

    #[test]
    fn repair_preview_error_boundary_normalizes_every_string_bearing_error() {
        const SENTINEL: &str = "DM_REPAIR_PREVIEW_ERROR_SENTINEL_3A9C";

        let timeout = contain_repair_preview_error(BrowserError::Timeout {
            operation: SENTINEL.to_string(),
        });
        assert_eq!(
            timeout,
            BrowserError::Timeout {
                operation: "repairHighlight".to_string(),
            }
        );

        let unavailable = contain_repair_preview_error(BrowserError::UnavailablePlatform {
            platform: SENTINEL.to_string(),
        });
        assert_eq!(
            unavailable,
            BrowserError::UnavailablePlatform {
                platform: std::env::consts::OS.to_string(),
            }
        );

        for contained in [
            timeout,
            unavailable,
            contain_repair_preview_error(BrowserError::CrashedView {
                message: SENTINEL.to_string(),
            }),
            contain_repair_preview_error(BrowserError::InvalidInvocation {
                field: SENTINEL.to_string(),
            }),
        ] {
            for surface in [
                format!("{contained:?}"),
                serde_json::to_string(&contained).unwrap(),
            ] {
                assert!(!surface.contains(SENTINEL), "{surface}");
            }
        }
    }

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
        input_name: &str,
    ) -> (
        BrowserReplayCoordinator,
        BrowserReplayInstance,
        BrowserReplaySecretLease,
    ) {
        let coordinator = BrowserReplayCoordinator::default();
        let plan = compile_browser_replay(
            &BrowserRecipeV1 {
                schema_version: BROWSER_RECIPE_SCHEMA_VERSION,
                id: "secure-command-recipe".to_string(),
                name: "Secure command recipe".to_string(),
                description: "Secure command authority fixture".to_string(),
                start_url: "https://example.test".to_string(),
                viewport: BrowserRecipeViewport {
                    width: 1280,
                    height: 720,
                    scale_percent: 100,
                },
                inputs: vec![BrowserRecipeInput {
                    name: input_name.to_string(),
                    kind: BrowserRecipeInputKind::Secret,
                    default_value: None,
                }],
                steps: vec![BrowserRecipeStep {
                    id: "type-secure-input".to_string(),
                    action: BrowserRecipeAction::Type {
                        locator: BrowserRecipeLocator {
                            test_id: Some("secret-input".to_string()),
                            ..BrowserRecipeLocator::default()
                        },
                        value: BrowserRecipeValue::Input {
                            name: input_name.to_string(),
                        },
                    },
                    wait: None,
                    assertions: Vec::new(),
                }],
            },
            Vec::new(),
        )
        .unwrap();
        let started = coordinator.start(workspace_key.clone(), plan).unwrap();
        let instance = started.instance.clone();
        coordinator
            .submit_secrets(
                &instance,
                BrowserReplaySecretSubmission::from_user_prompt(vec![(
                    input_name.to_string(),
                    SECRET_VALUE.to_string(),
                )]),
            )
            .unwrap();
        let lease = started.execution.secret_lease(input_name).unwrap();
        (coordinator, instance, lease)
    }

    #[cfg(target_os = "windows")]
    fn installed_repair(
        workspace_key: &BrowserWorkspaceKey,
        store: &BrowserResourceStore,
    ) -> (
        BrowserReplayCoordinator,
        BrowserReplayInstance,
        BrowserReplayRepairInstance,
    ) {
        let coordinator = BrowserReplayCoordinator::default();
        let plan = compile_browser_replay(
            &BrowserRecipeV1 {
                schema_version: BROWSER_RECIPE_SCHEMA_VERSION,
                id: "repair-command-recipe".to_string(),
                name: "Repair command recipe".to_string(),
                description: "Repair command authority fixture".to_string(),
                start_url: "https://example.test".to_string(),
                viewport: BrowserRecipeViewport {
                    width: 1280,
                    height: 720,
                    scale_percent: 100,
                },
                inputs: Vec::new(),
                steps: vec![BrowserRecipeStep {
                    id: "click-target".to_string(),
                    action: BrowserRecipeAction::Click {
                        locator: BrowserRecipeLocator {
                            test_id: Some("target".to_string()),
                            ..BrowserRecipeLocator::default()
                        },
                    },
                    wait: None,
                    assertions: Vec::new(),
                }],
            },
            Vec::new(),
        )
        .unwrap();
        let started = coordinator.start(workspace_key.clone(), plan).unwrap();
        coordinator.begin(&started.instance).unwrap();
        let repair = coordinator
            .reserve_locator_repair_capture(
                &started.instance,
                store,
                0,
                BrowserReplayLocatorSlot::PrimaryAction,
                "tab-a",
                BrowserRevision(9),
                BrowserReplayRepairResumeCursor::Action,
            )
            .unwrap();
        (coordinator, started.instance, repair)
    }

    #[cfg(target_os = "windows")]
    fn publish_repair_for_preview(
        coordinator: &BrowserReplayCoordinator,
        repair: &BrowserReplayRepairInstance,
    ) {
        let snapshot = coordinator
            .retain_locator_repair_evidence_for_test(
                repair,
                BrowserResourceKind::ReplayRepairSnapshot,
                "application/json",
                b"{}",
            )
            .unwrap();
        let screenshot = coordinator
            .retain_locator_repair_evidence_for_test(
                repair,
                BrowserResourceKind::ReplayRepairScreenshot,
                "image/png",
                b"png",
            )
            .unwrap();
        coordinator
            .publish_locator_repair(repair, &snapshot, &screenshot)
            .unwrap();
    }

    #[cfg(target_os = "windows")]
    fn repair_preview_candidate(test_id: &str) -> BrowserReplayRepairCandidate {
        repair_preview_candidate_at_revision(test_id, BrowserRevision(9))
    }

    #[cfg(target_os = "windows")]
    fn repair_preview_candidate_at_revision(
        test_id: &str,
        revision: BrowserRevision,
    ) -> BrowserReplayRepairCandidate {
        BrowserReplayRepairCandidate::new(BrowserElementRef {
            revision,
            locator: BrowserLocator {
                test_id: Some(test_id.to_string()),
                ..BrowserLocator::default()
            },
            backend_node_id: Some(91),
        })
    }

    #[cfg(target_os = "windows")]
    fn installed_saved_previewed_repair(
        label: &str,
    ) -> (
        PathBuf,
        PathBuf,
        BrowserResourceStore,
        BrowserWorkspaceKey,
        BrowserReplayCoordinator,
        crate::browser::BrowserReplayExecutionHandle,
        BrowserReplayInstance,
        BrowserReplayRepairInstance,
    ) {
        let root = std::env::temp_dir().join(format!(
            "devmanager-repair-apply-{label}-{}",
            random_operation_id().unwrap()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let recipe = BrowserRecipeV1 {
            schema_version: BROWSER_RECIPE_SCHEMA_VERSION,
            id: format!("repair-apply-{label}"),
            name: "Repair apply fixture".to_string(),
            description: "Repair apply orchestration fixture".to_string(),
            start_url: "https://example.test".to_string(),
            viewport: BrowserRecipeViewport {
                width: 1280,
                height: 720,
                scale_percent: 100,
            },
            inputs: Vec::new(),
            steps: vec![BrowserRecipeStep {
                id: "click-target".to_string(),
                action: BrowserRecipeAction::Click {
                    locator: BrowserRecipeLocator {
                        test_id: Some("target".to_string()),
                        ..BrowserRecipeLocator::default()
                    },
                },
                wait: None,
                assertions: Vec::new(),
            }],
        };
        let recipe_file = save_recipe(&root, &recipe).unwrap();
        assert_eq!(recipe_file, recipe_path(&root, &recipe.id).unwrap());
        let plan = compile_browser_replay(&recipe, Vec::new()).unwrap();
        let workspace_key = workspace(&format!("repair-apply-{label}"), "conversation-a");
        let coordinator = BrowserReplayCoordinator::default();
        let started = coordinator.start(workspace_key.clone(), plan).unwrap();
        started.execution.bind_canonical_recipe_root(&root).unwrap();
        coordinator.begin(&started.instance).unwrap();
        let store = BrowserResourceStore::open(
            root.join("resources"),
            BrowserResourceLimits {
                max_temporary_count: 0,
                max_temporary_bytes: 1024 * 1024,
                max_resource_bytes: 1024 * 1024,
            },
        )
        .unwrap();
        let repair = coordinator
            .reserve_locator_repair_capture(
                &started.instance,
                &store,
                0,
                BrowserReplayLocatorSlot::PrimaryAction,
                "tab-a",
                BrowserRevision(9),
                BrowserReplayRepairResumeCursor::Action,
            )
            .unwrap();
        publish_repair_for_preview(&coordinator, &repair);
        commit_preview_for_test(
            &coordinator,
            &repair,
            repair_preview_candidate("replacement"),
        );
        (
            root,
            recipe_file,
            store,
            workspace_key,
            coordinator,
            started.execution,
            started.instance,
            repair,
        )
    }

    #[cfg(target_os = "windows")]
    fn commit_preview_for_test(
        coordinator: &BrowserReplayCoordinator,
        repair: &BrowserReplayRepairInstance,
        candidate: BrowserReplayRepairCandidate,
    ) {
        let (authority, receipt) = coordinator
            .reserve_locator_repair_preview(repair, candidate)
            .unwrap();
        assert!(authority.acknowledge_for_test());
        let acknowledgement = receipt.consume_exact(repair).unwrap();
        coordinator
            .commit_locator_repair_preview(acknowledgement, || {
                BrowserReplayRepairHighlightCleanup::for_test(Arc::new(AtomicUsize::new(0)))
            })
            .unwrap();
    }

    #[cfg(target_os = "windows")]
    #[tokio::test]
    async fn post_context_factory_failure_aborts_before_repair_apply_commit() {
        let (root, recipe_file, store, workspace_key, coordinator, execution, instance, repair) =
            installed_saved_previewed_repair("post-context-failure");
        let before = std::fs::read(&recipe_file).unwrap();
        let (bridge, mut inbox) = browser_command_channel(8);
        let controller = bridge.bind(workspace_key, Duration::from_secs(2));
        let task_controller = controller.clone();
        let task_coordinator = coordinator.clone();
        let task_repair = repair.clone();
        let task = tokio::spawn(async move {
            task_controller
                .request_replay_repair_apply_with_post_context_factory(
                    &task_coordinator,
                    &task_repair,
                    true,
                    true,
                    BrowserInvocationContext::agent("apply repaired locator", BrowserRisk::Normal)
                        .unwrap(),
                    |_| {
                        Err(BrowserError::CrashedView {
                            message: "injected post-context operation-id failure".to_string(),
                        })
                    },
                )
                .await
        });

        let pre_commit = inbox.recv().await.expect("pre-commit validation request");
        let pre_authority = pre_commit.repair_apply_authority().unwrap().clone();
        assert!(pre_authority.acknowledge_for_test());
        pre_commit.respond(Ok(BrowserResponse::Acknowledged));

        assert!(matches!(
            task.await.unwrap(),
            Err(BrowserError::InvalidInvocation { field }) if field == "repairApplySidecar"
        ));
        assert_eq!(std::fs::read(&recipe_file).unwrap(), before);
        assert!(
            execution
                .locator_override(0, BrowserReplayLocatorSlot::PrimaryAction)
                .is_none(),
            "failed post-context creation must not install a locator override"
        );
        assert_eq!(
            coordinator.locator_repair_status(&repair).unwrap().phase,
            crate::browser::BrowserReplayRepairPhase::Previewed
        );
        assert!(!pre_authority.is_live());
        assert!(
            tokio::time::timeout(Duration::from_millis(20), inbox.recv())
                .await
                .is_err()
        );

        coordinator.cancel(&instance).unwrap();
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "windows")]
    #[tokio::test]
    async fn repair_apply_denial_fences_write_and_post_commit_drift_reports_applied() {
        let (root, recipe_file, store, workspace_key, coordinator, _execution, instance, repair) =
            installed_saved_previewed_repair("denial-drift");
        let before = std::fs::read(&recipe_file).unwrap();
        let (bridge, mut inbox) = browser_command_channel(8);
        let controller = bridge.bind(workspace_key, Duration::from_secs(2));

        let denied_controller = controller.clone();
        let denied_coordinator = coordinator.clone();
        let denied_repair = repair.clone();
        let denied = tokio::spawn(async move {
            denied_controller
                .request_replay_repair_apply(
                    &denied_coordinator,
                    &denied_repair,
                    true,
                    true,
                    BrowserInvocationContext::agent("apply repaired locator", BrowserRisk::Normal)
                        .unwrap(),
                )
                .await
        });
        let denied_request = inbox.recv().await.expect("pre-commit validation request");
        assert!(matches!(
            denied_request.command(),
            BrowserCommand::RepairValidate { .. }
        ));
        denied_request.validate_repair_apply_sidecar().unwrap();
        let denied_authority = denied_request.repair_apply_authority().unwrap();
        assert_eq!(denied_authority.effective_risk(), BrowserRisk::Destructive);
        assert!(!denied_request.records_workflow_recipe_action());
        assert_eq!(std::fs::read(&recipe_file).unwrap(), before);
        denied_request.respond(Err(BrowserError::BlockedPermission {
            permission: "Destructive".to_string(),
        }));
        assert!(matches!(
            denied.await.unwrap(),
            Err(BrowserError::BlockedPermission { permission }) if permission == "Destructive"
        ));
        assert_eq!(std::fs::read(&recipe_file).unwrap(), before);
        assert_eq!(
            coordinator.locator_repair_status(&repair).unwrap().phase,
            crate::browser::BrowserReplayRepairPhase::Previewed
        );

        let apply_controller = controller.clone();
        let apply_coordinator = coordinator.clone();
        let apply_repair = repair.clone();
        let apply = tokio::spawn(async move {
            apply_controller
                .request_replay_repair_apply(
                    &apply_coordinator,
                    &apply_repair,
                    true,
                    true,
                    BrowserInvocationContext::agent("apply repaired locator", BrowserRisk::Normal)
                        .unwrap(),
                )
                .await
        });
        let pre_commit = inbox.recv().await.expect("approved pre-commit validation");
        let pre_authority = pre_commit.repair_apply_authority().unwrap().clone();
        assert_eq!(pre_authority.effective_risk(), BrowserRisk::Destructive);
        assert!(pre_authority.acknowledge_for_test());
        pre_commit.respond(Ok(BrowserResponse::Acknowledged));

        let post_commit = inbox.recv().await.expect("post-commit validation");
        let committed = std::fs::read(&recipe_file).unwrap();
        assert_ne!(
            committed, before,
            "write occurs only after pre-commit acknowledgement"
        );
        let loaded = crate::browser::load_recipe(&root, "repair-apply-denial-drift").unwrap();
        assert!(matches!(
            &loaded.steps[0].action,
            BrowserRecipeAction::Click { locator }
                if locator.test_id.as_deref() == Some("replacement")
        ));
        assert_eq!(
            coordinator.locator_repair_status(&repair).unwrap().phase,
            crate::browser::BrowserReplayRepairPhase::Applied
        );
        let post_authority = post_commit.repair_apply_authority().unwrap();
        assert_eq!(post_authority.effective_risk(), BrowserRisk::Normal);
        post_commit.respond(Err(BrowserError::StaleReference {
            expected: BrowserRevision(9),
            actual: BrowserRevision(10),
        }));
        let applied = apply.await.unwrap().unwrap();
        assert!(applied.recipe_written);
        assert_eq!(
            applied.repair.phase,
            crate::browser::BrowserReplayRepairPhase::Applied
        );
        assert_eq!(
            applied.replay.status,
            BrowserReplayStatus::PausedLocatorRepair
        );
        assert_eq!(std::fs::read(&recipe_file).unwrap(), committed);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), inbox.recv())
                .await
                .is_err()
        );

        assert!(matches!(
            controller
                .request_replay_repair_apply(
                    &coordinator,
                    &repair,
                    true,
                    true,
                    BrowserInvocationContext::user("resume repaired locator", BrowserRisk::Normal)
                        .unwrap(),
                )
                .await,
            Err(BrowserError::InvalidInvocation { field }) if field == "repairApplySidecar"
        ));
        commit_preview_for_test(
            &coordinator,
            &repair,
            repair_preview_candidate_at_revision("replacement", BrowserRevision(10)),
        );
        let mut permissions = std::fs::metadata(&recipe_file).unwrap().permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&recipe_file, permissions.clone()).unwrap();
        let resume_controller = controller.clone();
        let resume_coordinator = coordinator.clone();
        let resume_repair = repair.clone();
        let resume = tokio::spawn(async move {
            resume_controller
                .request_replay_repair_apply(
                    &resume_coordinator,
                    &resume_repair,
                    true,
                    true,
                    BrowserInvocationContext::user("resume repaired locator", BrowserRisk::Normal)
                        .unwrap(),
                )
                .await
        });
        let resume_validation = inbox.recv().await.expect("no-write resume validation");
        let resume_authority = resume_validation.repair_apply_authority().unwrap().clone();
        assert!(resume_authority.acknowledge_for_test());
        resume_validation.respond(Ok(BrowserResponse::Acknowledged));
        let resumed = resume.await.unwrap().unwrap();
        assert!(!resumed.recipe_written);
        assert_eq!(resumed.replay.status, BrowserReplayStatus::Running);
        assert_eq!(std::fs::read(&recipe_file).unwrap(), committed);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), inbox.recv())
                .await
                .is_err()
        );

        permissions.set_readonly(false);
        std::fs::set_permissions(&recipe_file, permissions).unwrap();
        drop(resumed);
        drop(applied);
        coordinator.cancel(&instance).unwrap();
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "windows")]
    #[tokio::test]
    async fn applied_without_resume_does_not_manufacture_fresh_preview_authority() {
        let (root, _recipe_file, store, workspace_key, coordinator, _execution, instance, repair) =
            installed_saved_previewed_repair("no-resume");
        let (bridge, mut inbox) = browser_command_channel(8);
        let controller = bridge.bind(workspace_key, Duration::from_secs(2));
        let task_controller = controller.clone();
        let task_coordinator = coordinator.clone();
        let task_repair = repair.clone();
        let task = tokio::spawn(async move {
            task_controller
                .request_replay_repair_apply(
                    &task_coordinator,
                    &task_repair,
                    true,
                    false,
                    BrowserInvocationContext::user("save repaired locator", BrowserRisk::Normal)
                        .unwrap(),
                )
                .await
        });
        for expected_intent in [
            "save repaired locator",
            "validate applied replay repair locator before resume",
        ] {
            let request = inbox.recv().await.expect("exact apply validation");
            assert_eq!(request.context().intent, expected_intent);
            let authority = request.repair_apply_authority().unwrap().clone();
            assert!(authority.acknowledge_for_test());
            request.respond(Ok(BrowserResponse::Acknowledged));
        }
        let applied = task.await.unwrap().unwrap();
        assert!(applied.recipe_written);
        assert_eq!(
            applied.repair.phase,
            crate::browser::BrowserReplayRepairPhase::Applied
        );
        assert_eq!(
            applied.replay.status,
            BrowserReplayStatus::PausedLocatorRepair
        );
        assert!(matches!(
            controller
                .request_replay_repair_apply(
                    &coordinator,
                    &repair,
                    true,
                    true,
                    BrowserInvocationContext::user("resume repaired locator", BrowserRisk::Normal)
                        .unwrap(),
                )
                .await,
            Err(BrowserError::InvalidInvocation { field }) if field == "repairApplySidecar"
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(20), inbox.recv())
                .await
                .is_err()
        );

        drop(applied);
        coordinator.cancel(&instance).unwrap();
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "windows")]
    #[tokio::test]
    async fn repair_preview_uses_fixed_context_and_superseded_install_cannot_block_new_preview() {
        let root = std::env::temp_dir().join(format!(
            "devmanager-repair-preview-command-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let store = BrowserResourceStore::open(
            &root,
            BrowserResourceLimits {
                max_temporary_count: 0,
                max_temporary_bytes: 1024 * 1024,
                max_resource_bytes: 1024 * 1024,
            },
        )
        .unwrap();
        let workspace_key = workspace("repair-preview-command", "conversation-a");
        let (coordinator, instance, repair) = installed_repair(&workspace_key, &store);
        publish_repair_for_preview(&coordinator, &repair);
        let (bridge, mut inbox) = browser_command_channel(8);
        let controller = bridge.bind(workspace_key.clone(), Duration::from_secs(2));

        let user_controller = controller.clone();
        let user_coordinator = coordinator.clone();
        let user_repair = repair.clone();
        let user_task = tokio::spawn(async move {
            user_controller
                .request_replay_repair_preview(
                    &user_coordinator,
                    &user_repair,
                    repair_preview_candidate("committed"),
                    BrowserInvocationActor::User,
                )
                .await
        });
        let user_request = inbox.recv().await.expect("user preview marker");
        assert_eq!(user_request.context().actor, BrowserInvocationActor::User);
        assert_eq!(
            user_request.context().intent,
            "preview replay repair locator"
        );
        assert!(!user_request.records_workflow_recipe_action());
        let user_authority = user_request
            .repair_preview_highlight_authority()
            .expect("private user preview authority")
            .clone();
        assert!(user_authority.acknowledge_for_test());
        user_request.respond(Ok(BrowserResponse::Acknowledged));
        assert_eq!(
            user_task.await.unwrap().unwrap().phase,
            crate::browser::BrowserReplayRepairPhase::Previewed
        );

        let a_controller = controller.clone();
        let a_coordinator = coordinator.clone();
        let a_repair = repair.clone();
        let a_task = tokio::spawn(async move {
            a_controller
                .request_replay_repair_preview(
                    &a_coordinator,
                    &a_repair,
                    repair_preview_candidate("superseded-a"),
                    BrowserInvocationActor::Agent,
                )
                .await
        });
        let a_request = inbox.recv().await.expect("preview A marker");
        let a_authority = a_request
            .repair_preview_highlight_authority()
            .expect("preview A authority")
            .clone();
        assert!(a_authority.is_live());
        assert_eq!(a_request.context().actor, BrowserInvocationActor::Agent);
        assert_eq!(a_request.context().intent, "preview replay repair locator");

        let b_controller = controller.clone();
        let b_coordinator = coordinator.clone();
        let b_repair = repair.clone();
        let b_task = tokio::spawn(async move {
            b_controller
                .request_replay_repair_preview(
                    &b_coordinator,
                    &b_repair,
                    repair_preview_candidate("current-b"),
                    BrowserInvocationActor::Agent,
                )
                .await
        });
        let b_request = inbox.recv().await.expect("preview B marker");
        let b_authority = b_request
            .repair_preview_highlight_authority()
            .expect("preview B authority")
            .clone();
        assert!(!a_authority.is_live(), "B reservation closes A authority");
        assert!(b_authority.is_live());
        assert!(
            a_authority.expected_previous_token() == b_authority.expected_previous_token(),
            "A and B both CAS against the last committed preview"
        );

        // A's failed request publishes cleanup only to the private host lane; it never
        // competes with B or any ordinary command envelope.
        a_request.respond(Err(BrowserError::Interrupted));
        assert_eq!(a_task.await.unwrap(), Err(BrowserError::Interrupted));
        assert!(b_authority.acknowledge_for_test());
        b_request.respond(Ok(BrowserResponse::Acknowledged));
        assert_eq!(
            b_task.await.unwrap().unwrap().phase,
            crate::browser::BrowserReplayRepairPhase::Previewed
        );

        let (controls, lifecycle, cleanups) =
            bridge.with_locked_host_work_and_repair_cleanups(|controls, lifecycle, cleanups| {
                (controls, lifecycle, cleanups)
            });
        assert!(controls.is_empty());
        assert!(lifecycle.is_empty());
        assert_eq!(cleanups.len(), 1);
        let late_a_clear = &cleanups[0];
        assert!(late_a_clear.token() == a_authority.token());
        assert!(late_a_clear.token() != b_authority.token());
        assert!(
            late_a_clear.restore().is_none(),
            "a superseded guard must clear only and cannot resurrect its predecessor"
        );
        assert_eq!(late_a_clear.context().actor, BrowserInvocationActor::Agent);
        assert_eq!(
            late_a_clear.context().intent,
            "clear replay repair preview highlight"
        );

        assert!(matches!(
            controller
                .request_replay_repair_preview(
                    &coordinator,
                    &repair,
                    repair_preview_candidate("internal-forbidden"),
                    BrowserInvocationActor::Internal,
                )
                .await,
            Err(BrowserError::InvalidInvocation { field }) if field == "repairPreviewSidecar"
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(20), inbox.recv())
                .await
                .is_err()
        );

        drop(inbox);
        coordinator.cancel(&instance).unwrap();
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "windows")]
    #[tokio::test]
    async fn repair_preview_cleanup_drop_outside_tokio_survives_a_full_bridge_queue() {
        let root = std::env::temp_dir().join(format!(
            "devmanager-repair-preview-full-queue-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let store = BrowserResourceStore::open(
            &root,
            BrowserResourceLimits {
                max_temporary_count: 0,
                max_temporary_bytes: 1024 * 1024,
                max_resource_bytes: 1024 * 1024,
            },
        )
        .unwrap();
        let workspace_key = workspace("repair-preview-full-queue", "conversation-a");
        let (coordinator, instance, repair) = installed_repair(&workspace_key, &store);
        let token = BrowserReplayRepairHighlightToken::new(
            repair,
            NonZeroU64::new(1).unwrap(),
            "tab-a".to_string(),
            "dddddddddddddddddddddddddddddddddddddddddddddddd".to_string(),
        );
        let (bridge, inbox) = browser_command_channel(1);
        let registration = BrowserRegistrationLease::new();
        let controller = bridge.bind_with_registration_lease(
            workspace_key.clone(),
            Duration::from_secs(2),
            Some(registration.clone()),
        );
        let filler_controller = controller.clone();
        let filler = tokio::spawn(async move {
            filler_controller
                .request_with_context(
                    BrowserCommand::Status,
                    BrowserInvocationContext::user("fill bridge", BrowserRisk::Normal).unwrap(),
                )
                .await
        });
        wait_for_pending(&bridge).await;

        let admission = controller
            .host_controls
            .try_admit_repair_cleanup()
            .expect("cleanup slot admitted before preview install");
        let cleanup_queue = Arc::clone(&controller.host_controls);
        let cleanup_token = token.clone();
        let cleanup_admission = admission.clone();
        let cleanup = BrowserReplayRepairHighlightCleanup::new(move || {
            cleanup_queue.enqueue_repair_cleanup(
                cleanup_token,
                BrowserInvocationActor::Agent,
                None,
                cleanup_admission,
            );
        });
        bridge.interrupt_tab(&workspace_key, "tab-a");
        bridge.revoke_registration(&workspace_key, &registration);
        drop(inbox);
        std::thread::spawn(move || drop(cleanup))
            .join()
            .expect("cleanup drops outside Tokio");

        for _ in 0..1_000 {
            controller.host_controls.enqueue_repair_cleanup(
                token.clone(),
                BrowserInvocationActor::Agent,
                None,
                admission.clone(),
            );
        }
        let _ = filler.await.unwrap();
        assert_eq!(bridge.pending_work_count(), 0);
        let (controls, lifecycle, cleanups) =
            bridge.with_locked_host_work_and_repair_cleanups(|controls, lifecycle, cleanups| {
                (controls, lifecycle, cleanups)
            });
        assert!(
            !controls.is_empty(),
            "later interruption remains independently queued"
        );
        assert!(lifecycle.is_empty());
        assert_eq!(cleanups.len(), 1, "repeated cleanup requests coalesce");
        let cleanup_request = &cleanups[0];
        assert_eq!(
            cleanup_request.context().intent,
            "clear replay repair preview highlight"
        );
        assert!(cleanup_request.token() == &token);
        assert!(cleanup_request.restore().is_none());
        drop(cleanups);
        drop(admission);
        assert_eq!(
            controller
                .host_controls
                .repair_cleanup_admissions
                .load(Ordering::Acquire),
            0
        );

        coordinator.cancel(&instance).unwrap();
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn repair_preview_cleanup_admission_is_strictly_bounded() {
        let (bridge, _inbox) = browser_command_channel(1);
        let admissions: Vec<_> = (0..MAX_BROWSER_REPAIR_HIGHLIGHT_CLEANUPS)
            .map(|_| {
                bridge
                    .host_controls
                    .try_admit_repair_cleanup()
                    .expect("bounded slot remains available")
            })
            .collect();
        assert!(bridge.host_controls.try_admit_repair_cleanup().is_none());
        drop(admissions);
        assert_eq!(
            bridge
                .host_controls
                .repair_cleanup_admissions
                .load(Ordering::Acquire),
            0
        );
        assert!(bridge.host_controls.try_admit_repair_cleanup().is_some());
    }

    fn forged_request(
        workspace_key: BrowserWorkspaceKey,
        command: BrowserCommand,
        replay_secret_sidecar: Option<BrowserReplaySecretSidecar>,
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
                replay_secret_sidecar,
                replay_repair_sidecar: None,
                replay_repair_preview_sidecar: None,
                response,
                pending_work: pending_work.track(),
            },
            cancellations,
            Arc::new(Mutex::new(())),
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

    fn forged_repair_request(
        workspace_key: BrowserWorkspaceKey,
        command: BrowserCommand,
        context: BrowserInvocationContext,
        authority: BrowserReplayRepairCaptureAuthority,
    ) -> BrowserCommandRequest {
        let cancellations = Arc::new(CancellationEpochs::default());
        let cancellation_ticket = cancellations.ticket(&workspace_key, command.tab_id());
        let pending_work = Arc::new(PendingWork::default());
        let (response, _receiver) = oneshot::channel();
        BrowserCommandRequest::from_envelope(
            BrowserCommandEnvelope {
                workspace_key,
                command,
                context,
                local_project_root: None,
                cancellation_ticket,
                registration_lease: None,
                replay_secret_sidecar: None,
                replay_repair_sidecar: Some(BrowserReplayRepairRetentionSidecar { authority }),
                replay_repair_preview_sidecar: None,
                response,
                pending_work: pending_work.track(),
            },
            cancellations,
            Arc::new(Mutex::new(())),
        )
    }

    #[cfg(target_os = "windows")]
    #[tokio::test]
    async fn repair_capture_sidecar_retains_and_records_exact_snapshot_then_screenshot() {
        let root = std::env::temp_dir().join(format!(
            "devmanager-repair-command-sidecar-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let store = BrowserResourceStore::open(
            &root,
            BrowserResourceLimits {
                max_temporary_count: 0,
                max_temporary_bytes: 1024 * 1024,
                max_resource_bytes: 1024 * 1024,
            },
        )
        .unwrap();
        let (bridge, mut inbox) = browser_command_channel(2);
        let workspace_key = workspace("repair-command", "conversation-a");
        let controller = bridge.bind(workspace_key.clone(), Duration::from_secs(1));
        let (coordinator, instance, repair) = installed_repair(&workspace_key, &store);
        let task_controller = controller.clone();
        let task_coordinator = coordinator.clone();
        let task_repair = repair.clone();
        let task = tokio::spawn(async move {
            task_controller
                .request_replay_repair_capture(
                    &task_coordinator,
                    &task_repair,
                    BrowserCommand::Snapshot {
                        tab_id: "tab-a".to_string(),
                    },
                    BrowserInvocationContext::agent("repair snapshot", BrowserRisk::Normal)
                        .unwrap(),
                )
                .await
        });

        let request = inbox.recv().await.expect("repair snapshot reaches host");
        assert!(request
            .validate_repair_retention_sidecar()
            .expect("exact private sidecar")
            .is_some());
        assert!(!request.records_workflow_recipe_action());
        assert_eq!(
            crate::browser::host::unsupported_request_response("fixture", &request),
            Err(BrowserError::UnavailablePlatform {
                platform: "fixture".to_string(),
            })
        );
        let resource = request
            .retain_repair_resource_for_test(
                &store,
                BrowserResourceKind::ReplayRepairSnapshot,
                "application/json",
                b"{}",
            )
            .unwrap();
        request.respond(Ok(BrowserResponse::Snapshot {
            summary: BrowserSnapshotSummary {
                tab_id: "tab-a".to_string(),
                url: "https://example.test".to_string(),
                revision: BrowserRevision(9),
                element_count: 0,
            },
            resource: resource.clone(),
        }));
        assert!(matches!(
            task.await.unwrap(),
            Ok(BrowserResponse::Snapshot { resource: returned, .. }) if returned == resource
        ));
        assert_eq!(
            coordinator.status(&instance).unwrap().status,
            crate::browser::BrowserReplayStatus::Running
        );
        assert!(store.handle(&workspace_key, &resource.id).unwrap().pinned);

        let screenshot_controller = controller.clone();
        let screenshot_coordinator = coordinator.clone();
        let screenshot_repair = repair.clone();
        let screenshot_task = tokio::spawn(async move {
            screenshot_controller
                .request_replay_repair_capture(
                    &screenshot_coordinator,
                    &screenshot_repair,
                    BrowserCommand::Screenshot {
                        tab_id: "tab-a".to_string(),
                        mode: BrowserScreenshotMode::Viewport,
                    },
                    BrowserInvocationContext::agent("repair screenshot", BrowserRisk::Normal)
                        .unwrap(),
                )
                .await
        });
        let screenshot_request = inbox.recv().await.expect("repair screenshot reaches host");
        assert!(screenshot_request
            .validate_repair_retention_sidecar()
            .expect("exact screenshot sidecar")
            .is_some());
        for (kind, mime_type) in [
            (BrowserResourceKind::ReplayRepairSnapshot, "image/png"),
            (
                BrowserResourceKind::ReplayRepairScreenshot,
                "application/octet-stream",
            ),
        ] {
            assert!(matches!(
                screenshot_request.retain_repair_resource_for_test(
                    &store,
                    kind,
                    mime_type,
                    b"png",
                ),
                Err(BrowserError::InvalidInvocation { field }) if field == "repairSidecar"
            ));
        }
        let screenshot = screenshot_request
            .retain_repair_resource_for_test(
                &store,
                BrowserResourceKind::ReplayRepairScreenshot,
                "image/png",
                b"png",
            )
            .unwrap();
        screenshot_request.respond(Ok(BrowserResponse::Screenshot {
            resource: screenshot.clone(),
        }));
        assert!(matches!(
            screenshot_task.await.unwrap(),
            Ok(BrowserResponse::Screenshot { resource: returned }) if returned == screenshot
        ));
        let projection = coordinator
            .publish_locator_repair(&repair, &resource, &screenshot)
            .unwrap();
        assert_eq!(projection.snapshot, resource);
        assert_eq!(projection.screenshot, screenshot);
        assert_eq!(
            coordinator.status(&instance).unwrap().status,
            crate::browser::BrowserReplayStatus::PausedLocatorRepair
        );

        coordinator.cancel(&instance).unwrap();
        assert!(matches!(
            store.handle(&workspace_key, &resource.id),
            Err(BrowserError::MissingResource { .. })
        ));
        assert!(matches!(
            store.handle(&workspace_key, &screenshot.id),
            Err(BrowserError::MissingResource { .. })
        ));
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "windows")]
    #[tokio::test]
    async fn repair_capture_rejects_same_root_cross_coordinator_handle_substitution() {
        let root = std::env::temp_dir().join(format!(
            "devmanager-repair-command-receipt-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let store = BrowserResourceStore::open(
            &root,
            BrowserResourceLimits {
                max_temporary_count: 0,
                max_temporary_bytes: 1024 * 1024,
                max_resource_bytes: 1024 * 1024,
            },
        )
        .unwrap();
        let workspace_key = workspace("repair-receipt", "conversation-a");
        let (left_coordinator, left_instance, left_repair) =
            installed_repair(&workspace_key, &store);
        let (right_coordinator, right_instance, right_repair) =
            installed_repair(&workspace_key, &store);
        let (bridge, mut inbox) = browser_command_channel(2);
        let controller = bridge.bind(workspace_key.clone(), Duration::from_secs(2));

        let left_controller = controller.clone();
        let task_coordinator = left_coordinator.clone();
        let task_repair = left_repair.clone();
        let left_task = tokio::spawn(async move {
            left_controller
                .request_replay_repair_capture(
                    &task_coordinator,
                    &task_repair,
                    BrowserCommand::Snapshot {
                        tab_id: "tab-a".to_string(),
                    },
                    BrowserInvocationContext::agent("left repair", BrowserRisk::Normal).unwrap(),
                )
                .await
        });
        let left_request = inbox.recv().await.unwrap();
        let left_handle = left_request
            .retain_repair_resource_for_test(
                &store,
                BrowserResourceKind::ReplayRepairSnapshot,
                "application/json",
                b"left",
            )
            .unwrap();

        let right_controller = controller.clone();
        let task_coordinator = right_coordinator.clone();
        let task_repair = right_repair.clone();
        let right_task = tokio::spawn(async move {
            right_controller
                .request_replay_repair_capture(
                    &task_coordinator,
                    &task_repair,
                    BrowserCommand::Snapshot {
                        tab_id: "tab-a".to_string(),
                    },
                    BrowserInvocationContext::agent("right repair", BrowserRisk::Normal).unwrap(),
                )
                .await
        });
        let right_request = inbox.recv().await.unwrap();
        let right_handle = right_request
            .retain_repair_resource_for_test(
                &store,
                BrowserResourceKind::ReplayRepairSnapshot,
                "application/json",
                b"right",
            )
            .unwrap();

        left_request.respond(Ok(BrowserResponse::Snapshot {
            summary: BrowserSnapshotSummary {
                tab_id: "tab-a".to_string(),
                url: "https://example.test".to_string(),
                revision: BrowserRevision(9),
                element_count: 0,
            },
            resource: right_handle.clone(),
        }));
        right_request.respond(Ok(BrowserResponse::Snapshot {
            summary: BrowserSnapshotSummary {
                tab_id: "tab-a".to_string(),
                url: "https://example.test".to_string(),
                revision: BrowserRevision(9),
                element_count: 0,
            },
            resource: left_handle.clone(),
        }));
        assert!(matches!(
            left_task.await.unwrap(),
            Err(BrowserError::InvalidInvocation { field }) if field == "repairSidecar"
        ));
        assert!(matches!(
            right_task.await.unwrap(),
            Err(BrowserError::InvalidInvocation { field }) if field == "repairSidecar"
        ));
        assert!(matches!(
            store.handle(&workspace_key, &left_handle.id),
            Err(BrowserError::MissingResource { .. })
        ));
        assert!(matches!(
            store.handle(&workspace_key, &right_handle.id),
            Err(BrowserError::MissingResource { .. })
        ));

        left_coordinator.cancel(&left_instance).unwrap();
        right_coordinator.cancel(&right_instance).unwrap();
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "windows")]
    #[tokio::test]
    async fn repair_sidecar_rejects_wrong_sequence_root_command_and_late_cancelled_use() {
        let root = std::env::temp_dir().join(format!(
            "devmanager-repair-sidecar-edge-{}",
            std::process::id()
        ));
        let other_root = std::env::temp_dir().join(format!(
            "devmanager-repair-sidecar-other-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&other_root);
        let limits = BrowserResourceLimits {
            max_temporary_count: 0,
            max_temporary_bytes: 1024 * 1024,
            max_resource_bytes: 1024 * 1024,
        };
        let store = BrowserResourceStore::open(&root, limits).unwrap();
        let other_store = BrowserResourceStore::open(&other_root, limits).unwrap();
        let (bridge, mut inbox) = browser_command_channel(4);
        let workspace_key = workspace("repair-edge", "conversation-a");
        let controller = bridge.bind(workspace_key.clone(), Duration::from_secs(2));
        let (coordinator, instance, repair) = installed_repair(&workspace_key, &store);

        let ordinary_controller = controller.clone();
        let ordinary = tokio::spawn(async move {
            ordinary_controller
                .request_with_context(
                    BrowserCommand::Screenshot {
                        tab_id: "tab-a".to_string(),
                        mode: BrowserScreenshotMode::Viewport,
                    },
                    BrowserInvocationContext::agent("ordinary screenshot", BrowserRisk::Normal)
                        .unwrap(),
                )
                .await
        });
        let ordinary_request = inbox.recv().await.unwrap();
        assert!(matches!(
            ordinary_request.validate_repair_retention_sidecar(),
            Ok(None)
        ));
        assert!(ordinary_request.records_workflow_recipe_action());
        ordinary_request.respond(Err(BrowserError::Interrupted));
        assert_eq!(ordinary.await.unwrap(), Err(BrowserError::Interrupted));

        assert!(matches!(
            controller
                .request_replay_repair_capture(
                    &coordinator,
                    &repair,
                    BrowserCommand::Snapshot {
                        tab_id: "tab-a".to_string(),
                    },
                    BrowserInvocationContext::user("forged user repair", BrowserRisk::Normal)
                        .unwrap(),
                )
                .await,
            Err(BrowserError::InvalidInvocation { field }) if field == "repairSidecar"
        ));
        assert!(matches!(
            controller
                .request_replay_repair_capture(
                    &BrowserReplayCoordinator::default(),
                    &repair,
                    BrowserCommand::Snapshot {
                        tab_id: "tab-a".to_string(),
                    },
                    BrowserInvocationContext::agent("foreign coordinator", BrowserRisk::Normal)
                        .unwrap(),
                )
                .await,
            Err(BrowserError::InvalidInvocation { field }) if field == "repairSidecar"
        ));
        let foreign_controller = bridge.bind(
            workspace("repair-edge-other", "conversation-a"),
            Duration::from_secs(1),
        );
        assert!(matches!(
            foreign_controller
                .request_replay_repair_capture(
                    &coordinator,
                    &repair,
                    BrowserCommand::Snapshot {
                        tab_id: "tab-a".to_string(),
                    },
                    BrowserInvocationContext::agent("foreign workspace", BrowserRisk::Normal)
                        .unwrap(),
                )
                .await,
            Err(BrowserError::InvalidInvocation { field }) if field == "repairSidecar"
        ));

        assert!(matches!(
            controller
                .request_replay_repair_capture(
                    &coordinator,
                    &repair,
                    BrowserCommand::Screenshot {
                        tab_id: "tab-a".to_string(),
                        mode: BrowserScreenshotMode::Viewport,
                    },
                    BrowserInvocationContext::agent("early screenshot", BrowserRisk::Normal)
                        .unwrap(),
                )
                .await,
            Err(BrowserError::InvalidInvocation { field }) if field == "repairSidecar"
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(20), inbox.recv())
                .await
                .is_err()
        );

        let (wrong_authority, _wrong_receipt) = coordinator
            .issue_locator_repair_capture_authority(
                &repair,
                BrowserResourceKind::ReplayRepairSnapshot,
            )
            .unwrap();
        let wrong_command = forged_repair_request(
            workspace_key.clone(),
            BrowserCommand::Screenshot {
                tab_id: "tab-a".to_string(),
                mode: BrowserScreenshotMode::Viewport,
            },
            BrowserInvocationContext::agent("wrong command", BrowserRisk::Normal).unwrap(),
            wrong_authority,
        );
        assert!(matches!(
            wrong_command.validate_repair_retention_sidecar(),
            Err(BrowserError::InvalidInvocation { field }) if field == "repairSidecar"
        ));
        assert!(matches!(
            crate::browser::host::unsupported_request_response("fixture", &wrong_command),
            Err(BrowserError::InvalidInvocation { field }) if field == "repairSidecar"
        ));
        coordinator.abort_locator_repair_capture(&repair);
        drop(wrong_command);

        let repair = coordinator
            .reserve_locator_repair_capture(
                &instance,
                &store,
                0,
                BrowserReplayLocatorSlot::PrimaryAction,
                "tab-a",
                BrowserRevision(9),
                BrowserReplayRepairResumeCursor::Action,
            )
            .unwrap();
        let cancelled_controller = controller.clone();
        let task_coordinator = coordinator.clone();
        let task_repair = repair.clone();
        let task = tokio::spawn(async move {
            cancelled_controller
                .request_replay_repair_capture(
                    &task_coordinator,
                    &task_repair,
                    BrowserCommand::Snapshot {
                        tab_id: "tab-a".to_string(),
                    },
                    BrowserInvocationContext::agent("cancelled snapshot", BrowserRisk::Normal)
                        .unwrap(),
                )
                .await
        });
        let request = inbox.recv().await.expect("sidecar request reaches host");
        assert!(matches!(
            request.retain_repair_resource_for_test(
                &other_store,
                BrowserResourceKind::ReplayRepairSnapshot,
                "application/json",
                b"{}",
            ),
            Err(BrowserError::InvalidInvocation { field }) if field == "repairSidecar"
        ));
        task.abort();
        let _ = task.await;
        tokio::task::yield_now().await;
        assert!(matches!(
            request.retain_repair_resource_for_test(
                &store,
                BrowserResourceKind::ReplayRepairSnapshot,
                "application/json",
                b"{}",
            ),
            Err(BrowserError::InvalidInvocation { field }) if field == "repairSidecar"
        ));
        assert_eq!(
            coordinator.status(&instance).unwrap().status,
            crate::browser::BrowserReplayStatus::Running
        );

        drop(request);

        let swapped_repair = coordinator
            .reserve_locator_repair_capture(
                &instance,
                &store,
                0,
                BrowserReplayLocatorSlot::PrimaryAction,
                "tab-a",
                BrowserRevision(9),
                BrowserReplayRepairResumeCursor::Action,
            )
            .unwrap();
        let swapped_controller = controller.clone();
        let swapped_coordinator = coordinator.clone();
        let task_repair = swapped_repair.clone();
        let swapped_task = tokio::spawn(async move {
            swapped_controller
                .request_replay_repair_capture(
                    &swapped_coordinator,
                    &task_repair,
                    BrowserCommand::Snapshot {
                        tab_id: "tab-a".to_string(),
                    },
                    BrowserInvocationContext::agent("swapped response", BrowserRisk::Normal)
                        .unwrap(),
                )
                .await
        });
        let swapped_request = inbox.recv().await.expect("swapped request reaches host");
        let swapped_resource = swapped_request
            .retain_repair_resource_for_test(
                &store,
                BrowserResourceKind::ReplayRepairSnapshot,
                "application/json",
                b"{}",
            )
            .unwrap();
        swapped_request.respond(Ok(BrowserResponse::Screenshot {
            resource: swapped_resource.clone(),
        }));
        assert!(matches!(
            swapped_task.await.unwrap(),
            Err(BrowserError::InvalidInvocation { field }) if field == "repairSidecar"
        ));
        assert!(matches!(
            store.handle(&workspace_key, &swapped_resource.id),
            Err(BrowserError::MissingResource { .. })
        ));

        const PATH_SENTINEL: &str = "DM_REPAIR_PATH_SENTINEL_5D8C";
        let io_repair = coordinator
            .reserve_locator_repair_capture(
                &instance,
                &store,
                0,
                BrowserReplayLocatorSlot::PrimaryAction,
                "tab-a",
                BrowserRevision(9),
                BrowserReplayRepairResumeCursor::Action,
            )
            .unwrap();
        let io_controller = controller.clone();
        let io_coordinator = coordinator.clone();
        let task_repair = io_repair.clone();
        let io_task = tokio::spawn(async move {
            io_controller
                .request_replay_repair_capture(
                    &io_coordinator,
                    &task_repair,
                    BrowserCommand::Snapshot {
                        tab_id: "tab-a".to_string(),
                    },
                    BrowserInvocationContext::agent(
                        "path-bearing host failure",
                        BrowserRisk::Normal,
                    )
                    .unwrap(),
                )
                .await
        });
        let io_request = inbox.recv().await.expect("io request reaches host");
        io_request.respond(Err(BrowserError::Io {
            operation: PATH_SENTINEL.to_string(),
            path: PathBuf::from(PATH_SENTINEL),
            message: PATH_SENTINEL.to_string(),
        }));
        let error = io_task.await.unwrap().unwrap_err();
        assert_eq!(error, BrowserError::ResourceRootUnavailable);
        for surface in [format!("{error:?}"), serde_json::to_string(&error).unwrap()] {
            assert!(!surface.contains(PATH_SENTINEL), "{surface}");
        }

        let terminal_repair = coordinator
            .reserve_locator_repair_capture(
                &instance,
                &store,
                0,
                BrowserReplayLocatorSlot::PrimaryAction,
                "tab-a",
                BrowserRevision(9),
                BrowserReplayRepairResumeCursor::Action,
            )
            .unwrap();
        let terminal_controller = controller.clone();
        let terminal_coordinator = coordinator.clone();
        let task_repair = terminal_repair.clone();
        let terminal_task = tokio::spawn(async move {
            terminal_controller
                .request_replay_repair_capture(
                    &terminal_coordinator,
                    &task_repair,
                    BrowserCommand::Snapshot {
                        tab_id: "tab-a".to_string(),
                    },
                    BrowserInvocationContext::agent("terminal cancellation", BrowserRisk::Normal)
                        .unwrap(),
                )
                .await
        });
        let terminal_request = inbox.recv().await.expect("terminal request reaches host");
        coordinator.cancel(&instance).unwrap();
        assert!(matches!(
            terminal_request.retain_repair_resource_for_test(
                &store,
                BrowserResourceKind::ReplayRepairSnapshot,
                "application/json",
                b"{}",
            ),
            Err(BrowserError::InvalidInvocation { field }) if field == "repairSidecar"
        ));
        terminal_request.respond(Err(BrowserError::Interrupted));
        assert_eq!(terminal_task.await.unwrap(), Err(BrowserError::Interrupted));
        assert_eq!(
            coordinator.status(&instance).unwrap().status,
            crate::browser::BrowserReplayStatus::Cancelled
        );

        drop(coordinator);
        drop(other_store);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(other_root).unwrap();
    }

    #[tokio::test]
    async fn secure_command_method_enqueues_only_an_exact_agent_marker_and_lease_pair() {
        let (bridge, mut inbox) = browser_command_channel(2);
        let workspace_key = workspace("project-a", "conversation-a");
        let controller = bridge.bind(workspace_key.clone(), Duration::from_secs(1));
        let (_coordinator, instance, lease) = installed_secret(&workspace_key, SECRET_INPUT);
        let task = tokio::spawn(async move {
            controller
                .request_replay_secret_type(marker(SECRET_INPUT), agent_context(), instance, lease)
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
    async fn secure_command_rejects_colliding_live_foreign_replay_scope_at_controller_and_host() {
        let (bridge, mut inbox) = browser_command_channel(2);
        let workspace_key = workspace("project-a", "conversation-a");
        let controller = bridge.bind(workspace_key.clone(), Duration::from_millis(100));
        let (_left_coordinator, left_instance, left_lease) =
            installed_secret(&workspace_key, SECRET_INPUT);
        let (_right_coordinator, right_instance, right_lease) =
            installed_secret(&workspace_key, SECRET_INPUT);

        assert_eq!(
            left_instance.workspace_key(),
            right_instance.workspace_key()
        );
        assert_eq!(left_instance.id(), right_instance.id());
        assert_ne!(left_instance, right_instance, "opaque scopes must differ");
        assert!(matches!(
            controller
                .request_replay_secret_type(
                    marker(SECRET_INPUT),
                    agent_context(),
                    left_instance.clone(),
                    right_lease,
                )
                .await,
            Err(BrowserError::InvalidInvocation { field }) if field == "secretSidecar"
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(20), inbox.recv())
                .await
                .is_err()
        );

        let forged = forged_request(
            workspace_key,
            marker(SECRET_INPUT),
            Some(BrowserReplaySecretSidecar {
                expected_instance: right_instance,
                lease: left_lease,
            }),
        );
        assert!(matches!(
            forged.validate_secret_sidecar(),
            Err(BrowserError::InvalidInvocation { field }) if field == "secretSidecar"
        ));
    }

    #[tokio::test]
    async fn secure_command_real_installed_authority_never_leaks_value_to_safe_surfaces() {
        let (bridge, mut inbox) = browser_command_channel(1);
        let workspace_key = workspace("project-a", "conversation-a");
        let controller = bridge.bind(workspace_key.clone(), Duration::from_secs(1));
        let (coordinator, instance, lease) = installed_secret(&workspace_key, SECRET_INPUT);
        let request_controller = controller.clone();
        let request_instance = instance.clone();
        let task = tokio::spawn(async move {
            request_controller
                .request_replay_secret_type(
                    marker(SECRET_INPUT),
                    agent_context(),
                    request_instance,
                    lease,
                )
                .await
        });

        let request = inbox.recv().await.expect("secure request");
        request
            .validate_secret_sidecar()
            .expect("installed exact authority");
        for surface in [
            format!("{:?}", request.command()),
            serde_json::to_string(request.command()).unwrap(),
            format!("{:?}", request.context()),
            serde_json::to_string(request.context()).unwrap(),
        ] {
            assert!(!surface.contains(SECRET_VALUE));
        }

        coordinator.cancel(&instance).unwrap();
        assert!(request.cancellation_is_current());
        let error = match request.validate_secret_sidecar() {
            Err(error) => error,
            Ok(_) => panic!("closed replay authority must be rejected"),
        };
        for surface in [
            error.to_string(),
            format!("{error:?}"),
            serde_json::to_string(&error).unwrap(),
        ] {
            assert!(!surface.contains(SECRET_VALUE));
        }
        request.respond(Err(error));
        assert!(matches!(
            task.await.unwrap(),
            Err(BrowserError::InvalidInvocation { field }) if field == "secretSidecar"
        ));
    }

    #[tokio::test]
    async fn secure_command_validated_unsupported_ingress_preserves_platform_error() {
        let (bridge, mut inbox) = browser_command_channel(1);
        let workspace_key = workspace("project-a", "conversation-a");
        let controller = bridge.bind(workspace_key.clone(), Duration::from_secs(1));
        let (_coordinator, instance, lease) = installed_secret(&workspace_key, SECRET_INPUT);
        let task = tokio::spawn(async move {
            controller
                .request_replay_secret_type(marker(SECRET_INPUT), agent_context(), instance, lease)
                .await
        });

        let request = inbox.recv().await.expect("secure request");
        request
            .validate_secret_sidecar()
            .expect("exact sidecar validates at ingress");
        let result = crate::browser::host::unsupported_validated_command_response(
            "fixture",
            request.command().clone(),
        );
        assert_eq!(
            result,
            Err(BrowserError::UnavailablePlatform {
                platform: "fixture".to_string(),
            })
        );
        request.respond(result);
        assert_eq!(
            task.await.unwrap(),
            Err(BrowserError::UnavailablePlatform {
                platform: "fixture".to_string(),
            })
        );
    }

    #[tokio::test]
    async fn secure_command_method_rejects_wrong_actor_workspace_input_and_stale_store() {
        let (bridge, mut inbox) = browser_command_channel(4);
        let workspace_key = workspace("project-a", "conversation-a");
        let controller = bridge.bind(workspace_key.clone(), Duration::from_millis(100));

        let (_actor_coordinator, actor_instance, actor_lease) =
            installed_secret(&workspace_key, SECRET_INPUT);
        let user_context =
            BrowserInvocationContext::user("type replay secret", BrowserRisk::Normal).unwrap();
        assert!(matches!(
            controller
                .request_replay_secret_type(
                    marker(SECRET_INPUT),
                    user_context,
                    actor_instance,
                    actor_lease,
                )
                .await,
            Err(BrowserError::InvalidInvocation { field }) if field == "secretSidecar"
        ));

        let foreign_workspace = workspace("project-b", "conversation-b");
        let (_foreign_coordinator, foreign_instance, foreign_lease) =
            installed_secret(&foreign_workspace, SECRET_INPUT);
        assert!(matches!(
            controller
                .request_replay_secret_type(
                    marker(SECRET_INPUT),
                    agent_context(),
                    foreign_instance,
                    foreign_lease,
                )
                .await,
            Err(BrowserError::InvalidInvocation { field }) if field == "secretSidecar"
        ));

        let (_input_coordinator, input_instance, input_lease) =
            installed_secret(&workspace_key, SECRET_INPUT);
        assert!(matches!(
            controller
                .request_replay_secret_type(
                    marker("other-input"),
                    agent_context(),
                    input_instance,
                    input_lease,
                )
                .await,
            Err(BrowserError::InvalidInvocation { field }) if field == "secretSidecar"
        ));

        let (stale_coordinator, stale_instance, stale_lease) =
            installed_secret(&workspace_key, SECRET_INPUT);
        stale_coordinator.cancel(&stale_instance).unwrap();
        assert!(matches!(
            controller
                .request_replay_secret_type(
                    marker(SECRET_INPUT),
                    agent_context(),
                    stale_instance,
                    stale_lease,
                )
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

        let (_command_coordinator, command_instance, command_lease) =
            installed_secret(&workspace_key, SECRET_INPUT);
        let wrong_command = forged_request(
            workspace_key.clone(),
            BrowserCommand::Status,
            Some(BrowserReplaySecretSidecar {
                expected_instance: command_instance,
                lease: command_lease,
            }),
        );
        assert!(matches!(
            wrong_command.validate_secret_sidecar(),
            Err(BrowserError::InvalidInvocation { field }) if field == "secretSidecar"
        ));

        let (_input_coordinator, input_instance, input_lease) =
            installed_secret(&workspace_key, SECRET_INPUT);
        let wrong_input = forged_request(
            workspace_key.clone(),
            marker("other-input"),
            Some(BrowserReplaySecretSidecar {
                expected_instance: input_instance,
                lease: input_lease,
            }),
        );
        assert!(matches!(
            wrong_input.validate_secret_sidecar(),
            Err(BrowserError::InvalidInvocation { field }) if field == "secretSidecar"
        ));

        let foreign_workspace = workspace("project-b", "conversation-b");
        let (_workspace_coordinator, workspace_instance, workspace_lease) =
            installed_secret(&foreign_workspace, SECRET_INPUT);
        let wrong_workspace = forged_request(
            workspace_key.clone(),
            marker(SECRET_INPUT),
            Some(BrowserReplaySecretSidecar {
                expected_instance: workspace_instance,
                lease: workspace_lease,
            }),
        );
        assert!(matches!(
            wrong_workspace.validate_secret_sidecar(),
            Err(BrowserError::InvalidInvocation { field }) if field == "secretSidecar"
        ));

        let (stale_coordinator, stale_instance, stale_lease) =
            installed_secret(&workspace_key, SECRET_INPUT);
        stale_coordinator.cancel(&stale_instance).unwrap();
        let stale = forged_request(
            workspace_key,
            marker(SECRET_INPUT),
            Some(BrowserReplaySecretSidecar {
                expected_instance: stale_instance,
                lease: stale_lease,
            }),
        );
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
        let (_coordinator, instance, lease) = installed_secret(&workspace_key, SECRET_INPUT);
        let task = tokio::spawn(async move {
            request_controller
                .request_replay_secret_type(marker(SECRET_INPUT), agent_context(), instance, lease)
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
        let (_coordinator, instance, lease) = installed_secret(&workspace_key, SECRET_INPUT);
        let task = tokio::spawn(async move {
            request_controller
                .request_replay_secret_type(marker(SECRET_INPUT), agent_context(), instance, lease)
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn response_and_tab_cancellation_follow_one_forced_linearization_order() {
        let workspace_key = workspace("project-a", "conversation-a");

        let (bridge, mut inbox) = browser_command_channel(2);
        let controller = bridge.bind(workspace_key.clone(), Duration::from_secs(1));
        let pending = tokio::spawn(async move {
            controller
                .request(BrowserCommand::Reload {
                    tab_id: "tab-a".to_string(),
                })
                .await
        });
        let request = inbox.recv().await.expect("response-first request");
        let (response_entered_tx, response_entered_rx) = std::sync::mpsc::sync_channel(0);
        let (release_response_tx, release_response_rx) = std::sync::mpsc::sync_channel(0);
        let response = std::thread::spawn(move || {
            request.respond_with_linearization_hook(Ok(BrowserResponse::Acknowledged), || {
                response_entered_tx.send(()).unwrap();
                release_response_rx.recv().unwrap();
            });
        });
        response_entered_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("response holds the linearization gate");

        let cancellation_bridge = bridge.clone();
        let cancellation_key = workspace_key.clone();
        let (cancellation_attempted_tx, cancellation_attempted_rx) =
            std::sync::mpsc::sync_channel(0);
        let (cancellation_done_tx, cancellation_done_rx) = std::sync::mpsc::channel();
        let cancellation = std::thread::spawn(move || {
            cancellation_attempted_tx.send(()).unwrap();
            cancellation_bridge.interrupt_tab(&cancellation_key, "tab-a");
            cancellation_done_tx.send(()).unwrap();
        });
        cancellation_attempted_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("cancellation attempts the occupied gate");
        assert!(
            cancellation_done_rx
                .recv_timeout(Duration::from_millis(25))
                .is_err(),
            "cancellation must wait behind the earlier response"
        );
        release_response_tx.send(()).unwrap();
        response.join().unwrap();
        cancellation.join().unwrap();
        assert_eq!(pending.await.unwrap(), Ok(BrowserResponse::Acknowledged));

        let (bridge, mut inbox) = browser_command_channel(2);
        let controller = bridge.bind(workspace_key.clone(), Duration::from_secs(1));
        let pending = tokio::spawn(async move {
            controller
                .request(BrowserCommand::Reload {
                    tab_id: "tab-a".to_string(),
                })
                .await
        });
        let request = inbox.recv().await.expect("cancellation-first request");
        let cancellation_bridge = bridge.clone();
        let cancellation_key = workspace_key.clone();
        let (cancellation_entered_tx, cancellation_entered_rx) = std::sync::mpsc::sync_channel(0);
        let (release_cancellation_tx, release_cancellation_rx) = std::sync::mpsc::sync_channel(0);
        let cancellation = std::thread::spawn(move || {
            cancellation_bridge.interrupt_control_with_linearization_hook(
                BrowserHostControl::InterruptTab {
                    workspace_key: cancellation_key,
                    tab_id: "tab-a".to_string(),
                },
                || {
                    cancellation_entered_tx.send(()).unwrap();
                    release_cancellation_rx.recv().unwrap();
                },
            );
        });
        cancellation_entered_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("cancellation holds the linearization gate");

        let (response_attempted_tx, response_attempted_rx) = std::sync::mpsc::sync_channel(0);
        let (response_done_tx, response_done_rx) = std::sync::mpsc::channel();
        let response = std::thread::spawn(move || {
            response_attempted_tx.send(()).unwrap();
            request.respond(Ok(BrowserResponse::Acknowledged));
            response_done_tx.send(()).unwrap();
        });
        response_attempted_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("response attempts the occupied gate");
        assert!(
            response_done_rx
                .recv_timeout(Duration::from_millis(25))
                .is_err(),
            "response must wait behind the earlier cancellation"
        );
        release_cancellation_tx.send(()).unwrap();
        cancellation.join().unwrap();
        response.join().unwrap();
        assert_eq!(pending.await.unwrap(), Err(BrowserError::Interrupted));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn registration_revocation_linearizes_before_a_waiting_response() {
        let workspace_key = workspace("project-a", "conversation-a");
        let registration = BrowserRegistrationLease::new();
        let (bridge, mut inbox) = browser_command_channel(1);
        let controller = bridge.bind_with_registration_lease(
            workspace_key.clone(),
            Duration::from_secs(1),
            Some(registration.clone()),
        );
        let pending = tokio::spawn(async move {
            controller
                .request(BrowserCommand::Reload {
                    tab_id: "tab-a".to_string(),
                })
                .await
        });
        let request = inbox.recv().await.expect("registered request");

        let revocation_bridge = bridge.clone();
        let revocation_key = workspace_key.clone();
        let revocation_lease = registration.clone();
        let (revocation_entered_tx, revocation_entered_rx) = std::sync::mpsc::sync_channel(0);
        let (release_revocation_tx, release_revocation_rx) = std::sync::mpsc::sync_channel(0);
        let revocation = std::thread::spawn(move || {
            revocation_bridge.revoke_registration_with_linearization_hook(
                &revocation_key,
                &revocation_lease,
                || {
                    revocation_entered_tx.send(()).unwrap();
                    release_revocation_rx.recv().unwrap();
                },
            );
        });
        revocation_entered_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("revocation holds the linearization gate");

        let (response_attempted_tx, response_attempted_rx) = std::sync::mpsc::sync_channel(0);
        let (response_done_tx, response_done_rx) = std::sync::mpsc::channel();
        let response = std::thread::spawn(move || {
            response_attempted_tx.send(()).unwrap();
            request.respond(Ok(BrowserResponse::Acknowledged));
            response_done_tx.send(()).unwrap();
        });
        response_attempted_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("response attempts the occupied gate");
        assert!(
            response_done_rx
                .recv_timeout(Duration::from_millis(25))
                .is_err(),
            "response must wait behind the earlier registration revocation"
        );
        release_revocation_tx.send(()).unwrap();
        revocation.join().unwrap();
        response.join().unwrap();
        assert_eq!(pending.await.unwrap(), Err(BrowserError::Interrupted));
        assert!(!registration.is_current(BrowserRegistrationLeaseTicket(0)));
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
