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
use async_trait::async_trait;
use std::fmt;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

#[cfg(unix)]
use std::os::unix::process::CommandExt;
#[cfg(windows)]
use std::os::windows::process::CommandExt;

pub const MAX_PROVIDER_INPUT_BYTES: usize = 64 * 1024;
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchProviderRequest {
    executable: ProviderExecutableHandle,
    input: Option<ProviderInput>,
    provider_session_id: Option<ProviderSessionId>,
}

impl LaunchProviderRequest {
    pub const fn new(
        executable: ProviderExecutableHandle,
        input: Option<ProviderInput>,
        provider_session_id: Option<ProviderSessionId>,
    ) -> Self {
        Self {
            executable,
            input,
            provider_session_id,
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderLaunchSpec {
    executable: ProviderExecutableHandle,
    arguments: Vec<ProviderArgument>,
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

/// Task 4.1 keeps the normalized journal event opaque until the journal task
/// owns its concrete event model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalEvent;

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
}

impl ProviderProbeKind {
    pub const fn arguments(self) -> &'static [&'static str] {
        match self {
            Self::Version => &["--version"],
            Self::Help => &["--help"],
            Self::AuthStatus => &["auth", "status"],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderProbeRequestError {
    EmptyExecutable,
    ZeroTimeout,
    TimeoutTooLong,
    OutputBoundTooLarge,
}

impl fmt::Display for ProviderProbeRequestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyExecutable => write!(f, "provider probe executable must be non-empty"),
            Self::ZeroTimeout => write!(f, "provider probe timeout must be non-zero"),
            Self::TimeoutTooLong => write!(f, "provider probe timeout exceeded its bound"),
            Self::OutputBoundTooLarge => write!(f, "provider probe output bound is too large"),
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
            .finish()
    }
}

impl ProviderProbeRequest {
    pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);
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
        })
    }

    /// Binds this request to one exact issued auth invocation.  The nonce and
    /// generation are private correlation material copied by the invocation;
    /// callers cannot select or replace them.
    pub fn bind_to_auth_invocation(
        mut self,
        invocation: &ProviderAuthProbeInvocation,
    ) -> Result<Self, ProviderAuthEvidenceError> {
        if self.kind != ProviderProbeKind::AuthStatus
            || self.executable != *invocation.executable_handle()
        {
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
        })
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
        &self,
        invocation: &ProviderAuthProbeInvocation,
        request: &ProviderProbeRequest,
    ) -> Result<ProviderAuthProbeObservation, ProviderAuthEvidenceError> {
        if request.kind() != ProviderProbeKind::AuthStatus
            || !request.auth_binding_matches(invocation)
        {
            return Err(ProviderAuthEvidenceError::UntrustedAuthenticationEvidence);
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
        ProviderKind::Cursor => contains_any(
            &lower,
            &[
                "authenticated with cursor",
                "logged in with cursor",
                "cursor subscription",
            ],
        ),
    };
    if api_key {
        return ProviderAuthProbeResult::ApiKeyDetected;
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
    OutputTooLarge,
    NonZeroExit(Option<i32>),
    InvalidRequest(ProviderProbeRequestError),
}

impl fmt::Display for ProviderProbeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "provider probe I/O failed: {error:?}"),
            Self::TimedOut => write!(f, "provider probe timed out"),
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
/// and resumed only after the claim succeeds. Both output pipes are drained
/// concurrently, while a shared admission counter enforces the request's
/// total byte bound exactly.
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
        #[cfg(unix)]
        let (executable, fixed_arguments, _launch_files) =
            prepare_unix_launch(&self.policy, request.executable())?;
        #[cfg(windows)]
        let executable = validate_probe_executable(&self.policy, request.executable())?;
        let mut command = std::process::Command::new(&executable);
        #[cfg(windows)]
        command.args(request.executable().launch_fixed_arguments());
        #[cfg(unix)]
        command.args(fixed_arguments);
        command
            .args(request.arguments())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        scrub_provider_secret_environment(&mut command);

        #[cfg(windows)]
        command.creation_flags(crate::services::platform_service::MANAGED_PROCESS_CREATION_FLAGS);
        #[cfg(unix)]
        command.process_group(0);

        let mut process = ProbeProcess::spawn(command, deadline)?;
        // Windows keeps the no-delete handles open through CreateProcess;
        // Unix uses inherited descriptor paths from `prepare_unix_launch`.
        // Revalidate immediately after spawn as a final identity diagnostic.
        if request.executable().revalidate().is_err() {
            process.terminate_tree(deadline)?;
            return Err(ProviderProbeError::Io(
                ProviderProbeIoError::ExecutableNotAllowed,
            ));
        }
        let stdout = process
            .child_mut()
            .stdout
            .take()
            .ok_or(ProviderProbeError::Io(ProviderProbeIoError::SpawnFailed))?;
        let stderr = process
            .child_mut()
            .stderr
            .take()
            .ok_or(ProviderProbeError::Io(ProviderProbeIoError::SpawnFailed))?;
        let capture = Arc::new(BoundedProbeCapture::new(request.max_output_bytes()));
        let stdout_reader = spawn_probe_reader(stdout, Arc::clone(&capture), true);
        let stderr_reader = spawn_probe_reader(stderr, Arc::clone(&capture), false);

        let mut timed_out = false;
        let exit_code = loop {
            match process
                .child_mut()
                .try_wait()
                .map_err(|_| ProviderProbeError::Io(ProviderProbeIoError::WaitFailed))?
            {
                Some(status) => break status.code(),
                None if std::time::Instant::now()
                    .checked_add(PROVIDER_PROBE_CLEANUP_RESERVE)
                    .is_some_and(|cleanup_start| cleanup_start < deadline) =>
                {
                    std::thread::sleep(Duration::from_millis(5));
                }
                None => {
                    timed_out = true;
                    process.terminate_tree(deadline)?;
                    break None;
                }
            }
        };

        if !timed_out {
            process.terminate_tree(deadline)?;
        }
        receive_probe_reader(stdout_reader)?;
        receive_probe_reader(stderr_reader)?;
        let (stdout, stderr, overflowed) = capture.finish();
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
        ProviderProbeResult::with_output(&request, output)
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
        .revalidate()
        .map_err(|_| ProviderProbeError::Io(ProviderProbeIoError::ExecutableNotAllowed))?;
    let canonical = requested.canonical_path().to_path_buf();
    policy
        .validate_canonical_path(&canonical)
        .map_err(|_| ProviderProbeError::Io(ProviderProbeIoError::ExecutableNotAllowed))?;
    Ok(requested.launch_program().canonical_path().to_path_buf())
}

