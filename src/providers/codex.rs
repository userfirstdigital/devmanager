//! Stock Codex CLI adapter for Task 4.4.
//!
//! Launch/resume use probed CLI entry points. Semantic identity is admitted
//! only through the authenticated Codex hook registry/relay. This adapter
//! never starts app-server, Responses API clients, `--last`, or rollout
//! inference.

use crate::ai::codex_hooks::{
    codex_hook_argument_tokens, CodexHookRegistration, CodexHookRegistry, CodexLaunchPermit,
    CodexRelayIngestObservation, MAX_CODEX_HOOK_BODY_BYTES,
};
use crate::domain::{
    AgentSessionId, ProviderSessionId, ProviderSessionIdError, TaskId,
    MAX_PROVIDER_SESSION_ID_BYTES,
};
use crate::process::identity::ManagedProcessId;
use crate::providers::adapter::{
    JournalEvent, LaunchProviderRequest, ProviderAdapter, ProviderArgument, ProviderError,
    ProviderLaunchSpec, ProviderProbeError, ProviderProbeRequest, ProviderProbeRunner,
    ProviderProbeStatus, ProviderRuntime, ProviderSignal, QuotaObservation, StopStrategy,
};
use crate::providers::capabilities::{
    CapabilityEvidence, CapabilitySupport, EvidenceDiagnostic, EvidenceDiagnosticCode,
    EvidenceSourceId, EvidenceStatus, ProviderAuthState, ProviderCapabilities,
    ProviderCapabilitiesError, ProviderCapability, ProviderExecutable, ProviderKind,
    ProviderVersion,
};
use async_trait::async_trait;
use sha2::{Digest, Sha256};
use std::fmt;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{atomic::AtomicU64, Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

const HOOK_TRUST_FLAG: &str = "--dangerously-bypass-hook-trust";
const RESUME_COMMAND: &str = "resume";
const LOGIN_METHOD_LINE: &str = "Logged in using ChatGPT";
const LOGIN_PLAN_LINE: &str = "ChatGPT Plus subscription";
const LOGIN_NOT_AUTH_LINES: &[&str] = &[
    "not authenticated",
    "not logged in",
    "auth required",
    "login required",
];
const RESUME_USAGE_TOKENS: &[&str] = &[
    "Usage:",
    "codex",
    "resume",
    "[OPTIONS]",
    "[SESSION_ID]",
    "[PROMPT]",
];
const FORBIDDEN_SUBCOMMANDS: &[&str] = &["app-server", "remote-control", "exec-server", "exec"];
const FORBIDDEN_FLAGS: &[&str] = &["--remote", "--last", "--listen"];
const MAX_CLASSIFY_BYTES: usize = 4 * 1024;
const MAX_ADMIT_JSON_BYTES: usize = 8 * 1024;
const MAX_ADMIT_JSON_DEPTH: u32 = 3;
const MAX_ADMIT_JSON_NODES: u32 = 32;
const MAX_ADMIT_JSON_STRING: usize = MAX_PROVIDER_SESSION_ID_BYTES;
const UNKNOWN_SESSION_ID: &str = "unknown";

#[derive(Clone)]
struct ProbedCodexSurface {
    identity: ProviderExecutable,
    capabilities: ProviderCapabilities,
    hooks_advertised: bool,
    semantic_state: CodexSemanticLaunchState,
}

pub struct CodexAdapter {
    runner: Arc<dyn ProviderProbeRunner>,
    probed: Arc<Mutex<Option<ProbedCodexSurface>>>,
    pinned: Arc<Mutex<Option<ProviderExecutable>>>,
    attestation_generation: AtomicU64,
}

impl CodexAdapter {
    pub fn new(runner: Arc<dyn ProviderProbeRunner>) -> Self {
        Self {
            runner,
            probed: Arc::new(Mutex::new(None)),
            pinned: Arc::new(Mutex::new(None)),
            attestation_generation: AtomicU64::new(0),
        }
    }

    pub fn last_capabilities(&self, identity: &ProviderExecutable) -> Option<ProviderCapabilities> {
        if !pinned_matches(&self.pinned, identity) {
            return None;
        }
        lock_surface(&self.probed)
            .ok()
            .flatten()
            .filter(|surface| surface.identity == *identity)
            .map(|surface| surface.capabilities.clone())
    }

    pub fn semantic_launch_state(&self, identity: &ProviderExecutable) -> CodexSemanticLaunchState {
        if !pinned_matches(&self.pinned, identity) {
            return CodexSemanticLaunchState::TerminalOnly;
        }
        lock_surface(&self.probed)
            .ok()
            .flatten()
            .filter(|surface| surface.identity == *identity)
            .map(|surface| surface.semantic_state)
            .unwrap_or(CodexSemanticLaunchState::TerminalOnly)
    }

    fn quarantine_attestation(&self) -> Result<(), ProviderError> {
        let mut pinned = self
            .pinned
            .lock()
            .map_err(|_| dependency(ProviderCapability::BuildLaunch))?;
        self.bump_attestation_generation()?;
        let mut probed = self
            .probed
            .lock()
            .map_err(|_| dependency(ProviderCapability::BuildLaunch))?;
        *probed = None;
        *pinned = None;
        Ok(())
    }

    fn begin_attestation(&self, identity: &ProviderExecutable) -> Result<u64, ProviderError> {
        let mut pinned = self
            .pinned
            .lock()
            .map_err(|_| dependency(ProviderCapability::BuildLaunch))?;
        let generation = self.bump_attestation_generation()?;
        let mut probed = self
            .probed
            .lock()
            .map_err(|_| dependency(ProviderCapability::BuildLaunch))?;
        // A probe is an attestation epoch, even when path and digest are
        // unchanged. Clear the prior surface before the first probe command
        // so a stale concurrent probe cannot publish usable capabilities.
        *probed = None;
        *pinned = Some(identity.clone());
        Ok(generation)
    }

    fn bump_attestation_generation(&self) -> Result<u64, ProviderError> {
        let previous = self
            .attestation_generation
            .fetch_update(
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
                |generation| generation.checked_add(1),
            )
            .map_err(|_| dependency(ProviderCapability::BuildLaunch))?;
        Ok(previous + 1)
    }

    fn publish_attestation(
        &self,
        identity: &ProviderExecutable,
        generation: u64,
        surface: ProbedCodexSurface,
    ) -> Result<(), ProviderError> {
        let pinned = self
            .pinned
            .lock()
            .map_err(|_| dependency(ProviderCapability::BuildLaunch))?;
        if pinned.as_ref() != Some(identity)
            || self
                .attestation_generation
                .load(std::sync::atomic::Ordering::Acquire)
                != generation
        {
            return Err(dependency(ProviderCapability::BuildLaunch));
        }
        let mut probed = self
            .probed
            .lock()
            .map_err(|_| dependency(ProviderCapability::BuildLaunch))?;
        *probed = Some(surface);
        Ok(())
    }

    async fn probe_attested(
        &self,
        identity: &ProviderExecutable,
    ) -> Result<ProviderCapabilities, ProviderError> {
        let generation = self.begin_attestation(identity)?;

        let executable = identity.canonical_path();
        let version_request =
            attested_probe_request(ProviderProbeRequest::version(executable), identity)?;
        require_request_identity(&version_request, identity)?;
        let version_result = self.runner.run(version_request).await?;
        require_attestation(
            &self.pinned,
            &self.attestation_generation,
            generation,
            identity,
        )?;
        require_clean_completion(&version_result)?;
        let version = ProviderVersion::from_probe_output(version_result.stdout())?;

        let help_request =
            attested_probe_request(ProviderProbeRequest::help(executable), identity)?;
        require_request_identity(&help_request, identity)?;
        require_attestation(
            &self.pinned,
            &self.attestation_generation,
            generation,
            identity,
        )?;
        let help_result = self.runner.run(help_request).await?;
        require_attestation(
            &self.pinned,
            &self.attestation_generation,
            generation,
            identity,
        )?;
        require_clean_completion(&help_result)?;
        let help = classified_text(help_result.stdout()).unwrap_or("");

        let resume_request =
            attested_probe_request(ProviderProbeRequest::resume_help(executable), identity)?;
        require_request_identity(&resume_request, identity)?;
        require_attestation(
            &self.pinned,
            &self.attestation_generation,
            generation,
            identity,
        )?;
        let exact_resume = match self.runner.run(resume_request).await {
            Ok(result) if is_clean_completion(&result) => {
                support(resume_help_supports_exact_id(result.stdout()))
            }
            _ => CapabilitySupport::Unsupported,
        };
        require_attestation(
            &self.pinned,
            &self.attestation_generation,
            generation,
            identity,
        )?;

        let login_request =
            attested_probe_request(ProviderProbeRequest::login_status(executable), identity)?;
        require_request_identity(&login_request, identity)?;
        require_attestation(
            &self.pinned,
            &self.attestation_generation,
            generation,
            identity,
        )?;
        if login_request.kind() != crate::providers::adapter::ProviderProbeKind::LoginStatus
            || login_request.arguments() != ["login", "status"]
        {
            return Err(ProviderError::Probe(ProviderProbeError::InvalidRequest(
                crate::providers::adapter::ProviderProbeRequestError::EmptyExecutable,
            )));
        }
        let login_result = self.runner.run(login_request).await?;
        require_attestation(
            &self.pinned,
            &self.attestation_generation,
            generation,
            identity,
        )?;
        require_clean_completion(&login_result)?;
        let (auth_state, auth_status) = classify_login_status(login_result.stdout());

        let hooks_advertised = help_advertises_flag(&help, HOOK_TRUST_FLAG);
        let semantic_state = if hooks_advertised {
            CodexSemanticLaunchState::DependencyUnavailable
        } else {
            CodexSemanticLaunchState::TerminalOnly
        };
        let observed_at = unix_now_ms();
        let mut evidence = vec![
            CapabilityEvidence::new(
                EvidenceSourceId::ExecutableVersion,
                observed_at,
                EvidenceStatus::Supported,
                None,
            )
            .map_err(ProviderCapabilitiesError::InvalidEvidence)?,
            CapabilityEvidence::new(
                EvidenceSourceId::CapabilityProbe,
                observed_at,
                if exact_resume.is_supported() || hooks_advertised {
                    EvidenceStatus::Supported
                } else {
                    EvidenceStatus::Unsupported
                },
                None,
            )
            .map_err(ProviderCapabilitiesError::InvalidEvidence)?,
        ];
        if auth_state != ProviderAuthState::Unknown {
            evidence.push(
                CapabilityEvidence::new(
                    EvidenceSourceId::AuthStatusProbe,
                    observed_at,
                    auth_status,
                    Some(EvidenceDiagnostic::new(
                        if auth_state == ProviderAuthState::AuthRequired {
                            EvidenceDiagnosticCode::AuthenticationRequired
                        } else {
                            EvidenceDiagnosticCode::ProbeFailed
                        },
                        Some(identity_status_digest(identity, auth_status)),
                    )),
                )
                .map_err(ProviderCapabilitiesError::InvalidEvidence)?,
            );
        }

        let capabilities = ProviderCapabilities {
            kind: ProviderKind::Codex,
            version,
            auth_state,
            exact_resume,
            semantic_events: CapabilitySupport::Unsupported,
            provider_session_id: CapabilitySupport::Unsupported,
            build_launch: CapabilitySupport::Supported,
            parse_signal: CapabilitySupport::Unsupported,
            cooperative_stop: CapabilitySupport::Unsupported,
            observe_quota: CapabilitySupport::Unsupported,
            evidence,
        };
        capabilities.validate()?;
        self.publish_attestation(
            identity,
            generation,
            ProbedCodexSurface {
                identity: identity.clone(),
                capabilities: capabilities.clone(),
                hooks_advertised,
                semantic_state,
            },
        )?;
        Ok(capabilities)
    }

    pub fn prepare_correlated_launch(
        &self,
        request: LaunchProviderRequest,
        permit: CodexLaunchPermit,
        endpoint: &str,
        relay_executable: &Path,
    ) -> Result<CodexCorrelatedLaunch, ProviderError> {
        require_pinned(&self.pinned, request.executable())?;
        let registry = permit.registry();
        let issued = permit.registration();
        if registry
            .current_registration(&issued.nonce)
            .is_none_or(|current| {
                current.generation != issued.generation
                    || current.nonce != issued.nonce
                    || current.stable_session_key != issued.stable_session_key
            })
        {
            return Err(dependency(ProviderCapability::SemanticEvents));
        }
        let correlation = CodexLaunchCorrelation::from_live_registration(
            permit.task_id(),
            permit.agent_session_id(),
            permit.process_root(),
            issued.generation,
        )
        .map_err(|_| dependency(ProviderCapability::SemanticEvents))?;
        let mut probed = self
            .probed
            .lock()
            .map_err(|_| dependency(ProviderCapability::SemanticEvents))?;
        let surface = probed
            .as_mut()
            .ok_or_else(|| dependency(ProviderCapability::BuildLaunch))?;
        require_same_executable(surface, request.executable())?;
        if !surface.hooks_advertised {
            return Err(dependency(ProviderCapability::SemanticEvents));
        }
        if let Some(session_id) = request.provider_session_id() {
            if !surface.capabilities.exact_resume.is_supported() {
                return Err(ProviderError::UnsupportedCapability(
                    ProviderCapability::ExactResume,
                ));
            }
            if surface.capabilities.auth_state == ProviderAuthState::AuthRequired {
                return Err(ProviderError::UnsupportedCapability(
                    ProviderCapability::ExactResume,
                ));
            }
            let _ = session_id;
        }
        let mut arguments = stock_arguments(request.provider_session_id())?;
        for token in codex_hook_argument_tokens(relay_executable, endpoint, &issued.nonce)
            .map_err(|_| dependency(ProviderCapability::SemanticEvents))?
        {
            arguments.push(argument(&token)?);
        }
        let spec = ProviderLaunchSpec::new(request.executable().clone(), arguments)
            .map_err(|_| ProviderError::UnsupportedCapability(ProviderCapability::BuildLaunch))?;
        reject_forbidden_launch(&spec)?;
        surface.semantic_state = CodexSemanticLaunchState::Registered;
        surface.capabilities.semantic_events = CapabilitySupport::Supported;
        surface.capabilities.provider_session_id = CapabilitySupport::Supported;
        surface.capabilities.parse_signal = CapabilitySupport::Unsupported;
        let registration = permit.into_registration();
        Ok(CodexCorrelatedLaunch {
            spec,
            identity: request.executable().clone(),
            resume_session: request.provider_session_id().cloned(),
            authority: CodexIdentityAuthority {
                correlation,
                registration,
                registry,
                surface: Arc::clone(&self.probed),
                bound: None,
            },
        })
    }

    pub fn managed_process_views(&self) -> Result<(), ProviderError> {
        Err(dependency(ProviderCapability::SemanticEvents))
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn session_id_from_rollout_path(
        _path: &Path,
    ) -> Result<ProviderSessionId, CodexIdentityError> {
        Err(CodexIdentityError::RolloutInferenceForbidden)
    }
}

#[async_trait]
impl ProviderAdapter for CodexAdapter {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Codex
    }

    async fn probe(&self, executable: &Path) -> Result<ProviderCapabilities, ProviderError> {
        self.quarantine_attestation()?;
        let identity =
            ProviderExecutable::inspect_blocking(executable).map_err(ProviderError::Executable)?;
        self.probe_attested(&identity).await
    }

    fn build_launch(
        &self,
        request: LaunchProviderRequest,
    ) -> Result<ProviderLaunchSpec, ProviderError> {
        require_pinned(&self.pinned, request.executable())?;
        let probed = self
            .probed
            .lock()
            .map_err(|_| dependency(ProviderCapability::BuildLaunch))?;
        let surface = probed
            .as_ref()
            .ok_or_else(|| dependency(ProviderCapability::BuildLaunch))?;
        require_same_executable(surface, request.executable())?;
        if let Some(session_id) = request.provider_session_id() {
            if !surface.capabilities.exact_resume.is_supported() {
                return Err(ProviderError::UnsupportedCapability(
                    ProviderCapability::ExactResume,
                ));
            }
            if surface.capabilities.auth_state == ProviderAuthState::AuthRequired {
                return Err(ProviderError::UnsupportedCapability(
                    ProviderCapability::ExactResume,
                ));
            }
            let _ = session_id;
        }
        let arguments = stock_arguments(request.provider_session_id())?;
        let spec = ProviderLaunchSpec::new(request.executable().clone(), arguments)
            .map_err(|_| ProviderError::UnsupportedCapability(ProviderCapability::BuildLaunch))?;
        reject_forbidden_launch(&spec)?;
        Ok(spec)
    }

    fn parse_signal(&self, _signal: ProviderSignal) -> Vec<JournalEvent> {
        Vec::new()
    }

    fn cooperative_stop(&self, _session: &ProviderRuntime) -> StopStrategy {
        StopStrategy::Unsupported
    }

    async fn observe_quota(
        &self,
        _executable: &Path,
    ) -> Result<Option<QuotaObservation>, ProviderError> {
        Ok(None)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct CodexLaunchCorrelation {
    task_id: TaskId,
    agent_session_id: AgentSessionId,
    runtime_generation: u64,
    resource_generation: u64,
    action_epoch: u64,
    process_root: ManagedProcessId,
}

impl fmt::Debug for CodexLaunchCorrelation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CodexLaunchCorrelation")
            .field("runtime_generation", &self.runtime_generation)
            .field("resource_generation", &self.resource_generation)
            .field("action_epoch", &self.action_epoch)
            .finish_non_exhaustive()
    }
}

impl CodexLaunchCorrelation {
    fn new(
        task_id: TaskId,
        agent_session_id: AgentSessionId,
        runtime_generation: u64,
        resource_generation: u64,
        action_epoch: u64,
        process_root: ManagedProcessId,
    ) -> Result<Self, CodexIdentityError> {
        if runtime_generation == 0 || resource_generation == 0 || action_epoch == 0 {
            return Err(CodexIdentityError::WrongGeneration);
        }
        Ok(Self {
            task_id,
            agent_session_id,
            runtime_generation,
            resource_generation,
            action_epoch,
            process_root,
        })
    }

    fn from_live_registration(
        task_id: TaskId,
        agent_session_id: AgentSessionId,
        process_root: ManagedProcessId,
        generation: u64,
    ) -> Result<Self, CodexIdentityError> {
        Self::new(
            task_id,
            agent_session_id,
            generation,
            generation,
            generation,
            process_root,
        )
    }
}

pub struct CodexCorrelatedLaunch {
    spec: ProviderLaunchSpec,
    identity: ProviderExecutable,
    resume_session: Option<ProviderSessionId>,
    authority: CodexIdentityAuthority,
}

impl fmt::Debug for CodexCorrelatedLaunch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CodexCorrelatedLaunch")
            .field("has_resume_session", &self.resume_session.is_some())
            .finish_non_exhaustive()
    }
}

