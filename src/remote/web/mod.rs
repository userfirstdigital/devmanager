pub mod action;
pub mod assets;
pub mod auth;
pub mod bridge;
pub(crate) mod command_catalog;
pub mod dto;
pub mod image_paste;
pub(crate) mod input_executor;
pub mod lease;
pub mod push;
pub(crate) mod request_executor;
pub mod wire;

use self::auth::{generate_web_client_id, PairingAttemptTracker, PairingThrottleStatus};
use self::command_catalog::{
    discover_slash_commands, DiscoveredSlashCommand, DiscoveryLimits, SlashCommandProvider,
};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::sync::{Arc, Weak};
use std::time::Instant;

use axum::body::Bytes;
use axum::extract::{ConnectInfo, DefaultBodyLimit, Query, Request, State};
use axum::http::{header, uri::Authority, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::Router;
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

use super::{
    now_epoch_ms, remote_worker_admission_pool, ListenerBindFailure, ListenerLease,
    RemoteAccessActivityEvent, RemoteAccessActivityKind, RemoteAccessSource, RemoteHostConfig,
    RemoteHostInner, RemoteWorkerAdmissionPool, RemoteWorkerPermit,
};
use crate::browser::redact_browser_text;
use crate::remote::presentation::StableSessionKey;
use crate::state::SessionKind;

pub use auth::{
    cookie_name_for_server_id, extract_cookie, generate_cookie_secret_hex,
    generate_web_pairing_token, sign_cookie, verify_cookie, PairedWebClient, WEB_COOKIE_NAME,
};

const WEB_COOKIE_MAX_AGE_SECS: u64 = 60 * 60 * 24 * 365 * 10;
const PUSH_REGISTRATION_BODY_BYTES: usize = 16 * 1024;
const MAX_BROWSER_INSTALL_ID_BYTES: usize = 128;
const MAX_BROWSER_USER_AGENT_BYTES: usize = 512;
const MAX_BROWSER_NICKNAME_BYTES: usize = 128;

/// Persisted configuration for the legacy same-origin remote web listener.
/// Lives inside `RemoteHostConfig` and is serialized to `remote.json` via serde
/// defaults. This listener is not a Connect production path: it does not
/// instantiate Noise/sealed-frame Connect sessions. Production Connect uses
/// [`crate::connect::ConnectProductionStartup`].
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
    pub push: push::WebPushConfig,
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
            push: push::WebPushConfig::default(),
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
        let cookie_secret_is_valid =
            auth::hex_decode(&self.cookie_secret_hex).is_some_and(|secret| secret.len() == 32);
        if !cookie_secret_is_valid {
            self.cookie_secret_hex = generate_cookie_secret_hex();
        }
        if self.bind_address.is_empty() {
            self.bind_address = "0.0.0.0".to_string();
        }
        if self.port == 0 {
            self.port = 43872;
        }
        self.push.ensure_keys();
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

fn browser_metadata_character_is_forbidden(character: char) -> bool {
    character.is_control()
        || matches!(
            character,
            '\u{200E}'
                | '\u{200F}'
                | '\u{202A}'..='\u{202E}'
                | '\u{2066}'..='\u{2069}'
        )
}

fn truncate_utf8_bytes(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].trim_end().to_string()
}

fn sanitize_browser_metadata_text(value: &str, max_bytes: usize) -> Option<String> {
    let redacted = redact_browser_text(value);
    let normalized = redacted
        .chars()
        .map(|character| {
            if browser_metadata_character_is_forbidden(character) {
                ' '
            } else {
                character
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let bounded = truncate_utf8_bytes(&normalized, max_bytes);
    (!bounded.is_empty()).then_some(bounded)
}

fn validate_browser_install_id(value: Option<String>) -> Result<Option<String>, String> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.is_empty() {
        return Ok(None);
    }
    if value.len() > MAX_BROWSER_INSTALL_ID_BYTES {
        return Err(format!(
            "Browser install ID exceeds {MAX_BROWSER_INSTALL_ID_BYTES} bytes."
        ));
    }
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
    {
        return Err(
            "Browser install ID must be an opaque ASCII identifier without whitespace or metadata."
                .to_string(),
        );
    }
    Ok(Some(value))
}

fn browser_metadata_from_headers(headers: &HeaderMap) -> BrowserClientMetadata {
    let user_agent = headers
        .get(header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| sanitize_browser_metadata_text(value, MAX_BROWSER_USER_AGENT_BYTES));
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

#[derive(Debug, Clone)]
pub(super) struct BrowserConnectionActivity {
    client_id: String,
    client_ip: String,
    browser_install_id: Option<String>,
    metadata: BrowserClientMetadata,
}

pub(super) fn prepare_browser_connection_activity(
    client_id: &str,
    client_ip: IpAddr,
    browser_install_id: Option<String>,
    headers: &HeaderMap,
) -> Result<BrowserConnectionActivity, String> {
    Ok(BrowserConnectionActivity {
        client_id: client_id.to_string(),
        client_ip: client_ip.to_string(),
        browser_install_id: validate_browser_install_id(browser_install_id)?,
        metadata: browser_metadata_from_headers(headers),
    })
}

pub(super) fn apply_browser_connection_activity(
    config: &mut RemoteHostConfig,
    activity: &BrowserConnectionActivity,
    occurred_at_epoch_ms: u64,
) -> bool {
    let client_id = activity.client_id.as_str();
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
        return false;
    };

    let (event_client_id, event_label, browser_family, browser_version, os_family, device_class) = {
        let client = &mut config.web.paired_clients[client_index];
        client.nickname = client.nickname.as_deref().and_then(|nickname| {
            sanitize_browser_metadata_text(nickname.trim(), MAX_BROWSER_NICKNAME_BYTES)
        });
        if let Some(browser_install_id) = activity.browser_install_id.as_ref() {
            if client.browser_install_id.trim().is_empty()
                || client.browser_install_id == client.client_id
            {
                client.browser_install_id = browser_install_id.clone();
            }
        }
        client.last_seen_epoch_ms = Some(occurred_at_epoch_ms);
        client.last_seen_ip = Some(activity.client_ip.clone());
        client.label = activity.metadata.label.clone();
        client.user_agent = activity.metadata.user_agent.clone();
        client.browser_family = activity.metadata.browser_family.clone();
        client.browser_version = activity.metadata.browser_version.clone();
        client.os_family = activity.metadata.os_family.clone();
        client.device_class = activity.metadata.device_class.clone();
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
            ip_address: Some(activity.client_ip.clone()),
            event_at_epoch_ms: Some(occurred_at_epoch_ms),
            browser_family,
            browser_version,
            os_family,
            device_class,
        },
    );
    true
}

pub(crate) fn record_browser_connection(
    inner: &Arc<RemoteHostInner>,
    client_id: &str,
    client_ip: IpAddr,
    browser_install_id: Option<String>,
    headers: &HeaderMap,
) -> Result<(), String> {
    let activity =
        prepare_browser_connection_activity(client_id, client_ip, browser_install_id, headers)?;
    super::mutate_host_config_if(
        inner,
        |config| {
            config
                .web
                .paired_clients
                .iter()
                .any(|client| client.client_id == activity.client_id)
        },
        |config| {
            let applied = apply_browser_connection_activity(config, &activity, now_epoch_ms());
            debug_assert!(applied, "serialized browser activity client disappeared");
        },
    )
    .map_err(|error| error.to_string())?
    .ok_or_else(|| "Browser pairing is no longer valid.".to_string())
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
    shutdown_permit: Option<RemoteWorkerPermit>,
    push_inner: std::sync::Weak<RemoteHostInner>,
    push_dispatcher: Option<push::PushDispatcher>,
    push_sender: Option<push::PushSender>,
    /// The production Connect session is owned by the listener generation.
    /// Keeping it here prevents startup custody from being prepared and then
    /// immediately dropped while the route opens a second, untracked session.
    connect_startup: Option<std::sync::Arc<crate::connect::ConnectProductionStartup>>,
    host_requests: crate::connect::ConnectHostRequestSlot,
    listener_generation: u64,
    pub bind_info: String,
}

fn publish_web_push_sender(
    inner: &Arc<RemoteHostInner>,
    listener_generation: u64,
    sender: push::PushSender,
) {
    if let Ok(mut slot) = inner.web_push_sender.write() {
        *slot = Some(super::RegisteredWebPushSender {
            listener_generation,
            sender,
        });
    }
}

fn clear_web_push_sender_if_current(
    inner: &Arc<RemoteHostInner>,
    listener_generation: u64,
    expected: &push::PushSender,
) -> bool {
    let Ok(mut slot) = inner.web_push_sender.write() else {
        return false;
    };
    let matches = slot.as_ref().is_some_and(|registered| {
        registered.listener_generation == listener_generation
            && registered.sender.belongs_to_same_dispatcher(expected)
    });
    if matches {
        *slot = None;
    }
    matches
}

fn reserve_web_listener_shutdown_permit(
    worker_pool: &Arc<RemoteWorkerAdmissionPool>,
    bind: &str,
) -> Result<RemoteWorkerPermit, ListenerBindFailure> {
    worker_pool
        .try_acquire()
        .ok_or_else(|| ListenerBindFailure::Other {
            bind: bind.to_string(),
            detail: "web listener cleanup admission is exhausted".to_string(),
        })
}

impl WebListenerHandle {
    pub(crate) fn start(
        inner: Arc<RemoteHostInner>,
        config: WebConfig,
        lease: ListenerLease,
    ) -> Result<Self, ListenerBindFailure> {
        Self::start_with_worker_pool(inner, config, lease, remote_worker_admission_pool())
    }

    fn start_with_worker_pool(
        inner: Arc<RemoteHostInner>,
        config: WebConfig,
        lease: ListenerLease,
        worker_pool: Arc<RemoteWorkerAdmissionPool>,
    ) -> Result<Self, ListenerBindFailure> {
        let listener_generation = lease.generation;
        let bind = format!("{}:{}", config.bind_address, config.port);
        let shutdown_permit = reserve_web_listener_shutdown_permit(&worker_pool, &bind)?;
        let runtime = match tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("devmanager-web")
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => {
                shutdown_permit.release();
                return Err(ListenerBindFailure::Other {
                    bind,
                    detail: format!("failed to build tokio runtime: {error}"),
                });
            }
        };

        let bind_info = bind.clone();
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let (bind_result_tx, bind_result_rx) =
            std::sync::mpsc::channel::<Result<(), ListenerBindFailure>>();

        let connect_startup = match crate::connect::ConnectProductionStartup::prepare_direct(
            crate::connect::DirectBindPolicy::loopback(),
        ) {
            Ok(startup) => Some(std::sync::Arc::new(startup)),
            Err(error) => {
                eprintln!("[remote-web] Connect production startup held closed: {error}");
                None
            }
        };
        let host_requests = crate::connect::process_connect_host_request_slot();
        let router_state = Arc::new(WebState {
            inner: Arc::downgrade(&inner),
            listener_generation,
            pairing_attempts: Arc::new(std::sync::Mutex::new(PairingAttemptTracker::default())),
            connect_startup: connect_startup.clone(),
            host_requests: host_requests.clone(),
        });

        // /api/ws remains the legacy same-origin UI. /api/connect is the
        // production Connect route registered on this same TCP bind.
        let _ = crate::connect::ConnectProductionStartup::reject_legacy_remote_web_as_connect();
        inner
            .connect_encryption_required
            .store(true, std::sync::atomic::Ordering::Release);
        runtime.spawn(async move {
            let app = build_router(router_state);
            if !lease.is_current() {
                let _ = bind_result_tx.send(Err(ListenerBindFailure::GenerationStale {
                    bind: bind.clone(),
                    phase: "before",
                }));
                return;
            }
            match tokio::net::TcpListener::bind(&bind).await {
                Ok(listener) => {
                    if !lease.is_current() {
                        let _ = bind_result_tx.send(Err(ListenerBindFailure::GenerationStale {
                            bind: bind.clone(),
                            phase: "after",
                        }));
                        return;
                    }
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
                    let _ = bind_result_tx.send(Err(ListenerBindFailure::from_io(bind, error)));
                }
            }
        });

        match bind_result_rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(Ok(())) => {
                let push_inner = Arc::downgrade(&inner);
                let (push_dispatcher, push_sender) =
                    match push::PushDispatcher::start(push_inner.clone()) {
                        Ok(dispatcher) => {
                            let sender = dispatcher.sender();
                            (Some(dispatcher), Some(sender))
                        }
                        Err(error) => {
                            eprintln!("[remote-web] Web Push delivery disabled: {error}");
                            (None, None)
                        }
                    };
                if let Some(startup) = connect_startup.as_ref() {
                    startup.mark_listener_bound();
                }
                Ok(Self {
                    runtime: Some(runtime),
                    shutdown_tx: Some(shutdown_tx),
                    shutdown_permit: Some(shutdown_permit),
                    push_inner,
                    push_dispatcher,
                    push_sender,
                    connect_startup,
                    host_requests,
                    listener_generation,
                    bind_info,
                })
            }
            Ok(Err(error)) => {
                let _ = shutdown_tx.send(());
                drop(runtime);
                shutdown_permit.release();
                Err(error)
            }
            Err(_) => {
                let _ = shutdown_tx.send(());
                drop(runtime);
                shutdown_permit.release();
                Err(ListenerBindFailure::Other {
                    bind: bind_info,
                    detail: "web listener failed to report bind status in time".to_string(),
                })
            }
        }
    }

    pub(super) fn take_shutdown_permit(&mut self) -> RemoteWorkerPermit {
        self.shutdown_permit
            .take()
            .expect("active web listener must retain cleanup admission")
    }

    pub(crate) fn publish_push_sender(&self) {
        let (Some(inner), Some(sender)) = (self.push_inner.upgrade(), self.push_sender.clone())
        else {
            return;
        };
        publish_web_push_sender(&inner, self.listener_generation, sender);
    }

    /// Attach the durable host's one [`crate::host::HostRequestHandle`].
    /// `start` clones the process slot into both this handle and live
    /// [`WebState`]; attach writes that shared slot so `/api/connect` observes
    /// it after start. Same-process only.
    pub fn attach_host_requests(&self, handle: crate::host::HostRequestHandle) {
        self.host_requests.attach(handle);
    }

    /// Attach the narrow Connect request lane. The listener does not own an
    /// executor; this is only a lifecycle-safe reference to the host bridge.
    pub fn attach_host_executor(
        &self,
        executor: std::sync::Arc<dyn crate::client::ConnectHostCommandPort>,
    ) {
        self.host_requests.attach_executor(executor);
    }

    pub fn shutdown(mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        self.stop_push_dispatcher();
        if let Some(runtime) = self.runtime.take() {
            // Drop in a blocking context. tokio's Runtime::drop blocks the
            // calling thread until outstanding tasks finish, which is what we
            // want here — we are called from a std thread, not from inside
            // the runtime itself.
            drop(runtime);
        }
        if let Some(permit) = self.shutdown_permit.take() {
            permit.release();
        }
    }

    fn stop_push_dispatcher(&mut self) {
        if let (Some(inner), Some(sender)) = (self.push_inner.upgrade(), self.push_sender.take()) {
            clear_web_push_sender_if_current(&inner, self.listener_generation, &sender);
        }
        self.push_dispatcher.take();
    }
}

