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

use super::{
    now_epoch_ms, RemoteAccessActivityEvent, RemoteAccessActivityKind, RemoteAccessSource,
    RemoteHostInner,
};

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
    pub activity_log: Vec<RemoteAccessActivityEvent>,
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
            activity_log: Vec::new(),
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

#[derive(Debug, Clone)]
struct BrowserClientMetadata {
    label: String,
    user_agent: Option<String>,
    browser_family: Option<String>,
    browser_version: Option<String>,
    os_family: Option<String>,
    device_class: Option<String>,
}

fn browser_metadata_from_headers(headers: &HeaderMap) -> BrowserClientMetadata {
    let user_agent = headers
        .get(header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);
    let lower = user_agent
        .as_deref()
        .map(|value| value.to_ascii_lowercase())
        .unwrap_or_default();

    let (browser_family, browser_version) = if lower.contains("edg/") {
        (
            Some("Edge".to_string()),
            extract_user_agent_version(user_agent.as_deref(), "Edg/"),
        )
    } else if lower.contains("opr/") || lower.contains("opera") {
        (
            Some("Opera".to_string()),
            extract_user_agent_version(user_agent.as_deref(), "OPR/"),
        )
    } else if lower.contains("firefox/") {
        (
            Some("Firefox".to_string()),
            extract_user_agent_version(user_agent.as_deref(), "Firefox/"),
        )
    } else if lower.contains("chrome/") && !lower.contains("edg/") && !lower.contains("opr/") {
        (
            Some("Chrome".to_string()),
            extract_user_agent_version(user_agent.as_deref(), "Chrome/"),
        )
    } else if lower.contains("safari/")
        && lower.contains("version/")
        && !lower.contains("chrome/")
        && !lower.contains("chromium/")
    {
        (
            Some("Safari".to_string()),
            extract_user_agent_version(user_agent.as_deref(), "Version/"),
        )
    } else {
        (None, None)
    };

    let (device_label, os_family, device_class) = if lower.contains("iphone") {
        (
            Some("iPhone".to_string()),
            Some("iOS".to_string()),
            Some("phone".to_string()),
        )
    } else if lower.contains("ipad") {
        (
            Some("iPad".to_string()),
            Some("iOS".to_string()),
            Some("tablet".to_string()),
        )
    } else if lower.contains("android") && lower.contains("mobile") {
        (
            Some("Android Phone".to_string()),
            Some("Android".to_string()),
            Some("phone".to_string()),
        )
    } else if lower.contains("android") {
        (
            Some("Android Tablet".to_string()),
            Some("Android".to_string()),
            Some("tablet".to_string()),
        )
    } else if lower.contains("windows") {
        (
            Some("Windows".to_string()),
            Some("Windows".to_string()),
            Some("desktop".to_string()),
        )
    } else if lower.contains("macintosh") || lower.contains("mac os x") {
        (
            Some("Mac".to_string()),
            Some("macOS".to_string()),
            Some("desktop".to_string()),
        )
    } else if lower.contains("linux") {
        (
            Some("Linux".to_string()),
            Some("Linux".to_string()),
            Some("desktop".to_string()),
        )
    } else {
        (None, None, None)
    };

    let label = match (device_label.as_deref(), browser_family.as_deref()) {
        (Some(device), Some(browser)) => format!("{device} {browser}"),
        (Some(device), None) => device.to_string(),
        (None, Some(browser)) => browser.to_string(),
        (None, None) => "Browser".to_string(),
    };

    BrowserClientMetadata {
        label,
        user_agent,
        browser_family,
        browser_version,
        os_family,
        device_class,
    }
}

fn extract_user_agent_version(user_agent: Option<&str>, marker: &str) -> Option<String> {
    let user_agent = user_agent?;
    let marker_idx = user_agent.find(marker)?;
    let version = &user_agent[marker_idx + marker.len()..];
    let end = version
        .find(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '.' || ch == '_'))
        .unwrap_or(version.len());
    let trimmed = version[..end].trim_matches('.');
    (!trimmed.is_empty()).then(|| trimmed.replace('_', "."))
}

fn browser_display_label(client: &PairedWebClient) -> String {
    client
        .nickname
        .clone()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| {
            if client.label.trim().is_empty() {
                "Browser".to_string()
            } else {
                client.label.clone()
            }
        })
}

