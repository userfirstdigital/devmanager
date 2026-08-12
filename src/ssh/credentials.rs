//! Host-owned SSH credential references and temporary-key custody.
//!
//! The module intentionally stops at a typed, local boundary.  A resolver is
//! supplied by the future task supervisor; this module never reads a profile,
//! launches a process, or puts secret bytes in a command/event/snapshot.  A
//! pasted private key is securely materialized into a restrictive retained
//! file for the eventual SSH child; cleanup is bounded and identity-aware, but
//! it is not accurate to claim that bytes never exist in files.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, TryLockError, Weak};
use std::time::Instant;

#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

use serde::de::{self, Visitor};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

pub(crate) const MAX_CREDENTIAL_REF_BYTES: usize = 256;
pub(crate) const MAX_SECRET_BYTES: usize = 128 * 1024;
pub(crate) const MAX_KEY_TEXT_BYTES: usize = 128 * 1024;

const KEY_FILE_SUFFIX: &str = ".key";
const MANIFEST_SUFFIX: &str = ".json";
const TEMP_SUFFIX: &str = ".tmp";
const BACKUP_SUFFIX: &str = ".old";
const MANIFEST_VERSION: u8 = 3;
const MAX_RECOVERY_RECORDS: usize = 256;
const MAX_RECOVERY_ORPHANS: usize = 256;
const MAX_RECOVERY_ENTRIES: usize = MAX_RECOVERY_RECORDS * 2 + MAX_RECOVERY_ORPHANS;
const MAX_MANIFEST_BYTES: usize = 512;
const MAX_PID_TEXT_BYTES: usize = 10;
const MAX_NONCE_TEXT_BYTES: usize = 20;
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

// Every store instance canonicalizes its root and joins this lock.  The lock
// is the host-owned operation boundary for the check/identity/cleanup and
// check/publication sequences.  In particular, a second store (or restart
// recovery) cannot replace a path between those steps.
static ROOT_OPERATION_LOCKS: OnceLock<Mutex<BTreeMap<PathBuf, Weak<Mutex<()>>>>> = OnceLock::new();

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MaterializeFailurePoint {
    Acl,
    Write,
    Flush,
    Sync,
    PostPublicationExpiry,
}

#[cfg(test)]
type CleanupBarrier = Arc<(std::sync::Barrier, std::sync::Barrier)>;

#[cfg(test)]
static TEST_CLEANUP_BARRIER: OnceLock<Mutex<Option<CleanupBarrier>>> = OnceLock::new();

#[cfg(all(test, unix))]
type PostcheckSwapBarrier = Arc<(std::sync::Barrier, std::sync::Barrier)>;

#[cfg(all(test, unix))]
static TEST_POSTCHECK_SWAP_BARRIER: OnceLock<Mutex<Option<PostcheckSwapBarrier>>> = OnceLock::new();

#[cfg(all(test, unix))]
type UnrecognizedSwapBarrier = (std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>);

#[cfg(all(test, unix))]
static TEST_UNRECOGNIZED_SWAP_BARRIER: OnceLock<Mutex<Option<UnrecognizedSwapBarrier>>> =
    OnceLock::new();

#[cfg(all(test, unix))]
type PreUnlinkSwapBarrier = (
    std::sync::mpsc::Sender<PathBuf>,
    std::sync::mpsc::Receiver<()>,
);

#[cfg(all(test, unix))]
static TEST_PRE_UNLINK_SWAP_BARRIER: OnceLock<Mutex<Option<PreUnlinkSwapBarrier>>> =
    OnceLock::new();

/// A safe reference into a host-owned credential provider.
///
/// The value is deliberately restricted to a small identifier alphabet.  It
/// can therefore be carried in a snapshot without becoming a path, a shell
/// argument, or a secret-bearing error message.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub(crate) struct CredentialRef(String);

impl CredentialRef {
    pub(crate) fn new(value: impl AsRef<str>) -> Result<Self, CredentialError> {
        Self::parse(value)
    }

    pub(crate) fn parse(value: impl AsRef<str>) -> Result<Self, CredentialError> {
        let value = value.as_ref();
        validate_credential_ref(value)?;
        Ok(Self(value.to_string()))
    }

    fn parse_owned(value: String) -> Result<Self, CredentialError> {
        validate_credential_ref(&value)?;
        Ok(Self(value))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

fn validate_credential_ref(value: &str) -> Result<(), CredentialError> {
    if value.is_empty()
        || value.len() > MAX_CREDENTIAL_REF_BYTES
        || !value.starts_with("credential:")
        || value.len() == "credential:".len()
    {
        return Err(CredentialError::InvalidReference);
    }
    if !value["credential:".len()..].bytes().all(|byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'/' | b':' | b'-')
    }) {
        return Err(CredentialError::InvalidReference);
    }
    Ok(())
}

impl fmt::Debug for CredentialRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CredentialRef(REDACTED)")
    }
}

impl fmt::Display for CredentialRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for CredentialRef {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct CredentialRefVisitor;

        impl<'de> Visitor<'de> for CredentialRefVisitor {
            type Value = CredentialRef;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a bounded credential reference")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                CredentialRef::parse(value).map_err(E::custom)
            }

            fn visit_borrowed_str<E>(self, value: &'de str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                self.visit_str(value)
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                CredentialRef::parse_owned(value).map_err(E::custom)
            }
        }

        deserializer.deserialize_str(CredentialRefVisitor)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum CredentialKind {
    Password,
    PrivateKey,
    Passphrase,
}

/// A short-lived zeroizing secret.  It is intentionally not `Clone`,
/// `Serialize`, or `Deserialize`; only the host-owned resolver can construct
/// one and only this module can borrow its bytes.
pub(super) struct CredentialSecret {
    kind: CredentialKind,
    bytes: Zeroizing<Vec<u8>>,
}

impl CredentialSecret {
    #[cfg(test)]
    pub(super) fn password(value: impl AsRef<str>) -> Self {
        Self::new(CredentialKind::Password, value.as_ref().as_bytes())
    }

    #[cfg(test)]
    pub(super) fn private_key(value: impl AsRef<str>) -> Self {
        Self::new(CredentialKind::PrivateKey, value.as_ref().as_bytes())
    }

    #[cfg(test)]
    pub(super) fn passphrase(value: impl AsRef<str>) -> Self {
        Self::new(CredentialKind::Passphrase, value.as_ref().as_bytes())
    }

    pub(super) fn from_bytes(kind: CredentialKind, value: &[u8]) -> Result<Self, CredentialError> {
        if value.is_empty() || value.len() > MAX_SECRET_BYTES {
            return Err(CredentialError::SecretTooLarge);
        }
        Ok(Self::new(kind, value))
    }

    fn new(kind: CredentialKind, value: &[u8]) -> Self {
        Self {
            kind,
            bytes: Zeroizing::new(value.to_vec()),
        }
    }

    pub(super) fn kind(&self) -> CredentialKind {
        self.kind
    }

    pub(super) fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub(super) fn validate(&self) -> Result<(), CredentialError> {
        if self.bytes.is_empty() || self.bytes.len() > MAX_SECRET_BYTES {
            return Err(CredentialError::SecretTooLarge);
        }
        match self.kind {
            CredentialKind::PrivateKey => {
                std::str::from_utf8(&self.bytes)
                    .map_err(|_| CredentialError::InvalidSecretMaterial)?;
                sanitize_private_key(&self.bytes)?;
            }
            CredentialKind::Password | CredentialKind::Passphrase => {
                if self.bytes.iter().any(|byte| byte.is_ascii_control()) {
                    return Err(CredentialError::InvalidSecretMaterial);
                }
            }
        }
        Ok(())
    }
}

impl fmt::Debug for CredentialSecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CredentialSecret")
            .field("kind", &self.kind)
            .field("bytes", &"REDACTED")
            .finish()
    }
}

/// The only credential lookup seam.  The Task 3 supervisor will provide the
/// production implementation; tests use a memory-only resolver.
pub(super) trait CredentialResolver {
    fn resolve(&self, reference: &CredentialRef) -> Result<CredentialSecret, CredentialError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CredentialError {
    InvalidReference,
    MissingReference(CredentialRef),
    WrongKind {
        expected: CredentialKind,
        actual: CredentialKind,
    },
    SecretTooLarge,
    InvalidSecretMaterial,
    MissingKeyStore,
    InvalidKeyIdentity,
    InvalidPath,
    StoreFull,
    AlreadyRetained,
    NotRetained,
    Io,
    DeadlineExpired,
    UnsupportedRuntime,
    CleanupUncertain,
}

impl fmt::Display for CredentialError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidReference => formatter.write_str("invalid credential reference"),
            Self::MissingReference(_) => formatter.write_str("credential reference unavailable"),
            Self::WrongKind { expected, actual } => {
                write!(
                    formatter,
                    "credential kind mismatch: expected {expected:?}, got {actual:?}"
                )
            }
            Self::SecretTooLarge => formatter.write_str("credential material exceeds the bound"),
            Self::InvalidSecretMaterial => formatter.write_str("credential material is invalid"),
            Self::MissingKeyStore => formatter.write_str("pasted-key auth requires a key store"),
            Self::InvalidKeyIdentity => formatter.write_str("invalid retained-key identity"),
            Self::InvalidPath => formatter.write_str("invalid retained-key path"),
            Self::StoreFull => formatter.write_str("retained-key store is at capacity"),
            Self::AlreadyRetained => formatter.write_str("key identity is already retained"),
            Self::NotRetained => formatter.write_str("key identity is not retained"),
            Self::Io => formatter.write_str("credential storage I/O failed"),
            Self::DeadlineExpired => formatter.write_str("credential operation deadline expired"),
            Self::UnsupportedRuntime => {
                formatter.write_str("credential operation unsupported on this runtime")
            }
            Self::CleanupUncertain => formatter.write_str("credential cleanup is uncertain"),
        }
    }
}

impl std::error::Error for CredentialError {}

// Authorities are small executables, known-host files, or private keys.  Keep
// the synchronous fingerprint bounded well below the old 256 MiB ceiling so
// a launch request cannot spend an unbounded amount of time hashing input.
const MAX_PINNED_FILE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_PINNED_PATH_BYTES: usize = 4 * 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    first: u64,
    second: u64,
}

fn file_identity(file: &File) -> Result<FileIdentity, CredentialError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = file.metadata().map_err(|_| CredentialError::Io)?;
        Ok(FileIdentity {
            first: metadata.dev(),
            second: metadata.ino(),
        })
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows::Win32::Storage::FileSystem::{
            GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        };
        let mut information = BY_HANDLE_FILE_INFORMATION::default();
        unsafe {
            GetFileInformationByHandle(
                windows::Win32::Foundation::HANDLE(file.as_raw_handle() as *mut _),
                &mut information,
            )
        }
        .map_err(|_| CredentialError::Io)?;
        Ok(FileIdentity {
            first: information.dwVolumeSerialNumber as u64,
            second: ((information.nFileIndexHigh as u64) << 32) | information.nFileIndexLow as u64,
        })
    }
    #[cfg(not(any(unix, windows)))]
    {
        let metadata = file.metadata().map_err(|_| CredentialError::Io)?;
        Ok(FileIdentity {
            first: 0,
            second: metadata.len(),
        })
    }
}

fn entry_identity(metadata: &fs::Metadata) -> Result<FileIdentity, CredentialError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        return Ok(FileIdentity {
            first: metadata.dev(),
            second: metadata.ino(),
        });
    }
    #[cfg(windows)]
    {
        // Windows orphan symlinks are deleted through an opened reparse-point
        // handle below; the lstat identity is only enforced on Unix, where
        // the no-replace quarantine primitive is available.
        let _ = metadata;
        return Ok(FileIdentity {
            first: 0,
            second: 0,
        });
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = metadata;
        Ok(FileIdentity {
            first: 0,
            second: 0,
        })
    }
}

/// A canonical, read-only file authority retained across planning and the
/// eventual child handoff.  The open handle and content fingerprint make a
/// path replacement or in-place mutation observable before delivery.
pub(super) struct PinnedFile {
    path: PathBuf,
    handle: Arc<File>,
    identity: FileIdentity,
    length: u64,
    fingerprint: [u8; 32],
}

impl PinnedFile {
    pub(super) fn open(path: &Path) -> Result<Self, CredentialError> {
        Self::open_with_deadline(path, None)
    }

    pub(super) fn open_until(path: &Path, deadline: Instant) -> Result<Self, CredentialError> {
        Self::open_with_deadline(path, Some(deadline))
    }

    fn open_with_deadline(path: &Path, deadline: Option<Instant>) -> Result<Self, CredentialError> {
        check_deadline(deadline)?;
        if native_path_length(path) > MAX_PINNED_PATH_BYTES {
            return Err(CredentialError::InvalidPath);
        }
        reject_symlink_if_present(path)?;
        check_deadline(deadline)?;
        // Open the caller spelling before canonicalization.  The no-follow
        // handle lets us detect a final-component replacement between the
        // metadata check and canonicalization instead of silently pinning a
        // different target.
        let original = open_no_follow_until(path, deadline)?;
        check_deadline(deadline)?;
        let canonical = fs::canonicalize(path).map_err(|_| CredentialError::InvalidPath)?;
        check_deadline(deadline)?;
        if native_path_length(&canonical) > MAX_PINNED_PATH_BYTES {
            return Err(CredentialError::InvalidPath);
        }
        let handle = Arc::new(open_no_follow_until(&canonical, deadline)?);
        check_deadline(deadline)?;
        let original_identity = file_identity(&original)?;
        let canonical_identity = file_identity(&handle)?;
        check_deadline(deadline)?;
        let metadata = fs::metadata(&canonical).map_err(|_| CredentialError::Io)?;
        check_deadline(deadline)?;
        if original_identity != canonical_identity {
            return Err(CredentialError::InvalidPath);
        }
        // Query the canonical path for regular-file shape after opening the
        // no-follow handle.  Windows reports an OPEN_REPARSE_POINT handle as
        // a reparse object even when the canonical target itself is a normal
        // file; the path check still rejects a caller-visible reparse point.
        if is_windows_reparse_metadata(&metadata)
            || !metadata.is_file()
            || metadata.len() > MAX_PINNED_FILE_BYTES
        {
            return Err(CredentialError::InvalidPath);
        }
        let fingerprint = fingerprint_file_with_deadline(&handle, deadline)?;
        Ok(Self {
            path: canonical,
            handle,
            identity: canonical_identity,
            length: metadata.len(),
            fingerprint,
        })
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    pub(super) fn path_string(&self) -> String {
        self.path.to_string_lossy().into_owned()
    }

    pub(super) fn revalidate(&self) -> Result<(), CredentialError> {
        self.revalidate_with_deadline(None)
    }

    pub(super) fn revalidate_until(&self, deadline: Instant) -> Result<(), CredentialError> {
        self.revalidate_with_deadline(Some(deadline))
    }

    fn revalidate_with_deadline(&self, deadline: Option<Instant>) -> Result<(), CredentialError> {
        let current = Self::open_with_deadline(&self.path, deadline)?;
        if current.identity != self.identity
            || current.length != self.length
            || current.fingerprint != self.fingerprint
        {
            return Err(CredentialError::InvalidPath);
        }
        Ok(())
    }

    pub(super) fn fingerprint(&self) -> [u8; 32] {
        self.fingerprint
    }
}

impl Clone for PinnedFile {
    fn clone(&self) -> Self {
        Self {
            path: self.path.clone(),
            handle: Arc::clone(&self.handle),
            identity: self.identity,
            length: self.length,
            fingerprint: self.fingerprint,
        }
    }
}

impl fmt::Debug for PinnedFile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PinnedFile")
            .field("path", &"REDACTED_PATH")
            .field("length", &self.length)
            .field("fingerprint", &"REDACTED")
            .finish()
    }
}

