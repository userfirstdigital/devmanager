pub mod assets;
pub mod auth;
pub mod bridge;
pub mod wire;

use self::auth::{PairingAttemptTracker, PairingThrottleStatus};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::time::Instant;

use axum::extract::{ConnectInfo, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::Router;
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

use super::{now_epoch_ms, RemoteHostInner};

pub use auth::{
    extract_cookie, generate_cookie_secret_hex, generate_web_pairing_token, sign_cookie,
    verify_cookie, PairedWebClient, WEB_COOKIE_NAME,
};

/// Persisted configuration for the web listener. Lives inside `RemoteHostConfig`
/// and is serialized to `remote.json` via serde defaults.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, rename_all = "camelCase")]
pub struct WebConfig {
    pub enabled: bool,
    pub bind_address: String,
    pub port: u16,
    pub pairing_token: String,
    pub cookie_secret_hex: String,
    pub paired_clients: Vec<PairedWebClient>,
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bind_address: "0.0.0.0".to_string(),
            port: 43872,
            pairing_token: generate_web_pairing_token(),
            cookie_secret_hex: generate_cookie_secret_hex(),
            paired_clients: Vec::new(),
        }
    }
}

impl WebConfig {
    /// Backfill any empty secret fields in-place so older saved configs
    /// upgrade cleanly on the first run after installing this feature.
    pub fn ensure_secrets(&mut self) {
        if self.pairing_token.is_empty() {
            self.pairing_token = generate_web_pairing_token();
        }
        if self.cookie_secret_hex.is_empty() {
            self.cookie_secret_hex = generate_cookie_secret_hex();
        }
        if self.bind_address.is_empty() {
            self.bind_address = "0.0.0.0".to_string();
        }
        if self.port == 0 {
            self.port = 43872;
        }
    }

    /// Human-friendly listener URL for the current bind. When the host binds to
    /// a wildcard address (0.0.0.0 / ::), try to discover a LAN-reachable IP so
    /// phones see something they can actually type into a browser.
    pub fn display_url(&self) -> String {
        let host = host_for_display(&self.bind_address);
        format!("http://{host}:{}", self.port)
    }
}

fn host_for_display(bind_address: &str) -> String {
    let trimmed = bind_address.trim();
    let is_wildcard = trimmed.is_empty() || trimmed == "0.0.0.0" || trimmed == "::";
    if is_wildcard {
        if let Some(ip) = discover_lan_ip() {
            return ip.to_string();
        }
        return "localhost".to_string();
    }
    trimmed.to_string()
}

/// Best-effort LAN IP discovery using the "connect a UDP socket and read
/// local_addr" trick. Does not send any bytes — `connect` on a UDP socket only
/// sets the peer, which is enough for the kernel to pick an outgoing
/// interface. Returns None on any error so callers can fall back to localhost.
pub fn discover_lan_ip() -> Option<IpAddr> {
    let socket = UdpSocket::bind((Ipv4Addr::new(0, 0, 0, 0), 0)).ok()?;
    // 192.0.2.1 is a documentation-reserved address — routing decisions made
    // by `connect` here do not generate any packets.
    socket.connect(("192.0.2.1", 80)).ok()?;
    let local = socket.local_addr().ok()?;
    let ip = local.ip();
    if ip.is_unspecified() {
        None
    } else {
        Some(ip)
    }
}

/// Handle returned from `WebListenerHandle::start`. Dropping the handle (or
/// explicitly calling `shutdown`) signals the axum server to stop and blocks
/// until the tokio runtime has fully torn down.
pub struct WebListenerHandle {
    runtime: Option<tokio::runtime::Runtime>,
    shutdown_tx: Option<oneshot::Sender<()>>,
    pub bind_info: String,
}

impl WebListenerHandle {
    pub(crate) fn start(
        inner: Arc<RemoteHostInner>,
        config: WebConfig,
    ) -> Result<Self, String> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("devmanager-web")
            .build()
            .map_err(|error| format!("failed to build tokio runtime: {error}"))?;

        let bind = format!("{}:{}", config.bind_address, config.port);
        let bind_info = bind.clone();
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let (bind_result_tx, bind_result_rx) = std::sync::mpsc::channel::<Result<(), String>>();

