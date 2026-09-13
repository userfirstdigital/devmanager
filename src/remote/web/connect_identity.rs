//! Host-side paired-browser Noise static-public trust pins.
//!
//! Pins live inside [`super::WebConfig`] persistence (`remote.json`), keyed by
//! the paired browser cookie `client_id`. Each pin stores the Noise public key
//! and a host-assigned native [`ClientId`] (UUIDv7). This module never stores
//! private keys or cookie signing secrets.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Weak};

use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use tokio::sync::watch;

use super::super::{
    mutate_host_config_if, HostConfigAdmissionError, RemoteHostConfig, RemoteHostInner,
};
use super::auth::{hex_decode, hex_encode, PairedWebClient};
use crate::connect::BrowserEnrollmentMetadata;
use crate::domain::ClientId;

/// Exact hex character count for a persisted Noise public key.
pub const CONNECT_PEER_PUBLIC_KEY_HEX_CHARS: usize = 64;
/// Noise static public key length in bytes.
pub const CONNECT_PEER_PUBLIC_KEY_BYTES: usize = 32;
/// Maximum paired-cookie client id / pin-map key length in bytes.
pub const MAX_PAIRED_COOKIE_CLIENT_ID_BYTES: usize = 256;
/// Maximum number of persisted Connect peer pins.
pub const MAX_CONNECT_PEER_PINS: usize = 256;

/// Bounded Noise static public key persisted for a paired browser.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ConnectPeerPublicKey([u8; CONNECT_PEER_PUBLIC_KEY_BYTES]);

impl ConnectPeerPublicKey {
    pub fn from_bytes(
        bytes: [u8; CONNECT_PEER_PUBLIC_KEY_BYTES],
    ) -> Result<Self, ConnectPeerTrustError> {
        if bytes.iter().all(|byte| *byte == 0) {
            return Err(ConnectPeerTrustError::PeerKeyRejected);
        }
        Ok(Self(bytes))
    }

    pub const fn as_bytes(self) -> [u8; CONNECT_PEER_PUBLIC_KEY_BYTES] {
        self.0
    }
}

impl fmt::Debug for ConnectPeerPublicKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ConnectPeerPublicKey")
            .field("len", &CONNECT_PEER_PUBLIC_KEY_BYTES)
            .finish()
    }
}

impl Serialize for ConnectPeerPublicKey {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&hex_encode(&self.0))
    }
}

impl<'de> Deserialize<'de> for ConnectPeerPublicKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct KeyVisitor;

        impl<'de> Visitor<'de> for KeyVisitor {
            type Value = ConnectPeerPublicKey;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str(
                    "a 32-byte Noise public key as exactly 64 hex chars or a 32-byte sequence",
                )
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                if value.len() != CONNECT_PEER_PUBLIC_KEY_HEX_CHARS {
                    return Err(E::custom(
                        "connect peer public key must be exactly 64 hex characters",
                    ));
                }
                let decoded = hex_decode(value)
                    .ok_or_else(|| E::custom("connect peer public key must be hexadecimal"))?;
                let bytes: [u8; CONNECT_PEER_PUBLIC_KEY_BYTES] = decoded
                    .as_slice()
                    .try_into()
                    .map_err(|_| E::custom("connect peer public key must be exactly 32 bytes"))?;
                ConnectPeerPublicKey::from_bytes(bytes).map_err(E::custom)
            }

            fn visit_bytes<E>(self, value: &[u8]) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                let bytes: [u8; CONNECT_PEER_PUBLIC_KEY_BYTES] = value
                    .try_into()
                    .map_err(|_| E::custom("connect peer public key must be exactly 32 bytes"))?;
                ConnectPeerPublicKey::from_bytes(bytes).map_err(E::custom)
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut bytes = [0_u8; CONNECT_PEER_PUBLIC_KEY_BYTES];
                let mut seen = 0_usize;
                while let Some(byte) = seq.next_element::<u8>()? {
                    if seen >= CONNECT_PEER_PUBLIC_KEY_BYTES {
                        return Err(de::Error::custom(
                            "connect peer public key sequence exceeds 32 bytes",
                        ));
                    }
                    bytes[seen] = byte;
                    seen += 1;
                }
                if seen != CONNECT_PEER_PUBLIC_KEY_BYTES {
                    return Err(de::Error::custom(
                        "connect peer public key sequence must contain exactly 32 bytes",
                    ));
                }
                ConnectPeerPublicKey::from_bytes(bytes).map_err(de::Error::custom)
            }
        }

        deserializer.deserialize_any(KeyVisitor)
    }
}

/// Host-assigned Connect trust pin for one paired browser cookie identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConnectPeerPin {
    pub public_key: ConnectPeerPublicKey,
    pub client_id: ClientId,
}

/// Typed, redacted trust failure. Never includes key material.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectPeerTrustError {
    WebDisabled,
    NotPaired,
    PeerKeyRejected,
    KeyMismatchRequiresRepair,
    Persistence,
    ConfigUnavailable,
    InvalidClientId,
    HostStopped,
    Capacity,
}

impl fmt::Display for ConnectPeerTrustError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WebDisabled => formatter.write_str("browser remote access is disabled"),
            Self::NotPaired => formatter.write_str("browser client is not currently paired"),
            Self::PeerKeyRejected => formatter.write_str("peer public key was rejected"),
            Self::KeyMismatchRequiresRepair => formatter
                .write_str("paired browser Noise key changed; revoke and re-pair to repair trust"),
            Self::Persistence => formatter.write_str("host config persistence failed"),
            Self::ConfigUnavailable => formatter.write_str("host config unavailable"),
            Self::InvalidClientId => formatter.write_str("paired client id is invalid"),
            Self::HostStopped => formatter.write_str("remote host is stopped"),
            Self::Capacity => formatter.write_str("paired Connect device limit reached"),
        }
    }
}

