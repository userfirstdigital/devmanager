use serde::de::{self, Deserializer, SeqAccess, Visitor};
use serde::ser::SerializeStruct;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use std::ffi::OsStr;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::hash::{Hash, Hasher};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const MAX_PROVIDER_VERSION_BYTES: usize = 128;
pub const MAX_CAPABILITY_EVIDENCE_ITEMS: usize = 16;
pub const MAX_EXECUTABLE_ENTRYPOINT_BYTES: usize = 128;
pub const MAX_PROVIDER_PATH_BYTES: usize = 4096;
pub const PROVIDER_AUTH_NONCE_BYTES: usize = 32;
pub const MAX_PROVIDER_SHIM_BYTES: usize = 16 * 1024;
pub const MAX_PROVIDER_PATH_ENTRIES: usize = 256;
pub const MAX_PROVIDER_PATH_VALUE_BYTES: usize = 32 * 1024;
pub const MAX_PROVIDER_CAPABILITY_CACHE_ENTRIES: usize = 64;
pub const MAX_PROVIDER_AUTH_PENDING_ENTRIES: usize = 64;
pub const MAX_PROVIDER_AUTH_ACCEPTED_ENTRIES: usize = 64;
pub const PROVIDER_CAPABILITY_SCHEMA_VERSION: u16 = 1;
pub const PROVIDER_EVIDENCE_SCHEMA_VERSION: u16 = 1;
pub const PROVIDER_EXECUTABLE_SCHEMA_VERSION: u16 = 1;
pub const PROVIDER_FILE_IDENTITY_SCHEMA_VERSION: u16 = 1;
pub const PROVIDER_OBSERVATION_SCHEMA_VERSION: u16 = 1;
pub const PROVIDER_CACHE_KEY_SCHEMA_VERSION: u16 = 1;

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
        struct ProviderVersionVisitor;

        impl<'de> Visitor<'de> for ProviderVersionVisitor {
            type Value = ProviderVersion;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a bounded provider version string")
            }

            fn visit_borrowed_str<E>(self, value: &'de str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                self.visit_str(value)
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                if value.len() > MAX_PROVIDER_VERSION_BYTES {
                    return Err(E::custom(ProviderVersionError::TooLong));
                }
                ProviderVersion::new(value.to_owned()).map_err(E::custom)
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                if value.len() > MAX_PROVIDER_VERSION_BYTES {
                    return Err(E::custom(ProviderVersionError::TooLong));
                }
                ProviderVersion::new(value).map_err(E::custom)
            }
        }

        deserializer.deserialize_string(ProviderVersionVisitor)
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
pub enum EvidenceConfidence {
    High,
    Medium,
    Low,
    Unknown,
}

impl Default for EvidenceConfidence {
    fn default() -> Self {
        Self::Unknown
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderAuthEvidenceSource {
    ClaudeCodeSubscriptionLogin,
    CodexSubscriptionLogin,
    CursorSubscriptionLogin,
}

impl ProviderAuthEvidenceSource {
    pub const fn for_kind(kind: ProviderKind) -> Self {
        match kind {
            ProviderKind::ClaudeCode => Self::ClaudeCodeSubscriptionLogin,
            ProviderKind::Codex => Self::CodexSubscriptionLogin,
            ProviderKind::Cursor => Self::CursorSubscriptionLogin,
        }
    }

    pub const fn provider_kind(self) -> ProviderKind {
        match self {
            Self::ClaudeCodeSubscriptionLogin => ProviderKind::ClaudeCode,
            Self::CodexSubscriptionLogin => ProviderKind::Codex,
            Self::CursorSubscriptionLogin => ProviderKind::Cursor,
        }
    }
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

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceDiagnostic {
    code: EvidenceDiagnosticCode,
    digest: Option<[u8; 32]>,
}

impl fmt::Debug for EvidenceDiagnostic {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EvidenceDiagnostic")
            .field("code", &self.code)
            .field("digest_present", &self.digest.is_some())
            .finish()
    }
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
    ExpiryNotAfterObserved,
    AuthEvidenceRequiresReceipt,
    UnsupportedSchemaVersion(u16),
}

impl fmt::Display for CapabilityEvidenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ObservedAtZero => write!(f, "capability evidence observed_at must be non-zero"),
            Self::ExpiryNotAfterObserved => {
                write!(f, "capability evidence expiry must follow observed_at")
            }
            Self::AuthEvidenceRequiresReceipt => {
                write!(
                    f,
                    "subscription authentication evidence requires a registry receipt"
                )
            }
            Self::UnsupportedSchemaVersion(version) => {
                write!(
                    f,
                    "unsupported capability evidence schema version {version}"
                )
            }
        }
    }
}

impl std::error::Error for CapabilityEvidenceError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityEvidence {
    source: EvidenceSourceId,
    observed_at: u64,
    status: EvidenceStatus,
    diagnostic: Option<EvidenceDiagnostic>,
    auth_source: Option<ProviderAuthEvidenceSource>,
    expires_at: Option<u64>,
    confidence: EvidenceConfidence,
    auth_authority: bool,
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
            auth_source: None,
            expires_at: None,
            confidence: EvidenceConfidence::Unknown,
            auth_authority: false,
        };
        evidence.validate()?;
        Ok(evidence)
    }

    fn new_with_lifecycle(
        source: EvidenceSourceId,
        observed_at: u64,
        expires_at: Option<u64>,
        confidence: EvidenceConfidence,
        auth_source: Option<ProviderAuthEvidenceSource>,
        status: EvidenceStatus,
        diagnostic: Option<EvidenceDiagnostic>,
    ) -> Result<Self, CapabilityEvidenceError> {
        let evidence = Self {
            source,
            observed_at,
            status,
            diagnostic,
            auth_source,
            expires_at,
            confidence,
            auth_authority: false,
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

    pub const fn auth_source(&self) -> Option<ProviderAuthEvidenceSource> {
        self.auth_source
    }

    pub const fn expires_at(&self) -> Option<u64> {
        self.expires_at
    }

    pub const fn confidence(&self) -> EvidenceConfidence {
        self.confidence
    }

    pub fn validate(&self) -> Result<(), CapabilityEvidenceError> {
        if self.observed_at == 0 {
            return Err(CapabilityEvidenceError::ObservedAtZero);
        }
        if self
            .expires_at
            .is_some_and(|expires_at| expires_at <= self.observed_at)
        {
            return Err(CapabilityEvidenceError::ExpiryNotAfterObserved);
        }
        let is_authentication_evidence = self.source == EvidenceSourceId::AuthStatusProbe
            || self.auth_source.is_some()
            || matches!(
                self.status,
                EvidenceStatus::Authenticated | EvidenceStatus::AuthRequired
            );
        if is_authentication_evidence && !self.is_registry_authorized_auth() {
            return Err(CapabilityEvidenceError::AuthEvidenceRequiresReceipt);
        }
        Ok(())
    }

    fn from_auth_receipt(receipt: &ProviderAuthEvidenceReceipt) -> Self {
        let status = match receipt.result {
            ProviderAuthProbeResult::AuthenticatedSubscription => EvidenceStatus::Authenticated,
            ProviderAuthProbeResult::AuthRequired => EvidenceStatus::AuthRequired,
            ProviderAuthProbeResult::ApiKeyDetected | ProviderAuthProbeResult::Unknown => {
                EvidenceStatus::Unknown
            }
        };
        let observed_at = receipt.generation;
        Self {
            source: EvidenceSourceId::AuthStatusProbe,
            observed_at,
            status,
            diagnostic: None,
            auth_source: Some(receipt.source),
            expires_at: Some(observed_at.saturating_add(1)),
            confidence: receipt.confidence,
            auth_authority: true,
        }
    }

    fn is_registry_authorized_auth(&self) -> bool {
        self.auth_authority
    }
}

impl Serialize for CapabilityEvidence {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("CapabilityEvidence", 8)?;
        state.serialize_field("schema_version", &PROVIDER_EVIDENCE_SCHEMA_VERSION)?;
        state.serialize_field("source", &self.source)?;
        state.serialize_field("observed_at", &self.observed_at)?;
        state.serialize_field("expires_at", &self.expires_at)?;
        state.serialize_field("confidence", &self.confidence)?;
        state.serialize_field("auth_source", &self.auth_source)?;
        state.serialize_field("status", &self.status)?;
        state.serialize_field("diagnostic", &self.diagnostic)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for CapabilityEvidence {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            schema_version: u16,
            source: EvidenceSourceId,
            observed_at: u64,
            expires_at: Option<u64>,
            confidence: EvidenceConfidence,
            auth_source: Option<ProviderAuthEvidenceSource>,
            status: EvidenceStatus,
            diagnostic: Option<EvidenceDiagnostic>,
        }

        let wire = Wire::deserialize(deserializer)?;
        if wire.schema_version != PROVIDER_EVIDENCE_SCHEMA_VERSION {
            return Err(de::Error::custom(
                CapabilityEvidenceError::UnsupportedSchemaVersion(wire.schema_version),
            ));
        }
        Self::new_with_lifecycle(
            wire.source,
            wire.observed_at,
            wire.expires_at,
            wire.confidence,
            wire.auth_source,
            wire.status,
            wire.diagnostic,
        )
        .map_err(de::Error::custom)
    }
}

