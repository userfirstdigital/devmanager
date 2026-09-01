//! Asynchronous authenticated Connect client over a caller-supplied socket.
//!
//! Reuses [`ClientConnection`] correlation, deadline, poison, and unsolicited
//! inbox machinery after Noise + Hello. The root fleet opener owns TLS/cookie
//! admission and supplies an already-authenticated WebSocket (or loopback ws).

use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::{Sink, SinkExt, Stream, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::protocol::Message as WsMessage;
use tokio_tungstenite::WebSocketStream;

use crate::connect::{
    connect_prologue, ChannelBinding, ChannelId, ConnectChannelRole, ConnectCredentialPurpose,
    ConnectEnvelope, ConnectLimits, ConnectNoiseCustody, ConnectNoiseHandshakeMessage,
    ConnectNoiseIdentityBinding, ConnectNoiseStaticPublicKey, ConnectPayload, ConnectPrivacyClass,
    ConnectionId, EndToEndChannel, HelloPayload, SessionId, CONNECT_NOISE_FIRST_PAIRING_PATTERN,
    CONNECT_NOISE_PINNED_DEVICE_PATTERN,
};
use crate::domain::command::CommandEnvelope;
use crate::domain::id::{ClientId, RequestId};
use crate::domain::query::QueryEnvelope;
use crate::host::{
    handshake_timeout, request_completion_timeout, supervise_duplex_halves, IpcError,
};
use crate::protocol::{
    Capability, CapabilitySet, ClientRequest, SealedFrame, ServerMessage,
    MAX_HANDSHAKE_MESSAGE_BYTES, MAX_SEALED_FRAME_BYTES, SEALED_NONCE_BYTES,
};

use super::connection::{
    complete_query_waiter_error, dispatch_server_message, finish_shared_connection,
    new_supervisor_handles, poison_mutex, ClientConnection, ConnectAuthenticatedSession,
    ConnectionMetadata, SharedState, SupervisorHandles, UnsolicitedInbox, WriteJob,
};

const CONNECT_WS_GREETING_MAGIC: &[u8; 5] = b"DMCN1";
const CONNECT_WS_GREETING_BYTES: usize = 5 + 16 + 16 + 16;
const CONNECT_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const CONNECT_LOOPBACK_OPEN_TIMEOUT: Duration = Duration::from_secs(15);

/// Caller-owned Connect session parameters. Device static secret is supplied
/// separately at connect time and is never stored in this config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectClientConfig {
    pub expected_host_public_id: [u8; 16],
    pub host_key_pin: ConnectNoiseStaticPublicKey,
    pub requested_capabilities: CapabilitySet,
    pub limits: ConnectLimits,
    /// `true` → Noise XX with a verified host pin (production browser route);
    /// `false` → IK, only for a route explicitly configured to accept IK.
    pub first_pairing: bool,
    pub device_public_id: Option<[u8; 16]>,
    /// Optional Hello client_id request. Assigned Hello reply is authoritative.
    pub requested_client_id: Option<ClientId>,
}

impl ConnectClientConfig {
    /// Browser-fleet capability intersection used by native remote-PC clients.
    pub fn browser_fleet_capabilities() -> CapabilitySet {
        CapabilitySet::from_capabilities([
            Capability::ConnectEncryption,
            Capability::PagedSnapshots,
            Capability::EventReplay,
            Capability::SemanticConversation,
            Capability::ProviderInput,
            Capability::TaskCockpit,
            Capability::BrowserProjection,
        ])
    }

    pub fn for_browser_fleet(
        expected_host_public_id: [u8; 16],
        host_key_pin: ConnectNoiseStaticPublicKey,
        device_public_id: Option<[u8; 16]>,
    ) -> Self {
        Self {
            expected_host_public_id,
            host_key_pin,
            requested_capabilities: Self::browser_fleet_capabilities(),
            limits: ConnectLimits::v1_default(),
            // The production browser listener speaks XX on every connection.
            // Reconnect stays pinned; XX is not implicit trust-on-first-use.
            first_pairing: true,
            device_public_id,
            requested_client_id: None,
        }
    }
}

/// Parsed DMCN1 greeting (host id + prologue route/session).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectGreeting {
    pub host_public_id: [u8; 16],
    pub route_id: [u8; 16],
    pub session_id: [u8; 16],
}

pub fn parse_connect_greeting(bytes: &[u8]) -> Result<ConnectGreeting, IpcError> {
    if bytes.len() != CONNECT_WS_GREETING_BYTES {
        return Err(IpcError::Unauthorized);
    }
    if &bytes[..5] != CONNECT_WS_GREETING_MAGIC {
        return Err(IpcError::Unauthorized);
    }
    let mut host_public_id = [0_u8; 16];
    let mut route_id = [0_u8; 16];
    let mut session_id = [0_u8; 16];
    host_public_id.copy_from_slice(&bytes[5..21]);
    route_id.copy_from_slice(&bytes[21..37]);
    session_id.copy_from_slice(&bytes[37..53]);
    if host_public_id == [0_u8; 16] || route_id == [0_u8; 16] || session_id == [0_u8; 16] {
        return Err(IpcError::Unauthorized);
    }
    let _ = ConnectionId::from_bytes(route_id).map_err(|_| IpcError::Unauthorized)?;
    let _ = SessionId::from_bytes(session_id).map_err(|_| IpcError::Unauthorized)?;
    Ok(ConnectGreeting {
        host_public_id,
        route_id,
        session_id,
    })
}

