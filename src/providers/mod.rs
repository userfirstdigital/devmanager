//! Provider-neutral boundaries for stock CLI discovery and capability facts.
//!
//! This module deliberately stops before launching or owning a provider
//! runtime. Later phases can wire these typed observations into the kernel and
//! process services without replacing a provider's native harness.

pub mod adapter;
pub mod capabilities;
pub mod claude;
pub mod cursor;
pub mod registry;
pub mod session;

pub use adapter::{
    JournalEvent, LaunchProviderRequest, ProviderAdapter, ProviderArgument, ProviderError,
    ProviderInput, ProviderInputError, ProviderLaunchSpec, ProviderProbeError,
    ProviderProbeFailureCode, ProviderProbeIoError, ProviderProbeKind, ProviderProbeRequest,
    ProviderProbeRequestError, ProviderProbeResult, ProviderProbeRunner, ProviderProbeStatus,
    ProviderQuotaStatus, ProviderRuntime, ProviderSignal, QuotaObservation, StopStrategy,
    WindowsProviderProbeRunner, MAX_PROVIDER_ARGUMENTS, MAX_PROVIDER_ARGUMENT_BYTES,
    MAX_PROVIDER_INPUT_BYTES, MAX_PROVIDER_PROBE_OUTPUT_BYTES, MAX_PROVIDER_PROBE_TIMEOUT,
    MAX_PROVIDER_SIGNAL_BYTES,
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
pub use cursor::CursorAdapter;
pub use registry::{
    CacheStatus, CapabilityCacheKey, ExecutableInspector, FileSystemExecutableInspector,
    ProviderDiscoveryConfig, ProviderObservation, ProviderRegistry,
};

pub use crate::domain::{ProviderSessionId, ProviderSessionIdError, MAX_PROVIDER_SESSION_ID_BYTES};
