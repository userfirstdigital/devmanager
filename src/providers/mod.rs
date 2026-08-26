//! Provider-neutral boundaries for stock CLI discovery, capability facts,
//! and the semantic journal contract.

pub mod adapter;
pub mod capabilities;
pub mod claude;
pub mod codex;
pub mod conformance;
pub mod controller;
pub mod cursor;
pub mod dispatch;
pub mod hook_bridge;
pub mod host;
pub mod input;
pub mod journal;
pub mod orchestrator;
pub mod quota;
pub mod quota_runtime;
pub mod registry;
pub mod session;
pub mod startup;

// Test-only provider identities are copied from the real test executable so
// the production attestation invariant remains exercised on every platform.
#[cfg(test)]
pub(crate) mod test_support;

pub use adapter::{
    AdapterDeliveryPermit, AdapterIngressUnavailable, JournalNormalizeError, LaunchProviderRequest,
    NormalizedAdapterDelivery, ProviderAccessMode, ProviderAdapter, ProviderArgument,
    ProviderError, ProviderInput, ProviderInputError, ProviderLaunchOptions, ProviderLaunchSpec,
    ProviderModel, ProviderProbeError, ProviderProbeFailureCode, ProviderProbeIoError,
    ProviderProbeKind, ProviderProbeRequest, ProviderProbeRequestError, ProviderProbeResult,
    ProviderProbeRunner, ProviderProbeStatus, ProviderQuotaStatus, ProviderReasoningEffort,
    ProviderRuntime, ProviderSignal, QuotaObservation, StopStrategy, WindowsProviderProbeRunner,
    MAX_PROVIDER_ARGUMENTS, MAX_PROVIDER_ARGUMENT_BYTES, MAX_PROVIDER_INPUT_BYTES,
    MAX_PROVIDER_PROBE_OUTPUT_BYTES, MAX_PROVIDER_PROBE_TIMEOUT, MAX_PROVIDER_SIGNAL_BYTES,
};
pub use capabilities::{
    AdapterRevision, CapabilityEvidence, CapabilityEvidenceError, CapabilityState,
    CapabilityStatus, CapabilitySupport, EvidenceConfidence, EvidenceDiagnostic,
    EvidenceDiagnosticCode, EvidenceSourceId, EvidenceStatus, ProviderAuthClock,
    ProviderAuthEvidenceError, ProviderAuthEvidenceReceipt, ProviderAuthEvidenceRegistry,
    ProviderAuthEvidenceSource, ProviderAuthProbeInvocation, ProviderAuthProbeResult,
    ProviderAuthState, ProviderCapabilities, ProviderCapabilitiesError, ProviderCapability,
    ProviderDiscoveryCandidate, ProviderDiscoveryCandidateInput, ProviderDiscoveryContract,
    ProviderDiscoveryError, ProviderDiscoveryOrigin, ProviderExecutable, ProviderExecutableError,
    ProviderExecutableForm, ProviderExecutableHandle, ProviderExecutablePolicy,
    ProviderExecutablePolicyError, ProviderExecutablePolicyViolation, ProviderFileIdentity,
    ProviderKind, ProviderPathSnapshot, ProviderVersion, ProviderVersionError,
    SemanticSchemaVersion, SystemProviderAuthClock, MAX_CAPABILITY_EVIDENCE_ITEMS,
    MAX_EXECUTABLE_ENTRYPOINT_BYTES, MAX_PROVIDER_AUTH_ACCEPTED_ENTRIES,
    MAX_PROVIDER_AUTH_PENDING_ENTRIES, MAX_PROVIDER_AUTH_TTL,
    MAX_PROVIDER_CAPABILITY_CACHE_ENTRIES, MAX_PROVIDER_PATH_BYTES, MAX_PROVIDER_PATH_ENTRIES,
    MAX_PROVIDER_PATH_VALUE_BYTES, MAX_PROVIDER_SHIM_BYTES, MAX_PROVIDER_VERSION_BYTES,
    PROVIDER_AUTH_ADAPTER_REVISION, PROVIDER_AUTH_NONCE_BYTES,
    PROVIDER_AUTH_SEMANTIC_SCHEMA_VERSION, PROVIDER_CACHE_KEY_SCHEMA_VERSION,
    PROVIDER_CAPABILITY_SCHEMA_VERSION, PROVIDER_EVIDENCE_SCHEMA_VERSION,
    PROVIDER_EXECUTABLE_SCHEMA_VERSION, PROVIDER_FILE_IDENTITY_SCHEMA_VERSION,
    PROVIDER_OBSERVATION_SCHEMA_VERSION,
};
pub use claude::ClaudeCodeAdapter;
pub use codex::CodexAdapter;
pub use controller::{StockProviderSessionController, StockProviderSessionError};
pub use cursor::CursorAdapter;
pub use host::{
    admit_specialist_start, cancel_specialist_with_authority, correlate_specialist_authority,
    deliver_claimed_provider_input, deliver_live_provider_input, HostAiLaunchAdmission,
    HostLaunchError, ProviderHost, SpecialistLifecycleLineage, SpecialistProcessAuthority,
    SpecialistResultLineage,
};
pub use input::{
    BoundProviderInputPort, ProviderInputBridgeHold, ProviderInputDeliveryError,
    ProviderInputDeliveryIdentity, ProviderInputDeliveryPlan, ProviderInputWriteReceipt,
    ProviderRuntimeWriteHandle,
};
pub use journal::{
    stock_adapter_ingress, stock_adapter_ingress_available, JournalBackpressure, JournalEvent,
    JournalIngestOutcome, JournalLimits, JournalRedactionClass, JournalRejectReason,
    JournalSemanticKind, JournalVisibility, SemanticJournal, JOURNAL_SCHEMA_VERSION,
};
pub use quota::{
    canonical_top_bar, AdapterQuotaSource, CanonicalQuotaBar, ProductionJitter, ProviderQuotaHost,
    QuotaCacheKey, QuotaDiagnostic, QuotaJitterError, QuotaObserver, QuotaObserverConfig,
    QuotaProbeLimiter, QuotaState, QuotaStripEntry, QuotaView,
};
pub use quota_runtime::{
    NativeQuotaHost, QuotaRuntimeConfig, QuotaRuntimeError, SystemQuotaClock,
    QUOTA_RUNTIME_SHUTDOWN_TIMEOUT,
};
pub use registry::{
    CacheStatus, CapabilityCacheKey, ExecutableInspector, FileSystemExecutableInspector,
    ProviderDiscoveryConfig, ProviderObservation, ProviderRegistry,
};
pub use session::{
    ProviderSessionManager, ProviderSessionStartMode, UnavailableProviderProcessLauncher,
};
pub use startup::{
    register_stock_adapters, registered_stock_kinds, start_request_from_adapter,
    start_request_from_adapter_with_options, stock_provider_registry, ProviderBridgeError,
    STOCK_PROVIDER_REGISTRATION_ORDER,
};

pub use crate::domain::{ProviderSessionId, ProviderSessionIdError, MAX_PROVIDER_SESSION_ID_BYTES};
