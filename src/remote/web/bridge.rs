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
use std::sync::atomic::Ordering;
use std::sync::{mpsc as std_mpsc, Arc, MutexGuard};
use std::time::Duration;

use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::mpsc as tokio_mpsc;

use super::super::presentation::{
    SemanticEventDraft, SemanticEventKind, SemanticRetention, SemanticSessionMetadata,
    SemanticSource, StableSessionKey,
};
use super::super::{
    now_epoch_ms, publish_semantic_event, release_web_writer_connection,
    request_timeout_for_action, requires_control, stable_hash, ConnectedRemoteClient,
    PendingRemoteRequest, RemoteActionResult, RemoteHostInner, RemoteImageAttachment,
    RemoteSessionStreamEvent, RemoteTerminalInput, RemoteWorkspaceSnapshot, ServerMessage,
    WebComposerMutationRecord, WebComposerMutationStatus,
};
use super::action::WebActionResult;
use super::dto::{WebWorkspaceSnapshot, WebWriterLeaseState};
use super::lease::{LeaseError, MutationBegin, WriterLease};
use super::wire::{
    ComposerAccepted, ComposerAttachment, ComposerRejectCode, ComposerRejected, ResumeRequest,
    ResumeState, SemanticBootstrap, WsInbound, WsOutbound,
};
use super::{authenticate_request, record_browser_connection, WebState};
use crate::state::{SessionDimensions, SessionKind};

/// Frame type byte prefixed to binary WS frames carrying terminal output.
const BINARY_FRAME_SESSION_OUTPUT: u8 = 0x01;
const WEB_PUSH_CHANNEL_CAPACITY: usize = 256;
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
const MAX_CLIENT_INSTANCE_ID_BYTES: usize = 128;
const MAX_STABLE_SESSION_KEY_BYTES: usize = 512;
const MAX_SESSION_ID_BYTES: usize = 512;
const MAX_SESSION_SUBSCRIPTIONS: usize = 256;

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct WsConnectQuery {
    browser_install_id: Option<String>,
}

