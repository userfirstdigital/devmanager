pub mod identity;
pub(crate) mod job;
pub(crate) mod launcher;
pub mod registry;
pub mod teardown;

// These acceptance modules exercise the crate-private authority seams from
// inside the library test crate. Keeping them here preserves the production
// sealing boundary: an external crate cannot implement the teardown or Job
// membership extension points.
#[cfg(test)]
#[path = "../../tests/process_supervisor/membership.rs"]
mod membership_acceptance;

#[cfg(test)]
#[path = "../../tests/process_supervisor/registry.rs"]
mod registry_acceptance;

#[cfg(test)]
#[path = "../../tests/process_supervisor/teardown.rs"]
mod teardown_acceptance;

#[cfg(test)]
#[path = "../../tests/process_supervisor/job.rs"]
mod job_acceptance;

#[cfg(test)]
#[path = "../../tests/process_supervisor/launcher.rs"]
mod launcher_acceptance;

#[cfg(all(test, windows))]
#[path = "../../tests/process_supervisor/teardown_windows.rs"]
mod teardown_windows_acceptance;
