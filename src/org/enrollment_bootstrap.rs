use std::fmt;
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use base64::Engine;
use ed25519_compact::{KeyPair, Seed};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;
use zeroize::{Zeroize, Zeroizing};

use crate::org::portal::{
    validate_iso_timestamp, validate_opaque_id, PortalAdapterError, MAX_REQUEST_BYTES,
    MAX_RESPONSE_BYTES,
};

pub const ENROLLMENT_ATTESTATION_ALGORITHM: &str = "ed25519-v1";
pub const ENROLLMENT_CLAIM_DOMAIN: &str = "devmanager.host-enrollment-claim.v1";
pub const ENROLLMENT_CONFIRM_DOMAIN: &str = "devmanager.host-enrollment.v1";
const ED25519_SPKI_PREFIX: &[u8; 12] = b"\x30\x2a\x30\x05\x06\x03\x2b\x65\x70\x03\x21\x00";
const ENROLLMENT_KEY_PURPOSE: &[u8] = b"DevManagerConnect/v1/enrollment-ed25519\0";

#[derive(Debug)]
pub enum EnrollmentBootstrapError {
    Invalid(PortalAdapterError),
    Custody(&'static str),
    Transport(&'static str),
    Http { status: u16 },
    Response(&'static str),
}

impl fmt::Display for EnrollmentBootstrapError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(error) => error.fmt(formatter),
            Self::Custody(message) | Self::Transport(message) | Self::Response(message) => {
                formatter.write_str(message)
            }
            Self::Http { status } => write!(formatter, "Portal enrollment HTTP {status}"),
        }
    }
}

impl std::error::Error for EnrollmentBootstrapError {}

impl From<PortalAdapterError> for EnrollmentBootstrapError {
    fn from(error: PortalAdapterError) -> Self {
        Self::Invalid(error)
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EnrollmentBootstrap {
    pub challenge_id: String,
    pub handle: String,
    pub nonce: String,
    pub expires_at: String,
    pub tenant_id: String,
    pub host_id: String,
    pub policy_revision: u32,
}

impl fmt::Debug for EnrollmentBootstrap {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EnrollmentBootstrap")
            .field("challenge_id", &self.challenge_id)
            .field("handle", &"<redacted>")
            .field("nonce", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .field("tenant_id", &self.tenant_id)
            .field("host_id", &self.host_id)
            .field("policy_revision", &self.policy_revision)
            .finish()
    }
}

impl EnrollmentBootstrap {
    fn validate(&self) -> Result<(), PortalAdapterError> {
        validate_opaque_id(&self.challenge_id, "challengeId")?;
        validate_opaque_id(&self.handle, "handle")?;
        validate_opaque_id(&self.nonce, "nonce")?;
        validate_opaque_id(&self.tenant_id, "tenantId")?;
        validate_opaque_id(&self.host_id, "hostId")?;
        validate_iso_timestamp(&self.expires_at, "expiresAt")?;
        if self.expires_at.len() != 24
            || !self.expires_at.ends_with('Z')
            || self.expires_at.as_bytes().get(19) != Some(&b'.')
        {
            return Err(PortalAdapterError::InvalidTimestamp {
                field: "expiresAt".into(),
                value: "<redacted>".into(),
            });
        }
        if self.policy_revision == 0 {
            return Err(PortalAdapterError::InvalidValue {
                field: "policyRevision".into(),
                reason: "must be positive".into(),
            });
        }
        Ok(())
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EnrollmentKeyRecord {
    pub key_handle: String,
    pub fingerprint: String,
}

impl EnrollmentKeyRecord {
    fn validate(&self) -> Result<(), EnrollmentBootstrapError> {
        validate_opaque_id(&self.key_handle, "deviceKeyId")?;
        validate_fingerprint(&self.fingerprint)?;
        Ok(())
    }
}

impl fmt::Debug for EnrollmentKeyRecord {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EnrollmentKeyRecord")
            .field("key_handle", &self.key_handle)
            .field("fingerprint", &self.fingerprint)
            .finish()
    }
}

fn validate_fingerprint(value: &str) -> Result<(), PortalAdapterError> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(PortalAdapterError::InvalidValue {
            field: "devicePublicKeyFingerprint".into(),
            reason: "must be lowercase SHA-256 hex".into(),
        });
    }
    if value.bytes().any(|byte| byte.is_ascii_uppercase()) {
        return Err(PortalAdapterError::InvalidValue {
            field: "devicePublicKeyFingerprint".into(),
            reason: "must be lowercase SHA-256 hex".into(),
        });
    }
    Ok(())
}

pub fn canonical_claim_message(
    bootstrap: &EnrollmentBootstrap,
    device_public_id: &str,
    device_key_id: &str,
    device_public_key_spki: &str,
    device_public_key_fingerprint: &str,
) -> Result<Vec<u8>, PortalAdapterError> {
    bootstrap.validate()?;
    validate_opaque_id(device_public_id, "devicePublicId")?;
    validate_opaque_id(device_key_id, "deviceKeyId")?;
    if device_public_key_spki.trim().is_empty() {
        return Err(PortalAdapterError::InvalidValue {
            field: "devicePublicKeySpki".into(),
            reason: "must be non-empty".into(),
        });
    }
    validate_fingerprint(device_public_key_fingerprint)?;
    Ok([
        ENROLLMENT_CLAIM_DOMAIN.to_string(),
        format!("tenantId:{}", bootstrap.tenant_id),
        format!("challengeId:{}", bootstrap.challenge_id),
        format!("hostId:{}", bootstrap.host_id),
        format!("devicePublicId:{device_public_id}"),
        format!("deviceKeyId:{device_key_id}"),
        format!("publicKeyAlgorithm:{ENROLLMENT_ATTESTATION_ALGORITHM}"),
        format!("devicePublicKeySpki:{device_public_key_spki}"),
        format!("devicePublicKeyFingerprint:{device_public_key_fingerprint}"),
        format!("policyRevision:{}", bootstrap.policy_revision),
        format!("nonce:{}", bootstrap.nonce),
        format!("expiresAt:{}", bootstrap.expires_at),
    ]
    .join("\n")
    .into_bytes())
}

pub fn canonical_confirm_message(
    bootstrap: &EnrollmentBootstrap,
    device_public_id: &str,
    device_key_id: &str,
    confirmation_nonce: &str,
) -> Result<Vec<u8>, PortalAdapterError> {
    bootstrap.validate()?;
    validate_opaque_id(device_public_id, "devicePublicId")?;
    validate_opaque_id(device_key_id, "deviceKeyId")?;
    validate_opaque_id(confirmation_nonce, "nonce")?;
    Ok([
        ENROLLMENT_CONFIRM_DOMAIN.to_string(),
        format!("tenantId:{}", bootstrap.tenant_id),
        format!("challengeId:{}", bootstrap.challenge_id),
        format!("hostId:{}", bootstrap.host_id),
        format!("devicePublicId:{device_public_id}"),
        format!("deviceKeyId:{device_key_id}"),
        format!("policyRevision:{}", bootstrap.policy_revision),
        format!("nonce:{confirmation_nonce}"),
    ]
    .join("\n")
    .into_bytes())
}

pub fn ed25519_spki(public_key: &[u8]) -> Vec<u8> {
    let mut spki = Vec::with_capacity(ED25519_SPKI_PREFIX.len() + public_key.len());
    spki.extend_from_slice(ED25519_SPKI_PREFIX);
    spki.extend_from_slice(public_key);
    spki
}

#[derive(Clone, PartialEq, Eq)]
pub struct EnrollmentPublicKey {
    pub key_handle: String,
    pub spki_base64: String,
    pub fingerprint: String,
}

impl fmt::Debug for EnrollmentPublicKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EnrollmentPublicKey")
            .field("key_handle", &self.key_handle)
            .field("fingerprint", &self.fingerprint)
            .finish()
    }
}

