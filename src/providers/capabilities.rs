use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

pub const MAX_PROVIDER_VERSION_BYTES: usize = 128;
pub const MAX_CAPABILITY_EVIDENCE_ITEMS: usize = 16;
pub const MAX_EXECUTABLE_ENTRYPOINT_BYTES: usize = 128;

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
    Io { path: PathBuf, kind: io::ErrorKind },
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

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct ProviderExecutable {
    canonical_path: PathBuf,
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
        Ok(Self {
            canonical_path: canonical_path.into(),
            sha256,
        })
    }

    pub(crate) fn inspect_blocking(path: &Path) -> Result<Self, ProviderExecutableError> {
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
        let metadata =
            fs::metadata(&canonical_path).map_err(|error| ProviderExecutableError::Io {
                path: canonical_path.clone(),
                kind: error.kind(),
            })?;
        if !metadata.is_file() {
            return Err(ProviderExecutableError::NotAFile(canonical_path));
        }

        let mut file =
            File::open(&canonical_path).map_err(|error| ProviderExecutableError::Io {
                path: canonical_path.clone(),
                kind: error.kind(),
            })?;
        let mut hasher = Sha256::new();
        let mut buffer = [0u8; 16 * 1024];
        loop {
            let read = file
                .read(&mut buffer)
                .map_err(|error| ProviderExecutableError::Io {
                    path: canonical_path.clone(),
                    kind: error.kind(),
                })?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }

        Self::new(canonical_path, hasher.finalize().into())
    }

    pub fn canonical_path(&self) -> &Path {
        &self.canonical_path
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
            sha256: [u8; 32],
        }

        let wire = Wire::deserialize(deserializer)?;
        Self::new(wire.canonical_path, wire.sha256).map_err(de::Error::custom)
    }
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

    pub(crate) fn without_auth_status(&self) -> Self {
        let mut stable = self.clone();
        stable.auth_state = ProviderAuthState::Unknown;
        stable
            .evidence
            .retain(|evidence| evidence.source() != EvidenceSourceId::AuthStatusProbe);
        stable
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
