//! Hosted-relay route tickets and the production `wss://` relay client.
//!
//! Tickets authorize a route. They do not carry task content, pairing secrets,
//! invitation secrets, or private keys. The production client forwards
//! already-sealed `SealedFrame` bytes only. [`OpaqueRelay`] is an in-memory
//! router/test double and is not production networking.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::net::IpAddr;
use std::sync::Arc;

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use uuid::{Uuid, Variant};
use zeroize::Zeroizing;

use super::crypto::preferred_connect_route;
use super::envelope::ConnectIdError;
use super::transport::ConnectRoute;
use crate::protocol::{SealedFrame, MAX_SEALED_FRAME_BYTES};

type HmacSha256 = Hmac<Sha256>;

pub const ROUTE_TICKET_DOMAIN: &[u8] = b"DevManagerConnect/v1/route-ticket\0";
pub const MAX_ROUTE_TICKET_TTL_SECS: u64 = 5 * 60;
pub const MAX_RELAY_QUEUE_FRAMES: usize = 8;
pub const MAX_RELAY_QUEUE_BYTES: u32 = MAX_SEALED_FRAME_BYTES;
pub const MAX_BIND_ATTEMPTS_PER_WINDOW: u32 = 8;
pub const BIND_RATE_WINDOW_SECS: u64 = 60;
pub const PRESENCE_TTL_SECS: u64 = 30;
pub const MAX_RELAY_ROUTES: usize = 1_024;
pub const MAX_RELAY_REVOKED_TICKETS: usize = 4_096;
pub const MAX_RELAY_REVOKED_DEVICES: usize = 4_096;
pub const MAX_RELAY_CONSUMED_NONCES: usize = 16_384;
pub const MAX_RELAY_RATE_KEYS: usize = 4_096;
pub const MAX_RELAY_ENDPOINT_BYTES: usize = 256;
pub const RELAY_INITIAL_BACKOFF_MS: u64 = 250;
pub const RELAY_MAX_BACKOFF_MS: u64 = 30_000;
const TICKET_TAG_BYTES: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RouteId(Uuid);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TicketId(Uuid);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HostPublicId(Uuid);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DevicePublicId(Uuid);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AccountId(Uuid);

macro_rules! impl_connect_uuid {
    ($name:ident) => {
        impl $name {
            pub fn from_uuid(value: Uuid) -> Result<Self, ConnectIdError> {
                if value.get_version_num() != 7 {
                    return Err(ConnectIdError::InvalidVersion);
                }
                if value.get_variant() != Variant::RFC4122 {
                    return Err(ConnectIdError::InvalidVariant);
                }
                Ok(Self(value))
            }

            pub fn from_bytes(value: [u8; 16]) -> Result<Self, ConnectIdError> {
                Self::from_uuid(Uuid::from_bytes(value))
            }

            pub const fn as_uuid(self) -> Uuid {
                self.0
            }

            pub const fn as_bytes(self) -> [u8; 16] {
                self.0.into_bytes()
            }
        }

        impl TryFrom<Uuid> for $name {
            type Error = ConnectIdError;

            fn try_from(value: Uuid) -> Result<Self, Self::Error> {
                Self::from_uuid(value)
            }
        }
    };
}

impl_connect_uuid!(RouteId);
impl_connect_uuid!(TicketId);
impl_connect_uuid!(HostPublicId);
impl_connect_uuid!(DevicePublicId);
impl_connect_uuid!(AccountId);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TicketAudience {
    HostSocket,
    DeviceSocket,
}