fn fingerprint_file(handle: &File) -> Result<[u8; 32], CredentialError> {
    fingerprint_file_with_deadline(handle, None)
}

fn fingerprint_file_with_deadline(
    handle: &File,
    deadline: Option<Instant>,
) -> Result<[u8; 32], CredentialError> {
    check_deadline(deadline)?;
    let mut reader = handle.try_clone().map_err(|_| CredentialError::Io)?;
    reader
        .seek(SeekFrom::Start(0))
        .map_err(|_| CredentialError::Io)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 16 * 1024];
    let mut total = 0u64;
    loop {
        check_deadline(deadline)?;
        let read = reader.read(&mut buffer).map_err(|_| CredentialError::Io)?;
        if read == 0 {
            break;
        }
        total = total
            .checked_add(read as u64)
            .ok_or(CredentialError::StoreFull)?;
        if total > MAX_PINNED_FILE_BYTES {
            return Err(CredentialError::StoreFull);
        }
        hasher.update(&buffer[..read]);
    }
    check_deadline(deadline)?;
    Ok(hasher.finalize().into())
}

fn check_deadline(deadline: Option<Instant>) -> Result<(), CredentialError> {
    if deadline.is_some_and(|deadline| deadline <= Instant::now()) {
        Err(CredentialError::DeadlineExpired)
    } else {
        Ok(())
    }
}

fn lock_mutex_until<'a, T>(
    mutex: &'a Mutex<T>,
    deadline: Option<Instant>,
) -> Result<MutexGuard<'a, T>, CredentialError> {
    loop {
        match mutex.try_lock() {
            Ok(guard) => return Ok(guard),
            Err(TryLockError::Poisoned(_)) => return Err(CredentialError::Io),
            Err(TryLockError::WouldBlock) => {
                check_deadline(deadline)?;
                std::thread::yield_now();
            }
        }
    }
}

fn native_path_length(path: &Path) -> usize {
    native_os_str_length(path.as_os_str())
}

fn native_os_str_length(value: &OsStr) -> usize {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        return value.as_bytes().len();
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        return value
            .encode_wide()
            .count()
            .saturating_mul(std::mem::size_of::<u16>());
    }
    #[cfg(not(any(unix, windows)))]
    {
        value.len()
    }
}

fn native_separator_length() -> usize {
    #[cfg(windows)]
    {
        return std::mem::size_of::<u16>();
    }
    #[cfg(not(windows))]
    {
        1
    }
}

pub(super) fn ensure_supported_runtime() -> Result<(), CredentialError> {
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        all(
            unix,
            not(any(
                target_os = "linux",
                target_os = "android",
                target_os = "macos",
                target_os = "ios"
            ))
        )
    ))]
    {
        return Err(CredentialError::UnsupportedRuntime);
    }
    #[cfg(not(any(
        target_os = "macos",
        target_os = "ios",
        all(
            unix,
            not(any(
                target_os = "linux",
                target_os = "android",
                target_os = "macos",
                target_os = "ios"
            ))
        )
    )))]
    {
        Ok(())
    }
}

fn ensure_child_name_bound(
    root: &Path,
    name_length: usize,
    error: CredentialError,
) -> Result<(), CredentialError> {
    if name_length == 0
        || native_path_length(root)
            .saturating_add(native_separator_length())
            .saturating_add(name_length)
            > MAX_PINNED_PATH_BYTES
    {
        return Err(error);
    }
    Ok(())
}

#[cfg(windows)]
pub(super) fn bounded_system_path(
    root: &OsStr,
    components: &[&str],
) -> Result<PathBuf, CredentialError> {
    let mut length = native_os_str_length(root);
    for component in components {
        length = length
            .checked_add(native_separator_length())
            .and_then(|length| length.checked_add(native_os_str_length(OsStr::new(component))))
            .ok_or(CredentialError::InvalidPath)?;
    }
    if root.is_empty() || length > MAX_PINNED_PATH_BYTES {
        return Err(CredentialError::InvalidPath);
    }

    let mut path = PathBuf::from(root);
    for component in components {
        path.push(component);
    }
    if native_path_length(&path) > MAX_PINNED_PATH_BYTES {
        return Err(CredentialError::InvalidPath);
    }
    Ok(path)
}

fn join_store_child(root: &Path, name: &str) -> Result<PathBuf, CredentialError> {
    join_store_child_os(root, OsStr::new(name), CredentialError::InvalidPath)
}

fn join_store_child_os(
    root: &Path,
    name: &OsStr,
    error: CredentialError,
) -> Result<PathBuf, CredentialError> {
    ensure_child_name_bound(root, native_os_str_length(name), error.clone())?;
    let path = root.join(name);
    if native_path_length(&path) > MAX_PINNED_PATH_BYTES {
        return Err(error);
    }
    Ok(path)
}

fn bounded_entry_path(root: &Path, name: &OsStr) -> Result<PathBuf, CredentialError> {
    join_store_child_os(root, name, CredentialError::StoreFull)
}

fn key_material_path(
    root: &Path,
    identity: &KeyIdentity,
    suffix: &str,
) -> Result<PathBuf, CredentialError> {
    const PREFIX: &str = "ssh-";
    let name_length = PREFIX
        .len()
        .saturating_add(identity.digest.len().saturating_mul(2))
        .saturating_add(suffix.len());
    ensure_child_name_bound(root, name_length, CredentialError::InvalidPath)?;
    let digest = identity.digest_hex();
    let mut name = String::with_capacity(name_length);
    name.push_str(PREFIX);
    name.push_str(&digest);
    name.push_str(suffix);
    join_store_child(root, &name)
}

/// Opaque fixed-size identity for retained key material.  It contains no
/// connection text, credential reference, path, or serde implementation.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct KeyIdentity {
    digest: [u8; 32],
    index: u32,
}

impl KeyIdentity {
    pub(crate) fn issue(
        connection_id: &str,
        credential_ref: &CredentialRef,
    ) -> Result<Self, CredentialError> {
        if connection_id.is_empty()
            || connection_id.len() > MAX_CREDENTIAL_REF_BYTES
            || !connection_id.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'/' | b':' | b'-')
            })
        {
            return Err(CredentialError::InvalidReference);
        }
        let mut hasher = Sha256::new();
        hasher.update(connection_id.as_bytes());
        hasher.update([0]);
        hasher.update(credential_ref.as_str().as_bytes());
        let digest: [u8; 32] = hasher.finalize().into();
        Ok(Self {
            index: u32::from_be_bytes(digest[..4].try_into().expect("fixed digest prefix")),
            digest,
        })
    }

    fn from_manifest(digest: [u8; 32], index: u32) -> Result<Self, CredentialError> {
        let expected = u32::from_be_bytes(digest[..4].try_into().expect("fixed digest prefix"));
        if digest.iter().all(|byte| *byte == 0) || expected != index {
            return Err(CredentialError::InvalidKeyIdentity);
        }
        Ok(Self { digest, index })
    }

    fn digest_hex(&self) -> String {
        self.digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    pub(crate) fn index(&self) -> u32 {
        self.index
    }
}

impl fmt::Debug for KeyIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("KeyIdentity")
            .field("index", &self.index)
            .field("digest", &"REDACTED")
            .finish()
    }
}

/// A retained key owns the open read handle and exact cleanup record.  It is
/// not cloneable or serializable; dropping the launch plan drops this value
/// and requests identity-bound cleanup of only the recorded file/manifest.
/// Platforms without an identity-bound delete primitive retain quarantine
/// residue and report [`CredentialError::CleanupUncertain`] instead.
pub(crate) struct RetainedKey {
    identity: KeyIdentity,
    path: PathBuf,
    handle: Option<File>,
    file_identity: FileIdentity,
    fingerprint: [u8; 32],
    manifest_path: PathBuf,
    manifest_identity: FileIdentity,
    manifest_fingerprint: [u8; 32],
    store: Arc<StoreInner>,
}

impl RetainedKey {
    pub(crate) fn identity(&self) -> &KeyIdentity {
        &self.identity
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn path_string(&self) -> String {
        self.path.to_string_lossy().into_owned()
    }

    pub(crate) fn revalidate(&self) -> Result<(), CredentialError> {
        self.revalidate_with_deadline(None)
    }

    pub(crate) fn revalidate_until(&self, deadline: Instant) -> Result<(), CredentialError> {
        self.revalidate_with_deadline(Some(deadline))
    }

    fn revalidate_with_deadline(&self, deadline: Option<Instant>) -> Result<(), CredentialError> {
        check_deadline(deadline)?;
        let pinned = match deadline {
            Some(deadline) => PinnedFile::open_until(&self.path, deadline)?,
            None => PinnedFile::open(&self.path)?,
        };
        if pinned.identity != self.file_identity || pinned.fingerprint() != self.fingerprint {
            return Err(CredentialError::InvalidPath);
        }
        check_deadline(deadline)?;
        Ok(())
    }
}

impl fmt::Debug for RetainedKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RetainedKey")
            .field("identity", &self.identity)
            .field("path", &"REDACTED_PATH")
            .finish()
    }
}

impl Drop for RetainedKey {
    fn drop(&mut self) {
        let record = RetainedRecord {
            key_path: self.path.clone(),
            key_identity: self.file_identity,
            key_fingerprint: self.fingerprint,
            manifest_path: self.manifest_path.clone(),
            manifest_identity: self.manifest_identity,
            manifest_fingerprint: self.manifest_fingerprint,
        };
        if self
            .store
            .cleanup_record(&self.identity, &record, self.handle.as_ref())
            .is_err()
        {
            self.store.record_uncertain_cleanup();
        }
        drop(self.handle.take());
    }
}

#[derive(Debug)]
pub(crate) struct RecoveryReport {
    retained: Vec<RetainedKey>,
    removed_orphans: usize,
    uncertain_cleanups: usize,
}

impl RecoveryReport {
    pub(crate) fn retained(&self) -> &[RetainedKey] {
        &self.retained
    }

    pub(crate) fn removed_orphans(&self) -> usize {
        self.removed_orphans
    }

    pub(crate) fn uncertain_cleanup_count(&self) -> usize {
        self.uncertain_cleanups
    }
}

struct StoreInner {
    root: PathBuf,
    records: Mutex<BTreeMap<KeyIdentity, RetainedRecord>>,
    uncertain_cleanups: AtomicU64,
    icacls: Option<Arc<PinnedFile>>,
    operation_lock: Arc<Mutex<()>>,
    #[cfg(test)]
    materialize_failure: Mutex<Option<MaterializeFailurePoint>>,
}

#[derive(Clone, PartialEq, Eq)]
struct RetainedRecord {
    key_path: PathBuf,
    key_identity: FileIdentity,
    key_fingerprint: [u8; 32],
    manifest_path: PathBuf,
    manifest_identity: FileIdentity,
    manifest_fingerprint: [u8; 32],
}

fn root_operation_lock(root: &Path) -> Result<Arc<Mutex<()>>, CredentialError> {
    let locks = ROOT_OPERATION_LOCKS.get_or_init(|| Mutex::new(BTreeMap::new()));
    let mut locks = locks.lock().map_err(|_| CredentialError::Io)?;
    if let Some(lock) = locks.get(root).and_then(Weak::upgrade) {
        return Ok(lock);
    }
    let lock = Arc::new(Mutex::new(()));
    locks.insert(root.to_path_buf(), Arc::downgrade(&lock));
    Ok(lock)
}

/// Explicit, bounded directory for temporary pasted keys.
#[derive(Clone)]
pub(crate) struct KeyMaterialStore {
    inner: Arc<StoreInner>,
}

impl fmt::Debug for KeyMaterialStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("KeyMaterialStore")
            .field("root", &"REDACTED_PATH")
            .field("capacity", &MAX_RECOVERY_RECORDS)
            .finish()
    }
}

impl KeyMaterialStore {
    pub(crate) fn new(root: impl AsRef<Path>) -> Result<Self, CredentialError> {
        ensure_supported_runtime()?;
        let root = root.as_ref();
        if root.as_os_str().is_empty() || native_path_length(root) > MAX_PINNED_PATH_BYTES {
            return Err(CredentialError::InvalidPath);
        }
        fs::create_dir_all(root).map_err(|_| CredentialError::Io)?;
        let root_metadata = fs::symlink_metadata(root).map_err(|_| CredentialError::Io)?;
        if is_windows_reparse_metadata(&root_metadata) || root_metadata.file_type().is_symlink() {
            return Err(CredentialError::InvalidPath);
        }
        let root = fs::canonicalize(root).map_err(|_| CredentialError::Io)?;
        if native_path_length(&root) > MAX_PINNED_PATH_BYTES {
            return Err(CredentialError::InvalidPath);
        }
        let operation_lock = root_operation_lock(&root)?;
        let icacls = retain_icacls_authority()?;
        lock_directory_permissions(&root, icacls.as_ref()).map_err(|_| CredentialError::Io)?;
        Ok(Self {
            inner: Arc::new(StoreInner {
                root,
                records: Mutex::new(BTreeMap::new()),
                uncertain_cleanups: AtomicU64::new(0),
                icacls: icacls.map(Arc::new),
                operation_lock,
                #[cfg(test)]
                materialize_failure: Mutex::new(None),
            }),
        })
    }