impl EnrollmentPublicKey {
    pub fn from_spki(key_handle: String, spki: Vec<u8>) -> Result<Self, EnrollmentBootstrapError> {
        validate_opaque_id(&key_handle, "deviceKeyId")?;
        if spki.len() != 44 || !spki.starts_with(ED25519_SPKI_PREFIX) {
            return Err(EnrollmentBootstrapError::Custody(
                "enrollment key is not canonical Ed25519 SPKI",
            ));
        }
        let fingerprint = hex_sha256(&spki);
        Ok(Self {
            key_handle,
            spki_base64: base64::engine::general_purpose::STANDARD.encode(spki),
            fingerprint,
        })
    }

    fn record(&self) -> EnrollmentKeyRecord {
        EnrollmentKeyRecord {
            key_handle: self.key_handle.clone(),
            fingerprint: self.fingerprint.clone(),
        }
    }
}

pub trait EnrollmentKeyCustody {
    fn load_or_create(
        &self,
        previous: Option<&EnrollmentKeyRecord>,
    ) -> Result<EnrollmentPublicKey, EnrollmentBootstrapError>;

    fn sign(&self, key_handle: &str, message: &[u8]) -> Result<Vec<u8>, EnrollmentBootstrapError>;
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EnrollmentClaimRequest {
    pub challenge_id: String,
    pub host_id: String,
    pub device_public_id: String,
    pub device_key_id: String,
    pub attestation_algorithm: String,
    pub device_public_key_spki: String,
    pub device_public_key_fingerprint: String,
    pub attestation: String,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EnrollmentClaimResponse {
    pub nonce: String,
    pub expires_at: String,
    pub host_id: String,
    pub tenant_id: String,
    pub policy_revision: u32,
    pub device_public_id: String,
    pub device_key_id: String,
    pub device_public_key_fingerprint: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EnrollmentConfirmRequest {
    pub challenge_id: String,
    pub device_public_id: String,
    pub device_key_id: String,
    pub attestation_algorithm: String,
    pub attestation: String,
    pub nonce: String,
    pub signed_policy_revision: u32,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnrollmentConfirmedHost {
    pub host_id: String,
    #[serde(alias = "organizationId")]
    pub tenant_id: String,
    pub device_public_id: String,
    pub status: String,
    pub policy_revision: u32,
    pub signed_policy_revision: Option<u32>,
    pub host_confirmed_at: Option<String>,
}

pub trait EnrollmentTransport {
    fn claim(
        &self,
        handle: &str,
        request: &EnrollmentClaimRequest,
    ) -> Result<EnrollmentClaimResponse, EnrollmentBootstrapError>;

    fn confirm(
        &self,
        host_id: &str,
        request: &EnrollmentConfirmRequest,
    ) -> Result<EnrollmentConfirmedHost, EnrollmentBootstrapError>;
}

#[derive(Clone, PartialEq, Eq)]
pub struct EnrollmentConfirmation {
    host_id: String,
    tenant_id: String,
    device_public_id: String,
    device_key_id: String,
    policy_revision: u32,
    host_confirmed_at: String,
}

impl fmt::Debug for EnrollmentConfirmation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EnrollmentConfirmation")
            .field("host_id", &self.host_id)
            .field("tenant_id", &self.tenant_id)
            .field("device_public_id", &self.device_public_id)
            .field("device_key_id", &self.device_key_id)
            .field("policy_revision", &self.policy_revision)
            .field("host_confirmed_at", &self.host_confirmed_at)
            .finish()
    }
}

impl EnrollmentConfirmation {
    pub fn host_id(&self) -> &str {
        &self.host_id
    }

    pub fn tenant_id(&self) -> &str {
        &self.tenant_id
    }

    pub fn device_public_id(&self) -> &str {
        &self.device_public_id
    }

    pub fn policy_revision(&self) -> u32 {
        self.policy_revision
    }

    #[cfg(test)]
    pub(crate) fn fixture(
        host_id: impl Into<String>,
        tenant_id: impl Into<String>,
        device_public_id: impl Into<String>,
        policy_revision: u32,
    ) -> Self {
        Self {
            host_id: host_id.into(),
            tenant_id: tenant_id.into(),
            device_public_id: device_public_id.into(),
            device_key_id: "test-enrollment-key".into(),
            policy_revision,
            host_confirmed_at: "2026-08-13T23:40:01.123Z".into(),
        }
    }
}

pub struct EnrollmentOutcome {
    pub key_record: EnrollmentKeyRecord,
    pub confirmation: EnrollmentConfirmation,
}

pub fn claim_and_confirm<T: EnrollmentTransport, K: EnrollmentKeyCustody>(
    transport: &T,
    custody: &K,
    bootstrap: &EnrollmentBootstrap,
    device_public_id: &str,
    previous_key: Option<&EnrollmentKeyRecord>,
) -> Result<EnrollmentOutcome, EnrollmentBootstrapError> {
    bootstrap.validate()?;
    validate_opaque_id(device_public_id, "devicePublicId")?;
    if let Some(previous) = previous_key {
        previous.validate()?;
    }
    let public_key = custody.load_or_create(previous_key)?;
    validate_fingerprint(&public_key.fingerprint)?;
    if let Some(previous) = previous_key {
        if previous.key_handle != public_key.key_handle
            || previous.fingerprint != public_key.fingerprint
        {
            return Err(EnrollmentBootstrapError::Custody(
                "enrollment key does not match persisted handle and fingerprint",
            ));
        }
    }
    let claim_message = canonical_claim_message(
        bootstrap,
        device_public_id,
        &public_key.key_handle,
        &public_key.spki_base64,
        &public_key.fingerprint,
    )?;
    let claim_signature = custody.sign(&public_key.key_handle, &claim_message)?;
    if claim_signature.len() != 64 {
        return Err(EnrollmentBootstrapError::Custody(
            "enrollment signer returned an invalid Ed25519 signature",
        ));
    }
    let claim = transport.claim(
        &bootstrap.handle,
        &EnrollmentClaimRequest {
            challenge_id: bootstrap.challenge_id.clone(),
            host_id: bootstrap.host_id.clone(),
            device_public_id: device_public_id.to_string(),
            device_key_id: public_key.key_handle.clone(),
            attestation_algorithm: ENROLLMENT_ATTESTATION_ALGORITHM.into(),
            device_public_key_spki: public_key.spki_base64.clone(),
            device_public_key_fingerprint: public_key.fingerprint.clone(),
            attestation: base64::engine::general_purpose::STANDARD.encode(claim_signature),
        },
    )?;
    validate_claim_response(bootstrap, device_public_id, &public_key, &claim)?;

    let confirm_message = canonical_confirm_message(
        bootstrap,
        device_public_id,
        &public_key.key_handle,
        &claim.nonce,
    )?;
    let confirm_signature = custody.sign(&public_key.key_handle, &confirm_message)?;
    if confirm_signature.len() != 64 {
        return Err(EnrollmentBootstrapError::Custody(
            "enrollment signer returned an invalid Ed25519 signature",
        ));
    }
    let confirmed = transport.confirm(
        &bootstrap.host_id,
        &EnrollmentConfirmRequest {
            challenge_id: bootstrap.challenge_id.clone(),
            device_public_id: device_public_id.to_string(),
            device_key_id: public_key.key_handle.clone(),
            attestation_algorithm: ENROLLMENT_ATTESTATION_ALGORITHM.into(),
            attestation: base64::engine::general_purpose::STANDARD.encode(confirm_signature),
            nonce: claim.nonce,
            signed_policy_revision: bootstrap.policy_revision,
        },
    )?;
    let confirmation = validate_confirmed_host(
        bootstrap,
        device_public_id,
        &public_key.key_handle,
        confirmed,
    )?;
    Ok(EnrollmentOutcome {
        key_record: public_key.record(),
        confirmation,
    })
}

fn validate_claim_response(
    bootstrap: &EnrollmentBootstrap,
    device_public_id: &str,
    public_key: &EnrollmentPublicKey,
    claim: &EnrollmentClaimResponse,
) -> Result<(), EnrollmentBootstrapError> {
    validate_opaque_id(&claim.nonce, "nonce")?;
    validate_iso_timestamp(&claim.expires_at, "expiresAt")?;
    if claim.host_id != bootstrap.host_id
        || claim.tenant_id != bootstrap.tenant_id
        || claim.expires_at != bootstrap.expires_at
        || claim.policy_revision != bootstrap.policy_revision
        || claim.device_public_id != device_public_id
        || claim.device_key_id != public_key.key_handle
        || claim.device_public_key_fingerprint != public_key.fingerprint
    {
        return Err(EnrollmentBootstrapError::Response(
            "Portal enrollment claim correlation failed",
        ));
    }
    Ok(())
}

fn validate_confirmed_host(
    bootstrap: &EnrollmentBootstrap,
    device_public_id: &str,
    device_key_id: &str,
    host: EnrollmentConfirmedHost,
) -> Result<EnrollmentConfirmation, EnrollmentBootstrapError> {
    let host_confirmed_at = host
        .host_confirmed_at
        .ok_or(EnrollmentBootstrapError::Response(
            "Portal enrollment confirmation is incomplete",
        ))?;
    validate_iso_timestamp(&host_confirmed_at, "hostConfirmedAt")?;
    if host.host_id != bootstrap.host_id
        || host.tenant_id != bootstrap.tenant_id
        || host.device_public_id != device_public_id
        || host.status != "enrolled"
        || host.policy_revision != bootstrap.policy_revision
        || host.signed_policy_revision != Some(bootstrap.policy_revision)
    {
        return Err(EnrollmentBootstrapError::Response(
            "Portal enrollment confirmation correlation failed",
        ));
    }
    Ok(EnrollmentConfirmation {
        host_id: host.host_id,
        tenant_id: host.tenant_id,
        device_public_id: host.device_public_id,
        device_key_id: device_key_id.to_string(),
        policy_revision: host.policy_revision,
        host_confirmed_at,
    })
}

#[derive(Deserialize)]
struct PortalEnvelope<T> {
    data: T,
}

pub struct PortalEnrollmentClient {
    endpoint: Url,
    token: String,
    agent: ureq::Agent,
}

impl fmt::Debug for PortalEnrollmentClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PortalEnrollmentClient")
            .field("endpoint", &self.endpoint)
            .field("token", &"<redacted>")
            .finish()
    }
}

