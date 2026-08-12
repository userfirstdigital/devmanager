//! Contract-first Connect core. Network, identity, pairing persistence, and UI
//! implementations remain later phase gates. Source-level end-to-end channel
//! and opaque relay tickets live here; production Noise stays on HOLD.

mod crypto;
mod envelope;
mod evidence;
mod local_actions;
mod managed;
mod org;
mod org_prompts;
mod epoch;
mod failure;
mod identity;
mod identity_codec;
mod identity_store;
mod invites;
mod permission;
mod policy;
mod presence;
mod projection;
mod push;
mod relay;
mod schema;
mod telemetry;
#[cfg(test)]
mod telemetry_tests;
mod transport;
mod watcher;
mod session;
mod update;

#[cfg(test)]
mod identity_tests;

pub use crypto::{
    connect_prologue, lock_noise_pattern, preferred_connect_route, ConnectChannelKey,
    ConnectChannelRole, ConnectCredentialPurpose, ConnectCryptoError, ConnectCryptoHold,
    ConnectCryptoHoldReason, ConnectCryptoPrologue, ConnectSealedFrame, EndToEndChannel,
    CONNECT_CRYPTO_PRODUCTION_READY, CONNECT_NOISE_FIRST_PAIRING_PATTERN,
    CONNECT_NOISE_PINNED_DEVICE_PATTERN,
};
pub use envelope::{
    ChannelBinding, ChannelId, ChannelKind, ChunkContext, Compression, ConnectEnvelope,
    ConnectHostId, ConnectIdError, ConnectLimitError, ConnectLimitField, ConnectLimits, ConnectPrivacyClass,
    ConnectionId, EnvelopeError, NegotiatedLimits, PayloadKind, PrivacyClass, SessionId,
    CONNECT_PROTOCOL_MAJOR, CONNECT_PROTOCOL_MINOR, MAX_CONNECT_CHUNK_BYTES,
    MAX_CONNECT_CUMULATIVE_BYTES, MAX_CONNECT_CURSOR_BYTES, MAX_CONNECT_DIAGNOSTIC_BYTES,
    MAX_CONNECT_PAGE_ENCODED_BYTES, MAX_CONNECT_PAGE_ITEMS, MAX_CONNECT_PHYSICAL_FRAME_BYTES,
    MAX_CONNECT_REASSEMBLED_MESSAGE_BYTES,
};
pub use evidence::{EvidenceBundle, EvidenceIntake, TaskDraft};
pub use local_actions::{LocalActionReceipt, LocalActionRegistry, LocalActionRequest};
pub use managed::{ManagedTaskLink, ManagedTaskProjection, TaskLinkReducer};
pub use org::{OrganizationProjection, StandaloneOrganization};
pub use org_prompts::{ComposerInsertion, OrganizationPromptProjection};
pub use watcher::{FleetWatcherView, TaskWatcherView, WatcherProjection};
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
pub use epoch::{ActionEpoch, FocusEpoch, RuntimeGeneration, TurnEpoch};
pub use failure::{
    matrix_covers_direct_and_hosted, simulate_fault, ConnectActor, ConnectRouteKind, ConnectSurface,
    FailureCase, FailureClass, FailureExpectation, SimulatedFaultOutcome, FAILURE_MATRIX,
};
pub use identity_store::{
    IdentityPersistence, InMemoryIdentityPersistence, IsolatedRemoteStore, LoadedRemoteDocument,
};
pub use invites::{
    guest_may_perform, ContentClass, InviteAuditEvent, InviteAuditKind, InviteError, InviteGrantView,
    InviteRole, InviteUsePolicy, IssuedInvite, PinnedHostPublicId, RedeemedDevicePublicId,
    TaskInviteStore, INVITE_SECRET_BYTES, MAX_INVITE_NICKNAME_BYTES, MAX_TASK_INVITES,
};
pub use permission::{
    ActionId, AuthoritativePermissionContext, ConnectRole, KnownAction, PermissionDecision,
    PermissionDenyReason, PermissionEvaluator, PermissionRequest, ScopedPermissionGrant,
};
pub use policy::{
    ActiveSessionInterval, ActiveSessionIntervalError, ContentClass as ManagementContentClass,
    DeniedContentClass,
    GrantError, ManagedField, ManagementGrant, ManagementPolicy, ManagementPrivacyClass,
    ManagementRole, MetadataField, PolicyAuthority, PolicyDecision, PolicyOperation,
    PolicyPrincipal, PolicyPrivacyClass, PolicyReasonCode, TaskContext, TaskEnrollment,
    ACTIVE_SESSION_IDLE_LIMIT_MS,
};
pub use presence::{EphemeralPresence, LastSenderHint, PresenceSink};
pub use projection::{
    project_field, project_object, ConnectEnrollment, OutboundField, ProjectedObject,
    ProjectionDenyReason, ProjectionGrant,
};
pub use push::{
    forbidden_push_fields, sanitize_push, AttentionKind, PushPolicy, PushSanitizeError,
    SanitizedPush, MAX_ROUTE_BYTES, MAX_SAFE_TITLE_BYTES,
};
pub use session::{
    ActionAnswer, ConnectSession, DeviceInput, SessionAdmitError, SessionReceipt, SessionReceiptKind,
};
pub use update::{
    PairingContinuity, UpdateContinuity, UpdateContinuityError,
};
pub use relay::{
    AccountId, DevicePublicId, HostPublicId, OpaqueRelay, RateKey, RelayError, RelayObservation,
    RelayStatus, RouteId, RouteTicket, SignedRouteTicket, TicketAudience, TicketId,
    TicketSigningKey, BIND_RATE_WINDOW_SECS, MAX_BIND_ATTEMPTS_PER_WINDOW, MAX_RELAY_QUEUE_BYTES,
    MAX_RELAY_QUEUE_FRAMES, MAX_ROUTE_TICKET_TTL_SECS, PRESENCE_TTL_SECS, ROUTE_TICKET_DOMAIN,
};
pub use schema::{
    canonical_schema_fixtures, catalog_entry, encode_canonical_schema, payload_catalog,
    CanonicalSchemaFixture, ChunkPayload, ConnectPayload, ErrorPayload, GenericExtensionPayload,
    HelloPayload, OperationSettlementPayload, PayloadDecodeError, PayloadDescriptor, ResyncPayload,
    ResyncReason, StreamDeltaPayload, CONNECT_PAYLOAD_SCHEMA_VERSION,
};
pub use telemetry::{
    encode_observation, ObservationAuthority, ObservationCompleteness, ObservationConfidence,
    ObservationCursor, ObservationDependency, ObservationError, ObservationFreshness,
    ObservationId, ObservationMessageClass, ObservationPage, ObservationRecord, ObservationReducer,
    ObservationSchema, PageBudget, ProviderObservation, QualifyingActivity, ReduceOutcome,
    RestrictiveGitSummary, TaskObservationFacts, UsageKind, UsageMeasure, UsageProvenance,
    ACTIVE_SESSION_TIME_LABEL, MAX_ACTIVITIES_PER_TASK, MAX_OBSERVATION_DOCUMENT_BYTES,
    MAX_OBSERVATION_RETENTION_MS, MAX_OBSERVATION_TASKS, MAX_READY_INTERVALS, MAX_SPECIALISTS,
    OBSERVATION_SCHEMA_REVISION, OBSERVATION_STALE_AFTER_MS,
};
pub use transport::{
    decode_inner, encode_inner, validate_event_page, validate_snapshot_page,
    BrowserExtensionDescriptor, ConnectRoute, ConnectTransport, ConnectTransportError,
    FramedConnectTransport, ProjectionError, ProjectionExtensions, ProjectionResponse,
    ProjectionSource, PromptExtensionDescriptor, ReplayRequest, SnapshotRequest,
    MAX_CONNECT_RESUME_CURSOR_BYTES,
};