impl TicketAudience {
    const fn wire_tag(self) -> u8 {
        match self {
            Self::HostSocket => 1,
            Self::DeviceSocket => 2,
        }
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteTicket {
    ticket_id: TicketId,
    route_id: RouteId,
    host_public_id: HostPublicId,
    device_public_id: DevicePublicId,
    account_id: AccountId,
    audience: TicketAudience,
    issued_at_unix: u64,
    expires_at_unix: u64,
    nonce: [u8; 16],
}

impl RouteTicket {
    pub fn new(
        ticket_id: TicketId,
        route_id: RouteId,
        host_public_id: HostPublicId,
        device_public_id: DevicePublicId,
        account_id: AccountId,
        audience: TicketAudience,
        issued_at_unix: u64,
        expires_at_unix: u64,
        nonce: [u8; 16],
    ) -> Result<Self, RelayError> {
        if expires_at_unix <= issued_at_unix {
            return Err(RelayError::InvalidTicket);
        }
        if expires_at_unix.saturating_sub(issued_at_unix) > MAX_ROUTE_TICKET_TTL_SECS {
            return Err(RelayError::InvalidTicket);
        }
        Ok(Self {
            ticket_id,
            route_id,
            host_public_id,
            device_public_id,
            account_id,
            audience,
            issued_at_unix,
            expires_at_unix,
            nonce,
        })
    }

    pub const fn ticket_id(&self) -> TicketId {
        self.ticket_id
    }

    pub const fn route_id(&self) -> RouteId {
        self.route_id
    }

    pub const fn host_public_id(&self) -> HostPublicId {
        self.host_public_id
    }

    pub const fn device_public_id(&self) -> DevicePublicId {
        self.device_public_id
    }

    pub const fn account_id(&self) -> AccountId {
        self.account_id
    }

    pub const fn audience(&self) -> TicketAudience {
        self.audience
    }

    pub const fn issued_at_unix(&self) -> u64 {
        self.issued_at_unix
    }

    pub const fn expires_at_unix(&self) -> u64 {
        self.expires_at_unix
    }

    pub const fn nonce(&self) -> [u8; 16] {
        self.nonce
    }

    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(ROUTE_TICKET_DOMAIN.len() + 16 * 5 + 1 + 8 + 8 + 16);
        bytes.extend_from_slice(ROUTE_TICKET_DOMAIN);
        bytes.extend_from_slice(&self.ticket_id.as_bytes());
        bytes.extend_from_slice(&self.route_id.as_bytes());
        bytes.extend_from_slice(&self.host_public_id.as_bytes());
        bytes.extend_from_slice(&self.device_public_id.as_bytes());
        bytes.extend_from_slice(&self.account_id.as_bytes());
        bytes.push(self.audience.wire_tag());
        bytes.extend_from_slice(&self.issued_at_unix.to_be_bytes());
        bytes.extend_from_slice(&self.expires_at_unix.to_be_bytes());
        bytes.extend_from_slice(&self.nonce);
        bytes
    }
}

impl fmt::Debug for RouteTicket {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RouteTicket")
            .field("ticket_id", &self.ticket_id)
            .field("route_id", &self.route_id)
            .field("host_public_id", &self.host_public_id)
            .field("device_public_id", &self.device_public_id)
            .field("account_id", &self.account_id)
            .field("audience", &self.audience)
            .field("issued_at_unix", &self.issued_at_unix)
            .field("expires_at_unix", &self.expires_at_unix)
            .field("nonce_len", &self.nonce.len())
            .finish()
    }
}

#[derive(Clone)]
pub struct TicketSigningKey(Zeroizing<[u8; 32]>);

impl TicketSigningKey {
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(Zeroizing::new(bytes))
    }

    pub fn generate() -> Result<Self, RelayError> {
        let mut bytes = [0_u8; 32];
        getrandom::fill(&mut bytes).map_err(|_| RelayError::EntropyUnavailable)?;
        Ok(Self::from_bytes(bytes))
    }
}

impl fmt::Debug for TicketSigningKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TicketSigningKey")
            .field("len", &32)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct SignedRouteTicket {
    claims: RouteTicket,
    tag: [u8; TICKET_TAG_BYTES],
}

impl SignedRouteTicket {
    pub fn issue(key: &TicketSigningKey, claims: RouteTicket) -> Self {
        let tag = sign_ticket(key, &claims);
        Self { claims, tag }
    }

    pub fn verify(&self, key: &TicketSigningKey) -> Result<&RouteTicket, RelayError> {
        let expected = sign_ticket(key, &self.claims);
        if !constant_time_eq(&expected, &self.tag) {
            return Err(RelayError::InvalidTicket);
        }
        Ok(&self.claims)
    }

    pub const fn claims(&self) -> &RouteTicket {
        &self.claims
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut encoded = self.claims.canonical_bytes();
        encoded.extend_from_slice(&self.tag);
        encoded
    }

    pub fn decode(key: &TicketSigningKey, bytes: &[u8]) -> Result<Self, RelayError> {
        if bytes.len() < TICKET_TAG_BYTES + ROUTE_TICKET_DOMAIN.len() + 16 * 5 + 1 + 8 + 8 + 16 {
            return Err(RelayError::InvalidTicket);
        }
        let (canonical, tag_bytes) = bytes.split_at(bytes.len() - TICKET_TAG_BYTES);
        if !canonical.starts_with(ROUTE_TICKET_DOMAIN) {
            return Err(RelayError::InvalidTicket);
        }
        let body = &canonical[ROUTE_TICKET_DOMAIN.len()..];
        if body.len() != 16 * 5 + 1 + 8 + 8 + 16 {
            return Err(RelayError::InvalidTicket);
        }
        let ticket_id = TicketId::from_bytes(copy16(&body[0..16]))?;
        let route_id = RouteId::from_bytes(copy16(&body[16..32]))?;
        let host_public_id = HostPublicId::from_bytes(copy16(&body[32..48]))?;
        let device_public_id = DevicePublicId::from_bytes(copy16(&body[48..64]))?;
        let account_id = AccountId::from_bytes(copy16(&body[64..80]))?;
        let audience = match body[80] {
            1 => TicketAudience::HostSocket,
            2 => TicketAudience::DeviceSocket,
            _ => return Err(RelayError::InvalidTicket),
        };
        let issued_at_unix = u64::from_be_bytes(copy8(&body[81..89]));
        let expires_at_unix = u64::from_be_bytes(copy8(&body[89..97]));
        let nonce = copy16(&body[97..113]);
        let tag = copy32(tag_bytes);
        let claims = RouteTicket::new(
            ticket_id,
            route_id,
            host_public_id,
            device_public_id,
            account_id,
            audience,
            issued_at_unix,
            expires_at_unix,
            nonce,
        )?;
        let ticket = Self { claims, tag };
        ticket.verify(key)?;
        Ok(ticket)
    }
}

