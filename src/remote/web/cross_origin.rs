//! Server-side cross-origin browser Connect pairing and resume.
//!
//! Phone pages hosted by PC A may pair/resume directly to PC B. Same-origin
//! cookie `/api/connect` and [`crate::connect::direct`] policy stay unchanged.
//! No credentialed CORS, plaintext LAN, URL tickets, or alternate command bus.

use std::collections::HashMap;
use std::fmt;
use std::net::IpAddr;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, DefaultBodyLimit, State};
use axum::http::uri::Authority;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, options, post};
use axum::{Extension, Json, Router};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::auth::{generate_web_client_id, hex_decode, PairedWebClient};
use super::bridge::{
    cross_origin_verified_tls_only, run_connect_session, ConnectSessionMode,
    CrossOriginTicketBinding, CONNECT_HANDSHAKE_TIMEOUT,
};
use super::connect_identity::{
    ConnectPeerPin, ConnectPeerPublicKey, CONNECT_PEER_PUBLIC_KEY_BYTES,
    CONNECT_PEER_PUBLIC_KEY_HEX_CHARS, MAX_CONNECT_PEER_PINS,
};
use super::{
    now_epoch_ms, validate_authenticated_request, validate_browser_install_id, WebAuthError,
    WebState,
};
use crate::domain::ClientId;
use crate::remote::blocking_work::RemoteBlockingWork;
use crate::remote::{
    mutate_host_config_if, RemoteAccessActivityEvent, RemoteAccessActivityKind, RemoteAccessSource,
};

pub(crate) const CROSS_ORIGIN_PAIR_BODY_BYTES: usize = 8192;
pub(crate) const MAX_CROSS_ORIGIN_GRANTS: usize = 4;
pub(crate) const MAX_CROSS_ORIGIN_TICKETS: usize = 64;
pub(crate) const MAX_CANONICAL_ORIGIN_BYTES: usize = 2048;
pub(crate) const MAX_CROSS_ORIGIN_RATE_IPS: usize = 1_024;
const GRANT_TTL: Duration = Duration::from_secs(5 * 60);
const TICKET_TTL: Duration = Duration::from_secs(60);
const RATE_ENTRY_TTL: Duration = Duration::from_secs(15 * 60);
const RATE_WINDOW: Duration = Duration::from_secs(60);
const RATE_MAX_ATTEMPTS_PER_WINDOW: usize = 8;
const RATE_BACKOFF_SECS: [u64; 5] = [1, 2, 4, 8, 16];
const RATE_LOCKOUT_SECS: u64 = 60;
const CROSS_ORIGIN_PRELUDE: &[u8; 5] = b"DMCX1";
const CROSS_ORIGIN_PRELUDE_JSON_MAX: usize = 1024;

/// Bounded per-listener admission registry (grant + ticket hashes only).
#[derive(Default)]
pub(crate) struct CrossOriginAdmissionRegistry {
    grants: HashMap<[u8; 32], GrantRecord>,
    /// Reservation IDs are never ticket-hash keys.
    reservations: HashMap<u64, Instant>,
    ready: HashMap<[u8; 32], TicketRecord>,
    next_reservation: u64,
}

#[derive(Clone)]
struct GrantRecord {
    origin: String,
    issuer_client_id: String,
    host_public_id: [u8; 16],
    listener_generation: u64,
    expires_at: Instant,
    expires_at_epoch_ms: u64,
}

#[derive(Clone)]
struct TicketRecord {
    origin: String,
    paired_client_id: String,
    public_key: ConnectPeerPublicKey,
    host_public_id: [u8; 16],
    listener_generation: u64,
    expires_at: Instant,
    expires_at_epoch_ms: u64,
}

/// Opaque grant/ticket material. Debug never prints raw bearer bytes.
pub(crate) struct RedactedSecret(String);