impl PortalEnrollmentClient {
    pub fn new(
        base_url: &str,
        bearer_token: impl Into<String>,
    ) -> Result<Self, EnrollmentBootstrapError> {
        let mut endpoint = Url::parse(base_url)
            .map_err(|_| PortalAdapterError::InvalidBaseUrl("<redacted>".into()))?;
        if endpoint.query().is_some()
            || endpoint.fragment().is_some()
            || endpoint.host_str().is_none()
        {
            return Err(PortalAdapterError::InvalidBaseUrl("<redacted>".into()).into());
        }
        let path = endpoint.path().trim_end_matches('/');
        let path = if path.ends_with("/api/devmanager") {
            path.to_string()
        } else if path.ends_with("/api") {
            format!("{path}/devmanager")
        } else {
            format!("{path}/api/devmanager")
        };
        endpoint.set_path(&path);
        let token = bearer_token.into();
        if token.trim().is_empty() {
            return Err(PortalAdapterError::InvalidValue {
                field: "bearerToken".into(),
                reason: "must be non-empty".into(),
            }
            .into());
        }
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(20)))
            .max_redirects(0)
            .proxy(None)
            .build()
            .into();
        Ok(Self {
            endpoint,
            token,
            agent,
        })
    }

    fn post<T: DeserializeOwned, B: Serialize>(
        &self,
        segments: &[&str],
        body: &B,
    ) -> Result<T, EnrollmentBootstrapError> {
        let mut url = self.endpoint.clone();
        {
            let invalid = url.to_string();
            let mut path = url
                .path_segments_mut()
                .map_err(|_| PortalAdapterError::InvalidBaseUrl(invalid))?;
            path.pop_if_empty();
            path.extend(segments.iter().copied());
        }
        let encoded = serde_json::to_vec(body)
            .map_err(|_| EnrollmentBootstrapError::Transport("Portal enrollment request failed"))?;
        if encoded.len() > MAX_REQUEST_BYTES {
            return Err(PortalAdapterError::RequestTooLarge {
                bytes: encoded.len(),
            }
            .into());
        }
        let result = self
            .agent
            .post(url.as_str())
            .header("Accept", "application/json")
            .header("Authorization", &format!("Bearer {}", self.token))
            .header("Content-Type", "application/json")
            .send(encoded)
            .map_err(|_| EnrollmentBootstrapError::Transport("Portal enrollment request failed"))?;
        let status = result.status().as_u16();
        let mut response = result;
        let bytes = response
            .body_mut()
            .with_config()
            .limit(MAX_RESPONSE_BYTES as u64)
            .read_to_vec()
            .map_err(|_| {
                EnrollmentBootstrapError::Transport("Portal enrollment response failed")
            })?;
        if !(200..300).contains(&status) {
            return Err(EnrollmentBootstrapError::Http { status });
        }
        serde_json::from_slice(&bytes).map_err(|_| {
            EnrollmentBootstrapError::Response("Portal enrollment response is invalid")
        })
    }
}

