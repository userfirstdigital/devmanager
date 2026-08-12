//! Reusable HostClient wrapper over the low-level ClientConnection.
//!
//! Tracks accepted OperationIds without inventing settlement. Settlement is
//! observed only through an explicit correlated OperationStatus query.

use std::collections::BTreeMap;

use uuid::Uuid;

use crate::domain::command::{Command, CommandEnvelope, CommandReceipt, ConfirmHostQuitIntent};
use crate::domain::host::HostQuitInspection;
use crate::domain::id::{
    ArtifactId, CommandId, OperationId, RequestId, SnapshotId, SubscriptionId, TaskId,
};
use crate::domain::operation::OperationState;
use crate::domain::query::{Query, QueryEnvelope, QueryError, QueryOutcome, QueryResult};
use crate::domain::snapshot::{
    ArtifactContentPage, EventPage, SnapshotPage, SnapshotSection, TaskSnapshotItem,
};
use crate::domain::ClientId;
use crate::host::{
    pipe_endpoint_for_named_profile, profile_fingerprint_for_named_profile, IpcError,
};
use crate::protocol::{
    Capability, CapabilitySet, ClientHello, DetachAck, DetachRequest, FrameLimits, ServerHello,
};

use super::action::task_show_query;
use super::connection::{connect, ClientConnection, UnsolicitedServerMessage};

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

/// One correlated event-replay page batch from open or continue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventReplayBatch {
    pub subscription_id: SubscriptionId,
    pub page: EventPage,
}