impl Drop for WebListenerHandle {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        self.stop_push_dispatcher();
        if let Some(runtime) = self.runtime.take() {
            drop(runtime);
        }
        if let Some(permit) = self.shutdown_permit.take() {
            permit.release();
        }
    }
}

#[derive(Clone)]
pub(crate) struct WebState {
    pub(crate) inner: Weak<RemoteHostInner>,
    pub(crate) listener_generation: u64,
    pub(crate) pairing_attempts: Arc<std::sync::Mutex<PairingAttemptTracker>>,
    pub(crate) connect_startup: Option<std::sync::Arc<crate::connect::ConnectProductionStartup>>,
    pub(crate) host_requests: crate::connect::ConnectHostRequestSlot,
}

impl WebState {
    fn upgrade_inner(&self) -> Option<Arc<RemoteHostInner>> {
        self.inner.upgrade()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WebAuthError {
    Unauthorized,
    Durability,
}

fn web_auth_error_response(error: WebAuthError) -> Response {
    match error {
        WebAuthError::Unauthorized => (StatusCode::UNAUTHORIZED, "not paired").into_response(),
        WebAuthError::Durability => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "authentication state unavailable",
        )
            .into_response(),
    }
}

fn build_router(state: Arc<WebState>) -> Router {
    Router::new()
        .route("/", get(assets::index_handler))
        .route("/pair", get(pair_handler))
        .route("/api/health", get(health_handler))
        .route("/api/me", get(me_handler))
        .route("/api/slash-commands", get(slash_commands_handler))
        .route(
            "/api/push",
            get(push_status_handler).post(push_subscribe_handler),
        )
        .route("/api/push/unsubscribe", post(push_unsubscribe_handler))
        .route("/api/ws", get(bridge::ws_handler))
        .route("/api/connect", get(bridge::connect_ws_handler))
        .route("/*path", get(assets::static_handler))
        .layer(DefaultBodyLimit::max(PUSH_REGISTRATION_BODY_BYTES))
        .layer(middleware::from_fn(web_response_policy))
        .with_state(state)
}

fn is_dynamic_web_path(path: &str) -> bool {
    path == "/api" || path.starts_with("/api/") || path == "/pair" || path.starts_with("/pair/")
}

async fn web_response_policy(request: Request, next: Next) -> Response {
    let dynamic = is_dynamic_web_path(request.uri().path());
    let websocket_authority = request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<Authority>().ok())
        .map(|authority| authority.to_string());
    let mut response = next.run(request).await;

    if dynamic {
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    if response
        .headers()
        .contains_key(header::CONTENT_SECURITY_POLICY)
    {
        response.headers_mut().insert(
            header::CONTENT_SECURITY_POLICY,
            assets::content_security_policy(websocket_authority.as_deref()),
        );
    }
    response
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
    let Some(inner) = state.upgrade_inner() else {
        return (StatusCode::INTERNAL_SERVER_ERROR, "host unavailable").into_response();
    };
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

    let nickname = query
        .label
        .and_then(|label| sanitize_browser_metadata_text(label.trim(), MAX_BROWSER_NICKNAME_BYTES));
    let metadata = browser_metadata_from_headers(&headers);
    let now = now_epoch_ms();
    let client_ip_string = client_ip.to_string();

    let browser_install_id = match validate_browser_install_id(query.browser_install_id) {
        Ok(browser_install_id) => browser_install_id,
        Err(error) => return (StatusCode::BAD_REQUEST, error).into_response(),
    };
    let paired = match super::mutate_host_config_if(
        &inner,
        |config| config.web.enabled && provided == config.web.pairing_token,
        |config| {
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
                    existing.nickname = nickname.clone().or_else(|| {
                        existing.nickname.as_deref().and_then(|nickname| {
                            sanitize_browser_metadata_text(
                                nickname.trim(),
                                MAX_BROWSER_NICKNAME_BYTES,
                            )
                        })
                    });
                    existing.client_id.clone()
                } else {
                    let client_id = generate_web_client_id();
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
                let client_id = generate_web_client_id();
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
            (
                client_id,
                config.web.cookie_secret_hex.clone(),
                cookie_name_for_server_id(&config.server_id),
            )
        },
    ) {
        Ok(Some(paired)) => paired,
        Ok(None) => {
            let web_enabled = match inner.config.read() {
                Ok(config) => config.web.enabled,
                Err(_) => {
                    return (StatusCode::INTERNAL_SERVER_ERROR, "config unavailable")
                        .into_response();
                }
            };
            if !web_enabled {
                return (StatusCode::FORBIDDEN, "web UI disabled").into_response();
            }
            let throttle = state
                .pairing_attempts
                .lock()
                .ok()
                .map(|mut pairing_attempts| {
                    pairing_attempts.record_failure(client_ip, Instant::now())
                })
                .unwrap_or(PairingThrottleStatus::Allowed);
            return pair_token_rejected_response(throttle);
        }
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to persist web pairing: {error}"),
            )
                .into_response();
        }
    };
    let (client_id, cookie_secret_hex, cookie_name) = paired;

    if let Ok(mut pairing_attempts) = state.pairing_attempts.lock() {
        pairing_attempts.record_success(client_ip);
    }

    let Some(signed) = sign_cookie(&cookie_secret_hex, &client_id) else {
        return (StatusCode::INTERNAL_SERVER_ERROR, "cookie signing failed").into_response();
    };

    let cookie = auth_cookie_header(&cookie_name, &signed, &headers);

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