impl fmt::Debug for RedactedSecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RedactedSecret")
            .field("len", &self.0.len())
            .finish()
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct GrantRequestBody {
    origin: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GrantResponseBody {
    grant: String,
    origin: String,
    expires_at_epoch_ms: u64,
}

impl fmt::Debug for GrantResponseBody {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GrantResponseBody")
            .field("grant", &RedactedSecret(self.grant.clone()))
            .field("origin", &self.origin)
            .field("expires_at_epoch_ms", &self.expires_at_epoch_ms)
            .finish()
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PairRequestBody {
    grant: String,
    browser_install_id: String,
    #[serde(default)]
    label: Option<String>,
    public_key: String,
}

impl fmt::Debug for PairRequestBody {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PairRequestBody")
            .field("grant", &RedactedSecret(self.grant.clone()))
            .field("browser_install_id", &self.browser_install_id)
            .field("label", &self.label)
            .field("public_key", &"<redacted>")
            .finish()
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PairResponseBody {
    attach_ticket: String,
    expires_at_epoch_ms: u64,
    host_public_id: String,
    client_id: String,
}

impl fmt::Debug for PairResponseBody {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PairResponseBody")
            .field("attach_ticket", &RedactedSecret(self.attach_ticket.clone()))
            .field("expires_at_epoch_ms", &self.expires_at_epoch_ms)
            .field("host_public_id", &self.host_public_id)
            .field("client_id", &self.client_id)
            .finish()
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum PreludeMessage {
    #[serde(rename = "ticket")]
    Ticket { ticket: String },
    #[serde(rename = "resume")]
    Resume {},
}

impl fmt::Debug for PreludeMessage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ticket { ticket } => formatter
                .debug_struct("Ticket")
                .field("ticket", &RedactedSecret(ticket.clone()))
                .finish(),
            Self::Resume {} => formatter.debug_struct("Resume").finish(),
        }
    }
}

/// Bounded per-WebState cross-origin IP rate budget (separate from same-origin tracker).
#[derive(Default)]
pub(crate) struct CrossOriginRateBudget {
    entries: HashMap<IpAddr, RateEntry>,
}

#[derive(Clone, Copy)]
struct RateEntry {
    window_started: Instant,
    attempts_in_window: usize,
    blocked_until: Option<Instant>,
    last_seen: Instant,
    consecutive_penalties: usize,
}

impl CrossOriginRateBudget {
    fn reap(&mut self, now: Instant) {
        self.entries
            .retain(|_, entry| now.saturating_duration_since(entry.last_seen) < RATE_ENTRY_TTL);
    }

    /// Check-and-record one attempt. Fail closed when the map is at capacity for a new IP.
    pub(crate) fn admit_attempt(&mut self, ip: IpAddr, now: Instant) -> Result<(), Duration> {
        self.reap(now);
        if let Some(entry) = self.entries.get(&ip).copied() {
            if let Some(blocked_until) = entry.blocked_until {
                if blocked_until > now {
                    return Err(blocked_until.saturating_duration_since(now));
                }
            }
        } else if self.entries.len() >= MAX_CROSS_ORIGIN_RATE_IPS {
            return Err(Duration::from_secs(RATE_LOCKOUT_SECS));
        }

        let entry = self.entries.entry(ip).or_insert(RateEntry {
            window_started: now,
            attempts_in_window: 0,
            blocked_until: None,
            last_seen: now,
            consecutive_penalties: 0,
        });
        entry.last_seen = now;
        if now.saturating_duration_since(entry.window_started) >= RATE_WINDOW {
            entry.window_started = now;
            entry.attempts_in_window = 0;
        }
        entry.attempts_in_window = entry.attempts_in_window.saturating_add(1);
        if entry.attempts_in_window > RATE_MAX_ATTEMPTS_PER_WINDOW {
            entry.consecutive_penalties = entry.consecutive_penalties.saturating_add(1);
            let delay = if entry.consecutive_penalties > RATE_BACKOFF_SECS.len() {
                entry.consecutive_penalties = 0;
                Duration::from_secs(RATE_LOCKOUT_SECS)
            } else {
                Duration::from_secs(
                    RATE_BACKOFF_SECS[entry.consecutive_penalties.saturating_sub(1)],
                )
            };
            entry.blocked_until = Some(now + delay);
            entry.attempts_in_window = 0;
            return Err(delay);
        }
        Ok(())
    }

    pub(crate) fn clear(&mut self, ip: IpAddr) {
        self.entries.remove(&ip);
    }
}

/// RAII ticket capacity reservation. Drop releases the exact slot unless disarmed.
pub(crate) struct TicketReservationGuard {
    registry: Arc<Mutex<CrossOriginAdmissionRegistry>>,
    reservation_id: Option<u64>,
}

impl TicketReservationGuard {
    fn arm(registry: Arc<Mutex<CrossOriginAdmissionRegistry>>, reservation_id: u64) -> Self {
        Self {
            registry,
            reservation_id: Some(reservation_id),
        }
    }

    fn id(&self) -> Option<u64> {
        self.reservation_id
    }

    fn disarm(mut self) -> u64 {
        self.reservation_id
            .take()
            .expect("ticket reservation already disarmed")
    }
}

impl Drop for TicketReservationGuard {
    fn drop(&mut self) {
        if let Some(id) = self.reservation_id.take() {
            if let Ok(mut registry) = self.registry.lock() {
                registry.release_ticket_reservation(id);
            }
        }
    }
}

pub(crate) fn mount_cross_origin_routes(router: Router<Arc<WebState>>) -> Router<Arc<WebState>> {
    router
        .route("/api/connect/cross-origin-grants", post(grant_handler))
        .route(
            "/api/connect/cross-origin-pair",
            options(pair_options_handler)
                .post(pair_handler)
                .layer(DefaultBodyLimit::max(CROSS_ORIGIN_PAIR_BODY_BYTES)),
        )
        .route("/api/connect/cross-origin", get(cross_origin_ws_handler))
}

impl CrossOriginAdmissionRegistry {
    fn purge_expired(&mut self, now: Instant) {
        self.grants.retain(|_, grant| grant.expires_at > now);
        self.reservations.retain(|_, expires_at| *expires_at > now);
        self.ready.retain(|_, ticket| ticket.expires_at > now);
    }

    fn ticket_slots_in_use(&self) -> usize {
        self.reservations.len().saturating_add(self.ready.len())
    }

    fn active_grant_origin(&self, origin: &str, now: Instant) -> bool {
        self.grants
            .values()
            .any(|grant| grant.expires_at > now && grant.origin == origin)
    }

    fn mint_grant(
        &mut self,
        origin: String,
        issuer_client_id: String,
        host_public_id: [u8; 16],
        listener_generation: u64,
        now: Instant,
    ) -> Result<(String, u64), StatusCode> {
        self.purge_expired(now);
        if self.grants.len() >= MAX_CROSS_ORIGIN_GRANTS {
            return Err(StatusCode::SERVICE_UNAVAILABLE);
        }
        let raw = random_token_32()?;
        let hash = hash_token(&raw);
        let expires_at_epoch_ms = epoch_ms_after(GRANT_TTL);
        self.grants.insert(
            hash,
            GrantRecord {
                origin,
                issuer_client_id,
                host_public_id,
                listener_generation,
                expires_at: now + GRANT_TTL,
                expires_at_epoch_ms,
            },
        );
        Ok((raw, expires_at_epoch_ms))
    }

    /// Atomically claim a one-use grant. Claimed grants are never restored.
    fn claim_grant(
        &mut self,
        grant: &str,
        origin: &str,
        now: Instant,
    ) -> Result<GrantRecord, StatusCode> {
        self.purge_expired(now);
        let hash = hash_token(grant);
        let Some(record) = self.grants.remove(&hash) else {
            return Err(StatusCode::UNAUTHORIZED);
        };
        if record.expires_at <= now || record.origin != origin {
            return Err(StatusCode::UNAUTHORIZED);
        }
        Ok(record)
    }

    fn reserve_ticket_slot(&mut self, now: Instant) -> Result<u64, StatusCode> {
        self.purge_expired(now);
        if self.ticket_slots_in_use() >= MAX_CROSS_ORIGIN_TICKETS {
            return Err(StatusCode::SERVICE_UNAVAILABLE);
        }
        let id = self.next_reservation.wrapping_add(1).max(1);
        self.next_reservation = id;
        self.reservations.insert(id, now + TICKET_TTL);
        Ok(id)
    }

    fn release_ticket_reservation(&mut self, reservation_id: u64) {
        self.reservations.remove(&reservation_id);
    }

    /// Commit requires the exact still-live Reserved slot (not release-then-mint).
    fn commit_ticket(
        &mut self,
        reservation_id: u64,
        record: TicketRecord,
        now: Instant,
    ) -> Result<String, StatusCode> {
        self.purge_expired(now);
        let Some(expires_at) = self.reservations.get(&reservation_id).copied() else {
            return Err(StatusCode::SERVICE_UNAVAILABLE);
        };
        if expires_at <= now {
            self.reservations.remove(&reservation_id);
            return Err(StatusCode::SERVICE_UNAVAILABLE);
        }
        if self.ready.len() >= MAX_CROSS_ORIGIN_TICKETS {
            return Err(StatusCode::SERVICE_UNAVAILABLE);
        }
        let raw = random_token_32()?;
        let hash = hash_token(&raw);
        self.reservations.remove(&reservation_id);
        self.ready.insert(hash, record);
        Ok(raw)
    }

    fn consume_ticket(
        &mut self,
        ticket: &str,
        origin: &str,
        listener_generation: u64,
        host_public_id: [u8; 16],
        now: Instant,
    ) -> Result<TicketRecord, StatusCode> {
        self.purge_expired(now);
        let hash = hash_token(ticket);
        let Some(record) = self.ready.remove(&hash) else {
            return Err(StatusCode::UNAUTHORIZED);
        };
        if record.expires_at <= now
            || record.origin != origin
            || record.listener_generation != listener_generation
            || record.host_public_id != host_public_id
        {
            return Err(StatusCode::UNAUTHORIZED);
        }
        Ok(record)
    }

    fn has_ready_ticket_for_origin(&self, origin: &str, now: Instant) -> bool {
        self.ready
            .values()
            .any(|ticket| ticket.expires_at > now && ticket.origin == origin)
    }
}

fn random_token_32() -> Result<String, StatusCode> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn hash_token(raw: &str) -> [u8; 32] {
    let digest = Sha256::digest(raw.as_bytes());
    let mut out = [0_u8; 32];
    out.copy_from_slice(&digest);
    out
}

fn epoch_ms_after(ttl: Duration) -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0);
    now.saturating_add(ttl.as_millis() as u64)
}

fn host_public_id_uuid_string(bytes: [u8; 16]) -> String {
    Uuid::from_bytes(bytes).to_string()
}

