//! Transport-neutral protocol compatibility and wire framing contracts.

mod capabilities;
mod control;
mod envelope;
mod frame;
mod reconnect;
mod request;
mod stream;

pub use capabilities::{
    Capability, CapabilitySet, ProtocolVersion, VersionNegotiationError, PROTOCOL_MAJOR,
    PROTOCOL_MINOR,
};
pub use control::{DetachAck, DetachRequest};
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
pub use reconnect::ReconnectGrant;
pub use request::{ClientRequest, ServerMessage};
pub use stream::{StreamFrame, StreamKey, StreamPayloadKind};
