//! WebSocket bridge between browser clients and the in-process
//! `RemoteHostInner`. A `WebClientSession` registers itself as a normal
//! `ConnectedRemoteClient` (the same type the TCP listener uses) so the
//! existing `run_broadcaster` fan-out loop delivers snapshot deltas and
//! session stream events to browsers for free.
//!
//! ## Thread/runtime plumbing
//!
//! The rest of `remote::mod` is built on `std::sync::mpsc` + std threads.
//! This file lives inside a tokio runtime owned by `WebListenerHandle`. The
//! bridge therefore needs to shuttle `ServerMessage`s out of the std channel
//! the broadcaster writes to and into the async WS writer task. We do that
//! with a `spawn_blocking` drainer that polls `std::sync::mpsc::Receiver` and
//! forwards into a tokio mpsc. No awaits happen while holding any
//! `RwLock`/`Mutex` from `RemoteHostInner`.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc as std_mpsc, Arc, MutexGuard};
use std::time::{Duration, Instant};

use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use futures_util::{Sink, SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::mpsc as tokio_mpsc;
use tokio::sync::watch;

use super::super::presentation::{
    SemanticEventDraft, SemanticEventKind, SemanticReplay, SemanticRetention,
    SemanticSessionMetadata, SemanticSource, StableSessionKey,
};
use super::super::{
    acknowledge_browser_attention, now_epoch_ms, publish_semantic_event,
    request_timeout_for_action, requires_control, stable_hash, try_enqueue_pending_request,
    ComposerReconciliationReservation, ConnectedRemoteClient, PendingRemoteRequest,
    RemoteActionResult, RemoteHostInner, RemoteHostService, RemoteImageAttachment,
    RemoteSessionStreamEvent, RemoteTerminalInput, RemoteWebMutationAuthority,
    RemoteWorkspaceSnapshot, ServerMessage, WebComposerMutationRecord, WebComposerMutationStatus,
};
use super::action::WebActionResult;
use super::dto::{WebWorkspaceSnapshot, WebWriterLeaseState, WEB_BUILD_ID, WEB_PROTOCOL_VERSION};
#[cfg(test)]
use super::lease::MutationBegin;
use super::lease::{LeaseError, WriterLease};
use super::wire::{
    ComposerAccepted, ComposerAttachment, ComposerRejectCode, ComposerRejected, ResumeRequest,
    ResumeState, SemanticReplayDescriptor, SemanticReplayPage, WsInbound, WsOutbound,
};
use super::{authenticate_request, record_browser_connection, request_is_same_origin, WebState};
use crate::ai::codex_bridge::canonical_codex_composer_prompt;
use crate::state::{SessionDimensions, SessionKind};

/// Frame type byte prefixed to binary WS frames carrying terminal output.
const BINARY_FRAME_SESSION_OUTPUT: u8 = 0x01;
const WEB_PUSH_CHANNEL_CAPACITY: usize = 256;
const WEB_OUTBOUND_MAX_BYTES: usize = 4 * 1024 * 1024;
const WEB_OUTBOUND_STALL_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_SEMANTIC_REPLAY_PAGE_EVENTS: usize = 256;
const MAX_SEMANTIC_REPLAY_PAGE_BYTES: usize = 256 * 1024;
const MAX_MOBILE_REPLAY_EVENTS: usize = 5_000;
const MAX_MOBILE_REPLAY_BYTES: usize = 2 * 1024 * 1024;
const MAX_COMPOSER_MUTATION_ID_BYTES: usize = 128;
const MAX_COMPOSER_ERROR_BYTES: usize = 1024;
// At the mutation-ID limit, new prompts fail closed until this host runtime
// restarts. 16,384 entries supports one unique phone submission roughly every
// five seconds for a full day without ever forgetting an at-most-once result.
const MAX_COMPOSER_MUTATION_RECORDS: usize = 16_384;
const MAX_COMPOSER_TEXT_BYTES: usize = 256 * 1024;
const MAX_COMPOSER_ATTACHMENTS: usize = 4;
const MAX_COMPOSER_ATTACHMENT_TOTAL_BYTES: usize = 10 * 1024 * 1024;
const MAX_COMPOSER_FILE_NAME_BYTES: usize = 255;
const MAX_RESUME_ROUTE_BYTES: usize = 2048;
const MAX_RESUME_CAPTURE_ATTEMPTS: usize = 4;
const MAX_CLIENT_INSTANCE_ID_BYTES: usize = 128;
const MAX_STABLE_SESSION_KEY_BYTES: usize = 512;
const MAX_SESSION_ID_BYTES: usize = 512;
const MAX_SESSION_SUBSCRIPTIONS: usize = 256;

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct WsConnectQuery {
    browser_install_id: Option<String>,
}

fn authorize_ws_request(state: &WebState, headers: &HeaderMap) -> Result<String, StatusCode> {
    if !request_is_same_origin(headers) {
        return Err(StatusCode::FORBIDDEN);
    }
    authenticate_request(state, headers).ok_or(StatusCode::UNAUTHORIZED)
}

pub(crate) async fn ws_handler(
    State(state): State<Arc<WebState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Query(query): Query<WsConnectQuery>,
    ws: WebSocketUpgrade,
    headers: HeaderMap,
) -> Response {
    let client_id = match authorize_ws_request(&state, &headers) {
        Ok(client_id) => client_id,
        Err(StatusCode::FORBIDDEN) => {
            return (StatusCode::FORBIDDEN, "cross-origin websocket rejected").into_response();
        }
        Err(_) => {
            return (
                StatusCode::UNAUTHORIZED,
                "missing or invalid web auth cookie",
            )
                .into_response();
        }
    };
    let inner = state.inner.clone();
    if let Err(error) = record_browser_connection(
        &inner,
        &client_id,
        addr.ip(),
        query.browser_install_id,
        &headers,
    ) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to record browser connection: {error}"),
        )
            .into_response();
    }
    ws.on_upgrade(move |socket| run_session(socket, inner, client_id))
}

fn initial_web_hello(client_id: &str, snapshot: &RemoteWorkspaceSnapshot) -> WsOutbound {
    WsOutbound::Hello {
        client_id: client_id.to_string(),
        server_id: snapshot.server_id.clone(),
        protocol_version: WEB_PROTOCOL_VERSION,
        web_build_id: WEB_BUILD_ID.to_string(),
    }
}

fn queue_initial_browser_hello(
    outbound: &BrowserOutboundSender,
    inner: &Arc<RemoteHostInner>,
    client_id: &str,
) -> Result<(), BrowserEnqueueError> {
    let snapshot = light_snapshot(inner, client_id);
    outbound.try_send(initial_web_hello(client_id, &snapshot))
}

fn queue_initial_browser_snapshot(
    outbound: &BrowserOutboundSender,
    inner: &Arc<RemoteHostInner>,
    connection_id: u64,
    client_id: &str,
) -> Result<(), BrowserEnqueueError> {
    let snapshot = light_snapshot(inner, client_id);
    outbound.try_send_server_message(
        &ServerMessage::Snapshot { snapshot },
        inner,
        connection_id,
        client_id,
    )
}

async fn run_session(socket: WebSocket, inner: Arc<RemoteHostInner>, client_id: String) {
    let connection_id = inner.next_connection_id.fetch_add(1, Ordering::Relaxed);
    let (outbound, outbound_rx) =
        BrowserOutboundSender::channel(WEB_PUSH_CHANNEL_CAPACITY, WEB_OUTBOUND_MAX_BYTES);
    let tombstone = outbound.tombstone();
    // Seed hello while this sender is still private. Once the client enters
    // the broadcaster map, concurrent deltas may enqueue immediately; keeping
    // hello ahead of registration makes the first-frame contract absolute.
    if queue_initial_browser_hello(&outbound, &inner, &client_id).is_err() {
        tombstone.deactivate();
        return;
    }
    if !register_browser_client(&inner, connection_id, &client_id, outbound.clone()) {
        let _ = outbound
            .try_send_disconnect("This browser is no longer paired with the host.".to_string());
        tombstone.deactivate();
    }

    // Push an initial snapshot so the browser has state to render before any
    // delta arrives. We deliberately use a *lightweight* snapshot that omits
    // `session_views` — fetching those calls `session_bootstrap_provider`
    // which in turn grabs `process_manager` locks, and any session that's
    // mid-spawn (e.g. a Claude Code PTY still running `npx` download) can
    // hold those locks indefinitely and stall every new WS connect.
    //
    // The browser doesn't NEED the bootstrap map on connect: when it clicks
    // a row it subscribes, and the next `push_session_output` call fires
    // `auto_bootstrap_subscribed_clients` which sends the bootstrap as a
    // `SessionStream { Bootstrap }` event. That path is async to the initial
    // snapshot and can't stall the handshake.
    let _ = queue_initial_browser_snapshot(&outbound, &inner, connection_id, &client_id);

    let (mut ws_sink, mut ws_stream) = socket.split();
    let writer_inner = inner.clone();
    let writer_client_id = client_id.clone();
    let writer_tombstone = tombstone.clone();
    let writer_task = tokio::spawn(async move {
        run_browser_writer(
            &mut ws_sink,
            outbound_rx,
            writer_inner,
            connection_id,
            writer_client_id,
            writer_tombstone,
            WEB_OUTBOUND_STALL_TIMEOUT,
        )
        .await;
    });

    // Reader loop: handle inbound WS messages directly against
    // `RemoteHostInner` state. We do not await while holding any std lock.
    while let Some(frame) = ws_stream.next().await {
        match frame {
            Ok(WsMessage::Text(text)) => match serde_json::from_str::<WsInbound>(&text) {
                Ok(inbound) => {
                    handle_inbound_browser(&inner, connection_id, &client_id, inbound, &outbound);
                }
                Err(error) => {
                    let _ = outbound.try_send_disconnect(format!("invalid inbound frame: {error}"));
                    break;
                }
            },
            Ok(WsMessage::Binary(_)) => {
                // Reserved for future binary client→server frames (raw
                // terminal byte input). Ignored for now.
            }
            Ok(WsMessage::Ping(payload)) => {
                // axum auto-responds with pong; nothing to do.
                let _ = payload;
            }
            Ok(WsMessage::Pong(_)) | Ok(WsMessage::Close(_)) => break,
            Err(_) => break,
        }
    }

    // Teardown order matters: remove from clients first so the broadcaster
    // stops pushing into a dying channel, then let the drainer + writer wind
    // down.
    unregister_browser_client(&inner, connection_id, &client_id, &tombstone);
    drop(outbound);
    let _ = writer_task.await;
}

async fn run_browser_writer<S>(
    ws_sink: &mut S,
    mut outbound: BrowserOutboundReceiver,
    inner: Arc<RemoteHostInner>,
    connection_id: u64,
    client_id: String,
    tombstone: Arc<WebConnectionTombstone>,
    stall_timeout: Duration,
) where
    S: Sink<WsMessage> + Unpin,
{
    let mut cancellation = outbound.tombstone.subscribe();
    let mut delivery_failed = false;
    'commands: loop {
        let accounted = tokio::select! {
            biased;
            command = outbound.rx.recv() => match command {
                Some(command) => command,
                None => break,
            },
            changed = cancellation.changed() => {
                if changed.is_err() || !*cancellation.borrow() {
                    break;
                }
                continue;
            }
        };
        match &accounted.command {
            BrowserOutboundCommand::Frame {
                frame,
                deliver_when_inactive,
                closes_connection,
            } => {
                if tombstone.is_active() || *deliver_when_inactive {
                    if send_browser_frame(ws_sink, frame, stall_timeout)
                        .await
                        .is_err()
                    {
                        delivery_failed = true;
                        break;
                    }
                }
                if *closes_connection {
                    break;
                }
            }
            BrowserOutboundCommand::ReplayWake { epoch } => {
                if !tombstone.is_active() || outbound.replay_epoch.load(Ordering::Acquire) != *epoch
                {
                    continue;
                }
                let replay_deadline = tokio::time::Instant::now() + stall_timeout;
                loop {
                    if !tombstone.is_active()
                        || outbound.replay_epoch.load(Ordering::Acquire) != *epoch
                    {
                        break;
                    }
                    let (frame, complete) = {
                        let mut slot = outbound
                            .replay_slot
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        let Some(pending) = slot.as_mut().filter(|pending| pending.epoch == *epoch)
                        else {
                            break;
                        };
                        let (frame, complete) = if let Some(prefix) = pending.prefix.take() {
                            (Ok(Some(prefix)), false)
                        } else {
                            let frame = pending.encoder.next_frame();
                            let complete = pending.encoder.finished;
                            (frame, complete)
                        };
                        if complete {
                            *slot = None;
                        }
                        (frame, complete)
                    };
                    let frame = match frame {
                        Ok(Some(frame)) => frame,
                        Ok(None) => break,
                        Err(_) => {
                            delivery_failed = true;
                            break 'commands;
                        }
                    };
                    if !tombstone.is_active()
                        || outbound.replay_epoch.load(Ordering::Acquire) != *epoch
                    {
                        break;
                    }
                    if send_browser_frame_until(ws_sink, &frame, replay_deadline)
                        .await
                        .is_err()
                    {
                        delivery_failed = true;
                        break 'commands;
                    }
                    if complete {
                        break;
                    }
                }
            }
        }
    }
    if delivery_failed {
        unregister_browser_client(&inner, connection_id, &client_id, &tombstone);
    }
    let _ = tokio::time::timeout(stall_timeout, ws_sink.close()).await;
}

async fn send_browser_frame<S>(
    ws_sink: &mut S,
    frame: &EncodedFrame,
    stall_timeout: Duration,
) -> Result<(), ()>
where
    S: Sink<WsMessage> + Unpin,
{
    let message = match frame {
        EncodedFrame::Text(text) => WsMessage::Text(text.clone()),
        EncodedFrame::Binary(bytes) => WsMessage::Binary(bytes.clone()),
    };
    match tokio::time::timeout(stall_timeout, ws_sink.send(message)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(_)) | Err(_) => Err(()),
    }
}

async fn send_browser_frame_until<S>(
    ws_sink: &mut S,
    frame: &EncodedFrame,
    deadline: tokio::time::Instant,
) -> Result<(), ()>
where
    S: Sink<WsMessage> + Unpin,
{
    let message = match frame {
        EncodedFrame::Text(text) => WsMessage::Text(text.clone()),
        EncodedFrame::Binary(bytes) => WsMessage::Binary(bytes.clone()),
    };
    match tokio::time::timeout_at(deadline, ws_sink.send(message)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(_)) | Err(_) => Err(()),
    }
}

/// Lightweight snapshot for web client handshakes. Reads only
/// `std::sync::RwLock` guards held for microseconds each, and specifically
/// does NOT call `session_bootstrap_provider` (which fans out into
/// `process_manager` and can stall on a mid-spawn PTY). Use this instead of
/// `current_snapshot` when you need the shared state but not the terminal
/// replay scrollback.
fn light_snapshot(inner: &Arc<RemoteHostInner>, client_id: &str) -> RemoteWorkspaceSnapshot {
    let app_state = inner
        .shared_state
        .read()
        .map(|slot| slot.clone())
        .unwrap_or_default();
    let runtime_state = inner
        .runtime_state
        .read()
        .map(|slot| slot.clone())
        .unwrap_or_default();
    let port_statuses = inner
        .port_statuses
        .read()
        .map(|slot| slot.clone())
        .unwrap_or_default();
    let controller_client_id = inner
        .controller_client_id
        .read()
        .map(|slot| slot.clone())
        .unwrap_or_default();
    let server_id = inner
        .config
        .read()
        .map(|cfg| cfg.server_id.clone())
        .unwrap_or_default();
    let you_have_control = controller_client_id.as_deref() == Some(client_id);
    RemoteWorkspaceSnapshot {
        app_state,
        runtime_state,
        session_views: HashMap::new(),
        port_statuses,
        controller_client_id,
        you_have_control,
        server_id,
    }
}

fn register_browser_client(
    inner: &Arc<RemoteHostInner>,
    connection_id: u64,
    client_id: &str,
    web_sender: BrowserOutboundSender,
) -> bool {
    let app_state = inner
        .shared_state
        .read()
        .map(|slot| slot.clone())
        .unwrap_or_default();
    let runtime_state = inner
        .runtime_state
        .read()
        .map(|slot| slot.clone())
        .unwrap_or_default();
    let port_statuses = inner
        .port_statuses
        .read()
        .map(|slot| slot.clone())
        .unwrap_or_default();
    let controller_client_id = inner
        .controller_client_id
        .read()
        .map(|slot| slot.clone())
        .unwrap_or_default();
    let you_have_control = controller_client_id.as_deref() == Some(client_id);
    let _operation = inner
        .web_control_operation_lock
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if !web_client_is_still_paired(inner, client_id) {
        return false;
    }
    let Ok(mut clients) = inner.clients.lock() else {
        return false;
    };
    if clients.contains_key(&connection_id) {
        return false;
    }
    let tombstone = web_sender.tombstone();

    clients.insert(
        connection_id,
        ConnectedRemoteClient {
            client_id: client_id.to_string(),
            sender: None,
            web_sender: Some(web_sender),
            web_tombstone: Some(tombstone),
            semantic_cursors: HashMap::new(),
            subscribed_session_ids: HashSet::new(),
            bootstrapped_session_ids: HashSet::new(),
            bootstrap_pending_session_ids: HashSet::new(),
            focused_session_id: None,
            last_app_hash: stable_hash(&app_state),
            last_runtime_hash: stable_hash(&runtime_state),
            last_port_hash: stable_hash(&port_statuses),
            last_controller_client_id: controller_client_id,
            last_you_have_control: you_have_control,
            last_snapshot_revision: inner.snapshot_revision.load(Ordering::Relaxed),
        },
    );
    true
}

fn unregister_browser_client(
    inner: &Arc<RemoteHostInner>,
    connection_id: u64,
    client_id: &str,
    tombstone: &Arc<WebConnectionTombstone>,
) {
    revoke_web_connection(inner, connection_id, client_id, tombstone, None);
}

#[cfg(test)]
fn register_client(
    inner: &Arc<RemoteHostInner>,
    connection_id: u64,
    client_id: &str,
    _native_sender: std_mpsc::Sender<ServerMessage>,
    web_sender: BrowserOutboundSender,
) -> bool {
    register_browser_client(inner, connection_id, client_id, web_sender)
}

#[cfg(test)]
fn unregister_client(inner: &Arc<RemoteHostInner>, connection_id: u64, client_id: &str) {
    let tombstone = inner.clients.lock().ok().and_then(|clients| {
        clients
            .get(&connection_id)
            .filter(|client| client.client_id == client_id)
            .and_then(|client| client.web_tombstone.clone())
    });
    if let Some(tombstone) = tombstone {
        unregister_browser_client(inner, connection_id, client_id, &tombstone);
    }
}

pub(crate) fn revoke_web_connection(
    inner: &Arc<RemoteHostInner>,
    connection_id: u64,
    client_id: &str,
    tombstone: &Arc<WebConnectionTombstone>,
    reason: Option<String>,
) -> bool {
    let _operation = inner
        .web_control_operation_lock
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let revoked = revoke_web_connection_locked(inner, connection_id, client_id, tombstone, reason);
    if revoked {
        broadcast_writer_lease_state_locked(inner, now_epoch_ms());
    }
    revoked
}

pub(crate) fn revoke_web_connection_locked(
    inner: &Arc<RemoteHostInner>,
    connection_id: u64,
    client_id: &str,
    tombstone: &Arc<WebConnectionTombstone>,
    reason: Option<String>,
) -> bool {
    let (removed, tombstone_registered_elsewhere) = inner
        .clients
        .lock()
        .map(|mut clients| {
            let exact = clients.get(&connection_id).is_some_and(|client| {
                client.client_id == client_id
                    && client
                        .web_tombstone
                        .as_ref()
                        .is_some_and(|registered| Arc::ptr_eq(registered, tombstone))
            });
            let removed = exact.then(|| clients.remove(&connection_id)).flatten();
            let tombstone_registered_elsewhere = removed.is_none()
                && clients.values().any(|client| {
                    client
                        .web_tombstone
                        .as_ref()
                        .is_some_and(|registered| Arc::ptr_eq(registered, tombstone))
                });
            (removed, tombstone_registered_elsewhere)
        })
        .unwrap_or((None, false));
    let Some(removed) = removed else {
        if !tombstone_registered_elsewhere {
            tombstone.deactivate();
        }
        return false;
    };
    if let (Some(sender), Some(reason)) = (removed.web_sender.as_ref(), reason) {
        let _ = sender.try_send_disconnect(reason);
    }
    tombstone.deactivate();
    let release = inner
        .web_control
        .lock()
        .map(|mut control| control.release_connection(connection_id, client_id))
        .unwrap_or_default();
    let same_client_browser_remains = inner.clients.lock().ok().is_some_and(|clients| {
        clients.values().any(|client| {
            client.client_id == client_id
                && client
                    .web_tombstone
                    .as_ref()
                    .is_some_and(|registered| registered.is_active())
        })
    });
    let clear_controller = release.released_lease.is_some()
        || release.legacy_released
        || (!release.lease_release_deferred && !same_client_browser_remains);
    if clear_controller {
        if let Ok(mut controller) = inner.controller_client_id.write() {
            if controller.as_deref() == Some(client_id) {
                *controller = None;
            }
        }
    }
    true
}

fn web_client_is_still_paired(inner: &Arc<RemoteHostInner>, client_id: &str) -> bool {
    inner
        .config
        .read()
        .map(|config| {
            config
                .web
                .paired_clients
                .iter()
                .any(|client| client.client_id == client_id)
        })
        .unwrap_or(false)
}

fn web_connection_is_registered(
    inner: &Arc<RemoteHostInner>,
    connection_id: u64,
    client_id: &str,
    expected_tombstone: Option<&Arc<WebConnectionTombstone>>,
) -> bool {
    inner
        .clients
        .lock()
        .map(|clients| {
            clients.get(&connection_id).is_some_and(|client| {
                client.client_id == client_id
                    && client.web_sender.is_some()
                    && client.web_tombstone.as_ref().is_some_and(|registered| {
                        registered.is_active()
                            && expected_tombstone
                                .is_none_or(|expected| Arc::ptr_eq(registered, expected))
                    })
            })
        })
        .unwrap_or(false)
}

fn web_connection_is_authoritative_locked(
    inner: &Arc<RemoteHostInner>,
    connection_id: u64,
    client_id: &str,
    expected_tombstone: Option<&Arc<WebConnectionTombstone>>,
) -> bool {
    web_client_is_still_paired(inner, client_id)
        && registered_web_tombstone(inner, connection_id, client_id, expected_tombstone).is_some()
}

fn registered_web_tombstone(
    inner: &Arc<RemoteHostInner>,
    connection_id: u64,
    client_id: &str,
    expected_tombstone: Option<&Arc<WebConnectionTombstone>>,
) -> Option<Arc<WebConnectionTombstone>> {
    inner.clients.lock().ok().and_then(|clients| {
        let client = clients.get(&connection_id)?;
        let registered = client.web_tombstone.as_ref()?;
        (client.client_id == client_id
            && client.web_sender.is_some()
            && registered.is_active()
            && expected_tombstone.is_none_or(|expected| Arc::ptr_eq(registered, expected)))
        .then(|| registered.clone())
    })
}

#[cfg(test)]
fn handle_inbound(
    inner: &Arc<RemoteHostInner>,
    connection_id: u64,
    client_id: &str,
    message: WsInbound,
    tokio_tx: &tokio_mpsc::UnboundedSender<ServerMessage>,
) {
    let (web_tx, _web_rx) = tokio_mpsc::unbounded_channel();
    handle_inbound_with_web(inner, connection_id, client_id, message, tokio_tx, &web_tx);
}

#[cfg(test)]
fn handle_inbound_with_web(
    inner: &Arc<RemoteHostInner>,
    connection_id: u64,
    client_id: &str,
    message: WsInbound,
    tokio_tx: &tokio_mpsc::UnboundedSender<ServerMessage>,
    web_tx: &tokio_mpsc::UnboundedSender<WsOutbound>,
) {
    handle_inbound_core(
        inner,
        connection_id,
        client_id,
        message,
        InboundResponder::Test {
            native: tokio_tx.clone(),
            web: web_tx.clone(),
        },
    );
}

fn handle_inbound_browser(
    inner: &Arc<RemoteHostInner>,
    connection_id: u64,
    client_id: &str,
    message: WsInbound,
    sender: &BrowserOutboundSender,
) {
    handle_inbound_core(
        inner,
        connection_id,
        client_id,
        message,
        InboundResponder::Browser {
            sender: sender.clone(),
            inner: inner.clone(),
            connection_id,
            client_id: client_id.to_string(),
        },
    );
}

#[derive(Clone)]
enum InboundResponder {
    Browser {
        sender: BrowserOutboundSender,
        inner: Arc<RemoteHostInner>,
        connection_id: u64,
        client_id: String,
    },
    #[cfg(test)]
    Test {
        native: tokio_mpsc::UnboundedSender<ServerMessage>,
        web: tokio_mpsc::UnboundedSender<WsOutbound>,
    },
}

impl InboundResponder {
    fn tombstone(&self) -> Option<Arc<WebConnectionTombstone>> {
        match self {
            Self::Browser { sender, .. } => Some(sender.tombstone()),
            #[cfg(test)]
            Self::Test { .. } => None,
        }
    }

    fn send_server(&self, message: ServerMessage) -> Result<(), BrowserEnqueueError> {
        match self {
            Self::Browser {
                sender,
                inner,
                connection_id,
                client_id,
            } => {
                let result =
                    sender.try_send_server_message(&message, inner, *connection_id, client_id);
                if result.is_err() {
                    revoke_web_connection(
                        inner,
                        *connection_id,
                        client_id,
                        &sender.tombstone(),
                        None,
                    );
                }
                result
            }
            #[cfg(test)]
            Self::Test { native, .. } => native
                .send(message)
                .map_err(|_| BrowserEnqueueError::Closed),
        }
    }

    fn send_web(&self, message: WsOutbound) -> Result<(), BrowserEnqueueError> {
        match self {
            Self::Browser {
                sender,
                inner,
                connection_id,
                client_id,
            } => {
                let result = sender.try_send(message);
                if result.is_err() {
                    revoke_web_connection(
                        inner,
                        *connection_id,
                        client_id,
                        &sender.tombstone(),
                        None,
                    );
                }
                result
            }
            #[cfg(test)]
            Self::Test { web, .. } => web.send(message).map_err(|_| BrowserEnqueueError::Closed),
        }
    }
}

#[derive(Clone)]
struct ServerResponseLane(InboundResponder);

impl ServerResponseLane {
    fn send(&self, message: ServerMessage) -> Result<(), BrowserEnqueueError> {
        self.0.send_server(message)
    }
}

#[derive(Clone)]
struct WebResponseLane(InboundResponder);

impl WebResponseLane {
    fn send(&self, message: WsOutbound) -> Result<(), BrowserEnqueueError> {
        self.0.send_web(message)
    }

    fn is_test_lane(&self) -> bool {
        #[cfg(test)]
        {
            matches!(self.0, InboundResponder::Test { .. })
        }
        #[cfg(not(test))]
        {
            false
        }
    }
}

