//! Host-command port used by Connect sessions.
//!
//! This is the HostClient-equivalent surface: mutations, queries, and
//! subscriptions enter as domain envelopes. Presentation DTOs and writer
//! leases are not part of this contract.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Mutex as AsyncMutex;

use crate::domain::command::{CommandEnvelope, CommandReceipt};
use crate::domain::id::{ArtifactId, RequestId, TaskId};
use crate::domain::query::{QueryEnvelope, QueryReply};
use crate::host::IpcError;
use crate::protocol::{ClientRequest, NegotiatedParameters, ServerMessage, UpdateHandoffReply};

use super::connection::UnsolicitedServerMessage;
use super::host_client::{ArtifactContentBatch, EventReplayBatch, HostClient, HostClientConfig};

/// Async Connect-facing command port. The implementation is deliberately
/// request-lane only: the host remains the sole executor and the web route
/// never gets a second CommandBus or unsolicited-message writer.
#[async_trait]
pub trait ConnectHostCommandPort: Send + Sync {
    async fn execute(
        &self,
        negotiated: NegotiatedParameters,
        request: ClientRequest,
    ) -> Result<ServerMessage, IpcError>;
}

const MAX_CONNECT_HOST_CLIENTS: usize = 32;

struct ConnectHostClients {
    clients: HashMap<crate::domain::id::ClientId, Arc<AsyncMutex<HostClient>>>,
}

/// Cross-process adapter for Connect. Each authenticated Connect client gets
/// one HostClient/pipe connection keyed by its exact negotiated ClientId. The
/// map is bounded, operations on one client are serialized, and a broken host
/// connection is removed so a restart can be discovered on the next request.
pub struct HostClientConnectPort {
    template: HostClientConfig,
    clients: Arc<AsyncMutex<ConnectHostClients>>,
}

impl fmt::Debug for HostClientConnectPort {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HostClientConnectPort")
            .field("named_profile", &self.template.named_profile)
            .field("max_clients", &MAX_CONNECT_HOST_CLIENTS)
            .finish()
    }
}

impl HostClientConnectPort {
    pub fn new(template: HostClientConfig) -> Self {
        Self {
            template,
            clients: Arc::new(AsyncMutex::new(ConnectHostClients {
                clients: HashMap::new(),
            })),
        }
    }

    /// Drop all per-client pipes before the owning listener is joined.
    pub async fn clear(&self) {
        self.clients.lock().await.clients.clear();
    }

    pub async fn client_count(&self) -> usize {
        self.clients.lock().await.clients.len()
    }

    async fn client_for(
        &self,
        client_id: crate::domain::id::ClientId,
    ) -> Result<Arc<AsyncMutex<HostClient>>, IpcError> {
        {
            let state = self.clients.lock().await;
            if let Some(client) = state.clients.get(&client_id) {
                return Ok(client.clone());
            }
            if state.clients.len() >= MAX_CONNECT_HOST_CLIENTS {
                return Err(IpcError::Busy);
            }
        }

        let mut config = self.template.clone();
        // The HostClient Hello and every request must carry the exact Connect
        // identity; do not collapse clients onto the native shell's identity.
        config.client_id = client_id;
        let client = Arc::new(AsyncMutex::new(HostClient::connect(config).await?));

        let mut state = self.clients.lock().await;
        if let Some(existing) = state.clients.get(&client_id) {
            return Ok(existing.clone());
        }
        if state.clients.len() >= MAX_CONNECT_HOST_CLIENTS {
            return Err(IpcError::Busy);
        }
        state.clients.insert(client_id, client.clone());
        Ok(client)
    }

    async fn remove_if_same(
        &self,
        client_id: crate::domain::id::ClientId,
        client: &Arc<AsyncMutex<HostClient>>,
    ) {
        let mut state = self.clients.lock().await;
        if state
            .clients
            .get(&client_id)
            .is_some_and(|current| Arc::ptr_eq(current, client))
        {
            state.clients.remove(&client_id);
        }
    }
}

