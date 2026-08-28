//! Reusable HostClient wrapper over the low-level ClientConnection.
//!
//! Tracks accepted OperationIds without inventing settlement. Settlement is
//! observed only through an explicit correlated OperationStatus query.

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use uuid::Uuid;

use crate::domain::cockpit::{AgentConnectionSnapshot, ConfigSidebarSnapshot, TaskCockpitQuery};
use crate::domain::command::{Command, CommandEnvelope, CommandReceipt, PrepareUpdateIntent};
use crate::domain::host::HostQuitInspection;
use crate::domain::id::{
    ArtifactId, CommandId, OperationId, RequestId, SnapshotId, SubscriptionId, TaskId,
};
use crate::domain::operation::OperationState;
use crate::domain::query::{
    Query, QueryEnvelope, QueryError, QueryOutcome, QueryReply, QueryResult,
};
use crate::domain::snapshot::{
    ArtifactContentPage, EventPage, SnapshotPage, SnapshotSection, TaskSnapshotItem,
};
use crate::domain::ClientId;
use crate::host::{
    pipe_endpoint_for_named_profile, profile_fingerprint_for_named_profile, IpcError,
};
use crate::prompts::projection::{PromptLibraryQuery, PromptProjectionReply};
use crate::protocol::{
    Capability, CapabilitySet, ClientHello, DetachAck, DetachRequest, FrameLimits, ReconnectGrant,
    ServerHello,
};
use crate::terminal::protocol::{InputAck, TerminalInputRequest};
use crate::updater::UpdateHandoffToken;

use super::connect_client::{connect_authenticated_halves, ConnectClientConfig};
use super::connection::{connect, ClientConnection, ConnectionMetadata, UnsolicitedServerMessage};
use super::inbox_controller::{InboxTransport, InboxTransportFuture};
use super::port::AsyncHostRequestPort;
use super::subscription::{ClientSubscription, SubscriptionError, SubscriptionUpdate};
use super::typed_queries;

use crate::connect::ConnectNoiseCustody;
use futures_util::{Sink, Stream};
use tokio_tungstenite::tungstenite::protocol::Message as WsMessage;

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
    transport: HostClientTransport,
    connection: Option<ClientConnection>,
    metadata: ConnectionMetadata,
    tracked: BTreeMap<OperationId, TrackedOperation>,
}

enum HostClientTransport {
    Local {
        config: HostClientConfig,
        endpoint: String,
        reconnect_grant: Option<ReconnectGrant>,
    },
    Connect {
        config: ConnectClientConfig,
    },
}

impl HostClient {
    /// Validate the named profile, build ClientHello, and connect.
    pub async fn connect(config: HostClientConfig) -> Result<Self, IpcError> {
        let endpoint = pipe_endpoint_for_named_profile(&config.named_profile)?;
        let (connection, metadata) = open_local_connection(&config, &endpoint, None).await?;
        let reconnect_grant = metadata.reconnect_grant();
        Ok(Self {
            transport: HostClientTransport::Local {
                config,
                endpoint,
                reconnect_grant,
            },
            connection: Some(connection),
            metadata,
            tracked: BTreeMap::new(),
        })
    }

    /// Connect over a caller-authenticated Connect transport (Noise + Hello).
    ///
    /// The opener supplies already-verified socket halves (TLS/cookie owned by
    /// the fleet route). Device custody is caller-owned and not retained here.
    pub async fn connect_connect<Si, St>(
        config: ConnectClientConfig,
        custody: &ConnectNoiseCustody,
        sink: Si,
        stream: St,
    ) -> Result<Self, IpcError>
    where
        Si: Sink<WsMessage> + Unpin + Send + 'static,
        Si::Error: std::fmt::Display,
        St: Stream<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>>
            + Unpin
            + Send
            + 'static,
    {
        let connection = connect_authenticated_halves(sink, stream, custody, &config).await?;
        let metadata = connection.metadata();
        // Assigned Connect client id is authoritative for subsequent queries.
        let mut config = config;
        if let Some(session) = metadata.as_connect() {
            config.requested_client_id = Some(session.assigned_client_id());
        }
        Ok(Self {
            transport: HostClientTransport::Connect { config },
            connection: Some(connection),
            metadata,
            tracked: BTreeMap::new(),
        })
    }

    /// Drop any prior connection, then rebuild Hello from the same config/client_id.
    /// A failed attempt leaves the client disconnected while preserving tracking.
    ///
    /// Local reconnect is self-contained. Connect reconnect requires
    /// [`Self::reconnect_connect`] with a fresh authenticated socket and the
    /// same caller-owned pin/custody (never auto-trust).
    pub async fn reconnect(&mut self) -> Result<(), IpcError> {
        match &self.transport {
            HostClientTransport::Local {
                config,
                endpoint,
                reconnect_grant,
            } => {
                self.connection = None;
                match open_local_connection(config, endpoint, reconnect_grant.clone()).await {
                    Ok((connection, metadata)) => {
                        if let HostClientTransport::Local {
                            reconnect_grant, ..
                        } = &mut self.transport
                        {
                            *reconnect_grant = metadata.reconnect_grant();
                        }
                        self.connection = Some(connection);
                        self.metadata = metadata;
                        Ok(())
                    }
                    Err(error) => Err(error),
                }
            }
            HostClientTransport::Connect { .. } => Err(IpcError::Unsupported),
        }
    }

