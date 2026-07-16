use super::{
    BrowserError, BrowserTabSnapshot, BrowserViewport, BrowserWorkspaceKey,
    BrowserWorkspaceMutation, BrowserWorkspaceSnapshot,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::marker::PhantomData;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, watch};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum BrowserCommand {
    Status,
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
}

impl BrowserCommand {
    fn operation_name(&self) -> &'static str {
        match self {
            Self::Status => "status",
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
            | Self::OpenDevTools { tab_id } => Some(tab_id),
            Self::Stop { tab_id } => tab_id.as_deref(),
            Self::Status
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
    Input,
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
    response: oneshot::Sender<Result<BrowserResponse, BrowserError>>,
}

#[derive(Clone)]
pub struct BrowserCommandBridge {
    sender: mpsc::Sender<BrowserCommandEnvelope>,
    cancellations: Arc<CancellationEpochs>,
}

impl BrowserCommandBridge {
    pub fn bind(&self, workspace_key: BrowserWorkspaceKey, timeout: Duration) -> BrowserController {
        BrowserController {
            workspace_key,
            sender: self.sender.clone(),
            timeout,
            cancellations: Arc::clone(&self.cancellations),
        }
    }
}

#[derive(Clone)]
pub struct BrowserController {
    workspace_key: BrowserWorkspaceKey,
    sender: mpsc::Sender<BrowserCommandEnvelope>,
    timeout: Duration,
    cancellations: Arc<CancellationEpochs>,
}

impl BrowserController {
    pub fn workspace_key(&self) -> &BrowserWorkspaceKey {
        &self.workspace_key
    }

    pub async fn request(&self, command: BrowserCommand) -> Result<BrowserResponse, BrowserError> {
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
            response,
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
        self.interrupt_for_command(&command);
        let (response, receiver) = oneshot::channel();
        drop(receiver);
        self.sender
            .send(BrowserCommandEnvelope {
                workspace_key: self.workspace_key.clone(),
                command,
                response,
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
    _main_thread_only: PhantomData<Rc<()>>,
}

impl BrowserCommandInbox {
    pub async fn recv(&mut self) -> Option<BrowserCommandRequest> {
        self.receiver.recv().await.map(BrowserCommandRequest::from)
    }

    pub fn interrupt_workspace(&self, workspace_key: &BrowserWorkspaceKey) {
        self.cancellations.interrupt_workspace(workspace_key);
    }

    pub fn interrupt_tab(&self, workspace_key: &BrowserWorkspaceKey, tab_id: &str) {
        self.cancellations.interrupt_tab(workspace_key, tab_id);
    }

    pub fn observe_host_event(&self, event: &BrowserHostEvent) {
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

pub struct BrowserCommandRequest {
    workspace_key: BrowserWorkspaceKey,
    command: BrowserCommand,
    response: oneshot::Sender<Result<BrowserResponse, BrowserError>>,
}

impl BrowserCommandRequest {
    pub fn workspace_key(&self) -> &BrowserWorkspaceKey {
        &self.workspace_key
    }

    pub fn command(&self) -> &BrowserCommand {
        &self.command
    }

    pub fn respond(self, result: Result<BrowserResponse, BrowserError>) {
        let _ = self.response.send(result);
    }
}

impl From<BrowserCommandEnvelope> for BrowserCommandRequest {
    fn from(envelope: BrowserCommandEnvelope) -> Self {
        Self {
            workspace_key: envelope.workspace_key,
            command: envelope.command,
            response: envelope.response,
        }
    }
}

pub fn browser_command_channel(capacity: usize) -> (BrowserCommandBridge, BrowserCommandInbox) {
    let (sender, receiver) = mpsc::channel(capacity.max(1));
    let cancellations = Arc::new(CancellationEpochs::default());
    (
        BrowserCommandBridge {
            sender,
            cancellations: Arc::clone(&cancellations),
        },
        BrowserCommandInbox {
            receiver,
            cancellations,
            _main_thread_only: PhantomData,
        },
    )
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
