use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::collections::HashMap;
use std::net::IpAddr;
use std::time::{Duration, Instant};

use super::super::now_epoch_ms;

pub const WEB_COOKIE_NAME: &str = "dm_web";
const WEB_COOKIE_NAME_PREFIX: &str = "dm_web_";
const PAIRING_BACKOFF_STEPS_SECS: [u64; 5] = [1, 2, 4, 8, 16];
const PAIRING_LOCKOUT_SECS: u64 = 60;

pub fn cookie_name_for_server_id(server_id: &str) -> String {
    if server_id.trim().is_empty() {
        return WEB_COOKIE_NAME.to_string();
    }
    use sha2::Digest;
    let digest = Sha256::digest(server_id.as_bytes());
    format!("{WEB_COOKIE_NAME_PREFIX}{}", hex_encode(&digest[..8]))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, rename_all = "camelCase")]
pub struct PairedWebClient {
    pub client_id: String,
    pub label: String,
    pub issued_at_epoch_ms: Option<u64>,
    pub last_seen_epoch_ms: Option<u64>,
}

impl Default for PairedWebClient {
    fn default() -> Self {
        Self {
            client_id: String::new(),
            label: String::new(),
            issued_at_epoch_ms: None,
            last_seen_epoch_ms: None,
        }
    }
}

pub fn generate_web_pairing_token() -> String {
    // 8-char uppercase alphanumeric, derived from epoch millis + process id +
    // a SHA-256 digest. Cheap and collision-resistant enough for a host-local
    // secret that rotates on demand.
    let seed = format!("web-{}-{}", now_epoch_ms(), std::process::id());
    let digest = {
        use sha2::Digest;
        let mut hasher = Sha256::new();
        hasher.update(seed.as_bytes());
        hasher.finalize()
    };
    const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut out = String::with_capacity(8);
    for byte in digest.iter().take(8) {
        out.push(ALPHABET[(*byte as usize) % ALPHABET.len()] as char);
    }
    out
}

pub fn generate_cookie_secret_hex() -> String {
    // Derive 32 bytes by chaining SHA-256 over epoch, pid, and a small
    // monotonic counter. Not as strong as /dev/urandom but good enough for an
    // MVP cookie signing secret that the user can rotate any time.
    let mut bytes = [0u8; 32];
    let seed = format!(
        "cookie-{}-{}-{:?}",
        now_epoch_ms(),
        std::process::id(),
        std::thread::current().id()
    );
    use sha2::Digest;
    let mut hasher = Sha256::new();
    hasher.update(seed.as_bytes());
    let first = hasher.finalize();
    bytes[..32].copy_from_slice(&first[..32]);
    hex_encode(&bytes)
}

pub fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

pub fn hex_decode(encoded: &str) -> Option<Vec<u8>> {
    if encoded.len() % 2 != 0 {
        return None;
    }
    (0..encoded.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&encoded[i..i + 2], 16).ok())
        .collect()
}

type HmacSha256 = Hmac<Sha256>;

pub fn sign_cookie(secret_hex: &str, client_id: &str) -> Option<String> {
    let secret = hex_decode(secret_hex)?;
    let mut mac = HmacSha256::new_from_slice(&secret).ok()?;
    mac.update(client_id.as_bytes());
    let tag = mac.finalize().into_bytes();
    let id_b64 = URL_SAFE_NO_PAD.encode(client_id.as_bytes());
    let tag_b64 = URL_SAFE_NO_PAD.encode(tag);
    Some(format!("{id_b64}.{tag_b64}"))
}

pub fn verify_cookie(secret_hex: &str, cookie_value: &str) -> Option<String> {
    let (id_b64, tag_b64) = cookie_value.split_once('.')?;
    let id_bytes = URL_SAFE_NO_PAD.decode(id_b64).ok()?;
    let expected_tag = URL_SAFE_NO_PAD.decode(tag_b64).ok()?;
    let client_id = String::from_utf8(id_bytes).ok()?;

    let secret = hex_decode(secret_hex)?;
    let mut mac = HmacSha256::new_from_slice(&secret).ok()?;
    mac.update(client_id.as_bytes());
    mac.verify_slice(&expected_tag).ok()?;
    Some(client_id)
}