        let router_state = Arc::new(WebState {
            inner: inner.clone(),
            pairing_attempts: Arc::new(std::sync::Mutex::new(PairingAttemptTracker::default())),
        });

        runtime.spawn(async move {
            let app = build_router(router_state);
            match tokio::net::TcpListener::bind(&bind).await {
                Ok(listener) => {
                    let _ = bind_result_tx.send(Ok(()));
                    let _ = axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
                        .with_graceful_shutdown(async {
                            let _ = shutdown_rx.await;
                        })
                        .await;
                }
                Err(error) => {
                    let _ = bind_result_tx.send(Err(format!("bind {bind}: {error}")));
                }
            }
        });

        match bind_result_rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(Self {
                runtime: Some(runtime),
                shutdown_tx: Some(shutdown_tx),
                bind_info,
            }),
            Ok(Err(error)) => Err(error),
            Err(_) => Err("web listener failed to report bind status in time".to_string()),
        }
    }

    pub fn shutdown(mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(runtime) = self.runtime.take() {
            // Drop in a blocking context. tokio's Runtime::drop blocks the
            // calling thread until outstanding tasks finish, which is what we
            // want here — we are called from a std thread, not from inside
            // the runtime itself.
            drop(runtime);
        }
    }
}

impl Drop for WebListenerHandle {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(runtime) = self.runtime.take() {
            drop(runtime);
        }
    }
}

#[derive(Clone)]
pub(crate) struct WebState {
    pub(crate) inner: Arc<RemoteHostInner>,
    pub(crate) pairing_attempts: Arc<std::sync::Mutex<PairingAttemptTracker>>,
}

fn build_router(state: Arc<WebState>) -> Router {
    Router::new()
        .route("/", get(assets::index_handler))
        .route("/pair", get(pair_handler))
        .route("/api/health", get(health_handler))
        .route("/api/me", get(me_handler))
        .route("/api/ws", get(bridge::ws_handler))
        .route("/*path", get(assets::static_handler))
        .with_state(state)
}

async fn health_handler() -> impl IntoResponse {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        r#"{"ok":true}"#,
    )
}

#[derive(Debug, Deserialize)]
struct PairQuery {
    t: Option<String>,
    label: Option<String>,
}

/// `/pair?t=<web_pairing_token>&label=<optional phone name>`
///
/// Validates the token, mints a new `PairedWebClient` plus a signed cookie,
/// and redirects to `/`. On failure returns 401 with a short message (no
/// redirect, so users see what went wrong).
async fn pair_handler(
    State(state): State<Arc<WebState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Query(query): Query<PairQuery>,
) -> Response {
    let client_ip = addr.ip();
    let provided = match query.t {
        Some(token) if !token.is_empty() => token,
        _ => return (StatusCode::UNAUTHORIZED, "missing pairing token").into_response(),
    };

    if let Ok(mut pairing_attempts) = state.pairing_attempts.lock() {
        match pairing_attempts.status(client_ip, Instant::now()) {
            PairingThrottleStatus::Allowed => {}
            PairingThrottleStatus::Backoff(retry_after)
            | PairingThrottleStatus::LockedOut(retry_after) => {
                return throttled_pair_response(retry_after);
            }
        }
    }

    // Read current config snapshot.
    let (expected_token, cookie_secret_hex) = {
        let Ok(config) = state.inner.config.read() else {
            return (StatusCode::INTERNAL_SERVER_ERROR, "config unavailable").into_response();
        };
        if !config.web.enabled {
            return (StatusCode::FORBIDDEN, "web UI disabled").into_response();
        }
        (
            config.web.pairing_token.clone(),
            config.web.cookie_secret_hex.clone(),
        )
    };

    if provided != expected_token {
        let throttle = state
            .pairing_attempts
            .lock()
            .ok()
            .map(|mut pairing_attempts| pairing_attempts.record_failure(client_ip, Instant::now()))
            .unwrap_or(PairingThrottleStatus::Allowed);
        return pair_token_rejected_response(throttle);
    }

    if let Ok(mut pairing_attempts) = state.pairing_attempts.lock() {
        pairing_attempts.record_success(client_ip);
    }

    let client_id = format!("web-{}", now_epoch_ms());
    let label = query
        .label
        .filter(|l| !l.is_empty())
        .unwrap_or_else(|| "Browser".to_string());

    // Persist the new paired client into the shared config. A later iteration
    // will also nudge the outer persistence loop to flush this to disk; for
    // now the in-memory record is enough to make subsequent requests work.
    if let Ok(mut config) = state.inner.config.write() {
        config.web.paired_clients.push(PairedWebClient {
            client_id: client_id.clone(),
            label,
            issued_at_epoch_ms: Some(now_epoch_ms()),
            last_seen_epoch_ms: Some(now_epoch_ms()),
        });
    }
    state
        .inner
        .config_revision
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    let Some(signed) = sign_cookie(&cookie_secret_hex, &client_id) else {
        return (StatusCode::INTERNAL_SERVER_ERROR, "cookie signing failed").into_response();
    };

    // HttpOnly + SameSite=Lax. `Secure` is intentionally omitted because MVP
    // ships over plain HTTP on LAN; later TLS modes will add it conditionally.
    let cookie = format!(
        "{}={}; HttpOnly; SameSite=Lax; Path=/; Max-Age={}",
        WEB_COOKIE_NAME,
        signed,
        60 * 60 * 24 * 30,
    );

    let mut response = Redirect::to("/").into_response();
    response
        .headers_mut()
        .insert(header::SET_COOKIE, cookie.parse().unwrap());
    response
}

