mod automation;
mod commands;
mod gateway;
mod host;
mod mcp;
mod model;
mod operation_queue;
mod pane;
mod policy;
mod provider;
mod recipes;
mod resources;
mod storage;

pub use automation::{
    build_semantic_snapshot, effective_browser_risk, runtime_target_risk, BrowserAction,
    BrowserActionResult, BrowserActionTarget, BrowserConsoleEntry, BrowserConsoleOperation,
    BrowserDownloadEntry, BrowserDownloadOperation, BrowserLocatorStrategy, BrowserNetworkEntry,
    BrowserNetworkOperation, BrowserPerformanceOperation, BrowserPerformanceSnapshot, BrowserPoint,
    BrowserRawSemanticElement, BrowserRedactedAction, BrowserRuntimeTarget, BrowserScreenshotMode,
    BrowserSemanticElement, BrowserSemanticSnapshot, BrowserSnapshotSummary,
    BrowserTelemetryBuffer, BrowserUploadResult, BrowserWaitCondition, BrowserWaitResult,
    MAX_BROWSER_ACTIONS, MAX_BROWSER_JOURNAL_ENTRIES, REDACTED_VALUE,
};
pub use commands::{
    browser_command_channel, BrowserCommand, BrowserCommandBridge, BrowserCommandInbox,
    BrowserCommandRequest, BrowserController, BrowserDiagnosticLevel, BrowserDownloadState,
    BrowserHostEvent, BrowserHostStatus, BrowserInvocationActor, BrowserInvocationContext,
    BrowserPageLoadState, BrowserResponse, BrowserUserInputKind,
};
pub use gateway::{BrowserGatewayHandle, BrowserGatewayRegistrar, BrowserGatewayRegistration};
pub use host::{
    browser_user_input_initialization_script, unique_download_path, unsupported_host_status,
    unsupported_platform_error, validate_browser_url, BrowserHostState, BrowserMemoryTarget,
    BrowserProfileClearPlan, BrowserProjectContextKey, BrowserViewCreationPlan,
    BrowserViewVisibilityPlan, BrowserWebViewHost, BrowserWorkspaceMutation,
};
pub use model::{
    BrowserAnnotation, BrowserBounds, BrowserElementRef, BrowserError, BrowserJournalActor,
    BrowserJournalEntry, BrowserLocator, BrowserResourceId, BrowserRevision, BrowserTabSnapshot,
    BrowserViewport, BrowserWorkspaceKey, BrowserWorkspaceSnapshot,
};
pub use operation_queue::{
    BrowserOperationQueue, BrowserOperationTarget, BrowserQueueCancellation,
};
pub use pane::{
    browser_action_plan, browser_content_bounds, browser_event_plan, browser_host_reconcile_plan,
    browser_host_visibility, browser_pane_eligible, browser_pane_open_fallback,
    browser_response_sync, browser_settings_plan, calculate_browser_split,
    normalize_browser_address, render_browser_pane, selected_browser_tab_id, BrowserActionPlan,
    BrowserHostReconcilePlan, BrowserHostVisibility, BrowserPaneAction, BrowserPaneActions,
    BrowserPaneContext, BrowserPaneEventPlan, BrowserPaneModel, BrowserPaneSurface,
    BrowserPaneTransient, BrowserSettingsAction, BrowserSettingsPlan, BrowserSnapshotSync,
    BrowserSplitLayout, BrowserViewportPreset,
};
pub use policy::{classify_upload_path, BrowserApprovalPolicy, BrowserRisk};
pub use provider::{
    codex_browser_config_overrides, prepare_claude_browser_overlay, BrowserProviderAccess,
    ClaudeBrowserOverlay, DEVMANAGER_BROWSER_TOKEN_ENV,
};
pub use recipes::{
    load_recipe, recipe_path, save_recipe, BrowserRecipeAction, BrowserRecipeInput,
    BrowserRecipeInputKind, BrowserRecipeStep, BrowserRecipeV1, BROWSER_RECIPE_SCHEMA_VERSION,
};
pub use resources::{
    resource_id_from_uri, resource_uri, BrowserResource, BrowserResourceHandle,
    BrowserResourceKind, BrowserResourceLimits, BrowserResourceMetadata, BrowserResourceStore,
};
pub use storage::BrowserStorageLayout;
