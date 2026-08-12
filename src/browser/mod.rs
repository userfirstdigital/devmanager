mod annotations;
mod attachments;
mod automation;
mod commands;
mod conformance;
pub mod domain;
mod downloads;
mod gateway;
mod generation;
mod host;
mod mcp;
mod model;
mod native_shell_controller;
mod operation_queue;
mod pane;
mod policy;
mod projection;
pub mod protocol;
mod provider;
mod recipes;
mod recording;
mod recording_coordinator;
mod recording_ipc;
mod recording_mcp;
mod replay;
mod replay_executor;
mod service;
mod surface;
// Task 2's domain slice will consume this private authority seam and remove the allowance.
#[cfg_attr(not(test), allow(dead_code))]
mod replay_repair;
mod replay_secrets;
mod resources;
mod storage;
mod teardown;
mod workflow_mcp;

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
    effective_browser_risk_for_targets, effective_browser_secret_type_risk,
    redact_browser_resource_bytes, redact_browser_text, runtime_target_risk, BrowserAction,
    BrowserActionResult, BrowserActionTarget, BrowserConsoleEntry, BrowserConsoleOperation,
    BrowserDownloadEntry, BrowserDownloadOperation, BrowserLocatorStrategy, BrowserNetworkEntry,
    BrowserNetworkOperation, BrowserPerformanceOperation, BrowserPerformanceSnapshot, BrowserPoint,
    BrowserRawSemanticElement, BrowserRedactedAction, BrowserReplayRepairCandidate,
    BrowserRuntimeTarget, BrowserScreenshotMode, BrowserSemanticElement, BrowserSemanticSnapshot,
    BrowserSnapshotSummary, BrowserTelemetryBuffer, BrowserUploadResult, BrowserWaitCondition,
    BrowserWaitResult, MAX_BROWSER_ACTIONS, MAX_BROWSER_JOURNAL_ENTRIES, REDACTED_VALUE,
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
pub(crate) use commands::{
    validate_direct_repair_preview_command, validate_direct_secret_command,
    verified_authenticated_local_project_root, BrowserRegistrationLease, BrowserReplayAdmission,
    BrowserReplayRepairCleanupWork,
};
pub use conformance::{
    browser_fixture_root, classify_visible_host_proof, hold_authenticated_provider_launch,
    real_provider_launch_is_forbidden, validate_browser_fixture_site, BrowserFixtureAction,
    BrowserFixtureCase, BrowserFixtureRecoveryCase, BrowserFixtureValidation,
    BrowserFixtureValidationError, BrowserProviderArm, BrowserProviderE2EHold,
    BrowserProviderHoldRecord, BrowserVisibleHostProofClaim, BrowserVisibleHostProofClass,
    BROWSER_E2E_SCHEMA_VERSION, BROWSER_E2E_VERIFICATION_TOKEN, BROWSER_FIXTURE_CASES,
    BROWSER_VISIBLE_WEBVIEW2_OPT_IN_ENV,
};
pub use native_shell_controller::{
    BrowserGatewayBindingRef, BrowserNativeCallback, BrowserNativeCallbackKind,
    BrowserNativeControllerError, BrowserNativeDestination, BrowserNativeHostCommand,
    BrowserNativeHostOutcome, BrowserNativeIdentity, BrowserNativeLease,
    BrowserNativeShellController,
};
pub use downloads::{
    prepare_verified_download_root, prepare_verified_profile_root, remove_verified_profile,
    BrowserDownloadStore, BrowserIoController, BrowserIoError, BrowserSecretFillReport,
    BrowserStagedDownload,
};
pub use gateway::{BrowserGatewayHandle, BrowserGatewayRegistrar, BrowserGatewayRegistration};
pub use generation::{
    BrowserGenerationError, BrowserGenerationTicket, BrowserTaskArtifact, BrowserTaskArtifactKind,
    BrowserTaskGenerationAuthority, BrowserWorkflowKind, MAX_BROWSER_GENERATION_CONTEXTS,
    MAX_BROWSER_GENERATION_QUEUE,
};
pub(crate) use host::BrowserAppExitDisposition;
pub use host::{
    acknowledge_attachment_projection_and_reconcile_pins, browser_user_input_initialization_script,
    legacy_mcp_command_task_identity, require_completed_wry_task_identity, unique_download_path,
    unsupported_command_response, unsupported_host_status, unsupported_platform_error,
    validate_browser_url, BrowserAnnotationMutationResult, BrowserHostOwnedSurfaceProof,
    BrowserHostState, BrowserMemoryTarget, BrowserNativeSurfaceBackend, BrowserNativeViewError,
    BrowserNativeViewReceipt, BrowserNativeViewRegistration, BrowserProfileClearPlan,
    BrowserProjectContextKey, BrowserTaskSurfaceBindBlocker, BrowserTeardownObserver,
    BrowserViewCreationPlan, BrowserViewVisibilityPlan, BrowserWebViewHost,
    BrowserWorkspaceMutation, HostOwnedNativeSurfaceBackend, HostOwnedSurfaceBindError,
    LegacyMcpTaskSurfaceBlocker,
};
pub use model::{
    BrowserAnnotation, BrowserAnnotationKind, BrowserAttachmentRevision, BrowserBounds,
    BrowserElementRef, BrowserError, BrowserJournalActor, BrowserJournalEntry, BrowserLocator,
    BrowserLocatorFailureTarget, BrowserResourceId, BrowserRevision, BrowserTabSnapshot,
    BrowserViewport, BrowserWorkspaceKey, BrowserWorkspaceSnapshot,
};
pub use operation_queue::{
    BrowserOperationQueue, BrowserOperationTarget, BrowserQueueCancellation,
};
pub use pane::{
    apply_browser_workflow_review_mutation, browser_action_plan, browser_annotation_preview_plan,
    browser_content_bounds, browser_event_plan, browser_host_reconcile_plan,
    browser_host_visibility, browser_pane_eligible, browser_pane_open_fallback,
    browser_replay_repair_candidate_from_annotation, browser_replay_secret_mask,
    browser_response_sync, browser_settings_plan, browser_workflow_review_editor_for_field,
    browser_workflow_review_editor_mutation, browser_workflow_review_projection,
    calculate_browser_split, discard_browser_workflow_review, normalize_browser_address,
    preview_browser_workflow_review, render_browser_pane, save_browser_workflow_review,
    selected_browser_tab_id, BrowserActionPlan, BrowserHostReconcilePlan, BrowserHostVisibility,
    BrowserPaneAction, BrowserPaneActions, BrowserPaneContext, BrowserPaneEventPlan,
    BrowserPaneModel, BrowserPaneSurface, BrowserPaneTransient, BrowserReplayPaneProjection,
    BrowserReplaySecretPromptEvent, BrowserReplaySecretPromptOperation,
    BrowserReplaySecretPromptProjection, BrowserReplaySecretPromptVault, BrowserSettingsAction,
    BrowserSettingsPlan, BrowserSnapshotSync, BrowserSplitLayout, BrowserViewportPreset,
    BrowserWorkflowReviewAssertionKind, BrowserWorkflowReviewEditor,
    BrowserWorkflowReviewEditorField, BrowserWorkflowReviewInputProjection,
    BrowserWorkflowReviewMetadataProjection, BrowserWorkflowReviewMutation,
    BrowserWorkflowReviewProjection, BrowserWorkflowReviewStepProjection,
    BrowserWorkflowReviewUiState, BROWSER_REPLAY_SECRET_MASK,
};
pub use policy::{classify_upload_path, BrowserApprovalPolicy, BrowserIoRole, BrowserRisk};
pub use projection::{
    projection_meta, BrowserProjectionError, BrowserProjectionEvent, BrowserProjectionSession,
};
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
    compile_browser_replay, BrowserReplayActiveState, BrowserReplayCancellationLease,
    BrowserReplayCoordinator, BrowserReplayError, BrowserReplayExecutionHandle,
    BrowserReplayFailureCode, BrowserReplayInstance, BrowserReplayPlan, BrowserReplayProjection,
    BrowserReplayPublicInput, BrowserReplayStart, BrowserReplayStatus,
    MAX_BROWSER_REPLAY_FILE_BYTES, MAX_BROWSER_REPLAY_INPUTS, MAX_BROWSER_REPLAY_INPUT_NAME_BYTES,
    MAX_BROWSER_REPLAY_STEPS, MAX_BROWSER_REPLAY_TEXT_BYTES, MAX_BROWSER_REPLAY_URL_BYTES,
};
pub use replay_executor::execute_browser_replay;
pub(crate) use replay_repair::BrowserReplayRepairResumeCursor;
pub use replay_repair::{
    BrowserReplayLocatorSlot, BrowserReplayRepairInstance, BrowserReplayRepairPhase,
    BrowserReplayRepairProjection,
};
pub use replay_secrets::{
    BrowserReplaySecretError, BrowserReplaySecretLease, BrowserReplaySecretStore,
    BrowserReplaySecretSubmission, MAX_BROWSER_REPLAY_SECRET_INPUTS,
    MAX_BROWSER_REPLAY_SECRET_INPUT_NAME_BYTES, MAX_BROWSER_REPLAY_SECRET_VALUE_BYTES,
};
pub use resources::{
    resource_id_from_uri, resource_uri, BrowserResource, BrowserResourceHandle,
    BrowserResourceKind, BrowserResourceLimits, BrowserResourceMetadata, BrowserResourceStore,
};
pub use service::{
    reject_serialized_secrets, BrowserRepairProposal, BrowserSecretPlaceholder, BrowserTaskService,
    BrowserTaskServiceError,
};
pub use storage::BrowserStorageLayout;
pub use surface::{
    BoundsEpoch, BrowserSurfaceDescriptor, BrowserSurfaceFixture, BrowserSurfaceFixtureError,
    BrowserSurfaceFixtureSnapshot, BrowserSurfaceHost, BrowserSurfaceIdentity,
    BrowserSurfaceRegistration, BrowserSurfaceSnapshot, ClientBinding, DpiScale, DpiScaleError,
    FocusEpoch, HostHwndOwnership, HostHwndOwnershipError, HostProcessIdentity, HostSurfaceRequest,
    HostTeardownProof, PhysicalBounds, PhysicalBoundsError, ProcessIdentity, ProcessIdentityError,
    RuntimeGeneration, SurfaceAction, SurfaceAttachRequest, SurfaceAuthority, SurfaceBoundsUpdate,
    SurfaceClientRequest, SurfaceCommand, SurfaceDescriptorField, SurfaceDetachReason,
    SurfaceEpochError, SurfaceError, SurfaceEvent, SurfaceEventKind, SurfaceFocusUpdate,
    SurfaceInputAction, SurfaceInputReceipt, SurfaceInputRequest, SurfaceLifecycle, SurfaceNonce,
    SurfaceNonceError, SurfaceOwner, SurfaceParkReason, SurfacePermission, SurfacePermissions,
    SurfaceReceipt, SurfaceTaskSwitchReceipt, SurfaceTaskSwitchRequest, SurfaceTeardownReason,
    SurfaceThreadAffinity, SurfaceWindowHandle, SurfaceWindowHandleError, TextInputError,
    BROWSER_SURFACE_FIXTURE_CLICK_TOKEN, BROWSER_SURFACE_FIXTURE_RETAINED_STATE,
    BROWSER_SURFACE_FIXTURE_VISIBLE_TOKEN, MAX_SURFACE_EVENTS, MAX_SURFACE_TARGET_TOKEN_BYTES,
    MAX_SURFACE_TEXT_INPUT_BYTES,
};
pub use surface::{
    BrowserDockChrome, BrowserDockError, BrowserDockFocusTarget, BrowserDockGesture,
    BrowserDockSurface, BrowserPointerDisposition,
};
pub use teardown::{
    BrowserRecoveryCause, BrowserRecoveryController, BrowserRecoveryError, BrowserRecoveryOutcome,
    BrowserTeardownStage, BROWSER_TEARDOWN_STAGE_COUNT,
};
pub use workflow_mcp::{
    get_browser_workflow_recipe, list_browser_workflow_recipes, BrowserWorkflowRecipeGet,
    BrowserWorkflowRecipeInputSummary, BrowserWorkflowRecipeSummary,
};
pub(crate) use workflow_mcp::{
    BrowserWorkflowMcpService, BrowserWorkflowRepairApplyResult, BrowserWorkflowReplayStatus,
    BrowserWorkflowServiceError,
};
