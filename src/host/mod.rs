//! Host process ownership primitives.
//!
//! The host lock binds to an explicitly supplied profile root and never
//! resolves installed app-data paths on its own.

mod ipc;
mod lock;

pub(crate) use ipc::{
    handshake_codecs, handshake_timeout, read_physical_frame, write_physical_frame,
};
pub use ipc::{
    pipe_endpoint_for_named_profile, profile_fingerprint_for_named_profile, AcceptHelloConfig,
    AcceptedHello, HelloListener, IpcError,
};
pub use lock::{HostIdentity, HostLock, HostLockError};
