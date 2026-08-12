//! Host process ownership primitives.
//!
//! The host lock binds to an explicitly supplied profile root and never
//! resolves installed app-data paths on its own.

mod connection;
mod ipc;
mod lock;
mod shutdown;
mod update;

#[cfg(test)]
pub(crate) use connection::{ConnectionOutputHandle, ConnectionOutputId, OutputInspection};
pub use connection::{
    dispatch_host_request, HostExecutorOutcome, HostRequestExecutor, HostRequestHandle,
    PhysicalExitArmRequest, SupervisedHostExecutor, HOST_REQUEST_QUEUE_CAPACITY,
};
pub(crate) use ipc::{
    codecs_for_limits, handshake_codecs, handshake_timeout, read_physical_frame,
    read_physical_frame_idle_then_deadline, request_completion_timeout, supervise_duplex_halves,
    write_physical_frame, write_physical_frame_with_deadline,
};
pub use ipc::{
    pipe_endpoint_for_named_profile, profile_fingerprint_for_named_profile, AcceptHelloConfig,
    AcceptedHello, HelloListener, HostConnection, IpcError,
};
pub use lock::{HostIdentity, HostLock, HostLockError, HOST_EXIT_ALREADY_RUNNING};
pub use shutdown::{
    HostCleanupProgress, HostCleanupSuccessSettlement, HostCleanupWorker, HostRestartDisposition,
    ProcessEmptyTeardown, ProcessEmptyTeardownWorker,
};
pub use update::{
    update_inspection_from_host_quit, CommandBusActiveResourceProbe, HostConnectionUpdateProbe,
    HostQuitInspectionSource, HostUpdateAdmission, HostUpdateHandoff,
};