    pub(crate) fn root(&self) -> &Path {
        &self.inner.root
    }

    #[cfg(test)]
    fn inject_materialize_failure(&self, point: MaterializeFailurePoint) {
        *self.inner.materialize_failure.lock().unwrap() = Some(point);
    }

    pub(crate) fn materialize(
        &self,
        identity: &KeyIdentity,
        secret: &CredentialSecret,
    ) -> Result<RetainedKey, CredentialError> {
        self.materialize_with_deadline(identity, secret, None)
    }

    pub(crate) fn materialize_until(
        &self,
        identity: &KeyIdentity,
        secret: &CredentialSecret,
        deadline: Instant,
    ) -> Result<RetainedKey, CredentialError> {
        self.materialize_with_deadline(identity, secret, Some(deadline))
    }

    fn materialize_with_deadline(
        &self,
        identity: &KeyIdentity,
        secret: &CredentialSecret,
        deadline: Option<Instant>,
    ) -> Result<RetainedKey, CredentialError> {
        // Pasted key material is deliberately materialized into this
        // restrictive file.  The custody guarantee is bounded lifetime,
        // canonical identity, and cleanup—not a claim that bytes never touch
        // the filesystem.
        ensure_supported_runtime()?;
        check_deadline(deadline)?;
        if secret.kind() != CredentialKind::PrivateKey {
            return Err(CredentialError::WrongKind {
                expected: CredentialKind::PrivateKey,
                actual: secret.kind(),
            });
        }
        secret.validate()?;
        let normalized = sanitize_private_key(secret.bytes())?;
        check_deadline(deadline)?;
        let _operation_guard = lock_mutex_until(&self.inner.operation_lock, deadline)?;
        {
            let records = lock_mutex_until(&self.inner.records, deadline)?;
            check_deadline(deadline)?;
            if records.len() >= MAX_RECOVERY_RECORDS {
                return Err(CredentialError::StoreFull);
            }
            if records.contains_key(identity) {
                return Err(CredentialError::AlreadyRetained);
            }
        }

        let path = key_material_path(&self.inner.root, identity, KEY_FILE_SUFFIX)?;
        let manifest_path = key_material_path(&self.inner.root, identity, MANIFEST_SUFFIX)?;
        // Reject a caller-created symlink or special destination before any
        // pasted bytes are materialized.  The no-replace publication below
        // still handles a replacement race as AlreadyRetained.
        reject_symlink_if_present(&path)?;
        reject_symlink_if_present(&manifest_path)?;
        check_deadline(deadline)?;
        let nonce = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let temp_name_length = "ssh-"
            .len()
            .saturating_add(identity.digest.len().saturating_mul(2))
            .saturating_add(1)
            .saturating_add(MAX_PID_TEXT_BYTES)
            .saturating_add(1)
            .saturating_add(MAX_NONCE_TEXT_BYTES)
            .saturating_add(TEMP_SUFFIX.len());
        ensure_child_name_bound(
            &self.inner.root,
            temp_name_length,
            CredentialError::InvalidPath,
        )?;
        let stem = format!("ssh-{}", identity.digest_hex());
        let temp_name = format!("{stem}-{}-{nonce}{TEMP_SUFFIX}", std::process::id());
        let temp_path = join_store_child(&self.inner.root, &temp_name)?;
        let mut key_snapshot = None;
        let mut manifest_snapshot = None;
        let result = (|| {
            let mut options = OpenOptions::new();
            options.read(true).write(true).create_new(true);
            #[cfg(unix)]
            options.mode(0o600);
            let file = options.open(&temp_path).map_err(|_| CredentialError::Io)?;
            // The guard starts immediately after create_new.  ACL, write,
            // flush, sync, identity, and publication failures all retain an
            // identity-bound cleanup owner.
            let mut temp_guard =
                TempFileGuard::new(temp_path.clone(), file, Arc::clone(&self.inner));
            check_deadline(deadline)?;
            #[cfg(test)]
            if self
                .inner
                .take_materialize_failure(MaterializeFailurePoint::Acl)
            {
                return Err(CredentialError::Io);
            }
            lock_file_permissions_until(&temp_path, self.inner.icacls.as_deref(), deadline)?;
            #[cfg(test)]
            if self
                .inner
                .take_materialize_failure(MaterializeFailurePoint::Write)
            {
                return Err(CredentialError::Io);
            }
            temp_guard
                .file_mut()?
                .write_all(&normalized)
                .map_err(|_| CredentialError::Io)?;
            check_deadline(deadline)?;
            #[cfg(test)]
            if self
                .inner
                .take_materialize_failure(MaterializeFailurePoint::Flush)
            {
                return Err(CredentialError::Io);
            }
            temp_guard
                .file_mut()?
                .flush()
                .map_err(|_| CredentialError::Io)?;
            check_deadline(deadline)?;
            #[cfg(test)]
            if self
                .inner
                .take_materialize_failure(MaterializeFailurePoint::Sync)
            {
                return Err(CredentialError::Io);
            }
            temp_guard
                .file_mut()?
                .sync_all()
                .map_err(|_| CredentialError::Io)?;
            check_deadline(deadline)?;
            let (temp_identity, temp_fingerprint) = temp_guard.capture_snapshot_until(deadline)?;
            temp_guard.close_file();
            // Arm destination cleanup before no-replace publication so a
            // deadline/error immediately after publication cannot strand the
            // published key without an identity-bound rollback owner.
            key_snapshot = Some((temp_identity, temp_fingerprint));
            let PublishedFile {
                handle,
                identity: published_identity,
                fingerprint: published_fingerprint,
            } = publish_noreplace_until(&temp_path, &path, deadline)?;
            temp_guard.disarm();
            #[cfg(test)]
            if self
                .inner
                .take_materialize_failure(MaterializeFailurePoint::PostPublicationExpiry)
            {
                return Err(CredentialError::DeadlineExpired);
            }
            // The publication handle is the retained authority.  Reusing it
            // avoids reopening a pathname after the no-replace move.
            check_deadline(deadline)?;
            let key_identity = file_identity(&handle)?;
            let key_fingerprint = fingerprint_file_with_deadline(&handle, deadline)?;
            if key_identity != published_identity
                || key_fingerprint != published_fingerprint
                || key_identity != temp_identity
                || key_fingerprint != temp_fingerprint
            {
                return Err(CredentialError::InvalidPath);
            }
            let manifest = Manifest {
                version: MANIFEST_VERSION,
                digest: identity.digest,
                index: identity.index,
                file_identity_first: key_identity.first,
                file_identity_second: key_identity.second,
                fingerprint: key_fingerprint,
            };
            let manifest_record = write_manifest(
                &manifest_path,
                &manifest,
                self.inner.icacls.as_deref(),
                &self.inner,
                deadline,
            )?;
            manifest_snapshot = Some((manifest_record.identity, manifest_record.fingerprint));
            let mut records = lock_mutex_until(&self.inner.records, deadline)?;
            check_deadline(deadline)?;
            if records.len() >= MAX_RECOVERY_RECORDS {
                return Err(CredentialError::StoreFull);
            }
            records.insert(
                identity.clone(),
                RetainedRecord {
                    key_path: path.clone(),
                    key_identity,
                    key_fingerprint,
                    manifest_path: manifest_path.clone(),
                    manifest_identity: manifest_record.identity,
                    manifest_fingerprint: manifest_record.fingerprint,
                },
            );
            Ok(RetainedKey {
                identity: identity.clone(),
                path: path.clone(),
                handle: Some(handle),
                file_identity: key_identity,
                fingerprint: key_fingerprint,
                manifest_path: manifest_path.clone(),
                manifest_identity: manifest_record.identity,
                manifest_fingerprint: manifest_record.fingerprint,
                store: Arc::clone(&self.inner),
            })
        })();

        if result.is_err() {
            if let Some((identity, fingerprint)) = key_snapshot {
                if remove_exact_snapshot(&path, identity, fingerprint).is_err() {
                    self.inner.record_uncertain_cleanup();
                }
            }
            if let Some((identity, fingerprint)) = manifest_snapshot {
                if remove_exact_snapshot(&manifest_path, identity, fingerprint).is_err() {
                    self.inner.record_uncertain_cleanup();
                }
            }
        }
        result
    }

    pub(crate) fn recover(&self) -> Result<RecoveryReport, CredentialError> {
        ensure_supported_runtime()?;
        let _operation_guard = self
            .inner
            .operation_lock
            .lock()
            .map_err(|_| CredentialError::Io)?;
        let entries: Vec<PathBuf> = fs::read_dir(&self.inner.root)
            .map_err(|_| CredentialError::Io)?
            .take(MAX_RECOVERY_ENTRIES + 1)
            .map(|entry| {
                let entry = entry.map_err(|_| CredentialError::Io)?;
                bounded_entry_path(&self.inner.root, &entry.file_name())
            })
            .collect::<Result<_, _>>()?;
        if entries.len() > MAX_RECOVERY_ENTRIES {
            return Err(CredentialError::StoreFull);
        }
        // Keep handles and records pending until every recovery operation has
        // completed.  If a later entry fails, dropping a partially recovered
        // RetainedKey cannot recursively contend with this operation lock.
        let mut pending: Vec<(KeyIdentity, RetainedRecord, File)> = Vec::new();
        let mut known = std::collections::BTreeSet::new();
        let mut protected = std::collections::BTreeSet::new();
        let mut removed_orphans = 0;
        for path in &entries {
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if name.ends_with(MANIFEST_SUFFIX) {
                let manifest_file = match read_manifest(path) {
                    Ok(value) if value.manifest.version == MANIFEST_VERSION => value,
                    _ => {
                        count_recovery_orphan(&mut removed_orphans)?;
                        remove_unrecognized_entry(path)?;
                        continue;
                    }
                };
                let manifest = manifest_file.manifest;
                let identity = match KeyIdentity::from_manifest(manifest.digest, manifest.index) {
                    Ok(value) => value,
                    Err(_) => {
                        count_recovery_orphan(&mut removed_orphans)?;
                        remove_exact_snapshot(
                            path,
                            manifest_file.identity,
                            manifest_file.fingerprint,
                        )?;
                        continue;
                    }
                };
                let key_path = key_material_path(&self.inner.root, &identity, KEY_FILE_SUFFIX)?;
                let expected_manifest_path =
                    key_material_path(&self.inner.root, &identity, MANIFEST_SUFFIX)?;
                if path != &expected_manifest_path {
                    count_recovery_orphan(&mut removed_orphans)?;
                    remove_exact_snapshot(path, manifest_file.identity, manifest_file.fingerprint)?;
                    continue;
                }
                protected.insert(key_path.clone());
                if reject_symlink_if_present(&key_path).is_err() {
                    self.inner.record_uncertain_cleanup();
                    continue;
                }
                let handle = match open_no_follow(&key_path) {
                    Ok(handle) => handle,
                    Err(_) => {
                        self.inner.record_uncertain_cleanup();
                        continue;
                    }
                };
                lock_file_permissions(&key_path, self.inner.icacls.as_deref())
                    .map_err(|_| CredentialError::Io)?;
                let file_identity = file_identity(&handle)?;
                let fingerprint = fingerprint_file(&handle)?;
                let expected_identity = FileIdentity {
                    first: manifest.file_identity_first,
                    second: manifest.file_identity_second,
                };
                if file_identity != expected_identity || fingerprint != manifest.fingerprint {
                    drop(handle);
                    self.inner.record_uncertain_cleanup();
                    continue;
                }
                if pending.len() >= MAX_RECOVERY_RECORDS {
                    return Err(CredentialError::StoreFull);
                }
                known.insert(key_path.clone());
                let record = RetainedRecord {
                    key_path,
                    key_identity: file_identity,
                    key_fingerprint: fingerprint,
                    manifest_path: path.clone(),
                    manifest_identity: manifest_file.identity,
                    manifest_fingerprint: manifest_file.fingerprint,
                };
                pending.push((identity, record, handle));
            }
        }

        for path in &entries {
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let orphan = name.starts_with("ssh-")
                && (name.ends_with(TEMP_SUFFIX)
                    || name.ends_with(BACKUP_SUFFIX)
                    || (name.ends_with(KEY_FILE_SUFFIX)
                        && !known.contains(path)
                        && !protected.contains(path)));
            if orphan {
                count_recovery_orphan(&mut removed_orphans)?;
                remove_unrecognized_entry(path)?;
            }
        }
        let mut retained = Vec::with_capacity(pending.len());
        {
            let mut records = self.inner.records.lock().map_err(|_| CredentialError::Io)?;
            if records.len().saturating_add(pending.len()) > MAX_RECOVERY_RECORDS {
                return Err(CredentialError::StoreFull);
            }
            for (identity, record, handle) in pending {
                records.insert(identity.clone(), record.clone());
                retained.push(RetainedKey {
                    identity,
                    path: record.key_path.clone(),
                    handle: Some(handle),
                    file_identity: record.key_identity,
                    fingerprint: record.key_fingerprint,
                    manifest_path: record.manifest_path.clone(),
                    manifest_identity: record.manifest_identity,
                    manifest_fingerprint: record.manifest_fingerprint,
                    store: Arc::clone(&self.inner),
                });
            }
        }
        retained.sort_by(|left, right| left.identity.cmp(&right.identity));
        Ok(RecoveryReport {
            retained,
            removed_orphans,
            uncertain_cleanups: self.inner.uncertain_cleanups.swap(0, Ordering::AcqRel) as usize,
        })
    }

    /// Cleanup is record-based: the caller supplies an opaque identity and we
    /// only use the path previously retained in this store's ledger.
    pub(crate) fn cleanup(&self, identity: &KeyIdentity) -> Result<(), CredentialError> {
        let record = self
            .inner
            .records
            .lock()
            .map_err(|_| CredentialError::Io)?
            .get(identity)
            .cloned()
            .ok_or(CredentialError::NotRetained)?;
        self.inner.cleanup_record(identity, &record, None)
    }
}

fn count_recovery_orphan(count: &mut usize) -> Result<(), CredentialError> {
    *count = count.checked_add(1).ok_or(CredentialError::StoreFull)?;
    if *count > MAX_RECOVERY_ORPHANS {
        Err(CredentialError::StoreFull)
    } else {
        Ok(())
    }
}

impl StoreInner {
    fn record_uncertain_cleanup(&self) {
        self.uncertain_cleanups.fetch_add(1, Ordering::AcqRel);
    }