impl CodexCorrelatedLaunch {
    pub fn spec(&self) -> &ProviderLaunchSpec {
        &self.spec
    }

    pub fn authority(&self) -> &CodexIdentityAuthority {
        &self.authority
    }

    pub fn settle_exact_resume(
        &self,
        identity: &ProviderExecutable,
        session_id: &ProviderSessionId,
        observed: CodexResumeObservation,
    ) -> Result<(), CodexResumeFailure> {
        if self.identity != *identity {
            return Err(CodexResumeFailure::Incompatible);
        }
        if self.authority.verify_live().is_err() {
            return Err(CodexResumeFailure::Incompatible);
        }
        let Some(expected) = &self.resume_session else {
            return Err(CodexResumeFailure::Incompatible);
        };
        if expected != session_id {
            return Err(CodexResumeFailure::Incompatible);
        }
        let surface = lock_surface(&self.authority.surface)
            .ok()
            .flatten()
            .filter(|surface| surface.identity == *identity)
            .ok_or(CodexResumeFailure::Incompatible)?;
        match observed {
            CodexResumeObservation::Failed(failure) => Err(failure),
            CodexResumeObservation::Succeeded => {
                if surface.capabilities.auth_state == ProviderAuthState::AuthRequired {
                    return Err(CodexResumeFailure::AuthRequired);
                }
                if !surface.capabilities.exact_resume.is_supported() {
                    return Err(CodexResumeFailure::Incompatible);
                }
                Ok(())
            }
        }
    }

