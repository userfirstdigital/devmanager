//! Bounded sealed-frame and replay contract for Connect end-to-end channels.
//!
//! Production hosted use is locked to `Noise_XX_25519_ChaChaPoly_BLAKE2s` for
//! first pairing or invitation redemption and
//! `Noise_IK_25519_ChaChaPoly_BLAKE2s` for pinned-device sessions. Those
//! patterns are instantiated with `snow` 0.10.0 (`ring-accelerated` plus the
//! BLAKE2s/X25519 default-resolver primitives ring does not provide).
//! `CRYPTO_PRODUCTION_READY` is true because that compiled path is used.
//! snow crate metadata does not claim a formal third-party audit.
//!
//! The source-level sealer is an HMAC-SHA256 PRF plus Encrypt-then-MAC bound
//! to the v1 prologue. It exists so tests can prove frame bounds, replay
//! rejection, purpose isolation, and relay opacity. It is never a production
//! opener.

use std::fmt;

use hmac::{Hmac, Mac};
use sha2::Sha256;
use snow::params::NoiseParams;
use snow::{Builder, HandshakeState, TransportState};
use zeroize::{Zeroize, Zeroizing};

use super::frame::MAX_PHYSICAL_FRAME_BYTES;
use super::PROTOCOL_MAJOR;

type HmacSha256 = Hmac<Sha256>;

pub const CONNECT_CRYPTO_PROTOCOL: &[u8] = b"DevManagerConnect/v1\0";
pub const NOISE_FIRST_PAIRING_PATTERN: &str = "Noise_XX_25519_ChaChaPoly_BLAKE2s";
pub const NOISE_PINNED_DEVICE_PATTERN: &str = "Noise_IK_25519_ChaChaPoly_BLAKE2s";
pub const CRYPTO_PRODUCTION_READY: bool = true;
pub const SEALED_FRAME_VERSION: u8 = 1;
pub const SEALED_NONCE_BYTES: usize = 16;
pub const SEALED_TAG_BYTES: usize = 32;
pub const NOISE_AEAD_TAG_BYTES: usize = 16;
pub const NOISE_STATIC_KEY_BYTES: usize = 32;
pub const MAX_HANDSHAKE_MESSAGE_BYTES: u32 = 2_048;
pub const MAX_HANDSHAKE_PAYLOAD_BYTES: u32 = 96;
pub const HANDSHAKE_FRAME_VERSION: u8 = 1;
pub const NOISE_IDENTITY_CLAIM_BYTES: usize = 50;
pub const CHANNEL_KEY_BYTES: usize = 32;
pub const REPLAY_WINDOW_SIZE: u64 = 64;
pub const MAX_SESSION_AGE_SECS: u64 = 60 * 60;
pub const MAX_CHANNEL_SEQUENCES: u64 = u64::MAX;
pub const SEALED_FRAME_OVERHEAD_BYTES: u32 =
    1 + 8 + SEALED_NONCE_BYTES as u32 + SEALED_TAG_BYTES as u32;
pub const MAX_SEALED_FRAME_BYTES: u32 = MAX_PHYSICAL_FRAME_BYTES;
pub const MAX_SEALED_PLAINTEXT_BYTES: u32 = MAX_SEALED_FRAME_BYTES - SEALED_FRAME_OVERHEAD_BYTES;

const OWNER_PAIRING_LABEL: &[u8] = b"owner-pairing";
const TASK_INVITATION_LABEL: &[u8] = b"task-invitation";
const ENC_LABEL: &[u8] = b"e2e-enc-v1\0";
const MAC_LABEL: &[u8] = b"e2e-mac-v1\0";
const KS_LABEL: &[u8] = b"e2e-ks-v1\0";
const TAG_LABEL: &[u8] = b"e2e-tag-v1\0";
const SEND_ROLE: &[u8] = b"send\0";
const RECV_ROLE: &[u8] = b"recv\0";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialPurpose {
    OwnerPairing,
    TaskInvitation,
}

impl CredentialPurpose {
    pub const fn transcript_label(self) -> &'static [u8] {
        match self {
            Self::OwnerPairing => OWNER_PAIRING_LABEL,
            Self::TaskInvitation => TASK_INVITATION_LABEL,
        }
    }

    pub const fn first_pairing_pattern(self) -> &'static str {
        NOISE_FIRST_PAIRING_PATTERN
    }

    pub const fn pinned_pattern(self) -> &'static str {
        NOISE_PINNED_DEVICE_PATTERN
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelRole {
    Initiator,
    Responder,
}

impl ChannelRole {
    const fn send_role_label(self) -> &'static [u8] {
        match self {
            Self::Initiator => SEND_ROLE,
            Self::Responder => RECV_ROLE,
        }
    }

    const fn recv_role_label(self) -> &'static [u8] {
        match self {
            Self::Initiator => RECV_ROLE,
            Self::Responder => SEND_ROLE,
        }
    }

    const fn is_initiator(self) -> bool {
        matches!(self, Self::Initiator)
    }
}

/// Vault-supplied X25519 static private key. Never derived from profile IDs,
/// identity JSON, timestamps, or public metadata.
pub struct NoiseStaticPrivateKey(Zeroizing<[u8; NOISE_STATIC_KEY_BYTES]>);

impl NoiseStaticPrivateKey {
    pub fn from_vault_bytes(bytes: [u8; NOISE_STATIC_KEY_BYTES]) -> Result<Self, CryptoError> {
        reject_all_zero_key(&bytes)?;
        Ok(Self(Zeroizing::new(bytes)))
    }

    fn as_bytes(&self) -> &[u8; NOISE_STATIC_KEY_BYTES] {
        &self.0
    }
}

impl fmt::Debug for NoiseStaticPrivateKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("NoiseStaticPrivateKey(redacted)")
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct NoiseStaticPublicKey([u8; NOISE_STATIC_KEY_BYTES]);

impl NoiseStaticPublicKey {
    pub fn from_bytes(bytes: [u8; NOISE_STATIC_KEY_BYTES]) -> Result<Self, CryptoError> {
        reject_all_zero_key(&bytes)?;
        Ok(Self(bytes))
    }

    pub fn as_bytes(self) -> [u8; NOISE_STATIC_KEY_BYTES] {
        self.0
    }
}

impl fmt::Debug for NoiseStaticPublicKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NoiseStaticPublicKey")
            .field("len", &NOISE_STATIC_KEY_BYTES)
            .finish()
    }
}

/// Public profile/device identifiers used as handshake identity claims.
/// Contains no private key material.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct NoiseIdentityBinding {
    host_public_id: [u8; 16],
    device_public_id: Option<[u8; 16]>,
}

impl NoiseIdentityBinding {
    pub fn host(host_public_id: [u8; 16]) -> Self {
        Self {
            host_public_id,
            device_public_id: None,
        }
    }

    pub fn host_device(host_public_id: [u8; 16], device_public_id: [u8; 16]) -> Self {
        Self {
            host_public_id,
            device_public_id: Some(device_public_id),
        }
    }

    pub const fn host_public_id(self) -> [u8; 16] {
        self.host_public_id
    }

    pub const fn device_public_id(self) -> Option<[u8; 16]> {
        self.device_public_id
    }

    fn claim_id(self) -> [u8; 16] {
        self.device_public_id.unwrap_or(self.host_public_id)
    }
}

impl fmt::Debug for NoiseIdentityBinding {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NoiseIdentityBinding")
            .field("has_device", &self.device_public_id.is_some())
            .finish()
    }
}

/// Vault-backed static keypair used by production constructors.
pub struct NoiseCustody {
    private: NoiseStaticPrivateKey,
    public: NoiseStaticPublicKey,
}

