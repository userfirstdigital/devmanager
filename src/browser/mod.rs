mod annotations;
mod attachments;
mod automation;
mod commands;
mod downloads;
mod gateway;
mod host;
mod mcp;
mod model;
mod operation_queue;
mod pane;
mod policy;
mod provider;
mod recipes;
mod recording;
mod recording_coordinator;
mod recording_ipc;
mod recording_mcp;
mod replay;
mod replay_executor;
mod resources;
mod storage;

pub(crate) use annotations::redacted_browser_annotation;
pub use annotations::{
    crop_annotation_png, effective_browser_annotation_risk, parse_browser_annotation_ipc_message,
    parse_browser_page_ipc_message, validate_annotation_candidate_context,
    BrowserAnnotationCandidate, BrowserAnnotationCleanupLedger, BrowserAnnotationDetails,
    BrowserAnnotationDraft, BrowserAnnotationLifecycle, BrowserAnnotationOperation,
    BrowserAnnotationResourceCleanup, BrowserAnnotationRoute, BrowserAnnotationSummary,
    BrowserPageIpcMessage, MAX_ANNOTATION_IPC_BYTES,
};
pub use attachments::{
    browser_input_opens_prompt_boundary, BrowserAttachmentBroker, BrowserAttachmentError,
    BrowserAttachmentProjection, BrowserAttachmentReservation, BrowserAttachmentSessionBinding,
    BrowserPromptInput, MAX_BROWSER_ATTACHMENT_PREAMBLE_BYTES,
};
pub(crate) use attachments::{compact_browser_attachment_text, compact_browser_attachment_url};
pub use automation::{
    browser_cdp_method_risk, build_semantic_snapshot, effective_browser_risk,
    effective_browser_risk_for_targets, redact_browser_resource_bytes, redact_browser_text,
    runtime_target_risk, BrowserAction, BrowserActionResult, BrowserActionTarget,
    BrowserConsoleEntry, BrowserConsoleOperation, BrowserDownloadEntry, BrowserDownloadOperation,
    BrowserLocatorStrategy, BrowserNetworkEntry, BrowserNetworkOperation,
    BrowserPerformanceOperation, BrowserPerformanceSnapshot, BrowserPoint,
    BrowserRawSemanticElement, BrowserRedactedAction, BrowserRuntimeTarget, BrowserScreenshotMode,
    BrowserSemanticElement, BrowserSemanticSnapshot, BrowserSnapshotSummary,
    BrowserTelemetryBuffer, BrowserUploadResult, BrowserWaitCondition, BrowserWaitResult,
    MAX_BROWSER_ACTIONS, MAX_BROWSER_JOURNAL_ENTRIES, REDACTED_VALUE,
};
pub use commands::{
    browser_command_channel, browser_lifecycle_control, browser_operation_target_tab_id,
    browser_request_preempts_operation_queue, browser_response_resource_ids, route_browser_request,
    BrowserApprovalRequest, BrowserCommand, BrowserCommandBridge, BrowserCommandInbox,
    BrowserCommandRequest, BrowserController, BrowserDiagnosticLevel, BrowserDownloadState,
    BrowserHostControl, BrowserHostEvent, BrowserHostStatus, BrowserInvocationActor,
    BrowserInvocationContext, BrowserPageLoadState, BrowserRecordingInputSummary,
    BrowserRecordingOperation, BrowserRecordingResult, BrowserResponse, BrowserUserInputKind,
};
pub(crate) use commands::{verified_authenticated_local_project_root, BrowserRegistrationLease};
pub use downloads::{
    prepare_verified_download_root, prepare_verified_profile_root, remove_verified_profile,
    BrowserDownloadStore,
};
pub use gateway::{BrowserGatewayHandle, BrowserGatewayRegistrar, BrowserGatewayRegistration};
pub use host::{
    acknowledge_attachment_projection_and_reconcile_pins, browser_user_input_initialization_script,
    unique_download_path, unsupported_command_response, unsupported_host_status,
    unsupported_platform_error, validate_browser_url, BrowserAnnotationMutationResult,
    BrowserHostState, BrowserMemoryTarget, BrowserProfileClearPlan, BrowserProjectContextKey,
    BrowserViewCreationPlan, BrowserViewVisibilityPlan, BrowserWebViewHost,
    BrowserWorkspaceMutation,
};
pub use model::{
    BrowserAnnotation, BrowserAnnotationKind, BrowserAttachmentRevision, BrowserBounds,
    BrowserElementRef, BrowserError, BrowserJournalActor, BrowserJournalEntry, BrowserLocator,
    BrowserResourceId, BrowserRevision, BrowserTabSnapshot, BrowserViewport, BrowserWorkspaceKey,
    BrowserWorkspaceSnapshot,
};
pub use operation_queue::{
    BrowserOperationQueue, BrowserOperationTarget, BrowserQueueCancellation,
};
pub use pane::{
    apply_browser_workflow_review_mutation, browser_action_plan, browser_annotation_preview_plan,
    browser_content_bounds, browser_event_plan, browser_host_reconcile_plan,
    browser_host_visibility, browser_pane_eligible, browser_pane_open_fallback,
    browser_response_sync, browser_settings_plan, browser_workflow_review_editor_for_field,
    browser_workflow_review_editor_mutation, browser_workflow_review_projection,
    calculate_browser_split, discard_browser_workflow_review, normalize_browser_address,
    preview_browser_workflow_review, render_browser_pane, save_browser_workflow_review,
    selected_browser_tab_id, BrowserActionPlan, BrowserHostReconcilePlan, BrowserHostVisibility,
    BrowserPaneAction, BrowserPaneActions, BrowserPaneContext, BrowserPaneEventPlan,
    BrowserPaneModel, BrowserPaneSurface, BrowserPaneTransient, BrowserSettingsAction,
    BrowserSettingsPlan, BrowserSnapshotSync, BrowserSplitLayout, BrowserViewportPreset,
    BrowserWorkflowReviewAssertionKind, BrowserWorkflowReviewEditor,
    BrowserWorkflowReviewEditorField, BrowserWorkflowReviewInputProjection,
    BrowserWorkflowReviewMetadataProjection, BrowserWorkflowReviewMutation,
    BrowserWorkflowReviewProjection, BrowserWorkflowReviewStepProjection,
    BrowserWorkflowReviewUiState,
};
pub use policy::{classify_upload_path, BrowserApprovalPolicy, BrowserRisk};
pub use provider::{
    codex_browser_config_overrides, prepare_claude_browser_overlay, BrowserProviderAccess,
    ClaudeBrowserOverlay, DEVMANAGER_BROWSER_TOKEN_ENV,
};
pub use recipes::{
    list_recipes, load_recipe, recipe_path, save_recipe, BrowserRecipeAction,
    BrowserRecipeAssertion, BrowserRecipeElementState, BrowserRecipeInput, BrowserRecipeInputKind,
    BrowserRecipeLocator, BrowserRecipeStep, BrowserRecipeV1, BrowserRecipeValue,
    BrowserRecipeViewport, BrowserRecipeWait, BROWSER_RECIPE_SCHEMA_VERSION,
    MAX_BROWSER_RECIPE_WAIT_MS,
};
pub use recording::{
    BrowserRecordingAction, BrowserRecordingActor, BrowserRecordingCommit, BrowserRecordingError,
    BrowserRecordingInstance, BrowserRecordingMetadata, BrowserRecordingReservation,
    BrowserRecordingReview, BrowserRecordingStatus, BrowserWorkflowRecorder,
    MAX_BROWSER_RECORDING_ASSERTIONS, MAX_BROWSER_RECORDING_ASSERTIONS_PER_ACTION,
    MAX_BROWSER_RECORDING_INPUTS,
};
pub use recording_coordinator::{BrowserUserChromeCapture, BrowserWorkflowCoordinator};
pub(crate) use recording_ipc::{
    browser_page_origin_from_url, BrowserPageRecordingIngress, BrowserPageRecordingSubmit,
    BrowserPageRecordingTransport, BrowserPageRecordingTransportFailureKind,
};
pub use recording_ipc::{
    canonical_browser_page_origin, BrowserPageRecordingAuthority, BrowserPageRecordingEnvelope,
    BrowserPageRecordingEvent, BrowserPageRecordingIpc, BrowserPageRecordingIpcError,
    BrowserPageRecordingTextEdit, MAX_BROWSER_PAGE_RECORDING_IPC_BYTES,
    MAX_BROWSER_PAGE_RECORDING_IPC_DEPTH, MAX_BROWSER_PAGE_RECORDING_IPC_STRINGS,
    MAX_BROWSER_PAGE_RECORDING_LOCATOR_FALLBACKS, MAX_BROWSER_PAGE_RECORDING_SELECT_VALUES,
    MAX_BROWSER_PAGE_RECORDING_STRING_BYTES,
};
pub(crate) use recording_mcp::recording_resource_unavailable;
pub use recording_mcp::{
    browser_recording_review_result, browser_recording_save_would_overwrite,
    browser_recording_status_result, discard_browser_recording, effective_browser_recording_risk,
    save_browser_recording_review,
};
pub use replay::{
    compile_browser_replay, BrowserReplayCancellationLease, BrowserReplayCoordinator,
    BrowserReplayError, BrowserReplayExecutionHandle, BrowserReplayFailureCode,
    BrowserReplayInstance, BrowserReplayPlan, BrowserReplayProjection, BrowserReplayPublicInput,
    BrowserReplayStart, BrowserReplayStatus, MAX_BROWSER_REPLAY_FILE_BYTES,
    MAX_BROWSER_REPLAY_INPUTS, MAX_BROWSER_REPLAY_INPUT_NAME_BYTES, MAX_BROWSER_REPLAY_STEPS,
    MAX_BROWSER_REPLAY_TEXT_BYTES, MAX_BROWSER_REPLAY_URL_BYTES,
};
pub use replay_executor::execute_browser_replay;
pub use resources::{
    resource_id_from_uri, resource_uri, BrowserResource, BrowserResourceHandle,
    BrowserResourceKind, BrowserResourceLimits, BrowserResourceMetadata, BrowserResourceStore,
};
pub use storage::BrowserStorageLayout;