    #[cfg(test)]
    fn take_materialize_failure(&self, point: MaterializeFailurePoint) -> bool {
        let mut failure = self.materialize_failure.lock().unwrap();
        if *failure == Some(point) {
            *failure = None;
            true
        } else {
            false
        }
    }

    fn cleanup_record(
        &self,
        identity: &KeyIdentity,
        record: &RetainedRecord,
        retained_key_handle: Option<&File>,
    ) -> Result<(), CredentialError> {
        let _operation_guard = self
            .operation_lock
            .lock()
            .map_err(|_| CredentialError::Io)?;
        self.cleanup_record_locked(identity, record, retained_key_handle)
    }

    fn cleanup_record_locked(
        &self,
        identity: &KeyIdentity,
        record: &RetainedRecord,
        retained_key_handle: Option<&File>,
    ) -> Result<(), CredentialError> {
        let recorded = self
            .records
            .lock()
            .map_err(|_| CredentialError::Io)?
            .get(identity)
            .cloned()
            .ok_or(CredentialError::NotRetained)?;
        if recorded != *record
            || record.key_path.parent() != Some(self.root.as_path())
            || record.manifest_path.parent() != Some(self.root.as_path())
        {
            return Err(CredentialError::InvalidPath);
        }
        // The key is the retained authority.  If its pathname is missing or
        // no longer names the retained inode, keep both the record and its
        // manifest visible for recovery; never erase the ledger after a
        // partial cleanup.
        remove_opened_snapshot(
            &record.key_path,
            retained_key_handle,
            record.key_identity,
            record.key_fingerprint,
        )?;
        remove_opened_snapshot(
            &record.manifest_path,
            None,
            record.manifest_identity,
            record.manifest_fingerprint,
        )?;
        self.records
            .lock()
            .map_err(|_| CredentialError::Io)?
            .remove(identity);
        Ok(())
    }
}

fn remove_exact_snapshot(
    path: &Path,
    expected_identity: FileIdentity,
    expected_fingerprint: [u8; 32],
) -> Result<(), CredentialError> {
    remove_opened_snapshot(path, None, expected_identity, expected_fingerprint)
}

fn remove_opened_snapshot(
    path: &Path,
    retained_handle: Option<&File>,
    expected_identity: FileIdentity,
    expected_fingerprint: [u8; 32],
) -> Result<(), CredentialError> {
    remove_opened_entry(
        path,
        retained_handle,
        expected_identity,
        Some(expected_fingerprint),
    )
}

fn remove_opened_entry(
    path: &Path,
    retained_handle: Option<&File>,
    expected_identity: FileIdentity,
    expected_fingerprint: Option<[u8; 32]>,
) -> Result<(), CredentialError> {
    if let Some(handle) = retained_handle {
        if file_identity(handle).map_err(|_| CredentialError::CleanupUncertain)?
            != expected_identity
        {
            return Err(CredentialError::CleanupUncertain);
        }
        if expected_fingerprint
            .is_some_and(|fingerprint| fingerprint_file(handle).ok() != Some(fingerprint))
        {
            return Err(CredentialError::CleanupUncertain);
        }
    }
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return if retained_handle.is_some() {
                Err(CredentialError::CleanupUncertain)
            } else {
                Ok(())
            };
        }
        Err(_) => return Err(CredentialError::CleanupUncertain),
    };
    if is_windows_reparse_metadata(&metadata) || !metadata.file_type().is_file() {
        return Err(CredentialError::CleanupUncertain);
    }
    let current = open_no_follow(path).map_err(|_| CredentialError::CleanupUncertain)?;
    if file_identity(&current).map_err(|_| CredentialError::CleanupUncertain)? != expected_identity
    {
        return Err(CredentialError::CleanupUncertain);
    }
    if expected_fingerprint
        .is_some_and(|fingerprint| fingerprint_file(&current).ok() != Some(fingerprint))
    {
        return Err(CredentialError::CleanupUncertain);
    }
    maybe_test_cleanup_barrier();
    delete_verified_file(path, expected_identity, expected_fingerprint)
}

#[cfg(test)]
fn maybe_test_cleanup_barrier() {
    let barrier = TEST_CLEANUP_BARRIER
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap()
        .take();
    if let Some((identity_checked, delete_started)) = barrier.as_deref() {
        identity_checked.wait();
        delete_started.wait();
    }
}

#[cfg(not(test))]
fn maybe_test_cleanup_barrier() {}

fn remove_unrecognized_entry(path: &Path) -> Result<(), CredentialError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err(CredentialError::CleanupUncertain),
    };
    if is_windows_reparse_metadata(&metadata) || metadata.file_type().is_symlink() {
        let expected_identity = entry_identity(&metadata)?;
        maybe_test_unrecognized_swap_barrier();
        return delete_directory_entry(path, expected_identity);
    }
    if !metadata.file_type().is_file() {
        return Err(CredentialError::CleanupUncertain);
    }
    let handle = open_no_follow(path).map_err(|_| CredentialError::CleanupUncertain)?;
    let identity = file_identity(&handle).map_err(|_| CredentialError::CleanupUncertain)?;
    let fingerprint = if path
        .extension()
        .is_some_and(|extension| extension == "json")
    {
        let length = handle
            .metadata()
            .map_err(|_| CredentialError::CleanupUncertain)?
            .len();
        if length > MAX_MANIFEST_BYTES as u64 {
            None
        } else {
            Some(hash_bounded_manifest(&handle).map_err(|_| CredentialError::CleanupUncertain)?)
        }
    } else {
        Some(fingerprint_file(&handle).map_err(|_| CredentialError::CleanupUncertain)?)
    };
    remove_opened_entry(path, Some(&handle), identity, fingerprint)
}

fn delete_verified_file(
    path: &Path,
    expected_identity: FileIdentity,
    expected_fingerprint: Option<[u8; 32]>,
) -> Result<(), CredentialError> {
    #[cfg(unix)]
    {
        let quarantine = quarantine_verified_file(path, expected_identity, expected_fingerprint)?;
        verify_quarantined_file(&quarantine, expected_identity, expected_fingerprint)?;
        maybe_test_pre_unlink_swap_barrier(&quarantine);
        // Unix has no portable identity-bound unlink primitive.  Never turn
        // the final identity check into a pathname unlink: retain the
        // quarantine and surface uncertainty for host-owned recovery.
        verify_quarantined_file(&quarantine, expected_identity, expected_fingerprint)?;
        Err(CredentialError::CleanupUncertain)
    }
    #[cfg(windows)]
    {
        let current = open_delete_handle(path)?;
        if file_identity(&current).map_err(|_| CredentialError::CleanupUncertain)?
            != expected_identity
        {
            return Err(CredentialError::CleanupUncertain);
        }
        if expected_fingerprint
            .is_some_and(|fingerprint| fingerprint_file(&current).ok() != Some(fingerprint))
        {
            return Err(CredentialError::CleanupUncertain);
        }
        delete_handle(&current).map_err(|_| CredentialError::CleanupUncertain)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let current = open_no_follow(path).map_err(|_| CredentialError::CleanupUncertain)?;
        if file_identity(&current).map_err(|_| CredentialError::CleanupUncertain)?
            != expected_identity
        {
            return Err(CredentialError::CleanupUncertain);
        }
        if expected_fingerprint
            .is_some_and(|fingerprint| fingerprint_file(&current).ok() != Some(fingerprint))
        {
            return Err(CredentialError::CleanupUncertain);
        }
        fs::remove_file(path).map_err(|_| CredentialError::CleanupUncertain)
    }
}

#[cfg(all(test, unix))]
fn maybe_test_postcheck_swap_barrier() {
    let barrier = TEST_POSTCHECK_SWAP_BARRIER
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap()
        .take();
    if let Some((verified, swapped)) = barrier.as_deref() {
        verified.wait();
        swapped.wait();
    }
}

#[cfg(not(all(test, unix)))]
fn maybe_test_postcheck_swap_barrier() {}

#[cfg(all(test, unix))]
fn maybe_test_unrecognized_swap_barrier() {
    let barrier = TEST_UNRECOGNIZED_SWAP_BARRIER
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap()
        .take();
    if let Some(barrier) = barrier {
        let _ = barrier.0.send(());
        let _ = barrier.1.recv();
    }
}

#[cfg(not(all(test, unix)))]
fn maybe_test_unrecognized_swap_barrier() {}

#[cfg(all(test, unix))]
fn maybe_test_pre_unlink_swap_barrier(path: &Path) {
    let barrier = TEST_PRE_UNLINK_SWAP_BARRIER
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap()
        .take();
    if let Some(barrier) = barrier {
        let _ = barrier.0.send(path.to_path_buf());
        let _ = barrier.1.recv();
    }
}

#[cfg(not(all(test, unix)))]
fn maybe_test_pre_unlink_swap_barrier(_path: &Path) {}

fn delete_directory_entry(
    path: &Path,
    expected_identity: FileIdentity,
) -> Result<(), CredentialError> {
    #[cfg(unix)]
    {
        let quarantine = quarantine_entry(path, expected_identity)?;
        verify_quarantined_entry(&quarantine, expected_identity)?;
        maybe_test_pre_unlink_swap_barrier(&quarantine);
        verify_quarantined_entry(&quarantine, expected_identity)?;
        // See delete_verified_file: quarantine is the only safe Unix
        // operation here; deleting a pathname after verification is swappable.
        Err(CredentialError::CleanupUncertain)
    }
    #[cfg(windows)]
    {
        let _ = (path, expected_identity);
        // A Windows reparse-point delete requires a handle-level tag and
        // identity proof that this seam does not yet own.  Hold visibly rather
        // than deleting a pathname that may have become a regular file.
        Err(CredentialError::UnsupportedRuntime)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = expected_identity;
        fs::remove_file(path).map_err(|_| CredentialError::CleanupUncertain)
    }
}

#[cfg(unix)]
fn verify_quarantined_file(
    path: &Path,
    expected_identity: FileIdentity,
    expected_fingerprint: Option<[u8; 32]>,
) -> Result<(), CredentialError> {
    let handle = open_no_follow(path).map_err(|_| CredentialError::CleanupUncertain)?;
    if file_identity(&handle).map_err(|_| CredentialError::CleanupUncertain)? != expected_identity {
        return Err(CredentialError::CleanupUncertain);
    }
    if expected_fingerprint
        .is_some_and(|fingerprint| fingerprint_file(&handle).ok() != Some(fingerprint))
    {
        return Err(CredentialError::CleanupUncertain);
    }
    Ok(())
}

#[cfg(unix)]
fn verify_quarantined_entry(
    path: &Path,
    expected_identity: FileIdentity,
) -> Result<(), CredentialError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| CredentialError::CleanupUncertain)?;
    if !metadata.file_type().is_symlink() || entry_identity(&metadata)? != expected_identity {
        return Err(CredentialError::CleanupUncertain);
    }
    Ok(())
}

#[cfg(unix)]
fn quarantine_verified_file(
    path: &Path,
    expected_identity: FileIdentity,
    expected_fingerprint: Option<[u8; 32]>,
) -> Result<PathBuf, CredentialError> {
    for _ in 0..8 {
        let quarantine = quarantine_path(path)?;
        match rename_noreplace(path, &quarantine) {
            Ok(()) => {
                // The path has now left the attacker-controlled name.  Verify
                // the moved inode before the test hook/cleanup; any mismatch
                // remains in quarantine and is surfaced as uncertain.
                let moved =
                    open_no_follow(&quarantine).map_err(|_| CredentialError::CleanupUncertain)?;
                if file_identity(&moved).map_err(|_| CredentialError::CleanupUncertain)?
                    != expected_identity
                {
                    return Err(CredentialError::CleanupUncertain);
                }
                if expected_fingerprint
                    .is_some_and(|fingerprint| fingerprint_file(&moved).ok() != Some(fingerprint))
                {
                    return Err(CredentialError::CleanupUncertain);
                }
                maybe_test_postcheck_swap_barrier();
                return Ok(quarantine);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(_) => return Err(CredentialError::CleanupUncertain),
        }
    }
    Err(CredentialError::CleanupUncertain)
}

#[cfg(unix)]
fn quarantine_entry(
    path: &Path,
    expected_identity: FileIdentity,
) -> Result<PathBuf, CredentialError> {
    for _ in 0..8 {
        // Close the check/replacement window as much as possible before the
        // atomic no-replace move.  The moved entry is checked again below;
        // either mismatch leaves the quarantine visible and fails closed.
        let current = fs::symlink_metadata(path).map_err(|_| CredentialError::CleanupUncertain)?;
        if entry_identity(&current)? != expected_identity {
            return Err(CredentialError::CleanupUncertain);
        }
        let quarantine = quarantine_path(path)?;
        match rename_noreplace(path, &quarantine) {
            Ok(()) => {
                let moved = fs::symlink_metadata(&quarantine)
                    .map_err(|_| CredentialError::CleanupUncertain)?;
                if entry_identity(&moved)? != expected_identity || !moved.file_type().is_symlink() {
                    return Err(CredentialError::CleanupUncertain);
                }
                return Ok(quarantine);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(_) => return Err(CredentialError::CleanupUncertain),
        }
    }
    Err(CredentialError::CleanupUncertain)
}

#[cfg(unix)]
fn quarantine_path(path: &Path) -> Result<PathBuf, CredentialError> {
    let parent = path.parent().ok_or(CredentialError::CleanupUncertain)?;
    let name = path.file_name().ok_or(CredentialError::CleanupUncertain)?;
    // Check the worst-case generated name before allocating an owned path
    // component.  The decimal bounds cover the process id and the monotonic
    // nonce even on targets where their concrete widths differ.
    const QUARANTINE_FIXED_BYTES: usize = 1 + ".quarantine-".len() + 1;
    ensure_child_name_bound(
        parent,
        native_os_str_length(name)
            .saturating_add(QUARANTINE_FIXED_BYTES)
            .saturating_add(MAX_PID_TEXT_BYTES)
            .saturating_add(MAX_NONCE_TEXT_BYTES),
        CredentialError::CleanupUncertain,
    )?;

    let mut quarantine_name = OsString::from(".");
    quarantine_name.push(name);
    quarantine_name.push(".quarantine-");
    quarantine_name.push(std::process::id().to_string());
    quarantine_name.push("-");
    quarantine_name.push(TEMP_COUNTER.fetch_add(1, Ordering::Relaxed).to_string());
    join_store_child_os(parent, &quarantine_name, CredentialError::CleanupUncertain)
}

#[cfg(unix)]
fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    #[cfg(all(
        unix,
        not(any(
            target_os = "linux",
            target_os = "android",
            target_os = "macos",
            target_os = "ios"
        ))
    ))]
    {
        let _ = (from, to);
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Unix target lacks an atomic no-replace quarantine primitive",
        ));
    }

    let from_parent = from
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "entry has no parent"))?;
    let to_parent = to
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "entry has no parent"))?;
    if from_parent != to_parent {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "quarantine must stay in its parent directory",
        ));
    }
    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags(no_follow_flag() | directory_flag());
    let directory = options.open(from_parent)?;
    let old_name = CString::new(
        from.file_name()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "entry has no name"))?
            .as_bytes(),
    )
    .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "entry name contains NUL"))?;
    let new_name = CString::new(
        to.file_name()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "entry has no name"))?
            .as_bytes(),
    )
    .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "entry name contains NUL"))?;

    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        // renameat2(RENAME_NOREPLACE) is the OS quarantine primitive: moving
        // the checked entry is one atomic directory operation and cannot
        // replace an attacker-precreated destination.
        let result = unsafe {
            renameat2(
                directory.as_raw_fd(),
                old_name.as_ptr(),
                directory.as_raw_fd(),
                new_name.as_ptr(),
                RENAME_NOREPLACE,
            )
        };
        if result == 0 {
            return Ok(());
        }
        return Err(io::Error::last_os_error());
    }

    #[cfg(any(target_os = "macos", target_os = "ios"))]
    {
        let _ = (directory, old_name, new_name);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "platform no-replace rename is not wired for this target",
        ))
    }
}