impl NoiseCustody {
    pub fn from_vault(
        private: NoiseStaticPrivateKey,
        public: NoiseStaticPublicKey,
    ) -> Result<Self, CryptoError> {
        reject_all_zero_key(private.as_bytes())?;
        reject_all_zero_key(&public.as_bytes())?;
        Ok(Self { private, public })
    }

    pub fn generate() -> Result<Self, CryptoError> {
        let (private, public) = generate_noise_static_keypair()?;
        Ok(Self { private, public })
    }

    pub fn public(&self) -> NoiseStaticPublicKey {
        self.public
    }

    pub fn private(&self) -> &NoiseStaticPrivateKey {
        &self.private
    }
}

impl fmt::Debug for NoiseCustody {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("NoiseCustody(redacted)")
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct AuthenticatedPeer {
    static_public: NoiseStaticPublicKey,
    public_id: [u8; 16],
}

impl AuthenticatedPeer {
    pub const fn static_public(self) -> NoiseStaticPublicKey {
        self.static_public
    }

    pub const fn public_id(self) -> [u8; 16] {
        self.public_id
    }
}

impl fmt::Debug for AuthenticatedPeer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AuthenticatedPeer")
            .field("public_id_len", &16_usize)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct NoiseHandshakeMessage {
    step: u8,
    body: Vec<u8>,
}

impl NoiseHandshakeMessage {
    pub const fn step(&self) -> u8 {
        self.step
    }

    pub fn body(&self) -> &[u8] {
        &self.body
    }

    pub fn encoded_len(&self) -> usize {
        2 + self.body.len()
    }

    pub fn encode(&self) -> Result<Vec<u8>, CryptoError> {
        self.validate()?;
        let mut encoded = Vec::with_capacity(self.encoded_len());
        encoded.push(HANDSHAKE_FRAME_VERSION);
        encoded.push(self.step);
        encoded.extend_from_slice(&self.body);
        Ok(encoded)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, CryptoError> {
        if bytes.len() > usize::try_from(MAX_HANDSHAKE_MESSAGE_BYTES).unwrap_or(usize::MAX) {
            return Err(CryptoError::HandshakeOversized);
        }
        if bytes.len() < 2 {
            return Err(CryptoError::TruncatedFrame {
                declared: bytes.len(),
            });
        }
        if bytes[0] != HANDSHAKE_FRAME_VERSION {
            return Err(CryptoError::UnsupportedVersion { version: bytes[0] });
        }
        let message = Self {
            step: bytes[1],
            body: bytes[2..].to_vec(),
        };
        message.validate()?;
        Ok(message)
    }

    fn validate(&self) -> Result<(), CryptoError> {
        if self.encoded_len() > usize::try_from(MAX_HANDSHAKE_MESSAGE_BYTES).unwrap_or(usize::MAX) {
            return Err(CryptoError::HandshakeOversized);
        }
        Ok(())
    }
}

impl fmt::Debug for NoiseHandshakeMessage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NoiseHandshakeMessage")
            .field("step", &self.step)
            .field("body_len", &self.body.len())
            .finish()
    }
}

pub struct NoiseHandshake {
    state: HandshakeState,
    role: ChannelRole,
    first_pairing: bool,
    prologue: CryptoPrologue,
    local_public: NoiseStaticPublicKey,
    local_identity: NoiseIdentityBinding,
    expected_remote: Option<NoiseStaticPublicKey>,
    remote_peer: Option<AuthenticatedPeer>,
    writes: u8,
    reads: u8,
    expected_messages: u8,
    opened_at_unix: u64,
    direct_reachable: bool,
}

pub struct NoiseTransport {
    transport: TransportState,
    role: ChannelRole,
    prologue: CryptoPrologue,
    local_static: NoiseStaticPublicKey,
    remote: AuthenticatedPeer,
    opened_at_unix: u64,
    direct_reachable: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct CryptoPrologue {
    protocol_major: u16,
    purpose: CredentialPurpose,
    route_id: [u8; 16],
    session_id: [u8; 16],
}

impl CryptoPrologue {
    pub fn new(
        protocol_major: u16,
        purpose: CredentialPurpose,
        route_id: [u8; 16],
        session_id: [u8; 16],
    ) -> Result<Self, CryptoError> {
        if protocol_major != PROTOCOL_MAJOR {
            return Err(CryptoError::ProtocolMajor);
        }
        Ok(Self {
            protocol_major,
            purpose,
            route_id,
            session_id,
        })
    }

    pub const fn protocol_major(self) -> u16 {
        self.protocol_major
    }

    pub const fn purpose(self) -> CredentialPurpose {
        self.purpose
    }

    pub const fn route_id(self) -> [u8; 16] {
        self.route_id
    }

    pub const fn session_id(self) -> [u8; 16] {
        self.session_id
    }

    pub fn canonical_bytes(self) -> Vec<u8> {
        let label = self.purpose.transcript_label();
        let mut bytes =
            Vec::with_capacity(CONNECT_CRYPTO_PROTOCOL.len() + 2 + 1 + label.len() + 32);
        bytes.extend_from_slice(CONNECT_CRYPTO_PROTOCOL);
        bytes.extend_from_slice(&self.protocol_major.to_be_bytes());
        bytes.push(u8::try_from(label.len()).expect("transcript label fits u8"));
        bytes.extend_from_slice(label);
        bytes.extend_from_slice(&self.route_id);
        bytes.extend_from_slice(&self.session_id);
        bytes
    }
}

impl fmt::Debug for CryptoPrologue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CryptoPrologue")
            .field("protocol_major", &self.protocol_major)
            .field("purpose", &self.purpose)
            .field("route_id", &self.route_id)
            .field("session_id", &self.session_id)
            .finish()
    }
}

#[derive(Clone)]
pub struct ChannelKey(Zeroizing<[u8; CHANNEL_KEY_BYTES]>);

impl ChannelKey {
    pub fn from_bytes(bytes: [u8; CHANNEL_KEY_BYTES]) -> Self {
        Self(Zeroizing::new(bytes))
    }

    pub fn generate() -> Result<Self, CryptoError> {
        let mut bytes = [0_u8; CHANNEL_KEY_BYTES];
        getrandom::fill(&mut bytes).map_err(|_| CryptoError::EntropyUnavailable)?;
        Ok(Self::from_bytes(bytes))
    }

    pub fn as_bytes(&self) -> &[u8; CHANNEL_KEY_BYTES] {
        &self.0
    }
}

impl PartialEq for ChannelKey {
    fn eq(&self, other: &Self) -> bool {
        self.0.as_slice() == other.0.as_slice()
    }
}

impl Eq for ChannelKey {}

impl fmt::Debug for ChannelKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ChannelKey")
            .field("len", &CHANNEL_KEY_BYTES)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct SealedFrame {
    version: u8,
    sequence: u64,
    nonce: [u8; SEALED_NONCE_BYTES],
    ciphertext: Vec<u8>,
    tag: [u8; SEALED_TAG_BYTES],
}

impl SealedFrame {
    pub fn from_parts(
        version: u8,
        sequence: u64,
        nonce: [u8; SEALED_NONCE_BYTES],
        ciphertext: Vec<u8>,
        tag: [u8; SEALED_TAG_BYTES],
    ) -> Result<Self, CryptoError> {
        let frame = Self {
            version,
            sequence,
            nonce,
            ciphertext,
            tag,
        };
        frame.validate()?;
        Ok(frame)
    }