impl fmt::Debug for SignedRouteTicket {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SignedRouteTicket")
            .field("claims", &self.claims)
            .field("tag_len", &TICKET_TAG_BYTES)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayStatus {
    Bound,
    Forwarded,
    Dropped,
    Disconnected,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayObservation {
    route_id: RouteId,
    frame_bytes: u32,
    status: RelayStatus,
    error_class: Option<RelayError>,
}

impl RelayObservation {
    pub const fn route_id(&self) -> RouteId {
        self.route_id
    }

    pub const fn frame_bytes(&self) -> u32 {
        self.frame_bytes
    }

    pub const fn status(&self) -> RelayStatus {
        self.status
    }

    pub const fn error_class(&self) -> Option<RelayError> {
        self.error_class
    }

    pub fn contains_secret(&self, secret: &str) -> bool {
        let rendered = format!("{self:?}");
        rendered.contains(secret)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RateKey {
    Account(AccountId),
    Ip(IpAddr),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RateWindow {
    started_at_unix: u64,
    count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BoundChannel {
    claims: RouteTicket,
    queued: VecDeque<SealedFrame>,
    queued_bytes: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PresenceRecord {
    expires_at_unix: u64,
}

#[derive(Clone, PartialEq, Eq)]
pub struct RelayEndpoint {
    url: url::Url,
}

impl fmt::Debug for RelayEndpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RelayEndpoint")
            .field("scheme", &self.url.scheme())
            .field("has_host", &self.url.host_str().is_some())
            .finish()
    }
}

impl RelayEndpoint {
    pub fn parse(raw: &str) -> Result<Self, RelayError> {
        if raw.is_empty() {
            return Err(RelayError::InsecureEndpoint);
        }
        if raw.len() > MAX_RELAY_ENDPOINT_BYTES {
            return Err(RelayError::InsecureEndpoint);
        }
        let url = url::Url::parse(raw).map_err(|_| RelayError::InsecureEndpoint)?;
        if url.scheme() != "wss" {
            return Err(RelayError::InsecureEndpoint);
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(RelayError::InsecureEndpoint);
        }
        let host = url.host_str().unwrap_or("");
        if host.is_empty() || host == "*" {
            return Err(RelayError::InsecureEndpoint);
        }
        if url.port_or_known_default().is_none() {
            return Err(RelayError::InsecureEndpoint);
        }
        Ok(Self { url })
    }

    pub fn as_str(&self) -> &str {
        self.url.as_str()
    }
}

/// Production TLS WebSocket relay client. Forwards already-sealed Connect
/// frames only. Does not wrap [`OpaqueRelay`].
pub struct ConnectRelayClient {
    endpoint: RelayEndpoint,
    ticket: SignedRouteTicket,
    route: ConnectRoute,
    queued: VecDeque<SealedFrame>,
    queued_bytes: u32,
    max_queue_frames: usize,
    max_queue_bytes: u32,
    reconnect_attempt: u32,
}

impl fmt::Debug for ConnectRelayClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ConnectRelayClient")
            .field("endpoint", &self.endpoint)
            .field("route", &self.route)
            .field("queued_frames", &self.queued.len())
            .field("reconnect_attempt", &self.reconnect_attempt)
            .finish()
    }
}

impl ConnectRelayClient {
    pub fn connect_configured(
        endpoint: RelayEndpoint,
        ticket: SignedRouteTicket,
    ) -> Result<Self, RelayError> {
        let _ = ticket.claims();
        Ok(Self {
            endpoint,
            ticket,
            route: ConnectRoute::Relay,
            queued: VecDeque::new(),
            queued_bytes: 0,
            max_queue_frames: MAX_RELAY_QUEUE_FRAMES,
            max_queue_bytes: MAX_RELAY_QUEUE_BYTES,
            reconnect_attempt: 0,
        })
    }

    pub fn preferred_route(&self) -> ConnectRoute {
        self.route
    }

    pub fn next_backoff_ms(&self) -> u64 {
        relay_backoff_ms(self.reconnect_attempt)
    }

    pub fn record_reconnect_failure(&mut self) {
        self.reconnect_attempt = self.reconnect_attempt.saturating_add(1);
    }

    pub fn enqueue_sealed(&mut self, frame: SealedFrame) -> Result<(), RelayError> {
        let encoded_len = u32::try_from(frame.encoded_len()).unwrap_or(u32::MAX);
        if encoded_len > MAX_SEALED_FRAME_BYTES {
            return Err(RelayError::FrameExceeded {
                declared: u64::from(encoded_len),
            });
        }
        if self.queued.len() >= self.max_queue_frames
            || self.queued_bytes.saturating_add(encoded_len) > self.max_queue_bytes
        {
            return Err(RelayError::QueueExceeded);
        }
        self.queued_bytes = self.queued_bytes.saturating_add(encoded_len);
        self.queued.push_back(frame);
        Ok(())
    }

    pub async fn connect(&self) -> Result<ConnectRelaySocket, RelayError> {
        connect_relay_socket(&self.endpoint, &self.ticket).await
    }
}

pub struct ConnectRelaySocket {
    stream:
        tokio_tungstenite::WebSocketStream<tokio_rustls::client::TlsStream<tokio::net::TcpStream>>,
    route: ConnectRoute,
}

impl fmt::Debug for ConnectRelaySocket {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ConnectRelaySocket")
            .field("route", &self.route)
            .finish()
    }
}

impl ConnectRelaySocket {
    pub fn route(&self) -> ConnectRoute {
        self.route
    }

    pub async fn send_sealed(&mut self, frame: &SealedFrame) -> Result<(), RelayError> {
        use futures_util::SinkExt;
        let encoded = frame.encode().map_err(|_| RelayError::OpaqueFrame)?;
        if encoded.len() > usize::try_from(MAX_SEALED_FRAME_BYTES).unwrap_or(usize::MAX) {
            return Err(RelayError::FrameExceeded {
                declared: encoded.len() as u64,
            });
        }
        self.stream
            .send(tokio_tungstenite::tungstenite::Message::Binary(encoded))
            .await
            .map_err(|_| RelayError::PeerDisconnected)
    }

    pub async fn recv_sealed(&mut self) -> Result<SealedFrame, RelayError> {
        use futures_util::StreamExt;
        loop {
            match self.stream.next().await {
                Some(Ok(tokio_tungstenite::tungstenite::Message::Binary(bytes))) => {
                    if bytes.len() > usize::try_from(MAX_SEALED_FRAME_BYTES).unwrap_or(usize::MAX) {
                        return Err(RelayError::FrameExceeded {
                            declared: bytes.len() as u64,
                        });
                    }
                    return SealedFrame::decode(&bytes).map_err(|_| RelayError::OpaqueFrame);
                }
                Some(Ok(
                    tokio_tungstenite::tungstenite::Message::Ping(_)
                    | tokio_tungstenite::tungstenite::Message::Pong(_)
                    | tokio_tungstenite::tungstenite::Message::Frame(_),
                )) => {}
                Some(Ok(_)) => return Err(RelayError::OpaqueFrame),
                Some(Err(_)) | None => return Err(RelayError::PeerDisconnected),
            }
        }
    }
}

fn relay_backoff_ms(attempt: u32) -> u64 {
    let shift = u32::min(attempt, 8);
    u64::min(
        RELAY_INITIAL_BACKOFF_MS.saturating_mul(1_u64 << shift),
        RELAY_MAX_BACKOFF_MS,
    )
}

fn relay_tls_config() -> Result<Arc<rustls::ClientConfig>, RelayError> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|_| RelayError::TlsHandshake)?
    .with_root_certificates(roots)
    .with_no_client_auth();
    Ok(Arc::new(config))
}

