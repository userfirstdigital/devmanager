use crate::remote::presentation::StableSessionKey;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Weak};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use ureq::http::{Request, Uri};
use web_push_native::jwt_simple::algorithms::ES256KeyPair;
use web_push_native::p256::elliptic_curve::sec1::ToEncodedPoint;
use web_push_native::{Auth, WebPushBuilder};

pub const MAX_PUSH_SUBSCRIPTIONS: usize = 32;
#[cfg(test)]
const MAX_PUSH_SUBSCRIPTIONS_PER_CLIENT: usize = 2;
const PUSH_INTENT_SCHEMA_VERSION: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushEnableError {
    ClientLimitReached,
}
const MAX_ENDPOINT_BYTES: usize = 2_048;
const MAX_KEY_TEXT_BYTES: usize = 256;
const PUSH_TTL: Duration = Duration::from_secs(5 * 60);
const VAPID_CONTACT: &str = "mailto:devmanager@userfirst.com";
const DELIVERY_QUEUE_CAPACITY: usize = 128;
const DELIVERY_WORKERS: usize = 4;
const DELIVERY_TIMEOUT: Duration = Duration::from_secs(8);
const DELIVERY_POLL_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, rename_all = "camelCase")]
pub struct WebPushConfig {
    pub vapid_private_key_base64: String,
    pub vapid_public_key_base64: String,
    #[serde(default = "legacy_intent_schema_version")]
    pub intent_schema_version: u8,
    pub enabled_client_ids: Vec<String>,
    pub subscriptions: Vec<WebPushSubscription>,
}

fn legacy_intent_schema_version() -> u8 {
    0
}

impl fmt::Debug for WebPushConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WebPushConfig")
            .field("vapid_private_key_base64", &"[REDACTED]")
            .field("vapid_public_key_base64", &self.vapid_public_key_base64)
            .field("enabled_client_count", &self.enabled_client_ids.len())
            .field("subscription_count", &self.subscriptions.len())
            .finish()
    }
}

impl Default for WebPushConfig {
    fn default() -> Self {
        let (private_key, public_key) = generate_vapid_pair();
        Self {
            vapid_private_key_base64: private_key,
            vapid_public_key_base64: public_key,
            intent_schema_version: PUSH_INTENT_SCHEMA_VERSION,
            enabled_client_ids: Vec::new(),
            subscriptions: Vec::new(),
        }
    }
}

impl WebPushConfig {
    pub fn ensure_keys(&mut self) {
        self.normalize_subscription_intent();
        if self.keys_are_valid() {
            return;
        }
        let (private_key, public_key) = generate_vapid_pair();
        self.vapid_private_key_base64 = private_key;
        self.vapid_public_key_base64 = public_key;
        // Push subscriptions are cryptographically bound to the application
        // server key. Keeping them after a key rotation can only create a
        // permanent failing delivery loop.
        self.subscriptions.clear();
    }

    fn normalize_subscription_intent(&mut self) {
        if self.intent_schema_version == 0 {
            for client_id in self
                .subscriptions
                .iter()
                .map(|subscription| subscription.client_id.clone())
            {
                if !self.enabled_client_ids.contains(&client_id) {
                    self.enabled_client_ids.push(client_id);
                }
            }
            self.intent_schema_version = PUSH_INTENT_SCHEMA_VERSION;
        }
        self.enabled_client_ids = canonical_enabled_client_ids(&self.enabled_client_ids);
        self.subscriptions = canonical_subscriptions(&self.enabled_client_ids, &self.subscriptions);
    }

    pub fn notifications_enabled(&self, client_id: &str) -> bool {
        self.enabled_client_ids
            .iter()
            .any(|enabled| enabled == client_id)
    }

    pub fn enable_and_replace_subscription(
        &mut self,
        client_id: &str,
        validated: ValidatedPushSubscription,
        created_at_epoch_ms: u64,
    ) -> Result<(), PushEnableError> {
        if !self.notifications_enabled(client_id) {
            if self.enabled_client_ids.len() >= MAX_PUSH_SUBSCRIPTIONS {
                return Err(PushEnableError::ClientLimitReached);
            }
            self.enabled_client_ids.push(client_id.to_string());
        }
        self.replace_client_subscription(client_id, validated, created_at_epoch_ms);
        Ok(())
    }

    pub fn reconcile_and_replace_subscription(
        &mut self,
        client_id: &str,
        validated: ValidatedPushSubscription,
        created_at_epoch_ms: u64,
    ) -> bool {
        if !self.notifications_enabled(client_id) {
            return false;
        }
        self.replace_client_subscription(client_id, validated, created_at_epoch_ms);
        true
    }

    fn replace_client_subscription(
        &mut self,
        client_id: &str,
        validated: ValidatedPushSubscription,
        created_at_epoch_ms: u64,
    ) {
        self.subscriptions
            .retain(|subscription| subscription.client_id != client_id);
        while self.subscriptions.len() >= MAX_PUSH_SUBSCRIPTIONS {
            let oldest = self
                .subscriptions
                .iter()
                .enumerate()
                .min_by_key(|(_, subscription)| subscription.created_at_epoch_ms)
                .map(|(index, _)| index)
                .unwrap_or(0);
            self.subscriptions.remove(oldest);
        }
        self.subscriptions.push(WebPushSubscription::from_validated(
            client_id,
            validated,
            created_at_epoch_ms,
        ));
    }

    pub fn keys_are_valid(&self) -> bool {
        let Ok(private_bytes) = URL_SAFE_NO_PAD.decode(&self.vapid_private_key_base64) else {
            return false;
        };
        let Ok(public_bytes) = URL_SAFE_NO_PAD.decode(&self.vapid_public_key_base64) else {
            return false;
        };
        if private_bytes.len() != 32 || public_bytes.len() != 65 {
            return false;
        }
        let Ok(secret) = web_push_native::p256::SecretKey::from_slice(&private_bytes) else {
            return false;
        };
        let derived = secret.public_key().to_encoded_point(false);
        derived.as_bytes() == public_bytes.as_slice()
            && ES256KeyPair::from_bytes(&private_bytes).is_ok()
    }

    #[cfg(test)]
    pub(crate) fn insert_legacy_subscription_for_test(
        &mut self,
        client_id: &str,
        validated: ValidatedPushSubscription,
        created_at_epoch_ms: u64,
    ) {
        self.subscriptions
            .retain(|subscription| subscription.endpoint != validated.endpoint);

        let mut client_indices = self
            .subscriptions
            .iter()
            .enumerate()
            .filter_map(|(index, subscription)| {
                (subscription.client_id == client_id)
                    .then_some((index, subscription.created_at_epoch_ms))
            })
            .collect::<Vec<_>>();
        client_indices.sort_by_key(|(_, created_at)| *created_at);
        while client_indices.len() >= MAX_PUSH_SUBSCRIPTIONS_PER_CLIENT {
            let (index, _) = client_indices.remove(0);
            self.subscriptions.remove(index);
            for (remaining, _) in &mut client_indices {
                if *remaining > index {
                    *remaining -= 1;
                }
            }
        }

        while self.subscriptions.len() >= MAX_PUSH_SUBSCRIPTIONS {
            let oldest = self
                .subscriptions
                .iter()
                .enumerate()
                .min_by_key(|(_, subscription)| subscription.created_at_epoch_ms)
                .map(|(index, _)| index)
                .unwrap_or(0);
            self.subscriptions.remove(oldest);
        }
        self.subscriptions.push(WebPushSubscription::from_validated(
            client_id,
            validated,
            created_at_epoch_ms,
        ));
    }