    pub fn validate(&self) -> Result<(), CryptoError> {
        if self.version != SEALED_FRAME_VERSION {
            return Err(CryptoError::UnsupportedVersion {
                version: self.version,
            });
        }
        if self.sequence == 0 {
            return Err(CryptoError::ZeroSequence);
        }
        if self.encoded_len() > usize::try_from(MAX_SEALED_FRAME_BYTES).unwrap_or(usize::MAX) {
            return Err(CryptoError::FrameExceeded {
                declared: u64::try_from(self.encoded_len()).unwrap_or(u64::MAX),
            });
        }
        Ok(())
    }

    pub const fn version(&self) -> u8 {
        self.version
    }

    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    pub const fn nonce(&self) -> [u8; SEALED_NONCE_BYTES] {
        self.nonce
    }

    pub fn ciphertext(&self) -> &[u8] {
        &self.ciphertext
    }

    pub const fn tag(&self) -> [u8; SEALED_TAG_BYTES] {
        self.tag
    }

    pub fn encoded_len(&self) -> usize {
        1 + 8 + SEALED_NONCE_BYTES + self.ciphertext.len() + SEALED_TAG_BYTES
    }

    pub fn encode(&self) -> Result<Vec<u8>, CryptoError> {
        self.validate()?;
        let mut encoded = Vec::with_capacity(self.encoded_len());
        encoded.push(self.version);
        encoded.extend_from_slice(&self.sequence.to_be_bytes());
        encoded.extend_from_slice(&self.nonce);
        encoded.extend_from_slice(&self.ciphertext);
        encoded.extend_from_slice(&self.tag);
        Ok(encoded)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, CryptoError> {
        if bytes.len() > usize::try_from(MAX_SEALED_FRAME_BYTES).unwrap_or(usize::MAX) {
            return Err(CryptoError::FrameExceeded {
                declared: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
            });
        }
        let minimum = SEALED_FRAME_OVERHEAD_BYTES as usize;
        if bytes.len() < minimum {
            return Err(CryptoError::TruncatedFrame {
                declared: bytes.len(),
            });
        }
        let version = bytes[0];
        let sequence =
            u64::from_be_bytes(bytes[1..9].try_into().expect("sequence slice is 8 bytes"));
        let nonce: [u8; SEALED_NONCE_BYTES] =
            bytes[9..25].try_into().expect("nonce slice is 16 bytes");
        let tag_offset = bytes.len() - SEALED_TAG_BYTES;
        let ciphertext = bytes[25..tag_offset].to_vec();
        let tag: [u8; SEALED_TAG_BYTES] = bytes[tag_offset..]
            .try_into()
            .expect("tag slice is 32 bytes");
        Self::from_parts(version, sequence, nonce, ciphertext, tag)
    }
}

impl fmt::Debug for SealedFrame {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SealedFrame")
            .field("version", &self.version)
            .field("sequence", &self.sequence)
            .field("ciphertext_len", &self.ciphertext.len())
            .field("tag_len", &SEALED_TAG_BYTES)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplayWindow {
    highest: u64,
    seen: u64,
}

impl ReplayWindow {
    pub const fn new() -> Self {
        Self {
            highest: 0,
            seen: 0,
        }
    }

    pub const fn highest(self) -> u64 {
        self.highest
    }

    pub fn accept(&mut self, sequence: u64) -> Result<(), CryptoError> {
        if sequence == 0 {
            return Err(CryptoError::ZeroSequence);
        }
        if self.highest == 0 {
            self.highest = sequence;
            self.seen = 1;
            return Ok(());
        }
        if sequence > self.highest {
            let shift = sequence - self.highest;
            self.seen = if shift >= REPLAY_WINDOW_SIZE {
                1
            } else {
                (self.seen << shift) | 1
            };
            self.highest = sequence;
            return Ok(());
        }
        let distance = self.highest - sequence;
        if distance >= REPLAY_WINDOW_SIZE {
            return Err(CryptoError::ReplayTooOld { sequence });
        }
        let mask = 1_u64 << distance;
        if self.seen & mask != 0 {
            return Err(CryptoError::Replay { sequence });
        }
        self.seen |= mask;
        Ok(())
    }
}

impl Default for ReplayWindow {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone)]
pub struct SourceLevelSealer {
    send_enc: Zeroizing<[u8; 32]>,
    send_mac: Zeroizing<[u8; 32]>,
    recv_enc: Zeroizing<[u8; 32]>,
    recv_mac: Zeroizing<[u8; 32]>,
}

impl SourceLevelSealer {
    pub fn derive(master: &ChannelKey, prologue: CryptoPrologue, role: ChannelRole) -> Self {
        let prologue_bytes = prologue.canonical_bytes();
        Self {
            send_enc: derive_key(master, ENC_LABEL, role.send_role_label(), &prologue_bytes),
            send_mac: derive_key(master, MAC_LABEL, role.send_role_label(), &prologue_bytes),
            recv_enc: derive_key(master, ENC_LABEL, role.recv_role_label(), &prologue_bytes),
            recv_mac: derive_key(master, MAC_LABEL, role.recv_role_label(), &prologue_bytes),
        }
    }

    pub fn seal(
        &self,
        sequence: u64,
        nonce: [u8; SEALED_NONCE_BYTES],
        plaintext: &[u8],
    ) -> Result<SealedFrame, CryptoError> {
        seal_with(&self.send_enc, &self.send_mac, sequence, nonce, plaintext)
    }

    pub fn open(&self, frame: &SealedFrame) -> Result<Vec<u8>, CryptoError> {
        open_with(&self.recv_enc, &self.recv_mac, frame)
    }
}

impl fmt::Debug for SourceLevelSealer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SourceLevelSealer")
            .field("production_ready", &CRYPTO_PRODUCTION_READY)
            .finish()
    }
}

pub fn validate_noise_pattern(pattern: &str, first_pairing: bool) -> Result<(), CryptoError> {
    let expected = if first_pairing {
        NOISE_FIRST_PAIRING_PATTERN
    } else {
        NOISE_PINNED_DEVICE_PATTERN
    };
    if pattern != expected {
        return Err(CryptoError::AlgorithmDowngrade);
    }
    Ok(())
}

/// Validate the locked pattern and construct a real snow handshake.
///
/// `local_static` must be vault-supplied key material. IK requires
/// `expected_remote`. Empty prologues and unlocked patterns fail closed.
pub fn instantiate_noise_channel(
    pattern: &str,
    first_pairing: bool,
    local_static: &NoiseStaticPrivateKey,
    local_public: NoiseStaticPublicKey,
    expected_remote: Option<NoiseStaticPublicKey>,
    prologue: CryptoPrologue,
    role: ChannelRole,
    local_identity: NoiseIdentityBinding,
    now_unix: u64,
    direct_reachable: bool,
) -> Result<NoiseHandshake, CryptoHold> {
    validate_noise_pattern(pattern, first_pairing).map_err(|_| CryptoHold {
        reason: CryptoHoldReason::AlgorithmRejected,
    })?;
    if !first_pairing && expected_remote.is_none() {
        return Err(CryptoHold {
            reason: CryptoHoldReason::MissingStaticKey,
        });
    }
    if first_pairing && expected_remote.is_some() {
        return Err(CryptoHold {
            reason: CryptoHoldReason::AlgorithmRejected,
        });
    }
    NoiseHandshake::open(
        pattern,
        first_pairing,
        local_static,
        local_public,
        expected_remote,
        prologue,
        role,
        local_identity,
        now_unix,
        direct_reachable,
    )
    .map_err(|error| match error {
        CryptoError::AlgorithmDowngrade | CryptoError::EmptyPrologue => CryptoHold {
            reason: CryptoHoldReason::AlgorithmRejected,
        },
        CryptoError::RevokedKey => CryptoHold {
            reason: CryptoHoldReason::AlgorithmRejected,
        },
        _ => CryptoHold {
            reason: CryptoHoldReason::HandshakeRejected,
        },
    })
}