impl std::error::Error for ConnectPeerTrustError {}

impl From<HostConfigAdmissionError> for ConnectPeerTrustError {
    fn from(_error: HostConfigAdmissionError) -> Self {
        Self::Persistence
    }
}

/// Process-local authorization lease for one authenticated paired browser peer.
///
/// Holds only a [`Weak`] host handle so a websocket path cannot keep the host
/// alive. Authorization is re-checked against current WebConfig membership and
/// the exact pinned public key plus host-assigned [`ClientId`]. Optional identity
/// authority invalidation closes idle duplexes on revoke/repair/host rotation.
/// Captured [`Self::permitted_origin`] is re-checked on every config revision so
/// origin or key removal closes idle duplexes.
#[derive(Clone)]
pub struct ConnectPeerLease {
    inner: Weak<RemoteHostInner>,
    paired_cookie_client_id: String,
    assigned_client_id: ClientId,
    peer_public: ConnectPeerPublicKey,
    /// `None` = same-origin cookie route; `Some(origin)` = cross-origin route.
    permitted_origin: Option<String>,
    revision_rx: watch::Receiver<u64>,
    identity_rx: Option<watch::Receiver<u64>>,
    identity_generation: u64,
}

impl fmt::Debug for ConnectPeerLease {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ConnectPeerLease")
            .field("paired_cookie_client_id", &self.paired_cookie_client_id)
            .field("assigned_client_id", &self.assigned_client_id)
            .field("peer_public", &self.peer_public)
            .field("permitted_origin", &self.permitted_origin)
            .finish()
    }
}

impl ConnectPeerLease {
    /// Paired browser cookie client id used as the pin map key.
    pub(crate) fn paired_client_id(&self) -> &str {
        &self.paired_cookie_client_id
    }

    /// Host-assigned native Connect client identity (UUIDv7).
    pub(crate) fn client_id(&self) -> ClientId {
        self.assigned_client_id
    }

    pub(crate) fn peer_public(&self) -> ConnectPeerPublicKey {
        self.peer_public
    }

    /// Attach canonical identity invalidation using the generation captured
    /// when the credential/authority was minted. A revocation between mint and
    /// attach must fail the lease closed; do not advance the baseline via
    /// `borrow_and_update` at attach time.
    pub(crate) fn with_identity_invalidation(
        mut self,
        identity_rx: watch::Receiver<u64>,
        captured_generation: u64,
    ) -> Self {
        self.identity_generation = captured_generation;
        self.identity_rx = Some(identity_rx);
        self
    }

    /// Synchronous authorization against the current committed WebConfig and
    /// optional identity authority generation.
    pub(crate) fn is_authorized(&self) -> bool {
        if let Some(identity_rx) = &self.identity_rx {
            if self.identity_generation == u64::MAX
                || *identity_rx.borrow() != self.identity_generation
            {
                return false;
            }
        }
        let Some(inner) = self.inner.upgrade() else {
            return false;
        };
        if inner.stop_flag.load(Ordering::Acquire) {
            return false;
        }
        let Ok(config) = inner.config.read() else {
            return false;
        };
        peer_is_authorized(
            &config,
            &self.paired_cookie_client_id,
            self.peer_public,
            self.assigned_client_id,
            self.permitted_origin.as_deref(),
        )
    }

    pub(crate) fn permitted_origin(&self) -> Option<&str> {
        self.permitted_origin.as_deref()
    }

    /// Wait until this lease is no longer authorized. Wakes on host-config
    /// revision notifications after durable commits, identity authority
    /// invalidation, or host stop; does not poll or spawn.
    pub(crate) async fn revoked(&mut self) {
        loop {
            if !self.is_authorized() {
                return;
            }
            tokio::select! {
                biased;
                result = self.revision_rx.changed() => {
                    if result.is_err() {
                        return;
                    }
                }
                result = async {
                    match self.identity_rx.as_mut() {
                        Some(rx) => rx.changed().await,
                        None => std::future::pending().await,
                    }
                } => {
                    if result.is_err() {
                        return;
                    }
                }
            }
        }
    }
}

/// Read paired-browser enrollment metadata from committed WebConfig.
///
/// Never trusts Noise claim fields for browser_install_id / nickname / label.
pub(crate) fn paired_browser_enrollment_metadata(
    inner: &RemoteHostInner,
    paired_client_id: &str,
) -> Result<BrowserEnrollmentMetadata, ConnectPeerTrustError> {
    let paired_client_id = validate_paired_cookie_client_id(paired_client_id)?;
    let config = inner
        .config
        .read()
        .map_err(|_| ConnectPeerTrustError::ConfigUnavailable)?;
    if !config.web.enabled {
        return Err(ConnectPeerTrustError::WebDisabled);
    }
    let client = config
        .web
        .paired_clients
        .iter()
        .find(|client| client.client_id == paired_client_id)
        .ok_or(ConnectPeerTrustError::NotPaired)?;
    Ok(browser_metadata_from_paired_client(client))
}