/// Canonical HTTPS origin only: no credentials, path, query, fragment, null, or wildcard.
pub(crate) fn canonicalize_https_origin(raw: &str) -> Result<String, StatusCode> {
    let trimmed = raw.trim();
    if trimmed.is_empty()
        || trimmed.len() > MAX_CANONICAL_ORIGIN_BYTES
        || trimmed.eq_ignore_ascii_case("null")
        || trimmed.contains('*')
    {
        return Err(StatusCode::BAD_REQUEST);
    }
    let uri: axum::http::Uri = trimmed.parse().map_err(|_| StatusCode::BAD_REQUEST)?;
    if uri.scheme_str() != Some("https") {
        return Err(StatusCode::BAD_REQUEST);
    }
    let authority = uri.authority().ok_or(StatusCode::BAD_REQUEST)?;
    if authority.as_str().contains('@') {
        return Err(StatusCode::BAD_REQUEST);
    }
    if let Some(path_and_query) = uri.path_and_query() {
        let path = path_and_query.path();
        if path != "/" && !path.is_empty() {
            return Err(StatusCode::BAD_REQUEST);
        }
        if path_and_query.query().is_some() {
            return Err(StatusCode::BAD_REQUEST);
        }
    }
    if trimmed.contains('#') {
        return Err(StatusCode::BAD_REQUEST);
    }
    let host = authority.host();
    let port = authority.port_u16();
    let canonical = match port {
        None | Some(443) => format!("https://{host}"),
        Some(port) => format!("https://{host}:{port}"),
    };
    if canonical.len() > MAX_CANONICAL_ORIGIN_BYTES {
        return Err(StatusCode::BAD_REQUEST);
    }
    Ok(canonical)
}

fn cors_headers(origin: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    if let Ok(value) = HeaderValue::from_str(origin) {
        headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, value);
    }
    headers.insert(header::VARY, HeaderValue::from_static("Origin"));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers
}

fn denied_no_cors(status: StatusCode, message: &'static str) -> Response {
    (status, message).into_response()
}

fn request_origin_header(headers: &HeaderMap) -> Result<String, StatusCode> {
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .ok_or(StatusCode::FORBIDDEN)?;
    canonicalize_https_origin(origin)
}

fn https_origin_for_advertised_host(
    headers: &HeaderMap,
    advertised_hostname: &str,
) -> Result<String, StatusCode> {
    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .ok_or(StatusCode::FORBIDDEN)?;
    let authority: Authority = host.parse().map_err(|_| StatusCode::FORBIDDEN)?;
    if !authority.host().eq_ignore_ascii_case(advertised_hostname) {
        return Err(StatusCode::FORBIDDEN);
    }
    let expected = match authority.port_u16() {
        None | Some(443) => format!("https://{}", authority.host()),
        Some(port) => format!("https://{}:{port}", authority.host()),
    };
    let expected = canonicalize_https_origin(&expected)?;
    let origin = request_origin_header(headers)?;
    if origin != expected {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(origin)
}

fn grant_issuer_still_authorized(
    config: &crate::remote::RemoteHostConfig,
    grant: &GrantRecord,
    host_public_id: [u8; 16],
    listener_generation: u64,
) -> bool {
    if !config.web.enabled
        || grant.listener_generation != listener_generation
        || grant.host_public_id != host_public_id
    {
        return false;
    }
    config.web.paired_clients.iter().any(|client| {
        client.client_id == grant.issuer_client_id && client.permitted_origin.is_none()
    })
}

/// Live grant + issuer/host/listener authority for preflight/preclaim ACAO.
/// Read-only; never mutates grants, tickets, or lastSeen.
fn live_grant_origin_cors_eligible(state: &WebState, origin: &str, now: Instant) -> bool {
    let Ok(mut registry) = state.cross_origin.lock() else {
        return false;
    };
    registry.purge_expired(now);
    let Some(inner) = state.upgrade_inner() else {
        return false;
    };
    let Ok(config) = inner.config.read() else {
        return false;
    };
    let Some(startup) = state.connect_startup.as_ref() else {
        return false;
    };
    if startup.require_bound_listener().is_err() {
        return false;
    }
    let host_public_id = *startup.session().profile_host_public_id().as_bytes();
    let listener_generation = state.listener_generation;
    registry.grants.values().any(|grant| {
        grant.expires_at > now
            && grant.origin == origin
            && grant_issuer_still_authorized(&config, grant, host_public_id, listener_generation)
    })
}

fn attach_preclaim_cors_if_eligible(
    state: &WebState,
    origin: &str,
    now: Instant,
    response: &mut Response,
) {
    if live_grant_origin_cors_eligible(state, origin, now) {
        response.headers_mut().extend(cors_headers(origin));
    }
}

fn authorize_owner_grant(
    state: &WebState,
    headers: &HeaderMap,
    peer_ip: IpAddr,
    verified: Option<&super::VerifiedDirectTransport>,
) -> Result<(super::ValidatedWebAuthentication, [u8; 16], String), Response> {
    let (_scheme, policy) =
        cross_origin_verified_tls_only(headers, peer_ip, verified).ok_or_else(|| {
            denied_no_cors(
                StatusCode::FORBIDDEN,
                "grant requires verified TLS listener evidence",
            )
        })?;
    let owner_origin = https_origin_for_advertised_host(headers, &policy.advertised_hostname)
        .map_err(|_| {
            denied_no_cors(
                StatusCode::FORBIDDEN,
                "grant Origin must be exact HTTPS same-host authority",
            )
        })?;
    let authentication =
        validate_authenticated_request(state, headers).map_err(|error| match error {
            WebAuthError::Unauthorized => denied_no_cors(StatusCode::UNAUTHORIZED, "not paired"),
            WebAuthError::Durability => denied_no_cors(
                StatusCode::INTERNAL_SERVER_ERROR,
                "authentication state unavailable",
            ),
        })?;
    let Some(startup) = state.connect_startup.as_ref() else {
        return Err(denied_no_cors(
            StatusCode::SERVICE_UNAVAILABLE,
            "Connect production startup unavailable",
        ));
    };
    if startup.require_bound_listener().is_err() {
        return Err(denied_no_cors(
            StatusCode::SERVICE_UNAVAILABLE,
            "Connect listener is not bound",
        ));
    }
    let host_public_id = *startup.session().profile_host_public_id().as_bytes();
    let _ = owner_origin;
    Ok((authentication, host_public_id, policy.advertised_hostname))
}

fn lock_registry(
    state: &WebState,
) -> Result<MutexGuard<'_, CrossOriginAdmissionRegistry>, Response> {
    state.cross_origin.lock().map_err(|_| {
        denied_no_cors(
            StatusCode::INTERNAL_SERVER_ERROR,
            "admission registry unavailable",
        )
    })
}

fn lock_rate(state: &WebState) -> Result<MutexGuard<'_, CrossOriginRateBudget>, Response> {
    state
        .cross_origin_rate
        .lock()
        .map_err(|_| denied_no_cors(StatusCode::INTERNAL_SERVER_ERROR, "rate budget unavailable"))
}

fn admit_cross_origin_rate(state: &WebState, ip: IpAddr) -> Result<(), Response> {
    let mut budget = lock_rate(state)?;
    budget.admit_attempt(ip, Instant::now()).map_err(|retry| {
        (
            StatusCode::TOO_MANY_REQUESTS,
            format!("retry after {}s", retry.as_secs().max(1)),
        )
            .into_response()
    })
}