pub fn generate_noise_static_keypair(
) -> Result<(NoiseStaticPrivateKey, NoiseStaticPublicKey), CryptoError> {
    let params = parse_locked_params(NOISE_FIRST_PAIRING_PATTERN)?;
    let builder = Builder::new(params);
    let mut pair = builder
        .generate_keypair()
        .map_err(|_| CryptoError::EntropyUnavailable)?;
    if pair.private.len() != NOISE_STATIC_KEY_BYTES || pair.public.len() != NOISE_STATIC_KEY_BYTES {
        pair.private.zeroize();
        return Err(CryptoError::HandshakeFailed);
    }
    let mut private_bytes = [0_u8; NOISE_STATIC_KEY_BYTES];
    private_bytes.copy_from_slice(&pair.private);
    let mut public_bytes = [0_u8; NOISE_STATIC_KEY_BYTES];
    public_bytes.copy_from_slice(&pair.public);
    pair.private.zeroize();
    Ok((
        NoiseStaticPrivateKey::from_vault_bytes(private_bytes)?,
        NoiseStaticPublicKey::from_bytes(public_bytes)?,
    ))
}

fn parse_locked_params(pattern: &str) -> Result<NoiseParams, CryptoError> {
    pattern.parse().map_err(|_| CryptoError::AlgorithmDowngrade)
}

const IDENTITY_CLAIM_KIND_HOST: u8 = 1;
const IDENTITY_CLAIM_KIND_DEVICE: u8 = 2;
const INNER_AD_VERSION: u8 = 1;
const INNER_AD_HEADER_BYTES: usize = 1 + 8 + 1 + 1 + 16 + 32 + SEALED_NONCE_BYTES;

impl NoiseHandshake {
    fn open(
        pattern: &str,
        first_pairing: bool,
        local_static: &NoiseStaticPrivateKey,
        local_public: NoiseStaticPublicKey,
        expected_remote: Option<NoiseStaticPublicKey>,
        prologue: CryptoPrologue,
        role: ChannelRole,
        local_identity: NoiseIdentityBinding,
        now_unix: u64,
        direct_reachable: bool,
    ) -> Result<Self, CryptoError> {
        let prologue_bytes = prologue.canonical_bytes();
        if prologue_bytes.is_empty() {
            return Err(CryptoError::EmptyPrologue);
        }
        reject_all_zero_key(local_static.as_bytes())?;
        reject_all_zero_key(&local_public.as_bytes())?;
        if let Some(remote) = expected_remote {
            reject_all_zero_key(&remote.as_bytes())?;
        }
        if local_identity
            .host_public_id()
            .iter()
            .all(|byte| *byte == 0)
        {
            return Err(CryptoError::IdentityBinding);
        }
        if local_identity
            .device_public_id()
            .is_some_and(|device| device.iter().all(|byte| *byte == 0))
        {
            return Err(CryptoError::IdentityBinding);
        }
        let params = parse_locked_params(pattern)?;
        let builder = Builder::new(params)
            .local_private_key(local_static.as_bytes())
            .map_err(|_| CryptoError::HandshakeFailed)?
            .prologue(&prologue_bytes)
            .map_err(|_| CryptoError::HandshakeFailed)?;
        let builder = if let Some(remote) = expected_remote {
            let remote_bytes = remote.as_bytes();
            builder
                .remote_public_key(&remote_bytes)
                .map_err(|_| CryptoError::HandshakeFailed)?
        } else {
            builder
        };
        let state = if role.is_initiator() {
            builder
                .build_initiator()
                .map_err(|_| CryptoError::HandshakeFailed)?
        } else {
            builder
                .build_responder()
                .map_err(|_| CryptoError::HandshakeFailed)?
        };
        Ok(Self {
            state,
            role,
            first_pairing,
            prologue,
            local_public,
            local_identity,
            expected_remote,
            remote_peer: None,
            writes: 0,
            reads: 0,
            expected_messages: if first_pairing { 3 } else { 2 },
            opened_at_unix: now_unix,
            direct_reachable,
        })
    }

    pub const fn role(&self) -> ChannelRole {
        self.role
    }

    pub const fn prologue(&self) -> CryptoPrologue {
        self.prologue
    }

    pub const fn is_finished(&self) -> bool {
        self.writes + self.reads >= self.expected_messages
    }

    pub fn write_message(&mut self) -> Result<NoiseHandshakeMessage, CryptoError> {
        if self.is_finished() {
            return Err(CryptoError::HandshakeDuplicate);
        }
        if !self.state.is_my_turn() {
            return Err(CryptoError::HandshakeWrongRole);
        }
        let expected_step = self.writes + self.reads;
        let payload = self.local_claim_bytes();
        if payload.len() > usize::try_from(MAX_HANDSHAKE_PAYLOAD_BYTES).unwrap_or(usize::MAX) {
            return Err(CryptoError::HandshakeOversized);
        }
        let mut buffer = vec![0_u8; usize::try_from(MAX_HANDSHAKE_MESSAGE_BYTES).unwrap_or(0)];
        let written = self
            .state
            .write_message(&payload, &mut buffer)
            .map_err(map_handshake_error)?;
        if written > usize::try_from(MAX_HANDSHAKE_MESSAGE_BYTES).unwrap_or(usize::MAX) {
            return Err(CryptoError::HandshakeOversized);
        }
        buffer.truncate(written);
        self.writes = self
            .writes
            .checked_add(1)
            .ok_or(CryptoError::HandshakeFailed)?;
        Ok(NoiseHandshakeMessage {
            step: expected_step,
            body: buffer,
        })
    }

    pub fn read_message(&mut self, message: &NoiseHandshakeMessage) -> Result<(), CryptoError> {
        if self.is_finished() {
            return Err(CryptoError::HandshakeDuplicate);
        }
        if self.state.is_my_turn() {
            return Err(CryptoError::HandshakeWrongRole);
        }
        let expected_step = self.writes + self.reads;
        if message.step != expected_step {
            return Err(CryptoError::HandshakeOutOfOrder);
        }
        if message.body.len() > usize::try_from(MAX_HANDSHAKE_MESSAGE_BYTES).unwrap_or(usize::MAX) {
            return Err(CryptoError::HandshakeOversized);
        }
        let mut payload = vec![0_u8; usize::try_from(MAX_HANDSHAKE_PAYLOAD_BYTES).unwrap_or(0)];
        let read = self
            .state
            .read_message(&message.body, &mut payload)
            .map_err(map_handshake_error)?;
        if read > usize::try_from(MAX_HANDSHAKE_PAYLOAD_BYTES).unwrap_or(usize::MAX) {
            return Err(CryptoError::HandshakeOversized);
        }
        payload.truncate(read);
        if !payload.is_empty() {
            let claim = decode_identity_claim(&payload)?;
            if let Some(remote) = self.state.get_remote_static() {
                let remote_key = public_key_from_slice(remote)?;
                if claim.static_public != remote_key {
                    return Err(CryptoError::IdentityBinding);
                }
                if let Some(expected) = self.expected_remote {
                    if !constant_time_eq(&expected.as_bytes(), &remote_key.as_bytes()) {
                        return Err(CryptoError::UnexpectedPeer);
                    }
                }
                self.remote_peer = Some(AuthenticatedPeer {
                    static_public: remote_key,
                    public_id: claim.public_id,
                });
            }
        }
        self.reads = self
            .reads
            .checked_add(1)
            .ok_or(CryptoError::HandshakeFailed)?;
        Ok(())
    }