fn auth_cookie_header(cookie_name: &str, signed: &str, headers: &HeaderMap) -> String {
    let mut cookie = format!(
        "{}={}; HttpOnly; SameSite=Lax; Path=/; Max-Age={}",
        cookie_name, signed, WEB_COOKIE_MAX_AGE_SECS,
    );
    if single_request_header(headers, "x-forwarded-proto")
        .ok()
        .flatten()
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("https"))
    {
        cookie.push_str("; Secure");
    }
    cookie
}

fn request_auth_cookie(
    inner: &Arc<RemoteHostInner>,
    headers: &HeaderMap,
) -> Result<(String, String), WebAuthError> {
    let cookie_header = headers
        .get(header::COOKIE)
        .ok_or(WebAuthError::Unauthorized)?
        .to_str()
        .map_err(|_| WebAuthError::Unauthorized)?;
    let current_cookie_name = {
        let config = inner.config.read().map_err(|_| WebAuthError::Durability)?;
        cookie_name_for_server_id(&config.server_id)
    };
    let cookie_value = extract_cookie(cookie_header, &current_cookie_name)
        .or_else(|| extract_cookie(cookie_header, WEB_COOKIE_NAME))
        .ok_or(WebAuthError::Unauthorized)?;
    Ok((current_cookie_name, cookie_value))
}

/// `/api/me` — returns 200 with the paired-client id if the dm_web cookie is
/// valid, 401 otherwise. Small endpoint used by the SPA on load to decide
/// whether to show the "not paired yet" screen or start connecting.
async fn me_handler(State(state): State<Arc<WebState>>, headers: HeaderMap) -> Response {
    match authenticate_request(&state, &headers) {
        Ok(client_id) => {
            let mut response = (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "application/json")],
                format!(r#"{{"clientId":{:?},"ok":true}}"#, client_id),
            )
                .into_response();
            if let Some(inner) = state.upgrade_inner() {
                if let Ok((cookie_name, cookie_value)) = request_auth_cookie(&inner, &headers) {
                    let cookie = auth_cookie_header(&cookie_name, &cookie_value, &headers);
                    if let Ok(value) = cookie.parse() {
                        response.headers_mut().insert(header::SET_COOKIE, value);
                    }
                }
            }
            response
        }
        Err(WebAuthError::Unauthorized) => (
            StatusCode::UNAUTHORIZED,
            [(header::CONTENT_TYPE, "application/json")],
            r#"{"ok":false}"#,
        )
            .into_response(),
        Err(WebAuthError::Durability) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "authentication state unavailable",
        )
            .into_response(),
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct SlashCommandQuery {
    session_key: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SlashCommandResponse {
    provider: SlashCommandProvider,
    commands: Vec<DiscoveredSlashCommand>,
}

async fn slash_commands_handler(
    State(state): State<Arc<WebState>>,
    headers: HeaderMap,
    Query(query): Query<SlashCommandQuery>,
) -> Response {
    if let Err(error) = authenticate_request(&state, &headers) {
        return web_auth_error_response(error);
    }
    let Some(session_key) = query
        .session_key
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty() && value.len() <= 256)
    else {
        return (StatusCode::BAD_REQUEST, "invalid session key").into_response();
    };

    let Some(inner) = state.upgrade_inner() else {
        return web_auth_error_response(WebAuthError::Durability);
    };
    let resolved = {
        let app = match inner.shared_state.read() {
            Ok(app) => app,
            Err(_) => {
                return (StatusCode::INTERNAL_SERVER_ERROR, "workspace unavailable").into_response()
            }
        };
        let runtime = match inner.runtime_state.read() {
            Ok(runtime) => runtime,
            Err(_) => {
                return (StatusCode::INTERNAL_SERVER_ERROR, "runtime unavailable").into_response()
            }
        };
        let mut matches = runtime.sessions.values().filter(|session| {
            StableSessionKey::resolve(session, &app.open_tabs)
                .as_ref()
                .is_some_and(|key| key.as_str() == session_key)
        });
        let Some(session) = matches.next() else {
            return (StatusCode::NOT_FOUND, "AI session not found").into_response();
        };
        if matches.next().is_some() {
            return (StatusCode::CONFLICT, "session key is ambiguous").into_response();
        }
        let provider = match session.session_kind {
            SessionKind::Claude => SlashCommandProvider::Claude,
            SessionKind::Codex => SlashCommandProvider::Codex,
            _ => return (StatusCode::NOT_FOUND, "AI session not found").into_response(),
        };
        let project_root = session.project_id.as_deref().and_then(|project_id| {
            app.config
                .projects
                .iter()
                .find(|project| project.id == project_id)
                .map(|project| PathBuf::from(&project.root_path))
        });
        (provider, project_root, session.cwd.clone())
    };
    let (provider, project_root, session_cwd) = resolved;
    let commands = discover_slash_commands(
        provider,
        project_root.as_deref(),
        &session_cwd,
        dirs::home_dir().as_deref(),
        DiscoveryLimits::default(),
    );
    let body = match serde_json::to_string(&SlashCommandResponse { provider, commands }) {
        Ok(body) => body,
        Err(_) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, "catalog unavailable").into_response()
        }
    };
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        body,
    )
        .into_response()
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PushStatusResponse {
    public_key: String,
    enabled: bool,
    /// Compatibility alias for the first notification-capable web bundle.
    subscribed: bool,
}

#[derive(Serialize)]
struct PushMutationResponse {
    enabled: bool,
}

fn single_request_header<'a>(
    headers: &'a HeaderMap,
    name: &'static str,
) -> Result<Option<&'a str>, ()> {
    let mut values = headers.get_all(name).iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(());
    }
    let value = value.to_str().map_err(|_| ())?.trim();
    if value.is_empty() || value.contains(',') {
        return Err(());
    }
    Ok(Some(value))
}

pub(crate) fn request_is_same_origin(headers: &HeaderMap) -> bool {
    let Ok(Some(origin)) = single_request_header(headers, "origin") else {
        return false;
    };
    let Ok(origin) = origin.parse::<axum::http::Uri>() else {
        return false;
    };
    let (Some(origin_scheme), Some(origin_authority)) = (origin.scheme_str(), origin.authority())
    else {
        return false;
    };
    if !matches!(origin_scheme, "http" | "https")
        || origin_authority.as_str().contains('@')
        || origin
            .path_and_query()
            .is_some_and(|path| path.as_str() != "/")
    {
        return false;
    }

    let effective_authority = match single_request_header(headers, "x-forwarded-host") {
        Ok(Some(authority)) => authority,
        Ok(None) => match single_request_header(headers, "host") {
            Ok(Some(authority)) => authority,
            _ => return false,
        },
        Err(()) => return false,
    };
    let Ok(effective_authority) = effective_authority.parse::<Authority>() else {
        return false;
    };

    // The listener itself is HTTP. A trusted HTTPS proxy must overwrite the
    // standard forwarding headers, as it already does for WebSocket routing.
    let effective_scheme = match single_request_header(headers, "x-forwarded-proto") {
        Ok(Some(scheme))
            if scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https") =>
        {
            scheme
        }
        Ok(Some(_)) | Err(()) => return false,
        Ok(None) => "http",
    };
    if !origin_scheme.eq_ignore_ascii_case(effective_scheme) {
        return false;
    }
    let default_port = if origin_scheme.eq_ignore_ascii_case("https") {
        443
    } else {
        80
    };
    if effective_authority.as_str().contains('@')
        || !origin_authority
            .host()
            .eq_ignore_ascii_case(effective_authority.host())
        || origin_authority.port_u16().unwrap_or(default_port)
            != effective_authority.port_u16().unwrap_or(default_port)
    {
        return false;
    }
    true
}

