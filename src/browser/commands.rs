use super::{
    BrowserAction, BrowserActionResult, BrowserActionTarget, BrowserConsoleEntry,
    BrowserConsoleOperation, BrowserDownloadEntry, BrowserDownloadOperation, BrowserError,
    BrowserNetworkEntry, BrowserNetworkOperation, BrowserPerformanceOperation,
    BrowserPerformanceSnapshot, BrowserResourceHandle, BrowserRisk, BrowserScreenshotMode,
    BrowserSnapshotSummary, BrowserTabSnapshot, BrowserUploadResult, BrowserViewport,
    BrowserWaitCondition, BrowserWaitResult, BrowserWorkspaceKey, BrowserWorkspaceMutation,
    BrowserWorkspaceSnapshot,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::marker::PhantomData;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
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
            | Self::CloseTab { tab_id }
            | Self::Navigate { tab_id, .. }
            | Self::Back { tab_id }
            | Self::Forward { tab_id }
            | Self::Reload { tab_id }
            | Self::UpdateViewport { tab_id, .. }
            | Self::OpenDevTools { tab_id }
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
            | Self::ListTabs
            | Self::CreateTab { .. }
            | Self::ResetWorkspace
            | Self::ClearProjectProfile
            | Self::DownloadDirectory => None,
        }
    }
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
    Acknowledged,
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

struct BrowserCommandEnvelope {
    workspace_key: BrowserWorkspaceKey,
    command: BrowserCommand,
    context: BrowserInvocationContext,
    response: oneshot::Sender<Result<BrowserResponse, BrowserError>>,
    pending_work: PendingWorkGuard,
}

#[derive(Clone)]
pub struct BrowserCommandBridge {
    sender: mpsc::Sender<BrowserCommandEnvelope>,
    cancellations: Arc<CancellationEpochs>,
    pending_work: Arc<PendingWork>,
}

impl BrowserCommandBridge {
    pub fn bind(&self, workspace_key: BrowserWorkspaceKey, timeout: Duration) -> BrowserController {
        BrowserController {
            workspace_key,
            sender: self.sender.clone(),
            timeout,
            cancellations: Arc::clone(&self.cancellations),
            pending_work: Arc::clone(&self.pending_work),
        }
    }

    pub fn pending_work_count(&self) -> usize {
        self.pending_work.count()
    }

    pub fn observe_host_event(&self, event: &BrowserHostEvent) {
        self.cancellations.observe_host_event(event);
    }

    pub fn interrupt_workspace(&self, workspace_key: &BrowserWorkspaceKey) {
        self.cancellations.interrupt_workspace(workspace_key);
    }

    pub fn interrupt_tab(&self, workspace_key: &BrowserWorkspaceKey, tab_id: &str) {
        self.cancellations.interrupt_tab(workspace_key, tab_id);
    }
}

#[derive(Clone)]
pub struct BrowserController {
    workspace_key: BrowserWorkspaceKey,
    sender: mpsc::Sender<BrowserCommandEnvelope>,
    timeout: Duration,
    cancellations: Arc<CancellationEpochs>,
    pending_work: Arc<PendingWork>,
}

impl BrowserController {
    pub fn workspace_key(&self) -> &BrowserWorkspaceKey {
        &self.workspace_key
    }