#[derive(Clone, PartialEq, Eq)]
pub enum ProviderExecutableError {
    EmptyPath,
    PathTooLong,
    Missing(PathBuf),
    NotAFile(PathBuf),
    NotNativeExecutable(PathBuf),
    SymlinkOrReparse(PathBuf),
    HardlinkAmbiguous(PathBuf),
    ChangedDuringValidation(PathBuf),
    InvalidFileIdentity(PathBuf),
    UnsupportedPlatform(PathBuf),
    NotCanonical {
        requested: PathBuf,
        canonical: PathBuf,
    },
    HashMismatch(PathBuf),
    UnsupportedSchemaVersion(u16),
    Io {
        path: PathBuf,
        kind: io::ErrorKind,
    },
    BackgroundTask,
}

impl fmt::Debug for ProviderExecutableError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let code = match self {
            Self::EmptyPath => "empty_path",
            Self::PathTooLong => "path_too_long",
            Self::Missing(_) => "missing",
            Self::NotAFile(_) => "not_a_file",
            Self::NotNativeExecutable(_) => "not_native_executable",
            Self::SymlinkOrReparse(_) => "symlink_or_reparse",
            Self::HardlinkAmbiguous(_) => "hardlink_ambiguous",
            Self::ChangedDuringValidation(_) => "changed_during_validation",
            Self::InvalidFileIdentity(_) => "invalid_file_identity",
            Self::UnsupportedPlatform(_) => "unsupported_platform",
            Self::NotCanonical { .. } => "not_canonical",
            Self::HashMismatch(_) => "hash_mismatch",
            Self::UnsupportedSchemaVersion(_) => "unsupported_schema_version",
            Self::Io { .. } => "io",
            Self::BackgroundTask => "background_task",
        };
        formatter
            .debug_struct("ProviderExecutableError")
            .field("code", &code)
            .finish()
    }
}