fn handle_inbound_core(
    inner: &Arc<RemoteHostInner>,
    connection_id: u64,
    client_id: &str,
    message: WsInbound,
    outbound: InboundResponder,
) {
    let tokio_tx = ServerResponseLane(outbound.clone());
    let web_tx = WebResponseLane(outbound);
    let expected_tombstone = web_tx.0.tombstone();
    if !web_client_is_still_paired(inner, client_id)
        || !web_connection_is_registered(
            inner,
            connection_id,
            client_id,
            expected_tombstone.as_ref(),
        )
    {
        match &web_tx.0 {
            InboundResponder::Browser { sender, .. } => {
                unregister_browser_client(inner, connection_id, client_id, &sender.tombstone());
            }
            #[cfg(test)]
            InboundResponder::Test { .. } => {
                if let Ok(mut clients) = inner.clients.lock() {
                    clients.remove(&connection_id);
                }
            }
        }
        let _ = tokio_tx.send(ServerMessage::Disconnected {
            message: "This browser connection is no longer active. Reconnect or pair again."
                .to_string(),
        });
        return;
    }

    match message {
        WsInbound::Resume { request } => {
            send_resume_state_with_lane(
                inner,
                connection_id,
                client_id,
                request,
                now_epoch_ms(),
                &web_tx,
                expected_tombstone.as_ref(),
            );
        }
        WsInbound::AcquireWriterLease {
            client_instance_id,
            visible,
        } => {
            if !valid_client_instance_id(&client_instance_id) {
                let _ = web_tx.send(WsOutbound::Error {
                    message: "Client instance IDs must be 1-128 bytes.".to_string(),
                });
                return;
            }
            // The helper broadcasts connection-specific state to every
            // browser, including this requester; a direct reply would race it.
            if visible {
                let _ = acquire_writer_lease_for_connection(
                    inner,
                    connection_id,
                    client_id,
                    &client_instance_id,
                    now_epoch_ms(),
                    expected_tombstone.as_ref(),
                );
            } else {
                let _ = set_writer_visibility_for_connection(
                    inner,
                    connection_id,
                    client_id,
                    &client_instance_id,
                    false,
                    now_epoch_ms(),
                    expected_tombstone.as_ref(),
                );
            }
        }
        WsInbound::WriterLeaseHeartbeat {
            client_instance_id,
            expected_lease_generation,
            visible,
        } => {
            if !valid_client_instance_id(&client_instance_id) {
                let _ = web_tx.send(WsOutbound::Error {
                    message: "Client instance IDs must be 1-128 bytes.".to_string(),
                });
                return;
            }
            // Renewal broadcasts its authoritative result to this requester.
            let heartbeat_at_epoch_ms = now_epoch_ms();
            let _ = renew_writer_lease(
                inner,
                connection_id,
                client_id,
                &client_instance_id,
                expected_lease_generation,
                visible,
                heartbeat_at_epoch_ms,
                expected_tombstone.as_ref(),
            );
        }
        WsInbound::SetVisibility {
            client_instance_id,
            visible,
        } => {
            if !valid_client_instance_id(&client_instance_id) {
                let _ = web_tx.send(WsOutbound::Error {
                    message: "Client instance IDs must be 1-128 bytes.".to_string(),
                });
                return;
            }
            // Visibility changes use the same ordered broadcast-only path.
            let _ = set_writer_visibility_for_connection(
                inner,
                connection_id,
                client_id,
                &client_instance_id,
                visible,
                now_epoch_ms(),
                expected_tombstone.as_ref(),
            );
        }
        WsInbound::ComposerSubmit {
            mutation_id,
            stable_session_key,
            text,
            attachments,
            expected_lease_generation,
        } => {
            match dispatch_composer_submit_for_connection(
                inner,
                connection_id,
                client_id,
                mutation_id,
                stable_session_key,
                text,
                attachments,
                expected_lease_generation,
                now_epoch_ms(),
                expected_tombstone.as_ref(),
                ComposerCompletion::Web(web_tx.clone()),
            ) {
                Ok(Some(accepted)) => {
                    let _ = web_tx.send(WsOutbound::ComposerAccepted { accepted });
                }
                Ok(None) => {}
                Err(rejected) => {
                    let _ = web_tx.send(WsOutbound::ComposerRejected { rejected });
                }
            }
        }
        WsInbound::SubscribeSemantic {
            stable_session_key,
            after_sequence,
        } => {
            if !valid_stable_session_key(&stable_session_key) {
                let _ = web_tx.send(WsOutbound::Error {
                    message: "Semantic session key is empty or too long.".to_string(),
                });
            } else {
                subscribe_semantic(
                    inner,
                    connection_id,
                    client_id,
                    stable_session_key,
                    after_sequence,
                    expected_tombstone.as_ref(),
                )
            }
        }
        WsInbound::UnsubscribeSemantic { stable_session_key } => {
            if !valid_stable_session_key(&stable_session_key) {
                let _ = web_tx.send(WsOutbound::Error {
                    message: "Semantic session key is empty or too long.".to_string(),
                });
            } else {
                unsubscribe_semantic(
                    inner,
                    connection_id,
                    client_id,
                    &stable_session_key,
                    expected_tombstone.as_ref(),
                )
            }
        }
        WsInbound::InterruptSession {
            stable_session_key,
            expected_lease_generation,
        } => {
            if !valid_stable_session_key(&stable_session_key) {
                let _ = tokio_tx.send(ServerMessage::Error {
                    message: "The semantic session key is empty or too long.".to_string(),
                });
                return;
            }
            let result = enqueue_terminal_input(
                inner,
                connection_id,
                client_id,
                expected_tombstone.as_ref(),
                Some(expected_lease_generation),
                now_epoch_ms(),
                stable_session_key,
                1,
                tokio_tx.clone(),
                |session_id| RemoteTerminalInput::Bytes {
                    session_id,
                    bytes: vec![0x03],
                },
            );
            match result {
                Ok(()) => {}
                Err(WebInputEnqueueError::Authority) => {
                    let _ = tokio_tx.send(ServerMessage::Error {
                        message: "The writer lease changed before the interrupt was accepted."
                            .to_string(),
                    });
                }
                Err(WebInputEnqueueError::QueueUnavailable) => {
                    let _ = tokio_tx.send(ServerMessage::Error {
                        message: "Terminal input is busy. Retry the interrupt.".to_string(),
                    });
                }
            }
        }
        WsInbound::Ping => {
            let _ = tokio_tx.send(ServerMessage::Pong);
        }
        WsInbound::SubscribeSessions { session_ids } => {
            if session_ids.len() > MAX_SESSION_SUBSCRIPTIONS
                || session_ids
                    .iter()
                    .any(|session_id| !valid_session_id(session_id))
            {
                let _ = tokio_tx.send(ServerMessage::Error {
                    message: "Session subscriptions are too large or invalid.".to_string(),
                });
                return;
            }
            if let Ok(mut clients) = inner.clients.lock() {
                if let Some(client) = clients.get_mut(&connection_id).filter(|client| {
                    client.client_id == client_id
                        && client.web_tombstone.as_ref().is_some_and(|registered| {
                            expected_tombstone
                                .as_ref()
                                .is_none_or(|expected| Arc::ptr_eq(registered, expected))
                        })
                }) {
                    for session_id in &session_ids {
                        client.subscribed_session_ids.insert(session_id.clone());
                        if !client.bootstrapped_session_ids.contains(session_id) {
                            client
                                .bootstrap_pending_session_ids
                                .insert(session_id.clone());
                        }
                    }
                }
            }

            let bootstraps = inner
                .session_bootstrap_provider
                .read()
                .ok()
                .and_then(|provider| provider.as_ref().cloned())
                .map(|provider| {
                    session_ids
                        .iter()
                        .filter_map(|session_id| {
                            provider(session_id).map(|bootstrap| (session_id.clone(), bootstrap))
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            for (session_id, bootstrap) in bootstraps {
                let still_subscribed = inner.clients.lock().ok().is_some_and(|clients| {
                    clients.get(&connection_id).is_some_and(|client| {
                        client.client_id == client_id
                            && client.web_tombstone.as_ref().is_some_and(|registered| {
                                expected_tombstone
                                    .as_ref()
                                    .is_none_or(|expected| Arc::ptr_eq(registered, expected))
                            })
                            && client.subscribed_session_ids.contains(&session_id)
                    })
                });
                if !still_subscribed {
                    continue;
                }
                if tokio_tx
                    .send(ServerMessage::SessionStream {
                        event: RemoteSessionStreamEvent::Bootstrap { bootstrap },
                    })
                    .is_ok()
                {
                    if let Ok(mut clients) = inner.clients.lock() {
                        if let Some(client) = clients.get_mut(&connection_id).filter(|client| {
                            client.client_id == client_id
                                && client.web_tombstone.as_ref().is_some_and(|registered| {
                                    expected_tombstone
                                        .as_ref()
                                        .is_none_or(|expected| Arc::ptr_eq(registered, expected))
                                })
                                && client.subscribed_session_ids.contains(&session_id)
                        }) {
                            client.bootstrapped_session_ids.insert(session_id.clone());
                            client.bootstrap_pending_session_ids.remove(&session_id);
                        }
                    }
                }
            }
        }
        WsInbound::UnsubscribeSessions { session_ids } => {
            if session_ids.len() > MAX_SESSION_SUBSCRIPTIONS
                || session_ids
                    .iter()
                    .any(|session_id| !valid_session_id(session_id))
            {
                let _ = tokio_tx.send(ServerMessage::Error {
                    message: "Session subscriptions are too large or invalid.".to_string(),
                });
                return;
            }
            if let Ok(mut clients) = inner.clients.lock() {
                if let Some(client) = clients.get_mut(&connection_id).filter(|client| {
                    client.client_id == client_id
                        && client.web_tombstone.as_ref().is_some_and(|registered| {
                            expected_tombstone
                                .as_ref()
                                .is_none_or(|expected| Arc::ptr_eq(registered, expected))
                        })
                }) {
                    for session_id in &session_ids {
                        client.subscribed_session_ids.remove(session_id);
                        client.bootstrapped_session_ids.remove(session_id);
                        client.bootstrap_pending_session_ids.remove(session_id);
                    }
                }
            }
        }
        WsInbound::FocusSession { session_id } => {
            if !valid_session_id(&session_id) {
                let _ = tokio_tx.send(ServerMessage::Error {
                    message: "Session ID is empty or too long.".to_string(),
                });
                return;
            }
            if let Ok(mut clients) = inner.clients.lock() {
                if let Some(client) = clients.get_mut(&connection_id).filter(|client| {
                    client.client_id == client_id
                        && client.web_tombstone.as_ref().is_some_and(|registered| {
                            expected_tombstone
                                .as_ref()
                                .is_none_or(|expected| Arc::ptr_eq(registered, expected))
                        })
                }) {
                    client.focused_session_id = Some(session_id);
                }
            }
        }
        WsInbound::Input {
            session_id,
            text,
            expected_lease_generation,
        } => {
            if !valid_session_id(&session_id) || text.len() > MAX_COMPOSER_TEXT_BYTES {
                let _ = tokio_tx.send(ServerMessage::Error {
                    message: "Terminal input is empty-session or exceeds 256 KiB.".to_string(),
                });
                return;
            }
            let Some(stable_session_key) = stable_key_for_session_id(inner, &session_id) else {
                let _ = tokio_tx.send(ServerMessage::Error {
                    message: "The requested session no longer exists.".to_string(),
                });
                return;
            };
            let retained_bytes = text.len();
            let result = enqueue_terminal_input(
                inner,
                connection_id,
                client_id,
                expected_tombstone.as_ref(),
                expected_lease_generation,
                now_epoch_ms(),
                stable_session_key,
                retained_bytes,
                tokio_tx.clone(),
                move |current_session_id| RemoteTerminalInput::Text {
                    session_id: current_session_id,
                    text,
                },
            );
            match result {
                Ok(()) | Err(WebInputEnqueueError::Authority) => {
                    // Viewer-mode typing is a no-op on the host, matching the
                    // native TCP client's behavior.
                }
                Err(WebInputEnqueueError::QueueUnavailable) => {
                    let _ = tokio_tx.send(ServerMessage::Error {
                        message: "Terminal input is busy. Retry the input.".to_string(),
                    });
                }
            }
        }
        WsInbound::PasteImage {
            session_id,
            mime_type,
            file_name,
            data_base64,
            expected_lease_generation,
        } => {
            use super::image_paste::WEB_PASTE_IMAGE_MAX_BYTES;

            let max_encoded_bytes = WEB_PASTE_IMAGE_MAX_BYTES.div_ceil(3) * 4 + 4;
            if !valid_session_id(&session_id)
                || !matches!(mime_type.as_str(), "image/png" | "image/jpeg")
                || file_name
                    .as_ref()
                    .is_some_and(|name| name.len() > MAX_COMPOSER_FILE_NAME_BYTES)
                || data_base64.len() > max_encoded_bytes
            {
                let _ = tokio_tx.send(ServerMessage::Error {
                    message:
                        "Pasted images must be PNG/JPEG, at most 5 MiB, with a short file name."
                            .to_string(),
                });
                return;
            }
            let bytes = match BASE64.decode(data_base64.as_bytes()) {
                Ok(bytes) => bytes,
                Err(error) => {
                    let _ = tokio_tx.send(ServerMessage::Error {
                        message: format!("Invalid pasted image payload: {error}"),
                    });
                    return;
                }
            };
            if bytes.is_empty() || bytes.len() > WEB_PASTE_IMAGE_MAX_BYTES {
                let _ = tokio_tx.send(ServerMessage::Error {
                    message: "Pasted images must be non-empty and at most 5 MiB.".to_string(),
                });
                return;
            }
            let Some(stable_session_key) = stable_key_for_session_id(inner, &session_id) else {
                let _ = tokio_tx.send(ServerMessage::Error {
                    message: "The requested session no longer exists.".to_string(),
                });
                return;
            };
            let retained_bytes =
                bytes.len() + mime_type.len() + file_name.as_ref().map_or(0, String::len);
            let result = enqueue_terminal_input(
                inner,
                connection_id,
                client_id,
                expected_tombstone.as_ref(),
                expected_lease_generation,
                now_epoch_ms(),
                stable_session_key,
                retained_bytes,
                tokio_tx.clone(),
                move |current_session_id| RemoteTerminalInput::Image {
                    session_id: current_session_id,
                    attachment: RemoteImageAttachment {
                        mime_type,
                        file_name,
                        bytes,
                    },
                },
            );
            match result {
                Ok(()) => {}
                Err(WebInputEnqueueError::Authority) => {
                    let _ = tokio_tx.send(ServerMessage::Error {
                        message: "This client is in viewer mode. Take control first.".to_string(),
                    });
                }
                Err(WebInputEnqueueError::QueueUnavailable) => {
                    let _ = tokio_tx.send(ServerMessage::Error {
                        message: "Terminal input is busy. Retry the image paste.".to_string(),
                    });
                }
            }
        }
        WsInbound::Resize {
            session_id,
            rows,
            cols,
            expected_lease_generation,
        } => {
            if !valid_session_id(&session_id) {
                let _ = tokio_tx.send(ServerMessage::Error {
                    message: "Session ID is empty or too long.".to_string(),
                });
                return;
            }
            let Some(stable_session_key) = stable_key_for_session_id(inner, &session_id) else {
                let _ = tokio_tx.send(ServerMessage::Error {
                    message: "The requested session no longer exists.".to_string(),
                });
                return;
            };
            if matches!(
                enqueue_terminal_resize(
                    inner,
                    connection_id,
                    client_id,
                    expected_tombstone.as_ref(),
                    expected_lease_generation,
                    now_epoch_ms(),
                    stable_session_key,
                    tokio_tx.clone(),
                    SessionDimensions {
                        rows,
                        cols,
                        cell_width: 10,
                        cell_height: 20,
                    },
                ),
                Err(WebInputEnqueueError::QueueUnavailable)
            ) {
                let _ = tokio_tx.send(ServerMessage::Error {
                    message: "Terminal input is busy. Retry the resize.".to_string(),
                });
            }
        }
        WsInbound::Action {
            action,
            expected_lease_generation,
        } => {
            let action = action.into_remote();
            let requires_writer = requires_control(&action);
            let enqueue = || {
                try_enqueue_pending_request(
                    inner,
                    PendingRemoteRequest {
                        client_id: client_id.to_string(),
                        action,
                        response: None,
                    },
                )
                .is_ok()
            };
            let accepted = if requires_writer {
                with_web_mutation_authority(
                    inner,
                    connection_id,
                    client_id,
                    expected_tombstone.as_ref(),
                    expected_lease_generation,
                    now_epoch_ms(),
                    enqueue,
                )
            } else {
                with_registered_web_operation(
                    inner,
                    connection_id,
                    client_id,
                    expected_tombstone.as_ref(),
                    enqueue,
                )
            };
            match accepted {
                Some(true) => {}
                Some(false) => {
                    let _ = tokio_tx.send(ServerMessage::Error {
                        message: "Remote host is busy. Retry shortly.".to_string(),
                    });
                }
                None if requires_writer => {
                    let _ = tokio_tx.send(ServerMessage::Disconnected {
                        message: "viewer mode: take control before acting".to_string(),
                    });
                }
                None => {}
            }
        }
        WsInbound::Request {
            id,
            action,
            expected_lease_generation,
        } => {
            let action = action.into_remote();
            let requires_writer = requires_control(&action);
            let (response_tx, response_rx) = std_mpsc::channel();
            let timeout = request_timeout_for_action(&action);
            let deadline = Instant::now() + timeout;
            let (start_tx, start_rx) = std_mpsc::sync_channel(1);
            let response_lane = tokio_tx.clone();
            if inner
                .web_request_executor
                .dispatch(move || {
                    if !matches!(start_rx.recv(), Ok(true)) {
                        return;
                    }
                    let result = response_rx
                        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                        .unwrap_or_else(|_| RemoteActionResult::error("Remote host timed out."));
                    let _ = response_lane.send(ServerMessage::Response {
                        request_id: id,
                        result,
                    });
                })
                .is_err()
            {
                let _ = tokio_tx.send(ServerMessage::Response {
                    request_id: id,
                    result: RemoteActionResult::error("Remote host is busy. Retry shortly."),
                });
                return;
            }
            let enqueue = || {
                try_enqueue_pending_request(
                    inner,
                    PendingRemoteRequest {
                        client_id: client_id.to_string(),
                        action,
                        response: Some(response_tx),
                    },
                )
                .is_ok()
            };
            let accepted = if requires_writer {
                with_web_mutation_authority(
                    inner,
                    connection_id,
                    client_id,
                    expected_tombstone.as_ref(),
                    expected_lease_generation,
                    now_epoch_ms(),
                    enqueue,
                )
            } else {
                with_registered_web_operation(
                    inner,
                    connection_id,
                    client_id,
                    expected_tombstone.as_ref(),
                    enqueue,
                )
            };
            match accepted {
                Some(true) => {
                    let _ = start_tx.send(true);
                }
                Some(false) => {
                    let _ = start_tx.send(false);
                    let _ = tokio_tx.send(ServerMessage::Response {
                        request_id: id,
                        result: RemoteActionResult::error("Remote host is busy. Retry shortly."),
                    });
                }
                None => {
                    let _ = start_tx.send(false);
                    if !requires_writer {
                        return;
                    }
                    let _ = tokio_tx.send(ServerMessage::Response {
                        request_id: id,
                        result: RemoteActionResult::error(
                            "This client is in viewer mode. Take control first.",
                        ),
                    });
                }
            }
        }
        WsInbound::TakeControl => {
            claim_legacy_control_for_connection(
                inner,
                connection_id,
                client_id,
                true,
                expected_tombstone.as_ref(),
            );
        }
        WsInbound::ClaimControlIfAvailable => {
            claim_legacy_control_for_connection(
                inner,
                connection_id,
                client_id,
                false,
                expected_tombstone.as_ref(),
            );
        }
        WsInbound::ReleaseControl => {
            release_legacy_control_for_connection(
                inner,
                connection_id,
                client_id,
                expected_tombstone.as_ref(),
            );
        }
    }
}

fn subscribe_semantic(
    inner: &Arc<RemoteHostInner>,
    connection_id: u64,
    client_id: &str,
    stable_session_key: StableSessionKey,
    after_sequence: u64,
    expected_tombstone: Option<&Arc<WebConnectionTombstone>>,
) {
    let _operation = inner
        .web_control_operation_lock
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let _delivery = inner
        .semantic_delivery_lock
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if !web_connection_is_authoritative_locked(inner, connection_id, client_id, expected_tombstone)
    {
        return;
    }
    let Some((sender, tombstone)) = inner.clients.lock().ok().and_then(|clients| {
        let client = clients.get(&connection_id)?;
        if client.client_id != client_id
            || client.web_tombstone.as_ref().is_none_or(|registered| {
                expected_tombstone.is_some_and(|expected| !Arc::ptr_eq(registered, expected))
            })
        {
            return None;
        }
        Some((client.web_sender.clone()?, client.web_tombstone.clone()?))
    }) else {
        return;
    };
    let capture =
        inner.semantic_journals.lock().ok().and_then(|journals| {
            journals.capture_replay_after(&stable_session_key, after_sequence)
        });
    let replay = Arc::new(cap_semantic_replay_for_mobile(capture.map_or(
        SemanticReplay {
            oldest_sequence: 0,
            through_sequence: 0,
            cursor_rolled_over: false,
            events: Vec::new(),
        },
        |capture| capture.into_replay(),
    )));
    let through_sequence = replay.through_sequence;
    let epoch = sender.next_replay_epoch();
    let replay_id = sender.next_replay_id();
    let delivered = sender
        .try_send_replay(
            None,
            replay_id,
            stable_session_key.clone(),
            after_sequence,
            replay,
            epoch,
        )
        .is_ok();
    if delivered {
        if let Ok(mut clients) = inner.clients.lock() {
            if let Some(client) = clients.get_mut(&connection_id).filter(|client| {
                client.client_id == client_id
                    && client
                        .web_tombstone
                        .as_ref()
                        .is_some_and(|registered| Arc::ptr_eq(registered, &tombstone))
            }) {
                client
                    .semantic_cursors
                    .insert(stable_session_key, through_sequence);
                return;
            }
        }
    }
    drop(_delivery);
    revoke_web_connection_locked(inner, connection_id, client_id, &tombstone, None);
}

fn unsubscribe_semantic(
    inner: &Arc<RemoteHostInner>,
    connection_id: u64,
    client_id: &str,
    stable_session_key: &StableSessionKey,
    expected_tombstone: Option<&Arc<WebConnectionTombstone>>,
) {
    let _operation = inner
        .web_control_operation_lock
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let _delivery = inner
        .semantic_delivery_lock
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if !web_connection_is_authoritative_locked(inner, connection_id, client_id, expected_tombstone)
    {
        return;
    }
    if let Ok(mut clients) = inner.clients.lock() {
        if let Some(client) = clients.get_mut(&connection_id).filter(|client| {
            client.client_id == client_id
                && client.web_tombstone.as_ref().is_some_and(|registered| {
                    expected_tombstone.is_none_or(|expected| Arc::ptr_eq(registered, expected))
                })
        }) {
            client.semantic_cursors.remove(stable_session_key);
            if let Some(sender) = client.web_sender.as_ref() {
                sender.supersede_replay();
            }
        }
    }
}

fn with_web_mutation_authority<R>(
    inner: &Arc<RemoteHostInner>,
    connection_id: u64,
    client_id: &str,
    expected_tombstone: Option<&Arc<WebConnectionTombstone>>,
    expected_lease_generation: Option<u64>,
    now_epoch_ms: u64,
    operation: impl FnOnce() -> R,
) -> Option<R> {
    let _operation = inner
        .web_control_operation_lock
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if !web_client_is_still_paired(inner, client_id) {
        return None;
    }
    let permit = registered_web_tombstone(inner, connection_id, client_id, expected_tombstone)?;
    let (authorized, before, after) = {
        let Ok(mut control) = inner.web_control.lock() else {
            return None;
        };
        let before = control.writer_leases().peek();
        let authorized = match expected_lease_generation {
            Some(generation) => control
                .writer_leases_mut()
                .authorize(connection_id, client_id, generation, now_epoch_ms)
                .is_ok(),
            None => control.legacy_authorizes(connection_id, client_id),
        };
        let after = control.writer_leases().peek();
        (authorized, before, after)
    };
    clear_controller_after_lease_removal(inner, before.as_ref(), after.as_ref());
    let lease_changed = before != after;
    let controller_matches = inner
        .controller_client_id
        .read()
        .map(|controller| controller.as_deref() == Some(client_id))
        .unwrap_or(false);
    if lease_changed {
        broadcast_writer_lease_state_locked(inner, now_epoch_ms);
    }
    let permitted = authorized && controller_matches && permit.is_active();
    drop(_operation);
    if !permitted {
        return None;
    }
    Some(operation())
}

fn with_registered_web_operation<R>(
    inner: &Arc<RemoteHostInner>,
    connection_id: u64,
    client_id: &str,
    expected_tombstone: Option<&Arc<WebConnectionTombstone>>,
    operation: impl FnOnce() -> R,
) -> Option<R> {
    let _operation = inner
        .web_control_operation_lock
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let permit = web_client_is_still_paired(inner, client_id)
        .then(|| registered_web_tombstone(inner, connection_id, client_id, expected_tombstone))??;
    drop(_operation);
    permit.is_active().then(operation)
}

#[derive(Clone)]
struct WebInputFence {
    inner: Arc<RemoteHostInner>,
    connection_id: u64,
    client_id: String,
    tombstone: Option<Arc<WebConnectionTombstone>>,
    lease_generation: Option<u64>,
    runtime_instance_id: String,
}

impl WebInputFence {
    fn is_current(&self) -> bool {
        self.is_current_at(now_epoch_ms())
    }

    fn is_current_at(&self, checked_at_epoch_ms: u64) -> bool {
        if self.runtime_instance_id != self.inner.runtime_instance_id {
            return false;
        }
        let _operation = self
            .inner
            .web_control_operation_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if self.tombstone.as_ref().is_some_and(|expected| {
            !web_connection_is_authoritative_locked(
                &self.inner,
                self.connection_id,
                &self.client_id,
                Some(expected),
            )
        }) {
            return false;
        }
        let (authorized, lease_changed) = {
            let Ok(mut control) = self.inner.web_control.lock() else {
                return false;
            };
            let before = control.writer_leases().peek();
            let authorized = match self.lease_generation {
                Some(generation) => control
                    .writer_leases_mut()
                    .authorize(
                        self.connection_id,
                        &self.client_id,
                        generation,
                        checked_at_epoch_ms,
                    )
                    .is_ok(),
                None => control.legacy_authorizes(self.connection_id, &self.client_id),
            };
            let after = control.writer_leases().peek();
            clear_controller_after_lease_removal(&self.inner, before.as_ref(), after.as_ref());
            (authorized, before != after)
        };
        let controller_matches = self
            .inner
            .controller_client_id
            .read()
            .map(|controller| controller.as_deref() == Some(self.client_id.as_str()))
            .unwrap_or(false);
        if lease_changed {
            broadcast_writer_lease_state_locked(&self.inner, checked_at_epoch_ms);
        }
        authorized && controller_matches
    }
}

pub(crate) fn web_mutation_authority_is_current(
    inner: &Arc<RemoteHostInner>,
    authority: &RemoteWebMutationAuthority,
) -> bool {
    if authority.runtime_instance_id != inner.runtime_instance_id {
        return false;
    }
    let Some(tombstone) =
        registered_web_tombstone(inner, authority.connection_id, &authority.client_id, None)
    else {
        return false;
    };
    WebInputFence {
        inner: inner.clone(),
        connection_id: authority.connection_id,
        client_id: authority.client_id.clone(),
        tombstone: Some(tombstone),
        lease_generation: Some(authority.lease_generation),
        runtime_instance_id: authority.runtime_instance_id.clone(),
    }
    .is_current()
}

fn reserve_web_input_fence(
    inner: &Arc<RemoteHostInner>,
    connection_id: u64,
    client_id: &str,
    expected_tombstone: Option<&Arc<WebConnectionTombstone>>,
    expected_lease_generation: Option<u64>,
    now_epoch_ms: u64,
) -> Option<WebInputFence> {
    let _operation = inner
        .web_control_operation_lock
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let tombstone = registered_web_tombstone(inner, connection_id, client_id, expected_tombstone);
    if expected_tombstone.is_some() && tombstone.is_none() {
        return None;
    }
    let (authorized, lease_generation, lease_changed) = {
        let Ok(mut control) = inner.web_control.lock() else {
            return None;
        };
        let before = control.writer_leases().peek();
        let (authorized, lease_generation) = match expected_lease_generation {
            Some(generation) => match control.writer_leases_mut().authorize(
                connection_id,
                client_id,
                generation,
                now_epoch_ms,
            ) {
                Ok(lease) => (true, Some(lease.generation)),
                Err(_) => (false, Some(generation)),
            },
            None => (control.legacy_authorizes(connection_id, client_id), None),
        };
        let after = control.writer_leases().peek();
        clear_controller_after_lease_removal(inner, before.as_ref(), after.as_ref());
        (authorized, lease_generation, before != after)
    };
    let controller_matches = inner
        .controller_client_id
        .read()
        .map(|controller| controller.as_deref() == Some(client_id))
        .unwrap_or(false);
    if lease_changed {
        broadcast_writer_lease_state_locked(inner, now_epoch_ms);
    }
    (authorized && controller_matches).then(|| WebInputFence {
        inner: inner.clone(),
        connection_id,
        client_id: client_id.to_string(),
        tombstone,
        lease_generation,
        runtime_instance_id: inner.runtime_instance_id.clone(),
    })
}

fn stable_key_for_session_id(
    inner: &Arc<RemoteHostInner>,
    session_id: &str,
) -> Option<StableSessionKey> {
    if let Some(key) = inner
        .semantic_journals
        .lock()
        .ok()
        .and_then(|journals| journals.stable_key_for_session(session_id))
    {
        return Some(key);
    }
    let _snapshot = inner
        .snapshot_state_lock
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let tabs = inner
        .shared_state
        .read()
        .map(|state| state.open_tabs.clone())
        .unwrap_or_default();
    inner
        .runtime_state
        .read()
        .ok()
        .and_then(|runtime| runtime.sessions.get(session_id).cloned())
        .and_then(|runtime| StableSessionKey::resolve(&runtime, &tabs))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WebInputEnqueueError {
    Authority,
    QueueUnavailable,
}

fn enqueue_terminal_input(
    inner: &Arc<RemoteHostInner>,
    connection_id: u64,
    client_id: &str,
    expected_tombstone: Option<&Arc<WebConnectionTombstone>>,
    expected_lease_generation: Option<u64>,
    enqueued_at_epoch_ms: u64,
    stable_session_key: StableSessionKey,
    retained_bytes: usize,
    response: ServerResponseLane,
    build_input: impl FnOnce(String) -> RemoteTerminalInput + Send + 'static,
) -> Result<(), WebInputEnqueueError> {
    let Some(fence) = reserve_web_input_fence(
        inner,
        connection_id,
        client_id,
        expected_tombstone,
        expected_lease_generation,
        enqueued_at_epoch_ms,
    ) else {
        return Err(WebInputEnqueueError::Authority);
    };
    let job_key = stable_session_key.clone();
    inner
        .web_input_executor
        .dispatch(job_key, retained_bytes, move || {
            if !fence.is_current() {
                let _ = response.send(ServerMessage::Error {
                    message: "The writer lease changed before terminal input executed.".to_string(),
                });
                return;
            }
            let (session_id, _) = match resolve_unique_session(&fence.inner, &stable_session_key) {
                Ok(session) => session,
                Err(_) => {
                    let _ = response.send(ServerMessage::Error {
                        message: "The requested session no longer exists.".to_string(),
                    });
                    return;
                }
            };
            let handler = fence
                .inner
                .terminal_input_handler
                .read()
                .ok()
                .and_then(|slot| slot.as_ref().cloned());
            let result = handler.map_or_else(
                || Err("The target PTY is not ready for input.".to_string()),
                |handler| {
                    invoke_terminal_input(&handler, build_input(session_id), enqueued_at_epoch_ms)
                },
            );
            if let Err(message) = result {
                let _ = response.send(ServerMessage::Error { message });
            }
        })
        .map_err(|_| WebInputEnqueueError::QueueUnavailable)
}

fn enqueue_terminal_resize(
    inner: &Arc<RemoteHostInner>,
    connection_id: u64,
    client_id: &str,
    expected_tombstone: Option<&Arc<WebConnectionTombstone>>,
    expected_lease_generation: Option<u64>,
    enqueued_at_epoch_ms: u64,
    stable_session_key: StableSessionKey,
    response: ServerResponseLane,
    dimensions: SessionDimensions,
) -> Result<(), WebInputEnqueueError> {
    let Some(fence) = reserve_web_input_fence(
        inner,
        connection_id,
        client_id,
        expected_tombstone,
        expected_lease_generation,
        enqueued_at_epoch_ms,
    ) else {
        return Err(WebInputEnqueueError::Authority);
    };
    let job_key = stable_session_key.clone();
    inner
        .web_input_executor
        .dispatch(
            job_key,
            std::mem::size_of::<SessionDimensions>(),
            move || {
                if !fence.is_current() {
                    let _ = response.send(ServerMessage::Error {
                        message: "The writer lease changed before terminal resize executed."
                            .to_string(),
                    });
                    return;
                }
                let (session_id, _) =
                    match resolve_unique_session(&fence.inner, &stable_session_key) {
                        Ok(session) => session,
                        Err(_) => {
                            let _ = response.send(ServerMessage::Error {
                                message: "The requested session no longer exists.".to_string(),
                            });
                            return;
                        }
                    };
                let handler = fence
                    .inner
                    .terminal_resize_handler
                    .read()
                    .ok()
                    .and_then(|slot| slot.as_ref().cloned());
                if let Some(handler) = handler {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        handler(session_id, dimensions);
                    }));
                    if result.is_err() {
                        let _ = response.send(ServerMessage::Error {
                            message: "The terminal resize handler panicked.".to_string(),
                        });
                    }
                }
            },
        )
        .map_err(|_| WebInputEnqueueError::QueueUnavailable)
}

#[cfg(test)]
fn claim_legacy_control(
    inner: &Arc<RemoteHostInner>,
    connection_id: u64,
    client_id: &str,
    force: bool,
) {
    claim_legacy_control_for_connection(inner, connection_id, client_id, force, None);
}

fn claim_legacy_control_for_connection(
    inner: &Arc<RemoteHostInner>,
    connection_id: u64,
    client_id: &str,
    force: bool,
    expected_tombstone: Option<&Arc<WebConnectionTombstone>>,
) {
    let _operation = inner
        .web_control_operation_lock
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if expected_tombstone.is_some()
        && !web_connection_is_authoritative_locked(
            inner,
            connection_id,
            client_id,
            expected_tombstone,
        )
    {
        return;
    }
    let controller_id = inner
        .controller_client_id
        .read()
        .map(|controller| controller.clone())
        .unwrap_or_default();
    let claimed = inner
        .web_control
        .lock()
        .map(|mut control| {
            control.claim_legacy(connection_id, client_id, force, controller_id.as_deref())
        })
        .unwrap_or(false);
    if claimed {
        if let Ok(mut controller) = inner.controller_client_id.write() {
            *controller = Some(client_id.to_string());
        }
    }
}

fn release_legacy_control_for_connection(
    inner: &Arc<RemoteHostInner>,
    connection_id: u64,
    client_id: &str,
    expected_tombstone: Option<&Arc<WebConnectionTombstone>>,
) {
    let _operation = inner
        .web_control_operation_lock
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if expected_tombstone.is_some()
        && !web_connection_is_authoritative_locked(
            inner,
            connection_id,
            client_id,
            expected_tombstone,
        )
    {
        return;
    }
    let released = inner
        .web_control
        .lock()
        .map(|mut control| control.clear_legacy_claim(connection_id, client_id))
        .unwrap_or(false);
    if released {
        if let Ok(mut controller) = inner.controller_client_id.write() {
            if controller.as_deref() == Some(client_id) {
                *controller = None;
            }
        }
    }
}

#[cfg(test)]
fn build_resume_state(
    inner: &Arc<RemoteHostInner>,
    connection_id: u64,
    client_id: &str,
    request: ResumeRequest,
    now_epoch_ms: u64,
) -> ResumeState {
    let _operation = inner
        .web_control_operation_lock
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (state, _, _) =
        build_resume_state_locked(inner, connection_id, client_id, request, now_epoch_ms, 1);
    broadcast_writer_lease_state_locked(inner, now_epoch_ms);
    state
}

fn send_resume_state_with_lane(
    inner: &Arc<RemoteHostInner>,
    connection_id: u64,
    client_id: &str,
    request: ResumeRequest,
    now_epoch_ms: u64,
    web_tx: &WebResponseLane,
    expected_tombstone: Option<&Arc<WebConnectionTombstone>>,
) {
    let valid = valid_client_instance_id(&request.client_instance_id)
        && request.route.len() <= MAX_RESUME_ROUTE_BYTES
        && request
            .desired_session_key
            .as_ref()
            .is_none_or(valid_stable_session_key)
        && request
            .raw_session_id
            .as_deref()
            .is_none_or(valid_session_id);
    if !valid {
        let _ = web_tx.send(WsOutbound::Error {
            message: "Resume request identifiers are too long or empty.".to_string(),
        });
        return;
    }
    let _operation = inner
        .web_control_operation_lock
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let _delivery = inner
        .semantic_delivery_lock
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if !web_connection_is_authoritative_locked(inner, connection_id, client_id, expected_tombstone)
    {
        return;
    }
    let Some((sender, tombstone)) = inner.clients.lock().ok().and_then(|clients| {
        let client = clients.get(&connection_id)?;
        if client.client_id != client_id
            || client.web_tombstone.as_ref().is_none_or(|registered| {
                expected_tombstone.is_some_and(|expected| !Arc::ptr_eq(registered, expected))
            })
        {
            return None;
        }
        Some((client.web_sender.clone()?, client.web_tombstone.clone()?))
    }) else {
        return;
    };
    if !sender.is_active() {
        return;
    }
    let replay_epoch = sender.next_replay_epoch();
    let replay_id = sender.next_replay_id();
    let (state, replay, _) = build_resume_state_locked(
        inner,
        connection_id,
        client_id,
        request.clone(),
        now_epoch_ms,
        replay_id,
    );
    let desired_session_key = state.desired_session_key.clone();
    let through_sequence = state
        .semantic_replay
        .as_ref()
        .map(|descriptor| descriptor.through_sequence);
    let desired_session_id = desired_session_key.as_ref().and_then(|key| {
        resolve_unique_session(inner, key)
            .ok()
            .map(|(session_id, _)| session_id)
    });
    let raw_session_id = request.raw_session_id.clone();
    let delivered = if web_tx.is_test_lane() {
        web_tx.send(WsOutbound::ResumeState { state }).is_ok()
    } else {
        let prefix = serialize_text(&WsOutbound::ResumeState { state });
        match (prefix, replay, desired_session_key.as_ref()) {
            (Some(prefix), Some(replay), Some(key)) => sender
                .try_send_replay(
                    Some(prefix),
                    replay_id,
                    key.clone(),
                    request.semantic_after_sequence.unwrap_or(0),
                    replay,
                    replay_epoch,
                )
                .is_ok(),
            (Some(prefix), None, _) => sender.try_send_frame(prefix).is_ok(),
            _ => false,
        }
    };
    if !delivered {
        drop(_delivery);
        revoke_web_connection_locked(inner, connection_id, client_id, &tombstone, None);
        return;
    }
    let committed = inner.clients.lock().ok().is_some_and(|mut clients| {
        let Some(client) = clients.get_mut(&connection_id).filter(|client| {
            client.client_id == client_id
                && client
                    .web_tombstone
                    .as_ref()
                    .is_some_and(|registered| Arc::ptr_eq(registered, &tombstone))
        }) else {
            return false;
        };
        client.semantic_cursors.clear();
        if let (Some(key), Some(through)) = (desired_session_key.clone(), through_sequence) {
            client.semantic_cursors.insert(key, through);
        }
        client.subscribed_session_ids.clear();
        client.bootstrapped_session_ids.clear();
        client.bootstrap_pending_session_ids.clear();
        client.focused_session_id = request
            .visible
            .then(|| desired_session_id.clone())
            .flatten();
        if let Some(session_id) = raw_session_id.as_ref() {
            client.subscribed_session_ids.insert(session_id.clone());
            client
                .bootstrap_pending_session_ids
                .insert(session_id.clone());
        }
        true
    });
    if !committed {
        drop(_delivery);
        revoke_web_connection_locked(inner, connection_id, client_id, &tombstone, None);
        return;
    }
    broadcast_writer_lease_state_locked_excluding(inner, now_epoch_ms, Some(connection_id));
    let visible_focus = request
        .visible
        .then(|| (desired_session_id.clone(), desired_session_key.clone()));
    drop(_delivery);
    drop(_operation);
    if let Some((Some(session_id), stable_session_key)) = visible_focus {
        let handler = inner
            .focused_session_handler
            .read()
            .ok()
            .and_then(|handler| handler.clone());
        if let Some(handler) = handler {
            handler(session_id);
        }
        if let Some(stable_session_key) = stable_session_key {
            acknowledge_browser_attention(inner, &stable_session_key);
        }
    }
}

#[cfg(test)]
fn send_resume_state(
    inner: &Arc<RemoteHostInner>,
    connection_id: u64,
    client_id: &str,
    request: ResumeRequest,
    now_epoch_ms: u64,
    web_tx: &tokio_mpsc::UnboundedSender<WsOutbound>,
) {
    let (native, _) = tokio_mpsc::unbounded_channel();
    send_resume_state_with_lane(
        inner,
        connection_id,
        client_id,
        request,
        now_epoch_ms,
        &WebResponseLane(InboundResponder::Test {
            native,
            web: web_tx.clone(),
        }),
        None,
    );
}

fn build_resume_state_locked(
    inner: &Arc<RemoteHostInner>,
    connection_id: u64,
    client_id: &str,
    request: ResumeRequest,
    now_epoch_ms: u64,
    replay_id: u64,
) -> (ResumeState, Option<Arc<SemanticReplay>>, u64) {
    let hard_reset = request
        .seen_runtime_instance_id
        .as_deref()
        .is_some_and(|seen| seen != inner.runtime_instance_id);
    let writer_lease = if request.wants_writer_lease && request.visible {
        acquire_writer_lease_locked(
            inner,
            connection_id,
            client_id,
            &request.client_instance_id,
            now_epoch_ms,
        )
    } else {
        set_writer_visibility_locked(
            inner,
            connection_id,
            client_id,
            &request.client_instance_id,
            request.visible,
            now_epoch_ms,
        )
    };
    let requested_key = (!hard_reset)
        .then_some(request.desired_session_key.clone())
        .flatten();
    let semantic_after_sequence = request.semantic_after_sequence.unwrap_or(0);
    let (projection, captured_replay, semantic_generation) = capture_resume_projection(
        inner,
        client_id,
        &writer_lease,
        requested_key.as_ref(),
        semantic_after_sequence,
    );
    let (route, desired_session_key) = if hard_reset {
        ("/sessions".to_string(), None)
    } else {
        validate_resume_route(&request.route, requested_key.as_ref(), &projection)
    };
    let semantic_replay_snapshot = desired_session_key.as_ref().map(|_| {
        Arc::new(captured_replay.unwrap_or(SemanticReplay {
            oldest_sequence: 0,
            through_sequence: 0,
            cursor_rolled_over: false,
            events: Vec::new(),
        }))
    });
    let semantic_replay = desired_session_key
        .as_ref()
        .zip(semantic_replay_snapshot.as_ref())
        .map(|(key, replay)| SemanticReplayDescriptor {
            replay_id,
            stable_session_key: key.clone(),
            from_sequence: semantic_after_sequence,
            through_sequence: replay.through_sequence,
            rollover: replay.cursor_rolled_over,
        });

    let revision = projection.revision;
    let runtime_matches =
        request.seen_runtime_instance_id.as_deref() == Some(inner.runtime_instance_id.as_str());
    let workspace = (hard_reset || !runtime_matches || request.seen_revision != Some(revision))
        .then_some(projection);
    (
        ResumeState {
            runtime_instance_id: inner.runtime_instance_id.clone(),
            revision,
            hard_reset,
            route,
            desired_session_key,
            workspace,
            semantic_replay,
            writer_lease,
        },
        semantic_replay_snapshot,
        semantic_generation,
    )
}

fn capture_resume_projection(
    inner: &Arc<RemoteHostInner>,
    client_id: &str,
    writer_lease: &WebWriterLeaseState,
    desired_session_key: Option<&StableSessionKey>,
    semantic_after_sequence: u64,
) -> (WebWorkspaceSnapshot, Option<SemanticReplay>, u64) {
    let generation = &inner.semantic_publication_generation;
    let capture = || {
        capture_resume_projection_raw(
            inner,
            client_id,
            desired_session_key,
            semantic_after_sequence,
        )
    };
    let (snapshot, revision, semantic_metadata, replay_capture) =
        capture_with_bounded_generation(generation, capture, || {
            let _publication = inner
                .semantic_publication_lock
                .lock()
                .unwrap_or_else(|poisoned| {
                    inner.semantic_publication_lock.clear_poison();
                    poisoned.into_inner()
                });
            capture_resume_projection_raw(
                inner,
                client_id,
                desired_session_key,
                semantic_after_sequence,
            )
        });

    // Replay materialization/capping and web projection can be comparatively
    // expensive. Do them once, after a coherent capture has been selected.
    let replay =
        replay_capture.map(|capture| cap_semantic_replay_for_mobile(capture.into_replay()));
    (
        project_web_snapshot(inner, &snapshot, revision, &semantic_metadata, writer_lease),
        replay,
        generation.load(Ordering::Acquire),
    )
}

fn capture_with_bounded_generation<T>(
    generation: &AtomicU64,
    mut capture: impl FnMut() -> T,
    fallback: impl FnOnce() -> T,
) -> T {
    for _ in 0..MAX_RESUME_CAPTURE_ATTEMPTS {
        let before = generation.load(Ordering::Acquire);
        if before % 2 != 0 {
            std::thread::yield_now();
            continue;
        }
        let value = capture();
        let after = generation.load(Ordering::Acquire);
        if before == after {
            return value;
        }
        std::thread::yield_now();
    }
    fallback()
}

fn capture_resume_projection_raw(
    inner: &Arc<RemoteHostInner>,
    client_id: &str,
    desired_session_key: Option<&StableSessionKey>,
    semantic_after_sequence: u64,
) -> (
    RemoteWorkspaceSnapshot,
    u64,
    HashMap<StableSessionKey, SemanticSessionMetadata>,
    Option<super::super::presentation::SemanticReplayCapture>,
) {
    let _snapshot_guard = inner
        .snapshot_state_lock
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let semantic_journals = match inner.semantic_journals.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            let guard = poisoned.into_inner();
            inner.semantic_journals.clear_poison();
            guard
        }
    };
    let replay = desired_session_key
        .and_then(|key| semantic_journals.capture_replay_after(key, semantic_after_sequence));
    (
        light_snapshot(inner, client_id),
        inner.snapshot_revision.load(Ordering::Relaxed),
        semantic_journals.metadata_snapshot(),
        replay,
    )
}

fn validate_resume_route(
    route: &str,
    requested_key: Option<&StableSessionKey>,
    workspace: &WebWorkspaceSnapshot,
) -> (String, Option<StableSessionKey>) {
    let path = route
        .split(['?', '#'])
        .next()
        .unwrap_or("/sessions")
        .trim_end_matches('/');
    let path = if path.is_empty() { "/" } else { path };
    match path {
        "/sessions" | "/projects" | "/settings" => (path.to_string(), None),
        _ => {
            if let Some(project_id) = path.strip_prefix("/projects/") {
                if !project_id.is_empty()
                    && !project_id.contains('/')
                    && workspace
                        .projects
                        .iter()
                        .any(|project| project.id == project_id)
                {
                    return (path.to_string(), None);
                }
            }
            if let Some(rest) = path.strip_prefix("/session/") {
                let mut parts = rest.split('/');
                let kind = parts.next();
                let id = parts.next();
                if parts.next().is_none() {
                    let route_key = match (kind, id) {
                        (Some("tab"), Some(id)) if !id.is_empty() => {
                            Some(StableSessionKey::from_tab(id))
                        }
                        (Some("server"), Some(id)) if !id.is_empty() => {
                            Some(StableSessionKey::from_server(id))
                        }
                        _ => None,
                    };
                    if let Some(route_key) = route_key {
                        let requested_matches = requested_key == Some(&route_key);
                        let exists = workspace
                            .sessions
                            .iter()
                            .any(|session| session.stable_session_key.as_ref() == Some(&route_key));
                        if requested_matches && exists {
                            return (path.to_string(), Some(route_key));
                        }
                    }
                }
            }
            ("/sessions".to_string(), None)
        }
    }
}

fn writer_lease_state(
    inner: &Arc<RemoteHostInner>,
    connection_id: u64,
    now_epoch_ms: u64,
) -> WebWriterLeaseState {
    let _operation = inner
        .web_control_operation_lock
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    writer_lease_state_locked(inner, connection_id, now_epoch_ms)
}

fn writer_lease_state_locked(
    inner: &Arc<RemoteHostInner>,
    connection_id: u64,
    now_epoch_ms: u64,
) -> WebWriterLeaseState {
    let (before, current, generation) = {
        let Ok(mut control) = inner.web_control.lock() else {
            return WebWriterLeaseState::default();
        };
        let before = control.writer_leases().peek();
        let current = control.writer_leases_mut().current(now_epoch_ms);
        let generation = control.writer_leases().generation();
        (before, current, generation)
    };
    clear_controller_after_lease_removal(inner, before.as_ref(), current.as_ref());
    writer_lease_state_from(current.as_ref(), generation, connection_id)
}