    pub fn pending_work_count(&self) -> usize {
        self.pending_work.count()
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
        context.validate()?;
        self.interrupt_for_command(&command);
        let operation = command.operation_name().to_string();
        let cancellations = self
            .cancellations
            .subscribe(&self.workspace_key, command.tab_id());
        let mut workspace_cancellation = cancellations.workspace;
        let mut tab_cancellation = cancellations.tab;
        let (response, receiver) = oneshot::channel();
        let timeout = tokio::time::sleep(self.timeout);
        tokio::pin!(timeout);
        let send = self.sender.send(BrowserCommandEnvelope {
            workspace_key: self.workspace_key.clone(),
            command,
            context,
            response,
            pending_work: self.pending_work.track(),
        });
        tokio::pin!(send);
        tokio::select! {
            result = &mut send => result.map_err(|_| BrowserError::CrashedView {
                message: "browser command inbox is closed".to_string(),
            })?,
            _ = workspace_cancellation.changed() => return Err(BrowserError::Interrupted),
            _ = wait_for_tab_cancellation(&mut tab_cancellation) => {
                return Err(BrowserError::Interrupted);
            }
            _ = &mut timeout => return Err(BrowserError::Timeout { operation }),
        }
        tokio::select! {
            response = receiver => response.unwrap_or_else(|_| {
                Err(BrowserError::CrashedView {
                    message: "browser command request was dropped without a response".to_string(),
                })
            }),
            _ = workspace_cancellation.changed() => Err(BrowserError::Interrupted),
            _ = wait_for_tab_cancellation(&mut tab_cancellation) => Err(BrowserError::Interrupted),
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
        self.interrupt_for_command(&command);
        let (response, receiver) = oneshot::channel();
        drop(receiver);
        self.sender
            .send(BrowserCommandEnvelope {
                workspace_key: self.workspace_key.clone(),
                command,
                context,
                response,
                pending_work: self.pending_work.track(),
            })
            .await
            .map_err(|_| BrowserError::CrashedView {
                message: "browser command inbox is closed".to_string(),
            })
    }

    pub fn interrupt_workspace(&self) {
        self.cancellations.interrupt_workspace(&self.workspace_key);
    }

    pub fn interrupt_tab(&self, tab_id: &str) {
        self.cancellations
            .interrupt_tab(&self.workspace_key, tab_id);
    }

    fn interrupt_for_command(&self, command: &BrowserCommand) {
        if let BrowserCommand::Stop { tab_id } = command {
            if let Some(tab_id) = tab_id {
                self.interrupt_tab(tab_id);
            } else {
                self.interrupt_workspace();
            }
        }
    }
}

pub struct BrowserCommandInbox {
    receiver: mpsc::Receiver<BrowserCommandEnvelope>,
    cancellations: Arc<CancellationEpochs>,
    pending_work: Arc<PendingWork>,
    _main_thread_only: PhantomData<Rc<()>>,
}

impl BrowserCommandInbox {
    pub async fn recv(&mut self) -> Option<BrowserCommandRequest> {
        self.receiver.recv().await.map(BrowserCommandRequest::from)
    }

    pub fn pending_work_count(&self) -> usize {
        self.pending_work.count()
    }

    pub fn interrupt_workspace(&self, workspace_key: &BrowserWorkspaceKey) {
        self.cancellations.interrupt_workspace(workspace_key);
    }

    pub fn interrupt_tab(&self, workspace_key: &BrowserWorkspaceKey, tab_id: &str) {
        self.cancellations.interrupt_tab(workspace_key, tab_id);
    }

    pub fn observe_host_event(&self, event: &BrowserHostEvent) {
        self.cancellations.observe_host_event(event);
    }
}

pub struct BrowserCommandRequest {
    workspace_key: BrowserWorkspaceKey,
    command: BrowserCommand,
    context: BrowserInvocationContext,
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

impl From<BrowserCommandEnvelope> for BrowserCommandRequest {
    fn from(envelope: BrowserCommandEnvelope) -> Self {
        let BrowserCommandEnvelope {
            workspace_key,
            command,
            context,
            response,
            pending_work,
        } = envelope;
        Self {
            workspace_key,
            command,
            context,
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
    let pending_work = Arc::new(PendingWork::default());
    (
        BrowserCommandBridge {
            sender,
            cancellations: Arc::clone(&cancellations),
            pending_work: Arc::clone(&pending_work),
        },
        BrowserCommandInbox {
            receiver,
            cancellations,
            pending_work,
            _main_thread_only: PhantomData,
        },
    )
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

#[derive(Default)]
struct CancellationEpochs {
    workspaces: Mutex<HashMap<BrowserWorkspaceKey, watch::Sender<u64>>>,
    tabs: Mutex<HashMap<(BrowserWorkspaceKey, String), watch::Sender<u64>>>,
}

impl CancellationEpochs {
    fn subscribe(
        &self,
        workspace_key: &BrowserWorkspaceKey,
        tab_id: Option<&str>,
    ) -> CancellationSubscriptions {
        let workspace = sender_for(&mut lock(&self.workspaces), workspace_key.clone()).subscribe();
        let tab = tab_id.map(|tab_id| {
            sender_for(
                &mut lock(&self.tabs),
                (workspace_key.clone(), tab_id.to_string()),
            )
            .subscribe()
        });
        CancellationSubscriptions { workspace, tab }
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

struct CancellationSubscriptions {
    workspace: watch::Receiver<u64>,
    tab: Option<watch::Receiver<u64>>,
}

async fn wait_for_tab_cancellation(tab: &mut Option<watch::Receiver<u64>>) {
    match tab {
        Some(tab) => {
            let _ = tab.changed().await;
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

fn advance(sender: &watch::Sender<u64>) {
    let next = (*sender.borrow()).saturating_add(1);
    sender.send_replace(next);
}