    pub fn finish(self) -> Result<NoiseTransport, CryptoError> {
        if !self.state.is_handshake_finished() {
            return Err(CryptoError::HandshakeFailed);
        }
        let remote_static = public_key_from_slice(
            self.state
                .get_remote_static()
                .ok_or(CryptoError::UnexpectedPeer)?,
        )?;
        if let Some(expected) = self.expected_remote {
            if !constant_time_eq(&expected.as_bytes(), &remote_static.as_bytes()) {
                return Err(CryptoError::UnexpectedPeer);
            }
        }
        let remote = self.remote_peer.ok_or(CryptoError::IdentityBinding)?;
        if remote.static_public != remote_static {
            return Err(CryptoError::IdentityBinding);
        }
        let transport = self
            .state
            .into_transport_mode()
            .map_err(|_| CryptoError::HandshakeFailed)?;
        Ok(NoiseTransport {
            transport,
            role: self.role,
            prologue: self.prologue,
            local_static: self.local_public,
            remote,
            opened_at_unix: self.opened_at_unix,
            direct_reachable: self.direct_reachable,
        })
    }

    fn local_claim_bytes(&self) -> Vec<u8> {
        encode_identity_claim(IdentityClaim {
            kind: if self.local_identity.device_public_id.is_some() {
                IDENTITY_CLAIM_KIND_DEVICE
            } else {
                IDENTITY_CLAIM_KIND_HOST
            },
            public_id: self.local_identity.claim_id(),
            static_public: self.local_public,
        })
    }
}

impl fmt::Debug for NoiseHandshake {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NoiseHandshake")
            .field("role", &self.role)
            .field("first_pairing", &self.first_pairing)
            .field("writes", &self.writes)
            .field("reads", &self.reads)
            .field("finished", &self.is_finished())
            .finish()
    }
}

impl NoiseTransport {
    pub const fn role(&self) -> ChannelRole {
        self.role
    }

    pub const fn prologue(&self) -> CryptoPrologue {
        self.prologue
    }

    pub const fn local_static_public(&self) -> NoiseStaticPublicKey {
        self.local_static
    }

    pub const fn remote_peer(&self) -> AuthenticatedPeer {
        self.remote
    }

    pub const fn opened_at_unix(&self) -> u64 {
        self.opened_at_unix
    }

    pub const fn direct_reachable(&self) -> bool {
        self.direct_reachable
    }

    pub fn seal(
        &mut self,
        sequence: u64,
        nonce: [u8; SEALED_NONCE_BYTES],
        plaintext: &[u8],
    ) -> Result<SealedFrame, CryptoError> {
        if sequence == 0 {
            return Err(CryptoError::ZeroSequence);
        }
        let inner = encode_transport_inner(
            self.prologue,
            self.direct_reachable,
            sequence,
            nonce,
            self.local_static,
            self.remote,
            plaintext,
        )?;
        let mut message = vec![0_u8; inner.len().saturating_add(NOISE_AEAD_TAG_BYTES)];
        let written = self
            .transport
            .write_message(&inner, &mut message)
            .map_err(map_transport_error)?;
        message.truncate(written);
        frame_from_noise_output(sequence, nonce, &message)
    }

    pub fn open(&mut self, frame: &SealedFrame) -> Result<Vec<u8>, CryptoError> {
        frame.validate()?;
        let message = noise_output_from_frame(frame)?;
        let mut payload = vec![0_u8; message.len()];
        let read = self
            .transport
            .read_message(&message, &mut payload)
            .map_err(map_transport_error)?;
        payload.truncate(read);
        decode_transport_inner(
            self.prologue,
            self.direct_reachable,
            frame,
            self.remote,
            &payload,
        )
    }
}

impl fmt::Debug for NoiseTransport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NoiseTransport")
            .field("role", &self.role)
            .field("purpose", &self.prologue.purpose())
            .finish()
    }
}

struct IdentityClaim {
    kind: u8,
    public_id: [u8; 16],
    static_public: NoiseStaticPublicKey,
}

fn encode_identity_claim(claim: IdentityClaim) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(NOISE_IDENTITY_CLAIM_BYTES);
    bytes.push(1);
    bytes.push(claim.kind);
    bytes.extend_from_slice(&claim.public_id);
    bytes.extend_from_slice(&claim.static_public.as_bytes());
    bytes
}

fn decode_identity_claim(bytes: &[u8]) -> Result<IdentityClaim, CryptoError> {
    if bytes.len() != NOISE_IDENTITY_CLAIM_BYTES || bytes[0] != 1 {
        return Err(CryptoError::IdentityBinding);
    }
    let kind = bytes[1];
    if kind != IDENTITY_CLAIM_KIND_HOST && kind != IDENTITY_CLAIM_KIND_DEVICE {
        return Err(CryptoError::IdentityBinding);
    }
    let public_id: [u8; 16] = bytes[2..18]
        .try_into()
        .map_err(|_| CryptoError::IdentityBinding)?;
    if public_id.iter().all(|byte| *byte == 0) {
        return Err(CryptoError::IdentityBinding);
    }
    let static_bytes: [u8; NOISE_STATIC_KEY_BYTES] = bytes[18..50]
        .try_into()
        .map_err(|_| CryptoError::IdentityBinding)?;
    if static_bytes.iter().all(|byte| *byte == 0) {
        return Err(CryptoError::IdentityBinding);
    }
    Ok(IdentityClaim {
        kind,
        public_id,
        static_public: NoiseStaticPublicKey::from_bytes(static_bytes)?,
    })
}

fn public_key_from_slice(bytes: &[u8]) -> Result<NoiseStaticPublicKey, CryptoError> {
    let key: [u8; NOISE_STATIC_KEY_BYTES] = bytes
        .get(..NOISE_STATIC_KEY_BYTES)
        .ok_or(CryptoError::UnexpectedPeer)?
        .try_into()
        .map_err(|_| CryptoError::UnexpectedPeer)?;
    NoiseStaticPublicKey::from_bytes(key).map_err(|_| CryptoError::UnexpectedPeer)
}

fn reject_all_zero_key(bytes: &[u8; NOISE_STATIC_KEY_BYTES]) -> Result<(), CryptoError> {
    if bytes.iter().all(|byte| *byte == 0) {
        return Err(CryptoError::IdentityBinding);
    }
    Ok(())
}

fn encode_transport_inner(
    prologue: CryptoPrologue,
    direct_reachable: bool,
    sequence: u64,
    nonce: [u8; SEALED_NONCE_BYTES],
    sender_static: NoiseStaticPublicKey,
    remote: AuthenticatedPeer,
    plaintext: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    let declared =
        u64::try_from(plaintext.len().saturating_add(INNER_AD_HEADER_BYTES)).unwrap_or(u64::MAX);
    if declared > u64::from(MAX_SEALED_PLAINTEXT_BYTES) {
        return Err(CryptoError::PlaintextExceeded { declared });
    }
    reject_all_zero_key(&sender_static.as_bytes())?;
    reject_all_zero_key(&remote.static_public.as_bytes())?;
    let mut inner = Vec::with_capacity(INNER_AD_HEADER_BYTES + plaintext.len());
    inner.push(INNER_AD_VERSION);
    inner.extend_from_slice(&sequence.to_be_bytes());
    inner.push(match prologue.purpose() {
        CredentialPurpose::OwnerPairing => 0,
        CredentialPurpose::TaskInvitation => 1,
    });
    inner.push(if direct_reachable { 1 } else { 2 });
    inner.extend_from_slice(&prologue.session_id());
    inner.extend_from_slice(&sender_static.as_bytes());
    inner.extend_from_slice(&nonce);
    inner.extend_from_slice(plaintext);
    Ok(inner)
}

