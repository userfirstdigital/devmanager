//! Minimal local ClientHello helper, duplex multiplexed connection, and
//! the reusable HostClient wrapper for profile-derived attach/reconnect.

pub mod action;
pub mod cli;
mod connection;
mod host_client;
pub mod inbox_controller;
pub mod model;
pub mod preferences;
pub mod subscription;

pub use action::{
    catalog, require_unique_ids, task_show_query, ActionDescriptor, ActionRisk, ActionScope,
    ACTION_HOST_ACTIONS, ACTION_HOST_STATUS, ACTION_TASK_SHOW,
};
pub use cli::{dispatch_ctl_from_args, parse_ctl_args, run_ctl, CliError, CtlCommand};
pub use connection::{connect, perform_client_hello, ClientConnection, UnsolicitedServerMessage};
pub use host_client::{
    ArtifactContentBatch, EventReplayBatch, HostClient, HostClientConfig, TrackedOperation,
};
pub use inbox_controller::{InboxControllerError, InboxHostController, SharedInboxSubscription};
pub use model::{
    ClientModel, ClientModelBuilder, ClientModelError, SearchContinuation, SearchPage,
    SearchPageStatus,
};
pub use preferences::{ClientPreferenceError, InboxPreferenceStore};
pub use subscription::{
    ClientSubscription, ClientSubscriptionState, SubscriptionError, SubscriptionUpdate,
};
