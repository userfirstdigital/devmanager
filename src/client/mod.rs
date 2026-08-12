//! Minimal local ClientHello helper, duplex multiplexed connection, and
//! the reusable HostClient wrapper for profile-derived attach/reconnect.

pub mod action;
pub mod cli;
pub mod command_center;
pub mod composer;
mod connection;
mod host_client;
pub mod inbox_controller;
pub mod model;
pub mod port;
pub mod preferences;
pub mod subscription;

pub use action::{
    action_enabled_with_service_state, catalog, cockpit_surface_descriptors, require_unique_ids,
    service_action_disabled_reason, service_control_command, service_control_command_with_task,
    task_cockpit_query, task_show_query, ActionDescriptor, ActionRisk, ActionScope,
    BrowserActionRequest, CockpitSurfaceAccess, CockpitSurfaceDescriptor, CockpitSurfaceKind,
    ServiceControlActionError,
    ServiceControlArguments, ACTION_FILES_LIST, ACTION_FILES_READ, ACTION_GIT_STATUS,
    ACTION_BROWSER_NATIVE, ACTION_HOST_ACTIONS, ACTION_HOST_STATUS, ACTION_SERVICE_HEALTH,
    ACTION_SERVICE_LOGS,
    ACTION_SERVICE_RESTART, ACTION_SERVICE_START, ACTION_SERVICE_STOP, ACTION_SSH_STATUS,
    ACTION_TASK_SHOW, ACTION_WORKSPACE_STATUS,
};
pub use cli::{dispatch_ctl_from_args, parse_ctl_args, run_ctl, CliError, CtlCommand};
pub use composer::{
    apply_put_prompt_version, put_prompt_version_in_composer, ComposerDraft, ComposerInsertionMode,
    ExactPromptPayload, ProviderCommandSuggestion, PutPromptVersionInComposer,
};
pub use connection::{connect, perform_client_hello, ClientConnection, UnsolicitedServerMessage};
pub(crate) use host_client::track_accepted_receipt;
pub use host_client::{
    ArtifactContentBatch, EventReplayBatch, HostClient, HostClientConfig, TrackedOperation,
};
pub use inbox_controller::{
    InboxControllerError, InboxHostController, InboxLane, InboxLaneTick, InboxTransport,
    InboxTransportFuture, SharedInboxSubscription,
};
pub(crate) use model::normalize_bounded_search_text;
pub use model::{
    admit_subscription_stream, one_fresh_quota_per_provider, quota_observation_is_fresh,
    AdmittedStreamFrame, ClientBrowserDockView, ClientModel, ClientModelBuilder, ClientModelError,
    SearchContinuation, SearchPage, SearchPageStatus, StreamAdmissionReject,
    TaskCockpitSurfaceProjection, PROVIDER_QUOTA_MAX_AGE_MS,
};
pub use port::{
    ApprovalAnswerCall, ConnectHostCommandPort, HostClientConnectPort, HostCommandPort,
    HostPortError, PromptMetadataItem, PromptMetadataPage, PromptQueryCall, ProviderInputCall,
    TranscriptFetchCall,
};
pub use preferences::{ClientPreferenceError, InboxPreferenceStore};
pub use subscription::{
    ClientSubscription, ClientSubscriptionState, SubscriptionError, SubscriptionUpdate,
};
