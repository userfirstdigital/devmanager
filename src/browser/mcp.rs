use super::{
    BrowserCommand, BrowserController, BrowserError, BrowserResponse, BrowserRisk,
    BrowserTabSnapshot, BrowserWorkspaceSnapshot,
};
use rmcp::handler::server::{router::tool::ToolRouter, wrapper::Parameters};
use rmcp::model::{CallToolResult, Implementation, ServerCapabilities, ServerInfo};
use rmcp::schemars;
use rmcp::{tool, tool_handler, tool_router, ServerHandler};
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
    ) -> Self {
        Self {
            context: Arc::new(BrowserMcpContext {
                controller,
                live_snapshot: Mutex::new(initial_snapshot.clone()),
                initial_snapshot,
                first_use: Mutex::new(false),
            }),
            tool_router: Self::tool_router(),
        }
    }

    async fn validate_and_ensure(
        &self,
        intent: &str,
        risk: BrowserMcpRisk,
    ) -> Result<BrowserWorkspaceSnapshot, ToolFailure> {
        if intent.trim().is_empty() {
            return Err(ToolFailure::invalid_request("intent cannot be blank"));
        }
        let _declared_risk: BrowserRisk = risk.into();
        let mut first_use = self.context.first_use.lock().await;
        if !*first_use {
            let ensured = self
                .context
                .controller
                .request(BrowserCommand::Ensure {
                    snapshot: self.context.initial_snapshot.clone(),
                })
                .await
                .map_err(ToolFailure::from)?;
            self.apply_workspace_response(ensured).await?;
            let opened = self
                .context
                .controller
                .request(BrowserCommand::SetPaneOpen { open: true })
                .await
                .map_err(ToolFailure::from)?;
            self.apply_workspace_response(opened).await?;
            *first_use = true;
        }
        Ok(self.context.live_snapshot.lock().await.clone())
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

    async fn apply_tab_list(
        &self,
        response: BrowserResponse,
    ) -> Result<BrowserWorkspaceSnapshot, ToolFailure> {
        let BrowserResponse::Tabs {
            tabs,
            selected_tab_id,
        } = response
        else {
            return Err(ToolFailure::invalid_response(
                "browser host returned the wrong tab response type",
            ));
        };
        let mut snapshot = self.context.live_snapshot.lock().await;
        snapshot.tabs = tabs;
        snapshot.selected_tab_id = selected_tab_id;
        Ok(snapshot.clone())
    }

    async fn request_tabs(&self) -> Result<BrowserWorkspaceSnapshot, ToolFailure> {
        let response = self
            .context
            .controller
            .request(BrowserCommand::ListTabs)
            .await
            .map_err(ToolFailure::from)?;
        self.apply_tab_list(response).await
    }

    async fn run_tabs_operation(&self, request: BrowserTabsRequest) -> Result<Value, ToolFailure> {
        self.validate_and_ensure(&request.intent, request.risk)
            .await?;
        let snapshot = match request.operation {
            BrowserTabsOperation::List => self.request_tabs().await?,
            BrowserTabsOperation::Create => {
                let response = self
                    .context
                    .controller
                    .request(BrowserCommand::CreateTab { url: request.url })
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
                    .request(command)
                    .await
                    .map_err(ToolFailure::from)?;
                self.apply_workspace_response(response).await?
            }
        };
        Ok(tabs_payload(&snapshot))
    }

    async fn run_navigation(&self, request: BrowserNavigateRequest) -> Result<Value, ToolFailure> {
        let mut snapshot = self
            .validate_and_ensure(&request.intent, request.risk)
            .await?;
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
            .request(command)
            .await
            .map_err(ToolFailure::from)?;
        if matches!(response, BrowserResponse::Workspace { .. }) {
            snapshot = self.apply_workspace_response(response).await?;
        } else if !matches!(response, BrowserResponse::Acknowledged) {
            return Err(ToolFailure::invalid_response(
                "browser host returned the wrong navigation response type",
            ));
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
            let snapshot = self
                .validate_and_ensure(&request.intent, request.risk)
                .await?;
            let response = self
                .context
                .controller
                .request(BrowserCommand::Status)
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
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(
                Implementation::new("devmanager-browser", "v1")
                    .with_title("devmanager-browser"),
            )
            .with_instructions(
                "Tools operate only the caller's visible per-conversation companion pane. Semantic references are revision-bound and large results are returned as resources.",
            )
    }
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
            BrowserError::StaleReference { .. } => "stale_reference",
            BrowserError::MissingFile { .. } => "missing_file",
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
