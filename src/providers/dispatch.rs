//! Bounded, host-owned delivery of durable provider-input effects.
//!
//! Acceptance only appends an outbox effect. This lane is the only place that
//! may bind the effect to a live stock provider runtime and settle it. A
//! missing live session is held and released for a later bounded pass; once a
//! write boundary has been entered, failure is recorded as uncertainty and is
//! never silently replayed.

use std::time::Duration;

use crate::domain::operation::OperationState;
use crate::domain::provider_input::ProviderInputAction;
use crate::kernel::{AmbiguityDisposition, DestinationClass, Effect, KernelStore, StoreError};
use crate::providers::input::{
    sequence_provider_action, ProviderInputDeliveryError, ProviderInputDeliveryIdentity,
    ProviderInputWriteReceipt,
};
use crate::services::{ProcessManager, ProcessManagerProviderLauncher};

const DISPATCH_LEASE: Duration = Duration::from_secs(30);
const HOLD_RETRY: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderDispatchOutcome {
    Idle,
    Held,
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
    Hold,
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
            disposition: WriteFailureDisposition::Hold,
        })?;
        let handle = self
            .launcher
            .write_handle_for_identity(identity)
            .map_err(|error| ProviderWriteFailure {
                error,
                disposition: WriteFailureDisposition::Hold,
            })?;
        handle
            .write_action(identity, action, &plan)
            .map_err(|error| ProviderWriteFailure {
                error,
                disposition: match error {
                    ProviderInputDeliveryError::SessionNotBound
                    | ProviderInputDeliveryError::StaleGeneration
                    | ProviderInputDeliveryError::StaleFence
                    | ProviderInputDeliveryError::ProviderMismatch
                    | ProviderInputDeliveryError::RuntimeAuthorityAbsent
                    | ProviderInputDeliveryError::UnsupportedAction
                    | ProviderInputDeliveryError::ActionMismatch
                    | ProviderInputDeliveryError::BytesMismatch => WriteFailureDisposition::Hold,
                    ProviderInputDeliveryError::PostBoundaryFailure => {
                        WriteFailureDisposition::Uncertain
                    }
                },
            })
    }
}

/// One bounded provider effect pass. It is intentionally called by host
/// maintenance, never by the request dispatcher or UI hot path.
pub struct ProviderDispatchRuntime {
    authority: Box<dyn ProviderDispatchWriteAuthority>,
}

impl std::fmt::Debug for ProviderDispatchRuntime {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ProviderDispatchRuntime")
    }
}

impl ProviderDispatchRuntime {
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
        let Some(claim) = store
            .claim_next_dispatch_for_destination(DestinationClass::ProviderInput, DISPATCH_LEASE)?
        else {
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
            Err(failure) if failure.disposition == WriteFailureDisposition::Hold => {
                let _ = failure.error;
                store.defer_dispatch_before_boundary(&permit, HOLD_RETRY)?;
                Ok(ProviderDispatchOutcome::Held)
            }
            Err(failure) => {
                let _ = failure.error;
                let disposition = store.record_dispatch_ambiguity(&permit, HOLD_RETRY)?;
                if disposition != AmbiguityDisposition::Uncertain {
                    return Err(StoreError::Corruption);
                }
                Ok(ProviderDispatchOutcome::Uncertain {
                    operation_id: *operation_id,
                })
            }
        }
    }
}
