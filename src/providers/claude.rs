//! Stock Claude Code CLI adapter and exact current-generation hook correlation.
//!
//! `probe` and `build_launch` are real adapter methods. Identity is accepted
//! only after the existing authenticated loopback Claude relay admits the
//! current registration. Hook JSON is bounded before serde.

use crate::ai::claude_hooks::{
    physically_bound_claude_hook_json, ClaudeBindingField, ClaudeCorrelatedIngestError,
    ClaudeHookJsonBound, ClaudeHookRegistry,
};
use crate::domain::{
    AgentSessionId, ProviderSessionId, ProviderSessionIdError, ResourceId, TaskId,
};
use crate::providers::adapter::{
    AdapterDeliveryPermit, AdapterIngressUnavailable, JournalNormalizeError, LaunchProviderRequest,
    NormalizedAdapterDelivery, ProviderAdapter, ProviderArgument, ProviderError,
    ProviderLaunchSpec, ProviderProbeError, ProviderProbeIoError, ProviderProbeKind,
    ProviderProbeRequest, ProviderProbeRunner, ProviderProbeStatus, ProviderRuntime,
    QuotaObservation, StopStrategy,
};
use crate::providers::capabilities::{
    CapabilityEvidence, CapabilityEvidenceError, CapabilitySupport, EvidenceSourceId,
    EvidenceStatus, ProviderAuthState, ProviderCapabilities, ProviderCapabilitiesError,
    ProviderCapability, ProviderExecutable, ProviderExecutableHandle, ProviderKind,
    ProviderVersion,
};
use crate::providers::registry::ProviderObservation;
use crate::remote::presentation::StableSessionKey;
use async_trait::async_trait;
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

pub const CLAUDE_LAUNCH_NONCE_BYTES: usize = 32;
const MAX_CLAUDE_BOUND_SESSIONS: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaudeAdapterError {
    EntropyUnavailable,
    RelayUnavailable,
    UnsupportedCapability(ProviderCapability),
}

impl ClaudeAdapterError {
    pub const fn unsupported_capability(self) -> Option<ProviderCapability> {
        match self {
            Self::UnsupportedCapability(capability) => Some(capability),
            _ => None,
        }
    }
}

impl fmt::Display for ClaudeAdapterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EntropyUnavailable => write!(f, "could not generate a Claude launch nonce"),
            Self::RelayUnavailable => write!(f, "Claude hook relay registration failed"),
            Self::UnsupportedCapability(capability) => {
                write!(f, "Claude capability is unsupported: {capability:?}")
            }
        }
    }
}

impl std::error::Error for ClaudeAdapterError {}

impl From<ClaudeAdapterError> for ProviderError {
    fn from(error: ClaudeAdapterError) -> Self {
        match error {
            ClaudeAdapterError::UnsupportedCapability(capability) => {
                Self::UnsupportedCapability(capability)
            }
            ClaudeAdapterError::EntropyUnavailable | ClaudeAdapterError::RelayUnavailable => {
                Self::UnsupportedCapability(ProviderCapability::BuildLaunch)
            }
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub enum ClaudeBindError {
    WrongProvider,
    WrongNonce,
    WrongTask,
    WrongAgent,
    WrongGeneration,
    LatePriorSession,
    WrongActionEpoch,
    WrongProcessRoot,
    MissingProviderSessionId,
    NotSessionStart,
    ForeignEndpoint,
    PayloadTooLarge,
    InvalidPayload,
    ExpiredRegistration,
    RelayRejected,
    StaleRegistration,
    CorrelationMismatch,
    WrongRelayGeneration,
    ExactResumeMismatch {
        expected: ProviderSessionId,
        observed: ProviderSessionId,
    },
    RebindRejected {
        bound: ProviderSessionId,
        observed: ProviderSessionId,
    },
    InvalidProviderSessionId(ProviderSessionIdError),
    ProviderSessionIdTooLong,
    SessionIdMismatch,
    LaunchIdentityMismatch,
}

impl fmt::Debug for ClaudeBindError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ExactResumeMismatch { .. } => f
                .debug_struct("ExactResumeMismatch")
                .field("expected", &"<redacted>")
                .field("observed", &"<redacted>")
                .finish(),
            Self::RebindRejected { .. } => f
                .debug_struct("RebindRejected")
                .field("bound", &"<redacted>")
                .field("observed", &"<redacted>")
                .finish(),
            Self::WrongProvider => write!(f, "WrongProvider"),
            Self::WrongNonce => write!(f, "WrongNonce"),
            Self::WrongTask => write!(f, "WrongTask"),
            Self::WrongAgent => write!(f, "WrongAgent"),
            Self::WrongGeneration => write!(f, "WrongGeneration"),
            Self::LatePriorSession => write!(f, "LatePriorSession"),
            Self::WrongActionEpoch => write!(f, "WrongActionEpoch"),
            Self::WrongProcessRoot => write!(f, "WrongProcessRoot"),
            Self::MissingProviderSessionId => write!(f, "MissingProviderSessionId"),
            Self::NotSessionStart => write!(f, "NotSessionStart"),
            Self::ForeignEndpoint => write!(f, "ForeignEndpoint"),
            Self::PayloadTooLarge => write!(f, "PayloadTooLarge"),
            Self::InvalidPayload => write!(f, "InvalidPayload"),
            Self::ExpiredRegistration => write!(f, "ExpiredRegistration"),
            Self::RelayRejected => write!(f, "RelayRejected"),
            Self::StaleRegistration => write!(f, "StaleRegistration"),
            Self::CorrelationMismatch => write!(f, "CorrelationMismatch"),
            Self::WrongRelayGeneration => write!(f, "WrongRelayGeneration"),
            Self::InvalidProviderSessionId(_) => write!(f, "InvalidProviderSessionId"),
            Self::ProviderSessionIdTooLong => write!(f, "ProviderSessionIdTooLong"),
            Self::SessionIdMismatch => write!(f, "SessionIdMismatch"),
            Self::LaunchIdentityMismatch => write!(f, "LaunchIdentityMismatch"),
        }
    }
}

