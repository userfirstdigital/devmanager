use super::{
    classify_upload_path, effective_browser_risk, resource_id_from_uri,
    verified_authenticated_local_project_root, BrowserAction, BrowserActionTarget,
    BrowserAnnotationOperation, BrowserCommand, BrowserConsoleOperation, BrowserController,
    BrowserDownloadOperation, BrowserError, BrowserInvocationContext, BrowserNetworkOperation,
    BrowserPerformanceOperation, BrowserRecordingOperation, BrowserResourceStore, BrowserResponse,
    BrowserRisk, BrowserScreenshotMode, BrowserTabSnapshot, BrowserWaitCondition,
    BrowserWorkspaceSnapshot,
};
use base64::Engine as _;
use rmcp::handler::server::{router::tool::ToolRouter, wrapper::Parameters};
use rmcp::model::{
    CallToolResult, Implementation, ListResourcesResult, PaginatedRequestParams,
    ReadResourceRequestParams, ReadResourceResult, Resource, ResourceContents, ServerCapabilities,
    ServerInfo,
};
use rmcp::schemars;
use rmcp::service::RequestContext;
use rmcp::{tool, tool_handler, tool_router, ErrorData, RoleServer, ServerHandler};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

const MAX_BROWSER_MCP_INTENT_BYTES: usize = 1_024;

#[derive(Debug, Clone, Copy, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(rename_all = "camelCase")]
enum BrowserMcpRisk {
    Normal,
    Financial,
    Destructive,
    AccountSecurity,
    PermissionChange,
    OutsideWorkspaceFile,
    OsPermission,
}

impl From<BrowserMcpRisk> for BrowserRisk {
    fn from(value: BrowserMcpRisk) -> Self {
        match value {
            BrowserMcpRisk::Normal => Self::Normal,
            BrowserMcpRisk::Financial => Self::Financial,
            BrowserMcpRisk::Destructive => Self::Destructive,
            BrowserMcpRisk::AccountSecurity => Self::AccountSecurity,
            BrowserMcpRisk::PermissionChange => Self::PermissionChange,
            BrowserMcpRisk::OutsideWorkspaceFile => Self::OutsideWorkspaceFile,
            BrowserMcpRisk::OsPermission => Self::OsPermission,
        }
    }
}

#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct BrowserStatusRequest {
    intent: String,
    risk: BrowserMcpRisk,
}

#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BrowserAnnotationsRequestWire {
    intent: String,
    risk: BrowserMcpRisk,
    operation: BrowserAnnotationOperation,
    annotation_id: Option<String>,
}

#[derive(Debug)]
struct BrowserAnnotationsRequest {
    parsed: Result<BrowserAnnotationsRequestWire, String>,
}

#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BrowserRecordingRequestWire {
    #[schemars(length(max = 1024))]
    intent: String,
    risk: BrowserMcpRisk,
    operation: BrowserRecordingOperation,
}

#[derive(Debug)]
struct BrowserRecordingRequest {
    parsed: Result<BrowserRecordingRequestWire, String>,
}

impl<'de> Deserialize<'de> for BrowserRecordingRequest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        Ok(Self {
            parsed: serde_json::from_value(value).map_err(|error| error.to_string()),
        })
    }
}

impl rmcp::schemars::JsonSchema for BrowserRecordingRequest {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "BrowserRecordingRequest".into()
    }

    fn json_schema(generator: &mut rmcp::schemars::SchemaGenerator) -> rmcp::schemars::Schema {
        BrowserRecordingRequestWire::json_schema(generator)
    }
}

impl<'de> Deserialize<'de> for BrowserAnnotationsRequest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        Ok(Self {
            parsed: serde_json::from_value(value).map_err(|error| error.to_string()),
        })
    }
}

impl rmcp::schemars::JsonSchema for BrowserAnnotationsRequest {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "BrowserAnnotationsRequest".into()
    }

    fn json_schema(generator: &mut rmcp::schemars::SchemaGenerator) -> rmcp::schemars::Schema {
        BrowserAnnotationsRequestWire::json_schema(generator)
    }
}

#[derive(Debug, Clone, Copy, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(rename_all = "camelCase")]
enum BrowserTabsOperation {
    List,
    Create,
    Select,
    Close,
}

#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct BrowserTabsRequest {
    intent: String,
    risk: BrowserMcpRisk,
    operation: BrowserTabsOperation,
    tab_id: Option<String>,
    url: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(rename_all = "camelCase")]
enum BrowserNavigateOperation {
    Goto,
    Back,
    Forward,
    Reload,
}

#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct BrowserNavigateRequest {
    intent: String,
    risk: BrowserMcpRisk,
    operation: BrowserNavigateOperation,
    tab_id: Option<String>,
    url: Option<String>,
}

#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct BrowserSnapshotRequest {
    intent: String,
    risk: BrowserMcpRisk,
    tab_id: Option<String>,
}

#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct BrowserScreenshotRequest {
    intent: String,
    risk: BrowserMcpRisk,
    tab_id: Option<String>,
    mode: BrowserScreenshotMode,
}