#[cfg(unix)]
fn directory_flag() -> i32 {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        0x0001_0000
    }
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    {
        0x0010_0000
    }
    #[cfg(all(
        unix,
        not(any(
            target_os = "linux",
            target_os = "android",
            target_os = "macos",
            target_os = "ios"
        ))
    ))]
    {
        0
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
const RENAME_NOREPLACE: u32 = 1;

#[cfg(any(target_os = "linux", target_os = "android"))]
unsafe extern "C" {
    fn renameat2(
        olddirfd: std::os::fd::RawFd,
        oldpath: *const std::ffi::c_char,
        newdirfd: std::os::fd::RawFd,
        newpath: *const std::ffi::c_char,
        flags: u32,
    ) -> i32;
}

#[cfg(windows)]
fn open_delete_handle(path: &Path) -> Result<File, CredentialError> {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::FromRawHandle;
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_READ,
        FILE_SHARE_DELETE, FILE_SHARE_MODE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };

    if native_path_length(path) > MAX_PINNED_PATH_BYTES {
        return Err(CredentialError::InvalidPath);
    }
    let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let share = FILE_SHARE_MODE(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0 | FILE_SHARE_DELETE.0);
    let handle = unsafe {
        CreateFileW(
            PCWSTR(path.as_ptr()),
            FILE_GENERIC_READ.0 | 0x0001_0000,
            share,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(FILE_FLAG_OPEN_REPARSE_POINT.0),
            None,
        )
    }
    .map_err(|_| CredentialError::CleanupUncertain)?;
    let file = unsafe { File::from_raw_handle(handle.0 as *mut _) };
    let metadata = file
        .metadata()
        .map_err(|_| CredentialError::CleanupUncertain)?;
    if is_windows_reparse_metadata(&metadata) || !metadata.is_file() {
        return Err(CredentialError::UnsupportedRuntime);
    }
    Ok(file)
}

#[cfg(windows)]
fn delete_handle(handle: &File) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Storage::FileSystem::{
        FileDispositionInfo, SetFileInformationByHandle, FILE_DISPOSITION_INFO,
    };

    let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
    unsafe {
        SetFileInformationByHandle(
            windows::Win32::Foundation::HANDLE(handle.as_raw_handle() as *mut _),
            FileDispositionInfo,
            (&disposition as *const FILE_DISPOSITION_INFO).cast(),
            std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
        .map_err(|error| io::Error::from_raw_os_error(error.code().0))
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct Manifest {
    version: u8,
    digest: [u8; 32],
    index: u32,
    file_identity_first: u64,
    file_identity_second: u64,
    fingerprint: [u8; 32],
}

struct ManifestFile {
    manifest: Manifest,
    identity: FileIdentity,
    fingerprint: [u8; 32],
}

/// Owns a create-new temporary entry from the instant it is created until a
/// no-replace publication has completed.  It is intentionally independent of
/// the caller's stack cleanup so ACL/write/flush/sync failures cannot strand a
/// temp file.  Cleanup is identity-checked and any ambiguity remains visible
/// through the store's uncertainty counter.
struct TempFileGuard {
    path: PathBuf,
    file: Option<File>,
    identity: Option<FileIdentity>,
    fingerprint: Option<[u8; 32]>,
    store: Arc<StoreInner>,
    armed: bool,
}

impl TempFileGuard {
    fn new(path: PathBuf, file: File, store: Arc<StoreInner>) -> Self {
        Self {
            path,
            file: Some(file),
            identity: None,
            fingerprint: None,
            store,
            armed: true,
        }
    }

    fn file_mut(&mut self) -> Result<&mut File, CredentialError> {
        self.file.as_mut().ok_or(CredentialError::Io)
    }

    fn capture_snapshot(&mut self) -> Result<(FileIdentity, [u8; 32]), CredentialError> {
        self.capture_snapshot_until(None)
    }

    fn capture_snapshot_until(
        &mut self,
        deadline: Option<Instant>,
    ) -> Result<(FileIdentity, [u8; 32]), CredentialError> {
        check_deadline(deadline)?;
        let file = self.file.as_ref().ok_or(CredentialError::Io)?;
        let identity = file_identity(file)?;
        self.identity = Some(identity);
        let fingerprint = fingerprint_file_with_deadline(file, deadline)?;
        self.fingerprint = Some(fingerprint);
        check_deadline(deadline)?;
        Ok((identity, fingerprint))
    }

    fn close_file(&mut self) {
        drop(self.file.take());
    }

    fn disarm(&mut self) {
        self.armed = false;
        self.file.take();
    }
}

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let identity = self
            .identity
            .or_else(|| self.file.as_ref().and_then(|file| file_identity(file).ok()));
        let fingerprint = self.fingerprint.or_else(|| {
            self.file
                .as_ref()
                .and_then(|file| fingerprint_file(file).ok())
        });
        let had_retained_handle = self.file.is_some();
        let first_result = identity.map_or(Err(CredentialError::CleanupUncertain), |identity| {
            remove_opened_entry(&self.path, self.file.as_ref(), identity, fingerprint)
        });
        // Close the retained handle before recording uncertainty.  A
        // pathname retry after closing it could erase a replacement (or turn
        // a missing pathname into Ok), so retained-inode failures never retry
        // by name.
        drop(self.file.take());
        if first_result.is_err() {
            if let Some(identity) = identity {
                // If an inode handle was retained, a missing or moved
                // pathname is proof of uncertainty.  Closing it and retrying
                // by pathname would turn that proof into a false Ok.
                if !had_retained_handle
                    && remove_opened_entry(&self.path, None, identity, fingerprint).is_ok()
                {
                    return;
                }
            }
            self.store.record_uncertain_cleanup();
        }
    }
}

struct PublishedFile {
    handle: File,
    identity: FileIdentity,
    fingerprint: [u8; 32],
}

fn publish_noreplace(temp: &Path, destination: &Path) -> Result<PublishedFile, CredentialError> {
    publish_noreplace_until(temp, destination, None)
}

fn publish_noreplace_until(
    temp: &Path,
    destination: &Path,
    deadline: Option<Instant>,
) -> Result<PublishedFile, CredentialError> {
    check_deadline(deadline)?;
    let source = open_no_follow_until(temp, deadline)?;
    let identity = file_identity(&source)?;
    let fingerprint = fingerprint_file_with_deadline(&source, deadline)?;
    check_deadline(deadline)?;

    #[cfg(unix)]
    {
        // Rename the already-opened publication inode into place.  This
        // avoids a post-publication pathname unlink of the temporary secret;
        // the source handle remains the retained identity authority.
        rename_noreplace(temp, destination).map_err(|error| {
            if error.kind() == io::ErrorKind::AlreadyExists {
                CredentialError::AlreadyRetained
            } else if error.kind() == io::ErrorKind::Unsupported {
                CredentialError::UnsupportedRuntime
            } else {
                CredentialError::Io
            }
        })?;
        check_deadline(deadline)?;
        return Ok(PublishedFile {
            handle: source,
            identity,
            fingerprint,
        });
    }

    #[cfg(not(unix))]
    {
        fs::hard_link(temp, destination).map_err(|error| {
            if error.kind() == io::ErrorKind::AlreadyExists {
                CredentialError::AlreadyRetained
            } else {
                CredentialError::Io
            }
        })?;
        check_deadline(deadline)?;
        if let Err(error) = remove_opened_snapshot(temp, Some(&source), identity, fingerprint) {
            let _ = remove_exact_snapshot(destination, identity, fingerprint);
            return Err(error);
        }
        Ok(PublishedFile {
            handle: source,
            identity,
            fingerprint,
        })
    }
}

fn write_manifest(
    path: &Path,
    manifest: &Manifest,
    icacls: Option<&PinnedFile>,
    store: &Arc<StoreInner>,
    deadline: Option<Instant>,
) -> Result<PublishedFile, CredentialError> {
    check_deadline(deadline)?;
    let bytes = serde_json::to_vec(manifest).map_err(|_| CredentialError::Io)?;
    if bytes.len() > MAX_MANIFEST_BYTES {
        return Err(CredentialError::Io);
    }
    let nonce = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let file_name_os = path.file_name().ok_or(CredentialError::InvalidPath)?;
    let file_name = file_name_os.to_str().ok_or(CredentialError::InvalidPath)?;
    let parent = path.parent().ok_or(CredentialError::InvalidPath)?;
    ensure_child_name_bound(
        parent,
        native_os_str_length(file_name_os)
            .saturating_add(1)
            .saturating_add(MAX_NONCE_TEXT_BYTES)
            .saturating_add(TEMP_SUFFIX.len()),
        CredentialError::InvalidPath,
    )?;
    let temp_name = format!("{file_name}-{nonce}{TEMP_SUFFIX}");
    let temp = join_store_child(parent, &temp_name)?;
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut published_snapshot = None;
    let result = (|| {
        let file = options.open(&temp).map_err(|_| CredentialError::Io)?;
        // Begin custody immediately after create_new so every later failure
        // (including ACL, durability, or publication) has a cleanup owner.
        let mut temp_guard = TempFileGuard::new(temp.clone(), file, Arc::clone(store));
        check_deadline(deadline)?;
        lock_file_permissions_until(&temp, icacls, deadline)?;
        temp_guard
            .file_mut()?
            .write_all(&bytes)
            .map_err(|_| CredentialError::Io)?;
        check_deadline(deadline)?;
        temp_guard
            .file_mut()?
            .flush()
            .map_err(|_| CredentialError::Io)?;
        check_deadline(deadline)?;
        temp_guard
            .file_mut()?
            .sync_all()
            .map_err(|_| CredentialError::Io)?;
        check_deadline(deadline)?;
        let snapshot = temp_guard.capture_snapshot_until(deadline)?;
        temp_guard.close_file();
        // Arm manifest-destination rollback before no-replace publication so a
        // deadline/error after publication cannot strand a manifest.
        published_snapshot = Some(snapshot);
        let published = publish_noreplace_until(&temp, path, deadline)?;
        temp_guard.disarm();
        Ok(published)
    })();
    if result.is_err() {
        if let Some((identity, fingerprint)) = published_snapshot {
            if remove_exact_snapshot(path, identity, fingerprint).is_err() {
                store.record_uncertain_cleanup();
            }
        }
    }
    result
}

fn read_manifest(path: &Path) -> Result<ManifestFile, CredentialError> {
    let mut file = open_no_follow(path)?;
    let identity = file_identity(&file)?;
    let metadata = file.metadata().map_err(|_| CredentialError::Io)?;
    if !metadata.is_file() {
        return Err(CredentialError::InvalidPath);
    }
    if metadata.len() > MAX_MANIFEST_BYTES as u64 {
        return Err(CredentialError::StoreFull);
    }
    let mut bytes = Vec::with_capacity(
        usize::try_from(metadata.len()).map_err(|_| CredentialError::StoreFull)?,
    );
    file.seek(SeekFrom::Start(0))
        .map_err(|_| CredentialError::Io)?;
    file.take((MAX_MANIFEST_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| CredentialError::Io)?;
    if bytes.len() > MAX_MANIFEST_BYTES {
        return Err(CredentialError::StoreFull);
    }
    let manifest = serde_json::from_slice(&bytes).map_err(|_| CredentialError::Io)?;
    let fingerprint: [u8; 32] = Sha256::digest(&bytes).into();
    Ok(ManifestFile {
        manifest,
        identity,
        fingerprint,
    })
}

fn hash_bounded_manifest(file: &File) -> Result<[u8; 32], CredentialError> {
    let metadata = file.metadata().map_err(|_| CredentialError::Io)?;
    if !metadata.is_file() || metadata.len() > MAX_MANIFEST_BYTES as u64 {
        return Err(CredentialError::StoreFull);
    }
    let mut reader = file.try_clone().map_err(|_| CredentialError::Io)?;
    reader
        .seek(SeekFrom::Start(0))
        .map_err(|_| CredentialError::Io)?;
    let mut bytes = Vec::with_capacity(
        usize::try_from(metadata.len()).map_err(|_| CredentialError::StoreFull)?,
    );
    reader
        .take((MAX_MANIFEST_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| CredentialError::Io)?;
    if bytes.len() > MAX_MANIFEST_BYTES {
        return Err(CredentialError::StoreFull);
    }
    Ok(Sha256::digest(&bytes).into())
}

/// Normalize directly into a zeroizing buffer.  No intermediate `String`
/// contains pasted key material.
pub(crate) fn sanitize_private_key(value: &[u8]) -> Result<Zeroizing<Vec<u8>>, CredentialError> {
    if value.is_empty() || value.len() > MAX_KEY_TEXT_BYTES {
        return Err(CredentialError::SecretTooLarge);
    }
    if value
        .iter()
        .copied()
        .any(|byte| byte.is_ascii_control() && !matches!(byte, b'\r' | b'\n' | b'\t'))
    {
        return Err(CredentialError::InvalidSecretMaterial);
    }
    let mut normalized = Zeroizing::new(Vec::with_capacity(value.len() + 1));
    let mut at_start = true;
    let bytes = value;
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b'\r' {
            if bytes.get(index + 1) == Some(&b'\n') {
                index += 1;
            }
            if !at_start {
                normalized.push(b'\n');
            }
        } else if at_start && matches!(byte, b'\n' | b'\t' | b' ') {
            // discard leading formatting whitespace
        } else {
            normalized.push(byte);
            at_start = false;
        }
        index += 1;
    }
    while normalized
        .last()
        .is_some_and(|byte| matches!(*byte, b'\n' | b'\t' | b' '))
    {
        normalized.pop();
    }
    if normalized.is_empty() || !has_matching_private_key_markers(&normalized) {
        return Err(CredentialError::InvalidSecretMaterial);
    }
    normalized.push(b'\n');
    Ok(normalized)
}

fn has_matching_private_key_markers(value: &[u8]) -> bool {
    const BEGIN: &[u8] = b"-----BEGIN ";
    const END: &[u8] = b"-----END ";
    const DASHES: &[u8] = b"-----";
    if !value.starts_with(BEGIN) || !value.ends_with(DASHES) {
        return false;
    }
    let begin_label_end = value[BEGIN.len()..]
        .windows(DASHES.len())
        .position(|window| window == DASHES)
        .map(|offset| BEGIN.len() + offset);
    let Some(begin_label_end) = begin_label_end else {
        return false;
    };
    let Some(end_start) = value.windows(END.len()).rposition(|window| window == END) else {
        return false;
    };
    let end_label_start = end_start + END.len();
    let end_label_end = value.len() - DASHES.len();
    begin_label_end > BEGIN.len()
        && end_label_end > end_label_start
        && value[BEGIN.len()..begin_label_end].ends_with(b"PRIVATE KEY")
        && value[BEGIN.len()..begin_label_end] == value[end_label_start..end_label_end]
}

fn reject_symlink_if_present(path: &Path) -> Result<(), CredentialError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if is_windows_reparse_metadata(&metadata) => Err(CredentialError::InvalidPath),
        Ok(metadata) if !metadata.file_type().is_file() => Err(CredentialError::InvalidPath),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(CredentialError::Io),
    }
}

