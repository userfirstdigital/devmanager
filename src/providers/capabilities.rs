use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const MAX_PROVIDER_VERSION_BYTES: usize = 128;
pub const MAX_CAPABILITY_EVIDENCE_ITEMS: usize = 16;
pub const MAX_EXECUTABLE_ENTRYPOINT_BYTES: usize = 128;
pub const PROVIDER_AUTH_NONCE_BYTES: usize = 32;
pub const MAX_PROVIDER_SHIM_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    ClaudeCode,
    Codex,
    Cursor,
}

impl ProviderKind {
    pub const ALL: [Self; 3] = [Self::ClaudeCode, Self::Codex, Self::Cursor];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilitySupport {
    Supported,
    Unsupported,
    Unknown,
}

impl Default for CapabilitySupport {
    fn default() -> Self {
        Self::Unknown
    }
}

impl CapabilitySupport {
    pub const fn is_supported(self) -> bool {
        matches!(self, Self::Supported)
    }

    pub const fn is_known(self) -> bool {
        !matches!(self, Self::Unknown)
    }
}

pub type CapabilityState = CapabilitySupport;
pub type CapabilityStatus = CapabilitySupport;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderCapability {
    ExactResume,
    SemanticEvents,
    ProviderSessionId,
    BuildLaunch,
    ParseSignal,
    CooperativeStop,
    ObserveQuota,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderAuthState {
    AuthenticatedSubscription,
    AuthRequired,
    Unknown,
}

impl Default for ProviderAuthState {
    fn default() -> Self {
        Self::Unknown
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderVersionError {
    Empty,
    InvalidUtf8,
    ContainsControlCharacter,
    TooLong,
    MultipleLines,
}

impl fmt::Display for ProviderVersionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "provider version must be non-empty"),
            Self::InvalidUtf8 => write!(f, "provider version output was not valid UTF-8"),
            Self::ContainsControlCharacter => {
                write!(f, "provider version must not contain control characters")
            }
            Self::TooLong => write!(
                f,
                "provider version exceeds {MAX_PROVIDER_VERSION_BYTES} bytes"
            ),
            Self::MultipleLines => write!(f, "provider version output contained multiple values"),
        }
    }
}

impl std::error::Error for ProviderVersionError {}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct ProviderVersion(String);

impl ProviderVersion {
    pub fn new(value: impl Into<String>) -> Result<Self, ProviderVersionError> {
        let value = value.into();
        if value.is_empty() || value.trim().is_empty() {
            return Err(ProviderVersionError::Empty);
        }
        if value.len() > MAX_PROVIDER_VERSION_BYTES {
            return Err(ProviderVersionError::TooLong);
        }
        if value.chars().any(char::is_control) {
            return Err(ProviderVersionError::ContainsControlCharacter);
        }
        Ok(Self(value))
    }

    pub fn from_probe_output(output: &[u8]) -> Result<Self, ProviderVersionError> {
        let output = std::str::from_utf8(output).map_err(|_| ProviderVersionError::InvalidUtf8)?;
        let mut lines = output.lines().filter(|line| !line.trim().is_empty());
        let first = lines.next().ok_or(ProviderVersionError::Empty)?;
        if lines.next().is_some() {
            return Err(ProviderVersionError::MultipleLines);
        }
        Self::new(first.trim())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }

    pub fn into_inner(self) -> String {
        self.0
    }
}

