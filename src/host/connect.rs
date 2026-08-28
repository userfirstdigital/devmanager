//! Host-owned encrypted Connect duplex transport.
//!
//! Serves an authenticated Hello-negotiated WebSocket using
//! [`HostConnectDuplex`], [`ConnectionOutputPorts`] /
//! [`PrioritizedOutbound`], and [`supervise_duplex_halves`]. Does not create a
//! second executor, store, output queue, or handshake.
//!
//! # Preconditions (enforced)
//! - Production-grade Noise [`EndToEndChannel`]
//! - Bound nonempty paired Connect identity on the dispatch session
//! - Exact `ClientId` match between dispatch binding and [`HostConnectDuplex`]
//! - Completed Hello negotiation (limits, capabilities, channel binding)
//!
//! # Limitations
//! Physical-quit receipt routing and update handoff are not supported on this
//! transport. `ConfirmHostQuit`, `PrepareUpdate`, and related update mutating
//! commands are denied at the bound request-port boundary before host
//! invocation (`IpcError::Unsupported`).

use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use axum::extract::ws::{Message as WsMessage, WebSocket};
use futures_util::{Sink, SinkExt, Stream, StreamExt};
use tokio::sync::mpsc;

use crate::client::ConnectHostCommandPort;
use crate::connect::{
    ChannelBinding, ConnectDispatchSession, ConnectEnvelope, ConnectLimits, ConnectPayload,
    ConnectPrivacyClass, ConnectSessionDisposition, EndToEndChannel, HostOutputLane,
};
use crate::domain::cockpit::TaskCockpitQuery;
use crate::domain::command::Command;
use crate::domain::id::{ClientId, OperationId, RequestId};
use crate::domain::query::Query;
use crate::protocol::{
    Capability, CapabilitySet, ClientRequest, NegotiatedParameters, SealedFrame, ServerMessage,
    StreamPayloadKind, MAX_SEALED_FRAME_BYTES, SEALED_NONCE_BYTES,
};

use super::connection::{
    ConnectionOutputPorts, HostConnectDuplex, HostRequestHandle, PrioritizedOutbound,
};
use super::ipc::{
    agent_connection_query_timeout, supervise_duplex_halves, task_cockpit_query_timeout, IpcError,
};

const SOCKET_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const REPLY_QUEUE_CAPACITY: usize = 8;

/// Serve an authenticated Hello-negotiated Connect WebSocket with a host-owned
/// duplex session. Does not perform Noise handshake, Hello negotiation,
/// listener bind, or executor construction.
///
/// `peer` must already carry cookie-pin authorization and, for Device-kind
/// enrollment, the identity authority invalidation watch so revoke/repair/host
/// rotation wakes idle duplex cancellation without polling.
pub(crate) async fn serve_host_connect_duplex(
    socket: WebSocket,
    channel: EndToEndChannel,
    dispatch: ConnectDispatchSession,
    duplex: HostConnectDuplex,
    mut peer: crate::remote::ConnectPeerLease,
) -> Result<(), IpcError> {
    let (sink, stream) = socket.split();
    if !peer.is_authorized() || peer.client_id() != duplex.client_id {
        return Err(IpcError::Unauthorized);
    }
    let authorization = Some(Arc::new(peer.clone()));
    tokio::select! {
        biased;
        _ = peer.revoked() => Err(IpcError::Unauthorized),
        result = serve_connect_io(sink, stream, channel, dispatch, duplex, authorization) => result,
    }
}

/// The production ownership path is generic only over the socket halves so
/// deterministic transport tests exercise the same registration and writer.
async fn serve_connect_io<S, R>(
    sink: S,
    stream: R,
    channel: EndToEndChannel,
    dispatch: ConnectDispatchSession,
    duplex: HostConnectDuplex,
    peer: Option<Arc<crate::remote::ConnectPeerLease>>,
) -> Result<(), IpcError>
where
    S: Sink<WsMessage> + Unpin,
    S::Error: std::fmt::Display,
    R: Stream<Item = Result<WsMessage, axum::Error>> + Unpin,
{
    let (binding, limits, capabilities) =
        enforce_connect_duplex_preconditions(&channel, &dispatch, duplex.client_id)?;

    // Registration stays in this outer scope until both halves finish so drop
    // still unregisters through the existing guard after supervision returns.
    let HostConnectDuplex {
        client_id: _,
        requests,
        output,
        ports,
        registration,
    } = duplex;
    let _registration_guard = registration;

    let channel = Arc::new(Mutex::new(channel));
    let (reply_tx, reply_rx) = mpsc::channel::<PendingReply>(REPLY_QUEUE_CAPACITY);
    let bound_port = ConnectDuplexBoundPort {
        inner: requests,
        peer,
    };
    let mut shutdown_rx = output.subscribe_shutdown();

    let reader = read_connect_half(
        stream,
        Arc::clone(&channel),
        dispatch,
        bound_port,
        reply_tx,
        binding,
        limits,
    );
    let writer = write_connect_half(
        sink,
        channel,
        ports,
        reply_rx,
        binding,
        limits,
        capabilities,
        &mut shutdown_rx,
    );

    supervise_duplex_halves(reader, writer).await
}

/// Enforce production Noise, paired identity, exact client binding, and Hello.
pub(crate) fn enforce_connect_duplex_preconditions(
    channel: &EndToEndChannel,
    dispatch: &ConnectDispatchSession,
    duplex_client_id: ClientId,
) -> Result<(ChannelBinding, ConnectLimits, CapabilitySet), IpcError> {
    if !channel.is_production_grade() {
        return Err(IpcError::Unauthorized);
    }
    if !dispatch.paired_identity_bound() {
        return Err(IpcError::Unauthorized);
    }
    let Some(bound_client_id) = dispatch.bound_client_id() else {
        return Err(IpcError::Unauthorized);
    };
    if bound_client_id != duplex_client_id {
        return Err(IpcError::Unauthorized);
    }
    let Some(binding) = dispatch.channel_binding() else {
        return Err(IpcError::Unauthorized);
    };
    if binding.connection_id.as_bytes() != channel.prologue().route_id() {
        return Err(IpcError::Unauthorized);
    }
    let Some(limits) = dispatch.negotiated_limits() else {
        return Err(IpcError::Unauthorized);
    };
    let Some(capabilities) = dispatch.negotiated_capabilities() else {
        return Err(IpcError::Unauthorized);
    };
    channel
        .bind_session(binding)
        .map_err(|_| IpcError::Unauthorized)?;
    Ok((binding, limits, capabilities))
}

fn max_connect_frame_bytes(limits: ConnectLimits) -> usize {
    let negotiated = usize::try_from(limits.max_physical_frame_bytes).unwrap_or(usize::MAX);
    let sealed_max = usize::try_from(MAX_SEALED_FRAME_BYTES).unwrap_or(usize::MAX);
    negotiated.min(sealed_max)
}

struct PendingReply {
    payload: ConnectPayload,
    request_id: Option<RequestId>,
    operation_id: Option<OperationId>,
    close_after_flush: Option<tokio::sync::oneshot::Sender<()>>,
}

/// Bound request port for the duplex engine.
///
/// Denies physical-quit and update-handoff mutating commands before they reach
/// the host executor. Accepted ConfirmHostQuit receipt ownership must not be
/// weakened by this transport; those commands stay unsupported here.
struct ConnectDuplexBoundPort {
    inner: HostRequestHandle,
    peer: Option<Arc<crate::remote::ConnectPeerLease>>,
}

#[async_trait]
impl ConnectHostCommandPort for ConnectDuplexBoundPort {
    async fn execute(
        &self,
        negotiated: NegotiatedParameters,
        request: ClientRequest,
    ) -> Result<ServerMessage, IpcError> {
        // A command admitted before revocation may finish; no later request
        // can be admitted merely because its old socket remains open.
        if self.peer.as_ref().is_some_and(|peer| !peer.is_authorized()) {
            return Err(IpcError::Unauthorized);
        }
        if denies_physical_quit_or_update(&request) {
            return Err(IpcError::Unsupported);
        }
        let timeout = command_completion_deadline(&request);
        tokio::time::timeout(timeout, self.inner.execute(negotiated, request))
            .await
            .map_err(|_| IpcError::Timeout)?
    }

