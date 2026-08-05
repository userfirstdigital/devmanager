//! Kernel persistence: SQLite event store and deterministic projections.
//!
//! Schema and projector internals stay crate-private. Callers only see
//! domain-safe store APIs — never rusqlite types or a connection accessor.

mod command_bus;
mod dispatch;
mod lineage;
mod outbox;
mod projector;
mod runtime;
mod schema;
mod store;

pub use dispatch::{
    AmbiguityDisposition, DispatchClaim, DispatchCompletion, DispatchPermit, ReconciliationClaim,
    ReconciliationFinding, ReconciliationOrigin,
};
pub use outbox::{DestinationClass, Effect, ReplayPolicy};
pub use runtime::{CompletionDisposition, RuntimePresence, RuntimeRegistry, RuntimeRegistryError};
pub use store::{KernelStore, ProjectionRebuild, StoreError};