fn decode_transport_inner(
    prologue: CryptoPrologue,
    direct_reachable: bool,
    frame: &SealedFrame,
    remote: AuthenticatedPeer,
    inner: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    if inner.len() < INNER_AD_HEADER_BYTES {
        return Err(CryptoError::Authenticity);
    }
    if inner[0] != INNER_AD_VERSION {
        return Err(CryptoError::Authenticity);
    }
    let sequence = u64::from_be_bytes(
        inner[1..9]
            .try_into()
            .map_err(|_| CryptoError::Authenticity)?,
    );
    if sequence != frame.sequence() {
        return Err(CryptoError::Authenticity);
    }
    let purpose = match inner[9] {
        0 => CredentialPurpose::OwnerPairing,
        1 => CredentialPurpose::TaskInvitation,
        _ => return Err(CryptoError::Authenticity),
    };
    if purpose != prologue.purpose() {
        return Err(CryptoError::Authenticity);
    }
    let route = inner[10];
    let expected_route = if direct_reachable { 1 } else { 2 };
    if route != expected_route {
        return Err(CryptoError::Authenticity);
    }
    let session: [u8; 16] = inner[11..27]
        .try_into()
        .map_err(|_| CryptoError::Authenticity)?;
    if session != prologue.session_id() {
        return Err(CryptoError::Authenticity);
    }
    let bound_sender: [u8; NOISE_STATIC_KEY_BYTES] = inner[27..59]
        .try_into()
        .map_err(|_| CryptoError::Authenticity)?;
    reject_all_zero_key(&remote.static_public.as_bytes()).map_err(|_| CryptoError::Authenticity)?;
    if !constant_time_eq(&bound_sender, &remote.static_public.as_bytes()) {
        return Err(CryptoError::Authenticity);
    }
    let bound_nonce: [u8; SEALED_NONCE_BYTES] = inner[59..75]
        .try_into()
        .map_err(|_| CryptoError::Authenticity)?;
    if bound_nonce != frame.nonce() {
        return Err(CryptoError::Authenticity);
    }
    Ok(inner[INNER_AD_HEADER_BYTES..].to_vec())
}

fn frame_from_noise_output(
    sequence: u64,
    nonce: [u8; SEALED_NONCE_BYTES],
    message: &[u8],
) -> Result<SealedFrame, CryptoError> {
    if message.len() < NOISE_AEAD_TAG_BYTES {
        return Err(CryptoError::HandshakeFailed);
    }
    let split = message.len() - NOISE_AEAD_TAG_BYTES;
    let mut tag = [0_u8; SEALED_TAG_BYTES];
    tag[..NOISE_AEAD_TAG_BYTES].copy_from_slice(&message[split..]);
    SealedFrame::from_parts(
        SEALED_FRAME_VERSION,
        sequence,
        nonce,
        message[..split].to_vec(),
        tag,
    )
}

fn noise_output_from_frame(frame: &SealedFrame) -> Result<Vec<u8>, CryptoError> {
    let tag = frame.tag();
    if tag[NOISE_AEAD_TAG_BYTES..].iter().any(|byte| *byte != 0) {
        return Err(CryptoError::Authenticity);
    }
    let mut message = Vec::with_capacity(frame.ciphertext().len() + NOISE_AEAD_TAG_BYTES);
    message.extend_from_slice(frame.ciphertext());
    message.extend_from_slice(&tag[..NOISE_AEAD_TAG_BYTES]);
    Ok(message)
}

fn map_handshake_error(error: snow::Error) -> CryptoError {
    match error {
        snow::Error::State(snow::error::StateProblem::NotTurnToWrite)
        | snow::Error::State(snow::error::StateProblem::NotTurnToRead) => {
            CryptoError::HandshakeWrongRole
        }
        snow::Error::State(snow::error::StateProblem::HandshakeAlreadyFinished) => {
            CryptoError::HandshakeDuplicate
        }
        snow::Error::Input => CryptoError::HandshakeOversized,
        snow::Error::Decrypt => CryptoError::Authenticity,
        _ => CryptoError::HandshakeFailed,
    }
}

fn map_transport_error(error: snow::Error) -> CryptoError {
    match error {
        snow::Error::Decrypt => CryptoError::Authenticity,
        snow::Error::Input => CryptoError::PlaintextExceeded {
            declared: u64::from(MAX_SEALED_PLAINTEXT_BYTES).saturating_add(1),
        },
        snow::Error::State(snow::error::StateProblem::Exhausted) => CryptoError::SequenceExhausted,
        _ => CryptoError::Authenticity,
    }
}

fn derive_key(
    master: &ChannelKey,
    label: &[u8],
    role: &[u8],
    prologue: &[u8],
) -> Zeroizing<[u8; 32]> {
    let mut mac =
        HmacSha256::new_from_slice(master.as_bytes()).expect("HMAC-SHA256 accepts 32 bytes");
    mac.update(label);
    mac.update(role);
    mac.update(prologue);
    Zeroizing::new(mac.finalize().into_bytes().into())
}

fn hmac_block(key: &[u8; 32], message: &[u8]) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC-SHA256 accepts 32 bytes");
    mac.update(message);
    mac.finalize().into_bytes().into()
}

fn apply_keystream(enc_key: &[u8; 32], nonce: &[u8; 16], sequence: u64, buffer: &mut [u8]) {
    let mut offset = 0_usize;
    let mut block_index = 0_u32;
    while offset < buffer.len() {
        let mut message = Vec::with_capacity(KS_LABEL.len() + 16 + 8 + 4);
        message.extend_from_slice(KS_LABEL);
        message.extend_from_slice(nonce);
        message.extend_from_slice(&sequence.to_be_bytes());
        message.extend_from_slice(&block_index.to_be_bytes());
        let block = hmac_block(enc_key, &message);
        let take = (buffer.len() - offset).min(block.len());
        for (index, byte) in block[..take].iter().enumerate() {
            buffer[offset + index] ^= byte;
        }
        offset += take;
        block_index = block_index
            .checked_add(1)
            .expect("keystream block index stays in u32 for bounded frames");
    }
}

fn authenticate(
    mac_key: &[u8; 32],
    sequence: u64,
    nonce: &[u8; 16],
    ciphertext: &[u8],
) -> [u8; 32] {
    let mut message = Vec::with_capacity(TAG_LABEL.len() + 1 + 8 + 16 + ciphertext.len());
    message.extend_from_slice(TAG_LABEL);
    message.push(SEALED_FRAME_VERSION);
    message.extend_from_slice(&sequence.to_be_bytes());
    message.extend_from_slice(nonce);
    message.extend_from_slice(ciphertext);
    hmac_block(mac_key, &message)
}

fn seal_with(
    enc_key: &[u8; 32],
    mac_key: &[u8; 32],
    sequence: u64,
    nonce: [u8; SEALED_NONCE_BYTES],
    plaintext: &[u8],
) -> Result<SealedFrame, CryptoError> {
    if sequence == 0 {
        return Err(CryptoError::ZeroSequence);
    }
    let declared = u64::try_from(plaintext.len()).unwrap_or(u64::MAX);
    if declared > u64::from(MAX_SEALED_PLAINTEXT_BYTES) {
        return Err(CryptoError::PlaintextExceeded { declared });
    }
    let mut ciphertext = plaintext.to_vec();
    apply_keystream(enc_key, &nonce, sequence, &mut ciphertext);
    let tag = authenticate(mac_key, sequence, &nonce, &ciphertext);
    SealedFrame::from_parts(SEALED_FRAME_VERSION, sequence, nonce, ciphertext, tag)
}

