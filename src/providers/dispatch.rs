//! Bounded, host-owned delivery of durable provider-input effects.
//!
//! Acceptance only appends an outbox effect. This lane is the only place that
//! may bind the effect to a live stock provider runtime and settle it. A
//! missing live session is held and released for a later bounded pass; once a
//! write boundary has been entered, failure is recorded as uncertainty and is
//! never silently replayed.

use std::time::Duration;

use crate::domain::id::TaskId;
use crate::domain::operation::OperationState;
use crate::domain::provider_input::ProviderInputAction;
use crate::kernel::{AmbiguityDisposition, Effect, KernelStore, StoreError};
use crate::providers::input::{
    sequence_provider_action, ProviderInputDeliveryError, ProviderInputDeliveryIdentity,
    ProviderInputWriteReceipt,
};
use crate::services::{ProcessManager, ProcessManagerProviderLauncher};

const DISPATCH_LEASE: Duration = Duration::from_secs(30);
const HOLD_RETRY: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderDispatchHoldReason {
    SessionNotBound,
    RuntimeAuthorityAbsent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderDispatchOutcome {
    Idle,
    Held {
        task_id: TaskId,
        reason: ProviderDispatchHoldReason,
    },
    Settled {
        operation_id: crate::domain::OperationId,
    },
    Uncertain {
        operation_id: crate::domain::OperationId,
    },
    Recovered {
        disposition: AmbiguityDisposition,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriteFailureDisposition {
    Hold(ProviderDispatchHoldReason),
    HoldOther,
    Uncertain,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ProviderWriteFailure {
    error: ProviderInputDeliveryError,
    disposition: WriteFailureDisposition,
}

trait ProviderDispatchWriteAuthority: Send + Sync {
    fn write(
        &self,
        identity: &ProviderInputDeliveryIdentity,
        action: &ProviderInputAction,
    ) -> Result<ProviderInputWriteReceipt, ProviderWriteFailure>;
}

struct ProcessManagerDispatchAuthority {
    launcher: ProcessManagerProviderLauncher,
}

impl ProviderDispatchWriteAuthority for ProcessManagerDispatchAuthority {
    fn write(
        &self,
        identity: &ProviderInputDeliveryIdentity,
        action: &ProviderInputAction,
    ) -> Result<ProviderInputWriteReceipt, ProviderWriteFailure> {
        let plan = sequence_provider_action(action).map_err(|_| ProviderWriteFailure {
            error: ProviderInputDeliveryError::BytesMismatch,
            disposition: WriteFailureDisposition::HoldOther,
        })?;
        let handle = self
            .launcher
            .write_handle_for_identity(identity)
            .map_err(|error| ProviderWriteFailure {
                error,
                disposition: hold_disposition(error),
            })?;
        handle
            .write_action(identity, action, &plan)
            .map_err(|error| ProviderWriteFailure {
                error,
                disposition: hold_disposition(error),
            })
    }
}

fn hold_disposition(error: ProviderInputDeliveryError) -> WriteFailureDisposition {
    match error {
        ProviderInputDeliveryError::SessionNotBound => {
            WriteFailureDisposition::Hold(ProviderDispatchHoldReason::SessionNotBound)
        }
        ProviderInputDeliveryError::RuntimeAuthorityAbsent => {
            WriteFailureDisposition::Hold(ProviderDispatchHoldReason::RuntimeAuthorityAbsent)
        }
        ProviderInputDeliveryError::StaleGeneration
        | ProviderInputDeliveryError::StaleFence
        | ProviderInputDeliveryError::ProviderMismatch
        | ProviderInputDeliveryError::UnsupportedAction
        | ProviderInputDeliveryError::ActionMismatch
        | ProviderInputDeliveryError::BytesMismatch => WriteFailureDisposition::HoldOther,
        ProviderInputDeliveryError::PostBoundaryFailure => WriteFailureDisposition::Uncertain,
    }
}

/// One bounded provider effect pass. The host calls it after durable provider
/// input acceptance, after exact-session restoration, and as a maintenance
/// fallback. It never runs in the UI process.
pub struct ProviderDispatchRuntime {
    authority: Box<dyn ProviderDispatchWriteAuthority>,
}

impl std::fmt::Debug for ProviderDispatchRuntime {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ProviderDispatchRuntime")
    }
}

#[cfg(test)]
struct AlwaysUnboundDispatchAuthority;

#[cfg(test)]
impl ProviderDispatchWriteAuthority for AlwaysUnboundDispatchAuthority {
    fn write(
        &self,
        _identity: &ProviderInputDeliveryIdentity,
        _action: &ProviderInputAction,
    ) -> Result<ProviderInputWriteReceipt, ProviderWriteFailure> {
        Err(ProviderWriteFailure {
            error: ProviderInputDeliveryError::SessionNotBound,
            disposition: hold_disposition(ProviderInputDeliveryError::SessionNotBound),
        })
    }
}

impl ProviderDispatchRuntime {
    /// A pass whose write authority always refuses before the external
    /// boundary. Kernel tests use it to exercise claim, retirement and
    /// convergence without a process manager or a live provider.
    #[cfg(test)]
    pub(crate) fn unbound_for_test() -> Self {
        Self {
            authority: Box::new(AlwaysUnboundDispatchAuthority),
        }
    }

    pub(crate) fn from_process_manager(manager: ProcessManager) -> Self {
        Self {
            authority: Box::new(ProcessManagerDispatchAuthority {
                launcher: manager.provider_process_launcher(),
            }),
        }
    }

    pub(crate) fn run_once(
        &self,
        store: &mut KernelStore,
    ) -> Result<ProviderDispatchOutcome, StoreError> {
        if let Some(disposition) = store.recover_next_expired_dispatch(HOLD_RETRY)? {
            return Ok(ProviderDispatchOutcome::Recovered { disposition });
        }
        let (claim, retired) = store.claim_next_provider_input_dispatch(DISPATCH_LEASE)?;
        for row in &retired {
            // Once per retired row, never once per tick: the row reached a
            // terminal state in the scan's own transaction, so it can never be
            // selected, re-leased, or reported again.
            eprintln!(
                "provider dispatch retired a permanently stale effect: \
                 task_id={} operation_id={} outbox_id={} check={}",
                row.task_id,
                row.operation_id,
                row.outbox_id,
                row.check.as_str()
            );
        }
        let Some(claim) = claim else {
            return Ok(ProviderDispatchOutcome::Idle);
        };
        let permit = store.begin_dispatch(&claim)?;
        let Effect::DeliverProviderInput {
            task_id,
            operation_id,
            command_id,
            client_id,
            agent_session_id,
            provider_kind,
            provider_session_id,
            runtime_generation,
            action_epoch,
            turn_id,
            question_id,
            approval_id,
            action,
            ..
        } = permit.effect()
        else {
            return Err(StoreError::Corruption);
        };
        let identity = ProviderInputDeliveryIdentity {
            task_id: *task_id,
            operation_id: *operation_id,
            command_id: *command_id,
            client_id: *client_id,
            agent_session_id: *agent_session_id,
            provider_kind: *provider_kind,
            provider_session_id: provider_session_id.clone(),
            runtime_generation: *runtime_generation,
            action_epoch: *action_epoch,
            turn_id: *turn_id,
            question_id: *question_id,
            approval_id: *approval_id,
        };
        let held_task_id = *task_id;
        match self.authority.write(&identity, action) {
            Ok(receipt) => {
                let state = match store.settle_provider_input_delivery(&permit, &receipt) {
                    Ok(state) => state,
                    Err(error) => {
                        eprintln!(
                            "provider input durable settlement failed: operation_id={} error={:?}",
                            operation_id, error
                        );
                        let disposition = store.record_dispatch_ambiguity(&permit, HOLD_RETRY)?;
                        if disposition != AmbiguityDisposition::Uncertain {
                            return Err(StoreError::Corruption);
                        }
                        return Ok(ProviderDispatchOutcome::Uncertain {
                            operation_id: *operation_id,
                        });
                    }
                };
                if !matches!(state, OperationState::Settled { .. }) {
                    return Err(StoreError::Corruption);
                }
                Ok(ProviderDispatchOutcome::Settled {
                    operation_id: *operation_id,
                })
            }
            Err(failure) => match failure.disposition {
                WriteFailureDisposition::Hold(reason) => {
                    store.defer_dispatch_before_boundary(&permit, HOLD_RETRY)?;
                    Ok(ProviderDispatchOutcome::Held {
                        task_id: held_task_id,
                        reason,
                    })
                }
                WriteFailureDisposition::HoldOther => {
                    store.defer_dispatch_before_boundary(&permit, HOLD_RETRY)?;
                    // Non-restart holds stay deferred without advertising a
                    // SessionNotBound/RuntimeAuthorityAbsent restart signal.
                    Ok(ProviderDispatchOutcome::Idle)
                }
                WriteFailureDisposition::Uncertain => {
                    let disposition = store.record_dispatch_ambiguity(&permit, HOLD_RETRY)?;
                    if disposition != AmbiguityDisposition::Uncertain {
                        return Err(StoreError::Corruption);
                    }
                    Ok(ProviderDispatchOutcome::Uncertain {
                        operation_id: *operation_id,
                    })
                }
            },
        }
    }
}