#[cfg(unix)]
fn prepare_unix_launch(
    policy: &ProviderExecutablePolicy,
    requested: &ProviderExecutableHandle,
) -> Result<(PathBuf, Vec<std::ffi::OsString>, Vec<std::fs::File>), ProviderProbeError> {
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

fn scrub_provider_secret_environment(command: &mut std::process::Command) {
    for (key, _) in std::env::vars_os() {
        let normalized = key.to_string_lossy().to_ascii_uppercase();
        if is_provider_secret_environment_key(&normalized) {
            command.env_remove(key);
        }
    }
}

fn is_provider_secret_environment_key(key: &str) -> bool {
    [
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_AUTH_TOKEN",
        "CLAUDE_CODE_OAUTH_TOKEN",
        "OPENAI_API_KEY",
        "OPENAI_AUTH_TOKEN",
        "CODEX_API_KEY",
        "CODEX_AUTH_TOKEN",
        "CURSOR_API_KEY",
        "CURSOR_AUTH_TOKEN",
        "AWS_ACCESS_KEY_ID",
        "AWS_SECRET_ACCESS_KEY",
        "AWS_SESSION_TOKEN",
        "GOOGLE_APPLICATION_CREDENTIALS",
        "GITHUB_TOKEN",
        "GH_TOKEN",
    ]
    .iter()
    .any(|known| *known == key)
        || key.contains("API_KEY")
        || key.contains("APIKEY")
        || key.contains("TOKEN")
        || key.contains("AUTH_TOKEN")
        || key.contains("OAUTH_TOKEN")
        || key.contains("ACCESS_TOKEN")
        || key.contains("SECRET")
        || key.contains("CLIENT_SECRET")
        || key.contains("PRIVATE_KEY")
        || key.contains("CREDENTIAL")
}

struct ProbeProcess {
    child: Child,
    managed_job: Option<crate::process::job::ManagedProcessJob>,
    deadline: std::time::Instant,
    #[cfg(unix)]
    process_group: bool,
}

impl ProbeProcess {
    fn spawn(
        mut command: std::process::Command,
        deadline: std::time::Instant,
    ) -> Result<Self, ProviderProbeError> {
        let mut child = command.spawn().map_err(|error| {
            ProviderProbeError::Io(if error.kind() == std::io::ErrorKind::NotFound {
                ProviderProbeIoError::ExecutableMissing
            } else if error.kind() == std::io::ErrorKind::PermissionDenied {
                ProviderProbeIoError::PermissionDenied
            } else {
                ProviderProbeIoError::SpawnFailed
            })
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
        })
    }

    fn child_mut(&mut self) -> &mut Child {
        &mut self.child
    }

    fn terminate_tree(&mut self, deadline: std::time::Instant) -> Result<(), ProviderProbeError> {
        let mut job_empty = true;
        if let Some(job) = self.managed_job.as_ref() {
            let active_before = job.active_process_ids().map_err(|_| {
                ProviderProbeError::Io(ProviderProbeIoError::DescendantCleanupFailed)
            })?;
            if !active_before.is_empty() {
                job.terminate_members().map_err(|_| {
                    ProviderProbeError::Io(ProviderProbeIoError::DescendantCleanupFailed)
                })?;
            }
            job_empty = job.wait_for_active_process_zero(deadline).map_err(|_| {
                ProviderProbeError::Io(ProviderProbeIoError::DescendantCleanupFailed)
            })?;
        }
        #[cfg(unix)]
        let group_cleanup_ok = if self.process_group {
            crate::services::platform_service::terminate_owned_process_group(
                self.child.id(),
                deadline.saturating_duration_since(std::time::Instant::now()),
            )
            .is_ok()
        } else {
            true
        };
        #[cfg(not(unix))]
        let _group_cleanup_ok = true;
        // Job ACTIVE_PROCESS_ZERO state is authoritative for the managed tree.
        // The raw `Child` handle can lag that state on Windows, so do not
        // spend the absolute deadline waiting for a second observation of the
        // same process exit.
        let child_exited = if self.managed_job.is_some() && job_empty {
            true
        } else {
            reap_child_until(&mut self.child, deadline)
        };
        #[cfg(unix)]
        let group_exited =
            group_cleanup_ok && wait_for_unix_process_group_exit(self.child.id(), deadline);
        #[cfg(not(unix))]
        let group_exited = true;
        if !job_empty || !child_exited || !group_exited {
            return Err(ProviderProbeError::Io(
                ProviderProbeIoError::DescendantCleanupFailed,
            ));
        }
        drop(self.managed_job.take());
        Ok(())
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
fn wait_for_unix_process_group_exit(pid: u32, deadline: std::time::Instant) -> bool {
    let group_target = format!("-{pid}");
    loop {
        let exists = std::process::Command::new("kill")
            .args(["-0", "--", group_target.as_str()])
            .status()
            .is_ok_and(|status| status.success());
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

fn spawn_probe_reader<R: Read + Send + 'static>(
    mut pipe: R,
    capture: Arc<BoundedProbeCapture>,
    stdout: bool,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut buffer = [0_u8; 8 * 1024];
        loop {
            match pipe.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(read) => capture.append(stdout, &buffer[..read]),
            }
        }
    })
}

fn receive_probe_reader(reader: JoinHandle<()>) -> Result<(), ProviderProbeError> {
    reader
        .join()
        .map_err(|_| ProviderProbeError::Io(ProviderProbeIoError::WaitFailed))
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
    /// executable. Authentication is a registry-owned receipt flow.
    async fn probe(
        &self,
        executable: &ProviderExecutable,
    ) -> Result<ProviderCapabilities, ProviderError>;

    fn build_launch(
        &self,
        request: LaunchProviderRequest,
    ) -> Result<ProviderLaunchSpec, ProviderError>;

    fn parse_signal(&self, signal: ProviderSignal) -> Vec<JournalEvent>;

    fn cooperative_stop(&self, session: &ProviderRuntime) -> StopStrategy;

    async fn observe_quota(
        &self,
        executable: &ProviderExecutable,
    ) -> Result<Option<QuotaObservation>, ProviderError>;
}

#[cfg(test)]
mod tests {
    use super::{
        classify_auth_output, ProviderAuthEvidenceError, ProviderAuthProbeResult,
        ProviderExecutable, ProviderKind, ProviderProbeOutput, ProviderProbeRequest,
        ProviderProbeResult,
    };
    use crate::providers::capabilities::ProviderAuthEvidenceRegistry;
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
        let result = ProviderProbeResult::with_output(&request, output).unwrap();
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
        let result = ProviderProbeResult::with_output(&request, output).unwrap();
        let observation = result.into_auth_observation(&first, &request).unwrap();
        let accepted = evidence.accept_observation(first, observation);
        assert!(accepted.is_ok(), "{accepted:?}");
    }
}