pub(crate) fn record_browser_connection(
    inner: &Arc<RemoteHostInner>,
    client_id: &str,
    client_ip: IpAddr,
    browser_install_id: Option<String>,
    headers: &HeaderMap,
) -> Result<(), String> {
    let metadata = browser_metadata_from_headers(headers);
    let now = now_epoch_ms();
    let client_ip_string = client_ip.to_string();
    super::mutate_host_config(inner, |config| {
        let had_previous_connect = config.web.activity_log.iter().any(|event| {
            event.source == RemoteAccessSource::Browser
                && event.client_id == client_id
                && matches!(
                    event.event_kind,
                    RemoteAccessActivityKind::Connected | RemoteAccessActivityKind::Reconnected
                )
        });
        let Some(client_index) = config
            .web
            .paired_clients
            .iter()
            .position(|client| client.client_id == client_id)
        else {
            return;
        };

        let normalized_browser_install_id = browser_install_id
            .filter(|value| !value.trim().is_empty())
            .map(|value| value.trim().to_string());
        let (
            event_client_id,
            event_label,
            browser_family,
            browser_version,
            os_family,
            device_class,
        ) = {
            let client = &mut config.web.paired_clients[client_index];
            if let Some(browser_install_id) = normalized_browser_install_id {
                if client.browser_install_id.trim().is_empty()
                    || client.browser_install_id == client.client_id
                {
                    client.browser_install_id = browser_install_id;
                }
            }
            client.last_seen_epoch_ms = Some(now);
            client.last_seen_ip = Some(client_ip_string.clone());
            client.label = metadata.label.clone();
            client.user_agent = metadata.user_agent.clone();
            client.browser_family = metadata.browser_family.clone();
            client.browser_version = metadata.browser_version.clone();
            client.os_family = metadata.os_family.clone();
            client.device_class = metadata.device_class.clone();
            (
                client.client_id.clone(),
                browser_display_label(client),
                client.browser_family.clone(),
                client.browser_version.clone(),
                client.os_family.clone(),
                client.device_class.clone(),
            )
        };

        super::append_remote_access_activity_event(
            config,
            RemoteAccessActivityEvent {
                client_id: event_client_id,
                source: RemoteAccessSource::Browser,
                event_kind: if had_previous_connect {
                    RemoteAccessActivityKind::Reconnected
                } else {
                    RemoteAccessActivityKind::Connected
                },
                label: event_label,
                ip_address: Some(client_ip_string.clone()),
                event_at_epoch_ms: Some(now),
                browser_family,
                browser_version,
                os_family,
                device_class,
            },
        );
    })
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

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct PairQuery {
    t: Option<String>,
    label: Option<String>,
    browser_install_id: Option<String>,
}

/// `/pair?t=<web_pairing_token>&label=<optional phone name>`
///
/// Validates the token, mints a new `PairedWebClient` plus a signed cookie,
/// and redirects to `/`. On failure returns 401 with a short message (no
/// redirect, so users see what went wrong).
async fn pair_handler(
    State(state): State<Arc<WebState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
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

    let nickname = query
        .label
        .filter(|l| !l.is_empty())
        .map(|label| label.trim().to_string())
        .filter(|label| !label.is_empty());
    let metadata = browser_metadata_from_headers(&headers);
    let now = now_epoch_ms();
    let client_ip_string = client_ip.to_string();

    let browser_install_id = query
        .browser_install_id
        .filter(|value| !value.trim().is_empty())
        .map(|value| value.trim().to_string());
    let client_id = match super::mutate_host_config(&state.inner, |config| {
        let client_id = if let Some(browser_install_id) = browser_install_id.as_deref() {
            if let Some(existing) = config
                .web
                .paired_clients
                .iter_mut()
                .find(|client| client.browser_install_id == browser_install_id)
            {
                existing.last_seen_epoch_ms = Some(now);
                existing.last_seen_ip = Some(client_ip_string.clone());
                existing.label = metadata.label.clone();
                existing.user_agent = metadata.user_agent.clone();
                existing.browser_family = metadata.browser_family.clone();
                existing.browser_version = metadata.browser_version.clone();
                existing.os_family = metadata.os_family.clone();
                existing.device_class = metadata.device_class.clone();
                if nickname.is_some() {
                    existing.nickname = nickname.clone();
                }
                existing.client_id.clone()
            } else {
                let client_id = format!("web-{}", now_epoch_ms());
                config.web.paired_clients.push(PairedWebClient {
                    client_id: client_id.clone(),
                    browser_install_id: browser_install_id.to_string(),
                    nickname: nickname.clone(),
                    label: metadata.label.clone(),
                    issued_at_epoch_ms: Some(now),
                    last_seen_epoch_ms: Some(now),
                    last_seen_ip: Some(client_ip_string.clone()),
                    user_agent: metadata.user_agent.clone(),
                    browser_family: metadata.browser_family.clone(),
                    browser_version: metadata.browser_version.clone(),
                    os_family: metadata.os_family.clone(),
                    device_class: metadata.device_class.clone(),
                });
                client_id
            }
        } else {
            let client_id = format!("web-{}", now_epoch_ms());
            config.web.paired_clients.push(PairedWebClient {
                client_id: client_id.clone(),
                browser_install_id: client_id.clone(),
                nickname: nickname.clone(),
                label: metadata.label.clone(),
                issued_at_epoch_ms: Some(now),
                last_seen_epoch_ms: Some(now),
                last_seen_ip: Some(client_ip_string.clone()),
                user_agent: metadata.user_agent.clone(),
                browser_family: metadata.browser_family.clone(),
                browser_version: metadata.browser_version.clone(),
                os_family: metadata.os_family.clone(),
                device_class: metadata.device_class.clone(),
            });
            client_id
        };

        super::append_remote_access_activity_event(
            config,
            RemoteAccessActivityEvent {
                client_id: client_id.clone(),
                source: RemoteAccessSource::Browser,
                event_kind: RemoteAccessActivityKind::Paired,
                label: config
                    .web
                    .paired_clients
                    .iter()
                    .find(|client| client.client_id == client_id)
                    .map(browser_display_label)
                    .unwrap_or_else(|| metadata.label.clone()),
                ip_address: Some(client_ip_string.clone()),
                event_at_epoch_ms: Some(now),
                browser_family: metadata.browser_family.clone(),
                browser_version: metadata.browser_version.clone(),
                os_family: metadata.os_family.clone(),
                device_class: metadata.device_class.clone(),
            },
        );
        client_id
    }) {
        Ok(client_id) => client_id,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to persist web pairing: {error}"),
            )
                .into_response();
        }
    };

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

    fn test_headers(user_agent: Option<&str>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if let Some(user_agent) = user_agent {
            headers.insert(
                header::USER_AGENT,
                user_agent.parse().expect("user agent header"),
            );
        }
        headers
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
                test_headers(None),
                Query(PairQuery {
                    t: Some("PAIR1234".to_string()),
                    label: None,
                    browser_install_id: None,
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
                test_headers(None),
                Query(PairQuery {
                    t: Some("PAIR1234".to_string()),
                    label: None,
                    browser_install_id: None,
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
                test_headers(None),
                Query(PairQuery {
                    t: Some("PAIR1234".to_string()),
                    label: None,
                    browser_install_id: None,
                }),
            )
            .await
        });
        let response_b = runtime.block_on(async {
            pair_handler(
                State(state_b),
                ConnectInfo(test_addr()),
                test_headers(None),
                Query(PairQuery {
                    t: Some("PAIR1234".to_string()),
                    label: None,
                    browser_install_id: None,
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
                test_headers(Some(
                    "Mozilla/5.0 (iPhone; CPU iPhone OS 17_4 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.4 Mobile/15E148 Safari/604.1",
                )),
                Query(PairQuery {
                    t: Some("PAIR1234".to_string()),
                    label: Some("Phone".to_string()),
                    browser_install_id: Some("browser-install-1".to_string()),
                }),
            )
            .await
        });
        drop(runtime);

        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let saved = load_remote_machine_state().expect("load persisted remote state");
        assert_eq!(saved.host.web.paired_clients.len(), 1);
        assert_eq!(
            saved.host.web.paired_clients[0].nickname.as_deref(),
            Some("Phone")
        );
        assert_eq!(saved.known_hosts.len(), 1);
        assert_eq!(saved.known_hosts[0].server_id, "other-host");
    }

    #[test]
    fn pair_handler_records_browser_activity_with_ip_and_metadata() {
        let _profile = TestProfileGuard::new("web-browser-activity-pair");
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
                test_headers(Some(
                    "Mozilla/5.0 (iPhone; CPU iPhone OS 17_4 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.4 Mobile/15E148 Safari/604.1",
                )),
                Query(PairQuery {
                    t: Some("PAIR1234".to_string()),
                    label: None,
                    browser_install_id: Some("browser-install-activity".to_string()),
                }),
            )
            .await
        });
        drop(runtime);

        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let config = service.config();
        assert_eq!(config.web.activity_log.len(), 1);
        let event = &config.web.activity_log[0];
        assert_eq!(event.source, RemoteAccessSource::Browser);
        assert_eq!(event.event_kind, RemoteAccessActivityKind::Paired);
        assert_eq!(event.label, "iPhone Safari");
        assert_eq!(event.ip_address.as_deref(), Some("127.0.0.1"));
        assert_eq!(event.browser_family.as_deref(), Some("Safari"));
        assert_eq!(event.os_family.as_deref(), Some("iOS"));
        assert_eq!(event.device_class.as_deref(), Some("phone"));
    }

    #[test]
    fn pair_handler_reuses_existing_browser_identity_for_same_install_id() {
        let _profile = TestProfileGuard::new("web-dedupe");
        let service = test_service("host-a");
        let state = test_state(&service);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");

        let user_agent = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
             (KHTML, like Gecko) Chrome/135.0.0.0 Safari/537.36";

        let first = runtime.block_on(async {
            pair_handler(
                State(state.clone()),
                ConnectInfo(test_addr()),
                test_headers(Some(user_agent)),
                Query(PairQuery {
                    t: Some("PAIR1234".to_string()),
                    label: None,
                    browser_install_id: Some("work-browser".to_string()),
                }),
            )
            .await
        });
        let second = runtime.block_on(async {
            pair_handler(
                State(state),
                ConnectInfo(SocketAddr::from(([127, 0, 0, 2], 43872))),
                test_headers(Some(user_agent)),
                Query(PairQuery {
                    t: Some("PAIR1234".to_string()),
                    label: None,
                    browser_install_id: Some("work-browser".to_string()),
                }),
            )
            .await
        });
        drop(runtime);

        assert_eq!(first.status(), StatusCode::SEE_OTHER);
        assert_eq!(second.status(), StatusCode::SEE_OTHER);

        let config = service.config();
        assert_eq!(config.web.paired_clients.len(), 1);
        assert_eq!(
            config.web.paired_clients[0].browser_install_id,
            "work-browser"
        );
        assert_eq!(
            config.web.paired_clients[0].last_seen_ip.as_deref(),
            Some("127.0.0.2")
        );
        assert_eq!(config.web.activity_log.len(), 2);
    }

    #[test]
    fn browser_activity_log_trims_to_recent_limit() {
        let _profile = TestProfileGuard::new("web-browser-activity-trim");
        let service = test_service("host-a");
        let result = crate::remote::mutate_host_config(&service.inner, |config| {
            for index in 0..(crate::remote::REMOTE_ACCESS_LOG_LIMIT + 5) {
                crate::remote::append_remote_access_activity_event(
                    config,
                    RemoteAccessActivityEvent {
                        client_id: format!("browser-{index}"),
                        source: RemoteAccessSource::Browser,
                        event_kind: RemoteAccessActivityKind::Connected,
                        label: format!("Browser {index}"),
                        ip_address: Some(format!("10.0.0.{index}")),
                        event_at_epoch_ms: Some(index as u64),
                        browser_family: Some("Chrome".to_string()),
                        browser_version: Some("135".to_string()),
                        os_family: Some("Windows".to_string()),
                        device_class: Some("desktop".to_string()),
                    },
                );
            }
            config.web.activity_log.clone()
        })
        .expect("mutate host config");

        assert_eq!(result.len(), crate::remote::REMOTE_ACCESS_LOG_LIMIT);
        assert_eq!(
            result.first().and_then(|event| event.event_at_epoch_ms),
            Some(5)
        );
    }

    #[test]
    fn record_browser_connection_marks_repeat_connect_as_reconnected() {
        let _profile = TestProfileGuard::new("web-browser-activity-connect");
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
                test_headers(Some(
                    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/135.0.0.0 Safari/537.36",
                )),
                Query(PairQuery {
                    t: Some("PAIR1234".to_string()),
                    label: None,
                    browser_install_id: Some("browser-install-connect".to_string()),
                }),
            )
            .await
        });
        drop(runtime);

        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let client_id = service.config().web.paired_clients[0].client_id.clone();

        super::record_browser_connection(
            &service.inner,
            &client_id,
            IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2)),
            Some("browser-install-connect".to_string()),
            &test_headers(Some(
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/135.0.0.0 Safari/537.36",
            )),
        )
        .expect("first browser connection");
        super::record_browser_connection(
            &service.inner,
            &client_id,
            IpAddr::V4(Ipv4Addr::new(127, 0, 0, 3)),
            Some("browser-install-connect".to_string()),
            &test_headers(Some(
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/135.0.0.0 Safari/537.36",
            )),
        )
        .expect("second browser connection");

        let config = service.config();
        let kinds: Vec<RemoteAccessActivityKind> = config
            .web
            .activity_log
            .iter()
            .map(|event| event.event_kind.clone())
            .collect();
        assert_eq!(
            kinds,
            vec![
                RemoteAccessActivityKind::Paired,
                RemoteAccessActivityKind::Connected,
                RemoteAccessActivityKind::Reconnected,
            ]
        );
        assert_eq!(
            config.web.paired_clients[0].last_seen_ip.as_deref(),
            Some("127.0.0.3")
        );
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
                test_headers(None),
                Query(PairQuery {
                    t: Some("PAIR1234".to_string()),
                    label: None,
                    browser_install_id: None,
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
                test_headers(None),
                Query(PairQuery {
                    t: Some("PAIR1234".to_string()),
                    label: None,
                    browser_install_id: None,
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
