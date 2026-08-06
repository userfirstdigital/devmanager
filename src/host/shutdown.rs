//! Bounded process-empty task teardown and Closing host-cleanup workers.
//!
//! One `ProcessEmptyTeardownWorker::run_once` settles at most one current
//! `BeginTaskTeardown` whose Closing task has no Active or Releasing Task-owned
//! resources. Unrelated outbox work is skipped without claiming.
//!
//! One `HostCleanupWorker::run_once` advances exactly one durable host-cleanup
//! journal unit while host admission is Closing. It records honest residue only
//! and never claims OS processes were stopped or that the host exited.

use std::time::Duration;

use crate::domain::host::{HostCleanupBranch, HostCleanupBranchOutcome};
use crate::domain::id::{OperationId, TaskId};
use crate::kernel::{CommandBus, HostCleanupUnit, StoreError};

/// Fixed lease used for the single in-process claim/begin/settle transaction.
const PROCESS_EMPTY_TEARDOWN_LEASE: Duration = Duration::from_secs(30);

/// Result of one process-empty teardown worker pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessEmptyTeardown {
    /// No eligible process-empty teardown was available.
    Idle,
    /// Exactly one eligible teardown was settled.
    Settled {
        task_id: TaskId,
        operation_id: OperationId,
    },
}

/// Synchronous host primitive that drains at most one empty task teardown.
pub struct ProcessEmptyTeardownWorker;

impl ProcessEmptyTeardownWorker {
    /// Settle at most one eligible process-empty `BeginTaskTeardown`.
    pub fn run_once(bus: &mut CommandBus) -> Result<ProcessEmptyTeardown, StoreError> {
        match bus.settle_next_process_empty_task_teardown(PROCESS_EMPTY_TEARDOWN_LEASE)? {
            None => Ok(ProcessEmptyTeardown::Idle),
            Some((task_id, operation_id)) => Ok(ProcessEmptyTeardown::Settled {
                task_id,
                operation_id,
            }),
        }
    }
}

/// Result of one Closing host-cleanup journal advancement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostCleanupProgress {
    /// Host admission is Open, journal already terminalized, or nothing to do.
    Idle,
    /// TaskTeardowns settled one process-empty teardown without terminalizing.
    Progressed {
        task_id: TaskId,
        operation_id: OperationId,
    },
    /// Exactly one fixed branch was terminally recorded.
    BranchCompleted {
        operation_id: OperationId,
        action_epoch: u64,
        branch: HostCleanupBranch,
        outcome: HostCleanupBranchOutcome,
    },
    /// Complete all-success journal; physical exit is deferred to a later slice.
    ReadyToExit {
        operation_id: OperationId,
        action_epoch: u64,
    },
    /// Complete journal with ≥1 failed branch; durable `OperationFailed(CleanupFailed)` appended.
    Failed {
        operation_id: OperationId,
        action_epoch: u64,
        settled_at_ms: i64,
    },
}

/// Thin worker that advances one durable host-cleanup unit per call.
pub struct HostCleanupWorker;

impl HostCleanupWorker {
    /// Advance at most one Closing host-cleanup journal unit.
    pub fn run_once(bus: &mut CommandBus) -> Result<HostCleanupProgress, StoreError> {
        match bus.advance_next_host_cleanup_unit(PROCESS_EMPTY_TEARDOWN_LEASE)? {
            HostCleanupUnit::Idle => Ok(HostCleanupProgress::Idle),
            HostCleanupUnit::Progressed {
                task_id,
                operation_id,
            } => Ok(HostCleanupProgress::Progressed {
                task_id,
                operation_id,
            }),
            HostCleanupUnit::BranchCompleted {
                operation_id,
                action_epoch,
                branch,
                outcome,
            } => Ok(HostCleanupProgress::BranchCompleted {
                operation_id,
                action_epoch,
                branch,
                outcome,
            }),
            HostCleanupUnit::ReadyToExit {
                operation_id,
                action_epoch,
            } => Ok(HostCleanupProgress::ReadyToExit {
                operation_id,
                action_epoch,
            }),
            HostCleanupUnit::Failed {
                operation_id,
                action_epoch,
                settled_at_ms,
            } => Ok(HostCleanupProgress::Failed {
                operation_id,
                action_epoch,
                settled_at_ms,
            }),
        }
    }
}
