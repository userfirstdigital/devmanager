//! Bounded sealed-frame and replay contract for Connect end-to-end channels.
//!
//! Production hosted use is locked to `Noise_XX_25519_ChaChaPoly_BLAKE2s` for
//! first pairing or invitation redemption and
//! `Noise_IK_25519_ChaChaPoly_BLAKE2s` for pinned-device sessions. Those
//! constructions are not instantiated here: the `snow` crate is not a current
//! dependency, dual-target WASM proof has not run, and independent review is
//! still required. See ADR 0002 and `CRYPTO_PRODUCTION_READY`.
//!
//! The source-level sealer is an HMAC-SHA256 PRF plus Encrypt-then-MAC bound
//! to the v1 prologue. It exists so tests can prove frame bounds, replay
//! rejection, purpose isolation, and relay opacity without adding crates.

use std::fmt;

use hmac::{Hmac, Mac};
use sha2::Sha256;
use zeroize::Zeroizing;

use super::frame::MAX_PHYSICAL_FRAME_BYTES;
use super::PROTOCOL_MAJOR;

type HmacSha256 = Hmac<Sha256>;

pub const CONNECT_CRYPTO_PROTOCOL: &[u8] = b"DevManagerConnect/v1\0";
pub const NOISE_FIRST_PAIRING_PATTERN: &str = "Noise_XX_25519_ChaChaPoly_BLAKE2s";
pub const NOISE_PINNED_DEVICE_PATTERN: &str = "Noise_IK_25519_ChaChaPoly_BLAKE2s";
pub const CRYPTO_PRODUCTION_READY: bool = false;
pub const SEALED_FRAME_VERSION: u8 = 1;
pub const SEALED_NONCE_BYTES: usize = 16;
pub const SEALED_TAG_BYTES: usize = 32;
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

pub fn instantiate_noise_channel(pattern: &str, first_pairing: bool) -> Result<(), CryptoHold> {
    validate_noise_pattern(pattern, first_pairing).map_err(|_| CryptoHold {
        reason: CryptoHoldReason::AlgorithmRejected,
    })?;
    Err(CryptoHold {
        reason: CryptoHoldReason::ProductionReviewRequired,
    })
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CryptoHold {
    pub reason: CryptoHoldReason,
}

impl fmt::Display for CryptoHold {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self.reason {
            CryptoHoldReason::ProductionReviewRequired => {
                "Connect Noise channel is on HOLD until production crypto review"
            }
            CryptoHoldReason::MissingSnowCrate => {
                "Connect Noise channel is on HOLD because snow is not a current dependency"
            }
            CryptoHoldReason::DualTargetUnproven => {
                "Connect Noise channel is on HOLD until native and wasm32 proofs pass"
            }
            CryptoHoldReason::IndependentReviewRequired => {
                "Connect Noise channel is on HOLD until independent security review"
            }
            CryptoHoldReason::AlgorithmRejected => {
                "Connect Noise channel rejected a non-locked algorithm"
            }
        })
    }
}

impl std::error::Error for CryptoHold {}