impl fmt::Display for ProviderVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::str::FromStr for ProviderVersion {
    type Err = ProviderVersionError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl TryFrom<String> for ProviderVersion {
    type Error = ProviderVersionError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl<'de> Deserialize<'de> for ProviderVersion {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(de::Error::custom)
    }
}

macro_rules! define_revision {
    ($name:ident) => {
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        pub struct $name(u32);

        impl $name {
            pub const fn new(value: u32) -> Self {
                Self(value)
            }

            pub const fn value(self) -> u32 {
                self.0
            }
        }
    };
}

define_revision!(AdapterRevision);
define_revision!(SemanticSchemaVersion);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceSourceId {
    ExecutableVersion,
    AuthStatusProbe,
    CapabilityProbe,
    Registry,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceStatus {
    Supported,
    Unsupported,
    Unknown,
    Authenticated,
    AuthRequired,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceDiagnosticCode {
    AuthenticationRequired,
    ExecutableMissing,
    ProbeTimedOut,
    ProbeFailed,
    OutputBoundExceeded,
    VersionMalformed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceDiagnostic {
    code: EvidenceDiagnosticCode,
    digest: Option<[u8; 32]>,
}

impl EvidenceDiagnostic {
    pub const fn new(code: EvidenceDiagnosticCode, digest: Option<[u8; 32]>) -> Self {
        Self { code, digest }
    }

    pub const fn code(&self) -> EvidenceDiagnosticCode {
        self.code
    }

    pub const fn digest(&self) -> Option<&[u8; 32]> {
        self.digest.as_ref()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityEvidenceError {
    ObservedAtZero,
}

impl fmt::Display for CapabilityEvidenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ObservedAtZero => write!(f, "capability evidence observed_at must be non-zero"),
        }
    }
}

impl std::error::Error for CapabilityEvidenceError {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityEvidence {
    source: EvidenceSourceId,
    observed_at: u64,
    status: EvidenceStatus,
    diagnostic: Option<EvidenceDiagnostic>,
}

impl CapabilityEvidence {
    pub fn new(
        source: EvidenceSourceId,
        observed_at: u64,
        status: EvidenceStatus,
        diagnostic: Option<EvidenceDiagnostic>,
    ) -> Result<Self, CapabilityEvidenceError> {
        let evidence = Self {
            source,
            observed_at,
            status,
            diagnostic,
        };
        evidence.validate()?;
        Ok(evidence)
    }

    pub const fn source(&self) -> EvidenceSourceId {
        self.source
    }

    pub const fn observed_at(&self) -> u64 {
        self.observed_at
    }

    pub const fn status(&self) -> EvidenceStatus {
        self.status
    }

    pub const fn diagnostic(&self) -> Option<&EvidenceDiagnostic> {
        self.diagnostic.as_ref()
    }

    pub fn validate(&self) -> Result<(), CapabilityEvidenceError> {
        if self.observed_at == 0 {
            return Err(CapabilityEvidenceError::ObservedAtZero);
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for CapabilityEvidence {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            source: EvidenceSourceId,
            observed_at: u64,
            status: EvidenceStatus,
            diagnostic: Option<EvidenceDiagnostic>,
        }

        let wire = Wire::deserialize(deserializer)?;
        Self::new(wire.source, wire.observed_at, wire.status, wire.diagnostic)
            .map_err(de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderExecutableError {
    EmptyPath,
    Missing(PathBuf),
    NotAFile(PathBuf),
    SymlinkOrReparse(PathBuf),
    HardlinkAmbiguous(PathBuf),
    ChangedDuringValidation(PathBuf),
    InvalidFileIdentity(PathBuf),
    NotCanonical {
        requested: PathBuf,
        canonical: PathBuf,
    },
    HashMismatch(PathBuf),
    Io {
        path: PathBuf,
        kind: io::ErrorKind,
    },
    BackgroundTask,
}

impl fmt::Display for ProviderExecutableError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyPath => write!(f, "provider executable path must be non-empty"),
            Self::Missing(path) => {
                write!(f, "provider executable does not exist: {}", path.display())
            }
            Self::NotAFile(path) => {
                write!(f, "provider executable is not a file: {}", path.display())
            }
            Self::SymlinkOrReparse(path) => write!(
                f,
                "provider executable must not be a symlink or reparse point: {}",
                path.display()
            ),
            Self::HardlinkAmbiguous(path) => write!(
                f,
                "provider executable must not have hardlink ambiguity: {}",
                path.display()
            ),
            Self::ChangedDuringValidation(path) => write!(
                f,
                "provider executable changed during validation: {}",
                path.display()
            ),
            Self::InvalidFileIdentity(path) => write!(
                f,
                "provider executable has invalid file identity: {}",
                path.display()
            ),
            Self::NotCanonical {
                requested,
                canonical,
            } => write!(
                f,
                "provider executable path is not canonical: {} (canonical {})",
                requested.display(),
                canonical.display()
            ),
            Self::HashMismatch(path) => write!(
                f,
                "provider executable hash does not match: {}",
                path.display()
            ),
            Self::Io { path, kind } => {
                write!(
                    f,
                    "could not inspect provider executable {} ({kind:?})",
                    path.display()
                )
            }
            Self::BackgroundTask => write!(f, "provider executable inspection task failed"),
        }
    }
}

impl std::error::Error for ProviderExecutableError {}

/// The OS identity of the exact regular file that was inspected.
///
/// The identity is deliberately retained in addition to the canonical path
/// and content hash. A path and a timestamp are not sufficient to identify a
/// runnable provider after replacement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "platform", rename_all = "snake_case")]
pub enum ProviderFileIdentity {
    Windows {
        volume_serial: u32,
        file_index: [u8; 16],
        link_count: u32,
    },
    Unix {
        device: u64,
        inode: u64,
        link_count: u64,
    },
    Other {
        stable_id: [u8; 16],
        link_count: u64,
    },
}

impl ProviderFileIdentity {
    pub const fn link_count(self) -> u64 {
        match self {
            Self::Windows { link_count, .. } => link_count as u64,
            Self::Unix { link_count, .. } | Self::Other { link_count, .. } => link_count,
        }
    }

    pub const fn stable_id(self) -> u128 {
        match self {
            Self::Windows {
                volume_serial,
                file_index,
                ..
            } => ((volume_serial as u128) << 96) | u128::from_le_bytes(file_index),
            Self::Unix { device, inode, .. } => ((device as u128) << 64) | inode as u128,
            Self::Other { stable_id, .. } => u128::from_le_bytes(stable_id),
        }
    }

    fn validate(self, path: &Path) -> Result<(), ProviderExecutableError> {
        if self.link_count() != 1 {
            return Err(ProviderExecutableError::HardlinkAmbiguous(
                path.to_path_buf(),
            ));
        }
        if self.stable_id() == 0 {
            return Err(ProviderExecutableError::InvalidFileIdentity(
                path.to_path_buf(),
            ));
        }
        Ok(())
    }
}

/// The result of a provider-specific authentication observation. The
/// provider adapter chooses the supported command; this type deliberately
/// contains no provider-neutral command or argument assumption.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderAuthProbeResult {
    AuthenticatedSubscription,
    AuthRequired,
    ApiKeyDetected,
    Unknown,
}

impl ProviderAuthProbeResult {
    pub const fn is_authenticated_subscription(self) -> bool {
        matches!(self, Self::AuthenticatedSubscription)
    }

    pub const fn as_stable_state(self) -> Option<ProviderAuthState> {
        match self {
            Self::AuthenticatedSubscription => Some(ProviderAuthState::AuthenticatedSubscription),
            Self::AuthRequired => Some(ProviderAuthState::AuthRequired),
            Self::Unknown | Self::ApiKeyDetected => Some(ProviderAuthState::Unknown),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderAuthEvidenceError {
    InvalidDeadline,
    NonceGenerationFailed,
    UnknownInvocation,
    WrongProvider,
    WrongExecutable,
    Expired,
    FutureTimestamp,
    Reordered,
    NonMonotonicTimestamp,
    ExecutableChanged(ProviderExecutableError),
}

impl fmt::Display for ProviderAuthEvidenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidDeadline => write!(f, "auth probe deadline must follow issuance"),
            Self::NonceGenerationFailed => write!(f, "could not issue auth probe nonce"),
            Self::UnknownInvocation => write!(f, "auth evidence invocation was not issued"),
            Self::WrongProvider => write!(f, "auth evidence provider does not match invocation"),
            Self::WrongExecutable => {
                write!(f, "auth evidence executable does not match invocation")
            }
            Self::Expired => write!(f, "auth evidence invocation is expired"),
            Self::FutureTimestamp => write!(
                f,
                "auth evidence timestamp is not a current monotonic reading"
            ),
            Self::Reordered => write!(f, "auth evidence generation is reordered"),
            Self::NonMonotonicTimestamp => {
                write!(f, "auth evidence timestamp is not strictly increasing")
            }
            Self::ExecutableChanged(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for ProviderAuthEvidenceError {}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ProviderAuthInvocationKey {
    kind: ProviderKind,
    executable: ProviderExecutable,
    nonce: [u8; PROVIDER_AUTH_NONCE_BYTES],
}

#[derive(Debug, Clone)]
pub struct ProviderAuthProbeInvocation {
    kind: ProviderKind,
    executable: ProviderExecutable,
    nonce: [u8; PROVIDER_AUTH_NONCE_BYTES],
    generation: u64,
    issued_at: Instant,
    deadline: Instant,
}

impl ProviderAuthProbeInvocation {
    pub const fn provider_kind(&self) -> ProviderKind {
        self.kind
    }

    pub const fn nonce(&self) -> &[u8; PROVIDER_AUTH_NONCE_BYTES] {
        &self.nonce
    }

    pub const fn generation(&self) -> u64 {
        self.generation
    }

    pub const fn executable(&self) -> &ProviderExecutable {
        &self.executable
    }

    pub const fn issued_at(&self) -> Instant {
        self.issued_at
    }

    pub const fn deadline(&self) -> Instant {
        self.deadline
    }

    fn key(&self) -> ProviderAuthInvocationKey {
        ProviderAuthInvocationKey {
            kind: self.kind,
            executable: self.executable.clone(),
            nonce: self.nonce,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProviderAuthEvidenceReceipt {
    kind: ProviderKind,
    executable: ProviderExecutable,
    nonce: [u8; PROVIDER_AUTH_NONCE_BYTES],
    generation: u64,
    result: ProviderAuthProbeResult,
    observed_at: Instant,
    deadline: Instant,
}

impl ProviderAuthEvidenceReceipt {
    pub const fn provider_kind(&self) -> ProviderKind {
        self.kind
    }

    pub const fn executable(&self) -> &ProviderExecutable {
        &self.executable
    }

    pub const fn nonce(&self) -> &[u8; PROVIDER_AUTH_NONCE_BYTES] {
        &self.nonce
    }

    pub const fn generation(&self) -> u64 {
        self.generation
    }

    pub const fn result(&self) -> ProviderAuthProbeResult {
        self.result
    }

    pub const fn observed_at(&self) -> Instant {
        self.observed_at
    }

    pub const fn deadline(&self) -> Instant {
        self.deadline
    }

    pub const fn is_authenticated_subscription(&self) -> bool {
        self.result.is_authenticated_subscription()
    }

    pub fn is_fresh_at(&self, now: Instant) -> bool {
        now >= self.observed_at && now <= self.deadline
    }
}

#[derive(Debug, Default)]
pub struct ProviderAuthEvidenceRegistry {
    next_generation: u64,
    pending: HashMap<ProviderAuthInvocationKey, ProviderAuthProbeInvocation>,
    last_accepted: HashMap<(ProviderKind, ProviderExecutable), (u64, Instant)>,
}

impl ProviderAuthEvidenceRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn begin(
        &mut self,
        kind: ProviderKind,
        executable: ProviderExecutable,
        ttl: Duration,
    ) -> Result<ProviderAuthProbeInvocation, ProviderAuthEvidenceError> {
        let issued_at = Instant::now();
        let deadline = issued_at
            .checked_add(ttl)
            .ok_or(ProviderAuthEvidenceError::InvalidDeadline)?;
        self.begin_at(kind, executable, issued_at, deadline)
    }

    pub fn begin_at(
        &mut self,
        kind: ProviderKind,
        executable: ProviderExecutable,
        issued_at: Instant,
        deadline: Instant,
    ) -> Result<ProviderAuthProbeInvocation, ProviderAuthEvidenceError> {
        if deadline <= issued_at {
            return Err(ProviderAuthEvidenceError::InvalidDeadline);
        }
        self.next_generation = self
            .next_generation
            .checked_add(1)
            .ok_or(ProviderAuthEvidenceError::NonceGenerationFailed)?;
        let mut nonce = [0_u8; PROVIDER_AUTH_NONCE_BYTES];
        getrandom::fill(&mut nonce)
            .map_err(|_| ProviderAuthEvidenceError::NonceGenerationFailed)?;
        if nonce == [0; PROVIDER_AUTH_NONCE_BYTES] {
            nonce[0] = 1;
        }
        let invocation = ProviderAuthProbeInvocation {
            kind,
            executable,
            nonce,
            generation: self.next_generation,
            issued_at,
            deadline,
        };
        self.pending.insert(invocation.key(), invocation.clone());
        Ok(invocation)
    }

    pub fn accept_at_for(
        &mut self,
        expected_kind: ProviderKind,
        expected_executable: &ProviderExecutable,
        invocation: ProviderAuthProbeInvocation,
        result: ProviderAuthProbeResult,
        observed_at: Instant,
    ) -> Result<ProviderAuthEvidenceReceipt, ProviderAuthEvidenceError> {
        if invocation.kind != expected_kind {
            return Err(ProviderAuthEvidenceError::WrongProvider);
        }
        if &invocation.executable != expected_executable {
            return Err(ProviderAuthEvidenceError::WrongExecutable);
        }
        let key = invocation.key();
        if !self.pending.contains_key(&key) {
            return Err(ProviderAuthEvidenceError::UnknownInvocation);
        }
        expected_executable
            .validate_current()
            .map_err(ProviderAuthEvidenceError::ExecutableChanged)?;
        if observed_at > Instant::now() {
            return Err(ProviderAuthEvidenceError::FutureTimestamp);
        }
        if observed_at < invocation.issued_at || observed_at > invocation.deadline {
            self.pending.remove(&key);
            return Err(ProviderAuthEvidenceError::Expired);
        }
        self.pending.remove(&key);

        let identity_key = (invocation.kind, invocation.executable.clone());
        if let Some((last_generation, last_observed_at)) = self.last_accepted.get(&identity_key) {
            if invocation.generation <= *last_generation {
                return Err(ProviderAuthEvidenceError::Reordered);
            }
            if observed_at <= *last_observed_at {
                return Err(ProviderAuthEvidenceError::NonMonotonicTimestamp);
            }
        }
        self.last_accepted
            .insert(identity_key, (invocation.generation, observed_at));
        Ok(ProviderAuthEvidenceReceipt {
            kind: invocation.kind,
            executable: invocation.executable,
            nonce: invocation.nonce,
            generation: invocation.generation,
            result,
            observed_at,
            deadline: invocation.deadline,
        })
    }

    pub fn accept_at(
        &mut self,
        invocation: ProviderAuthProbeInvocation,
        result: ProviderAuthProbeResult,
        observed_at: Instant,
    ) -> Result<ProviderAuthEvidenceReceipt, ProviderAuthEvidenceError> {
        let kind = invocation.kind;
        let executable = invocation.executable.clone();
        self.accept_at_for(kind, &executable, invocation, result, observed_at)
    }

    pub fn accept_now(
        &mut self,
        invocation: ProviderAuthProbeInvocation,
        result: ProviderAuthProbeResult,
    ) -> Result<ProviderAuthEvidenceReceipt, ProviderAuthEvidenceError> {
        self.accept_at(invocation, result, Instant::now())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct ProviderExecutable {
    canonical_path: PathBuf,
    file_identity: ProviderFileIdentity,
    sha256: [u8; 32],
}

impl ProviderExecutable {
    pub fn new(
        canonical_path: impl Into<PathBuf>,
        sha256: [u8; 32],
    ) -> Result<Self, ProviderExecutableError> {
        let canonical_path = canonical_path.into();
        if canonical_path.as_os_str().is_empty() {
            return Err(ProviderExecutableError::EmptyPath);
        }
        let inspected = Self::inspect_blocking(&canonical_path)?;
        if inspected.canonical_path != canonical_path {
            return Err(ProviderExecutableError::NotCanonical {
                requested: canonical_path,
                canonical: inspected.canonical_path,
            });
        }
        if inspected.sha256 != sha256 {
            return Err(ProviderExecutableError::HashMismatch(
                inspected.canonical_path,
            ));
        }
        Ok(inspected)
    }

    /// Resolve and inspect a candidate path without trusting its basename.
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self, ProviderExecutableError> {
        Self::inspect_blocking(path.as_ref())
    }

    /// Re-check the current path and file identity before using a cached fact.
    pub fn validate_current(&self) -> Result<(), ProviderExecutableError> {
        let current = Self::inspect_blocking(&self.canonical_path)?;
        if current == *self {
            Ok(())
        } else {
            Err(ProviderExecutableError::ChangedDuringValidation(
                self.canonical_path.clone(),
            ))
        }
    }

    pub(crate) fn inspect_blocking(path: &Path) -> Result<Self, ProviderExecutableError> {
        if path.as_os_str().is_empty() {
            return Err(ProviderExecutableError::EmptyPath);
        }
        reject_reparse_components(path)?;
        let canonical_path = fs::canonicalize(path).map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                ProviderExecutableError::Missing(path.to_path_buf())
            } else {
                ProviderExecutableError::Io {
                    path: path.to_path_buf(),
                    kind: error.kind(),
                }
            }
        })?;
        reject_reparse_components(&canonical_path)?;
        let first = inspect_snapshot(&canonical_path)?;
        let canonical_after =
            fs::canonicalize(path).map_err(|error| ProviderExecutableError::Io {
                path: path.to_path_buf(),
                kind: error.kind(),
            })?;
        if canonical_after != canonical_path {
            return Err(ProviderExecutableError::ChangedDuringValidation(
                path.to_path_buf(),
            ));
        }
        let second = inspect_snapshot(&canonical_path)?;
        if first != second {
            return Err(ProviderExecutableError::ChangedDuringValidation(
                canonical_path,
            ));
        }

        Ok(Self {
            canonical_path,
            file_identity: second.0,
            sha256: second.1,
        })
    }

    pub fn canonical_path(&self) -> &Path {
        &self.canonical_path
    }

    pub const fn file_identity(&self) -> &ProviderFileIdentity {
        &self.file_identity
    }

    pub fn sha256(&self) -> &[u8; 32] {
        &self.sha256
    }

    pub fn sha256_hex(&self) -> String {
        self.sha256
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }
}

impl<'de> Deserialize<'de> for ProviderExecutable {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            canonical_path: PathBuf,
            file_identity: ProviderFileIdentity,
            sha256: [u8; 32],
        }

        let wire = Wire::deserialize(deserializer)?;
        let inspected =
            Self::new(wire.canonical_path.clone(), wire.sha256).map_err(de::Error::custom)?;
        if inspected.file_identity != wire.file_identity {
            return Err(de::Error::custom(
                ProviderExecutableError::InvalidFileIdentity(wire.canonical_path),
            ));
        }
        Ok(inspected)
    }
}

fn inspect_snapshot(
    path: &Path,
) -> Result<(ProviderFileIdentity, [u8; 32]), ProviderExecutableError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            ProviderExecutableError::Missing(path.to_path_buf())
        } else {
            ProviderExecutableError::Io {
                path: path.to_path_buf(),
                kind: error.kind(),
            }
        }
    })?;
    if metadata.file_type().is_symlink() || metadata_is_reparse(&metadata) {
        return Err(ProviderExecutableError::SymlinkOrReparse(
            path.to_path_buf(),
        ));
    }
    if !metadata.is_file() {
        return Err(ProviderExecutableError::NotAFile(path.to_path_buf()));
    }

    let mut file = File::open(path).map_err(|error| ProviderExecutableError::Io {
        path: path.to_path_buf(),
        kind: error.kind(),
    })?;
    let file_identity = file_identity(&file, path)?;
    file_identity.validate(path)?;
    let sha256 = hash_file(&mut file, path)?;
    Ok((file_identity, sha256))
}

fn hash_file(file: &mut File, path: &Path) -> Result<[u8; 32], ProviderExecutableError> {
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 16 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| ProviderExecutableError::Io {
                path: path.to_path_buf(),
                kind: error.kind(),
            })?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().into())
}

fn reject_reparse_components(path: &Path) -> Result<(), ProviderExecutableError> {
    for ancestor in path.ancestors() {
        match fs::symlink_metadata(ancestor) {
            Ok(metadata) if metadata.file_type().is_symlink() || metadata_is_reparse(&metadata) => {
                return Err(ProviderExecutableError::SymlinkOrReparse(
                    ancestor.to_path_buf(),
                ));
            }
            Ok(_) | Err(_) => {}
        }
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn metadata_is_reparse(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
}

#[cfg(not(target_os = "windows"))]
fn metadata_is_reparse(_metadata: &fs::Metadata) -> bool {
    false
}

#[cfg(target_os = "windows")]
fn file_identity(
    file: &File,
    path: &Path,
) -> Result<ProviderFileIdentity, ProviderExecutableError> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };

    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut information) }.map_err(
        |_| ProviderExecutableError::Io {
            path: path.to_path_buf(),
            kind: io::ErrorKind::Other,
        },
    )?;
    let identity = ProviderFileIdentity::Windows {
        volume_serial: information.dwVolumeSerialNumber,
        file_index: (((information.nFileIndexHigh as u128) << 32)
            | information.nFileIndexLow as u128)
            .to_le_bytes(),
        link_count: information.nNumberOfLinks,
    };
    if information.dwFileAttributes
        & windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT.0
        != 0
    {
        return Err(ProviderExecutableError::SymlinkOrReparse(
            path.to_path_buf(),
        ));
    }
    Ok(identity)
}