fn clear_controller_after_lease_removal(
    inner: &Arc<RemoteHostInner>,
    previous: Option<&WriterLease>,
    current: Option<&WriterLease>,
) {
    if let (Some(previous), None) = (previous, current) {
        if let Ok(mut controller) = inner.controller_client_id.write() {
            if controller.as_deref() == Some(previous.owner_client_id.as_str()) {
                *controller = None;
            }
        }
    }
}

fn writer_lease_state_from(
    current: Option<&WriterLease>,
    generation: u64,
    connection_id: u64,
) -> WebWriterLeaseState {
    WebWriterLeaseState {
        owner_client_instance_id: current.map(|lease| lease.owner_client_instance_id.clone()),
        generation,
        expires_at_epoch_ms: current.map(|lease| lease.expires_at_epoch_ms),
        you_are_owner: current.is_some_and(|lease| lease.owner_connection_id == connection_id),
    }
}

#[cfg(test)]
fn acquire_writer_lease(
    inner: &Arc<RemoteHostInner>,
    connection_id: u64,
    client_id: &str,
    client_instance_id: &str,
    now_epoch_ms: u64,
) -> WebWriterLeaseState {
    acquire_writer_lease_for_connection(
        inner,
        connection_id,
        client_id,
        client_instance_id,
        now_epoch_ms,
        None,
    )
}

fn acquire_writer_lease_for_connection(
    inner: &Arc<RemoteHostInner>,
    connection_id: u64,
    client_id: &str,
    client_instance_id: &str,
    now_epoch_ms: u64,
    expected_tombstone: Option<&Arc<WebConnectionTombstone>>,
) -> WebWriterLeaseState {
    let _operation = inner
        .web_control_operation_lock
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if expected_tombstone.is_some()
        && !web_connection_is_authoritative_locked(
            inner,
            connection_id,
            client_id,
            expected_tombstone,
        )
    {
        return writer_lease_state_locked(inner, connection_id, now_epoch_ms);
    }
    let state = acquire_writer_lease_locked(
        inner,
        connection_id,
        client_id,
        client_instance_id,
        now_epoch_ms,
    );
    broadcast_writer_lease_state_locked(inner, now_epoch_ms);
    state
}

fn acquire_writer_lease_locked(
    inner: &Arc<RemoteHostInner>,
    connection_id: u64,
    client_id: &str,
    client_instance_id: &str,
    now_epoch_ms: u64,
) -> WebWriterLeaseState {
    let Ok(mut control) = inner.web_control.lock() else {
        return WebWriterLeaseState::default();
    };
    let before = control.writer_leases().peek();
    let current = control.writer_leases_mut().current(now_epoch_ms);
    let generation = control.writer_leases().generation();
    let Ok(mut controller) = inner.controller_client_id.write() else {
        return writer_lease_state_from(current.as_ref(), generation, connection_id);
    };
    if let (Some(expired), None) = (before.as_ref(), current.as_ref()) {
        if controller.as_deref() == Some(expired.owner_client_id.as_str()) {
            *controller = None;
        }
    }
    let controller_is_current_web_owner = controller.as_deref().is_some_and(|controller_id| {
        current
            .as_ref()
            .is_some_and(|lease| lease.owner_client_id == controller_id)
    });
    let upgrading_exact_legacy = control.legacy_authorizes(connection_id, client_id)
        && controller.as_deref() == Some(client_id);
    if controller.is_some() && !controller_is_current_web_owner && !upgrading_exact_legacy {
        return writer_lease_state_from(current.as_ref(), generation, connection_id);
    }
    if upgrading_exact_legacy {
        control.clear_legacy_claim(connection_id, client_id);
    }
    if let Ok(lease) = control.writer_leases_mut().acquire(
        connection_id,
        client_id,
        client_instance_id,
        now_epoch_ms,
    ) {
        *controller = Some(client_id.to_string());
        let generation = control.writer_leases().generation();
        return writer_lease_state_from(Some(&lease), generation, connection_id);
    }
    let current = control.writer_leases().peek();
    writer_lease_state_from(
        current.as_ref(),
        control.writer_leases().generation(),
        connection_id,
    )
}

fn renew_writer_lease(
    inner: &Arc<RemoteHostInner>,
    connection_id: u64,
    client_id: &str,
    client_instance_id: &str,
    expected_generation: u64,
    visible: bool,
    now_epoch_ms: u64,
    expected_tombstone: Option<&Arc<WebConnectionTombstone>>,
) -> WebWriterLeaseState {
    let _operation = inner
        .web_control_operation_lock
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if !web_connection_is_authoritative_locked(inner, connection_id, client_id, expected_tombstone)
    {
        return writer_lease_state_locked(inner, connection_id, now_epoch_ms);
    }
    if !visible {
        clear_focused_session_for_connection(inner, connection_id, client_id);
    }
    let state = {
        let Ok(mut control) = inner.web_control.lock() else {
            return WebWriterLeaseState::default();
        };
        let before = control.writer_leases().peek();
        let _ = control.writer_leases_mut().renew(
            connection_id,
            client_id,
            client_instance_id,
            expected_generation,
            visible,
            now_epoch_ms,
        );
        let current = control.writer_leases().peek();
        let generation = control.writer_leases().generation();
        drop(control);
        clear_controller_after_lease_removal(inner, before.as_ref(), current.as_ref());
        writer_lease_state_from(current.as_ref(), generation, connection_id)
    };
    broadcast_writer_lease_state_locked(inner, now_epoch_ms);
    state
}

#[cfg(test)]
fn set_writer_visibility(
    inner: &Arc<RemoteHostInner>,
    connection_id: u64,
    client_id: &str,
    client_instance_id: &str,
    visible: bool,
    now_epoch_ms: u64,
) -> WebWriterLeaseState {
    set_writer_visibility_for_connection(
        inner,
        connection_id,
        client_id,
        client_instance_id,
        visible,
        now_epoch_ms,
        None,
    )
}

fn set_writer_visibility_for_connection(
    inner: &Arc<RemoteHostInner>,
    connection_id: u64,
    client_id: &str,
    client_instance_id: &str,
    visible: bool,
    now_epoch_ms: u64,
    expected_tombstone: Option<&Arc<WebConnectionTombstone>>,
) -> WebWriterLeaseState {
    let _operation = inner
        .web_control_operation_lock
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if expected_tombstone.is_some()
        && !web_connection_is_authoritative_locked(
            inner,
            connection_id,
            client_id,
            expected_tombstone,
        )
    {
        return writer_lease_state_locked(inner, connection_id, now_epoch_ms);
    }
    if !visible {
        clear_focused_session_for_connection(inner, connection_id, client_id);
    }
    let state = set_writer_visibility_locked(
        inner,
        connection_id,
        client_id,
        client_instance_id,
        visible,
        now_epoch_ms,
    );
    broadcast_writer_lease_state_locked(inner, now_epoch_ms);
    state
}

fn clear_focused_session_for_connection(
    inner: &Arc<RemoteHostInner>,
    connection_id: u64,
    client_id: &str,
) {
    if let Ok(mut clients) = inner.clients.lock() {
        if let Some(client) = clients
            .get_mut(&connection_id)
            .filter(|client| client.client_id == client_id)
        {
            client.focused_session_id = None;
        }
    }
}

fn set_writer_visibility_locked(
    inner: &Arc<RemoteHostInner>,
    connection_id: u64,
    client_id: &str,
    client_instance_id: &str,
    visible: bool,
    now_epoch_ms: u64,
) -> WebWriterLeaseState {
    let Ok(mut control) = inner.web_control.lock() else {
        return WebWriterLeaseState::default();
    };
    let before = control.writer_leases().peek();
    let _ = control.writer_leases_mut().set_visibility(
        connection_id,
        client_id,
        client_instance_id,
        visible,
        now_epoch_ms,
    );
    let current = control.writer_leases().peek();
    let generation = control.writer_leases().generation();
    drop(control);
    clear_controller_after_lease_removal(inner, before.as_ref(), current.as_ref());
    writer_lease_state_from(current.as_ref(), generation, connection_id)
}

pub(crate) fn broadcast_writer_lease_state_locked(inner: &Arc<RemoteHostInner>, now_epoch_ms: u64) {
    broadcast_writer_lease_state_locked_excluding(inner, now_epoch_ms, None);
}

fn broadcast_writer_lease_state_locked_excluding(
    inner: &Arc<RemoteHostInner>,
    now_epoch_ms: u64,
    excluded_connection_id: Option<u64>,
) {
    for _ in 0..2 {
        let (current, generation) = {
            let Ok(mut control) = inner.web_control.lock() else {
                return;
            };
            let before = control.writer_leases().peek();
            let current = control.writer_leases_mut().current(now_epoch_ms);
            let generation = control.writer_leases().generation();
            drop(control);
            clear_controller_after_lease_removal(inner, before.as_ref(), current.as_ref());
            (current, generation)
        };
        let targets = inner
            .clients
            .lock()
            .map(|clients| {
                clients
                    .iter()
                    .filter_map(|(connection_id, client)| {
                        if Some(*connection_id) == excluded_connection_id {
                            return None;
                        }
                        client
                            .web_sender
                            .clone()
                            .zip(client.web_tombstone.clone())
                            .map(|(sender, tombstone)| {
                                (*connection_id, client.client_id.clone(), sender, tombstone)
                            })
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let mut dead = Vec::new();
        for (connection_id, client_id, sender, tombstone) in targets {
            let writer_lease = writer_lease_state_from(current.as_ref(), generation, connection_id);
            if sender
                .try_send(WsOutbound::WriterLeaseState { writer_lease })
                .is_err()
            {
                dead.push((connection_id, client_id, tombstone));
            }
        }
        if dead.is_empty() {
            return;
        }
        let mut revoked_any = false;
        for (connection_id, client_id, tombstone) in dead {
            revoked_any |=
                revoke_web_connection_locked(inner, connection_id, &client_id, &tombstone, None);
        }
        if !revoked_any {
            return;
        }
    }
}

enum ComposerCompletion {
    Web(WebResponseLane),
    #[cfg(test)]
    Test(std_mpsc::SyncSender<Result<ComposerAccepted, ComposerRejected>>),
}

impl ComposerCompletion {
    fn uses_deterministic_test_clock(&self) -> bool {
        match self {
            Self::Web(lane) => lane.is_test_lane(),
            #[cfg(test)]
            Self::Test(_) => true,
        }
    }

    fn send(self, result: Result<ComposerAccepted, ComposerRejected>) {
        match self {
            Self::Web(lane) => match result {
                Ok(accepted) => {
                    let _ = lane.send(WsOutbound::ComposerAccepted { accepted });
                }
                Err(rejected) => {
                    let _ = lane.send(WsOutbound::ComposerRejected { rejected });
                }
            },
            #[cfg(test)]
            Self::Test(sender) => {
                let _ = sender.send(result);
            }
        }
    }
}

#[cfg(test)]
fn process_composer_submit(
    inner: &Arc<RemoteHostInner>,
    connection_id: u64,
    client_id: &str,
    mutation_id: String,
    stable_session_key: StableSessionKey,
    text: String,
    attachments: Vec<ComposerAttachment>,
    expected_lease_generation: u64,
    now_epoch_ms: u64,
) -> Result<ComposerAccepted, ComposerRejected> {
    process_composer_submit_for_connection(
        inner,
        connection_id,
        client_id,
        mutation_id,
        stable_session_key,
        text,
        attachments,
        expected_lease_generation,
        now_epoch_ms,
        None,
    )
}

#[cfg(test)]
fn process_composer_submit_for_connection(
    inner: &Arc<RemoteHostInner>,
    connection_id: u64,
    client_id: &str,
    mutation_id: String,
    stable_session_key: StableSessionKey,
    text: String,
    attachments: Vec<ComposerAttachment>,
    expected_lease_generation: u64,
    now_epoch_ms: u64,
    expected_tombstone: Option<&Arc<WebConnectionTombstone>>,
) -> Result<ComposerAccepted, ComposerRejected> {
    let (completion_tx, completion_rx) = std_mpsc::sync_channel(1);
    match dispatch_composer_submit_for_connection(
        inner,
        connection_id,
        client_id,
        mutation_id,
        stable_session_key,
        text,
        attachments,
        expected_lease_generation,
        now_epoch_ms,
        expected_tombstone,
        ComposerCompletion::Test(completion_tx),
    ) {
        Ok(Some(accepted)) => Ok(accepted),
        Ok(None) => completion_rx.recv().unwrap_or_else(|_| {
            Err(composer_rejected(
                inner,
                connection_id,
                "composer-completion".to_string(),
                ComposerRejectCode::PtyRejected,
                "The terminal input worker stopped before reporting an outcome.",
                now_epoch_ms,
            ))
        }),
        Err(rejected) => Err(rejected),
    }
}

#[allow(clippy::too_many_arguments)]
fn dispatch_composer_submit_for_connection(
    inner: &Arc<RemoteHostInner>,
    connection_id: u64,
    client_id: &str,
    mutation_id: String,
    stable_session_key: StableSessionKey,
    text: String,
    attachments: Vec<ComposerAttachment>,
    expected_lease_generation: u64,
    now_epoch_ms: u64,
    expected_tombstone: Option<&Arc<WebConnectionTombstone>>,
    completion: ComposerCompletion,
) -> Result<Option<ComposerAccepted>, ComposerRejected> {
    if !valid_composer_mutation_id(&mutation_id)
        || !valid_stable_session_key(&stable_session_key)
        || text.len() > MAX_COMPOSER_TEXT_BYTES
        || (text.is_empty() && attachments.is_empty())
    {
        return Err(composer_rejected(
            inner,
            connection_id,
            bounded_composer_mutation_id(&mutation_id),
            ComposerRejectCode::InvalidRequest,
            "Composer submissions require a short printable ASCII mutation ID and content.",
            now_epoch_ms,
        ));
    }
    let decoded_attachments = match decode_composer_attachments(&attachments) {
        Ok(attachments) => attachments,
        Err(message) => {
            return Err(composer_rejected(
                inner,
                connection_id,
                mutation_id,
                ComposerRejectCode::InvalidRequest,
                message,
                now_epoch_ms,
            ));
        }
    };
    let retained_bytes = text.len()
        + mutation_id.len()
        + stable_session_key.as_str().len()
        + decoded_attachments
            .iter()
            .map(|attachment| {
                attachment.bytes.len()
                    + attachment.mime_type.len()
                    + attachment.file_name.as_ref().map_or(0, String::len)
            })
            .sum::<usize>();
    if let Err(code) = resolve_unique_session(inner, &stable_session_key) {
        let message = if code == ComposerRejectCode::AmbiguousSession {
            "The stable session key resolves to more than one PTY."
        } else {
            "The requested session no longer exists."
        };
        return Err(composer_rejected(
            inner,
            connection_id,
            mutation_id,
            code,
            message,
            now_epoch_ms,
        ));
    }
    let fingerprint = stable_hash(&(stable_session_key.as_str(), text.as_str(), &attachments));
    enum ComposerStart {
        Started(u64),
        Existing(WebComposerMutationRecord),
        Rejected(ComposerRejectCode, &'static str),
    }
    let start = {
        let _operation = inner
            .web_control_operation_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if expected_tombstone.is_some()
            && !web_connection_is_authoritative_locked(
                inner,
                connection_id,
                client_id,
                expected_tombstone,
            )
        {
            return Err(composer_rejected_locked(
                inner,
                connection_id,
                mutation_id,
                ComposerRejectCode::LeaseBusy,
                "This browser connection is no longer active.",
                now_epoch_ms,
            ));
        }
        let authorization = {
            let mut control = match inner.web_control.lock() {
                Ok(control) => control,
                Err(poisoned) => {
                    let control = poisoned.into_inner();
                    inner.web_control.clear_poison();
                    control
                }
            };
            let previous = control.writer_leases().peek();
            let authorization = control.writer_leases_mut().authorize(
                connection_id,
                client_id,
                expected_lease_generation,
                now_epoch_ms,
            );
            let current = control.writer_leases().peek();
            drop(control);
            clear_controller_after_lease_removal(inner, previous.as_ref(), current.as_ref());
            authorization
        };
        let start = match authorization {
            Err(LeaseError::StaleGeneration { .. } | LeaseError::Expired) => {
                ComposerStart::Rejected(
                    ComposerRejectCode::StaleGeneration,
                    "The writer lease changed before the prompt was accepted.",
                )
            }
            Err(LeaseError::ActiveOwner | LeaseError::NotOwner) => ComposerStart::Rejected(
                ComposerRejectCode::LeaseBusy,
                "The writer lease changed before the prompt was accepted.",
            ),
            Ok(lease) => {
                let controller_matches = inner
                    .controller_client_id
                    .read()
                    .map(|controller| controller.as_deref() == Some(client_id))
                    .unwrap_or(false);
                if !controller_matches {
                    ComposerStart::Rejected(
                        ComposerRejectCode::NativeControllerActive,
                        "A native desktop controller is active.",
                    )
                } else {
                    // Keep the registry guard through deduplication, the
                    // capacity check, and insertion. The PTY callback runs on
                    // the keyed executor without pinning the writer lease.
                    let mut mutations = composer_mutations(inner);
                    if let Some(existing) = mutations.get(&mutation_id).cloned() {
                        ComposerStart::Existing(existing)
                    } else if mutations.len() >= MAX_COMPOSER_MUTATION_RECORDS {
                        ComposerStart::Rejected(
                            ComposerRejectCode::CapacityExceeded,
                            "Composer mutation history is full for this host runtime. Restart the host before submitting a new prompt.",
                        )
                    } else {
                        let previous = mutations.insert(
                            mutation_id.clone(),
                            WebComposerMutationRecord {
                                fingerprint,
                                status: WebComposerMutationStatus::InFlight,
                            },
                        );
                        debug_assert!(previous.is_none());
                        ComposerStart::Started(lease.generation)
                    }
                }
            }
        };
        broadcast_writer_lease_state_locked(inner, now_epoch_ms);
        start
    };
    let lease_generation = match start {
        ComposerStart::Started(generation) => generation,
        ComposerStart::Rejected(code, message) => {
            return Err(composer_rejected(
                inner,
                connection_id,
                mutation_id,
                code,
                message,
                now_epoch_ms,
            ));
        }
        ComposerStart::Existing(existing) => {
            if existing.fingerprint != fingerprint {
                return Err(composer_rejected(
                    inner,
                    connection_id,
                    mutation_id,
                    ComposerRejectCode::MutationConflict,
                    "This mutation ID was already used for different content.",
                    now_epoch_ms,
                ));
            }
            return match existing.status {
                WebComposerMutationStatus::InFlight => Err(composer_rejected(
                    inner,
                    connection_id,
                    mutation_id,
                    ComposerRejectCode::MutationInFlight,
                    "This mutation is still being accepted by the PTY.",
                    now_epoch_ms,
                )),
                WebComposerMutationStatus::PtyRejected { message } => Err(composer_rejected(
                    inner,
                    connection_id,
                    mutation_id,
                    ComposerRejectCode::PtyRejected,
                    message,
                    now_epoch_ms,
                )),
                WebComposerMutationStatus::Accepted {
                    stable_session_key,
                    accepted_sequence,
                    lease_generation,
                } => Ok(Some(ComposerAccepted {
                    mutation_id,
                    stable_session_key,
                    accepted_sequence,
                    lease_generation,
                })),
            };
        }
    };

    let Some(fence) = reserve_web_input_fence(
        inner,
        connection_id,
        client_id,
        expected_tombstone,
        Some(lease_generation),
        now_epoch_ms,
    ) else {
        clear_in_flight_composer(inner, &mutation_id, fingerprint);
        return Err(composer_rejected(
            inner,
            connection_id,
            mutation_id,
            ComposerRejectCode::StaleGeneration,
            "The writer lease changed before the prompt was queued.",
            now_epoch_ms,
        ));
    };
    let job_key = stable_session_key.clone();
    let dispatch_failure_mutation_id = mutation_id.clone();
    let fence_check_epoch_ms = completion
        .uses_deterministic_test_clock()
        .then_some(now_epoch_ms);
    let dispatch = inner
        .web_input_executor
        .dispatch(job_key, retained_bytes, move || {
            let execution_epoch_ms =
                fence_check_epoch_ms.unwrap_or_else(super::super::now_epoch_ms);
            if !fence.is_current_at(execution_epoch_ms) {
                clear_in_flight_composer(&fence.inner, &mutation_id, fingerprint);
                completion.send(Err(composer_rejected(
                    &fence.inner,
                    connection_id,
                    mutation_id,
                    ComposerRejectCode::StaleGeneration,
                    "The writer lease changed before the prompt executed.",
                    execution_epoch_ms,
                )));
                return;
            }
            let (session_id, session_kind) =
                match resolve_unique_session(&fence.inner, &stable_session_key) {
                    Ok(session) => session,
                    Err(code) => {
                        clear_in_flight_composer(&fence.inner, &mutation_id, fingerprint);
                        completion.send(Err(composer_rejected(
                            &fence.inner,
                            connection_id,
                            mutation_id,
                            code,
                            "The requested session no longer exists.",
                            execution_epoch_ms,
                        )));
                        return;
                    }
                };
            let host_service = RemoteHostService::borrowed(fence.inner.clone());
            let reconciliation = match session_kind {
                SessionKind::Claude => host_service.reserve_claude_composer_prompt(
                    &mutation_id,
                    &session_id,
                    &stable_session_key,
                    &text,
                ),
                SessionKind::Codex => {
                    let provider_visible_text =
                        canonical_codex_composer_prompt(&text, decoded_attachments.len());
                    host_service.reserve_codex_composer_prompt(
                        &mutation_id,
                        &session_id,
                        &stable_session_key,
                        &provider_visible_text,
                    )
                }
                SessionKind::Shell | SessionKind::Server | SessionKind::Ssh => {
                    ComposerReconciliationReservation::NotNeeded
                }
            };
            if reconciliation == ComposerReconciliationReservation::CapacityExceeded {
                clear_in_flight_composer(&fence.inner, &mutation_id, fingerprint);
                completion.send(Err(composer_rejected(
                    &fence.inner,
                    connection_id,
                    mutation_id,
                    ComposerRejectCode::CapacityExceeded,
                    "AI prompt reconciliation is full. Retry after pending prompts settle.",
                    execution_epoch_ms,
                )));
                return;
            }
            let reconcile_claude_prompt = session_kind == SessionKind::Claude
                && reconciliation == ComposerReconciliationReservation::Reserved;
            let reconcile_codex_prompt = session_kind == SessionKind::Codex
                && reconciliation == ComposerReconciliationReservation::Reserved;
            let handler = fence
                .inner
                .terminal_input_handler
                .read()
                .ok()
                .and_then(|slot| slot.as_ref().cloned());
            let callback_result = handler.map_or_else(
                || Err("The target PTY is not ready for input.".to_string()),
                |handler| {
                    invoke_terminal_input(
                        &handler,
                        RemoteTerminalInput::ComposerBatch {
                            session_id,
                            text: format!("{text}\r"),
                            attachments: decoded_attachments,
                            authority: RemoteWebMutationAuthority {
                                runtime_instance_id: fence.inner.runtime_instance_id.clone(),
                                connection_id,
                                client_id: fence.client_id.clone(),
                                lease_generation,
                            },
                        },
                        execution_epoch_ms,
                    )
                },
            );
            if let Err(message) = callback_result {
                if reconcile_claude_prompt {
                    host_service.cancel_claude_composer_prompt(&mutation_id);
                }
                if reconcile_codex_prompt {
                    host_service.cancel_codex_composer_prompt(&mutation_id);
                }
                if message == super::image_paste::WEB_COMPOSER_AUTHORITY_CHANGED {
                    clear_in_flight_composer(&fence.inner, &mutation_id, fingerprint);
                    completion.send(Err(composer_rejected(
                        &fence.inner,
                        connection_id,
                        mutation_id,
                        ComposerRejectCode::StaleGeneration,
                        message,
                        execution_epoch_ms,
                    )));
                    return;
                }
                let message = bounded_composer_error(&message);
                store_pty_rejection(&fence.inner, &mutation_id, fingerprint, &message);
                completion.send(Err(composer_rejected(
                    &fence.inner,
                    connection_id,
                    mutation_id,
                    ComposerRejectCode::PtyRejected,
                    message,
                    execution_epoch_ms,
                )));
                return;
            }

            let source = match session_kind {
                SessionKind::Claude => SemanticSource::Claude,
                SessionKind::Codex => SemanticSource::Codex,
                SessionKind::Shell => SemanticSource::Shell,
                SessionKind::Server => SemanticSource::Server,
                SessionKind::Ssh => SemanticSource::Ssh,
            };
            let kind = if session_kind.is_ai() {
                SemanticEventKind::UserMessage { text: text.clone() }
            } else {
                SemanticEventKind::Command {
                    command_id: mutation_id.clone(),
                    text: text.clone(),
                    exit_code: None,
                }
            };
            let occurred_at_epoch_ms = execution_epoch_ms;
            let published = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                publish_semantic_event(
                    &fence.inner,
                    SemanticEventDraft {
                        stable_session_key: stable_session_key.clone(),
                        occurred_at_epoch_ms,
                        source,
                        kind,
                        retention: SemanticRetention::Canonical,
                        deduplication_key: Some(format!("composer:{mutation_id}")),
                    },
                )
            }));
            if reconcile_claude_prompt {
                if published.is_ok() {
                    host_service.accept_claude_composer_prompt(&mutation_id);
                } else {
                    host_service.cancel_claude_composer_prompt(&mutation_id);
                }
            }
            if reconcile_codex_prompt {
                if published.is_ok() {
                    host_service.accept_codex_composer_prompt(&mutation_id);
                } else {
                    host_service.cancel_codex_composer_prompt(&mutation_id);
                }
            }
            let accepted_sequence = published.map(|event| event.sequence).unwrap_or_else(|_| {
                fence
                    .inner
                    .semantic_journals
                    .lock()
                    .ok()
                    .and_then(|journals| journals.metadata(&stable_session_key))
                    .map(|metadata| metadata.latest_sequence)
                    .unwrap_or(0)
            });
            let accepted = ComposerAccepted {
                mutation_id: mutation_id.clone(),
                stable_session_key: stable_session_key.clone(),
                accepted_sequence,
                lease_generation,
            };
            store_composer_mutation_outcome(
                &fence.inner,
                mutation_id,
                WebComposerMutationRecord {
                    fingerprint,
                    status: WebComposerMutationStatus::Accepted {
                        stable_session_key,
                        accepted_sequence,
                        lease_generation,
                    },
                },
            );
            completion.send(Ok(accepted));
        });
    if dispatch.is_err() {
        clear_in_flight_composer(inner, &dispatch_failure_mutation_id, fingerprint);
        return Err(composer_rejected(
            inner,
            connection_id,
            dispatch_failure_mutation_id,
            ComposerRejectCode::CapacityExceeded,
            "The terminal input queue is full. Retry this prompt.",
            now_epoch_ms,
        ));
    }
    Ok(None)
}

fn resolve_unique_session(
    inner: &Arc<RemoteHostInner>,
    stable_session_key: &StableSessionKey,
) -> Result<(String, SessionKind), ComposerRejectCode> {
    let _snapshot_guard = inner
        .snapshot_state_lock
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let tabs = inner
        .shared_state
        .read()
        .map(|state| state.open_tabs.clone())
        .unwrap_or_default();
    let sessions = inner
        .runtime_state
        .read()
        .map(|state| state.sessions.values().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    let mut matches = sessions.into_iter().filter(|session| {
        StableSessionKey::resolve(session, &tabs).as_ref() == Some(stable_session_key)
    });
    let Some(first) = matches.next() else {
        return Err(ComposerRejectCode::SessionNotFound);
    };
    if matches.next().is_some() {
        return Err(ComposerRejectCode::AmbiguousSession);
    }
    Ok((first.session_id, first.session_kind))
}

fn decode_composer_attachments(
    attachments: &[ComposerAttachment],
) -> Result<Vec<RemoteImageAttachment>, String> {
    use super::image_paste::WEB_PASTE_IMAGE_MAX_BYTES;

    if attachments.len() > MAX_COMPOSER_ATTACHMENTS {
        return Err(format!(
            "Composer submissions support at most {MAX_COMPOSER_ATTACHMENTS} attachments."
        ));
    }
    let max_encoded_bytes = WEB_PASTE_IMAGE_MAX_BYTES.div_ceil(3) * 4 + 4;
    let mut decoded = Vec::with_capacity(attachments.len());
    let mut total_bytes = 0_usize;
    for attachment in attachments {
        if !matches!(attachment.mime_type.as_str(), "image/png" | "image/jpeg") {
            return Err("Composer attachments must be PNG or JPEG images.".to_string());
        }
        if attachment
            .file_name
            .as_ref()
            .is_some_and(|name| name.len() > MAX_COMPOSER_FILE_NAME_BYTES)
        {
            return Err("Composer attachment file names are too long.".to_string());
        }
        if attachment.data_base64.len() > max_encoded_bytes {
            return Err("Composer attachment is too large. Max size is 5 MiB.".to_string());
        }
        let bytes = BASE64
            .decode(attachment.data_base64.as_bytes())
            .map_err(|error| format!("Invalid composer attachment: {error}"))?;
        if bytes.is_empty() || bytes.len() > WEB_PASTE_IMAGE_MAX_BYTES {
            return Err("Composer attachment must be non-empty and at most 5 MiB.".to_string());
        }
        total_bytes = total_bytes.saturating_add(bytes.len());
        if total_bytes > MAX_COMPOSER_ATTACHMENT_TOTAL_BYTES {
            return Err("Composer attachments exceed the 10 MiB total limit.".to_string());
        }
        decoded.push(RemoteImageAttachment {
            mime_type: attachment.mime_type.clone(),
            file_name: attachment.file_name.clone(),
            bytes,
        });
    }
    Ok(decoded)
}

fn valid_composer_mutation_id(mutation_id: &str) -> bool {
    !mutation_id.is_empty()
        && mutation_id.len() <= MAX_COMPOSER_MUTATION_ID_BYTES
        && mutation_id.bytes().all(|byte| byte.is_ascii_graphic())
}

fn bounded_composer_mutation_id(mutation_id: &str) -> String {
    if mutation_id.len() <= MAX_COMPOSER_MUTATION_ID_BYTES {
        return mutation_id.to_string();
    }
    let mut boundary = MAX_COMPOSER_MUTATION_ID_BYTES;
    while !mutation_id.is_char_boundary(boundary) {
        boundary -= 1;
    }
    mutation_id[..boundary].to_string()
}

fn valid_client_instance_id(client_instance_id: &str) -> bool {
    !client_instance_id.is_empty() && client_instance_id.len() <= MAX_CLIENT_INSTANCE_ID_BYTES
}

fn valid_stable_session_key(stable_session_key: &StableSessionKey) -> bool {
    !stable_session_key.as_str().is_empty()
        && stable_session_key.as_str().len() <= MAX_STABLE_SESSION_KEY_BYTES
}

fn valid_session_id(session_id: &str) -> bool {
    !session_id.is_empty() && session_id.len() <= MAX_SESSION_ID_BYTES
}

fn invoke_terminal_input(
    handler: &super::super::TerminalInputHandler,
    input: RemoteTerminalInput,
    now_epoch_ms: u64,
) -> Result<(), String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        handler(input, now_epoch_ms)
    }))
    .unwrap_or_else(|_| Err("The terminal input handler panicked.".to_string()))
    .map_err(|message| bounded_composer_error(&message))
}

fn bounded_composer_error(message: &str) -> String {
    if message.len() <= MAX_COMPOSER_ERROR_BYTES {
        return message.to_string();
    }
    let mut boundary = MAX_COMPOSER_ERROR_BYTES;
    while !message.is_char_boundary(boundary) {
        boundary -= 1;
    }
    message[..boundary].to_string()
}

fn store_composer_mutation_outcome(
    inner: &Arc<RemoteHostInner>,
    mutation_id: String,
    record: WebComposerMutationRecord,
) {
    let mut mutations = composer_mutations(inner);
    // A PTY callback can only run after its InFlight record was reserved.
    // If that invariant is ever violated, preserving the terminal outcome is
    // still safer than forgetting the ID after a possible PTY side effect.
    match mutations.entry(mutation_id) {
        std::collections::hash_map::Entry::Occupied(mut entry) => {
            entry.insert(record);
        }
        std::collections::hash_map::Entry::Vacant(entry) => {
            entry.insert(record);
        }
    }
}

fn clear_in_flight_composer(inner: &Arc<RemoteHostInner>, mutation_id: &str, fingerprint: u64) {
    let mut mutations = composer_mutations(inner);
    let is_matching_in_flight = mutations.get(mutation_id).is_some_and(|record| {
        record.fingerprint == fingerprint
            && matches!(record.status, WebComposerMutationStatus::InFlight)
    });
    if is_matching_in_flight {
        mutations.remove(mutation_id);
    }
}

fn store_pty_rejection(
    inner: &Arc<RemoteHostInner>,
    mutation_id: &str,
    fingerprint: u64,
    message: &str,
) {
    store_composer_mutation_outcome(
        inner,
        mutation_id.to_string(),
        WebComposerMutationRecord {
            fingerprint,
            status: WebComposerMutationStatus::PtyRejected {
                message: message.to_string(),
            },
        },
    );
}

fn composer_mutations(
    inner: &Arc<RemoteHostInner>,
) -> MutexGuard<'_, HashMap<String, WebComposerMutationRecord>> {
    match inner.web_composer_mutations.lock() {
        Ok(mutations) => mutations,
        Err(poisoned) => {
            let mutations = poisoned.into_inner();
            inner.web_composer_mutations.clear_poison();
            mutations
        }
    }
}

fn composer_rejected(
    inner: &Arc<RemoteHostInner>,
    connection_id: u64,
    mutation_id: String,
    code: ComposerRejectCode,
    message: impl Into<String>,
    now_epoch_ms: u64,
) -> ComposerRejected {
    ComposerRejected {
        mutation_id,
        code,
        message: message.into(),
        writer_lease: writer_lease_state(inner, connection_id, now_epoch_ms),
    }
}

fn composer_rejected_locked(
    inner: &Arc<RemoteHostInner>,
    connection_id: u64,
    mutation_id: String,
    code: ComposerRejectCode,
    message: impl Into<String>,
    now_epoch_ms: u64,
) -> ComposerRejected {
    ComposerRejected {
        mutation_id,
        code,
        message: message.into(),
        writer_lease: writer_lease_state_locked(inner, connection_id, now_epoch_ms),
    }
}

#[derive(Debug, Clone)]
pub(crate) enum EncodedFrame {
    Text(String),
    Binary(Vec<u8>),
}

impl EncodedFrame {
    fn encoded_len(&self) -> usize {
        match self {
            Self::Text(text) => text.len(),
            Self::Binary(bytes) => bytes.len(),
        }
    }
}

#[derive(Debug)]
pub(crate) struct WebConnectionTombstone {
    active: AtomicBool,
    cancellation: watch::Sender<bool>,
}

impl WebConnectionTombstone {
    fn new() -> Arc<Self> {
        let (cancellation, _) = watch::channel(true);
        Arc::new(Self {
            active: AtomicBool::new(true),
            cancellation,
        })
    }

    pub(crate) fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    fn deactivate(&self) -> bool {
        if self
            .active
            .compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            self.cancellation.send_replace(false);
            true
        } else {
            false
        }
    }

    fn subscribe(&self) -> watch::Receiver<bool> {
        self.cancellation.subscribe()
    }
}

#[derive(Debug)]
enum BrowserOutboundCommand {
    Frame {
        frame: EncodedFrame,
        deliver_when_inactive: bool,
        closes_connection: bool,
    },
    ReplayWake {
        epoch: u64,
    },
}

#[derive(Debug)]
struct ByteReservation {
    accounted_bytes: usize,
    queued_bytes: Arc<AtomicUsize>,
}

impl Drop for ByteReservation {
    fn drop(&mut self) {
        self.queued_bytes
            .fetch_sub(self.accounted_bytes, Ordering::AcqRel);
    }
}

#[derive(Debug)]
struct PendingReplay {
    epoch: u64,
    prefix: Option<EncodedFrame>,
    encoder: SemanticReplayPageEncoder,
    _reservation: ByteReservation,
}

type ReplaySlot = Arc<std::sync::Mutex<Option<PendingReplay>>>;

#[derive(Debug)]
struct AccountedBrowserCommand {
    command: BrowserOutboundCommand,
    accounted_bytes: usize,
    queued_bytes: Arc<AtomicUsize>,
}

