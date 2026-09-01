//! Authenticated A-side publication of trusted remote Connect hosts.
//!
//! Serves public descriptors for the phone page hosted by PC A. Never exposes
//! pairing cookies, assigned client IDs, CA material, or private LAN roster to
//! unauthenticated visitors. Production reads
//! [`crate::client::RemoteTrustStore::open_under_app_config`].

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use uuid::Uuid;
use zeroize::Zeroize;

use super::{validate_authenticated_request, ValidatedWebAuthentication, WebAuthError, WebState};
use crate::client::{
    validate_remote_endpoint, RemoteTrustError, RemoteTrustStore, TrustedHostRecord,
};
use crate::connect::ConnectWebPublication;
use crate::remote::blocking_work::{RemoteBlockingWork, RemoteWorkAdmission};

pub(crate) const MAX_FLEET_REMOTES: usize = 15;
pub(crate) const MAX_FLEET_JSON_BYTES: usize = 16_384;
pub(crate) const MAX_FLEET_LABEL_CHARS: usize = 80;
pub(crate) const FLEET_META_NAME: &str = "devmanager-connect-fleet";
const FLEET_LOAD_DEADLINE: Duration = Duration::from_secs(2);
const PROTOCOL_MAJOR: u8 = 1;
const PROTOCOL_MINOR: u8 = 0;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FleetPublicationError {
    Unavailable,
    Corrupt,
    Oversized,
    AuthRevoked,
    Cancelled,
}

impl FleetPublicationError {
    fn status(&self) -> StatusCode {
        match self {
            Self::AuthRevoked => StatusCode::UNAUTHORIZED,
            Self::Unavailable | Self::Corrupt | Self::Oversized | Self::Cancelled => {
                StatusCode::SERVICE_UNAVAILABLE
            }
        }
    }
}

/// Wire shape for authenticated fleet publication (remotes only).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FleetHostsPublication {
    pub version: u8,
    pub hosts: Vec<FleetRemoteHostDescriptor>,
}

/// One trusted remote PC descriptor. No cookies, client IDs, or CA material.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FleetRemoteHostDescriptor {
    pub host_public_id: String,
    pub host_public_key: String,
    pub origin: String,
    pub label: String,
    pub generation: u64,
    pub protocol_major: u8,
    pub protocol_minor: u8,
}

/// Validated roster attached to an authenticated HTML response so middleware
/// builds CSP from the same set that was published in meta.
#[derive(Debug, Clone)]
pub(crate) struct PreparedFleetRoster {
    pub publication: FleetHostsPublication,
    pub json: String,
}

/// Self marker + fleet roster from one validated Connect publication boundary.
#[derive(Debug, Clone)]
pub(crate) struct AuthenticatedShellPublication {
    pub self_marker_json: String,
    pub roster: PreparedFleetRoster,
}

/// Captured Connect web publication fence (stable A listener identity generation).
#[derive(Debug, Clone, PartialEq, Eq)]
struct CapturedConnectPublication {
    generation: u64,
    marker_json: String,
    host_public_id: Option<String>,
    host_public_key: Option<String>,
}

pub(crate) trait FleetTrustSource: Send + Sync {
    fn load_remote_descriptors(
        &self,
        publication_generation: u64,
        admission: &RemoteWorkAdmission,
    ) -> Result<Vec<FleetRemoteHostDescriptor>, FleetPublicationError>;
}

#[derive(Debug, Default)]
pub(crate) struct ProductionFleetTrustSource;

impl FleetTrustSource for ProductionFleetTrustSource {
    fn load_remote_descriptors(
        &self,
        publication_generation: u64,
        admission: &RemoteWorkAdmission,
    ) -> Result<Vec<FleetRemoteHostDescriptor>, FleetPublicationError> {
        if admission.cancellation_requested() {
            return Err(FleetPublicationError::Cancelled);
        }
        let store = match RemoteTrustStore::open_under_app_config() {
            Ok(store) => store,
            Err(RemoteTrustError::NotFound) => return Ok(Vec::new()),
            Err(_) => return Err(FleetPublicationError::Unavailable),
        };
        if admission.cancellation_requested() {
            return Err(FleetPublicationError::Cancelled);
        }
        load_descriptors_from_store(&store, publication_generation, admission)
    }
}