/// Open a loopback `ws://` Connect route. Caller supplies path and optional
/// pairing cookie; TLS is intentionally out of scope here.
///
/// TCP connect and WebSocket upgrade share one absolute 15s deadline.
pub async fn open_loopback_connect_ws(
    port: u16,
    path: &str,
    cookie_header: Option<&str>,
) -> Result<WebSocketStream<tokio::net::TcpStream>, IpcError> {
    validate_loopback_connect_path(path)?;
    let url = format!("ws://127.0.0.1:{port}{path}");
    let mut request =
        tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(
            url.as_str(),
        )
        .map_err(|_| IpcError::Unauthorized)?;
    // Fail closed if the request URI ever leaves exact loopback after parsing.
    let authority = request.uri().authority().ok_or(IpcError::Unauthorized)?;
    if authority.host() != "127.0.0.1" {
        return Err(IpcError::Unauthorized);
    }
    request.headers_mut().insert(
        tokio_tungstenite::tungstenite::http::header::ORIGIN,
        format!("http://127.0.0.1:{port}")
            .parse()
            .map_err(|_| IpcError::Unauthorized)?,
    );
    if let Some(cookie) = cookie_header {
        let value = cookie.parse().map_err(|_| IpcError::Unauthorized)?;
        request
            .headers_mut()
            .insert(tokio_tungstenite::tungstenite::http::header::COOKIE, value);
    }
    tokio::time::timeout(CONNECT_LOOPBACK_OPEN_TIMEOUT, async {
        let tcp = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .map_err(IpcError::Io)?;
        let (socket, _response) = tokio_tungstenite::client_async(request, tcp)
            .await
            .map_err(|_| IpcError::Unavailable)?;
        Ok(socket)
    })
    .await
    .map_err(|_| IpcError::Timeout)?
}

fn validate_loopback_connect_path(path: &str) -> Result<(), IpcError> {
    if !path.starts_with('/') || path.starts_with("//") {
        return Err(IpcError::Unauthorized);
    }
    if path.contains('\\')
        || path.contains('#')
        || path.contains('@')
        || path.contains("://")
        || path.bytes().any(|byte| byte < 0x20 || byte == 0x7f)
    {
        return Err(IpcError::Unauthorized);
    }
    Ok(())
}

/// Complete Noise + Hello over a caller-authenticated WebSocket, then attach
/// the shared post-Hello supervisor.
pub async fn connect_authenticated<S>(
    socket: WebSocketStream<S>,
    custody: &ConnectNoiseCustody,
    config: &ConnectClientConfig,
) -> Result<ClientConnection, IpcError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (sink, stream) = socket.split();
    connect_authenticated_halves(sink, stream, custody, config).await
}

/// Generic binary-frame halves (tests and non-WS adapters).
pub async fn connect_authenticated_halves<Si, St>(
    mut sink: Si,
    mut stream: St,
    custody: &ConnectNoiseCustody,
    config: &ConnectClientConfig,
) -> Result<ClientConnection, IpcError>
where
    Si: Sink<WsMessage> + Unpin + Send + 'static,
    Si::Error: std::fmt::Display,
    St: Stream<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>>
        + Unpin
        + Send
        + 'static,
{
    let deadline = tokio::time::Instant::now() + handshake_timeout().max(Duration::from_secs(15));
    let negotiated = tokio::time::timeout_at(
        deadline,
        negotiate_connect_session(&mut sink, &mut stream, custody, config),
    )
    .await
    .map_err(|_| IpcError::Timeout)??;

    Ok(spawn_connect_supervisor(
        sink,
        stream,
        negotiated.channel,
        negotiated.session,
        negotiated.binding,
        negotiated.limits,
    ))
}

struct NegotiatedConnect {
    channel: EndToEndChannel,
    session: ConnectAuthenticatedSession,
    binding: ChannelBinding,
    limits: ConnectLimits,
}