fn browser_metadata_from_paired_client(client: &PairedWebClient) -> BrowserEnrollmentMetadata {
    let label = client
        .nickname
        .clone()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| {
            if client.label.trim().is_empty() {
                "Browser".to_string()
            } else {
                client.label.clone()
            }
        });
    let browser_install_id = if client.browser_install_id.trim().is_empty() {
        client.client_id.clone()
    } else {
        client.browser_install_id.clone()
    };
    BrowserEnrollmentMetadata {
        paired_cookie_client_id: client.client_id.clone(),
        label,
        browser_install_id,
        nickname: client.nickname.clone(),
    }
}

/// Subscribe to config revisions, then validate or atomically bind the peer key.
///
/// On first bind, mints a host-assigned [`ClientId`] inside the persistence
/// transaction. Callers that must stay off the UI thread should invoke this
/// from a worker.
pub(crate) fn validate_or_bind_connect_peer(
    inner: &Arc<RemoteHostInner>,
    paired_client_id: &str,
    peer_public: [u8; CONNECT_PEER_PUBLIC_KEY_BYTES],
) -> Result<ConnectPeerLease, ConnectPeerTrustError> {
    let paired_client_id = validate_paired_cookie_client_id(paired_client_id)?;
    let peer_public = ConnectPeerPublicKey::from_bytes(peer_public)?;
    if inner.stop_flag.load(Ordering::Acquire) {
        return Err(ConnectPeerTrustError::HostStopped);
    }
    // Subscribe before any membership/pin check so a concurrent revoke/stop
    // cannot be missed between observation and lease construction.
    let revision_rx = inner.host_config_watch.subscribe();

    // Same-origin cookie route never first-binds a cross-origin-only record.
    {
        let config = inner
            .config
            .read()
            .map_err(|_| ConnectPeerTrustError::ConfigUnavailable)?;
        let client = config
            .web
            .paired_clients
            .iter()
            .find(|client| client.client_id == paired_client_id)
            .ok_or(ConnectPeerTrustError::NotPaired)?;
        if client.permitted_origin.is_some() {
            return Err(ConnectPeerTrustError::NotPaired);
        }
    }
    let assigned_client_id = attempt_validate_or_bind(inner, paired_client_id, peer_public)?;
    Ok(ConnectPeerLease {
        inner: Arc::downgrade(inner),
        paired_cookie_client_id: paired_client_id.to_string(),
        assigned_client_id,
        peer_public,
        permitted_origin: None,
        revision_rx,
        identity_rx: None,
        identity_generation: 0,
    })
}

/// Cross-origin Connect: reverse-lookup an existing pin by authenticated static
/// public key. Never first-binds. Rejects absent or ambiguous pins and requires
/// `PairedWebClient.permitted_origin` to match the request origin exactly.
pub(crate) fn validate_cross_origin_connect_peer(
    inner: &Arc<RemoteHostInner>,
    origin: &str,
    peer_public: [u8; CONNECT_PEER_PUBLIC_KEY_BYTES],
    expected_paired_client_id: Option<&str>,
    expected_public_key: Option<ConnectPeerPublicKey>,
) -> Result<ConnectPeerLease, ConnectPeerTrustError> {
    if origin.is_empty()
        || origin.len() > 2048
        || origin.chars().any(|character| character.is_control())
    {
        return Err(ConnectPeerTrustError::PeerKeyRejected);
    }
    let peer_public = ConnectPeerPublicKey::from_bytes(peer_public)?;
    if let Some(expected) = expected_public_key {
        if !constant_time_eq(&expected.as_bytes(), &peer_public.as_bytes()) {
            return Err(ConnectPeerTrustError::KeyMismatchRequiresRepair);
        }
    }
    if inner.stop_flag.load(Ordering::Acquire) {
        return Err(ConnectPeerTrustError::HostStopped);
    }
    let revision_rx = inner.host_config_watch.subscribe();
    let config = inner
        .config
        .read()
        .map_err(|_| ConnectPeerTrustError::ConfigUnavailable)?;
    if !config.web.enabled {
        return Err(ConnectPeerTrustError::WebDisabled);
    }
    let (paired_cookie_client_id, pin) = find_unique_peer_by_public(&config, peer_public)?;
    if let Some(expected_id) = expected_paired_client_id {
        let expected_id = validate_paired_cookie_client_id(expected_id)?;
        if paired_cookie_client_id != expected_id {
            return Err(ConnectPeerTrustError::NotPaired);
        }
    }
    let client = config
        .web
        .paired_clients
        .iter()
        .find(|client| client.client_id == paired_cookie_client_id)
        .ok_or(ConnectPeerTrustError::NotPaired)?;
    match client.permitted_origin.as_deref() {
        Some(stored) if stored == origin => {}
        _ => return Err(ConnectPeerTrustError::NotPaired),
    }
    Ok(ConnectPeerLease {
        inner: Arc::downgrade(inner),
        paired_cookie_client_id,
        assigned_client_id: pin.client_id,
        peer_public,
        permitted_origin: Some(origin.to_string()),
        revision_rx,
        identity_rx: None,
        identity_generation: 0,
    })
}

fn find_unique_peer_by_public(
    config: &RemoteHostConfig,
    peer_public: ConnectPeerPublicKey,
) -> Result<(String, ConnectPeerPin), ConnectPeerTrustError> {
    let mut matches =
        config.web.connect_peer_keys.iter().filter(|(_, pin)| {
            constant_time_eq(&pin.public_key.as_bytes(), &peer_public.as_bytes())
        });
    match (matches.next(), matches.next()) {
        (Some((client_id, pin)), None) => Ok((client_id.clone(), *pin)),
        (None, _) => Err(ConnectPeerTrustError::NotPaired),
        (Some(_), Some(_)) => Err(ConnectPeerTrustError::PeerKeyRejected),
    }
}