fn load_descriptors_from_store(
    store: &RemoteTrustStore,
    publication_generation: u64,
    admission: &RemoteWorkAdmission,
) -> Result<Vec<FleetRemoteHostDescriptor>, FleetPublicationError> {
    if admission.cancellation_requested() {
        return Err(FleetPublicationError::Cancelled);
    }
    let ids = store.list_trusted_host_ids().map_err(|error| match error {
        RemoteTrustError::NotFound => FleetPublicationError::Unavailable,
        RemoteTrustError::Corrupt => FleetPublicationError::Corrupt,
        _ => FleetPublicationError::Unavailable,
    })?;
    // Cap before any DPAPI/decrypt work (store may list up to 256).
    if ids.len() > MAX_FLEET_REMOTES {
        return Err(FleetPublicationError::Oversized);
    }
    let mut remotes = Vec::new();
    for id in ids {
        if admission.cancellation_requested() {
            return Err(FleetPublicationError::Cancelled);
        }
        let (record, mut cookie) = store.load_trusted_host(id).map_err(|error| match error {
            RemoteTrustError::Corrupt => FleetPublicationError::Corrupt,
            RemoteTrustError::NotFound => FleetPublicationError::Corrupt,
            _ => FleetPublicationError::Unavailable,
        })?;
        // Cookie must never leave this scope; zeroize immediately.
        cookie.zeroize();
        drop(cookie);
        if admission.cancellation_requested() {
            return Err(FleetPublicationError::Cancelled);
        }
        if let Some(descriptor) = descriptor_from_trusted_host(record, publication_generation)? {
            remotes.push(descriptor);
        }
    }
    remotes.sort_by(|left, right| left.host_public_id.cmp(&right.host_public_id));
    Ok(remotes)
}

fn descriptor_from_trusted_host(
    record: TrustedHostRecord,
    publication_generation: u64,
) -> Result<Option<FleetRemoteHostDescriptor>, FleetPublicationError> {
    let endpoint = record.endpoint.trim();
    let lower = endpoint.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("ws://") || lower.starts_with("wss://") {
        // Loopback/plaintext/websocket fixtures are not phone-reachable remotes.
        return Ok(None);
    }
    if !lower.starts_with("https://") {
        return Err(FleetPublicationError::Corrupt);
    }
    let validated =
        validate_remote_endpoint(endpoint).map_err(|_| FleetPublicationError::Corrupt)?;
    if !validated.origin().starts_with("https://") {
        return Err(FleetPublicationError::Corrupt);
    }
    // Reject non-root path on the stored endpoint URL (origin only).
    let parsed = url::Url::parse(endpoint).map_err(|_| FleetPublicationError::Corrupt)?;
    let path = parsed.path();
    if path != "/" && !path.is_empty() {
        return Err(FleetPublicationError::Corrupt);
    }
    let origin = validated.origin().to_string();
    let key = record.host_key_pin.as_bytes();
    if key.iter().all(|byte| *byte == 0) {
        return Err(FleetPublicationError::Corrupt);
    }
    let host_public_key = hex_encode_lower(&key);
    if host_public_key.len() != 64 || !host_public_key.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(FleetPublicationError::Corrupt);
    }
    if publication_generation == 0 {
        return Err(FleetPublicationError::Corrupt);
    }
    let host_uuid = Uuid::from_bytes(record.host_public_id);
    if host_uuid.get_version_num() != 7 {
        return Err(FleetPublicationError::Corrupt);
    }
    Ok(Some(FleetRemoteHostDescriptor {
        host_public_id: host_uuid.to_string(),
        host_public_key,
        origin: origin.clone(),
        label: label_from_origin(&origin),
        generation: publication_generation,
        protocol_major: PROTOCOL_MAJOR,
        protocol_minor: PROTOCOL_MINOR,
    }))
}

fn label_from_origin(origin: &str) -> String {
    let host = origin
        .strip_prefix("https://")
        .unwrap_or(origin)
        .split('/')
        .next()
        .unwrap_or(origin);
    let truncated: String = host.chars().take(MAX_FLEET_LABEL_CHARS).collect();
    if truncated.is_empty() {
        "remote-host".to_string()
    } else {
        truncated
    }
}

fn hex_encode_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn validate_descriptor_fields(
    host: &FleetRemoteHostDescriptor,
) -> Result<(), FleetPublicationError> {
    if host.generation == 0
        || host.protocol_major != PROTOCOL_MAJOR
        || host.protocol_minor != PROTOCOL_MINOR
    {
        return Err(FleetPublicationError::Corrupt);
    }
    let uuid = Uuid::parse_str(&host.host_public_id).map_err(|_| FleetPublicationError::Corrupt)?;
    if uuid.get_version_num() != 7 || uuid.to_string() != host.host_public_id {
        return Err(FleetPublicationError::Corrupt);
    }
    if host.host_public_key.len() != 64
        || !host.host_public_key.bytes().all(|b| b.is_ascii_hexdigit())
        || host.host_public_key.bytes().all(|b| b == b'0')
    {
        return Err(FleetPublicationError::Corrupt);
    }
    if !host.origin.starts_with("https://")
        || host.origin.contains('*')
        || host.origin.contains(';')
        || host.origin.bytes().any(|byte| byte <= 0x20 || byte == 0x7f)
    {
        return Err(FleetPublicationError::Corrupt);
    }
    let validated =
        validate_remote_endpoint(&host.origin).map_err(|_| FleetPublicationError::Corrupt)?;
    if validated.origin() != host.origin {
        return Err(FleetPublicationError::Corrupt);
    }
    let parsed = url::Url::parse(&host.origin).map_err(|_| FleetPublicationError::Corrupt)?;
    let path = parsed.path();
    if path != "/" && !path.is_empty() {
        return Err(FleetPublicationError::Corrupt);
    }
    if host.label.chars().count() > MAX_FLEET_LABEL_CHARS || host.label.is_empty() {
        return Err(FleetPublicationError::Corrupt);
    }
    Ok(())
}

