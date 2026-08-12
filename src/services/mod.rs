pub mod binding;
pub mod env_service;
pub mod health;
pub mod model;
pub mod pid_file;
pub mod platform_service;
pub mod ports_service;
mod process_manager;
mod process_ops;
pub mod pwsh_probe;
pub mod scanner_service;
mod session_manager;
pub mod supervisor;

pub use binding::*;
pub use env_service::*;
pub use health::*;
pub use model::*;
pub use pid_file::*;
pub use platform_service::*;
pub use ports_service::*;
pub(crate) use process_manager::ai_session_needs_restore;
pub use process_manager::{ManagedShutdownReport, ProcessManager, RemoteSessionEvent};
pub use process_ops::{ProcessOpCompletion, ProcessOpKind};
pub use scanner_service::*;
pub use session_manager::{ConfigImportMode, SessionManager};
pub use supervisor::{
    session_status_for_ui, BoundedServiceLog, DueProbe, ManagedLaunchSpec, ProbeKind,
    RedactedSupervisorEvent, SupervisorAction, SupervisorError, SupervisorEventKind,
    SupervisorOutcome,
};

#[cfg(test)]
#[path = "../../tests/configured_service_supervisor/runtime.rs"]
mod supervisor_runtime_acceptance;