impl fmt::Display for ProviderExecutableError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyPath => write!(f, "provider executable path must be non-empty"),
            Self::PathTooLong => write!(f, "provider executable path is too long"),
            Self::Missing(_) => write!(f, "provider executable is missing"),
            Self::NotAFile(_) => write!(f, "provider executable is not a file"),
            Self::NotNativeExecutable(_) => {
                write!(f, "provider executable is not a runnable native binary")
            }
            Self::SymlinkOrReparse(_) => {
                write!(
                    f,
                    "provider executable must not be a symlink or reparse point"
                )
            }
            Self::HardlinkAmbiguous(_) => {
                write!(f, "provider executable has ambiguous hardlink identity")
            }
            Self::ChangedDuringValidation(_) => {
                write!(f, "provider executable changed during validation")
            }
            Self::InvalidFileIdentity(_) => {
                write!(f, "provider executable has invalid file identity")
            }
            Self::UnsupportedPlatform(_) => {
                write!(
                    f,
                    "provider executable identity cannot be proven on this platform"
                )
            }
            Self::NotCanonical { .. } => write!(f, "provider executable path is not canonical"),
            Self::HashMismatch(_) => write!(f, "provider executable hash does not match"),
            Self::UnsupportedSchemaVersion(version) => {
                write!(
                    f,
                    "unsupported provider executable schema version {version}"
                )
            }
            Self::Io { kind, .. } => {
                write!(f, "could not inspect provider executable ({kind:?})")
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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

impl Serialize for ProviderFileIdentity {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Windows {
                volume_serial,
                file_index,
                link_count,
            } => {
                let mut state = serializer.serialize_struct("ProviderFileIdentity", 5)?;
                state.serialize_field("schema_version", &PROVIDER_FILE_IDENTITY_SCHEMA_VERSION)?;
                state.serialize_field("platform", "windows")?;
                state.serialize_field("volume_serial", volume_serial)?;
                state.serialize_field("file_index", file_index)?;
                state.serialize_field("link_count", link_count)?;
                state.end()
            }
            Self::Unix {
                device,
                inode,
                link_count,
            } => {
                let mut state = serializer.serialize_struct("ProviderFileIdentity", 5)?;
                state.serialize_field("schema_version", &PROVIDER_FILE_IDENTITY_SCHEMA_VERSION)?;
                state.serialize_field("platform", "unix")?;
                state.serialize_field("device", device)?;
                state.serialize_field("inode", inode)?;
                state.serialize_field("link_count", link_count)?;
                state.end()
            }
            Self::Other {
                stable_id,
                link_count,
            } => {
                let mut state = serializer.serialize_struct("ProviderFileIdentity", 4)?;
                state.serialize_field("schema_version", &PROVIDER_FILE_IDENTITY_SCHEMA_VERSION)?;
                state.serialize_field("platform", "other")?;
                state.serialize_field("stable_id", stable_id)?;
                state.serialize_field("link_count", link_count)?;
                state.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for ProviderFileIdentity {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(tag = "platform", rename_all = "snake_case", deny_unknown_fields)]
        enum Wire {
            Windows {
                schema_version: u16,
                volume_serial: u32,
                file_index: [u8; 16],
                link_count: u32,
            },
            Unix {
                schema_version: u16,
                device: u64,
                inode: u64,
                link_count: u64,
            },
            Other {
                schema_version: u16,
                stable_id: [u8; 16],
                link_count: u64,
            },
        }

        let wire = Wire::deserialize(deserializer)?;
        match wire {
            Wire::Windows {
                schema_version,
                volume_serial,
                file_index,
                link_count,
            } if schema_version == PROVIDER_FILE_IDENTITY_SCHEMA_VERSION => Ok(Self::Windows {
                volume_serial,
                file_index,
                link_count,
            }),
            Wire::Unix {
                schema_version,
                device,
                inode,
                link_count,
            } if schema_version == PROVIDER_FILE_IDENTITY_SCHEMA_VERSION => Ok(Self::Unix {
                device,
                inode,
                link_count,
            }),
            Wire::Other {
                schema_version,
                stable_id,
                link_count,
            } if schema_version == PROVIDER_FILE_IDENTITY_SCHEMA_VERSION => Ok(Self::Other {
                stable_id,
                link_count,
            }),
            Wire::Windows { schema_version, .. }
            | Wire::Unix { schema_version, .. }
            | Wire::Other { schema_version, .. } => Err(de::Error::custom(
                ProviderExecutableError::UnsupportedSchemaVersion(schema_version),
            )),
        }
    }
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
        if matches!(self, Self::Other { .. }) {
            return Err(ProviderExecutableError::UnsupportedPlatform(
                path.to_path_buf(),
            ));
        }
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

    pub const fn default_confidence(self) -> EvidenceConfidence {
        match self {
            Self::AuthenticatedSubscription | Self::AuthRequired => EvidenceConfidence::High,
            Self::ApiKeyDetected => EvidenceConfidence::Low,
            Self::Unknown => EvidenceConfidence::Unknown,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderAuthEvidenceError {
    InvalidDeadline,
    NonceGenerationFailed,
    UnknownInvocation,
    WrongProvider,
    WrongAuthSource,
    WrongExecutable,
    Expired,
    FutureTimestamp,
    Reordered,
    NonMonotonicTimestamp,
    AlreadyConsumed,
    ExecutableChanged(ProviderExecutableError),
}

impl fmt::Display for ProviderAuthEvidenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidDeadline => write!(f, "auth probe deadline must follow issuance"),
            Self::NonceGenerationFailed => write!(f, "could not issue auth probe nonce"),
            Self::UnknownInvocation => write!(f, "auth evidence invocation was not issued"),
            Self::WrongProvider => write!(f, "auth evidence provider does not match invocation"),
            Self::WrongAuthSource => write!(f, "auth evidence source does not match provider"),
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
            Self::AlreadyConsumed => write!(f, "auth evidence receipt was already consumed"),
            Self::ExecutableChanged(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for ProviderAuthEvidenceError {}

#[derive(Clone, PartialEq, Eq, Hash)]
struct ProviderAuthInvocationKey {
    kind: ProviderKind,
    executable: ProviderExecutable,
    nonce: [u8; PROVIDER_AUTH_NONCE_BYTES],
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct ProviderAuthReceiptKey {
    kind: ProviderKind,
    executable: ProviderExecutable,
    nonce: [u8; PROVIDER_AUTH_NONCE_BYTES],
    generation: u64,
}

#[derive(Clone)]
pub struct ProviderAuthProbeInvocation {
    kind: ProviderKind,
    source: ProviderAuthEvidenceSource,
    executable: ProviderExecutable,
    nonce: [u8; PROVIDER_AUTH_NONCE_BYTES],
    generation: u64,
    issued_at: Instant,
    deadline: Instant,
}

impl fmt::Debug for ProviderAuthProbeInvocation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderAuthProbeInvocation")
            .field("kind", &self.kind)
            .field("source", &self.source)
            .field("executable", &self.executable)
            .field("nonce", &"<redacted>")
            .field("generation", &self.generation)
            .field("issued_at", &self.issued_at)
            .field("deadline", &self.deadline)
            .finish()
    }
}

impl ProviderAuthProbeInvocation {
    pub const fn provider_kind(&self) -> ProviderKind {
        self.kind
    }

    pub const fn source(&self) -> ProviderAuthEvidenceSource {
        self.source
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

#[derive(Clone, PartialEq, Eq)]
pub struct ProviderAuthEvidenceReceipt {
    kind: ProviderKind,
    source: ProviderAuthEvidenceSource,
    executable: ProviderExecutable,
    nonce: [u8; PROVIDER_AUTH_NONCE_BYTES],
    generation: u64,
    result: ProviderAuthProbeResult,
    observed_at: Instant,
    deadline: Instant,
    confidence: EvidenceConfidence,
}

impl fmt::Debug for ProviderAuthEvidenceReceipt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderAuthEvidenceReceipt")
            .field("kind", &self.kind)
            .field("source", &self.source)
            .field("executable", &self.executable)
            .field("nonce", &"<redacted>")
            .field("generation", &self.generation)
            .field("result", &self.result)
            .field("observed_at", &self.observed_at)
            .field("deadline", &self.deadline)
            .field("confidence", &self.confidence)
            .finish()
    }
}

impl ProviderAuthEvidenceReceipt {
    pub const fn provider_kind(&self) -> ProviderKind {
        self.kind
    }

    pub const fn source(&self) -> ProviderAuthEvidenceSource {
        self.source
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

    pub const fn expires_at(&self) -> Instant {
        self.deadline
    }

    pub const fn confidence(&self) -> EvidenceConfidence {
        self.confidence
    }

    pub const fn is_authenticated_subscription(&self) -> bool {
        self.result.is_authenticated_subscription()
    }

    pub fn is_fresh_at(&self, now: Instant) -> bool {
        now >= self.observed_at && now <= self.deadline
    }

    fn key(&self) -> ProviderAuthReceiptKey {
        ProviderAuthReceiptKey {
            kind: self.kind,
            executable: self.executable.clone(),
            nonce: self.nonce,
            generation: self.generation,
        }
    }
}

#[derive(Default)]
pub struct ProviderAuthEvidenceRegistry {
    next_generation: u64,
    pending: HashMap<ProviderAuthInvocationKey, ProviderAuthProbeInvocation>,
    pending_order: VecDeque<ProviderAuthInvocationKey>,
    accepted: HashMap<ProviderAuthReceiptKey, (ProviderAuthEvidenceReceipt, bool)>,
    accepted_order: VecDeque<ProviderAuthReceiptKey>,
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
        self.begin_with_source(
            kind,
            ProviderAuthEvidenceSource::for_kind(kind),
            executable,
            ttl,
        )
    }

    pub fn begin_with_source(
        &mut self,
        kind: ProviderKind,
        source: ProviderAuthEvidenceSource,
        executable: ProviderExecutable,
        ttl: Duration,
    ) -> Result<ProviderAuthProbeInvocation, ProviderAuthEvidenceError> {
        let issued_at = Instant::now();
        let deadline = issued_at
            .checked_add(ttl)
            .ok_or(ProviderAuthEvidenceError::InvalidDeadline)?;
        self.begin_at_with_source(kind, source, executable, issued_at, deadline)
    }

    pub fn begin_at(
        &mut self,
        kind: ProviderKind,
        executable: ProviderExecutable,
        issued_at: Instant,
        deadline: Instant,
    ) -> Result<ProviderAuthProbeInvocation, ProviderAuthEvidenceError> {
        self.begin_at_with_source(
            kind,
            ProviderAuthEvidenceSource::for_kind(kind),
            executable,
            issued_at,
            deadline,
        )
    }

    pub fn begin_at_with_source(
        &mut self,
        kind: ProviderKind,
        source: ProviderAuthEvidenceSource,
        executable: ProviderExecutable,
        issued_at: Instant,
        deadline: Instant,
    ) -> Result<ProviderAuthProbeInvocation, ProviderAuthEvidenceError> {
        if source.provider_kind() != kind {
            return Err(ProviderAuthEvidenceError::WrongAuthSource);
        }
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
            source,
            executable,
            nonce,
            generation: self.next_generation,
            issued_at,
            deadline,
        };
        let key = invocation.key();
        self.evict_oldest_pending();
        self.pending.insert(key.clone(), invocation.clone());
        self.pending_order.push_back(key);
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
        self.accept_at_for_with_confidence(
            expected_kind,
            expected_executable,
            invocation,
            result,
            observed_at,
            result.default_confidence(),
        )
    }

    pub(crate) fn accept_at_for_with_confidence(
        &mut self,
        expected_kind: ProviderKind,
        expected_executable: &ProviderExecutable,
        invocation: ProviderAuthProbeInvocation,
        result: ProviderAuthProbeResult,
        observed_at: Instant,
        confidence: EvidenceConfidence,
    ) -> Result<ProviderAuthEvidenceReceipt, ProviderAuthEvidenceError> {
        if invocation.kind != expected_kind {
            return Err(ProviderAuthEvidenceError::WrongProvider);
        }
        if invocation.source.provider_kind() != expected_kind {
            return Err(ProviderAuthEvidenceError::WrongAuthSource);
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
        let now = Instant::now();
        if observed_at > now {
            return Err(ProviderAuthEvidenceError::FutureTimestamp);
        }
        if observed_at < invocation.issued_at || observed_at > invocation.deadline {
            self.remove_pending(&key);
            return Err(ProviderAuthEvidenceError::Expired);
        }
        self.remove_pending(&key);

        let identity_key = (invocation.kind, invocation.executable.clone());
        if let Some((last_generation, last_observed_at)) = self.last_accepted.get(&identity_key) {
            if invocation.generation <= *last_generation {
                return Err(ProviderAuthEvidenceError::Reordered);
            }
            if observed_at <= *last_observed_at {
                return Err(ProviderAuthEvidenceError::NonMonotonicTimestamp);
            }
        }
        let receipt = ProviderAuthEvidenceReceipt {
            kind: invocation.kind,
            source: invocation.source,
            executable: invocation.executable,
            nonce: invocation.nonce,
            generation: invocation.generation,
            result,
            observed_at,
            deadline: invocation.deadline,
            confidence,
        };
        self.last_accepted
            .insert(identity_key, (invocation.generation, observed_at));
        self.evict_oldest_accepted();
        let receipt_key = receipt.key();
        self.accepted_order.push_back(receipt_key.clone());
        self.accepted.insert(receipt_key, (receipt.clone(), false));
        Ok(receipt)
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

    pub fn consume_at_for(
        &mut self,
        expected_kind: ProviderKind,
        expected_executable: &ProviderExecutable,
        receipt: ProviderAuthEvidenceReceipt,
        now: Instant,
    ) -> Result<ProviderAuthEvidenceReceipt, ProviderAuthEvidenceError> {
        if receipt.kind != expected_kind {
            return Err(ProviderAuthEvidenceError::WrongProvider);
        }
        if receipt.source.provider_kind() != expected_kind {
            return Err(ProviderAuthEvidenceError::WrongAuthSource);
        }
        if &receipt.executable != expected_executable {
            return Err(ProviderAuthEvidenceError::WrongExecutable);
        }
        expected_executable
            .validate_current()
            .map_err(ProviderAuthEvidenceError::ExecutableChanged)?;
        if !receipt.is_fresh_at(now) {
            return Err(ProviderAuthEvidenceError::Expired);
        }
        if self
            .last_accepted
            .get(&(receipt.kind, receipt.executable.clone()))
            .map_or(true, |(generation, _)| *generation != receipt.generation)
        {
            return Err(ProviderAuthEvidenceError::Reordered);
        }
        let key = receipt.key();
        let Some((stored, consumed)) = self.accepted.get_mut(&key) else {
            return Err(ProviderAuthEvidenceError::UnknownInvocation);
        };
        if *consumed {
            return Err(ProviderAuthEvidenceError::AlreadyConsumed);
        }
        if stored != &receipt {
            return Err(ProviderAuthEvidenceError::UnknownInvocation);
        }
        *consumed = true;
        Ok(receipt)
    }

    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    pub fn accepted_len(&self) -> usize {
        self.accepted.len()
    }

    fn evict_oldest_pending(&mut self) {
        while self.pending.len() >= MAX_PROVIDER_AUTH_PENDING_ENTRIES {
            let Some(key) = self.pending_order.pop_front() else {
                break;
            };
            self.pending.remove(&key);
        }
    }

    fn remove_pending(&mut self, key: &ProviderAuthInvocationKey) {
        self.pending.remove(key);
        if let Some(position) = self.pending_order.iter().position(|current| current == key) {
            self.pending_order.remove(position);
        }
    }

    fn evict_oldest_accepted(&mut self) {
        while self.accepted.len() >= MAX_PROVIDER_AUTH_ACCEPTED_ENTRIES {
            let Some(key) = self.accepted_order.pop_front() else {
                break;
            };
            if let Some((receipt, _)) = self.accepted.remove(&key) {
                let identity_key = (receipt.kind, receipt.executable.clone());
                if self
                    .last_accepted
                    .get(&identity_key)
                    .is_some_and(|(generation, _)| *generation == receipt.generation)
                {
                    self.last_accepted.remove(&identity_key);
                }
            }
        }
    }
}

#[derive(Clone)]
pub struct ProviderExecutable {
    canonical_path: PathBuf,
    file_identity: ProviderFileIdentity,
    sha256: [u8; 32],
    is_native: bool,
    handle: Arc<Mutex<File>>,
}

impl fmt::Debug for ProviderExecutable {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderExecutable")
            .field("identity_bound", &true)
            .field("is_native", &self.is_native)
            .finish()
    }
}

impl PartialEq for ProviderExecutable {
    fn eq(&self, other: &Self) -> bool {
        self.canonical_path == other.canonical_path
            && self.file_identity == other.file_identity
            && self.sha256 == other.sha256
            && self.is_native == other.is_native
    }
}

impl Eq for ProviderExecutable {}

impl Hash for ProviderExecutable {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.canonical_path.hash(state);
        self.file_identity.hash(state);
        self.sha256.hash(state);
        self.is_native.hash(state);
    }
}

/// A launch-time capability bound to the same no-follow handle and file
/// identity captured by [`ProviderExecutable`]. The later launcher can
/// consume the handle instead of reopening an attacker-controlled path.
#[derive(Debug, Clone)]
pub struct ProviderExecutableHandle {
    executable: ProviderExecutable,
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
        if path_bytes(&canonical_path) > MAX_PROVIDER_PATH_BYTES {
            return Err(ProviderExecutableError::PathTooLong);
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

    /// Resolve and inspect a native candidate path without trusting its
    /// basename. The resulting identity retains the opened no-follow handle.
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self, ProviderExecutableError> {
        Self::inspect_blocking(path.as_ref())
    }

    /// Prepare a launch handle after revalidating both the current path and
    /// the originally captured file handle.
    pub fn open_for_launch(&self) -> Result<ProviderExecutableHandle, ProviderExecutableError> {
        self.validate_current()?;
        Ok(ProviderExecutableHandle {
            executable: self.clone(),
        })
    }

    /// Re-check the current path, native format, and captured file identity
    /// before using a cached fact.
    pub fn validate_current(&self) -> Result<(), ProviderExecutableError> {
        let current = Self::inspect_blocking_with_mode(&self.canonical_path, self.is_native)?;
        if current != *self || !self.validate_bound_handle()? {
            Err(ProviderExecutableError::ChangedDuringValidation(
                self.canonical_path.clone(),
            ))
        } else {
            Ok(())
        }
    }

    pub(crate) fn inspect_blocking(path: &Path) -> Result<Self, ProviderExecutableError> {
        Self::inspect_blocking_with_mode(path, true)
    }

    pub(crate) fn inspect_non_native_blocking(
        path: &Path,
    ) -> Result<Self, ProviderExecutableError> {
        Self::inspect_blocking_with_mode(path, false)
    }

    fn inspect_blocking_with_mode(
        path: &Path,
        is_native: bool,
    ) -> Result<Self, ProviderExecutableError> {
        if path.as_os_str().is_empty() {
            return Err(ProviderExecutableError::EmptyPath);
        }
        if path_bytes(path) > MAX_PROVIDER_PATH_BYTES {
            return Err(ProviderExecutableError::PathTooLong);
        }
        reject_reparse_components(path)?;
        let initial_file = open_nofollow(path)?;
        let initial_identity = inspect_opened_metadata(&initial_file, path, is_native)?;
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
        if !canonical_path.is_absolute() {
            return Err(ProviderExecutableError::NotCanonical {
                requested: path.to_path_buf(),
                canonical: canonical_path,
            });
        }
        reject_reparse_components(&canonical_path)?;
        let first =
            inspect_opened_file(open_nofollow(&canonical_path)?, &canonical_path, is_native)?;
        if initial_identity != first.identity {
            return Err(ProviderExecutableError::ChangedDuringValidation(
                canonical_path,
            ));
        }
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
        let second =
            inspect_opened_file(open_nofollow(&canonical_path)?, &canonical_path, is_native)?;
        if first.identity != second.identity || first.sha256 != second.sha256 {
            return Err(ProviderExecutableError::ChangedDuringValidation(
                canonical_path.clone(),
            ));
        }

        Ok(Self {
            canonical_path,
            file_identity: second.identity,
            sha256: second.sha256,
            is_native,
            handle: Arc::new(Mutex::new(second.file)),
        })
    }

    fn validate_bound_handle(&self) -> Result<bool, ProviderExecutableError> {
        let file = self
            .handle
            .lock()
            .map_err(|_| ProviderExecutableError::Io {
                path: self.canonical_path.clone(),
                kind: io::ErrorKind::Other,
            })?;
        let identity = inspect_opened_metadata(&file, &self.canonical_path, self.is_native)?;
        Ok(identity == self.file_identity)
    }

    fn clone_file_handle(&self) -> Result<File, ProviderExecutableError> {
        self.handle
            .lock()
            .map_err(|_| ProviderExecutableError::Io {
                path: self.canonical_path.clone(),
                kind: io::ErrorKind::Other,
            })?
            .try_clone()
            .map_err(|error| ProviderExecutableError::Io {
                path: self.canonical_path.clone(),
                kind: error.kind(),
            })
    }

    fn read_handle_contents(&self) -> Result<Vec<u8>, ProviderExecutableError> {
        let mut file = self
            .handle
            .lock()
            .map_err(|_| ProviderExecutableError::Io {
                path: self.canonical_path.clone(),
                kind: io::ErrorKind::Other,
            })?;
        file.seek(SeekFrom::Start(0))
            .map_err(|error| ProviderExecutableError::Io {
                path: self.canonical_path.clone(),
                kind: error.kind(),
            })?;
        let mut contents = Vec::with_capacity(MAX_PROVIDER_SHIM_BYTES + 1);
        let mut bounded = (&mut *file).take((MAX_PROVIDER_SHIM_BYTES + 1) as u64);
        bounded
            .read_to_end(&mut contents)
            .map_err(|error| ProviderExecutableError::Io {
                path: self.canonical_path.clone(),
                kind: error.kind(),
            })?;
        file.seek(SeekFrom::Start(0))
            .map_err(|error| ProviderExecutableError::Io {
                path: self.canonical_path.clone(),
                kind: error.kind(),
            })?;
        Ok(contents)
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

    pub const fn is_native(&self) -> bool {
        self.is_native
    }
}

impl ProviderExecutableHandle {
    pub fn executable(&self) -> &ProviderExecutable {
        &self.executable
    }

    pub fn canonical_path(&self) -> &Path {
        self.executable.canonical_path()
    }

    pub const fn file_identity(&self) -> &ProviderFileIdentity {
        self.executable.file_identity()
    }

    pub fn revalidate(&self) -> Result<(), ProviderExecutableError> {
        self.executable.validate_current()
    }

    pub fn try_clone_file(&self) -> Result<File, ProviderExecutableError> {
        self.executable.clone_file_handle()
    }

    pub fn into_file(self) -> Result<File, ProviderExecutableError> {
        self.revalidate()?;
        self.executable.clone_file_handle()
    }
}

impl Serialize for ProviderExecutable {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("ProviderExecutable", 5)?;
        state.serialize_field("schema_version", &PROVIDER_EXECUTABLE_SCHEMA_VERSION)?;
        state.serialize_field("canonical_path", &self.canonical_path)?;
        state.serialize_field("file_identity", &self.file_identity)?;
        state.serialize_field("sha256", &self.sha256)?;
        state.serialize_field("is_native", &self.is_native)?;
        state.end()
    }
}

struct BoundedPathBuf(PathBuf);

impl<'de> Deserialize<'de> for BoundedPathBuf {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct BoundedPathVisitor;

        impl<'de> Visitor<'de> for BoundedPathVisitor {
            type Value = BoundedPathBuf;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a bounded executable path string")
            }

            fn visit_borrowed_str<E>(self, value: &'de str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                self.visit_str(value)
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                if value.len() > MAX_PROVIDER_PATH_BYTES {
                    return Err(E::custom(ProviderExecutableError::PathTooLong));
                }
                Ok(BoundedPathBuf(PathBuf::from(value)))
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                if value.len() > MAX_PROVIDER_PATH_BYTES {
                    return Err(E::custom(ProviderExecutableError::PathTooLong));
                }
                Ok(BoundedPathBuf(PathBuf::from(value)))
            }
        }

        deserializer
            .deserialize_string(BoundedPathVisitor)
            .map_err(|error| error)
    }
}

impl<'de> Deserialize<'de> for ProviderExecutable {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            schema_version: u16,
            canonical_path: BoundedPathBuf,
            file_identity: ProviderFileIdentity,
            sha256: [u8; 32],
            is_native: bool,
        }

        let wire = Wire::deserialize(deserializer)?;
        if wire.schema_version != PROVIDER_EXECUTABLE_SCHEMA_VERSION {
            return Err(de::Error::custom(
                ProviderExecutableError::UnsupportedSchemaVersion(wire.schema_version),
            ));
        }
        let inspected = Self::inspect_blocking_with_mode(&wire.canonical_path.0, wire.is_native)
            .map_err(de::Error::custom)?;
        if inspected.sha256 != wire.sha256 {
            return Err(de::Error::custom(ProviderExecutableError::HashMismatch(
                wire.canonical_path.0,
            )));
        }
        if inspected.file_identity != wire.file_identity {
            return Err(de::Error::custom(
                ProviderExecutableError::InvalidFileIdentity(wire.canonical_path.0),
            ));
        }
        Ok(inspected)
    }
}