/// Re-validate bounds so a custom trust source cannot bypass publication policy.
pub(crate) fn encode_fleet_publication(
    hosts: Vec<FleetRemoteHostDescriptor>,
) -> Result<PreparedFleetRoster, FleetPublicationError> {
    if hosts.len() > MAX_FLEET_REMOTES {
        return Err(FleetPublicationError::Oversized);
    }
    let mut seen = BTreeSet::new();
    let mut validated = Vec::with_capacity(hosts.len());
    for host in hosts {
        validate_descriptor_fields(&host)?;
        if !seen.insert(host.host_public_id.clone()) {
            continue; // dedup by hostPublicId
        }
        validated.push(host);
    }
    if validated.len() > MAX_FLEET_REMOTES {
        return Err(FleetPublicationError::Oversized);
    }
    validated.sort_by(|left, right| left.host_public_id.cmp(&right.host_public_id));
    let publication = FleetHostsPublication {
        version: 1,
        hosts: validated,
    };
    let json =
        serde_json::to_string(&publication).map_err(|_| FleetPublicationError::Unavailable)?;
    if json.len() > MAX_FLEET_JSON_BYTES {
        return Err(FleetPublicationError::Oversized);
    }
    Ok(PreparedFleetRoster { publication, json })
}

fn resolve_trust_source(state: &WebState) -> Arc<dyn FleetTrustSource> {
    #[cfg(test)]
    if let Some(source) = state.fleet_trust_source.as_ref() {
        return source.clone();
    }
    let _ = state;
    Arc::new(ProductionFleetTrustSource)
}

fn resolve_web_publication(
    state: &WebState,
) -> Result<&ConnectWebPublication, FleetPublicationError> {
    // This fixture seam is absent from production builds.
    #[cfg(test)]
    if let Some(publication) = state.fleet_test_publication.as_ref() {
        return Ok(publication);
    }
    let startup = state
        .connect_startup
        .as_ref()
        .ok_or(FleetPublicationError::Unavailable)?;
    startup
        .require_bound_listener()
        .map_err(|_| FleetPublicationError::Unavailable)?;
    Ok(startup.web_publication())
}

fn capture_bound_publication(
    state: &WebState,
) -> Result<CapturedConnectPublication, FleetPublicationError> {
    let publication = resolve_web_publication(state)?;
    if !publication.is_published() {
        return Err(FleetPublicationError::Unavailable);
    }
    let generation = publication.generation();
    if generation == 0 {
        return Err(FleetPublicationError::Unavailable);
    }
    let marker = publication
        .marker()
        .ok_or(FleetPublicationError::Unavailable)?;
    if marker.generation != generation {
        return Err(FleetPublicationError::Unavailable);
    }
    let marker_json = publication
        .marker_json()
        .ok_or(FleetPublicationError::Unavailable)?;
    Ok(CapturedConnectPublication {
        generation,
        marker_json,
        host_public_id: marker.host_public_id,
        host_public_key: marker.host_public_key,
    })
}

fn publication_still_current(state: &WebState, captured: &CapturedConnectPublication) -> bool {
    let Ok(publication) = resolve_web_publication(state) else {
        return false;
    };
    if !publication.is_published() || publication.generation() != captured.generation {
        return false;
    }
    let Some(marker) = publication.marker() else {
        return false;
    };
    if marker.generation != captured.generation
        || marker.host_public_id != captured.host_public_id
        || marker.host_public_key != captured.host_public_key
    {
        return false;
    }
    matches!(publication.marker_json(), Some(json) if json == captured.marker_json)
}

fn auth_still_valid(
    state: &WebState,
    headers: &HeaderMap,
    expected: &ValidatedWebAuthentication,
) -> bool {
    match validate_authenticated_request(state, headers) {
        Ok(auth) => {
            auth.client_id == expected.client_id
                && auth.cookie_secret_hex == expected.cookie_secret_hex
        }
        Err(_) => false,
    }
}