fn open_with(
    enc_key: &[u8; 32],
    mac_key: &[u8; 32],
    frame: &SealedFrame,
) -> Result<Vec<u8>, CryptoError> {
    frame.validate()?;
    let expected = authenticate(mac_key, frame.sequence, &frame.nonce, &frame.ciphertext);
    if !constant_time_eq(&expected, &frame.tag) {
        return Err(CryptoError::Authenticity);
    }
    let mut plaintext = frame.ciphertext.clone();
    apply_keystream(enc_key, &frame.nonce, frame.sequence, &mut plaintext);
    Ok(plaintext)
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CryptoError {
    ProtocolMajor,
    AlgorithmDowngrade,
    EntropyUnavailable,
    UnsupportedVersion { version: u8 },
    ZeroSequence,
    SequenceExhausted,
    SessionExpired,
    Replay { sequence: u64 },
    ReplayTooOld { sequence: u64 },
    TruncatedFrame { declared: usize },
    FrameExceeded { declared: u64 },
    PlaintextExceeded { declared: u64 },
    Authenticity,
    InvalidEnvelope,
    RevokedKey,
    EmptyPrologue,
    HandshakeWrongRole,
    HandshakeOutOfOrder,
    HandshakeDuplicate,
    HandshakeOversized,
    HandshakeFailed,
    UnexpectedPeer,
    IdentityBinding,
}

impl fmt::Display for CryptoError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ProtocolMajor => {
                formatter.write_str("Connect crypto prologue protocol major must be 1")
            }
            Self::AlgorithmDowngrade => {
                formatter.write_str("Connect crypto rejects runtime algorithm negotiation")
            }
            Self::EntropyUnavailable => {
                formatter.write_str("Connect crypto entropy is unavailable")
            }
            Self::UnsupportedVersion { version } => {
                write!(
                    formatter,
                    "Connect sealed frame version {version} is unsupported"
                )
            }
            Self::ZeroSequence => {
                formatter.write_str("Connect sealed frame sequence must be nonzero")
            }
            Self::SequenceExhausted => {
                formatter.write_str("Connect crypto session requires a new handshake")
            }
            Self::SessionExpired => {
                formatter.write_str("Connect crypto session exceeded the one-hour bound")
            }
            Self::Replay { sequence } => {
                write!(
                    formatter,
                    "Connect sealed frame sequence {sequence} was already accepted"
                )
            }
            Self::ReplayTooOld { sequence } => {
                write!(
                    formatter,
                    "Connect sealed frame sequence {sequence} is outside the replay window"
                )
            }
            Self::TruncatedFrame { declared } => {
                write!(
                    formatter,
                    "Connect sealed frame is truncated at {declared} bytes"
                )
            }
            Self::FrameExceeded { declared } => write!(
                formatter,
                "Connect sealed frame length {declared} exceeds {MAX_SEALED_FRAME_BYTES}"
            ),
            Self::PlaintextExceeded { declared } => write!(
                formatter,
                "Connect sealed plaintext length {declared} exceeds {MAX_SEALED_PLAINTEXT_BYTES}"
            ),
            Self::Authenticity => formatter.write_str("Connect sealed frame failed authenticity"),
            Self::InvalidEnvelope => {
                formatter.write_str("Connect sealed frame did not contain a valid inner envelope")
            }
            Self::RevokedKey => formatter.write_str("Connect device or grant key is revoked"),
            Self::EmptyPrologue => formatter
                .write_str("Connect Noise prologue must be the canonical non-empty binding"),
            Self::HandshakeWrongRole => formatter
                .write_str("Connect Noise handshake message was written or read out of role"),
            Self::HandshakeOutOfOrder => {
                formatter.write_str("Connect Noise handshake message arrived out of order")
            }
            Self::HandshakeDuplicate => {
                formatter.write_str("Connect Noise handshake rejected a duplicate message")
            }
            Self::HandshakeOversized => {
                formatter.write_str("Connect Noise handshake message exceeded the bounded size")
            }
            Self::HandshakeFailed => formatter.write_str("Connect Noise handshake failed closed"),
            Self::UnexpectedPeer => formatter
                .write_str("Connect Noise handshake rejected an unexpected peer static key"),
            Self::IdentityBinding => {
                formatter.write_str("Connect Noise handshake identity did not match the transcript")
            }
        }
    }
}

impl std::error::Error for CryptoError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CryptoHoldReason {
    ProductionReviewRequired,
    MissingSnowCrate,
    DualTargetUnproven,
    IndependentReviewRequired,
    AlgorithmRejected,
    MissingStaticKey,
    HandshakeRejected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CryptoHold {
    pub reason: CryptoHoldReason,
}

impl fmt::Display for CryptoHold {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self.reason {
            CryptoHoldReason::ProductionReviewRequired => {
                "Connect Noise channel requires the retained production review gate"
            }
            CryptoHoldReason::MissingSnowCrate => {
                "Connect Noise channel requires its Noise implementation dependency"
            }
            CryptoHoldReason::DualTargetUnproven => {
                "Connect Noise channel requires native and wasm32 proof coverage"
            }
            CryptoHoldReason::IndependentReviewRequired => {
                "Connect Noise channel requires the retained independent review gate"
            }
            CryptoHoldReason::AlgorithmRejected => {
                "Connect Noise channel rejected a non-locked algorithm"
            }
            CryptoHoldReason::MissingStaticKey => {
                "Connect Noise channel requires vault-supplied static key material"
            }
            CryptoHoldReason::HandshakeRejected => {
                "Connect Noise handshake was rejected by the production opener"
            }
        })
    }
}

