//! Kernel persistence: SQLite event store and deterministic projections.
//!
//! Schema and projector internals stay crate-private. Callers only see
//! domain-safe store APIs — never rusqlite types or a connection accessor.

mod command_bus;
mod dispatch;
mod lineage;
mod maintenance;
mod outbox;
mod projector;
mod replay;
mod runtime;
mod schema;
mod snapshot;
mod store;

pub use dispatch::{
    AmbiguityDisposition, DispatchClaim, DispatchCompletion, DispatchPermit, ReconciliationClaim,
    ReconciliationFinding, ReconciliationOrigin,
};
pub(crate) use maintenance::{StoreMaintenanceReport, WalCheckpointOutcome};
pub use outbox::{DestinationClass, Effect, ReplayPolicy};
pub use runtime::{
    CompletionDisposition, RecoveringResource, RuntimePresence, RuntimeRegistry,
    RuntimeRegistryError,
};
pub use store::{KernelStore, ProjectionRebuild, StoreError};