impl EnrollmentTransport for PortalEnrollmentClient {
    fn claim(
        &self,
        handle: &str,
        request: &EnrollmentClaimRequest,
    ) -> Result<EnrollmentClaimResponse, EnrollmentBootstrapError> {
        validate_opaque_id(handle, "handle")?;
        self.post(
            &["hosts", "enrollment-challenges", handle, "claim"],
            request,
        )
    }

    fn confirm(
        &self,
        host_id: &str,
        request: &EnrollmentConfirmRequest,
    ) -> Result<EnrollmentConfirmedHost, EnrollmentBootstrapError> {
        validate_opaque_id(host_id, "hostId")?;
        self.post::<PortalEnvelope<EnrollmentConfirmedHost>, _>(
            &["hosts", host_id, "confirm"],
            request,
        )
        .map(|envelope| envelope.data)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SealedEnrollmentKey {
    version: u8,
    public_key: String,
    protected_seed: String,
}

pub struct OsEnrollmentKeyCustody {
    root: PathBuf,
}

impl fmt::Debug for OsEnrollmentKeyCustody {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("OsEnrollmentKeyCustody(redacted)")
    }
}

impl OsEnrollmentKeyCustody {
    pub fn open_active_profile() -> Result<Self, EnrollmentBootstrapError> {
        let mut root = crate::persistence::app_config_dir()
            .map_err(|_| EnrollmentBootstrapError::Custody("enrollment custody unavailable"))?;
        root.push("connect");
        root.push("enrollment-keys");
        Ok(Self { root })
    }

