use super::{
    resource_id_from_uri, BrowserCommand, BrowserController, BrowserError,
    BrowserInvocationContext, BrowserResourceStore, BrowserResponse, BrowserRisk,
    BrowserTabSnapshot, BrowserWorkspaceSnapshot,
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
use std::sync::Arc;
use tokio::sync::Mutex;

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

struct BrowserMcpContext {
    controller: BrowserController,
    initial_snapshot: BrowserWorkspaceSnapshot,
    live_snapshot: Mutex<BrowserWorkspaceSnapshot>,
    first_use: Mutex<bool>,
    resource_store: BrowserResourceStore,
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
    ) -> Self {
        Self {
            context: Arc::new(BrowserMcpContext {
                controller,
                live_snapshot: Mutex::new(initial_snapshot.clone()),
                initial_snapshot,
                first_use: Mutex::new(false),
                resource_store,
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
        Ok(ListResourcesResult::with_all_items(resources))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, ErrorData> {
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