fn validate_push_mutation_request(headers: &HeaderMap) -> Result<(), StatusCode> {
    let content_type = single_request_header(headers, "content-type")
        .map_err(|_| StatusCode::UNSUPPORTED_MEDIA_TYPE)?
        .ok_or(StatusCode::UNSUPPORTED_MEDIA_TYPE)?;
    let media_type = content_type
        .split(';')
        .next()
        .map(str::trim)
        .unwrap_or_default();
    if !media_type.eq_ignore_ascii_case("application/json") {
        return Err(StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }
    if !request_is_same_origin(headers) {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(())
}

async fn push_status_handler(State(state): State<Arc<WebState>>, headers: HeaderMap) -> Response {
    let client_id = match authenticate_request(&state, &headers) {
        Ok(client_id) => client_id,
        Err(error) => return web_auth_error_response(error),
    };
    let Some(inner) = state.upgrade_inner() else {
        return web_auth_error_response(WebAuthError::Durability);
    };
    let Ok(config) = inner.config.read() else {
        return (StatusCode::INTERNAL_SERVER_ERROR, "config unavailable").into_response();
    };
    let enabled = config.web.push.notifications_enabled(&client_id);
    let response = PushStatusResponse {
        public_key: config.web.push.vapid_public_key_base64.clone(),
        enabled,
        subscribed: enabled,
    };
    match serde_json::to_vec(&response) {
        Ok(body) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            body,
        )
            .into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "encoding failed").into_response(),
    }
}

async fn push_subscribe_handler(
    State(state): State<Arc<WebState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let client_id = match authenticate_request(&state, &headers) {
        Ok(client_id) => client_id,
        Err(error) => return web_auth_error_response(error),
    };
    if let Err(status) = validate_push_mutation_request(&headers) {
        return status.into_response();
    }
    let request = match serde_json::from_slice::<push::PushRegistrationRequest>(&body) {
        Ok(request) => request,
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid subscription").into_response(),
    };
    let mode = request.mode;
    let validated = match push::validate_registration(request) {
        Ok(validated) => validated,
        Err(error) => return (StatusCode::BAD_REQUEST, error).into_response(),
    };
    let Some(inner) = state.upgrade_inner() else {
        return web_auth_error_response(WebAuthError::Durability);
    };
    let registered = match super::mutate_host_config(&inner, |config| {
        if !config
            .web
            .paired_clients
            .iter()
            .any(|client| client.client_id == client_id)
        {
            return None;
        }
        let enabled = match mode {
            push::PushRegistrationMode::Enable => config
                .web
                .push
                .enable_and_replace_subscription(&client_id, validated, now_epoch_ms())
                .map(|()| true),
            push::PushRegistrationMode::Reconcile => Ok(config
                .web
                .push
                .reconcile_and_replace_subscription(&client_id, validated, now_epoch_ms())),
        };
        Some(enabled)
    }) {
        Ok(registered) => registered,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "subscription save failed",
            )
                .into_response()
        }
    };
    let Some(enabled) = registered else {
        return (StatusCode::UNAUTHORIZED, "not paired").into_response();
    };
    let enabled = match enabled {
        Ok(enabled) => enabled,
        Err(push::PushEnableError::ClientLimitReached) => {
            return (StatusCode::CONFLICT, "notification client limit reached").into_response()
        }
    };
    match serde_json::to_vec(&PushMutationResponse { enabled }) {
        Ok(body) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            body,
        )
            .into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "encoding failed").into_response(),
    }
}