/// Load fleet under the current bound Connect publication fence.
pub(crate) async fn load_authenticated_fleet(
    state: &WebState,
    headers: &HeaderMap,
) -> Result<AuthenticatedShellPublication, FleetPublicationError> {
    let authentication =
        validate_authenticated_request(state, headers).map_err(|error| match error {
            WebAuthError::Unauthorized => FleetPublicationError::AuthRevoked,
            WebAuthError::Durability => FleetPublicationError::Unavailable,
        })?;
    // Fence BEFORE scheduling trust I/O.
    let captured = capture_bound_publication(state)?;
    let generation = captured.generation;
    let source = resolve_trust_source(state);
    let deadline = Instant::now() + FLEET_LOAD_DEADLINE;
    let Ok(mut work) = RemoteBlockingWork::spawn("fleet-trust-read", deadline, move |admission| {
        // Read-only: no mutation admission; honour cancellation between records.
        source.load_remote_descriptors(generation, &admission)
    }) else {
        return Err(FleetPublicationError::Unavailable);
    };
    let loaded = match work.wait().await {
        Ok(Ok(hosts)) => hosts,
        Ok(Err(error)) => return Err(error),
        Err(_) => return Err(FleetPublicationError::Unavailable),
    };
    if !auth_still_valid(state, headers, &authentication) {
        return Err(FleetPublicationError::AuthRevoked);
    }
    if !publication_still_current(state, &captured) {
        return Err(FleetPublicationError::Unavailable);
    }
    let roster = encode_fleet_publication(loaded)?;
    Ok(AuthenticatedShellPublication {
        self_marker_json: captured.marker_json,
        roster,
    })
}

pub(crate) async fn load_prepared_fleet_roster(
    state: &WebState,
    headers: &HeaderMap,
) -> Result<PreparedFleetRoster, FleetPublicationError> {
    Ok(load_authenticated_fleet(state, headers).await?.roster)
}

pub(crate) async fn fleet_hosts_handler(
    State(state): State<Arc<WebState>>,
    headers: HeaderMap,
) -> Response {
    match load_prepared_fleet_roster(&state, &headers).await {
        Ok(roster) => match serde_json::to_vec(&roster.publication) {
            Ok(bytes) => (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, "application/json"),
                    (header::CACHE_CONTROL, "no-store"),
                ],
                bytes,
            )
                .into_response(),
            Err(_) => (
                StatusCode::SERVICE_UNAVAILABLE,
                [(header::CACHE_CONTROL, "no-store")],
                "fleet publication unavailable",
            )
                .into_response(),
        },
        Err(FleetPublicationError::AuthRevoked) => (
            StatusCode::UNAUTHORIZED,
            [(header::CACHE_CONTROL, "no-store")],
            "not paired",
        )
            .into_response(),
        Err(error) => (
            error.status(),
            [(header::CACHE_CONTROL, "no-store")],
            "fleet publication unavailable",
        )
            .into_response(),
    }
}

/// Build CSP connect-src from the same validated Host + prepared roster.
pub(crate) fn content_security_policy_with_fleet(
    websocket_authority: Option<&str>,
    fleet: Option<&PreparedFleetRoster>,
) -> Result<HeaderValue, ()> {
    let mut connect = String::from("connect-src 'self'");
    if let Some(authority) = websocket_authority {
        if authority.is_empty()
            || authority.contains(' ')
            || authority.contains(';')
            || authority.contains('*')
        {
            return Err(());
        }
        connect.push_str(&format!(" ws://{authority} wss://{authority}"));
    }
    if let Some(roster) = fleet {
        for host in &roster.publication.hosts {
            let origin = &host.origin;
            if !origin.starts_with("https://")
                || origin.contains(' ')
                || origin.contains(';')
                || origin.contains('*')
            {
                return Err(());
            }
            let wss = origin.replacen("https://", "wss://", 1);
            connect.push(' ');
            connect.push_str(origin);
            connect.push(' ');
            connect.push_str(&wss);
        }
    }
    connect.push(';');
    let policy = format!(
        "default-src 'self'; base-uri 'self'; {connect} \
font-src 'self' data:; img-src 'self' data: blob:; manifest-src 'self'; \
object-src 'none'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self' 'unsafe-inline'; \
worker-src 'self' blob:; frame-ancestors 'none'; form-action 'self'"
    );
    HeaderValue::from_str(&policy).map_err(|_| ())
}

#[cfg(test)]
pub(crate) mod tests_support {
    use super::*;
    use std::sync::Mutex;

    pub(crate) fn sample_remote_for_tests(id_byte: u8) -> FleetRemoteHostDescriptor {
        let mut id = *Uuid::now_v7().as_bytes();
        id[0] = id_byte;
        id[6] = (id[6] & 0x0f) | 0x70;
        id[8] = (id[8] & 0x3f) | 0x80;
        FleetRemoteHostDescriptor {
            host_public_id: Uuid::from_bytes(id).to_string(),
            host_public_key: "ab".repeat(32),
            origin: format!("https://remote{id_byte}.example:8443"),
            label: format!("remote{id_byte}.example:8443"),
            generation: 1,
            protocol_major: 1,
            protocol_minor: 0,
        }
    }

    pub(crate) fn static_source(
        hosts: Vec<FleetRemoteHostDescriptor>,
    ) -> Arc<dyn FleetTrustSource> {
        Arc::new(StaticSource {
            hosts: Mutex::new(Ok(hosts)),
        })
    }