async fn connect_relay_socket(
    endpoint: &RelayEndpoint,
    ticket: &SignedRouteTicket,
) -> Result<ConnectRelaySocket, RelayError> {
    use futures_util::SinkExt;
    use rustls::pki_types::ServerName;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    let host = endpoint
        .url
        .host_str()
        .ok_or(RelayError::InsecureEndpoint)?
        .to_string();
    let port = endpoint
        .url
        .port_or_known_default()
        .ok_or(RelayError::InsecureEndpoint)?;
    let tcp = tokio::net::TcpStream::connect((host.as_str(), port))
        .await
        .map_err(|_| RelayError::Transport)?;
    let server_name = ServerName::try_from(host).map_err(|_| RelayError::InsecureEndpoint)?;
    let tls = tokio_rustls::TlsConnector::from(relay_tls_config()?)
        .connect(server_name, tcp)
        .await
        .map_err(|_| RelayError::TlsHandshake)?;
    let request = endpoint
        .url
        .as_str()
        .into_client_request()
        .map_err(|_| RelayError::InsecureEndpoint)?;
    let (mut stream, _) = tokio_tungstenite::client_async(request, tls)
        .await
        .map_err(|_| RelayError::Transport)?;
    stream
        .send(tokio_tungstenite::tungstenite::Message::Binary(
            ticket.encode(),
        ))
        .await
        .map_err(|_| RelayError::PeerDisconnected)?;
    Ok(ConnectRelaySocket {
        stream,
        route: ConnectRoute::Relay,
    })
}

/// In-memory router/test double. Not a production network client.
/// Production relay I/O uses [`ConnectRelayClient`].
pub struct OpaqueRelay {
    signing_key: Option<TicketSigningKey>,
    online_hosts: HashSet<HostPublicId>,
    revoked_tickets: HashSet<TicketId>,
    revoked_devices: HashSet<DevicePublicId>,
    revoked_ticket_overflow: bool,
    revoked_device_overflow: bool,
    consumed_nonces: HashSet<[u8; 16]>,
    channels: HashMap<RouteId, HashMap<TicketAudience, BoundChannel>>,
    presence: HashMap<RouteId, PresenceRecord>,
    bind_attempts: HashMap<RateKey, RateWindow>,
    max_queue_frames: usize,
    max_queue_bytes: u32,
}

