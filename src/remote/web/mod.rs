pub mod assets;
pub mod auth;
pub mod bridge;
pub mod image_paste;
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
    cookie_name_for_server_id, extract_cookie, generate_cookie_secret_hex,
    generate_web_pairing_token, sign_cookie, verify_cookie, PairedWebClient, WEB_COOKIE_NAME,
};

const WEB_COOKIE_MAX_AGE_SECS: u64 = 60 * 60 * 24 * 365 * 10;

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
    pub(crate) fn start(inner: Arc<RemoteHostInner>, config: WebConfig) -> Result<Self, String> {
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
                    let _ = axum::serve(
                        listener,
                        app.into_make_service_with_connect_info::<SocketAddr>(),
                    )
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
    let (expected_token, cookie_secret_hex, cookie_name) = {
        let Ok(config) = state.inner.config.read() else {
            return (StatusCode::INTERNAL_SERVER_ERROR, "config unavailable").into_response();
        };
        if !config.web.enabled {
            return (StatusCode::FORBIDDEN, "web UI disabled").into_response();
        }
        (
            config.web.pairing_token.clone(),
            config.web.cookie_secret_hex.clone(),
            cookie_name_for_server_id(&config.server_id),
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
    let now = now_epoch_ms();

    if let Err(error) = super::mutate_host_config(&state.inner, |config| {
        config.web.paired_clients.push(PairedWebClient {
            client_id: client_id.clone(),
            label: label.clone(),
            issued_at_epoch_ms: Some(now),
            last_seen_epoch_ms: Some(now),
        });
    }) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to persist web pairing: {error}"),
        )
            .into_response();
    }

    let Some(signed) = sign_cookie(&cookie_secret_hex, &client_id) else {
        return (StatusCode::INTERNAL_SERVER_ERROR, "cookie signing failed").into_response();
    };

    let cookie = auth_cookie_header(&cookie_name, &signed);

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
        response.headers_mut().insert(header::RETRY_AFTER, value);
    }
    response
}

fn auth_cookie_header(cookie_name: &str, signed: &str) -> String {
    // HttpOnly + SameSite=Lax. `Secure` is intentionally omitted because MVP
    // ships over plain HTTP on LAN; later TLS modes will add it conditionally.
    format!(
        "{}={}; HttpOnly; SameSite=Lax; Path=/; Max-Age={}",
        cookie_name, signed, WEB_COOKIE_MAX_AGE_SECS,
    )
}

fn request_auth_cookie(state: &WebState, headers: &HeaderMap) -> Option<(String, String)> {
    let cookie_header = headers.get(header::COOKIE)?.to_str().ok()?;
    let current_cookie_name = {
        let config = state.inner.config.read().ok()?;
        cookie_name_for_server_id(&config.server_id)
    };
    let cookie_value = extract_cookie(cookie_header, &current_cookie_name)
        .or_else(|| extract_cookie(cookie_header, WEB_COOKIE_NAME))?;
    Some((current_cookie_name, cookie_value))
}