    pub(crate) fn published_test_fence() -> ConnectWebPublication {
        let publication = ConnectWebPublication::new("/api/connect");
        publication.publish();
        publication
    }

    struct StaticSource {
        hosts: Mutex<Result<Vec<FleetRemoteHostDescriptor>, FleetPublicationError>>,
    }

    impl FleetTrustSource for StaticSource {
        fn load_remote_descriptors(
            &self,
            publication_generation: u64,
            admission: &RemoteWorkAdmission,
        ) -> Result<Vec<FleetRemoteHostDescriptor>, FleetPublicationError> {
            if admission.cancellation_requested() {
                return Err(FleetPublicationError::Cancelled);
            }
            let mut hosts = self.hosts.lock().unwrap().clone()?;
            for host in &mut hosts {
                host.generation = publication_generation;
            }
            Ok(hosts)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    struct StaticSource {
        hosts: Mutex<Result<Vec<FleetRemoteHostDescriptor>, FleetPublicationError>>,
    }

    impl FleetTrustSource for StaticSource {
        fn load_remote_descriptors(
            &self,
            publication_generation: u64,
            admission: &RemoteWorkAdmission,
        ) -> Result<Vec<FleetRemoteHostDescriptor>, FleetPublicationError> {
            if admission.cancellation_requested() {
                return Err(FleetPublicationError::Cancelled);
            }
            let mut hosts = self.hosts.lock().unwrap().clone()?;
            for host in &mut hosts {
                host.generation = publication_generation;
            }
            Ok(hosts)
        }
    }

    fn sample_remote(id_byte: u8) -> FleetRemoteHostDescriptor {
        tests_support::sample_remote_for_tests(id_byte)
    }

    fn v7_id(seed: u8) -> [u8; 16] {
        let mut id = *Uuid::now_v7().as_bytes();
        id[0] = seed;
        id[6] = (id[6] & 0x0f) | 0x70;
        id[8] = (id[8] & 0x3f) | 0x80;
        id
    }

    #[test]
    fn encode_rejects_oversized_roster_json() {
        let mut huge = sample_remote(1);
        huge.label = "z".repeat(MAX_FLEET_JSON_BYTES);
        // label bound fails first
        assert!(matches!(
            encode_fleet_publication(vec![huge]),
            Err(FleetPublicationError::Corrupt | FleetPublicationError::Oversized)
        ));
    }

    #[test]
    fn encode_bounds_and_omits_secrets_from_shape() {
        let roster = encode_fleet_publication(vec![sample_remote(2)]).expect("encode");
        assert!(roster.json.len() <= MAX_FLEET_JSON_BYTES);
        assert!(roster.json.contains("\"version\":1"));
        assert!(!roster.json.contains("cookie"));
        assert!(!roster.json.contains("clientId"));
        assert!(!roster.json.contains("assigned"));
        assert!(!roster.json.contains("BEGIN CERTIFICATE"));
        let csp =
            content_security_policy_with_fleet(Some("a.example:8443"), Some(&roster)).expect("csp");
        let text = csp.to_str().unwrap();
        assert!(text.contains("https://remote2.example:8443"));
        assert!(text.contains("wss://remote2.example:8443"));
        assert!(!text.contains("ws:*"));
        assert!(!text.contains("wss:*"));
    }

    #[test]
    fn repeated_encode_of_same_descriptors_is_byte_identical() {
        let hosts = vec![sample_remote(3), sample_remote(4)];
        let first = encode_fleet_publication(hosts.clone()).unwrap();
        let second = encode_fleet_publication(hosts).unwrap();
        assert_eq!(first.json, second.json);
        assert_eq!(first.publication, second.publication);
    }

    #[test]
    fn http_endpoints_are_skipped_not_emitted() {
        let record = TrustedHostRecord {
            host_public_id: v7_id(9),
            host_key_pin: crate::connect::ConnectNoiseStaticPublicKey::from_bytes([3_u8; 32])
                .unwrap(),
            endpoint: "http://127.0.0.1:43872".into(),
            connect_path: "/api/connect".into(),
            assigned_client_id: crate::domain::ClientId::new(),
            additional_ca_pem: Some("SECRET-CA".into()),
        };
        assert_eq!(descriptor_from_trusted_host(record, 1).unwrap(), None);
    }

    #[test]
    fn https_endpoint_with_nonroot_path_fails_closed() {
        let record = TrustedHostRecord {
            host_public_id: v7_id(7),
            host_key_pin: crate::connect::ConnectNoiseStaticPublicKey::from_bytes([6_u8; 32])
                .unwrap(),
            endpoint: "https://remote.example:8443/api/connect".into(),
            connect_path: "/api/connect".into(),
            assigned_client_id: crate::domain::ClientId::new(),
            additional_ca_pem: None,
        };
        assert!(descriptor_from_trusted_host(record, 1).is_err());
    }

    #[test]
    fn non_v7_host_id_fails_closed() {
        let record = TrustedHostRecord {
            host_public_id: [1_u8; 16],
            host_key_pin: crate::connect::ConnectNoiseStaticPublicKey::from_bytes([5_u8; 32])
                .unwrap(),
            endpoint: "https://remote.example:8443".into(),
            connect_path: "/api/connect".into(),
            assigned_client_id: crate::domain::ClientId::new(),
            additional_ca_pem: None,
        };
        assert!(descriptor_from_trusted_host(record, 1).is_err());
    }

    #[test]
    fn bad_https_origin_with_userinfo_fails_closed() {
        let record = TrustedHostRecord {
            host_public_id: v7_id(8),
            host_key_pin: crate::connect::ConnectNoiseStaticPublicKey::from_bytes([4_u8; 32])
                .unwrap(),
            endpoint: "https://user:pass@evil.example".into(),
            connect_path: "/api/connect".into(),
            assigned_client_id: crate::domain::ClientId::new(),
            additional_ca_pem: None,
        };
        assert!(descriptor_from_trusted_host(record, 1).is_err());
    }

    #[test]
    fn invalid_custom_source_cannot_widen_csp_via_encode() {
        let mut bad = sample_remote(1);
        bad.origin = "https://*.evil.example".into();
        assert!(encode_fleet_publication(vec![bad]).is_err());
        let mut pathy = sample_remote(2);
        pathy.origin = "https://evil.example/app".into();
        assert!(encode_fleet_publication(vec![pathy]).is_err());
    }

    #[test]
    fn more_than_max_remotes_fails_at_encode_without_silent_truncate() {
        let mut hosts = Vec::new();
        for index in 0..(MAX_FLEET_REMOTES + 1) {
            hosts.push(sample_remote(index as u8 + 1));
        }
        assert!(matches!(
            encode_fleet_publication(hosts),
            Err(FleetPublicationError::Oversized)
        ));
    }

    #[test]
    #[cfg(windows)]
    fn id_count_cap_runs_before_record_decrypt_loop() {
        let config = crate::persistence::app_config_dir().unwrap();
        std::fs::create_dir_all(&config).unwrap();
        let root = tempfile::Builder::new()
            .prefix("fleet-cap-")
            .tempdir_in(config)
            .unwrap();
        let store = RemoteTrustStore::open(root.path().to_path_buf()).unwrap();
        let mut last_path = None;
        for index in 1..=MAX_FLEET_REMOTES + 1 {
            let path = store
                .root()
                .join("hosts")
                .join(format!("{index:032x}.json"));
            // A load of ANY record would fail Corrupt before reaching DPAPI.
            crate::remote::atomic_write_remote_state_bytes(&path, b"not a trusted host record")
                .unwrap();
            last_path = Some(path);
        }
        let run = |store: RemoteTrustStore| {
            RemoteBlockingWork::spawn(
                "fleet-production-cap-test",
                Instant::now() + Duration::from_secs(5),
                move |admission| load_descriptors_from_store(&store, 1, &admission),
            )
            .unwrap()
            .wait_blocking()
            .unwrap()
        };
        assert!(matches!(
            run(RemoteTrustStore::open(root.path().to_path_buf()).unwrap()),
            Err(FleetPublicationError::Oversized)
        ));
        std::fs::remove_file(last_path.unwrap()).unwrap();
        // Control: with 15 IDs the exact same production path attempts a load.
        assert!(matches!(run(store), Err(FleetPublicationError::Corrupt)));
    }

    struct CountingCancelSource {
        loads_started: AtomicUsize,
        cancel_after: usize,
    }

    impl FleetTrustSource for CountingCancelSource {
        fn load_remote_descriptors(
            &self,
            publication_generation: u64,
            admission: &RemoteWorkAdmission,
        ) -> Result<Vec<FleetRemoteHostDescriptor>, FleetPublicationError> {
            let _ = publication_generation;
            // Mimic per-record cancel checks without a real store.
            for index in 0..5 {
                if admission.cancellation_requested() || index >= self.cancel_after {
                    return Err(FleetPublicationError::Cancelled);
                }
                self.loads_started.fetch_add(1, Ordering::SeqCst);
            }
            Ok(vec![sample_remote(1)])
        }
    }

    #[test]
    fn cancel_during_read_halts_further_record_work() {
        let source = Arc::new(CountingCancelSource {
            loads_started: AtomicUsize::new(0),
            cancel_after: 2,
        });
        // Build a real admission via RemoteBlockingWork and cancel by short deadline.
        let deadline = Instant::now() + Duration::from_millis(1);
        std::thread::sleep(Duration::from_millis(5));
        let worker_source = source.clone();
        let result = RemoteBlockingWork::spawn("fleet-cancel-test", deadline, move |admission| {
            worker_source.load_remote_descriptors(3, &admission)
        })
        .and_then(|mut work| work.wait_blocking());
        match result {
            Ok(Err(FleetPublicationError::Cancelled)) => {}
            Err(_) => {} // deadline at wait layer also acceptable
            Ok(Ok(_)) => panic!("cancel must not return a roster"),
            Ok(Err(other)) => panic!("unexpected {other:?}"),
        }
        assert!(source.loads_started.load(Ordering::SeqCst) <= 2);
    }

    fn paired_state_with_fence(
        profile: &str,
        server_id: &str,
        client_id: &str,
        source: Arc<dyn FleetTrustSource>,
        fence: ConnectWebPublication,
    ) -> (
        crate::remote::RemoteHostService,
        Arc<super::super::WebState>,
        HeaderMap,
    ) {
        let _ = profile;
        let mut config = crate::remote::RemoteHostConfig::default();
        config.server_id = server_id.into();
        config.web.enabled = true;
        config.web.ensure_secrets();
        config
            .web
            .paired_clients
            .push(super::super::PairedWebClient {
                client_id: client_id.into(),
                browser_install_id: "install".into(),
                label: "Phone".into(),
                permitted_origin: None,
                ..super::super::PairedWebClient::default()
            });
        let service =
            crate::remote::RemoteHostService::new_web_only(config).expect("web auth shell");
        let state = Arc::new(super::super::WebState {
            inner: Arc::downgrade(&service.inner),
            listener_generation: 1,
            pairing_attempts: Arc::new(std::sync::Mutex::new(Default::default())),
            connect_startup: None,
            host_requests: crate::connect::ConnectHostRequestSlot::new(),
            cross_origin: Arc::new(std::sync::Mutex::new(Default::default())),
            cross_origin_rate: Arc::new(std::sync::Mutex::new(Default::default())),
            fleet_trust_source: Some(source),
            fleet_test_publication: Some(fence),
        });
        let secret = service.config().web.cookie_secret_hex.clone();
        let signed = super::super::sign_cookie(&secret, client_id).unwrap();
        let cookie_name = super::super::cookie_name_for_server_id(server_id);
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            format!("{cookie_name}={signed}").parse().unwrap(),
        );
        (service, state, headers)
    }