#[cfg(windows)]
fn is_windows_reparse_attributes(attributes: u32) -> bool {
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(windows)]
fn is_windows_reparse_metadata(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    is_windows_reparse_attributes(metadata.file_attributes())
}

#[cfg(not(windows))]
fn is_windows_reparse_metadata(_metadata: &fs::Metadata) -> bool {
    false
}

fn open_no_follow(path: &Path) -> Result<File, CredentialError> {
    reject_symlink_if_present(path)?;
    #[cfg(all(
        unix,
        not(any(
            target_os = "linux",
            target_os = "android",
            target_os = "macos",
            target_os = "ios"
        ))
    ))]
    {
        let _ = path;
        return Err(CredentialError::CleanupUncertain);
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    // O_NONBLOCK closes the metadata/open race for FIFOs and other special
    // objects: a replaced path must fail promptly instead of blocking a
    // launch or recovery thread before the type check can report it.
    options.custom_flags(no_follow_flag() | nonblocking_flag());
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_FLAG_OPEN_REPARSE_POINT keeps a race-created reparse point
        // from being followed between the metadata check and the open.
        options.custom_flags(0x0020_0000);
    }
    let file = options.open(path).map_err(|_| CredentialError::Io)?;
    let metadata = file.metadata().map_err(|_| CredentialError::Io)?;
    if is_windows_reparse_metadata(&metadata) || !metadata.is_file() {
        return Err(CredentialError::InvalidPath);
    }
    Ok(file)
}

fn open_no_follow_until(path: &Path, deadline: Option<Instant>) -> Result<File, CredentialError> {
    check_deadline(deadline)?;
    let file = open_no_follow(path)?;
    check_deadline(deadline)?;
    Ok(file)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn no_follow_flag() -> i32 {
    // O_NOFOLLOW from Linux fcntl.h; keeping it local avoids a broad runtime
    // dependency solely for this narrow custody boundary.
    0x20000
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn no_follow_flag() -> i32 {
    0x100
}

#[cfg(all(
    unix,
    not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios"
    ))
))]
fn no_follow_flag() -> i32 {
    panic!("Unix target lacks a verified no-follow primitive")
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn nonblocking_flag() -> i32 {
    // O_NONBLOCK from Linux fcntl.h.
    0x800
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn nonblocking_flag() -> i32 {
    // O_NONBLOCK from Darwin fcntl.h.
    0x4
}

#[cfg(all(
    unix,
    not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios"
    ))
))]
fn nonblocking_flag() -> i32 {
    panic!("Unix target lacks a verified nonblocking primitive")
}

#[cfg(unix)]
fn lock_directory_permissions(path: &Path, _icacls: Option<&PinnedFile>) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

#[cfg(unix)]
fn lock_file_permissions(path: &Path, _icacls: Option<&PinnedFile>) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(windows)]
fn lock_permissions(path: &Path, icacls: Option<&PinnedFile>) -> io::Result<()> {
    use std::process::Command;
    let username = std::env::var("USERNAME")
        .map_err(|_| io::Error::new(io::ErrorKind::PermissionDenied, "current user unavailable"))?;
    let authority = icacls.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            "icacls authority unavailable",
        )
    })?;
    authority
        .revalidate()
        .map_err(|_| io::Error::new(io::ErrorKind::PermissionDenied, "icacls authority changed"))?;
    let output = Command::new(authority.path())
        .arg(path)
        .arg("/inheritance:r")
        .arg("/grant:r")
        .arg(format!("{username}:F"))
        .output()?;
    if output.status.success() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "icacls rejected restrictive ACL",
        ))
    }
}

#[cfg(windows)]
fn lock_directory_permissions(path: &Path, icacls: Option<&PinnedFile>) -> io::Result<()> {
    lock_permissions(path, icacls)
}

#[cfg(windows)]
fn lock_file_permissions(path: &Path, icacls: Option<&PinnedFile>) -> io::Result<()> {
    lock_permissions(path, icacls)
}