fn pair_token_rejected_response(throttle: PairingThrottleStatus) -> Response {
    match throttle {
        PairingThrottleStatus::LockedOut(retry_after) => throttled_pair_response(retry_after),
        PairingThrottleStatus::Backoff(retry_after) => response_with_retry_after(
            StatusCode::UNAUTHORIZED,
            "invalid pairing token",
            retry_after,
        ),
        PairingThrottleStatus::Allowed => {
            (StatusCode::UNAUTHORIZED, "invalid pairing token").into_response()
        }
    }
}

fn throttled_pair_response(retry_after: std::time::Duration) -> Response {
    response_with_retry_after(
        StatusCode::TOO_MANY_REQUESTS,
        "too many pairing attempts",
        retry_after,
    )
}

fn response_with_retry_after(
    status: StatusCode,
    message: &'static str,
    retry_after: std::time::Duration,
) -> Response {
    let seconds = retry_after.as_secs().max(1);
    let mut response = (status, message).into_response();
    if let Ok(value) = seconds.to_string().parse() {
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, value);
    }
    response
}

/// `/api/me` — returns 200 with the paired-client id if the dm_web cookie is
/// valid, 401 otherwise. Small endpoint used by the SPA on load to decide
/// whether to show the "not paired yet" screen or start connecting.
async fn me_handler(State(state): State<Arc<WebState>>, headers: HeaderMap) -> Response {
    match authenticate_request(&state, &headers) {
        Some(client_id) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            format!(r#"{{"clientId":{:?},"ok":true}}"#, client_id),
        )
            .into_response(),
        None => (
            StatusCode::UNAUTHORIZED,
            [(header::CONTENT_TYPE, "application/json")],
            r#"{"ok":false}"#,
        )
            .into_response(),
    }
}

/// Shared helper: returns `Some(client_id)` when the request carries a valid
/// `dm_web` cookie that matches a currently-paired web client and verifies
/// against the host's cookie secret. Used by `/api/me` and (later) the
/// WebSocket upgrade handler.
pub(crate) fn authenticate_request(state: &WebState, headers: &HeaderMap) -> Option<String> {
    let cookie_header = headers.get(header::COOKIE)?.to_str().ok()?;
    let cookie_value = extract_cookie(cookie_header, WEB_COOKIE_NAME)?;

    let (cookie_secret_hex, paired_ids) = {
        let config = state.inner.config.read().ok()?;
        if !config.web.enabled {
            return None;
        }
        let ids: Vec<String> = config
            .web
            .paired_clients
            .iter()
            .map(|client| client.client_id.clone())
            .collect();
        (config.web.cookie_secret_hex.clone(), ids)
    };

    let client_id = verify_cookie(&cookie_secret_hex, &cookie_value)?;
    if !paired_ids.iter().any(|id| id == &client_id) {
        return None;
    }
    Some(client_id)
}