    /// Reconnect an authenticated Connect session over a fresh socket.
    ///
    /// The host key pin in `config` must match the prior session pin exactly;
    /// this method never rewrites or auto-trusts a new pin.
    pub async fn reconnect_connect<Si, St>(
        &mut self,
        custody: &ConnectNoiseCustody,
        sink: Si,
        stream: St,
    ) -> Result<(), IpcError>
    where
        Si: Sink<WsMessage> + Unpin + Send + 'static,
        Si::Error: std::fmt::Display,
        St: Stream<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>>
            + Unpin
            + Send
            + 'static,
    {
        let HostClientTransport::Connect { config } = &self.transport else {
            return Err(IpcError::Unsupported);
        };
        if let Some(prior) = self.metadata.as_connect() {
            if prior.host_key_pin() != config.host_key_pin
                || prior.host_public_id() != config.expected_host_public_id
            {
                return Err(IpcError::Unauthorized);
            }
        }
        self.connection = None;
        let connection = connect_authenticated_halves(sink, stream, custody, config).await?;
        let metadata = connection.metadata();
        if let Some(session) = metadata.as_connect() {
            if let HostClientTransport::Connect { config } = &mut self.transport {
                if config.host_key_pin != session.host_key_pin() {
                    return Err(IpcError::Unauthorized);
                }
                config.requested_client_id = Some(session.assigned_client_id());
            }
        }
        self.connection = Some(connection);
        self.metadata = metadata;
        Ok(())
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
    /// Connect transport does not carry Detach payloads (`IpcError::Unsupported`).
    pub async fn detach(&mut self) -> Result<Uuid, IpcError> {
        if matches!(self.transport, HostClientTransport::Connect { .. }) {
            return Err(IpcError::Unsupported);
        }
        if !self
            .metadata
            .granted_capabilities()
            .contains(Capability::ExplicitDetach)
        {
            return Err(IpcError::UnsupportedCapability);
        }
        let Some(connection) = self.connection.as_ref() else {
            return Err(IpcError::Unavailable);
        };
        let client_id = self.client_id();
        let connection_id = self.metadata.connection_id();
        let request = DetachRequest {
            request_id: RequestId::new(),
            client_id,
            connection_id,
        };
        let ack = match connection.detach(request).await {
            Ok(ack) => ack,
            Err(error) => {
                self.retire_connection();
                return Err(error);
            }
        };
        finish_detach_after_matching_ack(&mut self.connection, connection_id, ack)
    }

    pub fn is_connected(&self) -> bool {
        self.connection
            .as_ref()
            .is_some_and(ClientConnection::is_live)
    }

    pub fn client_id(&self) -> ClientId {
        match &self.transport {
            HostClientTransport::Local { config, .. } => config.client_id,
            HostClientTransport::Connect { config } => config
                .requested_client_id
                .or_else(|| {
                    self.metadata
                        .as_connect()
                        .map(|session| session.assigned_client_id())
                })
                .expect("Connect HostClient always has an assigned client id after Hello"),
        }
    }

    pub fn endpoint(&self) -> &str {
        match &self.transport {
            HostClientTransport::Local { endpoint, .. } => endpoint.as_str(),
            HostClientTransport::Connect { .. } => "connect",
        }
    }

    pub fn metadata(&self) -> &ConnectionMetadata {
        &self.metadata
    }

    pub fn granted_capabilities(&self) -> CapabilitySet {
        self.metadata.granted_capabilities()
    }

    pub fn connection_id(&self) -> Uuid {
        self.metadata.connection_id()
    }

    /// Local pipe Hello carries a host boot id. Connect sessions have none.
    pub fn host_boot_id(&self) -> Option<Uuid> {
        self.metadata.host_boot_id()
    }

    pub fn server_build(&self) -> &str {
        self.metadata.server_build()
    }

    pub fn protocol_major(&self) -> u16 {
        self.metadata.protocol_major()
    }

    pub fn protocol_minor(&self) -> u16 {
        self.metadata.protocol_minor()
    }

    pub fn tracked_operation(&self, operation_id: OperationId) -> Option<&TrackedOperation> {
        self.tracked.get(&operation_id)
    }

    /// Move accepted-operation tracking from `source` into `self` without
    /// inventing command IDs or touching either transport.
    ///
    /// Used by fleet reconnect replacement so prior Pending/Resolved facts stay
    /// under the surviving owner identity. Collision with a different CommandId
    /// for the same OperationId fails closed and leaves both maps unchanged.
    pub fn absorb_tracked_operations(&mut self, source: &mut Self) -> Result<(), IpcError> {
        for (operation_id, incoming) in &source.tracked {
            if let Some(existing) = self.tracked.get(operation_id) {
                let existing_command = match existing {
                    TrackedOperation::Pending { command_id }
                    | TrackedOperation::Resolved { command_id, .. } => *command_id,
                };
                let incoming_command = match incoming {
                    TrackedOperation::Pending { command_id }
                    | TrackedOperation::Resolved { command_id, .. } => *command_id,
                };
                if existing_command != incoming_command {
                    return Err(IpcError::CorrelationMismatch);
                }
            }
        }
        let stolen = std::mem::take(&mut source.tracked);
        for (operation_id, incoming) in stolen {
            match self.tracked.get(&operation_id) {
                None => {
                    self.tracked.insert(operation_id, incoming);
                }
                Some(TrackedOperation::Resolved { .. }) => {
                    // Never downgrade Resolved → Pending on same-command collision.
                    // Destination settlement stays authoritative.
                }
                Some(TrackedOperation::Pending { .. }) => match &incoming {
                    TrackedOperation::Resolved { .. } => {
                        self.tracked.insert(operation_id, incoming);
                    }
                    TrackedOperation::Pending { .. } => {
                        // Same CommandId Pending collision: keep destination.
                    }
                },
            }
        }
        Ok(())
    }

    /// Execute a command, tracking Accepted receipts as Pending without settlement.
    pub async fn execute_command(
        &mut self,
        envelope: CommandEnvelope,
    ) -> Result<CommandReceipt, IpcError> {
        if envelope.client_id != self.client_id() {
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

    /// Execute a command while the caller-driven unsolicited receiver is
    /// waiting on the same authenticated connection. The duplex transport
    /// correlates command replies independently from unsolicited events, so a
    /// shared immutable connection borrow is sufficient here. The ordinary
    /// `execute_command` path remains the tracked-operation authority; this
    /// concurrent lane returns the real receipt and lets its owner decide how
    /// to fence/settle the UI action.
    pub async fn execute_command_concurrent(
        &self,
        envelope: CommandEnvelope,
    ) -> Result<CommandReceipt, IpcError> {
        if envelope.client_id != self.client_id() {
            return Err(IpcError::Unauthorized);
        }
        self.live_connection()?.execute_command(envelope).await
    }

    /// Forward one fully-fenced terminal input request to the host-owned
    /// terminal service. The caller must provide the exact task/session,
    /// generation, focus/action epoch, and monotonic input sequence.
    pub async fn execute_terminal_input(
        &self,
        request: TerminalInputRequest,
    ) -> Result<InputAck, IpcError> {
        if request.client_id != self.client_id() {
            return Err(IpcError::Unauthorized);
        }
        if matches!(self.transport, HostClientTransport::Connect { .. }) {
            return Err(IpcError::Unsupported);
        }
        if !self
            .metadata
            .granted_capabilities()
            .contains(Capability::ProviderInput)
        {
            return Err(IpcError::UnsupportedCapability);
        }
        self.live_connection()?
            .execute_terminal_input(request)
            .await
    }

    /// Ask the authenticated host to prepare one update handoff on this exact
    /// connection and return the host-issued token. The caller owns the
    /// command id so an unknown delivery outcome can retry the same request;
    /// this method never creates a second HostClient or nests a client lock
    /// across the await.
    pub async fn prepare_update(
        &mut self,
        command_id: CommandId,
        target_version: &str,
        client_build: &str,
        host_build: &str,
        allow_explicit_confirm_with_active: bool,
    ) -> Result<UpdateHandoffToken, IpcError> {
        if matches!(self.transport, HostClientTransport::Connect { .. }) {
            return Err(IpcError::Unsupported);
        }
        if !self
            .metadata
            .granted_capabilities()
            .contains(Capability::HostShutdown)
            || !self
                .metadata
                .granted_capabilities()
                .contains(Capability::UpdateHandoff)
        {
            return Err(IpcError::UnsupportedCapability);
        }

        let envelope = CommandEnvelope {
            command_id,
            client_id: self.client_id(),
            task_id: None,
            issued_at_ms: unix_time_ms(),
            expected_task_revision: None,
            command: Command::PrepareUpdate(PrepareUpdateIntent {
                target_version: target_version.to_string(),
                client_build: client_build.to_string(),
                host_build: host_build.to_string(),
                allow_explicit_confirm_with_active,
            }),
        };
        let outcome = {
            let connection = self.live_connection()?;
            connection.execute_update_handoff(envelope).await
        };
        let reply = match outcome {
            Ok(reply) => reply,
            Err(error) => {
                // The host retains the prepared token under command_id. Retire
                // this exact connection so a later retry cannot share a
                // poisoned epoch or stale inbound tail.
                self.retire_connection();
                return Err(error);
            }
        };
        let token = reply.token;
        if reply.command_id != command_id
            || self.host_boot_id() != Some(token.host_boot_id)
            || token.host_boot_id == Uuid::nil()
            || token.target_version != target_version
            || token.client_build != client_build
            || token.host_build != host_build
        {
            self.retire_connection();
            return Err(IpcError::CorrelationMismatch);
        }
        Ok(token)
    }

    /// Execute an arbitrary query while preserving the HostClient connection
    /// lifecycle and the exact caller-owned request id.
    pub async fn query(&mut self, envelope: QueryEnvelope) -> Result<QueryReply, IpcError> {
        self.query_with_timeout(envelope, None).await
    }

    /// Query with an optional completion deadline. `None` uses the connection
    /// default; transport errors retire the exact connection before returning.
    pub async fn query_with_timeout(
        &mut self,
        envelope: QueryEnvelope,
        timeout: Option<Duration>,
    ) -> Result<QueryReply, IpcError> {
        AsyncHostRequestPort::request_query(self, envelope, timeout).await
    }

    /// Read one Task snapshot through the shared `task.show` query factory.
    pub async fn task_snapshot(
        &mut self,
        task_id: TaskId,
    ) -> Result<Result<TaskSnapshotItem, QueryError>, IpcError> {
        typed_queries::task_snapshot(self, task_id).await
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
        typed_queries::confirm_host_quit(
            self,
            command_id,
            inspection_id,
            allow_uninspected_worktrees,
        )
        .await
    }

    /// Inspect durable host-quit blockers. Requires granted HostShutdown.
    ///
    /// Side-effect-free: does not authorize, confirm, begin, or perform quit.
    pub async fn inspect_host_quit(
        &mut self,
    ) -> Result<Result<HostQuitInspection, QueryError>, IpcError> {
        typed_queries::inspect_host_quit(self).await
    }

    /// Page the personal prompt library. Requires the active Hello
    /// [`Capability::PromptProjection`] grant; continuation cursors remint from
    /// that same live grant rather than a cached bool.
    pub async fn query_prompt_library(
        &mut self,
        query: PromptLibraryQuery,
    ) -> Result<Result<PromptProjectionReply, QueryError>, IpcError> {
        typed_queries::query_prompt_library(self, query).await
    }

    /// Query one Task Cockpit surface. Requires granted TaskCockpit and an
    /// exact selected Task identity in the envelope.
    pub async fn query_task_cockpit(
        &mut self,
        task_id: TaskId,
        query: TaskCockpitQuery,
    ) -> Result<Result<crate::domain::TaskCockpitResult, QueryError>, IpcError> {
        typed_queries::query_task_cockpit(self, task_id, query).await
    }

    /// Query the host-owned redacted configuration projection without a task.
    pub async fn query_config_sidebar(
        &mut self,
    ) -> Result<Result<ConfigSidebarSnapshot, QueryError>, IpcError> {
        typed_queries::query_config_sidebar(self).await
    }

    pub async fn query_agent_connection(
        &mut self,
    ) -> Result<Result<AgentConnectionSnapshot, QueryError>, IpcError> {
        typed_queries::query_agent_connection(self).await
    }

    /// Global local-pipe-only setup; no synthetic task identity is attached.
    pub async fn query_remote_access(
        &mut self,
        request: crate::host::remote_setup::RemoteSetupRequest,
    ) -> Result<Result<crate::host::remote_setup::RemoteSetupReply, QueryError>, IpcError> {
        typed_queries::query_remote_access(self, request).await
    }

    pub async fn query_provider_settings(
        &mut self,
        request: crate::providers::settings::ProviderSettingsHostRequest,
    ) -> Result<Result<crate::providers::settings::ProviderSettingsReply, QueryError>, IpcError>
    {
        typed_queries::query_provider_settings(self, request).await
    }

    /// Query the bounded path-redacted repository catalog for one Task.
    pub async fn query_git_repositories(
        &mut self,
        task_id: TaskId,
    ) -> Result<Result<crate::domain::TaskGitRepositoriesProjection, QueryError>, IpcError> {
        typed_queries::query_git_repositories(self, task_id).await
    }

    /// Open or resume one paged snapshot section. Requires granted PagedSnapshots.
    pub async fn snapshot_page(
        &mut self,
        section: SnapshotSection,
        snapshot_id: Option<SnapshotId>,
        resume_cursor: Option<Vec<u8>>,
    ) -> Result<Result<SnapshotPage, QueryError>, IpcError> {
        if !self
            .metadata
            .granted_capabilities()
            .contains(Capability::PagedSnapshots)
        {
            return Err(IpcError::UnsupportedCapability);
        }

        let request_id = RequestId::new();
        let client_id = self.client_id();
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
            .metadata
            .granted_capabilities()
            .contains(Capability::PagedSnapshots)
        {
            return Err(IpcError::UnsupportedCapability);
        }

        let request_id = RequestId::new();
        let client_id = self.client_id();
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
        if !self
            .metadata
            .granted_capabilities()
            .contains(Capability::EventReplay)
        {
            return Err(IpcError::UnsupportedCapability);
        }

        let request_id = RequestId::new();
        let client_id = self.client_id();
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
        if !self
            .metadata
            .granted_capabilities()
            .contains(Capability::EventReplay)
        {
            return Err(IpcError::UnsupportedCapability);
        }

        let request_id = RequestId::new();
        let client_id = self.client_id();
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
        if !self
            .metadata
            .granted_capabilities()
            .contains(Capability::EventReplay)
        {
            // The capability may have disappeared during reconnect while the
            // caller still owns an older generation. Fence its borrowed queue
            // before surfacing the capability error, so replacement can never
            // observe a foreign late frame.
            if self.connection.is_some() {
                self.fence_retired_subscription(subscription_id).await?;
            }
            return Err(IpcError::UnsupportedCapability);
        }

        let request_id = RequestId::new();
        let client_id = self.client_id();
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
                // A response timeout/transport failure still retires the exact
                // generation in the borrowed queue before this connection is
                // discarded. Cloned test/bridge handles must not be able to
                // deliver that old tail into a later replacement.
                let _ = self.fence_retired_subscription(subscription_id).await;
                self.retire_connection();
                return Err(error);
            }
        };

        match reply.outcome {
            QueryOutcome::Err(error @ QueryError::NotFound) => {
                self.fence_retired_subscription(subscription_id).await?;
                Ok(Err(error))
            }
            QueryOutcome::Err(error) => {
                self.fence_retired_subscription(subscription_id).await?;
                Ok(Err(error))
            }
            QueryOutcome::Ok(QueryResult::EventReplayReleased {
                subscription_id: released,
            }) if released == subscription_id => {
                self.fence_retired_subscription(subscription_id).await?;
                Ok(Ok(()))
            }
            QueryOutcome::Ok(QueryResult::EventReplayReleased { .. }) => {
                let _ = self.fence_retired_subscription(subscription_id).await;
                self.retire_connection();
                Err(IpcError::CorrelationMismatch)
            }
            QueryOutcome::Ok(_) => {
                let _ = self.fence_retired_subscription(subscription_id).await;
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
        if !self
            .metadata
            .granted_capabilities()
            .contains(Capability::ChunkResume)
        {
            return Err(IpcError::UnsupportedCapability);
        }

        let request_id = RequestId::new();
        let client_id = self.client_id();
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
        if !self
            .metadata
            .granted_capabilities()
            .contains(Capability::ChunkResume)
        {
            return Err(IpcError::UnsupportedCapability);
        }

        let request_id = RequestId::new();
        let client_id = self.client_id();
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
        if !self
            .metadata
            .granted_capabilities()
            .contains(Capability::ChunkResume)
        {
            return Err(IpcError::UnsupportedCapability);
        }

        let request_id = RequestId::new();
        let client_id = self.client_id();
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
            .metadata
            .granted_capabilities()
            .contains(Capability::OperationSettlement)
        {
            return Err(IpcError::UnsupportedCapability);
        }

        let request_id = RequestId::new();
        let client_id = self.client_id();
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
        self.connection
            .as_ref()
            .filter(|connection| connection.is_live())
            .ok_or(IpcError::Unavailable)
    }

    fn retire_connection(&mut self) {
        self.connection = None;
    }

    async fn fence_retired_subscription(
        &mut self,
        subscription_id: SubscriptionId,
    ) -> Result<(), IpcError> {
        let result = match self.connection.as_ref() {
            Some(connection) => connection.retire_subscription_id(subscription_id).await,
            None => Err(IpcError::Unavailable),
        };
        if result.is_err() {
            // A bounded drain failure is not safe to carry into a replacement
            // generation. Force reconnect/resync instead of dropping unknown
            // frames or leaving the shared queue poisoned.
            self.retire_connection();
        }
        result
    }
}

#[async_trait]
impl AsyncHostRequestPort for HostClient {
    fn client_id(&self) -> ClientId {
        HostClient::client_id(self)
    }

    fn granted_capabilities(&self) -> CapabilitySet {
        HostClient::granted_capabilities(self)
    }

    async fn request_query(
        &mut self,
        envelope: QueryEnvelope,
        timeout: Option<Duration>,
    ) -> Result<QueryReply, IpcError> {
        if envelope.client_id != self.client_id() {
            return Err(IpcError::Unauthorized);
        }
        let outcome = {
            let connection = self.live_connection()?;
            match timeout {
                Some(deadline) => connection.query_with_timeout(envelope, deadline).await,
                None => connection.query(envelope).await,
            }
        };
        match outcome {
            Ok(reply) => Ok(reply),
            Err(error) => {
                self.retire_connection();
                Err(error)
            }
        }
    }

    async fn request_command(
        &mut self,
        envelope: CommandEnvelope,
    ) -> Result<CommandReceipt, IpcError> {
        self.execute_command(envelope).await
    }

    async fn retire_request_transport(&mut self) {
        self.retire_connection();
    }
}

impl HostClient {
    #[cfg(test)]
    pub(crate) fn from_parts_for_test(
        config: HostClientConfig,
        server_hello: ServerHello,
        connection: Option<ClientConnection>,
        tracked: BTreeMap<OperationId, TrackedOperation>,
    ) -> Self {
        let reconnect_grant = server_hello.reconnect_grant.clone();
        Self {
            transport: HostClientTransport::Local {
                config,
                endpoint: String::new(),
                reconnect_grant,
            },
            connection,
            metadata: ConnectionMetadata::Local(server_hello),
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

impl InboxTransport for HostClient {
    fn is_connected(&self) -> bool {
        HostClient::is_connected(self)
    }

    fn synchronize<'a>(
        &'a mut self,
        subscription: &'a mut ClientSubscription,
    ) -> InboxTransportFuture<'a, Result<(), SubscriptionError>> {
        Box::pin(async move { subscription.synchronize(self).await })
    }

    fn receive_one<'a>(
        &'a self,
        subscription: &'a mut ClientSubscription,
    ) -> InboxTransportFuture<'a, Result<SubscriptionUpdate, SubscriptionError>> {
        Box::pin(async move { subscription.recv_and_apply(self).await })
    }

    fn release<'a>(
        &'a mut self,
        subscription: &'a mut ClientSubscription,
    ) -> InboxTransportFuture<'a, Result<(), SubscriptionError>> {
        Box::pin(async move { subscription.release(self).await })
    }

