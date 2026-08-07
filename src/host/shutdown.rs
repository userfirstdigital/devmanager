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

use crate::domain::event::{DomainEvent, Event};
use crate::domain::host::{HostCleanupBranch, HostCleanupBranchOutcome};
use crate::domain::id::{EventId, OperationId, TaskId};
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

    /// Explicit all-success settle for c8b after physical exit is armed.
    ///
    /// Maintenance must never call this; exact retry is idempotent and returns
    /// the same persisted terminal [`DomainEvent`] (event id + sequence).
    pub fn settle_success(
        bus: &mut CommandBus,
    ) -> Result<HostCleanupSuccessSettlement, StoreError> {
        let terminal_event = bus.settle_host_cleanup_success()?;
        let (operation_id, action_epoch, settled_at_ms, result_event_ids) =
            settlement_fields_from_terminal(&terminal_event)?;
        Ok(HostCleanupSuccessSettlement {
            operation_id,
            action_epoch,
            settled_at_ms,
            result_event_ids,
            terminal_event,
        })
    }

    /// Read-only durable restart disposition for bind/serve decisions.
    pub fn restart_disposition(bus: &CommandBus) -> Result<HostRestartDisposition, StoreError> {
        Ok(match bus.host_restart_disposition()? {
            crate::kernel::HostRestartDispositionUnit::ServeResume => {
                HostRestartDisposition::ServeResume
            }
            crate::kernel::HostRestartDispositionUnit::ServeInspection {
                operation_id,
                action_epoch,
                settled_at_ms,
            } => HostRestartDisposition::ServeInspection {
                operation_id,
                action_epoch,
                settled_at_ms,
            },
            crate::kernel::HostRestartDispositionUnit::ReadyToArmAndSettle {
                operation_id,
                action_epoch,
            } => HostRestartDisposition::ReadyToArmAndSettle {
                operation_id,
                action_epoch,
            },
            crate::kernel::HostRestartDispositionUnit::Closed {
                operation_id,
                action_epoch,
                settled_at_ms,
            } => HostRestartDisposition::Closed {
                operation_id,
                action_epoch,
                settled_at_ms,
            },
        })
    }
}

fn settlement_fields_from_terminal(
    terminal_event: &DomainEvent,
) -> Result<(OperationId, u64, i64, Vec<EventId>), StoreError> {
    let Event::OperationSettled(fact) = &terminal_event.payload else {
        return Err(StoreError::Corruption);
    };
    let action_epoch = fact.action_epoch.ok_or(StoreError::Corruption)?;
    Ok((
        fact.operation_id,
        action_epoch,
        fact.settled_at_ms,
        fact.result_event_ids.clone(),
    ))
}

/// Exact all-success host-cleanup settlement recorded by [`HostCleanupWorker::settle_success`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostCleanupSuccessSettlement {
    pub operation_id: OperationId,
    pub action_epoch: u64,
    pub settled_at_ms: i64,
    pub result_event_ids: Vec<EventId>,
    /// Exact persisted terminal event (same id/sequence on idempotent retry).
    pub terminal_event: DomainEvent,
}

/// Read-only durable restart disposition derived from Closing admission state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostRestartDisposition {
    /// No Closing admission, or Accepted without a complete all-success journal.
    ServeResume,
    /// Exact durable `OperationFailed(CleanupFailed)`; keep serving for inspection.
    ServeInspection {
        operation_id: OperationId,
        action_epoch: u64,
        settled_at_ms: i64,
    },
    /// Complete all-success Accepted journal; arm physical exit then settle.
    ReadyToArmAndSettle {
        operation_id: OperationId,
        action_epoch: u64,
    },
    /// Exact all-success `OperationSettled`; exit before binding a new listener.
    Closed {
        operation_id: OperationId,
        action_epoch: u64,
        settled_at_ms: i64,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::command::{Command, CommandEnvelope, CommandReceipt, ConfirmHostQuitIntent};
    use crate::domain::event::Event;
    use crate::domain::id::CommandId;
    use crate::domain::ClientId;

    fn confirm_quit(bus: &mut CommandBus) -> OperationId {
        let inspection = bus.inspect_host_quit().expect("inspect");
        let receipt = bus
            .execute(CommandEnvelope {
                command_id: CommandId::new(),
                client_id: ClientId::new(),
                task_id: None,
                issued_at_ms: 1_725_000_000_800,
                expected_task_revision: None,
                command: Command::ConfirmHostQuit(ConfirmHostQuitIntent {
                    inspection_id: inspection.inspection_id,
                    allow_uninspected_worktrees: true,
                }),
            })
            .expect("confirm");
        match receipt {
            CommandReceipt::Accepted { operation_id, .. } => operation_id,
            other => panic!("expected Accepted, got {other:?}"),
        }
    }

    fn drive_ready(bus: &mut CommandBus, quit_op: OperationId) {
        for branch in HostCleanupBranch::ORDER {
            match HostCleanupWorker::run_once(bus).expect("branch") {
                HostCleanupProgress::BranchCompleted {
                    operation_id,
                    action_epoch,
                    branch: got,
                    ..
                } => {
                    assert_eq!(operation_id, quit_op);
                    assert_eq!(action_epoch, 1);
                    assert_eq!(got, branch);
                }
                other => panic!("expected BranchCompleted for {branch:?}, got {other:?}"),
            }
        }
        assert_eq!(
            HostCleanupWorker::run_once(bus).expect("ready"),
            HostCleanupProgress::ReadyToExit {
                operation_id: quit_op,
                action_epoch: 1,
            }
        );
    }

    #[test]
    fn settle_success_returns_exact_terminal_domain_event_identity_on_first_and_idempotent_retry() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut bus = CommandBus::open(&dir.path().join("settle-identity.db")).expect("bus");
        let quit_op = confirm_quit(&mut bus);
        drive_ready(&mut bus, quit_op);

        let first = HostCleanupWorker::settle_success(&mut bus).expect("first settle");
        assert_eq!(first.operation_id, quit_op);
        assert_eq!(first.action_epoch, 1);
        assert!(first.terminal_event.sequence > 0);
        assert_eq!(first.terminal_event.occurred_at_ms, first.settled_at_ms);
        assert_eq!(first.terminal_event.task_id, None);
        assert_eq!(first.terminal_event.task_revision, None);
        match &first.terminal_event.payload {
            Event::OperationSettled(fact) => {
                assert_eq!(fact.operation_id, quit_op);
                assert_eq!(fact.action_epoch, Some(1));
                assert_eq!(fact.result_event_ids, first.result_event_ids);
                assert_eq!(fact.settled_at_ms, first.settled_at_ms);
            }
            other => panic!("expected OperationSettled payload, got {other:?}"),
        }

        let second = HostCleanupWorker::settle_success(&mut bus).expect("idempotent");
        assert_eq!(second.terminal_event.id, first.terminal_event.id);
        assert_eq!(
            second.terminal_event.sequence,
            first.terminal_event.sequence
        );
        assert_eq!(second, first);
    }
}