pub(crate) async fn ws_handler(
    State(state): State<Arc<WebState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Query(query): Query<WsConnectQuery>,
    ws: WebSocketUpgrade,
    headers: HeaderMap,
) -> Response {
    let Some(client_id) = authenticate_request(&state, &headers) else {
        return (
            StatusCode::UNAUTHORIZED,
            "missing or invalid web auth cookie",
        )
            .into_response();
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

async fn run_session(socket: WebSocket, inner: Arc<RemoteHostInner>, client_id: String) {
    let connection_id = inner.next_connection_id.fetch_add(1, Ordering::Relaxed);

    // std channel used by the existing broadcaster / push_session_* paths.
    // The broadcaster pushes here; we drain from another task.
    let (std_tx, std_rx) = std_mpsc::channel::<ServerMessage>();

    // tokio channel the WS writer actually awaits on.
    let (tokio_tx, mut tokio_rx) = tokio_mpsc::unbounded_channel::<ServerMessage>();
    let (web_tx, mut web_rx) = tokio_mpsc::unbounded_channel::<WsOutbound>();
    let (web_push_tx, mut web_push_rx) =
        tokio_mpsc::channel::<WsOutbound>(WEB_PUSH_CHANNEL_CAPACITY);

    // Register in the shared clients map so the broadcaster and
    // push_session_* methods see us.
    register_client(&inner, connection_id, &client_id, std_tx, web_push_tx);

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
    {
        let snapshot = light_snapshot(&inner, &client_id);
        let _ = tokio_tx.send(ServerMessage::Snapshot { snapshot });
    }

    // Spawn a blocking drainer that forwards from the std receiver into the
    // tokio channel. Checks the stop flag + channel disconnect on every poll
    // so shutdown is prompt when the WS task drops its side.
    let drainer_inner = inner.clone();
    let drainer_tokio_tx = tokio_tx.clone();
    let drainer_handle = tokio::task::spawn_blocking(move || loop {
        if drainer_inner.stop_flag.load(Ordering::Relaxed) {
            break;
        }
        match std_rx.recv_timeout(Duration::from_millis(150)) {
            Ok(message) => {
                if drainer_tokio_tx.send(message).is_err() {
                    break;
                }
            }
            Err(std_mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std_mpsc::RecvTimeoutError::Disconnected) => break,
        }
    });

    let (mut ws_sink, mut ws_stream) = socket.split();

    // Writer task: serialize `ServerMessage`s and push out on the WS.
    let writer_inner = inner.clone();
    let writer_client_id = client_id.clone();
    let writer_task = tokio::spawn(async move {
        let mut native_open = true;
        let mut web_open = true;
        loop {
            let encoded = tokio::select! {
                message = tokio_rx.recv(), if native_open => {
                    match message {
                        Some(message) => translate_outbound(
                            &message,
                            &writer_inner,
                            connection_id,
                            &writer_client_id,
                        ),
                        None => {
                            native_open = false;
                            None
                        }
                    }
                }
                message = web_rx.recv(), if web_open => {
                    match message {
                        Some(message) => serialize_text(&message),
                        None => {
                            web_open = false;
                            None
                        }
                    }
                }
                message = web_push_rx.recv() => {
                    match message {
                        Some(message) => serialize_text(&message),
                        // The shared client record owns the only bounded push
                        // sender. Fanout drops that record on saturation or a
                        // dead receiver; closing the socket here makes that
                        // eviction real instead of leaving an unregistered
                        // reader able to reclaim control through direct frames.
                        None => break,
                    }
                }
            };
            match encoded {
                Some(EncodedFrame::Text(text)) => {
                    if ws_sink.send(WsMessage::Text(text)).await.is_err() {
                        break;
                    }
                }
                Some(EncodedFrame::Binary(bytes)) => {
                    if ws_sink.send(WsMessage::Binary(bytes)).await.is_err() {
                        break;
                    }
                }
                None => {
                    // Unsupported/ignored server messages (e.g., HelloOk which
                    // only makes sense on the TCP path).
                }
            }
        }
        let _ = ws_sink.close().await;
    });

    // Reader loop: handle inbound WS messages directly against
    // `RemoteHostInner` state. We do not await while holding any std lock.
    while let Some(frame) = ws_stream.next().await {
        match frame {
            Ok(WsMessage::Text(text)) => match serde_json::from_str::<WsInbound>(&text) {
                Ok(inbound) => {
                    handle_inbound_with_web(
                        &inner,
                        connection_id,
                        &client_id,
                        inbound,
                        &tokio_tx,
                        &web_tx,
                    );
                }
                Err(error) => {
                    let _ = tokio_tx.send(ServerMessage::Disconnected {
                        message: format!("invalid inbound frame: {error}"),
                    });
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
    unregister_client(&inner, connection_id, &client_id);
    drop(tokio_tx);
    drop(web_tx);
    let _ = drainer_handle.await;
    writer_task.abort();
    let _ = writer_task.await;
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

fn register_client(
    inner: &Arc<RemoteHostInner>,
    connection_id: u64,
    client_id: &str,
    sender: std_mpsc::Sender<ServerMessage>,
    web_sender: tokio_mpsc::Sender<WsOutbound>,
) {
    let Ok(mut clients) = inner.clients.lock() else {
        return;
    };
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

    clients.insert(
        connection_id,
        ConnectedRemoteClient {
            client_id: client_id.to_string(),
            sender,
            web_sender: Some(web_sender),
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
}

fn unregister_client(inner: &Arc<RemoteHostInner>, connection_id: u64, client_id: &str) {
    // Remove this specific connection. Other same-cookie tabs remain attached,
    // viewer tab therefore cannot clear the owner tab's controller state.
    if let Ok(mut clients) = inner.clients.lock() {
        clients.remove(&connection_id);
    }
    release_web_writer_connection(inner, connection_id, client_id);
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
) -> bool {
    inner
        .clients
        .lock()
        .map(|clients| {
            clients
                .get(&connection_id)
                .is_some_and(|client| client.web_sender.is_some() && client.client_id == client_id)
        })
        .unwrap_or(false)
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

fn handle_inbound_with_web(
    inner: &Arc<RemoteHostInner>,
    connection_id: u64,
    client_id: &str,
    message: WsInbound,
    tokio_tx: &tokio_mpsc::UnboundedSender<ServerMessage>,
    web_tx: &tokio_mpsc::UnboundedSender<WsOutbound>,
) {
    if !web_client_is_still_paired(inner, client_id)
        || !web_connection_is_registered(inner, connection_id, client_id)
    {
        unregister_client(inner, connection_id, client_id);
        let _ = tokio_tx.send(ServerMessage::Disconnected {
            message: "This browser connection is no longer active. Reconnect or pair again."
                .to_string(),
        });
        return;
    }

    match message {
        WsInbound::Resume { request } => {
            send_resume_state(
                inner,
                connection_id,
                client_id,
                request,
                now_epoch_ms(),
                web_tx,
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
            let writer_lease = if visible {
                acquire_writer_lease(
                    inner,
                    connection_id,
                    client_id,
                    &client_instance_id,
                    now_epoch_ms(),
                )
            } else {
                set_writer_visibility(
                    inner,
                    connection_id,
                    client_id,
                    &client_instance_id,
                    false,
                    now_epoch_ms(),
                )
            };
            let _ = web_tx.send(WsOutbound::WriterLeaseState { writer_lease });
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
            let writer_lease = renew_writer_lease(
                inner,
                connection_id,
                client_id,
                &client_instance_id,
                expected_lease_generation,
                visible,
                now_epoch_ms(),
            );
            let _ = web_tx.send(WsOutbound::WriterLeaseState { writer_lease });
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
            let writer_lease = set_writer_visibility(
                inner,
                connection_id,
                client_id,
                &client_instance_id,
                visible,
                now_epoch_ms(),
            );
            let _ = web_tx.send(WsOutbound::WriterLeaseState { writer_lease });
        }
        WsInbound::ComposerSubmit {
            mutation_id,
            stable_session_key,
            text,
            attachments,
            expected_lease_generation,
        } => {
            match process_composer_submit(
                inner,
                connection_id,
                client_id,
                mutation_id,
                stable_session_key,
                text,
                attachments,
                expected_lease_generation,
                now_epoch_ms(),
            ) {
                Ok(accepted) => {
                    let _ = web_tx.send(WsOutbound::ComposerAccepted { accepted });
                }
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
                subscribe_semantic(inner, connection_id, stable_session_key, after_sequence)
            }
        }
        WsInbound::UnsubscribeSemantic { stable_session_key } => {
            if !valid_stable_session_key(&stable_session_key) {
                let _ = web_tx.send(WsOutbound::Error {
                    message: "Semantic session key is empty or too long.".to_string(),
                });
            } else {
                unsubscribe_semantic(inner, connection_id, &stable_session_key)
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
            if !web_mutation_authorized(
                inner,
                connection_id,
                client_id,
                Some(expected_lease_generation),
                now_epoch_ms(),
            ) {
                let _ = tokio_tx.send(ServerMessage::Error {
                    message: "The writer lease changed before the interrupt was accepted."
                        .to_string(),
                });
                return;
            }
            let Ok((session_id, _)) = resolve_unique_session(inner, &stable_session_key) else {
                let _ = tokio_tx.send(ServerMessage::Error {
                    message: "The requested session no longer exists.".to_string(),
                });
                return;
            };
            let handler = inner
                .terminal_input_handler
                .read()
                .ok()
                .and_then(|slot| slot.as_ref().cloned());
            if let Some(handler) = handler {
                if let Err(error) = invoke_terminal_input(
                    &handler,
                    RemoteTerminalInput::Bytes {
                        session_id,
                        bytes: vec![0x03],
                    },
                    now_epoch_ms(),
                ) {
                    let _ = tokio_tx.send(ServerMessage::Error { message: error });
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
                if let Some(client) = clients.get_mut(&connection_id) {
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
            if let Ok(mut clients) = inner.clients.lock() {
                if let Some(client) = clients.get_mut(&connection_id) {
                    for (session_id, bootstrap) in bootstraps {
                        if !client.subscribed_session_ids.contains(&session_id) {
                            client.bootstrap_pending_session_ids.remove(&session_id);
                            continue;
                        }
                        if tokio_tx
                            .send(ServerMessage::SessionStream {
                                event: RemoteSessionStreamEvent::Bootstrap { bootstrap },
                            })
                            .is_ok()
                        {
                            // `bootstrapped_session_ids` must mean "this
                            // connection has actually been sent a bootstrap",
                            // not merely "it subscribed". Otherwise a
                            // late-attaching browser can miss the snapshot
                            // forever if the first lookup returns `None`.
                            client.bootstrapped_session_ids.insert(session_id.clone());
                            // Only clear the pending bit after a bootstrap has
                            // really been delivered. Clearing it on an eager
                            // subscribe miss regressed late-attaching AI tabs:
                            // the browser stayed subscribed, but no later host
                            // retry was allowed to send the first snapshot.
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
                if let Some(client) = clients.get_mut(&connection_id) {
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
                if let Some(client) = clients.get_mut(&connection_id) {
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
            if !web_mutation_authorized(
                inner,
                connection_id,
                client_id,
                expected_lease_generation,
                now_epoch_ms(),
            ) {
                // Viewer-mode typing is a no-op on the host, matching the
                // native TCP client's behavior.
                return;
            }
            let handler = inner
                .terminal_input_handler
                .read()
                .ok()
                .and_then(|slot| slot.as_ref().cloned());
            if let Some(handler) = handler {
                if let Err(error) = invoke_terminal_input(
                    &handler,
                    RemoteTerminalInput::Text { session_id, text },
                    now_epoch_ms(),
                ) {
                    let _ = tokio_tx.send(ServerMessage::Error { message: error });
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
            if !web_mutation_authorized(
                inner,
                connection_id,
                client_id,
                expected_lease_generation,
                now_epoch_ms(),
            ) {
                let _ = tokio_tx.send(ServerMessage::Error {
                    message: "This client is in viewer mode. Take control first.".to_string(),
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
            let handler = inner
                .terminal_input_handler
                .read()
                .ok()
                .and_then(|slot| slot.as_ref().cloned());
            if let Some(handler) = handler {
                if let Err(error) = invoke_terminal_input(
                    &handler,
                    RemoteTerminalInput::Image {
                        session_id,
                        attachment: RemoteImageAttachment {
                            mime_type,
                            file_name,
                            bytes,
                        },
                    },
                    now_epoch_ms(),
                ) {
                    let _ = tokio_tx.send(ServerMessage::Error { message: error });
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
            if !web_mutation_authorized(
                inner,
                connection_id,
                client_id,
                expected_lease_generation,
                now_epoch_ms(),
            ) {
                return;
            }
            let handler = inner
                .terminal_resize_handler
                .read()
                .ok()
                .and_then(|slot| slot.as_ref().cloned());
            if let Some(handler) = handler {
                // The SPA only knows rows/cols; the host's `SessionDimensions`
                // carries cell pixel size too. Use sensible defaults so the
                // PTY sizing math still works — the host recomputes its own
                // pixel dimensions when it paints.
                handler(
                    session_id,
                    SessionDimensions {
                        rows,
                        cols,
                        cell_width: 10,
                        cell_height: 20,
                    },
                );
            }
        }
        WsInbound::Action {
            action,
            expected_lease_generation,
        } => {
            let action = action.into_remote();
            if requires_control(&action)
                && !web_mutation_authorized(
                    inner,
                    connection_id,
                    client_id,
                    expected_lease_generation,
                    now_epoch_ms(),
                )
            {
                let _ = tokio_tx.send(ServerMessage::Disconnected {
                    message: "viewer mode: take control before acting".to_string(),
                });
                return;
            }
            if let Ok(mut requests) = inner.pending_requests.lock() {
                requests.push(PendingRemoteRequest {
                    client_id: client_id.to_string(),
                    action,
                    response: None,
                });
            }
        }
        WsInbound::Request {
            id,
            action,
            expected_lease_generation,
        } => {
            let action = action.into_remote();
            if requires_control(&action)
                && !web_mutation_authorized(
                    inner,
                    connection_id,
                    client_id,
                    expected_lease_generation,
                    now_epoch_ms(),
                )
            {
                let _ = tokio_tx.send(ServerMessage::Response {
                    request_id: id,
                    result: RemoteActionResult::error(
                        "This client is in viewer mode. Take control first.",
                    ),
                });
                return;
            }

            let (response_tx, response_rx) = std_mpsc::channel();
            let timeout = request_timeout_for_action(&action);
            if let Ok(mut requests) = inner.pending_requests.lock() {
                requests.push(PendingRemoteRequest {
                    client_id: client_id.to_string(),
                    action,
                    response: Some(response_tx),
                });
            }

            let response_tx = tokio_tx.clone();
            std::thread::spawn(move || {
                let result = response_rx
                    .recv_timeout(timeout)
                    .unwrap_or_else(|_| RemoteActionResult::error("Remote host timed out."));
                let _ = response_tx.send(ServerMessage::Response {
                    request_id: id,
                    result,
                });
            });
        }
        WsInbound::TakeControl => {
            claim_legacy_control(inner, connection_id, client_id, true);
        }
        WsInbound::ClaimControlIfAvailable => {
            claim_legacy_control(inner, connection_id, client_id, false);
        }
        WsInbound::ReleaseControl => {
            release_legacy_control(inner, connection_id, client_id);
        }
    }
}

fn subscribe_semantic(
    inner: &Arc<RemoteHostInner>,
    connection_id: u64,
    stable_session_key: StableSessionKey,
    after_sequence: u64,
) {
    let _delivery = inner
        .semantic_delivery_lock
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // Make the subscription pending before cloning replay. A delivery that
    // took an older cursor snapshot must fail its cursor recheck, while the
    // delivery-only lock prevents two concurrent Subscribe frames from
    // committing duplicate bootstraps.
    let registered = inner
        .clients
        .lock()
        .map(|mut clients| {
            clients.get_mut(&connection_id).is_some_and(|client| {
                if client.web_sender.is_none() {
                    return false;
                }
                client.semantic_cursors.remove(&stable_session_key);
                true
            })
        })
        .unwrap_or(false);
    if !registered {
        return;
    }
    loop {
        let generation = inner
            .semantic_publication_generation
            .load(Ordering::Acquire);
        if generation % 2 != 0 {
            std::thread::yield_now();
            continue;
        }
        // Potentially large replay cloning happens without excluding PTY
        // publishers. A brief commit lock below verifies this optimistic view.
        let replay = inner
            .semantic_journals
            .lock()
            .ok()
            .and_then(|journals| journals.replay_after(&stable_session_key, after_sequence));
        let bootstrap = replay.map_or_else(
            || SemanticBootstrap {
                stable_session_key: stable_session_key.clone(),
                oldest_sequence: 0,
                latest_sequence: 0,
                cursor_rolled_over: false,
                events: Vec::new(),
            },
            |replay| SemanticBootstrap {
                stable_session_key: stable_session_key.clone(),
                oldest_sequence: replay.oldest_sequence,
                latest_sequence: replay.latest_sequence,
                cursor_rolled_over: replay.cursor_rolled_over,
                events: replay.events,
            },
        );
        let latest_sequence = bootstrap.latest_sequence;
        let publication_guard = inner
            .semantic_publication_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if inner
            .semantic_publication_generation
            .load(Ordering::Acquire)
            != generation
        {
            drop(publication_guard);
            std::thread::yield_now();
            continue;
        }
        let send_result = inner.clients.lock().ok().and_then(|mut clients| {
            let client = clients.get_mut(&connection_id)?;
            let sender = client.web_sender.as_ref()?;
            let result = sender.try_send(WsOutbound::SemanticBootstrap { bootstrap });
            if result.is_ok() {
                client
                    .semantic_cursors
                    .insert(stable_session_key.clone(), latest_sequence);
            }
            Some(result)
        });
        let delivered = send_result.as_ref().is_some_and(Result::is_ok);
        drop(publication_guard);
        // Any rejected frame (and its potentially large replay Vec) is dropped
        // only after PTY publication is free to continue.
        drop(send_result);
        if !delivered {
            let client_id = inner
                .clients
                .lock()
                .ok()
                .and_then(|mut clients| clients.remove(&connection_id))
                .map(|client| client.client_id);
            if let Some(client_id) = client_id {
                release_web_writer_connection(inner, connection_id, &client_id);
            }
        }
        break;
    }
}

fn unsubscribe_semantic(
    inner: &Arc<RemoteHostInner>,
    connection_id: u64,
    stable_session_key: &StableSessionKey,
) {
    let _delivery = inner
        .semantic_delivery_lock
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let _publication = inner
        .semantic_publication_lock
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Ok(mut clients) = inner.clients.lock() {
        if let Some(client) = clients.get_mut(&connection_id) {
            client.semantic_cursors.remove(stable_session_key);
        }
    }
}

fn web_mutation_authorized(
    inner: &Arc<RemoteHostInner>,
    connection_id: u64,
    client_id: &str,
    expected_lease_generation: Option<u64>,
    now_epoch_ms: u64,
) -> bool {
    let _operation = inner
        .web_control_operation_lock
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (authorized, lease_changed) = {
        let Ok(mut control) = inner.web_control.lock() else {
            return false;
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
        (authorized, before != after)
    };
    let controller_matches = inner
        .controller_client_id
        .read()
        .map(|controller| controller.as_deref() == Some(client_id))
        .unwrap_or(false);
    if lease_changed {
        broadcast_writer_lease_state_locked(inner, now_epoch_ms);
    }
    authorized && controller_matches
}

fn claim_legacy_control(
    inner: &Arc<RemoteHostInner>,
    connection_id: u64,
    client_id: &str,
    force: bool,
) {
    let _operation = inner
        .web_control_operation_lock
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
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

fn release_legacy_control(inner: &Arc<RemoteHostInner>, connection_id: u64, client_id: &str) {
    let _operation = inner
        .web_control_operation_lock
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
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
    let (state, _) =
        build_resume_state_locked(inner, connection_id, client_id, request, now_epoch_ms);
    broadcast_writer_lease_state_locked(inner, now_epoch_ms);
    state
}

fn send_resume_state(
    inner: &Arc<RemoteHostInner>,
    connection_id: u64,
    client_id: &str,
    request: ResumeRequest,
    now_epoch_ms: u64,
    web_tx: &tokio_mpsc::UnboundedSender<WsOutbound>,
) {
    let valid = valid_client_instance_id(&request.client_instance_id)
        && request.route.len() <= MAX_RESUME_ROUTE_BYTES
        && request
            .desired_session_key
            .as_ref()
            .is_none_or(valid_stable_session_key);
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
    loop {
        let (mut state, semantic_generation) = build_resume_state_locked(
            inner,
            connection_id,
            client_id,
            request.clone(),
            now_epoch_ms,
        );
        let publication_guard = inner
            .semantic_publication_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if inner
            .semantic_publication_generation
            .load(Ordering::Acquire)
            != semantic_generation
        {
            drop(publication_guard);
            std::thread::yield_now();
            continue;
        }
        if let Ok(mut clients) = inner.clients.lock() {
            if let Some(client) = clients.get_mut(&connection_id) {
                client.semantic_cursors.clear();
                if let Some(key) = state.desired_session_key.clone() {
                    let latest = state
                        .semantic_bootstrap
                        .as_ref()
                        .map(|bootstrap| bootstrap.latest_sequence)
                        .unwrap_or(0);
                    client.semantic_cursors.insert(key, latest);
                }
            }
        }
        // Broadcasting can evict a saturated connection and release its
        // lease. Read once more afterwards so the response enqueued under the
        // operation lock is the final authoritative state, not a pre-eviction
        // snapshot. The grant itself carries the 700ms handoff guard.
        broadcast_writer_lease_state_locked(inner, now_epoch_ms);
        state.writer_lease = writer_lease_state_locked(inner, connection_id, now_epoch_ms);
        let _ = web_tx.send(WsOutbound::ResumeState { state });
        drop(publication_guard);
        break;
    }
}

fn build_resume_state_locked(
    inner: &Arc<RemoteHostInner>,
    connection_id: u64,
    client_id: &str,
    request: ResumeRequest,
    now_epoch_ms: u64,
) -> (ResumeState, u64) {
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
    let (projection, semantic_bootstrap, semantic_generation) = capture_resume_projection(
        inner,
        client_id,
        &writer_lease,
        requested_key.as_ref(),
        request.semantic_after_sequence.unwrap_or(0),
    );
    let (route, desired_session_key) = if hard_reset {
        ("/sessions".to_string(), None)
    } else {
        validate_resume_route(&request.route, requested_key.as_ref(), &projection)
    };
    let semantic_bootstrap = desired_session_key.as_ref().and_then(|desired| {
        semantic_bootstrap.filter(|bootstrap| &bootstrap.stable_session_key == desired)
    });

    mark_resume_subscription(
        inner,
        connection_id,
        desired_session_key.as_ref(),
        &projection,
    );

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
            semantic_bootstrap,
            writer_lease,
        },
        semantic_generation,
    )
}

fn capture_resume_projection(
    inner: &Arc<RemoteHostInner>,
    client_id: &str,
    writer_lease: &WebWriterLeaseState,
    desired_session_key: Option<&StableSessionKey>,
    semantic_after_sequence: u64,
) -> (WebWorkspaceSnapshot, Option<SemanticBootstrap>, u64) {
    loop {
        let generation_before = inner
            .semantic_publication_generation
            .load(Ordering::Acquire);
        if generation_before % 2 != 0 {
            std::thread::yield_now();
            continue;
        }
        let (snapshot, revision, semantic_metadata, replay) = {
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
                .and_then(|key| semantic_journals.replay_after(key, semantic_after_sequence))
                .map(|replay| {
                    let key = desired_session_key
                        .expect("semantic replay requires a desired key")
                        .clone();
                    SemanticBootstrap {
                        stable_session_key: key,
                        oldest_sequence: replay.oldest_sequence,
                        latest_sequence: replay.latest_sequence,
                        cursor_rolled_over: replay.cursor_rolled_over,
                        events: replay.events,
                    }
                });
            (
                light_snapshot(inner, client_id),
                inner.snapshot_revision.load(Ordering::Relaxed),
                semantic_journals.metadata_snapshot(),
                replay,
            )
        };
        let generation_after = inner
            .semantic_publication_generation
            .load(Ordering::Acquire);
        if generation_before == generation_after {
            return (
                project_web_snapshot(inner, &snapshot, revision, &semantic_metadata, writer_lease),
                replay,
                generation_after,
            );
        }
        std::thread::yield_now();
    }
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

fn mark_resume_subscription(
    inner: &Arc<RemoteHostInner>,
    connection_id: u64,
    desired_session_key: Option<&StableSessionKey>,
    workspace: &WebWorkspaceSnapshot,
) {
    let desired_session_id = desired_session_key.and_then(|key| {
        workspace
            .sessions
            .iter()
            .find(|session| session.stable_session_key.as_ref() == Some(key))
            .map(|session| session.session_id.clone())
    });
    let Ok(mut clients) = inner.clients.lock() else {
        return;
    };
    let Some(client) = clients.get_mut(&connection_id) else {
        return;
    };
    client.subscribed_session_ids.clear();
    client.bootstrapped_session_ids.clear();
    client.bootstrap_pending_session_ids.clear();
    client.focused_session_id = desired_session_id.clone();
    if let Some(session_id) = desired_session_id {
        client.subscribed_session_ids.insert(session_id.clone());
        client.bootstrap_pending_session_ids.insert(session_id);
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

fn acquire_writer_lease(
    inner: &Arc<RemoteHostInner>,
    connection_id: u64,
    client_id: &str,
    client_instance_id: &str,
    now_epoch_ms: u64,
) -> WebWriterLeaseState {
    let _operation = inner
        .web_control_operation_lock
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
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
) -> WebWriterLeaseState {
    let _operation = inner
        .web_control_operation_lock
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
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

fn set_writer_visibility(
    inner: &Arc<RemoteHostInner>,
    connection_id: u64,
    client_id: &str,
    client_instance_id: &str,
    visible: bool,
    now_epoch_ms: u64,
) -> WebWriterLeaseState {
    let _operation = inner
        .web_control_operation_lock
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
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
                        client
                            .web_sender
                            .clone()
                            .map(|sender| (*connection_id, sender))
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let mut dead = Vec::new();
        for (connection_id, sender) in targets {
            let writer_lease = writer_lease_state_from(current.as_ref(), generation, connection_id);
            if sender
                .try_send(WsOutbound::WriterLeaseState { writer_lease })
                .is_err()
            {
                dead.push(connection_id);
            }
        }
        if dead.is_empty() {
            return;
        }
        let removed = inner
            .clients
            .lock()
            .map(|mut clients| {
                dead.into_iter()
                    .filter_map(|connection_id| {
                        clients
                            .remove(&connection_id)
                            .map(|client| (connection_id, client.client_id))
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let mut authority_changed = false;
        for (connection_id, client_id) in removed {
            let release = inner
                .web_control
                .lock()
                .map(|mut control| control.release_connection(connection_id, &client_id))
                .unwrap_or_default();
            let released_now = release.released_lease.is_some() || release.legacy_released;
            authority_changed |= released_now || release.lease_release_deferred;
            if released_now {
                if let Ok(mut controller) = inner.controller_client_id.write() {
                    if controller.as_deref() == Some(client_id.as_str()) {
                        *controller = None;
                    }
                }
            }
        }
        if !authority_changed {
            return;
        }
    }
}

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
    let (session_id, session_kind) = match resolve_unique_session(inner, &stable_session_key) {
        Ok(session) => session,
        Err(code) => {
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
    };
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
            Ok(_) => {
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
                    // Keep the registry guard through the capacity check,
                    // busy-marker transition, and insertion. The operation
                    // lock serializes reservations, and completion paths drop
                    // this registry lock before taking that operation lock.
                    let mut mutations = composer_mutations(inner);
                    if let Some(existing) = mutations.get(&mutation_id).cloned() {
                        ComposerStart::Existing(existing)
                    } else if mutations.len() >= MAX_COMPOSER_MUTATION_RECORDS {
                        ComposerStart::Rejected(
                            ComposerRejectCode::CapacityExceeded,
                            "Composer mutation history is full for this host runtime. Restart the host before submitting a new prompt.",
                        )
                    } else {
                        let begin = {
                            let mut control = match inner.web_control.lock() {
                                Ok(control) => control,
                                Err(poisoned) => {
                                    let control = poisoned.into_inner();
                                    inner.web_control.clear_poison();
                                    control
                                }
                            };
                            control.writer_leases_mut().begin_mutation(
                                connection_id,
                                client_id,
                                expected_lease_generation,
                                &mutation_id,
                                now_epoch_ms,
                            )
                        };
                        match begin {
                            Ok(MutationBegin::Started(lease)) => {
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
                            Ok(MutationBegin::AlreadyInFlight(_)) => ComposerStart::Rejected(
                                ComposerRejectCode::MutationInFlight,
                                "This mutation is still being accepted by the PTY.",
                            ),
                            Err(_) => ComposerStart::Rejected(
                                ComposerRejectCode::LeaseBusy,
                                "Another prompt is still being accepted by the PTY.",
                            ),
                        }
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
                } => Ok(ComposerAccepted {
                    mutation_id,
                    stable_session_key,
                    accepted_sequence,
                    lease_generation,
                }),
            };
        }
    };

    let handler = inner
        .terminal_input_handler
        .read()
        .ok()
        .and_then(|slot| slot.as_ref().cloned());
    let callback_result = match handler {
        None => Err("The target PTY is not ready for input.".to_string()),
        Some(handler) => std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            for attachment in decoded_attachments {
                handler(
                    RemoteTerminalInput::Image {
                        session_id: session_id.clone(),
                        attachment,
                    },
                    now_epoch_ms,
                )?;
            }
            handler(
                RemoteTerminalInput::Text {
                    session_id,
                    text: format!("{text}\r"),
                },
                now_epoch_ms,
            )
        }))
        .unwrap_or_else(|_| Err("The terminal input handler panicked.".to_string())),
    };
    if let Err(message) = callback_result {
        let message = bounded_composer_error(&message);
        store_pty_rejection(inner, &mutation_id, fingerprint, &message);
        finish_composer_mutation(
            inner,
            connection_id,
            client_id,
            lease_generation,
            &mutation_id,
            now_epoch_ms,
        );
        return Err(composer_rejected(
            inner,
            connection_id,
            mutation_id,
            ComposerRejectCode::PtyRejected,
            message,
            now_epoch_ms,
        ));
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
    let published = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        publish_semantic_event(
            inner,
            SemanticEventDraft {
                stable_session_key: stable_session_key.clone(),
                occurred_at_epoch_ms: now_epoch_ms,
                source,
                kind,
                retention: SemanticRetention::Canonical,
                deduplication_key: Some(format!("composer:{mutation_id}")),
            },
        )
    }));
    let accepted_sequence = published.map(|event| event.sequence).unwrap_or_else(|_| {
        inner
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
        inner,
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
    finish_composer_mutation(
        inner,
        connection_id,
        client_id,
        lease_generation,
        &accepted.mutation_id,
        now_epoch_ms,
    );
    Ok(accepted)
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

fn finish_composer_mutation(
    inner: &Arc<RemoteHostInner>,
    connection_id: u64,
    client_id: &str,
    lease_generation: u64,
    mutation_id: &str,
    finished_at_epoch_ms: u64,
) {
    let _operation = inner
        .web_control_operation_lock
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let finished = inner
        .web_control
        .lock()
        .map(|mut control| {
            control.finish_mutation(
                connection_id,
                client_id,
                lease_generation,
                mutation_id,
                finished_at_epoch_ms,
            )
        })
        .unwrap_or_default();
    if let Some(target) = finished.controller_target {
        if let Ok(mut controller) = inner.controller_client_id.write() {
            *controller = target.client_id().map(str::to_string);
        }
    } else if let Some(released) = finished.released_lease {
        if let Ok(mut controller) = inner.controller_client_id.write() {
            if controller.as_deref() == Some(released.owner_client_id.as_str()) {
                *controller = None;
            }
        }
    }
    broadcast_writer_lease_state_locked(inner, finished_at_epoch_ms);
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

enum EncodedFrame {
    Text(String),
    Binary(Vec<u8>),
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

fn capture_web_snapshot(
    inner: &Arc<RemoteHostInner>,
    connection_id: u64,
    client_id: &str,
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
            let writer_lease = writer_lease_state(inner, connection_id, now_epoch_ms());
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
            serialize_text(&WsOutbound::SessionBootstrap {
                session_id: bootstrap.session_id.clone(),
                replay_base64: STANDARD.encode(&bootstrap.replay_bytes),
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
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::mpsc as std_mpsc;

    fn test_web_sender() -> tokio_mpsc::Sender<WsOutbound> {
        let (sender, receiver) = tokio_mpsc::channel(4096);
        // Focused bridge tests drive the synchronous handler directly rather
        // than a Tokio writer task. Keep the bounded receiver alive so lease
        // fanout exercises try_send without treating the fixture as dead.
        std::mem::forget(receiver);
        sender
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
        assert!(state.semantic_bootstrap.is_none());
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
    fn resume_marks_raw_bootstrap_pending_without_calling_the_provider() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
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
        let state = build_resume_state(
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
        assert!(client.subscribed_session_ids.contains("session-a"));
        assert!(client.bootstrap_pending_session_ids.contains("session-a"));
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
            RemoteTerminalInput::Text { session_id, text } => {
                assert_eq!(session_id, "session-a");
                assert_eq!(text, "hello\r");
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
        assert_eq!(first.accepted_sequence, replay.latest_sequence);
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
            match input {
                RemoteTerminalInput::Image { .. } => Ok(()),
                RemoteTerminalInput::Text { .. } => Err("text write failed".to_string()),
                other => panic!("unexpected composer input: {other:?}"),
            }
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
        assert_eq!(writes.load(Ordering::SeqCst), 3);
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
    fn busy_composer_keeps_authority_until_callback_then_applies_native_takeover() {
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
        assert!(!competitor.you_are_owner);
        assert_eq!(
            competitor.owner_client_instance_id.as_deref(),
            Some("tab-a")
        );

        crate::remote::set_native_controller(&service.inner, Some("native-client".to_string()));
        assert_eq!(
            service
                .inner
                .controller_client_id
                .read()
                .expect("controller lock")
                .as_deref(),
            Some("web-client"),
            "native takeover must wait for the callback"
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
    fn restart_drain_defers_busy_web_cleanup_until_composer_finishes() {
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
                .is_some(),
            "restart must retain the busy owner until callback completion"
        );
        assert_eq!(
            service
                .inner
                .controller_client_id
                .read()
                .expect("controller lock")
                .as_deref(),
            Some("web-client")
        );

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
    fn pty_panic_becomes_stored_terminal_rejection_and_releases_busy_marker() {
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
            "panic cleanup must clear the busy marker"
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
    fn busy_marker_without_registry_record_cannot_execute_the_same_mutation() {
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

        let duplicate = process_composer_submit(
            &service.inner,
            1,
            "web-client",
            "busy-gap".to_string(),
            key,
            "must not execute".to_string(),
            Vec::new(),
            generation,
            1_100,
        )
        .expect_err("the lease's busy marker must fail closed");
        assert_eq!(duplicate.code, ComposerRejectCode::MutationInFlight);
        assert_eq!(writes.load(Ordering::SeqCst), 0);
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
                assert_eq!(value["workspace"]["webProtocolVersion"], 2);
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
    fn viewer_mode_paste_image_reports_error_without_disconnect() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
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
            let (web_tx, web_rx) = tokio_mpsc::channel(8);
            register_client(&service.inner, connection_id, client_id, std_tx, web_tx);
            receivers.push(web_rx);
        }

        let state = acquire_writer_lease(&service.inner, 1, client_id, "tab-a", 1_000);
        assert!(state.you_are_owner);
        let first = receivers[0].try_recv().expect("owner lease push");
        let second = receivers[1].try_recv().expect("viewer lease push");
        assert!(matches!(
            first,
            WsOutbound::WriterLeaseState { writer_lease } if writer_lease.you_are_owner
        ));
        assert!(matches!(
            second,
            WsOutbound::WriterLeaseState { writer_lease } if !writer_lease.you_are_owner
        ));
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
        let (push_tx, _push_rx) = tokio_mpsc::channel(1);
        push_tx
            .try_send(WsOutbound::Pong)
            .expect("prefill bounded push channel");
        register_client(&service.inner, 1, client_id, std_tx, push_tx);
        let (response_tx, mut response_rx) = tokio_mpsc::unbounded_channel();

        send_resume_state(
            &service.inner,
            1,
            client_id,
            resume_request(None, None, "tab-a"),
            1_000,
            &response_tx,
        );

        let state = match response_rx.try_recv().expect("resume response") {
            WsOutbound::ResumeState { state } => state,
            other => panic!("unexpected response: {other:?}"),
        };
        assert!(!state.writer_lease.you_are_owner);
        assert!(state.writer_lease.owner_client_instance_id.is_none());
        assert!(service
            .inner
            .web_control
            .lock()
            .expect("web control lock")
            .writer_leases()
            .peek()
            .is_none());
        let (native_tx, _native_rx) = tokio_mpsc::unbounded_channel();
        handle_inbound(
            &service.inner,
            1,
            client_id,
            WsInbound::ClaimControlIfAvailable,
            &native_tx,
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
        let (web_tx, mut web_rx) = tokio_mpsc::channel(8);
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
        match web_rx.try_recv().expect("semantic bootstrap") {
            WsOutbound::SemanticBootstrap { bootstrap } => {
                assert_eq!(bootstrap.events, vec![retained.clone()]);
                assert_eq!(bootstrap.latest_sequence, retained.sequence);
            }
            other => panic!("unexpected subscribe frame: {other:?}"),
        }

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
        assert!(matches!(
            web_rx.try_recv(),
            Ok(WsOutbound::SemanticEvent { event }) if event == live
        ));
        assert!(crate::remote::deliver_live_semantic_events(&service.inner));
        assert!(web_rx.try_recv().is_err());

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
        assert!(web_rx.try_recv().is_err());
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
        let (std_tx, std_rx) = std_mpsc::channel::<ServerMessage>();
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

        service.push_session_output("alpha", b"hello".to_vec());
        match std_rx.recv_timeout(Duration::from_millis(250)) {
            Ok(ServerMessage::SessionStream {
                event:
                    RemoteSessionStreamEvent::Output {
                        session_id, bytes, ..
                    },
            }) => {
                assert_eq!(session_id, "alpha");
                assert_eq!(bytes, b"hello".to_vec());
            }
            other => panic!("expected output while bootstrap lookup blocks, got {other:?}"),
        }

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
        let (std_tx, std_rx) = std_mpsc::channel::<ServerMessage>();
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
            WsInbound::SubscribeSessions {
                session_ids: vec!["alpha".to_string()],
            },
            &tokio_tx,
        );

        service.push_session_output("alpha", b"before-ready".to_vec());
        match std_rx.recv_timeout(Duration::from_millis(250)) {
            Ok(ServerMessage::SessionStream {
                event:
                    RemoteSessionStreamEvent::Output {
                        session_id, bytes, ..
                    },
            }) => {
                assert_eq!(session_id, "alpha");
                assert_eq!(bytes, b"before-ready".to_vec());
            }
            other => panic!("expected output before bootstrap is ready, got {other:?}"),
        }

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

        match std_rx.recv_timeout(Duration::from_millis(250)) {
            Ok(ServerMessage::SessionStream {
                event: RemoteSessionStreamEvent::Bootstrap { bootstrap },
            }) => assert_eq!(bootstrap.session_id, "alpha"),
            other => panic!("expected late bootstrap event, got {other:?}"),
        }

        service.push_session_output("alpha", b"after-ready".to_vec());

        match std_rx.recv_timeout(Duration::from_millis(250)) {
            Ok(ServerMessage::SessionStream {
                event:
                    RemoteSessionStreamEvent::Output {
                        session_id, bytes, ..
                    },
            }) => {
                assert_eq!(session_id, "alpha");
                assert_eq!(bytes, b"after-ready".to_vec());
            }
            other => panic!("expected output after late bootstrap, got {other:?}"),
        }
    }
}
