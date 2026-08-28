//! Host process ownership primitives.
//!
//! The host lock binds to an explicitly supplied profile root and never
//! resolves installed app-data paths on its own.

mod agent_connection;
mod cockpit;
mod connect;
mod connection;
mod conversation_wake;
mod ipc;
mod lock;
mod organization_runtime;
mod provider_health;
mod provider_launch;
mod remote_access;
pub mod remote_setup;
mod shutdown;
mod update;

#[cfg(test)]
#[path = "provider_health_tests.rs"]
mod provider_health_tests;

pub use crate::updater::handoff::{HostUpdateAdmission, HostUpdateHandoff};
pub(crate) use connect::serve_host_connect_duplex;
pub use connection::{
    dispatch_host_request, HostConnectDuplex, HostExecutorOutcome, HostRequestExecutor,
    HostRequestHandle, PhysicalExitArmRequest, SupervisedHostExecutor, HOST_REQUEST_QUEUE_CAPACITY,
};
#[cfg(test)]
pub(crate) use connection::{ConnectionOutputHandle, ConnectionOutputId, OutputInspection};
pub(crate) use ipc::{
    agent_connection_query_timeout, codecs_for_limits, handshake_codecs, handshake_timeout,
    read_physical_frame, read_physical_frame_idle_then_deadline, request_completion_timeout,
    supervise_duplex_halves, task_cockpit_query_timeout, write_physical_frame,
    write_physical_frame_with_deadline,
};
pub use ipc::{
    pipe_endpoint_for_named_profile, profile_fingerprint_for_named_profile, AcceptHelloConfig,
    AcceptedHello, HelloListener, HostConnection, IpcError,
};
pub use lock::{HostIdentity, HostLock, HostLockError, HOST_EXIT_ALREADY_RUNNING};
pub use organization_runtime::{
    OrganizationEvidenceMetadata, OrganizationIpcCommand, OrganizationIpcQuery,
    OrganizationIpcReply, OrganizationPromptView, OrganizationRefreshReply, OrganizationRuntime,
    OrganizationRuntimeConfig, OrganizationRuntimeError, OrganizationRuntimeHandle,
    OrganizationSnapshot, ORGANIZATION_RUNTIME_DEFAULT_REFRESH_INTERVAL_MS,
    ORGANIZATION_RUNTIME_MAX_REFRESH_INTERVAL_MS,
};
pub use remote_access::{
    shutdown_remote_access_before_fullquit_arm, HostRemoteAccessController, HostRemoteAccessError,
};
pub use shutdown::{
    HostCleanupProgress, HostCleanupSuccessSettlement, HostCleanupWorker, HostRestartDisposition,
    ProcessEmptyTeardown, ProcessEmptyTeardownWorker,
};
pub use update::{
    owned_probe_from_quit_inspection, update_inspection_from_host_quit,
    HostExecutorActiveResourceProbe, HostQuitInspectionSource, HostUpdateRuntimeGate,
    OwnedActiveResourceProbe,
};

/// Capabilities implemented by every production native host. Optional
/// organization and service-supervisor capabilities are appended at runtime.
/// Keep the desktop client's requested set covered by this base before adding
/// a native query path.
pub const NATIVE_HOST_BASE_CAPABILITIES: [crate::protocol::Capability; 11] = [
    crate::protocol::Capability::PagedSnapshots,
    crate::protocol::Capability::EventReplay,
    crate::protocol::Capability::OperationSettlement,
    crate::protocol::Capability::ChunkResume,
    crate::protocol::Capability::PromptProjection,
    crate::protocol::Capability::ExplicitDetach,
    crate::protocol::Capability::HostShutdown,
    crate::protocol::Capability::UpdateHandoff,
    crate::protocol::Capability::ProviderInput,
    crate::protocol::Capability::TaskCockpit,
    crate::protocol::Capability::SemanticConversation,
];