impl fmt::Display for ClaudeBindError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongProvider => write!(f, "hook envelope is not Claude Code"),
            Self::WrongNonce => write!(f, "hook envelope nonce does not match this generation"),
            Self::WrongTask => write!(f, "hook envelope task does not match this generation"),
            Self::WrongAgent => write!(f, "hook envelope agent does not match this generation"),
            Self::WrongGeneration => write!(f, "hook envelope generation is not current"),
            Self::LatePriorSession => {
                write!(f, "hook envelope belongs to a prior runtime generation")
            }
            Self::WrongActionEpoch => write!(f, "hook envelope action epoch does not match"),
            Self::WrongProcessRoot => write!(f, "hook envelope process root does not match"),
            Self::MissingProviderSessionId => {
                write!(f, "SessionStart did not carry an official session_id")
            }
            Self::NotSessionStart => {
                write!(f, "only a SessionStart hook can bind providerSessionId")
            }
            Self::ForeignEndpoint => write!(f, "Claude hook peer is not the loopback relay"),
            Self::PayloadTooLarge => write!(f, "Claude hook payload exceeded its byte bound"),
            Self::InvalidPayload => write!(f, "Claude hook payload was not valid JSON"),
            Self::ExpiredRegistration => write!(f, "Claude hook registration has expired"),
            Self::RelayRejected => write!(f, "Claude hook relay rejected the delivery"),
            Self::StaleRegistration => {
                write!(f, "Claude hook registration is not the current generation")
            }
            Self::CorrelationMismatch => {
                write!(
                    f,
                    "Claude hook correlation does not match the relay-issued binding"
                )
            }
            Self::WrongRelayGeneration => {
                write!(
                    f,
                    "Claude hook relay generation does not match this registration"
                )
            }
            Self::ExactResumeMismatch { .. } => {
                write!(
                    f,
                    "SessionStart did not match the exact resume providerSessionId"
                )
            }
            Self::RebindRejected { .. } => {
                write!(f, "providerSessionId cannot be rebound to a different id")
            }
            Self::InvalidProviderSessionId(error) => error.fmt(f),
            Self::ProviderSessionIdTooLong => {
                write!(
                    f,
                    "official providerSessionId exceeds the accepted byte bound"
                )
            }
            Self::SessionIdMismatch => {
                write!(
                    f,
                    "hook session_id does not match the bound providerSessionId"
                )
            }
            Self::LaunchIdentityMismatch => {
                write!(f, "launch executable does not match the attested identity")
            }
        }
    }
}

impl std::error::Error for ClaudeBindError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaudeResumeFailure {
    NotFound,
    Incompatible,
    AuthFailure,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaudeRuntimeSettlement {
    Running,
    Unclassified,
    ResumeFailed(ClaudeResumeFailure),
}

#[derive(Clone, PartialEq, Eq, Hash)]
pub struct ClaudeLaunchNonce(String);

impl fmt::Debug for ClaudeLaunchNonce {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ClaudeLaunchNonce(<redacted>)")
    }
}

impl ClaudeLaunchNonce {
    pub fn generate() -> Result<Self, ClaudeAdapterError> {
        let mut bytes = [0_u8; CLAUDE_LAUNCH_NONCE_BYTES];
        getrandom::fill(&mut bytes).map_err(|_| ClaudeAdapterError::EntropyUnavailable)?;
        Ok(Self(encode_hex(&bytes)))
    }

