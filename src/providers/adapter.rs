//! Object-safe provider adapter and bounded native-CLI boundaries.
//!
//! This phase defines the provider-neutral contracts only. It does not launch
//! a provider runtime, infer authentication, or consume a quota subscription.

use crate::domain::ProviderSessionId;
use crate::providers::capabilities::{
    ProviderAuthEvidenceError, ProviderAuthProbeBinding, ProviderAuthProbeInvocation,
    ProviderAuthProbeObservation, ProviderAuthProbeResult, ProviderCapabilities,
    ProviderCapabilitiesError, ProviderCapability, ProviderDiscoveryError, ProviderExecutable,
    ProviderExecutableError, ProviderExecutableHandle, ProviderExecutablePolicy,
    ProviderExecutablePolicyError, ProviderKind, ProviderVersion, ProviderVersionError,
};
pub use crate::providers::journal::{
    AdapterDeliveryPermit, AdapterIngressUnavailable, JournalNormalizeError,
    NormalizedAdapterDelivery,
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

#[cfg(unix)]
use std::os::unix::process::CommandExt;
#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
unsafe extern "system" {
    fn PeekNamedPipe(
        named_pipe: *mut std::ffi::c_void,
        buffer: *mut std::ffi::c_void,
        buffer_size: u32,
        bytes_read: *mut u32,
        total_bytes_available: *mut u32,
        bytes_left_this_message: *mut u32,
    ) -> i32;

    fn SetNamedPipeHandleState(
        named_pipe: *mut std::ffi::c_void,
        mode: *const u32,
        max_collection_count: *const u32,
        collect_data_timeout: *const u32,
    ) -> i32;
}

#[cfg(windows)]
const PIPE_NOWAIT: u32 = 0x0000_0001;

pub const MAX_PROVIDER_INPUT_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderModel {
    ProviderDefault,
    CodexSol,
    CodexTerra,
    CodexLuna,
    ClaudeOpus,
    ClaudeSonnet,
    ClaudeHaiku,
}

impl ProviderModel {
    pub const fn cli_name(self) -> Option<&'static str> {
        match self {
            Self::ProviderDefault => None,
            Self::CodexSol => Some("gpt-5.6-sol"),
            Self::CodexTerra => Some("gpt-5.6-terra"),
            Self::CodexLuna => Some("gpt-5.6-luna"),
            Self::ClaudeOpus => Some("opus"),
            Self::ClaudeSonnet => Some("sonnet"),
            Self::ClaudeHaiku => Some("haiku"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderReasoningEffort {
    ProviderDefault,
    Low,
    Medium,
    High,
    ExtraHigh,
    Max,
    /// Codex exposes an additional highest reasoning tier in its live
    /// catalog. Keep it distinct from Max; the provider protocol accepts
    /// the literal ultra value and silently mapping it would change the
    /// user's requested budget.
    Ultra,
}

impl ProviderReasoningEffort {
    pub const fn cli_name(self) -> Option<&'static str> {
        match self {
            Self::ProviderDefault => None,
            Self::Low => Some("low"),
            Self::Medium => Some("medium"),
            Self::High => Some("high"),
            Self::ExtraHigh => Some("xhigh"),
            Self::Max => Some("max"),
            Self::Ultra => Some("ultra"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderAccessMode {
    FullAccess,
    WorkspaceWrite,
    ReadOnly,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderLaunchOptions {
    pub model: ProviderModel,
    pub reasoning_effort: ProviderReasoningEffort,
    pub access: ProviderAccessMode,
    /// When set, this slug is passed as `--model` and overrides the enum catalog.
    /// Used for custom models configured in provider settings. Empty/None keeps
    /// the Copy-friendly `model` enum behavior for persisted preferences.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_model_slug: Option<String>,
    /// Extra non-reserved launch arguments from the provider instance config.
    /// Applied by adapters after identity/protocol wiring; reserved overrides
    /// are rejected at settings validation time.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_launch_args: Vec<String>,
    /// Durable provider instance id selected for this launch (task binding).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_instance_id: Option<String>,
}

impl Default for ProviderLaunchOptions {
    fn default() -> Self {
        Self {
            model: ProviderModel::ProviderDefault,
            reasoning_effort: ProviderReasoningEffort::ProviderDefault,
            access: ProviderAccessMode::FullAccess,
            custom_model_slug: None,
            extra_launch_args: Vec::new(),
            provider_instance_id: None,
        }
    }
}

impl ProviderLaunchOptions {
    pub fn effective_model_slug(&self) -> Option<&str> {
        if let Some(slug) = self.custom_model_slug.as_deref() {
            let trimmed = slug.trim();
            if !trimmed.is_empty() {
                return Some(trimmed);
            }
        }
        self.model.cli_name()
    }
}
pub const MAX_PROVIDER_SIGNAL_BYTES: usize = 64 * 1024;
pub const MAX_PROVIDER_ARGUMENTS: usize = 32;
pub const MAX_PROVIDER_ARGUMENT_BYTES: usize = 2048;
pub const MAX_PROVIDER_PROBE_OUTPUT_BYTES: usize = 64 * 1024;
pub const MAX_PROVIDER_PROBE_TIMEOUT: Duration = Duration::from_secs(30);
const PROVIDER_PROBE_CLEANUP_RESERVE: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderInputError {
    Empty,
    TooLarge,
    TooManyArguments,
    ArgumentTooLong,
}

impl fmt::Display for ProviderInputError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "provider input must be non-empty"),
            Self::TooLarge => write!(f, "provider input exceeded its byte bound"),
            Self::TooManyArguments => write!(f, "provider launch contained too many arguments"),
            Self::ArgumentTooLong => write!(f, "provider launch argument exceeded its byte bound"),
        }
    }
}

impl std::error::Error for ProviderInputError {}

#[derive(Clone, PartialEq, Eq)]
pub struct ProviderInput(Vec<u8>);

impl ProviderInput {
    pub fn new(input: impl Into<Vec<u8>>) -> Result<Self, ProviderInputError> {
        let input = input.into();
        if input.is_empty() {
            return Err(ProviderInputError::Empty);
        }
        if input.len() > MAX_PROVIDER_INPUT_BYTES {
            return Err(ProviderInputError::TooLarge);
        }
        Ok(Self(input))
    }

    pub const fn len(&self) -> usize {
        self.0.len()
    }

    pub const fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub(crate) fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for ProviderInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderInput")
            .field("bytes", &self.len())
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct ProviderArgument(String);

impl fmt::Debug for ProviderArgument {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderArgument")
            .field("bytes", &self.0.len())
            .finish()
    }
}

impl ProviderArgument {
    pub fn new(value: impl Into<String>) -> Result<Self, ProviderInputError> {
        let value = value.into();
        if value.len() > MAX_PROVIDER_ARGUMENT_BYTES {
            return Err(ProviderInputError::ArgumentTooLong);
        }
        if value.chars().any(char::is_control) {
            return Err(ProviderInputError::ArgumentTooLong);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct LaunchProviderRequest {
    executable: ProviderExecutableHandle,
    input: Option<ProviderInput>,
    provider_session_id: Option<ProviderSessionId>,
    launch_options: ProviderLaunchOptions,
    /// Opaque instance scope fingerprint from the same probe/observe context.
    scope_fingerprint: Option<String>,
    /// Commitment over the sealed effective provider environment.
    env_commitment: String,
}

impl fmt::Debug for LaunchProviderRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LaunchProviderRequest")
            .field("executable", &self.executable)
            .field("has_input", &self.input.is_some())
            .field("provider_session_id", &"<redacted>")
            .field("scope_fingerprint", &self.scope_fingerprint)
            .field("env_commitment", &self.env_commitment)
            .finish()
    }
}

impl LaunchProviderRequest {
    pub fn new(
        executable: ProviderExecutableHandle,
        input: Option<ProviderInput>,
        provider_session_id: Option<ProviderSessionId>,
    ) -> Self {
        Self {
            executable,
            input,
            provider_session_id,
            launch_options: ProviderLaunchOptions::default(),
            scope_fingerprint: None,
            env_commitment: crate::providers::capabilities::commit_child_environment(
                &BTreeMap::new(),
            ),
        }
    }

    pub fn executable(&self) -> &ProviderExecutableHandle {
        &self.executable
    }

    pub fn input(&self) -> Option<&ProviderInput> {
        self.input.as_ref()
    }

    pub fn provider_session_id(&self) -> Option<&ProviderSessionId> {
        self.provider_session_id.as_ref()
    }

    pub fn with_launch_options(mut self, launch_options: ProviderLaunchOptions) -> Self {
        self.launch_options = launch_options;
        self
    }

    pub fn with_scope_fingerprint(mut self, scope_fingerprint: Option<String>) -> Self {
        self.scope_fingerprint = scope_fingerprint;
        self
    }

    pub fn with_env_commitment(mut self, env_commitment: impl Into<String>) -> Self {
        self.env_commitment = env_commitment.into();
        self
    }

    pub fn launch_options(&self) -> &ProviderLaunchOptions {
        &self.launch_options
    }

    pub fn scope_fingerprint(&self) -> Option<&str> {
        self.scope_fingerprint.as_deref()
    }

    pub fn env_commitment(&self) -> &str {
        &self.env_commitment
    }

    /// Adapter-local state key: scope fingerprint + sealed env commitment.
    pub fn scope_env_key(&self) -> String {
        provider_scope_env_key(self.scope_fingerprint(), self.env_commitment())
    }
}

/// Composite key for adapter-local observed state (never scope-only).
pub fn provider_scope_env_key(scope_fingerprint: Option<&str>, env_commitment: &str) -> String {
    format!(
        "{}|{}",
        scope_fingerprint.unwrap_or_default(),
        env_commitment
    )
}

/// Immutable per-instance probe context shared by discovery probes and launch.
#[derive(Clone, Default)]
pub struct ProviderProbeContext {
    pub child_environment: BTreeMap<std::ffi::OsString, std::ffi::OsString>,
    pub scope_fingerprint: Option<String>,
}

impl fmt::Debug for ProviderProbeContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderProbeContext")
            .field("child_environment_count", &self.child_environment.len())
            .field("scope_fingerprint", &self.scope_fingerprint)
            .finish()
    }
}

impl ProviderProbeContext {
    pub fn scope_key(&self) -> String {
        let env = crate::providers::capabilities::commit_child_environment(&self.child_environment);
        provider_scope_env_key(self.scope_fingerprint.as_deref(), &env)
    }

    pub fn from_discovery(config: &crate::providers::registry::ProviderDiscoveryConfig) -> Self {
        Self {
            child_environment: config.child_environment.clone(),
            scope_fingerprint: config
                .instance_scope
                .as_ref()
                .map(|scope| scope.as_cache_key()),
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct ProviderLaunchSpec {
    executable: ProviderExecutableHandle,
    arguments: Vec<ProviderArgument>,
}

impl fmt::Debug for ProviderLaunchSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderLaunchSpec")
            .field("executable", &self.executable)
            .field("argument_count", &self.arguments.len())
            .finish_non_exhaustive()
    }
}

impl ProviderLaunchSpec {
    pub fn new(
        executable: ProviderExecutableHandle,
        arguments: Vec<ProviderArgument>,
    ) -> Result<Self, ProviderInputError> {
        if arguments.len() > MAX_PROVIDER_ARGUMENTS {
            return Err(ProviderInputError::TooManyArguments);
        }
        Ok(Self {
            executable,
            arguments,
        })
    }

    pub fn executable(&self) -> &ProviderExecutableHandle {
        &self.executable
    }

    pub fn arguments(&self) -> impl Iterator<Item = &str> {
        self.arguments.iter().map(ProviderArgument::as_str)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderSignal {
    SessionStarted(ProviderSessionId),
    SessionEnded,
    TurnCompleted,
    PermissionRequired,
}

/// Task 4.1 keeps runtime ownership opaque until provider runtime startup is
/// implemented by the later provider-session task.
#[derive(Debug, Default)]
pub struct ProviderRuntime;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopStrategy {
    Unsupported,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderQuotaStatus {
    Available,
    Exhausted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuotaObservation {
    status: ProviderQuotaStatus,
    remaining_percent: Option<u8>,
    resets_at_ms: Option<u64>,
}

impl QuotaObservation {
    pub fn new(
        status: ProviderQuotaStatus,
        remaining_percent: Option<u8>,
        resets_at_ms: Option<u64>,
    ) -> Result<Self, ProviderInputError> {
        if remaining_percent.is_some_and(|percent| percent > 100) {
            return Err(ProviderInputError::ArgumentTooLong);
        }
        Ok(Self {
            status,
            remaining_percent,
            resets_at_ms,
        })
    }

    pub const fn status(self) -> ProviderQuotaStatus {
        self.status
    }

    pub const fn remaining_percent(self) -> Option<u8> {
        self.remaining_percent
    }

    pub const fn resets_at_ms(self) -> Option<u64> {
        self.resets_at_ms
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderProbeKind {
    Version,
    Help,
    AuthStatus,
    LoginStatus,
    ResumeHelp,
    /// Cursor `agent about --format json` health surface.
    CursorAboutJson,
    /// Cursor `about` fallback when `--format` is unsupported.
    CursorAboutPlain,
}

impl ProviderProbeKind {
    pub const fn arguments(self) -> &'static [&'static str] {
        match self {
            Self::Version => &["--version"],
            Self::Help => &["--help"],
            Self::AuthStatus => &["auth", "status"],
            Self::LoginStatus => &["login", "status"],
            Self::ResumeHelp => &["resume", "--help"],
            Self::CursorAboutJson => &["about", "--format", "json"],
            Self::CursorAboutPlain => &["about"],
        }
    }

    pub const fn for_auth_probe(provider: ProviderKind) -> Self {
        match provider {
            ProviderKind::Codex => Self::LoginStatus,
            ProviderKind::ClaudeCode => Self::AuthStatus,
            ProviderKind::Cursor => Self::CursorAboutJson,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderProbeRequestError {
    EmptyExecutable,
    ZeroTimeout,
    TimeoutTooLong,
    OutputBoundTooLarge,
    AuthStatusRequiresRunnerProof,
}

impl fmt::Display for ProviderProbeRequestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyExecutable => write!(f, "provider probe executable must be non-empty"),
            Self::ZeroTimeout => write!(f, "provider probe timeout must be non-zero"),
            Self::TimeoutTooLong => write!(f, "provider probe timeout exceeded its bound"),
            Self::OutputBoundTooLarge => write!(f, "provider probe output bound is too large"),
            Self::AuthStatusRequiresRunnerProof => {
                write!(
                    f,
                    "auth-status probe results can only be issued by the bounded runner"
                )
            }
        }
    }
}

impl std::error::Error for ProviderProbeRequestError {}

#[derive(Clone, PartialEq, Eq)]
pub struct ProviderProbeRequest {
    executable: ProviderExecutableHandle,
    kind: ProviderProbeKind,
    timeout: Duration,
    max_output_bytes: usize,
    auth_binding: Option<ProviderAuthProbeBinding>,
    /// Scoped overlay applied after the process allowlist (not process-global).
    child_environment: BTreeMap<std::ffi::OsString, std::ffi::OsString>,
    /// Opaque instance scope fingerprint for wrong-scope receipt rejection.
    scope_fingerprint: Option<String>,
}

impl fmt::Debug for ProviderProbeRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderProbeRequest")
            .field("executable_bound", &true)
            .field("kind", &self.kind)
            .field("timeout", &self.timeout)
            .field("max_output_bytes", &self.max_output_bytes)
            .field("auth_bound", &self.auth_binding.is_some())
            .field("child_environment_count", &self.child_environment.len())
            .field("scope_fingerprint", &self.scope_fingerprint)
            .finish()
    }
}

impl ProviderProbeRequest {
    /// Cold `claude.exe` / Defender scans routinely miss a 5s spawn budget.
    pub const DEFAULT_TIMEOUT: Duration = MAX_PROVIDER_PROBE_TIMEOUT;
    pub const DEFAULT_MAX_OUTPUT_BYTES: usize = MAX_PROVIDER_PROBE_OUTPUT_BYTES;

    pub fn version(
        executable: ProviderExecutableHandle,
    ) -> Result<Self, ProviderProbeRequestError> {
        Self::new(executable, ProviderProbeKind::Version)
    }

    pub fn help(executable: ProviderExecutableHandle) -> Result<Self, ProviderProbeRequestError> {
        Self::new(executable, ProviderProbeKind::Help)
    }

    pub fn auth_status(
        executable: ProviderExecutableHandle,
    ) -> Result<Self, ProviderProbeRequestError> {
        Self::new(executable, ProviderProbeKind::AuthStatus)
    }

    pub fn login_status(
        executable: ProviderExecutableHandle,
    ) -> Result<Self, ProviderProbeRequestError> {
        Self::new(executable, ProviderProbeKind::LoginStatus)
    }

    pub fn resume_help(
        executable: ProviderExecutableHandle,
    ) -> Result<Self, ProviderProbeRequestError> {
        Self::new(executable, ProviderProbeKind::ResumeHelp)
    }

    pub fn new(
        executable: ProviderExecutableHandle,
        kind: ProviderProbeKind,
    ) -> Result<Self, ProviderProbeRequestError> {
        Self::with_limits(
            executable,
            kind,
            Self::DEFAULT_TIMEOUT,
            Self::DEFAULT_MAX_OUTPUT_BYTES,
        )
    }

    pub fn with_limits(
        executable: ProviderExecutableHandle,
        kind: ProviderProbeKind,
        timeout: Duration,
        max_output_bytes: usize,
    ) -> Result<Self, ProviderProbeRequestError> {
        if timeout.is_zero() {
            return Err(ProviderProbeRequestError::ZeroTimeout);
        }
        if timeout > MAX_PROVIDER_PROBE_TIMEOUT {
            return Err(ProviderProbeRequestError::TimeoutTooLong);
        }
        if max_output_bytes == 0 || max_output_bytes > MAX_PROVIDER_PROBE_OUTPUT_BYTES {
            return Err(ProviderProbeRequestError::OutputBoundTooLarge);
        }
        Ok(Self {
            executable,
            kind,
            timeout,
            max_output_bytes,
            auth_binding: None,
            child_environment: BTreeMap::new(),
            scope_fingerprint: None,
        })
    }

    pub fn with_child_environment(
        mut self,
        environment: BTreeMap<std::ffi::OsString, std::ffi::OsString>,
    ) -> Self {
        self.child_environment = environment;
        self
    }

    pub fn with_scope_fingerprint(mut self, fingerprint: Option<String>) -> Self {
        self.scope_fingerprint = fingerprint;
        self
    }

    pub fn child_environment(&self) -> &BTreeMap<std::ffi::OsString, std::ffi::OsString> {
        &self.child_environment
    }

    pub fn scope_fingerprint(&self) -> Option<&str> {
        self.scope_fingerprint.as_deref()
    }

    /// Binds this request to one exact issued auth invocation.  The nonce and
    /// generation are private correlation material copied by the invocation;
    /// callers cannot select or replace them.
    pub fn bind_to_auth_invocation(
        mut self,
        invocation: &ProviderAuthProbeInvocation,
    ) -> Result<Self, ProviderAuthEvidenceError> {
        if self.kind != ProviderProbeKind::for_auth_probe(invocation.provider_kind())
            || self.executable != *invocation.executable_handle()
        {
            return Err(ProviderAuthEvidenceError::RequestBindingMismatch);
        }
        if self.scope_fingerprint.as_deref() != invocation.scope_fingerprint() {
            return Err(ProviderAuthEvidenceError::RequestBindingMismatch);
        }
        let env_commitment =
            crate::providers::capabilities::commit_child_environment(&self.child_environment);
        if env_commitment != invocation.env_commitment() {
            return Err(ProviderAuthEvidenceError::RequestBindingMismatch);
        }
        let binding = invocation.binding();
        if self
            .auth_binding
            .as_ref()
            .is_some_and(|existing| existing != &binding)
        {
            return Err(ProviderAuthEvidenceError::RequestBindingMismatch);
        }
        self.auth_binding = Some(binding);
        Ok(self)
    }

    pub(crate) fn auth_binding_matches(&self, invocation: &ProviderAuthProbeInvocation) -> bool {
        self.auth_binding.as_ref() == Some(&invocation.binding())
    }

    pub fn executable(&self) -> &ProviderExecutableHandle {
        &self.executable
    }

    pub const fn kind(&self) -> ProviderProbeKind {
        self.kind
    }

    pub const fn arguments(&self) -> &'static [&'static str] {
        self.kind.arguments()
    }

    pub const fn uses_null_stdin(&self) -> bool {
        true
    }

    pub const fn uses_shell(&self) -> bool {
        false
    }

    pub const fn strips_api_key_environment(&self) -> bool {
        true
    }

    pub const fn kills_descendants_on_timeout(&self) -> bool {
        true
    }

    pub const fn timeout(&self) -> Duration {
        self.timeout
    }

    pub const fn max_output_bytes(&self) -> usize {
        self.max_output_bytes
    }

    fn binding(&self) -> ProviderProbeRequestBinding {
        ProviderProbeRequestBinding {
            executable: self.executable.clone(),
            kind: self.kind,
            timeout: self.timeout,
            max_output_bytes: self.max_output_bytes,
            auth_binding: self.auth_binding.clone(),
            scope_fingerprint: self.scope_fingerprint.clone(),
            env_commitment: crate::providers::capabilities::commit_child_environment(
                &self.child_environment,
            ),
        }
    }
}

impl ProviderAuthProbeInvocation {
    /// Produces an auth-status request correlated to this exact issued
    /// invocation.  Reusing the returned request with another invocation is
    /// rejected by the registry's nonce+generation check.
    pub fn bind_request(
        &self,
        request: ProviderProbeRequest,
    ) -> Result<ProviderProbeRequest, ProviderAuthEvidenceError> {
        request.bind_to_auth_invocation(self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderProbeFailureCode {
    ExecutableMissing,
    PermissionDenied,
    SpawnFailed,
    WaitFailed,
    DescendantCleanupFailed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderProbeStatus {
    Completed,
    NonZeroExit,
    TimedOut,
    OutputTooLarge,
    Failed(ProviderProbeFailureCode),
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ProviderProbeOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    exit_code: Option<i32>,
    overflowed: bool,
}

impl fmt::Debug for ProviderProbeOutput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderProbeOutput")
            .field("stdout_bytes", &self.stdout.len())
            .field("stderr_bytes", &self.stderr.len())
            .field("exit_code", &self.exit_code)
            .finish()
    }
}

impl ProviderProbeOutput {
    pub(crate) fn new(
        stdout: Vec<u8>,
        stderr: Vec<u8>,
        exit_code: Option<i32>,
    ) -> Result<Self, ProviderProbeError> {
        if stdout.len() > MAX_PROVIDER_PROBE_OUTPUT_BYTES
            || stderr.len() > MAX_PROVIDER_PROBE_OUTPUT_BYTES
            || stdout.len().saturating_add(stderr.len()) > MAX_PROVIDER_PROBE_OUTPUT_BYTES
        {
            return Err(ProviderProbeError::OutputTooLarge);
        }
        Ok(Self {
            stdout,
            stderr,
            exit_code,
            overflowed: false,
        })
    }

    fn bounded(
        max_output_bytes: usize,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
        exit_code: Option<i32>,
        overflowed: bool,
    ) -> Result<Self, ProviderProbeError> {
        if stdout.len() > max_output_bytes
            || stderr.len() > max_output_bytes
            || stdout.len().saturating_add(stderr.len()) > max_output_bytes
        {
            return Err(ProviderProbeError::OutputTooLarge);
        }
        let output = Self::new(stdout, stderr, exit_code)?;
        Ok(Self {
            overflowed,
            ..output
        })
    }

    pub(crate) fn stdout(&self) -> &[u8] {
        &self.stdout
    }

    pub(crate) fn stderr(&self) -> &[u8] {
        &self.stderr
    }

    pub(crate) const fn exit_code(&self) -> Option<i32> {
        self.exit_code
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct ProviderProbeResult {
    status: ProviderProbeStatus,
    stdout_bytes: usize,
    stderr_bytes: usize,
    output: Option<ProviderProbeOutput>,
    request_binding: ProviderProbeRequestBinding,
    auth_proof: Option<ProviderProbeProof>,
}

#[derive(Clone, PartialEq, Eq)]
struct ProviderProbeRequestBinding {
    executable: ProviderExecutableHandle,
    kind: ProviderProbeKind,
    timeout: Duration,
    max_output_bytes: usize,
    auth_binding: Option<ProviderAuthProbeBinding>,
    scope_fingerprint: Option<String>,
    env_commitment: String,
}

#[derive(Clone, PartialEq, Eq)]
struct ProviderProbeProof {
    request: ProviderProbeRequestBinding,
}

impl fmt::Debug for ProviderProbeResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderProbeResult")
            .field("status", &self.status)
            .field("stdout_bytes", &self.stdout_bytes)
            .field("stderr_bytes", &self.stderr_bytes)
            .finish()
    }
}

impl ProviderProbeResult {
    pub fn completed(
        request: &ProviderProbeRequest,
        exit_code: i32,
        stdout_bytes: usize,
        stderr_bytes: usize,
    ) -> Result<Self, ProviderProbeError> {
        // Public metadata may describe a version/help probe.  It cannot claim
        // an auth-status observation: that result is issued only by the
        // crate-owned runner with a private proof token.
        if request.auth_binding.is_some()
            || matches!(
                request.kind(),
                ProviderProbeKind::AuthStatus
                    | ProviderProbeKind::LoginStatus
                    | ProviderProbeKind::CursorAboutJson
                    | ProviderProbeKind::CursorAboutPlain
            )
        {
            return Err(ProviderProbeError::InvalidRequest(
                ProviderProbeRequestError::AuthStatusRequiresRunnerProof,
            ));
        }
        if stdout_bytes.saturating_add(stderr_bytes) > request.max_output_bytes() {
            return Err(ProviderProbeError::OutputTooLarge);
        }
        let status = if exit_code == 0 {
            ProviderProbeStatus::Completed
        } else {
            ProviderProbeStatus::NonZeroExit
        };
        Ok(Self {
            status,
            stdout_bytes,
            stderr_bytes,
            output: None,
            request_binding: request.binding(),
            auth_proof: None,
        })
    }

    fn with_output(
        request: &ProviderProbeRequest,
        output: ProviderProbeOutput,
    ) -> Result<Self, ProviderProbeError> {
        if output.stdout.len() > request.max_output_bytes()
            || output.stderr.len() > request.max_output_bytes()
            || output.stdout.len().saturating_add(output.stderr.len()) > request.max_output_bytes()
        {
            return Err(ProviderProbeError::OutputTooLarge);
        }
        let status = if output.overflowed {
            ProviderProbeStatus::OutputTooLarge
        } else if output.exit_code() == Some(0) {
            ProviderProbeStatus::Completed
        } else {
            ProviderProbeStatus::NonZeroExit
        };
        Ok(Self {
            status,
            stdout_bytes: output.stdout.len(),
            stderr_bytes: output.stderr.len(),
            output: Some(output),
            request_binding: request.binding(),
            auth_proof: None,
        })
    }

    /// Marks output as observed by the crate-owned bounded runner.  The
    /// request binding is private correlation material; public callers can
    /// construct result metadata but cannot mint this proof.
    fn with_trusted_output(
        request: &ProviderProbeRequest,
        output: ProviderProbeOutput,
    ) -> Result<Self, ProviderProbeError> {
        let mut result = Self::with_output(request, output)?;
        if request.auth_binding.is_some() {
            result.auth_proof = Some(ProviderProbeProof {
                request: request.binding(),
            });
        }
        Ok(result)
    }

    /// Captures bounded stdout/stderr for an injected `ProviderProbeRunner`.
    /// Status still follows exit code and the request byte bound.
    #[cfg(test)]
    pub fn from_bounded_output(
        request: &ProviderProbeRequest,
        exit_code: Option<i32>,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
    ) -> Result<Self, ProviderProbeError> {
        let output = ProviderProbeOutput::bounded(
            request.max_output_bytes(),
            stdout,
            stderr,
            exit_code,
            false,
        )?;
        Self::with_output(request, output)
    }

    pub const fn status(&self) -> ProviderProbeStatus {
        self.status
    }

    pub const fn stdout_bytes(&self) -> usize {
        self.stdout_bytes
    }

    pub const fn stderr_bytes(&self) -> usize {
        self.stderr_bytes
    }

    pub fn stdout(&self) -> &[u8] {
        self.output
            .as_ref()
            .map_or(&[], ProviderProbeOutput::stdout)
    }

    pub fn stderr(&self) -> &[u8] {
        self.output
            .as_ref()
            .map_or(&[], ProviderProbeOutput::stderr)
    }

    pub(crate) fn output_for_adapter(&self) -> Option<&ProviderProbeOutput> {
        self.output.as_ref()
    }

    pub(crate) fn into_auth_observation(
        self,
        invocation: &ProviderAuthProbeInvocation,
        request: &ProviderProbeRequest,
    ) -> Result<ProviderAuthProbeObservation, ProviderAuthEvidenceError> {
        if request.kind() != ProviderProbeKind::for_auth_probe(invocation.provider_kind())
            || !request.auth_binding_matches(invocation)
        {
            return Err(ProviderAuthEvidenceError::UntrustedAuthenticationEvidence);
        }
        if self.request_binding != request.binding() {
            return Err(ProviderAuthEvidenceError::RequestBindingMismatch);
        }
        let Some(proof) = self.auth_proof.as_ref() else {
            return Err(ProviderAuthEvidenceError::UntrustedAuthenticationEvidence);
        };
        if proof.request != request.binding()
            || !proof
                .request
                .auth_binding
                .as_ref()
                .is_some_and(|binding| *binding == invocation.binding())
        {
            return Err(ProviderAuthEvidenceError::RequestBindingMismatch);
        }
        if self.status != ProviderProbeStatus::Completed {
            return Err(ProviderAuthEvidenceError::UntrustedAuthenticationEvidence);
        }
        let output = self
            .output_for_adapter()
            .ok_or(ProviderAuthEvidenceError::UntrustedAuthenticationEvidence)?;
        let result =
            classify_auth_output(invocation.provider_kind(), output.stdout(), output.stderr());
        ProviderAuthProbeObservation::from_bounded_probe(
            invocation,
            result,
            result.default_confidence(),
        )
    }
}

/// Classify only provider-specific, structured authentication markers.  A
/// generic "logged in" line is intentionally insufficient: wrapper output,
/// API-key login, negative status, and contradictory text must never become a
/// subscription claim.
fn classify_auth_output(
    kind: ProviderKind,
    stdout: &[u8],
    stderr: &[u8],
) -> ProviderAuthProbeResult {
    let mut raw = Vec::with_capacity(stdout.len().saturating_add(stderr.len()));
    raw.extend_from_slice(stdout);
    raw.extend_from_slice(stderr);
    let text = String::from_utf8_lossy(&raw);
    let lower = text.to_ascii_lowercase();

    let api_key = contains_any(
        &lower,
        &[
            "api key",
            "api_key",
            "apikey",
            "api-key",
            "token authentication",
        ],
    );
    let negative = contains_any(
        &lower,
        &[
            "not logged in",
            "logged out",
            "unauthenticated",
            "authentication required",
            "auth required",
            "no active login",
            "no active session",
            "\"loggedin\":false",
            "\"logged_in\":false",
            "\"authenticated\":false",
        ],
    );
    let positive = match kind {
        ProviderKind::ClaudeCode => contains_any(
            &lower,
            &[
                "logged in with claude.ai",
                "authenticated with claude.ai",
                "claude.ai subscription",
            ],
        ),
        ProviderKind::Codex => contains_any(
            &lower,
            &[
                "logged in using chatgpt",
                "authenticated via chatgpt",
                "chatgpt subscription",
            ],
        ),
        ProviderKind::Cursor => false,
    };
    if api_key {
        return ProviderAuthProbeResult::ApiKeyDetected;
    }
    if kind == ProviderKind::Cursor {
        let facts = crate::providers::settings::parse_cursor_about_strict_json(stdout);
        return match facts.auth {
            crate::providers::settings::CursorAboutAuth::Authenticated => {
                ProviderAuthProbeResult::AuthenticatedSubscription
            }
            crate::providers::settings::CursorAboutAuth::Unauthenticated => {
                ProviderAuthProbeResult::AuthRequired
            }
            crate::providers::settings::CursorAboutAuth::Unknown => {
                ProviderAuthProbeResult::Unknown
            }
        };
    }
    if negative && positive {
        return ProviderAuthProbeResult::Unknown;
    }
    if negative {
        return ProviderAuthProbeResult::AuthRequired;
    }
    if let Some(result) = classify_structured_json(kind, &text) {
        return if positive && result == ProviderAuthProbeResult::AuthRequired {
            ProviderAuthProbeResult::Unknown
        } else {
            result
        };
    }
    if positive {
        ProviderAuthProbeResult::AuthenticatedSubscription
    } else {
        ProviderAuthProbeResult::Unknown
    }
}

fn classify_structured_json(kind: ProviderKind, text: &str) -> Option<ProviderAuthProbeResult> {
    let value: serde_json::Value = serde_json::from_str(text.trim()).ok()?;
    let object = value.as_object()?;
    let serialized = value.to_string().to_ascii_lowercase();
    if contains_any(
        &serialized,
        &[
            "not logged in",
            "logged out",
            "unauthenticated",
            "authentication required",
            "auth required",
            "no active login",
            "no active session",
        ],
    ) {
        return Some(ProviderAuthProbeResult::AuthRequired);
    }
    let authenticated = object
        .get("authenticated")
        .or_else(|| object.get("loggedIn"))
        .or_else(|| object.get("logged_in"))
        .and_then(serde_json::Value::as_bool);
    let method = object
        .get("authMethod")
        .or_else(|| object.get("auth_method"))
        .or_else(|| object.get("loginMethod"))
        .or_else(|| object.get("login_method"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_ascii_lowercase);
    if method
        .as_deref()
        .is_some_and(|method| contains_any(method, &["api", "key", "token"]))
    {
        return Some(ProviderAuthProbeResult::ApiKeyDetected);
    }
    if authenticated == Some(false) {
        return Some(ProviderAuthProbeResult::AuthRequired);
    }
    if authenticated != Some(true) {
        return None;
    }
    let subscription_method = match kind {
        ProviderKind::ClaudeCode => contains_any(
            method.as_deref().unwrap_or_default(),
            &["claude.ai", "claude_ai", "oauth", "subscription"],
        ),
        ProviderKind::Codex => contains_any(
            method.as_deref().unwrap_or_default(),
            &["chatgpt", "oauth", "subscription"],
        ),
        ProviderKind::Cursor => contains_any(
            method.as_deref().unwrap_or_default(),
            &["cursor", "oauth", "subscription"],
        ),
    };
    Some(if subscription_method {
        ProviderAuthProbeResult::AuthenticatedSubscription
    } else {
        ProviderAuthProbeResult::Unknown
    })
}

fn contains_any(value: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| value.contains(needle))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderProbeIoError {
    ExecutableMissing,
    ExecutableNotAllowed,
    PermissionDenied,
    SpawnFailed,
    WaitFailed,
    DescendantCleanupFailed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderProbeError {
    Io(ProviderProbeIoError),
    TimedOut,
    UnsupportedAttestation,
    OutputTooLarge,
    NonZeroExit(Option<i32>),
    InvalidRequest(ProviderProbeRequestError),
}

impl fmt::Display for ProviderProbeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "provider probe I/O failed: {error:?}"),
            Self::TimedOut => write!(f, "provider probe timed out"),
            Self::UnsupportedAttestation => {
                write!(
                    f,
                    "provider probe image attestation is unsupported on this platform"
                )
            }
            Self::OutputTooLarge => write!(f, "provider probe output exceeded its bound"),
            Self::NonZeroExit(code) => write!(f, "provider probe exited unsuccessfully: {code:?}"),
            Self::InvalidRequest(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for ProviderProbeError {}

#[async_trait]
pub trait ProviderProbeRunner: Send + Sync {
    async fn run(
        &self,
        request: ProviderProbeRequest,
    ) -> Result<ProviderProbeResult, ProviderProbeError>;
}

/// Runs only an already-resolved executable whose file name is in `policy`.
///
/// Windows launches are suspended, claimed by the Phase-3 kill-on-close Job,
/// and resumed only after the complete launch graph has been attested. Both
/// output pipes are drained concurrently, while a shared admission counter
/// enforces the request's total byte bound exactly.
#[derive(Debug, Clone)]
pub struct WindowsProviderProbeRunner {
    policy: ProviderExecutablePolicy,
}

impl WindowsProviderProbeRunner {
    pub fn new(policy: ProviderExecutablePolicy) -> Self {
        Self { policy }
    }

    fn run_blocking(
        &self,
        request: ProviderProbeRequest,
    ) -> Result<ProviderProbeResult, ProviderProbeError> {
        let deadline = std::time::Instant::now() + request.timeout();
        #[cfg(target_os = "macos")]
        let (executable, fixed_arguments, launch_files) =
            prepare_unix_launch(&self.policy, request.executable())?;
        #[cfg(all(unix, not(target_os = "macos")))]
        let (executable, fixed_arguments, _launch_files) =
            prepare_unix_launch(&self.policy, request.executable())?;
        #[cfg(windows)]
        let executable = validate_probe_executable(&self.policy, request.executable())?;
        #[cfg(not(target_os = "macos"))]
        let mut command = std::process::Command::new(&executable);
        #[cfg(not(target_os = "macos"))]
        {
            #[cfg(windows)]
            command.args(request.executable().launch_fixed_arguments());
            #[cfg(unix)]
            command.args(fixed_arguments);
            command
                .args(request.arguments())
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            apply_provider_environment_exact(&mut command, request.child_environment());

            #[cfg(windows)]
            command
                .creation_flags(crate::services::platform_service::MANAGED_PROCESS_CREATION_FLAGS);
            #[cfg(unix)]
            command.process_group(0);
        }
        #[cfg(not(target_os = "macos"))]
        let mut process = ProbeProcess::spawn(
            command,
            deadline,
            Some(request.executable().launch_program().canonical_path()),
            request.executable(),
        )?;
        #[cfg(target_os = "macos")]
        let mut process = ProbeProcess::spawn_macos(
            &executable,
            &fixed_arguments,
            request.arguments(),
            launch_files,
            deadline,
            request.executable().launch_program().canonical_path(),
            request.executable(),
            request.child_environment(),
        )?;
        // Windows keeps the no-delete handles open through CreateProcess;
        // Unix uses inherited descriptor paths from `prepare_unix_launch`.
        // Revalidate immediately after spawn as a final identity diagnostic.
        if request.executable().revalidate_bound_identity().is_err() {
            process.terminate_tree(deadline)?;
            return Err(ProviderProbeError::Io(
                ProviderProbeIoError::ExecutableNotAllowed,
            ));
        }
        if std::time::Instant::now() >= deadline {
            let _ = process.terminate_tree(deadline);
            return Err(ProviderProbeError::TimedOut);
        }
        if let Err(error) = process.release_attestation_barrier_once() {
            let _ = process.terminate_tree(deadline);
            return Err(error);
        }
        #[cfg(any(windows, target_os = "linux"))]
        {
            let post_revalidate = request.executable().revalidate_bound_identity();
            let post_attestation = if post_revalidate.is_ok() {
                let retry_deadline = std::cmp::min(
                    deadline,
                    std::time::Instant::now() + Duration::from_millis(50),
                );
                let mut attestation = Err(ProviderProbeError::Io(
                    ProviderProbeIoError::ExecutableNotAllowed,
                ));
                loop {
                    match process.try_wait() {
                        Ok(ProbeWait::Exited(_)) => {
                            // A retained Child/process handle is still bound
                            // to the attested instance after a fast provider
                            // exit; querying its image through the now-ended
                            // process can fail.
                            attestation = Ok(());
                            break;
                        }
                        Ok(ProbeWait::Running) => {
                            attestation = attest_launched_image(
                                &process.child,
                                request.executable().launch_program().canonical_path(),
                            );
                            if attestation.is_ok() || std::time::Instant::now() >= retry_deadline {
                                break;
                            }
                            std::thread::sleep(Duration::from_millis(1));
                        }
                        Err(_) => break,
                    }
                }
                attestation
            } else {
                Err(ProviderProbeError::Io(
                    ProviderProbeIoError::ExecutableNotAllowed,
                ))
            };
            if post_revalidate.is_err() || post_attestation.is_err() {
                let _ = process.terminate_tree(deadline);
                return Err(ProviderProbeError::Io(
                    ProviderProbeIoError::ExecutableNotAllowed,
                ));
            }
        }
        #[cfg(target_os = "macos")]
        if request.executable().revalidate_bound_identity().is_err()
            || attest_launched_image(
                process.pid(),
                request.executable().launch_program().canonical_path(),
            )
            .is_err()
        {
            let _ = process.terminate_tree(deadline);
            return Err(ProviderProbeError::Io(
                ProviderProbeIoError::ExecutableNotAllowed,
            ));
        }
        let stdout = process
            .take_stdout()
            .ok_or(ProviderProbeError::Io(ProviderProbeIoError::SpawnFailed))?;
        let stderr = process
            .take_stderr()
            .ok_or(ProviderProbeError::Io(ProviderProbeIoError::SpawnFailed))?;
        let capture = Arc::new(BoundedProbeCapture::new(request.max_output_bytes()));
        let stdout_reader = spawn_probe_reader(stdout, Arc::clone(&capture), true, deadline);
        let stderr_reader = spawn_probe_reader(stderr, Arc::clone(&capture), false, deadline);

        let mut timed_out = false;
        let mut primary_error = None;
        let exit_code = loop {
            match process.try_wait() {
                Err(_) => {
                    primary_error = Some(ProviderProbeError::Io(ProviderProbeIoError::WaitFailed));
                    let _ = process.terminate_tree(deadline);
                    break None;
                }
                Ok(ProbeWait::Exited(code)) => break code,
                Ok(ProbeWait::Running)
                    if std::time::Instant::now()
                        .checked_add(PROVIDER_PROBE_CLEANUP_RESERVE)
                        .is_some_and(|cleanup_start| cleanup_start < deadline) =>
                {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Ok(ProbeWait::Running) => {
                    timed_out = true;
                    if let Err(error) = process.terminate_tree(deadline) {
                        primary_error = Some(error);
                    }
                    break None;
                }
            }
        };

        if !timed_out {
            if let Err(error) = process.terminate_tree(deadline) {
                primary_error = Some(error);
            }
        }
        let mut stdout_reader = stdout_reader;
        let mut stderr_reader = stderr_reader;
        let readers_join = receive_probe_readers(&mut stdout_reader, &mut stderr_reader, deadline);
        if let Some(error) = primary_error {
            return Err(error);
        }
        if let Err(error) = readers_join {
            return Err(error);
        }
        let (stdout, stderr, overflowed) = capture.finish();
        // Keep the entire attested graph alive through cleanup and perform a
        // final identity/hash check before releasing it or issuing proof.  A
        // wrapper target, interpreter, or script that changed during the
        // observation cannot produce a trusted result even if the launched
        // image path itself still matches.
        if request.executable().revalidate_bound_identity().is_err() {
            return Err(ProviderProbeError::Io(
                ProviderProbeIoError::ExecutableNotAllowed,
            ));
        }
        let output = ProviderProbeOutput::bounded(
            request.max_output_bytes(),
            stdout,
            stderr,
            exit_code,
            overflowed,
        )?;
        if timed_out {
            return Err(ProviderProbeError::TimedOut);
        }
        ProviderProbeResult::with_trusted_output(&request, output)
    }
}

#[async_trait]
impl ProviderProbeRunner for WindowsProviderProbeRunner {
    async fn run(
        &self,
        request: ProviderProbeRequest,
    ) -> Result<ProviderProbeResult, ProviderProbeError> {
        let runner = self.clone();
        tokio::task::spawn_blocking(move || runner.run_blocking(request))
            .await
            .map_err(|_| ProviderProbeError::Io(ProviderProbeIoError::WaitFailed))?
    }
}

fn validate_probe_executable(
    policy: &ProviderExecutablePolicy,
    requested: &ProviderExecutableHandle,
) -> Result<PathBuf, ProviderProbeError> {
    requested
        .revalidate_bound_identity()
        .map_err(|_| ProviderProbeError::Io(ProviderProbeIoError::ExecutableNotAllowed))?;
    let canonical = requested.canonical_path().to_path_buf();
    policy
        .validate_canonical_path(&canonical)
        .map_err(|_| ProviderProbeError::Io(ProviderProbeIoError::ExecutableNotAllowed))?;
    Ok(requested.launch_program().canonical_path().to_path_buf())
}

#[cfg(windows)]
fn attest_launched_image(
    child: &Child,
    expected: &std::path::Path,
) -> Result<(), ProviderProbeError> {
    use std::os::windows::ffi::OsStringExt;
    use std::os::windows::io::AsRawHandle;
    use windows::core::PWSTR;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Threading::{QueryFullProcessImageNameW, PROCESS_NAME_WIN32};

    // Query the process handle owned by Child rather than reopening by PID.
    // This remains an exact-image attestation even when a short-lived probe
    // exits before the PID-based OpenProcess race can complete.
    let process = HANDLE(child.as_raw_handle());
    let mut buffer = vec![0_u16; 32_768];
    let mut length = buffer.len() as u32;
    let result = unsafe {
        QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &mut length,
        )
    };
    if !result.is_ok() {
        return Err(ProviderProbeError::Io(
            ProviderProbeIoError::ExecutableNotAllowed,
        ));
    }
    if std::fs::canonicalize(std::ffi::OsString::from_wide(&buffer[..length as usize]))
        .ok()
        .is_some_and(|path| path == expected)
    {
        Ok(())
    } else {
        Err(ProviderProbeError::Io(
            ProviderProbeIoError::ExecutableNotAllowed,
        ))
    }
}

#[cfg(target_os = "linux")]
fn attest_launched_image(
    child: &Child,
    expected: &std::path::Path,
) -> Result<(), ProviderProbeError> {
    if std::fs::read_link(format!("/proc/{}/exe", child.id()))
        .ok()
        .and_then(|path| std::fs::canonicalize(path).ok())
        .is_some_and(|path| path == expected)
    {
        Ok(())
    } else {
        Err(ProviderProbeError::Io(
            ProviderProbeIoError::ExecutableNotAllowed,
        ))
    }
}

#[cfg(target_os = "macos")]
#[link(name = "proc")]
unsafe extern "C" {
    fn proc_pidpath(pid: i32, buffer: *mut u8, buffersize: u32) -> i32;
}

#[cfg(target_os = "macos")]
fn attest_launched_image(pid: u32, expected: &std::path::Path) -> Result<(), ProviderProbeError> {
    let mut buffer = vec![0_u8; 4096];
    let length = unsafe { proc_pidpath(pid as i32, buffer.as_mut_ptr(), buffer.len() as u32) };
    if length <= 0 {
        return Err(ProviderProbeError::Io(
            ProviderProbeIoError::ExecutableNotAllowed,
        ));
    }
    let path = std::str::from_utf8(&buffer[..length as usize])
        .ok()
        .map(PathBuf::from)
        .and_then(|path| std::fs::canonicalize(path).ok());
    if path.is_some_and(|path| path == expected) {
        Ok(())
    } else {
        Err(ProviderProbeError::Io(
            ProviderProbeIoError::ExecutableNotAllowed,
        ))
    }
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn attest_launched_image(
    _child: &Child,
    _expected: &std::path::Path,
) -> Result<(), ProviderProbeError> {
    Err(ProviderProbeError::UnsupportedAttestation)
}

#[cfg(not(any(windows, unix)))]
fn attest_launched_image(
    _child: &Child,
    _expected: &std::path::Path,
) -> Result<(), ProviderProbeError> {
    Err(ProviderProbeError::UnsupportedAttestation)
}

#[cfg(unix)]
fn prepare_unix_launch(
    policy: &ProviderExecutablePolicy,
    requested: &ProviderExecutableHandle,
) -> Result<(PathBuf, Vec<std::ffi::OsString>, Vec<std::fs::File>), ProviderProbeError> {
    #[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
    {
        let _ = (policy, requested);
        return Err(ProviderProbeError::UnsupportedAttestation);
    }
    validate_probe_executable(policy, requested)?;
    let (program_file, script_file) = requested
        .launch_files()
        .map_err(|_| ProviderProbeError::Io(ProviderProbeIoError::ExecutableNotAllowed))?;
    let program_path = inherit_descriptor(&program_file)?;
    let mut launch_files = vec![program_file];
    let fixed_arguments = if let Some(script_file) = script_file {
        let script_path = inherit_descriptor(&script_file)?;
        launch_files.push(script_file);
        vec![script_path.into_os_string()]
    } else {
        Vec::new()
    };
    Ok((program_path, fixed_arguments, launch_files))
}

#[cfg(unix)]
fn inherit_descriptor(file: &std::fs::File) -> Result<PathBuf, ProviderProbeError> {
    use std::os::unix::io::AsRawFd;

    const F_GETFD: i32 = 1;
    const F_SETFD: i32 = 2;
    const FD_CLOEXEC: i32 = 1;
    let fd = file.as_raw_fd();
    // `std::fs::File` descriptors are close-on-exec by default. Clear that
    // bit only for the two already-attested launch files so the child can
    // resolve its own `/proc/self/fd` (or `/dev/fd`) path at exec time.
    let flags = unsafe { unix_fcntl(fd, F_GETFD, 0) };
    if flags < 0 {
        return Err(ProviderProbeError::Io(
            ProviderProbeIoError::ExecutableNotAllowed,
        ));
    }
    if flags & FD_CLOEXEC != 0 && unsafe { unix_fcntl(fd, F_SETFD, flags & !FD_CLOEXEC) } < 0 {
        return Err(ProviderProbeError::Io(
            ProviderProbeIoError::ExecutableNotAllowed,
        ));
    }
    let root = if cfg!(target_os = "linux") {
        "/proc/self/fd"
    } else {
        "/dev/fd"
    };
    Ok(PathBuf::from(root).join(fd.to_string()))
}

#[cfg(unix)]
unsafe extern "C" {
    fn unix_fcntl(fd: i32, command: i32, argument: i32) -> i32;
}

const PROVIDER_ENVIRONMENT_ALLOWLIST: &[&str] = &[
    "PATH",
    "PATHEXT",
    "SystemRoot",
    "WINDIR",
    "ComSpec",
    "TEMP",
    "TMP",
    "USERPROFILE",
    "HOME",
    "APPDATA",
    "LOCALAPPDATA",
    "HOMEDRIVE",
    "HOMEPATH",
    // Windows libraries may expand these while initializing caches. Omitting
    // them from the sealed provider environment leaves a literal
    // `%SystemDrive%` path, which is then resolved beneath the project cwd.
    "SystemDrive",
    "ProgramData",
    "ALLUSERSPROFILE",
];

/// Fixed provider-runtime transport defaults that are part of the sealed
/// effective environment (probe, commitment, and provider launch share them).
const PROVIDER_RUNTIME_TRANSPORT_DEFAULTS: &[(&str, &str)] = &[
    ("TERM", "xterm-256color"),
    ("COLORTERM", "truecolor"),
    ("TERM_PROGRAM", "DevManager"),
    ("CLICOLOR", "1"),
    ("CLICOLOR_FORCE", "1"),
    ("FORCE_COLOR", "1"),
];

/// Materialize the one effective provider environment: platform allowlist from
/// ambient at this instant, fixed transport defaults, then configured overrides.
/// Probe, commitment, and provider launch must all use this exact map.
pub fn materialize_provider_environment(
    overrides: BTreeMap<std::ffi::OsString, std::ffi::OsString>,
) -> BTreeMap<std::ffi::OsString, std::ffi::OsString> {
    let mut env = BTreeMap::new();
    for key in PROVIDER_ENVIRONMENT_ALLOWLIST {
        if let Some(value) = std::env::var_os(key) {
            env.insert(
                provider_environment_key(std::ffi::OsString::from(*key)),
                value,
            );
        }
    }
    for (key, value) in PROVIDER_RUNTIME_TRANSPORT_DEFAULTS {
        env.entry(std::ffi::OsString::from(*key))
            .or_insert_with(|| std::ffi::OsString::from(*value));
    }
    env.entry(std::ffi::OsString::from("TERM_PROGRAM_VERSION"))
        .or_insert_with(|| std::ffi::OsString::from(env!("CARGO_PKG_VERSION")));
    for (key, value) in overrides {
        env.insert(provider_environment_key(key), value);
    }
    env
}

fn provider_environment_key(key: std::ffi::OsString) -> std::ffi::OsString {
    #[cfg(windows)]
    {
        // Settings keys are ASCII; Windows treats environment names without case.
        // Seal one key per effective variable, not both PATH and Path.
        std::ffi::OsString::from(key.to_string_lossy().to_ascii_uppercase())
    }
    #[cfg(not(windows))]
    {
        key
    }
}

/// Provider-only: clear inherited ambient, then install the sealed map.
pub fn apply_provider_environment_exact(
    command: &mut std::process::Command,
    environment: &BTreeMap<std::ffi::OsString, std::ffi::OsString>,
) {
    command.env_clear();
    for (key, value) in environment {
        command.env(key, value);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProbeWait {
    Running,
    Exited(Option<i32>),
}

struct ProbeProcess {
    #[cfg(not(target_os = "macos"))]
    child: Child,
    #[cfg(target_os = "macos")]
    pid: u32,
    #[cfg(target_os = "macos")]
    macos_stdout: Option<std::fs::File>,
    #[cfg(target_os = "macos")]
    macos_stderr: Option<std::fs::File>,
    #[cfg(target_os = "macos")]
    macos_launch_files: Vec<std::fs::File>,
    #[cfg(target_os = "macos")]
    macos_waited: bool,
    #[cfg(target_os = "macos")]
    macos_exit_code: Option<i32>,
    managed_job: Option<crate::process::job::ManagedProcessJob>,
    deadline: std::time::Instant,
    #[cfg(unix)]
    process_group: bool,
    #[cfg(target_os = "linux")]
    linux_ptrace_stopped: bool,
    #[cfg(target_os = "macos")]
    macos_suspended: bool,
    #[cfg(windows)]
    windows_suspended: bool,
    attestation_barrier_killed: bool,
    #[cfg(target_os = "linux")]
    linux_process_start: u64,
}

impl ProbeProcess {
    #[cfg(not(target_os = "macos"))]
    fn spawn(
        mut command: std::process::Command,
        deadline: std::time::Instant,
        expected: Option<&Path>,
        requested: &ProviderExecutableHandle,
    ) -> Result<Self, ProviderProbeError> {
        #[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
        {
            let _ = (&command, deadline, expected, requested);
            return Err(ProviderProbeError::UnsupportedAttestation);
        }
        #[cfg(target_os = "linux")]
        if expected.is_none() {
            let _ = (&command, deadline, requested);
            return Err(ProviderProbeError::UnsupportedAttestation);
        }
        #[cfg(target_os = "linux")]
        unsafe {
            command.pre_exec(linux_ptrace_traceme);
        }
        let mut child = command.spawn().map_err(|error| {
            ProviderProbeError::Io(if error.kind() == std::io::ErrorKind::NotFound {
                ProviderProbeIoError::ExecutableMissing
            } else if error.kind() == std::io::ErrorKind::PermissionDenied {
                ProviderProbeIoError::PermissionDenied
            } else {
                ProviderProbeIoError::SpawnFailed
            })
        })?;
        #[cfg(target_os = "linux")]
        if let Err(error) = wait_for_linux_exec_stop(&child, deadline) {
            let _ = child.kill();
            reap_child_until(&mut child, deadline);
            return Err(error);
        }
        #[cfg(target_os = "linux")]
        let linux_process_start = linux_process_start_token(child.id()).ok_or_else(|| {
            let _ = child.kill();
            reap_child_until(&mut child, deadline);
            ProviderProbeError::Io(ProviderProbeIoError::SpawnFailed)
        })?;
        let managed_job =
            match crate::services::platform_service::claim_suspended_process(child.id()) {
                Ok(job) => job,
                Err(_) => {
                    let _ = child.kill();
                    reap_child_until(&mut child, deadline);
                    return Err(ProviderProbeError::Io(ProviderProbeIoError::SpawnFailed));
                }
            };
        #[cfg(windows)]
        if expected.is_some_and(|expected| attest_launched_image(&child, expected).is_err())
            || requested.revalidate_bound_identity().is_err()
        {
            let _ = child.kill();
            reap_child_until(&mut child, deadline);
            return Err(ProviderProbeError::Io(
                ProviderProbeIoError::ExecutableNotAllowed,
            ));
        }
        #[cfg(target_os = "linux")]
        if attest_launched_image(
            &child,
            expected.expect("Linux expected image checked above"),
        )
        .is_err()
            || requested.revalidate_bound_identity().is_err()
        {
            let _ = child.kill();
            reap_child_until(&mut child, deadline);
            return Err(ProviderProbeError::Io(
                ProviderProbeIoError::ExecutableNotAllowed,
            ));
        }
        #[cfg(windows)]
        if managed_job.is_none() {
            let _ = child.kill();
            reap_child_until(&mut child, deadline);
            return Err(ProviderProbeError::Io(ProviderProbeIoError::SpawnFailed));
        }
        Ok(Self {
            child,
            managed_job,
            deadline,
            #[cfg(unix)]
            process_group: cfg!(unix),
            #[cfg(target_os = "linux")]
            linux_ptrace_stopped: true,
            #[cfg(target_os = "macos")]
            macos_suspended: true,
            #[cfg(windows)]
            windows_suspended: true,
            attestation_barrier_killed: false,
            #[cfg(target_os = "linux")]
            linux_process_start,
        })
    }

    #[cfg(target_os = "macos")]
    fn spawn_macos(
        executable: &Path,
        fixed_arguments: &[std::ffi::OsString],
        request_arguments: &[&str],
        launch_files: Vec<std::fs::File>,
        deadline: std::time::Instant,
        expected: &Path,
        requested: &ProviderExecutableHandle,
        child_environment: &BTreeMap<std::ffi::OsString, std::ffi::OsString>,
    ) -> Result<Self, ProviderProbeError> {
        use std::ffi::{CString, OsStr};
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::io::{FromRawFd, RawFd};

        fn cstring(value: &OsStr) -> Result<CString, ProviderProbeError> {
            CString::new(value.as_bytes())
                .map_err(|_| ProviderProbeError::Io(ProviderProbeIoError::ExecutableNotAllowed))
        }

        requested
            .revalidate_bound_identity()
            .map_err(|_| ProviderProbeError::Io(ProviderProbeIoError::ExecutableNotAllowed))?;
        let executable_c = cstring(executable.as_os_str())?;
        let mut arguments = Vec::with_capacity(1 + fixed_arguments.len() + request_arguments.len());
        arguments.push(executable_c.clone());
        for argument in fixed_arguments {
            arguments.push(cstring(argument.as_os_str())?);
        }
        for argument in request_arguments {
            arguments.push(cstring(OsStr::new(argument))?);
        }
        let mut argv: Vec<*mut libc::c_char> = arguments
            .iter_mut()
            .map(|argument| argument.as_ptr() as *mut libc::c_char)
            .collect();
        argv.push(std::ptr::null_mut());

        let mut environment = Vec::new();
        // child_environment is already the sealed effective map (env_clear semantics).
        for (key, value) in child_environment {
            let mut entry = key.as_os_str().as_bytes().to_vec();
            entry.push(b'=');
            entry.extend_from_slice(value.as_os_str().as_bytes());
            environment.push(
                CString::new(entry)
                    .map_err(|_| ProviderProbeError::Io(ProviderProbeIoError::SpawnFailed))?,
            );
        }
        let mut envp: Vec<*mut libc::c_char> = environment
            .iter_mut()
            .map(|entry| entry.as_ptr() as *mut libc::c_char)
            .collect();
        envp.push(std::ptr::null_mut());

        let mut stdout_pipe = [-1 as RawFd; 2];
        let mut stderr_pipe = [-1 as RawFd; 2];
        if unsafe { libc::pipe(stdout_pipe.as_mut_ptr()) } != 0
            || unsafe { libc::pipe(stderr_pipe.as_mut_ptr()) } != 0
        {
            if stdout_pipe[0] >= 0 {
                unsafe {
                    libc::close(stdout_pipe[0]);
                    if stdout_pipe[1] >= 0 {
                        libc::close(stdout_pipe[1]);
                    }
                }
            }
            if stderr_pipe[0] >= 0 {
                unsafe {
                    libc::close(stderr_pipe[0]);
                    if stderr_pipe[1] >= 0 {
                        libc::close(stderr_pipe[1]);
                    }
                }
            }
            return Err(ProviderProbeError::Io(ProviderProbeIoError::SpawnFailed));
        }

        let mut actions: libc::posix_spawn_file_actions_t = std::ptr::null_mut();
        let mut attributes: libc::posix_spawnattr_t = std::ptr::null_mut();
        let mut initialized_actions = false;
        let mut initialized_attributes = false;
        let setup_result = unsafe {
            let mut result = libc::posix_spawn_file_actions_init(&mut actions);
            if result == 0 {
                initialized_actions = true;
                result = libc::posix_spawn_file_actions_addopen(
                    &mut actions,
                    libc::STDIN_FILENO,
                    b"/dev/null\0".as_ptr() as *const libc::c_char,
                    libc::O_RDONLY,
                    0,
                );
            }
            if result == 0 {
                result = libc::posix_spawn_file_actions_adddup2(
                    &mut actions,
                    stdout_pipe[1],
                    libc::STDOUT_FILENO,
                );
            }
            if result == 0 {
                result = libc::posix_spawn_file_actions_adddup2(
                    &mut actions,
                    stderr_pipe[1],
                    libc::STDERR_FILENO,
                );
            }
            for fd in [
                stdout_pipe[0],
                stdout_pipe[1],
                stderr_pipe[0],
                stderr_pipe[1],
            ] {
                if result != 0 {
                    break;
                }
                if (libc::STDIN_FILENO..=libc::STDERR_FILENO).contains(&fd) {
                    continue;
                }
                result = libc::posix_spawn_file_actions_addclose(&mut actions, fd);
            }
            if result == 0 {
                result = libc::posix_spawnattr_init(&mut attributes);
                initialized_attributes = result == 0;
            }
            if result == 0 {
                result = libc::posix_spawnattr_setflags(
                    &mut attributes,
                    (libc::POSIX_SPAWN_START_SUSPENDED | libc::POSIX_SPAWN_SETPGROUP)
                        as libc::c_short,
                );
            }
            if result == 0 {
                result = libc::posix_spawnattr_setpgroup(&mut attributes, 0);
            }
            result
        };
        if setup_result != 0 {
            unsafe {
                if initialized_attributes {
                    libc::posix_spawnattr_destroy(&mut attributes);
                }
                if initialized_actions {
                    libc::posix_spawn_file_actions_destroy(&mut actions);
                }
                libc::close(stdout_pipe[0]);
                libc::close(stdout_pipe[1]);
                libc::close(stderr_pipe[0]);
                libc::close(stderr_pipe[1]);
            }
            return Err(ProviderProbeError::Io(ProviderProbeIoError::SpawnFailed));
        }

        let mut pid = 0 as libc::pid_t;
        let spawn_result = unsafe {
            libc::posix_spawn(
                &mut pid,
                executable_c.as_ptr(),
                &actions,
                &attributes,
                argv.as_ptr(),
                envp.as_ptr(),
            )
        };
        unsafe {
            libc::posix_spawnattr_destroy(&mut attributes);
            libc::posix_spawn_file_actions_destroy(&mut actions);
        }
        if spawn_result != 0 {
            unsafe {
                libc::close(stdout_pipe[0]);
                libc::close(stdout_pipe[1]);
                libc::close(stderr_pipe[0]);
                libc::close(stderr_pipe[1]);
            }
            return Err(ProviderProbeError::Io(ProviderProbeIoError::SpawnFailed));
        }
        unsafe {
            libc::close(stdout_pipe[1]);
            libc::close(stderr_pipe[1]);
        }
        let stdout = unsafe { std::fs::File::from_raw_fd(stdout_pipe[0]) };
        let stderr = unsafe { std::fs::File::from_raw_fd(stderr_pipe[0]) };
        let pid = pid as u32;
        if attest_launched_image(pid, expected).is_err()
            || requested.revalidate_bound_identity().is_err()
        {
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGKILL);
            }
            let _ = macos_reap_pid(pid, deadline);
            return Err(ProviderProbeError::Io(
                ProviderProbeIoError::ExecutableNotAllowed,
            ));
        }
        let managed_job = match crate::services::platform_service::claim_suspended_process(pid) {
            Ok(job) => job,
            Err(_) => {
                unsafe {
                    libc::kill(pid as libc::pid_t, libc::SIGKILL);
                }
                let _ = macos_reap_pid(pid, deadline);
                return Err(ProviderProbeError::Io(ProviderProbeIoError::SpawnFailed));
            }
        };
        Ok(Self {
            pid,
            macos_stdout: Some(stdout),
            macos_stderr: Some(stderr),
            macos_launch_files: launch_files,
            macos_waited: false,
            macos_exit_code: None,
            managed_job,
            deadline,
            process_group: true,
            macos_suspended: true,
            #[cfg(target_os = "linux")]
            linux_ptrace_stopped: false,
            attestation_barrier_killed: false,
        })
    }

    fn pid(&self) -> u32 {
        #[cfg(target_os = "macos")]
        {
            self.pid
        }
        #[cfg(not(target_os = "macos"))]
        {
            self.child.id()
        }
    }

    fn take_stdout(&mut self) -> Option<Box<dyn ProbePipe>> {
        #[cfg(target_os = "macos")]
        {
            return self
                .macos_stdout
                .take()
                .and_then(|pipe| boxed_probe_pipe(pipe).ok());
        }
        #[cfg(not(target_os = "macos"))]
        {
            self.child
                .stdout
                .take()
                .and_then(|pipe| boxed_probe_pipe(pipe).ok())
        }
    }

    #[cfg(not(target_os = "macos"))]
    fn take_stdin(&mut self) -> Option<ChildStdin> {
        self.child.stdin.take()
    }

    fn take_stderr(&mut self) -> Option<Box<dyn ProbePipe>> {
        #[cfg(target_os = "macos")]
        {
            return self
                .macos_stderr
                .take()
                .and_then(|pipe| boxed_probe_pipe(pipe).ok());
        }
        #[cfg(not(target_os = "macos"))]
        {
            self.child
                .stderr
                .take()
                .and_then(|pipe| boxed_probe_pipe(pipe).ok())
        }
    }

    fn try_wait(&mut self) -> std::io::Result<ProbeWait> {
        #[cfg(target_os = "macos")]
        {
            return self.macos_try_wait();
        }
        #[cfg(not(target_os = "macos"))]
        {
            self.child.try_wait().map(|status| {
                status
                    .map(|status| ProbeWait::Exited(status.code()))
                    .unwrap_or(ProbeWait::Running)
            })
        }
    }

    #[cfg(target_os = "macos")]
    fn macos_try_wait(&mut self) -> std::io::Result<ProbeWait> {
        if self.macos_waited {
            return Ok(ProbeWait::Exited(self.macos_exit_code));
        }
        let mut status = 0_i32;
        let waited = unsafe { libc::waitpid(self.pid as libc::pid_t, &mut status, libc::WNOHANG) };
        if waited == 0 {
            return Ok(ProbeWait::Running);
        }
        if waited < 0 {
            return Err(std::io::Error::last_os_error());
        }
        self.macos_waited = true;
        self.macos_exit_code = if (status & 0x7f) == 0 {
            Some((status >> 8) & 0xff)
        } else {
            None
        };
        Ok(ProbeWait::Exited(self.macos_exit_code))
    }

    fn release_attestation_barrier_once(&mut self) -> Result<(), ProviderProbeError> {
        #[cfg(windows)]
        if self.windows_suspended {
            // Consume the one explicit release attempt before calling the OS.
            // ResumeThread can partially succeed before reporting an error;
            // retrying from cleanup would violate the one-resume boundary and
            // could run an incompletely attested thread.
            self.windows_suspended = false;
            crate::services::platform_service::resume_suspended_process(self.pid())
                .map_err(|_| ProviderProbeError::Io(ProviderProbeIoError::SpawnFailed))?;
        }
        #[cfg(target_os = "linux")]
        if self.linux_ptrace_stopped {
            // PTRACE_DETACH is the single release boundary.  If the syscall
            // fails, termination below kills/reaps the still-stopped child;
            // a second detach attempt is not safe or necessary.
            self.linux_ptrace_stopped = false;
            if unsafe { linux_ptrace_detach(self.pid()) } != 0 {
                return Err(ProviderProbeError::Io(ProviderProbeIoError::SpawnFailed));
            }
        }
        #[cfg(target_os = "macos")]
        if self.macos_suspended {
            self.macos_suspended = false;
            resume_macos_suspended_process(self.pid())
                .map_err(|_| ProviderProbeError::Io(ProviderProbeIoError::SpawnFailed))?;
        }
        Ok(())
    }

    fn kill_while_attestation_barrier(&mut self) -> bool {
        if self.attestation_barrier_killed {
            return true;
        }
        let barrier_active = {
            #[cfg(windows)]
            {
                self.windows_suspended
            }
            #[cfg(target_os = "linux")]
            {
                self.linux_ptrace_stopped
            }
            #[cfg(target_os = "macos")]
            {
                self.macos_suspended
            }
            #[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
            {
                false
            }
            #[cfg(not(any(unix, windows)))]
            {
                false
            }
        };
        if !barrier_active {
            return true;
        }
        self.attestation_barrier_killed = true;
        let mut killed = true;
        if let Some(job) = self.managed_job.as_ref() {
            if job.terminate_members().is_err() {
                killed = false;
            }
        }
        #[cfg(unix)]
        if self.process_group {
            #[cfg(target_os = "linux")]
            let identity_matches =
                process_group_matches_start(self.pid(), self.linux_process_start);
            #[cfg(not(target_os = "linux"))]
            let identity_matches = true;
            if identity_matches {
                if !force_kill_unix_process_group(
                    self.pid(),
                    #[cfg(target_os = "linux")]
                    Some(self.linux_process_start),
                    #[cfg(not(target_os = "linux"))]
                    None,
                ) {
                    killed = false;
                }
            } else {
                killed = false;
            }
        }
        if matches!(self.try_wait(), Ok(ProbeWait::Running)) && self.kill_process().is_err() {
            killed = false;
        }
        killed
    }

    fn terminate_tree(&mut self, deadline: std::time::Instant) -> Result<(), ProviderProbeError> {
        let mut job_empty = true;
        let barrier_kill_ok = self.kill_while_attestation_barrier();
        let mut cleanup_failed = !barrier_kill_ok;
        #[cfg(unix)]
        let group_cleanup_deadline = deadline;
        if let Some(job) = self.managed_job.as_ref() {
            match job.active_process_ids() {
                Ok(active_before) => {
                    if !active_before.is_empty()
                        && (!self.attestation_barrier_killed || !barrier_kill_ok)
                    {
                        if job.terminate_members().is_err() {
                            cleanup_failed = true;
                        }
                    }
                }
                Err(_) => cleanup_failed = true,
            }
            job_empty = match job.wait_for_active_process_zero(deadline) {
                Ok(empty) => empty,
                Err(_) => {
                    cleanup_failed = true;
                    false
                }
            };
        }
        #[cfg(unix)]
        let group_signal_ok =
            if self.process_group && (!self.attestation_barrier_killed || !barrier_kill_ok) {
                #[cfg(target_os = "linux")]
                let identity_matches =
                    process_group_matches_start(self.pid(), self.linux_process_start);
                #[cfg(not(target_os = "linux"))]
                let identity_matches = true;
                if !identity_matches {
                    false
                } else {
                    let remaining =
                        group_cleanup_deadline.saturating_duration_since(std::time::Instant::now());
                    let term_grace = remaining / 3;
                    let result = crate::services::platform_service::terminate_owned_process_group(
                        self.pid(),
                        term_grace,
                    );
                    if result.is_err() {
                        force_kill_unix_process_group(
                            self.pid(),
                            #[cfg(target_os = "linux")]
                            Some(self.linux_process_start),
                            #[cfg(not(target_os = "linux"))]
                            None,
                        )
                    } else {
                        true
                    }
                }
            } else {
                true
            };
        #[cfg(not(unix))]
        let _group_cleanup_ok = true;
        // Job ACTIVE_PROCESS_ZERO state is authoritative for the managed tree.
        // The raw `Child` handle can lag that state on Windows, so do not
        // spend the absolute deadline waiting for a second observation of the
        // same process exit.
        if !(self.managed_job.is_some() && job_empty) {
            if matches!(self.try_wait(), Ok(ProbeWait::Running)) {
                let _ = self.kill_process();
            }
        }
        #[cfg(unix)]
        let group_exited =
            group_signal_ok && wait_for_unix_process_group_exit(self.pid(), group_cleanup_deadline);
        #[cfg(not(unix))]
        let group_exited = true;
        let child_exited = if self.managed_job.is_some() && job_empty {
            // On Windows the Job's ACTIVE_PROCESS_ZERO notification is the
            // authoritative reap boundary.  The raw Child handle can remain
            // non-waitable after the Job has consumed the process.
            true
        } else {
            self.reap_owned_until(deadline)
        };
        if cleanup_failed || !job_empty || !child_exited || !group_exited {
            return Err(ProviderProbeError::Io(
                ProviderProbeIoError::DescendantCleanupFailed,
            ));
        }
        drop(self.managed_job.take());
        Ok(())
    }

    fn kill_process(&mut self) -> std::io::Result<()> {
        #[cfg(target_os = "macos")]
        {
            if unsafe { libc::kill(self.pid() as libc::pid_t, libc::SIGKILL) } == 0 {
                return Ok(());
            }
            return Err(std::io::Error::last_os_error());
        }
        #[cfg(not(target_os = "macos"))]
        {
            self.child.kill()
        }
    }

    fn reap_owned_until(&mut self, deadline: std::time::Instant) -> bool {
        #[cfg(target_os = "macos")]
        {
            loop {
                match self.try_wait() {
                    Ok(ProbeWait::Exited(_)) => return true,
                    Ok(ProbeWait::Running) if std::time::Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Ok(ProbeWait::Running) | Err(_) => return false,
                }
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            reap_child_until(&mut self.child, deadline)
        }
    }
}

#[cfg(target_os = "linux")]
const LINUX_PTRACE_TRACEME: i64 = 0;
#[cfg(target_os = "linux")]
const LINUX_PTRACE_DETACH: i64 = 17;
#[cfg(target_os = "linux")]
const LINUX_PTRACE_CONT: i64 = 7;
#[cfg(target_os = "linux")]
const LINUX_PTRACE_SETOPTIONS: i64 = 0x4200;
#[cfg(target_os = "linux")]
const LINUX_PTRACE_GETEVENTMSG: i64 = 0x4201;
#[cfg(target_os = "linux")]
const LINUX_PTRACE_O_TRACEEXEC: i64 = 0x10;
#[cfg(target_os = "linux")]
const LINUX_PTRACE_O_EXITKILL: i64 = 0x0010_0000;
#[cfg(target_os = "linux")]
const LINUX_DESCENDANT_CONTAINMENT_HOLD: &str =
    "platform HOLD: fork/clone/setsid descendants are not yet ptrace-supervised";
#[cfg(target_os = "linux")]
const LINUX_PTRACE_EVENT_EXEC: i32 = 4;
#[cfg(target_os = "linux")]
const LINUX_WAIT_NOHANG: i32 = 1;
#[cfg(target_os = "linux")]
const LINUX_WAIT_WALL: i32 = 0x4000_0000;
#[cfg(target_os = "linux")]
const LINUX_SIGTRAP: i32 = 5;
#[cfg(target_os = "linux")]
const LINUX_SIGSTOP: i32 = 19;

#[cfg(target_os = "linux")]
unsafe extern "C" {
    fn linux_ptrace(
        request: i64,
        pid: i32,
        address: *mut std::ffi::c_void,
        data: *mut std::ffi::c_void,
    ) -> i64;
    fn linux_waitpid(pid: i32, status: *mut i32, options: i32) -> i32;
}

#[cfg(target_os = "linux")]
fn linux_ptrace_traceme() -> std::io::Result<()> {
    if unsafe {
        linux_ptrace(
            LINUX_PTRACE_TRACEME,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    } == -1
    {
        Err(std::io::Error::last_os_error())
    } else if unsafe { libc::raise(LINUX_SIGSTOP) } != 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn wait_for_linux_exec_stop(
    child: &Child,
    deadline: std::time::Instant,
) -> Result<(), ProviderProbeError> {
    wait_for_linux_stop(child, deadline, |status| {
        linux_status_is_stopped(status) && linux_status_signal(status) == LINUX_SIGSTOP
    })?;
    linux_ptrace_set_options(child.id())?;
    linux_ptrace_continue(child.id())?;

    wait_for_linux_stop(child, deadline, linux_status_is_exact_exec_event)?;
    if linux_ptrace_event_message(child.id())? == 0 {
        return Err(ProviderProbeError::Io(ProviderProbeIoError::SpawnFailed));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn linux_ptrace_set_options(pid: u32) -> Result<(), ProviderProbeError> {
    // This implementation intentionally records an explicit platform HOLD:
    // TRACEEXEC plus EXITKILL does not control fork/clone children that leave
    // the process group (for example after setsid). No production provider
    // launch may rely on Linux containment until those descendants are
    // traced and reaped under the same absolute deadline.
    let _platform_hold = LINUX_DESCENDANT_CONTAINMENT_HOLD;
    if unsafe {
        linux_ptrace(
            LINUX_PTRACE_SETOPTIONS,
            pid as i32,
            std::ptr::null_mut(),
            (LINUX_PTRACE_O_TRACEEXEC | LINUX_PTRACE_O_EXITKILL) as usize as *mut std::ffi::c_void,
        )
    } == -1
    {
        Err(ProviderProbeError::Io(ProviderProbeIoError::SpawnFailed))
    } else {
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn linux_ptrace_continue(pid: u32) -> Result<(), ProviderProbeError> {
    if unsafe {
        linux_ptrace(
            LINUX_PTRACE_CONT,
            pid as i32,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    } == -1
    {
        Err(ProviderProbeError::Io(ProviderProbeIoError::SpawnFailed))
    } else {
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn linux_ptrace_event_message(pid: u32) -> Result<u64, ProviderProbeError> {
    let mut event_message = 0_u64;
    if unsafe {
        linux_ptrace(
            LINUX_PTRACE_GETEVENTMSG,
            pid as i32,
            std::ptr::null_mut(),
            std::ptr::addr_of_mut!(event_message).cast(),
        )
    } == -1
    {
        Err(ProviderProbeError::Io(ProviderProbeIoError::SpawnFailed))
    } else {
        Ok(event_message)
    }
}

#[cfg(target_os = "linux")]
fn wait_for_linux_stop(
    child: &Child,
    deadline: std::time::Instant,
    expected: impl Fn(i32) -> bool,
) -> Result<i32, ProviderProbeError> {
    let mut status = 0_i32;
    loop {
        let waited = unsafe {
            linux_waitpid(
                child.id() as i32,
                &mut status,
                LINUX_WAIT_NOHANG | LINUX_WAIT_WALL,
            )
        };
        if waited == child.id() as i32 {
            if expected(status) {
                return Ok(status);
            }
            return Err(ProviderProbeError::Io(ProviderProbeIoError::SpawnFailed));
        }
        if waited < 0 {
            return Err(ProviderProbeError::Io(ProviderProbeIoError::WaitFailed));
        }
        if std::time::Instant::now() >= deadline {
            return Err(ProviderProbeError::TimedOut);
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[cfg(target_os = "linux")]
const fn linux_status_is_stopped(status: i32) -> bool {
    (status & 0x7f) == 0x7f
}

#[cfg(target_os = "linux")]
const fn linux_status_signal(status: i32) -> i32 {
    (status >> 8) & 0xff
}

#[cfg(target_os = "linux")]
const fn linux_status_event(status: i32) -> i32 {
    (status >> 16) & 0xffff
}

#[cfg(target_os = "linux")]
const fn linux_status_is_exact_exec_event(status: i32) -> bool {
    linux_status_is_stopped(status)
        && linux_status_signal(status) == LINUX_SIGTRAP
        && linux_status_event(status) == LINUX_PTRACE_EVENT_EXEC
}

#[cfg(target_os = "linux")]
unsafe fn linux_ptrace_detach(pid: u32) -> i64 {
    linux_ptrace(
        LINUX_PTRACE_DETACH,
        pid as i32,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
    )
}

#[cfg(target_os = "linux")]
fn linux_process_start_token(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, fields) = stat.rsplit_once(") ")?;
    fields.split_whitespace().nth(19)?.parse().ok()
}

#[cfg(target_os = "linux")]
fn process_group_matches_start(pid: u32, expected_start: u64) -> bool {
    linux_process_start_token(pid).is_some_and(|actual| actual == expected_start)
}

#[cfg(target_os = "macos")]
const MACOS_SIGCONT: i32 = 19;

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn macos_kill(pid: i32, signal: i32) -> i32;
}

#[cfg(target_os = "macos")]
fn resume_macos_suspended_process(pid: u32) -> std::io::Result<()> {
    if unsafe { macos_kill(pid as i32, MACOS_SIGCONT) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(target_os = "macos")]
fn macos_reap_pid(pid: u32, deadline: std::time::Instant) -> bool {
    let mut status = 0_i32;
    loop {
        let waited = unsafe { libc::waitpid(pid as libc::pid_t, &mut status, libc::WNOHANG) };
        if waited == pid as libc::pid_t {
            return true;
        }
        if waited < 0 || std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

impl Drop for ProbeProcess {
    fn drop(&mut self) {
        let _ = self.terminate_tree(self.deadline);
    }
}

fn reap_child_until(child: &mut Child, deadline: std::time::Instant) -> bool {
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return true,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Ok(None) | Err(_) => return false,
        }
    }
}

#[cfg(unix)]
fn force_kill_unix_process_group(pid: u32, expected_start: Option<u64>) -> bool {
    #[cfg(target_os = "linux")]
    if let Some(expected_start) = expected_start {
        if !process_group_matches_start(pid, expected_start) {
            return false;
        }
    }
    let result = unsafe { libc::kill(-(pid as libc::pid_t), libc::SIGKILL) };
    result == 0
        || (result == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH))
}

#[cfg(unix)]
fn wait_for_unix_process_group_exit(pid: u32, deadline: std::time::Instant) -> bool {
    let group_target = -(pid as libc::pid_t);
    loop {
        let result = unsafe { libc::kill(group_target, 0) };
        let exists = result == 0
            || (result == -1
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM));
        if !exists {
            return true;
        }
        let now = std::time::Instant::now();
        if now >= deadline {
            return false;
        }
        std::thread::sleep((deadline - now).min(Duration::from_millis(5)));
    }
}

trait ProbePipe: Send {
    fn poll_read(&mut self, buffer: &mut [u8]) -> io::Result<usize>;
}

#[cfg(unix)]
impl<R> ProbePipe for R
where
    R: Read + Send + std::os::unix::io::AsRawFd + 'static,
{
    fn poll_read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.read(buffer)
    }
}

#[cfg(windows)]
impl<R> ProbePipe for R
where
    R: Read + Send + std::os::windows::io::AsRawHandle + 'static,
{
    fn poll_read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let mut available = 0_u32;
        let peeked = unsafe {
            PeekNamedPipe(
                self.as_raw_handle() as *mut std::ffi::c_void,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                std::ptr::addr_of_mut!(available),
                std::ptr::null_mut(),
            )
        };
        if peeked == 0 {
            return Ok(0);
        }
        if available == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "probe pipe is idle",
            ));
        }
        self.read(buffer)
    }
}

#[cfg(unix)]
fn boxed_probe_pipe<R>(pipe: R) -> io::Result<Box<dyn ProbePipe>>
where
    R: Read + Send + std::os::unix::io::AsRawFd + 'static,
{
    use std::os::unix::io::AsRawFd;

    let fd = pipe.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Box::new(pipe))
}

#[cfg(windows)]
fn boxed_probe_pipe<R>(pipe: R) -> io::Result<Box<dyn ProbePipe>>
where
    R: Read + Send + std::os::windows::io::AsRawHandle + 'static,
{
    Ok(Box::new(pipe))
}

struct BoundedProbeCapture {
    max_bytes: usize,
    total_bytes: AtomicUsize,
    overflowed: AtomicBool,
    stdout: Mutex<Vec<u8>>,
    stderr: Mutex<Vec<u8>>,
}

impl BoundedProbeCapture {
    fn new(max_bytes: usize) -> Self {
        Self {
            max_bytes,
            total_bytes: AtomicUsize::new(0),
            overflowed: AtomicBool::new(false),
            stdout: Mutex::new(Vec::new()),
            stderr: Mutex::new(Vec::new()),
        }
    }

    fn append(&self, stdout: bool, bytes: &[u8]) {
        let mut current = self.total_bytes.load(Ordering::Acquire);
        let allowed = loop {
            if current >= self.max_bytes {
                self.overflowed.store(true, Ordering::Release);
                break 0;
            }
            let allowed = bytes.len().min(self.max_bytes - current);
            match self.total_bytes.compare_exchange(
                current,
                current + allowed,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break allowed,
                Err(next) => current = next,
            }
        };
        if allowed < bytes.len() {
            self.overflowed.store(true, Ordering::Release);
        }
        if allowed == 0 {
            return;
        }
        if stdout {
            self.stdout
                .lock()
                .unwrap()
                .extend_from_slice(&bytes[..allowed]);
        } else {
            self.stderr
                .lock()
                .unwrap()
                .extend_from_slice(&bytes[..allowed]);
        }
    }

    fn finish(&self) -> (Vec<u8>, Vec<u8>, bool) {
        (
            std::mem::take(&mut *self.stdout.lock().unwrap()),
            std::mem::take(&mut *self.stderr.lock().unwrap()),
            self.overflowed.load(Ordering::Acquire),
        )
    }
}

struct ProbeReaderHandle {
    cancelled: Arc<AtomicBool>,
    join: Option<JoinHandle<io::Result<()>>>,
    deadline: std::time::Instant,
}

/// Owns an exceptional reader that did not join by the probe deadline.  The
/// reaper itself is bounded by that same absolute deadline; dropping its
/// `JoinHandle` after the bound is the last-resort containment boundary for a
/// custom pipe that violates the production non-blocking pipe contract.
struct ProbeReaderReaper {
    join: JoinHandle<io::Result<()>>,
    deadline: std::time::Instant,
}

const PROBE_READER_REAPER_THREAD_NAME: &str = "devmanager-provider-probe-reader-reaper";

impl ProbeReaderReaper {
    fn run(self) {
        let _reader_reaper = PROBE_READER_REAPER_THREAD_NAME;
        while !self.join.is_finished() {
            let now = std::time::Instant::now();
            if now >= self.deadline {
                break;
            }
            std::thread::sleep((self.deadline - now).min(Duration::from_millis(1)));
        }
        if self.join.is_finished() {
            let _ = self.join.join();
        }
        // Dropping an unfinished JoinHandle after the absolute deadline is
        // the last-resort containment boundary for a custom pipe that
        // violates the production non-blocking contract.
    }
}

impl Drop for ProbeReaderHandle {
    fn drop(&mut self) {
        let Some(join) = self.join.take() else {
            return;
        };
        self.cancelled.store(true, Ordering::Release);
        // Keep cleanup owned on this thread.  Spawning a helper here would
        // detach the JoinHandle if thread creation failed, and the happy
        // path already joined through `receive_probe_reader`.
        ProbeReaderReaper {
            join,
            deadline: self.deadline,
        }
        .run();
    }
}

fn spawn_probe_reader(
    mut pipe: Box<dyn ProbePipe>,
    capture: Arc<BoundedProbeCapture>,
    stdout: bool,
    deadline: std::time::Instant,
) -> ProbeReaderHandle {
    let cancelled = Arc::new(AtomicBool::new(false));
    let reader_cancelled = Arc::clone(&cancelled);
    let join = std::thread::spawn(move || {
        let mut buffer = [0_u8; 8 * 1024];
        while !reader_cancelled.load(Ordering::Acquire) {
            match pipe.poll_read(&mut buffer) {
                Ok(0) => return Ok(()),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    if reader_cancelled.load(Ordering::Acquire) {
                        return Ok(());
                    }
                    let now = std::time::Instant::now();
                    if now >= deadline {
                        return Ok(());
                    }
                    std::thread::sleep((deadline - now).min(Duration::from_millis(5)));
                }
                Err(error) => return Err(error),
                Ok(read) => capture.append(stdout, &buffer[..read]),
            }
        }
        Ok(())
    });
    ProbeReaderHandle {
        cancelled,
        join: Some(join),
        deadline,
    }
}

fn receive_probe_reader(
    reader: &mut ProbeReaderHandle,
    deadline: std::time::Instant,
) -> Result<(), ProviderProbeError> {
    let Some(join) = reader.join.take() else {
        return Ok(());
    };
    while !join.is_finished() {
        let now = std::time::Instant::now();
        if now >= deadline {
            // Cancellation uses the caller's absolute deadline. A real pipe
            // closes when the owned provider process is killed; retain the
            // handle if a custom pipe violates that contract rather than
            // silently detaching an unowned reader worker.
            reader.cancelled.store(true, Ordering::Release);
            while !join.is_finished() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
            if !join.is_finished() {
                reader.join = Some(join);
                return Err(ProviderProbeError::Io(ProviderProbeIoError::WaitFailed));
            }
            break;
        }
        std::thread::sleep((deadline - now).min(Duration::from_millis(5)));
    }
    let result = join
        .join()
        .map_err(|_| ProviderProbeError::Io(ProviderProbeIoError::WaitFailed))?;
    result.map_err(|_| ProviderProbeError::Io(ProviderProbeIoError::WaitFailed))
}

fn receive_probe_readers(
    stdout: &mut ProbeReaderHandle,
    stderr: &mut ProbeReaderHandle,
    deadline: std::time::Instant,
) -> Result<(), ProviderProbeError> {
    while stdout.join.as_ref().is_some_and(|join| !join.is_finished())
        || stderr.join.as_ref().is_some_and(|join| !join.is_finished())
    {
        let now = std::time::Instant::now();
        if now >= deadline {
            // Both readers share the probe's one absolute cleanup deadline;
            // signal both before collecting either result so one idle stream
            // cannot consume the entire remaining join window.
            stdout.cancelled.store(true, Ordering::Release);
            stderr.cancelled.store(true, Ordering::Release);
            break;
        }
        std::thread::sleep((deadline - now).min(Duration::from_millis(5)));
    }

    let stdout_result = receive_probe_reader(stdout, deadline);
    let stderr_result = receive_probe_reader(stderr, deadline);
    stdout_result.and(stderr_result)
}

#[derive(Clone, PartialEq, Eq)]
pub enum ProviderError {
    DuplicateProviderKind(ProviderKind),
    ProviderNotRegistered(ProviderKind),
    MissingCli {
        kind: ProviderKind,
        requested: Option<PathBuf>,
    },
    WrapperCommandNotAllowed {
        path: PathBuf,
    },
    ExecutableNotAllowed {
        kind: ProviderKind,
        path: PathBuf,
    },
    Executable(ProviderExecutableError),
    ExecutableChanged {
        before: ProviderExecutable,
        after: ProviderExecutable,
    },
    Probe(ProviderProbeError),
    MalformedVersion(ProviderVersionError),
    CapabilityKindMismatch {
        expected: ProviderKind,
        actual: ProviderKind,
    },
    CapabilityVersionMismatch {
        expected: ProviderVersion,
        actual: ProviderVersion,
    },
    InvalidCapabilities(ProviderCapabilitiesError),
    InvalidExecutablePolicy(ProviderExecutablePolicyError),
    UnsupportedCapability(ProviderCapability),
    DependencyUnavailable {
        capability: ProviderCapability,
    },
    Discovery(ProviderDiscoveryError),
    AuthEvidence(ProviderAuthEvidenceError),
    UntrustedAuthenticationEvidence,
}

impl fmt::Debug for ProviderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let code = match self {
            Self::DuplicateProviderKind(_) => "duplicate_provider_kind",
            Self::ProviderNotRegistered(_) => "provider_not_registered",
            Self::MissingCli { .. } => "missing_cli",
            Self::WrapperCommandNotAllowed { .. } => "wrapper_command_not_allowed",
            Self::ExecutableNotAllowed { .. } => "executable_not_allowed",
            Self::Executable(_) => "executable",
            Self::ExecutableChanged { .. } => "executable_changed",
            Self::Probe(_) => "probe",
            Self::MalformedVersion(_) => "malformed_version",
            Self::CapabilityKindMismatch { .. } => "capability_kind_mismatch",
            Self::CapabilityVersionMismatch { .. } => "capability_version_mismatch",
            Self::InvalidCapabilities(_) => "invalid_capabilities",
            Self::InvalidExecutablePolicy(_) => "invalid_executable_policy",
            Self::UnsupportedCapability(_) => "unsupported_capability",
            Self::DependencyUnavailable { .. } => "dependency_unavailable",
            Self::Discovery(_) => "discovery",
            Self::AuthEvidence(_) => "auth_evidence",
            Self::UntrustedAuthenticationEvidence => "untrusted_authentication_evidence",
        };
        formatter
            .debug_struct("ProviderError")
            .field("code", &code)
            .finish()
    }
}

impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateProviderKind(kind) => {
                write!(f, "provider kind is already registered: {kind:?}")
            }
            Self::ProviderNotRegistered(kind) => {
                write!(f, "provider kind is not registered: {kind:?}")
            }
            Self::MissingCli { kind, .. } => write!(f, "missing {kind:?} CLI"),
            Self::WrapperCommandNotAllowed { .. } => {
                write!(
                    f,
                    "wrapper commands and package runners are not provider executables"
                )
            }
            Self::ExecutableNotAllowed { kind, .. } => {
                write!(f, "{kind:?} does not declare the requested executable")
            }
            Self::Executable(error) => error.fmt(f),
            Self::ExecutableChanged { .. } => {
                write!(f, "provider executable identity changed during observation")
            }
            Self::Probe(error) => error.fmt(f),
            Self::MalformedVersion(error) => error.fmt(f),
            Self::CapabilityKindMismatch { expected, actual } => {
                write!(
                    f,
                    "adapter returned {actual:?} capabilities for {expected:?}"
                )
            }
            Self::CapabilityVersionMismatch { expected, actual } => {
                write!(
                    f,
                    "adapter returned version {actual} after probing {expected}"
                )
            }
            Self::InvalidCapabilities(error) => write!(f, "invalid provider capabilities: {error}"),
            Self::InvalidExecutablePolicy(error) => {
                write!(f, "invalid provider executable policy: {error}")
            }
            Self::UnsupportedCapability(capability) => {
                write!(f, "provider capability is unsupported: {capability:?}")
            }
            Self::DependencyUnavailable { capability } => {
                write!(
                    f,
                    "provider capability dependency is unavailable: {capability:?}"
                )
            }
            Self::Discovery(error) => error.fmt(f),
            Self::AuthEvidence(error) => error.fmt(f),
            Self::UntrustedAuthenticationEvidence => {
                write!(
                    f,
                    "provider adapter returned untrusted authentication evidence"
                )
            }
        }
    }
}

impl std::error::Error for ProviderError {}

impl From<ProviderProbeError> for ProviderError {
    fn from(error: ProviderProbeError) -> Self {
        Self::Probe(error)
    }
}

impl From<ProviderVersionError> for ProviderError {
    fn from(error: ProviderVersionError) -> Self {
        Self::MalformedVersion(error)
    }
}

impl From<ProviderCapabilitiesError> for ProviderError {
    fn from(error: ProviderCapabilitiesError) -> Self {
        Self::InvalidCapabilities(error)
    }
}

impl From<ProviderDiscoveryError> for ProviderError {
    fn from(error: ProviderDiscoveryError) -> Self {
        Self::Discovery(error)
    }
}

impl From<ProviderAuthEvidenceError> for ProviderError {
    fn from(error: ProviderAuthEvidenceError) -> Self {
        Self::AuthEvidence(error)
    }
}

#[async_trait]
pub trait ProviderAdapter: Send + Sync {
    fn kind(&self) -> ProviderKind;

    /// Probe non-authentication capabilities for an already validated
    /// executable under one immutable per-instance context. Authentication is
    /// a registry-owned receipt flow.
    async fn probe(
        &self,
        executable: &ProviderExecutableHandle,
        context: &ProviderProbeContext,
    ) -> Result<ProviderCapabilities, ProviderError>;

    fn build_launch(
        &self,
        request: LaunchProviderRequest,
    ) -> Result<ProviderLaunchSpec, ProviderError>;

    /// Binding-required normalize seam. Must not mint EventId/sequence.
    /// Free stock adapters return typed unavailability; Claude/Codex require
    /// authenticated current-generation admission bridges first. Cursor stays
    /// typed unsupported.
    fn normalize_delivery(
        &self,
        permit: &AdapterDeliveryPermit,
        bytes: &[u8],
    ) -> Result<NormalizedAdapterDelivery, JournalNormalizeError>;

    fn cooperative_stop(&self, session: &ProviderRuntime) -> StopStrategy;

    async fn observe_quota(
        &self,
        executable: &ProviderExecutableHandle,
    ) -> Result<Option<QuotaObservation>, ProviderError>;
}

/// Bounded attested interactive probe for metadata-only protocols (Claude
/// initialize, Codex app-server JSON-RPC, Cursor ACP). Kill-on-drop, fixed
/// overall deadline, bounded stdout/stderr. Not a conversation launcher.
pub struct ProviderInteractiveSession {
    process: ProbeProcess,
    stdin: Option<ChildStdin>,
    stdout: Option<Box<dyn ProbePipe>>,
    stderr: Option<Box<dyn ProbePipe>>,
    stdout_buf: Vec<u8>,
    stderr_buf: Vec<u8>,
    /// Cumulative stdout+stderr bytes observed (including drained lines).
    total_io_bytes: usize,
    lines_read: usize,
    stdin_bytes_written: usize,
    max_output_bytes: usize,
    max_lines: usize,
    max_stdin_bytes: usize,
    deadline: std::time::Instant,
    cancel: Option<Arc<AtomicBool>>,
    executable: ProviderExecutableHandle,
    finished: bool,
}

pub const MAX_INTERACTIVE_PROBE_LINES: usize = 4_096;
pub const MAX_INTERACTIVE_STDIN_BYTES: usize = 64 * 1024;
pub const MAX_INTERACTIVE_WRITE_CHUNK: usize = 8 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderInteractiveProbeError {
    Probe(ProviderProbeError),
    TooManyArguments,
    ArgumentTooLong,
    StdinClosed,
    StdinTooLarge,
    OutputTooLarge,
    TooManyLines,
    TimedOut,
    Cancelled,
    Protocol(String),
}

impl fmt::Display for ProviderInteractiveProbeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Probe(error) => write!(f, "{error}"),
            Self::TooManyArguments => write!(f, "interactive probe has too many arguments"),
            Self::ArgumentTooLong => write!(f, "interactive probe argument exceeds bound"),
            Self::StdinClosed => write!(f, "interactive probe stdin closed"),
            Self::StdinTooLarge => write!(f, "interactive probe stdin exceeded bound"),
            Self::OutputTooLarge => write!(f, "interactive probe output exceeded bound"),
            Self::TooManyLines => write!(f, "interactive probe line count exceeded bound"),
            Self::TimedOut => write!(f, "interactive probe timed out"),
            Self::Cancelled => write!(f, "interactive probe cancelled"),
            Self::Protocol(message) => write!(f, "interactive probe protocol: {message}"),
        }
    }
}

impl std::error::Error for ProviderInteractiveProbeError {}

impl From<ProviderProbeError> for ProviderInteractiveProbeError {
    fn from(value: ProviderProbeError) -> Self {
        match value {
            ProviderProbeError::TimedOut => Self::TimedOut,
            ProviderProbeError::OutputTooLarge => Self::OutputTooLarge,
            other => Self::Probe(other),
        }
    }
}

impl WindowsProviderProbeRunner {
    /// Spawn a metadata-only interactive child with piped stdin/stdout/stderr.
    /// Arguments are fixed argv tokens (never shell-interpolated).
    pub fn spawn_interactive(
        &self,
        executable: ProviderExecutableHandle,
        arguments: &[String],
        child_environment: BTreeMap<std::ffi::OsString, std::ffi::OsString>,
        timeout: Duration,
        max_output_bytes: usize,
    ) -> Result<ProviderInteractiveSession, ProviderInteractiveProbeError> {
        self.spawn_interactive_with_cancel(
            executable,
            arguments,
            child_environment,
            timeout,
            max_output_bytes,
            None,
        )
    }

    pub fn spawn_interactive_with_cancel(
        &self,
        executable: ProviderExecutableHandle,
        arguments: &[String],
        child_environment: BTreeMap<std::ffi::OsString, std::ffi::OsString>,
        timeout: Duration,
        max_output_bytes: usize,
        cancel: Option<Arc<AtomicBool>>,
    ) -> Result<ProviderInteractiveSession, ProviderInteractiveProbeError> {
        if arguments.len() > MAX_PROVIDER_ARGUMENTS {
            return Err(ProviderInteractiveProbeError::TooManyArguments);
        }
        for argument in arguments {
            if argument.len() > MAX_PROVIDER_ARGUMENT_BYTES {
                return Err(ProviderInteractiveProbeError::ArgumentTooLong);
            }
        }
        if timeout.is_zero() || timeout > MAX_PROVIDER_PROBE_TIMEOUT {
            return Err(ProviderProbeError::InvalidRequest(if timeout.is_zero() {
                ProviderProbeRequestError::ZeroTimeout
            } else {
                ProviderProbeRequestError::TimeoutTooLong
            })
            .into());
        }
        if max_output_bytes == 0 || max_output_bytes > MAX_PROVIDER_PROBE_OUTPUT_BYTES {
            return Err(ProviderProbeError::InvalidRequest(
                ProviderProbeRequestError::OutputBoundTooLarge,
            )
            .into());
        }
        if cancel
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Acquire))
        {
            return Err(ProviderInteractiveProbeError::Cancelled);
        }
        let deadline = std::time::Instant::now() + timeout;
        #[cfg(windows)]
        let resolved = validate_probe_executable(&self.policy, &executable)?;
        #[cfg(windows)]
        let mut command = std::process::Command::new(&resolved);
        #[cfg(windows)]
        {
            command
                .args(executable.launch_fixed_arguments())
                .args(arguments)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            apply_provider_environment_exact(&mut command, &child_environment);
            command
                .creation_flags(crate::services::platform_service::MANAGED_PROCESS_CREATION_FLAGS);
        }
        #[cfg(not(windows))]
        {
            let _ = (&executable, arguments, &child_environment, &self.policy);
            return Err(ProviderInteractiveProbeError::Protocol(
                "interactive metadata probe is Windows-attested in this slice".into(),
            ));
        }
        #[cfg(windows)]
        {
            let mut process = ProbeProcess::spawn(
                command,
                deadline,
                Some(executable.launch_program().canonical_path()),
                &executable,
            )?;
            if executable.revalidate_bound_identity().is_err() {
                let _ = process.terminate_tree(deadline);
                return Err(
                    ProviderProbeError::Io(ProviderProbeIoError::ExecutableNotAllowed).into(),
                );
            }
            if std::time::Instant::now() >= deadline {
                let _ = process.terminate_tree(deadline);
                return Err(ProviderInteractiveProbeError::TimedOut);
            }
            if let Err(error) = process.release_attestation_barrier_once() {
                let _ = process.terminate_tree(deadline);
                return Err(error.into());
            }
            // Match the production probe runner: post-barrier image/graph
            // revalidation before any protocol I/O.
            let post_revalidate = executable.revalidate_bound_identity();
            let post_attestation = if post_revalidate.is_ok() {
                let retry_deadline = std::cmp::min(
                    deadline,
                    std::time::Instant::now() + Duration::from_millis(50),
                );
                let mut attestation = Err(ProviderProbeError::Io(
                    ProviderProbeIoError::ExecutableNotAllowed,
                ));
                loop {
                    match process.try_wait() {
                        Ok(ProbeWait::Exited(_)) => {
                            attestation = Ok(());
                            break;
                        }
                        Ok(ProbeWait::Running) => {
                            attestation = attest_launched_image(
                                &process.child,
                                executable.launch_program().canonical_path(),
                            );
                            if attestation.is_ok() || std::time::Instant::now() >= retry_deadline {
                                break;
                            }
                            std::thread::sleep(Duration::from_millis(1));
                        }
                        Err(_) => break,
                    }
                }
                attestation
            } else {
                Err(ProviderProbeError::Io(
                    ProviderProbeIoError::ExecutableNotAllowed,
                ))
            };
            if post_revalidate.is_err() || post_attestation.is_err() {
                let _ = process.terminate_tree(deadline);
                return Err(
                    ProviderProbeError::Io(ProviderProbeIoError::ExecutableNotAllowed).into(),
                );
            }
            let stdin = process.take_stdin();
            let stdout = process.take_stdout();
            let stderr = process.take_stderr();
            #[cfg(windows)]
            {
                if let Some(stdin_ref) = stdin.as_ref() {
                    if let Err(_) = set_stdin_pipe_nowait(stdin_ref) {
                        let _ = process.terminate_tree(deadline);
                        return Err(ProviderInteractiveProbeError::Protocol(
                            "failed to set stdin PIPE_NOWAIT".into(),
                        ));
                    }
                } else {
                    let _ = process.terminate_tree(deadline);
                    return Err(ProviderInteractiveProbeError::StdinClosed);
                }
            }
            Ok(ProviderInteractiveSession {
                process,
                stdin,
                stdout,
                stderr,
                stdout_buf: Vec::new(),
                stderr_buf: Vec::new(),
                total_io_bytes: 0,
                lines_read: 0,
                stdin_bytes_written: 0,
                max_output_bytes,
                max_lines: MAX_INTERACTIVE_PROBE_LINES,
                max_stdin_bytes: MAX_INTERACTIVE_STDIN_BYTES,
                deadline,
                cancel,
                executable,
                finished: false,
            })
        }
    }
}

#[cfg(windows)]
fn set_stdin_pipe_nowait(stdin: &ChildStdin) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    let mode = PIPE_NOWAIT;
    let ok = unsafe {
        SetNamedPipeHandleState(
            stdin.as_raw_handle() as *mut std::ffi::c_void,
            &mode,
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

impl ProviderInteractiveSession {
    pub fn write_stdin(&mut self, bytes: &[u8]) -> Result<(), ProviderInteractiveProbeError> {
        self.ensure_alive()?;
        // Metadata protocols only emit small fixed JSON lines; reject larger
        // writes rather than blocking indefinitely on an unbounded pipe.
        if bytes.len() > MAX_INTERACTIVE_WRITE_CHUNK {
            return Err(ProviderInteractiveProbeError::StdinTooLarge);
        }
        if self.stdin_bytes_written.saturating_add(bytes.len()) > self.max_stdin_bytes {
            return Err(ProviderInteractiveProbeError::StdinTooLarge);
        }
        let mut offset = 0;
        let cancel = self.cancel.clone();
        let deadline = self.deadline;
        let stdin = self
            .stdin
            .as_mut()
            .ok_or(ProviderInteractiveProbeError::StdinClosed)?;
        while offset < bytes.len() {
            if std::time::Instant::now() >= deadline {
                return Err(ProviderInteractiveProbeError::TimedOut);
            }
            if cancel
                .as_ref()
                .is_some_and(|flag| flag.load(Ordering::Acquire))
            {
                return Err(ProviderInteractiveProbeError::Cancelled);
            }
            match stdin.write(&bytes[offset..]) {
                Ok(0) => {
                    // PIPE_NOWAIT full buffer can surface as Ok(0); wait for deadline.
                    std::thread::sleep(Duration::from_millis(1));
                }
                Ok(written) => {
                    offset = offset.saturating_add(written);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(error)
                    if error.raw_os_error() == Some(232)
                        || error.kind() == io::ErrorKind::Interrupted =>
                {
                    // Windows ERROR_NO_DATA (232) on PIPE_NOWAIT when the buffer is full.
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(_) => return Err(ProviderInteractiveProbeError::StdinClosed),
            }
        }
        // Best-effort flush; PIPE_NOWAIT may return WouldBlock.
        match stdin.flush() {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) if error.raw_os_error() == Some(232) => {}
            Err(_) => return Err(ProviderInteractiveProbeError::StdinClosed),
        }
        self.stdin_bytes_written = self.stdin_bytes_written.saturating_add(bytes.len());
        Ok(())
    }

    pub fn write_line(&mut self, line: &str) -> Result<(), ProviderInteractiveProbeError> {
        let mut bytes = line.as_bytes().to_vec();
        if !bytes.ends_with(b"\n") {
            bytes.push(b'\n');
        }
        self.write_stdin(&bytes)
    }

    pub fn close_stdin(&mut self) {
        self.stdin.take();
    }

    /// Read until a complete line is available or the deadline hits.
    pub fn read_line(&mut self) -> Result<String, ProviderInteractiveProbeError> {
        loop {
            if let Some(idx) = self.stdout_buf.iter().position(|b| *b == b'\n') {
                let mut line: Vec<u8> = self.stdout_buf.drain(..=idx).collect();
                if line.ends_with(b"\n") {
                    line.pop();
                }
                if line.ends_with(b"\r") {
                    line.pop();
                }
                self.lines_read = self.lines_read.saturating_add(1);
                if self.lines_read > self.max_lines {
                    let _ = self.process.terminate_tree(self.deadline);
                    self.finished = true;
                    return Err(ProviderInteractiveProbeError::TooManyLines);
                }
                return String::from_utf8(line)
                    .map_err(|_| ProviderInteractiveProbeError::Protocol("stdout utf8".into()));
            }
            self.ensure_alive()?;
            if std::time::Instant::now() >= self.deadline {
                return Err(ProviderInteractiveProbeError::TimedOut);
            }
            self.pump_once()?;
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Read lines until `predicate` returns true for a line (that line is included).
    pub fn read_until<F>(
        &mut self,
        mut predicate: F,
    ) -> Result<Vec<String>, ProviderInteractiveProbeError>
    where
        F: FnMut(&str) -> bool,
    {
        let mut lines = Vec::new();
        loop {
            let line = self.read_line()?;
            // Bound retained line vector by the same line/IO budget.
            if lines.len() >= self.max_lines {
                let _ = self.process.terminate_tree(self.deadline);
                self.finished = true;
                return Err(ProviderInteractiveProbeError::TooManyLines);
            }
            let done = predicate(&line);
            lines.push(line);
            if done {
                return Ok(lines);
            }
        }
    }

    pub fn total_io_bytes(&self) -> usize {
        self.total_io_bytes
    }

    pub fn terminate(mut self) -> Result<(), ProviderInteractiveProbeError> {
        self.finished = true;
        self.stdin.take();
        self.process
            .terminate_tree(self.deadline)
            .map_err(ProviderInteractiveProbeError::from)
    }

    fn ensure_alive(&mut self) -> Result<(), ProviderInteractiveProbeError> {
        if self.finished {
            return Err(ProviderInteractiveProbeError::Cancelled);
        }
        if self
            .cancel
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Acquire))
        {
            let _ = self.process.terminate_tree(self.deadline);
            self.finished = true;
            return Err(ProviderInteractiveProbeError::Cancelled);
        }
        if std::time::Instant::now() >= self.deadline {
            let _ = self.process.terminate_tree(self.deadline);
            self.finished = true;
            return Err(ProviderInteractiveProbeError::TimedOut);
        }
        if self.executable.revalidate_bound_identity().is_err() {
            let _ = self.process.terminate_tree(self.deadline);
            self.finished = true;
            return Err(ProviderProbeError::Io(ProviderProbeIoError::ExecutableNotAllowed).into());
        }
        match self.process.try_wait() {
            Ok(ProbeWait::Running) => Ok(()),
            Ok(ProbeWait::Exited(_)) => Err(ProviderInteractiveProbeError::Protocol(
                "metadata process exited early".into(),
            )),
            Err(_) => Err(ProviderProbeError::Io(ProviderProbeIoError::WaitFailed).into()),
        }
    }

    fn pump_once(&mut self) -> Result<(), ProviderInteractiveProbeError> {
        let mut buffer = [0_u8; 8 * 1024];
        if let Some(stdout) = self.stdout.as_mut() {
            match stdout.poll_read(&mut buffer) {
                Ok(0) => {}
                Ok(read) => {
                    if self.total_io_bytes.saturating_add(read) > self.max_output_bytes {
                        let _ = self.process.terminate_tree(self.deadline);
                        self.finished = true;
                        return Err(ProviderInteractiveProbeError::OutputTooLarge);
                    }
                    self.total_io_bytes = self.total_io_bytes.saturating_add(read);
                    self.stdout_buf.extend_from_slice(&buffer[..read]);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(_) => {
                    return Err(ProviderProbeError::Io(ProviderProbeIoError::WaitFailed).into());
                }
            }
        }
        if let Some(stderr) = self.stderr.as_mut() {
            match stderr.poll_read(&mut buffer) {
                Ok(0) => {}
                Ok(read) => {
                    if self.total_io_bytes.saturating_add(read) > self.max_output_bytes {
                        let _ = self.process.terminate_tree(self.deadline);
                        self.finished = true;
                        return Err(ProviderInteractiveProbeError::OutputTooLarge);
                    }
                    self.total_io_bytes = self.total_io_bytes.saturating_add(read);
                    self.stderr_buf.extend_from_slice(&buffer[..read]);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(_) => {}
            }
        }
        Ok(())
    }
}

impl Drop for ProviderInteractiveSession {
    fn drop(&mut self) {
        if !self.finished {
            self.stdin.take();
            let _ = self.process.terminate_tree(self.deadline);
            self.finished = true;
        }
    }
}

#[cfg(test)]
mod tests {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    use super::ProbeProcess;
    use super::{
        classify_auth_output, ProviderAuthEvidenceError, ProviderAuthProbeResult,
        ProviderExecutable, ProviderKind, ProviderProbeKind, ProviderProbeOutput,
        ProviderProbeRequest, ProviderProbeResult,
    };
    #[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
    use super::{ProviderExecutablePolicy, ProviderProbeError};
    use crate::providers::capabilities::ProviderAuthEvidenceRegistry;
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    use std::path::Path;
    #[cfg(target_os = "linux")]
    use std::process::Stdio;
    use std::time::Duration;

    #[test]
    fn auth_parser_rejects_negative_and_contradictory_subscription_text() {
        assert_eq!(
            classify_auth_output(ProviderKind::ClaudeCode, b"not logged in", b""),
            ProviderAuthProbeResult::AuthRequired
        );
        assert_eq!(
            classify_auth_output(
                ProviderKind::ClaudeCode,
                b"logged in with claude.ai; not logged in",
                b""
            ),
            ProviderAuthProbeResult::Unknown
        );
    }

    #[test]
    fn auth_parser_never_promotes_api_key_login_to_subscription() {
        assert_eq!(
            classify_auth_output(ProviderKind::Codex, b"logged in using API key", b""),
            ProviderAuthProbeResult::ApiKeyDetected
        );
    }

    #[test]
    fn auth_parser_requires_provider_specific_subscription_markers() {
        assert_eq!(
            classify_auth_output(ProviderKind::ClaudeCode, b"logged in", b""),
            ProviderAuthProbeResult::Unknown
        );
        assert_eq!(
            classify_auth_output(ProviderKind::ClaudeCode, b"subscription active", b""),
            ProviderAuthProbeResult::Unknown
        );
        assert_eq!(
            classify_auth_output(ProviderKind::ClaudeCode, b"logged in with claude.ai", b""),
            ProviderAuthProbeResult::AuthenticatedSubscription
        );
        assert_eq!(
            classify_auth_output(
                ProviderKind::Codex,
                br#"{"authenticated":true,"auth_method":"chatgpt"}"#,
                b""
            ),
            ProviderAuthProbeResult::AuthenticatedSubscription
        );
        assert_eq!(
            classify_auth_output(
                ProviderKind::ClaudeCode,
                br#"{"authenticated":true,"message":"not logged in"}"#,
                b""
            ),
            ProviderAuthProbeResult::AuthRequired
        );
    }

    #[test]
    fn cursor_auth_classifier_uses_strict_about_json() {
        assert_eq!(
            classify_auth_output(
                ProviderKind::Cursor,
                br#"{"cliVersion":"1.2.3","userEmail":"a@example.com","subscriptionTier":"pro"}"#,
                b""
            ),
            ProviderAuthProbeResult::AuthenticatedSubscription
        );
        assert_eq!(
            classify_auth_output(
                ProviderKind::Cursor,
                br#"{"cliVersion":"1.2.3","userEmail":"a@example.com","authenticated":false}"#,
                b""
            ),
            ProviderAuthProbeResult::AuthRequired
        );
        assert_eq!(
            classify_auth_output(
                ProviderKind::Cursor,
                br#"{"cliVersion":"1.2.3","userEmail":null}"#,
                b""
            ),
            ProviderAuthProbeResult::AuthRequired
        );
        // Plain labels are not accepted on the trusted JSON auth path.
        assert_eq!(
            classify_auth_output(
                ProviderKind::Cursor,
                b"CLI Version  1.2.3\nUser Email  user@example.com",
                b""
            ),
            ProviderAuthProbeResult::Unknown
        );
    }

    #[test]
    fn auth_probe_kind_uses_login_status_for_codex() {
        assert_eq!(
            ProviderProbeKind::for_auth_probe(ProviderKind::Codex),
            ProviderProbeKind::LoginStatus
        );
        assert_eq!(
            ProviderProbeKind::for_auth_probe(ProviderKind::ClaudeCode),
            ProviderProbeKind::AuthStatus
        );
    }

    #[test]
    fn auth_observation_is_bound_to_one_issued_invocation() {
        let executable = ProviderExecutable::from_path(std::env::current_exe().unwrap()).unwrap();
        let mut evidence = ProviderAuthEvidenceRegistry::new();
        let first = evidence
            .begin(
                ProviderKind::ClaudeCode,
                executable.clone(),
                Duration::from_secs(30),
            )
            .unwrap();
        let second = evidence
            .begin(
                ProviderKind::ClaudeCode,
                executable,
                Duration::from_secs(30),
            )
            .unwrap();
        let request = ProviderProbeRequest::auth_status(first.executable_handle().clone())
            .unwrap()
            .bind_to_auth_invocation(&first)
            .unwrap();
        assert!(request.clone().bind_to_auth_invocation(&second).is_err());
        let output =
            ProviderProbeOutput::new(b"logged in with claude.ai".to_vec(), Vec::new(), Some(0))
                .unwrap();
        let result = ProviderProbeResult::with_trusted_output(&request, output).unwrap();
        let observation = result.into_auth_observation(&first, &request).unwrap();
        assert!(matches!(
            evidence.accept_observation(second, observation),
            Err(ProviderAuthEvidenceError::RequestBindingMismatch)
        ));

        // The exact request/observation pair remains valid for its own
        // invocation after the cross-invocation attempt is rejected.
        let output =
            ProviderProbeOutput::new(b"logged in with claude.ai".to_vec(), Vec::new(), Some(0))
                .unwrap();
        let result = ProviderProbeResult::with_trusted_output(&request, output).unwrap();
        let observation = result.into_auth_observation(&first, &request).unwrap();
        let accepted = evidence.accept_observation(first, observation);
        assert!(accepted.is_ok(), "{accepted:?}");
    }

    #[test]
    fn public_completed_result_cannot_describe_auth_status() {
        let executable = ProviderExecutable::from_path(std::env::current_exe().unwrap()).unwrap();
        let request =
            ProviderProbeRequest::auth_status(executable.open_for_launch().unwrap()).unwrap();
        assert!(matches!(
            ProviderProbeResult::completed(&request, 0, 0, 0),
            Err(super::ProviderProbeError::InvalidRequest(
                super::ProviderProbeRequestError::AuthStatusRequiresRunnerProof
            ))
        ));
    }

    #[test]
    fn auth_probe_result_without_runner_proof_is_rejected() {
        let executable = ProviderExecutable::from_path(std::env::current_exe().unwrap()).unwrap();
        let mut evidence = ProviderAuthEvidenceRegistry::new();
        let invocation = evidence
            .begin(
                ProviderKind::ClaudeCode,
                executable.clone(),
                Duration::from_secs(30),
            )
            .unwrap();
        let request = ProviderProbeRequest::auth_status(invocation.executable_handle().clone())
            .unwrap()
            .bind_to_auth_invocation(&invocation)
            .unwrap();
        let output =
            ProviderProbeOutput::new(b"logged in with claude.ai".to_vec(), Vec::new(), Some(0))
                .unwrap();
        let result = ProviderProbeResult::with_output(&request, output).unwrap();

        assert!(matches!(
            result.into_auth_observation(&invocation, &request),
            Err(ProviderAuthEvidenceError::UntrustedAuthenticationEvidence)
        ));
    }

    #[test]
    fn auth_probe_result_cannot_attach_to_a_different_request_same_invocation() {
        let executable = ProviderExecutable::from_path(std::env::current_exe().unwrap()).unwrap();
        let mut evidence = ProviderAuthEvidenceRegistry::new();
        let invocation = evidence
            .begin(
                ProviderKind::ClaudeCode,
                executable,
                Duration::from_secs(30),
            )
            .unwrap();
        let request = invocation
            .bind_request(
                ProviderProbeRequest::with_limits(
                    invocation.executable_handle().clone(),
                    ProviderProbeKind::AuthStatus,
                    Duration::from_secs(1),
                    1024,
                )
                .unwrap(),
            )
            .unwrap();
        let altered_request = invocation
            .bind_request(
                ProviderProbeRequest::with_limits(
                    invocation.executable_handle().clone(),
                    ProviderProbeKind::AuthStatus,
                    Duration::from_secs(2),
                    1024,
                )
                .unwrap(),
            )
            .unwrap();
        let output =
            ProviderProbeOutput::new(b"logged in with claude.ai".to_vec(), Vec::new(), Some(0))
                .unwrap();
        let result = ProviderProbeResult::with_trusted_output(&request, output).unwrap();

        let mismatch = result
            .clone()
            .into_auth_observation(&invocation, &altered_request);
        assert!(matches!(
            mismatch,
            Err(ProviderAuthEvidenceError::RequestBindingMismatch)
        ));
        assert!(result.into_auth_observation(&invocation, &request).is_ok());
    }

    #[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
    #[test]
    fn unsupported_unix_attestation_is_typed_and_fail_closed() {
        let executable = ProviderExecutable::from_path(std::env::current_exe().unwrap()).unwrap();
        let handle = executable.open_for_launch().unwrap();
        let file_name = handle.canonical_path().file_name().unwrap().to_os_string();
        let policy = ProviderExecutablePolicy::new([file_name]).unwrap();
        assert!(matches!(
            prepare_unix_launch(&policy, &handle),
            Err(ProviderProbeError::UnsupportedAttestation)
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_image_attestation_rejects_a_different_expected_image() {
        use std::process::Command;

        let mut child = Command::new(std::env::current_exe().unwrap())
            .arg("--help")
            .spawn()
            .unwrap();
        assert!(
            attest_launched_image(&child, Path::new("/definitely/not-the-running-image")).is_err()
        );
        let _ = child.kill();
        let _ = child.wait();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_plain_sigtrap_is_not_an_exec_event() {
        let plain_sigtrap = (LINUX_SIGTRAP << 8) | 0x7f;
        let exact_exec_event = plain_sigtrap | (LINUX_PTRACE_EVENT_EXEC << 16);

        assert!(!linux_status_is_exact_exec_event(plain_sigtrap));
        assert!(linux_status_is_exact_exec_event(exact_exec_event));
    }

    struct WouldBlockProbePipe;

    impl super::ProbePipe for WouldBlockProbePipe {
        fn poll_read(&mut self, _buffer: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "test pipe remains idle",
            ))
        }
    }

    struct FailingProbePipe;

    impl super::ProbePipe for FailingProbePipe {
        fn poll_read(&mut self, _buffer: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "test pipe failed",
            ))
        }
    }

    #[test]
    fn probe_reader_cancellation_is_bounded() {
        let capture = std::sync::Arc::new(super::BoundedProbeCapture::new(64));
        let deadline = std::time::Instant::now() + Duration::from_millis(250);
        let mut reader =
            super::spawn_probe_reader(Box::new(WouldBlockProbePipe), capture, true, deadline);
        // The production caller signals cancellation after its owned process
        // has been reaped. Exercise that join path directly; waiting until an
        // exact deadline would make the test depend on OS thread scheduling at
        // the deadline boundary.
        reader
            .cancelled
            .store(true, std::sync::atomic::Ordering::Release);

        assert!(super::receive_probe_reader(&mut reader, deadline,).is_ok());
        assert!(reader.join.is_none(), "reader worker must be joined");
    }

    #[test]
    fn probe_reader_surfaces_pipe_errors() {
        let capture = std::sync::Arc::new(super::BoundedProbeCapture::new(64));
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        let mut reader =
            super::spawn_probe_reader(Box::new(FailingProbePipe), capture, true, deadline);

        assert!(super::receive_probe_reader(&mut reader, deadline,).is_err());
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn unix_probe_process_has_no_user_code_side_effect_before_attestation_release() {
        let temp = tempfile::tempdir().unwrap();
        let marker = temp.path().join("provider-started");
        let requested = ProviderExecutable::from_path(std::env::current_exe().unwrap()).unwrap();
        let requested_handle = requested.open_for_launch().unwrap();
        let expected = ProviderExecutable::from_path(Path::new("/bin/sh")).unwrap();

        #[cfg(not(target_os = "macos"))]
        use std::process::{Command, Stdio};
        #[cfg(not(target_os = "macos"))]
        let mut command = Command::new(expected.canonical_path());
        #[cfg(not(target_os = "macos"))]
        command
            .arg("-c")
            .arg("printf started > \"$DEV_MANAGER_PROVIDER_IDENTITY_MARKER\"; sleep 2")
            .env("DEV_MANAGER_PROVIDER_IDENTITY_MARKER", &marker)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(not(target_os = "macos"))]
        command.process_group(0);

        #[cfg(not(target_os = "macos"))]
        let mut process = ProbeProcess::spawn(
            command,
            std::time::Instant::now() + Duration::from_secs(3),
            Some(expected.canonical_path()),
            &requested_handle,
        )
        .unwrap();
        #[cfg(target_os = "macos")]
        let mut process = ProbeProcess::spawn_macos(
            expected.canonical_path(),
            &[
                std::ffi::OsString::from("-c"),
                std::ffi::OsString::from("printf started > \"$1\"; sleep 2"),
                std::ffi::OsString::from("--"),
                marker.as_os_str().to_os_string(),
            ],
            &[],
            Vec::new(),
            std::time::Instant::now() + Duration::from_secs(3),
            expected.canonical_path(),
            &requested_handle,
        )
        .unwrap();

        assert!(
            !marker.exists(),
            "provider user code ran before the image/graph attestation barrier released it"
        );

        let _ = process.terminate_tree(std::time::Instant::now() + Duration::from_secs(3));
    }
}