async fn negotiate_connect_session<Si, St>(
    sink: &mut Si,
    stream: &mut St,
    custody: &ConnectNoiseCustody,
    config: &ConnectClientConfig,
) -> Result<NegotiatedConnect, IpcError>
where
    Si: Sink<WsMessage> + Unpin,
    Si::Error: std::fmt::Display,
    St: Stream<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    validate_connect_config(config)?;
    let greeting_bytes = recv_connect_binary(stream, CONNECT_WS_GREETING_BYTES).await?;
    let greeting = parse_connect_greeting(&greeting_bytes)?;
    if greeting.host_public_id != config.expected_host_public_id {
        return Err(IpcError::Unauthorized);
    }

    let prologue = connect_prologue(
        ConnectCredentialPurpose::OwnerPairing,
        greeting.route_id,
        greeting.session_id,
    )
    .map_err(|_| IpcError::Unauthorized)?;
    let identity = match config.device_public_id {
        Some(device) => ConnectNoiseIdentityBinding::host_device(greeting.host_public_id, device),
        None => ConnectNoiseIdentityBinding::host(greeting.host_public_id),
    };
    let pattern = if config.first_pairing {
        CONNECT_NOISE_FIRST_PAIRING_PATTERN
    } else {
        CONNECT_NOISE_PINNED_DEVICE_PATTERN
    };
    // Verify XX's responder before sending our claim; IK also needs this pin.
    let expected_remote = Some(config.host_key_pin);
    let mut handshake = EndToEndChannel::open_production_handshake(
        pattern,
        config.first_pairing,
        custody,
        expected_remote,
        prologue,
        ConnectChannelRole::Initiator,
        identity,
        unix_now_secs(),
        true,
        false,
    )
    .map_err(|_| IpcError::Unauthorized)?;

    let first = handshake
        .write_message()
        .map_err(|_| IpcError::Unauthorized)?;
    send_connect_binary(sink, first.encode().map_err(|_| IpcError::Unauthorized)?).await?;

    let second = recv_connect_binary(
        stream,
        usize::try_from(MAX_HANDSHAKE_MESSAGE_BYTES).unwrap_or(usize::MAX),
    )
    .await?;
    handshake
        .read_message(
            &ConnectNoiseHandshakeMessage::decode(&second).map_err(|_| IpcError::Unauthorized)?,
        )
        .map_err(|_| IpcError::Unauthorized)?;

    if !handshake.is_finished() {
        let third = handshake
            .write_message()
            .map_err(|_| IpcError::Unauthorized)?;
        send_connect_binary(sink, third.encode().map_err(|_| IpcError::Unauthorized)?).await?;
    }

    let mut channel = EndToEndChannel::from_noise_transport(
        handshake.finish().map_err(|_| IpcError::Unauthorized)?,
    );
    if !channel.is_production_grade() {
        return Err(IpcError::Unauthorized);
    }
    let peer = channel.authenticated_peer().ok_or(IpcError::Unauthorized)?;
    if peer.static_public() != config.host_key_pin {
        return Err(IpcError::Unauthorized);
    }

    let binding = ChannelBinding::new(
        ConnectionId::from_bytes(greeting.route_id).map_err(|_| IpcError::Unauthorized)?,
        SessionId::from_bytes(greeting.session_id).map_err(|_| IpcError::Unauthorized)?,
        ChannelId::new(),
    );
    channel
        .bind_session(binding)
        .map_err(|_| IpcError::Unauthorized)?;

    let hello = ConnectPayload::Hello(HelloPayload {
        capabilities: config.requested_capabilities,
        limits: config.limits,
        privacy_class: ConnectPrivacyClass::LocalOnly,
        relay_url: None,
        capability_grant: None,
        client_id: config.requested_client_id,
    });
    // Seal advances the send cursor before the await; a failed physical write
    // must not retry this sealed frame.
    let encoded = seal_connect_frame(&mut channel, binding, config.limits, hello, None, None)?;
    send_sealed_binary(sink, encoded).await?;

    let reply_bytes = recv_connect_binary(stream, max_connect_frame_bytes(config.limits)).await?;
    let frame = SealedFrame::decode(&reply_bytes).map_err(|_| IpcError::Unauthorized)?;
    let plaintext = channel
        .open_bytes(&frame, unix_now_secs())
        .map_err(|_| IpcError::Unauthorized)?;
    let envelope = ConnectEnvelope::decode(&plaintext).map_err(|_| IpcError::Unauthorized)?;
    if envelope.sequence != frame.sequence() {
        return Err(IpcError::Unauthorized);
    }
    let reply_binding = envelope.binding().map_err(|_| IpcError::Unauthorized)?;
    if reply_binding != binding {
        return Err(IpcError::Unauthorized);
    }
    channel
        .bind_session(binding)
        .map_err(|_| IpcError::Unauthorized)?;

    let payload = envelope
        .decode_payload()
        .map_err(|_| IpcError::Unauthorized)?;
    let ConnectPayload::Hello(hello) = payload else {
        return Err(IpcError::HelloInconsistent);
    };
    validate_hello_reply(config, &hello, envelope.limits)?;
    let assigned = hello.client_id.ok_or(IpcError::HelloInconsistent)?;
    let session = ConnectAuthenticatedSession::new(
        greeting.host_public_id,
        config.host_key_pin,
        assigned,
        binding,
        hello.capabilities,
        hello.limits,
        None,
    );
    Ok(NegotiatedConnect {
        channel,
        session,
        binding,
        limits: hello.limits,
    })
}

fn validate_connect_config(config: &ConnectClientConfig) -> Result<(), IpcError> {
    if config.expected_host_public_id == [0_u8; 16] {
        return Err(IpcError::Unauthorized);
    }
    config
        .limits
        .validate()
        .map_err(|_| IpcError::HelloInconsistent)?;
    Ok(())
}

fn validate_hello_reply(
    config: &ConnectClientConfig,
    hello: &HelloPayload,
    envelope_limits: ConnectLimits,
) -> Result<(), IpcError> {
    if hello.capability_grant.is_some() {
        return Err(IpcError::HelloInconsistent);
    }
    if hello.capabilities.bits() & !config.requested_capabilities.bits() != 0 {
        return Err(IpcError::HelloInconsistent);
    }
    hello
        .limits
        .validate()
        .map_err(|_| IpcError::HelloInconsistent)?;
    if hello.limits != envelope_limits {
        return Err(IpcError::HelloInconsistent);
    }
    let negotiated = config
        .limits
        .negotiate(hello.limits)
        .map_err(|_| IpcError::HelloInconsistent)?;
    if negotiated != hello.limits {
        return Err(IpcError::HelloInconsistent);
    }
    if matches!(hello.privacy_class, ConnectPrivacyClass::RawContent) {
        return Err(IpcError::HelloInconsistent);
    }
    // Assigned client id is authoritative; requested id is never required to match.
    let _ = hello.client_id.ok_or(IpcError::HelloInconsistent)?;
    Ok(())
}

