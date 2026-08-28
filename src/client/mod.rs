//! Minimal local ClientHello helper, duplex multiplexed connection,
//! authenticated Connect client, and the reusable HostClient wrapper for
//! profile-derived attach/reconnect.

pub mod action;
pub mod cli;
pub mod command_center;
pub mod composer;
mod connect_client;
mod connection;
pub mod fleet;
mod fleet_port;
mod host_client;
pub mod inbox_controller;
pub mod model;
pub mod port;
pub mod preferences;
mod remote_transport;
mod remote_trust;
pub mod subscription;
mod typed_queries;

pub use action::{
    action_enabled_with_service_state, catalog, cockpit_surface_descriptors, require_unique_ids,
    service_action_disabled_reason, service_control_command, service_control_command_with_task,
    task_cockpit_query, task_show_query, ActionDescriptor, ActionRisk, ActionScope,
    BrowserActionRequest, CockpitSurfaceAccess, CockpitSurfaceDescriptor, CockpitSurfaceKind,
    ServiceControlActionError, ServiceControlArguments, ACTION_BROWSER_NATIVE, ACTION_FILES_LIST,
    ACTION_FILES_READ, ACTION_GIT_STATUS, ACTION_HOST_ACTIONS, ACTION_HOST_STATUS,
    ACTION_SERVICE_HEALTH, ACTION_SERVICE_LOGS, ACTION_SERVICE_RESTART, ACTION_SERVICE_START,
    ACTION_SERVICE_STOP, ACTION_SSH_STATUS, ACTION_TASK_SHOW, ACTION_WORKSPACE_STATUS,
};
pub use cli::{dispatch_ctl_from_args, parse_ctl_args, run_ctl, CliError, CtlCommand};
pub use composer::{
    apply_put_prompt_version, put_prompt_version_in_composer, ComposerDraft, ComposerInsertionMode,
    ExactPromptPayload, ProviderCommandSuggestion, PutPromptVersionInComposer,
};
pub use connect_client::{
    connect_authenticated, connect_authenticated_halves, connect_command_request,
    connect_query_request, open_loopback_connect_ws, parse_connect_greeting, ConnectClientConfig,
    ConnectGreeting,
};
pub use connection::{
    connect, perform_client_hello, ClientConnection, ConnectAuthenticatedSession,
    ConnectionMetadata, UnsolicitedServerMessage,
};
pub(crate) use host_client::track_accepted_receipt;
pub use fleet::{
    FleetAdmission, FleetError, FleetOwned, FleetRemoval, FleetRetainedCommand,
    FleetUncertainCommand, FleetUnsupportedKind, HostClientConnectFuture, HostClientFactory,
    HostFleet, HostHandle, HostId, HostTaskKey, MAX_FLEET_HOSTS,
};
pub use fleet_port::FleetClientPort;
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
    SearchContinuation, SearchPage, SearchPageStatus, SearchScope, StreamAdmissionReject,
    TaskCockpitSurfaceProjection, TaskInboxPreview, PROVIDER_QUOTA_MAX_AGE_MS,
};
pub use port::{
    ApprovalAnswerCall, AsyncHostRequestPort, ConnectHostCommandPort, HostClientConnectPort,
    HostCommandPort, HostPortError, PromptMetadataItem, PromptMetadataPage, PromptQueryCall,
    ProviderInputCall, TranscriptFetchCall,
};
pub use preferences::{ClientPreferenceError, InboxPreferenceStore};
pub use remote_transport::{
    build_rustls_client_config, extract_pairing_cookie_header, get_bounded, get_bounded_until,
    hex_encode, open_remote_connect_ws, open_remote_connect_ws_until,
    parse_devmanager_connect_meta, parse_host_public_id, parse_host_public_key_hex,
    post_pair_collect_cookie, post_pair_collect_cookie_until, validate_additional_ca_pem,
    validate_http_header_value, validate_remote_endpoint, PublishedHostIdentity, RemoteEndpoint,
    RemoteHttpResponse, RemoteIo, RemoteTlsOptions, RemoteTransportError, REMOTE_CA_PEM_MAX_BYTES,
    REMOTE_CONNECT_PATH_MAX_BYTES, REMOTE_COOKIE_MAX_BYTES, REMOTE_HTTP_MAX_BODY_BYTES,
    REMOTE_TRANSPORT_DEFAULT_DEADLINE,
};
pub use typed_queries::{
    confirm_host_quit, inspect_host_quit, query_agent_connection, query_config_sidebar,
    query_git_repositories, query_prompt_library, query_provider_settings, query_remote_access,
    query_task_cockpit, task_snapshot,
};
pub use remote_trust::{
    connect_trusted_host, fetch_published_host_identity, forget_trusted_host, list_trusted_hosts,
    pair_enroll_and_connect,
    ConnectTrustedOptions, PairEnrollRequest, RemoteDeviceCustody, RemoteDevicePublicId,
    RemoteTrustError, RemoteTrustStore, TrustedHostRecord,
};
pub use subscription::{
    ClientSubscription, ClientSubscriptionState, SubscriptionError, SubscriptionUpdate,
};