impl Default for OpaqueRelay {
    fn default() -> Self {
        Self {
            signing_key: None,
            online_hosts: HashSet::new(),
            revoked_tickets: HashSet::new(),
            revoked_devices: HashSet::new(),
            revoked_ticket_overflow: false,
            revoked_device_overflow: false,
            consumed_nonces: HashSet::new(),
            channels: HashMap::new(),
            presence: HashMap::new(),
            bind_attempts: HashMap::new(),
            max_queue_frames: MAX_RELAY_QUEUE_FRAMES,
            max_queue_bytes: MAX_RELAY_QUEUE_BYTES,
        }
    }
}

impl fmt::Debug for OpaqueRelay {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpaqueRelay")
            .field("online_hosts", &self.online_hosts.len())
            .field("bound_routes", &self.channels.len())
            .field("revoked_tickets", &self.revoked_tickets.len())
            .field("revoked_devices", &self.revoked_devices.len())
            .field("presence", &self.presence.len())
            .finish()
    }
}

impl OpaqueRelay {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_signing_key(key: TicketSigningKey) -> Self {
        Self {
            signing_key: Some(key),
            ..Self::default()
        }
    }

    pub fn with_queue_bounds(mut self, max_frames: usize, max_bytes: u32) -> Self {
        self.max_queue_frames = max_frames.max(1);
        self.max_queue_bytes = max_bytes.max(1);
        self
    }

    pub fn preferred_route(direct_reachable: bool) -> ConnectRoute {
        preferred_connect_route(direct_reachable)
    }

    pub fn set_host_online(&mut self, host: HostPublicId, online: bool) {
        if online {
            self.online_hosts.insert(host);
        } else {
            self.online_hosts.remove(&host);
            let mut emptied = Vec::new();
            self.channels.retain(|route_id, sockets| {
                sockets.retain(|_, channel| channel.claims.host_public_id != host);
                if sockets.is_empty() {
                    emptied.push(*route_id);
                    false
                } else {
                    true
                }
            });
            for route_id in emptied {
                self.presence.remove(&route_id);
            }
        }
    }

    pub fn revoke_ticket(&mut self, ticket_id: TicketId) {
        self.remember_revoked_ticket(ticket_id);
        self.drop_ticket(ticket_id);
    }

    pub fn revoke_device(&mut self, device: DevicePublicId) {
        self.remember_revoked_device(device);
        let mut emptied = Vec::new();
        self.channels.retain(|route_id, sockets| {
            sockets.retain(|_, channel| channel.claims.device_public_id != device);
            if sockets.is_empty() {
                emptied.push(*route_id);
                false
            } else {
                true
            }
        });
        for route_id in emptied {
            self.presence.remove(&route_id);
        }
    }

    pub fn bind(
        &mut self,
        ticket: &SignedRouteTicket,
        now_unix: u64,
        source: RateKey,
    ) -> Result<RelayObservation, RelayError> {
        self.expire_stale(now_unix);
        self.admit_rate(source, now_unix)?;
        let key = self.signing_key.as_ref().ok_or(RelayError::InvalidTicket)?;
        let claims = ticket.verify(key)?.clone();
        if now_unix < claims.issued_at_unix || now_unix >= claims.expires_at_unix {
            return Err(RelayError::ExpiredTicket);
        }
        if self.revoked_tickets.contains(&claims.ticket_id) {
            return Err(RelayError::RevokedTicket);
        }
        if self.revoked_devices.contains(&claims.device_public_id) {
            return Err(RelayError::RevokedDevice);
        }
        // Once the bounded revocation index cannot retain another identity,
        // reject new binds rather than evicting a tombstone and reopening a
        // revoked ticket/device.
        if self.revoked_ticket_overflow || self.revoked_device_overflow {
            return Err(RelayError::StateBoundExceeded);
        }
        if claims.audience == TicketAudience::DeviceSocket
            && !self.online_hosts.contains(&claims.host_public_id)
        {
            return Err(RelayError::HostOffline);
        }
        if self.consumed_nonces.len() >= MAX_RELAY_CONSUMED_NONCES
            && !self.consumed_nonces.contains(&claims.nonce)
        {
            return Err(RelayError::StateBoundExceeded);
        }
        if !self.channels.contains_key(&claims.route_id) && self.channels.len() >= MAX_RELAY_ROUTES
        {
            return Err(RelayError::StateBoundExceeded);
        }
        if !self.consumed_nonces.insert(claims.nonce) {
            return Err(RelayError::TicketReused);
        }
        if claims.audience == TicketAudience::HostSocket {
            self.online_hosts.insert(claims.host_public_id);
        }
        let route_id = claims.route_id;
        self.channels.entry(route_id).or_default().insert(
            claims.audience,
            BoundChannel {
                claims,
                queued: VecDeque::new(),
                queued_bytes: 0,
            },
        );
        self.presence.insert(
            route_id,
            PresenceRecord {
                expires_at_unix: now_unix.saturating_add(PRESENCE_TTL_SECS),
            },
        );
        Ok(RelayObservation {
            route_id,
            frame_bytes: 0,
            status: RelayStatus::Bound,
            error_class: None,
        })
    }