async fn grant_handler(
    State(state): State<Arc<WebState>>,
    ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>,
    verified: Option<Extension<super::VerifiedDirectTransport>>,
    headers: HeaderMap,
    Json(body): Json<GrantRequestBody>,
) -> Response {
    let (authentication, host_public_id, _) = match authorize_owner_grant(
        &state,
        &headers,
        addr.ip(),
        verified.as_ref().map(|value| &value.0),
    ) {
        Ok(values) => values,
        Err(response) => return response,
    };
    let origin = match canonicalize_https_origin(&body.origin) {
        Ok(origin) => origin,
        Err(status) => return denied_no_cors(status, "invalid origin"),
    };
    let now = Instant::now();
    let mut registry = match lock_registry(&state) {
        Ok(guard) => guard,
        Err(response) => return response,
    };
    match registry.mint_grant(
        origin.clone(),
        authentication.client_id,
        host_public_id,
        state.listener_generation,
        now,
    ) {
        Ok((grant, expires_at_epoch_ms)) => {
            let body = GrantResponseBody {
                grant,
                origin,
                expires_at_epoch_ms,
            };
            match serde_json::to_vec(&body) {
                Ok(bytes) => (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "application/json")],
                    bytes,
                )
                    .into_response(),
                Err(_) => denied_no_cors(StatusCode::INTERNAL_SERVER_ERROR, "encoding failed"),
            }
        }
        Err(status) => denied_no_cors(status, "grant capacity exceeded"),
    }
}

async fn pair_options_handler(
    State(state): State<Arc<WebState>>,
    ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>,
    verified: Option<Extension<super::VerifiedDirectTransport>>,
    headers: HeaderMap,
) -> Response {
    if cross_origin_verified_tls_only(&headers, addr.ip(), verified.as_ref().map(|v| &v.0))
        .is_none()
    {
        return denied_no_cors(StatusCode::FORBIDDEN, "TLS authority required");
    }
    let origin = match request_origin_header(&headers) {
        Ok(origin) => origin,
        Err(status) => return denied_no_cors(status, "invalid origin"),
    };
    let now = Instant::now();
    if !live_grant_origin_cors_eligible(&state, &origin, now) {
        return denied_no_cors(StatusCode::FORBIDDEN, "no active grant for origin");
    }
    let mut response = (StatusCode::NO_CONTENT, ()).into_response();
    let mut cors = cors_headers(&origin);
    cors.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("POST"),
    );
    cors.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("content-type"),
    );
    response.headers_mut().extend(cors);
    response
}

async fn pair_handler(
    State(state): State<Arc<WebState>>,
    ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>,
    verified: Option<Extension<super::VerifiedDirectTransport>>,
    headers: HeaderMap,
    Json(body): Json<PairRequestBody>,
) -> Response {
    if cross_origin_verified_tls_only(&headers, addr.ip(), verified.as_ref().map(|value| &value.0))
        .is_none()
    {
        return denied_no_cors(StatusCode::FORBIDDEN, "TLS authority required");
    }
    let origin = match request_origin_header(&headers) {
        Ok(origin) => origin,
        Err(status) => return denied_no_cors(status, "invalid origin"),
    };
    let client_ip = addr.ip();
    let now = Instant::now();
    if let Err(mut response) = admit_cross_origin_rate(&state, client_ip) {
        // Preclaim: ACAO only when a live authorized grant exists for Origin.
        attach_preclaim_cors_if_eligible(&state, &origin, now, &mut response);
        return response;
    }

    let public_key = match parse_public_key_hex(&body.public_key) {
        Ok(key) => key,
        Err(status) => {
            let mut response = denied_no_cors(status, "invalid public key");
            attach_preclaim_cors_if_eligible(&state, &origin, now, &mut response);
            return response;
        }
    };
    if body.grant.is_empty() || body.grant.len() > 128 {
        let mut response = denied_no_cors(StatusCode::BAD_REQUEST, "invalid grant");
        attach_preclaim_cors_if_eligible(&state, &origin, now, &mut response);
        return response;
    }
    let browser_install_id =
        match validate_browser_install_id(Some(body.browser_install_id.clone())) {
            Ok(Some(id)) => id,
            Ok(None) | Err(_) => {
                let mut response =
                    denied_no_cors(StatusCode::BAD_REQUEST, "invalid browserInstallId");
                attach_preclaim_cors_if_eligible(&state, &origin, now, &mut response);
                return response;
            }
        };

    let Some(inner) = state.upgrade_inner() else {
        let mut response = denied_no_cors(StatusCode::INTERNAL_SERVER_ERROR, "host unavailable");
        attach_preclaim_cors_if_eligible(&state, &origin, now, &mut response);
        return response;
    };
    let Some(connect_startup) = state.connect_startup.clone() else {
        let mut response = denied_no_cors(
            StatusCode::SERVICE_UNAVAILABLE,
            "Connect production startup unavailable",
        );
        attach_preclaim_cors_if_eligible(&state, &origin, now, &mut response);
        return response;
    };
    if connect_startup.require_bound_listener().is_err() {
        let mut response = denied_no_cors(
            StatusCode::SERVICE_UNAVAILABLE,
            "Connect listener is not bound",
        );
        attach_preclaim_cors_if_eligible(&state, &origin, now, &mut response);
        return response;
    }

    let host_public_id = *connect_startup
        .session()
        .profile_host_public_id()
        .as_bytes();
    let listener_generation = state.listener_generation;

    let (claimed_grant, reservation_guard) = {
        let mut registry = match lock_registry(&state) {
            Ok(guard) => guard,
            Err(mut response) => {
                attach_preclaim_cors_if_eligible(&state, &origin, now, &mut response);
                return response;
            }
        };
        let grant = match registry.claim_grant(&body.grant, &origin, now) {
            Ok(grant) => grant,
            Err(status) => {
                drop(registry);
                let mut response = denied_no_cors(status, "grant rejected");
                // Grant missing/expired/mismatched: no ACAO unless another live grant remains.
                attach_preclaim_cors_if_eligible(&state, &origin, now, &mut response);
                return response;
            }
        };
        // Pre-check issuer/host/listener against current config (no lastSeen revision equality).
        let issuer_ok = inner.config.read().ok().is_some_and(|config| {
            grant_issuer_still_authorized(&config, &grant, host_public_id, listener_generation)
        });
        if !issuer_ok {
            drop(registry);
            // Claim already consumed; may keep exact ACAO for the captured origin.
            let mut response = denied_no_cors(StatusCode::UNAUTHORIZED, "grant authority revoked");
            response.headers_mut().extend(cors_headers(&origin));
            return response;
        }
        let reservation = match registry.reserve_ticket_slot(now) {
            Ok(id) => id,
            Err(status) => {
                // Grant already claimed: never restore; keep exact ACAO.
                drop(registry);
                let mut response = denied_no_cors(status, "ticket capacity exceeded");
                response.headers_mut().extend(cors_headers(&origin));
                return response;
            }
        };
        drop(registry);
        (
            grant,
            TicketReservationGuard::arm(Arc::clone(&state.cross_origin), reservation),
        )
    };

    let label = body
        .label
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.chars().take(128).collect::<String>())
        .unwrap_or_else(|| "Cross-origin browser".to_string());
    let permitted_origin = claimed_grant.origin.clone();
    let assigned_client_id = ClientId::new();
    let paired_client_id = generate_web_client_id();
    let issued_at = now_epoch_ms();
    let client_ip_string = client_ip.to_string();
    let grant_for_predicate = claimed_grant.clone();

    let persist_deadline = Instant::now() + Duration::from_secs(15);
    let persist_inner = Arc::clone(&inner);
    let persist_public_key = public_key;
    let persist_paired_id = paired_client_id.clone();
    let persist_origin = permitted_origin.clone();
    let persist_label = label.clone();
    let persist_install = browser_install_id.clone();
    let Ok(mut persist_work) =
        RemoteBlockingWork::spawn(
            "cross-origin-pair-persist",
            persist_deadline,
            move |admission| {
                mutate_host_config_if(
                    &persist_inner,
                    |config| {
                        if !grant_issuer_still_authorized(
                            config,
                            &grant_for_predicate,
                            host_public_id,
                            listener_generation,
                        ) {
                            return false;
                        }
                        if !(config.web.paired_clients.len() < MAX_CONNECT_PEER_PINS
                            && config.web.connect_peer_keys.len() < MAX_CONNECT_PEER_PINS
                            && !config.web.paired_clients.iter().any(|client| {
                                client.client_id == persist_paired_id
                                    || (!client.browser_install_id.is_empty()
                                        && client.browser_install_id == persist_install)
                            })
                            && !config.web.connect_peer_keys.values().any(|pin| {
                                pin.public_key.as_bytes() == persist_public_key.as_bytes()
                            }))
                        {
                            return false;
                        }
                        // Admit only after authority/capacity checks, under config lock frontier.
                        admission.try_admit()
                    },
                    |config| {
                        config.web.paired_clients.push(PairedWebClient {
                            client_id: persist_paired_id.clone(),
                            browser_install_id: persist_install.clone(),
                            nickname: None,
                            label: persist_label.clone(),
                            issued_at_epoch_ms: Some(issued_at),
                            last_seen_epoch_ms: Some(issued_at),
                            last_seen_ip: Some(client_ip_string.clone()),
                            user_agent: None,
                            browser_family: None,
                            browser_version: None,
                            os_family: None,
                            device_class: None,
                            permitted_origin: Some(persist_origin.clone()),
                        });
                        config.web.connect_peer_keys.insert(
                            persist_paired_id.clone(),
                            ConnectPeerPin {
                                public_key: persist_public_key,
                                client_id: assigned_client_id,
                            },
                        );
                        crate::remote::append_remote_access_activity_event(
                            config,
                            RemoteAccessActivityEvent {
                                client_id: persist_paired_id.clone(),
                                source: RemoteAccessSource::Browser,
                                event_kind: RemoteAccessActivityKind::Paired,
                                label: persist_label.clone(),
                                ip_address: Some(client_ip_string.clone()),
                                event_at_epoch_ms: Some(issued_at),
                                browser_family: None,
                                browser_version: None,
                                os_family: None,
                                device_class: None,
                            },
                        );
                        true
                    },
                )
                .map_err(|_| "persist")
                .and_then(|bound| bound.ok_or("predicate"))
            },
        )
    else {
        drop(reservation_guard);
        let mut response = denied_no_cors(
            StatusCode::SERVICE_UNAVAILABLE,
            "persistence worker unavailable",
        );
        response.headers_mut().extend(cors_headers(&origin));
        return response;
    };

    let persist_result = persist_work.wait().await;
    let durable_ok = matches!(persist_result, Ok(Ok(true)));
    if !durable_ok {
        drop(reservation_guard);
        let mut response = denied_no_cors(StatusCode::CONFLICT, "pair persistence failed");
        response.headers_mut().extend(cors_headers(&origin));
        return response;
    }

    let ticket_record = TicketRecord {
        origin: permitted_origin.clone(),
        paired_client_id: paired_client_id.clone(),
        public_key,
        host_public_id,
        listener_generation,
        expires_at: Instant::now() + TICKET_TTL,
        expires_at_epoch_ms: epoch_ms_after(TICKET_TTL),
    };
    let reservation_id = match reservation_guard.id() {
        Some(id) => id,
        None => {
            let mut response =
                denied_no_cors(StatusCode::INTERNAL_SERVER_ERROR, "ticket reservation lost");
            response.headers_mut().extend(cors_headers(&origin));
            return response;
        }
    };
    let attach_ticket = {
        let mut registry = match lock_registry(&state) {
            Ok(guard) => guard,
            Err(response) => {
                let mut response = response;
                response.headers_mut().extend(cors_headers(&origin));
                return response;
            }
        };
        match registry.commit_ticket(reservation_id, ticket_record, Instant::now()) {
            Ok(ticket) => {
                let _ = reservation_guard.disarm();
                ticket
            }
            Err(status) => {
                drop(registry);
                drop(reservation_guard);
                let mut response = denied_no_cors(status, "ticket mint failed after durable pair");
                response.headers_mut().extend(cors_headers(&origin));
                return response;
            }
        }
    };

    if let Ok(mut budget) = state.cross_origin_rate.lock() {
        budget.clear(client_ip);
    }

    let body = PairResponseBody {
        attach_ticket,
        expires_at_epoch_ms: epoch_ms_after(TICKET_TTL),
        host_public_id: host_public_id_uuid_string(host_public_id),
        client_id: assigned_client_id.to_string(),
    };
    match serde_json::to_vec(&body) {
        Ok(bytes) => {
            let mut response = (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "application/json")],
                bytes,
            )
                .into_response();
            response.headers_mut().extend(cors_headers(&origin));
            response
        }
        Err(_) => {
            let mut response = denied_no_cors(StatusCode::INTERNAL_SERVER_ERROR, "encoding failed");
            response.headers_mut().extend(cors_headers(&origin));
            response
        }
    }
}

