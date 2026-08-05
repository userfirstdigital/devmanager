//! Kernel persistence: SQLite event store and deterministic projections.
//!
//! Schema and projector internals stay crate-private. Callers only see
//! domain-safe store APIs — never rusqlite types or a connection accessor.

mod command_bus;
mod lineage;
mod outbox;
mod projector;
mod schema;
mod store;

pub use store::{KernelStore, ProjectionRebuild, StoreError};