    fn reconnect<'a>(&'a mut self) -> InboxTransportFuture<'a, Result<(), IpcError>> {
        Box::pin(async move { HostClient::reconnect(self).await })
    }

    fn execute_command<'a>(
        &'a mut self,
        envelope: CommandEnvelope,
    ) -> InboxTransportFuture<'a, Result<CommandReceipt, IpcError>> {
        Box::pin(async move { HostClient::execute_command(self, envelope).await })
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

fn unix_time_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};

    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
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
        QueryResult::CommandReceiptStatus { .. }
        | QueryResult::TaskSnapshot { .. }
        | QueryResult::SnapshotPage { .. }
        | QueryResult::SnapshotReleased { .. }
        | QueryResult::EventReplayPage { .. }
        | QueryResult::EventReplayReleased { .. }
        | QueryResult::ArtifactContentPage { .. }
        | QueryResult::ArtifactContentReleased { .. }
        | QueryResult::HostQuitInspection { .. }
        | QueryResult::PromptLibrary(_)
        | QueryResult::TaskCockpit(_) => Err(IpcError::UnexpectedResponse),
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

async fn open_local_connection(
    config: &HostClientConfig,
    endpoint: &str,
    reconnect_grant: Option<ReconnectGrant>,
) -> Result<(ClientConnection, ConnectionMetadata), IpcError> {
    let fingerprint = profile_fingerprint_for_named_profile(&config.named_profile)?;
    let hello = ClientHello::new_with_reconnect_grant(
        config.client_build.clone(),
        config.client_id,
        fingerprint,
        config.requested,
        config.limits,
        reconnect_grant,
    )
    .map_err(IpcError::ClientHello)?;
    let connection = connect(endpoint, &hello).await?;
    let metadata = connection.metadata();
    Ok((connection, metadata))
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

    #[tokio::test]
    async fn retained_handle_is_not_connected_after_shared_transport_failure() {
        use super::{HostClient, HostClientConfig};
        use crate::client::connection::{ClientConnection, ScriptedDetachBehavior};
        use crate::domain::{ClientId, Command, CommandEnvelope};
        use crate::protocol::{CapabilitySet, FrameLimits};

        let client_id = ClientId::new();
        let hello = test_server_hello(CapabilitySet::empty(), uuid::Uuid::now_v7());
        let connection = ClientConnection::scripted_for_test(
            client_id,
            hello.clone(),
            ScriptedDetachBehavior::ClosedWriteQueue,
        );
        let client = HostClient::from_parts_for_test(
            HostClientConfig {
                named_profile: "transport-health-test".into(),
                client_build: "test".into(),
                client_id,
                requested: CapabilitySet::empty(),
                limits: FrameLimits::v1_default(),
            },
            hello,
            Some(connection),
            BTreeMap::new(),
        );
        assert!(client.is_connected());
        assert!(client
            .execute_command_concurrent(CommandEnvelope {
                command_id: CommandId::new(),
                client_id,
                task_id: None,
                issued_at_ms: 1,
                expected_task_revision: None,
                command: Command::SettleTask,
            })
            .await
            .is_err());
        assert!(
            client.connection.is_some(),
            "read-only call retains the handle"
        );
        assert!(
            !client.is_connected(),
            "poisoned transport cannot grant readiness"
        );
        assert!(client.live_connection().is_err());
    }

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
            prompt_mutation: None,
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
    fn absorb_tracked_operations_moves_pending_without_inventing_ids() {
        use super::{HostClient, HostClientConfig};
        use crate::client::connection::ClientConnection;
        use crate::domain::ClientId;
        use crate::protocol::{Capability, CapabilitySet, FrameLimits};

        let client_id = ClientId::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xd0,
        ])
        .expect("client");
        let connection_id = uuid::Uuid::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xd1,
        ]);
        let hello = test_server_hello(
            CapabilitySet::from_capabilities([Capability::ChunkResume]),
            connection_id,
        );
        let op = operation_id(0xd2);
        let cmd = command_id(0xd3);
        let mut source = HostClient::from_parts_for_test(
            HostClientConfig {
                named_profile: "absorb-unit".into(),
                client_build: "devmanager/test".into(),
                client_id,
                requested: CapabilitySet::from_capabilities([Capability::ChunkResume]),
                limits: FrameLimits::v1_default(),
            },
            hello.clone(),
            Some(ClientConnection::inert_stub_for_test(
                client_id,
                hello.clone(),
            )),
            BTreeMap::from([(op, TrackedOperation::Pending { command_id: cmd })]),
        );
        let mut dest = HostClient::from_parts_for_test(
            HostClientConfig {
                named_profile: "absorb-unit".into(),
                client_build: "devmanager/test".into(),
                client_id,
                requested: CapabilitySet::from_capabilities([Capability::ChunkResume]),
                limits: FrameLimits::v1_default(),
            },
            hello.clone(),
            Some(ClientConnection::inert_stub_for_test(client_id, hello)),
            BTreeMap::new(),
        );
        dest.absorb_tracked_operations(&mut source).expect("absorb");
        assert!(source.tracked().is_empty());
        assert_eq!(
            dest.tracked_operation(op),
            Some(&TrackedOperation::Pending { command_id: cmd })
        );
    }

    #[test]
    fn absorb_tracked_operations_does_not_downgrade_resolved_to_pending() {
        use super::{HostClient, HostClientConfig};
        use crate::client::connection::ClientConnection;
        use crate::domain::ClientId;
        use crate::protocol::{Capability, CapabilitySet, FrameLimits};

        let client_id = ClientId::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xd4,
        ])
        .expect("client");
        let connection_id = uuid::Uuid::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xd5,
        ]);
        let hello = test_server_hello(
            CapabilitySet::from_capabilities([Capability::ChunkResume]),
            connection_id,
        );
        let op = operation_id(0xd6);
        let cmd = command_id(0xd7);
        let mut source = HostClient::from_parts_for_test(
            HostClientConfig {
                named_profile: "absorb-unit".into(),
                client_build: "devmanager/test".into(),
                client_id,
                requested: CapabilitySet::from_capabilities([Capability::ChunkResume]),
                limits: FrameLimits::v1_default(),
            },
            hello.clone(),
            Some(ClientConnection::inert_stub_for_test(
                client_id,
                hello.clone(),
            )),
            BTreeMap::from([(op, TrackedOperation::Pending { command_id: cmd })]),
        );
        let mut dest = HostClient::from_parts_for_test(
            HostClientConfig {
                named_profile: "absorb-unit".into(),
                client_build: "devmanager/test".into(),
                client_id,
                requested: CapabilitySet::from_capabilities([Capability::ChunkResume]),
                limits: FrameLimits::v1_default(),
            },
            hello.clone(),
            Some(ClientConnection::inert_stub_for_test(client_id, hello)),
            BTreeMap::from([(
                op,
                TrackedOperation::Resolved {
                    command_id: cmd,
                    state: settled(),
                },
            )]),
        );
        dest.absorb_tracked_operations(&mut source).expect("absorb");
        assert_eq!(
            dest.tracked_operation(op),
            Some(&TrackedOperation::Resolved {
                command_id: cmd,
                state: settled(),
            })
        );
    }

    #[test]
    fn absorb_tracked_operations_upgrades_pending_to_resolved() {
        use super::{HostClient, HostClientConfig};
        use crate::client::connection::ClientConnection;
        use crate::domain::ClientId;
        use crate::protocol::{Capability, CapabilitySet, FrameLimits};

        let client_id = ClientId::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xd8,
        ])
        .expect("client");
        let connection_id = uuid::Uuid::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xd9,
        ]);
        let hello = test_server_hello(
            CapabilitySet::from_capabilities([Capability::ChunkResume]),
            connection_id,
        );
        let op = operation_id(0xda);
        let cmd = command_id(0xdb);
        let mut source = HostClient::from_parts_for_test(
            HostClientConfig {
                named_profile: "absorb-unit".into(),
                client_build: "devmanager/test".into(),
                client_id,
                requested: CapabilitySet::from_capabilities([Capability::ChunkResume]),
                limits: FrameLimits::v1_default(),
            },
            hello.clone(),
            Some(ClientConnection::inert_stub_for_test(
                client_id,
                hello.clone(),
            )),
            BTreeMap::from([(
                op,
                TrackedOperation::Resolved {
                    command_id: cmd,
                    state: settled(),
                },
            )]),
        );
        let mut dest = HostClient::from_parts_for_test(
            HostClientConfig {
                named_profile: "absorb-unit".into(),
                client_build: "devmanager/test".into(),
                client_id,
                requested: CapabilitySet::from_capabilities([Capability::ChunkResume]),
                limits: FrameLimits::v1_default(),
            },
            hello.clone(),
            Some(ClientConnection::inert_stub_for_test(client_id, hello)),
            BTreeMap::from([(op, TrackedOperation::Pending { command_id: cmd })]),
        );
        dest.absorb_tracked_operations(&mut source).expect("absorb");
        assert_eq!(
            dest.tracked_operation(op),
            Some(&TrackedOperation::Resolved {
                command_id: cmd,
                state: settled(),
            })
        );
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
            reconnect_grant: None,
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

    #[tokio::test(flavor = "current_thread")]
    async fn release_errors_always_fence_old_subscription_queue() {
        use super::{HostClient, HostClientConfig};
        use crate::client::connection::{ClientConnection, ScriptedDetachBehavior};
        use crate::client::UnsolicitedServerMessage;
        use crate::domain::event::{DomainEvent, Event};
        use crate::domain::id::{EventId, SubscriptionId};
        use crate::domain::query::QueryError;
        use crate::domain::ClientId;
        use crate::protocol::{Capability, CapabilitySet, FrameLimits};
        use std::time::Duration;

        let client_id = ClientId::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xe1,
        ])
        .expect("client");
        let connection_id = uuid::Uuid::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xe2,
        ]);
        let subscription_id = SubscriptionId::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xe3,
        ])
        .expect("subscription");
        let hello = test_server_hello(
            CapabilitySet::from_capabilities([Capability::EventReplay]),
            connection_id,
        );
        let connection = ClientConnection::scripted_for_test(
            client_id,
            hello.clone(),
            ScriptedDetachBehavior::ReleaseQueryError,
        );
        let old_frame = UnsolicitedServerMessage::DurableEvent {
            subscription_id,
            event: DomainEvent {
                id: EventId::from_bytes([
                    0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x00, 0xe4,
                ])
                .expect("event"),
                task_id: None,
                sequence: 1,
                task_revision: None,
                occurred_at_ms: 1,
                payload: Event::TaskReopened,
            },
        };
        connection
            .push_durable_for_test(old_frame.clone())
            .expect("queue old frame");
        let mut client = HostClient::from_parts_for_test(
            HostClientConfig {
                named_profile: "release-error-unit".into(),
                client_build: "devmanager/test".into(),
                client_id,
                requested: CapabilitySet::from_capabilities([Capability::EventReplay]),
                limits: FrameLimits::v1_default(),
            },
            hello.clone(),
            Some(connection.clone()),
            BTreeMap::new(),
        );

        assert!(matches!(
            client.release_event_replay(subscription_id).await,
            Ok(Err(QueryError::Unauthorized))
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(50), connection.recv_unsolicited())
                .await
                .is_err(),
            "application release errors must still fence queued old frames"
        );

        let transport_connection = ClientConnection::scripted_for_test(
            client_id,
            hello.clone(),
            ScriptedDetachBehavior::ClosedWriteQueue,
        );
        transport_connection
            .push_durable_for_test(old_frame.clone())
            .expect("queue old frame before transport failure");
        let mut transport_client = HostClient::from_parts_for_test(
            HostClientConfig {
                named_profile: "release-transport-error-unit".into(),
                client_build: "devmanager/test".into(),
                client_id,
                requested: CapabilitySet::from_capabilities([Capability::EventReplay]),
                limits: FrameLimits::v1_default(),
            },
            hello.clone(),
            Some(transport_connection.clone()),
            BTreeMap::new(),
        );
        assert!(matches!(
            transport_client.release_event_replay(subscription_id).await,
            Err(IpcError::Unavailable)
        ));
        assert!(
            tokio::time::timeout(
                Duration::from_millis(50),
                transport_connection.recv_unsolicited()
            )
            .await
            .is_err(),
            "transport release failures must fence queued old frames before replacement"
        );

        let wrong_connection = ClientConnection::scripted_for_test(
            client_id,
            hello,
            ScriptedDetachBehavior::ReleaseWrongSubscriptionAck,
        );
        wrong_connection
            .push_durable_for_test(old_frame)
            .expect("queue old frame before wrong release acknowledgement");
        let mut wrong_client = HostClient::from_parts_for_test(
            HostClientConfig {
                named_profile: "release-wrong-ack-unit".into(),
                client_build: "devmanager/test".into(),
                client_id,
                requested: CapabilitySet::from_capabilities([Capability::EventReplay]),
                limits: FrameLimits::v1_default(),
            },
            test_server_hello(
                CapabilitySet::from_capabilities([Capability::EventReplay]),
                connection_id,
            ),
            Some(wrong_connection.clone()),
            BTreeMap::new(),
        );
        assert!(matches!(
            wrong_client.release_event_replay(subscription_id).await,
            Err(IpcError::CorrelationMismatch)
        ));
        assert!(
            tokio::time::timeout(
                Duration::from_millis(50),
                wrong_connection.recv_unsolicited()
            )
            .await
            .is_err(),
            "wrong release acknowledgements must fence queued old frames before replacement"
        );
    }
}