    fn from_registered(value: String) -> Self {
        Self(value)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn encode_hex(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(encoded, "{byte:02x}");
    }
    encoded
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct BoundKey {
    task_id: TaskId,
    agent_session_id: AgentSessionId,
    runtime_generation: u64,
    action_epoch: u64,
    process_root: ResourceId,
    nonce: String,
    relay_generation: u64,
    provider: ProviderKind,
    executable: ProviderExecutable,
    version: ProviderVersion,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeLaunchRegistration {
    inner: crate::ai::claude_hooks::ClaudeCorrelatedRegistration,
}

impl ClaudeLaunchRegistration {
    pub fn nonce(&self) -> ClaudeLaunchNonce {
        ClaudeLaunchNonce::from_registered(self.inner.nonce().to_string())
    }

    pub fn runtime_generation(&self) -> u64 {
        self.inner.runtime_generation()
    }

    pub fn relay_generation(&self) -> u64 {
        self.inner.relay_generation()
    }

    pub fn binding(&self) -> &ClaudeCorrelationBinding {
        self.inner.binding()
    }

    pub fn expected_provider_session_id(&self) -> Option<&ProviderSessionId> {
        self.inner.expected_provider_session_id()
    }

    pub fn journal_key(&self) -> &crate::remote::presentation::StableSessionKey {
        self.inner.journal_key()
    }

    fn bound_key(&self, identity: &AttestedIdentity) -> BoundKey {
        BoundKey::from_registration(&self.inner, identity)
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct AttestedIdentity {
    executable: ProviderExecutable,
    version: ProviderVersion,
}

impl BoundKey {
    fn from_registration(
        registration: &crate::ai::claude_hooks::ClaudeCorrelatedRegistration,
        identity: &AttestedIdentity,
    ) -> Self {
        let binding = registration.binding();
        Self {
            task_id: binding.task_id(),
            agent_session_id: binding.agent_session_id(),
            runtime_generation: binding.runtime_generation(),
            action_epoch: binding.action_epoch(),
            process_root: binding.process_root(),
            nonce: registration.nonce().to_string(),
            relay_generation: registration.relay_generation(),
            provider: ProviderKind::ClaudeCode,
            executable: identity.executable.clone(),
            version: identity.version.clone(),
        }
    }
}

pub use crate::ai::claude_hooks::{ClaudeAdmittedDelivery, ClaudeCorrelationBinding};

struct ClaudeAdapterState {
    probed: Option<ProviderCapabilities>,
    identity: Option<AttestedIdentity>,
    bound: HashMap<BoundKey, ProviderSessionId>,
    bound_order: VecDeque<BoundKey>,
}

pub struct ClaudeCodeAdapter {
    probes: Arc<dyn ProviderProbeRunner>,
    now_ms: Arc<dyn Fn() -> u64 + Send + Sync>,
    state: Mutex<ClaudeAdapterState>,
}

impl ClaudeCodeAdapter {
    pub fn from_attested_observation(
        observation: ProviderObservation,
    ) -> Result<Self, ProviderError> {
        if observation.kind() != ProviderKind::ClaudeCode
            || observation.capabilities().kind != ProviderKind::ClaudeCode
        {
            return Err(ProviderError::CapabilityKindMismatch {
                expected: ProviderKind::ClaudeCode,
                actual: observation.kind(),
            });
        }
        observation.capabilities().validate()?;
        let adapter = Self::from_runner(Arc::new(UnusableProbeRunner), default_now_ms);
        let mut state = adapter.state.lock().map_err(|_| {
            ProviderError::Probe(ProviderProbeError::Io(ProviderProbeIoError::WaitFailed))
        })?;
        state.identity = Some(AttestedIdentity {
            executable: observation.executable().clone(),
            version: observation.version().clone(),
        });
        state.probed = Some(observation.capabilities().clone());
        drop(state);
        Ok(adapter)
    }

    #[cfg(test)]
    pub fn with_probe_runner(probes: Arc<dyn ProviderProbeRunner>) -> Self {
        Self::with_clock(probes, default_now_ms)
    }

    #[cfg(test)]
    pub fn with_clock(
        probes: Arc<dyn ProviderProbeRunner>,
        now_ms: impl Fn() -> u64 + Send + Sync + 'static,
    ) -> Self {
        Self::from_runner(probes, now_ms)
    }

    fn from_runner(
        probes: Arc<dyn ProviderProbeRunner>,
        now_ms: impl Fn() -> u64 + Send + Sync + 'static,
    ) -> Self {
        Self {
            probes,
            now_ms: Arc::new(now_ms),
            state: Mutex::new(ClaudeAdapterState {
                probed: None,
                identity: None,
                bound: HashMap::new(),
                bound_order: VecDeque::new(),
            }),
        }
    }

    pub fn probed_capabilities(&self) -> Option<ProviderCapabilities> {
        self.state.lock().ok()?.probed.clone()
    }

    pub fn register_with_relay(
        &self,
        relay: &ClaudeHookRegistry,
        journal_key: StableSessionKey,
        expected: &ClaudeCorrelationBinding,
        launch: &LaunchProviderRequest,
        now: Instant,
    ) -> Result<ClaudeLaunchRegistration, ClaudeAdapterError> {
        let identity = self
            .attested_identity()
            .ok_or(ClaudeAdapterError::RelayUnavailable)?;
        if launch.executable().executable() != &identity.executable {
            return Err(ClaudeAdapterError::RelayUnavailable);
        }
        let expected_resume = launch.provider_session_id().cloned();
        let registered = relay
            .register_correlated_at(journal_key, expected.clone(), expected_resume, None, now)
            .map_err(|_| ClaudeAdapterError::RelayUnavailable)?;
        Ok(ClaudeLaunchRegistration { inner: registered })
    }

    pub fn rotate_relay_nonce(
        &self,
        relay: &ClaudeHookRegistry,
        current: &ClaudeLaunchRegistration,
        now: Instant,
    ) -> Result<ClaudeLaunchRegistration, ClaudeAdapterError> {
        let identity = self
            .attested_identity()
            .ok_or(ClaudeAdapterError::RelayUnavailable)?;
        let carry = relay.bound_provider_session_id(current.nonce().as_str());
        relay.unregister(current.nonce().as_str());
        let registered = relay
            .register_correlated_at(
                current.journal_key().clone(),
                current.binding().clone(),
                current.expected_provider_session_id().cloned(),
                carry,
                now,
            )
            .map_err(|_| ClaudeAdapterError::RelayUnavailable)?;
        let rotated = ClaudeLaunchRegistration { inner: registered };
        let mut state = self
            .state
            .lock()
            .map_err(|_| ClaudeAdapterError::RelayUnavailable)?;
        if let Some(bound) = state.bound.get(&current.bound_key(&identity)).cloned() {
            let ClaudeAdapterState {
                bound: bound_map,
                bound_order,
                ..
            } = &mut *state;
            replace_bound(bound_map, bound_order, rotated.bound_key(&identity), bound);
        }
        Ok(rotated)
    }

    fn attested_identity(&self) -> Option<AttestedIdentity> {
        self.state.lock().ok()?.identity.clone()
    }

    pub fn admit_session_start(
        &self,
        relay: &ClaudeHookRegistry,
        peer: SocketAddr,
        presented: &ClaudeLaunchRegistration,
        expected: &ClaudeCorrelationBinding,
        body: &[u8],
        now: Instant,
    ) -> Result<ClaudeAdmittedDelivery, ClaudeBindError> {
        if !peer.ip().is_loopback() {
            return Err(ClaudeBindError::ForeignEndpoint);
        }
        physically_bound_json(body)?;
        let identity = self
            .attested_identity()
            .ok_or(ClaudeBindError::RelayRejected)?;
        let delivery = relay
            .ingest_correlated_at(peer, &presented.inner, expected, body, now, unix_epoch_ms())
            .map_err(map_correlated_ingest_error)?;
        if delivery.registration() != &presented.inner
            || delivery.binding() != presented.binding()
            || delivery.nonce() != presented.nonce().as_str()
            || delivery.relay_generation() != presented.relay_generation()
        {
            return Err(ClaudeBindError::CorrelationMismatch);
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| ClaudeBindError::RelayRejected)?;
        let ClaudeAdapterState {
            bound: bound_map,
            bound_order,
            ..
        } = &mut *state;
        replace_bound(
            bound_map,
            bound_order,
            presented.bound_key(&identity),
            delivery.provider_session_id().clone(),
        );
        Ok(delivery)
    }

    pub fn admit_hook(
        &self,
        relay: &ClaudeHookRegistry,
        presented: &ClaudeLaunchRegistration,
        expected: &ClaudeCorrelationBinding,
        body: &[u8],
        now: Instant,
    ) -> Result<(), ClaudeBindError> {
        physically_bound_json(body)?;
        relay
            .validate_hook_session_at(&presented.inner, expected, body, now)
            .map_err(map_correlated_ingest_error)
    }

    pub fn settle_launch_output(
        &self,
        spec: &ProviderLaunchSpec,
        status: ProviderProbeStatus,
        stdout: &[u8],
        stderr: &[u8],
    ) -> Result<ClaudeRuntimeSettlement, ProviderError> {
        if status == ProviderProbeStatus::Completed {
            return Ok(ClaudeRuntimeSettlement::Running);
        }
        if !is_exact_resume_spec(spec) {
            return Ok(ClaudeRuntimeSettlement::Unclassified);
        }
        if let Some(failure) =
            classify_resume_failure(stderr).or_else(|| classify_resume_failure(stdout))
        {
            return Ok(ClaudeRuntimeSettlement::ResumeFailed(failure));
        }
        Ok(ClaudeRuntimeSettlement::Unclassified)
    }

    fn interpret_probe_outputs(
        &self,
        version_stdout: &[u8],
        help_stdout: &[u8],
        auth_stdout: &[u8],
        observed_at: u64,
    ) -> Result<ProviderCapabilities, ProviderError> {
        let version = ProviderVersion::from_probe_output(version_stdout)?;
        let help = std::str::from_utf8(help_stdout).unwrap_or("");
        let exact_resume = if help.split_whitespace().any(|token| token == "--resume") {
            CapabilitySupport::Supported
        } else {
            CapabilitySupport::Unsupported
        };
        let semantic = if help
            .split(|ch: char| ch == ',' || ch.is_whitespace())
            .any(|token| token == "SessionStart")
        {
            CapabilitySupport::Supported
        } else {
            CapabilitySupport::Unknown
        };
        let auth_state = parse_subscription_auth(auth_stdout);
        let auth_status = match auth_state {
            ProviderAuthState::AuthenticatedSubscription => EvidenceStatus::Authenticated,
            ProviderAuthState::AuthRequired => EvidenceStatus::AuthRequired,
            ProviderAuthState::Unknown => EvidenceStatus::Unknown,
        };
        let capabilities = ProviderCapabilities {
            kind: ProviderKind::ClaudeCode,
            version,
            auth_state,
            exact_resume,
            semantic_events: semantic,
            provider_session_id: semantic,
            build_launch: CapabilitySupport::Supported,
            parse_signal: CapabilitySupport::Unsupported,
            cooperative_stop: CapabilitySupport::Unknown,
            observe_quota: CapabilitySupport::Unknown,
            evidence: vec![
                evidence(
                    EvidenceSourceId::ExecutableVersion,
                    observed_at,
                    EvidenceStatus::Supported,
                )?,
                evidence(
                    EvidenceSourceId::CapabilityProbe,
                    observed_at,
                    EvidenceStatus::Supported,
                )?,
                evidence(EvidenceSourceId::AuthStatusProbe, observed_at, auth_status)?,
            ],
        };
        capabilities.validate()?;
        Ok(capabilities)
    }

    async fn run_probe(
        &self,
        executable: &ProviderExecutableHandle,
        kind: ProviderProbeKind,
    ) -> Result<crate::providers::adapter::ProviderProbeResult, ProviderError> {
        let request = ProviderProbeRequest::new(executable.clone(), kind)
            .map_err(ProviderProbeError::InvalidRequest)?;
        let result = self.probes.run(request).await?;
        match result.status() {
            ProviderProbeStatus::Completed => Ok(result),
            ProviderProbeStatus::NonZeroExit => Err(ProviderError::Probe(
                ProviderProbeError::NonZeroExit(None),
            )),
            ProviderProbeStatus::TimedOut => Err(ProviderError::Probe(ProviderProbeError::TimedOut)),
            ProviderProbeStatus::OutputTooLarge => {
                Err(ProviderError::Probe(ProviderProbeError::OutputTooLarge))
            }
            ProviderProbeStatus::Failed(code) => Err(ProviderError::Probe(ProviderProbeError::Io(
                match code {
                    crate::providers::adapter::ProviderProbeFailureCode::ExecutableMissing => {
                        crate::providers::adapter::ProviderProbeIoError::ExecutableMissing
                    }
                    crate::providers::adapter::ProviderProbeFailureCode::PermissionDenied => {
                        crate::providers::adapter::ProviderProbeIoError::PermissionDenied
                    }
                    crate::providers::adapter::ProviderProbeFailureCode::SpawnFailed => {
                        crate::providers::adapter::ProviderProbeIoError::SpawnFailed
                    }
                    crate::providers::adapter::ProviderProbeFailureCode::WaitFailed => {
                        crate::providers::adapter::ProviderProbeIoError::WaitFailed
                    }
                    crate::providers::adapter::ProviderProbeFailureCode::DescendantCleanupFailed => {
                        crate::providers::adapter::ProviderProbeIoError::DescendantCleanupFailed
                    }
                },
            ))),
        }
    }
}

fn default_now_ms() -> u64 {
    unix_epoch_ms()
}

fn unix_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

fn evidence(
    source: EvidenceSourceId,
    observed_at: u64,
    status: EvidenceStatus,
) -> Result<CapabilityEvidence, ProviderError> {
    CapabilityEvidence::new(source, observed_at, status, None).map_err(|_| {
        ProviderError::InvalidCapabilities(ProviderCapabilitiesError::InvalidEvidence(
            CapabilityEvidenceError::ObservedAtZero,
        ))
    })
}

fn parse_subscription_auth(stdout: &[u8]) -> ProviderAuthState {
    let Ok(value) = serde_json::from_slice::<Value>(stdout) else {
        return ProviderAuthState::Unknown;
    };
    let Some(object) = value.as_object() else {
        return ProviderAuthState::Unknown;
    };
    match object.get("loggedIn") {
        Some(Value::Bool(false)) => ProviderAuthState::AuthRequired,
        Some(Value::Bool(true)) => match object.get("authMethod").and_then(Value::as_str) {
            Some("claude.ai") => ProviderAuthState::AuthenticatedSubscription,
            _ => ProviderAuthState::Unknown,
        },
        _ => ProviderAuthState::Unknown,
    }
}

fn classify_resume_failure(output: &[u8]) -> Option<ClaudeResumeFailure> {
    let text = std::str::from_utf8(output).ok()?;
    for line in text.lines() {
        if line.starts_with("Error: No conversation found with session ID:") {
            return Some(ClaudeResumeFailure::NotFound);
        }
        if line == "Error: Session is incompatible with this Claude Code version" {
            return Some(ClaudeResumeFailure::Incompatible);
        }
        if line == "Error: Authentication required to resume this session" {
            return Some(ClaudeResumeFailure::AuthFailure);
        }
    }
    None
}

fn is_exact_resume_spec(spec: &ProviderLaunchSpec) -> bool {
    spec.arguments().any(|argument| argument == "--resume")
}

fn replace_bound(
    bound: &mut HashMap<BoundKey, ProviderSessionId>,
    order: &mut VecDeque<BoundKey>,
    key: BoundKey,
    id: ProviderSessionId,
) {
    bound.retain(|existing, _| {
        existing.task_id != key.task_id || existing.agent_session_id != key.agent_session_id
    });
    order.retain(|existing| bound.contains_key(existing));
    while bound.len() >= MAX_CLAUDE_BOUND_SESSIONS {
        let Some(evicted) = order.pop_front() else {
            break;
        };
        bound.remove(&evicted);
    }
    bound.insert(key.clone(), id);
    order.push_back(key);
}

fn physically_bound_json(body: &[u8]) -> Result<(), ClaudeBindError> {
    match physically_bound_claude_hook_json(body) {
        Ok(()) => Ok(()),
        Err(ClaudeHookJsonBound::BodyTooLarge) => Err(ClaudeBindError::PayloadTooLarge),
        Err(ClaudeHookJsonBound::Invalid) => Err(ClaudeBindError::InvalidPayload),
    }
}

fn map_correlated_ingest_error(error: ClaudeCorrelatedIngestError) -> ClaudeBindError {
    match error {
        ClaudeCorrelatedIngestError::StaleRegistration => ClaudeBindError::StaleRegistration,
        ClaudeCorrelatedIngestError::Rejected => ClaudeBindError::WrongNonce,
        ClaudeCorrelatedIngestError::Expired => ClaudeBindError::ExpiredRegistration,
        ClaudeCorrelatedIngestError::BodyTooLarge => ClaudeBindError::PayloadTooLarge,
        ClaudeCorrelatedIngestError::InvalidPayload => ClaudeBindError::InvalidPayload,
        ClaudeCorrelatedIngestError::ForeignEndpoint => ClaudeBindError::ForeignEndpoint,
        ClaudeCorrelatedIngestError::BindingMismatch(ClaudeBindingField::Task) => {
            ClaudeBindError::WrongTask
        }
        ClaudeCorrelatedIngestError::BindingMismatch(ClaudeBindingField::Agent) => {
            ClaudeBindError::WrongAgent
        }
        ClaudeCorrelatedIngestError::BindingMismatch(ClaudeBindingField::Generation) => {
            ClaudeBindError::WrongGeneration
        }
        ClaudeCorrelatedIngestError::BindingMismatch(ClaudeBindingField::ActionEpoch) => {
            ClaudeBindError::WrongActionEpoch
        }
        ClaudeCorrelatedIngestError::BindingMismatch(ClaudeBindingField::ProcessRoot) => {
            ClaudeBindError::WrongProcessRoot
        }
        ClaudeCorrelatedIngestError::BindingMismatch(ClaudeBindingField::RelayGeneration) => {
            ClaudeBindError::WrongRelayGeneration
        }
        ClaudeCorrelatedIngestError::LatePriorSession => ClaudeBindError::LatePriorSession,
        ClaudeCorrelatedIngestError::ExactResumeMismatch { expected, observed } => {
            match (
                ProviderSessionId::new(expected),
                ProviderSessionId::new(observed),
            ) {
                (Ok(expected), Ok(observed)) => {
                    ClaudeBindError::ExactResumeMismatch { expected, observed }
                }
                _ => ClaudeBindError::InvalidPayload,
            }
        }
        ClaudeCorrelatedIngestError::RebindRejected { bound, observed } => {
            match (
                ProviderSessionId::new(bound),
                ProviderSessionId::new(observed),
            ) {
                (Ok(bound), Ok(observed)) => ClaudeBindError::RebindRejected { bound, observed },
                _ => ClaudeBindError::InvalidPayload,
            }
        }
        ClaudeCorrelatedIngestError::NotSessionStart => ClaudeBindError::NotSessionStart,
        ClaudeCorrelatedIngestError::MissingProviderSessionId => {
            ClaudeBindError::MissingProviderSessionId
        }
        ClaudeCorrelatedIngestError::CorrelationMismatch => ClaudeBindError::CorrelationMismatch,
        ClaudeCorrelatedIngestError::ProviderSessionIdTooLong => {
            ClaudeBindError::ProviderSessionIdTooLong
        }
        ClaudeCorrelatedIngestError::SessionIdMismatch => ClaudeBindError::SessionIdMismatch,
    }
}

struct UnusableProbeRunner;

#[async_trait]
impl ProviderProbeRunner for UnusableProbeRunner {
    async fn run(
        &self,
        _request: ProviderProbeRequest,
    ) -> Result<crate::providers::adapter::ProviderProbeResult, ProviderProbeError> {
        Err(ProviderProbeError::Io(ProviderProbeIoError::SpawnFailed))
    }
}

#[async_trait]
impl ProviderAdapter for ClaudeCodeAdapter {
    fn kind(&self) -> ProviderKind {
        ProviderKind::ClaudeCode
    }

    async fn probe(
        &self,
        executable: &ProviderExecutableHandle,
    ) -> Result<ProviderCapabilities, ProviderError> {
        let version = self
            .run_probe(executable, ProviderProbeKind::Version)
            .await?;
        let help = self.run_probe(executable, ProviderProbeKind::Help).await?;
        let auth = self
            .run_probe(executable, ProviderProbeKind::AuthStatus)
            .await?;
        let capabilities = self.interpret_probe_outputs(
            version.stdout(),
            help.stdout(),
            auth.stdout(),
            (self.now_ms)(),
        )?;
        let mut state = self.state.lock().map_err(|_| {
            ProviderError::Probe(ProviderProbeError::Io(
                crate::providers::adapter::ProviderProbeIoError::WaitFailed,
            ))
        })?;
        state.probed = Some(capabilities.clone());
        Ok(capabilities)
    }

    fn build_launch(
        &self,
        request: LaunchProviderRequest,
    ) -> Result<ProviderLaunchSpec, ProviderError> {
        let capabilities =
            self.probed_capabilities()
                .ok_or(ProviderError::UnsupportedCapability(
                    ProviderCapability::BuildLaunch,
                ))?;
        if capabilities.kind != ProviderKind::ClaudeCode {
            return Err(ProviderError::CapabilityKindMismatch {
                expected: ProviderKind::ClaudeCode,
                actual: capabilities.kind,
            });
        }
        let identity = self
            .attested_identity()
            .ok_or(ProviderError::UnsupportedCapability(
                ProviderCapability::BuildLaunch,
            ))?;
        if request.executable().executable() != &identity.executable
            || capabilities.version != identity.version
        {
            return Err(ProviderError::ExecutableChanged {
                before: identity.executable,
                after: request.executable().executable().clone(),
            });
        }
        let arguments = if let Some(session_id) = request.provider_session_id() {
            if capabilities.exact_resume != CapabilitySupport::Supported
                || capabilities.provider_session_id != CapabilitySupport::Supported
            {
                return Err(ProviderError::UnsupportedCapability(
                    ProviderCapability::ExactResume,
                ));
            }
            vec![
                ProviderArgument::new("--resume").map_err(|_| {
                    ProviderError::UnsupportedCapability(ProviderCapability::ExactResume)
                })?,
                ProviderArgument::new(session_id.as_str()).map_err(|_| {
                    ProviderError::UnsupportedCapability(ProviderCapability::ExactResume)
                })?,
            ]
        } else {
            Vec::new()
        };
        ProviderLaunchSpec::new(request.executable().clone(), arguments)
            .map_err(|_| ProviderError::UnsupportedCapability(ProviderCapability::BuildLaunch))
    }

    fn normalize_delivery(
        &self,
        _permit: &AdapterDeliveryPermit,
        _bytes: &[u8],
    ) -> Result<NormalizedAdapterDelivery, JournalNormalizeError> {
        Err(JournalNormalizeError::Unavailable(
            AdapterIngressUnavailable,
        ))
    }

    fn cooperative_stop(&self, _session: &ProviderRuntime) -> StopStrategy {
        StopStrategy::Unsupported
    }

    async fn observe_quota(
        &self,
        _executable: &ProviderExecutableHandle,
    ) -> Result<Option<QuotaObservation>, ProviderError> {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::adapter::{ProviderProbeKind, ProviderProbeResult};
    use crate::providers::registry::{ProviderDiscoveryConfig, ProviderRegistry};
    use std::path::Path;
    use tempfile::tempdir;

    fn fixture(name: &str) -> &'static [u8] {
        match name {
            "version" => include_bytes!("../../tests/fixtures/providers/claude/version.txt"),
            "help" => include_bytes!("../../tests/fixtures/providers/claude/help.txt"),
            "auth_authenticated" => {
                include_bytes!(
                    "../../tests/fixtures/providers/claude/auth_status_authenticated.txt"
                )
            }
            "auth_required" => {
                include_bytes!("../../tests/fixtures/providers/claude/auth_status_required.txt")
            }
            "auth_api_key" => {
                include_bytes!("../../tests/fixtures/providers/claude/auth_api_key.txt")
            }
            "auth_negated" => {
                include_bytes!("../../tests/fixtures/providers/claude/auth_negated.txt")
            }
            "auth_ambiguous" => {
                include_bytes!("../../tests/fixtures/providers/claude/auth_ambiguous.txt")
            }
            _ => panic!("unknown Claude fixture {name}"),
        }
    }

    struct FixtureProbeRunner {
        auth: &'static [u8],
        auth_exit: i32,
        auth_stderr: &'static [u8],
        fail: Option<ProviderProbeError>,
    }

    impl FixtureProbeRunner {
        fn authenticated() -> Arc<Self> {
            Arc::new(Self {
                auth: fixture("auth_authenticated"),
                auth_exit: 0,
                auth_stderr: b"",
                fail: None,
            })
        }

        fn auth(body: &'static [u8]) -> Arc<Self> {
            Arc::new(Self {
                auth: body,
                auth_exit: 0,
                auth_stderr: b"",
                fail: None,
            })
        }
    }

    #[async_trait]
    impl ProviderProbeRunner for FixtureProbeRunner {
        async fn run(
            &self,
            request: ProviderProbeRequest,
        ) -> Result<ProviderProbeResult, ProviderProbeError> {
            if let Some(error) = self.fail.clone() {
                return Err(error);
            }
            let (exit, stdout, stderr) = match request.kind() {
                ProviderProbeKind::Version => (0, fixture("version").to_vec(), Vec::new()),
                ProviderProbeKind::Help => (0, fixture("help").to_vec(), Vec::new()),
                ProviderProbeKind::AuthStatus => (
                    self.auth_exit,
                    self.auth.to_vec(),
                    self.auth_stderr.to_vec(),
                ),
                // These probe kinds belong to the shared request contract;
                // Claude's adapter still uses its stock auth-status command
                // and does not need a separate login/resume probe here.
                ProviderProbeKind::LoginStatus | ProviderProbeKind::ResumeHelp => {
                    (0, Vec::new(), Vec::new())
                }
            };
            ProviderProbeResult::from_bounded_output(&request, Some(exit), stdout, stderr)
        }
    }

    #[tokio::test]
    async fn probe_from_injected_runner_reports_authenticated_subscription() {
        let adapter = ClaudeCodeAdapter::with_clock(FixtureProbeRunner::authenticated(), || {
            1_700_000_000_100
        });
        let capabilities = adapter
            .probe(Path::new(r"C:\bin\claude.exe"))
            .await
            .expect("fixture probe");
        assert_eq!(capabilities.kind, ProviderKind::ClaudeCode);
        assert_eq!(capabilities.version.as_str(), "2.0.72 (Claude Code)");
        assert_eq!(
            capabilities.auth_state,
            ProviderAuthState::AuthenticatedSubscription
        );
        assert_eq!(capabilities.exact_resume, CapabilitySupport::Supported);
        assert_eq!(capabilities.semantic_events, CapabilitySupport::Supported);
        assert_eq!(
            capabilities.provider_session_id,
            CapabilitySupport::Supported
        );
        assert_eq!(capabilities.parse_signal, CapabilitySupport::Unsupported);
        capabilities.validate().expect("auth evidence must match");
    }

    #[tokio::test]
    async fn auth_required_api_key_negated_and_ambiguous_are_not_subscription() {
        for (body, expected) in [
            (fixture("auth_required"), ProviderAuthState::AuthRequired),
            (fixture("auth_api_key"), ProviderAuthState::Unknown),
            (fixture("auth_negated"), ProviderAuthState::Unknown),
            (fixture("auth_ambiguous"), ProviderAuthState::Unknown),
        ] {
            let adapter =
                ClaudeCodeAdapter::with_clock(FixtureProbeRunner::auth(body), || 1_700_000_000_100);
            let capabilities = adapter
                .probe(Path::new(r"C:\bin\claude.exe"))
                .await
                .unwrap();
            assert_eq!(capabilities.auth_state, expected, "{body:?}");
            assert_ne!(
                capabilities.auth_state,
                ProviderAuthState::AuthenticatedSubscription
            );
        }

        let stderr_only = Arc::new(FixtureProbeRunner {
            auth: b"",
            auth_exit: 0,
            auth_stderr: fixture("auth_authenticated"),
            fail: None,
        });
        let capabilities = ClaudeCodeAdapter::with_clock(stderr_only, || 1_700_000_000_100)
            .probe(Path::new(r"C:\bin\claude.exe"))
            .await
            .unwrap();
        assert_ne!(
            capabilities.auth_state,
            ProviderAuthState::AuthenticatedSubscription
        );
    }

    #[tokio::test]
    async fn nonzero_and_timeout_probes_do_not_mint_capability_evidence() {
        let nonzero = Arc::new(FixtureProbeRunner {
            auth: fixture("auth_authenticated"),
            auth_exit: 1,
            auth_stderr: b"",
            fail: None,
        });
        let error = ClaudeCodeAdapter::with_clock(nonzero, || 1_700_000_000_100)
            .probe(Path::new(r"C:\bin\claude.exe"))
            .await
            .expect_err("nonzero auth must not mint evidence");
        assert!(matches!(
            error,
            ProviderError::Probe(ProviderProbeError::NonZeroExit(_))
        ));

        let timed_out = Arc::new(FixtureProbeRunner {
            auth: fixture("auth_authenticated"),
            auth_exit: 0,
            auth_stderr: b"",
            fail: Some(ProviderProbeError::TimedOut),
        });
        let error = ClaudeCodeAdapter::with_clock(timed_out, || 1_700_000_000_100)
            .probe(Path::new(r"C:\bin\claude.exe"))
            .await
            .expect_err("timeout must not mint evidence");
        assert!(matches!(
            error,
            ProviderError::Probe(ProviderProbeError::TimedOut)
        ));
    }

    #[tokio::test]
    async fn registry_observe_uses_real_adapter_probe() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("claude");
        std::fs::write(&path, b"fixture-claude").unwrap();

        let mut registry = ProviderRegistry::new();
        registry
            .register(Arc::new(ClaudeCodeAdapter::with_clock(
                FixtureProbeRunner::authenticated(),
                || 1_700_000_000_100,
            )))
            .unwrap();
        let observation = registry
            .observe(
                ProviderKind::ClaudeCode,
                &ProviderDiscoveryConfig {
                    executable_override: Some(path),
                    path: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            observation.capabilities().auth_state,
            ProviderAuthState::AuthenticatedSubscription
        );
        assert_eq!(
            observation.capabilities().exact_resume,
            CapabilitySupport::Supported
        );
    }

    #[test]
    fn replace_bound_keeps_one_correlation_per_task_agent() {
        let executable =
            ProviderExecutable::new(r"C:\bin\claude.exe", [0x11; 32]).expect("executable");
        let version = ProviderVersion::from_probe_output(fixture("version")).unwrap();
        let task = TaskId::new();
        let agent = AgentSessionId::new();
        let first = BoundKey {
            task_id: task,
            agent_session_id: agent,
            runtime_generation: 1,
            action_epoch: 1,
            process_root: ResourceId::new(),
            nonce: "nonce-a".to_string(),
            relay_generation: 1,
            provider: ProviderKind::ClaudeCode,
            executable: executable.clone(),
            version: version.clone(),
        };
        let second = BoundKey {
            task_id: task,
            agent_session_id: agent,
            runtime_generation: 2,
            action_epoch: 1,
            process_root: ResourceId::new(),
            nonce: "nonce-b".to_string(),
            relay_generation: 2,
            provider: ProviderKind::ClaudeCode,
            executable,
            version,
        };
        let mut bound = HashMap::new();
        let mut order = VecDeque::new();
        replace_bound(
            &mut bound,
            &mut order,
            first,
            ProviderSessionId::new("session-1").unwrap(),
        );
        replace_bound(
            &mut bound,
            &mut order,
            second,
            ProviderSessionId::new("session-1").unwrap(),
        );
        assert_eq!(bound.len(), 1);
        assert!(bound.keys().all(|key| key.nonce == "nonce-b"));
    }

    #[test]
    fn bound_correlations_evict_oldest_at_the_capacity() {
        let executable =
            ProviderExecutable::new(r"C:\bin\claude.exe", [0x11; 32]).expect("executable");
        let version = ProviderVersion::from_probe_output(fixture("version")).unwrap();
        let mut bound = HashMap::new();
        let mut order = VecDeque::new();
        let mut oldest = None;
        let mut newest = None;
        for index in 0..=MAX_CLAUDE_BOUND_SESSIONS {
            let key = BoundKey {
                task_id: TaskId::new(),
                agent_session_id: AgentSessionId::new(),
                runtime_generation: index as u64,
                action_epoch: 1,
                process_root: ResourceId::new(),
                nonce: format!("nonce-{index}"),
                relay_generation: index as u64,
                provider: ProviderKind::ClaudeCode,
                executable: executable.clone(),
                version: version.clone(),
            };
            if index == 0 {
                oldest = Some(key.clone());
            }
            newest = Some(key.clone());
            replace_bound(
                &mut bound,
                &mut order,
                key,
                ProviderSessionId::new("session-1").unwrap(),
            );
        }
        assert_eq!(bound.len(), MAX_CLAUDE_BOUND_SESSIONS);
        assert!(!bound.contains_key(&oldest.expect("oldest key")));
        assert!(bound.contains_key(&newest.expect("newest key")));
    }
}
