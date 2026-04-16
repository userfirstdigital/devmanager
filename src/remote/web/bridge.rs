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
use std::sync::atomic::Ordering;
use std::sync::{mpsc as std_mpsc, Arc};
use std::time::Duration;

use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc as tokio_mpsc;

use super::super::{
    current_controller_allows, now_epoch_ms, requires_control, stable_hash, ConnectedRemoteClient,
    PendingRemoteRequest, RemoteActionResult, RemoteHostInner, RemoteImageAttachment,
    RemoteSessionStreamEvent, RemoteTerminalInput, RemoteWorkspaceSnapshot, ServerMessage,
    REQUEST_TIMEOUT,
};
use super::wire::{WsInbound, WsOutbound};
use super::{authenticate_request, WebState};
use crate::state::SessionDimensions;

/// Frame type byte prefixed to binary WS frames carrying terminal output.
const BINARY_FRAME_SESSION_OUTPUT: u8 = 0x01;

pub(crate) async fn ws_handler(
    State(state): State<Arc<WebState>>,
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
    ws.on_upgrade(move |socket| run_session(socket, inner, client_id))
}

async fn run_session(socket: WebSocket, inner: Arc<RemoteHostInner>, client_id: String) {
    let connection_id = inner.next_connection_id.fetch_add(1, Ordering::Relaxed);

    // std channel used by the existing broadcaster / push_session_* paths.
    // The broadcaster pushes here; we drain from another task.
    let (std_tx, std_rx) = std_mpsc::channel::<ServerMessage>();

    // tokio channel the WS writer actually awaits on.
    let (tokio_tx, mut tokio_rx) = tokio_mpsc::unbounded_channel::<ServerMessage>();

    // Register in the shared clients map so the broadcaster and
    // push_session_* methods see us.
    register_client(&inner, connection_id, &client_id, std_tx);

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
    let writer_task = tokio::spawn(async move {
        while let Some(message) = tokio_rx.recv().await {
            match encode_outbound(&message) {
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
                    handle_inbound(&inner, connection_id, &client_id, inbound, &tokio_tx);
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
            subscribed_session_ids: HashSet::new(),
            bootstrapped_session_ids: HashSet::new(),
            focused_session_id: None,
            last_app_hash: stable_hash(&app_state),
            last_runtime_hash: stable_hash(&runtime_state),
            last_port_hash: stable_hash(&port_statuses),
            last_controller_client_id: controller_client_id,
            last_you_have_control: you_have_control,
        },
    );
}

fn unregister_client(inner: &Arc<RemoteHostInner>, connection_id: u64, client_id: &str) {
    // Remove this specific connection, then check whether any OTHER client
    // from the same cookie (same client_id) is still attached. If so, the
    // controller bit belongs to them, so leave it alone — otherwise a second
    // browser tab closing would silently pull control away from the first.
    let still_attached = {
        let Ok(mut clients) = inner.clients.lock() else {
            return;
        };
        clients.remove(&connection_id);
        clients.values().any(|client| client.client_id == client_id)
    };
    if still_attached {
        return;
    }
    if let Ok(mut controller) = inner.controller_client_id.write() {
        if controller.as_deref() == Some(client_id) {
            *controller = None;
        }
    }
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

fn handle_inbound(
    inner: &Arc<RemoteHostInner>,
    connection_id: u64,
    client_id: &str,
    message: WsInbound,
    tokio_tx: &tokio_mpsc::UnboundedSender<ServerMessage>,
) {
    if !web_client_is_still_paired(inner, client_id) {
        if let Ok(mut clients) = inner.clients.lock() {
            clients.remove(&connection_id);
        }
        if let Ok(mut controller) = inner.controller_client_id.write() {
            if controller.as_deref() == Some(client_id) {
                *controller = None;
            }
        }
        let _ = tokio_tx.send(ServerMessage::Disconnected {
            message: "This browser is no longer trusted. Pair again to reconnect.".to_string(),
        });
        return;
    }

    match message {
        WsInbound::Ping => {
            let _ = tokio_tx.send(ServerMessage::Pong);
        }
        WsInbound::SubscribeSessions { session_ids } => {
            if let Ok(mut clients) = inner.clients.lock() {
                if let Some(client) = clients.get_mut(&connection_id) {
                    for session_id in &session_ids {
                        client.subscribed_session_ids.insert(session_id.clone());
                        // Mark web clients "bootstrap satisfied" immediately so
                        // live PTY output can flow even if the eager bootstrap
                        // snapshot is slow or blocked on a hot AI session. The
                        // subscribe handler may still deliver a bootstrap frame
                        // later for scrollback, but `push_session_output()`
                        // must not stall behind that lookup.
                        client.bootstrapped_session_ids.insert(session_id.clone());
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
                            continue;
                        }
                        let _ = tokio_tx.send(ServerMessage::SessionStream {
                            event: RemoteSessionStreamEvent::Bootstrap { bootstrap },
                        });
                    }
                }
            }
        }
        WsInbound::UnsubscribeSessions { session_ids } => {
            if let Ok(mut clients) = inner.clients.lock() {
                if let Some(client) = clients.get_mut(&connection_id) {
                    for session_id in &session_ids {
                        client.subscribed_session_ids.remove(session_id);
                        client.bootstrapped_session_ids.remove(session_id);
                    }
                }
            }
        }
        WsInbound::FocusSession { session_id } => {
            if let Ok(mut clients) = inner.clients.lock() {
                if let Some(client) = clients.get_mut(&connection_id) {
                    client.focused_session_id = Some(session_id);
                }
            }
        }
        WsInbound::Input { session_id, text } => {
            if !current_controller_allows(inner, client_id) {
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
                if let Err(error) = handler(
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
        } => {
            if !current_controller_allows(inner, client_id) {
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
            let handler = inner
                .terminal_input_handler
                .read()
                .ok()
                .and_then(|slot| slot.as_ref().cloned());
            if let Some(handler) = handler {
                if let Err(error) = handler(
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
        } => {
            if !current_controller_allows(inner, client_id) {
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
        WsInbound::Action { action } => {
            if requires_control(&action) && !current_controller_allows(inner, client_id) {
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
        WsInbound::Request { id, action } => {
            if requires_control(&action) && !current_controller_allows(inner, client_id) {
                let _ = tokio_tx.send(ServerMessage::Response {
                    request_id: id,
                    result: RemoteActionResult::error(
                        "This client is in viewer mode. Take control first.",
                    ),
                });
                return;
            }

            let (response_tx, response_rx) = std_mpsc::channel();
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
                    .recv_timeout(REQUEST_TIMEOUT)
                    .unwrap_or_else(|_| RemoteActionResult::error("Remote host timed out."));
                let _ = response_tx.send(ServerMessage::Response {
                    request_id: id,
                    result,
                });
            });
        }
        WsInbound::TakeControl => {
            if let Ok(mut controller) = inner.controller_client_id.write() {
                *controller = Some(client_id.to_string());
            }
        }
        WsInbound::ReleaseControl => {
            if let Ok(mut controller) = inner.controller_client_id.write() {
                if controller.as_deref() == Some(client_id) {
                    *controller = None;
                }
            }
        }
    }
}

enum EncodedFrame {
    Text(String),
    Binary(Vec<u8>),
}

/// Translate a `ServerMessage` (the type the broadcaster produces) into a
/// WS frame. Returns `None` for variants that only make sense on the TCP
/// path (e.g., `HelloOk`, `PortForwardOk`).
fn encode_outbound(message: &ServerMessage) -> Option<EncodedFrame> {
    match message {
        ServerMessage::Snapshot { snapshot } => serialize_text(&WsOutbound::Snapshot {
            workspace: snapshot.clone(),
        }),
        ServerMessage::Delta { delta } => serialize_text(&WsOutbound::Delta {
            delta: delta.clone(),
        }),
        ServerMessage::Pong => serialize_text(&WsOutbound::Pong),
        ServerMessage::Error { message } => serialize_text(&WsOutbound::Error {
            message: message.clone(),
        }),
        ServerMessage::Disconnected { message } => serialize_text(&WsOutbound::Disconnected {
            message: message.clone(),
        }),
        ServerMessage::Response { request_id, result } => serialize_text(&WsOutbound::Response {
            id: *request_id,
            result: result.clone(),
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
    use super::*;
    use crate::models::TabType;
    use crate::remote::{
        RemoteAction, RemoteHostConfig, RemoteHostService, RemoteSessionBootstrap, PROTOCOL_VERSION,
    };
    use crate::state::{SessionDimensions, SessionRuntimeState};
    use crate::terminal::session::{TerminalBackend, TerminalScreenSnapshot};
    use std::path::PathBuf;
    use std::sync::mpsc as std_mpsc;

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
    fn encode_outbound_handles_snapshot_as_text() {
        use super::super::super::RemoteWorkspaceSnapshot;
        let snapshot = RemoteWorkspaceSnapshot::default();
        let frame = encode_outbound(&ServerMessage::Snapshot {
            snapshot: snapshot.clone(),
        })
        .expect("snapshot encodes");
        match frame {
            EncodedFrame::Text(text) => {
                assert!(text.contains("\"type\":\"snapshot\""));
            }
            EncodedFrame::Binary(_) => panic!("snapshot should be text"),
        }
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
    fn remote_action_start_server_uses_snake_case_fields() {
        // `RemoteAction` has `rename_all = "camelCase"` on the enum (which
        // only affects the `type` tag) but no `rename_all_fields`. Confirm
        // the variant fields keep their Rust snake_case names so we know
        // exactly what the browser SPA must send.
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
        let (std_tx, _std_rx) = std_mpsc::channel::<ServerMessage>();
        register_client(&service.inner, connection_id, client_id, std_tx);

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
    fn request_frames_queue_host_requests_instead_of_disconnect() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let connection_id = 9;
        let client_id = "web-client";
        let (std_tx, _std_rx) = std_mpsc::channel::<ServerMessage>();
        register_client(&service.inner, connection_id, client_id, std_tx);
        *service
            .inner
            .controller_client_id
            .write()
            .expect("controller lock") = Some(client_id.to_string());

        let (tokio_tx, mut tokio_rx) = tokio_mpsc::unbounded_channel::<ServerMessage>();
        handle_inbound(
            &service.inner,
            connection_id,
            client_id,
            WsInbound::Request {
                id: 17,
                action: RemoteAction::LaunchAi {
                    project_id: "project-1".to_string(),
                    tab_type: TabType::Claude,
                    dimensions: SessionDimensions::default(),
                },
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
        let (std_tx, _std_rx) = std_mpsc::channel::<ServerMessage>();
        register_client(&service.inner, connection_id, client_id, std_tx);

        let (tokio_tx, mut tokio_rx) = tokio_mpsc::unbounded_channel::<ServerMessage>();
        handle_inbound(
            &service.inner,
            connection_id,
            client_id,
            WsInbound::Request {
                id: 23,
                action: RemoteAction::LaunchAi {
                    project_id: "project-1".to_string(),
                    tab_type: TabType::Claude,
                    dimensions: SessionDimensions::default(),
                },
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
        let (std_tx, _std_rx) = std_mpsc::channel::<ServerMessage>();
        register_client(&service.inner, connection_id, client_id, std_tx);

        let (tokio_tx, mut tokio_rx) = tokio_mpsc::unbounded_channel::<ServerMessage>();
        handle_inbound(
            &service.inner,
            connection_id,
            client_id,
            WsInbound::Request {
                id: 29,
                action: RemoteAction::StopAllServers,
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
        let (std_tx, _std_rx) = std_mpsc::channel::<ServerMessage>();
        register_client(&service.inner, connection_id, client_id, std_tx);

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
        let (std_tx, _std_rx) = std_mpsc::channel::<ServerMessage>();
        register_client(&service.inner, connection_id, client_id, std_tx);
        *service
            .inner
            .controller_client_id
            .write()
            .expect("controller lock") = Some(client_id.to_string());

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
        let (std_tx, _std_rx) = std_mpsc::channel::<ServerMessage>();
        register_client(&service.inner, connection_id, client_id, std_tx);

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
        let (std_tx, std_rx) = std_mpsc::channel::<ServerMessage>();
        register_client(&service.inner, connection_id, client_id, std_tx);

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
}
