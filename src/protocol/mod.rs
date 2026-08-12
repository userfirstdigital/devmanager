//! Transport-neutral protocol compatibility and wire framing contracts.

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
    instantiate_noise_channel, validate_noise_pattern, ChannelKey, ChannelRole, CredentialPurpose,
    CryptoError, CryptoHold, CryptoHoldReason, CryptoPrologue, ReplayWindow, SealedFrame,
    SourceLevelSealer, CHANNEL_KEY_BYTES, CONNECT_CRYPTO_PROTOCOL, CRYPTO_PRODUCTION_READY,
    MAX_CHANNEL_SEQUENCES, MAX_SEALED_FRAME_BYTES, MAX_SEALED_PLAINTEXT_BYTES,
    MAX_SESSION_AGE_SECS, NOISE_FIRST_PAIRING_PATTERN, NOISE_PINNED_DEVICE_PATTERN,
    REPLAY_WINDOW_SIZE, SEALED_FRAME_OVERHEAD_BYTES, SEALED_FRAME_VERSION, SEALED_NONCE_BYTES,
    SEALED_TAG_BYTES,
};
pub use envelope::{
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