#[cfg(unix)]
fn file_identity(
    file: &File,
    path: &Path,
) -> Result<ProviderFileIdentity, ProviderExecutableError> {
    use std::os::unix::fs::MetadataExt;

    let metadata = file
        .metadata()
        .map_err(|error| ProviderExecutableError::Io {
            path: path.to_path_buf(),
            kind: error.kind(),
        })?;
    Ok(ProviderFileIdentity::Unix {
        device: metadata.dev(),
        inode: metadata.ino(),
        link_count: metadata.nlink(),
    })
}

#[cfg(not(any(target_os = "windows", unix)))]
fn file_identity(
    file: &File,
    path: &Path,
) -> Result<ProviderFileIdentity, ProviderExecutableError> {
    let metadata = file
        .metadata()
        .map_err(|error| ProviderExecutableError::Io {
            path: path.to_path_buf(),
            kind: error.kind(),
        })?;
    let mut hasher = Sha256::new();
    hasher.update(path.as_os_str().to_string_lossy().as_bytes());
    hasher.update(metadata.len().to_le_bytes());
    let digest: [u8; 32] = hasher.finalize().into();
    Ok(ProviderFileIdentity::Other {
        stable_id: digest[..16].try_into().unwrap(),
        link_count: 1,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderExecutablePolicyError {
    EmptyEntrypoint,
    EntrypointTooLong,
    EntrypointContainsControlCharacter,
    EntrypointContainsPathSeparator,
    DuplicateEntrypoint,
    ForbiddenRunner,
}

impl fmt::Display for ProviderExecutablePolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyEntrypoint => write!(f, "provider executable entrypoint must be non-empty"),
            Self::EntrypointTooLong => write!(f, "provider executable entrypoint is too long"),
            Self::EntrypointContainsControlCharacter => {
                write!(
                    f,
                    "provider executable entrypoint contains a control character"
                )
            }
            Self::EntrypointContainsPathSeparator => {
                write!(f, "provider executable entrypoint must be a file name")
            }
            Self::DuplicateEntrypoint => {
                write!(f, "provider executable entrypoints must be unique")
            }
            Self::ForbiddenRunner => {
                write!(f, "shell and package runners are forbidden entrypoints")
            }
        }
    }
}