struct OpenedExecutable {
    file: File,
    identity: ProviderFileIdentity,
    sha256: [u8; 32],
}

fn inspect_opened_file(
    mut file: File,
    path: &Path,
    is_native: bool,
) -> Result<OpenedExecutable, ProviderExecutableError> {
    let identity = inspect_opened_metadata(&file, path, is_native)?;
    let sha256 = hash_file(&mut file, path)?;
    let after_identity = file_identity(&file, path)?;
    after_identity.validate(path)?;
    if after_identity != identity {
        return Err(ProviderExecutableError::ChangedDuringValidation(
            path.to_path_buf(),
        ));
    }
    Ok(OpenedExecutable {
        file,
        identity,
        sha256,
    })
}

fn inspect_opened_metadata(
    file: &File,
    path: &Path,
    is_native: bool,
) -> Result<ProviderFileIdentity, ProviderExecutableError> {
    let metadata = file
        .metadata()
        .map_err(|error| ProviderExecutableError::Io {
            path: path.to_path_buf(),
            kind: error.kind(),
        })?;
    if !metadata.is_file() {
        return Err(ProviderExecutableError::NotAFile(path.to_path_buf()));
    }
    let identity = file_identity(file, path)?;
    identity.validate(path)?;
    if is_native {
        let mut file = file
            .try_clone()
            .map_err(|error| ProviderExecutableError::Io {
                path: path.to_path_buf(),
                kind: error.kind(),
            })?;
        validate_native_format(&mut file, path)?;
    }
    Ok(identity)
}

