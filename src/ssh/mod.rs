//! Host-owned SSH launch/auth contract.
//!
//! Only the typed fail-closed runtime outcome is public at this stage.  The
//! launch and credential seams remain crate-private until the Task 3
//! supervisor can issue exact process-bound authority.

mod cockpit;
mod credentials;
mod launch;

pub(crate) use cockpit::{accept_exact_endpoint, redacted_endpoints, SshEndpointDenial};
pub use launch::{ssh_runtime_outcome, SshRuntimeOutcome, SshUnavailableReason};
