//! Minimal local ClientHello helper, synchronous request/reply connection, and
//! the reusable HostClient wrapper for profile-derived attach/reconnect.

pub mod action;
pub mod cli;
mod host_client;

pub use action::{
    catalog, require_unique_ids, task_show_query, ActionDescriptor, ActionRisk, ActionScope,
    ACTION_HOST_ACTIONS, ACTION_HOST_STATUS, ACTION_TASK_SHOW,
};
pub use cli::{dispatch_ctl_from_args, parse_ctl_args, run_ctl, CliError, CtlCommand};
pub use host_client::{HostClient, HostClientConfig, TrackedOperation};

use crate::domain::command::{CommandEnvelope, CommandReceipt};
use crate::domain::id::CommandId;
use crate::domain::query::{QueryEnvelope, QueryReply};
use crate::domain::{
    ClientId, RequestId, MAX_SNAPSHOT_PAGE_ENCODED_BYTES, MAX_SNAPSHOT_PAGE_ITEMS,
};
use crate::host::{
    codecs_for_limits, handshake_codecs, handshake_timeout, read_physical_frame,
    request_completion_timeout, write_physical_frame, IpcError,
};
use crate::protocol::{
    ClientHello, ClientRequest, FrameLimits, MessagePackCodec, PhysicalFrameCodec, ServerHello,
    ServerResponse, MAX_PHYSICAL_FRAME_BYTES, MAX_REASSEMBLED_MESSAGE_BYTES,
};

/// Connect to a host pipe endpoint, complete Hello, and retain the pipe.
pub async fn connect(endpoint: &str, hello: &ClientHello) -> Result<ClientConnection, IpcError> {
    #[cfg(windows)]
    {
        windows_connect(endpoint, hello).await
    }
    #[cfg(not(windows))]
    {
        let _ = endpoint;
        let _ = hello;
        Err(IpcError::Unsupported)
    }
}

/// Connect, complete Hello, and drop the retained pipe (compatibility wrapper).
pub async fn perform_client_hello(
    endpoint: &str,
    hello: &ClientHello,
) -> Result<ServerHello, IpcError> {
    let connection = connect(endpoint, hello).await?;
    Ok(connection.server_hello().clone())
}

/// Client-side authenticated pipe after Hello.
pub struct ClientConnection {
    client_id: ClientId,
    server_hello: ServerHello,
    physical: PhysicalFrameCodec,
    message: MessagePackCodec,
    poisoned: bool,
    #[cfg(windows)]
    pipe: tokio::net::windows::named_pipe::NamedPipeClient,
}

impl ClientConnection {
    pub fn client_id(&self) -> ClientId {
        self.client_id
    }

    pub fn server_hello(&self) -> &ServerHello {
        &self.server_hello
    }

    pub fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    pub async fn execute_command(
        &mut self,
        envelope: CommandEnvelope,
    ) -> Result<CommandReceipt, IpcError> {
        connection_ensure_live(self.poisoned)?;
        let command_id = envelope.command_id;
        let outcome = match self.round_trip(ClientRequest::Command(envelope)).await {
            Ok(response) => match_command_response(command_id, response),
            Err(error) => Err(error),
        };
        connection_fail_closed(&mut self.poisoned, outcome)
    }

    pub async fn query(&mut self, envelope: QueryEnvelope) -> Result<QueryReply, IpcError> {
        connection_ensure_live(self.poisoned)?;
        let request_id = envelope.request_id;
        let outcome = match self.round_trip(ClientRequest::Query(envelope)).await {
            Ok(response) => match_query_response(request_id, response),
            Err(error) => Err(error),
        };
        connection_fail_closed(&mut self.poisoned, outcome)
    }

    async fn round_trip(&mut self, request: ClientRequest) -> Result<ServerResponse, IpcError> {
        // Caller already ensured live; transport/encode failures poison via fail_closed above.
        #[cfg(windows)]
        {
            windows_client_round_trip(self, request).await
        }
        #[cfg(not(windows))]
        {
            let _ = request;
            Err(IpcError::Unsupported)
        }
    }
}

