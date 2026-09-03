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
pub(crate) mod schema;
pub(crate) mod semantic_journal;
mod snapshot;
mod store;

use crate::domain::id::{ClientId, TaskId};
use uuid::Uuid;

/// Authenticated request namespace for all resumable read sessions. Cursor
/// bytes are HMAC protected, but the HMAC alone is not an authorization check;
/// this exact scope is also retained by the session registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct SessionScope {
    pub(crate) client_id: Option<ClientId>,
    pub(crate) task_id: Option<TaskId>,
    pub(crate) connection_id: Option<Uuid>,
    pub(crate) action_epoch: Option<u64>,
    pub(crate) runtime_generation: Option<u64>,
}

impl SessionScope {
    pub(crate) const GLOBAL: Self = Self {
        client_id: None,
        task_id: None,
        connection_id: None,
        action_epoch: None,
        runtime_generation: None,
    };
}

pub(crate) use artifact_content::{ArtifactContentError, ArtifactContentRegistry};
pub use command_bus::{CommandBus, LoadedTaskRuntime, TaskRuntimeLoadError, TerminalFactOutcome};
pub(crate) use command_bus::{HostCleanupUnit, HostRestartDispositionUnit};
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
