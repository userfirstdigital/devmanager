//! Object-safe provider adapter and bounded native-CLI boundaries.
//!
//! This phase defines the provider-neutral contracts only. It does not launch
//! a provider runtime, infer authentication, or consume a quota subscription.

use crate::domain::ProviderSessionId;
use crate::providers::capabilities::{
    ProviderAuthEvidenceError, ProviderAuthEvidenceSource, ProviderAuthProbeObservation,
    ProviderAuthProbeResult, ProviderCapabilities, ProviderCapabilitiesError, ProviderCapability,
    ProviderDiscoveryError, ProviderExecutable, ProviderExecutableError, ProviderExecutableHandle,
    ProviderExecutablePolicy, ProviderExecutablePolicyError, ProviderKind, ProviderVersion,
    ProviderVersionError,
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
}

impl fmt::Debug for ProviderProbeRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderProbeRequest")
            .field("executable_bound", &true)
            .field("kind", &self.kind)
            .field("timeout", &self.timeout)
            .field("max_output_bytes", &self.max_output_bytes)
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
        })
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
        kind: ProviderKind,
        request: &ProviderProbeRequest,
        executable: ProviderExecutableHandle,
        version: ProviderVersion,
    ) -> Result<ProviderAuthProbeObservation, ProviderAuthEvidenceError> {
        if request.kind() != ProviderProbeKind::AuthStatus {
            return Err(ProviderAuthEvidenceError::UntrustedAuthenticationEvidence);
        }
        if self.status != ProviderProbeStatus::Completed {
            return Err(ProviderAuthEvidenceError::UntrustedAuthenticationEvidence);
        }
        let output = self
            .output_for_adapter()
            .ok_or(ProviderAuthEvidenceError::UntrustedAuthenticationEvidence)?;
        let mut text = String::with_capacity(output.stdout().len() + output.stderr().len());
        text.push_str(&String::from_utf8_lossy(output.stdout()).to_ascii_lowercase());
        text.push_str(&String::from_utf8_lossy(output.stderr()).to_ascii_lowercase());
        let result = if text.contains("authenticated subscription")
            || text.contains("subscription authenticated")
            || text.contains("logged in")
        {
            ProviderAuthProbeResult::AuthenticatedSubscription
        } else if text.contains("auth required") || text.contains("authentication required") {
            ProviderAuthProbeResult::AuthRequired
        } else if text.contains("api key") || text.contains("api_key") {
            ProviderAuthProbeResult::ApiKeyDetected
        } else {
            ProviderAuthProbeResult::Unknown
        };
        ProviderAuthProbeObservation::from_bounded_probe(
            kind,
            ProviderAuthEvidenceSource::for_kind(kind),
            executable,
            version,
            result,
            result.default_confidence(),
        )
    }
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
        let executable = validate_probe_executable(&self.policy, request.executable())?;
        let mut command = std::process::Command::new(&executable);
        command
            .args(request.arguments())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        scrub_provider_secret_environment(&mut command);

        #[cfg(windows)]
        command.creation_flags(crate::services::platform_service::MANAGED_PROCESS_CREATION_FLAGS);

        let mut process = ProbeProcess::spawn(command, deadline)?;
        // Standard Command cannot pass the opened executable handle through
        // CreateProcess. Revalidate immediately after spawn so a same-path
        // replacement cannot be mistaken for the requested identity.
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
    Ok(canonical)
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
        // Job ACTIVE_PROCESS_ZERO state is authoritative for the managed tree.
        // The raw `Child` handle can lag that state on Windows, so do not
        // spend the absolute deadline waiting for a second observation of the
        // same process exit.
        let child_exited = if self.managed_job.is_some() && job_empty {
            true
        } else {
            reap_child_until(&mut self.child, deadline)
        };
        if !job_empty || !child_exited {
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