    #[cfg(test)]
    fn for_test(root: PathBuf) -> Self {
        Self { root }
    }

    fn path_for(&self, handle: &str) -> PathBuf {
        self.root
            .join(format!("{}.dpapi", hex_sha256(handle.as_bytes())))
    }

    fn load_key(&self, handle: &str) -> Result<(KeyPair, Vec<u8>), EnrollmentBootstrapError> {
        validate_opaque_id(handle, "deviceKeyId")?;
        let bytes = fs::read(self.path_for(handle))
            .map_err(|_| EnrollmentBootstrapError::Custody("enrollment key is unavailable"))?;
        let sealed: SealedEnrollmentKey = serde_json::from_slice(&bytes)
            .map_err(|_| EnrollmentBootstrapError::Custody("enrollment key is invalid"))?;
        if sealed.version != 1 {
            return Err(EnrollmentBootstrapError::Custody(
                "enrollment key is invalid",
            ));
        }
        let public = base64::engine::general_purpose::STANDARD
            .decode(sealed.public_key)
            .map_err(|_| EnrollmentBootstrapError::Custody("enrollment key is invalid"))?;
        if public.len() != 32 {
            return Err(EnrollmentBootstrapError::Custody(
                "enrollment key is invalid",
            ));
        }
        let protected = base64::engine::general_purpose::STANDARD
            .decode(sealed.protected_seed)
            .map_err(|_| EnrollmentBootstrapError::Custody("enrollment key is invalid"))?;
        let entropy = enrollment_key_entropy(handle, &public);
        let mut seed_bytes = unprotect_enrollment_seed(&protected, &entropy)?;
        if seed_bytes.len() != 32 {
            seed_bytes.zeroize();
            return Err(EnrollmentBootstrapError::Custody(
                "enrollment key is invalid",
            ));
        }
        let seed = Seed::from_slice(&seed_bytes)
            .map_err(|_| EnrollmentBootstrapError::Custody("enrollment key is invalid"))?;
        seed_bytes.zeroize();
        let key_pair = KeyPair::from_seed(seed);
        if key_pair.pk.as_ref() != public.as_slice() {
            return Err(EnrollmentBootstrapError::Custody(
                "enrollment key public identity mismatch",
            ));
        }
        Ok((key_pair, public))
    }