#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct BrowserWaitRequest {
    intent: String,
    risk: BrowserMcpRisk,
    tab_id: Option<String>,
    condition: BrowserWaitCondition,
    timeout_ms: u64,
}

#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct BrowserActRequest {
    intent: String,
    risk: BrowserMcpRisk,
    tab_id: Option<String>,
    actions: Vec<BrowserAction>,
}

#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct BrowserConsoleRequest {
    intent: String,
    risk: BrowserMcpRisk,
    tab_id: Option<String>,
    operation: BrowserConsoleOperation,
}

#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct BrowserNetworkRequest {
    intent: String,
    risk: BrowserMcpRisk,
    tab_id: Option<String>,
    operation: BrowserNetworkOperation,
    request_id: Option<String>,
}

#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct BrowserPerformanceRequest {
    intent: String,
    risk: BrowserMcpRisk,
    tab_id: Option<String>,
    operation: BrowserPerformanceOperation,
}

#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct BrowserUploadRequest {
    intent: String,
    risk: BrowserMcpRisk,
    tab_id: Option<String>,
    target: BrowserActionTarget,
    paths: Vec<PathBuf>,
}

#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct BrowserDownloadsRequest {
    intent: String,
    risk: BrowserMcpRisk,
    tab_id: Option<String>,
    operation: BrowserDownloadOperation,
    download_id: Option<String>,
}

#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct BrowserCdpRequest {
    intent: String,
    risk: BrowserMcpRisk,
    tab_id: Option<String>,
    method: String,
    params: Value,
}

struct BrowserMcpContext {
    controller: BrowserController,
    initial_snapshot: BrowserWorkspaceSnapshot,
    live_snapshot: Mutex<BrowserWorkspaceSnapshot>,
    first_use: Mutex<bool>,
    resource_store: BrowserResourceStore,
    project_root: PathBuf,
}

#[derive(Clone)]
pub(crate) struct BrowserMcpServer {
    context: Arc<BrowserMcpContext>,
    tool_router: ToolRouter<Self>,
}

impl BrowserMcpServer {
    pub(crate) fn new(
        controller: BrowserController,
        initial_snapshot: BrowserWorkspaceSnapshot,
        resource_store: BrowserResourceStore,
        project_root: PathBuf,
    ) -> Self {
        Self {
            context: Arc::new(BrowserMcpContext {
                controller,
                live_snapshot: Mutex::new(initial_snapshot.clone()),
                initial_snapshot,
                first_use: Mutex::new(false),
                resource_store,
                project_root,
            }),
            tool_router: Self::tool_router(),
        }
    }

    async fn validate_and_ensure(
        &self,
        context: &BrowserInvocationContext,
    ) -> Result<BrowserWorkspaceSnapshot, ToolFailure> {
        let mut first_use = self.context.first_use.lock().await;
        if !*first_use {
            let ensured = self
                .context
                .controller
                .request_with_context(
                    BrowserCommand::Ensure {
                        snapshot: self.context.initial_snapshot.clone(),
                    },
                    context.clone(),
                )
                .await
                .map_err(ToolFailure::from)?;
            self.apply_workspace_response(ensured).await?;
            let opened = self
                .context
                .controller
                .request_with_context(BrowserCommand::SetPaneOpen { open: true }, context.clone())
                .await
                .map_err(ToolFailure::from)?;
            self.apply_workspace_response(opened).await?;
            *first_use = true;
        }
        drop(first_use);
        self.refresh_workspace_state(context).await
    }

    async fn refresh_workspace_state(
        &self,
        context: &BrowserInvocationContext,
    ) -> Result<BrowserWorkspaceSnapshot, ToolFailure> {
        let response = self
            .context
            .controller
            .request_with_context(BrowserCommand::WorkspaceState, context.clone())
            .await
            .map_err(ToolFailure::from)?;
        let BrowserResponse::WorkspaceState { snapshot } = response else {
            return Err(ToolFailure::invalid_response(
                "browser host returned the wrong workspace-state response type",
            ));
        };
        *self.context.live_snapshot.lock().await = snapshot.clone();
        Ok(snapshot)
    }

    async fn apply_workspace_response(
        &self,
        response: BrowserResponse,
    ) -> Result<BrowserWorkspaceSnapshot, ToolFailure> {
        let BrowserResponse::Workspace { mutation } = response else {
            return Err(ToolFailure::invalid_response(
                "browser host returned the wrong response type",
            ));
        };
        let snapshot = mutation.snapshot;
        *self.context.live_snapshot.lock().await = snapshot.clone();
        Ok(snapshot)
    }

