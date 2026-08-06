//! Bounded process-empty task teardown worker.
//!
//! One `run_once` settles at most one current `BeginTaskTeardown` whose Closing
//! task has no Active or Releasing Task-owned resources. Unrelated outbox work
//! is skipped without claiming.

use std::time::Duration;

use crate::domain::id::{OperationId, TaskId};
use crate::kernel::{CommandBus, StoreError};

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
