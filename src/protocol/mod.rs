//! Transport-neutral protocol compatibility and wire framing contracts.

mod capabilities;
mod envelope;
mod frame;

pub use capabilities::{
    Capability, CapabilitySet, ProtocolVersion, VersionNegotiationError, PROTOCOL_MAJOR,
    PROTOCOL_MINOR,
};
pub use envelope::{
    ClientBuildError, ClientHello, ClientHelloError, MessagePackCodec, MessagePackError,
    MessagePackLengthKind, NegotiatedParameters, MAX_CLIENT_BUILD_BYTES,
    MAX_MESSAGEPACK_COLLECTION_ITEMS, MAX_MESSAGEPACK_DEPTH, MAX_MESSAGEPACK_VALUES,
};
pub use frame::{
    FrameLimitField, FrameLimits, FrameLimitsError, PhysicalFrameCodec, PhysicalFrameError,
};
