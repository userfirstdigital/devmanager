//! Process supervisor acceptance surface.
//!
//! Phase 3 identity slice: pure process identity and ownership values only.

#[path = "process_supervisor/identity.rs"]
mod identity;

#[path = "process_supervisor/job.rs"]
mod job;

#[path = "process_supervisor/launcher.rs"]
mod launcher;

#[path = "process_supervisor/membership.rs"]
mod membership;

#[path = "process_supervisor/registry.rs"]
mod registry;

#[path = "process_supervisor/teardown.rs"]
mod teardown;