    #[tokio::test]
    async fn unchanged_repeated_publication_json_is_byte_identical() {
        let _profile = crate::remote::test_support::TestProfileGuard::new("fleet-stable-json");
        let fence = tests_support::published_test_fence();
        let remote = sample_remote(5);
        let (_service, state, headers) = paired_state_with_fence(
            "fleet-stable-json",
            "fleet-stable",
            "fleet-browser",
            Arc::new(StaticSource {
                hosts: Mutex::new(Ok(vec![remote])),
            }),
            fence,
        );
        let first = load_prepared_fleet_roster(&state, &headers)
            .await
            .expect("first");
        let second = load_prepared_fleet_roster(&state, &headers)
            .await
            .expect("second");
        assert_eq!(first.json, second.json);
        assert_eq!(
            first.publication.hosts[0].generation,
            second.publication.hosts[0].generation
        );
    }

    #[tokio::test]
    async fn unauthenticated_api_never_returns_remote_descriptors() {
        let _profile = crate::remote::test_support::TestProfileGuard::new("fleet-unauth-api");
        let mut config = crate::remote::RemoteHostConfig::default();
        config.web.enabled = true;
        config.web.ensure_secrets();
        let service =
            crate::remote::RemoteHostService::new_web_only(config).expect("web auth shell");
        let fence = tests_support::published_test_fence();
        let remote = sample_remote(4);
        let state = Arc::new(super::super::WebState {
            inner: Arc::downgrade(&service.inner),
            listener_generation: 1,
            pairing_attempts: Arc::new(std::sync::Mutex::new(Default::default())),
            connect_startup: None,
            host_requests: crate::connect::ConnectHostRequestSlot::new(),
            cross_origin: Arc::new(std::sync::Mutex::new(Default::default())),
            cross_origin_rate: Arc::new(std::sync::Mutex::new(Default::default())),
            fleet_trust_source: Some(Arc::new(StaticSource {
                hosts: Mutex::new(Ok(vec![remote])),
            })),
            fleet_test_publication: Some(fence),
        });
        let response = fleet_hosts_handler(State(state), HeaderMap::new()).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(!String::from_utf8_lossy(&body).contains("remote4.example"));
    }