fn open_nofollow(path: &Path) -> Result<File, ProviderExecutableError> {
    let mut options = OpenOptions::new();
    options.read(true);

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0);
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let Some(no_follow) = unix_open_no_follow_flag() else {
            return Err(ProviderExecutableError::UnsupportedPlatform(
                path.to_path_buf(),
            ));
        };
        options.custom_flags(no_follow);
    }

    #[cfg(not(any(target_os = "windows", unix)))]
    {
        return Err(ProviderExecutableError::UnsupportedPlatform(
            path.to_path_buf(),
        ));
    }

    options.open(path).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            ProviderExecutableError::Missing(path.to_path_buf())
        } else {
            ProviderExecutableError::Io {
                path: path.to_path_buf(),
                kind: error.kind(),
            }
        }
    })
}

#[cfg(unix)]
fn unix_open_no_follow_flag() -> Option<i32> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        return Some(0x20000);
    }
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "tvos",
        target_os = "watchos"
    ))]
    {
        return Some(0x0100);
    }
    #[cfg(any(
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    ))]
    {
        return Some(0x0200);
    }
    None
}

fn validate_native_format(file: &mut File, path: &Path) -> Result<(), ProviderExecutableError> {
    file.seek(SeekFrom::Start(0))
        .map_err(|error| ProviderExecutableError::Io {
            path: path.to_path_buf(),
            kind: error.kind(),
        })?;

    #[cfg(target_os = "windows")]
    {
        let mut dos_header = [0_u8; 64];
        file.read_exact(&mut dos_header)
            .map_err(|_| ProviderExecutableError::NotNativeExecutable(path.to_path_buf()))?;
        if &dos_header[..2] != b"MZ" {
            return Err(ProviderExecutableError::NotNativeExecutable(
                path.to_path_buf(),
            ));
        }
        let pe_offset = u32::from_le_bytes(dos_header[60..64].try_into().unwrap()) as u64;
        let length = file
            .metadata()
            .map_err(|error| ProviderExecutableError::Io {
                path: path.to_path_buf(),
                kind: error.kind(),
            })?
            .len();
        if pe_offset > length.saturating_sub(4) {
            return Err(ProviderExecutableError::NotNativeExecutable(
                path.to_path_buf(),
            ));
        }
        file.seek(SeekFrom::Start(pe_offset))
            .map_err(|error| ProviderExecutableError::Io {
                path: path.to_path_buf(),
                kind: error.kind(),
            })?;
        let mut signature = [0_u8; 4];
        file.read_exact(&mut signature)
            .map_err(|_| ProviderExecutableError::NotNativeExecutable(path.to_path_buf()))?;
        if &signature != b"PE\0\0" {
            return Err(ProviderExecutableError::NotNativeExecutable(
                path.to_path_buf(),
            ));
        }
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if file
            .metadata()
            .map_err(|error| ProviderExecutableError::Io {
                path: path.to_path_buf(),
                kind: error.kind(),
            })?
            .permissions()
            .mode()
            & 0o111
            == 0
        {
            return Err(ProviderExecutableError::NotNativeExecutable(
                path.to_path_buf(),
            ));
        }
        let mut magic = [0_u8; 4];
        file.read_exact(&mut magic)
            .map_err(|_| ProviderExecutableError::NotNativeExecutable(path.to_path_buf()))?;
        let valid = matches!(
            magic,
            [0x7f, b'E', b'L', b'F']
                | [0xfe, 0xed, 0xfa, 0xce]
                | [0xce, 0xfa, 0xed, 0xfe]
                | [0xfe, 0xed, 0xfa, 0xcf]
                | [0xcf, 0xfa, 0xed, 0xfe]
                | [0xca, 0xfe, 0xba, 0xbe]
                | [0xbe, 0xba, 0xfe, 0xca]
                | [0xca, 0xfe, 0xba, 0xbf]
                | [0xbf, 0xba, 0xfe, 0xca]
        );
        if !valid {
            return Err(ProviderExecutableError::NotNativeExecutable(
                path.to_path_buf(),
            ));
        }
    }

    #[cfg(not(any(target_os = "windows", unix)))]
    {
        return Err(ProviderExecutableError::UnsupportedPlatform(
            path.to_path_buf(),
        ));
    }

    file.seek(SeekFrom::Start(0))
        .map_err(|error| ProviderExecutableError::Io {
            path: path.to_path_buf(),
            kind: error.kind(),
        })?;
    Ok(())
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

fn path_bytes(path: &Path) -> usize {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        return path.as_os_str().as_bytes().len();
    }

    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        return path.as_os_str().encode_wide().count().saturating_mul(2);
    }

    #[cfg(not(any(unix, windows)))]
    {
        path.to_string_lossy().len()
    }
}

