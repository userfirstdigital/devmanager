//! Direct-connect HTTP/WebSocket admission policy.
//!
//! Loopback may use the browser-defined trustworthy `localhost` origin.
//! LAN binding is opt-in and requires TLS whose SAN matches the advertised
//! host name. Pairing is a one-time POST; tokens never appear in URLs,
//! referrers, or query strings.
//!
//! Physical Connect frames on this path remain capped by
//! [`MAX_DIRECT_FRAME_BYTES`]. Realtime payloads must arrive as sealed frames
//! from a production-grade [`crate::connect::crypto::EndToEndChannel`] before
//! framing. This module never admits plaintext Connect application bytes.
//! Production construction uses snow Noise XX/IK plus OS-backed static custody.
//! Unsupported platforms and missing/mismatched custody remain fail-closed.

use std::collections::HashMap;
use std::fmt;
use std::net::IpAddr;
use std::time::{Duration, Instant};

/// Hard bound for a pairing POST body.
pub const MAX_DIRECT_PAIRING_BODY_BYTES: u64 = 4 * 1024;
/// Hard bound for Connect physical frames on the direct path.
///
/// Stricter than the protocol physical maximum so direct admission stays
/// conservative for browser and LAN peers.
pub const MAX_DIRECT_FRAME_BYTES: u64 = 256 * 1024;

pub const CSP_DIRECT: &str =
    "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; connect-src 'self'; frame-ancestors 'none'; base-uri 'self'; form-action 'self'";
