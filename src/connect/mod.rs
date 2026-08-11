//! Contract-first Connect core. Network, identity, crypto, relay, and UI
//! implementations are intentionally later phase gates.

mod envelope;
mod identity;
mod identity_codec;
mod identity_store;
mod permission;
mod presence;
mod schema;
mod transport;

#[cfg(test)]
mod identity_tests;

pub use envelope::{
    ChannelBinding, ChannelId, ChannelKind, ChunkContext, Compression, ConnectEnvelope,
    ConnectIdError, ConnectLimitError, ConnectLimitField, ConnectLimits, ConnectPrivacyClass,
    ConnectionId, EnvelopeError, NegotiatedLimits, PayloadKind, PrivacyClass, SessionId,
    CONNECT_PROTOCOL_MAJOR, CONNECT_PROTOCOL_MINOR, MAX_CONNECT_CHUNK_BYTES,
    MAX_CONNECT_CUMULATIVE_BYTES, MAX_CONNECT_CURSOR_BYTES, MAX_CONNECT_PAGE_ENCODED_BYTES,
    MAX_CONNECT_PAGE_ITEMS, MAX_CONNECT_PHYSICAL_FRAME_BYTES,
    MAX_CONNECT_REASSEMBLED_MESSAGE_BYTES,
};
pub use identity::{
    bind_device_credential, validate_device_credential, BrowserDeviceDto, BrowserPrivateStorage,
    ConnectIdentity, CredentialLocation, CredentialVault, DeviceCredentialProof, DeviceId,
    DeviceKeyProof, DeviceKind, DeviceRecord, HostIdentityRotation, HostKeyProof, HostPublicId,
    IdentityCommand, IdentityError, IdentityLimitField, IdentityOp, IdentityReceipt, IdentitySetup,
    KeyReference, MachineBinding, PairingCode, PairingPurpose, RegisterDevice, RepairDevice,
    CONNECT_IDENTITY_SCHEMA_VERSION, IDENTITY_CODEC_VERSION, MAX_FINGERPRINT_BYTES,
    MAX_IDENTITY_ARRAY_ITEMS, MAX_IDENTITY_DEVICES, MAX_IDENTITY_MAP_ENTRIES, MAX_IDENTITY_NESTING,
    MAX_IDENTITY_PHYSICAL_BYTES, MAX_IDENTITY_RECEIPTS, MAX_ID_BYTES, MAX_LABEL_BYTES,
    PAIRING_CODE_LEN,
};
pub use identity_store::{
    IdentityPersistence, InMemoryIdentityPersistence, IsolatedRemoteStore, LoadedRemoteDocument,
};
pub use permission::{
    ActionId, ConnectRole, KnownAction, PermissionDecision, PermissionDenyReason,
    PermissionEvaluator, PermissionRequest,
};
pub use presence::{EphemeralPresence, LastSenderHint, PresenceSink};
pub use schema::{
    canonical_artifact_content_page_size, canonical_event_page_size, canonical_snapshot_page_size,
    payload_catalog, ChunkFrame, ConnectPayload, ErrorPayload, GenericExtensionPayload,
    HelloPayload, KnownPayloadKind, OperationSettlementPayload, PayloadDescriptor, PayloadError,
    UnknownPayload, CONNECT_PAYLOAD_SCHEMA_VERSION, PAYLOAD_CATALOG,
};
pub use transport::{
    BrowserExtensionDescriptor, ConnectRoute, ConnectTransport, ConnectTransportError,
    FramedConnectTransport, ProjectionError, ProjectionExtensions, ProjectionResponse,
    ProjectionSource, PromptExtensionDescriptor, ReplayRequest, SnapshotRequest,
    MAX_CONNECT_RESUME_CURSOR_BYTES,
};