/// One correlated artifact-content page batch from open or continue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactContentBatch {
    pub subscription_id: SubscriptionId,
    pub page: ArtifactContentPage,
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

    /// Host-acknowledged detach: wait for Detached ack, then drop the local connection.
    ///
    /// Returns the acknowledged wire `connection_id`. Does not clear tracked operations.
    /// Without granted [`Capability::ExplicitDetach`], returns
    /// [`IpcError::UnsupportedCapability`] and leaves the connection live.
    pub async fn detach(&mut self) -> Result<Uuid, IpcError> {
        if !self
            .server_hello
            .granted
            .contains(Capability::ExplicitDetach)
        {
            return Err(IpcError::UnsupportedCapability);
        }
        let Some(connection) = self.connection.as_ref() else {
            return Err(IpcError::Unavailable);
        };
        let request = DetachRequest {
            request_id: RequestId::new(),
            client_id: self.config.client_id,
            connection_id: self.server_hello.connection_id,
        };
        let ack = match connection.detach(request).await {
            Ok(ack) => ack,
            Err(error) => {
                self.retire_connection();
                return Err(error);
            }
        };
        finish_detach_after_matching_ack(&mut self.connection, self.server_hello.connection_id, ack)
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

    pub fn server_build(&self) -> &str {
        &self.server_hello.server_build
    }

    pub fn protocol_major(&self) -> u16 {
        self.server_hello.protocol_major
    }

    pub fn protocol_minor(&self) -> u16 {
        self.server_hello.protocol_minor
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
            let connection = self.live_connection()?;
            connection.execute_command(envelope).await
        };
        let receipt = match outcome {
            Ok(receipt) => receipt,
            Err(IpcError::DuplicateInFlight) => return Err(IpcError::DuplicateInFlight),
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

    /// Read one Task snapshot through the shared `task.show` query factory.
    pub async fn task_snapshot(
        &mut self,
        task_id: TaskId,
    ) -> Result<Result<TaskSnapshotItem, QueryError>, IpcError> {
        let request_id = RequestId::new();
        let client_id = self.config.client_id;
        let outcome = {
            let connection = self.live_connection()?;
            connection
                .query(task_show_query(request_id, client_id, task_id))
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
            QueryOutcome::Ok(QueryResult::TaskSnapshot { snapshot })
                if snapshot.task.id == task_id =>
            {
                Ok(Ok(snapshot))
            }
            QueryOutcome::Ok(_) => {
                self.retire_connection();
                Err(IpcError::CorrelationMismatch)
            }
        }
    }

    /// Confirm host quit admission. Requires granted HostShutdown.
    ///
    /// `command_id` is caller-owned and must be retained for exact retries: if the
    /// Accepted response is lost, resubmit the same ID to recover the original
    /// receipt. A fresh ID after Closing is Rejected.
    ///
    /// Accepts a durable drain-intent operation and enters global Closing; does
    /// not drain, settle, release the lock, or exit the host in this slice.
    pub async fn confirm_host_quit(
        &mut self,
        command_id: CommandId,
        inspection_id: u64,
        allow_uninspected_worktrees: bool,
    ) -> Result<CommandReceipt, IpcError> {
        if !self.server_hello.granted.contains(Capability::HostShutdown) {
            return Err(IpcError::UnsupportedCapability);
        }

        let envelope = CommandEnvelope {
            command_id,
            client_id: self.config.client_id,
            task_id: None,
            issued_at_ms: {
                use std::time::{SystemTime, UNIX_EPOCH};
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
                    .unwrap_or(0)
            },
            expected_task_revision: None,
            command: Command::ConfirmHostQuit(ConfirmHostQuitIntent {
                inspection_id,
                allow_uninspected_worktrees,
            }),
        };
        self.execute_command(envelope).await
    }

    /// Inspect durable host-quit blockers. Requires granted HostShutdown.
    ///
    /// Side-effect-free: does not authorize, confirm, begin, or perform quit.
    pub async fn inspect_host_quit(
        &mut self,
    ) -> Result<Result<HostQuitInspection, QueryError>, IpcError> {
        if !self.server_hello.granted.contains(Capability::HostShutdown) {
            return Err(IpcError::UnsupportedCapability);
        }

        let request_id = RequestId::new();
        let client_id = self.config.client_id;
        let outcome = {
            let connection = self.live_connection()?;
            connection
                .query(QueryEnvelope {
                    request_id,
                    client_id,
                    task_id: None,
                    query: Query::InspectHostQuit,
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
            QueryOutcome::Ok(QueryResult::HostQuitInspection { inspection }) => Ok(Ok(inspection)),
            QueryOutcome::Ok(_) => {
                self.retire_connection();
                Err(IpcError::UnexpectedResponse)
            }
        }
    }

    /// Open or resume one paged snapshot section. Requires granted PagedSnapshots.
    pub async fn snapshot_page(
        &mut self,
        section: SnapshotSection,
        snapshot_id: Option<SnapshotId>,
        resume_cursor: Option<Vec<u8>>,
    ) -> Result<Result<SnapshotPage, QueryError>, IpcError> {
        if !self
            .server_hello
            .granted
            .contains(Capability::PagedSnapshots)
        {
            return Err(IpcError::UnsupportedCapability);
        }

        let request_id = RequestId::new();
        let client_id = self.config.client_id;
        let expected_snapshot_id = snapshot_id;
        let outcome = {
            let connection = self.live_connection()?;
            connection
                .query(QueryEnvelope {
                    request_id,
                    client_id,
                    task_id: None,
                    query: Query::SnapshotPage {
                        section,
                        snapshot_id,
                        resume_cursor,
                    },
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
            QueryOutcome::Ok(QueryResult::SnapshotPage { page }) => {
                if page.section != section {
                    self.retire_connection();
                    return Err(IpcError::CorrelationMismatch);
                }
                if let Some(expected) = expected_snapshot_id {
                    if page.snapshot_id != expected {
                        self.retire_connection();
                        return Err(IpcError::CorrelationMismatch);
                    }
                }
                Ok(Ok(page))
            }
            QueryOutcome::Ok(_) => {
                self.retire_connection();
                Err(IpcError::UnexpectedResponse)
            }
        }
    }

    /// Release a retained snapshot. Requires granted PagedSnapshots.
    pub async fn release_snapshot(
        &mut self,
        snapshot_id: SnapshotId,
    ) -> Result<Result<(), QueryError>, IpcError> {
        if !self
            .server_hello
            .granted
            .contains(Capability::PagedSnapshots)
        {
            return Err(IpcError::UnsupportedCapability);
        }

        let request_id = RequestId::new();
        let client_id = self.config.client_id;
        let outcome = {
            let connection = self.live_connection()?;
            connection
                .query(QueryEnvelope {
                    request_id,
                    client_id,
                    task_id: None,
                    query: Query::ReleaseSnapshot { snapshot_id },
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
            QueryOutcome::Ok(QueryResult::SnapshotReleased {
                snapshot_id: released,
            }) if released == snapshot_id => Ok(Ok(())),
            QueryOutcome::Ok(QueryResult::SnapshotReleased { .. }) => {
                self.retire_connection();
                Err(IpcError::CorrelationMismatch)
            }
            QueryOutcome::Ok(_) => {
                self.retire_connection();
                Err(IpcError::UnexpectedResponse)
            }
        }
    }

    /// Open a durable event replay session. Requires granted EventReplay.
    pub async fn open_event_replay(
        &mut self,
        after_sequence: u64,
    ) -> Result<Result<EventReplayBatch, QueryError>, IpcError> {
        if !self.server_hello.granted.contains(Capability::EventReplay) {
            return Err(IpcError::UnsupportedCapability);
        }

        let request_id = RequestId::new();
        let client_id = self.config.client_id;
        let outcome = {
            let connection = self.live_connection()?;
            connection
                .query(QueryEnvelope {
                    request_id,
                    client_id,
                    task_id: None,
                    query: Query::OpenEventReplay { after_sequence },
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
            QueryOutcome::Ok(QueryResult::EventReplayPage {
                subscription_id,
                page,
            }) => {
                if page.after_sequence != after_sequence {
                    self.retire_connection();
                    return Err(IpcError::CorrelationMismatch);
                }
                Ok(Ok(EventReplayBatch {
                    subscription_id,
                    page,
                }))
            }
            QueryOutcome::Ok(_) => {
                self.retire_connection();
                Err(IpcError::UnexpectedResponse)
            }
        }
    }

    /// Continue a retained event replay. Requires granted EventReplay.
    pub async fn continue_event_replay(
        &mut self,
        subscription_id: SubscriptionId,
        resume_cursor: Vec<u8>,
    ) -> Result<Result<EventReplayBatch, QueryError>, IpcError> {
        if !self.server_hello.granted.contains(Capability::EventReplay) {
            return Err(IpcError::UnsupportedCapability);
        }

        let request_id = RequestId::new();
        let client_id = self.config.client_id;
        let outcome = {
            let connection = self.live_connection()?;
            connection
                .query(QueryEnvelope {
                    request_id,
                    client_id,
                    task_id: None,
                    query: Query::ContinueEventReplay {
                        subscription_id,
                        resume_cursor,
                    },
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
            QueryOutcome::Ok(QueryResult::EventReplayPage {
                subscription_id: returned,
                page,
            }) => {
                if returned != subscription_id {
                    self.retire_connection();
                    return Err(IpcError::CorrelationMismatch);
                }
                Ok(Ok(EventReplayBatch {
                    subscription_id: returned,
                    page,
                }))
            }
            QueryOutcome::Ok(_) => {
                self.retire_connection();
                Err(IpcError::UnexpectedResponse)
            }
        }
    }

    /// Release a retained event replay. Requires granted EventReplay.
    pub async fn release_event_replay(
        &mut self,
        subscription_id: SubscriptionId,
    ) -> Result<Result<(), QueryError>, IpcError> {
        if !self.server_hello.granted.contains(Capability::EventReplay) {
            return Err(IpcError::UnsupportedCapability);
        }

        let request_id = RequestId::new();
        let client_id = self.config.client_id;
        let outcome = {
            let connection = self.live_connection()?;
            connection
                .query(QueryEnvelope {
                    request_id,
                    client_id,
                    task_id: None,
                    query: Query::ReleaseEventReplay { subscription_id },
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
            QueryOutcome::Ok(QueryResult::EventReplayReleased {
                subscription_id: released,
            }) if released == subscription_id => Ok(Ok(())),
            QueryOutcome::Ok(QueryResult::EventReplayReleased { .. }) => {
                self.retire_connection();
                Err(IpcError::CorrelationMismatch)
            }
            QueryOutcome::Ok(_) => {
                self.retire_connection();
                Err(IpcError::UnexpectedResponse)
            }
        }
    }

    /// Open an artifact content session. Requires granted ChunkResume.
    pub async fn open_artifact_content(
        &mut self,
        task_id: TaskId,
        artifact_id: ArtifactId,
    ) -> Result<Result<ArtifactContentBatch, QueryError>, IpcError> {
        if !self.server_hello.granted.contains(Capability::ChunkResume) {
            return Err(IpcError::UnsupportedCapability);
        }

        let request_id = RequestId::new();
        let client_id = self.config.client_id;
        let outcome = {
            let connection = self.live_connection()?;
            connection
                .query(QueryEnvelope {
                    request_id,
                    client_id,
                    task_id: Some(task_id),
                    query: Query::OpenArtifactContent { artifact_id },
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
            QueryOutcome::Ok(QueryResult::ArtifactContentPage {
                subscription_id,
                page,
            }) => {
                if page.artifact_id != artifact_id {
                    self.retire_connection();
                    return Err(IpcError::CorrelationMismatch);
                }
                Ok(Ok(ArtifactContentBatch {
                    subscription_id,
                    page,
                }))
            }
            QueryOutcome::Ok(_) => {
                self.retire_connection();
                Err(IpcError::UnexpectedResponse)
            }
        }
    }

    /// Continue a retained artifact content session. Requires granted ChunkResume.
    pub async fn continue_artifact_content(
        &mut self,
        task_id: TaskId,
        subscription_id: SubscriptionId,
        resume_cursor: Vec<u8>,
    ) -> Result<Result<ArtifactContentBatch, QueryError>, IpcError> {
        if !self.server_hello.granted.contains(Capability::ChunkResume) {
            return Err(IpcError::UnsupportedCapability);
        }

        let request_id = RequestId::new();
        let client_id = self.config.client_id;
        let outcome = {
            let connection = self.live_connection()?;
            connection
                .query(QueryEnvelope {
                    request_id,
                    client_id,
                    task_id: Some(task_id),
                    query: Query::ContinueArtifactContent {
                        subscription_id,
                        resume_cursor,
                    },
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
            QueryOutcome::Ok(QueryResult::ArtifactContentPage {
                subscription_id: returned,
                page,
            }) => {
                if returned != subscription_id {
                    self.retire_connection();
                    return Err(IpcError::CorrelationMismatch);
                }
                Ok(Ok(ArtifactContentBatch {
                    subscription_id: returned,
                    page,
                }))
            }
            QueryOutcome::Ok(_) => {
                self.retire_connection();
                Err(IpcError::UnexpectedResponse)
            }
        }
    }

    /// Release a retained artifact content session. Requires granted ChunkResume.
    pub async fn release_artifact_content(
        &mut self,
        task_id: TaskId,
        subscription_id: SubscriptionId,
    ) -> Result<Result<(), QueryError>, IpcError> {
        if !self.server_hello.granted.contains(Capability::ChunkResume) {
            return Err(IpcError::UnsupportedCapability);
        }

        let request_id = RequestId::new();
        let client_id = self.config.client_id;
        let outcome = {
            let connection = self.live_connection()?;
            connection
                .query(QueryEnvelope {
                    request_id,
                    client_id,
                    task_id: Some(task_id),
                    query: Query::ReleaseArtifactContent { subscription_id },
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
            QueryOutcome::Ok(QueryResult::ArtifactContentReleased {
                subscription_id: released,
            }) if released == subscription_id => Ok(Ok(())),
            QueryOutcome::Ok(QueryResult::ArtifactContentReleased { .. }) => {
                self.retire_connection();
                Err(IpcError::CorrelationMismatch)
            }
            QueryOutcome::Ok(_) => {
                self.retire_connection();
                Err(IpcError::UnexpectedResponse)
            }
        }
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
            let connection = self.live_connection()?;
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

    /// Receive the next unsolicited durable/resync message from the live connection.
    ///
    /// Live events may arrive before a correlated open reply; this exposes the
    /// existing connection inbox without inventing a second queue.
    pub async fn recv_unsolicited(&self) -> Result<UnsolicitedServerMessage, IpcError> {
        self.live_connection()?.recv_unsolicited().await
    }

    fn live_connection(&self) -> Result<&ClientConnection, IpcError> {
        self.connection.as_ref().ok_or(IpcError::Unavailable)
    }

    fn retire_connection(&mut self) {
        self.connection = None;
    }

    #[cfg(test)]
    fn from_parts_for_test(
        config: HostClientConfig,
        server_hello: ServerHello,
        connection: Option<ClientConnection>,
        tracked: BTreeMap<OperationId, TrackedOperation>,
    ) -> Self {
        Self {
            config,
            endpoint: String::new(),
            connection,
            server_hello,
            tracked,
        }
    }

    #[cfg(test)]
    fn tracked(&self) -> &BTreeMap<OperationId, TrackedOperation> {
        &self.tracked
    }

    #[cfg(test)]
    fn attached_connection(&self) -> Option<&ClientConnection> {
        self.connection.as_ref()
    }
}

fn finish_detach_after_matching_ack(
    connection: &mut Option<ClientConnection>,
    expected_connection_id: Uuid,
    ack: DetachAck,
) -> Result<Uuid, IpcError> {
    if ack.connection_id != expected_connection_id {
        *connection = None;
        return Err(IpcError::CorrelationMismatch);
    }
    let connection_id = ack.connection_id;
    *connection = None;
    Ok(connection_id)
}

/// Record an Accepted receipt. Collision with a different CommandId leaves the map unchanged.
pub(crate) fn track_accepted_receipt(
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
        QueryResult::TaskSnapshot { .. }
        | QueryResult::SnapshotPage { .. }
        | QueryResult::SnapshotReleased { .. }
        | QueryResult::EventReplayPage { .. }
        | QueryResult::EventReplayReleased { .. }
        | QueryResult::ArtifactContentPage { .. }
        | QueryResult::ArtifactContentReleased { .. }
        | QueryResult::HostQuitInspection { .. } => Err(IpcError::UnexpectedResponse),
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
    let server_hello = connection.server_hello();
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

    fn test_server_hello(
        granted: crate::protocol::CapabilitySet,
        connection_id: uuid::Uuid,
    ) -> crate::protocol::ServerHello {
        use crate::protocol::{FrameLimits, ProfileFingerprint, PROTOCOL_MAJOR, PROTOCOL_MINOR};
        crate::protocol::ServerHello {
            protocol_major: PROTOCOL_MAJOR,
            protocol_minor: PROTOCOL_MINOR,
            server_build: "devmanager-host/test".into(),
            host_boot_id: uuid::Uuid::from_bytes([
                0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0xb0,
            ]),
            connection_id,
            profile_fingerprint: ProfileFingerprint::hash_normalized("detach-unit"),
            granted,
            limits: FrameLimits::v1_default(),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn detach_without_capability_keeps_connection_and_tracked_ops() {
        use super::{HostClient, HostClientConfig};
        use crate::client::connection::ClientConnection;
        use crate::domain::ClientId;
        use crate::protocol::{Capability, CapabilitySet, FrameLimits};

        let client_id = ClientId::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xb1,
        ])
        .expect("client");
        let connection_id = uuid::Uuid::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xb2,
        ]);
        let hello = test_server_hello(
            CapabilitySet::from_capabilities([Capability::ChunkResume]),
            connection_id,
        );
        let op = operation_id(0xb3);
        let cmd = command_id(0xb4);
        let mut tracked = BTreeMap::from([(op, TrackedOperation::Pending { command_id: cmd })]);
        let before = tracked.clone();
        let stub = ClientConnection::inert_stub_for_test(
            client_id,
            test_server_hello(
                CapabilitySet::from_capabilities([Capability::ChunkResume]),
                connection_id,
            ),
        );
        let mut client = HostClient::from_parts_for_test(
            HostClientConfig {
                named_profile: "detach-unit".into(),
                client_build: "devmanager/test".into(),
                client_id,
                requested: CapabilitySet::from_capabilities([Capability::ChunkResume]),
                limits: FrameLimits::v1_default(),
            },
            hello,
            Some(stub.clone()),
            std::mem::take(&mut tracked),
        );
        assert!(matches!(
            client.detach().await,
            Err(IpcError::UnsupportedCapability)
        ));
        assert!(client.is_connected());
        assert_eq!(client.tracked(), &before);
        let still = client
            .attached_connection()
            .expect("pre-I/O unsupported detach must keep the connection attached");
        assert!(
            still.shares_state_with(&stub),
            "exact same ClientConnection must remain attached"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn inspect_host_quit_without_capability_keeps_connection_and_tracked_ops() {
        use super::{HostClient, HostClientConfig};
        use crate::client::connection::ClientConnection;
        use crate::domain::ClientId;
        use crate::protocol::{Capability, CapabilitySet, FrameLimits};

        let client_id = ClientId::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xc1,
        ])
        .expect("client");
        let connection_id = uuid::Uuid::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xc2,
        ]);
        let hello = test_server_hello(
            CapabilitySet::from_capabilities([Capability::ChunkResume]),
            connection_id,
        );
        let op = operation_id(0xc3);
        let cmd = command_id(0xc4);
        let mut tracked = BTreeMap::from([(op, TrackedOperation::Pending { command_id: cmd })]);
        let before = tracked.clone();
        let stub = ClientConnection::inert_stub_for_test(
            client_id,
            test_server_hello(
                CapabilitySet::from_capabilities([Capability::ChunkResume]),
                connection_id,
            ),
        );
        let mut client = HostClient::from_parts_for_test(
            HostClientConfig {
                named_profile: "inspect-quit-unit".into(),
                client_build: "devmanager/test".into(),
                client_id,
                requested: CapabilitySet::from_capabilities([Capability::ChunkResume]),
                limits: FrameLimits::v1_default(),
            },
            hello,
            Some(stub.clone()),
            std::mem::take(&mut tracked),
        );
        assert!(matches!(
            client.inspect_host_quit().await,
            Err(IpcError::UnsupportedCapability)
        ));
        assert!(client.is_connected());
        assert_eq!(client.tracked(), &before);
        let still = client
            .attached_connection()
            .expect("pre-I/O unsupported inspect must keep the connection attached");
        assert!(
            still.shares_state_with(&stub),
            "exact same ClientConnection must remain attached"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn confirm_host_quit_without_capability_keeps_connection_and_tracked_ops() {
        use super::{HostClient, HostClientConfig};
        use crate::client::connection::ClientConnection;
        use crate::domain::ClientId;
        use crate::protocol::{Capability, CapabilitySet, FrameLimits};

        let client_id = ClientId::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xd1,
        ])
        .expect("client");
        let connection_id = uuid::Uuid::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xd2,
        ]);
        let hello = test_server_hello(
            CapabilitySet::from_capabilities([Capability::ChunkResume]),
            connection_id,
        );
        let op = operation_id(0xd3);
        let cmd = command_id(0xd4);
        let mut tracked = BTreeMap::from([(op, TrackedOperation::Pending { command_id: cmd })]);
        let before = tracked.clone();
        let stub = ClientConnection::inert_stub_for_test(
            client_id,
            test_server_hello(
                CapabilitySet::from_capabilities([Capability::ChunkResume]),
                connection_id,
            ),
        );
        let mut client = HostClient::from_parts_for_test(
            HostClientConfig {
                named_profile: "confirm-quit-unit".into(),
                client_build: "devmanager/test".into(),
                client_id,
                requested: CapabilitySet::from_capabilities([Capability::ChunkResume]),
                limits: FrameLimits::v1_default(),
            },
            hello,
            Some(stub.clone()),
            std::mem::take(&mut tracked),
        );
        assert!(matches!(
            client.confirm_host_quit(command_id(0xd5), 0, true).await,
            Err(IpcError::UnsupportedCapability)
        ));
        assert!(client.is_connected());
        assert_eq!(client.tracked(), &before);
        let still = client
            .attached_connection()
            .expect("pre-I/O unsupported confirm must keep the connection attached");
        assert!(
            still.shares_state_with(&stub),
            "exact same ClientConnection must remain attached"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn detach_matching_ack_drops_only_connection_keeps_tracked() {
        use super::{HostClient, HostClientConfig};
        use crate::client::connection::{ClientConnection, ScriptedDetachBehavior};
        use crate::domain::ClientId;
        use crate::protocol::{Capability, CapabilitySet, FrameLimits};

        let client_id = ClientId::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xb5,
        ])
        .expect("client");
        let connection_id = uuid::Uuid::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xb6,
        ]);
        let hello = test_server_hello(
            CapabilitySet::from_capabilities([Capability::ExplicitDetach]),
            connection_id,
        );
        let op = operation_id(0xb7);
        let cmd = command_id(0xb8);
        let tracked = BTreeMap::from([(op, TrackedOperation::Pending { command_id: cmd })]);
        let mut client = HostClient::from_parts_for_test(
            HostClientConfig {
                named_profile: "detach-unit".into(),
                client_build: "devmanager/test".into(),
                client_id,
                requested: CapabilitySet::from_capabilities([Capability::ExplicitDetach]),
                limits: FrameLimits::v1_default(),
            },
            hello.clone(),
            Some(ClientConnection::scripted_for_test(
                client_id,
                hello,
                ScriptedDetachBehavior::MatchingAck,
            )),
            tracked.clone(),
        );
        assert_eq!(client.detach().await.expect("detach"), connection_id);
        assert!(!client.is_connected());
        assert_eq!(client.tracked(), &tracked);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn detach_correlation_mismatch_retires_connection_keeps_tracked() {
        use super::{HostClient, HostClientConfig};
        use crate::client::connection::{ClientConnection, ScriptedDetachBehavior};
        use crate::domain::ClientId;
        use crate::protocol::{Capability, CapabilitySet, FrameLimits};

        let client_id = ClientId::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xba,
        ])
        .expect("client");
        let connection_id = uuid::Uuid::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xbb,
        ]);
        let hello = test_server_hello(
            CapabilitySet::from_capabilities([Capability::ExplicitDetach]),
            connection_id,
        );
        let op = operation_id(0xbc);
        let cmd = command_id(0xbd);
        let tracked = BTreeMap::from([(op, TrackedOperation::Pending { command_id: cmd })]);
        let mut client = HostClient::from_parts_for_test(
            HostClientConfig {
                named_profile: "detach-unit".into(),
                client_build: "devmanager/test".into(),
                client_id,
                requested: CapabilitySet::from_capabilities([Capability::ExplicitDetach]),
                limits: FrameLimits::v1_default(),
            },
            hello.clone(),
            Some(ClientConnection::scripted_for_test(
                client_id,
                hello,
                ScriptedDetachBehavior::WrongConnectionAck,
            )),
            tracked.clone(),
        );
        assert!(matches!(
            client.detach().await,
            Err(IpcError::CorrelationMismatch)
        ));
        assert!(!client.is_connected());
        assert_eq!(client.tracked(), &tracked);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn detach_transport_failure_retires_connection_keeps_tracked() {
        use super::{HostClient, HostClientConfig};
        use crate::client::connection::{ClientConnection, ScriptedDetachBehavior};
        use crate::domain::ClientId;
        use crate::protocol::{Capability, CapabilitySet, FrameLimits};

        let client_id = ClientId::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xbe,
        ])
        .expect("client");
        let connection_id = uuid::Uuid::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xbf,
        ]);
        let hello = test_server_hello(
            CapabilitySet::from_capabilities([Capability::ExplicitDetach]),
            connection_id,
        );
        let op = operation_id(0xc0);
        let cmd = command_id(0xc1);
        let tracked = BTreeMap::from([(op, TrackedOperation::Pending { command_id: cmd })]);
        let mut client = HostClient::from_parts_for_test(
            HostClientConfig {
                named_profile: "detach-unit".into(),
                client_build: "devmanager/test".into(),
                client_id,
                requested: CapabilitySet::from_capabilities([Capability::ExplicitDetach]),
                limits: FrameLimits::v1_default(),
            },
            hello.clone(),
            Some(ClientConnection::scripted_for_test(
                client_id,
                hello,
                ScriptedDetachBehavior::ClosedWriteQueue,
            )),
            tracked.clone(),
        );
        assert!(matches!(client.detach().await, Err(IpcError::Unavailable)));
        assert!(!client.is_connected());
        assert_eq!(client.tracked(), &tracked);
    }
}