    pub fn disable_client(&mut self, client_id: &str) -> bool {
        let enabled_before = self.enabled_client_ids.len();
        self.enabled_client_ids
            .retain(|enabled| enabled != client_id);
        let subscriptions_before = self.subscriptions.len();
        self.subscriptions
            .retain(|subscription| subscription.client_id != client_id);
        self.enabled_client_ids.len() != enabled_before
            || self.subscriptions.len() != subscriptions_before
    }

    fn expire_subscription_if_matches(&mut self, expected: &WebPushSubscription) -> bool {
        let before = self.subscriptions.len();
        self.subscriptions
            .retain(|subscription| subscription != expected);
        self.subscriptions.len() != before
    }

    pub fn remove_client(&mut self, client_id: &str) -> bool {
        self.disable_client(client_id)
    }
}

fn generate_vapid_pair() -> (String, String) {
    let key_pair = ES256KeyPair::generate();
    let private = key_pair.to_bytes();
    let secret = web_push_native::p256::SecretKey::from_slice(&private)
        .expect("ES256 generated private key must be valid P-256 material");
    let public = secret.public_key().to_encoded_point(false);
    (
        URL_SAFE_NO_PAD.encode(private),
        URL_SAFE_NO_PAD.encode(public.as_bytes()),
    )
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WebPushSubscription {
    pub client_id: String,
    pub endpoint: String,
    pub p256dh: String,
    pub auth: String,
    pub created_at_epoch_ms: u64,
}

impl WebPushSubscription {
    fn from_validated(
        client_id: impl Into<String>,
        validated: ValidatedPushSubscription,
        created_at_epoch_ms: u64,
    ) -> Self {
        Self {
            client_id: client_id.into(),
            endpoint: validated.endpoint,
            p256dh: validated.p256dh,
            auth: validated.auth,
            created_at_epoch_ms,
        }
    }

    fn validated(&self) -> Result<ValidatedPushSubscription, String> {
        validate_registration(PushRegistrationRequest {
            mode: PushRegistrationMode::Reconcile,
            endpoint: self.endpoint.clone(),
            keys: PushRegistrationKeys {
                p256dh: self.p256dh.clone(),
                auth: self.auth.clone(),
            },
        })
    }
}

#[derive(Debug, Clone)]
pub(crate) struct PushDelivery {
    pub(crate) config: WebPushConfig,
    pub(crate) subscription: WebPushSubscription,
    pub(crate) payload: PushPayload,
}

type PushTransport = Arc<dyn Fn(Request<Vec<u8>>) -> Result<u16, String> + Send + Sync>;

#[derive(Clone)]
pub(crate) struct PushSender {
    shards: Arc<[SyncSender<PushDelivery>]>,
}

impl PushSender {
    fn new(shards: Vec<SyncSender<PushDelivery>>) -> Self {
        debug_assert!(!shards.is_empty());
        Self {
            shards: Arc::from(shards),
        }
    }

    #[cfg(test)]
    pub(crate) fn single(sender: SyncSender<PushDelivery>) -> Self {
        Self::new(vec![sender])
    }

    fn shard_index(&self, delivery: &PushDelivery) -> usize {
        let mut hasher = DefaultHasher::new();
        delivery.subscription.client_id.hash(&mut hasher);
        delivery.subscription.endpoint.hash(&mut hasher);
        hasher.finish() as usize % self.shards.len()
    }

    pub(crate) fn try_send(
        &self,
        delivery: PushDelivery,
    ) -> Result<(), TrySendError<PushDelivery>> {
        let shard = self.shard_index(&delivery);
        self.shards[shard].try_send(delivery)
    }

    #[cfg(test)]
    pub(crate) fn send(
        &self,
        delivery: PushDelivery,
    ) -> Result<(), std::sync::mpsc::SendError<PushDelivery>> {
        let shard = self.shard_index(&delivery);
        self.shards[shard].send(delivery)
    }
}

/// Bounded, off-PTY Web Push delivery pool. Queue admission is always a
/// non-blocking `try_send`; slow or unavailable push services can never hold a
/// terminal, semantic-journal, or browser-control lock.
pub(crate) struct PushDispatcher {
    sender: Option<PushSender>,
    stop: Arc<AtomicBool>,
    workers: Vec<JoinHandle<()>>,
}

impl PushDispatcher {
    pub(crate) fn start(inner: Weak<crate::remote::RemoteHostInner>) -> Result<Self, String> {
        let config = ureq::Agent::config_builder()
            .https_only(true)
            .max_redirects(0)
            .max_redirects_will_error(true)
            .http_status_as_error(false)
            .timeout_global(Some(DELIVERY_TIMEOUT))
            .build();
        let agent = ureq::Agent::new_with_config(config);
        let transport: PushTransport = Arc::new(move |request| {
            agent
                .run(request)
                .map(|response| response.status().as_u16())
                .map_err(|error| format!("Push delivery failed: {error}"))
        });
        Self::start_with_transport(inner, DELIVERY_QUEUE_CAPACITY, DELIVERY_WORKERS, transport)
    }

    fn start_with_transport(
        inner: Weak<crate::remote::RemoteHostInner>,
        queue_capacity: usize,
        worker_count: usize,
        transport: PushTransport,
    ) -> Result<Self, String> {
        let worker_count = worker_count.max(1);
        let queue_capacity_per_worker = queue_capacity.max(1).div_ceil(worker_count);
        let stop = Arc::new(AtomicBool::new(false));
        let mut workers = Vec::with_capacity(worker_count);
        let mut shard_senders = Vec::with_capacity(worker_count);
        for worker_index in 0..worker_count {
            let (sender, receiver) = mpsc::sync_channel(queue_capacity_per_worker);
            let worker_stop = stop.clone();
            let worker_inner = inner.clone();
            let worker_transport = transport.clone();
            match thread::Builder::new()
                .name(format!("devmanager-push-{worker_index}"))
                .spawn(move || {
                    push_worker_loop(receiver, worker_stop, worker_inner, worker_transport)
                }) {
                Ok(worker) => {
                    workers.push(worker);
                    shard_senders.push(sender);
                }
                Err(error) => {
                    stop.store(true, Ordering::Release);
                    drop(sender);
                    drop(shard_senders);
                    for worker in workers {
                        let _ = worker.join();
                    }
                    return Err(format!("failed to start Web Push worker: {error}"));
                }
            }
        }
        Ok(Self {
            sender: Some(PushSender::new(shard_senders)),
            stop,
            workers,
        })
    }

    pub(crate) fn sender(&self) -> PushSender {
        self.sender
            .as_ref()
            .expect("active push dispatcher must own a sender")
            .clone()
    }
}

impl Drop for PushDispatcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.sender.take();
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

fn push_worker_loop(
    receiver: Receiver<PushDelivery>,
    stop: Arc<AtomicBool>,
    inner: Weak<crate::remote::RemoteHostInner>,
    transport: PushTransport,
) {
    while !stop.load(Ordering::Acquire) {
        let delivery = match receiver.recv_timeout(DELIVERY_POLL_INTERVAL) {
            Ok(delivery) => delivery,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        if stop.load(Ordering::Acquire) {
            break;
        }
        let Some(inner) = inner.upgrade() else {
            break;
        };
        let Some((current_config, current_subscription)) =
            inner.config.read().ok().and_then(|config| {
                let current = &config.web.push;
                if current.vapid_private_key_base64 != delivery.config.vapid_private_key_base64
                    || current.vapid_public_key_base64 != delivery.config.vapid_public_key_base64
                    || !current.notifications_enabled(&delivery.subscription.client_id)
                {
                    return None;
                }
                current
                    .subscriptions
                    .iter()
                    .find(|subscription| **subscription == delivery.subscription)
                    .cloned()
                    .map(|subscription| (current.clone(), subscription))
            })
        else {
            // Pairing revocation, host reset, key rotation, and a replacement
            // subscription all invalidate already-queued delivery work.
            continue;
        };
        let Ok(validated) = current_subscription.validated() else {
            continue;
        };
        let Ok(request) = build_push_request(&current_config, &validated, &delivery.payload) else {
            continue;
        };
        let Ok(status) = transport(request) else {
            continue;
        };
        if classify_push_status(status) != PushDeliveryOutcome::Expired {
            continue;
        }
        let _ = crate::remote::mutate_host_config(&inner, |config| {
            // A browser can replace a subscription while an old request is in
            // flight. A terminal response for the old request must not delete
            // that newer registration merely because the endpoint was reused.
            config
                .web
                .push
                .expire_subscription_if_matches(&current_subscription)
        });
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct PushRegistrationRequest {
    #[serde(default)]
    pub mode: PushRegistrationMode,
    pub endpoint: String,
    pub keys: PushRegistrationKeys,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum PushRegistrationMode {
    Enable,
    #[default]
    Reconcile,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct PushRegistrationKeys {
    pub p256dh: String,
    pub auth: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct PushUnsubscribeRequest {
    #[serde(default)]
    pub endpoint: Option<String>,
    #[serde(default)]
    pub disable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedPushSubscription {
    pub endpoint: String,
    pub p256dh: String,
    pub auth: String,
}

pub fn validate_registration(
    request: PushRegistrationRequest,
) -> Result<ValidatedPushSubscription, String> {
    if request.endpoint.is_empty() || request.endpoint.len() > MAX_ENDPOINT_BYTES {
        return Err("Push endpoint length is invalid.".to_string());
    }
    if request.keys.p256dh.len() > MAX_KEY_TEXT_BYTES
        || request.keys.auth.len() > MAX_KEY_TEXT_BYTES
    {
        return Err("Push subscription keys are too large.".to_string());
    }
    validate_push_endpoint(&request.endpoint)?;

    let public = URL_SAFE_NO_PAD
        .decode(&request.keys.p256dh)
        .map_err(|_| "Push p256dh key is not valid base64url.".to_string())?;
    if public.len() != 65 || public.first() != Some(&4) {
        return Err("Push p256dh key must be an uncompressed P-256 key.".to_string());
    }
    web_push_native::p256::PublicKey::from_sec1_bytes(&public)
        .map_err(|_| "Push p256dh key is not valid P-256 material.".to_string())?;

    let auth = URL_SAFE_NO_PAD
        .decode(&request.keys.auth)
        .map_err(|_| "Push auth key is not valid base64url.".to_string())?;
    if auth.len() != 16 {
        return Err("Push auth key must contain exactly 16 bytes.".to_string());
    }

    Ok(ValidatedPushSubscription {
        endpoint: request.endpoint,
        p256dh: request.keys.p256dh,
        auth: request.keys.auth,
    })
}

pub(crate) fn validate_push_endpoint(endpoint: &str) -> Result<(), String> {
    let uri = endpoint
        .parse::<Uri>()
        .map_err(|_| "Push endpoint URL is invalid.".to_string())?;
    if uri.scheme_str() != Some("https") {
        return Err("Push endpoint must use HTTPS.".to_string());
    }
    let Some(authority) = uri.authority() else {
        return Err("Push endpoint host is missing.".to_string());
    };
    if authority.as_str().contains('@') {
        return Err("Push endpoint must not contain user information.".to_string());
    }
    if authority.port_u16().is_some_and(|port| port != 443) {
        return Err("Push endpoint must use the standard HTTPS port.".to_string());
    }
    let host = authority.host().trim_end_matches('.').to_ascii_lowercase();
    let allowed = host == "web.push.apple.com"
        || host.ends_with(".push.apple.com")
        || host == "fcm.googleapis.com"
        || host == "updates.push.services.mozilla.com"
        || host.ends_with(".notify.windows.com");
    if !allowed {
        return Err("Push endpoint is not an approved browser push service.".to_string());
    }
    if uri.path().is_empty() || uri.path() == "/" {
        return Err("Push endpoint path is missing.".to_string());
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum PushAttentionKind {
    NeedsInput,
    Completed,
    ServerCrashed,
    SshDisconnected,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PushPayload {
    pub title: String,
    pub body: String,
    pub route: String,
    pub tag: String,
    pub event_id: String,
    pub runtime_instance_id: String,
    pub action: PushAttentionKind,
    pub badge: u64,
}

impl PushPayload {
    #[allow(clippy::too_many_arguments)]
    pub fn attention(
        runtime_instance_id: impl Into<String>,
        stable_session_key: &StableSessionKey,
        action: PushAttentionKind,
        project_label: &str,
        session_label: &str,
        event_id: impl Into<String>,
        badge: u64,
    ) -> Self {
        let event_id = event_id.into();
        let title = match action {
            PushAttentionKind::NeedsInput => "DevManager needs input",
            PushAttentionKind::Completed => "DevManager task completed",
            PushAttentionKind::ServerCrashed => "DevManager server stopped",
            PushAttentionKind::SshDisconnected => "DevManager SSH disconnected",
        }
        .to_string();
        let project = clean_label(project_label, "Project");
        let session = clean_label(session_label, "Session");
        Self {
            title,
            body: format!("{project} · {session}"),
            route: route_for_stable_key(stable_session_key),
            tag: format!("devmanager-{event_id}"),
            event_id,
            runtime_instance_id: runtime_instance_id.into(),
            action,
            badge,
        }
    }
}

fn clean_label(value: &str, fallback: &str) -> String {
    let cleaned = value
        .chars()
        .filter(|character| !character.is_control())
        .take(80)
        .collect::<String>();
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        fallback.to_string()
    } else {
        cleaned.to_string()
    }
}

fn route_for_stable_key(key: &StableSessionKey) -> String {
    if let Some(id) = key.as_str().strip_prefix("tab:") {
        safe_route_id(id)
            .map(|id| format!("/session/tab/{id}"))
            .unwrap_or_else(|| "/sessions".to_string())
    } else if let Some(id) = key.as_str().strip_prefix("server:") {
        safe_route_id(id)
            .map(|id| format!("/session/server/{id}"))
            .unwrap_or_else(|| "/sessions".to_string())
    } else {
        "/sessions".to_string()
    }
}

fn safe_route_id(id: &str) -> Option<&str> {
    if !id.is_empty()
        && id.len() <= 256
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        Some(id)
    } else {
        None
    }
}

pub fn build_push_request(
    config: &WebPushConfig,
    subscription: &ValidatedPushSubscription,
    payload: &PushPayload,
) -> Result<Request<Vec<u8>>, String> {
    let private = URL_SAFE_NO_PAD
        .decode(&config.vapid_private_key_base64)
        .map_err(|_| "Stored VAPID private key is invalid.".to_string())?;
    let key_pair = ES256KeyPair::from_bytes(&private)
        .map_err(|_| "Stored VAPID private key is invalid.".to_string())?;
    let public = URL_SAFE_NO_PAD
        .decode(&subscription.p256dh)
        .map_err(|_| "Stored push public key is invalid.".to_string())?;
    let public = web_push_native::p256::PublicKey::from_sec1_bytes(&public)
        .map_err(|_| "Stored push public key is invalid.".to_string())?;
    let auth = URL_SAFE_NO_PAD
        .decode(&subscription.auth)
        .map_err(|_| "Stored push auth key is invalid.".to_string())?;
    if auth.len() != 16 {
        return Err("Stored push auth key is invalid.".to_string());
    }
    let auth = Auth::clone_from_slice(&auth);
    let endpoint = subscription
        .endpoint
        .parse::<Uri>()
        .map_err(|_| "Stored push endpoint is invalid.".to_string())?;
    let body = serde_json::to_vec(payload)
        .map_err(|error| format!("Cannot encode push payload: {error}"))?;

    WebPushBuilder::new(endpoint, public, auth)
        .with_valid_duration(PUSH_TTL)
        .with_vapid(&key_pair, VAPID_CONTACT)
        .build(body)
        .map_err(|error| format!("Cannot encrypt push payload: {error}"))
}

fn canonical_enabled_client_ids(enabled_client_ids: &[String]) -> Vec<String> {
    let mut unique = Vec::new();
    for client_id in enabled_client_ids {
        if !unique.contains(client_id) {
            unique.push(client_id.clone());
            if unique.len() == MAX_PUSH_SUBSCRIPTIONS {
                break;
            }
        }
    }
    unique
}

fn canonical_subscriptions(
    enabled_client_ids: &[String],
    subscriptions: &[WebPushSubscription],
) -> Vec<WebPushSubscription> {
    canonical_enabled_client_ids(enabled_client_ids)
        .iter()
        .filter_map(|client_id| {
            subscriptions
                .iter()
                .filter(|subscription| subscription.client_id == *client_id)
                .max_by(|left, right| {
                    left.created_at_epoch_ms
                        .cmp(&right.created_at_epoch_ms)
                        .then_with(|| left.endpoint.cmp(&right.endpoint))
                        .then_with(|| left.p256dh.cmp(&right.p256dh))
                        .then_with(|| left.auth.cmp(&right.auth))
                })
                .cloned()
        })
        .collect()
}

pub fn eligible_subscriptions(
    config: &WebPushConfig,
    visibly_focused_client_ids: &[String],
) -> Vec<WebPushSubscription> {
    canonical_subscriptions(&config.enabled_client_ids, &config.subscriptions)
        .into_iter()
        .filter(|subscription| {
            !visibly_focused_client_ids
                .iter()
                .any(|client_id| client_id == &subscription.client_id)
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushDeliveryOutcome {
    Delivered,
    Expired,
    Retryable,
}

pub fn classify_push_status(status: u16) -> PushDeliveryOutcome {
    match status {
        200..=299 => PushDeliveryOutcome::Delivered,
        404 | 410 => PushDeliveryOutcome::Expired,
        _ => PushDeliveryOutcome::Retryable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::test_support::TestProfileGuard;
    use crate::remote::{RemoteHostConfig, RemoteHostService};
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use std::sync::atomic::AtomicUsize;
    use std::sync::{mpsc, Arc, Barrier, Mutex};
    use std::time::Instant;

    fn valid_registration() -> PushRegistrationRequest {
        let application_key = WebPushConfig::default().vapid_public_key_base64;
        PushRegistrationRequest {
            mode: PushRegistrationMode::Reconcile,
            endpoint: "https://web.push.apple.com/QM-valid-endpoint".to_string(),
            keys: PushRegistrationKeys {
                p256dh: application_key,
                auth: URL_SAFE_NO_PAD.encode([7_u8; 16]),
            },
        }
    }

    fn delivery(
        config: &WebPushConfig,
        subscription: &WebPushSubscription,
        event_id: &str,
    ) -> PushDelivery {
        PushDelivery {
            config: config.clone(),
            subscription: subscription.clone(),
            payload: PushPayload::attention(
                "runtime-1",
                &StableSessionKey::from_tab("tab-1"),
                PushAttentionKind::Completed,
                "Project",
                "Claude",
                event_id,
                1,
            ),
        }
    }

    #[test]
    fn legacy_config_generates_a_matching_vapid_pair_without_subscriptions() {
        let mut config: WebPushConfig = serde_json::from_str("{}").unwrap();
        config.ensure_keys();

        assert_eq!(
            URL_SAFE_NO_PAD
                .decode(&config.vapid_private_key_base64)
                .unwrap()
                .len(),
            32
        );
        assert_eq!(
            URL_SAFE_NO_PAD
                .decode(&config.vapid_public_key_base64)
                .unwrap()
                .len(),
            65
        );
        assert!(config.subscriptions.is_empty());
        assert!(config.keys_are_valid());
        assert!(!format!("{config:?}").contains(&config.vapid_private_key_base64));
    }

    #[test]
    fn legacy_subscriptions_migrate_to_enabled_intent() {
        let mut legacy = WebPushConfig::default();
        legacy.subscriptions.push(WebPushSubscription {
            client_id: "phone".to_string(),
            endpoint: valid_registration().endpoint,
            p256dh: valid_registration().keys.p256dh,
            auth: valid_registration().keys.auth,
            created_at_epoch_ms: 1,
        });
        let mut serialized = serde_json::to_value(legacy).unwrap();
        serialized
            .as_object_mut()
            .unwrap()
            .remove("enabledClientIds");
        serialized
            .as_object_mut()
            .unwrap()
            .remove("intentSchemaVersion");
        let mut migrated: WebPushConfig = serde_json::from_value(serialized).unwrap();

        migrated.ensure_keys();

        assert!(migrated.notifications_enabled("phone"));
        assert!(!migrated.notifications_enabled("tablet"));
    }

    #[test]
    fn legacy_migration_keeps_only_the_newest_endpoint_for_each_client() {
        let mut legacy = WebPushConfig::default();
        for (endpoint, created_at_epoch_ms) in [
            ("https://web.push.apple.com/QM-phone-old", 1),
            ("https://web.push.apple.com/QM-phone-new", 2),
        ] {
            legacy.insert_legacy_subscription_for_test(
                "phone",
                validate_registration(PushRegistrationRequest {
                    endpoint: endpoint.to_string(),
                    ..valid_registration()
                })
                .unwrap(),
                created_at_epoch_ms,
            );
        }
        let mut serialized = serde_json::to_value(legacy).unwrap();
        serialized
            .as_object_mut()
            .unwrap()
            .remove("enabledClientIds");
        serialized
            .as_object_mut()
            .unwrap()
            .remove("intentSchemaVersion");
        let mut migrated: WebPushConfig = serde_json::from_value(serialized).unwrap();

        migrated.ensure_keys();

        assert!(migrated.notifications_enabled("phone"));
        assert_eq!(migrated.subscriptions.len(), 1);
        assert_eq!(
            migrated.subscriptions[0].endpoint,
            "https://web.push.apple.com/QM-phone-new"
        );
    }

    #[test]
    fn normalization_refilters_subscriptions_after_truncating_enabled_intent() {
        let mut config = WebPushConfig::default();
        config.enabled_client_ids = (0..=MAX_PUSH_SUBSCRIPTIONS)
            .map(|index| format!("client-{index}"))
            .collect();
        config.subscriptions = (0..=MAX_PUSH_SUBSCRIPTIONS)
            .map(|index| {
                WebPushSubscription::from_validated(
                    format!("client-{index}"),
                    validate_registration(PushRegistrationRequest {
                        endpoint: format!("https://web.push.apple.com/QM-client-{index}"),
                        ..valid_registration()
                    })
                    .unwrap(),
                    index as u64,
                )
            })
            .collect();

        config.ensure_keys();

        assert_eq!(config.enabled_client_ids.len(), MAX_PUSH_SUBSCRIPTIONS);
        assert_eq!(config.subscriptions.len(), MAX_PUSH_SUBSCRIPTIONS);
        assert!(!config.notifications_enabled(&format!("client-{}", MAX_PUSH_SUBSCRIPTIONS)));
        assert!(config
            .subscriptions
            .iter()
            .all(|subscription| { config.notifications_enabled(&subscription.client_id) }));
    }

    #[test]
    fn normalization_resolves_equal_timestamps_independent_of_saved_order() {
        let make_config = |endpoints: [&str; 2]| {
            let mut config = WebPushConfig::default();
            config.enabled_client_ids = vec!["phone".to_string()];
            config.subscriptions = endpoints
                .into_iter()
                .map(|endpoint| {
                    WebPushSubscription::from_validated(
                        "phone",
                        validate_registration(PushRegistrationRequest {
                            endpoint: endpoint.to_string(),
                            ..valid_registration()
                        })
                        .unwrap(),
                        7,
                    )
                })
                .collect();
            config
        };
        let mut forward = make_config([
            "https://web.push.apple.com/QM-phone-a",
            "https://web.push.apple.com/QM-phone-z",
        ]);
        let mut reversed = make_config([
            "https://web.push.apple.com/QM-phone-z",
            "https://web.push.apple.com/QM-phone-a",
        ]);

        forward.ensure_keys();
        reversed.ensure_keys();

        assert_eq!(
            forward.subscriptions[0].endpoint,
            reversed.subscriptions[0].endpoint
        );
        assert_eq!(
            forward.subscriptions[0].endpoint,
            "https://web.push.apple.com/QM-phone-z"
        );
    }

    #[test]
    fn normalized_disabled_intent_drops_stale_endpoint_without_reenabling() {
        let mut normalized = WebPushConfig::default();
        normalized.subscriptions.push(WebPushSubscription {
            client_id: "phone".to_string(),
            endpoint: valid_registration().endpoint,
            p256dh: valid_registration().keys.p256dh,
            auth: valid_registration().keys.auth,
            created_at_epoch_ms: 1,
        });
        normalized.enabled_client_ids.clear();
        let mut serialized = serde_json::to_value(normalized).unwrap();
        serialized["intentSchemaVersion"] = serde_json::Value::from(1);
        let mut loaded: WebPushConfig = serde_json::from_value(serialized).unwrap();

        loaded.ensure_keys();

        assert!(!loaded.notifications_enabled("phone"));
        assert!(loaded.subscriptions.is_empty());
    }

    #[test]
    fn key_rotation_drops_legacy_endpoint_but_preserves_migrated_intent() {
        let mut legacy = WebPushConfig::default();
        legacy.subscriptions.push(WebPushSubscription {
            client_id: "phone".to_string(),
            endpoint: valid_registration().endpoint,
            p256dh: valid_registration().keys.p256dh,
            auth: valid_registration().keys.auth,
            created_at_epoch_ms: 1,
        });
        legacy.vapid_public_key_base64 = "invalid".to_string();
        let mut serialized = serde_json::to_value(legacy).unwrap();
        serialized
            .as_object_mut()
            .unwrap()
            .remove("enabledClientIds");
        serialized
            .as_object_mut()
            .unwrap()
            .remove("intentSchemaVersion");
        let mut migrated: WebPushConfig = serde_json::from_value(serialized).unwrap();

        migrated.ensure_keys();

        assert!(migrated.subscriptions.is_empty());
        assert!(migrated.notifications_enabled("phone"));
    }

    #[test]
    fn invalid_or_changed_vapid_material_rotates_and_drops_bound_subscriptions() {
        let mut config = WebPushConfig::default();
        config.subscriptions.push(WebPushSubscription {
            client_id: "phone".to_string(),
            endpoint: valid_registration().endpoint,
            p256dh: valid_registration().keys.p256dh,
            auth: valid_registration().keys.auth,
            created_at_epoch_ms: 1,
        });
        config.vapid_public_key_base64 = "invalid".to_string();

        config.ensure_keys();

        assert!(config.keys_are_valid());
        assert!(config.subscriptions.is_empty());
    }

    #[test]
    fn registration_validation_is_push_service_allowlisted_and_key_strict() {
        assert!(validate_registration(valid_registration()).is_ok());

        let mut internal = valid_registration();
        internal.endpoint = "https://127.0.0.1/admin".to_string();
        assert!(validate_registration(internal)
            .unwrap_err()
            .contains("push service"));

        let mut http = valid_registration();
        http.endpoint = "http://web.push.apple.com/nope".to_string();
        assert!(validate_registration(http).unwrap_err().contains("HTTPS"));

        let mut userinfo = valid_registration();
        userinfo.endpoint = "https://attacker@web.push.apple.com/nope".to_string();
        assert!(validate_registration(userinfo)
            .unwrap_err()
            .contains("user information"));

        let mut bad_auth = valid_registration();
        bad_auth.keys.auth = URL_SAFE_NO_PAD.encode([0_u8; 15]);
        assert!(validate_registration(bad_auth)
            .unwrap_err()
            .contains("auth"));
    }

    #[test]
    fn explicit_enable_replaces_every_stale_endpoint_for_only_that_client() {
        let mut config = WebPushConfig::default();
        for (client_id, endpoint, created_at) in [
            ("phone", "https://web.push.apple.com/QM-phone-old-a", 1),
            ("phone", "https://web.push.apple.com/QM-phone-old-b", 2),
            ("tablet", "https://web.push.apple.com/QM-tablet", 3),
        ] {
            config.insert_legacy_subscription_for_test(
                client_id,
                validate_registration(PushRegistrationRequest {
                    endpoint: endpoint.to_string(),
                    ..valid_registration()
                })
                .unwrap(),
                created_at,
            );
        }
        let replacement_endpoint = "https://web.push.apple.com/QM-phone-current";
        let replacement = validate_registration(PushRegistrationRequest {
            endpoint: replacement_endpoint.to_string(),
            ..valid_registration()
        })
        .unwrap();

        config
            .enable_and_replace_subscription("phone", replacement, 4)
            .unwrap();

        assert!(config.notifications_enabled("phone"));
        let phone = config
            .subscriptions
            .iter()
            .filter(|subscription| subscription.client_id == "phone")
            .collect::<Vec<_>>();
        assert_eq!(phone.len(), 1);
        assert_eq!(phone[0].endpoint, replacement_endpoint);
        assert!(config
            .subscriptions
            .iter()
            .any(|subscription| subscription.client_id == "tablet"));
    }

    #[test]
    fn automatic_reconcile_cannot_register_while_host_intent_is_disabled() {
        let mut config = WebPushConfig::default();
        let subscription = validate_registration(valid_registration()).unwrap();

        let enabled = config.reconcile_and_replace_subscription("phone", subscription, 1);

        assert!(!enabled);
        assert!(!config.notifications_enabled("phone"));
        assert!(config.subscriptions.is_empty());
    }

    #[test]
    fn explicit_disable_clears_intent_and_every_endpoint_for_only_that_client() {
        let mut config = WebPushConfig::default();
        for (client_id, endpoint, created_at) in [
            ("phone", "https://web.push.apple.com/QM-phone", 1),
            ("tablet", "https://web.push.apple.com/QM-tablet", 2),
        ] {
            config
                .enable_and_replace_subscription(
                    client_id,
                    validate_registration(PushRegistrationRequest {
                        endpoint: endpoint.to_string(),
                        ..valid_registration()
                    })
                    .unwrap(),
                    created_at,
                )
                .unwrap();
        }

        assert!(config.disable_client("phone"));

        assert!(!config.notifications_enabled("phone"));
        assert!(config.notifications_enabled("tablet"));
        assert!(config
            .subscriptions
            .iter()
            .all(|subscription| subscription.client_id != "phone"));
        assert!(config
            .subscriptions
            .iter()
            .any(|subscription| subscription.client_id == "tablet"));
    }

    #[test]
    fn explicit_enable_rejects_a_new_client_at_the_intent_cap_but_allows_rotation() {
        let mut config = WebPushConfig::default();
        for index in 0..MAX_PUSH_SUBSCRIPTIONS {
            let registration = PushRegistrationRequest {
                endpoint: format!("https://web.push.apple.com/QM-client-{index}"),
                ..valid_registration()
            };
            config
                .enable_and_replace_subscription(
                    &format!("client-{index}"),
                    validate_registration(registration).unwrap(),
                    index as u64,
                )
                .expect("client below intent cap should be enabled");
        }

        let overflow = config.enable_and_replace_subscription(
            "overflow",
            validate_registration(PushRegistrationRequest {
                endpoint: "https://web.push.apple.com/QM-overflow".to_string(),
                ..valid_registration()
            })
            .unwrap(),
            100,
        );

        assert_eq!(overflow, Err(PushEnableError::ClientLimitReached));
        assert!(!config.notifications_enabled("overflow"));
        assert!(config
            .subscriptions
            .iter()
            .all(|subscription| subscription.client_id != "overflow"));

        let rotated_endpoint = "https://web.push.apple.com/QM-client-0-rotated";
        config
            .enable_and_replace_subscription(
                "client-0",
                validate_registration(PushRegistrationRequest {
                    endpoint: rotated_endpoint.to_string(),
                    ..valid_registration()
                })
                .unwrap(),
                101,
            )
            .expect("enabled client should rotate at the intent cap");
        assert_eq!(config.enabled_client_ids.len(), MAX_PUSH_SUBSCRIPTIONS);
        assert_eq!(
            config
                .subscriptions
                .iter()
                .find(|subscription| subscription.client_id == "client-0")
                .unwrap()
                .endpoint,
            rotated_endpoint
        );
    }

    #[test]
    fn terminal_delivery_expiry_removes_endpoint_but_preserves_enabled_intent() {
        let mut config = WebPushConfig::default();
        config
            .enable_and_replace_subscription(
                "phone",
                validate_registration(valid_registration()).unwrap(),
                1,
            )
            .unwrap();
        let expired = config.subscriptions[0].clone();

        assert!(config.expire_subscription_if_matches(&expired));

        assert!(config.subscriptions.is_empty());
        assert!(config.notifications_enabled("phone"));
    }

    #[test]
    fn client_revocation_removes_endpoint_and_enabled_intent() {
        let mut config = WebPushConfig::default();
        config
            .enable_and_replace_subscription(
                "phone",
                validate_registration(valid_registration()).unwrap(),
                1,
            )
            .unwrap();

        assert!(config.remove_client("phone"));

        assert!(config.subscriptions.is_empty());
        assert!(!config.notifications_enabled("phone"));
    }

    #[test]
    fn payload_is_actionable_minimal_and_contains_no_terminal_or_prompt_content() {
        let payload = PushPayload::attention(
            "runtime-1",
            &StableSessionKey::from_tab("tab-1"),
            PushAttentionKind::NeedsInput,
            "Project Alpha",
            "Claude",
            "event-1",
            2,
        );
        let json = serde_json::to_string(&payload).unwrap();

        assert_eq!(payload.route, "/session/tab/tab-1");
        assert_eq!(payload.badge, 2);
        assert!(!json.contains("PROMPT_SENTINEL"));
        assert!(!json.contains("OUTPUT_SENTINEL"));
        assert!(!json.contains("prompt"));
        assert!(!json.contains("terminal"));

        let unsafe_route = PushPayload::attention(
            "runtime-1",
            &StableSessionKey::from_tab("../other"),
            PushAttentionKind::NeedsInput,
            "Project",
            "Claude",
            "event-unsafe",
            1,
        );
        assert_eq!(unsafe_route.route, "/sessions");
    }

    #[test]
    fn request_builder_encrypts_the_json_and_targets_the_validated_endpoint() {
        let config = WebPushConfig::default();
        let subscription = validate_registration(valid_registration()).unwrap();
        let payload = PushPayload::attention(
            "runtime-1",
            &StableSessionKey::from_tab("tab-1"),
            PushAttentionKind::Completed,
            "PROMPT_SENTINEL",
            "OUTPUT_SENTINEL",
            "event-2",
            1,
        );

        let request = build_push_request(&config, &subscription, &payload).unwrap();

        assert_eq!(request.uri().to_string(), subscription.endpoint);
        assert_eq!(request.method(), "POST");
        assert!(!String::from_utf8_lossy(request.body()).contains("PROMPT_SENTINEL"));
        assert!(!String::from_utf8_lossy(request.body()).contains("OUTPUT_SENTINEL"));
    }

    #[test]
    fn visible_client_is_suppressed_without_suppressing_other_installs() {
        let mut config = WebPushConfig::default();
        config.enabled_client_ids = vec!["phone".to_string(), "tablet".to_string()];
        config.subscriptions = vec![
            WebPushSubscription::from_validated(
                "phone",
                validate_registration(valid_registration()).unwrap(),
                1,
            ),
            WebPushSubscription::from_validated(
                "tablet",
                validate_registration(PushRegistrationRequest {
                    endpoint: "https://web.push.apple.com/QM-tablet".to_string(),
                    ..valid_registration()
                })
                .unwrap(),
                2,
            ),
        ];

        let eligible = eligible_subscriptions(&config, &["phone".to_string()]);

        assert_eq!(eligible.len(), 1);
        assert_eq!(eligible[0].client_id, "tablet");
    }

    #[test]
    fn malformed_disabled_endpoint_is_never_eligible_for_delivery() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let subscription = WebPushSubscription::from_validated(
            "phone",
            validate_registration(valid_registration()).unwrap(),
            1,
        );
        {
            let mut config = service.inner.config.write().unwrap();
            config.web.push.enabled_client_ids.clear();
            config.web.push.subscriptions = vec![subscription];
        }
        let (sender, receiver) = mpsc::sync_channel(2);
        *service.inner.web_push_sender.write().unwrap() = Some(PushSender::single(sender));

        service.enqueue_push_attention(
            None,
            &StableSessionKey::from_tab("tab-1"),
            PushAttentionKind::Completed,
        );

        assert!(receiver.recv_timeout(Duration::from_millis(100)).is_err());
    }

    #[test]
    fn malformed_duplicate_endpoints_deliver_only_the_newest_registration() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let old = WebPushSubscription::from_validated(
            "phone",
            validate_registration(PushRegistrationRequest {
                endpoint: "https://web.push.apple.com/QM-phone-old".to_string(),
                ..valid_registration()
            })
            .unwrap(),
            1,
        );
        let newest = WebPushSubscription::from_validated(
            "phone",
            validate_registration(PushRegistrationRequest {
                endpoint: "https://web.push.apple.com/QM-phone-new".to_string(),
                ..valid_registration()
            })
            .unwrap(),
            2,
        );
        {
            let mut config = service.inner.config.write().unwrap();
            config.web.push.enabled_client_ids = vec!["phone".to_string()];
            config.web.push.subscriptions = vec![old, newest];
        }
        let (sender, receiver) = mpsc::sync_channel(3);
        *service.inner.web_push_sender.write().unwrap() = Some(PushSender::single(sender));

        service.enqueue_push_attention(
            None,
            &StableSessionKey::from_tab("tab-1"),
            PushAttentionKind::Completed,
        );

        let delivery = receiver
            .recv_timeout(Duration::from_millis(100))
            .expect("newest subscription should remain eligible");
        assert_eq!(
            delivery.subscription.endpoint,
            "https://web.push.apple.com/QM-phone-new"
        );
        assert!(receiver.recv_timeout(Duration::from_millis(100)).is_err());
    }

    #[test]
    fn terminal_push_statuses_expire_subscriptions() {
        assert_eq!(classify_push_status(201), PushDeliveryOutcome::Delivered);
        assert_eq!(classify_push_status(404), PushDeliveryOutcome::Expired);
        assert_eq!(classify_push_status(410), PushDeliveryOutcome::Expired);
        assert_eq!(classify_push_status(429), PushDeliveryOutcome::Retryable);
        assert_eq!(classify_push_status(500), PushDeliveryOutcome::Retryable);
    }

    #[test]
    fn dispatcher_expires_terminal_subscriptions_through_persisted_host_config() {
        let _profile = TestProfileGuard::new("push-expiry-worker");
        let mut config = RemoteHostConfig::default();
        let validated = validate_registration(valid_registration()).unwrap();
        config
            .web
            .push
            .enable_and_replace_subscription("phone", validated, 1)
            .unwrap();
        let service = RemoteHostService::new(config);
        let push = service.config().web.push;
        let subscription = push.subscriptions[0].clone();
        let (delivered_tx, delivered_rx) = mpsc::channel();
        let transport: PushTransport = Arc::new(move |_| {
            let _ = delivered_tx.send(());
            Ok(410)
        });
        let dispatcher =
            PushDispatcher::start_with_transport(Arc::downgrade(&service.inner), 2, 1, transport)
                .unwrap();

        dispatcher
            .sender()
            .send(delivery(&push, &subscription, "expired-event"))
            .unwrap();
        delivered_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("delivery attempted");

        let deadline = Instant::now() + Duration::from_secs(2);
        while !service.config().web.push.subscriptions.is_empty() && Instant::now() < deadline {
            thread::yield_now();
        }
        assert!(service.config().web.push.subscriptions.is_empty());
        drop(dispatcher);
    }

    #[test]
    fn dispatcher_reauthorizes_queued_work_after_subscription_revocation() {
        let mut config = RemoteHostConfig::default();
        let validated = validate_registration(valid_registration()).unwrap();
        config
            .web
            .push
            .enable_and_replace_subscription("phone", validated, 1)
            .unwrap();
        let service = RemoteHostService::new(config);
        let push = service.config().web.push;
        let subscription = push.subscriptions[0].clone();
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let worker_entered = entered.clone();
        let worker_release = release.clone();
        let calls = Arc::new(AtomicUsize::new(0));
        let worker_calls = calls.clone();
        let (called_tx, called_rx) = mpsc::channel();
        let transport: PushTransport = Arc::new(move |_| {
            let call = worker_calls.fetch_add(1, Ordering::AcqRel);
            let _ = called_tx.send(());
            if call == 0 {
                worker_entered.wait();
                worker_release.wait();
            }
            Ok(201)
        });
        let dispatcher =
            PushDispatcher::start_with_transport(Arc::downgrade(&service.inner), 2, 1, transport)
                .unwrap();

        dispatcher
            .sender()
            .send(delivery(&push, &subscription, "in-flight"))
            .unwrap();
        called_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("first delivery reached transport");
        entered.wait();
        dispatcher
            .sender()
            .send(delivery(&push, &subscription, "queued"))
            .unwrap();
        service
            .inner
            .config
            .write()
            .unwrap()
            .web
            .push
            .disable_client("phone");
        release.wait();

        assert!(
            called_rx.recv_timeout(Duration::from_millis(250)).is_err(),
            "revoked queued work must never reach the network"
        );
        drop(dispatcher);
    }

    #[test]
    fn dispatcher_reauthorizes_queued_work_against_current_enabled_intent() {
        let mut config = RemoteHostConfig::default();
        let validated = validate_registration(valid_registration()).unwrap();
        config
            .web
            .push
            .enable_and_replace_subscription("phone", validated, 1)
            .unwrap();
        let service = RemoteHostService::new(config);
        let push = service.config().web.push;
        let subscription = push.subscriptions[0].clone();
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let worker_entered = entered.clone();
        let worker_release = release.clone();
        let calls = Arc::new(AtomicUsize::new(0));
        let worker_calls = calls.clone();
        let (called_tx, called_rx) = mpsc::channel();
        let transport: PushTransport = Arc::new(move |_| {
            let call = worker_calls.fetch_add(1, Ordering::AcqRel);
            let _ = called_tx.send(());
            if call == 0 {
                worker_entered.wait();
                worker_release.wait();
            }
            Ok(201)
        });
        let dispatcher =
            PushDispatcher::start_with_transport(Arc::downgrade(&service.inner), 2, 1, transport)
                .unwrap();

        dispatcher
            .sender()
            .send(delivery(&push, &subscription, "in-flight"))
            .unwrap();
        called_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("first delivery reached transport");
        entered.wait();
        dispatcher
            .sender()
            .send(delivery(&push, &subscription, "queued"))
            .unwrap();
        service
            .inner
            .config
            .write()
            .unwrap()
            .web
            .push
            .enabled_client_ids
            .clear();
        release.wait();

        assert!(
            called_rx.recv_timeout(Duration::from_millis(250)).is_err(),
            "queued work for disabled intent must never reach the network"
        );
        drop(dispatcher);
    }

    #[test]
    fn dispatcher_preserves_delivery_order_for_each_subscription() {
        let mut config = RemoteHostConfig::default();
        let validated = validate_registration(valid_registration()).unwrap();
        config
            .web
            .push
            .enable_and_replace_subscription("phone", validated, 1)
            .unwrap();
        let service = RemoteHostService::new(config);
        let push = service.config().web.push;
        let subscription = push.subscriptions[0].clone();
        let calls = Arc::new(AtomicUsize::new(0));
        let worker_calls = calls.clone();
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::sync_channel(0);
        let release_rx = Arc::new(Mutex::new(release_rx));
        let worker_release = release_rx.clone();
        let transport: PushTransport = Arc::new(move |_| {
            let call = worker_calls.fetch_add(1, Ordering::AcqRel);
            started_tx.send(call).expect("record transport start");
            if call == 0 {
                worker_release
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_millis(500))
                    .expect("release first delivery");
            }
            Ok(201)
        });
        let dispatcher =
            PushDispatcher::start_with_transport(Arc::downgrade(&service.inner), 8, 2, transport)
                .unwrap();
        let sender = dispatcher.sender();

        sender
            .send(delivery(&push, &subscription, "badge-1"))
            .unwrap();
        assert_eq!(started_rx.recv_timeout(Duration::from_secs(2)).unwrap(), 0);
        sender
            .send(delivery(&push, &subscription, "badge-2"))
            .unwrap();
        assert!(
            started_rx.recv_timeout(Duration::from_millis(150)).is_err(),
            "later delivery reached transport before the first completed"
        );

        release_tx.send(()).unwrap();
        assert_eq!(started_rx.recv_timeout(Duration::from_secs(2)).unwrap(), 1);
        drop(sender);
        drop(dispatcher);
    }

    #[test]
    fn stale_terminal_response_cannot_remove_a_replacement_subscription() {
        let _profile = TestProfileGuard::new("push-stale-terminal-response");
        let mut config = RemoteHostConfig::default();
        let validated = validate_registration(valid_registration()).unwrap();
        config
            .web
            .push
            .enable_and_replace_subscription("phone", validated.clone(), 1)
            .unwrap();
        let service = RemoteHostService::new(config);
        let push = service.config().web.push;
        let old_subscription = push.subscriptions[0].clone();
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let worker_entered = entered.clone();
        let worker_release = release.clone();
        let transport: PushTransport = Arc::new(move |_| {
            worker_entered.wait();
            worker_release.wait();
            Ok(410)
        });
        let dispatcher =
            PushDispatcher::start_with_transport(Arc::downgrade(&service.inner), 2, 1, transport)
                .unwrap();

        dispatcher
            .sender()
            .send(delivery(&push, &old_subscription, "old-event"))
            .unwrap();
        entered.wait();
        service
            .inner
            .config
            .write()
            .unwrap()
            .web
            .push
            .enable_and_replace_subscription("phone", validated, 2)
            .unwrap();
        release.wait();
        drop(dispatcher);

        let subscriptions = &service.config().web.push.subscriptions;
        assert_eq!(subscriptions.len(), 1);
        assert_eq!(subscriptions[0].created_at_epoch_ms, 2);
    }

    #[test]
    fn dispatcher_queue_is_bounded_and_admission_never_blocks() {
        let mut config = RemoteHostConfig::default();
        let validated = validate_registration(valid_registration()).unwrap();
        config
            .web
            .push
            .enable_and_replace_subscription("phone", validated, 1)
            .unwrap();
        let service = RemoteHostService::new(config);
        let push = service.config().web.push;
        let subscription = push.subscriptions[0].clone();
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let worker_entered = entered.clone();
        let worker_release = release.clone();
        let transport: PushTransport = Arc::new(move |_| {
            worker_entered.wait();
            worker_release.wait();
            Ok(201)
        });
        let dispatcher =
            PushDispatcher::start_with_transport(Arc::downgrade(&service.inner), 1, 1, transport)
                .unwrap();
        let sender = dispatcher.sender();

        sender
            .try_send(delivery(&push, &subscription, "in-flight"))
            .unwrap();
        entered.wait();
        sender
            .try_send(delivery(&push, &subscription, "queued"))
            .unwrap();
        assert!(matches!(
            sender.try_send(delivery(&push, &subscription, "overflow")),
            Err(mpsc::TrySendError::Full(_))
        ));

        release.wait();
        drop(sender);
        drop(dispatcher);
    }
}