fn parse_public_key_hex(value: &str) -> Result<ConnectPeerPublicKey, StatusCode> {
    if value.len() != CONNECT_PEER_PUBLIC_KEY_HEX_CHARS {
        return Err(StatusCode::BAD_REQUEST);
    }
    let decoded = hex_decode(value).ok_or(StatusCode::BAD_REQUEST)?;
    let bytes: [u8; CONNECT_PEER_PUBLIC_KEY_BYTES] = decoded
        .as_slice()
        .try_into()
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    ConnectPeerPublicKey::from_bytes(bytes).map_err(|_| StatusCode::BAD_REQUEST)
}

pub(crate) async fn cross_origin_ws_handler(
    State(state): State<Arc<WebState>>,
    ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>,
    verified: Option<Extension<super::VerifiedDirectTransport>>,
    ws: WebSocketUpgrade,
    headers: HeaderMap,
) -> Response {
    if cross_origin_verified_tls_only(&headers, addr.ip(), verified.as_ref().map(|value| &value.0))
        .is_none()
    {
        return denied_no_cors(StatusCode::FORBIDDEN, "TLS authority required");
    }
    let origin = match request_origin_header(&headers) {
        Ok(origin) => origin,
        Err(status) => return denied_no_cors(status, "invalid origin"),
    };
    if let Err(response) = admit_cross_origin_rate(&state, addr.ip()) {
        return response;
    }
    let Some(inner) = state.upgrade_inner() else {
        return denied_no_cors(StatusCode::INTERNAL_SERVER_ERROR, "host unavailable");
    };
    let Some(connect_startup) = state.connect_startup.clone() else {
        return denied_no_cors(
            StatusCode::SERVICE_UNAVAILABLE,
            "Connect production startup unavailable",
        );
    };
    let host_public_id = *connect_startup
        .session()
        .profile_host_public_id()
        .as_bytes();

    let now = Instant::now();
    let origin_admissible = {
        let registry = match lock_registry(&state) {
            Ok(guard) => guard,
            Err(response) => return response,
        };
        let has_ticket = registry.has_ready_ticket_for_origin(&origin, now);
        drop(registry);
        let has_paired = inner.config.read().ok().is_some_and(|config| {
            config.web.enabled
                && config
                    .web
                    .paired_clients
                    .iter()
                    .any(|client| client.permitted_origin.as_deref() == Some(origin.as_str()))
        });
        has_ticket || has_paired
    };
    if !origin_admissible {
        return denied_no_cors(StatusCode::FORBIDDEN, "origin not admitted");
    }

    inner
        .connect_encryption_required
        .store(true, std::sync::atomic::Ordering::Release);
    let host_requests = state.host_requests.clone();
    let listener_generation = state.listener_generation;
    let cross_origin = Arc::clone(&state.cross_origin);
    let rate = Arc::clone(&state.cross_origin_rate);
    let peer_ip = addr.ip();
    let inner_weak = Arc::downgrade(&inner);
    ws.max_message_size(crate::protocol::MAX_SEALED_FRAME_BYTES as usize)
        .max_frame_size(crate::protocol::MAX_SEALED_FRAME_BYTES as usize)
        .on_upgrade(move |socket| {
            run_cross_origin_connect(
                socket,
                inner_weak,
                connect_startup,
                host_requests,
                origin,
                listener_generation,
                host_public_id,
                cross_origin,
                rate,
                peer_ip,
            )
        })
}