    async fn run_tabs_operation(&self, request: BrowserTabsRequest) -> Result<Value, ToolFailure> {
        let context = invocation_context(&request.intent, request.risk)?;
        let current = self.validate_and_ensure(&context).await?;
        let snapshot = match request.operation {
            BrowserTabsOperation::List => current,
            BrowserTabsOperation::Create => {
                let response = self
                    .context
                    .controller
                    .request_with_context(
                        BrowserCommand::CreateTab { url: request.url },
                        context.clone(),
                    )
                    .await
                    .map_err(ToolFailure::from)?;
                self.apply_workspace_response(response).await?
            }
            BrowserTabsOperation::Select | BrowserTabsOperation::Close => {
                let tab_id = required_nonblank(request.tab_id, "tabId")?;
                let command = match request.operation {
                    BrowserTabsOperation::Select => BrowserCommand::SelectTab { tab_id },
                    BrowserTabsOperation::Close => BrowserCommand::CloseTab { tab_id },
                    _ => unreachable!(),
                };
                let response = self
                    .context
                    .controller
                    .request_with_context(command, context.clone())
                    .await
                    .map_err(ToolFailure::from)?;
                self.apply_workspace_response(response).await?
            }
        };
        Ok(tabs_payload(&snapshot))
    }

    async fn run_navigation(&self, request: BrowserNavigateRequest) -> Result<Value, ToolFailure> {
        let context = invocation_context(&request.intent, request.risk)?;
        let mut snapshot = self.validate_and_ensure(&context).await?;
        let tab_id = request
            .tab_id
            .filter(|value| !value.trim().is_empty())
            .or_else(|| snapshot.selected_tab_id.clone())
            .ok_or_else(|| ToolFailure::invalid_request("no selected browser tab"))?;
        let command = match request.operation {
            BrowserNavigateOperation::Goto => BrowserCommand::Navigate {
                tab_id: tab_id.clone(),
                url: required_nonblank(request.url, "url")?,
            },
            BrowserNavigateOperation::Back => BrowserCommand::Back {
                tab_id: tab_id.clone(),
            },
            BrowserNavigateOperation::Forward => BrowserCommand::Forward {
                tab_id: tab_id.clone(),
            },
            BrowserNavigateOperation::Reload => BrowserCommand::Reload {
                tab_id: tab_id.clone(),
            },
        };
        let response = self
            .context
            .controller
            .request_with_context(command, context.clone())
            .await
            .map_err(ToolFailure::from)?;
        match response {
            workspace @ BrowserResponse::Workspace { .. } => {
                snapshot = self.apply_workspace_response(workspace).await?;
            }
            BrowserResponse::Acknowledged => {
                snapshot = self.refresh_workspace_state(&context).await?;
            }
            _ => {
                return Err(ToolFailure::invalid_response(
                    "browser host returned the wrong navigation response type",
                ));
            }
        }
        let selected = snapshot
            .tabs
            .iter()
            .find(|tab| tab.id == tab_id)
            .ok_or_else(|| ToolFailure::invalid_request("browser tab does not exist"))?;
        Ok(json!({
            "ok": true,
            "loadAcknowledged": true,
            "revision": snapshot.revision,
            "tab": compact_tab(selected),
        }))
    }

    async fn prepare_automation(
        &self,
        intent: &str,
        risk: BrowserMcpRisk,
        tab_id: Option<String>,
    ) -> Result<(BrowserInvocationContext, String), ToolFailure> {
        let context = invocation_context(intent, risk)?;
        let snapshot = self.validate_and_ensure(&context).await?;
        let tab_id = tab_id
            .filter(|value| !value.trim().is_empty())
            .or(snapshot.selected_tab_id)
            .ok_or_else(|| ToolFailure::invalid_request("no selected browser tab"))?;
        if !snapshot.tabs.iter().any(|tab| tab.id == tab_id) {
            return Err(ToolFailure::invalid_request("browser tab does not exist"));
        }
        Ok((context, tab_id))
    }

    async fn send_automation(
        &self,
        context: BrowserInvocationContext,
        command: BrowserCommand,
    ) -> Result<BrowserResponse, ToolFailure> {
        self.context
            .controller
            .request_with_context(command, context)
            .await
            .map_err(ToolFailure::from)
    }

    async fn run_upload(&self, request: BrowserUploadRequest) -> Result<Value, ToolFailure> {
        let mut effective_risk = BrowserRisk::from(request.risk);
        let mut paths = Vec::with_capacity(request.paths.len());
        for path in request.paths {
            let candidate = if path.is_absolute() {
                path
            } else {
                self.context.project_root.join(path)
            };
            let (path, path_risk) = classify_upload_path(&self.context.project_root, candidate)
                .map_err(ToolFailure::from)?;
            effective_risk = effective_browser_risk(effective_risk, None, Some(path_risk));
            paths.push(path);
        }
        let context = BrowserInvocationContext::agent(&request.intent, effective_risk)
            .map_err(ToolFailure::from)?;
        let snapshot = self.validate_and_ensure(&context).await?;
        let tab_id = request
            .tab_id
            .filter(|value| !value.trim().is_empty())
            .or(snapshot.selected_tab_id)
            .ok_or_else(|| ToolFailure::invalid_request("no selected browser tab"))?;
        if !snapshot.tabs.iter().any(|tab| tab.id == tab_id) {
            return Err(ToolFailure::invalid_request("browser tab does not exist"));
        }
        let response = self
            .send_automation(
                context,
                BrowserCommand::Upload {
                    tab_id,
                    target: request.target,
                    paths,
                },
            )
            .await?;
        match response {
            BrowserResponse::Upload { result } => Ok(json!({
                "ok": true,
                "version": 1,
                "uploadedCount": result.files.len(),
                "revision": result.revision,
            })),
            _ => Err(ToolFailure::invalid_response(
                "browser host returned the wrong upload response type",
            )),
        }
    }
}