    pub fn relay_ingest(
        &self,
        peer: SocketAddr,
        body: &[u8],
        occurred_at_epoch_ms: u64,
    ) -> CodexRelayIngestObservation {
        self.authority.registry.observe_ingest(
            peer,
            &self.authority.registration.nonce,
            body,
            occurred_at_epoch_ms,
        )
    }

    pub fn admit_ingest(
        &mut self,
        observation: CodexRelayIngestObservation,
        body: &[u8],
    ) -> Result<CodexAdmission, CodexIdentityError> {
        self.authority.admit_ingest(observation, body)
    }
}

pub struct CodexIdentityAuthority {
    correlation: CodexLaunchCorrelation,
    registration: CodexHookRegistration,
    registry: Arc<CodexHookRegistry>,
    surface: Arc<Mutex<Option<ProbedCodexSurface>>>,
    bound: Option<ProviderSessionId>,
}

impl fmt::Debug for CodexIdentityAuthority {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CodexIdentityAuthority")
            .field("runtime_generation", &self.correlation.runtime_generation)
            .field("bound", &self.bound.is_some())
            .finish_non_exhaustive()
    }
}

impl CodexIdentityAuthority {
    pub fn bound_id(&self) -> Option<&ProviderSessionId> {
        self.bound.as_ref()
    }

