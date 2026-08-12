//! Incremental deletion ledger for the presentation-era web bridge.
//!
//! Each row records an old RemoteAction / lease / snapshot path that
//! ConnectSession now owns. Rows stay until Task 9.10 closes the cutover.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeletionLedgerEntry {
    pub old_path: &'static str,
    pub replacement: &'static str,
    pub status: DeletionStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeletionStatus {
    Replaced,
    CompatibilityShim,
    PendingHostWiring,
}

pub const DELETION_LEDGER: &[DeletionLedgerEntry] = &[
    DeletionLedgerEntry {
        old_path: "src/remote/web/bridge.rs::ws_handler presentation Snapshot/Delta",
        replacement: "src/connect/session.rs::resume_snapshot + durable events",
        status: DeletionStatus::CompatibilityShim,
    },
    DeletionLedgerEntry {
        old_path: "src/remote/web/bridge.rs::composer / writer-lease mutations",
        replacement: "src/connect/session.rs::handle_request CommandEnvelope",
        status: DeletionStatus::CompatibilityShim,
    },
    DeletionLedgerEntry {
        old_path: "src/remote/web/request_executor.rs RemoteAction dispatch",
        replacement: "src/client/port.rs::HostCommandPort",
        status: DeletionStatus::CompatibilityShim,
    },
    DeletionLedgerEntry {
        old_path: "src/remote/web/input_executor.rs terminal presentation input",
        replacement: "src/connect/session.rs::submit_provider_input",
        status: DeletionStatus::CompatibilityShim,
    },
    DeletionLedgerEntry {
        old_path: "src/remote/web/lease.rs WriterLease",
        replacement: "accepted CommandReceipt pending until OperationSettlement",
        status: DeletionStatus::CompatibilityShim,
    },
    DeletionLedgerEntry {
        old_path: "GET /pair?t= token-in-URL pairing",
        replacement: "POST /pair JSON body via connect::direct::DirectPairingExchange",
        status: DeletionStatus::Replaced,
    },
    DeletionLedgerEntry {
        old_path: "src/host/mod.rs in-process HostClient attach from web",
        replacement: "HostCommandPort adapter once host wiring is in scope",
        status: DeletionStatus::PendingHostWiring,
    },
];

pub fn replaced_paths() -> impl Iterator<Item = &'static DeletionLedgerEntry> {
    DELETION_LEDGER
        .iter()
        .filter(|entry| entry.status == DeletionStatus::Replaced)
}