#[tool_router]
impl BrowserMcpServer {
    #[tool(
        name = "browser_status",
        description = "Report availability and compact state for this conversation's visible DevManager browser pane."
    )]
    async fn browser_status(
        &self,
        Parameters(request): Parameters<BrowserStatusRequest>,
    ) -> CallToolResult {
        let result = async {
            let context = invocation_context(&request.intent, request.risk)?;
            let snapshot = self.validate_and_ensure(&context).await?;
            let response = self
                .context
                .controller
                .request_with_context(BrowserCommand::Status, context)
                .await
                .map_err(ToolFailure::from)?;
            let BrowserResponse::Status { status } = response else {
                return Err(ToolFailure::invalid_response(
                    "browser host returned the wrong status response type",
                ));
            };
            Ok(json!({
                "ok": true,
                "version": 1,
                "host": status,
                "workspace": self.context.controller.workspace_key(),
                "paneOpen": snapshot.pane_open,
                "revision": snapshot.revision,
                "selectedTabId": snapshot.selected_tab_id,
                "pendingWorkCount": self.context.controller.pending_work_count(),
                "diagnostic": status.diagnostic,
            }))
        }
        .await;
        into_tool_result(result)
    }

    #[tool(
        name = "browser_annotations",
        description = "List, inspect, resolve, unresolve, or delete annotations owned by this conversation's DevManager browser pane."
    )]
    async fn browser_annotations(
        &self,
        Parameters(request): Parameters<BrowserAnnotationsRequest>,
    ) -> CallToolResult {
        let result = async {
            let request = request.parsed.map_err(|message| {
                ToolFailure::invalid_request(format!(
                    "malformed browser_annotations request: {message}"
                ))
            })?;
            let context = invocation_context(&request.intent, request.risk)?;
            let annotation_id = match request.operation {
                BrowserAnnotationOperation::List => request
                    .annotation_id
                    .map(|id| required_nonblank(Some(id), "annotationId"))
                    .transpose()?,
                BrowserAnnotationOperation::Get
                | BrowserAnnotationOperation::Resolve
                | BrowserAnnotationOperation::Unresolve
                | BrowserAnnotationOperation::Delete => {
                    Some(required_nonblank(request.annotation_id, "annotationId")?)
                }
            };
            self.validate_and_ensure(&context).await?;
            let response = self
                .context
                .controller
                .request_with_context(
                    BrowserCommand::Annotations {
                        operation: request.operation,
                        annotation_id,
                    },
                    context,
                )
                .await
                .map_err(ToolFailure::from)?;
            match response {
                BrowserResponse::Annotations {
                    annotations,
                    mutation,
                } if request.operation == BrowserAnnotationOperation::List => {
                    *self.context.live_snapshot.lock().await = mutation.snapshot.clone();
                    Ok(json!({
                        "ok": true,
                        "version": 1,
                        "operation": request.operation,
                        "revision": mutation.revision,
                        "annotations": annotations,
                    }))
                }
                BrowserResponse::Annotation { details, mutation }
                    if request.operation == BrowserAnnotationOperation::Get =>
                {
                    *self.context.live_snapshot.lock().await = mutation.snapshot.clone();
                    Ok(json!({
                        "ok": true,
                        "version": 1,
                        "operation": request.operation,
                        "revision": mutation.revision,
                        "annotation": details.annotation,
                        "stale": details.stale,
                        "resources": {
                            "screenshot": details.screenshot,
                            "details": details.details_resource,
                        },
                    }))
                }
                BrowserResponse::AnnotationMutation { result }
                    if result.operation == request.operation =>
                {
                    *self.context.live_snapshot.lock().await = result.mutation.snapshot.clone();
                    Ok(json!({
                        "ok": true,
                        "version": 1,
                        "operation": result.operation,
                        "annotationId": result.annotation_id,
                        "revision": result.mutation.revision,
                        "resolved": result
                            .mutation
                            .snapshot
                            .annotations
                            .iter()
                            .find(|annotation| annotation.id == result.annotation_id)
                            .map(|annotation| annotation.resolved),
                        "resources": {
                            "screenshot": result.screenshot,
                        },
                    }))
                }
                _ => Err(ToolFailure::invalid_response(
                    "browser host returned the wrong annotation response type",
                )),
            }
        }
        .await;
        into_tool_result(result)
    }

    #[tool(
        name = "browser_recording",
        description = "Inspect or explicitly control the workflow recording owned by this conversation's DevManager browser pane."
    )]
    async fn browser_recording(
        &self,
        Parameters(request): Parameters<BrowserRecordingRequest>,
    ) -> CallToolResult {
        let result = async {
            let request = request.parsed.map_err(|message| {
                ToolFailure::invalid_request(format!(
                    "malformed browser_recording request: {message}"
                ))
            })?;
            if request.intent.len() > MAX_BROWSER_MCP_INTENT_BYTES {
                return Err(ToolFailure::invalid_request(
                    "intent must be at most 1024 bytes",
                ));
            }
            let context = invocation_context(&request.intent, request.risk)?;
            let project_root =
                verified_authenticated_local_project_root(&self.context.project_root)
                    .map_err(ToolFailure::from)?;
            self.validate_and_ensure(&context).await?;
            let response = self
                .context
                .controller
                .request_with_local_project_root(
                    BrowserCommand::Recording {
                        operation: request.operation,
                    },
                    context,
                    &project_root,
                )
                .await
                .map_err(ToolFailure::from)?;
            let BrowserResponse::Recording { result } = response else {
                return Err(ToolFailure::invalid_response(
                    "browser host returned the wrong recording response type",
                ));
            };
            if result.operation != request.operation {
                return Err(ToolFailure::invalid_response(
                    "browser host returned the wrong recording operation",
                ));
            }
            Ok(json!({
                "ok": true,
                "version": 1,
                "operation": result.operation,
                "recording": {
                    "status": result.status,
                    "recordingId": result.recording_id,
                    "recipeId": result.recipe_id,
                    "stepCount": result.step_count,
                    "inputs": result.inputs,
                    "valid": result.valid,
                },
                "resource": result.resource,
                "overwroteExisting": result.overwrote_existing,
            }))
        }
        .await;
        into_tool_result(result)
    }

    #[tool(
        name = "browser_tabs",
        description = "List, create, select, or close logical tabs in this conversation's DevManager browser pane."
    )]
    async fn browser_tabs(
        &self,
        Parameters(request): Parameters<BrowserTabsRequest>,
    ) -> CallToolResult {
        into_tool_result(self.run_tabs_operation(request).await)
    }

    #[tool(
        name = "browser_navigate",
        description = "Navigate the selected logical tab with goto, back, forward, or reload."
    )]
    async fn browser_navigate(
        &self,
        Parameters(request): Parameters<BrowserNavigateRequest>,
    ) -> CallToolResult {
        into_tool_result(self.run_navigation(request).await)
    }

    #[tool(
        name = "browser_snapshot",
        description = "Capture a revision-bound semantic page snapshot and return a compact summary plus resource handle."
    )]
    async fn browser_snapshot(
        &self,
        Parameters(request): Parameters<BrowserSnapshotRequest>,
    ) -> CallToolResult {
        let result = async {
            let (context, tab_id) = self
                .prepare_automation(&request.intent, request.risk, request.tab_id)
                .await?;
            let response = self
                .send_automation(context, BrowserCommand::Snapshot { tab_id })
                .await?;
            require_response(response, "snapshot", |response| {
                matches!(response, BrowserResponse::Snapshot { .. })
            })
        }
        .await;
        into_tool_result(result)
    }

    #[tool(
        name = "browser_screenshot",
        description = "Capture a viewport or full-page PNG and return an authenticated resource handle."
    )]
    async fn browser_screenshot(
        &self,
        Parameters(request): Parameters<BrowserScreenshotRequest>,
    ) -> CallToolResult {
        let result = async {
            let (context, tab_id) = self
                .prepare_automation(&request.intent, request.risk, request.tab_id)
                .await?;
            let response = self
                .send_automation(
                    context,
                    BrowserCommand::Screenshot {
                        tab_id,
                        mode: request.mode,
                    },
                )
                .await?;
            require_response(response, "screenshot", |response| {
                matches!(response, BrowserResponse::Screenshot { .. })
            })
        }
        .await;
        into_tool_result(result)
    }

    #[tool(
        name = "browser_wait",
        description = "Wait asynchronously for a typed page condition with a bounded timeout."
    )]
    async fn browser_wait(
        &self,
        Parameters(request): Parameters<BrowserWaitRequest>,
    ) -> CallToolResult {
        let result = async {
            if !(1..=60_000).contains(&request.timeout_ms) {
                return Err(ToolFailure::invalid_request(
                    "timeoutMs must be between 1 and 60000",
                ));
            }
            let (context, tab_id) = self
                .prepare_automation(&request.intent, request.risk, request.tab_id)
                .await?;
            let response = self
                .send_automation(
                    context,
                    BrowserCommand::Wait {
                        tab_id,
                        condition: request.condition,
                        timeout_ms: request.timeout_ms,
                    },
                )
                .await?;
            require_response(response, "wait", |response| {
                matches!(response, BrowserResponse::Wait { .. })
            })
        }
        .await;
        into_tool_result(result)
    }

    #[tool(
        name = "browser_act",
        description = "Run one bounded ordered list of semantic browser actions with runtime risk inspection."
    )]
    async fn browser_act(
        &self,
        Parameters(request): Parameters<BrowserActRequest>,
    ) -> CallToolResult {
        let result = async {
            let (context, tab_id) = self
                .prepare_automation(&request.intent, request.risk, request.tab_id)
                .await?;
            let response = self
                .send_automation(
                    context,
                    BrowserCommand::Act {
                        tab_id,
                        actions: request.actions,
                    },
                )
                .await?;
            require_response(response, "action", |response| {
                matches!(response, BrowserResponse::Action { .. })
            })
        }
        .await;
        into_tool_result(result)
    }

    #[tool(
        name = "browser_console",
        description = "List or clear the bounded redacted console and runtime-error buffer."
    )]
    async fn browser_console(
        &self,
        Parameters(request): Parameters<BrowserConsoleRequest>,
    ) -> CallToolResult {
        let result = async {
            let (context, tab_id) = self
                .prepare_automation(&request.intent, request.risk, request.tab_id)
                .await?;
            let response = self
                .send_automation(
                    context,
                    BrowserCommand::Console {
                        tab_id,
                        operation: request.operation,
                    },
                )
                .await?;
            require_response(response, "console", |response| {
                matches!(response, BrowserResponse::Console { .. })
            })
        }
        .await;
        into_tool_result(result)
    }

    #[tool(
        name = "browser_network",
        description = "List or clear bounded request metadata, or retrieve one explicit captured body."
    )]
    async fn browser_network(
        &self,
        Parameters(mut request): Parameters<BrowserNetworkRequest>,
    ) -> CallToolResult {
        let result = async {
            if request.operation == BrowserNetworkOperation::Body {
                request.request_id = Some(required_nonblank(request.request_id, "requestId")?);
            }
            let (context, tab_id) = self
                .prepare_automation(&request.intent, request.risk, request.tab_id)
                .await?;
            let response = self
                .send_automation(
                    context,
                    BrowserCommand::Network {
                        tab_id,
                        operation: request.operation,
                        request_id: request.request_id,
                    },
                )
                .await?;
            require_response(response, "network", |response| {
                matches!(response, BrowserResponse::Network { .. })
            })
        }
        .await;
        into_tool_result(result)
    }

    #[tool(
        name = "browser_performance",
        description = "Capture bounded performance data or start and stop an in-page trace resource."
    )]
    async fn browser_performance(
        &self,
        Parameters(request): Parameters<BrowserPerformanceRequest>,
    ) -> CallToolResult {
        let result = async {
            let (context, tab_id) = self
                .prepare_automation(&request.intent, request.risk, request.tab_id)
                .await?;
            let response = self
                .send_automation(
                    context,
                    BrowserCommand::Performance {
                        tab_id,
                        operation: request.operation,
                    },
                )
                .await?;
            require_response(response, "performance", |response| {
                matches!(response, BrowserResponse::Performance { .. })
            })
        }
        .await;
        into_tool_result(result)
    }

    #[tool(
        name = "browser_upload",
        description = "Set canonical project files on a semantic file input through WebView2 CDP."
    )]
    async fn browser_upload(
        &self,
        Parameters(request): Parameters<BrowserUploadRequest>,
    ) -> CallToolResult {
        into_tool_result(self.run_upload(request).await)
    }

    #[tool(
        name = "browser_downloads",
        description = "List, reveal, or confirm-delete verified files in this project's browser download directory."
    )]
    async fn browser_downloads(
        &self,
        Parameters(mut request): Parameters<BrowserDownloadsRequest>,
    ) -> CallToolResult {
        let result = async {
            if request.operation != BrowserDownloadOperation::List {
                request.download_id = Some(required_nonblank(request.download_id, "downloadId")?);
            }
            let (context, tab_id) = self
                .prepare_automation(&request.intent, request.risk, request.tab_id)
                .await?;
            let response = self
                .send_automation(
                    context,
                    BrowserCommand::Downloads {
                        tab_id,
                        operation: request.operation,
                        download_id: request.download_id,
                    },
                )
                .await?;
            require_response(response, "downloads", |response| {
                matches!(response, BrowserResponse::Downloads { .. })
            })
        }
        .await;
        into_tool_result(result)
    }

    #[tool(
        name = "browser_cdp",
        description = "Call an enabled raw WebView2 DevTools Protocol method without opening a debugging port."
    )]
    async fn browser_cdp(
        &self,
        Parameters(request): Parameters<BrowserCdpRequest>,
    ) -> CallToolResult {
        let result = async {
            if request.method.trim().is_empty() || !request.params.is_object() {
                return Err(ToolFailure::invalid_request(
                    "method is required and params must be an object",
                ));
            }
            let (context, tab_id) = self
                .prepare_automation(&request.intent, request.risk, request.tab_id)
                .await?;
            let response = self
                .send_automation(
                    context,
                    BrowserCommand::Cdp {
                        tab_id,
                        method: request.method,
                        params: request.params,
                    },
                )
                .await?;
            require_response(response, "CDP", |response| {
                matches!(response, BrowserResponse::Cdp { .. })
            })
        }
        .await;
        into_tool_result(result)
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for BrowserMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
        )
            .with_server_info(
                Implementation::new("devmanager-browser", "v1")
                    .with_title("devmanager-browser"),
            )
            .with_instructions(
                "Tools operate only the caller's visible per-conversation companion pane. Semantic references are revision-bound and large results are returned as resources.",
            )
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        let lease_ticket = self
            .context
            .controller
            .capture_registration_lease_ticket()
            .map_err(|_| ErrorData::resource_not_found("resource store unavailable", None))?;
        let owner = self.context.controller.workspace_key();
        let resources = self
            .context
            .resource_store
            .list(owner)
            .map_err(|_| ErrorData::resource_not_found("resource store unavailable", None))?
            .into_iter()
            .map(|handle| {
                Resource::new(handle.uri, format!("browser-{:?}", handle.kind))
                    .with_mime_type(handle.mime_type)
                    .with_size(handle.byte_size)
            })
            .collect();
        if !self
            .context
            .controller
            .registration_lease_is_current(lease_ticket)
        {
            return Err(ErrorData::resource_not_found(
                "resource store unavailable",
                None,
            ));
        }
        Ok(ListResourcesResult::with_all_items(resources))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, ErrorData> {
        let lease_ticket = self
            .context
            .controller
            .capture_registration_lease_ticket()
            .map_err(|_| ErrorData::resource_not_found("resource not found", None))?;
        let id = resource_id_from_uri(&request.uri)
            .map_err(|_| ErrorData::resource_not_found("resource not found", None))?;
        let resource = self
            .context
            .resource_store
            .read(self.context.controller.workspace_key(), &id)
            .map_err(|_| ErrorData::resource_not_found("resource not found", None))?;
        let contents = if is_text_resource(&resource.metadata.mime_type) {
            let text = String::from_utf8(resource.bytes)
                .map_err(|_| ErrorData::resource_not_found("resource not found", None))?;
            ResourceContents::text(text, request.uri).with_mime_type(resource.metadata.mime_type)
        } else {
            let blob = base64::engine::general_purpose::STANDARD.encode(resource.bytes);
            ResourceContents::blob(blob, request.uri).with_mime_type(resource.metadata.mime_type)
        };
        if !self
            .context
            .controller
            .registration_lease_is_current(lease_ticket)
        {
            return Err(ErrorData::resource_not_found("resource not found", None));
        }
        Ok(ReadResourceResult::new(vec![contents]))
    }
}

