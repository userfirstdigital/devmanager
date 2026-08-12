pub mod binding;
pub mod cockpit;
pub mod env_service;
pub mod health;
pub mod launch_authority;
pub mod model;
pub mod pid_file;
pub mod platform_service;
pub mod ports_service;
mod process_manager;
mod process_ops;
mod provider_process_launcher;
pub mod pwsh_probe;
pub mod scanner_service;
mod session_manager;
pub mod supervisor;

pub use binding::{
    bind_configured_command, bind_configured_services, resolve_configured_env_file_path,
    with_task_workspace_root, BindingError, ConfiguredServiceBinding, ConfiguredServiceOwner,
    ConfiguredServiceSource, EnvironmentOverlay, TaskServicePathContext,
};
pub use cockpit::{
    filter_snapshots_for_task, snapshot_visible_to_task, supervisor_service_id, to_wire_health,
    to_wire_logs, to_wire_projection, TaskServiceCockpitProjection,
};
pub use env_service::*;
pub use health::*;
pub use launch_authority::{
    HostLiveLaunch, HostManagedLaunchAuthority, HostPendingLaunch, ServiceLaunchIssuer,
};
pub use model::*;
pub use pid_file::*;
pub use platform_service::*;
pub use ports_service::*;
pub(crate) use process_manager::ai_session_needs_restore;
pub use process_manager::{ManagedShutdownReport, ProcessManager, RemoteSessionEvent};
pub use process_ops::{ProcessOpCompletion, ProcessOpKind};
pub use provider_process_launcher::ProcessManagerProviderLauncher;
pub use scanner_service::*;
pub use session_manager::{ConfigImportMode, SessionManager};
pub use supervisor::{
    resolve_configured_service_program, resolve_configured_service_program_with,
    session_status_for_ui, BoundedServiceLog, ConfiguredServiceSupervisor, DueProbe, FakeFailStage,
    FakeLaunchAuthority, ManagedLaunchAuthority, ManagedLaunchSpec, ManagedLaunchStage,
    PortClaimView, ProbeKind, RedactedSupervisorEvent, ServiceSupervisor, SupervisorAction,
    SupervisorError, SupervisorEventKind, SupervisorOutcome, SupervisorRefusal,
};

#[cfg(test)]
#[path = "../../tests/configured_service_supervisor/runtime.rs"]
mod supervisor_runtime_acceptance;
