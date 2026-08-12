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