fn is_text_resource(mime_type: &str) -> bool {
    mime_type.starts_with("text/")
        || mime_type == "application/json"
        || mime_type.ends_with("+json")
        || mime_type == "application/javascript"
}

#[derive(Debug)]
struct ToolFailure {
    code: &'static str,
    message: String,
}

impl ToolFailure {
    fn invalid_request(message: impl Into<String>) -> Self {
        Self {
            code: "invalid_request",
            message: message.into(),
        }
    }

    fn invalid_response(message: impl Into<String>) -> Self {
        Self {
            code: "crashed_view",
            message: message.into(),
        }
    }
}

impl From<BrowserError> for ToolFailure {
    fn from(error: BrowserError) -> Self {
        let code = match &error {
            BrowserError::InvalidWorkspaceKey { .. } => "invalid_workspace_key",
            BrowserError::InvalidInvocation { .. } => "invalid_request",
            BrowserError::InvalidAnnotation { .. } => "invalid_annotation",
            BrowserError::MissingAnnotation { .. } => "missing_annotation",
            BrowserError::StaleReference { .. } => "stale_reference",
            BrowserError::MissingFile { .. } => "missing_file",
            BrowserError::MissingResource { .. } => "missing_resource",
            BrowserError::ResourceTooLarge { .. } => "resource_too_large",
            BrowserError::ResourceRootBusy => "resource_root_busy",
            BrowserError::ResourceRootUnavailable => "resource_root_unavailable",
            BrowserError::OutsideWorkspace { .. } => "outside_workspace_file",
            BrowserError::InvalidRecipe { .. } | BrowserError::UnsupportedRecipeVersion { .. } => {
                "invalid_recipe"
            }
            BrowserError::RecordingResourceUnavailable => "recording_resource_unavailable",
            BrowserError::Interrupted => "user_interrupted",
            BrowserError::Timeout { .. } => "timeout",
            BrowserError::NavigationFailure { .. } => "navigation_failure",
            BrowserError::CrashedView { .. } => "crashed_view",
            BrowserError::LocatorNotFound { .. } => "locator_not_found",
            BrowserError::BlockedPermission { .. } => "blocked_permission",
            BrowserError::UnavailablePlatform { .. } => "unavailable_platform",
            BrowserError::Io { .. } => "io_error",
        };
        Self {
            code,
            message: error.to_string(),
        }
    }
}