    pub fn admit(
        &mut self,
        route_id: RouteId,
        from: TicketAudience,
        frame: SealedFrame,
        now_unix: u64,
    ) -> Result<RelayObservation, RelayError> {
        self.expire_stale(now_unix);
        let encoded_len = u32::try_from(frame.encoded_len()).unwrap_or(u32::MAX);
        if encoded_len > MAX_SEALED_FRAME_BYTES {
            return Err(RelayError::FrameExceeded {
                declared: u64::from(encoded_len),
            });
        }
        frame.validate().map_err(|_| RelayError::OpaqueFrame)?;
        let target = match from {
            TicketAudience::HostSocket => TicketAudience::DeviceSocket,
            TicketAudience::DeviceSocket => TicketAudience::HostSocket,
        };
        let (source_host, peer_host) = {
            let sockets = self
                .channels
                .get(&route_id)
                .ok_or(RelayError::UnknownRoute)?;
            let source = sockets.get(&from).ok_or(RelayError::UnknownRoute)?;
            Self::channel_live(
                source,
                now_unix,
                &self.revoked_tickets,
                &self.revoked_devices,
            )?;
            let peer = sockets.get(&target).ok_or(RelayError::PeerDisconnected)?;
            Self::channel_live(peer, now_unix, &self.revoked_tickets, &self.revoked_devices)?;
            (source.claims.host_public_id, peer.claims.host_public_id)
        };
        if from == TicketAudience::DeviceSocket && !self.online_hosts.contains(&source_host) {
            return Err(RelayError::HostOffline);
        }
        if from == TicketAudience::HostSocket && !self.online_hosts.contains(&peer_host) {
            return Err(RelayError::HostOffline);
        }
        let inbound = self
            .channels
            .get_mut(&route_id)
            .ok_or(RelayError::UnknownRoute)?
            .get_mut(&target)
            .ok_or(RelayError::PeerDisconnected)?;
        if inbound.queued.len() >= self.max_queue_frames
            || inbound.queued_bytes.saturating_add(encoded_len) > self.max_queue_bytes
        {
            return Ok(RelayObservation {
                route_id,
                frame_bytes: encoded_len,
                status: RelayStatus::Dropped,
                error_class: Some(RelayError::QueueExceeded),
            });
        }
        inbound.queued_bytes = inbound.queued_bytes.saturating_add(encoded_len);
        inbound.queued.push_back(frame);
        if let Some(presence) = self.presence.get_mut(&route_id) {
            presence.expires_at_unix = now_unix.saturating_add(PRESENCE_TTL_SECS);
        }
        Ok(RelayObservation {
            route_id,
            frame_bytes: encoded_len,
            status: RelayStatus::Forwarded,
            error_class: None,
        })
    }

    pub fn take(
        &mut self,
        route_id: RouteId,
        audience: TicketAudience,
    ) -> Result<SealedFrame, RelayError> {
        let sockets = self
            .channels
            .get_mut(&route_id)
            .ok_or(RelayError::UnknownRoute)?;
        let channel = sockets.get_mut(&audience).ok_or(RelayError::UnknownRoute)?;
        let frame = channel.queued.pop_front().ok_or(RelayError::QueueEmpty)?;
        let encoded_len = u32::try_from(frame.encoded_len()).unwrap_or(0);
        channel.queued_bytes = channel.queued_bytes.saturating_sub(encoded_len);
        Ok(frame)
    }

    pub fn disconnect(&mut self, route_id: RouteId, audience: TicketAudience) -> RelayObservation {
        if let Some(sockets) = self.channels.get_mut(&route_id) {
            sockets.remove(&audience);
            if sockets.is_empty() {
                self.channels.remove(&route_id);
                self.presence.remove(&route_id);
            }
        }
        RelayObservation {
            route_id,
            frame_bytes: 0,
            status: RelayStatus::Disconnected,
            error_class: None,
        }
    }

    pub fn queued_frames(&self, route_id: RouteId, audience: TicketAudience) -> usize {
        self.channels
            .get(&route_id)
            .and_then(|sockets| sockets.get(&audience))
            .map(|channel| channel.queued.len())
            .unwrap_or(0)
    }

    pub fn presence_live(&self, route_id: RouteId, now_unix: u64) -> bool {
        self.presence
            .get(&route_id)
            .is_some_and(|record| record.expires_at_unix > now_unix)
    }