pub const CACHE_NO_STORE: &str = "no-store";
pub const REFERRER_NO_REFERRER: &str = "no-referrer";
pub const X_CONTENT_TYPE_OPTIONS: &str = "nosniff";
pub const X_FRAME_OPTIONS: &str = "DENY";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectBindMode {
    Loopback,
    Lan,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectBindPolicy {
    pub mode: DirectBindMode,
    pub advertised_hostname: String,
    pub certificate_san_matches: bool,
}

impl DirectBindPolicy {
    pub fn loopback() -> Self {
        Self {
            mode: DirectBindMode::Loopback,
            advertised_hostname: "localhost".to_string(),
            certificate_san_matches: true,
        }
    }

    pub fn lan(advertised_hostname: impl Into<String>, certificate_san_matches: bool) -> Self {
        Self {
            mode: DirectBindMode::Lan,
            advertised_hostname: advertised_hostname.into(),
            certificate_san_matches,
        }
    }

    pub fn validate_transport(&self, scheme: &str) -> Result<(), DirectAdmitError> {
        match self.mode {
            DirectBindMode::Loopback => {
                if matches!(scheme, "http" | "https" | "ws" | "wss") {
                    Ok(())
                } else {
                    Err(DirectAdmitError::PlaintextLan)
                }
            }
            DirectBindMode::Lan => {
                if !matches!(scheme, "https" | "wss") {
                    return Err(DirectAdmitError::PlaintextLan);
                }
                if !self.certificate_san_matches {
                    return Err(DirectAdmitError::CertificateUntrusted);
                }
                Ok(())
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirectRequestView<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub scheme: &'a str,
    pub host: &'a str,
    pub origin: Option<&'a str>,
    pub referer: Option<&'a str>,
    pub query: Option<&'a str>,
    pub content_length: Option<u64>,
    pub advertised_hostname: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirectAdmitError {
    WrongOrigin,
    PlaintextLan,
    CertificateUntrusted,
    CredentialInUrl,
    CredentialInReferrer,
    Csrf,
    OversizedBody,
    MethodNotAllowed,
    RateLimited { retry_after_secs: u64 },
}

impl fmt::Display for DirectAdmitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::WrongOrigin => "Connect direct request origin is not trusted",
            Self::PlaintextLan => "Connect LAN bind requires TLS",
            Self::CertificateUntrusted => {
                "Connect certificate SAN does not match the advertised host"
            }
            Self::CredentialInUrl => "Connect pairing credentials must not appear in the URL",
            Self::CredentialInReferrer => "Connect pairing credentials must not appear in Referer",
            Self::Csrf => "Connect pairing request failed the origin check",
            Self::OversizedBody => "Connect request body exceeds the bound",
            Self::MethodNotAllowed => "Connect request method is not allowed",
            Self::RateLimited { .. } => "Connect pairing is rate limited",
        })
    }
}

impl DirectAdmitError {
    pub fn status_hint(&self) -> u16 {
        match self {
            Self::WrongOrigin | Self::Csrf => 403,
            Self::PlaintextLan | Self::CertificateUntrusted => 400,
            Self::CredentialInUrl | Self::CredentialInReferrer => 400,
            Self::OversizedBody => 413,
            Self::MethodNotAllowed => 405,
            Self::RateLimited { .. } => 429,
        }
    }
}

/// Reject pairing secrets in query strings or referrers.
pub fn query_contains_pairing_secret(query: Option<&str>) -> bool {
    let Some(query) = query else {
        return false;
    };
    query.split('&').any(|pair| {
        let key = pair.split('=').next().unwrap_or("");
        matches!(
            key,
            "t" | "token" | "pairing" | "pairingToken" | "pairing_token"
        )
    })
}

pub fn referer_contains_pairing_secret(referer: Option<&str>) -> bool {
    let Some(referer) = referer else {
        return false;
    };
    referer.contains('?') && query_contains_pairing_secret(referer.split_once('?').map(|(_, q)| q))
}

pub fn is_trustworthy_loopback_host(host: &str) -> bool {
    // Parse the HTTP authority rather than splitting IPv6 at its first colon.
    // Paths, user-info and malformed ports are never loopback authority.
    let Ok(authority) = host.parse::<axum::http::uri::Authority>() else {
        return false;
    };
    if authority.as_str().contains('@') {
        return false;
    }
    matches!(
        authority.host().to_ascii_lowercase().as_str(),
        "localhost" | "127.0.0.1" | "[::1]" | "::1"
    )
}

pub fn origin_matches_host(origin: &str, scheme: &str, host: &str) -> bool {
    let Some((origin_scheme, rest)) = origin.split_once("://") else {
        return false;
    };
    if rest.contains('@')
        || rest.contains('/') && rest.split_once('/').is_some_and(|(_, path)| path != "")
    {
        return false;
    }
    let origin_host = rest.trim_end_matches('/');
    origin_scheme.eq_ignore_ascii_case(scheme) && origin_host.eq_ignore_ascii_case(host)
}

/// Admit one direct HTTP/WebSocket request before pairing or session start.
pub fn admit_direct_request(
    request: DirectRequestView<'_>,
    bind: &DirectBindPolicy,
    max_body: u64,
) -> Result<(), DirectAdmitError> {
    bind.validate_transport(request.scheme)?;
    if let Some(advertised) = request.advertised_hostname {
        if !advertised.eq_ignore_ascii_case(&bind.advertised_hostname)
            && bind.mode == DirectBindMode::Lan
        {
            return Err(DirectAdmitError::CertificateUntrusted);
        }
    }
    if query_contains_pairing_secret(request.query) {
        return Err(DirectAdmitError::CredentialInUrl);
    }
    if referer_contains_pairing_secret(request.referer) {
        return Err(DirectAdmitError::CredentialInReferrer);
    }
    if request.content_length.is_some_and(|len| len > max_body) {
        return Err(DirectAdmitError::OversizedBody);
    }
    match request.method {
        "GET" | "HEAD" if request.path == "/pair" => {
            return Err(DirectAdmitError::MethodNotAllowed);
        }
        "POST" if request.path == "/pair" => {
            let origin = request.origin.ok_or(DirectAdmitError::Csrf)?;
            if !origin_matches_host(origin, request.scheme, request.host)
                && !(is_trustworthy_loopback_host(request.host)
                    && origin_matches_host(origin, request.scheme, request.host))
            {
                return Err(DirectAdmitError::WrongOrigin);
            }
        }
        "GET" if request.path == "/api/connect" || request.path == "/api/ws" => {
            let origin = request.origin.ok_or(DirectAdmitError::WrongOrigin)?;
            if !origin_matches_host(origin, request.scheme, request.host)
                && !is_trustworthy_loopback_origin(origin, request.host)
            {
                return Err(DirectAdmitError::WrongOrigin);
            }
        }
        _ => {}
    }
    Ok(())
}

fn is_trustworthy_loopback_origin(origin: &str, host: &str) -> bool {
    is_trustworthy_loopback_host(host)
        && (origin_matches_host(origin, "http", host) || origin_matches_host(origin, "https", host))
}

pub fn security_headers() -> &'static [(&'static str, &'static str)] {
    &[
        ("Content-Security-Policy", CSP_DIRECT),
        ("Cache-Control", CACHE_NO_STORE),
        ("Referrer-Policy", REFERRER_NO_REFERRER),
        ("X-Content-Type-Options", X_CONTENT_TYPE_OPTIONS),
        ("X-Frame-Options", X_FRAME_OPTIONS),
    ]
}

