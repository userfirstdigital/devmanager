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
pub mod preferences;
pub mod subscription;

pub use action::{
    ACTION_HOST_ACTIONS, ACTION_HOST_STATUS, ACTION_TASK_SHOW, ActionDescriptor, ActionRisk,
    ActionScope, catalog, require_unique_ids, task_show_query,
};
pub use cli::{CliError, CtlCommand, dispatch_ctl_from_args, parse_ctl_args, run_ctl};
pub use composer::{
    ComposerDraft, ComposerInsertionMode, ExactPromptPayload, ProviderCommandSuggestion,
    PutPromptVersionInComposer, apply_put_prompt_version, put_prompt_version_in_composer,
};
pub use connection::{ClientConnection, UnsolicitedServerMessage, connect, perform_client_hello};
pub use host_client::{
    ArtifactContentBatch, EventReplayBatch, HostClient, HostClientConfig, TrackedOperation,
};
pub use inbox_controller::{
    InboxControllerError, InboxHostController, InboxLane, InboxLaneTick, InboxTransport,
    InboxTransportFuture, SharedInboxSubscription,
};
pub(crate) use model::normalize_bounded_search_text;
pub use model::{
    ClientModel, ClientModelBuilder, ClientModelError, SearchContinuation, SearchPage,
    SearchPageStatus,
};
pub use preferences::{ClientPreferenceError, InboxPreferenceStore};
pub use subscription::{
    ClientSubscription, ClientSubscriptionState, SubscriptionError, SubscriptionUpdate,
};
