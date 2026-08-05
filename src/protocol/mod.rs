//! Transport-neutral protocol compatibility and wire framing contracts.

mod capabilities;
mod frame;

pub use capabilities::{
    Capability, CapabilitySet, ProtocolVersion, VersionNegotiationError, PROTOCOL_MAJOR,
    PROTOCOL_MINOR,
};
pub use frame::{
    FrameLimitField, FrameLimits, FrameLimitsError, PhysicalFrameCodec, PhysicalFrameError,
};