#[async_trait]
impl ConnectHostCommandPort for HostClientConnectPort {
    async fn execute(
        &self,
        negotiated: NegotiatedParameters,
        request: ClientRequest,
    ) -> Result<ServerMessage, IpcError> {
        let client_id = negotiated.client_id;
        let client = match &request {
            ClientRequest::TerminalInput(request) if request.client_id != client_id => {
                return Err(IpcError::Unauthorized);
            }
            ClientRequest::Command(envelope) if envelope.client_id != client_id => {
                return Err(IpcError::Unauthorized);
            }
            ClientRequest::Query(envelope) if envelope.client_id != client_id => {
                return Err(IpcError::Unauthorized);
            }
            ClientRequest::Detach(_) => return Err(IpcError::Unsupported),
            _ => self.client_for(client_id).await?,
        };

        let mut client_guard = client.lock().await;
        // HostClient's authenticated IPC Hello is the grant authority. The
        // Connect capability claim is intentionally not used to elevate it.
        let result = match request {
            ClientRequest::TerminalInput(request) => {
                let input_id = request.input_id;
                client_guard
                    .execute_terminal_input(request)
                    .await
                    .map(|ack| {
                        ServerMessage::TerminalInputAck(
                            crate::terminal::protocol::TerminalInputAck {
                                // The low-level HostClient API returns only the
                                // admission result; the request id is retained by
                                // the caller-side connection. Connect's one request
                                // path uses the request's own id for correlation.
                                input_id,
                                ack,
                            },
                        )
                    })
            }
            ClientRequest::Command(envelope) => {
                if let crate::domain::command::Command::PrepareUpdate(intent) = &envelope.command {
                    client_guard
                        .prepare_update(
                            envelope.command_id,
                            &intent.target_version,
                            &intent.client_build,
                            &intent.host_build,
                            intent.allow_explicit_confirm_with_active,
                        )
                        .await
                        .map(|token| {
                            ServerMessage::UpdateHandoff(UpdateHandoffReply {
                                command_id: envelope.command_id,
                                token,
                            })
                        })
                } else {
                    client_guard
                        .execute_command(envelope)
                        .await
                        .map(ServerMessage::CommandReceipt)
                }
            }
            ClientRequest::Query(envelope) => client_guard
                .query(envelope)
                .await
                .map(ServerMessage::QueryReply),
            ClientRequest::Detach(_) => Err(IpcError::Unsupported),
        };
        let still_connected = client_guard.is_connected();
        drop(client_guard);

        if !still_connected {
            self.remove_if_same(client_id, &client).await;
        }
        result
    }
}

/// Caller-owned provider input. The host remains execution authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderInputCall {
    pub task_id: TaskId,
    pub text: String,
}

/// First-answer-wins approval/question response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalAnswerCall {
    pub task_id: TaskId,
    pub request_id: RequestId,
    pub approved: bool,
}

/// On-demand child transcript fetch. Never part of the initial snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptFetchCall {
    pub task_id: TaskId,
    pub artifact_id: ArtifactId,
}

/// Owner-only personal prompt-library search. Task invitations cannot use this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptQueryCall {
    pub query: String,
    pub resume_cursor: Option<Vec<u8>>,
}

/// Bounded prompt metadata page. Bodies stay on-demand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptMetadataPage {
    pub items: Vec<PromptMetadataItem>,
    pub next_cursor: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptMetadataItem {
    pub prompt_id: String,
    pub title: String,
    pub version: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostPortError {
    Unavailable,
    Unauthorized,
    Unsupported,
    CorrelationMismatch,
    Bounds,
    Ipc(String),
}

impl fmt::Display for HostPortError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable => formatter.write_str("host command port is unavailable"),
            Self::Unauthorized => formatter.write_str("host command port unauthorized"),
            Self::Unsupported => formatter.write_str("host command port unsupported"),
            Self::CorrelationMismatch => {
                formatter.write_str("host command port correlation mismatch")
            }
            Self::Bounds => formatter.write_str("host command port bounds exceeded"),
            Self::Ipc(detail) => write!(formatter, "host command port ipc: {detail}"),
        }
    }
}

impl std::error::Error for HostPortError {}

impl From<IpcError> for HostPortError {
    fn from(error: IpcError) -> Self {
        match error {
            IpcError::Unavailable => Self::Unavailable,
            IpcError::Unauthorized => Self::Unauthorized,
            IpcError::UnsupportedCapability => Self::Unsupported,
            IpcError::CorrelationMismatch => Self::CorrelationMismatch,
            other => Self::Ipc(other.to_string()),
        }
    }
}

/// Synchronous HostClient-equivalent used by ConnectSession and tests.
pub trait HostCommandPort {
    fn execute_command(
        &mut self,
        envelope: CommandEnvelope,
    ) -> Result<CommandReceipt, HostPortError>;

    fn query(&mut self, envelope: QueryEnvelope) -> Result<QueryReply, HostPortError>;

    fn provider_input(&mut self, call: ProviderInputCall) -> Result<CommandReceipt, HostPortError>;

    fn answer_approval(
        &mut self,
        call: ApprovalAnswerCall,
    ) -> Result<CommandReceipt, HostPortError>;

    fn fetch_child_transcript(
        &mut self,
        call: TranscriptFetchCall,
    ) -> Result<ArtifactContentBatch, HostPortError>;

    fn query_personal_prompts(
        &mut self,
        call: PromptQueryCall,
    ) -> Result<PromptMetadataPage, HostPortError>;

    fn drain_unsolicited(&mut self) -> Vec<UnsolicitedServerMessage>;

    fn open_event_replay(
        &mut self,
        after_sequence: u64,
    ) -> Result<EventReplayBatch, HostPortError> {
        let _ = after_sequence;
        Err(HostPortError::Unsupported)
    }
}