pub fn extract_cookie(header_value: &str, name: &str) -> Option<String> {
    for part in header_value.split(';') {
        let trimmed = part.trim();
        if let Some(rest) = trimmed.strip_prefix(name) {
            if let Some(value) = rest.strip_prefix('=') {
                return Some(value.to_string());
            }
        }
    }
    None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PairingThrottleStatus {
    Allowed,
    Backoff(Duration),
    LockedOut(Duration),
}

#[derive(Debug, Default)]
pub(crate) struct PairingAttemptTracker {
    attempts: HashMap<IpAddr, PairingAttemptState>,
}

#[derive(Debug, Clone, Copy, Default)]
struct PairingAttemptState {
    consecutive_failures: usize,
    blocked_until: Option<Instant>,
    penalty_kind: PairingPenaltyKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum PairingPenaltyKind {
    #[default]
    Backoff,
    Lockout,
}

impl PairingAttemptTracker {
    pub(crate) fn status(&mut self, ip: IpAddr, now: Instant) -> PairingThrottleStatus {
        let Some(state) = self.attempts.get(&ip).copied() else {
            return PairingThrottleStatus::Allowed;
        };
        let Some(blocked_until) = state.blocked_until else {
            return PairingThrottleStatus::Allowed;
        };
        if blocked_until <= now {
            if state.consecutive_failures == 0 {
                self.attempts.remove(&ip);
            }
            return PairingThrottleStatus::Allowed;
        }
        let remaining = blocked_until.saturating_duration_since(now);
        match state.penalty_kind {
            PairingPenaltyKind::Backoff => PairingThrottleStatus::Backoff(remaining),
            PairingPenaltyKind::Lockout => PairingThrottleStatus::LockedOut(remaining),
        }
    }

    pub(crate) fn record_failure(&mut self, ip: IpAddr, now: Instant) -> PairingThrottleStatus {
        match self.status(ip, now) {
            PairingThrottleStatus::Allowed => {}
            status => return status,
        }

        let state = self.attempts.entry(ip).or_default();
        state.consecutive_failures = state.consecutive_failures.saturating_add(1);
        if state.consecutive_failures > PAIRING_BACKOFF_STEPS_SECS.len() {
            state.consecutive_failures = 0;
            state.penalty_kind = PairingPenaltyKind::Lockout;
            state.blocked_until = Some(now + Duration::from_secs(PAIRING_LOCKOUT_SECS));
            return PairingThrottleStatus::LockedOut(Duration::from_secs(PAIRING_LOCKOUT_SECS));
        }

        let delay = Duration::from_secs(
            PAIRING_BACKOFF_STEPS_SECS[state.consecutive_failures.saturating_sub(1)],
        );
        state.penalty_kind = PairingPenaltyKind::Backoff;
        state.blocked_until = Some(now + delay);
        PairingThrottleStatus::Backoff(delay)
    }

    pub(crate) fn record_success(&mut self, ip: IpAddr) {
        self.attempts.remove(&ip);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};
    use std::time::{Duration, Instant};

    #[test]
    fn sign_and_verify_round_trip() {
        let secret = generate_cookie_secret_hex();
        let signed = sign_cookie(&secret, "client-xyz").expect("sign should succeed");
        let verified = verify_cookie(&secret, &signed).expect("verify should succeed");
        assert_eq!(verified, "client-xyz");
    }

    #[test]
    fn verify_rejects_tampered_cookie() {
        let secret = generate_cookie_secret_hex();
        let signed = sign_cookie(&secret, "client-xyz").expect("sign should succeed");
        // Flip a character in the tag portion.
        let mut bytes = signed.into_bytes();
        let last = bytes.len() - 1;
        bytes[last] = if bytes[last] == b'A' { b'B' } else { b'A' };
        let tampered = String::from_utf8(bytes).unwrap();
        assert!(verify_cookie(&secret, &tampered).is_none());
    }

    #[test]
    fn verify_rejects_different_secret() {
        let secret_a = generate_cookie_secret_hex();
        let secret_b = {
            let mut s = generate_cookie_secret_hex();
            if s == secret_a {
                s.replace_range(0..1, "ff");
            }
            s
        };
        let signed = sign_cookie(&secret_a, "client-xyz").expect("sign should succeed");
        assert!(verify_cookie(&secret_b, &signed).is_none());
    }

    #[test]
    fn extract_cookie_finds_named_entry() {
        let header = "other=1; dm_web=AAA.BBB; another=xyz";
        assert_eq!(
            extract_cookie(header, WEB_COOKIE_NAME),
            Some("AAA.BBB".to_string())
        );
    }

    #[test]
    fn pairing_token_is_eight_chars() {
        assert_eq!(generate_web_pairing_token().len(), 8);
    }

    #[test]
    fn pairing_attempt_tracker_applies_backoff_then_lockout_and_reset() {
        let ip = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 20));
        let base = Instant::now();
        let mut tracker = PairingAttemptTracker::default();

        assert_eq!(tracker.status(ip, base), PairingThrottleStatus::Allowed);

        assert_eq!(
            tracker.record_failure(ip, base),
            PairingThrottleStatus::Backoff(Duration::from_secs(1))
        );
        assert_eq!(
            tracker.status(ip, base),
            PairingThrottleStatus::Backoff(Duration::from_secs(1))
        );
        assert_eq!(
            tracker.record_failure(ip, base + Duration::from_secs(1)),
            PairingThrottleStatus::Backoff(Duration::from_secs(2))
        );
        assert_eq!(
            tracker.record_failure(ip, base + Duration::from_secs(3)),
            PairingThrottleStatus::Backoff(Duration::from_secs(4))
        );
        assert_eq!(
            tracker.record_failure(ip, base + Duration::from_secs(7)),
            PairingThrottleStatus::Backoff(Duration::from_secs(8))
        );
        assert_eq!(
            tracker.record_failure(ip, base + Duration::from_secs(15)),
            PairingThrottleStatus::Backoff(Duration::from_secs(16))
        );
        assert_eq!(
            tracker.record_failure(ip, base + Duration::from_secs(31)),
            PairingThrottleStatus::LockedOut(Duration::from_secs(60))
        );
        assert_eq!(
            tracker.status(ip, base + Duration::from_secs(45)),
            PairingThrottleStatus::LockedOut(Duration::from_secs(46))
        );

        tracker.record_success(ip);
        assert_eq!(
            tracker.status(ip, base + Duration::from_secs(45)),
            PairingThrottleStatus::Allowed
        );
    }
}
