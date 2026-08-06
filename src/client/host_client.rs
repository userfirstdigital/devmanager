//! Reusable HostClient wrapper over the low-level ClientConnection.
//!
//! Tracks accepted OperationIds without inventing settlement. Settlement is
//! observed only through an explicit correlated OperationStatus query.

use std::collections::BTreeMap;

use uuid::Uuid;

use crate::domain::command::{CommandEnvelope, CommandReceipt};
use crate::domain::id::{CommandId, OperationId, RequestId};
use crate::domain::operation::OperationState;
use crate::domain::query::{Query, QueryEnvelope, QueryError, QueryOutcome, QueryResult};
use crate::domain::ClientId;
use crate::host::{
    pipe_endpoint_for_named_profile, profile_fingerprint_for_named_profile, IpcError,
};
use crate::protocol::{Capability, CapabilitySet, ClientHello, FrameLimits, ServerHello};

use super::{connect, ClientConnection};

/// Caller-owned connection configuration. `client_id` is never rotated here.
#[derive(Debug, Clone)]
pub struct HostClientConfig {
    pub named_profile: String,
    pub client_build: String,
    pub client_id: ClientId,
    pub requested: CapabilitySet,
    pub limits: FrameLimits,
}

/// Local tracking for an accepted operation. Acceptance never implies settlement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackedOperation {
    Pending {
        command_id: CommandId,
    },
    Resolved {
        command_id: CommandId,
        state: OperationState,
    },
}

/// Profile-derived host client with stable ClientId and operation tracking.
pub struct HostClient {
    config: HostClientConfig,
    endpoint: String,
    connection: Option<ClientConnection>,
    server_hello: ServerHello,
    tracked: BTreeMap<OperationId, TrackedOperation>,
}

impl HostClient {
    /// Validate the named profile, build ClientHello, and connect.
    pub async fn connect(config: HostClientConfig) -> Result<Self, IpcError> {
        let endpoint = pipe_endpoint_for_named_profile(&config.named_profile)?;
        let (connection, server_hello) = open_connection(&config, &endpoint).await?;
        Ok(Self {
            config,
            endpoint,
            connection: Some(connection),
            server_hello,
            tracked: BTreeMap::new(),
        })
    }

