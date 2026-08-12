//! Host-owned SSH launch/auth contract.
//!
//! Only the typed fail-closed runtime outcome is public at this stage.  The
//! launch and credential seams remain crate-private until the Task 3
//! supervisor can issue exact process-bound authority.

mod credentials;
mod launch;

pub use launch::{ssh_runtime_outcome, SshRuntimeOutcome, SshUnavailableReason};