impl Drop for AccountedBrowserCommand {
    fn drop(&mut self) {
        self.queued_bytes
            .fetch_sub(self.accounted_bytes, Ordering::AcqRel);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BrowserEnqueueError {
    Revoked,
    CommandFull,
    ByteFull,
    Closed,
    Serialization,
}

#[derive(Clone, Debug)]
pub(crate) struct BrowserOutboundSender {
    tx: tokio_mpsc::Sender<AccountedBrowserCommand>,
    queued_bytes: Arc<AtomicUsize>,
    max_bytes: usize,
    tombstone: Arc<WebConnectionTombstone>,
    replay_epoch: Arc<AtomicU64>,
    next_replay_id: Arc<AtomicU64>,
    replay_slot: ReplaySlot,
}

struct BrowserOutboundReceiver {
    rx: tokio_mpsc::Receiver<AccountedBrowserCommand>,
    tombstone: Arc<WebConnectionTombstone>,
    replay_epoch: Arc<AtomicU64>,
    replay_slot: ReplaySlot,
}

impl BrowserOutboundSender {
    fn channel(command_capacity: usize, max_bytes: usize) -> (Self, BrowserOutboundReceiver) {
        let (tx, rx) = tokio_mpsc::channel(command_capacity);
        let tombstone = WebConnectionTombstone::new();
        let replay_slot = Arc::new(std::sync::Mutex::new(None));
        let sender = Self {
            tx,
            queued_bytes: Arc::new(AtomicUsize::new(0)),
            max_bytes,
            tombstone: tombstone.clone(),
            replay_epoch: Arc::new(AtomicU64::new(0)),
            next_replay_id: Arc::new(AtomicU64::new(0)),
            replay_slot: replay_slot.clone(),
        };
        let replay_epoch = sender.replay_epoch.clone();
        (
            sender,
            BrowserOutboundReceiver {
                rx,
                tombstone,
                replay_epoch,
                replay_slot,
            },
        )
    }

    #[cfg(test)]
    pub(crate) fn detached_for_test(command_capacity: usize, max_bytes: usize) -> Self {
        let (sender, receiver) = Self::channel(command_capacity, max_bytes);
        std::mem::forget(receiver);
        sender
    }

    pub(crate) fn tombstone(&self) -> Arc<WebConnectionTombstone> {
        self.tombstone.clone()
    }

    pub(crate) fn is_active(&self) -> bool {
        self.tombstone.is_active()
    }

    fn reserve_and_send(
        &self,
        command: BrowserOutboundCommand,
        accounted_bytes: usize,
    ) -> Result<(), BrowserEnqueueError> {
        let reservation = self.reserve_bytes(accounted_bytes)?;
        let accounted = AccountedBrowserCommand {
            command,
            accounted_bytes: reservation.accounted_bytes,
            queued_bytes: reservation.queued_bytes.clone(),
        };
        std::mem::forget(reservation);
        self.tx.try_send(accounted).map_err(|error| match error {
            tokio_mpsc::error::TrySendError::Full(_) => BrowserEnqueueError::CommandFull,
            tokio_mpsc::error::TrySendError::Closed(_) => BrowserEnqueueError::Closed,
        })
    }

    fn reserve_bytes(
        &self,
        accounted_bytes: usize,
    ) -> Result<ByteReservation, BrowserEnqueueError> {
        if !self.is_active() {
            return Err(BrowserEnqueueError::Revoked);
        }
        let mut current = self.queued_bytes.load(Ordering::Acquire);
        loop {
            let Some(next) = current.checked_add(accounted_bytes) else {
                return Err(BrowserEnqueueError::ByteFull);
            };
            if next > self.max_bytes {
                return Err(BrowserEnqueueError::ByteFull);
            }
            match self.queued_bytes.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(observed) => current = observed,
            }
        }
        Ok(ByteReservation {
            accounted_bytes,
            queued_bytes: self.queued_bytes.clone(),
        })
    }

    pub(crate) fn try_send(&self, outbound: WsOutbound) -> Result<(), BrowserEnqueueError> {
        let frame = serialize_text(&outbound).ok_or(BrowserEnqueueError::Serialization)?;
        self.try_send_frame(frame)
    }

    pub(crate) fn try_send_server_message(
        &self,
        message: &ServerMessage,
        inner: &Arc<RemoteHostInner>,
        connection_id: u64,
        client_id: &str,
    ) -> Result<(), BrowserEnqueueError> {
        if matches!(
            message,
            ServerMessage::Snapshot { .. } | ServerMessage::Delta { .. }
        ) {
            let _operation = inner
                .web_control_operation_lock
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let _delivery = inner
                .semantic_delivery_lock
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let frame = translate_outbound_locked(message, inner, connection_id, client_id)
                .ok_or(BrowserEnqueueError::Serialization)?;
            self.try_send_frame(frame)
        } else {
            let frame = translate_outbound(message, inner, connection_id, client_id)
                .ok_or(BrowserEnqueueError::Serialization)?;
            self.try_send_frame(frame)
        }
    }

    pub(crate) fn try_send_frame(&self, frame: EncodedFrame) -> Result<(), BrowserEnqueueError> {
        let bytes = frame.encoded_len();
        self.reserve_and_send(
            BrowserOutboundCommand::Frame {
                frame,
                deliver_when_inactive: false,
                closes_connection: false,
            },
            bytes,
        )
    }

    fn try_send_disconnect(&self, message: String) -> Result<(), BrowserEnqueueError> {
        let frame = serialize_text(&WsOutbound::Disconnected { message })
            .ok_or(BrowserEnqueueError::Serialization)?;
        let bytes = frame.encoded_len();
        self.reserve_and_send(
            BrowserOutboundCommand::Frame {
                frame,
                deliver_when_inactive: true,
                closes_connection: true,
            },
            bytes,
        )
    }

    pub(crate) fn next_replay_epoch(&self) -> u64 {
        let epoch = self.replay_epoch.fetch_add(1, Ordering::AcqRel) + 1;
        let mut slot = self
            .replay_slot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *slot = None;
        epoch
    }

    pub(crate) fn next_replay_id(&self) -> u64 {
        self.next_replay_id.fetch_add(1, Ordering::AcqRel) + 1
    }

    pub(crate) fn supersede_replay(&self) {
        self.next_replay_epoch();
    }

    pub(crate) fn try_send_replay(
        &self,
        prefix: Option<EncodedFrame>,
        replay_id: u64,
        stable_session_key: StableSessionKey,
        from_sequence: u64,
        replay: Arc<SemanticReplay>,
        epoch: u64,
    ) -> Result<(), BrowserEnqueueError> {
        let accounted_bytes = prefix.as_ref().map_or(0, EncodedFrame::encoded_len)
            + replay.events.len()
                * std::mem::size_of::<Arc<super::super::presentation::SemanticEvent>>()
            + stable_session_key.as_str().len()
            + std::mem::size_of::<PendingReplay>();
        let mut slot = self
            .replay_slot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *slot = None;
        let reservation = self.reserve_bytes(accounted_bytes)?;
        *slot = Some(PendingReplay {
            epoch,
            prefix,
            encoder: SemanticReplayPageEncoder::new(
                replay_id,
                stable_session_key,
                from_sequence,
                replay,
            ),
            _reservation: reservation,
        });
        let result = self.reserve_and_send(
            BrowserOutboundCommand::ReplayWake { epoch },
            std::mem::size_of::<BrowserOutboundCommand>(),
        );
        if result.is_err() && slot.as_ref().is_some_and(|pending| pending.epoch == epoch) {
            *slot = None;
        }
        result
    }

    pub(crate) fn try_send_live_events(
        &self,
        events: &[Arc<super::super::presentation::SemanticEvent>],
    ) -> Result<(), BrowserEnqueueError> {
        let frames = events
            .iter()
            .map(|event| {
                serialize_text(&WsOutbound::SemanticEvent {
                    event: event.as_ref().clone(),
                })
                .ok_or(BrowserEnqueueError::Serialization)
            })
            .collect::<Result<Vec<_>, _>>()?;
        for frame in frames {
            self.try_send_frame(frame)?;
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn queued_bytes(&self) -> usize {
        self.queued_bytes.load(Ordering::Acquire)
    }
}

#[derive(Debug)]
struct SemanticReplayPageEncoder {
    replay_id: u64,
    stable_session_key: StableSessionKey,
    next_from_sequence: u64,
    replay: Arc<SemanticReplay>,
    next_event: usize,
    finished: bool,
    validated: bool,
}

fn cap_semantic_replay_for_mobile(mut replay: SemanticReplay) -> SemanticReplay {
    // Include JSON array delimiters and separators so the retained event
    // payload itself cannot cross the advertised mobile replay byte cap.
    let mut retained_bytes = 2_usize;
    let mut retained_events = 0_usize;
    let mut first_retained = replay.events.len();
    for index in (0..replay.events.len()).rev() {
        let event_bytes = serde_json::to_vec(replay.events[index].as_ref())
            .map_or(MAX_MOBILE_REPLAY_BYTES.saturating_add(1), |encoded| {
                encoded.len()
            });
        let separator_bytes = usize::from(retained_events > 0);
        if retained_events >= MAX_MOBILE_REPLAY_EVENTS
            || retained_bytes
                .saturating_add(separator_bytes)
                .saturating_add(event_bytes)
                > MAX_MOBILE_REPLAY_BYTES
        {
            break;
        }
        retained_bytes = retained_bytes
            .saturating_add(separator_bytes)
            .saturating_add(event_bytes);
        retained_events += 1;
        first_retained = index;
    }
    if first_retained > 0 {
        replay.events.drain(..first_retained);
        replay.cursor_rolled_over = true;
    }
    replay.oldest_sequence = replay.events.first().map_or(0, |event| event.sequence);
    replay
}

impl SemanticReplayPageEncoder {
    fn new(
        replay_id: u64,
        stable_session_key: StableSessionKey,
        from_sequence: u64,
        replay: Arc<SemanticReplay>,
    ) -> Self {
        Self {
            replay_id,
            stable_session_key,
            next_from_sequence: from_sequence,
            replay,
            next_event: 0,
            finished: false,
            validated: false,
        }
    }

    fn next_frame(&mut self) -> Result<Option<EncodedFrame>, String> {
        if self.finished {
            return Ok(None);
        }
        if !self.validated {
            let mut previous = self.next_from_sequence;
            for event in &self.replay.events {
                if event.sequence <= previous || event.sequence > self.replay.through_sequence {
                    return Err(format!(
                        "invalid semantic replay sequence {} after {} through {}",
                        event.sequence, previous, self.replay.through_sequence
                    ));
                }
                previous = event.sequence;
            }
            self.validated = true;
        }

        let base_len = serde_json::to_vec(&WsOutbound::SemanticReplayPage {
            page: SemanticReplayPage {
                replay_id: self.replay_id,
                stable_session_key: self.stable_session_key.clone(),
                from_sequence: u64::MAX,
                through_sequence: self.replay.through_sequence,
                next_sequence: u64::MAX,
                rollover: self.replay.cursor_rolled_over,
                complete: false,
                events: Vec::new(),
            },
        })
        .map_err(|error| error.to_string())?
        .len();

        let start = self.next_event;
        let mut estimated_len = base_len;
        while self.next_event < self.replay.events.len()
            && self.next_event - start < MAX_SEMANTIC_REPLAY_PAGE_EVENTS
        {
            let event_len = serde_json::to_vec(self.replay.events[self.next_event].as_ref())
                .map_err(|error| error.to_string())?
                .len();
            let separator_len = usize::from(self.next_event > start);
            if self.next_event > start
                && estimated_len + separator_len + event_len > MAX_SEMANTIC_REPLAY_PAGE_BYTES
            {
                break;
            }
            if self.next_event == start
                && estimated_len + event_len > MAX_SEMANTIC_REPLAY_PAGE_BYTES
            {
                return Err(format!(
                    "semantic event {} cannot fit in a replay page",
                    self.replay.events[self.next_event].sequence
                ));
            }
            estimated_len += separator_len + event_len;
            self.next_event += 1;
        }

        let complete = self.next_event == self.replay.events.len();
        let next_sequence = if complete {
            self.replay.through_sequence
        } else {
            self.replay.events[self.next_event - 1].sequence
        };
        let page = SemanticReplayPage {
            replay_id: self.replay_id,
            stable_session_key: self.stable_session_key.clone(),
            from_sequence: self.next_from_sequence,
            through_sequence: self.replay.through_sequence,
            next_sequence,
            rollover: self.replay.cursor_rolled_over,
            complete,
            events: self.replay.events[start..self.next_event].to_vec(),
        };
        let text = serde_json::to_string(&WsOutbound::SemanticReplayPage { page })
            .map_err(|error| error.to_string())?;
        if text.len() > MAX_SEMANTIC_REPLAY_PAGE_BYTES {
            return Err(format!(
                "semantic replay page exceeded byte cap: {} > {}",
                text.len(),
                MAX_SEMANTIC_REPLAY_PAGE_BYTES
            ));
        }
        self.next_from_sequence = next_sequence;
        self.finished = complete;
        Ok(Some(EncodedFrame::Text(text)))
    }
}

/// Convert native broadcaster messages into browser-only wire frames. Native
/// snapshots are treated strictly as host-side source data and are projected
/// through the allowlist before JSON serialization. Native deltas intentionally
/// trigger a fresh full projection for now.
fn translate_outbound(
    message: &ServerMessage,
    inner: &Arc<RemoteHostInner>,
    connection_id: u64,
    client_id: &str,
) -> Option<EncodedFrame> {
    match message {
        ServerMessage::Snapshot { .. } | ServerMessage::Delta { .. } => {
            serialize_web_snapshot(capture_web_snapshot(inner, connection_id, client_id))
        }
        _ => encode_outbound(message),
    }
}

fn translate_outbound_locked(
    message: &ServerMessage,
    inner: &Arc<RemoteHostInner>,
    connection_id: u64,
    client_id: &str,
) -> Option<EncodedFrame> {
    match message {
        ServerMessage::Snapshot { .. } | ServerMessage::Delta { .. } => serialize_web_snapshot(
            capture_web_snapshot_inner(inner, connection_id, client_id, true),
        ),
        _ => encode_outbound(message),
    }
}

fn capture_web_snapshot(
    inner: &Arc<RemoteHostInner>,
    connection_id: u64,
    client_id: &str,
) -> WebWorkspaceSnapshot {
    capture_web_snapshot_inner(inner, connection_id, client_id, false)
}

fn capture_web_snapshot_inner(
    inner: &Arc<RemoteHostInner>,
    connection_id: u64,
    client_id: &str,
    web_control_operation_locked: bool,
) -> WebWorkspaceSnapshot {
    loop {
        let generation_before = inner
            .semantic_publication_generation
            .load(Ordering::Acquire);
        if generation_before % 2 != 0 {
            std::thread::yield_now();
            continue;
        }
        let (snapshot, revision, semantic_metadata) = {
            let _snapshot_guard = inner
                .snapshot_state_lock
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let semantic_journals = match inner.semantic_journals.lock() {
                Ok(guard) => guard,
                Err(poisoned) => {
                    let guard = poisoned.into_inner();
                    inner.semantic_journals.clear_poison();
                    guard
                }
            };
            let semantic_metadata = semantic_journals.metadata_snapshot();
            (
                light_snapshot(inner, client_id),
                inner.snapshot_revision.load(Ordering::Relaxed),
                semantic_metadata,
            )
        };
        let generation_after = inner
            .semantic_publication_generation
            .load(Ordering::Acquire);
        if generation_before == generation_after {
            let writer_lease = if web_control_operation_locked {
                writer_lease_state_locked(inner, connection_id, now_epoch_ms())
            } else {
                writer_lease_state(inner, connection_id, now_epoch_ms())
            };
            return project_web_snapshot(
                inner,
                &snapshot,
                revision,
                &semantic_metadata,
                &writer_lease,
            );
        }
        std::thread::yield_now();
    }
}

fn project_web_snapshot(
    inner: &Arc<RemoteHostInner>,
    snapshot: &RemoteWorkspaceSnapshot,
    revision: u64,
    semantic_metadata: &HashMap<StableSessionKey, SemanticSessionMetadata>,
    lease: &WebWriterLeaseState,
) -> WebWorkspaceSnapshot {
    let mut projected = WebWorkspaceSnapshot::from_host(
        inner.runtime_instance_id.clone(),
        revision,
        &snapshot.app_state,
        &snapshot.runtime_state,
        &snapshot.port_statuses,
        lease,
        semantic_metadata,
    );
    projected.server_id = snapshot.server_id.clone();
    projected
}

fn serialize_web_snapshot(workspace: WebWorkspaceSnapshot) -> Option<EncodedFrame> {
    serialize_text(&WsOutbound::Snapshot { workspace })
}

/// Translate a `ServerMessage` (the type the broadcaster produces) into a
/// WS frame. Returns `None` for variants that only make sense on the TCP
/// path (e.g., `HelloOk`, `PortForwardOk`).
fn encode_outbound(message: &ServerMessage) -> Option<EncodedFrame> {
    match message {
        ServerMessage::Snapshot { .. } | ServerMessage::Delta { .. } => None,
        ServerMessage::Pong => serialize_text(&WsOutbound::Pong),
        ServerMessage::Error { message } => serialize_text(&WsOutbound::Error {
            message: message.clone(),
        }),
        ServerMessage::Disconnected { message } => serialize_text(&WsOutbound::Disconnected {
            message: message.clone(),
        }),
        ServerMessage::Response { request_id, result } => serialize_text(&WsOutbound::Response {
            id: *request_id,
            result: WebActionResult::from_remote(result),
        }),
        ServerMessage::SessionStream { event } => encode_session_stream(event),
        ServerMessage::HelloOk { .. }
        | ServerMessage::PortForwardOk
        | ServerMessage::HelloErr { .. } => None,
    }
}

fn encode_session_stream(event: &RemoteSessionStreamEvent) -> Option<EncodedFrame> {
    match event {
        RemoteSessionStreamEvent::Output {
            session_id,
            chunk_seq,
            bytes,
            ..
        } => Some(EncodedFrame::Binary(encode_session_output_frame(
            session_id, *chunk_seq, bytes,
        ))),
        RemoteSessionStreamEvent::Bootstrap { bootstrap } => {
            use base64::engine::general_purpose::STANDARD;
            use base64::Engine;
            let replay_base64 = if bootstrap.screen.rows > 0 && bootstrap.screen.cols > 0 {
                String::new()
            } else {
                STANDARD.encode(&bootstrap.replay_bytes)
            };
            serialize_text(&WsOutbound::SessionBootstrap {
                session_id: bootstrap.session_id.clone(),
                replay_base64,
                screen: bootstrap.screen.clone(),
            })
        }
        RemoteSessionStreamEvent::Closed { session_id, .. } => {
            serialize_text(&WsOutbound::SessionClosed {
                session_id: session_id.clone(),
            })
        }
        RemoteSessionStreamEvent::Removed { session_id } => {
            serialize_text(&WsOutbound::SessionRemoved {
                session_id: session_id.clone(),
            })
        }
        // RuntimePatch is covered by the periodic delta fanout; no need to
        // stream it separately. A follow-up can add a dedicated wire frame if
        // this turns out to matter for responsiveness.
        RemoteSessionStreamEvent::RuntimePatch { .. } => None,
    }
}

/// Binary frame layout for session output:
/// ```text
///   [0]       frame type (0x01)
///   [1..5)    big-endian u32: session_id UTF-8 length
///   [5..5+N)  session_id UTF-8
///   [5+N..13+N)  big-endian u64: chunk_seq
///   [13+N..)  raw PTY bytes
/// ```
pub(crate) fn encode_session_output_frame(
    session_id: &str,
    chunk_seq: u64,
    bytes: &[u8],
) -> Vec<u8> {
    let id_bytes = session_id.as_bytes();
    let id_len = id_bytes.len() as u32;
    let mut out = Vec::with_capacity(1 + 4 + id_bytes.len() + 8 + bytes.len());
    out.push(BINARY_FRAME_SESSION_OUTPUT);
    out.extend_from_slice(&id_len.to_be_bytes());
    out.extend_from_slice(id_bytes);
    out.extend_from_slice(&chunk_seq.to_be_bytes());
    out.extend_from_slice(bytes);
    out
}

fn serialize_text(value: &WsOutbound) -> Option<EncodedFrame> {
    serde_json::to_string(value).ok().map(EncodedFrame::Text)
}

#[cfg(test)]
mod tests {
    use super::super::action::{WebAction, WebAiKind};
    use super::*;
    use crate::remote::{
        deliver_pending_bootstraps, PairedWebClient, RemoteActionPayload, RemoteHostConfig,
        RemoteHostService, RemoteSessionBootstrap, PROTOCOL_VERSION,
    };
    use crate::state::{AiLaunchSpec, SessionDimensions, SessionKind, SessionRuntimeState};
    use crate::terminal::session::{TerminalBackend, TerminalScreenSnapshot, TerminalSessionView};
    use futures_util::Sink;
    use std::collections::HashMap;
    use std::future::Future;
    use std::path::PathBuf;
    use std::pin::Pin;
    use std::sync::mpsc as std_mpsc;
    use std::task::{Context, Poll};

    fn test_web_sender() -> BrowserOutboundSender {
        let (sender, receiver) = BrowserOutboundSender::channel(4096, WEB_OUTBOUND_MAX_BYTES * 4);
        // Focused bridge tests drive the synchronous handler directly rather
        // than a Tokio writer task. Keep the bounded receiver alive so lease
        // fanout exercises try_send without treating the fixture as dead.
        std::mem::forget(receiver);
        sender
    }

    fn test_web_channel() -> (BrowserOutboundSender, BrowserOutboundReceiver) {
        BrowserOutboundSender::channel(4096, WEB_OUTBOUND_MAX_BYTES * 4)
    }

    #[test]
    fn valid_cookie_cannot_authorize_a_cross_origin_websocket() {
        let mut config = RemoteHostConfig::default();
        config.server_id = "ws-origin-host".to_string();
        config.web.enabled = true;
        let service = RemoteHostService::new(config);
        pair_web_client(&service, "paired-browser");
        let state = WebState {
            inner: service.inner.clone(),
            pairing_attempts: Arc::new(std::sync::Mutex::new(Default::default())),
        };
        let config = service.config();
        let signed = super::super::sign_cookie(&config.web.cookie_secret_hex, "paired-browser")
            .expect("signed browser cookie");
        assert_eq!(
            super::super::verify_cookie(&config.web.cookie_secret_hex, &signed).as_deref(),
            Some("paired-browser"),
            "the fixture must carry a genuinely valid auth cookie"
        );
        let cookie_name = super::super::cookie_name_for_server_id(&config.server_id);
        drop(config);

        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::HOST,
            "devmanager.test:43872".parse().unwrap(),
        );
        headers.insert(
            axum::http::header::ORIGIN,
            "https://attacker.example".parse().unwrap(),
        );
        headers.insert(
            axum::http::header::COOKIE,
            format!("{cookie_name}={signed}").parse().unwrap(),
        );

        assert_eq!(
            authorize_ws_request(&state, &headers),
            Err(StatusCode::FORBIDDEN)
        );
    }

    fn try_recv_web_text(receiver: &mut BrowserOutboundReceiver) -> String {
        let accounted = receiver.rx.try_recv().expect("browser command");
        let frame = match &accounted.command {
            BrowserOutboundCommand::Frame { frame, .. } => frame.clone(),
            BrowserOutboundCommand::ReplayWake { epoch } => {
                let mut slot = receiver
                    .replay_slot
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let pending = slot
                    .as_mut()
                    .filter(|pending| pending.epoch == *epoch)
                    .expect("matching replay slot");
                let frame = if let Some(prefix) = pending.prefix.take() {
                    prefix
                } else {
                    pending
                        .encoder
                        .next_frame()
                        .expect("encode replay frame")
                        .expect("replay frame")
                };
                if pending.encoder.finished {
                    *slot = None;
                }
                frame
            }
        };
        match frame {
            EncodedFrame::Text(text) => text,
            EncodedFrame::Binary(_) => panic!("expected browser text frame"),
        }
    }

    fn try_recv_web_json(receiver: &mut BrowserOutboundReceiver) -> serde_json::Value {
        serde_json::from_str(&try_recv_web_text(receiver)).expect("browser json frame")
    }

    fn try_recv_web_binary(receiver: &mut BrowserOutboundReceiver) -> Vec<u8> {
        let accounted = receiver.rx.try_recv().expect("browser command");
        match &accounted.command {
            BrowserOutboundCommand::Frame {
                frame: EncodedFrame::Binary(bytes),
                ..
            } => bytes.clone(),
            other => panic!("expected browser binary frame, got {other:?}"),
        }
    }

    fn assert_session_output_frame(frame: &[u8], session_id: &str, expected: &[u8]) {
        assert_eq!(frame[0], BINARY_FRAME_SESSION_OUTPUT);
        let id_len =
            u32::from_be_bytes(frame[1..5].try_into().expect("session id length")) as usize;
        assert_eq!(&frame[5..5 + id_len], session_id.as_bytes());
        assert_eq!(&frame[5 + id_len + 8..], expected);
    }

    struct StalledSink;

    impl Sink<WsMessage> for StalledSink {
        type Error = ();

        fn poll_ready(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Pending
        }

        fn start_send(self: Pin<&mut Self>, _item: WsMessage) -> Result<(), Self::Error> {
            Ok(())
        }

        fn poll_flush(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Pending
        }

        fn poll_close(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
    }

    struct SlowSink {
        delay: Duration,
        waiting: Option<Pin<Box<tokio::time::Sleep>>>,
        ready: bool,
        sent: Arc<AtomicUsize>,
    }

    impl SlowSink {
        fn new(delay: Duration, sent: Arc<AtomicUsize>) -> Self {
            Self {
                delay,
                waiting: None,
                ready: false,
                sent,
            }
        }
    }

    impl Sink<WsMessage> for SlowSink {
        type Error = ();

        fn poll_ready(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            if self.ready {
                return Poll::Ready(Ok(()));
            }
            if self.waiting.is_none() {
                self.waiting = Some(Box::pin(tokio::time::sleep(self.delay)));
            }
            let waiting = self.waiting.as_mut().expect("slow sink timer");
            if waiting.as_mut().poll(cx).is_pending() {
                return Poll::Pending;
            }
            self.waiting = None;
            self.ready = true;
            Poll::Ready(Ok(()))
        }

        fn start_send(mut self: Pin<&mut Self>, _item: WsMessage) -> Result<(), Self::Error> {
            assert!(self.ready);
            self.ready = false;
            self.sent.fetch_add(1, Ordering::SeqCst);
            Ok(())
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

    fn replay_event(
        sequence: u64,
        text_bytes: usize,
    ) -> Arc<super::super::super::presentation::SemanticEvent> {
        Arc::new(super::super::super::presentation::SemanticEvent {
            stable_session_key: StableSessionKey::from_server("paged"),
            sequence,
            replaces_sequence: None,
            occurred_at_epoch_ms: sequence,
            source: SemanticSource::Server,
            kind: SemanticEventKind::Output {
                stream: super::super::super::presentation::SemanticStream::Stdout,
                text: "x".repeat(text_bytes),
            },
        })
    }

    #[test]
    fn semantic_replay_pages_obey_exact_event_and_serialized_byte_caps() {
        let key = StableSessionKey::from_server("paged");
        let replay = Arc::new(super::super::super::presentation::SemanticReplay {
            oldest_sequence: 3,
            through_sequence: 1_401,
            cursor_rolled_over: true,
            events: (0..700)
                .map(|index| replay_event(3 + index * 2, 2_000))
                .collect(),
        });
        let mut encoder = SemanticReplayPageEncoder::new(77, key.clone(), 1, replay);
        let mut frames = Vec::new();
        while let Some(frame) = encoder.next_frame().expect("page encode") {
            frames.push(frame);
        }

        assert!(frames.len() > 3, "byte cap should split before count alone");
        let mut observed = Vec::new();
        let mut prior_next = 1;
        for (index, frame) in frames.iter().enumerate() {
            let EncodedFrame::Text(text) = frame else {
                panic!("semantic pages must be JSON text");
            };
            assert!(text.len() <= MAX_SEMANTIC_REPLAY_PAGE_BYTES);
            let value: serde_json::Value = serde_json::from_str(text).expect("page json");
            let events = value["events"].as_array().expect("events");
            assert!(events.len() <= MAX_SEMANTIC_REPLAY_PAGE_EVENTS);
            assert_eq!(value["type"], "semanticReplayPage");
            assert_eq!(value["replayId"], 77);
            assert_eq!(value["stableSessionKey"], key.as_str());
            assert_eq!(value["fromSequence"], prior_next);
            assert_eq!(value["throughSequence"], 1_401);
            assert_eq!(value["rollover"], true);
            observed.extend(
                events
                    .iter()
                    .map(|event| event["sequence"].as_u64().expect("sequence")),
            );
            prior_next = value["nextSequence"].as_u64().expect("next sequence");
            assert_eq!(value["complete"], index + 1 == frames.len());
        }
        assert_eq!(
            observed,
            (0..700).map(|index| 3 + index * 2).collect::<Vec<_>>()
        );
        assert_eq!(prior_next, 1_401);
    }

    #[test]
    fn mobile_replay_caps_events_and_bytes_and_marks_rollover() {
        let replay = SemanticReplay {
            oldest_sequence: 1,
            through_sequence: 5_001,
            cursor_rolled_over: false,
            events: (1..=5_001)
                .map(|sequence| replay_event(sequence, 8))
                .collect(),
        };

        let capped = cap_semantic_replay_for_mobile(replay);

        assert_eq!(capped.events.len(), 5_000);
        assert_eq!(capped.events.first().unwrap().sequence, 2);
        assert_eq!(capped.through_sequence, 5_001);
        assert!(capped.cursor_rolled_over);
        let retained_bytes = serde_json::to_vec(&capped.events).unwrap().len();
        assert!(retained_bytes <= 2 * 1024 * 1024);
    }

    #[test]
    fn replay_writer_uses_one_total_send_deadline() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let client_id = "slow-replay";
        pair_web_client(&service, client_id);
        let (native, _native_rx) = std_mpsc::channel();
        let (sender, receiver) = test_web_channel();
        let tombstone = sender.tombstone();
        assert!(register_client(
            &service.inner,
            1,
            client_id,
            native,
            sender.clone(),
        ));
        let key = StableSessionKey::from_server("slow-replay");
        let replay = Arc::new(SemanticReplay {
            oldest_sequence: 1,
            through_sequence: 600,
            cursor_rolled_over: false,
            events: (1..=600)
                .map(|sequence| replay_event(sequence, 32))
                .collect(),
        });
        let epoch = sender.next_replay_epoch();
        sender
            .try_send_replay(None, sender.next_replay_id(), key, 0, replay, epoch)
            .expect("queue replay");
        sender
            .try_send_disconnect("test complete".to_string())
            .expect("queue terminal command");
        drop(sender);
        let sent = Arc::new(AtomicUsize::new(0));
        let observed = sent.clone();

        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("test runtime")
            .block_on(async {
                run_browser_writer(
                    &mut SlowSink::new(Duration::from_millis(20), observed),
                    receiver,
                    service.inner.clone(),
                    1,
                    client_id.to_string(),
                    tombstone.clone(),
                    Duration::from_millis(30),
                )
                .await;
            });

        assert!(!tombstone.is_active());
        assert!(sent.load(Ordering::SeqCst) < 3, "deadline reset per page");
    }

    #[test]
    fn empty_semantic_replay_has_one_complete_highwater_page() {
        let key = StableSessionKey::from_server("empty-page");
        let replay = Arc::new(super::super::super::presentation::SemanticReplay {
            oldest_sequence: 0,
            through_sequence: 41,
            cursor_rolled_over: false,
            events: Vec::new(),
        });
        let mut encoder = SemanticReplayPageEncoder::new(9, key, 40, replay);

        let EncodedFrame::Text(text) = encoder
            .next_frame()
            .expect("encode")
            .expect("empty completion page")
        else {
            panic!("text frame");
        };
        let value: serde_json::Value = serde_json::from_str(&text).expect("page json");
        assert_eq!(value["fromSequence"], 40);
        assert_eq!(value["throughSequence"], 41);
        assert_eq!(value["nextSequence"], 41);
        assert_eq!(value["complete"], true);
        assert_eq!(value["events"], serde_json::json!([]));
        assert!(encoder.next_frame().expect("finished").is_none());
    }

    #[test]
    fn replay_replacement_and_supersession_drop_old_snapshot_ownership() {
        let (sender, _receiver) = BrowserOutboundSender::channel(8, WEB_OUTBOUND_MAX_BYTES);
        let key = StableSessionKey::from_server("paged");
        let first = Arc::new(SemanticReplay {
            oldest_sequence: 1,
            through_sequence: 1,
            cursor_rolled_over: false,
            events: vec![replay_event(1, 8)],
        });
        let second = Arc::new(SemanticReplay {
            oldest_sequence: 2,
            through_sequence: 2,
            cursor_rolled_over: false,
            events: vec![replay_event(2, 8)],
        });
        let wake_bytes = std::mem::size_of::<BrowserOutboundCommand>();

        let first_epoch = sender.next_replay_epoch();
        sender
            .try_send_replay(
                None,
                sender.next_replay_id(),
                key.clone(),
                0,
                first.clone(),
                first_epoch,
            )
            .expect("first replay");
        assert_eq!(Arc::strong_count(&first), 2);

        let second_epoch = sender.next_replay_epoch();
        assert_eq!(Arc::strong_count(&first), 1, "replacement drops old Arc");
        sender
            .try_send_replay(
                None,
                sender.next_replay_id(),
                key,
                1,
                second.clone(),
                second_epoch,
            )
            .expect("replacement replay");
        assert_eq!(Arc::strong_count(&second), 2);

        sender.supersede_replay();
        assert_eq!(Arc::strong_count(&second), 1, "supersede drops replay Arc");
        assert_eq!(
            sender.queued_bytes(),
            wake_bytes * 2,
            "only the two bounded stale wake tokens remain queued"
        );
    }

    #[test]
    fn unsubscribe_clears_pending_replay_slot_and_cursor() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let client_id = "unsubscribe-client";
        pair_web_client(&service, client_id);
        let (native, _native_rx) = std_mpsc::channel();
        let (sender, _receiver) = test_web_channel();
        let tombstone = sender.tombstone();
        assert!(register_client(
            &service.inner,
            1,
            client_id,
            native,
            sender.clone(),
        ));
        let key = StableSessionKey::from_server("paged");
        service
            .inner
            .clients
            .lock()
            .expect("clients lock")
            .get_mut(&1)
            .expect("registered client")
            .semantic_cursors
            .insert(key.clone(), 0);
        let replay = Arc::new(SemanticReplay {
            oldest_sequence: 1,
            through_sequence: 1,
            cursor_rolled_over: false,
            events: vec![replay_event(1, 8)],
        });
        let epoch = sender.next_replay_epoch();
        sender
            .try_send_replay(
                None,
                sender.next_replay_id(),
                key.clone(),
                0,
                replay.clone(),
                epoch,
            )
            .expect("pending replay");

        unsubscribe_semantic(&service.inner, 1, client_id, &key, Some(&tombstone));

        assert_eq!(Arc::strong_count(&replay), 1);
        assert_eq!(
            sender.queued_bytes(),
            std::mem::size_of::<BrowserOutboundCommand>()
        );
        assert!(!service
            .inner
            .clients
            .lock()
            .expect("clients lock")
            .get(&1)
            .expect("registered client")
            .semantic_cursors
            .contains_key(&key));
    }

    #[test]
    fn resume_without_replay_clears_old_slot_and_queues_response_after_stale_wake() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let client_id = "resume-no-replay";
        pair_web_client(&service, client_id);
        let (native, _native_rx) = std_mpsc::channel();
        let (sender, mut receiver) = test_web_channel();
        let tombstone = sender.tombstone();
        assert!(register_client(
            &service.inner,
            1,
            client_id,
            native,
            sender.clone(),
        ));
        let replay = Arc::new(SemanticReplay {
            oldest_sequence: 1,
            through_sequence: 1,
            cursor_rolled_over: false,
            events: vec![replay_event(1, 8)],
        });
        let epoch = sender.next_replay_epoch();
        sender
            .try_send_replay(
                None,
                sender.next_replay_id(),
                StableSessionKey::from_server("paged"),
                0,
                replay.clone(),
                epoch,
            )
            .expect("old replay");
        let lane = WebResponseLane(InboundResponder::Browser {
            sender: sender.clone(),
            inner: service.inner.clone(),
            connection_id: 1,
            client_id: client_id.to_string(),
        });

        send_resume_state_with_lane(
            &service.inner,
            1,
            client_id,
            resume_request(None, None, "tab-a"),
            1_000,
            &lane,
            Some(&tombstone),
        );

        assert_eq!(Arc::strong_count(&replay), 1);
        let stale = receiver.rx.try_recv().expect("stale replay wake");
        assert!(matches!(
            &stale.command,
            BrowserOutboundCommand::ReplayWake { .. }
        ));
        let resume = try_recv_web_json(&mut receiver);
        assert_eq!(resume["type"], "resumeState");
        assert!(resume["semanticReplay"].is_null());
    }

    #[test]
    fn byte_budget_saturates_before_command_count_limit() {
        let pong_bytes = serialize_text(&WsOutbound::Pong)
            .expect("pong frame")
            .encoded_len();
        let (sender, _receiver) = BrowserOutboundSender::channel(8, pong_bytes);

        sender.try_send(WsOutbound::Pong).expect("first pong");
        assert_eq!(
            sender.try_send(WsOutbound::Pong),
            Err(BrowserEnqueueError::ByteFull)
        );
        assert!(sender.is_active());
    }

    #[test]
    fn revoked_tombstone_lease_paths_return_without_deadlock() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let client_id = "revoked-lease-client";
        pair_web_client(&service, client_id);
        let (native, _native_rx) = std_mpsc::channel();
        let (sender, _receiver) = test_web_channel();
        let tombstone = sender.tombstone();
        assert!(register_client(
            &service.inner,
            1,
            client_id,
            native,
            sender,
        ));
        let granted = acquire_writer_lease_for_connection(
            &service.inner,
            1,
            client_id,
            "phone",
            1_000,
            Some(&tombstone),
        );
        assert!(granted.you_are_owner);
        assert!(revoke_web_connection(
            &service.inner,
            1,
            client_id,
            &tombstone,
            None,
        ));

        let (done_tx, done_rx) = std_mpsc::channel();
        let inner = service.inner.clone();
        let tombstone_for_thread = tombstone.clone();
        std::thread::spawn(move || {
            let acquire = acquire_writer_lease_for_connection(
                &inner,
                1,
                client_id,
                "phone",
                1_001,
                Some(&tombstone_for_thread),
            );
            let renew = renew_writer_lease(
                &inner,
                1,
                client_id,
                "phone",
                granted.generation,
                true,
                1_002,
                Some(&tombstone_for_thread),
            );
            let visibility = set_writer_visibility_for_connection(
                &inner,
                1,
                client_id,
                "phone",
                false,
                1_003,
                Some(&tombstone_for_thread),
            );
            done_tx
                .send((acquire, renew, visibility))
                .expect("timeout observer");
        });
        let (acquire, renew, visibility) = done_rx
            .recv_timeout(Duration::from_millis(250))
            .expect("revoked branches must not deadlock");
        assert!(!acquire.you_are_owner);
        assert!(!renew.you_are_owner);
        assert!(!visibility.you_are_owner);
    }

    #[test]
    fn exact_tombstone_revoke_is_idempotent_and_preserves_same_cookie_viewer() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let client_id = "same-cookie-exact";
        pair_web_client(&service, client_id);
        let (native_one, _native_one_rx) = std_mpsc::channel();
        let (native_two, _native_two_rx) = std_mpsc::channel();
        let (owner, _owner_rx) = test_web_channel();
        let (viewer, _viewer_rx) = test_web_channel();
        let owner_tombstone = owner.tombstone();
        let viewer_tombstone = viewer.tombstone();
        assert!(register_client(
            &service.inner,
            1,
            client_id,
            native_one,
            owner,
        ));
        assert!(register_client(
            &service.inner,
            2,
            client_id,
            native_two,
            viewer,
        ));
        assert!(
            acquire_writer_lease_for_connection(
                &service.inner,
                1,
                client_id,
                "owner-tab",
                1_000,
                Some(&owner_tombstone),
            )
            .you_are_owner
        );

        assert!(!revoke_web_connection(
            &service.inner,
            1,
            client_id,
            &viewer_tombstone,
            None,
        ));
        assert!(viewer_tombstone.is_active());
        assert!(writer_lease_state(&service.inner, 1, 1_001).you_are_owner);

        assert!(revoke_web_connection(
            &service.inner,
            1,
            client_id,
            &owner_tombstone,
            None,
        ));
        assert!(!revoke_web_connection(
            &service.inner,
            1,
            client_id,
            &owner_tombstone,
            None,
        ));
        let clients = service.inner.clients.lock().expect("clients lock");
        assert!(!clients.contains_key(&1));
        assert!(clients.contains_key(&2));
        drop(clients);
        assert!(viewer_tombstone.is_active());
        assert!(writer_lease_state(&service.inner, 2, 1_002)
            .owner_client_instance_id
            .is_none());
    }