    pub fn task_id(&self) -> TaskId {
        self.correlation.task_id
    }

    pub fn agent_session_id(&self) -> AgentSessionId {
        self.correlation.agent_session_id
    }

    pub fn runtime_generation(&self) -> u64 {
        self.correlation.runtime_generation
    }

    pub fn resource_generation(&self) -> u64 {
        self.correlation.resource_generation
    }

    pub fn action_epoch(&self) -> u64 {
        self.correlation.action_epoch
    }

    pub fn process_root(&self) -> ManagedProcessId {
        self.correlation.process_root
    }

    fn admit_ingest(
        &mut self,
        observation: CodexRelayIngestObservation,
        body: &[u8],
    ) -> Result<CodexAdmission, CodexIdentityError> {
        self.verify_live()?;
        if body.len() > MAX_CODEX_HOOK_BODY_BYTES || body.len() > MAX_ADMIT_JSON_BYTES {
            return Err(CodexIdentityError::Rejected);
        }
        if !observation.authenticates(&self.registration, body) {
            return Err(CodexIdentityError::Rejected);
        }
        let outcome = preflight_hook_json(body)?;
        let session_id = match outcome.hook {
            PreflightHook::SessionStart => Some(
                outcome
                    .session_id
                    .ok_or(CodexIdentityError::MissingSessionId)?
                    .into_provider_session_id()?,
            ),
            PreflightHook::Other => None,
        };
        let registry = Arc::clone(&self.registry);
        let registration = self.registration.clone();
        let admitted =
            registry.admit_and_publish(&registration, &observation, body, || match session_id {
                Some(session_id) => self.bind_first_unchecked(session_id),
                None => Ok(redacted_partial(body)),
            })?;
        admitted.ok_or(CodexIdentityError::Rejected)
    }

    fn verify_live(&self) -> Result<(), CodexIdentityError> {
        let Some(current) = self.registry.current_registration(&self.registration.nonce) else {
            return Err(CodexIdentityError::Rejected);
        };
        if current.generation != self.registration.generation
            || current.nonce != self.registration.nonce
            || current.stable_session_key != self.registration.stable_session_key
        {
            return Err(CodexIdentityError::Rejected);
        }
        Ok(())
    }

