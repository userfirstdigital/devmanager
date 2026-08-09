//! Contract-first Connect core. Network, identity, crypto, relay, and UI
//! implementations are intentionally later phase gates.

mod envelope;
mod identity;
mod identity_codec;
mod identity_store;
mod permission;
mod policy;
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
    validate_device_credential, BrowserDeviceDto, BrowserPrivateStorage,
    ConnectIdentity, CredentialLocation, CredentialVault, DeviceCredentialProof, DeviceId,
    DeviceEstablishmentHandle, DeviceKeyProof, DeviceKind, DeviceRecord, DeviceRepairHandle,
    HostEstablishmentHandle, HostIdentityRotation, HostKeyProof, HostPublicId,
    HostRotationHandle, IdentityCommand, IdentityError,
    IdentityLimitField, IdentityOp, IdentityReceipt, IdentitySetup, KeyReference, MachineBinding,
    PairingCode, PairingPurpose, RegisterDevice, RepairDevice,
    CONNECT_IDENTITY_SCHEMA_VERSION, IDENTITY_CODEC_VERSION, MAX_FINGERPRINT_BYTES,
    MAX_IDENTITY_ARRAY_ITEMS, MAX_IDENTITY_DEVICES, MAX_IDENTITY_MAP_ENTRIES, MAX_IDENTITY_NESTING,
    MAX_IDENTITY_PHYSICAL_BYTES, MAX_IDENTITY_RECEIPTS, MAX_ID_BYTES, MAX_LABEL_BYTES,
    PAIRING_CODE_LEN,
};
#[cfg(test)]
pub(crate) use identity::bind_device_credential_from_snapshot as bind_device_credential;
pub use identity_store::{
    IdentityPersistence, InMemoryIdentityPersistence, IsolatedRemoteStore, LoadedRemoteDocument,
};
pub use permission::{
    ActionId, AuthoritativePermissionContext, ConnectRole, KnownAction, PermissionDecision,
    PermissionDenyReason, PermissionEvaluator, PermissionRequest, ScopedPermissionGrant,
};
pub use policy::{
    ActiveSessionInterval, ActiveSessionIntervalError, ContentClass, DeniedContentClass,
    GrantError, ManagedField, ManagementGrant, ManagementPolicy, ManagementPrivacyClass,
    ManagementRole, MetadataField, PolicyAuthority, PolicyDecision, PolicyOperation,
    PolicyPrincipal, PolicyPrivacyClass, PolicyReasonCode, TaskContext, TaskEnrollment,
    ACTIVE_SESSION_IDLE_LIMIT_MS,
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