fn os_str_bytes(value: &OsStr) -> usize {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        return value.as_bytes().len();
    }

    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        return value.encode_wide().count().saturating_mul(2);
    }

    #[cfg(not(any(unix, windows)))]
    {
        value.to_string_lossy().len()
    }
}

fn directory_identity(path: &Path) -> Result<u128, ProviderDiscoveryError> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|_| ProviderDiscoveryError::InvalidPathSnapshot(path.to_path_buf()))?;
    if !metadata.is_dir() || metadata_is_reparse(&metadata) {
        return Err(ProviderDiscoveryError::InvalidPathSnapshot(
            path.to_path_buf(),
        ));
    }

    let _ = metadata;
    let mut options = OpenOptions::new();
    options.read(true);

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows::Win32::Storage::FileSystem::{
            FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        };
        options.custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0 | FILE_FLAG_OPEN_REPARSE_POINT.0);
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let Some(no_follow) = unix_open_no_follow_flag() else {
            return Err(ProviderDiscoveryError::UnsupportedPlatform);
        };
        options.custom_flags(no_follow);
    }

    #[cfg(not(any(target_os = "windows", unix)))]
    {
        return Err(ProviderDiscoveryError::UnsupportedPlatform);
    }

    let file = options
        .open(path)
        .map_err(|_| ProviderDiscoveryError::InvalidPathSnapshot(path.to_path_buf()))?;
    let identity = file_identity(&file, path)
        .map_err(|_| ProviderDiscoveryError::InvalidPathSnapshot(path.to_path_buf()))?;
    let stable_id = identity.stable_id();
    if stable_id == 0 {
        return Err(ProviderDiscoveryError::InvalidPathSnapshot(
            path.to_path_buf(),
        ));
    }
    Ok(stable_id)
}