    fn admit_rate(&mut self, source: RateKey, now_unix: u64) -> Result<(), RelayError> {
        if self.bind_attempts.len() >= MAX_RELAY_RATE_KEYS
            && !self.bind_attempts.contains_key(&source)
        {
            self.evict_expired_rate_windows(now_unix);
            if self.bind_attempts.len() >= MAX_RELAY_RATE_KEYS
                && !self.bind_attempts.contains_key(&source)
            {
                return Err(RelayError::StateBoundExceeded);
            }
        }
        let window = self.bind_attempts.entry(source).or_insert(RateWindow {
            started_at_unix: now_unix,
            count: 0,
        });
        if now_unix.saturating_sub(window.started_at_unix) >= BIND_RATE_WINDOW_SECS {
            window.started_at_unix = now_unix;
            window.count = 0;
        }
        if window.count >= MAX_BIND_ATTEMPTS_PER_WINDOW {
            return Err(RelayError::RateLimited);
        }
        window.count = window.count.saturating_add(1);
        Ok(())
    }

    fn expire_stale(&mut self, now_unix: u64) {
        let expired_presence: Vec<RouteId> = self
            .presence
            .iter()
            .filter_map(|(route_id, record)| {
                (record.expires_at_unix <= now_unix).then_some(*route_id)
            })
            .collect();
        for route_id in expired_presence {
            self.presence.remove(&route_id);
            self.channels.remove(&route_id);
        }

        let mut stale_sockets = Vec::new();
        for (route_id, sockets) in &self.channels {
            for (audience, channel) in sockets {
                let expired = now_unix < channel.claims.issued_at_unix
                    || now_unix >= channel.claims.expires_at_unix;
                let revoked = self.revoked_tickets.contains(&channel.claims.ticket_id)
                    || self
                        .revoked_devices
                        .contains(&channel.claims.device_public_id);
                if expired || revoked {
                    stale_sockets.push((*route_id, *audience));
                }
            }
        }
        let mut emptied = Vec::new();
        for (route_id, audience) in stale_sockets {
            if let Some(sockets) = self.channels.get_mut(&route_id) {
                sockets.remove(&audience);
                if sockets.is_empty() {
                    emptied.push(route_id);
                }
            }
        }
        for route_id in emptied {
            self.channels.remove(&route_id);
            self.presence.remove(&route_id);
        }
        self.evict_expired_rate_windows(now_unix);
    }

    fn channel_live(
        channel: &BoundChannel,
        now_unix: u64,
        revoked_tickets: &HashSet<TicketId>,
        revoked_devices: &HashSet<DevicePublicId>,
    ) -> Result<(), RelayError> {
        if now_unix < channel.claims.issued_at_unix || now_unix >= channel.claims.expires_at_unix {
            return Err(RelayError::ExpiredTicket);
        }
        if revoked_tickets.contains(&channel.claims.ticket_id) {
            return Err(RelayError::RevokedTicket);
        }
        if revoked_devices.contains(&channel.claims.device_public_id) {
            return Err(RelayError::RevokedDevice);
        }
        Ok(())
    }

    fn remember_revoked_ticket(&mut self, ticket_id: TicketId) {
        if self.revoked_tickets.len() >= MAX_RELAY_REVOKED_TICKETS
            && !self.revoked_tickets.contains(&ticket_id)
        {
            self.revoked_ticket_overflow = true;
            return;
        }
        self.revoked_tickets.insert(ticket_id);
    }

    fn remember_revoked_device(&mut self, device: DevicePublicId) {
        if self.revoked_devices.len() >= MAX_RELAY_REVOKED_DEVICES
            && !self.revoked_devices.contains(&device)
        {
            self.revoked_device_overflow = true;
            return;
        }
        self.revoked_devices.insert(device);
    }

    fn evict_expired_rate_windows(&mut self, now_unix: u64) {
        self.bind_attempts.retain(|_, window| {
            now_unix.saturating_sub(window.started_at_unix) < BIND_RATE_WINDOW_SECS
        });
    }

    fn drop_ticket(&mut self, ticket_id: TicketId) {
        let mut emptied = Vec::new();
        self.channels.retain(|route_id, sockets| {
            sockets.retain(|_, channel| channel.claims.ticket_id != ticket_id);
            if sockets.is_empty() {
                emptied.push(*route_id);
                false
            } else {
                true
            }
        });
        for route_id in emptied {
            self.presence.remove(&route_id);
        }
    }
}