    #[test]
    fn duplicate_connection_registration_cannot_replace_live_tombstone() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let client_id = "duplicate-registration";
        pair_web_client(&service, client_id);
        let (native_one, _native_one_rx) = std_mpsc::channel();
        let (native_two, _native_two_rx) = std_mpsc::channel();
        let (first, _first_rx) = test_web_channel();
        let (replacement, _replacement_rx) = test_web_channel();
        let first_tombstone = first.tombstone();
        assert!(register_client(
            &service.inner,
            7,
            client_id,
            native_one,
            first,
        ));
        assert!(!register_client(
            &service.inner,
            7,
            client_id,
            native_two,
            replacement,
        ));
        let registered = service
            .inner
            .clients
            .lock()
            .expect("clients lock")
            .get(&7)
            .and_then(|client| client.web_tombstone.clone())
            .expect("original registration");
        assert!(Arc::ptr_eq(&registered, &first_tombstone));
        assert!(registered.is_active());
    }

    #[test]
    fn stalled_writer_times_out_and_revokes_exact_registration() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let client_id = "stalled-writer";
        pair_web_client(&service, client_id);
        let (native, _native_rx) = std_mpsc::channel();
        let (sender, receiver) = test_web_channel();
        let tombstone = sender.tombstone();
        assert!(register_client(
            &service.inner,
            1,
            client_id,
            native,
            sender.clone(),
        ));
        sender.try_send(WsOutbound::Pong).expect("queued frame");

        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("test runtime")
            .block_on(async {
                run_browser_writer(
                    &mut StalledSink,
                    receiver,
                    service.inner.clone(),
                    1,
                    client_id.to_string(),
                    tombstone.clone(),
                    Duration::from_millis(10),
                )
                .await;
            });

        assert!(!tombstone.is_active());
        assert!(!service
            .inner
            .clients
            .lock()
            .expect("clients lock")
            .contains_key(&1));
    }

    #[test]
    fn one_browser_queue_orders_initial_resume_replay_lease_live_and_raw_frames() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let client_id = "fifo-client";
        pair_web_client(&service, client_id);
        let (native, _native_rx) = std_mpsc::channel();
        let (sender, mut receiver) = test_web_channel();
        assert!(register_client(
            &service.inner,
            1,
            client_id,
            native,
            sender.clone(),
        ));
        sender
            .try_send_server_message(
                &ServerMessage::Snapshot {
                    snapshot: RemoteWorkspaceSnapshot::default(),
                },
                &service.inner,
                1,
                client_id,
            )
            .expect("initial snapshot");
        let key = StableSessionKey::from_server("paged");
        let replay = Arc::new(SemanticReplay {
            oldest_sequence: 1,
            through_sequence: 1,
            cursor_rolled_over: false,
            events: vec![replay_event(1, 8)],
        });
        let replay_id = sender.next_replay_id();
        let prefix = serialize_text(&WsOutbound::ResumeState {
            state: ResumeState {
                runtime_instance_id: "runtime".to_string(),
                revision: 1,
                hard_reset: false,
                route: "/sessions".to_string(),
                desired_session_key: Some(key.clone()),
                workspace: None,
                semantic_replay: Some(SemanticReplayDescriptor {
                    replay_id,
                    stable_session_key: key.clone(),
                    from_sequence: 0,
                    through_sequence: 1,
                    rollover: false,
                }),
                writer_lease: WebWriterLeaseState::default(),
            },
        })
        .expect("resume prefix");
        let epoch = sender.next_replay_epoch();
        sender
            .try_send_replay(Some(prefix), replay_id, key.clone(), 0, replay, epoch)
            .expect("resume replay");
        sender
            .try_send(WsOutbound::WriterLeaseState {
                writer_lease: WebWriterLeaseState::default(),
            })
            .expect("lease frame");
        sender
            .try_send_live_events(&[replay_event(2, 8)])
            .expect("live semantic frame");
        sender
            .try_send_server_message(
                &ServerMessage::SessionStream {
                    event: RemoteSessionStreamEvent::Output {
                        session_id: "raw".to_string(),
                        chunk_seq: 1,
                        emitted_at_epoch_ms: 1,
                        bytes: b"raw".to_vec(),
                    },
                },
                &service.inner,
                1,
                client_id,
            )
            .expect("raw frame");

        assert_eq!(try_recv_web_json(&mut receiver)["type"], "snapshot");
        let wake = receiver.rx.try_recv().expect("resume replay wake");
        let wake_epoch = match &wake.command {
            BrowserOutboundCommand::ReplayWake { epoch } => *epoch,
            other => panic!("expected replay wake, got {other:?}"),
        };
        let mut slot = receiver.replay_slot.lock().expect("replay slot");
        let pending = slot
            .as_mut()
            .filter(|pending| pending.epoch == wake_epoch)
            .expect("pending replay");
        let EncodedFrame::Text(resume) = pending.prefix.take().expect("resume prefix") else {
            panic!("resume is text");
        };
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&resume).expect("resume json")["type"],
            "resumeState"
        );
        let EncodedFrame::Text(page) = pending
            .encoder
            .next_frame()
            .expect("replay encode")
            .expect("replay page")
        else {
            panic!("replay is text");
        };
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&page).expect("page json")["type"],
            "semanticReplayPage"
        );
        drop(slot);
        drop(wake);
        assert_eq!(try_recv_web_json(&mut receiver)["type"], "writerLeaseState");
        assert_eq!(try_recv_web_json(&mut receiver)["type"], "semanticEvent");
        let raw = try_recv_web_binary(&mut receiver);
        assert_session_output_frame(&raw, "raw", b"raw");
    }

    #[test]
    fn normal_snapshot_waits_behind_resume_capture_and_enqueue() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let (sender, mut receiver) = test_web_channel();
        let delivery = service
            .inner
            .semantic_delivery_lock
            .lock()
            .expect("delivery lock");
        let (started_tx, started_rx) = std_mpsc::channel();
        let (done_tx, done_rx) = std_mpsc::channel();
        let snapshot_sender = sender.clone();
        let snapshot_inner = service.inner.clone();
        let snapshot_thread = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            let result = snapshot_sender.try_send_server_message(
                &ServerMessage::Snapshot {
                    snapshot: RemoteWorkspaceSnapshot::default(),
                },
                &snapshot_inner,
                1,
                "web-client",
            );
            done_tx.send(result).unwrap();
        });
        started_rx.recv().unwrap();

        assert!(
            done_rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "normal snapshot bypassed the resume delivery boundary"
        );
        sender
            .try_send(WsOutbound::ResumeState {
                state: ResumeState {
                    runtime_instance_id: service.inner.runtime_instance_id.clone(),
                    revision: 1,
                    hard_reset: false,
                    route: "/sessions".to_string(),
                    desired_session_key: None,
                    workspace: None,
                    semantic_replay: None,
                    writer_lease: WebWriterLeaseState::default(),
                },
            })
            .expect("resume frame");
        drop(delivery);
        done_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("snapshot completed after resume")
            .expect("snapshot queued");
        snapshot_thread.join().unwrap();

        assert_eq!(try_recv_web_json(&mut receiver)["type"], "resumeState");
        assert_eq!(try_recv_web_json(&mut receiver)["type"], "snapshot");
    }

    #[test]
    fn snapshot_waiting_for_authority_does_not_hold_the_semantic_delivery_lock() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let (sender, _receiver) = test_web_channel();
        let authority = service
            .inner
            .web_control_operation_lock
            .lock()
            .expect("authority lock");
        let (started_tx, started_rx) = std_mpsc::channel();
        let (done_tx, done_rx) = std_mpsc::channel();
        let snapshot_sender = sender.clone();
        let snapshot_inner = service.inner.clone();
        let snapshot_thread = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            let result = snapshot_sender.try_send_server_message(
                &ServerMessage::Snapshot {
                    snapshot: RemoteWorkspaceSnapshot::default(),
                },
                &snapshot_inner,
                1,
                "web-client",
            );
            done_tx.send(result).unwrap();
        });
        started_rx.recv().unwrap();
        assert!(done_rx.recv_timeout(Duration::from_millis(50)).is_err());
        let delivery = service
            .inner
            .semantic_delivery_lock
            .try_lock()
            .expect("snapshot inverted authority and semantic delivery locks");
        drop(delivery);
        drop(authority);
        done_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("snapshot completed")
            .expect("snapshot queued");
        snapshot_thread.join().unwrap();
    }

    #[test]
    fn saturation_revocation_blocks_composer_raw_action_and_request_side_effects() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let client_id = "saturation-race";
        pair_web_client(&service, client_id);
        ai_session(&service, "tab-a", "session-a");
        let writes = Arc::new(AtomicUsize::new(0));
        let observed_writes = writes.clone();
        service.set_terminal_input_handler(Some(Arc::new(move |_, _| {
            observed_writes.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })));
        let (native, _native_rx) = std_mpsc::channel();
        let (sender, _receiver) = BrowserOutboundSender::channel(1, WEB_OUTBOUND_MAX_BYTES);
        let tombstone = sender.tombstone();
        assert!(register_client(
            &service.inner,
            1,
            client_id,
            native,
            sender.clone(),
        ));
        let lease = acquire_writer_lease_for_connection(
            &service.inner,
            1,
            client_id,
            "phone",
            1_000,
            Some(&tombstone),
        );
        assert!(lease.you_are_owner);
        {
            let _operation = service
                .inner
                .web_control_operation_lock
                .lock()
                .expect("operation lock");
            broadcast_writer_lease_state_locked(&service.inner, 1_001);
        }
        assert!(!tombstone.is_active());

        let composer = process_composer_submit_for_connection(
            &service.inner,
            1,
            client_id,
            "saturated-composer".to_string(),
            StableSessionKey::from_tab("tab-a"),
            "hello".to_string(),
            Vec::new(),
            lease.generation,
            1_002,
            Some(&tombstone),
        );
        assert!(composer.is_err());
        handle_inbound_browser(
            &service.inner,
            1,
            client_id,
            WsInbound::Input {
                session_id: "session-a".to_string(),
                text: "raw".to_string(),
                expected_lease_generation: Some(lease.generation),
            },
            &sender,
        );
        handle_inbound_browser(
            &service.inner,
            1,
            client_id,
            WsInbound::Action {
                action: WebAction::StopAllServers,
                expected_lease_generation: Some(lease.generation),
            },
            &sender,
        );
        handle_inbound_browser(
            &service.inner,
            1,
            client_id,
            WsInbound::Request {
                id: 9,
                action: WebAction::StopAllServers,
                expected_lease_generation: Some(lease.generation),
            },
            &sender,
        );
        assert_eq!(writes.load(Ordering::SeqCst), 0);
        assert!(service
            .inner
            .pending_requests
            .lock()
            .expect("pending requests")
            .is_empty());
    }

    #[derive(Debug, Clone, Copy)]
    enum SemanticPublicationCase {
        Output,
        Runtime,
    }

    fn assert_browser_capture_waits_for_semantic_publication(case: SemanticPublicationCase) {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let mut app = crate::state::AppState::default();
        app.open_tabs.push(crate::models::SessionTab {
            id: "semantic-tab".to_string(),
            tab_type: crate::models::TabType::Claude,
            pty_session_id: Some("semantic-runtime".to_string()),
            ..crate::models::SessionTab::default()
        });
        let mut runtime = SessionRuntimeState::new(
            "semantic-runtime",
            PathBuf::new(),
            SessionDimensions::default(),
            TerminalBackend::default(),
        );
        runtime.session_kind = SessionKind::Claude;
        runtime.tab_id = Some("semantic-tab".to_string());
        let mut runtime_state = crate::state::RuntimeState::default();
        runtime_state
            .sessions
            .insert(runtime.session_id.clone(), runtime.clone());
        service.update_snapshot(app, runtime_state, HashMap::new());
        let initial = capture_web_snapshot(&service.inner, 1, "web-client");
        let initial_latest_sequence = initial
            .sessions
            .iter()
            .find(|session| session.session_id == "semantic-runtime")
            .expect("semantic session")
            .latest_sequence;

        let (mutation_reached_tx, mutation_reached_rx) = std_mpsc::channel();
        let (resume_tx, resume_rx) = std_mpsc::channel();
        let resume_rx = Arc::new(std::sync::Mutex::new(resume_rx));
        let hook_resume_rx = resume_rx.clone();
        *service
            .inner
            .semantic_publication_test_hook
            .write()
            .expect("publication hook lock") = Some(Arc::new(move || {
            mutation_reached_tx
                .send(())
                .expect("publication observer should remain open");
            let _ = hook_resume_rx.lock().expect("publication gate lock").recv();
        }));

        let publisher_service = service.clone();
        let publisher = std::thread::spawn(move || match case {
            SemanticPublicationCase::Output => {
                publisher_service.push_session_output("semantic-runtime", b"new output".to_vec())
            }
            SemanticPublicationCase::Runtime => {
                runtime.status = crate::state::SessionStatus::Running;
                runtime.unseen_ready = true;
                runtime.notification_count = 3;
                publisher_service.push_session_runtime("semantic-runtime", runtime);
            }
        });
        mutation_reached_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("semantic mutation should reach the pre-revision gate");

        let capture_inner = service.inner.clone();
        let (capture_tx, capture_rx) = std_mpsc::channel();
        let capture = std::thread::spawn(move || {
            capture_tx
                .send(capture_web_snapshot(&capture_inner, 1, "web-client"))
                .expect("capture receiver should remain open");
        });
        let premature = capture_rx.recv_timeout(Duration::from_millis(100)).ok();

        resume_tx.send(()).expect("publisher should remain open");
        publisher.join().expect("publisher should complete");
        if let Some(snapshot) = premature {
            capture.join().expect("capture should complete");
            panic!(
                "{case:?} capture exposed semantic revision {} before publication advanced from {}",
                snapshot.revision, initial.revision
            );
        }

        let snapshot = capture_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("capture should complete after publication");
        capture.join().expect("capture should complete");
        let session = snapshot
            .sessions
            .iter()
            .find(|session| session.session_id == "semantic-runtime")
            .expect("semantic session");
        assert!(snapshot.revision > initial.revision);
        assert!(session.latest_sequence > initial_latest_sequence);
        if matches!(case, SemanticPublicationCase::Runtime) {
            assert_eq!(session.attention_count, 3);
        }
    }

    #[test]
    fn browser_capture_waits_for_output_semantic_revision_publication() {
        assert_browser_capture_waits_for_semantic_publication(SemanticPublicationCase::Output);
    }

    #[test]
    fn browser_capture_waits_for_runtime_semantic_revision_publication() {
        assert_browser_capture_waits_for_semantic_publication(SemanticPublicationCase::Runtime);
    }

    #[test]
    fn browser_capture_recovers_after_semantic_publication_panic() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let mut app = crate::state::AppState::default();
        let mut runtime_state = crate::state::RuntimeState::default();

        for (tab_id, session_id) in [
            ("exhausted-tab", "exhausted-runtime"),
            ("healthy-tab", "healthy-runtime"),
        ] {
            app.open_tabs.push(crate::models::SessionTab {
                id: tab_id.to_string(),
                tab_type: crate::models::TabType::Claude,
                pty_session_id: Some(session_id.to_string()),
                ..crate::models::SessionTab::default()
            });
            let mut runtime = SessionRuntimeState::new(
                session_id,
                PathBuf::new(),
                SessionDimensions::default(),
                TerminalBackend::default(),
            );
            runtime.session_kind = SessionKind::Claude;
            runtime.tab_id = Some(tab_id.to_string());
            runtime_state
                .sessions
                .insert(runtime.session_id.clone(), runtime);
        }

        service.update_snapshot(app, runtime_state, HashMap::new());
        let initial = capture_web_snapshot(&service.inner, 1, "web-client");
        let initial_exhausted = initial
            .sessions
            .iter()
            .find(|session| session.session_id == "exhausted-runtime")
            .expect("exhausted semantic session");
        let initial_exhausted_latest = initial_exhausted.latest_sequence;
        assert!(
            initial_exhausted_latest > 0,
            "the exhausted session should begin with published metadata"
        );
        let initial_healthy_latest = initial
            .sessions
            .iter()
            .find(|session| session.session_id == "healthy-runtime")
            .expect("healthy semantic session")
            .latest_sequence;

        service
            .inner
            .semantic_journals
            .lock()
            .expect("semantic journals lock")
            .set_next_sequence_for_test(&StableSessionKey::from_tab("exhausted-tab"), u64::MAX);

        let publication = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            service.push_session_output("exhausted-runtime", b"exhaust sequence".to_vec());
        }));
        assert!(
            publication.is_err(),
            "sequence exhaustion should still panic"
        );
        assert!(
            !service.inner.semantic_journals.is_poisoned(),
            "panic recovery should not poison the semantic journal store"
        );
        assert_eq!(
            service
                .inner
                .semantic_publication_generation
                .load(Ordering::Acquire)
                % 2,
            0,
            "publication generation should recover to an even value"
        );

        let recovered = capture_web_snapshot(&service.inner, 1, "web-client");
        assert!(
            recovered.revision > initial.revision,
            "panic recovery must conservatively publish a new revision"
        );
        assert_eq!(
            recovered
                .sessions
                .iter()
                .find(|session| session.session_id == "exhausted-runtime")
                .expect("exhausted semantic session should remain projected")
                .latest_sequence,
            initial_exhausted_latest,
            "panic recovery must preserve previously published metadata"
        );

        service.push_session_output("healthy-runtime", b"publication still works".to_vec());
        let subsequent = capture_web_snapshot(&service.inner, 1, "web-client");
        assert!(subsequent.revision > recovered.revision);
        assert!(
            subsequent
                .sessions
                .iter()
                .find(|session| session.session_id == "healthy-runtime")
                .expect("healthy semantic session should remain projected")
                .latest_sequence
                > initial_healthy_latest,
            "a healthy journal should publish successfully after the panic"
        );
    }

    #[test]
    fn resume_projection_generation_retry_is_bounded() {
        let generation = AtomicU64::new(0);
        let captures = AtomicUsize::new(0);

        let value = capture_with_bounded_generation(
            &generation,
            || {
                captures.fetch_add(1, Ordering::SeqCst);
                generation.fetch_add(2, Ordering::SeqCst);
                1
            },
            || 99,
        );

        assert_eq!(captures.load(Ordering::SeqCst), MAX_RESUME_CAPTURE_ATTEMPTS);
        assert_eq!(value, 99);
    }

    fn pair_web_client(service: &RemoteHostService, client_id: &str) {
        let mut config = service.inner.config.write().expect("config lock");
        if config
            .web
            .paired_clients
            .iter()
            .any(|client| client.client_id == client_id)
        {
            return;
        }
        config.web.paired_clients.push(PairedWebClient {
            client_id: client_id.to_string(),
            browser_install_id: format!("browser-install-{client_id}"),
            nickname: None,
            label: "Test Browser".to_string(),
            issued_at_epoch_ms: Some(1),
            last_seen_epoch_ms: Some(1),
            last_seen_ip: Some("127.0.0.1".to_string()),
            user_agent: Some("Test Browser".to_string()),
            browser_family: Some("Chrome".to_string()),
            browser_version: Some("135".to_string()),
            os_family: Some("Windows".to_string()),
            device_class: Some("desktop".to_string()),
        });
    }

    fn ai_session(service: &RemoteHostService, tab_id: &str, session_id: &str) {
        let mut app = crate::state::AppState::default();
        app.open_tabs.push(crate::models::SessionTab {
            id: tab_id.to_string(),
            tab_type: crate::models::TabType::Claude,
            pty_session_id: Some(session_id.to_string()),
            ..crate::models::SessionTab::default()
        });
        let mut runtime = SessionRuntimeState::new(
            session_id,
            PathBuf::new(),
            SessionDimensions::default(),
            TerminalBackend::default(),
        );
        runtime.session_kind = SessionKind::Claude;
        runtime.tab_id = Some(tab_id.to_string());
        let mut runtime_state = crate::state::RuntimeState::default();
        runtime_state
            .sessions
            .insert(session_id.to_string(), runtime);
        service.update_snapshot(app, runtime_state, HashMap::new());
    }

    fn codex_session(service: &RemoteHostService, tab_id: &str, session_id: &str) {
        let mut app = crate::state::AppState::default();
        app.open_tabs.push(crate::models::SessionTab {
            id: tab_id.to_string(),
            tab_type: crate::models::TabType::Codex,
            pty_session_id: Some(session_id.to_string()),
            ..crate::models::SessionTab::default()
        });
        let mut runtime = SessionRuntimeState::new(
            session_id,
            PathBuf::new(),
            SessionDimensions::default(),
            TerminalBackend::default(),
        );
        runtime.session_kind = SessionKind::Codex;
        runtime.tab_id = Some(tab_id.to_string());
        let mut runtime_state = crate::state::RuntimeState::default();
        runtime_state
            .sessions
            .insert(session_id.to_string(), runtime);
        service.update_snapshot(app, runtime_state, HashMap::new());
    }

    fn resume_request(
        runtime_instance_id: Option<String>,
        desired_session_key: Option<StableSessionKey>,
        client_instance_id: &str,
    ) -> ResumeRequest {
        ResumeRequest {
            seen_runtime_instance_id: runtime_instance_id,
            seen_revision: None,
            route: desired_session_key
                .as_ref()
                .map(|key| format!("/session/{}", key.as_str().replace(':', "/")))
                .unwrap_or_else(|| "/sessions".to_string()),
            desired_session_key,
            raw_session_id: None,
            semantic_after_sequence: Some(0),
            client_instance_id: client_instance_id.to_string(),
            visible: true,
            wants_writer_lease: true,
        }
    }

    #[test]
    fn resume_runtime_mismatch_is_a_hard_reset_to_sessions() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let state = build_resume_state(
            &service.inner,
            1,
            "web-client",
            resume_request(
                Some("stale-runtime".to_string()),
                Some(StableSessionKey::from_tab("gone")),
                "tab-a",
            ),
            1_000,
        );

        assert!(state.hard_reset);
        assert_eq!(state.route, "/sessions");
        assert!(state.desired_session_key.is_none());
        assert!(state.semantic_replay.is_none());
        assert!(state.workspace.is_some());
    }

    #[test]
    fn same_cookie_tabs_get_connection_specific_lease_state() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let current_runtime = service.inner.runtime_instance_id.clone();
        let now = now_epoch_ms();
        let first = build_resume_state(
            &service.inner,
            1,
            "same-cookie-client",
            resume_request(Some(current_runtime), None, "tab-a"),
            now,
        );

        let owner_snapshot = capture_web_snapshot(&service.inner, 1, "same-cookie-client");
        let viewer_snapshot = capture_web_snapshot(&service.inner, 2, "same-cookie-client");
        assert!(first.writer_lease.you_are_owner);
        assert!(owner_snapshot.writer_lease.you_are_owner);
        assert!(!viewer_snapshot.writer_lease.you_are_owner);
        assert_eq!(
            owner_snapshot.writer_lease.generation,
            viewer_snapshot.writer_lease.generation
        );
    }

    #[test]
    fn native_controller_blocks_automatic_web_acquisition() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        *service
            .inner
            .controller_client_id
            .write()
            .expect("controller lock") = Some("native-desktop".to_string());
        let current_runtime = service.inner.runtime_instance_id.clone();

        let state = build_resume_state(
            &service.inner,
            1,
            "web-client",
            resume_request(Some(current_runtime), None, "tab-a"),
            1_000,
        );

        assert!(!state.writer_lease.you_are_owner);
        assert_eq!(
            service
                .inner
                .controller_client_id
                .read()
                .expect("controller lock")
                .as_deref(),
            Some("native-desktop")
        );
    }

    #[test]
    fn disconnect_releases_only_the_exact_same_cookie_owner() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        pair_web_client(&service, "same-cookie");
        let (owner_tx, _owner_rx) = std_mpsc::channel();
        let (viewer_tx, _viewer_rx) = std_mpsc::channel();
        register_client(
            &service.inner,
            1,
            "same-cookie",
            owner_tx,
            test_web_sender(),
        );
        register_client(
            &service.inner,
            2,
            "same-cookie",
            viewer_tx,
            test_web_sender(),
        );
        let current_runtime = service.inner.runtime_instance_id.clone();
        let now = now_epoch_ms();
        let state = build_resume_state(
            &service.inner,
            1,
            "same-cookie",
            resume_request(Some(current_runtime), None, "tab-a"),
            now,
        );
        assert!(state.writer_lease.you_are_owner);

        unregister_client(&service.inner, 2, "same-cookie");
        assert!(writer_lease_state(&service.inner, 1, now + 1).you_are_owner);
        unregister_client(&service.inner, 1, "same-cookie");

        let released = writer_lease_state(&service.inner, 1, now + 2);
        assert!(!released.you_are_owner);
        assert!(released.owner_client_instance_id.is_none());
        assert!(service
            .inner
            .controller_client_id
            .read()
            .expect("controller lock")
            .is_none());
    }

    #[test]
    fn local_desktop_takeover_invalidates_the_web_generation() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let current_runtime = service.inner.runtime_instance_id.clone();
        let state = build_resume_state(
            &service.inner,
            1,
            "web-client",
            resume_request(Some(current_runtime), None, "tab-a"),
            1_000,
        );
        let generation = state.writer_lease.generation;

        service.take_local_control();

        let result = service
            .inner
            .web_control
            .lock()
            .expect("lease lock")
            .writer_leases_mut()
            .authorize(1, "web-client", generation, 1_001);
        assert!(matches!(result, Err(LeaseError::StaleGeneration { .. })));
        assert!(service
            .inner
            .controller_client_id
            .read()
            .expect("controller lock")
            .is_none());
    }

    #[test]
    fn semantic_resume_does_not_subscribe_to_raw_terminal_or_call_provider() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        pair_web_client(&service, "web-client");
        ai_session(&service, "tab-a", "session-a");
        let (std_tx, _std_rx) = std_mpsc::channel::<ServerMessage>();
        register_client(&service.inner, 1, "web-client", std_tx, test_web_sender());
        let provider_called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let called = provider_called.clone();
        service.set_session_bootstrap_provider(Some(Arc::new(move |_| {
            called.store(true, Ordering::SeqCst);
            None
        })));

        let current_runtime = service.inner.runtime_instance_id.clone();
        let (response_tx, mut response_rx) = tokio_mpsc::unbounded_channel();
        send_resume_state(
            &service.inner,
            1,
            "web-client",
            resume_request(
                Some(current_runtime),
                Some(StableSessionKey::from_tab("tab-a")),
                "tab-a",
            ),
            1_000,
            &response_tx,
        );
        let state = match response_rx.try_recv().expect("resume response") {
            WsOutbound::ResumeState { state } => state,
            other => panic!("unexpected response: {other:?}"),
        };

        assert_eq!(
            state
                .desired_session_key
                .as_ref()
                .map(StableSessionKey::as_str),
            Some("tab:tab-a")
        );
        assert!(!provider_called.load(Ordering::SeqCst));
        let clients = service.inner.clients.lock().expect("clients lock");
        let client = clients.get(&1).expect("resume connection");
        assert!(client.subscribed_session_ids.is_empty());
        assert!(client.bootstrap_pending_session_ids.is_empty());
    }

    #[test]
    fn raw_resume_marks_only_requested_terminal_bootstrap_pending() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        pair_web_client(&service, "web-client");
        ai_session(&service, "tab-a", "session-a");
        let (std_tx, _std_rx) = std_mpsc::channel::<ServerMessage>();
        register_client(&service.inner, 1, "web-client", std_tx, test_web_sender());

        let current_runtime = service.inner.runtime_instance_id.clone();
        let mut request = resume_request(
            Some(current_runtime),
            Some(StableSessionKey::from_tab("tab-a")),
            "tab-a",
        );
        request.raw_session_id = Some("session-a".to_string());
        let (response_tx, _response_rx) = tokio_mpsc::unbounded_channel();
        send_resume_state(
            &service.inner,
            1,
            "web-client",
            request,
            1_000,
            &response_tx,
        );

        let clients = service.inner.clients.lock().expect("clients lock");
        let client = clients.get(&1).expect("resume connection");
        assert_eq!(
            client.subscribed_session_ids,
            HashSet::from(["session-a".to_string()])
        );
        assert_eq!(
            client.bootstrap_pending_session_ids,
            HashSet::from(["session-a".to_string()])
        );
    }

    #[test]
    fn hidden_resume_and_visibility_loss_never_mark_a_session_visibly_focused() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        pair_web_client(&service, "web-client");
        ai_session(&service, "tab-a", "session-a");
        let (std_tx, _std_rx) = std_mpsc::channel::<ServerMessage>();
        register_client(&service.inner, 1, "web-client", std_tx, test_web_sender());
        let mut ready = SessionRuntimeState::new(
            "session-a",
            PathBuf::new(),
            SessionDimensions::default(),
            TerminalBackend::default(),
        );
        ready.session_kind = SessionKind::Claude;
        ready.tab_id = Some("tab-a".to_string());
        ready.status = crate::state::SessionStatus::Running;
        ready.unseen_ready = true;
        ready.notification_count = 1;
        service.push_session_runtime("session-a", ready);
        let key = StableSessionKey::from_tab("tab-a");
        assert_eq!(
            service.semantic_session_metadata(&key).unwrap().attention,
            crate::remote::presentation::SemanticAttention::Unread
        );
        let (focused_tx, focused_rx) = std_mpsc::channel();
        service.set_focused_session_handler(Some(Arc::new(move |session_id| {
            let _ = focused_tx.send(session_id);
        })));
        let (response_tx, mut response_rx) = tokio_mpsc::unbounded_channel();

        let mut hidden = resume_request(
            Some(service.inner.runtime_instance_id.clone()),
            Some(StableSessionKey::from_tab("tab-a")),
            "phone",
        );
        hidden.visible = false;
        hidden.wants_writer_lease = false;
        send_resume_state(&service.inner, 1, "web-client", hidden, 1_000, &response_tx);
        let _ = response_rx.try_recv().expect("hidden resume response");
        assert!(service
            .inner
            .clients
            .lock()
            .unwrap()
            .get(&1)
            .unwrap()
            .focused_session_id
            .is_none());
        assert!(focused_rx.try_recv().is_err());
        assert_eq!(
            service.semantic_session_metadata(&key).unwrap().attention,
            crate::remote::presentation::SemanticAttention::Unread
        );

        send_resume_state(
            &service.inner,
            1,
            "web-client",
            resume_request(
                Some(service.inner.runtime_instance_id.clone()),
                Some(StableSessionKey::from_tab("tab-a")),
                "phone",
            ),
            1_001,
            &response_tx,
        );
        let _ = response_rx.try_recv().expect("visible resume response");
        assert_eq!(
            service
                .inner
                .clients
                .lock()
                .unwrap()
                .get(&1)
                .unwrap()
                .focused_session_id
                .as_deref(),
            Some("session-a")
        );
        assert_eq!(
            focused_rx.recv_timeout(Duration::from_millis(250)).unwrap(),
            "session-a"
        );
        assert_eq!(
            service.semantic_session_metadata(&key).unwrap().attention,
            crate::remote::presentation::SemanticAttention::None
        );

        let (native_tx, _native_rx) = tokio_mpsc::unbounded_channel();
        let (web_tx, _web_rx) = tokio_mpsc::unbounded_channel();
        handle_inbound_with_web(
            &service.inner,
            1,
            "web-client",
            WsInbound::SetVisibility {
                client_instance_id: "phone".to_string(),
                visible: false,
            },
            &native_tx,
            &web_tx,
        );
        assert!(service
            .inner
            .clients
            .lock()
            .unwrap()
            .get(&1)
            .unwrap()
            .focused_session_id
            .is_none());
    }

    #[test]
    fn composer_ack_follows_pty_acceptance_and_identical_retry_is_deduplicated() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        ai_session(&service, "tab-a", "session-a");
        let current_runtime = service.inner.runtime_instance_id.clone();
        let resume = build_resume_state(
            &service.inner,
            1,
            "web-client",
            resume_request(
                Some(current_runtime),
                Some(StableSessionKey::from_tab("tab-a")),
                "tab-a",
            ),
            1_000,
        );
        let generation = resume.writer_lease.generation;
        let (seen_tx, seen_rx) = std_mpsc::channel();
        service.set_terminal_input_handler(Some(Arc::new(move |input, _| {
            seen_tx.send(input).expect("capture input");
            Ok(())
        })));

        let submit = || {
            process_composer_submit(
                &service.inner,
                1,
                "web-client",
                "mutation-1".to_string(),
                StableSessionKey::from_tab("tab-a"),
                "hello".to_string(),
                Vec::new(),
                generation,
                1_100,
            )
        };
        let first = submit().expect("first submit accepted");
        let duplicate = submit().expect("identical retry returns stored ack");

        assert_eq!(first, duplicate);
        match seen_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("PTY write")
        {
            RemoteTerminalInput::ComposerBatch {
                session_id,
                text,
                attachments,
                ..
            } => {
                assert_eq!(session_id, "session-a");
                assert_eq!(text, "hello\r");
                assert!(attachments.is_empty());
            }
            other => panic!("unexpected PTY input: {other:?}"),
        }
        assert!(
            seen_rx.try_recv().is_err(),
            "duplicate must not write twice"
        );
        let replay = service
            .semantic_replay(&StableSessionKey::from_tab("tab-a"), 0)
            .expect("semantic replay");
        assert_eq!(
            replay
                .events
                .iter()
                .filter(|event| matches!(
                    &event.kind,
                    crate::remote::presentation::SemanticEventKind::UserMessage { text }
                        if text == "hello"
                ))
                .count(),
            1
        );
        assert_eq!(first.accepted_sequence, replay.through_sequence);
    }

    #[test]
    fn composer_authority_change_at_pty_boundary_is_retryable() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        ai_session(&service, "tab-a", "session-a");
        let resume = build_resume_state(
            &service.inner,
            1,
            "web-client",
            resume_request(
                Some(service.inner.runtime_instance_id.clone()),
                Some(StableSessionKey::from_tab("tab-a")),
                "tab-a",
            ),
            1_000,
        );
        let writes = Arc::new(AtomicUsize::new(0));
        let observed = writes.clone();
        service.set_terminal_input_handler(Some(Arc::new(move |_, _| {
            observed.fetch_add(1, Ordering::SeqCst);
            Err(super::super::image_paste::WEB_COMPOSER_AUTHORITY_CHANGED.to_string())
        })));
        let submit = || {
            process_composer_submit(
                &service.inner,
                1,
                "web-client",
                "authority-race".to_string(),
                StableSessionKey::from_tab("tab-a"),
                "hello".to_string(),
                Vec::new(),
                resume.writer_lease.generation,
                1_100,
            )
            .unwrap_err()
        };

        assert_eq!(submit().code, ComposerRejectCode::StaleGeneration);
        assert_eq!(submit().code, ComposerRejectCode::StaleGeneration);
        assert_eq!(writes.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn full_claude_reconciliation_queue_rejects_1025th_before_pty_write() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        ai_session(&service, "tab-a", "session-a");
        let stable_key = StableSessionKey::from_tab("tab-a");
        let identity = crate::remote::ClaudeSemanticIdentity {
            pty_session_id: "session-a".to_string(),
            stable_session_key: stable_key.clone(),
            registration_generation: 7,
        };
        service.push_claude_adapter_registered(identity.clone());
        for index in 0..1_024 {
            let _ = service.reserve_claude_composer_prompt(
                &format!("reserved-{index}"),
                &identity.pty_session_id,
                &stable_key,
                &format!("reserved prompt {index}"),
            );
        }
        assert_eq!(
            service
                .inner
                .claude_composer_reconciliation
                .lock()
                .unwrap()
                .pending
                .len(),
            1_024
        );
        let generation = build_resume_state(
            &service.inner,
            1,
            "web-client",
            resume_request(
                Some(service.inner.runtime_instance_id.clone()),
                Some(stable_key.clone()),
                "tab-a",
            ),
            1_000,
        )
        .writer_lease
        .generation;
        let writes = Arc::new(AtomicUsize::new(0));
        let observed_writes = writes.clone();
        service.set_terminal_input_handler(Some(Arc::new(move |_, _| {
            observed_writes.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })));

        let submit = || {
            process_composer_submit(
                &service.inner,
                1,
                "web-client",
                "overflow-claude".to_string(),
                stable_key.clone(),
                "overflow prompt".to_string(),
                Vec::new(),
                generation,
                1_100,
            )
            .unwrap_err()
        };
        assert_eq!(submit().code, ComposerRejectCode::CapacityExceeded);
        assert_eq!(submit().code, ComposerRejectCode::CapacityExceeded);
        assert_eq!(writes.load(Ordering::SeqCst), 0);
        assert_eq!(
            service
                .inner
                .claude_composer_reconciliation
                .lock()
                .unwrap()
                .pending
                .len(),
            1_024
        );

        service.push_claude_semantic_draft(
            identity,
            SemanticEventDraft {
                stable_session_key: stable_key.clone(),
                occurred_at_epoch_ms: 1_200,
                source: SemanticSource::Claude,
                kind: SemanticEventKind::UserMessage {
                    text: "overflow prompt".to_string(),
                },
                retention: SemanticRetention::Canonical,
                deduplication_key: Some("provider-overflow-claude".to_string()),
            },
        );
        let replay = service.semantic_replay(&stable_key, 0).unwrap();
        assert_eq!(
            replay
                .events
                .iter()
                .filter(|event| matches!(
                    &event.kind,
                    SemanticEventKind::UserMessage { text } if text == "overflow prompt"
                ))
                .count(),
            1,
            "an unreserved provider event remains visible exactly once"
        );
    }

    #[test]
    fn full_codex_reconciliation_queue_rejects_1025th_before_pty_write() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        codex_session(&service, "tab-codex", "session-codex");
        let stable_key = StableSessionKey::from_tab("tab-codex");
        let identity = crate::remote::CodexSemanticIdentity {
            pty_session_id: "session-codex".to_string(),
            stable_session_key: stable_key.clone(),
            registration_generation: 7,
        };
        service.push_codex_adapter_registered(identity.clone());
        for index in 0..1_024 {
            let _ = service.reserve_codex_composer_prompt(
                &format!("reserved-{index}"),
                &identity.pty_session_id,
                &stable_key,
                &format!("reserved prompt {index}"),
            );
        }
        assert_eq!(
            service
                .inner
                .codex_composer_reconciliation
                .lock()
                .unwrap()
                .pending
                .len(),
            1_024
        );
        let generation = build_resume_state(
            &service.inner,
            1,
            "web-client",
            resume_request(
                Some(service.inner.runtime_instance_id.clone()),
                Some(stable_key.clone()),
                "tab-codex",
            ),
            1_000,
        )
        .writer_lease
        .generation;
        let writes = Arc::new(AtomicUsize::new(0));
        let observed_writes = writes.clone();
        service.set_terminal_input_handler(Some(Arc::new(move |_, _| {
            observed_writes.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })));

        let submit = || {
            process_composer_submit(
                &service.inner,
                1,
                "web-client",
                "overflow-codex".to_string(),
                stable_key.clone(),
                "overflow prompt".to_string(),
                Vec::new(),
                generation,
                1_100,
            )
            .unwrap_err()
        };
        assert_eq!(submit().code, ComposerRejectCode::CapacityExceeded);
        assert_eq!(submit().code, ComposerRejectCode::CapacityExceeded);
        assert_eq!(writes.load(Ordering::SeqCst), 0);
        assert_eq!(
            service
                .inner
                .codex_composer_reconciliation
                .lock()
                .unwrap()
                .pending
                .len(),
            1_024
        );

        service.push_codex_semantic_draft(
            identity,
            SemanticEventDraft {
                stable_session_key: stable_key.clone(),
                occurred_at_epoch_ms: 1_200,
                source: SemanticSource::Codex,
                kind: SemanticEventKind::UserMessage {
                    text: "overflow prompt".to_string(),
                },
                retention: SemanticRetention::Canonical,
                deduplication_key: Some("provider-overflow-codex".to_string()),
            },
        );
        let replay = service.semantic_replay(&stable_key, 0).unwrap();
        assert_eq!(
            replay
                .events
                .iter()
                .filter(|event| matches!(
                    &event.kind,
                    SemanticEventKind::UserMessage { text } if text == "overflow prompt"
                ))
                .count(),
            1,
            "an unreserved provider event remains visible exactly once"
        );
    }

    #[test]
    fn missing_adapter_does_not_require_composer_reconciliation() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let stable_key = StableSessionKey::from_tab("tab-a");

        assert_eq!(
            service.reserve_claude_composer_prompt(
                "claude-no-adapter",
                "session-a",
                &stable_key,
                "prompt",
            ),
            ComposerReconciliationReservation::NotNeeded
        );
        assert_eq!(
            service.reserve_codex_composer_prompt(
                "codex-no-adapter",
                "session-a",
                &stable_key,
                "prompt",
            ),
            ComposerReconciliationReservation::NotNeeded
        );
    }

    #[test]
    fn claude_composer_hook_reconciliation_is_one_to_one_and_generation_scoped() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        ai_session(&service, "tab-a", "session-a");
        let stable_key = StableSessionKey::from_tab("tab-a");
        let identity = crate::remote::ClaudeSemanticIdentity {
            pty_session_id: "session-a".to_string(),
            stable_session_key: stable_key.clone(),
            registration_generation: 7,
        };
        service.push_claude_adapter_registered(identity.clone());
        let generation = build_resume_state(
            &service.inner,
            1,
            "web-client",
            resume_request(
                Some(service.inner.runtime_instance_id.clone()),
                Some(stable_key.clone()),
                "tab-a",
            ),
            1_000,
        )
        .writer_lease
        .generation;

        let hook_service = service.clone();
        let hook_identity = identity.clone();
        let hook_number = Arc::new(AtomicUsize::new(0));
        let callback_hook_number = hook_number.clone();
        service.set_terminal_input_handler(Some(Arc::new(move |input, _| {
            if let RemoteTerminalInput::ComposerBatch { text, .. } = input {
                let number = callback_hook_number.fetch_add(1, Ordering::SeqCst) + 1;
                hook_service.push_claude_semantic_draft(
                    hook_identity.clone(),
                    SemanticEventDraft {
                        stable_session_key: hook_identity.stable_session_key.clone(),
                        occurred_at_epoch_ms: 1_100 + number as u64,
                        source: SemanticSource::Claude,
                        kind: SemanticEventKind::UserMessage {
                            text: text.trim_end_matches('\r').to_string(),
                        },
                        retention: SemanticRetention::Canonical,
                        deduplication_key: Some(format!("provider-prompt-{number}")),
                    },
                );
            }
            Ok(())
        })));

        for mutation_id in ["mutation-one", "mutation-two"] {
            process_composer_submit(
                &service.inner,
                1,
                "web-client",
                mutation_id.to_string(),
                stable_key.clone(),
                "same legitimate text".to_string(),
                Vec::new(),
                generation,
                1_100,
            )
            .expect("composer prompt accepted");
        }

        let prompt_count =
            || {
                service
                .semantic_replay(&stable_key, 0)
                .expect("semantic replay")
                .events
                .iter()
                .filter(|event| matches!(
                    &event.kind,
                    SemanticEventKind::UserMessage { text } if text == "same legitimate text"
                ))
                .count()
            };
        assert_eq!(prompt_count(), 2, "each phone prompt appears exactly once");

        service.push_claude_semantic_draft(
            identity.clone(),
            SemanticEventDraft {
                stable_session_key: stable_key.clone(),
                occurred_at_epoch_ms: 1_200,
                source: SemanticSource::Claude,
                kind: SemanticEventKind::UserMessage {
                    text: "same legitimate text".to_string(),
                },
                retention: SemanticRetention::Canonical,
                deduplication_key: Some("provider-prompt-2".to_string()),
            },
        );
        assert_eq!(
            prompt_count(),
            2,
            "official provider retries remain reconciled"
        );

        service.push_claude_semantic_draft(
            crate::remote::ClaudeSemanticIdentity {
                registration_generation: identity.registration_generation + 1,
                ..identity.clone()
            },
            SemanticEventDraft {
                stable_session_key: stable_key.clone(),
                occurred_at_epoch_ms: 1_201,
                source: SemanticSource::Claude,
                kind: SemanticEventKind::UserMessage {
                    text: "same legitimate text".to_string(),
                },
                retention: SemanticRetention::Canonical,
                deduplication_key: Some("different-generation".to_string()),
            },
        );
        assert_eq!(prompt_count(), 3, "a generation mismatch must fail open");

        for occurred_at_epoch_ms in [1_202, 1_203] {
            service.push_claude_semantic_draft(
                identity.clone(),
                SemanticEventDraft {
                    stable_session_key: stable_key.clone(),
                    occurred_at_epoch_ms,
                    source: SemanticSource::Claude,
                    kind: SemanticEventKind::UserMessage {
                        text: "same legitimate text".to_string(),
                    },
                    retention: SemanticRetention::Canonical,
                    deduplication_key: None,
                },
            );
        }
        assert_eq!(prompt_count(), 5, "no-ID provider events remain distinct");
    }

    #[test]
    fn claude_adapter_removal_during_reserved_prompt_keeps_one_user_message() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let stable_key = StableSessionKey::from_tab("tab-a");
        let identity = crate::remote::ClaudeSemanticIdentity {
            pty_session_id: "session-a".to_string(),
            stable_session_key: stable_key.clone(),
            registration_generation: 7,
        };
        service.push_claude_adapter_registered(identity.clone());
        let _ = service.reserve_claude_composer_prompt(
            "mutation-a",
            &identity.pty_session_id,
            &stable_key,
            "inspect the race",
        );

        service.push_claude_semantic_draft(
            identity.clone(),
            SemanticEventDraft {
                stable_session_key: stable_key.clone(),
                occurred_at_epoch_ms: 1_001,
                source: SemanticSource::Claude,
                kind: SemanticEventKind::UserMessage {
                    text: "inspect the race".to_string(),
                },
                retention: SemanticRetention::Canonical,
                deduplication_key: Some("provider-prompt-a".to_string()),
            },
        );
        service.push_claude_adapter_removed(&identity);
        service.push_semantic_draft(SemanticEventDraft {
            stable_session_key: stable_key.clone(),
            occurred_at_epoch_ms: 1_002,
            source: SemanticSource::Claude,
            kind: SemanticEventKind::UserMessage {
                text: "inspect the race".to_string(),
            },
            retention: SemanticRetention::Canonical,
            deduplication_key: Some("composer:mutation-a".to_string()),
        });
        service.accept_claude_composer_prompt("mutation-a");

        let replay = service.semantic_replay(&stable_key, 0).unwrap();
        assert_eq!(
            replay
                .events
                .iter()
                .filter(|event| matches!(
                    &event.kind,
                    SemanticEventKind::UserMessage { text } if text == "inspect the race"
                ))
                .count(),
            1,
            "adapter removal must not flush a hook while its composer write can still succeed"
        );
    }

    #[test]
    fn claude_adapter_replacement_during_reserved_prompt_keeps_one_user_message() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let stable_key = StableSessionKey::from_tab("tab-a");
        let identity = crate::remote::ClaudeSemanticIdentity {
            pty_session_id: "session-a".to_string(),
            stable_session_key: stable_key.clone(),
            registration_generation: 7,
        };
        service.push_claude_adapter_registered(identity.clone());
        let _ = service.reserve_claude_composer_prompt(
            "mutation-a",
            &identity.pty_session_id,
            &stable_key,
            "inspect the race",
        );

        service.push_claude_semantic_draft(
            identity.clone(),
            SemanticEventDraft {
                stable_session_key: stable_key.clone(),
                occurred_at_epoch_ms: 1_001,
                source: SemanticSource::Claude,
                kind: SemanticEventKind::UserMessage {
                    text: "inspect the race".to_string(),
                },
                retention: SemanticRetention::Canonical,
                deduplication_key: Some("provider-prompt-a".to_string()),
            },
        );
        service.push_claude_adapter_registered(crate::remote::ClaudeSemanticIdentity {
            registration_generation: identity.registration_generation + 1,
            ..identity.clone()
        });
        service.push_semantic_draft(SemanticEventDraft {
            stable_session_key: stable_key.clone(),
            occurred_at_epoch_ms: 1_002,
            source: SemanticSource::Claude,
            kind: SemanticEventKind::UserMessage {
                text: "inspect the race".to_string(),
            },
            retention: SemanticRetention::Canonical,
            deduplication_key: Some("composer:mutation-a".to_string()),
        });
        service.accept_claude_composer_prompt("mutation-a");

        let replay = service.semantic_replay(&stable_key, 0).unwrap();
        assert_eq!(
            replay
                .events
                .iter()
                .filter(|event| matches!(
                    &event.kind,
                    SemanticEventKind::UserMessage { text } if text == "inspect the race"
                ))
                .count(),
            1,
            "replacement must not flush an old-generation hook while its write can still succeed"
        );
    }

    #[test]
    fn accepted_claude_prompt_reconciles_late_hook_after_adapter_removal() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let stable_key = StableSessionKey::from_tab("tab-a");
        let identity = crate::remote::ClaudeSemanticIdentity {
            pty_session_id: "session-a".to_string(),
            stable_session_key: stable_key.clone(),
            registration_generation: 7,
        };
        service.push_claude_adapter_registered(identity.clone());
        let _ = service.reserve_claude_composer_prompt(
            "mutation-a",
            &identity.pty_session_id,
            &stable_key,
            "late provider hook",
        );
        service.push_semantic_draft(SemanticEventDraft {
            stable_session_key: stable_key.clone(),
            occurred_at_epoch_ms: 1_001,
            source: SemanticSource::Claude,
            kind: SemanticEventKind::UserMessage {
                text: "late provider hook".to_string(),
            },
            retention: SemanticRetention::Canonical,
            deduplication_key: Some("composer:mutation-a".to_string()),
        });
        service.accept_claude_composer_prompt("mutation-a");
        service.push_claude_adapter_removed(&identity);

        for occurred_at_epoch_ms in [1_002, 1_003] {
            service.push_claude_semantic_draft(
                identity.clone(),
                SemanticEventDraft {
                    stable_session_key: stable_key.clone(),
                    occurred_at_epoch_ms,
                    source: SemanticSource::Claude,
                    kind: SemanticEventKind::UserMessage {
                        text: "late provider hook".to_string(),
                    },
                    retention: SemanticRetention::Canonical,
                    deduplication_key: Some("provider-prompt-a".to_string()),
                },
            );
        }

        let replay = service.semantic_replay(&stable_key, 0).unwrap();
        assert_eq!(
            replay
                .events
                .iter()
                .filter(|event| matches!(
                    &event.kind,
                    SemanticEventKind::UserMessage { text } if text == "late provider hook"
                ))
                .count(),
            1,
            "a late official hook and retry remain reconciled after exact adapter removal"
        );
    }

    #[test]
    fn cancelled_claude_prompt_after_adapter_removal_releases_provider_hook_once() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let stable_key = StableSessionKey::from_tab("tab-a");
        let identity = crate::remote::ClaudeSemanticIdentity {
            pty_session_id: "session-a".to_string(),
            stable_session_key: stable_key.clone(),
            registration_generation: 7,
        };
        service.push_claude_adapter_registered(identity.clone());
        let _ = service.reserve_claude_composer_prompt(
            "mutation-a",
            &identity.pty_session_id,
            &stable_key,
            "provider saw rejected write",
        );
        service.push_claude_semantic_draft(
            identity.clone(),
            SemanticEventDraft {
                stable_session_key: stable_key.clone(),
                occurred_at_epoch_ms: 1_001,
                source: SemanticSource::Claude,
                kind: SemanticEventKind::UserMessage {
                    text: "provider saw rejected write".to_string(),
                },
                retention: SemanticRetention::Canonical,
                deduplication_key: Some("provider-prompt-a".to_string()),
            },
        );
        service.push_claude_adapter_removed(&identity);
        service.cancel_claude_composer_prompt("mutation-a");

        service.push_claude_semantic_draft(
            identity,
            SemanticEventDraft {
                stable_session_key: stable_key.clone(),
                occurred_at_epoch_ms: 1_002,
                source: SemanticSource::Claude,
                kind: SemanticEventKind::UserMessage {
                    text: "provider saw rejected write".to_string(),
                },
                retention: SemanticRetention::Canonical,
                deduplication_key: Some("provider-prompt-a".to_string()),
            },
        );

        let replay = service.semantic_replay(&stable_key, 0).unwrap();
        assert_eq!(
            replay
                .events
                .iter()
                .filter(|event| matches!(
                    &event.kind,
                    SemanticEventKind::UserMessage { text }
                        if text == "provider saw rejected write"
                ))
                .count(),
            1,
            "write rejection must publish the deferred provider hook without duplicating retries"
        );
    }

    #[test]
    fn codex_composer_provider_reconciliation_is_fifo_generation_scoped_and_retry_safe() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        codex_session(&service, "tab-codex", "session-codex");
        let stable_key = StableSessionKey::from_tab("tab-codex");
        let identity = crate::remote::CodexSemanticIdentity {
            pty_session_id: "session-codex".to_string(),
            stable_session_key: stable_key.clone(),
            registration_generation: 7,
        };
        service.push_codex_adapter_registered(identity.clone());
        let generation = build_resume_state(
            &service.inner,
            1,
            "web-client",
            resume_request(
                Some(service.inner.runtime_instance_id.clone()),
                Some(stable_key.clone()),
                "tab-codex",
            ),
            1_000,
        )
        .writer_lease
        .generation;
        let provider_service = service.clone();
        let provider_identity = identity.clone();
        let provider_number = Arc::new(AtomicUsize::new(0));
        let callback_number = provider_number.clone();
        service.set_terminal_input_handler(Some(Arc::new(move |input, _| {
            let RemoteTerminalInput::ComposerBatch { text, .. } = input else {
                return Ok(());
            };
            let number = callback_number.fetch_add(1, Ordering::SeqCst) + 1;
            provider_service.push_codex_semantic_draft(
                provider_identity.clone(),
                SemanticEventDraft {
                    stable_session_key: provider_identity.stable_session_key.clone(),
                    occurred_at_epoch_ms: 1_100 + number as u64,
                    source: SemanticSource::Codex,
                    kind: SemanticEventKind::UserMessage {
                        text: text.trim_end_matches('\r').to_string(),
                    },
                    retention: SemanticRetention::Canonical,
                    deduplication_key: Some(format!("codex:user:item-{number}")),
                },
            );
            Ok(())
        })));

        for mutation_id in ["codex-mutation-1", "codex-mutation-2"] {
            process_composer_submit(
                &service.inner,
                1,
                "web-client",
                mutation_id.to_string(),
                stable_key.clone(),
                "same prompt".to_string(),
                Vec::new(),
                generation,
                1_100,
            )
            .expect("Codex composer prompt accepted");
        }
        let prompt_count = || {
            service
                .semantic_replay(&stable_key, 0)
                .unwrap()
                .events
                .iter()
                .filter(|event| {
                    matches!(
                        &event.kind,
                        SemanticEventKind::UserMessage { text } if text == "same prompt"
                    )
                })
                .count()
        };
        assert_eq!(prompt_count(), 2, "FIFO prompts must each appear once");

        service.push_codex_semantic_draft(
            identity.clone(),
            SemanticEventDraft {
                stable_session_key: stable_key.clone(),
                occurred_at_epoch_ms: 1_200,
                source: SemanticSource::Codex,
                kind: SemanticEventKind::UserMessage {
                    text: "same prompt".to_string(),
                },
                retention: SemanticRetention::Canonical,
                deduplication_key: Some("codex:user:item-2".to_string()),
            },
        );
        assert_eq!(prompt_count(), 2, "official retries remain reconciled");

        service.push_codex_semantic_draft(
            crate::remote::CodexSemanticIdentity {
                registration_generation: 8,
                ..identity.clone()
            },
            SemanticEventDraft {
                stable_session_key: stable_key.clone(),
                occurred_at_epoch_ms: 1_201,
                source: SemanticSource::Codex,
                kind: SemanticEventKind::UserMessage {
                    text: "same prompt".to_string(),
                },
                retention: SemanticRetention::Canonical,
                deduplication_key: Some("codex:user:different-generation".to_string()),
            },
        );
        assert_eq!(prompt_count(), 3, "generation mismatch must fail open");

        service.push_codex_semantic_draft(
            identity,
            SemanticEventDraft {
                stable_session_key: stable_key.clone(),
                occurred_at_epoch_ms: 1_202,
                source: SemanticSource::Codex,
                kind: SemanticEventKind::UserMessage {
                    text: "local TUI prompt".to_string(),
                },
                retention: SemanticRetention::Canonical,
                deduplication_key: Some("codex:user:local-tui".to_string()),
            },
        );
        let replay = service.semantic_replay(&stable_key, 0).unwrap();
        assert!(
            replay.events.iter().any(|event| matches!(
                &event.kind,
                SemanticEventKind::UserMessage { text } if text == "local TUI prompt"
            )),
            "unreserved local-TUI prompts must remain visible"
        );
    }

    #[test]
    fn codex_composer_reconciles_provider_visible_long_text_and_image_markers_once() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        codex_session(&service, "tab-codex", "session-codex");
        let stable_key = StableSessionKey::from_tab("tab-codex");
        let identity = crate::remote::CodexSemanticIdentity {
            pty_session_id: "session-codex".to_string(),
            stable_session_key: stable_key.clone(),
            registration_generation: 17,
        };
        service.push_codex_adapter_registered(identity.clone());
        let generation = build_resume_state(
            &service.inner,
            1,
            "web-client",
            resume_request(
                Some(service.inner.runtime_instance_id.clone()),
                Some(stable_key.clone()),
                "tab-codex",
            ),
            1_000,
        )
        .writer_lease
        .generation;
        let provider_service = service.clone();
        let provider_identity = identity.clone();
        let provider_number = Arc::new(AtomicUsize::new(0));
        let callback_number = provider_number.clone();
        service.set_terminal_input_handler(Some(Arc::new(move |input, _| {
            let RemoteTerminalInput::ComposerBatch {
                text, attachments, ..
            } = input
            else {
                return Ok(());
            };
            let text = text.trim_end_matches('\r');
            let mut provider_parts = vec!["[Image]"; attachments.len()];
            if !text.is_empty() {
                provider_parts.push(text);
            }
            let provider_text = provider_parts.join("\n");
            let provider_text = provider_text[..provider_text.len().min(64 * 1024)].to_string();
            let number = callback_number.fetch_add(1, Ordering::SeqCst) + 1;
            provider_service.push_codex_semantic_draft(
                provider_identity.clone(),
                SemanticEventDraft {
                    stable_session_key: provider_identity.stable_session_key.clone(),
                    occurred_at_epoch_ms: 1_100 + number as u64,
                    source: SemanticSource::Codex,
                    kind: SemanticEventKind::UserMessage {
                        text: provider_text,
                    },
                    retention: SemanticRetention::Canonical,
                    deduplication_key: Some(format!("codex:user:canonical-{number}")),
                },
            );
            Ok(())
        })));

        let long_text = "x".repeat(70 * 1024);
        process_composer_submit(
            &service.inner,
            1,
            "web-client",
            "codex-long-prompt".to_string(),
            stable_key.clone(),
            long_text.clone(),
            Vec::new(),
            generation,
            1_100,
        )
        .expect("long Codex composer prompt accepted");
        process_composer_submit(
            &service.inner,
            1,
            "web-client",
            "codex-image-prompt".to_string(),
            stable_key.clone(),
            "inspect both images".to_string(),
            vec![
                ComposerAttachment {
                    mime_type: "image/png".to_string(),
                    file_name: Some("first.png".to_string()),
                    data_base64: "AQID".to_string(),
                },
                ComposerAttachment {
                    mime_type: "image/jpeg".to_string(),
                    file_name: Some("second.jpg".to_string()),
                    data_base64: "BAUG".to_string(),
                },
            ],
            generation,
            1_101,
        )
        .expect("image Codex composer prompt accepted");

        let replay = service.semantic_replay(&stable_key, 0).unwrap();
        let messages = replay
            .events
            .iter()
            .filter_map(|event| match &event.kind {
                SemanticEventKind::UserMessage { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(messages, ["inspect both images"]);
        assert_eq!(
            replay
                .events
                .iter()
                .filter(|event| matches!(
                    &event.kind,
                    SemanticEventKind::Status { state, .. }
                        if state == "semanticEventTruncated"
                ))
                .count(),
            1,
            "the local oversized prompt may truncate, but its provider echo must be reconciled"
        );

        service.push_codex_semantic_draft(
            identity,
            SemanticEventDraft {
                stable_session_key: stable_key.clone(),
                occurred_at_epoch_ms: 1_200,
                source: SemanticSource::Codex,
                kind: SemanticEventKind::UserMessage {
                    text: "local TUI prompt".to_string(),
                },
                retention: SemanticRetention::Canonical,
                deduplication_key: Some("codex:user:local-tui-canonical-test".to_string()),
            },
        );
        let replay = service.semantic_replay(&stable_key, 0).unwrap();
        assert!(replay.events.iter().any(|event| matches!(
            &event.kind,
            SemanticEventKind::UserMessage { text } if text == "local TUI prompt"
        )));
    }

    #[test]
    fn codex_reconciliation_survives_removal_and_cancel_releases_deferred_provider() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let stable_key = StableSessionKey::from_tab("tab-codex");
        let identity = crate::remote::CodexSemanticIdentity {
            pty_session_id: "session-codex".to_string(),
            stable_session_key: stable_key.clone(),
            registration_generation: 9,
        };
        let provider = |text: &str, key: &str, occurred_at_epoch_ms| SemanticEventDraft {
            stable_session_key: stable_key.clone(),
            occurred_at_epoch_ms,
            source: SemanticSource::Codex,
            kind: SemanticEventKind::UserMessage {
                text: text.to_string(),
            },
            retention: SemanticRetention::Canonical,
            deduplication_key: Some(key.to_string()),
        };
        let composer = |text: &str, mutation: &str, occurred_at_epoch_ms| SemanticEventDraft {
            stable_session_key: stable_key.clone(),
            occurred_at_epoch_ms,
            source: SemanticSource::Codex,
            kind: SemanticEventKind::UserMessage {
                text: text.to_string(),
            },
            retention: SemanticRetention::Canonical,
            deduplication_key: Some(format!("composer:{mutation}")),
        };

        service.push_codex_adapter_registered(identity.clone());
        let _ = service.reserve_codex_composer_prompt(
            "reserved-removal",
            &identity.pty_session_id,
            &stable_key,
            "reserved removal",
        );
        service.push_codex_semantic_draft(
            identity.clone(),
            provider("reserved removal", "codex:user:reserved", 1),
        );
        service.push_codex_adapter_removed(&identity);
        service.push_semantic_draft(composer("reserved removal", "reserved-removal", 2));
        service.accept_codex_composer_prompt("reserved-removal");

        service.push_codex_adapter_registered(identity.clone());
        let _ = service.reserve_codex_composer_prompt(
            "accepted-removal",
            &identity.pty_session_id,
            &stable_key,
            "accepted removal",
        );
        service.push_semantic_draft(composer("accepted removal", "accepted-removal", 3));
        service.accept_codex_composer_prompt("accepted-removal");
        service.push_codex_adapter_removed(&identity);
        for occurred_at_epoch_ms in [4, 5] {
            service.push_codex_semantic_draft(
                identity.clone(),
                provider(
                    "accepted removal",
                    "codex:user:accepted",
                    occurred_at_epoch_ms,
                ),
            );
        }

        service.push_codex_adapter_registered(identity.clone());
        let _ = service.reserve_codex_composer_prompt(
            "cancelled",
            &identity.pty_session_id,
            &stable_key,
            "cancelled write",
        );
        service.push_codex_semantic_draft(
            identity.clone(),
            provider("cancelled write", "codex:user:cancelled", 6),
        );
        service.push_codex_adapter_removed(&identity);
        service.cancel_codex_composer_prompt("cancelled");
        service.push_codex_semantic_draft(
            identity,
            provider("cancelled write", "codex:user:cancelled", 7),
        );

        let replay = service.semantic_replay(&stable_key, 0).unwrap();
        for text in ["reserved removal", "accepted removal", "cancelled write"] {
            assert_eq!(
                replay
                    .events
                    .iter()
                    .filter(|event| matches!(
                        &event.kind,
                        SemanticEventKind::UserMessage { text: actual } if actual == text
                    ))
                    .count(),
                1,
                "{text} must appear exactly once"
            );
        }
    }

    #[test]
    fn rejected_pty_write_releases_a_deferred_claude_prompt_fail_open() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        ai_session(&service, "tab-a", "session-a");
        let stable_key = StableSessionKey::from_tab("tab-a");
        let identity = crate::remote::ClaudeSemanticIdentity {
            pty_session_id: "session-a".to_string(),
            stable_session_key: stable_key.clone(),
            registration_generation: 1,
        };
        service.push_claude_adapter_registered(identity.clone());
        let generation = build_resume_state(
            &service.inner,
            1,
            "web-client",
            resume_request(None, Some(stable_key.clone()), "tab-a"),
            1_000,
        )
        .writer_lease
        .generation;
        let hook_service = service.clone();
        service.set_terminal_input_handler(Some(Arc::new(move |_, _| {
            hook_service.push_claude_semantic_draft(
                identity.clone(),
                SemanticEventDraft {
                    stable_session_key: identity.stable_session_key.clone(),
                    occurred_at_epoch_ms: 1_101,
                    source: SemanticSource::Claude,
                    kind: SemanticEventKind::UserMessage {
                        text: "provider observed it".to_string(),
                    },
                    retention: SemanticRetention::Canonical,
                    deduplication_key: Some("observed-prompt".to_string()),
                },
            );
            Err("synthetic PTY rejection".to_string())
        })));

        process_composer_submit(
            &service.inner,
            1,
            "web-client",
            "rejected-mutation".to_string(),
            stable_key.clone(),
            "provider observed it".to_string(),
            Vec::new(),
            generation,
            1_100,
        )
        .expect_err("PTY rejection remains authoritative");

        let replay = service.semantic_replay(&stable_key, 0).unwrap();
        assert_eq!(
            replay
                .events
                .iter()
                .filter(|event| matches!(
                    &event.kind,
                    SemanticEventKind::UserMessage { text } if text == "provider observed it"
                ))
                .count(),
            1,
            "a deferred provider event must not be lost when the composer write reports failure"
        );
    }

    #[test]
    fn composer_retry_never_replays_attachments_after_the_text_write_fails() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        ai_session(&service, "tab-a", "session-a");
        let current_runtime = service.inner.runtime_instance_id.clone();
        let resume = build_resume_state(
            &service.inner,
            1,
            "web-client",
            resume_request(
                Some(current_runtime),
                Some(StableSessionKey::from_tab("tab-a")),
                "tab-a",
            ),
            1_000,
        );
        let generation = resume.writer_lease.generation;
        let writes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let write_count = writes.clone();
        service.set_terminal_input_handler(Some(Arc::new(move |input, _| {
            write_count.fetch_add(1, Ordering::SeqCst);
            let RemoteTerminalInput::ComposerBatch { .. } = input else {
                panic!("unexpected composer input: {input:?}")
            };
            Err("batch write failed".to_string())
        })));
        let attachments = vec![
            ComposerAttachment {
                mime_type: "image/png".to_string(),
                file_name: Some("first.png".to_string()),
                data_base64: "Zmlyc3Q=".to_string(),
            },
            ComposerAttachment {
                mime_type: "image/png".to_string(),
                file_name: Some("second.png".to_string()),
                data_base64: "c2Vjb25k".to_string(),
            },
        ];
        let submit = || {
            process_composer_submit(
                &service.inner,
                1,
                "web-client",
                "partial-mutation".to_string(),
                StableSessionKey::from_tab("tab-a"),
                "hello".to_string(),
                attachments.clone(),
                generation,
                1_100,
            )
        };

        let first = submit().expect_err("text failure rejects mutation");
        let retry = submit().expect_err("terminal rejection is remembered");

        assert_eq!(first.code, ComposerRejectCode::PtyRejected);
        assert_eq!(retry.code, ComposerRejectCode::PtyRejected);
        assert_eq!(writes.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn blocked_composer_does_not_pin_lease_or_other_session_input() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let mut app = crate::state::AppState::default();
        let mut runtime_state = crate::state::RuntimeState::default();
        for (tab_id, session_id) in [("tab-a", "session-a"), ("tab-b", "session-b")] {
            app.open_tabs.push(crate::models::SessionTab {
                id: tab_id.to_string(),
                tab_type: crate::models::TabType::Claude,
                pty_session_id: Some(session_id.to_string()),
                ..crate::models::SessionTab::default()
            });
            let mut runtime = SessionRuntimeState::new(
                session_id,
                PathBuf::new(),
                SessionDimensions::default(),
                TerminalBackend::default(),
            );
            runtime.session_kind = SessionKind::Claude;
            runtime.tab_id = Some(tab_id.to_string());
            runtime.status = crate::state::SessionStatus::Running;
            runtime_state
                .sessions
                .insert(session_id.to_string(), runtime);
        }
        service.update_snapshot(app, runtime_state, HashMap::new());

        for (connection_id, client_id) in [(1, "client-a"), (2, "client-b")] {
            pair_web_client(&service, client_id);
            let (native, _native_rx) = std_mpsc::channel();
            assert!(register_client(
                &service.inner,
                connection_id,
                client_id,
                native,
                test_web_sender(),
            ));
        }
        let now = now_epoch_ms();
        let first_resume = build_resume_state(
            &service.inner,
            1,
            "client-a",
            resume_request(
                Some(service.inner.runtime_instance_id.clone()),
                Some(StableSessionKey::from_tab("tab-a")),
                "instance-a",
            ),
            now,
        );
        let generation_a = first_resume.writer_lease.generation;

        let entered_a = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        let release_a = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        let (progress_b_tx, progress_b_rx) = std_mpsc::channel();
        let handler_entered = entered_a.clone();
        let handler_release = release_a.clone();
        service.set_terminal_input_handler(Some(Arc::new(move |input, _| {
            let RemoteTerminalInput::ComposerBatch { session_id, .. } = input else {
                panic!("web composer must be one batch input")
            };
            if session_id == "session-a" {
                let (lock, cvar) = &*handler_entered;
                *lock.lock().unwrap() = true;
                cvar.notify_all();
                let (lock, cvar) = &*handler_release;
                let mut released = lock.lock().unwrap();
                while !*released {
                    released = cvar.wait(released).unwrap();
                }
            } else if session_id == "session-b" {
                progress_b_tx.send(()).unwrap();
            }
            Ok(())
        })));

        let (native_a, _native_a_rx) = tokio_mpsc::unbounded_channel();
        let submit_inner = service.inner.clone();
        let (returned_tx, returned_rx) = std_mpsc::channel();
        let submit_a = std::thread::spawn(move || {
            handle_inbound(
                &submit_inner,
                1,
                "client-a",
                WsInbound::ComposerSubmit {
                    mutation_id: "blocked-a".to_string(),
                    stable_session_key: StableSessionKey::from_tab("tab-a"),
                    text: "first".to_string(),
                    attachments: Vec::new(),
                    expected_lease_generation: generation_a,
                },
                &native_a,
            );
            returned_tx.send(()).unwrap();
        });
        {
            let (lock, cvar) = &*entered_a;
            let entered = lock.lock().unwrap();
            let (_entered, timeout) = cvar
                .wait_timeout_while(entered, Duration::from_secs(1), |entered| !*entered)
                .unwrap();
            assert!(!timeout.timed_out(), "first session callback never started");
        }
        let protocol_returned = returned_rx.recv_timeout(Duration::from_millis(100)).is_ok();

        let second_resume = build_resume_state(
            &service.inner,
            2,
            "client-b",
            resume_request(
                Some(service.inner.runtime_instance_id.clone()),
                Some(StableSessionKey::from_tab("tab-b")),
                "instance-b",
            ),
            now + 1_000,
        );
        let generation_b = second_resume.writer_lease.generation;
        let second_acquired = second_resume.writer_lease.you_are_owner;
        let (native_b, _native_b_rx) = tokio_mpsc::unbounded_channel();
        handle_inbound(
            &service.inner,
            2,
            "client-b",
            WsInbound::ComposerSubmit {
                mutation_id: "progress-b".to_string(),
                stable_session_key: StableSessionKey::from_tab("tab-b"),
                text: "second".to_string(),
                attachments: Vec::new(),
                expected_lease_generation: generation_b,
            },
            &native_b,
        );
        let other_session_progressed = progress_b_rx.recv_timeout(Duration::from_secs(1)).is_ok();

        {
            let (lock, cvar) = &*release_a;
            *lock.lock().unwrap() = true;
            cvar.notify_all();
        }
        submit_a.join().unwrap();

        assert!(
            protocol_returned,
            "blocked callback pinned the socket handler"
        );
        assert!(
            second_acquired,
            "blocked callback pinned global writer authority"
        );
        assert!(
            other_session_progressed,
            "blocked callback stalled another session worker"
        );
    }

    #[test]
    fn composer_ack_is_stored_even_if_the_registry_is_poisoned_after_pty_write() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        ai_session(&service, "tab-a", "session-a");
        let current_runtime = service.inner.runtime_instance_id.clone();
        let resume = build_resume_state(
            &service.inner,
            1,
            "web-client",
            resume_request(
                Some(current_runtime),
                Some(StableSessionKey::from_tab("tab-a")),
                "tab-a",
            ),
            1_000,
        );
        let generation = resume.writer_lease.generation;
        let poison_inner = service.inner.clone();
        let poison_once = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let poison_once_handler = poison_once.clone();
        service.set_terminal_input_handler(Some(Arc::new(move |_, _| {
            if !poison_once_handler.swap(true, Ordering::SeqCst) {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let _registry = poison_inner
                        .web_composer_mutations
                        .lock()
                        .expect("registry starts healthy");
                    panic!("poison mutation registry after PTY write");
                }));
            }
            Ok(())
        })));
        let submit = || {
            process_composer_submit(
                &service.inner,
                1,
                "web-client",
                "poisoned-registry-mutation".to_string(),
                StableSessionKey::from_tab("tab-a"),
                "hello".to_string(),
                Vec::new(),
                generation,
                1_100,
            )
        };

        let first = submit().expect("first submit accepted");
        let duplicate = submit().expect("stored ack survives registry poison");

        assert_eq!(first, duplicate);
    }

    #[test]
    fn blocked_composer_does_not_delay_competing_or_native_control() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        ai_session(&service, "tab-a", "session-a");
        let generation = build_resume_state(
            &service.inner,
            1,
            "web-client",
            resume_request(None, Some(StableSessionKey::from_tab("tab-a")), "tab-a"),
            1_000,
        )
        .writer_lease
        .generation;
        let (entered_tx, entered_rx) = std_mpsc::channel();
        let release = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        let callback_release = release.clone();
        let callback_inner = service.inner.clone();
        service.set_terminal_input_handler(Some(Arc::new(move |_, _| {
            // Acquiring takes the operation lock. Reaching this send proves
            // the host does not hold that lock across a PTY callback.
            let competitor =
                acquire_writer_lease(&callback_inner, 2, "other-browser", "tab-b", 5_000);
            entered_tx.send(competitor).expect("callback observer");
            let (lock, cvar) = &*callback_release;
            let mut released = lock.lock().expect("callback gate lock");
            while !*released {
                released = cvar.wait(released).expect("callback gate wait");
            }
            Ok(())
        })));

        let submit_inner = service.inner.clone();
        let submit = std::thread::spawn(move || {
            process_composer_submit(
                &submit_inner,
                1,
                "web-client",
                "busy-mutation".to_string(),
                StableSessionKey::from_tab("tab-a"),
                "hello".to_string(),
                Vec::new(),
                generation,
                1_100,
            )
        });
        let competitor = entered_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("callback entered without holding operation lock");
        assert!(competitor.you_are_owner);
        assert_eq!(
            competitor.owner_client_instance_id.as_deref(),
            Some("tab-b")
        );

        crate::remote::set_native_controller(&service.inner, Some("native-client".to_string()));
        assert_eq!(
            service
                .inner
                .controller_client_id
                .read()
                .expect("controller lock")
                .as_deref(),
            Some("native-client"),
            "native takeover must not wait for a blocked callback"
        );
        let (lock, cvar) = &*release;
        *lock.lock().expect("callback gate lock") = true;
        cvar.notify_all();
        submit
            .join()
            .expect("submit thread")
            .expect("composer accepted");

        assert_eq!(
            service
                .inner
                .controller_client_id
                .read()
                .expect("controller lock")
                .as_deref(),
            Some("native-client")
        );
        assert!(service
            .inner
            .web_control
            .lock()
            .expect("web control lock")
            .writer_leases()
            .peek()
            .is_none());
    }

    #[test]
    fn restart_drain_cleans_web_authority_while_composer_callback_is_blocked() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        ai_session(&service, "tab-a", "session-a");
        let (std_tx, _std_rx) = std_mpsc::channel::<ServerMessage>();
        register_client(&service.inner, 1, "web-client", std_tx, test_web_sender());
        let generation = build_resume_state(
            &service.inner,
            1,
            "web-client",
            resume_request(None, Some(StableSessionKey::from_tab("tab-a")), "tab-a"),
            1_000,
        )
        .writer_lease
        .generation;
        let (entered_tx, entered_rx) = std_mpsc::channel();
        let release = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        let callback_release = release.clone();
        service.set_terminal_input_handler(Some(Arc::new(move |_, _| {
            entered_tx.send(()).expect("callback observer");
            let (lock, cvar) = &*callback_release;
            let mut released = lock.lock().expect("callback gate lock");
            while !*released {
                released = cvar.wait(released).expect("callback gate wait");
            }
            Ok(())
        })));
        let submit_inner = service.inner.clone();
        let submit = std::thread::spawn(move || {
            process_composer_submit(
                &submit_inner,
                1,
                "web-client",
                "restart-mutation".to_string(),
                StableSessionKey::from_tab("tab-a"),
                "hello".to_string(),
                Vec::new(),
                generation,
                1_100,
            )
        });
        entered_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("callback entered");

        crate::remote::drain_web_clients_for_restart(&service.inner);
        assert!(
            service
                .inner
                .web_control
                .lock()
                .expect("web control lock")
                .writer_leases()
                .peek()
                .is_none(),
            "restart cleanup must not wait for a blocked callback"
        );
        assert!(service
            .inner
            .controller_client_id
            .read()
            .expect("controller lock")
            .is_none());

        let (lock, cvar) = &*release;
        *lock.lock().expect("callback gate lock") = true;
        cvar.notify_all();
        submit
            .join()
            .expect("submit thread")
            .expect("composer accepted");
        assert!(service
            .inner
            .web_control
            .lock()
            .expect("web control lock")
            .writer_leases()
            .peek()
            .is_none());
        assert!(service
            .inner
            .controller_client_id
            .read()
            .expect("controller lock")
            .is_none());
    }

    #[test]
    fn pty_panic_becomes_a_stored_terminal_rejection() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        ai_session(&service, "tab-a", "session-a");
        let generation = build_resume_state(
            &service.inner,
            1,
            "web-client",
            resume_request(None, Some(StableSessionKey::from_tab("tab-a")), "tab-a"),
            1_000,
        )
        .writer_lease
        .generation;
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let callback_calls = calls.clone();
        service.set_terminal_input_handler(Some(Arc::new(move |_, _| {
            callback_calls.fetch_add(1, Ordering::SeqCst);
            panic!("synthetic PTY panic")
        })));
        let submit = || {
            process_composer_submit(
                &service.inner,
                1,
                "web-client",
                "panic-mutation".to_string(),
                StableSessionKey::from_tab("tab-a"),
                "hello".to_string(),
                Vec::new(),
                generation,
                1_100,
            )
        };

        let first = submit().expect_err("PTY panic rejects");
        let retry = submit().expect_err("identical retry returns stored rejection");
        assert_eq!(first.code, ComposerRejectCode::PtyRejected);
        assert_eq!(retry.code, ComposerRejectCode::PtyRejected);
        assert_eq!(first.message, "The terminal input handler panicked.");
        assert_eq!(retry.message, first.message);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "retry must not call PTY again"
        );
        assert!(
            acquire_writer_lease(&service.inner, 2, "other", "tab-b", 2_000).you_are_owner,
            "a failed callback must not pin writer authority"
        );
    }

    #[test]
    fn full_composer_registry_preserves_oldest_terminal_outcomes() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        ai_session(&service, "tab-a", "session-a");
        let key = StableSessionKey::from_tab("tab-a");
        let generation = build_resume_state(
            &service.inner,
            1,
            "web-client",
            resume_request(None, Some(key.clone()), "tab-a"),
            1_000,
        )
        .writer_lease
        .generation;
        let accepted_text = "remember accepted";
        let rejected_text = "remember rejected";
        let accepted_fingerprint = stable_hash(&(
            key.as_str(),
            accepted_text,
            &Vec::<ComposerAttachment>::new(),
        ));
        let rejected_fingerprint = stable_hash(&(
            key.as_str(),
            rejected_text,
            &Vec::<ComposerAttachment>::new(),
        ));
        let expected_accepted = ComposerAccepted {
            mutation_id: "oldest-accepted".to_string(),
            stable_session_key: key.clone(),
            accepted_sequence: 41,
            lease_generation: generation,
        };
        {
            let mut records = composer_mutations(&service.inner);
            records.insert(
                "oldest-accepted".to_string(),
                WebComposerMutationRecord {
                    fingerprint: accepted_fingerprint,
                    status: WebComposerMutationStatus::Accepted {
                        stable_session_key: key.clone(),
                        accepted_sequence: expected_accepted.accepted_sequence,
                        lease_generation: generation,
                    },
                },
            );
            records.insert(
                "oldest-rejected".to_string(),
                WebComposerMutationRecord {
                    fingerprint: rejected_fingerprint,
                    status: WebComposerMutationStatus::PtyRejected {
                        message: "remembered PTY rejection".to_string(),
                    },
                },
            );
            for index in 2..MAX_COMPOSER_MUTATION_RECORDS {
                records.insert(
                    format!("terminal-{index}"),
                    WebComposerMutationRecord {
                        fingerprint: index as u64,
                        status: WebComposerMutationStatus::Accepted {
                            stable_session_key: key.clone(),
                            accepted_sequence: index as u64,
                            lease_generation: generation,
                        },
                    },
                );
            }
        }
        let writes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let callback_writes = writes.clone();
        service.set_terminal_input_handler(Some(Arc::new(move |_, _| {
            callback_writes.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })));

        for mutation_id in ["new-at-capacity-a", "new-at-capacity-b"] {
            let capacity = process_composer_submit(
                &service.inner,
                1,
                "web-client",
                mutation_id.to_string(),
                key.clone(),
                "must not write".to_string(),
                Vec::new(),
                generation,
                1_100,
            )
            .expect_err("a new mutation must be rejected at capacity");
            assert_eq!(
                serde_json::to_value(capacity.code).expect("serialize rejection code"),
                serde_json::json!("capacityExceeded")
            );
        }
        assert!(!service
            .inner
            .web_control
            .lock()
            .expect("web control lock")
            .writer_leases()
            .is_busy());

        let accepted_first = process_composer_submit(
            &service.inner,
            1,
            "web-client",
            "oldest-accepted".to_string(),
            key.clone(),
            accepted_text.to_string(),
            Vec::new(),
            generation,
            1_101,
        )
        .expect("oldest accepted result is retained");
        let accepted_retry = process_composer_submit(
            &service.inner,
            1,
            "web-client",
            "oldest-accepted".to_string(),
            key.clone(),
            accepted_text.to_string(),
            Vec::new(),
            generation,
            1_102,
        )
        .expect("oldest accepted retry is retained");
        assert_eq!(accepted_first, expected_accepted);
        assert_eq!(accepted_retry, expected_accepted);

        let rejected_first = process_composer_submit(
            &service.inner,
            1,
            "web-client",
            "oldest-rejected".to_string(),
            key.clone(),
            rejected_text.to_string(),
            Vec::new(),
            generation,
            1_103,
        )
        .expect_err("oldest rejection is retained");
        let rejected_retry = process_composer_submit(
            &service.inner,
            1,
            "web-client",
            "oldest-rejected".to_string(),
            key,
            rejected_text.to_string(),
            Vec::new(),
            generation,
            1_104,
        )
        .expect_err("oldest rejected retry is retained");
        assert_eq!(rejected_first.code, ComposerRejectCode::PtyRejected);
        assert_eq!(rejected_retry.code, rejected_first.code);
        assert_eq!(rejected_first.message, "remembered PTY rejection");
        assert_eq!(rejected_retry.message, rejected_first.message);
        assert_eq!(writes.load(Ordering::SeqCst), 0);
        let records = composer_mutations(&service.inner);
        assert_eq!(records.len(), MAX_COMPOSER_MUTATION_RECORDS);
        assert!(records.contains_key("oldest-accepted"));
        assert!(records.contains_key("oldest-rejected"));
        assert!(!records.contains_key("new-at-capacity-a"));
        assert!(!records.contains_key("new-at-capacity-b"));
    }

    #[test]
    fn full_composer_registry_never_evicts_in_flight_records() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        ai_session(&service, "tab-a", "session-a");
        let key = StableSessionKey::from_tab("tab-a");
        let generation = build_resume_state(
            &service.inner,
            1,
            "web-client",
            resume_request(None, Some(key.clone()), "tab-a"),
            1_000,
        )
        .writer_lease
        .generation;
        {
            let mut records = composer_mutations(&service.inner);
            for index in 0..MAX_COMPOSER_MUTATION_RECORDS {
                records.insert(
                    format!("in-flight-{index}"),
                    WebComposerMutationRecord {
                        fingerprint: index as u64,
                        status: WebComposerMutationStatus::InFlight,
                    },
                );
            }
        }
        let writes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let callback_writes = writes.clone();
        service.set_terminal_input_handler(Some(Arc::new(move |_, _| {
            callback_writes.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })));

        let capacity = process_composer_submit(
            &service.inner,
            1,
            "web-client",
            "new-terminal-record".to_string(),
            key,
            "must not write".to_string(),
            Vec::new(),
            generation,
            1_100,
        )
        .expect_err("a full in-flight registry rejects new IDs");
        assert_eq!(
            serde_json::to_value(capacity.code).expect("serialize rejection code"),
            serde_json::json!("capacityExceeded")
        );
        assert_eq!(writes.load(Ordering::SeqCst), 0);
        assert!(!service
            .inner
            .web_control
            .lock()
            .expect("web control lock")
            .writer_leases()
            .is_busy());
        let records = composer_mutations(&service.inner);
        assert_eq!(records.len(), MAX_COMPOSER_MUTATION_RECORDS);
        assert!(records.contains_key("in-flight-0"));
        assert!(!records.contains_key("new-terminal-record"));
    }

    #[test]
    fn orphaned_legacy_busy_marker_does_not_block_keyed_composer() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        ai_session(&service, "tab-a", "session-a");
        let key = StableSessionKey::from_tab("tab-a");
        let generation = build_resume_state(
            &service.inner,
            1,
            "web-client",
            resume_request(None, Some(key.clone()), "tab-a"),
            1_000,
        )
        .writer_lease
        .generation;
        let started = service
            .inner
            .web_control
            .lock()
            .expect("web control lock")
            .writer_leases_mut()
            .begin_mutation(1, "web-client", generation, "busy-gap", 1_050)
            .expect("synthetic in-flight marker");
        assert!(matches!(started, MutationBegin::Started(_)));
        let writes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let callback_writes = writes.clone();
        service.set_terminal_input_handler(Some(Arc::new(move |_, _| {
            callback_writes.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })));

        let accepted = process_composer_submit(
            &service.inner,
            1,
            "web-client",
            "busy-gap".to_string(),
            key,
            "execute on keyed worker".to_string(),
            Vec::new(),
            generation,
            1_100,
        )
        .expect("the keyed executor no longer depends on a global busy marker");
        assert_eq!(accepted.mutation_id, "busy-gap");
        assert_eq!(writes.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn concurrent_same_composer_mutation_writes_exactly_once() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        ai_session(&service, "tab-a", "session-a");
        let key = StableSessionKey::from_tab("tab-a");
        let generation = build_resume_state(
            &service.inner,
            1,
            "web-client",
            resume_request(None, Some(key.clone()), "tab-a"),
            1_000,
        )
        .writer_lease
        .generation;
        let writes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let callback_writes = writes.clone();
        let (entered_tx, entered_rx) = std_mpsc::channel();
        let release = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        let callback_release = release.clone();
        service.set_terminal_input_handler(Some(Arc::new(move |_, _| {
            let call = callback_writes.fetch_add(1, Ordering::SeqCst);
            if call == 0 {
                entered_tx.send(()).expect("first callback observer");
                let (lock, cvar) = &*callback_release;
                let mut released = lock.lock().expect("callback gate lock");
                while !*released {
                    released = cvar.wait(released).expect("callback gate wait");
                }
            }
            Ok(())
        })));
        let first_inner = service.inner.clone();
        let first_key = key.clone();
        let first = std::thread::spawn(move || {
            process_composer_submit(
                &first_inner,
                1,
                "web-client",
                "concurrent-id".to_string(),
                first_key,
                "write once".to_string(),
                Vec::new(),
                generation,
                1_100,
            )
        });
        entered_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("first callback entered");

        let duplicate = process_composer_submit(
            &service.inner,
            1,
            "web-client",
            "concurrent-id".to_string(),
            key,
            "write once".to_string(),
            Vec::new(),
            generation,
            1_101,
        );
        let (lock, cvar) = &*release;
        *lock.lock().expect("callback gate lock") = true;
        cvar.notify_all();
        first
            .join()
            .expect("first submit thread")
            .expect("first submit accepted");
        let duplicate = duplicate.expect_err("concurrent duplicate remains in flight");
        assert_eq!(duplicate.code, ComposerRejectCode::MutationInFlight);
        assert_eq!(writes.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn composer_terminal_errors_remain_bounded() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        ai_session(&service, "tab-a", "session-a");
        let key = StableSessionKey::from_tab("tab-a");
        let generation = build_resume_state(
            &service.inner,
            1,
            "web-client",
            resume_request(None, Some(key.clone()), "tab-a"),
            1_000,
        )
        .writer_lease
        .generation;
        service.set_terminal_input_handler(Some(Arc::new(|_, _| Err("é".repeat(2_000)))));
        let rejected = process_composer_submit(
            &service.inner,
            1,
            "web-client",
            "bounded-error".to_string(),
            key,
            "hello".to_string(),
            Vec::new(),
            generation,
            1_100,
        )
        .expect_err("PTY failure");
        assert!(rejected.message.len() <= MAX_COMPOSER_ERROR_BYTES);
        assert!(rejected.message.is_char_boundary(rejected.message.len()));
        assert_eq!(composer_mutations(&service.inner).len(), 1);
    }

    #[test]
    fn composer_payload_bounds_reject_without_pty_writes_or_unbounded_echo() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        ai_session(&service, "tab-a", "session-a");
        let generation = build_resume_state(
            &service.inner,
            1,
            "web-client",
            resume_request(None, Some(StableSessionKey::from_tab("tab-a")), "tab-a"),
            1_000,
        )
        .writer_lease
        .generation;
        let writes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let callback_writes = writes.clone();
        service.set_terminal_input_handler(Some(Arc::new(move |_, _| {
            callback_writes.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })));

        let oversized_id = "x".repeat(MAX_COMPOSER_MUTATION_ID_BYTES + 100);
        let id_rejection = process_composer_submit(
            &service.inner,
            1,
            "web-client",
            oversized_id,
            StableSessionKey::from_tab("tab-a"),
            "hello".to_string(),
            Vec::new(),
            generation,
            1_100,
        )
        .expect_err("oversized mutation ID");
        assert!(id_rejection.mutation_id.len() <= MAX_COMPOSER_MUTATION_ID_BYTES);

        let attachment_rejection = process_composer_submit(
            &service.inner,
            1,
            "web-client",
            "bad-attachments".to_string(),
            StableSessionKey::from_tab("tab-a"),
            String::new(),
            vec![
                ComposerAttachment {
                    mime_type: "image/gif".to_string(),
                    file_name: Some("x".repeat(MAX_COMPOSER_FILE_NAME_BYTES + 1)),
                    data_base64: "AQID".to_string(),
                };
                MAX_COMPOSER_ATTACHMENTS + 1
            ],
            generation,
            1_101,
        )
        .expect_err("bounded attachments");
        assert_eq!(
            attachment_rejection.code,
            ComposerRejectCode::InvalidRequest
        );
        assert_eq!(writes.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn composer_attachment_decoder_enforces_type_name_size_count_and_total() {
        use super::super::image_paste::WEB_PASTE_IMAGE_MAX_BYTES;

        let attachment =
            |mime_type: &str, file_name: Option<String>, data_base64: String| ComposerAttachment {
                mime_type: mime_type.to_string(),
                file_name,
                data_base64,
            };
        assert!(
            decode_composer_attachments(&[attachment("image/gif", None, "AQID".to_string(),)])
                .is_err()
        );
        assert!(decode_composer_attachments(&[attachment(
            "image/png",
            Some("x".repeat(MAX_COMPOSER_FILE_NAME_BYTES + 1)),
            "AQID".to_string(),
        )])
        .is_err());
        let max_encoded_bytes = WEB_PASTE_IMAGE_MAX_BYTES.div_ceil(3) * 4 + 4;
        assert!(decode_composer_attachments(&[attachment(
            "image/png",
            None,
            "A".repeat(max_encoded_bytes + 1),
        )])
        .is_err());
        assert!(decode_composer_attachments(&vec![
            attachment(
                "image/png",
                None,
                "AQID".to_string()
            );
            MAX_COMPOSER_ATTACHMENTS + 1
        ])
        .is_err());

        let each = MAX_COMPOSER_ATTACHMENT_TOTAL_BYTES / MAX_COMPOSER_ATTACHMENTS + 1;
        let encoded = BASE64.encode(vec![0_u8; each]);
        let total = vec![attachment("image/png", None, encoded); MAX_COMPOSER_ATTACHMENTS];
        assert!(decode_composer_attachments(&total).is_err());
    }

    #[test]
    fn stale_generation_and_mutation_conflicts_never_write_to_the_pty() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        ai_session(&service, "tab-a", "session-a");
        let current_runtime = service.inner.runtime_instance_id.clone();
        let resume = build_resume_state(
            &service.inner,
            1,
            "web-client",
            resume_request(
                Some(current_runtime),
                Some(StableSessionKey::from_tab("tab-a")),
                "tab-a",
            ),
            1_000,
        );
        let generation = resume.writer_lease.generation;
        let writes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let write_count = writes.clone();
        service.set_terminal_input_handler(Some(Arc::new(move |_, _| {
            write_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })));

        let accepted = process_composer_submit(
            &service.inner,
            1,
            "web-client",
            "mutation-1".to_string(),
            StableSessionKey::from_tab("tab-a"),
            "first".to_string(),
            Vec::new(),
            generation,
            1_100,
        )
        .expect("first accepted");
        assert!(accepted.accepted_sequence > 0);
        let conflict = process_composer_submit(
            &service.inner,
            1,
            "web-client",
            "mutation-1".to_string(),
            StableSessionKey::from_tab("tab-a"),
            "different".to_string(),
            Vec::new(),
            generation,
            1_101,
        )
        .expect_err("mutation ID conflict rejected");
        assert_eq!(conflict.code, ComposerRejectCode::MutationConflict);
        let stale = process_composer_submit(
            &service.inner,
            1,
            "web-client",
            "mutation-2".to_string(),
            StableSessionKey::from_tab("tab-a"),
            "stale".to_string(),
            Vec::new(),
            generation.saturating_sub(1),
            1_102,
        )
        .expect_err("stale generation rejected");
        assert_eq!(stale.code, ComposerRejectCode::StaleGeneration);
        assert_eq!(writes.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn expired_composer_lease_clears_controller_and_allows_reacquire() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        ai_session(&service, "tab-a", "session-a");
        let current_runtime = service.inner.runtime_instance_id.clone();
        let first = build_resume_state(
            &service.inner,
            1,
            "web-client-a",
            resume_request(
                Some(current_runtime.clone()),
                Some(StableSessionKey::from_tab("tab-a")),
                "tab-a",
            ),
            1_000,
        );

        let rejected = process_composer_submit(
            &service.inner,
            1,
            "web-client-a",
            "expired-mutation".to_string(),
            StableSessionKey::from_tab("tab-a"),
            "hello".to_string(),
            Vec::new(),
            first.writer_lease.generation,
            9_001,
        )
        .expect_err("expired generation rejected");
        assert_eq!(rejected.code, ComposerRejectCode::StaleGeneration);
        assert!(service
            .inner
            .controller_client_id
            .read()
            .expect("controller lock")
            .is_none());

        let second = build_resume_state(
            &service.inner,
            2,
            "web-client-b",
            resume_request(Some(current_runtime), None, "tab-b"),
            9_002,
        );
        assert!(second.writer_lease.you_are_owner);
    }

    #[test]
    fn expired_visibility_update_clears_controller_and_allows_reacquire() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let current_runtime = service.inner.runtime_instance_id.clone();
        let first = build_resume_state(
            &service.inner,
            1,
            "web-client-a",
            resume_request(Some(current_runtime.clone()), None, "tab-a"),
            1_000,
        );
        assert!(first.writer_lease.you_are_owner);

        let expired =
            set_writer_visibility(&service.inner, 1, "web-client-a", "tab-a", false, 9_001);
        assert!(!expired.you_are_owner);
        assert!(service
            .inner
            .controller_client_id
            .read()
            .expect("controller lock")
            .is_none());

        let second = build_resume_state(
            &service.inner,
            2,
            "web-client-b",
            resume_request(Some(current_runtime), None, "tab-b"),
            9_002,
        );
        assert!(second.writer_lease.you_are_owner);
    }

    #[test]
    fn binary_frame_layout_is_stable() {
        let frame = encode_session_output_frame("srv-1", 42, b"hello");
        assert_eq!(frame[0], BINARY_FRAME_SESSION_OUTPUT);
        let id_len = u32::from_be_bytes([frame[1], frame[2], frame[3], frame[4]]);
        assert_eq!(id_len, 5);
        assert_eq!(&frame[5..10], b"srv-1");
        let seq = u64::from_be_bytes([
            frame[10], frame[11], frame[12], frame[13], frame[14], frame[15], frame[16], frame[17],
        ]);
        assert_eq!(seq, 42);
        assert_eq!(&frame[18..], b"hello");
    }

    #[test]
    fn initial_web_hello_carries_the_validated_bundle_build_id() {
        let service = RemoteHostService::new(RemoteHostConfig {
            server_id: "host-1".to_string(),
            ..RemoteHostConfig::default()
        });
        let snapshot = light_snapshot(&service.inner, "web-client");
        let hello = initial_web_hello("web-client", &snapshot);
        let serialized = serde_json::to_value(&hello).expect("hello serializes");
        assert_eq!(serialized["type"], "hello");
        assert_eq!(serialized["webBuildId"], WEB_BUILD_ID);

        let WsOutbound::Hello {
            client_id,
            server_id,
            protocol_version,
            web_build_id,
        } = hello
        else {
            panic!("initial frame must be web hello");
        };
        assert_eq!(client_id, "web-client");
        assert_eq!(server_id, "host-1");
        assert_eq!(protocol_version, super::super::dto::WEB_PROTOCOL_VERSION);
        assert_eq!(
            web_build_id,
            include_str!("../../../web/bundle/source-fingerprint.txt").trim()
        );
    }

    #[test]
    fn initial_browser_queue_places_hello_before_snapshot() {
        let service = RemoteHostService::new(RemoteHostConfig {
            server_id: "host-first-frame".to_string(),
            ..RemoteHostConfig::default()
        });
        let (sender, mut receiver) = test_web_channel();

        queue_initial_browser_hello(&sender, &service.inner, "web-client")
            .expect("initial hello queues before registration");
        pair_web_client(&service, "web-client");
        let (native, _native_rx) = std_mpsc::channel();
        assert!(register_client(
            &service.inner,
            1,
            "web-client",
            native,
            sender.clone(),
        ));
        sender
            .try_send_server_message(
                &ServerMessage::Delta {
                    delta: super::super::super::RemoteWorkspaceDelta::default(),
                },
                &service.inner,
                1,
                "web-client",
            )
            .expect("concurrent broadcaster frame queues");
        queue_initial_browser_snapshot(&sender, &service.inner, 1, "web-client")
            .expect("initial snapshot queues");

        let hello = try_recv_web_json(&mut receiver);
        let broadcaster = try_recv_web_json(&mut receiver);
        let snapshot = try_recv_web_json(&mut receiver);
        assert_eq!(hello["type"], "hello");
        assert_eq!(hello["webBuildId"], WEB_BUILD_ID);
        assert_eq!(broadcaster["type"], "snapshot");
        assert_eq!(snapshot["type"], "snapshot");
    }

    #[test]
    fn translate_outbound_projects_snapshot_as_safe_text() {
        use super::super::super::RemoteWorkspaceSnapshot;
        let service = RemoteHostService::new(RemoteHostConfig {
            server_id: "host-1".to_string(),
            ..RemoteHostConfig::default()
        });
        let snapshot = RemoteWorkspaceSnapshot::default();
        let frame = translate_outbound(
            &ServerMessage::Snapshot { snapshot },
            &service.inner,
            1,
            "web-client",
        )
        .expect("snapshot encodes");
        match frame {
            EncodedFrame::Text(text) => {
                let value: serde_json::Value = serde_json::from_str(&text).expect("valid json");
                assert_eq!(value["type"], "snapshot");
                assert_eq!(
                    value["workspace"]["webProtocolVersion"],
                    serde_json::json!(WEB_PROTOCOL_VERSION)
                );
                assert_eq!(value["workspace"]["serverId"], "host-1");
                assert!(value["workspace"]["runtimeInstanceId"]
                    .as_str()
                    .is_some_and(|id| id.starts_with("runtime-")));
                assert!(value["workspace"].get("appState").is_none());
                assert!(value["workspace"].get("runtimeState").is_none());
            }
            EncodedFrame::Binary(_) => panic!("snapshot should be text"),
        }
    }

    #[test]
    fn queued_snapshot_captures_current_state_and_revision_together() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let mut old_app = crate::state::AppState::default();
        old_app.config.projects.push(crate::models::Project {
            id: "old-project".to_string(),
            name: "Old Project".to_string(),
            ..Default::default()
        });
        service.update_snapshot(
            old_app,
            crate::state::RuntimeState::default(),
            HashMap::new(),
        );
        let queued = ServerMessage::Snapshot {
            snapshot: light_snapshot(&service.inner, "web-client"),
        };

        let mut current_app = crate::state::AppState::default();
        current_app.config.projects.push(crate::models::Project {
            id: "current-project".to_string(),
            name: "Current Project".to_string(),
            ..Default::default()
        });
        service.update_snapshot(
            current_app,
            crate::state::RuntimeState::default(),
            HashMap::new(),
        );
        let expected_revision = service.inner.snapshot_revision.load(Ordering::Relaxed);

        let frame = translate_outbound(&queued, &service.inner, 1, "web-client")
            .expect("queued snapshot encodes");
        let EncodedFrame::Text(text) = frame else {
            panic!("snapshot should be text");
        };
        let value: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
        assert_eq!(value["workspace"]["revision"], expected_revision);
        assert_eq!(value["workspace"]["projects"][0]["id"], "current-project");
    }

    #[test]
    fn translate_outbound_emits_a_full_safe_snapshot_for_native_deltas() {
        use super::super::super::RemoteWorkspaceDelta;
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let mut app = crate::state::AppState::default();
        app.config.settings.github_token = Some("TOKEN_SENTINEL".to_string());
        service.update_snapshot(app, crate::state::RuntimeState::default(), HashMap::new());

        let frame = translate_outbound(
            &ServerMessage::Delta {
                delta: RemoteWorkspaceDelta::default(),
            },
            &service.inner,
            1,
            "web-client",
        )
        .expect("delta projects");
        let EncodedFrame::Text(text) = frame else {
            panic!("workspace update should be text");
        };
        let value: serde_json::Value = serde_json::from_str(&text).expect("valid json");
        assert_eq!(value["type"], "snapshot");
        assert_eq!(value["workspace"]["revision"], 2);
        assert!(!text.contains("TOKEN_SENTINEL"));
    }

    #[test]
    fn allowed_ai_response_never_serializes_native_session_runtime() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let mut runtime = SessionRuntimeState::new(
            "session-1",
            PathBuf::from("C:\\Code\\project"),
            SessionDimensions::default(),
            TerminalBackend::default(),
        );
        runtime.session_kind = SessionKind::Claude;
        runtime.ai_launch = Some(AiLaunchSpec {
            tab_id: "tab-1".to_string(),
            project_id: "project-1".to_string(),
            tool: SessionKind::Claude,
            cwd: PathBuf::from("C:\\Code\\project"),
            shell_program: "pwsh".to_string(),
            shell_args: vec!["RUNTIME_ARG_SENTINEL".to_string()],
            startup_command: "STARTUP_SENTINEL".to_string(),
        });
        let message = ServerMessage::Response {
            request_id: 41,
            result: RemoteActionResult::ok(
                None::<String>,
                Some(RemoteActionPayload::AiTab {
                    tab_id: "tab-1".to_string(),
                    project_id: "project-1".to_string(),
                    tab_type: crate::models::TabType::Claude,
                    session_id: "session-1".to_string(),
                    label: Some("Claude".to_string()),
                    session_view: Some(TerminalSessionView {
                        runtime,
                        screen: TerminalScreenSnapshot::default(),
                    }),
                }),
            ),
        };

        let frame = translate_outbound(&message, &service.inner, 1, "web-client")
            .expect("AI response encodes");
        let EncodedFrame::Text(text) = frame else {
            panic!("AI response should be text");
        };
        assert!(!text.contains("STARTUP_SENTINEL"), "leaked startup: {text}");
        assert!(
            !text.contains("RUNTIME_ARG_SENTINEL"),
            "leaked runtime args: {text}"
        );

        let value: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
        assert_eq!(value["type"], "response");
        assert_eq!(value["id"], 41);
        assert_eq!(value["result"]["payload"]["type"], "aiTab");
        assert_eq!(value["result"]["payload"]["tabId"], "tab-1");
        assert_eq!(value["result"]["payload"]["sessionId"], "session-1");
        assert!(value["result"]["payload"].get("sessionView").is_none());
    }

    #[test]
    fn encode_outbound_drops_hello_ok() {
        use super::super::super::RemoteWorkspaceSnapshot;
        let message = ServerMessage::HelloOk {
            protocol_version: PROTOCOL_VERSION,
            server_id: String::new(),
            certificate_fingerprint: String::new(),
            client_id: String::new(),
            client_token: String::new(),
            controller_client_id: None,
            you_have_control: false,
            snapshot: RemoteWorkspaceSnapshot::default(),
        };
        assert!(encode_outbound(&message).is_none());
    }

    #[test]
    fn native_remote_action_start_server_wire_shape_stays_unchanged() {
        // `RemoteAction` has `rename_all = "camelCase"` on the enum (which
        // only affects the `type` tag) but no `rename_all_fields`. Keep this
        // regression assertion for the separate native MessagePack protocol;
        // browser action input now goes through `WebAction`.
        use crate::remote::RemoteAction;
        use crate::state::SessionDimensions;
        let action = RemoteAction::StartServer {
            command_id: "cmd-1".to_string(),
            focus: true,
            dimensions: SessionDimensions {
                cols: 80,
                rows: 24,
                cell_width: 10,
                cell_height: 20,
            },
        };
        let json = serde_json::to_string(&action).unwrap();
        assert!(json.contains("\"type\":\"startServer\""));
        assert!(json.contains("\"command_id\":\"cmd-1\""));
        assert!(json.contains("\"cell_width\":10"));
    }

    #[test]
    fn encode_outbound_handles_session_output_as_binary() {
        let message = ServerMessage::SessionStream {
            event: RemoteSessionStreamEvent::Output {
                session_id: "abc".to_string(),
                chunk_seq: 7,
                emitted_at_epoch_ms: 0,
                bytes: vec![1, 2, 3],
            },
        };
        let frame = encode_outbound(&message).expect("binary frame");
        match frame {
            EncodedFrame::Binary(bytes) => {
                assert_eq!(bytes[0], BINARY_FRAME_SESSION_OUTPUT);
            }
            EncodedFrame::Text(_) => panic!("session output should be binary"),
        }
    }

    #[test]
    fn encode_outbound_bootstrap_carries_screen_snapshot() {
        let message = ServerMessage::SessionStream {
            event: RemoteSessionStreamEvent::Bootstrap {
                bootstrap: RemoteSessionBootstrap {
                    session_id: "alpha".to_string(),
                    runtime: SessionRuntimeState::new(
                        "alpha".to_string(),
                        PathBuf::from("C:\\Code"),
                        SessionDimensions::default(),
                        TerminalBackend::default(),
                    ),
                    screen: TerminalScreenSnapshot {
                        lines: vec![vec![crate::terminal::session::TerminalCellSnapshot {
                            character: 'A',
                            zero_width: Vec::new(),
                            foreground: 0xffffff,
                            background: 0,
                            bold: false,
                            dim: false,
                            italic: false,
                            underline: false,
                            undercurl: false,
                            strike: false,
                            hidden: false,
                            has_hyperlink: false,
                            default_background: true,
                        }]],
                        rows: 1,
                        cols: 1,
                        ..TerminalScreenSnapshot::default()
                    },
                    replay_bytes: b"boot".to_vec(),
                },
            },
        };
        let frame = encode_outbound(&message).expect("bootstrap text");
        let EncodedFrame::Text(text) = frame else {
            panic!("bootstrap should be text");
        };
        let value: serde_json::Value = serde_json::from_str(&text).expect("valid json");
        assert_eq!(value["type"], "sessionBootstrap");
        assert_eq!(value["screen"]["lines"][0][0]["character"], "A");
        assert_eq!(value["replayBase64"], "");
    }

    #[test]
    fn encode_outbound_bootstrap_keeps_replay_as_screenless_fallback() {
        let message = ServerMessage::SessionStream {
            event: RemoteSessionStreamEvent::Bootstrap {
                bootstrap: RemoteSessionBootstrap {
                    session_id: "alpha".to_string(),
                    runtime: SessionRuntimeState::new(
                        "alpha".to_string(),
                        PathBuf::from("C:\\Code"),
                        SessionDimensions::default(),
                        TerminalBackend::default(),
                    ),
                    screen: TerminalScreenSnapshot::default(),
                    replay_bytes: b"boot".to_vec(),
                },
            },
        };
        let frame = encode_outbound(&message).expect("bootstrap text");
        let EncodedFrame::Text(text) = frame else {
            panic!("bootstrap should be text");
        };
        let value: serde_json::Value = serde_json::from_str(&text).expect("valid json");
        assert_eq!(value["replayBase64"], "Ym9vdA==");
    }

    #[test]
    fn subscribe_sessions_eagerly_bootstraps_ready_sessions() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        service.set_session_bootstrap_provider(Some(Arc::new(|session_id| {
            Some(RemoteSessionBootstrap {
                session_id: session_id.to_string(),
                runtime: SessionRuntimeState::new(
                    session_id.to_string(),
                    PathBuf::from("C:\\Code"),
                    SessionDimensions::default(),
                    TerminalBackend::default(),
                ),
                screen: TerminalScreenSnapshot::default(),
                replay_bytes: b"boot".to_vec(),
            })
        })));

        let connection_id = 7;
        let client_id = "web-client";
        pair_web_client(&service, client_id);
        let (std_tx, _std_rx) = std_mpsc::channel::<ServerMessage>();
        register_client(
            &service.inner,
            connection_id,
            client_id,
            std_tx,
            test_web_sender(),
        );

        let (tokio_tx, mut tokio_rx) = tokio_mpsc::unbounded_channel::<ServerMessage>();
        handle_inbound(
            &service.inner,
            connection_id,
            client_id,
            WsInbound::SubscribeSessions {
                session_ids: vec!["alpha".to_string()],
            },
            &tokio_tx,
        );

        match tokio_rx.try_recv().expect("bootstrap frame") {
            ServerMessage::SessionStream {
                event: RemoteSessionStreamEvent::Bootstrap { bootstrap },
            } => {
                assert_eq!(bootstrap.session_id, "alpha");
                assert_eq!(bootstrap.replay_bytes, b"boot".to_vec());
            }
            other => panic!("unexpected message: {other:?}"),
        }

        let clients = service.inner.clients.lock().expect("clients lock");
        let client = clients.get(&connection_id).expect("registered client");
        assert!(client.subscribed_session_ids.contains("alpha"));
        assert!(client.bootstrapped_session_ids.contains("alpha"));
    }

    #[test]
    fn claim_control_if_available_claims_unowned_controller() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let connection_id = 16;
        let client_id = "web-client";
        pair_web_client(&service, client_id);
        let (std_tx, _std_rx) = std_mpsc::channel::<ServerMessage>();
        register_client(
            &service.inner,
            connection_id,
            client_id,
            std_tx,
            test_web_sender(),
        );

        let (tokio_tx, _tokio_rx) = tokio_mpsc::unbounded_channel::<ServerMessage>();
        handle_inbound(
            &service.inner,
            connection_id,
            client_id,
            WsInbound::ClaimControlIfAvailable,
            &tokio_tx,
        );

        assert_eq!(
            service
                .inner
                .controller_client_id
                .read()
                .expect("controller lock")
                .as_deref(),
            Some(client_id)
        );
    }

    #[test]
    fn claim_control_if_available_does_not_steal_controller() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let connection_id = 17;
        let client_id = "web-client";
        pair_web_client(&service, client_id);
        let (std_tx, _std_rx) = std_mpsc::channel::<ServerMessage>();
        register_client(
            &service.inner,
            connection_id,
            client_id,
            std_tx,
            test_web_sender(),
        );
        *service
            .inner
            .controller_client_id
            .write()
            .expect("controller lock") = Some("desktop-client".to_string());

        let (tokio_tx, _tokio_rx) = tokio_mpsc::unbounded_channel::<ServerMessage>();
        handle_inbound(
            &service.inner,
            connection_id,
            client_id,
            WsInbound::ClaimControlIfAvailable,
            &tokio_tx,
        );

        assert_eq!(
            service
                .inner
                .controller_client_id
                .read()
                .expect("controller lock")
                .as_deref(),
            Some("desktop-client")
        );
    }

    #[test]
    fn request_frames_queue_host_requests_instead_of_disconnect() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let connection_id = 9;
        let client_id = "web-client";
        pair_web_client(&service, client_id);
        let (std_tx, _std_rx) = std_mpsc::channel::<ServerMessage>();
        register_client(
            &service.inner,
            connection_id,
            client_id,
            std_tx,
            test_web_sender(),
        );
        claim_legacy_control(&service.inner, connection_id, client_id, false);

        let (tokio_tx, mut tokio_rx) = tokio_mpsc::unbounded_channel::<ServerMessage>();
        handle_inbound(
            &service.inner,
            connection_id,
            client_id,
            WsInbound::Request {
                id: 17,
                action: WebAction::LaunchAi {
                    project_id: "project-1".to_string(),
                    tab_type: WebAiKind::Claude,
                },
                expected_lease_generation: None,
            },
            &tokio_tx,
        );

        assert!(
            tokio_rx.try_recv().is_err(),
            "request should not disconnect"
        );

        let requests = service
            .inner
            .pending_requests
            .lock()
            .expect("pending requests lock");
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].client_id, client_id);
        assert!(
            requests[0].response.is_some(),
            "request must carry response channel"
        );
    }

    #[test]
    fn viewer_mode_requests_get_error_responses_without_disconnect() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let connection_id = 10;
        let client_id = "web-viewer";
        pair_web_client(&service, client_id);
        let (std_tx, _std_rx) = std_mpsc::channel::<ServerMessage>();
        register_client(
            &service.inner,
            connection_id,
            client_id,
            std_tx,
            test_web_sender(),
        );

        let (tokio_tx, mut tokio_rx) = tokio_mpsc::unbounded_channel::<ServerMessage>();
        handle_inbound(
            &service.inner,
            connection_id,
            client_id,
            WsInbound::Request {
                id: 23,
                action: WebAction::LaunchAi {
                    project_id: "project-1".to_string(),
                    tab_type: WebAiKind::Claude,
                },
                expected_lease_generation: None,
            },
            &tokio_tx,
        );

        match tokio_rx.try_recv().expect("response frame") {
            ServerMessage::Response { request_id, result } => {
                assert_eq!(request_id, 23);
                assert!(!result.ok);
                assert_eq!(
                    result.message.as_deref(),
                    Some("This client is in viewer mode. Take control first."),
                );
            }
            other => panic!("unexpected message: {other:?}"),
        }

        let requests = service
            .inner
            .pending_requests
            .lock()
            .expect("pending requests lock");
        assert!(requests.is_empty(), "viewer mode must not queue host work");
    }

    #[test]
    fn viewer_mode_stop_all_requests_get_error_responses_without_disconnect() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let connection_id = 12;
        let client_id = "web-viewer";
        pair_web_client(&service, client_id);
        let (std_tx, _std_rx) = std_mpsc::channel::<ServerMessage>();
        register_client(
            &service.inner,
            connection_id,
            client_id,
            std_tx,
            test_web_sender(),
        );

        let (tokio_tx, mut tokio_rx) = tokio_mpsc::unbounded_channel::<ServerMessage>();
        handle_inbound(
            &service.inner,
            connection_id,
            client_id,
            WsInbound::Request {
                id: 29,
                action: WebAction::StopAllServers,
                expected_lease_generation: None,
            },
            &tokio_tx,
        );

        match tokio_rx.try_recv().expect("response frame") {
            ServerMessage::Response { request_id, result } => {
                assert_eq!(request_id, 29);
                assert!(!result.ok);
                assert_eq!(
                    result.message.as_deref(),
                    Some("This client is in viewer mode. Take control first."),
                );
            }
            other => panic!("unexpected message: {other:?}"),
        }

        let requests = service
            .inner
            .pending_requests
            .lock()
            .expect("pending requests lock");
        assert!(requests.is_empty(), "viewer mode must not queue host work");
    }

    #[test]
    fn expired_request_authorization_clears_controller_and_allows_reacquire() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let first_connection = 120;
        let first_client = "expired-request-owner";
        pair_web_client(&service, first_client);
        let (first_native, _first_native_rx) = std_mpsc::channel::<ServerMessage>();
        register_client(
            &service.inner,
            first_connection,
            first_client,
            first_native,
            test_web_sender(),
        );
        let acquired = acquire_writer_lease(
            &service.inner,
            first_connection,
            first_client,
            "phone-a",
            now_epoch_ms().saturating_sub(10_000),
        );
        assert!(acquired.you_are_owner);

        let (response_tx, mut response_rx) = tokio_mpsc::unbounded_channel();
        handle_inbound(
            &service.inner,
            first_connection,
            first_client,
            WsInbound::Request {
                id: 91,
                action: WebAction::StopAllServers,
                expected_lease_generation: Some(acquired.generation),
            },
            &response_tx,
        );
        assert!(matches!(
            response_rx.try_recv(),
            Ok(ServerMessage::Response { request_id: 91, result }) if !result.ok
        ));
        assert_eq!(
            service
                .inner
                .controller_client_id
                .read()
                .expect("controller lock")
                .as_deref(),
            None,
            "expired request left the web controller stranded"
        );

        let second_connection = 121;
        let second_client = "replacement-request-owner";
        pair_web_client(&service, second_client);
        let (second_native, _second_native_rx) = std_mpsc::channel::<ServerMessage>();
        register_client(
            &service.inner,
            second_connection,
            second_client,
            second_native,
            test_web_sender(),
        );
        let reacquired = acquire_writer_lease(
            &service.inner,
            second_connection,
            second_client,
            "phone-b",
            now_epoch_ms(),
        );
        assert!(reacquired.you_are_owner);
    }

    #[test]
    fn stale_generation_activity_after_expiry_clears_controller_and_status() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let connection_id = 123;
        let client_id = "expired-stale-owner";
        pair_web_client(&service, client_id);
        let (native, _native_rx) = std_mpsc::channel();
        let (web_sender, mut web_receiver) = test_web_channel();
        register_client(&service.inner, connection_id, client_id, native, web_sender);
        let acquired = acquire_writer_lease(
            &service.inner,
            connection_id,
            client_id,
            "expired-stale-tab",
            now_epoch_ms().saturating_sub(10_000),
        );
        assert!(acquired.you_are_owner);
        assert_eq!(
            try_recv_web_json(&mut web_receiver)["type"],
            "writerLeaseState"
        );

        let (response_tx, mut response_rx) = tokio_mpsc::unbounded_channel();
        handle_inbound(
            &service.inner,
            connection_id,
            client_id,
            WsInbound::Request {
                id: 93,
                action: WebAction::StopAllServers,
                expected_lease_generation: Some(acquired.generation + 99),
            },
            &response_tx,
        );
        assert!(matches!(
            response_rx.try_recv(),
            Ok(ServerMessage::Response { request_id: 93, result }) if !result.ok
        ));
        assert_eq!(
            service
                .inner
                .controller_client_id
                .read()
                .expect("controller lock")
                .as_deref(),
            None
        );
        assert!(service
            .inner
            .web_control
            .lock()
            .expect("web control lock")
            .writer_leases()
            .peek()
            .is_none());
        let status = try_recv_web_json(&mut web_receiver);
        assert_eq!(status["type"], "writerLeaseState");
        assert!(status["writerLease"]["ownerClientInstanceId"].is_null());
        assert!(status["writerLease"]["generation"].as_u64().unwrap() > acquired.generation);
    }

    #[test]
    fn lease_actions_publish_one_ordered_state_through_expiry_and_handoff() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let expired_connection = 124;
        let expired_client = "expired-heartbeat-owner";
        let replacement_connection = 125;
        let replacement_client = "replacement-heartbeat-owner";
        pair_web_client(&service, expired_client);
        pair_web_client(&service, replacement_client);
        let (expired_native, _expired_native_rx) = std_mpsc::channel();
        let (expired_sender, mut expired_receiver) = test_web_channel();
        register_client(
            &service.inner,
            expired_connection,
            expired_client,
            expired_native,
            expired_sender.clone(),
        );
        let (replacement_native, _replacement_native_rx) = std_mpsc::channel();
        let (replacement_sender, mut replacement_receiver) = test_web_channel();
        register_client(
            &service.inner,
            replacement_connection,
            replacement_client,
            replacement_native,
            replacement_sender.clone(),
        );
        let expired = acquire_writer_lease(
            &service.inner,
            expired_connection,
            expired_client,
            "expired-heartbeat-tab",
            now_epoch_ms().saturating_sub(10_000),
        );
        assert!(expired.you_are_owner);
        assert_eq!(
            try_recv_web_json(&mut expired_receiver)["type"],
            "writerLeaseState"
        );
        assert_eq!(
            try_recv_web_json(&mut replacement_receiver)["type"],
            "writerLeaseState"
        );

        handle_inbound_browser(
            &service.inner,
            expired_connection,
            expired_client,
            WsInbound::WriterLeaseHeartbeat {
                client_instance_id: "expired-heartbeat-tab".to_string(),
                expected_lease_generation: expired.generation + 99,
                visible: true,
            },
            &expired_sender,
        );
        handle_inbound_browser(
            &service.inner,
            replacement_connection,
            replacement_client,
            WsInbound::AcquireWriterLease {
                client_instance_id: "replacement-heartbeat-tab".to_string(),
                visible: true,
            },
            &replacement_sender,
        );

        let expired_view = [
            try_recv_web_json(&mut expired_receiver),
            try_recv_web_json(&mut expired_receiver),
        ];
        assert!(expired_view
            .iter()
            .all(|frame| frame["type"] == "writerLeaseState"));
        assert!(expired_view[0]["writerLease"]["ownerClientInstanceId"].is_null());
        assert_eq!(
            expired_view[1]["writerLease"]["ownerClientInstanceId"],
            "replacement-heartbeat-tab"
        );
        let generations = expired_view
            .iter()
            .map(|frame| frame["writerLease"]["generation"].as_u64().unwrap())
            .collect::<Vec<_>>();
        assert!(
            generations.windows(2).all(|pair| pair[0] <= pair[1]),
            "writer lease frames regressed from a newer expiry or handoff generation"
        );
        assert!(expired_receiver.rx.try_recv().is_err());

        let replacement_view = [
            try_recv_web_json(&mut replacement_receiver),
            try_recv_web_json(&mut replacement_receiver),
        ];
        assert!(replacement_view[0]["writerLease"]["ownerClientInstanceId"].is_null());
        assert_eq!(
            replacement_view[1]["writerLease"]["ownerClientInstanceId"],
            "replacement-heartbeat-tab"
        );
        assert_eq!(replacement_view[1]["writerLease"]["youAreOwner"], true);
        assert!(replacement_receiver.rx.try_recv().is_err());

        handle_inbound_browser(
            &service.inner,
            replacement_connection,
            replacement_client,
            WsInbound::SetVisibility {
                client_instance_id: "replacement-heartbeat-tab".to_string(),
                visible: false,
            },
            &replacement_sender,
        );
        assert_eq!(
            try_recv_web_json(&mut expired_receiver)["writerLease"]["ownerClientInstanceId"],
            "replacement-heartbeat-tab"
        );
        assert_eq!(
            try_recv_web_json(&mut replacement_receiver)["writerLease"]["ownerClientInstanceId"],
            "replacement-heartbeat-tab"
        );
        assert!(expired_receiver.rx.try_recv().is_err());
        assert!(replacement_receiver.rx.try_recv().is_err());
    }

    #[test]
    fn saturated_host_request_queue_rejects_web_request_immediately() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let connection_id = 122;
        let client_id = "saturated-request-owner";
        pair_web_client(&service, client_id);
        let (native, _native_rx) = std_mpsc::channel();
        register_client(
            &service.inner,
            connection_id,
            client_id,
            native,
            test_web_sender(),
        );
        let lease = acquire_writer_lease(
            &service.inner,
            connection_id,
            client_id,
            "phone-saturated",
            now_epoch_ms(),
        );
        {
            let mut requests = service.inner.pending_requests.lock().unwrap();
            for index in 0..crate::remote::MAX_PENDING_REMOTE_REQUESTS {
                requests.push(PendingRemoteRequest {
                    client_id: format!("queued-{index}"),
                    action: super::super::super::RemoteAction::GitListRepos,
                    response: None,
                });
            }
        }

        let (response_tx, mut response_rx) = tokio_mpsc::unbounded_channel();
        handle_inbound(
            &service.inner,
            connection_id,
            client_id,
            WsInbound::Request {
                id: 92,
                action: WebAction::StopAllServers,
                expected_lease_generation: Some(lease.generation),
            },
            &response_tx,
        );

        match response_rx.try_recv().expect("capacity response") {
            ServerMessage::Response { request_id, result } => {
                assert_eq!(request_id, 92);
                assert!(!result.ok);
                assert_eq!(
                    result.message.as_deref(),
                    Some("Remote host is busy. Retry shortly.")
                );
            }
            other => panic!("unexpected message: {other:?}"),
        }
        assert_eq!(
            service.inner.pending_requests.lock().unwrap().len(),
            crate::remote::MAX_PENDING_REMOTE_REQUESTS
        );
    }

    #[test]
    fn viewer_mode_paste_image_reports_error_without_disconnect() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        ai_session(&service, "tab-a", "claude-1");
        let connection_id = 13;
        let client_id = "web-viewer";
        pair_web_client(&service, client_id);
        let (std_tx, _std_rx) = std_mpsc::channel::<ServerMessage>();
        register_client(
            &service.inner,
            connection_id,
            client_id,
            std_tx,
            test_web_sender(),
        );

        let (tokio_tx, mut tokio_rx) = tokio_mpsc::unbounded_channel::<ServerMessage>();
        handle_inbound(
            &service.inner,
            connection_id,
            client_id,
            WsInbound::PasteImage {
                session_id: "claude-1".to_string(),
                mime_type: "image/png".to_string(),
                file_name: Some("clip.png".to_string()),
                data_base64: "AQID".to_string(),
                expected_lease_generation: None,
            },
            &tokio_tx,
        );

        match tokio_rx.try_recv().expect("error frame") {
            ServerMessage::Error { message } => {
                assert_eq!(
                    message,
                    "This client is in viewer mode. Take control first."
                );
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }

    #[test]
    fn paste_image_decodes_base64_and_forwards_binary_attachment() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        ai_session(&service, "tab-a", "claude-1");
        let connection_id = 14;
        let client_id = "web-client";
        pair_web_client(&service, client_id);
        let (std_tx, _std_rx) = std_mpsc::channel::<ServerMessage>();
        register_client(
            &service.inner,
            connection_id,
            client_id,
            std_tx,
            test_web_sender(),
        );
        claim_legacy_control(&service.inner, connection_id, client_id, false);

        let (seen_tx, seen_rx) = std_mpsc::channel::<RemoteTerminalInput>();
        service.set_terminal_input_handler(Some(Arc::new(move |input, _| {
            seen_tx.send(input).expect("forwarded input");
            Ok(())
        })));

        let (tokio_tx, _tokio_rx) = tokio_mpsc::unbounded_channel::<ServerMessage>();
        handle_inbound(
            &service.inner,
            connection_id,
            client_id,
            WsInbound::PasteImage {
                session_id: "claude-1".to_string(),
                mime_type: "image/png".to_string(),
                file_name: Some("clip.png".to_string()),
                data_base64: "AQID".to_string(),
                expected_lease_generation: None,
            },
            &tokio_tx,
        );

        match seen_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("image forwarded")
        {
            RemoteTerminalInput::Image {
                session_id,
                attachment,
            } => {
                assert_eq!(session_id, "claude-1");
                assert_eq!(attachment.mime_type, "image/png");
                assert_eq!(attachment.file_name.as_deref(), Some("clip.png"));
                assert_eq!(attachment.bytes, vec![1, 2, 3]);
            }
            other => panic!("unexpected input: {other:?}"),
        }
    }

    #[test]
    fn legacy_raw_input_is_authorized_for_only_the_exact_claiming_connection() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        ai_session(&service, "tab-a", "session-a");
        let client_id = "same-cookie";
        pair_web_client(&service, client_id);
        for connection_id in [1, 2] {
            let (std_tx, _std_rx) = std_mpsc::channel::<ServerMessage>();
            register_client(
                &service.inner,
                connection_id,
                client_id,
                std_tx,
                test_web_sender(),
            );
        }
        let (tokio_tx, _tokio_rx) = tokio_mpsc::unbounded_channel();
        handle_inbound(
            &service.inner,
            1,
            client_id,
            WsInbound::ClaimControlIfAvailable,
            &tokio_tx,
        );
        let (seen_tx, seen_rx) = std_mpsc::channel();
        service.set_terminal_input_handler(Some(Arc::new(move |input, _| {
            seen_tx.send(input).expect("captured input");
            Ok(())
        })));

        for (connection_id, text) in [(2, "viewer"), (1, "owner")] {
            handle_inbound(
                &service.inner,
                connection_id,
                client_id,
                WsInbound::Input {
                    session_id: "session-a".to_string(),
                    text: text.to_string(),
                    expected_lease_generation: None,
                },
                &tokio_tx,
            );
        }
        assert!(matches!(
            seen_rx.recv_timeout(Duration::from_secs(1)),
            Ok(RemoteTerminalInput::Text { text, .. }) if text == "owner"
        ));
        assert!(seen_rx.try_recv().is_err());

        handle_inbound(
            &service.inner,
            2,
            client_id,
            WsInbound::ReleaseControl,
            &tokio_tx,
        );
        handle_inbound(
            &service.inner,
            1,
            client_id,
            WsInbound::Input {
                session_id: "session-a".to_string(),
                text: "still-owner".to_string(),
                expected_lease_generation: None,
            },
            &tokio_tx,
        );
        assert!(matches!(
            seen_rx.recv_timeout(Duration::from_secs(1)),
            Ok(RemoteTerminalInput::Text { text, .. }) if text == "still-owner"
        ));
    }

    #[test]
    fn generation_bearing_raw_input_rejects_a_same_cookie_viewer() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        ai_session(&service, "tab-a", "session-a");
        let client_id = "same-cookie";
        pair_web_client(&service, client_id);
        for connection_id in [1, 2] {
            let (std_tx, _std_rx) = std_mpsc::channel::<ServerMessage>();
            register_client(
                &service.inner,
                connection_id,
                client_id,
                std_tx,
                test_web_sender(),
            );
        }
        let generation =
            acquire_writer_lease(&service.inner, 1, client_id, "owner-tab", now_epoch_ms())
                .generation;
        let (seen_tx, seen_rx) = std_mpsc::channel();
        service.set_terminal_input_handler(Some(Arc::new(move |input, _| {
            seen_tx.send(input).expect("captured input");
            Ok(())
        })));
        let (tokio_tx, _tokio_rx) = tokio_mpsc::unbounded_channel();

        for connection_id in [2, 1] {
            handle_inbound(
                &service.inner,
                connection_id,
                client_id,
                WsInbound::Input {
                    session_id: "session-a".to_string(),
                    text: format!("connection-{connection_id}"),
                    expected_lease_generation: Some(generation),
                },
                &tokio_tx,
            );
        }
        assert!(matches!(
            seen_rx.recv_timeout(Duration::from_secs(1)),
            Ok(RemoteTerminalInput::Text { text, .. }) if text == "connection-1"
        ));
        assert!(seen_rx.try_recv().is_err());
    }

    #[test]
    fn interrupt_session_maps_to_ctrl_c_only_for_exact_generation_owner() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        ai_session(&service, "tab-a", "session-a");
        let client_id = "same-cookie";
        pair_web_client(&service, client_id);
        for connection_id in [1, 2] {
            let (std_tx, _std_rx) = std_mpsc::channel::<ServerMessage>();
            register_client(
                &service.inner,
                connection_id,
                client_id,
                std_tx,
                test_web_sender(),
            );
        }
        let generation =
            acquire_writer_lease(&service.inner, 1, client_id, "owner-tab", now_epoch_ms())
                .generation;
        let (seen_tx, seen_rx) = std_mpsc::channel();
        service.set_terminal_input_handler(Some(Arc::new(move |input, _| {
            seen_tx.send(input).expect("captured interrupt");
            Ok(())
        })));
        let (tokio_tx, mut tokio_rx) = tokio_mpsc::unbounded_channel();

        handle_inbound(
            &service.inner,
            2,
            client_id,
            WsInbound::InterruptSession {
                stable_session_key: StableSessionKey::from_tab("tab-a"),
                expected_lease_generation: generation,
            },
            &tokio_tx,
        );
        assert!(matches!(
            tokio_rx.try_recv(),
            Ok(ServerMessage::Error { .. })
        ));
        handle_inbound(
            &service.inner,
            1,
            client_id,
            WsInbound::InterruptSession {
                stable_session_key: StableSessionKey::from_tab("tab-a"),
                expected_lease_generation: generation,
            },
            &tokio_tx,
        );
        assert!(matches!(
            seen_rx.recv_timeout(Duration::from_secs(1)),
            Ok(RemoteTerminalInput::Bytes { session_id, bytes })
                if session_id == "session-a" && bytes == vec![0x03]
        ));
        assert!(seen_rx.try_recv().is_err());
    }

    #[test]
    fn raw_payload_bounds_reject_before_any_pty_write() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let connection_id = 1;
        let client_id = "web-client";
        pair_web_client(&service, client_id);
        let (std_tx, _std_rx) = std_mpsc::channel::<ServerMessage>();
        register_client(
            &service.inner,
            connection_id,
            client_id,
            std_tx,
            test_web_sender(),
        );
        claim_legacy_control(&service.inner, connection_id, client_id, false);
        let writes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let callback_writes = writes.clone();
        service.set_terminal_input_handler(Some(Arc::new(move |_, _| {
            callback_writes.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })));
        let (tokio_tx, mut tokio_rx) = tokio_mpsc::unbounded_channel();

        handle_inbound(
            &service.inner,
            connection_id,
            client_id,
            WsInbound::Input {
                session_id: "session-a".to_string(),
                text: "x".repeat(MAX_COMPOSER_TEXT_BYTES + 1),
                expected_lease_generation: None,
            },
            &tokio_tx,
        );
        handle_inbound(
            &service.inner,
            connection_id,
            client_id,
            WsInbound::PasteImage {
                session_id: "session-a".to_string(),
                mime_type: "image/gif".to_string(),
                file_name: Some("clip.gif".to_string()),
                data_base64: "AQID".to_string(),
                expected_lease_generation: None,
            },
            &tokio_tx,
        );

        assert_eq!(writes.load(Ordering::SeqCst), 0);
        assert!(matches!(
            tokio_rx.try_recv(),
            Ok(ServerMessage::Error { .. })
        ));
        assert!(matches!(
            tokio_rx.try_recv(),
            Ok(ServerMessage::Error { .. })
        ));
    }

    #[test]
    fn empty_or_oversized_client_instance_ids_never_acquire() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let connection_id = 1;
        let client_id = "web-client";
        pair_web_client(&service, client_id);
        let (std_tx, _std_rx) = std_mpsc::channel::<ServerMessage>();
        register_client(
            &service.inner,
            connection_id,
            client_id,
            std_tx,
            test_web_sender(),
        );
        let (tokio_tx, _tokio_rx) = tokio_mpsc::unbounded_channel();
        let (web_tx, mut web_rx) = tokio_mpsc::unbounded_channel();

        for client_instance_id in [String::new(), "x".repeat(MAX_CLIENT_INSTANCE_ID_BYTES + 1)] {
            handle_inbound_with_web(
                &service.inner,
                connection_id,
                client_id,
                WsInbound::AcquireWriterLease {
                    client_instance_id,
                    visible: true,
                },
                &tokio_tx,
                &web_tx,
            );
            assert!(matches!(web_rx.try_recv(), Ok(WsOutbound::Error { .. })));
        }
        assert!(service
            .inner
            .web_control
            .lock()
            .expect("web control lock")
            .writer_leases()
            .peek()
            .is_none());
    }

    #[test]
    fn lease_changes_are_pushed_to_every_browser_connection() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let client_id = "same-cookie";
        pair_web_client(&service, client_id);
        let mut receivers = Vec::new();
        for connection_id in [1, 2] {
            let (std_tx, _std_rx) = std_mpsc::channel::<ServerMessage>();
            let (web_tx, web_rx) = test_web_channel();
            register_client(&service.inner, connection_id, client_id, std_tx, web_tx);
            receivers.push(web_rx);
        }

        let state = acquire_writer_lease(&service.inner, 1, client_id, "tab-a", 1_000);
        assert!(state.you_are_owner);
        let first = try_recv_web_json(&mut receivers[0]);
        let second = try_recv_web_json(&mut receivers[1]);
        assert_eq!(first["type"], "writerLeaseState");
        assert_eq!(first["writerLease"]["youAreOwner"], true);
        assert_eq!(second["type"], "writerLeaseState");
        assert_eq!(second["writerLease"]["youAreOwner"], false);
    }

    #[test]
    fn serialized_resume_responses_cannot_name_an_already_preempted_owner() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        for (connection_id, client_id) in [(1, "phone"), (2, "laptop")] {
            pair_web_client(&service, client_id);
            let (std_tx, _std_rx) = std_mpsc::channel::<ServerMessage>();
            register_client(
                &service.inner,
                connection_id,
                client_id,
                std_tx,
                test_web_sender(),
            );
        }
        let (first_tx, mut first_rx) = tokio_mpsc::unbounded_channel();
        let (second_tx, mut second_rx) = tokio_mpsc::unbounded_channel();
        send_resume_state(
            &service.inner,
            1,
            "phone",
            resume_request(None, None, "phone-tab"),
            1_000,
            &first_tx,
        );
        send_resume_state(
            &service.inner,
            2,
            "laptop",
            ResumeRequest {
                client_instance_id: "laptop-tab".to_string(),
                ..resume_request(None, None, "laptop-tab")
            },
            1_001,
            &second_tx,
        );

        let first = match first_rx.try_recv().expect("first resume") {
            WsOutbound::ResumeState { state } => state,
            other => panic!("unexpected first response: {other:?}"),
        };
        let second = match second_rx.try_recv().expect("second resume") {
            WsOutbound::ResumeState { state } => state,
            other => panic!("unexpected second response: {other:?}"),
        };
        assert!(first.writer_lease.you_are_owner);
        assert_eq!(
            second.writer_lease.owner_client_instance_id.as_deref(),
            Some("phone-tab")
        );
        assert!(!second.writer_lease.you_are_owner);
        assert_eq!(
            first.writer_lease.generation,
            second.writer_lease.generation
        );
    }

    #[test]
    fn resume_final_reread_reflects_saturated_push_eviction() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let client_id = "web-client";
        pair_web_client(&service, client_id);
        let (std_tx, _std_rx) = std_mpsc::channel::<ServerMessage>();
        let (push_tx, _push_rx) = BrowserOutboundSender::channel(1, WEB_OUTBOUND_MAX_BYTES);
        push_tx
            .try_send(WsOutbound::Pong)
            .expect("prefill bounded push channel");
        let tombstone = push_tx.tombstone();
        register_client(&service.inner, 1, client_id, std_tx, push_tx.clone());
        let lane = WebResponseLane(InboundResponder::Browser {
            sender: push_tx.clone(),
            inner: service.inner.clone(),
            connection_id: 1,
            client_id: client_id.to_string(),
        });

        send_resume_state_with_lane(
            &service.inner,
            1,
            client_id,
            resume_request(None, None, "tab-a"),
            1_000,
            &lane,
            Some(&tombstone),
        );

        assert!(!tombstone.is_active());
        assert!(!service
            .inner
            .clients
            .lock()
            .expect("clients lock")
            .contains_key(&1));
        assert!(service
            .inner
            .web_control
            .lock()
            .expect("web control lock")
            .writer_leases()
            .peek()
            .is_none());
        handle_inbound_browser(
            &service.inner,
            1,
            client_id,
            WsInbound::ClaimControlIfAvailable,
            &push_tx,
        );
        assert!(
            service
                .inner
                .controller_client_id
                .read()
                .expect("controller lock")
                .is_none(),
            "an evicted connection must not reclaim control before its writer closes"
        );
    }

    #[test]
    fn resume_rejects_oversized_route_and_session_key_before_acquiring() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let (response_tx, mut response_rx) = tokio_mpsc::unbounded_channel();
        let oversized_key = StableSessionKey::from_tab("x".repeat(MAX_STABLE_SESSION_KEY_BYTES));
        send_resume_state(
            &service.inner,
            1,
            "web-client",
            ResumeRequest {
                route: "x".repeat(MAX_RESUME_ROUTE_BYTES + 1),
                desired_session_key: Some(oversized_key),
                ..resume_request(None, None, "tab-a")
            },
            1_000,
            &response_tx,
        );
        assert!(matches!(
            response_rx.try_recv(),
            Ok(WsOutbound::Error { .. })
        ));
        assert!(service
            .inner
            .web_control
            .lock()
            .expect("web control lock")
            .writer_leases()
            .peek()
            .is_none());
    }

    #[test]
    fn semantic_subscribe_bootstraps_then_delivers_live_once_and_unsubscribes() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let connection_id = 1;
        let client_id = "web-client";
        pair_web_client(&service, client_id);
        let (std_tx, _std_rx) = std_mpsc::channel::<ServerMessage>();
        let (web_tx, mut web_rx) = test_web_channel();
        register_client(&service.inner, connection_id, client_id, std_tx, web_tx);
        let key = StableSessionKey::from_tab("semantic-tab");
        let retained = publish_semantic_event(
            &service.inner,
            SemanticEventDraft {
                stable_session_key: key.clone(),
                occurred_at_epoch_ms: 1,
                source: SemanticSource::System,
                kind: SemanticEventKind::Status {
                    state: "retained".to_string(),
                    detail: None,
                },
                retention: SemanticRetention::Canonical,
                deduplication_key: None,
            },
        );
        let (tokio_tx, _tokio_rx) = tokio_mpsc::unbounded_channel();

        handle_inbound(
            &service.inner,
            connection_id,
            client_id,
            WsInbound::SubscribeSemantic {
                stable_session_key: key.clone(),
                after_sequence: 0,
            },
            &tokio_tx,
        );
        let bootstrap = try_recv_web_json(&mut web_rx);
        assert_eq!(bootstrap["type"], "semanticReplayPage");
        assert_eq!(bootstrap["events"][0]["sequence"], retained.sequence);
        assert_eq!(bootstrap["throughSequence"], retained.sequence);
        assert_eq!(bootstrap["complete"], true);

        let live = publish_semantic_event(
            &service.inner,
            SemanticEventDraft {
                stable_session_key: key.clone(),
                occurred_at_epoch_ms: 2,
                source: SemanticSource::System,
                kind: SemanticEventKind::Status {
                    state: "live".to_string(),
                    detail: None,
                },
                retention: SemanticRetention::Canonical,
                deduplication_key: None,
            },
        );
        assert!(crate::remote::deliver_live_semantic_events(&service.inner));
        let live_event = try_recv_web_json(&mut web_rx);
        assert_eq!(live_event["type"], "semanticEvent");
        assert_eq!(live_event["event"]["sequence"], live.sequence);
        assert!(crate::remote::deliver_live_semantic_events(&service.inner));
        assert!(web_rx.rx.try_recv().is_err());

        let partial = publish_semantic_event(
            &service.inner,
            SemanticEventDraft {
                stable_session_key: key.clone(),
                occurred_at_epoch_ms: 3,
                source: SemanticSource::Claude,
                kind: SemanticEventKind::AssistantMessage {
                    message_id: "message-1".to_string(),
                    text: "partial".to_string(),
                    streaming: true,
                },
                retention: SemanticRetention::Canonical,
                deduplication_key: Some("message-1".to_string()),
            },
        );
        assert!(crate::remote::deliver_live_semantic_events(&service.inner));
        assert_eq!(
            try_recv_web_json(&mut web_rx)["event"]["sequence"],
            partial.sequence
        );
        let replacement = publish_semantic_event(
            &service.inner,
            SemanticEventDraft {
                stable_session_key: key.clone(),
                occurred_at_epoch_ms: 4,
                source: SemanticSource::Claude,
                kind: SemanticEventKind::AssistantMessage {
                    message_id: "message-1".to_string(),
                    text: "complete".to_string(),
                    streaming: false,
                },
                retention: SemanticRetention::Canonical,
                deduplication_key: Some("message-1".to_string()),
            },
        );
        assert!(crate::remote::deliver_live_semantic_events(&service.inner));
        let replacement_frame = try_recv_web_json(&mut web_rx);
        assert_eq!(replacement_frame["type"], "semanticEvent");
        assert_eq!(replacement_frame["event"]["sequence"], replacement.sequence);
        assert_eq!(
            replacement_frame["event"]["replacesSequence"],
            partial.sequence
        );

        handle_inbound(
            &service.inner,
            connection_id,
            client_id,
            WsInbound::UnsubscribeSemantic {
                stable_session_key: key.clone(),
            },
            &tokio_tx,
        );
        publish_semantic_event(
            &service.inner,
            SemanticEventDraft {
                stable_session_key: key,
                occurred_at_epoch_ms: 3,
                source: SemanticSource::System,
                kind: SemanticEventKind::Status {
                    state: "after-unsubscribe".to_string(),
                    detail: None,
                },
                retention: SemanticRetention::Canonical,
                deduplication_key: None,
            },
        );
        assert!(crate::remote::deliver_live_semantic_events(&service.inner));
        assert!(web_rx.rx.try_recv().is_err());
    }

    #[test]
    fn subscribe_marks_session_before_eager_bootstrap_lookup() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let (entered_tx, entered_rx) = std_mpsc::channel::<()>();
        let gate = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        let provider_gate = gate.clone();
        service.set_session_bootstrap_provider(Some(Arc::new(move |_session_id| {
            let _ = entered_tx.send(());
            let (lock, cvar) = &*provider_gate;
            let mut released = lock.lock().expect("gate lock");
            while !*released {
                released = cvar.wait(released).expect("gate wait");
            }
            None
        })));

        let connection_id = 11;
        let client_id = "web-client";
        pair_web_client(&service, client_id);
        let (std_tx, _std_rx) = std_mpsc::channel::<ServerMessage>();
        register_client(
            &service.inner,
            connection_id,
            client_id,
            std_tx,
            test_web_sender(),
        );

        let (tokio_tx, _tokio_rx) = tokio_mpsc::unbounded_channel::<ServerMessage>();
        let inner = service.inner.clone();
        let worker = std::thread::spawn(move || {
            handle_inbound(
                &inner,
                connection_id,
                client_id,
                WsInbound::SubscribeSessions {
                    session_ids: vec!["alpha".to_string()],
                },
                &tokio_tx,
            );
        });

        entered_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("provider entered");

        let clients = service.inner.clients.lock().expect("clients lock");
        let client = clients.get(&connection_id).expect("registered client");
        assert!(
            client.subscribed_session_ids.contains("alpha"),
            "subscription should be recorded before bootstrap lookup finishes",
        );
        drop(clients);

        let (lock, cvar) = &*gate;
        *lock.lock().expect("gate lock") = true;
        cvar.notify_all();
        worker.join().expect("worker join");
    }

    #[test]
    fn subscribed_web_client_can_receive_output_while_bootstrap_lookup_blocks() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let (entered_tx, entered_rx) = std_mpsc::channel::<()>();
        let gate = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        let provider_gate = gate.clone();
        service.set_session_bootstrap_provider(Some(Arc::new(move |_session_id| {
            let _ = entered_tx.send(());
            let (lock, cvar) = &*provider_gate;
            let mut released = lock.lock().expect("gate lock");
            while !*released {
                released = cvar.wait(released).expect("gate wait");
            }
            None
        })));

        let connection_id = 12;
        let client_id = "web-client";
        pair_web_client(&service, client_id);
        let (std_tx, _std_rx) = std_mpsc::channel::<ServerMessage>();
        let (web_tx, mut web_rx) = test_web_channel();
        register_client(&service.inner, connection_id, client_id, std_tx, web_tx);

        let (tokio_tx, _tokio_rx) = tokio_mpsc::unbounded_channel::<ServerMessage>();
        let inner = service.inner.clone();
        let worker = std::thread::spawn(move || {
            handle_inbound(
                &inner,
                connection_id,
                client_id,
                WsInbound::SubscribeSessions {
                    session_ids: vec!["alpha".to_string()],
                },
                &tokio_tx,
            );
        });

        entered_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("provider entered");

        service.push_session_output("alpha", b"hello".to_vec());
        let frame = try_recv_web_binary(&mut web_rx);
        assert_session_output_frame(&frame, "alpha", b"hello");

        let (lock, cvar) = &*gate;
        *lock.lock().expect("gate lock") = true;
        cvar.notify_all();
        worker.join().expect("worker join");
    }

    #[test]
    fn subscribe_without_initial_bootstrap_still_bootstraps_once_session_becomes_ready() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let connection_id = 15;
        let client_id = "web-client";
        pair_web_client(&service, client_id);
        let (std_tx, _std_rx) = std_mpsc::channel::<ServerMessage>();
        let (web_tx, mut web_rx) = test_web_channel();
        register_client(&service.inner, connection_id, client_id, std_tx, web_tx);

        let (tokio_tx, _tokio_rx) = tokio_mpsc::unbounded_channel::<ServerMessage>();
        handle_inbound(
            &service.inner,
            connection_id,
            client_id,
            WsInbound::SubscribeSessions {
                session_ids: vec!["alpha".to_string()],
            },
            &tokio_tx,
        );

        service.push_session_output("alpha", b"before-ready".to_vec());
        let frame = try_recv_web_binary(&mut web_rx);
        assert_session_output_frame(&frame, "alpha", b"before-ready");

        service.set_session_bootstrap_provider(Some(Arc::new(|session_id| {
            Some(RemoteSessionBootstrap {
                session_id: session_id.to_string(),
                runtime: SessionRuntimeState::new(
                    session_id.to_string(),
                    PathBuf::from("C:\\Code"),
                    SessionDimensions::default(),
                    TerminalBackend::default(),
                ),
                screen: TerminalScreenSnapshot::default(),
                replay_bytes: format!("{session_id}\r\n").into_bytes(),
            })
        })));

        let mut last_bootstrap_retry_at = HashMap::new();
        deliver_pending_bootstraps(&service.inner, &mut last_bootstrap_retry_at);

        let bootstrap = try_recv_web_json(&mut web_rx);
        assert_eq!(bootstrap["type"], "sessionBootstrap");
        assert_eq!(bootstrap["sessionId"], "alpha");

        service.push_session_output("alpha", b"after-ready".to_vec());

        let frame = try_recv_web_binary(&mut web_rx);
        assert_session_output_frame(&frame, "alpha", b"after-ready");
    }
}