fn into_tool_result(result: Result<Value, ToolFailure>) -> CallToolResult {
    match result {
        Ok(value) => CallToolResult::structured(value),
        Err(error) => CallToolResult::structured_error(json!({
            "ok": false,
            "error": {
                "code": error.code,
                "message": error.message,
            }
        })),
    }
}

fn require_response(
    response: BrowserResponse,
    operation: &str,
    expected: impl FnOnce(&BrowserResponse) -> bool,
) -> Result<Value, ToolFailure> {
    if !expected(&response) {
        return Err(ToolFailure::invalid_response(format!(
            "browser host returned the wrong {operation} response type"
        )));
    }
    Ok(json!({
        "ok": true,
        "version": 1,
        "result": response,
    }))
}

fn required_nonblank(value: Option<String>, field: &str) -> Result<String, ToolFailure> {
    value
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| ToolFailure::invalid_request(format!("{field} is required")))
}

fn invocation_context(
    intent: &str,
    risk: BrowserMcpRisk,
) -> Result<BrowserInvocationContext, ToolFailure> {
    BrowserInvocationContext::agent(intent, BrowserRisk::from(risk)).map_err(ToolFailure::from)
}

fn compact_tab(tab: &BrowserTabSnapshot) -> Value {
    json!({
        "id": tab.id,
        "title": tab.title,
        "url": tab.url,
        "viewport": tab.viewport,
    })
}