/// `/api/me` — returns 200 with the paired-client id if the dm_web cookie is
/// valid, 401 otherwise. Small endpoint used by the SPA on load to decide
/// whether to show the "not paired yet" screen or start connecting.
async fn me_handler(State(state): State<Arc<WebState>>, headers: HeaderMap) -> Response {
    match authenticate_request(&state, &headers) {
        Some(client_id) => {
            let mut response = (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "application/json")],
                format!(r#"{{"clientId":{:?},"ok":true}}"#, client_id),
            )
                .into_response();
            if let Some((cookie_name, cookie_value)) = request_auth_cookie(&state, &headers) {
                let cookie = auth_cookie_header(&cookie_name, &cookie_value);
                if let Ok(value) = cookie.parse() {
                    response.headers_mut().insert(header::SET_COOKIE, value);
                }
            }
            response
        }
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
    let (_, cookie_value) = request_auth_cookie(state, headers)?;

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
    let now = now_epoch_ms();
    let _ = super::mutate_host_config(&state.inner, |config| {
        if let Some(client) = config
            .web
            .paired_clients
            .iter_mut()
            .find(|client| client.client_id == client_id)
        {
            client.last_seen_epoch_ms = Some(now);
        }
    });
    Some(client_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::{
        load_remote_machine_state, save_remote_machine_state, test_support::TestProfileGuard,
        KnownRemoteHost, RemoteHostConfig, RemoteHostService, RemoteMachineState,
    };

    fn test_service(server_id: &str) -> RemoteHostService {
        let mut config = RemoteHostConfig::default();
        config.server_id = server_id.to_string();
        config.web.enabled = true;
        config.web.pairing_token = "PAIR1234".to_string();
        RemoteHostService::new(config)
    }

    fn test_state(service: &RemoteHostService) -> Arc<WebState> {
        Arc::new(WebState {
            inner: service.inner.clone(),
            pairing_attempts: Arc::new(std::sync::Mutex::new(PairingAttemptTracker::default())),
        })
    }

    fn test_addr() -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], 43872))
    }

    #[test]
    fn pair_handler_sets_effectively_permanent_cookie() {
        let _profile = TestProfileGuard::new("web-cookie-max-age");
        let service = test_service("host-a");
        let state = test_state(&service);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");

        let response = runtime.block_on(async {
            pair_handler(
                State(state),
                ConnectInfo(test_addr()),
                Query(PairQuery {
                    t: Some("PAIR1234".to_string()),
                    label: None,
                }),
            )
            .await
        });
        drop(runtime);

        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let set_cookie = response
            .headers()
            .get(header::SET_COOKIE)
            .expect("pair response should set auth cookie")
            .to_str()
            .expect("cookie should be utf-8");
        assert!(
            set_cookie.contains("Max-Age=315360000"),
            "expected 10-year Max-Age, got: {set_cookie}"
        );
    }

    #[test]
    fn me_handler_refreshes_valid_cookie() {
        let _profile = TestProfileGuard::new("web-cookie-refresh");
        let service = test_service("host-a");
        let state = test_state(&service);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");

        let pair_response = runtime.block_on(async {
            pair_handler(
                State(state.clone()),
                ConnectInfo(test_addr()),
                Query(PairQuery {
                    t: Some("PAIR1234".to_string()),
                    label: None,
                }),
            )
            .await
        });
        let cookie_header = pair_response
            .headers()
            .get(header::SET_COOKIE)
            .expect("pair response should set auth cookie")
            .to_str()
            .expect("cookie should be utf-8")
            .split(';')
            .next()
            .expect("cookie name/value")
            .to_string();
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            cookie_header.parse().expect("cookie header"),
        );

        let response = runtime.block_on(async { me_handler(State(state), headers).await });
        drop(runtime);

        assert_eq!(response.status(), StatusCode::OK);
        let set_cookie = response
            .headers()
            .get(header::SET_COOKIE)
            .expect("me response should refresh auth cookie")
            .to_str()
            .expect("cookie should be utf-8");
        assert!(
            set_cookie.contains("Max-Age=315360000"),
            "expected refreshed 10-year Max-Age, got: {set_cookie}"
        );
    }

    #[test]
    fn pair_handler_uses_distinct_cookie_names_per_server_id() {
        let _profile = TestProfileGuard::new("web-cookie-isolation");
        let service_a = test_service("host-a");
        let state_a = test_state(&service_a);
        let service_b = test_service("host-b");
        let state_b = test_state(&service_b);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");

        let response_a = runtime.block_on(async {
            pair_handler(
                State(state_a),
                ConnectInfo(test_addr()),
                Query(PairQuery {
                    t: Some("PAIR1234".to_string()),
                    label: None,
                }),
            )
            .await
        });
        let response_b = runtime.block_on(async {
            pair_handler(
                State(state_b),
                ConnectInfo(test_addr()),
                Query(PairQuery {
                    t: Some("PAIR1234".to_string()),
                    label: None,
                }),
            )
            .await
        });
        drop(runtime);

        let cookie_name_a = response_a
            .headers()
            .get(header::SET_COOKIE)
            .expect("pair response should set cookie for host a")
            .to_str()
            .expect("cookie should be utf-8")
            .split('=')
            .next()
            .expect("cookie name")
            .to_string();
        let cookie_name_b = response_b
            .headers()
            .get(header::SET_COOKIE)
            .expect("pair response should set cookie for host b")
            .to_str()
            .expect("cookie should be utf-8")
            .split('=')
            .next()
            .expect("cookie name")
            .to_string();

        assert_ne!(
            cookie_name_a, cookie_name_b,
            "different server ids should mint different cookie names"
        );
    }

    #[test]
    fn pair_handler_persists_paired_client_immediately() {
        let _profile = TestProfileGuard::new("web-persist");
        let mut disk_state = RemoteMachineState::default();
        disk_state.host.web.enabled = true;
        disk_state.host.web.pairing_token = "PAIR1234".to_string();
        disk_state.known_hosts.push(KnownRemoteHost {
            label: "Other host".to_string(),
            address: "example.local".to_string(),
            port: 43871,
            server_id: "other-host".to_string(),
            certificate_fingerprint: "fingerprint".to_string(),
            client_id: "client-1".to_string(),
            auth_token: "token-1".to_string(),
            last_connected_epoch_ms: Some(1),
        });
        save_remote_machine_state(&disk_state).expect("seed remote state");

        let service = RemoteHostService::new(disk_state.host.clone());
        let state = test_state(&service);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");

        let response = runtime.block_on(async {
            pair_handler(
                State(state),
                ConnectInfo(test_addr()),
                Query(PairQuery {
                    t: Some("PAIR1234".to_string()),
                    label: Some("Phone".to_string()),
                }),
            )
            .await
        });
        drop(runtime);

        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let saved = load_remote_machine_state().expect("load persisted remote state");
        assert_eq!(saved.host.web.paired_clients.len(), 1);
        assert_eq!(saved.host.web.paired_clients[0].label, "Phone");
        assert_eq!(saved.known_hosts.len(), 1);
        assert_eq!(saved.known_hosts[0].server_id, "other-host");
    }

    #[test]
    fn me_handler_rejects_cookie_when_paired_client_is_removed() {
        let _profile = TestProfileGuard::new("web-cookie-revoke");
        let service = test_service("host-a");
        let state = test_state(&service);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");

        let pair_response = runtime.block_on(async {
            pair_handler(
                State(state.clone()),
                ConnectInfo(test_addr()),
                Query(PairQuery {
                    t: Some("PAIR1234".to_string()),
                    label: None,
                }),
            )
            .await
        });
        let cookie_header = pair_response
            .headers()
            .get(header::SET_COOKIE)
            .expect("pair response should set auth cookie")
            .to_str()
            .expect("cookie should be utf-8")
            .split(';')
            .next()
            .expect("cookie name/value")
            .to_string();
        if let Ok(mut config) = state.inner.config.write() {
            config.web.paired_clients.clear();
        }

        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            cookie_header.parse().expect("cookie header"),
        );
        let response = runtime.block_on(async { me_handler(State(state), headers).await });
        drop(runtime);

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn me_handler_rejects_cookie_from_different_server_id() {
        let _profile = TestProfileGuard::new("web-cookie-cross-server");
        let service_a = test_service("host-a");
        let state_a = test_state(&service_a);
        let service_b = test_service("host-b");
        let state_b = test_state(&service_b);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");

        let pair_response_b = runtime.block_on(async {
            pair_handler(
                State(state_b),
                ConnectInfo(test_addr()),
                Query(PairQuery {
                    t: Some("PAIR1234".to_string()),
                    label: None,
                }),
            )
            .await
        });
        let cookie_header = pair_response_b
            .headers()
            .get(header::SET_COOKIE)
            .expect("pair response should set auth cookie")
            .to_str()
            .expect("cookie should be utf-8")
            .split(';')
            .next()
            .expect("cookie name/value")
            .to_string();

        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            cookie_header.parse().expect("cookie header"),
        );
        let response = runtime.block_on(async { me_handler(State(state_a), headers).await });
        drop(runtime);

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
}