    async fn open_duplex(
        &self,
        _client_id: ClientId,
    ) -> Result<Option<HostConnectDuplex>, IpcError> {
        Ok(None)
    }
}

fn denies_physical_quit_or_update(request: &ClientRequest) -> bool {
    match request {
        ClientRequest::Command(envelope) => matches!(
            &envelope.command,
            Command::ConfirmHostQuit(_)
                | Command::PrepareUpdate(_)
                | Command::ConfirmUpdateDrain(_)
                | Command::AbortUpdateHandoff
                | Command::ArmUpdateInstall(_)
        ),
        _ => false,
    }
}

fn command_completion_deadline(request: &ClientRequest) -> Duration {
    match request {
        ClientRequest::Query(envelope) => match &envelope.query {
            Query::TaskCockpit(TaskCockpitQuery::AgentConnection) => {
                agent_connection_query_timeout()
            }
            Query::TaskCockpit(_) => task_cockpit_query_timeout(),
            _ => DEFAULT_COMMAND_TIMEOUT,
        },
        _ => DEFAULT_COMMAND_TIMEOUT,
    }
}

fn lock_channel<'a>(
    channel: &'a Mutex<EndToEndChannel>,
) -> Result<std::sync::MutexGuard<'a, EndToEndChannel>, IpcError> {
    channel.lock().map_err(|_| IpcError::Unavailable)
}

/// Open a sealed frame under negotiated limits and fence binding + sequences.
pub(crate) fn open_connect_envelope(
    channel: &Mutex<EndToEndChannel>,
    frame: &SealedFrame,
    expected_binding: ChannelBinding,
    negotiated_limits: ConnectLimits,
    now_unix: u64,
) -> Result<ConnectEnvelope, IpcError> {
    let mut channel = lock_channel(channel)?;
    let plaintext = channel
        .open_bytes(frame, now_unix)
        .map_err(|_| IpcError::Unauthorized)?;
    let envelope = ConnectEnvelope::decode_with_limits(&plaintext, negotiated_limits)
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
    Ok(envelope)
}

async fn read_connect_half<S>(
    mut stream: S,
    channel: Arc<Mutex<EndToEndChannel>>,
    mut dispatch: ConnectDispatchSession,
    bound_port: ConnectDuplexBoundPort,
    reply_tx: mpsc::Sender<PendingReply>,
    expected_binding: ChannelBinding,
    negotiated_limits: ConnectLimits,
) -> Result<(), IpcError>
where
    S: Stream<Item = Result<WsMessage, axum::Error>> + Unpin,
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
            WsMessage::Text(_) => return Err(IpcError::Unauthorized),
            WsMessage::Binary(bytes) => {
                if bytes.is_empty() || bytes.len() > max_bytes {
                    return Err(IpcError::Unauthorized);
                }
                let frame = SealedFrame::decode(&bytes).map_err(|_| IpcError::Unauthorized)?;
                let envelope = open_connect_envelope(
                    &channel,
                    &frame,
                    expected_binding,
                    negotiated_limits,
                    unix_now_secs(),
                )?;
                let payload = envelope
                    .decode_payload()
                    .map_err(|_| IpcError::Unauthorized)?;
                let request_id = envelope.request_id;
                let operation_id = envelope.operation_id;
                let (reply, disposition) = dispatch
                    .handle_payload(&envelope, payload, Some(&bound_port as _))
                    .await;
                let (close_after_flush, wait_for_flush) =
                    if matches!(disposition, ConnectSessionDisposition::Disconnect) {
                        let (tx, rx) = tokio::sync::oneshot::channel();
                        (Some(tx), Some(rx))
                    } else {
                        (None, None)
                    };
                reply_tx
                    .send(PendingReply {
                        payload: reply,
                        request_id,
                        operation_id,
                        close_after_flush,
                    })
                    .await
                    .map_err(|_| IpcError::Unavailable)?;
                if let Some(wait_for_flush) = wait_for_flush {
                    // Keep the reader alive until the writer has flushed the
                    // terminal reply. Supervision must not cancel it in queue.
                    let _ = wait_for_flush.await;
                    return Ok(());
                }
            }
        }
    }
    Ok(())
}

async fn write_connect_half<S>(
    mut sink: S,
    channel: Arc<Mutex<EndToEndChannel>>,
    mut ports: ConnectionOutputPorts,
    mut reply_rx: mpsc::Receiver<PendingReply>,
    binding: ChannelBinding,
    limits: ConnectLimits,
    capabilities: CapabilitySet,
    shutdown_rx: &mut tokio::sync::watch::Receiver<bool>,
) -> Result<(), IpcError>
where
    S: Sink<WsMessage> + Unpin,
    S::Error: std::fmt::Display,
{
    loop {
        if *shutdown_rx.borrow() {
            return Ok(());
        }
        tokio::select! {
            reply = reply_rx.recv() => {
                let Some(reply) = reply else {
                    return Ok(());
                };
                seal_and_send(
                    &mut sink,
                    &channel,
                    binding,
                    limits,
                    reply.payload,
                    reply.request_id,
                    reply.operation_id,
                )
                .await?;
                if let Some(flushed) = reply.close_after_flush {
                    let _ = flushed.send(());
                    return Ok(());
                }
            }
            outbound = ports.recv_prioritized() => {
                let Some(outbound) = outbound else {
                    return Ok(());
                };
                write_host_outbound(
                    &mut sink,
                    &channel,
                    binding,
                    limits,
                    capabilities,
                    outbound,
                )
                .await?;
            }
            changed = shutdown_rx.changed() => {
                if changed.is_err() || *shutdown_rx.borrow() {
                    return Ok(());
                }
            }
        }
    }
}

/// Convert, seal, send, and flush one host outbound; ack only after flush.
pub(crate) async fn write_host_outbound<S>(
    sink: &mut S,
    channel: &Mutex<EndToEndChannel>,
    binding: ChannelBinding,
    limits: ConnectLimits,
    capabilities: CapabilitySet,
    mut outbound: PrioritizedOutbound,
) -> Result<(), IpcError>
where
    S: Sink<WsMessage> + Unpin,
    S::Error: std::fmt::Display,
{
    if !outbound.should_write() {
        return Ok(());
    }
    outbound.prepare_for_write();
    let payload = match connect_payload_from_host_outbound(&outbound, capabilities, limits) {
        Ok(payload) => payload,
        Err(_) => {
            drop(outbound);
            return Err(IpcError::Unauthorized);
        }
    };
    seal_and_send(sink, channel, binding, limits, payload, None, None).await?;
    outbound.after_successful_write();
    Ok(())
}

async fn seal_and_send<S>(
    sink: &mut S,
    channel: &Mutex<EndToEndChannel>,
    binding: ChannelBinding,
    limits: ConnectLimits,
    payload: ConnectPayload,
    request_id: Option<RequestId>,
    operation_id: Option<OperationId>,
) -> Result<(), IpcError>
where
    S: Sink<WsMessage> + Unpin,
    S::Error: std::fmt::Display,
{
    let mut nonce = [0_u8; SEALED_NONCE_BYTES];
    getrandom::fill(&mut nonce).map_err(|_| IpcError::Unavailable)?;
    let now_unix = unix_now_secs();
    let sealed = {
        let mut channel = lock_channel(channel)?;
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
        channel
            .seal(&envelope, nonce, now_unix)
            .map_err(|_| IpcError::Unauthorized)?
    };
    send_sealed_binary(sink, &sealed, limits).await
}

async fn send_sealed_binary<S>(
    sink: &mut S,
    sealed: &SealedFrame,
    limits: ConnectLimits,
) -> Result<(), IpcError>
where
    S: Sink<WsMessage> + Unpin,
    S::Error: std::fmt::Display,
{
    let encoded = sealed.encode().map_err(|_| IpcError::Unauthorized)?;
    if encoded.len() > max_connect_frame_bytes(limits) {
        return Err(IpcError::Unauthorized);
    }
    tokio::time::timeout(SOCKET_WRITE_TIMEOUT, async {
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
    })
    .await
    .map_err(|_| IpcError::Timeout)?
}

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(1)
        .max(1)
}