fn tabs_payload(snapshot: &BrowserWorkspaceSnapshot) -> Value {
    json!({
        "ok": true,
        "revision": snapshot.revision,
        "selectedTabId": snapshot.selected_tab_id,
        "tabs": snapshot.tabs.iter().map(compact_tab).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser::{
        browser_command_channel, BrowserHostState, BrowserRecordingResult, BrowserRecordingStatus,
        BrowserResourceLimits, BrowserWorkspaceKey,
    };
    use std::sync::{Arc, Mutex as StdMutex};
    use std::time::Duration;

    #[tokio::test]
    async fn recording_rejects_unc_root_before_all_six_operations_or_lifecycle_effects() {
        let (bridge, mut inbox) = browser_command_channel(16);
        let owner =
            BrowserWorkspaceKey::new("root-fence-project", "root-fence-conversation").unwrap();
        let controller = bridge.bind(owner, Duration::from_secs(1));
        drop(bridge);
        let resource_root = std::env::temp_dir().join(format!(
            "devmanager-recording-root-fence-test-{}",
            std::process::id()
        ));
        let resources =
            BrowserResourceStore::open(&resource_root, BrowserResourceLimits::default()).unwrap();
        let remote_root = PathBuf::from(r"\\recording-root-path-sentinel\share\project");
        let server = BrowserMcpServer::new(
            controller,
            BrowserWorkspaceSnapshot::default(),
            resources,
            remote_root.clone(),
        );
        let observed = Arc::new(StdMutex::new(Vec::new()));
        let host_observed = Arc::clone(&observed);

        let host = async move {
            let mut state = BrowserHostState::new(PathBuf::from("root-fence-fake-host"));
            while let Some(request) = inbox.recv().await {
                let key = request.workspace_key().clone();
                let command = request.command().clone();
                host_observed.lock().unwrap().push(command.clone());
                let response = match command {
                    BrowserCommand::Ensure { snapshot } => state
                        .ensure_workspace(key, snapshot)
                        .map(|mutation| BrowserResponse::Workspace { mutation }),
                    BrowserCommand::SetPaneOpen { open } => state
                        .set_pane_open(&key, open)
                        .map(|mutation| BrowserResponse::Workspace { mutation }),
                    BrowserCommand::WorkspaceState => Ok(BrowserResponse::WorkspaceState {
                        snapshot: state.workspace(&key).unwrap().clone(),
                    }),
                    BrowserCommand::Recording { operation } => Ok(BrowserResponse::Recording {
                        result: BrowserRecordingResult {
                            operation,
                            status: BrowserRecordingStatus::Inactive,
                            recording_id: None,
                            recipe_id: None,
                            step_count: 0,
                            inputs: Vec::new(),
                            valid: false,
                            resource: None,
                            overwrote_existing: None,
                        },
                    }),
                    other => panic!("unexpected root-fence command: {other:?}"),
                };
                request.respond(response);
            }
        };
        let scenario = async move {
            for operation in [
                BrowserRecordingOperation::Status,
                BrowserRecordingOperation::Start,
                BrowserRecordingOperation::Stop,
                BrowserRecordingOperation::Review,
                BrowserRecordingOperation::Discard,
                BrowserRecordingOperation::Save,
            ] {
                let response = server
                    .browser_recording(Parameters(BrowserRecordingRequest {
                        parsed: Ok(BrowserRecordingRequestWire {
                            intent: format!("test {operation:?} root fence"),
                            risk: BrowserMcpRisk::Normal,
                            operation,
                        }),
                    }))
                    .await;
                assert_eq!(
                    response.is_error,
                    Some(true),
                    "{operation:?} was not rejected"
                );
                let body = response.structured_content.expect("typed root-fence error");
                assert_eq!(body["error"]["code"], "invalid_request");
                let encoded = serde_json::to_string(&body).unwrap();
                assert!(!encoded.contains("recording-root-path-sentinel"));
                assert!(!encoded.contains(remote_root.to_string_lossy().as_ref()));
            }
            drop(server);
        };

        tokio::join!(host, scenario);
        assert!(
            observed.lock().unwrap().is_empty(),
            "root rejection must occur before ensure, status, or recording commands"
        );
        std::fs::remove_dir_all(resource_root).unwrap();
    }
}