impl std::error::Error for ProviderExecutablePolicyError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderExecutablePolicyViolation {
    NotDeclared,
    ForbiddenRunner,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderExecutablePolicy {
    entrypoints: Vec<String>,
}

impl ProviderExecutablePolicy {
    pub fn new<I, S>(entrypoints: I) -> Result<Self, ProviderExecutablePolicyError>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut declared: Vec<String> = Vec::new();
        for entrypoint in entrypoints {
            let entrypoint = entrypoint.into();
            if entrypoint.is_empty() {
                return Err(ProviderExecutablePolicyError::EmptyEntrypoint);
            }
            if entrypoint.len() > MAX_EXECUTABLE_ENTRYPOINT_BYTES {
                return Err(ProviderExecutablePolicyError::EntrypointTooLong);
            }
            if entrypoint.chars().any(char::is_control) {
                return Err(ProviderExecutablePolicyError::EntrypointContainsControlCharacter);
            }
            if entrypoint
                .chars()
                .any(|character| matches!(character, '/' | '\\'))
            {
                return Err(ProviderExecutablePolicyError::EntrypointContainsPathSeparator);
            }
            if is_forbidden_runner_name(&entrypoint) {
                return Err(ProviderExecutablePolicyError::ForbiddenRunner);
            }
            if declared
                .iter()
                .any(|existing| same_entrypoint(existing, &entrypoint))
            {
                return Err(ProviderExecutablePolicyError::DuplicateEntrypoint);
            }
            declared.push(entrypoint);
        }
        if declared.is_empty() {
            return Err(ProviderExecutablePolicyError::EmptyEntrypoint);
        }
        Ok(Self {
            entrypoints: declared,
        })
    }