pub(crate) fn spawn_connect_supervisor<Si, St>(
    sink: Si,
    stream: St,
    channel: EndToEndChannel,
    session: ConnectAuthenticatedSession,
    binding: ChannelBinding,
    limits: ConnectLimits,
) -> ClientConnection
where
    Si: Sink<WsMessage> + Unpin + Send + 'static,
    Si::Error: std::fmt::Display,
    St: Stream<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>>
        + Unpin
        + Send
        + 'static,
{
    let client_id = session.assigned_client_id();
    let metadata = ConnectionMetadata::Connect(session);
    let SupervisorHandles {
        state,
        unsolicited,
        write_tx,
        write_rx,
    } = new_supervisor_handles();
    let channel = Arc::new(Mutex::new(channel));
    let reader_state = Arc::clone(&state);
    let terminal_state = Arc::clone(&state);
    let reader_unsolicited = Arc::clone(&unsolicited);
    let terminal_unsolicited = Arc::clone(&unsolicited);
    let writer_channel = Arc::clone(&channel);
    let reader_channel = Arc::clone(&channel);

    let io_task = tokio::spawn(async move {
        let writer = write_connect_half(sink, writer_channel, write_rx, binding, limits);
        let reader = read_connect_half(
            stream,
            reader_channel,
            reader_state,
            reader_unsolicited,
            binding,
            limits,
        );
        let _ = supervise_duplex_halves(reader, writer).await;
        terminal_unsolicited.close();
        poison_mutex(&terminal_state);
    });

    finish_shared_connection(client_id, metadata, state, unsolicited, write_tx, io_task)
}

async fn write_connect_half<Si>(
    mut sink: Si,
    channel: Arc<Mutex<EndToEndChannel>>,
    mut write_rx: mpsc::Receiver<WriteJob>,
    binding: ChannelBinding,
    limits: ConnectLimits,
) -> Result<(), IpcError>
where
    Si: Sink<WsMessage> + Unpin,
    Si::Error: std::fmt::Display,
{
    while let Some(job) = write_rx.recv().await {
        let (payload, request_id, operation_id) = client_request_to_payload(job.request)?;
        // Seal under a short synchronous lock, then drop before any await.
        // Sequence advances inside seal; a later write failure poisons and
        // must never retry the already-sealed frame.
        let encoded = {
            let mut channel = channel.lock().map_err(|_| IpcError::Unavailable)?;
            seal_connect_frame(
                &mut channel,
                binding,
                limits,
                payload,
                request_id,
                operation_id,
            )?
        };
        send_sealed_binary(&mut sink, encoded).await?;
    }
    Err(IpcError::Unavailable)
}

async fn read_connect_half<St>(
    mut stream: St,
    channel: Arc<Mutex<EndToEndChannel>>,
    state: Arc<Mutex<SharedState>>,
    unsolicited: Arc<UnsolicitedInbox>,
    expected_binding: ChannelBinding,
    negotiated_limits: ConnectLimits,
) -> Result<(), IpcError>
where
    St: Stream<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    let max_bytes = max_connect_frame_bytes(negotiated_limits);
    while let Some(message) = stream.next().await {
        let message = message.map_err(|error| {
            IpcError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                error.to_string(),
            ))
        })?;
        match message {
            WsMessage::Ping(_) | WsMessage::Pong(_) => continue,
            WsMessage::Close(_) => return Ok(()),
            WsMessage::Text(_) | WsMessage::Frame(_) => return Err(IpcError::Unauthorized),
            WsMessage::Binary(bytes) => {
                if bytes.is_empty() || bytes.len() > max_bytes {
                    return Err(IpcError::Unauthorized);
                }
                let frame = SealedFrame::decode(&bytes).map_err(|_| IpcError::Unauthorized)?;
                let envelope = {
                    let mut channel = channel.lock().map_err(|_| IpcError::Unavailable)?;
                    let plaintext = channel
                        .open_bytes(&frame, unix_now_secs())
                        .map_err(|_| IpcError::Unauthorized)?;
                    let envelope =
                        ConnectEnvelope::decode_with_limits(&plaintext, negotiated_limits)
                            .map_err(|_| IpcError::Unauthorized)?;
                    if envelope.sequence != frame.sequence() {
                        return Err(IpcError::Unauthorized);
                    }
                    let binding = envelope.binding().map_err(|_| IpcError::Unauthorized)?;
                    if binding != expected_binding {
                        return Err(IpcError::Unauthorized);
                    }
                    if envelope.limits != negotiated_limits {
                        return Err(IpcError::Unauthorized);
                    }
                    channel
                        .bind_session(binding)
                        .map_err(|_| IpcError::Unauthorized)?;
                    envelope
                };
                let payload = envelope
                    .decode_payload()
                    .map_err(|_| IpcError::Unauthorized)?;
                dispatch_connect_payload(&state, &unsolicited, payload, envelope.request_id)
                    .await?;
            }
        }
    }
    Ok(())
}

async fn dispatch_connect_payload(
    state: &Arc<Mutex<SharedState>>,
    unsolicited: &UnsolicitedInbox,
    payload: ConnectPayload,
    envelope_request_id: Option<RequestId>,
) -> Result<(), IpcError> {
    match payload {
        ConnectPayload::QueryReply(reply) => {
            dispatch_server_message(state, unsolicited, ServerMessage::QueryReply(reply)).await
        }
        ConnectPayload::CommandReceipt(receipt) => {
            dispatch_server_message(state, unsolicited, ServerMessage::CommandReceipt(receipt))
                .await
        }
        ConnectPayload::HostDurableOutput(host)
        | ConnectPayload::HostCriticalOutput(host)
        | ConnectPayload::HostStreamOutput(host)
        | ConnectPayload::HostConversationOutput(host) => {
            dispatch_server_message(state, unsolicited, host.message).await
        }
        ConnectPayload::Error(error) => {
            let request_id = error.request_id.or(envelope_request_id);
            let Some(request_id) = request_id else {
                return Err(IpcError::Unauthorized);
            };
            complete_query_waiter_error(state, request_id, IpcError::Unauthorized)
        }
        ConnectPayload::Hello(_)
        | ConnectPayload::Capabilities(_)
        | ConnectPayload::Query(_)
        | ConnectPayload::Command(_)
        | ConnectPayload::SnapshotPage(_)
        | ConnectPayload::EventPage(_)
        | ConnectPayload::OperationSettlement(_)
        | ConnectPayload::Presence(_)
        | ConnectPayload::TerminalDelta(_)
        | ConnectPayload::BrowserFrame(_)
        | ConnectPayload::PromptExtension(_)
        | ConnectPayload::BrowserExtension(_)
        | ConnectPayload::Chunk(_)
        | ConnectPayload::Resync(_)
        | ConnectPayload::Extension(_) => Err(IpcError::Unauthorized),
    }
}