async fn push_unsubscribe_handler(
    State(state): State<Arc<WebState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let client_id = match authenticate_request(&state, &headers) {
        Ok(client_id) => client_id,
        Err(error) => return web_auth_error_response(error),
    };
    if let Err(status) = validate_push_mutation_request(&headers) {
        return status.into_response();
    }
    let request = match serde_json::from_slice::<push::PushUnsubscribeRequest>(&body) {
        Ok(request) => request,
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid subscription").into_response(),
    };
    if !request.disable && request.endpoint.is_none() {
        return (StatusCode::BAD_REQUEST, "missing subscription endpoint").into_response();
    }
    if let Some(endpoint) = request.endpoint.as_deref() {
        if let Err(error) = push::validate_push_endpoint(endpoint) {
            return (StatusCode::BAD_REQUEST, error).into_response();
        }
    }
    let Some(inner) = state.upgrade_inner() else {
        return web_auth_error_response(WebAuthError::Durability);
    };
    match super::mutate_host_config(&inner, |config| {
        let legacy_endpoint_matches = request.endpoint.as_deref().is_some_and(|endpoint| {
            config.web.push.subscriptions.iter().any(|subscription| {
                subscription.client_id == client_id && subscription.endpoint == endpoint
            })
        });
        if request.disable || legacy_endpoint_matches {
            config.web.push.disable_client(&client_id);
        }
        true
    }) {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "subscription save failed",
        )
            .into_response(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ValidatedWebAuthentication {
    pub(super) client_id: String,
    pub(super) cookie_secret_hex: String,
}

/// Validates a browser cookie without mutating durable activity. WebSocket
/// admission carries this exact cookie-secret generation into its final
/// lifecycle fence and revalidates it there before recording the connection.
pub(super) fn validate_authenticated_request(
    state: &WebState,
    headers: &HeaderMap,
) -> Result<ValidatedWebAuthentication, WebAuthError> {
    let inner = state.upgrade_inner().ok_or(WebAuthError::Durability)?;
    let (_, cookie_value) = request_auth_cookie(&inner, headers)?;

    let (cookie_secret_hex, paired_ids) = {
        let config = inner.config.read().map_err(|_| WebAuthError::Durability)?;
        if !config.web.enabled {
            return Err(WebAuthError::Unauthorized);
        }
        let ids: Vec<String> = config
            .web
            .paired_clients
            .iter()
            .map(|client| client.client_id.clone())
            .collect();
        (config.web.cookie_secret_hex.clone(), ids)
    };

    let client_id =
        verify_cookie(&cookie_secret_hex, &cookie_value).ok_or(WebAuthError::Unauthorized)?;
    if !paired_ids.iter().any(|id| id == &client_id) {
        return Err(WebAuthError::Unauthorized);
    }
    Ok(ValidatedWebAuthentication {
        client_id,
        cookie_secret_hex,
    })
}

/// Shared HTTP helper: authenticates a browser cookie and durably advances the
/// paired client's `last_seen` timestamp before returning the client id.
///
/// A valid signature is not enough to authorize a request: if the host config
/// cannot be read or the `last_seen` update cannot be persisted, this returns
/// `WebAuthError::Durability` and callers fail closed with a server error.
pub(crate) fn authenticate_request(
    state: &WebState,
    headers: &HeaderMap,
) -> Result<String, WebAuthError> {
    let inner = state.upgrade_inner().ok_or(WebAuthError::Durability)?;
    let authentication = validate_authenticated_request(state, headers)?;
    let client_id = authentication.client_id;
    let cookie_secret_hex = authentication.cookie_secret_hex;
    let now = now_epoch_ms();
    let updated = super::mutate_host_config_if(
        &inner,
        |config| {
            config.web.enabled
                && config.web.cookie_secret_hex == cookie_secret_hex
                && config
                    .web
                    .paired_clients
                    .iter()
                    .any(|client| client.client_id == client_id)
        },
        |config| {
            if let Some(client) = config
                .web
                .paired_clients
                .iter_mut()
                .find(|client| client.client_id == client_id)
            {
                client.last_seen_epoch_ms = Some(now);
            }
        },
    )
    .map_err(|_| WebAuthError::Durability)?;
    if updated.is_none() {
        return Err(WebAuthError::Unauthorized);
    }
    Ok(client_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Project, SessionTab, TabType};
    use crate::remote::{
        load_remote_machine_state, save_remote_machine_state, test_support::TestProfileGuard,
        KnownRemoteHost, RemoteHostConfig, RemoteHostService, RemoteMachineState,
    };
    use crate::state::{AppState, RuntimeState, SessionKind, SessionRuntimeState};
    use crate::terminal::session::TerminalBackend;
    use axum::body::{to_bytes, Body};
    use base64::Engine as _;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tower::ServiceExt;

    fn test_service(server_id: &str) -> RemoteHostService {
        let mut config = RemoteHostConfig::default();
        config.server_id = server_id.to_string();
        config.web.enabled = true;
        config.web.pairing_token = "PAIR1234".to_string();
        RemoteHostService::new(config)
    }

    #[test]
    fn web_config_repairs_malformed_cookie_signing_secret() {
        let mut config = WebConfig::default();
        config.cookie_secret_hex = "not-a-32-byte-hex-secret".to_string();

        config.ensure_secrets();

        let decoded = auth::hex_decode(&config.cookie_secret_hex)
            .expect("repaired cookie secret should be hexadecimal");
        assert_eq!(decoded.len(), 32);
        assert!(sign_cookie(&config.cookie_secret_hex, "client").is_some());
    }

    #[test]
    fn web_listener_reserves_cleanup_admission_before_runtime_start() {
        let pool = Arc::new(RemoteWorkerAdmissionPool::new(1));
        let occupied = pool.try_acquire().expect("test admission");

        let failure = match reserve_web_listener_shutdown_permit(&pool, "127.0.0.1:0") {
            Err(failure) => failure,
            Ok(reserved) => {
                reserved.release();
                panic!("listener started without cleanup ownership")
            }
        };
        assert!(
            failure
                .to_string()
                .contains("cleanup admission is exhausted"),
            "typed listener failure should explain the lifecycle boundary"
        );
        assert_eq!(pool.in_use(), 1);

        occupied.release();
        let reserved = reserve_web_listener_shutdown_permit(&pool, "127.0.0.1:0")
            .expect("released admission should be reusable");
        assert_eq!(pool.in_use(), 1);
        reserved.release();
        assert_eq!(pool.in_use(), 0);
    }

    #[test]
    fn stale_web_listener_cleanup_preserves_newer_push_dispatcher_registration() {
        let service = test_service("web-push-registration-fence");
        let (old_tx, _old_rx) = std::sync::mpsc::sync_channel(1);
        let (new_tx, _new_rx) = std::sync::mpsc::sync_channel(1);
        let old_sender = push::PushSender::single(old_tx);
        let new_sender = push::PushSender::single(new_tx);
        publish_web_push_sender(&service.inner, 7, old_sender.clone());
        publish_web_push_sender(&service.inner, 8, new_sender.clone());

        assert!(!clear_web_push_sender_if_current(
            &service.inner,
            7,
            &old_sender,
        ));
        assert!(!clear_web_push_sender_if_current(
            &service.inner,
            8,
            &old_sender,
        ));
        let retained = service
            .inner
            .web_push_sender
            .read()
            .expect("push sender lock")
            .clone()
            .expect("newer push sender should remain");
        assert_eq!(retained.listener_generation, 8);
        assert!(retained.sender.belongs_to_same_dispatcher(&new_sender));
        assert!(clear_web_push_sender_if_current(
            &service.inner,
            8,
            &new_sender,
        ));
        assert!(service
            .inner
            .web_push_sender
            .read()
            .expect("push sender lock")
            .is_none());
    }

    #[test]
    fn web_state_does_not_retain_host_runtime_after_listener_shutdown() {
        let service = test_service("web-state-weak");
        let host = Arc::downgrade(&service.inner);
        let state = test_state(&service);

        drop(service);

        assert!(host.upgrade().is_none());
        assert!(state.inner.upgrade().is_none());
    }

    #[test]
    fn authenticated_request_fails_closed_when_last_seen_persistence_fails() {
        let _profile = TestProfileGuard::new("web-auth-last-seen-failure");
        let service = test_service("web-auth-last-seen-failure");
        let state = test_state(&service);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let headers = runtime.block_on(pair_cookie_headers(state.clone(), "durability-failure"));

        let mut persistence_hook = super::super::HOST_CONFIG_PERSISTENCE_TEST_HOOK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(
            persistence_hook.is_none(),
            "persistence hook leaked into test"
        );
        *persistence_hook = Some(Arc::new(|_, _| {
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "test persistence failure",
            ))
        }));
        drop(persistence_hook);

        let response = runtime.block_on(me_handler(State(state), headers));

        *super::super::HOST_CONFIG_PERSISTENCE_TEST_HOOK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        drop(runtime);

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    fn test_state(service: &RemoteHostService) -> Arc<WebState> {
        Arc::new(WebState {
            inner: Arc::downgrade(&service.inner),
            listener_generation: service
                .inner
                .native_runtime_generation
                .load(std::sync::atomic::Ordering::Acquire),
            pairing_attempts: Arc::new(std::sync::Mutex::new(PairingAttemptTracker::default())),
            connect_startup: None,
            host_requests: crate::connect::ConnectHostRequestSlot::new(),
        })
    }

    #[test]
    fn connect_route_is_not_the_legacy_websocket_and_rejects_cross_origin() {
        let _profile = TestProfileGuard::new("web-connect-route");
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let service = test_service("connect-route");
        assert!(service.connect_encryption_required());
        assert!(!service.status().connect_listener_bound);
        let state = test_state(&service);
        let missing_origin = runtime.block_on(route_response(state.clone(), "/api/connect"));
        assert_eq!(missing_origin.status(), StatusCode::FORBIDDEN);
        let legacy = runtime.block_on(route_response(state, "/api/ws"));
        assert_ne!(legacy.status(), StatusCode::NOT_FOUND);
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

    fn assert_persistable_browser_identifier(value: &str, max_bytes: usize) {
        assert!(
            value.len() <= max_bytes,
            "browser identifier exceeded its durable byte bound: {} > {max_bytes}",
            value.len()
        );
        assert!(
            !value.chars().any(|character| {
                character.is_control()
                    || matches!(
                        character,
                        '\u{200E}'
                            | '\u{200F}'
                            | '\u{202A}'..='\u{202E}'
                            | '\u{2066}'..='\u{2069}'
                    )
            }),
            "browser identifier retained control or bidi formatting characters: {value:?}"
        );
    }

    async fn route_response(state: Arc<WebState>, uri: &str) -> Response {
        build_router(state)
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .header(header::HOST, "devmanager.test:43872")
                    .extension(ConnectInfo(test_addr()))
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("router response")
    }

    async fn route_request(
        state: Arc<WebState>,
        method: axum::http::Method,
        uri: &str,
        headers: HeaderMap,
        body: Vec<u8>,
    ) -> Response {
        let mut request = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::HOST, "devmanager.test:43872")
            .extension(ConnectInfo(test_addr()))
            .body(Body::from(body))
            .expect("request");
        *request.headers_mut() = headers;
        request
            .headers_mut()
            .insert(header::HOST, "devmanager.test:43872".parse().unwrap());
        build_router(state)
            .oneshot(request)
            .await
            .expect("router response")
    }

    async fn pair_cookie_headers(state: Arc<WebState>, install_id: &str) -> HeaderMap {
        let inner = state.upgrade_inner().expect("host runtime");
        let pairing_token = inner
            .config
            .read()
            .expect("host config")
            .web
            .pairing_token
            .clone();
        let response = pair_handler(
            State(state),
            ConnectInfo(test_addr()),
            test_headers(None),
            Query(PairQuery {
                t: Some(pairing_token),
                label: None,
                browser_install_id: Some(install_id.to_string()),
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let cookie = response
            .headers()
            .get(header::SET_COOKIE)
            .expect("paired cookie")
            .to_str()
            .expect("cookie text")
            .split(';')
            .next()
            .expect("cookie value");
        let mut headers = HeaderMap::new();
        headers.insert(header::COOKIE, cookie.parse().expect("cookie header"));
        headers
    }

    fn push_mutation_headers(mut headers: HeaderMap) -> HeaderMap {
        headers.insert(
            header::ORIGIN,
            "http://devmanager.test:43872".parse().unwrap(),
        );
        headers.insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
        headers
    }

    fn valid_push_registration(service: &RemoteHostService, endpoint: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "endpoint": endpoint,
            "keys": {
                "p256dh": service.config().web.push.vapid_public_key_base64,
                "auth": base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([9_u8; 16]),
            }
        }))
        .expect("push registration")
    }

    fn slash_command_fixture(service: &RemoteHostService, kind: SessionKind) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "devmanager-slash-route-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(root.join(".claude/skills/project-check"))
            .expect("create command fixture");
        fs::write(
            root.join(".claude/skills/project-check/SKILL.md"),
            "---\nname: project-check\ndescription: Check this project.\n---\nPRIVATE BODY\n",
        )
        .expect("write command fixture");

        let mut app = AppState::default();
        app.config.projects = vec![Project {
            id: "project-1".to_string(),
            name: "Project One".to_string(),
            root_path: root.to_string_lossy().into_owned(),
            ..Project::default()
        }];
        app.open_tabs = vec![SessionTab {
            id: "ai-tab".to_string(),
            tab_type: match kind {
                SessionKind::Claude => TabType::Claude,
                SessionKind::Codex => TabType::Codex,
                _ => TabType::Server,
            },
            project_id: "project-1".to_string(),
            pty_session_id: Some("ai-pty".to_string()),
            provider_session_id: None,
            ..SessionTab::default()
        }];
        let mut runtime = RuntimeState::default();
        let mut session = SessionRuntimeState::new(
            "ai-pty",
            root.clone(),
            Default::default(),
            TerminalBackend::default(),
        );
        session.session_kind = kind;
        session.project_id = Some("project-1".to_string());
        session.tab_id = Some("ai-tab".to_string());
        runtime.sessions.insert("ai-pty".to_string(), session);
        service.update_snapshot(app, runtime, Default::default());
        root
    }

    #[test]
    fn slash_command_route_requires_pairing_and_returns_safe_live_provider_metadata() {
        let _profile = TestProfileGuard::new("web-slash-command-route");
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let service = test_service("slash-route");
        let fixture_root = slash_command_fixture(&service, SessionKind::Claude);
        let state = test_state(&service);

        let unauthorized = runtime.block_on(route_response(
            state.clone(),
            "/api/slash-commands?sessionKey=tab%3Aai-tab",
        ));
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

        let headers = runtime.block_on(pair_cookie_headers(state.clone(), "slash-browser"));
        let response = runtime.block_on(route_request(
            state,
            axum::http::Method::GET,
            "/api/slash-commands?sessionKey=tab%3Aai-tab",
            headers,
            Vec::new(),
        ));
        assert_eq!(response.status(), StatusCode::OK);
        let body = runtime
            .block_on(to_bytes(response.into_body(), 128 * 1024))
            .expect("catalog body");
        let value: serde_json::Value = serde_json::from_slice(&body).expect("catalog JSON");

        assert_eq!(value["provider"], "claude");
        let project_command = value["commands"]
            .as_array()
            .expect("command array")
            .iter()
            .find(|command| command["name"] == "/project-check")
            .expect("project command");
        assert_eq!(project_command["description"], "Check this project.");
        assert_eq!(project_command["source"], "project");
        let text = String::from_utf8(body.to_vec()).expect("UTF-8 body");
        assert!(!text.contains(fixture_root.to_string_lossy().as_ref()));
        assert!(!text.contains("PRIVATE BODY"));
        let _ = fs::remove_dir_all(fixture_root);
    }

    #[test]
    fn slash_command_route_rejects_unknown_and_non_ai_sessions() {
        let _profile = TestProfileGuard::new("web-slash-command-invalid-route");
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let service = test_service("slash-invalid-route");
        let fixture_root = slash_command_fixture(&service, SessionKind::Server);
        let state = test_state(&service);
        let headers = runtime.block_on(pair_cookie_headers(state.clone(), "slash-invalid-browser"));

        let unknown = runtime.block_on(route_request(
            state.clone(),
            axum::http::Method::GET,
            "/api/slash-commands?sessionKey=tab%3Amissing",
            headers.clone(),
            Vec::new(),
        ));
        assert_eq!(unknown.status(), StatusCode::NOT_FOUND);

        let non_ai = runtime.block_on(route_request(
            state,
            axum::http::Method::GET,
            "/api/slash-commands?sessionKey=tab%3Aai-tab",
            headers,
            Vec::new(),
        ));
        assert_eq!(non_ai.status(), StatusCode::NOT_FOUND);
        let _ = fs::remove_dir_all(fixture_root);
    }

    fn push_registration_with_mode(
        service: &RemoteHostService,
        endpoint: &str,
        mode: &str,
    ) -> Vec<u8> {
        let mut registration: serde_json::Value =
            serde_json::from_slice(&valid_push_registration(service, endpoint)).unwrap();
        registration["mode"] = serde_json::Value::String(mode.to_string());
        serde_json::to_vec(&registration).unwrap()
    }

    #[test]
    fn push_routes_require_pairing_and_never_expose_private_vapid_material() {
        let _profile = TestProfileGuard::new("web-push-auth");
        let service = test_service("host-push-auth");
        let state = test_state(&service);
        let private_key = service.config().web.push.vapid_private_key_base64.clone();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
            .block_on(async {
                let response = route_request(
                    state.clone(),
                    axum::http::Method::GET,
                    "/api/push",
                    HeaderMap::new(),
                    Vec::new(),
                )
                .await;
                assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

                let response = route_request(
                    state.clone(),
                    axum::http::Method::POST,
                    "/api/push",
                    HeaderMap::new(),
                    valid_push_registration(&service, "https://web.push.apple.com/QM-unauthorized"),
                )
                .await;
                assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

                let headers = pair_cookie_headers(state.clone(), "phone-auth").await;
                let response = route_request(
                    state,
                    axum::http::Method::GET,
                    "/api/push",
                    headers,
                    Vec::new(),
                )
                .await;
                assert_eq!(response.status(), StatusCode::OK);
                let body = to_bytes(response.into_body(), 16 * 1024)
                    .await
                    .expect("status body");
                let body = String::from_utf8(body.to_vec()).expect("status text");
                assert!(body.contains("publicKey"));
                assert!(!body.contains(&private_key));
                assert!(!body.contains("private"));
            });
    }

    #[test]
    fn push_subscription_is_bounded_validated_persisted_and_scoped_to_install() {
        let _profile = TestProfileGuard::new("web-push-registration");
        let service = test_service("host-push-registration");
        let state = test_state(&service);
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
            .block_on(async {
                let phone_headers =
                    push_mutation_headers(pair_cookie_headers(state.clone(), "phone-push").await);
                let tablet_headers =
                    push_mutation_headers(pair_cookie_headers(state.clone(), "tablet-push").await);
                let endpoint = "https://web.push.apple.com/QM-phone";

                let response = route_request(
                    state.clone(),
                    axum::http::Method::POST,
                    "/api/push",
                    phone_headers.clone(),
                    push_registration_with_mode(&service, endpoint, "enable"),
                )
                .await;
                assert_eq!(response.status(), StatusCode::OK);
                assert_eq!(service.config().web.push.subscriptions.len(), 1);

                let saved = load_remote_machine_state().expect("persisted push state");
                assert_eq!(saved.host.web.push.subscriptions.len(), 1);
                let phone_id = service
                    .config()
                    .web
                    .paired_clients
                    .iter()
                    .find(|client| client.browser_install_id == "phone-push")
                    .expect("paired phone")
                    .client_id
                    .clone();
                assert_eq!(saved.host.web.push.subscriptions[0].client_id, phone_id);

                let response = route_request(
                    state.clone(),
                    axum::http::Method::POST,
                    "/api/push/unsubscribe",
                    tablet_headers,
                    serde_json::to_vec(&serde_json::json!({ "endpoint": endpoint })).unwrap(),
                )
                .await;
                assert_eq!(response.status(), StatusCode::NO_CONTENT);
                assert_eq!(service.config().web.push.subscriptions.len(), 1);

                let response = route_request(
                    state.clone(),
                    axum::http::Method::POST,
                    "/api/push",
                    phone_headers.clone(),
                    vec![b'x'; PUSH_REGISTRATION_BODY_BYTES + 1],
                )
                .await;
                assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);

                let response = route_request(
                    state.clone(),
                    axum::http::Method::POST,
                    "/api/push",
                    phone_headers.clone(),
                    valid_push_registration(&service, "https://127.0.0.1/private"),
                )
                .await;
                assert_eq!(response.status(), StatusCode::BAD_REQUEST);

                for _ in 0..2 {
                    let response = route_request(
                        state.clone(),
                        axum::http::Method::POST,
                        "/api/push/unsubscribe",
                        phone_headers.clone(),
                        serde_json::to_vec(&serde_json::json!({ "endpoint": endpoint })).unwrap(),
                    )
                    .await;
                    assert_eq!(response.status(), StatusCode::NO_CONTENT);
                }
                assert!(service.config().web.push.subscriptions.is_empty());
            });
    }

    #[test]
    fn explicit_push_enable_sets_intent_and_registers_exact_endpoint() {
        let _profile = TestProfileGuard::new("web-push-explicit-enable");
        let service = test_service("host-push-explicit-enable");
        let state = test_state(&service);
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
            .block_on(async {
                let headers =
                    push_mutation_headers(pair_cookie_headers(state.clone(), "phone-enable").await);
                let endpoint = "https://web.push.apple.com/QM-phone-enabled";

                let response = route_request(
                    state,
                    axum::http::Method::POST,
                    "/api/push",
                    headers,
                    push_registration_with_mode(&service, endpoint, "enable"),
                )
                .await;

                assert_eq!(response.status(), StatusCode::OK);
                let body = to_bytes(response.into_body(), 16 * 1024).await.unwrap();
                assert_eq!(
                    serde_json::from_slice::<serde_json::Value>(&body).unwrap()["enabled"],
                    true
                );
                let saved = service.config();
                let client_id = &saved.web.paired_clients[0].client_id;
                assert!(saved.web.push.notifications_enabled(client_id));
                assert_eq!(saved.web.push.subscriptions.len(), 1);
                assert_eq!(saved.web.push.subscriptions[0].endpoint, endpoint);
            });
    }

    #[test]
    fn explicit_push_disable_clears_intent_and_every_client_endpoint() {
        let _profile = TestProfileGuard::new("web-push-explicit-disable");
        let service = test_service("host-push-explicit-disable");
        let state = test_state(&service);
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
            .block_on(async {
                let headers = push_mutation_headers(
                    pair_cookie_headers(state.clone(), "phone-disable").await,
                );
                let endpoint = "https://web.push.apple.com/QM-phone-disabled";

                let response = route_request(
                    state.clone(),
                    axum::http::Method::POST,
                    "/api/push",
                    headers.clone(),
                    push_registration_with_mode(&service, endpoint, "enable"),
                )
                .await;
                assert_eq!(response.status(), StatusCode::OK);

                let response = route_request(
                    state,
                    axum::http::Method::POST,
                    "/api/push/unsubscribe",
                    headers,
                    serde_json::to_vec(&serde_json::json!({ "disable": true })).unwrap(),
                )
                .await;

                assert_eq!(response.status(), StatusCode::NO_CONTENT);
                let saved = service.config();
                let client_id = &saved.web.paired_clients[0].client_id;
                assert!(!saved.web.push.notifications_enabled(client_id));
                assert!(saved
                    .web
                    .push
                    .subscriptions
                    .iter()
                    .all(|subscription| subscription.client_id != *client_id));
            });
    }

    #[test]
    fn push_status_follows_enabled_intent_even_when_the_endpoint_is_missing() {
        let _profile = TestProfileGuard::new("web-push-status-intent");
        let service = test_service("host-push-status-intent");
        let state = test_state(&service);
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
            .block_on(async {
                let paired = pair_cookie_headers(state.clone(), "phone-status-intent").await;
                let mutation_headers = push_mutation_headers(paired.clone());
                let response = route_request(
                    state.clone(),
                    axum::http::Method::POST,
                    "/api/push",
                    mutation_headers,
                    push_registration_with_mode(
                        &service,
                        "https://web.push.apple.com/QM-phone-status",
                        "enable",
                    ),
                )
                .await;
                assert_eq!(response.status(), StatusCode::OK);
                crate::remote::mutate_host_config(&service.inner, |config| {
                    config.web.push.subscriptions.clear();
                })
                .unwrap();

                let response = route_request(
                    state,
                    axum::http::Method::GET,
                    "/api/push",
                    paired,
                    Vec::new(),
                )
                .await;

                assert_eq!(response.status(), StatusCode::OK);
                let body = to_bytes(response.into_body(), 16 * 1024).await.unwrap();
                let status: serde_json::Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(status["enabled"], true);
                assert_eq!(status["subscribed"], true);
            });
    }

    #[test]
    fn delayed_reconcile_after_disable_cannot_resurrect_notifications() {
        let _profile = TestProfileGuard::new("web-push-disable-race");
        let service = test_service("host-push-disable-race");
        let state = test_state(&service);
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
            .block_on(async {
                let headers =
                    push_mutation_headers(pair_cookie_headers(state.clone(), "phone-race").await);
                let endpoint = "https://web.push.apple.com/QM-phone-race";
                let response = route_request(
                    state.clone(),
                    axum::http::Method::POST,
                    "/api/push",
                    headers.clone(),
                    push_registration_with_mode(&service, endpoint, "enable"),
                )
                .await;
                assert_eq!(response.status(), StatusCode::OK);
                let response = route_request(
                    state.clone(),
                    axum::http::Method::POST,
                    "/api/push/unsubscribe",
                    headers.clone(),
                    serde_json::to_vec(&serde_json::json!({ "disable": true })).unwrap(),
                )
                .await;
                assert_eq!(response.status(), StatusCode::NO_CONTENT);

                let response = route_request(
                    state,
                    axum::http::Method::POST,
                    "/api/push",
                    headers,
                    push_registration_with_mode(&service, endpoint, "reconcile"),
                )
                .await;

                assert_eq!(response.status(), StatusCode::OK);
                let body = to_bytes(response.into_body(), 16 * 1024).await.unwrap();
                assert_eq!(
                    serde_json::from_slice::<serde_json::Value>(&body).unwrap()["enabled"],
                    false
                );
                let saved = service.config();
                let client_id = &saved.web.paired_clients[0].client_id;
                assert!(!saved.web.push.notifications_enabled(client_id));
                assert!(saved.web.push.subscriptions.is_empty());
            });
    }

    #[test]
    fn push_mutations_require_same_origin_json_through_a_trusted_proxy() {
        let _profile = TestProfileGuard::new("web-push-csrf");
        let service = test_service("host-push-csrf");
        let state = test_state(&service);
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
            .block_on(async {
                let paired = pair_cookie_headers(state.clone(), "phone-csrf").await;
                let endpoint = "https://web.push.apple.com/QM-csrf";

                let mut text_headers = paired.clone();
                text_headers.insert(
                    header::ORIGIN,
                    "https://devmanager.test:43872".parse().unwrap(),
                );
                text_headers.insert(header::CONTENT_TYPE, "text/plain".parse().unwrap());
                let response = route_request(
                    state.clone(),
                    axum::http::Method::POST,
                    "/api/push",
                    text_headers,
                    valid_push_registration(&service, endpoint),
                )
                .await;
                assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
                assert!(service.config().web.push.subscriptions.is_empty());

                let mut cross_origin_headers = paired.clone();
                cross_origin_headers.insert(
                    header::ORIGIN,
                    "https://evil.devmanager.test:43872".parse().unwrap(),
                );
                cross_origin_headers
                    .insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
                let response = route_request(
                    state.clone(),
                    axum::http::Method::POST,
                    "/api/push",
                    cross_origin_headers,
                    valid_push_registration(&service, endpoint),
                )
                .await;
                assert_eq!(response.status(), StatusCode::FORBIDDEN);
                assert!(service.config().web.push.subscriptions.is_empty());

                let mut proxy_headers = paired;
                proxy_headers.insert(
                    header::ORIGIN,
                    "https://mobile.example.test".parse().unwrap(),
                );
                proxy_headers.insert(
                    header::CONTENT_TYPE,
                    "application/json; charset=utf-8".parse().unwrap(),
                );
                proxy_headers.insert(
                    "x-forwarded-host",
                    "mobile.example.test:443".parse().unwrap(),
                );
                proxy_headers.insert("x-forwarded-proto", "https".parse().unwrap());
                let response = route_request(
                    state.clone(),
                    axum::http::Method::POST,
                    "/api/push",
                    proxy_headers.clone(),
                    push_registration_with_mode(&service, endpoint, "enable"),
                )
                .await;
                assert_eq!(response.status(), StatusCode::OK);
                assert_eq!(service.config().web.push.subscriptions.len(), 1);

                proxy_headers.insert(header::CONTENT_TYPE, "text/plain".parse().unwrap());
                let response = route_request(
                    state,
                    axum::http::Method::POST,
                    "/api/push/unsubscribe",
                    proxy_headers,
                    serde_json::to_vec(&serde_json::json!({ "endpoint": endpoint })).unwrap(),
                )
                .await;
                assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
                assert_eq!(service.config().web.push.subscriptions.len(), 1);
            });
    }

    #[test]
    fn same_origin_request_validation_is_exact_and_fail_closed() {
        let mut direct = HeaderMap::new();
        direct.insert(header::HOST, "devmanager.test:43872".parse().unwrap());
        direct.insert(
            header::ORIGIN,
            "http://devmanager.test:43872".parse().unwrap(),
        );
        assert!(request_is_same_origin(&direct));

        let mut forwarded_https = HeaderMap::new();
        forwarded_https.insert(header::HOST, "127.0.0.1:43872".parse().unwrap());
        forwarded_https.insert(
            "x-forwarded-host",
            "mobile.example.test:443".parse().unwrap(),
        );
        forwarded_https.insert("x-forwarded-proto", "https".parse().unwrap());
        forwarded_https.insert(
            header::ORIGIN,
            "https://mobile.example.test".parse().unwrap(),
        );
        assert!(request_is_same_origin(&forwarded_https));

        let mut wrong_port = direct.clone();
        wrong_port.insert(
            header::ORIGIN,
            "http://devmanager.test:43873".parse().unwrap(),
        );
        assert!(!request_is_same_origin(&wrong_port));

        let mut missing_origin = direct.clone();
        missing_origin.remove(header::ORIGIN);
        assert!(!request_is_same_origin(&missing_origin));

        let mut malformed_origin = direct.clone();
        malformed_origin.insert(header::ORIGIN, "not-an-origin".parse().unwrap());
        assert!(!request_is_same_origin(&malformed_origin));

        let mut comma_joined_origin = direct.clone();
        comma_joined_origin.insert(
            header::ORIGIN,
            "http://devmanager.test:43872, http://evil.test"
                .parse()
                .unwrap(),
        );
        assert!(!request_is_same_origin(&comma_joined_origin));

        let mut duplicate_origin = direct.clone();
        duplicate_origin.append(
            header::ORIGIN,
            "http://devmanager.test:43872".parse().unwrap(),
        );
        assert!(!request_is_same_origin(&duplicate_origin));

        let mut duplicate_forwarding = forwarded_https;
        duplicate_forwarding.append("x-forwarded-proto", "https".parse().unwrap());
        assert!(!request_is_same_origin(&duplicate_forwarding));
    }

    #[test]
    fn dynamic_routes_are_no_store_on_success_errors_and_redirects() {
        let _profile = TestProfileGuard::new("web-dynamic-no-store");
        let service = test_service("host-no-store");
        let state = test_state(&service);
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
            .block_on(async {
                for uri in [
                    "/api/health",
                    "/api/me",
                    "/api/not-a-real-route",
                    "/api/ws",
                    "/pair",
                    "/pair?t=wrong",
                    "/pair?t=PAIR1234",
                    "/pair/unknown",
                ] {
                    let response = route_response(state.clone(), uri).await;
                    assert_eq!(
                        response
                            .headers()
                            .get(header::CACHE_CONTROL)
                            .and_then(|value| value.to_str().ok()),
                        Some("no-store"),
                        "{uri} returned {} without no-store",
                        response.status()
                    );
                }
            });
    }

    #[test]
    fn routed_static_csp_allows_only_the_request_host_for_websockets() {
        let service = test_service("host-csp");
        let state = test_state(&service);
        let response = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
            .block_on(route_response(state, "/"));
        let csp = response
            .headers()
            .get(header::CONTENT_SECURITY_POLICY)
            .expect("CSP")
            .to_str()
            .expect("CSP text");

        assert!(csp
            .contains("connect-src 'self' ws://devmanager.test:43872 wss://devmanager.test:43872"));
        assert!(!csp.contains(" ws: wss:"));
    }

    #[test]
    fn auth_cookie_header_preserves_raw_http_cookie_attributes() {
        let cookie = auth_cookie_header("dm_web_host", "signed-value", &HeaderMap::new());

        assert_eq!(
            cookie,
            "dm_web_host=signed-value; HttpOnly; SameSite=Lax; Path=/; Max-Age=315360000"
        );
    }

    #[test]
    fn auth_cookie_header_requires_one_valid_forwarded_https_scheme() {
        let mut https = HeaderMap::new();
        https.insert("x-forwarded-proto", "HTTPS".parse().unwrap());
        assert!(auth_cookie_header("dm_web", "signed", &https).ends_with("; Secure"));

        let mut http = HeaderMap::new();
        http.insert("x-forwarded-proto", "http".parse().unwrap());
        assert!(!auth_cookie_header("dm_web", "signed", &http).contains("; Secure"));

        let mut malformed = HeaderMap::new();
        malformed.insert("x-forwarded-proto", "https, http".parse().unwrap());
        assert!(!auth_cookie_header("dm_web", "signed", &malformed).contains("; Secure"));

        let mut duplicate = HeaderMap::new();
        duplicate.append("x-forwarded-proto", "https".parse().unwrap());
        duplicate.append("x-forwarded-proto", "https".parse().unwrap());
        assert!(!auth_cookie_header("dm_web", "signed", &duplicate).contains("; Secure"));
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
        assert!(!set_cookie.contains("; Secure"));
    }

    #[test]
    fn pair_handler_marks_forwarded_https_cookie_secure() {
        let _profile = TestProfileGuard::new("web-cookie-secure-pair");
        let service = test_service("host-secure-pair");
        let state = test_state(&service);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let mut headers = test_headers(None);
        headers.insert("x-forwarded-proto", "https".parse().unwrap());

        let response = runtime.block_on(route_request(
            state,
            axum::http::Method::GET,
            "/pair?t=PAIR1234",
            headers,
            Vec::new(),
        ));
        drop(runtime);

        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let set_cookie = response
            .headers()
            .get(header::SET_COOKIE)
            .expect("pair response should set auth cookie")
            .to_str()
            .expect("cookie should be utf-8");
        assert!(set_cookie.contains("; Secure"), "cookie was: {set_cookie}");
        assert!(set_cookie.contains("; HttpOnly; SameSite=Lax; Path=/;"));
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
        assert!(!set_cookie.contains("; Secure"));
    }

    #[test]
    fn me_handler_marks_forwarded_https_refresh_secure() {
        let _profile = TestProfileGuard::new("web-cookie-secure-refresh");
        let service = test_service("host-secure-refresh");
        let state = test_state(&service);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");

        let pair_response = runtime.block_on(route_request(
            state.clone(),
            axum::http::Method::GET,
            "/pair?t=PAIR1234",
            HeaderMap::new(),
            Vec::new(),
        ));
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
        headers.insert("x-forwarded-proto", "https".parse().unwrap());

        let response = runtime.block_on(route_request(
            state,
            axum::http::Method::GET,
            "/api/me",
            headers,
            Vec::new(),
        ));
        drop(runtime);

        assert_eq!(response.status(), StatusCode::OK);
        let set_cookie = response
            .headers()
            .get(header::SET_COOKIE)
            .expect("me response should refresh auth cookie")
            .to_str()
            .expect("cookie should be utf-8");
        assert!(set_cookie.contains("; Secure"), "cookie was: {set_cookie}");
        assert!(set_cookie.contains("; HttpOnly; SameSite=Lax; Path=/;"));
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
        assert_eq!(saved.host.web.pairing_token, "PAIR1234");
        assert_eq!(
            saved.host.web.paired_clients[0].nickname.as_deref(),
            Some("Phone")
        );
        assert_eq!(saved.known_hosts.len(), 1);
        assert_eq!(saved.known_hosts[0].server_id, "other-host");
    }

    #[test]
    fn browser_user_agent_is_redacted_and_bounded_before_activity_persistence() {
        let user_agent = format!(
            "Mozilla/5.0 Bearer ua-secret token=another-secret {}",
            "LongAgentSegment/123.456 ".repeat(48)
        );
        let activity = prepare_browser_connection_activity(
            "bounded-browser",
            "127.0.0.8".parse().expect("browser test address"),
            Some("exact-browser-install-id".to_string()),
            &test_headers(Some(&user_agent)),
        )
        .expect("valid exact browser install id");

        assert_eq!(
            activity.browser_install_id.as_deref(),
            Some("exact-browser-install-id")
        );

        let persisted_user_agent = activity
            .metadata
            .user_agent
            .as_deref()
            .expect("non-empty user agent");
        assert_persistable_browser_identifier(persisted_user_agent, 512);
        assert!(!persisted_user_agent.contains("ua-secret"));
        assert!(!persisted_user_agent.contains("another-secret"));
    }

    #[test]
    fn browser_install_id_rejects_unsafe_or_oversized_identity_without_truncation() {
        for invalid in [
            " leading-space".to_string(),
            "device?token=install-secret".to_string(),
            "x".repeat(MAX_BROWSER_INSTALL_ID_BYTES + 1),
        ] {
            let error = prepare_browser_connection_activity(
                "bounded-browser",
                "127.0.0.8".parse().expect("browser test address"),
                Some(invalid),
                &HeaderMap::new(),
            )
            .expect_err("unsafe browser identity must be rejected");
            assert!(error.contains("Browser install ID"), "{error}");
        }
    }

    #[test]
    fn pair_handler_rejects_invalid_browser_identity_without_persisting_a_pair() {
        let _profile = TestProfileGuard::new("web-pair-invalid-install-id");
        let service = test_service("invalid-install-id");
        let state = test_state(&service);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");

        let response = runtime.block_on(async {
            pair_handler(
                State(state),
                ConnectInfo(test_addr()),
                HeaderMap::new(),
                Query(PairQuery {
                    t: Some("PAIR1234".to_string()),
                    label: None,
                    browser_install_id: Some("x".repeat(MAX_BROWSER_INSTALL_ID_BYTES + 1)),
                }),
            )
            .await
        });
        drop(runtime);

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let path = crate::remote::remote_state_path().expect("isolated remote state path");
        if path.exists() {
            assert!(load_remote_machine_state()
                .expect("load rejected pair state")
                .host
                .web
                .paired_clients
                .is_empty());
        }
    }

    #[test]
    fn pair_handler_redacts_and_bounds_nickname_and_user_agent() {
        let _profile = TestProfileGuard::new("web-pair-bounded-metadata");
        let service = test_service("bounded-metadata");
        let state = test_state(&service);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let nickname = format!("Phone token=nickname-secret {}", "LongNickname ".repeat(32));
        let user_agent = format!(
            "Mozilla/5.0 Bearer pair-ua-secret token=pair-header-secret {}",
            "PairingAgent/987.654 ".repeat(48)
        );
        let response = runtime.block_on(async {
            pair_handler(
                State(state),
                ConnectInfo(test_addr()),
                test_headers(Some(&user_agent)),
                Query(PairQuery {
                    t: Some("PAIR1234".to_string()),
                    label: Some(nickname),
                    browser_install_id: Some("exact-pair-install-id".to_string()),
                }),
            )
            .await
        });
        drop(runtime);

        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let saved = load_remote_machine_state().expect("load bounded paired browser state");
        let paired = saved
            .host
            .web
            .paired_clients
            .first()
            .expect("paired browser should persist");
        assert_eq!(paired.browser_install_id, "exact-pair-install-id");
        let persisted_nickname = paired.nickname.as_deref().expect("bounded nickname");
        assert_persistable_browser_identifier(persisted_nickname, MAX_BROWSER_NICKNAME_BYTES);
        assert!(!persisted_nickname.contains("nickname-secret"));
        let persisted_user_agent = paired
            .user_agent
            .as_deref()
            .expect("paired browser should persist user agent metadata");
        assert_persistable_browser_identifier(persisted_user_agent, 512);
        assert!(!persisted_user_agent.contains("pair-ua-secret"));
        assert!(!persisted_user_agent.contains("pair-header-secret"));
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
    fn pair_handler_reuses_stable_invitation_for_multiple_browsers() {
        let _profile = TestProfileGuard::new("web-pair-stable-sequential");
        let service = test_service("host-a");
        let state = test_state(&service);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");

        let first = runtime.block_on(pair_handler(
            State(state.clone()),
            ConnectInfo(test_addr()),
            test_headers(None),
            Query(PairQuery {
                t: Some("PAIR1234".to_string()),
                label: None,
                browser_install_id: Some("phone-install".to_string()),
            }),
        ));
        let reused = runtime.block_on(pair_handler(
            State(state),
            ConnectInfo(SocketAddr::from(([127, 0, 0, 2], 43872))),
            test_headers(None),
            Query(PairQuery {
                t: Some("PAIR1234".to_string()),
                label: None,
                browser_install_id: Some("tablet-install".to_string()),
            }),
        ));
        drop(runtime);

        assert_eq!(first.status(), StatusCode::SEE_OTHER);
        assert_eq!(reused.status(), StatusCode::SEE_OTHER);
        let config = service.config();
        assert_eq!(config.web.paired_clients.len(), 2);
        assert_eq!(config.web.pairing_token, "PAIR1234");
    }

    #[test]
    fn pair_handler_accepts_concurrent_reuse_for_unique_browsers() {
        let _profile = TestProfileGuard::new("web-pair-stable-concurrent");
        let service = test_service("host-a");
        let state = test_state(&service);
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("test runtime");

        let mut statuses = runtime.block_on(async {
            let start = Arc::new(tokio::sync::Barrier::new(2));
            let mut requests = Vec::new();
            for (index, browser_install_id) in
                ["phone-install", "tablet-install"].into_iter().enumerate()
            {
                let state = state.clone();
                let start = start.clone();
                requests.push(tokio::spawn(async move {
                    start.wait().await;
                    pair_handler(
                        State(state),
                        ConnectInfo(SocketAddr::from(([127, 0, 0, (index + 1) as u8], 43872))),
                        test_headers(None),
                        Query(PairQuery {
                            t: Some("PAIR1234".to_string()),
                            label: None,
                            browser_install_id: Some(browser_install_id.to_string()),
                        }),
                    )
                    .await
                    .status()
                }));
            }

            let mut statuses = Vec::new();
            for request in requests {
                statuses.push(request.await.expect("pair request task"));
            }
            statuses
        });
        drop(runtime);

        statuses.sort_unstable();
        assert_eq!(statuses, [StatusCode::SEE_OTHER, StatusCode::SEE_OTHER]);
        let config = service.config();
        assert_eq!(config.web.paired_clients.len(), 2);
        assert_eq!(config.web.pairing_token, "PAIR1234");
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
        let fresh_token = service.config().web.pairing_token;
        let second = runtime.block_on(async {
            pair_handler(
                State(state),
                ConnectInfo(SocketAddr::from(([127, 0, 0, 2], 43872))),
                test_headers(Some(user_agent)),
                Query(PairQuery {
                    t: Some(fresh_token),
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
    fn pair_handler_mints_distinct_random_identity_for_each_browser_install() {
        let _profile = TestProfileGuard::new("web-random-client-ids");
        let service = test_service("host-a");
        let state = test_state(&service);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");

        for browser_install_id in ["phone-install", "desktop-install"] {
            let pairing_token = service.config().web.pairing_token;
            let response = runtime.block_on(async {
                pair_handler(
                    State(state.clone()),
                    ConnectInfo(test_addr()),
                    test_headers(None),
                    Query(PairQuery {
                        t: Some(pairing_token),
                        label: None,
                        browser_install_id: Some(browser_install_id.to_string()),
                    }),
                )
                .await
            });
            assert_eq!(response.status(), StatusCode::SEE_OTHER);
        }
        drop(runtime);

        let config = service.config();
        assert_eq!(config.web.paired_clients.len(), 2);
        let client_ids = config
            .web
            .paired_clients
            .iter()
            .map(|client| client.client_id.as_str())
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(client_ids.len(), 2);
        assert!(client_ids.iter().all(|client_id| {
            client_id.len() == 36
                && client_id.starts_with("web-")
                && client_id[4..].bytes().all(|byte| byte.is_ascii_hexdigit())
        }));
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
        if let Some(inner) = state.upgrade_inner() {
            if let Ok(mut config) = inner.config.write() {
                config.web.paired_clients.clear();
            }
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