    #[tokio::test]
    async fn publication_revoked_during_load_yields_no_roster() {
        let _profile = crate::remote::test_support::TestProfileGuard::new("fleet-fence-revoke");
        let fence = tests_support::published_test_fence();
        struct RevokeFenceSource {
            fence: ConnectWebPublication,
            hosts: Vec<FleetRemoteHostDescriptor>,
        }
        impl FleetTrustSource for RevokeFenceSource {
            fn load_remote_descriptors(
                &self,
                publication_generation: u64,
                admission: &RemoteWorkAdmission,
            ) -> Result<Vec<FleetRemoteHostDescriptor>, FleetPublicationError> {
                let _ = admission;
                self.fence.revoke();
                let mut hosts = self.hosts.clone();
                for host in &mut hosts {
                    host.generation = publication_generation;
                }
                Ok(hosts)
            }
        }
        let (_service, state, headers) = paired_state_with_fence(
            "fleet-fence-revoke",
            "fleet-revoke-fence",
            "fleet-revoked",
            Arc::new(RevokeFenceSource {
                fence: fence.clone(),
                hosts: vec![sample_remote(6)],
            }),
            fence,
        );
        let err = load_authenticated_fleet(&state, &headers)
            .await
            .expect_err("revoked fence");
        assert!(matches!(
            err,
            FleetPublicationError::Unavailable | FleetPublicationError::Cancelled
        ));
    }