fn attempt_validate_or_bind(
    inner: &Arc<RemoteHostInner>,
    paired_client_id: &str,
    peer_public: ConnectPeerPublicKey,
) -> Result<ClientId, ConnectPeerTrustError> {
    let map_key = paired_client_id.to_string();
    let bound = mutate_host_config_if(
        inner,
        |config| can_bind_new_peer(config, paired_client_id),
        |config| {
            let assigned = ClientId::new();
            config.web.connect_peer_keys.insert(
                map_key,
                ConnectPeerPin {
                    public_key: peer_public,
                    client_id: assigned,
                },
            );
            assigned
        },
    )?;
    if let Some(assigned) = bound {
        return Ok(assigned);
    }
    classify_existing_pin(inner, paired_client_id, peer_public)
}

fn can_bind_new_peer(config: &RemoteHostConfig, paired_client_id: &str) -> bool {
    config.web.enabled
        && config.web.connect_peer_keys.len() < MAX_CONNECT_PEER_PINS
        && paired_client_present(config, paired_client_id)
        && !config.web.connect_peer_keys.contains_key(paired_client_id)
        && config
            .web
            .paired_clients
            .iter()
            .find(|client| client.client_id == paired_client_id)
            .is_some_and(|client| client.permitted_origin.is_none())
}

fn classify_existing_pin(
    inner: &Arc<RemoteHostInner>,
    paired_client_id: &str,
    peer_public: ConnectPeerPublicKey,
) -> Result<ClientId, ConnectPeerTrustError> {
    if inner.stop_flag.load(Ordering::Acquire) {
        return Err(ConnectPeerTrustError::HostStopped);
    }
    let config = inner
        .config
        .read()
        .map_err(|_| ConnectPeerTrustError::ConfigUnavailable)?;
    if !config.web.enabled {
        return Err(ConnectPeerTrustError::WebDisabled);
    }
    if !paired_client_present(&config, paired_client_id) {
        return Err(ConnectPeerTrustError::NotPaired);
    }
    match config.web.connect_peer_keys.get(paired_client_id) {
        Some(pinned)
            if constant_time_eq(&pinned.public_key.as_bytes(), &peer_public.as_bytes()) =>
        {
            Ok(pinned.client_id)
        }
        Some(_) => Err(ConnectPeerTrustError::KeyMismatchRequiresRepair),
        None if config.web.connect_peer_keys.len() >= MAX_CONNECT_PEER_PINS => {
            Err(ConnectPeerTrustError::Capacity)
        }
        None => Err(ConnectPeerTrustError::NotPaired),
    }
}

fn paired_client_present(config: &RemoteHostConfig, paired_client_id: &str) -> bool {
    config
        .web
        .paired_clients
        .iter()
        .any(|client| client.client_id == paired_client_id)
}

fn peer_is_authorized(
    config: &RemoteHostConfig,
    paired_client_id: &str,
    peer_public: ConnectPeerPublicKey,
    assigned_client_id: ClientId,
    expected_permitted_origin: Option<&str>,
) -> bool {
    if !config.web.enabled {
        return false;
    }
    let Some(client) = config
        .web
        .paired_clients
        .iter()
        .find(|client| client.client_id == paired_client_id)
    else {
        return false;
    };
    match (
        client.permitted_origin.as_deref(),
        expected_permitted_origin,
    ) {
        (None, None) => {}
        (Some(stored), Some(expected)) if stored == expected => {}
        _ => return false,
    }
    match config.web.connect_peer_keys.get(paired_client_id) {
        Some(pinned) => {
            constant_time_eq(&pinned.public_key.as_bytes(), &peer_public.as_bytes())
                && pinned.client_id == assigned_client_id
        }
        None => false,
    }
}

