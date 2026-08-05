//! Host process ownership primitives.
//!
//! The host lock binds to an explicitly supplied profile root and never
//! resolves installed app-data paths on its own.

mod lock;

pub use lock::{HostIdentity, HostLock, HostLockError};