    fn create_key(&self) -> Result<EnrollmentPublicKey, EnrollmentBootstrapError> {
        let mut seed_bytes = Zeroizing::new([0_u8; 32]);
        getrandom::fill(seed_bytes.as_mut())
            .map_err(|_| EnrollmentBootstrapError::Custody("enrollment key entropy unavailable"))?;
        let seed = Seed::new(*seed_bytes);
        let key_pair = KeyPair::from_seed(seed);
        let public = key_pair.pk.as_ref().to_vec();
        let mut handle_bytes = [0_u8; 24];
        getrandom::fill(&mut handle_bytes)
            .map_err(|_| EnrollmentBootstrapError::Custody("enrollment key entropy unavailable"))?;
        let handle = format!(
            "enrollment-key:{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(handle_bytes)
        );
        handle_bytes.zeroize();
        let entropy = enrollment_key_entropy(&handle, &public);
        let protected = protect_enrollment_seed(seed_bytes.as_ref(), &entropy)?;
        seed_bytes.zeroize();
        let sealed = SealedEnrollmentKey {
            version: 1,
            public_key: base64::engine::general_purpose::STANDARD.encode(&public),
            protected_seed: base64::engine::general_purpose::STANDARD.encode(protected),
        };
        let encoded = serde_json::to_vec(&sealed).map_err(|_| {
            EnrollmentBootstrapError::Custody("enrollment key could not be persisted")
        })?;
        fs::create_dir_all(&self.root).map_err(|_| {
            EnrollmentBootstrapError::Custody("enrollment key could not be persisted")
        })?;
        let path = self.path_for(&handle);
        let temp = path.with_extension(format!("{}.tmp", uuid::Uuid::now_v7().as_simple()));
        fs::write(&temp, encoded).map_err(|_| {
            EnrollmentBootstrapError::Custody("enrollment key could not be persisted")
        })?;
        if let Err(error) = fs::rename(&temp, &path) {
            let _ = fs::remove_file(&temp);
            let _ = error;
            return Err(EnrollmentBootstrapError::Custody(
                "enrollment key could not be persisted",
            ));
        }
        EnrollmentPublicKey::from_spki(handle, ed25519_spki(&public))
    }
}

impl EnrollmentKeyCustody for OsEnrollmentKeyCustody {
    fn load_or_create(
        &self,
        previous: Option<&EnrollmentKeyRecord>,
    ) -> Result<EnrollmentPublicKey, EnrollmentBootstrapError> {
        let Some(previous) = previous else {
            return self.create_key();
        };
        previous.validate()?;
        let (_, public) = self.load_key(&previous.key_handle)?;
        let key =
            EnrollmentPublicKey::from_spki(previous.key_handle.clone(), ed25519_spki(&public))?;
        if key.fingerprint != previous.fingerprint {
            return Err(EnrollmentBootstrapError::Custody(
                "enrollment key fingerprint mismatch",
            ));
        }
        Ok(key)
    }

    fn sign(&self, key_handle: &str, message: &[u8]) -> Result<Vec<u8>, EnrollmentBootstrapError> {
        let (key_pair, _) = self.load_key(key_handle)?;
        Ok(key_pair.sk.sign(message, None).as_ref().to_vec())
    }
}

fn enrollment_key_entropy(handle: &str, public: &[u8]) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(ENROLLMENT_KEY_PURPOSE);
    digest.update((handle.len() as u64).to_be_bytes());
    digest.update(handle.as_bytes());
    digest.update(public);
    digest.finalize().into()
}