fn reject_reparse_components(path: &Path) -> Result<(), ProviderExecutableError> {
    for ancestor in path.ancestors() {
        match fs::symlink_metadata(ancestor) {
            Ok(metadata) if metadata.file_type().is_symlink() || metadata_is_reparse(&metadata) => {
                return Err(ProviderExecutableError::SymlinkOrReparse(
                    ancestor.to_path_buf(),
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(ProviderExecutableError::Missing(ancestor.to_path_buf()));
            }
            Err(error) => {
                return Err(ProviderExecutableError::Io {
                    path: ancestor.to_path_buf(),
                    kind: error.kind(),
                });
            }
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
    let _ = file;
    Err(ProviderExecutableError::UnsupportedPlatform(
        path.to_path_buf(),
    ))
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

/// One immutable, canonicalized capture of PATH. Resolution uses these
/// entries in their captured order and constructs candidate paths itself;
/// callers cannot assert a `PathEntry` origin for an arbitrary path.
#[derive(Clone, PartialEq, Eq)]
pub struct ProviderPathSnapshot {
    directories: Vec<ProviderPathSnapshotEntry>,
}

impl fmt::Debug for ProviderPathSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderPathSnapshot")
            .field("directory_count", &self.directories.len())
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProviderPathSnapshotEntry {
    index: usize,
    directory: PathBuf,
    identity: u128,
}

impl ProviderPathSnapshot {
    pub fn capture(path: impl AsRef<OsStr>) -> Result<Self, ProviderDiscoveryError> {
        let path = path.as_ref();
        if os_str_bytes(path) > MAX_PROVIDER_PATH_VALUE_BYTES {
            return Err(ProviderDiscoveryError::InvalidPathSnapshot(PathBuf::from(
                path,
            )));
        }
        let mut directories = Vec::with_capacity(MAX_PROVIDER_PATH_ENTRIES.min(8));
        for (index, directory) in std::env::split_paths(path).enumerate() {
            if index >= MAX_PROVIDER_PATH_ENTRIES {
                return Err(ProviderDiscoveryError::InvalidPathSnapshot(PathBuf::from(
                    path,
                )));
            }
            if directory.as_os_str().is_empty() || !directory.is_absolute() {
                return Err(ProviderDiscoveryError::InvalidPathSnapshot(directory));
            }
            if path_bytes(&directory) > MAX_PROVIDER_PATH_BYTES {
                return Err(ProviderDiscoveryError::InvalidPathSnapshot(directory));
            }
            match reject_reparse_components(&directory) {
                Ok(()) => {}
                Err(ProviderExecutableError::Missing(_)) => continue,
                Err(_) => {
                    return Err(ProviderDiscoveryError::InvalidPathSnapshot(directory));
                }
            }
            let canonical = match fs::canonicalize(&directory) {
                Ok(canonical) => canonical,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(_) => {
                    return Err(ProviderDiscoveryError::InvalidPathSnapshot(directory));
                }
            };
            if !canonical.is_dir() {
                continue;
            }
            reject_reparse_components(&canonical)
                .map_err(|_| ProviderDiscoveryError::InvalidPathSnapshot(canonical.clone()))?;
            let identity = directory_identity(&canonical)?;
            directories.push(ProviderPathSnapshotEntry {
                index,
                directory: canonical,
                identity,
            });
        }
        Ok(Self { directories })
    }

    pub fn capture_current() -> Result<Self, ProviderDiscoveryError> {
        let path = std::env::var_os("PATH")
            .ok_or_else(|| ProviderDiscoveryError::InvalidPathSnapshot(PathBuf::from("PATH")))?;
        Self::capture(path)
    }

    pub fn len(&self) -> usize {
        self.directories.len()
    }

    pub fn is_empty(&self) -> bool {
        self.directories.is_empty()
    }
}

impl ProviderPathSnapshotEntry {
    fn validate_current(&self) -> Result<(), ProviderDiscoveryError> {
        reject_reparse_components(&self.directory)
            .map_err(|_| ProviderDiscoveryError::InvalidPathSnapshot(self.directory.clone()))?;
        let canonical = fs::canonicalize(&self.directory)
            .map_err(|_| ProviderDiscoveryError::InvalidPathSnapshot(self.directory.clone()))?;
        if canonical != self.directory || !canonical.is_dir() {
            return Err(ProviderDiscoveryError::InvalidPathSnapshot(
                self.directory.clone(),
            ));
        }
        if directory_identity(&canonical)? != self.identity {
            return Err(ProviderDiscoveryError::InvalidPathSnapshot(
                self.directory.clone(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, PartialEq, Eq)]
pub enum ProviderDiscoveryOrigin {
    ConfiguredOverride,
    PathEntry { index: usize, directory: PathBuf },
}

impl fmt::Debug for ProviderDiscoveryOrigin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ConfiguredOverride => formatter.write_str("ConfiguredOverride"),
            Self::PathEntry { index, .. } => formatter
                .debug_struct("PathEntry")
                .field("index", index)
                .field("directory", &"<redacted>")
                .finish(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderExecutableForm {
    Native,
    WindowsShim { target: Box<ProviderExecutable> },
}

#[derive(Clone, PartialEq, Eq)]
pub struct ProviderDiscoveryCandidate {
    kind: ProviderKind,
    origin: ProviderDiscoveryOrigin,
    requested_path: PathBuf,
    executable: ProviderExecutable,
    form: ProviderExecutableForm,
}

impl fmt::Debug for ProviderDiscoveryCandidate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderDiscoveryCandidate")
            .field("origin", &self.origin)
            .field("requested_path", &"<redacted>")
            .field("executable", &self.executable)
            .field("form", &self.form)
            .finish()
    }
}

impl ProviderDiscoveryCandidate {
    pub const fn kind(&self) -> ProviderKind {
        self.kind
    }

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

#[derive(Clone, PartialEq, Eq)]
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

impl fmt::Debug for ProviderDiscoveryCandidateInput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Native { origin, .. } => formatter
                .debug_struct("Native")
                .field("path", &"<redacted>")
                .field("origin", origin)
                .finish(),
            Self::WindowsShim { origin, .. } => formatter
                .debug_struct("WindowsShim")
                .field("shim_path", &"<redacted>")
                .field("target_path", &"<redacted>")
                .field("origin", origin)
                .finish(),
        }
    }
}

impl ProviderDiscoveryCandidateInput {
    pub fn configured_override(path: impl Into<PathBuf>) -> Self {
        Self::Native {
            path: path.into(),
            origin: ProviderDiscoveryOrigin::ConfiguredOverride,
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

#[derive(Clone, PartialEq, Eq)]
pub enum ProviderDiscoveryError {
    UnsupportedPlatform,
    NoCandidate(ProviderKind),
    InvalidPathSnapshot(PathBuf),
    OriginNotAllowed(PathBuf),
    ForbiddenRunner(PathBuf),
    WrongEntrypoint(PathBuf),
    WrongFileType(PathBuf),
    ShimProofInvalid(PathBuf),
    Executable(ProviderExecutableError),
}

impl fmt::Debug for ProviderDiscoveryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let code = match self {
            Self::UnsupportedPlatform => "unsupported_platform",
            Self::NoCandidate(_) => "no_candidate",
            Self::InvalidPathSnapshot(_) => "invalid_path_snapshot",
            Self::OriginNotAllowed(_) => "origin_not_allowed",
            Self::ForbiddenRunner(_) => "forbidden_runner",
            Self::WrongEntrypoint(_) => "wrong_entrypoint",
            Self::WrongFileType(_) => "wrong_file_type",
            Self::ShimProofInvalid(_) => "shim_proof_invalid",
            Self::Executable(_) => "executable",
        };
        formatter
            .debug_struct("ProviderDiscoveryError")
            .field("code", &code)
            .finish()
    }
}

impl fmt::Display for ProviderDiscoveryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedPlatform => write!(f, "Windows provider shims are unsupported here"),
            Self::NoCandidate(kind) => {
                write!(f, "no trusted provider executable was found for {kind:?}")
            }
            Self::InvalidPathSnapshot(_) => {
                write!(f, "provider PATH snapshot entry is not a trusted directory")
            }
            Self::OriginNotAllowed(_) => {
                write!(f, "provider executable origin is not allowlisted")
            }
            Self::ForbiddenRunner(_) => {
                write!(f, "shell and package runners are not provider executables")
            }
            Self::WrongEntrypoint(_) => {
                write!(f, "provider executable is not an allowlisted entrypoint")
            }
            Self::WrongFileType(_) => {
                write!(f, "provider executable has the wrong file type")
            }
            Self::ShimProofInvalid(_) => {
                write!(f, "provider Windows shim proof is invalid")
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

    /// Resolve provider candidates from one captured PATH snapshot. Candidate
    /// paths and their `PathEntry` provenance are constructed here, never
    /// supplied by the caller.
    pub fn resolve_from_path_snapshot(
        &self,
        snapshot: &ProviderPathSnapshot,
    ) -> Result<ProviderDiscoveryCandidate, ProviderDiscoveryError> {
        self.resolve_all_from_path_snapshot(snapshot)?
            .into_iter()
            .next()
            .ok_or(ProviderDiscoveryError::NoCandidate(self.kind))
    }

    pub fn resolve_all_from_path_snapshot(
        &self,
        snapshot: &ProviderPathSnapshot,
    ) -> Result<Vec<ProviderDiscoveryCandidate>, ProviderDiscoveryError> {
        let mut candidates = Vec::new();
        for entry in &snapshot.directories {
            entry.validate_current()?;
            let origin = ProviderDiscoveryOrigin::PathEntry {
                index: entry.index,
                directory: entry.directory.clone(),
            };
            let native_path = entry.directory.join(&self.native_entrypoint);
            match ProviderExecutable::from_path(&native_path) {
                Ok(executable) => {
                    self.validate_native_path(&executable)?;
                    candidates.push(ProviderDiscoveryCandidate {
                        kind: self.kind,
                        origin: origin.clone(),
                        requested_path: native_path.clone(),
                        executable,
                        form: ProviderExecutableForm::Native,
                    });
                }
                Err(ProviderExecutableError::Missing(_) | ProviderExecutableError::NotAFile(_)) => {
                }
                Err(error) => return Err(ProviderDiscoveryError::Executable(error)),
            }

            #[cfg(target_os = "windows")]
            if let Some(shim_entrypoint) = self.shim_entrypoint() {
                let shim_path = entry.directory.join(shim_entrypoint);
                match fs::symlink_metadata(&shim_path) {
                    Ok(_) => candidates.push(self.validate_windows_shim(
                        shim_path,
                        native_path.clone(),
                        origin,
                        true,
                    )?),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(ProviderDiscoveryError::Executable(
                            ProviderExecutableError::Io {
                                path: shim_path,
                                kind: error.kind(),
                            },
                        ));
                    }
                }
            }
        }
        Ok(candidates)
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
                    kind: self.kind,
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
            } => self.validate_windows_shim(shim_path, target_path, origin, false),
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

    pub fn validate_executable(
        &self,
        executable: &ProviderExecutable,
    ) -> Result<(), ProviderDiscoveryError> {
        if !executable.is_native() {
            return Err(ProviderDiscoveryError::WrongFileType(
                executable.canonical_path().to_path_buf(),
            ));
        }
        self.validate_native_path(executable)
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
        if is_forbidden_runner_name(file_name) {
            return Err(ProviderDiscoveryError::ForbiddenRunner(
                executable.canonical_path().to_path_buf(),
            ));
        }
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
        match origin {
            ProviderDiscoveryOrigin::ConfiguredOverride => Ok(()),
            ProviderDiscoveryOrigin::PathEntry { .. } => {
                // A caller-supplied PathEntry has no captured PATH proof. Only
                // resolve_all_from_path_snapshot may create trusted provenance.
                Err(ProviderDiscoveryError::OriginNotAllowed(
                    executable.canonical_path().to_path_buf(),
                ))
            }
        }
    }

    fn validate_windows_shim(
        &self,
        shim_path: PathBuf,
        target_path: PathBuf,
        origin: ProviderDiscoveryOrigin,
        trusted_origin: bool,
    ) -> Result<ProviderDiscoveryCandidate, ProviderDiscoveryError> {
        #[cfg(not(target_os = "windows"))]
        {
            let _ = (shim_path, target_path, origin, trusted_origin);
            return Err(ProviderDiscoveryError::UnsupportedPlatform);
        }

        #[cfg(target_os = "windows")]
        {
            let Some(shim_entrypoint) = self.shim_entrypoint() else {
                return Err(ProviderDiscoveryError::UnsupportedPlatform);
            };
            let shim = ProviderExecutable::inspect_non_native_blocking(&shim_path)?;
            if !trusted_origin {
                self.validate_origin(&shim, &origin)?;
            }
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
            let contents = shim.read_handle_contents().map_err(|_| {
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
                kind: self.kind,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderCapabilities {
    pub kind: ProviderKind,
    pub version: ProviderVersion,
    pub auth_state: ProviderAuthState,
    pub exact_resume: CapabilitySupport,
    pub semantic_events: CapabilitySupport,
    pub provider_session_id: CapabilitySupport,
    pub build_launch: CapabilitySupport,
    pub parse_signal: CapabilitySupport,
    pub cooperative_stop: CapabilitySupport,
    pub observe_quota: CapabilitySupport,
    pub evidence: Vec<CapabilityEvidence>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderCapabilitiesError {
    TooManyEvidenceItems,
    UnsupportedSchemaVersion(u16),
    InvalidEvidence(CapabilityEvidenceError),
    MissingAuthStatusEvidence(ProviderAuthState),
    MismatchedAuthStatusEvidence {
        state: ProviderAuthState,
        evidence: EvidenceStatus,
    },
    MismatchedAuthEvidenceSource {
        kind: ProviderKind,
        source: ProviderAuthEvidenceSource,
    },
}

impl fmt::Display for ProviderCapabilitiesError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooManyEvidenceItems => write!(
                f,
                "provider capabilities contain more than {MAX_CAPABILITY_EVIDENCE_ITEMS} evidence items"
            ),
            Self::UnsupportedSchemaVersion(version) => {
                write!(f, "unsupported provider capability schema version {version}")
            }
            Self::InvalidEvidence(error) => error.fmt(f),
            Self::MissingAuthStatusEvidence(state) => write!(
                f,
                "provider auth state {state:?} requires matching AuthStatusProbe evidence"
            ),
            Self::MismatchedAuthStatusEvidence { state, evidence } => write!(
                f,
                "provider auth state {state:?} does not match AuthStatusProbe evidence {evidence:?}"
            ),
            Self::MismatchedAuthEvidenceSource { kind, source } => write!(
                f,
                "provider auth evidence source {source:?} does not belong to {kind:?}"
            ),
        }
    }
}

impl std::error::Error for ProviderCapabilitiesError {}

impl Serialize for ProviderCapabilities {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.validate().map_err(serde::ser::Error::custom)?;
        let mut state = serializer.serialize_struct("ProviderCapabilities", 12)?;
        state.serialize_field("schema_version", &PROVIDER_CAPABILITY_SCHEMA_VERSION)?;
        state.serialize_field("kind", &self.kind)?;
        state.serialize_field("version", &self.version)?;
        state.serialize_field("auth_state", &self.auth_state)?;
        state.serialize_field("exact_resume", &self.exact_resume)?;
        state.serialize_field("semantic_events", &self.semantic_events)?;
        state.serialize_field("provider_session_id", &self.provider_session_id)?;
        state.serialize_field("build_launch", &self.build_launch)?;
        state.serialize_field("parse_signal", &self.parse_signal)?;
        state.serialize_field("cooperative_stop", &self.cooperative_stop)?;
        state.serialize_field("observe_quota", &self.observe_quota)?;
        state.serialize_field("evidence", &self.evidence)?;
        state.end()
    }
}

struct BoundedCapabilityEvidence(Vec<CapabilityEvidence>);

impl<'de> Deserialize<'de> for BoundedCapabilityEvidence {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct EvidenceVisitor;

        impl<'de> Visitor<'de> for EvidenceVisitor {
            type Value = BoundedCapabilityEvidence;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a bounded capability evidence sequence")
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let size_hint = sequence.size_hint().unwrap_or_default();
                if size_hint > MAX_CAPABILITY_EVIDENCE_ITEMS {
                    return Err(de::Error::custom(
                        ProviderCapabilitiesError::TooManyEvidenceItems,
                    ));
                }
                let mut evidence = Vec::with_capacity(size_hint.min(MAX_CAPABILITY_EVIDENCE_ITEMS));
                while evidence.len() < MAX_CAPABILITY_EVIDENCE_ITEMS {
                    let Some(item) = sequence.next_element::<CapabilityEvidence>()? else {
                        return Ok(BoundedCapabilityEvidence(evidence));
                    };
                    evidence.push(item);
                }
                if sequence.next_element::<de::IgnoredAny>()?.is_some() {
                    return Err(de::Error::custom(
                        ProviderCapabilitiesError::TooManyEvidenceItems,
                    ));
                }
                Ok(BoundedCapabilityEvidence(evidence))
            }
        }

        deserializer.deserialize_seq(EvidenceVisitor)
    }
}

impl<'de> Deserialize<'de> for ProviderCapabilities {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            schema_version: u16,
            kind: ProviderKind,
            version: ProviderVersion,
            auth_state: ProviderAuthState,
            exact_resume: CapabilitySupport,
            semantic_events: CapabilitySupport,
            provider_session_id: CapabilitySupport,
            build_launch: CapabilitySupport,
            parse_signal: CapabilitySupport,
            cooperative_stop: CapabilitySupport,
            observe_quota: CapabilitySupport,
            evidence: BoundedCapabilityEvidence,
        }

        let wire = Wire::deserialize(deserializer)?;
        if wire.schema_version != PROVIDER_CAPABILITY_SCHEMA_VERSION {
            return Err(de::Error::custom(
                ProviderCapabilitiesError::UnsupportedSchemaVersion(wire.schema_version),
            ));
        }
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
            evidence: wire.evidence.0,
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
            if let Some(source) = evidence.auth_source() {
                if source.provider_kind() != self.kind {
                    return Err(ProviderCapabilitiesError::MismatchedAuthEvidenceSource {
                        kind: self.kind,
                        source,
                    });
                }
            }
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

    pub const fn auth_state(&self) -> ProviderAuthState {
        self.auth_state
    }

    pub fn evidence(&self) -> &[CapabilityEvidence] {
        &self.evidence
    }

    pub(crate) fn with_auth_receipt(
        &self,
        receipt: &ProviderAuthEvidenceReceipt,
    ) -> Result<Self, ProviderCapabilitiesError> {
        let mut merged = self.stable_projection();
        merged.auth_state = receipt
            .result()
            .as_stable_state()
            .unwrap_or(ProviderAuthState::Unknown);
        merged
            .evidence
            .push(CapabilityEvidence::from_auth_receipt(receipt));
        merged.validate()?;
        Ok(merged)
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