    pub fn entrypoints(&self) -> impl Iterator<Item = &str> {
        self.entrypoints.iter().map(String::as_str)
    }

    pub fn validate_canonical_path(
        &self,
        canonical_path: &Path,
    ) -> Result<(), ProviderExecutablePolicyViolation> {
        let Some(file_name) = canonical_path.file_name().and_then(|name| name.to_str()) else {
            return Err(ProviderExecutablePolicyViolation::NotDeclared);
        };
        if is_forbidden_runner_name(file_name) {
            return Err(ProviderExecutablePolicyViolation::ForbiddenRunner);
        }
        if self
            .entrypoints
            .iter()
            .any(|declared| same_entrypoint(declared, file_name))
        {
            Ok(())
        } else {
            Err(ProviderExecutablePolicyViolation::NotDeclared)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderDiscoveryOrigin {
    ConfiguredOverride,
    PathEntry { index: usize, directory: PathBuf },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderExecutableForm {
    Native,
    WindowsShim { target: Box<ProviderExecutable> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderDiscoveryCandidate {
    origin: ProviderDiscoveryOrigin,
    requested_path: PathBuf,
    executable: ProviderExecutable,
    form: ProviderExecutableForm,
}

impl ProviderDiscoveryCandidate {
    pub fn origin(&self) -> &ProviderDiscoveryOrigin {
        &self.origin
    }

    pub fn requested_path(&self) -> &Path {
        &self.requested_path
    }

    pub fn executable(&self) -> &ProviderExecutable {
        &self.executable
    }

    pub fn form(&self) -> &ProviderExecutableForm {
        &self.form
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderDiscoveryCandidateInput {
    Native {
        path: PathBuf,
        origin: ProviderDiscoveryOrigin,
    },
    WindowsShim {
        shim_path: PathBuf,
        target_path: PathBuf,
        origin: ProviderDiscoveryOrigin,
    },
}

impl ProviderDiscoveryCandidateInput {
    pub fn configured_override(path: impl Into<PathBuf>) -> Self {
        Self::Native {
            path: path.into(),
            origin: ProviderDiscoveryOrigin::ConfiguredOverride,
        }
    }

    pub fn path_entry(
        path: impl Into<PathBuf>,
        index: usize,
        directory: impl Into<PathBuf>,
    ) -> Self {
        Self::Native {
            path: path.into(),
            origin: ProviderDiscoveryOrigin::PathEntry {
                index,
                directory: directory.into(),
            },
        }
    }

    pub fn windows_shim(
        shim_path: impl Into<PathBuf>,
        target_path: impl Into<PathBuf>,
        origin: ProviderDiscoveryOrigin,
    ) -> Self {
        Self::WindowsShim {
            shim_path: shim_path.into(),
            target_path: target_path.into(),
            origin,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderDiscoveryError {
    UnsupportedPlatform,
    OriginNotAllowed(PathBuf),
    WrongEntrypoint(PathBuf),
    WrongFileType(PathBuf),
    ShimProofInvalid(PathBuf),
    Executable(ProviderExecutableError),
}

impl fmt::Display for ProviderDiscoveryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedPlatform => write!(f, "Windows provider shims are unsupported here"),
            Self::OriginNotAllowed(path) => write!(
                f,
                "provider executable origin is not allowlisted: {}",
                path.display()
            ),
            Self::WrongEntrypoint(path) => {
                write!(
                    f,
                    "provider executable is not an allowlisted entrypoint: {}",
                    path.display()
                )
            }
            Self::WrongFileType(path) => {
                write!(
                    f,
                    "provider executable has the wrong file type: {}",
                    path.display()
                )
            }
            Self::ShimProofInvalid(path) => {
                write!(
                    f,
                    "provider Windows shim proof is invalid: {}",
                    path.display()
                )
            }
            Self::Executable(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for ProviderDiscoveryError {}

impl From<ProviderExecutableError> for ProviderDiscoveryError {
    fn from(error: ProviderExecutableError) -> Self {
        Self::Executable(error)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderDiscoveryContract {
    kind: ProviderKind,
    native_entrypoint: String,
    shim_entrypoint: String,
}

impl ProviderDiscoveryContract {
    pub fn for_kind(kind: ProviderKind) -> Self {
        let stem = match kind {
            ProviderKind::ClaudeCode => "claude",
            ProviderKind::Codex => "codex",
            // The desktop Cursor executable is intentionally not this entrypoint.
            ProviderKind::Cursor => "cursor-agent",
        };
        let (native_entrypoint, shim_entrypoint) = if cfg!(windows) {
            (format!("{stem}.exe"), format!("{stem}.cmd"))
        } else {
            (stem.to_owned(), String::new())
        };
        Self {
            kind,
            native_entrypoint,
            shim_entrypoint,
        }
    }

    pub const fn kind(&self) -> ProviderKind {
        self.kind
    }

    pub fn native_entrypoint(&self) -> &str {
        &self.native_entrypoint
    }

    pub fn shim_entrypoint(&self) -> Option<&str> {
        (!self.shim_entrypoint.is_empty()).then_some(self.shim_entrypoint.as_str())
    }

    pub fn validate(
        &self,
        candidate: ProviderDiscoveryCandidateInput,
    ) -> Result<ProviderDiscoveryCandidate, ProviderDiscoveryError> {
        match candidate {
            ProviderDiscoveryCandidateInput::Native { path, origin } => {
                let executable = ProviderExecutable::from_path(&path)?;
                self.validate_origin(&executable, &origin)?;
                self.validate_native_path(&executable)?;
                Ok(ProviderDiscoveryCandidate {
                    origin,
                    requested_path: path,
                    executable,
                    form: ProviderExecutableForm::Native,
                })
            }
            ProviderDiscoveryCandidateInput::WindowsShim {
                shim_path,
                target_path,
                origin,
            } => self.validate_windows_shim(shim_path, target_path, origin),
        }
    }

    pub fn validate_in_order<I>(
        &self,
        candidates: I,
    ) -> Result<Vec<ProviderDiscoveryCandidate>, ProviderDiscoveryError>
    where
        I: IntoIterator<Item = ProviderDiscoveryCandidateInput>,
    {
        candidates
            .into_iter()
            .map(|candidate| self.validate(candidate))
            .collect()
    }

    fn validate_native_path(
        &self,
        executable: &ProviderExecutable,
    ) -> Result<(), ProviderDiscoveryError> {
        let file_name = executable
            .canonical_path()
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                ProviderDiscoveryError::WrongEntrypoint(executable.canonical_path().to_path_buf())
            })?;
        if !same_entrypoint(file_name, &self.native_entrypoint) {
            return Err(ProviderDiscoveryError::WrongEntrypoint(
                executable.canonical_path().to_path_buf(),
            ));
        }
        if cfg!(windows) && !file_name.to_ascii_lowercase().ends_with(".exe") {
            return Err(ProviderDiscoveryError::WrongFileType(
                executable.canonical_path().to_path_buf(),
            ));
        }
        if !cfg!(windows) && file_name.to_ascii_lowercase().ends_with(".cmd") {
            return Err(ProviderDiscoveryError::WrongFileType(
                executable.canonical_path().to_path_buf(),
            ));
        }
        Ok(())
    }

    fn validate_origin(
        &self,
        executable: &ProviderExecutable,
        origin: &ProviderDiscoveryOrigin,
    ) -> Result<(), ProviderDiscoveryError> {
        let ProviderDiscoveryOrigin::PathEntry { directory, .. } = origin else {
            return Ok(());
        };
        let Ok(directory) = fs::canonicalize(directory) else {
            return Err(ProviderDiscoveryError::OriginNotAllowed(
                executable.canonical_path().to_path_buf(),
            ));
        };
        if executable.canonical_path().parent() != Some(directory.as_path()) {
            return Err(ProviderDiscoveryError::OriginNotAllowed(
                executable.canonical_path().to_path_buf(),
            ));
        }
        Ok(())
    }

    fn validate_windows_shim(
        &self,
        shim_path: PathBuf,
        target_path: PathBuf,
        origin: ProviderDiscoveryOrigin,
    ) -> Result<ProviderDiscoveryCandidate, ProviderDiscoveryError> {
        #[cfg(not(target_os = "windows"))]
        {
            let _ = (shim_path, target_path, origin);
            return Err(ProviderDiscoveryError::UnsupportedPlatform);
        }

        #[cfg(target_os = "windows")]
        {
            let Some(shim_entrypoint) = self.shim_entrypoint() else {
                return Err(ProviderDiscoveryError::UnsupportedPlatform);
            };
            let shim = ProviderExecutable::from_path(&shim_path)?;
            self.validate_origin(&shim, &origin)?;
            let shim_name = shim
                .canonical_path()
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| ProviderDiscoveryError::WrongEntrypoint(shim_path.clone()))?;
            if !same_entrypoint(shim_name, shim_entrypoint) {
                return Err(ProviderDiscoveryError::WrongEntrypoint(shim_path));
            }
            if !shim_name.to_ascii_lowercase().ends_with(".cmd") {
                return Err(ProviderDiscoveryError::WrongFileType(shim_path));
            }

            let target = ProviderExecutable::from_path(&target_path)?;
            self.validate_native_path(&target)?;
            if shim.canonical_path().parent() != target.canonical_path().parent() {
                return Err(ProviderDiscoveryError::ShimProofInvalid(
                    shim.canonical_path().to_path_buf(),
                ));
            }
            let target_name = target
                .canonical_path()
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| ProviderDiscoveryError::ShimProofInvalid(shim_path.clone()))?;
            let contents = fs::read(shim.canonical_path()).map_err(|_| {
                ProviderDiscoveryError::ShimProofInvalid(shim.canonical_path().to_path_buf())
            })?;
            if contents.len() > MAX_PROVIDER_SHIM_BYTES {
                return Err(ProviderDiscoveryError::ShimProofInvalid(shim_path));
            }
            let contents = std::str::from_utf8(&contents)
                .map_err(|_| ProviderDiscoveryError::ShimProofInvalid(shim_path.clone()))?;
            let expected_crlf = format!("@echo off\r\ncall \"%~dp0{target_name}\" %*\r\n");
            let expected_lf = expected_crlf.replace("\r\n", "\n");
            if contents != expected_crlf && contents != expected_lf {
                return Err(ProviderDiscoveryError::ShimProofInvalid(shim_path));
            }
            Ok(ProviderDiscoveryCandidate {
                origin,
                requested_path: shim_path,
                executable: shim,
                form: ProviderExecutableForm::WindowsShim {
                    target: Box::new(target),
                },
            })
        }
    }
}

fn same_entrypoint(left: &str, right: &str) -> bool {
    if cfg!(windows) {
        left.eq_ignore_ascii_case(right)
    } else {
        left == right
    }
}

fn is_forbidden_runner_name(name: &str) -> bool {
    [
        "node",
        "node.exe",
        "npx",
        "npx.exe",
        "npm",
        "npm.cmd",
        "pnpm",
        "pnpm.cmd",
        "yarn",
        "yarn.cmd",
        "cmd",
        "cmd.exe",
        "powershell",
        "powershell.exe",
        "pwsh",
        "pwsh.exe",
        "sh",
        "sh.exe",
        "bash",
        "bash.exe",
        "cursor",
        "cursor.exe",
        "cursor.cmd",
    ]
    .iter()
    .any(|forbidden| same_entrypoint(forbidden, name))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProviderCapabilities {
    pub kind: ProviderKind,
    pub version: ProviderVersion,
    pub auth_state: ProviderAuthState,
    pub exact_resume: CapabilitySupport,
    pub semantic_events: CapabilitySupport,
    pub provider_session_id: CapabilitySupport,
    #[serde(default)]
    pub build_launch: CapabilitySupport,
    #[serde(default)]
    pub parse_signal: CapabilitySupport,
    #[serde(default)]
    pub cooperative_stop: CapabilitySupport,
    #[serde(default)]
    pub observe_quota: CapabilitySupport,
    pub evidence: Vec<CapabilityEvidence>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderCapabilitiesError {
    TooManyEvidenceItems,
    InvalidEvidence(CapabilityEvidenceError),
    MissingAuthStatusEvidence(ProviderAuthState),
    MismatchedAuthStatusEvidence {
        state: ProviderAuthState,
        evidence: EvidenceStatus,
    },
}

impl fmt::Display for ProviderCapabilitiesError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooManyEvidenceItems => write!(
                f,
                "provider capabilities contain more than {MAX_CAPABILITY_EVIDENCE_ITEMS} evidence items"
            ),
            Self::InvalidEvidence(error) => error.fmt(f),
            Self::MissingAuthStatusEvidence(state) => write!(
                f,
                "provider auth state {state:?} requires matching AuthStatusProbe evidence"
            ),
            Self::MismatchedAuthStatusEvidence { state, evidence } => write!(
                f,
                "provider auth state {state:?} does not match AuthStatusProbe evidence {evidence:?}"
            ),
        }
    }
}

impl std::error::Error for ProviderCapabilitiesError {}

impl<'de> Deserialize<'de> for ProviderCapabilities {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            kind: ProviderKind,
            version: ProviderVersion,
            auth_state: ProviderAuthState,
            exact_resume: CapabilitySupport,
            semantic_events: CapabilitySupport,
            provider_session_id: CapabilitySupport,
            #[serde(default)]
            build_launch: CapabilitySupport,
            #[serde(default)]
            parse_signal: CapabilitySupport,
            #[serde(default)]
            cooperative_stop: CapabilitySupport,
            #[serde(default)]
            observe_quota: CapabilitySupport,
            evidence: Vec<CapabilityEvidence>,
        }

        let wire = Wire::deserialize(deserializer)?;
        let capabilities = Self {
            kind: wire.kind,
            version: wire.version,
            auth_state: wire.auth_state,
            exact_resume: wire.exact_resume,
            semantic_events: wire.semantic_events,
            provider_session_id: wire.provider_session_id,
            build_launch: wire.build_launch,
            parse_signal: wire.parse_signal,
            cooperative_stop: wire.cooperative_stop,
            observe_quota: wire.observe_quota,
            evidence: wire.evidence,
        };
        capabilities.validate().map_err(de::Error::custom)?;
        Ok(capabilities)
    }
}

impl ProviderCapabilities {
    pub fn validate(&self) -> Result<(), ProviderCapabilitiesError> {
        if self.evidence.len() > MAX_CAPABILITY_EVIDENCE_ITEMS {
            return Err(ProviderCapabilitiesError::TooManyEvidenceItems);
        }
        for evidence in &self.evidence {
            evidence
                .validate()
                .map_err(ProviderCapabilitiesError::InvalidEvidence)?;
        }
        let auth_evidence = self
            .evidence
            .iter()
            .filter(|evidence| evidence.source() == EvidenceSourceId::AuthStatusProbe)
            .collect::<Vec<_>>();
        let expected = match self.auth_state {
            ProviderAuthState::AuthenticatedSubscription => Some(EvidenceStatus::Authenticated),
            ProviderAuthState::AuthRequired => Some(EvidenceStatus::AuthRequired),
            ProviderAuthState::Unknown => None,
        };
        if let Some(expected) = expected {
            if auth_evidence.is_empty() {
                return Err(ProviderCapabilitiesError::MissingAuthStatusEvidence(
                    self.auth_state,
                ));
            }
            if auth_evidence
                .iter()
                .any(|evidence| evidence.status() != expected)
            {
                let evidence = auth_evidence[0].status();
                return Err(ProviderCapabilitiesError::MismatchedAuthStatusEvidence {
                    state: self.auth_state,
                    evidence,
                });
            }
        } else if let Some(evidence) = auth_evidence.first() {
            if !matches!(evidence.status(), EvidenceStatus::Unknown) {
                return Err(ProviderCapabilitiesError::MismatchedAuthStatusEvidence {
                    state: self.auth_state,
                    evidence: evidence.status(),
                });
            }
        }
        Ok(())
    }

    /// Return the cacheable capability projection. Authentication is a
    /// registry-owned, monotonic receipt and is never part of this value.
    pub fn stable_projection(&self) -> Self {
        let mut stable = self.clone();
        stable.auth_state = ProviderAuthState::Unknown;
        stable
            .evidence
            .retain(|evidence| evidence.source() != EvidenceSourceId::AuthStatusProbe);
        stable
    }

    pub(crate) fn without_auth_status(&self) -> Self {
        self.stable_projection()
    }

    pub const fn auth_state(&self) -> ProviderAuthState {
        self.auth_state
    }

    pub fn evidence(&self) -> &[CapabilityEvidence] {
        &self.evidence
    }

    pub(crate) fn with_fresh_auth_status(
        &self,
        fresh: &Self,
    ) -> Result<Self, ProviderCapabilitiesError> {
        let mut merged = self.clone();
        merged.auth_state = fresh.auth_state;
        merged
            .evidence
            .retain(|evidence| evidence.source() != EvidenceSourceId::AuthStatusProbe);
        merged.evidence.extend(
            fresh
                .evidence
                .iter()
                .filter(|evidence| evidence.source() == EvidenceSourceId::AuthStatusProbe)
                .cloned(),
        );
        merged.validate()?;
        Ok(merged)
    }

    pub(crate) fn auth_status_observed_at(&self) -> Option<u64> {
        self.evidence
            .iter()
            .filter(|evidence| evidence.source() == EvidenceSourceId::AuthStatusProbe)
            .map(CapabilityEvidence::observed_at)
            .max()
    }

    pub const fn support_for(&self, capability: ProviderCapability) -> CapabilitySupport {
        match capability {
            ProviderCapability::ExactResume => self.exact_resume,
            ProviderCapability::SemanticEvents => self.semantic_events,
            ProviderCapability::ProviderSessionId => self.provider_session_id,
            ProviderCapability::BuildLaunch => self.build_launch,
            ProviderCapability::ParseSignal => self.parse_signal,
            ProviderCapability::CooperativeStop => self.cooperative_stop,
            ProviderCapability::ObserveQuota => self.observe_quota,
        }
    }
}