#[cfg(not(any(unix, windows)))]
fn lock_directory_permissions(_path: &Path, _icacls: Option<&PinnedFile>) -> io::Result<()> {
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn lock_file_permissions(_path: &Path, _icacls: Option<&PinnedFile>) -> io::Result<()> {
    Ok(())
}

#[cfg(not(windows))]
fn lock_file_permissions_until(
    path: &Path,
    icacls: Option<&PinnedFile>,
    deadline: Option<Instant>,
) -> Result<(), CredentialError> {
    check_deadline(deadline)?;
    lock_file_permissions(path, icacls).map_err(|_| CredentialError::Io)?;
    check_deadline(deadline)
}

#[cfg(windows)]
fn lock_file_permissions_until(
    path: &Path,
    icacls: Option<&PinnedFile>,
    deadline: Option<Instant>,
) -> Result<(), CredentialError> {
    use std::process::{Command, Stdio};
    use std::time::Duration;

    check_deadline(deadline)?;
    let username = std::env::var("USERNAME").map_err(|_| CredentialError::Io)?;
    let authority = icacls.ok_or(CredentialError::Io)?;
    if let Some(deadline) = deadline {
        authority.revalidate_until(deadline)?;
    } else {
        authority.revalidate()?;
    }
    let mut child = Command::new(authority.path())
        .arg(path)
        .arg("/inheritance:r")
        .arg("/grant:r")
        .arg(format!("{username}:F"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| CredentialError::Io)?;
    loop {
        if let Some(status) = child.try_wait().map_err(|_| CredentialError::Io)? {
            if status.success() {
                check_deadline(deadline)?;
                return Ok(());
            }
            return Err(CredentialError::Io);
        }
        if deadline.is_some_and(|deadline| deadline <= Instant::now()) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(CredentialError::DeadlineExpired);
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[cfg(windows)]
fn retain_icacls_authority() -> Result<Option<PinnedFile>, CredentialError> {
    let root = std::env::var_os("SystemRoot").ok_or(CredentialError::InvalidPath)?;
    let path = bounded_system_path(&root, &["System32", "icacls.exe"])?;
    Ok(Some(PinnedFile::open(&path)?))
}

#[cfg(not(windows))]
fn retain_icacls_authority() -> Result<Option<PinnedFile>, CredentialError> {
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::de::{self, Visitor};

    struct BorrowedOnlyDeserializer(&'static str);

    impl<'de> serde::Deserializer<'de> for BorrowedOnlyDeserializer {
        type Error = de::value::Error;

        fn deserialize_any<V>(self, visitor: V) -> Result<V::Value, Self::Error>
        where
            V: Visitor<'de>,
        {
            self.deserialize_str(visitor)
        }

        fn deserialize_str<V>(self, visitor: V) -> Result<V::Value, Self::Error>
        where
            V: Visitor<'de>,
        {
            visitor.visit_borrowed_str(self.0)
        }

        fn deserialize_string<V>(self, _visitor: V) -> Result<V::Value, Self::Error>
        where
            V: Visitor<'de>,
        {
            Err(de::Error::custom("owned string allocation forbidden"))
        }

        serde::forward_to_deserialize_any! {
            bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char bytes byte_buf
            option unit unit_struct newtype_struct seq tuple tuple_struct map struct enum
            identifier ignored_any
        }
    }

    #[test]
    fn private_key_sanitization_is_zeroizing_and_strict() {
        let value = sanitize_private_key(
            b"\n-----BEGIN OPENSSH PRIVATE KEY-----\r\nkey\r\n-----END OPENSSH PRIVATE KEY-----\n",
        )
        .expect("sanitized");
        assert_eq!(
            value.as_slice(),
            b"-----BEGIN OPENSSH PRIVATE KEY-----\nkey\n-----END OPENSSH PRIVATE KEY-----\n"
        );
        let crlf_leading = sanitize_private_key(
            b"\r\n-----BEGIN OPENSSH PRIVATE KEY-----\r\nkey\r\n-----END OPENSSH PRIVATE KEY-----\r\n",
        )
        .expect("leading CRLF sanitized");
        assert_eq!(crlf_leading.as_slice(), value.as_slice());
        assert!(sanitize_private_key(b"not-a-key").is_err());
        assert!(sanitize_private_key(
            b"-----BEGIN OPENSSH PRIVATE KEY-----\nkey\n-----END RSA PRIVATE KEY-----"
        )
        .is_err());
        assert!(sanitize_private_key(&vec![b'x'; MAX_KEY_TEXT_BYTES + 1]).is_err());
    }

    #[test]
    fn credential_ref_deserializes_borrowed_and_bounds_before_owned_copy() {
        let reference = CredentialRef::deserialize(BorrowedOnlyDeserializer("credential:bounded"))
            .expect("borrowed reference");
        assert_eq!(reference.as_str(), "credential:bounded");

        let oversized = Box::leak(
            format!("credential:{}", "x".repeat(MAX_CREDENTIAL_REF_BYTES)).into_boxed_str(),
        );
        let error = CredentialRef::deserialize(BorrowedOnlyDeserializer(oversized))
            .expect_err("oversized reference");
        assert_eq!(error.to_string(), "invalid credential reference");
    }

    #[test]
    fn credential_secret_rejects_oversize_before_copying_resolver_bytes() {
        let source = vec![b'x'; MAX_SECRET_BYTES + 1];
        let result = CredentialSecret::from_bytes(CredentialKind::Password, &source);
        assert!(matches!(result, Err(CredentialError::SecretTooLarge)));
    }

    #[test]
    fn key_identity_has_no_deserialization_or_path_authority() {
        let reference = CredentialRef::parse("credential:test-key").expect("reference");
        let identity = KeyIdentity::issue("connection", &reference).expect("identity");
        assert_eq!(
            identity.index(),
            u32::from_be_bytes(identity.digest[..4].try_into().unwrap())
        );
        assert!(!format!("{identity:?}").contains("connection"));
        assert!(!format!("{identity:?}").contains("test-key"));
    }

    #[cfg(unix)]
    #[test]
    fn pinned_file_rejects_symlink_before_canonicalization() {
        let root = tempfile::tempdir().expect("root");
        let target = root.path().join("target");
        let link = root.path().join("link");
        fs::write(&target, b"fixture").expect("target");
        std::os::unix::fs::symlink(&target, &link).expect("link");
        assert!(matches!(
            PinnedFile::open(&link),
            Err(CredentialError::InvalidPath)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn pinned_file_rejects_same_bytes_path_replacement() {
        let root = tempfile::tempdir().expect("root");
        let path = root.path().join("known_hosts");
        let replacement = root.path().join("replacement");
        fs::write(&path, b"known-host fixture\n").expect("original");
        let pinned = PinnedFile::open(&path).expect("pin");
        fs::write(&replacement, b"known-host fixture\n").expect("replacement");
        fs::rename(&replacement, &path).expect("replace");
        assert!(matches!(
            pinned.revalidate(),
            Err(CredentialError::InvalidPath)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn retained_inode_missing_path_keeps_manifest_and_reports_uncertain() {
        let root = tempfile::tempdir().expect("root");
        let store = KeyMaterialStore::new(root.path()).expect("store");
        let reference = CredentialRef::parse("credential:missing-retained-path").expect("ref");
        let identity = KeyIdentity::issue("connection", &reference).expect("identity");
        let secret = CredentialSecret::private_key(
            "-----BEGIN OPENSSH PRIVATE KEY-----\nkey\n-----END OPENSSH PRIVATE KEY-----",
        );
        let retained = store.materialize(&identity, &secret).expect("materialize");
        let key_path = retained.path().to_path_buf();
        let manifest_path = retained.manifest_path.clone();
        fs::remove_file(&key_path).expect("remove retained pathname");

        drop(retained);

        assert!(
            manifest_path.exists(),
            "uncertain cleanup must retain manifest"
        );
        assert!(store
            .inner
            .records
            .lock()
            .expect("records")
            .contains_key(&identity));
        assert!(store.inner.uncertain_cleanups.load(Ordering::Acquire) > 0);
    }

    #[cfg(unix)]
    #[test]
    fn orphan_symlink_swap_is_preserved_and_fails_closed() {
        use std::sync::mpsc;
        use std::time::Duration;

        let root = tempfile::tempdir().expect("root");
        let path = root.path().join("ssh-orphan.tmp");
        let target = root.path().join("target");
        let replacement = b"replacement regular file must survive";
        fs::write(&target, b"target").expect("target");
        std::os::unix::fs::symlink(&target, &path).expect("orphan symlink");

        let (ready_tx, ready_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        *TEST_UNRECOGNIZED_SWAP_BARRIER
            .get_or_init(|| Mutex::new(None))
            .lock()
            .expect("barrier") = Some((ready_tx, release_rx));
        let cleanup_path = path.clone();
        let cleanup = std::thread::spawn(move || remove_unrecognized_entry(&cleanup_path));

        if ready_rx.recv_timeout(Duration::from_millis(250)).is_ok() {
            fs::remove_file(&path).expect("swap symlink");
            fs::write(&path, replacement).expect("replacement");
            release_tx.send(()).expect("release cleanup");
        } else {
            // The pre-fix implementation has no barrier and deletes the
            // entry immediately; joining still makes this a deterministic
            // RED assertion instead of leaving a worker behind.
            let _ = release_tx.send(());
        }

        assert!(matches!(
            cleanup.join().expect("cleanup join"),
            Err(CredentialError::CleanupUncertain)
        ));
        assert_eq!(fs::read(&path).expect("replacement remains"), replacement);
    }

    #[cfg(unix)]
    #[test]
    fn post_verification_quarantine_swap_is_preserved_and_fails_closed() {
        use std::sync::mpsc;
        use std::time::Duration;

        let root = tempfile::tempdir().expect("root");
        let path = root.path().join("retained.key");
        let replacement = b"post-verification replacement must survive";
        fs::write(&path, b"original").expect("original");
        let handle = open_no_follow(&path).expect("handle");
        let identity = file_identity(&handle).expect("identity");
        let fingerprint = fingerprint_file(&handle).expect("fingerprint");
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        *TEST_PRE_UNLINK_SWAP_BARRIER
            .get_or_init(|| Mutex::new(None))
            .lock()
            .expect("barrier") = Some((ready_tx, release_rx));

        let cleanup_path = path.clone();
        let cleanup = std::thread::spawn(move || {
            remove_opened_snapshot(&cleanup_path, Some(&handle), identity, fingerprint)
        });
        if let Ok(quarantine) = ready_rx.recv_timeout(Duration::from_millis(250)) {
            fs::remove_file(&quarantine).expect("swap quarantine");
            fs::write(&quarantine, replacement).expect("replacement");
            release_tx.send(()).expect("release cleanup");
            let result = cleanup.join().expect("cleanup join");
            assert!(matches!(result, Err(CredentialError::CleanupUncertain)));
            assert_eq!(
                fs::read(&quarantine).expect("replacement remains"),
                replacement
            );
        } else {
            let _ = release_tx.send(());
            assert!(matches!(
                cleanup.join().expect("cleanup join"),
                Err(CredentialError::CleanupUncertain)
            ));
        }
    }

    #[cfg(unix)]
    #[test]
    fn unix_cleanup_never_unlinks_after_identity_verification() {
        let root = tempfile::tempdir().expect("root");
        let path = root.path().join("retained.key");
        fs::write(&path, b"original").expect("original");
        let handle = open_no_follow(&path).expect("handle");
        let identity = file_identity(&handle).expect("identity");
        let fingerprint = fingerprint_file(&handle).expect("fingerprint");

        let result = remove_opened_snapshot(&path, Some(&handle), identity, fingerprint);

        assert!(matches!(result, Err(CredentialError::CleanupUncertain)));
        assert!(!path.exists(), "the original name must be quarantined");
        assert!(
            fs::read_dir(root.path())
                .expect("entries")
                .flatten()
                .any(|entry| entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".retained.key.quarantine-")),
            "identity-checked Unix cleanup must leave quarantine residue"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_orphan_reparse_cleanup_is_explicitly_unsupported() {
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
        const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
        const FILE_ATTRIBUTE_SYSTEM: u32 = 0x0000_0004;

        for attributes in [
            FILE_ATTRIBUTE_REPARSE_POINT,
            FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_DIRECTORY,
            FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_SYSTEM,
        ] {
            assert!(
                is_windows_reparse_attributes(attributes),
                "all reparse attributes must fail closed: {attributes:#x}"
            );
        }
        assert!(!is_windows_reparse_attributes(FILE_ATTRIBUTE_DIRECTORY));
    }

    #[cfg(any(target_os = "macos", target_os = "ios"))]
    #[test]
    fn macos_key_store_rejects_before_secret_materialization() {
        let root = tempfile::tempdir().expect("root");
        assert!(matches!(
            KeyMaterialStore::new(root.path().join("keys")),
            Err(CredentialError::UnsupportedRuntime)
        ));
        assert_eq!(
            fs::read_dir(root.path()).expect("root entries").count(),
            0,
            "unsupported platform must not create a secret store"
        );
    }

    #[test]
    fn key_store_rejects_oversized_root_before_filesystem_access() {
        let root = tempfile::tempdir().expect("root");
        let oversized = root.path().join("x".repeat(MAX_PINNED_PATH_BYTES));

        assert!(matches!(
            KeyMaterialStore::new(&oversized),
            Err(CredentialError::InvalidPath)
        ));
        assert!(!oversized.exists(), "oversized root must not be created");
    }

    #[test]
    fn generated_child_path_is_bounded_before_join_allocation() {
        let root = PathBuf::from("r".repeat(MAX_PINNED_PATH_BYTES));
        assert!(matches!(
            join_store_child(&root, "generated.key"),
            Err(CredentialError::InvalidPath)
        ));
    }

    #[test]
    fn generated_child_bound_counts_native_separator_and_non_ascii_units() {
        let child = OsString::from("中");
        let child_length = native_os_str_length(&child);
        let separator_length = if cfg!(windows) {
            std::mem::size_of::<u16>()
        } else {
            1
        };
        let unit_length = if cfg!(windows) {
            std::mem::size_of::<u16>()
        } else {
            1
        };
        let root_length = MAX_PINNED_PATH_BYTES
            .saturating_sub(separator_length)
            .saturating_sub(child_length);
        let root = if cfg!(windows) {
            PathBuf::from("r".repeat(root_length / std::mem::size_of::<u16>()))
        } else {
            PathBuf::from("r".repeat(root_length))
        };
        assert!(join_store_child_os(&root, &child, CredentialError::InvalidPath).is_ok());

        let over_limit_length = root_length.saturating_add(unit_length);
        let over_limit_root = if cfg!(windows) {
            PathBuf::from("r".repeat(over_limit_length / std::mem::size_of::<u16>()))
        } else {
            PathBuf::from("r".repeat(over_limit_length))
        };
        assert!(matches!(
            join_store_child_os(&over_limit_root, &child, CredentialError::InvalidPath),
            Err(CredentialError::InvalidPath)
        ));
    }

    #[cfg(windows)]
    #[test]
    fn generated_child_bound_rejects_utf16_separator_overflow_before_join() {
        let root_length = MAX_PINNED_PATH_BYTES - std::mem::size_of::<u16>();
        let root = PathBuf::from("r".repeat(root_length / std::mem::size_of::<u16>()));

        assert!(matches!(
            ensure_child_name_bound(&root, 1, CredentialError::InvalidPath),
            Err(CredentialError::InvalidPath)
        ));
    }

    #[cfg(windows)]
    #[test]
    fn windows_system_authority_path_is_bounded_before_pathbuf_join() {
        let root = OsString::from(
            "r".repeat((MAX_PINNED_PATH_BYTES / std::mem::size_of::<u16>()).saturating_sub(1)),
        );

        assert!(matches!(
            bounded_system_path(&root, &["System32", "icacls.exe"]),
            Err(CredentialError::InvalidPath)
        ));
    }

    #[cfg(any(windows, target_os = "linux", target_os = "android"))]
    #[test]
    fn materialize_expiry_after_key_publication_rolls_back_visible_key_path() {
        let root = tempfile::tempdir().expect("root");
        let store = KeyMaterialStore::new(root.path()).expect("store");
        let reference = CredentialRef::parse("credential:post-publication-expiry").expect("ref");
        let identity = KeyIdentity::issue("connection", &reference).expect("identity");
        store.inject_materialize_failure(MaterializeFailurePoint::PostPublicationExpiry);

        let result = store.materialize_until(
            &identity,
            &CredentialSecret::private_key(
                "-----BEGIN OPENSSH PRIVATE KEY-----\nkey\n-----END OPENSSH PRIVATE KEY-----",
            ),
            Instant::now() + std::time::Duration::from_secs(1),
        );

        assert!(matches!(result, Err(CredentialError::DeadlineExpired)));
        let key_path =
            root.path()
                .join(format!("ssh-{}{}", identity.digest_hex(), KEY_FILE_SUFFIX));
        assert!(
            !key_path.exists(),
            "post-publication expiry must roll back key path"
        );
        assert!(!store
            .inner
            .records
            .lock()
            .expect("records")
            .contains_key(&identity));
    }

    #[cfg(unix)]
    #[test]
    fn quarantine_name_is_bounded_before_owned_component_allocation() {
        let path = PathBuf::from(format!("store/{}", "x".repeat(MAX_PINNED_PATH_BYTES)));
        assert!(matches!(
            quarantine_path(&path),
            Err(CredentialError::CleanupUncertain)
        ));
    }

    #[test]
    fn pinned_fingerprint_bound_does_not_allow_synchronous_hundred_megabyte_hashes() {
        assert!(
            MAX_PINNED_FILE_BYTES <= 16 * 1024 * 1024,
            "pinning must not synchronously hash a 256 MiB authority"
        );
    }

    #[test]
    fn pinned_authority_rejects_expired_deadline_before_hashing() {
        let root = tempfile::tempdir().expect("root");
        let path = root.path().join("known_hosts");
        fs::write(&path, b"known-host fixture\n").expect("fixture");
        assert!(matches!(
            PinnedFile::open_until(&path, Instant::now() - std::time::Duration::from_secs(1)),
            Err(CredentialError::DeadlineExpired)
        ));
    }

    #[test]
    fn deadline_bound_reopen_fails_before_open() {
        let root = tempfile::tempdir().expect("root");
        let path = root.path().join("reopen");
        assert!(matches!(
            open_no_follow_until(
                &path,
                Some(Instant::now() - std::time::Duration::from_secs(1))
            ),
            Err(CredentialError::DeadlineExpired)
        ));
        assert!(!path.exists());
    }

    #[test]
    fn dropping_retained_key_retains_record_when_unlink_is_uncertain() {
        let root = tempfile::tempdir().expect("root");
        let store = KeyMaterialStore::new(root.path()).expect("store");
        let reference = CredentialRef::parse("credential:test-key").expect("reference");
        let identity = KeyIdentity::issue("connection", &reference).expect("identity");
        let secret = CredentialSecret::private_key(
            "-----BEGIN OPENSSH PRIVATE KEY-----\nkey\n-----END OPENSSH PRIVATE KEY-----",
        );
        let retained = store.materialize(&identity, &secret).expect("materialize");
        let path = retained.path().to_path_buf();
        assert!(path.is_file());
        assert_eq!(
            fs::read(&path).expect("materialized key"),
            b"-----BEGIN OPENSSH PRIVATE KEY-----\nkey\n-----END OPENSSH PRIVATE KEY-----\n"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        drop(retained);
        assert!(!path.exists());
        #[cfg(unix)]
        {
            assert!(store
                .inner
                .records
                .lock()
                .expect("records")
                .contains_key(&identity));
            assert!(root
                .path()
                .join(format!("ssh-{}{}", identity.digest_hex(), MANIFEST_SUFFIX))
                .exists());
            assert!(store.inner.uncertain_cleanups.load(Ordering::Acquire) > 0);
        }
        #[cfg(windows)]
        {
            assert!(!store
                .inner
                .records
                .lock()
                .expect("records")
                .contains_key(&identity));
            assert!(!root
                .path()
                .join(format!("ssh-{}{}", identity.digest_hex(), MANIFEST_SUFFIX))
                .exists());
            assert_eq!(store.inner.uncertain_cleanups.load(Ordering::Acquire), 0);
        }
    }

    #[test]
    fn dropping_retained_key_never_deletes_replaced_path_content() {
        let root = tempfile::tempdir().expect("root");
        let store = KeyMaterialStore::new(root.path()).expect("store");
        let reference = CredentialRef::parse("credential:replacement-key").expect("reference");
        let identity = KeyIdentity::issue("connection", &reference).expect("identity");
        let secret = CredentialSecret::private_key(
            "-----BEGIN OPENSSH PRIVATE KEY-----\nkey\n-----END OPENSSH PRIVATE KEY-----",
        );
        let mut retained = store.materialize(&identity, &secret).expect("materialize");
        let path = retained.path().to_path_buf();
        drop(retained.handle.take());
        fs::write(&path, b"replacement content owned by another writer").expect("replace");
        drop(retained);
        assert_eq!(
            fs::read(&path).expect("replacement remains"),
            b"replacement content owned by another writer"
        );
    }

    #[test]
    fn exact_open_handle_cleanup_refuses_a_replaced_manifest_path() {
        let root = tempfile::tempdir().expect("root");
        let path = root.path().join("manifest.json");
        fs::write(&path, b"original manifest").expect("original");
        let handle = open_no_follow(&path).expect("open original");
        let identity = file_identity(&handle).expect("identity");
        let fingerprint = fingerprint_file(&handle).expect("fingerprint");

        fs::write(&path, b"replacement manifest owned by another writer").expect("replacement");
        assert!(matches!(
            remove_opened_snapshot(&path, Some(&handle), identity, fingerprint),
            Err(CredentialError::CleanupUncertain)
        ));
        assert_eq!(
            fs::read(&path).expect("replacement remains"),
            b"replacement manifest owned by another writer"
        );
    }

    #[test]
    fn publication_never_replaces_an_existing_destination() {
        let root = tempfile::tempdir().expect("root");
        let temp = root.path().join("manifest.tmp");
        let destination = root.path().join("manifest.json");
        fs::write(&temp, b"new manifest").expect("temp");
        fs::write(&destination, b"existing manifest").expect("destination");

        assert!(matches!(
            publish_noreplace(&temp, &destination),
            Err(CredentialError::AlreadyRetained)
        ));
        assert_eq!(
            fs::read(&destination).expect("destination remains"),
            b"existing manifest"
        );
        assert_eq!(fs::read(&temp).expect("temp remains"), b"new manifest");
    }

    #[test]
    fn early_materialize_failures_surface_quarantine_residue() {
        for failure in [
            MaterializeFailurePoint::Acl,
            MaterializeFailurePoint::Write,
            MaterializeFailurePoint::Flush,
            MaterializeFailurePoint::Sync,
        ] {
            let root = tempfile::tempdir().expect("root");
            let store = KeyMaterialStore::new(root.path()).expect("store");
            let reference = CredentialRef::parse("credential:early-failure").expect("reference");
            let identity = KeyIdentity::issue("connection", &reference).expect("identity");
            store.inject_materialize_failure(failure);
            let result = store.materialize(
                &identity,
                &CredentialSecret::private_key(
                    "-----BEGIN OPENSSH PRIVATE KEY-----\nkey\n-----END OPENSSH PRIVATE KEY-----",
                ),
            );
            assert!(matches!(result, Err(CredentialError::Io)), "{failure:?}");
            #[cfg(unix)]
            {
                assert!(
                    fs::read_dir(root.path())
                        .expect("entries")
                        .flatten()
                        .any(|entry| entry.file_name().to_string_lossy().contains("quarantine-")),
                    "early {failure:?} failure must retain uncertain cleanup residue"
                );
                assert_eq!(
                    store.inner.uncertain_cleanups.load(Ordering::Acquire),
                    1,
                    "early {failure:?} failure must surface uncertainty"
                );
            }
            #[cfg(windows)]
            {
                assert!(
                    !fs::read_dir(root.path())
                        .expect("entries")
                        .flatten()
                        .any(|entry| entry.file_name().to_string_lossy().contains("quarantine-")),
                    "Windows handle cleanup must remove early {failure:?} residue"
                );
                assert_eq!(
                    store.inner.uncertain_cleanups.load(Ordering::Acquire),
                    0,
                    "Windows early {failure:?} cleanup is exact"
                );
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_holds_root_operation_lock_through_identity_delete() {
        let root = tempfile::tempdir().expect("root");
        let store = KeyMaterialStore::new(root.path()).expect("store");
        let replacement_store = KeyMaterialStore::new(root.path()).expect("replacement store");
        let reference = CredentialRef::parse("credential:barrier-key").expect("reference");
        let identity = KeyIdentity::issue("connection", &reference).expect("identity");
        let mut retained = store
            .materialize(
                &identity,
                &CredentialSecret::private_key(
                    "-----BEGIN OPENSSH PRIVATE KEY-----\nkey\n-----END OPENSSH PRIVATE KEY-----",
                ),
            )
            .expect("materialize");
        let path = retained.path().to_path_buf();
        drop(retained.handle.take());

        let barrier = std::sync::Arc::new((std::sync::Barrier::new(2), std::sync::Barrier::new(2)));
        *TEST_CLEANUP_BARRIER.lock().unwrap() = Some(std::sync::Arc::clone(&barrier));
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (finished_tx, finished_rx) = std::sync::mpsc::channel();
        let replacement_lock = std::sync::Arc::clone(&replacement_store.inner.operation_lock);
        let replacement_path = path.clone();
        let replacement = std::thread::spawn(move || {
            barrier.0.wait();
            started_tx.send(()).expect("replacement started");
            let _guard = replacement_lock.lock().unwrap();
            fs::write(&replacement_path, b"replacement after cleanup").expect("replacement");
            finished_tx.send(()).expect("replacement finished");
        });
        let cleanup_store = store.clone();
        let cleanup_identity = identity.clone();
        let cleanup = std::thread::spawn(move || cleanup_store.cleanup(&cleanup_identity));
        started_rx.recv().expect("replacement attempted");
        assert!(finished_rx.try_recv().is_err());
        barrier.1.wait();
        assert!(matches!(
            cleanup.join().expect("cleanup join"),
            Err(CredentialError::CleanupUncertain)
        ));
        finished_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("replacement after cleanup");
        replacement.join().expect("replacement join");
        *TEST_CLEANUP_BARRIER.lock().unwrap() = None;
        assert_eq!(
            fs::read(&path).expect("replacement remains"),
            b"replacement after cleanup"
        );
        std::mem::forget(retained);
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_refuses_independent_post_verification_swap() {
        let root = tempfile::tempdir().expect("root");
        let store = KeyMaterialStore::new(root.path()).expect("store");
        let reference = CredentialRef::parse("credential:postcheck-key").expect("reference");
        let identity = KeyIdentity::issue("connection", &reference).expect("identity");
        let mut retained = store
            .materialize(
                &identity,
                &CredentialSecret::private_key(
                    "-----BEGIN OPENSSH PRIVATE KEY-----\nkey\n-----END OPENSSH PRIVATE KEY-----",
                ),
            )
            .expect("materialize");
        let path = retained.path().to_path_buf();
        drop(retained.handle.take());

        let barrier = Arc::new((std::sync::Barrier::new(2), std::sync::Barrier::new(2)));
        *TEST_POSTCHECK_SWAP_BARRIER.lock().unwrap() = Some(Arc::clone(&barrier));
        let swap_path = path.clone();
        let swap = std::thread::spawn(move || {
            barrier.0.wait();
            let _ = fs::remove_file(&swap_path);
            fs::write(&swap_path, b"independent replacement").expect("swap replacement");
            barrier.1.wait();
        });

        let result = store.cleanup(&identity);
        swap.join().expect("swap join");
        *TEST_POSTCHECK_SWAP_BARRIER.lock().unwrap() = None;

        assert!(matches!(result, Err(CredentialError::CleanupUncertain)));
        assert_eq!(
            fs::read(&path).expect("replacement remains"),
            b"independent replacement"
        );
        std::mem::forget(retained);
    }

    #[test]
    fn recovery_capacity_counts_valid_records_not_raw_key_and_manifest_entries() {
        let root = tempfile::tempdir().expect("root");
        let store = KeyMaterialStore::new(root.path()).expect("store");
        for index in 0..129u32 {
            let reference =
                CredentialRef::parse(format!("credential:capacity-{index}")).expect("reference");
            let identity =
                KeyIdentity::issue(&format!("connection-{index}"), &reference).expect("identity");
            let key_path =
                root.path()
                    .join(format!("ssh-{}{}", identity.digest_hex(), KEY_FILE_SUFFIX));
            let key_bytes = format!("fixture-key-{index}").into_bytes();
            fs::write(&key_path, &key_bytes).expect("key");
            let key_handle = open_no_follow(&key_path).expect("key handle");
            let key_file_identity = file_identity(&key_handle).expect("key identity");
            let key_fingerprint = fingerprint_file(&key_handle).expect("key fingerprint");
            let manifest = Manifest {
                version: MANIFEST_VERSION,
                digest: identity.digest,
                index: identity.index,
                file_identity_first: key_file_identity.first,
                file_identity_second: key_file_identity.second,
                fingerprint: key_fingerprint,
            };
            let manifest_path =
                root.path()
                    .join(format!("ssh-{}{}", identity.digest_hex(), MANIFEST_SUFFIX));
            fs::write(
                &manifest_path,
                serde_json::to_vec(&manifest).expect("manifest bytes"),
            )
            .expect("manifest");
        }
        store.inner.records.lock().unwrap().clear();
        let report = store.recover().expect("recover 129 records");
        assert_eq!(report.retained().len(), 129);
        drop(report);
    }

    #[test]
    fn restart_recovery_uses_bounded_manifest_identity() {
        let root = tempfile::tempdir().expect("root");
        let store = KeyMaterialStore::new(root.path()).expect("store");
        let reference = CredentialRef::parse("credential:test-key").expect("reference");
        let identity = KeyIdentity::issue("connection", &reference).expect("identity");
        let secret = CredentialSecret::private_key(
            "-----BEGIN OPENSSH PRIVATE KEY-----\nkey\n-----END OPENSSH PRIVATE KEY-----",
        );
        let mut retained = store.materialize(&identity, &secret).expect("materialize");
        let path = retained.path().to_path_buf();
        // Simulate a crash/restart: release the old handle and lose the
        // in-memory record without touching the durable key/manifest.
        let identity_copy = retained.identity().clone();
        drop(retained.handle.take());
        store.inner.records.lock().unwrap().clear();
        std::mem::forget(retained);
        let restarted = KeyMaterialStore::new(root.path()).expect("restart");
        let report = restarted.recover().expect("recover");
        assert_eq!(report.retained().len(), 1);
        assert_eq!(report.retained()[0].identity(), &identity_copy);
        let recovered = report.retained.into_iter().next().expect("retained");
        drop(recovered);
        assert!(!path.exists());
    }

    #[test]
    fn restart_recovery_rejects_key_content_fingerprint_drift() {
        let root = tempfile::tempdir().expect("root");
        let store = KeyMaterialStore::new(root.path()).expect("store");
        let reference = CredentialRef::parse("credential:fingerprint-key").expect("reference");
        let identity = KeyIdentity::issue("connection", &reference).expect("identity");
        let secret = CredentialSecret::private_key(
            "-----BEGIN OPENSSH PRIVATE KEY-----\nkey\n-----END OPENSSH PRIVATE KEY-----",
        );
        let mut retained = store.materialize(&identity, &secret).expect("materialize");
        let path = retained.path().to_path_buf();
        drop(retained.handle.take());
        store.inner.records.lock().unwrap().clear();
        std::mem::forget(retained);
        fs::write(&path, b"replacement after crash").expect("drift");

        let restarted = KeyMaterialStore::new(root.path()).expect("restart");
        let report = restarted.recover().expect("recover");
        assert!(report.retained().is_empty());
        assert_eq!(report.removed_orphans(), 0);
        assert_eq!(report.uncertain_cleanup_count(), 1);
        assert_eq!(
            fs::read(&path).expect("drift remains visible"),
            b"replacement after crash"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_key_and_caller_forged_identity_are_rejected() {
        let root = tempfile::tempdir().expect("root");
        let outside = tempfile::tempdir().expect("outside");
        let store = KeyMaterialStore::new(root.path()).expect("store");
        let reference = CredentialRef::parse("credential:test-key").expect("reference");
        let identity = KeyIdentity::issue("connection", &reference).expect("identity");
        let other = KeyIdentity::issue(
            "different-connection",
            &CredentialRef::parse("credential:other-key").expect("other reference"),
        )
        .expect("other identity");
        let forged_path =
            root.path()
                .join(format!("ssh-{}{}", identity.digest_hex(), KEY_FILE_SUFFIX));
        std::os::unix::fs::symlink(outside.path(), &forged_path).expect("symlink");
        let secret = CredentialSecret::private_key(
            "-----BEGIN OPENSSH PRIVATE KEY-----\nkey\n-----END OPENSSH PRIVATE KEY-----",
        );
        assert!(matches!(
            store.materialize(&identity, &secret),
            Err(CredentialError::InvalidPath)
        ));
        assert!(matches!(
            store.cleanup(&other),
            Err(CredentialError::NotRetained)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn recovery_does_not_follow_forged_manifest_symlink() {
        let root = tempfile::tempdir().expect("root");
        let outside = tempfile::tempdir().expect("outside");
        let store = KeyMaterialStore::new(root.path()).expect("store");
        let reference = CredentialRef::parse("credential:test-key").expect("reference");
        let identity = KeyIdentity::issue("connection", &reference).expect("identity");
        let manifest_name = format!("ssh-{}{}", identity.digest_hex(), MANIFEST_SUFFIX);
        let outside_manifest = outside.path().join("outside.json");
        let manifest = Manifest {
            version: MANIFEST_VERSION,
            digest: identity.digest,
            index: identity.index,
            file_identity_first: 0,
            file_identity_second: 0,
            fingerprint: [0; 32],
        };
        fs::write(&outside_manifest, serde_json::to_vec(&manifest).unwrap()).expect("manifest");
        std::os::unix::fs::symlink(&outside_manifest, root.path().join(manifest_name))
            .expect("manifest symlink");

        assert!(matches!(
            store.recover(),
            Err(CredentialError::CleanupUncertain)
        ));
        assert!(outside_manifest.is_file());
    }

    #[test]
    fn recovery_retains_manifest_without_key_and_reports_uncertainty() {
        let root = tempfile::tempdir().expect("root");
        let store = KeyMaterialStore::new(root.path()).expect("store");
        let reference = CredentialRef::parse("credential:missing-key").expect("reference");
        let identity = KeyIdentity::issue("connection", &reference).expect("identity");
        let manifest_path =
            root.path()
                .join(format!("ssh-{}{}", identity.digest_hex(), MANIFEST_SUFFIX));
        let manifest = Manifest {
            version: MANIFEST_VERSION,
            digest: identity.digest,
            index: identity.index,
            file_identity_first: 0,
            file_identity_second: 0,
            fingerprint: [0; 32],
        };
        fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).expect("manifest");

        let report = store.recover().expect("recover");
        assert_eq!(report.removed_orphans(), 0);
        assert_eq!(report.uncertain_cleanup_count(), 1);
        assert!(manifest_path.exists());
    }

    #[test]
    fn recovery_surfaces_cleanup_failure_for_manifest_without_key() {
        let root = tempfile::tempdir().expect("root");
        let store = KeyMaterialStore::new(root.path()).expect("store");
        let reference = CredentialRef::parse("credential:cleanup-failure").expect("reference");
        let identity = KeyIdentity::issue("connection", &reference).expect("identity");
        let manifest_path =
            root.path()
                .join(format!("ssh-{}{}", identity.digest_hex(), MANIFEST_SUFFIX));
        let manifest = Manifest {
            version: MANIFEST_VERSION,
            digest: identity.digest,
            index: identity.index,
            file_identity_first: 0,
            file_identity_second: 0,
            fingerprint: [0; 32],
        };
        fs::create_dir(&manifest_path).expect("directory manifest");
        fs::write(
            manifest_path.join("payload"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .expect("payload");

        assert!(matches!(
            store.recover(),
            Err(CredentialError::CleanupUncertain)
        ));
        assert!(manifest_path.exists(), "failed cleanup remains visible");
    }

    #[cfg(windows)]
    #[test]
    fn windows_acl_is_restricted_for_directory_and_key() {
        let root = tempfile::tempdir().expect("root");
        let store = KeyMaterialStore::new(root.path()).expect("store");
        let reference = CredentialRef::parse("credential:test-key").expect("reference");
        let identity = KeyIdentity::issue("connection", &reference).expect("identity");
        let secret = CredentialSecret::private_key(
            "-----BEGIN OPENSSH PRIVATE KEY-----\nkey\n-----END OPENSSH PRIVATE KEY-----",
        );
        let retained = store.materialize(&identity, &secret).expect("materialize");
        let authority = store
            .inner
            .icacls
            .as_ref()
            .expect("retained icacls authority");
        let output = std::process::Command::new(authority.path())
            .arg(retained.path())
            .output()
            .expect("icacls");
        assert!(output.status.success());
        drop(retained);
    }
}