    /// Drop any prior connection, then rebuild Hello from the same config/client_id.
    /// A failed attempt leaves the client disconnected while preserving tracking.
    pub async fn reconnect(&mut self) -> Result<(), IpcError> {
        self.connection = None;
        match open_connection(&self.config, &self.endpoint).await {
            Ok((connection, server_hello)) => {
                self.connection = Some(connection);
                self.server_hello = server_hello;
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    /// Drop the live connection without clearing tracked operations.
    pub fn disconnect(&mut self) {
        self.connection = None;
    }

    pub fn is_connected(&self) -> bool {
        self.connection.is_some()
    }

    pub fn client_id(&self) -> ClientId {
        self.config.client_id
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub fn granted_capabilities(&self) -> CapabilitySet {
        self.server_hello.granted
    }

    pub fn connection_id(&self) -> Uuid {
        self.server_hello.connection_id
    }

    pub fn host_boot_id(&self) -> Uuid {
        self.server_hello.host_boot_id
    }

    pub fn tracked_operation(&self, operation_id: OperationId) -> Option<&TrackedOperation> {
        self.tracked.get(&operation_id)
    }

    /// Execute a command, tracking Accepted receipts as Pending without settlement.
    pub async fn execute_command(
        &mut self,
        envelope: CommandEnvelope,
    ) -> Result<CommandReceipt, IpcError> {
        if envelope.client_id != self.config.client_id {
            return Err(IpcError::Unauthorized);
        }
        let outcome = {
            let connection = self.live_connection_mut()?;
            connection.execute_command(envelope).await
        };
        let receipt = match outcome {
            Ok(receipt) => receipt,
            Err(error) => {
                self.retire_connection();
                return Err(error);
            }
        };
        if let Err(error) = track_accepted_receipt(&mut self.tracked, &receipt) {
            self.retire_connection();
            return Err(error);
        }
        Ok(receipt)
    }

    /// Correlate a fresh OperationStatus query and resolve terminal states locally.
    pub async fn refresh_operation(
        &mut self,
        operation_id: OperationId,
    ) -> Result<Result<OperationState, QueryError>, IpcError> {
        if !self
            .server_hello
            .granted
            .contains(Capability::OperationSettlement)
        {
            return Err(IpcError::UnsupportedCapability);
        }

        let request_id = RequestId::new();
        let client_id = self.config.client_id;
        let outcome = {
            let connection = self.live_connection_mut()?;
            connection
                .query(QueryEnvelope {
                    request_id,
                    client_id,
                    task_id: None,
                    query: Query::OperationStatus { operation_id },
                })
                .await
        };
        let reply = match outcome {
            Ok(reply) => reply,
            Err(error) => {
                self.retire_connection();
                return Err(error);
            }
        };

        match reply.outcome {
            QueryOutcome::Err(error) => Ok(Err(error)),
            QueryOutcome::Ok(result) => {
                let state = match correlate_operation_status(operation_id, result) {
                    Ok(state) => state,
                    Err(error) => {
                        self.retire_connection();
                        return Err(error);
                    }
                };
                if let Err(error) =
                    apply_observed_operation_state(&mut self.tracked, operation_id, &state)
                {
                    self.retire_connection();
                    return Err(error);
                }
                Ok(Ok(state))
            }
        }
    }

    fn live_connection_mut(&mut self) -> Result<&mut ClientConnection, IpcError> {
        self.connection.as_mut().ok_or(IpcError::Unavailable)
    }

    fn retire_connection(&mut self) {
        self.connection = None;
    }
}

/// Record an Accepted receipt. Collision with a different CommandId leaves the map unchanged.
fn track_accepted_receipt(
    tracked: &mut BTreeMap<OperationId, TrackedOperation>,
    receipt: &CommandReceipt,
) -> Result<(), IpcError> {
    let CommandReceipt::Accepted {
        command_id,
        operation_id,
        ..
    } = receipt
    else {
        return Ok(());
    };

    match tracked.get(operation_id) {
        None => {
            tracked.insert(
                *operation_id,
                TrackedOperation::Pending {
                    command_id: *command_id,
                },
            );
            Ok(())
        }
        Some(TrackedOperation::Pending {
            command_id: existing,
        })
        | Some(TrackedOperation::Resolved {
            command_id: existing,
            ..
        }) if existing == command_id => Ok(()),
        Some(_) => Err(IpcError::CorrelationMismatch),
    }
}

/// Validate an OperationStatus query result against the requested OperationId.
fn correlate_operation_status(
    expected: OperationId,
    result: QueryResult,
) -> Result<OperationState, IpcError> {
    match result {
        QueryResult::OperationStatus {
            operation_id,
            state,
        } => {
            if operation_id != expected {
                Err(IpcError::CorrelationMismatch)
            } else {
                Ok(state)
            }
        }
        QueryResult::TaskSnapshot { .. } => Err(IpcError::UnexpectedResponse),
    }
}

/// Apply a correlated observed state with monotonic tracking rules.
/// Untracked ids return Ok without inventing an entry.
fn apply_observed_operation_state(
    tracked: &mut BTreeMap<OperationId, TrackedOperation>,
    operation_id: OperationId,
    state: &OperationState,
) -> Result<(), IpcError> {
    let Some(current) = tracked.get(&operation_id).cloned() else {
        return Ok(());
    };

    match (&current, state) {
        (TrackedOperation::Pending { .. }, OperationState::Accepted) => Ok(()),
        (
            TrackedOperation::Pending { command_id },
            terminal @ (OperationState::Settled { .. }
            | OperationState::Failed { .. }
            | OperationState::Cancelled { .. }
            | OperationState::Uncertain { .. }),
        ) => {
            tracked.insert(
                operation_id,
                TrackedOperation::Resolved {
                    command_id: *command_id,
                    state: terminal.clone(),
                },
            );
            Ok(())
        }
        (
            TrackedOperation::Resolved {
                command_id,
                state: existing,
            },
            observed,
        ) => {
            if existing == observed {
                return Ok(());
            }
            let allowed = matches!(
                (existing, observed),
                (
                    OperationState::Uncertain { .. },
                    OperationState::Settled { .. } | OperationState::Failed { .. }
                )
            );
            if !allowed {
                return Err(IpcError::CorrelationMismatch);
            }
            tracked.insert(
                operation_id,
                TrackedOperation::Resolved {
                    command_id: *command_id,
                    state: observed.clone(),
                },
            );
            Ok(())
        }
    }
}

async fn open_connection(
    config: &HostClientConfig,
    endpoint: &str,
) -> Result<(ClientConnection, ServerHello), IpcError> {
    let fingerprint = profile_fingerprint_for_named_profile(&config.named_profile)?;
    let hello = ClientHello::new(
        config.client_build.clone(),
        config.client_id,
        fingerprint,
        config.requested,
        config.limits,
    )
    .map_err(IpcError::ClientHello)?;
    let connection = connect(endpoint, &hello).await?;
    let server_hello = connection.server_hello().clone();
    Ok((connection, server_hello))
}

#[cfg(test)]
mod tests {
    use super::{
        apply_observed_operation_state, correlate_operation_status, track_accepted_receipt,
        TrackedOperation,
    };
    use crate::domain::command::CommandReceipt;
    use crate::domain::id::{CommandId, EventId, OperationId};
    use crate::domain::operation::{OperationErrorCode, OperationState, OperationUncertaintyCode};
    use crate::domain::query::QueryResult;
    use crate::host::IpcError;
    use std::collections::BTreeMap;

    fn command_id(tail: u8) -> CommandId {
        let mut bytes = [0_u8; 16];
        bytes[0] = 0x01;
        bytes[6] = 0x70;
        bytes[8] = 0x80;
        bytes[15] = tail;
        CommandId::from_bytes(bytes).expect("command id")
    }

    fn operation_id(tail: u8) -> OperationId {
        let mut bytes = [0_u8; 16];
        bytes[0] = 0x01;
        bytes[6] = 0x70;
        bytes[8] = 0x80;
        bytes[15] = tail;
        OperationId::from_bytes(bytes).expect("operation id")
    }

    fn event_id(tail: u8) -> EventId {
        let mut bytes = [0_u8; 16];
        bytes[0] = 0x01;
        bytes[6] = 0x70;
        bytes[8] = 0x80;
        bytes[15] = tail;
        EventId::from_bytes(bytes).expect("event id")
    }

    fn accepted(command: CommandId, operation: OperationId) -> CommandReceipt {
        CommandReceipt::Accepted {
            command_id: command,
            operation_id: operation,
            task_revision: Some(1),
            event_ids: vec![event_id(0x90)],
        }
    }

    fn settled() -> OperationState {
        OperationState::Settled {
            settled_at_ms: 100,
            result_event_ids: vec![event_id(0x91)],
        }
    }

    fn failed() -> OperationState {
        OperationState::Failed {
            settled_at_ms: 200,
            code: OperationErrorCode::SideEffectFailed,
        }
    }

    fn cancelled() -> OperationState {
        OperationState::Cancelled {
            settled_at_ms: 300,
            reason: crate::domain::operation::CancellationReason::Superseded,
        }
    }

    fn uncertain() -> OperationState {
        OperationState::Uncertain {
            observed_at_ms: 150,
            code: OperationUncertaintyCode::AmbiguousDispatch,
        }
    }

    #[test]
    fn duplicate_same_receipt_is_idempotent_and_does_not_regress_resolved() {
        let op = operation_id(0x10);
        let cmd = command_id(0x11);
        let mut tracked = BTreeMap::new();
        track_accepted_receipt(&mut tracked, &accepted(cmd, op)).expect("first");
        assert_eq!(
            tracked.get(&op),
            Some(&TrackedOperation::Pending { command_id: cmd })
        );

        track_accepted_receipt(&mut tracked, &accepted(cmd, op)).expect("duplicate pending");

        tracked.insert(
            op,
            TrackedOperation::Resolved {
                command_id: cmd,
                state: settled(),
            },
        );
        let before = tracked.clone();
        track_accepted_receipt(&mut tracked, &accepted(cmd, op)).expect("duplicate resolved");
        assert_eq!(
            tracked, before,
            "Resolved must not regress on duplicate receipt"
        );
    }

    #[test]
    fn same_operation_different_command_is_rejected_without_mutation() {
        let op = operation_id(0x12);
        let cmd_a = command_id(0x13);
        let cmd_b = command_id(0x14);
        let mut tracked = BTreeMap::new();
        track_accepted_receipt(&mut tracked, &accepted(cmd_a, op)).expect("insert");
        let before = tracked.clone();
        assert!(matches!(
            track_accepted_receipt(&mut tracked, &accepted(cmd_b, op)),
            Err(IpcError::CorrelationMismatch)
        ));
        assert_eq!(tracked, before);
    }

    #[test]
    fn pending_stays_pending_on_accepted_observation() {
        let op = operation_id(0x15);
        let cmd = command_id(0x16);
        let mut tracked = BTreeMap::from([(op, TrackedOperation::Pending { command_id: cmd })]);
        apply_observed_operation_state(&mut tracked, op, &OperationState::Accepted)
            .expect("accepted");
        assert_eq!(
            tracked.get(&op),
            Some(&TrackedOperation::Pending { command_id: cmd })
        );
    }

    #[test]
    fn uncertain_may_advance_to_settled_or_failed() {
        let op = operation_id(0x17);
        let cmd = command_id(0x18);
        let mut tracked = BTreeMap::from([(
            op,
            TrackedOperation::Resolved {
                command_id: cmd,
                state: uncertain(),
            },
        )]);
        apply_observed_operation_state(&mut tracked, op, &settled()).expect("to settled");
        assert_eq!(
            tracked.get(&op),
            Some(&TrackedOperation::Resolved {
                command_id: cmd,
                state: settled(),
            })
        );

        tracked.insert(
            op,
            TrackedOperation::Resolved {
                command_id: cmd,
                state: uncertain(),
            },
        );
        apply_observed_operation_state(&mut tracked, op, &failed()).expect("to failed");
        assert_eq!(
            tracked.get(&op),
            Some(&TrackedOperation::Resolved {
                command_id: cmd,
                state: failed(),
            })
        );

        tracked.insert(
            op,
            TrackedOperation::Resolved {
                command_id: cmd,
                state: uncertain(),
            },
        );
        let before = tracked.clone();
        assert!(matches!(
            apply_observed_operation_state(&mut tracked, op, &cancelled()),
            Err(IpcError::CorrelationMismatch)
        ));
        assert_eq!(tracked, before);
    }

    #[test]
    fn final_contradictory_rewrite_is_rejected_unchanged() {
        let op = operation_id(0x19);
        let cmd = command_id(0x1a);
        let mut tracked = BTreeMap::from([(
            op,
            TrackedOperation::Resolved {
                command_id: cmd,
                state: settled(),
            },
        )]);
        let before = tracked.clone();
        assert!(matches!(
            apply_observed_operation_state(&mut tracked, op, &failed()),
            Err(IpcError::CorrelationMismatch)
        ));
        assert_eq!(tracked, before);

        assert!(matches!(
            apply_observed_operation_state(&mut tracked, op, &OperationState::Accepted),
            Err(IpcError::CorrelationMismatch)
        ));
        assert_eq!(tracked, before);

        tracked.insert(
            op,
            TrackedOperation::Resolved {
                command_id: cmd,
                state: cancelled(),
            },
        );
        let before_cancelled = tracked.clone();
        assert!(matches!(
            apply_observed_operation_state(&mut tracked, op, &settled()),
            Err(IpcError::CorrelationMismatch)
        ));
        assert_eq!(tracked, before_cancelled);

        // Identical final state remains idempotent.
        apply_observed_operation_state(&mut tracked, op, &cancelled()).expect("idempotent");
        assert_eq!(tracked, before_cancelled);

        // Untracked observation returns state path without inventing an entry.
        let foreign = operation_id(0x1b);
        apply_observed_operation_state(&mut tracked, foreign, &settled()).expect("untracked");
        assert!(!tracked.contains_key(&foreign));
    }

    #[test]
    fn inner_operation_id_mismatch_is_rejected() {
        let expected = operation_id(0x1c);
        let other = operation_id(0x1d);
        assert!(matches!(
            correlate_operation_status(
                expected,
                QueryResult::OperationStatus {
                    operation_id: other,
                    state: settled(),
                }
            ),
            Err(IpcError::CorrelationMismatch)
        ));
        assert!(
            correlate_operation_status(
                expected,
                QueryResult::OperationStatus {
                    operation_id: expected,
                    state: settled(),
                }
            )
            .expect("matched")
                == settled()
        );
    }
}
