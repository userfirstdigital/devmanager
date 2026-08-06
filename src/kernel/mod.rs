//! Kernel persistence: SQLite event store and deterministic projections.
//!
//! Schema and projector internals stay crate-private. Callers only see
//! domain-safe store APIs — never rusqlite types or a connection accessor.

mod artifact_content;
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

pub(crate) use artifact_content::{ArtifactContentError, ArtifactContentRegistry};
pub use command_bus::CommandBus;
pub(crate) use command_bus::HostCleanupUnit;
pub use dispatch::{
    AmbiguityDisposition, DispatchClaim, DispatchCompletion, DispatchPermit, ReconciliationClaim,
    ReconciliationFinding, ReconciliationOrigin,
};
pub(crate) use maintenance::{StoreMaintenanceReport, WalCheckpointOutcome};
pub use outbox::{DestinationClass, Effect, ReplayPolicy};
pub(crate) use replay::{EventReplaySession, ReplayError};
pub use runtime::{
    CompletionDisposition, RecoveringResource, RuntimePresence, RuntimeRegistry,
    RuntimeRegistryError,
};
pub(crate) use snapshot::{SnapshotError, SnapshotSession};
pub use store::{KernelStore, ProjectionRebuild, StoreError};