    #[cfg(test)]
    fn bind_first(
        &mut self,
        session_id: ProviderSessionId,
    ) -> Result<CodexAdmission, CodexIdentityError> {
        let registry = Arc::clone(&self.registry);
        let registration = self.registration.clone();
        registry
            .with_live_registration(&registration, || self.bind_first_unchecked(session_id))
            .ok_or(CodexIdentityError::Rejected)?
    }

    fn bind_first_unchecked(
        &mut self,
        session_id: ProviderSessionId,
    ) -> Result<CodexAdmission, CodexIdentityError> {
        if let Some(bound) = &self.bound {
            if bound == &session_id {
                return Err(CodexIdentityError::Replay);
            }
            return Err(CodexIdentityError::AlreadyBound);
        }
        self.bound = Some(session_id.clone());
        Ok(CodexAdmission::Bound(session_id))
    }
}

impl Drop for CodexIdentityAuthority {
    fn drop(&mut self) {
        self.registry.unregister(&self.registration.nonce);
        if let Ok(mut probed) = self.surface.lock() {
            if let Some(surface) = probed.as_mut() {
                if !self.registry.has_live_registrations() {
                    surface.semantic_state = if surface.hooks_advertised {
                        CodexSemanticLaunchState::DependencyUnavailable
                    } else {
                        CodexSemanticLaunchState::TerminalOnly
                    };
                    surface.capabilities.semantic_events = CapabilitySupport::Unsupported;
                    surface.capabilities.provider_session_id = CapabilitySupport::Unsupported;
                    surface.capabilities.parse_signal = CapabilitySupport::Unsupported;
                }
            }
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub enum CodexAdmission {
    Bound(ProviderSessionId),
    Partial { diagnostic: EvidenceDiagnostic },
}

impl fmt::Debug for CodexAdmission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bound(_) => f.debug_tuple("Bound").finish_non_exhaustive(),
            Self::Partial { diagnostic } => f
                .debug_struct("Partial")
                .field("diagnostic", diagnostic)
                .finish(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexIdentityError {
    MissingSessionId,
    WrongTask,
    WrongAgent,
    WrongGeneration,
    WrongRoot,
    WrongActionEpoch,
    WrongNonce,
    AlreadyBound,
    Replay,
    Rejected,
    RolloutInferenceForbidden,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexResumeFailure {
    NotFound,
    Incompatible,
    AuthRequired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexResumeObservation {
    Succeeded,
    Failed(CodexResumeFailure),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexSemanticLaunchState {
    TerminalOnly,
    DependencyUnavailable,
    Registered,
}

fn lock_surface(
    probed: &Mutex<Option<ProbedCodexSurface>>,
) -> Result<Option<ProbedCodexSurface>, ProviderError> {
    probed
        .lock()
        .map(|guard| guard.clone())
        .map_err(|_| dependency(ProviderCapability::BuildLaunch))
}

fn pinned_matches(
    pinned: &Mutex<Option<ProviderExecutable>>,
    identity: &ProviderExecutable,
) -> bool {
    pinned
        .lock()
        .ok()
        .and_then(|guard| guard.clone())
        .is_some_and(|current| current == *identity)
}

fn require_pinned(
    pinned: &Mutex<Option<ProviderExecutable>>,
    identity: &ProviderExecutable,
) -> Result<(), ProviderError> {
    let current = pinned
        .lock()
        .map_err(|_| dependency(ProviderCapability::BuildLaunch))?
        .clone();
    match current {
        Some(current) if current == *identity => Ok(()),
        Some(current) => Err(ProviderError::ExecutableChanged {
            before: current,
            after: identity.clone(),
        }),
        None => Err(dependency(ProviderCapability::BuildLaunch)),
    }
}

fn require_attestation(
    pinned: &Mutex<Option<ProviderExecutable>>,
    generation: &AtomicU64,
    expected_generation: u64,
    identity: &ProviderExecutable,
) -> Result<(), ProviderError> {
    let current = pinned
        .lock()
        .map_err(|_| dependency(ProviderCapability::BuildLaunch))?
        .clone();
    match current {
        Some(current) if current != *identity => Err(ProviderError::ExecutableChanged {
            before: current,
            after: identity.clone(),
        }),
        Some(_) if generation.load(std::sync::atomic::Ordering::Acquire) == expected_generation => {
            Ok(())
        }
        _ => Err(dependency(ProviderCapability::BuildLaunch)),
    }
}

fn dependency(capability: ProviderCapability) -> ProviderError {
    ProviderError::DependencyUnavailable { capability }
}

fn require_same_executable(
    surface: &ProbedCodexSurface,
    executable: &ProviderExecutable,
) -> Result<(), ProviderError> {
    if surface.identity == *executable {
        Ok(())
    } else {
        Err(ProviderError::ExecutableChanged {
            before: surface.identity.clone(),
            after: executable.clone(),
        })
    }
}

fn require_request_identity(
    request: &ProviderProbeRequest,
    identity: &ProviderExecutable,
) -> Result<(), ProviderError> {
    if request.executable() == identity.canonical_path()
        && request.executable_sha256() == Some(identity.sha256())
    {
        Ok(())
    } else {
        Err(ProviderError::ExecutableChanged {
            before: identity.clone(),
            after: identity.clone(),
        })
    }
}

fn attested_probe_request(
    request: Result<ProviderProbeRequest, crate::providers::adapter::ProviderProbeRequestError>,
    identity: &ProviderExecutable,
) -> Result<ProviderProbeRequest, ProviderError> {
    Ok(probe_request(request)?.with_executable_digest(*identity.sha256()))
}

fn probe_request(
    request: Result<ProviderProbeRequest, crate::providers::adapter::ProviderProbeRequestError>,
) -> Result<ProviderProbeRequest, ProviderError> {
    request
        .map_err(ProviderProbeError::InvalidRequest)
        .map_err(ProviderError::from)
}

fn identity_status_digest(identity: &ProviderExecutable, status: EvidenceStatus) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(identity.sha256());
    hasher.update([status as u8]);
    hasher.finalize().into()
}

fn is_clean_completion(result: &crate::providers::adapter::ProviderProbeResult) -> bool {
    result.status() == ProviderProbeStatus::Completed && result.stderr().is_empty()
}

fn require_clean_completion(
    result: &crate::providers::adapter::ProviderProbeResult,
) -> Result<(), ProviderError> {
    if !is_clean_completion(result) {
        return Err(ProviderError::Probe(match result.status() {
            ProviderProbeStatus::NonZeroExit => ProviderProbeError::NonZeroExit(None),
            ProviderProbeStatus::TimedOut => ProviderProbeError::TimedOut,
            ProviderProbeStatus::OutputTooLarge => ProviderProbeError::OutputTooLarge,
            _ => ProviderProbeError::NonZeroExit(None),
        }));
    }
    Ok(())
}

fn stock_arguments(
    session_id: Option<&ProviderSessionId>,
) -> Result<Vec<ProviderArgument>, ProviderError> {
    let mut arguments = Vec::new();
    if let Some(session_id) = session_id {
        arguments.push(argument(RESUME_COMMAND)?);
        arguments.push(argument(session_id.as_str())?);
    }
    Ok(arguments)
}

fn argument(value: &str) -> Result<ProviderArgument, ProviderError> {
    ProviderArgument::new(value)
        .map_err(|_| ProviderError::UnsupportedCapability(ProviderCapability::BuildLaunch))
}

fn reject_forbidden_launch(spec: &ProviderLaunchSpec) -> Result<(), ProviderError> {
    let arguments: Vec<&str> = spec.arguments().collect();
    if let Some(first) = arguments.first() {
        let lower = first.to_ascii_lowercase();
        if FORBIDDEN_SUBCOMMANDS
            .iter()
            .any(|forbidden| lower == *forbidden)
        {
            return Err(ProviderError::UnsupportedCapability(
                ProviderCapability::BuildLaunch,
            ));
        }
    }
    for argument in arguments {
        let lower = argument.to_ascii_lowercase();
        if FORBIDDEN_FLAGS.iter().any(|forbidden| lower == *forbidden)
            || lower.contains("app-server")
            || lower.contains("responses")
            || lower.contains("api.openai")
        {
            return Err(ProviderError::UnsupportedCapability(
                ProviderCapability::BuildLaunch,
            ));
        }
    }
    Ok(())
}

fn classify_login_status(stdout: &[u8]) -> (ProviderAuthState, EvidenceStatus) {
    let Some(text) = classified_text(stdout) else {
        return (ProviderAuthState::Unknown, EvidenceStatus::Unknown);
    };
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    if lines
        .iter()
        .any(|line| LOGIN_NOT_AUTH_LINES.iter().any(|expected| line == expected))
    {
        return (
            ProviderAuthState::AuthRequired,
            EvidenceStatus::AuthRequired,
        );
    }
    let method = lines.iter().any(|line| *line == LOGIN_METHOD_LINE);
    let plan = lines.iter().any(|line| *line == LOGIN_PLAN_LINE);
    if method && plan {
        return (
            ProviderAuthState::AuthenticatedSubscription,
            EvidenceStatus::Authenticated,
        );
    }
    (ProviderAuthState::Unknown, EvidenceStatus::Unknown)
}

fn support(supported: bool) -> CapabilitySupport {
    if supported {
        CapabilitySupport::Supported
    } else {
        CapabilitySupport::Unsupported
    }
}

fn help_advertises_flag(help: &str, flag: &str) -> bool {
    if flag.is_empty() {
        return false;
    }
    help.match_indices(flag).any(|(offset, _)| {
        let before = help[..offset].chars().next_back();
        let after = help[offset + flag.len()..].chars().next();
        before.is_none_or(|character| !is_flag_name_character(character))
            && after.is_none_or(|character| !is_flag_name_character(character))
    })
}

fn resume_help_supports_exact_id(stdout: &[u8]) -> bool {
    let Some(text) = classified_text(stdout) else {
        return false;
    };
    text.lines().any(|line| {
        line.split_whitespace()
            .eq(RESUME_USAGE_TOKENS.iter().copied())
    })
}

fn is_flag_name_character(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
}

fn classified_text(bytes: &[u8]) -> Option<&str> {
    if bytes.len() > MAX_CLASSIFY_BYTES {
        return None;
    }
    std::str::from_utf8(bytes).ok()
}

#[derive(Clone, Copy)]
enum PreflightHook {
    SessionStart,
    Other,
}

struct BoundedSessionId {
    bytes: [u8; MAX_PROVIDER_SESSION_ID_BYTES],
    len: usize,
}

impl BoundedSessionId {
    fn from_raw(raw: &[u8]) -> Result<Self, CodexIdentityError> {
        if raw.is_empty() || raw.eq_ignore_ascii_case(UNKNOWN_SESSION_ID.as_bytes()) {
            return Err(CodexIdentityError::MissingSessionId);
        }
        if raw.len() > MAX_PROVIDER_SESSION_ID_BYTES
            || raw
                .iter()
                .any(|candidate| !candidate.is_ascii() || candidate.is_ascii_control())
        {
            return Err(CodexIdentityError::Rejected);
        }
        let mut bytes = [0u8; MAX_PROVIDER_SESSION_ID_BYTES];
        bytes[..raw.len()].copy_from_slice(raw);
        Ok(Self {
            bytes,
            len: raw.len(),
        })
    }

    fn into_provider_session_id(self) -> Result<ProviderSessionId, CodexIdentityError> {
        let raw = std::str::from_utf8(&self.bytes[..self.len])
            .map_err(|_| CodexIdentityError::Rejected)?;
        ProviderSessionId::new(raw).map_err(|error| match error {
            ProviderSessionIdError::Empty => CodexIdentityError::MissingSessionId,
            ProviderSessionIdError::TooLong | ProviderSessionIdError::ContainsControlCharacter => {
                CodexIdentityError::Rejected
            }
        })
    }
}

struct PreflightOutcome {
    hook: PreflightHook,
    session_id: Option<BoundedSessionId>,
}

fn preflight_hook_json(body: &[u8]) -> Result<PreflightOutcome, CodexIdentityError> {
    if body.len() > MAX_ADMIT_JSON_BYTES {
        return Err(CodexIdentityError::Rejected);
    }
    let mut parser = JsonPreflight {
        bytes: body,
        index: 0,
        nodes: 0,
        hook: PreflightHook::Other,
        session_id: None,
        hook_seen: false,
    };
    parser.skip_ws();
    if parser.bytes.get(parser.index) != Some(&b'{') {
        return Err(CodexIdentityError::Rejected);
    }
    parser.parse_object(1, true)?;
    parser.skip_ws();
    if parser.index != parser.bytes.len() {
        return Err(CodexIdentityError::Rejected);
    }
    Ok(PreflightOutcome {
        hook: parser.hook,
        session_id: parser.session_id,
    })
}

enum StringCapture {
    Key,
    HookEvent,
    Other,
}

enum ParsedString {
    SessionIdKey,
    HookEventNameKey,
    SessionStart,
    Other,
}

struct JsonPreflight<'a> {
    bytes: &'a [u8],
    index: usize,
    nodes: u32,
    hook: PreflightHook,
    session_id: Option<BoundedSessionId>,
    hook_seen: bool,
}

impl JsonPreflight<'_> {
    fn skip_ws(&mut self) {
        while self
            .bytes
            .get(self.index)
            .is_some_and(|byte| byte.is_ascii_whitespace())
        {
            self.index += 1;
        }
    }

    fn bump_node(&mut self) -> Result<(), CodexIdentityError> {
        self.nodes = self
            .nodes
            .checked_add(1)
            .filter(|count| *count <= MAX_ADMIT_JSON_NODES)
            .ok_or(CodexIdentityError::Rejected)?;
        Ok(())
    }

    fn parse_value(
        &mut self,
        depth: u32,
        capture_top_level_identity: bool,
    ) -> Result<(), CodexIdentityError> {
        if depth > MAX_ADMIT_JSON_DEPTH {
            return Err(CodexIdentityError::Rejected);
        }
        self.skip_ws();
        self.bump_node()?;
        match self.bytes.get(self.index).copied() {
            Some(b'{') => self.parse_object(depth, capture_top_level_identity),
            Some(b'[') => self.parse_array(depth, capture_top_level_identity),
            Some(b'"') => self.parse_string(StringCapture::Other).map(|_| ()),
            Some(b't') => self.parse_literal(b"true"),
            Some(b'f') => self.parse_literal(b"false"),
            Some(b'n') => self.parse_literal(b"null"),
            Some(byte) if byte == b'-' || byte.is_ascii_digit() => self.parse_number(),
            _ => Err(CodexIdentityError::Rejected),
        }
    }

    fn parse_object(
        &mut self,
        depth: u32,
        capture_top_level_identity: bool,
    ) -> Result<(), CodexIdentityError> {
        self.index += 1;
        self.skip_ws();
        if self.bytes.get(self.index) == Some(&b'}') {
            self.index += 1;
            return Ok(());
        }
        loop {
            self.skip_ws();
            let key = self.parse_string(StringCapture::Key)?;
            self.skip_ws();
            if self.bytes.get(self.index) != Some(&b':') {
                return Err(CodexIdentityError::Rejected);
            }
            self.index += 1;
            match key {
                ParsedString::SessionIdKey if capture_top_level_identity => {
                    self.skip_ws();
                    self.bump_node()?;
                    if self.session_id.is_some() {
                        return Err(CodexIdentityError::Rejected);
                    }
                    self.session_id = Some(self.parse_bounded_session_id()?);
                }
                ParsedString::HookEventNameKey if capture_top_level_identity => {
                    self.skip_ws();
                    self.bump_node()?;
                    if self.hook_seen {
                        return Err(CodexIdentityError::Rejected);
                    }
                    self.hook_seen = true;
                    if matches!(
                        self.parse_string(StringCapture::HookEvent)?,
                        ParsedString::SessionStart
                    ) {
                        self.hook = PreflightHook::SessionStart;
                    }
                }
                ParsedString::SessionIdKey | ParsedString::HookEventNameKey => {
                    // Session identity is accepted only in the stock
                    // top-level object. Reject nested/array copies instead
                    // of recursively discovering an attacker-controlled ID.
                    return Err(CodexIdentityError::Rejected);
                }
                _ => {
                    self.parse_value(depth + 1, false)?;
                }
            }
            self.skip_ws();
            match self.bytes.get(self.index) {
                Some(b',') => {
                    self.index += 1;
                    continue;
                }
                Some(b'}') => {
                    self.index += 1;
                    return Ok(());
                }
                _ => return Err(CodexIdentityError::Rejected),
            }
        }
    }

    fn parse_array(
        &mut self,
        depth: u32,
        capture_top_level_identity: bool,
    ) -> Result<(), CodexIdentityError> {
        self.index += 1;
        self.skip_ws();
        if self.bytes.get(self.index) == Some(&b']') {
            self.index += 1;
            return Ok(());
        }
        loop {
            self.parse_value(depth + 1, capture_top_level_identity)?;
            self.skip_ws();
            match self.bytes.get(self.index) {
                Some(b',') => {
                    self.index += 1;
                    continue;
                }
                Some(b']') => {
                    self.index += 1;
                    return Ok(());
                }
                _ => return Err(CodexIdentityError::Rejected),
            }
        }
    }

    fn parse_bounded_session_id(&mut self) -> Result<BoundedSessionId, CodexIdentityError> {
        if self.bytes.get(self.index) != Some(&b'"') {
            return Err(CodexIdentityError::Rejected);
        }
        self.index += 1;
        let start = self.index;
        while let Some(byte) = self.bytes.get(self.index).copied() {
            self.index += 1;
            if byte == b'\\' {
                return Err(CodexIdentityError::Rejected);
            }
            if byte == b'"' {
                return BoundedSessionId::from_raw(&self.bytes[start..self.index - 1]);
            }
            if byte < 0x20 {
                return Err(CodexIdentityError::Rejected);
            }
            if self.index - start > MAX_PROVIDER_SESSION_ID_BYTES {
                return Err(CodexIdentityError::Rejected);
            }
        }
        Err(CodexIdentityError::Rejected)
    }

    fn parse_string(&mut self, capture: StringCapture) -> Result<ParsedString, CodexIdentityError> {
        if self.bytes.get(self.index) != Some(&b'"') {
            return Err(CodexIdentityError::Rejected);
        }
        self.index += 1;
        let start = self.index;
        let mut escaped = false;
        let mut unescaped = 0usize;
        while let Some(byte) = self.bytes.get(self.index).copied() {
            self.index += 1;
            if escaped {
                escaped = false;
                unescaped = unescaped
                    .checked_add(1)
                    .ok_or(CodexIdentityError::Rejected)?;
                if matches!(capture, StringCapture::HookEvent) {
                    return Err(CodexIdentityError::Rejected);
                }
                if unescaped > MAX_ADMIT_JSON_STRING {
                    return Err(CodexIdentityError::Rejected);
                }
                continue;
            }
            if byte == b'\\' {
                escaped = true;
                continue;
            }
            if byte == b'"' {
                let raw = &self.bytes[start..self.index - 1];
                if unescaped > MAX_ADMIT_JSON_STRING {
                    return Err(CodexIdentityError::Rejected);
                }
                return Ok(match capture {
                    StringCapture::Key if raw == b"session_id" => ParsedString::SessionIdKey,
                    StringCapture::Key if raw == b"hook_event_name" => {
                        ParsedString::HookEventNameKey
                    }
                    StringCapture::HookEvent if raw == b"SessionStart" => {
                        ParsedString::SessionStart
                    }
                    _ => ParsedString::Other,
                });
            }
            if byte < 0x20 {
                return Err(CodexIdentityError::Rejected);
            }
            unescaped = unescaped
                .checked_add(1)
                .ok_or(CodexIdentityError::Rejected)?;
            if unescaped > MAX_ADMIT_JSON_STRING {
                return Err(CodexIdentityError::Rejected);
            }
        }
        Err(CodexIdentityError::Rejected)
    }

    fn parse_literal(&mut self, literal: &[u8]) -> Result<(), CodexIdentityError> {
        if self
            .bytes
            .get(self.index..)
            .is_some_and(|slice| slice.starts_with(literal))
        {
            self.index += literal.len();
            Ok(())
        } else {
            Err(CodexIdentityError::Rejected)
        }
    }

    fn parse_number(&mut self) -> Result<(), CodexIdentityError> {
        let start = self.index;
        if self.bytes.get(self.index) == Some(&b'-') {
            self.index += 1;
        }
        let digits_start = self.index;
        while self
            .bytes
            .get(self.index)
            .is_some_and(|byte| byte.is_ascii_digit())
        {
            self.index += 1;
        }
        if self.index == digits_start {
            return Err(CodexIdentityError::Rejected);
        }
        if self.bytes.get(self.index) == Some(&b'.') {
            self.index += 1;
            let frac = self.index;
            while self
                .bytes
                .get(self.index)
                .is_some_and(|byte| byte.is_ascii_digit())
            {
                self.index += 1;
            }
            if self.index == frac {
                return Err(CodexIdentityError::Rejected);
            }
        }
        if matches!(self.bytes.get(self.index), Some(b'e' | b'E')) {
            self.index += 1;
            if matches!(self.bytes.get(self.index), Some(b'+' | b'-')) {
                self.index += 1;
            }
            let exp = self.index;
            while self
                .bytes
                .get(self.index)
                .is_some_and(|byte| byte.is_ascii_digit())
            {
                self.index += 1;
            }
            if self.index == exp {
                return Err(CodexIdentityError::Rejected);
            }
        }
        if self.index == start {
            Err(CodexIdentityError::Rejected)
        } else {
            Ok(())
        }
    }
}

fn redacted_partial(body: &[u8]) -> CodexAdmission {
    let digest = Sha256::digest(body.get(..MAX_CLASSIFY_BYTES).unwrap_or(body));
    CodexAdmission::Partial {
        diagnostic: EvidenceDiagnostic::new(
            EvidenceDiagnosticCode::ProbeFailed,
            Some(digest.into()),
        ),
    }
}

fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(1)
        .max(1)
}

#[cfg(test)]
#[path = "codex_identity_tests.rs"]
mod codex_identity_tests;

#[cfg(test)]
mod authority_seal_tests {
    use super::*;

    #[test]
    fn crate_private_correlation_constructor_rejects_zero_generation() {
        assert!(matches!(
            CodexLaunchCorrelation::new(
                TaskId::new(),
                AgentSessionId::new(),
                0,
                3,
                8,
                ManagedProcessId::new(1001, 1_700_000_000_001).unwrap(),
            ),
            Err(CodexIdentityError::WrongGeneration)
        ));
    }

    #[test]
    fn crate_private_correlation_debug_omits_identity_fields() {
        let correlation = CodexLaunchCorrelation::new(
            TaskId::new(),
            AgentSessionId::new(),
            3,
            3,
            8,
            ManagedProcessId::new(1001, 1_700_000_000_001).unwrap(),
        )
        .unwrap();
        let rendered = format!("{correlation:?}");
        assert!(!rendered.contains("task_id"));
        assert!(!rendered.contains("agent_session_id"));
        assert!(!rendered.contains("process_root"));
    }

    #[test]
    fn unused_registry_permit_unregisters_and_cannot_be_forged() {
        let registry = Arc::new(CodexHookRegistry::default());
        let permit = CodexHookRegistry::issue_launch_permit(
            Arc::clone(&registry),
            TaskId::new(),
            AgentSessionId::new(),
            ManagedProcessId::new(1001, 1_700_000_000_001).unwrap(),
        )
        .expect("permit");
        let nonce = permit.registration().nonce.clone();
        assert!(permit.registration().generation >= 1);
        let rendered = format!("{permit:?}");
        assert!(!rendered.contains(&nonce));
        drop(permit);
        assert!(registry.current_registration(&nonce).is_none());
    }
}