fn sign_ticket(key: &TicketSigningKey, claims: &RouteTicket) -> [u8; TICKET_TAG_BYTES] {
    let mut mac =
        HmacSha256::new_from_slice(key.0.as_slice()).expect("HMAC-SHA256 accepts 32 bytes");
    mac.update(&claims.canonical_bytes());
    mac.finalize().into_bytes().into()
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0_u8;
    for (a, b) in left.iter().zip(right.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

fn copy8(bytes: &[u8]) -> [u8; 8] {
    bytes.try_into().expect("8-byte slice")
}

fn copy16(bytes: &[u8]) -> [u8; 16] {
    bytes.try_into().expect("16-byte slice")
}

fn copy32(bytes: &[u8]) -> [u8; 32] {
    bytes.try_into().expect("32-byte slice")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayError {
    InvalidTicket,
    ExpiredTicket,
    TicketReused,
    RevokedTicket,
    RevokedDevice,
    HostOffline,
    UnknownRoute,
    PeerDisconnected,
    OpaqueFrame,
    FrameExceeded { declared: u64 },
    QueueExceeded,
    QueueEmpty,
    RateLimited,
    StateBoundExceeded,
    EntropyUnavailable,
    InsecureEndpoint,
    TlsHandshake,
    Transport,
    Identity(ConnectIdError),
}

impl fmt::Display for RelayError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidTicket => formatter.write_str("Connect route ticket is invalid"),
            Self::ExpiredTicket => formatter.write_str("Connect route ticket is expired"),
            Self::TicketReused => {
                formatter.write_str("Connect route ticket nonce was already bound")
            }
            Self::RevokedTicket => formatter.write_str("Connect route ticket is revoked"),
            Self::RevokedDevice => formatter.write_str("Connect device identity is revoked"),
            Self::HostOffline => formatter.write_str("Connect host is offline"),
            Self::UnknownRoute => formatter.write_str("Connect relay route is not bound"),
            Self::PeerDisconnected => formatter.write_str("Connect relay peer is disconnected"),
            Self::OpaqueFrame => formatter.write_str("Connect relay admits only sealed frames"),
            Self::FrameExceeded { declared } => write!(
                formatter,
                "Connect relay frame length {declared} exceeds {MAX_SEALED_FRAME_BYTES}"
            ),
            Self::QueueExceeded => formatter.write_str("Connect relay queue bound exceeded"),
            Self::QueueEmpty => formatter.write_str("Connect relay queue is empty"),
            Self::RateLimited => formatter.write_str("Connect relay bind rate limit exceeded"),
            Self::StateBoundExceeded => {
                formatter.write_str("Connect relay state collection bound exceeded")
            }
            Self::EntropyUnavailable => formatter.write_str("Connect relay entropy is unavailable"),
            Self::InsecureEndpoint => {
                formatter.write_str("Connect relay endpoint must be a bounded wss:// URL")
            }
            Self::TlsHandshake => formatter.write_str("Connect relay TLS handshake failed"),
            Self::Transport => formatter.write_str("Connect relay transport failed"),
            Self::Identity(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for RelayError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Identity(error) => Some(error),
            _ => None,
        }
    }
}

impl From<ConnectIdError> for RelayError {
    fn from(error: ConnectIdError) -> Self {
        Self::Identity(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{SEALED_FRAME_VERSION, SEALED_NONCE_BYTES, SEALED_TAG_BYTES};

    fn test_ticket() -> SignedRouteTicket {
        let key = TicketSigningKey::from_bytes([9; 32]);
        let claims = RouteTicket::new(
            TicketId::from_uuid(Uuid::now_v7()).expect("ticket"),
            RouteId::from_uuid(Uuid::now_v7()).expect("route"),
            HostPublicId::from_uuid(Uuid::now_v7()).expect("host"),
            DevicePublicId::from_uuid(Uuid::now_v7()).expect("device"),
            AccountId::from_uuid(Uuid::now_v7()).expect("account"),
            TicketAudience::HostSocket,
            10,
            40,
            [4; 16],
        )
        .expect("claims");
        SignedRouteTicket::issue(&key, claims)
    }

    #[test]
    fn relay_endpoint_rejects_insecure_empty_and_unbounded_urls() {
        assert!(RelayEndpoint::parse("").is_err());
        assert!(RelayEndpoint::parse("ws://relay.example/connect").is_err());
        assert!(RelayEndpoint::parse("http://relay.example/connect").is_err());
        assert!(RelayEndpoint::parse("wss://user:pass@relay.example/connect").is_err());
        assert!(RelayEndpoint::parse(&format!("wss://relay.example/{}", "a".repeat(300))).is_err());
        assert!(RelayEndpoint::parse("wss://relay.example/connect").is_ok());
    }

    #[test]
    fn relay_client_bounds_sealed_frames_and_preserves_route() {
        let endpoint = RelayEndpoint::parse("wss://relay.example/connect").expect("endpoint");
        let mut client =
            ConnectRelayClient::connect_configured(endpoint, test_ticket()).expect("client");
        assert_eq!(client.preferred_route(), ConnectRoute::Relay);
        assert_eq!(client.next_backoff_ms(), RELAY_INITIAL_BACKOFF_MS);
        let frame = SealedFrame::from_parts(
            SEALED_FRAME_VERSION,
            1,
            [1; SEALED_NONCE_BYTES],
            vec![0; 8],
            [2; SEALED_TAG_BYTES],
        )
        .expect("frame");
        client.enqueue_sealed(frame).expect("enqueue");
        for _ in 0..MAX_RELAY_QUEUE_FRAMES {
            let extra = SealedFrame::from_parts(
                SEALED_FRAME_VERSION,
                2,
                [3; SEALED_NONCE_BYTES],
                vec![1; 8],
                [4; SEALED_TAG_BYTES],
            )
            .expect("extra");
            if client.enqueue_sealed(extra).is_err() {
                return;
            }
        }
        panic!("relay client must bound the sealed-frame queue");
    }
}