fn match_command_response(
    command_id: CommandId,
    response: ServerResponse,
) -> Result<CommandReceipt, IpcError> {
    match response {
        ServerResponse::CommandReceipt(receipt) => {
            if receipt.command_id() != command_id {
                return Err(IpcError::CorrelationMismatch);
            }
            Ok(receipt)
        }
        ServerResponse::QueryReply(_) => Err(IpcError::UnexpectedResponse),
    }
}

fn match_query_response(
    request_id: RequestId,
    response: ServerResponse,
) -> Result<QueryReply, IpcError> {
    match response {
        ServerResponse::QueryReply(reply) => {
            if reply.request_id != request_id {
                return Err(IpcError::CorrelationMismatch);
            }
            Ok(reply)
        }
        ServerResponse::CommandReceipt(_) => Err(IpcError::UnexpectedResponse),
    }
}

fn connection_ensure_live(poisoned: bool) -> Result<(), IpcError> {
    if poisoned {
        Err(IpcError::ConnectionPoisoned)
    } else {
        Ok(())
    }
}

fn connection_fail_closed<T>(
    poisoned: &mut bool,
    result: Result<T, IpcError>,
) -> Result<T, IpcError> {
    if result.is_err() {
        *poisoned = true;
    }
    result
}

#[cfg(windows)]
async fn windows_connect(
    endpoint: &str,
    hello: &ClientHello,
) -> Result<ClientConnection, IpcError> {
    use tokio::io::AsyncWriteExt;
    use tokio::net::windows::named_pipe::ClientOptions;

    let (hello_physical, hello_message) = handshake_codecs()?;
    let encoded = hello_message.encode(hello).map_err(IpcError::MessagePack)?;

    let mut pipe = ClientOptions::new().open(endpoint).map_err(IpcError::Io)?;

    let server_hello = tokio::time::timeout(handshake_timeout(), async {
        write_physical_frame(&mut pipe, &hello_physical, &encoded).await?;
        pipe.flush().await.map_err(IpcError::Io)?;
        let payload = read_physical_frame(&mut pipe, &hello_physical).await?;
        hello_message
            .decode::<ServerHello>(&payload)
            .map_err(IpcError::MessagePack)
    })
    .await
    .map_err(|_| IpcError::Timeout)??;

    validate_server_hello(hello, &server_hello)?;
    let (physical, message) = codecs_for_limits(server_hello.limits)?;
    Ok(ClientConnection {
        client_id: hello.client_id,
        server_hello,
        physical,
        message,
        poisoned: false,
        pipe,
    })
}

#[cfg(windows)]
async fn windows_client_round_trip(
    connection: &mut ClientConnection,
    request: ClientRequest,
) -> Result<ServerResponse, IpcError> {
    use tokio::io::AsyncWriteExt;

    tokio::time::timeout(request_completion_timeout(), async {
        let encoded = connection
            .message
            .encode(&request)
            .map_err(IpcError::MessagePack)?;
        write_physical_frame(&mut connection.pipe, &connection.physical, &encoded).await?;
        connection.pipe.flush().await.map_err(IpcError::Io)?;
        let payload = read_physical_frame(&mut connection.pipe, &connection.physical).await?;
        connection
            .message
            .decode::<ServerResponse>(&payload)
            .map_err(IpcError::MessagePack)
    })
    .await
    .map_err(|_| IpcError::Timeout)?
}

fn validate_server_hello(sent: &ClientHello, received: &ServerHello) -> Result<(), IpcError> {
    if received.profile_fingerprint != sent.profile_fingerprint {
        return Err(IpcError::ProfileMismatch);
    }
    if received.protocol_major != sent.protocol_major {
        return Err(IpcError::HelloInconsistent);
    }
    if received.protocol_minor > sent.protocol_minor {
        return Err(IpcError::HelloInconsistent);
    }
    if received.granted.bits() & !sent.requested.bits() != 0 {
        return Err(IpcError::HelloInconsistent);
    }
    if !limits_within_offer_and_caps(received.limits, sent.limits) {
        return Err(IpcError::HelloInconsistent);
    }
    Ok(())
}

