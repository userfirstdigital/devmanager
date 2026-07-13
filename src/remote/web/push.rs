use crate::remote::presentation::StableSessionKey;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex, Weak};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use ureq::http::{Request, Uri};
use web_push_native::jwt_simple::algorithms::ES256KeyPair;
use web_push_native::p256::elliptic_curve::sec1::ToEncodedPoint;
use web_push_native::{Auth, WebPushBuilder};

pub const MAX_PUSH_SUBSCRIPTIONS: usize = 32;
pub const MAX_PUSH_SUBSCRIPTIONS_PER_CLIENT: usize = 2;
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
    pub subscriptions: Vec<WebPushSubscription>,
}

impl fmt::Debug for WebPushConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WebPushConfig")
            .field("vapid_private_key_base64", &"[REDACTED]")
            .field("vapid_public_key_base64", &self.vapid_public_key_base64)
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
            subscriptions: Vec::new(),
        }
    }
}

impl WebPushConfig {
    pub fn ensure_keys(&mut self) {
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

    pub fn upsert_subscription(
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

    pub fn remove_subscription(&mut self, client_id: &str, endpoint: &str) -> bool {
        let before = self.subscriptions.len();
        self.subscriptions.retain(|subscription| {
            subscription.client_id != client_id || subscription.endpoint != endpoint
        });
        self.subscriptions.len() != before
    }

    pub fn remove_client(&mut self, client_id: &str) -> bool {
        let before = self.subscriptions.len();
        self.subscriptions
            .retain(|subscription| subscription.client_id != client_id);
        self.subscriptions.len() != before
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

/// Bounded, off-PTY Web Push delivery pool. Queue admission is always a
/// non-blocking `try_send`; slow or unavailable push services can never hold a
/// terminal, semantic-journal, or browser-control lock.
pub(crate) struct PushDispatcher {
    sender: Option<SyncSender<PushDelivery>>,
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
        let (sender, receiver) = mpsc::sync_channel(queue_capacity.max(1));
        let receiver = Arc::new(Mutex::new(receiver));
        let stop = Arc::new(AtomicBool::new(false));
        let mut workers = Vec::with_capacity(worker_count.max(1));
        for worker_index in 0..worker_count.max(1) {
            let worker_receiver = receiver.clone();
            let worker_stop = stop.clone();
            let worker_inner = inner.clone();
            let worker_transport = transport.clone();
            match thread::Builder::new()
                .name(format!("devmanager-push-{worker_index}"))
                .spawn(move || {
                    push_worker_loop(worker_receiver, worker_stop, worker_inner, worker_transport)
                }) {
                Ok(worker) => workers.push(worker),
                Err(error) => {
                    stop.store(true, Ordering::Release);
                    drop(sender);
                    for worker in workers {
                        let _ = worker.join();
                    }
                    return Err(format!("failed to start Web Push worker: {error}"));
                }
            }
        }
        Ok(Self {
            sender: Some(sender),
            stop,
            workers,
        })
    }

    pub(crate) fn sender(&self) -> SyncSender<PushDelivery> {
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
    receiver: Arc<Mutex<Receiver<PushDelivery>>>,
    stop: Arc<AtomicBool>,
    inner: Weak<crate::remote::RemoteHostInner>,
    transport: PushTransport,
) {
    while !stop.load(Ordering::Acquire) {
        let received = receiver
            .lock()
            .map(|receiver| receiver.recv_timeout(DELIVERY_POLL_INTERVAL));
        let delivery = match received {
            Ok(Ok(delivery)) => delivery,
            Ok(Err(RecvTimeoutError::Timeout)) => continue,
            Ok(Err(RecvTimeoutError::Disconnected)) | Err(_) => break,
        };
        if stop.load(Ordering::Acquire) {
            break;
        }
        let Ok(validated) = delivery.subscription.validated() else {
            continue;
        };
        let Ok(request) = build_push_request(&delivery.config, &validated, &delivery.payload)
        else {
            continue;
        };
        let Ok(status) = transport(request) else {
            continue;
        };
        if classify_push_status(status) != PushDeliveryOutcome::Expired {
            continue;
        }
        let Some(inner) = inner.upgrade() else {
            continue;
        };
        let client_id = delivery.subscription.client_id;
        let endpoint = delivery.subscription.endpoint;
        let _ = crate::remote::mutate_host_config(&inner, |config| {
            config.web.push.remove_subscription(&client_id, &endpoint)
        });
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct PushRegistrationRequest {
    pub endpoint: String,
    pub keys: PushRegistrationKeys,
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
    pub endpoint: String,
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

pub fn eligible_subscriptions(
    subscriptions: &[WebPushSubscription],
    visibly_focused_client_ids: &[String],
) -> Vec<WebPushSubscription> {
    subscriptions
        .iter()
        .filter(|subscription| {
            !visibly_focused_client_ids
                .iter()
                .any(|client_id| client_id == &subscription.client_id)
        })
        .cloned()
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
    use std::sync::{mpsc, Arc, Barrier};
    use std::time::Instant;

    fn valid_registration() -> PushRegistrationRequest {
        let application_key = WebPushConfig::default().vapid_public_key_base64;
        PushRegistrationRequest {
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
        let subscriptions = vec![
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

        let eligible = eligible_subscriptions(&subscriptions, &["phone".to_string()]);

        assert_eq!(eligible.len(), 1);
        assert_eq!(eligible[0].client_id, "tablet");
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
        config.web.push.upsert_subscription("phone", validated, 1);
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
    fn dispatcher_queue_is_bounded_and_admission_never_blocks() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let push = service.config().web.push;
        let subscription = WebPushSubscription::from_validated(
            "phone",
            validate_registration(valid_registration()).unwrap(),
            1,
        );
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