/// Per-source pairing-code rate limit. Visible backoff, then lockout.
#[derive(Debug, Default)]
pub struct DirectPairingLimiter {
    states: HashMap<IpAddr, DirectPairingState>,
}

#[derive(Debug, Clone)]
struct DirectPairingState {
    consecutive_failures: usize,
    blocked_until: Option<Instant>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectPairingThrottle {
    Allowed,
    Backoff(Duration),
    LockedOut(Duration),
}

const BACKOFF_SECS: [u64; 5] = [1, 2, 4, 8, 16];
const LOCKOUT_SECS: u64 = 60;
pub const MAX_DIRECT_PAIRING_RATE_KEYS: usize = 4_096;

impl DirectPairingLimiter {
    fn ensure_capacity(&mut self, ip: IpAddr) -> Result<(), DirectAdmitError> {
        if self.states.len() < MAX_DIRECT_PAIRING_RATE_KEYS || self.states.contains_key(&ip) {
            return Ok(());
        }
        if let Some((&victim, _)) = self
            .states
            .iter()
            .find(|(_, state)| state.blocked_until.is_none())
        {
            self.states.remove(&victim);
            return Ok(());
        }
        Err(DirectAdmitError::RateLimited {
            retry_after_secs: LOCKOUT_SECS,
        })
    }

    pub fn status(&mut self, ip: IpAddr, now: Instant) -> DirectPairingThrottle {
        if self.ensure_capacity(ip).is_err() {
            return DirectPairingThrottle::LockedOut(Duration::from_secs(LOCKOUT_SECS));
        }
        let state = self.states.entry(ip).or_insert(DirectPairingState {
            consecutive_failures: 0,
            blocked_until: None,
        });
        if let Some(until) = state.blocked_until {
            if now < until {
                return if state.consecutive_failures > BACKOFF_SECS.len() {
                    DirectPairingThrottle::LockedOut(until.saturating_duration_since(now))
                } else {
                    DirectPairingThrottle::Backoff(until.saturating_duration_since(now))
                };
            }
            state.blocked_until = None;
        }
        DirectPairingThrottle::Allowed
    }

    pub fn record_failure(&mut self, ip: IpAddr, now: Instant) -> DirectPairingThrottle {
        if self.ensure_capacity(ip).is_err() {
            return DirectPairingThrottle::LockedOut(Duration::from_secs(LOCKOUT_SECS));
        }
        let state = self.states.entry(ip).or_insert(DirectPairingState {
            consecutive_failures: 0,
            blocked_until: None,
        });
        state.consecutive_failures = state.consecutive_failures.saturating_add(1);
        if state.consecutive_failures > BACKOFF_SECS.len() {
            state.blocked_until = Some(now + Duration::from_secs(LOCKOUT_SECS));
            return DirectPairingThrottle::LockedOut(Duration::from_secs(LOCKOUT_SECS));
        }
        let wait = Duration::from_secs(BACKOFF_SECS[state.consecutive_failures.saturating_sub(1)]);
        state.blocked_until = Some(now + wait);
        DirectPairingThrottle::Backoff(wait)
    }

    pub fn record_success(&mut self, ip: IpAddr) {
        self.states.remove(&ip);
    }
}

/// JSON pairing exchange. Tokens stay in the POST body only.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct DirectPairingExchange {
    pub token: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub browser_install_id: Option<String>,
}