pub(crate) fn validate_paired_cookie_client_id(
    paired_client_id: &str,
) -> Result<&str, ConnectPeerTrustError> {
    if paired_client_id.is_empty()
        || paired_client_id.len() > MAX_PAIRED_COOKIE_CLIENT_ID_BYTES
        || paired_client_id
            .chars()
            .any(|character| character.is_control())
    {
        return Err(ConnectPeerTrustError::InvalidClientId);
    }
    Ok(paired_client_id)
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

/// Strict deserializer for [`WebConfig::connect_peer_keys`].
pub fn deserialize_connect_peer_keys<'de, D>(
    deserializer: D,
) -> Result<BTreeMap<String, ConnectPeerPin>, D::Error>
where
    D: Deserializer<'de>,
{
    struct MapVisitor;

    impl<'de> Visitor<'de> for MapVisitor {
        type Value = BTreeMap<String, ConnectPeerPin>;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("a bounded connectPeerKeys object")
        }

        fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
        where
            A: MapAccess<'de>,
        {
            let mut out = BTreeMap::new();
            while let Some(key) = map.next_key::<String>()? {
                if key.is_empty()
                    || key.len() > MAX_PAIRED_COOKIE_CLIENT_ID_BYTES
                    || key.chars().any(|character| character.is_control())
                {
                    return Err(de::Error::custom(
                        "connectPeerKeys entry key must be nonempty, at most 256 bytes, and free of control characters",
                    ));
                }
                if out.len() >= MAX_CONNECT_PEER_PINS {
                    return Err(de::Error::custom(
                        "connectPeerKeys exceeds the maximum of 256 pins",
                    ));
                }
                let pin = map
                    .next_value::<ConnectPeerPin>()
                    .map_err(|_| de::Error::custom("connectPeerKeys entry value is malformed"))?;
                if out.insert(key, pin).is_some() {
                    return Err(de::Error::custom(
                        "connectPeerKeys contains a duplicate entry key",
                    ));
                }
            }
            Ok(out)
        }
    }

    deserializer.deserialize_map(MapVisitor)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::test_support::TestProfileGuard;
    use crate::remote::{
        load_remote_machine_state, mutate_host_config_if, remote_state_path, PairedWebClient,
        RemoteHostConfig, RemoteHostService,
    };
    use std::fs;
    use std::sync::{Arc, Barrier};
    use std::thread;
    use std::time::Duration;

    fn paired_service(profile: &str, client_id: &str) -> (TestProfileGuard, RemoteHostService) {
        let profile = TestProfileGuard::new(profile);
        let mut config = RemoteHostConfig::default();
        config.web.enabled = true;
        config.web.paired_clients.push(PairedWebClient {
            client_id: client_id.to_string(),
            browser_install_id: "install-1".to_string(),
            label: "Phone".to_string(),
            ..PairedWebClient::default()
        });
        // Exercise the production auth owner without binding a listener. The
        // host listener has a separate OS-thread/runtime lifetime test suite.
        let service = RemoteHostService::new_web_only(config).expect("stored web config");
        (profile, service)
    }

    #[test]
    fn first_bind_persists_peer_public_key_and_assigned_client_id() {
        let (_profile, service) = paired_service("connect-peer-first-bind", "web-client-a");
        let peer = [9_u8; 32];

        let lease = validate_or_bind_connect_peer(&service.inner, "web-client-a", peer)
            .expect("first bind");
        assert!(lease.is_authorized());
        assert_eq!(lease.peer_public().as_bytes(), peer);
        let assigned = lease.client_id();

        let saved = load_remote_machine_state().expect("load persisted host config");
        let pin = saved
            .host
            .web
            .connect_peer_keys
            .get("web-client-a")
            .copied()
            .expect("persisted pin");
        assert_eq!(pin.public_key.as_bytes(), peer);
        assert_eq!(pin.client_id, assigned);
        assert!(saved.host.web.enabled);
        assert_eq!(saved.host.web.paired_clients.len(), 1);
    }

    #[test]
    fn stable_reconnect_returns_identical_assigned_id_without_disk_rewrite() {
        let (_profile, service) = paired_service("connect-peer-stable-reconnect", "web-client-a");
        let peer = [11_u8; 32];
        let first = validate_or_bind_connect_peer(&service.inner, "web-client-a", peer)
            .expect("first bind");
        let assigned = first.client_id();

        let path = remote_state_path().expect("remote state path");
        let before = fs::read(&path).expect("read remote state");
        let revision_before = service.config_revision();

        let lease = validate_or_bind_connect_peer(&service.inner, "web-client-a", peer)
            .expect("stable reconnect");
        assert!(lease.is_authorized());
        assert_eq!(lease.client_id(), assigned);
        assert_eq!(fs::read(&path).expect("reread remote state"), before);
        assert_eq!(service.config_revision(), revision_before);
    }

    #[test]
    fn restart_through_config_load_preserves_assigned_client_id() {
        let (_profile, service) = paired_service("connect-peer-restart-load", "web-client-a");
        let peer = [12_u8; 32];
        let assigned = validate_or_bind_connect_peer(&service.inner, "web-client-a", peer)
            .expect("first bind")
            .client_id();
        let saved = load_remote_machine_state().expect("load");
        drop(service);

        let restarted = RemoteHostService::new_web_only(saved.host).expect("stored web config");
        let lease = validate_or_bind_connect_peer(&restarted.inner, "web-client-a", peer)
            .expect("reconnect after restart");
        assert_eq!(lease.client_id(), assigned);
        assert!(lease.is_authorized());
    }

    #[test]
    fn mismatched_key_requires_explicit_repair() {
        let (_profile, service) = paired_service("connect-peer-mismatch", "web-client-a");
        validate_or_bind_connect_peer(&service.inner, "web-client-a", [21_u8; 32])
            .expect("first bind");

        let error = validate_or_bind_connect_peer(&service.inner, "web-client-a", [22_u8; 32])
            .expect_err("changed key must fail closed");
        assert_eq!(error, ConnectPeerTrustError::KeyMismatchRequiresRepair);
    }

    #[test]
    fn concurrent_first_bind_allows_only_one_key_and_one_assigned_id() {
        let (_profile, service) = paired_service("connect-peer-concurrent", "web-client-a");
        let inner = service.inner.clone();
        let barrier = Arc::new(Barrier::new(2));
        let left_barrier = barrier.clone();
        let right_barrier = barrier;
        let left_inner = inner.clone();
        let right_inner = inner;

        let left = thread::spawn(move || {
            left_barrier.wait();
            validate_or_bind_connect_peer(&left_inner, "web-client-a", [31_u8; 32])
        });
        let right = thread::spawn(move || {
            right_barrier.wait();
            validate_or_bind_connect_peer(&right_inner, "web-client-a", [32_u8; 32])
        });

        let left_result = left.join().expect("left join");
        let right_result = right.join().expect("right join");
        let left_id = left_result.as_ref().ok().map(ConnectPeerLease::client_id);
        let right_id = right_result.as_ref().ok().map(ConnectPeerLease::client_id);
        assert_eq!([left_id, right_id].iter().flatten().count(), 1);
        let failure = left_result
            .err()
            .or_else(|| right_result.err())
            .expect("losing candidate");
        assert_eq!(failure, ConnectPeerTrustError::KeyMismatchRequiresRepair);

        let pinned = service
            .config()
            .web
            .connect_peer_keys
            .get("web-client-a")
            .copied()
            .expect("exactly one pin");
        assert!(
            pinned.public_key.as_bytes() == [31_u8; 32]
                || pinned.public_key.as_bytes() == [32_u8; 32]
        );
        assert_eq!(Some(pinned.client_id), left_id.or(right_id));
    }

    #[tokio::test]
    async fn revoke_wakes_existing_lease() {
        let (_profile, service) = paired_service("connect-peer-revoke-wake", "web-client-a");
        let mut lease = validate_or_bind_connect_peer(&service.inner, "web-client-a", [41_u8; 32])
            .expect("bind");
        assert!(lease.is_authorized());

        let revoked = tokio::spawn(async move {
            lease.revoked().await;
        });
        tokio::task::yield_now().await;
        assert!(service.revoke_paired_web_client("web-client-a"));
        tokio::time::timeout(Duration::from_secs(2), revoked)
            .await
            .expect("revoke should wake lease")
            .expect("revoke task");
        assert!(service.config().web.connect_peer_keys.is_empty());
    }

    #[tokio::test]
    async fn disable_wakes_existing_lease() {
        let (_profile, service) = paired_service("connect-peer-disable-wake", "web-client-a");
        let mut lease = validate_or_bind_connect_peer(&service.inner, "web-client-a", [42_u8; 32])
            .expect("bind");
        let revoked = tokio::spawn(async move {
            lease.revoked().await;
        });
        tokio::task::yield_now().await;
        service
            .update_web_listener_settings(false, "127.0.0.1".to_string(), 43872)
            .expect("disable web");
        tokio::time::timeout(Duration::from_secs(2), revoked)
            .await
            .expect("disable should wake lease")
            .expect("disable task");
    }

    #[tokio::test]
    async fn service_owner_drop_revokes_while_strong_inner_arc_retained() {
        let (_profile, service) = paired_service("connect-peer-owner-stop-wake", "web-client-a");
        let retained = service.inner.clone();
        let mut lease =
            validate_or_bind_connect_peer(&retained, "web-client-a", [43_u8; 32]).expect("bind");
        assert!(lease.is_authorized());

        let revoked = tokio::spawn(async move {
            lease.revoked().await;
        });
        tokio::task::yield_now().await;
        drop(service);
        tokio::time::timeout(Duration::from_secs(2), revoked)
            .await
            .expect("owner drop should wake lease without waiting for weak destruction")
            .expect("revoked task");
        assert!(retained.stop_flag.load(Ordering::Acquire));
        assert!(Arc::strong_count(&retained) >= 1);
    }

    #[test]
    fn service_owner_drop_clears_authorization_while_inner_arc_retained() {
        let (_profile, service) = paired_service("connect-peer-owner-stop-auth", "web-client-a");
        let retained = service.inner.clone();
        let lease =
            validate_or_bind_connect_peer(&retained, "web-client-a", [44_u8; 32]).expect("bind");
        assert!(lease.is_authorized());
        drop(service);
        assert!(retained.stop_flag.load(Ordering::Acquire));
        assert!(!lease.is_authorized());
        assert!(Arc::strong_count(&retained) >= 1);
    }

    #[tokio::test]
    async fn unrelated_client_revoke_leaves_lease_authorized() {
        let profile = TestProfileGuard::new("connect-peer-unrelated-revoke");
        let mut config = RemoteHostConfig::default();
        config.web.enabled = true;
        for client_id in ["web-client-a", "web-client-b"] {
            config.web.paired_clients.push(PairedWebClient {
                client_id: client_id.to_string(),
                browser_install_id: format!("install-{client_id}"),
                label: client_id.to_string(),
                ..PairedWebClient::default()
            });
        }
        let service = RemoteHostService::new_web_only(config).expect("stored web config");
        let mut lease = validate_or_bind_connect_peer(&service.inner, "web-client-a", [51_u8; 32])
            .expect("bind a");
        validate_or_bind_connect_peer(&service.inner, "web-client-b", [52_u8; 32]).expect("bind b");

        assert!(service.revoke_paired_web_client("web-client-b"));
        tokio::task::yield_now().await;
        assert!(lease.is_authorized());
        assert!(
            !tokio::time::timeout(Duration::from_millis(50), lease.revoked())
                .await
                .is_ok()
        );
        drop(service);
        drop(profile);
    }

    #[test]
    fn weak_owner_drop_revokes_authorization() {
        let (_profile, service) = paired_service("connect-peer-weak-drop", "web-client-a");
        let host = service.inner.clone();
        let lease =
            validate_or_bind_connect_peer(&host, "web-client-a", [61_u8; 32]).expect("bind");
        assert!(lease.is_authorized());
        drop(service);
        for _ in 0..200 {
            if Arc::strong_count(&host) == 1 {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        drop(host);
        assert!(!lease.is_authorized());
    }

    #[test]
    fn regenerating_invite_token_preserves_existing_peer_pin() {
        let (_profile, service) = paired_service("connect-peer-invite-regen", "web-client-a");
        let peer = [71_u8; 32];
        let lease =
            validate_or_bind_connect_peer(&service.inner, "web-client-a", peer).expect("bind");
        let assigned = lease.client_id();
        service
            .regenerate_web_pairing_token()
            .expect("regenerate invite");
        assert!(lease.is_authorized());
        assert_eq!(
            service
                .config()
                .web
                .connect_peer_keys
                .get("web-client-a")
                .map(|pin| (pin.public_key.as_bytes(), pin.client_id)),
            Some((peer, assigned))
        );
    }

    #[test]
    fn default_migration_preserves_paired_clients_without_pins_or_activation() {
        let cookie = "ab".repeat(32);
        let json = format!(
            r#"{{
            "enabled": false,
            "bindAddress": "0.0.0.0",
            "port": 43872,
            "pairingToken": "PAIRTOKEN",
            "cookieSecretHex": "{cookie}",
            "pairedClients": [{{
                "clientId": "web-client-legacy",
                "browserInstallId": "install-legacy",
                "label": "Legacy"
            }}]
        }}"#
        );
        let config: crate::remote::web::WebConfig =
            serde_json::from_str(&json).expect("legacy web config without connectPeerKeys");
        assert!(!config.enabled);
        assert_eq!(config.paired_clients.len(), 1);
        assert_eq!(config.paired_clients[0].client_id, "web-client-legacy");
        assert!(config.connect_peer_keys.is_empty());
    }

    #[test]
    fn malformed_and_oversized_peer_keys_fail_closed() {
        assert!(serde_json::from_str::<ConnectPeerPublicKey>("\"00\"").is_err());
        assert!(
            serde_json::from_str::<ConnectPeerPublicKey>(&format!("\"{}\"", "ab".repeat(33)))
                .is_err()
        );
        assert!(
            serde_json::from_str::<ConnectPeerPublicKey>(&format!("\"{}\"", "00".repeat(32)))
                .is_err()
        );
        assert!(serde_json::from_str::<ConnectPeerPublicKey>("[1,2,3]").is_err());
    }

    #[test]
    fn malformed_and_non_v7_assigned_client_ids_fail_closed() {
        let key = "ab".repeat(32);
        let v4 = "\"00000000-0000-4000-8000-000000000000\"";
        let pin = format!(r#"{{"publicKey":"{key}","clientId":{v4}}}"#);
        assert!(serde_json::from_str::<ConnectPeerPin>(&pin).is_err());
        let truncated = format!(r#"{{"publicKey":"{key}","clientId":"not-a-uuid"}}"#);
        assert!(serde_json::from_str::<ConnectPeerPin>(&truncated).is_err());
    }

    #[test]
    fn bounded_connect_peer_keys_map_rejects_oversized_and_duplicate_entries() {
        let key = "ab".repeat(32);
        let v7 = ClientId::new().to_string();
        let oversized_key = "k".repeat(MAX_PAIRED_COOKIE_CLIENT_ID_BYTES + 1);
        let oversized =
            format!(r#"{{"{oversized_key}":{{"publicKey":"{key}","clientId":"{v7}"}}}}"#);
        assert!(deserialize_connect_peer_keys_from_str(&oversized).is_err());

        let mut many = String::from('{');
        for index in 0..=MAX_CONNECT_PEER_PINS {
            if index > 0 {
                many.push(',');
            }
            let id = ClientId::new();
            many.push_str(&format!(
                r#""cookie-{index}":{{"publicKey":"{key}","clientId":"{id}"}}"#
            ));
        }
        many.push('}');
        assert!(deserialize_connect_peer_keys_from_str(&many).is_err());
    }

    #[test]
    fn paired_cookie_client_id_bounds_reject_control_and_empty() {
        assert_eq!(
            validate_paired_cookie_client_id(""),
            Err(ConnectPeerTrustError::InvalidClientId)
        );
        assert_eq!(
            validate_paired_cookie_client_id("bad\nid"),
            Err(ConnectPeerTrustError::InvalidClientId)
        );
        assert_eq!(
            validate_paired_cookie_client_id(&"x".repeat(MAX_PAIRED_COOKIE_CLIENT_ID_BYTES + 1)),
            Err(ConnectPeerTrustError::InvalidClientId)
        );
    }

    #[test]
    fn zero_peer_key_rejected_before_bind() {
        let (_profile, service) = paired_service("connect-peer-zero-key", "web-client-a");
        let error = validate_or_bind_connect_peer(&service.inner, "web-client-a", [0_u8; 32])
            .expect_err("all-zero key");
        assert_eq!(error, ConnectPeerTrustError::PeerKeyRejected);
        assert!(service.config().web.connect_peer_keys.is_empty());
    }

    #[test]
    fn identity_authority_invalidation_fails_lease_closed() {
        let (_profile, service) = paired_service("connect-peer-identity-wake", "web-client-a");
        let (tx, rx) = watch::channel(1_u64);
        let lease = validate_or_bind_connect_peer(&service.inner, "web-client-a", [71_u8; 32])
            .expect("bind")
            .with_identity_invalidation(rx, 1);
        assert!(lease.is_authorized());
        tx.send_replace(2);
        assert!(!lease.is_authorized());
        let (_, rx) = watch::channel(u64::MAX);
        let exhausted = lease.with_identity_invalidation(rx, u64::MAX);
        assert!(
            !exhausted.is_authorized(),
            "exhaustion cannot mint authority"
        );
    }

    #[test]
    fn identity_generation_captured_at_mint_not_attach_baseline() {
        let (_profile, service) = paired_service("connect-peer-identity-mint", "web-client-a");
        let (tx, rx) = watch::channel(3_u64);
        // Revocation happened after mint (gen 3) before attach observes 4.
        tx.send_replace(4);
        let lease = validate_or_bind_connect_peer(&service.inner, "web-client-a", [72_u8; 32])
            .expect("bind")
            .with_identity_invalidation(rx, 3);
        assert!(
            !lease.is_authorized(),
            "stale mint generation must not accept a later authority bump"
        );
    }

    #[tokio::test]
    async fn identity_invalidation_wakes_idle_revoked_wait() {
        let (_profile, service) = paired_service("connect-peer-identity-idle", "web-client-a");
        let (tx, rx) = watch::channel(1_u64);
        let mut lease = validate_or_bind_connect_peer(&service.inner, "web-client-a", [73_u8; 32])
            .expect("bind")
            .with_identity_invalidation(rx, 1);
        let waiter = tokio::spawn(async move {
            lease.revoked().await;
        });
        tx.send_replace(2);
        tokio::time::timeout(std::time::Duration::from_secs(2), waiter)
            .await
            .expect("idle revoked wait must wake")
            .expect("join");
    }

    #[test]
    fn same_origin_bind_rejects_cross_origin_only_client() {
        let profile = TestProfileGuard::new("connect-peer-cross-origin-cookie");
        let mut config = RemoteHostConfig::default();
        config.web.enabled = true;
        config.web.paired_clients.push(PairedWebClient {
            client_id: "web-cross".into(),
            browser_install_id: "install-cross".into(),
            label: "Phone".into(),
            permitted_origin: Some("https://a.example".into()),
            ..PairedWebClient::default()
        });
        let service = RemoteHostService::new_web_only(config).expect("web");
        let error = validate_or_bind_connect_peer(&service.inner, "web-cross", [81_u8; 32])
            .expect_err("cross-origin-only must not bind on same-origin path");
        assert_eq!(error, ConnectPeerTrustError::NotPaired);
        drop(service);
        drop(profile);
    }

    #[test]
    fn cross_origin_validate_requires_exact_origin_and_unique_key() {
        let profile = TestProfileGuard::new("connect-peer-cross-origin-validate");
        let mut config = RemoteHostConfig::default();
        config.web.enabled = true;
        let peer = ConnectPeerPublicKey::from_bytes([82_u8; 32]).unwrap();
        let assigned = ClientId::new();
        config.web.paired_clients.push(PairedWebClient {
            client_id: "web-cross".into(),
            browser_install_id: "install-cross".into(),
            label: "Phone".into(),
            permitted_origin: Some("https://a.example".into()),
            ..PairedWebClient::default()
        });
        config.web.connect_peer_keys.insert(
            "web-cross".into(),
            ConnectPeerPin {
                public_key: peer,
                client_id: assigned,
            },
        );
        let service = RemoteHostService::new_web_only(config).expect("web");
        let lease = validate_cross_origin_connect_peer(
            &service.inner,
            "https://a.example",
            peer.as_bytes(),
            Some("web-cross"),
            Some(peer),
        )
        .expect("matching origin");
        assert!(lease.is_authorized());
        assert_eq!(lease.permitted_origin(), Some("https://a.example"));
        assert!(validate_cross_origin_connect_peer(
            &service.inner,
            "https://other.example",
            peer.as_bytes(),
            None,
            None,
        )
        .is_err());
        // Revoke origin membership.
        let _ = mutate_host_config_if(
            &service.inner,
            |_| true,
            |config| {
                if let Some(client) = config
                    .web
                    .paired_clients
                    .iter_mut()
                    .find(|client| client.client_id == "web-cross")
                {
                    client.permitted_origin = None;
                }
            },
        );
        assert!(!lease.is_authorized());
        drop(service);
        drop(profile);
    }

    #[test]
    fn cross_origin_rejects_ambiguous_duplicate_public_keys() {
        let profile = TestProfileGuard::new("connect-peer-cross-origin-ambiguous");
        let mut config = RemoteHostConfig::default();
        config.web.enabled = true;
        let peer = ConnectPeerPublicKey::from_bytes([83_u8; 32]).unwrap();
        for (id, origin) in [
            ("web-a", "https://a.example"),
            ("web-b", "https://a.example"),
        ] {
            config.web.paired_clients.push(PairedWebClient {
                client_id: id.into(),
                browser_install_id: id.into(),
                label: id.into(),
                permitted_origin: Some(origin.into()),
                ..PairedWebClient::default()
            });
            config.web.connect_peer_keys.insert(
                id.into(),
                ConnectPeerPin {
                    public_key: peer,
                    client_id: ClientId::new(),
                },
            );
        }
        let service = RemoteHostService::new_web_only(config).expect("web");
        let error = validate_cross_origin_connect_peer(
            &service.inner,
            "https://a.example",
            peer.as_bytes(),
            None,
            None,
        )
        .expect_err("ambiguous key");
        assert_eq!(error, ConnectPeerTrustError::PeerKeyRejected);
        drop(service);
        drop(profile);
    }

    fn deserialize_connect_peer_keys_from_str(
        json: &str,
    ) -> Result<BTreeMap<String, ConnectPeerPin>, serde_json::Error> {
        let mut deserializer = serde_json::Deserializer::from_str(json);
        deserialize_connect_peer_keys(&mut deserializer)
    }
}