fn client_request_to_payload(
    request: ClientRequest,
) -> Result<
    (
        ConnectPayload,
        Option<RequestId>,
        Option<crate::domain::id::OperationId>,
    ),
    IpcError,
> {
    match request {
        ClientRequest::Query(envelope) => {
            let request_id = envelope.request_id;
            Ok((ConnectPayload::Query(envelope), Some(request_id), None))
        }
        ClientRequest::Command(envelope) => Ok((ConnectPayload::Command(envelope), None, None)),
        ClientRequest::Detach(_) | ClientRequest::TerminalInput(_) => Err(IpcError::Unsupported),
    }
}

/// Synchronously seal one Connect envelope. Advances the channel send cursor.
/// Callers must not retry the returned bytes after a failed physical write.
fn seal_connect_frame(
    channel: &mut EndToEndChannel,
    binding: ChannelBinding,
    limits: ConnectLimits,
    payload: ConnectPayload,
    request_id: Option<RequestId>,
    operation_id: Option<crate::domain::id::OperationId>,
) -> Result<Vec<u8>, IpcError> {
    let mut nonce = [0_u8; SEALED_NONCE_BYTES];
    getrandom::fill(&mut nonce).map_err(|_| IpcError::Unavailable)?;
    let sequence = channel.next_send_sequence();
    let envelope = ConnectEnvelope::new(
        binding,
        payload.channel(),
        sequence,
        request_id,
        operation_id,
        limits,
        ConnectPrivacyClass::LocalOnly,
        payload,
    )
    .map_err(|_| IpcError::Unauthorized)?;
    let sealed = channel
        .seal(&envelope, nonce, unix_now_secs())
        .map_err(|_| IpcError::Unauthorized)?;
    let encoded = sealed.encode().map_err(|_| IpcError::Unauthorized)?;
    if encoded.len() > max_connect_frame_bytes(limits) {
        return Err(IpcError::Unauthorized);
    }
    Ok(encoded)
}

async fn send_sealed_binary<Si>(sink: &mut Si, encoded: Vec<u8>) -> Result<(), IpcError>
where
    Si: Sink<WsMessage> + Unpin,
    Si::Error: std::fmt::Display,
{
    tokio::time::timeout(
        CONNECT_WRITE_TIMEOUT.max(request_completion_timeout()),
        async {
            sink.feed(WsMessage::Binary(encoded))
                .await
                .map_err(|error| {
                    IpcError::Io(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        error.to_string(),
                    ))
                })?;
            sink.flush().await.map_err(|error| {
                IpcError::Io(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    error.to_string(),
                ))
            })?;
            Ok(())
        },
    )
    .await
    .map_err(|_| IpcError::Timeout)?
}

async fn send_connect_binary<Si>(sink: &mut Si, bytes: Vec<u8>) -> Result<(), IpcError>
where
    Si: Sink<WsMessage> + Unpin,
    Si::Error: std::fmt::Display,
{
    if bytes.is_empty() {
        return Err(IpcError::Unauthorized);
    }
    tokio::time::timeout(CONNECT_WRITE_TIMEOUT, async {
        sink.send(WsMessage::Binary(bytes)).await.map_err(|error| {
            IpcError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                error.to_string(),
            ))
        })
    })
    .await
    .map_err(|_| IpcError::Timeout)?
}

async fn recv_connect_binary<St>(stream: &mut St, max_bytes: usize) -> Result<Vec<u8>, IpcError>
where
    St: Stream<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        let message = stream
            .next()
            .await
            .ok_or(IpcError::Unavailable)?
            .map_err(|error| {
                IpcError::Io(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    error.to_string(),
                ))
            })?;
        match message {
            WsMessage::Ping(_) | WsMessage::Pong(_) => {}
            WsMessage::Binary(bytes) => {
                if bytes.is_empty() || bytes.len() > max_bytes {
                    return Err(IpcError::Unauthorized);
                }
                return Ok(bytes);
            }
            WsMessage::Text(_) | WsMessage::Close(_) | WsMessage::Frame(_) => {
                return Err(IpcError::Unauthorized);
            }
        }
    }
}

fn max_connect_frame_bytes(limits: ConnectLimits) -> usize {
    let negotiated = usize::try_from(limits.max_physical_frame_bytes).unwrap_or(usize::MAX);
    let sealed_max = usize::try_from(MAX_SEALED_FRAME_BYTES).unwrap_or(usize::MAX);
    negotiated.min(sealed_max)
}

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(1)
        .max(1)
}

pub fn connect_query_request(envelope: QueryEnvelope) -> ClientRequest {
    ClientRequest::Query(envelope)
}

