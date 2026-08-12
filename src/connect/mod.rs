//! Local-first Connect core. Identity persistence binds to the profile kernel
//! store, and production Noise XX/IK uses pinned `snow` with vault-supplied
//! static keys. Source-level sealing exists only for contract tests.

mod crypto;
mod deletion_ledger;
mod direct;
mod envelope;
mod epoch;
mod evidence;
mod failure;
mod identity;
mod identity_codec;
mod identity_store;
mod invites;
mod local_actions;
mod managed;
mod org;
mod org_prompts;
mod permission;
mod permissions;
mod policy;
mod presence;
mod projection;
mod push;
mod relay;
mod schema;
mod session;
mod telemetry;
mod transport;
mod update;
mod watcher;

pub use crypto::{
    connect_prologue, lock_noise_pattern, preferred_connect_route, ConnectAuthenticatedPeer,
    ConnectChannelKey, ConnectChannelRole, ConnectCredentialPurpose, ConnectCryptoError,
    ConnectCryptoHold, ConnectCryptoHoldReason, ConnectCryptoPrologue, ConnectNoiseCustody,
    ConnectNoiseHandshake, ConnectNoiseHandshakeMessage, ConnectNoiseIdentityBinding,
    ConnectNoiseStaticPrivateKey, ConnectNoiseStaticPublicKey, ConnectSealedFrame, EndToEndChannel,
    CONNECT_CRYPTO_PRODUCTION_READY, CONNECT_NOISE_FIRST_PAIRING_PATTERN,
    CONNECT_NOISE_PINNED_DEVICE_PATTERN,
};
pub use deletion_ledger::{DeletionLedgerEntry, DeletionStatus, DELETION_LEDGER};
pub use direct::{
    admit_direct_request, is_trustworthy_loopback_host, query_contains_pairing_secret,
    referer_contains_pairing_secret, security_headers, DirectAdmitError, DirectBindMode,
    DirectBindPolicy, DirectPairingExchange, DirectPairingLimiter, DirectPairingThrottle,
    DirectRequestView, MAX_DIRECT_FRAME_BYTES, MAX_DIRECT_PAIRING_BODY_BYTES,
};
pub use envelope::{
    ChannelBinding, ChannelId, ChannelKind, ChunkContext, Compression, ConnectEnvelope,
    ConnectHostId, ConnectIdError, ConnectLimitError, ConnectLimitField, ConnectLimits,
    ConnectPrivacyClass, ConnectionId, EnvelopeError, NegotiatedLimits, PayloadKind, PrivacyClass,
    SessionId, CONNECT_PROTOCOL_MAJOR, CONNECT_PROTOCOL_MINOR, MAX_CONNECT_CHUNK_BYTES,
    MAX_CONNECT_CUMULATIVE_BYTES, MAX_CONNECT_CURSOR_BYTES, MAX_CONNECT_DIAGNOSTIC_BYTES,
    MAX_CONNECT_PAGE_ENCODED_BYTES, MAX_CONNECT_PAGE_ITEMS, MAX_CONNECT_PHYSICAL_FRAME_BYTES,
    MAX_CONNECT_REASSEMBLED_MESSAGE_BYTES,
};
pub use epoch::{ActionEpoch, FocusEpoch, RuntimeGeneration, TurnEpoch};
pub use evidence::{
    EvidenceAccessClass, EvidenceAdapter, EvidenceBundle, EvidenceIntake,
    EvidenceMetadataProjection, TaskDraft,
};
pub use failure::{
    matrix_covers_direct_and_hosted, simulate_fault, ConnectActor, ConnectRouteKind,
    ConnectSurface, FailureCase, FailureClass, FailureExpectation, SimulatedFaultOutcome,
    FAILURE_MATRIX,
};
#[cfg(test)]
pub(crate) use identity::bind_device_credential_from_snapshot as bind_device_credential;
pub use identity::{
    validate_device_credential, BrowserDeviceDto, BrowserPrivateStorage, ConnectIdentity,
    CredentialLocation, CredentialVault, DeviceCredentialProof, DeviceEstablishmentHandle,
    DeviceId, DeviceKeyProof, DeviceKind, DeviceRecord, DeviceRepairHandle,
    HostEstablishmentHandle, HostIdentityRotation, HostKeyProof, HostPublicId, HostRotationHandle,
    IdentityCommand, IdentityError, IdentityLimitField, IdentityOp, IdentityReceipt, IdentitySetup,
    KeyReference, MachineBinding, PairingCode, PairingPurpose, RegisterDevice, RepairDevice,
    CONNECT_IDENTITY_SCHEMA_VERSION, IDENTITY_CODEC_VERSION, MAX_FINGERPRINT_BYTES,
    MAX_IDENTITY_ARRAY_ITEMS, MAX_IDENTITY_DEVICES, MAX_IDENTITY_MAP_ENTRIES, MAX_IDENTITY_NESTING,
    MAX_IDENTITY_PHYSICAL_BYTES, MAX_IDENTITY_RECEIPTS, MAX_ID_BYTES, MAX_LABEL_BYTES,
    PAIRING_CODE_LEN,
};
#[cfg(test)]
pub use identity_store::InMemoryIdentityPersistence;
pub use identity_store::{
    ConnectIdentityLiveState, ConnectListenerKind, ConnectProductionError,
    ConnectProductionSession, ConnectProductionStartup, ConnectStartupError, IdentityPersistence,
    IsolatedRemoteStore, KernelIdentityPersistence, LoadedRemoteDocument, OsNoiseCustody,
    OsNoiseCustodyError,
};
pub use invites::{
    guest_may_perform, ContentClass, InviteAuditEvent, InviteAuditKind, InviteError,
    InviteGrantView, InviteRole, InviteUsePolicy, IssuedInvite, PinnedHostPublicId,
    RedeemedDevicePublicId, TaskInviteStore, INVITE_SECRET_BYTES, MAX_INVITE_AUDIT_EVENTS,
    MAX_INVITE_LIFETIME_MS, MAX_INVITE_NICKNAME_BYTES, MAX_TASK_INVITES,
};
pub use local_actions::{
    LocalActionAdapter, LocalActionAdmissionState, LocalActionCatalogEntry, LocalActionKind,
    LocalActionReceipt, LocalActionReconcileState, LocalActionRegistry, LocalActionRequest,
    ReplayPolicy,
};
pub use managed::{
    ManagedTaskAdapter, ManagedTaskLink, ManagedTaskProjection, ManagedTaskSnapshot,
    TaskLinkReducer,
};
pub use org::{
    OrganizationAdapter, OrganizationCapabilityDisableReason, OrganizationCapabilityState,
    OrganizationFact, OrganizationProjection, OrganizationPublisher, OrganizationStateStore,
    OrganizationSyncState, SignedOrganizationEnvelope, StandaloneOrganization, SyncOutcome,
};
pub use org_prompts::{
    ComposerInsertion, OrganizationPromptAdapter, OrganizationPromptProjection,
    OrganizationPromptSnapshot,
};
pub use permission::{
    admit_connect_action, resolve_host_capability_grant, ActionId, AuthoritativePermissionContext,
    ConnectAdmission, ConnectRole, HostCapabilityGrant, HostConnectAction, HostConnectRole,
    KnownAction, PermissionDecision, PermissionDenyReason, PermissionEvaluator, PermissionRequest,
    ScopedPermissionGrant,
};
pub use permissions::{action_for_client_request, SessionAuthorizer, SessionPermissionContext};
pub use policy::{
    ActiveSessionInterval, ActiveSessionIntervalError, ContentClass as ManagementContentClass,
    DeniedContentClass, GrantError, ManagedField, ManagementGrant, ManagementPolicy,
    ManagementPrivacyClass, ManagementRole, MetadataField, PolicyAuthority, PolicyDecision,
    PolicyOperation, PolicyPrincipal, PolicyPrivacyClass, PolicyReasonCode, TaskContext,
    TaskEnrollment, ACTIVE_SESSION_IDLE_LIMIT_MS,
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
pub use relay::{
    AccountId, ConnectRelayClient, ConnectRelaySocket, DevicePublicId,
    HostPublicId as RelayHostPublicId, OpaqueRelay, RateKey, RelayEndpoint, RelayError,
    RelayObservation, RelayStatus, RouteId, RouteTicket, SignedRouteTicket, TicketAudience,
    TicketId, TicketSigningKey, BIND_RATE_WINDOW_SECS, MAX_BIND_ATTEMPTS_PER_WINDOW,
    MAX_RELAY_CONSUMED_NONCES, MAX_RELAY_QUEUE_BYTES, MAX_RELAY_QUEUE_FRAMES, MAX_RELAY_RATE_KEYS,
    MAX_RELAY_REVOKED_DEVICES, MAX_RELAY_REVOKED_TICKETS, MAX_RELAY_ROUTES,
    MAX_RELAY_ENDPOINT_BYTES,
    MAX_ROUTE_TICKET_TTL_SECS, PRESENCE_TTL_SECS, RELAY_INITIAL_BACKOFF_MS, RELAY_MAX_BACKOFF_MS,
    ROUTE_TICKET_DOMAIN,
};
pub use schema::{
    canonical_schema_fixtures, catalog_entry, encode_canonical_schema, payload_catalog,
    CanonicalSchemaFixture, ChunkPayload, ConnectPayload, ErrorPayload, GenericExtensionPayload,
    HelloPayload, OperationSettlementPayload, PayloadDecodeError, PayloadDescriptor, ResyncPayload,
    ResyncReason, StreamDeltaPayload, CONNECT_PAYLOAD_SCHEMA_VERSION,
};
pub use session::{
    ActionAnswer, ConnectSession, DeviceInput, SessionAdmitError, SessionReceipt,
    SessionReceiptKind, MAX_SESSION_ACCEPTED_COMMANDS, MAX_SESSION_CONNECTED,
    MAX_SESSION_INVALIDATED, MAX_SESSION_OUTSTANDING, MAX_SESSION_QUEUED, MAX_SESSION_RESOURCES,
    MAX_SESSION_SETTLED,
};
pub use telemetry::{
    encode_observation, ObservationAuthority, ObservationCompleteness, ObservationConfidence,
    ObservationCursor, ObservationDependency, ObservationError, ObservationFreshness,
    ObservationId, ObservationMessageClass, ObservationOutboxIntent, ObservationPage,
    ObservationRecord, ObservationReducer, ObservationSchema, ObservationSyncIntent, PageBudget,
    ProviderObservation, QualifyingActivity, ReduceOutcome, RestrictiveGitSummary,
    TaskObservationFacts, UsageKind, UsageMeasure, UsageProvenance, ACTIVE_SESSION_TIME_LABEL,
    MAX_ACTIVITIES_PER_TASK, MAX_OBSERVATION_DOCUMENT_BYTES, MAX_OBSERVATION_OUTBOX_INTENTS,
    MAX_OBSERVATION_RETENTION_MS, MAX_OBSERVATION_TASKS, MAX_READY_INTERVALS, MAX_SPECIALISTS,
    OBSERVATION_SCHEMA_REVISION, OBSERVATION_STALE_AFTER_MS,
};
pub use transport::{
    decode_inner, encode_inner, select_connect_route, validate_advertised_relay_url,
    validate_event_page, validate_snapshot_page, BrowserExtensionDescriptor, ConnectNoRouteReason,
    ConnectRoute, ConnectTransport, ConnectTransportError, FramedConnectTransport, ProjectionError,
    ProjectionExtensions, ProjectionResponse, ProjectionSource, PromptExtensionDescriptor,
    ReplayRequest, SealedFramedConnectTransport, SelectedConnectRoute, SnapshotRequest,
    MAX_ADVERTISED_RELAY_URL_BYTES, MAX_CONNECT_RESUME_CURSOR_BYTES,
};
pub use update::{PairingContinuity, UpdateContinuity, UpdateContinuityError};
pub use watcher::{FleetWatcherView, TaskWatcherView, WatcherProjection};