    #[tokio::test]
    async fn auth_revoked_during_lookup_returns_no_roster() {
        let _profile = crate::remote::test_support::TestProfileGuard::new("fleet-auth-revoke");
        let fence = tests_support::published_test_fence();
        let mut config = crate::remote::RemoteHostConfig::default();
        config.server_id = "fleet-revoke".into();
        config.web.enabled = true;
        config.web.ensure_secrets();
        let client_id = "fleet-revoked".to_string();
        config
            .web
            .paired_clients
            .push(super::super::PairedWebClient {
                client_id: client_id.clone(),
                browser_install_id: "install".into(),
                label: "Phone".into(),
                permitted_origin: None,
                ..super::super::PairedWebClient::default()
            });
        let service =
            crate::remote::RemoteHostService::new_web_only(config).expect("web auth shell");
        let inner = Arc::downgrade(&service.inner);
        struct RevokeOnLoad {
            inner: std::sync::Weak<crate::remote::RemoteHostInner>,
            hosts: Vec<FleetRemoteHostDescriptor>,
        }
        impl FleetTrustSource for RevokeOnLoad {
            fn load_remote_descriptors(
                &self,
                publication_generation: u64,
                admission: &RemoteWorkAdmission,
            ) -> Result<Vec<FleetRemoteHostDescriptor>, FleetPublicationError> {
                let _ = admission;
                if let Some(inner) = self.inner.upgrade() {
                    let _ = crate::remote::mutate_host_config_if(
                        &inner,
                        |_| true,
                        |config| {
                            config.web.paired_clients.clear();
                        },
                    );
                }
                let mut hosts = self.hosts.clone();
                for host in &mut hosts {
                    host.generation = publication_generation;
                }
                Ok(hosts)
            }
        }
        let state = Arc::new(super::super::WebState {
            inner: Arc::downgrade(&service.inner),
            listener_generation: 1,
            pairing_attempts: Arc::new(std::sync::Mutex::new(Default::default())),
            connect_startup: None,
            host_requests: crate::connect::ConnectHostRequestSlot::new(),
            cross_origin: Arc::new(std::sync::Mutex::new(Default::default())),
            cross_origin_rate: Arc::new(std::sync::Mutex::new(Default::default())),
            fleet_trust_source: Some(Arc::new(RevokeOnLoad {
                inner,
                hosts: vec![sample_remote(6)],
            })),
            fleet_test_publication: Some(fence),
        });
        let secret = service.config().web.cookie_secret_hex.clone();
        let signed = super::super::sign_cookie(&secret, &client_id).unwrap();
        let cookie_name = super::super::cookie_name_for_server_id("fleet-revoke");
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            format!("{cookie_name}={signed}").parse().unwrap(),
        );
        let response = fleet_hosts_handler(State(state), headers).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(!String::from_utf8_lossy(&body).contains("remote6.example"));
    }

    #[test]
    fn csp_builder_rejects_wildcard_or_unsafe_origin() {
        let mut roster = encode_fleet_publication(vec![sample_remote(1)]).unwrap();
        roster.publication.hosts[0].origin = "https://*.evil.example".into();
        assert!(content_security_policy_with_fleet(Some("a.example"), Some(&roster)).is_err());
    }

    #[test]
    fn missing_bound_publication_fails_closed() {
        let mut config = crate::remote::RemoteHostConfig::default();
        config.web.enabled = true;
        config.web.ensure_secrets();
        let service =
            crate::remote::RemoteHostService::new_web_only(config).expect("web auth shell");
        let state = super::super::WebState {
            inner: Arc::downgrade(&service.inner),
            listener_generation: 1,
            pairing_attempts: Arc::new(std::sync::Mutex::new(Default::default())),
            connect_startup: None,
            host_requests: crate::connect::ConnectHostRequestSlot::new(),
            cross_origin: Arc::new(std::sync::Mutex::new(Default::default())),
            cross_origin_rate: Arc::new(std::sync::Mutex::new(Default::default())),
            fleet_trust_source: None,
            fleet_test_publication: None,
        };
        assert!(capture_bound_publication(&state).is_err());
    }
}