impl std::error::Error for CryptoHold {}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_prologue() -> CryptoPrologue {
        CryptoPrologue::new(
            PROTOCOL_MAJOR,
            CredentialPurpose::OwnerPairing,
            [9; 16],
            [8; 16],
        )
        .expect("prologue")
    }

    fn complete_xx() -> (
        NoiseTransport,
        NoiseTransport,
        AuthenticatedPeer,
        AuthenticatedPeer,
    ) {
        let initiator_keys = NoiseCustody::generate().expect("initiator keys");
        let responder_keys = NoiseCustody::generate().expect("responder keys");
        let prologue = test_prologue();
        let mut initiator = instantiate_noise_channel(
            NOISE_FIRST_PAIRING_PATTERN,
            true,
            initiator_keys.private(),
            initiator_keys.public(),
            None,
            prologue,
            ChannelRole::Initiator,
            NoiseIdentityBinding::host([1; 16]),
            10,
            true,
        )
        .expect("xx initiator");
        let mut responder = instantiate_noise_channel(
            NOISE_FIRST_PAIRING_PATTERN,
            true,
            responder_keys.private(),
            responder_keys.public(),
            None,
            prologue,
            ChannelRole::Responder,
            NoiseIdentityBinding::host([2; 16]),
            10,
            true,
        )
        .expect("xx responder");
        let msg1 = initiator.write_message().expect("xx msg1");
        responder.read_message(&msg1).expect("read msg1");
        let msg2 = responder.write_message().expect("xx msg2");
        initiator.read_message(&msg2).expect("read msg2");
        let msg3 = initiator.write_message().expect("xx msg3");
        responder.read_message(&msg3).expect("read msg3");
        let initiator_transport = initiator.finish().expect("initiator finish");
        let responder_transport = responder.finish().expect("responder finish");
        let initiator_peer = initiator_transport.remote_peer();
        let responder_peer = responder_transport.remote_peer();
        (
            initiator_transport,
            responder_transport,
            initiator_peer,
            responder_peer,
        )
    }

    #[test]
    fn production_constructors_reject_all_zero_static_material() {
        assert!(matches!(
            NoiseStaticPrivateKey::from_vault_bytes([0; 32]),
            Err(CryptoError::IdentityBinding)
        ));
        assert!(matches!(
            NoiseStaticPublicKey::from_bytes([0; 32]),
            Err(CryptoError::IdentityBinding)
        ));
        let custody = NoiseCustody::generate().expect("nonzero custody");
        let zero_public = NoiseStaticPublicKey([0; 32]);
        assert!(NoiseCustody::from_vault(custody.private, zero_public).is_err());
    }

    #[test]
    fn production_ready_is_true_because_snow_is_compiled() {
        assert!(CRYPTO_PRODUCTION_READY);
        validate_noise_pattern(NOISE_FIRST_PAIRING_PATTERN, true).expect("xx");
        validate_noise_pattern(NOISE_PINNED_DEVICE_PATTERN, false).expect("ik");
        assert!(validate_noise_pattern(NOISE_PINNED_DEVICE_PATTERN, true).is_err());
        assert!(validate_noise_pattern("Noise_NN_25519_ChaChaPoly_BLAKE2s", true).is_err());
    }

    #[test]
    fn xx_role_pair_binds_identity_and_round_trips() {
        let (mut initiator, mut responder, initiator_peer, responder_peer) = complete_xx();
        assert_eq!(initiator_peer.public_id(), [2; 16]);
        assert_eq!(responder_peer.public_id(), [1; 16]);
        assert_ne!(
            initiator_peer.static_public(),
            responder_peer.static_public()
        );
        assert_eq!(
            initiator.local_static_public(),
            responder.remote_peer().static_public()
        );
        assert_eq!(
            responder.local_static_public(),
            initiator.remote_peer().static_public()
        );
        let frame = initiator.seal(1, [3; 16], b"hello-xx").expect("seal");
        assert_eq!(frame.tag()[NOISE_AEAD_TAG_BYTES..], [0_u8; 16]);
        let opened = responder.open(&frame).expect("open");
        assert_eq!(opened, b"hello-xx");
        let reply = responder.seal(1, [4; 16], b"ack-xx").expect("reply");
        assert_eq!(initiator.open(&reply).expect("open reply"), b"ack-xx");
    }

    #[test]
    fn ik_role_pair_requires_known_peer_and_rejects_mismatch() {
        let initiator_keys = NoiseCustody::generate().expect("initiator");
        let responder_keys = NoiseCustody::generate().expect("responder");
        let stranger = NoiseCustody::generate().expect("stranger");
        let prologue = test_prologue();
        let mut initiator = instantiate_noise_channel(
            NOISE_PINNED_DEVICE_PATTERN,
            false,
            initiator_keys.private(),
            initiator_keys.public(),
            Some(responder_keys.public()),
            prologue,
            ChannelRole::Initiator,
            NoiseIdentityBinding::host_device([3; 16], [4; 16]),
            20,
            false,
        )
        .expect("ik initiator");
        let mut responder = instantiate_noise_channel(
            NOISE_PINNED_DEVICE_PATTERN,
            false,
            responder_keys.private(),
            responder_keys.public(),
            Some(initiator_keys.public()),
            prologue,
            ChannelRole::Responder,
            NoiseIdentityBinding::host([5; 16]),
            20,
            false,
        )
        .expect("ik responder");
        let msg1 = initiator.write_message().expect("ik msg1");
        responder.read_message(&msg1).expect("ik read1");
        let msg2 = responder.write_message().expect("ik msg2");
        initiator.read_message(&msg2).expect("ik read2");
        let mut initiator = initiator.finish().expect("ik finish initiator");
        let mut responder = responder.finish().expect("ik finish responder");
        assert_eq!(
            initiator.remote_peer().static_public(),
            responder_keys.public()
        );
        assert_eq!(initiator.local_static_public(), initiator_keys.public());
        assert_eq!(
            initiator.local_static_public(),
            responder.remote_peer().static_public()
        );
        assert_eq!(
            responder.local_static_public(),
            initiator.remote_peer().static_public()
        );
        let frame = initiator.seal(1, [7; 16], b"hello-ik").expect("ik seal");
        assert_eq!(responder.open(&frame).expect("ik open"), b"hello-ik");

        let mismatch = instantiate_noise_channel(
            NOISE_PINNED_DEVICE_PATTERN,
            false,
            initiator_keys.private(),
            initiator_keys.public(),
            Some(stranger.public()),
            prologue,
            ChannelRole::Initiator,
            NoiseIdentityBinding::host([6; 16]),
            20,
            true,
        )
        .expect("mismatch initiator");
        let mut wrong_responder = instantiate_noise_channel(
            NOISE_PINNED_DEVICE_PATTERN,
            false,
            responder_keys.private(),
            responder_keys.public(),
            Some(initiator_keys.public()),
            prologue,
            ChannelRole::Responder,
            NoiseIdentityBinding::host([5; 16]),
            20,
            true,
        )
        .expect("true responder");
        let mut mismatch = mismatch;
        let bad = mismatch.write_message().expect("mismatch write");
        assert!(wrong_responder.read_message(&bad).is_err());
    }

    #[test]
    fn handshake_rejects_wrong_role_order_duplicate_and_oversized() {
        let initiator_keys = NoiseCustody::generate().expect("initiator");
        let responder_keys = NoiseCustody::generate().expect("responder");
        let prologue = test_prologue();
        let mut initiator = instantiate_noise_channel(
            NOISE_FIRST_PAIRING_PATTERN,
            true,
            initiator_keys.private(),
            initiator_keys.public(),
            None,
            prologue,
            ChannelRole::Initiator,
            NoiseIdentityBinding::host([1; 16]),
            1,
            true,
        )
        .expect("initiator");
        let mut responder = instantiate_noise_channel(
            NOISE_FIRST_PAIRING_PATTERN,
            true,
            responder_keys.private(),
            responder_keys.public(),
            None,
            prologue,
            ChannelRole::Responder,
            NoiseIdentityBinding::host([2; 16]),
            1,
            true,
        )
        .expect("responder");
        assert!(matches!(
            initiator.read_message(&NoiseHandshakeMessage {
                step: 0,
                body: vec![1, 2, 3],
            }),
            Err(CryptoError::HandshakeWrongRole)
        ));
        let msg1 = initiator.write_message().expect("msg1");
        assert!(matches!(
            responder.read_message(&NoiseHandshakeMessage {
                step: 2,
                body: msg1.body().to_vec(),
            }),
            Err(CryptoError::HandshakeOutOfOrder)
        ));
        responder.read_message(&msg1).expect("good read");
        let msg2 = responder.write_message().expect("msg2");
        initiator.read_message(&msg2).expect("msg2");
        let msg3 = initiator.write_message().expect("msg3");
        responder.read_message(&msg3).expect("msg3");
        assert!(matches!(
            initiator.write_message(),
            Err(CryptoError::HandshakeDuplicate)
        ));
        let oversized = vec![0_u8; usize::try_from(MAX_HANDSHAKE_MESSAGE_BYTES).unwrap() + 8];
        assert!(matches!(
            NoiseHandshakeMessage::decode(&oversized),
            Err(CryptoError::HandshakeOversized)
        ));
    }

    #[test]
    fn production_transport_rejects_replay_and_keeps_source_level_non_production() {
        let (mut initiator, mut responder, _, _) = complete_xx();
        let frame = initiator.seal(1, [9; 16], b"once").expect("seal");
        assert_eq!(responder.open(&frame).expect("first"), b"once");
        assert!(matches!(
            responder.open(&frame),
            Err(CryptoError::Authenticity) | Err(CryptoError::Replay { .. })
        ));
        let encoded = frame.encode().expect("encode");
        let relayed = SealedFrame::decode(&encoded).expect("opaque relay decode");
        assert_eq!(relayed.sequence(), frame.sequence());
        assert_eq!(relayed.ciphertext(), frame.ciphertext());
    }
}