pub fn connect_command_request(envelope: CommandEnvelope) -> ClientRequest {
    ClientRequest::Command(envelope)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::UnsolicitedServerMessage;
    use crate::connect::{
        ConnectDispatchSession, ConnectIdentityLiveState, ConnectSessionDisposition,
    };
    use crate::domain::command::{Command, CommandEnvelope, CommandReceipt, RejectionCode};
    use crate::domain::event::{DomainEvent, Event};
    use crate::domain::id::{CommandId, EventId, SubscriptionId, TaskId};
    use crate::domain::query::{Query, QueryEnvelope, QueryOutcome, QueryReply};
    use crate::protocol::Capability;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    fn fixture_uuid(tail: u8) -> [u8; 16] {
        let mut bytes = [0_u8; 16];
        bytes[0] = 0x01;
        bytes[1] = 0x23;
        bytes[2] = 0x45;
        bytes[3] = 0x67;
        bytes[4] = 0x89;
        bytes[5] = 0xab;
        bytes[6] = 0x70;
        bytes[8] = 0x80;
        bytes[15] = tail;
        bytes
    }

    fn production_pair(
        host_id: [u8; 16],
        route: [u8; 16],
        session: [u8; 16],
    ) -> (
        ConnectNoiseCustody,
        ConnectNoiseCustody,
        EndToEndChannel,
        EndToEndChannel,
    ) {
        let initiator = ConnectNoiseCustody::generate().expect("initiator");
        let responder = ConnectNoiseCustody::generate().expect("responder");
        let prologue = connect_prologue(ConnectCredentialPurpose::OwnerPairing, route, session)
            .expect("prologue");
        let mut init_hs = EndToEndChannel::open_production_handshake(
            CONNECT_NOISE_FIRST_PAIRING_PATTERN,
            true,
            &initiator,
            None,
            prologue,
            ConnectChannelRole::Initiator,
            ConnectNoiseIdentityBinding::host(host_id),
            unix_now_secs(),
            true,
            false,
        )
        .expect("initiator hs");
        let mut resp_hs = EndToEndChannel::open_production_handshake(
            CONNECT_NOISE_FIRST_PAIRING_PATTERN,
            true,
            &responder,
            None,
            prologue,
            ConnectChannelRole::Responder,
            ConnectNoiseIdentityBinding::host(host_id),
            unix_now_secs(),
            true,
            false,
        )
        .expect("responder hs");
        let first = init_hs.write_message().expect("first");
        resp_hs.read_message(&first).expect("read first");
        let second = resp_hs.write_message().expect("second");
        init_hs.read_message(&second).expect("read second");
        let third = init_hs.write_message().expect("third");
        resp_hs.read_message(&third).expect("read third");
        (
            initiator,
            responder,
            EndToEndChannel::from_noise_transport(init_hs.finish().expect("init finish")),
            EndToEndChannel::from_noise_transport(resp_hs.finish().expect("resp finish")),
        )
    }

    struct ChannelSink {
        tx: mpsc::UnboundedSender<WsMessage>,
    }

    impl Sink<WsMessage> for ChannelSink {
        type Error = std::io::Error;

        fn poll_ready(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn start_send(self: Pin<&mut Self>, item: WsMessage) -> Result<(), Self::Error> {
            self.tx
                .send(item)
                .map_err(|_| std::io::Error::from(std::io::ErrorKind::BrokenPipe))
        }

        fn poll_flush(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
    }

    struct ChannelStream {
        rx: mpsc::UnboundedReceiver<WsMessage>,
    }

    impl Stream for ChannelStream {
        type Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>;

        fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            match self.rx.poll_recv(cx) {
                Poll::Ready(Some(message)) => Poll::Ready(Some(Ok(message))),
                Poll::Ready(None) => Poll::Ready(None),
                Poll::Pending => Poll::Pending,
            }
        }
    }

    fn seal_payload(
        channel: &mut EndToEndChannel,
        binding: ChannelBinding,
        limits: ConnectLimits,
        payload: ConnectPayload,
        request_id: Option<RequestId>,
    ) -> Vec<u8> {
        let nonce = [7_u8; SEALED_NONCE_BYTES];
        let sequence = channel.next_send_sequence();
        let envelope = ConnectEnvelope::new(
            binding,
            payload.channel(),
            sequence,
            request_id,
            None,
            limits,
            ConnectPrivacyClass::LocalOnly,
            payload,
        )
        .expect("envelope");
        channel
            .seal(&envelope, nonce, unix_now_secs())
            .expect("seal")
            .encode()
            .expect("encode")
    }

    fn attach_pair(
        host_id_tail: u8,
    ) -> (
        ClientConnection,
        EndToEndChannel,
        ChannelBinding,
        ConnectLimits,
        ClientId,
        mpsc::UnboundedSender<WsMessage>,
        mpsc::UnboundedReceiver<WsMessage>,
    ) {
        let host_id = fixture_uuid(host_id_tail);
        let route = ConnectionId::new().as_bytes();
        let session = SessionId::new().as_bytes();
        let (_device, host_custody, mut client_channel, mut host_channel) =
            production_pair(host_id, route, session);
        let binding = ChannelBinding::new(
            ConnectionId::from_bytes(route).unwrap(),
            SessionId::from_bytes(session).unwrap(),
            ChannelId::new(),
        );
        client_channel.bind_session(binding).unwrap();
        host_channel.bind_session(binding).unwrap();
        let limits = ConnectLimits::v1_default();
        let assigned = ClientId::new();
        let (to_client_tx, to_client_rx) = mpsc::unbounded_channel();
        let (from_client_tx, from_client_rx) = mpsc::unbounded_channel();
        let connection = spawn_connect_supervisor(
            ChannelSink { tx: from_client_tx },
            ChannelStream { rx: to_client_rx },
            client_channel,
            ConnectAuthenticatedSession::new(
                host_id,
                host_custody.public(),
                assigned,
                binding,
                ConnectClientConfig::browser_fleet_capabilities(),
                limits,
                None,
            ),
            binding,
            limits,
        );
        (
            connection,
            host_channel,
            binding,
            limits,
            assigned,
            to_client_tx,
            from_client_rx,
        )
    }

    #[test]
    fn parse_greeting_rejects_wrong_magic_zeros_and_oversize() {
        let mut bytes = vec![0_u8; CONNECT_WS_GREETING_BYTES];
        bytes[..5].copy_from_slice(b"DMCN1");
        bytes[5..21].copy_from_slice(&ConnectionId::new().as_bytes());
        bytes[21..37].copy_from_slice(&ConnectionId::new().as_bytes());
        bytes[37..53].copy_from_slice(&SessionId::new().as_bytes());
        assert!(parse_connect_greeting(&bytes).is_ok());
        bytes[0] = b'X';
        assert!(parse_connect_greeting(&bytes).is_err());
        bytes[0] = b'D';
        bytes[5..21].fill(0);
        assert!(parse_connect_greeting(&bytes).is_err());
        assert!(parse_connect_greeting(&vec![0_u8; CONNECT_WS_GREETING_BYTES + 1]).is_err());
    }

    #[test]
    fn hello_capabilities_must_be_subset() {
        let pin = ConnectNoiseCustody::generate().expect("pin").public();
        let config = ConnectClientConfig {
            expected_host_public_id: fixture_uuid(0x41),
            host_key_pin: pin,
            requested_capabilities: CapabilitySet::from_capabilities([Capability::PagedSnapshots]),
            limits: ConnectLimits::v1_default(),
            first_pairing: true,
            device_public_id: None,
            requested_client_id: None,
        };
        let hello = HelloPayload {
            capabilities: CapabilitySet::from_capabilities([
                Capability::PagedSnapshots,
                Capability::EventReplay,
            ]),
            limits: ConnectLimits::v1_default(),
            privacy_class: ConnectPrivacyClass::LocalOnly,
            relay_url: None,
            capability_grant: None,
            client_id: Some(ClientId::new()),
        };
        assert!(matches!(
            validate_hello_reply(&config, &hello, ConnectLimits::v1_default()),
            Err(IpcError::HelloInconsistent)
        ));
    }

    #[test]
    fn assigned_client_id_is_authoritative_when_request_differs() {
        let pin = ConnectNoiseCustody::generate().expect("pin").public();
        let requested = ClientId::new();
        let assigned = ClientId::new();
        assert_ne!(requested, assigned);
        let config = ConnectClientConfig {
            expected_host_public_id: fixture_uuid(0x42),
            host_key_pin: pin,
            requested_capabilities: ConnectClientConfig::browser_fleet_capabilities(),
            limits: ConnectLimits::v1_default(),
            first_pairing: true,
            device_public_id: None,
            requested_client_id: Some(requested),
        };
        let hello = HelloPayload {
            capabilities: CapabilitySet::from_capabilities([Capability::PagedSnapshots]),
            limits: ConnectLimits::v1_default(),
            privacy_class: ConnectPrivacyClass::LocalOnly,
            relay_url: None,
            capability_grant: None,
            client_id: Some(assigned),
        };
        assert!(validate_hello_reply(&config, &hello, ConnectLimits::v1_default()).is_ok());
    }

    #[test]
    fn wrong_host_pin_detected_against_authenticated_peer() {
        let host_id = fixture_uuid(0x51);
        let route = ConnectionId::new().as_bytes();
        let session = SessionId::new().as_bytes();
        let (_device, host_custody, client_channel, _host) =
            production_pair(host_id, route, session);
        let stranger = ConnectNoiseCustody::generate().expect("stranger").public();
        let peer = client_channel.authenticated_peer().unwrap().static_public();
        assert_ne!(peer, stranger);
        assert_eq!(peer, host_custody.public());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn query_with_unsolicited_wake_before_reply() {
        let (
            connection,
            mut host_channel,
            binding,
            limits,
            assigned,
            to_client_tx,
            mut from_client_rx,
        ) = attach_pair(0x61);

        let sub = SubscriptionId::from_bytes(fixture_uuid(0x71)).unwrap();
        let durable = ConnectPayload::from_host_output(
            ServerMessage::DurableEvent {
                subscription_id: sub,
                event: DomainEvent {
                    id: EventId::from_bytes(fixture_uuid(0x55)).unwrap(),
                    task_id: Some(TaskId::from_bytes(fixture_uuid(0x43)).unwrap()),
                    sequence: 3,
                    task_revision: Some(1),
                    occurred_at_ms: 1,
                    payload: Event::TaskReopened,
                },
            },
            CapabilitySet::from_capabilities([Capability::EventReplay]),
        )
        .unwrap();
        to_client_tx
            .send(WsMessage::Binary(seal_payload(
                &mut host_channel,
                binding,
                limits,
                durable,
                None,
            )))
            .unwrap();

        let request_id = RequestId::new();
        let query_task = tokio::spawn({
            let connection = connection.clone();
            async move {
                connection
                    .query(QueryEnvelope {
                        request_id,
                        client_id: assigned,
                        task_id: None,
                        query: Query::InspectHostQuit,
                    })
                    .await
            }
        });

        let outbound = from_client_rx.recv().await.expect("client query frame");
        let WsMessage::Binary(outbound) = outbound else {
            panic!("binary");
        };
        let frame = SealedFrame::decode(&outbound).unwrap();
        let plaintext = host_channel.open_bytes(&frame, unix_now_secs()).unwrap();
        let envelope = ConnectEnvelope::decode_with_limits(&plaintext, limits).unwrap();
        assert_eq!(envelope.binding().unwrap(), binding);
        assert_eq!(envelope.sequence, frame.sequence());
        let ConnectPayload::Query(query) = envelope.decode_payload().unwrap() else {
            panic!("query");
        };
        assert_eq!(query.request_id, request_id);

        let wake = connection.recv_unsolicited().await.expect("wake");
        assert!(matches!(
            wake,
            UnsolicitedServerMessage::DurableEvent { .. }
        ));

        to_client_tx
            .send(WsMessage::Binary(seal_payload(
                &mut host_channel,
                binding,
                limits,
                ConnectPayload::QueryReply(QueryReply {
                    request_id,
                    outcome: QueryOutcome::Err(crate::domain::query::QueryError::Unauthorized),
                }),
                Some(request_id),
            )))
            .unwrap();
        let reply = query_task.await.expect("join").expect("query");
        assert!(matches!(reply.outcome, QueryOutcome::Err(_)));
        drop(connection);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn command_receipt_round_trip() {
        let (
            connection,
            mut host_channel,
            binding,
            limits,
            assigned,
            to_client_tx,
            mut from_client_rx,
        ) = attach_pair(0x62);
        let command_id = CommandId::new();
        let receipt = CommandReceipt::Rejected {
            command_id,
            code: RejectionCode::UnsupportedCapability,
            current_revision: None,
            resolution: None,
        };
        let cmd_task = tokio::spawn({
            let connection = connection.clone();
            async move {
                connection
                    .execute_command(CommandEnvelope {
                        command_id,
                        client_id: assigned,
                        task_id: None,
                        issued_at_ms: 1,
                        expected_task_revision: None,
                        command: Command::AbortUpdateHandoff,
                    })
                    .await
            }
        });
        let _ = from_client_rx.recv().await.expect("command frame");
        to_client_tx
            .send(WsMessage::Binary(seal_payload(
                &mut host_channel,
                binding,
                limits,
                ConnectPayload::CommandReceipt(receipt.clone()),
                None,
            )))
            .unwrap();
        assert_eq!(cmd_task.await.expect("join").expect("receipt"), receipt);
        drop(connection);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn foreign_binding_poisons_connection() {
        let (connection, mut host_channel, _binding, limits, _assigned, to_client_tx, _from) =
            attach_pair(0x63);
        let foreign = ChannelBinding::new(ConnectionId::new(), SessionId::new(), ChannelId::new());
        let bad = seal_payload(
            &mut host_channel,
            foreign,
            limits,
            ConnectPayload::QueryReply(QueryReply {
                request_id: RequestId::new(),
                outcome: QueryOutcome::Err(crate::domain::query::QueryError::Unauthorized),
            }),
            None,
        );
        to_client_tx.send(WsMessage::Binary(bad)).unwrap();
        for _ in 0..40 {
            if connection.is_poisoned() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(connection.is_poisoned());
        drop(connection);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn conversation_dirty_coalesces_to_high_water() {
        let (connection, mut host_channel, binding, limits, _assigned, to_client_tx, _from) =
            attach_pair(0x64);
        let sub = SubscriptionId::from_bytes(fixture_uuid(0x91)).unwrap();
        let task = TaskId::from_bytes(fixture_uuid(0x92)).unwrap();
        for high_water in [1_u64, 3, 2] {
            let dirty = ConnectPayload::from_host_output(
                ServerMessage::ConversationDirty {
                    subscription_id: sub,
                    task_id: task,
                    high_water,
                },
                CapabilitySet::from_capabilities([
                    Capability::TaskCockpit,
                    Capability::SemanticConversation,
                ]),
            )
            .unwrap();
            to_client_tx
                .send(WsMessage::Binary(seal_payload(
                    &mut host_channel,
                    binding,
                    limits,
                    dirty,
                    None,
                )))
                .unwrap();
        }
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(15)).await;
        let message = connection.recv_unsolicited().await.expect("dirty");
        match message {
            UnsolicitedServerMessage::ConversationDirty {
                subscription_id,
                task_id,
                high_water,
            } => {
                assert_eq!(subscription_id, sub);
                assert_eq!(task_id, task);
                assert_eq!(high_water, 3);
            }
            other => panic!("expected ConversationDirty, got {other:?}"),
        }
        drop(connection);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn drop_terminates_connect_supervisor() {
        let (connection, _host, _binding, _limits, _assigned, to_client_tx, _from) =
            attach_pair(0x65);
        drop(connection);
        drop(to_client_tx);
        tokio::task::yield_now().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn oversized_inbound_frame_poisons() {
        let (connection, _host, _binding, _limits, _assigned, to_client_tx, _from) =
            attach_pair(0x66);
        let oversized = vec![0_u8; max_connect_frame_bytes(ConnectLimits::v1_default()) + 1];
        to_client_tx.send(WsMessage::Binary(oversized)).unwrap();
        for _ in 0..40 {
            if connection.is_poisoned() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(connection.is_poisoned());
        drop(connection);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn hello_dispatch_assigns_client_id() {
        let assigned = ClientId::new();
        let binding = ChannelBinding::new(ConnectionId::new(), SessionId::new(), ChannelId::new());
        let limits = ConnectLimits::v1_default();
        let mut dispatch =
            ConnectDispatchSession::bind_paired("pair".into(), ConnectIdentityLiveState::Live)
                .with_assigned_client_id(assigned);
        let hello = ConnectPayload::Hello(HelloPayload {
            capabilities: ConnectClientConfig::browser_fleet_capabilities(),
            limits,
            privacy_class: ConnectPrivacyClass::LocalOnly,
            relay_url: None,
            capability_grant: None,
            client_id: None,
        });
        let env = ConnectEnvelope::new(
            binding,
            hello.channel(),
            1,
            None,
            None,
            limits,
            ConnectPrivacyClass::LocalOnly,
            hello.clone(),
        )
        .unwrap();
        let (reply, disposition) = dispatch.handle_payload(&env, hello, None).await;
        assert_eq!(disposition, ConnectSessionDisposition::Continue);
        let ConnectPayload::Hello(hello_reply) = reply else {
            panic!("hello");
        };
        assert_eq!(hello_reply.client_id, Some(assigned));
    }
}