async fn run_cross_origin_connect(
    mut socket: WebSocket,
    inner: std::sync::Weak<crate::remote::RemoteHostInner>,
    connect_startup: Arc<crate::connect::ConnectProductionStartup>,
    host_requests: crate::connect::ConnectHostRequestSlot,
    origin: String,
    listener_generation: u64,
    host_public_id: [u8; 16],
    registry: Arc<Mutex<CrossOriginAdmissionRegistry>>,
    rate: Arc<Mutex<CrossOriginRateBudget>>,
    peer_ip: IpAddr,
) {
    let handshake_deadline = tokio::time::Instant::now() + CONNECT_HANDSHAKE_TIMEOUT;
    let mode = match tokio::time::timeout_at(
        handshake_deadline,
        admit_cross_origin_prelude(
            &mut socket,
            &origin,
            listener_generation,
            host_public_id,
            &registry,
            &rate,
            peer_ip,
        ),
    )
    .await
    {
        Ok(Some(mode)) => mode,
        _ => {
            let _ = tokio::time::timeout_at(handshake_deadline, socket.close()).await;
            return;
        }
    };
    run_connect_session(
        socket,
        inner,
        connect_startup,
        host_requests,
        mode,
        handshake_deadline,
    )
    .await;
}

async fn admit_cross_origin_prelude(
    socket: &mut WebSocket,
    origin: &str,
    listener_generation: u64,
    host_public_id: [u8; 16],
    registry: &Arc<Mutex<CrossOriginAdmissionRegistry>>,
    rate: &Arc<Mutex<CrossOriginRateBudget>>,
    peer_ip: IpAddr,
) -> Option<ConnectSessionMode> {
    let prelude = recv_binary_exact(socket, CROSS_ORIGIN_PRELUDE.len()).await?;
    if prelude.as_slice() != CROSS_ORIGIN_PRELUDE {
        return None;
    }
    let json_bytes = recv_binary_capped(socket, CROSS_ORIGIN_PRELUDE_JSON_MAX).await?;
    let message: PreludeMessage = match serde_json::from_slice(&json_bytes) {
        Ok(message) => message,
        Err(_) => return None,
    };
    match message {
        PreludeMessage::Ticket { ticket } => {
            if ticket.is_empty() || ticket.len() > 128 {
                return None;
            }
            let record = {
                let mut registry = registry.lock().ok()?;
                registry
                    .consume_ticket(
                        &ticket,
                        origin,
                        listener_generation,
                        host_public_id,
                        Instant::now(),
                    )
                    .ok()?
            };
            if let Ok(mut budget) = rate.lock() {
                budget.clear(peer_ip);
            }
            Some(ConnectSessionMode::CrossOrigin {
                origin: origin.to_string(),
                ticket_binding: Some(CrossOriginTicketBinding {
                    paired_client_id: record.paired_client_id,
                    public_key: record.public_key,
                    host_public_id: record.host_public_id,
                }),
            })
        }
        PreludeMessage::Resume {} => {
            // Rate was already admitted at upgrade; do not hold locks across awaits.
            Some(ConnectSessionMode::CrossOrigin {
                origin: origin.to_string(),
                ticket_binding: None,
            })
        }
    }
}

async fn recv_binary_exact(socket: &mut WebSocket, expected: usize) -> Option<Vec<u8>> {
    let bytes = recv_binary_capped(socket, expected).await?;
    (bytes.len() == expected).then_some(bytes)
}

