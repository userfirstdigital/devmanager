//! Minimal local ClientHello helper, duplex multiplexed connection, and
//! the reusable HostClient wrapper for profile-derived attach/reconnect.

pub mod action;
pub mod cli;
mod connection;
mod host_client;

pub use action::{
    catalog, require_unique_ids, task_show_query, ActionDescriptor, ActionRisk, ActionScope,
    ACTION_HOST_ACTIONS, ACTION_HOST_STATUS, ACTION_TASK_SHOW,
};
pub use cli::{dispatch_ctl_from_args, parse_ctl_args, run_ctl, CliError, CtlCommand};
pub use connection::{connect, perform_client_hello, ClientConnection, UnsolicitedServerMessage};
pub use host_client::{EventReplayBatch, HostClient, HostClientConfig, TrackedOperation};