fn required_capabilities_for_host_message(
    message: &ServerMessage,
) -> Result<CapabilitySet, &'static str> {
    match message {
        ServerMessage::DurableEvent { .. } | ServerMessage::ResyncRequired { .. } => {
            Ok(CapabilitySet::from_capabilities([Capability::EventReplay]))
        }
        ServerMessage::Stream(frame) => {
            if frame.payload_kind != StreamPayloadKind::BROWSER_FRAME {
                return Err("host stream output rejects unknown StreamPayloadKind values");
            }
            Ok(CapabilitySet::from_capabilities([
                Capability::BrowserProjection,
            ]))
        }
        ServerMessage::ConversationDirty { .. } => Ok(CapabilitySet::from_capabilities([
            Capability::TaskCockpit,
            Capability::SemanticConversation,
        ])),
        _ => Err("request, reply, receipt, and detach variants are not host-output payloads"),
    }
}

fn connect_payload_from_host_outbound(
    outbound: &PrioritizedOutbound,
    negotiated: CapabilitySet,
    limits: ConnectLimits,
) -> Result<ConnectPayload, &'static str> {
    convert_host_outbound_for_connect(outbound.message().clone(), negotiated, limits)
}

/// Lossless host-output conversion with negotiated-cap check.
pub(crate) fn convert_host_outbound_for_connect(
    message: ServerMessage,
    negotiated: CapabilitySet,
    limits: ConnectLimits,
) -> Result<ConnectPayload, &'static str> {
    let required = required_capabilities_for_host_message(&message)?;
    let payload =
        ConnectPayload::from_host_output(message, required).map_err(|error| match error {
            crate::connect::PayloadDecodeError::Ambiguous { reason } => reason,
            _ => "host output conversion failed",
        })?;
    match &payload {
        ConnectPayload::HostDurableOutput(host)
        | ConnectPayload::HostCriticalOutput(host)
        | ConnectPayload::HostStreamOutput(host)
        | ConnectPayload::HostConversationOutput(host) => {
            host.validate_negotiated_capabilities(negotiated)
                .map_err(|_| {
                    "host output required_capabilities are not covered by negotiated capabilities"
                })?;
            let lane = match &payload {
                ConnectPayload::HostDurableOutput(_) => HostOutputLane::Durable,
                ConnectPayload::HostCriticalOutput(_) => HostOutputLane::Critical,
                ConnectPayload::HostStreamOutput(_) => HostOutputLane::Ephemeral,
                ConnectPayload::HostConversationOutput(_) => HostOutputLane::Ephemeral,
                _ => unreachable!(),
            };
            host.validate_for_lane(lane, limits)
                .map_err(|_| "host output lane validation failed")?;
        }
        _ => return Err("host output conversion produced a non-output payload"),
    }
    Ok(payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connect::{
        advertised_connect_capabilities, connect_prologue, ChannelId, ConnectChannelKey,
        ConnectChannelRole, ConnectCredentialPurpose, ConnectIdentityLiveState, ConnectLimits,
        ConnectNoiseCustody, ConnectNoiseIdentityBinding, ConnectPayload, ConnectPrivacyClass,
        ConnectionId, EndToEndChannel, HelloPayload, SessionId,
        CONNECT_NOISE_FIRST_PAIRING_PATTERN,
    };
    use crate::domain::command::{
        ArmUpdateInstallIntent, CommandEnvelope, ConfirmHostQuitIntent, ConfirmUpdateDrainIntent,
        PrepareUpdateIntent,
    };
    use crate::domain::event::{DomainEvent, Event};
    use crate::domain::id::{CommandId, EventId, ResourceId, SubscriptionId, TaskId};
    use crate::kernel::CommandBus;
    use crate::protocol::{
        Capability, CapabilitySet, FrameLimits, NegotiatedParameters, ProtocolVersion, StreamFrame,
        StreamKey,
    };
    use std::pin::Pin;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::task::Waker;
    use std::task::{Context, Poll};

    use super::super::connection::{
        ConnectionOutputHandle, HostRequestExecutor, PhysicalWriteAckStatus,
    };
    use tokio::sync::watch;

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

    fn sample_durable() -> ServerMessage {
        ServerMessage::DurableEvent {
            subscription_id: SubscriptionId::from_bytes(fixture_uuid(0x71)).expect("sub"),
            event: DomainEvent {
                id: EventId::from_bytes(fixture_uuid(0x55)).expect("event"),
                task_id: Some(TaskId::from_bytes(fixture_uuid(0x43)).expect("task")),
                sequence: 3,
                task_revision: Some(1),
                occurred_at_ms: 1,
                payload: Event::TaskReopened,
            },
        }
    }

    fn sample_resync() -> ServerMessage {
        ServerMessage::ResyncRequired {
            subscription_id: SubscriptionId::from_bytes(fixture_uuid(0x72)).expect("sub"),
            last_delivered_sequence: 2,
            newest_sequence: 5,
        }
    }

    fn sample_browser_stream() -> ServerMessage {
        ServerMessage::Stream(StreamFrame {
            subscription_id: SubscriptionId::from_bytes(fixture_uuid(0x73)).expect("sub"),
            stream: StreamKey::from_resource_id(
                ResourceId::from_bytes(fixture_uuid(0x74)).expect("resource"),
            ),
            generation: 1,
            sequence: 2,
            payload_kind: StreamPayloadKind::BROWSER_FRAME,
            schema_version: 1,
            payload: vec![0x62, 0x72],
        })
    }

    fn event_replay_caps() -> CapabilitySet {
        CapabilitySet::from_capabilities([Capability::EventReplay])
    }

    /// Production Noise pair + Hello-finished dispatch sharing one session id.
    struct NegotiatedDuplexFixture {
        host_channel: EndToEndChannel,
        peer_channel: EndToEndChannel,
        dispatch: ConnectDispatchSession,
        client_id: ClientId,
        binding: ChannelBinding,
        limits: ConnectLimits,
        capabilities: CapabilitySet,
    }

    fn complete_production_pair(
        prologue: crate::protocol::CryptoPrologue,
    ) -> (EndToEndChannel, EndToEndChannel) {
        let initiator_keys = ConnectNoiseCustody::generate().expect("initiator");
        let responder_keys = ConnectNoiseCustody::generate().expect("responder");
        let mut initiator = EndToEndChannel::open_production_handshake(
            CONNECT_NOISE_FIRST_PAIRING_PATTERN,
            true,
            &initiator_keys,
            None,
            prologue,
            ConnectChannelRole::Initiator,
            ConnectNoiseIdentityBinding::host([11; 16]),
            unix_now_secs(),
            true,
            false,
        )
        .expect("initiator handshake");
        let mut responder = EndToEndChannel::open_production_handshake(
            CONNECT_NOISE_FIRST_PAIRING_PATTERN,
            true,
            &responder_keys,
            None,
            prologue,
            ConnectChannelRole::Responder,
            ConnectNoiseIdentityBinding::host([12; 16]),
            unix_now_secs(),
            true,
            false,
        )
        .expect("responder handshake");
        let first = initiator.write_message().expect("first");
        responder.read_message(&first).expect("read first");
        let second = responder.write_message().expect("second");
        initiator.read_message(&second).expect("read second");
        let third = initiator.write_message().expect("third");
        responder.read_message(&third).expect("read third");
        (
            EndToEndChannel::from_noise_transport(initiator.finish().expect("initiator finish")),
            EndToEndChannel::from_noise_transport(responder.finish().expect("responder finish")),
        )
    }

    async fn negotiated_production_fixture() -> NegotiatedDuplexFixture {
        negotiated_production_fixture_with_port(None).await
    }

    async fn negotiated_production_fixture_with_port(
        port: Option<&dyn ConnectHostCommandPort>,
    ) -> NegotiatedDuplexFixture {
        negotiated_production_fixture_with_port_and_client_id(port, None).await
    }

    async fn negotiated_production_fixture_with_port_and_client_id(
        port: Option<&dyn ConnectHostCommandPort>,
        assigned_client_id: Option<ClientId>,
    ) -> NegotiatedDuplexFixture {
        let binding = ChannelBinding::new(ConnectionId::new(), SessionId::new(), ChannelId::new());
        let prologue = connect_prologue(
            ConnectCredentialPurpose::OwnerPairing,
            binding.connection_id.as_bytes(),
            binding.session_id.as_bytes(),
        )
        .expect("prologue");
        let (host_channel, peer_channel) = complete_production_pair(prologue);
        assert!(host_channel.is_production_grade());

        let mut dispatch = ConnectDispatchSession::bind_paired(
            "paired-web".into(),
            ConnectIdentityLiveState::Live,
        )
        .with_legacy_host_compat();
        if let Some(client_id) = assigned_client_id {
            dispatch = dispatch.with_assigned_client_id(client_id);
        }
        let limits = ConnectLimits::v1_default();
        let hello = ConnectPayload::Hello(HelloPayload {
            capabilities: CapabilitySet::from_bits(u64::MAX),
            limits,
            privacy_class: ConnectPrivacyClass::LocalOnly,
            relay_url: None,
            capability_grant: None,
            client_id: assigned_client_id,
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
        .expect("hello envelope");
        let (reply, disposition) = dispatch.handle_payload(&env, hello, port).await;
        assert_eq!(disposition, ConnectSessionDisposition::Continue);
        let ConnectPayload::Hello(hello) = reply else {
            panic!("expected Hello reply");
        };
        let client_id = hello.client_id.expect("assigned client");
        NegotiatedDuplexFixture {
            host_channel,
            peer_channel,
            dispatch,
            client_id,
            binding,
            limits: hello.limits,
            capabilities: hello.capabilities,
        }
    }

    fn source_level_pair(binding: ChannelBinding) -> (EndToEndChannel, EndToEndChannel) {
        let secret = ConnectChannelKey::from_bytes([7; 32]);
        let prologue = connect_prologue(
            ConnectCredentialPurpose::OwnerPairing,
            binding.connection_id.as_bytes(),
            binding.session_id.as_bytes(),
        )
        .expect("prologue");
        EndToEndChannel::pair_source_level(secret, prologue, true, unix_now_secs())
            .expect("source pair")
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum FlushGate {
        Blocked,
        Ok,
        BrokenPipe,
    }

    struct GateInner {
        state: FlushGate,
        waker: Option<Waker>,
    }

    struct GatedFlushWsSink {
        frames: Vec<Vec<u8>>,
        gate: Arc<Mutex<GateInner>>,
    }

    impl GatedFlushWsSink {
        fn new(gate: Arc<Mutex<GateInner>>) -> Self {
            Self {
                frames: Vec::new(),
                gate,
            }
        }

        fn set_gate(gate: &Arc<Mutex<GateInner>>, state: FlushGate) {
            let mut inner = gate.lock().expect("gate");
            inner.state = state;
            if let Some(waker) = inner.waker.take() {
                waker.wake();
            }
        }
    }

    impl Sink<WsMessage> for GatedFlushWsSink {
        type Error = std::io::Error;

        fn poll_ready(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn start_send(self: Pin<&mut Self>, item: WsMessage) -> Result<(), Self::Error> {
            let this = self.get_mut();
            match item {
                WsMessage::Binary(bytes) => this.frames.push(bytes.to_vec()),
                other => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        format!("unexpected ws message {other:?}"),
                    ));
                }
            }
            Ok(())
        }

        fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            let this = self.get_mut();
            let mut gate = this.gate.lock().expect("gate");
            match gate.state {
                FlushGate::Blocked => {
                    gate.waker = Some(cx.waker().clone());
                    Poll::Pending
                }
                FlushGate::Ok => Poll::Ready(Ok(())),
                FlushGate::BrokenPipe => Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "flush failed",
                ))),
            }
        }

        fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            self.poll_flush(cx)
        }
    }

    struct RecordingWsSink {
        frames: Arc<Mutex<Vec<Vec<u8>>>>,
    }

    impl Sink<WsMessage> for RecordingWsSink {
        type Error = std::io::Error;

        fn poll_ready(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn start_send(self: Pin<&mut Self>, item: WsMessage) -> Result<(), Self::Error> {
            if let WsMessage::Binary(bytes) = item {
                self.get_mut()
                    .frames
                    .lock()
                    .expect("frames")
                    .push(bytes.to_vec());
            }
            Ok(())
        }

        fn poll_flush(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            self.poll_flush(cx)
        }
    }

    #[test]
    fn host_output_conversion_is_lossless_for_durable_critical_stream() {
        let negotiated = CapabilitySet::from_capabilities([
            Capability::EventReplay,
            Capability::BrowserProjection,
        ]);
        let limits = ConnectLimits::v1_default();

        let durable = convert_host_outbound_for_connect(sample_durable(), negotiated, limits)
            .expect("durable");
        let critical = convert_host_outbound_for_connect(sample_resync(), negotiated, limits)
            .expect("critical");
        let stream = convert_host_outbound_for_connect(sample_browser_stream(), negotiated, limits)
            .expect("stream");

        let ConnectPayload::HostDurableOutput(host) = &durable else {
            panic!("durable wrapper");
        };
        assert_eq!(host.message, sample_durable());

        let ConnectPayload::HostCriticalOutput(host) = &critical else {
            panic!("critical wrapper");
        };
        assert_eq!(host.message, sample_resync());

        let ConnectPayload::HostStreamOutput(host) = &stream else {
            panic!("stream wrapper");
        };
        assert_eq!(host.message, sample_browser_stream());

        assert!(convert_host_outbound_for_connect(
            sample_durable(),
            CapabilitySet::empty(),
            limits
        )
        .is_err());
    }

    struct ChannelWsSink(mpsc::UnboundedSender<Vec<u8>>);

    impl Sink<WsMessage> for ChannelWsSink {
        type Error = std::io::Error;
        fn poll_ready(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
        fn start_send(self: Pin<&mut Self>, message: WsMessage) -> Result<(), Self::Error> {
            let WsMessage::Binary(bytes) = message else {
                panic!("binary only")
            };
            self.0
                .send(bytes)
                .map_err(|_| std::io::ErrorKind::BrokenPipe.into())
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
        fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            self.poll_flush(cx)
        }
    }

    struct TestConnectClient {
        input: mpsc::Sender<WsMessage>,
        output: mpsc::UnboundedReceiver<Vec<u8>>,
        peer: EndToEndChannel,
        binding: ChannelBinding,
        limits: ConnectLimits,
        client_id: ClientId,
        output_id: super::super::connection::ConnectionOutputId,
        task: tokio::task::JoinHandle<Result<(), IpcError>>,
    }

    impl TestConnectClient {
        async fn open(requests: &HostRequestHandle) -> Self {
            Self::open_with_stable_client_id(requests, ClientId::new()).await
        }

        async fn open_with_stable_client_id(
            requests: &HostRequestHandle,
            stable_client_id: ClientId,
        ) -> Self {
            let fixture = negotiated_production_fixture_with_port_and_client_id(
                Some(requests),
                Some(stable_client_id),
            )
            .await;
            assert!(fixture.capabilities.contains(Capability::EventReplay));
            assert_eq!(fixture.client_id, stable_client_id);
            let duplex = requests
                .open_connect_duplex(fixture.client_id)
                .await
                .expect("duplex");
            let output_id = duplex.registration.id();
            let (input, rx) = mpsc::channel(4);
            let (tx, output) = mpsc::unbounded_channel();
            let incoming = Box::pin(futures_util::stream::unfold(rx, |mut rx| async {
                rx.recv().await.map(|message| (Ok(message), rx))
            }));
            // Fixture has completed application Hello outside this engine.
            let task = tokio::spawn(serve_connect_io(
                ChannelWsSink(tx),
                incoming,
                fixture.host_channel.with_send_cursor(1),
                fixture.dispatch,
                duplex,
                None,
            ));
            Self {
                input,
                output,
                peer: fixture.peer_channel.with_send_cursor(1),
                binding: fixture.binding,
                limits: fixture.limits,
                client_id: fixture.client_id,
                output_id,
                task,
            }
        }

        async fn send(&mut self, payload: ConnectPayload) {
            let request_id = match &payload {
                ConnectPayload::Query(q) => Some(q.request_id),
                _ => None,
            };
            let envelope = ConnectEnvelope::new(
                self.binding,
                payload.channel(),
                self.peer.next_send_sequence(),
                request_id,
                None,
                self.limits,
                ConnectPrivacyClass::LocalOnly,
                payload,
            )
            .expect("request envelope");
            let sealed = self
                .peer
                .seal(&envelope, [17; SEALED_NONCE_BYTES], unix_now_secs())
                .expect("seal request");
            self.input
                .send(WsMessage::Binary(sealed.encode().expect("sealed bytes")))
                .await
                .expect("input");
        }

        async fn receive(&mut self) -> ConnectPayload {
            let bytes = tokio::time::timeout(Duration::from_secs(3), self.output.recv())
                .await
                .expect("bounded output wait")
                .expect("live writer");
            let sealed = SealedFrame::decode(&bytes).expect("sealed output");
            let plaintext = self
                .peer
                .open_bytes(&sealed, unix_now_secs())
                .expect("authenticated output");
            let envelope = ConnectEnvelope::decode_with_limits(&plaintext, self.limits)
                .expect("output envelope");
            assert_eq!(envelope.sequence, sealed.sequence());
            envelope.decode_payload().expect("output payload")
        }

        async fn subscribe(&mut self) {
            self.send(ConnectPayload::Query(crate::domain::query::QueryEnvelope {
                request_id: RequestId::new(),
                client_id: self.client_id,
                task_id: None,
                query: Query::OpenEventReplay { after_sequence: 1 },
            }))
            .await;
            assert!(matches!(
                self.receive().await,
                ConnectPayload::QueryReply(_)
            ));
        }

        async fn receipt_status(
            &mut self,
            task_id: Option<TaskId>,
            command: CommandEnvelope,
        ) -> crate::domain::query::QueryReply {
            let request_id = RequestId::new();
            self.send(ConnectPayload::Query(crate::domain::query::QueryEnvelope {
                request_id,
                client_id: self.client_id,
                task_id,
                query: Query::CommandReceiptStatus { command },
            }))
            .await;
            let ConnectPayload::QueryReply(reply) = self.receive().await else {
                panic!("receipt recovery must be a fresh query reply, not stale output");
            };
            assert_eq!(reply.request_id, request_id);
            reply
        }

        async fn discard_command_receipt(
            &mut self,
            command_id: CommandId,
        ) -> crate::domain::command::CommandReceipt {
            for _ in 0..8 {
                match self.receive().await {
                    ConnectPayload::CommandReceipt(receipt)
                        if matches!(
                            &receipt,
                            crate::domain::command::CommandReceipt::Accepted {
                                command_id: received,
                                ..
                            } if received == &command_id
                        ) =>
                    {
                        return receipt
                    }
                    ConnectPayload::HostDurableOutput(_) => {}
                    other => panic!("unexpected output while losing receipt: {other:?}"),
                }
            }
            panic!("command receipt was not written before reconnect");
        }

        async fn close(self, requests: &HostRequestHandle) {
            self.input
                .send(WsMessage::Close(None))
                .await
                .expect("close input");
            tokio::time::timeout(Duration::from_secs(3), self.task)
                .await
                .expect("join deadline")
                .expect("engine joined")
                .expect("engine clean exit");
            assert!(
                !requests
                    .inspect_output(self.output_id)
                    .await
                    .expect("inspect cleanup")
                    .registered
            );
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn two_production_noise_clients_share_one_executor_and_receive_live_event() {
        use crate::domain::command::{CommandReceipt, CreateTaskIntent, RenameTaskIntent};
        use crate::domain::id::{EnvironmentId, ProjectId};
        use crate::domain::task::{
            ReviewReadiness, TaskActivity, TaskAssignment, TaskAttention, TaskConnectivity,
            WorkspaceRef,
        };
        let directory = tempfile::tempdir().expect("isolated store");
        let mut bus = CommandBus::open(&directory.path().join("two-clients.db")).expect("bus");
        let task_id = TaskId::new();
        let created = bus
            .execute_for_test(CommandEnvelope {
                command_id: CommandId::new(),
                client_id: ClientId::new(),
                task_id: None,
                issued_at_ms: 1,
                expected_task_revision: None,
                command: Command::CreateTask(CreateTaskIntent {
                    id: task_id,
                    environment_id: EnvironmentId::new(),
                    title: "Before".into(),
                    description: None,
                    project_id: ProjectId::new(),
                    workspace: WorkspaceRef::Main,
                    assignment: TaskAssignment::LocalOwner,
                    created_at_ms: 1,
                    connectivity: TaskConnectivity::Connected,
                    attention: TaskAttention::None,
                    activity: TaskActivity::Idle,
                    review_readiness: ReviewReadiness::NotReady,
                }),
            })
            .expect("seed task");
        assert!(matches!(created, CommandReceipt::Accepted { .. }));
        let (requests, executor) = HostRequestExecutor::start(bus);
        let mut first = TestConnectClient::open(&requests).await;
        let mut second = TestConnectClient::open(&requests).await;
        first.subscribe().await;
        second.subscribe().await;
        let rename = ConnectPayload::Command(CommandEnvelope {
            command_id: CommandId::new(),
            client_id: first.client_id,
            task_id: Some(task_id),
            issued_at_ms: 2,
            expected_task_revision: Some(1),
            command: Command::RenameTask(RenameTaskIntent {
                title: "Remote edit".into(),
            }),
        });
        first.send(rename.clone()).await;
        // A command can emit several domain/operation facts. The writer may
        // legally deliver those before its receipt; never assume two frames.
        let mut accepted = false;
        let mut renamed = false;
        for _ in 0..8 {
            match first.receive().await {
                ConnectPayload::CommandReceipt(CommandReceipt::Accepted { .. }) => accepted = true,
                ConnectPayload::HostDurableOutput(output) => {
                    if let ServerMessage::DurableEvent { event, .. } = output.message {
                        renamed |= matches!(event.payload, Event::TaskRenamed { .. })
                            && event.task_id == Some(task_id);
                    }
                }
                other => panic!("unexpected first client output: {other:?}"),
            }
            if accepted && renamed {
                break;
            }
        }
        assert!(accepted && renamed);
        let mut second_renamed = false;
        for _ in 0..8 {
            let ConnectPayload::HostDurableOutput(output) = second.receive().await else {
                panic!("live event")
            };
            let ServerMessage::DurableEvent { event, .. } = output.message else {
                panic!("durable")
            };
            if matches!(event.payload, Event::TaskRenamed { .. }) {
                assert_eq!(event.task_id, Some(task_id));
                assert_eq!(event.task_revision, Some(2));
                second_renamed = true;
                break;
            }
        }
        assert!(second_renamed);
        first.send(rename).await;
        let mut retry_accepted = false;
        for _ in 0..8 {
            match first.receive().await {
                ConnectPayload::CommandReceipt(CommandReceipt::Accepted { .. }) => {
                    retry_accepted = true;
                    break;
                }
                ConnectPayload::HostDurableOutput(output) => {
                    if let ServerMessage::DurableEvent { event, .. } = output.message {
                        assert!(
                            !matches!(event.payload, Event::TaskRenamed { .. }),
                            "retry duplicated task mutation"
                        );
                    }
                }
                other => panic!("unexpected retry output: {other:?}"),
            }
        }
        assert!(retry_accepted);
        first.close(&requests).await;
        assert!(
            requests
                .inspect_output(second.output_id)
                .await
                .expect("second still attached")
                .registered
        );
        second.close(&requests).await;
        drop(requests);
        executor.abort();
        let _ = executor.await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn reconnect_receipt_recovery_is_stable_client_bound_read_only_and_conflict_fenced() {
        use crate::domain::command::{CommandReceipt, CreateTaskIntent, RenameTaskIntent};
        use crate::domain::id::{EnvironmentId, ProjectId};
        use crate::domain::query::{QueryError, QueryOutcome, QueryResult};
        use crate::domain::task::{
            ReviewReadiness, TaskActivity, TaskAssignment, TaskAttention, TaskConnectivity,
            WorkspaceRef,
        };

        let directory = tempfile::tempdir().expect("isolated store");
        let mut bus =
            CommandBus::open(&directory.path().join("receipt-reconnect.db")).expect("bus");
        let task_id = TaskId::new();
        assert!(matches!(
            bus.execute_for_test(CommandEnvelope {
                command_id: CommandId::new(),
                client_id: ClientId::new(),
                task_id: None,
                issued_at_ms: 1,
                expected_task_revision: None,
                command: Command::CreateTask(CreateTaskIntent {
                    id: task_id,
                    environment_id: EnvironmentId::new(),
                    title: "Before".into(),
                    description: None,
                    project_id: ProjectId::new(),
                    workspace: WorkspaceRef::Main,
                    assignment: TaskAssignment::LocalOwner,
                    created_at_ms: 1,
                    connectivity: TaskConnectivity::Connected,
                    attention: TaskAttention::None,
                    activity: TaskActivity::Idle,
                    review_readiness: ReviewReadiness::NotReady,
                }),
            })
            .expect("seed task"),
            CommandReceipt::Accepted { .. }
        ));

        let (requests, executor) = HostRequestExecutor::start(bus);
        let stable_client_id = ClientId::new();
        let command = CommandEnvelope {
            command_id: CommandId::new(),
            client_id: stable_client_id,
            task_id: Some(task_id),
            issued_at_ms: 2,
            expected_task_revision: Some(1),
            command: Command::RenameTask(RenameTaskIntent {
                title: "Recovered once".into(),
            }),
        };

        // The first authenticated Noise connection writes its receipt, but the
        // application drops it before consuming it and then reconnects.
        let mut first =
            TestConnectClient::open_with_stable_client_id(&requests, stable_client_id).await;
        first.send(ConnectPayload::Command(command.clone())).await;
        let original_receipt = first.discard_command_receipt(command.command_id).await;
        first.close(&requests).await;

        // A new Noise connection has the same pairing-owned client identity.
        // The recovery reply is correlated to its new query request, never a
        // stale receipt output from the first connection.
        let mut reconnect =
            TestConnectClient::open_with_stable_client_id(&requests, stable_client_id).await;
        let recovered = reconnect
            .receipt_status(Some(task_id), command.clone())
            .await;
        assert!(matches!(
            recovered.outcome,
            QueryOutcome::Ok(QueryResult::CommandReceiptStatus {
                receipt: Some(receipt),
            }) if receipt == original_receipt
        ));

        let mut foreign_client = command.clone();
        foreign_client.client_id = ClientId::new();
        assert!(matches!(
            reconnect
                .receipt_status(Some(task_id), foreign_client)
                .await
                .outcome,
            QueryOutcome::Err(QueryError::Unauthorized)
        ));

        let mut foreign_task = command.clone();
        let other_task = TaskId::new();
        foreign_task.task_id = Some(other_task);
        assert!(matches!(
            reconnect
                .receipt_status(Some(other_task), foreign_task)
                .await
                .outcome,
            QueryOutcome::Err(QueryError::Conflict)
        ));

        let mut changed_text = command.clone();
        changed_text.command = Command::RenameTask(RenameTaskIntent {
            title: "Tampered text".into(),
        });
        assert!(matches!(
            reconnect
                .receipt_status(Some(task_id), changed_text)
                .await
                .outcome,
            QueryOutcome::Err(QueryError::Conflict)
        ));

        // An authoritative absence is a read only answer. It does not reserve
        // the command id and the exact same command may execute later.
        let mut missing = command.clone();
        missing.command_id = CommandId::new();
        missing.expected_task_revision = Some(2);
        missing.command = Command::RenameTask(RenameTaskIntent {
            title: "Executed after absence".into(),
        });
        assert!(matches!(
            reconnect
                .receipt_status(Some(task_id), missing.clone())
                .await
                .outcome,
            QueryOutcome::Ok(QueryResult::CommandReceiptStatus { receipt: None })
        ));
        reconnect
            .send(ConnectPayload::Command(missing.clone()))
            .await;
        assert!(matches!(
            reconnect.discard_command_receipt(missing.command_id).await,
            CommandReceipt::Accepted { command_id, .. } if command_id == missing.command_id
        ));

        reconnect.close(&requests).await;
        drop(requests);
        executor.abort();
        let _ = executor.await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn noise_subscription_emits_typed_conversation_dirty_host_output() {
        use crate::domain::cockpit::{TaskCockpitQuery, TaskCockpitResult};
        use crate::domain::command::{CommandReceipt, CreateTaskIntent};
        use crate::domain::id::{EnvironmentId, ProjectId};
        use crate::domain::query::{QueryEnvelope, QueryOutcome, QueryResult};
        use crate::domain::task::{
            ReviewReadiness, TaskActivity, TaskAssignment, TaskAttention, TaskConnectivity,
            WorkspaceRef,
        };
        use crate::remote::presentation::{
            SemanticEventDraft, SemanticEventKind, SemanticJournalStore, SemanticRetention,
            SemanticSource, StableSessionKey,
        };

        let directory = tempfile::tempdir().expect("isolated store");
        let mut bus = CommandBus::open(&directory.path().join("semantic-connect.db")).expect("bus");
        let task_id = TaskId::new();
        assert!(matches!(
            bus.execute_for_test(CommandEnvelope {
                command_id: CommandId::new(),
                client_id: ClientId::new(),
                task_id: None,
                issued_at_ms: 1,
                expected_task_revision: None,
                command: Command::CreateTask(CreateTaskIntent {
                    id: task_id,
                    environment_id: EnvironmentId::new(),
                    title: "Semantic".into(),
                    description: None,
                    project_id: ProjectId::new(),
                    workspace: WorkspaceRef::Main,
                    assignment: TaskAssignment::LocalOwner,
                    created_at_ms: 1,
                    connectivity: TaskConnectivity::Connected,
                    attention: TaskAttention::None,
                    activity: TaskActivity::Idle,
                    review_readiness: ReviewReadiness::NotReady,
                }),
            })
            .expect("seed task"),
            CommandReceipt::Accepted { .. }
        ));

        let (requests, executor) = HostRequestExecutor::start(bus);
        requests
            .install_test_semantic_journal(Arc::new(Mutex::new(SemanticJournalStore::default())))
            .await
            .expect("install semantic journal");
        let mut client = TestConnectClient::open(&requests).await;
        client
            .send(ConnectPayload::Query(QueryEnvelope {
                request_id: RequestId::new(),
                client_id: client.client_id,
                task_id: Some(task_id),
                query: Query::TaskCockpit(TaskCockpitQuery::OpenConversationSubscription {
                    after_sequence: 0,
                }),
            }))
            .await;
        let ConnectPayload::QueryReply(opened) = client.receive().await else {
            panic!("open subscription reply");
        };
        let QueryOutcome::Ok(QueryResult::TaskCockpit(
            TaskCockpitResult::ConversationSubscription {
                subscription_id, ..
            },
        )) = opened.outcome
        else {
            panic!("expected conversation subscription");
        };

        let high_water = requests
            .record_test_semantic(SemanticEventDraft {
                stable_session_key: StableSessionKey::from_tab(task_id.to_string()),
                occurred_at_epoch_ms: 1_725_000_000_001,
                source: SemanticSource::Codex,
                kind: SemanticEventKind::AssistantMessage {
                    message_id: "message-1".into(),
                    text: "must not cross the dirty carrier".into(),
                    streaming: false,
                },
                retention: SemanticRetention::Canonical,
                deduplication_key: Some("semantic-connect-1".into()),
            })
            .await
            .expect("record semantic");
        let ConnectPayload::HostConversationOutput(output) = client.receive().await else {
            panic!("typed conversation dirty output");
        };
        assert_eq!(
            output.required_capabilities,
            CapabilitySet::from_capabilities([
                Capability::TaskCockpit,
                Capability::SemanticConversation,
            ])
        );
        assert!(matches!(
            output.message,
            ServerMessage::ConversationDirty {
                subscription_id: received_subscription,
                task_id: received_task,
                high_water: received_high_water,
            } if received_subscription == subscription_id
                && received_task == task_id
                && received_high_water == high_water
        ));

        client.close(&requests).await;
        drop(requests);
        executor.abort();
        let _ = executor.await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn write_host_outbound_ack_pending_while_flush_blocked_then_succeeds() {
        let binding = ChannelBinding::new(ConnectionId::new(), SessionId::new(), ChannelId::new());
        let (writer_channel, mut peer) = source_level_pair(binding);
        let channel = Mutex::new(writer_channel);
        let limits = ConnectLimits::v1_default();
        let caps = event_replay_caps();

        let (handle, mut ports) = ConnectionOutputHandle::new(2, 1, 1);
        let mut ack = handle
            .try_enqueue_critical_tracked(sample_durable())
            .expect("tracked");
        let outbound = ports.try_recv_prioritized().expect("dequeue");
        assert_eq!(ack.status(), PhysicalWriteAckStatus::Pending);

        let gate = Arc::new(Mutex::new(GateInner {
            state: FlushGate::Blocked,
            waker: None,
        }));
        let mut sink = GatedFlushWsSink::new(Arc::clone(&gate));
        {
            let write = write_host_outbound(&mut sink, &channel, binding, limits, caps, outbound);
            tokio::pin!(write);

            // Drive the actual writer until its physical flush parks.
            assert!(matches!(futures_util::poll!(&mut write), Poll::Pending));
            assert_eq!(ack.status(), PhysicalWriteAckStatus::Pending);
            GatedFlushWsSink::set_gate(&gate, FlushGate::Ok);
            write.await.expect("flush ok");
        }
        assert!(ack.wait().await.is_ok());

        let sealed = SealedFrame::decode(&sink.frames[0]).expect("sealed");
        let plaintext = peer
            .open_bytes(&sealed, unix_now_secs())
            .expect("peer open");
        let envelope =
            ConnectEnvelope::decode_with_limits(&plaintext, limits).expect("decode limits");
        assert_eq!(envelope.sequence, sealed.sequence());
        let payload = envelope.decode_payload().expect("payload");
        let ConnectPayload::HostDurableOutput(host) = payload else {
            panic!("expected durable host output");
        };
        assert_eq!(host.message, sample_durable());
        assert!(peer.bind_session(binding).is_ok());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn write_host_outbound_flush_error_aborts_ack() {
        let binding = ChannelBinding::new(ConnectionId::new(), SessionId::new(), ChannelId::new());
        let (writer_channel, _peer) = source_level_pair(binding);
        let channel = Mutex::new(writer_channel);
        let limits = ConnectLimits::v1_default();

        let (handle, mut ports) = ConnectionOutputHandle::new(2, 1, 1);
        let mut ack = handle
            .try_enqueue_critical_tracked(sample_resync())
            .expect("tracked");
        let outbound = ports.try_recv_prioritized().expect("dequeue");

        let gate = Arc::new(Mutex::new(GateInner {
            state: FlushGate::BrokenPipe,
            waker: None,
        }));
        let mut sink = GatedFlushWsSink::new(gate);
        let err = write_host_outbound(
            &mut sink,
            &channel,
            binding,
            limits,
            event_replay_caps(),
            outbound,
        )
        .await
        .expect_err("flush fail");
        assert!(matches!(err, IpcError::Io(_)));
        assert_eq!(ack.status(), PhysicalWriteAckStatus::Aborted);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn write_host_outbound_cancel_while_flush_blocked_aborts_ack() {
        let binding = ChannelBinding::new(ConnectionId::new(), SessionId::new(), ChannelId::new());
        let (writer_channel, _peer) = source_level_pair(binding);
        let channel = Mutex::new(writer_channel);
        let limits = ConnectLimits::v1_default();

        let (handle, mut ports) = ConnectionOutputHandle::new(2, 1, 1);
        let mut ack = handle
            .try_enqueue_critical_tracked(sample_durable())
            .expect("tracked");
        let outbound = ports.try_recv_prioritized().expect("dequeue");

        let gate = Arc::new(Mutex::new(GateInner {
            state: FlushGate::Blocked,
            waker: None,
        }));
        let mut sink = GatedFlushWsSink::new(gate);
        {
            let write = write_host_outbound(
                &mut sink,
                &channel,
                binding,
                limits,
                event_replay_caps(),
                outbound,
            );
            tokio::pin!(write);
            for _ in 0..8 {
                if matches!(futures_util::poll!(&mut write), Poll::Pending) {
                    break;
                }
            }
            assert_eq!(ack.status(), PhysicalWriteAckStatus::Pending);
            drop(write);
        }
        assert_eq!(ack.status(), PhysicalWriteAckStatus::Aborted);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn write_connect_half_emits_output_before_reply_arrives() {
        let binding = ChannelBinding::new(ConnectionId::new(), SessionId::new(), ChannelId::new());
        let (writer_channel, mut peer) = source_level_pair(binding);
        let channel = Arc::new(Mutex::new(writer_channel));
        let limits = ConnectLimits::v1_default();
        let caps = event_replay_caps();

        let (handle, ports) = ConnectionOutputHandle::new(4, 4, 1);
        handle
            .try_enqueue_critical(sample_resync())
            .expect("enqueue output");

        let (reply_tx, reply_rx) = mpsc::channel::<PendingReply>(REPLY_QUEUE_CAPACITY);
        let frames = Arc::new(Mutex::new(Vec::new()));
        let mut sink = RecordingWsSink {
            frames: Arc::clone(&frames),
        };
        let (_shutdown_tx, mut shutdown_rx) = watch::channel(false);

        let writer = write_connect_half(
            &mut sink,
            Arc::clone(&channel),
            ports,
            reply_rx,
            binding,
            limits,
            caps,
            &mut shutdown_rx,
        );
        tokio::pin!(writer);

        // Output is ready; reply channel has no item yet.
        for _ in 0..16 {
            let _ = futures_util::poll!(&mut writer);
            if frames.lock().expect("frames").len() >= 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(frames.lock().expect("frames").len(), 1);

        reply_tx
            .try_send(PendingReply {
                payload: ConnectPayload::Error(crate::connect::ErrorPayload {
                    code: 400,
                    message: "reply".into(),
                    request_id: None,
                    operation_id: None,
                }),
                request_id: None,
                operation_id: None,
                close_after_flush: None,
            })
            .expect("enqueue reply");

        for _ in 0..16 {
            let _ = futures_util::poll!(&mut writer);
            if frames.lock().expect("frames").len() >= 2 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(frames.lock().expect("frames").len(), 2);

        let first = frames.lock().expect("frames")[0].clone();
        let sealed = SealedFrame::decode(&first).expect("sealed output");
        let plaintext = peer
            .open_bytes(&sealed, unix_now_secs())
            .expect("open output");
        let envelope =
            ConnectEnvelope::decode_with_limits(&plaintext, limits).expect("decode output");
        let payload = envelope.decode_payload().expect("payload");
        assert!(matches!(payload, ConnectPayload::HostCriticalOutput(_)));

        drop(reply_tx);
        let _ = futures_util::poll!(&mut writer);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn enforce_accepts_negotiated_production_and_rejects_foreign_client() {
        let fixture = negotiated_production_fixture().await;
        let ok = enforce_connect_duplex_preconditions(
            &fixture.host_channel,
            &fixture.dispatch,
            fixture.client_id,
        );
        assert!(ok.is_ok());
        let foreign = ClientId::new();
        assert_ne!(foreign, fixture.client_id);
        assert!(matches!(
            enforce_connect_duplex_preconditions(&fixture.host_channel, &fixture.dispatch, foreign),
            Err(IpcError::Unauthorized)
        ));
        let _ = fixture.peer_channel;
        let _ = fixture.binding;
        let _ = fixture.limits;
        let _ = fixture.capabilities;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn enforce_rejects_unnegotiated_empty_paired_and_non_production() {
        let binding = ChannelBinding::new(ConnectionId::new(), SessionId::new(), ChannelId::new());
        let prologue = connect_prologue(
            ConnectCredentialPurpose::OwnerPairing,
            binding.connection_id.as_bytes(),
            binding.session_id.as_bytes(),
        )
        .expect("prologue");
        let (production, _) = complete_production_pair(prologue);

        let unnegotiated = ConnectDispatchSession::bind_paired(
            "paired-web".into(),
            ConnectIdentityLiveState::Live,
        );
        assert!(unnegotiated.paired_identity_bound());
        assert!(unnegotiated.bound_client_id().is_none());
        assert!(matches!(
            enforce_connect_duplex_preconditions(&production, &unnegotiated, ClientId::new()),
            Err(IpcError::Unauthorized)
        ));

        let empty_paired =
            ConnectDispatchSession::bind_paired(String::new(), ConnectIdentityLiveState::Live);
        assert!(!empty_paired.paired_identity_bound());
        assert!(matches!(
            enforce_connect_duplex_preconditions(&production, &empty_paired, ClientId::new()),
            Err(IpcError::Unauthorized)
        ));

        let (source, _) = source_level_pair(binding);
        assert!(!source.is_production_grade());
        let fixture = negotiated_production_fixture().await;
        assert!(matches!(
            enforce_connect_duplex_preconditions(&source, &fixture.dispatch, fixture.client_id),
            Err(IpcError::Unauthorized)
        ));
    }

    #[test]
    fn open_connect_envelope_rejects_sealed_sequence_mismatch() {
        let binding = ChannelBinding::new(ConnectionId::new(), SessionId::new(), ChannelId::new());
        let limits = ConnectLimits::v1_default();
        let (host, mut peer) = source_level_pair(binding);
        let payload = ConnectPayload::Error(crate::connect::ErrorPayload {
            code: 400,
            message: "y".into(),
            request_id: None,
            operation_id: None,
        });
        // Envelope claims sequence 99 while seal assigns the channel's next sequence (1).
        let mismatched = ConnectEnvelope::new(
            binding,
            payload.channel(),
            99,
            None,
            None,
            limits,
            ConnectPrivacyClass::LocalOnly,
            payload,
        )
        .expect("envelope");
        let sealed = peer
            .seal(&mismatched, [4; SEALED_NONCE_BYTES], 10)
            .expect("seal");
        assert_ne!(mismatched.sequence, sealed.sequence());
        let host = Mutex::new(host);
        assert!(matches!(
            open_connect_envelope(&host, &sealed, binding, limits, 10),
            Err(IpcError::Unauthorized)
        ));
    }

    #[test]
    fn max_connect_frame_bytes_is_min_of_negotiated_and_sealed_max() {
        let mut limits = ConnectLimits::v1_default();
        assert_eq!(
            max_connect_frame_bytes(limits),
            usize::try_from(MAX_SEALED_FRAME_BYTES)
                .unwrap_or(usize::MAX)
                .min(usize::try_from(limits.max_physical_frame_bytes).unwrap_or(usize::MAX))
        );
        limits.max_physical_frame_bytes = 1024;
        assert_eq!(max_connect_frame_bytes(limits), 1024);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn bound_port_denies_quit_and_update_before_host_invocation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bus = CommandBus::open(&dir.path().join("duplex-deny.db")).expect("bus");
        let (requests, executor) = HostRequestExecutor::start(bus);
        let port = ConnectDuplexBoundPort {
            inner: requests,
            peer: None,
        };
        let client = ClientId::new();
        let negotiated = NegotiatedParameters {
            version: ProtocolVersion::current(),
            client_id: client,
            capabilities: CapabilitySet::from_capabilities([
                Capability::HostShutdown,
                Capability::UpdateHandoff,
            ]),
            limits: FrameLimits::v1_default(),
        };

        let quit = port
            .execute(
                negotiated.clone(),
                ClientRequest::Command(CommandEnvelope {
                    command_id: CommandId::new(),
                    client_id: client,
                    task_id: None,
                    issued_at_ms: 1,
                    expected_task_revision: None,
                    command: Command::ConfirmHostQuit(ConfirmHostQuitIntent {
                        inspection_id: 0,
                        allow_uninspected_worktrees: true,
                    }),
                }),
            )
            .await;
        assert!(matches!(quit, Err(IpcError::Unsupported)));

        let prepare = port
            .execute(
                negotiated.clone(),
                ClientRequest::Command(CommandEnvelope {
                    command_id: CommandId::new(),
                    client_id: client,
                    task_id: None,
                    issued_at_ms: 1,
                    expected_task_revision: None,
                    command: Command::PrepareUpdate(PrepareUpdateIntent {
                        target_version: "0.0.0".into(),
                        client_build: "test".into(),
                        host_build: "test".into(),
                        allow_explicit_confirm_with_active: false,
                    }),
                }),
            )
            .await;
        assert!(matches!(prepare, Err(IpcError::Unsupported)));

        let drain = port
            .execute(
                negotiated.clone(),
                ClientRequest::Command(CommandEnvelope {
                    command_id: CommandId::new(),
                    client_id: client,
                    task_id: None,
                    issued_at_ms: 1,
                    expected_task_revision: None,
                    command: Command::ConfirmUpdateDrain(ConfirmUpdateDrainIntent {
                        token_id: uuid::Uuid::nil(),
                    }),
                }),
            )
            .await;
        assert!(matches!(drain, Err(IpcError::Unsupported)));

        let arm = port
            .execute(
                negotiated,
                ClientRequest::Command(CommandEnvelope {
                    command_id: CommandId::new(),
                    client_id: client,
                    task_id: None,
                    issued_at_ms: 1,
                    expected_task_revision: None,
                    command: Command::ArmUpdateInstall(ArmUpdateInstallIntent {
                        token_id: uuid::Uuid::nil(),
                    }),
                }),
            )
            .await;
        assert!(matches!(arm, Err(IpcError::Unsupported)));

        drop(port);
        executor.abort();
        let _ = executor.await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn supervise_cancel_drops_both_halves() {
        let dropped_reader = Arc::new(AtomicBool::new(false));
        let dropped_writer = Arc::new(AtomicBool::new(false));
        struct DropFlag(Arc<AtomicBool>);
        impl Drop for DropFlag {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let reader_flag = DropFlag(Arc::clone(&dropped_reader));
        let writer_flag = DropFlag(Arc::clone(&dropped_writer));
        let reader = async move {
            let _keep = reader_flag;
            std::future::pending::<Result<(), IpcError>>().await
        };
        let writer = async move {
            let _keep = writer_flag;
            std::future::pending::<Result<(), IpcError>>().await
        };
        {
            let supervised = supervise_duplex_halves(reader, writer);
            tokio::pin!(supervised);
            assert!(matches!(
                futures_util::poll!(&mut supervised),
                Poll::Pending
            ));
            // Exiting the scope drops the owning future, not merely Pin<&mut>.
        }
        assert!(dropped_reader.load(Ordering::SeqCst));
        assert!(dropped_writer.load(Ordering::SeqCst));
    }

    #[test]
    fn denies_helper_covers_abort_update_handoff() {
        let client = ClientId::new();
        let request = ClientRequest::Command(CommandEnvelope {
            command_id: CommandId::new(),
            client_id: client,
            task_id: None,
            issued_at_ms: 1,
            expected_task_revision: None,
            command: Command::AbortUpdateHandoff,
        });
        assert!(denies_physical_quit_or_update(&request));
    }
}
