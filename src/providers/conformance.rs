//! Dependency-safe provider compatibility and fixture conformance lab.
//!
//! This module classifies launch-critical versus enhancement capability, pins
//! a generation's contract, quarantines unknown or malformed semantic signals,
//! and records only adapter-declared operational metrics. It does not launch a
//! provider runtime or own a journal. The fixture artifact runner lives in
//! this module and remains independent from live provider execution.

use crate::domain::{AgentSessionId, CommandId, ProviderSessionId, TaskId};
use crate::providers::adapter::MAX_PROVIDER_SIGNAL_BYTES;
use crate::providers::capabilities::{
    EvidenceDiagnosticCode, EvidenceSourceId, EvidenceStatus, ProviderAuthState,
    ProviderCapabilities, ProviderExecutable, ProviderKind, ProviderVersion,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;

pub const PROVIDER_CONFORMANCE_SCHEMA_VERSION: u32 = 1;
pub const MAX_CONFORMANCE_DECODE_BYTES: usize = MAX_PROVIDER_SIGNAL_BYTES;
pub const MAX_CONFORMANCE_DEPTH: usize = 16;
pub const MAX_CONFORMANCE_MAP_KEYS: usize = 64;
pub const MAX_CONFORMANCE_ARRAY_ITEMS: usize = 256;
pub const MAX_CONFORMANCE_NODES: usize = 1024;
pub const MAX_CONFORMANCE_NONCE_BYTES: usize = 64;
pub const MAX_PROVIDER_SMOKE_DEADLINE_MS: u64 = 120_000;
const PROVIDER_SMOKE_PROFILE: &str = "native-next-dev";
const PROVIDER_SMOKE_INSTANCE_LABEL: &str = "Next";
const PROVIDER_SMOKE_RUNTIME_KIND: &str = "native-next";
const CURSOR_FILE: &str = "cursor.json";
const MANIFEST_DOMAIN: &[u8] = b"devmanager.provider-conformance-manifest-v1.sha256\0";
const TRACE_DOMAIN: &[u8] = b"devmanager.provider-conformance-trace-v1.sha256\0";
const CURSOR_DOMAIN: &[u8] = b"devmanager.provider-conformance-cursor-v1.sha256\0";
const FIXTURE_FILE: &str = "fixture.json";
const LIFECYCLE_STEPS: [&str; 6] = [
    "launch",
    "first_output",
    "first_update",
    "outcome",
    "stop",
    "close",
];
const KNOWN_EVENT_TYPES: [&str; 3] = ["session_start", "user_message", "turn_completed"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderConformanceCaseId {
    BaselineVersion,
    NewerVersion,
    MissingHooks,
    UnknownEventType,
    MalformedEvent,
    LaunchSuccessProbeFailure,
    ExecutableReplacement,
    StrictResumeSuccess,
    StrictResumeFailure,
    TerminalOnlyFallback,
    InterruptedCaseResume,
}

impl ProviderConformanceCaseId {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BaselineVersion => "baseline_version",
            Self::NewerVersion => "newer_version",
            Self::MissingHooks => "missing_hooks",
            Self::UnknownEventType => "unknown_event_type",
            Self::MalformedEvent => "malformed_event",
            Self::LaunchSuccessProbeFailure => "launch_success_probe_failure",
            Self::ExecutableReplacement => "executable_replacement",
            Self::StrictResumeSuccess => "strict_resume_success",
            Self::StrictResumeFailure => "strict_resume_failure",
            Self::TerminalOnlyFallback => "terminal_only_fallback",
            Self::InterruptedCaseResume => "interrupted_case_resume",
        }
    }
}