async fn recv_binary_capped(socket: &mut WebSocket, max_bytes: usize) -> Option<Vec<u8>> {
    use futures_util::StreamExt;
    loop {
        match socket.next().await? {
            Ok(WsMessage::Binary(bytes)) => {
                if bytes.is_empty() || bytes.len() > max_bytes {
                    return None;
                }
                return Some(bytes);
            }
            Ok(WsMessage::Ping(_) | WsMessage::Pong(_)) => {}
            Ok(WsMessage::Text(_) | WsMessage::Close(_)) | Err(_) => {
                return None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::test_support::TestProfileGuard;
    use crate::remote::{RemoteHostConfig, RemoteHostService};
    use axum::extract::ConnectInfo;
    use std::net::SocketAddr;
    use std::sync::atomic::Ordering;

    fn test_web_state(
        service: &RemoteHostService,
        startup: Option<Arc<crate::connect::ConnectProductionStartup>>,
    ) -> Arc<WebState> {
        Arc::new(WebState {
            inner: Arc::downgrade(&service.inner),
            listener_generation: service
                .inner
                .native_runtime_generation
                .load(Ordering::Acquire),
            pairing_attempts: Arc::new(Mutex::new(Default::default())),
            connect_startup: startup,
            host_requests: crate::connect::ConnectHostRequestSlot::new(),
            cross_origin: Arc::new(Mutex::new(CrossOriginAdmissionRegistry::default())),
            cross_origin_rate: Arc::new(Mutex::new(CrossOriginRateBudget::default())),
            fleet_trust_source: None,
            fleet_test_publication: None,
        })
    }

    #[test]
    fn canonicalize_rejects_http_null_wildcard_and_path() {
        assert!(canonicalize_https_origin("http://a.example").is_err());
        assert!(canonicalize_https_origin("null").is_err());
        assert!(canonicalize_https_origin("https://*.example").is_err());
        assert!(canonicalize_https_origin("https://a.example/path").is_err());
        assert!(canonicalize_https_origin("https://user:pass@a.example").is_err());
        assert_eq!(
            canonicalize_https_origin("https://phone.example:8443").unwrap(),
            "https://phone.example:8443"
        );
    }

    #[test]
    fn grant_one_use_and_issuer_fields_captured() {
        let mut registry = CrossOriginAdmissionRegistry::default();
        let now = Instant::now();
        let (raw, _) = registry
            .mint_grant(
                "https://a.example".into(),
                "web-owner".into(),
                [7_u8; 16],
                3,
                now,
            )
            .expect("mint");
        let claimed = registry
            .claim_grant(&raw, "https://a.example", now)
            .expect("claim");
        assert_eq!(claimed.issuer_client_id, "web-owner");
        assert_eq!(claimed.listener_generation, 3);
        assert_eq!(claimed.host_public_id, [7_u8; 16]);
        assert!(registry
            .claim_grant(&raw, "https://a.example", now)
            .is_err());
    }

    #[test]
    fn ticket_reservation_guard_releases_on_drop() {
        let registry = Arc::new(Mutex::new(CrossOriginAdmissionRegistry::default()));
        let now = Instant::now();
        let id = registry.lock().unwrap().reserve_ticket_slot(now).unwrap();
        {
            let _guard = TicketReservationGuard::arm(Arc::clone(&registry), id);
            assert_eq!(registry.lock().unwrap().reservations.len(), 1);
        }
        assert!(registry.lock().unwrap().reservations.is_empty());
    }

    #[test]
    fn commit_requires_live_reservation_not_fabricated_id() {
        let mut registry = CrossOriginAdmissionRegistry::default();
        let now = Instant::now();
        let record = TicketRecord {
            origin: "https://a.example".into(),
            paired_client_id: "web-a".into(),
            public_key: ConnectPeerPublicKey::from_bytes([3_u8; 32]).unwrap(),
            host_public_id: [9_u8; 16],
            listener_generation: 1,
            expires_at: now + TICKET_TTL,
            expires_at_epoch_ms: epoch_ms_after(TICKET_TTL),
        };
        assert!(registry.commit_ticket(99, record.clone(), now).is_err());
        let id = registry.reserve_ticket_slot(now).unwrap();
        registry
            .reservations
            .insert(id, now - Duration::from_secs(1));
        assert!(registry.commit_ticket(id, record.clone(), now).is_err());
        let id = registry.reserve_ticket_slot(now).unwrap();
        let ticket = registry.commit_ticket(id, record, now).expect("commit");
        assert!(registry
            .consume_ticket(&ticket, "https://a.example", 1, [9_u8; 16], now)
            .is_ok());
        assert!(registry
            .consume_ticket(&ticket, "https://a.example", 1, [9_u8; 16], now)
            .is_err());
    }

    #[test]
    fn host_public_id_response_is_uuid_string() {
        let bytes = [
            0x01, 0x8f, 0x2d, 0x6e, 0x5c, 0x4b, 0x70, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x01,
        ];
        let rendered = host_public_id_uuid_string(bytes);
        assert!(rendered.contains('-'));
        assert_eq!(Uuid::parse_str(&rendered).unwrap().as_bytes(), &bytes);
        let body = PairResponseBody {
            attach_ticket: "t".into(),
            expires_at_epoch_ms: 1,
            host_public_id: rendered.clone(),
            client_id: ClientId::new().to_string(),
        };
        let json = serde_json::to_string(&body).unwrap();
        assert!(json.contains(&rendered));
        assert!(!json.contains(&format!("{:02x}{:02x}", bytes[0], bytes[1]).repeat(8)));
    }

    #[test]
    fn cors_never_credentials_and_debug_redacts_secrets() {
        let headers = cors_headers("https://a.example");
        assert!(headers
            .get(header::ACCESS_CONTROL_ALLOW_CREDENTIALS)
            .is_none());
        assert!(headers.get(header::SET_COOKIE).is_none());
        let pair = PairRequestBody {
            grant: "secret-grant".into(),
            browser_install_id: "install".into(),
            label: None,
            public_key: "11".repeat(32),
        };
        assert!(!format!("{pair:?}").contains("secret-grant"));
        let prelude = PreludeMessage::Ticket {
            ticket: "secret-ticket".into(),
        };
        assert!(!format!("{prelude:?}").contains("secret-ticket"));
    }

    #[test]
    fn rate_budget_reaps_and_fail_closes_at_capacity() {
        let mut budget = CrossOriginRateBudget::default();
        let now = Instant::now();
        for index in 0..MAX_CROSS_ORIGIN_RATE_IPS {
            let ip = IpAddr::from([10, 0, (index / 256) as u8, (index % 256) as u8]);
            assert!(budget.admit_attempt(ip, now).is_ok());
        }
        let overflow = IpAddr::from([11, 0, 0, 1]);
        assert!(budget.admit_attempt(overflow, now).is_err());
        budget.clear(IpAddr::from([10, 0, 0, 0]));
        assert!(budget.admit_attempt(overflow, now).is_ok());
    }

    #[test]
    fn grant_authority_rejects_revoked_issuer_and_survives_unrelated_revision_concept() {
        let _profile = TestProfileGuard::new("cross-origin-grant-authority");
        let mut config = RemoteHostConfig::default();
        config.web.enabled = true;
        config.web.paired_clients.push(PairedWebClient {
            client_id: "web-owner".into(),
            browser_install_id: "owner".into(),
            label: "Owner".into(),
            permitted_origin: None,
            ..PairedWebClient::default()
        });
        let grant = GrantRecord {
            origin: "https://a.example".into(),
            issuer_client_id: "web-owner".into(),
            host_public_id: [1_u8; 16],
            listener_generation: 2,
            expires_at: Instant::now() + GRANT_TTL,
            expires_at_epoch_ms: 1,
        };
        assert!(grant_issuer_still_authorized(
            &config, &grant, [1_u8; 16], 2
        ));
        config.web.paired_clients.clear();
        assert!(!grant_issuer_still_authorized(
            &config, &grant, [1_u8; 16], 2
        ));
    }

    #[test]
    fn pair_persist_then_ticket_admits_without_config_revision_equality() {
        let _profile = TestProfileGuard::new("cross-origin-pair-ticket-lease");
        let mut config = RemoteHostConfig::default();
        config.web.enabled = true;
        config.web.paired_clients.push(PairedWebClient {
            client_id: "web-owner".into(),
            browser_install_id: "owner".into(),
            label: "Owner".into(),
            ..PairedWebClient::default()
        });
        let service = RemoteHostService::new_web_only(config).expect("web");
        let state = test_web_state(&service, None);
        let host_public_id = [4_u8; 16];
        let listener_generation = state.listener_generation;
        let peer = ConnectPeerPublicKey::from_bytes([5_u8; 32]).unwrap();
        let assigned = ClientId::new();
        let paired_id = "web-cross-a".to_string();
        let origin = "https://a.example".to_string();

        let mut registry = state.cross_origin.lock().unwrap();
        let (grant_raw, _) = registry
            .mint_grant(
                origin.clone(),
                "web-owner".into(),
                host_public_id,
                listener_generation,
                Instant::now(),
            )
            .unwrap();
        let grant = registry
            .claim_grant(&grant_raw, &origin, Instant::now())
            .unwrap();
        let reservation = registry.reserve_ticket_slot(Instant::now()).unwrap();
        drop(registry);

        // Simulate durable pair + activity (revision bump) then ticket commit.
        let _ = mutate_host_config_if(
            &service.inner,
            |_| true,
            |config| {
                config.web.paired_clients.push(PairedWebClient {
                    client_id: paired_id.clone(),
                    browser_install_id: "phone-1".into(),
                    label: "Phone".into(),
                    permitted_origin: Some(origin.clone()),
                    ..PairedWebClient::default()
                });
                config.web.connect_peer_keys.insert(
                    paired_id.clone(),
                    ConnectPeerPin {
                        public_key: peer,
                        client_id: assigned,
                    },
                );
                crate::remote::append_remote_access_activity_event(
                    config,
                    RemoteAccessActivityEvent {
                        client_id: paired_id.clone(),
                        source: RemoteAccessSource::Browser,
                        event_kind: RemoteAccessActivityKind::Paired,
                        label: "Phone".into(),
                        ip_address: None,
                        event_at_epoch_ms: Some(now_epoch_ms()),
                        browser_family: None,
                        browser_version: None,
                        os_family: None,
                        device_class: None,
                    },
                );
            },
        )
        .unwrap();
        // Unrelated lastSeen-style write after pair.
        let _ = mutate_host_config_if(
            &service.inner,
            |_| true,
            |config| {
                if let Some(client) = config
                    .web
                    .paired_clients
                    .iter_mut()
                    .find(|client| client.client_id == paired_id)
                {
                    client.last_seen_epoch_ms = Some(now_epoch_ms());
                }
            },
        )
        .unwrap();

        let ticket_record = TicketRecord {
            origin: origin.clone(),
            paired_client_id: paired_id.clone(),
            public_key: peer,
            host_public_id,
            listener_generation,
            expires_at: Instant::now() + TICKET_TTL,
            expires_at_epoch_ms: epoch_ms_after(TICKET_TTL),
        };
        let ticket = state
            .cross_origin
            .lock()
            .unwrap()
            .commit_ticket(reservation, ticket_record, Instant::now())
            .expect("ticket after revision bump");
        let consumed = state
            .cross_origin
            .lock()
            .unwrap()
            .consume_ticket(
                &ticket,
                &origin,
                listener_generation,
                host_public_id,
                Instant::now(),
            )
            .expect("consume");
        let lease = super::super::connect_identity::validate_cross_origin_connect_peer(
            &service.inner,
            &origin,
            peer.as_bytes(),
            Some(&consumed.paired_client_id),
            Some(peer),
        )
        .expect("lease");
        assert!(lease.is_authorized());
        assert!(grant_issuer_still_authorized(
            &service.config(),
            &grant,
            host_public_id,
            listener_generation
        ));
    }

    #[tokio::test]
    async fn grant_handler_denies_loopback_policy_without_tls() {
        let _profile = TestProfileGuard::new("cross-origin-grant-tls");
        let mut config = RemoteHostConfig::default();
        config.web.enabled = true;
        config.web.paired_clients.push(PairedWebClient {
            client_id: "web-owner".into(),
            browser_install_id: "owner".into(),
            label: "Owner".into(),
            ..PairedWebClient::default()
        });
        let service = RemoteHostService::new_web_only(config).expect("web");
        let state = test_web_state(&service, None);
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "127.0.0.1:43872".parse().unwrap());
        headers.insert(header::ORIGIN, "http://127.0.0.1:43872".parse().unwrap());
        let response = grant_handler(
            State(state),
            ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 43872))),
            None,
            headers,
            Json(GrantRequestBody {
                origin: "https://phone.example".into(),
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(response
            .headers()
            .get(header::ACCESS_CONTROL_ALLOW_CREDENTIALS)
            .is_none());
        assert!(response.headers().get(header::SET_COOKIE).is_none());
    }

    #[tokio::test]
    async fn pair_handler_denies_missing_tls_without_credentialed_cors() {
        let _profile = TestProfileGuard::new("cross-origin-pair-tls");
        let mut config = RemoteHostConfig::default();
        config.web.enabled = true;
        let service = RemoteHostService::new_web_only(config).expect("web");
        let state = test_web_state(&service, None);
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "b.example:8443".parse().unwrap());
        headers.insert(header::ORIGIN, "https://a.example".parse().unwrap());
        let response = pair_handler(
            State(state),
            ConnectInfo(SocketAddr::from(([192, 168, 1, 20], 50000))),
            None,
            headers,
            Json(PairRequestBody {
                grant: "abc".into(),
                browser_install_id: "install-1".into(),
                label: None,
                public_key: "11".repeat(32),
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(response
            .headers()
            .get(header::ACCESS_CONTROL_ALLOW_CREDENTIALS)
            .is_none());
    }

    #[tokio::test]
    async fn pair_options_unauthorized_origin_omits_acao() {
        let _profile = TestProfileGuard::new("cross-origin-options-no-grant");
        let mut config = RemoteHostConfig::default();
        config.web.enabled = true;
        let service = RemoteHostService::new_web_only(config).expect("web");
        let state = test_web_state(&service, None);
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "b.example:8443".parse().unwrap());
        headers.insert(header::ORIGIN, "https://a.example".parse().unwrap());
        let tls = super::super::tls::VerifiedDirectTransport::mint_after_rustls_handshake(
            "b.example:8443".into(),
        );
        let response = pair_options_handler(
            State(state),
            ConnectInfo(SocketAddr::from(([192, 168, 1, 20], 50000))),
            Some(Extension(tls)),
            headers,
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(response
            .headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .is_none());
    }

    #[tokio::test]
    async fn pair_handler_malformed_without_live_grant_omits_acao() {
        let _profile = TestProfileGuard::new("cross-origin-pair-no-acao");
        let mut config = RemoteHostConfig::default();
        config.web.enabled = true;
        let service = RemoteHostService::new_web_only(config).expect("web");
        let state = test_web_state(&service, None);
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "b.example:8443".parse().unwrap());
        headers.insert(
            header::ORIGIN,
            "https://unauthorized.example".parse().unwrap(),
        );
        let tls = super::super::tls::VerifiedDirectTransport::mint_after_rustls_handshake(
            "b.example:8443".into(),
        );
        let response = pair_handler(
            State(state),
            ConnectInfo(SocketAddr::from(([192, 168, 1, 20], 50000))),
            Some(Extension(tls)),
            headers,
            Json(PairRequestBody {
                grant: "not-a-grant".into(),
                browser_install_id: "install-1".into(),
                label: None,
                public_key: "zz".repeat(32), // invalid hex
            }),
        )
        .await;
        assert!(response.status().is_client_error() || response.status().is_server_error());
        assert!(response
            .headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .is_none());
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(!String::from_utf8_lossy(&body).contains("not-a-grant"));
    }

    #[test]
    fn live_grant_cors_eligibility_false_without_startup_even_with_grant() {
        let _profile = TestProfileGuard::new("cross-origin-cors-eligibility");
        let mut config = RemoteHostConfig::default();
        config.web.enabled = true;
        config.web.paired_clients.push(PairedWebClient {
            client_id: "web-owner".into(),
            browser_install_id: "owner".into(),
            label: "Owner".into(),
            permitted_origin: None,
            ..PairedWebClient::default()
        });
        let service = RemoteHostService::new_web_only(config).expect("web");
        let state = test_web_state(&service, None);
        let now = Instant::now();
        state
            .cross_origin
            .lock()
            .unwrap()
            .mint_grant(
                "https://a.example".into(),
                "web-owner".into(),
                [1_u8; 16],
                state.listener_generation,
                now,
            )
            .unwrap();
        // Active grant alone is insufficient: host/listener authority needs startup.
        assert!(!live_grant_origin_cors_eligible(
            &state,
            "https://a.example",
            now
        ));
        assert!(!live_grant_origin_cors_eligible(
            &state,
            "https://other.example",
            now
        ));
    }

    #[test]
    fn revoked_issuer_fails_cors_authority_predicate() {
        let mut config = RemoteHostConfig::default();
        config.web.enabled = true;
        let grant = GrantRecord {
            origin: "https://a.example".into(),
            issuer_client_id: "revoked-owner".into(),
            host_public_id: [2_u8; 16],
            listener_generation: 1,
            expires_at: Instant::now() + GRANT_TTL,
            expires_at_epoch_ms: 1,
        };
        assert!(!grant_issuer_still_authorized(
            &config, &grant, [2_u8; 16], 1
        ));
        config.web.paired_clients.push(PairedWebClient {
            client_id: "revoked-owner".into(),
            browser_install_id: "owner".into(),
            label: "Owner".into(),
            permitted_origin: None,
            ..PairedWebClient::default()
        });
        assert!(grant_issuer_still_authorized(
            &config, &grant, [2_u8; 16], 1
        ));
        config.web.paired_clients.clear();
        assert!(!grant_issuer_still_authorized(
            &config, &grant, [2_u8; 16], 1
        ));
    }

    #[test]
    fn cross_origin_tls_only_denies_wrong_host() {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "b.example:8443".parse().unwrap());
        let peer = "192.168.1.20".parse().unwrap();
        assert!(cross_origin_verified_tls_only(&headers, peer, None).is_none());
        let tls = super::super::tls::VerifiedDirectTransport::mint_after_rustls_handshake(
            "b.example:8443".into(),
        );
        assert!(cross_origin_verified_tls_only(&headers, peer, Some(&tls)).is_some());
        headers.insert(header::HOST, "evil.example:8443".parse().unwrap());
        assert!(cross_origin_verified_tls_only(&headers, peer, Some(&tls)).is_none());
    }
}