fn hex_sha256(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(64);
    for byte in digest {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

#[cfg(windows)]
fn protect_enrollment_seed(
    plaintext: &[u8],
    entropy: &[u8],
) -> Result<Vec<u8>, EnrollmentBootstrapError> {
    dpapi_protect(plaintext, entropy)
}

#[cfg(windows)]
fn unprotect_enrollment_seed(
    blob: &[u8],
    entropy: &[u8],
) -> Result<Zeroizing<Vec<u8>>, EnrollmentBootstrapError> {
    dpapi_unprotect(blob, entropy)
}

#[cfg(not(windows))]
fn protect_enrollment_seed(
    _plaintext: &[u8],
    _entropy: &[u8],
) -> Result<Vec<u8>, EnrollmentBootstrapError> {
    Err(EnrollmentBootstrapError::Custody(
        "enrollment key custody is unsupported on this platform",
    ))
}

#[cfg(not(windows))]
fn unprotect_enrollment_seed(
    _blob: &[u8],
    _entropy: &[u8],
) -> Result<Zeroizing<Vec<u8>>, EnrollmentBootstrapError> {
    Err(EnrollmentBootstrapError::Custody(
        "enrollment key custody is unsupported on this platform",
    ))
}

#[cfg(windows)]
fn dpapi_protect(plaintext: &[u8], entropy: &[u8]) -> Result<Vec<u8>, EnrollmentBootstrapError> {
    use windows::core::w;
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Cryptography::{
        CryptProtectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };
    if plaintext.len() != 32 || entropy.len() != 32 {
        return Err(EnrollmentBootstrapError::Custody(
            "enrollment key is invalid",
        ));
    }
    let input = CRYPT_INTEGER_BLOB {
        cbData: plaintext.len() as u32,
        pbData: plaintext.as_ptr() as *mut u8,
    };
    let entropy_blob = CRYPT_INTEGER_BLOB {
        cbData: entropy.len() as u32,
        pbData: entropy.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    unsafe {
        CryptProtectData(
            &input,
            w!("DevManagerConnectEnrollmentEd25519V1"),
            Some(&entropy_blob as *const CRYPT_INTEGER_BLOB),
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
        .map_err(|_| EnrollmentBootstrapError::Custody("enrollment key protection failed"))?;
        if output.pbData.is_null() || output.cbData == 0 {
            return Err(EnrollmentBootstrapError::Custody(
                "enrollment key protection failed",
            ));
        }
        let copy = std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
        let _ = LocalFree(Some(HLOCAL(output.pbData.cast())));
        Ok(copy)
    }
}

#[cfg(windows)]
fn dpapi_unprotect(
    blob: &[u8],
    entropy: &[u8],
) -> Result<Zeroizing<Vec<u8>>, EnrollmentBootstrapError> {
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Cryptography::{
        CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };
    if blob.is_empty() || blob.len() > 8 * 1024 || entropy.len() != 32 {
        return Err(EnrollmentBootstrapError::Custody(
            "enrollment key is invalid",
        ));
    }
    let mut input = CRYPT_INTEGER_BLOB {
        cbData: blob.len() as u32,
        pbData: blob.as_ptr() as *mut u8,
    };
    let entropy_blob = CRYPT_INTEGER_BLOB {
        cbData: entropy.len() as u32,
        pbData: entropy.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    unsafe {
        CryptUnprotectData(
            &mut input,
            None,
            Some(&entropy_blob as *const CRYPT_INTEGER_BLOB),
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
        .map_err(|_| EnrollmentBootstrapError::Custody("enrollment key unprotect failed"))?;
        if output.pbData.is_null() || output.cbData == 0 {
            return Err(EnrollmentBootstrapError::Custody(
                "enrollment key unprotect failed",
            ));
        }
        let copy = Zeroizing::new(
            std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec(),
        );
        let _ = LocalFree(Some(HLOCAL(output.pbData.cast())));
        Ok(copy)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use ed25519_compact::{KeyPair, Noise, Seed, Signature};
    use std::cell::RefCell;

    fn bootstrap() -> EnrollmentBootstrap {
        EnrollmentBootstrap {
            challenge_id: "challenge-1".into(),
            handle: "bootstrap-handle".into(),
            nonce: "challenge-nonce-1234567890".into(),
            expires_at: "2026-08-13T23:45:01.123Z".into(),
            tenant_id: "tenant-1".into(),
            host_id: "host-1".into(),
            policy_revision: 7,
        }
    }

    #[test]
    fn canonical_claim_binds_every_server_field_in_v1_order() {
        let message = canonical_claim_message(
            &bootstrap(),
            "device-1",
            "key-handle-1",
            "MCowBQYDK2VwAyEAERERERERERERERERERERERERERERERERERERERERERE=",
            "abcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcd",
        )
        .expect("canonical claim");

        assert_eq!(
            String::from_utf8(message).expect("utf8"),
            concat!(
                "devmanager.host-enrollment-claim.v1\n",
                "tenantId:tenant-1\n",
                "challengeId:challenge-1\n",
                "hostId:host-1\n",
                "devicePublicId:device-1\n",
                "deviceKeyId:key-handle-1\n",
                "publicKeyAlgorithm:ed25519-v1\n",
                "devicePublicKeySpki:MCowBQYDK2VwAyEAERERERERERERERERERERERERERERERERERERERERERE=\n",
                "devicePublicKeyFingerprint:abcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcd\n",
                "policyRevision:7\n",
                "nonce:challenge-nonce-1234567890\n",
                "expiresAt:2026-08-13T23:45:01.123Z"
            )
        );
    }

    #[test]
    fn restart_record_contains_only_opaque_handle_and_fingerprint() {
        let record = EnrollmentKeyRecord {
            key_handle: "key-handle-1".into(),
            fingerprint: "abcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcd".into(),
        };
        let encoded = serde_json::to_value(&record).expect("record");
        assert_eq!(
            encoded,
            serde_json::json!({
                "keyHandle": "key-handle-1",
                "fingerprint": "abcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcd"
            })
        );
        assert!(!encoded.to_string().to_ascii_lowercase().contains("private"));
        assert!(!encoded.to_string().to_ascii_lowercase().contains("seed"));
    }

    struct MemoryCustody {
        key_pair: KeyPair,
        loaded: RefCell<Vec<Option<EnrollmentKeyRecord>>>,
    }

    impl MemoryCustody {
        fn new() -> Self {
            Self {
                key_pair: KeyPair::from_seed(Seed::new([7; 32])),
                loaded: RefCell::new(Vec::new()),
            }
        }
    }

    impl EnrollmentKeyCustody for MemoryCustody {
        fn load_or_create(
            &self,
            previous: Option<&EnrollmentKeyRecord>,
        ) -> Result<EnrollmentPublicKey, EnrollmentBootstrapError> {
            self.loaded.borrow_mut().push(previous.cloned());
            let spki = ed25519_spki(self.key_pair.pk.as_ref());
            Ok(EnrollmentPublicKey::from_spki("key-handle-1".into(), spki)?)
        }

        fn sign(
            &self,
            key_handle: &str,
            message: &[u8],
        ) -> Result<Vec<u8>, EnrollmentBootstrapError> {
            assert_eq!(key_handle, "key-handle-1");
            Ok(self
                .key_pair
                .sk
                .sign(message, Some(Noise::new([3; 16])))
                .as_ref()
                .to_vec())
        }
    }

    #[derive(Default)]
    struct FakeTransport {
        calls: RefCell<Vec<String>>,
    }

    impl EnrollmentTransport for FakeTransport {
        fn claim(
            &self,
            handle: &str,
            request: &EnrollmentClaimRequest,
        ) -> Result<EnrollmentClaimResponse, EnrollmentBootstrapError> {
            self.calls.borrow_mut().push(format!("claim:{handle}"));
            let signature = base64::engine::general_purpose::STANDARD
                .decode(&request.attestation)
                .expect("signature");
            let signature = Signature::from_slice(&signature).expect("ed25519 signature");
            let public = base64::engine::general_purpose::STANDARD
                .decode(&request.device_public_key_spki)
                .expect("spki");
            let public = ed25519_compact::PublicKey::from_slice(&public[12..]).expect("public");
            public
                .verify(
                    canonical_claim_message(
                        &bootstrap(),
                        &request.device_public_id,
                        &request.device_key_id,
                        &request.device_public_key_spki,
                        &request.device_public_key_fingerprint,
                    )
                    .expect("claim message"),
                    &signature,
                )
                .expect("claim proof");
            Ok(EnrollmentClaimResponse {
                nonce: "confirmation-nonce-1234567890".into(),
                expires_at: "2026-08-13T23:45:01.123Z".into(),
                host_id: "host-1".into(),
                tenant_id: "tenant-1".into(),
                policy_revision: 7,
                device_public_id: "device-1".into(),
                device_key_id: "key-handle-1".into(),
                device_public_key_fingerprint: request.device_public_key_fingerprint.clone(),
            })
        }

        fn confirm(
            &self,
            host_id: &str,
            request: &EnrollmentConfirmRequest,
        ) -> Result<EnrollmentConfirmedHost, EnrollmentBootstrapError> {
            self.calls.borrow_mut().push(format!("confirm:{host_id}"));
            assert_eq!(request.challenge_id, "challenge-1");
            assert_eq!(request.nonce, "confirmation-nonce-1234567890");
            assert_eq!(request.signed_policy_revision, 7);
            Ok(EnrollmentConfirmedHost {
                host_id: "host-1".into(),
                tenant_id: "tenant-1".into(),
                device_public_id: "device-1".into(),
                status: "enrolled".into(),
                policy_revision: 7,
                signed_policy_revision: Some(7),
                host_confirmed_at: Some("2026-08-13T23:40:01.123Z".into()),
            })
        }
    }

    #[test]
    fn claim_then_confirm_is_exact_and_never_calls_removed_reconcile_route() {
        let custody = MemoryCustody::new();
        let transport = FakeTransport::default();
        let outcome = claim_and_confirm(&transport, &custody, &bootstrap(), "device-1", None)
            .expect("enrollment");

        assert_eq!(
            transport.calls.borrow().as_slice(),
            ["claim:bootstrap-handle", "confirm:host-1"]
        );
        assert!(transport
            .calls
            .borrow()
            .iter()
            .all(|call| !call.contains("reconcile")));
        assert_eq!(outcome.key_record.key_handle, "key-handle-1");
        assert_eq!(outcome.confirmation.host_id(), "host-1");

        let restarted = claim_and_confirm(
            &transport,
            &custody,
            &bootstrap(),
            "device-1",
            Some(&outcome.key_record),
        )
        .expect("restart");
        assert_eq!(restarted.key_record, outcome.key_record);
        assert_eq!(
            custody.loaded.borrow().last(),
            Some(&Some(outcome.key_record))
        );
    }

    #[cfg(windows)]
    #[test]
    fn os_custody_survives_restart_without_persisting_a_private_key() {
        let root = std::env::temp_dir().join(format!(
            "devmanager-enrollment-custody-{}",
            uuid::Uuid::now_v7().as_simple()
        ));
        let first = OsEnrollmentKeyCustody::for_test(root.clone());
        let public = first.load_or_create(None).expect("create protected key");
        let record = public.record();
        let first_signature = first
            .sign(&record.key_handle, b"restart-proof")
            .expect("sign");

        let restarted = OsEnrollmentKeyCustody::for_test(root.clone());
        let loaded = restarted
            .load_or_create(Some(&record))
            .expect("load protected key after restart");
        let second_signature = restarted
            .sign(&record.key_handle, b"restart-proof")
            .expect("sign after restart");

        assert_eq!(loaded.fingerprint, record.fingerprint);
        assert_eq!(first_signature, second_signature);
        let durable = serde_json::to_string(&record).expect("durable record");
        assert!(!durable.contains("protectedSeed"));
        assert!(!durable.contains("private"));
        let _ = fs::remove_dir_all(root);
    }
}
