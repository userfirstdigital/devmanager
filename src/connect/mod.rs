//! Contract-first Connect core. Network, identity, crypto, relay, and UI
//! implementations are intentionally later phase gates.

mod envelope;
mod permission;
mod presence;
mod schema;
mod transport;

pub use crate::protocol::{ChunkError, ChunkLimitField, ChunkLimits, ChunkLimitsError};
pub use envelope::{
    ChannelBinding, ChannelId, ChannelKind, Compression, ConnectEnvelope, ConnectIdError,
    ConnectLimitError, ConnectLimitField, ConnectLimits, ConnectPrivacyClass, ConnectionId,
    EnvelopeError, NegotiatedLimits, PayloadKind, PrivacyClass, SessionId, CONNECT_PROTOCOL_MAJOR,
    CONNECT_PROTOCOL_MINOR, MAX_CONNECT_CHUNK_BYTES, MAX_CONNECT_CUMULATIVE_BYTES,
    MAX_CONNECT_CURSOR_BYTES, MAX_CONNECT_PAGE_ENCODED_BYTES, MAX_CONNECT_PAGE_ITEMS,
    MAX_CONNECT_PHYSICAL_FRAME_BYTES, MAX_CONNECT_REASSEMBLED_MESSAGE_BYTES,
};
pub use permission::{
    ActionId, ConnectRole, KnownAction, PermissionDecision, PermissionDenyReason,
    PermissionEvaluator, PermissionRequest,
};
pub use presence::{EphemeralPresence, LastSenderHint, PresenceSink};
pub use schema::{
    canonical_artifact_content_page_size, canonical_event_page_size, canonical_snapshot_page_size,
    payload_catalog, ChunkContext, ChunkFrame, ConnectPayload, ErrorPayload,
    GenericExtensionPayload, HelloPayload, KnownPayloadKind, OperationSettlementPayload,
    PayloadDescriptor, PayloadError, UnknownPayload, CONNECT_PAYLOAD_SCHEMA_VERSION,
    PAYLOAD_CATALOG,
};
pub use transport::{
    decode_inner, encode_inner, validate_event_page, validate_snapshot_page,
    BrowserExtensionDescriptor, ConnectRoute, ConnectTransport, ProjectionError,
    ProjectionExtensions, ProjectionResponse, ProjectionSource, PromptExtensionDescriptor,
    ReplayRequest, SnapshotRequest, MAX_CONNECT_RESUME_CURSOR_BYTES,
};