fn limits_within_offer_and_caps(got: FrameLimits, offer: FrameLimits) -> bool {
    got.validate_offer().is_ok()
        && got.max_physical_frame_bytes
            <= offer.max_physical_frame_bytes.min(MAX_PHYSICAL_FRAME_BYTES)
        && got.max_reassembled_message_bytes
            <= offer
                .max_reassembled_message_bytes
                .min(MAX_REASSEMBLED_MESSAGE_BYTES)
        && got.max_page_items <= offer.max_page_items.min(MAX_SNAPSHOT_PAGE_ITEMS)
        && got.max_page_encoded_bytes
            <= offer
                .max_page_encoded_bytes
                .min(MAX_SNAPSHOT_PAGE_ENCODED_BYTES)
}

#[cfg(test)]
mod tests {
    use super::{
        connection_ensure_live, connection_fail_closed, match_command_response,
        match_query_response,
    };
    use crate::domain::command::CommandReceipt;
    use crate::domain::id::{CommandId, EventId, OperationId, RequestId};
    use crate::domain::query::{QueryError, QueryOutcome, QueryReply};
    use crate::host::IpcError;
    use crate::protocol::ServerResponse;

    fn command_id(tail: u8) -> CommandId {
        let mut bytes = [0_u8; 16];
        bytes[0] = 0x01;
        bytes[6] = 0x70;
        bytes[8] = 0x80;
        bytes[15] = tail;
        CommandId::from_bytes(bytes).expect("command id")
    }

    fn request_id(tail: u8) -> RequestId {
        let mut bytes = [0_u8; 16];
        bytes[0] = 0x01;
        bytes[6] = 0x70;
        bytes[8] = 0x80;
        bytes[15] = tail;
        RequestId::from_bytes(bytes).expect("request id")
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

    #[test]
    fn correlation_mismatch_and_unexpected_response_are_detected() {
        let expected = command_id(0x10);
        let mismatched = ServerResponse::CommandReceipt(CommandReceipt::Accepted {
            command_id: command_id(0x11),
            operation_id: operation_id(0x12),
            task_revision: Some(1),
            event_ids: vec![event_id(0x13)],
        });
        assert!(matches!(
            match_command_response(expected, mismatched),
            Err(IpcError::CorrelationMismatch)
        ));

        let unexpected = ServerResponse::QueryReply(QueryReply {
            request_id: request_id(0x14),
            outcome: QueryOutcome::Err(QueryError::NotFound),
        });
        assert!(matches!(
            match_command_response(expected, unexpected),
            Err(IpcError::UnexpectedResponse)
        ));

        assert!(matches!(
            match_query_response(
                request_id(0x15),
                ServerResponse::QueryReply(QueryReply {
                    request_id: request_id(0x16),
                    outcome: QueryOutcome::Err(QueryError::NotFound),
                })
            ),
            Err(IpcError::CorrelationMismatch)
        ));
    }

    #[test]
    fn fail_closed_poisons_and_blocks_reuse() {
        let mut poisoned = false;
        assert!(connection_ensure_live(poisoned).is_ok());
        let err = connection_fail_closed(&mut poisoned, Err::<(), _>(IpcError::Timeout));
        assert!(matches!(err, Err(IpcError::Timeout)));
        assert!(poisoned);
        assert!(matches!(
            connection_ensure_live(poisoned),
            Err(IpcError::ConnectionPoisoned)
        ));
        let blocked = connection_fail_closed(&mut poisoned, Ok(()));
        // success path does not clear poison; ensure_live remains the gate
        assert!(blocked.is_ok());
        assert!(poisoned);
        assert!(matches!(
            connection_ensure_live(poisoned),
            Err(IpcError::ConnectionPoisoned)
        ));
    }
}
