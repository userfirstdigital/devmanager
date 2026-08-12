//! Transport-neutral protocol compatibility and wire framing contracts.

mod browser;
mod capabilities;
mod chunk;
mod control;
mod crypto;
mod envelope;
mod frame;
mod org;
mod reconnect;
mod request;
mod stream;

pub use browser::*;
pub use capabilities::{
    Capability, CapabilitySet, ProtocolVersion, VersionNegotiationError, PROTOCOL_MAJOR,
    PROTOCOL_MINOR,
};
pub use chunk::{
    ChunkContext, ChunkError, ChunkFrame, ChunkLimitField, ChunkLimits, ChunkLimitsError,
    MAX_CHUNK_BYTES, MAX_CHUNK_CURSOR_BYTES, MAX_CHUNK_PAYLOAD_BYTES, MAX_CHUNK_REASSEMBLY_BYTES,
    MAX_CUMULATIVE_BYTES, MAX_CURSOR_BYTES,
};
pub use control::{DetachAck, DetachRequest};
pub use crypto::{
    generate_noise_static_keypair, instantiate_noise_channel, validate_noise_pattern,
    AuthenticatedPeer, ChannelKey, ChannelRole, CredentialPurpose, CryptoError, CryptoHold,
    CryptoHoldReason, CryptoPrologue, NoiseCustody, NoiseHandshake, NoiseHandshakeMessage,
    NoiseIdentityBinding, NoiseStaticPrivateKey, NoiseStaticPublicKey, NoiseTransport,
    ReplayWindow, SealedFrame, SourceLevelSealer, CHANNEL_KEY_BYTES, CONNECT_CRYPTO_PROTOCOL,
    CRYPTO_PRODUCTION_READY, HANDSHAKE_FRAME_VERSION, MAX_CHANNEL_SEQUENCES,
    MAX_HANDSHAKE_MESSAGE_BYTES, MAX_HANDSHAKE_PAYLOAD_BYTES, MAX_SEALED_FRAME_BYTES,
    MAX_SEALED_PLAINTEXT_BYTES, MAX_SESSION_AGE_SECS, NOISE_AEAD_TAG_BYTES,
    NOISE_FIRST_PAIRING_PATTERN, NOISE_IDENTITY_CLAIM_BYTES, NOISE_PINNED_DEVICE_PATTERN,
    NOISE_STATIC_KEY_BYTES, REPLAY_WINDOW_SIZE, SEALED_FRAME_OVERHEAD_BYTES, SEALED_FRAME_VERSION,
    SEALED_NONCE_BYTES, SEALED_TAG_BYTES,
};
pub use envelope::{
    personal_prompt_library_granted, BrowserProjectionEnvelope, BrowserProjectionEnvelopeError,
    ClientBuildError, ClientHello, ClientHelloError, MessagePackCodec, MessagePackError,
    MessagePackLengthKind, NegotiatedParameters, ProfileFingerprint, ServerBuildError, ServerHello,
    ServerHelloError, MAX_CLIENT_BUILD_BYTES, MAX_MESSAGEPACK_COLLECTION_ITEMS,
    MAX_MESSAGEPACK_DEPTH, MAX_MESSAGEPACK_VALUES, MAX_SERVER_BUILD_BYTES,
    PROFILE_FINGERPRINT_DOMAIN,
};
pub use frame::{
    FrameLimitField, FrameLimits, FrameLimitsError, PhysicalFrameCodec, PhysicalFrameError,
    MAX_PHYSICAL_FRAME_BYTES, MAX_REASSEMBLED_MESSAGE_BYTES,
};
pub use org::{
    organization_extension_type, OrganizationExtensionKind, ORGANIZATION_PROMPT_BODY_LIMIT_BYTES,
    ORGANIZATION_PROMPT_PAGE_ENCODED_LIMIT_BYTES, ORGANIZATION_PROMPT_PAGE_ITEM_LIMIT,
    ORGANIZATION_SCHEMA_VERSION,
};
pub use reconnect::ReconnectGrant;
pub use request::{ClientRequest, ServerMessage};
pub use stream::{StreamFrame, StreamKey, StreamPayloadKind};

/// Journal pages use the same bounded MessagePack codec as other protocol
/// documents. Raw provider payloads are never a protocol page field.
pub use crate::domain::{SemanticJournalFact, SemanticJournalPage, SemanticJournalPayload};

/// Host/protocol journal query is capability-unavailable until a later task
/// wires `Query`/`ServerMessage` and a kernel subscription. A type re-export
/// is not an integration seam.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SemanticJournalQueryUnavailable;

impl std::fmt::Display for SemanticJournalQueryUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("semantic journal protocol query is capability-unavailable on this base")
    }
}

impl std::error::Error for SemanticJournalQueryUnavailable {}

pub const fn semantic_journal_query_available() -> bool {
    false
}

pub fn query_semantic_journal_page(
    _after_sequence: u64,
    _limits: crate::domain::PageLimits,
) -> Result<SemanticJournalPage, SemanticJournalQueryUnavailable> {
    let _ = _after_sequence;
    let _ = _limits;
    Err(SemanticJournalQueryUnavailable)
}