impl FromStr for ProviderConformanceCaseId {
    type Err = ConformanceError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "baseline_version" => Ok(Self::BaselineVersion),
            "newer_version" => Ok(Self::NewerVersion),
            "missing_hooks" => Ok(Self::MissingHooks),
            "unknown_event_type" => Ok(Self::UnknownEventType),
            "malformed_event" => Ok(Self::MalformedEvent),
            "launch_success_probe_failure" => Ok(Self::LaunchSuccessProbeFailure),
            "executable_replacement" => Ok(Self::ExecutableReplacement),
            "strict_resume_success" => Ok(Self::StrictResumeSuccess),
            "strict_resume_failure" => Ok(Self::StrictResumeFailure),
            "terminal_only_fallback" => Ok(Self::TerminalOnlyFallback),
            "interrupted_case_resume" => Ok(Self::InterruptedCaseResume),
            other => Err(ConformanceError::UnknownCase(other.to_string())),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConformanceArm {
    Baseline,
    Variant,
}

impl ConformanceArm {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Baseline => "baseline",
            Self::Variant => "variant",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeclaredMetricId {
    ExactResumeResult,
    IdentityCorrelationResult,
    NormalizedEventCount,
    NormalizedEventOrder,
    UnknownEventFallback,
    TerminalFallback,
    InputOutcome,
    LaunchLatencyMs,
    FirstOutputLatencyMs,
    FirstUpdateLatencyMs,
    AcknowledgementLatencyMs,
    OutcomeLatencyMs,
    StopLatencyMs,
    CloseLatencyMs,
    DroppedEvents,
    CoalescedEvents,
    ForcedResync,
    ProcessResidue,
}

impl DeclaredMetricId {
    pub const ALL: [Self; 18] = [
        Self::ExactResumeResult,
        Self::IdentityCorrelationResult,
        Self::NormalizedEventCount,
        Self::NormalizedEventOrder,
        Self::UnknownEventFallback,
        Self::TerminalFallback,
        Self::InputOutcome,
        Self::LaunchLatencyMs,
        Self::FirstOutputLatencyMs,
        Self::FirstUpdateLatencyMs,
        Self::AcknowledgementLatencyMs,
        Self::OutcomeLatencyMs,
        Self::StopLatencyMs,
        Self::CloseLatencyMs,
        Self::DroppedEvents,
        Self::CoalescedEvents,
        Self::ForcedResync,
        Self::ProcessResidue,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ExactResumeResult => "exact_resume_result",
            Self::IdentityCorrelationResult => "identity_correlation_result",
            Self::NormalizedEventCount => "normalized_event_count",
            Self::NormalizedEventOrder => "normalized_event_order",
            Self::UnknownEventFallback => "unknown_event_fallback",
            Self::TerminalFallback => "terminal_fallback",
            Self::InputOutcome => "input_outcome",
            Self::LaunchLatencyMs => "launch_latency_ms",
            Self::FirstOutputLatencyMs => "first_output_latency_ms",
            Self::FirstUpdateLatencyMs => "first_update_latency_ms",
            Self::AcknowledgementLatencyMs => "acknowledgement_latency_ms",
            Self::OutcomeLatencyMs => "outcome_latency_ms",
            Self::StopLatencyMs => "stop_latency_ms",
            Self::CloseLatencyMs => "close_latency_ms",
            Self::DroppedEvents => "dropped_events",
            Self::CoalescedEvents => "coalesced_events",
            Self::ForcedResync => "forced_resync",
            Self::ProcessResidue => "process_residue",
        }
    }
}

impl FromStr for DeclaredMetricId {
    type Err = ConformanceError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .iter()
            .copied()
            .find(|metric| metric.as_str() == value)
            .ok_or_else(|| ConformanceError::UndeclaredMetric(value.to_string()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompatibilityMode {
    Semantic,
    TerminalOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompatibilityError {
    CliNotLaunchable,
}

impl fmt::Display for CompatibilityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CliNotLaunchable => {
                write!(f, "provider CLI is not launchable")
            }
        }
    }
}

impl std::error::Error for CompatibilityError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeOutcome {
    Succeeded {
        provider_session_id: ProviderSessionId,
    },
    FailedVisible {
        reason: StrictResumeFailure,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StrictResumeFailure {
    MissingProviderSessionId,
    ResumeCommandUnproven,
    NotFound,
    Incompatible,
    AuthFailure,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderEventClass {
    Known {
        kind: String,
    },
    Unknown {
        source_type: String,
        schema_version: Option<u32>,
    },
    Malformed {
        diagnostic_code: MalformedEventCode,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MalformedEventCode {
    MissingType,
    MissingProviderEventId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventDisposition {
    ProjectSemantic,
    QuarantineKeepPtyAlive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SanitizerRejection {
    PromptBody,
    ResponseBody,
    Credential,
    AbsoluteUserPath,
    ProprietarySourceBody,
}

impl fmt::Display for SanitizerRejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PromptBody => write!(f, "seeded trace contained a prompt body"),
            Self::ResponseBody => write!(f, "seeded trace contained a response body"),
            Self::Credential => write!(f, "seeded trace contained a credential"),
            Self::AbsoluteUserPath => write!(f, "seeded trace contained an absolute user path"),
            Self::ProprietarySourceBody => {
                write!(f, "seeded trace contained a proprietary source body")
            }
        }
    }
}

impl std::error::Error for SanitizerRejection {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConformanceError {
    UnknownCase(String),
    UndeclaredMetric(String),
    InvalidFixture(&'static str),
    InvalidCapabilities(String),
    InvalidProviderSessionId(String),
    Io(String),
    NoInterruptedRun,
    RunAlreadyComplete,
    Compatibility(CompatibilityError),
    DecodeBoundExceeded { bytes: usize },
    DecodeDepthExceeded { depth: usize },
    DecodeMapKeyLimit { keys: usize },
    DecodeArrayItemLimit { items: usize },
    DecodeNodeLimit { nodes: usize },
    InconsistentProviderIdentity,
    DuplicateKey,
    UnauthenticatedCursorArm,
    ImmutableArtifactChanged,
    PathEscapesLab,
    ForbiddenPathForm,
    ForbiddenProfileRoot,
    DependencyHold(ConformanceHold),
}

impl fmt::Display for ConformanceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownCase(case) => write!(f, "unknown provider conformance case: {case}"),
            Self::UndeclaredMetric(metric) => {
                write!(f, "undeclared or forbidden conformance metric: {metric}")
            }
            Self::InvalidFixture(reason) => {
                write!(f, "invalid provider conformance fixture: {reason}")
            }
            Self::InvalidCapabilities(reason) => {
                write!(f, "invalid fixture capabilities: {reason}")
            }
            Self::InvalidProviderSessionId(reason) => {
                write!(f, "invalid fixture provider session id: {reason}")
            }
            Self::Io(reason) => write!(f, "provider conformance artifact I/O failed: {reason}"),
            Self::NoInterruptedRun => {
                write!(f, "no interrupted provider conformance run to resume")
            }
            Self::RunAlreadyComplete => {
                write!(f, "completed provider conformance run cannot be resumed")
            }
            Self::Compatibility(error) => error.fmt(f),
            Self::DecodeBoundExceeded { bytes } => {
                write!(
                    f,
                    "conformance fixture exceeded {MAX_CONFORMANCE_DECODE_BYTES} bytes ({bytes})"
                )
            }
            Self::DecodeDepthExceeded { depth } => {
                write!(
                    f,
                    "conformance fixture exceeded nesting depth {MAX_CONFORMANCE_DEPTH} ({depth})"
                )
            }
            Self::DecodeMapKeyLimit { keys } => {
                write!(
                    f,
                    "conformance fixture exceeded {MAX_CONFORMANCE_MAP_KEYS} map keys ({keys})"
                )
            }
            Self::DecodeArrayItemLimit { items } => {
                write!(
                    f,
                    "conformance fixture exceeded {MAX_CONFORMANCE_ARRAY_ITEMS} array items ({items})"
                )
            }
            Self::DecodeNodeLimit { nodes } => {
                write!(
                    f,
                    "conformance fixture exceeded {MAX_CONFORMANCE_NODES} nodes ({nodes})"
                )
            }
            Self::InconsistentProviderIdentity => {
                write!(
                    f,
                    "fixture provider/kind/version is inconsistent with capabilities"
                )
            }
            Self::DuplicateKey => {
                write!(f, "conformance JSON object contained a duplicate key")
            }
            Self::UnauthenticatedCursorArm => {
                write!(
                    f,
                    "Cursor fixture arms require authenticated probe evidence"
                )
            }
            Self::ImmutableArtifactChanged => {
                write!(f, "immutable conformance artifact would change")
            }
            Self::PathEscapesLab => {
                write!(f, "conformance artifact path escapes the lab root")
            }
            Self::ForbiddenPathForm => {
                write!(
                    f,
                    "conformance path uses a forbidden UNC, device, reparse, or trailing-dot-space form"
                )
            }
            Self::ForbiddenProfileRoot => {
                write!(
                    f,
                    "conformance lab refuses the installed DevManager profile root"
                )
            }
            Self::DependencyHold(hold) => {
                write!(f, "conformance dependency hold: {}", hold.as_str())
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConformanceHold {
    ProviderRuntimeSession,
    ProviderJournal,
    ProviderSessionsCompatibilityGate,
    Phase2ConformanceArtifactRunner,
    ProviderClaudeAdapter,
    ProviderCodexAdapter,
    ProviderCursorAdapter,
}

impl ConformanceHold {
    pub const ALL: [Self; 7] = [
        Self::ProviderRuntimeSession,
        Self::ProviderJournal,
        Self::ProviderSessionsCompatibilityGate,
        Self::Phase2ConformanceArtifactRunner,
        Self::ProviderClaudeAdapter,
        Self::ProviderCodexAdapter,
        Self::ProviderCursorAdapter,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ProviderRuntimeSession => "provider_runtime_session",
            Self::ProviderJournal => "provider_journal",
            Self::ProviderSessionsCompatibilityGate => "provider_sessions_compatibility_gate",
            Self::Phase2ConformanceArtifactRunner => "phase2_conformance_artifact_runner",
            Self::ProviderClaudeAdapter => "provider_claude_adapter",
            Self::ProviderCodexAdapter => "provider_codex_adapter",
            Self::ProviderCursorAdapter => "provider_cursor_adapter",
        }
    }

    pub const fn reason(self) -> &'static str {
        match self {
            Self::ProviderRuntimeSession => {
                "src/providers/session.rs is absent; runtime generations are not launched here"
            }
            Self::ProviderJournal => {
                "src/providers/journal.rs is absent; semantic persistence is not claimed"
            }
            Self::ProviderSessionsCompatibilityGate => {
                "tests/provider_sessions.rs is absent; compatibility_ filters cannot run"
            }
            Self::Phase2ConformanceArtifactRunner => {
                "src/providers/conformance.rs is absent; the fixture manifest/trace runner is unavailable"
            }
            Self::ProviderClaudeAdapter => {
                "src/providers/claude.rs is absent; stock Claude sessions are not launched here"
            }
            Self::ProviderCodexAdapter => {
                "src/providers/codex.rs is absent; stock Codex sessions are not launched here"
            }
            Self::ProviderCursorAdapter => {
                "src/providers/cursor.rs is absent; stock Cursor sessions are not launched here"
            }
        }
    }

    const fn smoke_path(self) -> &'static str {
        match self {
            Self::ProviderRuntimeSession => "src/providers/session.rs",
            Self::ProviderJournal => "src/providers/journal.rs",
            Self::ProviderSessionsCompatibilityGate => "tests/provider_sessions.rs",
            Self::Phase2ConformanceArtifactRunner => "src/providers/conformance.rs",
            Self::ProviderClaudeAdapter => "src/providers/claude.rs",
            Self::ProviderCodexAdapter => "src/providers/codex.rs",
            Self::ProviderCursorAdapter => "src/providers/cursor.rs",
        }
    }

    const fn adapter_for(provider: ProviderKind) -> Self {
        match provider {
            ProviderKind::ClaudeCode => Self::ProviderClaudeAdapter,
            ProviderKind::Codex => Self::ProviderCodexAdapter,
            ProviderKind::Cursor => Self::ProviderCursorAdapter,
        }
    }
}

/// Catalog of hold kinds this lab can report. Presence is discovered from the
/// worktree; this slice does not claim that every kind is currently absent.
pub fn dependency_hold_catalog() -> &'static [ConformanceHold] {
    &ConformanceHold::ALL
}

/// Dependencies whose source files are actually absent under `worktree_root`.
/// Adapter/runtime holds are omitted once those files exist.
pub fn dependency_holds(worktree_root: &Path) -> Result<Vec<ConformanceHold>, ConformanceError> {
    reject_lab_root_form(worktree_root)?;
    reject_forbidden_root(worktree_root)?;
    reject_if_reparse(worktree_root)?;
    let mut holds = Vec::new();
    for hold in ConformanceHold::ALL {
        if !smoke_dependency_present(worktree_root, hold)? {
            holds.push(hold);
        }
    }
    Ok(holds)
}

/// Fixture-checkable quota surface. This is not a live CLI observation and
/// never invents a remaining percent or reset time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderQuotaFixtureState {
    MissingAdapter,
    Unsupported,
}

impl ProviderQuotaFixtureState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MissingAdapter => "missing_adapter",
            Self::Unsupported => "unsupported",
        }
    }
}

/// Distinguish a missing adapter from a present adapter that has no official
/// local stock-CLI quota command/output in current fixtures.
pub fn classify_provider_quota_fixture(
    worktree_root: &Path,
    provider: ProviderKind,
) -> Result<ProviderQuotaFixtureState, ConformanceError> {
    reject_lab_root_form(worktree_root)?;
    reject_forbidden_root(worktree_root)?;
    reject_if_reparse(worktree_root)?;
    let hold = ConformanceHold::adapter_for(provider);
    if !smoke_dependency_present(worktree_root, hold)? {
        return Ok(ProviderQuotaFixtureState::MissingAdapter);
    }
    // Observation is allowed only when an official stock CLI quota command
    // and parseable output are represented. Current fixtures have none, so
    // a present adapter stays typed Unsupported instead of missing/unavailable.
    let _no_official_surface = official_quota_fixture_path(provider);
    Ok(ProviderQuotaFixtureState::Unsupported)
}

fn official_quota_fixture_path(provider: ProviderKind) -> Option<&'static str> {
    match provider {
        ProviderKind::ClaudeCode | ProviderKind::Codex | ProviderKind::Cursor => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderSmokeArm {
    Fixture,
    Authenticated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderSmokeHold {
    Dependency(ConformanceHold),
    FixtureRuntimeUnimplemented,
}

impl ProviderSmokeHold {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Dependency(hold) => hold.as_str(),
            Self::FixtureRuntimeUnimplemented => "provider_smoke_fixture_runtime",
        }
    }

    pub const fn reason(self) -> &'static str {
        match self {
            Self::Dependency(hold) => hold.reason(),
            Self::FixtureRuntimeUnimplemented => {
                "fixture-only smoke runtime is unimplemented; this skeleton cannot PASS"
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderSmokeRejection {
    AuthenticatedWithoutOptIn,
    AuthenticatedWithoutAllowlist,
    AuthenticatedDuplicateAllowlist,
    AuthenticatedInCiOrNoninteractive,
    AuthenticatedWithoutHostRegistration,
    AuthenticatedCapabilityUnsupported,
    ProductionProfile,
    ProductionBrowserProfile,
    DeadlineOutOfBounds,
    InheritedOrSecretEnvironment,
    PromptResponseOrCredential,
}

impl fmt::Display for ProviderSmokeRejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AuthenticatedWithoutOptIn => {
                write!(
                    f,
                    "authenticated provider smoke requires explicit operator opt-in"
                )
            }
            Self::AuthenticatedWithoutAllowlist => {
                write!(
                    f,
                    "authenticated provider smoke requires an explicit provider allowlist"
                )
            }
            Self::AuthenticatedDuplicateAllowlist => {
                write!(
                    f,
                    "authenticated provider allowlist must not contain duplicates"
                )
            }
            Self::AuthenticatedInCiOrNoninteractive => {
                write!(
                    f,
                    "authenticated provider smoke refuses CI and noninteractive invocation"
                )
            }
            Self::AuthenticatedWithoutHostRegistration => {
                write!(
                    f,
                    "authenticated provider smoke requires host registration of the allowlisted runtime"
                )
            }
            Self::AuthenticatedCapabilityUnsupported => {
                write!(
                    f,
                    "authenticated provider smoke requires Supported capability evidence"
                )
            }
            Self::ProductionProfile => {
                write!(
                    f,
                    "provider smoke refuses the installed DevManager production profile"
                )
            }
            Self::ProductionBrowserProfile => {
                write!(
                    f,
                    "provider smoke refuses production browser or provider profile roots"
                )
            }
            Self::DeadlineOutOfBounds => {
                write!(
                    f,
                    "provider smoke deadline must be in 1..={MAX_PROVIDER_SMOKE_DEADLINE_MS} ms"
                )
            }
            Self::InheritedOrSecretEnvironment => {
                write!(
                    f,
                    "provider smoke requires an explicit-from-empty environment without secrets"
                )
            }
            Self::PromptResponseOrCredential => {
                write!(
                    f,
                    "provider smoke evidence must not include prompts, responses, or credentials"
                )
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderSmokeEvidence {
    executable: bool,
    version: bool,
    capabilities: bool,
    task_id: bool,
    agent_id: bool,
    generation: bool,
    action_id: bool,
    nonce: bool,
}

impl ProviderSmokeEvidence {
    pub const fn required() -> Self {
        Self {
            executable: true,
            version: true,
            capabilities: true,
            task_id: true,
            agent_id: true,
            generation: true,
            action_id: true,
            nonce: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderSmokeInvariants {
    exact_resume_failure_never_fresh: bool,
    one_provider_root: bool,
    one_pty_reader: bool,
    zero_job_listener_helper_residue: bool,
}

impl ProviderSmokeInvariants {
    pub const fn required() -> Self {
        Self {
            exact_resume_failure_never_fresh: true,
            one_provider_root: true,
            one_pty_reader: true,
            zero_job_listener_helper_residue: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderSmokeRequest {
    arm: ProviderSmokeArm,
    operator_opt_in: bool,
    provider_allowlist: Vec<ProviderKind>,
    isolated_profile_root: PathBuf,
    interactive_operator: bool,
    ci: bool,
    host_registered: bool,
    capability_supported: bool,
    deadline_ms: u64,
    explicit_environment: BTreeMap<String, String>,
}

impl ProviderSmokeRequest {
    pub fn fixture(
        isolated_profile_root: impl Into<PathBuf>,
        deadline_ms: u64,
        explicit_environment: BTreeMap<String, String>,
    ) -> Result<Self, ProviderSmokeRejection> {
        Self::new(
            ProviderSmokeArm::Fixture,
            false,
            Vec::new(),
            isolated_profile_root.into(),
            false,
            false,
            false,
            false,
            deadline_ms,
            explicit_environment,
        )
    }

    pub fn authenticated(
        isolated_profile_root: impl Into<PathBuf>,
        provider_allowlist: Vec<ProviderKind>,
        operator_opt_in: bool,
        interactive_operator: bool,
        ci: bool,
        host_registered: bool,
        capability_supported: bool,
        deadline_ms: u64,
        explicit_environment: BTreeMap<String, String>,
    ) -> Result<Self, ProviderSmokeRejection> {
        Self::new(
            ProviderSmokeArm::Authenticated,
            operator_opt_in,
            provider_allowlist,
            isolated_profile_root.into(),
            interactive_operator,
            ci,
            host_registered,
            capability_supported,
            deadline_ms,
            explicit_environment,
        )
    }

    fn new(
        arm: ProviderSmokeArm,
        operator_opt_in: bool,
        provider_allowlist: Vec<ProviderKind>,
        isolated_profile_root: PathBuf,
        interactive_operator: bool,
        ci: bool,
        host_registered: bool,
        capability_supported: bool,
        deadline_ms: u64,
        explicit_environment: BTreeMap<String, String>,
    ) -> Result<Self, ProviderSmokeRejection> {
        if deadline_ms == 0 || deadline_ms > MAX_PROVIDER_SMOKE_DEADLINE_MS {
            return Err(ProviderSmokeRejection::DeadlineOutOfBounds);
        }
        reject_smoke_environment(&explicit_environment)?;
        reject_production_identity_root(&isolated_profile_root)?;
        if arm == ProviderSmokeArm::Authenticated {
            if !operator_opt_in {
                return Err(ProviderSmokeRejection::AuthenticatedWithoutOptIn);
            }
            if provider_allowlist.is_empty() {
                return Err(ProviderSmokeRejection::AuthenticatedWithoutAllowlist);
            }
            let mut seen = BTreeMap::new();
            for provider in &provider_allowlist {
                if seen.insert(*provider, ()).is_some() {
                    return Err(ProviderSmokeRejection::AuthenticatedDuplicateAllowlist);
                }
            }
            if ci || !interactive_operator {
                return Err(ProviderSmokeRejection::AuthenticatedInCiOrNoninteractive);
            }
            if !host_registered {
                return Err(ProviderSmokeRejection::AuthenticatedWithoutHostRegistration);
            }
            if !capability_supported {
                return Err(ProviderSmokeRejection::AuthenticatedCapabilityUnsupported);
            }
        }
        Ok(Self {
            arm,
            operator_opt_in,
            provider_allowlist,
            isolated_profile_root,
            interactive_operator,
            ci,
            host_registered,
            capability_supported,
            deadline_ms,
            explicit_environment,
        })
    }

    pub const fn arm(&self) -> ProviderSmokeArm {
        self.arm
    }

    pub const fn operator_opt_in(&self) -> bool {
        self.operator_opt_in
    }

    pub fn provider_allowlist(&self) -> &[ProviderKind] {
        &self.provider_allowlist
    }

    pub fn isolated_profile_root(&self) -> &Path {
        &self.isolated_profile_root
    }

    pub const fn interactive_operator(&self) -> bool {
        self.interactive_operator
    }

    pub const fn ci(&self) -> bool {
        self.ci
    }

    pub const fn host_registered(&self) -> bool {
        self.host_registered
    }

    pub const fn capability_supported(&self) -> bool {
        self.capability_supported
    }

    pub const fn deadline_ms(&self) -> u64 {
        self.deadline_ms
    }

    pub fn explicit_environment(&self) -> &BTreeMap<String, String> {
        &self.explicit_environment
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderSmokeHoldReport {
    arm: ProviderSmokeArm,
    holds: Vec<ProviderSmokeHold>,
    required_evidence: ProviderSmokeEvidence,
    invariants: ProviderSmokeInvariants,
    launched_providers: bool,
    deadline_ms: u64,
}

impl ProviderSmokeHoldReport {
    pub fn arm(&self) -> ProviderSmokeArm {
        self.arm
    }

    pub fn holds(&self) -> &[ProviderSmokeHold] {
        &self.holds
    }

    pub const fn required_evidence(&self) -> ProviderSmokeEvidence {
        self.required_evidence
    }

    pub const fn invariants(&self) -> ProviderSmokeInvariants {
        self.invariants
    }

    pub const fn launched_providers(&self) -> bool {
        self.launched_providers
    }

    pub const fn deadline_ms(&self) -> u64 {
        self.deadline_ms
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderSmokeDisposition {
    Hold(ProviderSmokeHoldReport),
    Rejected(ProviderSmokeRejection),
}

impl ProviderSmokeDisposition {
    pub const fn is_pass(&self) -> bool {
        false
    }
}

pub fn provider_smoke_environment() -> BTreeMap<String, String> {
    let mut environment = BTreeMap::new();
    environment.insert(
        "DEVMANAGER_PROFILE".to_string(),
        PROVIDER_SMOKE_PROFILE.to_string(),
    );
    environment.insert(
        "DEVMANAGER_INSTANCE_LABEL".to_string(),
        PROVIDER_SMOKE_INSTANCE_LABEL.to_string(),
    );
    environment.insert(
        "DEVMANAGER_RUNTIME_KIND".to_string(),
        PROVIDER_SMOKE_RUNTIME_KIND.to_string(),
    );
    environment
}

pub fn discover_provider_smoke_holds(
    worktree_root: &Path,
) -> Result<Vec<ProviderSmokeHold>, ConformanceError> {
    let mut holds = vec![ProviderSmokeHold::FixtureRuntimeUnimplemented];
    for hold in dependency_holds(worktree_root)? {
        holds.push(ProviderSmokeHold::Dependency(hold));
    }
    Ok(holds)
}

pub fn reject_smoke_sensitive_payload(value: &Value) -> Result<(), ProviderSmokeRejection> {
    inspect_seeded_value(None, value)
        .map_err(|_| ProviderSmokeRejection::PromptResponseOrCredential)
}

pub fn evaluate_provider_smoke(
    request: &ProviderSmokeRequest,
    worktree_root: &Path,
) -> Result<ProviderSmokeDisposition, ConformanceError> {
    reject_lab_root_form(worktree_root)?;
    reject_forbidden_root(worktree_root)?;
    if let Err(reason) = reject_production_identity_root(&request.isolated_profile_root) {
        return Ok(ProviderSmokeDisposition::Rejected(reason));
    }
    if request.arm == ProviderSmokeArm::Authenticated {
        if !smoke_host_registered(worktree_root)? {
            return Ok(ProviderSmokeDisposition::Rejected(
                ProviderSmokeRejection::AuthenticatedWithoutHostRegistration,
            ));
        }
        if !smoke_capability_supported(worktree_root, &request.provider_allowlist)? {
            return Ok(ProviderSmokeDisposition::Rejected(
                ProviderSmokeRejection::AuthenticatedCapabilityUnsupported,
            ));
        }
    }
    let holds = discover_provider_smoke_holds(worktree_root)?;
    Ok(ProviderSmokeDisposition::Hold(ProviderSmokeHoldReport {
        arm: request.arm,
        holds,
        required_evidence: ProviderSmokeEvidence::required(),
        invariants: ProviderSmokeInvariants::required(),
        launched_providers: false,
        deadline_ms: request.deadline_ms,
    }))
}

fn smoke_dependency_present(
    worktree_root: &Path,
    hold: ConformanceHold,
) -> Result<bool, ConformanceError> {
    let relative = hold.smoke_path();
    reject_component_form_path(relative)?;
    let candidate = worktree_root.join(relative);
    Ok(match hold {
        ConformanceHold::Phase2ConformanceArtifactRunner => {
            candidate.is_dir() || candidate.is_file()
        }
        _ => candidate.is_file(),
    })
}

fn smoke_host_registered(worktree_root: &Path) -> Result<bool, ConformanceError> {
    smoke_dependency_present(worktree_root, ConformanceHold::ProviderRuntimeSession)
}

fn smoke_capability_supported(
    worktree_root: &Path,
    allowlist: &[ProviderKind],
) -> Result<bool, ConformanceError> {
    // Fail closed: this skeleton never invents Supported probe evidence.
    // An allowlisted adapter file is necessary but not sufficient.
    if allowlist.is_empty() {
        return Ok(false);
    }
    for provider in allowlist {
        let hold = ConformanceHold::adapter_for(*provider);
        if !smoke_dependency_present(worktree_root, hold)? {
            return Ok(false);
        }
    }
    Ok(false)
}

fn reject_smoke_environment(
    environment: &BTreeMap<String, String>,
) -> Result<(), ProviderSmokeRejection> {
    let expected = provider_smoke_environment();
    if environment != &expected {
        return Err(ProviderSmokeRejection::InheritedOrSecretEnvironment);
    }
    Ok(())
}

fn reject_production_identity_root(path: &Path) -> Result<(), ProviderSmokeRejection> {
    let rendered = path
        .to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase();
    if rendered.contains("com.userfirst.devmanager") {
        return Err(ProviderSmokeRejection::ProductionProfile);
    }
    if rendered.contains("/google/chrome/user data")
        || rendered.contains("/microsoft/edge/user data")
    {
        return Err(ProviderSmokeRejection::ProductionBrowserProfile);
    }
    for component in path.components() {
        match component {
            Component::Normal(name) => {
                let name = name.to_string_lossy();
                if matches!(name.as_ref(), ".claude" | ".codex" | ".cursor") {
                    return Err(ProviderSmokeRejection::ProductionBrowserProfile);
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn reject_component_form_path(relative: &str) -> Result<(), ConformanceError> {
    for part in relative.split(['/', '\\']) {
        reject_component_form(part)?;
    }
    Ok(())
}

impl std::error::Error for ConformanceError {}

/// Launch-critical interactive CLI may start as `TerminalOnly`. Exact resume
/// and semantic projection are enhancement capabilities and never keep a
/// working stock terminal from launching.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthenticatedFixture {
    schema_version: u32,
    case_id: ProviderConformanceCaseId,
    provider: ProviderKind,
    version: ProviderVersion,
    correlation: FixtureCorrelation,
    fixture_sha256: String,
    declared_metrics: Vec<DeclaredMetricId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FixtureCorrelation {
    task_id: TaskId,
    agent_id: AgentSessionId,
    generation: u64,
    action_id: CommandId,
    nonce: String,
}

impl FixtureCorrelation {
    pub const fn task_id(&self) -> TaskId {
        self.task_id
    }

    pub const fn agent_id(&self) -> AgentSessionId {
        self.agent_id
    }

    pub const fn generation(&self) -> u64 {
        self.generation
    }

    pub const fn action_id(&self) -> CommandId {
        self.action_id
    }

    pub fn nonce(&self) -> &str {
        &self.nonce
    }
}

impl AuthenticatedFixture {
    pub const fn schema_version(&self) -> u32 {
        self.schema_version
    }

    pub const fn case_id(&self) -> ProviderConformanceCaseId {
        self.case_id
    }

    pub const fn provider(&self) -> ProviderKind {
        self.provider
    }

    pub fn version(&self) -> &ProviderVersion {
        &self.version
    }

    pub const fn correlation(&self) -> &FixtureCorrelation {
        &self.correlation
    }

    pub fn fixture_sha256(&self) -> &str {
        &self.fixture_sha256
    }

    pub fn declared_metrics(&self) -> &[DeclaredMetricId] {
        &self.declared_metrics
    }
}

pub fn decode_fixture_bytes(bytes: &[u8]) -> Result<Value, ConformanceError> {
    if bytes.len() > MAX_CONFORMANCE_DECODE_BYTES {
        return Err(ConformanceError::DecodeBoundExceeded { bytes: bytes.len() });
    }
    let text = std::str::from_utf8(bytes).map_err(|_| ConformanceError::InvalidFixture("utf8"))?;
    preflight_json(text.as_bytes())?;
    serde_json::from_slice(text.as_bytes()).map_err(|_| ConformanceError::InvalidFixture("json"))
}

pub fn canonical_fixture_bytes(value: &Value) -> Result<Vec<u8>, ConformanceError> {
    let bytes = serde_json::to_vec(value).map_err(|_| ConformanceError::InvalidFixture("json"))?;
    if bytes.len() > MAX_CONFORMANCE_DECODE_BYTES {
        return Err(ConformanceError::DecodeBoundExceeded { bytes: bytes.len() });
    }
    Ok(bytes)
}

pub fn canonical_fixture_digest(value: &Value) -> Result<String, ConformanceError> {
    Ok(hex_digest(&canonical_fixture_bytes(value)?))
}

pub fn authenticate_fixture(raw: &str) -> Result<AuthenticatedFixture, ConformanceError> {
    let value = decode_fixture_bytes(raw.as_bytes())?;
    let document = parse_fixture_document(&value)?;
    reject_unauthenticated_cursor(&document)?;
    Ok(AuthenticatedFixture {
        schema_version: PROVIDER_CONFORMANCE_SCHEMA_VERSION,
        case_id: document.case_id,
        provider: document.provider,
        version: document.version.clone(),
        correlation: document.correlation.clone(),
        fixture_sha256: canonical_fixture_digest(&value)?,
        declared_metrics: document.declared_metrics,
    })
}

pub fn classify_compatibility(
    capabilities: &ProviderCapabilities,
    probe_ok: bool,
    cli_launchable: bool,
) -> Result<CompatibilityMode, CompatibilityError> {
    if !cli_launchable {
        return Err(CompatibilityError::CliNotLaunchable);
    }
    let probe_ok = probe_ok_from_evidence(capabilities, probe_ok);
    let semantic = probe_ok && capabilities.semantic_events.is_supported();
    if semantic {
        Ok(CompatibilityMode::Semantic)
    } else {
        Ok(CompatibilityMode::TerminalOnly)
    }
}

pub fn exact_resume_offered(capabilities: &ProviderCapabilities) -> bool {
    capabilities.exact_resume.is_supported() && capabilities.provider_session_id.is_supported()
}

pub fn decide_strict_resume(fixture: &Value) -> Result<ResumeOutcome, ConformanceError> {
    if fixture.get("resume_outcome").is_some() {
        return Err(ConformanceError::InvalidFixture(
            "fixtures must not self-assert resume_outcome",
        ));
    }
    let capabilities = capabilities_field(fixture)?;
    let resume_command_proven = fixture
        .get("resume_command_proven")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !resume_command_proven || !exact_resume_offered(&capabilities) {
        return Ok(ResumeOutcome::FailedVisible {
            reason: StrictResumeFailure::ResumeCommandUnproven,
        });
    }
    let Some(raw_id) = fixture.get("provider_session_id").and_then(Value::as_str) else {
        return Ok(ResumeOutcome::FailedVisible {
            reason: StrictResumeFailure::MissingProviderSessionId,
        });
    };
    let provider_session_id = ProviderSessionId::new(raw_id)
        .map_err(|error| ConformanceError::InvalidProviderSessionId(error.to_string()))?;
    if let Some(reason) = resume_failure_from_probe(&capabilities) {
        return Ok(ResumeOutcome::FailedVisible { reason });
    }
    Ok(ResumeOutcome::Succeeded {
        provider_session_id,
    })
}

pub fn classify_fixture_event(event: &Value) -> (ProviderEventClass, EventDisposition) {
    let Some(event_type) = event.get("type").and_then(Value::as_str) else {
        return (
            ProviderEventClass::Malformed {
                diagnostic_code: MalformedEventCode::MissingType,
            },
            EventDisposition::QuarantineKeepPtyAlive,
        );
    };
    if event
        .get("provider_event_id")
        .and_then(Value::as_str)
        .is_none()
    {
        return (
            ProviderEventClass::Malformed {
                diagnostic_code: MalformedEventCode::MissingProviderEventId,
            },
            EventDisposition::QuarantineKeepPtyAlive,
        );
    }
    if KNOWN_EVENT_TYPES.contains(&event_type) {
        (
            ProviderEventClass::Known {
                kind: event_type.to_string(),
            },
            EventDisposition::ProjectSemantic,
        )
    } else {
        (
            ProviderEventClass::Unknown {
                source_type: event_type.to_string(),
                schema_version: event
                    .get("schema_version")
                    .and_then(Value::as_u64)
                    .and_then(|value| u32::try_from(value).ok()),
            },
            EventDisposition::QuarantineKeepPtyAlive,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinnedGenerationContract {
    generation: u64,
    executable: ProviderExecutable,
    capabilities: ProviderCapabilities,
}

impl PinnedGenerationContract {
    pub fn pin(
        generation: u64,
        executable: ProviderExecutable,
        capabilities: ProviderCapabilities,
    ) -> Result<Self, ConformanceError> {
        capabilities
            .validate()
            .map_err(|error| ConformanceError::InvalidCapabilities(error.to_string()))?;
        Ok(Self {
            generation,
            executable,
            capabilities,
        })
    }

    pub const fn generation(&self) -> u64 {
        self.generation
    }

    pub const fn executable(&self) -> &ProviderExecutable {
        &self.executable
    }

    pub const fn capabilities(&self) -> &ProviderCapabilities {
        &self.capabilities
    }

    pub fn retain_after_executable_replacement(
        &self,
        _replacement: ProviderExecutable,
        _replacement_capabilities: ProviderCapabilities,
    ) -> Result<Self, ConformanceError> {
        Ok(self.clone())
    }

    pub fn next_generation_after_probe(
        &self,
        executable: ProviderExecutable,
        capabilities: ProviderCapabilities,
    ) -> Result<Self, ConformanceError> {
        Self::pin(self.generation.saturating_add(1), executable, capabilities)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SanitizedFixture {
    case_id: ProviderConformanceCaseId,
    provider: ProviderKind,
    raw: Value,
}

impl SanitizedFixture {
    pub const fn case_id(&self) -> ProviderConformanceCaseId {
        self.case_id
    }

    pub const fn provider(&self) -> ProviderKind {
        self.provider
    }

    pub const fn raw(&self) -> &Value {
        &self.raw
    }
}

pub fn promote_seeded_trace(raw: &str) -> Result<SanitizedFixture, SanitizerRejection> {
    let value = decode_fixture_bytes(raw.as_bytes()).map_err(|_| SanitizerRejection::PromptBody)?;
    inspect_seeded_value(None, &value)?;
    let document = parse_fixture_document(&value).map_err(|_| SanitizerRejection::PromptBody)?;
    Ok(SanitizedFixture {
        case_id: document.case_id,
        provider: document.provider,
        raw: value,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConformanceRunRecord {
    case_id: ProviderConformanceCaseId,
    arm: ConformanceArm,
    metrics: BTreeMap<DeclaredMetricId, i64>,
    pty_terminated: bool,
}

impl ConformanceRunRecord {
    pub const fn case_id(&self) -> ProviderConformanceCaseId {
        self.case_id
    }

    pub fn metric(&self, id: DeclaredMetricId) -> Option<i64> {
        self.metrics.get(&id).copied()
    }

    pub const fn pty_terminated(&self) -> bool {
        self.pty_terminated
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetricComparison {
    deltas: BTreeMap<DeclaredMetricId, i64>,
}

impl MetricComparison {
    pub fn delta(&self, id: DeclaredMetricId) -> Option<i64> {
        self.deltas.get(&id).copied()
    }

    pub fn undeclared_metric_ids(&self) -> impl Iterator<Item = &str> {
        std::iter::empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ConformanceIndex {
    records: BTreeMap<(ProviderConformanceCaseId, ConformanceArm), ConformanceRunRecord>,
}

impl ConformanceIndex {
    pub fn rebuild(
        records: impl IntoIterator<Item = ConformanceRunRecord>,
    ) -> Result<Self, ConformanceError> {
        let mut index = Self::default();
        for record in records {
            if index
                .records
                .insert((record.case_id, record.arm), record)
                .is_some()
            {
                return Err(ConformanceError::InvalidFixture("duplicate case/arm"));
            }
        }
        Ok(index)
    }

    pub fn record_count(&self) -> usize {
        self.records.len()
    }

    pub fn compare_arms(
        &self,
        left_case: ProviderConformanceCaseId,
        left_arm: ConformanceArm,
        right_case: ProviderConformanceCaseId,
        right_arm: ConformanceArm,
    ) -> Result<MetricComparison, ConformanceError> {
        let left = self.records.get(&(left_case, left_arm)).ok_or_else(|| {
            ConformanceError::UnknownCase(format!("{}_{}", left_case.as_str(), left_arm.as_str()))
        })?;
        let right = self.records.get(&(right_case, right_arm)).ok_or_else(|| {
            ConformanceError::UnknownCase(format!("{}_{}", right_case.as_str(), right_arm.as_str()))
        })?;
        let mut deltas = BTreeMap::new();
        for metric in DeclaredMetricId::ALL {
            if let (Some(left_value), Some(right_value)) =
                (left.metric(metric), right.metric(metric))
            {
                deltas.insert(metric, right_value - left_value);
            }
        }
        Ok(MetricComparison { deltas })
    }
}

#[derive(Debug, Clone)]
pub struct ProviderConformanceLab {
    location: LabLocation,
}

impl ProviderConformanceLab {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, ConformanceError> {
        let root = root.into();
        reject_lab_root_form(&root)?;
        reject_forbidden_root(&root)?;
        create_dir_all_no_reparse(&root)?;
        let location = retain_lab_location(root)?;
        reject_forbidden_root(&location.path)?;
        Ok(Self { location })
    }

    pub fn execute_fixture(
        &self,
        raw: &str,
        arm: ConformanceArm,
    ) -> Result<ConformanceRunRecord, ConformanceError> {
        let authenticated = authenticate_fixture(raw)?;
        let value = decode_fixture_bytes(raw.as_bytes())?;
        let document = parse_fixture_document(&value)?;
        let record = evaluate_fixture(&document, arm)?;
        persist_signed_run(&self.location, &authenticated, &document, arm, &record)?;
        Ok(record)
    }

    pub fn record_metrics(
        &self,
        case_id: ProviderConformanceCaseId,
        arm: ConformanceArm,
        _capabilities: &ProviderCapabilities,
        metrics: BTreeMap<String, i64>,
    ) -> Result<ConformanceRunRecord, ConformanceError> {
        let mut declared = BTreeMap::new();
        for (name, value) in metrics {
            let id = DeclaredMetricId::from_str(&name)?;
            declared.insert(id, value);
        }
        Ok(ConformanceRunRecord {
            case_id,
            arm,
            metrics: declared,
            pty_terminated: false,
        })
    }

    pub fn start_fixture(
        &self,
        raw: &str,
        arm: ConformanceArm,
    ) -> Result<ProviderConformanceRun, ConformanceError> {
        let authenticated = authenticate_fixture(raw)?;
        let value = decode_fixture_bytes(raw.as_bytes())?;
        let document = parse_fixture_document(&value)?;
        let fixture_bytes = canonical_fixture_bytes(&value)?;
        write_confined_bytes(&self.location, FIXTURE_FILE, &fixture_bytes)?;
        let mut state = DurableRunState {
            schema_version: PROVIDER_CONFORMANCE_SCHEMA_VERSION,
            case_id: document.case_id,
            arm,
            correlation: document.correlation.clone(),
            settled_steps: Vec::new(),
            complete: false,
            duplicate_settlements: 0,
            metrics: BTreeMap::new(),
            fixture_sha256: hex_digest(&fixture_bytes),
            cursor_sha256: String::new(),
        };
        seal_cursor(&mut state)?;
        write_json(&self.location, CURSOR_FILE, &state)?;
        persist_signed_run(
            &self.location,
            &authenticated,
            &document,
            arm,
            &evaluate_fixture(&document, arm)?,
        )?;
        Ok(ProviderConformanceRun {
            location: self.location.clone(),
            state,
        })
    }

    pub fn resume_interrupted(&self) -> Result<ProviderConformanceRun, ConformanceError> {
        let path = confined_child(&self.location.path, CURSOR_FILE)?;
        if symlink_metadata_if_exists(&path)?.is_none() {
            return Err(ConformanceError::NoInterruptedRun);
        }
        let state: DurableRunState = read_json(&self.location, CURSOR_FILE)?;
        verify_cursor(&state)?;
        if state.complete {
            return Err(ConformanceError::RunAlreadyComplete);
        }
        let fixture_bytes = read_confined_bounded(&self.location, FIXTURE_FILE)?;
        let fixture = decode_fixture_bytes(&fixture_bytes)?;
        let document = parse_fixture_document(&fixture)?;
        let digest = canonical_fixture_digest(&fixture)?;
        if digest != state.fixture_sha256 {
            return Err(ConformanceError::InvalidFixture(
                "interrupted fixture digest does not match cursor",
            ));
        }
        if document.case_id != state.case_id || document.correlation != state.correlation {
            return Err(ConformanceError::InvalidFixture(
                "interrupted fixture identity does not match cursor",
            ));
        }
        let manifest = verify_signed_artifact(
            &self.location,
            &signed_artifact_name("manifest", state.case_id, state.arm),
            MANIFEST_DOMAIN,
        )?;
        let signed: ConformanceManifest = serde_json::from_value(manifest)
            .map_err(|error| ConformanceError::Io(error.to_string()))?;
        if signed.case_id != state.case_id
            || signed.arm != state.arm
            || signed.correlation != state.correlation
            || signed.fixture_sha256 != state.fixture_sha256
        {
            return Err(ConformanceError::InvalidFixture(
                "signed manifest identity does not match cursor",
            ));
        }
        Ok(ProviderConformanceRun {
            location: self.location.clone(),
            state,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct DurableRunState {
    schema_version: u32,
    case_id: ProviderConformanceCaseId,
    arm: ConformanceArm,
    correlation: FixtureCorrelation,
    settled_steps: Vec<String>,
    complete: bool,
    duplicate_settlements: u64,
    metrics: BTreeMap<DeclaredMetricId, i64>,
    fixture_sha256: String,
    cursor_sha256: String,
}

#[derive(Debug, Clone)]
pub struct ProviderConformanceRun {
    location: LabLocation,
    state: DurableRunState,
}

impl ProviderConformanceRun {
    pub const fn case_id(&self) -> ProviderConformanceCaseId {
        self.state.case_id
    }

    pub fn settled_step_count(&self) -> usize {
        self.state.settled_steps.len()
    }

    pub fn settled_step_ids(&self) -> Vec<&str> {
        self.state
            .settled_steps
            .iter()
            .map(String::as_str)
            .collect()
    }

    pub const fn is_complete(&self) -> bool {
        self.state.complete
    }

    pub const fn duplicate_settlements(&self) -> u64 {
        self.state.duplicate_settlements
    }

    pub fn metric(&self, id: DeclaredMetricId) -> Option<i64> {
        self.state.metrics.get(&id).copied()
    }

    pub fn settle_next(&mut self) -> Result<(), ConformanceError> {
        if self.state.complete {
            self.state.duplicate_settlements = self.state.duplicate_settlements.saturating_add(1);
            self.persist()?;
            return Ok(());
        }
        let next = LIFECYCLE_STEPS
            .get(self.state.settled_steps.len())
            .copied()
            .ok_or(ConformanceError::RunAlreadyComplete)?;
        if self.state.settled_steps.iter().any(|step| step == next) {
            self.state.duplicate_settlements = self.state.duplicate_settlements.saturating_add(1);
            self.persist()?;
            return Ok(());
        }
        self.state.settled_steps.push(next.to_string());
        if self.state.settled_steps.len() == LIFECYCLE_STEPS.len() {
            self.state.complete = true;
            self.state
                .metrics
                .insert(DeclaredMetricId::ProcessResidue, 0);
        }
        self.persist()
    }

    pub fn interrupt(&mut self) -> Result<(), ConformanceError> {
        self.state.complete = false;
        self.persist()
    }

    fn persist(&mut self) -> Result<(), ConformanceError> {
        seal_cursor(&mut self.state)?;
        write_json(&self.location, CURSOR_FILE, &self.state)
    }
}

#[derive(Debug, Clone)]
struct FixtureDocument {
    case_id: ProviderConformanceCaseId,
    provider: ProviderKind,
    version: ProviderVersion,
    correlation: FixtureCorrelation,
    cli_launchable: bool,
    probe_ok: bool,
    capabilities: ProviderCapabilities,
    events: Vec<Value>,
    declared_metrics: Vec<DeclaredMetricId>,
    raw: Value,
}

fn parse_fixture_document(value: &Value) -> Result<FixtureDocument, ConformanceError> {
    if value.get("resume_outcome").is_some() {
        return Err(ConformanceError::InvalidFixture(
            "fixtures must not self-assert resume_outcome",
        ));
    }
    let case_id = value
        .get("case_id")
        .and_then(Value::as_str)
        .ok_or(ConformanceError::InvalidFixture("missing case_id"))?
        .parse()?;
    let provider = serde_json::from_value(
        value
            .get("provider")
            .cloned()
            .ok_or(ConformanceError::InvalidFixture("missing provider"))?,
    )
    .map_err(|_| ConformanceError::InvalidFixture("invalid provider"))?;
    let capabilities = capabilities_field(value)?;
    let version = value
        .get("version")
        .and_then(Value::as_str)
        .ok_or(ConformanceError::InvalidFixture("missing version"))
        .and_then(|version| {
            ProviderVersion::new(version)
                .map_err(|_| ConformanceError::InvalidFixture("invalid version"))
        })?;
    if provider != capabilities.kind || version.as_str() != capabilities.version.as_str() {
        return Err(ConformanceError::InconsistentProviderIdentity);
    }
    if !capabilities
        .evidence
        .iter()
        .any(|evidence| evidence.source() == EvidenceSourceId::CapabilityProbe)
    {
        return Err(ConformanceError::InvalidFixture(
            "missing capability_probe evidence",
        ));
    }
    let correlation = parse_correlation(value)?;
    let events = value
        .get("events")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let declared_metrics = value
        .get("declared_metrics")
        .and_then(Value::as_array)
        .ok_or(ConformanceError::InvalidFixture("missing declared_metrics"))?
        .iter()
        .map(|metric| {
            metric
                .as_str()
                .ok_or(ConformanceError::InvalidFixture(
                    "metric id must be a string",
                ))
                .and_then(DeclaredMetricId::from_str)
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(FixtureDocument {
        case_id,
        provider,
        version,
        correlation,
        cli_launchable: value
            .get("cli_launchable")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        probe_ok: value
            .get("probe_ok")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        capabilities,
        events,
        declared_metrics,
        raw: value.clone(),
    })
}

fn capabilities_field(value: &Value) -> Result<ProviderCapabilities, ConformanceError> {
    let capabilities = value
        .get("capabilities")
        .cloned()
        .ok_or(ConformanceError::InvalidFixture("missing capabilities"))?;
    serde_json::from_value(capabilities)
        .map_err(|error| ConformanceError::InvalidCapabilities(error.to_string()))
}

fn evaluate_fixture(
    document: &FixtureDocument,
    arm: ConformanceArm,
) -> Result<ConformanceRunRecord, ConformanceError> {
    let mode = classify_compatibility(
        &document.capabilities,
        document.probe_ok,
        document.cli_launchable,
    )
    .map_err(ConformanceError::Compatibility)?;
    let resume = decide_strict_resume(&document.raw)?;
    let identity_correlated = match &resume {
        ResumeOutcome::Succeeded { .. } => 1,
        ResumeOutcome::FailedVisible { reason } => i64::from(matches!(
            reason,
            StrictResumeFailure::NotFound
                | StrictResumeFailure::Incompatible
                | StrictResumeFailure::AuthFailure
        )),
    };
    let mut known_kinds = Vec::new();
    let mut unknown = 0_i64;
    for event in &document.events {
        match classify_fixture_event(event) {
            (ProviderEventClass::Known { kind }, EventDisposition::ProjectSemantic) => {
                known_kinds.push(kind);
            }
            (_, EventDisposition::QuarantineKeepPtyAlive) => unknown += 1,
            _ => {}
        }
    }
    let order_ok = known_kinds.windows(2).all(|pair| {
        event_rank(&pair[0]).unwrap_or(u8::MAX) <= event_rank(&pair[1]).unwrap_or(u8::MAX)
    });
    let mut metrics = BTreeMap::new();
    insert_if_declared(
        &mut metrics,
        &document.declared_metrics,
        DeclaredMetricId::ExactResumeResult,
        i64::from(matches!(resume, ResumeOutcome::Succeeded { .. })),
    );
    insert_if_declared(
        &mut metrics,
        &document.declared_metrics,
        DeclaredMetricId::IdentityCorrelationResult,
        identity_correlated,
    );
    insert_if_declared(
        &mut metrics,
        &document.declared_metrics,
        DeclaredMetricId::NormalizedEventCount,
        i64::try_from(known_kinds.len()).unwrap_or(i64::MAX),
    );
    insert_if_declared(
        &mut metrics,
        &document.declared_metrics,
        DeclaredMetricId::NormalizedEventOrder,
        i64::from(order_ok && !known_kinds.is_empty()),
    );
    insert_if_declared(
        &mut metrics,
        &document.declared_metrics,
        DeclaredMetricId::UnknownEventFallback,
        unknown,
    );
    insert_if_declared(
        &mut metrics,
        &document.declared_metrics,
        DeclaredMetricId::TerminalFallback,
        i64::from(mode == CompatibilityMode::TerminalOnly),
    );
    insert_if_declared(
        &mut metrics,
        &document.declared_metrics,
        DeclaredMetricId::ProcessResidue,
        0,
    );
    Ok(ConformanceRunRecord {
        case_id: document.case_id,
        arm,
        metrics,
        pty_terminated: false,
    })
}

fn insert_if_declared(
    metrics: &mut BTreeMap<DeclaredMetricId, i64>,
    declared: &[DeclaredMetricId],
    id: DeclaredMetricId,
    value: i64,
) {
    if declared.contains(&id) {
        metrics.insert(id, value);
    }
}

fn parse_correlation(value: &Value) -> Result<FixtureCorrelation, ConformanceError> {
    let task_id = value
        .get("task_id")
        .and_then(Value::as_str)
        .ok_or(ConformanceError::InvalidFixture("missing task_id"))
        .and_then(|id| {
            TaskId::parse(id).map_err(|_| ConformanceError::InvalidFixture("invalid task_id"))
        })?;
    let agent_id = value
        .get("agent_id")
        .and_then(Value::as_str)
        .ok_or(ConformanceError::InvalidFixture("missing agent_id"))
        .and_then(|id| {
            AgentSessionId::parse(id)
                .map_err(|_| ConformanceError::InvalidFixture("invalid agent_id"))
        })?;
    let action_id = value
        .get("action_id")
        .and_then(Value::as_str)
        .ok_or(ConformanceError::InvalidFixture("missing action_id"))
        .and_then(|id| {
            CommandId::parse(id).map_err(|_| ConformanceError::InvalidFixture("invalid action_id"))
        })?;
    let generation = value
        .get("generation")
        .and_then(Value::as_u64)
        .ok_or(ConformanceError::InvalidFixture("missing generation"))?;
    if generation == 0 {
        return Err(ConformanceError::InvalidFixture(
            "generation must be non-zero",
        ));
    }
    let nonce = value
        .get("nonce")
        .and_then(Value::as_str)
        .ok_or(ConformanceError::InvalidFixture("missing nonce"))?
        .to_string();
    if nonce.is_empty()
        || nonce.len() > MAX_CONFORMANCE_NONCE_BYTES
        || nonce.chars().any(char::is_control)
    {
        return Err(ConformanceError::InvalidFixture("invalid nonce"));
    }
    Ok(FixtureCorrelation {
        task_id,
        agent_id,
        generation,
        action_id,
        nonce,
    })
}

fn probe_ok_from_evidence(capabilities: &ProviderCapabilities, probe_ok: bool) -> bool {
    match capabilities
        .evidence
        .iter()
        .find(|evidence| evidence.source() == EvidenceSourceId::CapabilityProbe)
        .map(crate::providers::capabilities::CapabilityEvidence::status)
    {
        Some(EvidenceStatus::Failed | EvidenceStatus::Unsupported) => false,
        Some(EvidenceStatus::Supported) => probe_ok,
        _ => probe_ok,
    }
}

fn resume_failure_from_probe(capabilities: &ProviderCapabilities) -> Option<StrictResumeFailure> {
    for evidence in &capabilities.evidence {
        if evidence.source() == EvidenceSourceId::AuthStatusProbe
            && matches!(
                evidence.status(),
                EvidenceStatus::AuthRequired | EvidenceStatus::Failed
            )
        {
            return Some(StrictResumeFailure::AuthFailure);
        }
        if evidence.source() == EvidenceSourceId::CapabilityProbe
            && evidence.status() == EvidenceStatus::Failed
        {
            return Some(
                match evidence
                    .diagnostic()
                    .map(crate::providers::capabilities::EvidenceDiagnostic::code)
                {
                    Some(EvidenceDiagnosticCode::ProbeFailed)
                    | Some(EvidenceDiagnosticCode::ExecutableMissing) => {
                        StrictResumeFailure::NotFound
                    }
                    Some(EvidenceDiagnosticCode::AuthenticationRequired) => {
                        StrictResumeFailure::AuthFailure
                    }
                    _ => StrictResumeFailure::Incompatible,
                },
            );
        }
    }
    None
}

fn reject_unauthenticated_cursor(document: &FixtureDocument) -> Result<(), ConformanceError> {
    if document.provider != ProviderKind::Cursor {
        return Ok(());
    }
    let authenticated = document.capabilities.auth_state
        == ProviderAuthState::AuthenticatedSubscription
        && document.capabilities.evidence.iter().any(|evidence| {
            evidence.source() == EvidenceSourceId::AuthStatusProbe
                && evidence.status() == EvidenceStatus::Authenticated
        });
    if authenticated {
        Ok(())
    } else {
        Err(ConformanceError::UnauthenticatedCursorArm)
    }
}

fn event_rank(kind: &str) -> Option<u8> {
    match kind {
        "session_start" => Some(0),
        "user_message" => Some(1),
        "turn_completed" => Some(2),
        _ => None,
    }
}

fn inspect_seeded_value(key: Option<&str>, value: &Value) -> Result<(), SanitizerRejection> {
    if let Some(key) = key {
        let normalized = key.to_ascii_lowercase();
        if matches!(
            normalized.as_str(),
            "prompt" | "prompts" | "user_prompt" | "user_prompt_body" | "message_body"
        ) {
            return Err(SanitizerRejection::PromptBody);
        }
        if matches!(
            normalized.as_str(),
            "response" | "assistant_response" | "completion" | "assistant_text"
        ) {
            return Err(SanitizerRejection::ResponseBody);
        }
        if matches!(
            normalized.as_str(),
            "api_key"
                | "token"
                | "password"
                | "secret"
                | "credential"
                | "authorization"
                | "cookie"
                | "access_token"
                | "private_key"
                | "auth_token"
        ) {
            return Err(SanitizerRejection::Credential);
        }
        if matches!(
            normalized.as_str(),
            "source_body" | "source_code" | "file_body" | "file_contents" | "proprietary_source"
        ) {
            return Err(SanitizerRejection::ProprietarySourceBody);
        }
    }
    match value {
        Value::String(text) => {
            if looks_like_absolute_user_path(text) {
                return Err(SanitizerRejection::AbsoluteUserPath);
            }
        }
        Value::Object(map) => {
            for (child_key, child) in map {
                inspect_seeded_value(Some(child_key), child)?;
            }
        }
        Value::Array(items) => {
            for child in items {
                inspect_seeded_value(None, child)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn looks_like_absolute_user_path(value: &str) -> bool {
    let normalized = value.replace('\\', "/").to_ascii_lowercase();
    normalized.contains(":/users/")
        || normalized.starts_with("/users/")
        || normalized.starts_with("/home/")
}

fn hex_digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn domain_hex(domain: &[u8], bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update(bytes);
    hex_digest(&hasher.finalize())
}

fn signed_artifact_name(
    kind: &str,
    case_id: ProviderConformanceCaseId,
    arm: ConformanceArm,
) -> String {
    format!("{kind}_{}_{}", case_id.as_str(), arm.as_str())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SignedArtifact<T> {
    payload: T,
    sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ConformanceManifest {
    schema_version: u32,
    case_id: ProviderConformanceCaseId,
    arm: ConformanceArm,
    provider: ProviderKind,
    version: ProviderVersion,
    correlation: FixtureCorrelation,
    fixture_sha256: String,
    declared_metrics: Vec<DeclaredMetricId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ConformanceTrace {
    case_id: ProviderConformanceCaseId,
    arm: ConformanceArm,
    correlation: FixtureCorrelation,
    fixture_sha256: String,
    metrics: BTreeMap<DeclaredMetricId, i64>,
}

fn persist_signed_run(
    location: &LabLocation,
    authenticated: &AuthenticatedFixture,
    document: &FixtureDocument,
    arm: ConformanceArm,
    record: &ConformanceRunRecord,
) -> Result<(), ConformanceError> {
    write_signed_artifact(
        location,
        &signed_artifact_name("manifest", document.case_id, arm),
        MANIFEST_DOMAIN,
        &ConformanceManifest {
            schema_version: authenticated.schema_version(),
            case_id: document.case_id,
            arm,
            provider: authenticated.provider(),
            version: authenticated.version().clone(),
            correlation: document.correlation.clone(),
            fixture_sha256: authenticated.fixture_sha256().to_string(),
            declared_metrics: authenticated.declared_metrics().to_vec(),
        },
    )?;
    write_signed_artifact(
        location,
        &signed_artifact_name("trace", document.case_id, arm),
        TRACE_DOMAIN,
        &ConformanceTrace {
            case_id: document.case_id,
            arm,
            correlation: document.correlation.clone(),
            fixture_sha256: authenticated.fixture_sha256().to_string(),
            metrics: record.metrics.clone(),
        },
    )?;
    Ok(())
}

fn write_signed_artifact<T: Serialize>(
    location: &LabLocation,
    name: &str,
    domain: &[u8],
    payload: &T,
) -> Result<String, ConformanceError> {
    let payload_value =
        serde_json::to_value(payload).map_err(|error| ConformanceError::Io(error.to_string()))?;
    let payload_bytes = canonical_fixture_bytes(&payload_value)?;
    let sha256 = domain_hex(domain, &payload_bytes);
    let encoded = serde_json::to_vec(&SignedArtifact {
        payload,
        sha256: sha256.clone(),
    })
    .map_err(|error| ConformanceError::Io(error.to_string()))?;
    let path = confined_child(&location.path, name)?;
    if symlink_metadata_if_exists(&path)?.is_some() {
        let existing = read_confined_bounded(location, name)?;
        if existing != encoded {
            return Err(ConformanceError::ImmutableArtifactChanged);
        }
        return Ok(sha256);
    }
    write_confined_bytes(location, name, &encoded)?;
    Ok(sha256)
}

fn verify_signed_artifact(
    location: &LabLocation,
    name: &str,
    domain: &[u8],
) -> Result<Value, ConformanceError> {
    let bytes = read_confined_bounded(location, name)?;
    let envelope: SignedArtifact<Value> = {
        let value = decode_fixture_bytes(&bytes)?;
        serde_json::from_value(value).map_err(|error| ConformanceError::Io(error.to_string()))?
    };
    let payload_bytes = serde_json::to_vec(&envelope.payload)
        .map_err(|error| ConformanceError::Io(error.to_string()))?;
    if domain_hex(domain, &payload_bytes) != envelope.sha256 {
        return Err(ConformanceError::InvalidFixture(
            "signed artifact digest does not match payload",
        ));
    }
    Ok(envelope.payload)
}

fn seal_cursor(state: &mut DurableRunState) -> Result<(), ConformanceError> {
    state.cursor_sha256.clear();
    let body =
        serde_json::to_vec(&*state).map_err(|error| ConformanceError::Io(error.to_string()))?;
    let mut hasher = Sha256::new();
    hasher.update(CURSOR_DOMAIN);
    hasher.update(state.fixture_sha256.as_bytes());
    hasher.update(&body);
    state.cursor_sha256 = hex_digest(&hasher.finalize());
    Ok(())
}

fn verify_cursor(state: &DurableRunState) -> Result<(), ConformanceError> {
    let expected = state.cursor_sha256.clone();
    let mut probe = state.clone();
    seal_cursor(&mut probe)?;
    if probe.cursor_sha256 != expected {
        return Err(ConformanceError::InvalidFixture(
            "unauthenticated cursor digest",
        ));
    }
    Ok(())
}

fn preflight_json(bytes: &[u8]) -> Result<(), ConformanceError> {
    let mut scanner = JsonPreflight::new(bytes);
    scanner.scan_value(1)?;
    scanner.finish()
}

struct JsonPreflight<'a> {
    bytes: &'a [u8],
    offset: usize,
    nodes: usize,
}

impl<'a> JsonPreflight<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            offset: 0,
            nodes: 0,
        }
    }

    fn rest(&self) -> &[u8] {
        &self.bytes[self.offset..]
    }

    fn bump(&mut self) {
        self.offset += 1;
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.offset).copied()
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.bump();
        }
    }

    fn expect(&mut self, byte: u8) -> Result<(), ConformanceError> {
        if self.peek() == Some(byte) {
            self.bump();
            Ok(())
        } else {
            Err(ConformanceError::InvalidFixture("json"))
        }
    }

    fn account_node(&mut self) -> Result<(), ConformanceError> {
        self.nodes = self.nodes.saturating_add(1);
        if self.nodes > MAX_CONFORMANCE_NODES {
            Err(ConformanceError::DecodeNodeLimit { nodes: self.nodes })
        } else {
            Ok(())
        }
    }

    fn scan_value(&mut self, depth: usize) -> Result<(), ConformanceError> {
        if depth > MAX_CONFORMANCE_DEPTH {
            return Err(ConformanceError::DecodeDepthExceeded { depth });
        }
        self.skip_ws();
        self.account_node()?;
        match self.peek() {
            Some(b'{') => self.scan_object(depth),
            Some(b'[') => self.scan_array(depth),
            Some(b'"') => self.scan_string(),
            Some(b't') => self.scan_ident(b"true"),
            Some(b'f') => self.scan_ident(b"false"),
            Some(b'n') => self.scan_ident(b"null"),
            Some(b'-' | b'0'..=b'9') => self.scan_number(),
            _ => Err(ConformanceError::InvalidFixture("json")),
        }
    }

    fn scan_object(&mut self, depth: usize) -> Result<(), ConformanceError> {
        self.bump();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.bump();
            return Ok(());
        }
        let mut keys = 0_usize;
        let mut seen = std::collections::BTreeSet::new();
        loop {
            keys += 1;
            if keys > MAX_CONFORMANCE_MAP_KEYS {
                return Err(ConformanceError::DecodeMapKeyLimit { keys });
            }
            self.skip_ws();
            if self.peek() != Some(b'"') {
                return Err(ConformanceError::InvalidFixture("json"));
            }
            let key = self.scan_string_value()?;
            if !seen.insert(key) {
                return Err(ConformanceError::DuplicateKey);
            }
            self.skip_ws();
            self.expect(b':')?;
            self.scan_value(depth.saturating_add(1))?;
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.bump();
                    continue;
                }
                Some(b'}') => {
                    self.bump();
                    return Ok(());
                }
                _ => return Err(ConformanceError::InvalidFixture("json")),
            }
        }
    }

    fn scan_array(&mut self, depth: usize) -> Result<(), ConformanceError> {
        self.bump();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.bump();
            return Ok(());
        }
        let mut items = 0_usize;
        loop {
            items += 1;
            if items > MAX_CONFORMANCE_ARRAY_ITEMS {
                return Err(ConformanceError::DecodeArrayItemLimit { items });
            }
            self.scan_value(depth.saturating_add(1))?;
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.bump();
                    continue;
                }
                Some(b']') => {
                    self.bump();
                    return Ok(());
                }
                _ => return Err(ConformanceError::InvalidFixture("json")),
            }
        }
    }

    fn scan_string(&mut self) -> Result<(), ConformanceError> {
        self.scan_string_value().map(|_| ())
    }

    fn scan_string_value(&mut self) -> Result<String, ConformanceError> {
        self.expect(b'"')?;
        let mut decoded = String::new();
        loop {
            match self.peek() {
                None => return Err(ConformanceError::InvalidFixture("json")),
                Some(b'"') => {
                    self.bump();
                    return Ok(decoded);
                }
                Some(b'\\') => {
                    self.bump();
                    match self.peek() {
                        Some(b'"') => {
                            decoded.push('"');
                            self.bump();
                        }
                        Some(b'\\') => {
                            decoded.push('\\');
                            self.bump();
                        }
                        Some(b'/') => {
                            decoded.push('/');
                            self.bump();
                        }
                        Some(b'b') => {
                            decoded.push('\u{0008}');
                            self.bump();
                        }
                        Some(b'f') => {
                            decoded.push('\u{000c}');
                            self.bump();
                        }
                        Some(b'n') => {
                            decoded.push('\n');
                            self.bump();
                        }
                        Some(b'r') => {
                            decoded.push('\r');
                            self.bump();
                        }
                        Some(b't') => {
                            decoded.push('\t');
                            self.bump();
                        }
                        Some(b'u') => {
                            self.bump();
                            let mut hex = 0_u32;
                            for _ in 0..4 {
                                let digit = match self.peek() {
                                    Some(byte @ b'0'..=b'9') => u32::from(byte - b'0'),
                                    Some(byte @ b'a'..=b'f') => u32::from(byte - b'a' + 10),
                                    Some(byte @ b'A'..=b'F') => u32::from(byte - b'A' + 10),
                                    _ => return Err(ConformanceError::InvalidFixture("json")),
                                };
                                hex = (hex << 4) | digit;
                                self.bump();
                            }
                            decoded.push(
                                char::from_u32(hex)
                                    .ok_or(ConformanceError::InvalidFixture("json"))?,
                            );
                        }
                        _ => return Err(ConformanceError::InvalidFixture("json")),
                    }
                }
                Some(byte) if byte < 0x20 => {
                    return Err(ConformanceError::InvalidFixture("json"));
                }
                Some(byte) if byte < 0x80 => {
                    decoded.push(char::from(byte));
                    self.bump();
                }
                Some(_) => {
                    let rest = self.rest();
                    let text = std::str::from_utf8(rest)
                        .map_err(|_| ConformanceError::InvalidFixture("json"))?;
                    let next = text
                        .chars()
                        .next()
                        .ok_or(ConformanceError::InvalidFixture("json"))?;
                    decoded.push(next);
                    self.offset += next.len_utf8();
                }
            }
        }
    }

    fn scan_ident(&mut self, expected: &[u8]) -> Result<(), ConformanceError> {
        if self.rest().starts_with(expected) {
            self.offset += expected.len();
            Ok(())
        } else {
            Err(ConformanceError::InvalidFixture("json"))
        }
    }

    fn scan_number(&mut self) -> Result<(), ConformanceError> {
        if self.peek() == Some(b'-') {
            self.bump();
        }
        match self.peek() {
            Some(b'0') => self.bump(),
            Some(b'1'..=b'9') => {
                self.bump();
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.bump();
                }
            }
            _ => return Err(ConformanceError::InvalidFixture("json")),
        }
        if self.peek() == Some(b'.') {
            self.bump();
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(ConformanceError::InvalidFixture("json"));
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.bump();
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.bump();
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.bump();
            }
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(ConformanceError::InvalidFixture("json"));
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.bump();
            }
        }
        Ok(())
    }

    fn finish(mut self) -> Result<(), ConformanceError> {
        self.skip_ws();
        if self.offset == self.bytes.len() {
            Ok(())
        } else {
            Err(ConformanceError::InvalidFixture("json"))
        }
    }
}

fn reject_forbidden_root(path: &Path) -> Result<(), ConformanceError> {
    let rendered = path
        .to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase();
    if rendered.contains("com.userfirst.devmanager") {
        return Err(ConformanceError::ForbiddenProfileRoot);
    }
    Ok(())
}

fn is_unc_or_device(raw: &str) -> bool {
    let normalized = raw.replace('/', "\\");
    normalized.starts_with("\\\\")
}

fn is_reserved_dos_device(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name);
    matches!(
        stem.to_ascii_uppercase().as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    )
}

fn reject_component_form(name: &str) -> Result<(), ConformanceError> {
    if name.is_empty() || name == "." || name == ".." || name.contains("..") {
        return Err(ConformanceError::PathEscapesLab);
    }
    if is_unc_or_device(name)
        || name.ends_with('.')
        || name.ends_with(' ')
        || is_reserved_dos_device(name)
    {
        return Err(ConformanceError::ForbiddenPathForm);
    }
    if name.contains('/') || name.contains('\\') || name.contains(':') {
        return Err(ConformanceError::PathEscapesLab);
    }
    Ok(())
}

fn reject_lab_root_form(path: &Path) -> Result<(), ConformanceError> {
    let raw = path.as_os_str().to_string_lossy();
    if is_unc_or_device(&raw) {
        return Err(ConformanceError::ForbiddenPathForm);
    }
    if !path.is_absolute() {
        return Err(ConformanceError::PathEscapesLab);
    }
    for component in path.components() {
        match component {
            Component::CurDir | Component::ParentDir => {
                return Err(ConformanceError::PathEscapesLab);
            }
            Component::Normal(name) => reject_component_form(&name.to_string_lossy())?,
            Component::Prefix(_) | Component::RootDir => {}
        }
    }
    Ok(())
}

pub fn confined_artifact_path(root: &Path, name: &str) -> Result<PathBuf, ConformanceError> {
    reject_lab_root_form(root)?;
    reject_forbidden_root(root)?;
    reject_if_reparse(root)?;
    confined_child(root, name)
}

fn confined_child(root: &Path, name: &str) -> Result<PathBuf, ConformanceError> {
    reject_component_form(name)?;
    let candidate = root.join(name);
    if candidate.parent().is_some_and(|parent| parent != root) {
        return Err(ConformanceError::PathEscapesLab);
    }
    Ok(candidate)
}

#[derive(Debug, Clone)]
struct LabLocation {
    path: PathBuf,
    #[cfg(windows)]
    identity: WindowsFileIdentity,
}

#[cfg(windows)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WindowsFileIdentity {
    volume_serial_number: u32,
    file_index_high: u32,
    file_index_low: u32,
}

#[cfg(windows)]
impl From<windows::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION> for WindowsFileIdentity {
    fn from(info: windows::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION) -> Self {
        Self {
            volume_serial_number: info.dwVolumeSerialNumber,
            file_index_high: info.nFileIndexHigh,
            file_index_low: info.nFileIndexLow,
        }
    }
}

fn metadata_is_reparse(metadata: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x0000_0400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

fn symlink_metadata_if_exists(path: &Path) -> Result<Option<std::fs::Metadata>, ConformanceError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(ConformanceError::Io(error.to_string())),
    }
}

fn reject_if_reparse(path: &Path) -> Result<(), ConformanceError> {
    if let Some(metadata) = symlink_metadata_if_exists(path)? {
        if metadata_is_reparse(&metadata) {
            return Err(ConformanceError::ForbiddenPathForm);
        }
    }
    Ok(())
}

fn create_dir_all_no_reparse(path: &Path) -> Result<(), ConformanceError> {
    let mut acc = PathBuf::new();
    for component in path.components() {
        acc.push(component);
        match symlink_metadata_if_exists(&acc)? {
            Some(metadata) => {
                if metadata_is_reparse(&metadata) {
                    return Err(ConformanceError::ForbiddenPathForm);
                }
                if !metadata.is_dir() {
                    return Err(ConformanceError::Io(format!(
                        "{} is not a directory",
                        acc.display()
                    )));
                }
            }
            None => {
                fs::create_dir(&acc).map_err(|error| ConformanceError::Io(error.to_string()))?;
                reject_if_reparse(&acc)?;
            }
        }
    }
    Ok(())
}

#[cfg(windows)]
fn file_information(
    file: &File,
) -> Result<windows::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION, ConformanceError> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };

    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    let handle = HANDLE(file.as_raw_handle());
    unsafe { GetFileInformationByHandle(handle, &mut information) }
        .map_err(|error| ConformanceError::Io(error.to_string()))?;
    Ok(information)
}

#[cfg(windows)]
fn open_no_reparse(
    path: &Path,
    directory: bool,
) -> Result<(File, WindowsFileIdentity), ConformanceError> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    reject_if_reparse(path)?;
    let mut options = OpenOptions::new();
    options
        .read(true)
        .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0 | FILE_SHARE_DELETE.0);
    let mut flags = FILE_FLAG_OPEN_REPARSE_POINT.0;
    if directory {
        flags |= FILE_FLAG_BACKUP_SEMANTICS.0;
    }
    let file = options
        .custom_flags(flags)
        .open(path)
        .map_err(|error| ConformanceError::Io(error.to_string()))?;
    let info = file_information(&file)?;
    if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
        return Err(ConformanceError::ForbiddenPathForm);
    }
    Ok((file, WindowsFileIdentity::from(info)))
}

fn retain_lab_location(path: PathBuf) -> Result<LabLocation, ConformanceError> {
    #[cfg(windows)]
    {
        let (_file, identity) = open_no_reparse(&path, true)?;
        Ok(LabLocation { path, identity })
    }
    #[cfg(not(windows))]
    {
        reject_if_reparse(&path)?;
        Ok(LabLocation { path })
    }
}

fn assert_parent_identity(location: &LabLocation, child: &Path) -> Result<(), ConformanceError> {
    let parent = child.parent().ok_or(ConformanceError::PathEscapesLab)?;
    if parent != location.path {
        return Err(ConformanceError::PathEscapesLab);
    }
    #[cfg(windows)]
    {
        let (_file, identity) = open_no_reparse(parent, true)?;
        if identity != location.identity {
            return Err(ConformanceError::PathEscapesLab);
        }
    }
    Ok(())
}

fn write_confined_bytes(
    location: &LabLocation,
    name: &str,
    bytes: &[u8],
) -> Result<(), ConformanceError> {
    if bytes.len() > MAX_CONFORMANCE_DECODE_BYTES {
        return Err(ConformanceError::DecodeBoundExceeded { bytes: bytes.len() });
    }
    let path = confined_child(&location.path, name)?;
    assert_parent_identity(location, &path)?;
    reject_if_reparse(&path)?;
    let mut options = OpenOptions::new();
    options.create(true).write(true).truncate(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows::Win32::Storage::FileSystem::{
            FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT,
        };
        let file = options
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
            .open(&path)
            .map_err(|error| ConformanceError::Io(error.to_string()))?;
        let info = file_information(&file)?;
        if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
            return Err(ConformanceError::ForbiddenPathForm);
        }
        let mut file = file;
        file.write_all(bytes)
            .map_err(|error| ConformanceError::Io(error.to_string()))?;
        file.flush()
            .map_err(|error| ConformanceError::Io(error.to_string()))?;
        return Ok(());
    }
    #[cfg(not(windows))]
    {
        let mut file = options
            .open(&path)
            .map_err(|error| ConformanceError::Io(error.to_string()))?;
        file.write_all(bytes)
            .map_err(|error| ConformanceError::Io(error.to_string()))?;
        file.flush()
            .map_err(|error| ConformanceError::Io(error.to_string()))
    }
}

fn read_confined_bounded(location: &LabLocation, name: &str) -> Result<Vec<u8>, ConformanceError> {
    let path = confined_child(&location.path, name)?;
    assert_parent_identity(location, &path)?;
    reject_if_reparse(&path)?;
    #[cfg(windows)]
    let mut file = open_no_reparse(&path, false)?.0;
    #[cfg(not(windows))]
    let mut file = File::open(&path).map_err(|error| ConformanceError::Io(error.to_string()))?;
    let mut limited =
        (&mut file).take(u64::try_from(MAX_CONFORMANCE_DECODE_BYTES).unwrap_or(u64::MAX) + 1);
    let mut bytes = Vec::new();
    limited
        .read_to_end(&mut bytes)
        .map_err(|error| ConformanceError::Io(error.to_string()))?;
    if bytes.len() > MAX_CONFORMANCE_DECODE_BYTES {
        return Err(ConformanceError::DecodeBoundExceeded { bytes: bytes.len() });
    }
    Ok(bytes)
}

fn write_json(
    location: &LabLocation,
    name: &str,
    value: &impl Serialize,
) -> Result<(), ConformanceError> {
    let encoded = serde_json::to_vec_pretty(value)
        .map_err(|error| ConformanceError::Io(error.to_string()))?;
    write_confined_bytes(location, name, &encoded)
}

fn read_json<T: for<'de> Deserialize<'de>>(
    location: &LabLocation,
    name: &str,
) -> Result<T, ConformanceError> {
    let bytes = read_confined_bounded(location, name)?;
    let value = decode_fixture_bytes(&bytes)?;
    serde_json::from_value(value).map_err(|error| ConformanceError::Io(error.to_string()))
}
